//! The controller over a real engine (spec §4.3): every engine call goes through the
//! backend thread, and the engine's events drive what the controller reads.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use common::{
    FakeInstaller, FakePlatform, Gate, HeldEmbedder, NEAR, OpenOnDrop, TIME_WINDOW, bracket_folder,
    controller, data_dir, options, review_folder, runtime_found, runtime_needing_macos, settle,
    wait_for_the_answer, wait_until, write_ambiguous_stack, write_spaced_shots,
};
use image::RgbImage;
use photostacker_core::analyze::Stage;
use photostacker_core::engine::{
    EmbedSource, EngineOptions, GroupView, JobId, ModelActivity, ModelInstall, ModelView,
    MoveTarget,
};
use photostacker_core::model::{
    GroupOrigin, GroupStatus, JobStatus, OutputFormat, RawJpegPreference, RawTone,
};
use photostacker_core::models::{MODELS, ModelSpec};
use photostacker_core::{AppDirs, Engine, JobConfig, Store};
use photostacker_testkit::photos::{write_bracket, write_lone_shots, write_raw_bracket};
use photostacker_ui::backend::{BACKEND_THREAD, Command, Request};
use photostacker_ui::controller::form::Field;
use photostacker_ui::controller::{
    Controller, LibraryProblem, MlState, Notice, NoticeLevel, Question, REVEAL_THREAD, Reveal,
    Review, RuntimeCheck, Selection, StartError, View, WriteCounts,
};
use photostacker_ui::platform::{Picked, Platform};
use photostacker_ui::prefs::{FormDefaults, Panels, Prefs, ThemeChoice};

/// More photos than the analysis has workers (at most 16), so a scan held at its workers'
/// first ticks has photos left to examine.
const MANY: usize = 24;

/// A controller whose scans wait at `gate`.
fn gated_controller(dirs: &AppDirs, gate: &Arc<Gate>) -> Controller {
    let mut options = options(dirs);
    options.wake = gate.holding_the_scan();
    Controller::start(options).unwrap()
}

/// Submits a job on `input`; returns the token of its answer.
fn submit(controller: &Controller, input: &std::path::Path) -> u64 {
    controller.submit(JobConfig::new(input.to_path_buf()))
}

/// Waits until the controller's one job is hashing its photos, and returns it.
fn wait_for_hashing(controller: &mut Controller) -> JobId {
    wait_until(controller, "the scan's hashing", |controller| {
        controller.jobs().first().is_some_and(|job| {
            job.status == JobStatus::Scanning
                && controller
                    .progress(job.id)
                    .is_some_and(|progress| progress.stage == Stage::Hash)
        })
    });
    controller.jobs()[0].id
}

fn status(controller: &Controller, job: JobId) -> Option<JobStatus> {
    controller.job(job).map(|view| view.status)
}

/// A command that says it started, then waits at `gate`: the backend runs nothing else
/// meanwhile.
fn held_command(gate: &Arc<Gate>, entered: &Arc<AtomicBool>) -> Request {
    let gate = Arc::clone(gate);
    let entered = Arc::clone(entered);
    Request::Run {
        job: None,
        command: Box::new(move |_engine| {
            entered.store(true, Ordering::SeqCst);
            gate.pass();
            Ok(())
        }),
    }
}

#[test]
fn a_form_on_its_way_counts_as_a_job_running() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let mut controller = controller(&dirs);
    settle(&mut controller);
    let gate = Arc::new(Gate::default());
    let _open = OpenOnDrop(Arc::clone(&gate));
    let entered = Arc::new(AtomicBool::new(false));
    controller.send(held_command(&gate, &entered));
    wait_until(&mut controller, "the command to start", |_| {
        entered.load(Ordering::SeqCst)
    });

    // Start, then a close at once: the submission waits behind the command, and nothing
    // in the list or the events says a job runs yet.
    controller.open_new_job(Some(input.path().to_path_buf()));
    controller.submit_form();
    controller.poll();
    assert!(controller.jobs().is_empty());
    assert!(controller.is_working());

    gate.open();
    settle(&mut controller);
    assert!(!controller.is_working());
}

#[test]
fn a_job_its_events_say_runs_counts_before_the_list_says_so() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let scan = Arc::new(Gate::default());
    let mut controller = gated_controller(&dirs, &scan);
    let _open_scan = OpenOnDrop(Arc::clone(&scan));
    settle(&mut controller);
    let gate = Arc::new(Gate::default());
    let _open = OpenOnDrop(Arc::clone(&gate));
    let entered = Arc::new(AtomicBool::new(false));

    // The job is made, then the backend is held, so the read of the list that its
    // events ask for waits: only the events say it runs.
    submit(&controller, input.path());
    controller.send(held_command(&gate, &entered));
    wait_until(&mut controller, "the command to start", |_| {
        entered.load(Ordering::SeqCst)
    });
    controller.poll();
    assert!(controller.jobs().is_empty());
    assert!(controller.is_working());

    gate.open();
    wait_until(&mut controller, "the job listed", |controller| {
        controller.jobs().len() == 1
    });
    assert!(controller.is_working());
    scan.open();
    settle(&mut controller);
    assert!(!controller.is_working());
}

#[test]
fn a_slow_command_does_not_block_the_caller() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let mut controller = controller(&dirs);
    settle(&mut controller);
    let gate = Arc::new(Gate::default());
    let _open = OpenOnDrop(Arc::clone(&gate));
    let entered = Arc::new(AtomicBool::new(false));
    let command: Command = {
        let gate = Arc::clone(&gate);
        let entered = Arc::clone(&entered);
        Box::new(move |_engine| {
            entered.store(true, Ordering::SeqCst);
            gate.pass();
            Ok(())
        })
    };

    // Sending and polling return while the command waits at its gate, which only this
    // thread opens: a send or a poll that waited for the engine would never return.
    controller.send(Request::Run { job: None, command });
    let token = submit(&controller, input.path());
    controller.poll();
    wait_until(&mut controller, "the command to start", |_| {
        entered.load(Ordering::SeqCst)
    });

    // While the command waits, the submit waits behind it, and every poll returns.
    for _ in 0..30 {
        controller.poll();
        assert!(controller.jobs().is_empty());
        assert!(controller.take_submitted().is_empty());
        assert!(!controller.is_idle());
        thread::sleep(Duration::from_millis(10));
    }

    gate.open();
    wait_until(&mut controller, "the submitted job", |controller| {
        controller.jobs().len() == 1
    });
    let job = controller.jobs()[0].id;
    let submitted = controller.take_submitted();
    assert!(
        matches!(submitted.as_slice(), [(answered, Ok(id))] if *id == job && *answered == token),
        "{submitted:?}"
    );
}

#[test]
fn a_submitted_job_appears_and_reaches_review() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let mut controller = controller(&dirs);
    let token = submit(&controller, input.path());
    wait_until(&mut controller, "the submitted job", |controller| {
        !controller.jobs().is_empty()
    });
    settle(&mut controller);

    let submitted = controller.take_submitted();
    let [(answered, Ok(job))] = submitted.as_slice() else {
        panic!("{submitted:?}");
    };
    assert_eq!(*answered, token);
    let view = controller.job(*job).unwrap();
    assert_eq!(view.status, JobStatus::ReviewPending);
    assert_eq!(view.config.input_dir, input.path());
    assert_eq!((view.counts.files, view.counts.proposed), (8, 2));
    assert!(controller.take_notices().is_empty());
}

#[test]
fn progress_follows_the_scan_and_is_dropped_after_it() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let gate = Arc::new(Gate::default());
    let mut controller = gated_controller(&dirs, &gate);
    let _open = OpenOnDrop(Arc::clone(&gate));
    submit(&controller, input.path());

    let job = wait_for_hashing(&mut controller);
    let progress = controller.progress(job).unwrap();
    assert_eq!(progress.total, 8);
    assert!(progress.done >= 1);
    assert!(controller.is_working());

    gate.open();
    settle(&mut controller);
    assert_eq!(status(&controller, job), Some(JobStatus::ReviewPending));
    assert!(controller.progress(job).is_none());
    assert!(controller.scan_errors(job).is_empty());
}

#[test]
fn cancel_retry_and_delete_reach_the_engine() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let gate = Arc::new(Gate::default());
    let mut controller = gated_controller(&dirs, &gate);
    let _open = OpenOnDrop(Arc::clone(&gate));
    submit(&controller, input.path());
    let job = wait_for_hashing(&mut controller);

    // The cancel must reach the engine before the scan is let go: a cancel that comes
    // once the analysis is saved is too late, and the job goes to review. A cancel of a
    // Scanning job only flags its task, so the backend gets through it with the gate shut.
    controller.cancel(job);
    wait_until(&mut controller, "the cancel to run", Controller::is_idle);
    gate.open();
    settle(&mut controller);
    assert_eq!(status(&controller, job), Some(JobStatus::Cancelled));
    assert!(controller.progress(job).is_none());

    controller.retry(job);
    settle(&mut controller);
    assert_eq!(status(&controller, job), Some(JobStatus::ReviewPending));

    controller.select_job(job);
    assert_eq!(controller.selected().map(|view| view.id), Some(job));
    assert_eq!(controller.view(), &View::Job);
    controller.delete(job);
    settle(&mut controller);
    assert!(controller.jobs().is_empty());
    assert!(controller.selected().is_none());
    assert!(controller.take_notices().is_empty());
}

#[test]
fn a_job_being_deleted_shows_it_and_takes_no_command() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let mut controller = controller(&dirs);
    let job = reviewable_job(&mut controller, JobConfig::new(input.path().to_path_buf()));
    assert!(!controller.is_deleting(job));
    // The delete waits behind a held command, as behind a large job's files.
    let gate = Arc::new(Gate::default());
    let _open = OpenOnDrop(Arc::clone(&gate));
    let entered = Arc::new(AtomicBool::new(false));
    controller.send(held_command(&gate, &entered));
    wait_until(&mut controller, "the command to start", |_| {
        entered.load(Ordering::SeqCst)
    });

    controller.delete(job);
    assert!(controller.is_deleting(job));
    // A second delete, or any command on it, sends nothing: once the first has run,
    // each would be refused, the job being gone.
    controller.delete(job);
    controller.retry(job);
    controller.cancel(job);
    controller.poll();
    assert!(controller.is_deleting(job));

    gate.open();
    settle(&mut controller);
    assert!(controller.jobs().is_empty());
    assert!(!controller.is_deleting(job));
    assert_eq!(controller.take_notices(), []);
}

#[test]
fn a_failed_command_is_a_notice() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let mut controller = controller(&dirs);
    submit(&controller, input.path());
    wait_until(&mut controller, "the submitted job", |controller| {
        !controller.jobs().is_empty()
    });
    settle(&mut controller);
    let job = controller.jobs()[0].id;
    assert!(controller.take_notices().is_empty());

    // A job in review has nothing to retry.
    controller.retry(job);
    settle(&mut controller);
    let notices = controller.take_notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].level, NoticeLevel::Error);
    assert_eq!(
        notices[0].text,
        "That job is in review now, so this no longer applies."
    );
    assert!(controller.take_notices().is_empty());
}

