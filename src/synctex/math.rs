//! Conservative lexical matching, not a TeX macro expander.
use latex2mathml::token::Token;
use std::ops::Range;
use unicode_normalization::UnicodeNormalization;

#[derive(Debug)]
struct Atom {
    value: char,
    span: Range<usize>,
}

fn canonical(value: char) -> impl Iterator<Item = char> {
    // TeX's literal hyphen and a PDF mathematical minus denote the same operator.
    std::iter::once(if value == '−' { '-' } else { value }).nfkc()
}

fn append(atoms: &mut Vec<Atom>, value: char, span: Range<usize>) {
    atoms.extend(
        canonical(value)
            .filter(|c| !c.is_whitespace())
            .map(|value| Atom {
                value,
                span: span.clone(),
            }),
    );
}

pub(super) fn command(source: &str, start: usize) -> (&str, usize) {
    let after = start + 1;
    let end = if source
        .as_bytes()
        .get(after)
        .is_some_and(u8::is_ascii_alphabetic)
    {
        after
            + source[after..]
                .bytes()
                .take_while(u8::is_ascii_alphabetic)
                .count()
    } else {
        after + source[after..].chars().next().map_or(0, char::len_utf8)
    };
    (&source[after..end], end)
}

pub(super) fn group(source: &str, start: usize) -> Option<(Range<usize>, usize)> {
    let start = super::skip_source_space(source, start);
    if source.as_bytes().get(start) != Some(&b'{') {
        return None;
    }
    let mut depth = 1;
    let mut i = start + 1;
    while i < source.len() {
        match source.as_bytes()[i] {
            b'\\' => {
                i = command(source, i).1;
                continue;
            }
            b'%' => {
                i += source[i..].find('\n').unwrap_or(source.len() - i);
                continue;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((start + 1..i, i + 1));
                }
            }
            _ => {}
        }
        i += source[i..].chars().next()?.len_utf8();
    }
    None
}

/// Return the opaque command's end and whether its entire span is nonprinting.
pub(super) fn opaque_command_end(source: &str, name: &str, mut at: usize) -> Option<(usize, bool)> {
    match name {
        "verb" => {
            at += usize::from(source.as_bytes().get(at) == Some(&b'*'));
            let delimiter = source[at..].chars().next()?;
            at += delimiter.len_utf8();
            Some((
                at + source[at..]
                    .find(delimiter)
                    .map_or(source.len() - at, |n| n + delimiter.len_utf8()),
                false,
            ))
        }
        "begin" => {
            let (body, end) = group(source, super::skip_source_space(source, at))?;
            let environment = &source[body];
            if !matches!(
                environment.trim_end_matches('*'),
                "verbatim" | "Verbatim" | "lstlisting" | "minted" | "comment"
            ) {
                return None;
            }
            if environment.trim_end_matches('*') == "comment" {
                // comment.sty compares entire subsequent input lines, not
                // substrings; TeX removes trailing ASCII spaces on input.
                let Some(newline) = source[end..].find('\n') else {
                    return Some((source.len(), true));
                };
                let mut at = end + newline + 1;
                for row in source[at..].split_inclusive('\n') {
                    let text = row.strip_suffix('\n').unwrap_or(row);
                    let text = text
                        .strip_suffix('\r')
                        .unwrap_or(text)
                        .trim_end_matches(' ');
                    at += row.len();
                    if text
                        .strip_prefix("\\end{")
                        .and_then(|text| text.strip_suffix('}'))
                        == Some(environment)
                    {
                        return Some((at, true));
                    }
                }
                return Some((source.len(), true));
            }
            if matches!(environment.trim_end_matches('*'), "Verbatim" | "minted") {
                // Fancyvrb checks only the first \end{ marker on each subsequent line.
                let Some(newline) = source[end..].find('\n') else {
                    return Some((source.len(), false));
                };
                let mut at = end + newline + 1;
                for row in source[at..].split_inclusive('\n') {
                    at += row.len();
                    if row
                        .find("\\end{")
                        .and_then(|offset| row[offset + 5..].strip_prefix(environment))
                        .is_some_and(|rest| rest.starts_with('}'))
                    {
                        return Some((at, false));
                    }
                }
                return Some((source.len(), false));
            }
            Some((
                source[end..]
                    .match_indices("\\end{")
                    .find_map(|(offset, _)| {
                        source[end + offset + 5..]
                            .strip_prefix(environment)?
                            .strip_prefix('}')?;
                        Some(end + offset + 5 + environment.len() + 1)
                    })
                    .unwrap_or(source.len()),
                false,
            ))
        }
        _ => None,
    }
}

