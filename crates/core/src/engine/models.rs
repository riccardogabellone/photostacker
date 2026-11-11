//! The models of Stage 3 through the engine (spec §4.3, §8.2): what the
//! app's settings list, download, import and remove.
//!
//! - **Commands** return at once. A download or an import runs on a thread
//!   of its own, `photostacker-model-<id>`, never on the worker, through the
//!   engine's [`ModelInstall`]; a removal runs on the caller's thread. While
//!   a model is downloaded or imported it is busy: a second download, import
//!   or removal of it is refused. The model's own lock (`models/lock.rs`),
//!   which the installer and the removal take, covers the other processes.
//! - **Events:** [`EngineEvent::ModelChanged`] on every change of a model's
//!   view, [`EngineEvent::ModelDownloadProgress`] at each whole percent, and
//!   an [`EngineEvent::ModelFailed`] for a download, an import or a removal
//!   that fails; a cancel is no failure.
//! - **A model installed** is forgotten by Stage 3's source, the jobs whose
//!   Stage 3 failed in this run and that name it are forgiven, and every
//!   job in Review Pending that names it is regrouped, by a review edit
//!   that changes nothing else: its skipped groups become pending, and its
//!   embed task is queued. A model removed is forgotten; no job is
//!   regrouped.
//!
//! **A model thread never outlives the engine's store.** It holds the
//! commands' state ([`Models`]) and only a weak link to the engine's shared
//! state, which it reaches through [`Models::with_engine`], under the read
//! side of the `open` lock. The drop of the engine first sets every
//! download's cancel flag, then takes that lock's write side, which waits
//! for the threads that are in the engine to leave it, and closes it: from
//! then on no model thread reaches the engine, sends an event or touches
//! the store. Only then is the worker joined and the shared state dropped,
//! its store connections closed, and the database's lock released last. So
//! a regrouping that a finished install started ends before the drop does,
//! and one that has not started never starts. The drop waits for no
//! download or import: a thread still reading or writing its file ends on
//! its own, and reports nothing.

use std::cell::Cell;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock, Weak};
use std::thread;

use super::{Engine, EngineError, EngineEvent, JobId, Shared, error_chain, lock};
use crate::guard;
use crate::model::JobStatus;
use crate::models::{self, MODELS, ModelError, ModelSpec};

/// The name of a model's thread: this, then the model's id.
const MODEL_THREAD_PREFIX: &str = "photostacker-model-";

/// What downloads and imports the models for the engine: with the `ml`
/// feature, `models::install`, unless a stand-in is given through
/// [`EngineOptions::installer`](super::EngineOptions), so that tests need
/// no HTTP.
pub trait ModelInstall: Send + Sync {
    /// Downloads the model `spec` into `models_dir` and returns its
    /// installed file, as `models::install::download` does: `progress(done,
    /// total)` follows the bytes written, and a set `cancel` ends it with
    /// [`ModelError::Cancelled`], keeping its part.
    fn download(
        &self,
        spec: &ModelSpec,
        models_dir: &Path,
        progress: &dyn Fn(u64, u64),
        cancel: &AtomicBool,
    ) -> Result<PathBuf, ModelError>;

    /// Imports the model `spec` from `file` into `models_dir` and returns
    /// its installed file, as `models::install::import` does.
    fn import(
        &self,
        spec: &ModelSpec,
        file: &Path,
        models_dir: &Path,
    ) -> Result<PathBuf, ModelError>;
}

/// A model of the registry, as the app's settings show it.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelView {
    pub spec: &'static ModelSpec,
    /// Its file is in place ([`models::installed`]).
    pub installed: bool,
    pub activity: ModelActivity,
    /// The size of everything in its folder, `models/<id>/`, the unfinished
    /// part of a download included.
    pub disk_bytes: u64,
}

/// What the engine is doing with a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelActivity {
    Idle,
    /// Being downloaded: `done` bytes of `total`, as the last
    /// [`EngineEvent::ModelDownloadProgress`] said.
    Downloading {
        done: u64,
        total: u64,
    },
    Importing,
    /// Its download was cancelled, and its thread has not ended yet.
    Cancelling,
}

/// The state of the model commands, shared with their threads (see the
/// module docs).
pub(super) struct Models {
    models_dir: PathBuf,
    /// `None` without the `ml` feature, unless one was given.
    installer: Option<Arc<dyn ModelInstall>>,
    /// The busy models: those being downloaded or imported. No other lock is
    /// taken while it is held, but a removal ([`Engine::remove_model`]) holds
    /// it across the model's own lock and the removal of its folder, so a
    /// download of another model waits meanwhile to report its progress.
    busy: Mutex<HashMap<&'static str, Busy>>,
    /// Whether a model thread may reach the engine: true until the engine
    /// is dropped. A thread holds its read side while it does.
    open: RwLock<bool>,
    engine: Weak<Shared>,
}