#[test]
fn a_job_left_scanning_resumes_when_the_app_starts_again() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    write_lone_shots(input.path(), MANY);
    let gate = Arc::new(Gate::default());
    let mut first = gated_controller(&dirs, &gate);
    submit(&first, input.path());
    let job = wait_for_hashing(&mut first);

    // The engine's shutdown waits for the held workers, which the gate lets go well after
    // the quit flag is set.
    let opener = {
        let gate = Arc::clone(&gate);
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(2));
            gate.open();
        })
    };
    drop(first);
    opener.join().unwrap();
    {
        let store = Store::open_read_only(&dirs.db()).unwrap();
        let stored = store.job(job).unwrap().unwrap();
        assert_eq!(
            (stored.status, stored.scan_done),
            (JobStatus::Scanning, false)
        );
    }

    let mut second = controller(&dirs);
    wait_until(&mut second, "the job left scanning", |controller| {
        controller.job(job).is_some()
    });
    settle(&mut second);
    let view = second.job(job).unwrap();
    assert_eq!(view.status, JobStatus::ReviewPending);
    assert_eq!((view.counts.files, view.counts.errors), (MANY, 0));
}

#[test]
fn a_question_is_answered_once() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let mut controller = controller(&dirs);
    submit(&controller, input.path());
    wait_until(&mut controller, "the submitted job", |controller| {
        !controller.jobs().is_empty()
    });
    settle(&mut controller);
    let job = controller.jobs()[0].id;

    controller.ask(Question::DeleteJob(job));
    assert_eq!(controller.question(), Some(&Question::DeleteJob(job)));
    controller.answer(false);
    assert_eq!(controller.question(), None);
    settle(&mut controller);
    assert!(controller.job(job).is_some());

    controller.ask(Question::DeleteJob(job));
    controller.answer(true);
    assert_eq!(controller.question(), None);
    settle(&mut controller);
    assert!(controller.jobs().is_empty());

    // Answered already: a second delete would fail with a notice.
    controller.answer(true);
    settle(&mut controller);
    assert!(controller.take_notices().is_empty());
}

#[test]
fn a_second_controller_on_the_same_data_is_already_running() {
    let (_data, dirs) = data_dir();
    let _first = controller(&dirs);
    match Controller::start(options(&dirs)) {
        Err(StartError::AlreadyRunning(db)) => assert_eq!(db, dirs.db()),
        Err(other) => panic!("{other:?}"),
        Ok(_) => panic!("a second controller started on the same data"),
    }
}

#[test]
fn preferences_load_with_defaults_and_survive_one_bad_value() {
    let (_data, dirs) = data_dir();
    assert_eq!(controller(&dirs).prefs, Prefs::default());

    let panels = Panels {
        jobs: 300.0,
        inspector: 280.0,
        tray: Some(200.0),
    };
    let defaults = FormDefaults {
        output_format: OutputFormat::Tiff,
        hamming_threshold: 12,
        ..FormDefaults::default()
    };
    {
        let engine = Engine::start(EngineOptions {
            db: dirs.db(),
            thumbs_dir: dirs.thumbs(),
            models_dir: dirs.models(),
            embedder: None,
            installer: None,
            wake: Arc::new(|| {}),
        })
        .unwrap();
        engine.set_pref("theme", &7).unwrap();
        engine.set_pref("window", &[1000.0_f32, 700.0]).unwrap();
        engine.set_pref("panels", &panels).unwrap();
        engine.set_pref("form_defaults", &defaults).unwrap();
        engine.set_pref("ml_enabled", &true).unwrap();
        engine.set_pref("ml_model", "dinov2-base").unwrap();
        engine.set_pref("last_input_dir", "/photos/trip").unwrap();
    }

    let controller = controller(&dirs);
    assert_eq!(
        controller.prefs,
        Prefs {
            theme: ThemeChoice::System,
            window: Some([1000.0, 700.0]),
            panels,
            defaults,
            ml_enabled: true,
            ml_model: "dinov2-base".into(),
            last_input_dir: Some("/photos/trip".into()),
        }
    );
}

/// A platform whose reveal records each path with the thread that showed it, fails for a
/// folder named `refused`, and, given a gate, waits at it first, as a file manager that
/// does not answer would.
#[derive(Default)]
struct RecordingPlatform {
    revealed: Mutex<Vec<(PathBuf, Option<String>)>>,
    gate: Option<Arc<Gate>>,
}

impl Platform for RecordingPlatform {
    fn pick_folder(&self, _start: Option<&Path>) -> Picked {
        Picked::Cancelled
    }

    fn pick_file(&self, _start: Option<&Path>) -> Picked {
        Picked::Cancelled
    }

    fn reveal(&self, path: &Path) -> Result<(), String> {
        let thread = thread::current().name().map(str::to_owned);
        self.revealed
            .lock()
            .unwrap()
            .push((path.to_path_buf(), thread));
        if let Some(gate) = &self.gate {
            gate.pass();
        }
        if path.ends_with("refused") {
            Err("the file manager did not answer".into())
        } else {
            Ok(())
        }
    }
}

/// A controller on `dirs` whose platform is `platform`.
fn revealing_controller(dirs: &AppDirs, platform: &Arc<RecordingPlatform>) -> Controller {
    let mut options = options(dirs);
    options.platform = Arc::clone(platform) as Arc<dyn Platform>;
    Controller::start(options).unwrap()
}

#[test]
fn reveal_runs_on_a_thread_of_its_own_and_says_what_is_missing() {
    let (_data, dirs) = data_dir();
    let folders = tempfile::tempdir().unwrap();
    let shown = folders.path().join("shown");
    let refused = folders.path().join("refused");
    for folder in [&shown, &refused] {
        std::fs::create_dir(folder).unwrap();
    }
    let unwritten = folders.path().join("stacked");
    let gone = folders.path().join("IMG_0001.jpg");
    let platform = Arc::new(RecordingPlatform::default());
    let mut controller = revealing_controller(&dirs, &platform);

    controller.reveal(&shown, Reveal::OutputFolder);
    controller.reveal(&refused, Reveal::OutputFolder);
    controller.reveal(&unwritten, Reveal::OutputFolder);
    controller.reveal(&gone, Reveal::File);
    let mut notices = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    while notices.len() < 3 || platform.revealed.lock().unwrap().len() < 2 {
        assert!(Instant::now() < deadline, "only {notices:?}");
        thread::sleep(Duration::from_millis(5));
        controller.poll();
        notices.extend(controller.take_notices());
    }

    // The platform is asked only for what exists, never on the caller's thread.
    let mut revealed = platform.revealed.lock().unwrap().clone();
    revealed.sort();
    let thread = Some(REVEAL_THREAD.to_owned());
    assert_eq!(
        revealed,
        [(refused.clone(), thread.clone()), (shown, thread)]
    );
    let mut notices: Vec<(NoticeLevel, String)> = notices
        .into_iter()
        .map(|notice| (notice.level, notice.text))
        .collect();
    notices.sort_by(|a, b| a.1.cmp(&b.1));
    let mut expected = vec![
        (
            NoticeLevel::Error,
            format!(
                "Cannot show {}: the file manager did not answer",
                refused.display()
            ),
        ),
        (
            NoticeLevel::Info,
            format!(
                "Nothing is written yet: the stacks will go to {}.",
                unwritten.display()
            ),
        ),
        (
            NoticeLevel::Warning,
            format!("{} is no longer there.", gone.display()),
        ),
    ];
    expected.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(notices, expected);
}

#[test]
fn a_logs_folder_never_made_does_not_exist() {
    let (_data, dirs) = data_dir();
    let platform = Arc::new(RecordingPlatform::default());
    let mut controller = revealing_controller(&dirs, &platform);
    assert!(!dirs.logs().exists());

    controller.open_logs();
    let mut notices = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    while notices.is_empty() {
        assert!(Instant::now() < deadline, "no notice");
        thread::sleep(Duration::from_millis(5));
        controller.poll();
        notices.extend(controller.take_notices());
    }

    // A folder, not a file that was there.
    assert_eq!(
        notices,
        [Notice {
            level: NoticeLevel::Warning,
            text: format!("{} does not exist.", dirs.logs().display()),
        }]
    );
    assert!(platform.revealed.lock().unwrap().is_empty());
}

#[test]
fn a_reveal_that_blocks_does_not_delay_the_engine() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let gate = Arc::new(Gate::default());
    let platform = Arc::new(RecordingPlatform {
        gate: Some(Arc::clone(&gate)),
        ..RecordingPlatform::default()
    });
    let mut controller = revealing_controller(&dirs, &platform);
    let _open = OpenOnDrop(Arc::clone(&gate));

    controller.reveal(input.path(), Reveal::OutputFolder);
    wait_until(&mut controller, "the file manager to be asked", |_| {
        !platform.revealed.lock().unwrap().is_empty()
    });
    // The file manager never answers; the job is submitted, read and scanned all the same.
    submit(&controller, input.path());
    wait_until(&mut controller, "the job in review", |controller| {
        controller
            .jobs()
            .first()
            .is_some_and(|job| job.status == JobStatus::ReviewPending)
    });
}

#[test]
fn the_jobs_are_loaded_once_first_read_even_when_there_are_none() {
    let (_data, dirs) = data_dir();
    let mut controller = controller(&dirs);
    // Nothing is taken in before the first poll.
    assert!(!controller.jobs_loaded());
    wait_until(
        &mut controller,
        "the jobs to be read",
        Controller::jobs_loaded,
    );
    assert!(controller.jobs().is_empty());
}

#[test]
fn a_theme_is_kept_at_once_and_saved() {
    let (_data, dirs) = data_dir();
    let mut controller = controller(&dirs);
    controller.set_theme(ThemeChoice::Dark);
    assert_eq!(controller.prefs.theme, ThemeChoice::Dark);
    settle(&mut controller);
    assert!(controller.take_notices().is_empty());
    drop(controller);

    assert_eq!(common::controller(&dirs).prefs.theme, ThemeChoice::Dark);
}

/// New jobs' defaults unlike the built-in ones in every field Settings shows.
fn changed_defaults() -> FormDefaults {
    FormDefaults {
        output_format: OutputFormat::Tiff,
        recursive: true,
        auto_confirm: true,
        hamming_threshold: 12,
        time_window_seconds: 9,
        stage3_ambiguity_floor: 3,
        raw_tone: RawTone::Linear,
        raw_jpeg_preference: RawJpegPreference::Jpeg,
        ..FormDefaults::default()
    }
}

#[test]
fn settings_are_saved_and_come_back_after_a_restart() {
    let (_data, dirs) = data_dir();
    let mut controller = controller(&dirs);
    let panels = Panels {
        jobs: 300.0,
        inspector: 280.0,
        tray: Some(200.0),
    };

    controller.set_theme(ThemeChoice::Light);
    controller.set_defaults(changed_defaults());
    controller.set_window([1000.0, 700.0]);
    controller.set_panels(panels);
    // Each is kept at once.
    let expected = Prefs {
        theme: ThemeChoice::Light,
        window: Some([1000.0, 700.0]),
        panels,
        defaults: changed_defaults(),
        ..Prefs::default()
    };
    assert_eq!(controller.prefs, expected);
    settle(&mut controller);
    assert!(controller.take_notices().is_empty());
    drop(controller);

    assert_eq!(common::controller(&dirs).prefs, expected);
}

#[test]
fn a_waiting_preference_save_is_kept_when_the_app_closes() {
    let (_data, dirs) = data_dir();
    let mut controller = controller(&dirs);
    settle(&mut controller);
    let gate = Arc::new(Gate::default());
    hold_the_backend(&mut controller, &gate);
    // Waiting behind the held command, each a newer save of its key than the one before.
    controller.set_window([1000.0, 700.0]);
    controller.set_window([1100.0, 750.0]);
    controller.set_theme(ThemeChoice::Dark);
    controller.send(Request::Jobs);
    // The window closes meanwhile: the held command ends a moment later.
    let opener = {
        let gate = Arc::clone(&gate);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            gate.open();
        })
    };
    drop(controller);
    opener.join().unwrap();

    let prefs = common::controller(&dirs).prefs;
    assert_eq!(prefs.window, Some([1100.0, 750.0]));
    assert_eq!(prefs.theme, ThemeChoice::Dark);
}

