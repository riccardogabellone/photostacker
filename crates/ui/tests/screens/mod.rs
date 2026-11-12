//! The screens of the screenshot example (`examples/screenshot.rs`), which the interaction
//! tests draw too (`every_screen_renders_in_both_themes_at_the_smallest_window`): for each,
//! what fills its data folder before the app starts, and what builds its window from a
//! controller on that folder. The example includes this file by its path.
//!
//! The app runs on a temporary data folder, never the user's, and its jobs on temporary
//! folders of synthetic photos.
//!
//! A new screen is one entry in `SCREENS`, with the functions that fill its data folder
//! and build its window.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use eframe::egui;
use image::RgbImage;
use photostacker_core::{
    AppDirs, Engine, EngineOptions, JobConfig, Stage,
    analyze::WORKER_THREAD_PREFIX,
    engine::{
        ENGINE_THREAD, EmbedSource, GroupId, JobId, ModelActivity, ModelInstall, MoveTarget, Wake,
    },
    model::{JobStatus, MlStatus, OutputFormat},
    models::{self, ModelError, ModelSpec, UrlFailure},
};
use photostacker_testkit::photos::{
    HEIGHT, JPEG_QUALITY, WIDTH, canon_exposure_exif, plain_jpeg, write_bracket, write_lone_shots,
    write_raw_bracket,
};
use photostacker_testkit::{Value, expose, jpeg_with_exif, scene, shift};
use photostacker_ui::{
    app::App,
    backend::Request,
    controller::{
        Controller, LibraryProblem, OsVersion, Question, Review, RuntimeCheck, StartError,
        StartOptions,
    },
    failed_start::FailedStart,
    platform::Native,
    views::photo::DraggedPhoto,
    wake::Waker,
};
use tempfile::TempDir;

/// How long the jobs of a screen may take to reach their states.
const PREPARE_TIMEOUT: Duration = Duration::from_secs(60);
/// The files of the `scanning` screen's folder that are not photos.
const BROKEN_FILES: usize = 2;

/// What a screen's data folder holds before the app starts: the folders its jobs read,
/// kept until the window closes, the job to select, the question to open, the folder of
/// a new-job form to submit before the window opens, the folder of a job to scan then,
/// whose scan `hold` keeps in mid-run, the folder of a job to review through the
/// controller's commands, a job to write, whose write `hold` may keep in mid-run,
/// Stage 3's embeddings instead of a model, what installs the models instead of the
/// network, what keeps ONNX Runtime from loading, if anything, and a model to download
/// before the window opens.
#[derive(Default)]
pub struct Prepared {
    _folders: Vec<TempDir>,
    select: Option<JobId>,
    ask: Option<Question>,
    new_job: Option<PathBuf>,
    scan: Option<PathBuf>,
    hold: Option<(Arc<Gate>, Hold)>,
    review: Option<PathBuf>,
    /// The folder of a job to scan, then select once in review.
    inspect: Option<PathBuf>,
    write: Option<Write>,
    embedder: Option<Arc<dyn EmbedSource>>,
    installer: Option<Arc<dyn ModelInstall>>,
    /// Where ONNX Runtime is and what keeps it from loading (missing, or needing a newer
    /// macOS); `None` when it is found at [`RUNTIME_PATH`]. Either way at a made-up path,
    /// so no screen depends on this machine's library.
    runtime: Option<RuntimeCheck>,
    /// A model to download through the controller, until its download is held or has
    /// failed.
    download: Option<&'static str>,
}

/// What a screen's gate holds.
enum Hold {
    /// The scan's workers, at their first progress tick.
    Scan,
    /// A write, once the folder holds its first stack.
    WriteAfterOne(PathBuf),
}

/// A job to review and write through the controller, as a user would.
struct Write {
    input: PathBuf,
    /// Where its stacks go: `<input>/stacked` when none.
    output: Option<PathBuf>,
    recursive: bool,
    /// A photo of each stack to reject; the others are confirmed.
    reject: Vec<&'static str>,
    /// A photo of the stack to select once written.
    select: Option<&'static str>,
}

/// Fills the data folder of a screen before the controller starts on it.
pub type Prepare = fn(&AppDirs) -> Result<Prepared, String>;
/// Builds the window of a screen, from a controller on its data folder.
pub type Setup = fn(&egui::Context, Controller) -> Box<dyn Screened>;

pub struct Screen {
    pub name: &'static str,
    pub prepare: Prepare,
    pub setup: Setup,
}

