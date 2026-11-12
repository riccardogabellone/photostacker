//! Drives the real widgets of the app over a real engine, so these fail if a control is
//! miswired or missing: the toolbar, the jobs list, the status bar, the question dialog,
//! the toasts, the new-job form, the views of a job outside its review, and the review
//! with its stacks, sliders, bulk actions and shortcuts, its tray, the photos dragged
//! between them, the inspector of the selected job, stack or photo, Settings with the
//! models of Stage 3, closing the window while a job runs, and the window of a start that
//! failed (spec §9.2, §9.3). At the end, whole runs from an empty data folder to files on
//! disk through the widgets alone, every screen of the screenshot example (`screens`)
//! drawn at the smallest window, and frames drawn while the backend is held (§12.2).

mod common;
mod screens;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use common::{
    DroppedPath, FAILED_REASON, FakeInstaller, FakePlatform, Gate, HeldEmbedder, NEAR, NEEDS_MACOS,
    OpenOnDrop, TIME_WINDOW, bracket_folder, click, controller, data_dir, drop_folder, harness,
    harness_embedding, harness_installing, harness_revealing, harness_with, harness_with_options,
    options, place_model, psd_layer_names, raw_bracket_folder, review_folder, run_until,
    run_until_drawn, runtime_needing_macos, shown_layer_names, store_brackets, store_lone_photos,
    write_ambiguous_stack, write_bracket_folder, write_spaced_shots,
};
use eframe::egui::{
    self, Key, Modifiers, ThemePreference, ViewportId,
    accesskit::{Role, Toggled},
};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT as _, Queryable as _};
use photostacker_core::Stage;
use photostacker_core::engine::{EmbedSource, GroupId, GroupView, JobId, ModelActivity};
use photostacker_core::model::{GroupOrigin, GroupStatus, JobStatus, OutputFormat};
use photostacker_core::models::{self, MODELS};
use photostacker_core::{AppDirs, JobConfig, Store};
use photostacker_testkit::photos::{HEIGHT, WIDTH, plain_jpeg, write_bracket, write_lone_shots};
use photostacker_testkit::scene;
use photostacker_ui::app::{App, CloseDecision};
use photostacker_ui::backend::Request;
use photostacker_ui::controller::{
    Controller, MlState, Question, Review, Selection, StartError, View,
};
use photostacker_ui::failed_start::FailedStart;
use photostacker_ui::format::{display_path, folder_name, sentence};
use photostacker_ui::prefs::{FormDefaults, ThemeChoice};
use photostacker_ui::views::inspector::PREVIEW_WAITS;
use photostacker_ui::views::photo::DraggedPhoto;

/// What the centre says while there is no job.
const INVITATION: &str = "Drop a folder of photos here, or start a new job.";
/// What the dialog asks before `job` is deleted: it names the job.
fn delete_question(harness: &Harness<'static, App>, job: JobId) -> String {
    let name = harness.state().controller().job_name(job);
    format!("Delete {name}? Your photos and the stacks already written are not touched.")
}
/// What a retry of a job in review is told.
const IN_REVIEW: &str = "That job is in review now, so this no longer applies.";
/// What the centre says while files are dragged over the window.
const DROP_HINT: &str = "Drop to start a new job";
/// The width of the jobs list, where the centre starts, at the smallest window: the list
/// gives way from its default of 260 px, so the centre keeps 400 px.
const JOBS_WIDTH: f32 = 240.0;
/// The narrowest the centre gets while the window is 900 px wide or more.
const CENTRE_MIN_WIDTH: f32 = 400.0;
/// The margin of the centre on each side, inside which its content runs.
const CENTRE_MARGIN: f32 = 8.0;
/// The height of a stack's row.
const ROW_HEIGHT: f32 = 136.0;
/// The centre's Cancel, named apart from the selected row's.
const CANCEL_JOB: &str = "Cancel job";
/// The centre's Retry, named apart from the selected row's.
const RETRY_JOB: &str = "Retry job";
/// The scan's progress bar, or its spinner while a stage reports only when done.
const SCAN_PROGRESS: &str = "Scan progress";
/// What the centre says of a cancelled job.
const CANCELLED: &str = "This job was cancelled.";
/// How long a test waits for something drawn before it fails.
const TIMEOUT: Duration = Duration::from_secs(60);

/// The label of the row of the job on `input`.
fn row(input: &Path, status: &str) -> String {
    format!("Job {}, {status}", folder_name(input))
}

fn submit(harness: &Harness<'static, App>, input: &Path) {
    let config = JobConfig::new(input.to_path_buf());
    harness.state().controller().submit(config);
}

/// Whether every job of the app is in review, and there are `count` of them.
fn in_review(app: &App, count: usize) -> bool {
    let jobs = app.controller().jobs();
    jobs.len() == count
        && jobs
            .iter()
            .all(|job| job.status == JobStatus::ReviewPending)
        && app.controller().is_idle()
}

/// The app on `dirs` with one job, on `input`, in review; returns the job.
fn reviewing(harness: &mut Harness<'static, App>, input: &Path) -> JobId {
    submit(harness, input);
    run_until(harness, "the job in review", |app| in_review(app, 1));
    harness.state().controller().jobs()[0].id
}

fn status(app: &App, job: JobId) -> Option<JobStatus> {
    app.controller().job(job).map(|view| view.status)
}

/// Draws frames until the selected job's review is loaded, so the status bar counts its
/// tray.
fn run_until_the_review_is_loaded(harness: &mut Harness<'static, App>) {
    run_until(harness, "the review to load", |app| {
        app.controller()
            .review()
            .is_some_and(|review| review.loaded())
    });
}

/// Every text the last frame painted, widgets' and toasts' alike.
fn painted_texts(harness: &Harness<'static, App>) -> Vec<String> {
    fn collect(shape: &egui::Shape, texts: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(text) => texts.push(text.galley.text().to_owned()),
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| collect(shape, texts)),
            _ => {}
        }
    }
    let mut texts = Vec::new();
    for clipped in &harness.output().shapes {
        collect(&clipped.shape, &mut texts);
    }
    texts
}

/// Whether the last frame painted `text`, alone or in a longer text (a toast's starts with
/// its icon).
fn painted(harness: &Harness<'static, App>, text: &str) -> bool {
    painted_texts(harness)
        .iter()
        .any(|painted| painted.contains(text))
}

/// Draws frames, 5 ms apart, until `text` is painted.
fn run_until_painted(harness: &mut Harness<'static, App>, text: &str) {
    let deadline = Instant::now() + TIMEOUT;
    while !painted(harness, text) {
        assert!(Instant::now() < deadline, "{text:?} was never painted");
        thread::sleep(Duration::from_millis(5));
        harness.step();
    }
}

/// How long the last frame asked egui to wait before the next.
fn repaint_delay(harness: &Harness<'static, App>) -> Duration {
    harness
        .output()
        .viewport_output
        .get(&ViewportId::ROOT)
        .map_or(Duration::MAX, |viewport| viewport.repaint_delay)
}

/// Clicks at `position`, wherever it is, and draws the frames that take the click in.
fn click_at(harness: &mut Harness<'static, App>, position: egui::Pos2) {
    click_at_with(harness, position, Modifiers::NONE);
}

/// [`click_at`], holding `modifiers`.
fn click_at_with(harness: &mut Harness<'static, App>, position: egui::Pos2, modifiers: Modifiers) {
    harness.hover_at(position);
    for pressed in [true, false] {
        harness.event(egui::Event::PointerButton {
            pos: position,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers,
        });
    }
    harness.step();
    harness.step();
}

/// The files directly in `folder`, with their bytes.
fn files_in(folder: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fs::read_dir(folder)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_file())
        .map(|path| {
            let bytes = fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect()
}

#[test]
fn the_empty_app_invites_a_first_job() {
    let (_data, dirs) = data_dir();
    let mut harness = harness(&dirs);
    run_until(&mut harness, "the jobs to be read", |app| {
        app.controller().is_idle()
    });
    assert!(harness.query_by_label(INVITATION).is_some());
    assert!(harness.query_by_label("Start a new job").is_some());
    // The toolbar's, which is always there.
    assert!(harness.query_by_label("New job").is_some());
}

#[test]
fn a_job_shows_in_the_list_with_its_status_and_can_be_selected() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let mut harness = harness(&dirs);
    let job = reviewing(&mut harness, input.path());

    let label = row(input.path(), "Review");
    assert!(harness.query_by_label(&label).is_some());
    assert!(harness.query_by_label(INVITATION).is_none());
    assert!(
        harness.query_by_label("Review").is_some(),
        "its status chip"
    );
    assert!(harness.query_by_label("8 photos · 2 stacks").is_some());
    // Only the selected job shows its actions.
    assert!(harness.query_by_label("Delete").is_none());
    assert!(harness.state().controller().selected().is_none());

    click(&mut harness, &label);
    let selected = harness.state().controller().selected().map(|view| view.id);
    assert_eq!(selected, Some(job));
    assert!(harness.query_by_label("Open output folder").is_some());
    assert!(harness.query_by_label("Delete").is_some());
    // A job in review has nothing to retry or to cancel.
    assert!(harness.query_by_label("Retry").is_none());
    assert!(harness.query_by_label("Cancel").is_none());
    // The row and the status bar both give the selected job's totals; the status bar
    // also counts the tray once the review has read it.
    run_until_the_review_is_loaded(&mut harness);
    assert_eq!(harness.query_all_by_label("8 photos · 2 stacks").count(), 1);
    assert!(
        harness
            .query_by_label("8 photos · 2 stacks · 2 unstacked")
            .is_some()
    );
}

#[test]
fn cancel_stops_a_scan_from_the_list() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let gate = Arc::new(Gate::default());
    let mut harness = harness_with(&dirs, gate.holding_the_scan());
    let _open = OpenOnDrop(Arc::clone(&gate));
    submit(&harness, input.path());
    run_until(&mut harness, "the scan's hashing", |app| {
        let controller = app.controller();
        controller.jobs().first().is_some_and(|job| {
            job.status == JobStatus::Scanning
                && controller
                    .progress(job.id)
                    .is_some_and(|progress| progress.stage == Stage::Hash)
        })
    });
    let job = harness.state().controller().jobs()[0].id;

    click(&mut harness, &row(input.path(), "Scanning"));
    click(&mut harness, "Cancel");
    // The cancel must reach the engine before the scan is let go, or the scan finishes.
    run_until(&mut harness, "the cancel to run", |app| {
        app.controller().is_idle()
    });
    gate.open();
    run_until(&mut harness, "the cancelled job", |app| {
        status(app, job) == Some(JobStatus::Cancelled) && app.controller().is_idle()
    });
    assert!(
        harness
            .query_by_label(&row(input.path(), "Cancelled"))
            .is_some()
    );

    click(&mut harness, "Retry");
    run_until(&mut harness, "the job scanned again", |app| {
        in_review(app, 1)
    });
    assert!(
        harness
            .query_by_label(&row(input.path(), "Review"))
            .is_some()
    );
}

#[test]
fn delete_asks_first_and_removes_the_job() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let photos = files_in(input.path());
    let mut harness = harness(&dirs);
    let job = reviewing(&mut harness, input.path());
    click(&mut harness, &row(input.path(), "Review"));

    click(&mut harness, "Delete");
    assert_eq!(
        harness.state().controller().question(),
        Some(&Question::DeleteJob(job))
    );
    let asked = delete_question(&harness, job);
    assert!(asked.starts_with(&format!("Delete {}? ", folder_name(input.path()))));
    assert!(harness.query_by_label(&asked).is_some());
    // The safe answer has the keyboard.
    assert!(harness.get_by_label("Keep").is_focused());
    click(&mut harness, "Keep");
    assert_eq!(harness.state().controller().question(), None);
    assert!(harness.query_by_label(&asked).is_none());

    // Esc keeps it too.
    click(&mut harness, "Delete");
    assert!(harness.state().controller().question().is_some());
    harness.key_press(Key::Escape);
    harness.step();
    assert_eq!(harness.state().controller().question(), None);

    // And so does a click outside the dialog.
    click(&mut harness, "Delete");
    assert!(harness.state().controller().question().is_some());
    click_at(&mut harness, egui::pos2(700.0, 850.0));
    assert_eq!(harness.state().controller().question(), None);
    run_until(&mut harness, "nothing to do", |app| {
        app.controller().is_idle()
    });
    assert!(harness.state().controller().job(job).is_some());

    click(&mut harness, "Delete");
    click(&mut harness, "Delete job");
    run_until(&mut harness, "the job to go", |app| {
        app.controller().jobs().is_empty() && app.controller().is_idle()
    });
    assert!(harness.query_by_label(INVITATION).is_some());
    assert_eq!(files_in(input.path()), photos);
}

#[test]
fn two_jobs_on_one_folder_have_rows_of_their_own() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let mut harness = harness(&dirs);
    let older = reviewing(&mut harness, input.path());
    submit(&harness, input.path());
    run_until(&mut harness, "the second job in review", |app| {
        in_review(app, 2)
    });
    let name = folder_name(input.path());
    let first = format!("Job {name}, Review");
    let second = format!("Job {name} (2), Review");
    assert!(harness.query_by_label(&first).is_some());
    assert!(harness.query_by_label(&second).is_some());

    // Each row's Delete deletes its own job.
    click(&mut harness, &second);
    click(&mut harness, "Delete");
    click(&mut harness, "Delete job");
    run_until(&mut harness, "the newer job to go", |app| {
        app.controller().jobs().len() == 1 && app.controller().is_idle()
    });
    assert_eq!(harness.state().controller().jobs()[0].id, older);
    assert!(harness.query_by_label(&second).is_none());

    click(&mut harness, &first);
    click(&mut harness, "Delete");
    click(&mut harness, "Delete job");
    run_until(&mut harness, "the older job to go", |app| {
        app.controller().jobs().is_empty() && app.controller().is_idle()
    });
}

#[test]
fn the_theme_toggle_cycles_and_is_saved() {
    let (_data, dirs) = data_dir();
    let mut harness = harness(&dirs);
    let preference =
        |harness: &Harness<'static, App>| harness.ctx.options(|options| options.theme_preference);

    click(&mut harness, "Theme: follow the system");
    assert_eq!(harness.state().controller().prefs.theme, ThemeChoice::Light);
    assert_eq!(preference(&harness), ThemePreference::Light);
    click(&mut harness, "Theme: light");
    assert_eq!(harness.state().controller().prefs.theme, ThemeChoice::Dark);
    assert_eq!(preference(&harness), ThemePreference::Dark);
    assert!(harness.query_by_label("Theme: dark").is_some());
    run_until(&mut harness, "the theme to be saved", |app| {
        app.controller().is_idle()
    });
    drop(harness);
    assert_eq!(controller(&dirs).prefs.theme, ThemeChoice::Dark);

    // The next start shows the saved choice, and the cycle goes on from it.
    let mut harness = common::harness(&dirs);
    click(&mut harness, "Theme: dark");
    assert_eq!(
        harness.state().controller().prefs.theme,
        ThemeChoice::System
    );
    assert_eq!(preference(&harness), ThemePreference::System);
}

#[test]
fn an_engine_error_is_a_toast() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let mut harness = harness(&dirs);
    let job = reviewing(&mut harness, input.path());
    assert!(!painted(&harness, IN_REVIEW));

    // A job in review has nothing to retry: the engine refuses.
    harness.state_mut().controller_mut().retry(job);
    run_until_painted(&mut harness, IN_REVIEW);
}

#[test]
fn the_smallest_window_shows_every_panel_with_three_jobs() {
    let (_data, dirs) = data_dir();
    let root = tempfile::tempdir().unwrap();
    let names = [
        "trip",
        "a folder whose name is far longer than the list of jobs is wide",
        "city",
    ];
    let inputs: Vec<PathBuf> = names.iter().map(|name| root.path().join(name)).collect();
    let mut harness = harness(&dirs);
    for input in &inputs {
        fs::create_dir(input).unwrap();
        write_bracket_folder(input);
        submit(&harness, input);
    }
    run_until(&mut harness, "three jobs in review", |app| {
        in_review(app, 3)
    });
    harness.set_size(egui::vec2(900.0, 560.0));
    harness.step();
    click(&mut harness, &row(&inputs[1], "Review"));

    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 560.0));
    let rect = |label: &str| harness.get_by_label(label).rect();
    // The centre's header, named by the selected job's whole path.
    let centre = display_path(&inputs[1]);
    for label in [
        "New job",
        "Settings",
        "Theme: follow the system",
        "Open output folder",
        "Delete",
        centre.as_str(),
        "Inspector",
    ] {
        assert!(screen.contains_rect(rect(label)), "{label} is off screen");
    }
    for input in &inputs {
        let job = rect(&row(input, "Review"));
        assert!(
            screen.contains_rect(job),
            "{} is off screen",
            input.display()
        );
        // The long name is cut short: no row widens the list past its default width.
        assert!(job.max.x <= 260.0, "{} is {job:?}", input.display());
        assert!(job.max.x <= rect(&centre).min.x);
    }
    assert!(rect(&centre).max.x <= rect("Inspector").min.x);
    // The status bar gives the selected job's totals along the bottom.
    run_until_the_review_is_loaded(&mut harness);
    let totals: Vec<egui::Rect> = harness
        .query_all_by_label("8 photos · 2 stacks · 2 unstacked")
        .map(|node| node.rect())
        .collect();
    assert!(
        totals
            .iter()
            .any(|totals| screen.contains_rect(*totals) && totals.min.y > 500.0),
        "{totals:?}"
    );
}

#[test]
fn an_app_with_finished_jobs_goes_idle() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let mut harness = harness(&dirs);
    let job = reviewing(&mut harness, input.path());
    harness.state_mut().controller_mut().retry(job);
    run_until_painted(&mut harness, IN_REVIEW);

    // The toast counts down while frames are drawn; once it is gone, nothing asks for one.
    let mut frames = 0;
    while painted(&harness, IN_REVIEW) || repaint_delay(&harness) <= Duration::from_secs(1) {
        frames += 1;
        assert!(frames < 2_000, "the app never went idle");
        harness.step();
    }
    for _ in 0..3 {
        harness.step();
        assert!(repaint_delay(&harness) > Duration::from_secs(1));
    }

    // An event from the engine, while no frame is drawn, asks for one at once.
    let woken = Arc::new(AtomicBool::new(false));
    harness.ctx.set_request_repaint_callback({
        let woken = Arc::clone(&woken);
        move |info| {
            if info.delay == Duration::ZERO {
                woken.store(true, Ordering::SeqCst);
            }
        }
    });
    harness.state_mut().controller_mut().delete(job);
    let deadline = Instant::now() + TIMEOUT;
    while !woken.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "the engine's event drew no frame"
        );
        thread::sleep(Duration::from_millis(5));
    }
    run_until(&mut harness, "the job to go", |app| {
        app.controller().jobs().is_empty()
    });
}

/// What the app asks when its window is closed while a job runs.
const QUIT_QUESTION: &str = "A job is still running. If you quit now it stops, and continues \
                             the next time you open Photostacker.";
/// What the window of a start that failed says when another app holds the data.
const ALREADY_RUNNING: &str = "Photostacker is already running. Its window has your jobs. If \
    you have just closed it, it may still be finishing: try again in a moment.";
/// More photos than a scan has workers (at most 16), so a scan held at its workers' first
/// ticks has photos left to examine, and a quit leaves it unfinished.
const MANY: usize = 24;

/// Asks the window to close, as its title bar's close button does, and draws the frame
/// that takes the request in; whether the app cancelled the close.
fn request_close(harness: &mut Harness<'static, App>) -> bool {
    harness
        .input_mut()
        .viewports
        .entry(ViewportId::ROOT)
        .or_default()
        .events
        .push(egui::ViewportEvent::Close);
    harness.step();
    harness
        .output()
        .viewport_output
        .get(&ViewportId::ROOT)
        .is_some_and(|viewport| {
            viewport
                .commands
                .contains(&egui::ViewportCommand::CancelClose)
        })
}

/// Draws frames until the controller's one job is examining its photos, held by the
/// scan's gate; returns the job.
fn run_until_hashing(harness: &mut Harness<'static, App>) -> JobId {
    run_until(harness, "the scan's hashing", |app| {
        let controller = app.controller();
        controller.jobs().first().is_some_and(|job| {
            job.status == JobStatus::Scanning
                && controller
                    .progress(job.id)
                    .is_some_and(|progress| progress.stage == Stage::Hash)
        })
    });
    harness.state().controller().jobs()[0].id
}

