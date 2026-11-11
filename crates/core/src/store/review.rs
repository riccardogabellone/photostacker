//! A job's review: read in one snapshot, and each edit stored in one
//! transaction. The thresholds, the active file of a pair, the pins and the
//! groups change together, so a crash never leaves groups that name a
//! passive file or were grouped with other thresholds than the job's.

use super::files::{Swap, unstacked_in, write_pinned, write_swap};
use super::groups::{
    GroupColumns, StoredGroup, groups_in, write_automatic, write_group, written_groups,
};
use super::{
    GroupId, JobCounts, JobId, Store, StoreError, StoredJob, enum_text, jobs, read_snapshot,
};
use crate::group::Regrouped;
use crate::model::{GroupOrigin, GroupStatus, JobConfig, MlStatus};

/// What a review screen shows of a job, as [`Store::review`] reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredReview {
    pub job: StoredJob,
    pub counts: JobCounts,
    /// By position.
    pub groups: Vec<StoredGroup>,
    /// The hashed files in no group (the tray), ascending.
    pub unstacked: Vec<usize>,
}

/// A manual group as [`Store::apply_review`] stores it.
#[derive(Debug, Clone, PartialEq)]
pub struct ManualRow {
    /// Its row, or `None` for a group the edit makes.
    pub id: Option<GroupId>,
    /// File ids in layer order: `[0]` is the top layer.
    pub members: Vec<usize>,
    pub status: GroupStatus,
    pub bucket: String,
    pub max_distance: u32,
}

/// What one review edit of a job changes.
#[derive(Debug, Clone, Copy)]
pub struct ReviewChange<'a> {
    /// The job's new config, when the edit changes it.
    pub config: Option<&'a JobConfig>,
    /// The pair whose active file changes, when the edit swaps one.
    pub swap: Option<&'a Swap<'a>>,
    /// The files whose pin changes, with their new pin.
    pub pins: &'a [(usize, bool)],
    /// The automatic groups, and in `removed` every group the edit deletes,
    /// automatic or manual.
    pub regrouped: &'a Regrouped,
    /// Every manual group of the job, in the order of their positions.
    pub manual: &'a [ManualRow],
}

impl Store {
    /// The review of `job` from one read snapshot: the job, its counts, its
    /// groups and its tray, as [`Store::job`], [`Store::job_counts`],
    /// [`Store::groups`] and [`Store::unstacked`] give them, all from the
    /// same state of the database.
    pub fn review(&self, job: JobId) -> Result<StoredReview, StoreError> {
        let snapshot = read_snapshot(&self.conn)?;
        review_in(&snapshot, job)
    }

    /// Stores `change` to `job` in one transaction:
    ///
    /// 1. the config;
    /// 2. the swap, as [`Store::swap_active`] does;
    /// 3. the pins;
    /// 4. the automatic groups, as [`Store::apply_regroup`] does, which
    ///    deletes `regrouped.removed`;
    /// 5. each manual group, with its members, status, bucket and distance,
    ///    no review flag and no Stage-3 outcome: updated when it has a row,
    ///    inserted otherwise.
    ///
    /// The groups are numbered: the automatic ones in `regrouped`'s order,
    /// then the manual ones in the order given. The write state of a kept
    /// group (partial, output, error, warnings) is untouched, and so is a
    /// group whose row and members are unchanged. Returns the ids of
    /// `change.manual`, in their order.
    pub fn apply_review(
        &mut self,
        job: JobId,
        change: &ReviewChange,
    ) -> Result<Vec<GroupId>, StoreError> {
        let tx = self.conn.transaction()?;
        jobs::ensure_job(&tx, job)?;
        if let Some(config) = change.config {
            jobs::write_config(&tx, job, config)?;
        }
        if let Some(swap) = change.swap {
            write_swap(&tx, job, swap)?;
        }
        for &(idx, pinned) in change.pins {
            write_pinned(&tx, job, idx, pinned)?;
        }
        let mut written = written_groups(&tx, job)?;
        let automatic = write_automatic(&tx, job, change.regrouped, &mut written)?.len();
        let origin = enum_text(&GroupOrigin::Manual)?;
        let ml_status = enum_text(&MlStatus::None)?;
        let mut ids = Vec::with_capacity(change.manual.len());
        for (offset, group) in change.manual.iter().enumerate() {
            let columns = GroupColumns {
                position: automatic + offset,
                status: enum_text(&group.status)?,
                origin: origin.clone(),
                review_reset: false,
                bucket: group.bucket.clone(),
                max_distance: group.max_distance,
                ml_status: ml_status.clone(),
                ml_score: None,
            };
            ids.push(write_group(
                &tx,
                job,
                &mut written,
                group.id,
                &columns,
                &group.members,
            )?);
        }
        tx.commit()?;
        Ok(ids)
    }
}

