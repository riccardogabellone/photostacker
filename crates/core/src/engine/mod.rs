//! The engine (spec §4.3, §5.7): the jobs, their queue and their worker
//! thread, behind synchronous commands, queries and an event stream.
//!
//! - **One worker thread** runs one task at a time, in FIFO order. A scan
//!   runs [`crate::analyze_with`], whose per-file work has its own pool, with
//!   the store as its exam cache, so each file's examination is a
//!   checkpoint.
//! - **Stage 3** (`stage3.rs`): a scan computes the embeddings that its
//!   ambiguous groups need before it groups the job; a review edit whose
//!   regrouping leaves groups waiting for embeddings (`Pending`) queues an
//!   embed task, which computes them and regroups.
//! - **Commands** return at once. They change the store and the queue, or
//!   set the running task's stop flag:
//!   - the job's life: [`Engine::submit`], [`Engine::cancel`],
//!     [`Engine::delete`], [`Engine::retry`] and [`Engine::write`];
//!   - the review of a job in Review Pending: the decisions
//!     ([`Engine::confirm`] and its kin), the live regrouping
//!     ([`Engine::set_thresholds`]), the manual edits ([`Engine::move_file`],
//!     [`Engine::merge`], [`Engine::split`]) and [`Engine::swap_active`],
//!     whose examination of the file runs on the worker.
//! - **Queries** read through a read-only connection of their own, which WAL
//!   lets run beside the writer; [`Engine::review`] reads a review screen's
//!   job, groups and tray in one snapshot.
//! - **Preferences** ([`Engine::pref`], [`Engine::set_pref`]) are kept in
//!   the same database, so the owner opens no store of its own.
//! - **The models** of Stage 3 (`models.rs`): [`Engine::models`] lists them,
//!   and [`Engine::download_model`], [`Engine::import_model`] and
//!   [`Engine::remove_model`] install and remove them, each download or
//!   import on a thread of its own; a model installed is used at once.
//! - **Events** ([`EngineEvent`]) report every change, each followed by a
//!   call of the wake callback.
//!
//! Every write goes through one writer connection behind a mutex: the
//! commands and the tasks' checkpoints each hold it briefly. The commands
//! and the worker's settling of a finished task change a job's status under
//! the queue's lock, so a command sees the state the worker left and their
//! events come in the order of the changes. The status a task sets as it
//! starts (Scanning or Writing) is set by the running task itself: its job
//! is Queued, which no review command acts on, and a cancel or a delete of
//! it only flags the task. Events sent under a lock are woken for once the
//! sender has let go of it, so the wake callback runs under no engine lock
//! but one: a model thread wakes while it holds the read side of the models'
//! `open` lock (`models.rs`), whose write side the engine's drop takes. So
//! an owner must not drop the engine while it holds a lock its wake takes.
//!
//! [`Engine::shutdown`] (and dropping the engine) sets the quit flag and
//! joins the worker: the running task stops at its next check and leaves
//! its job's status as it was, so a job that was scanning or writing is
//! resumed by the next start, as a job a crash left is (`resume.rs`). Before
//! that it cancels every model download and shuts the model threads out of
//! the engine, without waiting for their downloads or imports (`models.rs`).
//!
//! **One engine per database:** [`Engine::start`] takes the lock of
//! `<db>.lock` before it opens the store, and the engine keeps it until it
//! is dropped, so two apps never resume and write the same jobs. A second
//! start is refused as [`EngineError::Busy`].
//!
//! **A worker that dies is reported:** a panic in a task fails only its job,
//! but one outside the task's guard (a bug in the worker's own loop) ends
//! the worker. Its exit guard then marks the queue stopped and sends
//! [`EngineEvent::EngineStopped`], so the owner never waits for it forever:
//! the commands that would queue work return [`EngineError::Stopped`], and
//! the others and the queries go on without it.

mod events;
mod models;
mod query;
mod resume;
mod review;
mod scan;
mod stage3;
mod worker;
mod writing;

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};

use crossbeam_channel::Receiver;
use serde::Serialize;

pub use events::EngineEvent;
pub use models::{ModelActivity, ModelInstall, ModelView};
pub use query::{FileView, GroupView, JobView, ReviewView};
pub use review::MoveTarget;
pub use stage3::EmbedSource;

