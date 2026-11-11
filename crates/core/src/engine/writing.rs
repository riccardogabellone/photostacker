//! Writing a job's stacks (spec §5.6, §5.7): the write command and the
//! write task, which records each stack's partial and outcome in the store
//! as the writer reports them.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use super::worker::{Settle, Task, TaskError};
use super::{
    Engine, EngineError, EngineEvent, GroupId, JobId, Shared, error_chain, lock, remove_partial,
};
use crate::analyze::{FileRecord, Progress, Stage, Ticker};
use crate::fsio::ensure_output_dir;
use crate::model::{GroupStatus, JobStatus, Warning};
use crate::store::StoreError;
use crate::write::{WriteError, WriteObserver, WriteOptions, WriteOutcome, write_groups};

impl Engine {
    /// Queues the write of `job`, which must be in Review Pending, or
    /// Cancelled once its scan was done. The job becomes Queued; its write
    /// then writes the confirmed groups that have no stack yet. Refused once
    /// the worker has stopped.
    pub fn write(&self, job: JobId) -> Result<(), EngineError> {
        let _flush = self.shared.events.flush_on_drop();
        let mut queue = lock(&self.shared.queue);
        queue.check_running()?;
        {
            let writer = lock(&self.shared.writer);
            let stored = writer.job(job)?.ok_or(EngineError::UnknownJob(job))?;
            let writable = match stored.status {
                JobStatus::ReviewPending => true,
                JobStatus::Cancelled => stored.scan_done,
                _ => false,
            };
            if !writable {
                return Err(EngineError::WrongState {
                    job,
                    status: stored.status,
                });
            }
            writer.set_job_status(job, JobStatus::Queued)?;
        }
        self.shared.events.post(EngineEvent::JobStatusChanged {
            job,
            status: JobStatus::Queued,
        });
        queue.push(Task::Write(job));
        self.shared.work.notify_one();
        Ok(())
    }
}

/// Writes `job`, which must be queued:
///
/// 1. sets it Writing;
/// 2. removes the unfinished stacks that an earlier write of it recorded
///    and did not finish (a crash, or a removal that failed), which fails
///    the job when one of them cannot be removed, or when the output folder
///    cannot be used: then every record is kept;
/// 3. writes its Confirmed groups that have no `output_path`, in position
///    order, recording each one's partial when it is created and its
///    outcome when it is done: Written (with its stack), Skipped or Failed
///    (with the reason), and the warnings;
/// 4. the job is then Completed.
///
/// An output folder that cannot be used fails the job. `stop` is checked
/// before each group and each layer; a stack it interrupts is removed by the
/// writer, so its partial is forgotten.
pub(super) fn write(shared: &Shared, job: JobId, stop: &AtomicBool) -> Result<Settle, TaskError> {
    let (config, files, groups, leftovers) = {
        let writer = lock(&shared.writer);
        let stored = writer
            .job(job)?
            .ok_or_else(|| TaskError::Failed(format!("no job {job} in the store")))?;
        writer.set_job_status(job, JobStatus::Writing)?;
        let files = writer.load_analysis(job)?.files;
        let mut groups: Vec<(GroupId, Vec<usize>)> = Vec::new();
        let mut leftovers: Vec<(GroupId, PathBuf)> = Vec::new();
        for group in writer.groups(job)? {
            if group.output_path.is_some() {
                continue;
            }
            if let Some(partial) = group.partial_path {
                leftovers.push((group.id, partial));
            }
            if group.status == GroupStatus::Confirmed {
                groups.push((group.id, group.members));
            }
        }
        (stored.config, files, groups, leftovers)
    };
    shared.events.send(EngineEvent::JobStatusChanged {
        job,
        status: JobStatus::Writing,
    });
    if !leftovers.is_empty() {
        // A folder that cannot be used (a drive not mounted) must not make
        // its leftovers look gone: they come back with it.
        ensure_output_dir(&config).map_err(|error| TaskError::Failed(error_chain(&error)))?;
    }
    remove_leftovers(shared, job, &config.output_dir, &leftovers)?;
    if groups.is_empty() {
        return Ok(Settle::Status(JobStatus::Completed));
    }

    let (ids, members): (Vec<GroupId>, Vec<Vec<usize>>) = groups.into_iter().unzip();
    let progress = |progress: Progress| {
        shared
            .events
            .send(EngineEvent::JobProgress { job, progress });
    };
    let total = members.iter().map(Vec::len).sum();
    let recorder = Recorder {
        shared,
        job,
        ids: &ids,
        files: &files,
        ticker: Ticker::new(&progress, Stage::Write, total, 0),
        open: Mutex::new(None),
    };
    let written = write_groups(
        &config,
        &files,
        &members,
        WriteOptions::default(),
        &recorder,
        stop,
    );
    match written {
        Ok(_) => Ok(Settle::Status(JobStatus::Completed)),
        Err(WriteError::Cancelled) => {
            recorder.forget_open_partial();
            Err(TaskError::Stopped)
        }
        Err(error) => Err(TaskError::Failed(error_chain(&error))),
    }
}

