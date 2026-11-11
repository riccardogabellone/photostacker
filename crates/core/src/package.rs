//! Where a package keeps the files the app needs besides its executables
//! (spec §8.3): ONNX Runtime's library and the licences that go with it.
//!
//! On Windows they sit beside the executable. A Linux package (the Debian
//! package, the AppImage) has the executables in `usr/bin` and the other
//! files in `usr/lib/<main binary>`; a macOS app bundle has the executables
//! in `Contents/MacOS`, its libraries in `Contents/Frameworks` and its other
//! files in `Contents/Resources`. A development build finds them beside the
//! executable too, on every system.
//!
//! The folders are joined, never resolved: one that does not exist is
//! simply not where a file is found.

use std::env;
use std::path::{Path, PathBuf};

/// The folder of `usr/lib` that holds a Linux package's other files: named
/// after the package's main binary, the desktop app's.
const LINUX_PACKAGE_FOLDER: &str = "Photostacker";

/// The running executable's folder; `None` when the system cannot say where
/// it is.
pub fn exe_dir() -> Option<PathBuf> {
    folder_of(&env::current_exe().ok()?)
}

/// The folder of the executable at `exe`. On Unix a link to it (such as one
/// to the CLI in a folder on the `PATH`) is resolved first, so the package's
/// folders are found from the real file; a path that cannot be resolved is
/// taken as it is. On Windows it is taken as it is, which keeps the `\\?\`
/// prefix of a resolved path out of the paths the app shows.
fn folder_of(exe: &Path) -> Option<PathBuf> {
    #[cfg(unix)]
    let resolved = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    #[cfg(not(unix))]
    let resolved = exe.to_path_buf();
    resolved.parent().map(Path::to_path_buf)
}

/// The folders a shared library of the package may be in, in the order they
/// are looked in: `exe_dir`, then on Linux `exe_dir/../lib/Photostacker`, on
/// macOS `exe_dir/../Frameworks`.
pub fn library_folders(exe_dir: &Path) -> Vec<PathBuf> {
    let mut folders = vec![exe_dir.to_path_buf()];
    if cfg!(target_os = "linux") {
        folders.push(linux_package_folder(exe_dir));
    }
    if cfg!(target_os = "macos") {
        folders.push(exe_dir.join("..").join("Frameworks"));
    }
    folders
}

/// The folders the package's other files (the licences) may be in, in the
/// order they are looked in: `exe_dir`, then on Linux
/// `exe_dir/../lib/Photostacker`, on macOS `exe_dir/../Resources`.
pub fn resource_folders(exe_dir: &Path) -> Vec<PathBuf> {
    let mut folders = vec![exe_dir.to_path_buf()];
    if cfg!(target_os = "linux") {
        folders.push(linux_package_folder(exe_dir));
    }
    if cfg!(target_os = "macos") {
        folders.push(exe_dir.join("..").join("Resources"));
    }
    folders
}

/// `usr/lib/Photostacker` of a Linux package whose executables are in
/// `exe_dir`, `usr/bin`.
fn linux_package_folder(exe_dir: &Path) -> PathBuf {
    exe_dir.join("..").join("lib").join(LINUX_PACKAGE_FOLDER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_folder_beside_the_executable_comes_first() {
        let exe_dir = Path::new("/opt/photostacker/bin");
        assert_eq!(library_folders(exe_dir)[0], exe_dir);
        assert_eq!(resource_folders(exe_dir)[0], exe_dir);
        if cfg!(windows) {
            assert_eq!(library_folders(exe_dir).len(), 1);
            assert_eq!(resource_folders(exe_dir).len(), 1);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_linux_package_keeps_its_libraries_under_lib_photostacker() {
        let exe_dir = Path::new("/usr/bin");
        let packaged = PathBuf::from("/usr/bin/../lib/Photostacker");
        assert_eq!(
            library_folders(exe_dir),
            [exe_dir.to_path_buf(), packaged.clone()]
        );
        assert_eq!(resource_folders(exe_dir), [exe_dir.to_path_buf(), packaged]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_mac_app_keeps_its_libraries_in_frameworks_and_its_files_in_resources() {
        let exe_dir = Path::new("/Applications/Photostacker.app/Contents/MacOS");
        assert_eq!(
            library_folders(exe_dir),
            [
                exe_dir.to_path_buf(),
                PathBuf::from("/Applications/Photostacker.app/Contents/MacOS/../Frameworks"),
            ]
        );
        assert_eq!(
            resource_folders(exe_dir),
            [
                exe_dir.to_path_buf(),
                PathBuf::from("/Applications/Photostacker.app/Contents/MacOS/../Resources"),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_the_executable_finds_the_real_folder() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("package").join("bin");
        let links = root.path().join("links");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(&links).unwrap();
        std::fs::write(real.join("tool"), b"").unwrap();
        std::os::unix::fs::symlink(real.join("tool"), links.join("tool")).unwrap();

        // The temporary folder may itself be behind a link (`/var` on macOS).
        let expected = std::fs::canonicalize(&real).unwrap();
        assert_eq!(folder_of(&links.join("tool")), Some(expected));
    }

    #[test]
    fn the_running_executable_has_a_folder() {
        let exe_dir = exe_dir().expect("the test's own executable");
        assert!(exe_dir.is_dir(), "{}", exe_dir.display());
    }
}
