//! Stage 3 in the engine (spec §5.4, §8.3): which jobs can check their
//! ambiguous groups with their model, the embeddings a regrouping waits for,
//! and the embed task that computes them in the background.
//!
//! - **Available:** a job's Stage 3 runs when its config names a model and
//!   the engine's source of embeddings says the model is available
//!   ([`EmbedSource::available`]): the source given in
//!   [`EngineOptions::embedder`](super::EngineOptions), or, with the `ml`
//!   feature, the installed models, each loaded on first use and kept (a
//!   failed load too) until the model is installed or removed through the
//!   engine, which makes the source forget it. Otherwise the ambiguous
//!   groups are `Skipped`, with no warning.
//! - **Computed:** the photos whose embeddings a regrouping waits for (its
//!   `needs_embeddings`, the members of its `Pending` groups that have none)
//!   are decoded again, the upright 512 px thumbnail they were hashed from,
//!   on a pool of their own under the decodes' memory budget; embedded in
//!   batches of 8; and stored batch by batch, each photo an `Embed` tick.
//!   Then the job regroups with them. Embeddings are computed only once the
//!   job's analysis is saved, and are kept until the job is deleted.
//! - **Failed:** a model that cannot be loaded or run, or a photo that
//!   cannot be decoded again, skips Stage 3 for the job for the rest of the
//!   engine's run, unless the job's model is installed through the engine
//!   meanwhile, which forgives it: its ambiguous groups are `Skipped`. The
//!   first failure is reported, once per job: a warning event, and the job's
//!   stored warning, `the visual check was skipped: <error>`, in the words
//!   the app and the CLI show (the visual check is Stage 3's name for the
//!   user). A panic in the model is such
//!   a failure. The stored warning is cleared only when [`prepare`] runs for
//!   the job again: a scan of the job (after a restart, or a retry once
//!   the model is forgiven), or an embed task. A forgiven
//!   job gets an embed task only when its regrouping leaves groups waiting
//!   for embeddings, so one that needs none keeps the warning.
//! - **The embed task** regroups only a job still in Review Pending.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use image::RgbImage;

use super::review::{Draft, discovered};
use super::worker::{Settle, TaskError};
use super::{EngineEvent, JobId, Shared, error_chain, lock};
use crate::analyze::{AnalyzeError, FileRecord, Progress, Stage, ThumbnailDecoder};
use crate::discover::DiscoveredFile;
use crate::fsio::display_path;
use crate::guard;
use crate::model::{JobConfig, JobStatus};
use crate::models::{self, ModelSpec};
use crate::store::{Store, StoreError};

/// The images embedded in one call of the model (spec §8.3).
const BATCH: usize = 8;

/// The photos decoded at a time: enough batches to keep the decoder's
/// threads busy, and at most 64 thumbnails of 512 px (about 50 MB) held at
/// once.
const CHUNK: usize = 8 * BATCH;

/// Where Stage 3's embeddings come from: the installed models by default,
/// or a stand-in given through [`EngineOptions::embedder`](super::EngineOptions).
pub trait EmbedSource: Send + Sync {
    /// The embeddings of `images` by the model `model`, in order, each of
    /// the model's `dims` values; or why there are none.
    fn embed(&self, model: &ModelSpec, images: &[RgbImage]) -> Result<Vec<Vec<f32>>, String>;

    /// Whether `model` can be used now: for the installed models, whether
    /// it is installed. Asked at every regrouping, so it must be quick.
    fn available(&self, _model: &ModelSpec) -> bool {
        true
    }

    /// Drops what the source kept of `model`, such as its load, failed or
    /// not: the engine installed or removed it, so its next use starts
    /// afresh.
    fn forget(&self, _model: &ModelSpec) {}
}

/// A job's embeddings for its model, by file id.
pub(super) type Embeddings = HashMap<usize, Vec<f32>>;

/// What the engine knows of Stage 3.
pub(super) struct Stage3 {
    /// `None` without the `ml` feature, unless one was given.
    source: Option<Arc<dyn EmbedSource>>,
    /// The jobs whose Stage 3 failed in this run. Nothing is locked while
    /// it is held.
    failed: Mutex<HashSet<JobId>>,
}

impl Stage3 {
    /// Stage 3 with `given` as its source, else the models installed in
    /// `models_dir`.
    pub(super) fn new(models_dir: &Path, given: Option<Arc<dyn EmbedSource>>) -> Self {
        Self {
            source: given.or_else(|| installed_models(models_dir)),
            failed: Mutex::default(),
        }
    }