/// Removes the unfinished stacks of `leftovers` (each group's recorded
/// partial, in `output_dir`, which exists) of `job`, clearing each record
/// once nothing of it is left. A stack that cannot be removed keeps its
/// record, so the next write or delete tries again, and fails the write
/// before it writes anything.
fn remove_leftovers(
    shared: &Shared,
    job: JobId,
    output_dir: &Path,
    leftovers: &[(GroupId, PathBuf)],
) -> Result<(), TaskError> {
    let mut kept: Vec<&Path> = Vec::new();
    for (group, partial) in leftovers {
        tracing::info!(job, partial = %partial.display(), "removing an unfinished stack");
        if remove_partial(output_dir, partial) {
            lock(&shared.writer).set_group_partial(*group, None)?;
        } else {
            kept.push(partial);
        }
    }
    match kept[..] {
        [] => Ok(()),
        [only] => Err(TaskError::Failed(format!(
            "cannot remove the unfinished stack {}",
            only.display()
        ))),
        [first, ref more @ ..] => Err(TaskError::Failed(format!(
            "cannot remove the unfinished stack {} and {} more",
            first.display(),
            more.len()
        ))),
    }
}

/// The observer of a job's write: it ticks the progress and records each
/// stack's partial and outcome.
struct Recorder<'a> {
    shared: &'a Shared,
    job: JobId,
    /// The id of each group written, by its index in the write.
    ids: &'a [GroupId],
    files: &'a [FileRecord],
    ticker: Ticker<'a>,
    /// The group whose partial is recorded and not done yet.
    open: Mutex<Option<GroupId>>,
}

impl Recorder<'_> {
    /// The writer removed the stack a cancel interrupted, with its partial
    /// and spill files: the partial recorded for it names nothing.
    fn forget_open_partial(&self) {
        if let Some(group) = lock(&self.open).take() {
            let forgotten = lock(&self.shared.writer).set_group_partial(group, None);
            self.check(forgotten);
        }
    }

    /// Reports a change the store could not record. The write goes on: at
    /// worst a partial is not recorded, or a group is written again.
    fn check(&self, stored: Result<(), StoreError>) {
        if let Err(error) = stored {
            self.shared.report_error(self.job, &error);
            self.shared.events.flush();
        }
    }
}

impl WriteObserver for Recorder<'_> {
    fn layer(&self, file: usize) {
        self.ticker.tick(&self.files[file].path);
    }

    fn partial_created(&self, group: usize, partial: &Path) {
        let id = self.ids[group];
        *lock(&self.open) = Some(id);
        let recorded = lock(&self.shared.writer).set_group_partial(id, Some(partial));
        self.check(recorded);
    }

    fn group_done(
        &self,
        group: usize,
        outcome: &Result<WriteOutcome, String>,
        warnings: &[(usize, Warning)],
    ) {
        let id = self.ids[group];
        *lock(&self.open) = None;
        let (status, output, error) = match outcome {
            Ok(WriteOutcome::Written { path, .. }) => {
                (GroupStatus::Written, Some(path.as_path()), None)
            }
            Ok(WriteOutcome::Skipped { reason }) => {
                (GroupStatus::Skipped, None, Some(reason.as_str()))
            }
            Err(error) => (GroupStatus::Failed, None, Some(error.as_str())),
        };
        let recorded =
            lock(&self.shared.writer).set_group_outcome(id, status, output, error, warnings);
        self.check(recorded);
        self.shared
            .events
            .send(EngineEvent::GroupsChanged { job: self.job });
    }
}