use crate::lock::{FileLock, try_lock};
use crate::model::{ConfigError, GroupStatus, JobConfig, JobStatus, Warning};
pub use crate::store::{GroupId, JobCounts, JobId};
use crate::store::{Store, StoreError};
use crate::thumbs::job_thumbs_dir;
use events::Events;
use models::Models;
use stage3::Stage3;
use worker::{Queue, Task};

/// The name of the engine's worker thread.
pub const ENGINE_THREAD: &str = "photostacker-engine";

/// The callback the engine calls once after each event it sends, such as the
/// UI's `request_repaint`. It is called from any of the engine's threads,
/// under none of the engine's locks except, on a model's thread, the read
/// side of the models' `open` lock, which the engine's drop waits for: an
/// owner must not drop the engine while it holds a lock the callback takes.
/// It must return quickly.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// What the engine works with.
pub struct EngineOptions {
    /// The database; created and migrated if needed.
    pub db: PathBuf,
    /// The review thumbnails, one folder per job: `<thumbs_dir>/<job id>/`.
    pub thumbs_dir: PathBuf,
    /// The installed models, which Stage 3 loads (with the `ml` feature).
    pub models_dir: PathBuf,
    /// Where Stage 3's embeddings come from instead of the installed models,
    /// for every model: for tests. `None` is the installed models.
    pub embedder: Option<Arc<dyn EmbedSource>>,
    /// What downloads and imports the models instead of `models::install`:
    /// for tests. `None` is `models::install` with the `ml` feature, and
    /// nothing without it, which refuses both commands.
    pub installer: Option<Arc<dyn ModelInstall>>,
    pub wake: Wake,
}

/// Why a command or a query failed.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// A job's config goes into the store as JSON, which holds only UTF-8.
    #[error("a job's folder must be valid UTF-8: {}", .0.display())]
    NotUtf8(PathBuf),
    #[error("no job {0}")]
    UnknownJob(JobId),
    #[error("no group {0} in this job")]
    UnknownGroup(GroupId),
    #[error("job {job} has no file {idx}")]
    UnknownFile { job: JobId, idx: usize },
    #[error("job {job} is {status:?}, which does not allow this")]
    WrongState { job: JobId, status: JobStatus },
    /// A command that the state of its job or its model does not allow,
    /// such as a review edit that the job's groups and files do not allow,
    /// or the download of a model that is busy.
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Store(StoreError),
    /// A thread of the engine, its worker or a model's, could not be
    /// started.
    #[error("cannot start a worker thread: {0}")]
    Spawn(io::Error),
    /// Another engine, of this process or another, holds the database's
    /// lock (spec §7.2).
    #[error("another Photostacker is using {}", .0.display())]
    Busy(PathBuf),
    /// The database's lock file cannot be opened or locked.
    #[error("cannot lock the database {}: {error}", .db.display())]
    Lock { db: PathBuf, error: io::Error },
    /// The worker thread has stopped ([`EngineEvent::EngineStopped`]), so
    /// nothing queued would run.
    #[error("the engine has stopped")]
    Stopped,
    /// The folder of the model could not be removed
    /// ([`Engine::remove_model`]).
    #[error("cannot remove {model}: {error}")]
    Remove {
        model: &'static str,
        error: io::Error,
    },
}

impl From<StoreError> for EngineError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::UnknownJob(job) => EngineError::UnknownJob(job),
            StoreError::UnknownGroup(group) => EngineError::UnknownGroup(group),
            StoreError::UnknownFile { job, idx } => EngineError::UnknownFile { job, idx },
            other => EngineError::Store(other),
        }
    }
}

/// The handle of a running engine. Dropping it shuts the engine down.
pub struct Engine {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    /// The state of the model commands, which their threads share; they
    /// reach `shared` only through it, until the drop closes it.
    models: Arc<Models>,
    /// The database's lock. Declared last, it is released last: once the
    /// worker is joined and the store's connections are closed.
    _db_lock: FileLock,
}

/// What the handle and the worker share.
struct Shared {
    /// The one connection that writes.
    writer: Mutex<Store>,
    /// The queries' read-only connection.
    reader: Mutex<Store>,
    queue: Mutex<Queue>,
    /// Signalled when a task is queued or the engine quits.
    work: Condvar,
    events: Events,
    thumbs_dir: PathBuf,
    stage3: Stage3,
}

