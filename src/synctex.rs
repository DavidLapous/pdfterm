mod math;

use crate::pdf::SearchRect;
use crate::process::Operation;
use serde::{Deserialize, Serialize};
use std::{fs, io, os::unix::fs::MetadataExt, path::Path, process::Command};
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

/// Source coordinates: one-based line, zero-based UTF-8 byte offset;
/// column is one-based UTF-16 (VS Code), column_char is one-based Unicode scalar.
#[derive(Debug, Serialize, Deserialize)]
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
        let words: Vec<_> = words(text)
            .into_iter()
            .filter(|(start, _)| *start == 0 || !text[..*start].ends_with('\\'))
            .collect();
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
    pub word: Option<ForwardWord>,
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
        if self.word.as_ref().is_some_and(|word| !word.valid()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid forward word context",
            ));
        }
        if !self.pdf.is_absolute()
            || self.page == 0
            || self.width < 0.0
            || self.height < 0.0
            || ![
                self.h,
                self.v,
                self.width,
                self.height,
                self.h + self.width,
                self.v - self.height,
            ]
            .into_iter()
            .all(f32::is_finite)
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

pub fn resolve_forward(
    pdf: &Path,
    file: &Path,
    line: u32,
    column: u32,
) -> io::Result<ForwardRequest> {
    resolve_forward_with_library(pdf, file, line, column, None)
}

