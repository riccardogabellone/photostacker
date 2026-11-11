//! The `photostacker-cli` command line (spec §10).
//!
//! - `run` (`run.rs`) analyses a folder through the engine and writes every
//!   proposed group as a layered file; with `--dry-run` it writes only the
//!   plan of the groups.
//! - `models` (`models.rs`) lists, downloads, imports and removes the
//!   Stage 3 models, in the app's models folder.
//!
//! Exit codes: 0 on success (per-file errors are in the plan,
//! and named on stderr when the run writes; a skipped stack is not a
//! failure), 2 for a usage or config error, 1 when the command fails after
//! it started, is cancelled, or any stack fails to be written; 130 when a
//! second Ctrl+C ends it at once.

mod models;
mod run;

use std::io;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::{Parser, Subcommand};
use photostacker_core::guard::install_panic_hook;
use tracing_log::{AsLog, LogTracer, log};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;

use crate::models::ModelsCommand;
use crate::run::RunArgs;

/// The exit code of a usage or config error; clap uses it too.
const USAGE_EXIT_CODE: u8 = 2;
/// The exit code of a command that started but could not finish.
const RUNTIME_EXIT_CODE: u8 = 1;
/// The exit code of a second Ctrl+C, the shell's for an end by SIGINT.
const INTERRUPTED_EXIT_CODE: i32 = 130;
/// The log filter without `RUST_LOG`: warnings, and nothing of rawler's.
/// rawler logs what it recovers from, at error level too (a file too short
/// to sniff, an I/O error with a backtrace, a missing TIFF tag it defaults),
/// and what that costs a file the core reports itself (spec §10).
const DEFAULT_LOG_FILTER: &str = "warn,rawler=off";

#[derive(Parser)]
#[command(
    name = "photostacker-cli",
    version,
    about = "Stacks photo series into layered files."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Groups the photos of a folder and writes each group as a stack.
    Run(RunArgs),
    /// Lists, downloads, imports or removes the Stage 3 models.
    #[command(subcommand)]
    Models(ModelsCommand),
}

/// Why a command did not succeed.
enum Failure {
    /// Bad arguments or config: nothing was done.
    Usage(String),
    /// The command started but could not finish.
    Runtime(anyhow::Error),
}

impl Failure {
    /// Prints the failure to stderr and gives its exit code.
    fn report(self) -> ExitCode {
        match self {
            Self::Usage(message) => {
                eprintln!("error: {message}");
                ExitCode::from(USAGE_EXIT_CODE)
            }
            Self::Runtime(error) => {
                // `{:#}` prints the whole cause chain on one line.
                eprintln!("error: {error:#}");
                ExitCode::from(RUNTIME_EXIT_CODE)
            }
        }
    }
}

fn main() -> ExitCode {
    // A panic the core catches in one file's work is logged, not printed.
    install_panic_hook();
    // clap exits with code 2 on a usage error, and 0 after --help.
    let cli = Cli::parse();
    init_tracing();
    let result = match &cli.command {
        Command::Run(args) => {
            catch_ctrl_c();
            run::run(args)
        }
        Command::Models(command) => models::models(command),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => failure.report(),
    }
}

/// Set by the first Ctrl+C: the running job or download is cancelled, and
/// the command ends once it has stopped and cleaned up.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Whether Ctrl+C was pressed (see [`catch_ctrl_c`]).
fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// From now on the first Ctrl+C sets [`INTERRUPTED`], and a second one
/// ends the process at once with code 130, leaving whatever it was doing.
/// Without the handler (it cannot be installed) Ctrl+C ends the process.
fn catch_ctrl_c() {
    // The handler runs on a thread of its own, not in the signal handler,
    // so it may print and exit.
    let installed = ctrlc::set_handler(|| {
        if INTERRUPTED.swap(true, Ordering::SeqCst) {
            std::process::exit(INTERRUPTED_EXIT_CODE);
        }
        eprintln!("cancelling: press Ctrl+C again to quit at once");
    });
    if let Err(error) = installed {
        eprintln!("warning: cannot catch Ctrl+C: {error}");
    }
}

/// Logs go to stderr, filtered by [`log_filter`]. The `log` records of the
/// dependencies (rawler's) go there too.
fn init_tracing() {
    let rust_log = std::env::var(EnvFilter::DEFAULT_ENV).unwrap_or_default();
    let filter = log_filter(&rust_log);
    // A `log` record more verbose than anything the filter enables is not
    // even built: its arguments are never evaluated. The limit is the whole
    // filter's, so at the default `warn` rawler's warning and error records
    // (an I/O error's backtrace among them) are still built, then dropped by
    // `rawler=off`.
    let log_level = filter
        .max_level_hint()
        .map_or(log::LevelFilter::Trace, |level| level.as_log());
    // Nothing else sets a `log` logger in this binary.
    LogTracer::builder()
        .with_max_level(log_level)
        .init()
        .expect("the log bridge is installed once");
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .init();
}

/// The log filter: `rust_log`'s directives, or [`DEFAULT_LOG_FILTER`] when
/// it has none. Invalid directives are left out with a note on stderr, and
/// when none is valid the filter is `warn`.
fn log_filter(rust_log: &str) -> EnvFilter {
    let directives = if rust_log.trim().is_empty() {
        DEFAULT_LOG_FILTER
    } else {
        rust_log
    };
    EnvFilter::builder()
        .with_default_directive(LevelFilter::WARN.into())
        .parse_lossy(directives)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    /// `RUST_LOG` replaces the default filter whole.
    #[test]
    fn log_filter_defaults_to_warn_with_rawler_off() {
        let directives = |rust_log: &str| {
            let mut directives: Vec<String> = log_filter(rust_log)
                .to_string()
                .split(',')
                .map(str::to_owned)
                .collect();
            directives.sort();
            directives
        };
        assert_eq!(directives(""), ["rawler=off", "warn"]);
        assert_eq!(directives("  "), ["rawler=off", "warn"]);
        assert_eq!(
            directives("photostacker_core=debug"),
            ["photostacker_core=debug"]
        );
    }
}
