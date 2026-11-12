//! File-system primitives of the writers: the output-folder check, the plan
//! write that may replace, the no-clobber persistence of a finished file,
//! and the folder sync after either (spec §2 anti-goals, §5.6).

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::model::JobConfig;

/// Writes `bytes` to `path`, replacing it if it exists, without ever leaving
/// a half-written file under `path`.
///
/// The bytes go to `<path>.partial` first, are synced, then renamed over
/// `path`, and the folder is synced ([`sync_dir_or_warn`]: a failed folder
/// sync is logged, not returned, since the file is already in place).
/// Replacing is allowed: this is for the plan file, never for a stack. A
/// stale `<path>.partial` from a crash is our own temp and is removed first.
/// On error, the partial is removed if this call created it.
///
/// A `path` without a file name (`out/`, `.`, `..`) is refused with
/// `InvalidInput` before anything is touched.
pub fn write_replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_replace_with(path, bytes, sync_dir)
}

/// [`write_replace`], syncing the folder with `sync`: [`sync_dir`], or in
/// tests a sync that fails.
fn write_replace_with(path: &Path, bytes: &[u8], sync: SyncDir) -> io::Result<()> {
    if file_name(path).is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} does not name a file", path.display()),
        ));
    }
    // `path` ends with its file name, so this is that name's partial.
    let partial = partial_path(path);
    match fs::remove_file(&partial) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    // A partial that cannot be created is not ours: it stays.
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial)?;
    let result = write_synced(file, bytes).and_then(|()| fs::rename(&partial, path));
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result?;
    sync_dir_or_warn(folder_of(path), sync);
    Ok(())
}