#[test]
fn closing_while_a_job_runs_asks_first() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    write_lone_shots(input.path(), MANY);
    let gate = Arc::new(Gate::default());
    let mut harness = harness_with(&dirs, gate.holding_the_scan());
    let _open = OpenOnDrop(Arc::clone(&gate));
    submit(&harness, input.path());
    let job = run_until_hashing(&mut harness);

    // The rule: a job is scanning, so the app asks.
    assert_eq!(harness.state_mut().on_close_request(), CloseDecision::Ask);
    harness.step();
    harness.step();
    let question =
        |harness: &Harness<'static, App>| harness.state().controller().question().cloned();
    assert_eq!(question(&harness), Some(Question::Quit));
    assert!(harness.query_by_label(QUIT_QUESTION).is_some());
    // The cautious answer has the keyboard.
    assert!(harness.get_by_label("Keep working").is_focused());
    click(&mut harness, "Keep working");
    assert_eq!(question(&harness), None);
    assert_eq!(status(harness.state(), job), Some(JobStatus::Scanning));

    // The window's own request: the close is cancelled and the question asked, in place
    // of a question already open.
    harness
        .state_mut()
        .controller_mut()
        .ask(Question::DeleteJob(job));
    assert!(request_close(&mut harness));
    assert_eq!(question(&harness), Some(Question::Quit));
    // A second request while it is open asks nothing more, and still keeps the window.
    assert!(request_close(&mut harness));
    harness.step();
    assert_eq!(question(&harness), Some(Question::Quit));
    assert_eq!(harness.query_all_by_label(QUIT_QUESTION).count(), 1);
    // Esc keeps working.
    harness.key_press(Key::Escape);
    harness.step();
    assert_eq!(question(&harness), None);
    assert_eq!(status(harness.state(), job), Some(JobStatus::Scanning));

    // Quit: the window is hidden, then closed, so it never shows unanswering while the
    // engine stops; the next request closes it.
    assert!(request_close(&mut harness));
    harness.step();
    harness.get_by_label("Quit").click();
    // The frames of the pointer's move, press and release: the click is the last one's.
    harness.step();
    let commands = harness
        .output()
        .viewport_output
        .get(&ViewportId::ROOT)
        .map(|viewport| viewport.commands.clone())
        .unwrap_or_default();
    let hidden = commands
        .iter()
        .position(|command| *command == egui::ViewportCommand::Visible(false));
    let closed = commands
        .iter()
        .position(|command| *command == egui::ViewportCommand::Close);
    assert!(
        matches!((hidden, closed), (Some(hidden), Some(closed)) if hidden < closed),
        "{commands:?}"
    );
    harness.step();
    assert_eq!(question(&harness), None);
    assert_eq!(harness.state_mut().on_close_request(), CloseDecision::Close);
    assert!(!request_close(&mut harness));

    // The engine leaves the job for the next start (§5.7). Its shutdown sets the quit flag,
    // then waits for the held workers, so the gate must open only after the flag is set,
    // or the scan could finish first. Nothing outside the engine shows that moment: the
    // flag sets no status and sends no event, and the lock is released only once the
    // workers are joined, past the gate. So the gate opens after a delay far longer than
    // the drop takes to reach the flag (the backend's join, then the shutdown's first
    // steps: milliseconds).
    let opener = {
        let gate = Arc::clone(&gate);
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(2));
            gate.open();
        })
    };
    drop(harness);
    opener.join().unwrap();
    {
        let store = Store::open_read_only(&dirs.db()).unwrap();
        let stored = store.job(job).unwrap().unwrap();
        assert_eq!(
            (stored.status, stored.scan_done),
            (JobStatus::Scanning, false)
        );
    }
    let mut again = common::harness(&dirs);
    run_until(&mut again, "the job resumed and scanned", |app| {
        status(app, job) == Some(JobStatus::ReviewPending) && app.controller().is_idle()
    });
    let view = again.state().controller().job(job).unwrap();
    assert_eq!((view.counts.files, view.counts.errors), (MANY, 0));
}

#[test]
fn the_quit_question_outlives_the_job_it_was_asked_for() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let gate = Arc::new(Gate::default());
    let mut harness = harness_with(&dirs, gate.holding_the_scan());
    let _open = OpenOnDrop(Arc::clone(&gate));
    submit(&harness, input.path());
    let job = run_until_hashing(&mut harness);
    assert!(request_close(&mut harness));

    // The job ends while the question is open: the question stays.
    gate.open();
    run_until(&mut harness, "the job in review", |app| in_review(app, 1));
    assert_eq!(status(harness.state(), job), Some(JobStatus::ReviewPending));
    assert_eq!(
        harness.state().controller().question(),
        Some(&Question::Quit)
    );
    // A request to close while it is open still keeps the window for the answer.
    assert!(request_close(&mut harness));
    click(&mut harness, "Keep working");
    assert_eq!(harness.state().controller().question(), None);
    // Nothing runs now: the window closes at once.
    assert!(!request_close(&mut harness));

    // Quit too still closes it.
    harness.state_mut().controller_mut().ask(Question::Quit);
    harness.step();
    click(&mut harness, "Quit");
    assert_eq!(harness.state().controller().question(), None);
    assert_eq!(harness.state_mut().on_close_request(), CloseDecision::Close);
}

#[test]
fn closing_an_idle_app_closes_at_once() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let mut harness = harness(&dirs);
    // A job in review: nothing is queued, scanning or writing.
    reviewing(&mut harness, input.path());
    assert!(!harness.state().controller().is_working());

    assert_eq!(harness.state_mut().on_close_request(), CloseDecision::Close);
    assert!(!request_close(&mut harness));
    assert_eq!(harness.state().controller().question(), None);
}

#[test]
fn a_size_still_waiting_when_quit_is_answered_is_saved() {
    let (_data, dirs) = data_dir();
    // Frames a millisecond apart: the width below never reaches the second after which
    // `show` would save it, so only the close can.
    let mut harness = common::harness_stepping(&dirs, 0.001);
    harness.step();
    // The inspector widened by a screen reader's step: its width waits for its second.
    inspector_splitter(&harness).increment_accesskit();
    harness.step();
    let width = harness.state().controller().prefs.panels.inspector;
    assert!(width > 321.0, "{width}");

    harness.state_mut().controller_mut().ask(Question::Quit);
    // A dialog's first frame measures it, unseen.
    harness.step();
    harness.step();
    click(&mut harness, "Quit");
    assert!(harness.state().controller().question().is_none());
    // The window is hidden now, so eframe takes the close that follows in `logic` alone,
    // which it runs without a pass of the ui (`egui::Context::run_logic`).
    let mut hidden = egui::RawInput::default();
    hidden
        .viewports
        .entry(ViewportId::ROOT)
        .or_default()
        .events
        .push(egui::ViewportEvent::Close);
    let context = harness.ctx.clone();
    let mut frame = eframe::Frame::_new_kittest();
    let _output = context.run_logic(&hidden, |context| {
        eframe::App::logic(harness.state_mut(), context, &mut frame);
    });
    drop(harness);
    assert_eq!(controller(&dirs).prefs.panels.inspector, width);
}

/// The window of a start that failed with `error`, drawn once, revealing on `platform`.
fn failed_start(error: StartError, platform: Arc<FakePlatform>) -> Harness<'static, FailedStart> {
    let mut harness = Harness::builder()
        .with_size(egui::vec2(900.0, 560.0))
        .build_eframe(move |creation| FailedStart::new(&creation.egui_ctx, &error, platform));
    // The focus asked for in the first frame shows from the second.
    harness.step();
    harness.step();
    harness
}

/// Why a controller on `dirs` cannot start, which it must not.
fn start_error(dirs: &AppDirs) -> StartError {
    match Controller::start(options(dirs)) {
        Ok(_) => panic!("a controller started on {}", dirs.data.display()),
        Err(error) => error,
    }
}

#[test]
fn a_second_app_on_the_same_data_says_it_is_already_running() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let mut harness = harness(&dirs);
    let first = reviewing(&mut harness, input.path());

    let error = start_error(&dirs);
    assert!(
        matches!(&error, StartError::AlreadyRunning(db) if *db == dirs.db()),
        "{error:?}"
    );
    let failed = failed_start(error, Arc::new(FakePlatform::default()));
    assert!(failed.query_by_label(ALREADY_RUNNING).is_some());
    // Enter or Space quit at once, as a question's cautious answer has the keyboard.
    assert!(failed.get_by_label("Quit").is_focused());
    // Its data folder is fine: nothing to open.
    assert!(failed.query_by_label("Open the folder").is_none());
    drop(failed);
    // The failed start left the first app's lock as it was: a third start is refused too.
    assert!(matches!(
        start_error(&dirs),
        StartError::AlreadyRunning(db) if db == dirs.db()
    ));

    // The first app's database is untouched, and its engine still answers.
    let second = bracket_folder();
    submit(&harness, second.path());
    run_until(&mut harness, "both jobs in review", |app| in_review(app, 2));
    assert!(harness.state().controller().job(first).is_some());
}

#[test]
fn a_database_that_cannot_be_opened_offers_its_folder() {
    let (_data, dirs) = data_dir();
    // A folder where the database should be: the store cannot open it.
    fs::create_dir_all(dirs.db()).unwrap();
    let error = start_error(&dirs);
    let StartError::Engine { error: reason, .. } = &error else {
        panic!("{error:?}");
    };
    // SQLite's words name the path already, so the sentence does not name it again.
    let reason = reason.to_string();
    assert!(reason.contains(&display_path(&dirs.db())), "{reason}");
    let sentence = format!("Photostacker cannot open its database: {reason}.");
    let platform = Arc::new(FakePlatform::default());
    let mut failed = failed_start(error, Arc::clone(&platform));
    assert!(failed.query_by_label(&sentence).is_some(), "{sentence}");
    assert!(failed.get_by_label("Quit").is_focused());

    failed.get_by_label("Open the folder").click();
    failed.step();
    // On a thread of its own, as every reveal.
    let deadline = Instant::now() + TIMEOUT;
    while platform.revealed.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "the folder was never shown");
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(*platform.revealed.lock().unwrap(), vec![dirs.data.clone()]);
}

/// What `path` reads as in a field of the form.
fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The text the form's field labelled `label` holds.
fn field(harness: &Harness<'static, App>, label: &str) -> Option<String> {
    harness.get_by_label(label).value()
}

fn form_shown(app: &App) -> bool {
    app.controller().view() == &View::NewJob
}

#[test]
fn new_job_from_the_form_reaches_review() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let mut harness = harness(&dirs);

    click(&mut harness, "New job");
    assert!(form_shown(harness.state()));
    click(&mut harness, "Photo folder");
    harness
        .get_by_label("Photo folder")
        .type_text(&text(input.path()));
    harness.step();
    assert_eq!(
        field(&harness, "Output folder"),
        Some(text(&input.path().join("stacked")))
    );

    click(&mut harness, "Start");
    run_until(&mut harness, "the job in review", |app| in_review(app, 1));
    assert!(
        harness
            .query_by_label(&row(input.path(), "Review"))
            .is_some()
    );
    // The form closed on the new job, which is selected.
    assert!(harness.query_by_label("Photo folder").is_none());
    assert!(harness.query_by_label("Open output folder").is_some());
}

#[test]
fn ctrl_n_opens_the_form_and_escape_closes_it() {
    let (_data, dirs) = data_dir();
    let mut harness = harness(&dirs);

    harness.key_press_modifiers(Modifiers::COMMAND, Key::N);
    harness.step();
    assert!(form_shown(harness.state()));
    assert!(harness.query_by_label("Photo folder").is_some());

    // In a text field, Esc leaves the field and keeps what was typed.
    click(&mut harness, "Photo folder");
    harness.get_by_label("Photo folder").type_text("/photos");
    harness.step();
    harness.key_press(Key::Escape);
    harness.step();
    assert!(form_shown(harness.state()));
    assert!(!harness.get_by_label("Photo folder").is_focused());
    assert_eq!(field(&harness, "Photo folder"), Some("/photos".into()));

    // Outside one, it closes the form.
    harness.key_press(Key::Escape);
    harness.step();
    assert!(!form_shown(harness.state()));
    assert!(harness.query_by_label("Photo folder").is_none());
    assert!(harness.state_mut().controller_mut().form().is_none());

    // Opened again, the form starts afresh.
    harness.key_press_modifiers(Modifiers::COMMAND, Key::N);
    harness.step();
    assert_eq!(field(&harness, "Photo folder"), Some(String::new()));
}

#[test]
fn a_pasted_quoted_path_is_taken_without_its_quotes() {
    let (_data, dirs) = data_dir();
    let mut harness = harness(&dirs);
    harness.key_press_modifiers(Modifiers::COMMAND, Key::N);
    harness.step();

    // What a paste from another program brings (the real clipboard is checked by hand):
    // a path copied from a file manager, quoted.
    click(&mut harness, "Photo folder");
    harness
        .input_mut()
        .events
        .push(egui::Event::Paste("\"/photos/Lake trip\"".into()));
    harness.step();
    harness.step();

    // The field shows what was pasted; the folder and the output follow it unquoted.
    assert_eq!(
        field(&harness, "Photo folder"),
        Some("\"/photos/Lake trip\"".into())
    );
    let form = harness.state_mut().controller_mut().form().unwrap().clone();
    assert_eq!(
        form.config(None).input_dir,
        PathBuf::from("/photos/Lake trip")
    );
    assert_eq!(
        field(&harness, "Output folder"),
        Some(text(&Path::new("/photos/Lake trip").join("stacked")))
    );
}

#[test]
fn dropping_a_folder_opens_the_form_filled() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let mut harness = harness(&dirs);
    assert!(!painted(&harness, DROP_HINT));

    harness.input_mut().hovered_files.push(egui::HoveredFile {
        path: Some(input.path().to_path_buf()),
        ..egui::HoveredFile::default()
    });
    harness.step();
    assert!(painted(&harness, DROP_HINT));

    harness.input_mut().hovered_files.clear();
    harness
        .input_mut()
        .dropped_files
        .push(Arc::new(DroppedPath(input.path().to_path_buf())));
    harness.step();
    harness.step();
    assert!(!painted(&harness, DROP_HINT));
    assert!(form_shown(harness.state()));
    assert_eq!(field(&harness, "Photo folder"), Some(text(input.path())));
    assert_eq!(
        field(&harness, "Output folder"),
        Some(text(&input.path().join("stacked")))
    );
}

#[test]
fn the_form_renders_at_the_smallest_window() {
    let (_data, dirs) = data_dir();
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("missing");
    let mut harness = harness(&dirs);
    harness.set_size(egui::vec2(900.0, 560.0));
    harness.step();

    // At its longest: Stage 3 on with its slider, and an error under the photo folder.
    harness.state_mut().controller_mut().prefs.ml_enabled = true;
    click(&mut harness, "New job");
    {
        let controller = harness.state_mut().controller_mut();
        let form = controller.form().unwrap();
        form.set_input(&text(&missing));
        form.fields.cosine_threshold = Some(0.8);
        controller.submit_form();
    }
    run_until(&mut harness, "the refusal", |app| {
        app.controller().is_idle()
    });
    harness.step();
    let error = format!("This is not a folder: {}", missing.display());
    assert!(painted(&harness, &error));

    let right = harness.get_by_label("Inspector").rect().min.x;
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 560.0));
    let mut controls = vec![
        (None, "Photo folder"),
        (None, "Browse for the photo folder"),
        (None, "Output folder"),
        (None, "Browse for the output folder"),
        (None, "Scan subfolders"),
        (None, "PSD"),
        (None, "Layered TIFF"),
        (None, "Write without review"),
        (None, "sRGB"),
        (None, "Linear (for HDR merging)"),
        (None, "Stack the RAW"),
        (None, "Stack the JPEG"),
        (Some(Role::Slider), "Hash distance"),
        (Some(Role::Slider), "Time window (s)"),
        (Some(Role::Slider), "Visual check floor"),
    ];
    // A build without the visual check names no model, so it has no similarity.
    if cfg!(feature = "ml") {
        controls.extend([
            (None, "Custom similarity"),
            (Some(Role::Slider), "Similarity"),
        ]);
    }
    controls.extend([(None, "Start"), (None, "Cancel new job")]);
    for (role, label) in controls {
        let find = |harness: &Harness<'static, App>| match role {
            Some(role) => harness.get_by_role_and_label(role, label).rect(),
            None => harness.get_by_label(label).rect(),
        };
        // Reachable: scrolled into view, it is on screen, in the centre.
        match role {
            Some(role) => harness.get_by_role_and_label(role, label).scroll_to_me(),
            None => harness.get_by_label(label).scroll_to_me(),
        }
        harness.step();
        harness.step();
        let rect = find(&harness);
        assert!(
            screen.contains_rect(rect),
            "{label} is off screen: {rect:?}"
        );
        assert!(
            rect.min.x >= JOBS_WIDTH && rect.max.x <= right,
            "{label} leaves the centre: {rect:?}"
        );
    }
}

#[test]
fn a_form_on_its_way_cannot_be_cancelled() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let mut harness = harness(&dirs);
    let gate = Arc::new(Gate::default());
    let _open = OpenOnDrop(Arc::clone(&gate));
    // The backend waits at the gate, so the submission stays on its way.
    let held = Arc::clone(&gate);
    harness.state().controller().send(Request::Run {
        job: None,
        command: Box::new(move |_engine| {
            held.pass();
            Ok(())
        }),
    });
    click(&mut harness, "New job");
    harness
        .state_mut()
        .controller_mut()
        .form()
        .unwrap()
        .set_input(&text(input.path()));
    click(&mut harness, "Start");
    assert!(
        harness
            .state_mut()
            .controller_mut()
            .form()
            .unwrap()
            .submitting
    );

    assert!(harness.get_by_label("Start").accesskit_node().is_disabled());
    assert!(
        harness
            .get_by_label("Cancel new job")
            .accesskit_node()
            .is_disabled()
    );
    harness.key_press(Key::Escape);
    harness.step();
    assert!(form_shown(harness.state()));

    gate.open();
    run_until(&mut harness, "the job in review", |app| in_review(app, 1));
    assert!(!form_shown(harness.state()));
}

/// Whether the first job of `app` is scanning and examining its photos.
fn hashing(app: &App) -> bool {
    let controller = app.controller();
    controller.jobs().first().is_some_and(|job| {
        job.status == JobStatus::Scanning
            && controller
                .progress(job.id)
                .is_some_and(|progress| progress.stage == Stage::Hash)
    })
}

/// The job on `input`.
fn job_on(app: &App, input: &Path) -> Option<JobId> {
    app.controller()
        .jobs()
        .iter()
        .find(|job| job.config.input_dir == input)
        .map(|job| job.id)
}

#[test]
fn a_scan_shows_its_stage_its_count_and_a_cancel() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    for n in 0..8 {
        write_bracket(
            input.path(),
            &format!("bracket{n}"),
            n + 1,
            30 * n as u32,
            3,
        );
    }
    let gate = Arc::new(Gate::default());
    let mut harness = harness_with(&dirs, gate.holding_the_scan());
    let _open = OpenOnDrop(Arc::clone(&gate));
    submit(&harness, input.path());
    run_until(&mut harness, "the scan's hashing", hashing);
    let job = harness.state().controller().jobs()[0].id;

    click(&mut harness, &row(input.path(), "Scanning"));
    assert!(harness.query_by_label(SCAN_PROGRESS).is_some());
    assert!(harness.query_by_label("Examining photos").is_some());
    // The controller is polled only at the start of a frame: this is what was drawn.
    let done = harness.state().controller().progress(job).unwrap().done;
    assert!(harness.query_by_label(&format!("{done} of 24")).is_some());

    click(&mut harness, CANCEL_JOB);
    // The cancel must reach the engine before the scan is let go, or the scan finishes.
    run_until(&mut harness, "the cancel to run", |app| {
        app.controller().is_idle()
    });
    gate.open();
    run_until(&mut harness, "the cancelled job", |app| {
        status(app, job) == Some(JobStatus::Cancelled) && app.controller().is_idle()
    });
    assert!(harness.query_by_label(CANCELLED).is_some());
    assert!(harness.query_by_label(SCAN_PROGRESS).is_none());
    assert!(harness.query_by_label(RETRY_JOB).is_some());
}

#[test]
fn files_that_fail_are_listed_while_the_scan_runs() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    // Named to sort first: the gate holds each of the scan's workers at its first event,
    // and the first two workers take these two (on a machine of two cores or more).
    for n in 1..=2 {
        fs::write(input.path().join(format!("broken_{n}.jpg")), b"not a jpeg").unwrap();
    }
    let gate = Arc::new(Gate::default());
    let mut harness = harness_with(&dirs, gate.holding_the_scan());
    let _open = OpenOnDrop(Arc::clone(&gate));
    submit(&harness, input.path());
    run_until(&mut harness, "both broken files reported", |app| {
        let controller = app.controller();
        controller
            .jobs()
            .first()
            .is_some_and(|job| controller.scan_errors(job.id).len() == 2)
    });
    let job = harness.state().controller().jobs()[0].id;
    // The scan reports its files by events, ahead of the list's read that says it is
    // scanning: its row reads Scanning once that read is in.
    run_until(&mut harness, "the job's row to say Scanning", |app| {
        status(app, job) == Some(JobStatus::Scanning)
    });

    click(&mut harness, &row(input.path(), "Scanning"));
    assert_eq!(status(harness.state(), job), Some(JobStatus::Scanning));
    assert!(
        harness
            .query_by_label("Files that could not be read (2)")
            .is_some()
    );
    let errors = harness.state().controller().scan_errors(job).to_vec();
    for (path, reason) in &errors {
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("broken_"), "{}", path.display());
        assert!(painted(&harness, &name), "{name} is not on screen");
        let reason = sentence(reason);
        assert!(painted(&harness, &reason), "{reason} is not on screen");
    }

    gate.open();
    run_until(&mut harness, "the job in review", |app| in_review(app, 1));
    assert!(
        harness
            .query_by_label("Files that could not be read (2)")
            .is_none()
    );
}

