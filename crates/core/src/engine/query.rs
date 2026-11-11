//! The queries: what the UI and the CLI read, through the engine's
//! read-only connection. [`Engine::review`] reads what a review screen
//! shows in one snapshot.

use std::path::PathBuf;

use serde::de::DeserializeOwned;

use super::scan::thumbnail_key;
use super::{Engine, EngineError, GroupId, JobCounts, JobId, lock};
use crate::analyze::Analysis;
use crate::exif::ExifData;
use crate::model::{FileKind, GroupOrigin, GroupStatus, JobConfig, JobStatus, MlStatus, Warning};
use crate::store::{StoredGroup, StoredJob};
use crate::thumbs;

/// A job, for the job list.
#[derive(Debug, Clone, PartialEq)]
pub struct JobView {
    pub id: JobId,
    /// UTC, ISO 8601 with milliseconds.
    pub created_at: String,
    pub status: JobStatus,
    /// Its scan's analysis is saved: it has files and groups to review, whatever its
    /// status (a job cancelled or failed during its write has one, a job cancelled
    /// during its scan's examination has none).
    pub scan_done: bool,
    pub config: JobConfig,
    pub error: Option<String>,
    /// Why Stage 3 was skipped for it, when its model failed (spec §8.3).
    pub warning: Option<String>,
    pub counts: JobCounts,
}

/// A group of a job, for the review.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupView {
    pub id: GroupId,
    /// Its place among the job's groups, from 0.
    pub position: usize,
    pub status: GroupStatus,
    pub origin: GroupOrigin,
    /// A decision the user made on its members no longer applies as it was.
    pub review_reset: bool,
    /// File ids in layer order: `[0]` is the top layer.
    pub members: Vec<usize>,
    pub bucket: String,
    pub max_distance: u32,
    pub ml_status: MlStatus,
    pub ml_score: Option<f32>,
    /// The stack its write produced.
    pub output_path: Option<PathBuf>,
    /// Why its write failed, or why it was skipped.
    pub error: Option<String>,
    /// The warnings of its last write, by file id.
    pub warnings: Vec<(usize, Warning)>,
}

/// A file of a job's analysis, for the review.
#[derive(Debug, Clone, PartialEq)]
pub struct FileView {
    /// Its id in the job's analysis.
    pub idx: usize,
    pub path: PathBuf,
    pub kind: FileKind,
    pub exif: Option<ExifData>,
    pub error: Option<String>,
    pub warnings: Vec<Warning>,
    /// For a passive file of a RAW+JPEG pair: its active partner's id.
    pub paired_with: Option<usize>,
    /// Its review thumbnail, when the scan made one and it is still there.
    pub thumb: Option<PathBuf>,
    /// The user put it in the tray.
    pub pinned: bool,
}

/// What a review screen shows of a job, read in one snapshot by
/// [`Engine::review`].
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewView {
    /// As [`Engine::job`] gives it.
    pub job: JobView,
    /// As [`Engine::groups`] gives them: by position.
    pub groups: Vec<GroupView>,
    /// As [`Engine::unstacked`] gives it: the tray, ascending.
    pub unstacked: Vec<usize>,
}

impl Engine {
    /// Every job, oldest first.
    pub fn jobs(&self) -> Result<Vec<JobView>, EngineError> {
        let reader = lock(&self.shared.reader);
        reader
            .jobs()?
            .into_iter()
            .map(|job| {
                let counts = reader.job_counts(job.id)?;
                Ok(job_view(job, counts))
            })
            .collect()
    }

    pub fn job(&self, job: JobId) -> Result<JobView, EngineError> {
        let reader = lock(&self.shared.reader);
        let stored = reader.job(job)?.ok_or(EngineError::UnknownJob(job))?;
        let counts = reader.job_counts(job)?;
        Ok(job_view(stored, counts))
    }

    /// The groups of `job`, by position.
    pub fn groups(&self, job: JobId) -> Result<Vec<GroupView>, EngineError> {
        let reader = lock(&self.shared.reader);
        if reader.job(job)?.is_none() {
            return Err(EngineError::UnknownJob(job));
        }
        Ok(reader.groups(job)?.into_iter().map(group_view).collect())
    }

    /// The files of `job`'s analysis, by id; none before its scan is done.
    pub fn files(&self, job: JobId) -> Result<Vec<FileView>, EngineError> {
        let (analysis, pinned) = {
            let reader = lock(&self.shared.reader);
            (reader.load_analysis(job)?, reader.pinned(job)?)
        };
        let mut warnings: Vec<Vec<Warning>> = vec![Vec::new(); analysis.files.len()];
        for (idx, warning) in analysis.warnings {
            if let Some(file_warnings) = warnings.get_mut(idx) {
                file_warnings.push(warning);
            }
        }
        let thumbs_dir = self.shared.job_thumbs(job);
        let files = analysis
            .files
            .into_iter()
            .zip(warnings)
            .enumerate()
            .map(|(idx, (file, warnings))| {
                let thumb = thumbs::thumbnail_path(&thumbs_dir, &thumbnail_key(&file.path));
                FileView {
                    idx,
                    kind: file.kind,
                    exif: file.exif,
                    error: file.error,
                    warnings,
                    paired_with: file.paired_with,
                    thumb: thumb.is_file().then_some(thumb),
                    pinned: pinned.binary_search(&idx).is_ok(),
                    path: file.path,
                }
            })
            .collect();
        Ok(files)
    }

    /// The hashed files of `job` in no group (the tray), ascending.
    pub fn unstacked(&self, job: JobId) -> Result<Vec<usize>, EngineError> {
        Ok(lock(&self.shared.reader).unstacked(job)?)
    }

    /// The job, its groups and its tray, as [`Engine::job`],
    /// [`Engine::groups`] and [`Engine::unstacked`] give them, read from one
    /// snapshot of the database: an edit the worker commits meanwhile is in
    /// all three or in none.
    pub fn review(&self, job: JobId) -> Result<ReviewView, EngineError> {
        let stored = lock(&self.shared.reader).review(job)?;
        Ok(ReviewView {
            job: job_view(stored.job, stored.counts),
            groups: stored.groups.into_iter().map(group_view).collect(),
            unstacked: stored.unstacked,
        })
    }

    /// The preference `key`, or `None` when it was never set. A value of
    /// another type than `T` is an error ([`crate::StoreError::Json`]).
    pub fn pref<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, EngineError> {
        Ok(lock(&self.shared.reader).pref(key)?)
    }

    /// The analysis of `job` as stored, with its groups as proposed groups in
    /// position order.
    pub fn analysis(&self, job: JobId) -> Result<Analysis, EngineError> {
        Ok(lock(&self.shared.reader).load_analysis(job)?)
    }
}

fn job_view(job: StoredJob, counts: JobCounts) -> JobView {
    JobView {
        id: job.id,
        created_at: job.created_at,
        status: job.status,
        scan_done: job.scan_done,
        config: job.config,
        error: job.error,
        warning: job.warning,
        counts,
    }
}

fn group_view(group: StoredGroup) -> GroupView {
    GroupView {
        id: group.id,
        position: group.position,
        status: group.status,
        origin: group.origin,
        review_reset: group.review_reset,
        members: group.members,
        bucket: group.bucket,
        max_distance: group.max_distance,
        ml_status: group.ml_status,
        ml_score: group.ml_score,
        output_path: group.output_path,
        error: group.error,
        warnings: group.warnings,
    }
}
