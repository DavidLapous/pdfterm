//! External navigation owns its cancellation/deadline, never the render thread.
use crate::{
    editor::Editor,
    pdf::{DocumentId, ResolvedClick},
    process::Operation,
    synctex::{self, DocumentRevision, InverseResolution},
};
use crossbeam_channel::{Receiver, Sender, bounded};
use std::{
    io,
    path::PathBuf,
    thread::{self, JoinHandle},
};

pub(crate) struct Coordinator {
    // Drop cancels the active request before joining the external worker.
    pub inverse: Option<PendingInverse>,
    pub worker: NavigationWorker,
    pub forward: Option<PendingForward>,
    pub flash: Option<PendingFlash>,
    pub next_request_id: u64,
}
impl Coordinator {
    pub fn new() -> Self {
        Self {
            inverse: None,
            worker: NavigationWorker::new(),
            forward: None,
            flash: None,
            next_request_id: 1,
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForwardStage {
    AwaitingDocument,
    AwaitingFrame,
}
pub(crate) struct PendingForward {
    pub request: synctex::ForwardRequest,
    pub reply: crate::ipc::ForwardReply,
    pub deadline: std::time::Instant,
    pub stage: ForwardStage,
}
pub(crate) struct PendingFlash {
    pub document_id: DocumentId,
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
        let mut result = synctex::resolve_inverse(
            &self.path,
            synctex::InversePoint {
                page: self.page + 1,
                x: self.click.pdf_x,
                y_from_top: self.click.page_height_pt - self.click.pdf_y,
                page_height_pt: self.click.page_height_pt,
            },
            word,
            self.radius,
            &self.operation,
        )?;
        if self.word_precision
            && let Err(error) = &self.click.text
        {
            result.warning = Some(format!(
                "line-only navigation: PDF text refinement unavailable: {error}"
            ));
        }
        self.operation.check()?;
        self.revision.check(&self.path)?;
        self.editor.deliver(&result.location, &self.operation)?;
        Ok(result)
    }
}

pub(crate) struct NavigationWorker {
    requests: Option<Sender<InverseTask>>,
    pub replies: Receiver<InverseReply>,
    thread: Option<JoinHandle<()>>,
}
impl NavigationWorker {
    pub fn new() -> Self {
        let (requests, receiver) = bounded::<InverseTask>(1);
        let (sender, replies) = bounded(1);
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
            replies,
            thread: Some(thread),
        }
    }
    pub fn submit(&self, task: InverseTask) -> io::Result<()> {
        self.requests
            .as_ref()
            .expect("live worker")
            .try_send(task)
            .map_err(|_| io::Error::other("navigation worker busy or stopped; repeat the click"))
    }
}
impl Drop for NavigationWorker {
    fn drop(&mut self) {
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
    use std::{fs, process::Command, time::Duration};

    #[test]
    fn failed_pdf_text_preserves_real_synctex_navigation_and_stale_pairs_fail() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("navigation.tex");
        fs::write(&source, include_str!("../tests/fixtures/navigation.tex")).unwrap();
        let output = crate::process::output(
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
        .expect("pdflatex is required for the navigation fixture");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let path = directory.path().join("navigation.pdf");
        let forward = synctex::resolve_forward(&path, &source, 6, 1).unwrap();
        let task = InverseTask {
            request_id: 1,
            revision: DocumentRevision::read(&path).unwrap(),
            path,
            page: forward.page - 1,
            click: ResolvedClick {
                pdf_x: forward.h + forward.width / 2.,
                pdf_y: -(forward.v - forward.height / 2.),
                page_height_pt: 0.,
                text: Err("unsupported glyph encoding".into()),
            },
            word_precision: true,
            radius: 4,
            editor: Editor::None,
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
        // A changed companion must reject before any source refinement/delivery.
        fs::remove_file(task.path.with_extension("synctex.gz")).unwrap();
        assert!(matches!(task.resolve(), Err(error) if error.to_string().contains("revision")));
        task.operation.cancel();
        assert!(matches!(task.resolve(), Err(error) if error.kind() == io::ErrorKind::Interrupted));
    }
}