#[test]
fn a_failed_job_shows_its_error_and_retries() {
    let (_data, dirs) = data_dir();
    let held = bracket_folder();
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("input");
    fs::create_dir(&input).unwrap();
    let gate = Arc::new(Gate::default());
    let mut harness = harness_with(&dirs, gate.holding_the_scan());
    let _open = OpenOnDrop(Arc::clone(&gate));

    // As `scan_failure_sets_failed_with_the_message` in the core's engine tests: the
    // folder goes away while its job waits behind a held scan.
    submit(&harness, held.path());
    run_until(&mut harness, "the first scan's hashing", hashing);
    submit(&harness, &input);
    run_until(&mut harness, "the second job queued", |app| {
        job_on(app, &input).is_some()
    });
    let job = job_on(harness.state(), &input).unwrap();
    fs::remove_dir(&input).unwrap();
    gate.open();
    run_until(&mut harness, "the job to fail", |app| {
        status(app, job) == Some(JobStatus::Failed) && app.controller().is_idle()
    });

    click(&mut harness, &row(&input, "Failed"));
    // The engine's words, as a sentence.
    let error = format!("Input is not a directory: {}.", input.display());
    assert!(harness.query_by_label(&error).is_some());

    fs::create_dir(&input).unwrap();
    write_bracket_folder(&input);
    click(&mut harness, RETRY_JOB);
    run_until(&mut harness, "both jobs in review", |app| in_review(app, 2));
    assert!(harness.query_by_label(&row(&input, "Review")).is_some());
    assert!(harness.query_by_label(&error).is_none());
}

#[test]
fn a_queued_job_names_what_it_waits_for() {
    let (_data, dirs) = data_dir();
    let running = bracket_folder();
    let waiting = bracket_folder();
    let gate = Arc::new(Gate::default());
    let mut harness = harness_with(&dirs, gate.holding_the_scan());
    let _open = OpenOnDrop(Arc::clone(&gate));
    submit(&harness, running.path());
    run_until(&mut harness, "the first scan's hashing", hashing);
    submit(&harness, waiting.path());
    run_until(&mut harness, "the second job queued", |app| {
        job_on(app, waiting.path()).is_some_and(|job| status(app, job) == Some(JobStatus::Queued))
    });
    let job = job_on(harness.state(), waiting.path()).unwrap();

    click(&mut harness, &row(waiting.path(), "Queued"));
    let waiting_for = format!("Waiting for {}.", folder_name(running.path()));
    assert!(harness.query_by_label(&waiting_for).is_some());

    // A queued job is cancelled at once, while the other still scans.
    click(&mut harness, CANCEL_JOB);
    run_until(&mut harness, "the queued job cancelled", |app| {
        status(app, job) == Some(JobStatus::Cancelled)
    });
    assert!(harness.query_by_label(CANCELLED).is_some());
    assert!(harness.query_by_label(&waiting_for).is_none());
}

#[test]
fn a_long_path_is_truncated_and_never_widens_the_window() {
    let (_data, dirs) = data_dir();
    let root = tempfile::tempdir().unwrap();
    let mut input = root.path().to_path_buf();
    let mut depth = 0;
    while display_path(&input).chars().count() < 240 {
        input.push(format!("photos-of-trip-{depth:02}"));
        depth += 1;
    }
    fs::create_dir_all(&input).unwrap();
    write_bracket_folder(&input);
    let mut harness = harness(&dirs);
    submit(&harness, &input);
    run_until(&mut harness, "the job in review", |app| in_review(app, 1));
    harness.set_size(egui::vec2(900.0, 560.0));
    harness.step();
    click(&mut harness, &row(&input, "Review"));

    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 560.0));
    let inspector = harness.get_by_label("Inspector").rect();
    assert!(screen.contains_rect(inspector), "{inspector:?}");
    // The header names the folder by its whole path, for screen readers and on hover.
    let folder = harness.get_by_label(&display_path(&input)).rect();
    assert!(
        folder.min.x >= JOBS_WIDTH && folder.max.x <= inspector.min.x,
        "the folder leaves the centre: {folder:?}"
    );
    for chip in harness.query_all_by_label("Review") {
        assert!(screen.contains_rect(chip.rect()), "{:?}", chip.rect());
        assert!(chip.rect().max.x <= inspector.min.x, "{:?}", chip.rect());
    }
    // Cut in the middle: the path's start and its end stay in sight.
    let path = display_path(&input);
    let cut_in_the_middle = |text: &String| {
        text.split_once('…').is_some_and(|(start, end)| {
            !start.is_empty() && !end.is_empty() && path.starts_with(start) && path.ends_with(end)
        })
    };
    assert!(
        painted_texts(&harness).iter().any(cut_in_the_middle),
        "{:?}",
        painted_texts(&harness)
    );
}

#[test]
fn no_drop_is_invited_while_a_question_is_open() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let mut harness = harness(&dirs);
    let job = reviewing(&mut harness, input.path());
    harness
        .state_mut()
        .controller_mut()
        .ask(Question::DeleteJob(job));
    harness.input_mut().hovered_files.push(egui::HoveredFile {
        path: Some(input.path().to_path_buf()),
        ..egui::HoveredFile::default()
    });
    // The dialog shows from the second frame.
    harness.step();
    harness.step();
    assert!(
        harness
            .query_by_label(&delete_question(&harness, job))
            .is_some()
    );
    assert!(!painted(&harness, DROP_HINT));

    harness.state_mut().controller_mut().answer(false);
    harness.step();
    assert!(painted(&harness, DROP_HINT));
}

// --- The review ---

/// A job on `input` in review, selected, with its review loaded and drawn.
fn reviewing_selected(harness: &mut Harness<'static, App>, input: &Path) -> JobId {
    let job = reviewing(harness, input);
    click(harness, &row(input, "Review"));
    run_until_the_review_is_loaded(harness);
    harness.step();
    job
}

fn review(app: &App) -> &Review {
    app.controller().review().expect("a review is open")
}

/// The id of the photo named `name`.
fn photo(app: &App, name: &str) -> usize {
    review(app)
        .files
        .iter()
        .position(|file| file.path.file_name().is_some_and(|n| n == name))
        .unwrap_or_else(|| panic!("no photo {name}"))
}

/// The stack that holds the photo named `name`.
fn stack<'a>(app: &'a App, name: &str) -> &'a GroupView {
    let id = photo(app, name);
    review(app)
        .groups
        .iter()
        .find(|group| group.members.contains(&id))
        .unwrap_or_else(|| panic!("{name} is in no stack"))
}

/// The decision on the stack that holds the photo named `name`.
fn decision(app: &App, name: &str) -> GroupStatus {
    stack(app, name).status
}

fn selection(app: &App) -> Selection {
    review(app).selection.clone()
}

/// The stacks `ids`, selected in that order, and no photo.
fn stacks(ids: &[GroupId]) -> Selection {
    Selection {
        groups: ids.to_vec(),
        photo: None,
    }
}

/// The label of stack `n`'s row, up to its first photo's name.
fn stack_row(n: usize) -> String {
    format!("Stack {n}: ")
}

/// Clicks stack `n`'s row on its title, holding `modifiers`.
fn click_stack(harness: &mut Harness<'static, App>, n: usize, modifiers: Modifiers) {
    let rect = harness.get_by_label_contains(&stack_row(n)).rect();
    click_at_with(harness, rect.left_top() + egui::vec2(24.0, 16.0), modifiers);
}

/// Presses `key` and draws the frames that take it in.
fn press(harness: &mut Harness<'static, App>, key: Key) {
    harness.key_press(key);
    harness.step();
    harness.step();
}

#[test]
fn confirm_and_reject_buttons_decide_their_stack() {
    use GroupStatus::{Confirmed, Proposed, Rejected};
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut harness = harness(&dirs);
    reviewing_selected(&mut harness, input.path());
    assert!(harness.query_by_label_contains(&stack_row(1)).is_some());
    assert!(harness.query_by_label_contains(&stack_row(2)).is_some());

    click(&mut harness, "Confirm stack 1");
    run_until(&mut harness, "stack 1 confirmed", |app| {
        decision(app, "a_1.jpg") == Confirmed
    });
    assert_eq!(decision(harness.state(), "b_1.jpg"), Proposed);
    // The button is the stack's, not its row's: nothing is selected.
    assert_eq!(selection(harness.state()), Selection::default());
    harness.step();
    let label = "Stack 1: a_1.jpg, 2 photos, Confirmed";
    assert!(harness.query_by_label(label).is_some());
    // Each photo has no capture time: its warning, named on hover.
    let warnings = harness.get_by_label("Warnings of stack 1: 2");
    warnings.hover();
    run_until_painted(
        &mut harness,
        "a_2.jpg: no EXIF capture time; the file modification time was used",
    );

    click(&mut harness, "Reject stack 2");
    run_until(&mut harness, "stack 2 rejected", |app| {
        decision(app, "b_1.jpg") == Rejected
    });
    assert_eq!(decision(harness.state(), "a_1.jpg"), Confirmed);

    // A decided stack can be decided again.
    click(&mut harness, "Reject stack 1");
    run_until(&mut harness, "stack 1 rejected", |app| {
        decision(app, "a_1.jpg") == Rejected
    });
}

#[test]
fn confirm_all_then_write_writes_every_stack() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut harness = harness(&dirs);
    let job = reviewing_selected(&mut harness, input.path());
    // Nothing is confirmed yet: nothing to write, and both stacks wait.
    let nothing = harness.get_by_label("Write 0 stacks");
    assert!(nothing.accesskit_node().is_disabled());
    assert!(painted(&harness, "2 still proposed"));

    click(&mut harness, "Confirm all");
    run_until(&mut harness, "both stacks confirmed", |app| {
        app.controller().write_counts().to_write == 2
    });
    harness.step();
    assert!(!painted(&harness, "still proposed"));
    let write = harness.get_by_label("Write 2 stacks");
    assert!(!write.accesskit_node().is_disabled());

    // Nothing is left proposed: it writes at once, without a question.
    click(&mut harness, "Write 2 stacks");
    assert_eq!(harness.state().controller().question(), None);
    run_until(&mut harness, "the job written", |app| {
        status(app, job) == Some(JobStatus::Completed)
            && app.controller().is_idle()
            && review(app)
                .groups
                .iter()
                .all(|group| group.status == GroupStatus::Written)
    });
    for group in &review(harness.state()).groups {
        let written = group
            .output_path
            .as_ref()
            .expect("a written stack has a file");
        assert!(written.is_file(), "{}", written.display());
    }
    assert_eq!(files_in(&input.path().join("stacked")).len(), 2);
}

#[test]
fn enter_confirms_and_delete_rejects_the_selected_stack() {
    use GroupStatus::{Confirmed, Proposed, Rejected};
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut harness = harness(&dirs);
    reviewing_selected(&mut harness, input.path());
    let a = stack(harness.state(), "a_1.jpg").id;

    // With nothing selected, Enter decides nothing.
    press(&mut harness, Key::Enter);
    run_until(&mut harness, "nothing to do", |app| {
        app.controller().is_idle()
    });
    assert_eq!(decision(harness.state(), "a_1.jpg"), Proposed);

    click_stack(&mut harness, 1, Modifiers::NONE);
    assert_eq!(selection(harness.state()), stacks(&[a]));
    press(&mut harness, Key::Enter);
    run_until(&mut harness, "stack 1 confirmed", |app| {
        decision(app, "a_1.jpg") == Confirmed
    });
    press(&mut harness, Key::Delete);
    run_until(&mut harness, "stack 1 rejected", |app| {
        decision(app, "a_1.jpg") == Rejected
    });
    assert_eq!(decision(harness.state(), "b_1.jpg"), Proposed);
    // The decided stack stays selected.
    assert_eq!(selection(harness.state()), stacks(&[a]));
}

#[test]
fn arrows_move_the_selection() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut harness = harness(&dirs);
    reviewing_selected(&mut harness, input.path());
    let (a, b) = {
        let app = harness.state();
        (stack(app, "a_1.jpg").clone(), stack(app, "b_1.jpg").clone())
    };

    press(&mut harness, Key::ArrowDown);
    assert_eq!(selection(harness.state()), stacks(&[a.id]));
    press(&mut harness, Key::ArrowDown);
    assert_eq!(selection(harness.state()), stacks(&[b.id]));
    press(&mut harness, Key::ArrowUp);
    assert_eq!(selection(harness.state()), stacks(&[a.id]));

    // Right and Left step through the primary stack's photos, top layer first.
    let with_photo = |photo: usize| Selection {
        groups: vec![a.id],
        photo: Some(photo),
    };
    press(&mut harness, Key::ArrowRight);
    assert_eq!(selection(harness.state()), with_photo(a.members[0]));
    press(&mut harness, Key::ArrowRight);
    assert_eq!(selection(harness.state()), with_photo(a.members[1]));
    press(&mut harness, Key::ArrowLeft);
    assert_eq!(selection(harness.state()), with_photo(a.members[0]));

    press(&mut harness, Key::Escape);
    assert_eq!(selection(harness.state()), Selection::default());
}

#[test]
fn ctrl_click_selects_several_stacks() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut harness = harness(&dirs);
    reviewing_selected(&mut harness, input.path());
    let (a, b) = {
        let app = harness.state();
        (stack(app, "a_1.jpg").id, stack(app, "b_1.jpg").id)
    };

    click_stack(&mut harness, 1, Modifiers::NONE);
    assert_eq!(selection(harness.state()), stacks(&[a]));
    click_stack(&mut harness, 2, Modifiers::COMMAND);
    assert_eq!(selection(harness.state()), stacks(&[a, b]));
    // Ctrl+click on a selected stack takes it out.
    click_stack(&mut harness, 1, Modifiers::COMMAND);
    assert_eq!(selection(harness.state()), stacks(&[b]));
    // A plain click selects one stack alone.
    click_stack(&mut harness, 1, Modifiers::COMMAND);
    click_stack(&mut harness, 2, Modifiers::NONE);
    assert_eq!(selection(harness.state()), stacks(&[b]));

    // A click on a thumbnail selects its photo, and its stack alone.
    let b_2 = photo(harness.state(), "b_2.jpg");
    click(&mut harness, "b_2.jpg");
    assert_eq!(
        selection(harness.state()),
        Selection {
            groups: vec![b],
            photo: Some(b_2),
        }
    );
}

#[test]
fn the_reviews_keys_wait_while_a_menu_is_open() {
    use GroupStatus::Proposed;
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut harness = harness(&dirs);
    reviewing_selected(&mut harness, input.path());
    let a_1 = photo(harness.state(), "a_1.jpg");
    click(&mut harness, "a_1.jpg");
    let selected = selection(harness.state());
    assert_eq!(selected.photo, Some(a_1));

    // `Move to stack…` opened with the mouse: the keys are the menu's, never the review's
    // behind it.
    click(&mut harness, "Move to stack…");
    assert!(harness.query_by_label_contains("Stack 2 (").is_some());
    for key in [
        Key::Enter,
        Key::Delete,
        Key::ArrowDown,
        Key::ArrowUp,
        Key::ArrowRight,
    ] {
        press(&mut harness, key);
    }
    run_until_idle(&mut harness);
    assert_eq!(selection(harness.state()), selected);
    assert_eq!(decision(harness.state(), "a_1.jpg"), Proposed);
    assert_eq!(decision(harness.state(), "b_1.jpg"), Proposed);
}

#[test]
fn ctrl_click_on_a_thumbnail_adds_its_stack() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut harness = harness(&dirs);
    reviewing_selected(&mut harness, input.path());
    let (a, b) = {
        let app = harness.state();
        (stack(app, "a_1.jpg").id, stack(app, "b_1.jpg").id)
    };

    click_stack(&mut harness, 1, Modifiers::NONE);
    // As a Ctrl+click on its row's title would: the stack joins the selection.
    let thumbnail = harness.get_by_label("b_2.jpg").rect();
    click_at_with(&mut harness, thumbnail.center(), Modifiers::COMMAND);
    assert_eq!(selection(harness.state()), stacks(&[a, b]));
    let merge = harness.get_by_label("Merge 2 stacks");
    assert!(!merge.accesskit_node().is_disabled());
    // And leaves it again.
    let thumbnail = harness.get_by_label("b_2.jpg").rect();
    click_at_with(&mut harness, thumbnail.center(), Modifiers::COMMAND);
    assert_eq!(selection(harness.state()), stacks(&[a]));
}

#[test]
fn a_photo_behind_more_is_reached_from_the_inspector() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    write_bracket(input.path(), "long", 1, 0, 7);
    let mut harness = harness(&dirs);
    reviewing_selected(&mut harness, input.path());
    let long = stack(harness.state(), "long_1.jpg").clone();
    assert_eq!(long.members.len(), 7);
    click_stack(&mut harness, 1, Modifiers::NONE);
    // The row shows what fits; the last photo is behind `+<k>`.
    let more = |harness: &Harness<'static, App>| {
        harness
            .query_all_by(|node| {
                node.role() == Role::Label
                    && node.value().is_some_and(|name| {
                        name.ends_with(" more photos") || name.ends_with(" more photo")
                    })
            })
            .map(|node| node.accesskit_node().toggled())
            .collect::<Vec<_>>()
    };
    assert_eq!(more(&harness), [Some(Toggled::False)]);
    assert!(harness.query_by_label("long_7.jpg").is_none());

    // Its name in the inspector selects it, as its thumbnail would.
    click(&mut harness, "Select long_7.jpg");
    let last = photo(harness.state(), "long_7.jpg");
    assert_eq!(
        selection(harness.state()),
        Selection {
            groups: vec![long.id],
            photo: Some(last),
        }
    );
    assert!(harness.query_by_label("Photo long_7.jpg").is_some());
    assert!(harness.query_by_label("Move to stack…").is_some());
    // The row's `+<k>` shows that the selected photo is behind it.
    assert_eq!(more(&harness), [Some(Toggled::True)]);
}

#[test]
fn the_hash_slider_regroups_live() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut harness = harness(&dirs);
    reviewing_selected(&mut harness, input.path());
    assert_eq!(stack(harness.state(), "b_1.jpg").members.len(), 2);

    // A slider keeps the arrows from moving the focus from the frame after it gains it.
    harness
        .get_by_role_and_label(Role::Slider, "Hash distance")
        .focus();
    harness.step();
    harness.step();
    for _ in 0..40 {
        if review(harness.state()).hamming == NEAR {
            break;
        }
        press(&mut harness, Key::ArrowRight);
    }
    assert_eq!(review(harness.state()).hamming, NEAR);
    run_until(&mut harness, "the near photo to join its stack", |app| {
        app.controller().is_idle() && stack(app, "b_3.jpg").members.len() == 3
    });
    let review = review(harness.state());
    assert_eq!(review.groups.len(), 2);
    assert!(review.unstacked.is_empty());
}

/// What a review with no stack says of a time window of `seconds`, below the top of its
/// range.
fn window_hint(seconds: u32) -> String {
    format!(
        "Only photos shot within {seconds} s of each other are compared: try a longer time \
         window."
    )
}

/// A job on `config` in review, selected, with its review loaded and drawn; the app has
/// `jobs` jobs with it.
fn reviewing_config(harness: &mut Harness<'static, App>, config: JobConfig, jobs: usize) {
    let input = config.input_dir.clone();
    harness.state().controller().submit(config);
    run_until(harness, "the job in review", |app| in_review(app, jobs));
    click(harness, &row(&input, "Review"));
    run_until_the_review_is_loaded(harness);
    harness.step();
}

#[test]
fn a_longer_time_window_finds_the_stack_the_hint_asked_for() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    write_spaced_shots(input.path(), 10);
    let mut harness = harness(&dirs);
    let mut config = JobConfig::new(input.path().to_path_buf());
    config.time_window_seconds = 4;
    reviewing_config(&mut harness, config, 1);
    assert!(painted(&harness, "No stack was found in 2 photos."));
    assert!(painted(&harness, &window_hint(4)));

    // A step up shows its value in the hint at once; still no stack at 5 s.
    let slider = harness.get_by_role_and_label(Role::Slider, "Time window (s)");
    assert_eq!(slider.accesskit_node().numeric_value(), Some(4.0));
    slider.set_value_accesskit(5.0);
    harness.step();
    harness.step();
    assert_eq!(review(harness.state()).time_window_seconds, 5);
    assert!(painted(&harness, &window_hint(5)));
    run_until(&mut harness, "the regrouping at 5 s", |app| {
        app.controller().is_idle()
    });
    harness.step();
    assert!(review(harness.state()).groups.is_empty());

    // Moved past the 10 s between the shots, they are one stack, and the hint goes.
    harness
        .get_by_role_and_label(Role::Slider, "Time window (s)")
        .set_value_accesskit(12.0);
    run_until(&mut harness, "the stack", |app| {
        app.controller().is_idle() && review(app).groups.len() == 1
    });
    harness.step();
    let members = &stack(harness.state(), "a.jpg").members;
    assert_eq!(names(harness.state(), members), ["a.jpg", "b.jpg"]);
    assert!(!painted(&harness, "No stack was found"));
    assert!(!painted(&harness, "Only photos shot within"));
    let job = review(harness.state()).job;
    let config = &harness.state().controller().job(job).unwrap().config;
    assert_eq!(config.time_window_seconds, 12);
}

