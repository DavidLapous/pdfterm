use pdfterm::{
    pdf::{DarkModeStyle, FitMode, RenderKey, RenderRequest, RenderWorker, WorkerMessage},
    process::{self, Operation},
    source_snapshot::{SourceInfo, SourceSnapshot},
    synctex::{self, DocumentRevision, ForwardSource},
};
use sha2::{Digest, Sha256};
use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

fn pdfium_test_lock() -> std::sync::MutexGuard<'static, ()> {
    // This integration binary has its own process-global PDFium bindings.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().expect("native PDFium fixture lock poisoned")
}

fn message(worker: &RenderWorker) -> WorkerMessage {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(message) = worker.try_recv() {
            return message;
        }
        assert!(Instant::now() < deadline, "worker never completed request");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn inverse_native_near_boundary_preserves_the_clicked_source_identity() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let child = root.join("child.tex");
    fs::write(
        &child,
        format!("{}ChildZephyr paragraph.\\par\n", "\n".repeat(40)),
    )
    .unwrap();
    let (pdf, _) = compile_out_of_tree_fixture(
        &root,
        "\\documentclass{article}\n\
         \\begin{document}\n\
         MainCopper paragraph.\\par\n\
         \\input{child}\n\
         \\end{document}\n",
    );
    // This point lies on the child's side of the native nearest-anchor boundary.
    // Rounding y to 137.28 selects MainCopper in the other input instead.
    let result = synctex::resolve_inverse(
        &pdf,
        synctex::InversePoint {
            page: 1,
            x: 140.0,
            y_from_top: 137.28305,
            page_height_pt: 792.0,
        },
        None,
        0,
        &Operation::default(),
        None,
        false,
    )
    .unwrap();
    assert_eq!(
        Path::new(&result.location.file),
        child.canonicalize().unwrap()
    );
    assert_eq!(result.location.line, 41);
    assert!(!result.location.precise);
    assert!(result.warning.is_none());
}

#[test]
fn beamer_overlay_forward_uses_visible_source_context() {
    let _native = pdfium_test_lock();
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("overlay.tex");
    fs::write(
        &source,
        r"\documentclass{beamer}
\begin{document}
\begin{frame}Cover page.\end{frame}
\begin{frame}{Overlay}
\begin{itemize}
\item Base words remain visible.
\pause
\item UniqueZephyr appears after the first overlay.
\end{itemize}
\only<3->{AnotherNebula appears on the third overlay.}
\end{frame}
\end{document}
",
    )
    .unwrap();
    let output = process::output(
        Command::new("pdflatex")
            .current_dir(directory.path())
            .args([
                "-interaction=nonstopmode",
                "-halt-on-error",
                "-synctex=1",
                "overlay.tex",
            ]),
        &Operation::new(Duration::from_secs(30)),
    )
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let pdf = directory.path().join("overlay.pdf");
    assert_eq!(
        synctex::resolve_forward(&pdf, &source, 6, 7).unwrap().page,
        2
    );
    assert_eq!(
        synctex::resolve_forward(&pdf, &source, 8, 7).unwrap().page,
        3
    );
    assert_eq!(
        synctex::resolve_forward(&pdf, &source, 10, 12)
            .unwrap()
            .page,
        4
    );
    let line_anchor = synctex::resolve_forward(&pdf, &source, 10, 7).unwrap();
    assert_eq!(line_anchor.page, 2);
    assert!(line_anchor.word.is_none());
    assert!(line_anchor.warning.is_some());
    assert_eq!(
        synctex::resolve_forward(&pdf, &source, 10, 1).unwrap().page,
        2
    );
}

#[test]
fn forward_multiline_caption_uses_caption_region_instead_of_graphic_legend() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let graphic_root = root.join("graphic");
    fs::create_dir(&graphic_root).unwrap();
    compile_out_of_tree_fixture(
        &graphic_root,
        "\\documentclass{article}\n\
         \\usepackage[paperwidth=220pt,paperheight=80pt,margin=10pt]{geometry}\n\
         \\pagestyle{empty}\n\
         \\begin{document}\n\
         \\noindent LegendQuartz labels the graphic.\n\
         \\end{document}\n",
    );
    let contents = r"\documentclass{article}
\usepackage{graphicx}
\usepackage[paperwidth=500pt,paperheight=400pt,margin=30pt]{geometry}
\pagestyle{empty}
\begin{document}
OutsideAmber remains ordinary prose.
\begin{figure}[h!]
\centering
\includegraphics{graphic/build/deck.pdf}
\caption[ShortQuartz]{CaptionCopper illustrates this literal caption.
InteriorNickel belongs to the caption rather than the legend above.
The final literal sentence closes the caption.
}
\end{figure}
\end{document}
";
    let (pdf, source) = compile_out_of_tree_fixture(&root, contents);
    let caption = painted_word_points(&pdf, "CaptionCopper").1;
    let legend = painted_word_points(&pdf, "LegendQuartz").1;
    let outside = painted_word_points(&pdf, "OutsideAmber").1;
    assert_eq!(caption.page, legend.page);
    assert!(legend.y_from_top < caption.y_from_top);
    let cursor = |word: &str| {
        let (index, row) = contents
            .lines()
            .enumerate()
            .find(|(_, row)| row.contains(word))
            .unwrap();
        (index as u32 + 1, row.find(word).unwrap() as u32 + 1)
    };
    let info = SourceInfo::read(&pdf).unwrap();
    let cache = root.join("compiled-source");
    fs::create_dir(&cache).unwrap();
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(cache.join("1.txt"), contents).unwrap();
    let mut digest = String::with_capacity(64);
    for byte in Sha256::digest(contents.as_bytes()) {
        write!(&mut digest, "{byte:02x}").unwrap();
    }
    let manifest = cache.join("manifest.json");
    fs::write(
        &manifest,
        serde_json::to_vec(&serde_json::json!({
            "pdf": info.pdf,
            "revision": info.revision,
            "cwd": info.cwd,
            "main": source,
            "files": std::collections::BTreeMap::from([(source.clone(), "1.txt")]),
            "digests": std::collections::BTreeMap::from([(source.clone(), digest)]),
            "warnings": std::collections::BTreeMap::from([
                (source.clone(), "source changed during compilation"),
            ]),
        }))
        .unwrap(),
    )
    .unwrap();
    let _native = pdfium_test_lock();
    let (line, column) = cursor("OutsideAmber");
    let prose = synctex::resolve_forward_with_library(
        &pdf,
        &source,
        line,
        column,
        None,
        ForwardSource::default(),
    )
    .unwrap();
    assert_eq!(prose.page, outside.page);
    assert!((prose.h..=prose.h + prose.width).contains(&outside.x));
    assert!((prose.v - prose.height..=prose.v).contains(&outside.y_from_top));
    assert!(!(prose.v - prose.height..=prose.v).contains(&caption.y_from_top));
    assert!(prose.word.is_none() && prose.warning.is_some());
    // Without captured source, anchor-only requests must keep the native query.
    let (line, column) = cursor("CaptionCopper");
    let raw = synctex::resolve_forward_with_library(
        &pdf,
        &source,
        line,
        column,
        None,
        ForwardSource {
            anchor_only: true,
            ..ForwardSource::default()
        },
    )
    .unwrap();
    assert_eq!(raw.page, legend.page);
    assert!((raw.h..=raw.h + raw.width).contains(&legend.x));
    assert!((raw.v - raw.height..=raw.v).contains(&legend.y_from_top));
    assert!(!(raw.v - raw.height..=raw.v).contains(&caption.y_from_top));
    assert!(raw.word.is_none() && raw.warning.is_some());
    for context in [
        ForwardSource::default(),
        ForwardSource {
            snapshot: Some(&manifest),
            best_guess: true,
            anchor_only: true,
        },
    ] {
        if context.snapshot.is_some() {
            // Captured bytes, not the live file, must identify the caption scope.
            fs::write(&source, "\\relax\n").unwrap();
        }
        for word in ["CaptionCopper", "InteriorNickel"] {
            let (line, column) = cursor(word);
            let request =
                synctex::resolve_forward_with_library(&pdf, &source, line, column, None, context)
                    .unwrap();
            assert_eq!(request.page, caption.page, "{word}: {request:?}");
            assert!(
                (request.h..=request.h + request.width).contains(&caption.x)
                    && (request.v - request.height..=request.v).contains(&caption.y_from_top),
                "caption body must reach its printed first row: {word}, {request:?}, {caption:?}"
            );
            assert!(
                !(request.v - request.height..=request.v).contains(&legend.y_from_top),
                "caption body must not highlight the graphic legend: {word}, {request:?}, {legend:?}"
            );
            assert!(request.word.is_none() && request.warning.is_some());
        }
    }
}