/// A busy model.
struct Busy {
    activity: ModelActivity,
    /// A download's cancel flag.
    cancel: Option<Arc<AtomicBool>>,
}

/// How a download or an import ended.
enum Ended {
    Installed,
    Cancelled,
    /// With the whole `error` for the log, and the `reason` the user reads.
    Failed {
        error: String,
        reason: String,
    },
}

impl Models {
    /// The model commands on the models of `models_dir`, installed by
    /// `installer` (`None`: the default, see [`ModelInstall`]), for the
    /// engine whose shared state is `engine`.
    pub(super) fn new(
        models_dir: PathBuf,
        installer: Option<Arc<dyn ModelInstall>>,
        engine: Weak<Shared>,
    ) -> Self {
        Self {
            models_dir,
            installer: installer.or_else(default_installer),
            busy: Mutex::default(),
            open: RwLock::new(true),
            engine,
        }
    }

    /// Sets every download's cancel flag, waits for the model threads that
    /// are in the engine to leave it, and keeps them out from then on. The
    /// drop of the engine calls it before anything else.
    pub(super) fn close(&self) {
        for busy in lock(&self.busy).values() {
            if let Some(cancel) = &busy.cancel {
                cancel.store(true, Ordering::SeqCst);
            }
        }
        *self.open.write().unwrap_or_else(PoisonError::into_inner) = false;
    }

    /// Runs `work` with the engine's shared state, unless the engine is
    /// dropped or being dropped: then it does nothing.
    fn with_engine(&self, work: impl FnOnce(&Shared)) {
        let open = self.open.read().unwrap_or_else(PoisonError::into_inner);
        if !*open {
            return;
        }
        // The engine keeps its own reference until it has closed `open`,
        // so this one, dropped before `open` is let go, is never the last.
        if let Some(shared) = self.engine.upgrade() {
            work(&shared);
        }
    }

    fn installer(&self) -> Result<Arc<dyn ModelInstall>, EngineError> {
        self.installer
            .clone()
            .ok_or_else(|| EngineError::Invalid("built without ml".into()))
    }

    /// Marks `spec` idle and posts its change, for a caller that flushes.
    fn set_idle(&self, shared: &Shared, spec: &ModelSpec) {
        lock(&self.busy).remove(spec.id);
        shared.events.post(model_changed(spec));
    }

    /// Reports that `spec`'s download wrote `done` bytes of `total`, when
    /// its whole percent passed `last`, the last one reported, which it then
    /// becomes. Nothing once the download is cancelling.
    fn progress(&self, spec: &ModelSpec, done: u64, total: u64, last: &Cell<Option<u64>>) {
        let percent = match total {
            0 => 100,
            total => done.min(total).saturating_mul(100) / total,
        };
        if last.get().is_some_and(|last| percent <= last) {
            return;
        }
        last.set(Some(percent));
        self.with_engine(|shared| {
            {
                let mut busy = lock(&self.busy);
                let Some(entry) = busy.get_mut(spec.id) else {
                    return;
                };
                if !matches!(entry.activity, ModelActivity::Downloading { .. }) {
                    return;
                }
                entry.activity = ModelActivity::Downloading { done, total };
                shared.events.post(EngineEvent::ModelDownloadProgress {
                    model: spec.id.into(),
                    done,
                    total,
                });
            }
            shared.events.flush();
        });
    }

    /// Settles the download or the import (`verb`) of `spec` once it
    /// `ended`: the model becomes idle, a failure is reported, and a model
    /// installed is put to use (see the module docs).
    ///
    /// The model becomes idle on every path, a panic included: a model left
    /// busy would refuse every later command on it until a restart. Putting
    /// it to use (its source forgets it, which drops a loaded session, and
    /// the store lists the jobs) runs under the panic guard, and a panic
    /// there is the model's failure; so does each job's regrouping, whose
    /// panic is that job's error, and the next job is still regrouped.
    fn finish(&self, spec: &'static ModelSpec, verb: &str, ended: Ended) {
        self.with_engine(|shared| {
            let _flush = shared.events.flush_on_drop();
            let idle = IdleOnDrop {
                models: self,
                shared,
                spec,
            };
            let (in_review, ended) = match ended {
                Ended::Installed => {
                    tracing::info!(model = spec.id, "model installed");
                    match guard::catch(|| shared.model_installed(spec)) {
                        Ok(in_review) => (in_review, Ended::Installed),
                        Err(panic) => (Vec::new(), panicked(&panic)),
                    }
                }
                other => (Vec::new(), other),
            };
            drop(idle);
            if let Ended::Failed { error, reason } = ended {
                tracing::warn!(model = spec.id, %error, "cannot {verb} a model");
                shared.events.post(model_failed(spec, verb, &reason));
            }
            shared.events.flush();
            for job in in_review {
                if let Err(panic) = guard::catch(|| shared.regroup_with_its_model(job)) {
                    tracing::warn!(job, model = spec.id, %panic, "regrouping a job with its model panicked");
                    shared.events.send(EngineEvent::Error {
                        job: Some(job),
                        message: format!(
                            "internal error while regrouping with {}: {panic}",
                            spec.id
                        ),
                    });
                }
            }
        });
    }
}