#[test]
fn at_the_longest_time_window_the_hint_suggests_nothing_more() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    write_lone_shots(input.path(), 2);
    let mut harness = harness(&dirs);
    let mut config = JobConfig::new(input.path().to_path_buf());
    config.time_window_seconds = 60;
    reviewing_config(&mut harness, config, 1);
    assert!(painted(&harness, "No stack was found in 2 photos."));
    assert!(painted(
        &harness,
        "Only photos shot within 60 s of each other are compared."
    ));
    assert!(!painted(&harness, "try a longer time window"));
}

#[test]
fn the_similarity_slider_is_off_without_a_model() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut harness = harness(&dirs);
    reviewing_selected(&mut harness, input.path());
    let similarity = harness.get_by_role_and_label(Role::Slider, "Similarity");
    assert!(similarity.accesskit_node().is_disabled());
    assert!(harness.query_by_label("no model").is_some());
    assert!(harness.query_by_label("Model default").is_none());

    // With a model, it shows the model's own similarity until it is moved.
    let other = review_folder();
    let mut config = JobConfig::new(other.path().to_path_buf());
    config.ml_model = Some("dinov2-small".into());
    harness.state().controller().submit(config);
    run_until(&mut harness, "the second job in review", |app| {
        in_review(app, 2)
    });
    click(&mut harness, &row(other.path(), "Review"));
    run_until_the_review_is_loaded(&mut harness);
    harness.step();
    let similarity = harness.get_by_role_and_label(Role::Slider, "Similarity");
    assert!(!similarity.accesskit_node().is_disabled());
    assert_eq!(similarity.accesskit_node().numeric_value(), Some(0.5));
    assert!(harness.query_by_label("no model").is_none());
    let default = harness.get_by_label("Model default");
    assert!(default.accesskit_node().is_disabled());

    similarity.set_value_accesskit(0.7);
    harness.step();
    harness.step();
    let cosine = review(harness.state()).cosine;
    assert!(
        cosine.is_some_and(|cosine| (cosine - 0.7).abs() < 1e-6),
        "{cosine:?}"
    );
    assert!(
        !harness
            .get_by_label("Model default")
            .accesskit_node()
            .is_disabled()
    );
    click(&mut harness, "Model default");
    assert_eq!(review(harness.state()).cosine, None);
    run_until(&mut harness, "the regroupings", |app| {
        app.controller().is_idle()
    });
    let similarity = harness.get_by_role_and_label(Role::Slider, "Similarity");
    assert_eq!(similarity.accesskit_node().numeric_value(), Some(0.5));
}

#[test]
fn a_folder_with_no_photo_says_so() {
    let (_data, dirs) = data_dir();
    let empty = tempfile::tempdir().unwrap();
    let mut harness = harness(&dirs);
    reviewing_selected(&mut harness, empty.path());
    assert!(painted(&harness, "No photo was found in this folder."));
    assert!(!painted(&harness, "No stack was found"));
    // Its subfolders were not looked at: how to have them.
    let subfolders = "Its subfolders were not looked at: turn on Scan subfolders in a new job.";
    assert!(painted(&harness, subfolders));

    // A job that looked at them does not say so.
    let other = tempfile::tempdir().unwrap();
    let mut config = JobConfig::new(other.path().to_path_buf());
    config.recursive = true;
    harness.state().controller().submit(config);
    run_until(&mut harness, "the job in review", |app| in_review(app, 2));
    click(&mut harness, &row(other.path(), "Review"));
    run_until_the_review_is_loaded(&mut harness);
    harness.step();
    assert!(painted(&harness, "No photo was found in this folder."));
    assert!(!painted(&harness, subfolders));
}

#[test]
fn a_job_of_two_thousand_stacks_lays_out_only_the_visible_rows() {
    const STACKS: usize = 2_000;
    let (_data, dirs) = data_dir();
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("large");
    store_brackets(&dirs, &input, STACKS);
    let mut harness = harness(&dirs);
    run_until(&mut harness, "the stored job", |app| in_review(app, 1));
    click(&mut harness, &row(&input, "Review"));
    run_until_the_review_is_loaded(&mut harness);
    harness.step();
    assert!(painted(&harness, "No stack was found in 6000 photos."));
    assert!(painted(&harness, "6000 photos are unstacked."));
    // Stored without a grouping: one setting of the slider groups it.
    harness
        .get_by_role_and_label(Role::Slider, "Hash distance")
        .set_value_accesskit(9.0);
    run_until(&mut harness, "the stacks", |app| {
        review(app).groups.len() == STACKS && app.controller().is_idle()
    });
    harness.step();
    let rows = |harness: &Harness<'static, App>| harness.state().stats().stack_rows;
    assert!((1..=12).contains(&rows(&harness)), "{}", rows(&harness));
    assert!(harness.query_by_label_contains(&stack_row(1)).is_some());

    // Scrolled to the end, the last stack is laid out, and still only the visible rows.
    let list = harness.get_by_label_contains(&stack_row(1)).rect().center();
    harness.hover_at(list);
    let last = stack_row(STACKS);
    let deadline = Instant::now() + TIMEOUT;
    while harness.query_by_label_contains(&last).is_none() {
        assert!(Instant::now() < deadline, "the list never reached its end");
        harness.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Page,
            delta: egui::vec2(0.0, -100.0),
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Unknown,
            modifiers: Modifiers::NONE,
        });
        harness.step();
    }
    // egui spreads a wheel's scroll over the next frames: let it end.
    while repaint_delay(&harness) <= Duration::from_secs(1) {
        assert!(
            Instant::now() < deadline,
            "the list never stopped scrolling"
        );
        harness.step();
    }
    assert!(harness.query_by_label_contains(&last).is_some());
    assert!(harness.query_by_label_contains(&stack_row(1)).is_none());
    assert!((1..=12).contains(&rows(&harness)), "{}", rows(&harness));

    // A step brings the stack it selects into view: with nothing selected, the first.
    press(&mut harness, Key::ArrowDown);
    harness.step();
    let first = review(harness.state()).groups[0].id;
    assert_eq!(selection(harness.state()), stacks(&[first]));
    assert!(harness.query_by_label_contains(&stack_row(1)).is_some());
    assert!((1..=12).contains(&rows(&harness)), "{}", rows(&harness));
}

#[test]
fn review_renders_at_the_smallest_window_in_both_themes() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut harness = harness(&dirs);
    reviewing_selected(&mut harness, input.path());
    harness.set_size(egui::vec2(900.0, 560.0));
    harness.step();
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 560.0));

    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        harness.ctx.set_theme(theme);
        harness.step();
        harness.step();
        let rect = |label: &str| harness.get_by_label(label).rect();
        let inspector = rect("Inspector");
        let stack_1 = harness.get_by_label_contains(&stack_row(1)).rect();
        // The side panels give way, so the centre keeps its width.
        assert!(
            stack_1.width() >= CENTRE_MIN_WIDTH - 2.0 * CENTRE_MARGIN,
            "{theme:?}: {stack_1:?}"
        );
        assert_eq!(stack_1.height(), ROW_HEIGHT, "{theme:?}");
        for label in [
            "Confirm stack 1",
            "Reject stack 1",
            "a_1.jpg",
            "Confirm all",
            "Reject all",
            "Write 0 stacks",
            // The tray, between the stacks and the bar, and its photo.
            "Unstacked (1)",
            "b_3.jpg",
        ] {
            let control = rect(label);
            assert!(
                screen.contains_rect(control),
                "{theme:?}: {label} is off screen: {control:?}"
            );
            assert!(
                control.min.x >= JOBS_WIDTH && control.max.x <= inspector.min.x,
                "{theme:?}: {label} leaves the centre: {control:?}"
            );
        }
        for label in ["Hash distance", "Time window (s)", "Similarity"] {
            let slider = harness.get_by_role_and_label(Role::Slider, label).rect();
            assert!(
                screen.contains_rect(slider) && slider.max.x <= inspector.min.x,
                "{theme:?}: {label} leaves the centre: {slider:?}"
            );
        }
    }
}

// --- The tray and drag and drop ---

/// What the row at the end of the stacks says: a photo dropped on it starts a stack.
const NEW_STACK: &str = "Drop a photo here to start a new stack";

/// The label of the tray's title while it holds `count` photos.
fn tray(count: usize) -> String {
    format!("Unstacked ({count})")
}

/// The number on screen of the stack that holds the photo named `name`.
fn number_of(app: &App, name: &str) -> usize {
    stack(app, name).position + 1
}

/// A point on the title of stack `n`'s row, away from its thumbnails and its buttons.
fn on_stack(harness: &Harness<'static, App>, n: usize) -> egui::Pos2 {
    harness
        .get_by_label_contains(&stack_row(n))
        .rect()
        .left_top()
        + egui::vec2(24.0, 16.0)
}

/// The middle of the widget labelled `label`.
fn centre_of(harness: &Harness<'static, App>, label: &str) -> egui::Pos2 {
    harness.get_by_label(label).rect().center()
}

/// Drags with the primary button from `from` to `to`, as a hand does, and releases it
/// there, in the frame drawn last.
fn drag(harness: &mut Harness<'static, App>, from: egui::Pos2, to: egui::Pos2) {
    hold(harness, from, to);
    harness.drop_at(to);
    harness.step();
}

/// Takes what is at `from` with the primary button and holds it at `to`: a press, a move
/// past egui's click distance, which makes it a drag, and the way to `to`.
fn hold(harness: &mut Harness<'static, App>, from: egui::Pos2, to: egui::Pos2) {
    harness.hover_at(from);
    harness.step();
    harness.drag_at(from);
    harness.step();
    harness.hover_at(from + egui::vec2(0.0, 12.0));
    harness.step();
    harness.hover_at(to);
    harness.step();
}

/// The photo being dragged, as egui carries it.
fn carried(harness: &Harness<'static, App>) -> Option<DraggedPhoto> {
    egui::DragAndDrop::payload::<DraggedPhoto>(&harness.ctx).map(|photo| *photo)
}

/// Draws frames until the backend is idle, then one more, so the screen shows what the
/// last read brought.
fn run_until_idle(harness: &mut Harness<'static, App>) {
    run_until(harness, "the controller to be idle", |app| {
        app.controller().is_idle()
    });
    harness.step();
}

/// [`reviewing_selected`] at a hash distance of [`NEAR`], at which `b_3` joins `b_1` and
/// `b_2`: a stack of three, which keeps two when a photo leaves it.
fn reviewing_near(harness: &mut Harness<'static, App>, input: &Path) {
    reviewing_selected(harness, input);
    harness
        .state_mut()
        .controller_mut()
        .set_thresholds(NEAR, TIME_WINDOW, None);
    run_until(harness, "b_3 to join its stack", |app| {
        app.controller().is_idle() && review(app).unstacked.is_empty()
    });
    harness.step();
}

#[test]
fn dragging_a_photo_onto_another_stack_moves_it() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut harness = harness(&dirs);
    reviewing_near(&mut harness, input.path());
    let a = stack(harness.state(), "a_1.jpg").id;
    let n = number_of(harness.state(), "a_1.jpg");

    let from = centre_of(&harness, "b_3.jpg");
    let to = on_stack(&harness, n);
    drag(&mut harness, from, to);
    run_until(&mut harness, "b_3 in the stack of a", |app| {
        app.controller().is_idle() && stack(app, "b_3.jpg").id == a
    });
    let app = harness.state();
    let joined = stack(app, "b_3.jpg");
    assert_eq!(joined.members.len(), 3);
    assert_eq!(joined.origin, GroupOrigin::Manual);
    let left = stack(app, "b_1.jpg");
    assert_eq!(left.members.len(), 2);
    assert_eq!(left.origin, GroupOrigin::Manual);
    assert!(review(app).unstacked.is_empty());
    // A drag never selects.
    assert_eq!(selection(app), Selection::default());
}

#[test]
fn dragging_a_photo_into_the_tray_unstacks_it() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut harness = harness(&dirs);
    reviewing_near(&mut harness, input.path());
    let job = harness.state().controller().jobs()[0].id;
    let b_3 = photo(harness.state(), "b_3.jpg");

    let from = centre_of(&harness, "b_3.jpg");
    let to = centre_of(&harness, &tray(0));
    drag(&mut harness, from, to);
    run_until(&mut harness, "b_3 in the tray", |app| {
        app.controller().is_idle() && review(app).unstacked == [b_3]
    });
    harness.step();
    assert!(harness.query_by_label(&tray(1)).is_some());
    let left = stack(harness.state(), "b_1.jpg");
    assert_eq!(left.members.len(), 2);
    assert_eq!(left.origin, GroupOrigin::Manual);

    // The tray pins it: a regrouping leaves it there.
    harness
        .get_by_role_and_label(Role::Slider, "Hash distance")
        .set_value_accesskit(30.0);
    run_until(&mut harness, "the regrouping", |app| {
        app.controller().is_idle()
            && app
                .controller()
                .job(job)
                .is_some_and(|view| view.config.hamming_threshold == 30)
    });
    assert_eq!(review(harness.state()).unstacked, [b_3]);
}

#[test]
fn dragging_a_tray_photo_onto_a_stack_adds_it() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut harness = harness(&dirs);
    reviewing_selected(&mut harness, input.path());
    run_until_idle(&mut harness);
    let a = stack(harness.state(), "a_1.jpg").id;
    let n = number_of(harness.state(), "a_1.jpg");
    assert!(harness.query_by_label(&tray(1)).is_some());

    let from = centre_of(&harness, "b_3.jpg");
    let to = harness.get_by_label_contains(&stack_row(n)).rect().center();
    drag(&mut harness, from, to);
    run_until(&mut harness, "b_3 in the stack of a", |app| {
        app.controller().is_idle() && review(app).unstacked.is_empty()
    });
    let joined = stack(harness.state(), "b_3.jpg");
    assert_eq!(joined.id, a);
    assert_eq!(joined.members.len(), 3);
    assert_eq!(joined.origin, GroupOrigin::Manual);
    harness.step();
    assert!(harness.query_by_label(&tray(0)).is_some());
}

#[test]
fn dropping_on_the_new_stack_row_starts_a_stack() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut harness = harness(&dirs);
    reviewing_selected(&mut harness, input.path());
    run_until_idle(&mut harness);
    let b_3 = photo(harness.state(), "b_3.jpg");

    let from = centre_of(&harness, "b_3.jpg");
    let to = centre_of(&harness, NEW_STACK);
    drag(&mut harness, from, to);
    run_until(&mut harness, "b_3 in a stack of its own", |app| {
        app.controller().is_idle() && review(app).unstacked.is_empty()
    });
    harness.step();
    let app = harness.state();
    assert_eq!(review(app).groups.len(), 3);
    let alone = stack(app, "b_3.jpg");
    assert_eq!(alone.members, [b_3]);
    assert_eq!(alone.origin, GroupOrigin::Manual);
    // A stack of one photo is not written: its row says so.
    let label = format!(
        "Stack {}: b_3.jpg, 1 photo, Proposed",
        number_of(app, "b_3.jpg")
    );
    assert!(harness.query_by_label(&label).is_some());
    assert!(painted(&harness, "needs 2 photos"));
    // The row stays, for the next photo.
    assert!(harness.query_by_label(NEW_STACK).is_some());

    // Confirmed, it is still not written: Write does not count it.
    let confirm = format!("Confirm stack {}", number_of(app, "b_3.jpg"));
    click(&mut harness, &confirm);
    run_until(&mut harness, "the stack of one confirmed", |app| {
        app.controller().is_idle() && stack(app, "b_3.jpg").status == GroupStatus::Confirmed
    });
    harness.step();
    assert!(harness.query_by_label("Write 0 stacks").is_some());
}

#[test]
fn dropping_a_photo_on_its_own_stack_changes_nothing() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut harness = harness(&dirs);
    reviewing_selected(&mut harness, input.path());
    run_until_idle(&mut harness);
    let before = review(harness.state()).clone();
    let n = number_of(harness.state(), "a_1.jpg");

    // The photo is carried over its own stack; dropped, nothing is sent: the backend is
    // idle right after the drop.
    let job = before.job;
    let a_1 = photo(harness.state(), "a_1.jpg");
    let from = centre_of(&harness, "a_1.jpg");
    let to = on_stack(&harness, n);
    hold(&mut harness, from, to);
    assert_eq!(carried(&harness), Some(DraggedPhoto { job, file: a_1 }));
    harness.drop_at(to);
    harness.step();
    assert_eq!(carried(&harness), None);
    assert!(harness.state().controller().is_idle());
    // Nor for a photo of the tray dropped on the tray.
    let b_3 = photo(harness.state(), "b_3.jpg");
    let from = centre_of(&harness, "b_3.jpg");
    let to = centre_of(&harness, &tray(1));
    hold(&mut harness, from, to);
    assert_eq!(carried(&harness), Some(DraggedPhoto { job, file: b_3 }));
    harness.drop_at(to);
    harness.step();
    assert!(harness.state().controller().is_idle());

    for _ in 0..5 {
        harness.step();
    }
    let after = review(harness.state());
    assert_eq!(after.groups, before.groups);
    assert_eq!(after.unstacked, before.unstacked);
    assert_eq!(after.selection, Selection::default());
}

#[test]
fn a_refused_drop_says_why() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    for (folder, seed, second) in [("one", 1, 0), ("two", 2, 30)] {
        let folder = input.path().join(folder);
        fs::create_dir(&folder).unwrap();
        write_bracket(&folder, "shot", seed, second, 3);
    }
    let mut harness = harness(&dirs);
    let mut config = JobConfig::new(input.path().to_path_buf());
    config.recursive = true;
    harness.state().controller().submit(config);
    run_until(&mut harness, "the job in review", |app| in_review(app, 1));
    click(&mut harness, &row(input.path(), "Review"));
    run_until_the_review_is_loaded(&mut harness);
    run_until_idle(&mut harness);
    let before = review(harness.state()).groups.clone();
    assert_eq!(before.len(), 2);
    let one = display_path(&Path::new("one").join("shot_1.jpg"));
    let two = display_path(&Path::new("two").join("shot_1.jpg"));
    // The stacks' first photos are both named `shot_1.jpg`: the other stack is the one
    // whose row does not hold `one`'s.
    let one_id = review(harness.state())
        .files
        .iter()
        .position(|file| file.path.ends_with(Path::new("one").join("shot_1.jpg")))
        .unwrap();
    let other = before
        .iter()
        .find(|group| !group.members.contains(&one_id))
        .map(|group| group.position + 1)
        .unwrap();
    assert!(harness.query_by_label(&two).is_some());

    let from = centre_of(&harness, &one);
    let to = on_stack(&harness, other);
    drag(&mut harness, from, to);
    run_until_painted(&mut harness, "A stack cannot hold photos of two folders.");
    run_until_idle(&mut harness);
    assert_eq!(review(harness.state()).groups, before);
}

#[test]
fn a_tray_of_three_thousand_photos_lays_out_only_the_visible_rows() {
    const PHOTOS: usize = 3_000;
    let (_data, dirs) = data_dir();
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("lone");
    store_lone_photos(&dirs, &input, PHOTOS);
    let mut harness = harness(&dirs);
    run_until(&mut harness, "the stored job", |app| in_review(app, 1));
    click(&mut harness, &row(&input, "Review"));
    run_until_the_review_is_loaded(&mut harness);
    harness.step();
    assert!(painted(&harness, "No stack was found in 3000 photos."));
    assert!(harness.query_by_label(&tray(PHOTOS)).is_some());
    let rows = harness.state().stats().tray_rows;
    assert!((1..=6).contains(&rows), "{rows}");
    assert!(harness.query_by_label("IMG_000000.jpg").is_some());
    assert!(harness.query_by_label("IMG_002999.jpg").is_none());
}

#[test]
fn a_photo_held_at_the_edge_of_the_list_scrolls_it() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut harness = harness(&dirs);
    reviewing_selected(&mut harness, input.path());
    // Short enough that the list shows about one row of its three.
    harness.set_size(egui::vec2(900.0, 560.0));
    run_until_idle(&mut harness);
    let first = harness.get_by_label_contains(&stack_row(1)).rect();
    // The list ends 17 points above the tray's title: its card's padding and outline, and
    // the gap above it.
    let list_bottom = harness.get_by_label(&tray(1)).rect().top() - 17.0;
    let near_the_edge = egui::pos2(first.center().x, list_bottom - 8.0);

    let from = centre_of(&harness, "b_3.jpg");
    harness.hover_at(from);
    harness.step();
    harness.drag_at(from);
    harness.step();
    harness.hover_at(from - egui::vec2(0.0, 12.0));
    harness.step();
    harness.hover_at(near_the_edge);
    // Held there, the list scrolls to its end, then stops asking for frames.
    let deadline = Instant::now() + TIMEOUT;
    harness.step();
    while repaint_delay(&harness) <= Duration::from_secs(1) {
        assert!(
            Instant::now() < deadline,
            "the list never stopped scrolling"
        );
        harness.step();
    }
    assert!(harness.query_by_label(NEW_STACK).is_some());
    let scrolled = harness
        .query_by_label_contains(&stack_row(1))
        .map(|node| node.rect().top());
    assert!(
        scrolled.is_none_or(|top| top < first.top() - ROW_HEIGHT),
        "{scrolled:?}"
    );
    // Dropped there, on the new-stack row the list ends with, the photo starts a stack.
    harness.drop_at(near_the_edge);
    harness.step();
    run_until(&mut harness, "b_3 in a stack of its own", |app| {
        app.controller().is_idle() && review(app).groups.len() == 3
    });
}