/// The lock file of the database `db`, beside it: its path with `.lock`
/// appended (`photostacker.db` gives `photostacker.db.lock`).
fn lock_file(db: &Path) -> PathBuf {
    let mut name = db.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// Locks `mutex`, also when a thread panicked while holding it: every
/// holder leaves the data consistent between its statements (the store's
/// own writes are transactions), so the engine goes on.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Engine {
    /// Locks the database at `options.db` ([`lock_file`]), opens its store
    /// (migrating it), queues again the jobs the last run left queued or
    /// running (spec §7.3), and starts the worker. A database another engine
    /// holds, in this process or another, is refused as
    /// [`EngineError::Busy`] before it is opened.
    pub fn start(options: EngineOptions) -> Result<Engine, EngineError> {
        let EngineOptions {
            db,
            thumbs_dir,
            models_dir,
            embedder,
            installer,
            wake,
        } = options;
        let db_lock = match try_lock(&lock_file(&db)) {
            Ok(Some(held)) => held,
            Ok(None) => return Err(EngineError::Busy(db)),
            Err(error) => return Err(EngineError::Lock { db, error }),
        };
        let writer = Store::open(&db)?;
        let reader = Store::open_read_only(&db)?;
        let shared = Arc::new(Shared {
            writer: Mutex::new(writer),
            reader: Mutex::new(reader),
            queue: Mutex::new(Queue::default()),
            work: Condvar::new(),
            events: Events::new(wake),
            thumbs_dir,
            stage3: Stage3::new(&models_dir, embedder),
        });
        let models = Arc::new(Models::new(models_dir, installer, Arc::downgrade(&shared)));
        resume::resume(&shared)?;
        let worker = thread::Builder::new()
            .name(ENGINE_THREAD.into())
            .spawn({
                let shared = Arc::clone(&shared);
                move || worker::run(&shared)
            })
            .map_err(EngineError::Spawn)?;
        Ok(Engine {
            shared,
            worker: Some(worker),
            models,
            _db_lock: db_lock,
        })
    }

    /// The event stream. Every receiver handed out shares one channel, so
    /// each event goes to one of them: the owner keeps one.
    pub fn events(&self) -> Receiver<EngineEvent> {
        self.shared.events.receiver()
    }

    /// Validates `config`, stores it as a new [`JobStatus::Queued`] job and
    /// queues its scan. Refused once the worker has stopped.
    pub fn submit(&self, config: JobConfig) -> Result<JobId, EngineError> {
        config.validate()?;
        for dir in [&config.input_dir, &config.output_dir] {
            if dir.to_str().is_none() {
                return Err(EngineError::NotUtf8(dir.clone()));
            }
        }
        let _flush = self.shared.events.flush_on_drop();
        let mut queue = lock(&self.shared.queue);
        queue.check_running()?;
        let job = lock(&self.shared.writer).insert_job(&config)?;
        self.shared.events.post(EngineEvent::JobQueued { job });
        queue.push(Task::Scan(job));
        self.shared.work.notify_one();
        Ok(job)
    }

    /// Cancels `job`. Its running scan or write stops at its next check, and
    /// the job becomes [`JobStatus::Cancelled`]; a queued job becomes it at
    /// once. Any other job is left as it is, and so is the swap of a job in
    /// review ([`Engine::swap_active`]).
    pub fn cancel(&self, job: JobId) -> Result<(), EngineError> {
        let _flush = self.shared.events.flush_on_drop();
        let mut queue = lock(&self.shared.queue);
        if let Some(running) = queue.running(job)
            && running.task.owns_status()
        {
            running.cancelled = true;
            running.stop.store(true, Ordering::SeqCst);
            return Ok(());
        }
        if queue.cancel_queued(job) {
            self.shared.set_status(job, JobStatus::Cancelled);
            return Ok(());
        }
        match lock(&self.shared.writer).job(job)? {
            Some(_) => Ok(()),
            None => Err(EngineError::UnknownJob(job)),
        }
    }

    /// Deletes `job` in any state, cancelling it first: its rows, its
    /// embeddings, its thumbnails and the unfinished files its writes
    /// recorded, never an input or an output. A running job is deleted by the
    /// worker when its task has stopped; any other at once. Either way
    /// [`EngineEvent::JobDeleted`] follows.
    ///
    /// When the store fails to delete a job that is not running, the error
    /// is returned, and a job the next start would scan or write again (such
    /// as a queued one, whose task is gone) is made Cancelled, best effort.
    pub fn delete(&self, job: JobId) -> Result<(), EngineError> {
        let _flush = self.shared.events.flush_on_drop();
        let mut queue = lock(&self.shared.queue);
        // Its queued tasks go too: they would find its rows gone.
        queue.remove(job);
        if let Some(running) = queue.running(job) {
            running.deleted = true;
            running.stop.store(true, Ordering::SeqCst);
            return Ok(());
        }
        let deleted = self.shared.delete_now(job);
        if deleted.is_err() {
            self.shared.cancel_after_failed_delete(job);
        }
        deleted
    }

    /// Queues a [`JobStatus::Failed`], [`JobStatus::Cancelled`] or
    /// [`JobStatus::Completed`] job again, as Queued with its error cleared:
    /// its scan when its analysis was not saved, which goes on from its
    /// checkpoints (the files already examined are not examined again);
    /// otherwise its write.
    ///
    /// Before that write, the job's stranded groups ([`is_stranded`]) go back
    /// to Confirmed, their error and warnings cleared, in the transaction
    /// that queues the job, and [`EngineEvent::GroupsChanged`] follows the
    /// status change when there were any. The write then writes them again, with the other confirmed
    /// groups that have no stack yet. A Completed job with no stranded group
    /// is refused as [`EngineError::Invalid`]: its write would write nothing.
    /// Refused once the worker has stopped.
    pub fn retry(&self, job: JobId) -> Result<(), EngineError> {
        let _flush = self.shared.events.flush_on_drop();
        let mut queue = lock(&self.shared.queue);
        queue.check_running()?;
        let (scan_done, requeued) = {
            let mut writer = lock(&self.shared.writer);
            let stored = writer.job(job)?.ok_or(EngineError::UnknownJob(job))?;
            if !matches!(
                stored.status,
                JobStatus::Failed | JobStatus::Cancelled | JobStatus::Completed
            ) {
                return Err(EngineError::WrongState {
                    job,
                    status: stored.status,
                });
            }
            let stranded: Vec<GroupId> = if stored.scan_done {
                writer
                    .groups(job)?
                    .iter()
                    .filter(|group| is_stranded(group.status, &group.warnings))
                    .map(|group| group.id)
                    .collect()
            } else {
                Vec::new()
            };
            if stored.status == JobStatus::Completed && stranded.is_empty() {
                return Err(EngineError::Invalid(
                    "no stack of this job can be written again".into(),
                ));
            }
            // One transaction: a failure leaves the job as it was, and once
            // it commits nothing below can fail before the task is queued.
            writer.requeue_job(job, &stranded)?;
            (stored.scan_done, !stranded.is_empty())
        };
        self.shared.events.post(EngineEvent::JobStatusChanged {
            job,
            status: JobStatus::Queued,
        });
        if requeued {
            self.shared.events.post(EngineEvent::GroupsChanged { job });
        }
        queue.push(if scan_done {
            Task::Write(job)
        } else {
            Task::Scan(job)
        });
        self.shared.work.notify_one();
        Ok(())
    }

    /// Sets the app's preference `key` (theme, defaults, last folders) to
    /// `value`, replacing what it was, in the database beside the jobs, so it
    /// survives a restart; [`Engine::pref`] reads it. Sends no event.
    pub fn set_pref<T: Serialize + ?Sized>(&self, key: &str, value: &T) -> Result<(), EngineError> {
        Ok(lock(&self.shared.writer).set_pref(key, value)?)
    }

    /// Whether the worker runs: false once it has stopped
    /// ([`EngineEvent::EngineStopped`]).
    pub fn is_running(&self) -> bool {
        !lock(&self.shared.queue).is_stopped()
    }

    /// Stops the engine: the running task stops at its next check without
    /// changing its job's status, and the worker is joined. Queued jobs stay
    /// queued. Every model download is cancelled, and neither it nor an
    /// import is waited for: neither sends anything from then on. Dropping
    /// the engine does the same.
    pub fn shutdown(self) {
        drop(self);
    }

    fn stop_worker(&mut self) {
        let Some(worker) = self.worker.take() else {
            return;
        };
        lock(&self.shared.queue).quit();
        self.shared.work.notify_all();
        if worker.join().is_err() {
            tracing::error!("the engine thread panicked");
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // First, so that once the worker is joined no thread holds `shared`
        // but this handle, which then drops it before the database's lock.
        self.models.close();
        self.stop_worker();
    }
}

impl Shared {
    /// The folder of `job`'s review thumbnails.
    fn job_thumbs(&self, job: JobId) -> PathBuf {
        job_thumbs_dir(&self.thumbs_dir, job)
    }

    // The helpers below post their events, since their callers may hold the
    // queue's lock: each caller flushes once it lets go of its locks.

    /// Sets `job`'s status and reports it; a store error is reported instead.
    fn set_status(&self, job: JobId, status: JobStatus) {
        if let Err(error) = self.try_set_status(job, status) {
            self.report_error(job, &error);
        }
    }

    /// Sets `job`'s status and reports it, or returns the store's error.
    fn try_set_status(&self, job: JobId, status: JobStatus) -> Result<(), StoreError> {
        lock(&self.writer).set_job_status(job, status)?;
        self.events
            .post(EngineEvent::JobStatusChanged { job, status });
        Ok(())
    }

    /// Sets `job` Failed with `message` as its error, and reports both.
    fn set_failed(&self, job: JobId, message: String) {
        let stored = lock(&self.writer).set_job_error(job, Some(&message));
        if let Err(error) = stored {
            self.report_error(job, &error);
        }
        self.set_status(job, JobStatus::Failed);
        self.events.post(EngineEvent::Error {
            job: Some(job),
            message,
        });
    }

    fn report_error(&self, job: JobId, error: &dyn std::error::Error) {
        tracing::warn!(job, %error, "the engine could not record a change");
        self.events.post(EngineEvent::Error {
            job: Some(job),
            message: error.to_string(),
        });
    }

    /// Deletes `job`, which is neither running nor queued: first the files
    /// the engine made for it (the unfinished files its writes recorded and
    /// its thumbnails), then its rows, so a crash in between leaves rows
    /// whose files are gone, which a later run tolerates, rather than files
    /// nothing records.
    ///
    /// A recorded file that cannot be removed is logged and left: the job
    /// is deleted all the same.
    fn delete_now(&self, job: JobId) -> Result<(), EngineError> {
        let (output_dir, partials): (PathBuf, Vec<PathBuf>) = {
            let writer = lock(&self.writer);
            let Some(stored) = writer.job(job)? else {
                return Err(EngineError::UnknownJob(job));
            };
            let partials = writer
                .groups(job)?
                .into_iter()
                .filter_map(|group| group.partial_path)
                .collect();
            (stored.config.output_dir, partials)
        };
        for partial in &partials {
            remove_partial(&output_dir, partial);
        }
        remove_dir(&self.job_thumbs(job));
        lock(&self.writer).delete_job(job)?;
        self.events.post(EngineEvent::JobDeleted { job });
        Ok(())
    }

    /// After the store failed to delete `job`, which is neither running nor
    /// queued any more: a job left Queued, Scanning or Writing would be
    /// scanned or written again by the next start (spec §7.3), so it is made
    /// Cancelled, for the user to delete again. A job in any other state,
    /// such as one in review, keeps it. Best effort: a job that is gone is
    /// left, and a store error is reported.
    fn cancel_after_failed_delete(&self, job: JobId) {
        let stored = lock(&self.writer).job(job);
        match stored {
            Ok(Some(stored))
                if matches!(
                    stored.status,
                    JobStatus::Queued | JobStatus::Scanning | JobStatus::Writing
                ) =>
            {
                self.set_status(job, JobStatus::Cancelled);
            }
            Ok(_) => {}
            Err(error) => self.report_error(job, &error),
        }
    }
}

/// Whether a group with `status` and the `warnings` of its last write is
/// stranded: its write ended in a way that another write may mend, once
/// the disk has room again or the drive is back, so [`Engine::retry`]
/// writes it again (spec §5.7). That is a Failed group, or a Skipped one
/// with a photo that could not be read (such as one on a drive that
/// dropped). A stack skipped only because its photos have different
/// resolutions would be skipped again.
///
/// Over a group's status and warnings, which a [`GroupView`] and the
/// store's group both hold, so the owner counts the stacks a retry would
/// write by the engine's own rule.
pub fn is_stranded(status: GroupStatus, warnings: &[(usize, Warning)]) -> bool {
    match status {
        GroupStatus::Failed => true,
        GroupStatus::Skipped => warnings
            .iter()
            .any(|(_, warning)| matches!(warning, Warning::LayerUnreadable { .. })),
        GroupStatus::Proposed
        | GroupStatus::Confirmed
        | GroupStatus::Rejected
        | GroupStatus::Written => false,
    }
}

/// Removes what a write of a job whose output folder is `output_dir` leaves
/// when it does not finish: the partial file `partial` it recorded and the
/// spill files `<partial>.layers` and `<partial>.merged`, each only if it
/// is a regular file. Returns whether nothing of it is left to remove: false
/// when one of them exists and could not be removed, so its record is kept
/// for the next try.
///
/// A recorded path that a write never makes, one whose file name does not
/// end in `.partial` or that is not inside `output_dir`, is refused with a
/// warning and nothing is removed: it names no file of ours, so nothing is
/// left to remove either.
fn remove_partial(output_dir: &Path, partial: &Path) -> bool {
    if !is_a_writes_partial(output_dir, partial) {
        tracing::warn!(
            partial = %partial.display(),
            output = %output_dir.display(),
            "a recorded partial that no write makes is left alone"
        );
        return true;
    }
    let mut gone = true;
    for suffix in ["", ".layers", ".merged"] {
        let mut name = partial.as_os_str().to_owned();
        name.push(suffix);
        let path = PathBuf::from(name);
        let removed = match fs::symlink_metadata(&path) {
            // A link or a folder is no write's.
            Ok(meta) if !meta.is_file() => Ok(()),
            Ok(_) => fs::remove_file(&path),
            Err(error) => Err(error),
        };
        match removed {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "cannot remove an unfinished stack");
                gone = false;
            }
        }
    }
    gone
}

