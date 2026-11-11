//! What the app's tests share: a temporary data folder, stand-ins for the platform and
//! the model, a controller or a whole app on them, folders of photos (RAW+JPEG pairs
//! among them), a large job stored
//! without a scan, a gate that holds a scan, a write or a visual check, ways to wait
//! for the controller and to drive the app's widgets (a folder dropped on the window
//! among them), and what the inspector shows of a stack against what a PSD holds.

// Each test binary uses its own share of these.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{self, accesskit::Role};
use egui_kittest::kittest::{NodeT as _, Queryable as _};
use egui_kittest::{Harness, HarnessBuilder};
use image::RgbImage;
use photostacker_core::analyze::{Analysis, FileRecord, WORKER_THREAD_PREFIX};
use photostacker_core::engine::{ENGINE_THREAD, EmbedSource, JobId, ModelInstall, Wake};
use photostacker_core::exif::ExifData;
use photostacker_core::hash::HashPair;
use photostacker_core::model::{FileKind, JobStatus};
use photostacker_core::models::{self, ModelError, ModelSpec, UrlFailure};
use photostacker_core::{AppDirs, JobConfig, Store};
use photostacker_testkit::photos::{
    HEIGHT, JPEG_QUALITY, WIDTH, canon_exif, plain_jpeg, write_bracket, write_lone_shots,
    write_raw_bracket, write_review_photos,
};
use photostacker_testkit::{expose, jpeg_with_exif, psd_reader, scene, shift};
use photostacker_ui::app::App;
use photostacker_ui::controller::{
    Controller, LibraryProblem, OsVersion, RuntimeCheck, StartOptions,
};
use photostacker_ui::platform::{Picked, Platform};
use photostacker_ui::wake::Waker;
use tempfile::TempDir;

/// How long a test waits for the controller before it fails.
const TIMEOUT: Duration = Duration::from_secs(60);
/// The size of the app's window in the interaction tests.
const WINDOW: egui::Vec2 = egui::vec2(1360.0, 900.0);

/// A temporary data folder and the app's folders in it, so no test touches the user's
/// real app data.
pub fn data_dir() -> (TempDir, AppDirs) {
    let root = tempfile::tempdir().unwrap();
    let dirs = AppDirs::under(root.path().to_path_buf());
    (root, dirs)
}

/// The platform, scripted: the pickers answer from their queues (a cancel once a queue is
/// empty, as a dialog closed without a choice), or that the desktop has no dialog, and
/// where the folder picker starts and what is revealed are recorded.
#[derive(Default)]
pub struct FakePlatform {
    pub folders: Mutex<VecDeque<PathBuf>>,
    pub files: Mutex<VecDeque<PathBuf>>,
    /// The desktop has no file dialog: every picker answers so.
    pub no_dialogs: AtomicBool,
    /// Where each folder picker started, in order.
    pub folder_starts: Mutex<Vec<Option<PathBuf>>>,
    pub revealed: Mutex<Vec<PathBuf>>,
}

impl FakePlatform {
    /// What a picker whose queue is `queue` answers.
    fn answer(&self, queue: &Mutex<VecDeque<PathBuf>>) -> Picked {
        if self.no_dialogs.load(Ordering::SeqCst) {
            return Picked::Unavailable;
        }
        queue
            .lock()
            .unwrap()
            .pop_front()
            .map_or(Picked::Cancelled, Picked::Path)
    }
}

impl Platform for FakePlatform {
    fn pick_folder(&self, start: Option<&Path>) -> Picked {
        self.folder_starts
            .lock()
            .unwrap()
            .push(start.map(Path::to_path_buf));
        self.answer(&self.folders)
    }

    fn pick_file(&self, _start: Option<&Path>) -> Picked {
        self.answer(&self.files)
    }

    fn reveal(&self, path: &Path) -> Result<(), String> {
        self.revealed.lock().unwrap().push(path.to_path_buf());
        Ok(())
    }
}

/// Stage 3's embeddings without a model: every photo gets the same one, so every
/// ambiguous stack it checks holds.
pub struct StubEmbedder;