fn caption_argument(source: &str, mut at: usize) -> Option<(Range<usize>, usize)> {
    at += usize::from(source.as_bytes().get(at) == Some(&b'*'));
    at = super::skip_source_space(source, at);
    if source.as_bytes().get(at) == Some(&b'[') {
        at = super::optional_argument_end(source, at)?;
    }
    group(source, super::skip_source_space(source, at))
}

/// Find a complete long caption argument owning this literal cursor byte.
pub(super) fn caption_argument_at(source: &str, byte: usize) -> Option<Range<usize>> {
    if byte >= source.len() || !source.is_char_boundary(byte) {
        return None;
    }
    let mut hidden = super::nonprinting_source_spans(source).peekable();
    let mut body = None;
    let mut at = 0;
    while at <= byte {
        while hidden.peek().is_some_and(|(_, range)| range.end <= at) {
            hidden.next();
        }
        if let Some((start, range)) = hidden.peek()
            && *start <= at
        {
            at = range.end;
            if byte < at {
                return None;
            }
            continue;
        }
        match source.as_bytes()[at] {
            b'%' => {
                at += source[at..].find('\n').unwrap_or(source.len() - at);
                if byte < at {
                    return None;
                }
            }
            b'\\' => {
                let (name, end) = command(source, at);
                at = end;
                if let Some((end, _)) = opaque_command_end(source, name, at) {
                    at = end;
                    if byte < at {
                        return None;
                    }
                } else if name == "caption"
                    && let Some((argument, _)) = caption_argument(source, at)
                {
                    if argument.contains(&byte) {
                        at = argument.start;
                        body = Some(argument);
                    } else {
                        at = argument.end + 1;
                    }
                }
            }
            _ => {
                at += source[at..].chars().next()?.len_utf8();
            }
        }
    }
    body
}

pub(super) fn math_environment(name: &str) -> bool {
    matches!(
        name.trim_end_matches('*'),
        "math"
            | "displaymath"
            | "equation"
            | "align"
            | "aligned"
            | "alignat"
            | "flalign"
            | "gather"
            | "gathered"
            | "multline"
            | "eqnarray"
            | "matrix"
            | "pmatrix"
            | "bmatrix"
            | "Bmatrix"
            | "vmatrix"
            | "Vmatrix"
            | "smallmatrix"
            | "cases"
            | "split"
    )
}