/// The screens, by name.
pub const SCREENS: &[Screen] = &[
    // The frame with nothing in it: no job yet.
    Screen {
        name: "empty",
        prepare: nothing,
        setup: app,
    },
    // The window of a start that failed: another Photostacker holds the data, there is
    // no folder for the data, or the database cannot be opened.
    Screen {
        name: "failed-start",
        prepare: nothing,
        setup: failed_start,
    },
    Screen {
        name: "failed-start-no-folder",
        prepare: nothing,
        setup: failed_start_no_folder,
    },
    Screen {
        name: "failed-start-database",
        prepare: nothing,
        setup: failed_start_database,
    },
    // Three jobs, newest first: one in review (selected), one that failed, one done.
    Screen {
        name: "jobs",
        prepare: three_jobs,
        setup: app,
    },
    // The `jobs` screen, asking whether to delete the selected job.
    Screen {
        name: "question",
        prepare: delete_question,
        setup: app,
    },
    // The new-job form with the visual check on and a similarity of its own, refused: its
    // photo folder does not exist.
    Screen {
        name: "new-job",
        prepare: missing_folder_form,
        setup: app,
    },
    // A scan held while it examines its photos, two of which it could not read.
    Screen {
        name: "scanning",
        prepare: held_scan,
        setup: app,
    },
    // The `scanning` screen, asking whether to quit while the scan runs.
    Screen {
        name: "quit-question",
        prepare: quit_question,
        setup: app,
    },
    // The `jobs` screen with the failed job selected.
    Screen {
        name: "failed",
        prepare: failed_job,
        setup: app,
    },
    // A job in review, its stacks decided through the controller: two confirmed, one
    // rejected, one made manual, one to look at again, one the visual check split, and a
    // manual stack of one photo; five photos in the tray; the stack to look at again
    // selected, with its second photo.
    Screen {
        name: "review",
        prepare: review_photos,
        setup: app,
    },
    // The `review` screen in the middle of a drag: the tray's first photo held over the
    // second stack's row.
    Screen {
        name: "review-drag",
        prepare: review_photos,
        setup: dragging,
    },
    // `review-drag` with the stack under the photo selected, as the primary stack: the
    // drop target's look over the selection's.
    Screen {
        name: "review-drag-selected",
        prepare: review_photos,
        setup: dragging_onto_the_selection,
    },
    // The inspector on a bracket of five JPEGs, one of another resolution, with two of its
    // photos checked, once its prediction has come.
    Screen {
        name: "inspector-stack",
        prepare: inspector_photos,
        setup: inspecting_the_stack,
    },
    // The inspector on the active RAW of a RAW+JPEG pair, with its exposure, once its
    // larger preview has come.
    Screen {
        name: "inspector-photo",
        prepare: inspector_photos,
        setup: inspecting_the_photo,
    },
    // A write of three brackets, held once it has written the first.
    Screen {
        name: "writing",
        prepare: held_write,
        setup: app,
    },
    // A job written: one stack written, one skipped for its photos' resolutions, one
    // failed (its output subfolder is a file), one rejected; the skipped one selected.
    Screen {
        name: "results",
        prepare: written_job,
        setup: app,
    },
    // A job in review on a bracket and three files its scan could not read, nothing
    // selected: the inspector lists them with their reasons.
    Screen {
        name: "inspector-unreadable",
        prepare: unreadable_photos,
        setup: app,
    },
    // The `inspector-*` screens' job with its JPEG bracket confirmed, asking whether to
    // write it while the RAW bracket is still proposed.
    Screen {
        name: "write-question",
        prepare: inspector_photos,
        setup: asking_to_write,
    },
    // The `jobs` screen with the job in review being deleted: its delete waits behind a
    // request the backend holds until the window closes.
    Screen {
        name: "deleting",
        prepare: three_jobs,
        setup: deleting,
    },
    // A bracket of eight in review, its last photo selected: behind the row's `+<k>`,
    // which shows it, and in the inspector.
    Screen {
        name: "review-hidden-photo",
        prepare: long_bracket,
        setup: selecting_the_last_photo,
    },
    // A job on a folder with no photo, its subfolders not looked at.
    Screen {
        name: "no-photos",
        prepare: no_photos,
        setup: app,
    },
    // A job in review with no stack: two shots of one scene, further apart than its time
    // window.
    Screen {
        name: "no-stacks",
        prepare: no_stacks,
        setup: app,
    },
    // The `jobs` screen with Settings open, the defaults of new jobs changed.
    Screen {
        name: "settings",
        prepare: three_jobs,
        setup: settings,
    },
    // Settings scrolled to their end: the About card, with the way to the licence texts.
    Screen {
        name: "about",
        prepare: three_jobs,
        setup: settings_at_the_end,
    },
    // Settings with the visual check on: its default model being downloaded, held at
    // 43 %, the other one installed.
    Screen {
        name: "models",
        prepare: models_downloading,
        setup: models_settings,
    },
    // Settings with the visual check on, without ONNX Runtime: the default model's
    // download failed, the other one is not installed.
    Screen {
        name: "models-failed",
        prepare: models_failed,
        setup: models_settings,
    },
    // Settings with the visual check on, on a Mac whose ONNX Runtime needs macOS 14.0
    // while it runs macOS 13.6; no model installed.
    Screen {
        name: "needs-macos",
        prepare: models_needing_macos,
        setup: models_settings,
    },
];

/// Where the screens say ONNX Runtime is, found or not.
const RUNTIME_PATH: &str = "/opt/onnxruntime/lib/libonnxruntime.so";
/// Where the screens say the package's licence texts are, as a Linux package has them: the
/// screens show the app as a package does, with its way to them in the About card.
const LICENCES_PATH: &str = "/usr/lib/Photostacker/licenses";
/// Where the screens of a Mac say ONNX Runtime is: inside the app.
const MAC_RUNTIME_PATH: &str =
    "/Applications/Photostacker.app/Contents/Frameworks/libonnxruntime.dylib";
/// How far the `models` screen's download is held, in percent.
const HELD_PERCENT: u64 = 43;

/// The `models` screen's data: `dinov2-base` installed (a sparse file of its size, which
/// is all the registry's check looks at), and `dinov2-small` to download, held.
fn models_downloading(dirs: &AppDirs) -> Result<Prepared, String> {
    let base = models::model("dinov2-base").ok_or("no dinov2-base in the registry")?;
    let file = models::model_file(&dirs.models(), base);
    let sparse = |file: &Path| -> std::io::Result<()> {
        fs::create_dir_all(file.parent().unwrap_or(file))?;
        photostacker_testkit::sparse_file(file, base.size)
    };
    sparse(&file).map_err(|error| format!("{}: {error}", file.display()))?;
    Ok(Prepared {
        installer: Some(Arc::new(ScreenInstaller { fails: false })),
        download: Some("dinov2-small"),
        ..Prepared::default()
    })
}

/// The `models-failed` screen's data: no ONNX Runtime, and `dinov2-small` to download,
/// which fails.
fn models_failed(_dirs: &AppDirs) -> Result<Prepared, String> {
    Ok(Prepared {
        installer: Some(Arc::new(ScreenInstaller { fails: true })),
        runtime: Some(RuntimeCheck {
            path: PathBuf::from(RUNTIME_PATH),
            problem: Some(LibraryProblem::Missing),
        }),
        download: Some("dinov2-small"),
        ..Prepared::default()
    })
}

/// The `needs-macos` screen's data: ONNX Runtime inside a Mac app, there but needing
/// macOS 14.0 on macOS 13.6, and nothing to download.
fn models_needing_macos(_dirs: &AppDirs) -> Result<Prepared, String> {
    let version = |major, minor| OsVersion {
        major,
        minor,
        patch: 0,
    };
    Ok(Prepared {
        installer: Some(Arc::new(ScreenInstaller { fails: false })),
        runtime: Some(RuntimeCheck {
            path: PathBuf::from(MAC_RUNTIME_PATH),
            problem: Some(LibraryProblem::NeedsMacos {
                needed: version(14, 0),
                running: version(13, 6),
            }),
        }),
        ..Prepared::default()
    })
}

/// The app with the visual check of new jobs on, and Settings open on its card, as the ML
/// chip opens them.
fn models_settings(context: &egui::Context, mut controller: Controller) -> Box<dyn Screened> {
    controller.set_ml_enabled(true);
    controller.open_visual_check();
    Box::new(App::new(context, controller))
}

/// The models' downloads of the `models` screens, with no network: a download writes
/// [`HELD_PERCENT`] of its model into its part, says so, and waits for its cancel (the
/// window's close); a failing one fails as two unreachable addresses would. It imports
/// nothing.
struct ScreenInstaller {
    fails: bool,
}