#[test]
fn the_tray_is_resized_from_its_top_edge() {
    // The tray's default height.
    const HEIGHT: f32 = 150.0;
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut harness = harness(&dirs);
    reviewing_selected(&mut harness, input.path());
    // Not resized yet: no height of its own.
    assert_eq!(harness.state().controller().prefs.panels.tray, None);
    let title = harness.get_by_label(&tray(1)).rect().top();

    // Taken by its top edge and raised 60 points.
    let edge = centre_of(&harness, "Tray height");
    let raised = edge - egui::vec2(0.0, 60.0);
    harness.hover_at(edge);
    harness.step();
    harness.drag_at(edge);
    harness.step();
    harness.hover_at(raised);
    harness.step();
    harness.drop_at(raised);
    harness.step();
    harness.step();
    let tray_height = harness.state().controller().prefs.panels.tray;
    let tray_height = tray_height.expect("the tray's height is the user's now");
    assert!((tray_height - (HEIGHT + 60.0)).abs() < 1.0, "{tray_height}");
    let moved = title - harness.get_by_label(&tray(1)).rect().top();
    assert!((moved - 60.0).abs() < 1.0, "{moved}");
}

// --- The inspector ---

/// The inspector's button that splits the checked photos off the stack it shows.
const SPLIT: &str = "Split off the checked photos";

/// The stack of the open review that holds the photo named `name`, if one does.
fn stack_holding(app: &App, name: &str) -> Option<GroupView> {
    let id = photo(app, name);
    review(app)
        .groups
        .iter()
        .find(|group| group.members.contains(&id))
        .cloned()
}

/// The names of `photos` of the open review, in their order.
fn names(app: &App, photos: &[usize]) -> Vec<String> {
    photos
        .iter()
        .map(|&id| folder_name(&review(app).files[id].path))
        .collect()
}

/// Whether the control labelled `label` is disabled.
fn disabled(harness: &Harness<'static, App>, label: &str) -> bool {
    harness.get_by_label(label).accesskit_node().is_disabled()
}

#[test]
fn selecting_a_stack_shows_its_members_and_its_predicted_file() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut harness = harness(&dirs);
    reviewing_near(&mut harness, input.path());
    // Nothing selected: the job's facts.
    assert!(harness.query_by_label_contains("Input: ").is_some());

    let n = number_of(harness.state(), "b_1.jpg");
    click_stack(&mut harness, n, Modifiers::NONE);
    // The prediction comes from the inspector's thread.
    run_until_painted(&mut harness, "b_1_3.psd");
    assert!(painted(&harness, "Will be written as"));
    assert!(painted(&harness, "640 \u{d7} 426, 8-bit"));
    for name in ["b_1.jpg", "b_2.jpg", "b_3.jpg"] {
        assert!(
            harness.query_by_label(&format!("Check {name}")).is_some(),
            "{name}"
        );
        assert!(
            harness
                .query_by_label(&format!("Move {name} to unstacked"))
                .is_some(),
            "{name}"
        );
    }
    for label in ["Confirm", "Reject", SPLIT, "Merge stacks"] {
        assert!(harness.query_by_label(label).is_some(), "{label}");
    }

    // Its Confirm decides it.
    click(&mut harness, "Confirm");
    run_until(&mut harness, "the stack confirmed", |app| {
        decision(app, "b_1.jpg") == GroupStatus::Confirmed
    });
}

#[test]
fn split_off_the_checked_photos_makes_a_second_stack() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut harness = harness(&dirs);
    reviewing_selected(&mut harness, input.path());
    // One stack of four, the a's and the b's, selected.
    let (a, b) = {
        let app = harness.state();
        (stack(app, "a_1.jpg").id, stack(app, "b_1.jpg").id)
    };
    let controller = harness.state_mut().controller_mut();
    controller.select_group(a, false);
    controller.select_group(b, true);
    controller.merge_selected();
    run_until(&mut harness, "the merged stack", |app| {
        app.controller().is_idle() && review(app).groups.len() == 1
    });
    harness.step();
    assert!(disabled(&harness, SPLIT));

    click(&mut harness, "Check a_1.jpg");
    let a_1 = photo(harness.state(), "a_1.jpg");
    assert_eq!(harness.state().controller().checked_members(), [a_1]);
    // One checked would leave a stack of one photo.
    assert!(disabled(&harness, SPLIT));
    click(&mut harness, "Check a_2.jpg");
    assert!(!disabled(&harness, SPLIT));

    click(&mut harness, SPLIT);
    run_until(&mut harness, "the second stack", |app| {
        app.controller().is_idle() && review(app).groups.len() == 2
    });
    let app = harness.state();
    let part = stack(app, "a_1.jpg");
    assert_eq!(names(app, &part.members), ["a_1.jpg", "a_2.jpg"]);
    let rest = stack(app, "b_1.jpg");
    assert_eq!(names(app, &rest.members), ["b_1.jpg", "b_2.jpg"]);
    assert_eq!(rest.id, b);
    assert!(app.controller().checked_members().is_empty());
}

#[test]
fn merge_joins_the_selected_stacks() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut harness = harness(&dirs);
    reviewing_selected(&mut harness, input.path());
    let b = stack(harness.state(), "b_1.jpg").id;

    click_stack(&mut harness, 1, Modifiers::NONE);
    // One stack has nothing to merge with.
    assert!(disabled(&harness, "Merge stacks"));
    click_stack(&mut harness, 2, Modifiers::COMMAND);
    click(&mut harness, "Merge 2 stacks");
    run_until(&mut harness, "the merged stack", |app| {
        app.controller().is_idle() && review(app).groups.len() == 1
    });
    let app = harness.state();
    let merged = stack(app, "a_1.jpg");
    assert_eq!(merged.id, b);
    assert_eq!(merged.members.len(), 4);
    assert_eq!(merged.origin, GroupOrigin::Manual);
    assert_eq!(selection(app), stacks(&[b]));
}

#[test]
fn move_to_unstacked_from_the_inspector_works_without_a_mouse_drag() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut harness = harness(&dirs);
    reviewing_near(&mut harness, input.path());
    let b_3 = photo(harness.state(), "b_3.jpg");
    let a = stack(harness.state(), "a_1.jpg").id;

    // From the keyboard: the button has the keyboard, and Enter presses it.
    click(&mut harness, "b_3.jpg");
    harness.get_by_label("Move to unstacked").focus();
    harness.step();
    press(&mut harness, Key::Enter);
    run_until(&mut harness, "b_3 in the tray", |app| {
        app.controller().is_idle() && review(app).unstacked == [b_3]
    });
    harness.step();

    // From the tray, `Move to stack…` offers the stacks of its folder.
    click(&mut harness, "b_3.jpg");
    assert!(disabled(&harness, "Move to unstacked"));
    click(&mut harness, "Move to stack…");
    let n = number_of(harness.state(), "a_1.jpg");
    click(&mut harness, &format!("Stack {n} (a_1.jpg)"));
    run_until(&mut harness, "b_3 in the stack of a", |app| {
        app.controller().is_idle()
            && stack_holding(app, "b_3.jpg").is_some_and(|stack| stack.id == a)
    });
    harness.step();

    // `Start a new stack` makes it a stack of its own.
    harness.state_mut().controller_mut().select_photo(b_3);
    harness.step();
    click(&mut harness, "Start a new stack");
    run_until(&mut harness, "b_3 in a stack of its own", |app| {
        app.controller().is_idle()
            && stack_holding(app, "b_3.jpg").is_some_and(|stack| stack.members == [b_3])
    });
}

#[test]
fn a_photo_shows_its_exposure_and_its_pair() {
    let (_data, dirs) = data_dir();
    let input = raw_bracket_folder();
    let mut harness = harness(&dirs);
    reviewing_selected(&mut harness, input.path());

    click(&mut harness, "brk_2.raf");
    for fact in [
        "Canon EOS 2000D",
        "35 mm",
        "1/250 s",
        "f/8",
        "ISO 100",
        "2026-05-01 10:00:01",
        "Exposure bracket, shot 2",
        "Paired with brk_2.jpg, which is not stacked.",
    ] {
        assert!(painted(&harness, fact), "{fact}");
    }
    assert!(harness.query_by_label("Stack brk_2.jpg instead").is_some());
    for label in ["Move to unstacked", "Start a new stack", "Move to stack…"] {
        assert!(harness.query_by_label(label).is_some(), "{label}");
    }
}

#[test]
fn stack_the_jpeg_instead_swaps_the_pair() {
    let (_data, dirs) = data_dir();
    let input = raw_bracket_folder();
    let mut harness = harness(&dirs);
    reviewing_selected(&mut harness, input.path());
    let (raf, jpg) = (
        photo(harness.state(), "brk_2.raf"),
        photo(harness.state(), "brk_2.jpg"),
    );
    let bracket = stack(harness.state(), "brk_2.raf").id;

    click(&mut harness, "brk_2.raf");
    click(&mut harness, "Stack brk_2.jpg instead");
    run_until(&mut harness, "the JPEG in the stack", |app| {
        app.controller().is_idle()
            && stack_holding(app, "brk_2.jpg").is_some()
            && review(app).files[raf].paired_with == Some(jpg)
    });
    let app = harness.state();
    let swapped = stack(app, "brk_2.jpg");
    assert_eq!(swapped.id, bracket);
    assert_eq!(
        names(app, &swapped.members),
        ["brk_1.raf", "brk_2.jpg", "brk_3.raf"]
    );
    assert_eq!(review(app).files[jpg].paired_with, None);
}

#[test]
fn the_larger_preview_waits_while_a_job_runs() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let other = bracket_folder();
    let gate = Arc::new(Gate::default());
    gate.open();
    let mut harness = harness_with(&dirs, gate.holding_the_scan());
    let _open = OpenOnDrop(Arc::clone(&gate));
    reviewing_selected(&mut harness, input.path());
    let a_1 = photo(harness.state(), "a_1.jpg");

    // Another job's scan, held while it examines its photos.
    gate.close();
    submit(&harness, other.path());
    run_until(&mut harness, "the second scan held", |app| {
        app.controller().jobs().iter().any(|job| {
            job.status == JobStatus::Scanning
                && app
                    .controller()
                    .progress(job.id)
                    .is_some_and(|progress| progress.stage == Stage::Hash)
        })
    });
    harness.state_mut().controller_mut().select_photo(a_1);
    for _ in 0..5 {
        harness.step();
    }
    assert!(painted(&harness, PREVIEW_WAITS));
    assert_eq!(harness.state().inspect().requested(), 0);

    // Once the scan ends, the next frame asks for it, and it comes.
    gate.open();
    run_until(&mut harness, "the preview", |app| {
        in_review(app, 2) && app.inspect().requested() == 1 && !app.inspecting()
    });
    harness.step();
    assert!(!painted(&harness, PREVIEW_WAITS));
}

#[test]
fn a_write_that_leaves_stacks_proposed_asks_first() {
    use GroupStatus::Proposed;
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut harness = harness(&dirs);
    let job = reviewing_selected(&mut harness, input.path());
    let disabled = harness.get_by_label("Write 0 stacks");
    assert!(disabled.accesskit_node().is_disabled());
    click(&mut harness, "Confirm stack 1");
    run_until(&mut harness, "stack 1 confirmed", |app| {
        app.controller().is_idle() && app.controller().write_counts().to_write == 1
    });
    harness.step();

    // One stack is still proposed: the write would end the review with it undecided.
    let asked = "Write 1 stack now? 1 stack is still proposed and will not be written. The review of this job ends with the write.";
    click(&mut harness, "Write 1 stack");
    assert!(painted(&harness, asked));
    // `Keep reviewing` has the keyboard, and Esc keeps reviewing too.
    assert!(harness.get_by_label("Keep reviewing").is_focused());
    press(&mut harness, Key::Escape);
    assert_eq!(harness.state().controller().question(), None);
    click(&mut harness, "Write 1 stack");
    click(&mut harness, "Keep reviewing");
    run_until_idle(&mut harness);
    assert_eq!(status(harness.state(), job), Some(JobStatus::ReviewPending));
    assert_eq!(decision(harness.state(), "b_1.jpg"), Proposed);

    // `Write 1 stack` writes it: the proposed one is left alone.
    click(&mut harness, "Write 1 stack");
    click(&mut harness, "Write 1 stack now");
    run_until_written(&mut harness, job);
    assert_eq!(decision(harness.state(), "a_1.jpg"), GroupStatus::Written);
    assert_eq!(decision(harness.state(), "b_1.jpg"), Proposed);
}

#[test]
fn a_written_stack_reveals_its_file() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let platform = Arc::new(FakePlatform::default());
    let mut harness = harness_revealing(&dirs, Arc::clone(&platform));
    let job = reviewing_selected(&mut harness, input.path());
    click(&mut harness, "Confirm all");
    run_until(&mut harness, "both stacks confirmed", |app| {
        app.controller().write_counts().to_write == 2
    });
    harness.step();
    click(&mut harness, "Write 2 stacks");
    run_until(&mut harness, "the job written", |app| {
        status(app, job) == Some(JobStatus::Completed)
            && app.controller().is_idle()
            && review(app)
                .groups
                .iter()
                .all(|group| group.status == GroupStatus::Written)
    });
    let written = stack(harness.state(), "a_1.jpg").clone();
    let path = written
        .output_path
        .clone()
        .expect("a written stack has a file");
    harness
        .state_mut()
        .controller_mut()
        .select_group(written.id, false);
    harness.step();
    assert!(painted(&harness, &folder_name(&path)));

    click(&mut harness, &format!("Reveal {}", folder_name(&path)));
    let deadline = Instant::now() + TIMEOUT;
    while platform.revealed.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "nothing was revealed");
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(*platform.revealed.lock().unwrap(), [path]);
}

#[test]
fn the_files_a_scan_could_not_read_stay_listed_once_it_ends() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    fs::write(input.path().join("broken.jpg"), b"not a jpeg").unwrap();
    let mut harness = harness(&dirs);
    reviewing_selected(&mut harness, input.path());
    let broken = photo(harness.state(), "broken.jpg");
    let reason = review(harness.state()).files[broken]
        .error
        .clone()
        .expect("broken.jpg could not be read");
    harness.step();

    // Nothing selected in the review: the job's facts, with the file and why.
    let section = "Could not be read (1)";
    assert!(painted(&harness, section));
    assert!(
        harness
            .query_by_label("Could not be read: broken.jpg")
            .is_some()
    );
    assert!(painted(&harness, &sentence(&reason)));

    // At the smallest window, inside the inspector.
    harness.set_size(egui::vec2(900.0, 560.0));
    harness.step();
    harness.step();
    let inspector = harness.get_by_label("Inspector").rect();
    let row = harness.get_by_label("Could not be read: broken.jpg").rect();
    assert!(painted(&harness, section));
    assert!(inspector.contains_rect(row), "{row:?} in {inspector:?}");
}

#[test]
fn the_inspector_renders_at_the_smallest_window() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut harness = harness(&dirs);
    reviewing_near(&mut harness, input.path());
    harness.set_size(egui::vec2(900.0, 560.0));
    harness.step();
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 560.0));
    let b = stack(harness.state(), "b_1.jpg").id;
    let b_3 = photo(harness.state(), "b_3.jpg");

    let inside = |harness: &Harness<'static, App>, labels: &[&str], state: &str| {
        let inspector = harness.get_by_label("Inspector").rect();
        assert!(screen.contains_rect(inspector), "{state}: {inspector:?}");
        // From 260 px, the inspector gives way to the centre: nothing in it widens it.
        assert!(inspector.width() <= 261.0, "{state}: {inspector:?}");
        for label in labels {
            let control = harness.get_by_label(label).rect();
            assert!(
                inspector.contains_rect(control),
                "{state}: {label} leaves the inspector: {control:?} in {inspector:?}"
            );
        }
    };
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        harness.ctx.set_theme(theme);
        harness.state_mut().controller_mut().clear_selection();
        harness.step();
        harness.step();
        assert!(
            harness.query_by_label_contains("Input: ").is_some(),
            "{theme:?}"
        );
        inside(&harness, &[], &format!("{theme:?}, nothing selected"));

        harness.state_mut().controller_mut().select_group(b, false);
        run_until_painted(&mut harness, "b_1_3.psd");
        inside(
            &harness,
            &[
                "Confirm",
                "Reject",
                SPLIT,
                "Merge stacks",
                "Check b_1.jpg",
                "Move b_1.jpg to unstacked",
            ],
            &format!("{theme:?}, a stack"),
        );

        harness.state_mut().controller_mut().select_photo(b_3);
        harness.step();
        harness.step();
        inside(
            &harness,
            &["Move to unstacked", "Start a new stack", "Move to stack…"],
            &format!("{theme:?}, a photo"),
        );
    }
}

#[test]
fn a_member_with_warnings_shows_its_size_and_depth() {
    const SIZE: &str = "640 \u{d7} 426, 8-bit";
    let (_data, dirs) = data_dir();
    // Photos without EXIF: each one warns that its capture time is its file's.
    let input = review_folder();
    let mut harness = harness(&dirs);
    reviewing_selected(&mut harness, input.path());
    click_stack(&mut harness, 1, Modifiers::NONE);
    run_until(&mut harness, "the prediction", |app| !app.inspecting());
    harness.step();
    let members = review(harness.state()).primary().unwrap().members.len();

    // At the inspector's default width, then at its least, in the smallest window.
    for (window, width) in [
        (egui::vec2(1360.0, 900.0), 320.0),
        (egui::vec2(900.0, 560.0), 260.0),
    ] {
        harness.set_size(window);
        harness.step();
        harness.step();
        let inspector = harness.get_by_label("Inspector").rect();
        assert!((inspector.width() - width).abs() < 24.0, "{inspector:?}");
        // The members' warnings, not the stacks' of the rows; egui gives a label's name
        // as its node's value.
        let warnings: Vec<egui::Rect> = harness
            .query_all_by(|node| {
                node.value().is_some_and(|label| {
                    label.starts_with("Warnings of ") && !label.starts_with("Warnings of stack ")
                })
            })
            .map(|node| node.rect())
            .collect();
        assert_eq!(warnings.len(), members, "{width}");
        let sizes: Vec<egui::Rect> = harness
            .query_all_by(|node| {
                node.role() == Role::Label && node.value().as_deref() == Some(SIZE)
            })
            .map(|node| node.rect())
            .collect();
        // Each member's line: its size and depth, in the inspector, before its warnings.
        for mark in warnings {
            let beside = sizes.iter().find(|size| {
                (size.center().y - mark.center().y).abs() < 2.0 && size.right() <= mark.left()
            });
            let size = beside.unwrap_or_else(|| panic!("{width}: none beside {mark:?}: {sizes:?}"));
            assert!(
                inspector.contains_rect(*size),
                "{width}: {size:?} in {inspector:?}"
            );
        }
    }
}

#[test]
fn the_inspector_is_resized_from_its_edge() {
    // The inspector's default width.
    const WIDTH: f32 = 320.0;
    let (_data, dirs) = data_dir();
    let mut harness = harness(&dirs);
    harness.step();
    assert_eq!(harness.state().controller().prefs.panels.inspector, WIDTH);
    let inspector = harness.get_by_label("Inspector").rect();
    // Inside its frame: 8 px of margin on each side, and the separator line.
    assert!(
        inspector.width() < WIDTH && inspector.width() > WIDTH - 24.0,
        "{inspector:?}"
    );

    // Taken by its left edge, egui's splitter, and pulled 40 points to the left.
    let edge = harness
        .get_all_by_label("Resize panel")
        .map(|splitter| splitter.rect().center())
        .max_by(|a, b| a.x.total_cmp(&b.x))
        .unwrap();
    assert!((edge.x - inspector.left()).abs() < 16.0, "{edge:?}");
    let pulled = edge - egui::vec2(40.0, 0.0);
    harness.hover_at(edge);
    harness.step();
    harness.drag_at(edge);
    harness.step();
    harness.hover_at(pulled);
    harness.step();
    harness.drop_at(pulled);
    harness.step();
    harness.step();
    let width = harness.state().controller().prefs.panels.inspector;
    assert!((width - (WIDTH + 40.0)).abs() < 1.0, "{width}");
    let grown = harness.get_by_label("Inspector").rect().width() - inspector.width();
    assert!((grown - 40.0).abs() < 1.0, "{grown}");

    // A screen reader's step resizes it as well: the width drawn becomes the preference.
    let dragged = harness.get_by_label("Inspector").rect().width();
    inspector_splitter(&harness).increment_accesskit();
    harness.step();
    harness.step();
    let stepped = harness.get_by_label("Inspector").rect().width();
    assert!(stepped > dragged + 1.0, "{stepped} after {dragged}");
    let width = harness.state().controller().prefs.panels.inspector;
    assert!(
        (width - (WIDTH + 40.0 + stepped - dragged)).abs() < 1.0,
        "{width}"
    );
}