impl EmbedSource for StubEmbedder {
    fn embed(&self, model: &ModelSpec, images: &[RgbImage]) -> Result<Vec<Vec<f32>>, String> {
        let mut embedding = vec![0.0; model.dims];
        embedding[0] = 1.0;
        Ok(vec![embedding; images.len()])
    }
}

/// Stage 3's embeddings as [`StubEmbedder`] gives them, once the first call has passed
/// `gate`: a scan's or a regrouping's visual check waits there until the test opens it.
pub struct HeldEmbedder {
    pub gate: Arc<Gate>,
    /// Whether a visual check has reached the gate.
    pub called: AtomicBool,
}

impl HeldEmbedder {
    pub fn new(gate: &Arc<Gate>) -> HeldEmbedder {
        HeldEmbedder {
            gate: Arc::clone(gate),
            called: AtomicBool::new(false),
        }
    }
}

impl EmbedSource for HeldEmbedder {
    fn embed(&self, model: &ModelSpec, images: &[RgbImage]) -> Result<Vec<Vec<f32>>, String> {
        self.called.store(true, Ordering::SeqCst);
        self.gate.pass();
        StubEmbedder.embed(model, images)
    }
}

/// Stage 3's embeddings as [`StubEmbedder`] gives them, for a model only once its file is
/// in place, as the installed models have it: a job's visual check waits for its model.
pub struct InstalledEmbedder {
    pub models_dir: PathBuf,
}

impl EmbedSource for InstalledEmbedder {
    fn embed(&self, model: &ModelSpec, images: &[RgbImage]) -> Result<Vec<Vec<f32>>, String> {
        StubEmbedder.embed(model, images)
    }

    fn available(&self, model: &ModelSpec) -> bool {
        models::installed(&self.models_dir, model)
    }
}

/// Where the tests say ONNX Runtime is: a made-up path, found, so no test depends on the
/// library of the machine it runs on.
pub fn runtime_found() -> RuntimeCheck {
    RuntimeCheck {
        path: PathBuf::from("/opt/onnxruntime/lib/libonnxruntime.so"),
        problem: None,
    }
}

/// ONNX Runtime at a made-up path inside a Mac app, there but needing macOS 14.0, on a Mac
/// running macOS 13.6.
pub fn runtime_needing_macos() -> RuntimeCheck {
    let version = |major, minor| OsVersion {
        major,
        minor,
        patch: 0,
    };
    RuntimeCheck {
        path: PathBuf::from(
            "/Applications/Photostacker.app/Contents/Frameworks/libonnxruntime.dylib",
        ),
        problem: Some(LibraryProblem::NeedsMacos {
            needed: version(14, 0),
            running: version(13, 6),
        }),
    }
}

/// What the app says of the visual check wherever ONNX Runtime needs a newer macOS, for
/// [`runtime_needing_macos`].
pub const NEEDS_MACOS: &str = "The visual check needs macOS 14.0 or later on this Mac.";

/// Why each address of a failing [`FakeInstaller`]'s download fails.
pub const FAILED_REASON: &str = "HTTP status 404";

/// A stand-in for the models' downloader and importer, as in the core's engine tests, so
/// the models are installed without any network: a download reports progress steps, then
/// puts a sparse file of the registry's size in place, as an import does. It can fail
/// (every address of the registry's, for [`FAILED_REASON`]),
/// and a held download stops at half the model until its gate opens (then it finishes)
/// or its cancel comes (then it waits for the gate again, and ends cancelled).
#[derive(Default)]
pub struct FakeInstaller {
    /// Its downloads fail with every URL failed, its imports as a file of another model.
    pub fails: AtomicBool,
    /// Its downloads stop at half the model, at `gate`.
    pub holds: AtomicBool,
    pub gate: Arc<Gate>,
    /// The downloads and imports it was asked for.
    calls: AtomicUsize,
    /// The files it was asked to import.
    imported: Mutex<Vec<PathBuf>>,
}

impl FakeInstaller {
    pub fn new() -> Arc<FakeInstaller> {
        Arc::new(FakeInstaller::default())
    }

