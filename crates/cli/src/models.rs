//! `photostacker-cli models` (spec §8.2): the Stage 3 models in the app's models
//! folder (spec §7.1), which the GUI shares. Downloading and importing need
//! the `ml` feature; listing and removing do not.

#[cfg(feature = "ml")]
use std::cell::Cell;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use clap::Subcommand;
use photostacker_core::AppDirs;
use photostacker_core::models::{self, MODELS, ModelSpec};

use crate::Failure;

/// The `models` subcommands.
#[derive(Subcommand)]
pub(crate) enum ModelsCommand {
    /// Lists the models: id, name, size, and whether it is installed.
    List,
    /// Downloads a model and verifies it, resuming an unfinished download;
    /// prints the installed file.
    Download {
        /// The model's id.
        id: String,
    },
    /// Installs a model from a file, for a machine with no network; prints
    /// the installed file. The file is only read.
    Import {
        /// The model's id.
        id: String,
        /// The model file (`model.onnx`).
        file: PathBuf,
    },
    /// Removes a model, and any unfinished download of it.
    Remove {
        /// The model's id.
        id: String,
    },
}

pub(crate) fn models(command: &ModelsCommand) -> Result<(), Failure> {
    match command {
        ModelsCommand::List => print_lines(&list_lines(&models_dir()?)),
        ModelsCommand::Download { id } => download(spec(id)?),
        ModelsCommand::Import { id, file } => import(spec(id)?, file),
        ModelsCommand::Remove { id } => {
            let spec = spec(id)?;
            models::remove(spec, &models_dir()?).map_err(|error| {
                Failure::Runtime(anyhow!(error).context(format!("cannot remove {}", spec.id)))
            })?;
            eprintln!("removed {}", spec.id);
            Ok(())
        }
    }
}

/// Prints each of `lines` to stdout; a stdout that is closed is a failure,
/// not a panic.
fn print_lines(lines: &[String]) -> Result<(), Failure> {
    let mut stdout = io::stdout().lock();
    lines
        .iter()
        .try_for_each(|line| writeln!(stdout, "{line}"))
        .and_then(|()| stdout.flush())
        .context("cannot write to stdout")
        .map_err(Failure::Runtime)
}

/// The app's models folder.
fn models_dir() -> Result<PathBuf, Failure> {
    AppDirs::resolve().map(|dirs| dirs.models()).ok_or_else(|| {
        Failure::Runtime(anyhow!(
            "there is no folder for the app's data; set {}",
            photostacker_core::appdirs::DATA_DIR_ENV
        ))
    })
}

/// The registry entry of `id`.
fn spec(id: &str) -> Result<&'static ModelSpec, Failure> {
    models::model(id).ok_or_else(|| {
        let ids: Vec<&str> = MODELS.iter().map(|spec| spec.id).collect();
        Failure::Usage(format!("no model {id}; the models are {}", ids.join(", ")))
    })
}

