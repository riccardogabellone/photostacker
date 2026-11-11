//! The models of Stage 3 (spec §8.2) and the state of the visual check: what the ML chip,
//! the status bar, the new-job form and the models' card of Settings show, and the
//! commands on the models.
//!
//! The models are read once at start, before the first frame, then through the backend
//! at each `ModelChanged`; a `ModelDownloadProgress` moves its model's download on in
//! place, and a `ModelFailed` is kept for the model's row until the next command on that
//! model. A command never reads its own result: its events do. Nothing here touches the
//! network: only the engine's download does, and only `download_model` asks for one.

use std::collections::HashMap;
use std::path::PathBuf;

use photostacker_core::engine::{ModelActivity, ModelView};
use photostacker_core::models::{self, MODELS, ModelSpec};
use photostacker_core::{Engine, EngineError};

use super::{Controller, NoticeLevel};
use crate::backend::Request;
use crate::format::{download_percent, sentence};
use crate::platform::Picked;

/// What the notice says when the desktop has no file dialog to pick a model file with.
const NO_FILE_DIALOG: &str =
    "No file dialog is available on this desktop, so a model file cannot be chosen here.";

#[cfg(feature = "ml")]
pub use photostacker_core::models::runtime::{LibraryProblem, OsVersion};
#[cfg(not(feature = "ml"))]
pub use without_ml::{LibraryProblem, OsVersion};

/// Where ONNX Runtime's library was looked for at start, and what keeps it from loading,
/// if anything. `main` looks once, before the first frame ([`RuntimeCheck::look`]); tests
/// and the screenshots say what they want, so none depends on the library of the machine
/// it runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCheck {
    pub path: PathBuf,
    /// `None` when the library is there and nothing is known against it.
    pub problem: Option<LibraryProblem>,
}

impl RuntimeCheck {
    /// Looks for the library where the embedder loads it from, and at what it needs (spec
    /// §8.3): the file system's answer, and on macOS a few kilobytes of the library's
    /// headers and the system's version file, for `main` before the first frame. A build
    /// without the `ml` feature has no library to look for.
    pub fn look() -> RuntimeCheck {
        #[cfg(feature = "ml")]
        {
            let path = models::runtime::library_path();
            let problem = models::runtime::library_problem(&path);
            RuntimeCheck { path, problem }
        }
        #[cfg(not(feature = "ml"))]
        RuntimeCheck {
            path: PathBuf::new(),
            problem: Some(LibraryProblem::Missing),
        }
    }
}

/// The core's types of ONNX Runtime's check, which a build without `ml` does not have:
/// the same shapes, so the controller, the views and the tests are written once for both
/// builds. Without `ml` the state is `NotBuilt` before the check is asked, so these only
/// ever hold what a test puts in them.
#[cfg(not(feature = "ml"))]
mod without_ml {
    use std::fmt;

    /// A version of macOS, as the core's `models::runtime::OsVersion`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct OsVersion {
        pub major: u32,
        pub minor: u32,
        pub patch: u32,
    }

    impl fmt::Display for OsVersion {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}.{}", self.major, self.minor)?;
            if self.patch != 0 {
                write!(f, ".{}", self.patch)?;
            }
            Ok(())
        }
    }

    /// Why ONNX Runtime's library cannot be loaded, as the core's
    /// `models::runtime::LibraryProblem`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum LibraryProblem {
        Missing,
        NeedsMacos {
            needed: OsVersion,
            running: OsVersion,
        },
    }
}

/// Whether new jobs are checked by their look, and with what: the first that holds of a
/// build without the visual check, ONNX Runtime missing (or needing a newer macOS), the
/// switch off, and then the active model's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MlState {
    /// This build has no `ml` feature.
    NotBuilt,
    /// The switch is off.
    Off,
    /// ONNX Runtime cannot be loaded, as this check at start found: it is missing, or it
    /// needs a newer macOS.
    NoRuntime(RuntimeCheck),
    /// The active model is not installed (also while it is imported, or its download is
    /// cancelled).
    Missing(&'static str),
    /// The active model is being downloaded, this far.
    Downloading { id: &'static str, percent: u8 },
    /// The active model is installed.
    Ready(&'static str),
}

/// The last failure of a model, kept until the next command on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFailure {
    /// What went wrong, as a sentence.
    pub text: String,
    /// It was a download's, which `Try again` starts again.
    pub download: bool,
}

/// What a command on a model was, so its failure knows whether `Try again` downloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attempt {
    Download,
    Import,
    Remove,
}