#[test]
fn a_new_job_form_starts_from_the_changed_defaults() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    let mut controller = controller(&dirs);

    // A form already open keeps what it holds.
    controller.open_new_job(Some(input.path().to_path_buf()));
    controller.set_defaults(changed_defaults());
    assert_eq!(controller.form().unwrap().fields, FormDefaults::default());

    // The next one starts from the new defaults, and its job has them.
    controller.close_form();
    controller.open_new_job(Some(input.path().to_path_buf()));
    assert_eq!(controller.form().unwrap().fields, changed_defaults());
    let model = controller.job_model();
    let config = controller.form().unwrap().config(model);
    assert_eq!(
        (config.output_format, config.recursive, config.auto_confirm),
        (OutputFormat::Tiff, true, true)
    );
    assert_eq!(
        (
            config.hamming_threshold,
            config.time_window_seconds,
            config.stage3_ambiguity_floor
        ),
        (12, 9, 3)
    );
    assert_eq!(
        (config.raw_tone, config.raw_jpeg_preference),
        (RawTone::Linear, RawJpegPreference::Jpeg)
    );
}

#[test]
fn settings_go_back_to_the_view_they_were_opened_over() {
    let (_data, dirs) = data_dir();
    let mut controller = controller(&dirs);

    controller.open_settings();
    assert_eq!(controller.view(), &View::Settings);
    controller.close_settings();
    assert_eq!(controller.view(), &View::Job);

    // Over the form, which keeps what was typed.
    controller.open_new_job(None);
    controller.form().unwrap().set_input("/photos/trip");
    controller.open_settings();
    controller.open_settings();
    assert_eq!(controller.view(), &View::Settings);
    controller.close_settings();
    assert_eq!(controller.view(), &View::NewJob);
    assert_eq!(controller.form().unwrap().input, "/photos/trip");

    // New job leaves Settings for the form.
    controller.open_settings();
    controller.open_new_job(None);
    assert_eq!(controller.view(), &View::NewJob);
}

#[test]
fn a_job_made_while_settings_are_open_is_selected_behind_them() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let mut controller = controller(&dirs);

    controller.open_new_job(Some(input.path().to_path_buf()));
    controller.submit_form();
    controller.open_settings();
    wait_until(&mut controller, "the job selected", |controller| {
        controller.selected().is_some()
    });
    // Settings stay; the form is gone, so Done shows the new job.
    assert_eq!(controller.view(), &View::Settings);
    assert!(controller.form().is_none());
    controller.close_settings();
    assert_eq!(controller.view(), &View::Job);
    settle(&mut controller);
}

/// What `path` reads as in a field of the form.
fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The text of the default output of `input`: its `stacked` subfolder.
fn stacked(input: &Path) -> String {
    text(&input.join("stacked"))
}

/// A controller on `dirs` whose folder picker answers `folders`, in order, then nothing.
fn picking_controller(dirs: &AppDirs, folders: &[&Path]) -> (Controller, Arc<FakePlatform>) {
    let platform = Arc::new(FakePlatform::default());
    platform
        .folders
        .lock()
        .unwrap()
        .extend(folders.iter().map(|folder| folder.to_path_buf()));
    let mut options = options(dirs);
    options.platform = Arc::clone(&platform) as Arc<dyn Platform>;
    (Controller::start(options).unwrap(), platform)
}

#[test]
fn a_desktop_without_file_dialogs_says_so_for_the_folders() {
    let (_data, dirs) = data_dir();
    let photos = tempfile::tempdir().unwrap();
    let (mut controller, platform) = picking_controller(&dirs, &[photos.path()]);
    platform.no_dialogs.store(true, Ordering::SeqCst);
    controller.open_new_job(None);
    controller.form().unwrap().set_input("/photos/trip");
    let before = controller.form().unwrap().clone();

    for browse in [Controller::browse_input, Controller::browse_output] {
        browse(&mut controller);
        assert_eq!(
            controller.take_notices(),
            [Notice {
                level: NoticeLevel::Warning,
                text:
                    "No file dialog is available on this desktop. Type the folder's path instead."
                        .into(),
            }]
        );
        // Nothing else changes.
        assert_eq!(controller.form().unwrap(), &before);
    }
    assert_eq!(platform.folders.lock().unwrap().len(), 1);

    // A dialog closed without a choice says nothing.
    platform.no_dialogs.store(false, Ordering::SeqCst);
    platform.folders.lock().unwrap().clear();
    controller.browse_input();
    assert!(controller.take_notices().is_empty());
    assert_eq!(controller.form().unwrap(), &before);
}

#[test]
fn dropping_a_file_uses_its_folder() {
    let (_data, dirs) = data_dir();
    let folder = tempfile::tempdir().unwrap();
    let photo = folder.path().join("IMG_0001.jpg");
    std::fs::write(&photo, b"not a photo").unwrap();
    let mut controller = controller(&dirs);

    controller.dropped(std::slice::from_ref(&photo));
    assert_eq!(controller.view(), &View::NewJob);
    let form = controller.form().unwrap();
    assert_eq!(form.input, text(folder.path()));
    assert_eq!(form.output, stacked(folder.path()));

    // On an open form, a drop replaces the folder and keeps the rest of what was chosen.
    form.fields.recursive = true;
    let other = tempfile::tempdir().unwrap();
    controller.dropped(&[other.path().to_path_buf()]);
    let form = controller.form().unwrap();
    assert_eq!(form.input, text(other.path()));
    assert!(form.fields.recursive);
    assert!(controller.take_notices().is_empty());
}

#[test]
fn dropping_two_folders_uses_the_first_and_says_so() {
    let (_data, dirs) = data_dir();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let photo = first.path().join("IMG_0001.jpg");
    std::fs::write(&photo, b"not a photo").unwrap();
    let mut controller = controller(&dirs);

    // A photo of the first folder is no second folder.
    controller.dropped(&[first.path().to_path_buf(), photo]);
    assert!(controller.take_notices().is_empty());
    controller.close_form();

    controller.dropped(&[first.path().to_path_buf(), second.path().to_path_buf()]);
    assert_eq!(controller.form().unwrap().input, text(first.path()));
    assert_eq!(
        controller.take_notices(),
        [Notice {
            level: NoticeLevel::Info,
            text: "Only the first folder was used: one job scans one folder.".into(),
        }]
    );
}

#[test]
fn dropping_nothing_usable_is_a_warning() {
    let (_data, dirs) = data_dir();
    let root = tempfile::tempdir().unwrap();
    let mut controller = controller(&dirs);

    controller.dropped(&[root.path().join("gone")]);
    assert!(controller.form().is_none());
    assert_ne!(controller.view(), &View::NewJob);
    let notices = controller.take_notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].level, NoticeLevel::Warning);
}

#[test]
fn browse_fills_the_input_and_the_output() {
    let (_data, dirs) = data_dir();
    let photos = tempfile::tempdir().unwrap();
    let stacks = tempfile::tempdir().unwrap();
    let (mut controller, platform) = picking_controller(&dirs, &[photos.path(), stacks.path()]);
    controller.prefs.last_input_dir = Some("/photos".into());

    // Without a form there is nothing to fill: no dialog opens.
    controller.browse_input();
    assert!(platform.folder_starts.lock().unwrap().is_empty());

    controller.open_new_job(None);
    controller.browse_input();
    let form = controller.form().unwrap();
    assert_eq!(form.input, text(photos.path()));
    assert_eq!(form.output, stacked(photos.path()));
    assert!(!form.output_edited);

    controller.browse_output();
    let form = controller.form().unwrap();
    assert_eq!(form.output, text(stacks.path()));
    assert!(form.output_edited);

    // A cancelled dialog changes nothing.
    controller.browse_input();
    let form = controller.form().unwrap();
    assert_eq!(form.input, text(photos.path()));
    assert_eq!(form.output, text(stacks.path()));

    // Each dialog starts in its field's folder, or in the last folder used.
    assert_eq!(
        *platform.folder_starts.lock().unwrap(),
        [
            Some(PathBuf::from("/photos")),
            Some(photos.path().join("stacked")),
            Some(photos.path().to_path_buf()),
        ]
    );
}

#[test]
fn a_missing_input_folder_is_an_inline_error() {
    let (_data, dirs) = data_dir();
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("missing");
    let mut controller = controller(&dirs);

    controller.open_new_job(Some(missing.clone()));
    controller.submit_form();
    assert!(controller.form().unwrap().submitting);
    wait_for_the_answer(&mut controller);

    assert_eq!(controller.view(), &View::NewJob);
    let form = controller.form().unwrap();
    assert_eq!(
        form.error,
        Some((
            Field::Input,
            format!("This is not a folder: {}", missing.display())
        ))
    );
    settle(&mut controller);
    assert!(controller.jobs().is_empty());
    assert!(controller.take_notices().is_empty());
}

#[test]
fn an_output_equal_to_the_input_is_an_inline_error() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let mut controller = controller(&dirs);

    controller.open_new_job(Some(input.path().to_path_buf()));
    controller.form().unwrap().set_output(&text(input.path()));
    controller.submit_form();
    wait_for_the_answer(&mut controller);

    let form = controller.form().unwrap();
    assert_eq!(
        form.error,
        Some((
            Field::Output,
            "The output folder must not be the photo folder.".into()
        ))
    );
    // Submitted again, unchanged, it is refused again.
    controller.submit_form();
    wait_for_the_answer(&mut controller);
    assert_eq!(
        controller
            .form()
            .unwrap()
            .error
            .as_ref()
            .map(|error| error.0),
        Some(Field::Output)
    );
}

#[test]
fn a_submitted_form_closes_and_selects_the_job() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let mut controller = controller(&dirs);

    controller.open_new_job(Some(input.path().to_path_buf()));
    controller.submit_form();
    // A second Start while the first is on its way sends nothing.
    controller.submit_form();
    wait_until(&mut controller, "the job selected", |controller| {
        controller.selected().is_some()
    });
    assert!(controller.form().is_none());
    assert_eq!(controller.view(), &View::Job);
    assert_eq!(
        controller.selected().unwrap().config.input_dir,
        input.path()
    );
    assert_eq!(
        controller.prefs.last_input_dir.as_deref(),
        input.path().to_str()
    );
    settle(&mut controller);
    assert_eq!(controller.jobs().len(), 1);
    // The form took its answer: none is left for whoever sends a Submit of their own.
    assert!(controller.take_submitted().is_empty());
    assert!(controller.take_notices().is_empty());
    drop(controller);

    assert_eq!(
        common::controller(&dirs).prefs.last_input_dir.as_deref(),
        input.path().to_str()
    );
}

#[test]
fn an_answer_after_the_form_closed_selects_the_job_or_is_dropped() {
    let (_data, dirs) = data_dir();
    let root = tempfile::tempdir().unwrap();
    let input = bracket_folder();
    let mut controller = controller(&dirs);

    // A refusal has no field left to show under.
    controller.open_new_job(Some(root.path().join("missing")));
    controller.submit_form();
    controller.close_form();
    settle(&mut controller);
    assert!(controller.form().is_none());
    assert!(controller.take_notices().is_empty());

    // The job exists all the same: it is selected.
    controller.open_new_job(Some(input.path().to_path_buf()));
    controller.submit_form();
    controller.close_form();
    wait_until(&mut controller, "the job selected", |controller| {
        controller.selected().is_some()
    });
    assert_eq!(controller.view(), &View::Job);
    assert!(controller.form().is_none());
}

