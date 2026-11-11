//! The inspector's two slow reads (spec §9.2): the larger preview of the selected photo
//! and what the selected stack will be written as. Both read files, so neither runs on
//! the UI thread: [`Inspect::preview`] and [`Inspect::prediction`] only look in memory and
//! ask one thread, [`INSPECT_THREAD`], for what is not there; [`Inspect::poll`] takes in
//! what it finished at the next frame.
//!
//! The inspector shows one photo or one stack at a time, so the thread keeps little:
//! - at most one preview and one prediction wait; a newer request of a kind replaces the
//!   one waiting, and a request no frame asked for again is dropped;
//! - a prediction being made is stopped through `predict_stack`'s stop flag when another
//!   is asked for, or when no frame asks for it any more; a preview being decoded cannot
//!   be stopped, so its result is kept even if nobody looks at it now;
//! - the last [`PREVIEWS`] previews (as textures) and [`PREDICTIONS`] predictions are
//!   kept, the least recently used going first. A failure, or a prediction with a photo
//!   that could not be read, is kept only while it is shown: a drive may come back, so
//!   the next time its photo or stack is shown it is asked for again.
//!
//! A result carries what it answers: a preview its job and path, a prediction its job,
//! its stack's members in layer order, the format and the RAW tone. So a late result is
//! filed under what it answers and never shows under another selection.
//!
//! A preview is one full decode, outside the engine's memory budget: one at a time
//! bounds it. Dropping [`Inspect`] does not wait for a decode in flight (a RAW can take
//! seconds and must not hold the window open): the thread ends by itself once it is done,
//! holding nothing but its queue.

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use lru::LruCache;
use photostacker_core::decode;
use photostacker_core::engine::{FileView, GroupView, JobId, JobView};
use photostacker_core::exif::ExifData;
use photostacker_core::guard;
use photostacker_core::model::{FileKind, OutputFormat, RawTone};
use photostacker_core::write::{PredictMember, StackPrediction, predict_stack};

/// The name of the inspector's thread.
pub const INSPECT_THREAD: &str = "photostacker-ui-inspect";

/// The long side of a preview.
pub const PREVIEW_SIDE: u32 = 1024;

/// The most previews kept: 8 textures of at most 1024 × 1024, about 32 MB.
pub const PREVIEWS: usize = 8;

/// The most predictions kept.
pub const PREDICTIONS: usize = 32;

/// What the thread is when it could not be started.
const NO_THREAD: &str = "the inspector's thread could not start";

/// What the inspector has of a preview or a prediction.
#[derive(Debug)]
pub enum Inspected<'a, T> {
    Ready(&'a T),
    /// Asked for, and not there yet.
    Waiting,
    /// It cannot be had, and why.
    Failed(&'a str),
}

/// What a preview answers: the photo, by its job and its path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PreviewKey {
    job: JobId,
    path: PathBuf,
}

/// What a prediction answers: the stack's members in layer order, of a job, in a format
/// and a RAW tone. A stack whose members change (a move, a swap) is another key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PredictionKey {
    job: JobId,
    members: Vec<usize>,
    format: OutputFormat,
    tone: RawTone,
}

struct PreviewRequest {
    key: PreviewKey,
    kind: FileKind,
    orientation: u16,
}

/// A member of a stack whose prediction is asked for: what `predict_stack` reads of it.
struct Member {
    id: usize,
    path: PathBuf,
    kind: FileKind,
    exif: Option<ExifData>,
}

struct PredictionRequest {
    key: PredictionKey,
    members: Vec<Member>,
    stop: Arc<AtomicBool>,
}

/// What the thread finished.
enum Done {
    Preview {
        key: PreviewKey,
        image: Result<ColorImage, String>,
    },
    /// `Ok(None)`: it was stopped.
    Prediction {
        key: PredictionKey,
        prediction: Result<Option<StackPrediction>, String>,
    },
}

/// What waits for the thread, and what it is doing.
#[derive(Default)]
struct Queue {
    preview: Option<PreviewRequest>,
    prediction: Option<PredictionRequest>,
    /// The prediction being made, with its stop flag.
    predicting: Option<(PredictionKey, Arc<AtomicBool>)>,
    /// The inspector is gone.
    quit: bool,
}

