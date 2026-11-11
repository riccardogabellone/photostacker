//! The worker thread: it takes the queued tasks one at a time, in FIFO
//! order, runs each with a stop flag, and then settles its job's status.
//!
//! A task's panic is caught by the task's guard and fails only its job. The
//! loop itself runs under the exit guard ([`exit_guard`]): a panic that
//! escapes it ends the worker, and the guard reports it.

use std::collections::VecDeque;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{EngineError, EngineEvent, JobId, Shared, lock, review, scan, stage3, writing};
use crate::guard;
use crate::model::JobStatus;
use crate::store::StoreError;

/// A unit of the worker's work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Task {
    /// Analyse the job's folder, save the analysis and group it.
    Scan(JobId),
    /// Write the job's confirmed groups that have no output yet.
    Write(JobId),
    /// Make the passive `file` the active file of its pair, and regroup.
    Swap { job: JobId, file: usize },
    /// Compute the embeddings the job's regrouping waits for, and regroup
    /// (`stage3.rs`).
    Embed(JobId),
}

impl Task {
    fn job(&self) -> JobId {
        match *self {
            Task::Scan(job) | Task::Write(job) | Task::Swap { job, .. } | Task::Embed(job) => job,
        }
    }

    /// Whether the task carries its job's status: a scan or a write, which
    /// a cancel stops and whose end sets the status. A swap or an embed is a
    /// review edit that runs on the worker: its job stays as it is.
    pub(super) fn owns_status(&self) -> bool {
        !matches!(self, Task::Swap { .. } | Task::Embed(_))
    }
}

/// The tasks waiting, the one running, whether the engine quits, and
/// whether the worker has stopped.
#[derive(Default)]
pub(super) struct Queue {
    tasks: VecDeque<Task>,
    running: Option<Running>,
    quit: bool,
    /// The worker ended by a panic ([`exit_guard`]): nothing queued runs.
    stopped: bool,
}

/// The task the worker is running, and what the commands asked of it.
pub(super) struct Running {
    pub(super) task: Task,
    /// What the task checks between files: set by a cancel, a delete or the
    /// quit.
    pub(super) stop: Arc<AtomicBool>,
    /// The user cancelled the job: it becomes Cancelled.
    pub(super) cancelled: bool,
    /// The user deleted the job: the worker deletes it once the task stops.
    pub(super) deleted: bool,
}

impl Queue {
    pub(super) fn push(&mut self, task: Task) {
        self.tasks.push_back(task);
    }

    /// Queues the embed task of `job`, unless one is queued already: that
    /// one computes whatever the job's regrouping waits for when it runs.
    pub(super) fn push_embed(&mut self, job: JobId) {
        let task = Task::Embed(job);
        if !self.tasks.contains(&task) {
            self.tasks.push_back(task);
        }
    }

    /// The running task, when it is one of `job`'s.
    pub(super) fn running(&mut self, job: JobId) -> Option<&mut Running> {
        self.running
            .as_mut()
            .filter(|running| running.task.job() == job)
    }

    /// Drops the queued tasks of `job`; returns whether there were any.
    pub(super) fn remove(&mut self, job: JobId) -> bool {
        let before = self.tasks.len();
        self.tasks.retain(|task| task.job() != job);
        self.tasks.len() != before
    }

    /// Drops the queued tasks of `job` when one of them is its scan or its
    /// write, which a cancel cancels; returns whether it did. Swaps alone
    /// stay queued: they edit a job in review, which a cancel leaves as it
    /// is.
    pub(super) fn cancel_queued(&mut self, job: JobId) -> bool {
        let owned = self
            .tasks
            .iter()
            .any(|task| task.job() == job && task.owns_status());
        owned && self.remove(job)
    }

    /// Makes the worker stop: the running task at its next check, and the
    /// worker before it takes another task.
    pub(super) fn quit(&mut self) {
        self.quit = true;
        if let Some(running) = &self.running {
            running.stop.store(true, Ordering::SeqCst);
        }
    }

    /// `Err(Stopped)` once the worker has stopped, for a command that would
    /// queue a task: it would never run.
    pub(super) fn check_running(&self) -> Result<(), EngineError> {
        if self.stopped {
            Err(EngineError::Stopped)
        } else {
            Ok(())
        }
    }

