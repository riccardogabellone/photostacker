//! The exam cache: what examining one file gave, kept per job, role and
//! path, so a resumed or retried scan skips the files it already examined.
//!
//! An exam is valid only for the content it examined (its fingerprint) and
//! for the meaning its result had when it was written (its format number):
//! a lookup with either different is a miss, and the next put replaces the
//! row.

use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{JobId, Store, StoreError, paths};

/// What an exam is looked up and stored by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExamKey<'a> {
    pub job: JobId,
    /// The examination that produced it, such as `active` or `passive`.
    pub role: &'a str,
    pub path: &'a Path,
    /// The file's content fingerprint when it was examined.
    pub fingerprint: &'a str,
    /// The version of the result's meaning; the analysis bumps it when a
    /// cached result would no longer be what examining the file gives.
    pub format: u32,
}

/// A cached exam.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredExam<T> {
    pub result: T,
    /// The thumbnail file written with it, if any.
    pub thumb: Option<PathBuf>,
}

impl Store {
    /// The exam stored for `key`'s job, role and path, when it was stored
    /// with the same fingerprint and format; otherwise `None`.
    pub fn exam<T: DeserializeOwned>(
        &self,
        key: &ExamKey,
    ) -> Result<Option<StoredExam<T>>, StoreError> {
        let row: Option<(String, Option<Vec<u8>>)> = self
            .conn
            .query_row(
                "SELECT result_json, thumb FROM exams
                 WHERE job_id = ?1 AND role = ?2 AND path = ?3
                   AND fingerprint = ?4 AND format = ?5",
                params![
                    key.job,
                    key.role,
                    paths::to_blob(key.path),
                    key.fingerprint,
                    key.format
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        row.map(|(json, thumb)| {
            Ok(StoredExam {
                result: serde_json::from_str(&json)?,
                thumb: thumb.as_deref().map(paths::from_blob),
            })
        })
        .transpose()
    }

    /// Stores `result` (and its thumbnail's path) for `key`, replacing any
    /// exam of the same job, role and path.
    pub fn put_exam<T: Serialize>(
        &self,
        key: &ExamKey,
        result: &T,
        thumb: Option<&Path>,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO exams (job_id, role, path, fingerprint, format, result_json, thumb)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (job_id, role, path) DO UPDATE SET
               fingerprint = excluded.fingerprint, format = excluded.format,
               result_json = excluded.result_json, thumb = excluded.thumb",
            params![
                key.job,
                key.role,
                paths::to_blob(key.path),
                key.fingerprint,
                key.format,
                serde_json::to_string(result)?,
                thumb.map(paths::to_blob)
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::JobConfig;

    fn key<'a>(job: JobId, path: &'a Path, fingerprint: &'a str, format: u32) -> ExamKey<'a> {
        ExamKey {
            job,
            role: "active",
            path,
            fingerprint,
            format,
        }
    }

    #[test]
    fn exam_misses_on_a_changed_fingerprint() {
        let store = Store::in_memory().unwrap();
        let job = store.insert_job(&JobConfig::new("/in".into())).unwrap();
        let path = Path::new("/in/a.jpg");
        let thumb = Path::new("/thumbs/1/0123456789abcdef.jpg");
        store
            .put_exam(&key(job, path, "f1", 1), &vec![1u64, u64::MAX], Some(thumb))
            .unwrap();

        let hit: StoredExam<Vec<u64>> = store.exam(&key(job, path, "f1", 1)).unwrap().unwrap();
        assert_eq!(hit.result, [1, u64::MAX]);
        assert_eq!(hit.thumb.as_deref(), Some(thumb));
        assert_eq!(
            store.exam::<Vec<u64>>(&key(job, path, "f2", 1)).unwrap(),
            None
        );
        // Another role or path is another exam.
        let passive = ExamKey {
            role: "passive",
            ..key(job, path, "f1", 1)
        };
        assert_eq!(store.exam::<Vec<u64>>(&passive).unwrap(), None);
        let other = Path::new("/in/b.jpg");
        assert_eq!(
            store.exam::<Vec<u64>>(&key(job, other, "f1", 1)).unwrap(),
            None
        );

        // The next put for the changed content replaces the row.
        store
            .put_exam(&key(job, path, "f2", 1), &vec![2u64], None)
            .unwrap();
        let hit: StoredExam<Vec<u64>> = store.exam(&key(job, path, "f2", 1)).unwrap().unwrap();
        assert_eq!((hit.result, hit.thumb), (vec![2], None));
        assert_eq!(
            store.exam::<Vec<u64>>(&key(job, path, "f1", 1)).unwrap(),
            None
        );
    }

    #[test]
    fn exam_misses_on_a_changed_format() {
        let store = Store::in_memory().unwrap();
        let job = store.insert_job(&JobConfig::new("/in".into())).unwrap();
        let path = Path::new("/in/a.jpg");
        store
            .put_exam(&key(job, path, "f1", 1), &"old meaning", None)
            .unwrap();
        assert_eq!(
            store.exam::<String>(&key(job, path, "f1", 2)).unwrap(),
            None
        );
        store
            .put_exam(&key(job, path, "f1", 2), &"new meaning", None)
            .unwrap();
        let hit: StoredExam<String> = store.exam(&key(job, path, "f1", 2)).unwrap().unwrap();
        assert_eq!(hit.result, "new meaning");
    }

    #[test]
    fn an_exam_of_a_missing_job_is_refused() {
        let store = Store::in_memory().unwrap();
        let path = Path::new("/in/a.jpg");
        assert!(store.put_exam(&key(42, path, "f", 1), &0, None).is_err());
    }
}