/// [`Store::review`] on `conn`, which must be inside a read snapshot.
fn review_in(conn: &rusqlite::Connection, job: JobId) -> Result<StoredReview, StoreError> {
    let stored = jobs::job_in(conn, job)?.ok_or(StoreError::UnknownJob(job))?;
    Ok(StoredReview {
        job: stored,
        counts: jobs::counts_in(conn, job)?,
        groups: groups_in(conn, job)?,
        unstacked: unstacked_in(conn, job)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::Analysis;
    use crate::group::RegroupedGroup;
    use crate::hash::HashPair;
    use crate::model::Warning;
    use crate::store::StoredGroup;
    use crate::store::files::{regrouped_of, sample_analysis};

    /// The sample analysis with its group `[2, 1]` stored, and its id.
    fn saved(store: &mut Store) -> (JobId, Analysis, GroupId) {
        let job = store.insert_job(&JobConfig::new("/in".into())).unwrap();
        let analysis = sample_analysis();
        store.save_analysis(job, &analysis).unwrap();
        let ids = store
            .apply_regroup(job, &regrouped_of(&analysis), &[])
            .unwrap();
        (job, analysis, ids[0])
    }

    fn summary(group: &StoredGroup) -> (usize, GroupStatus, GroupOrigin, Vec<usize>) {
        (
            group.position,
            group.status,
            group.origin,
            group.members.clone(),
        )
    }

    /// A reader on its own connection, as the UI's is, while the writer
    /// commits a change between two reads of one snapshot.
    #[test]
    fn a_review_reads_one_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photostacker.db");
        let mut writer = Store::open(&path).unwrap();
        let (job, _, group) = saved(&mut writer);
        let reader = Store::open_read_only(&path).unwrap();
        let before = reader.review(job).unwrap();
        assert_eq!(
            before,
            StoredReview {
                job: writer.job(job).unwrap().unwrap(),
                counts: writer.job_counts(job).unwrap(),
                groups: writer.groups(job).unwrap(),
                unstacked: writer.unstacked(job).unwrap(),
            }
        );

        let snapshot = read_snapshot(&reader.conn).unwrap();
        assert_eq!(review_in(&snapshot, job).unwrap(), before);
        // The group goes, so its photos go to the tray.
        let removed = Regrouped {
            removed: vec![group],
            ..Regrouped::default()
        };
        writer.apply_regroup(job, &removed, &[]).unwrap();
        assert_eq!(review_in(&snapshot, job).unwrap(), before);
        drop(snapshot);
        let after = reader.review(job).unwrap();
        assert_eq!((after.counts.groups(), after.groups.len()), (0, 0));
        assert_eq!(after.unstacked.len(), before.unstacked.len() + 2);
        assert!(matches!(
            reader.review(job + 1),
            Err(StoreError::UnknownJob(_))
        ));
    }

    #[test]
    fn a_review_edit_is_stored_whole() {
        let mut store = Store::in_memory().unwrap();
        let (job, _, auto) = saved(&mut store);
        store
            .set_group_status(auto, GroupStatus::Confirmed)
            .unwrap();
        let mut config = JobConfig::new("/in".into());
        config.hamming_threshold = 20;

        // The automatic group is reused; a manual group is made of 4 and 2.
        let regrouped = Regrouped {
            groups: vec![RegroupedGroup {
                id: Some(auto),
                members: vec![2, 1],
                bucket: "b".into(),
                max_distance: 7,
                status: GroupStatus::Confirmed,
                review_reset: false,
                ml_status: MlStatus::None,
                ml_score: None,
            }],
            ..Regrouped::default()
        };
        let made = ManualRow {
            id: None,
            members: vec![4, 3],
            status: GroupStatus::Proposed,
            bucket: "m".into(),
            max_distance: 12,
        };
        let ids = store
            .apply_review(
                job,
                &ReviewChange {
                    config: Some(&config),
                    swap: None,
                    pins: &[(4, true), (2, true), (2, false)],
                    regrouped: &regrouped,
                    manual: std::slice::from_ref(&made),
                },
            )
            .unwrap();
        let [manual] = ids[..] else { panic!("{ids:?}") };
        assert_ne!(manual, auto);
        let groups = store.groups(job).unwrap();
        assert_eq!(
            groups.iter().map(summary).collect::<Vec<_>>(),
            [
                (0, GroupStatus::Confirmed, GroupOrigin::Auto, vec![2, 1]),
                (1, GroupStatus::Proposed, GroupOrigin::Manual, vec![4, 3]),
            ]
        );
        assert_eq!(
            (groups[1].bucket.as_str(), groups[1].max_distance),
            ("m", 12)
        );
        assert_eq!(store.job(job).unwrap().unwrap().config, config);
        assert_eq!(store.pinned(job).unwrap(), [4]);

        // The manual group is updated in place, and the automatic one goes.
        let removed = Regrouped {
            removed: vec![auto],
            ..Regrouped::default()
        };
        let confirmed = ManualRow {
            id: Some(manual),
            status: GroupStatus::Confirmed,
            ..made
        };
        let ids = store
            .apply_review(
                job,
                &ReviewChange {
                    config: None,
                    swap: None,
                    pins: &[],
                    regrouped: &removed,
                    manual: &[confirmed],
                },
            )
            .unwrap();
        assert_eq!(ids, [manual]);
        let groups = store.groups(job).unwrap();
        assert_eq!(
            groups.iter().map(summary).collect::<Vec<_>>(),
            [(0, GroupStatus::Confirmed, GroupOrigin::Manual, vec![4, 3])]
        );
        assert_eq!(store.job(job).unwrap().unwrap().config, config);
    }

    /// Makes `store` record each row written to `groups` or `group_members`
    /// (inserted, updated or deleted) in a temporary table, with its group.
    fn record_group_writes(store: &Store) {
        let mut sql =
            String::from("CREATE TEMP TABLE test_writes (tbl TEXT, op TEXT, group_id INTEGER);");
        for (table, id) in [("groups", "id"), ("group_members", "group_id")] {
            for (op, row) in [("INSERT", "new"), ("UPDATE", "new"), ("DELETE", "old")] {
                sql.push_str(&format!(
                    "CREATE TEMP TRIGGER test_{table}_{op} AFTER {op} ON main.{table}
                     BEGIN INSERT INTO test_writes VALUES ('{table}', '{op}', {row}.{id}); END;"
                ));
            }
        }
        store.conn.execute_batch(&sql).unwrap();
    }

    /// The rows written since the last call, as `(table, operation, group)`,
    /// in order.
    fn group_writes(store: &Store) -> Vec<(String, String, GroupId)> {
        let mut statement = store
            .conn
            .prepare("SELECT tbl, op, group_id FROM test_writes ORDER BY rowid")
            .unwrap();
        let writes = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        store.conn.execute("DELETE FROM test_writes", []).unwrap();
        writes
    }

    /// An automatic group as a regrouping gives it, with every column the
    /// regrouping writes set.
    fn automatic(id: Option<GroupId>, members: &[usize], review_reset: bool) -> RegroupedGroup {
        RegroupedGroup {
            id,
            members: members.to_vec(),
            bucket: "|Bench Camera||@0".into(),
            max_distance: 8,
            status: GroupStatus::Proposed,
            review_reset,
            ml_status: MlStatus::Confirmed,
            ml_score: Some(0.912_345_6),
        }
    }

    /// A review edit that changes only the groups.
    fn change<'a>(regrouped: &'a Regrouped, manual: &'a [ManualRow]) -> ReviewChange<'a> {
        ReviewChange {
            config: None,
            swap: None,
            pins: &[],
            regrouped,
            manual,
        }
    }

    #[test]
    fn an_unchanged_group_is_not_rewritten() {
        let mut store = Store::in_memory().unwrap();
        let job = store.insert_job(&JobConfig::new("/in".into())).unwrap();
        let mut manual = ManualRow {
            id: None,
            members: vec![7, 6],
            status: GroupStatus::Proposed,
            bucket: "m".into(),
            max_distance: 3,
        };
        let made = Regrouped {
            groups: vec![
                automatic(None, &[2, 1], false),
                automatic(None, &[4, 0], true),
                automatic(None, &[5, 3], false),
            ],
            ..Regrouped::default()
        };
        manual.id = Some(
            store
                .apply_review(job, &change(&made, std::slice::from_ref(&manual)))
                .unwrap()[0],
        );
        let ids: Vec<GroupId> = store.groups(job).unwrap().iter().map(|g| g.id).collect();
        let [a, b, c, m] = ids[..] else {
            panic!("{ids:?}")
        };
        let mut same = made.clone();
        for (group, id) in same.groups.iter_mut().zip(&ids) {
            group.id = Some(*id);
        }
        record_group_writes(&store);

        // The same groups again: nothing is written.
        let before = store.groups(job).unwrap();
        store
            .apply_review(job, &change(&same, std::slice::from_ref(&manual)))
            .unwrap();
        assert_eq!(group_writes(&store), []);
        assert_eq!(store.groups(job).unwrap(), before);

        // `b` goes: `c` and `m` move up a place, and only their positions
        // are written.
        let without_b = Regrouped {
            groups: vec![same.groups[0].clone(), same.groups[2].clone()],
            removed: vec![b],
            ..Regrouped::default()
        };
        store
            .apply_review(job, &change(&without_b, std::slice::from_ref(&manual)))
            .unwrap();
        let writes = group_writes(&store);
        let (rows, members): (Vec<_>, Vec<_>) =
            writes.iter().partition(|(table, ..)| table == "groups");
        let op = |op: &str, id| ("groups".to_string(), op.to_string(), id);
        assert_eq!(
            rows.into_iter().cloned().collect::<Vec<_>>(),
            [op("DELETE", b), op("UPDATE", c), op("UPDATE", m)]
        );
        // Only the deleted group's members go.
        assert!(members.iter().all(|(.., id)| *id == b), "{members:?}");
        let positions: Vec<(GroupId, usize)> = store
            .groups(job)
            .unwrap()
            .iter()
            .map(|group| (group.id, group.position))
            .collect();
        assert_eq!(positions, [(a, 0), (c, 1), (m, 2)]);

        // A new layer order rewrites the members of that group alone.
        let mut reordered = without_b.clone();
        reordered.removed.clear();
        reordered.groups[0].members = vec![1, 2];
        store
            .apply_review(job, &change(&reordered, std::slice::from_ref(&manual)))
            .unwrap();
        let writes = group_writes(&store);
        assert!(!writes.is_empty());
        assert!(
            writes
                .iter()
                .all(|(table, _, id)| table == "group_members" && *id == a),
            "{writes:?}"
        );
        assert_eq!(store.groups(job).unwrap()[0].members, [1, 2]);
    }

    #[test]
    fn a_failing_review_edit_changes_nothing() {
        let mut store = Store::in_memory().unwrap();
        let (job, analysis, auto) = saved(&mut store);
        let before = (
            store.groups(job).unwrap(),
            store.load_analysis(job).unwrap(),
        );
        assert_eq!(before.1, analysis);

        // The swap is stored first; then the pin of a missing file fails.
        let swap = Swap {
            promoted: 0,
            exif: None,
            hashes: HashPair { phash: 1, dhash: 2 },
            normalised: HashPair { phash: 3, dhash: 4 },
            warnings: &[Warning::IccMismatch],
            demoted_exif: None,
        };
        let attempt = store.apply_review(
            job,
            &ReviewChange {
                config: None,
                swap: Some(&swap),
                pins: &[(9, true)],
                regrouped: &Regrouped {
                    removed: vec![auto],
                    ..Regrouped::default()
                },
                manual: &[],
            },
        );
        assert!(matches!(
            attempt,
            Err(StoreError::UnknownFile { idx: 9, .. })
        ));
        assert_eq!(
            (
                store.groups(job).unwrap(),
                store.load_analysis(job).unwrap()
            ),
            before
        );
        // A manual row that names another job's group is refused too.
        let attempt = store.apply_review(
            job,
            &ReviewChange {
                config: None,
                swap: None,
                pins: &[],
                regrouped: &Regrouped::default(),
                manual: &[ManualRow {
                    id: Some(auto + 100),
                    members: vec![4, 2],
                    status: GroupStatus::Proposed,
                    bucket: String::new(),
                    max_distance: 0,
                }],
            },
        );
        assert!(matches!(attempt, Err(StoreError::UnknownGroup(_))));
        assert_eq!(store.groups(job).unwrap(), before.0);
    }
}
