mod math;

use crate::pdf::SearchRect;
use crate::process::Operation;
use crate::source_snapshot::{SourceSnapshot, valid_absolute_path, valid_warning};
use serde::{Deserialize, Serialize};
use std::rc::Rc;
use std::{
    fs,
    io::{self, BufRead, BufReader, Read},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command,
};
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

/// Source coordinates: one-based line, zero-based UTF-8 byte offset;
/// column is one-based UTF-16 (VS Code), column_char is one-based Unicode scalar.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceLocation {
    pub file: String,
    pub line: u32,
    pub byte_column: usize,
    pub column: usize,
    pub column_char: usize,
    pub precise: bool,
}

/// Identity of a local PDF revision, including atomic replacement and in-place writes.
/// Not a content digest: the protocol assumes a non-adversarial local filesystem.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PdfRevision {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl PdfRevision {
    pub fn read(path: &Path) -> io::Result<Self> {
        let metadata = fs::metadata(path)?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        })
    }

    pub fn check(self, path: &Path) -> io::Result<()> {
        if Self::read(path)? != self {
            return Err(io::Error::other(
                "PDF revision changed; repeat forward search",
            ));
        }
        Ok(())
    }
}

/// Metadata identity of both files observed when PDFium opens a document.
/// Stability does not prove a common build; producers must publish a completed
/// PDF/SyncTeX pair before requesting navigation.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DocumentRevision {
    pub pdf: PdfRevision,
    companion: Option<(bool, PdfRevision)>,
}
impl DocumentRevision {
    pub fn read(path: &Path) -> io::Result<Self> {
        let pdf = PdfRevision::read(path)?;
        let mut companion = None;
        for (extension, compressed) in [("synctex.gz", true), ("synctex", false)] {
            match PdfRevision::read(&path.with_extension(extension)) {
                Ok(revision) => {
                    companion = Some((compressed, revision));
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(Self { pdf, companion })
    }
    pub fn check(self, path: &Path) -> io::Result<()> {
        if Self::read(path)? != self {
            return Err(io::Error::other(
                "displayed PDF/SyncTeX revision changed; wait for reload and click again",
            ));
        }
        Ok(())
    }
}

pub struct InverseResolution {
    pub location: SourceLocation,
    pub warning: Option<String>,
}

/// Literal source word and nearby prose tokens; not TeX macro expansion.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ForwardWord {
    pub words: Vec<String>,
    pub selected: usize,
}

impl ForwardWord {
    fn at(text: &str, column: u32) -> Option<Self> {
        let text = source_line_text(text);
        let byte = text.char_indices().nth(column.checked_sub(1)? as usize)?.0;
        let hidden = nonprinting_source_ranges(text);
        let words: Vec<_> = words(text)
            .into_iter()
            .filter(|(start, _)| {
                (*start == 0 || !text[..*start].ends_with('\\'))
                    && !hidden.iter().any(|range| range.contains(start))
            })
            .collect();
        Self::from_tokens(&words, byte)
    }

    fn from_tokens(words: &[(usize, &str)], byte: usize) -> Option<Self> {
        let selected = words
            .iter()
            .position(|(start, word)| *start <= byte && byte < start + word.len())?;
        let start = selected.saturating_sub(3);
        let end = (selected + 4).min(words.len());
        if words[start..end].iter().any(|(_, word)| word.len() > 128) {
            return None;
        }
        Some(Self {
            words: words[start..end]
                .iter()
                .map(|(_, word)| (*word).to_owned())
                .collect(),
            selected: selected - start,
        })
    }

    fn valid(&self) -> bool {
        self.words.len() <= 7
            && self.selected < self.words.len()
            && self.words.iter().all(|word| {
                word.len() <= 128
                    && word.chars().next().is_some_and(char::is_alphanumeric)
                    && word
                        .chars()
                        .all(|ch| ch.is_alphanumeric() || is_combining_mark(ch))
            })
    }
}

/// Compact raw or qualified forward geometry in original compiler coordinates.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ForwardCandidate {
    pub page: u32,
    pub rect: SearchRect,
}

// Editor adapters can supply a literal saved line and UTF-8 cursor offset.
// Tokenize here so every adapter shares PDF matching's Unicode word rules.
fn deserialize_forward_word<'de, D>(deserializer: D) -> Result<Option<ForwardWord>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Literal {
        text: String,
        byte_column: usize,
    }
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Input {
        Context(ForwardWord),
        Literal(Literal),
    }
    match Option::<Input>::deserialize(deserializer)? {
        None => Ok(None),
        Some(Input::Context(word)) => Ok(Some(word)),
        Some(Input::Literal(Literal { text, byte_column })) => {
            if !text.is_char_boundary(byte_column) {
                return Err(serde::de::Error::custom("invalid source word byte column"));
            }
            Ok(ForwardWord::from_tokens(&words(&text), byte_column))
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ForwardRequest {
    pub pdf: std::path::PathBuf,
    pub revision: PdfRevision,
    pub page: u32,
    pub h: f32,
    pub v: f32,
    pub width: f32,
    pub height: f32,
    #[serde(default, deserialize_with = "deserialize_forward_word")]
    pub word: Option<ForwardWord>,
    /// Revision-bound compiler source-map service supplied by an editor adapter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inverse_search: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_snapshot: Option<PathBuf>,
    /// Explicit opt-in to warned inverse anchors; a refinement warning alone is not opt-in.
    #[serde(default)]
    pub best_guess: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

impl ForwardRequest {
    pub fn rect(&self) -> SearchRect {
        SearchRect {
            left: self.h,
            top: self.v - self.height,
            right: self.h + self.width,
            bottom: self.v,
        }
    }

    pub fn validate(&self) -> io::Result<()> {
        if self
            .source_snapshot
            .as_ref()
            .is_some_and(|path| !valid_absolute_path(path))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "source snapshot must be an absolute UTF-8 path",
            ));
        }
        if self
            .warning
            .as_ref()
            .is_some_and(|warning| !valid_warning(warning))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid forward navigation warning",
            ));
        }
        if self
            .inverse_search
            .as_ref()
            .is_some_and(|path| !Path::new(path).is_absolute() || path.as_bytes().contains(&0))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "inverse source-map endpoint must be an absolute Unix socket path",
            ));
        }
        if self.word.as_ref().is_some_and(|word| !word.valid()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid forward word context",
            ));
        }
        if !valid_absolute_path(&self.pdf)
            || !forward_geometry_valid(self.page, self.h, self.v, self.width, self.height)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "forward request requires an absolute PDF path, positive page, and finite nonnegative box dimensions",
            ));
        }
        Ok(())
    }
}

pub fn parse_forward_request(payload: &str) -> io::Result<ForwardRequest> {
    let request: ForwardRequest = serde_json::from_str(payload)?;
    request.validate()?;
    Ok(request)
}

/// Explicit editor context; standalone navigation keeps the strict default.
#[derive(Clone, Copy, Debug, Default)]
pub struct ForwardSource<'a> {
    pub snapshot: Option<&'a Path>,
    pub best_guess: bool,
    pub anchor_only: bool,
}

/// No usable compiler geometry exists for this otherwise valid request.
/// Explicit editor best-guess commands distinguish this from operational errors.
#[derive(Debug)]
pub struct ForwardUnavailable(&'static str);

impl std::fmt::Display for ForwardUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for ForwardUnavailable {}

pub fn is_forward_unavailable(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|error| error.is::<ForwardUnavailable>())
}

pub fn resolve_forward(
    pdf: &Path,
    file: &Path,
    line: u32,
    column: u32,
) -> io::Result<ForwardRequest> {
    resolve_forward_with_library(pdf, file, line, column, None, ForwardSource::default())
}

pub fn resolve_forward_with_library(
    pdf: &Path,
    file: &Path,
    line: u32,
    column: u32,
    library: Option<&Path>,
    context: ForwardSource<'_>,
) -> io::Result<ForwardRequest> {
    if line == 0 || column == 0 {
        return Err(io::Error::other("source line and column must be positive"));
    }
    let pdf = fs::canonicalize(pdf)?;
    let snapshot = context
        .snapshot
        .map(|path| SourceSnapshot::load(path, &pdf))
        .transpose()?;
    let allow_anchor = context.best_guess || context.anchor_only;
    let mut document_revision = match &snapshot {
        Some(snapshot) => Some(snapshot.revision),
        None if allow_anchor => {
            drop(crate::source_snapshot::open_regular(&pdf, false)?);
            Some(DocumentRevision::read(&pdf)?)
        }
        None => None,
    };
    if snapshot.is_none()
        && allow_anchor
        && document_revision.is_some_and(|revision| revision.companion.is_none())
    {
        return Err(io::Error::other(ForwardUnavailable(
            "no SyncTeX sidecar is available for source positioning",
        )));
    }
    let file = match &snapshot {
        Some(snapshot) => snapshot.normalize(file)?,
        None if context.anchor_only => match fs::canonicalize(file) {
            Ok(file) => file,
            Err(_) => crate::source_snapshot::normalize_absolute(file, &std::env::current_dir()?)?,
        },
        None => fs::canonicalize(file)?,
    };
    let revision = match document_revision {
        Some(revision) => revision.pdf,
        None => PdfRevision::read(&pdf)?,
    };
    // Root identity is compiler metadata, never a basename or a live-source
    // guess. A published root cache remains required even for LINE fallback.
    let (root_source, cached_root) = match &snapshot {
        Some(snapshot) => {
            let root = snapshot.read(&snapshot.main).map_err(|error| {
                if crate::source_snapshot::required_cache_error(&error) {
                    error
                } else {
                    io::Error::new(
                        error.kind(),
                        crate::source_snapshot::SnapshotReadError(error),
                    )
                }
            })?;
            let root_source = snapshot.main == file;
            (root_source, root_source.then_some(root))
        }
        None => {
            let inputs = crate::source_snapshot::SourceInfo::read(&pdf)?;
            let main = inputs
                .main
                .as_ref()
                .ok_or_else(|| io::Error::other("compiler root source identity is unavailable"))?;
            if inputs.revision.pdf != revision
                || document_revision.is_some_and(|revision| revision != inputs.revision)
            {
                return Err(io::Error::other(
                    "document changed while reading compiler root identity",
                ));
            }
            document_revision = Some(inputs.revision);
            (main == &file, None)
        }
    };
    let mut warning = snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.warning(&file))
        .map(str::to_owned);
    if snapshot.is_none() && context.anchor_only {
        warning = Some(
            "best-guess navigation: compiled source history unavailable; using the compiler line anchor"
                .into(),
        );
    }
    let source = match &snapshot {
        Some(_) if root_source => cached_root,
        Some(snapshot) => match snapshot.read(&file) {
            Ok(source) => Some(source),
            Err(error) if allow_anchor && !crate::source_snapshot::required_cache_error(&error) => {
                warning = Some(format!(
                    "best-guess navigation: compiled source unavailable: {error}"
                ));
                None
            }
            Err(error) => return Err(error),
        },
        None if context.anchor_only => None,
        None => Some(read_source(&file)?),
    };
    if snapshot.is_some()
        && allow_anchor
        && document_revision.is_some_and(|revision| revision.companion.is_none())
    {
        return Err(io::Error::other(ForwardUnavailable(
            "no SyncTeX sidecar is available for source positioning",
        )));
    }
    if warning.is_some() && !allow_anchor {
        return Err(io::Error::other(
            "compiled source snapshot provenance is unverified",
        ));
    }
    let original_line = match source
        .as_deref()
        .map(|source| validate_forward_source_position(source, line, column))
    {
        Some(Ok(text)) => Some(text),
        Some(Err(_))
            if context.anchor_only && source.as_deref() == Some("") && line == 1 && column == 1 =>
        {
            return Err(io::Error::other(ForwardUnavailable(
                "compiled source is empty; no printed source position is available",
            )));
        }
        Some(Err(error)) if warning.is_some() && allow_anchor => {
            warning = Some(format!(
                "best-guess navigation: compiled source position cannot be qualified: {error}"
            ));
            None
        }
        Some(Err(error)) => return Err(error),
        None => None,
    };
    let page_hint = if context.anchor_only || warning.is_some() {
        None
    } else {
        original_line.and_then(|text| ForwardWord::at(text, column))
    };
    // Collected frame/caption bodies use their closing compiler line. Keep the
    // original source position for the page hint, and give Beamer priority.
    let view_line = source
        .as_deref()
        .and_then(|source| {
            frame_closing_line(source, line)
                .or_else(|| original_line.and_then(|_| caption_closing_line(source, line, column)))
        })
        .unwrap_or(line);
    let candidates = forward_candidates(&pdf, &file, snapshot.as_ref(), view_line)?;
    let line_candidate = |hint: Option<&ForwardWord>| {
        let anchor = best_guess_forward_candidate(&candidates)?;
        match hint {
            Some(hint) => {
                crate::pdf::select_forward_line_candidate(&pdf, &candidates, hint, anchor, library)
                    .map_err(io::Error::other)
            }
            None => Ok(anchor),
        }
    };
    let selected = if context.anchor_only || warning.is_some() {
        warning.get_or_insert_with(|| "line-only navigation: using the SyncTeX line anchor".into());
        best_guess_forward_candidate(&candidates)?
    } else {
        // Saved bytes and input identities do not trace TeX's hooks, imported
        // code, or token execution. Text similarity cannot prove source ownership.
        // Keep the legacy overlay page policy without publishing a WORD hint.
        let selected = line_candidate(page_hint.as_ref())?;
        warning = Some(
            "line-only navigation: compiled TeX execution cannot be verified; using the SyncTeX line anchor"
                .into(),
        );
        selected
    };
    let result = ForwardRequest {
        pdf,
        revision,
        page: selected.page,
        h: selected.rect.left,
        v: selected.rect.bottom,
        width: selected.rect.right - selected.rect.left,
        height: selected.rect.bottom - selected.rect.top,
        word: None,
        inverse_search: None,
        source_snapshot: snapshot.as_ref().map(|snapshot| snapshot.path.clone()),
        best_guess: context.best_guess,
        warning,
    };
    result.validate()?;
    if let Some(revision) = document_revision {
        revision.check(&result.pdf)?;
    } else {
        revision.check(&result.pdf)?;
    }
    Ok(result)
}

fn best_guess_forward_candidate(candidates: &[ForwardCandidate]) -> io::Result<ForwardCandidate> {
    // SyncTeX orders results by its nearest source anchor. Preserve that order,
    // skipping only empty geometry; never claim a particular printed word.
    candidates
        .iter()
        .find(|candidate| {
            candidate.rect.right > candidate.rect.left || candidate.rect.bottom > candidate.rect.top
        })
        .copied()
        .ok_or_else(|| {
            io::Error::other(ForwardUnavailable(
                "synctex view returned no usable compiler anchor geometry",
            ))
        })
}

#[derive(Clone)]
struct ForwardNode {
    kind: u8,
    page: u32,
    parent: Option<usize>,
    target: Option<usize>,
    children: Vec<usize>,
    tag: u32,
    line: i64,
    h: i64,
    v: i64,
    width: i64,
    height: i64,
    depth: i64,
    h_v: i64,
    v_v: i64,
    width_v: i64,
    height_v: i64,
    depth_v: i64,
    mean_line: i64,
    weight: i64,
    friend_weight: usize,
}

impl ForwardNode {
    fn new(
        kind: u8,
        page: u32,
        parent: Option<usize>,
        tag: u32,
        line: i64,
        coords: [i64; 5],
    ) -> Self {
        let [h, v, width, height, depth] = coords;
        Self {
            kind,
            page,
            parent,
            target: None,
            children: Vec::new(),
            tag,
            line,
            h,
            v,
            width,
            height,
            depth,
            h_v: h,
            v_v: v,
            width_v: width,
            height_v: height,
            depth_v: depth,
            mean_line: line,
            weight: 1,
            friend_weight: 0,
        }
    }

    fn contain_point(&mut self, ph: i64, pv: i64) {
        if self.kind != b'(' {
            return;
        }
        if self.width_v < 0 {
            let max_h = self.h_v;
            let min_h = max_h.saturating_add(self.width_v);
            if ph < min_h {
                self.width_v = ph.saturating_sub(max_h);
            } else if ph > max_h {
                self.h_v = ph;
                self.width_v = min_h.saturating_sub(ph);
            }
        } else {
            let min_h = self.h_v;
            let max_h = min_h.saturating_add(self.width_v);
            if ph < min_h {
                self.h_v = ph;
                self.width_v = max_h.saturating_sub(ph);
            } else if ph > max_h {
                self.width_v = ph.saturating_sub(min_h);
            }
        }
        let min_v = self.v_v.saturating_sub(self.height_v);
        let max_v = self.v_v.saturating_add(self.depth_v);
        if pv < min_v {
            self.height_v = self.v_v.saturating_sub(pv);
        } else if pv > max_v {
            self.depth_v = pv.saturating_sub(self.v_v);
        }
    }

    fn contain_box(&mut self, bounds: [i64; 4]) {
        if self.kind != b'(' {
            return;
        }
        let [min_h, max_h, min_v, max_v] = bounds;
        if self.width_v < 0 {
            let mx = self.h_v;
            let mn = mx.saturating_add(self.width_v);
            if min_h < mn {
                self.width_v = min_h.saturating_sub(mx);
            } else if max_h > mx {
                self.h_v = max_h;
                self.width_v = mn.saturating_sub(max_h);
            }
        } else {
            let mn = self.h_v;
            let mx = mn.saturating_add(self.width_v);
            if min_h < mn {
                self.h_v = min_h;
                self.width_v = mx.saturating_sub(min_h);
            } else if max_h > mx {
                self.width_v = max_h.saturating_sub(mn);
            }
        }
        let mn = self.v_v.saturating_sub(self.height_v);
        let mx = self.v_v.saturating_add(self.depth_v);
        if min_v < mn {
            self.height_v = self.v_v.saturating_sub(min_v);
        } else if max_v > mx {
            self.depth_v = max_v.saturating_sub(self.v_v);
        }
    }

    fn data_box(&self) -> [i64; 4] {
        let (min_h, max_h) = if self.width < 0 {
            (self.h.saturating_add(self.width), self.h)
        } else {
            (self.h, self.h.saturating_add(self.width))
        };
        [
            min_h,
            max_h,
            self.v.saturating_sub(self.height),
            self.v.saturating_add(self.depth),
        ]
    }

    fn data_xob(&self) -> [i64; 4] {
        let (min_h, max_h) = if self.width > 0 {
            (self.h.saturating_sub(self.width), self.h)
        } else {
            (self.h, self.h.saturating_sub(self.width))
        };
        [
            min_h,
            max_h,
            self.v.saturating_sub(self.height),
            self.v.saturating_add(self.depth),
        ]
    }

    fn data_box_v(&self) -> [i64; 4] {
        let (min_h, max_h) = if self.width_v < 0 {
            (self.h_v.saturating_add(self.width_v), self.h_v)
        } else {
            (self.h_v, self.h_v.saturating_add(self.width_v))
        };
        [
            min_h,
            max_h,
            self.v_v.saturating_sub(self.height_v),
            self.v_v.saturating_add(self.depth_v),
        ]
    }
}

fn parse_synctex_int_prefix(text: &str) -> Option<(i64, &str)> {
    let bytes = text.as_bytes();
    let mut index = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    Some((text.get(..index)?.parse().ok()?, text.get(index..)?))
}

const MAX_PROXY_DEPTH: usize = 16;
const MAX_FORWARD_NODES: usize = 500_000;

#[allow(clippy::too_many_arguments)]
fn clone_synctex_proxy_subtree(
    nodes: &mut Vec<ForwardNode>,
    source: usize,
    parent: Option<usize>,
    page: u32,
    dh: i64,
    dv: i64,
    depth: usize,
    register_friends: &mut Option<&mut [Vec<usize>; 1024]>,
) -> Option<usize> {
    if depth > MAX_PROXY_DEPTH || nodes.len() >= MAX_FORWARD_NODES {
        return None;
    }
    let mut clone = nodes[source].clone();
    let children = std::mem::take(&mut clone.children);
    clone.page = page;
    clone.parent = parent;
    clone.target = nodes[source].target.or(Some(source));
    clone.h = clone.h.saturating_add(dh);
    clone.v = clone.v.saturating_add(dv);
    clone.h_v = clone.h_v.saturating_add(dh);
    clone.v_v = clone.v_v.saturating_add(dv);
    let index = nodes.len();
    nodes.push(clone);
    if let Some(friends) = register_friends.as_deref_mut()
        && matches!(
            nodes[index].kind,
            b'[' | b'(' | b'v' | b'h' | b'b' | b'k' | b'g' | b'r' | b'$' | b'x'
        )
    {
        let bucket = ((i64::from(nodes[index].tag).saturating_add(nodes[index].line)).unsigned_abs()
            as usize)
            % 1024;
        friends[bucket].insert(0, index);
    }
    for child in children {
        if let Some(child_idx) = clone_synctex_proxy_subtree(
            nodes,
            child,
            Some(index),
            page,
            dh,
            dv,
            depth + 1,
            register_friends,
        ) {
            nodes[index].children.push(child_idx);
        }
    }
    Some(index)
}

fn forward_candidates(
    pdf: &Path,
    file: &Path,
    snapshot: Option<&SourceSnapshot>,
    view_line: u32,
) -> io::Result<Vec<ForwardCandidate>> {
    forward_candidates_from_reader(open_synctex(pdf)?, pdf, file, snapshot, view_line)
}

fn forward_candidates_from_reader(
    reader: impl BufRead,
    pdf: &Path,
    file: &Path,
    snapshot: Option<&SourceSnapshot>,
    view_line: u32,
) -> io::Result<Vec<ForwardCandidate>> {
    forward_candidates_from_reader_with_options(reader, pdf, file, snapshot, view_line, true)
}