/// Marks its model idle when dropped, whatever ended the work on it.
struct IdleOnDrop<'a> {
    models: &'a Models,
    shared: &'a Shared,
    spec: &'a ModelSpec,
}

impl Drop for IdleOnDrop<'_> {
    fn drop(&mut self) {
        self.models.set_idle(self.shared, self.spec);
    }
}

impl Engine {
    /// Every model of the registry, in its order: whether it is installed,
    /// what the engine is doing with it, and what its folder takes on disk,
    /// which this lists on the caller's thread.
    pub fn models(&self) -> Vec<ModelView> {
        let activities: Vec<ModelActivity> = {
            let busy = lock(&self.models.busy);
            MODELS
                .iter()
                .map(|spec| {
                    busy.get(spec.id)
                        .map_or(ModelActivity::Idle, |busy| busy.activity)
                })
                .collect()
        };
        let models_dir = &self.models.models_dir;
        MODELS
            .iter()
            .zip(activities)
            .map(|(spec, activity)| ModelView {
                spec,
                installed: models::installed(models_dir, spec),
                activity,
                disk_bytes: folder_size(&models_dir.join(spec.id)),
            })
            .collect()
    }

    /// Downloads the model `id` on a thread of its own, resuming its part:
    /// [`EngineEvent::ModelChanged`], then
    /// [`EngineEvent::ModelDownloadProgress`] at each whole percent, then
    /// [`EngineEvent::ModelChanged`] again when it ends, with an
    /// [`EngineEvent::ModelFailed`] when it failed. Refused for a model the
    /// registry does not have, a busy one, or without the `ml` feature.
    pub fn download_model(&self, id: &str) -> Result<(), EngineError> {
        let spec = registry_model(id)?;
        let installer = self.models.installer()?;
        let cancel = Arc::new(AtomicBool::new(false));
        let downloading = ModelActivity::Downloading {
            done: 0,
            total: spec.size,
        };
        let models = Arc::clone(&self.models);
        let flag = Arc::clone(&cancel);
        self.begin(spec, downloading, Some(cancel), move || {
            let last = Cell::new(None);
            let progress = |done, total| models.progress(spec, done, total, &last);
            let downloaded =
                guard::catch(|| installer.download(spec, &models.models_dir, &progress, &flag));
            models.finish(spec, "download", ended(downloaded));
        })
    }

    /// Cancels the download of the model `id`: it is
    /// [`ModelActivity::Cancelling`] until its thread ends, at its next
    /// check, and its part is kept for the next download. Refused for a
    /// model that is not being downloaded; one that is cancelling already is
    /// left so.
    pub fn cancel_download(&self, id: &str) -> Result<(), EngineError> {
        let spec = registry_model(id)?;
        let not_downloading =
            || EngineError::Invalid(format!("the model {id} is not being downloaded"));
        {
            let mut busy = lock(&self.models.busy);
            let entry = busy.get_mut(spec.id).ok_or_else(not_downloading)?;
            match (entry.activity, &entry.cancel) {
                (ModelActivity::Cancelling, _) => return Ok(()),
                (ModelActivity::Downloading { .. }, Some(cancel)) => {
                    cancel.store(true, Ordering::SeqCst);
                    entry.activity = ModelActivity::Cancelling;
                    self.shared.events.post(model_changed(spec));
                }
                _ => return Err(not_downloading()),
            }
        }
        self.shared.events.flush();
        Ok(())
    }

