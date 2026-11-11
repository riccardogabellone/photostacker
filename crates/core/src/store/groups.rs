//! Groups: their members in layer order, their review and their write state.
//!
//! A regrouping or a review edit writes only what changed: it reads the
//! job's groups as stored ([`written_groups`]) and leaves alone a group whose
//! row and members are already what it would write ([`write_group`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::types::ToSql;
use rusqlite::{Connection, OptionalExtension, params};

use super::{
    GroupId, JobId, Store, StoreError, changed_one, enum_from_text, enum_text, jobs, paths,
    read_snapshot,
};
use crate::group::{ManualGroup, Regrouped, RegroupedGroup};
use crate::model::{GroupOrigin, GroupStatus, MlStatus, Warning};

/// A group as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredGroup {
    pub id: GroupId,
    /// Its place among the job's groups, from 0.
    pub position: usize,
    pub status: GroupStatus,
    pub origin: GroupOrigin,
    pub review_reset: bool,
    /// File ids in layer order: `[0]` is the top layer.
    pub members: Vec<usize>,
    pub bucket: String,
    pub max_distance: u32,
    pub ml_status: MlStatus,
    pub ml_score: Option<f32>,
    /// The `.partial` file its write created and has not finished.
    pub partial_path: Option<PathBuf>,
    /// The stack its write produced.
    pub output_path: Option<PathBuf>,
    /// Why its write failed, or why it was skipped.
    pub error: Option<String>,
    /// The warnings of its last write, by file id, in the order given.
    pub warnings: Vec<(usize, Warning)>,
}

/// The columns of a group's row that a regrouping or a review edit writes:
/// all but its job, its id and its write state. The enums are their
/// spellings, as stored.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct GroupColumns {
    pub(super) position: usize,
    pub(super) status: String,
    pub(super) origin: String,
    pub(super) review_reset: bool,
    pub(super) bucket: String,
    pub(super) max_distance: u32,
    pub(super) ml_status: String,
    /// An f32 widened to f64, which narrows back exactly.
    pub(super) ml_score: Option<f64>,
}

impl GroupColumns {
    /// The row of the automatic group `group` at `position`.
    fn automatic(position: usize, group: &RegroupedGroup) -> Result<Self, StoreError> {
        Ok(Self {
            position,
            status: enum_text(&group.status)?,
            origin: enum_text(&GroupOrigin::Auto)?,
            review_reset: group.review_reset,
            bucket: group.bucket.clone(),
            max_distance: group.max_distance,
            ml_status: enum_text(&group.ml_status)?,
            ml_score: group.ml_score.map(f64::from),
        })
    }
}

/// A job's groups as stored, by id: their columns and their members in
/// layer order. [`write_group`] takes each group it writes out of it.
pub(super) type WrittenGroups = HashMap<GroupId, (GroupColumns, Vec<usize>)>;

/// A `groups` row as SQLite gives it, write state included.
struct GroupRow {
    id: GroupId,
    columns: GroupColumns,
    partial_path: Option<Vec<u8>>,
    output_path: Option<Vec<u8>>,
    error: Option<String>,
}

impl Store {
    /// Stores a regrouping of `job` in one transaction:
    ///
    /// - deletes the groups in `regrouped.removed`;
    /// - updates each automatic group that reuses an id (its status, review
    ///   flag, bucket, distance, Stage-3 outcome and members), and inserts
    ///   each new one;
    /// - stores each of `manual` as a manual group without a review flag,
    ///   with its members in the order given, which is its layer order;
    ///   their rows exist already, and their status is kept;
    /// - numbers the groups: the automatic ones in `regrouped`'s order, then
    ///   the manual ones in the order given.
    ///
    /// The write state of a kept group (partial, output, error, warnings) is
    /// untouched, and so is a group whose row and members are unchanged.
    /// Returns the ids of `regrouped.groups`, in their order.
    pub fn apply_regroup(
        &mut self,
        job: JobId,
        regrouped: &Regrouped,
        manual: &[ManualGroup],
    ) -> Result<Vec<GroupId>, StoreError> {
        let tx = self.conn.transaction()?;
        jobs::ensure_job(&tx, job)?;
        let mut written = written_groups(&tx, job)?;
        let ids = write_automatic(&tx, job, regrouped, &mut written)?;
        let origin = enum_text(&GroupOrigin::Manual)?;
        for (offset, group) in manual.iter().enumerate() {
            let (stored, _) = written
                .get(&group.id)
                .ok_or(StoreError::UnknownGroup(group.id))?;
            let columns = GroupColumns {
                position: regrouped.groups.len() + offset,
                origin: origin.clone(),
                review_reset: false,
                ..stored.clone()
            };
            write_group(
                &tx,
                job,
                &mut written,
                Some(group.id),
                &columns,
                &group.members,
            )?;
        }
        tx.commit()?;
        Ok(ids)
    }