    pub fn failing() -> Arc<FakeInstaller> {
        let installer = FakeInstaller::default();
        installer.fails.store(true, Ordering::SeqCst);
        Arc::new(installer)
    }

    pub fn holding() -> Arc<FakeInstaller> {
        let installer = FakeInstaller::default();
        installer.holds.store(true, Ordering::SeqCst);
        Arc::new(installer)
    }

    /// The downloads and imports it was asked for.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub fn imported(&self) -> Vec<PathBuf> {
        self.imported.lock().unwrap().clone()
    }

    /// A held download: half the model in its part, reported, then the gate or the cancel.
    fn hold(
        &self,
        spec: &ModelSpec,
        models_dir: &Path,
        progress: &dyn Fn(u64, u64),
        cancel: &AtomicBool,
    ) -> Result<PathBuf, ModelError> {
        let part = models_dir.join(spec.id).join("model.onnx.part");
        std::fs::create_dir_all(part.parent().unwrap())?;
        std::fs::File::create(&part)?.set_len(spec.size / 2)?;
        progress(spec.size / 2, spec.size);
        loop {
            if cancel.load(Ordering::SeqCst) {
                self.gate.pass();
                return Err(ModelError::Cancelled);
            }
            if self.gate.is_open() {
                std::fs::remove_file(&part)?;
                return Ok(place_model(models_dir, spec));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl ModelInstall for FakeInstaller {
    fn download(
        &self,
        spec: &ModelSpec,
        models_dir: &Path,
        progress: &dyn Fn(u64, u64),
        cancel: &AtomicBool,
    ) -> Result<PathBuf, ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fails.load(Ordering::SeqCst) {
            let failures = spec.urls.iter().map(|url| UrlFailure {
                url: (*url).to_owned(),
                reason: FAILED_REASON.to_owned(),
            });
            return Err(ModelError::AllUrlsFailed(failures.collect()));
        }
        if self.holds.load(Ordering::SeqCst) {
            return self.hold(spec, models_dir, progress, cancel);
        }
        for done in [0, spec.size / 4, spec.size / 2, spec.size] {
            progress(done, spec.size);
        }
        Ok(place_model(models_dir, spec))
    }

    fn import(
        &self,
        spec: &ModelSpec,
        file: &Path,
        models_dir: &Path,
    ) -> Result<PathBuf, ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.imported.lock().unwrap().push(file.to_path_buf());
        if self.fails.load(Ordering::SeqCst) {
            return Err(ModelError::HashMismatch {
                path: file.to_path_buf(),
                id: spec.id,
                expected: spec.sha256,
                actual: "0".repeat(64),
            });
        }
        Ok(place_model(models_dir, spec))
    }
}

/// Puts a sparse model file of `spec`'s size in place, which costs no disk: the model is
/// installed, as far as the registry's check goes.
pub fn place_model(models_dir: &Path, spec: &ModelSpec) -> PathBuf {
    let file = models::model_file(models_dir, spec);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::File::create(&file)
        .unwrap()
        .set_len(spec.size)
        .unwrap();
    file
}

/// What a test's controller starts with: `dirs`, the stand-ins, ONNX Runtime found, no
/// folder of licence texts (a development build's) and no wake.
pub fn options(dirs: &AppDirs) -> StartOptions {
    StartOptions {
        dirs: dirs.clone(),
        wake: Arc::new(|| {}),
        platform: Arc::new(FakePlatform::default()),
        embedder: Some(Arc::new(StubEmbedder)),
        installer: None,
        runtime: runtime_found(),
        licences: None,
    }
}

/// A controller on `dirs`.
pub fn controller(dirs: &AppDirs) -> Controller {
    match Controller::start(options(dirs)) {
        Ok(controller) => controller,
        Err(error) => panic!("the controller cannot start: {error}"),
    }
}

/// Polls `controller` until `done` holds; fails after [`TIMEOUT`], saying it was waiting
/// for `what`.
pub fn wait_until(controller: &mut Controller, what: &str, done: impl Fn(&Controller) -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        controller.poll();
        if done(controller) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

/// Polls `controller` until its new-job form, which must be open, has the engine's answer.
pub fn wait_for_the_answer(controller: &mut Controller) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        controller.poll();
        let form = controller.form().expect("the form is open");
        if !form.submitting {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the answer"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

/// Polls `controller` until its backend is idle and no job is working.
pub fn settle(controller: &mut Controller) {
    wait_until(controller, "the controller to settle", |controller| {
        controller.is_idle() && !controller.is_working()
    });
}

/// Writes two brackets of three, two minutes apart, past the longest time window, and two
/// lone shots into `dir`.
pub fn write_bracket_folder(dir: &Path) {
    write_bracket(dir, "first", 1, 0, 3);
    write_bracket(dir, "second", 2, 120, 3);
    write_lone_shots(dir, 2);
}

/// A folder with two brackets of three, two minutes apart, and two lone shots.
pub fn bracket_folder() -> TempDir {
    let input = tempfile::tempdir().unwrap();
    write_bracket_folder(input.path());
    input
}

/// A folder with a bracket of three RAW+JPEG pairs, `brk_<n>.raf` and `brk_<n>.jpg`, with
/// Canon EXIF and an exposure ([`write_raw_bracket`]): the RAWs are stacked.
pub fn raw_bracket_folder() -> TempDir {
    let input = tempfile::tempdir().unwrap();
    write_raw_bracket(input.path(), "brk", 5);
    input
}

/// The Hamming threshold at which `b_3` of [`review_folder`] joins `b_1` and `b_2`: their
/// distance is 16.
pub const NEAR: u32 = 20;

/// The default time window of a job (spec §5.2), which the threshold edits of the tests
/// keep.
pub const TIME_WINDOW: u32 = 30;

/// Writes `a.jpg` and `b.jpg` into `dir`: two shots of one scene with a Canon's EXIF and
/// no bracket, `seconds` apart. They are one stack once the time window reaches
/// `seconds`, and both unstacked below it.
pub fn write_spaced_shots(dir: &Path, seconds: u32) {
    let image = scene(1, WIDTH, HEIGHT);
    for (name, second) in [("a.jpg", 0), ("b.jpg", seconds)] {
        let mut exif = canon_exif(second, 1);
        exif.maker_note = None;
        let bytes = jpeg_with_exif(&image, &exif.build(), JPEG_QUALITY);
        std::fs::write(dir.join(name), bytes).unwrap();
    }
}

/// Five photos without EXIF ([`write_review_photos`]): at the default threshold, the
/// stacks `{a_1, a_2}` and `{b_1, b_2}`, with `b_3` in the tray; at [`NEAR`], `b_3` joins
/// `b`.
pub fn review_folder() -> TempDir {
    let input = tempfile::tempdir().unwrap();
    write_review_photos(input.path(), 1, 2);
    input
}

/// Writes 10 photos of one scene into `dir`, five of them panned: at [`NEAR`] one stack,
/// past the visual check's floor, so the visual check embeds them in two batches (a
/// cancel while it holds the first stops the scan before the second).
pub fn write_ambiguous_stack(dir: &Path) {
    let still = scene(2, WIDTH, HEIGHT);
    let panned = shift(&still, 0.04, 0.0);
    for n in 0..5 {
        let ev = 0.1 * n as f32;
        for (name, image) in [("still", &still), ("panned", &panned)] {
            let bytes = plain_jpeg(&expose(image, ev));
            std::fs::write(dir.join(format!("{name}_{n}.jpg")), bytes).unwrap();
        }
    }
}

/// The photos of each bracket [`store_brackets`] stores.
pub const BRACKET: usize = 3;

/// Puts a job on `input` straight into the store of `dirs`: `brackets` brackets of
/// [`BRACKET`] photos, in review, with no model and no stack yet, as the core's
/// `bench_review` builds its job. Each bracket has its own pseudo-random pair of hashes,
/// its shot `j` flips their `j` low bits, and the brackets are 30 s apart in a job whose
/// time window is 4 s, so each is a bucket and one stack once the job is grouped. Nothing
/// is read from `input`, which need not exist.
/// Call it before the app starts on `dirs`: the engine locks the database once it runs.
pub fn store_brackets(dirs: &AppDirs, input: &Path, brackets: usize) -> JobId {
    store_series(dirs, input, brackets, BRACKET)
}

/// [`store_brackets`] with `count` lone photos: each has hashes of its own and is 30 s
/// from the next, past the job's 4 s window, so each is its own bucket, and all of them
/// are unstacked.
pub fn store_lone_photos(dirs: &AppDirs, input: &Path, count: usize) -> JobId {
    store_series(dirs, input, count, 1)
}

/// [`store_brackets`] with `shots` photos in each of the `series` brackets.
fn store_series(dirs: &AppDirs, input: &Path, series: usize, shots: usize) -> JobId {
    std::fs::create_dir_all(&dirs.data).unwrap();
    let mut store = Store::open(&dirs.db()).unwrap();
    let mut config = JobConfig::new(input.to_path_buf());
    // Shorter than the 29.4 s between brackets (30 s between lone photos; the window is
    // inclusive), so each series is a bucket of its own: at the default 30 s they would
    // chain into one bucket, whose regrouping is quadratic.
    config.time_window_seconds = 4;
    let job = store.insert_job(&config).unwrap();
    // SplitMix64, so every run stores the same job.
    let mut state = 0x5EED_u64;
    let mut random = move || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    let mut files = Vec::with_capacity(series * shots);
    for bracket in 0..series {
        let base = HashPair {
            phash: random(),
            dhash: random(),
        };
        for shot in 0..shots {
            let flipped = (1u64 << shot) - 1;
            let hashes = HashPair {
                phash: base.phash ^ flipped,
                dhash: base.dhash ^ flipped,
            };
            let i = bracket * shots + shot;
            files.push(FileRecord {
                path: input.join(format!("IMG_{i:06}.jpg")),
                kind: FileKind::Jpeg,
                size: 1_000_000,
                mtime_ms: (bracket * 30_000 + shot * 300) as i64,
                fingerprint: format!("stored {i}"),
                exif: Some(ExifData {
                    model: Some("Stored Camera".into()),
                    ..ExifData::default()
                }),
                hashes: Some(hashes),
                normalised: Some(hashes),
                paired_with: None,
                error: None,
            });
        }
    }
    let analysis = Analysis {
        files,
        groups: Vec::new(),
        unstacked: Vec::new(),
        warnings: Vec::new(),
        skipped: Vec::new(),
    };
    store.save_analysis(job, &analysis).unwrap();
    store.set_scan_done(job, true).unwrap();
    store.set_job_status(job, JobStatus::ReviewPending).unwrap();
    job
}

/// A closed gate: what passes it waits until the test opens it.
#[derive(Default)]
pub struct Gate {
    open: Mutex<bool>,
    opened: Condvar,
}

impl Gate {
    pub fn pass(&self) {
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.opened.wait(open).unwrap();
        }
    }

    pub fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.opened.notify_all();
    }

    pub fn is_open(&self) -> bool {
        *self.open.lock().unwrap()
    }

    /// Holds what passes it again, until the next [`Gate::open`].
    pub fn close(&self) {
        *self.open.lock().unwrap() = false;
    }

    /// A wake that holds every worker of an analysis at its first progress tick (the
    /// analysis names its workers [`WORKER_THREAD_PREFIX`]`<n>`), so a scan stays
    /// Scanning until the gate opens.
    pub fn holding_the_scan(self: &Arc<Self>) -> Wake {
        let gate = Arc::clone(self);
        Arc::new(move || {
            let on_a_worker = thread::current()
                .name()
                .is_some_and(|name| name.starts_with(WORKER_THREAD_PREFIX));
            if on_a_worker {
                gate.pass();
            }
        })
    }

    /// A wake that holds the engine's thread once `output` holds `stacks` finished
    /// stacks, until the gate opens: a write gets there at the wake of the
    /// `GroupsChanged` it sends for its `stacks`-th stack (no event comes between a
    /// stack's file taking its name and that one), so it stays Writing between that
    /// stack and the next, with what it wrote stored.
    pub fn holding_the_write(self: &Arc<Self>, output: PathBuf, stacks: usize) -> Wake {
        let gate = Arc::clone(self);
        Arc::new(move || {
            if thread::current().name() == Some(ENGINE_THREAD) && finished(&output) >= stacks {
                gate.pass();
            }
        })
    }
}

/// How many finished stacks `output` holds: its PSD files, not the `.partial` of one
/// being written.
fn finished(output: &Path) -> usize {
    std::fs::read_dir(output).map_or(0, |entries| {
        entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "psd"))
            .count()
    })
}