/// One line per model of the registry: its id, name, size, and whether it
/// is installed in `dir`.
fn list_lines(dir: &Path) -> Vec<String> {
    let id_width = MODELS.iter().map(|spec| spec.id.len()).max().unwrap_or(0);
    let name_width = MODELS.iter().map(|spec| spec.name.len()).max().unwrap_or(0);
    MODELS
        .iter()
        .map(|spec| {
            let installed = if models::installed(dir, spec) {
                "yes"
            } else {
                "no"
            };
            format!(
                "{:<id_width$}  {:<name_width$}  {:>6.1} MiB  installed: {installed}",
                spec.id,
                spec.name,
                mib(spec.size)
            )
        })
        .collect()
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

#[cfg(not(feature = "ml"))]
fn download(_: &ModelSpec) -> Result<(), Failure> {
    Err(without_ml())
}

#[cfg(not(feature = "ml"))]
fn import(_: &ModelSpec, _: &Path) -> Result<(), Failure> {
    Err(without_ml())
}

#[cfg(not(feature = "ml"))]
fn without_ml() -> Failure {
    Failure::Usage(
        "this photostacker-cli was built without ml: it cannot download or import a model".into(),
    )
}

/// Downloads `spec` into the models folder, with a progress line on stderr
/// every 5 %, and prints the installed file. The first Ctrl+C cancels the
/// download, keeping its part for the next one to resume.
#[cfg(feature = "ml")]
fn download(spec: &ModelSpec) -> Result<(), Failure> {
    let dir = &models_dir()?;
    if models::installed(dir, spec) {
        eprintln!("{} is already installed", spec.id);
        return print_installed(&models::model_file(dir, spec));
    }
    crate::catch_ctrl_c();
    eprintln!(
        "downloading {} ({:.1} MiB, {})",
        spec.id,
        mib(spec.size),
        spec.licence
    );
    let steps = ProgressSteps::default();
    let progress = |done: u64, total: u64| {
        if let Some(percent) = steps.next(done, total) {
            eprintln!(
                "download: {percent}% ({:.1} of {:.1} MiB)",
                mib(done),
                mib(total)
            );
        }
    };
    let file =
        models::install::download(spec, dir, &progress, &crate::INTERRUPTED).map_err(|error| {
            Failure::Runtime(anyhow!(error).context(format!("cannot download {}", spec.id)))
        })?;
    print_installed(&file)
}

/// Imports `spec` from `file` into the models folder, and prints the
/// installed file.
#[cfg(feature = "ml")]
fn import(spec: &ModelSpec, file: &Path) -> Result<(), Failure> {
    let dir = &models_dir()?;
    let installed = models::install::import(spec, file, dir).map_err(|error| {
        Failure::Runtime(anyhow!(error).context(format!("cannot import {}", spec.id)))
    })?;
    print_installed(&installed)
}

#[cfg(feature = "ml")]
fn print_installed(file: &Path) -> Result<(), Failure> {
    print_lines(&[file.display().to_string()])
}

/// Which download progress reports get a line: the first one, then one
/// each time the download passes another multiple of 5 %.
#[cfg(feature = "ml")]
#[derive(Default)]
struct ProgressSteps {
    /// The step last printed, in percent.
    last: Cell<Option<u64>>,
}

#[cfg(feature = "ml")]
impl ProgressSteps {
    /// The percentage to print for `done` of `total` bytes, if it is due.
    fn next(&self, done: u64, total: u64) -> Option<u64> {
        let percent = match total {
            0 => 100,
            total => (done.min(total) * 100 / total) / 5 * 5,
        };
        let due = self.last.get().is_none_or(|last| percent > last);
        due.then(|| {
            self.last.set(Some(percent));
            percent
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "ml")]
    #[test]
    fn progress_lines_come_every_5_percent() {
        let steps = ProgressSteps::default();
        let printed: Vec<u64> = (0..=1000)
            .filter_map(|done| steps.next(done, 1000))
            .collect();
        let expected: Vec<u64> = (0..=100).step_by(5).collect();
        assert_eq!(printed, expected);
    }

    /// A resumed download starts where its part ended.
    #[cfg(feature = "ml")]
    #[test]
    fn a_resumed_download_starts_at_its_step() {
        let steps = ProgressSteps::default();
        assert_eq!(steps.next(437, 1000), Some(40));
        assert_eq!(steps.next(449, 1000), None);
        assert_eq!(steps.next(450, 1000), Some(45));
    }

    #[test]
    fn list_shows_the_installed_model() {
        let dir = tempfile::tempdir().unwrap();
        let spec = &MODELS[1];
        let file = models::model_file(dir.path(), spec);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::File::create(&file)
            .unwrap()
            .set_len(spec.size)
            .unwrap();
        let lines = list_lines(dir.path());
        assert!(lines[0].ends_with("installed: no"), "{lines:?}");
        assert!(lines[1].ends_with("installed: yes"), "{lines:?}");
        assert!(lines[1].contains("330.5 MiB"), "{lines:?}");
    }
}