/// The folder that holds the file `path`: its parent, or `.` for a bare
/// file name, whose parent is empty.
fn folder_of(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// A folder sync: [`sync_dir`], or in tests one that fails.
pub(crate) type SyncDir = fn(&Path) -> io::Result<()>;

/// Syncs the folder `dir` with `sync` once a name was made in it. A failure
/// is logged with `tracing::warn!` (the folder and the error), not
/// returned: the file is complete under its name already, and only whether
/// the name survives a crash is in doubt.
pub(crate) fn sync_dir_or_warn(dir: &Path, sync: SyncDir) {
    if let Err(error) = sync(dir) {
        tracing::warn!(
            folder = %dir.display(),
            %error,
            "cannot sync the folder; the file just named in it may not survive a crash"
        );
    }
}

/// Syncs the folder `dir`, so that the names just made in it (a persisted
/// stack, a replaced plan) survive a crash. The writers call it through
/// [`sync_dir_or_warn`], so its error never fails a write.
///
/// On Unix the folder is opened and synced (`fsync`). A file system that
/// cannot sync a folder, which it reports with `EINVAL` or as unsupported,
/// counts as synced, as PostgreSQL treats it. Elsewhere, Windows included,
/// there is no folder sync, and this does nothing.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use io::ErrorKind::{InvalidInput, Unsupported};
        match File::open(dir)?.sync_all() {
            Err(e) if matches!(e.kind(), InvalidInput | Unsupported) => Ok(()),
            result => result,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// The file name `path` ends with, `None` when it ends with a separator,
/// `.` or `..` (or is a bare root or prefix): such a path names a folder.
fn file_name(path: &Path) -> Option<&OsStr> {
    let bytes = path.as_os_str().as_encoded_bytes();
    // Separators are ASCII, so a byte equal to one is one.
    let last = bytes
        .rsplit(|&b| std::path::is_separator(char::from(b)))
        .next()
        .unwrap_or_default();
    if last.is_empty() || last == b"." || last == b".." {
        return None;
    }
    path.file_name()
}

/// Writes `bytes` to `file` and syncs it; the file is closed on return.
fn write_synced(mut file: File, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()
}

/// `<path>.partial`.
fn partial_path(path: &Path) -> PathBuf {
    with_suffix(path, ".partial")
}

/// `path` with `suffix` appended to its whole file name, extension
/// included: `a.psd` and `.partial` give `a.psd.partial`. The partial of
/// [`PartialFile`] and the encoder's spill files are named so.
pub(crate) fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(suffix);
    PathBuf::from(name)
}

/// A file being written at `<final>.partial`, moved to a final name only by
/// [`PartialFile::persist_no_clobber`], which never replaces an existing
/// file. Dropped before it is persisted, it removes the partial.
#[derive(Debug)]
pub struct PartialFile {
    /// The partial's path.
    path: PathBuf,
    /// The open partial. `None` only while the partial is being dropped, so
    /// it is closed before it is removed.
    file: Option<File>,
    /// The partial has been moved to its final name: nothing to remove.
    persisted: bool,
}

impl PartialFile {
    /// Creates `<final_path>.partial`. Fails with `AlreadyExists` if that
    /// partial already exists (an earlier crash, or another writer).
    pub fn create(final_path: &Path) -> io::Result<Self> {
        let path = partial_path(final_path);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(Self {
            path,
            file: Some(file),
            persisted: false,
        })
    }

    /// The partial's path, `<final>.partial`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The open partial, to write the output into.
    pub fn file(&mut self) -> &mut File {
        self.file
            .as_mut()
            .expect("the file is open until the PartialFile is dropped")
    }

    /// Syncs the partial and moves it to `final_path`, which it never
    /// replaces. Returns `final_path`.
    ///
    /// It links the partial to `final_path` with `hard_link`, which fails if
    /// `final_path` exists, then removes the partial. Only where the file
    /// system has no hard links (see [`hard_links_unsupported`]) does it fall
    /// back to renaming the partial, after checking that `final_path` does
    /// not exist. Any other link error is returned as is.
    ///
    /// The fallback leaves one race: the check and the rename are not
    /// atomic, and `rename` replaces its target on Linux and on Windows
    /// alike, so a file that appears at `final_path` between the two is
    /// replaced. Only file systems without hard links (FAT, exFAT, some
    /// network shares) are exposed to it.
    ///
    /// On error the partial is kept and handed back in [`PersistError`]: when
    /// the name is taken (`error.kind()` is `AlreadyExists`), the caller can
    /// retry with the next name and the same file. Dropping the error drops
    /// the file, which removes the partial.
    pub fn persist_no_clobber(mut self, final_path: &Path) -> Result<PathBuf, PersistError> {
        if let Err(error) = self.file().sync_all() {
            return Err(PersistError { error, file: self });
        }
        match fs::hard_link(&self.path, final_path) {
            Ok(()) => {
                // The output is complete under its final name. If the extra
                // link cannot be removed, it is a leftover, not a failure:
                // reporting one would make the caller retry another name.
                let _ = fs::remove_file(&self.path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(PersistError { error, file: self });
            }
            Err(error) if hard_links_unsupported(&error) => {
                if let Err(error) = rename_no_clobber(&self.path, final_path) {
                    return Err(PersistError { error, file: self });
                }
            }
            Err(error) => return Err(PersistError { error, file: self }),
        }
        self.persisted = true;
        Ok(final_path.to_path_buf())
    }
}

/// Windows error codes of a file system without hard links:
/// `ERROR_INVALID_FUNCTION` and `ERROR_NOT_SUPPORTED`.
#[cfg(windows)]
const NO_HARD_LINK_CODES: [i32; 2] = [1, 50];

/// The `hard_link` error means the file system has no hard links, so the
/// rename fallback may run: `Unsupported`; on Unix `PermissionDenied` (EPERM,
/// which Linux gives on FAT and exFAT); on Windows [`NO_HARD_LINK_CODES`],
/// which FAT and exFAT give there. On Windows `PermissionDenied` is
/// `ERROR_ACCESS_DENIED`, a real refusal, so it never takes the racy rename.
/// Any other error, such as EIO, ENOSPC or EMLINK, is a real failure and is
/// returned as is.
fn hard_links_unsupported(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    #[cfg(unix)]
    if error.kind() == io::ErrorKind::PermissionDenied {
        return true;
    }
    #[cfg(windows)]
    if error
        .raw_os_error()
        .is_some_and(|code| NO_HARD_LINK_CODES.contains(&code))
    {
        return true;
    }
    false
}

/// Renames `from` to `to` only if `to` does not exist: the fallback where
/// hard links are unsupported. Anything at `to`, a dangling symlink
/// included, or a `to` that cannot be inspected, counts as taken.
fn rename_no_clobber(from: &Path, to: &Path) -> io::Result<()> {
    match fs::symlink_metadata(to) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => fs::rename(from, to),
        Err(e) => Err(e),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists", to.display()),
        )),
    }
}

