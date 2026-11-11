//! A job's analysis: its files (with their EXIF, hashes, pairing, errors and
//! warnings) and the entries its scan skipped; and the review edits that
//! touch single files (pins and the active file of a pair).

use std::collections::HashSet;

use rusqlite::{Connection, params};

use super::{
    JobId, Store, StoreError, changed_one, enum_from_text, enum_text, groups, jobs, paths,
    read_snapshot, vectors,
};
use crate::analyze::{Analysis, FileRecord, ProposedGroup};
use crate::discover::DiscoverSkip;
use crate::exif::ExifData;
use crate::hash::HashPair;
use crate::model::Warning;

/// What [`Store::swap_active`] stores: the passive file that becomes the
/// active one of its pair, as examining it as an active file found it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Swap<'a> {
    /// The passive file that becomes active.
    pub promoted: usize,
    /// Its EXIF, completed by its partners'.
    pub exif: Option<&'a ExifData>,
    pub hashes: HashPair,
    pub normalised: HashPair,
    pub warnings: &'a [Warning],
    /// The demoted file's own EXIF, which it keeps as a passive file.
    pub demoted_exif: Option<&'a ExifData>,
}

/// One `files` row as SQLite gives it.
struct FileRow {
    idx: usize,
    path: Vec<u8>,
    kind: String,
    size: u64,
    fingerprint: String,
    mtime_ms: i64,
    exif_json: Option<String>,
    hashes: [Option<i64>; 4],
    paired_with: Option<usize>,
    error: Option<String>,
    warnings_json: String,
}