impl ModelInstall for ScreenInstaller {
    fn download(
        &self,
        spec: &ModelSpec,
        models_dir: &Path,
        progress: &dyn Fn(u64, u64),
        cancel: &AtomicBool,
    ) -> Result<PathBuf, ModelError> {
        if self.fails {
            let reasons = ["connection timed out", "HTTP status 404"];
            let failed = spec.urls.iter().zip(reasons);
            let failed = failed.map(|(url, reason)| UrlFailure {
                url: (*url).to_owned(),
                reason: reason.to_owned(),
            });
            return Err(ModelError::AllUrlsFailed(failed.collect()));
        }
        let folder = models_dir.join(spec.id);
        fs::create_dir_all(&folder)?;
        // Rounded up, so the whole percent is HELD_PERCENT exactly.
        let done = (spec.size * HELD_PERCENT).div_ceil(100);
        photostacker_testkit::sparse_file(&folder.join("model.onnx.part"), done)?;
        progress(done, spec.size);
        while !cancel.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(50));
        }
        Err(ModelError::Cancelled)
    }

    fn import(
        &self,
        _spec: &ModelSpec,
        file: &Path,
        _models_dir: &Path,
    ) -> Result<PathBuf, ModelError> {
        Err(ModelError::Io(std::io::Error::other(format!(
            "the screenshots import nothing, not {}",
            file.display()
        ))))
    }
}

/// Downloads the model `id` through the controller, as the question's `Download` does,
/// and waits until the download is under way or has failed.
fn download_until_held(controller: &mut Controller, id: &'static str) -> Result<(), String> {
    controller.download_model(id);
    let deadline = Instant::now() + PREPARE_TIMEOUT;
    loop {
        controller.poll();
        let under_way = controller.models().iter().any(|view| {
            view.spec.id == id
                && matches!(view.activity, ModelActivity::Downloading { done, .. } if done > 0)
        });
        if under_way || controller.model_failure(id).is_some() {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(format!("the download of {id} never got under way"));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// The app with Settings open, after new jobs were set to start with a layered TIFF and
/// a hash distance of 12.
fn settings(context: &egui::Context, mut controller: Controller) -> Box<dyn Screened> {
    let mut defaults = controller.prefs.defaults.clone();
    defaults.output_format = OutputFormat::Tiff;
    defaults.hamming_threshold = 12;
    controller.set_defaults(defaults);
    controller.open_settings();
    Box::new(App::new(context, controller))
}

/// The app with Settings open and scrolled to their end, the About card.
fn settings_at_the_end(context: &egui::Context, mut controller: Controller) -> Box<dyn Screened> {
    controller.open_settings();
    Box::new(ScrolledDown {
        app: App::new(context, controller),
    })
}

/// The app with the mouse wheel turned down over the middle of the window every frame, so
/// what scrolls there shows its end.
struct ScrolledDown {
    app: App,
}

impl eframe::App for ScrolledDown {
    fn raw_input_hook(&mut self, _context: &egui::Context, raw_input: &mut egui::RawInput) {
        let middle = raw_input
            .screen_rect
            .map_or(egui::pos2(680.0, 410.0), |screen| screen.center());
        raw_input.events.push(egui::Event::PointerMoved(middle));
        // Negative: the content moves up, showing what is below.
        raw_input.events.push(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, -400.0),
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Unknown,
            modifiers: egui::Modifiers::NONE,
        });
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.app.ui(ui, frame);
    }
}

impl Screened for ScrolledDown {
    fn ready(&self) -> bool {
        self.app.ready()
    }
}

/// The `writing` screen's folder: three Canon brackets of three, 20 s apart.
fn held_write(_dirs: &AppDirs) -> Result<Prepared, String> {
    let root = photos_root()?;
    let folder = root.path().join("Lake trip");
    fs::create_dir(&folder).map_err(|error| format!("{}: {error}", folder.display()))?;
    // Inside a new job's 30 s window: each bracket's shot numbers starting again keep
    // them apart.
    for (n, prefix) in ["IMG_0412", "IMG_0415", "IMG_0418"].into_iter().enumerate() {
        write_bracket(&folder, prefix, n as u64 + 1, 20 * n as u32, 3);
    }
    let output = folder.join("stacked");
    Ok(Prepared {
        _folders: vec![root],
        hold: Some((Arc::new(Gate::default()), Hold::WriteAfterOne(output))),
        write: Some(Write {
            input: folder,
            output: None,
            recursive: false,
            reject: Vec::new(),
            select: None,
        }),
        ..Prepared::default()
    })
}

/// The `results` screen's folder, with a subfolder for each stack: `Harbour`, a bracket
/// that is written; `Pier`, two shots of one scene, the second smaller, which the write
/// skips; `Cliffs`, a bracket whose output subfolder is a file, so its write fails; and
/// `Forest`, a bracket that is rejected. The stacks go to a `Stacks` folder beside it.
fn written_job(_dirs: &AppDirs) -> Result<Prepared, String> {
    let root = photos_root()?;
    let input = root.path().join("Lake trip");
    let output = root.path().join("Stacks");
    let folder = |name: &str| {
        let folder = input.join(name);
        fs::create_dir_all(&folder)
            .map(|()| folder.clone())
            .map_err(|error| format!("{}: {error}", folder.display()))
    };
    write_bracket(&folder("Harbour")?, "IMG_0501", 1, 0, 3);
    write_bracket(&folder("Cliffs")?, "IMG_0507", 2, 20, 3);
    write_bracket(&folder("Forest")?, "IMG_0510", 5, 40, 3);
    let pier = folder("Pier")?;
    let image = scene(6, WIDTH, HEIGHT);
    let smaller = image::imageops::resize(
        &image,
        WIDTH * 3 / 4,
        HEIGHT * 3 / 4,
        image::imageops::FilterType::Triangle,
    );
    for (name, image) in [("IMG_0504.jpg", &image), ("IMG_0505.jpg", &smaller)] {
        let path = pier.join(name);
        fs::write(&path, plain_jpeg(image))
            .map_err(|error| format!("{}: {error}", path.display()))?;
    }
    fs::create_dir(&output).map_err(|error| format!("{}: {error}", output.display()))?;
    let in_the_way = output.join("Cliffs");
    fs::write(&in_the_way, b"not a folder")
        .map_err(|error| format!("{}: {error}", in_the_way.display()))?;
    Ok(Prepared {
        _folders: vec![root],
        write: Some(Write {
            input,
            output: Some(output),
            recursive: true,
            reject: vec!["IMG_0510_1.jpg"],
            select: Some("IMG_0504.jpg"),
        }),
        ..Prepared::default()
    })
}

/// Reviews and writes `write`'s job through the controller: confirms its stacks but those
/// it rejects, writes them, and waits until the write is done, or, while `held`, until
/// it has written its first stack; then selects the job and the stack to select.
fn write_job(controller: &mut Controller, write: Write, held: bool) -> Result<(), String> {
    let mut config = JobConfig::new(write.input);
    if let Some(output) = write.output {
        config.output_dir = output;
    }
    config.recursive = write.recursive;
    controller.submit(config);
    wait(controller, "the job in review", |controller| {
        controller
            .jobs()
            .first()
            .is_some_and(|job| job.status == JobStatus::ReviewPending)
            && settled(controller)
    })?;
    let job = controller.jobs()[0].id;
    controller.select_job(job);
    wait(controller, "the review", |controller| {
        controller.review().is_some_and(Review::loaded)
    })?;
    controller.confirm_all();
    wait(controller, "the stacks confirmed", settled)?;
    for photo in &write.reject {
        controller.reject(stack_of(controller, photo)?);
    }
    wait(controller, "the decisions", settled)?;
    controller.write();
    if held {
        wait(controller, "the write's first stack", |controller| {
            controller.is_idle()
                && controller
                    .job(job)
                    .is_some_and(|view| view.status == JobStatus::Writing)
                && controller
                    .review()
                    .is_some_and(|review| review.results().written == 1)
        })?;
    } else {
        wait(controller, "the write", |controller| {
            settled(controller)
                && controller
                    .job(job)
                    .is_some_and(|view| view.status == JobStatus::Completed)
        })?;
    }
    if let Some(photo) = write.select {
        controller.select_group(stack_of(controller, photo)?, false);
    }
    let notices = controller.take_notices();
    if notices.is_empty() {
        Ok(())
    } else {
        Err(format!("the write was refused: {notices:?}"))
    }
}

/// The `inspector-*` screens' bracket of JPEGs: its first photo.
const JPEG_BRACKET: &str = "IMG_0412";
/// The `inspector-*` screens' bracket of RAW+JPEG pairs.
const RAW_BRACKET: &str = "IMG_0420";

/// The `inspector-*` screens' folder: a bracket of five JPEGs 1 EV apart, with a Canon's
/// exposure, the last smaller than the others (a member the writer leaves out), two
/// minutes after a bracket of three RAW+JPEG pairs, past the longest time window.
fn inspector_photos(_dirs: &AppDirs) -> Result<Prepared, String> {
    let root = photos_root()?;
    let folder = root.path().join("Lake trip");
    fs::create_dir(&folder).map_err(|error| format!("{}: {error}", folder.display()))?;
    write_raw_bracket(&folder, RAW_BRACKET, 7);
    let base = scene(3, WIDTH, HEIGHT);
    for shot in 1..=5u16 {
        let ev = i32::from(shot) - 3;
        let mut exif = canon_exposure_exif(120 + u32::from(shot) - 1, shot);
        exif.exif.push((0x9204, Value::SRational(vec![(ev, 1)])));
        let mut image = expose(&base, ev as f32);
        if shot == 5 {
            image = image::imageops::resize(
                &image,
                WIDTH * 3 / 4,
                HEIGHT * 3 / 4,
                image::imageops::FilterType::Triangle,
            );
        }
        let path = folder.join(format!("{JPEG_BRACKET}_{shot}.jpg"));
        fs::write(&path, jpeg_with_exif(&image, &exif.build(), JPEG_QUALITY))
            .map_err(|error| format!("{}: {error}", path.display()))?;
    }
    Ok(Prepared {
        _folders: vec![root],
        inspect: Some(folder),
        ..Prepared::default()
    })
}

/// Starts a job on `input`, waits for its review and selects it.
fn inspected_job(controller: &mut Controller, input: PathBuf) -> Result<(), String> {
    controller.submit(JobConfig::new(input));
    wait(controller, "the job in review", |controller| {
        controller
            .jobs()
            .first()
            .is_some_and(|job| job.status == JobStatus::ReviewPending)
            && settled(controller)
    })?;
    let job = controller.jobs()[0].id;
    controller.select_job(job);
    wait(controller, "the review", |controller| {
        controller.review().is_some_and(Review::loaded)
    })
}

/// The `inspector-unreadable` screen's folder: a bracket of three, then three files that
/// are not photos, named as a camera and a phone would.
fn unreadable_photos(_dirs: &AppDirs) -> Result<Prepared, String> {
    let root = photos_root()?;
    let folder = root.path().join("Lake trip");
    fs::create_dir(&folder).map_err(|error| format!("{}: {error}", folder.display()))?;
    write_bracket(&folder, "IMG_0412", 1, 0, 3);
    let broken: [(&str, &[u8]); 3] = [
        ("IMG_0415.jpg", b"not a jpeg"),
        ("IMG_0416.jpg", &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]),
        ("PXL_20261101_101500.jpg", b""),
    ];
    for (name, bytes) in broken {
        let path = folder.join(name);
        fs::write(&path, bytes).map_err(|error| format!("{}: {error}", path.display()))?;
    }
    Ok(Prepared {
        _folders: vec![root],
        inspect: Some(folder),
        ..Prepared::default()
    })
}