    /// Every group of `job`, by position.
    pub fn groups(&self, job: JobId) -> Result<Vec<StoredGroup>, StoreError> {
        let snapshot = read_snapshot(&self.conn)?;
        groups_in(&snapshot, job)
    }

    /// Whether `group` is one of `job`'s groups.
    pub fn group_in_job(&self, job: JobId, group: GroupId) -> Result<bool, StoreError> {
        let mut statement = self
            .conn
            .prepare_cached("SELECT 1 FROM groups WHERE id = ?2 AND job_id = ?1")?;
        Ok(statement.exists(params![job, group])?)
    }

    /// Gives `group` `status`, and clears its review flag: a decision answers
    /// it (spec §5.4.1).
    pub fn set_group_status(&self, group: GroupId, status: GroupStatus) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE groups SET status = ?2, review_reset = 0 WHERE id = ?1",
            params![group, enum_text(&status)?],
        )?;
        changed_one(changed, StoreError::UnknownGroup(group))
    }

    pub fn set_group_partial(
        &self,
        group: GroupId,
        partial: Option<&Path>,
    ) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE groups SET partial_path = ?2 WHERE id = ?1",
            params![group, partial.map(paths::to_blob)],
        )?;
        changed_one(changed, StoreError::UnknownGroup(group))
    }

    pub fn set_group_output(
        &self,
        group: GroupId,
        output: Option<&Path>,
    ) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE groups SET output_path = ?2 WHERE id = ?1",
            params![group, output.map(paths::to_blob)],
        )?;
        changed_one(changed, StoreError::UnknownGroup(group))
    }

    pub fn set_group_error(&self, group: GroupId, error: Option<&str>) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE groups SET error = ?2 WHERE id = ?1",
            params![group, error],
        )?;
        changed_one(changed, StoreError::UnknownGroup(group))
    }

    /// Gives every Proposed group of `job` `status`, clearing their review
    /// flags (a decision answers them); returns how many there were.
    pub fn decide_proposed(&self, job: JobId, status: GroupStatus) -> Result<usize, StoreError> {
        Ok(self.conn.execute(
            "UPDATE groups SET status = ?3, review_reset = 0 WHERE job_id = ?1 AND status = ?2",
            params![job, enum_text(&GroupStatus::Proposed)?, enum_text(&status)?],
        )?)
    }

    /// Records how the write of `group` ended, in one transaction: its
    /// `status`, its `output` and its `error`, and its `warnings` in place of
    /// the earlier ones. Its partial is gone, so `partial_path` is cleared.
    pub fn set_group_outcome(
        &mut self,
        group: GroupId,
        status: GroupStatus,
        output: Option<&Path>,
        error: Option<&str>,
        warnings: &[(usize, Warning)],
    ) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        let changed = tx.execute(
            "UPDATE groups SET status = ?2, output_path = ?3, error = ?4, partial_path = NULL
             WHERE id = ?1",
            params![
                group,
                enum_text(&status)?,
                output.map(paths::to_blob),
                error
            ],
        )?;
        changed_one(changed, StoreError::UnknownGroup(group))?;
        write_warnings(&tx, group, warnings)?;
        tx.commit()?;
        Ok(())
    }

    /// Puts `groups` of `job` back to Confirmed, with their error and
    /// warnings cleared, in one transaction, so that the job's next write
    /// writes them again. A group that is not one of `job`'s is refused as
    /// [`StoreError::UnknownGroup`], and nothing changes.
    pub fn requeue_groups(&mut self, job: JobId, groups: &[GroupId]) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        jobs::ensure_job(&tx, job)?;
        requeue_in(&tx, job, groups)?;
        tx.commit()?;
        Ok(())
    }

    /// Replaces the warnings of `group` with `warnings`, in one transaction.
    pub fn set_group_warnings(
        &mut self,
        group: GroupId,
        warnings: &[(usize, Warning)],
    ) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        let exists = tx
            .query_row("SELECT 1 FROM groups WHERE id = ?1", [group], |_| Ok(()))
            .optional()?;
        exists.ok_or(StoreError::UnknownGroup(group))?;
        write_warnings(&tx, group, warnings)?;
        tx.commit()?;
        Ok(())
    }
}