/// The inspector's edge: egui's splitter furthest right.
fn inspector_splitter<'tree>(harness: &'tree Harness<'static, App>) -> egui_kittest::Node<'tree> {
    harness
        .get_all_by_label("Resize panel")
        .max_by(|a, b| a.rect().center().x.total_cmp(&b.rect().center().x))
        .unwrap()
}

#[test]
fn the_side_panels_give_way_to_a_narrow_window_and_grow_back() {
    let (_data, dirs) = data_dir();
    let mut harness = harness(&dirs);
    harness.step();
    // The jobs list's width, and the inspector's, as their edges, egui's splitters, show.
    let widths = |harness: &Harness<'static, App>, window: f32| {
        let edges: Vec<f32> = harness
            .get_all_by_label("Resize panel")
            .map(|splitter| splitter.rect().center().x)
            .collect();
        let jobs = edges.iter().copied().fold(f32::INFINITY, f32::min);
        let inspector = window - edges.iter().copied().fold(0.0, f32::max);
        (jobs, inspector)
    };
    let wide = widths(&harness, 1360.0);
    assert!(
        (wide.0 - 260.0).abs() < 4.0 && (wide.1 - 320.0).abs() < 4.0,
        "{wide:?}"
    );

    harness.set_size(egui::vec2(900.0, 560.0));
    harness.step();
    harness.step();
    let narrow = widths(&harness, 900.0);
    assert!(
        narrow.0 < wide.0 - 10.0 && narrow.1 < wide.1 - 10.0,
        "{narrow:?}"
    );
    // The sizes the window allowed never become the preferences.
    let panels = harness.state().controller().prefs.panels;
    assert_eq!((panels.jobs, panels.inspector), (260.0, 320.0));

    harness.set_size(egui::vec2(1360.0, 900.0));
    harness.step();
    harness.step();
    let again = widths(&harness, 1360.0);
    assert!(
        (again.0 - wide.0).abs() < 1.0 && (again.1 - wide.1).abs() < 1.0,
        "{again:?} after {wide:?}"
    );
}

// --- The writing and the results ---

/// The write's progress bar.
const WRITE_PROGRESS: &str = "Write progress";
/// The results' `Open output folder`, named apart from the selected job's row's.
const OPEN_OUTPUT: &str = "Open the output folder";
/// What a cancelled job with confirmed stacks left to write offers.
const WRITE_CONFIRMED: &str = "Write the confirmed stacks";

/// A folder of three brackets of three, 20 s apart, `first_<n>.jpg`, `second_<n>.jpg`
/// and `third_<n>.jpg`: stacks 1, 2 and 3. (Inside a new job's 30 s window, each
/// bracket's shot numbers starting again keep them apart.)
fn three_brackets() -> tempfile::TempDir {
    let input = tempfile::tempdir().unwrap();
    for (n, prefix) in ["first", "second", "third"].into_iter().enumerate() {
        write_bracket(input.path(), prefix, n as u64 + 1, 20 * n as u32, 3);
    }
    input
}

/// Selects the job on `input` once it is in review, confirms all its stacks and clicks
/// `write`, the Write button of that many; returns the job.
fn confirm_all_and_write(harness: &mut Harness<'static, App>, input: &Path, write: &str) -> JobId {
    let job = reviewing_selected(harness, input);
    click(harness, "Confirm all");
    run_until(harness, "every stack confirmed", |app| {
        app.controller().is_idle() && app.controller().write_counts().proposed == 0
    });
    harness.step();
    click(harness, write);
    job
}

/// Draws frames until `job` is done and what its write did is read and drawn.
fn run_until_written(harness: &mut Harness<'static, App>, job: JobId) {
    run_until(harness, "the job written", |app| {
        status(app, job) == Some(JobStatus::Completed) && app.controller().is_idle()
    });
    harness.step();
}

/// Draws frames until `job`'s write, held by [`Gate::holding_the_write`] after its first
/// stack, has had that stack read and drawn.
fn run_until_held_after_one(harness: &mut Harness<'static, App>, job: JobId) {
    run_until(harness, "the write held after its first stack", |app| {
        status(app, job) == Some(JobStatus::Writing)
            && app.controller().is_idle()
            && review(app).results().written == 1
    });
    harness.step();
}

/// Waits until `platform` has revealed something, and returns what.
fn revealed(platform: &FakePlatform) -> Vec<PathBuf> {
    let deadline = Instant::now() + TIMEOUT;
    while platform.revealed.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "nothing was revealed");
        thread::sleep(Duration::from_millis(5));
    }
    platform.revealed.lock().unwrap().clone()
}

/// A point of stack `n`'s row of the results away from its thumbnail, its texts and its
/// button: under its second line, in its middle.
fn on_result(harness: &Harness<'static, App>, n: usize) -> egui::Pos2 {
    let rect = harness.get_by_label_contains(&stack_row(n)).rect();
    rect.center_bottom() - egui::vec2(0.0, 4.0)
}

#[test]
fn a_write_shows_its_progress_and_each_stack_as_it_is_written() {
    let (_data, dirs) = data_dir();
    let input = three_brackets();
    let gate = Arc::new(Gate::default());
    let wake = gate.holding_the_write(input.path().join("stacked"), 1);
    let mut harness = harness_with(&dirs, wake);
    let _open = OpenOnDrop(Arc::clone(&gate));
    let job = confirm_all_and_write(&mut harness, input.path(), "Write 3 stacks");
    run_until_held_after_one(&mut harness, job);

    // How far it is: the first stack's three photos of the nine, its last one just done.
    assert!(harness.query_by_label(WRITE_PROGRESS).is_some());
    assert!(painted(&harness, "Writing stacks"));
    assert!(painted(&harness, "3 of 9 photos"));
    assert!(painted(&harness, "first_3.jpg"));
    assert!(harness.query_by_label(CANCEL_JOB).is_some());
    // Every stack confirmed for the write: the first written, with its file, the others
    // waiting for it.
    let label = "Stack 1: first_1.jpg, 3 photos, Written";
    assert!(harness.query_by_label(label).is_some());
    for (n, first) in [(2, "second_1.jpg"), (3, "third_1.jpg")] {
        let label = format!("Stack {n}: {first}, 3 photos, Waiting");
        assert!(harness.query_by_label(&label).is_some(), "{label}");
    }
    let written = stack(harness.state(), "first_1.jpg").output_path.clone();
    let written = written.expect("a written stack has a file");
    assert!(painted(&harness, &folder_name(&written)));
    assert!(harness.query_by_label("Reveal stack 1").is_some());
    assert!(harness.query_by_label("Reveal stack 2").is_none());
    assert_eq!(harness.state().stats().stack_rows, 3);

    gate.open();
    run_until_written(&mut harness, job);
    for (n, first) in [(1, "first_1.jpg"), (2, "second_1.jpg"), (3, "third_1.jpg")] {
        let label = format!("Stack {n}: {first}, 3 photos, Written");
        assert!(harness.query_by_label(&label).is_some(), "{label}");
        assert!(
            harness
                .query_by_label(&format!("Reveal stack {n}"))
                .is_some()
        );
    }
    assert!(painted(&harness, "3 written, 0 skipped, 0 failed"));
    assert!(harness.query_by_label(WRITE_PROGRESS).is_none());
    assert!(harness.query_by_label(CANCEL_JOB).is_none());
    // Nothing was left alone, nothing is left to retry.
    assert!(!painted(&harness, "not written"));
    assert!(harness.query_by_label_contains("Retry ").is_none());
}

#[test]
fn a_finished_job_lists_its_files_and_reveals_them() {
    use GroupStatus::{Rejected, Written};
    let (_data, dirs) = data_dir();
    let input = three_brackets();
    let platform = Arc::new(FakePlatform::default());
    let mut harness = harness_revealing(&dirs, Arc::clone(&platform));
    let job = reviewing_selected(&mut harness, input.path());
    click(&mut harness, "Confirm stack 1");
    click(&mut harness, "Confirm stack 2");
    click(&mut harness, "Reject stack 3");
    run_until(&mut harness, "the decisions", |app| {
        app.controller().is_idle()
            && app.controller().write_counts().to_write == 2
            && review(app).results().rejected == 1
    });
    harness.step();
    click(&mut harness, "Write 2 stacks");
    run_until_written(&mut harness, job);

    let files: Vec<PathBuf> = ["first_1.jpg", "second_1.jpg"]
        .iter()
        .map(|name| stack(harness.state(), name).output_path.clone().unwrap())
        .collect();
    for (n, (first, file)) in ["first_1.jpg", "second_1.jpg"]
        .iter()
        .zip(&files)
        .enumerate()
    {
        let label = format!("Stack {}: {first}, 3 photos, Written", n + 1);
        assert!(harness.query_by_label(&label).is_some(), "{label}");
        assert!(painted(&harness, &folder_name(file)), "{}", file.display());
    }
    // The rejected stack is not listed, only counted, in a line right under the list's
    // last row, which is far shorter than the centre.
    assert!(harness.query_by_label_contains(&stack_row(3)).is_none());
    assert!(painted(&harness, "1 rejected stack was not written."));
    let last_row = harness.get_by_label_contains(&stack_row(2)).rect();
    let note = harness
        .get_by_label("1 rejected stack was not written.")
        .rect();
    let gap = note.top() - last_row.bottom();
    assert!((0.0..=24.0).contains(&gap), "{gap} px under the last row");
    assert!(painted(&harness, "2 written, 0 skipped, 0 failed"));

    click(&mut harness, "Reveal stack 2");
    assert_eq!(revealed(&platform), [files[1].clone()]);

    // A click on a row selects its stack, which the inspector shows; on its thumbnail,
    // the top layer.
    let first = stack(harness.state(), "first_1.jpg").id;
    let at = on_result(&harness, 1);
    click_at(&mut harness, at);
    assert_eq!(selection(harness.state()), stacks(&[first]));
    let inspector_reveal = format!("Reveal {}", folder_name(&files[0]));
    assert!(harness.query_by_label(&inspector_reveal).is_some());
    let top = photo(harness.state(), "first_1.jpg");
    // The row names its first photo too, in a label.
    let thumbnail = harness
        .get_by_role_and_label(Role::Image, "first_1.jpg")
        .rect()
        .center();
    click_at(&mut harness, thumbnail);
    let expected = Selection {
        groups: vec![first],
        photo: Some(top),
    };
    assert_eq!(selection(harness.state()), expected);

    // The review's keys do nothing here: no decision, no step. The engine would refuse a
    // decision on a done job, so the decisions alone could not tell: no refusal may come
    // back either. The app takes the notices into toasts every frame, so the toast is
    // what would show one.
    for key in [Key::Enter, Key::Delete, Key::ArrowDown, Key::ArrowRight] {
        press(&mut harness, key);
    }
    run_until_idle(&mut harness);
    assert!(!painted(&harness, "That job is done now"));
    assert_eq!(selection(harness.state()), expected);
    assert_eq!(decision(harness.state(), "first_1.jpg"), Written);
    assert_eq!(decision(harness.state(), "third_1.jpg"), Rejected);
    // Esc clears the selection, as in the review: the job's facts show again.
    press(&mut harness, Key::Escape);
    assert_eq!(selection(harness.state()), Selection::default());
    assert!(harness.query_by_label_contains("Input: ").is_some());
}

#[test]
fn open_output_folder_goes_through_the_platform() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let platform = Arc::new(FakePlatform::default());
    let mut harness = harness_revealing(&dirs, Arc::clone(&platform));
    let job = confirm_all_and_write(&mut harness, input.path(), "Write 2 stacks");
    run_until_written(&mut harness, job);
    let output = harness
        .state()
        .controller()
        .job(job)
        .unwrap()
        .config
        .output_dir
        .clone();

    click(&mut harness, OPEN_OUTPUT);
    assert_eq!(revealed(&platform), [output]);
}

#[test]
fn a_skipped_stack_says_why() {
    let (_data, dirs) = data_dir();
    // Two shots of one scene, the second smaller: one stack, which the write skips, since
    // only one of its photos has the canvas's resolution.
    let input = tempfile::tempdir().unwrap();
    let image = scene(5, WIDTH, HEIGHT);
    let smaller = image::imageops::resize(
        &image,
        WIDTH * 3 / 4,
        HEIGHT * 3 / 4,
        image::imageops::FilterType::Triangle,
    );
    for (name, image) in [("pier_1.jpg", &image), ("pier_2.jpg", &smaller)] {
        fs::write(input.path().join(name), plain_jpeg(image)).unwrap();
    }
    let mut harness = harness(&dirs);
    let job = confirm_all_and_write(&mut harness, input.path(), "Write 1 stack");
    run_until_written(&mut harness, job);

    let label = "Stack 1: pier_1.jpg, 2 photos, Skipped";
    assert!(harness.query_by_label(label).is_some());
    let reason = "1 of 2 photos could be read at the canvas resolution; a stack needs at least 2.";
    assert!(painted(&harness, reason));
    // The photo left out, with why on hover.
    assert!(harness.query_by_label("Warnings of stack 1: 1").is_some());
    assert!(painted(&harness, "0 written, 1 skipped, 0 failed"));
    // Written again it would be skipped again: nothing to retry.
    assert_eq!(review(harness.state()).results().stranded, 0);
    assert!(harness.query_by_label_contains("Retry ").is_none());

    // Selected, the inspector says why in the same sentence as the row.
    let at = on_result(&harness, 1);
    click_at(&mut harness, at);
    let said: Vec<String> = painted_texts(&harness)
        .into_iter()
        .filter(|text| text.contains("a stack needs at least 2"))
        .collect();
    assert_eq!(said, [reason, reason]);
}

#[test]
fn retry_is_offered_for_stranded_stacks_and_writes_them() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut harness = harness(&dirs);
    let job = reviewing_selected(&mut harness, input.path());
    click(&mut harness, "Confirm all");
    run_until(&mut harness, "both stacks confirmed", |app| {
        app.controller().write_counts().to_write == 2
    });
    harness.step();
    // Stack 1's photos go away before the write, as on a drive that dropped.
    let away = tempfile::tempdir().unwrap();
    let moved = ["a_1.jpg", "a_2.jpg"];
    for name in moved {
        fs::rename(input.path().join(name), away.path().join(name)).unwrap();
    }
    click(&mut harness, "Write 2 stacks");
    run_until_written(&mut harness, job);
    assert!(
        harness
            .query_by_label("Stack 1: a_1.jpg, 2 photos, Skipped")
            .is_some()
    );
    assert!(
        harness
            .query_by_label("Stack 2: b_1.jpg, 2 photos, Written")
            .is_some()
    );
    assert!(painted(&harness, "1 written, 1 skipped, 0 failed"));
    assert!(harness.query_by_label("Retry 1 stack").is_some());

    // Back in place, the retry writes it.
    for name in moved {
        fs::rename(away.path().join(name), input.path().join(name)).unwrap();
    }
    click(&mut harness, "Retry 1 stack");
    run_until(&mut harness, "every stack written", |app| {
        status(app, job) == Some(JobStatus::Completed)
            && app.controller().is_idle()
            && review(app)
                .groups
                .iter()
                .all(|group| group.status == GroupStatus::Written)
    });
    harness.step();
    assert!(
        harness
            .query_by_label("Stack 1: a_1.jpg, 2 photos, Written")
            .is_some()
    );
    assert!(painted(&harness, "2 written, 0 skipped, 0 failed"));
    assert!(harness.query_by_label("Retry 1 stack").is_none());
    assert_eq!(files_in(&input.path().join("stacked")).len(), 2);
}

#[test]
fn cancelling_a_write_keeps_what_was_written() {
    let (_data, dirs) = data_dir();
    let input = three_brackets();
    let output = input.path().join("stacked");
    let gate = Arc::new(Gate::default());
    let mut harness = harness_with(&dirs, gate.holding_the_write(output.clone(), 1));
    let _open = OpenOnDrop(Arc::clone(&gate));
    let job = confirm_all_and_write(&mut harness, input.path(), "Write 3 stacks");
    run_until_held_after_one(&mut harness, job);

    click(&mut harness, CANCEL_JOB);
    // The cancel must reach the engine before the write is let go, or the write finishes.
    run_until(&mut harness, "the cancel to run", |app| {
        app.controller().is_idle()
    });
    gate.open();
    run_until(&mut harness, "the cancelled job", |app| {
        status(app, job) == Some(JobStatus::Cancelled) && app.controller().is_idle()
    });
    harness.step();
    assert!(
        harness
            .query_by_label("Stack 1: first_1.jpg, 3 photos, Written")
            .is_some()
    );
    for (n, first) in [(2, "second_1.jpg"), (3, "third_1.jpg")] {
        let label = format!("Stack {n}: {first}, 3 photos, Confirmed");
        assert!(harness.query_by_label(&label).is_some(), "{label}");
    }
    assert_eq!(files_in(&output).len(), 1);
    assert!(painted(&harness, "1 written, 0 skipped, 0 failed"));
    assert!(harness.query_by_label(CANCELLED).is_some());
    assert!(harness.query_by_label(RETRY_JOB).is_some());

    // The confirmed stacks left are written on request.
    click(&mut harness, WRITE_CONFIRMED);
    run_until_written(&mut harness, job);
    assert!(
        review(harness.state())
            .groups
            .iter()
            .all(|group| group.status == GroupStatus::Written)
    );
    assert_eq!(files_in(&output).len(), 3);
    assert!(harness.query_by_label(WRITE_CONFIRMED).is_none());
}

/// A folder of [`write_ambiguous_stack`]'s photos, and a job on it with a model at
/// [`NEAR`]: one stack, which the scan's visual check embeds in two batches.
fn checked_visually() -> (tempfile::TempDir, JobConfig) {
    let input = tempfile::tempdir().unwrap();
    write_ambiguous_stack(input.path());
    let mut config = JobConfig::new(input.path().to_path_buf());
    config.ml_model = Some("dinov2-small".into());
    config.hamming_threshold = NEAR;
    (input, config)
}

/// Draws frames until `embedder` holds a visual check at its gate.
fn run_until_held_in_the_visual_check(
    harness: &mut Harness<'static, App>,
    embedder: &HeldEmbedder,
) {
    run_until(harness, "the visual check held", |_| {
        embedder.called.load(Ordering::SeqCst)
    });
}

#[test]
fn a_job_cancelled_before_its_review_shows_no_write_results() {
    let (_data, dirs) = data_dir();
    let (input, config) = checked_visually();
    let (other, other_config) = checked_visually();
    let gate = Arc::new(Gate::default());
    let embedder = Arc::new(HeldEmbedder::new(&gate));
    let mut harness = harness_embedding(&dirs, Arc::clone(&embedder) as Arc<dyn EmbedSource>);
    let _open = OpenOnDrop(Arc::clone(&gate));

    // Cancelled while its scan's visual check runs: its analysis is saved, its stack is
    // proposed, and none was ever sent to a write.
    harness.state().controller().submit(config);
    run_until_held_in_the_visual_check(&mut harness, &embedder);
    run_until(&mut harness, "the job's row to say Scanning", |app| {
        job_on(app, input.path()).is_some_and(|job| status(app, job) == Some(JobStatus::Scanning))
    });
    let job = job_on(harness.state(), input.path()).unwrap();
    click(&mut harness, &row(input.path(), "Scanning"));
    click(&mut harness, CANCEL_JOB);
    // The cancel must reach the engine before the visual check is let go.
    run_until(&mut harness, "the cancel to run", |app| {
        app.controller().is_idle()
    });
    gate.open();
    run_until(&mut harness, "the cancelled job, its scan saved", |app| {
        app.controller().is_idle()
            && app
                .controller()
                .job(job)
                .is_some_and(|view| view.status == JobStatus::Cancelled && view.counts.files > 0)
    });
    run_until_idle(&mut harness);

    // What a cancelled job showed before it had a write: its sentence and Retry.
    assert!(harness.query_by_label(CANCELLED).is_some());
    assert!(harness.query_by_label(RETRY_JOB).is_some());
    assert!(!painted(&harness, " written, "), "the tallies");
    assert!(harness.query_by_label(OPEN_OUTPUT).is_none());
    assert!(!painted(&harness, "No stack was written."));
    assert!(!painted(&harness, "not written"));
    assert!(!painted(&harness, "Waiting to write."));

    // Retried, it is queued for its scan, behind another job held in its own visual check.
    gate.close();
    embedder.called.store(false, Ordering::SeqCst);
    harness.state().controller().submit(other_config);
    run_until_held_in_the_visual_check(&mut harness, &embedder);
    run_until(&mut harness, "the other job's row to say Scanning", |app| {
        job_on(app, other.path()).is_some_and(|job| status(app, job) == Some(JobStatus::Scanning))
    });
    let running = job_on(harness.state(), other.path()).unwrap();
    click(&mut harness, &row(input.path(), "Cancelled"));
    click(&mut harness, RETRY_JOB);
    run_until(&mut harness, "the job queued again", |app| {
        app.controller().is_idle() && status(app, job) == Some(JobStatus::Queued)
    });
    harness.step();
    let waiting_for = format!(
        "Waiting for {}.",
        harness.state().controller().job_name(running)
    );
    assert!(
        harness.query_by_label(&waiting_for).is_some(),
        "{waiting_for}"
    );
    assert!(harness.query_by_label(CANCEL_JOB).is_some());
    assert!(!painted(&harness, "Waiting to write."));
    assert!(!painted(&harness, " written, "), "the tallies");

    gate.open();
    run_until(&mut harness, "both jobs in review", |app| in_review(app, 2));
}