#[test]
fn forward_fancyvrb_literal_captions_keep_their_original_line_regions() {
    for environment in ["Verbatim", "Verbatim*"] {
        let contents = format!(
            "\\documentclass{{article}}\n\
             \\usepackage{{fancyvrb}}\n\
             \\usepackage[paperwidth=500pt,paperheight=600pt,margin=30pt]{{geometry}}\n\
             \\pagestyle{{empty}}\n\
             \\begin{{document}}\n\
             \\begin{{{environment}}}\n\
             \\end{{other}} ignored \\end{{{environment}}}\n\
             \\caption{{LiteralCopper originally owns this printed line.\n\
             LiteralNickel owns a separate printed line.\n\
             }}\n\
             \\end{{{environment}}}\n\
             AfterQuartz ordinary prose.\n\
             \\begin{{figure}}[h]\n\
             \\caption{{VisibleCopper begins an actual caption.\n\
             VisibleNickel continues the actual caption.\n\
             }}\n\
             \\end{{figure}}\n\
             \\end{{document}}\n"
        );
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (pdf, source) = compile_out_of_tree_fixture(&root, &contents);
        let copper = painted_word_points(&pdf, "LiteralCopper").1;
        let nickel = painted_word_points(&pdf, "LiteralNickel").1;
        let caption = painted_word_points(&pdf, "VisibleCopper").1;
        let _native = pdfium_test_lock();
        for (line, column, expected) in [(8, 10, copper), (9, 1, nickel), (15, 1, caption)] {
            let request = synctex::resolve_forward_with_library(
                &pdf,
                &source,
                line,
                column,
                None,
                ForwardSource::default(),
            )
            .unwrap();
            assert_eq!(request.page, expected.page);
            assert!(
                (request.h..=request.h + request.width).contains(&expected.x)
                    && (request.v - request.height..=request.v).contains(&expected.y_from_top),
                "{environment}, source {line}:{column}: {request:?}, {expected:?}"
            );
            assert!(request.word.is_none() && request.warning.is_some());
        }
    }
}

#[test]
fn forward_reference_expansion_does_not_join_literal_context() {
    let contents = r"\documentclass{article}
\newcounter{owner}
\newcommand{\Other}{Alpha needle Omega}
\begin{document}
\refstepcounter{owner}\label{owner}
Alpha needle \ref{owner} Omega \Other

Independent Copper owner
\end{document}
";
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (pdf, source) = compile_out_of_tree_fixture(&root, contents);
    // Load the actual first-pass label so \ref prints an alphanumeric token.
    let output = process::output(
        Command::new("pdflatex")
            .current_dir(&root)
            .args([
                "-interaction=nonstopmode",
                "-halt-on-error",
                "-synctex=1",
                "-output-directory=build",
                "-jobname=deck",
            ])
            .arg(&source),
        &Operation::new(Duration::from_secs(30)),
    )
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(painted_word_occurrences(&pdf, "needle").len(), 2);
    let expected = painted_word_points(&pdf, "Copper").1;
    let _native = pdfium_test_lock();
    let reference = synctex::resolve_forward(&pdf, &source, 6, 7).unwrap();
    assert!(
        reference.word.is_none() && reference.warning.is_some(),
        "unsupported reference context must retain a warned line anchor: {reference:?}"
    );
    let literal = synctex::resolve_forward(&pdf, &source, 8, 13).unwrap();
    assert_eq!(literal.page, expected.page);
    assert!(
        (literal.h..=literal.h + literal.width).contains(&expected.x),
        "qualified glyph must contain the rendered word center: {literal:?}, {expected:?}"
    );
    assert!(
        (literal.v - literal.height..=literal.v).contains(&expected.y_from_top),
        "qualified glyph must contain the rendered word center: {literal:?}, {expected:?}"
    );
}

#[test]
fn forward_unknown_printed_macros_cannot_manufacture_literal_ownership() {
    let contents = r"\documentclass{article}
\usepackage[paperwidth=900pt,paperheight=300pt,margin=20pt]{geometry}
\newcommand{\Opaque}{Intruder}
\newcommand{\Duplicate}{needle alpha beta gamma delta epsilon zeta}
\begin{document}
Cover page.
\newpage
needle alpha beta gamma \Opaque delta epsilon zeta \Duplicate
\newcommand{\Discard}[1]{}
\Discard{needle alpha beta gamma delta epsilon zeta}
\def\DiscardUntil#1\Stop{}

\DiscardUntil needle alpha beta gamma delta epsilon zeta\Stop\Duplicate

Independent Copper owner
\end{document}
";
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (pdf, source) = compile_out_of_tree_fixture(&root, contents);
    assert_eq!(painted_word_occurrences(&pdf, "needle").len(), 3);
    let expected = painted_word_points(&pdf, "Copper").1;
    assert_eq!(expected.page, 2);
    let _native = pdfium_test_lock();
    for (prefix, selected) in [
        ("needle alpha", "needle"),
        (r"\Discard{", "zeta"),
        (r"\DiscardUntil ", "gamma"),
    ] {
        let (index, text) = contents
            .lines()
            .enumerate()
            .find(|(_, text)| text.starts_with(prefix))
            .unwrap();
        let column = text.find(selected).unwrap() as u32 + 1;
        let coarse = synctex::resolve_forward(&pdf, &source, index as u32 + 1, column).unwrap();
        assert_eq!(
            coarse.page, expected.page,
            "word failure must retain the line's page instead of opening page one: {coarse:?}"
        );
        assert!(
            coarse.word.is_none(),
            "unknown macros cannot prove a printed word: {coarse:?}"
        );
        assert!(coarse.warning.is_some());
        assert!(coarse.width > 0.0 || coarse.height > 0.0);
    }
    let literal_line = contents
        .lines()
        .position(|text| text.starts_with("Independent"))
        .unwrap() as u32
        + 1;
    let literal = synctex::resolve_forward(&pdf, &source, literal_line, 13).unwrap();
    assert!(literal.word.is_none() && literal.warning.is_some());
    assert_eq!(literal.page, expected.page);
    assert!((literal.h..=literal.h + literal.width).contains(&expected.x));
    assert!((literal.v - literal.height..=literal.v).contains(&expected.y_from_top));
}

#[test]
fn forward_discarded_multiline_body_retains_the_compiler_line_anchor() {
    let contents = r"\documentclass{article}
\usepackage[paperwidth=900pt,paperheight=300pt,margin=20pt]{geometry}
\newcommand{\Discard}[1]{}
\newcommand{\Duplicate}{needle alpha beta gamma delta epsilon zeta}
\begin{document}
\input{control.tex}
Cover page.
\newpage
\Discard{
needle alpha beta gamma delta epsilon zeta
}\Duplicate

Independent Copper owner
\end{document}
";
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let control_source = root.join("control.tex");
    fs::write(&control_source, "Independent NickelControl owner\n\n").unwrap();
    let (pdf, source) = compile_out_of_tree_fixture(&root, contents);
    // Only \Duplicate prints gamma; the queried \Discard argument prints none.
    let printed = painted_word_occurrences(&pdf, "gamma");
    assert_eq!(printed.len(), 1);
    assert_eq!(printed[0].1.page, 2);
    let copper = painted_word_points(&pdf, "Copper").1;
    let expected_control = painted_word_points(&pdf, "NickelControl").1;
    let _native = pdfium_test_lock();
    let query_line = contents
        .lines()
        .position(|row| row == "needle alpha beta gamma delta epsilon zeta")
        .unwrap() as u32
        + 1;
    let coarse = synctex::resolve_forward(&pdf, &source, query_line, 19).unwrap();
    assert_eq!(coarse.page, 2);
    assert!(
        coarse.word.is_none() && coarse.warning.is_some(),
        "{coarse:?}"
    );
    let anchor = synctex::resolve_forward_with_library(
        &pdf,
        &source,
        query_line,
        19,
        None,
        ForwardSource {
            best_guess: true,
            anchor_only: true,
            ..ForwardSource::default()
        },
    )
    .unwrap();
    assert_eq!(
        (coarse.page, coarse.h, coarse.v, coarse.width, coarse.height),
        (anchor.page, anchor.h, anchor.v, anchor.width, anchor.height)
    );
    let literal_line = contents
        .lines()
        .position(|row| row.starts_with("Independent Copper owner"))
        .unwrap() as u32
        + 1;
    let literal = synctex::resolve_forward(&pdf, &source, literal_line, 13).unwrap();
    assert!(literal.word.is_none() && literal.warning.is_some());
    assert_eq!(literal.page, copper.page);
    assert!((literal.h..=literal.h + literal.width).contains(&copper.x));
    assert!((literal.v - literal.height..=literal.v).contains(&copper.y_from_top));
    let control = synctex::resolve_forward(&pdf, &control_source, 1, 13).unwrap();
    assert!(control.word.is_none() && control.warning.is_some());
    assert_eq!(control.page, expected_control.page);
    assert!((control.h..=control.h + control.width).contains(&expected_control.x));
    assert!((control.v - control.height..=control.v).contains(&expected_control.y_from_top));
}

