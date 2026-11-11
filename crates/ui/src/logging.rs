//! The app's log (spec §7.1): a file a day in the logs folder, `photostacker.<date>.log`
//! (the date in UTC, as `tracing-appender` names it), the last 7 kept, written by
//! `tracing-appender`'s worker thread so a log line never waits for the disk; and stderr
//! too in a debug build. A release build on Windows has no console, so the file is where
//! its errors and panics go. The filter is `RUST_LOG`, else [`DEFAULT_FILTER`]; the `log`
//! records of the dependencies (rawler's) are bridged, as in the CLI.
//!
//! A logs folder that cannot be used leaves the log on stderr only: the app starts all the
//! same. `file_writer`, `filter` and `subscriber` are the pieces [`init`] puts together,
//! so the tests build the same subscriber without installing it for the process.

use std::fs;
use std::panic::{self, PanicHookInfo};
use std::path::Path;
use std::sync::Once;

use photostacker_core::{AppDirs, guard};
use tracing::Subscriber;
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_log::{AsLog as _, LogTracer, log};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt as _;

/// The filter when `RUST_LOG` gives none: what the app does, without rawler's own records
/// (a RAW it cannot read is the file's error, logged by the core).
pub const DEFAULT_FILTER: &str = "info,rawler=off";
/// The log files are `<prefix>.<date>.<suffix>`.
const FILE_PREFIX: &str = "photostacker";
const FILE_SUFFIX: &str = "log";
/// How many days of log files are kept: an older one goes when a new one is made.
const KEPT_FILES: usize = 7;

/// The writer of the day's log file in `logs`, which it makes when it is not there, and
/// the guard that writes out what waits when it is dropped; why not, when the folder
/// cannot be used.
pub fn file_writer(logs: &Path) -> Result<(NonBlocking, WorkerGuard), String> {
    // Made first: the appender looks for old files to remove before it makes the folder,
    // and says on stderr that it found none.
    fs::create_dir_all(logs).map_err(|error| error.to_string())?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(FILE_PREFIX)
        .filename_suffix(FILE_SUFFIX)
        .max_log_files(KEPT_FILES)
        .build(logs)
        .map_err(|error| error.to_string())?;
    Ok(tracing_appender::non_blocking(appender))
}

/// The filter: `rust_log`'s directives when it has some and they can be read, else
/// [`DEFAULT_FILTER`]; and, when `rust_log` was not used, why.
fn filter(rust_log: Option<&str>) -> (EnvFilter, Option<String>) {
    let default = || EnvFilter::new(DEFAULT_FILTER);
    match rust_log
        .map(str::trim)
        .filter(|directives| !directives.is_empty())
    {
        None => (default(), None),
        Some(directives) => match EnvFilter::try_new(directives) {
            Ok(filter) => (filter, None),
            Err(error) => (default(), Some(error.to_string())),
        },
    }
}

/// The app's subscriber: `filter` over a line per event in `file`, without colours, and
/// on stderr when `stderr`.
fn subscriber(
    filter: EnvFilter,
    file: Option<NonBlocking>,
    stderr: bool,
) -> impl Subscriber + Send + Sync {
    let file = file.map(|writer| fmt::layer().with_ansi(false).with_writer(writer));
    let stderr = stderr.then(|| fmt::layer().with_writer(std::io::stderr));
    tracing_subscriber::registry()
        .with(filter)
        .with(file)
        .with(stderr)
}

