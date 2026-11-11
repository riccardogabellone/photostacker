//! Resuming on start (spec §7.3): before the engine takes a command, each
//! job that the last run left queued, or running when it quit or crashed, is
//! queued again, in the order of the job ids.
//!
//! - A job whose scan is not done (Queued or Scanning) is scanned. The exam
//!   cache makes the scan go on from its checkpoints, each re-checked by its
//!   file's fingerprint.
//! - A job left Scanning with its scan done (the run stopped between saving
//!   the scan and settling it) is not scanned again, since an analysis is
//!   saved once per job: it goes to Review Pending, or, with `auto_confirm`,
//!   its proposed groups are confirmed and its write is queued.
//! - A job whose write was queued (Queued with its scan done) or running
//!   (Writing) is written. The write task first removes the unfinished
//!   stacks that its job's records name (`writing.rs`), then writes the
//!   confirmed groups that have no stack yet.
//! - A job in Review Pending stays as it is, but when some of its groups
//!   wait for embeddings (`Pending`: the quit dropped its embed task), its
//!   embed task is queued (`stage3.rs`).
//! - Every other job (Completed, Failed, Cancelled) stays as it is.
//!
//! A job queued again is Queued, as the job of a queued task always is; its
//! task sets Scanning or Writing when it starts. A job that the store fails
//! to resume becomes Failed, with the error, and the next one is resumed.
//!
//! Only recorded paths are ever removed. A crash between persisting a stack
//! and recording its `output_path` can leave a duplicate `-1` file on the
//! rewrite, never a lost or overwritten one.

use super::worker::Task;
use super::{EngineError, EngineEvent, Shared, lock};
use crate::model::{GroupStatus, JobStatus, MlStatus};
use crate::store::{StoreError, StoredJob};

/// Queues again the jobs the last run left queued or running. Called by
/// [`super::Engine::start`] before the worker starts; it fails only when
/// the jobs cannot be listed.
pub(super) fn resume(shared: &Shared) -> Result<(), EngineError> {
    let _flush = shared.events.flush_on_drop();
    let mut queue = lock(&shared.queue);
    let jobs = lock(&shared.writer).jobs()?;
    for job in &jobs {
        match resume_job(shared, job) {
            Ok(Some(task)) => {
                tracing::info!(job = job.id, status = ?job.status, ?task, "resuming a job");
                queue.push(task);
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(job = job.id, %error, "cannot resume a job");
                shared.set_failed(job.id, format!("cannot resume the job: {error}"));
            }
        }
    }
    Ok(())
}

/// Brings `job` back to where its next task starts; returns that task, if
/// it has one.
fn resume_job(shared: &Shared, job: &StoredJob) -> Result<Option<Task>, StoreError> {
    let task = match (job.status, job.scan_done) {
        (JobStatus::Queued | JobStatus::Scanning, false) => Task::Scan(job.id),
        (JobStatus::Scanning, true) if !job.config.auto_confirm => {
            move_to(shared, job, JobStatus::ReviewPending)?;
            return Ok(None);
        }
        (JobStatus::Scanning, true) => {
            let confirmed = lock(&shared.writer).decide_proposed(job.id, GroupStatus::Confirmed)?;
            if confirmed > 0 {
                shared
                    .events
                    .post(EngineEvent::GroupsChanged { job: job.id });
            }
            Task::Write(job.id)
        }
        (JobStatus::Queued, true) | (JobStatus::Writing, _) => Task::Write(job.id),
        // Its groups that wait for embeddings get them, as when the embed
        // task the quit dropped had run; the job stays in review.
        (JobStatus::ReviewPending, _) => {
            let waits = lock(&shared.writer)
                .groups(job.id)?
                .iter()
                .any(|group| group.ml_status == MlStatus::Pending);
            return Ok(waits.then_some(Task::Embed(job.id)));
        }
        (JobStatus::Completed | JobStatus::Failed | JobStatus::Cancelled, _) => return Ok(None),
    };
    move_to(shared, job, JobStatus::Queued)?;
    Ok(Some(task))
}

/// Gives `job` `status`, when it is not its status already.
fn move_to(shared: &Shared, job: &StoredJob, status: JobStatus) -> Result<(), StoreError> {
    if job.status == status {
        return Ok(());
    }
    shared.try_set_status(job.id, status)
}