/// Only closed, literal math regions qualify; escaped dollars and comments do not.
fn regions(source: &str, limit: usize) -> Vec<Range<usize>> {
    let mut result = Vec::new();
    let mut open: Option<(&str, usize)> = None;
    let mut i = 0;
    while i < source.len() {
        if open.is_none() && i >= limit {
            break;
        }
        let start = i;
        let ch = source[i..].chars().next().unwrap();
        i += ch.len_utf8();
        match ch {
            '%' => i += source[i..].find('\n').unwrap_or(source.len() - i),
            '$' => {
                let delimiter = if source[i..].starts_with('$') {
                    i += 1;
                    "$$"
                } else {
                    "$"
                };
                match open {
                    None => open = Some((delimiter, i)),
                    Some((expected, begin)) if expected == delimiter => {
                        result.push(begin..start);
                        open = None;
                    }
                    _ => {}
                }
            }
            '\\' => {
                let (name, end) = command(source, start);
                i = end;
                if open.is_none()
                    && let Some((end, _)) = opaque_command_end(source, name, i)
                {
                    i = end;
                    continue;
                }
                match (name, open) {
                    ("(" | "[", None) => open = Some((if name == "(" { ")" } else { "]" }, i)),
                    (")" | "]", Some((expected, begin))) if name == expected => {
                        result.push(begin..start);
                        open = None;
                    }
                    ("begin" | "end", _) => {
                        if let Some((span, end)) = group(source, i) {
                            let environment = &source[span];
                            i = end;
                            if name == "begin" && open.is_none() && math_environment(environment) {
                                open = Some((environment, start));
                            } else if let Some((expected, begin)) = open
                                && name == "end"
                                && expected == environment
                            {
                                result.push(begin..start);
                                open = None;
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    result
}

/// Captions are collected before expansion, so SyncTeX can report the closing
/// line for every word. Restrict refinement to that complete literal argument.
pub(super) fn caption_lines(source: &str, line: u32) -> Option<Range<usize>> {
    let target = line.checked_sub(1)? as usize;
    let mut hidden = super::nonprinting_source_spans(source).peekable();
    let mut i = 0;
    let mut counted_to = 0;
    let mut counted_lines = 0;
    while i < source.len() {
        while hidden.peek().is_some_and(|(_, range)| range.end <= i) {
            hidden.next();
        }
        if let Some((start, range)) = hidden.peek()
            && *start <= i
        {
            i = range.end;
            continue;
        }
        let ch = source[i..].chars().next()?;
        if ch == '%' {
            i += source[i..].find('\n').unwrap_or(source.len() - i);
        } else if ch == '\\' {
            let (name, end) = command(source, i);
            i = end;
            if let Some((end, _)) = opaque_command_end(source, name, i) {
                i = end;
            } else if name == "caption"
                && let Some((body, end)) = caption_argument(source, i)
            {
                let first = counted_lines
                    + source[counted_to..body.start]
                        .bytes()
                        .filter(|c| *c == b'\n')
                        .count();
                let last = first
                    + source[body.start..end]
                        .bytes()
                        .filter(|c| *c == b'\n')
                        .count();
                counted_to = end;
                counted_lines = last;
                if (first..=last).contains(&target) {
                    return Some(first..last + 1);
                }
                if first > target {
                    return None;
                }
                i = end;
            }
        } else {
            i += ch.len_utf8();
        }
    }
    None
}

/// Literal prose immediately beside inline math is evidence for which
/// occurrence was clicked. Stop at markup, comments, and blank-line
/// paragraph boundaries; never invent a TeX expansion.
fn inline_context(source: &str, range: &Range<usize>, atoms: Vec<Atom>) -> Vec<Atom> {
    let (before, after) =
        if source[..range.start].ends_with("\\(") && source[range.end..].starts_with("\\)") {
            (range.start - 2, range.end + 2)
        } else if source[..range.start].ends_with('$')
            && !source[..range.start].ends_with("$$")
            && source[range.end..].starts_with('$')
            && !source[range.end..].starts_with("$$")
        {
            (range.start - 1, range.end + 1)
        } else {
            return atoms;
        };
    let literal = |c: char| {
        !matches!(
            c,
            '\\' | '$' | '{' | '}' | '%' | '^' | '_' | '&' | '~' | '\n' | '\r'
        )
    };
    let mut prefix = Vec::with_capacity(8);
    let mut cursor = before;
    'backward: while prefix.len() < 8 {
        let line_start = source[..cursor].rfind('\n').map_or(0, |p| p + 1);
        for (i, c) in source[line_start..cursor].char_indices().rev() {
            if c == '\r' {
                continue;
            }
            if !literal(c) {
                break 'backward;
            }
            if !c.is_whitespace() {
                prefix.push((line_start + i, c));
                if prefix.len() == 8 {
                    break 'backward;
                }
            }
        }
        if line_start == 0 {
            break;
        }
        let nl = line_start - 1;
        let prev_start = source[..nl].rfind('\n').map_or(0, |p| p + 1);
        let prev_line = &source[prev_start..nl];
        if prev_line.trim().is_empty() || prev_line.contains('%') {
            break;
        }
        cursor = nl;
    }
    let mut result = Vec::with_capacity(atoms.len() + 16);
    for (i, c) in prefix.into_iter().rev() {
        append(&mut result, c, i..i + c.len_utf8());
    }
    result.extend(atoms);
    let mut suffix_count = 0;
    let mut cursor = after;
    'forward: while suffix_count < 8 && cursor < source.len() {
        let line_end = source[cursor..]
            .find('\n')
            .map_or(source.len(), |p| cursor + p);
        for (i, c) in source[cursor..line_end].char_indices() {
            if c == '\r' {
                continue;
            }
            if !literal(c) {
                break 'forward;
            }
            if !c.is_whitespace() {
                append(&mut result, c, cursor + i..cursor + i + c.len_utf8());
                suffix_count += 1;
                if suffix_count == 8 {
                    break 'forward;
                }
            }
        }
        if line_end >= source.len() {
            break;
        }
        let next_start = line_end + 1;
        let next_end = source[next_start..]
            .find('\n')
            .map_or(source.len(), |p| next_start + p);
        if source[next_start..next_end].trim().is_empty() {
            break;
        }
        cursor = next_start;
    }
    result
}

/// An opaque expression can render glyphs absent from its lexical atoms, or
/// change their extraction order. Its math atoms cannot prove a location.
fn atoms(source: &str, range: Range<usize>) -> Option<(Vec<Atom>, bool)> {
    let mut result = Vec::new();
    let mut opaque = false;
    let mut i = range.start;
    while i < range.end {
        let start = i;
        let ch = source[i..].chars().next()?;
        i += ch.len_utf8();
        match ch {
            '%' => i += source[i..range.end].find('\n').unwrap_or(range.end - i),
            '{' | '}' | '&' => {}
            '_' | '^' => {
                // A single script has a linear base/script order. Simultaneous
                // scripts on the same base can extract in visual rather than source order.
                let arg = super::skip_source_space(source, i).min(range.end);
                let arg_end = match source.as_bytes().get(arg) {
                    Some(b'{') => group(source, arg).map(|(_, end)| end),
                    Some(b'\\') => Some(command(source, arg).1),
                    Some(_) => source[arg..range.end]
                        .chars()
                        .next()
                        .map(|c| arg + c.len_utf8()),
                    None => None,
                };
                opaque |= arg_end.is_none_or(|end| {
                    let after = super::skip_source_space(source, end).min(range.end);
                    source[after..range.end].starts_with(['_', '^'])
                });
            }
            '\\' => {
                let (name, end) = command(source, start);
                i = end;
                let span = start..end;
                if name == "label" {
                    i = group(source, i)?.1;
                    continue;
                }
                if name == "begin" {
                    let (environment, end) = group(source, i)?;
                    let environment = &source[environment];
                    if !math_environment(environment) {
                        opaque = true;
                        append(&mut result, '\0', span);
                    }
                    i = end;
                    if environment.trim_end_matches('*') == "alignat" {
                        i = group(source, i)?.1;
                    }
                    continue;
                }
                if name == "end" {
                    let (environment, end) = group(source, i)?;
                    opaque |= !math_environment(&source[environment]);
                    i = end;
                    continue;
                }
                // These standard wrappers change appearance, not character identity.
                if matches!(
                    name,
                    "mathcal"
                        | "boxed"
                        | "mathnormal"
                        | "textnormal"
                        | "displaystyle"
                        | "textstyle"
                        | "scriptstyle"
                        | "scriptscriptstyle"
                        | "limits"
                        | "nolimits"
                ) {
                    continue;
                }
                match Token::from_command(name) {
                    Token::Letter(c, _)
                    | Token::Operator(c)
                    | Token::BigOp(c)
                    | Token::Integral(c) => append(&mut result, c, span),
                    Token::Function(text) | Token::Lim(text) => {
                        for c in text.chars() {
                            append(&mut result, c, span.clone());
                        }
                    }
                    Token::Paren(text) if !text.starts_with('&') => {
                        for c in text.chars() {
                            append(&mut result, c, span.clone());
                        }
                    }
                    Token::Sqrt => {
                        // Optional root indices change PDF extraction order and
                        // contain non-rendered brackets; do not treat them as text.
                        opaque |= source[i..range.end].trim_start().starts_with('[');
                        append(&mut result, '√', span);
                    }
                    Token::Left | Token::Right | Token::Middle | Token::Big(_) => {
                        let following = source[i..range.end].trim_start();
                        if following.starts_with('.') {
                            i = range.end - following.len() + 1;
                        }
                    }
                    Token::Style(_)
                    | Token::Space(_)
                    | Token::Frac
                    | Token::Text
                    | Token::OperatorName
                    | Token::Over(_, _)
                    | Token::Under(_, _)
                    | Token::NewLine => {}
                    _ => {
                        opaque = true;
                        append(&mut result, '\0', span);
                        while let Some((_, end)) = group(source, i) {
                            i = end;
                        }
                    }
                }
            }
            c if !c.is_whitespace() => append(&mut result, c, start..i),
            _ => {}
        }
    }
    Some((result, opaque))
}

fn private_use(c: char) -> bool {
    matches!(c as u32, 0xe000..=0xf8ff | 0xf0000..=0xffffd | 0x100000..=0x10fffd)
}

pub(super) fn source_location(
    source: &str,
    line: u32,
    lines: Range<usize>,
    within_frame: bool,
    context: &str,
    offset: usize,
    prose: Option<(u32, usize, isize)>,
) -> Option<(u32, usize)> {
    let starts: Vec<_> = std::iter::once(0)
        .chain(source.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let limit = if let Some((row, _, _)) = prose {
        (*starts.get(lines.end).unwrap_or(&source.len()))
            .max(*starts.get(row as usize).unwrap_or(&source.len()))
    } else {
        *starts.get(lines.end).unwrap_or(&source.len())
    };
    let regions = regions(source, limit);
    let prose = prose.filter(|(row, byte, _)| {
        let index = starts[*row as usize - 1] + byte;
        !regions.iter().any(|range| range.contains(&index))
    });
    let mut pdf = Vec::new();
    for (i, c) in context.char_indices() {
        append(&mut pdf, c, i..i + c.len_utf8());
    }
    let Some(selected) = pdf.iter().position(|atom| atom.span.contains(&offset)) else {
        return prose.map(|(row, byte, _)| (row, byte));
    };
    if pdf[selected].value == '\0' || private_use(pdf[selected].value) {
        return None;
    }
    // A letter inside an ordinary PDF word must match that entire word, not a
    // nearby single-letter variable with the same spelling.
    let mut word = selected..selected + 1;
    if pdf[selected].value.is_alphanumeric() {
        while word.start > 0
            && pdf[word.start - 1].value.is_alphanumeric()
            && pdf[word.start - 1].span.end == pdf[word.start].span.start
        {
            word.start -= 1;
        }
        while word.end < pdf.len()
            && pdf[word.end].value.is_alphanumeric()
            && pdf[word.end - 1].span.end == pdf[word.end].span.start
        {
            word.end += 1;
        }
    }
    let mut best = None;
    let mut tied = false;
    let mut saw_opaque = false;
    for range in regions {
        if range.start >= *starts.get(lines.end).unwrap_or(&source.len())
            || range.end <= *starts.get(lines.start).unwrap_or(&source.len())
        {
            continue;
        }
        let (atoms, opaque) = atoms(source, range.clone())?;
        if opaque {
            saw_opaque = true;
            continue;
        }
        let atoms = inline_context(source, &range, atoms);
        for (index, atom) in atoms.iter().enumerate() {
            if !range.contains(&atom.span.start) || atom.value != pdf[selected].value {
                continue;
            }
            let row = starts.partition_point(|start| *start <= atom.span.start);
            if !lines.contains(&(row - 1)) {
                continue;
            }
            let Some(begin) = index.checked_sub(selected - word.start) else {
                continue;
            };
            if atoms
                .get(begin..begin + word.len())
                .is_none_or(|source_word| {
                    !source_word
                        .iter()
                        .zip(&pdf[word.clone()])
                        .all(|(a, b)| a.value == b.value)
                })
            {
                continue;
            }
            // A PDF word cannot be a substring of a longer source word.
            if pdf[selected].value.is_alphanumeric()
                && (begin
                    .checked_sub(1)
                    .and_then(|i| atoms.get(i))
                    .is_some_and(|previous| {
                        previous.value.is_alphanumeric()
                            && previous.span.end == atoms[begin].span.start
                    })
                    || atoms.get(begin + word.len()).is_some_and(|next| {
                        next.value.is_alphanumeric()
                            && atoms[begin + word.len() - 1].span.end == next.span.start
                    }))
            {
                continue;
            }
            // Stop at the first mismatch and at this expression's boundary.
            // Context from a different formula must not break an ambiguity tie.
            let mut score = 0;
            let mut lexical_score = 0;
            let mut contradictions = 0usize;
            for direction in [-1isize, 1] {
                let source_edge = if direction < 0 {
                    begin
                } else {
                    begin + word.len() - 1
                };
                let pdf_edge = if direction < 0 {
                    word.start
                } else {
                    word.end - 1
                };
                for distance in 1..=8 {
                    let delta = direction * distance;
                    match (
                        source_edge
                            .checked_add_signed(delta)
                            .and_then(|i| atoms.get(i)),
                        pdf_edge.checked_add_signed(delta).and_then(|i| pdf.get(i)),
                    ) {
                        (Some(a), Some(b)) if a.value == b.value => {
                            score += 9 - distance;
                            lexical_score += usize::from(a.value.is_alphanumeric());
                        }
                        (Some(_), Some(_)) if distance == 1 => {
                            contradictions += 1;
                            break;
                        }
                        _ => break,
                    }
                }
            }
            // A standalone punctuation match is not evidence that the clicked
            // glyph belongs to this formula rather than nearby prose.
            if !pdf[selected].value.is_alphanumeric() && score == 0 {
                continue;
            }
            let location = (row as u32, atom.span.start - starts[row - 1]);
            let proximity = if within_frame {
                0
            } else {
                location.0.abs_diff(line)
            };
            let rank = (
                score,
                lexical_score > 0,
                std::cmp::Reverse(contradictions),
                std::cmp::Reverse(proximity),
                lexical_score,
            );
            match best {
                Some((old, _)) if rank < old => {}
                Some((old, previous)) if rank == old => tied |= previous != location,
                _ => {
                    best = Some((rank, location));
                    tied = false;
                }
            }
        }
    }
    let known_math = best.is_some();
    let mathematical = best.filter(|_| !tied).map(|(_, location)| location);
    let prose_evidence = prose.is_some_and(|(_, _, score)| score >= 6)
        && best.is_some_and(|((_, has_lexical, _, _, _), _)| !has_lexical);
    if prose_evidence {
        return prose.map(|(row, byte, _)| (row, byte));
    }
    // Opaque math leaves symbols uncertain. A complete literal word still
    // identifies its known source span when no second known match competes.
    if saw_opaque {
        let literal_word = word.len() > 1
            && pdf[word.clone()]
                .iter()
                .all(|atom| atom.value.is_alphabetic());
        if prose.is_some() && known_math {
            return None;
        }
        let prose = prose.filter(|(row, _, score)| {
            !known_math
                && (*score >= 3
                    || (literal_word
                        || source
                            .lines()
                            .nth(*row as usize - 1)
                            .is_some_and(|line| line.contains("\\begin{frame}")))
                        && pdf[word.clone()]
                            .iter()
                            .all(|atom| atom.value.is_alphabetic()))
        });
        let mathematical = mathematical.filter(|_| literal_word);
        return prose.map(|(row, byte, _)| (row, byte)).or(mathematical);
    }
    match (prose, mathematical) {
        (Some(_), Some(_)) => None,
        (prose, mathematical) => prose.map(|(row, byte, _)| (row, byte)).or(mathematical),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caption_scopes_preserve_lines_after_skipped_captions() {
        let source = "prefix\n% ignored\n\\caption{\nfirst\n}\ntext\n\\caption*\n{\nsecond\n}";
        assert_eq!(caption_lines(source, 4), Some(2..5));
        assert_eq!(caption_lines(source, 9), Some(7..10));
        assert_eq!(caption_lines(source, 6), None);
        assert_eq!(caption_lines(source, 0), None);
        let source = "\\caption*[short {nested}]\n% between arguments\n{\nlong\n}";
        assert_eq!(caption_lines(source, 4), Some(2..5));
        assert_eq!(caption_lines(source, 1), None);
    }

    #[test]
    fn fancyvrb_closure_uses_only_the_first_end_marker_on_each_input_line() {
        for environment in ["Verbatim", "Verbatim*", "minted", "minted*"] {
            for newline in ["\n", "\r\n"] {
                let source = format!(
                    "\\begin{{{environment}}}{newline}\
                     \\end{{other}} ignored \\end{{{environment}}}{newline}\
                     \\caption{{LiteralCopper{newline}LiteralNickel{newline}}}{newline}\
                     prefix \\end{{{environment}}}   {newline}\
                     \\caption{{VisibleCopper{newline}VisibleNickel{newline}}}"
                );
                let visible = source.find("\\caption{VisibleCopper").unwrap();
                assert_eq!(
                    opaque_command_end(&source, "begin", 6),
                    Some((visible, false))
                );
                for word in ["LiteralCopper", "LiteralNickel"] {
                    assert_eq!(
                        caption_argument_at(&source, source.find(word).unwrap()),
                        None,
                        "{environment}, {word}"
                    );
                }
                let argument =
                    caption_argument_at(&source, source.find("VisibleNickel").unwrap()).unwrap();
                assert_eq!(
                    &source[argument],
                    format!("VisibleCopper{newline}VisibleNickel{newline}")
                );
                let unclosed = &source[..source.find("prefix").unwrap()];
                assert_eq!(
                    opaque_command_end(unclosed, "begin", 6),
                    Some((unclosed.len(), false))
                );
            }
        }
    }

    #[test]
    fn other_printed_opaque_environments_keep_their_inline_closure() {
        for environment in ["verbatim", "verbatim*", "lstlisting", "lstlisting*"] {
            let source = format!(
                "\\begin{{{environment}}}\\end{{other}} \\end{{{environment}}}\
                 \\caption{{VisibleCopper}}"
            );
            let visible = source.find("\\caption").unwrap();
            assert_eq!(
                opaque_command_end(&source, "begin", 6),
                Some((visible, false))
            );
            assert!(caption_argument_at(&source, source.find("VisibleCopper").unwrap()).is_some());
        }
    }

    #[test]
    fn math_regions_respect_tex_delimiters_and_comments() {
        let source = "\\$ ignored % $not math$\n$one$ \\(two\\) \\[three\\] $$four$$ \\begin{align*}five\\end{align*}\n\\verb|$hidden$| \\begin{verbatim}$hidden$\\end{verbatim} $unterminated";
        for word in ["one", "two", "three", "four", "five"] {
            assert_eq!(
                source_location(source, 2, 0..4, false, word, 0, None),
                Some((2, source.lines().nth(1).unwrap().find(word).unwrap()))
            );
        }
        for word in ["ignored", "hidden", "unterminated"] {
            assert_eq!(source_location(source, 2, 0..4, false, word, 0, None), None);
        }
    }

    #[test]
    fn non_rendered_delimiters_and_environment_arguments_are_not_candidates() {
        for (source, glyph) in [
            (r"$\left. x\right|$", "."),
            (r"$\sqrt[3]{x}$", "["),
            (r"\begin{alignat}{2}x&=y\end{alignat}", "2"),
            (r"$\begin{array}{c}x\end{array}$", "c"),
        ] {
            assert_eq!(
                source_location(source, 1, 0..1, false, glyph, 0, None),
                None
            );
        }
        let source = r"\begin{alignat}{2}\alpha&=\beta\end{alignat}";
        assert_eq!(
            source_location(source, 1, 0..1, false, "α=β", 0, None),
            Some((1, source.find(r"\alpha").unwrap()))
        );
    }

    #[test]
    fn mathematical_commands_and_styles_keep_original_spans() {
        let source = r"$\alpha\longmapsto\mathcal H+\mathbf v$";
        for (context, selected, needle) in [
            ("𝛼⟼ℋ+𝐯", "𝛼", "\\alpha"),
            ("𝛼⟼ℋ+𝐯", "⟼", "\\longmapsto"),
            ("𝛼⟼ℋ+𝐯", "ℋ", "H"),
            ("𝛼⟼ℋ+𝐯", "𝐯", "v"),
        ] {
            assert_eq!(
                source_location(
                    source,
                    1,
                    0..1,
                    false,
                    context,
                    context.find(selected).unwrap(),
                    None
                ),
                Some((1, source.find(needle).unwrap()))
            );
        }
        let matrix = "\\begin{align*}\\dot f(t)&=\\begin{pmatrix}\\cos\\alpha_1(t)\\\\\\sin\\alpha_2(t)\\end{pmatrix}\\end{align*}";
        let context = "f(t)=cosα1(t) sinα2(t)";
        assert_eq!(
            source_location(
                matrix,
                1,
                0..1,
                false,
                context,
                context.find("sin").unwrap(),
                None
            ),
            Some((1, matrix.find("\\sin").unwrap()))
        );
    }

    #[test]
    fn opaque_duplicate_expressions_cannot_make_other_matches_unique() {
        let source = "$x\\longmapsto y+\\custom{z}$\n$x\\longmapsto y$";
        assert_eq!(source_location(source, 2, 0..2, true, "x⟼y", 1, None), None);
        let source = "$x\\longmapsto\\custom y$\n$x\\longmapsto h$";
        assert_eq!(source_location(source, 2, 0..2, true, "x⟼y", 1, None), None);
        assert_eq!(source_location(source, 2, 0..2, true, "x⟼h", 1, None), None);
        let source = "$\\alpha\\custom{z}$\n$\\beta$";
        assert_eq!(source_location(source, 2, 0..2, true, "β", 0, None), None);
        let source = "$\\custom{z}$\nA. Box in 2020";
        assert_eq!(
            source_location(source, 2, 0..2, true, "A. Box", 0, Some((2, 0, 3))),
            Some((2, 0))
        );
        assert_eq!(
            source_location(
                source,
                2,
                0..2,
                true,
                "in 2020",
                "in ".len(),
                Some((2, 10, 3))
            ),
            Some((2, 10))
        );
    }

    #[test]
    fn duplicate_equations_opaque_commands_and_private_glyphs_stay_coarse() {
        let source = "$\\alpha\\leq\\beta$\n$\\gamma\\geq\\delta$\n$\\alpha\\leq\\beta$";
        assert_eq!(
            source_location(source, 3, 0..3, true, "α≤β γ≥δ α≤β", 0, None),
            None
        );
        assert_eq!(
            source_location("$\\custom{x}+y$", 1, 0..1, false, "x+y", 0, Some((1, 9, 0))),
            None
        );
        assert_eq!(
            source_location("$\\mathcal H$", 1, 0..1, false, "\u{e234}", 0, None),
            None
        );
        assert_eq!(
            source_location("$x$", 1, 0..1, false, "example", 1, None),
            None
        );
        assert_eq!(
            source_location("$X+x$", 1, 0..1, false, "𝑋+𝑥", 0, None),
            Some((1, 1))
        );
    }

    #[test]
    fn unsupported_neighbors_and_unanchored_punctuation_remain_coarse() {
        for (source, context, offset) in [
            ("Hello.\n$x.y$", "Hello. x.y", 5),
            ("$\\identity{\\alpha}$\n$\\alpha$", "α α", 0),
            ("$\\alpha_a^b$\n$\\alpha_b^a$", "α\r\nba\r\nα\r\nab", 0),
        ] {
            assert_eq!(
                source_location(source, 1, 0..2, false, context, offset, None),
                None,
                "{source}"
            );
        }
        assert_eq!(
            source_location("$x.y$", 1, 0..1, false, "x.y", 1, None),
            Some((1, 2))
        );
    }

    #[test]
    fn repeated_inline_math_across_soft_wraps_preserves_occurrence_ownership() {
        let source = "\
  \\begin{enumerate}
    \\item if \\(u\\in\\mathcal M_k(a_0,a_1)\\) is regular and
      \\(q_*\\) is its initial sample value, then \\(q_*\\) scales \\(\\Omega_k\\) on
      \\(B_n^{\\le k}\\);
    \\item similarly, if \\(q_*\\in B_n^{\\le k}\\) scales
      \\(\\Omega_k\\), then the
      combination of any local sample interval
  \\end{enumerate}";
        let context = "similarly, if q* ∈ Bn≤k scales Ωk,";
        let offset = context.find("Ωk").unwrap();
        assert_eq!(
            source_location(source, 6, 1..8, false, context, offset, None),
            Some((
                6,
                source.lines().nth(5).unwrap().find("\\Omega_k").unwrap()
            ))
        );
    }

    #[test]
    fn caption_lines_ignore_nonprinting_and_opaque_captions() {
        let source = "\
\\begin{comment}
\\caption{Hidden
comment
caption}
\\end{comment}
\\newcommand{\\mycap}{\\caption{Macro
caption}}
\\caption{Visible
caption}";
        assert_eq!(caption_lines(source, 3), None);
        assert_eq!(caption_lines(source, 6), None);
        assert_eq!(caption_lines(source, 9), Some(7..9));
    }
}