    pub(super) fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// Marks the worker stopped. The task it was running is forgotten, so a
    /// delete of its job deletes it at once instead of leaving it to a
    /// worker that is gone; the queued tasks stay, for a cancel to find.
    fn stop(&mut self) {
        self.stopped = true;
        self.running = None;
    }
}

/// What a task that finished leaves its job in.
#[derive(Debug)]
pub(super) enum Settle {
    /// This status.
    Status(JobStatus),
    /// Review Pending, after a scan whose Stage 3 could not use the model
    /// the job names: once the status is stored, the job is regrouped if
    /// its model is available now ([`Shared::regroup_if_its_model_came`]).
    ReviewWithoutItsModel,
    /// Queued again, for this task: an `auto_confirm` scan's write.
    Then(Task),
    /// The status it has: a swap's.
    Unchanged,
}

/// Why a task did not finish.
#[derive(Debug)]
pub(super) enum TaskError {
    /// Its stop flag was set: a cancel, a delete or the quit.
    Stopped,
    /// It failed; the job's error says why.
    Failed(String),
}

impl From<StoreError> for TaskError {
    fn from(error: StoreError) -> Self {
        TaskError::Failed(error.to_string())
    }
}

impl From<EngineError> for TaskError {
    fn from(error: EngineError) -> Self {
        TaskError::Failed(error.to_string())
    }
}

/// The worker thread's body: its loop, under its exit guard.
pub(super) fn run(shared: &Shared) {
    exit_guard(shared, || work(shared));
}

/// Runs `work`, the worker's loop. Should it end by a panic that no task's
/// guard caught (a bug in the loop itself), the worker is gone: the guard
/// marks the queue stopped, so the commands that queue work are refused,
/// then sends [`EngineEvent::EngineStopped`] with the panic's message, once
/// the queue's lock is let go.
///
/// The panic is caught here, rather than seen from a `Drop` while the
/// thread unwinds, because only the caught payload carries its message. It
/// still reaches the panic hook first, as any panic outside the core's
/// guard does, so its location is printed or logged.
pub(super) fn exit_guard(shared: &Shared, work: impl FnOnce()) {
    // Unwind safety: what the loop leaves behind is read again only under
    // the engine's locks, which tolerate poisoning since every holder keeps
    // the data consistent between its statements.
    let Err(panic) = panic::catch_unwind(AssertUnwindSafe(work)) else {
        return;
    };
    let message = guard::message(&*panic);
    tracing::error!(%message, "the engine thread stopped");
    lock(&shared.queue).stop();
    shared.events.send(EngineEvent::EngineStopped { message });
}

/// The worker's loop, until the engine quits.
fn work(shared: &Shared) {
    while let Some((task, stop)) = next_task(shared) {
        // A panic in a task (a bug) fails its job, not the engine.
        let outcome = guard::catch(|| perform(shared, &task, &stop)).unwrap_or_else(|panic| {
            tracing::error!(job = task.job(), %panic, "the engine task panicked");
            Err(TaskError::Failed(format!("internal error: {panic}")))
        });
        let look_for_its_model = matches!(outcome, Ok(Settle::ReviewWithoutItsModel));
        finish(shared, &task, outcome);
        shared.events.flush();
        if look_for_its_model {
            shared.regroup_if_its_model_came(task.job());
        }
    }
}