fn forward_candidates_from_reader_with_options(
    mut reader: impl BufRead,
    pdf: &Path,
    file: &Path,
    snapshot: Option<&SourceSnapshot>,
    view_line: u32,
    fix_readahead: bool,
) -> io::Result<Vec<ForwardCandidate>> {
    let base = snapshot
        .map(|snapshot| snapshot.cwd.as_path())
        .unwrap_or_else(|| pdf.parent().unwrap_or(Path::new(".")));
    let mut inputs: Vec<(u32, String)> = Vec::new();
    let mut max_lines = std::collections::HashMap::<u32, i64>::new();
    let mut pre_magnification = 1000i64;
    let mut pre_unit = 8192i64;
    let mut pre_x_offset = 578i64;
    let mut pre_y_offset = 578i64;
    let mut post_magnification = None::<f64>;
    let mut post_x_offset = None::<f64>;
    let mut post_y_offset = None::<f64>;
    let mut in_post_scriptum = false;

    let mut nodes = Vec::<ForwardNode>::new();
    let mut friends: [Vec<usize>; 1024] = std::array::from_fn(|_| Vec::new());
    let mut sheets = std::collections::BTreeMap::<u32, usize>::new();
    let mut forms = std::collections::HashMap::<u32, usize>::new();
    let mut ref_in_form = Vec::<(usize, usize, u32, i64, i64)>::new();
    let mut ref_in_sheet = Vec::<(usize, usize, u32, u32, i64, i64)>::new();

    let mut current_page = None::<u32>;
    let mut form_stack = Vec::<usize>::new();
    let mut ignored_form_depth = 0usize;
    let mut parent = None::<usize>;
    let mut x_stack = Vec::<Vec<usize>>::new();
    let mut last_k = None::<usize>;
    let mut last_g = None::<usize>;
    let mut last_v = -1i64;

    let make_friend = |friends: &mut [Vec<usize>; 1024], nodes: &[ForwardNode], idx: usize| {
        let bucket =
            ((i64::from(nodes[idx].tag).saturating_add(nodes[idx].line)).unsigned_abs() as usize)
                % 1024;
        friends[bucket].insert(0, idx);
    };
    let flush_x_set = |x_stack: &mut [Vec<usize>],
                       friends: &mut [Vec<usize>; 1024],
                       nodes: &mut [ForwardNode],
                       in_form: bool,
                       source_idx: usize| {
        if let Some(pending) = x_stack.last_mut()
            && !pending.is_empty()
        {
            let (tag, line) = (nodes[source_idx].tag, nodes[source_idx].line);
            for x_idx in pending.drain(..).rev() {
                nodes[x_idx].tag = tag;
                nodes[x_idx].line = line;
                if !in_form {
                    make_friend(friends, nodes, x_idx);
                }
            }
        }
    };
    let flush_x_make = |x_stack: &mut [Vec<usize>],
                        friends: &mut [Vec<usize>; 1024],
                        nodes: &[ForwardNode],
                        in_form: bool| {
        if let Some(pending) = x_stack.last_mut()
            && !pending.is_empty()
        {
            for x_idx in pending.drain(..).rev() {
                if !in_form {
                    make_friend(friends, nodes, x_idx);
                }
            }
        }
    };
    fn parse_link_pos<'a>(
        body: &'a str,
        last_v: &mut i64,
    ) -> Option<(u32, i64, i64, i64, &'a str)> {
        let (link, rest) = body.split_once(':')?;
        let (tag_str, line_rest) = link.split_once(',')?;
        let tag = tag_str.parse::<u32>().ok()?;
        let line = line_rest.split(',').next()?.parse::<i64>().ok()?;
        let (pos_str, rem) = rest.split_once(':').unwrap_or((rest, ""));
        let (x_str, y_str) = pos_str.split_once(',')?;
        let x = x_str.parse::<i64>().ok()?;
        let y = if y_str.starts_with('=') {
            *last_v
        } else {
            let (parsed_y, _) = parse_synctex_int_prefix(y_str)?;
            *last_v = parsed_y;
            parsed_y
        };
        Some((tag, line, x, y, rem))
    }
    let parse_whd = |rem: &str| -> Option<(i64, i64, i64)> {
        let mut parts = rem.splitn(3, ',');
        let w = parts.next()?.parse::<i64>().ok()?;
        let h = parts.next()?.parse::<i64>().ok()?;
        let (d, _) = parse_synctex_int_prefix(parts.next()?)?;
        Some((w, h, d))
    };
    let parse_post_dim = |val: &str| -> Option<f64> {
        let s = val.trim_start();
        let unit_pos = s.find(|c: char| c.is_ascii_alphabetic())?;
        let num = s[..unit_pos].trim().parse::<f64>().ok()?;
        let scale = match &s[unit_pos..unit_pos.saturating_add(2).min(s.len())] {
            "in" => 72.27 * 65536.0,
            "cm" => 72.27 * 65536.0 / 2.54,
            "mm" => 72.27 * 65536.0 / 25.4,
            "pt" => 65536.0,
            "bp" => 72.27 / 72.0 * 65536.0,
            "pc" => 12.0 * 65536.0,
            "sp" => 1.0,
            "dd" => 1238.0 / 1157.0 * 65536.0,
            "cc" => 14856.0 / 1157.0 * 65536.0,
            "nd" => 685.0 / 642.0 * 65536.0,
            "nc" => 1370.0 / 107.0 * 65536.0,
            _ => return None,
        };
        Some(num * scale)
    };

    let mut row = String::new();
    while reader.read_line(&mut row)? > 0 {
        let trimmed = row.trim_end_matches(['\r', '\n']);
        if ignored_form_depth > 0 {
            if trimmed.starts_with('<') {
                ignored_form_depth += 1;
            } else if trimmed.starts_with('>') {
                ignored_form_depth -= 1;
            }
            row.clear();
            continue;
        }
        if let Some(input) = trimmed.strip_prefix("Input:")
            && let Some((tag_str, path)) = input.split_once(':')
            && let Ok(tag) = tag_str.parse::<u32>()
        {
            inputs.push((tag, path.trim_end_matches(' ').to_string()));
        } else if let Some(val) = trimmed.strip_prefix("Magnification:") {
            if in_post_scriptum {
                post_magnification = val.trim().parse::<f64>().ok();
            } else if let Ok(mag) = val.trim().parse::<i64>() {
                pre_magnification = mag;
            }
        } else if let Some(val) = trimmed.strip_prefix("Unit:") {
            if let Ok(unit) = val.trim().parse::<i64>() {
                pre_unit = unit;
            }
        } else if let Some(val) = trimmed.strip_prefix("X Offset:") {
            if in_post_scriptum {
                post_x_offset = parse_post_dim(val);
            } else if let Ok(off) = val.trim().parse::<i64>() {
                pre_x_offset = off;
            }
        } else if let Some(val) = trimmed.strip_prefix("Y Offset:") {
            if in_post_scriptum {
                post_y_offset = parse_post_dim(val);
            } else if let Ok(off) = val.trim().parse::<i64>() {
                pre_y_offset = off;
            }
        } else if trimmed.starts_with("Post scriptum:") {
            in_post_scriptum = true;
        } else if trimmed.starts_with("Content:") {
            last_v = -1;
        } else if let Some(page_str) = trimmed.strip_prefix('{')
            && form_stack.is_empty()
            && current_page.is_none()
            && let Ok((page, _)) = parse_synctex_int_prefix(page_str).ok_or(())
            && let Ok(page) = u32::try_from(page)
        {
            current_page = Some(page);
            let root = nodes.len();
            nodes.push(ForwardNode::new(b'{', page, None, 0, 0, [0; 5]));
            sheets.insert(page, root);
            parent = Some(root);
            x_stack.clear();
            last_k = None;
            last_g = None;
        } else if trimmed.starts_with('}') && form_stack.is_empty() && current_page.is_some() {
            current_page = None;
            parent = None;
            x_stack.clear();
            last_k = None;
            last_g = None;
        } else if let Some(tag_str) = trimmed.strip_prefix('<') {
            if let Some((tag, _)) = parse_synctex_int_prefix(tag_str)
                && let Ok(tag) = u32::try_from(tag)
                && !forms.contains_key(&tag)
            {
                let root = nodes.len();
                nodes.push(ForwardNode::new(b'<', 0, parent, tag, 0, [0; 5]));
                forms.insert(tag, root);
                form_stack.push(root);
                parent = Some(root);
                last_k = None;
                last_g = None;
            } else {
                ignored_form_depth = 1;
            }
        } else if trimmed.starts_with('>') && !form_stack.is_empty() {
            let form_root = form_stack.pop().unwrap();
            parent = nodes[form_root]
                .parent
                .or_else(|| current_page.and_then(|page| sheets.get(&page).copied()));
            last_k = None;
            last_g = None;
        } else if (current_page.is_some() || !form_stack.is_empty())
            && let Some(&kind) = trimmed.as_bytes().first()
        {
            let in_form = !form_stack.is_empty();
            let page = if in_form {
                0
            } else {
                current_page.unwrap_or(0)
            };
            match kind {
                b'[' | b'(' => {
                    if let Some((tag, line, x, y, rem)) = parse_link_pos(&trimmed[1..], &mut last_v)
                        && let Some((w, h, d)) = parse_whd(rem)
                    {
                        let idx = nodes.len();
                        nodes.push(ForwardNode::new(
                            kind,
                            page,
                            parent,
                            tag,
                            line,
                            [x, y, w, h, d],
                        ));
                        if let Some(p_idx) = parent {
                            nodes[p_idx].children.push(idx);
                        }
                        parent = Some(idx);
                        x_stack.push(Vec::new());
                        if kind == b'(' {
                            let bdry = nodes.len();
                            nodes.push(ForwardNode::new(
                                b'b',
                                page,
                                Some(idx),
                                tag,
                                line,
                                [x, y, 0, 0, 0],
                            ));
                            nodes[idx].children.push(bdry);
                            if !in_form {
                                make_friend(&mut friends, &nodes, bdry);
                            }
                        }
                        max_lines
                            .entry(tag)
                            .and_modify(|max| *max = (*max).max(line))
                            .or_insert(line);
                        last_k = None;
                        last_g = None;
                    }
                }
                b']' => {
                    if let Some(p_idx) = parent
                        && nodes[p_idx].kind == b'['
                    {
                        if nodes[p_idx].children.is_empty() && !in_form {
                            make_friend(&mut friends, &nodes, p_idx);
                        }
                        let next_parent = nodes[p_idx].parent;
                        flush_x_make(&mut x_stack, &mut friends, &nodes, in_form);
                        x_stack.pop();
                        flush_x_set(&mut x_stack, &mut friends, &mut nodes, in_form, p_idx);
                        parent = next_parent;
                        last_k = None;
                        last_g = None;
                    }
                }
                b')' => {
                    if let Some(p_idx) = parent
                        && nodes[p_idx].kind == b'('
                    {
                        let ch_len = nodes[p_idx].children.len();
                        if ch_len > 1 {
                            let first_child = nodes[p_idx].children[0];
                            let second_line = nodes[nodes[p_idx].children[1]].line;
                            nodes[first_child].line = second_line;
                            let mut nw = 0i64;
                            let mut cum = 0i64;
                            for i in 1..ch_len {
                                let c_idx = nodes[p_idx].children[i];
                                if nodes[c_idx].kind == b'(' {
                                    let cw = nodes[c_idx].weight.max(1);
                                    nw = nw.saturating_add(cw);
                                    cum =
                                        cum.saturating_add(nodes[c_idx].mean_line.saturating_mul(cw));
                                } else {
                                    nw = nw.saturating_add(1);
                                    cum = cum.saturating_add(nodes[c_idx].line);
                                }
                            }
                            nodes[p_idx].mean_line = cum.saturating_add(nw / 2) / nw.max(1);
                            nodes[p_idx].weight = nw;
                        } else {
                            nodes[p_idx].mean_line = nodes[p_idx].line;
                            nodes[p_idx].weight = 1;
                        }
                        let last_c = nodes[p_idx]
                            .children
                            .iter()
                            .rev()
                            .copied()
                            .find(|&c| nodes[c].kind != b'f')
                            .unwrap_or(nodes[p_idx].children[0]);
                        let (last_tag, last_line) = (nodes[last_c].tag, nodes[last_c].line);
                        let (end_h, end_v) = (
                            nodes[p_idx].h_v.saturating_add(nodes[p_idx].width_v),
                            nodes[p_idx].v_v,
                        );
                        let bdry2 = nodes.len();
                        nodes.push(ForwardNode::new(
                            b'b',
                            page,
                            Some(p_idx),
                            last_tag,
                            last_line,
                            [end_h, end_v, 0, 0, 0],
                        ));
                        nodes[p_idx].children.push(bdry2);
                        let first_c = nodes[p_idx].children[0];
                        let (start_h, start_v) = (nodes[p_idx].h_v, nodes[p_idx].v_v);
                        nodes[first_c].h = start_h;
                        nodes[first_c].v = start_v;
                        if let (Some(k_idx), Some(g_idx)) = (last_k, last_g) {
                            for i in 0..nodes[p_idx].children.len().saturating_sub(1) {
                                if nodes[p_idx].children[i + 1] == k_idx {
                                    let prev_idx = nodes[p_idx].children[i];
                                    let (ptag, pline) = (nodes[prev_idx].tag, nodes[prev_idx].line);
                                    nodes[k_idx].tag = ptag;
                                    nodes[k_idx].line = pline;
                                    nodes[g_idx].tag = ptag;
                                    nodes[g_idx].line = pline;
                                    break;
                                }
                            }
                        }
                        let next_parent = nodes[p_idx].parent;
                        let box_v = nodes[p_idx].data_box_v();
                        flush_x_make(&mut x_stack, &mut friends, &nodes, in_form);
                        x_stack.pop();
                        flush_x_set(&mut x_stack, &mut friends, &mut nodes, in_form, p_idx);
                        if let Some(np_idx) = next_parent {
                            nodes[np_idx].contain_box(box_v);
                        }
                        parent = next_parent;
                        last_k = None;
                        last_g = None;
                    }
                }
                b'v' | b'h' | b'r' => {
                    if let Some((tag, line, x, y, rem)) = parse_link_pos(&trimmed[1..], &mut last_v)
                        && let Some((w, h, d)) = parse_whd(rem)
                    {
                        let idx = nodes.len();
                        nodes.push(ForwardNode::new(
                            kind,
                            page,
                            parent,
                            tag,
                            line,
                            [x, y, w, h, d],
                        ));
                        if let Some(p_idx) = parent {
                            nodes[p_idx].children.push(idx);
                            if kind == b'h' {
                                let bounds = nodes[idx].data_box();
                                nodes[p_idx].contain_box(bounds);
                            }
                        }
                        if kind == b'r' && !in_form {
                            make_friend(&mut friends, &nodes, idx);
                        }
                        flush_x_set(&mut x_stack, &mut friends, &mut nodes, in_form, idx);
                        max_lines
                            .entry(tag)
                            .and_modify(|max| *max = (*max).max(line))
                            .or_insert(line);
                        last_k = None;
                        last_g = None;
                    }
                }
                b'k' => {
                    if let Some((tag, line, x, y, rem)) = parse_link_pos(&trimmed[1..], &mut last_v)
                        && let Some((w, _)) = parse_synctex_int_prefix(rem)
                    {
                        let idx = nodes.len();
                        nodes.push(ForwardNode::new(
                            b'k',
                            page,
                            parent,
                            tag,
                            line,
                            [x, y, w, 0, 0],
                        ));
                        if let Some(p_idx) = parent {
                            nodes[p_idx].children.push(idx);
                            let bounds = nodes[idx].data_xob();
                            nodes[p_idx].contain_box(bounds);
                        }
                        if !in_form {
                            make_friend(&mut friends, &nodes, idx);
                        }
                        flush_x_set(&mut x_stack, &mut friends, &mut nodes, in_form, idx);
                        max_lines
                            .entry(tag)
                            .and_modify(|max| *max = (*max).max(line))
                            .or_insert(line);
                        last_k = Some(idx);
                        last_g = None;
                    }
                }
                b'g' | b'$' => {
                    if let Some((tag, line, x, y, _)) = parse_link_pos(&trimmed[1..], &mut last_v) {
                        let idx = nodes.len();
                        nodes.push(ForwardNode::new(
                            kind,
                            page,
                            parent,
                            tag,
                            line,
                            [x, y, 0, 0, 0],
                        ));
                        if let Some(p_idx) = parent {
                            nodes[p_idx].children.push(idx);
                            nodes[p_idx].contain_point(x, y);
                        }
                        if !in_form {
                            make_friend(&mut friends, &nodes, idx);
                        }
                        flush_x_set(&mut x_stack, &mut friends, &mut nodes, in_form, idx);
                        max_lines
                            .entry(tag)
                            .and_modify(|max| *max = (*max).max(line))
                            .or_insert(line);
                        if kind == b'g' && last_k.is_some() {
                            last_g = Some(idx);
                        } else {
                            last_k = None;
                            last_g = None;
                        }
                    }
                }
                b'x' => {
                    if let Some((tag, line, x, y, _)) = parse_link_pos(&trimmed[1..], &mut last_v) {
                        let idx = nodes.len();
                        nodes.push(ForwardNode::new(
                            b'x',
                            page,
                            parent,
                            tag,
                            line,
                            [x, y, 0, 0, 0],
                        ));
                        let prev_is_bdry = parent.is_some_and(|p_idx| {
                            nodes[p_idx].children.len() == 1
                                && nodes[nodes[p_idx].children[0]].kind == b'b'
                        });
                        if let Some(p_idx) = parent {
                            nodes[p_idx].children.push(idx);
                            nodes[p_idx].contain_point(x, y);
                        }
                        if prev_is_bdry || x_stack.last().is_some_and(|s| !s.is_empty()) {
                            if let Some(pending) = x_stack.last_mut() {
                                pending.push(idx);
                            }
                        } else if !in_form {
                            make_friend(&mut friends, &nodes, idx);
                        }
                        max_lines
                            .entry(tag)
                            .and_modify(|max| *max = (*max).max(line))
                            .or_insert(line);
                        last_k = None;
                        last_g = None;
                    }
                }
                b'f' => {
                    if let Some((tag_str, rest)) = trimmed[1..].split_once(':')
                        && let Ok(ftag) = tag_str.parse::<u32>()
                        && let Some((x_str, y_str)) = rest.split_once(',')
                        && let Ok(fx) = x_str.parse::<i64>()
                    {
                        let fy = if y_str.starts_with('=') {
                            last_v
                        } else if let Some((py, _)) = parse_synctex_int_prefix(y_str) {
                            last_v = py;
                            py
                        } else {
                            row.clear();
                            continue;
                        };
                        if let Some(p_idx) = parent {
                            let ref_idx = nodes.len();
                            nodes.push(ForwardNode::new(
                                b'f',
                                page,
                                Some(p_idx),
                                0,
                                0,
                                [fx, fy, 0, 0, 0],
                            ));
                            nodes[p_idx].children.push(ref_idx);
                            if in_form {
                                ref_in_form.push((ref_idx, p_idx, ftag, fx, fy));
                            } else {
                                ref_in_sheet.push((ref_idx, p_idx, page, ftag, fx, fy));
                            }
                        }
                        last_k = None;
                        last_g = None;
                    }
                }
                _ => {
                    last_k = None;
                    last_g = None;
                }
            }
        }
        row.clear();
    }

    for &(ref_idx, p_idx, ftag, fx, fy) in &ref_in_form {
        let replacement = forms
            .get(&ftag)
            .and_then(|&form_root| nodes[form_root].children.first().copied())
            .and_then(|target| {
                let (dh, dv) = (fx, fy.saturating_sub(nodes[target].height));
                let mut none_friends = None;
                clone_synctex_proxy_subtree(
                    &mut nodes,
                    target,
                    Some(p_idx),
                    0,
                    dh,
                    dv,
                    0,
                    &mut none_friends,
                )
            });
        if let Some(pos) = nodes[p_idx].children.iter().position(|&c| c == ref_idx) {
            if let Some(proxy) = replacement {
                nodes[p_idx].children[pos] = proxy;
            } else {
                nodes[p_idx].children.remove(pos);
            }
        }
    }
    for &(ref_idx, p_idx, page, ftag, fx, fy) in &ref_in_sheet {
        let replacement = forms
            .get(&ftag)
            .and_then(|&form_root| nodes[form_root].children.first().copied())
            .and_then(|target| {
                let (dh, dv) = (fx, fy.saturating_sub(nodes[target].height));
                let mut reg = Some(&mut friends);
                clone_synctex_proxy_subtree(
                    &mut nodes,
                    target,
                    Some(p_idx),
                    page,
                    dh,
                    dv,
                    0,
                    &mut reg,
                )
            });
        if let Some(pos) = nodes[p_idx].children.iter().position(|&c| c == ref_idx) {
            if let Some(proxy) = replacement {
                nodes[p_idx].children[pos] = proxy;
            } else {
                nodes[p_idx].children.remove(pos);
            }
        }
    }

    let mut matching_tags = std::collections::HashSet::new();
    let mut input_tag = None::<u32>;
    for (tag, raw_path) in inputs.iter().rev() {
        let normalized = match snapshot {
            Some(snapshot) => snapshot.normalize(Path::new(raw_path)).ok(),
            None => crate::source_snapshot::normalize_absolute(Path::new(raw_path), base)
                .ok()
                .map(|path| fs::canonicalize(&path).unwrap_or(path)),
        };
        if normalized.as_deref() == Some(file) {
            matching_tags.insert(*tag);
            if input_tag.is_none() {
                input_tag = Some(*tag);
            }
        }
    }
    if input_tag.is_none()
        && snapshot.is_none()
        && let Some(name) = file.file_name()
    {
        let mut matched_path = None::<&str>;
        for (tag, raw_path) in inputs.iter().rev() {
            if Path::new(raw_path).file_name() == Some(name) {
                match matched_path {
                    None => {
                        matched_path = Some(raw_path.as_str());
                        input_tag = Some(*tag);
                        matching_tags.insert(*tag);
                    }
                    Some(prev) if prev == raw_path.as_str() => {
                        matching_tags.insert(*tag);
                    }
                    Some(_) => {
                        input_tag = None;
                        matching_tags.clear();
                        break;
                    }
                }
            }
        }
    }
    let Some(input_tag) = input_tag else {
        return Err(io::Error::other(ForwardUnavailable(
            "synctex view returned no complete match",
        )));
    };

    if pre_unit <= 0 {
        pre_unit = 8192;
    }
    if pre_magnification <= 0 {
        pre_magnification = 1000;
    }
    let base_unit = (pre_unit as f64) / 65781.76;
    let mag_factor = (pre_magnification as f64) / 1000.0;
    let unit = post_magnification
        .filter(|&m| m > 0.0)
        .map_or(base_unit, |m| m * base_unit)
        * mag_factor;
    let x_off = post_x_offset.map_or((pre_x_offset as f64) * base_unit, |x| x / 65781.76);
    let y_off = post_y_offset.map_or((pre_y_offset as f64) * base_unit, |y| y / 65781.76);
    let bound = (1_500_000.0 / mag_factor) as i64;

    let node_mean_line = |nodes: &[ForwardNode], idx: usize| -> i64 {
        let n = nodes[idx].target.unwrap_or(idx);
        if nodes[n].kind == b'(' && nodes[n].target.is_none() {
            nodes[n].mean_line
        } else if let Some(p_idx) = nodes[n].parent
            && nodes[p_idx].kind == b'('
            && nodes[p_idx].target.is_none()
        {
            nodes[p_idx].mean_line
        } else {
            nodes[idx].line
        }
    };

    let box_visible = |nodes: &[ForwardNode], mut idx: usize| -> usize {
        if !matches!(nodes[idx].kind, b'[' | b'(' | b'v' | b'h')
            && let Some(p_idx) = nodes[idx].parent
        {
            idx = p_idx;
        }
        let mean = node_mean_line(nodes, idx);
        let mut cursor = idx;
        while let Some(p_idx) = nodes[cursor].parent {
            cursor = p_idx;
            if matches!(nodes[cursor].kind, b'(' | b'h') {
                if mean.saturating_sub(node_mean_line(nodes, cursor)).abs() > 1 {
                    return idx;
                } else if nodes[cursor].width > bound
                    || nodes[cursor].height.saturating_add(nodes[cursor].depth) > bound
                {
                    return cursor;
                }
                idx = cursor;
            }
        }
        idx
    };

    let display_query = |nodes: &mut [ForwardNode],
                         tag: u32,
                         query_line: u32|
     -> Vec<ForwardCandidate> {
        let max_l = max_lines.get(&tag).copied().unwrap_or(0);
        let mut line = i64::from(query_line).min(max_l);
        let mut line_offset = 1i64;
        for _ in 0..100 {
            if line <= max_l {
                let bucket_idx =
                    ((i64::from(tag).saturating_add(line)).unsigned_abs() as usize) % 1024;
                for exclude_box in [true, false] {
                    let mut pages: Vec<(u32, Vec<usize>)> = Vec::new();
                    let mut first_page = None::<u32>;
                    for &target in &friends[bucket_idx] {
                        if (exclude_box && matches!(nodes[target].kind, b'[' | b'(' | b'v' | b'h'))
                            || nodes[target].tag != tag
                            || nodes[target].line != line
                        {
                            continue;
                        }
                        let t_page = nodes[target].page;
                        if pages.is_empty() {
                            pages.push((t_page, vec![target]));
                            first_page = Some(t_page);
                        } else if pages.len() == 1 && Some(t_page) == first_page {
                            pages[0].1.insert(0, target);
                        } else if let Some(entry) = pages.iter_mut().find(|(p, _)| *p == t_page) {
                            let insert_idx = 1.min(entry.1.len());
                            entry.1.insert(insert_idx, target);
                        } else if pages.len() == 1 {
                            pages.push((t_page, vec![target]));
                        } else {
                            pages.insert(0, (t_page, vec![target]));
                        }
                    }
                    if !pages.is_empty() {
                        let mut sorted_pages = Vec::with_capacity(pages.len());
                        for (p_num, page_nodes) in pages {
                            for &n_idx in &page_nodes {
                                if let Some(p_idx) = nodes[n_idx].parent {
                                    nodes[p_idx].friend_weight = 0;
                                }
                            }
                            let mut weighted = Vec::with_capacity(page_nodes.len());
                            for n_idx in page_nodes {
                                if let Some(p_idx) = nodes[n_idx].parent {
                                    let has_weight_slot =
                                        nodes[p_idx].kind == b'(' && nodes[p_idx].target.is_none();
                                    let cached_w = if has_weight_slot {
                                        nodes[p_idx].friend_weight
                                    } else {
                                        0
                                    };
                                    if cached_w == 0 {
                                        let w = nodes[p_idx]
                                            .children
                                            .iter()
                                            .filter(|&&c| {
                                                nodes[c].tag == tag && nodes[c].line == line
                                            })
                                            .count();
                                        if has_weight_slot {
                                            nodes[p_idx].friend_weight = w;
                                        }
                                        weighted.push((w, n_idx));
                                    } else {
                                        weighted.push((0, n_idx));
                                    }
                                } else {
                                    weighted.push((0, n_idx));
                                }
                            }
                            let mut res_list = vec![weighted[0]];
                            for item in weighted.into_iter().skip(1) {
                                if item.0 > res_list[0].0 {
                                    res_list.insert(0, item);
                                } else if item.0 == 0 {
                                    continue;
                                } else {
                                    let mut idx = 0;
                                    while idx + 1 < res_list.len() {
                                        idx += 1;
                                        if item.0 < res_list[idx].0 {
                                            continue;
                                        }
                                        break;
                                    }
                                    res_list.insert(idx + 1, item);
                                }
                            }
                            if let Some(zero_pos) =
                                res_list.iter().skip(1).position(|(w, _)| *w == 0)
                            {
                                res_list.truncate(zero_pos + 1);
                            }
                            sorted_pages.push((
                                p_num,
                                res_list.into_iter().map(|(_, n)| n).collect::<Vec<_>>(),
                            ));
                        }
                        let mut ordered = vec![sorted_pages[0].clone()];
                        let mut best_match = ordered[0].0;
                        for sib in sorted_pages.into_iter().skip(1) {
                            if sib.0 < best_match {
                                best_match = sib.0;
                                ordered.insert(0, sib);
                            } else {
                                ordered.insert(1, sib);
                            }
                        }
                        let mut out = Vec::new();
                        for (p_num, page_nodes) in ordered {
                            if p_num == 0 {
                                continue;
                            }
                            for n_idx in page_nodes {
                                let bv = box_visible(nodes, n_idx);
                                let h = ((nodes[bv].h_v as f64) * unit + x_off) as f32;
                                let v = ((nodes[bv].v_v.saturating_add(nodes[bv].depth_v) as f64)
                                    * unit
                                    + y_off) as f32;
                                let width = ((nodes[bv].width_v as f64) * unit) as f32;
                                let height = ((nodes[bv]
                                    .height_v
                                    .saturating_add(nodes[bv].depth_v)
                                    as f64)
                                    * unit) as f32;
                                if !forward_geometry_valid(p_num, h, v, width, height) {
                                    continue;
                                }
                                let candidate = ForwardCandidate {
                                    page: p_num,
                                    rect: SearchRect {
                                        left: h,
                                        top: v - height,
                                        right: h + width,
                                        bottom: v,
                                    },
                                };
                                if !out.iter().any(|old: &ForwardCandidate| {
                                    old.page == candidate.page && old.rect == candidate.rect
                                }) {
                                    out.push(candidate);
                                }
                            }
                        }
                        return out;
                    }
                }
                line += line_offset;
                line_offset = if line_offset < 0 {
                    -(line_offset - 1)
                } else {
                    -(line_offset + 1)
                };
                if line <= 0 {
                    line += line_offset;
                    line_offset = if line_offset < 0 {
                        -(line_offset - 1)
                    } else {
                        -(line_offset + 1)
                    };
                }
            } else {
                break;
            }
        }
        Vec::new()
    };

    let mut candidates = display_query(&mut nodes, input_tag, view_line);
    if fix_readahead
        && let Ok(first) = best_guess_forward_candidate(&candidates)
        && let Some((next_page, target_line)) = resolve_readahead_sheet_in_memory(
            &nodes,
            &sheets,
            &matching_tags,
            view_line,
            first.page,
        )
    {
        let mut overflow: Vec<_> = candidates
            .iter()
            .copied()
            .filter(|candidate| candidate.page >= next_page)
            .collect();
        if best_guess_forward_candidate(&overflow).is_err() {
            overflow = display_query(&mut nodes, input_tag, target_line)
                .into_iter()
                .filter(|candidate| candidate.page >= next_page)
                .collect();
        }
        if best_guess_forward_candidate(&overflow).is_ok() {
            candidates = overflow;
        }
    }
    if candidates.is_empty() {
        return Err(io::Error::other(ForwardUnavailable(
            "synctex view returned no complete match",
        )));
    }
    Ok(candidates)
}