    /// The model `job` checks its ambiguous groups with, when Stage 3 is
    /// available to it (see the module docs).
    fn model(&self, job: JobId, config: &JobConfig) -> Option<Model<'_>> {
        let spec = models::model(config.ml_model.as_deref()?)?;
        let source = self.source.as_deref()?;
        if !source.available(spec) {
            return None;
        }
        if lock(&self.failed).contains(&job) {
            return None;
        }
        Some(Model { spec, source })
    }

    /// Whether Stage 3 is available to `job` now (see the module docs).
    pub(super) fn available(&self, job: JobId, config: &JobConfig) -> bool {
        self.model(job, config).is_some()
    }

    /// Makes the source forget `model` (see [`EmbedSource::forget`]).
    pub(super) fn forget(&self, model: &ModelSpec) {
        if let Some(source) = &self.source {
            source.forget(model);
        }
    }

    /// Lets Stage 3 run again for `jobs`, whose model failed in this run:
    /// it was installed again.
    pub(super) fn forgive(&self, jobs: impl IntoIterator<Item = JobId>) {
        let mut failed = lock(&self.failed);
        for job in jobs {
            failed.remove(&job);
        }
    }

    /// The embeddings `job` regroups with: those stored for its model, when
    /// Stage 3 is available to it; `None` when it is not.
    pub(super) fn embeddings(
        &self,
        store: &Store,
        job: JobId,
        config: &JobConfig,
    ) -> Result<Option<Embeddings>, StoreError> {
        self.model(job, config)
            .map(|model| model.stored(store, job))
            .transpose()
    }

    /// Records that `job`'s Stage 3 failed; whether it is the first time.
    fn fail(&self, job: JobId) -> bool {
        lock(&self.failed).insert(job)
    }
}

/// A job's model and where its embeddings come from.
#[derive(Clone, Copy)]
struct Model<'a> {
    spec: &'static ModelSpec,
    source: &'a dyn EmbedSource,
}

impl Model<'_> {
    /// The stored embeddings of `job`'s files for this model.
    fn stored(&self, store: &Store, job: JobId) -> Result<Embeddings, StoreError> {
        let rows = store.file_row_ids(job)?;
        let vectors = store.embeddings(self.spec.id, &rows)?;
        Ok(vectors
            .into_iter()
            .enumerate()
            .filter_map(|(id, vector)| Some((id, vector?)))
            .collect())
    }

    /// The embeddings of `images`, each checked: one per image, of the
    /// model's `dims` finite values, so a broken source never stores one.
    /// A panic in the source (a bug in it or in ONNX Runtime) is an error
    /// like any other, so the job's Stage 3 is skipped rather than panicking
    /// again at every edit.
    fn embed(&self, images: &[RgbImage]) -> Result<Vec<Vec<f32>>, String> {
        let vectors = guard::catch(|| self.source.embed(self.spec, images))
            .map_err(|panic| format!("internal error while embedding: {panic}"))??;
        if vectors.len() != images.len() {
            return Err(format!(
                "the model gave {} embeddings for {} images",
                vectors.len(),
                images.len()
            ));
        }
        for vector in &vectors {
            if vector.len() != self.spec.dims {
                return Err(format!(
                    "the model gave an embedding of {} values, not {}",
                    vector.len(),
                    self.spec.dims
                ));
            }
            if !vector.iter().all(|value| value.is_finite()) {
                return Err("the model gave a NaN or infinite value in an embedding".into());
            }
        }
        Ok(vectors)
    }
}

/// Why the embeddings were not all computed.
enum Failure {
    /// The task's stop flag was set.
    Stopped,
    /// The model failed, or a photo could not be decoded again: Stage 3 is
    /// skipped for the job.
    Model(String),
    /// The store or the system failed: so does the task.
    Task(TaskError),
}

impl From<StoreError> for Failure {
    fn from(error: StoreError) -> Self {
        Failure::Task(error.into())
    }
}

/// Stage 3 for `job`, whose analysis (`files`) is saved: computes and
/// stores the embeddings that `needs`, given those stored, says its
/// regrouping waits for, and returns the embeddings to regroup with. `None`
/// when Stage 3 is unavailable to the job, or just failed, which it reports
/// (see the module docs). Called with no engine lock held; `needs` may lock
/// the writer.
pub(super) fn prepare(
    shared: &Shared,
    job: JobId,
    config: &JobConfig,
    files: &[FileRecord],
    stop: &AtomicBool,
    needs: impl FnOnce(&Embeddings) -> Result<Vec<usize>, TaskError>,
) -> Result<Option<Embeddings>, TaskError> {
    let Some(model) = shared.stage3.model(job, config) else {
        return Ok(None);
    };
    let stored = model.stored(&lock(&shared.writer), job)?;
    let wanted = needs(&stored)?;
    if wanted.is_empty() {
        shared.clear_stage3_warning(job)?;
        return Ok(Some(stored));
    }
    match compute(shared, job, model, files, &wanted, stop) {
        Ok(()) => {
            shared.clear_stage3_warning(job)?;
            Ok(Some(model.stored(&lock(&shared.writer), job)?))
        }
        Err(Failure::Stopped) => Err(TaskError::Stopped),
        Err(Failure::Task(error)) => Err(error),
        Err(Failure::Model(error)) => {
            shared.skip_stage3(job, &error);
            Ok(None)
        }
    }
}