/// What the controller knows of the models of Stage 3.
pub(super) struct Models {
    runtime: RuntimeCheck,
    /// As last read, in the registry's order.
    views: Vec<ModelView>,
    /// The last command on each model.
    attempts: HashMap<&'static str, Attempt>,
    failures: HashMap<&'static str, ModelFailure>,
    /// The visual check's card is to be scrolled into view at its next drawing.
    reveal: bool,
}

impl Models {
    /// The models as `views`, read at start, and ONNX Runtime as `runtime` found it.
    pub(super) fn new(runtime: RuntimeCheck, views: Vec<ModelView>) -> Self {
        Self {
            runtime,
            views,
            attempts: HashMap::new(),
            failures: HashMap::new(),
            reveal: false,
        }
    }

    /// Takes in a read of the models. A download's progress that an event showed past the
    /// read's stays: a read made just before a progress event can be taken in after it.
    pub(super) fn read(&mut self, mut views: Vec<ModelView>) {
        for view in &mut views {
            let ModelActivity::Downloading { done, total } = view.activity else {
                continue;
            };
            let shown = self
                .views
                .iter()
                .find(|shown| shown.spec.id == view.spec.id)
                .map(|shown| shown.activity);
            if let Some(ModelActivity::Downloading {
                done: shown,
                total: shown_total,
            }) = shown
                && shown_total == total
                && shown > done
            {
                view.activity = ModelActivity::Downloading { done: shown, total };
            }
        }
        self.views = views;
    }

    /// A download of `model` wrote `done` bytes of `total`.
    pub(super) fn progressed(&mut self, model: &str, done: u64, total: u64) {
        if let Some(view) = self.views.iter_mut().find(|view| view.spec.id == model)
            && matches!(
                view.activity,
                ModelActivity::Idle | ModelActivity::Downloading { .. }
            )
        {
            view.activity = ModelActivity::Downloading { done, total };
        }
    }

    /// A command on `model` failed, as `message` says.
    pub(super) fn failed(&mut self, model: &str, message: &str) {
        let Some(spec) = models::model(model) else {
            return;
        };
        let download = self.attempts.get(spec.id) == Some(&Attempt::Download);
        let text = sentence(message);
        self.failures
            .insert(spec.id, ModelFailure { text, download });
    }

    /// A new command on `id`: its last failure no longer holds.
    fn attempt(&mut self, id: &'static str, attempt: Attempt) {
        self.failures.remove(id);
        self.attempts.insert(id, attempt);
    }

    /// The state of the visual check with the switch `enabled` and `active` the active
    /// model (see [`MlState`]).
    fn state(&self, enabled: bool, active: &'static ModelSpec) -> MlState {
        if !cfg!(feature = "ml") {
            return MlState::NotBuilt;
        }
        if self.runtime.problem.is_some() {
            return MlState::NoRuntime(self.runtime.clone());
        }
        if !enabled {
            return MlState::Off;
        }
        let id = active.id;
        let view = self.views.iter().find(|view| view.spec.id == id);
        match view {
            Some(ModelView {
                activity: ModelActivity::Downloading { done, total },
                ..
            }) => MlState::Downloading {
                id,
                percent: download_percent(*done, *total),
            },
            Some(view) if view.installed => MlState::Ready(id),
            _ => MlState::Missing(id),
        }
    }
}

impl Controller {
    /// Shows Settings with the visual check's card in view: what the ML chip opens.
    pub fn open_visual_check(&mut self) {
        self.open_settings();
        self.models.reveal = true;
    }

    /// Whether the visual check's card is to be scrolled into view, once: the card asks
    /// as it is drawn.
    pub fn take_reveal_visual_check(&mut self) -> bool {
        std::mem::take(&mut self.models.reveal)
    }

