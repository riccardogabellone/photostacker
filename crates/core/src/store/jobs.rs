//! Jobs: their config, status and checkpoints.

use rusqlite::{OptionalExtension, params};

use super::{
    GroupId, JobId, Store, StoreError, changed_one, enum_from_text, enum_text, groups,
    read_snapshot, vectors,
};
use crate::model::{GroupStatus, JobConfig, JobStatus};

/// What a job holds, counted: its files and its groups by status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobCounts {
    /// The files of its analysis; none until its scan is saved.
    pub files: usize,
    /// Those of its files that could not be analysed.
    pub errors: usize,
    pub proposed: usize,
    pub confirmed: usize,
    pub rejected: usize,
    pub written: usize,
    pub skipped: usize,
    pub failed: usize,
}

impl JobCounts {
    /// All its groups, whatever their status.
    pub fn groups(&self) -> usize {
        self.proposed + self.confirmed + self.rejected + self.written + self.skipped + self.failed
    }
}

/// A job as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredJob {
    pub id: JobId,
    /// When it was submitted: UTC, ISO 8601 with milliseconds, such as
    /// `2026-11-08T10:20:30.250Z`.
    pub created_at: String,
    pub status: JobStatus,
    /// Its analysis is saved and grouped, so a retry writes instead of
    /// scanning again.
    pub scan_done: bool,
    pub config: JobConfig,
    pub error: Option<String>,
    /// Why Stage 3 was skipped for the job, when its model failed (spec
    /// §8.3).
    pub warning: Option<String>,
}

const JOB_COLUMNS: &str = "id, created_at, status, scan_done, config_json, error, warning";

type JobRow = (
    JobId,
    String,
    String,
    bool,
    String,
    Option<String>,
    Option<String>,
);

impl Store {
    /// Adds a [`JobStatus::Queued`] job with `config`, created now.
    pub fn insert_job(&self, config: &JobConfig) -> Result<JobId, StoreError> {
        self.conn.execute(
            "INSERT INTO jobs (created_at, status, config_json)
             VALUES (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?1, ?2)",
            params![
                enum_text(&JobStatus::Queued)?,
                serde_json::to_string(config)?
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn set_job_status(&self, job: JobId, status: JobStatus) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE jobs SET status = ?2 WHERE id = ?1",
            params![job, enum_text(&status)?],
        )?;
        changed_one(changed, StoreError::UnknownJob(job))
    }

    pub fn set_job_error(&self, job: JobId, error: Option<&str>) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE jobs SET error = ?2 WHERE id = ?1",
            params![job, error],
        )?;
        changed_one(changed, StoreError::UnknownJob(job))
    }

    pub fn set_job_warning(&self, job: JobId, warning: Option<&str>) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE jobs SET warning = ?2 WHERE id = ?1",
            params![job, warning],
        )?;
        changed_one(changed, StoreError::UnknownJob(job))
    }

    pub fn set_scan_done(&self, job: JobId, done: bool) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE jobs SET scan_done = ?2 WHERE id = ?1",
            params![job, done],
        )?;
        changed_one(changed, StoreError::UnknownJob(job))
    }

    /// Queues `job` again for a retry, in one transaction: it becomes
    /// [`JobStatus::Queued`] with its error cleared, and `groups`, its
    /// stranded groups (none for a scan), go back to Confirmed as
    /// [`Store::requeue_groups`] puts them. Nothing changes when any of it
    /// fails, such as for a group that is not one of `job`'s.
    pub fn requeue_job(&mut self, job: JobId, groups: &[GroupId]) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        let changed = tx.execute(
            "UPDATE jobs SET status = ?2, error = NULL WHERE id = ?1",
            params![job, enum_text(&JobStatus::Queued)?],
        )?;
        changed_one(changed, StoreError::UnknownJob(job))?;
        groups::requeue_in(&tx, job, groups)?;
        tx.commit()?;
        Ok(())
    }

    /// The job `job`, or `None` when there is none.
    pub fn job(&self, job: JobId) -> Result<Option<StoredJob>, StoreError> {
        job_in(&self.conn, job)
    }

    /// Every job, oldest first.
    pub fn jobs(&self) -> Result<Vec<StoredJob>, StoreError> {
        let mut statement = self
            .conn
            .prepare(&format!("SELECT {JOB_COLUMNS} FROM jobs ORDER BY id"))?;
        let rows = statement.query_map([], job_row)?;
        rows.map(|row| stored_job(row?)).collect()
    }

    /// The counts of `job`, from one snapshot: all zero for a job without an
    /// analysis, or without a row.
    pub fn job_counts(&self, job: JobId) -> Result<JobCounts, StoreError> {
        let snapshot = read_snapshot(&self.conn)?;
        counts_in(&snapshot, job)
    }

    /// Deletes the job and everything stored for it: its embeddings first
    /// (a vec0 table cannot cascade), then its row, whose deletion cascades
    /// to its files, skips, exams, groups and their members and warnings.
    /// Returns whether there was such a job.
    pub fn delete_job(&mut self, job: JobId) -> Result<bool, StoreError> {
        let tx = self.conn.transaction()?;
        vectors::delete_job_vectors(&tx, job)?;
        let deleted = tx.execute("DELETE FROM jobs WHERE id = ?1", [job])?;
        tx.commit()?;
        Ok(deleted == 1)
    }
}

