//! App preferences: one JSON value per key (theme, defaults, ML switch,
//! active model, last folders).

use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{Store, StoreError};

impl Store {
    /// The preference `key`, or `None` when it was never set.
    pub fn pref<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, StoreError> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT value_json FROM prefs WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|json| serde_json::from_str(&json))
            .transpose()
            .map_err(StoreError::from)
    }

    /// Sets the preference `key` to `value`, replacing what it was.
    pub fn set_pref<T: Serialize + ?Sized>(&self, key: &str, value: &T) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO prefs (key, value_json) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value_json = excluded.value_json",
            params![key, serde_json::to_string(value)?],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefs_round_trip() {
        let store = Store::in_memory().unwrap();
        assert_eq!(store.pref::<String>("theme").unwrap(), None);
        store.set_pref("theme", "dark").unwrap();
        store.set_pref("ml", &true).unwrap();
        store.set_pref("last_folders", &vec!["/a", "/b"]).unwrap();
        assert_eq!(
            store.pref::<String>("theme").unwrap().as_deref(),
            Some("dark")
        );
        assert_eq!(store.pref::<bool>("ml").unwrap(), Some(true));
        assert_eq!(
            store.pref::<Vec<String>>("last_folders").unwrap(),
            Some(vec!["/a".to_string(), "/b".to_string()])
        );
        store.set_pref("theme", "light").unwrap();
        assert_eq!(
            store.pref::<String>("theme").unwrap().as_deref(),
            Some("light")
        );
        // A value of another type is an error, not a missing preference.
        assert!(matches!(
            store.pref::<u32>("theme"),
            Err(StoreError::Json(_))
        ));
    }
}