#[test]
fn a_submit_that_panics_unlocks_the_form_and_the_next_one_works() {
    photostacker_core::guard::install_panic_hook();
    let (_data, dirs) = data_dir();
    let held = review_folder();
    let first = bracket_folder();
    let second = bracket_folder();
    // Armed, the wake panics once on the backend thread. It must be the wake that
    // `Engine::submit` runs for the job it queued, under the request's guard.
    let armed = Arc::new(AtomicBool::new(false));
    let embedding = Arc::new(Gate::default());
    let embedder = Arc::new(HeldEmbedder {
        gate: Arc::clone(&embedding),
        called: AtomicBool::new(false),
    });
    let mut options = options(&dirs);
    options.embedder = Some(Arc::clone(&embedder) as Arc<dyn EmbedSource>);
    options.wake = {
        let armed = Arc::clone(&armed);
        Arc::new(move || {
            let on_the_backend = thread::current().name() == Some(BACKEND_THREAD);
            if on_the_backend && armed.swap(false, Ordering::SeqCst) {
                panic!("the wake failed");
            }
        })
    };
    let mut controller = Controller::start(options).unwrap();
    let _open = OpenOnDrop(Arc::clone(&embedding));

    // `Events::flush` runs every wake that is due, on whichever thread calls it, so an
    // engine thread that flushes between the submit's post and its flush would take the
    // submit's wake, and the armed panic would hit a later wake of the backend, outside
    // any guard. So no engine thread may flush while armed: an earlier job's scan is held
    // in its visual check, where the job worker waits in the embedder, outside any flush,
    // and the analysis threads are done; the decoder sends no event. The backend is then
    // the only thread that flushes.
    let mut config = JobConfig::new(held.path().to_path_buf());
    config.ml_model = Some("dinov2-small".into());
    config.hamming_threshold = NEAR;
    controller.submit(config);
    wait_until(&mut controller, "the visual check held", |controller| {
        controller.is_idle() && embedder.called.load(Ordering::SeqCst)
    });
    // The backend wakes the window after a reply is stored and its request marked done,
    // so a reply already taken can still have its wake to come. A command with no reply
    // runs only after that wake, and has none of its own.
    let barrier = Arc::new(AtomicBool::new(false));
    let command: Command = {
        let barrier = Arc::clone(&barrier);
        Box::new(move |_engine| {
            barrier.store(true, Ordering::SeqCst);
            Ok(())
        })
    };
    controller.send(Request::Run { job: None, command });
    wait_until(&mut controller, "the backend's last wake", |controller| {
        barrier.load(Ordering::SeqCst) && controller.is_idle()
    });

    armed.store(true, Ordering::SeqCst);
    controller.open_new_job(Some(first.path().to_path_buf()));
    controller.submit_form();
    wait_for_the_answer(&mut controller);
    assert!(!armed.load(Ordering::SeqCst), "the submit never panicked");
    assert_eq!(controller.form().unwrap().error, None);
    let notices = controller.take_notices();
    assert!(
        matches!(
            notices.as_slice(),
            [Notice { level: NoticeLevel::Error, text }] if text == "Internal error: the wake failed."
        ),
        "{notices:?}"
    );

    // The next submission is answered as its own.
    controller.form().unwrap().set_input(&text(second.path()));
    controller.submit_form();
    wait_until(&mut controller, "the second job selected", |controller| {
        controller.selected().is_some()
    });
    assert!(controller.form().is_none());
    assert_eq!(
        controller.selected().unwrap().config.input_dir,
        second.path()
    );
    assert_eq!(
        controller.prefs.last_input_dir.as_deref(),
        second.path().to_str()
    );
}

#[test]
fn answers_out_of_the_forms_sight_land_on_their_own_form_or_job() {
    let (_data, dirs) = data_dir();
    let closed = bracket_folder();
    let fixed = bracket_folder();
    let others = bracket_folder();
    let root = tempfile::tempdir().unwrap();
    let mut controller = controller(&dirs);

    // A job submitted by other means, whose answer is for whoever sent it.
    let own = submit(&controller, others.path());
    controller.open_new_job(Some(closed.path().to_path_buf()));
    controller.submit_form();
    controller.close_form();
    controller.open_new_job(Some(root.path().join("missing")));
    controller.submit_form();
    wait_for_the_answer(&mut controller);
    settle(&mut controller);

    // The newer form has its own refusal, and the closed form's job does not replace it.
    assert_eq!(controller.view(), &View::NewJob);
    assert_eq!(
        controller
            .form()
            .unwrap()
            .error
            .as_ref()
            .map(|error| error.0),
        Some(Field::Input)
    );
    assert!(controller.selected().is_none());
    assert_eq!(
        controller.prefs.last_input_dir.as_deref(),
        closed.path().to_str()
    );
    let submitted = controller.take_submitted();
    assert!(
        matches!(submitted.as_slice(), [(token, Ok(_))] if *token == own),
        "{submitted:?}"
    );

    // Fixed, the newer form makes the job that is selected.
    controller.form().unwrap().set_input(&text(fixed.path()));
    controller.submit_form();
    wait_until(&mut controller, "the fixed job selected", |controller| {
        controller.selected().is_some()
    });
    assert_eq!(
        controller.selected().unwrap().config.input_dir,
        fixed.path()
    );
    settle(&mut controller);
    assert_eq!(controller.jobs().len(), 3);
    assert!(controller.take_submitted().is_empty());
}

/// A folder in `root` whose name is not valid UTF-8. Not on macOS, whose file system
/// (APFS) refuses to create such a name: the tests that need one are left out there.
#[cfg(all(unix, not(target_os = "macos")))]
fn not_unicode_folder(root: &Path) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    let folder = root.join(std::ffi::OsStr::from_bytes(b"caf\xe9"));
    std::fs::create_dir(&folder).unwrap();
    folder
}

/// The warning about `folder`, whose name is not valid Unicode.
#[cfg(all(unix, not(target_os = "macos")))]
fn not_unicode_warning(folder: &Path) -> Notice {
    Notice {
        level: NoticeLevel::Warning,
        text: format!(
            "{}: Photostacker can only use folders whose names are valid Unicode.",
            folder.display()
        ),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn dropping_a_folder_whose_name_is_not_unicode_is_refused_plainly() {
    let (_data, dirs) = data_dir();
    let root = tempfile::tempdir().unwrap();
    let folder = not_unicode_folder(root.path());
    let photo = folder.join("IMG_0001.jpg");
    std::fs::write(&photo, b"not a photo").unwrap();
    let mut controller = controller(&dirs);

    // With no form open, none opens.
    controller.dropped(std::slice::from_ref(&folder));
    assert!(controller.form().is_none());
    assert_ne!(controller.view(), &View::NewJob);
    assert_eq!(controller.take_notices(), [not_unicode_warning(&folder)]);

    // An open form is left as it was, for a photo in such a folder too.
    let photos = tempfile::tempdir().unwrap();
    controller.open_new_job(Some(photos.path().to_path_buf()));
    let before = controller.form().unwrap().clone();
    controller.dropped(&[photo]);
    assert_eq!(controller.form().unwrap(), &before);
    assert_eq!(controller.take_notices(), [not_unicode_warning(&folder)]);
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn browsing_to_a_folder_whose_name_is_not_unicode_is_refused_plainly() {
    let (_data, dirs) = data_dir();
    let root = tempfile::tempdir().unwrap();
    let folder = not_unicode_folder(root.path());
    let photos = tempfile::tempdir().unwrap();
    let (mut controller, _platform) = picking_controller(&dirs, &[&folder, &folder]);

    controller.open_new_job(Some(photos.path().to_path_buf()));
    let before = controller.form().unwrap().clone();
    // Each refused with its warning (taken apart: equal notices taken together are one).
    let warning = not_unicode_warning(&folder);
    controller.browse_input();
    assert_eq!(controller.take_notices(), std::slice::from_ref(&warning));
    controller.browse_output();
    assert_eq!(controller.take_notices(), [warning]);
    assert_eq!(controller.form().unwrap(), &before);
}

// --- The review ---

/// Submits `config` and waits until its job is in review, not selected; returns the job.
fn reviewable_job(controller: &mut Controller, config: JobConfig) -> JobId {
    let token = controller.submit(config);
    settle(controller);
    let submitted = controller.take_submitted();
    let job = submitted
        .iter()
        .find_map(|(answered, result)| (*answered == token).then_some(result))
        .and_then(|result| result.as_ref().ok().copied())
        .unwrap_or_else(|| panic!("{submitted:?}"));
    assert_eq!(status(controller, job), Some(JobStatus::ReviewPending));
    job
}

/// Selects `job` and waits until its review is loaded.
fn open(controller: &mut Controller, job: JobId) {
    controller.select_job(job);
    wait_until(controller, "the review to load", |controller| {
        controller
            .review()
            .is_some_and(|review| review.job == job && review.loaded())
    });
}

/// A job on `input` in review, opened.
fn opened_review(controller: &mut Controller, input: &Path) -> JobId {
    let job = reviewable_job(controller, JobConfig::new(input.to_path_buf()));
    open(controller, job);
    job
}

fn review(controller: &Controller) -> &Review {
    controller.review().expect("a review is open")
}

/// The id of the photo named `name`.
fn photo(review: &Review, name: &str) -> usize {
    review
        .files
        .iter()
        .position(|file| file.path.file_name().is_some_and(|n| n == name))
        .unwrap_or_else(|| panic!("no photo {name}"))
}

/// The stack that holds the photo named `name`.
fn stack<'a>(review: &'a Review, name: &str) -> &'a GroupView {
    let id = photo(review, name);
    review
        .groups
        .iter()
        .find(|group| group.members.contains(&id))
        .unwrap_or_else(|| panic!("{name} is in no stack: {:#?}", review.groups))
}

/// The names of `photos`, sorted.
fn names(review: &Review, photos: &[usize]) -> Vec<String> {
    let mut names: Vec<String> = photos
        .iter()
        .map(|&id| {
            let name = review.files[id].path.file_name().unwrap();
            name.to_string_lossy().into_owned()
        })
        .collect();
    names.sort();
    names
}

/// The decision on the stack that holds the photo named `name`.
fn decision(controller: &Controller, name: &str) -> GroupStatus {
    stack(review(controller), name).status
}

fn write_counts(to_write: usize, proposed: usize) -> WriteCounts {
    WriteCounts { to_write, proposed }
}

#[test]
fn opening_a_job_in_review_loads_its_files_stacks_and_tray() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let other = bracket_folder();
    let mut controller = controller(&dirs);
    let mut config = JobConfig::new(input.path().to_path_buf());
    config.ml_model = Some("dinov2-small".into());
    let job = reviewable_job(&mut controller, config);
    let second = reviewable_job(&mut controller, JobConfig::new(other.path().to_path_buf()));
    // No review is open until a job is selected.
    assert!(controller.review().is_none());

    // It opens with the selection, empty until the engine answers; the sliders show the
    // job's thresholds, and the model's own similarity.
    controller.select_job(job);
    let opened = review(&controller);
    assert_eq!((opened.job, opened.loaded()), (job, false));
    assert!(opened.files.is_empty() && opened.groups.is_empty() && opened.unstacked.is_empty());
    assert_eq!(
        (opened.hamming, opened.cosine, opened.default_cosine),
        (8, None, Some(0.5))
    );
    wait_until(&mut controller, "the review to load", |controller| {
        controller.review().is_some_and(Review::loaded)
    });

    let review = review(&controller);
    assert_eq!(review.files.len(), 5);
    assert_eq!(review.groups.len(), 2);
    assert_eq!(
        names(review, &stack(review, "a_1.jpg").members),
        ["a_1.jpg", "a_2.jpg"]
    );
    assert_eq!(
        names(review, &stack(review, "b_1.jpg").members),
        ["b_1.jpg", "b_2.jpg"]
    );
    assert_eq!(names(review, &review.unstacked), ["b_3.jpg"]);
    assert_eq!(review.selection, Selection::default());

    // Selecting another job opens its review instead; a job leaving the list closes it.
    controller.select_job(second);
    let replaced = controller.review().unwrap();
    assert_eq!((replaced.job, replaced.default_cosine), (second, None));
    controller.delete(second);
    settle(&mut controller);
    assert!(controller.review().is_none());
    assert!(controller.take_notices().is_empty());
}