/// Opens its gate when dropped, so a failing test never leaves a thread held while the
/// controller joins it. Declared after the controller, it is dropped before it.
pub struct OpenOnDrop(pub Arc<Gate>);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

/// The app on `dirs`, in a 1360 × 900 window, with its engine waking the harness's
/// context as it wakes the window.
pub fn harness(dirs: &AppDirs) -> Harness<'static, App> {
    harness_with(dirs, Arc::new(|| {}))
}

/// [`harness`], whose engine also calls `wake` after each event, such as
/// [`Gate::holding_the_scan`].
pub fn harness_with(dirs: &AppDirs, wake: Wake) -> Harness<'static, App> {
    harness_on(dirs, wake, Arc::new(FakePlatform::default()), None)
}

/// [`harness`] on `platform`, which records what the app reveals.
pub fn harness_revealing(dirs: &AppDirs, platform: Arc<FakePlatform>) -> Harness<'static, App> {
    harness_on(dirs, Arc::new(|| {}), platform, None)
}

/// [`harness`] whose visual checks embed with `embedder`, such as a [`HeldEmbedder`].
pub fn harness_embedding(dirs: &AppDirs, embedder: Arc<dyn EmbedSource>) -> Harness<'static, App> {
    harness_on(
        dirs,
        Arc::new(|| {}),
        Arc::new(FakePlatform::default()),
        Some(embedder),
    )
}