/// The app asking whether to write the JPEG bracket's stack, confirmed, while the RAW
/// bracket's is still proposed: as `Write 1 stack` does.
fn asking_to_write(context: &egui::Context, mut controller: Controller) -> Box<dyn Screened> {
    let first = format!("{JPEG_BRACKET}_1.jpg");
    if let Ok(stack) = stack_of(&controller, &first) {
        controller.confirm(stack);
        let confirmed = wait(&mut controller, "the stack confirmed", |controller| {
            settled(controller) && controller.write_counts().to_write == 1
        });
        match confirmed {
            Ok(()) => controller.write_from_the_review(),
            Err(error) => eprintln!("{error}"),
        }
    }
    Box::new(App::new(context, controller))
}

/// The app with the job in review being deleted: the backend holds a request until the
/// window closes, so the delete waits behind it.
fn deleting(context: &egui::Context, mut controller: Controller) -> Box<dyn Screened> {
    // The job's review shown first, as the user would see it before deleting the job.
    let shown = wait(&mut controller, "the review", |controller| {
        controller.review().is_some_and(Review::loaded) && settled(controller)
    });
    if let Err(error) = shown {
        eprintln!("{error}");
    }
    let gate = Arc::new(Gate::default());
    let held = Arc::clone(&gate);
    controller.send(Request::Run {
        job: None,
        command: Box::new(move |_engine| {
            held.pass();
            Ok(())
        }),
    });
    if let Some(job) = controller.selected().map(|job| job.id) {
        controller.delete(job);
    }
    Box::new(Deleting {
        _release: OpenOnDrop(gate),
        app: App::new(context, controller),
    })
}

/// The app while a delete waits. Its gate opens first, when the window closes, so the
/// backend's held request ends before the controller's drop waits for it.
struct Deleting {
    _release: OpenOnDrop,
    app: App,
}

impl eframe::App for Deleting {
    fn logic(&mut self, context: &egui::Context, frame: &mut eframe::Frame) {
        self.app.logic(context, frame);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.app.ui(ui, frame);
    }
}

impl Screened for Deleting {
    fn ready(&self) -> bool {
        self.app.ready()
    }
}

