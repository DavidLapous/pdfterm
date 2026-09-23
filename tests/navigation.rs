use pdfterm::{
    pdf::{DarkModeStyle, FitMode, RenderKey, RenderRequest, RenderWorker, WorkerMessage},
    process::{self, Operation},
    synctex::{self, DocumentRevision},
};
use std::{
    fs,
    process::Command,
    time::{Duration, Instant},
};

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
fn beamer_overlay_forward_uses_visible_source_context() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("overlay.tex");
    fs::write(
        &source,
        r"\documentclass{beamer}
\begin{document}
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
        synctex::resolve_forward(&pdf, &source, 5, 7).unwrap().page,
        1
    );
    assert_eq!(
        synctex::resolve_forward(&pdf, &source, 7, 7).unwrap().page,
        2
    );
    assert_eq!(
        synctex::resolve_forward(&pdf, &source, 9, 12).unwrap().page,
        3
    );
    assert_eq!(
        synctex::resolve_forward(&pdf, &source, 9, 1).unwrap().page,
        1
    );
}

#[test]
fn real_synctex_revisions_failed_hit_tests_and_coarse_refinement() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("navigation.tex");
    fs::write(&source, include_str!("fixtures/navigation.tex")).unwrap();
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
    let point = (
        request.h + request.width / 2.,
        request.v - request.height / 2.,
    );
    let inverse = synctex::resolve_inverse(
        &pdf,
        request.page,
        point.0,
        point.1,
        Some(("target zephyr", 7)),
        4,
        &Operation::default(),
    )
    .unwrap();
    assert_eq!(inverse.location.line, 6);
    assert!(inverse.location.precise);
    let expected = fs::read_to_string(&source)
        .unwrap()
        .lines()
        .nth(5)
        .unwrap()
        .find("zephyr")
        .unwrap();
    assert_eq!(inverse.location.byte_column, expected);
    fs::write(&source, [0xff]).unwrap();
    assert!(synctex::resolve_forward(&pdf, &source, 6, 1).is_err());
    let coarse = synctex::resolve_inverse(
        &pdf,
        request.page,
        point.0,
        point.1,
        Some(("zephyr", 0)),
        4,
        &Operation::default(),
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
            request.page,
            point.0,
            point.1,
            Some(("zephyr", 0)),
            4,
            &Operation::default(),
        )
        .unwrap();
        assert_eq!(result.location.line, 6);
        assert!(!result.location.precise);
        assert!(result.warning.is_some());
    }
    fs::remove_file(&source).unwrap();
    fs::write(&source, include_str!("fixtures/navigation.tex")).unwrap();
    let word_request = synctex::resolve_forward(&pdf, &source, 6, expected as u32 + 1).unwrap();

    // One PDFium lifetime in this process, including both reload revisions.
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
    assert!(highlight.rect.right - highlight.rect.left < request.width / 2.0);
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

/// Real Bonn deck: Beamer maps frame bodies to the closing line, so forward
/// search from an interior line must select the frame's own pages, not the
/// preceding frame's, and inverse clicks refine into the frame body.
#[test]
fn beamer_frame_body_navigates_to_own_frame() {
    let default_tex = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/"))
        .join("Documents/stuff/talks/cours/bonn_hsm_2026/bonn_hsm_2026.tex");
    let tex = std::env::var_os("PDFTERM_BEAMER_TEX").map_or(default_tex, std::path::PathBuf::from);
    let Ok(tex) = fs::canonicalize(&tex) else {
        eprintln!("skipping beamer_frame_body_navigates_to_own_frame: deck absent at {tex:?}");
        return;
    };
    let pdf = tex.with_extension("pdf");
    let operation = process::Operation::new(Duration::from_secs(30));

    // Locate the immunofluorescence frame dynamically: the deck is the
    // user's live document, so line numbers drift between edits.
    let source = fs::read_to_string(&tex).unwrap();
    let begin = source
        .lines()
        .position(|row| {
            row.contains(r"\begin{frame}{Multiparameter persistence motivation}{Immunofluoresc")
        })
        .expect("immunofluorescence frame in deck")
        + 1;
    let end = source
        .lines()
        .enumerate()
        .skip(begin - 1)
        .find(|(_, row)| row.trim_start().starts_with(r"\end{frame}"))
        .expect("closing frame line")
        .0
        + 1;
    let image_line = source
        .lines()
        .position(|row| row.contains("immune2.png") && !row.trim_start().starts_with('%'))
        .expect("immune2 includegraphics line")
        + 1;
    assert!(
        begin < image_line && image_line < end,
        "includegraphics sits inside the frame"
    );

    let interior = synctex::resolve_forward(&pdf, &tex, image_line as u32, 3).unwrap();
    let page = interior.page;
    assert_eq!(
        interior.page,
        synctex::resolve_forward(&pdf, &tex, end as u32, 3)
            .unwrap()
            .page,
        "interior line must show its own frame"
    );
    assert!(interior.word.is_none(), "includegraphics line has no words");

    // The closing line itself is unaffected.
    let closing = synctex::resolve_forward(&pdf, &tex, end as u32, 3).unwrap();
    assert_eq!(closing.page, page);

    // Inverse: the raw sidecar resolves the clicked point within the deck's
    // live layout, which drifts between builds, so only the resolver-guaranteed
    // invariants are asserted: a positive line, no precise refinement (the
    // figure environment blocks precision throughout the frame scope per the
    // documented opaque-region contract), and line-only navigation without a
    // warning.
    let inverse = synctex::resolve_inverse(
        &pdf,
        page,
        160.0,
        130.0,
        Some(("Consider the two following image functions", 0)),
        4,
        &operation,
    )
    .unwrap();
    assert!(inverse.location.line > 0, "resolved line is positive");
    assert!(!inverse.location.precise, "figure blocks precision");
    assert!(inverse.warning.is_none());
}
