mod math;

use crate::pdf::SearchRect;
use serde::{Deserialize, Serialize};
use std::{fs, io, os::unix::fs::MetadataExt, path::Path, process::Command};

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

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ForwardRequest {
    pub pdf: std::path::PathBuf,
    pub revision: PdfRevision,
    pub page: u32,
    pub h: f32,
    pub v: f32,
    pub width: f32,
    pub height: f32,
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
    if line == 0 || column == 0 {
        return Err(io::Error::other("source line and column must be positive"));
    }
    let pdf = fs::canonicalize(pdf)?;
    let file = fs::canonicalize(file)?;
    let revision = PdfRevision::read(&pdf)?;
    let spec = format!(
        "{line}:{column}:{}",
        file.to_str()
            .ok_or_else(|| io::Error::other("source path is not UTF-8"))?
    );
    let stdout = run(&[
        "view",
        "-i",
        &spec,
        "-o",
        pdf.to_str()
            .ok_or_else(|| io::Error::other("PDF path is not UTF-8"))?,
    ])?;
    let (mut page, mut h, mut v, mut width, mut height) = (None, None, None, None, None);
    for row in stdout.lines() {
        if let Some((key, value)) = row.split_once(':') {
            match key {
                "Page" => {
                    page = value.trim().parse::<u32>().ok();
                    h = None;
                    v = None;
                    width = None;
                    height = None;
                }
                "h" => h = value.trim().parse::<f32>().ok(),
                "v" => v = value.trim().parse::<f32>().ok(),
                "W" => width = value.trim().parse::<f32>().ok(),
                "H" => height = value.trim().parse::<f32>().ok(),
                _ => {}
            }
        }
        if let (Some(page), Some(h), Some(v), Some(width), Some(height)) =
            (page, h, v, width, height)
        {
            let result = ForwardRequest {
                revision,
                pdf,
                page,
                h,
                v,
                width,
                height,
            };
            result.validate()?;
            revision.check(&result.pdf)?;
            return Ok(result);
        }
    }
    Err(io::Error::other("synctex view returned no complete match"))
}

fn run(args: &[&str]) -> io::Result<String> {
    let output = Command::new("synctex").args(args).output()?;
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
) -> io::Result<SourceLocation> {
    let spec = format!("{page}:{x:.2}:{y_from_top:.2}:{}", pdf.display());
    let stdout = run(&["edit", "-o", &spec])?;
    let mut target = parse_synctex_edit(&stdout)
        .ok_or_else(|| io::Error::other("synctex edit returned no match"))?;
    let path = Path::new(&target.file);
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        pdf.parent().unwrap_or(Path::new(".")).join(path)
    };
    target.file = fs::canonicalize(absolute)?
        .into_os_string()
        .into_string()
        .map_err(|_| io::Error::other("source path is not UTF-8"))?;
    if let Some((context, offset)) = word {
        let source = fs::read_to_string(&target.file)?;
        if let Some((line, byte)) =
            source_word_location(&source, target.line, context, offset, radius)
        {
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
    Ok(target)
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

/// Find a literal frame environment enclosing the one-based SyncTeX line.
/// Beamer often attributes every overlay's text to the closing frame line.
fn source_frame_range(source: &str, line: u32) -> Option<std::ops::Range<usize>> {
    let anchor = line.checked_sub(1)? as usize;
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
                if let Some(start) = start.take()
                    && start <= anchor
                    && anchor <= index
                {
                    return Some(start..index + 1);
                }
            }
        }
        if index >= anchor && start.is_none() {
            return None;
        }
    }
    None
}

/// Refine prose and mathematical atoms without expanding arbitrary TeX macros.
fn source_word_location(
    source: &str,
    line: u32,
    context: &str,
    offset: usize,
    radius: u32,
) -> Option<(u32, usize)> {
    let frame = source_frame_range(source, line);
    let within_frame = frame.is_some();
    let lines = frame.unwrap_or_else(|| {
        line.saturating_sub(radius.saturating_add(1)) as usize
            ..(line as usize).saturating_add(radius as usize)
    });
    let prose = source_prose_location(source, line, context, offset, lines.clone(), within_frame);
    math::source_location(source, line, lines, within_frame, context, offset, prose)
}

fn source_prose_location(
    source: &str,
    line: u32,
    context: &str,
    offset: usize,
    lines: std::ops::Range<usize>,
    within_frame: bool,
) -> Option<(u32, usize)> {
    fn words(text: &str) -> Vec<(usize, &str)> {
        let mut result = Vec::new();
        let mut start = None;
        for (index, ch) in text
            .char_indices()
            .chain(std::iter::once((text.len(), ' ')))
        {
            if ch.is_alphanumeric() {
                start.get_or_insert(index);
            } else if let Some(start) = start.take() {
                result.push((start, &text[start..index]));
            }
        }
        result
    }
    fn normalized(word: &str) -> String {
        word.to_lowercase()
            .replace('ﬁ', "fi")
            .replace('ﬂ', "fl")
            .replace('ﬀ', "ff")
            .replace('ﬃ', "ffi")
            .replace('ﬄ', "ffl")
    }
    let pdf = words(context);
    let selected = pdf
        .iter()
        .position(|(start, word)| *start <= offset && offset < start + word.len())?;
    let pdf: Vec<_> = pdf.iter().map(|(_, word)| normalized(word)).collect();
    let mut candidates = Vec::new();
    for (index, text) in source.lines().enumerate().take(lines.end).skip(lines.start) {
        let text = source_line_text(text);
        for (byte, word) in words(text) {
            if byte == 0 || !text[..byte].ends_with('\\') {
                candidates.push((index as u32 + 1, byte, normalized(word)));
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
        // A Beamer boundary is not evidence that the last occurrence is best.
        let proximity = if within_frame { 0 } else { row.abs_diff(line) };
        let rank = (score, std::cmp::Reverse(proximity));
        match best {
            Some((old, _, _)) if rank < old => {}
            Some((old, _, _)) if rank == old => tied = true,
            _ => {
                best = Some((rank, *row, *byte));
                tied = false;
            }
        }
    }
    best.filter(|_| !tied).map(|(_, row, byte)| (row, byte))
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
        assert_eq!(source_word_location(source, 10, "datasets", 1, 4), None);
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
        ] {
            assert!(parse_forward_request(&invalid).is_err(), "{invalid}");
        }
    }
}
