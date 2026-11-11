//! Showing a folder or a file in the file manager, on a thread of its own.
//!
//! The desktop's call can block for a long time when nothing answers it: on Linux it asks
//! the file manager over D-Bus, and waits out D-Bus's timeout when there is none (WSLg has
//! none). So it runs neither on the UI thread nor on the backend's, where it would hold up
//! every engine request queued behind it. Each reveal gets a short-lived thread, which
//! also looks at the path first (the UI thread touches no file) and reports what went
//! wrong as a notice, which [`Controller::poll`] takes in. The window of a start that
//! failed, which has no controller, reveals the same way ([`reveal_in_the_background`]).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use super::{Controller, Notice, NoticeLevel};
use crate::format::display_path;
use crate::platform::Platform;

/// The name of a reveal's thread.
pub const REVEAL_THREAD: &str = "photostacker-ui-reveal";

/// What is shown, which says what the user is told when it is not there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reveal {
    /// A job's output folder, which does not exist until its first stack is written.
    OutputFolder,
    /// A photo or a written stack.
    File,
    /// A folder of the app's own, such as the data folder a start that failed shows,
    /// which may never have been made.
    Folder,
}

impl Controller {
    /// Shows `path` in the file manager, on a thread of its own; what went wrong comes
    /// back as a notice.
    pub fn reveal(&mut self, path: &Path, what: Reveal) {
        let posted = self.post.clone();
        let wake = Arc::clone(&self.wake);
        let spawned = reveal_in_the_background(
            Arc::clone(&self.platform),
            path.to_path_buf(),
            what,
            move |notice| {
                // Nothing receives once the controller is gone: nobody is left to tell.
                if posted.send(notice).is_ok() {
                    wake();
                }
            },
        );
        if let Err(error) = spawned {
            let text = format!("Cannot show {}: {error}", display_path(path));
            self.notify(NoticeLevel::Error, text);
        }
    }
}

/// Shows `path` through `platform` on a short-lived thread, [`REVEAL_THREAD`], which
/// hands what went wrong, if anything, to `report`; the error when the thread cannot be
/// started.
pub fn reveal_in_the_background(
    platform: Arc<dyn Platform>,
    path: PathBuf,
    what: Reveal,
    report: impl FnOnce(Notice) + Send + 'static,
) -> io::Result<()> {
    thread::Builder::new()
        .name(REVEAL_THREAD.into())
        .spawn(move || {
            if let Some(notice) = show(platform.as_ref(), &path, what) {
                report(notice);
            }
        })
        .map(drop)
}

/// Shows `path` through `platform`, or says why it is not shown.
fn show(platform: &dyn Platform, path: &Path, what: Reveal) -> Option<Notice> {
    let shown = display_path(path);
    // A path that cannot be looked at is left to the file manager, which says why.
    if matches!(path.try_exists(), Ok(false)) {
        return Some(match what {
            Reveal::OutputFolder => Notice {
                level: NoticeLevel::Info,
                text: format!("Nothing is written yet: the stacks will go to {shown}."),
            },
            Reveal::File => Notice {
                level: NoticeLevel::Warning,
                text: format!("{shown} is no longer there."),
            },
            Reveal::Folder => Notice {
                level: NoticeLevel::Warning,
                text: format!("{shown} does not exist."),
            },
        });
    }
    platform.reveal(path).err().map(|message| Notice {
        level: NoticeLevel::Error,
        text: format!("Cannot show {shown}: {message}"),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::platform::Picked;

    /// A file manager that records what it is asked to show.
    #[derive(Default)]
    struct Recording(Mutex<Vec<PathBuf>>);

    impl Platform for Recording {
        fn pick_folder(&self, _start: Option<&Path>) -> Picked {
            Picked::Cancelled
        }

        fn pick_file(&self, _start: Option<&Path>) -> Picked {
            Picked::Cancelled
        }

        fn reveal(&self, path: &Path) -> Result<(), String> {
            self.0.lock().unwrap().push(path.to_path_buf());
            Ok(())
        }
    }

    #[test]
    fn a_folder_that_was_never_made_does_not_exist() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("never made");
        let platform = Recording::default();
        let notice = show(&platform, &missing, Reveal::Folder);
        assert_eq!(
            notice,
            Some(Notice {
                level: NoticeLevel::Warning,
                text: format!("{} does not exist.", display_path(&missing)),
            })
        );
        assert!(platform.0.lock().unwrap().is_empty());
        // One that is there is shown.
        assert_eq!(show(&platform, root.path(), Reveal::Folder), None);
        assert_eq!(*platform.0.lock().unwrap(), vec![root.path().to_path_buf()]);
    }
}