    /// The models of Stage 3 as last read, in the registry's order.
    pub fn models(&self) -> &[ModelView] {
        &self.models.views
    }

    /// The last failure of the model `id`, until the next command on it.
    pub fn model_failure(&self, id: &str) -> Option<&ModelFailure> {
        self.models.failures.get(id)
    }

    /// The model the visual check of new jobs uses: the one chosen, or the registry's
    /// first when the one saved is not in this build's registry.
    pub fn active_model(&self) -> &'static ModelSpec {
        models::model(&self.prefs.ml_model).unwrap_or(&MODELS[0])
    }

    /// The state of the visual check, which the ML chip, the status bar and the new-job
    /// form show.
    pub fn ml_state(&self) -> MlState {
        self.models
            .state(self.prefs.ml_enabled, self.active_model())
    }

    /// The model a new job names: the active one while the check can run, installed or
    /// not (the job is checked once it is), none otherwise.
    pub fn job_model(&self) -> Option<&'static str> {
        match self.ml_state() {
            MlState::Ready(id) | MlState::Missing(id) | MlState::Downloading { id, .. } => Some(id),
            MlState::NotBuilt | MlState::Off | MlState::NoRuntime(_) => None,
        }
    }

    /// Downloads the model `id` now: the network's one use, after the user's `Download`
    /// (the view asks [`Question::DownloadModel`](super::Question) first; `Try again`
    /// does not ask again). The engine resumes what an earlier download left.
    pub fn download_model(&mut self, id: &str) {
        if let Some(spec) = models::model(id) {
            let id = spec.id;
            self.model_command(id, Attempt::Download, move |engine| {
                engine.download_model(id)
            });
        }
    }

    /// Cancels the download of the model `id`, which keeps what it wrote.
    pub fn cancel_download(&mut self, id: &str) {
        if let Some(spec) = models::model(id) {
            let id = spec.id;
            self.send(Request::Run {
                job: None,
                command: Box::new(move |engine| engine.cancel_download(id)),
            });
        }
    }

    /// Asks for a model file with the desktop's dialog, which blocks until it is closed,
    /// and imports it as the model `id`; a dialog closed without a file does nothing, and
    /// a desktop without a dialog is a warning.
    pub fn import_model(&mut self, id: &str) {
        let Some(spec) = models::model(id) else {
            return;
        };
        let file = match self.platform.pick_file(None) {
            Picked::Path(file) => file,
            Picked::Cancelled => return,
            Picked::Unavailable => {
                self.notify(NoticeLevel::Warning, NO_FILE_DIALOG.into());
                return;
            }
        };
        let id = spec.id;
        self.model_command(id, Attempt::Import, move |engine| {
            engine.import_model(id, &file)
        });
    }

    /// Deletes the model `id`, and any unfinished download of it.
    pub fn remove_model(&mut self, id: &str) {
        if let Some(spec) = models::model(id) {
            let id = spec.id;
            self.model_command(id, Attempt::Remove, move |engine| {
                match engine.remove_model(id) {
                    // Its `ModelFailed` says why, under the model's row.
                    Err(EngineError::Remove { .. }) => Ok(()),
                    other => other,
                }
            });
        }
    }

    /// Sends `command`, the `attempt` on the model `id`, whose last failure goes.
    fn model_command(
        &mut self,
        id: &'static str,
        attempt: Attempt,
        command: impl FnOnce(&Engine) -> Result<(), EngineError> + Send + 'static,
    ) {
        self.models.attempt(id, attempt);
        self.send(Request::Run {
            job: None,
            command: Box::new(command),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ONNX Runtime where the tests say it is: a made-up path, found or not, never a look
    /// at this machine's file system.
    fn runtime(found: bool) -> RuntimeCheck {
        RuntimeCheck {
            path: PathBuf::from("/opt/onnxruntime/lib/libonnxruntime.so"),
            problem: (!found).then_some(LibraryProblem::Missing),
        }
    }

    fn view(index: usize, installed: bool, activity: ModelActivity) -> ModelView {
        ModelView {
            spec: &MODELS[index],
            installed,
            activity,
            disk_bytes: 0,
        }
    }

    fn downloading(done: u64) -> ModelActivity {
        ModelActivity::Downloading {
            done,
            total: MODELS[0].size,
        }
    }

    #[test]
    fn a_read_never_takes_a_download_back() {
        let mut models = Models::new(runtime(true), Vec::new());
        models.read(vec![view(0, false, ModelActivity::Idle)]);
        // The progress event comes before the read made at the download's start.
        models.progressed("dinov2-small", 500, MODELS[0].size);
        models.read(vec![view(0, false, downloading(0))]);
        assert_eq!(models.views[0].activity, downloading(500));
        // A later read further on, or the download's end, is taken as it is.
        models.read(vec![view(0, false, downloading(900))]);
        assert_eq!(models.views[0].activity, downloading(900));
        models.read(vec![view(0, true, ModelActivity::Idle)]);
        assert_eq!(models.views[0].activity, ModelActivity::Idle);
        // A cancelling download takes no progress.
        models.read(vec![view(0, false, ModelActivity::Cancelling)]);
        models.progressed("dinov2-small", 1000, MODELS[0].size);
        assert_eq!(models.views[0].activity, ModelActivity::Cancelling);
    }

    #[test]
    fn a_failure_holds_until_the_next_command_on_its_model() {
        let mut models = Models::new(runtime(true), Vec::new());
        models.attempt("dinov2-small", Attempt::Download);
        models.attempt("dinov2-base", Attempt::Import);
        models.failed("dinov2-small", "cannot download: every download URL failed");
        models.failed(
            "dinov2-base",
            "cannot import: it does not match dinov2-base",
        );
        models.failed("dinov2-large", "not a model of this build");
        assert_eq!(
            models.failures.get("dinov2-small"),
            Some(&ModelFailure {
                text: "Cannot download: every download URL failed.".into(),
                download: true,
            })
        );
        assert!(!models.failures["dinov2-base"].download);
        assert_eq!(models.failures.len(), 2);
        models.attempt("dinov2-small", Attempt::Remove);
        assert!(!models.failures.contains_key("dinov2-small"));
        assert!(models.failures.contains_key("dinov2-base"));
    }

    #[test]
    fn a_model_being_imported_or_cancelled_is_missing() {
        let small = &MODELS[0];
        let state = |installed, activity| {
            let models = Models::new(runtime(true), vec![view(0, installed, activity)]);
            models.state(true, small)
        };
        if !cfg!(feature = "ml") {
            assert_eq!(state(false, ModelActivity::Idle), MlState::NotBuilt);
            return;
        }
        for activity in [ModelActivity::Importing, ModelActivity::Cancelling] {
            assert_eq!(state(false, activity), MlState::Missing(small.id));
        }
        assert_eq!(
            state(false, downloading(MODELS[0].size / 2)),
            MlState::Downloading {
                id: small.id,
                percent: 50
            }
        );
        assert_eq!(state(true, ModelActivity::Idle), MlState::Ready(small.id));
        assert_eq!(
            state(false, ModelActivity::Idle),
            MlState::Missing(small.id)
        );
        // The switch, then the runtime, come first.
        let models = Models::new(runtime(true), vec![view(0, true, ModelActivity::Idle)]);
        assert_eq!(models.state(false, small), MlState::Off);
        let models = Models::new(runtime(false), vec![view(0, true, ModelActivity::Idle)]);
        assert_eq!(
            models.state(true, small),
            MlState::NoRuntime(runtime(false))
        );
        // A library that needs a newer macOS cannot run either.
        let version = |major, minor| OsVersion {
            major,
            minor,
            patch: 0,
        };
        let needs_macos = RuntimeCheck {
            problem: Some(LibraryProblem::NeedsMacos {
                needed: version(14, 0),
                running: version(13, 6),
            }),
            ..runtime(true)
        };
        let models = Models::new(
            needs_macos.clone(),
            vec![view(0, true, ModelActivity::Idle)],
        );
        assert_eq!(models.state(true, small), MlState::NoRuntime(needs_macos));
    }
}