/// What [`Inspect`] and its thread share.
#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    /// Signalled when a request is queued and when the inspector quits.
    work: Condvar,
    /// What the thread finished, not taken in yet.
    done: Mutex<Vec<Done>>,
}

/// Locks `mutex`, also when a thread panicked while holding it: every holder leaves the
/// queue and the results consistent.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The inspector's previews and predictions, and the thread that makes them.
pub struct Inspect {
    previews: LruCache<PreviewKey, Result<TextureHandle, String>>,
    predictions: LruCache<PredictionKey, Result<StackPrediction, String>>,
    /// The previews asked of the thread and not back yet: waiting or being decoded.
    asked_previews: Vec<PreviewKey>,
    /// The predictions asked of the thread and not back yet: waiting or being made.
    asked_predictions: Vec<PredictionKey>,
    /// The preview and the prediction the views asked for since the last poll.
    shown_preview: Option<PreviewKey>,
    shown_prediction: Option<PredictionKey>,
    /// The requests handed to the thread since the start. For tests.
    requested: usize,
    /// Whether the thread runs (or, in the tests that hold it, would).
    working: bool,
    shared: Arc<Shared>,
}

impl Inspect {
    /// Starts the thread, [`INSPECT_THREAD`], which repaints `context` after each result.
    pub fn new(context: &egui::Context) -> Inspect {
        let mut inspect = Inspect::without_thread();
        let spawned = thread::Builder::new().name(INSPECT_THREAD.into()).spawn({
            let shared = Arc::clone(&inspect.shared);
            let context = context.clone();
            move || work(&shared, &context)
        });
        // Not joined: the thread ends by itself once the inspector quits.
        inspect.working = spawned
            .inspect_err(|error| tracing::error!("cannot start the inspector's thread: {error}"))
            .is_ok();
        inspect
    }

    /// An inspector whose requests wait, with no thread to take them.
    fn without_thread() -> Inspect {
        Inspect {
            previews: LruCache::new(NonZeroUsize::new(PREVIEWS).expect("PREVIEWS is not zero")),
            predictions: LruCache::new(
                NonZeroUsize::new(PREDICTIONS).expect("PREDICTIONS is not zero"),
            ),
            asked_previews: Vec::new(),
            asked_predictions: Vec::new(),
            shown_preview: None,
            shown_prediction: None,
            requested: 0,
            working: false,
            shared: Arc::default(),
        }
    }

