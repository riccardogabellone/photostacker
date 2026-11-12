//! Symbolic links, for the tests about them, on every system. Windows lets
//! an account create one only with Developer Mode on or as an administrator:
//! where it refuses, the test that wanted the link says so and returns.

use std::io;
use std::path::Path;

/// What a test prints when it cannot run for want of a symbolic link.
const SKIPPED: &str = "skipped: this account may not create symlinks";

/// Makes `link` a symbolic link to the file `target`, which need not exist.
/// `false`, with [`SKIPPED`] printed, when Windows refuses for lack of the
/// privilege: the calling test returns. Any other failure panics.
pub fn symlink_file(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_file(target, link);
    allowed(made, target, link)
}

/// Makes `link` a symbolic link to the folder `target`, which need not
/// exist; as [`symlink_file`] otherwise. Windows tells a link to a folder
/// from a link to a file; Unix does not.
pub fn symlink_dir(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_dir(target, link);
    allowed(made, target, link)
}

/// Whether the link was made: `false` for Windows' `ERROR_PRIVILEGE_NOT_HELD`,
/// after printing [`SKIPPED`]; a panic for any other error.
fn allowed(made: io::Result<()>, target: &Path, link: &Path) -> bool {
    /// `ERROR_PRIVILEGE_NOT_HELD`: the account may not create symbolic links.
    const PRIVILEGE_NOT_HELD: i32 = 1314;
    match made {
        Ok(()) => true,
        Err(error) if cfg!(windows) && error.raw_os_error() == Some(PRIVILEGE_NOT_HELD) => {
            println!("{SKIPPED}");
            false
        }
        Err(error) => panic!(
            "cannot link {} to {}: {error}",
            link.display(),
            target.display()
        ),
    }
}