/// The automatic part of [`Store::apply_regroup`] on `conn`, which must be
/// inside a transaction, with `written` the job's groups as stored: deletes
/// the groups in `regrouped.removed`, writes each group that reuses an id
/// and inserts each new one ([`write_group`]), numbered from 0 in
/// `regrouped`'s order. Returns their ids, in that order.
pub(super) fn write_automatic(
    conn: &Connection,
    job: JobId,
    regrouped: &Regrouped,
    written: &mut WrittenGroups,
) -> Result<Vec<GroupId>, StoreError> {
    let mut delete = conn.prepare_cached("DELETE FROM groups WHERE id = ?1 AND job_id = ?2")?;
    for &id in &regrouped.removed {
        written.remove(&id);
        changed_one(
            delete.execute(params![id, job])?,
            StoreError::UnknownGroup(id),
        )?;
    }
    regrouped
        .groups
        .iter()
        .enumerate()
        .map(|(position, group)| {
            let columns = GroupColumns::automatic(position, group)?;
            write_group(conn, job, written, group.id, &columns, &group.members)
        })
        .collect()
}

/// Stores a group of `job` on `conn`, which must be inside a transaction:
/// with `id` `None`, a new group, inserted; else the group `id`, taken out
/// of `written`, whose row is updated only when its columns differ from
/// `columns`, and whose members are replaced only when they differ from
/// `members` (in layer order). A group that is not in `written` (not the
/// job's, or written already) is [`StoreError::UnknownGroup`]. Returns the
/// group's id.
pub(super) fn write_group(
    conn: &Connection,
    job: JobId,
    written: &mut WrittenGroups,
    id: Option<GroupId>,
    columns: &GroupColumns,
    members: &[usize],
) -> Result<GroupId, StoreError> {
    let values: [&dyn ToSql; 10] = [
        &job,
        &columns.position,
        &columns.status,
        &columns.origin,
        &columns.review_reset,
        &columns.bucket,
        &columns.max_distance,
        &columns.ml_status,
        &columns.ml_score,
        &id,
    ];
    let Some(id) = id else {
        // ?10 is NULL, so SQLite picks the new row's id.
        conn.prepare_cached(
            "INSERT INTO groups (job_id, position, status, origin, review_reset,
               bucket, max_distance, ml_status, ml_score, id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?
        .execute(values)?;
        let id = conn.last_insert_rowid();
        write_members(conn, id, members)?;
        return Ok(id);
    };
    let (stored, stored_members) = written.remove(&id).ok_or(StoreError::UnknownGroup(id))?;
    if stored != *columns {
        let changed = conn
            .prepare_cached(
                "UPDATE groups SET position = ?2, status = ?3, origin = ?4,
                   review_reset = ?5, bucket = ?6, max_distance = ?7,
                   ml_status = ?8, ml_score = ?9
                 WHERE job_id = ?1 AND id = ?10",
            )?
            .execute(values)?;
        changed_one(changed, StoreError::UnknownGroup(id))?;
    }
    if stored_members != members {
        write_members(conn, id, members)?;
    }
    Ok(id)
}

/// The groups of `job` as stored, on `conn`, which must be inside a
/// transaction (see [`WrittenGroups`]).
pub(super) fn written_groups(conn: &Connection, job: JobId) -> Result<WrittenGroups, StoreError> {
    let mut members = members_in(conn, job)?;
    Ok(group_rows(conn, job)?
        .into_iter()
        .map(|row| {
            let group_members = members.remove(&row.id).unwrap_or_default();
            (row.id, (row.columns, group_members))
        })
        .collect())
}

/// The group part of [`Store::requeue_groups`] on `conn`, which must be
/// inside a transaction: puts `groups` of `job` back to Confirmed, with
/// their error and warnings cleared. A group that is not one of `job`'s is
/// [`StoreError::UnknownGroup`].
pub(super) fn requeue_in(
    conn: &Connection,
    job: JobId,
    groups: &[GroupId],
) -> Result<(), StoreError> {
    for &group in groups {
        let changed = conn.execute(
            "UPDATE groups SET status = ?3, error = NULL WHERE job_id = ?1 AND id = ?2",
            params![job, group, enum_text(&GroupStatus::Confirmed)?],
        )?;
        changed_one(changed, StoreError::UnknownGroup(group))?;
        write_warnings(conn, group, &[])?;
    }
    Ok(())
}

/// Replaces the warnings of `group` with `warnings`, on `conn`, which must
/// be inside a transaction.
fn write_warnings(
    conn: &Connection,
    group: GroupId,
    warnings: &[(usize, Warning)],
) -> Result<(), StoreError> {
    conn.execute("DELETE FROM group_warnings WHERE group_id = ?1", [group])?;
    let mut insert = conn.prepare(
        "INSERT INTO group_warnings (group_id, file_idx, warning_json) VALUES (?1, ?2, ?3)",
    )?;
    for (file, warning) in warnings {
        insert.execute(params![group, file, serde_json::to_string(warning)?])?;
    }
    Ok(())
}

/// [`Store::groups`] on `conn`, which must be inside a read snapshot: the
/// groups, their members and their warnings then come from one state.
pub(super) fn groups_in(conn: &Connection, job: JobId) -> Result<Vec<StoredGroup>, StoreError> {
    debug_assert!(
        !conn.is_autocommit(),
        "a multi-statement read outside a snapshot"
    );
    let mut members = members_in(conn, job)?;
    let mut warnings = warnings_in(conn, job)?;
    group_rows(conn, job)?
        .into_iter()
        .map(|row| {
            let id = row.id;
            stored_group(
                row,
                members.remove(&id).unwrap_or_default(),
                warnings.remove(&id).unwrap_or_default(),
            )
        })
        .collect()
}

/// Replaces the members of `group` with `members`, in layer order.
pub(super) fn write_members(
    conn: &Connection,
    group: GroupId,
    members: &[usize],
) -> Result<(), StoreError> {
    conn.prepare_cached("DELETE FROM group_members WHERE group_id = ?1")?
        .execute([group])?;
    let mut insert = conn.prepare_cached(
        "INSERT INTO group_members (group_id, file_idx, layer_index) VALUES (?1, ?2, ?3)",
    )?;
    for (layer, file) in members.iter().enumerate() {
        insert.execute(params![group, file, layer])?;
    }
    Ok(())
}

/// The rows of `job`'s groups, by position.
fn group_rows(conn: &Connection, job: JobId) -> Result<Vec<GroupRow>, StoreError> {
    let mut statement = conn.prepare_cached(
        "SELECT id, position, status, origin, review_reset, bucket, max_distance,
           ml_status, ml_score, partial_path, output_path, error
         FROM groups WHERE job_id = ?1 ORDER BY position, id",
    )?;
    let rows = statement.query_map([job], |row| {
        Ok(GroupRow {
            id: row.get(0)?,
            columns: GroupColumns {
                position: row.get(1)?,
                status: row.get(2)?,
                origin: row.get(3)?,
                review_reset: row.get(4)?,
                bucket: row.get(5)?,
                max_distance: row.get(6)?,
                ml_status: row.get(7)?,
                ml_score: row.get(8)?,
            },
            partial_path: row.get(9)?,
            output_path: row.get(10)?,
            error: row.get(11)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The members of every group of `job`, in layer order, by group: one
/// statement for them all.
fn members_in(conn: &Connection, job: JobId) -> Result<HashMap<GroupId, Vec<usize>>, StoreError> {
    let mut statement = conn.prepare_cached(
        "SELECT m.group_id, m.file_idx FROM group_members m JOIN groups g ON g.id = m.group_id
         WHERE g.job_id = ?1 ORDER BY m.group_id, m.layer_index",
    )?;
    let rows = statement.query_map([job], |row| {
        Ok((row.get::<_, GroupId>(0)?, row.get::<_, usize>(1)?))
    })?;
    let mut members: HashMap<GroupId, Vec<usize>> = HashMap::new();
    for row in rows {
        let (group, file) = row?;
        members.entry(group).or_default().push(file);
    }
    Ok(members)
}

/// The warnings of every group of `job`, each group's in the order stored,
/// by group: one statement for them all.
fn warnings_in(
    conn: &Connection,
    job: JobId,
) -> Result<HashMap<GroupId, Vec<(usize, Warning)>>, StoreError> {
    let mut statement = conn.prepare_cached(
        "SELECT w.group_id, w.file_idx, w.warning_json
         FROM group_warnings w JOIN groups g ON g.id = w.group_id
         WHERE g.job_id = ?1 ORDER BY w.group_id, w.rowid",
    )?;
    let rows = statement.query_map([job], |row| {
        Ok((
            row.get::<_, GroupId>(0)?,
            row.get::<_, usize>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut warnings: HashMap<GroupId, Vec<(usize, Warning)>> = HashMap::new();
    for row in rows {
        let (group, file, json) = row?;
        warnings
            .entry(group)
            .or_default()
            .push((file, serde_json::from_str(&json)?));
    }
    Ok(warnings)
}

fn stored_group(
    row: GroupRow,
    members: Vec<usize>,
    warnings: Vec<(usize, Warning)>,
) -> Result<StoredGroup, StoreError> {
    let GroupRow {
        id,
        columns,
        partial_path,
        output_path,
        error,
    } = row;
    Ok(StoredGroup {
        id,
        position: columns.position,
        status: enum_from_text(columns.status)?,
        origin: enum_from_text(columns.origin)?,
        review_reset: columns.review_reset,
        members,
        bucket: columns.bucket,
        max_distance: columns.max_distance,
        ml_status: enum_from_text(columns.ml_status)?,
        // An f32 widened to f64 narrows back exactly.
        ml_score: columns.ml_score.map(|score| score as f32),
        partial_path: partial_path.as_deref().map(paths::from_blob),
        output_path: output_path.as_deref().map(paths::from_blob),
        error,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::group::RegroupedGroup;
    use crate::model::JobConfig;

    fn auto(id: Option<GroupId>, members: &[usize], status: GroupStatus) -> RegroupedGroup {
        RegroupedGroup {
            id,
            members: members.to_vec(),
            bucket: "|model|lens|50.0@0".into(),
            max_distance: 3,
            status,
            review_reset: false,
            ml_status: MlStatus::None,
            ml_score: None,
        }
    }

    fn regrouped(groups: Vec<RegroupedGroup>, removed: Vec<GroupId>) -> Regrouped {
        Regrouped {
            groups,
            removed,
            ..Regrouped::default()
        }
    }

    fn summary(store: &Store, job: JobId) -> Vec<(GroupId, usize, GroupStatus, Vec<usize>)> {
        store
            .groups(job)
            .unwrap()
            .into_iter()
            .map(|group| (group.id, group.position, group.status, group.members))
            .collect()
    }

    #[test]
    fn apply_regroup_keeps_reused_ids() {
        let mut store = Store::in_memory().unwrap();
        let job = store.insert_job(&JobConfig::new("/in".into())).unwrap();
        let ids = store
            .apply_regroup(
                job,
                &regrouped(
                    vec![
                        auto(None, &[1, 0], GroupStatus::Proposed),
                        auto(None, &[2, 3], GroupStatus::Proposed),
                    ],
                    vec![],
                ),
                &[],
            )
            .unwrap();
        let [a, b] = ids[..] else { panic!("{ids:?}") };
        store.set_group_status(a, GroupStatus::Confirmed).unwrap();
        store
            .set_group_warnings(b, &[(2, Warning::IccMismatch)])
            .unwrap();

        // A raised threshold: `a` is unchanged, `b` grows into a new group.
        let mut grown = auto(None, &[2, 3, 4], GroupStatus::Proposed);
        grown.review_reset = true;
        grown.ml_status = MlStatus::Confirmed;
        grown.ml_score = Some(0.912_345_6);
        let ids = store
            .apply_regroup(
                job,
                &regrouped(
                    vec![auto(Some(a), &[1, 0], GroupStatus::Confirmed), grown],
                    vec![b],
                ),
                &[],
            )
            .unwrap();
        assert_eq!(ids[0], a);
        let c = ids[1];
        assert_ne!(c, b);
        assert_eq!(
            summary(&store, job),
            [
                (a, 0, GroupStatus::Confirmed, vec![1, 0]),
                (c, 1, GroupStatus::Proposed, vec![2, 3, 4]),
            ]
        );
        let groups = store.groups(job).unwrap();
        assert_eq!(groups[0].origin, GroupOrigin::Auto);
        assert_eq!(groups[0].bucket, "|model|lens|50.0@0");
        assert_eq!(groups[0].max_distance, 3);
        assert!(groups[1].review_reset);
        assert_eq!(groups[1].ml_status, MlStatus::Confirmed);
        assert_eq!(groups[1].ml_score, Some(0.912_345_6));
        // The removed group took its members and warnings with it.
        let orphans: i64 = store
            .conn
            .query_row(
                "SELECT (SELECT count(*) FROM group_members WHERE group_id = ?1)
                      + (SELECT count(*) FROM group_warnings WHERE group_id = ?1)",
                [b],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(orphans, 0);
    }

    #[test]
    fn apply_regroup_places_manual_groups_after_automatic_ones() {
        let mut store = Store::in_memory().unwrap();
        let job = store.insert_job(&JobConfig::new("/in".into())).unwrap();
        let mut flagged = auto(None, &[3, 4], GroupStatus::Proposed);
        flagged.review_reset = true;
        let ids = store
            .apply_regroup(
                job,
                &regrouped(
                    vec![auto(None, &[0, 1], GroupStatus::Proposed), flagged],
                    vec![],
                ),
                &[],
            )
            .unwrap();
        let [a, b] = ids[..] else { panic!("{ids:?}") };
        store.set_group_status(a, GroupStatus::Confirmed).unwrap();
        // The user moved photo 5 into `a`, which made it manual.
        let manual = [ManualGroup {
            id: a,
            members: vec![5, 0, 1],
        }];
        let ids = store
            .apply_regroup(
                job,
                &regrouped(vec![auto(Some(b), &[3, 4], GroupStatus::Proposed)], vec![]),
                &manual,
            )
            .unwrap();
        assert_eq!(ids, [b]);
        assert_eq!(
            summary(&store, job),
            [
                (b, 0, GroupStatus::Proposed, vec![3, 4]),
                (a, 1, GroupStatus::Confirmed, vec![5, 0, 1]),
            ]
        );
        let groups = store.groups(job).unwrap();
        assert_eq!(
            (groups[1].origin, groups[1].review_reset),
            (GroupOrigin::Manual, false)
        );
    }

    #[test]
    fn a_kept_review_flag_is_not_rewritten_and_a_decision_clears_it() {
        let mut store = Store::in_memory().unwrap();
        let job = store.insert_job(&JobConfig::new("/in".into())).unwrap();
        let flagged = |id| {
            let mut group = auto(id, &[0, 1], GroupStatus::Proposed);
            group.review_reset = true;
            group
        };
        let flags = |store: &Store| -> Vec<bool> {
            let groups = store.groups(job).unwrap();
            groups.iter().map(|group| group.review_reset).collect()
        };
        let ids = store
            .apply_regroup(
                job,
                &regrouped(
                    vec![
                        flagged(None),
                        auto(None, &[2, 3], GroupStatus::Proposed),
                        auto(None, &[4, 5], GroupStatus::Proposed),
                    ],
                    vec![],
                ),
                &[],
            )
            .unwrap();
        let [a, b, c] = ids[..] else {
            panic!("{ids:?}")
        };

        // A regrouping that keeps the flag leaves the row alone.
        let before = store.conn.total_changes();
        let again = regrouped(
            vec![
                flagged(Some(a)),
                auto(Some(b), &[2, 3], GroupStatus::Proposed),
                auto(Some(c), &[4, 5], GroupStatus::Proposed),
            ],
            vec![],
        );
        store.apply_regroup(job, &again, &[]).unwrap();
        assert_eq!(store.conn.total_changes(), before);
        assert_eq!(flags(&store), [true, false, false]);

        // A decision clears it, once; one on all the Proposed groups too.
        store.set_group_status(a, GroupStatus::Confirmed).unwrap();
        assert_eq!(flags(&store), [false, false, false]);
        store
            .conn
            .execute("UPDATE groups SET review_reset = 1 WHERE id = ?1", [b])
            .unwrap();
        store.decide_proposed(job, GroupStatus::Rejected).unwrap();
        assert_eq!(flags(&store), [false, false, false]);
    }

    #[test]
    fn apply_regroup_refuses_a_foreign_group_and_changes_nothing() {
        let mut store = Store::in_memory().unwrap();
        let job = store.insert_job(&JobConfig::new("/in".into())).unwrap();
        let other = store.insert_job(&JobConfig::new("/other".into())).unwrap();
        let theirs = store
            .apply_regroup(
                other,
                &regrouped(vec![auto(None, &[0, 1], GroupStatus::Proposed)], vec![]),
                &[],
            )
            .unwrap()[0];
        let attempt = store.apply_regroup(
            job,
            &regrouped(
                vec![
                    auto(None, &[5, 6], GroupStatus::Proposed),
                    auto(Some(theirs), &[0, 1], GroupStatus::Proposed),
                ],
                vec![],
            ),
            &[],
        );
        assert!(matches!(attempt, Err(StoreError::UnknownGroup(id)) if id == theirs));
        assert_eq!(store.groups(job).unwrap(), []);
        assert_eq!(store.groups(other).unwrap().len(), 1);
    }

    #[test]
    fn groups_of_a_job_keep_their_members_and_warnings_in_order() {
        let mut store = Store::in_memory().unwrap();
        let job = store.insert_job(&JobConfig::new("/in".into())).unwrap();
        let other = store.insert_job(&JobConfig::new("/other".into())).unwrap();
        // 300 groups of 2 to 7 members, each in a layer order that is not
        // ascending.
        let mut next = 0;
        let groups: Vec<RegroupedGroup> = (0..300)
            .map(|n| {
                let size = 2 + n % 6;
                let mut members: Vec<usize> = (next..next + size).collect();
                members.rotate_left(1 + n % (size - 1));
                next += size;
                auto(None, &members, GroupStatus::Proposed)
            })
            .collect();
        let ids = store
            .apply_regroup(job, &regrouped(groups.clone(), vec![]), &[])
            .unwrap();
        store
            .apply_regroup(other, &regrouped(groups[..2].to_vec(), vec![]), &[])
            .unwrap();
        // Every third group has warnings, not in the order of its members.
        let warnings_of = |n: usize, members: &[usize]| -> Vec<(usize, Warning)> {
            if !n.is_multiple_of(3) {
                return Vec::new();
            }
            vec![
                (members[1], Warning::IccMismatch),
                (
                    members[0],
                    Warning::LayerUnreadable {
                        reason: format!("gone {n}"),
                    },
                ),
                (members[1], Warning::NoCaptureTime),
            ]
        };
        for (n, (&id, group)) in ids.iter().zip(&groups).enumerate() {
            store
                .set_group_warnings(id, &warnings_of(n, &group.members))
                .unwrap();
        }

        let stored = store.groups(job).unwrap();
        assert_eq!(stored.len(), groups.len());
        for (n, ((stored, &id), group)) in stored.iter().zip(&ids).zip(&groups).enumerate() {
            assert_eq!(
                (
                    stored.id,
                    stored.position,
                    &stored.members,
                    &stored.warnings
                ),
                (id, n, &group.members, &warnings_of(n, &group.members)),
                "group {n}"
            );
        }
        let theirs = store.groups(other).unwrap();
        assert_eq!(
            theirs
                .iter()
                .map(|group| (&group.members, group.warnings.len()))
                .collect::<Vec<_>>(),
            [(&groups[0].members, 0), (&groups[1].members, 0)]
        );
    }

    #[test]
    fn group_write_state_round_trips() {
        let mut store = Store::in_memory().unwrap();
        let job = store.insert_job(&JobConfig::new("/in".into())).unwrap();
        let group = store
            .apply_regroup(
                job,
                &regrouped(vec![auto(None, &[0, 1, 2], GroupStatus::Confirmed)], vec![]),
                &[],
            )
            .unwrap()[0];
        let partial = Path::new("/in/stacked/a_3.psd.partial");
        let output = Path::new("/in/stacked/a_2.psd");
        let warnings = [
            (
                2,
                Warning::LayerUnreadable {
                    reason: "cut off".into(),
                },
            ),
            (1, Warning::IccMismatch),
        ];
        store.set_group_partial(group, Some(partial)).unwrap();
        let stored = &store.groups(job).unwrap()[0];
        assert_eq!(stored.partial_path.as_deref(), Some(partial));

        store.set_group_partial(group, None).unwrap();
        store.set_group_output(group, Some(output)).unwrap();
        store.set_group_status(group, GroupStatus::Written).unwrap();
        store.set_group_warnings(group, &warnings).unwrap();
        store.set_group_error(group, Some("no space left")).unwrap();
        let stored = &store.groups(job).unwrap()[0];
        assert_eq!(stored.partial_path, None);
        assert_eq!(stored.output_path.as_deref(), Some(output));
        assert_eq!(stored.status, GroupStatus::Written);
        assert_eq!(stored.warnings, warnings);
        assert_eq!(stored.error.as_deref(), Some("no space left"));

        // Warnings are replaced, not added.
        store.set_group_warnings(group, &[]).unwrap();
        assert_eq!(store.groups(job).unwrap()[0].warnings, []);
        assert!(matches!(
            store.set_group_status(group + 1, GroupStatus::Failed),
            Err(StoreError::UnknownGroup(_))
        ));
    }

    #[test]
    fn a_write_outcome_replaces_the_write_state() {
        let mut store = Store::in_memory().unwrap();
        let job = store.insert_job(&JobConfig::new("/in".into())).unwrap();
        let group = store
            .apply_regroup(
                job,
                &regrouped(vec![auto(None, &[0, 1, 2], GroupStatus::Confirmed)], vec![]),
                &[],
            )
            .unwrap()[0];
        let partial = Path::new("/in/stacked/a_3.psd.partial");
        let output = Path::new("/in/stacked/a_2.psd");
        store.set_group_partial(group, Some(partial)).unwrap();
        store
            .set_group_warnings(group, &[(0, Warning::IccMismatch)])
            .unwrap();
        let warnings = [(2, Warning::IccMismatch)];
        store
            .set_group_outcome(group, GroupStatus::Written, Some(output), None, &warnings)
            .unwrap();
        let stored = &store.groups(job).unwrap()[0];
        assert_eq!(
            (
                stored.status,
                stored.partial_path.as_deref(),
                stored.output_path.as_deref(),
                stored.error.as_deref(),
                stored.warnings.as_slice()
            ),
            (
                GroupStatus::Written,
                None,
                Some(output),
                None,
                &warnings[..]
            )
        );

        store
            .set_group_outcome(group, GroupStatus::Failed, None, Some("disk full"), &[])
            .unwrap();
        let stored = &store.groups(job).unwrap()[0];
        assert_eq!(
            (
                stored.status,
                stored.output_path.as_deref(),
                stored.error.as_deref(),
                stored.warnings.len()
            ),
            (GroupStatus::Failed, None, Some("disk full"), 0)
        );
        assert!(matches!(
            store.set_group_outcome(group + 1, GroupStatus::Written, None, None, &[]),
            Err(StoreError::UnknownGroup(_))
        ));
    }

    #[test]
    fn requeued_groups_are_confirmed_without_their_write_state() {
        let mut store = Store::in_memory().unwrap();
        let job = store.insert_job(&JobConfig::new("/in".into())).unwrap();
        let other = store.insert_job(&JobConfig::new("/other".into())).unwrap();
        let groups = vec![
            auto(None, &[0, 1], GroupStatus::Confirmed),
            auto(None, &[2, 3], GroupStatus::Confirmed),
        ];
        let ids = store
            .apply_regroup(job, &regrouped(groups, vec![]), &[])
            .unwrap();
        let [failed, skipped] = ids[..] else {
            panic!("{ids:?}")
        };
        let theirs = store
            .apply_regroup(
                other,
                &regrouped(vec![auto(None, &[0, 1], GroupStatus::Confirmed)], vec![]),
                &[],
            )
            .unwrap()[0];
        let unreadable = [(
            2,
            Warning::LayerUnreadable {
                reason: "gone".into(),
            },
        )];
        store
            .set_group_outcome(failed, GroupStatus::Failed, None, Some("disk full"), &[])
            .unwrap();
        store
            .set_group_outcome(
                skipped,
                GroupStatus::Skipped,
                None,
                Some("1 of 2 layers"),
                &unreadable,
            )
            .unwrap();

        // A foreign group is refused, and the job's groups are left as they
        // were.
        assert!(matches!(
            store.requeue_groups(job, &[failed, theirs]),
            Err(StoreError::UnknownGroup(id)) if id == theirs
        ));
        assert_eq!(store.groups(job).unwrap()[0].status, GroupStatus::Failed);

        store.requeue_groups(job, &[failed, skipped]).unwrap();
        for group in store.groups(job).unwrap() {
            assert_eq!(
                (
                    group.status,
                    group.error.as_deref(),
                    group.warnings.len(),
                    group.output_path.as_deref()
                ),
                (GroupStatus::Confirmed, None, 0, None),
                "{group:#?}"
            );
        }
        assert!(matches!(
            store.requeue_groups(other + 1, &[]),
            Err(StoreError::UnknownJob(_))
        ));
    }

    #[test]
    fn bulk_decisions_change_only_proposed_groups_of_the_job() {
        let mut store = Store::in_memory().unwrap();
        let job = store.insert_job(&JobConfig::new("/in".into())).unwrap();
        let other = store.insert_job(&JobConfig::new("/other".into())).unwrap();
        let groups = vec![
            auto(None, &[0, 1], GroupStatus::Proposed),
            auto(None, &[2, 3], GroupStatus::Rejected),
            auto(None, &[4, 5], GroupStatus::Proposed),
        ];
        let ids = store
            .apply_regroup(job, &regrouped(groups.clone(), vec![]), &[])
            .unwrap();
        store
            .apply_regroup(other, &regrouped(groups, vec![]), &[])
            .unwrap();
        assert_eq!(
            store.decide_proposed(job, GroupStatus::Confirmed).unwrap(),
            2
        );
        let statuses = |job| -> Vec<GroupStatus> {
            store
                .groups(job)
                .unwrap()
                .iter()
                .map(|g| g.status)
                .collect()
        };
        assert_eq!(
            statuses(job),
            [
                GroupStatus::Confirmed,
                GroupStatus::Rejected,
                GroupStatus::Confirmed
            ]
        );
        assert_eq!(statuses(other)[0], GroupStatus::Proposed);
        assert_eq!(
            store.decide_proposed(job, GroupStatus::Rejected).unwrap(),
            0
        );
        assert_eq!(ids.len(), 3);
    }
}