/// The `review-hidden-photo` screen's folder: a bracket of eight, more than a row shows.
fn long_bracket(_dirs: &AppDirs) -> Result<Prepared, String> {
    let root = photos_root()?;
    let folder = root.path().join("Lake trip");
    fs::create_dir(&folder).map_err(|error| format!("{}: {error}", folder.display()))?;
    write_bracket(&folder, "IMG_0430", 4, 0, 8);
    Ok(Prepared {
        _folders: vec![root],
        inspect: Some(folder),
        ..Prepared::default()
    })
}

/// The app with the bracket's last photo selected, as its name in the inspector selects it.
fn selecting_the_last_photo(
    context: &egui::Context,
    mut controller: Controller,
) -> Box<dyn Screened> {
    match photo_id(&controller, "IMG_0430_8.jpg") {
        Ok(photo) => controller.select_photo(photo),
        Err(error) => eprintln!("{error}"),
    }
    Box::new(App::new(context, controller))
}

/// The `no-photos` screen's folder: no photo, but a subfolder of them, which the job does
/// not look at.
fn no_photos(_dirs: &AppDirs) -> Result<Prepared, String> {
    let root = photos_root()?;
    let folder = root.path().join("Lake trip");
    let inside = folder.join("Day 1");
    fs::create_dir_all(&inside).map_err(|error| format!("{}: {error}", inside.display()))?;
    write_bracket(&inside, "IMG_0440", 1, 0, 3);
    Ok(Prepared {
        _folders: vec![root],
        inspect: Some(folder),
        ..Prepared::default()
    })
}

/// The `no-stacks` screen's folder: two shots of one scene with a Canon's exposure and no
/// bracket, 45 s apart, further than a new job's time window.
fn no_stacks(_dirs: &AppDirs) -> Result<Prepared, String> {
    let root = photos_root()?;
    let folder = root.path().join("Lake trip");
    fs::create_dir(&folder).map_err(|error| format!("{}: {error}", folder.display()))?;
    let image = scene(4, WIDTH, HEIGHT);
    for (name, second) in [("IMG_0460.jpg", 0), ("IMG_0461.jpg", 45)] {
        let mut exif = canon_exposure_exif(second, 1);
        exif.maker_note = None;
        let path = folder.join(name);
        fs::write(&path, jpeg_with_exif(&image, &exif.build(), JPEG_QUALITY))
            .map_err(|error| format!("{}: {error}", path.display()))?;
    }
    Ok(Prepared {
        _folders: vec![root],
        inspect: Some(folder),
        ..Prepared::default()
    })
}

/// The app with the JPEG bracket's stack selected, its second and third photos checked.
fn inspecting_the_stack(context: &egui::Context, mut controller: Controller) -> Box<dyn Screened> {
    let first = format!("{JPEG_BRACKET}_1.jpg");
    if let Ok(stack) = stack_of(&controller, &first) {
        controller.select_group(stack, false);
        for shot in [2, 3] {
            if let Ok(photo) = photo_id(&controller, &format!("{JPEG_BRACKET}_{shot}.jpg")) {
                controller.toggle_member(photo);
            }
        }
    }
    Box::new(App::new(context, controller))
}

/// The app with the second RAW of the RAW+JPEG bracket selected.
fn inspecting_the_photo(context: &egui::Context, mut controller: Controller) -> Box<dyn Screened> {
    if let Ok(photo) = photo_id(&controller, &format!("{RAW_BRACKET}_2.raf")) {
        controller.select_photo(photo);
    }
    Box::new(App::new(context, controller))
}

fn nothing(_dirs: &AppDirs) -> Result<Prepared, String> {
    Ok(Prepared::default())
}

/// A folder of 24 photos, eight brackets of three, and two files that are not photos
/// at all, named to sort first, so the scan's workers take them before they are held.
fn held_scan(_dirs: &AppDirs) -> Result<Prepared, String> {
    let root = photos_root()?;
    let folder = root.path().join("Lake trip");
    fs::create_dir(&folder).map_err(|error| format!("{}: {error}", folder.display()))?;
    for n in 0..BROKEN_FILES {
        let broken = folder.join(format!("DSC_{:04}.jpg", n + 1));
        fs::write(&broken, b"not a jpeg")
            .map_err(|error| format!("{}: {error}", broken.display()))?;
    }
    for n in 0..8 {
        write_bracket(
            &folder,
            &format!("IMG_{:02}", n + 1),
            n + 1,
            30 * n as u32,
            3,
        );
    }
    Ok(Prepared {
        _folders: vec![root],
        scan: Some(folder),
        hold: Some((Arc::new(Gate::default()), Hold::Scan)),
        ..Prepared::default()
    })
}

/// [`held_scan`], with the question the window asks when it is closed meanwhile.
fn quit_question(dirs: &AppDirs) -> Result<Prepared, String> {
    Ok(Prepared {
        ask: Some(Question::Quit),
        ..held_scan(dirs)?
    })
}

/// The `review` screen's folder: four Canon brackets of three; then, without EXIF (one
/// bucket by their modification times), two shots of a beach and a third panned, which
/// joins them at a hash distance of 20; two shots of a cliff and a third cropped, which
/// joins them at 20 too, but which [`ShapeEmbedder`] sets apart; and [`LONE_SHOTS`]
/// lone shots.
fn review_photos(_dirs: &AppDirs) -> Result<Prepared, String> {
    let root = photos_root()?;
    let folder = root.path().join("Lake trip");
    fs::create_dir(&folder).map_err(|error| format!("{}: {error}", folder.display()))?;
    for n in 0..4 {
        write_bracket(
            &folder,
            &format!("IMG_{:02}", n + 1),
            n + 1,
            30 * n as u32,
            3,
        );
    }
    let beach = scene(21, WIDTH, HEIGHT);
    let cliff = scene(22, WIDTH, HEIGHT);
    let cropped = image::imageops::crop_imm(&cliff, 0, 0, WIDTH, CROPPED_HEIGHT).to_image();
    let photos = [
        ("beach_1", beach.clone()),
        ("beach_2", expose(&beach, 0.3)),
        ("beach_3", shift(&beach, 0.04, 0.0)),
        ("cliff_1", cliff.clone()),
        ("cliff_2", expose(&cliff, 0.3)),
        ("cliff_3", cropped),
    ];
    for (name, image) in photos {
        let path = folder.join(format!("{name}.jpg"));
        fs::write(&path, plain_jpeg(&image))
            .map_err(|error| format!("{}: {error}", path.display()))?;
    }
    write_lone_shots(&folder, LONE_SHOTS);
    Ok(Prepared {
        _folders: vec![root],
        review: Some(folder),
        embedder: Some(Arc::new(ShapeEmbedder)),
        ..Prepared::default()
    })
}

/// The lone shots of the `review` screen: one is moved into a bracket, one starts a stack
/// of its own, and three stay in the tray, with the cropped cliff and the bracket shot
/// that hashes apart.
const LONE_SHOTS: usize = 5;

