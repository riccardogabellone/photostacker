//! The SQLite store (spec §7): jobs, their analyses, review state and
//! embeddings, behind numbered migrations.
//!
//! A [`Store`] is one connection. The engine owns the one writer
//! ([`Store::open`]); readers open their own read-only connection
//! ([`Store::open_read_only`]), which WAL lets run beside the writer.
//!
//! The enums are stored as their serde spellings, which the model pins.

mod exams;
mod files;
mod groups;
mod jobs;
pub mod paths;
mod prefs;
mod review;
mod schema;
mod vectors;

use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub use exams::{ExamKey, StoredExam};
pub use files::Swap;
pub use groups::StoredGroup;
pub use jobs::{JobCounts, StoredJob};
pub use review::{ManualRow, ReviewChange, StoredReview};
pub use vectors::embedding_table;

/// A job's row id.
pub type JobId = i64;
/// A group's row id; stable while its members are unchanged (spec §5.4.1).
pub type GroupId = i64;

/// How long a connection waits for another one's lock before it fails.
const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);

/// The page cache of the writer ([`Store::open`]) and of an in-memory
/// store, in KiB: SQLite's default.
///
/// The bundled SQLite is built with `SQLITE_ENABLE_MEMORY_MANAGEMENT`
/// (`LIBSQLITE3_FLAGS` in `.cargo/config.toml`), so the page caches of
/// every connection of the process are one pool, as large as the sum of
/// their sizes (spec §4.3, "What a review edit costs"). A connection that
/// reaches its own size takes the oldest pages of the pool, whoever's they
/// are. A connection left with fewer pages than its own size can then keep
/// none it reads while the pool is full: it reads every page from the
/// operating system. The writer reads every embedding at each edit, and
/// vec0 reaches a vector by walking its chunk of 384 pages from the start,
/// so a writer that cannot keep its pages makes an edit about 7 times
/// slower. The writer needs no more than this: it is always at its size,
/// so it takes the pages it needs from the pool.
const WRITER_CACHE_KIB: i64 = 2_000;

/// The page cache of a read-only connection ([`Store::open_read_only`]),
/// in KiB: 128 MiB.
///
/// Large enough that a review's reads (`files()`, `groups()`,
/// `unstacked()`) never fill it, so they never take the writer's pages
/// (see [`WRITER_CACHE_KIB`]). Measured with `tests/bench_review.rs`, with
/// rows as long as a camera's full EXIF makes them: the reads of a job of
/// 200,000 photos fit, those of 300,000 do not (an edit then takes 37 s
/// instead of 6), and 64 MiB fit 100,000 photos but not 150,000. A page is
/// allocated only when it is read, so a small job costs little; once the
/// writer has read more than the pool holds, the pool is full: about
/// 135 MiB in SQLite for the process.
const READ_ONLY_CACHE_KIB: i64 = 128 * 1024;

/// Why a store call failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("database migration error: {0}")]
    Migration(#[from] rusqlite_migration::Error),
    #[error("stored JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("cannot create the database folder: {0}")]
    Io(#[from] io::Error),
    /// A read-only connection found a schema this build does not know.
    #[error("the database schema is at version {found}, this build reads version {expected}")]
    SchemaVersion { found: usize, expected: usize },
    #[error("no job {0} in the store")]
    UnknownJob(JobId),
    #[error("no group {0} in the store")]
    UnknownGroup(GroupId),
    #[error("job {job} has no file {idx}")]
    UnknownFile { job: JobId, idx: usize },
    #[error("no embedding table for the model id {0:?}")]
    UnknownModel(String),
    /// The caller's data breaks an invariant of what it stores.
    #[error("invalid data for the store: {0}")]
    Invalid(String),
    /// A stored value breaks an invariant of what the store writes.
    #[error("the database holds an invalid value: {0}")]
    Corrupt(String),
}

/// One connection to the database, with sqlite-vec loaded and the schema
/// migrated.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (creating it and its folder if needed) the database at `path`
    /// for reading and writing, in WAL mode, and applies the migrations.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        schema::register_sqlite_vec();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        configure(&conn, WRITER_CACHE_KIB)?;
        let mode: String =
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            // Readers then wait for the writer instead of reading beside it.
            tracing::warn!(db = %path.display(), mode, "the database cannot use WAL");
        }
        Self::migrated(conn)
    }

    /// Opens the existing database at `path` read-only. It must already be
    /// migrated to this build's schema: only [`Store::open`] migrates.
    pub fn open_read_only(path: &Path) -> Result<Self, StoreError> {
        schema::register_sqlite_vec();
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        configure(&conn, READ_ONLY_CACHE_KIB)?;
        let found = usize::from(schema::MIGRATIONS.current_version(&conn)?);
        if found != schema::SCHEMA_VERSION {
            return Err(StoreError::SchemaVersion {
                found,
                expected: schema::SCHEMA_VERSION,
            });
        }
        Ok(Self { conn })
    }

    /// A private database in memory, migrated; gone when dropped.
    pub fn in_memory() -> Result<Self, StoreError> {
        schema::register_sqlite_vec();
        let conn = Connection::open_in_memory()?;
        configure(&conn, WRITER_CACHE_KIB)?;
        Self::migrated(conn)
    }

    fn migrated(mut conn: Connection) -> Result<Self, StoreError> {
        schema::MIGRATIONS.to_latest(&mut conn)?;
        Ok(Self { conn })
    }
}

