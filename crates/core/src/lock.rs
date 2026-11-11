//! An exclusive lock across processes, on a lock file: one engine per
//! database (spec §7.2), and one download, import or removal per model
//! (`models/lock.rs`). The CLI and the app may run together.
//!
//! The lock is the operating system's exclusive lock on the file
//! ([`File::try_lock`]), held while the file is open. The system releases it
//! when its process ends, crashed or not, so a lock is never stale. The
//! file itself stays: deleting a lock file while another process may have
//! it open would let two processes lock two different files of one name.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::Path;

/// A held lock; dropping it releases the lock.
#[derive(Debug)]
pub(crate) struct FileLock {
    _file: File,
}

/// Locks `file`, creating it and its folder if needed. `Ok(None)` when
/// another holder, of this process or another, has the lock.
///
/// A file system without locks (`Unsupported`) is logged and counts as
/// locked: what the lock guards still works there, without the guard.
pub(crate) fn try_lock(file: &Path) -> io::Result<Option<FileLock>> {
    if let Some(folder) = file
        .parent()
        .filter(|folder| !folder.as_os_str().is_empty())
    {
        fs::create_dir_all(folder)?;
    }
    let handle = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(file)?;
    match handle.try_lock() {
        Ok(()) => Ok(Some(FileLock { _file: handle })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) if error.kind() == io::ErrorKind::Unsupported => {
            tracing::warn!(
                file = %file.display(),
                %error,
                "the file system cannot lock this file; going on without the lock"
            );
            Ok(Some(FileLock { _file: handle }))
        }
        Err(TryLockError::Error(error)) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_holder_at_a_time() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("data").join("photostacker.db.lock");
        let held = try_lock(&file).unwrap().expect("a free lock");
        assert!(try_lock(&file).unwrap().is_none());
        drop(held);
        // Released on drop, and the lock file stays for the next holder.
        assert!(file.is_file());
        assert!(try_lock(&file).unwrap().is_some());
    }
}
