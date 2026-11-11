//! Where the app keeps its database, thumbnails, models and logs (spec §7.1).

use std::ffi::OsString;
use std::path::PathBuf;

use directories::ProjectDirs;

/// The environment variable that moves all of the app's data into one
/// folder, so tests and portable setups never touch the user's real data.
pub const DATA_DIR_ENV: &str = "PHOTOSTACKER_DATA_DIR";

/// The app id `io.github.riccardogabellone.Photostacker`, in the three parts
/// `ProjectDirs` takes.
const QUALIFIER: &str = "io.github";
const ORGANIZATION: &str = "riccardogabellone";
const APPLICATION: &str = "Photostacker";

/// The app's two root folders. Nothing is created here: the store creates
/// the database's folder when it opens it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppDirs {
    /// The database, the models and the logs.
    pub data: PathBuf,
    /// What can be rebuilt: the review thumbnails.
    pub cache: PathBuf,
}

impl AppDirs {
    /// `PHOTOSTACKER_DATA_DIR` when it is set and not empty, with the cache
    /// in its `cache` subfolder; otherwise the platform's folders for the app
    /// id. `None` when the platform has no home folder to put them in.
    pub fn resolve() -> Option<Self> {
        Self::from_env(std::env::var_os(DATA_DIR_ENV))
    }

    /// [`AppDirs::resolve`] with the variable's value given.
    fn from_env(data_dir: Option<OsString>) -> Option<Self> {
        match data_dir.filter(|dir| !dir.is_empty()) {
            Some(dir) => Some(Self::under(PathBuf::from(dir))),
            None => ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION).map(|dirs| Self {
                data: dirs.data_dir().to_path_buf(),
                cache: dirs.cache_dir().to_path_buf(),
            }),
        }
    }

    /// Everything under `data`, with the cache in `data/cache`.
    pub fn under(data: PathBuf) -> Self {
        Self {
            cache: data.join("cache"),
            data,
        }
    }

    /// The SQLite database.
    pub fn db(&self) -> PathBuf {
        self.data.join("photostacker.db")
    }

    /// The review thumbnails.
    pub fn thumbs(&self) -> PathBuf {
        self.cache.join("thumbs")
    }

    /// The installed ML models, one folder per model id.
    pub fn models(&self) -> PathBuf {
        self.data.join("models")
    }

    /// The log files.
    pub fn logs(&self) -> PathBuf {
        self.data.join("logs")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_dirs_follow_photostacker_data_dir() {
        let root = PathBuf::from("/tmp/ps-data");
        let dirs = AppDirs::from_env(Some(root.clone().into())).unwrap();
        assert_eq!(dirs, AppDirs::under(root.clone()));
        assert_eq!(dirs.data, root);
        assert_eq!(dirs.cache, root.join("cache"));
        assert_eq!(dirs.db(), root.join("photostacker.db"));
        assert_eq!(dirs.thumbs(), root.join("cache").join("thumbs"));
        assert_eq!(dirs.models(), root.join("models"));
        assert_eq!(dirs.logs(), root.join("logs"));
    }

    #[test]
    fn app_dirs_without_the_variable_are_the_platform_folders() {
        let platform = ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION).map(|dirs| {
            (
                dirs.data_dir().to_path_buf(),
                dirs.cache_dir().to_path_buf(),
            )
        });
        for unset in [None, Some(OsString::new())] {
            let dirs = AppDirs::from_env(unset).map(|dirs| (dirs.data, dirs.cache));
            assert_eq!(dirs, platform);
        }
    }
}
