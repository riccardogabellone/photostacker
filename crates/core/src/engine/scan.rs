//! The scan task: the job's analysis with the store as its exam cache, then
//! its first grouping. The exam cache serves the swap of a pair's active
//! file too (`review.rs`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use image::RgbImage;

use super::review::regroup_job;
use super::worker::{Settle, Task, TaskError};
use super::{EngineEvent, JobId, Shared, error_chain, lock, stage3};
use crate::analyze::{
    AnalyzeError, AnalyzeHooks, EXAM_FORMAT, ExamCache, ExamRole, FileExam, Progress, analyze_with,
};
use crate::discover::DiscoveredFile;
use crate::fsio::display_path;
use crate::model::{GroupStatus, JobStatus};
use crate::store::{ExamKey, paths};
use crate::thumbs;

/// Scans `job`, which must be queued:
///
/// 1. sets it Scanning;
/// 2. analyses its folder with the store as the exam cache, so each file
///    examined is a checkpoint, and the review thumbnails go to
///    `thumbs/<job>/`; each file that fails is reported as it is known
///    ([`EngineEvent::FileFailed`]);
/// 3. saves the analysis, which happens once per job: the scan is done, and
///    is never saved again, once `scan_done` is set;
/// 4. when Stage 3 is available to the job, groups it once to learn which
///    photos its ambiguous groups need embeddings of, and computes and
///    stores them (`stage3.rs`);
/// 5. groups it, with nothing manual, pinned or existing yet, and sets
///    `scan_done`. An `auto_confirm` job's groups are stored confirmed.
///
/// The job then waits in Review Pending, or an `auto_confirm` job is queued
/// for its write. A job in review whose model Stage 3 could not use is
/// checked again once its status is stored, in case the model was installed
/// meanwhile ([`Settle::ReviewWithoutItsModel`]). `stop` is checked by the
/// analysis before each file, once more before the save, and by Stage 3
/// before each photo and batch.
pub(super) fn scan(shared: &Shared, job: JobId, stop: &AtomicBool) -> Result<Settle, TaskError> {
    let config = {
        let writer = lock(&shared.writer);
        let stored = writer
            .job(job)?
            .ok_or_else(|| TaskError::Failed(format!("no job {job} in the store")))?;
        writer.set_job_status(job, JobStatus::Scanning)?;
        stored.config
    };
    shared.events.send(EngineEvent::JobStatusChanged {
        job,
        status: JobStatus::Scanning,
    });

    let cache = StoreCache::new(shared, job);
    let progress = |progress: Progress| {
        shared
            .events
            .send(EngineEvent::JobProgress { job, progress });
    };
    let file_failed = |path: &Path, message: &str| {
        shared.events.send(EngineEvent::FileFailed {
            job,
            path: path.to_path_buf(),
            message: message.to_owned(),
        });
    };
    let hooks = AnalyzeHooks {
        progress: &progress,
        cancel: stop,
        cache: Some(&cache),
        file_failed: Some(&file_failed),
    };
    let analysis = analyze_with(&config, &hooks).map_err(|error| match error {
        AnalyzeError::Cancelled => TaskError::Stopped,
        other => TaskError::Failed(error_chain(&other)),
    })?;
    if stop.load(Ordering::SeqCst) {
        return Err(TaskError::Stopped);
    }

    let files = &analysis.files;
    lock(&shared.writer).save_analysis(job, &analysis)?;
    let embeddings = stage3::prepare(shared, job, &config, files, stop, |stored| {
        Ok(regroup_job(&config, files, &[], &[], &[], Some(stored)).needs_embeddings)
    })?;
    // A job that names a model its Stage 3 could not use: once in review, it
    // looks again, for a model installed meanwhile.
    let without_its_model = embeddings.is_none() && config.ml_model.is_some();
    let mut regrouped = regroup_job(&config, files, &[], &[], &[], embeddings.as_ref());
    if config.auto_confirm {
        for group in &mut regrouped.groups {
            group.status = GroupStatus::Confirmed;
        }
    }
    {
        let mut writer = lock(&shared.writer);
        writer.apply_regroup(job, &regrouped, &[])?;
        writer.set_scan_done(job, true)?;
    }
    shared.events.send(EngineEvent::GroupsChanged { job });
    Ok(if config.auto_confirm {
        Settle::Then(Task::Write(job))
    } else if without_its_model {
        Settle::ReviewWithoutItsModel
    } else {
        Settle::Status(JobStatus::ReviewPending)
    })
}