/// TeX stamps each sheet's outer vboxes, running header/footer, and rule nodes
/// with the input scanner position `(tag, oline)` at `\shipout`. When a page
/// break splits a paragraph or display before `oline`, SyncTeX can rank the
/// preceding sheet's header ahead of the overflow sheet where `view_line` is
/// actually rendered.
fn resolve_readahead_sheet_in_memory(
    nodes: &[ForwardNode],
    sheets: &std::collections::BTreeMap<u32, usize>,
    matching_tags: &std::collections::HashSet<u32>,
    view_line: u32,
    first_page: u32,
) -> Option<(u32, u32)> {
    let mut target_tag = None::<u32>;
    let mut readahead = None::<(u32, u32)>;
    let mut resolved = None::<(u32, u32)>;
    for (&page, &sheet_root) in sheets.range(first_page..) {
        let Some(&outer_idx) = nodes[sheet_root].children.first() else {
            continue;
        };
        if !matches!(nodes[outer_idx].kind, b'[' | b'(') {
            return resolved;
        }
        let otag = nodes[outer_idx].tag;
        let Ok(oline) = u32::try_from(nodes[outer_idx].line) else {
            return resolved;
        };
        if page == first_page {
            if !matching_tags.contains(&otag) {
                return None;
            }
            target_tag = Some(otag);
        }
        let mut s_max = 0u32;
        let mut min_next = None::<u32>;
        let mut hbox_means = Vec::<u32>::new();
        let mut stack = vec![(outer_idx, false)];
        let mut hbox_stats = Vec::<(u32, u64, u32)>::new();
        while let Some((idx, exiting)) = stack.pop() {
            let node = &nodes[idx];
            if exiting {
                if node.kind == b'('
                    && Some(node.tag) == target_tag
                    && let Some((box_line, sum, count)) = hbox_stats.pop()
                    && let Some((prev_max, _)) = readahead
                {
                    let mean = if count > 0 {
                        (sum / u64::from(count)) as u32
                    } else {
                        box_line
                    };
                    if mean > prev_max {
                        hbox_means.push(mean);
                    }
                }
                continue;
            }
            match node.kind {
                b'[' | b'(' => {
                    if node.kind == b'(' && Some(node.tag) == target_tag {
                        hbox_stats.push((u32::try_from(node.line).unwrap_or(0), 0, 0));
                    }
                    stack.push((idx, true));
                    for &child in node.children.iter().rev() {
                        stack.push((child, false));
                    }
                }
                b'v' | b'h' | b'x' | b'k' | b'g' | b'$' if Some(node.tag) == target_tag => {
                    if let Ok(line) = u32::try_from(node.line) {
                        if otag == node.tag && line < oline {
                            s_max = s_max.max(line);
                            if page == first_page && s_max >= view_line {
                                return None;
                            }
                        }
                        if let Some((prev_max, _)) = readahead {
                            if (otag, oline) != (node.tag, line) && line > prev_max {
                                min_next = Some(min_next.map_or(line, |old| old.min(line)));
                            }
                            if let Some(parent_idx) = node.parent
                                && nodes[parent_idx].kind == b'('
                                && Some(nodes[parent_idx].tag) == target_tag
                                && let Some(top) = hbox_stats.last_mut()
                            {
                                top.1 += u64::from(line);
                                top.2 += 1;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if page == first_page {
            if view_line > s_max && (s_max == 0 || oline.abs_diff(view_line) <= view_line - s_max) {
                readahead = Some((s_max, oline));
            } else {
                return None;
            }
        } else if let Some((_, prev_oline)) = readahead
            && let Some(min_line) = min_next
        {
            if min_line > prev_oline {
                return resolved;
            }
            let target_line = hbox_means
                .iter()
                .copied()
                .min_by_key(|line| (line.abs_diff(view_line), *line))
                .unwrap_or(min_line);
            resolved = Some((page, target_line));
            if Some(otag) == target_tag
                && view_line > s_max
                && (s_max == 0 || oline.abs_diff(view_line) <= view_line - s_max)
            {
                readahead = Some((s_max, oline));
            } else {
                return resolved;
            }
        }
    }
    resolved
}

fn forward_geometry_valid(page: u32, h: f32, v: f32, width: f32, height: f32) -> bool {
    page > 0
        && width >= 0.0
        && height >= 0.0
        && [h, v, width, height, h + width, v - height]
            .into_iter()
            .all(f32::is_finite)
}


fn run(operation: &Operation, args: &[&str]) -> io::Result<String> {
    let output = crate::process::output(Command::new("synctex").args(args), operation)?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "synctex failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[derive(Clone, Copy, Debug)]
pub struct InversePoint {
    pub page: u32,
    pub x: f32,
    pub y_from_top: f32,
    pub page_height_pt: f32,
}

pub fn resolve_inverse(
    pdf: &Path,
    point: InversePoint,
    word: Option<(&str, usize)>,
    radius: u32,
    operation: &Operation,
    source_snapshot: Option<&Path>,
    best_guess: bool,
) -> io::Result<InverseResolution> {
    operation.check()?;
    if point.page == 0
        || point.page_height_pt <= 0.0
        || ![point.x, point.y_from_top, point.page_height_pt]
            .into_iter()
            .all(f32::is_finite)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "inverse search requires a positive page and finite compiler coordinates",
        ));
    }
    let pdf = std::path::absolute(pdf)?;
    let snapshot = source_snapshot
        .map(|path| SourceSnapshot::load(path, &pdf))
        .transpose()?;
    resolve_inverse_with_snapshot(
        &pdf,
        point,
        word,
        radius,
        operation,
        snapshot.as_ref(),
        best_guess,
    )
}

pub(crate) fn resolve_inverse_with_snapshot(
    pdf: &Path,
    point: InversePoint,
    word: Option<(&str, usize)>,
    radius: u32,
    operation: &Operation,
    snapshot: Option<&SourceSnapshot>,
    best_guess: bool,
) -> io::Result<InverseResolution> {
    operation.check()?;
    if point.page == 0
        || point.page_height_pt <= 0.0
        || ![point.x, point.y_from_top, point.page_height_pt]
            .into_iter()
            .all(f32::is_finite)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "inverse search requires a positive page and finite compiler coordinates",
        ));
    }
    let pdf = std::path::absolute(pdf)?;
    if let Some(snapshot) = snapshot {
        snapshot.revision.check(&pdf)?;
    }
    let raw_revision = if best_guess && snapshot.is_none() {
        drop(crate::source_snapshot::open_regular(&pdf, false)?);
        Some(DocumentRevision::read(&pdf)?)
    } else {
        None
    };
    operation.check()?;
    let spec = format!(
        "{}:{}:{}:{}",
        point.page,
        point.x,
        point.y_from_top,
        pdf.display()
    );
    let stdout = run(operation, &["edit", "-o", &spec])?;
    let sources = QuerySources {
        snapshot,
        best_guess,
        ..QuerySources::default()
    };
    let result = resolve_inverse_output(&pdf, point, word, radius, operation, &stdout, sources)?;
    if let Some(snapshot) = snapshot {
        snapshot.revision.check(&pdf)?;
    }
    if let Some(revision) = raw_revision {
        revision.check(&pdf)?;
    }
    operation.check()?;
    Ok(result)
}

fn same_source_position(a: &SourceLocation, b: &SourceLocation) -> bool {
    Path::new(&a.file) == Path::new(&b.file) && a.line == b.line && a.byte_column == b.byte_column
}

fn resolve_inverse_output(
    pdf: &Path,
    point: InversePoint,
    word: Option<(&str, usize)>,
    radius: u32,
    operation: &Operation,
    stdout: &str,
    sources: QuerySources<'_>,
) -> io::Result<InverseResolution> {
    let targets = parse_synctex_edit(stdout);
    if targets.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            InverseFailure::NoMatch,
        ));
    }
    let best_guess = sources.best_guess;
    let assume_unchanged = best_guess && sources.snapshot.is_none();
    let mut query = InverseQuery {
        pdf,
        point,
        word,
        radius,
        operation,
        sources,
        structural_map: None,
        geometry_map: None,
        metadata: None,
    };
    let mut resolutions = Vec::with_capacity(targets.len());
    for target in targets {
        operation.check()?;
        resolutions.push(query.resolve_target(target)?);
    }
    operation.check()?;
    let mut precise = resolutions
        .iter()
        .enumerate()
        .filter(|(_, (result, _))| result.location.precise);
    let mut ambiguous = None;
    let selected = if let Some((index, (supported, _))) = precise.next() {
        if precise
            .any(|(_, (other, _))| !same_source_position(&supported.location, &other.location))
        {
            ambiguous = Some("ambiguous inverse search: multiple precise source locations");
        }
        index
    } else {
        if resolutions[1..]
            .iter()
            .any(|(other, _)| !same_source_position(&resolutions[0].0.location, &other.location))
        {
            ambiguous = Some("ambiguous inverse search: multiple coarse source anchors");
        }
        0
    };
    if let Some(reason) = ambiguous
        && !best_guess
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            InverseFailure::Ambiguous(reason),
        ));
    }
    // The executable's first result is its nearest compiler anchor. Explicit
    // best-guess mode cannot turn an ambiguous word refinement into precision.
    let (mut chosen, raw) = resolutions.swap_remove(if ambiguous.is_some() { 0 } else { selected });
    let mut warnings = std::collections::BTreeSet::new();
    if assume_unchanged {
        warnings.insert(
            "best-guess navigation: compiled source history unavailable; assuming saved source is unchanged since compilation"
                .into(),
        );
    }
    if let Some(reason) = ambiguous {
        if let Some(raw) = raw {
            chosen.location = raw;
        }
        chosen.location.precise = false;
        warnings.insert(format!(
            "best-guess navigation: {reason}; using the first compiler anchor"
        ));
    }
    if let Some(warning) = chosen.warning {
        warnings.insert(warning);
    }
    for (other, _) in resolutions {
        if same_source_position(&chosen.location, &other.location)
            && let Some(warning) = other.warning
        {
            warnings.insert(warning);
        }
    }
    if best_guess && !chosen.location.precise && warnings.is_empty() {
        warnings.insert("best-guess navigation: no literal source position was qualified; using the compiler line anchor".into());
    }
    operation.check()?;
    Ok(InverseResolution {
        location: chosen.location,
        warning: (!warnings.is_empty())
            .then(|| warnings.into_iter().collect::<Vec<_>>().join("; ")),
    })
}

#[derive(Default)]
struct QuerySources<'a> {
    snapshot: Option<&'a SourceSnapshot>,
    best_guess: bool,
    files: std::collections::HashMap<PathBuf, Result<Rc<String>, (String, bool)>>,
}

impl QuerySources<'_> {
    fn normalize(&self, path: &Path) -> io::Result<PathBuf> {
        match self.snapshot {
            Some(snapshot) => snapshot.normalize(path),
            None => fs::canonicalize(path),
        }
    }

    fn warning(&self, path: &Path) -> Option<&str> {
        self.snapshot.and_then(|snapshot| snapshot.warning(path))
    }

    fn read_error((message, required): (String, bool)) -> io::Error {
        let error = io::Error::other(message);
        if required {
            io::Error::other(crate::source_snapshot::SnapshotReadError(error))
        } else {
            error
        }
    }

    fn read(&mut self, path: &Path, operation: &Operation) -> io::Result<Rc<String>> {
        operation.check()?;
        if let Some(result) = self.files.get(path) {
            return result.clone().map_err(Self::read_error);
        }
        let result = match self.snapshot {
            Some(snapshot) => snapshot.read(path),
            None => read_source(path),
        }
        .map(Rc::new)
        .map_err(|error| {
            let required = crate::source_snapshot::required_cache_error(&error);
            (error.to_string(), required)
        });
        operation.check()?;
        let value = result.clone().map_err(Self::read_error);
        self.files.insert(path.to_owned(), result);
        value
    }
}

type MetadataLocation = (String, Rc<String>, u32, usize, isize);

#[derive(Debug, thiserror::Error)]
enum InverseFailure {
    #[error("synctex edit returned no match")]
    NoMatch,
    #[error("{0}")]
    Ambiguous(&'static str),
    #[error("invalid source anchor: {file}:{line} is out of range (available lines: {available})")]
    InvalidSourceAnchor {
        file: PathBuf,
        line: u32,
        available: usize,
    },
}

struct InverseAnchor {
    target: SourceLocation,
    generated: bool,
    warning: Option<String>,
    source_map: Option<io::Result<Rc<SourceMap>>>,
}

fn is_generated_sidecar(path: &Path, pdf: &Path) -> bool {
    path.file_stem() == pdf.file_stem() && crate::source_snapshot::is_compiler_sidecar_input(path)
}

struct InverseQuery<'a> {
    pdf: &'a Path,
    point: InversePoint,
    word: Option<(&'a str, usize)>,
    radius: u32,
    operation: &'a Operation,
    sources: QuerySources<'a>,
    structural_map: Option<(PathBuf, Result<Rc<SourceMap>, String>)>,
    geometry_map: Option<Result<Rc<SourceMap>, String>>,
    metadata: Option<Option<MetadataLocation>>,
}

impl InverseQuery<'_> {
    fn source_map(&mut self, generated: Option<&Path>) -> io::Result<Rc<SourceMap>> {
        self.operation.check()?;
        let current_dir = if self.sources.best_guess && self.sources.snapshot.is_none() {
            Some(std::env::current_dir()?)
        } else {
            None
        };
        let input_cwd = self
            .sources
            .snapshot
            .map(|snapshot| snapshot.cwd.as_path())
            .or(current_dir.as_deref());
        let default_vrb = self.pdf.with_extension("vrb");
        let generated_path = generated.unwrap_or(&default_vrb);
        let result = if let Some(generated) = generated {
            if generated == default_vrb
                && let Some(geometry) = &self.geometry_map
            {
                geometry
            } else {
                if self
                    .structural_map
                    .as_ref()
                    .is_none_or(|(cached, _)| cached != generated)
                {
                    let loaded = read_source_map(
                        self.pdf,
                        self.point,
                        generated_path,
                        false,
                        self.operation,
                        input_cwd,
                    )
                    .map(Rc::new)
                    .map_err(|error| error.to_string());
                    self.structural_map = Some((generated.to_owned(), loaded));
                }
                &self.structural_map.as_ref().unwrap().1
            }
        } else {
            if self.geometry_map.is_none()
                && let Some((cached, Ok(map))) = &self.structural_map
                && cached == &default_vrb
                && map.page_source.is_none()
            {
                self.geometry_map = Some(Ok(Rc::clone(map)));
            }
            self.geometry_map.get_or_insert_with(|| {
                read_source_map(
                    self.pdf,
                    self.point,
                    generated_path,
                    true,
                    self.operation,
                    input_cwd,
                )
                .map(Rc::new)
                .map_err(|error| error.to_string())
            })
        };
        self.operation.check()?;
        result.clone().map_err(io::Error::other)
    }

    fn resolve_target(
        &mut self,
        target: SourceLocation,
    ) -> io::Result<(InverseResolution, Option<SourceLocation>)> {
        let mut anchor = self.normalize_anchor(target)?;
        let Some(source) = self.qualify_source(&mut anchor)? else {
            return Ok((
                InverseResolution {
                    location: anchor.target,
                    warning: anchor.warning,
                },
                None,
            ));
        };
        self.refine_target(anchor, source)
    }

    fn normalize_anchor(&mut self, mut target: SourceLocation) -> io::Result<InverseAnchor> {
        let path = Path::new(&target.file);
        let absolute = if let Some(snapshot) = self.sources.snapshot {
            snapshot.normalize(path)?
        } else if self.sources.best_guess {
            std::path::absolute(path)?
        } else if path.is_absolute() {
            path.to_owned()
        } else {
            self.pdf.parent().unwrap_or(Path::new(".")).join(path)
        };
        let mut warning = None;
        let resolved = if self.sources.snapshot.is_some() || self.sources.best_guess {
            absolute.components().collect()
        } else {
            match fs::canonicalize(&absolute) {
                Ok(path) => path,
                Err(error) => {
                    warning = Some(format!(
                        "line-only navigation: source path unavailable: {error}"
                    ));
                    absolute
                }
            }
        };
        target.file = resolved
            .into_os_string()
            .into_string()
            .map_err(|_| io::Error::other("source path is not UTF-8"))?;
        let generated = is_generated_sidecar(Path::new(&target.file), self.pdf);
        // Known generated hits use structural sheet provenance before any
        // source bounds, and never read the final reused .vrb / sidecar file.
        let source_map = generated.then(|| self.source_map(Some(Path::new(&target.file))));
        if self.sources.best_guess
            && let Some(Err(error)) = &source_map
        {
            self.operation.check()?;
            return Err(io::Error::other(error.to_string()));
        }
        Ok(InverseAnchor {
            target,
            generated,
            warning,
            source_map,
        })
    }

    // Authority and bounds are settled before word ownership. A coarse result
    // carries its compiler anchor unchanged; only a qualified source is refined.
    fn qualify_source(&mut self, anchor: &mut InverseAnchor) -> io::Result<Option<Rc<String>>> {
        let target = &mut anchor.target;
        let generated = anchor.generated;
        let source_map = &anchor.source_map;
        let warning = &mut anchor.warning;
        if !generated
            && let Some(reason) = self
                .sources
                .warning(Path::new(&target.file))
                .map(str::to_owned)
        {
            // A provenance warning cannot waive authority for a recorded input.
            // Uncaptured inputs still retain their original coarse anchor.
            if let Err(error) = self.sources.read(Path::new(&target.file), self.operation)
                && crate::source_snapshot::required_cache_error(&error)
            {
                return Err(error);
            }
            self.operation.check()?;
            *warning = Some(reason);
            return Ok(None);
        }
        let source = if generated {
            let Some(source) = self.remap_generated_source(target, source_map, warning)? else {
                return Ok(None);
            };
            source
        } else {
            match self.sources.read(Path::new(&target.file), self.operation) {
                Ok(source) => source,
                Err(error) => {
                    self.operation.check()?;
                    if crate::source_snapshot::required_cache_error(&error) {
                        return Err(error);
                    }
                    *warning = Some(format!(
                        "line-only navigation: source refinement unavailable: {error}"
                    ));
                    return Ok(None);
                }
            }
        };
        if let Err(error) =
            validate_inverse_source_line(&source, target.line, Path::new(&target.file))
        {
            if !self.sources.best_guess {
                return Err(error);
            }
            *warning = Some(format!(
                "best-guess navigation: {error}; preserving the original compiler anchor for editor mapping"
            ));
            return Ok(None);
        }
        if let Some(reason) = self.sources.warning(Path::new(&target.file)) {
            *warning = Some(reason.to_owned());
            return Ok(None);
        }
        Ok(Some(source))
    }