#[test]
fn a_confirmed_stack_of_one_photo_is_not_counted_to_write() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut controller = controller(&dirs);
    opened_review(&mut controller, input.path());
    let b_3 = photo(review(&controller), "b_3.jpg");
    controller.move_photo(b_3, MoveTarget::NewGroup);
    settle(&mut controller);
    let alone = stack(review(&controller), "b_3.jpg").id;
    assert_eq!(controller.write_counts(), write_counts(0, 3));

    // The write would skip it: confirmed, it is no longer proposed, and not to write.
    controller.confirm(alone);
    settle(&mut controller);
    assert_eq!(decision(&controller, "b_3.jpg"), GroupStatus::Confirmed);
    assert_eq!(controller.write_counts(), write_counts(0, 2));
    controller.confirm_all();
    settle(&mut controller);
    assert_eq!(controller.write_counts(), write_counts(2, 0));
    assert!(controller.take_notices().is_empty());
}

#[test]
fn confirm_reject_and_the_bulk_actions_reach_the_stacks() {
    use GroupStatus::{Confirmed, Proposed, Rejected};
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut controller = controller(&dirs);
    opened_review(&mut controller, input.path());
    let a = stack(review(&controller), "a_1.jpg").id;
    let decisions = |controller: &Controller| {
        (
            decision(controller, "a_1.jpg"),
            decision(controller, "b_1.jpg"),
            controller.write_counts(),
        )
    };
    assert_eq!(
        decisions(&controller),
        (Proposed, Proposed, write_counts(0, 2))
    );

    controller.confirm(a);
    settle(&mut controller);
    assert_eq!(
        decisions(&controller),
        (Confirmed, Proposed, write_counts(1, 1))
    );
    controller.confirm_all();
    settle(&mut controller);
    assert_eq!(
        decisions(&controller),
        (Confirmed, Confirmed, write_counts(2, 0))
    );
    controller.reject(a);
    settle(&mut controller);
    assert_eq!(
        decisions(&controller),
        (Rejected, Confirmed, write_counts(1, 0))
    );

    // `b` grows into a new stack, Proposed; reject_all decides only that one.
    controller.set_thresholds(NEAR, TIME_WINDOW, None);
    settle(&mut controller);
    assert_eq!(
        decisions(&controller),
        (Rejected, Proposed, write_counts(0, 1))
    );
    controller.reject_all();
    settle(&mut controller);
    assert_eq!(
        decisions(&controller),
        (Rejected, Rejected, write_counts(0, 0))
    );

    // The selected stacks, each.
    let b = stack(review(&controller), "b_1.jpg").id;
    controller.select_group(a, false);
    controller.select_group(b, true);
    controller.confirm_selected();
    settle(&mut controller);
    assert_eq!(
        decisions(&controller),
        (Confirmed, Confirmed, write_counts(2, 0))
    );
    controller.reject_selected();
    settle(&mut controller);
    assert_eq!(
        decisions(&controller),
        (Rejected, Rejected, write_counts(0, 0))
    );
    assert!(controller.take_notices().is_empty());
}

#[test]
fn slider_changes_regroup_and_the_shown_value_is_the_users() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut controller = controller(&dirs);
    let job = opened_review(&mut controller, input.path());
    let a = stack(review(&controller), "a_1.jpg").id;

    // The slider shows the new value at once, before the engine has regrouped.
    controller.set_thresholds(NEAR, TIME_WINDOW, None);
    assert_eq!(review(&controller).hamming, NEAR);
    settle(&mut controller);
    let review = review(&controller);
    assert_eq!(
        names(review, &stack(review, "b_1.jpg").members),
        ["b_1.jpg", "b_2.jpg", "b_3.jpg"]
    );
    assert!(review.unstacked.is_empty());
    assert_eq!(stack(review, "a_1.jpg").id, a);
    assert_eq!((review.hamming, review.cosine), (NEAR, None));
    // The job in the list follows the review's reads.
    assert_eq!(controller.job(job).unwrap().config.hamming_threshold, NEAR);

    // A drag: every position is shown as it comes, and the last one is the grouping.
    for hamming in [16, 12, 10, 8] {
        controller.set_thresholds(hamming, TIME_WINDOW, Some(0.9));
        assert_eq!(controller.review().unwrap().hamming, hamming);
    }
    settle(&mut controller);
    let review = controller.review().unwrap();
    assert_eq!(
        names(review, &stack(review, "b_1.jpg").members),
        ["b_1.jpg", "b_2.jpg"]
    );
    assert_eq!(names(review, &review.unstacked), ["b_3.jpg"]);
    assert_eq!((review.hamming, review.cosine), (8, Some(0.9)));
    let config = &controller.job(job).unwrap().config;
    assert_eq!(
        (config.hamming_threshold, config.cosine_threshold),
        (8, Some(0.9))
    );
    assert!(controller.take_notices().is_empty());
}

#[test]
fn a_selection_survives_a_regrouping_that_keeps_its_stack_and_drops_one_that_is_gone() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut controller = controller(&dirs);
    opened_review(&mut controller, input.path());
    let (a, b) = {
        let review = review(&controller);
        (stack(review, "a_1.jpg").id, stack(review, "b_1.jpg").id)
    };
    controller.select_group(a, false);
    controller.select_group(b, true);
    assert_eq!(review(&controller).selection.groups, [a, b]);

    // `a` keeps its members, and its id; `b` grows into a new stack.
    controller.set_thresholds(NEAR, TIME_WINDOW, None);
    settle(&mut controller);
    assert_eq!(
        review(&controller).selection,
        Selection {
            groups: vec![a],
            photo: None
        }
    );

    // A photo of a stack that survives stays selected too.
    let a_2 = photo(review(&controller), "a_2.jpg");
    controller.select_photo(a_2);
    let selected = Selection {
        groups: vec![a],
        photo: Some(a_2),
    };
    assert_eq!(review(&controller).selection, selected);
    controller.set_thresholds(8, TIME_WINDOW, None);
    settle(&mut controller);
    assert_eq!(review(&controller).selection, selected);

    // A selected photo stays only in the primary stack: moved to another stack, or to the
    // tray while its stack lives on, it is no longer selected.
    controller.set_thresholds(NEAR, TIME_WINDOW, None);
    settle(&mut controller);
    let b = stack(review(&controller), "b_1.jpg").id;
    let b_3 = photo(review(&controller), "b_3.jpg");
    controller.select_photo(b_3);
    controller.move_photo(b_3, MoveTarget::Group(a));
    settle(&mut controller);
    assert_eq!(stack(review(&controller), "b_1.jpg").id, b);
    assert_eq!(
        review(&controller).selection,
        Selection {
            groups: vec![b],
            photo: None
        }
    );
    controller.select_photo(b_3);
    controller.move_photo(b_3, MoveTarget::Tray);
    settle(&mut controller);
    assert_eq!(stack(review(&controller), "a_1.jpg").id, a);
    assert_eq!(
        review(&controller).selection,
        Selection {
            groups: vec![a],
            photo: None
        }
    );

    // In the tray with no stack selected, it stays.
    controller.select_photo(b_3);
    controller.set_thresholds(8, TIME_WINDOW, None);
    settle(&mut controller);
    let in_the_tray = Selection {
        groups: Vec::new(),
        photo: Some(b_3),
    };
    assert_eq!(review(&controller).selection, in_the_tray);
    // A photo the review does not show is not selected.
    controller.select_photo(review(&controller).files.len());
    assert_eq!(review(&controller).selection, in_the_tray);
    assert!(controller.take_notices().is_empty());
}

#[test]
fn stepping_moves_through_stacks_and_photos_and_stops_at_the_ends() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut controller = controller(&dirs);
    opened_review(&mut controller, input.path());
    let review = review(&controller);
    let (a, b) = (review.groups[0].clone(), review.groups[1].clone());
    let b_3 = photo(review, "b_3.jpg");
    let selection = |controller: &Controller| controller.review().unwrap().selection.clone();
    let stacks = |groups: &[&GroupView], photo: Option<usize>| Selection {
        groups: groups.iter().map(|group| group.id).collect(),
        photo,
    };

    // With nothing selected, a step up does nothing and a step down takes the first stack.
    controller.step_selection(-1, 0);
    assert_eq!(selection(&controller), Selection::default());
    controller.step_selection(1, 0);
    assert_eq!(selection(&controller), stacks(&[&a], None));
    controller.step_selection(1, 0);
    assert_eq!(selection(&controller), stacks(&[&b], None));
    controller.step_selection(1, 0);
    assert_eq!(selection(&controller), stacks(&[&b], None));
    controller.step_selection(-5, 0);
    assert_eq!(selection(&controller), stacks(&[&a], None));

    // Photos: the top one first, then down and up the stack, stopping at its ends.
    controller.step_selection(0, 1);
    assert_eq!(selection(&controller), stacks(&[&a], Some(a.members[0])));
    controller.step_selection(0, 1);
    assert_eq!(selection(&controller), stacks(&[&a], Some(a.members[1])));
    controller.step_selection(0, 1);
    assert_eq!(selection(&controller), stacks(&[&a], Some(a.members[1])));
    controller.step_selection(0, -3);
    assert_eq!(selection(&controller), stacks(&[&a], Some(a.members[0])));

    // A stack step selects one stack alone, from the primary, and no photo.
    controller.select_group(b.id, true);
    assert_eq!(selection(&controller), stacks(&[&a, &b], None));
    controller.step_selection(-1, 0);
    assert_eq!(selection(&controller), stacks(&[&a], None));

    // A photo of the tray is in no stack: a photo step has none to move in.
    controller.select_photo(b_3);
    assert_eq!(selection(&controller), stacks(&[], Some(b_3)));
    controller.step_selection(0, 1);
    assert_eq!(selection(&controller), stacks(&[], Some(b_3)));
    controller.clear_selection();
    assert_eq!(selection(&controller), Selection::default());
}