/// Waits for the next task and marks it running; `None` once the engine
/// quits.
fn next_task(shared: &Shared) -> Option<(Task, Arc<AtomicBool>)> {
    let mut queue = lock(&shared.queue);
    loop {
        if queue.quit {
            return None;
        }
        if let Some(task) = queue.tasks.pop_front() {
            let stop = Arc::new(AtomicBool::new(false));
            queue.running = Some(Running {
                task: task.clone(),
                stop: Arc::clone(&stop),
                cancelled: false,
                deleted: false,
            });
            return Some((task, stop));
        }
        queue = shared
            .work
            .wait(queue)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}

/// Runs `task`; on success, what its job is left in.
fn perform(shared: &Shared, task: &Task, stop: &AtomicBool) -> Result<Settle, TaskError> {
    match *task {
        Task::Scan(job) => scan::scan(shared, job, stop),
        Task::Write(job) => writing::write(shared, job, stop),
        Task::Swap { job, file } => review::swap(shared, job, file, stop),
        Task::Embed(job) => stage3::embed(shared, job, stop),
    }
}

/// Settles the job of `task` once it returned, under the queue's lock, so no
/// command acts on the job in between:
///
/// - a delete deletes it. When the store fails to, the job is made
///   Cancelled if the next start would scan or write it again: a scan's or
///   a write's job, or one whose write was queued behind a swap or an embed
///   and dropped by the delete. A job in review whose swap or embed ran
///   stays in review;
/// - a task that finished settles it as it says, even when a cancel came
///   once its work was done: it is too late to cancel; but a cancel stops
///   the task that was to follow (an `auto_confirm` scan's write), which
///   makes it Cancelled;
/// - a task that a cancel stopped, or that failed after one, makes it
///   Cancelled;
/// - a task that only the quit stopped leaves it as it is, for the next
///   start to resume;
/// - a failed scan or write makes it Failed with the error; a failed swap or
///   embed only reports its error.
///
/// The events it posts are woken for by the caller, once the lock is gone.
fn finish(shared: &Shared, task: &Task, outcome: Result<Settle, TaskError>) {
    let job = task.job();
    let mut queue = lock(&shared.queue);
    let Some(running) = queue.running.take() else {
        return;
    };
    if running.deleted {
        // A task a command queued for the job after its delete would find
        // its rows gone.
        queue.remove(job);
        if let Err(error) = shared.delete_now(job) {
            shared.cancel_after_failed_delete(job);
            shared.report_error(job, &error);
        }
        return;
    }
    match outcome {
        Ok(Settle::Status(status)) => shared.set_status(job, status),
        Ok(Settle::ReviewWithoutItsModel) => shared.set_status(job, JobStatus::ReviewPending),
        // The task to follow has not started, so the cancel stops the job:
        // an auto_confirm scan's write.
        Ok(Settle::Then(_)) if running.cancelled => shared.set_status(job, JobStatus::Cancelled),
        Ok(Settle::Then(next)) => {
            shared.set_status(job, JobStatus::Queued);
            queue.push(next);
        }
        Ok(Settle::Unchanged) => {}
        Err(_) if running.cancelled => shared.set_status(job, JobStatus::Cancelled),
        Err(TaskError::Stopped) => {}
        Err(TaskError::Failed(message)) if task.owns_status() => {
            tracing::info!(job, error = %message, "job failed");
            shared.set_failed(job, message);
        }
        Err(TaskError::Failed(message)) => {
            tracing::warn!(job, error = %message, "a review task failed");
            shared.events.post(EngineEvent::Error {
                job: Some(job),
                message,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::thread;

    use super::*;
    use crate::engine::{Engine, EngineOptions};
    use crate::model::JobConfig;

    /// The guard runs here on a thread of the test's, beside the engine's
    /// idle worker, which the queue marked stopped never wakes for work.
    #[test]
    fn a_worker_that_dies_reports_it_and_refuses_new_work() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::start(EngineOptions {
            db: dir.path().join("photostacker.db"),
            thumbs_dir: dir.path().join("thumbs"),
            models_dir: dir.path().join("models"),
            embedder: None,
            installer: None,
            wake: Arc::new(|| {}),
        })
        .unwrap();
        let events = engine.events();
        // A loop that returns reports nothing.
        exit_guard(&engine.shared, || {});
        assert!(engine.is_running());
        assert!(events.is_empty());

        let shared = Arc::clone(&engine.shared);
        let died = thread::spawn(move || exit_guard(&shared, || panic!("a bug in the loop")));
        assert!(died.join().is_ok(), "the guard ends the thread normally");

        assert_eq!(
            events.try_iter().collect::<Vec<_>>(),
            [EngineEvent::EngineStopped {
                message: "a bug in the loop".into()
            }]
        );
        assert!(!engine.is_running());
        let input = dir.path().join("photos");
        fs::create_dir(&input).unwrap();
        assert!(matches!(
            engine.submit(JobConfig::new(input)),
            Err(EngineError::Stopped)
        ));
        for refused in [engine.retry(1), engine.write(1), engine.swap_active(1, 0)] {
            assert!(matches!(refused, Err(EngineError::Stopped)), "{refused:?}");
        }
        assert_eq!(EngineError::Stopped.to_string(), "the engine has stopped");
        // The queries still work, and nothing was stored.
        assert_eq!(engine.jobs().unwrap(), []);
        assert!(events.is_empty());
    }
}