    /// The preview of `file`, a photo of job `job`: its image upright, [`PREVIEW_SIDE`] on
    /// its long side (never enlarged). Asks for it if needed; looks only in memory.
    pub fn preview(&mut self, job: JobId, file: &FileView) -> Inspected<'_, TextureHandle> {
        if !self.working {
            return Inspected::Failed(NO_THREAD);
        }
        let key = PreviewKey {
            job,
            path: file.path.clone(),
        };
        self.shown_preview = Some(key.clone());
        // `contains` first: returning the borrow of a `get` that may miss would hold
        // `self` borrowed for the rest of the function.
        if self.previews.contains(&key) {
            return match self.previews.get(&key) {
                Some(Ok(texture)) => Inspected::Ready(texture),
                Some(Err(reason)) => Inspected::Failed(reason),
                None => Inspected::Waiting,
            };
        }
        if !self.asked_previews.contains(&key) {
            let request = PreviewRequest {
                key: key.clone(),
                kind: file.kind,
                orientation: file.exif.as_ref().map_or(1, |exif| exif.orientation),
            };
            self.ask_preview(request);
        }
        Inspected::Waiting
    }

    /// The preview of `file` if one is kept, and nothing asked otherwise (`Waiting`): what
    /// the inspector shows while the engine is busy, when it asks for no decode.
    pub fn kept_preview(&mut self, job: JobId, file: &FileView) -> Inspected<'_, TextureHandle> {
        let key = PreviewKey {
            job,
            path: file.path.clone(),
        };
        match self.previews.get(&key) {
            Some(Ok(texture)) => Inspected::Ready(texture),
            Some(Err(reason)) => Inspected::Failed(reason),
            None => Inspected::Waiting,
        }
    }

    /// Queues `request`, in place of the preview waiting.
    fn ask_preview(&mut self, request: PreviewRequest) {
        self.requested += 1;
        self.asked_previews.push(request.key.clone());
        let replaced = lock(&self.shared.queue).preview.replace(request);
        self.shared.work.notify_one();
        if let Some(replaced) = replaced {
            self.asked_previews.retain(|key| *key != replaced.key);
        }
    }

    /// What `group` of `job` will be written as, as far as its members' headers tell (see
    /// `predict_stack`), from the same facts the writer is given: each member's path,
    /// kind and EXIF as the job's analysis has them (`files`), in layer order, with the
    /// job's format and RAW tone. Asks for it if needed; looks only in memory. Waits
    /// while `files` does not hold every member yet.
    pub fn prediction(
        &mut self,
        job: &JobView,
        group: &GroupView,
        files: &[FileView],
    ) -> Inspected<'_, StackPrediction> {
        if !self.working {
            return Inspected::Failed(NO_THREAD);
        }
        let key = PredictionKey {
            job: job.id,
            members: group.members.clone(),
            format: job.config.output_format,
            tone: job.config.raw_tone,
        };
        self.shown_prediction = Some(key.clone());
        if self.predictions.contains(&key) {
            return match self.predictions.get(&key) {
                Some(Ok(prediction)) => Inspected::Ready(prediction),
                Some(Err(reason)) => Inspected::Failed(reason),
                None => Inspected::Waiting,
            };
        }
        if !self.asked_predictions.contains(&key) {
            let members: Option<Vec<Member>> = group
                .members
                .iter()
                .map(|&id| {
                    let file = files.get(id)?;
                    Some(Member {
                        id,
                        path: file.path.clone(),
                        kind: file.kind,
                        exif: file.exif.clone(),
                    })
                })
                .collect();
            if let Some(members) = members {
                self.ask_prediction(PredictionRequest {
                    key,
                    members,
                    stop: Arc::default(),
                });
            }
        }
        Inspected::Waiting
    }

    /// Queues `request`, in place of the prediction waiting, and stops the prediction
    /// being made: it is not the one asked for any more.
    fn ask_prediction(&mut self, request: PredictionRequest) {
        self.requested += 1;
        self.asked_predictions.push(request.key.clone());
        let replaced = {
            let mut queue = lock(&self.shared.queue);
            if let Some((_, stop)) = &queue.predicting {
                stop.store(true, Ordering::Relaxed);
            }
            queue.prediction.replace(request)
        };
        self.shared.work.notify_one();
        if let Some(replaced) = replaced {
            self.asked_predictions.retain(|key| *key != replaced.key);
        }
    }

    /// Takes in what the thread finished, turning previews into textures; then drops the
    /// requests the last frame did not ask for, and stops the prediction it did not ask
    /// for. Called at the start of each frame.
    pub fn poll(&mut self, context: &egui::Context) {
        let finished = std::mem::take(&mut *lock(&self.shared.done));
        for done in finished {
            match done {
                Done::Preview { key, image } => {
                    self.asked_previews.retain(|asked| *asked != key);
                    let texture = image.and_then(|image| texture(context, &key, image));
                    self.previews.put(key, texture);
                }
                Done::Prediction { key, prediction } => {
                    self.asked_predictions.retain(|asked| *asked != key);
                    match prediction {
                        Ok(Some(prediction)) => {
                            self.predictions.put(key, Ok(prediction));
                        }
                        // Stopped: asked for again if it is shown again.
                        Ok(None) => {}
                        Err(reason) => {
                            self.predictions.put(key, Err(reason));
                        }
                    }
                }
            }
        }
        self.forget_what_is_not_shown();
    }

    /// Drops the waiting requests no view asked for since the last poll, and stops the
    /// prediction being made unless it was asked for. A failure not shown is dropped too
    /// ([`Inspect::forget_failures_not_shown`]).
    fn forget_what_is_not_shown(&mut self) {
        let shown_preview = self.shown_preview.take();
        let shown_prediction = self.shown_prediction.take();
        self.forget_failures_not_shown(shown_preview.as_ref(), shown_prediction.as_ref());
        let (preview, prediction) = {
            let mut queue = lock(&self.shared.queue);
            if let Some((key, stop)) = &queue.predicting
                && Some(key) != shown_prediction.as_ref()
            {
                stop.store(true, Ordering::Relaxed);
            }
            let preview = queue
                .preview
                .take_if(|request| Some(&request.key) != shown_preview.as_ref());
            let prediction = queue
                .prediction
                .take_if(|request| Some(&request.key) != shown_prediction.as_ref());
            (preview, prediction)
        };
        if let Some(dropped) = preview {
            self.asked_previews.retain(|key| *key != dropped.key);
        }
        if let Some(dropped) = prediction {
            self.asked_predictions.retain(|key| *key != dropped.key);
        }
    }

    /// Drops the failed previews, and the predictions with a photo that could not be read,
    /// but the ones shown (`shown_preview`, `shown_prediction`): a failure may pass (a
    /// drive that comes back), so it is kept only while it is shown, and the next time
    /// its photo or stack is shown it is asked for again.
    fn forget_failures_not_shown(
        &mut self,
        shown_preview: Option<&PreviewKey>,
        shown_prediction: Option<&PredictionKey>,
    ) {
        let failed: Vec<PreviewKey> = self
            .previews
            .iter()
            .filter(|(key, preview)| preview.is_err() && Some(*key) != shown_preview)
            .map(|(key, _)| key.clone())
            .collect();
        for key in failed {
            self.previews.pop(&key);
        }
        let failed: Vec<PredictionKey> = self
            .predictions
            .iter()
            .filter(|(key, prediction)| !read_whole(prediction) && Some(*key) != shown_prediction)
            .map(|(key, _)| key.clone())
            .collect();
        for key in failed {
            self.predictions.pop(&key);
        }
    }

    /// Whether a preview or a prediction was asked for and is not back yet.
    pub fn busy(&self) -> bool {
        !self.asked_previews.is_empty() || !self.asked_predictions.is_empty()
    }

    /// The requests handed to the thread since the start. For tests.
    pub fn requested(&self) -> usize {
        self.requested
    }
}