/// The settings of every connection but the journal mode, with a page
/// cache of `cache_kib` KiB.
fn configure(conn: &Connection, cache_kib: i64) -> Result<(), StoreError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    // A negative size is in KiB.
    conn.pragma_update(None, "cache_size", -cache_kib)?;
    Ok(())
}

/// Starts a read of several statements as one snapshot: a deferred
/// transaction, whose first SELECT fixes the state every later one sees, so
/// a writer committing in between (on another connection) never gives a
/// torn view, such as groups whose members point past the files just read.
/// The read never writes, so dropping the transaction (a rollback) ends it.
fn read_snapshot(conn: &Connection) -> Result<rusqlite::Transaction<'_>, StoreError> {
    Ok(conn.unchecked_transaction()?)
}

/// A unit enum's serde spelling, such as `review_pending`.
fn enum_text<T: Serialize>(value: &T) -> Result<String, StoreError> {
    match serde_json::to_value(value)? {
        serde_json::Value::String(text) => Ok(text),
        other => Err(StoreError::Invalid(format!(
            "{other} is not the spelling of a unit enum"
        ))),
    }
}

/// The unit enum whose serde spelling is `text`.
fn enum_from_text<T: DeserializeOwned>(text: String) -> Result<T, StoreError> {
    Ok(serde_json::from_value(serde_json::Value::String(text))?)
}

