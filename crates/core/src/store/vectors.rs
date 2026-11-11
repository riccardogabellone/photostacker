//! Embeddings in sqlite-vec's vec0 tables, one table per model (its
//! dimension is fixed per table). A vector's rowid is its file's `files.id`.

use rusqlite::{Connection, OptionalExtension, params};

use super::{JobId, Store, StoreError, read_snapshot};

/// The vec0 table of `model`'s embeddings: `embeddings_` + the model id with
/// `-` replaced by `_`, such as `embeddings_dinov2_small`. Only ids of ASCII
/// letters, digits, `-` and `_` name a table; the table itself must exist.
pub fn embedding_table(model: &str) -> Result<String, StoreError> {
    let valid = !model.is_empty()
        && model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if valid {
        Ok(format!("embeddings_{}", model.replace('-', "_")))
    } else {
        Err(StoreError::UnknownModel(model.to_string()))
    }
}

impl Store {
    /// Stores each `(file row id, embedding)` for `model`, replacing the
    /// file's previous embedding, in one transaction. An embedding of the
    /// wrong length for the model's table is an error, and nothing is stored.
    pub fn put_embeddings(
        &mut self,
        model: &str,
        embeddings: &[(i64, &[f32])],
    ) -> Result<(), StoreError> {
        let table = existing_table(&self.conn, model)?;
        let tx = self.conn.transaction()?;
        {
            // vec0 has no upsert: the old row goes first.
            let mut delete = tx.prepare(&format!("DELETE FROM {table} WHERE rowid = ?1"))?;
            let mut insert = tx.prepare(&format!(
                "INSERT INTO {table} (rowid, embedding) VALUES (?1, ?2)"
            ))?;
            for &(file_id, embedding) in embeddings {
                delete.execute([file_id])?;
                insert.execute(params![file_id, vector_blob(embedding)])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The embeddings of `file_ids` for `model`, in the same order; `None`
    /// for a file that has none. They are read from one snapshot, by one
    /// cached statement.
    ///
    /// A row costs the same whatever the query's shape: vec0 opens the
    /// chunk of 1024 vectors that holds a vector for every vector it reads,
    /// so a scan or a `rowid IN` costs as much per row as a point query
    /// (spec §4.3, "What a review edit costs").
    pub fn embeddings(
        &self,
        model: &str,
        file_ids: &[i64],
    ) -> Result<Vec<Option<Vec<f32>>>, StoreError> {
        let snapshot = read_snapshot(&self.conn)?;
        let table = existing_table(&snapshot, model)?;
        let mut select =
            snapshot.prepare_cached(&format!("SELECT embedding FROM {table} WHERE rowid = ?1"))?;
        file_ids
            .iter()
            .map(|&file_id| {
                let blob: Option<Vec<u8>> =
                    select.query_row([file_id], |row| row.get(0)).optional()?;
                blob.map(|blob| vector_from_blob(&blob)).transpose()
            })
            .collect()
    }
}

/// [`embedding_table`] when that table exists.
fn existing_table(conn: &Connection, model: &str) -> Result<String, StoreError> {
    let table = embedding_table(model)?;
    if vec_tables(conn)?.contains(&table) {
        Ok(table)
    } else {
        Err(StoreError::UnknownModel(model.to_string()))
    }
}

/// The names of every vec0 table (not their shadow tables).
fn vec_tables(conn: &Connection) -> Result<Vec<String>, StoreError> {
    let mut statement = conn.prepare_cached(
        "SELECT name FROM sqlite_schema
         WHERE type = 'table' AND sql LIKE 'CREATE VIRTUAL TABLE % USING vec0(%'
         ORDER BY name",
    )?;
    let names = statement.query_map([], |row| row.get(0))?;
    Ok(names.collect::<Result<_, _>>()?)
}

/// Removes the embeddings of every file of `job`, in every model's table.
pub(super) fn delete_job_vectors(conn: &Connection, job: JobId) -> Result<(), StoreError> {
    let file_ids: Vec<i64> = {
        let mut statement = conn.prepare("SELECT id FROM files WHERE job_id = ?1")?;
        let ids = statement.query_map([job], |row| row.get(0))?;
        ids.collect::<Result<_, _>>()?
    };
    for table in vec_tables(conn)? {
        let mut delete = conn.prepare(&format!("DELETE FROM {table} WHERE rowid = ?1"))?;
        for &file_id in &file_ids {
            delete.execute([file_id])?;
        }
    }
    Ok(())
}

/// sqlite-vec reads a BLOB vector as the machine's own `float` array.
fn vector_blob(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect()
}

fn vector_from_blob(blob: &[u8]) -> Result<Vec<f32>, StoreError> {
    let (values, rest) = blob.as_chunks::<4>();
    if !rest.is_empty() {
        return Err(StoreError::Corrupt(format!(
            "an embedding of {} bytes is not a float array",
            blob.len()
        )));
    }
    Ok(values
        .iter()
        .map(|value| f32::from_ne_bytes(*value))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unit vector of `dims` values that differ, so an order or byte
    /// mix-up shows.
    fn embedding(dims: usize, seed: f32) -> Vec<f32> {
        let raw: Vec<f32> = (0..dims).map(|i| seed + i as f32 * 0.37).collect();
        let norm = raw.iter().map(|v| v * v).sum::<f32>().sqrt();
        raw.iter().map(|v| v / norm).collect()
    }

    #[test]
    fn embedding_table_names_follow_the_model_id() {
        assert_eq!(
            embedding_table("dinov2-small").unwrap(),
            "embeddings_dinov2_small"
        );
        for bad in ["", "a b", "x;DROP TABLE jobs", "dinov2.small"] {
            assert!(matches!(
                embedding_table(bad),
                Err(StoreError::UnknownModel(_))
            ));
        }
    }

    /// The schema has a vec0 table for every registry model, of the
    /// model's dimension.
    #[test]
    fn every_registry_model_has_a_vec0_table_of_its_dims() {
        let mut store = Store::in_memory().unwrap();
        let tables = vec_tables(&store.conn).unwrap();
        for spec in crate::models::MODELS {
            let table = embedding_table(spec.id).unwrap();
            assert!(tables.contains(&table), "{table} not in {tables:?}");
            let vector = embedding(spec.dims, 1.0);
            store
                .put_embeddings(spec.id, &[(1, vector.as_slice())])
                .unwrap();
            assert_eq!(store.embeddings(spec.id, &[1]).unwrap(), [Some(vector)]);
        }
    }

    #[test]
    fn embedding_round_trip_384() {
        let mut store = Store::in_memory().unwrap();
        let first = embedding(384, 1.0);
        let second = embedding(384, -2.5);
        store
            .put_embeddings(
                "dinov2-small",
                &[(10, first.as_slice()), (11, second.as_slice())],
            )
            .unwrap();
        assert_eq!(
            store.embeddings("dinov2-small", &[11, 12, 10]).unwrap(),
            [Some(second.clone()), None, Some(first)]
        );
        // A second put replaces the vector.
        let third = embedding(384, 7.0);
        store
            .put_embeddings("dinov2-small", &[(10, third.as_slice())])
            .unwrap();
        assert_eq!(
            store.embeddings("dinov2-small", &[10, 11]).unwrap(),
            [Some(third), Some(second)]
        );
        // The tables are per model.
        assert_eq!(store.embeddings("dinov2-base", &[10]).unwrap(), [None]);
    }

    #[test]
    fn a_wrong_length_or_model_is_refused() {
        let mut store = Store::in_memory().unwrap();
        let short = embedding(383, 1.0);
        let good = embedding(384, 1.0);
        assert!(
            store
                .put_embeddings(
                    "dinov2-small",
                    &[(1, good.as_slice()), (2, short.as_slice())]
                )
                .is_err()
        );
        // The transaction stored nothing.
        assert_eq!(store.embeddings("dinov2-small", &[1]).unwrap(), [None]);
        assert!(matches!(
            store.put_embeddings("dinov2-large", &[(1, good.as_slice())]),
            Err(StoreError::UnknownModel(_))
        ));
    }
}