impl Drop for PartialFile {
    fn drop(&mut self) {
        // Close the file, then remove it. Closing first is not needed for
        // the removal (std opens files with FILE_SHARE_DELETE on Windows,
        // which is also why an open partial can be linked or renamed), but
        // it frees the space as soon as the name is gone.
        drop(self.file.take());
        if !self.persisted {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// A failed [`PartialFile::persist_no_clobber`]: the cause, and the partial,
/// still on disk, so the caller can retry under another name.
#[derive(Debug, thiserror::Error)]
#[error("cannot persist {}", file.path.display())]
pub struct PersistError {
    #[source]
    pub error: io::Error,
    pub file: PartialFile,
}

impl From<PersistError> for io::Error {
    /// The cause; the partial is dropped, so removed.
    fn from(e: PersistError) -> Self {
        e.error
    }
}

/// Why the output folder cannot receive the stacks.
#[derive(Debug, thiserror::Error)]
#[error("output folder {}: {reason}", path.display())]
pub struct OutputDirError {
    pub path: PathBuf,
    pub reason: String,
}

/// Creates the output folder of `config` if needed and returns its canonical
/// path. Run once before writing: since [`JobConfig::validate`], the folder
/// may have been replaced, e.g. by a symlink to the input folder.
///
/// It fails if the folder cannot be created, is not a directory, or is the
/// input folder once both are canonicalised.
pub fn ensure_output_dir(config: &JobConfig) -> Result<PathBuf, OutputDirError> {
    let output = &config.output_dir;
    let fail = |reason: String| OutputDirError {
        path: output.clone(),
        reason,
    };
    let not_a_directory = || fail("is not a directory".into());

    if output.exists() && !output.is_dir() {
        return Err(not_a_directory());
    }
    fs::create_dir_all(output).map_err(|e| fail(format!("cannot be created: {e}")))?;
    let canonical = output
        .canonicalize()
        .map_err(|e| fail(format!("cannot be resolved: {e}")))?;
    if !canonical.is_dir() {
        return Err(not_a_directory());
    }
    let input = config.input_dir.canonicalize().map_err(|e| {
        fail(format!(
            "the input folder {} cannot be resolved: {e}",
            config.input_dir.display()
        ))
    })?;
    if canonical == input {
        return Err(fail("is the input folder".into()));
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use io::ErrorKind;

    /// The names of the entries of `dir`, sorted.
    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn write_replace_replaces_and_leaves_no_partial() {
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("plan.json");
        write_replace(&plan, b"first").unwrap();
        assert_eq!(fs::read(&plan).unwrap(), b"first");
        write_replace(&plan, b"second").unwrap();
        assert_eq!(fs::read(&plan).unwrap(), b"second");
        assert_eq!(entries(dir.path()), ["plan.json"]);
    }

    #[test]
    fn write_replace_succeeds_when_the_folder_sync_fails() {
        // The plan is in place once renamed; the failed sync is only logged.
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("plan.json");
        let fails: SyncDir = |_| Err(io::Error::other("injected sync failure"));
        write_replace_with(&plan, b"plan", fails).unwrap();
        assert_eq!(fs::read(&plan).unwrap(), b"plan");
        assert_eq!(entries(dir.path()), ["plan.json"]);
    }

    #[test]
    fn a_bare_file_name_is_in_the_current_folder() {
        // `write_replace` syncs `.` for `plan.json`, not its empty parent.
        assert_eq!(folder_of(Path::new("plan.json")), Path::new("."));
        let nested: PathBuf = ["out", "plan.json"].iter().collect();
        assert_eq!(folder_of(&nested), Path::new("out"));
    }

    #[test]
    fn sync_dir_syncs_a_folder() {
        let dir = tempfile::tempdir().unwrap();
        sync_dir(dir.path()).unwrap();
        let missing = sync_dir(&dir.path().join("missing"));
        if cfg!(unix) {
            assert_eq!(missing.unwrap_err().kind(), ErrorKind::NotFound);
        } else {
            missing.unwrap();
        }
    }

    #[test]
    fn write_replace_removes_a_stale_partial() {
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("plan.json");
        fs::write(dir.path().join("plan.json.partial"), b"crashed").unwrap();
        write_replace(&plan, b"plan").unwrap();
        assert_eq!(fs::read(&plan).unwrap(), b"plan");
        assert_eq!(entries(dir.path()), ["plan.json"]);
    }

    #[test]
    fn write_replace_failure_leaves_no_partial() {
        let dir = tempfile::tempdir().unwrap();
        // A folder under the final name: the rename over it fails.
        let plan = dir.path().join("plan.json");
        fs::create_dir(&plan).unwrap();
        assert!(write_replace(&plan, b"plan").is_err());
        assert_eq!(entries(dir.path()), ["plan.json"]);
        assert!(plan.is_dir());
    }

    #[test]
    fn write_replace_refuses_a_path_without_a_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("out");
        fs::create_dir(&folder).unwrap();
        let mut trailing = folder.clone().into_os_string();
        trailing.push(std::path::MAIN_SEPARATOR_STR);
        for path in [
            PathBuf::from(trailing),
            PathBuf::from("."),
            folder.join("."),
            folder.join(".."),
        ] {
            let error = write_replace(&path, b"plan").unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "{}", path.display());
        }
        // Nothing was touched: no partial next to or inside the folder.
        assert_eq!(entries(dir.path()), ["out"]);
        assert!(entries(&folder).is_empty());
    }

    #[test]
    fn write_replace_keeps_a_partial_it_did_not_create() {
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("plan.json");
        // Not a stale file of ours but a folder: it cannot be removed or
        // created over, and the call must leave it as it was.
        let partial = dir.path().join("plan.json.partial");
        fs::create_dir(&partial).unwrap();
        fs::write(partial.join("keep"), b"user data").unwrap();
        assert!(write_replace(&plan, b"plan").is_err());
        assert_eq!(fs::read(partial.join("keep")).unwrap(), b"user data");
        assert_eq!(entries(dir.path()), ["plan.json.partial"]);
    }

    #[test]
    fn only_unsupported_link_errors_fall_back_to_rename() {
        let kind = |kind: ErrorKind| io::Error::from(kind);
        assert!(hard_links_unsupported(&kind(ErrorKind::Unsupported)));
        for other in [
            ErrorKind::NotFound,
            ErrorKind::StorageFull,
            ErrorKind::TooManyLinks,
            ErrorKind::Other,
        ] {
            assert!(!hard_links_unsupported(&kind(other)), "{other:?}");
        }
        #[cfg(unix)]
        {
            // EIO, ENOSPC and EMLINK never take the rename path; EPERM
            // (FAT, exFAT) does.
            for code in [5, 28, 31] {
                let error = io::Error::from_raw_os_error(code);
                assert!(!hard_links_unsupported(&error), "{code}: {error}");
            }
            assert!(hard_links_unsupported(&io::Error::from_raw_os_error(1)));
            assert!(hard_links_unsupported(&kind(ErrorKind::PermissionDenied)));
        }
        #[cfg(windows)]
        {
            // ERROR_INVALID_FUNCTION and ERROR_NOT_SUPPORTED; not
            // ERROR_ACCESS_DENIED, ERROR_DISK_FULL or ERROR_TOO_MANY_LINKS.
            for code in [1, 50] {
                assert!(hard_links_unsupported(&io::Error::from_raw_os_error(code)));
            }
            for code in [5, 112, 1142] {
                assert!(!hard_links_unsupported(&io::Error::from_raw_os_error(code)));
            }
            // Access denied is a real refusal on Windows, never "no links".
            assert!(!hard_links_unsupported(&kind(ErrorKind::PermissionDenied)));
        }
    }

    #[test]
    fn other_link_errors_are_returned_with_the_partial() {
        let dir = tempfile::tempdir().unwrap();
        let mut partial = PartialFile::create(&dir.path().join("a.psd")).unwrap();
        partial.file().write_all(b"stack").unwrap();
        let target = dir.path().join("missing").join("a.psd");
        let error = partial.persist_no_clobber(&target).unwrap_err();
        assert_eq!(error.error.kind(), ErrorKind::NotFound);
        assert_eq!(fs::read(error.file.path()).unwrap(), b"stack");
        drop(error);
        assert!(entries(dir.path()).is_empty());
    }

    #[test]
    fn persist_no_clobber_moves_the_partial() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("IMG_0001_3.psd");
        let mut partial = PartialFile::create(&target).unwrap();
        assert_eq!(partial.path(), dir.path().join("IMG_0001_3.psd.partial"));
        partial.file().write_all(b"stack").unwrap();
        assert_eq!(partial.persist_no_clobber(&target).unwrap(), target);
        assert_eq!(fs::read(&target).unwrap(), b"stack");
        assert_eq!(entries(dir.path()), ["IMG_0001_3.psd"]);
    }

    #[test]
    fn persist_no_clobber_refuses_existing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("IMG_0001_3.psd");
        fs::write(&target, b"user file").unwrap();
        let mut partial = PartialFile::create(&target).unwrap();
        partial.file().write_all(b"stack").unwrap();

        let error = partial.persist_no_clobber(&target).unwrap_err();
        assert_eq!(error.error.kind(), ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&target).unwrap(), b"user file");
        assert_eq!(fs::read(error.file.path()).unwrap(), b"stack");