/// Logs, for the whole process, to the day's file in `logs` and, in a debug build or
/// when there is no file (no folder for the app's data, or one that cannot be used), to
/// stderr; filtered by `RUST_LOG`, or [`DEFAULT_FILTER`] when it is not set or cannot be
/// read (which the log says). Call once, before anything is logged. The guard writes out
/// what waits for the file when it is dropped: keep it until the app ends.
pub fn init(logs: Option<&Path>) -> Option<WorkerGuard> {
    let rust_log = std::env::var(EnvFilter::DEFAULT_ENV).ok();
    let (filter, unread) = filter(rust_log.as_deref());
    // A `log` record more verbose than anything the filter enables is not even built.
    let log_level = filter
        .max_level_hint()
        .map_or(log::LevelFilter::Trace, |level| level.as_log());
    let (writer, guard, unusable) = match logs.map(file_writer) {
        Some(Ok((writer, guard))) => (Some(writer), Some(guard), None),
        Some(Err(error)) => (None, None, Some(error)),
        None => (None, None, None),
    };
    let stderr = cfg!(debug_assertions) || writer.is_none();
    // Nothing else sets a logger or a subscriber in this binary; should one have, the app
    // starts all the same, and says so where it can.
    if let Err(error) = LogTracer::builder().with_max_level(log_level).init() {
        eprintln!("the log records of the dependencies are not logged: {error}");
    }
    if let Err(error) = tracing::subscriber::set_global_default(subscriber(filter, writer, stderr))
    {
        eprintln!("nothing is logged: {error}");
    }
    if let Some(error) = unread {
        tracing::warn!(
            "RUST_LOG={:?} cannot be read, so the log uses {DEFAULT_FILTER}: {error}",
            rust_log.unwrap_or_default()
        );
    }
    if let (Some(logs), Some(error)) = (logs, unusable) {
        tracing::warn!(
            "the logs folder {} cannot be used, so the log goes to stderr only: {error}",
            logs.display()
        );
    }
    guard
}

/// Logs what this run is, first: the app's version, its data and logs folders, and
/// whether the build has the visual check (`ml`) and HEIC (`heic`).
pub fn log_the_start(dirs: Option<&AppDirs>) {
    tracing::info!("Photostacker {} is starting", env!("CARGO_PKG_VERSION"));
    match dirs {
        Some(dirs) => tracing::info!(
            "data folder {}, logs folder {}",
            dirs.data.display(),
            dirs.logs().display()
        ),
        None => tracing::warn!("there is no folder for the app's data"),
    }
    tracing::info!(
        "built with ml: {}, heic: {}",
        cfg!(feature = "ml"),
        cfg!(feature = "heic")
    );
}

/// Installs the app's panic hooks; call it first, before anything else sets one. A panic
/// outside the core's guard is logged at error, with its location and message,
/// before it reaches the hook it replaced (which prints it to stderr): a release build on
/// Windows has no console. The core's hook ([`guard::install_panic_hook`]) is installed
/// around it, so it sees a panic first and keeps one inside [`guard::catch`] quiet (logged
/// at debug only). Only the first call installs anything.
pub fn install_panic_hook() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            log_panic(info);
            previous(info);
        }));
        // Last, so it wraps the hook above.
        guard::install_panic_hook();
    });
}