    // The structural sheet anchor must identify the enclosing source frame for
    // generated sidecars (.vrb, .toc, .nav, .out, .aux, .snm).
    fn remap_generated_source(
        &mut self,
        target: &mut SourceLocation,
        source_map: &Option<io::Result<Rc<SourceMap>>>,
        warning: &mut Option<String>,
    ) -> io::Result<Option<Rc<String>>> {
        let verbatim_only = Path::new(&target.file)
            .extension()
            .is_some_and(|ext| ext == "vrb");
        let remapped = source_map
            .as_ref()
            .and_then(|map| map.as_ref().ok())
            .ok_or_else(|| {
                io::Error::other(
                    source_map
                        .as_ref()
                        .and_then(|map| map.as_ref().err())
                        .map_or_else(
                            || "original source map is unavailable".to_owned(),
                            ToString::to_string,
                        ),
                )
            })
            .and_then(|map| {
                if verbatim_only {
                    original_source_from_verbatim(map, &mut self.sources, self.operation)
                } else {
                    original_source_from_generated(map, &mut self.sources, self.operation)
                }
            });
        match remapped {
            Ok((file, original, anchor)) => {
                *warning = None;
                target.file = file;
                target.line = anchor;
                Ok(Some(original))
            }
            Err(error) => {
                self.operation.check()?;
                if crate::source_snapshot::required_cache_error(&error) {
                    return Err(error);
                }
                if self.sources.best_guess
                    && let Some(map) = source_map.as_ref().and_then(|map| map.as_ref().ok())
                {
                    let (original, line) = map
                        .page_source
                        .as_ref()
                        .map(|(path, line)| (path.as_path(), *line))
                        .unwrap_or((&map.main, 1));
                    if line == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "SyncTeX original compiler anchor line must be positive",
                        ));
                    }
                    let original = if let Some(snapshot) = self.sources.snapshot {
                        snapshot.normalize(original)?
                    } else {
                        fs::canonicalize(original).unwrap_or_else(|_| original.to_owned())
                    };
                    target.file = original
                        .into_os_string()
                        .into_string()
                        .map_err(|_| io::Error::other("source path is not UTF-8"))?;
                    target.line = line;
                    self.operation.check()?;
                }
                *warning = Some(format!(
                    "line-only navigation: original fragile frame unavailable: {error}"
                ));
                Ok(None)
            }
        }
    }

    fn refine_target(
        &mut self,
        anchor: InverseAnchor,
        mut source: Rc<String>,
    ) -> io::Result<(InverseResolution, Option<SourceLocation>)> {
        let InverseAnchor {
            mut target,
            generated,
            mut warning,
            mut source_map,
        } = anchor;
        let anchor = (self.sources.best_guess && self.word.is_some()).then(|| target.clone());
        if let Some((context, offset)) = self.word {
            let fragile_frame = fragile_frame_range(&source, target.line);
            if !generated {
                // Ordinary source refinement and document metadata need only
                // structural provenance, not fragile-body geometry.
                let default_vrb = self.pdf.with_extension("vrb");
                source_map = Some(self.source_map(fragile_frame.is_none().then_some(&default_vrb)));
            }
            if self.sources.best_guess
                && let Some(Err(error)) = &source_map
            {
                self.operation.check()?;
                return Err(io::Error::other(error.to_string()));
            }
            self.operation.check()?;
            // A fragile sheet also contains nongenerated headings and footers.
            // Sheet-wide .vrb participation does not prove this point is body text.
            let scope_proven = fragile_frame.as_ref().is_none_or(|frame| {
                generated
                    || source_map
                        .as_ref()
                        .and_then(|result| result.as_ref().ok())
                        .is_some_and(|map| {
                            map.generated_at_point
                                && map.page_source.as_ref().is_some_and(|(path, line)| {
                                    *line == frame.end as u32
                                        && self
                                            .sources
                                            .normalize(path)
                                            .is_ok_and(|path| path == Path::new(&target.file))
                                })
                        })
            });
            let mut location = if scope_proven {
                source_word_location(&source, target.line, context, offset, self.radius)
            } else {
                source_word_location_in_range(
                    &source,
                    target.line,
                    context,
                    offset,
                    target.line.saturating_sub(1) as usize..target.line as usize,
                    false,
                )
            };
            if location.is_none() && !generated {
                if fragile_frame.is_none()
                    && source_map
                        .as_ref()
                        .and_then(|result| result.as_ref().ok())
                        .is_some_and(|map| map.page_source.is_some())
                {
                    source_map = Some(self.source_map(None));
                    if self.sources.best_guess
                        && let Some(Err(error)) = &source_map
                    {
                        self.operation.check()?;
                        return Err(io::Error::other(error.to_string()));
                    }
                }
                if let Some(map) = source_map
                    .as_ref()
                    .and_then(|result| result.as_ref().ok())
                    .filter(|map| map.generated_at_point && map.page_source.is_some())
                {
                    match original_source_from_verbatim(map, &mut self.sources, self.operation) {
                        Ok((file, original, anchor)) => {
                            if self.sources.warning(Path::new(&file)).is_none()
                                && let Some(candidate) = source_word_location(
                                    &original,
                                    anchor,
                                    context,
                                    offset,
                                    self.radius,
                                )
                            {
                                target.file = file;
                                target.line = anchor;
                                source = original;
                                location = Some(candidate);
                            }
                        }
                        Err(error) => {
                            self.operation.check()?;
                            if crate::source_snapshot::required_cache_error(&error) {
                                return Err(error);
                            }
                            warning = Some(format!(
                                "line-only navigation: original fragile frame unavailable: {error}"
                            ));
                        }
                    }
                }
            }
            if let Some(Err(error)) = &source_map {
                self.operation.check()?;
                warning = Some(format!("source-map refinement unavailable: {error}"));
            }
            if !generated
                && let Some(map) = source_map.as_ref().and_then(|result| result.as_ref().ok())
                && if location.is_some() {
                    source_frame_range(&source, target.line).is_some()
                } else {
                    allows_metadata_fallback(
                        &source,
                        target.line,
                        self.radius,
                        self.point.y_from_top,
                        self.point.page_height_pt,
                    )
                }
            {
                if self.metadata.is_none() {
                    self.metadata = Some(document_metadata_word_location(
                        &map.main,
                        context,
                        offset,
                        &mut self.sources,
                        self.operation,
                    )?);
                }
                if let Some((file, original, line, byte, score)) =
                    self.metadata.as_ref().and_then(Option::as_ref)
                    && (location.is_none()
                        || *score >= 6
                            && metadata_beats_frame(
                                &source,
                                target.line,
                                context,
                                offset,
                                self.point.y_from_top,
                                self.point.page_height_pt,
                                *score,
                            ))
                {
                    target.file = file.clone();
                    source = Rc::clone(original);
                    location = Some((*line, *byte));
                }
            }
            if let Some((line, byte)) = location {
                let text = validate_inverse_source_line(&source, line, Path::new(&target.file))?;
                let prefix = text
                    .get(..byte)
                    .ok_or_else(|| io::Error::other("source column is not a UTF-8 boundary"))?;
                target.line = line;
                target.byte_column = byte;
                target.column = prefix.encode_utf16().count() + 1;
                target.column_char = prefix.chars().count() + 1;
                target.precise = true;
            }
        }
        self.operation.check()?;
        Ok((
            InverseResolution {
                location: target,
                warning,
            },
            anchor,
        ))
    }
}

fn validate_source_line(source: &str, line: u32) -> io::Result<&str> {
    line.checked_sub(1)
        .and_then(|index| source.lines().nth(index as usize))
        .ok_or_else(|| io::Error::other(format!("source line {line} is out of range")))
}

fn validate_inverse_source_line<'a>(
    source: &'a str,
    line: u32,
    file: &Path,
) -> io::Result<&'a str> {
    line.checked_sub(1)
        .and_then(|index| source.lines().nth(index as usize))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                InverseFailure::InvalidSourceAnchor {
                    file: file.to_owned(),
                    line,
                    available: source.lines().count(),
                },
            )
        })
}

fn validate_forward_source_position(source: &str, line: u32, column: u32) -> io::Result<&str> {
    let text = validate_source_line(source, line)?;
    if column == 0 || column as usize > text.chars().count() + 1 {
        return Err(io::Error::other("source scalar column is out of range"));
    }
    Ok(text)
}

fn allows_metadata_fallback(
    source: &str,
    line: u32,
    radius: u32,
    y_from_top: f32,
    page_height_pt: f32,
) -> bool {
    if source_frame_range(source, line).is_some() {
        return true;
    }
    if page_height_pt > 0.0
        && (y_from_top <= page_height_pt * 0.12 || y_from_top >= page_height_pt * 0.88)
    {
        return true;
    }
    let start = line.saturating_sub(radius.saturating_add(1)) as usize;
    let end = (line as usize).saturating_add(radius as usize);
    source
        .lines()
        .skip(start)
        .take(end.saturating_sub(start))
        .any(|row| {
            let text = source_line_text(row);
            text.contains("\\maketitle") || text.contains("\\titlepage")
        })
}

/// A frame title and the running document title may contain the same words.
/// Preserve a local frame hit except when a footer has stronger metadata context.
fn metadata_beats_frame(
    source: &str,
    line: u32,
    context: &str,
    offset: usize,
    y_from_top: f32,
    page_height_pt: f32,
    metadata_score: isize,
) -> bool {
    let Some(frame) = source_frame_range(source, line) else {
        return false;
    };
    y_from_top >= page_height_pt / 2.0
        && source_prose_scored_location(
            source,
            line,
            context,
            offset,
            frame,
            true,
            &nonprinting_source_ranges(source),
        )
        .is_some_and(|(_, _, local_score)| metadata_score >= local_score)
}

fn read_source(path: &Path) -> io::Result<String> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    read_source_file(file)
}

pub(crate) fn read_source_file(file: fs::File) -> io::Result<String> {
    let mut bytes = Vec::new();
    if !file.metadata()?.is_file() {
        return Err(io::Error::other(
            "source refinement requires a regular file",
        ));
    }
    file.take(2 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 2 * 1024 * 1024 {
        return Err(io::Error::other("source exceeds 2 MiB refinement limit"));
    }
    String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

struct SourceMap {
    main: PathBuf,
    page_source: Option<(PathBuf, u32)>,
    generated_at_point: bool,
}

#[derive(Clone, Copy)]
struct SourceBox {
    bounds: [f64; 4],
    generated: bool,
}

fn source_box_bounds(row: &str, position: [f64; 2]) -> io::Result<Option<[f64; 4]>> {
    let parse = || {
        let size = row.split(':').nth(2)?;
        let mut size = size.split(',');
        let [width, height, depth] = [size.next()?, size.next()?, size.next()?]
            .map(|value| value.parse::<f64>().ok().filter(|value| value.is_finite()));
        Some([position[0], position[1], width?, height?, depth?])
    };
    let [x, y, width, height, depth] =
        parse().ok_or_else(|| io::Error::other("SyncTeX horizontal box has invalid geometry"))?;
    Ok((width > 0.0 && height + depth > 0.0).then_some([x, y - height, x + width, y + depth]))
}

fn source_map_position(row: &str, last_v: &mut f64) -> io::Result<[f64; 2]> {
    let position = row
        .split(':')
        .nth(1)
        .and_then(|position| position.split_once(','))
        .ok_or_else(|| io::Error::other("SyncTeX node has invalid position"))?;
    let x = source_map_number(position.0)?;
    let y = if position.1 == "=" {
        *last_v
    } else {
        let y = source_map_number(position.1)?;
        *last_v = y;
        y
    };
    Ok([x, y])
}

fn generated_box_at_point(
    boxes: &[SourceBox],
    x: f64,
    y: f64,
    operation: &Operation,
) -> io::Result<bool> {
    let contains =
        |a: [f64; 4], b: [f64; 4]| a[0] <= b[0] && a[1] <= b[1] && a[2] >= b[2] && a[3] >= b[3];
    let mut nearest: Vec<SourceBox> = Vec::new();
    for candidate in boxes {
        operation.check()?;
        if !contains(candidate.bounds, [x, y, x, y]) {
            continue;
        }
        if let Some(equal) = nearest
            .iter_mut()
            .find(|old| old.bounds == candidate.bounds)
        {
            equal.generated &= candidate.generated;
        } else if !nearest
            .iter()
            .any(|old| contains(candidate.bounds, old.bounds))
        {
            nearest.retain(|old| !contains(old.bounds, candidate.bounds));
            nearest.push(*candidate);
        }
    }
    // Conflicting nonnested overlaps and equal boxes with mixed ownership abstain.
    Ok(nearest.len() == 1 && nearest[0].generated)
}

fn source_map_number(value: &str) -> io::Result<f64> {
    value
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
        .ok_or_else(|| io::Error::other("SyncTeX transform has an invalid number"))
}

fn source_map_dimension(value: &str) -> io::Result<f64> {
    let units = [
        ("in", 72.27 * 65536.0),
        ("cm", 72.27 * 65536.0 / 2.54),
        ("mm", 72.27 * 65536.0 / 25.4),
        ("pt", 65536.0),
        ("bp", 72.27 / 72.0 * 65536.0),
        ("pc", 12.0 * 65536.0),
        ("sp", 1.0),
        ("dd", 1238.0 / 1157.0 * 65536.0),
        ("cc", 14856.0 / 1157.0 * 65536.0),
        ("nd", 685.0 / 642.0 * 65536.0),
        ("nc", 1370.0 / 107.0 * 65536.0),
    ];
    for (suffix, scale) in units {
        if let Some(number) = value.trim().strip_suffix(suffix) {
            return source_map_number(number).map(|number| number * scale);
        }
    }
    source_map_number(value)
}

fn open_synctex(pdf: &Path) -> io::Result<Box<dyn BufRead>> {
    let reader: Box<dyn Read> = match fs::File::open(pdf.with_extension("synctex.gz")) {
        Ok(file) => Box::new(
            flate2::read::GzDecoder::new(BufReader::with_capacity(64 * 1024, file))
                .take(128 * 1024 * 1024),
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Box::new(fs::File::open(pdf.with_extension("synctex"))?.take(128 * 1024 * 1024))
        }
        Err(error) => return Err(error),
    };
    Ok(Box::new(BufReader::with_capacity(64 * 1024, reader)))
}

fn synctex_node_link(row: &str) -> Option<(u32, u32)> {
    let (link, _) = row.get(1..)?.split_once(':')?;
    let (tag, rest) = link.split_once(',')?;
    Some((
        tag.parse::<u32>().ok()?,
        rest.split(',').next()?.parse::<u32>().ok()?,
    ))
}

fn read_source_map(
    pdf: &Path,
    point: InversePoint,
    generated: &Path,
    geometry_required: bool,
    operation: &Operation,
    input_cwd: Option<&Path>,
) -> io::Result<SourceMap> {
    source_map_from_reader(
        open_synctex(pdf)?,
        pdf,
        point,
        generated,
        geometry_required,
        operation,
        input_cwd,
    )
}

fn source_map_from_reader(
    mut reader: impl BufRead,
    pdf: &Path,
    point: InversePoint,
    generated: &Path,
    geometry_required: bool,
    operation: &Operation,
    input_cwd: Option<&Path>,
) -> io::Result<SourceMap> {
    let mut inputs = std::collections::HashMap::new();
    let mut generated_tags = std::collections::HashSet::new();
    let mut row = String::new();
    let mut in_page = false;
    let mut page_complete = false;
    let mut origin = None;
    let mut generated_on_page = false;
    let mut page_tags = std::collections::HashSet::new();
    let mut stack: Vec<(u8, Option<[f64; 4]>, bool)> = Vec::new();
    let mut boxes = Vec::new();
    let mut defined_forms = std::collections::HashSet::new();
    let mut sheet_form_refs = std::collections::HashSet::new();
    let mut unit = None;
    let mut magnification = None;
    let mut x_offset = None;
    let mut y_offset = None;
    let mut post_magnification = 1.0;
    let mut post_x_offset = None;
    let mut post_y_offset = None;
    let mut post_scriptum = false;
    let mut postamble = false;
    let mut postamble_count = false;
    let mut bytes_read = 0;
    let mut lines = 0usize;
    let mut last_v = -1.0;
    loop {
        if lines & 1023 == 0 {
            operation.check()?;
        }
        lines = lines.wrapping_add(1);
        row.clear();
        let count = reader.read_line(&mut row)?;
        bytes_read += count;
        if bytes_read >= 128 * 1024 * 1024 {
            return Err(io::Error::other("SyncTeX source map exceeds 128 MiB"));
        }
        if count == 0 {
            break;
        }
        let row = if input_cwd.is_some() {
            row.trim_end_matches(['\r', '\n'])
        } else {
            row.trim_end()
        };
        if (geometry_required || input_cwd.is_some()) && row == "Postamble:" {
            postamble = true;
        } else if (geometry_required || input_cwd.is_some())
            && postamble
            && let Some(value) = row.strip_prefix("Count:")
        {
            value
                .parse::<u32>()
                .map_err(|_| io::Error::other("SyncTeX postamble has invalid Count"))?;
            postamble_count = true;
        }
        if geometry_required && row == "Post scriptum:" {
            post_scriptum = true;
        } else if geometry_required && let Some(value) = row.strip_prefix("Unit:") {
            unit = Some(source_map_number(value)?);
        } else if geometry_required && let Some(value) = row.strip_prefix("Magnification:") {
            let value = source_map_number(value)?;
            if post_scriptum {
                post_magnification = value;
            } else {
                magnification = Some(value);
            }
        } else if geometry_required && let Some(value) = row.strip_prefix("X Offset:") {
            if post_scriptum {
                post_x_offset = Some(source_map_dimension(value)?);
            } else {
                x_offset = Some(source_map_number(value)?);
            }
        } else if geometry_required && let Some(value) = row.strip_prefix("Y Offset:") {
            if post_scriptum {
                post_y_offset = Some(source_map_dimension(value)?);
            } else {
                y_offset = Some(source_map_number(value)?);
            }
        }
        // Compressed ",=" reuses the scanner's global last vertical value,
        // including records from earlier sheets and form definitions.
        // Later positions cannot affect this query's completed sheet, but the
        // remaining input tags, transforms, and postamble still require a scan.
        let position = if geometry_required
            && !page_complete
            && matches!(
                row.as_bytes().first(),
                Some(b'[' | b'(' | b'v' | b'h' | b'x' | b'k' | b'g' | b'r' | b'$' | b'f')
            ) {
            Some(source_map_position(row, &mut last_v)?)
        } else {
            None
        };
        if geometry_required && let Some(tag) = row.strip_prefix('<') {
            let tag = tag
                .parse::<u32>()
                .ok()
                .filter(|tag| *tag > 0)
                .ok_or_else(|| io::Error::other("SyncTeX form has an invalid tag"))?;
            defined_forms.insert(tag);
        }
        if let Some(input) = row.strip_prefix("Input:") {
            if let Some((tag, path)) = input.split_once(':')
                && let Ok(tag) = tag.parse::<u32>()
            {
                let path = Path::new(path);
                if input_cwd.is_some() && (tag == 0 || path.as_os_str().is_empty()) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid SyncTeX input declaration",
                    ));
                }
                let path = if path.is_absolute() {
                    path.to_owned()
                } else {
                    input_cwd
                        .unwrap_or_else(|| pdf.parent().unwrap_or(Path::new(".")))
                        .join(path)
                };
                if input_cwd.is_some() && !valid_absolute_path(&path) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid SyncTeX input path",
                    ));
                }
                // TeX can emit .vrb in the build cwd while the PDF is in a
                // separate output directory. Jobname is the map identity;
                // the final generated file is neither read nor trusted.
                if path.file_name() == generated.file_name() {
                    generated_tags.insert(tag);
                }
                match inputs.entry(tag) {
                    std::collections::hash_map::Entry::Occupied(mut entry) => {
                        if input_cwd.is_some() && entry.get() != &path {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "conflicting SyncTeX input tag",
                            ));
                        }
                        if input_cwd.is_none() {
                            entry.insert(path);
                        }
                    }
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(path);
                    }
                }
            } else if input_cwd.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "malformed SyncTeX input declaration",
                ));
            }
        } else if let Some(page) = row.strip_prefix('{') {
            in_page = page.parse::<u32>() == Ok(point.page);
            if in_page && page_complete {
                return Err(io::Error::other(
                    "SyncTeX source map repeats the clicked sheet",
                ));
            }
        } else if in_page && row.starts_with('}') {
            if row[1..].parse::<u32>() != Ok(point.page) || !stack.is_empty() {
                return Err(io::Error::other(
                    "SyncTeX source map has mismatched sheet bounds",
                ));
            }
            page_complete = true;
            in_page = false;
            if !geometry_required && input_cwd.is_none() {
                operation.check()?;
                let main = inputs
                    .get(&1)
                    .cloned()
                    .ok_or_else(|| io::Error::other("SyncTeX source map has no main input"))?;
                let page_source = origin
                    .filter(|_| generated_on_page)
                    .and_then(|(tag, line)| inputs.remove(&tag).map(|path| (path, line)));
                return Ok(SourceMap {
                    main,
                    page_source,
                    generated_at_point: false,
                });
            }
        } else if in_page {
            if geometry_required && matches!(row.as_bytes().first(), Some(b'<' | b'>')) {
                return Err(io::Error::other(
                    "SyncTeX fragile body ownership cannot resolve sheet form transforms",
                ));
            }
            if geometry_required && let Some(reference) = row.strip_prefix('f') {
                let tag = reference
                    .split_once(':')
                    .and_then(|(tag, _)| tag.parse::<u32>().ok())
                    .filter(|tag| *tag > 0)
                    .ok_or_else(|| io::Error::other("SyncTeX form reference has an invalid tag"))?;
                sheet_form_refs.insert(tag);
            }
            let link = synctex_node_link(row);
            // Only the first box enclosing the whole sheet proves provenance.
            if origin.is_none() {
                if !matches!(row.as_bytes().first(), Some(b'[' | b'(')) || link.is_none() {
                    return Err(io::Error::other(
                        "SyncTeX sheet has no enclosing source box",
                    ));
                }
                origin = link;
            }
            let generated_node = link.is_some_and(|(tag, _)| generated_tags.contains(&tag));
            if geometry_required {
                match row.as_bytes().first() {
                    Some(kind @ (b'[' | b'(')) => {
                        let bounds = if *kind == b'(' {
                            source_box_bounds(
                                row,
                                position.ok_or_else(|| {
                                    io::Error::other("SyncTeX horizontal box has no position")
                                })?,
                            )?
                        } else {
                            None
                        };
                        stack.push((*kind, bounds, generated_node));
                    }
                    Some(b']' | b')') => {
                        let (kind, bounds, generated) = stack.pop().ok_or_else(|| {
                            io::Error::other("SyncTeX sheet has an unmatched box close")
                        })?;
                        if (kind == b'(') != row.starts_with(')') {
                            return Err(io::Error::other(
                                "SyncTeX sheet has mismatched box bounds",
                            ));
                        }
                        if let Some(bounds) = bounds {
                            boxes.push(SourceBox { bounds, generated });
                        }
                        if let Some(parent) = stack.last_mut() {
                            parent.2 |= generated;
                        }
                    }
                    _ => {
                        if generated_node && let Some(parent) = stack.last_mut() {
                            parent.2 = true;
                        }
                    }
                }
            }
            if matches!(
                row.as_bytes().first(),
                Some(b'[' | b'(' | b'v' | b'h' | b'k' | b'g' | b'x' | b'$' | b'r')
            ) && let Some((tag, _)) = link
            {
                generated_on_page |= generated_tags.contains(&tag);
                if input_cwd.is_some() && !geometry_required {
                    page_tags.insert(tag);
                }
            }
        }
    }
    operation.check()?;
    if !page_complete || !postamble_count {
        return Err(io::Error::other("SyncTeX source map is incomplete"));
    }
    if !geometry_required {
        generated_on_page |= page_tags.iter().any(|tag| generated_tags.contains(tag));
        let main = inputs
            .get(&1)
            .cloned()
            .ok_or_else(|| io::Error::other("SyncTeX source map has no main input"))?;
        let page_source = origin
            .filter(|_| generated_on_page)
            .and_then(|(tag, line)| inputs.remove(&tag).map(|path| (path, line)));
        return Ok(SourceMap {
            main,
            page_source,
            generated_at_point: false,
        });
    }
    // With form recording disabled, TeX still emits sheet references whose
    // definitions are absent. The native SyncTeX parser removes those refs:
    // they carry no source ownership. Do not invent geometry or source tags.
    if sheet_form_refs
        .iter()
        .any(|tag| defined_forms.contains(tag))
    {
        return Err(io::Error::other(
            "SyncTeX fragile body ownership cannot resolve recorded sheet form transforms",
        ));
    }
    // Match SyncTeX's effective preamble/Post scriptum transform. The offsets
    // are not magnified; post offsets are dimensions in scaled TeX points.
    let unit = unit
        .filter(|unit| *unit > 0.0)
        .ok_or_else(|| io::Error::other("SyncTeX source map has no positive Unit"))?;
    let magnification = magnification
        .filter(|magnification| *magnification > 0.0)
        .ok_or_else(|| io::Error::other("SyncTeX source map has no positive Magnification"))?;
    if post_magnification <= 0.0 {
        return Err(io::Error::other(
            "SyncTeX post magnification is not positive",
        ));
    }
    let pre_unit = unit / 65781.76;
    let visible_unit = pre_unit * magnification / 1000.0 * post_magnification;
    let (x_offset, y_offset) = if let Some(x) = post_x_offset {
        let y = post_y_offset
            .ok_or_else(|| io::Error::other("SyncTeX post X Offset has no Y Offset"))?;
        (x / 65781.76, y / 65781.76)
    } else {
        (
            x_offset.ok_or_else(|| io::Error::other("SyncTeX source map has no X Offset"))?
                * pre_unit,
            y_offset.ok_or_else(|| io::Error::other("SyncTeX source map has no Y Offset"))?
                * pre_unit,
        )
    };
    if !visible_unit.is_finite()
        || visible_unit <= 0.0
        || !x_offset.is_finite()
        || !y_offset.is_finite()
    {
        return Err(io::Error::other(
            "SyncTeX effective transform is not finite",
        ));
    }
    let main = inputs
        .get(&1)
        .cloned()
        .ok_or_else(|| io::Error::other("SyncTeX source map has no main input"))?;
    let page_source = origin
        .filter(|_| generated_on_page)
        .and_then(|(tag, line)| inputs.remove(&tag).map(|path| (path, line)));
    let generated_at_point = generated_box_at_point(
        &boxes,
        (f64::from(point.x) - x_offset) / visible_unit,
        (f64::from(point.y_from_top) - y_offset) / visible_unit,
        operation,
    )?;
    Ok(SourceMap {
        main,
        page_source,
        generated_at_point,
    })
}

