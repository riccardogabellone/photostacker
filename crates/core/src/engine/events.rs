//! What the engine tells its owner (spec §4.3): every change it makes, as an
//! [`EngineEvent`] on an unbounded channel, followed by a call of the wake
//! callback once the sender holds none of the engine's locks.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use crossbeam_channel::{Receiver, Sender};

use super::{JobId, Wake};
use crate::analyze::Progress;
use crate::model::JobStatus;

/// One thing that happened in the engine.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    /// A new job was submitted; it is [`JobStatus::Queued`].
    JobQueued {
        job: JobId,
    },
    JobStatusChanged {
        job: JobId,
        status: JobStatus,
    },
    /// A step of the job's running task advanced.
    JobProgress {
        job: JobId,
        progress: Progress,
    },
    /// A file of the job's running scan ends its analysis with an error,
    /// `message`, which its [`super::FileView::error`] shows once the scan is
    /// saved. Sent as soon as that is known, while the scan runs, once per
    /// file and scan ([`crate::AnalyzeHooks::file_failed`]); a scan resumed
    /// later reports again the files it examines again.
    FileFailed {
        job: JobId,
        path: PathBuf,
        message: String,
    },
    /// The job's files changed (a swap of a pair's active file): read them
    /// again. Its [`EngineEvent::GroupsChanged`] follows.
    FilesChanged {
        job: JobId,
    },
    /// The job's groups changed: read them again.
    GroupsChanged {
        job: JobId,
    },
    Warning {
        job: Option<JobId>,
        message: String,
    },
    Error {
        job: Option<JobId>,
        message: String,
    },
    /// A download of the model ([`super::Engine::download_model`]) wrote
    /// `done` bytes of its `total`. Sent when the whole percent
    /// `done * 100 / total` passes the last one sent, so at most 101 times
    /// per download; the last has `done == total`.
    ModelDownloadProgress {
        model: String,
        done: u64,
        total: u64,
    },
    /// The model's [`super::ModelView`] changed, whether it is installed or
    /// its activity: read [`super::Engine::models`] again. Also sent after
    /// each removal.
    ModelChanged {
        model: String,
    },
    /// A download, an import or a removal of the model failed, and
    /// `message` says why, without the model's id in front: `cannot
    /// download: <error>` (or `cannot import`, `cannot remove`). A cancel is
    /// no failure. The [`EngineEvent::ModelChanged`] of its end comes first.
    ModelFailed {
        model: String,
        message: String,
    },
    /// The job and everything the engine kept for it are gone.
    JobDeleted {
        job: JobId,
    },
    /// The worker thread ended by a panic outside a task's guard (a bug in
    /// the engine itself), with the panic's message. Sent once; from then on
    /// [`super::Engine::is_running`] is false and the commands that queue
    /// work return [`super::EngineError::Stopped`].
    EngineStopped {
        message: String,
    },
}

/// The sending side: the channel and the wake callback.
pub(super) struct Events {
    sender: Sender<EngineEvent>,
    /// Kept so that a send never fails, and handed out by [`Events::receiver`].
    receiver: Receiver<EngineEvent>,
    wake: Wake,
    /// The events posted whose wake is still due.
    unwoken: AtomicUsize,
}

impl Events {
    pub(super) fn new(wake: Wake) -> Self {
        let (sender, receiver) = crossbeam_channel::unbounded();
        Self {
            sender,
            receiver,
            wake,
            unwoken: AtomicUsize::new(0),
        }
    }

    pub(super) fn receiver(&self) -> Receiver<EngineEvent> {
        self.receiver.clone()
    }

    /// Sends `event` and leaves its wake due, for a sender that holds one of
    /// the engine's locks: the events then go out in the order of the
    /// changes. Once the sender lets go of its locks it calls
    /// [`Events::flush`], or holds a [`FlushOnDrop`] taken before them.
    pub(super) fn post(&self, event: EngineEvent) {
        // The channel stays open while `self.receiver` lives.
        let _ = self.sender.send(event);
        self.unwoken.fetch_add(1, Ordering::SeqCst);
    }

    /// Calls the wake callback once for each event posted whose wake is due
    /// (see [`Wake`]). Called with none of the engine's locks held.
    pub(super) fn flush(&self) {
        for _ in 0..self.unwoken.swap(0, Ordering::SeqCst) {
            (self.wake)();
        }
    }

    /// Sends `event`, then calls the wake callback; for a sender that holds
    /// none of the engine's locks.
    pub(super) fn send(&self, event: EngineEvent) {
        self.post(event);
        self.flush();
    }

    /// A guard that calls [`Events::flush`] when dropped. Taken before the
    /// locks of a command, it is dropped after them.
    pub(super) fn flush_on_drop(&self) -> FlushOnDrop<'_> {
        FlushOnDrop(self)
    }
}

/// See [`Events::flush_on_drop`].
pub(super) struct FlushOnDrop<'a>(&'a Events);

impl Drop for FlushOnDrop<'_> {
    fn drop(&mut self) {
        self.0.flush();
    }
}