/// [`harness`] whose models are installed by `installer`, and whose visual checks wait
/// for their model's file ([`InstalledEmbedder`]).
pub fn harness_installing(dirs: &AppDirs, installer: &Arc<FakeInstaller>) -> Harness<'static, App> {
    harness_with_options(dirs, Arc::new(|| {}), |options| {
        options.installer = Some(Arc::clone(installer) as Arc<dyn ModelInstall>);
        options.embedder = Some(Arc::new(InstalledEmbedder {
            models_dir: dirs.models(),
        }));
    })
}

/// The app on `dirs` and `platform`, its engine calling `wake` after each event, and
/// embedding with `embedder` (by default [`StubEmbedder`]).
fn harness_on(
    dirs: &AppDirs,
    wake: Wake,
    platform: Arc<FakePlatform>,
    embedder: Option<Arc<dyn EmbedSource>>,
) -> Harness<'static, App> {
    harness_with_options(dirs, wake, |options| {
        options.platform = platform;
        if let Some(embedder) = embedder {
            options.embedder = Some(embedder);
        }
    })
}

/// The app on `dirs`, started with [`options`] as `configure` changes them, its engine
/// calling `wake` after each event.
pub fn harness_with_options(
    dirs: &AppDirs,
    wake: Wake,
    configure: impl FnOnce(&mut StartOptions),
) -> Harness<'static, App> {
    harness_built(Harness::builder(), dirs, wake, configure)
}