#[test]
fn moving_a_photo_makes_its_stacks_manual() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut controller = controller(&dirs);
    opened_review(&mut controller, input.path());
    let review = review(&controller);
    let (a, b) = (stack(review, "a_1.jpg").id, stack(review, "b_1.jpg").id);
    let (b_1, b_3) = (photo(review, "b_1.jpg"), photo(review, "b_3.jpg"));

    controller.move_photo(b_3, MoveTarget::Group(b));
    settle(&mut controller);
    let review = controller.review().unwrap();
    let joined = stack(review, "b_3.jpg");
    assert_eq!(
        (joined.id, joined.origin, joined.status),
        (b, GroupOrigin::Manual, GroupStatus::Proposed)
    );
    assert_eq!(
        names(review, &joined.members),
        ["b_1.jpg", "b_2.jpg", "b_3.jpg"]
    );
    assert!(review.unstacked.is_empty());

    // The stack it leaves and the stack it joins.
    controller.move_photo(b_1, MoveTarget::Group(a));
    settle(&mut controller);
    let review = controller.review().unwrap();
    for (name, id, members) in [
        ("a_1.jpg", a, ["a_1.jpg", "a_2.jpg", "b_1.jpg"].as_slice()),
        ("b_2.jpg", b, ["b_2.jpg", "b_3.jpg"].as_slice()),
    ] {
        let edited = stack(review, name);
        assert_eq!((edited.id, edited.origin), (id, GroupOrigin::Manual));
        assert_eq!(names(review, &edited.members), members);
    }
    assert!(controller.take_notices().is_empty());
}

#[test]
fn merge_selected_keeps_the_primary_stack() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut controller = controller(&dirs);
    opened_review(&mut controller, input.path());
    let review = review(&controller);
    let (a, b) = (stack(review, "a_1.jpg").id, stack(review, "b_1.jpg").id);

    // One stack is no merge: nothing is sent.
    controller.select_group(a, false);
    controller.merge_selected();
    settle(&mut controller);
    assert_eq!(controller.review().unwrap().groups.len(), 2);

    // `b`, picked last, is the primary stack: the merged one keeps its id.
    controller.select_group(b, true);
    controller.merge_selected();
    let primary_alone = Selection {
        groups: vec![b],
        photo: None,
    };
    assert_eq!(controller.review().unwrap().selection, primary_alone);
    settle(&mut controller);
    let review = controller.review().unwrap();
    assert_eq!(review.groups.len(), 1, "{:#?}", review.groups);
    let merged = &review.groups[0];
    assert_eq!((merged.id, merged.origin), (b, GroupOrigin::Manual));
    assert_eq!(
        names(review, &merged.members),
        ["a_1.jpg", "a_2.jpg", "b_1.jpg", "b_2.jpg"]
    );
    assert_eq!(review.selection, primary_alone);
    assert!(controller.take_notices().is_empty());
}

#[test]
fn split_makes_a_second_stack() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut controller = controller(&dirs);
    opened_review(&mut controller, input.path());
    let review = review(&controller);
    let (a, b) = (stack(review, "a_1.jpg").id, stack(review, "b_1.jpg").id);
    let (a_1, a_2) = (photo(review, "a_1.jpg"), photo(review, "a_2.jpg"));
    controller.select_group(a, false);
    controller.select_group(b, true);
    controller.merge_selected();

    controller.split(b, &[a_1, a_2]);
    settle(&mut controller);
    let review = controller.review().unwrap();
    assert_eq!(review.groups.len(), 2, "{:#?}", review.groups);
    let (rest, part) = (stack(review, "b_1.jpg"), stack(review, "a_1.jpg"));
    assert_eq!(rest.id, b);
    assert_ne!(part.id, b);
    assert_eq!(names(review, &rest.members), ["b_1.jpg", "b_2.jpg"]);
    assert_eq!(names(review, &part.members), ["a_1.jpg", "a_2.jpg"]);
    for group in [rest, part] {
        assert_eq!(
            (group.origin, group.status),
            (GroupOrigin::Manual, GroupStatus::Proposed)
        );
    }
    assert!(controller.take_notices().is_empty());
}

#[test]
fn checked_members_follow_the_primary_stack() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut controller = controller(&dirs);
    opened_review(&mut controller, input.path());
    let opened = review(&controller);
    let (a, b) = (stack(opened, "a_1.jpg").id, stack(opened, "b_1.jpg").id);
    let [a_1, a_2, b_2, b_3] =
        ["a_1.jpg", "a_2.jpg", "b_2.jpg", "b_3.jpg"].map(|name| photo(opened, name));
    controller.select_group(a, false);
    controller.select_group(b, true);
    controller.merge_selected();
    settle(&mut controller);
    let merged = stack(review(&controller), "a_1.jpg").clone();
    assert_eq!(merged.id, b);
    // In layer order, whatever the order they were checked in.
    let layer_order = |photos: &[usize]| -> Vec<usize> {
        merged
            .members
            .iter()
            .copied()
            .filter(|member| photos.contains(member))
            .collect()
    };

    controller.toggle_member(b_2);
    controller.toggle_member(a_1);
    assert_eq!(controller.checked_members(), layer_order(&[a_1, b_2]));
    controller.toggle_member(a_1);
    assert_eq!(controller.checked_members(), [b_2]);
    // A photo outside the primary stack is not checked.
    controller.toggle_member(b_3);
    assert_eq!(controller.checked_members(), [b_2]);
    // Reads that leave the stack's members as they were keep them: another stack made,
    // then confirmed.
    controller.move_photo(b_3, MoveTarget::NewGroup);
    settle(&mut controller);
    let alone = stack(review(&controller), "b_3.jpg").id;
    controller.confirm(alone);
    settle(&mut controller);
    assert_eq!(
        stack(review(&controller), "b_3.jpg").status,
        GroupStatus::Confirmed
    );
    assert_eq!(controller.checked_members(), [b_2]);
    // A photo of the primary stack selected keeps them.
    controller.select_photo(a_2);
    assert_eq!(controller.checked_members(), [b_2]);
    // Another selection clears them, for good.
    controller.clear_selection();
    assert!(controller.checked_members().is_empty());
    controller.select_group(b, false);
    assert!(controller.checked_members().is_empty());

    // A read that changes the stack's members clears them.
    controller.toggle_member(a_1);
    controller.toggle_member(a_2);
    assert_eq!(controller.checked_members(), layer_order(&[a_1, a_2]));
    controller.move_photo(b_3, MoveTarget::Group(b));
    settle(&mut controller);
    assert_eq!(stack(review(&controller), "b_3.jpg").id, b);
    assert!(controller.checked_members().is_empty());

    // A split of the checked photos clears them.
    controller.toggle_member(a_1);
    controller.toggle_member(a_2);
    controller.split_checked();
    assert!(controller.checked_members().is_empty());
    settle(&mut controller);
    let split = review(&controller);
    assert_eq!(
        names(split, &stack(split, "a_1.jpg").members),
        ["a_1.jpg", "a_2.jpg"]
    );
    assert_eq!(
        names(split, &stack(split, "b_1.jpg").members),
        ["b_1.jpg", "b_2.jpg", "b_3.jpg"]
    );
    assert!(controller.take_notices().is_empty());
}

#[test]
fn a_selection_the_results_do_not_list_goes_when_the_write_starts() {
    let (_data, dirs) = data_dir();
    let (first, second) = (review_folder(), review_folder());
    let mut controller = controller(&dirs);

    for (input, select_the_confirmed, kept) in
        [(first.path(), false, false), (second.path(), true, true)]
    {
        let job = opened_review(&mut controller, input);
        let opened = review(&controller);
        let (a, b) = (stack(opened, "a_1.jpg").id, stack(opened, "b_1.jpg").id);
        controller.confirm(a);
        wait_until(&mut controller, "stack a confirmed", |controller| {
            controller.write_counts().to_write == 1
        });
        // The confirmed stack is listed in the results; the proposed one is not.
        let selected = if select_the_confirmed { a } else { b };
        controller.select_group(selected, false);
        controller.write();
        settle(&mut controller);
        assert_eq!(status(&controller, job), Some(JobStatus::Completed));

        let expected = if kept {
            Selection {
                groups: vec![selected],
                photo: None,
            }
        } else {
            Selection::default()
        };
        assert_eq!(review(&controller).selection, expected);
    }
}

#[test]
fn swap_active_refreshes_the_files() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    write_raw_bracket(input.path(), "brk", 5);
    let mut controller = controller(&dirs);
    opened_review(&mut controller, input.path());
    let opened = review(&controller);
    let (raf, jpg) = (photo(opened, "brk_2.raf"), photo(opened, "brk_2.jpg"));
    assert_eq!(opened.files[jpg].paired_with, Some(raf));
    assert_eq!(opened.partners(raf), [jpg]);
    let bracket = stack(opened, "brk_2.raf").id;

    controller.swap_active(jpg);
    // `FilesChanged` reads the files again: nothing else asks for them. Its
    // `GroupsChanged` comes on its own, maybe after a moment of quiet, so the test waits
    // for the regrouped stack itself, not for the controller to settle.
    wait_until(&mut controller, "the files read again", |controller| {
        let files = &review(controller).files;
        files[jpg].paired_with.is_none() && files[raf].paired_with == Some(jpg)
    });
    wait_until(&mut controller, "the stacks read again", |controller| {
        review(controller)
            .groups
            .iter()
            .any(|group| group.members.contains(&jpg))
    });
    settle(&mut controller);
    let swapped = review(&controller);
    assert_eq!(swapped.partners(jpg), [raf]);
    assert!(swapped.partners(raf).is_empty());
    assert_eq!(stack(swapped, "brk_2.jpg").id, bracket);
    assert!(controller.take_notices().is_empty());
}

/// Stage 3's embeddings as [`common::StubEmbedder`] gives them; every call after the first
/// waits at `gate`.
struct HeldAfterTheFirst {
    gate: Arc<Gate>,
    calls: AtomicUsize,
}

impl EmbedSource for HeldAfterTheFirst {
    fn embed(&self, model: &ModelSpec, images: &[RgbImage]) -> Result<Vec<Vec<f32>>, String> {
        if self.calls.fetch_add(1, Ordering::SeqCst) > 0 {
            self.gate.pass();
        }
        common::StubEmbedder.embed(model, images)
    }
}

#[test]
fn the_engine_is_busy_while_a_job_runs_or_a_visual_check_embeds() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    write_ambiguous_stack(input.path());
    let scan = Arc::new(Gate::default());
    let embedding = Arc::new(Gate::default());
    let mut options = options(&dirs);
    options.wake = scan.holding_the_scan();
    options.embedder = Some(Arc::new(HeldAfterTheFirst {
        gate: Arc::clone(&embedding),
        calls: AtomicUsize::new(0),
    }));
    let mut controller = Controller::start(options).unwrap();
    let _open_scan = OpenOnDrop(Arc::clone(&scan));
    let _open_embedding = OpenOnDrop(Arc::clone(&embedding));
    settle(&mut controller);
    assert!(!controller.engine_busy());

    // A scan running, held while it examines the photos.
    let mut config = JobConfig::new(input.path().to_path_buf());
    config.ml_model = Some("dinov2-small".into());
    controller.submit(config);
    let job = wait_for_hashing(&mut controller);
    assert!(controller.engine_busy());
    scan.open();
    settle(&mut controller);
    assert_eq!(status(&controller, job), Some(JobStatus::ReviewPending));
    assert!(!controller.engine_busy());

    // In review, a regrouping makes a stack the visual check must embed: its embed task
    // stores the first batch, reports it, and is held at the second.
    open(&mut controller, job);
    controller.set_thresholds(NEAR, TIME_WINDOW, None);
    wait_until(
        &mut controller,
        "the visual check's progress",
        |controller| {
            controller
                .progress(job)
                .is_some_and(|progress| progress.stage == Stage::Embed)
        },
    );
    assert_eq!(status(&controller, job), Some(JobStatus::ReviewPending));
    assert!(!controller.is_working());
    assert!(controller.engine_busy());

    // Its groups changed: the visual check is over.
    embedding.open();
    wait_until(&mut controller, "the engine idle", |controller| {
        controller.is_idle() && !controller.engine_busy()
    });
    assert!(controller.take_notices().is_empty());
}