#[test]
fn forward_scoped_and_numeric_expansions_cannot_claim_discarded_input() {
    let main = r"\documentclass{article}
\usepackage[paperwidth=900pt,paperheight=300pt,margin=20pt]{geometry}
\usepackage{beamerarticle}
\newcommand{\Take}[2]{}
\newcommand{\TakeNumber}[1]{0}
\newcommand{\Duplicate}{needle alpha beta gamma delta epsilon zeta}
\newcommand{\ExternalGlobalMutator}{\global\def\Take##1##2{}}
\let\SavedNoindent\noindent
\let\SavedRelax\relax
\begin{document}
\input{control.tex}
Cover page.
\newpage
\input{scoped.tex}
\end{document}
";
    let ordinary = [
        r"Independent Copper owner

\begin{center}
\def\Take#1{}
Center marker.
\end{center}
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        r"Independent Copper owner

\pdfcompresslevel=0\TakeNumber{
needle alpha beta gamma delta epsilon zeta
}\relax\Duplicate
",
        r"Independent Copper owner

$\def\Take#1{}x$
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        r"Independent Copper owner

$$\def\Take#1{}x$$
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        r"Independent Copper owner

\(\def\Take#1{}x\)
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        r"Independent Copper owner

\[\def\Take#1{}x\]
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        r"Independent Copper owner

\def\Take#1{}
\input{redefine.tex}
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        r"Independent Copper owner

\def\Take#1{}
{\ExternalGlobalMutator}
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        r"Independent Copper owner

\def\Take#1{}
\textbf{\ExternalGlobalMutator}
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        r"Independent Copper owner

\only<presentation>\def\Take#1{}
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        r"\title{\ExternalGlobalMutator}
\def\Take#1{}
\maketitle
Independent Copper owner
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        r"Independent Copper owner

{\global\let\noindent\Take}
\noindent{first}{
needle alpha beta gamma delta epsilon zeta
}\global\let\noindent\SavedNoindent\Duplicate
",
        r"Independent Copper owner

{\global\let\relax\TakeNumber}
\pdfcompresslevel=0\relax{
needle alpha beta gamma delta epsilon zeta
}\global\let\relax\SavedRelax\Duplicate
",
    ];
    let inherited = [
        (
            "{",
            r"\def\Take#1{}
}
Independent Copper owner
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        ),
        (
            r"\begin{center}",
            r"\def\Take#1{}
\end{center}
Independent Copper owner
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        ),
        (
            "$",
            r"\def\Take#1{}
$
Independent Copper owner
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        ),
        (
            "$$",
            r"\def\Take#1{}
$$
Independent Copper owner
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        ),
        (
            r"\(",
            r"\def\Take#1{}
\)
Independent Copper owner
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        ),
        (
            r"\[",
            r"\def\Take#1{}
\]
Independent Copper owner
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        ),
    ];
    for (opening, contents) in ordinary
        .into_iter()
        .map(|contents| ("", contents))
        .chain(inherited)
    {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let included = root.join("scoped.tex");
        fs::write(&included, contents).unwrap();
        fs::write(root.join("redefine.tex"), "\\def\\Take#1#2{}\n").unwrap();
        let control_source = root.join("control.tex");
        fs::write(&control_source, "Independent NickelControl owner\n\n").unwrap();
        let main_with_opening;
        let fixture_main = if opening.is_empty() {
            main
        } else {
            main_with_opening = main.replace(
                "\\input{scoped.tex}",
                &format!("{opening}\n\\input{{scoped.tex}}"),
            );
            &main_with_opening
        };
        let (pdf, _) = compile_out_of_tree_fixture(&root, fixture_main);
        // Neither captured argument prints gamma; only the main's Duplicate does.
        let printed = painted_word_occurrences(&pdf, "gamma");
        assert_eq!(printed.len(), 1);
        assert_eq!(printed[0].1.page, 2);
        let copper = painted_word_points(&pdf, "Copper").1;
        let (index, body) = contents
            .lines()
            .enumerate()
            .find(|(_, row)| row.contains("needle alpha"))
            .unwrap();
        let line = index as u32 + 1;
        let column = body.find("gamma").unwrap() as u32 + 1;
        let expected_control = painted_word_points(&pdf, "NickelControl").1;
        let _native = pdfium_test_lock();
        let coarse = synctex::resolve_forward(&pdf, &included, line, column).unwrap();
        assert_eq!(coarse.page, 2);
        assert!(
            coarse.word.is_none() && coarse.warning.is_some(),
            "{coarse:?}"
        );
        let anchor = synctex::resolve_forward_with_library(
            &pdf,
            &included,
            line,
            column,
            None,
            ForwardSource {
                best_guess: true,
                anchor_only: true,
                ..ForwardSource::default()
            },
        )
        .unwrap();
        assert_eq!(
            (coarse.page, coarse.h, coarse.v, coarse.width, coarse.height),
            (anchor.page, anchor.h, anchor.v, anchor.width, anchor.height)
        );
        let (copper_index, copper_body) = contents
            .lines()
            .enumerate()
            .find(|(_, row)| row.contains("Independent Copper owner"))
            .unwrap();
        let copper_column = copper_body.find("Copper").unwrap() as u32 + 1;
        let literal =
            synctex::resolve_forward(&pdf, &included, copper_index as u32 + 1, copper_column)
                .unwrap();
        assert!(
            literal.word.is_none() && literal.warning.is_some(),
            "{literal:?}"
        );
        assert_eq!(literal.page, copper.page);
        assert!((literal.h..=literal.h + literal.width).contains(&copper.x));
        assert!((literal.v - literal.height..=literal.v).contains(&copper.y_from_top));
        let control = synctex::resolve_forward(&pdf, &control_source, 1, 13).unwrap();
        assert!(
            control.word.is_none() && control.warning.is_some(),
            "{control:?}"
        );
        assert_eq!(control.page, expected_control.page);
        assert!((control.h..=control.h + control.width).contains(&expected_control.x));
        assert!((control.v - control.height..=control.v).contains(&expected_control.y_from_top));
    }
}