/// [`harness`] whose frames are `step` seconds of input time apart, not kittest's quarter
/// of a second: a test that must stay inside the second a size waits to settle.
pub fn harness_stepping(dirs: &AppDirs, step: f32) -> Harness<'static, App> {
    harness_built(
        Harness::builder().with_step_dt(step),
        dirs,
        Arc::new(|| {}),
        |_| {},
    )
}

/// The app on `dirs`, from `builder`, in a 1360 × 900 window, started with [`options`] as
/// `configure` changes them, its engine calling `wake` after each event.
fn harness_built(
    builder: HarnessBuilder<App>,
    dirs: &AppDirs,
    wake: Wake,
    configure: impl FnOnce(&mut StartOptions),
) -> Harness<'static, App> {
    let mut harness = builder.with_size(WINDOW).build_eframe(|creation| {
        let waker = Waker::default();
        waker.attach(&creation.egui_ctx);
        let window = waker.wake();
        let mut options = options(dirs);
        configure(&mut options);
        options.wake = Arc::new(move || {
            wake();
            window();
        });
        match Controller::start(options) {
            Ok(controller) => App::new(&creation.egui_ctx, controller),
            Err(error) => panic!("the controller cannot start: {error}"),
        }
    });
    harness.step();
    harness
}

/// A file or folder dragged in from the file manager: only its path is read.
#[derive(Debug)]
pub struct DroppedPath(pub PathBuf);

