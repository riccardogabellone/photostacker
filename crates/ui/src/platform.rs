//! What the app asks of the desktop: the folder and file pickers, and showing a path in
//! the file manager. Behind a trait, so tests script the answers instead of opening
//! dialogs.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::format::openable_path;

/// What a file dialog gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Picked {
    /// The user chose this path.
    Path(PathBuf),
    /// The user closed the dialog without choosing.
    Cancelled,
    /// The desktop has no dialog to show, so nothing could be chosen.
    Unavailable,
}

/// The desktop's dialogs and file manager.
pub trait Platform: Send + Sync {
    /// Asks for a folder, starting in `start`.
    fn pick_folder(&self, start: Option<&Path>) -> Picked;
    /// Asks for a file, starting in `start`.
    fn pick_file(&self, start: Option<&Path>) -> Picked;
    /// Shows `path` in the file manager: a file selected in its folder, or a folder opened.
    fn reveal(&self, path: &Path) -> Result<(), String>;
}

/// The real desktop: `rfd`'s native dialogs (on Linux through the XDG desktop portal, or
/// `zenity` without one) and `opener`'s file manager calls.
pub struct Native;

impl Platform for Native {
    fn pick_folder(&self, start: Option<&Path>) -> Picked {
        timed(|| dialog(start).pick_folder())
    }

    fn pick_file(&self, start: Option<&Path>) -> Picked {
        timed(|| dialog(start).pick_file())
    }

    /// The path without the Windows verbatim prefix the engine's canonical output paths
    /// carry, which the shell's calls do not take.
    fn reveal(&self, path: &Path) -> Result<(), String> {
        let path = openable_path(path);
        let shown = if path.is_dir() {
            opener::open(&path)
        } else {
            opener::reveal(&path)
        };
        shown.map_err(|error| error.to_string())
    }
}

/// A blocking dialog that opens in `start`, when given.
fn dialog(start: Option<&Path>) -> rfd::FileDialog {
    let dialog = rfd::FileDialog::new();
    match start {
        Some(folder) => dialog.set_directory(folder),
        None => dialog,
    }
}

/// How soon a dialog that returns nothing must have returned to count as none shown. On
/// Linux rfd asks the XDG desktop portal, then `zenity`, and with neither (WSLg has
/// neither by default) it logs an error and returns nothing, as it does for a dialog the
/// user cancelled. Nobody cancels a dialog this fast, so nothing this soon means that no
/// dialog was shown.
const NO_DIALOG_WITHIN: Duration = Duration::from_millis(200);

/// Whether a dialog that returns nothing may mean the desktop has none: on Linux only,
/// where rfd's dialogs come from the desktop. Windows and macOS always have theirs, so
/// nothing there is a cancel.
const DIALOGS_MAY_BE_MISSING: bool = cfg!(target_os = "linux");

/// What `dialog`, a call that blocks while its dialog is open, gave, by what it returned
/// and how long it took.
fn timed(dialog: impl FnOnce() -> Option<PathBuf>) -> Picked {
    let opened = Instant::now();
    let answer = dialog();
    picked(answer, opened.elapsed(), DIALOGS_MAY_BE_MISSING)
}

/// What a dialog that returned `answer` after `took` gave: a path is a path; nothing is a
/// cancel, unless `may_be_missing` (a desktop that may have no dialog) and it came within
/// [`NO_DIALOG_WITHIN`].
fn picked(answer: Option<PathBuf>, took: Duration, may_be_missing: bool) -> Picked {
    match answer {
        Some(path) => Picked::Path(path),
        None if may_be_missing && took < NO_DIALOG_WITHIN => Picked::Unavailable,
        None => Picked::Cancelled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_at_once_is_no_dialog_and_nothing_later_is_a_cancel() {
        let fast = Duration::from_millis(1);
        let slow = Duration::from_secs(2);
        assert_eq!(picked(None, fast, true), Picked::Unavailable);
        assert_eq!(picked(None, slow, true), Picked::Cancelled);
        let chosen = PathBuf::from("/photos/trip");
        assert_eq!(
            picked(Some(chosen.clone()), fast, true),
            Picked::Path(chosen.clone())
        );
        // Where the desktop always has its dialogs, nothing is always a cancel.
        assert_eq!(picked(None, fast, false), Picked::Cancelled);
        assert_eq!(
            picked(Some(chosen.clone()), slow, false),
            Picked::Path(chosen)
        );
    }

    #[test]
    fn a_dialog_call_is_timed() {
        let none_at_once = timed(|| None);
        let expected = if DIALOGS_MAY_BE_MISSING {
            Picked::Unavailable
        } else {
            Picked::Cancelled
        };
        assert_eq!(none_at_once, expected);
        assert_eq!(
            timed(|| Some(PathBuf::from("/photos"))),
            Picked::Path(PathBuf::from("/photos"))
        );
    }
}