    /// Imports the model `id` from `file` on a thread of its own:
    /// [`EngineEvent::ModelChanged`] as it starts and as it ends, with an
    /// [`EngineEvent::ModelFailed`] when it failed, such as for a file that is not
    /// the model. `file` itself is only read. Refused as
    /// [`Engine::download_model`] is; an import cannot be cancelled.
    pub fn import_model(&self, id: &str, file: &Path) -> Result<(), EngineError> {
        let spec = registry_model(id)?;
        let installer = self.models.installer()?;
        let models = Arc::clone(&self.models);
        let file = file.to_path_buf();
        self.begin(spec, ModelActivity::Importing, None, move || {
            let imported = guard::catch(|| installer.import(spec, &file, &models.models_dir));
            models.finish(spec, "import", ended(imported));
        })
    }

    /// Removes the model `id`, its folder and any unfinished part, on the
    /// caller's thread; Stage 3 forgets it, and no job is regrouped.
    /// [`EngineEvent::ModelChanged`] follows whether it succeeded or not,
    /// since a failure may have removed a part of the folder, and an
    /// [`EngineEvent::ModelFailed`] with the error returned when it failed,
    /// as when another process downloads the model. Refused for a model the
    /// registry does not have, or a busy one. A model that is not there is
    /// removed already.
    pub fn remove_model(&self, id: &str) -> Result<(), EngineError> {
        let spec = registry_model(id)?;
        let removed = {
            // Held throughout: no download or import of the model starts
            // meanwhile in this process.
            let busy = lock(&self.models.busy);
            if busy.contains_key(spec.id) {
                return Err(busy_error(spec));
            }
            let removed = models::remove(spec, &self.models.models_dir);
            self.shared.events.post(model_changed(spec));
            removed
        };
        self.shared.stage3.forget(spec);
        match &removed {
            Ok(()) => tracing::info!(model = spec.id, "model removed"),
            Err(error) => {
                tracing::warn!(model = spec.id, %error, "cannot remove a model");
                let failed = model_failed(spec, "remove", &error_chain(error));
                self.shared.events.post(failed);
            }
        }
        self.shared.events.flush();
        removed.map_err(|error| EngineError::Remove {
            model: spec.id,
            error,
        })
    }

    /// Marks `spec` busy with `activity` and `cancel`, unless it is busy
    /// already, and runs `work` on the model's thread.
    fn begin(
        &self,
        spec: &'static ModelSpec,
        activity: ModelActivity,
        cancel: Option<Arc<AtomicBool>>,
        work: impl FnOnce() + Send + 'static,
    ) -> Result<(), EngineError> {
        {
            let mut busy = lock(&self.models.busy);
            if busy.contains_key(spec.id) {
                return Err(busy_error(spec));
            }
            busy.insert(spec.id, Busy { activity, cancel });
            self.shared.events.post(model_changed(spec));
        }
        self.shared.events.flush();
        let spawned = thread::Builder::new()
            .name(format!("{MODEL_THREAD_PREFIX}{}", spec.id))
            .spawn(work);
        if let Err(error) = spawned {
            self.models.set_idle(&self.shared, spec);
            self.shared.events.flush();
            return Err(EngineError::Spawn(error));
        }
        Ok(())
    }
}

impl Shared {
    /// Puts `spec`, just installed, to use: its source forgets it, and the
    /// jobs that name it are forgiven. Returns those of them in Review
    /// Pending, to regroup. A store that cannot list the jobs is reported
    /// (posted), and none is regrouped.
    fn model_installed(&self, spec: &ModelSpec) -> Vec<JobId> {
        self.stage3.forget(spec);
        let jobs = lock(&self.writer).jobs();
        let jobs = match jobs {
            Ok(jobs) => jobs,
            Err(error) => {
                tracing::warn!(model = spec.id, %error, "cannot list the jobs to regroup");
                self.events.post(EngineEvent::Error {
                    job: None,
                    message: format!(
                        "cannot regroup the jobs that name {}: {}",
                        spec.id,
                        error_chain(&error)
                    ),
                });
                return Vec::new();
            }
        };
        let naming: Vec<_> = jobs
            .into_iter()
            .filter(|job| job.config.ml_model.as_deref() == Some(spec.id))
            .collect();
        self.stage3.forgive(naming.iter().map(|job| job.id));
        naming
            .into_iter()
            .filter(|job| job.status == JobStatus::ReviewPending)
            .map(|job| job.id)
            .collect()
    }

    /// Regroups `job`, which a scan just settled in Review Pending without
    /// its model, when the model is available to it now. A model installed
    /// while the scan ran, once the scan's Stage 3 had found none, lists the
    /// jobs in review before this one is: without this check, nothing would
    /// ever check its groups. Called with no engine lock held, after the
    /// status is stored, so either the install's listing or this check sees
    /// the other; when both do, the second regrouping changes nothing.
    pub(super) fn regroup_if_its_model_came(&self, job: JobId) {
        let stored = lock(&self.writer).job(job);
        let config = match stored {
            Ok(Some(stored)) => stored.config,
            Ok(None) => return,
            Err(error) => {
                self.report_error(job, &error);
                self.events.flush();
                return;
            }
        };
        if self.stage3.available(job, &config) {
            tracing::info!(job, "the job's model came during its scan: regrouping it");
            self.regroup_with_its_model(job);
        }
    }

