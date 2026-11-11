//! Paths as BLOBs: the platform's own path units, so a path that is not
//! valid Unicode round-trips exactly. The store keeps a path as it was
//! given, never canonicalised: the writer compares a file's path with the
//! input folder as spelled in the job's config.
//!
//! - Unix: the path's bytes.
//! - Windows: its UTF-16 code units (unpaired surrogates included), each as
//!   two little-endian bytes.

use std::path::{Path, PathBuf};

#[cfg(unix)]
pub fn to_blob(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(unix)]
pub fn from_blob(blob: &[u8]) -> PathBuf {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(OsStr::from_bytes(blob))
}

#[cfg(windows)]
pub fn to_blob(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

/// A trailing odd byte, which [`to_blob`] never writes, is ignored.
#[cfg(windows)]
pub fn from_blob(blob: &[u8]) -> PathBuf {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    let units: Vec<u16> = blob
        .as_chunks::<2>()
        .0
        .iter()
        .map(|unit| u16::from_le_bytes(*unit))
        .collect();
    PathBuf::from(OsString::from_wide(&units))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_path_round_trips() {
        let path = Path::new("/photos/città/IMG_0001 (1).jpg");
        assert_eq!(from_blob(&to_blob(path)), path);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_path_round_trips() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(OsStr::from_bytes(b"a\xffb.jpg"));
        assert_eq!(to_blob(path), b"a\xffb.jpg");
        assert_eq!(from_blob(&to_blob(path)), path);
    }

    #[cfg(windows)]
    #[test]
    fn unpaired_surrogate_round_trips() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        let path = PathBuf::from(OsString::from_wide(&[0x61, 0xD800, 0x62]));
        assert_eq!(to_blob(&path), [0x61, 0, 0x00, 0xD8, 0x62, 0]);
        assert_eq!(from_blob(&to_blob(&path)), path);
    }
}