/// Whether `partial` can be a partial that a write into `output_dir`
/// created: a file name ending in `.partial`, inside `output_dir`, with no
/// `..` component. The writer records it under the output folder's
/// canonical path, which the job's config may spell otherwise (through a
/// link), so either spelling is inside; the comparison is by components.
fn is_a_writes_partial(output_dir: &Path, partial: &Path) -> bool {
    let named = partial
        .file_name()
        .is_some_and(|name| name.as_encoded_bytes().ends_with(b".partial"));
    let climbs = partial
        .components()
        .any(|component| component == Component::ParentDir);
    let inside = |dir: &Path| {
        partial
            .strip_prefix(dir)
            .is_ok_and(|rest| !rest.as_os_str().is_empty())
    };
    named
        && !climbs
        && (inside(output_dir) || output_dir.canonicalize().is_ok_and(|dir| inside(&dir)))
}

/// `error` with its causes, as `error: cause: cause`, the way a job's error
/// is stored, so that a failure keeps its operating system's reason (such
/// as "Permission denied"). A cause whose text the message already shows,
/// as an error that prints its source does, is not repeated.
pub(super) fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = cause.source();
    }
    message
}

/// Removes the folder `dir` and everything in it, if it exists.
fn remove_dir(dir: &Path) {
    match fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(dir = %dir.display(), %error, "cannot remove a job's thumbnails");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    enum Outer {
        #[error("cannot read /x")]
        Hidden(#[source] io::Error),
        #[error("cannot write: {0}")]
        Shown(#[from] io::Error),
    }

    #[test]
    fn error_chain_keeps_each_cause_once() {
        let denied = || io::Error::new(io::ErrorKind::PermissionDenied, "Permission denied");
        assert_eq!(
            error_chain(&Outer::Hidden(denied())),
            "cannot read /x: Permission denied"
        );
        assert_eq!(
            error_chain(&Outer::Shown(denied())),
            "cannot write: Permission denied"
        );
        assert_eq!(error_chain(&denied()), "Permission denied");
    }

    #[test]
    fn a_stack_with_an_unreadable_photo_is_stranded() {
        let unreadable = || Warning::LayerUnreadable {
            reason: "gone".into(),
        };
        let mismatch = || Warning::ResolutionMismatch {
            width: 320,
            height: 213,
            canvas_width: 640,
            canvas_height: 426,
        };
        // One unreadable photo is enough, whatever else the write found.
        assert!(is_stranded(
            GroupStatus::Skipped,
            &[(1, mismatch()), (2, unreadable())]
        ));
        assert!(is_stranded(GroupStatus::Failed, &[]));
        // Skipped for its resolutions alone, or for being one photo, it would be
        // skipped again.
        assert!(!is_stranded(GroupStatus::Skipped, &[(1, mismatch())]));
        assert!(!is_stranded(GroupStatus::Skipped, &[]));
        for status in [
            GroupStatus::Proposed,
            GroupStatus::Confirmed,
            GroupStatus::Rejected,
            GroupStatus::Written,
        ] {
            assert!(!is_stranded(status, &[(2, unreadable())]));
        }
    }

    #[test]
    fn engine_is_shared_across_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Engine>();
    }

    #[test]
    fn remove_partial_removes_the_partial_and_its_spill_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let partial = dir.path().join("a_3.psd.partial");
        let names = [
            "a_3.psd.partial",
            "a_3.psd.partial.layers",
            "a_3.psd.partial.merged",
            "a_3.psd",
            "b_2.psd.partial",
        ];
        for name in names {
            fs::write(dir.path().join(name), name).unwrap();
        }
        assert!(remove_partial(dir.path(), &partial));
        let mut left: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["a_3.psd", "b_2.psd.partial"]);
        // Nothing left to remove is no error.
        assert!(remove_partial(dir.path(), &partial));
    }

    #[test]
    fn remove_partial_refuses_what_no_write_records() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("stacked");
        fs::create_dir(&output).unwrap();
        let photo = output.join("IMG_1.jpg");
        let outside = root.path().join("a_3.psd.partial");
        let climbing = output.join("..").join("b_3.psd.partial");
        let named_as_the_folder = output.join("");
        for file in [&photo, &outside, &root.path().join("b_3.psd.partial")] {
            fs::write(file, b"keep").unwrap();
            let mut layers = file.as_os_str().to_owned();
            layers.push(".layers");
            fs::write(PathBuf::from(layers), b"keep").unwrap();
        }
        for recorded in [&photo, &outside, &climbing, &named_as_the_folder] {
            assert!(remove_partial(&output, recorded), "{recorded:?}");
        }
        let mut left: Vec<String> = fs::read_dir(root.path())
            .unwrap()
            .chain(fs::read_dir(&output).unwrap())
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "IMG_1.jpg",
                "IMG_1.jpg.layers",
                "a_3.psd.partial",
                "a_3.psd.partial.layers",
                "b_3.psd.partial",
                "b_3.psd.partial.layers",
                "stacked"
            ]
        );
    }

    #[test]
    fn a_writes_partial_is_named_so_inside_the_output_folder() {
        let output = Path::new("/out/stacked");
        assert!(is_a_writes_partial(
            output,
            Path::new("/out/stacked/a_2.psd.partial")
        ));
        assert!(is_a_writes_partial(
            output,
            Path::new("/out/stacked/sub/a_2.tif.partial")
        ));
        assert!(!is_a_writes_partial(
            output,
            Path::new("/out/stacked/IMG_1.jpg")
        ));
        assert!(!is_a_writes_partial(
            output,
            Path::new("/out/a_2.psd.partial")
        ));
        assert!(!is_a_writes_partial(
            output,
            Path::new("/out/stacked2/a_2.psd.partial")
        ));
        assert!(!is_a_writes_partial(
            output,
            Path::new("/out/stacked/../a_2.psd.partial")
        ));
        assert!(!is_a_writes_partial(
            output,
            Path::new("/out/stacked.partial")
        ));
        assert!(!is_a_writes_partial(
            output,
            Path::new("stacked/a_2.psd.partial")
        ));
    }

    #[test]
    fn remove_partial_leaves_folders_and_links() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("photo.jpg");
        fs::write(&target, b"photo").unwrap();
        let partial = dir.path().join("a_3.psd.partial");
        fs::create_dir(&partial).unwrap();
        let link = dir.path().join("a_3.psd.partial.layers");
        if !photostacker_testkit::symlink_file(&target, &link) {
            return;
        }
        assert!(remove_partial(dir.path(), &partial));
        assert!(partial.is_dir());
        assert!(link.is_symlink());
        assert_eq!(fs::read(&target).unwrap(), b"photo");
    }
}
