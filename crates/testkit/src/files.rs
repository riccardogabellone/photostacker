//! Files of a given length that cost no disk space, for the tests that only
//! look at a file's size (a model's, or one over the decode budget).

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

/// Creates (or truncates) the file `path` and gives it a length of `len`
/// bytes, all zero, as a sparse file: no disk space is used for them.
///
/// On Unix a file extended by `set_len` is sparse already. NTFS allocates
/// the whole length unless the file is marked sparse first, so on Windows it
/// is, with `fsutil sparse setflag`, which needs no administrator rights and
/// no API beyond the standard library's.
pub fn sparse_file(path: &Path, len: u64) -> io::Result<()> {
    File::create(path)?;
    #[cfg(windows)]
    mark_sparse(path)?;
    OpenOptions::new().write(true).open(path)?.set_len(len)
}

/// Marks the empty file `path` sparse on NTFS.
#[cfg(windows)]
fn mark_sparse(path: &Path) -> io::Result<()> {
    let output = std::process::Command::new("fsutil")
        .args(["sparse", "setflag"])
        .arg(path)
        .output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "fsutil sparse setflag {} failed ({}): {} {}",
        path.display(),
        output.status,
        String::from_utf8_lossy(&output.stdout).trim(),
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// A folder of the test's own, removed when it is dropped, a failing test's too.
    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_sparse_file_has_its_length() {
        let dir =
            Scratch(std::env::temp_dir().join(format!("testkit-sparse-{}", std::process::id())));
        std::fs::create_dir_all(&dir.0).unwrap();
        let path = dir.0.join("big.bin");
        let len = 3 << 30;
        sparse_file(&path, len).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(metadata.len(), len);
        // On Windows the file is marked sparse: NTFS would allocate it whole otherwise.
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;
            assert_ne!(
                metadata.file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE,
                0,
                "{} is not sparse",
                path.display()
            );
        }
        // Again, over the first: it is truncated and extended anew.
        sparse_file(&path, 5).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), [0; 5]);
    }
}