// --- Settings ---

/// The title of Settings, which the toolbar's `Settings` opens.
const SETTINGS_TITLE: &str = "App settings";
/// What sets the defaults of new jobs back to the built-in ones.
const RESET: &str = "Reset to the built-in defaults";
/// Settings' `Done`, named apart from a done job's status chip.
const CLOSE_SETTINGS: &str = "Close settings";

fn settings_shown(app: &App) -> bool {
    app.controller().view() == &View::Settings
}

/// Whether the toolbar's `Settings` button reads as selected.
fn settings_button_selected(harness: &Harness<'static, App>) -> bool {
    harness.get_by_label("Settings").accesskit_node().toggled() == Some(Toggled::True)
}

#[test]
fn settings_open_from_the_toolbar_and_close_with_done() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    store_brackets(&dirs, input.path(), 2);
    let mut harness = harness(&dirs);
    run_until(&mut harness, "the jobs to be read", |app| {
        app.controller().jobs_loaded()
    });

    assert!(!settings_button_selected(&harness));
    click(&mut harness, "Settings");
    assert!(settings_shown(harness.state()));
    assert!(settings_button_selected(&harness));
    for label in [
        SETTINGS_TITLE,
        "Appearance",
        "New jobs start with",
        "About",
        CLOSE_SETTINGS,
    ] {
        assert!(harness.query_by_label(label).is_some(), "{label}");
    }
    click(&mut harness, CLOSE_SETTINGS);
    assert_eq!(harness.state().controller().view(), &View::Job);
    assert!(harness.query_by_label(SETTINGS_TITLE).is_none());
    assert!(!settings_button_selected(&harness));

    // Over the new-job form, which Esc brings back with what was typed.
    click(&mut harness, "New job");
    click(&mut harness, "Photo folder");
    harness.get_by_label("Photo folder").type_text("/photos");
    harness.step();
    click(&mut harness, "Settings");
    assert!(settings_shown(harness.state()));
    assert!(harness.query_by_label("Photo folder").is_none());
    harness.key_press(Key::Escape);
    harness.step();
    assert!(form_shown(harness.state()));
    assert_eq!(field(&harness, "Photo folder"), Some("/photos".into()));

    // A job picked in the list leaves Settings for the job.
    click(&mut harness, "Settings");
    click(&mut harness, &row(input.path(), "Review"));
    assert_eq!(harness.state().controller().view(), &View::Job);
    assert!(harness.query_by_label(SETTINGS_TITLE).is_none());

    // So does New job, for the form, which kept what was typed.
    click(&mut harness, "Settings");
    click(&mut harness, "New job");
    assert!(form_shown(harness.state()));
    assert_eq!(field(&harness, "Photo folder"), Some("/photos".into()));
}

#[test]
fn choosing_dark_changes_the_theme_at_once() {
    let (_data, dirs) = data_dir();
    let mut harness = harness(&dirs);
    let preference =
        |harness: &Harness<'static, App>| harness.ctx.options(|options| options.theme_preference);
    let chosen = |harness: &Harness<'static, App>, label: &str| {
        harness.get_by_label(label).accesskit_node().toggled() == Some(Toggled::True)
    };
    click(&mut harness, "Settings");
    assert!(chosen(&harness, "Follow the system"));

    click(&mut harness, "Dark");
    assert_eq!(harness.state().controller().prefs.theme, ThemeChoice::Dark);
    assert_eq!(preference(&harness), ThemePreference::Dark);
    assert!(chosen(&harness, "Dark"));
    assert!(!chosen(&harness, "Follow the system"));
    // The toolbar's toggle shows the same choice, and a change there shows here.
    assert!(harness.query_by_label("Theme: dark").is_some());
    click(&mut harness, "Theme: dark");
    assert_eq!(preference(&harness), ThemePreference::System);
    assert!(chosen(&harness, "Follow the system"));
    click(&mut harness, "Light");
    assert_eq!(preference(&harness), ThemePreference::Light);
    assert!(harness.query_by_label("Theme: light").is_some());
}

#[test]
fn reset_brings_the_built_in_defaults_back() {
    let (_data, dirs) = data_dir();
    let mut harness = harness(&dirs);
    click(&mut harness, "Settings");
    assert!(
        disabled(&harness, RESET),
        "the defaults are the built-in ones"
    );

    harness
        .get_by_role_and_label(Role::Slider, "Hash distance")
        .set_value_accesskit(12.0);
    harness.step();
    click(&mut harness, "Layered TIFF");
    click(&mut harness, "Scan subfolders");
    let defaults = harness.state().controller().prefs.defaults.clone();
    assert_eq!(
        (
            defaults.hamming_threshold,
            defaults.output_format,
            defaults.recursive
        ),
        (12, OutputFormat::Tiff, true)
    );
    assert!(!disabled(&harness, RESET));

    // A new job starts from them.
    click(&mut harness, "New job");
    assert_eq!(
        harness.state_mut().controller_mut().form().unwrap().fields,
        defaults
    );
    click(&mut harness, "Cancel new job");

    click(&mut harness, "Settings");
    click(&mut harness, RESET);
    assert_eq!(
        harness.state().controller().prefs.defaults,
        FormDefaults::default()
    );
    assert!(disabled(&harness, RESET));
    let shown = harness
        .get_by_role_and_label(Role::Slider, "Hash distance")
        .accesskit_node()
        .numeric_value();
    let built_in = FormDefaults::default().hamming_threshold;
    assert_eq!(shown, Some(f64::from(built_in)));
    run_until(&mut harness, "the defaults to be saved", |app| {
        app.controller().is_idle()
    });
    drop(harness);
    assert_eq!(controller(&dirs).prefs.defaults, FormDefaults::default());
}

#[test]
fn the_about_card_names_the_lgpl_components() {
    let (_data, dirs) = data_dir();
    let mut harness = harness(&dirs);
    click(&mut harness, "Settings");

    let version = format!("Photostacker {}", env!("CARGO_PKG_VERSION"));
    for label in [
        version.as_str(),
        "rawler",
        "LGPL-2.1",
        "https://github.com/dnglab/dnglab",
        "Inter",
        "OFL-1.1",
    ] {
        assert!(harness.query_by_label(label).is_some(), "{label}");
    }
    #[cfg(feature = "heic")]
    {
        assert!(harness.query_by_label("libheif and libde265").is_some());
        assert!(harness.query_by_label("LGPL-3.0").is_some());
        for source in [
            "https://github.com/strukturag/libheif",
            "https://github.com/strukturag/libde265",
        ] {
            assert!(harness.query_by_label(source).is_some(), "{source}");
        }
        assert!(harness.query_by_label_contains("HEVC").is_some());
    }
    #[cfg(not(feature = "heic"))]
    {
        assert!(harness.query_by_label("libheif and libde265").is_none());
        assert!(
            harness
                .query_by_label("https://github.com/strukturag/libheif")
                .is_none()
        );
        assert!(harness.query_by_label_contains("HEVC").is_none());
    }
    #[cfg(feature = "ml")]
    {
        assert!(harness.query_by_label("ONNX Runtime").is_some());
        assert!(harness.query_by_label("DINOv2 models").is_some());
        assert!(harness.query_by_label("Apache-2.0").is_some());
    }
    #[cfg(not(feature = "ml"))]
    assert!(harness.query_by_label("ONNX Runtime").is_none());
    drop(harness);

    // The logs folder is shown by the file manager, on the reveal's thread.
    let platform = Arc::new(FakePlatform::default());
    let (_logs_data, logs_dirs) = data_dir();
    fs::create_dir_all(logs_dirs.logs()).unwrap();
    let mut harness = harness_revealing(&logs_dirs, Arc::clone(&platform));
    click(&mut harness, "Settings");
    // Below the window's bottom edge, at the end of the cards: pressed as a screen
    // reader does (`settings_render_at_the_smallest_window` scrolls to it).
    harness
        .get_by_label("Open the logs folder")
        .click_accesskit();
    harness.step();
    let deadline = Instant::now() + TIMEOUT;
    while platform.revealed.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "the logs folder was never shown");
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(*platform.revealed.lock().unwrap(), [logs_dirs.logs()]);
}

/// The About card's way to the licence texts a package ships.
const OPEN_LICENCES: &str = "Open the licences folder";
/// What the About card says above [`OPEN_LICENCES`].
const LICENCES_SHIP: &str = "The licence texts of every part ship with the app.";

#[test]
fn the_about_card_opens_the_licences_folder_when_the_package_has_one() {
    // A development build has no licences folder: the card says nothing of one.
    let (_data, dirs) = data_dir();
    let mut harness = harness(&dirs);
    click(&mut harness, "Settings");
    assert!(harness.query_by_label("Open the logs folder").is_some());
    assert!(harness.query_by_label(OPEN_LICENCES).is_none());
    assert!(harness.query_by_label(LICENCES_SHIP).is_none());
    drop(harness);

    // A package's: the sentence and the button, which shows that folder in the file
    // manager, on the reveal's thread.
    let licences = tempfile::tempdir().unwrap();
    let platform = Arc::new(FakePlatform::default());
    let (_package_data, package_dirs) = data_dir();
    let mut harness = harness_with_options(&package_dirs, Arc::new(|| {}), |options| {
        options.platform = platform.clone();
        options.licences = Some(licences.path().to_path_buf());
    });
    click(&mut harness, "Settings");
    assert!(harness.query_by_label(LICENCES_SHIP).is_some());
    let button = harness.get_by_role_and_label(Role::Button, OPEN_LICENCES);
    // Below the window's bottom edge, at the end of the cards: pressed as a screen
    // reader does.
    button.click_accesskit();
    harness.step();
    let deadline = Instant::now() + TIMEOUT;
    while platform.revealed.lock().unwrap().is_empty() {
        assert!(
            Instant::now() < deadline,
            "the licences folder was never shown"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        *platform.revealed.lock().unwrap(),
        [licences.path().to_path_buf()]
    );
}

#[test]
fn settings_render_at_the_smallest_window() {
    let (_data, dirs) = data_dir();
    let mut harness = harness(&dirs);
    harness.set_size(egui::vec2(900.0, 560.0));
    harness.step();
    click(&mut harness, "Settings");

    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 560.0));
    let right = harness.get_by_label("Inspector").rect().min.x;
    let done = harness.get_by_label(CLOSE_SETTINGS).rect();
    assert!(screen.contains_rect(done), "Done is off screen: {done:?}");
    let mut controls = vec![
        (None, "Follow the system"),
        (None, "Light"),
        (None, "Dark"),
        (None, "Scan subfolders"),
        (None, "PSD"),
        (None, "Layered TIFF"),
        (None, "Write without review"),
        (None, "sRGB"),
        (None, "Linear (for HDR merging)"),
        (None, "Stack the RAW"),
        (None, "Stack the JPEG"),
        (Some(Role::Slider), "Hash distance"),
        (Some(Role::Slider), "Time window (s)"),
        (Some(Role::Slider), "Visual check floor"),
        (None, RESET),
    ];
    // The visual check's card, in a build that has it.
    if cfg!(feature = "ml") {
        controls.extend([
            (None, ML_SWITCH),
            (None, "Use DINOv2 small"),
            (None, DOWNLOAD_SMALL),
            (None, "Import a file for DINOv2 base"),
            (None, "Custom similarity"),
        ]);
    }
    controls.extend([
        (None, "rawler"),
        (None, "Inter"),
        (None, "Open the logs folder"),
    ]);
    for (role, label) in controls {
        match role {
            Some(role) => harness.get_by_role_and_label(role, label).scroll_to_me(),
            None => harness.get_by_label(label).scroll_to_me(),
        }
        // Long enough for egui's scroll animation, which takes up to 0.3 s.
        for _ in 0..4 {
            harness.step();
        }
        let rect = match role {
            Some(role) => harness.get_by_role_and_label(role, label).rect(),
            None => harness.get_by_label(label).rect(),
        };
        assert!(
            screen.contains_rect(rect),
            "{label} is off screen: {rect:?}"
        );
        assert!(
            rect.min.x >= JOBS_WIDTH && rect.max.x <= right,
            "{label} leaves the centre: {rect:?}"
        );
        // Done stays where it was, whatever is scrolled into view.
        assert_eq!(
            harness.get_by_label(CLOSE_SETTINGS).rect(),
            done,
            "after {label}"
        );
    }
}

// --- The models of Stage 3 ---

/// The switch of the visual check.
const ML_SWITCH: &str = "Check ambiguous stacks by their look";
/// The `Download` of the default model's row.
const DOWNLOAD_SMALL: &str = "Download DINOv2 small";
/// What `Download` asks first.
const DOWNLOAD_QUESTION: &str = "Download DINOv2 small? 84 MB through huggingface.co, or \
    through github.com if that cannot be reached. Nothing else is sent or received.";

/// Whether the toolbar's ML chip, a button whose name is its text, reads `text`.
fn chip(harness: &Harness<'static, App>, text: &str) -> bool {
    harness
        .query_by_role_and_label(Role::Button, text)
        .is_some()
}

/// Opens Settings, draws frames until the models are read, and scrolls their card, the
/// one above About, into view.
fn open_the_models(harness: &mut Harness<'static, App>) {
    click(harness, "Settings");
    run_until(harness, "the models", |app| {
        app.controller().models().len() == MODELS.len()
    });
    harness.step();
    harness.get_by_label("About").scroll_to_me();
    for _ in 0..4 {
        harness.step();
    }
}

/// Draws frames until a model's download is under way, past its first byte.
fn run_until_downloading(harness: &mut Harness<'static, App>) {
    run_until(harness, "the download", |app| {
        app.controller().models().iter().any(
            |view| matches!(view.activity, ModelActivity::Downloading { done, .. } if done > 0),
        )
    });
    harness.step();
}

/// Draws frames until the default model is installed and idle, and its row says so. The
/// model's file is in place a moment before the engine ends the download or the import: a
/// read of the models made in between shows the model installed and still busy, and its
/// row then shows the download, not `Delete`.
fn run_until_installed(harness: &mut Harness<'static, App>) {
    run_until(harness, "the model installed", |app| {
        app.controller().models().iter().any(|view| {
            view.spec.id == "dinov2-small" && view.installed && view.activity == ModelActivity::Idle
        })
    });
    harness.step();
}

#[test]
fn turning_the_switch_on_downloads_nothing() {
    let (_data, dirs) = data_dir();
    let installer = FakeInstaller::new();
    let mut harness = harness_installing(&dirs, &installer);
    let chip_text = if cfg!(feature = "ml") {
        "ML off"
    } else {
        "ML unavailable"
    };
    // The chip opens Settings on the visual check's card, in view.
    harness
        .get_by_role_and_label(Role::Button, chip_text)
        .click();
    for _ in 0..4 {
        harness.step();
    }
    assert!(settings_shown(harness.state()));
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1360.0, 900.0));
    let title = harness.get_by_label("Visual check (Stage 3)").rect();
    assert!(screen.contains_rect(title), "{title:?}");
    if !cfg!(feature = "ml") {
        assert!(painted(&harness, "This build has no visual check."));
        assert!(harness.query_by_label(ML_SWITCH).is_none());
        return;
    }
    let switch = harness.get_by_label(ML_SWITCH).rect();
    assert!(screen.contains_rect(switch), "{switch:?}");

    click(&mut harness, ML_SWITCH);
    run_until(&mut harness, "the switch saved", |app| {
        app.controller().is_idle()
    });
    thread::sleep(Duration::from_millis(200));
    harness.step();

    assert!(harness.state().controller().prefs.ml_enabled);
    assert!(harness.state().controller().question().is_none());
    assert_eq!(installer.calls(), 0, "the switch downloaded something");
    // The chip, and the status bar's same words as text.
    assert!(chip(&harness, "ML: no model"));
    assert!(
        harness
            .query_by_role_and_label(Role::Label, "ML: no model")
            .is_some()
    );
    assert!(harness.query_by_label(DOWNLOAD_SMALL).is_some());

    // Choosing the other model downloads nothing either.
    click(&mut harness, "Use DINOv2 base");
    assert_eq!(
        harness.state().controller().ml_state(),
        MlState::Missing("dinov2-base")
    );
    // Nor does a job submitted while its model is missing: it names the model, and is
    // checked once the model is installed.
    let input = bracket_folder();
    {
        let controller = harness.state_mut().controller_mut();
        controller.open_new_job(Some(input.path().to_path_buf()));
        controller.submit_form();
    }
    run_until(&mut harness, "the job in review", |app| in_review(app, 1));
    let job = job_on(harness.state(), input.path()).unwrap();
    let config = &harness.state().controller().job(job).unwrap().config;
    assert_eq!(config.ml_model.as_deref(), Some("dinov2-base"));
    run_until(&mut harness, "the backend idle", |app| {
        app.controller().is_idle()
    });
    thread::sleep(Duration::from_millis(200));
    harness.step();
    assert_eq!(
        installer.calls(),
        0,
        "a choice or a job downloaded something"
    );
    assert!(harness.state().controller().question().is_none());
}

/// Draws frames, 5 ms apart, until a text painted is `text`, whole.
fn run_until_painted_whole(harness: &mut Harness<'static, App>, text: &str) {
    let deadline = Instant::now() + TIMEOUT;
    while !painted_texts(harness).iter().any(|painted| painted == text) {
        assert!(
            Instant::now() < deadline,
            "{text:?} was never painted whole"
        );
        thread::sleep(Duration::from_millis(5));
        harness.step();
    }
}

/// Whether anything on screen, painted or named, says that ONNX Runtime is missing.
fn says_missing(harness: &Harness<'static, App>) -> bool {
    ["not found", "is missing"]
        .iter()
        .any(|words| painted(harness, words) || harness.query_by_label_contains(words).is_some())
}

/// A library that needs a newer macOS is said in one sentence wherever the app says that
/// ONNX Runtime is missing: on hover the ML chip (before what it does) and the status bar,
/// which keep their words, the new-job form and the models' card; never as missing.
#[test]
fn a_library_that_needs_a_newer_macos_says_so() {
    let (_data, dirs) = data_dir();
    let mut harness = harness_with_options(&dirs, Arc::new(|| {}), |options| {
        options.runtime = runtime_needing_macos();
    });
    assert!(chip(&harness, "ML unavailable"));
    let status = harness.query_by_role_and_label(Role::Label, "ML unavailable");
    assert!(status.is_some());
    if !cfg!(feature = "ml") {
        // A build without the visual check has no library to need anything.
        assert!(harness.query_by_label_contains("macOS").is_none());
        return;
    }
    assert!(!painted(&harness, NEEDS_MACOS));

    // The chip's hover, then the status bar's.
    harness
        .get_by_role_and_label(Role::Button, "ML unavailable")
        .hover();
    run_until_painted_whole(
        &mut harness,
        &format!("{NEEDS_MACOS}\nThe visual check of new jobs: open its settings"),
    );
    harness
        .get_by_role_and_label(Role::Label, "ML unavailable")
        .hover();
    run_until_painted_whole(&mut harness, NEEDS_MACOS);
    assert!(!says_missing(&harness));
    // Away from both, it goes.
    harness.get_by_label("New job").hover();
    let deadline = Instant::now() + TIMEOUT;
    while painted(&harness, NEEDS_MACOS) {
        assert!(Instant::now() < deadline, "the status bar's hover stayed");
        harness.step();
    }

    // The new-job form says it in place of its sentence.
    harness.state_mut().controller_mut().open_new_job(None);
    harness.step();
    harness.get_by_label(NEEDS_MACOS).scroll_to_me();
    for _ in 0..4 {
        harness.step();
    }
    assert!(painted(&harness, NEEDS_MACOS));
    assert!(!says_missing(&harness));
    click(&mut harness, "Cancel new job");

    // So does the models' card, after its warning icon.
    open_the_models(&mut harness);
    harness.get_by_label(NEEDS_MACOS).scroll_to_me();
    for _ in 0..4 {
        harness.step();
    }
    assert!(painted(&harness, NEEDS_MACOS));
    assert!(!says_missing(&harness));
}