impl Drop for Inspect {
    /// Tells the thread to quit and stops the prediction it is making; does not wait for
    /// a decode in flight.
    fn drop(&mut self) {
        let mut queue = lock(&self.shared.queue);
        queue.quit = true;
        if let Some((_, stop)) = &queue.predicting {
            stop.store(true, Ordering::Relaxed);
        }
        drop(queue);
        self.shared.work.notify_all();
    }
}

/// Whether `prediction` read every photo of its stack: a failure, or a photo that could not
/// be read, may pass.
fn read_whole(prediction: &Result<StackPrediction, String>) -> bool {
    prediction
        .as_ref()
        .is_ok_and(|prediction| prediction.members.iter().all(|(_, probe)| probe.is_ok()))
}

/// `image`'s texture, under `key`'s path; an image larger than the GPU takes is a
/// failure (a preview of [`PREVIEW_SIDE`] never is).
fn texture(
    context: &egui::Context,
    key: &PreviewKey,
    image: ColorImage,
) -> Result<TextureHandle, String> {
    let max_side = context.input(|input| input.max_texture_side);
    if image.width() > max_side || image.height() > max_side {
        return Err(format!(
            "the preview is larger than the screen's {max_side} pixels"
        ));
    }
    let name = format!("preview {}", key.path.to_string_lossy());
    Ok(context.load_texture(name, image, TextureOptions::LINEAR))
}

/// What the thread takes next.
enum Work {
    Preview(PreviewRequest),
    Prediction(PredictionRequest),
}

/// The thread's loop: makes the next prediction, or else the next preview, hands it over
/// and repaints, until the inspector quits.
fn work(shared: &Shared, context: &egui::Context) {
    while let Some(next) = next(shared) {
        let done = match next {
            Work::Prediction(request) => {
                let prediction = predict(&request);
                lock(&shared.queue).predicting = None;
                Done::Prediction {
                    key: request.key,
                    prediction,
                }
            }
            Work::Preview(request) => Done::Preview {
                image: preview(&request),
                key: request.key,
            },
        };
        lock(&shared.done).push(done);
        context.request_repaint();
    }
}

/// Waits for a request, then takes it: a prediction first, since it only reads headers,
/// while a preview decodes a whole photo. `None` once the inspector quits.
fn next(shared: &Shared) -> Option<Work> {
    let mut queue = lock(&shared.queue);
    loop {
        if queue.quit {
            return None;
        }
        if let Some(request) = queue.prediction.take() {
            queue.predicting = Some((request.key.clone(), Arc::clone(&request.stop)));
            return Some(Work::Prediction(request));
        }
        if let Some(request) = queue.preview.take() {
            return Some(Work::Preview(request));
        }
        queue = shared
            .work
            .wait(queue)
            .unwrap_or_else(PoisonError::into_inner);
    }
}