    /// Regroups `job`, whose model was just installed, by a review edit that
    /// changes nothing else. A job that left review meanwhile, or is gone,
    /// is left alone; any other failure is reported for the job.
    fn regroup_with_its_model(&self, job: JobId) {
        match self.edit(job, |_| Ok(true)) {
            Ok(()) | Err(EngineError::WrongState { .. } | EngineError::UnknownJob(_)) => {}
            Err(error) => {
                tracing::warn!(job, %error, "cannot regroup a job with its model");
                self.events.send(EngineEvent::Error {
                    job: Some(job),
                    message: error_chain(&error),
                });
            }
        }
    }
}

/// The registry entry of `id`, or why there is none.
fn registry_model(id: &str) -> Result<&'static ModelSpec, EngineError> {
    models::model(id).ok_or_else(|| EngineError::Invalid(format!("no model {id}")))
}

/// A refusal never starts with the model's id: the app shows a refusal as a
/// sentence, with its first letter capitalised, which would misquote the id.
fn busy_error(spec: &ModelSpec) -> EngineError {
    EngineError::Invalid(format!("the model {} is busy", spec.id))
}

fn model_changed(spec: &ModelSpec) -> EngineEvent {
    EngineEvent::ModelChanged {
        model: spec.id.into(),
    }
}

/// The failure of the `verb` (download, import, remove) of `spec`, for
/// `error`.
fn model_failed(spec: &ModelSpec, verb: &str, error: &str) -> EngineEvent {
    EngineEvent::ModelFailed {
        model: spec.id.into(),
        message: format!("cannot {verb}: {error}"),
    }
}

/// How the installer's call, run under the panic guard, ended: a panic is a
/// failure like any other, so the model is idle again.
fn ended(outcome: Result<Result<PathBuf, ModelError>, String>) -> Ended {
    match outcome {
        Ok(Ok(_)) => Ended::Installed,
        Ok(Err(ModelError::Cancelled)) => Ended::Cancelled,
        Ok(Err(error)) => Ended::Failed {
            reason: reason(&error),
            error: error_chain(&error),
        },
        Err(panic) => panicked(&panic),
    }
}

/// The failure of a model's work that panicked with `panic`.
fn panicked(panic: &str) -> Ended {
    let error = format!("internal error: {panic}");
    Ended::Failed {
        reason: error.clone(),
        error,
    }
}

/// What the user reads of `error`: for a download whose every address
/// failed, each address's host and its reason (`huggingface.co: HTTP status
/// 404; github.com: …`), without the paths and pinned revisions the log
/// keeps; any other error's whole text.
fn reason(error: &ModelError) -> String {
    match error {
        ModelError::AllUrlsFailed(failures) => {
            let failures: Vec<String> = failures
                .iter()
                .map(|failure| format!("{}: {}", models::url_host(&failure.url), failure.reason))
                .collect();
            failures.join("; ")
        }
        other => error_chain(other),
    }
}

/// The size of every file under `dir`, links not followed: 0 when it does
/// not exist. What cannot be read counts for nothing.
fn folder_size(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(metadata) if metadata.is_dir() => folder_size(&entry.path()),
            Ok(metadata) => metadata.len(),
            Err(_) => 0,
        })
        .sum()
}

/// The default installer: `models::install`, with the `ml` feature only.
#[cfg(feature = "ml")]
fn default_installer() -> Option<Arc<dyn ModelInstall>> {
    Some(Arc::new(Installer))
}

#[cfg(not(feature = "ml"))]
fn default_installer() -> Option<Arc<dyn ModelInstall>> {
    None
}

/// `models::install` as the engine's installer.
#[cfg(feature = "ml")]
struct Installer;

#[cfg(feature = "ml")]
impl ModelInstall for Installer {
    fn download(
        &self,
        spec: &ModelSpec,
        models_dir: &Path,
        progress: &dyn Fn(u64, u64),
        cancel: &AtomicBool,
    ) -> Result<PathBuf, ModelError> {
        models::install::download(spec, models_dir, progress, cancel)
    }

    fn import(
        &self,
        spec: &ModelSpec,
        file: &Path,
        models_dir: &Path,
    ) -> Result<PathBuf, ModelError> {
        models::install::import(spec, file, models_dir)
    }
}