#[test]
fn download_asks_first_then_shows_progress_then_installed() {
    let (_data, dirs) = data_dir();
    let installer = FakeInstaller::holding();
    let mut harness = harness_installing(&dirs, &installer);
    let _open = OpenOnDrop(Arc::clone(&installer.gate));
    open_the_models(&mut harness);
    if !cfg!(feature = "ml") {
        assert!(harness.query_by_label(DOWNLOAD_SMALL).is_none());
        return;
    }
    click(&mut harness, ML_SWITCH);

    // Not now: nothing is downloaded.
    click(&mut harness, DOWNLOAD_SMALL);
    assert!(harness.query_by_label(DOWNLOAD_QUESTION).is_some());
    click(&mut harness, "Not now");
    assert!(harness.state().controller().question().is_none());
    assert!(harness.query_by_label(DOWNLOAD_QUESTION).is_none());
    run_until(&mut harness, "the backend idle", |app| {
        app.controller().is_idle()
    });
    assert_eq!(installer.calls(), 0);

    // Download: its progress, held at half the model.
    click(&mut harness, DOWNLOAD_SMALL);
    click(&mut harness, "Download");
    run_until_downloading(&mut harness);
    let bar = harness.get_by_role_and_label(Role::ProgressIndicator, "Download of DINOv2 small");
    assert_eq!(bar.accesskit_node().numeric_value(), Some(50.0));
    assert!(painted(&harness, "50 % of 84 MB"));
    assert!(
        harness
            .query_by_label("Cancel the download of DINOv2 small")
            .is_some()
    );
    assert!(chip(&harness, "ML 50 %"));

    installer.gate.open();
    run_until_installed(&mut harness);
    assert!(harness.query_by_label("Delete DINOv2 small").is_some());
    assert!(harness.query_by_label(DOWNLOAD_SMALL).is_none());
    assert!(chip(&harness, "ML ready"));
    assert_eq!(installer.calls(), 1);
}

#[test]
fn a_download_can_be_cancelled() {
    let (_data, dirs) = data_dir();
    let installer = FakeInstaller::holding();
    let mut harness = harness_installing(&dirs, &installer);
    let _open = OpenOnDrop(Arc::clone(&installer.gate));
    open_the_models(&mut harness);
    if !cfg!(feature = "ml") {
        return;
    }
    click(&mut harness, DOWNLOAD_SMALL);
    click(&mut harness, "Download");
    run_until_downloading(&mut harness);

    click(&mut harness, "Cancel the download of DINOv2 small");
    run_until_painted(&mut harness, "Cancelling…");
    installer.gate.open();
    run_until(&mut harness, "the download's end", |app| {
        app.controller()
            .models()
            .iter()
            .all(|view| view.activity == ModelActivity::Idle)
    });
    harness.step();

    // Not installed, nothing failed, and what was downloaded is kept for the next time.
    assert!(harness.query_by_label(DOWNLOAD_SMALL).is_some());
    assert!(!painted(&harness, "Cannot download"));
    let part = dirs.models().join("dinov2-small").join("model.onnx.part");
    assert!(part.is_file());
    assert!(painted(&harness, "Models use 42 MB on disk."));
}

#[test]
fn a_failed_download_shows_its_reason_and_can_be_tried_again() {
    let (_data, dirs) = data_dir();
    let installer = FakeInstaller::failing();
    let mut harness = harness_installing(&dirs, &installer);
    open_the_models(&mut harness);
    if !cfg!(feature = "ml") {
        return;
    }
    click(&mut harness, DOWNLOAD_SMALL);
    click(&mut harness, "Download");

    // Each address by its host, as the question named them.
    let reason =
        format!("Cannot download: huggingface.co: {FAILED_REASON}; github.com: {FAILED_REASON}.");
    run_until_painted(&mut harness, &reason);
    // It can be downloaded or imported still.
    assert!(harness.query_by_label(DOWNLOAD_SMALL).is_some());
    assert!(
        harness
            .query_by_label("Import a file for DINOv2 small")
            .is_some()
    );

    // Try again downloads at once, without asking again.
    installer.fails.store(false, Ordering::SeqCst);
    click(&mut harness, "Try again with DINOv2 small");
    assert!(harness.state().controller().question().is_none());
    run_until_installed(&mut harness);
    assert!(!painted(&harness, &reason));
    assert!(harness.query_by_label("Delete DINOv2 small").is_some());
    assert_eq!(installer.calls(), 2);
}

#[test]
fn delete_removes_an_installed_model() {
    let (_data, dirs) = data_dir();
    place_model(&dirs.models(), models::model("dinov2-small").unwrap());
    let installer = FakeInstaller::new();
    let mut harness = harness_installing(&dirs, &installer);
    open_the_models(&mut harness);
    if !cfg!(feature = "ml") {
        assert!(harness.query_by_label("Delete DINOv2 small").is_none());
        return;
    }
    assert!(painted(&harness, "Models use 84 MB on disk."));

    click(&mut harness, "Delete DINOv2 small");
    run_until(&mut harness, "the model removed", |app| {
        app.controller().models().iter().all(|view| !view.installed)
    });
    harness.step();

    assert!(!dirs.models().join("dinov2-small").exists());
    assert!(harness.query_by_label(DOWNLOAD_SMALL).is_some());
    assert!(painted(&harness, "Models use 0 MB on disk."));
    assert_eq!(installer.calls(), 0);
}

#[test]
fn a_job_in_review_is_checked_once_its_model_is_installed() {
    let (_data, dirs) = data_dir();
    let (input, config) = checked_visually();
    let installer = FakeInstaller::new();
    let mut harness = harness_installing(&dirs, &installer);
    harness.state().controller().submit(config);
    run_until(&mut harness, "the job in review", |app| in_review(app, 1));
    click(&mut harness, &row(input.path(), "Review"));
    run_until_the_review_is_loaded(&mut harness);
    // Its model is not installed: its stack's visual check was skipped.
    run_until_painted(&mut harness, "ML skipped");
    if !cfg!(feature = "ml") {
        return;
    }

    open_the_models(&mut harness);
    click(&mut harness, DOWNLOAD_SMALL);
    click(&mut harness, "Download");
    run_until_installed(&mut harness);
    click(&mut harness, CLOSE_SETTINGS);

    // Checked with no other click: every photo looks alike to the stand-in model.
    run_until_painted(&mut harness, "ML 1.00");
    assert!(!painted(&harness, "ML skipped"));
}

// --- End to end: from an empty data folder to files on disk, through the widgets ---

/// Drops `folder` on the window, and starts the job the form it opens shows.
fn start_on(harness: &mut Harness<'static, App>, folder: &Path) {
    drop_folder(harness, folder);
    assert!(form_shown(harness.state()));
    click(harness, "Start");
}

/// The stacks of the open review as the screen shows them, in their order: each one's id,
/// and the layer names the inspector lists once its row is clicked, top layer first.
fn stacks_shown(harness: &mut Harness<'static, App>) -> Vec<(GroupId, Vec<String>)> {
    let count = review(harness.state()).groups.len();
    (1..=count)
        .map(|n| {
            click_stack(harness, n, Modifiers::NONE);
            run_until(harness, "the inspector's prediction", |app| {
                !app.inspecting()
            });
            harness.step();
            let app = harness.state();
            let [id] = selection(app).groups[..] else {
                panic!("stack {n} is not selected alone: {:?}", selection(app));
            };
            let names = shown_layer_names(harness);
            let members = stack_by_id(app, id).members.len();
            assert_eq!(names.len(), members, "stack {n}: {names:?}");
            (id, names)
        })
        .collect()
}

/// The stack `id` of the open review.
fn stack_by_id(app: &App, id: GroupId) -> &GroupView {
    review(app)
        .groups
        .iter()
        .find(|group| group.id == id)
        .unwrap_or_else(|| panic!("no stack {id}"))
}

/// Draws frames until `job`, on `input`, is done, its row says so, and what it wrote is
/// read.
fn run_until_done(harness: &mut Harness<'static, App>, input: &Path, job: JobId) {
    run_until_drawn(harness, &row(input, "Done"));
    run_until(harness, "the written stacks read", |app| {
        app.controller().is_idle()
            && status(app, job) == Some(JobStatus::Completed)
            && review(app)
                .groups
                .iter()
                .all(|group| group.status != GroupStatus::Confirmed)
    });
}

/// The files directly in `folder`, in order, under its canonical path, as the engine
/// records the stacks it writes: on Windows a canonical path has the verbatim prefix
/// `\\?\`, and on macOS the temporary folder is reached through a link.
fn paths_in(folder: &Path) -> Vec<PathBuf> {
    files_in(&folder.canonicalize().unwrap())
        .into_keys()
        .collect()
}

#[test]
fn a_folder_becomes_stacks() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let mut harness = harness(&dirs);
    start_on(&mut harness, input.path());
    run_until(&mut harness, "the job in review", |app| in_review(app, 1));
    run_until_the_review_is_loaded(&mut harness);
    harness.step();
    let job = harness.state().controller().jobs()[0].id;
    let shown = stacks_shown(&mut harness);
    assert_eq!(shown.len(), 2);

    click(&mut harness, "Confirm all");
    run_until(&mut harness, "both stacks confirmed", |app| {
        app.controller().is_idle() && app.controller().write_counts().to_write == 2
    });
    harness.step();
    click(&mut harness, "Write 2 stacks");
    run_until_done(&mut harness, input.path(), job);

    let mut written = Vec::new();
    for (id, names) in &shown {
        let stack = stack_by_id(harness.state(), *id);
        assert_eq!(stack.status, GroupStatus::Written);
        let file = stack
            .output_path
            .clone()
            .expect("a written stack has a file");
        assert_eq!(psd_layer_names(&file), *names, "{}", file.display());
        written.push(file);
    }
    written.sort();
    assert_eq!(paths_in(&input.path().join("stacked")), written);
}

#[test]
fn a_reviewed_and_edited_job_writes_what_was_reviewed() {
    let (_data, dirs) = data_dir();
    // Without EXIF, `a` and `b`, with `b_3` in the tray (`review_folder`); with a Canon's
    // EXIF, the brackets `first` and `second`, 20 s apart: four stacks.
    let input = review_folder();
    for (n, prefix) in ["first", "second"].into_iter().enumerate() {
        write_bracket(input.path(), prefix, 11 + n as u64, 20 * n as u32, 3);
    }
    let mut harness = harness(&dirs);
    // Tall enough for every row of the review.
    harness.set_size(egui::vec2(1360.0, 1400.0));
    start_on(&mut harness, input.path());
    run_until(&mut harness, "the job in review", |app| in_review(app, 1));
    run_until_the_review_is_loaded(&mut harness);
    run_until_idle(&mut harness);
    let job = harness.state().controller().jobs()[0].id;
    assert_eq!(review(harness.state()).groups.len(), 4);

    // The hash slider raised: `b_3` joins `b`.
    harness
        .get_by_role_and_label(Role::Slider, "Hash distance")
        .set_value_accesskit(f64::from(NEAR));
    harness.step();
    run_until(&mut harness, "b_3 in its stack", |app| {
        app.controller().is_idle()
            && stack_holding(app, "b_3.jpg").is_some_and(|stack| stack.members.len() == 3)
    });
    harness.step();

    // A photo dragged into the tray.
    let second_3 = photo(harness.state(), "second_3.jpg");
    let from = centre_of(&harness, "second_3.jpg");
    let to = centre_of(&harness, &tray(0));
    drag(&mut harness, from, to);
    run_until(&mut harness, "second_3 in the tray", |app| {
        app.controller().is_idle() && review(app).unstacked == [second_3]
    });
    harness.step();

    // Two stacks merged.
    let (a, first) = {
        let app = harness.state();
        (number_of(app, "a_1.jpg"), number_of(app, "first_1.jpg"))
    };
    click_stack(&mut harness, a, Modifiers::NONE);
    click_stack(&mut harness, first, Modifiers::COMMAND);
    click(&mut harness, "Merge 2 stacks");
    run_until(&mut harness, "the merged stack", |app| {
        app.controller().is_idle() && review(app).groups.len() == 3
    });
    harness.step();

    // One rejected, the rest confirmed.
    let b = number_of(harness.state(), "b_1.jpg");
    click(&mut harness, &format!("Reject stack {b}"));
    run_until(&mut harness, "b rejected", |app| {
        app.controller().is_idle() && decision(app, "b_1.jpg") == GroupStatus::Rejected
    });
    harness.step();
    click(&mut harness, "Confirm all");
    run_until(&mut harness, "the rest confirmed", |app| {
        app.controller().is_idle() && app.controller().write_counts().proposed == 0
    });
    harness.step();

    // What the screen shows now is what is written.
    let shown = stacks_shown(&mut harness);
    assert_eq!(shown.len(), 3);
    let rejected = stack(harness.state(), "b_1.jpg").id;
    assert!(harness.query_by_label(&tray(1)).is_some());
    assert!(harness.query_by_label("second_3.jpg").is_some());
    click(&mut harness, "Write 2 stacks");
    run_until_done(&mut harness, input.path(), job);

    let mut written = Vec::new();
    for (id, names) in &shown {
        let stack = stack_by_id(harness.state(), *id);
        if *id == rejected {
            assert_eq!(stack.status, GroupStatus::Rejected);
            assert_eq!(stack.output_path, None);
            continue;
        }
        assert_eq!(stack.status, GroupStatus::Written);
        let file = stack
            .output_path
            .clone()
            .expect("a written stack has a file");
        let layers = psd_layer_names(&file);
        assert_eq!(layers, *names, "{}", file.display());
        // The photo dragged into the tray is in no file.
        assert!(
            layers.iter().all(|layer| !layer.starts_with("second_3")),
            "{layers:?}"
        );
        written.push(file);
    }
    written.sort();
    assert_eq!(paths_in(&input.path().join("stacked")), written);
    assert_eq!(review(harness.state()).unstacked, [second_3]);
}

#[test]
fn write_without_review_goes_straight_to_done() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let mut harness = harness(&dirs);
    drop_folder(&mut harness, input.path());
    click(&mut harness, "Write without review");
    let switch = harness.get_by_label("Write without review");
    assert_eq!(switch.accesskit_node().toggled(), Some(Toggled::True));
    click(&mut harness, "Start");

    // Every frame until the job is done: no review shown, nor a job in review.
    let done = row(input.path(), "Done");
    let deadline = Instant::now() + TIMEOUT;
    while harness.query_by_label(&done).is_none() {
        assert!(Instant::now() < deadline, "the job was never done");
        let statuses: Vec<JobStatus> = harness
            .state()
            .controller()
            .jobs()
            .iter()
            .map(|job| job.status)
            .collect();
        assert!(
            !statuses.contains(&JobStatus::ReviewPending),
            "{statuses:?}"
        );
        assert!(
            harness.query_by_label("Confirm all").is_none(),
            "the review is shown"
        );
        assert!(!painted(&harness, "Loading the review"));
        thread::sleep(Duration::from_millis(5));
        harness.step();
    }
    let job = harness.state().controller().jobs()[0].clone();
    assert!(job.config.auto_confirm);
    assert_eq!(files_in(&input.path().join("stacked")).len(), 2);
}

/// A screen of the screenshot example as the harness draws it. What lets its held scan or
/// write go comes first, so it is dropped before the window, whose engine waits for them.
struct Shown {
    _open: Option<screens::OpenOnDrop>,
    app: Box<dyn screens::Screened>,
}

impl eframe::App for Shown {
    fn logic(&mut self, context: &egui::Context, frame: &mut eframe::Frame) {
        self.app.logic(context, frame);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.app.ui(ui, frame);
    }
}

#[test]
fn every_screen_renders_in_both_themes_at_the_smallest_window() {
    // Enough for the panels to settle and egui's animations to end, the harness's frames
    // being a quarter of a second apart.
    const FRAMES: usize = 8;
    let size = egui::vec2(900.0, 560.0);
    // The screens that hold a scan at its workers' first ticks need three cores, so its
    // two files that are not photos are taken before every worker is held.
    let cores = thread::available_parallelism().map_or(1, usize::from);
    for screen in screens::SCREENS {
        if cores < 3 && ["scanning", "quit-question"].contains(&screen.name) {
            println!(
                "skipped: the {} screen needs 3 cores, not {cores}",
                screen.name
            );
            continue;
        }
        let (_data, _prepared, (open, controller), waker) =
            screens::start(screen).unwrap_or_else(|error| panic!("{error}"));
        let mut builder = Harness::builder().with_size(size);
        // Its names are in scripts the app's fonts lack. The window takes them from the
        // system's fonts; the harness looks for none, so its layout is the same on every
        // machine, and draws boxes instead.
        if screen.name == "names" {
            builder = builder.allow_missing_glyphs();
        }
        let mut harness = builder.build_eframe(move |creation| {
            waker.attach(&creation.egui_ctx);
            let app = (screen.setup)(&creation.egui_ctx, controller);
            Shown { _open: open, app }
        });
        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            harness.ctx.set_theme(theme);
            for _ in 0..FRAMES {
                harness.step();
            }
            // Taken as the example takes it: once the inspector has what it asked for.
            let deadline = Instant::now() + TIMEOUT;
            while !harness.state().app.ready() {
                assert!(Instant::now() < deadline, "{}: never ready", screen.name);
                thread::sleep(Duration::from_millis(5));
                harness.step();
            }
            harness.step();
            // A list or a column of cards scrolls what the window's height leaves out, so
            // only the width tells: no widget runs past the window's left or right edge.
            let outside: Vec<String> = harness
                .query_all_by(|node| node.bounding_box().is_some())
                .filter(|node| {
                    let rect = node.rect();
                    rect.left() < -0.5 || rect.right() > size.x + 0.5
                })
                .map(|node| {
                    let accesskit = node.accesskit_node();
                    let name = accesskit.label().or_else(|| accesskit.value());
                    format!("{:?} {name:?} at {:?}", accesskit.role(), node.rect())
                })
                .collect();
            assert!(
                outside.is_empty(),
                "{} ({theme:?}): {outside:?}",
                screen.name
            );
        }
    }
}

#[test]
fn no_frame_waits_for_the_backend() {
    const FRAMES: u64 = 100;
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut harness = harness(&dirs);
    // A build with the visual check turns it on, so the job names a model and its
    // `Similarity` moves too.
    let similarity = cfg!(feature = "ml");
    if similarity {
        click(&mut harness, "Settings");
        click(&mut harness, ML_SWITCH);
        click(&mut harness, CLOSE_SETTINGS);
    }
    start_on(&mut harness, input.path());
    run_until(&mut harness, "the job in review", |app| in_review(app, 1));
    run_until_the_review_is_loaded(&mut harness);
    run_until_idle(&mut harness);
    let job = harness.state().controller().jobs()[0].id;
    let config = &harness.state().controller().job(job).unwrap().config;
    assert_eq!(config.ml_model.is_some(), similarity);
    let b_3 = photo(harness.state(), "b_3.jpg");
    assert_eq!(review(harness.state()).unstacked, [b_3]);

    // The backend held at a gate, as a long engine call holds it. Should a frame wait for
    // it, the gate opens in the end, and the test fails instead of hanging.
    let gate = Arc::new(Gate::default());
    let _open = OpenOnDrop(Arc::clone(&gate));
    let held = Arc::clone(&gate);
    harness.state().controller().send(Request::Run {
        job: None,
        command: Box::new(move |_engine| {
            held.pass();
            Ok(())
        }),
    });
    let watchdog = Arc::clone(&gate);
    thread::spawn(move || {
        thread::sleep(TIMEOUT);
        watchdog.open();
    });

    let passes = harness.ctx.cumulative_pass_nr();
    for frame in 0..FRAMES {
        let last = frame + 1 == FRAMES;
        let hamming = if last { NEAR } else { (frame % 40) as u32 };
        harness
            .get_by_role_and_label(Role::Slider, "Hash distance")
            .set_value_accesskit(f64::from(hamming));
        if similarity {
            let cosine = if last {
                0.7
            } else {
                0.5 + (frame % 40) as f64 / 100.0
            };
            harness
                .get_by_role_and_label(Role::Slider, "Similarity")
                .set_value_accesskit(cosine);
        }
        harness.step();
    }
    assert!(!gate.is_open(), "a frame waited for the backend");
    let drawn = harness.ctx.cumulative_pass_nr() - passes;
    assert!(drawn >= FRAMES, "{drawn} frames");
    // The sliders show the last values at once; the regrouping waits for the backend.
    let slider = harness.get_by_role_and_label(Role::Slider, "Hash distance");
    assert_eq!(
        slider.accesskit_node().numeric_value(),
        Some(f64::from(NEAR))
    );
    let shown = review(harness.state());
    assert_eq!(shown.hamming, NEAR);
    if similarity {
        let cosine = shown.cosine.expect("the similarity moved");
        assert!((cosine - 0.7).abs() < 1e-6, "{cosine}");
    }
    assert!(!harness.state().controller().is_idle(), "nothing waits");
    assert_eq!(shown.unstacked, [b_3]);

    // Served once the gate opens: the last values win.
    gate.open();
    run_until(&mut harness, "the last thresholds stored", |app| {
        app.controller().is_idle()
            && app
                .controller()
                .job(job)
                .is_some_and(|view| view.config.hamming_threshold == NEAR)
            && stack_holding(app, "b_3.jpg").is_some_and(|stack| stack.members.len() == 3)
    });
    let stored = &harness.state().controller().job(job).unwrap().config;
    if similarity {
        let cosine = stored.cosine_threshold.expect("the similarity was stored");
        assert!((cosine - 0.7).abs() < 1e-6, "{cosine}");
    }
}