/// Computes and stores the embeddings of the `wanted` photos of `job`
/// (ids into `files`) with `model`, batch by batch, each photo stored an
/// `Embed` tick. `stop` is checked before each photo is decoded and before
/// each batch.
fn compute(
    shared: &Shared,
    job: JobId,
    model: Model,
    files: &[FileRecord],
    wanted: &[usize],
    stop: &AtomicBool,
) -> Result<(), Failure> {
    tracing::info!(
        job,
        model = model.spec.id,
        photos = wanted.len(),
        "embed: start"
    );
    let rows = lock(&shared.writer).file_row_ids(job)?;
    let decoder = ThumbnailDecoder::new().map_err(|error| task_failure(&error))?;
    let total = wanted.len();
    let mut done = 0;
    for chunk in wanted.chunks(CHUNK) {
        let thumbs = decode(&decoder, files, chunk, stop)?;
        for (ids, images) in chunk.chunks(BATCH).zip(thumbs.chunks(BATCH)) {
            if stop.load(Ordering::SeqCst) {
                return Err(Failure::Stopped);
            }
            let vectors = model.embed(images).map_err(Failure::Model)?;
            let stored: Vec<(i64, &[f32])> = ids
                .iter()
                .zip(&vectors)
                .map(|(&id, vector)| (rows[id], vector.as_slice()))
                .collect();
            lock(&shared.writer).put_embeddings(model.spec.id, &stored)?;
            for &id in ids {
                done += 1;
                shared.events.send(EngineEvent::JobProgress {
                    job,
                    progress: Progress {
                        stage: Stage::Embed,
                        done,
                        total,
                        current: Some(files[id].path.clone()),
                    },
                });
            }
        }
    }
    tracing::info!(job, photos = total, "embed: done");
    Ok(())
}

/// The upright thumbnails of the photos `ids` of `files`, in order, turned
/// by the EXIF orientation they were hashed with. A photo that cannot be
/// decoded again (changed or gone since the scan) fails Stage 3.
fn decode(
    decoder: &ThumbnailDecoder,
    files: &[FileRecord],
    ids: &[usize],
    stop: &AtomicBool,
) -> Result<Vec<RgbImage>, Failure> {
    let chunk: Vec<DiscoveredFile> = ids.iter().map(|&id| discovered(&files[id])).collect();
    let orientations: Vec<u16> = ids
        .iter()
        .map(|&id| files[id].exif.as_ref().map_or(1, |exif| exif.orientation))
        .collect();
    let decoded = decoder
        .decode(&chunk, &orientations, stop)
        .map_err(|error| match error {
            AnalyzeError::Cancelled => Failure::Stopped,
            other => task_failure(&other),
        })?;
    ids.iter()
        .zip(decoded)
        .map(|(&id, thumb)| {
            thumb.map_err(|error| {
                Failure::Model(format!("{}: {error}", display_path(&files[id].path)))
            })
        })
        .collect()
}

fn task_failure(error: &AnalyzeError) -> Failure {
    Failure::Task(TaskError::Failed(error_chain(error)))
}

/// The embed task: computes the embeddings `job`'s regrouping waits for,
/// then regroups the job and stores it, as a review edit does. The edits
/// whose regrouping leaves groups `Pending` queue it, and so does the start
/// for a job in review that has some. It leaves the job's status as it is.
///
/// Only a job still in Review Pending is regrouped: one the user sent to
/// its write meanwhile (Queued, Writing, Completed, ...) keeps the groups
/// that were confirmed, so a regrouping never takes a confirmed group out
/// of its write, nor deletes a written one. Embeddings already computed are
/// kept.
pub(super) fn embed(shared: &Shared, job: JobId, stop: &AtomicBool) -> Result<Settle, TaskError> {
    let (config, files) = {
        let writer = lock(&shared.writer);
        let stored = writer
            .job(job)?
            .ok_or_else(|| TaskError::Failed(format!("no job {job} in the store")))?;
        if stored.status != JobStatus::ReviewPending {
            return Ok(Settle::Unchanged);
        }
        (stored.config, writer.load_analysis(job)?.files)
    };
    let embeddings = prepare(shared, job, &config, &files, stop, |stored| {
        let writer = lock(&shared.writer);
        Ok(Draft::load(&writer, job)?
            .regroup(Some(stored))
            .needs_embeddings)
    })?;
    {
        let mut writer = lock(&shared.writer);
        let draft = Draft::load(&writer, job)?;
        // A write the user asked for while the embeddings were computed.
        if draft.status() != JobStatus::ReviewPending {
            return Ok(Settle::Unchanged);
        }
        // An edit made meanwhile that leaves groups waiting queued its own
        // embed task.
        draft.store(&mut writer, None, embeddings.as_ref())?;
    }
    shared.events.send(EngineEvent::GroupsChanged { job });
    Ok(Settle::Unchanged)
}