#[test]
fn a_visual_check_ticking_after_its_write_was_cancelled_leaves_no_progress() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    write_ambiguous_stack(input.path());
    let other = bracket_folder();
    let embedding = Arc::new(Gate::default());
    let mut options = options(&dirs);
    options.embedder = Some(Arc::new(HeldAfterTheFirst {
        gate: Arc::clone(&embedding),
        calls: AtomicUsize::new(0),
    }));
    let mut controller = Controller::start(options).unwrap();
    let _open = OpenOnDrop(Arc::clone(&embedding));
    let mut config = JobConfig::new(input.path().to_path_buf());
    config.ml_model = Some("dinov2-small".into());
    let job = reviewable_job(&mut controller, config);
    open(&mut controller, job);

    // A regrouping's visual check, held at its second batch.
    controller.set_thresholds(NEAR, TIME_WINDOW, None);
    wait_until(
        &mut controller,
        "the visual check's progress",
        |controller| {
            controller
                .progress(job)
                .is_some_and(|progress| progress.stage == Stage::Embed)
        },
    );
    // Sent to its write, which waits behind the visual check, then cancelled.
    controller.write();
    wait_until(&mut controller, "the write queued", |controller| {
        controller.is_idle() && status(controller, job) == Some(JobStatus::Queued)
    });
    controller.cancel(job);
    wait_until(&mut controller, "the job cancelled", |controller| {
        controller.is_idle() && status(controller, job) == Some(JobStatus::Cancelled)
    });
    assert!(controller.progress(job).is_none());

    // The visual check goes on and ticks for its second batch. A job queued behind it is
    // scanned once it is done: by then its last ticks have come.
    embedding.open();
    reviewable_job(&mut controller, JobConfig::new(other.path().to_path_buf()));
    assert_eq!(status(&controller, job), Some(JobStatus::Cancelled));
    assert!(controller.progress(job).is_none());
    assert!(!controller.engine_busy());
}

#[test]
fn a_refused_edit_is_a_notice_and_changes_nothing() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    for (folder, prefix, seed, second) in [("one", "first", 1, 0), ("two", "second", 2, 30)] {
        let dir = input.path().join(folder);
        std::fs::create_dir(&dir).unwrap();
        write_bracket(&dir, prefix, seed, second, 3);
    }
    let mut controller = controller(&dirs);
    let mut config = JobConfig::new(input.path().to_path_buf());
    config.recursive = true;
    let job = reviewable_job(&mut controller, config);
    open(&mut controller, job);
    let review = review(&controller);
    let before = (review.groups.clone(), review.unstacked.clone());
    let first_1 = photo(review, "first_1.jpg");
    let other_folder = stack(review, "second_1.jpg").id;

    controller.move_photo(first_1, MoveTarget::Group(other_folder));
    settle(&mut controller);
    assert_eq!(
        controller.take_notices(),
        [Notice {
            level: NoticeLevel::Error,
            text: "A stack cannot hold photos of two folders.".into(),
        }]
    );
    let review = controller.review().unwrap();
    assert_eq!((review.groups.clone(), review.unstacked.clone()), before);
}

#[test]
fn write_writes_the_confirmed_stacks_and_the_review_follows() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut controller = controller(&dirs);
    let job = opened_review(&mut controller, input.path());
    let a = stack(review(&controller), "a_1.jpg").id;
    controller.confirm(a);
    settle(&mut controller);
    assert_eq!(controller.write_counts(), write_counts(1, 1));

    // The review stays open while the job is queued, written and done, and follows each
    // stack's outcome without being asked.
    controller.write();
    wait_until(&mut controller, "the write to end", |controller| {
        assert_eq!(controller.review().map(|review| review.job), Some(job));
        controller.is_idle() && status(controller, job) == Some(JobStatus::Completed)
    });
    let review = review(&controller);
    let written = stack(review, "a_1.jpg");
    assert_eq!((written.id, written.status), (a, GroupStatus::Written));
    assert!(
        written
            .output_path
            .as_ref()
            .is_some_and(|path| path.is_file())
    );
    let proposed = stack(review, "b_1.jpg");
    assert_eq!(
        (proposed.status, &proposed.output_path),
        (GroupStatus::Proposed, &None)
    );
    assert_eq!(controller.write_counts(), write_counts(0, 1));
    assert_eq!(controller.job(job).unwrap().counts.written, 1);
    assert!(controller.take_notices().is_empty());
}

#[test]
fn a_review_reopens_as_it_was_left() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut first = controller(&dirs);
    let job = opened_review(&mut first, input.path());
    let a = stack(review(&first), "a_1.jpg").id;
    // The slider waits for its regrouping: a newer position would take its place in the
    // queue, ahead of the edits below.
    first.set_thresholds(NEAR, TIME_WINDOW, None);
    settle(&mut first);
    let b_3 = photo(review(&first), "b_3.jpg");
    first.confirm(a);
    first.move_photo(b_3, MoveTarget::Tray);
    first.set_thresholds(12, TIME_WINDOW, None);
    settle(&mut first);
    let left = review(&first).clone();
    assert!(first.take_notices().is_empty());
    drop(first);

    let mut second = controller(&dirs);
    wait_until(&mut second, "the jobs to be read", Controller::jobs_loaded);
    open(&mut second, job);
    let review = review(&second);
    assert_eq!(review.groups, left.groups);
    assert_eq!(decision(&second, "a_1.jpg"), GroupStatus::Confirmed);
    let manual = stack(review, "b_1.jpg");
    assert_eq!(manual.origin, GroupOrigin::Manual);
    assert_eq!(names(review, &manual.members), ["b_1.jpg", "b_2.jpg"]);
    assert_eq!(review.unstacked, [b_3]);
    assert!(review.files[b_3].pinned);
    assert_eq!(review.hamming, 12);
}

/// Holds the backend at a command until `gate` opens; returns once the command runs.
fn hold_the_backend(controller: &mut Controller, gate: &Arc<Gate>) {
    let entered = Arc::new(AtomicBool::new(false));
    let command: Command = {
        let gate = Arc::clone(gate);
        let entered = Arc::clone(&entered);
        Box::new(move |_engine| {
            entered.store(true, Ordering::SeqCst);
            gate.pass();
            Ok(())
        })
    };
    controller.send(Request::Run { job: None, command });
    wait_until(controller, "the backend to be held", |_| {
        entered.load(Ordering::SeqCst)
    });
}

#[test]
fn a_review_reopened_before_its_thresholds_are_read_shows_them() {
    let (_data, dirs) = data_dir();
    let (first, second) = (review_folder(), bracket_folder());
    let mut controller = controller(&dirs);
    let a = reviewable_job(&mut controller, JobConfig::new(first.path().to_path_buf()));
    let b = reviewable_job(&mut controller, JobConfig::new(second.path().to_path_buf()));
    open(&mut controller, a);

    // The regrouping's GroupsChanged comes once `b` is open: nothing reads `a` again.
    let gate = Arc::new(Gate::default());
    let _open = OpenOnDrop(Arc::clone(&gate));
    hold_the_backend(&mut controller, &gate);
    controller.set_thresholds(NEAR, TIME_WINDOW, None);
    controller.select_job(b);
    gate.open();
    settle(&mut controller);
    assert_eq!(
        controller.job(a).unwrap().config.hamming_threshold,
        8,
        "the list's copy of `a` is older than its regrouping"
    );

    // Opened again, `a` shows the thresholds it was regrouped with.
    controller.select_job(a);
    assert_eq!(review(&controller).hamming, NEAR);
    open(&mut controller, a);
    let review = review(&controller);
    assert_eq!(review.hamming, NEAR);
    assert_eq!(
        names(review, &stack(review, "b_1.jpg").members),
        ["b_1.jpg", "b_2.jpg", "b_3.jpg"]
    );
    assert!(controller.take_notices().is_empty());
}

#[test]
fn the_time_window_regroups_and_shows_the_users_value_until_it_is_stored() {
    let (_data, dirs) = data_dir();
    let (spaced, other) = (tempfile::tempdir().unwrap(), bracket_folder());
    write_spaced_shots(spaced.path(), 10);
    let mut controller = controller(&dirs);
    let mut config = JobConfig::new(spaced.path().to_path_buf());
    config.time_window_seconds = 4;
    let a = reviewable_job(&mut controller, config);
    let b = reviewable_job(&mut controller, JobConfig::new(other.path().to_path_buf()));
    open(&mut controller, a);
    assert_eq!(review(&controller).time_window_seconds, 4);
    assert!(review(&controller).groups.is_empty());

    // Until the engine answers, the review shows the window sent, also when it is opened
    // again meanwhile; the list keeps the stored one.
    let gate = Arc::new(Gate::default());
    let _open = OpenOnDrop(Arc::clone(&gate));
    hold_the_backend(&mut controller, &gate);
    controller.set_thresholds(8, 30, None);
    assert_eq!(review(&controller).time_window_seconds, 30);
    controller.select_job(b);
    controller.select_job(a);
    assert_eq!(review(&controller).time_window_seconds, 30);
    assert_eq!(controller.job(a).unwrap().config.time_window_seconds, 4);

    // The request carried the window: the engine regrouped with it, and stored it.
    gate.open();
    settle(&mut controller);
    open(&mut controller, a);
    let review = review(&controller);
    assert_eq!(
        names(review, &stack(review, "a.jpg").members),
        ["a.jpg", "b.jpg"]
    );
    assert!(review.unstacked.is_empty());
    assert_eq!((review.hamming, review.time_window_seconds), (8, 30));
    assert_eq!(controller.job(a).unwrap().config.time_window_seconds, 30);
    assert!(controller.take_notices().is_empty());
}

#[test]
fn refused_thresholds_are_forgotten_and_the_stored_ones_show_again() {
    let (_data, dirs) = data_dir();
    let (first, second) = (review_folder(), bracket_folder());
    let mut controller = controller(&dirs);
    let a = reviewable_job(&mut controller, JobConfig::new(first.path().to_path_buf()));
    let b = reviewable_job(&mut controller, JobConfig::new(second.path().to_path_buf()));
    open(&mut controller, a);

    // Past the slider's end: the engine refuses it.
    controller.set_thresholds(41, TIME_WINDOW, None);
    settle(&mut controller);
    let notices = controller.take_notices();
    assert!(
        matches!(
            notices.as_slice(),
            [Notice {
                level: NoticeLevel::Error,
                ..
            }]
        ),
        "{notices:?}"
    );
    controller.select_job(b);
    controller.select_job(a);
    assert_eq!(review(&controller).hamming, 8);
}