pub fn resolve_forward_with_library(
    pdf: &Path,
    file: &Path,
    line: u32,
    column: u32,
    library: Option<&Path>,
) -> io::Result<ForwardRequest> {
    if line == 0 || column == 0 {
        return Err(io::Error::other("source line and column must be positive"));
    }
    let pdf = fs::canonicalize(pdf)?;
    let file = fs::canonicalize(file)?;
    let revision = PdfRevision::read(&pdf)?;
    let source = read_source(&file)?;
    let word = source
        .lines()
        .nth(line as usize - 1)
        .and_then(|text| ForwardWord::at(text, column));
    // Beamer records frame bodies at the closing line; query that line so the
    // resolver cannot pick an earlier frame's mapping.
    let view_line = frame_closing_line(&source, line).unwrap_or(line);
    let spec = format!(
        "{view_line}:{column}:{}",
        file.to_str()
            .ok_or_else(|| io::Error::other("source path is not UTF-8"))?
    );
    let stdout = run(
        &Operation::default(),
        &[
            "view",
            "-i",
            &spec,
            "-o",
            pdf.to_str()
                .ok_or_else(|| io::Error::other("PDF path is not UTF-8"))?,
        ],
    )?;
    let (mut page, mut h, mut v, mut width, mut height) = (None, None, None, None, None);
    let mut candidates = Vec::new();
    let mut captured = false;
    for row in stdout.lines() {
        if let Some((key, value)) = row.split_once(':') {
            match key {
                "Page" => {
                    page = value.trim().parse::<u32>().ok();
                    h = None;
                    v = None;
                    width = None;
                    height = None;
                    captured = false;
                }
                "h" => h = value.trim().parse::<f32>().ok(),
                "v" => v = value.trim().parse::<f32>().ok(),
                "W" => width = value.trim().parse::<f32>().ok(),
                "H" => height = value.trim().parse::<f32>().ok(),
                _ => {}
            }
        }
        if !captured
            && let (Some(page), Some(h), Some(v), Some(width), Some(height)) =
                (page, h, v, width, height)
        {
            let result = ForwardRequest {
                revision,
                pdf: pdf.clone(),
                page,
                h,
                v,
                width,
                height,
                word: word.clone(),
            };
            match result.validate() {
                Ok(()) => candidates.push(result),
                Err(error) if candidates.is_empty() => return Err(error),
                Err(_) => {}
            }
            captured = true;
        }
    }
    let mut result = candidates
        .first()
        .cloned()
        .ok_or_else(|| io::Error::other("synctex view returned no complete match"))?;
    if let Some(hint) = &word {
        let mut pages = Vec::new();
        for candidate in &candidates {
            if !pages.contains(&candidate.page) {
                pages.push(candidate.page);
            }
        }
        // SyncTeX returns overlay matches in traversal order, which need not
        // be page order. Keep its first result as fallback, then inspect the
        // remaining overlays in the order a reader sees them.
        if pages.len() > 1 {
            pages[1..].sort_unstable();
        }
        if let Some(page) = crate::pdf::first_visible_forward_page(&pdf, &pages, hint, library)
            .map_err(io::Error::other)?
            && let Some(candidate) = candidates.iter().find(|candidate| candidate.page == page)
        {
            result = candidate.clone();
        }
    }
    revision.check(&pdf)?;
    Ok(result)
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

pub fn resolve_inverse(
    pdf: &Path,
    page: u32,
    x: f32,
    y_from_top: f32,
    word: Option<(&str, usize)>,
    radius: u32,
    operation: &Operation,
) -> io::Result<InverseResolution> {
    let pdf = std::path::absolute(pdf)?;
    let spec = format!("{page}:{x:.2}:{y_from_top:.2}:{}", pdf.display());
    let stdout = run(operation, &["edit", "-o", &spec])?;
    let mut target = parse_synctex_edit(&stdout)
        .ok_or_else(|| io::Error::other("synctex edit returned no match"))?;
    let path = Path::new(&target.file);
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        pdf.parent().unwrap_or(Path::new(".")).join(path)
    };
    let mut warning = None;
    let resolved = match fs::canonicalize(&absolute) {
        Ok(path) => path,
        Err(error) => {
            warning = Some(format!(
                "line-only navigation: source path unavailable: {error}"
            ));
            absolute
        }
    };
    target.file = resolved
        .into_os_string()
        .into_string()
        .map_err(|_| io::Error::other("source path is not UTF-8"))?;
    if let Some((context, offset)) = word {
        operation.check()?;
        let source = read_source(Path::new(&target.file));
        if let Err(error) = &source {
            warning = Some(format!(
                "line-only navigation: source refinement unavailable: {error}"
            ));
        }
        if let Ok(mut source) = source {
            if let Some((file, original, anchor)) =
                original_source_from_verbatim(&pdf, Path::new(&target.file), &source, target.line)
            {
                target.file = file;
                target.line = anchor;
                source = original;
            }
            let mut location = source_word_location(&source, target.line, context, offset, radius);
            if let Some((file, original, line, byte, score)) =
                document_metadata_word_location(&pdf, context, offset)
                && (location.is_none() || score >= 6)
            {
                target.file = file;
                source = original;
                location = Some((line, byte));
            }
            if let Some((line, byte)) = location {
                let text = source
                    .lines()
                    .nth(line as usize - 1)
                    .ok_or_else(|| io::Error::other("source line is missing"))?;
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
    }
    operation.check()?;
    Ok(InverseResolution {
        location: target,
        warning,
    })
}

fn read_source(path: &Path) -> io::Result<String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let mut bytes = Vec::new();
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
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

/// A Beamer fragile frame is copied to a `.vrb` file. SyncTeX may name that
/// generated file. Map it back only when distinct source lines agree on one
/// line offset; otherwise keep the original SyncTeX result.
fn original_source_from_verbatim(
    pdf: &Path,
    generated: &Path,
    contents: &str,
    line: u32,
) -> Option<(String, String, u32)> {
    if generated.extension()? != "vrb" || generated.file_stem()? != pdf.file_stem()? {
        return None;
    }
    let original = fs::canonicalize(pdf.with_extension("tex")).ok()?;
    let original_path = original.to_str()?.to_owned();
    let source = read_source(&original).ok()?;
    let mut unique = std::collections::HashMap::new();
    for (index, row) in source.lines().enumerate() {
        let row = row.trim();
        if row.len() >= 24 {
            unique
                .entry(row)
                .and_modify(|position| *position = None)
                .or_insert(Some(index));
        }
    }
    let mut offsets = std::collections::HashMap::<isize, usize>::new();
    for (index, row) in contents.lines().enumerate() {
        if let Some(Some(position)) = unique.get(row.trim()) {
            *offsets
                .entry(*position as isize - index as isize)
                .or_default() += 1;
        }
    }
    let mut ranked: Vec<_> = offsets.into_iter().collect();
    ranked.sort_unstable_by_key(|(_, count)| std::cmp::Reverse(*count));
    let (delta, count) = *ranked.first()?;
    if count < 2 || ranked.get(1).is_some_and(|(_, next)| *next == count) {
        return None;
    }
    let anchor = line.checked_add_signed(delta as i32)?;
    if anchor == 0 || anchor as usize > source.lines().count() {
        return None;
    }
    Some((original_path, source, anchor))
}

/// Title pages and running headers can be produced from declarations far
/// outside SyncTeX's reported frame. Search only literal document metadata,
/// including direct preamble inputs, and require neighboring PDF words.
fn document_metadata_word_location(
    pdf: &Path,
    context: &str,
    offset: usize,
) -> Option<(String, String, u32, usize, isize)> {
    let main = fs::canonicalize(pdf.with_extension("tex")).ok()?;
    let main_source = read_source(&main).ok()?;
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
        let mut path = main.parent()?.join(name);
        if path.extension().is_none() {
            path.set_extension("tex");
        }
        if let Ok(path) = fs::canonicalize(path)
            && !files.iter().any(|(known, _)| *known == path)
            && let Ok(source) = read_source(&path)
        {
            files.push((path, source));
        }
    }
    let mut best = None;
    let mut tied = false;
    for (file_index, (_, source)) in files.iter().enumerate() {
        let mut start = 0;
        for row in source.lines() {
            let visible = source_line_text(row);
            for command in ["\\title", "\\subtitle", "\\author", "\\date", "\\institute"] {
                let Some(at) = visible.find(command) else {
                    continue;
                };
                let Some((body, end)) = math::group(source, start + at + command.len()) else {
                    continue;
                };
                let first = source[..body.start]
                    .bytes()
                    .filter(|ch| *ch == b'\n')
                    .count();
                let last = source[..end].bytes().filter(|ch| *ch == b'\n').count();
                let Some((line, byte, score)) =
                    source_prose_scored_location(source, 1, context, offset, first..last + 1, true)
                else {
                    continue;
                };
                if score < 3 {
                    continue;
                }
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
            start += row.len() + 1;
        }
    }
    let (score, index, line, byte) = best.filter(|_| !tied)?;
    let (path, source) = files.swap_remove(index);
    Some((path.to_str()?.to_owned(), source, line, byte, score))
}

pub(crate) fn parse_synctex_edit(stdout: &str) -> Option<SourceLocation> {
    let (mut input, mut line) = (None, None);
    let mut result = None;
    for row in stdout.lines() {
        if let Some(value) = row.strip_prefix("Input:") {
            input = Some(value.trim().to_owned());
            line = None;
        } else if let Some(value) = row.strip_prefix("Line:") {
            line = value.trim().parse::<u32>().ok().filter(|line| *line > 0);
        }
        if let (Some(file), Some(line)) = (&input, line)
            && !file.is_empty()
        {
            result = Some(SourceLocation {
                file: file.clone(),
                line,
                byte_column: 0,
                column: 1,
                column_char: 1,
                precise: false,
            });
        }
    }
    result
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
    let mut start = None;
    for (index, text) in source.lines().enumerate() {
        let mut text = source_line_text(text);
        while let Some((_, rest)) = text.split_once('\\') {
            text = rest;
            if let Some(rest) = text.strip_prefix('\\') {
                text = rest;
                continue;
            }
            if let Some(rest) = text
                .strip_prefix("begin")
                .and_then(|rest| rest.trim_start().strip_prefix("{frame}"))
            {
                text = rest;
                // Nested frames are not a reliable source boundary.
                if start.replace(index).is_some() {
                    return None;
                }
            } else if let Some(rest) = text
                .strip_prefix("end")
                .and_then(|rest| rest.trim_start().strip_prefix("{frame}"))
            {
                text = rest;
                if let Some(begin) = start.take() {
                    if begin <= anchor && anchor <= index {
                        return Some((begin, index));
                    }
                } else if anchor <= index {
                    return Some((index, index));
                }
            }
        }
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

fn normalized_word(word: &str) -> String {
    word.nfkd()
        .filter(|ch| !is_combining_mark(*ch))
        .collect::<String>()
        .to_lowercase()
        .replace('ﬁ', "fi")
        .replace('ﬂ', "fl")
        .replace('ﬀ', "ff")
        .replace('ﬃ', "ffi")
        .replace('ﬄ', "ffl")
}

fn ordinary_word_source_start(
    source: &str,
    context: &str,
    offset: usize,
    line: u32,
    byte: usize,
) -> Option<usize> {
    let Some((_, pdf_word)) = words(context)
        .into_iter()
        .find(|(start, word)| *start <= offset && offset < start + word.len())
    else {
        return Some(byte);
    };
    let pdf_word = normalized_word(pdf_word);
    if pdf_word.len() < 2 || !pdf_word.bytes().all(|ch| ch.is_ascii_alphabetic()) {
        return Some(byte);
    }
    source
        .lines()
        .nth(line as usize - 1)
        .and_then(|text| {
            words(text)
                .into_iter()
                .find(|(start, word)| *start <= byte && byte < start + word.len())
        })
        .and_then(|(start, word)| (normalized_word(word) == pdf_word).then_some(start))
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
    let prose =
        source_prose_scored_location(source, line, context, offset, lines.clone(), within_scope);
    math::source_location(source, line, lines, within_scope, context, offset, prose).and_then(
        |(row, byte)| {
            ordinary_word_source_start(source, context, offset, row, byte).map(|start| (row, start))
        },
    )
}

fn source_prose_scored_location(
    source: &str,
    line: u32,
    context: &str,
    offset: usize,
    lines: std::ops::Range<usize>,
    within_frame: bool,
) -> Option<(u32, usize, isize)> {
    let pdf = words(context);
    let selected = pdf
        .iter()
        .position(|(start, word)| *start <= offset && offset < start + word.len())?;
    let pdf: Vec<_> = pdf.iter().map(|(_, word)| normalized_word(word)).collect();
    let frame_header = within_frame
        && source
            .lines()
            .nth(lines.start)
            .is_some_and(|row| source_line_text(row).contains("\\begin{frame}"));
    let mut candidates = Vec::new();
    for (index, text) in source.lines().enumerate().take(lines.end).skip(lines.start) {
        let text = source_line_text(text);
        let graphics: Vec<_> = text
            .match_indices("\\includegraphics")
            .filter_map(|(start, _)| {
                let mut end = start + "\\includegraphics".len();
                end += text[end..].len() - text[end..].trim_start().len();
                if text[end..].starts_with('*') {
                    end += 1;
                }
                if text[end..].starts_with('[') {
                    end += text[end..].find(']')? + 1;
                }
                math::group(text, end).map(|(_, end)| start..end)
            })
            .collect();
        for (byte, word) in words(text) {
            if (byte == 0 || !text[..byte].ends_with('\\'))
                && !graphics.iter().any(|range| range.contains(&byte))
            {
                candidates.push((index as u32 + 1, byte, normalized_word(word)));
            }
        }
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
            source_prose_scored_location(source, 6, context, 0, 0..6, true)
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
        let source = "\\begin{frame}{An example from \\v Cech's construction}\n\
                      $x$\n\
                      \\end{frame}";
        let context = "An example from Čech’s construction";
        assert_eq!(
            source_word_location(source, 3, context, context.find('Č').unwrap(), 4),
            Some((1, source.lines().next().unwrap().find("Cech").unwrap()))
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
    fn inverse_generated_verbatim_maps_to_original_only_with_unique_alignment() {
        let directory = tempfile::tempdir().unwrap();
        let pdf = directory.path().join("slides.pdf");
        let source = "\\begin{frame}[fragile]{A title}\n\
                      Distinct first sentence in the original frame.\n\
                      Distinct second sentence in the original frame.\n\
                      \\end{frame}\n";
        fs::write(pdf.with_extension("tex"), source).unwrap();
        let generated = directory.path().join("build/slides.vrb");
        fs::create_dir(generated.parent().unwrap()).unwrap();
        let contents = "\\frametitle{A title}\n\
                        Distinct first sentence in the original frame.\n\
                        Distinct second sentence in the original frame.\n";
        assert_eq!(
            original_source_from_verbatim(&pdf, &generated, contents, 2),
            Some((
                fs::canonicalize(pdf.with_extension("tex"))
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned(),
                source.to_owned(),
                2
            ))
        );
        assert!(
            original_source_from_verbatim(&pdf, &generated, contents.lines().next().unwrap(), 1)
                .is_none()
        );
    }

    #[test]
    fn inverse_document_metadata_resolves_title_and_direct_input() {
        let directory = tempfile::tempdir().unwrap();
        let pdf = directory.path().join("slides.pdf");
        let main = "\\input{preamble.tex}\n\
                    \\title{Example Report and Results}\n\
                    \\author{Ada Example}\n\
                    \\begin{document}\n\
                    \\begin{frame}{Other title}\n\
                    Body text.\n\
                    \\end{frame}\n";
        fs::write(pdf.with_extension("tex"), main).unwrap();
        fs::write(
            directory.path().join("preamble.tex"),
            "\\institute{North Research Center\\\\\nExample School for Mathematics}\n",
        )
        .unwrap();
        let found =
            document_metadata_word_location(&pdf, "Example Report and Results", "Example ".len())
                .unwrap();
        assert_eq!((found.2, found.3), (2, 15));
        assert!(found.4 >= 6);
        let found = document_metadata_word_location(
            &pdf,
            "North Research Center\r\nExample School for Mathematics",
            "North Research Center\r\nExample School for ".len(),
        )
        .unwrap();
        assert!(found.0.ends_with("preamble.tex"));
        assert_eq!((found.2, found.3), (2, 19));
        assert!(document_metadata_word_location(&pdf, "Report", 0).is_none());
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
            cloud vertices at exact quotient distance at most \\(\\rho_q\\) from the\n\
            exact reference segment.\n\n\n\n\n\
            The transverse coordinate is in units of \\(\\rho_q\\), hence magnified.\n\
            End of caption.}\n";
        let line = source.lines().count() as u32;
        for (context, row) in [
            ("distance at most 𝜌𝑞 from the exact reference segment.", 2),
            ("coordinate is in units of 𝜌𝑞, hence magnified.", 8),
        ] {
            assert_eq!(
                source_word_location(source, line, context, context.find('𝜌').unwrap(), 4),
                Some((
                    row,
                    source
                        .lines()
                        .nth(row as usize - 1)
                        .unwrap()
                        .find("\\rho")
                        .unwrap()
                ))
            );
        }
        // The glyph alone cannot identify which repeated expression was clicked.
        assert_eq!(source_word_location(source, line, "𝜌𝑞", 0, 4), None);
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

    #[test]
    fn parse_synctex_edit_finds_file_and_line() {
        let stdout = "Output: paper.pdf\nInput: /private/tmp/synctex-test/./paper.tex\nLine: 3\nColumn: -1\n";
        let target = parse_synctex_edit(stdout).expect("synctex match");
        assert_eq!(target.file, "/private/tmp/synctex-test/./paper.tex");
        assert_eq!(target.line, 3);
    }

    #[test]
    fn parse_synctex_edit_takes_last_input() {
        let stdout = "Input: preamble.tex\nLine: 9\nInput: paper.tex\nLine: 3\n";
        let target = parse_synctex_edit(stdout).expect("synctex match");
        assert_eq!(target.file, "paper.tex");
        assert_eq!(target.line, 3);
    }

    #[test]
    fn parse_synctex_edit_rejects_missing_input() {
        assert!(parse_synctex_edit("Output: paper.pdf\nLine: 3\n").is_none());
        assert!(parse_synctex_edit("Output: paper.pdf\nInput: paper.tex\n").is_none());
        assert!(parse_synctex_edit("").is_none());
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
}