#[test]
fn forward_inherited_controls_and_joined_words_retain_warned_line_geometry() {
    let main = r"\documentclass{article}
\usepackage[paperwidth=900pt,paperheight=300pt,margin=20pt]{geometry}
\def\TakeTwo#1#2{}
\def\Duplicate{needle alpha beta gamma delta epsilon zeta}
\def\Opaque{Intruder}
\let\SavedNoindent\noindent
\let\SavedLabel\label
\def\Restore{\global\let\noindent\SavedNoindent\global\let\label\SavedLabel}
\begin{document}
\input{control.tex}
Cover page.
\newpage
% inherited binding
\input{original.tex}
\end{document}
";
    for (name, binding, contents, printed_count) in [
        (
            "noindent-bare",
            "\\global\\let\\noindent\\TakeTwo",
            "\\noindent needle alpha beta gamma delta epsilon zeta\n\\Restore\\Duplicate\n",
            2,
        ),
        (
            "noindent-group",
            "\\global\\let\\noindent\\TakeTwo",
            "\\noindent{first}{needle alpha beta gamma delta epsilon zeta}\\Restore\\Duplicate\n",
            1,
        ),
        (
            "noindent-restore-same-row",
            "\\global\\let\\noindent\\TakeTwo",
            "\\noindent needle alpha beta gamma delta epsilon zeta\\Restore\\Duplicate\n",
            2,
        ),
        (
            "label-bare",
            "\\global\\let\\label\\TakeTwo",
            "\\label{first}needle alpha beta gamma delta epsilon zeta\\Restore\\Duplicate\n",
            2,
        ),
        (
            "label-extra-group",
            "\\global\\let\\label\\TakeTwo",
            "\\label{first}{needle alpha beta gamma delta epsilon zeta}\\Restore\\Duplicate\n",
            1,
        ),
        (
            "joined-label-suffix",
            "",
            "needle alpha beta gamma delta epsilon zeta\\label{owner}Suffix \\Duplicate\n",
            2,
        ),
        (
            "opaque-separated",
            "",
            "needle alpha beta gamma delta epsilon zeta\\Opaque \\Duplicate\n",
            2,
        ),
        (
            "everypar-plain-prefix",
            "\\everypar{\\global\\everypar{}\\TakeTwo}",
            "needle alpha beta gamma delta epsilon zeta \\Duplicate\n",
            2,
        ),
        (
            "active-tilde-group",
            "\\global\\let~\\TakeTwo",
            "~{first}{needle alpha beta gamma delta epsilon zeta}\\Duplicate\n",
            1,
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        // Deliberately share the compiler root's basename, not its identity.
        let included = root.join("original.tex");
        fs::write(&included, contents).unwrap();
        let control_source = root.join("control.tex");
        fs::write(&control_source, "Independent NickelControl owner\n\n").unwrap();
        let (pdf, main_source) =
            compile_out_of_tree_fixture(&root, &main.replace("% inherited binding", binding));
        let inputs = SourceInfo::read(&pdf).unwrap();
        assert_eq!(inputs.main.as_ref(), Some(&main_source));
        assert_ne!(main_source, included);
        assert!(inputs.inputs.contains(&included));
        let printed = painted_word_occurrences(&pdf, "gamma");
        assert_eq!(printed.len(), printed_count, "{name}");
        assert!(printed.iter().all(|(_, point)| point.page == 2), "{name}");
        let expected_control = painted_word_points(&pdf, "NickelControl").1;
        let column = contents.lines().next().unwrap().find("gamma").unwrap() as u32 + 1;
        let _native = pdfium_test_lock();
        for best_guess in [false, true] {
            let request = synctex::resolve_forward_with_library(
                &pdf,
                &included,
                1,
                column,
                None,
                ForwardSource {
                    best_guess,
                    ..ForwardSource::default()
                },
            )
            .unwrap();
            assert_eq!(request.page, 2, "{name}: {request:?}");
            assert!(
                request.word.is_none() && request.warning.is_some(),
                "{name}: {request:?}"
            );
            assert_eq!(request.best_guess, best_guess);
            assert!(request.width > 0.0 || request.height > 0.0, "{name}");
        }
        let control = synctex::resolve_forward(&pdf, &control_source, 1, 13).unwrap();
        assert!(
            control.word.is_none() && control.warning.is_some(),
            "{name}: {control:?}"
        );
        assert_eq!(control.page, expected_control.page);
        assert!((control.h..=control.h + control.width).contains(&expected_control.x));
        assert!((control.v - control.height..=control.v).contains(&expected_control.y_from_top));
    }
}

#[test]
fn beamer_included_controls_keep_honest_compiler_line_geometry() {
    use std::os::unix::fs::PermissionsExt;
    let main = "\\documentclass{beamer}\n\\begin{document}\n\
                \\begin{frame}RootCover supplies the first page.\\end{frame}\n\
                \\input{controls.tex}\n\\end{document}\n";
    let contents = r"\begin{frame}{Inherited controls}
LiteralCopper keeps ordinary body context available.
\noindent LayoutNickel keeps ordinary layout context available.
\label{owner} LabelQuartz keeps metadata context available.
\only<1>{OverlayAurora keeps visible overlay context available.}
\uncover<1>{RevealNebula keeps visible overlay context available.}
\onslide<1>{ShiftCobalt keeps visible overlay context available.}
\begin{itemize}
\item<1> ItemSilver keeps visible item context available.
\end{itemize}
\end{frame}
";
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let source = root.join("controls.tex");
    fs::write(&source, contents).unwrap();
    let (pdf, main_source) = compile_out_of_tree_fixture(&root, main);
    let inputs = SourceInfo::read(&pdf).unwrap();
    assert_eq!(inputs.main.as_ref(), Some(&main_source));
    let cache = root.join("compiled-source");
    fs::create_dir(&cache).unwrap();
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o700)).unwrap();
    let mut files = std::collections::BTreeMap::new();
    let mut digests = std::collections::BTreeMap::new();
    for (original, text, filename) in [
        (&source, contents, "body.txt"),
        (&main_source, main, "main.txt"),
    ] {
        fs::write(cache.join(filename), text).unwrap();
        let mut digest = String::with_capacity(64);
        for byte in Sha256::digest(text.as_bytes()) {
            write!(&mut digest, "{byte:02x}").unwrap();
        }
        files.insert(original, filename);
        digests.insert(original, digest);
    }
    let manifest = cache.join("manifest.json");
    fs::write(
        &manifest,
        serde_json::to_vec(&serde_json::json!({
            "pdf": inputs.pdf,
            "revision": inputs.revision,
            "cwd": inputs.cwd,
            "main": main_source,
            "files": files,
            "digests": digests,
            "warnings": {},
        }))
        .unwrap(),
    )
    .unwrap();
    // These are genuine printed controls, not evidence that hashes trace TeX.
    // The existing multi-overlay fixture separately pins pages 2, 3, and 4.
    for word in [
        "LiteralCopper",
        "LayoutNickel",
        "LabelQuartz",
        "OverlayAurora",
        "RevealNebula",
        "ShiftCobalt",
        "ItemSilver",
    ] {
        let printed = painted_word_points(&pdf, word).1;
        assert_eq!(printed.page, 2, "{word}");
        let (index, row) = contents
            .lines()
            .enumerate()
            .find(|(_, row)| row.contains(word))
            .unwrap();
        let line = index as u32 + 1;
        let column = row.find(word).unwrap() as u32 + 1;
        let _native = pdfium_test_lock();
        let context = ForwardSource {
            snapshot: Some(&manifest),
            ..ForwardSource::default()
        };
        let request =
            synctex::resolve_forward_with_library(&pdf, &source, line, column, None, context)
                .unwrap();
        assert_eq!(request.page, printed.page, "{word}: {request:?}");
        assert!(request.word.is_none() && request.warning.is_some());
        assert!(!request.best_guess);
        assert!(request.width > 0.0 || request.height > 0.0);
        let anchor = synctex::resolve_forward_with_library(
            &pdf,
            &source,
            line,
            column,
            None,
            ForwardSource {
                anchor_only: true,
                ..context
            },
        )
        .unwrap();
        assert_eq!(
            (
                request.page,
                request.h,
                request.v,
                request.width,
                request.height
            ),
            (anchor.page, anchor.h, anchor.v, anchor.width, anchor.height),
            "{word}: no source WORD proof may replace the compiler's LINE rectangle"
        );
    }
    let _native = pdfium_test_lock();
    let row = main.lines().nth(2).unwrap();
    let root_request = synctex::resolve_forward_with_library(
        &pdf,
        &main_source,
        3,
        row.find("RootCover").unwrap() as u32 + 1,
        None,
        ForwardSource {
            snapshot: Some(&manifest),
            ..ForwardSource::default()
        },
    )
    .unwrap();
    assert_eq!(root_request.page, 1);
    assert!(root_request.word.is_none() && root_request.warning.is_some());
    assert!(!root_request.best_guess);
}

#[test]
fn forward_control_cursor_always_warns_without_enabling_best_guess() {
    let contents = "\\documentclass{article}\n\\newcommand{\\NoArg}{Copper}\n\
                    \\begin{document}\n\\NoArg\n\\end{document}\n";
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (pdf, source) = compile_out_of_tree_fixture(&root, contents);
    let _native = pdfium_test_lock();
    for (best_guess, anchor_only) in [(false, false), (true, false), (false, true)] {
        let result = synctex::resolve_forward_with_library(
            &pdf,
            &source,
            4,
            1,
            None,
            ForwardSource {
                best_guess,
                anchor_only,
                ..ForwardSource::default()
            },
        )
        .unwrap();
        assert_eq!(result.page, 1);
        assert!(
            result.word.is_none() && result.warning.is_some(),
            "{result:?}"
        );
        assert!(result.width > 0.0 || result.height > 0.0);
        assert_eq!(result.best_guess, best_guess);
    }
}

