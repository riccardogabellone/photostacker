//! The per-model lock: one download, import or removal of a model at a
//! time, across processes (the CLI and the app may run together). Without
//! it, a second download could truncate the `.part` between the first one's
//! hash check and its rename, and the first would install bytes it never
//! verified.
//!
//! It is the crate's file lock ([`crate::lock`]) on `<id>/.lock`, which
//! stays when released, as every lock file does.

use std::io;
use std::path::Path;

/// The lock file inside a model's folder.
pub(super) const LOCK_FILE: &str = ".lock";

/// A held model lock; dropping it releases the lock.
pub(super) type ModelLock = crate::lock::FileLock;

/// Locks the model folder `model_dir`, creating it if needed. `Ok(None)`
/// when another download, import or removal holds the lock.
///
/// A file system without locks is logged and treated as locked: the model
/// can still be installed there, without the guard.
pub(super) fn try_lock(model_dir: &Path) -> io::Result<Option<ModelLock>> {
    crate::lock::try_lock(&model_dir.join(LOCK_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_holder_at_a_time() {
        let models_dir = tempfile::tempdir().unwrap();
        let model_dir = models_dir.path().join("dinov2-small");
        let held = try_lock(&model_dir).unwrap().expect("a free lock");
        assert!(try_lock(&model_dir).unwrap().is_none());
        drop(held);
        // Released on drop, and the lock file stays for the next holder.
        assert!(model_dir.join(LOCK_FILE).is_file());
        assert!(try_lock(&model_dir).unwrap().is_some());
    }
}