/// Replaces the config of `job` with `config`, which the caller validated.
pub(super) fn write_config(
    conn: &rusqlite::Connection,
    job: JobId,
    config: &JobConfig,
) -> Result<(), StoreError> {
    let changed = conn.execute(
        "UPDATE jobs SET config_json = ?2 WHERE id = ?1",
        params![job, serde_json::to_string(config)?],
    )?;
    changed_one(changed, StoreError::UnknownJob(job))
}

/// [`Store::job`] on `conn`.
pub(super) fn job_in(
    conn: &rusqlite::Connection,
    job: JobId,
) -> Result<Option<StoredJob>, StoreError> {
    conn.query_row(
        &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1"),
        [job],
        job_row,
    )
    .optional()?
    .map(stored_job)
    .transpose()
}

/// [`Store::job_counts`] on `conn`, which must be inside a read snapshot.
pub(super) fn counts_in(conn: &rusqlite::Connection, job: JobId) -> Result<JobCounts, StoreError> {
    debug_assert!(
        !conn.is_autocommit(),
        "a multi-statement read outside a snapshot"
    );
    let (files, errors) = conn.query_row(
        "SELECT count(*), count(error) FROM files WHERE job_id = ?1",
        [job],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let mut counts = JobCounts {
        files,
        errors,
        ..JobCounts::default()
    };
    let mut statement =
        conn.prepare("SELECT status, count(*) FROM groups WHERE job_id = ?1 GROUP BY status")?;
    let rows = statement.query_map([job], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, usize>(1)?))
    })?;
    for row in rows {
        let (status, count) = row?;
        let slot = match enum_from_text(status)? {
            GroupStatus::Proposed => &mut counts.proposed,
            GroupStatus::Confirmed => &mut counts.confirmed,
            GroupStatus::Rejected => &mut counts.rejected,
            GroupStatus::Written => &mut counts.written,
            GroupStatus::Skipped => &mut counts.skipped,
            GroupStatus::Failed => &mut counts.failed,
        };
        *slot = count;
    }
    Ok(counts)
}

/// `Ok` when the job exists, so a write for a deleted job fails with
/// [`StoreError::UnknownJob`] rather than a foreign-key error.
pub(super) fn ensure_job(conn: &rusqlite::Connection, job: JobId) -> Result<(), StoreError> {
    let exists = conn
        .query_row("SELECT 1 FROM jobs WHERE id = ?1", [job], |_| Ok(()))
        .optional()?;
    exists.ok_or(StoreError::UnknownJob(job))
}

fn job_row(row: &rusqlite::Row) -> rusqlite::Result<JobRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

