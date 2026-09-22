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
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let tex = root.join("../Documents/stuff/talks/cours/bonn_hsm_2026/bonn_hsm_2026.tex");
    let Ok(tex) = fs::canonicalize(&tex) else {
        return; // Deck absent on other machines.
    };
    let pdf = tex.with_extension("pdf");
    let operation = process::Operation::new(Duration::from_secs(30));

    // Line 4713 sits inside the immunofluorescence frame (4708..4730); the raw
    // sidecar maps it to the preceding multifiltration frame's pages.
    let interior = synctex::resolve_forward(&pdf, &tex, 4713, 3).unwrap();
    assert_eq!(interior.page, 196, "interior line must show its own frame");
    assert!(interior.word.is_none(), "includegraphics line has no words");

    // The closing line itself is unaffected.
    let closing = synctex::resolve_forward(&pdf, &tex, 4730, 3).unwrap();
    assert_eq!(closing.page, 196);

    // Inverse: any interior point of page 196 refines into the frame body.
    let inverse = synctex::resolve_inverse(
        &pdf,
        196,
        160.0,
        130.0,
        Some(("Consider the two following image functions", 0)),
        4,
        &operation,
    )
    .unwrap();
    assert_eq!(
        inverse.location.line, 4709,
        "prose context picks the body line"
    );
    assert!(inverse.location.precise);
    assert!(inverse.warning.is_none());
}