/// Beamer reopens the same .vrb for successive fragile frames. The clicked
/// sheet's enclosing source-map box identifies the original frame; the final
/// generated file's contents cannot establish that identity.
fn original_source_from_verbatim(
    map: &SourceMap,
    sources: &mut QuerySources<'_>,
    operation: &Operation,
) -> io::Result<(String, Rc<String>, u32)> {
    let (file, source, line) = original_source_from_generated(map, sources, operation)?;
    fragile_frame_range(&source, line)
        .filter(|frame| line == frame.end as u32)
        .ok_or_else(|| {
            io::Error::other("clicked sheet does not identify a fragile frame's closing line")
        })?;
    Ok((file, source, line))
}

fn original_source_from_generated(
    map: &SourceMap,
    sources: &mut QuerySources<'_>,
    operation: &Operation,
) -> io::Result<(String, Rc<String>, u32)> {
    let (original, line) = map.page_source.as_ref().ok_or_else(|| {
        io::Error::other("clicked sheet has no original generated-file provenance")
    })?;
    let original = sources.normalize(original)?;
    let source = sources.read(&original, operation)?;
    validate_inverse_source_line(&source, *line, &original)?;
    let file = original
        .to_str()
        .ok_or_else(|| io::Error::other("original fragile source path is not UTF-8"))?
        .to_owned();
    Ok((file, source, *line))
}

fn fragile_frame_range(source: &str, line: u32) -> Option<std::ops::Range<usize>> {
    let frame = source_frame_range(source, line)?;
    let row = source_line_text(source.lines().nth(frame.start)?);
    let row_start: usize = source
        .split_inclusive('\n')
        .take(frame.start)
        .map(str::len)
        .sum();
    let mut at = row
        .match_indices("\\begin")
        .filter(|(at, _)| {
            row[..*at]
                .bytes()
                .rev()
                .take_while(|ch| *ch == b'\\')
                .count()
                % 2
                == 0
        })
        .find_map(|(at, _)| {
            let (environment, end) = math::group(source, row_start + at + "\\begin".len())?;
            (&source[environment] == "frame").then_some(end)
        })?;
    at = skip_source_space(source, at);
    if source.as_bytes().get(at) == Some(&b'<') {
        let end = source[at..].find('>')?;
        at = skip_source_space(source, at + end + 1);
    }
    let end = optional_argument_end(source, at)?;
    let options = &source[at + 1..end - 1];
    options
        .split(',')
        .flat_map(str::lines)
        .any(|option| {
            let option = source_line_text(option).trim();
            option == "fragile" || option.starts_with("fragile=")
        })
        .then_some(frame)
}

/// Title pages and running headers can be produced from declarations far
/// outside SyncTeX's reported frame. Search only literal document metadata,
/// including direct preamble inputs, and require neighboring PDF words.
fn document_metadata_word_location(
    main: &Path,
    context: &str,
    offset: usize,
    sources: &mut QuerySources<'_>,
    operation: &Operation,
) -> io::Result<Option<MetadataLocation>> {
    operation.check()?;
    let Ok(main) = sources.normalize(main) else {
        return Ok(None);
    };
    if sources.warning(&main).is_some() {
        return Ok(None);
    }
    let main_source = match sources.read(&main, operation) {
        Ok(source) => source,
        Err(error) if crate::source_snapshot::required_cache_error(&error) => return Err(error),
        Err(_) => {
            operation.check()?;
            return Ok(None);
        }
    };
    let mut files = vec![(main.clone(), main_source)];
    let inputs: Vec<_> = files[0]
        .1
        .lines()
        .take_while(|row| !source_line_text(row).contains("\\begin{document}"))
        .filter_map(|row| {
            source_line_text(row)
                .trim()
                .strip_prefix("\\input{")
                .and_then(|rest| rest.split_once('}'))
                .map(|(name, _)| name.to_owned())
        })
        .take(8)
        .collect();
    for name in inputs {
        operation.check()?;
        let Some(parent) = main.parent() else {
            return Ok(None);
        };
        let mut path = parent.join(name);
        if path.extension().is_none() {
            path.set_extension("tex");
        }
        if let Ok(path) = sources.normalize(&path)
            && !files.iter().any(|(known, _)| *known == path)
            && sources.warning(&path).is_none()
        {
            match sources.read(&path, operation) {
                Ok(source) => files.push((path, source)),
                Err(error) if crate::source_snapshot::required_cache_error(&error) => {
                    return Err(error);
                }
                Err(_) => operation.check()?,
            }
        }
    }
    let mut best = None;
    let mut tied = false;
    for (file_index, (_, source)) in files.iter().enumerate() {
        operation.check()?;
        let hidden = nonprinting_source_ranges(source);
        let mut start = 0;
        for raw_row in source.split_inclusive('\n') {
            operation.check()?;
            let row = raw_row.strip_suffix('\n').unwrap_or(raw_row);
            let row = row.strip_suffix('\r').unwrap_or(row);
            let visible = source_line_text(row);
            for command in ["\\title", "\\subtitle", "\\author", "\\date", "\\institute"] {
                for (at, _) in visible.match_indices(command) {
                    let after_cmd = start + at + command.len();
                    if source
                        .as_bytes()
                        .get(after_cmd)
                        .is_some_and(|ch| ch.is_ascii_alphabetic() || *ch == b'@')
                    {
                        continue;
                    }
                    let mut arg_at =
                        after_cmd + usize::from(source.as_bytes().get(after_cmd) == Some(&b'*'));
                    let mut spans = Vec::with_capacity(2);
                    while let Some(opt_end) = optional_argument_end(source, arg_at) {
                        let opt_start = skip_source_space(source, arg_at) + 1;
                        spans.push(opt_start..opt_end - 1);
                        arg_at = opt_end;
                    }
                    if let Some((body, end)) =
                        math::group(source, skip_source_space(source, arg_at))
                    {
                        spans.push(body.start..end);
                    }
                    let mut cmd_best: Option<(isize, u32, usize)> = None;
                    for span in spans {
                        let first = source[..span.start]
                            .bytes()
                            .filter(|ch| *ch == b'\n')
                            .count();
                        let last = source[..span.end].bytes().filter(|ch| *ch == b'\n').count();
                        let mut span_hidden = hidden.clone();
                        span_hidden.push(0..span.start);
                        span_hidden.push(span.end..source.len());
                        let Some((line, byte, score)) = source_prose_scored_location(
                            source,
                            1,
                            context,
                            offset,
                            first..last + 1,
                            true,
                            &span_hidden,
                        ) else {
                            continue;
                        };
                        if score >= 3 && cmd_best.is_none_or(|(old, _, _)| score >= old) {
                            cmd_best = Some((score, line, byte));
                        }
                    }
                    if let Some((score, line, byte)) = cmd_best {
                        match best {
                            Some((old, _, _, _)) if score < old => {}
                            Some((old, index, old_line, old_byte)) if score == old => {
                                tied |= (file_index, line, byte) != (index, old_line, old_byte);
                            }
                            _ => {
                                best = Some((score, file_index, line, byte));
                                tied = false;
                            }
                        }
                    }
                }
            }
            start += raw_row.len();
        }
    }
    operation.check()?;
    let Some((score, index, line, byte)) = best.filter(|_| !tied) else {
        return Ok(None);
    };
    let (path, source) = files.swap_remove(index);
    let Some(file) = path.to_str() else {
        return Ok(None);
    };
    Ok(Some((file.to_owned(), source, line, byte, score)))
}

pub(crate) fn parse_synctex_edit(stdout: &str) -> Vec<SourceLocation> {
    let mut input = None;
    let mut seen = std::collections::HashSet::new();
    let mut results = Vec::new();
    for row in stdout.lines() {
        if let Some(value) = row.strip_prefix("Input:") {
            input = Some(value.trim());
        } else if let Some(value) = row.strip_prefix("Line:")
            && let Some(file) = input.take()
            && !file.is_empty()
            && let Ok(line) = value.trim().parse::<u32>()
            && line > 0
            && seen.insert((file, line))
        {
            results.push(SourceLocation {
                file: file.to_owned(),
                line,
                byte_column: 0,
                column: 1,
                column_char: 1,
                precise: false,
            });
        }
    }
    results
}
/// TeX comments start at an unescaped percent sign.
fn source_line_text(text: &str) -> &str {
    let mut escaped = false;
    for (index, byte) in text.bytes().enumerate() {
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == b'%' {
            return &text[..index];
        }
    }
    text
}

/// Beamer attributes frame bodies to the frame's last source line: forward
/// search refines against the closing line, inverse search against the range.
fn frame_bounds(source: &str, anchor: usize) -> Option<(usize, usize)> {
    let mut ignored = Vec::new();
    let mut scan = 0;
    while scan < source.len() {
        match source.as_bytes()[scan] {
            b'%' => {
                scan += source[scan..].find('\n').unwrap_or(source.len() - scan);
            }
            b'\\' => {
                let start = scan;
                let (command, end) = math::command(source, scan);
                if end == start + 1 {
                    scan = end;
                    continue;
                }
                if let Some((opaque_end, _)) = math::opaque_command_end(source, command, end) {
                    ignored.push(start..opaque_end);
                    scan = opaque_end;
                    continue;
                }
                scan = end;
            }
            _ => {
                scan += source[scan..].chars().next().map_or(1, char::len_utf8);
            }
        }
    }
    ignored.extend(nonprinting_source_ranges(source));
    let mut start = None;
    let mut line_offset = 0;
    for (index, raw_row) in source.split_inclusive('\n').enumerate() {
        let row = raw_row.strip_suffix('\n').unwrap_or(raw_row);
        let row = row.strip_suffix('\r').unwrap_or(row);
        let visible = source_line_text(row);
        let mut cursor = 0;
        while let Some(rel) = visible[cursor..].find('\\') {
            let slash = cursor + rel;
            let abs = line_offset + slash;
            cursor = slash + 1;
            if visible[cursor..].starts_with('\\') {
                cursor += 1;
                continue;
            }
            if ignored.iter().any(|range| range.contains(&abs)) {
                continue;
            }
            let text = &visible[cursor..];
            if let Some(rest) = text
                .strip_prefix("begin")
                .and_then(|rest| rest.trim_start().strip_prefix("{frame}"))
            {
                cursor = visible.len() - rest.len();
                // Nested frames are not a reliable source boundary.
                if start.replace(index).is_some() {
                    return None;
                }
            } else if let Some(rest) = text
                .strip_prefix("end")
                .and_then(|rest| rest.trim_start().strip_prefix("{frame}"))
            {
                cursor = visible.len() - rest.len();
                if let Some(begin) = start.take() {
                    if begin <= anchor && anchor <= index {
                        return Some((begin, index));
                    }
                } else if anchor <= index {
                    return Some((index, index));
                }
            }
        }
        line_offset += raw_row.len();
        if index >= anchor && start.is_none() {
            return None;
        }
    }
    None
}

/// Find the literal frame whose body closes at or after the one-based
/// SyncTeX line. Beamer forwards map interior lines to the closing line.
pub(crate) fn frame_closing_line(source: &str, line: u32) -> Option<u32> {
    frame_bounds(source, line.checked_sub(1)? as usize).map(|(_, end)| end as u32 + 1)
}

fn caption_closing_line(source: &str, line: u32, column: u32) -> Option<u32> {
    let row = validate_forward_source_position(source, line, column).ok()?;
    // End-of-line is a cursor boundary, not a character in the argument.
    let byte = row.char_indices().nth(column.checked_sub(1)? as usize)?.0;
    let start = source
        .split_inclusive('\n')
        .take(line.checked_sub(1)? as usize)
        .map(str::len)
        .sum::<usize>();
    let body = math::caption_argument_at(source, start + byte)?;
    u32::try_from(source[..body.end].bytes().filter(|ch| *ch == b'\n').count())
        .ok()?
        .checked_add(1)
}

/// Find a literal frame environment enclosing the one-based SyncTeX line.
fn source_frame_range(source: &str, line: u32) -> Option<std::ops::Range<usize>> {
    frame_bounds(source, line.checked_sub(1)? as usize).map(|(start, end)| start..end + 1)
}

pub(crate) fn words(text: &str) -> Vec<(usize, &str)> {
    let mut result = Vec::new();
    let mut start = None;
    for (index, ch) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
    {
        if ch.is_alphanumeric() || start.is_some() && is_combining_mark(ch) {
            start.get_or_insert(index);
        } else if let Some(start) = start.take() {
            result.push((start, &text[start..index]));
        }
    }
    result
}

pub(crate) fn word_index_at(text: &str, words: &[(usize, &str)], offset: usize) -> Option<usize> {
    if let Some(index) = words
        .iter()
        .position(|(start, word)| *start <= offset && offset < start + word.len())
    {
        return Some(index);
    }
    let ch = text.get(offset..)?.chars().next()?;
    if matches!(
        ch,
        '-' | '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\'' | '\u{2019}'
    ) {
        let next = offset + ch.len_utf8();
        return words
            .iter()
            .position(|(start, word)| start + word.len() == offset || *start == next);
    }
    None
}

fn tex_accent_char(text: &str, at: usize) -> Option<(char, usize)> {
    let after = at.checked_add(1)?;
    let cmd = *text.as_bytes().get(after)?;
    let rest = text.get(after + 1..)?;
    if matches!(cmd, b'\'' | b'`' | b'^' | b'"' | b'~' | b'=' | b'.') {
        if let Some(braced) = rest.strip_prefix('{') {
            if let Some(tail) = braced
                .strip_prefix("\\i}")
                .or_else(|| braced.strip_prefix("i}"))
            {
                return Some(('i', text.len() - tail.len()));
            }
            if let Some(tail) = braced
                .strip_prefix("\\j}")
                .or_else(|| braced.strip_prefix("j}"))
            {
                return Some(('j', text.len() - tail.len()));
            }
            let ch = braced.chars().next().filter(|ch| ch.is_alphabetic())?;
            let tail = braced.get(ch.len_utf8()..)?.strip_prefix('}')?;
            return Some((ch, text.len() - tail.len()));
        }
        if let Some(tail) = rest.strip_prefix("\\i")
            && !tail
                .as_bytes()
                .first()
                .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'@')
        {
            return Some(('i', text.len() - tail.len()));
        }
        if let Some(tail) = rest.strip_prefix("\\j")
            && !tail
                .as_bytes()
                .first()
                .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'@')
        {
            return Some(('j', text.len() - tail.len()));
        }
        let ch = rest.chars().next().filter(|ch| ch.is_alphabetic())?;
        return Some((ch, after + 1 + ch.len_utf8()));
    }
    if matches!(
        cmd,
        b'u' | b'v' | b'H' | b'c' | b'k' | b'r' | b'b' | b'd' | b't'
    ) && let Some(braced) = rest.strip_prefix('{')
    {
        if let Some(tail) = braced.strip_prefix("\\i}") {
            return Some(('i', text.len() - tail.len()));
        }
        if let Some(tail) = braced.strip_prefix("\\j}") {
            return Some(('j', text.len() - tail.len()));
        }
        let ch = braced.chars().next().filter(|ch| ch.is_alphabetic())?;
        let tail = braced.get(ch.len_utf8()..)?.strip_prefix('}')?;
        return Some((ch, text.len() - tail.len()));
    }
    None
}

fn tex_accent_at(text: &str, index: usize) -> Option<(char, usize)> {
    if index > 0 && text.as_bytes()[index - 1] == b'\\' {
        return None;
    }
    match *text.as_bytes().get(index)? {
        b'\\' => tex_accent_char(text, index),
        b'{' => {
            let (ch, next) = tex_accent_char(text, index + 1)?;
            (text.as_bytes().get(next) == Some(&b'}')).then_some((ch, next + 1))
        }
        _ => None,
    }
}

fn source_words(text: &str) -> Vec<(usize, usize, String)> {
    let mut result = Vec::new();
    let mut start = None;
    let mut word = String::new();
    let mut index = 0;
    while index < text.len() {
        if let Some((ch, next)) = tex_accent_at(text, index) {
            start.get_or_insert(index);
            word.push(ch);
            index = next;
            continue;
        }
        let ch = text[index..].chars().next().unwrap();
        if ch.is_alphanumeric() || start.is_some() && is_combining_mark(ch) {
            start.get_or_insert(index);
            word.push(ch);
        } else if let Some(start) = start.take() {
            result.push((start, index, normalized_word(&word)));
            word.clear();
        }
        index += ch.len_utf8();
    }
    if let Some(start) = start {
        result.push((start, text.len(), normalized_word(&word)));
    }
    result
}

fn normalized_word(word: &str) -> String {
    if word.is_ascii() {
        return word.to_ascii_lowercase();
    }
    word.nfkd()
        .filter(|ch| !is_combining_mark(*ch))
        .flat_map(char::to_lowercase)
        .collect()
}

fn ordinary_word_source_start(
    source: &str,
    context: &str,
    offset: usize,
    line: u32,
    byte: usize,
) -> Option<usize> {
    let pdf = words(context);
    let Some(selected) = word_index_at(context, &pdf, offset) else {
        return Some(byte);
    };
    let pdf_word = normalized_word(pdf[selected].1);
    if pdf_word.len() < 2 || !pdf_word.bytes().all(|ch| ch.is_ascii_alphabetic()) {
        return Some(byte);
    }
    source
        .lines()
        .nth(line as usize - 1)
        .and_then(|text| {
            source_words(source_line_text(text))
                .into_iter()
                .find(|(start, end, _)| *start <= byte && byte < *end)
        })
        .and_then(|(start, _, word)| (word == pdf_word).then_some(start))
}

/// Refine prose and mathematical atoms without expanding arbitrary TeX macros.
fn source_word_location(
    source: &str,
    line: u32,
    context: &str,
    offset: usize,
    radius: u32,
) -> Option<(u32, usize)> {
    let scope = math::caption_lines(source, line).or_else(|| source_frame_range(source, line));
    let within_scope = scope.is_some();
    let lines = scope.unwrap_or_else(|| {
        line.saturating_sub(radius.saturating_add(1)) as usize
            ..(line as usize).saturating_add(radius as usize)
    });
    source_word_location_in_range(source, line, context, offset, lines, within_scope)
}

fn source_word_location_in_range(
    source: &str,
    line: u32,
    context: &str,
    offset: usize,
    lines: std::ops::Range<usize>,
    within_scope: bool,
) -> Option<(u32, usize)> {
    let hidden = nonprinting_source_ranges(source);
    // A closing layout environment can alias every row to one source line.
    // A search radius alone does not identify neighboring prose or math;
    // independently established frame/caption and math scopes remain eligible.
    let closing_layout = source
        .lines()
        .nth(line.saturating_sub(1) as usize)
        .map(source_line_text)
        .and_then(|text| text.trim_start().strip_prefix("\\end"))
        .and_then(|rest| {
            let rest = rest.trim_start();
            math::group(rest, 0).map(|(name, _)| !math::math_environment(&rest[name]))
        })
        .unwrap_or(false);
    let lines = if !within_scope && closing_layout {
        line.saturating_sub(1) as usize..line as usize
    } else {
        lines
    };
    let prose = source_prose_scored_location(
        source,
        line,
        context,
        offset,
        lines.clone(),
        within_scope,
        &hidden,
    );
    math::source_location(source, line, lines, within_scope, context, offset, prose).and_then(
        |(row, byte)| {
            if !hidden.is_empty() {
                let absolute = source
                    .split_inclusive('\n')
                    .take(row as usize - 1)
                    .map(str::len)
                    .sum::<usize>()
                    + byte;
                if hidden.iter().any(|range| range.contains(&absolute)) {
                    return None;
                }
            }
            ordinary_word_source_start(source, context, offset, row, byte).map(|start| (row, start))
        },
    )
}

fn skip_source_space(source: &str, mut at: usize) -> usize {
    loop {
        at += source[at..].len() - source[at..].trim_start().len();
        if source.as_bytes().get(at) != Some(&b'%') {
            return at;
        }
        at += source[at..].find('\n').unwrap_or(source.len() - at);
    }
}

/// TeX optional arguments end at the first unbraced, unescaped closing bracket.
fn optional_argument_end(source: &str, start: usize) -> Option<usize> {
    let start = skip_source_space(source, start);
    if source.as_bytes().get(start) != Some(&b'[') {
        return None;
    }
    let mut at = start + 1;
    while at < source.len() {
        match source.as_bytes()[at] {
            b'\\' => {
                at += 1;
                at += source[at..].chars().next().map_or(0, char::len_utf8);
                continue;
            }
            b'%' => {
                at += source[at..].find('\n').unwrap_or(source.len() - at);
                continue;
            }
            b'{' => {
                at = math::group(source, at)?.1;
                continue;
            }
            b']' => return Some(at + 1),
            _ => {}
        }
        at += source[at..].chars().next()?.len_utf8();
    }
    None
}

fn overlay_argument_end(source: &str, start: usize) -> Option<usize> {
    if source.as_bytes().get(start) != Some(&b'<') {
        return None;
    }
    let rest = &source[start + 1..];
    let end = rest.find(['>', '\n'])?;
    if rest.as_bytes()[end] != b'>' {
        return None;
    }
    let body = &rest[..end];
    if body.is_empty() {
        return None;
    }
    let has_action = body.contains([':', '@']);
    body.bytes()
        .all(|ch| {
            ch.is_ascii_digit()
                || matches!(
                    ch,
                    b'+' | b'-' | b',' | b'.' | b'(' | b')' | b'|' | b'*' | b' '
                )
                || has_action && (ch.is_ascii_alphabetic() || matches!(ch, b':' | b'@'))
        })
        .then_some(start + 1 + end + 1)
}

/// Only known nonprinting arguments are masked. This is not macro expansion;
/// ordinary formatting/caption arguments and href display text remain literal.
fn nonprinting_source_ranges(source: &str) -> Vec<std::ops::Range<usize>> {
    nonprinting_source_spans(source)
        .map(|(_, body)| body)
        .collect()
}