impl egui::DroppedFile for DroppedPath {
    fn path(&self) -> &Path {
        &self.0
    }

    fn bytes(&self) -> Result<Vec<u8>, String> {
        Err("the app never reads a dropped file".into())
    }
}

/// Drops `folder` on the window as the file manager does, carried over it and let go, and
/// draws the frames that take the drop in.
pub fn drop_folder(harness: &mut Harness<'static, App>, folder: &Path) {
    harness.input_mut().hovered_files.push(egui::HoveredFile {
        path: Some(folder.to_path_buf()),
        ..egui::HoveredFile::default()
    });
    harness.step();
    harness.input_mut().hovered_files.clear();
    harness
        .input_mut()
        .dropped_files
        .push(Arc::new(DroppedPath(folder.to_path_buf())));
    harness.step();
    harness.step();
}

/// Draws frames, 5 ms apart, until a widget labelled `label` is drawn; fails after
/// [`TIMEOUT`].
pub fn run_until_drawn(harness: &mut Harness<'static, App>, label: &str) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        harness.step();
        if harness.query_by_label(label).is_some() {
            return;
        }
        assert!(Instant::now() < deadline, "{label:?} was never drawn");
        thread::sleep(Duration::from_millis(5));
    }
}

/// The layer names the inspector shows for the stack it shows, top layer first: in each
/// member's row, the name beside its `Check <file name>` box, which is the value of the
/// button that selects the member (`Select <file name>`).
pub fn shown_layer_names(harness: &Harness<'static, App>) -> Vec<String> {
    let rows: Vec<egui::Rect> = harness
        .query_all_by(|node| {
            node.role() == Role::CheckBox
                && node
                    .label()
                    .is_some_and(|label| label.starts_with("Check "))
        })
        .map(|node| node.rect())
        .collect();
    let labels: Vec<(egui::Rect, String)> = harness
        .query_all_by(|node| {
            node.role() == Role::Button
                && node
                    .label()
                    .is_some_and(|label| label.starts_with("Select "))
        })
        .filter_map(|node| Some((node.rect(), node.accesskit_node().value()?)))
        .collect();
    rows.iter()
        .map(|row| {
            let beside: Vec<&String> = labels
                .iter()
                .filter(|(rect, _)| {
                    rect.left() >= row.right() && (rect.center().y - row.center().y).abs() < 2.0
                })
                .map(|(_, label)| label)
                .collect();
            match beside.as_slice() {
                [name] => (*name).clone(),
                _ => panic!("the row at {row:?} shows {beside:?}"),
            }
        })
        .collect()
}

/// The layer names of the PSD at `path`, read back with the testkit's reader, top layer
/// first.
pub fn psd_layer_names(path: &Path) -> Vec<String> {
    let bytes = std::fs::read(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let psd =
        psd_reader::read(&bytes).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    // The file holds the bottom layer first.
    psd.layers
        .iter()
        .rev()
        .map(|layer| {
            layer
                .name_unicode
                .clone()
                .unwrap_or(layer.name_pascal.clone())
        })
        .collect()
}

/// Clicks the widget labelled `label`, and draws the frames that take the click in.
pub fn click(harness: &mut Harness<'static, App>, label: &str) {
    harness.get_by_label(label).click();
    harness.step();
    harness.step();
}

/// Draws frames, 5 ms apart, until `done` holds; fails after [`TIMEOUT`], saying it was
/// waiting for `what`.
pub fn run_until(harness: &mut Harness<'static, App>, what: &str, done: impl Fn(&App) -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        harness.step();
        if done(harness.state()) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(5));
    }
}