/// The height `cliff_3` is cropped to: a wider shape than the other photos'.
const CROPPED_HEIGHT: u32 = 400;

/// Stage 3's embeddings for the `review` screen, without a model: a photo of the usual
/// 3:2 shape gets one embedding, a wider one (`cliff_3`) another at right angles to it,
/// so the visual check splits the stack that holds both and keeps every other whole.
struct ShapeEmbedder;

impl EmbedSource for ShapeEmbedder {
    fn embed(&self, model: &ModelSpec, images: &[RgbImage]) -> Result<Vec<Vec<f32>>, String> {
        let embeddings = images
            .iter()
            .map(|image| {
                let wide = image.width() as f32 / image.height() as f32 > 1.55;
                let mut embedding = vec![0.0; model.dims];
                embedding[usize::from(wide)] = 1.0;
                embedding
            })
            .collect();
        Ok(embeddings)
    }
}

/// Reviews the job on `input` through the controller, as a user would. The beach's two
/// shots are confirmed at the default hash distance; raised to 20, it adds the panned
/// shot to them (a new stack, to look at again, as it stays through the regroupings
/// after) and the cropped one to the cliff's, which the visual check splits off again.
/// The first lone shot is moved into the fourth bracket, which makes it manual, and the
/// last starts a stack of its own; two brackets are confirmed and one rejected, and the
/// beach's second photo selected.
fn review_job(controller: &mut Controller, input: PathBuf) -> Result<(), String> {
    let mut config = JobConfig::new(input);
    config.ml_model = Some("dinov2-small".into());
    controller.submit(config);
    wait(controller, "the job in review", |controller| {
        controller
            .jobs()
            .first()
            .is_some_and(|job| job.status == JobStatus::ReviewPending)
            && settled(controller)
    })?;
    let job = controller.jobs()[0].id;
    controller.select_job(job);
    wait(controller, "the review", |controller| {
        controller.review().is_some_and(Review::loaded)
    })?;
    controller.confirm(stack_of(controller, "beach_1.jpg")?);
    wait(controller, "the beach confirmed", settled)?;
    regroup(controller, 20)?;
    let manual = stack_of(controller, "IMG_04_1.jpg")?;
    controller.move_photo(
        photo_id(controller, "lone_00.jpg")?,
        MoveTarget::Group(manual),
    );
    let last = format!("lone_{:02}.jpg", LONE_SHOTS - 1);
    controller.move_photo(photo_id(controller, &last)?, MoveTarget::NewGroup);
    controller.confirm(stack_of(controller, "IMG_01_1.jpg")?);
    controller.confirm(stack_of(controller, "IMG_02_1.jpg")?);
    // The third bracket's darkest shot hashes apart from the other two, in the tray.
    controller.reject(stack_of(controller, "IMG_03_2.jpg")?);
    wait(controller, "the decisions", settled)?;
    controller.select_photo(photo_id(controller, "beach_2.jpg")?);
    let notices = controller.take_notices();
    if notices.is_empty() {
        Ok(())
    } else {
        Err(format!("the review was refused: {notices:?}"))
    }
}

/// Regroups the open review at a hash distance of `hamming`, in its own time window, and
/// waits for it and its visual check.
fn regroup(controller: &mut Controller, hamming: u32) -> Result<(), String> {
    let window = controller
        .review()
        .ok_or("no review is open")?
        .time_window_seconds;
    controller.set_thresholds(hamming, window, None);
    wait(
        controller,
        "the regrouping and its visual check",
        |controller| {
            settled(controller)
                && controller.review().is_some_and(|review| {
                    review.hamming == hamming
                        && review
                            .groups
                            .iter()
                            .all(|group| group.ml_status != MlStatus::Pending)
                })
        },
    )
}

/// Whether the controller has nothing waiting and no job is working.
fn settled(controller: &Controller) -> bool {
    controller.is_idle() && !controller.is_working()
}