/// `Ok` when a statement changed one row, else `missing`.
fn changed_one(changed: usize, missing: StoreError) -> Result<(), StoreError> {
    if changed == 1 { Ok(()) } else { Err(missing) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pragma<T: rusqlite::types::FromSql>(store: &Store, name: &str) -> T {
        store
            .conn
            .pragma_query_value(None, name, |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn fresh_file_opens_twice() {
        let dir = tempfile::tempdir().unwrap();
        // The folder does not exist yet: opening creates it.
        let path = dir.path().join("data").join("photostacker.db");
        let job = {
            let store = Store::open(&path).unwrap();
            store
                .insert_job(&crate::JobConfig::new("/in".into()))
                .unwrap()
        };
        let store = Store::open(&path).unwrap();
        assert_eq!(pragma::<i64>(&store, "user_version"), 2);
        assert_eq!(store.jobs().unwrap().len(), 1);
        assert!(store.job(job).unwrap().is_some());
    }

    #[test]
    fn sqlite_vec_is_loaded() {
        let store = Store::in_memory().unwrap();
        let version: String = store
            .conn
            .query_row("SELECT vec_version()", [], |row| row.get(0))
            .unwrap();
        assert!(version.starts_with('v'), "{version}");
    }

    #[test]
    fn wal_and_foreign_keys_are_on() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photostacker.db");
        let writer = Store::open(&path).unwrap();
        let reader = Store::open_read_only(&path).unwrap();
        for store in [&writer, &reader] {
            assert_eq!(pragma::<String>(store, "journal_mode"), "wal");
            assert_eq!(pragma::<i64>(store, "foreign_keys"), 1);
            assert_eq!(pragma::<i64>(store, "busy_timeout"), 5000);
            // NORMAL
            assert_eq!(pragma::<i64>(store, "synchronous"), 1);
        }
        // The reader sees the writer's rows, and cannot write itself.
        let job = writer
            .insert_job(&crate::JobConfig::new("/in".into()))
            .unwrap();
        assert!(reader.job(job).unwrap().is_some());
        assert!(
            reader
                .insert_job(&crate::JobConfig::new("/in".into()))
                .is_err()
        );
        // The foreign keys hold: a skip of a job that does not exist is refused.
        let orphan = writer.conn.execute(
            "INSERT INTO job_skips (job_id, path, reason) VALUES (999, x'00', 'x')",
            [],
        );
        assert!(orphan.is_err());
    }

    #[test]
    fn the_read_only_connection_has_the_larger_page_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photostacker.db");
        let writer = Store::open(&path).unwrap();
        let reader = Store::open_read_only(&path).unwrap();
        assert_eq!(pragma::<i64>(&writer, "cache_size"), -WRITER_CACHE_KIB);
        assert_eq!(pragma::<i64>(&reader, "cache_size"), -READ_ONLY_CACHE_KIB);
    }

    /// The store's cache sizes assume that every connection of the process
    /// shares one page cache, which only a SQLite built with
    /// `SQLITE_ENABLE_MEMORY_MANAGEMENT` does (spec §4.3).
    #[test]
    fn the_bundled_sqlite_shares_one_page_cache() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("photostacker.db")).unwrap();
        let options: Vec<String> = store
            .conn
            .prepare("PRAGMA compile_options")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            options.iter().any(|o| o == "ENABLE_MEMORY_MANAGEMENT"),
            "SQLite was built without SQLITE_ENABLE_MEMORY_MANAGEMENT, so each \
             connection has a page cache of its own: build it with \
             LIBSQLITE3_FLAGS=-DSQLITE_ENABLE_MEMORY_MANAGEMENT (see \
             .cargo/config.toml). Its options: {options:?}"
        );
    }

    #[test]
    fn read_only_store_refuses_an_unmigrated_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("other.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE other (x)")
            .unwrap();
        assert!(matches!(
            Store::open_read_only(&path),
            Err(StoreError::SchemaVersion {
                found: 0,
                expected: 2
            })
        ));
    }

    /// The rows of `job` in each table that holds any.
    fn rows_of(store: &Store, job: JobId) -> [i64; 6] {
        let count =
            |sql: &str| -> i64 { store.conn.query_row(sql, [job], |row| row.get(0)).unwrap() };
        [
            count("SELECT count(*) FROM jobs WHERE id = ?1"),
            count("SELECT count(*) FROM files WHERE job_id = ?1"),
            count("SELECT count(*) FROM job_skips WHERE job_id = ?1"),
            count("SELECT count(*) FROM exams WHERE job_id = ?1"),
            count("SELECT count(*) FROM groups WHERE job_id = ?1"),
            count(
                "SELECT (SELECT count(*) FROM group_members WHERE group_id IN
                          (SELECT id FROM groups WHERE job_id = ?1))
                      + (SELECT count(*) FROM group_warnings WHERE group_id IN
                          (SELECT id FROM groups WHERE job_id = ?1))",
            ),
        ]
    }

    #[test]
    fn deleting_a_job_removes_its_rows_and_vectors() {
        let mut store = Store::in_memory().unwrap();
        let analysis = files::sample_analysis();
        let mut jobs = Vec::new();
        for _ in 0..2 {
            let job = store
                .insert_job(&crate::JobConfig::new("/in".into()))
                .unwrap();
            store.save_analysis(job, &analysis).unwrap();
            let groups = store
                .apply_regroup(job, &files::regrouped_of(&analysis), &[])
                .unwrap();
            store
                .set_group_warnings(groups[0], &[(1, crate::model::Warning::IccMismatch)])
                .unwrap();
            let key = ExamKey {
                job,
                role: "active",
                path: Path::new("/in/a.jpg"),
                fingerprint: "f",
                format: 1,
            };
            store.put_exam(&key, &"exam", None).unwrap();
            let ids = store.file_row_ids(job).unwrap();
            let small = vec![0.25f32; 384];
            let base = vec![0.5f32; 768];
            store
                .put_embeddings(
                    "dinov2-small",
                    &[(ids[1], small.as_slice()), (ids[2], small.as_slice())],
                )
                .unwrap();
            store
                .put_embeddings("dinov2-base", &[(ids[1], base.as_slice())])
                .unwrap();
            jobs.push((job, ids));
        }
        let [(deleted, deleted_ids), (kept, kept_ids)] = &jobs[..] else {
            unreachable!()
        };
        let kept_rows = rows_of(&store, *kept);
        assert!(kept_rows.iter().all(|&rows| rows > 0), "{kept_rows:?}");

        assert!(store.delete_job(*deleted).unwrap());
        assert_eq!(rows_of(&store, *deleted), [0; 6]);
        for model in ["dinov2-small", "dinov2-base"] {
            let gone = store.embeddings(model, deleted_ids).unwrap();
            assert!(gone.iter().all(Option::is_none), "{model}");
        }
        // The other job is untouched.
        assert_eq!(rows_of(&store, *kept), kept_rows);
        let small = store.embeddings("dinov2-small", kept_ids).unwrap();
        assert_eq!(small.iter().filter(|v| v.is_some()).count(), 2);
        let base = store.embeddings("dinov2-base", kept_ids).unwrap();
        assert_eq!(base.iter().filter(|v| v.is_some()).count(), 1);
    }
}