/// The names of the files in `dir`, sorted.
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn a_review_closes_while_its_job_is_scanned_again() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    write_ambiguous_stack(input.path());
    let embedding = Arc::new(Gate::default());
    let scan = Arc::new(Gate::default());
    scan.open();
    let embedder = Arc::new(HeldEmbedder {
        gate: Arc::clone(&embedding),
        called: AtomicBool::new(false),
    });
    let mut options = options(&dirs);
    options.embedder = Some(Arc::clone(&embedder) as Arc<dyn EmbedSource>);
    options.wake = scan.holding_the_scan();
    let mut controller = Controller::start(options).unwrap();
    let _open_embedding = OpenOnDrop(Arc::clone(&embedding));
    let _open_scan = OpenOnDrop(Arc::clone(&scan));

    // Cancelled during its visual check, the job keeps the analysis its scan saved.
    let mut config = JobConfig::new(input.path().to_path_buf());
    config.ml_model = Some("dinov2-small".into());
    config.hamming_threshold = NEAR;
    controller.submit(config);
    wait_until(&mut controller, "the visual check", |controller| {
        !controller.jobs().is_empty() && embedder.called.load(Ordering::SeqCst)
    });
    let job = controller.jobs()[0].id;
    controller.select_job(job);
    assert!(
        controller.review().is_none(),
        "a scanning job has no review"
    );
    controller.cancel(job);
    wait_until(&mut controller, "the cancel to run", Controller::is_idle);
    embedding.open();
    settle(&mut controller);
    assert_eq!(status(&controller, job), Some(JobStatus::Cancelled));
    wait_until(&mut controller, "the review to load", |controller| {
        controller.review().is_some_and(Review::loaded)
    });
    assert_eq!(review(&controller).files.len(), 10);

    // Scanned again, on a folder that lost two photos: no review while it scans.
    for name in ["panned_3.jpg", "panned_4.jpg"] {
        std::fs::remove_file(input.path().join(name)).unwrap();
    }
    scan.close();
    controller.retry(job);
    wait_until(&mut controller, "the scan again", |controller| {
        status(controller, job) == Some(JobStatus::Scanning)
    });
    assert!(
        controller.review().is_none(),
        "a scanning job has no review"
    );
    scan.open();
    settle(&mut controller);
    assert_eq!(status(&controller, job), Some(JobStatus::ReviewPending));

    // Back in review, its files and its stacks are the new scan's.
    wait_until(&mut controller, "the review to load", |controller| {
        controller.review().is_some_and(Review::loaded)
    });
    let review = review(&controller);
    let files: Vec<usize> = (0..review.files.len()).collect();
    assert_eq!(names(review, &files), names_in(input.path()));
    assert!(review.unstacked.is_empty(), "{:?}", review.unstacked);
    assert_eq!(review.groups.len(), 1, "{:#?}", review.groups);
    assert_eq!(
        names(review, &review.groups[0].members),
        names_in(input.path())
    );
    assert!(controller.take_notices().is_empty());
}

// --- The models of Stage 3 ---

/// A controller whose models `installer` installs, and which was told ONNX Runtime is
/// where `runtime` says.
fn installing_controller(
    dirs: &AppDirs,
    installer: &Arc<FakeInstaller>,
    runtime: RuntimeCheck,
) -> Controller {
    let mut options = options(dirs);
    options.installer = Some(Arc::clone(installer) as Arc<dyn ModelInstall>);
    options.runtime = runtime;
    Controller::start(options).unwrap()
}

/// The view of the model `id`, once the models are read.
fn model_view<'a>(controller: &'a Controller, id: &str) -> Option<&'a ModelView> {
    controller.models().iter().find(|view| view.spec.id == id)
}

/// Waits until the models are read.
fn wait_for_the_models(controller: &mut Controller) {
    wait_until(controller, "the models", |controller| {
        controller.models().len() == MODELS.len()
    });
}

/// ONNX Runtime looked for at a made-up path, and not found.
fn runtime_missing() -> RuntimeCheck {
    RuntimeCheck {
        path: PathBuf::from("/nowhere/libonnxruntime.so"),
        problem: Some(LibraryProblem::Missing),
    }
}

#[test]
fn ml_state_follows_the_switch_the_model_and_its_download() {
    let (_data, dirs) = data_dir();
    let installer = FakeInstaller::holding();
    let mut controller = installing_controller(&dirs, &installer, runtime_found());
    let _open = OpenOnDrop(Arc::clone(&installer.gate));
    wait_for_the_models(&mut controller);
    if !cfg!(feature = "ml") {
        // A build without the visual check has none, whatever the switch says.
        controller.set_ml_enabled(true);
        assert_eq!(controller.ml_state(), MlState::NotBuilt);
        return;
    }
    let small = "dinov2-small";

    assert_eq!(controller.ml_state(), MlState::Off);
    controller.set_ml_enabled(true);
    assert_eq!(controller.ml_state(), MlState::Missing(small));
    controller.set_ml_model("dinov2-base");
    assert_eq!(controller.ml_state(), MlState::Missing("dinov2-base"));
    controller.set_ml_model(small);

    // Downloading, then cancelled: missing while it cancels, and after.
    controller.download_model(small);
    let half = MlState::Downloading {
        id: small,
        percent: 50,
    };
    wait_until(&mut controller, "the download at half", |controller| {
        controller.ml_state() == half
    });
    controller.cancel_download(small);
    wait_until(&mut controller, "the cancel", |controller| {
        model_view(controller, small).is_some_and(|view| view.activity == ModelActivity::Cancelling)
    });
    assert_eq!(controller.ml_state(), MlState::Missing(small));
    installer.gate.open();
    wait_until(&mut controller, "the download's end", |controller| {
        model_view(controller, small).is_some_and(|view| view.activity == ModelActivity::Idle)
    });
    assert_eq!(controller.ml_state(), MlState::Missing(small));

    // Downloaded: ready, until the switch is off.
    installer.holds.store(false, Ordering::SeqCst);
    controller.download_model(small);
    wait_until(&mut controller, "the model ready", |controller| {
        controller.ml_state() == MlState::Ready(small)
    });
    controller.set_ml_enabled(false);
    assert_eq!(controller.ml_state(), MlState::Off);
    assert!(controller.take_notices().is_empty());
    controller.set_ml_enabled(true);
    settle(&mut controller);
    drop(controller);

    // Without ONNX Runtime the check cannot run, whatever is installed and switched on.
    let controller = installing_controller(&dirs, &installer, runtime_missing());
    assert!(controller.prefs.ml_enabled);
    assert_eq!(controller.ml_state(), MlState::NoRuntime(runtime_missing()));
    drop(controller);

    // Nor with a library that needs a newer macOS.
    let controller = installing_controller(&dirs, &installer, runtime_needing_macos());
    assert_eq!(
        controller.ml_state(),
        MlState::NoRuntime(runtime_needing_macos())
    );
}

/// Submits the form on `input` with a similarity of 0.7, and gives the new job's model
/// and similarity.
fn model_of_a_new_job(controller: &mut Controller, input: &Path) -> (Option<String>, Option<f32>) {
    wait_until(controller, "the jobs", Controller::jobs_loaded);
    let before = controller.jobs().len();
    controller.open_new_job(Some(input.to_path_buf()));
    controller.form().unwrap().fields.cosine_threshold = Some(0.7);
    controller.submit_form();
    wait_until(controller, "the form's answer", |controller| {
        controller.view() != &View::NewJob
    });
    wait_until(controller, "the new job", |controller| {
        controller.jobs().len() == before + 1
    });
    let newest = controller.jobs().iter().max_by_key(|job| job.id).unwrap();
    (
        newest.config.ml_model.clone(),
        newest.config.cosine_threshold,
    )
}

#[test]
fn a_new_job_names_the_active_model_only_when_ml_is_on() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let installer = FakeInstaller::holding();
    let mut controller = installing_controller(&dirs, &installer, runtime_found());
    let _open = OpenOnDrop(Arc::clone(&installer.gate));
    wait_for_the_models(&mut controller);
    let none = (None, None);

    assert_eq!(model_of_a_new_job(&mut controller, input.path()), none);
    controller.set_ml_enabled(true);
    if !cfg!(feature = "ml") {
        assert_eq!(model_of_a_new_job(&mut controller, input.path()), none);
        return;
    }
    let small = (Some("dinov2-small".to_owned()), Some(0.7));
    // Not installed yet: the job is checked once it is.
    assert_eq!(model_of_a_new_job(&mut controller, input.path()), small);
    controller.download_model("dinov2-small");
    wait_until(&mut controller, "the download", |controller| {
        matches!(controller.ml_state(), MlState::Downloading { .. })
    });
    assert_eq!(model_of_a_new_job(&mut controller, input.path()), small);
    installer.gate.open();
    wait_until(&mut controller, "the model ready", |controller| {
        controller.ml_state() == MlState::Ready("dinov2-small")
    });
    assert_eq!(model_of_a_new_job(&mut controller, input.path()), small);
    controller.set_ml_model("dinov2-base");
    assert_eq!(
        model_of_a_new_job(&mut controller, input.path()),
        (Some("dinov2-base".to_owned()), Some(0.7))
    );
    settle(&mut controller);
    drop(controller);

    // Without ONNX Runtime no job names a model, nor with one that needs a newer macOS.
    for runtime in [runtime_missing(), runtime_needing_macos()] {
        let mut controller = installing_controller(&dirs, &installer, runtime);
        assert!(controller.prefs.ml_enabled);
        assert_eq!(model_of_a_new_job(&mut controller, input.path()), none);
        settle(&mut controller);
    }
}

#[test]
fn a_desktop_without_file_dialogs_says_a_model_file_cannot_be_chosen() {
    let (_data, dirs) = data_dir();
    let installer = FakeInstaller::new();
    let platform = Arc::new(FakePlatform::default());
    platform.no_dialogs.store(true, Ordering::SeqCst);
    let mut options = options(&dirs);
    options.installer = Some(Arc::clone(&installer) as Arc<dyn ModelInstall>);
    options.platform = Arc::clone(&platform) as Arc<dyn Platform>;
    let mut controller = Controller::start(options).unwrap();
    wait_for_the_models(&mut controller);

    controller.import_model("dinov2-small");
    settle(&mut controller);
    assert_eq!(
        controller.take_notices(),
        [Notice {
            level: NoticeLevel::Warning,
            text: "No file dialog is available on this desktop, so a model file cannot be chosen here."
                .into(),
        }]
    );
    // Nothing else happens.
    assert_eq!(installer.calls(), 0);
    assert!(controller.model_failure("dinov2-small").is_none());
    assert!(!model_view(&controller, "dinov2-small").unwrap().installed);
}

#[test]
fn importing_asks_for_a_file_and_installs_it() {
    let (_data, dirs) = data_dir();
    let installer = FakeInstaller::new();
    let platform = Arc::new(FakePlatform::default());
    let mut options = options(&dirs);
    options.installer = Some(Arc::clone(&installer) as Arc<dyn ModelInstall>);
    options.platform = Arc::clone(&platform) as Arc<dyn Platform>;
    let mut controller = Controller::start(options).unwrap();
    wait_for_the_models(&mut controller);

    // The dialog closed without a file: nothing happens.
    controller.import_model("dinov2-small");
    settle(&mut controller);
    assert_eq!(installer.calls(), 0);
    assert!(!model_view(&controller, "dinov2-small").unwrap().installed);

    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("model.onnx");
    std::fs::write(&file, b"weights").unwrap();
    platform.files.lock().unwrap().push_back(file.clone());
    controller.import_model("dinov2-small");
    wait_until(&mut controller, "the model installed", |controller| {
        model_view(controller, "dinov2-small")
            .is_some_and(|view| view.installed && view.activity == ModelActivity::Idle)
    });

    assert_eq!(installer.imported(), [file]);
    // The other model is left alone, and nothing went wrong.
    assert!(!model_view(&controller, "dinov2-base").unwrap().installed);
    assert!(controller.take_notices().is_empty());
}