/// Polls `controller` until `done` holds, or fails after [`PREPARE_TIMEOUT`] saying it
/// waited for `what`.
fn wait(
    controller: &mut Controller,
    what: &str,
    done: impl Fn(&Controller) -> bool,
) -> Result<(), String> {
    let deadline = Instant::now() + PREPARE_TIMEOUT;
    loop {
        controller.poll();
        if done(controller) {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(format!("timed out waiting for {what}"));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// The id of the open review's photo named `name`.
fn photo_id(controller: &Controller, name: &str) -> Result<usize, String> {
    controller
        .review()
        .and_then(|review| {
            review
                .files
                .iter()
                .position(|file| file.path.file_name().is_some_and(|n| n == name))
        })
        .ok_or_else(|| format!("no photo {name}"))
}

/// The stack of the open review that holds the photo named `name`.
fn stack_of(controller: &Controller, name: &str) -> Result<GroupId, String> {
    let photo = photo_id(controller, name)?;
    controller
        .review()
        .and_then(|review| {
            review
                .groups
                .iter()
                .find(|group| group.members.contains(&photo))
        })
        .map(|group| group.id)
        .ok_or_else(|| format!("{name} is in no stack: {}", stack_names(controller)))
}

/// The open review's stacks, each its photos' names: what a failure to prepare it shows.
fn stack_names(controller: &Controller) -> String {
    let Some(review) = controller.review() else {
        return "no review".into();
    };
    let name = |file: usize| {
        review
            .files
            .get(file)
            .and_then(|file| file.path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    review
        .groups
        .iter()
        .map(|group| {
            let names: Vec<String> = group.members.iter().map(|&file| name(file)).collect();
            format!("[{}]", names.join(" "))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// [`three_jobs`], with the failed job selected.
fn failed_job(dirs: &AppDirs) -> Result<Prepared, String> {
    three_jobs_selecting(dirs, JobStatus::Failed)
}

/// The form on a made-up folder, which is not there.
fn missing_folder_form(_dirs: &AppDirs) -> Result<Prepared, String> {
    Ok(Prepared {
        new_job: Some(PathBuf::from("/home/you/Pictures/Lake trip")),
        ..Prepared::default()
    })
}

fn app(context: &egui::Context, controller: Controller) -> Box<dyn Screened> {
    Box::new(App::new(context, controller))
}

/// A screen's window, and whether it shows what it waits for.
pub trait Screened: eframe::App {
    /// Whether nothing the screen shows is on its way.
    fn ready(&self) -> bool {
        true
    }
}

impl Screened for App {
    /// The inspector's preview or prediction has come.
    fn ready(&self) -> bool {
        !self.inspecting()
    }
}

impl Screened for FailedStart {}

impl Screened for Dragging {
    fn ready(&self) -> bool {
        self.app.ready()
    }
}

/// Where the `review-drag` screen holds its photo: on the second stack's row, at the
/// default size.
const DRAG_AT: egui::Pos2 = egui::pos2(640.0, 300.0);

/// The app with the first photo of the review's tray held at [`DRAG_AT`].
fn dragging(context: &egui::Context, controller: Controller) -> Box<dyn Screened> {
    held(context, controller)
}

/// [`dragging`], with the second stack, the one under the pointer, selected alone.
fn dragging_onto_the_selection(
    context: &egui::Context,
    mut controller: Controller,
) -> Box<dyn Screened> {
    let second = controller
        .review()
        .and_then(|review| review.groups.get(1))
        .map(|group| group.id);
    if let Some(second) = second {
        controller.select_group(second, false);
    }
    held(context, controller)
}

/// The app with the first photo of the review's tray held at [`DRAG_AT`].
fn held(context: &egui::Context, controller: Controller) -> Box<dyn Screened> {
    let photo = controller.review().and_then(|review| {
        review.unstacked.first().map(|&file| DraggedPhoto {
            job: review.job,
            file,
        })
    });
    Box::new(Dragging {
        app: App::new(context, controller),
        photo,
    })
}

/// The app in the middle of a drag, held as egui holds one: the drag's payload set and
/// the pointer over [`DRAG_AT`], every frame.
struct Dragging {
    app: App,
    photo: Option<DraggedPhoto>,
}

impl eframe::App for Dragging {
    fn raw_input_hook(&mut self, _context: &egui::Context, raw_input: &mut egui::RawInput) {
        raw_input.events.push(egui::Event::PointerMoved(DRAG_AT));
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if let Some(photo) = self.photo {
            egui::DragAndDrop::set_payload(ui.ctx(), photo);
        }
        self.app.ui(ui, frame);
    }
}

/// The window of an app that could not start, because another Photostacker holds its
/// data (at a made-up path). The controller is not needed.
fn failed_start(context: &egui::Context, _controller: Controller) -> Box<dyn Screened> {
    let db = PathBuf::from("/home/you/.local/share/photostacker/photostacker.db");
    Box::new(FailedStart::new(
        context,
        &StartError::AlreadyRunning(db),
        Arc::new(Native),
    ))
}

/// The window of an app that could not start, because there is no folder for its data.
fn failed_start_no_folder(context: &egui::Context, _controller: Controller) -> Box<dyn Screened> {
    Box::new(FailedStart::new(
        context,
        &StartError::NoAppDirs,
        Arc::new(Native),
    ))
}

/// The window of an app that could not start, because its database cannot be opened: a
/// controller started on a data folder whose database is a folder, which the store cannot
/// open, so the words are the engine's own.
fn failed_start_database(context: &egui::Context, _controller: Controller) -> Box<dyn Screened> {
    let broken = || -> Result<StartError, String> {
        let data = tempfile::tempdir().map_err(|error| format!("no temporary folder: {error}"))?;
        let dirs = AppDirs::under(data.path().to_path_buf());
        fs::create_dir_all(dirs.db()).map_err(|error| error.to_string())?;
        let started = Controller::start(StartOptions {
            dirs,
            wake: Arc::new(|| {}),
            platform: Arc::new(Native),
            embedder: None,
            installer: None,
            runtime: RuntimeCheck {
                path: PathBuf::from(RUNTIME_PATH),
                problem: None,
            },
            licences: Some(PathBuf::from(LICENCES_PATH)),
        });
        match started {
            Ok(_) => Err("a controller started on a database that is a folder".into()),
            Err(error) => Ok(error),
        }
    };
    let error = broken().unwrap_or_else(|message| {
        eprintln!("{message}");
        StartError::NoAppDirs
    });
    Box::new(FailedStart::new(context, &error, Arc::new(Native)))
}

/// A closed gate: what passes it waits until it opens.
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    opened: Condvar,
}

impl Gate {
    fn open(&self) {
        *self.open.lock().expect("never poisoned") = true;
        self.opened.notify_all();
    }

    /// A wake that holds every worker of an analysis at its first progress tick.
    fn holding_the_scan(self: &Arc<Self>) -> Wake {
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

    /// A wake that holds the engine's thread once `output` holds a finished stack: at the
    /// wake of the `GroupsChanged` a write sends for its first stack, so the write stays
    /// Writing between its first stack and its second.
    fn holding_the_write(self: &Arc<Self>, output: PathBuf) -> Wake {
        let gate = Arc::clone(self);
        Arc::new(move || {
            let on_the_engine = thread::current().name() == Some(ENGINE_THREAD);
            if on_the_engine && holds_a_stack(&output) {
                gate.pass();
            }
        })
    }

    /// Waits until the gate is open.
    fn pass(&self) {
        let mut open = self.open.lock().expect("never poisoned");
        while !*open {
            open = self.opened.wait(open).expect("never poisoned");
        }
    }
}

/// Whether `output` holds a finished stack: a PSD, not the `.partial` of one being
/// written.
fn holds_a_stack(output: &Path) -> bool {
    fs::read_dir(output).is_ok_and(|mut entries| {
        entries.any(|entry| {
            entry.is_ok_and(|entry| entry.path().extension().is_some_and(|ext| ext == "psd"))
        })
    })
}

/// Opens its gate when dropped, so an early return never leaves a worker held.
pub struct OpenOnDrop(Arc<Gate>);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

/// Where a screen's folders of photos go: a temporary folder named `Pictures`, so a
/// picture of the screen shows `<temporary folder>/Pictures/Lake trip` rather than a
/// random name; while another run has that name, one named `Pictures-<random>`.
fn photos_root() -> Result<TempDir, String> {
    let named = tempfile::Builder::new()
        .prefix("Pictures")
        .rand_bytes(0)
        .tempdir();
    named
        .or_else(|_| tempfile::Builder::new().prefix("Pictures-").tempdir())
        .map_err(|error| format!("no temporary folder: {error}"))
}

/// A folder named `name` in `root`, with two brackets of three and two lone shots.
fn photo_folder(root: &Path, name: &str) -> Result<PathBuf, String> {
    let folder = root.join(name);
    fs::create_dir(&folder).map_err(|error| format!("{}: {error}", folder.display()))?;
    write_bracket(&folder, "first", 1, 0, 3);
    write_bracket(&folder, "second", 2, 30, 3);
    write_lone_shots(&folder, 2);
    Ok(folder)
}

/// [`three_jobs_selecting`] the job in review.
fn three_jobs(dirs: &AppDirs) -> Result<Prepared, String> {
    three_jobs_selecting(dirs, JobStatus::ReviewPending)
}

/// Runs three jobs on an engine of its own: one written without review, one whose
/// folder goes away before its scan (as `scan_failure_sets_failed_with_the_message` in
/// the core's engine tests does it: the first job's scan is held at a gate meanwhile, so
/// the second cannot start), and one left in review; selects the one that ends in
/// `selected`. The engine is shut down before the app starts on the same data.
fn three_jobs_selecting(dirs: &AppDirs, selected: JobStatus) -> Result<Prepared, String> {
    let text = |error: &dyn std::fmt::Display| error.to_string();
    let root = photos_root()?;
    let city = photo_folder(root.path(), "City walk")?;
    let studio = photo_folder(root.path(), "Studio test")?;
    let lake = photo_folder(root.path(), "Lake trip")?;
    for folder in [&dirs.data, &dirs.cache] {
        fs::create_dir_all(folder).map_err(|error| text(&error))?;
    }
    let gate = Arc::new(Gate::default());
    let engine = Engine::start(EngineOptions {
        db: dirs.db(),
        thumbs_dir: dirs.thumbs(),
        models_dir: dirs.models(),
        embedder: None,
        installer: None,
        wake: gate.holding_the_scan(),
    })
    .map_err(|error| text(&error))?;
    // Dropped before the engine, whose drop waits for the held workers.
    let _open = OpenOnDrop(Arc::clone(&gate));

    let mut written = JobConfig::new(city);
    written.auto_confirm = true;
    let written = engine.submit(written).map_err(|error| text(&error))?;
    let failed = engine
        .submit(JobConfig::new(studio.clone()))
        .map_err(|error| text(&error))?;
    fs::remove_dir_all(&studio).map_err(|error| text(&error))?;
    let reviewed = engine
        .submit(JobConfig::new(lake))
        .map_err(|error| text(&error))?;
    gate.open();

    let expected = [
        (written, JobStatus::Completed),
        (failed, JobStatus::Failed),
        (reviewed, JobStatus::ReviewPending),
    ];
    let deadline = Instant::now() + PREPARE_TIMEOUT;
    loop {
        let reached = expected
            .iter()
            .all(|(job, status)| engine.job(*job).is_ok_and(|view| view.status == *status));
        if reached {
            break;
        }
        if Instant::now() > deadline {
            return Err("the jobs did not reach review, done and failed".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    drop(engine);
    let select = expected
        .iter()
        .find(|(_, status)| *status == selected)
        .map(|(job, _)| *job);
    Ok(Prepared {
        _folders: vec![root],
        select,
        ..Prepared::default()
    })
}

/// [`three_jobs`], with the question whether to delete the job in review.
fn delete_question(dirs: &AppDirs) -> Result<Prepared, String> {
    let mut prepared = three_jobs(dirs)?;
    prepared.ask = prepared.select.map(Question::DeleteJob);
    Ok(prepared)
}

/// A controller, after what lets its held scan or write go, if it has one: a tuple drops
/// its first element first, so the task is let go before the engine, which waits for it,
/// is dropped.
pub type Held = (Option<OpenOnDrop>, Controller);

/// A new temporary data folder, filled for `screen`, a controller on it, and the waker
/// its engine wakes.
pub fn start(screen: &Screen) -> Result<(TempDir, Prepared, Held, Waker), String> {
    let data = tempfile::tempdir().map_err(|error| format!("no temporary folder: {error}"))?;
    let dirs = AppDirs::under(data.path().to_path_buf());
    let mut prepared = (screen.prepare)(&dirs)
        .map_err(|error| format!("the {} screen cannot be prepared: {error}", screen.name))?;
    let waker = Waker::default();
    let wake = match &prepared.hold {
        Some((gate, what)) => {
            let hold = match what {
                Hold::Scan => gate.holding_the_scan(),
                Hold::WriteAfterOne(output) => gate.holding_the_write(output.clone()),
            };
            let window = waker.wake();
            let wake: Wake = Arc::new(move || {
                window();
                hold();
            });
            wake
        }
        None => waker.wake(),
    };
    let mut controller = Controller::start(StartOptions {
        dirs,
        wake,
        platform: Arc::new(Native),
        embedder: prepared.embedder.clone(),
        installer: prepared.installer.clone(),
        runtime: prepared.runtime.clone().unwrap_or_else(|| RuntimeCheck {
            path: PathBuf::from(RUNTIME_PATH),
            problem: None,
        }),
        licences: Some(PathBuf::from(LICENCES_PATH)),
    })
    .map_err(|error| format!("the app cannot start: {error}"))?;
    // Declared after the controller, so an early return drops it first.
    let open = prepared
        .hold
        .as_ref()
        .map(|(gate, _)| OpenOnDrop(Arc::clone(gate)));
    if let Some(job) = prepared.select {
        controller.select_job(job);
    }
    if let Some(question) = prepared.ask.take() {
        controller.ask(question);
    }
    if let Some(input) = prepared.new_job.take() {
        submit_form(&mut controller, input)?;
    }
    if let Some(input) = prepared.scan.take() {
        scan_until_held(&mut controller, input)?;
    }
    if let Some(input) = prepared.review.take() {
        review_job(&mut controller, input)?;
    }
    if let Some(input) = prepared.inspect.take() {
        inspected_job(&mut controller, input)?;
    }
    if let Some(write) = prepared.write.take() {
        let held = matches!(prepared.hold, Some((_, Hold::WriteAfterOne(_))));
        write_job(&mut controller, write, held)?;
    }
    if let Some(id) = prepared.download.take() {
        download_until_held(&mut controller, id)?;
    }
    Ok((data, prepared, (open, controller), waker))
}

/// Starts a job on `input` and waits until its scan, held by the screen's gate, examines
/// its photos and has reported the [`BROKEN_FILES`] it could not read; selects the job.
fn scan_until_held(controller: &mut Controller, input: PathBuf) -> Result<(), String> {
    controller.submit(JobConfig::new(input));
    let deadline = Instant::now() + PREPARE_TIMEOUT;
    loop {
        controller.poll();
        let held = controller.jobs().first().filter(|job| {
            job.status == JobStatus::Scanning
                && controller
                    .progress(job.id)
                    .is_some_and(|progress| progress.stage == Stage::Hash)
                && controller.scan_errors(job.id).len() == BROKEN_FILES
        });
        if let Some(job) = held.map(|job| job.id) {
            controller.select_job(job);
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err("the scan was never held while examining its photos".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Opens the new-job form on `input`, with the visual check on (for this run only) and a
/// similarity of its own, submits it and waits for the engine's answer.
fn submit_form(controller: &mut Controller, input: PathBuf) -> Result<(), String> {
    controller.prefs.ml_enabled = true;
    controller.open_new_job(Some(input));
    if let Some(form) = controller.form() {
        form.fields.cosine_threshold = Some(0.8);
    }
    controller.submit_form();
    let deadline = Instant::now() + PREPARE_TIMEOUT;
    while controller.form().is_some_and(|form| form.submitting) {
        if Instant::now() > deadline {
            return Err("the engine never answered the form".into());
        }
        thread::sleep(Duration::from_millis(10));
        controller.poll();
    }
    Ok(())
}