impl Store {
    /// Stores a scan's result for `job` in one transaction: its files, with
    /// their warnings, and its skips. It replaces everything a previous scan
    /// of the job left: the files (with their embeddings and pins), the
    /// skips and the groups, whose members were ids of those files. The
    /// analysis' own groups are not stored: [`Store::apply_regroup`] stores
    /// a grouping.
    pub fn save_analysis(&mut self, job: JobId, analysis: &Analysis) -> Result<(), StoreError> {
        let mut warnings: Vec<Vec<&Warning>> = vec![Vec::new(); analysis.files.len()];
        for (id, warning) in &analysis.warnings {
            warnings
                .get_mut(*id)
                .ok_or_else(|| {
                    StoreError::Invalid(format!(
                        "a warning for file {id} of {}",
                        analysis.files.len()
                    ))
                })?
                .push(warning);
        }
        let tx = self.conn.transaction()?;
        jobs::ensure_job(&tx, job)?;
        vectors::delete_job_vectors(&tx, job)?;
        for table in ["groups", "files", "job_skips"] {
            tx.execute(&format!("DELETE FROM {table} WHERE job_id = ?1"), [job])?;
        }
        {
            let mut insert = tx.prepare(
                "INSERT INTO files (job_id, idx, path, kind, size, fingerprint, mtime_ms,
                   exif_json, phash, dhash, nphash, ndhash, paired_with, error, warnings_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            )?;
            for (idx, (file, warnings)) in analysis.files.iter().zip(&warnings).enumerate() {
                let exif_json = file.exif.as_ref().map(serde_json::to_string).transpose()?;
                let [phash, dhash] = hash_columns(file.hashes);
                let [nphash, ndhash] = hash_columns(file.normalised);
                insert.execute(params![
                    job,
                    idx,
                    paths::to_blob(&file.path),
                    enum_text(&file.kind)?,
                    file.size,
                    file.fingerprint,
                    file.mtime_ms,
                    exif_json,
                    phash,
                    dhash,
                    nphash,
                    ndhash,
                    file.paired_with,
                    file.error,
                    serde_json::to_string(warnings)?,
                ])?;
            }
            let mut insert =
                tx.prepare("INSERT INTO job_skips (job_id, path, reason) VALUES (?1, ?2, ?3)")?;
            for skip in &analysis.skipped {
                insert.execute(params![job, paths::to_blob(&skip.path), skip.reason])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The analysis of `job` as stored: its files, their warnings (by file
    /// id, each file's in their order), its skips, its groups by position as
    /// [`ProposedGroup`]s, and as unstacked the hashed files in no group.
    pub fn load_analysis(&self, job: JobId) -> Result<Analysis, StoreError> {
        let snapshot = read_snapshot(&self.conn)?;
        analysis_in(&snapshot, job)
    }

    /// The files of `job`'s analysis, by id, as [`Store::load_analysis`]
    /// gives them, without the rest of the analysis.
    pub fn files(&self, job: JobId) -> Result<Vec<FileRecord>, StoreError> {
        let snapshot = read_snapshot(&self.conn)?;
        jobs::ensure_job(&snapshot, job)?;
        placed_file_rows(&snapshot, job)?
            .into_iter()
            .map(file_record)
            .collect()
    }

    /// The hashed files of `job` in no group (the tray), ascending: the
    /// `unstacked` of [`Store::load_analysis`], found by SQLite alone.
    pub fn unstacked(&self, job: JobId) -> Result<Vec<usize>, StoreError> {
        let snapshot = read_snapshot(&self.conn)?;
        jobs::ensure_job(&snapshot, job)?;
        unstacked_in(&snapshot, job)
    }

    /// Pins file `idx` of `job` to the tray, or unpins it.
    pub fn set_pinned(&self, job: JobId, idx: usize, pinned: bool) -> Result<(), StoreError> {
        write_pinned(&self.conn, job, idx, pinned)
    }

    /// The pinned files of `job`, ascending.
    pub fn pinned(&self, job: JobId) -> Result<Vec<usize>, StoreError> {
        let mut statement = self
            .conn
            .prepare("SELECT idx FROM files WHERE job_id = ?1 AND pinned = 1 ORDER BY idx")?;
        let ids = statement.query_map([job], |row| row.get(0))?;
        Ok(ids.collect::<Result<_, _>>()?)
    }

    /// The row id (`files.id`, the embeddings' key) of each file of `job`,
    /// by file id.
    pub fn file_row_ids(&self, job: JobId) -> Result<Vec<i64>, StoreError> {
        let mut statement = self
            .conn
            .prepare("SELECT id FROM files WHERE job_id = ?1 ORDER BY idx")?;
        let ids = statement.query_map([job], |row| row.get(0))?;
        Ok(ids.collect::<Result<_, _>>()?)
    }

    /// Makes `swap.promoted`, a passive file, the active file of its pair, in
    /// one transaction, as the analysis does when it promotes a partner over
    /// its RAW (spec §5.1):
    ///
    /// - the promoted file gets `swap`'s EXIF, hashes and warnings, no error
    ///   and no partner;
    /// - its old active file becomes its passive partner, with its own EXIF
    ///   and no hashes, error or warnings;
    /// - the old active file's other partners pair with the promoted file.
    pub fn swap_active(&mut self, job: JobId, swap: &Swap) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        write_swap(&tx, job, swap)?;
        tx.commit()?;
        Ok(())
    }
}

/// Pins file `idx` of `job`, or unpins it (see [`Store::set_pinned`]).
pub(super) fn write_pinned(
    conn: &Connection,
    job: JobId,
    idx: usize,
    pinned: bool,
) -> Result<(), StoreError> {
    let changed = conn
        .prepare_cached("UPDATE files SET pinned = ?3 WHERE job_id = ?1 AND idx = ?2")?
        .execute(params![job, idx, pinned])?;
    changed_one(changed, StoreError::UnknownFile { job, idx })
}

/// [`Store::swap_active`] on `conn`, which must be inside a transaction.
pub(super) fn write_swap(conn: &Connection, job: JobId, swap: &Swap) -> Result<(), StoreError> {
    let promoted = swap.promoted;
    let demoted: Option<usize> = conn
        .query_row(
            "SELECT paired_with FROM files WHERE job_id = ?1 AND idx = ?2",
            params![job, promoted],
            |row| row.get(0),
        )
        .map_err(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => StoreError::UnknownFile { job, idx: promoted },
            other => other.into(),
        })?;
    let demoted = demoted.ok_or_else(|| {
        StoreError::Invalid(format!("file {promoted} of job {job} is already active"))
    })?;
    let exif_json = |exif: Option<&ExifData>| exif.map(serde_json::to_string).transpose();
    conn.execute(
        "UPDATE files SET paired_with = ?3 WHERE job_id = ?1 AND paired_with = ?2",
        params![job, demoted, promoted],
    )?;
    let [phash, dhash] = hash_columns(Some(swap.hashes));
    let [nphash, ndhash] = hash_columns(Some(swap.normalised));
    conn.execute(
        "UPDATE files SET paired_with = NULL, exif_json = ?3, phash = ?4, dhash = ?5,
           nphash = ?6, ndhash = ?7, error = NULL, warnings_json = ?8
         WHERE job_id = ?1 AND idx = ?2",
        params![
            job,
            promoted,
            exif_json(swap.exif)?,
            phash,
            dhash,
            nphash,
            ndhash,
            serde_json::to_string(swap.warnings)?
        ],
    )?;
    let changed = conn.execute(
        "UPDATE files SET paired_with = ?3, exif_json = ?4, phash = NULL, dhash = NULL,
           nphash = NULL, ndhash = NULL, error = NULL, warnings_json = '[]'
         WHERE job_id = ?1 AND idx = ?2",
        params![job, demoted, promoted, exif_json(swap.demoted_exif)?],
    )?;
    changed_one(changed, StoreError::UnknownFile { job, idx: demoted })
}

/// The tray of `job` ([`Store::unstacked`]) on `conn`, by one query, without
/// the check that the job exists.
pub(super) fn unstacked_in(conn: &Connection, job: JobId) -> Result<Vec<usize>, StoreError> {
    let mut statement = conn.prepare_cached(
        "SELECT idx FROM files
         WHERE job_id = ?1 AND nphash IS NOT NULL AND idx NOT IN (
           SELECT m.file_idx FROM group_members m JOIN groups g ON g.id = m.group_id
           WHERE g.job_id = ?1)
         ORDER BY idx",
    )?;
    let ids = statement.query_map([job], |row| row.get(0))?;
    Ok(ids.collect::<Result<_, _>>()?)
}

/// [`Store::load_analysis`] on `conn`, which must be inside a read
/// snapshot: its several SELECTs then see one state of the database.
pub(super) fn analysis_in(conn: &Connection, job: JobId) -> Result<Analysis, StoreError> {
    debug_assert!(
        !conn.is_autocommit(),
        "a multi-statement read outside a snapshot"
    );
    jobs::ensure_job(conn, job)?;
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    for row in placed_file_rows(conn, job)? {
        let file_warnings: Vec<Warning> = serde_json::from_str(&row.warnings_json)?;
        warnings.extend(file_warnings.into_iter().map(|warning| (row.idx, warning)));
        files.push(file_record(row)?);
    }
    let skipped = {
        let mut statement =
            conn.prepare("SELECT path, reason FROM job_skips WHERE job_id = ?1 ORDER BY rowid")?;
        let rows = statement.query_map([job], |row| {
            Ok(DiscoverSkip {
                path: paths::from_blob(&row.get::<_, Vec<u8>>(0)?),
                reason: row.get(1)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let groups: Vec<ProposedGroup> = groups::groups_in(conn, job)?
        .into_iter()
        .map(|group| ProposedGroup {
            members: group.members,
            bucket: group.bucket,
            max_distance: group.max_distance,
        })
        .collect();
    let grouped: HashSet<usize> = groups
        .iter()
        .flat_map(|group| group.members.iter().copied())
        .collect();
    let unstacked = (0..files.len())
        .filter(|id| files[*id].normalised.is_some() && !grouped.contains(id))
        .collect();
    Ok(Analysis {
        files,
        groups,
        unstacked,
        warnings,
        skipped,
    })
}

/// A pair's two hashes, each bit-cast from u64 to i64 (SQLite integers are
/// signed).
fn hash_columns(pair: Option<HashPair>) -> [Option<i64>; 2] {
    match pair {
        Some(pair) => [
            Some(pair.phash.cast_signed()),
            Some(pair.dhash.cast_signed()),
        ],
        None => [None, None],
    }
}

/// The pair stored as `[phash, dhash]` columns; both or neither are set.
fn hash_pair(columns: [Option<i64>; 2]) -> Result<Option<HashPair>, StoreError> {
    match columns {
        [Some(phash), Some(dhash)] => Ok(Some(HashPair {
            phash: phash.cast_unsigned(),
            dhash: dhash.cast_unsigned(),
        })),
        [None, None] => Ok(None),
        _ => Err(StoreError::Corrupt(
            "a file with one hash of a pair".to_string(),
        )),
    }
}

/// The rows of `job`'s files, by id, each checked to be at its id: the ids
/// run from 0 with no gap.
fn placed_file_rows(conn: &Connection, job: JobId) -> Result<Vec<FileRow>, StoreError> {
    let rows = file_rows(conn, job)?;
    if let Some((place, row)) = rows
        .iter()
        .enumerate()
        .find(|(place, row)| row.idx != *place)
    {
        return Err(StoreError::Corrupt(format!(
            "job {job} has file {} where file {place} belongs",
            row.idx
        )));
    }
    Ok(rows)
}

fn file_rows(conn: &Connection, job: JobId) -> Result<Vec<FileRow>, StoreError> {
    let mut statement = conn.prepare_cached(
        "SELECT idx, path, kind, size, fingerprint, mtime_ms, exif_json,
           phash, dhash, nphash, ndhash, paired_with, error, warnings_json
         FROM files WHERE job_id = ?1 ORDER BY idx",
    )?;
    let rows = statement.query_map([job], |row| {
        Ok(FileRow {
            idx: row.get(0)?,
            path: row.get(1)?,
            kind: row.get(2)?,
            size: row.get(3)?,
            fingerprint: row.get(4)?,
            mtime_ms: row.get(5)?,
            exif_json: row.get(6)?,
            hashes: [row.get(7)?, row.get(8)?, row.get(9)?, row.get(10)?],
            paired_with: row.get(11)?,
            error: row.get(12)?,
            warnings_json: row.get(13)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn file_record(row: FileRow) -> Result<FileRecord, StoreError> {
    let [phash, dhash, nphash, ndhash] = row.hashes;
    Ok(FileRecord {
        path: paths::from_blob(&row.path),
        kind: enum_from_text(row.kind)?,
        size: row.size,
        mtime_ms: row.mtime_ms,
        fingerprint: row.fingerprint,
        exif: row
            .exif_json
            .map(|json| serde_json::from_str(&json))
            .transpose()?,
        hashes: hash_pair([phash, dhash])?,
        normalised: hash_pair([nphash, ndhash])?,
        paired_with: row.paired_with,
        error: row.error,
    })
}

/// An analysis with one of everything a file can carry:
///
/// - 0: a RAW demoted to passive (paired with 1), with its own EXIF and a
///   time before 1970;
/// - 1: its promoted JPEG, with a bracket, hashes past `i64::MAX` and a
///   warning;
/// - 2: a JPEG without EXIF, with two warnings;
/// - 3: a PNG that could not be decoded;
/// - 4: a HEIC that is unstacked;
///
/// and the group `[2, 1]` and two skips.
#[cfg(test)]
pub(super) fn sample_analysis() -> Analysis {
    use std::path::PathBuf;

    use crate::exif::{BracketInfo, BracketMode};
    use crate::model::FileKind;

    let record = |name: &str, kind: FileKind| FileRecord {
        path: PathBuf::from("/in").join(name),
        kind,
        size: 1_000 + name.len() as u64,
        mtime_ms: 1_762_600_000_000,
        fingerprint: format!("fingerprint of {name}"),
        exif: None,
        hashes: None,
        normalised: None,
        paired_with: None,
        error: None,
    };
    let own_exif = ExifData {
        make: Some("Canon".into()),
        model: Some("Canon EOS 2000D".into()),
        orientation: 6,
        ..ExifData::default()
    };
    let full_exif = ExifData {
        focal_length_mm: Some(18.0),
        capture_time: chrono::NaiveDate::from_ymd_opt(2026, 5, 1)
            .and_then(|day| day.and_hms_milli_opt(10, 20, 30, 250)),
        utc_offset_minutes: Some(120),
        exposure_time_s: Some(1.0 / 250.0),
        exposure_bias_ev: Some(-0.7),
        bracket: Some(BracketInfo {
            mode: BracketMode::Exposure,
            shot_number: Some(2),
            value_ev: Some(-2.0),
        }),
        ..own_exif.clone()
    };
    let mut files = vec![
        FileRecord {
            exif: Some(own_exif),
            paired_with: Some(1),
            mtime_ms: -1_500,
            ..record("a.cr2", FileKind::Raw)
        },
        FileRecord {
            exif: Some(full_exif),
            hashes: Some(HashPair {
                phash: u64::MAX,
                dhash: 1 << 63,
            }),
            normalised: Some(HashPair {
                phash: 0x0123_4567_89ab_cdef,
                dhash: 7,
            }),
            ..record("a.jpg", FileKind::Jpeg)
        },
        record("b.jpg", FileKind::Jpeg),
        FileRecord {
            error: Some("corrupt image: cut off".into()),
            ..record("c.png", FileKind::Png)
        },
        record("d.heic", FileKind::Heic),
    ];
    for (id, seed) in [(2, 3u64), (4, 9)] {
        files[id].hashes = Some(HashPair {
            phash: seed,
            dhash: seed << 32,
        });
        files[id].normalised = Some(HashPair {
            phash: seed + 1,
            dhash: (seed + 1) << 32,
        });
    }
    Analysis {
        files,
        groups: vec![ProposedGroup {
            members: vec![2, 1],
            bucket: "|Canon EOS 2000D||18.0@0".into(),
            max_distance: 7,
        }],
        unstacked: vec![4],
        warnings: vec![
            (
                1,
                Warning::PromotedOverRaw {
                    reason: "unsupported image: unknown camera".into(),
                },
            ),
            (2, Warning::NoCaptureTime),
            (
                2,
                Warning::ExifUnreadable {
                    reason: "not a TIFF block".into(),
                },
            ),
        ],
        skipped: vec![
            DiscoverSkip {
                path: "/in/locked".into(),
                reason: "permission denied (os error 13)".into(),
            },
            DiscoverSkip {
                path: "/in/gone.jpg".into(),
                reason: "no such file (os error 2)".into(),
            },
        ],
    }
}

/// The analysis' groups as a regrouping of a job that had none.
#[cfg(test)]
pub(super) fn regrouped_of(analysis: &Analysis) -> crate::group::Regrouped {
    use crate::group::{Regrouped, RegroupedGroup};
    use crate::model::{GroupStatus, MlStatus};

    Regrouped {
        groups: analysis
            .groups
            .iter()
            .map(|group| RegroupedGroup {
                id: None,
                members: group.members.clone(),
                bucket: group.bucket.clone(),
                max_distance: group.max_distance,
                status: GroupStatus::Proposed,
                review_reset: false,
                ml_status: MlStatus::None,
                ml_score: None,
            })
            .collect(),
        unstacked: analysis.unstacked.clone(),
        ..Regrouped::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::JobConfig;

    fn saved(store: &mut Store, analysis: &Analysis) -> JobId {
        let job = store.insert_job(&JobConfig::new("/in".into())).unwrap();
        store.save_analysis(job, analysis).unwrap();
        store
            .apply_regroup(job, &regrouped_of(analysis), &[])
            .unwrap();
        job
    }

    #[test]
    fn analysis_round_trips() {
        let mut store = Store::in_memory().unwrap();
        let analysis = sample_analysis();
        let job = saved(&mut store, &analysis);
        assert_eq!(store.load_analysis(job).unwrap(), analysis);
    }

    /// A reader on its own connection, as the UI's is, while the writer
    /// commits a rescan between two reads of one snapshot.
    #[test]
    fn a_load_reads_one_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photostacker.db");
        let mut writer = Store::open(&path).unwrap();
        let analysis = sample_analysis();
        let job = saved(&mut writer, &analysis);
        let reader = Store::open_read_only(&path).unwrap();

        let snapshot = read_snapshot(&reader.conn).unwrap();
        assert_eq!(analysis_in(&snapshot, job).unwrap(), analysis);
        // The rescan drops the files the group [2, 1] points at.
        let mut rescan = sample_analysis();
        rescan.files.truncate(1);
        rescan.warnings.clear();
        rescan.skipped.clear();
        writer.save_analysis(job, &rescan).unwrap();
        assert_eq!(groups::groups_in(&snapshot, job).unwrap().len(), 1);
        assert_eq!(analysis_in(&snapshot, job).unwrap(), analysis);
        drop(snapshot);

        let fresh = reader.load_analysis(job).unwrap();
        assert_eq!((fresh.files, fresh.groups), (rescan.files, vec![]));
    }

    /// The reads that take several statements check that they run inside a
    /// snapshot; the public calls open one.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "a multi-statement read outside a snapshot")]
    fn a_read_outside_a_snapshot_is_caught() {
        let mut store = Store::in_memory().unwrap();
        let job = saved(&mut store, &sample_analysis());
        let _ = analysis_in(&store.conn, job);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_round_trip_through_the_analysis() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let mut store = Store::in_memory().unwrap();
        let mut analysis = sample_analysis();
        analysis.files[4].path = OsStr::from_bytes(b"/in/d\xff.heic").into();
        analysis.skipped[0].path = OsStr::from_bytes(b"/in/\xfe").into();
        let job = saved(&mut store, &analysis);
        assert_eq!(store.load_analysis(job).unwrap(), analysis);
    }

    #[test]
    fn saving_again_replaces_the_previous_scan() {
        let mut store = Store::in_memory().unwrap();
        let analysis = sample_analysis();
        let job = saved(&mut store, &analysis);
        store.set_pinned(job, 4, true).unwrap();
        let old_ids = store.file_row_ids(job).unwrap();
        store
            .put_embeddings("dinov2-small", &[(old_ids[4], &[0.5f32; 384][..])])
            .unwrap();

        let mut rescan = sample_analysis();
        rescan.files.truncate(4);
        rescan.warnings.clear();
        rescan.skipped.clear();
        store.save_analysis(job, &rescan).unwrap();
        let loaded = store.load_analysis(job).unwrap();
        assert_eq!(loaded.files, rescan.files);
        assert_eq!(loaded.warnings, []);
        assert_eq!(loaded.skipped, []);
        // The old groups and pins named the old files.
        assert_eq!(loaded.groups, []);
        assert_eq!(loaded.unstacked, [1, 2]);
        assert_eq!(store.pinned(job).unwrap(), Vec::<usize>::new());
        assert_eq!(
            store.embeddings("dinov2-small", &old_ids[4..]).unwrap(),
            [None]
        );
    }

    #[test]
    fn a_warning_for_a_missing_file_is_refused() {
        let mut store = Store::in_memory().unwrap();
        let job = store.insert_job(&JobConfig::new("/in".into())).unwrap();
        let mut analysis = sample_analysis();
        analysis.warnings.push((5, Warning::IccMismatch));
        assert!(matches!(
            store.save_analysis(job, &analysis),
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(
            store.save_analysis(job + 1, &sample_analysis()),
            Err(StoreError::UnknownJob(_))
        ));
    }

    /// The sample has a passive file (0), grouped files (1 and 2), a failed
    /// file (3) and a hashed file in no group (4).
    #[test]
    fn unstacked_is_what_load_analysis_gives() {
        let mut store = Store::in_memory().unwrap();
        let analysis = sample_analysis();
        let grouped = saved(&mut store, &analysis);
        store.set_pinned(grouped, 4, true).unwrap();
        store.set_pinned(grouped, 3, true).unwrap();
        // Another job of the same files, grouped otherwise.
        let mut regrouped = regrouped_of(&analysis);
        regrouped.groups[0].members = vec![4, 2];
        let other = store.insert_job(&JobConfig::new("/in".into())).unwrap();
        store.save_analysis(other, &analysis).unwrap();
        store.apply_regroup(other, &regrouped, &[]).unwrap();
        // And one with no group.
        let ungrouped = store.insert_job(&JobConfig::new("/in".into())).unwrap();
        store.save_analysis(ungrouped, &analysis).unwrap();

        for (job, expected) in [(grouped, &[4][..]), (other, &[1]), (ungrouped, &[1, 2, 4])] {
            let unstacked = store.unstacked(job).unwrap();
            assert_eq!(unstacked, store.load_analysis(job).unwrap().unstacked);
            assert_eq!(unstacked, expected, "job {job}");
        }
        assert!(matches!(
            store.unstacked(ungrouped + 1),
            Err(StoreError::UnknownJob(_))
        ));
    }

    #[test]
    fn pins_are_set_and_cleared() {
        let mut store = Store::in_memory().unwrap();
        let job = saved(&mut store, &sample_analysis());
        store.set_pinned(job, 4, true).unwrap();
        store.set_pinned(job, 2, true).unwrap();
        assert_eq!(store.pinned(job).unwrap(), [2, 4]);
        store.set_pinned(job, 4, false).unwrap();
        assert_eq!(store.pinned(job).unwrap(), [2]);
        assert!(matches!(
            store.set_pinned(job, 9, true),
            Err(StoreError::UnknownFile { idx: 9, .. })
        ));
    }

    #[test]
    fn swap_active_rewires_the_pair() {
        let mut store = Store::in_memory().unwrap();
        let mut analysis = sample_analysis();
        // The RAW 0 is active, with the JPEG 1 and a second partner, 3.
        analysis.files[0].paired_with = None;
        analysis.files[0].hashes = analysis.files[1].hashes.take();
        analysis.files[0].normalised = analysis.files[1].normalised.take();
        analysis.files[1].paired_with = Some(0);
        analysis.files[3].paired_with = Some(0);
        analysis.files[3].error = None;
        analysis.groups[0].members = vec![2, 0];
        analysis.warnings = vec![(0, Warning::NoCaptureTime)];
        let job = saved(&mut store, &analysis);

        let exif = ExifData {
            model: Some("swapped".into()),
            ..ExifData::default()
        };
        let own = ExifData {
            model: Some("own".into()),
            ..ExifData::default()
        };
        let hashes = HashPair { phash: 1, dhash: 2 };
        let normalised = HashPair { phash: 3, dhash: 4 };
        let warnings = [Warning::IccMismatch];
        store
            .swap_active(
                job,
                &Swap {
                    promoted: 1,
                    exif: Some(&exif),
                    hashes,
                    normalised,
                    warnings: &warnings,
                    demoted_exif: Some(&own),
                },
            )
            .unwrap();
        let loaded = store.load_analysis(job).unwrap();
        let [raw, jpeg, _, partner, _] = &loaded.files[..] else {
            panic!("{loaded:?}")
        };
        assert_eq!(
            (
                jpeg.paired_with,
                jpeg.exif.as_ref(),
                jpeg.hashes,
                jpeg.normalised
            ),
            (None, Some(&exif), Some(hashes), Some(normalised))
        );
        assert_eq!(
            (
                raw.paired_with,
                raw.exif.as_ref(),
                raw.hashes,
                raw.normalised
            ),
            (Some(1), Some(&own), None, None)
        );
        assert_eq!(partner.paired_with, Some(1));
        assert_eq!(loaded.warnings, [(1, Warning::IccMismatch)]);

        // The JPEG is active now: it has nothing to swap with.
        let again = Swap {
            promoted: 1,
            exif: None,
            hashes,
            normalised,
            warnings: &[],
            demoted_exif: None,
        };
        assert!(matches!(
            store.swap_active(job, &again),
            Err(StoreError::Invalid(_))
        ));
    }
}