        // The same partial persists under the next name.
        let next = dir.path().join("IMG_0001_3-1.psd");
        assert_eq!(error.file.persist_no_clobber(&next).unwrap(), next);
        assert_eq!(fs::read(&next).unwrap(), b"stack");
        assert_eq!(fs::read(&target).unwrap(), b"user file");
        assert_eq!(entries(dir.path()), ["IMG_0001_3-1.psd", "IMG_0001_3.psd"]);
    }

    #[test]
    fn rename_fallback_refuses_existing() {
        let dir = tempfile::tempdir().unwrap();
        let partial = dir.path().join("a.psd.partial");
        let target = dir.path().join("a.psd");
        fs::write(&partial, b"stack").unwrap();
        fs::write(&target, b"user file").unwrap();
        let error = rename_no_clobber(&partial, &target).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&target).unwrap(), b"user file");
        assert_eq!(fs::read(&partial).unwrap(), b"stack");

        fs::remove_file(&target).unwrap();
        rename_no_clobber(&partial, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"stack");
        assert_eq!(entries(dir.path()), ["a.psd"]);
    }

    #[test]
    fn create_refuses_an_existing_partial() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.psd");
        fs::write(dir.path().join("a.psd.partial"), b"crashed").unwrap();
        let error = PartialFile::create(&target).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(dir.path().join("a.psd.partial")).unwrap(),
            b"crashed"
        );
    }

    #[test]
    fn dropped_partial_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let mut partial = PartialFile::create(&dir.path().join("a.psd")).unwrap();
        partial.file().write_all(b"half a stack").unwrap();
        assert_eq!(entries(dir.path()), ["a.psd.partial"]);
        drop(partial);
        assert!(entries(dir.path()).is_empty());
    }

    #[test]
    fn ensure_output_dir_creates_and_canonicalises() {
        let input = tempfile::tempdir().unwrap();
        let config = JobConfig::new(input.path().to_path_buf());
        let output = ensure_output_dir(&config).unwrap();
        assert!(output.is_dir());
        assert_eq!(output, config.output_dir.canonicalize().unwrap());
    }

    #[test]
    fn ensure_output_dir_rejects_a_file() {
        let input = tempfile::tempdir().unwrap();
        let mut config = JobConfig::new(input.path().to_path_buf());
        config.output_dir = input.path().join("notes.txt");
        fs::write(&config.output_dir, b"not a folder").unwrap();
        let error = ensure_output_dir(&config).unwrap_err();
        assert_eq!(error.path, config.output_dir);
        assert!(error.reason.contains("not a directory"), "{error}");
    }

    #[test]
    fn ensure_output_dir_rejects_input_via_symlink() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("photos");
        fs::create_dir(&input).unwrap();
        let link = root.path().join("out");
        if !photostacker_testkit::symlink_dir(&input, &link) {
            return;
        }
        let mut config = JobConfig::new(input);
        config.output_dir = link.clone();

        let error = ensure_output_dir(&config).unwrap_err();
        assert_eq!(error.path, link);
        assert!(error.reason.contains("input folder"), "{error}");
        assert_eq!(
            error.to_string(),
            format!("output folder {}: {}", link.display(), error.reason)
        );
    }
}