fn nonprinting_source_spans(
    source: &str,
) -> impl Iterator<Item = (usize, std::ops::Range<usize>)> + '_ {
    let mut at = 0;
    std::iter::from_fn(move || {
        loop {
            if at >= source.len() {
                return None;
            }
            if source.as_bytes()[at] == b'%' {
                at += source[at..].find('\n').unwrap_or(source.len() - at);
            } else if source.as_bytes()[at] == b'<'
                && at > 0
                && matches!(source.as_bytes()[at - 1], b'}' | b']')
            {
                let start = at;
                if let Some(end) = overlay_argument_end(source, at) {
                    at = end;
                    return Some((start, start..end));
                }
                at += 1;
            } else if source.as_bytes()[at] == b'\\' {
                let start = at;
                at += 1;
                let end = at
                    + source[at..]
                        .bytes()
                        .take_while(|ch| ch.is_ascii_alphabetic() || *ch == b'@')
                        .count();
                if end == at {
                    at += source[at..].chars().next().map_or(0, char::len_utf8);
                    continue;
                }
                let command = &source[at..end];
                if let Some((end, nonprinting)) = math::opaque_command_end(source, command, end) {
                    at = end;
                    if nonprinting {
                        return Some((start, start..end));
                    }
                    continue;
                }
                if matches!(command, "let" | "futurelet") {
                    at = skip_source_space(source, end);
                    if source.as_bytes().get(at) == Some(&b'\\') {
                        at = math::command(source, at).1;
                    } else if at < source.len() {
                        at += source[at..].chars().next().map_or(1, char::len_utf8);
                    }
                    at = skip_source_space(source, at);
                    if command == "let" && source.as_bytes().get(at) == Some(&b'=') {
                        at = skip_source_space(source, at + 1);
                    }
                    if source.as_bytes().get(at) == Some(&b'\\') {
                        at = math::command(source, at).1;
                    } else if at < source.len() {
                        at += source[at..].chars().next().map_or(1, char::len_utf8);
                    }
                    return Some((start, start..at));
                }
                at = end + usize::from(matches!(source.as_bytes().get(end), Some(b'*' | b'+')));
                let definition = matches!(
                    command,
                    "newcommand"
                        | "renewcommand"
                        | "providecommand"
                        | "DeclareRobustCommand"
                        | "NewDocumentCommand"
                        | "RenewDocumentCommand"
                        | "ProvideDocumentCommand"
                        | "DeclareDocumentCommand"
                        | "newenvironment"
                        | "renewenvironment"
                        | "def"
                        | "gdef"
                        | "edef"
                        | "xdef"
                );
                if definition {
                    if matches!(command, "def" | "gdef" | "edef" | "xdef") {
                        while at < source.len() && source.as_bytes()[at] != b'{' {
                            if source.as_bytes()[at] == b'\\' {
                                at += 1;
                                at += source[at..].chars().next().map_or(0, char::len_utf8);
                            } else if source.as_bytes()[at] == b'%' {
                                at = skip_source_space(source, at);
                            } else {
                                at += source[at..].chars().next().unwrap().len_utf8();
                            }
                        }
                    } else {
                        at = skip_source_space(source, at);
                        if let Some((_, end)) = math::group(source, at) {
                            at = end;
                        } else if source.as_bytes().get(at) == Some(&b'\\') {
                            at = math::command(source, at).1;
                        }
                        while let Some(end) = optional_argument_end(source, at) {
                            at = end;
                        }
                        if command.contains("Document") {
                            let Some((_, end)) = math::group(source, skip_source_space(source, at))
                            else {
                                continue;
                            };
                            at = end;
                        }
                    }
                    let body_start = skip_source_space(source, at);
                    let Some((_, end)) = math::group(source, body_start) else {
                        if source.as_bytes().get(body_start) == Some(&b'{') {
                            let end = source.len();
                            at = end;
                            return Some((start, start..end));
                        }
                        at = body_start;
                        continue;
                    };
                    at = end;
                    if command.ends_with("environment")
                        && let Some((_, end)) = math::group(source, skip_source_space(source, at))
                    {
                        at = end;
                    }
                    return Some((start, start..at));
                } else if matches!(
                    command,
                    "label"
                        | "ref"
                        | "eqref"
                        | "pageref"
                        | "autoref"
                        | "cref"
                        | "Cref"
                        | "cite"
                        | "citep"
                        | "citet"
                        | "parencite"
                        | "textcite"
                        | "autocite"
                        | "footcite"
                        | "nocite"
                        | "href"
                        | "includegraphics"
                        | "includesvg"
                        | "input"
                        | "include"
                        | "bibliography"
                        | "addbibresource"
                ) {
                    while let Some(end) = optional_argument_end(source, at)
                        .or_else(|| overlay_argument_end(source, at))
                    {
                        at = end;
                    }
                    if let Some((body, end)) = math::group(source, skip_source_space(source, at)) {
                        at = end;
                        return Some((
                            start,
                            if matches!(command, "includegraphics" | "includesvg") {
                                start..end
                            } else {
                                body
                            },
                        ));
                    }
                } else if let Some(overlay_end) = overlay_argument_end(source, at) {
                    at = overlay_end;
                    return Some((start, end..overlay_end));
                }
            } else {
                at += source[at..].chars().next().unwrap().len_utf8();
            }
        }
    })
}