/// Logs a panic at error: where it happened and its message. Not its thread's name, so
/// the hook adds no call of its own (`thread::current`) that can panic while a thread's
/// locals are destroyed, where a panic in a panic hook aborts the process. The logging
/// call itself keeps a thread-local buffer, so that risk remains for a panic during a
/// thread's teardown; it is accepted: such a panic is rare, and the line is worth it.
fn log_panic(info: &PanicHookInfo) {
    let message = guard::message(info.payload());
    let location = info
        .location()
        .map_or_else(|| "unknown".to_owned(), ToString::to_string);
    tracing::error!(%location, "panicked: {message}");
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::panic;
    use std::path::PathBuf;
    use std::thread;

    use photostacker_core::guard;
    use tracing::Dispatch;

    use super::*;

    /// The log files in `logs`, by name.
    fn log_files(logs: &Path) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = fs::read_dir(logs)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        files.sort();
        files
    }

    /// The one log file in `logs`, which must be named `photostacker.<date>.log`; its text.
    fn the_days_file(logs: &Path) -> String {
        let files = log_files(logs);
        let [file] = files.as_slice() else {
            panic!("one log file, not {files:?}");
        };
        let name = file.file_name().unwrap().to_str().unwrap();
        let date = name
            .strip_prefix("photostacker.")
            .and_then(|rest| rest.strip_suffix(".log"))
            .unwrap_or_else(|| panic!("{name} is not photostacker.<date>.log"));
        // YYYY-MM-DD
        let shape: Vec<bool> = date.chars().map(|c| c.is_ascii_digit()).collect();
        let mut expected = vec![true; 10];
        expected[4] = false;
        expected[7] = false;
        assert_eq!(shape, expected, "{name}");
        fs::read_to_string(file).unwrap()
    }

    #[test]
    fn a_log_line_reaches_the_days_file() {
        let data = tempfile::tempdir().unwrap();
        // Not there yet: the writer makes it.
        let logs = data.path().join("logs");
        let (writer, guard) = file_writer(&logs).expect("the folder can be used");
        let (filter, rejected) = filter(None);
        assert_eq!(rejected, None);
        tracing::subscriber::with_default(subscriber(filter, Some(writer), false), || {
            tracing::info!("a line for the day's file");
            tracing::debug!("a line the default filter leaves out");
        });
        // Dropping the guard writes out what is waiting.
        drop(guard);
        let text = the_days_file(&logs);
        let line = text
            .lines()
            .find(|line| line.contains("a line for the day's file"))
            .unwrap_or_else(|| panic!("no line in {text:?}"));
        assert!(line.contains("INFO"), "{line}");
        assert!(!text.contains("leaves out"), "{text}");
        // No colours in a file.
        assert!(!text.contains('\u{1b}'), "{text:?}");
    }

    #[test]
    fn an_unusable_logs_folder_is_no_error() {
        let data = tempfile::tempdir().unwrap();
        let file = data.path().join("a file");
        fs::write(&file, b"not a folder").unwrap();
        // A folder under a regular file can be neither made nor used.
        assert!(file_writer(&file.join("logs")).is_err());
        // The log goes on without a file.
        let (filter, _) = filter(None);
        tracing::subscriber::with_default(subscriber(filter, None, false), || {
            tracing::info!("a line with no file to go to");
        });
    }

    #[test]
    fn rust_log_replaces_the_default_filter_unless_it_cannot_be_read() {
        let directives = |rust_log: Option<&str>| {
            let (filter, rejected) = filter(rust_log);
            (filter.to_string(), rejected.is_some())
        };
        let default = (EnvFilter::new(DEFAULT_FILTER).to_string(), false);
        assert_eq!(directives(None), default);
        assert_eq!(directives(Some("")), default);
        assert_eq!(directives(Some("  ")), default);
        assert_eq!(directives(Some("debug")), ("debug".to_owned(), false));
        let (used, rejected) = directives(Some("photostacker=[{"));
        assert_eq!(used, default.0);
        assert!(rejected);
    }

    #[test]
    fn a_panic_outside_the_guard_reaches_the_log_and_one_inside_it_does_not() {
        const OUTSIDE: &str = "a logging test's panic outside the guard";
        const INSIDE: &str = "a logging test's panic inside the guard";
        // The default hook would print these two; any other panic still reaches it.
        let default = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let message = guard::message(info.payload());
            if message != OUTSIDE && message != INSIDE {
                default(info);
            }
        }));
        install_panic_hook();
        let data = tempfile::tempdir().unwrap();
        let logs = data.path().join("logs");
        let (writer, guard) = file_writer(&logs).expect("the folder can be used");
        // At debug, so the line the core's hook logs for a caught panic is in the file.
        let (filter, _) = filter(Some("debug"));
        let dispatch = Dispatch::new(subscriber(filter, Some(writer), false));

        // The hook runs on the thread that panics, where the subscriber is the default.
        let outside = {
            let dispatch = dispatch.clone();
            thread::spawn(move || {
                tracing::dispatcher::with_default(&dispatch, || panic!("{OUTSIDE}"));
            })
        };
        assert!(outside.join().is_err());
        let inside = thread::spawn(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                guard::catch(|| -> u8 { panic!("{INSIDE}") })
            })
        });
        assert_eq!(inside.join().unwrap(), Err(INSIDE.to_owned()));
        drop(guard);

        let text = the_days_file(&logs);
        let lines = |message: &str| -> Vec<String> {
            text.lines()
                .filter(|line| line.contains(message))
                .map(str::to_owned)
                .collect()
        };
        let outside = lines(OUTSIDE);
        assert!(outside.iter().any(|line| line.contains("ERROR")), "{text}");
        // The core's hook kept it quiet: at debug, never at error.
        let inside = lines(INSIDE);
        assert!(inside.iter().any(|line| line.contains("DEBUG")), "{text}");
        assert!(inside.iter().all(|line| !line.contains("ERROR")), "{text}");
    }
}
