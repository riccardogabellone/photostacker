//! The store's schema as numbered migrations, and the sqlite-vec extension
//! its embedding tables need.
//!
//! A migration is never edited once released: a change is a new migration
//! appended to [`MIGRATIONS`].

use std::ffi::{c_char, c_int};
use std::sync::Once;

use rusqlite::ffi;
use rusqlite_migration::{M, Migrations};

/// Migration 1, the first schema (spec §7.3).
///
/// - Paths are BLOBs ([`super::paths`]).
/// - `files.idx` is the file's id in its job's analysis, and every
///   `file_idx` and `paired_with` is such an id. `files.id` is the rowid of
///   the file's embeddings.
/// - The hashes are the u64 values bit-cast to i64.
/// - The `*_json` columns are serde_json text, which only Rust reads; no
///   SQLite JSON function ever touches them.
/// - `exams.format` is the meaning of `result_json`: a row of another format
///   is a cache miss.
/// - The ids of jobs, files and groups are AUTOINCREMENT, so a deleted row's
///   id is never given to a new row: an id the UI still holds, or a
///   vector whose file is gone, never names another job, group or photo.
/// - A vec0 table per model: `embeddings_` + the model id with `-` as `_`.
const MIGRATION_1: &str = "
CREATE TABLE jobs (
  id INTEGER PRIMARY KEY AUTOINCREMENT, created_at TEXT NOT NULL, status TEXT NOT NULL,
  scan_done INTEGER NOT NULL DEFAULT 0, config_json TEXT NOT NULL, error TEXT);
CREATE TABLE job_skips (
  job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  path BLOB NOT NULL, reason TEXT NOT NULL);
CREATE INDEX job_skips_job ON job_skips(job_id);
CREATE TABLE exams (
  job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  role TEXT NOT NULL, path BLOB NOT NULL, fingerprint TEXT NOT NULL,
  format INTEGER NOT NULL, result_json TEXT NOT NULL, thumb BLOB,
  PRIMARY KEY (job_id, role, path));
CREATE TABLE files (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  idx INTEGER NOT NULL, path BLOB NOT NULL, kind TEXT NOT NULL, size INTEGER NOT NULL,
  fingerprint TEXT NOT NULL, mtime_ms INTEGER NOT NULL, exif_json TEXT,
  phash INTEGER, dhash INTEGER, nphash INTEGER, ndhash INTEGER,
  paired_with INTEGER, error TEXT, warnings_json TEXT NOT NULL DEFAULT '[]', thumb BLOB,
  pinned INTEGER NOT NULL DEFAULT 0, UNIQUE (job_id, idx));
CREATE TABLE groups (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  position INTEGER NOT NULL, status TEXT NOT NULL, origin TEXT NOT NULL,
  review_reset INTEGER NOT NULL DEFAULT 0, bucket TEXT NOT NULL, max_distance INTEGER NOT NULL,
  ml_status TEXT NOT NULL, ml_score REAL, partial_path BLOB, output_path BLOB, error TEXT);
CREATE INDEX groups_job ON groups(job_id, position);
CREATE TABLE group_members (
  group_id INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
  file_idx INTEGER NOT NULL, layer_index INTEGER NOT NULL, PRIMARY KEY (group_id, file_idx));
CREATE TABLE group_warnings (
  group_id INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
  file_idx INTEGER NOT NULL, warning_json TEXT NOT NULL);
CREATE INDEX group_warnings_group ON group_warnings(group_id);
CREATE TABLE prefs (key TEXT PRIMARY KEY, value_json TEXT NOT NULL);
CREATE VIRTUAL TABLE embeddings_dinov2_small USING vec0(embedding float[384]);
CREATE VIRTUAL TABLE embeddings_dinov2_base USING vec0(embedding float[768]);
";

/// Migration 2: `jobs.warning`, why Stage 3 was skipped for the job (spec
/// §8.3).
const MIGRATION_2: &str = "ALTER TABLE jobs ADD COLUMN warning TEXT;";

const MIGRATION_LIST: &[M<'_>] = &[M::up(MIGRATION_1), M::up(MIGRATION_2)];

/// Every migration, in order. The database's `user_version` is the number
/// applied.
pub(crate) const MIGRATIONS: Migrations<'_> = Migrations::from_slice(MIGRATION_LIST);

/// The schema version this build reads and writes.
pub(crate) const SCHEMA_VERSION: usize = MIGRATION_LIST.len();

/// Makes every SQLite connection opened from now on load sqlite-vec. Runs
/// once per process; call it before opening any connection.
pub(crate) fn register_sqlite_vec() {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        type EntryPoint = unsafe extern "C" fn(
            *mut ffi::sqlite3,
            *mut *mut c_char,
            *const ffi::sqlite3_api_routines,
        ) -> c_int;
        // SAFETY: `sqlite3_vec_init` is sqlite-vec's extension entry point,
        // which has this signature in C; the Rust crate declares it without
        // arguments. It is linked into this binary with the bundled SQLite,
        // so it stays valid for the life of the process.
        let entry_point = unsafe {
            std::mem::transmute::<*const (), EntryPoint>(sqlite_vec::sqlite3_vec_init as *const ())
        };
        // SAFETY: registering an entry point has no other precondition. The
        // only failure is SQLITE_NOMEM, after which every connection fails
        // its migration with "no such module: vec0", so the code is not
        // checked here.
        unsafe {
            ffi::sqlite3_auto_extension(Some(entry_point));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_valid() {
        register_sqlite_vec();
        MIGRATIONS.validate().unwrap();
    }

    /// A database of migration 1 opens with its jobs, which have no warning.
    #[test]
    fn a_version_1_database_migrates_with_its_jobs() {
        register_sqlite_vec();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v1.db");
        {
            let mut conn = rusqlite::Connection::open(&path).unwrap();
            Migrations::from_slice(&MIGRATION_LIST[..1])
                .to_latest(&mut conn)
                .unwrap();
            let config = serde_json::to_string(&crate::JobConfig::new("/in".into())).unwrap();
            conn.execute(
                "INSERT INTO jobs (created_at, status, config_json)
                 VALUES ('2026-11-08T10:20:30.250Z', 'review_pending', ?1)",
                [config],
            )
            .unwrap();
        }
        let jobs = super::super::Store::open(&path).unwrap().jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            (jobs[0].error.as_ref(), jobs[0].warning.as_ref()),
            (None, None)
        );
    }
}