#[test]
fn real_compiled_source_snapshot_survives_overwrite_and_warns_on_coarse_navigation() {
    use std::os::unix::fs::PermissionsExt;
    let main = r"\documentclass{article}
\newcommand{\Other}{Alpha needle Omega}
\begin{document}
\input{body.tex}
\end{document}
";
    let contents = "% authoritative compiled source\n% preserves original line numbering\n\
                    % literal control precedes unknown execution\n\
                    Compiled Copper owner remains stable.\n\n\
                    Alpha needle \\ref{missing} Omega \\Other\n";
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let source = root.join("body.tex");
    fs::write(&source, contents).unwrap();
    let (pdf, main_source) = compile_out_of_tree_fixture(&root, main);
    let expected = painted_word_points(&pdf, "Copper").1;
    // Metadata collection must work with no external SyncTeX executable.
    let metadata = process::output(
        Command::new(env!("CARGO_BIN_EXE_pdfterm"))
            .current_dir(&root)
            .env("PATH", "")
            .arg(&pdf)
            .arg("--synctex-source-info"),
        &Operation::default(),
    )
    .unwrap();
    assert!(
        metadata.status.success(),
        "{}",
        String::from_utf8_lossy(&metadata.stderr)
    );
    let info: SourceInfo = serde_json::from_slice(&metadata.stdout).unwrap();
    assert_eq!(info.main.as_deref(), Some(main_source.as_path()));
    assert_eq!(info.cwd, root);
    assert!(info.inputs.contains(&source));
    let cache = root.join("compiled-source");
    fs::create_dir(&cache).unwrap();
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(cache.join("1.txt"), contents).unwrap();
    fs::write(cache.join("2.txt"), main).unwrap();
    let manifest = cache.join("manifest.json");
    let files = std::collections::BTreeMap::from([
        (source.clone(), "1.txt"),
        (main_source.clone(), "2.txt"),
    ]);
    let mut digest = String::with_capacity(64);
    for byte in Sha256::digest(contents.as_bytes()) {
        write!(&mut digest, "{byte:02x}").unwrap();
    }
    let mut main_digest = String::with_capacity(64);
    for byte in Sha256::digest(main.as_bytes()) {
        write!(&mut main_digest, "{byte:02x}").unwrap();
    }
    fs::write(
        &manifest,
        serde_json::to_vec(&serde_json::json!({
            "pdf": info.pdf,
            "revision": info.revision,
            "cwd": info.cwd,
            "main": main_source,
            "files": files,
            "digests": std::collections::BTreeMap::from([
                (source.clone(), digest),
                (main_source.clone(), main_digest),
            ]),
            "warnings": {},
        }))
        .unwrap(),
    )
    .unwrap();
    for best_guess in [false, true] {
        let saved = synctex::resolve_inverse(
            &pdf,
            expected,
            Some(("Compiled Copper owner remains stable", 9)),
            4,
            &Operation::default(),
            None,
            best_guess,
        )
        .unwrap();
        assert_eq!(saved.location.file, source.to_str().unwrap());
        assert_eq!(saved.location.line, 4);
        assert_eq!(saved.location.byte_column, 9);
        assert!(saved.location.precise);
        assert_eq!(saved.warning.is_some(), best_guess);
    }
    fs::write(&source, "\\relax\n").unwrap();
    let snapshot = SourceSnapshot::load(&manifest, &pdf).unwrap();
    assert_eq!(snapshot.read(&source).unwrap(), contents);
    let _native = pdfium_test_lock();
    assert!(synctex::resolve_forward(&pdf, &source, 4, 10).is_err());
    let context = ForwardSource {
        snapshot: Some(&manifest),
        ..ForwardSource::default()
    };
    let exact = synctex::resolve_forward_with_library(&pdf, &source, 4, 10, None, context).unwrap();
    assert!(exact.warning.is_some());
    assert!(exact.word.is_none());
    assert_eq!(exact.source_snapshot.as_deref(), Some(manifest.as_path()));
    assert_eq!(exact.page, expected.page);
    assert!((exact.h..=exact.h + exact.width).contains(&expected.x));
    assert!((exact.v - exact.height..=exact.v).contains(&expected.y_from_top));
    for (line, column, anchor_only, best_guess) in [(6, 7, false, false), (4, 10, true, true)] {
        let coarse = synctex::resolve_forward_with_library(
            &pdf,
            &source,
            line,
            column,
            None,
            ForwardSource {
                best_guess,
                anchor_only,
                ..context
            },
        )
        .unwrap();
        assert!(
            coarse.word.is_none(),
            "coarse results must never refine a printed word"
        );
        assert!(coarse.warning.is_some());
        assert_eq!(coarse.source_snapshot.as_deref(), Some(manifest.as_path()));
    }
    let inverse = synctex::resolve_inverse(
        &pdf,
        expected,
        Some(("Compiled Copper owner remains stable", 9)),
        4,
        &Operation::default(),
        Some(&manifest),
        true,
    )
    .unwrap();
    assert_eq!(inverse.location.file, source.to_str().unwrap());
    assert_eq!(inverse.location.line, 4);
    assert_eq!(inverse.location.byte_column, 9);
    assert!(inverse.location.precise);
    assert!(inverse.warning.is_none());
    let coarse = synctex::resolve_inverse(
        &pdf,
        expected,
        Some(("Compiled Copper owner remains stable", 9)),
        4,
        &Operation::default(),
        None,
        true,
    )
    .unwrap();
    assert!(!coarse.location.precise);
    assert!(
        coarse.location.line > 1,
        "live disk bounds must not erase the compiled anchor"
    );
    assert_eq!(
        (
            coarse.location.byte_column,
            coarse.location.column,
            coarse.location.column_char
        ),
        (0, 1, 1)
    );
    assert!(coarse.warning.is_some());
    assert!(
        synctex::resolve_inverse(&pdf, expected, None, 4, &Operation::default(), None, false)
            .is_err()
    );
    let unverified_manifest = cache.join("unverified.json");
    let mut unverified: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    unverified["warnings"][source.to_str().unwrap()] = "source changed during compilation".into();
    fs::write(
        &unverified_manifest,
        serde_json::to_vec(&unverified).unwrap(),
    )
    .unwrap();
    let unverified_forward = synctex::resolve_forward_with_library(
        &pdf,
        &source,
        4,
        10,
        None,
        ForwardSource {
            snapshot: Some(&unverified_manifest),
            best_guess: true,
            anchor_only: false,
        },
    )
    .unwrap();
    assert!(
        unverified_forward.word.is_none(),
        "unverified cached text cannot establish literal precision"
    );
    assert!(unverified_forward.warning.is_some());
    let unverified_inverse = synctex::resolve_inverse(
        &pdf,
        expected,
        Some(("Compiled Copper owner remains stable", 9)),
        4,
        &Operation::default(),
        Some(&unverified_manifest),
        true,
    )
    .unwrap();
    assert!(!unverified_inverse.location.precise);
    assert!(unverified_inverse.location.line > 1);
    assert!(unverified_inverse.warning.is_some());
    fs::rename(&source, source.with_file_name("renamed.tex")).unwrap();
    let renamed =
        synctex::resolve_forward_with_library(&pdf, &source, 4, 10, None, context).unwrap();
    assert!(renamed.word.is_none());
    assert!(renamed.warning.is_some());
    let unavailable_history = synctex::resolve_forward_with_library(
        &pdf,
        &source,
        4,
        10,
        None,
        ForwardSource {
            snapshot: None,
            best_guess: true,
            anchor_only: true,
        },
    )
    .unwrap();
    assert_eq!(unavailable_history.page, renamed.page);
    assert!(unavailable_history.word.is_none());
    assert!(unavailable_history.warning.is_some());
    assert!(synctex::resolve_forward(&pdf, &source, 4, 10).is_err());
    // A recorded cache file is required authority, not optional live refinement.
    for malformed in [
        b"valid UTF-8 but not the published compiled source\n".to_vec(),
        vec![0xff],
        vec![b'x'; 2 * 1024 * 1024 + 1],
    ] {
        fs::write(cache.join("1.txt"), malformed).unwrap();
        let error = synctex::resolve_forward_with_library(
            &pdf,
            &source,
            4,
            10,
            None,
            ForwardSource {
                best_guess: true,
                anchor_only: true,
                ..context
            },
        )
        .unwrap_err();
        assert!(!synctex::is_forward_unavailable(&error));
        assert!(
            synctex::resolve_inverse(
                &pdf,
                expected,
                Some(("Compiled Copper owner remains stable", 9)),
                4,
                &Operation::default(),
                Some(&manifest),
                true,
            )
            .is_err()
        );
    }
    // A provenance warning must still validate each required captured entry.
    // It also covers the unhinted path, before source bounds/refinement.
    for cached in [
        Some(contents.as_bytes()),
        Some(b"valid UTF-8 rewrite".as_slice()),
        None,
    ] {
        if let Some(cached) = cached {
            fs::write(cache.join("1.txt"), cached).unwrap();
        } else {
            fs::remove_file(cache.join("1.txt")).unwrap();
        }
        for word in [None, Some(("Compiled Copper owner remains stable", 9))] {
            let result = synctex::resolve_inverse(
                &pdf,
                expected,
                word,
                4,
                &Operation::default(),
                Some(&unverified_manifest),
                true,
            );
            if cached == Some(contents.as_bytes()) {
                let result = result.unwrap();
                assert!(!result.location.precise);
                assert!(result.warning.is_some());
                assert!(result.location.line > 1);
            } else {
                assert!(
                    result.is_err(),
                    "provenance warning cannot waive required cache authority"
                );
            }
        }
    }
    fs::write(cache.join("1.txt"), contents).unwrap();
    // Even a warned LINE request requires the published compiler-root entry.
    // Editing the live main cannot repair or replace its captured authority.
    fs::write(&main_source, "\\relax\n").unwrap();
    for malformed in [
        Some(b"valid UTF-8 rewrite of the compiled root".as_slice()),
        Some(b"\xff".as_slice()),
        None,
    ] {
        if let Some(malformed) = malformed {
            fs::write(cache.join("2.txt"), malformed).unwrap();
        } else {
            fs::remove_file(cache.join("2.txt")).unwrap();
        }
        for (best_guess, anchor_only) in [(false, false), (true, false), (true, true)] {
            let error = synctex::resolve_forward_with_library(
                &pdf,
                &source,
                4,
                10,
                None,
                ForwardSource {
                    best_guess,
                    anchor_only,
                    ..context
                },
            )
            .unwrap_err();
            assert!(!synctex::is_forward_unavailable(&error));
            assert!(error.to_string().contains("required compiled source cache"));
        }
        let error = synctex::resolve_forward_with_library(
            &pdf,
            &source,
            4,
            10,
            None,
            ForwardSource {
                snapshot: Some(&unverified_manifest),
                best_guess: true,
                anchor_only: true,
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("required compiled source cache"));
    }
    fs::write(cache.join("2.txt"), main).unwrap();
    let missing_root_manifest = cache.join("missing-root.json");
    let mut missing_root: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    missing_root["files"]
        .as_object_mut()
        .unwrap()
        .remove(main_source.to_str().unwrap());
    missing_root["digests"]
        .as_object_mut()
        .unwrap()
        .remove(main_source.to_str().unwrap());
    fs::write(
        &missing_root_manifest,
        serde_json::to_vec(&missing_root).unwrap(),
    )
    .unwrap();
    let error = synctex::resolve_forward_with_library(
        &pdf,
        &source,
        4,
        10,
        None,
        ForwardSource {
            snapshot: Some(&missing_root_manifest),
            best_guess: true,
            anchor_only: true,
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("required compiled source cache"));
    fs::remove_file(cache.join("1.txt")).unwrap();
    std::os::unix::fs::symlink(source.with_file_name("renamed.tex"), cache.join("1.txt")).unwrap();
    assert!(
        synctex::resolve_forward_with_library(
            &pdf,
            &source,
            4,
            10,
            None,
            ForwardSource {
                best_guess: true,
                anchor_only: true,
                ..context
            },
        )
        .is_err()
    );
    assert!(
        synctex::resolve_inverse(
            &pdf,
            expected,
            None,
            4,
            &Operation::default(),
            Some(&manifest),
            true,
        )
        .is_err()
    );
    fs::remove_file(cache.join("1.txt")).unwrap();
    fs::write(cache.join("1.txt"), contents).unwrap();
    let no_map_pdf = root.join("no-map.pdf");
    fs::copy(&pdf, &no_map_pdf).unwrap();
    let no_map_info = SourceInfo::read(&no_map_pdf).unwrap();
    assert!(
        no_map_info.main.is_none()
            && no_map_info.inputs.is_empty()
            && no_map_info.warning.is_some()
    );
    // This is a valid publication for a PDF with no companion, not a stale
    // snapshot whose sidecar was deleted after its revision was captured.
    let no_map_manifest = cache.join("no-map.json");
    let mut no_map_snapshot: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    no_map_snapshot["pdf"] = serde_json::to_value(&no_map_pdf).unwrap();
    no_map_snapshot["revision"] = serde_json::to_value(no_map_info.revision).unwrap();
    fs::write(
        &no_map_manifest,
        serde_json::to_vec(&no_map_snapshot).unwrap(),
    )
    .unwrap();
    let no_map_context = ForwardSource {
        snapshot: Some(&no_map_manifest),
        best_guess: true,
        anchor_only: true,
    };
    let published = SourceSnapshot::load(&no_map_manifest, &no_map_pdf).unwrap();
    assert_eq!(published.read(&main_source).unwrap(), main);
    assert_eq!(published.read(&source).unwrap(), contents);
    assert!(synctex::is_forward_unavailable(
        &synctex::resolve_forward_with_library(&no_map_pdf, &source, 4, 10, None, no_map_context,)
            .unwrap_err()
    ));
    for (filename, original) in [("2.txt", main), ("1.txt", contents)] {
        for replacement in [Some(b"rewritten cache".as_slice()), None] {
            if let Some(replacement) = replacement {
                fs::write(cache.join(filename), replacement).unwrap();
            } else {
                fs::remove_file(cache.join(filename)).unwrap();
            }
            let error = synctex::resolve_forward_with_library(
                &no_map_pdf,
                &source,
                4,
                10,
                None,
                no_map_context,
            )
            .unwrap_err();
            assert!(!synctex::is_forward_unavailable(&error), "{error}");
            fs::write(cache.join(filename), original).unwrap();
        }
    }
    for (document, best_guess, expected_exit) in [
        (no_map_pdf, true, 3),
        (root.join("no-map.pdf"), false, 1),
        (root.join("missing.pdf"), true, 1),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pdfterm"));
        command
            .current_dir(&root)
            .env("XDG_CONFIG_HOME", root.join("cli-config"))
            .arg(document)
            .arg("--synctex-view")
            .arg(source.with_file_name("renamed.tex"));
        if best_guess {
            command.arg("--synctex-best-guess");
        }
        let output = process::output(&mut command, &Operation::default()).unwrap();
        assert_eq!(
            output.status.code(),
            Some(expected_exit),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let companion = pdf.with_extension("synctex.gz");
    fs::rename(&companion, companion.with_extension("backup")).unwrap();
    fs::write(&companion, b"new sidecar revision").unwrap();
    assert!(SourceSnapshot::load(&manifest, &pdf).is_err());
    assert!(synctex::resolve_forward_with_library(&pdf, &source, 4, 10, None, context).is_err());
}

#[test]
fn real_synctex_revisions_failed_hit_tests_and_coarse_refinement() {
    let _native = pdfium_test_lock();
    let directory = tempfile::tempdir().unwrap();
    let source_contents = include_str!("fixtures/navigation.tex");
    let source = directory.path().join("navigation.tex");
    fs::write(&source, source_contents).unwrap();
    let source = source.canonicalize().unwrap();
    let output = process::output(
        Command::new("pdflatex")
            .current_dir(directory.path())
            .args([
                "-interaction=nonstopmode",
                "-halt-on-error",
                "-synctex=1",
                "navigation.tex",
            ]),
        &Operation::new(Duration::from_secs(30)),
    )
    .expect("pdflatex is required for the real SyncTeX fixture");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let pdf = directory.path().join("navigation.pdf");
    let request = synctex::resolve_forward(&pdf, &source, 6, 1).unwrap();
    assert!(
        request.word.is_none() && request.warning.is_some(),
        "{request:?}"
    );
    let point = (
        request.h + request.width / 2.,
        request.v - request.height / 2.,
    );
    let inverse = synctex::resolve_inverse(
        &pdf,
        synctex::InversePoint {
            page: request.page,
            x: point.0,
            y_from_top: point.1,
            page_height_pt: 792.0,
        },
        Some(("target zephyr", 7)),
        4,
        &Operation::default(),
        None,
        false,
    )
    .unwrap();
    assert_eq!(inverse.location.line, 6);
    assert_eq!(inverse.location.file, source.to_str().unwrap());
    assert!(inverse.location.precise);
    let expected = fs::read_to_string(&source)
        .unwrap()
        .lines()
        .nth(5)
        .unwrap()
        .find("zephyr")
        .unwrap();
    assert_eq!(inverse.location.byte_column, expected);
    fs::write(&source, "\\relax\n").unwrap();
    let invalid_anchor = synctex::resolve_inverse(
        &pdf,
        synctex::InversePoint {
            page: request.page,
            x: point.0,
            y_from_top: point.1,
            page_height_pt: 792.0,
        },
        None,
        4,
        &Operation::default(),
        None,
        false,
    );
    assert!(
        invalid_anchor.is_err(),
        "a readable source shorter than its raw SyncTeX anchor must not navigate"
    );
    fs::write(&source, [0xff]).unwrap();
    assert!(synctex::resolve_forward(&pdf, &source, 6, 1).is_err());
    let coarse = synctex::resolve_inverse(
        &pdf,
        synctex::InversePoint {
            page: request.page,
            x: point.0,
            y_from_top: point.1,
            page_height_pt: 792.0,
        },
        Some(("zephyr", 0)),
        4,
        &Operation::default(),
        None,
        false,
    )
    .unwrap();
    assert_eq!(coarse.location.line, 6);
    assert!(!coarse.location.precise);
    assert!(
        coarse
            .warning
            .unwrap()
            .contains("source refinement unavailable")
    );
    for special in ["oversized", "fifo"] {
        fs::remove_file(&source).unwrap();
        if special == "oversized" {
            fs::write(&source, vec![b'x'; 2 * 1024 * 1024 + 1]).unwrap();
        } else {
            use std::os::unix::ffi::OsStrExt;
            let name = std::ffi::CString::new(source.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        }
        assert!(synctex::resolve_forward(&pdf, &source, 6, 1).is_err());
        let result = synctex::resolve_inverse(
            &pdf,
            synctex::InversePoint {
                page: request.page,
                x: point.0,
                y_from_top: point.1,
                page_height_pt: 792.0,
            },
            Some(("zephyr", 0)),
            4,
            &Operation::default(),
            None,
            false,
        )
        .unwrap();
        assert_eq!(result.location.line, 6);
        assert!(!result.location.precise);
        assert!(result.warning.is_some());
    }
    fs::remove_file(&source).unwrap();
    fs::write(&source, source_contents).unwrap();
    let mut word_request = synctex::resolve_forward(&pdf, &source, 6, expected as u32 + 1).unwrap();
    assert!(word_request.word.is_none() && word_request.warning.is_some());
    // The fixture owns its literal text. This hint tests worker WORD paint,
    // not source-resolver proof of an arbitrary compiled TeX program.
    word_request.word = Some(synctex::ForwardWord {
        words: [
            "Second",
            "paragraph",
            "contains",
            "the",
            "distinct",
            "target",
            "zephyr",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        selected: 6,
    });

    // Finish this native transaction, including both reload revisions, before
    // the fixture lock allows another resolver or worker to open PDFium.
    let worker = RenderWorker::spawn(1, pdf.clone(), None);
    assert_eq!(worker.wait_until_ready().unwrap().0, 2);
    let key = RenderKey {
        document_id: 1,
        page: 0,
        width: 600,
        height: 800,
        zoom: 100,
        fit: FitMode::Width,
        invert: false,
        dark_mode_style: DarkModeStyle::new([0; 3], [255; 3]),
        search_request_id: 0,
        search_highlight: [255, 255, 0],
        link_mode: false,
        link_highlight: [255, 255, 0],
        selected_link_ordinal: None,
    };
    worker.begin_generation(1);
    worker.flash(
        1,
        word_request.page - 1,
        word_request.rect(),
        word_request.word,
    );
    worker.render(RenderRequest { key, generation: 1 }).unwrap();
    let WorkerMessage::Frame(frame) = message(&worker) else {
        panic!("expected displayed frame")
    };
    let displayed = frame.revision;
    assert_eq!(displayed, DocumentRevision::read(&pdf).unwrap());
    let highlight = frame.flash.as_ref().unwrap();
    assert!(highlight.error.is_none());
    assert!(highlight.word_precise);
    assert!(highlight.rect.left > request.h);
    worker.flash(1, request.page - 1, request.rect(), None);
    worker.render(RenderRequest { key, generation: 1 }).unwrap();
    let WorkerMessage::Frame(coarse) = message(&worker) else {
        panic!("expected coarse forward frame")
    };
    let coarse = coarse.flash.as_ref().unwrap();
    assert!(!coarse.word_precise);
    assert!(coarse.error.is_none());
    assert!((coarse.rect.right - coarse.rect.left - request.width).abs() < 0.001);
    for (id, bad_key, success) in [
        (1, RenderKey { page: 99, ..key }, false),
        (
            2,
            RenderKey {
                document_id: 99,
                ..key
            },
            false,
        ),
        (3, key, true),
    ] {
        worker.page_point(displayed, id, 100, 100, bad_key);
        let WorkerMessage::PagePoint {
            request_id, result, ..
        } = message(&worker)
        else {
            panic!("missing hit-test completion")
        };
        assert_eq!(request_id, id);
        assert_eq!(result.is_ok(), success);
    }
    let companion = pdf.with_extension("synctex.gz");
    let contents = fs::read(&companion).unwrap();
    fs::remove_file(&companion).unwrap();
    fs::write(&companion, contents).unwrap();
    assert!(
        displayed.check(&pdf).is_err(),
        "companion-only replacement must invalidate old pixels"
    );
    worker.open(1, pdf.clone()).unwrap();
    assert!(matches!(message(&worker), WorkerMessage::Opened { .. }));
    worker.page_point(displayed, 4, 100, 100, key);
    let WorkerMessage::PagePoint { result, .. } = message(&worker) else {
        panic!("missing stale reply")
    };
    assert!(result.unwrap_err().contains("revision"));
    worker.page_point(DocumentRevision::read(&pdf).unwrap(), 5, 100, 100, key);
    let WorkerMessage::PagePoint { result, .. } = message(&worker) else {
        panic!("missing recovered reply")
    };
    assert!(result.is_ok());
    worker.close(1);
    worker.page_point(DocumentRevision::read(&pdf).unwrap(), 6, 100, 100, key);
    let WorkerMessage::PagePoint { result, .. } = message(&worker) else {
        panic!("missing closed-document reply")
    };
    assert!(result.is_err());
}

fn compile_out_of_tree_fixture(root: &Path, contents: &str) -> (PathBuf, PathBuf) {
    fs::create_dir(root.join("sources")).unwrap();
    fs::create_dir(root.join("build")).unwrap();
    let source = root.join("sources/original.tex");
    fs::write(&source, contents).unwrap();
    let output = process::output(
        Command::new("pdflatex")
            .current_dir(root)
            .args([
                "-interaction=nonstopmode",
                "-halt-on-error",
                "-synctex=1",
                "-output-directory=build",
                "-jobname=deck",
            ])
            .arg(&source),
        &Operation::new(Duration::from_secs(30)),
    )
    .expect("pdflatex is required for the real SyncTeX fixture");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    (root.join("build/deck.pdf"), source)
}

fn painted_word_occurrences(
    pdf: &Path,
    word: &str,
) -> Vec<(synctex::InversePoint, synctex::InversePoint)> {
    let _native = pdfium_test_lock();
    let worker = RenderWorker::spawn(1, pdf.to_owned(), None);
    let (pages, _, _) = worker.wait_until_ready().unwrap();
    let revision = DocumentRevision::read(pdf).unwrap();
    let mut keys = Vec::with_capacity(pages as usize);
    for page in 0..pages {
        let key = RenderKey {
            document_id: 1,
            page,
            width: 800,
            height: 600,
            zoom: 100,
            fit: FitMode::Page,
            invert: false,
            dark_mode_style: DarkModeStyle::new([0; 3], [255; 3]),
            search_request_id: 0,
            search_highlight: [255, 255, 0],
            link_mode: false,
            link_highlight: [255, 255, 0],
            selected_link_ordinal: None,
        };
        worker.render(RenderRequest { key, generation: 0 }).unwrap();
        let WorkerMessage::Frame(frame) = message(&worker) else {
            panic!("expected fixture page frame");
        };
        keys.push((key, frame.width, frame.height));
    }
    worker
        .find_visible(1, 1, revision, word.to_owned(), keys)
        .unwrap();
    let WorkerMessage::VisibleMatches { matches, .. } = message(&worker) else {
        panic!("expected painted fixture word matches");
    };
    assert!(!matches.is_empty(), "fixture word {word} must be painted");
    let mut occurrences = Vec::with_capacity(matches.len());
    for matched in &matches {
        let left = matched.rects.iter().map(|rect| rect.left).min().unwrap();
        let right = matched.rects.iter().map(|rect| rect.right).max().unwrap();
        let top = matched.rects.iter().map(|rect| rect.top).min().unwrap();
        let bottom = matched.rects.iter().map(|rect| rect.bottom).max().unwrap();
        let mut points = [None, None];
        for (index, (x, y)) in [matched.hit, ((left + right) / 2, (top + bottom) / 2)]
            .into_iter()
            .enumerate()
        {
            worker.page_point(revision, index as u64 + 2, x, y, matched.key);
            let WorkerMessage::PagePoint { result, .. } = message(&worker) else {
                panic!("expected fixture glyph point");
            };
            let click = result.unwrap();
            let (context, offset) = click.text.unwrap().expect("fixture glyph must be hit");
            assert!(
                context
                    .match_indices(word)
                    .any(|(start, word)| { (start..start + word.len()).contains(&offset) }),
                "hit offset {offset} in {context:?} is not inside fixture word {word}"
            );
            points[index] = Some(click.synctex);
        }
        occurrences.push((points[0].unwrap(), points[1].unwrap()));
    }
    // Finish native document destruction before releasing the fixture lock.
    worker.close(1);
    worker.page_point(revision, 4, 0, 0, matches[0].key);
    let WorkerMessage::PagePoint { result, .. } = message(&worker) else {
        panic!("expected closed native fixture reply");
    };
    assert!(result.is_err());
    occurrences
}

fn painted_word_points(pdf: &Path, word: &str) -> (synctex::InversePoint, synctex::InversePoint) {
    let mut occurrences = painted_word_occurrences(pdf, word);
    assert_eq!(occurrences.len(), 1, "fixture word {word} must be unique");
    occurrences.pop().unwrap()
}

#[test]
fn real_fragile_frames_remap_by_clicked_page_with_separate_output_directory() {
    let contents = r"\documentclass{beamer}
\title{GlobalAurora Metadata Example}
\begin{document}
\begin{frame}
\titlepage
\end{frame}
\begin{frame}[fragile]{First Original Frame}
EarlyZephyr belongs only to the first frame.
FirstCopper is another distinctive sentence here.
\end{frame}
\begin{frame}[fragile]{Second Original Frame}
LateNebula belongs only to the second frame.
SecondSilver is another distinctive sentence here.
\end{frame}
\end{document}
";
    let directory = tempfile::tempdir().unwrap();
    let (pdf, source) = compile_out_of_tree_fixture(directory.path(), contents);
    // A same-basename sibling is not source-map/project evidence.
    fs::write(
        pdf.with_extension("tex"),
        "\\title{Wrong GlobalAurora Metadata Example}\n",
    )
    .unwrap();
    for (word, page) in [("EarlyZephyr", 2), ("LateNebula", 3)] {
        let (point, center) = painted_word_points(&pdf, word);
        assert_eq!(point.page, page);
        let inverse = synctex::resolve_inverse(
            &pdf,
            point,
            Some((word, 0)),
            4,
            &Operation::default(),
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            Path::new(&inverse.location.file),
            fs::canonicalize(&source).unwrap()
        );
        let expected = contents.lines().position(|row| row.contains(word)).unwrap() as u32 + 1;
        assert_eq!(inverse.location.line, expected);
        assert_eq!(inverse.location.byte_column, 0);
        assert!(inverse.location.precise);
        assert!(inverse.warning.is_none());
        let coarse =
            synctex::resolve_inverse(&pdf, center, None, 4, &Operation::default(), None, false)
                .unwrap();
        assert_eq!(
            Path::new(&coarse.location.file),
            fs::canonicalize(&source).unwrap()
        );
        let closing = contents
            .lines()
            .skip(expected as usize - 1)
            .position(|row| row == "\\end{frame}")
            .unwrap() as u32
            + expected;
        assert_eq!(coarse.location.line, closing);
        assert!(!coarse.location.precise);
        assert!(coarse.warning.is_none());
    }
    let (point, _) = painted_word_points(&pdf, "GlobalAurora");
    assert_eq!(point.page, 1);
    let metadata = synctex::resolve_inverse(
        &pdf,
        point,
        Some(("GlobalAurora Metadata Example", 0)),
        4,
        &Operation::default(),
        None,
        false,
    )
    .unwrap();
    assert_eq!(
        Path::new(&metadata.location.file),
        fs::canonicalize(&source).unwrap()
    );
    assert_eq!(metadata.location.line, 2);
    assert!(metadata.location.precise);
    assert!(metadata.warning.is_none());
}

#[test]
fn real_macro_definition_and_same_word_label_stay_coarse() {
    let contents = r"\documentclass{article}
\usepackage{hyperref}
\newcommand{\Hidden}{Zephyr}
\begin{document}
\Hidden\label{Zephyr}

Printed prose is visible here.
éé \href{https://example.test/DisplayAmber}{DisplayAmber stays printed.}
\begin{figure}[h]
\caption{PrintedCopper remains visible.}
\end{figure}
\end{document}
";
    let directory = tempfile::tempdir().unwrap();
    let (pdf, source) = compile_out_of_tree_fixture(directory.path(), contents);
    let (point, _) = painted_word_points(&pdf, "Zephyr");
    let coarse =
        synctex::resolve_inverse(&pdf, point, None, 4, &Operation::default(), None, false).unwrap();
    let inverse = synctex::resolve_inverse(
        &pdf,
        point,
        Some(("Zephyr", 0)),
        4,
        &Operation::default(),
        None,
        false,
    )
    .unwrap();
    assert_eq!(
        Path::new(&inverse.location.file),
        fs::canonicalize(&source).unwrap()
    );
    assert_eq!(inverse.location.line, coarse.location.line);
    assert!(
        !inverse.location.precise,
        "macro body/label key cannot prove printed text"
    );
    assert_eq!(
        (
            inverse.location.byte_column,
            inverse.location.column,
            inverse.location.column_char
        ),
        (0, 1, 1)
    );
    assert!(inverse.warning.is_none());
    for word in ["DisplayAmber", "PrintedCopper"] {
        let (point, _) = painted_word_points(&pdf, word);
        let inverse = synctex::resolve_inverse(
            &pdf,
            point,
            Some((word, 0)),
            4,
            &Operation::default(),
            None,
            false,
        )
        .unwrap();
        let (row, text) = contents
            .lines()
            .enumerate()
            .find(|(_, row)| row.contains(word))
            .unwrap();
        let byte = text.rfind(word).unwrap();
        assert_eq!(inverse.location.line, row as u32 + 1);
        assert_eq!(inverse.location.byte_column, byte);
        assert_eq!(
            inverse.location.column,
            text[..byte].encode_utf16().count() + 1
        );
        assert_eq!(
            inverse.location.column_char,
            text[..byte].chars().count() + 1
        );
        assert!(inverse.location.precise);
        assert!(inverse.warning.is_none());
    }
}

#[test]
fn real_fragile_macro_title_and_footer_do_not_refine_to_duplicate_body() {
    let contents = r"\documentclass{beamer}
\newcommand{\DocTitle}{FooterZephyr}
\title[\DocTitle]{\DocTitle}
\setbeamertemplate{footline}{%
  \leavevmode%
  \hbox{%
    \begin{beamercolorbox}[wd=\paperwidth,ht=2.5ex,dp=1.125ex,right]{title in head/foot}%
      \usebeamerfont{title in head/foot}\insertshorttitle\hspace*{2em}%
    \end{beamercolorbox}%
  }%
}
\begin{document}
\begin{frame}
\titlepage
\end{frame}
\begin{frame}[fragile]{\DocTitle}
\centering Body repeats FooterZephyr as a literal word.
\end{frame}
\end{document}
";
    let directory = tempfile::tempdir().unwrap();
    let (pdf, source) = compile_out_of_tree_fixture(directory.path(), contents);
    let mut occurrences: Vec<_> = painted_word_occurrences(&pdf, "FooterZephyr")
        .into_iter()
        .filter(|(_, point)| point.page == 2)
        .collect();
    occurrences.sort_by(|a, b| a.1.y_from_top.total_cmp(&b.1.y_from_top));
    assert_eq!(
        occurrences.len(),
        3,
        "frame title, literal body, and running footer"
    );
    for (index, (_, point)) in occurrences.iter().enumerate() {
        let inverse = synctex::resolve_inverse(
            &pdf,
            *point,
            Some(("FooterZephyr", 0)),
            4,
            &Operation::default(),
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            Path::new(&inverse.location.file),
            fs::canonicalize(&source).unwrap()
        );
        assert!(inverse.warning.is_none());
        if index == 1 {
            assert_eq!(inverse.location.line, 17);
            assert_eq!(inverse.location.byte_column, 24);
            assert!(inverse.location.precise);
        } else {
            assert_eq!(inverse.location.line, 18);
            assert_eq!(inverse.location.byte_column, 0);
            assert!(
                !inverse.location.precise,
                "macro title/footer must not borrow body prose"
            );
        }
    }
}