impl Shared {
    /// Skips Stage 3 for `job` for the rest of this run because of `error`,
    /// and the first time reports it: a warning event and the job's stored
    /// warning. Called with no engine lock held.
    fn skip_stage3(&self, job: JobId, error: &str) {
        if !self.stage3.fail(job) {
            return;
        }
        tracing::warn!(job, %error, "Stage 3 skipped");
        let message = format!("the visual check was skipped: {error}");
        let stored = lock(&self.writer).set_job_warning(job, Some(&message));
        if let Err(error) = stored {
            self.report_error(job, &error);
        }
        self.events.post(EngineEvent::Warning {
            job: Some(job),
            message,
        });
        self.events.flush();
    }

    /// Clears the stored warning of `job` that an earlier run's failure of
    /// Stage 3 left, now that it runs: the job's groups are checked again.
    fn clear_stage3_warning(&self, job: JobId) -> Result<(), StoreError> {
        let writer = lock(&self.writer);
        if writer
            .job(job)?
            .is_some_and(|stored| stored.warning.is_some())
        {
            writer.set_job_warning(job, None)?;
        }
        Ok(())
    }
}

/// The installed models as a source of embeddings: with the `ml` feature
/// only.
#[cfg(feature = "ml")]
fn installed_models(models_dir: &Path) -> Option<Arc<dyn EmbedSource>> {
    Some(Arc::new(installed::InstalledModels::new(models_dir)))
}

#[cfg(not(feature = "ml"))]
fn installed_models(_models_dir: &Path) -> Option<Arc<dyn EmbedSource>> {
    None
}

#[cfg(feature = "ml")]
mod installed {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use image::RgbImage;

    use super::EmbedSource;
    use crate::engine::lock;
    use crate::guard;
    use crate::models::embed::Embedder;
    use crate::models::{ModelSpec, installed, model_file};

    /// The models installed in a models folder, each loaded on its first
    /// use.
    pub(super) struct InstalledModels {
        models_dir: PathBuf,
        /// Each model loaded so far, or why it could not be: a model is
        /// loaded once, and again only once it is forgotten.
        loaded: Mutex<HashMap<&'static str, Result<Arc<Embedder>, String>>>,
    }

    impl InstalledModels {
        pub(super) fn new(models_dir: &Path) -> Self {
            Self {
                models_dir: models_dir.to_path_buf(),
                loaded: Mutex::default(),
            }
        }

        fn embedder(&self, model: &ModelSpec) -> Result<Arc<Embedder>, String> {
            lock(&self.loaded)
                .entry(model.id)
                .or_insert_with(|| {
                    let file = model_file(&self.models_dir, model);
                    tracing::info!(model = model.id, file = %crate::fsio::display_path(&file), "loading a model");
                    // A panic while loading is kept as a failed load too.
                    guard::catch(|| Embedder::load(model, &file))
                        .map_err(|panic| format!("internal error while loading: {panic}"))
                        .and_then(|loaded| loaded.map_err(|error| error.to_string()))
                        .map(Arc::new)
                        .map_err(|error| format!("cannot load {}: {error}", model.id))
                })
                .clone()
        }
    }

    impl EmbedSource for InstalledModels {
        fn embed(&self, model: &ModelSpec, images: &[RgbImage]) -> Result<Vec<Vec<f32>>, String> {
            let embedder = self.embedder(model)?;
            embedder.embed(images).map_err(|error| error.to_string())
        }

        fn available(&self, model: &ModelSpec) -> bool {
            installed(&self.models_dir, model)
        }

        /// Waits for a load of `model` that runs, then drops it, failed or
        /// not. An embed that holds its embedder already finishes with it.
        fn forget(&self, model: &ModelSpec) {
            lock(&self.loaded).remove(model.id);
        }
    }
}