/// What a file's review thumbnail is named by in its job's folder: its path,
/// as the store keeps it. The active exam of a file and its thumbnail as a
/// promoted file share it, so a file has one review thumbnail.
pub(super) fn thumbnail_key(path: &Path) -> Vec<u8> {
    paths::to_blob(path)
}

/// The exam cache of one job, in the store (spec §7.3). It follows
/// [`ExamCache`]'s contract: no method panics, and each swallows (and logs)
/// its own errors.
pub(super) struct StoreCache<'a> {
    shared: &'a Shared,
    job: JobId,
    /// The job's thumbnail folder.
    thumbs: PathBuf,
}

impl<'a> StoreCache<'a> {
    /// The cache of `job`, whose review thumbnails go to `thumbs/<job>/`.
    pub(super) fn new(shared: &'a Shared, job: JobId) -> Self {
        Self {
            shared,
            job,
            thumbs: shared.job_thumbs(job),
        }
    }

    fn key<'k>(&self, role: ExamRole, file: &'k DiscoveredFile) -> ExamKey<'k> {
        ExamKey {
            job: self.job,
            role: role.as_str(),
            path: &file.path,
            fingerprint: &file.fingerprint,
            format: EXAM_FORMAT,
        }
    }

    fn write_thumbnail(&self, file: &DiscoveredFile, thumb: &RgbImage) -> Option<PathBuf> {
        match thumbs::write_thumbnail(&self.thumbs, &thumbnail_key(&file.path), thumb) {
            Ok(path) => Some(path),
            Err(error) => {
                tracing::warn!(path = %display_path(&file.path), %error, "cannot write a review thumbnail");
                None
            }
        }
    }
}

impl ExamCache for StoreCache<'_> {
    /// The stored exam, when it is served with its review thumbnail: an
    /// active exam whose thumbnail file is gone is a miss, so the file is
    /// examined (and its thumbnail written) again.
    fn get(&self, role: ExamRole, file: &DiscoveredFile) -> Option<FileExam> {
        let stored = lock(&self.shared.writer).exam::<FileExam>(&self.key(role, file));
        let stored = match stored {
            Ok(stored) => stored?,
            Err(error) => {
                tracing::warn!(path = %display_path(&file.path), %error, "cannot read a cached exam");
                return None;
            }
        };
        let thumb_ok = match &stored.thumb {
            Some(thumb) => thumb.is_file(),
            None => role == ExamRole::Passive,
        };
        thumb_ok.then_some(stored.result)
    }

    /// Writes the review thumbnail (an active exam's), then stores the exam
    /// with its path. Without its thumbnail an active exam could not be
    /// served, so it is not stored.
    fn put(
        &self,
        role: ExamRole,
        file: &DiscoveredFile,
        exam: &FileExam,
        thumb: Option<&RgbImage>,
    ) {
        let thumb_path = match thumb {
            Some(thumb) => match self.write_thumbnail(file, thumb) {
                Some(path) => Some(path),
                None => return,
            },
            None => None,
        };
        let stored =
            lock(&self.shared.writer).put_exam(&self.key(role, file), exam, thumb_path.as_deref());
        if let Err(error) = stored {
            tracing::warn!(path = %display_path(&file.path), %error, "cannot cache an exam");
        }
    }

    /// Writes a promoted file's review thumbnail, replacing the one of an
    /// earlier run under the same name.
    fn thumbnail(&self, file: &DiscoveredFile, thumb: &RgbImage) {
        self.write_thumbnail(file, thumb);
    }
}