/// The prediction `request` asks for; `Ok(None)` when it was stopped. A panic is a
/// failure of this stack.
fn predict(request: &PredictionRequest) -> Result<Option<StackPrediction>, String> {
    let members: Vec<PredictMember<'_>> = request
        .members
        .iter()
        .map(|member| PredictMember {
            id: member.id,
            path: &member.path,
            kind: member.kind,
            exif: member.exif.as_ref(),
        })
        .collect();
    let key = &request.key;
    guard::catch(|| predict_stack(&members, key.format, key.tone, &request.stop))
        .map_err(|panic| format!("internal error while measuring the photos: {panic}"))
}

/// The preview `request` asks for. A decoder's panic is a failure of this photo.
fn preview(request: &PreviewRequest) -> Result<ColorImage, String> {
    let path = &request.key.path;
    let thumbnail =
        guard::catch(|| decode::thumbnail(path, request.kind, request.orientation, PREVIEW_SIDE))
            .map_err(|panic| format!("internal error while decoding: {panic}"))?
            .map_err(|error| error.to_string())?;
    let image = thumbnail.image;
    let size = [image.width() as usize, image.height() as usize];
    Ok(ColorImage::from_rgb(size, image.as_raw()))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use photostacker_core::JobConfig;
    use photostacker_core::engine::JobCounts;
    use photostacker_core::model::{GroupOrigin, GroupStatus, JobStatus, MlStatus};

    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(60);

    fn job(input: &Path) -> JobView {
        JobView {
            id: 1,
            created_at: String::new(),
            status: JobStatus::ReviewPending,
            scan_done: true,
            config: JobConfig::new(input.to_path_buf()),
            error: None,
            warning: None,
            counts: JobCounts::default(),
        }
    }

    fn file(idx: usize, path: PathBuf) -> FileView {
        FileView {
            idx,
            path,
            kind: FileKind::Jpeg,
            exif: None,
            error: None,
            warnings: Vec::new(),
            paired_with: None,
            thumb: None,
            pinned: false,
        }
    }

    fn group(id: i64, members: Vec<usize>) -> GroupView {
        GroupView {
            id,
            position: 0,
            status: GroupStatus::Proposed,
            origin: GroupOrigin::Auto,
            review_reset: false,
            members,
            bucket: String::new(),
            max_distance: 0,
            ml_status: MlStatus::None,
            ml_score: None,
            output_path: None,
            error: None,
            warnings: Vec::new(),
        }
    }

    /// Writes a black JPEG of `width` × `height` at `path`.
    fn jpeg(path: &Path, width: u32, height: u32) {
        image::RgbImage::new(width, height).save(path).unwrap();
    }

    /// Asks `inspect` for `ask`'s answer each frame until it is no longer waiting, as the
    /// inspector does while it shows it; whether it came ready.
    fn wait_for(
        inspect: &mut Inspect,
        context: &egui::Context,
        ask: impl Fn(&mut Inspect) -> Option<bool>,
    ) -> bool {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            inspect.poll(context);
            if let Some(ready) = ask(inspect) {
                return ready;
            }
            assert!(Instant::now() < deadline, "the answer never came");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// `inspected` as [`wait_for`] takes it: `None` while it waits.
    fn settled<T>(inspected: &Inspected<'_, T>) -> Option<bool> {
        match inspected {
            Inspected::Ready(_) => Some(true),
            Inspected::Failed(_) => Some(false),
            Inspected::Waiting => None,
        }
    }

    #[test]
    fn a_prediction_arrives_and_is_kept() {
        let folder = tempfile::tempdir().unwrap();
        let paths: Vec<PathBuf> = ["a_1.jpg", "a_2.jpg", "a_3.jpg"]
            .iter()
            .map(|name| folder.path().join(name))
            .collect();
        jpeg(&paths[0], 32, 16);
        jpeg(&paths[1], 32, 16);
        // Another resolution: left out.
        jpeg(&paths[2], 16, 16);
        let files: Vec<FileView> = paths
            .iter()
            .enumerate()
            .map(|(idx, path)| file(idx, path.clone()))
            .collect();
        let (job, stack) = (job(folder.path()), group(7, vec![0, 1, 2]));
        let context = egui::Context::default();
        let mut inspect = Inspect::new(&context);

        assert!(matches!(
            inspect.prediction(&job, &stack, &files),
            Inspected::Waiting
        ));
        assert!(inspect.busy());
        let ready = wait_for(&mut inspect, &context, |inspect| {
            settled(&inspect.prediction(&job, &stack, &files))
        });
        assert!(ready);
        assert!(!inspect.busy());
        let Inspected::Ready(prediction) = inspect.prediction(&job, &stack, &files) else {
            panic!("the prediction is not kept");
        };
        assert_eq!(prediction.canvas, Some((32, 16)));
        assert_eq!(prediction.kept, [0, 1]);
        assert_eq!(prediction.file_name.as_deref(), Some("a_1_2.psd"));
        assert_eq!(inspect.requested(), 1);

        // Kept in memory: the files are not read again.
        for path in &paths {
            std::fs::remove_file(path).unwrap();
        }
        for _ in 0..3 {
            inspect.poll(&context);
            assert!(matches!(
                inspect.prediction(&job, &stack, &files),
                Inspected::Ready(_)
            ));
        }
        assert_eq!(inspect.requested(), 1);
    }

    #[test]
    fn a_newer_request_replaces_the_waiting_one() {
        let folder = tempfile::tempdir().unwrap();
        let files: Vec<FileView> = (0..4)
            .map(|idx| file(idx, folder.path().join(format!("{idx}.jpg"))))
            .collect();
        let job = job(folder.path());
        let (first, second) = (group(1, vec![0, 1]), group(2, vec![2, 3]));
        // No thread takes the requests: they wait.
        let mut inspect = Inspect::without_thread();
        inspect.working = true;
        let waiting = |inspect: &Inspect| {
            let queue = lock(&inspect.shared.queue);
            queue
                .prediction
                .as_ref()
                .map(|request| request.key.members.clone())
        };

        let _ = inspect.prediction(&job, &first, &files);
        assert_eq!(waiting(&inspect), Some(vec![0, 1]));
        // One being made, as the thread would have taken it.
        let stop = Arc::new(AtomicBool::new(false));
        {
            let mut queue = lock(&inspect.shared.queue);
            let taken = queue.prediction.take().unwrap();
            queue.predicting = Some((taken.key, Arc::clone(&stop)));
        }
        let _ = inspect.prediction(&job, &second, &files);
        assert_eq!(waiting(&inspect), Some(vec![2, 3]));
        // The one being made is stopped: another is asked for.
        assert!(stop.load(Ordering::Relaxed));
        assert_eq!(inspect.requested(), 2);

        // A third replaces the second, which is no longer counted as asked.
        let third = group(3, vec![0, 3]);
        let _ = inspect.prediction(&job, &third, &files);
        assert_eq!(waiting(&inspect), Some(vec![0, 3]));
        assert_eq!(inspect.asked_predictions.len(), 2);
        // Asked for again while it waits: not queued twice.
        let _ = inspect.prediction(&job, &third, &files);
        assert_eq!(inspect.requested(), 3);

        // The previews alike.
        let _ = inspect.preview(job.id, &files[0]);
        let _ = inspect.preview(job.id, &files[1]);
        let waiting_preview = lock(&inspect.shared.queue)
            .preview
            .as_ref()
            .map(|request| request.key.path.clone());
        assert_eq!(waiting_preview, Some(files[1].path.clone()));
        assert_eq!(
            inspect.asked_previews,
            [PreviewKey {
                job: job.id,
                path: files[1].path.clone(),
            }]
        );

        // A frame that asks for neither drops both; only the prediction being made is
        // still out, until its (stopped) result comes back.
        let context = egui::Context::default();
        inspect.poll(&context);
        inspect.poll(&context);
        let queue = lock(&inspect.shared.queue);
        assert!(queue.preview.is_none() && queue.prediction.is_none());
        drop(queue);
        assert!(inspect.asked_previews.is_empty());
        assert_eq!(inspect.asked_predictions.len(), 1);
        assert_eq!(inspect.asked_predictions[0].members, [0, 1]);
    }

    #[test]
    fn a_preview_that_cannot_be_decoded_is_kept_as_failed() {
        let folder = tempfile::tempdir().unwrap();
        let broken = folder.path().join("broken.jpg");
        std::fs::write(&broken, b"not a jpeg").unwrap();
        let good = folder.path().join("good.jpg");
        jpeg(&good, 2048, 1024);
        let (broken, good) = (file(0, broken), file(1, good));
        let context = egui::Context::default();
        let mut inspect = Inspect::new(&context);

        let ready = wait_for(&mut inspect, &context, |inspect| {
            settled(&inspect.preview(1, &broken))
        });
        assert!(!ready);
        let Inspected::Failed(reason) = inspect.preview(1, &broken) else {
            panic!("the failure is not kept");
        };
        assert!(!reason.is_empty());
        assert_eq!(inspect.requested(), 1);
        inspect.poll(&context);
        assert!(matches!(inspect.preview(1, &broken), Inspected::Failed(_)));
        assert_eq!(inspect.requested(), 1);

        // A photo that decodes comes at the preview's size.
        let ready = wait_for(&mut inspect, &context, |inspect| {
            settled(&inspect.preview(1, &good))
        });
        assert!(ready);
        let Inspected::Ready(texture) = inspect.preview(1, &good) else {
            panic!("the preview is not kept");
        };
        assert_eq!(texture.size(), [1024, 512]);
    }

    #[test]
    fn a_failed_preview_is_asked_for_again_once_it_is_shown_again() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("away.jpg");
        let photo = file(0, path.clone());
        let context = egui::Context::default();
        let mut inspect = Inspect::new(&context);

        // On a drive that is away: it cannot be decoded.
        let ready = wait_for(&mut inspect, &context, |inspect| {
            settled(&inspect.preview(1, &photo))
        });
        assert!(!ready);
        // Kept while it is shown.
        inspect.poll(&context);
        assert!(matches!(inspect.preview(1, &photo), Inspected::Failed(_)));
        assert_eq!(inspect.requested(), 1);

        // Another selection, then the drive is back: shown again, it is asked again.
        inspect.poll(&context);
        inspect.poll(&context);
        jpeg(&path, 64, 32);
        let ready = wait_for(&mut inspect, &context, |inspect| {
            settled(&inspect.preview(1, &photo))
        });
        assert!(ready);
        assert_eq!(inspect.requested(), 2);
    }

    #[test]
    fn a_prediction_with_unreadable_photos_is_asked_for_again_once_shown_again() {
        let folder = tempfile::tempdir().unwrap();
        let paths: Vec<PathBuf> = ["a_1.jpg", "a_2.jpg"]
            .iter()
            .map(|name| folder.path().join(name))
            .collect();
        let files: Vec<FileView> = paths
            .iter()
            .enumerate()
            .map(|(idx, path)| file(idx, path.clone()))
            .collect();
        let (job, stack) = (job(folder.path()), group(7, vec![0, 1]));
        let context = egui::Context::default();
        let mut inspect = Inspect::new(&context);

        // None of its photos can be read: a prediction all the same.
        let ready = wait_for(&mut inspect, &context, |inspect| {
            settled(&inspect.prediction(&job, &stack, &files))
        });
        assert!(ready);
        let Inspected::Ready(away) = inspect.prediction(&job, &stack, &files) else {
            panic!("the prediction is not shown");
        };
        assert_eq!(away.canvas, None);
        inspect.poll(&context);
        assert!(matches!(
            inspect.prediction(&job, &stack, &files),
            Inspected::Ready(_)
        ));
        assert_eq!(inspect.requested(), 1);

        // The photos come back while another stack is shown: asked again.
        inspect.poll(&context);
        inspect.poll(&context);
        for path in &paths {
            jpeg(path, 32, 16);
        }
        let measured = wait_for(&mut inspect, &context, |inspect| {
            match inspect.prediction(&job, &stack, &files) {
                Inspected::Ready(prediction) => Some(prediction.canvas.is_some()),
                Inspected::Failed(_) => Some(false),
                Inspected::Waiting => None,
            }
        });
        assert!(measured);
        assert_eq!(inspect.requested(), 2);
    }
}