fn stored_job(
    (id, created_at, status, scan_done, config_json, error, warning): JobRow,
) -> Result<StoredJob, StoreError> {
    Ok(StoredJob {
        id,
        created_at,
        status: enum_from_text(status)?,
        scan_done,
        config: serde_json::from_str(&config_json)?,
        error,
        warning,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::OutputFormat;

    #[test]
    fn job_round_trips() {
        let store = Store::in_memory().unwrap();
        let mut config = JobConfig::new("/photos/trip".into());
        config.output_format = OutputFormat::Tiff;
        config.cosine_threshold = Some(0.85);
        let first = store.insert_job(&config).unwrap();
        let second = store.insert_job(&JobConfig::new("/b".into())).unwrap();

        let job = store.job(first).unwrap().unwrap();
        assert_eq!(job.status, JobStatus::Queued);
        assert!(!job.scan_done);
        assert_eq!(job.config, config);
        assert_eq!((job.error, job.warning), (None, None));
        // e.g. 2026-11-08T10:20:30.250Z
        assert_eq!(job.created_at.len(), 24, "{}", job.created_at);
        assert!(job.created_at.ends_with('Z'), "{}", job.created_at);

        store.set_job_status(first, JobStatus::Failed).unwrap();
        store.set_job_error(first, Some("disk full")).unwrap();
        store.set_scan_done(first, true).unwrap();
        let job = store.job(first).unwrap().unwrap();
        assert_eq!(
            (job.status, job.scan_done, job.error.as_deref()),
            (JobStatus::Failed, true, Some("disk full"))
        );
        store.set_job_error(first, None).unwrap();
        assert_eq!(store.job(first).unwrap().unwrap().error, None);
        store
            .set_job_warning(first, Some("the visual check was skipped: no runtime"))
            .unwrap();
        let job = store.job(first).unwrap().unwrap();
        assert_eq!(
            job.warning.as_deref(),
            Some("the visual check was skipped: no runtime")
        );
        store.set_job_warning(first, None).unwrap();
        assert_eq!(store.job(first).unwrap().unwrap().warning, None);

        let ids: Vec<JobId> = store.jobs().unwrap().iter().map(|job| job.id).collect();
        assert_eq!(ids, [first, second]);
    }

    #[test]
    fn job_counts_count_files_errors_and_groups_by_status() {
        let mut store = Store::in_memory().unwrap();
        let job = store.insert_job(&JobConfig::new("/in".into())).unwrap();
        assert_eq!(store.job_counts(job).unwrap(), JobCounts::default());

        let analysis = super::super::files::sample_analysis();
        store.save_analysis(job, &analysis).unwrap();
        let mut regrouped = super::super::files::regrouped_of(&analysis);
        let mut second = regrouped.groups[0].clone();
        second.members = vec![4, 0];
        regrouped.groups.push(second);
        let ids = store.apply_regroup(job, &regrouped, &[]).unwrap();
        store
            .set_group_status(ids[1], GroupStatus::Written)
            .unwrap();

        let counts = store.job_counts(job).unwrap();
        assert_eq!(
            counts,
            JobCounts {
                files: 5,
                errors: 1,
                proposed: 1,
                written: 1,
                ..JobCounts::default()
            }
        );
        assert_eq!(counts.groups(), 2);
        // Another job, or none, counts nothing.
        assert_eq!(store.job_counts(job + 1).unwrap(), JobCounts::default());
    }

    #[test]
    fn requeue_job_queues_the_job_and_its_groups_or_nothing() {
        let mut store = Store::in_memory().unwrap();
        let job = store.insert_job(&JobConfig::new("/in".into())).unwrap();
        let analysis = super::super::files::sample_analysis();
        store.save_analysis(job, &analysis).unwrap();
        let regrouped = super::super::files::regrouped_of(&analysis);
        let group = store.apply_regroup(job, &regrouped, &[]).unwrap()[0];
        store.set_job_status(job, JobStatus::Completed).unwrap();
        store.set_job_error(job, Some("disk full")).unwrap();
        store
            .set_group_outcome(group, GroupStatus::Failed, None, Some("disk full"), &[])
            .unwrap();
        let state = |store: &Store| {
            let stored = store.job(job).unwrap().unwrap();
            let group = &store.groups(job).unwrap()[0];
            (
                stored.status,
                stored.error,
                group.status,
                group.error.clone(),
            )
        };
        let before = state(&store);

        // A group that is not the job's refuses it all.
        assert!(matches!(
            store.requeue_job(job, &[group, group + 1]),
            Err(StoreError::UnknownGroup(_))
        ));
        assert_eq!(state(&store), before);

        store.requeue_job(job, &[group]).unwrap();
        assert_eq!(
            state(&store),
            (JobStatus::Queued, None, GroupStatus::Confirmed, None)
        );
        assert!(matches!(
            store.requeue_job(job + 1, &[]),
            Err(StoreError::UnknownJob(_))
        ));
    }

    #[test]
    fn a_missing_job_is_reported() {
        let mut store = Store::in_memory().unwrap();
        assert_eq!(store.job(7).unwrap(), None);
        assert!(matches!(
            store.set_job_status(7, JobStatus::Writing),
            Err(StoreError::UnknownJob(7))
        ));
        assert!(!store.delete_job(7).unwrap());
    }

    #[test]
    fn a_deleted_job_id_is_never_reused() {
        let mut store = Store::in_memory().unwrap();
        let first = store.insert_job(&JobConfig::new("/a".into())).unwrap();
        assert!(store.delete_job(first).unwrap());
        let second = store.insert_job(&JobConfig::new("/b".into())).unwrap();
        assert!(second > first);
    }
}