fn source_prose_scored_location(
    source: &str,
    line: u32,
    context: &str,
    offset: usize,
    lines: std::ops::Range<usize>,
    within_frame: bool,
    hidden: &[std::ops::Range<usize>],
) -> Option<(u32, usize, isize)> {
    let pdf = words(context);
    let selected = word_index_at(context, &pdf, offset)?;
    let pdf: Vec<_> = pdf.iter().map(|(_, word)| normalized_word(word)).collect();
    let frame_header = within_frame
        && source
            .lines()
            .nth(lines.start)
            .is_some_and(|row| source_line_text(row).contains("\\begin{frame}"));
    let mut candidates = Vec::new();
    let mut line_start = 0;
    for (index, raw_row) in source.split_inclusive('\n').enumerate().take(lines.end) {
        let text = source_line_text(raw_row.trim_end_matches(['\r', '\n']));
        if index >= lines.start {
            for (byte, _, word) in source_words(text) {
                if (byte == 0 || !text[..byte].ends_with('\\'))
                    && !hidden
                        .iter()
                        .any(|range| range.contains(&(line_start + byte)))
                {
                    candidates.push((index as u32 + 1, byte, word));
                }
            }
        }
        line_start += raw_row.len();
    }
    let mut best = None;
    let mut tied = false;
    for (index, (row, byte, word)) in candidates.iter().enumerate() {
        if *word != pdf[selected] {
            continue;
        }
        let mut score = 0;
        for distance in 1..=3 {
            for direction in [-1isize, 1] {
                let delta = distance * direction;
                if let (Some(a), Some(b)) = (
                    selected.checked_add_signed(delta).and_then(|i| pdf.get(i)),
                    index
                        .checked_add_signed(delta)
                        .and_then(|i| candidates.get(i)),
                ) && *a == b.2
                {
                    score += 4 - distance;
                }
            }
        }
        // TeX math can insert source tokens that PDF text extraction omits.
        // Use nearby longer words as a tie break for repeated prose.
        let mut context_bonus = 0;
        for direction in [-1isize, 1] {
            for distance in 1..=8 {
                let Some(neighbor) = selected
                    .checked_add_signed(direction * distance)
                    .and_then(|i| pdf.get(i))
                else {
                    break;
                };
                if neighbor.chars().count() < 4 {
                    continue;
                }
                if (1..=10).any(|step| {
                    index
                        .checked_add_signed(direction * step)
                        .and_then(|i| candidates.get(i))
                        .is_some_and(|candidate| candidate.2 == *neighbor)
                }) {
                    context_bonus += 1;
                }
            }
        }
        // A collected frame/caption boundary does not favor its last occurrence.
        let proximity = if within_frame { 0 } else { row.abs_diff(line) };
        let rank = (
            score,
            frame_header && *row as usize == lines.start + 1,
            context_bonus,
            std::cmp::Reverse(proximity),
        );
        match best {
            Some((old, _, _)) if rank < old => {}
            Some((old, _, _)) if rank == old => tied = true,
            _ => {
                best = Some((rank, *row, *byte));
                tied = false;
            }
        }
    }
    best.filter(|_| !tied)
        .map(|(rank, row, byte)| (row, byte, rank.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caption_review_embedded_comment_end_does_not_expose_hidden_caption() {
        let source = "\\begin{comment}\nignored \\end{comment}\n\
                      \\caption{hidden\ncaption}\n\\end{comment}";
        assert_eq!(caption_closing_line(source, 3, 10), None);
    }

    #[test]
    fn caption_review_comment_keys_cannot_become_inverse_word_targets() {
        let source = "\\newcommand{\\Printed}{Zephyr}\n\
                      \\begin{comment}\n\
                      \\label{Zephyr}\n\
                      \\end{comment}\n\
                      \\Printed";
        assert_eq!(source_word_location(source, 5, "Zephyr", 0, 4), None);
    }

    #[test]
    fn caption_review_unbraced_short_brackets_do_not_own_adjacent_prose() {
        let source = "\\caption[short[inner]]{first\nsecond}";
        let column = source.find("first").unwrap() as u32 + 1;
        assert_eq!(caption_closing_line(source, 1, column), None);
        assert_eq!(math::caption_lines(source, 2), None);
        let source = "\\caption[{short[inner]}]{first\nsecond}";
        let column = source.find("first").unwrap() as u32 + 1;
        assert_eq!(caption_closing_line(source, 1, column), Some(2));
        assert_eq!(math::caption_lines(source, 2), Some(0..2));
    }

    #[test]
    fn optional_arguments_shield_braces_comments_and_control_symbols() {
        let source = "[short[inner]] trailing";
        assert_eq!(
            optional_argument_end(source, 0),
            Some(source.find(']').unwrap() + 1)
        );
        for source in [
            "[{short[inner]}]",
            "[% ignored ]\n{brace]} \\] tail]",
            "[escaped \\{ brace]",
        ] {
            assert_eq!(optional_argument_end(source, 0), Some(source.len()));
        }
        assert_eq!(optional_argument_end("[{unclosed]", 0), None);
        assert!(
            nonprinting_source_ranges("\\begin{verbatim}\n\\label{printed}\n\\end{verbatim}")
                .is_empty()
        );
        let source = "\\begin{comment}\n\\label{hidden}\n\\end{comment}";
        assert_eq!(nonprinting_source_ranges(source), vec![0..source.len()]);
    }

    #[test]
    fn comment_closure_requires_a_complete_subsequent_input_line() {
        for fake in [
            "ignored \\end{comment}",
            "\\end{comment} ignored",
            " \\end{comment}",
        ] {
            for newline in ["\n", "\r\n"] {
                let source = format!(
                    "\\begin{{comment}}{newline}{fake}{newline}\\caption{{hidden{newline}caption}}{newline}\\end{{comment}}{newline}\\caption{{visible{newline}caption}}"
                );
                assert_eq!(caption_closing_line(&source, 3, 10), None);
                assert_eq!(caption_closing_line(&source, 6, 10), Some(7));
                let unclosed = format!(
                    "\\begin{{comment}}{newline}{fake}{newline}\\caption{{hidden{newline}caption}}"
                );
                assert_eq!(caption_closing_line(&unclosed, 3, 10), None);
            }
        }
        for newline in ["\n", "\r\n"] {
            let source = format!(
                "\\begin{{comment}}\\end{{comment}}{newline}\\caption{{hidden{newline}caption}}{newline}\\end{{comment}}   {newline}\\caption{{visible{newline}caption}}"
            );
            assert_eq!(caption_closing_line(&source, 2, 10), None);
            assert_eq!(caption_closing_line(&source, 5, 10), Some(6));
        }
    }

    #[test]
    fn forward_caption_anchor_uses_only_complete_long_argument() {
        for caption in [
            "\\caption",
            "\\caption*",
            "\\caption[short {nested}]",
            "\\caption*[short]",
        ] {
            let source = format!(
                "before\n{caption}% argument follows\n{{école first\nsecond \\emph{{literal}}\n}}\nafter"
            );
            assert_eq!(caption_closing_line(&source, 3, 2), Some(5));
            assert_eq!(caption_closing_line(&source, 4, 1), Some(5));
            for (line, column) in [(1, 1), (2, 1), (3, 13), (5, 1), (6, 1), (0, 1), (3, 0)] {
                assert_eq!(caption_closing_line(&source, line, column), None);
            }
        }
        for source in [
            "\\caption[short]{unclosed\nbody",
            "\\caption[unclosed{body}",
            "\\caption{body\n} trailing",
        ] {
            let last = source.lines().count() as u32;
            let column = source.lines().last().unwrap().chars().count() as u32;
            assert_eq!(caption_closing_line(source, last, column), None);
        }
    }

    #[test]
    fn forward_caption_anchor_excludes_nonprinting_and_opaque_source() {
        let source = "\\newcommand{\\Hidden}{\\caption{definition}}\n\
                      \\def\\Other{\\caption{primitive}}\n\
                      \\verb|\\caption{inline}|\n\
                      \\begin{verbatim}\n\\caption{verbatim}\n\\end{verbatim}\n\
                      \\caption[short]{visible \\label{key} % comment\n\
                      \\verb|opaque| \\newcommand{\\Inner}{nested} final}\n\
                      % \\caption{commented}\n\
                      \\label{\\caption{label}}\n";
        for word in [
            "definition",
            "primitive",
            "inline",
            "verbatim}",
            "short",
            "key",
            "comment",
            "opaque",
            "nested",
            "commented",
            "label",
        ] {
            let at = source.find(word).unwrap();
            let line = source[..at].bytes().filter(|ch| *ch == b'\n').count() as u32 + 1;
            let start = source[..at].rfind('\n').map_or(0, |end| end + 1);
            let column = source[start..at].chars().count() as u32 + 1;
            assert_eq!(caption_closing_line(source, line, column), None, "{word}");
        }
        assert_eq!(caption_closing_line(source, 7, 17), Some(8));
        let source = "\\begin{frame}\n\\caption{first\nsecond}\n\\end{frame}";
        assert_eq!(caption_closing_line(source, 2, 10), Some(3));
        assert_eq!(
            frame_closing_line(source, 2).or_else(|| caption_closing_line(source, 2, 10)),
            Some(4),
        );
    }

    #[test]
    fn explicit_inverse_guess_preserves_compiled_coordinates_and_warns_on_ambiguity() {
        let root = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(root.path()).unwrap();
        let source = root.join("edited.tex");
        fs::write(&source, "only one current line\n").unwrap();
        let pdf = root.join("paper.pdf");
        let point = InversePoint {
            page: 1,
            x: 10.0,
            y_from_top: 20.0,
            page_height_pt: 100.0,
        };
        let output = format!(
            "Input:{}\nLine:9\nInput:{}\nLine:12\n",
            source.display(),
            source.display()
        );
        let guessed = resolve_inverse_output(
            &pdf,
            point,
            Some(("current", 0)),
            4,
            &Operation::default(),
            &output,
            QuerySources {
                best_guess: true,
                ..QuerySources::default()
            },
        )
        .unwrap();
        assert_eq!(guessed.location.line, 9);
        assert_eq!(
            (
                guessed.location.byte_column,
                guessed.location.column,
                guessed.location.column_char
            ),
            (0, 1, 1)
        );
        assert!(!guessed.location.precise);
        assert!(guessed.warning.unwrap().contains("multiple coarse"));
        assert!(
            resolve_inverse_output(
                &pdf,
                point,
                None,
                4,
                &Operation::default(),
                &output,
                QuerySources::default(),
            )
            .is_err()
        );
        fs::write(&source, "current source line\n".repeat(12)).unwrap();
        assert!(
            resolve_inverse_output(
                &pdf,
                point,
                None,
                4,
                &Operation::default(),
                &output,
                QuerySources::default(),
            )
            .err()
            .unwrap()
            .to_string()
            .contains("multiple coarse")
        );
        let operation = Operation::default();
        operation.cancel();
        assert!(
            resolve_inverse_output(
                &pdf,
                point,
                None,
                4,
                &operation,
                &output,
                QuerySources {
                    best_guess: true,
                    ..QuerySources::default()
                },
            )
            .is_err()
        );
    }

    #[test]
    fn pdf_revision_rejects_rewrites_and_atomic_replacements() {
        let root = tempfile::tempdir().unwrap();
        let pdf = root.path().join("paper.pdf");
        fs::write(&pdf, b"first").unwrap();
        let first = PdfRevision::read(&pdf).unwrap();
        first.check(&pdf).unwrap();
        fs::write(&pdf, b"second").unwrap();
        assert!(first.check(&pdf).is_err());
        let second = PdfRevision::read(&pdf).unwrap();
        let replacement = root.path().join("replacement.pdf");
        fs::write(&replacement, b"second").unwrap();
        fs::rename(replacement, &pdf).unwrap();
        assert!(second.check(&pdf).is_err());
    }
    #[test]
    fn inverse_word_matching_resolves_context_and_utf8_byte_columns() {
        let source = "A repeated word far away.\nÉlie uses \\emph{repeated} maps near fibers.\nRepeated noise.\n";
        let context = "Élie uses repeated maps near ﬁbers.";
        assert_eq!(
            super::source_word_location(source, 2, context, context.find("repeated").unwrap(), 4),
            Some((2, source.lines().nth(1).unwrap().find("repeated").unwrap()))
        );
        assert_eq!(
            super::source_word_location(source, 3, context, context.find("ﬁbers").unwrap(), 4),
            Some((2, source.lines().nth(1).unwrap().find("fibers").unwrap()))
        );
        assert_eq!(
            super::source_word_location("word word", 1, "word", 1, 4),
            None
        );
        assert_eq!(
            super::source_word_location("\\word % word", 1, "word", 1, 4),
            None
        );
        assert_eq!(
            super::source_word_location("word", 1, "missing", 1, 4),
            None
        );
        let boundary = "one\ntwo\nthree\nfour\nfive\nsix";
        assert_eq!(
            super::source_word_location(boundary, 1, "five", 1, 4),
            Some((5, 0))
        );
        assert_eq!(super::source_word_location(boundary, 1, "six", 1, 4), None);
    }

    #[test]
    fn inverse_frame_matching_reaches_prose_without_crossing_frames() {
        let source = "\\begin{frame}\nSome datasets convey geometry.\n\\pause\n\
                      \\includegraphics{datasets/image.pdf}\n\n\n\n\n\
                      \\only<2>{Other visible text.}\n\\end{frame}\n\
                      \\begin{frame}\nSome datasets convey geometry.\n\\end{frame}";
        assert_eq!(
            source_word_location(source, 10, "Some datasets convey geometry.", 5, 4),
            Some((2, 5))
        );
        assert_eq!(
            source_word_location(source, 13, "Some datasets convey geometry.", 5, 4),
            Some((12, 5))
        );
        assert_eq!(
            source_word_location(source, 10, "datasets", 1, 4),
            Some((2, 5))
        );
        assert_eq!(source_word_location(source, 13, "Other", 1, 4), None);
    }

    #[test]
    fn inverse_frame_matching_keeps_duplicate_overlays_line_only() {
        let source = "\\begin{frame}\n\\only<1>{Identical visible phrase.}\n\
                      \\only<2>{Identical visible phrase.}\n\\end{frame}";
        assert_eq!(
            source_word_location(source, 4, "Identical visible phrase.", 10, 4),
            None
        );
    }

    #[test]
    fn inverse_title_word_survives_unrelated_opaque_math() {
        let source = "\\begin{frame}[fragile]{Let's revisit this example}\n\
                      Ordinary body words:\n\
                      \\begin{equation*}\n\
                      \\custom{x}\n\
                      \\end{equation*}\n\
                      \\end{frame}";
        let context = "Let’s revisit this example\r\nOrdinary body words:";
        assert_eq!(
            source_prose_scored_location(
                source,
                6,
                context,
                0,
                0..6,
                true,
                &nonprinting_source_ranges(source)
            )
            .map(|(row, byte, _)| (row, byte)),
            Some((1, 23))
        );
        assert_eq!(
            source_word_location(source, 6, context, 0, 4),
            Some((1, 23))
        );
    }

    #[test]
    fn inverse_title_words_prefer_the_frame_header_when_body_repeats_them() {
        let source = "\\begin{frame}{Overview}\n\
                      An overview follows in the body.\n\
                      $\\custom{x}$\n\
                      \\end{frame}";
        assert_eq!(
            source_word_location(source, 4, "Overview\r\n0.0 0.2", 6, 4),
            Some((1, 14))
        );
        assert_eq!(
            source_word_location(source, 4, "An overview follows", 3, 4),
            Some((2, 3))
        );
        let source = "\\begin{frame}{Primary Topic}{A small example}\n\
                      A different body sentence.\n\
                      $\\custom{x}$\n\
                      \\end{frame}";
        let context = "Primary Topic\r\nA small example\r\nTheorem";
        assert_eq!(
            source_word_location(source, 4, context, context.find("A small").unwrap(), 4),
            Some((
                1,
                source.lines().next().unwrap().find("{A small").unwrap() + 1
            ))
        );
    }

    #[test]
    fn inverse_heading_matches_tex_accent_and_pdf_unicode() {
        let source = "\\begin{frame}{A note on \\v Zora's method}\n\
                      $x$\n\
                      \\end{frame}";
        let context = "A note on Žora’s method";
        assert_eq!(
            source_word_location(source, 3, context, context.find('Ž').unwrap(), 4),
            Some((1, source.lines().next().unwrap().find("Zora").unwrap()))
        );
    }

    #[test]
    fn inverse_literal_words_inside_boxed_math_keep_their_source_spans() {
        let source = "\\begin{frame}{Example}\n\
                      \\[\\boxed{\\textnormal{Visible sentence inside the box.}}\\]\n\
                      \\[\\begin{aligned}N\\to\\infty\\end{aligned}\\]\n\
                      \\end{frame}";
        let context = "Example\r\nVisible sentence inside the box.";
        assert_eq!(
            source_word_location(source, 4, context, context.find("Visible").unwrap(), 4),
            Some((2, source.lines().nth(1).unwrap().find("Visible").unwrap()))
        );
    }

    #[test]
    fn inverse_prose_word_does_not_match_inside_longer_math_word() {
        let source = "\\begin{frame}{Example}\n\
                      Pick an option here.\n\
                      $\\mathrm{constant}(x)$ $\\custom{q}$\n\
                      \\end{frame}";
        let context = "Pick an option here. constant(x)";
        assert_eq!(
            source_word_location(source, 4, context, context.find("an option").unwrap(), 4),
            Some((2, 5))
        );
    }

    #[test]
    fn inverse_caption_word_ignores_image_filename() {
        let source = "\\begin{frame}{Example}\n\
                      \\includegraphics[width=\\textwidth]{assets/sample_green_blue_shapes--[Maker].jpg}\n\
                      \\caption{Sample green blue shapes, Maker.}\n\
                      \\end{frame}";
        let context = "Figure 1: Sample green blue shapes, Maker.";
        assert_eq!(
            source_word_location(source, 4, context, context.find("green").unwrap(), 4),
            Some((3, source.lines().nth(2).unwrap().find("green").unwrap()))
        );
        assert_eq!(source_word_location(source, 4, "assets", 0, 4), None);
    }

    #[test]
    fn inverse_prose_excludes_nonprinting_keys_and_multiline_definitions() {
        let source = "\\newcommand{\\Hidden}{\nZephyr\n}\n\
                      \\Hidden\\label% keep key on the next line\n{Zephyr}\n\
                      \\cite[PrintedNote]{Zephyr}\\ref{Zephyr}\\href{Zephyr}{DisplayAmber}\n\
                      \\newcommand{\\Unused}{Zephyr} éé PrintedCopper\n";
        assert_eq!(source_word_location(source, 5, "Zephyr", 0, 10), None);
        for (word, row) in [
            ("PrintedNote", 6),
            ("DisplayAmber", 6),
            ("PrintedCopper", 7),
        ] {
            let byte = source.lines().nth(row - 1).unwrap().find(word).unwrap();
            assert_eq!(
                source_word_location(source, row as u32, word, 0, 10),
                Some((row as u32, byte))
            );
        }
        let source = "\\def\\Hidden#1{Zephyr {NestedKey}} PrintedCopper\n";
        for word in ["Zephyr", "NestedKey"] {
            assert_eq!(source_word_location(source, 1, word, 0, 4), None);
        }
        assert_eq!(
            source_word_location(source, 1, "PrintedCopper", 0, 4),
            Some((1, source.find("PrintedCopper").unwrap()))
        );
        let source = "\\newcommand{\\Hidden}{$Zephyr$}\n\\Hidden\\label{Zephyr}\n";
        assert_eq!(source_word_location(source, 2, "Zephyr", 0, 4), None);
    }

    #[test]
    fn inverse_repeated_prose_uses_words_past_adjacent_math() {
        let source = "\\begin{frame}{Example}\n\
                      \\item the \\blue{$X_i$}s are random variables in $\\mathcal{X}$, a space of inputs,\n\
                      \\item the \\blue{$Y_i$}s are random variables in $\\mathcal{Y}$, a space of labels,\n\
                      \\end{frame}";
        let context = "the Xis are random variables in X, a space of inputs,\r\n\
                       the Yis are random variables in Y, a space of labels,";
        for (needle, line) in [("inputs", 2), ("labels", 3)] {
            let at = context.find(needle).unwrap();
            let selected = context[..at].rfind("variables").unwrap();
            assert_eq!(
                source_word_location(source, 4, context, selected, 4),
                Some((
                    line,
                    source
                        .lines()
                        .nth(line as usize - 1)
                        .unwrap()
                        .find("variables")
                        .unwrap()
                ))
            );
        }
    }

    #[test]
    fn inverse_prose_context_beats_unrelated_math_word() {
        let source = "\\begin{frame}{Example}\n\
                      The map defines an \\emph{order} $(a_1, a_2)$ on a set.\n\
                      \\begin{equation*}\\mathrm{order}(f)=\\mathrm{order}(g)\\end{equation*}\n\
                      \\end{frame}";
        let context = "The map defines an order (a1, a2) on a set.";
        assert_eq!(
            source_word_location(source, 4, context, context.find("order").unwrap(), 4),
            Some((2, source.lines().nth(1).unwrap().find("order").unwrap()))
        );
    }

    #[test]
    fn inverse_repeated_word_uses_short_context_across_math() {
        let source = "\\begin{frame}{Example}\n\
                      It is smooth on $\\mathrm{cell}(f)$ as soon\n\
                      as $\\mathrm{t}(g)$ is smooth.\n\
                      \\end{frame}";
        let context = "It is smooth on cell(f) as soon\r\nas t(g) is smooth.";
        let selected = context.rfind("smooth").unwrap();
        assert_eq!(
            source_word_location(source, 4, context, selected, 4),
            Some((3, source.lines().nth(2).unwrap().find("smooth").unwrap()))
        );
    }

    #[test]
    fn inverse_ordinary_word_rejects_a_different_source_word() {
        let source = "The map is smooth and has a finite image.";
        let byte = source.find("smooth").unwrap();
        assert_eq!(
            ordinary_word_source_start(
                source,
                "The map is regular and has a finite image.",
                "The map is ".len(),
                1,
                byte,
            ),
            None
        );
        assert_eq!(
            ordinary_word_source_start(
                source,
                "The map is smooth and has a finite image.",
                "The map is ".len(),
                1,
                byte + 2,
            ),
            Some(byte)
        );
    }

    #[test]
    fn inverse_generated_verbatim_uses_clicked_sheet_original_frame() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("original.tex");
        let source = "\\begin {frame}\n[fragile]{First}\nEarlyZephyr.\nFirstCopper.\n\\end{frame}\n\
                      \\begin{frame}[fragile]{Second}\nLateNebula.\nSecondSilver.\n\\end{frame}\n";
        fs::write(&source_path, source).unwrap();
        let pdf = directory.path().join("build/deck.pdf");
        let generated = directory.path().join("build/deck.vrb");
        let contents = format!(
            "SyncTeX Version:1\nInput:1:{}\nInput:2:{}\n\
             Magnification:1000\nUnit:1\nX Offset:0\nY Offset:0\nContent:\n\
             {{1\n[1,5:0,0:1,1,0\nx2,2:0,0\n]\n}}1\n\
             Input:3:{}\n{{2\n[1,9:0,0:1,1,0\nx3,2:0,0\n]\n}}2\n\
             Postamble:\nCount:8\nPost scriptum:\n",
            source_path.display(),
            generated.display(),
            generated.display()
        );
        for (page, anchor) in [(1, 5), (2, 9)] {
            let map = source_map_from_reader(
                contents.as_bytes(),
                &pdf,
                InversePoint {
                    page,
                    x: 0.0,
                    y_from_top: 0.0,
                    page_height_pt: 1.0,
                },
                &generated,
                false,
                &Operation::default(),
                None,
            )
            .unwrap();
            let remapped = original_source_from_verbatim(
                &map,
                &mut QuerySources::default(),
                &Operation::default(),
            )
            .unwrap();
            assert_eq!(
                remapped.0,
                fs::canonicalize(&source_path).unwrap().to_str().unwrap()
            );
            assert_eq!(remapped.1.as_str(), source);
            assert_eq!(remapped.2, anchor);
        }
        let missing = source_map_from_reader(
            contents.as_bytes(),
            &pdf,
            InversePoint {
                page: 1,
                x: 0.0,
                y_from_top: 0.0,
                page_height_pt: 1.0,
            },
            Path::new("/unrelated.vrb"),
            false,
            &Operation::default(),
            None,
        )
        .unwrap();
        assert!(
            original_source_from_verbatim(
                &missing,
                &mut QuerySources::default(),
                &Operation::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn inverse_body_ownership_uses_specific_boxes_and_effective_transform() {
        let sheet = "SyncTeX Version:1\nInput:1:/source.tex\nInput:2:/deck.vrb\n\
                     Magnification:1000\nUnit:65536\nX Offset:0\nY Offset:0\nContent:\n\
                     {2\n[1,18:0,100:100,100,0\n\
                     (1,18:0,100:100,100,0\n(1,18:0,100:100,100,0\n)\n)\n\
                     (1,18:10,20:30,10,0\nx2,2:20,20\n)\n\
                     (1,18:70,90:20,10,0\nx1,18:80,90\n)\n\
                     ]\n}2\nPostamble:\nCount:12\nPost scriptum:\n";
        for (has_post_scriptum, post, magnification, x_offset, y_offset) in [
            (true, "", 1.0, 0.0, 0.0),
            (false, "", 1.0, 0.0, 0.0),
            (
                true,
                "Magnification:2\nX Offset:10bp\nY Offset:20bp\n",
                2.0,
                10.0,
                20.0,
            ),
        ] {
            let header = if has_post_scriptum {
                sheet
            } else {
                sheet.strip_suffix("Post scriptum:\n").unwrap()
            };
            let contents = format!("{header}{post}");
            let compressed = contents
                .replacen(
                    "{2\n[1,18:0,100",
                    "{1\n[1,1:0,200:100,200,0\nx1,1:0,100\n]\n}1\n{2\n[1,18:0,=",
                    1,
                )
                .replace("(1,18:10,20:", "$1,18:0,20\n(1,18:10,=:");
            let compressed_form = contents.replace("(1,18:10,20:", "f481:0,20\n(1,18:10,=:");
            let form_only_footer = contents.replace("x1,18:80,90", "f2:80,90");
            let form_without_generated = contents.replace("x2,2:20,20", "f2:20,20");
            for (contents, x, y, body) in [
                (&contents, 20.0, 15.0, true),
                (&contents, 80.0, 85.0, false),
                (&contents, 50.0, 50.0, false),
                (&compressed, 20.0, 15.0, true),
                (&compressed_form, 20.0, 15.0, true),
                (&form_only_footer, 80.0, 85.0, false),
                (&form_without_generated, 20.0, 15.0, false),
            ] {
                let map = source_map_from_reader(
                    contents.as_bytes(),
                    Path::new("/deck.pdf"),
                    InversePoint {
                        page: 2,
                        x: (x * 65536.0 / 65781.76 * magnification + x_offset) as f32,
                        y_from_top: (y * 65536.0 / 65781.76 * magnification + y_offset) as f32,
                        page_height_pt: 200.0,
                    },
                    Path::new("/deck.vrb"),
                    true,
                    &Operation::default(),
                    None,
                )
                .unwrap();
                assert_eq!(
                    map.generated_at_point, body,
                    "post={post:?}, point=({x},{y})"
                );
            }
        }
        for contents in [
            sheet.replace("Unit:65536\n", ""),
            sheet.replace("(1,18:10,20:30,10,0", "(1,18:invalid"),
            sheet.replace("Count:12\n", ""),
            sheet.replace("x2,2:20,20", "finvalid:20,20"),
            format!(
                "{}<2\n(2,2:0,20:30,10,0\n)\n>\n",
                sheet.replace("x2,2:20,20", "f2:20,20")
            ),
        ] {
            assert!(
                source_map_from_reader(
                    contents.as_bytes(),
                    Path::new("/deck.pdf"),
                    InversePoint {
                        page: 2,
                        x: 20.0,
                        y_from_top: 15.0,
                        page_height_pt: 200.0
                    },
                    Path::new("/deck.vrb"),
                    true,
                    &Operation::default(),
                    None,
                )
                .is_err()
            );
        }
        let operation = Operation::default();
        let generated = SourceBox {
            bounds: [0.0, 0.0, 20.0, 20.0],
            generated: true,
        };
        for other in [
            SourceBox {
                bounds: generated.bounds,
                generated: false,
            },
            SourceBox {
                bounds: [10.0, 0.0, 30.0, 20.0],
                generated: false,
            },
        ] {
            assert!(!generated_box_at_point(&[generated, other], 15.0, 15.0, &operation).unwrap());
        }
    }

    #[test]
    fn inverse_document_metadata_resolves_title_and_direct_input() {
        let directory = tempfile::tempdir().unwrap();
        let main_path = directory.path().join("original.tex");
        let main = "\\input{preamble.tex}\n\
                    \\title[Example Report and Results]{Example Report and Results}\n\
                    \\author{Ada Example}\n\
                    \\begin{document}\n\
                    \\begin{frame}{Other title}\n\
                    Body text.\n\
                    \\end{frame}\n";
        fs::write(&main_path, main).unwrap();
        fs::write(
            directory.path().join("preamble.tex"),
            "\\institute{North Research Center\\\\\nExample School for Mathematics}\n",
        )
        .unwrap();
        let found = document_metadata_word_location(
            &main_path,
            "Example Report and Results",
            "Example ".len(),
            &mut QuerySources::default(),
            &Operation::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!((found.2, found.3), (2, 43));
        assert!(found.4 >= 6);
        let found = document_metadata_word_location(
            &main_path,
            "North Research Center\r\nExample School for Mathematics",
            "North Research Center\r\nExample School for ".len(),
            &mut QuerySources::default(),
            &Operation::default(),
        )
        .unwrap()
        .unwrap();
        assert!(found.0.ends_with("preamble.tex"));
        assert_eq!((found.2, found.3), (2, 19));
        assert!(
            document_metadata_word_location(
                &main_path,
                "Report",
                0,
                &mut QuerySources::default(),
                &Operation::default(),
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn inverse_metadata_preserves_crlf_and_unicode_byte_offsets() {
        let directory = tempfile::tempdir().unwrap();
        let main_path = directory.path().join("original.tex");
        let declaration = "\\newcommand{\\unused}{éééé}\\title{Example Report}";
        for newline in ["\n", "\r\n"] {
            let mut source = format!("\\documentclass{{beamer}}{newline}");
            for _ in 0..7 {
                source.push_str(&format!("% padding{newline}"));
            }
            source.push_str(declaration);
            fs::write(&main_path, &source).unwrap();
            let found = document_metadata_word_location(
                &main_path,
                "Example Report",
                8,
                &mut QuerySources::default(),
                &Operation::default(),
            )
            .unwrap()
            .unwrap();
            assert_eq!((found.2, found.3), (9, declaration.find("Report").unwrap()));
        }
    }

    #[test]
    fn inverse_frame_title_beats_same_words_in_document_title() {
        let directory = tempfile::tempdir().unwrap();
        let main_path = directory.path().join("original.tex");
        let source = "\\title{Numerical Analysis and Applications in Computational Fluid Dynamics}\n\
                      \\begin{document}\n\
                      \\begin{frame}{Computational Fluid Dynamics}{Grid refinement application}\n\
                      \\begin{figure}\\includegraphics{grid.png}\\end{figure}\n\
                      \\end{frame}\n";
        fs::write(&main_path, source).unwrap();
        let title = "Computational Fluid Dynamics\nGrid refinement application\n\
                     Numerical Analysis and Applications in Computational Fluid Dynamics";
        let title_offset = title.find("Fluid").unwrap();
        let metadata_score = document_metadata_word_location(
            &main_path,
            title,
            title_offset,
            &mut QuerySources::default(),
            &Operation::default(),
        )
        .unwrap()
        .unwrap()
        .4;
        assert!(metadata_score >= 6);
        assert_eq!(
            source_word_location(source, 5, title, title_offset, 4),
            Some((3, source.lines().nth(2).unwrap().find("Fluid").unwrap()))
        );
        assert!(!metadata_beats_frame(
            source,
            5,
            title,
            title_offset,
            10.0,
            272.0,
            metadata_score
        ));
        let footer = "Computational Fluid Dynamics\nGrid refinement application\n\
                      Numerical Analysis and Applications in Computational Fluid Dynamics";
        let footer_offset = footer.rfind("Fluid").unwrap();
        let footer_score = document_metadata_word_location(
            &main_path,
            footer,
            footer_offset,
            &mut QuerySources::default(),
            &Operation::default(),
        )
        .unwrap()
        .unwrap()
        .4;
        assert!(metadata_beats_frame(
            source,
            5,
            footer,
            footer_offset,
            228.0,
            272.0,
            footer_score
        ));
    }

    #[test]
    fn inverse_identical_running_title_uses_click_height_for_tie() {
        let directory = tempfile::tempdir().unwrap();
        let main_path = directory.path().join("original.tex");
        let source = "\\title{Computational Fluid Dynamics}\n\
                      \\begin{frame}{Computational Fluid Dynamics}\n\
                      \\end{frame}\n";
        fs::write(&main_path, source).unwrap();
        let context = "Computational Fluid Dynamics";
        let offset = context.find("Fluid").unwrap();
        let metadata_score = document_metadata_word_location(
            &main_path,
            context,
            offset,
            &mut QuerySources::default(),
            &Operation::default(),
        )
        .unwrap()
        .unwrap()
        .4;
        assert_eq!(
            metadata_score,
            source_prose_scored_location(
                source,
                3,
                context,
                offset,
                1..3,
                true,
                &nonprinting_source_ranges(source)
            )
            .unwrap()
            .2
        );
        assert!(!metadata_beats_frame(
            source,
            3,
            context,
            offset,
            10.0,
            272.0,
            metadata_score
        ));
        assert!(metadata_beats_frame(
            source,
            3,
            context,
            offset,
            228.0,
            272.0,
            metadata_score
        ));
    }

    #[test]
    fn inverse_frame_matching_ignores_commented_and_escaped_boundaries() {
        let source = "% \\begin{frame}\n\\begin {frame}\n\
                      % \\end{frame}\n\\\\end{frame}\n\
                      Before \\% percent target. % hidden\n\n\n\n\n\\end {frame}";
        assert_eq!(
            source_word_location(source, 10, "percent target", 9, 0),
            Some((5, 18))
        );
        assert_eq!(source_word_location(source, 10, "hidden", 1, 4), None);
        let unterminated = "\\begin{frame}\ntarget\n\n\n\n\n";
        assert_eq!(source_word_location(unterminated, 6, "target", 1, 0), None);
    }

    #[test]
    fn inverse_caption_math_uses_literal_context_not_closing_line_proximity() {
        let source = "\\caption{Selected views.\n\
            sample points at measured offset at most \\(\\alpha_k\\) from the\n\
            baseline marker.\n\n\n\n\n\
            The vertical axis is in units of \\(\\alpha_k\\), hence rescaled.\n\
            End of caption.}\n";
        let line = source.lines().count() as u32;
        for (context, row) in [
            ("offset at most 𝛼𝑘 from the baseline marker.", 2),
            ("axis is in units of 𝛼𝑘, hence rescaled.", 8),
        ] {
            assert_eq!(
                source_word_location(source, line, context, context.find('𝛼').unwrap(), 4),
                Some((
                    row,
                    source
                        .lines()
                        .nth(row as usize - 1)
                        .unwrap()
                        .find("\\alpha")
                        .unwrap()
                ))
            );
        }
        // The glyph alone cannot identify which repeated expression was clicked.
        assert_eq!(source_word_location(source, line, "𝛼𝑘", 0, 4), None);
    }

    #[test]
    fn inverse_inline_math_does_not_use_hidden_comment_context() {
        let source = "\\caption{\\emph{at most} \\(\\rho_q\\)\n\
                      % at most\n\\(\\rho_q\\)\n}";
        assert_eq!(
            source_word_location(source, 4, "at most 𝜌𝑞", "at most ".len(), 4),
            None
        );
    }

    fn inverse_output(
        pdf: &Path,
        stdout: &str,
        word: Option<(&str, usize)>,
    ) -> io::Result<InverseResolution> {
        resolve_inverse_output(
            pdf,
            InversePoint {
                page: 1,
                x: 72.0,
                y_from_top: 120.0,
                page_height_pt: 792.0,
            },
            word,
            0,
            &Operation::default(),
            stdout,
            QuerySources::default(),
        )
    }

    #[test]
    fn inverse_no_match_and_competing_anchors_have_distinct_typed_failures() {
        let directory = tempfile::tempdir().unwrap();
        let pdf = directory.path().join("paper.pdf");
        fs::write(directory.path().join("paper.tex"), "Copper.\nZephyr.\n").unwrap();
        let no_match = inverse_output(&pdf, "SyncTeX result begin\nSyncTeX result end\n", None)
            .err()
            .expect("empty native results are not source ambiguity");
        assert_eq!(no_match.kind(), io::ErrorKind::NotFound);
        assert!(matches!(
            no_match
                .get_ref()
                .and_then(|error| error.downcast_ref::<InverseFailure>()),
            Some(InverseFailure::NoMatch)
        ));
        let ambiguous = inverse_output(
            &pdf,
            "Input: paper.tex\nLine: 1\nInput: paper.tex\nLine: 2\n",
            None,
        )
        .err()
        .expect("competing valid anchors are not no-match");
        assert_eq!(ambiguous.kind(), io::ErrorKind::InvalidData);
        assert!(matches!(
            ambiguous
                .get_ref()
                .and_then(|error| error.downcast_ref::<InverseFailure>()),
            Some(InverseFailure::Ambiguous(_))
        ));
    }

    #[test]
    fn inverse_raw_hits_preserve_distinct_records_without_trailing_duplicates() {
        let stdout = "Output: paper.pdf\nInput: preamble.tex\nLine: 9\nColumn: -1\n\
                      Offset: 0\nContext: ignored\nInput: paper.tex\nLine: 3\n\
                      Column: -1\nInput: preamble.tex\nLine: 9\nColumn: -1\n";
        let targets = parse_synctex_edit(stdout);
        assert_eq!(
            targets
                .iter()
                .map(|target| (target.file.as_str(), target.line))
                .collect::<Vec<_>>(),
            vec![("preamble.tex", 9), ("paper.tex", 3)]
        );
        for incomplete in [
            "Output: paper.pdf\nLine: 3\n",
            "Output: paper.pdf\nInput: paper.tex\n",
            "Input: paper.tex\nLine: 0\n",
            "",
        ] {
            assert!(parse_synctex_edit(incomplete).is_empty());
        }
    }

    #[test]
    fn inverse_unique_supported_position_beats_unrelated_coarse_hits_in_any_order() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("paper.tex");
        let caption = "\\caption{Mechanical measurements remain stable.}";
        fs::write(&source, format!("{caption}\nUnrelated coarse heading.\n")).unwrap();
        let supported = "Input: paper.tex\nLine: 1\nColumn: -1\n";
        let alias = "Input: ./paper.tex\nLine: 1\nColumn: -1\n";
        let coarse = "Input: paper.tex\nLine: 2\nColumn: -1\n";
        for stdout in [
            format!("{supported}{coarse}{alias}"),
            format!("{alias}{coarse}{supported}"),
            format!("{coarse}{supported}{alias}"),
        ] {
            let result = inverse_output(
                &directory.path().join("paper.pdf"),
                &stdout,
                Some(("Mechanical measurements remain stable", 0)),
            )
            .unwrap();
            assert_eq!(
                result.location.file,
                source.canonicalize().unwrap().to_str().unwrap()
            );
            assert_eq!(result.location.line, 1);
            assert_eq!(
                result.location.byte_column,
                caption.find("Mechanical").unwrap()
            );
            assert!(result.location.precise);
        }
    }

    #[test]
    fn inverse_closing_layout_anchor_does_not_guess_a_neighboring_prose_row() {
        let source = "\\begin{tabularx}{\\linewidth}{X}\n\
                      Group alpha \\(n\\); primary sample columns. \\\\\n\
                      \\\\\n\\\\\n\\\\\n\\\\\n\
                      Group beta \\(n\\); direct step. \\\\\n\
                      Group alpha \\(n\\); secondary columns for sample tables. \\\\\n\
                      \\end{tabularx}\n";
        let first_context = "Group alpha 𝑛; primary sample columns.";
        assert_eq!(source_word_location(source, 9, first_context, 0, 4), None);
        assert_eq!(
            source_word_location(source, 2, first_context, 0, 4),
            Some((2, 0))
        );
        let directory = tempfile::tempdir().unwrap();
        let complete = format!("{source}{}Unrelated coarse heading.\n", "\n".repeat(8));
        let coarse_line = complete.lines().count();
        fs::write(directory.path().join("paper.tex"), complete).unwrap();
        let error = resolve_inverse_output(
            &directory.path().join("paper.pdf"),
            InversePoint {
                page: 1,
                x: 10.0,
                y_from_top: 10.0,
                page_height_pt: 600.0,
            },
            Some((first_context, 0)),
            4,
            &Operation::default(),
            &format!("Input: paper.tex\nLine: 9\nInput: paper.tex\nLine: {coarse_line}\n"),
            QuerySources::default(),
        )
        .err()
        .expect("unqualified layout rows must remain competing coarse anchors");
        assert!(error.to_string().contains("ambiguous inverse search"));
    }

    #[test]
    fn inverse_closing_layout_does_not_guess_neighboring_mathematical_atoms() {
        let source = "\\begin{tabular}{l}\n\\(s+1\\) \\\\\n\n\n\n\n\
                      \\(s+1\\) \\\\\n\\end{tabular}\n";
        assert_eq!(source_word_location(source, 8, "𝑠+1", 0, 4), None);
        assert_eq!(source_word_location(source, 2, "𝑠+1", 0, 4), Some((2, 2)));
        let equation = "\\begin{equation}\ns+1\n\\end{equation}\n";
        assert_eq!(source_word_location(equation, 3, "𝑠+1", 0, 4), Some((2, 0)));
    }

    #[test]
    fn inverse_competing_precise_or_coarse_positions_are_ambiguous_in_any_order() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("paper.tex"),
            "Silver signal remains stable.\nSilver signal remains stable.\n",
        )
        .unwrap();
        let first = "Input: paper.tex\nLine: 1\n";
        let second = "Input: paper.tex\nLine: 2\n";
        for stdout in [format!("{first}{second}"), format!("{second}{first}")] {
            for (word, expected) in [
                (Some(("Silver signal remains stable", 0)), "precise"),
                (None, "coarse"),
            ] {
                let error = inverse_output(&directory.path().join("paper.pdf"), &stdout, word)
                    .err()
                    .expect("competing source locations must be ambiguous")
                    .to_string();
                assert!(error.contains("ambiguous inverse search"), "{error}");
                assert!(error.contains(expected), "{error}");
            }
        }
    }

    #[test]
    fn inverse_duplicate_hits_deduplicate_final_canonical_coarse_positions() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("paper.tex");
        fs::write(&source, "First visible line.\nSecond visible line.\n").unwrap();
        let stdout = "Input: paper.tex\nLine: 2\nColumn: -1\n\
                      Input: ./paper.tex\nLine: 2\nColumn: -1\n\
                      Input: paper.tex\nLine: 2\nColumn: -1\n";
        let result = inverse_output(&directory.path().join("paper.pdf"), stdout, None).unwrap();
        assert_eq!(
            result.location.file,
            source.canonicalize().unwrap().to_str().unwrap()
        );
        assert_eq!((result.location.line, result.location.byte_column), (2, 0));
        assert!(!result.location.precise);
        assert!(result.warning.is_none());
    }

    #[test]
    fn inverse_readable_bounds_are_required_even_without_word_refinement() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("main.tex"),
            "Visible line.\n".repeat(272),
        )
        .unwrap();
        let error = inverse_output(
            &directory.path().join("main.pdf"),
            "Input: main.tex\nLine: 490\n",
            None,
        )
        .err()
        .expect("a readable out-of-range source anchor must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let failure = error
            .get_ref()
            .and_then(|error| error.downcast_ref::<InverseFailure>())
            .expect("source bounds retain a typed failure");
        match failure {
            InverseFailure::InvalidSourceAnchor {
                file,
                line,
                available,
            } => {
                assert_eq!(
                    file,
                    &directory.path().join("main.tex").canonicalize().unwrap()
                );
                assert_eq!((*line, *available), (490, 272));
            }
            other => panic!("unexpected inverse failure: {other}"),
        }
    }

    #[test]
    fn inverse_unreadable_source_remains_explicitly_warned_coarse_navigation() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("main.tex");
        fs::write(&source, [0xff]).unwrap();
        for unavailable in ["invalid UTF-8", "missing"] {
            if unavailable == "missing" {
                fs::remove_file(&source).unwrap();
            }
            for word in [None, Some(("Visible line", 0))] {
                let result = inverse_output(
                    &directory.path().join("main.pdf"),
                    "Input: main.tex\nLine: 490\nInput: ./main.tex\nLine: 490\n",
                    word,
                )
                .unwrap();
                assert_eq!(result.location.line, 490);
                assert!(!result.location.precise);
                assert!(
                    result
                        .warning
                        .unwrap()
                        .contains("source refinement unavailable"),
                    "{unavailable}"
                );
            }
        }
    }

    #[test]
    fn inverse_generated_hit_remaps_before_bounds_without_reading_reused_verbatim() {
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("original.tex");
        fs::write(
            &original,
            "\\title{Shared Section Words}\n\\begin{frame}[fragile]\nLiteral body text.\n\\end{frame}\n\\begin{frame}{Plan}\n\\tableofcontents\n\\end{frame}\n",
        )
        .unwrap();
        fs::write(directory.path().join("deck.vrb"), [0xff]).unwrap();
        fs::write(
            directory.path().join("deck.synctex"),
            "Input:1:original.tex\nInput:2:deck.vrb\nInput:3:build/deck.toc\nContent:\n\
             {1\n[1,4:0,0:1,1,0\nx2,490:0,0\n]\n}1\n\
             {2\n[1,7:0,0:1,1,0\nx3,12:0,0\n]\n}2\n",
        )
        .unwrap();
        let result = inverse_output(
            &directory.path().join("deck.pdf"),
            "Input: deck.vrb\nLine: 490\n",
            None,
        )
        .unwrap();
        assert_eq!(
            result.location.file,
            original.canonicalize().unwrap().to_str().unwrap()
        );
        assert_eq!(result.location.line, 4);
        assert!(!result.location.precise);
        assert!(result.warning.is_none());

        let toc_result = resolve_inverse_output(
            &directory.path().join("deck.pdf"),
            InversePoint {
                page: 2,
                x: 72.0,
                y_from_top: 120.0,
                page_height_pt: 792.0,
            },
            Some(("Shared Section Words", 0)),
            0,
            &Operation::default(),
            "Input: build/deck.toc\nLine: 12\n",
            QuerySources::default(),
        )
        .unwrap();
        assert_eq!(
            toc_result.location.file,
            original.canonicalize().unwrap().to_str().unwrap()
        );
        assert_eq!(toc_result.location.line, 7);
        assert!(!toc_result.location.precise);
        assert!(toc_result.warning.is_none());
    }

    #[test]
    fn forward_source_bounds_apply_when_the_original_position_has_no_word() {
        let source = "école \\hidden{} % not printed\n\n";
        for (line, column) in [(1, 6), (2, 1)] {
            let text = validate_forward_source_position(source, line, column).unwrap();
            assert!(ForwardWord::at(text, column).is_none());
        }
        assert!(validate_forward_source_position(source, 2, 2).is_err());
        assert!(validate_forward_source_position(source, 3, 1).is_err());
        assert!(validate_forward_source_position(source, 1, u32::MAX).is_err());
    }

    #[test]
    fn forward_duplicate_regions_do_not_hide_invalid_or_incomplete_records() {
        let synctex = "\
Input:1:/tmp/main.tex
Content:
<10
(1,5:0,100:200,10,0
x1,5:0,100
)
>
{1
[1,1:0,0:0,0,0
(1,5:100,200:200,10,0
x1,5:100,200
)
(1,5:100,200:200,10,0
x1,5:100,200
)
(1,5:100,300:-50,10,0
x1,5:100,300
)
f10:50,400
]
}1
";
        let pdf = Path::new("/tmp/main.pdf");
        let file = Path::new("/tmp/main.tex");
        let candidates =
            forward_candidates_from_reader(synctex.as_bytes(), pdf, file, None, 5).unwrap();
        assert_eq!(candidates.len(), 2);
        assert!(
            forward_candidates_from_reader(
                b"Input:1:/tmp/other.tex\nContent:\n" as &[u8],
                pdf,
                file,
                None,
                1
            )
            .is_err()
        );
    }

    #[test]
    fn forward_word_uses_scalar_columns_and_rejects_commands_and_comments() {
        let line = "école \\emph{naïve} word % hidden";
        let hint = ForwardWord::at(line, 13).unwrap();
        assert_eq!(hint.words[hint.selected], "naïve");
        for column in [0, 6, 8, 24, 26, u32::MAX] {
            assert!(ForwardWord::at(line, column).is_none(), "column {column}");
        }
        let mut hint = hint;
        hint.selected = hint.words.len();
        assert!(!hint.valid());
        let decomposed = "e\u{301}cole";
        for column in 1..=decomposed.chars().count() as u32 {
            let hint = ForwardWord::at(decomposed, column).unwrap();
            assert_eq!(hint.words[hint.selected], decomposed);
            assert!(hint.valid());
        }
        let overlay = "\\only<3>{visible} \\includesvg[width=\\linewidth]{diagram}";
        assert!(ForwardWord::at(overlay, 7).is_none());
        assert!(ForwardWord::at(overlay, 50).is_none());
        let visible_hint = ForwardWord::at(overlay, 10).unwrap();
        assert_eq!(visible_hint.words, ["visible"]);
    }

    #[test]
    fn literal_forward_word_uses_utf8_boundaries_and_unicode_context() {
        let revision = PdfRevision::read(Path::new(file!())).unwrap();
        let request = |text: &str, byte_column: usize| {
            parse_forward_request(
                &serde_json::json!({
                    "pdf": "/literal.pdf", "revision": revision, "page": 1,
                    "h": 72, "v": 120, "width": 0, "height": 0,
                    "word": { "text": text, "byte_column": byte_column }
                })
                .to_string(),
            )
        };
        let text = "one two three four café λ e\u{301}cole 漢字 nine ten";
        let selected = text.find("e\u{301}cole").unwrap();
        for byte in [selected, selected + 1] {
            let hint = request(text, byte).unwrap().word.unwrap();
            assert_eq!(
                hint.words,
                ["four", "café", "λ", "e\u{301}cole", "漢字", "nine", "ten"]
            );
            assert_eq!(hint.selected, 3);
        }
        assert_eq!(request("λ first", 0).unwrap().word.unwrap().words[0], "λ");
        assert_eq!(
            request("100% literal", 5).unwrap().word.unwrap().words[1],
            "literal"
        );
        assert!(request("λ first", 1).is_err());
        assert!(request(text, text.len() + 1).is_err());
        for byte in [3, text.len()] {
            assert!(request(text, byte).unwrap().word.is_none());
        }
        assert!(request(&"x".repeat(129), 0).unwrap().word.is_none());
    }

    #[test]
    fn forward_geometry_preserves_top_down_coordinates_and_rejects_overflow() {
        let revision = PdfRevision::read(Path::new(file!())).unwrap();
        let payload = format!(
            r#"{{"pdf":"/a:b.pdf","revision":{},"page":2,"h":72,"v":120,"width":250,"height":12}}"#,
            serde_json::to_string(&revision).unwrap()
        );
        let request = parse_forward_request(&payload).unwrap();
        assert_eq!(
            request.rect(),
            SearchRect {
                left: 72.0,
                top: 108.0,
                right: 322.0,
                bottom: 120.0
            }
        );
        assert_eq!(request.pdf, Path::new("/a:b.pdf"));
        let mut request = request;
        request.h = f32::MAX;
        request.width = f32::MAX;
        assert!(request.validate().is_err());
        for invalid in [
            payload.replace("\"page\":2", "\"page\":0"),
            payload.replace("\"/a:b.pdf\"", "\"relative.pdf\""),
            payload.replace("\"height\":12", "\"height\":-12"),
            payload.replace("\"h\":72", "\"h\":1e100"),
            payload.replace("\"h\":72", "\"h\":72,\"h\":73"),
            payload.replace("\"h\":72", "\"h\":72,\"unexpected\":0"),
            payload.replace("\"h\":72", r#""h":72,"best_guess":"false""#),
            payload.replace("\"h\":72", r#""h":72,"best_guess":null"#),
            payload.replace("\"h\":72", r#""h":72,"source_snapshot":"relative.json""#),
            payload.replace(
                "\"h\":72",
                r#""h":72,"source_snapshot":"/cache/\u0000.json""#,
            ),
            payload.replace("\"h\":72", r#""h":72,"warning":"" "#),
            payload.replace("\"h\":72", r#""h":72,"warning":"\u001b[31munsafe""#),
            payload.replace(
                "\"h\":72",
                &format!(
                    "\"h\":72,\"warning\":\"{}\"",
                    "x".repeat(crate::source_snapshot::MAX_WARNING_BYTES + 1)
                ),
            ),
            payload.replace("\"h\":72", r#""h":72,"word":{"words":[],"selected":0}"#),
            payload.replace(
                "\"h\":72",
                r#""h":72,"word":{"words":["text"],"selected":1}"#,
            ),
            payload.replace(
                "\"h\":72",
                r#""h":72,"word":{"words":["two words"],"selected":0}"#,
            ),
        ] {
            assert!(parse_forward_request(&invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn nonprinting_spans_and_frame_bounds_handle_control_symbols_let_and_verbatim() {
        let source = "\\let\\olddef\\def\n\\newcommand\\+{HiddenSymbol}\nVisibleProse\n";
        let visible_at = source.find("VisibleProse").unwrap();
        let hidden = nonprinting_source_ranges(source);
        assert!(hidden.iter().all(|range| !range.contains(&visible_at)));
        let hidden_at = source.find("HiddenSymbol").unwrap();
        assert!(hidden.iter().any(|range| range.contains(&hidden_at)));

        let frame_source = "\
\\begin{frame}
Slide text
\\begin{verbatim}
\\begin{frame}
\\end{frame}
\\end{verbatim}
More slide text
\\end{frame}";
        assert_eq!(frame_bounds(frame_source, 7), Some((0, 7)));
        assert_eq!(normalized_word("oﬃce"), "office");
        assert_eq!(normalized_word(" Café "), " cafe ");

        let article = "\\title{Sample Document Title for Testing}\n\
                       \\maketitle\n\
                       Keywords. Sample-document testing, synthetic examples.\n";
        assert!(!metadata_beats_frame(
            article,
            3,
            "Sample-document testing,",
            9,
            386.5,
            792.0,
            6,
        ));
        assert!(!allows_metadata_fallback(article, 3, 0, 386.5, 792.0));
        assert!(allows_metadata_fallback(article, 2, 1, 115.0, 792.0));
        assert_eq!(
            source_word_location(article, 3, "Keywords. Sample-document testing", 13, 1),
            Some((3, 10)),
        );
        let accented =
            "\\author{Jane Example\\thanks{School of Th\\'eorie\nMod\\^ele d'Étude}}";
        assert_eq!(
            source_word_location(
                accented,
                1,
                "Center of Études Modêle d’Éxemple",
                10,
                1,
            ),
            Some((1, 38)),
        );
        assert_eq!(
            source_word_location(
                accented,
                2,
                "Center of Études Modêle d’Éxemple",
                18,
                1,
            ),
            Some((2, 0)),
        );

        let synctex = "\
Input:1:/tmp/main.tex
Input:2:/tmp/./section.tex
{1
[2,44:0,0:0,0,0
(2,44:0,49:100,10,0
x2,44:0,49
)
(2,29:0,700:100,10,0
x2,29:0,700
)
r2,44:0,700:8,7,0
]
}1
{2
[1,10:0,0:0,0,0
(2,44:0,178:100,10,0
x2,43:0,178
g2,43:10,178
k2,44:20,178:0
)
(2,48:0,200:100,10,0
x2,48:0,200
)
]
}2
";
        let pdf = Path::new("/tmp/main.pdf");
        let section = Path::new("/tmp/section.tex");
        assert_eq!(
            forward_candidates_from_reader(synctex.as_bytes(), pdf, section, None, 29).unwrap()[0]
                .page,
            1
        );
        assert_eq!(
            forward_candidates_from_reader(synctex.as_bytes(), pdf, section, None, 44).unwrap()[0]
                .page,
            2
        );
        assert_eq!(
            forward_candidates_from_reader(synctex.as_bytes(), pdf, section, None, 45).unwrap()[0]
                .page,
            2
        );
    }

    #[test]
    fn fuzz_forward_candidates_matches_synctex_view_and_survives_malformed_inputs() {
        fn next_u64(state: &mut u64) -> u64 {
            let mut x = *state;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *state = x;
            x
        }
        fn parse_cli_synctex_view(output: &str) -> Vec<ForwardCandidate> {
            let mut out = Vec::new();
            let mut page = None::<u32>;
            let mut h = None::<f32>;
            let mut v = None::<f32>;
            let mut w = None::<f32>;
            let mut height_opt = None::<f32>;
            let flush = |out: &mut Vec<ForwardCandidate>,
                         page: &mut Option<u32>,
                         h: &mut Option<f32>,
                         v: &mut Option<f32>,
                         w: &mut Option<f32>,
                         height_opt: &mut Option<f32>| {
                if let (Some(p), Some(x), Some(y), Some(width), Some(height)) =
                    (page.take(), h.take(), v.take(), w.take(), height_opt.take())
                    && forward_geometry_valid(p, x, y, width, height)
                {
                    let cand = ForwardCandidate {
                        page: p,
                        rect: SearchRect {
                            left: x,
                            top: y - height,
                            right: x + width,
                            bottom: y,
                        },
                    };
                    if !out
                        .iter()
                        .any(|old| old.page == cand.page && old.rect == cand.rect)
                    {
                        out.push(cand);
                    }
                }
                *page = None;
                *h = None;
                *v = None;
                *w = None;
                *height_opt = None;
            };
            for line in output.lines() {
                if let Some(val) = line.strip_prefix("Page:") {
                    flush(&mut out, &mut page, &mut h, &mut v, &mut w, &mut height_opt);
                    page = val.trim().parse().ok();
                } else if let Some(val) = line.strip_prefix("h:") {
                    h = val.trim().parse().ok();
                } else if let Some(val) = line.strip_prefix("v:") {
                    v = val.trim().parse().ok();
                } else if let Some(val) = line.strip_prefix("W:") {
                    w = val.trim().parse().ok();
                } else if let Some(val) = line.strip_prefix("H:") {
                    height_opt = val.trim().parse().ok();
                }
            }
            flush(&mut out, &mut page, &mut h, &mut v, &mut w, &mut height_opt);
            out
        }
        fn gen_box_body(
            rng: &mut u64,
            out: &mut String,
            depth: usize,
            base_line: i64,
            max_form_tag: u32,
            in_hbox: bool,
        ) {
            let count = (next_u64(rng) % 6) + 1;
            for idx in 0..count {
                let tag = if next_u64(rng) % 4 == 0 { 2 } else { 1 };
                let line = base_line + (next_u64(rng) % 8) as i64;
                let x = ((next_u64(rng) % 400_000) as i64) + 10_000;
                let y = ((next_u64(rng) % 600_000) as i64) + 20_000;
                let w = ((next_u64(rng) % 150_000) as i64) + 1_000;
                let h = ((next_u64(rng) % 20_000) as i64) + 500;
                let d = (next_u64(rng) % 5_000) as i64;
                let y_str = if next_u64(rng) % 5 == 0 {
                    "=".to_string()
                } else {
                    y.to_string()
                };
                match next_u64(rng) % 10 {
                    0 if depth < 3 && (in_hbox || idx + 1 < count) => {
                        out.push_str(&format!("[{tag},{line}:{x},{y}:{w},{h},{d}\n"));
                        gen_box_body(rng, out, depth + 1, base_line, max_form_tag, false);
                        out.push_str("]\n");
                    }
                    1 | 2 if depth < 3 && (in_hbox || idx + 1 < count) => {
                        out.push_str(&format!("({tag},{line}:{x},{y}:{w},{h},{d}\n"));
                        gen_box_body(rng, out, depth + 1, base_line, max_form_tag, true);
                        out.push_str(")\n");
                    }
                    3 => out.push_str(&format!("v{tag},{line}:{x},{y_str}:{w},{h},{d}\n")),
                    4 => out.push_str(&format!("h{tag},{line}:{x},{y_str}:{w},{h},{d}\n")),
                    5 => {
                        let kw = if next_u64(rng) % 3 == 0 { -w } else { w };
                        out.push_str(&format!("k{tag},{line}:{x},{y_str}:{kw}\n"));
                        if next_u64(rng) % 2 == 0 {
                            out.push_str(&format!("g{tag},{line}:{x},{y_str}\n"));
                        }
                    }
                    6 => out.push_str(&format!("g{tag},{line}:{x},{y_str}\n")),
                    7 => out.push_str(&format!("r{tag},{line}:{x},{y_str}:{w},{h},{d}\n")),
                    8 => out.push_str(&format!("${tag},{line}:{x},{y_str}\n")),
                    9 if max_form_tag > 0 && (in_hbox || idx > 0) && next_u64(rng) % 2 == 0 => {
                        let ftag = ((next_u64(rng) as u32) % max_form_tag) + 1;
                        out.push_str(&format!("f{ftag}:{x},{y_str}\n"));
                    }
                    _ => out.push_str(&format!("x{tag},{line}:{x},{y_str}\n")),
                }
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("doc.pdf");
        let main_tex = dir.path().join("main.tex");
        let sub_tex = dir.path().join("sub.tex");
        let synctex_path = dir.path().join("doc.synctex");
        fs::write(&pdf, b"%PDF-1.4\n").unwrap();
        fs::write(&main_tex, "line\n".repeat(120)).unwrap();
        fs::write(&sub_tex, "line\n".repeat(120)).unwrap();

        let mut rng = 0x9E37_79B9_7F4A_7C15u64;
        for case_idx in 0..80 {
            let mag = [1000, 1200, 800][(next_u64(&mut rng) % 3) as usize];
            let unit = [8192, 4096][(next_u64(&mut rng) % 2) as usize];
            let x_off = (next_u64(&mut rng) % 600) as i64;
            let y_off = (next_u64(&mut rng) % 600) as i64;
            let mut synctex = format!(
                "SyncTeX Version:1\nInput:1:{}\nInput:2:{}\nOutput:pdf\nMagnification:{mag}\nUnit:{unit}\nX Offset:{x_off}\nY Offset:{y_off}\nContent:\n",
                main_tex.display(),
                sub_tex.display(),
            );
            let num_forms = (next_u64(&mut rng) % 3) as u32;
            for ftag in 1..=num_forms {
                synctex.push_str(&format!("<{ftag}\n(1,5:10000,20000:50000,8000,2000\n"));
                gen_box_body(&mut rng, &mut synctex, 1, 5, ftag - 1, true);
                synctex.push_str(")\n>\n");
            }
            let pages = ((next_u64(&mut rng) % 3) + 1) as u32;
            for p in 1..=pages {
                let base_line = i64::from(p) * 20;
                synctex.push_str(&format!(
                    "{{{p}\n[1,1:0,0:400000,600000,0\n(1,{base_line}:10000,30000:300000,12000,3000\n"
                ));
                gen_box_body(&mut rng, &mut synctex, 1, base_line, num_forms, true);
                synctex.push_str(&format!(")\n]\n}}{p}\n"));
            }
            synctex.push_str("Postamble:\nCount:100\n");
            if next_u64(&mut rng) % 2 == 0 {
                let x_unit = ["pt", "bp", "in", "cm", "mm", "pc", "sp", "dd", "cc", "nd", "nc"]
                    [(next_u64(&mut rng) % 11) as usize];
                let y_unit = ["pt", "bp", "in", "cm", "mm", "pc", "sp", "dd", "cc", "nd", "nc"]
                    [(next_u64(&mut rng) % 11) as usize];
                synctex.push_str(&format!(
                    "Post scriptum:\nMagnification:1.25\nX Offset:2{x_unit}\nY Offset:3{y_unit}\n"
                ));
            }
            fs::write(&synctex_path, &synctex).unwrap();

            for (target_tex, query_line) in [
                (&main_tex, 1u32),
                (&main_tex, 5),
                (&main_tex, 20),
                (&main_tex, 23),
                (&main_tex, 27),
                (&main_tex, 41),
                (&main_tex, 45),
                (&main_tex, 62),
                (&main_tex, 95),
                (&sub_tex, 21),
                (&sub_tex, 42),
            ] {
                let cli_out = Command::new("synctex")
                    .args([
                        "view",
                        "-i",
                        &format!("{query_line}:0:{}", target_tex.display()),
                        "-o",
                        &pdf.to_string_lossy(),
                    ])
                    .output()
                    .unwrap();
                let expected = parse_cli_synctex_view(&String::from_utf8_lossy(&cli_out.stdout));
                let actual = forward_candidates_from_reader_with_options(
                    synctex.as_bytes(),
                    &pdf,
                    target_tex,
                    None,
                    query_line,
                    false,
                )
                .unwrap_or_default();
                let _ = forward_candidates_from_reader(
                    synctex.as_bytes(),
                    &pdf,
                    target_tex,
                    None,
                    query_line,
                );
                assert_eq!(
                    actual.len(),
                    expected.len(),
                    "case {case_idx} line {query_line} count mismatch\nactual={actual:?}\nexpected={expected:?}\nstdout={}\nstderr={}\nsynctex:\n{synctex}",
                    String::from_utf8_lossy(&cli_out.stdout),
                    String::from_utf8_lossy(&cli_out.stderr),
                );
                for (i, (a, e)) in actual.iter().zip(expected.iter()).enumerate() {
                    assert_eq!(
                        a.page, e.page,
                        "case {case_idx} line {query_line} idx {i} page\nactual={actual:?}\nexpected={expected:?}\nsynctex:\n{synctex}"
                    );
                    assert!(
                        (a.rect.left - e.rect.left).abs() < 2e-2
                            && (a.rect.top - e.rect.top).abs() < 2e-2
                            && (a.rect.right - e.rect.right).abs() < 2e-2
                            && (a.rect.bottom - e.rect.bottom).abs() < 2e-2,
                        "case {case_idx} line {query_line} idx {i} rect mismatch: actual={:?} expected={:?}\nsynctex:\n{synctex}",
                        a.rect,
                        e.rect
                    );
                }
            }

            // Mutate the valid document 8 ways per case (640 total mutated inputs) and verify no panics
            for _ in 0..8 {
                let mut mutated = synctex.clone().into_bytes();
                let ops = (next_u64(&mut rng) % 6) + 1;
                for _ in 0..ops {
                    if mutated.is_empty() {
                        break;
                    }
                    let idx = (next_u64(&mut rng) as usize) % mutated.len();
                    match next_u64(&mut rng) % 4 {
                        0 => mutated.truncate(idx),
                        1 => {
                            let b = b"()[]{}<>fxgkhvr$0123456789,-:=\n"
                                [(next_u64(&mut rng) % 31) as usize];
                            mutated[idx] = b;
                        }
                        2 => {
                            mutated.remove(idx);
                        }
                        _ => {
                            mutated.insert(idx, (next_u64(&mut rng) & 0xFF) as u8);
                        }
                    }
                }
                let qline = ((next_u64(&mut rng) % 80) + 1) as u32;
                let _ = forward_candidates_from_reader(&mutated[..], &pdf, &main_tex, None, qline);
            }
        }

        // Adversarial / malformed inputs: cyclic forms, huge i64 values, unbalanced delimiters
        let adversarial = [
            format!(
                "Input:1:{}\nContent:\n<1\n(1,1:0,0:10,10,0\nf2:0,0\n)\n>\n<2\n(1,1:0,0:10,10,0\nf1:0,0\n)\n>\n{{1\n[1,1:0,0:10,10,0\nf1:0,0\n]\n}}1\n",
                main_tex.display()
            ),
            format!(
                "Input:1:{}\nMagnification:-9223372036854775808\nUnit:0\nContent:\n{{1\n(1,9223372036854775807:9223372036854775807,-9223372036854775808:9223372036854775807,9223372036854775807,9223372036854775807\nx1,1:-9223372036854775808,9223372036854775807\nk1,1:9223372036854775807,=:9223372036854775807\ng1,1:0,=\n)\n]\n)\n}}1\n",
                main_tex.display()
            ),
            format!(
                "Input:1:{}\nContent:\n{{1\n(\n[\n)\n]\nf999:1,2\nx1,2:3,=\n",
                main_tex.display()
            ),
        ];
        for bad in &adversarial {
            let _ = forward_candidates_from_reader(bad.as_bytes(), &pdf, &main_tex, None, 1);
        }
    }
}
