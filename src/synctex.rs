use std::io;
use std::path::Path;

pub use synctex_parser::*;

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
    resolve_forward_with_selector(
        pdf,
        file,
        line,
        column,
        context,
        |pdf, candidates, hint, anchor| {
            crate::pdf::select_forward_line_candidate(pdf, candidates, hint, anchor, library)
        },
    )
}
