use crate::pdf::SearchRect;
use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path, process::Command};

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

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ForwardRequest {
    pub pdf: std::path::PathBuf,
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
                pdf,
                page,
                h,
                v,
                width,
                height,
            };
            result.validate()?;
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
/// Resolve a PDF word near SyncTeX's source line, using neighboring words to
/// disambiguate repeats. Columns are zero-based UTF-8 byte offsets.
/// ponytail: prose matching, not a TeX expander; macros may remain line-only.
fn source_word_location(
    source: &str,
    line: u32,
    context: &str,
    offset: usize,
    radius: u32,
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
    let start_line = line.saturating_sub(radius + 1) as usize;
    for (index, text) in source
        .lines()
        .enumerate()
        .take(line as usize + radius as usize)
        .skip(start_line)
    {
        let text = text.split('%').next().unwrap_or("");
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
        let rank = (score, std::cmp::Reverse(row.abs_diff(line)));
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
        let payload = r#"{"pdf":"/a:b.pdf","page":2,"h":72,"v":120,"width":250,"height":12}"#;
        let request = parse_forward_request(payload).unwrap();
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
