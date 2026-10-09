//! External navigation owns its cancellation/deadline, never the render thread.
use crate::{
    editor::Editor,
    pdf::{DocumentId, ResolvedClick},
    process::Operation,
    synctex::{self, DocumentRevision, InverseResolution},
};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    thread::{self, JoinHandle},
};

pub(crate) struct Coordinator {
    // Drop cancels the active request before joining the external worker.
    pub inverse: Option<PendingInverse>,
    pub worker: NavigationWorker,
    pub forward: Option<PendingForward>,
    pub flash: Option<PendingFlash>,
    pub next_request_id: u64,
    // The retained resolver validates each displayed revision, including reloads.
    pub source_maps: HashMap<PathBuf, String>,
    pub source_snapshots: HashMap<PathBuf, SourceState>,
}
impl Coordinator {
    pub fn new() -> Self {
        Self {
            inverse: None,
            worker: NavigationWorker::new(),
            forward: None,
            flash: None,
            next_request_id: 1,
            source_maps: HashMap::new(),
            source_snapshots: HashMap::new(),
        }
    }
}

pub(crate) struct SourceState {
    pub revision: DocumentRevision,
    pub snapshot: Option<PathBuf>,
    pub best_guess: bool,
}
impl SourceState {
    pub fn snapshot_for_revision(&self, revision: DocumentRevision) -> Option<&Path> {
        (self.revision == revision)
            .then_some(self.snapshot.as_deref())
            .flatten()
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForwardStage {
    AwaitingDocument,
    AwaitingFrame,
}
pub(crate) struct PendingForward {
    pub request: synctex::ForwardRequest,
    pub document_revision: Option<DocumentRevision>,
    pub source_state: Option<SourceState>,
    pub reply: crate::ipc::ForwardReply,
    pub deadline: std::time::Instant,
    pub stage: ForwardStage,
}
impl PendingForward {
    pub fn check_revision(&self) -> io::Result<()> {
        match self.document_revision {
            Some(revision) => revision.check(&self.request.pdf),
            None => self.request.revision.check(&self.request.pdf),
        }
    }
}
pub(crate) struct PendingFlash {
    pub document_id: DocumentId,
    pub revision: synctex::PdfRevision,
    pub page: u32,
    pub positioning_pending: bool,
    pub expires_at: Option<std::time::Instant>,
}

pub(crate) enum InverseStage {
    HitTest,
    Resolving,
}
pub(crate) struct PendingInverse {
    pub document_id: DocumentId,
    pub page: u32,
    pub request_id: u64,
    pub revision: DocumentRevision,
    pub operation: Operation,
    pub stage: InverseStage,
}
impl Drop for PendingInverse {
    fn drop(&mut self) {
        self.operation.cancel();
    }
}
pub(crate) struct InverseTask {
    pub request_id: u64,
    pub path: PathBuf,
    pub revision: DocumentRevision,
    pub page: u32,
    pub click: ResolvedClick,
    pub word_precision: bool,
    pub radius: u32,
    pub editor: Editor,
    pub inverse_search: Option<String>,
    pub source_snapshot: Option<PathBuf>,
    pub best_guess: bool,
    pub operation: Operation,
}
pub(crate) struct InverseReply {
    pub request_id: u64,
    pub result: io::Result<InverseResolution>,
}
impl InverseTask {
    fn resolve(&self) -> io::Result<InverseResolution> {
        self.operation.check()?;
        self.revision.check(&self.path)?;
        let word = if self.word_precision {
            self.click
                .text
                .as_ref()
                .ok()
                .and_then(|v| v.as_ref())
                .map(|(s, i)| (s.as_str(), *i))
        } else {
            None
        };
        let point = synctex::InversePoint {
            page: self.page + 1,
            ..if self.inverse_search.is_some() {
                self.click.typst
            } else {
                self.click.synctex
            }
        };
        let mut result = if let Some(endpoint) = &self.inverse_search {
            crate::typst::resolve_inverse(endpoint, self.revision.pdf, point, &self.operation)?
        } else {
            synctex::resolve_inverse(
                &self.path,
                point,
                word,
                self.radius,
                &self.operation,
                self.source_snapshot.as_deref(),
                self.best_guess,
            )?
        };
        if self.inverse_search.is_none()
            && self.word_precision
            && let Err(error) = &self.click.text
        {
            let warning = format!("line-only navigation: PDF text refinement unavailable: {error}");
            result.warning = Some(match result.warning.take() {
                Some(previous) => format!("{previous}; {warning}"),
                None => warning,
            });
        }
        if let Some(warning) = &mut result.warning
            && warning.len() > 4096
        {
            let mut end = 4096;
            while !warning.is_char_boundary(end) {
                end -= 1;
            }
            warning.truncate(end);
        }
        self.operation.check()?;
        self.revision.check(&self.path)?;
        self.editor.deliver(
            &result.location,
            &self.operation,
            Some(crate::editor::SourceDocument {
                pdf: &self.path,
                revision: self.revision.pdf,
                document_revision: self.revision,
                source_snapshot: self.source_snapshot.as_deref(),
                warning: result.warning.as_deref(),
            }),
        )?;
        Ok(result)
    }
}

pub(crate) struct NavigationWorker {
    requests: Option<Sender<InverseTask>>,
    queued: Receiver<InverseTask>,
    pub replies: Receiver<InverseReply>,
    thread: Option<JoinHandle<()>>,
}
impl NavigationWorker {
    pub fn new() -> Self {
        let (requests, receiver) = bounded::<InverseTask>(1);
        let (sender, replies) = bounded(1);
        let queued = receiver.clone();
        let thread = thread::spawn(move || {
            while let Ok(task) = receiver.recv() {
                let reply = InverseReply {
                    request_id: task.request_id,
                    result: task.resolve(),
                };
                if sender.send(reply).is_err() {
                    break;
                }
            }
        });
        Self {
            requests: Some(requests),
            queued,
            replies,
            thread: Some(thread),
        }
    }
    pub fn submit(&self, task: InverseTask) -> io::Result<()> {
        let requests = self.requests.as_ref().expect("live worker");
        let mut task = task;
        loop {
            if self.thread.as_ref().is_none_or(JoinHandle::is_finished) {
                return Err(io::Error::other("navigation worker stopped"));
            }
            match requests.try_send(task) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Full(latest)) => {
                    // The UI already cancels the superseded active request.
                    // Replace its queued successor instead of losing this click.
                    if let Ok(obsolete) = self.queued.try_recv() {
                        obsolete.operation.cancel();
                    }
                    task = latest;
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err(io::Error::other("navigation worker stopped"));
                }
            }
        }
    }
}
impl Drop for NavigationWorker {
    fn drop(&mut self) {
        while let Ok(task) = self.queued.try_recv() {
            task.operation.cancel();
        }
        self.requests.take();
        // Drain completions while shutting down so the bounded reply queue
        // cannot keep the worker alive. Active operations have a hard deadline.
        if let Some(thread) = self.thread.take() {
            while !thread.is_finished() {
                let _ = self
                    .replies
                    .recv_timeout(std::time::Duration::from_millis(5));
            }
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        io::{Read, Write},
        os::unix::{fs::PermissionsExt, net::UnixStream},
        process::Command,
        time::{Duration, Instant},
    };

    struct InverseHarness {
        directory: tempfile::TempDir,
        path: PathBuf,
        map: crate::ipc::Listener,
        editor: crate::ipc::Listener,
    }
    impl InverseHarness {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let path = directory.path().join("navigation.pdf");
            fs::write(&path, b"stable inverse-navigation revision").unwrap();
            let map = crate::ipc::Listener::bind(&directory.path().join("map.sock")).unwrap();
            let editor = crate::ipc::Listener::bind(&directory.path().join("editor.sock")).unwrap();
            Self {
                directory,
                path,
                map,
                editor,
            }
        }
        fn task(&self, request_id: u64) -> InverseTask {
            InverseTask {
                request_id,
                path: self.path.clone(),
                revision: DocumentRevision::read(&self.path).unwrap(),
                page: 0,
                click: ResolvedClick {
                    synctex: crate::synctex::InversePoint {
                        page: 1,
                        x: request_id as f32,
                        y_from_top: 580.,
                        page_height_pt: 600.,
                    },
                    typst: crate::synctex::InversePoint {
                        page: 1,
                        x: request_id as f32,
                        y_from_top: 580.,
                        page_height_pt: 600.,
                    },
                    text: Ok(None),
                },
                word_precision: false,
                radius: 4,
                editor: Editor::Socket {
                    path: self
                        .directory
                        .path()
                        .join("editor.sock")
                        .to_str()
                        .unwrap()
                        .into(),
                },
                inverse_search: Some(
                    self.directory
                        .path()
                        .join("map.sock")
                        .to_str()
                        .unwrap()
                        .into(),
                ),
                source_snapshot: None,
                best_guess: false,
                operation: Operation::new(Duration::from_secs(5)),
            }
        }
        fn accept(listener: &crate::ipc::Listener) -> UnixStream {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match listener.socket.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(true).unwrap();
                        return stream;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "inverse IPC did not connect");
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("inverse IPC accept failed: {error}"),
                }
            }
        }
        fn read(stream: &mut UnixStream) -> serde_json::Value {
            let mut bytes = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match stream.read_to_end(&mut bytes) {
                    Ok(_) => return serde_json::from_slice(&bytes).unwrap(),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "inverse IPC did not finish");
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => panic!("inverse IPC read failed: {error}"),
                }
            }
        }
        fn stalled_worker(&self) -> (NavigationWorker, Operation, UnixStream) {
            let worker = NavigationWorker::new();
            let completed = self.task(1);
            completed.operation.cancel();
            worker.submit(completed).unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while worker.replies.is_empty() {
                assert!(Instant::now() < deadline, "cancellation did not complete");
                thread::sleep(Duration::from_millis(1));
            }

            // Keep the first completion queued. The next completion therefore
            // blocks the worker before it can take any queued replacement.
            let active = self.task(2);
            let operation = active.operation.clone();
            worker.submit(active).unwrap();
            let mut stream = Self::accept(&self.map);
            assert_eq!(Self::read(&mut stream)["x"].as_f64(), Some(2.));
            (worker, operation, stream)
        }
    }

    #[test]
    fn inverse_burst_replaces_canceled_queue_and_only_delivers_latest_click() {
        let harness = InverseHarness::new();
        let (worker, active, stalled) = harness.stalled_worker();
        active.cancel();

        let queued = harness.task(3);
        let obsolete = queued.operation.clone();
        worker.submit(queued).unwrap();
        let replacement = harness.task(4);
        let mut operation = replacement.operation.clone();
        worker.submit(replacement).unwrap();
        assert!(obsolete.is_cancelled());
        for request_id in 5..=12 {
            operation.cancel();
            let latest = harness.task(request_id);
            operation = latest.operation.clone();
            worker.submit(latest).unwrap();
        }
        drop(stalled);

        for request_id in [1, 2] {
            let reply = worker.replies.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(reply.request_id, request_id);
            assert!(
                matches!(reply.result, Err(error) if error.kind() == io::ErrorKind::Interrupted)
            );
        }
        let mut mapped = InverseHarness::accept(&harness.map);
        assert_eq!(InverseHarness::read(&mut mapped)["x"].as_f64(), Some(12.));
        let source = harness.directory.path().join("latest.typ");
        mapped
            .write_all(
                &serde_json::to_vec(&serde_json::json!({
                    "ok": true,
                    "location": {
                        "file": source,
                        "line": 37,
                        "byte_column": 0,
                        "column": 1,
                        "column_char": 1,
                        "precise": true
                    }
                }))
                .unwrap(),
            )
            .unwrap();
        drop(mapped);

        let mut editor = InverseHarness::accept(&harness.editor);
        let delivered = InverseHarness::read(&mut editor);
        assert_eq!(delivered["file"].as_str(), source.to_str());
        assert_eq!(delivered["line"].as_u64(), Some(37));
        let reply = worker.replies.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(reply.request_id, 12);
        assert_eq!(reply.result.unwrap().location.line, 37);
        assert!(
            matches!(harness.map.socket.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
        assert!(
            matches!(harness.editor.socket.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
    }

    #[test]
    fn inverse_shutdown_discards_queued_work_and_drains_blocked_completions() {
        let harness = InverseHarness::new();
        let (worker, active, stalled) = harness.stalled_worker();
        let queued = harness.task(3);
        let operation = queued.operation.clone();
        worker.submit(queued).unwrap();
        // Coordinator drops PendingInverse before the worker.
        active.cancel();
        let started = Instant::now();
        drop(worker);
        drop(stalled);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(operation.is_cancelled());
        assert!(
            matches!(harness.map.socket.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
        assert!(
            matches!(harness.editor.socket.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
    }

    #[test]
    fn snapshot_authority_requires_the_full_revision_independently_of_mode() {
        let directory = tempfile::tempdir().unwrap();
        let pdf = directory.path().join("document.pdf");
        fs::write(&pdf, b"stable document revision").unwrap();
        let revision = DocumentRevision::read(&pdf).unwrap();
        let snapshot = directory.path().join("manifest.json");
        fs::write(pdf.with_extension("synctex"), b"new companion revision").unwrap();
        let reloaded = DocumentRevision::read(&pdf).unwrap();
        assert_eq!(revision.pdf, reloaded.pdf);
        assert_ne!(revision, reloaded);
        for best_guess in [false, true] {
            let state = SourceState {
                revision,
                snapshot: Some(snapshot.clone()),
                best_guess,
            };
            assert_eq!(
                state.snapshot_for_revision(revision),
                Some(snapshot.as_path())
            );
            assert!(state.snapshot_for_revision(reloaded).is_none());
        }
    }

    #[test]
    fn failed_pdf_text_preserves_real_synctex_navigation_and_stale_pairs_fail() {
        use sha2::Digest;
        let _native = crate::pdf::pdfium_test_lock();
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let source = root.join("navigation.tex");
        let contents = include_str!("../tests/fixtures/navigation.tex");
        fs::write(&source, contents).unwrap();
        let output = crate::process::output(
            Command::new("pdflatex").current_dir(&root).args([
                "-interaction=nonstopmode",
                "-halt-on-error",
                "-synctex=1",
                "navigation.tex",
            ]),
            &Operation::new(Duration::from_secs(30)),
        )
        .expect("pdflatex is required for the navigation fixture");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let path = root.join("navigation.pdf");
        let forward = synctex::resolve_forward(&path, &source, 6, 1).unwrap();
        let mut task = InverseTask {
            request_id: 1,
            revision: DocumentRevision::read(&path).unwrap(),
            path,
            page: forward.page - 1,
            click: ResolvedClick {
                synctex: crate::synctex::InversePoint {
                    page: forward.page,
                    x: forward.h + forward.width / 2.,
                    y_from_top: forward.v - forward.height / 2.,
                    page_height_pt: 792.,
                },
                typst: crate::synctex::InversePoint {
                    page: forward.page,
                    x: forward.h + forward.width / 2.,
                    y_from_top: forward.v - forward.height / 2.,
                    page_height_pt: 792.,
                },
                text: Err("unsupported glyph encoding".into()),
            },
            word_precision: true,
            radius: 4,
            editor: Editor::None,
            inverse_search: None,
            source_snapshot: None,
            best_guess: false,
            operation: Operation::default(),
        };
        let resolution = task.resolve().unwrap();
        assert_eq!(resolution.location.line, 6);
        assert!(!resolution.location.precise);
        assert!(
            resolution
                .warning
                .unwrap()
                .contains("PDF text refinement unavailable")
        );
        fs::write(&source, "\\relax\n").unwrap();
        assert!(matches!(task.resolve(), Err(error) if error.to_string().contains("out of range")));
        task.best_guess = true;
        let approximate = task.resolve().unwrap();
        assert_eq!(approximate.location.line, 6);
        assert!(!approximate.location.precise);
        assert!(
            approximate
                .warning
                .unwrap()
                .contains("best-guess navigation")
        );

        let cache = root.join("compiled-source");
        fs::create_dir(&cache).unwrap();
        fs::set_permissions(&cache, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(cache.join("1.txt"), contents).unwrap();
        let manifest = cache.join("manifest.json");
        use std::fmt::Write as _;
        let mut digest = String::with_capacity(64);
        for byte in sha2::Sha256::digest(contents.as_bytes()) {
            write!(&mut digest, "{byte:02x}").unwrap();
        }
        fs::write(
            &manifest,
            serde_json::to_vec(&serde_json::json!({
                "pdf": task.path,
                "revision": task.revision,
                "cwd": root,
                "main": source,
                "files": std::collections::BTreeMap::from([(source.clone(), "1.txt")]),
                "digests": std::collections::BTreeMap::from([(source.clone(), digest)]),
                "warnings": {},
            }))
            .unwrap(),
        )
        .unwrap();
        task.source_snapshot = Some(manifest);
        task.best_guess = false;
        let captured = task.resolve().unwrap();
        assert_eq!(captured.location.line, 6);
        assert!(!captured.warning.unwrap().contains("best-guess navigation"));
        // Explicit opt-in must not downgrade required captured-text failures.
        task.best_guess = true;
        fs::write(cache.join("1.txt"), "changed cached source\n").unwrap();
        assert!(
            matches!(task.resolve(), Err(error) if error.to_string().contains("cached source changed"))
        );
        // A changed companion must reject before any source refinement/delivery.
        fs::remove_file(task.path.with_extension("synctex.gz")).unwrap();
        assert!(matches!(task.resolve(), Err(error) if error.to_string().contains("revision")));
        task.operation.cancel();
        assert!(matches!(task.resolve(), Err(error) if error.kind() == io::ErrorKind::Interrupted));
    }
}
