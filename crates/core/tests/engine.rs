//! Engine scenarios (spec §4.3, §5.4.1, §5.6, §5.7, §7.3): jobs submitted to
//! a real [`Engine`] over synthetic photos in a temporary folder, reviewed
//! and written.
//!
//! Each test gives its engine the folders of a temporary data folder, laid
//! out as `PHOTOSTACKER_DATA_DIR` lays them out ([`AppDirs::under`]), so no
//! test reads or writes the user's real app data.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, TryRecvError};
use image::RgbImage;
use photostacker_core::analyze::{Stage, WORKER_THREAD_PREFIX};
use photostacker_core::engine::{
    ENGINE_THREAD, EmbedSource, Engine, EngineError, EngineEvent, EngineOptions, FileView, GroupId,
    GroupView, JobId, JobView, ModelActivity, ModelInstall, ModelView, MoveTarget, Wake,
};
use photostacker_core::hash::HashPair;
use photostacker_core::model::{FileKind, GroupOrigin, GroupStatus, JobStatus, MlStatus, Warning};
use photostacker_core::models::{self, ModelError, ModelSpec, UrlFailure};
use photostacker_core::store::paths;
use photostacker_core::{AppDirs, ConfigError, JobConfig, PlanV1, Store, StoreError, analyze};
use photostacker_testkit::photos::{
    HEIGHT, JPEG_QUALITY, WIDTH, canon_exif, plain_jpeg, write_bracket, write_lone_shots,
    write_raw_bracket, write_review_photos,
};
use photostacker_testkit::{expose, jpeg_with_exif, raf_with_jpeg, scene, shift};
use tempfile::TempDir;

/// More files than the analysis has workers (at most 16), so a scan held at
/// its first ticks and then cancelled leaves some files unexamined.
const MANY: usize = 24;
/// How long a test waits for an event before it fails.
const TIMEOUT: Duration = Duration::from_secs(120);

// --- Fixtures ---

/// A temporary data folder and the app's folders in it.
fn data_dir() -> (TempDir, AppDirs) {
    let root = tempfile::tempdir().unwrap();
    let dirs = AppDirs::under(root.path().to_path_buf());
    (root, dirs)
}

fn options(dirs: &AppDirs, wake: Wake) -> EngineOptions {
    EngineOptions {
        db: dirs.db(),
        thumbs_dir: dirs.thumbs(),
        models_dir: dirs.models(),
        embedder: None,
        installer: None,
        wake,
    }
}

fn no_wake() -> Wake {
    Arc::new(|| {})
}

fn start(dirs: &AppDirs) -> Engine {
    Engine::start(options(dirs, no_wake())).unwrap()
}

/// A folder with a bracket of five and one lone shot.
fn bracket_folder() -> TempDir {
    let input = tempfile::tempdir().unwrap();
    write_bracket(input.path(), "brk", 1, 0, 5);
    write_lone_shots(input.path(), 1);
    input
}

/// Every file under `dir` with its bytes.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(snapshot(&path));
        } else {
            files.insert(path.clone(), fs::read(&path).unwrap());
        }
    }
    files
}

fn fresh_plan(config: &JobConfig) -> PlanV1 {
    let analysis = analyze(config, &|_| {}, &AtomicBool::new(false)).unwrap();
    PlanV1::from_analysis(config, &analysis)
}

fn engine_plan(engine: &Engine, job: JobId, config: &JobConfig) -> PlanV1 {
    PlanV1::from_analysis(config, &engine.analysis(job).unwrap())
}

// --- Events ---

/// Receives events until one satisfies `done`, and returns them all, that
/// one last.
fn wait_for(
    events: &Receiver<EngineEvent>,
    done: impl Fn(&EngineEvent) -> bool,
) -> Vec<EngineEvent> {
    let deadline = Instant::now() + TIMEOUT;
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let event = events
            .recv_timeout(left)
            .unwrap_or_else(|error| panic!("{error} after {seen:#?}"));
        let last = done(&event);
        seen.push(event);
        if last {
            return seen;
        }
    }
}

fn is_status(job: JobId, status: JobStatus) -> impl Fn(&EngineEvent) -> bool {
    move |event| *event == EngineEvent::JobStatusChanged { job, status }
}

fn is_hash_tick(job: JobId) -> impl Fn(&EngineEvent) -> bool {
    move |event| {
        matches!(event, EngineEvent::JobProgress { job: of, progress }
            if *of == job && progress.stage == Stage::Hash)
    }
}

/// The status changes among `events`, in order.
fn status_changes(events: &[EngineEvent]) -> Vec<(JobId, JobStatus)> {
    events
        .iter()
        .filter_map(|event| match *event {
            EngineEvent::JobStatusChanged { job, status } => Some((job, status)),
            _ => None,
        })
        .collect()
}

/// The failures among `events`: the engine's errors and the models'.
fn errors(events: &[EngineEvent]) -> Vec<&EngineEvent> {
    events
        .iter()
        .filter(|event| {
            matches!(
                event,
                EngineEvent::Error { .. } | EngineEvent::ModelFailed { .. }
            )
        })
        .collect()
}

/// Polls the job's view until it has `status`.
fn poll_status(engine: &Engine, job: JobId, status: JobStatus) -> JobView {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let view = engine.job(job).unwrap();
        if view.status == status {
            return view;
        }
        assert!(Instant::now() < deadline, "still {:?}", view.status);
        thread::sleep(Duration::from_millis(10));
    }
}

/// Holds the engine's threads in the wake callback that follows an event,
/// until the test opens it: by default every worker of an analysis at its
/// first progress tick (the analysis names its workers
/// [`WORKER_THREAD_PREFIX`]`<n>`), or the threads and moments that
/// [`Gate::holding`] says.
struct Gate {
    open: Mutex<bool>,
    opened: Condvar,
    /// Whether the wake that runs now waits for the gate.
    hold: Box<dyn Fn() -> bool + Send + Sync>,
}

impl Default for Gate {
    fn default() -> Self {
        Self::holding(|| on_thread(WORKER_THREAD_PREFIX))
    }
}

/// Whether the current thread's name starts with `prefix`.
fn on_thread(prefix: &str) -> bool {
    thread::current()
        .name()
        .is_some_and(|name| name.starts_with(prefix))
}

impl Gate {
    /// A closed gate that holds a wake when `hold` says so.
    fn holding(hold: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self {
            open: Mutex::new(false),
            opened: Condvar::new(),
            hold: Box::new(hold),
        }
    }

    fn wake(self: &Arc<Self>) -> Wake {
        let gate = Arc::clone(self);
        Arc::new(move || {
            if (gate.hold)() {
                gate.pass();
            }
        })
    }

    /// Waits until the gate is open.
    fn pass(&self) {
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.opened.wait(open).unwrap();
        }
    }

    fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.opened.notify_all();
    }

    /// Holds the workers of the next analysis again.
    fn close(&self) {
        *self.open.lock().unwrap() = false;
    }
}

/// Opens its gate when dropped, so a failing test never leaves the
/// analysis workers held while the engine joins them. Declared after the
/// engine, it is dropped before it.
struct OpenOnDrop(Arc<Gate>);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

// --- The database, read directly ---

/// Makes the database at `db` record every exam put (insert or update) in a
/// `test_puts` table, before an engine opens it.
fn record_exam_puts(db: &Path) {
    drop(Store::open(db).unwrap());
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute_batch(
        "CREATE TABLE test_puts (n INTEGER PRIMARY KEY, role TEXT, path BLOB);
         CREATE TRIGGER test_put_insert AFTER INSERT ON exams
           BEGIN INSERT INTO test_puts (role, path) VALUES (new.role, new.path); END;
         CREATE TRIGGER test_put_update AFTER UPDATE ON exams
           BEGIN INSERT INTO test_puts (role, path) VALUES (new.role, new.path); END;",
    )
    .unwrap();
}

/// The paths of the active exams put so far, in order.
fn active_puts(db: &Path) -> Vec<PathBuf> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut statement = conn
        .prepare("SELECT path FROM test_puts WHERE role = 'active' ORDER BY n")
        .unwrap();
    statement
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .unwrap()
        .map(|blob| paths::from_blob(&blob.unwrap()))
        .collect()
}

/// The number of rows of every table that holds a job's data.
fn row_counts(db: &Path) -> BTreeMap<&'static str, i64> {
    // The engine's store registered sqlite-vec for every connection of the
    // process, so this one reads the vec0 tables too.
    let conn = rusqlite::Connection::open(db).unwrap();
    [
        "jobs",
        "job_skips",
        "exams",
        "files",
        "groups",
        "group_members",
        "group_warnings",
        "embeddings_dinov2_small",
        "embeddings_dinov2_base",
    ]
    .into_iter()
    .map(|table| {
        let count = conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        (table, count)
    })
    .collect()
}

// --- Scenarios ---

#[test]
fn submitted_job_reaches_review_pending() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let engine = start(&dirs);
    let events = engine.events();
    let config = JobConfig::new(input.path().to_path_buf());

    let job = engine.submit(config.clone()).unwrap();
    let seen = wait_for(&events, is_status(job, JobStatus::ReviewPending));
    assert_eq!(seen[0], EngineEvent::JobQueued { job });
    assert_eq!(
        status_changes(&seen),
        [(job, JobStatus::Scanning), (job, JobStatus::ReviewPending)]
    );
    assert!(seen.iter().any(is_hash_tick(job)), "{seen:#?}");
    assert!(
        seen.contains(&EngineEvent::GroupsChanged { job }),
        "{seen:#?}"
    );
    assert!(errors(&seen).is_empty(), "{seen:#?}");

    let view = engine.job(job).unwrap();
    assert_eq!(view.id, job);
    assert_eq!(view.status, JobStatus::ReviewPending);
    assert_eq!(view.config, config);
    assert_eq!(view.error, None);
    assert_eq!((view.counts.files, view.counts.errors), (6, 0));
    assert_eq!((view.counts.groups(), view.counts.proposed), (1, 1));
    assert_eq!(engine.jobs().unwrap(), [view]);

    let groups = engine.groups(job).unwrap();
    assert_eq!(groups.len(), 1);
    let group = &groups[0];
    assert_eq!(
        (
            group.position,
            group.status,
            group.origin,
            group.review_reset
        ),
        (0, GroupStatus::Proposed, GroupOrigin::Auto, false)
    );
    assert_eq!(group.members.len(), 5);
    assert_eq!(
        (group.output_path.as_ref(), group.error.as_ref()),
        (None, None)
    );

    let files = engine.files(job).unwrap();
    assert_eq!(files.len(), 6);
    let job_thumbs = dirs.thumbs().join(job.to_string());
    for (idx, file) in files.iter().enumerate() {
        assert_eq!(file.idx, idx);
        assert!(file.path.starts_with(input.path()), "{:?}", file.path);
        assert_eq!((file.error.as_ref(), file.pinned), (None, false));
        let thumb = file.thumb.as_ref().unwrap();
        assert!(
            thumb.starts_with(&job_thumbs) && thumb.is_file(),
            "{thumb:?}"
        );
    }
    let lone = files
        .iter()
        .position(|file| file.path.ends_with("lone_00.jpg"))
        .unwrap();
    assert_eq!(engine.unstacked(job).unwrap(), [lone]);
}

#[test]
fn engine_analysis_matches_analyze() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    // A RAW+JPEG pair whose RAW is a decoy (JPEG bytes named .cr3), so the
    // JPEG is promoted over it; and a file that is no image.
    let jpeg = jpeg_with_exif(&scene(7, WIDTH, HEIGHT), &canon_exif(30, 1).build(), 90);
    fs::write(input.path().join("pair.jpg"), &jpeg).unwrap();
    fs::write(input.path().join("pair.cr3"), &jpeg).unwrap();
    fs::write(input.path().join("broken.jpg"), b"not a jpeg").unwrap();
    let config = JobConfig::new(input.path().to_path_buf());

    let engine = start(&dirs);
    let events = engine.events();
    let job = engine.submit(config.clone()).unwrap();
    wait_for(&events, is_status(job, JobStatus::ReviewPending));

    let fresh = fresh_plan(&config);
    assert!(
        !fresh.paired.is_empty() && !fresh.errors.is_empty(),
        "{fresh:#?}"
    );
    assert_eq!(engine_plan(&engine, job, &config), fresh);
    // The promoted JPEG has its review thumbnail too.
    let files = engine.files(job).unwrap();
    let promoted = files
        .iter()
        .find(|file| file.path.ends_with("pair.jpg"))
        .unwrap();
    assert!(promoted.thumb.as_ref().is_some_and(|thumb| thumb.is_file()));
}

#[test]
fn jobs_run_one_at_a_time_in_fifo_order() {
    let (_data, dirs) = data_dir();
    let inputs: Vec<TempDir> = (0..3)
        .map(|seed| {
            let input = tempfile::tempdir().unwrap();
            write_bracket(input.path(), "brk", seed + 1, 0, 3);
            input
        })
        .collect();
    let engine = start(&dirs);
    let events = engine.events();

    let jobs: Vec<JobId> = inputs
        .iter()
        .map(|input| {
            engine
                .submit(JobConfig::new(input.path().to_path_buf()))
                .unwrap()
        })
        .collect();
    let seen = wait_for(&events, is_status(jobs[2], JobStatus::ReviewPending));

    let expected: Vec<(JobId, JobStatus)> = jobs
        .iter()
        .flat_map(|&job| [(job, JobStatus::Scanning), (job, JobStatus::ReviewPending)])
        .collect();
    assert_eq!(status_changes(&seen), expected);
    // Each job's progress comes while it is the one scanning.
    let mut scanning = None;
    for event in &seen {
        match *event {
            EngineEvent::JobStatusChanged {
                job,
                status: JobStatus::Scanning,
            } => scanning = Some(job),
            EngineEvent::JobProgress { job, .. } => assert_eq!(Some(job), scanning),
            _ => {}
        }
    }
}

#[test]
fn invalid_config_is_rejected_at_submit() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    let engine = start(&dirs);
    let events = engine.events();

    let mut config = JobConfig::new(input.path().to_path_buf());
    config.hamming_threshold = 41;
    assert!(matches!(
        engine.submit(config),
        Err(EngineError::Config(ConfigError::OutOfRange {
            field: "hamming_threshold",
            ..
        }))
    ));
    let missing = JobConfig::new(input.path().join("missing"));
    assert!(matches!(
        engine.submit(missing),
        Err(EngineError::Config(ConfigError::InputNotADirectory(_)))
    ));
    assert_eq!(engine.jobs().unwrap(), []);
    assert_eq!(events.try_recv(), Err(TryRecvError::Empty));
}

#[cfg(unix)]
#[test]
fn non_utf8_folder_is_refused_at_submit() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let (_data, dirs) = data_dir();
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join(OsStr::from_bytes(b"a\xffb"));
    fs::create_dir(&input).unwrap();
    let engine = start(&dirs);

    assert!(matches!(
        engine.submit(JobConfig::new(input)),
        Err(EngineError::NotUtf8(_))
    ));
    assert_eq!(engine.jobs().unwrap(), []);
}

#[test]
fn cancel_during_scan_gives_cancelled() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    write_lone_shots(input.path(), MANY);
    let gate = Arc::new(Gate::default());
    let engine = Engine::start(options(&dirs, gate.wake())).unwrap();
    let _open = OpenOnDrop(Arc::clone(&gate));
    let events = engine.events();

    let job = engine
        .submit(JobConfig::new(input.path().to_path_buf()))
        .unwrap();
    wait_for(&events, is_hash_tick(job));
    engine.cancel(job).unwrap();
    gate.open();
    let seen = wait_for(&events, is_status(job, JobStatus::Cancelled));
    assert!(errors(&seen).is_empty(), "{seen:#?}");

    let view = engine.job(job).unwrap();
    assert_eq!((view.status, view.error), (JobStatus::Cancelled, None));
    // The scan stopped before its analysis was saved.
    assert_eq!((view.counts.files, view.counts.groups()), (0, 0));
    assert!(engine.groups(job).unwrap().is_empty());
    // Cancelling it again changes nothing.
    engine.cancel(job).unwrap();
    assert_eq!(engine.job(job).unwrap().status, JobStatus::Cancelled);
}

#[test]
fn delete_during_scan_removes_everything_but_inputs() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    write_lone_shots(input.path(), MANY);
    let before = snapshot(input.path());
    let gate = Arc::new(Gate::default());
    let engine = Engine::start(options(&dirs, gate.wake())).unwrap();
    let _open = OpenOnDrop(Arc::clone(&gate));
    let events = engine.events();

    let job = engine
        .submit(JobConfig::new(input.path().to_path_buf()))
        .unwrap();
    wait_for(&events, is_hash_tick(job));
    let job_thumbs = dirs.thumbs().join(job.to_string());
    assert!(job_thumbs.is_dir(), "the first put wrote a thumbnail");
    engine.delete(job).unwrap();
    gate.open();
    let seen = wait_for(&events, |event| *event == EngineEvent::JobDeleted { job });
    // The worker stopped the scan and deleted the job, and nothing failed on
    // the rows that vanished.
    assert!(errors(&seen).is_empty(), "{seen:#?}");
    assert!(
        !status_changes(&seen).contains(&(job, JobStatus::Cancelled)),
        "{seen:#?}"
    );

    assert!(matches!(engine.job(job), Err(EngineError::UnknownJob(id)) if id == job));
    assert_eq!(engine.jobs().unwrap(), []);
    assert!(!job_thumbs.exists());
    let rows = row_counts(&dirs.db());
    assert!(rows.values().all(|&count| count == 0), "{rows:?}");
    assert_eq!(snapshot(input.path()), before);
    assert!(matches!(
        engine.delete(job),
        Err(EngineError::UnknownJob(_))
    ));

    // The worker goes on with the next job.
    let next = bracket_folder();
    let next_job = engine
        .submit(JobConfig::new(next.path().to_path_buf()))
        .unwrap();
    let seen = wait_for(&events, is_status(next_job, JobStatus::ReviewPending));
    assert!(errors(&seen).is_empty(), "{seen:#?}");
}

#[test]
fn delete_of_a_finished_job_is_immediate() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let before = snapshot(input.path());
    let engine = start(&dirs);
    let events = engine.events();
    let job = engine
        .submit(JobConfig::new(input.path().to_path_buf()))
        .unwrap();
    wait_for(&events, is_status(job, JobStatus::ReviewPending));

    engine.delete(job).unwrap();
    assert_eq!(events.try_recv(), Ok(EngineEvent::JobDeleted { job }));
    assert!(!dirs.thumbs().join(job.to_string()).exists());
    let rows = row_counts(&dirs.db());
    assert!(rows.values().all(|&count| count == 0), "{rows:?}");
    assert_eq!(snapshot(input.path()), before);
}

#[test]
fn queued_jobs_are_cancelled_or_deleted_at_once() {
    let (_data, dirs) = data_dir();
    let held = tempfile::tempdir().unwrap();
    write_lone_shots(held.path(), 2);
    let queued = bracket_folder();
    let gate = Arc::new(Gate::default());
    let engine = Engine::start(options(&dirs, gate.wake())).unwrap();
    let _open = OpenOnDrop(Arc::clone(&gate));
    let events = engine.events();

    // The first job is held scanning while the others wait in the queue.
    let first = engine
        .submit(JobConfig::new(held.path().to_path_buf()))
        .unwrap();
    wait_for(&events, is_hash_tick(first));
    let config = JobConfig::new(queued.path().to_path_buf());
    let cancelled = engine.submit(config.clone()).unwrap();
    let deleted = engine.submit(config.clone()).unwrap();
    engine.cancel(cancelled).unwrap();
    assert_eq!(engine.job(cancelled).unwrap().status, JobStatus::Cancelled);
    engine.delete(deleted).unwrap();
    assert!(matches!(
        engine.job(deleted),
        Err(EngineError::UnknownJob(_))
    ));
    gate.open();

    let seen = wait_for(&events, is_status(first, JobStatus::ReviewPending));
    let changes = status_changes(&seen);
    assert!(
        changes.contains(&(cancelled, JobStatus::Cancelled)),
        "{seen:#?}"
    );
    assert!(seen.contains(&EngineEvent::JobDeleted { job: deleted }));
    assert!(
        changes
            .iter()
            .all(|&(job, status)| job == first || status != JobStatus::Scanning),
        "{seen:#?}"
    );
    assert!(matches!(
        engine.cancel(deleted),
        Err(EngineError::UnknownJob(_))
    ));

    // A cancelled job is scanned when it is retried.
    engine.retry(cancelled).unwrap();
    wait_for(&events, is_status(cancelled, JobStatus::ReviewPending));
    assert_eq!(
        engine_plan(&engine, cancelled, &config),
        fresh_plan(&config)
    );
}

#[test]
fn scan_failure_sets_failed_with_the_message() {
    let (_data, dirs) = data_dir();
    let held = tempfile::tempdir().unwrap();
    write_lone_shots(held.path(), 2);
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("input");
    fs::create_dir(&input).unwrap();
    let gate = Arc::new(Gate::default());
    let engine = Engine::start(options(&dirs, gate.wake())).unwrap();
    let _open = OpenOnDrop(Arc::clone(&gate));
    let events = engine.events();

    let first = engine
        .submit(JobConfig::new(held.path().to_path_buf()))
        .unwrap();
    wait_for(&events, is_hash_tick(first));
    // The folder goes away while its job waits.
    let job = engine.submit(JobConfig::new(input.clone())).unwrap();
    fs::remove_dir(&input).unwrap();
    gate.open();

    let seen = wait_for(&events, |event| matches!(event, EngineEvent::Error { .. }));
    let message = format!("input is not a directory: {}", input.display());
    assert_eq!(
        seen.last(),
        Some(&EngineEvent::Error {
            job: Some(job),
            message: message.clone()
        })
    );
    assert!(
        status_changes(&seen).ends_with(&[(job, JobStatus::Scanning), (job, JobStatus::Failed)])
    );
    let view = engine.job(job).unwrap();
    assert_eq!(
        (view.status, view.error),
        (JobStatus::Failed, Some(message))
    );

    // Retried once the folder is back, it scans, and its error is cleared.
    fs::create_dir(&input).unwrap();
    engine.retry(job).unwrap();
    wait_for(&events, is_status(job, JobStatus::ReviewPending));
    assert_eq!(engine.job(job).unwrap().error, None);
}

#[test]
fn retry_rescans_a_cancelled_job_from_its_checkpoints() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    write_lone_shots(input.path(), MANY);
    let config = JobConfig::new(input.path().to_path_buf());
    record_exam_puts(&dirs.db());
    let gate = Arc::new(Gate::default());
    let engine = Engine::start(options(&dirs, gate.wake())).unwrap();
    let _open = OpenOnDrop(Arc::clone(&gate));
    let events = engine.events();

    let job = engine.submit(config.clone()).unwrap();
    wait_for(&events, is_hash_tick(job));
    engine.cancel(job).unwrap();
    gate.open();
    wait_for(&events, is_status(job, JobStatus::Cancelled));
    let first = active_puts(&dirs.db());
    assert!(!first.is_empty() && first.len() < MANY, "{}", first.len());

    engine.retry(job).unwrap();
    let seen = wait_for(&events, is_status(job, JobStatus::ReviewPending));
    assert_eq!(
        status_changes(&seen),
        [
            (job, JobStatus::Queued),
            (job, JobStatus::Scanning),
            (job, JobStatus::ReviewPending)
        ]
    );
    let all = active_puts(&dirs.db());
    let again: BTreeSet<&PathBuf> = all[first.len()..].iter().collect();
    let before: BTreeSet<&PathBuf> = first.iter().collect();
    assert!(again.is_disjoint(&before), "{again:?} were examined again");
    assert_eq!(before.len() + again.len(), MANY);
    assert_eq!(engine_plan(&engine, job, &config), fresh_plan(&config));

    // Only a failed or cancelled job is retried.
    assert!(matches!(
        engine.retry(job),
        Err(EngineError::WrongState {
            status: JobStatus::ReviewPending,
            ..
        })
    ));
    assert!(matches!(
        engine.retry(job + 1),
        Err(EngineError::UnknownJob(_))
    ));
}

#[test]
fn shutdown_mid_scan_leaves_the_job_scanning() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    write_lone_shots(input.path(), MANY);
    let gate = Arc::new(Gate::default());
    let engine = Engine::start(options(&dirs, gate.wake())).unwrap();
    let events = engine.events();
    let job = engine
        .submit(JobConfig::new(input.path().to_path_buf()))
        .unwrap();
    wait_for(&events, is_hash_tick(job));

    // The shutdown sets the quit flag at once, then waits for the held
    // workers, which the gate lets go well after that.
    let opener = {
        let gate = Arc::clone(&gate);
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(2));
            gate.open();
        })
    };
    engine.shutdown();
    opener.join().unwrap();

    let remaining: Vec<EngineEvent> = events.try_iter().collect();
    assert!(status_changes(&remaining).is_empty(), "{remaining:#?}");
    let store = Store::open_read_only(&dirs.db()).unwrap();
    let stored = store.job(job).unwrap().unwrap();
    assert_eq!(
        (stored.status, stored.scan_done),
        (JobStatus::Scanning, false)
    );
    assert!(store.load_analysis(job).unwrap().files.is_empty());
    // The files examined before the quit are checkpoints for the resume.
    let exams: i64 = rusqlite::Connection::open(dirs.db())
        .unwrap()
        .query_row("SELECT count(*) FROM exams", [], |row| row.get(0))
        .unwrap();
    assert!(exams > 0);
}

#[test]
fn wake_is_called_after_events() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    // At each wake, how many events were waiting; nothing reads them until
    // the end.
    let receiver: Arc<OnceLock<Receiver<EngineEvent>>> = Arc::default();
    let waiting: Arc<Mutex<Vec<usize>>> = Arc::default();
    let wake: Wake = {
        let receiver = Arc::clone(&receiver);
        let waiting = Arc::clone(&waiting);
        Arc::new(move || {
            let count = receiver.get().map_or(0, Receiver::len);
            waiting.lock().unwrap().push(count);
        })
    };
    let engine = Engine::start(options(&dirs, wake)).unwrap();
    let events = engine.events();
    receiver.set(engine.events()).unwrap();

    let job = engine
        .submit(JobConfig::new(input.path().to_path_buf()))
        .unwrap();
    poll_status(&engine, job, JobStatus::ReviewPending);
    engine.shutdown();

    let waiting = waiting.lock().unwrap();
    assert_eq!(waiting.len(), events.len(), "one wake per event");
    for (calls, &count) in waiting.iter().enumerate() {
        assert!(count > calls, "wake {calls} saw {count} events");
    }
}

#[test]
fn idle_engine_sends_no_events() {
    let (_data, dirs) = data_dir();
    let wakes = Arc::new(AtomicUsize::new(0));
    let wake: Wake = {
        let wakes = Arc::clone(&wakes);
        Arc::new(move || {
            wakes.fetch_add(1, Ordering::SeqCst);
        })
    };
    let engine = Engine::start(options(&dirs, wake)).unwrap();
    let events = engine.events();

    thread::sleep(Duration::from_millis(300));
    assert_eq!(events.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(engine.jobs().unwrap(), []);
    engine.shutdown();
    assert_eq!(wakes.load(Ordering::SeqCst), 0);
    assert!(events.is_empty());
}

// --- Review and writing ---

/// The Hamming threshold at which `b_3` of [`review_folder`] joins `b_1`
/// and `b_2`: their distance is 16.
const NEAR: u32 = 20;
/// The default time window (spec §5.2), which the threshold edits of these
/// tests keep.
const WINDOW: u32 = 30;

/// Five photos without EXIF, so one bucket by their modification times:
/// `a_1` and `a_2` (one scene, distance 0), `b_1` and `b_2` (another scene,
/// distance 0), and `b_3`, the second scene panned, 16 from `b_1` and `b_2`.
/// At the default threshold of 8 the groups are `{a_1, a_2}` and
/// `{b_1, b_2}`, with `b_3` in the tray; at [`NEAR`], `b_3` joins `b`.
fn review_folder() -> TempDir {
    let input = tempfile::tempdir().unwrap();
    write_review_photos(input.path(), 1, 2);
    input
}

/// A job in Review Pending, and its files.
struct Review {
    engine: Engine,
    events: Receiver<EngineEvent>,
    job: JobId,
    files: Vec<FileView>,
    /// The events of the scan, up to its Review Pending.
    scanned: Vec<EngineEvent>,
}

impl Review {
    /// Scans `config` on a new engine and waits for its review.
    fn start(dirs: &AppDirs, config: JobConfig) -> Review {
        Review::on(start(dirs), config)
    }

    /// Scans `config` on `engine` and waits for its review.
    fn on(engine: Engine, config: JobConfig) -> Review {
        let events = engine.events();
        let job = engine.submit(config).unwrap();
        let scanned = wait_for(&events, is_status(job, JobStatus::ReviewPending));
        assert!(errors(&scanned).is_empty(), "{scanned:#?}");
        let files = engine.files(job).unwrap();
        Review {
            engine,
            events,
            job,
            files,
            scanned,
        }
    }

    /// The id of the file named `name`.
    fn id(&self, name: &str) -> usize {
        self.files
            .iter()
            .position(|file| file.path.file_name().is_some_and(|n| n == name))
            .unwrap_or_else(|| panic!("no file {name}"))
    }

    fn groups(&self) -> Vec<GroupView> {
        self.engine.groups(self.job).unwrap()
    }

    /// The group that holds the file named `name`, if one does.
    fn group_of(&self, name: &str) -> Option<GroupView> {
        let id = self.id(name);
        self.groups()
            .into_iter()
            .find(|group| group.members.contains(&id))
    }

    fn group(&self, name: &str) -> GroupView {
        self.group_of(name)
            .unwrap_or_else(|| panic!("{name} is in no group: {:#?}", self.groups()))
    }

    /// The names of `members`, sorted.
    fn names(&self, members: &[usize]) -> Vec<String> {
        let mut names: Vec<String> = members
            .iter()
            .map(|&id| {
                let name = self.files[id].path.file_name().unwrap();
                name.to_string_lossy().into_owned()
            })
            .collect();
        names.sort();
        names
    }

    /// The names of the files in the tray, sorted.
    fn tray(&self) -> Vec<String> {
        self.names(&self.engine.unstacked(self.job).unwrap())
    }

    /// The names of the pinned files, sorted.
    fn pinned(&self) -> Vec<String> {
        let files = self.engine.files(self.job).unwrap();
        let pinned: Vec<usize> = files
            .iter()
            .filter(|file| file.pinned)
            .map(|file| file.idx)
            .collect();
        self.names(&pinned)
    }

    /// Drops the events received so far.
    fn drain(&self) {
        while self.events.try_recv().is_ok() {}
    }

    fn move_file(&self, name: &str, to: MoveTarget) {
        self.engine.move_file(self.job, self.id(name), to).unwrap();
    }

    /// Queues the write and waits for the job to end in `status`.
    fn write_until(&self, status: JobStatus) -> Vec<EngineEvent> {
        self.engine.write(self.job).unwrap();
        wait_for(&self.events, is_status(self.job, status))
    }
}

/// The partial and output paths of every group, read from the database.
fn write_state(db: &Path) -> Vec<(Option<PathBuf>, Option<PathBuf>)> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut statement = conn
        .prepare("SELECT partial_path, output_path FROM groups ORDER BY id")
        .unwrap();
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, Option<Vec<u8>>>(0)?,
                row.get::<_, Option<Vec<u8>>>(1)?,
            ))
        })
        .unwrap()
        .map(|row| {
            let (partial, output) = row.unwrap();
            (
                partial.as_deref().map(paths::from_blob),
                output.as_deref().map(paths::from_blob),
            )
        })
        .collect()
}

/// The names of the files in `dir`, sorted; none when it does not exist.
fn file_names(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn confirm_survives_an_unchanged_regroup() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let review = Review::start(&dirs, JobConfig::new(input.path().to_path_buf()));
    let (engine, job) = (&review.engine, review.job);
    let a = review.group("a_1.jpg");
    assert_eq!(review.names(&a.members), ["a_1.jpg", "a_2.jpg"]);
    assert_eq!(review.tray(), ["b_3.jpg"]);

    engine.confirm(job, a.id).unwrap();
    assert_eq!(
        review.events.try_recv(),
        Ok(EngineEvent::GroupsChanged { job })
    );
    engine.set_thresholds(job, NEAR, WINDOW, Some(0.9)).unwrap();
    assert_eq!(
        review.events.try_recv(),
        Ok(EngineEvent::GroupsChanged { job })
    );

    // `a` is unchanged, so its row and its decision stay.
    let kept = review.group("a_1.jpg");
    assert_eq!(
        (kept.id, kept.status, kept.review_reset, kept.origin),
        (a.id, GroupStatus::Confirmed, false, GroupOrigin::Auto)
    );
    assert_eq!(review.tray(), Vec::<String>::new());
    let config = engine.job(job).unwrap().config;
    assert_eq!(
        (config.hamming_threshold, config.cosine_threshold),
        (NEAR, Some(0.9))
    );
    // Flipping a decision works both ways.
    engine.reject(job, a.id).unwrap();
    assert_eq!(review.group("a_1.jpg").status, GroupStatus::Rejected);
    engine.confirm(job, a.id).unwrap();
    assert_eq!(review.group("a_1.jpg").status, GroupStatus::Confirmed);
}

#[test]
fn changed_group_resets_to_proposed_with_the_flag() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let review = Review::start(&dirs, JobConfig::new(input.path().to_path_buf()));
    let (engine, job) = (&review.engine, review.job);
    let b = review.group("b_1.jpg");
    engine.confirm(job, b.id).unwrap();
    engine.reject_all(job).unwrap();
    assert_eq!(review.group("a_1.jpg").status, GroupStatus::Rejected);

    engine.set_thresholds(job, NEAR, WINDOW, None).unwrap();
    let grown = review.group("b_1.jpg");
    assert_eq!(
        review.names(&grown.members),
        ["b_1.jpg", "b_2.jpg", "b_3.jpg"]
    );
    assert_ne!(grown.id, b.id);
    assert_eq!(
        (grown.status, grown.review_reset, grown.origin),
        (GroupStatus::Proposed, true, GroupOrigin::Auto)
    );
    assert!(review.groups().iter().all(|group| group.id != b.id));
    // A group's own regrouping does not touch the others.
    let a = review.group("a_1.jpg");
    assert_eq!((a.status, a.review_reset), (GroupStatus::Rejected, false));

    // Back at the default, b's members were last in a group nobody decided, but
    // flagged: the stack that replaces it is still to look at again (ruling
    // T16-b), until it is decided.
    engine.set_thresholds(job, 8, WINDOW, None).unwrap();
    let shrunk = review.group("b_1.jpg");
    assert_eq!(review.names(&shrunk.members), ["b_1.jpg", "b_2.jpg"]);
    assert_eq!(
        (shrunk.status, shrunk.review_reset),
        (GroupStatus::Proposed, true)
    );
    engine.confirm_all(job).unwrap();
    let decided = review.group("b_1.jpg");
    assert_eq!(
        (decided.status, decided.review_reset),
        (GroupStatus::Confirmed, false)
    );
    assert_eq!(review.group("a_1.jpg").status, GroupStatus::Rejected);
}

#[test]
fn look_again_survives_slider_moves_until_the_stack_is_decided() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let review = Review::start(&dirs, JobConfig::new(input.path().to_path_buf()));
    let (engine, job) = (&review.engine, review.job);
    let flag = |name: &str| {
        let group = review.group(name);
        (group.status, group.review_reset)
    };
    engine.confirm(job, review.group("b_1.jpg").id).unwrap();

    // b grows past its confirmed members: a new stack to look at again.
    engine.set_thresholds(job, NEAR, WINDOW, None).unwrap();
    let grown = review.group("b_1.jpg");
    assert_eq!(review.names(&grown.members).len(), 3);
    assert_eq!(flag("b_1.jpg"), (GroupStatus::Proposed, true));
    // Regroupings that keep its members keep it, and its flag: another
    // threshold, and an edit of another stack (a goes to the tray whole).
    engine.set_thresholds(job, NEAR - 1, WINDOW, None).unwrap();
    review.move_file("a_2.jpg", MoveTarget::Tray);
    let kept = review.group("b_1.jpg");
    assert_eq!(kept.id, grown.id);
    assert_eq!(flag("b_1.jpg"), (GroupStatus::Proposed, true));
    // A decision clears it.
    engine.confirm(job, grown.id).unwrap();
    assert_eq!(flag("b_1.jpg"), (GroupStatus::Confirmed, false));

    // Back at 8, b shrinks out of its confirmed self: flagged again, and a
    // rejection clears it.
    engine.set_thresholds(job, 8, WINDOW, None).unwrap();
    assert_eq!(flag("b_1.jpg"), (GroupStatus::Proposed, true));
    engine.reject(job, review.group("b_1.jpg").id).unwrap();
    assert_eq!(flag("b_1.jpg"), (GroupStatus::Rejected, false));

    // Grown again out of a rejected stack: flagged, and Confirm all clears it.
    engine.set_thresholds(job, NEAR, WINDOW, None).unwrap();
    assert_eq!(flag("b_1.jpg"), (GroupStatus::Proposed, true));
    engine.confirm_all(job).unwrap();
    assert_eq!(flag("b_1.jpg"), (GroupStatus::Confirmed, false));
}

/// Puts a job straight into the store of `dirs`, in review with no group yet:
/// `photos` without EXIF, all in one folder (so one bucket), each named and with
/// its pHash (its dHash 0), so the hash distances between them are exactly the
/// bits their pHashes differ by. Call it before an engine starts on `dirs`.
fn stored_job(dirs: &AppDirs, photos: &[(String, u64)]) -> JobId {
    fs::create_dir_all(&dirs.data).unwrap();
    let mut store = Store::open(&dirs.db()).unwrap();
    let input = PathBuf::from("/stored");
    let job = store.insert_job(&JobConfig::new(input.clone())).unwrap();
    let files = photos
        .iter()
        .enumerate()
        .map(|(i, (name, phash))| {
            let hashes = HashPair {
                phash: *phash,
                dhash: 0,
            };
            analyze::FileRecord {
                path: input.join(name),
                kind: FileKind::Jpeg,
                size: 1_000,
                mtime_ms: i as i64 * 1_000,
                fingerprint: format!("stored {i}"),
                exif: None,
                hashes: Some(hashes),
                normalised: Some(hashes),
                paired_with: None,
                error: None,
            }
        })
        .collect();
    let analysis = analyze::Analysis {
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

#[test]
fn look_again_follows_a_stack_through_a_drag_until_it_is_decided() {
    use GroupStatus::{Confirmed, Proposed};
    let (_data, dirs) = data_dir();
    // A chain: 0 and 1 alike, 2 six bits from them, 3 twelve from 2 (18 from
    // them), 4 two from 3. `b` is decided on; `u`, the same chain far from it,
    // never is.
    let chain = [0, 0, 0x3F, 0x3_FFFF, 0xF_FFFF];
    let far = 0xFFFF_FFFF_0000_0000_u64;
    let photos: Vec<(String, u64)> = ["b", "u"]
        .into_iter()
        .flat_map(|prefix| {
            chain.iter().enumerate().map(move |(i, &bits)| {
                let base = if prefix == "u" { far } else { 0 };
                (format!("{prefix}{i}.jpg"), base | bits)
            })
        })
        .collect();
    let job = stored_job(&dirs, &photos);
    let engine = start(&dirs);
    let files = engine.files(job).unwrap();
    let name = |id: usize| {
        files[id]
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    };
    let stacks = || {
        let mut stacks: Vec<(String, GroupStatus, bool)> = engine
            .groups(job)
            .unwrap()
            .into_iter()
            .map(|group| {
                let mut names: Vec<String> = group.members.iter().map(|&id| name(id)).collect();
                names.sort();
                (names.join(" "), group.status, group.review_reset)
            })
            .collect();
        stacks.sort_by(|a, b| a.0.cmp(&b.0));
        stacks
    };
    let stack = |s: &str, status, flag| (s.to_owned(), status, flag);
    let id_of = |members: &str| {
        let groups = engine.groups(job).unwrap();
        groups
            .into_iter()
            .find(|group| {
                let mut names: Vec<String> = group.members.iter().map(|&id| name(id)).collect();
                names.sort();
                names.join(" ") == members
            })
            .unwrap_or_else(|| panic!("no stack {members}"))
            .id
    };

    engine.set_thresholds(job, 4, WINDOW, None).unwrap();
    engine.confirm(job, id_of("b0.jpg b1.jpg")).unwrap();
    assert_eq!(
        stacks(),
        [
            stack("b0.jpg b1.jpg", Confirmed, false),
            stack("b3.jpg b4.jpg", Proposed, false),
            stack("u0.jpg u1.jpg", Proposed, false),
            stack("u3.jpg u4.jpg", Proposed, false),
        ]
    );
    // A drag up: b grows once, then again. The flag of each b goes to the one
    // that replaces it, however many steps the drag ran.
    engine.set_thresholds(job, 8, WINDOW, None).unwrap();
    assert_eq!(
        stacks(),
        [
            stack("b0.jpg b1.jpg b2.jpg", Proposed, true),
            stack("b3.jpg b4.jpg", Proposed, false),
            stack("u0.jpg u1.jpg u2.jpg", Proposed, false),
            stack("u3.jpg u4.jpg", Proposed, false),
        ]
    );
    engine.set_thresholds(job, 14, WINDOW, None).unwrap();
    assert_eq!(
        stacks(),
        [
            stack("b0.jpg b1.jpg b2.jpg b3.jpg b4.jpg", Proposed, true),
            stack("u0.jpg u1.jpg u2.jpg u3.jpg u4.jpg", Proposed, false),
        ]
    );
    // Back down, b splits into two stacks, neither of them the confirmed one:
    // both to look at again. u, whose stacks nobody decided, never is.
    engine.set_thresholds(job, 8, WINDOW, None).unwrap();
    assert_eq!(
        stacks(),
        [
            stack("b0.jpg b1.jpg b2.jpg", Proposed, true),
            stack("b3.jpg b4.jpg", Proposed, true),
            stack("u0.jpg u1.jpg u2.jpg", Proposed, false),
            stack("u3.jpg u4.jpg", Proposed, false),
        ]
    );
    // Deciding one clears its flag alone.
    engine.confirm(job, id_of("b3.jpg b4.jpg")).unwrap();
    assert_eq!(
        stacks(),
        [
            stack("b0.jpg b1.jpg b2.jpg", Proposed, true),
            stack("b3.jpg b4.jpg", Confirmed, false),
            stack("u0.jpg u1.jpg u2.jpg", Proposed, false),
            stack("u3.jpg u4.jpg", Proposed, false),
        ]
    );
}

#[test]
fn manual_group_is_frozen_against_sliders() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let review = Review::start(&dirs, JobConfig::new(input.path().to_path_buf()));
    let (engine, job) = (&review.engine, review.job);
    let b = review.group("b_1.jpg");
    engine.confirm(job, b.id).unwrap();

    // Moving b_3 in makes b manual and Proposed again, under its own id.
    review.move_file("b_3.jpg", MoveTarget::Group(b.id));
    let manual = review.group("b_1.jpg");
    assert_eq!(
        (manual.id, manual.origin, manual.status, manual.review_reset),
        (b.id, GroupOrigin::Manual, GroupStatus::Proposed, false)
    );
    assert_eq!(
        review.names(&manual.members),
        ["b_1.jpg", "b_2.jpg", "b_3.jpg"]
    );
    engine.confirm(job, b.id).unwrap();

    for hamming in [0, 40, 8] {
        engine.set_thresholds(job, hamming, WINDOW, None).unwrap();
        let frozen = review.group("b_1.jpg");
        assert_eq!(
            (frozen.id, frozen.origin, frozen.status, &frozen.members),
            (
                b.id,
                GroupOrigin::Manual,
                GroupStatus::Confirmed,
                &manual.members
            ),
            "hamming {hamming}"
        );
        assert_eq!(review.names(&review.group("a_1.jpg").members).len(), 2);
    }
    // The manual group comes after the automatic one.
    let positions: Vec<(usize, GroupOrigin)> = review
        .groups()
        .iter()
        .map(|group| (group.position, group.origin))
        .collect();
    assert_eq!(
        positions,
        [(0, GroupOrigin::Auto), (1, GroupOrigin::Manual)]
    );
}

#[test]
fn photo_moved_to_the_tray_stays_there() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let review = Review::start(&dirs, JobConfig::new(input.path().to_path_buf()));
    let (engine, job) = (&review.engine, review.job);
    engine.set_thresholds(job, NEAR, WINDOW, None).unwrap();
    let b = review.group("b_1.jpg");
    assert_eq!(b.members.len(), 3);

    review.move_file("b_3.jpg", MoveTarget::Tray);
    assert_eq!(
        (review.tray(), review.pinned()),
        (vec!["b_3.jpg".to_string()], vec!["b_3.jpg".to_string()])
    );
    let left = review.group("b_1.jpg");
    assert_eq!(
        (left.id, left.origin, review.names(&left.members)),
        (
            b.id,
            GroupOrigin::Manual,
            vec!["b_1.jpg".into(), "b_2.jpg".into()]
        )
    );
    // It would link at this threshold, but stays in the tray.
    engine.set_thresholds(job, 40, WINDOW, None).unwrap();
    assert_eq!(review.tray(), ["b_3.jpg"]);
    assert!(review.group_of("b_3.jpg").is_none());
    // Moving it to the tray again changes nothing.
    review.drain();
    review.move_file("b_3.jpg", MoveTarget::Tray);
    assert_eq!(review.events.try_recv(), Err(TryRecvError::Empty));

    // Out of the tray, it is unpinned.
    review.move_file("b_3.jpg", MoveTarget::Group(b.id));
    assert_eq!(review.group("b_3.jpg").id, b.id);
    assert_eq!(review.pinned(), Vec::<String>::new());

    // A group left with one photo dissolves: both photos stay in the tray.
    let a = review.group("a_1.jpg");
    review.move_file("a_2.jpg", MoveTarget::Tray);
    assert!(review.groups().iter().all(|group| group.id != a.id));
    assert_eq!(review.pinned(), ["a_1.jpg", "a_2.jpg"]);
    engine.set_thresholds(job, 8, WINDOW, None).unwrap();
    assert_eq!(review.tray(), ["a_1.jpg", "a_2.jpg"]);

    // A photo moved to a new group is alone in it, until another joins.
    review.move_file("a_1.jpg", MoveTarget::NewGroup);
    let alone = review.group("a_1.jpg");
    assert_eq!(
        (alone.origin, alone.members.len()),
        (GroupOrigin::Manual, 1)
    );
    review.move_file("a_2.jpg", MoveTarget::Group(alone.id));
    assert_eq!(review.group("a_2.jpg").id, alone.id);
    assert_eq!(review.pinned(), Vec::<String>::new());
}

#[test]
fn merge_and_split_make_manual_groups() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let review = Review::start(&dirs, JobConfig::new(input.path().to_path_buf()));
    let (engine, job) = (&review.engine, review.job);
    let (a, b) = (review.group("a_1.jpg"), review.group("b_1.jpg"));
    engine.confirm_all(job).unwrap();

    for groups in [vec![a.id], vec![a.id, a.id], vec![]] {
        assert_invalid(engine.merge(job, &groups), "a merge needs 2 stacks or more");
    }
    engine.merge(job, &[a.id, b.id]).unwrap();
    let groups = review.groups();
    assert_eq!(groups.len(), 1, "{groups:#?}");
    let merged = &groups[0];
    assert_eq!(
        (merged.id, merged.origin, merged.status),
        (a.id, GroupOrigin::Manual, GroupStatus::Proposed)
    );
    assert_eq!(
        review.names(&merged.members),
        ["a_1.jpg", "a_2.jpg", "b_1.jpg", "b_2.jpg"]
    );

    let (a_1, a_2, b_1) = (
        review.id("a_1.jpg"),
        review.id("a_2.jpg"),
        review.id("b_1.jpg"),
    );
    // A refusal names the photo and the stack as the user sees them.
    let too_few = "a split must leave 2 photos or more on each side";
    for (files, refusal) in [
        (vec![a_1], too_few),
        (vec![a_1, a_2, b_1], too_few),
        (
            vec![a_1, review.id("b_3.jpg")],
            "the photo b_3.jpg is not in stack 1",
        ),
    ] {
        assert_invalid(engine.split(job, a.id, &files), refusal);
    }
    engine.split(job, a.id, &[a_2, a_1]).unwrap();
    let rest = review.group("b_1.jpg");
    let part = review.group("a_1.jpg");
    assert_eq!(rest.id, a.id);
    assert_ne!(part.id, a.id);
    assert_eq!(review.names(&rest.members), ["b_1.jpg", "b_2.jpg"]);
    assert_eq!(review.names(&part.members), ["a_1.jpg", "a_2.jpg"]);
    for group in [&rest, &part] {
        assert_eq!(
            (group.origin, group.status),
            (GroupOrigin::Manual, GroupStatus::Proposed)
        );
    }
    assert!(matches!(
        engine.split(job, b.id, &[a_1, a_2]),
        Err(EngineError::UnknownGroup(id)) if id == b.id
    ));
}

#[test]
fn edits_never_put_two_folders_in_one_group() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    for (folder, seed) in [("a", 1), ("b", 2)] {
        let dir = input.path().join(folder);
        fs::create_dir(&dir).unwrap();
        let image = scene(seed, WIDTH, HEIGHT);
        for (name, image) in [("1", image.clone()), ("2", expose(&image, 0.3))] {
            let bytes = plain_jpeg(&image);
            fs::write(dir.join(format!("{folder}_{name}.jpg")), bytes).unwrap();
        }
    }
    let mut config = JobConfig::new(input.path().to_path_buf());
    config.recursive = true;
    let review = Review::start(&dirs, config);
    let (engine, job) = (&review.engine, review.job);
    let (a, b) = (review.group("a_1.jpg"), review.group("b_1.jpg"));
    let groups = review.groups();
    review.drain();

    let a_1 = review.id("a_1.jpg");
    let refusal = "a stack cannot hold photos of two folders";
    assert_invalid(engine.move_file(job, a_1, MoveTarget::Group(b.id)), refusal);
    assert_invalid(engine.merge(job, &[a.id, b.id]), refusal);
    assert_eq!(review.groups(), groups);
    assert_eq!(review.events.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn passive_and_failed_files_cannot_be_moved() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    write_raw_bracket(input.path(), "brk", 5);
    fs::write(input.path().join("broken.jpg"), b"not a jpeg").unwrap();
    let review = Review::start(&dirs, JobConfig::new(input.path().to_path_buf()));
    let (engine, job) = (&review.engine, review.job);
    let bracket = review.group("brk_1.raf");
    let (groups, tray) = (review.groups(), review.tray());
    let files = engine.files(job).unwrap();
    let (passive, failed) = (review.id("brk_2.jpg"), review.id("broken.jpg"));
    assert!(files[passive].paired_with.is_some());
    assert!(files[failed].error.is_some());
    review.drain();

    for (file, name) in [(passive, "brk_2.jpg"), (failed, "broken.jpg")] {
        let refusal = format!(
            "the photo {name} cannot be stacked: it could not be read, or its RAW+JPEG pair leaves it out"
        );
        for to in [
            MoveTarget::Group(bracket.id),
            MoveTarget::NewGroup,
            MoveTarget::Tray,
        ] {
            assert_invalid(engine.move_file(job, file, to), &refusal);
        }
    }
    assert_eq!((review.groups(), review.tray()), (groups, tray));
    assert_eq!(review.events.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn write_writes_only_confirmed_groups() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let config = JobConfig::new(input.path().to_path_buf());
    let review = Review::start(&dirs, config.clone());
    let a = review.group("a_1.jpg");
    let b = review.group("b_1.jpg");
    review.engine.confirm(review.job, a.id).unwrap();
    let before = snapshot(input.path());

    let seen = review.write_until(JobStatus::Completed);
    let job = review.job;
    assert_eq!(
        status_changes(&seen),
        [
            (job, JobStatus::Queued),
            (job, JobStatus::Writing),
            (job, JobStatus::Completed)
        ]
    );
    assert!(errors(&seen).is_empty(), "{seen:#?}");
    let ticks = seen
        .iter()
        .filter(|event| {
            matches!(event, EngineEvent::JobProgress { progress, .. }
                if progress.stage == Stage::Write)
        })
        .count();
    assert_eq!(ticks, a.members.len());

    let written = review.group("a_1.jpg");
    assert_eq!(written.status, GroupStatus::Written);
    let output = written.output_path.unwrap();
    let output_dir = config.output_dir.canonicalize().unwrap();
    assert_eq!(output.parent(), Some(output_dir.as_path()));
    assert_eq!(
        file_names(&config.output_dir),
        [output.file_name().unwrap().to_string_lossy()]
    );
    let proposed = review.group("b_1.jpg");
    assert_eq!(
        (proposed.id, proposed.status, proposed.output_path),
        (b.id, GroupStatus::Proposed, None)
    );
    let counts = review.engine.job(job).unwrap().counts;
    assert_eq!((counts.written, counts.proposed), (1, 1));
    // The inputs are untouched; the stack went to the output folder only.
    let mut after = snapshot(input.path());
    after.retain(|path, _| !path.starts_with(&config.output_dir));
    assert_eq!(after, before);
}

#[test]
fn auto_confirm_writes_everything() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut config = JobConfig::new(input.path().to_path_buf());
    config.auto_confirm = true;
    let engine = start(&dirs);
    let events = engine.events();

    let job = engine.submit(config.clone()).unwrap();
    let seen = wait_for(&events, is_status(job, JobStatus::Completed));
    assert_eq!(
        status_changes(&seen),
        [
            (job, JobStatus::Scanning),
            (job, JobStatus::Queued),
            (job, JobStatus::Writing),
            (job, JobStatus::Completed)
        ]
    );
    assert!(errors(&seen).is_empty(), "{seen:#?}");
    let groups = engine.groups(job).unwrap();
    assert_eq!(groups.len(), 2);
    for group in &groups {
        assert_eq!(group.status, GroupStatus::Written, "{group:#?}");
        assert!(
            group
                .output_path
                .as_ref()
                .is_some_and(|path| path.is_file())
        );
    }
    assert_eq!(file_names(&config.output_dir).len(), 2);
}

#[test]
fn written_group_has_an_output_and_no_partial() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let config = JobConfig::new(input.path().to_path_buf());
    let review = Review::start(&dirs, config.clone());
    review.engine.confirm_all(review.job).unwrap();
    review.write_until(JobStatus::Completed);

    let outputs: Vec<PathBuf> = review
        .groups()
        .into_iter()
        .map(|group| group.output_path.unwrap())
        .collect();
    let state = write_state(&dirs.db());
    assert_eq!(state.len(), 2);
    for (partial, output) in &state {
        assert_eq!(partial, &None);
        let output = output.as_ref().unwrap();
        assert!(outputs.contains(output) && output.is_file(), "{output:?}");
    }
    assert!(
        file_names(&config.output_dir)
            .iter()
            .all(|name| name.ends_with(".psd")),
        "{:?}",
        file_names(&config.output_dir)
    );
}

#[test]
fn failed_group_does_not_stop_the_others() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    for (folder, seed) in [("a", 1), ("b", 2), ("c", 3)] {
        let dir = input.path().join(folder);
        fs::create_dir(&dir).unwrap();
        let image = scene(seed, WIDTH, HEIGHT);
        for (name, image) in [("1", image.clone()), ("2", expose(&image, 0.3))] {
            let bytes = plain_jpeg(&image);
            fs::write(dir.join(format!("{folder}_{name}.jpg")), bytes).unwrap();
        }
    }
    let output = tempfile::tempdir().unwrap();
    let mut config = JobConfig::new(input.path().to_path_buf());
    config.recursive = true;
    config.output_dir = output.path().to_path_buf();
    let review = Review::start(&dirs, config);
    assert_eq!(review.groups().len(), 3);
    review.engine.confirm_all(review.job).unwrap();
    // b's stacks go to `<output>/b`, which is a file; c's photos are gone.
    fs::write(output.path().join("b"), b"in the way").unwrap();
    for name in ["c_1.jpg", "c_2.jpg"] {
        fs::remove_file(input.path().join("c").join(name)).unwrap();
    }

    let seen = review.write_until(JobStatus::Completed);
    assert!(errors(&seen).is_empty(), "{seen:#?}");
    let written = review.group("a_1.jpg");
    assert_eq!(written.status, GroupStatus::Written);
    let output_dir = output.path().canonicalize().unwrap();
    assert!(
        written
            .output_path
            .unwrap()
            .starts_with(output_dir.join("a"))
    );
    let failed = review.group("b_1.jpg");
    assert_eq!(
        (failed.status, failed.output_path.as_ref()),
        (GroupStatus::Failed, None)
    );
    assert!(failed.error.is_some_and(|error| !error.is_empty()));
    let skipped = review.group("c_1.jpg");
    assert_eq!(
        (skipped.status, skipped.output_path.as_ref()),
        (GroupStatus::Skipped, None)
    );
    assert!(skipped.error.is_some(), "the skip says why");
    assert_eq!(skipped.warnings.len(), 2, "{:?}", skipped.warnings);
    assert!(
        skipped
            .warnings
            .iter()
            .all(|(_, warning)| matches!(warning, Warning::LayerUnreadable { .. })),
        "{:?}",
        skipped.warnings
    );
    let counts = review.engine.job(review.job).unwrap().counts;
    assert_eq!((counts.written, counts.failed, counts.skipped), (1, 1, 1));
}

#[test]
fn unusable_output_folder_fails_the_job_and_retry_writes() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let root = tempfile::tempdir().unwrap();
    let mut config = JobConfig::new(input.path().to_path_buf());
    config.output_dir = root.path().join("out");
    let review = Review::start(&dirs, config.clone());
    let job = review.job;
    review.engine.confirm_all(job).unwrap();
    // The output folder became a file after the job was submitted.
    fs::write(&config.output_dir, b"not a folder").unwrap();

    review.engine.write(job).unwrap();
    let seen = wait_for(&review.events, |event| {
        matches!(event, EngineEvent::Error { .. })
    });
    assert!(
        status_changes(&seen).ends_with(&[(job, JobStatus::Writing), (job, JobStatus::Failed)]),
        "{seen:#?}"
    );
    let view = review.engine.job(job).unwrap();
    assert_eq!(view.status, JobStatus::Failed);
    let message = view.error.unwrap();
    assert!(message.contains("out"), "{message}");
    assert_eq!(
        seen.last(),
        Some(&EngineEvent::Error {
            job: Some(job),
            message
        })
    );
    assert!(
        review
            .groups()
            .iter()
            .all(|group| group.status == GroupStatus::Confirmed)
    );

    // Retried once the folder can be made, the job is written, not scanned.
    fs::remove_file(&config.output_dir).unwrap();
    review.engine.retry(job).unwrap();
    let seen = wait_for(&review.events, is_status(job, JobStatus::Completed));
    assert_eq!(
        status_changes(&seen),
        [
            (job, JobStatus::Queued),
            (job, JobStatus::Writing),
            (job, JobStatus::Completed)
        ]
    );
    assert_eq!(review.engine.job(job).unwrap().error, None);
    assert_eq!(file_names(&config.output_dir).len(), 2);
}

#[test]
fn cancelled_write_is_written_later() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let held = tempfile::tempdir().unwrap();
    write_lone_shots(held.path(), 2);
    let gate = Arc::new(Gate::default());
    gate.open();
    let engine = Engine::start(options(&dirs, gate.wake())).unwrap();
    let _open = OpenOnDrop(Arc::clone(&gate));
    let events = engine.events();
    let config = JobConfig::new(input.path().to_path_buf());
    let job = engine.submit(config.clone()).unwrap();
    wait_for(&events, is_status(job, JobStatus::ReviewPending));
    engine.confirm_all(job).unwrap();

    // Another job's scan is held, so the write waits in the queue.
    gate.close();
    let first = engine
        .submit(JobConfig::new(held.path().to_path_buf()))
        .unwrap();
    wait_for(&events, is_hash_tick(first));
    engine.write(job).unwrap();
    assert_eq!(engine.job(job).unwrap().status, JobStatus::Queued);
    engine.cancel(job).unwrap();
    assert_eq!(engine.job(job).unwrap().status, JobStatus::Cancelled);
    gate.open();
    wait_for(&events, is_status(first, JobStatus::ReviewPending));
    assert!(file_names(&config.output_dir).is_empty());

    // A cancelled job whose scan is done is written on request.
    engine.write(job).unwrap();
    let seen = wait_for(&events, is_status(job, JobStatus::Completed));
    assert!(errors(&seen).is_empty(), "{seen:#?}");
    assert_eq!(file_names(&config.output_dir).len(), 2);
    // A completed job is written no more.
    assert!(matches!(
        engine.write(job),
        Err(EngineError::WrongState {
            status: JobStatus::Completed,
            ..
        })
    ));
}

/// Moves the files `names` from the folder `from` into `to`, as a drive
/// that drops takes them away, or brings them back.
fn move_files(from: &Path, to: &Path, names: &[&str]) {
    for name in names {
        fs::rename(from.join(name), to.join(name)).unwrap();
    }
}

/// Whether some group of the database at `db` has the stored status
/// `status`, such as `failed`.
fn has_group_status(db: &Path, status: &str) -> bool {
    let conn = rusqlite::Connection::open(db).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM groups WHERE status = ?1",
            [status],
            |row| row.get(0),
        )
        .unwrap();
    count > 0
}

/// A Completed job of [`review_folder`] with a stranded stack: both groups
/// confirmed, and the photos of `b` on a drive that dropped during the
/// write, so `a` is written and `b` skipped. The drive is back once this
/// returns. Gives the input folder, the job's config and its review.
fn written_with_b_stranded(dirs: &AppDirs) -> (TempDir, JobConfig, Review) {
    let input = review_folder();
    let config = JobConfig::new(input.path().to_path_buf());
    let review = Review::start(dirs, config.clone());
    review.engine.confirm_all(review.job).unwrap();
    let away = tempfile::tempdir().unwrap();
    let dropped = ["b_1.jpg", "b_2.jpg"];
    move_files(input.path(), away.path(), &dropped);
    let seen = review.write_until(JobStatus::Completed);
    assert!(errors(&seen).is_empty(), "{seen:#?}");
    move_files(away.path(), input.path(), &dropped);
    (input, config, review)
}

#[test]
fn retry_rewrites_failed_stacks_once_the_inputs_are_back() {
    let (_data, dirs) = data_dir();
    let (_input, config, review) = written_with_b_stranded(&dirs);
    let (engine, job) = (&review.engine, review.job);
    let written = review.group("a_1.jpg");
    assert_eq!(written.status, GroupStatus::Written);
    let stack = written.output_path.unwrap();
    let bytes = fs::read(&stack).unwrap();
    let stranded = review.group("b_1.jpg");
    assert_eq!(
        (stranded.status, stranded.output_path),
        (GroupStatus::Skipped, None)
    );
    assert!(
        !stranded.warnings.is_empty()
            && stranded
                .warnings
                .iter()
                .all(|(_, warning)| matches!(warning, Warning::LayerUnreadable { .. })),
        "{:?}",
        stranded.warnings
    );

    // Once the drive is back, the retry writes the stranded stack, and it
    // alone.
    engine.retry(job).unwrap();
    let seen = wait_for(&review.events, is_status(job, JobStatus::Completed));
    assert_eq!(
        seen[..2],
        [
            EngineEvent::JobStatusChanged {
                job,
                status: JobStatus::Queued
            },
            EngineEvent::GroupsChanged { job }
        ]
    );
    assert_eq!(
        status_changes(&seen),
        [
            (job, JobStatus::Queued),
            (job, JobStatus::Writing),
            (job, JobStatus::Completed)
        ]
    );
    assert!(errors(&seen).is_empty(), "{seen:#?}");
    for group in review.groups() {
        assert_eq!(
            (group.status, group.error.as_deref(), group.warnings.len()),
            (GroupStatus::Written, None, 0),
            "{group:#?}"
        );
    }
    assert_eq!(review.group("a_1.jpg").output_path, Some(stack.clone()));
    assert_eq!(fs::read(&stack).unwrap(), bytes, "the first stack was kept");
    assert_eq!(file_names(&config.output_dir).len(), 2);
}

#[test]
fn a_retry_the_store_refuses_leaves_the_job_as_it_was() {
    let (_data, dirs) = data_dir();
    let (_input, config, review) = written_with_b_stranded(&dirs);
    let (engine, job) = (&review.engine, review.job);
    review.drain();
    let before = (engine.job(job).unwrap(), review.groups());
    // The store refuses to change a group.
    edit_db(
        &dirs.db(),
        "CREATE TRIGGER test_refuse_groups BEFORE UPDATE ON groups
           BEGIN SELECT RAISE(ABORT, 'refused by the test'); END;",
    );

    let refused = engine.retry(job);
    assert!(
        matches!(&refused, Err(EngineError::Store(error))
            if error.to_string().contains("refused by the test")),
        "{refused:?}"
    );
    // The job was not queued either: it is as it was, and can be retried.
    assert_eq!((engine.job(job).unwrap(), review.groups()), before);
    assert_eq!(before.0.status, JobStatus::Completed);
    assert_eq!(review.events.try_recv(), Err(TryRecvError::Empty));

    edit_db(&dirs.db(), "DROP TRIGGER test_refuse_groups");
    engine.retry(job).unwrap();
    let seen = wait_for(&review.events, is_status(job, JobStatus::Completed));
    assert!(errors(&seen).is_empty(), "{seen:#?}");
    assert!(
        review
            .groups()
            .iter()
            .all(|group| group.status == GroupStatus::Written),
        "{:#?}",
        review.groups()
    );
    assert_eq!(file_names(&config.output_dir).len(), 2);
}

#[test]
fn a_stack_skipped_for_its_resolutions_is_not_retried() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    // One scene at two sizes: they group, but a stack holds only one size.
    for (name, width, height) in [("big", WIDTH, HEIGHT), ("small", WIDTH / 2, HEIGHT / 2)] {
        let bytes = plain_jpeg(&scene(3, width, height));
        fs::write(input.path().join(format!("{name}.jpg")), bytes).unwrap();
    }
    let mut config = JobConfig::new(input.path().to_path_buf());
    config.auto_confirm = true;
    let engine = start(&dirs);
    let events = engine.events();
    let job = engine.submit(config.clone()).unwrap();
    let seen = wait_for(&events, is_status(job, JobStatus::Completed));
    assert!(errors(&seen).is_empty(), "{seen:#?}");
    let groups = engine.groups(job).unwrap();
    let [group] = &groups[..] else {
        panic!("{groups:#?}")
    };
    assert_eq!(group.status, GroupStatus::Skipped);
    assert!(
        matches!(
            group.warnings[..],
            [(_, Warning::ResolutionMismatch { .. })]
        ),
        "{:?}",
        group.warnings
    );

    // It would be skipped again: nothing of the job can be written again.
    let refused = engine.retry(job);
    assert!(
        matches!(&refused, Err(EngineError::Invalid(message))
            if message == "no stack of this job can be written again"),
        "{refused:?}"
    );
    assert_eq!(engine.job(job).unwrap().status, JobStatus::Completed);
    assert_eq!(engine.groups(job).unwrap(), groups);
    assert_eq!(events.try_recv(), Err(TryRecvError::Empty));
    assert!(file_names(&config.output_dir).is_empty());
}

#[test]
fn retry_of_a_failed_job_also_requeues_its_stranded_stacks() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    for (folder, seed) in [("a", 1), ("b", 2)] {
        let dir = input.path().join(folder);
        fs::create_dir(&dir).unwrap();
        let image = scene(seed, WIDTH, HEIGHT);
        for (name, image) in [("1", image.clone()), ("2", expose(&image, 0.3))] {
            let bytes = plain_jpeg(&image);
            fs::write(dir.join(format!("{folder}_{name}.jpg")), bytes).unwrap();
        }
    }
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("out");
    let mut config = JobConfig::new(input.path().to_path_buf());
    config.recursive = true;
    config.output_dir = output.clone();
    // Holds the engine's thread once a stack has failed: at the wake of its
    // GroupsChanged.
    let gate = {
        let db = dirs.db();
        Arc::new(Gate::holding(move || {
            on_thread(ENGINE_THREAD) && has_group_status(&db, "failed")
        }))
    };
    let review = Review::on(
        Engine::start(options(&dirs, gate.wake())).unwrap(),
        config.clone(),
    );
    let _open = OpenOnDrop(Arc::clone(&gate));
    let (engine, job) = (&review.engine, review.job);
    // The groups are written folder by folder: `a` first.
    assert_eq!(review.groups()[0].id, review.group("a_1.jpg").id);
    engine.confirm_all(job).unwrap();
    review.drain();
    // The stacks of `a` go to `<out>/a`, which is a file.
    fs::create_dir(&output).unwrap();
    fs::write(output.join("a"), b"in the way").unwrap();

    // The write is cancelled once the stack of `a` has failed, before `b`.
    engine.write(job).unwrap();
    wait_for(&review.events, |event| {
        *event == EngineEvent::GroupsChanged { job }
    });
    engine.cancel(job).unwrap();
    gate.open();
    wait_for(&review.events, is_status(job, JobStatus::Cancelled));
    assert_eq!(review.group("a_1.jpg").status, GroupStatus::Failed);
    assert_eq!(review.group("b_1.jpg").status, GroupStatus::Confirmed);

    // Written again on request, the job fails: its output folder is a file.
    fs::remove_file(output.join("a")).unwrap();
    fs::remove_dir(&output).unwrap();
    fs::write(&output, b"not a folder").unwrap();
    engine.write(job).unwrap();
    let seen = wait_for(&review.events, |event| {
        matches!(event, EngineEvent::Error { .. })
    });
    assert!(
        status_changes(&seen).ends_with(&[(job, JobStatus::Failed)]),
        "{seen:#?}"
    );
    assert_eq!(review.group("a_1.jpg").status, GroupStatus::Failed);

    // Retried once the folder can be made, it writes both stacks.
    fs::remove_file(&output).unwrap();
    engine.retry(job).unwrap();
    let seen = wait_for(&review.events, is_status(job, JobStatus::Completed));
    assert_eq!(
        seen[..2],
        [
            EngineEvent::JobStatusChanged {
                job,
                status: JobStatus::Queued
            },
            EngineEvent::GroupsChanged { job }
        ]
    );
    assert!(errors(&seen).is_empty(), "{seen:#?}");
    for name in ["a_1.jpg", "b_1.jpg"] {
        let group = review.group(name);
        assert_eq!(
            (group.status, group.error.as_deref()),
            (GroupStatus::Written, None),
            "{group:#?}"
        );
    }
    assert_eq!(engine.job(job).unwrap().error, None);
}

#[test]
fn swap_active_makes_the_jpeg_the_layer() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    write_raw_bracket(input.path(), "brk", 5);
    // A pair whose JPEG cannot be decoded.
    let lone = plain_jpeg(&scene(50, WIDTH, HEIGHT));
    fs::write(input.path().join("bad.raf"), raf_with_jpeg(&lone)).unwrap();
    fs::write(input.path().join("bad.jpg"), b"not a jpeg").unwrap();
    let review = Review::start(&dirs, JobConfig::new(input.path().to_path_buf()));
    let (engine, job) = (&review.engine, review.job);
    let bracket = review.group("brk_2.raf");
    assert_eq!(
        review.names(&bracket.members),
        ["brk_1.raf", "brk_2.raf", "brk_3.raf"]
    );
    engine.confirm(job, bracket.id).unwrap();
    review.drain();
    let (raf, jpg) = (review.id("brk_2.raf"), review.id("brk_2.jpg"));
    assert_eq!(review.files[jpg].paired_with, Some(raf));

    // An active file has no partner to give way to.
    assert_invalid(
        engine.swap_active(job, raf),
        "the photo brk_2.raf is not the one a RAW+JPEG pair leaves out",
    );
    engine.swap_active(job, jpg).unwrap();
    wait_for(&review.events, |event| {
        *event == EngineEvent::GroupsChanged { job }
    });
    let swapped = review.group("brk_2.jpg");
    assert_eq!(
        (swapped.id, swapped.status),
        (bracket.id, GroupStatus::Confirmed)
    );
    assert_eq!(
        review.names(&swapped.members),
        ["brk_1.raf", "brk_2.jpg", "brk_3.raf"]
    );
    // The JPEG takes the RAW's layer, second from the top.
    assert_eq!(swapped.members[1], jpg);
    let files = engine.files(job).unwrap();
    assert_eq!(
        (files[jpg].paired_with, files[raf].paired_with),
        (None, Some(jpg))
    );
    assert!(
        files[jpg]
            .thumb
            .as_ref()
            .is_some_and(|thumb| thumb.is_file())
    );
    let analysis = engine.analysis(job).unwrap();
    assert!(analysis.files[jpg].normalised.is_some());
    assert_eq!(analysis.files[raf].normalised, None);

    // A partner that cannot be decoded is refused, and nothing changes.
    let groups = review.groups();
    let bad = review.id("bad.jpg");
    engine.swap_active(job, bad).unwrap();
    let seen = wait_for(&review.events, |event| {
        matches!(event, EngineEvent::Error { .. })
    });
    let EngineEvent::Error { job: of, message } = seen.last().unwrap() else {
        unreachable!()
    };
    assert_eq!(*of, Some(job));
    // Named as the user knows it, inside the job's folder, and never first.
    assert!(
        message.starts_with("the photo bad.jpg cannot be the active file of its pair: "),
        "{message}"
    );
    assert_eq!(review.groups(), groups);
    assert_eq!(engine.files(job).unwrap(), files);
    assert_eq!(engine.job(job).unwrap().status, JobStatus::ReviewPending);
}

#[test]
fn commands_in_the_wrong_state_are_refused() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let gate = Arc::new(Gate::default());
    let engine = Engine::start(options(&dirs, gate.wake())).unwrap();
    let _open = OpenOnDrop(Arc::clone(&gate));
    let events = engine.events();
    let job = engine
        .submit(JobConfig::new(input.path().to_path_buf()))
        .unwrap();
    wait_for(&events, is_hash_tick(job));

    let wrong = |status: JobStatus, attempts: Vec<Result<(), EngineError>>| {
        for (n, attempt) in attempts.into_iter().enumerate() {
            assert!(
                matches!(attempt, Err(EngineError::WrongState { status: s, job: j }) if s == status && j == job),
                "command {n} in {status:?}: {attempt:?}"
            );
        }
    };
    let every_command = |engine: &Engine, group, file| {
        vec![
            engine.confirm(job, group),
            engine.reject(job, group),
            engine.confirm_all(job),
            engine.reject_all(job),
            engine.set_thresholds(job, NEAR, WINDOW, None),
            engine.move_file(job, file, MoveTarget::Tray),
            engine.merge(job, &[group, group + 1]),
            engine.split(job, group, &[file, file + 1]),
            engine.swap_active(job, file),
            engine.write(job),
        ]
    };
    wrong(JobStatus::Scanning, every_command(&engine, 1, 0));
    gate.open();
    wait_for(&events, is_status(job, JobStatus::ReviewPending));

    // In review, the job's groups and files are checked.
    let groups = engine.groups(job).unwrap();
    let group = groups[0].id;
    let other = groups.iter().map(|group| group.id).max().unwrap() + 1;
    assert!(matches!(
        engine.confirm(job, other),
        Err(EngineError::UnknownGroup(id)) if id == other
    ));
    assert!(matches!(
        engine.move_file(job, 99, MoveTarget::Tray),
        Err(EngineError::UnknownFile { idx: 99, .. })
    ));
    assert!(matches!(
        engine.swap_active(job, 99),
        Err(EngineError::UnknownFile { idx: 99, .. })
    ));
    assert!(matches!(
        engine.set_thresholds(job, 41, WINDOW, None),
        Err(EngineError::Config(ConfigError::OutOfRange {
            field: "hamming_threshold",
            ..
        }))
    ));
    assert!(matches!(
        engine.set_thresholds(job, 8, WINDOW, Some(0.2)),
        Err(EngineError::Config(ConfigError::OutOfRange {
            field: "cosine_threshold",
            ..
        }))
    ));
    assert_eq!(engine.job(job).unwrap().config.hamming_threshold, 8);
    assert!(matches!(
        engine.confirm(job + 1, group),
        Err(EngineError::UnknownJob(_))
    ));
    assert!(matches!(
        engine.write(job + 1),
        Err(EngineError::UnknownJob(_))
    ));
    assert!(matches!(
        engine.retry(job),
        Err(EngineError::WrongState {
            status: JobStatus::ReviewPending,
            ..
        })
    ));

    engine.write(job).unwrap();
    wait_for(&events, is_status(job, JobStatus::Completed));
    wrong(JobStatus::Completed, every_command(&engine, group, 0));
    // No group was confirmed, so none is stranded: a retry would write
    // nothing.
    let refused = engine.retry(job);
    assert!(
        matches!(&refused, Err(EngineError::Invalid(message))
            if message == "no stack of this job can be written again"),
        "{refused:?}"
    );
}

#[test]
fn confirm_of_an_unknown_group_is_refused() {
    let (_data, dirs) = data_dir();
    let (input, other_input) = (review_folder(), review_folder());
    let engine = start(&dirs);
    let events = engine.events();
    let job = engine
        .submit(JobConfig::new(input.path().to_path_buf()))
        .unwrap();
    let other = engine
        .submit(JobConfig::new(other_input.path().to_path_buf()))
        .unwrap();
    // The jobs run in order, so both are in review.
    wait_for(&events, is_status(other, JobStatus::ReviewPending));
    let (groups, theirs) = (engine.groups(job).unwrap(), engine.groups(other).unwrap());
    let unknown = groups
        .iter()
        .chain(&theirs)
        .map(|group| group.id)
        .max()
        .unwrap()
        + 1;

    // A group of another job is no group of this one.
    for group in [theirs[0].id, unknown] {
        for attempt in [engine.confirm(job, group), engine.reject(job, group)] {
            assert!(
                matches!(attempt, Err(EngineError::UnknownGroup(id)) if id == group),
                "group {group}: {attempt:?}"
            );
        }
    }
    assert_eq!(engine.groups(job).unwrap(), groups);
    assert_eq!(engine.groups(other).unwrap(), theirs);
}

/// Whether the analysis of `job` in the database at `db` is saved.
fn scan_done(db: &Path, job: JobId) -> bool {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row("SELECT scan_done FROM jobs WHERE id = ?1", [job], |row| {
        row.get(0)
    })
    .unwrap_or(false)
}

/// A gate that holds the engine's thread once the analysis of job 1 (the
/// first job of a fresh database) is saved: at the wake of the scan's
/// `GroupsChanged`, before the worker settles the scan.
fn after_the_scan(dirs: &AppDirs) -> Arc<Gate> {
    let db = dirs.db();
    Arc::new(Gate::holding(move || {
        on_thread(ENGINE_THREAD) && scan_done(&db, 1)
    }))
}

#[test]
fn late_cancel_of_a_scan_still_reviews() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let gate = after_the_scan(&dirs);
    let engine = Engine::start(options(&dirs, gate.wake())).unwrap();
    let _open = OpenOnDrop(Arc::clone(&gate));
    let events = engine.events();
    let job = engine
        .submit(JobConfig::new(input.path().to_path_buf()))
        .unwrap();
    assert_eq!(job, 1);
    wait_for(&events, |event| {
        *event == EngineEvent::GroupsChanged { job }
    });

    // The scan's work is done: the cancel comes too late.
    engine.cancel(job).unwrap();
    gate.open();
    let seen = wait_for(&events, is_status(job, JobStatus::ReviewPending));
    assert!(
        !status_changes(&seen).contains(&(job, JobStatus::Cancelled)),
        "{seen:#?}"
    );
}

#[test]
fn cancel_before_an_auto_confirm_write_cancels() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut config = JobConfig::new(input.path().to_path_buf());
    config.auto_confirm = true;
    let gate = after_the_scan(&dirs);
    let engine = Engine::start(options(&dirs, gate.wake())).unwrap();
    let _open = OpenOnDrop(Arc::clone(&gate));
    let events = engine.events();
    let job = engine.submit(config.clone()).unwrap();
    assert_eq!(job, 1);
    wait_for(&events, |event| {
        *event == EngineEvent::GroupsChanged { job }
    });

    // The write had not started: the job is cancelled, scanned and confirmed.
    engine.cancel(job).unwrap();
    gate.open();
    let seen = wait_for(&events, is_status(job, JobStatus::Cancelled));
    assert!(
        !status_changes(&seen).contains(&(job, JobStatus::Writing)),
        "{seen:#?}"
    );
    let groups = engine.groups(job).unwrap();
    assert_eq!(groups.len(), 2);
    for group in &groups {
        assert_eq!(
            (group.status, group.output_path.as_ref()),
            (GroupStatus::Confirmed, None)
        );
    }
    assert!(file_names(&config.output_dir).is_empty());

    // It is written on request.
    engine.write(job).unwrap();
    wait_for(&events, is_status(job, JobStatus::Completed));
    assert_eq!(file_names(&config.output_dir).len(), 2);
}

/// Whether file `idx` of `job` is an active file: one that pairs with none.
fn is_active(db: &Path, job: JobId, idx: usize) -> bool {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row(
        "SELECT paired_with IS NULL FROM files WHERE job_id = ?1 AND idx = ?2",
        rusqlite::params![job, idx],
        |row| row.get(0),
    )
    .unwrap_or(false)
}

#[test]
fn delete_during_a_swap_drops_the_jobs_queued_tasks() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    write_raw_bracket(input.path(), "brk", 5);
    // Holds the engine's thread at the wake of the swap's GroupsChanged:
    // once the swap committed the JPEG, of job 1 (the first job of a fresh
    // database), as an active file.
    let swapped: Arc<OnceLock<usize>> = Arc::default();
    let gate = {
        let swapped = Arc::clone(&swapped);
        let db = dirs.db();
        Arc::new(Gate::holding(move || {
            on_thread(ENGINE_THREAD) && swapped.get().is_some_and(|&jpg| is_active(&db, 1, jpg))
        }))
    };
    let engine = Engine::start(options(&dirs, gate.wake())).unwrap();
    let _open = OpenOnDrop(Arc::clone(&gate));
    let events = engine.events();
    let job = engine
        .submit(JobConfig::new(input.path().to_path_buf()))
        .unwrap();
    assert_eq!(job, 1);
    wait_for(&events, is_status(job, JobStatus::ReviewPending));
    let files = engine.files(job).unwrap();
    let jpg = files
        .iter()
        .position(|file| file.path.ends_with("brk_2.jpg"))
        .unwrap();

    swapped.set(jpg).unwrap();
    engine.swap_active(job, jpg).unwrap();
    wait_for(&events, |event| {
        *event == EngineEvent::GroupsChanged { job }
    });
    // The swap is still running: a write and the delete come behind it.
    engine.write(job).unwrap();
    engine.delete(job).unwrap();
    gate.open();
    let mut seen = wait_for(&events, |event| *event == EngineEvent::JobDeleted { job });

    // The next job is scanned after anything left in the queue for the
    // deleted one, so nothing of it ran.
    let next = review_folder();
    let next_job = engine
        .submit(JobConfig::new(next.path().to_path_buf()))
        .unwrap();
    seen.extend(wait_for(
        &events,
        is_status(next_job, JobStatus::ReviewPending),
    ));
    assert!(errors(&seen).is_empty(), "{seen:#?}");
    assert!(
        !status_changes(&seen).contains(&(job, JobStatus::Writing)),
        "{seen:#?}"
    );
    assert!(matches!(engine.job(job), Err(EngineError::UnknownJob(_))));
}

#[test]
fn set_thresholds_needs_only_the_store() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let review = Review::start(&dirs, JobConfig::new(input.path().to_path_buf()));
    let (engine, job) = (&review.engine, review.job);
    // The input folder is gone, as an unplugged drive is.
    let path = input.path().to_path_buf();
    drop(input);
    assert!(!path.exists());

    engine.set_thresholds(job, NEAR, WINDOW, Some(0.9)).unwrap();
    assert_eq!(
        review.names(&review.group("b_1.jpg").members),
        ["b_1.jpg", "b_2.jpg", "b_3.jpg"]
    );
    assert!(matches!(
        engine.set_thresholds(job, 41, WINDOW, None),
        Err(EngineError::Config(ConfigError::OutOfRange {
            field: "hamming_threshold",
            ..
        }))
    ));
    let config = engine.job(job).unwrap().config;
    assert_eq!(
        (config.hamming_threshold, config.cosine_threshold),
        (NEAR, Some(0.9))
    );
}

#[test]
fn a_time_window_edit_regroups_and_is_kept() {
    let (_data, dirs) = data_dir();
    // Two shots of one scene, 10 s apart, with a Canon's EXIF and no
    // bracket: at 4 s they are never compared.
    let input = tempfile::tempdir().unwrap();
    let image = scene(1, WIDTH, HEIGHT);
    for (name, second) in [("a.jpg", 0), ("b.jpg", 10)] {
        let mut exif = canon_exif(second, 1);
        exif.maker_note = None;
        let bytes = jpeg_with_exif(&image, &exif.build(), JPEG_QUALITY);
        fs::write(input.path().join(name), bytes).unwrap();
    }
    let mut config = JobConfig::new(input.path().to_path_buf());
    config.time_window_seconds = 4;
    let review = Review::start(&dirs, config);
    let (engine, job) = (&review.engine, review.job);
    assert_eq!(review.groups(), Vec::<GroupView>::new());
    assert_eq!(review.tray(), ["a.jpg", "b.jpg"]);

    // Only the window changes: the two are one stack.
    engine.set_thresholds(job, 8, 30, None).unwrap();
    assert_eq!(
        review.events.try_recv(),
        Ok(EngineEvent::GroupsChanged { job })
    );
    let stack = review.group("a.jpg");
    assert_eq!(review.names(&stack.members), ["a.jpg", "b.jpg"]);
    assert_eq!(
        (stack.status, stack.origin),
        (GroupStatus::Proposed, GroupOrigin::Auto)
    );
    assert_eq!(review.tray(), Vec::<String>::new());

    // A window out of its range is refused, and changes nothing.
    for window in [0, 61] {
        assert!(
            matches!(
                engine.set_thresholds(job, 8, window, None),
                Err(EngineError::Config(ConfigError::OutOfRange {
                    field: "time_window_seconds",
                    ..
                }))
            ),
            "{window}"
        );
    }
    assert_eq!(review.group("a.jpg").id, stack.id);

    // The job keeps it: a reopened engine reads it, and its stack.
    review.shutdown();
    let engine = start(&dirs);
    let config = engine.job(job).unwrap().config;
    assert_eq!(
        (config.hamming_threshold, config.time_window_seconds),
        (8, 30)
    );
    assert_eq!(engine.groups(job).unwrap()[0].id, stack.id);
    assert_eq!(engine.unstacked(job).unwrap(), Vec::<usize>::new());
}

// --- Resume on start ---

/// Runs `sql` on the database at `db`, as a crash would have left it.
fn edit_db(db: &Path, sql: &str) {
    rusqlite::Connection::open(db)
        .unwrap()
        .execute_batch(sql)
        .unwrap();
}

/// Records `partial` as the unfinished stack of `group`, as a write does.
fn record_partial(db: &Path, group: GroupId, partial: &Path) {
    Store::open(db)
        .unwrap()
        .set_group_partial(group, Some(partial))
        .unwrap();
}

/// The three files a write leaves when it crashes: `partial` and its spill
/// files.
fn partial_triple(partial: &Path) -> [PathBuf; 3] {
    ["", ".layers", ".merged"].map(|suffix| {
        let mut name = partial.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    })
}

/// Writes `files`, each holding its own path, creating their folders.
fn plant(files: &[PathBuf]) {
    for file in files {
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, file.as_os_str().as_encoded_bytes()).unwrap();
    }
}

/// A job whose write crashed: [`review_folder`] reviewed, its `a` group
/// confirmed, and the job left Writing with nothing written yet.
struct CrashedWrite {
    input: TempDir,
    config: JobConfig,
    job: JobId,
    group: GroupId,
}

impl CrashedWrite {
    fn new(dirs: &AppDirs) -> CrashedWrite {
        let input = review_folder();
        let config = JobConfig::new(input.path().to_path_buf());
        CrashedWrite::of(dirs, input, config)
    }

    /// A crashed write whose job writes into `output_dir`.
    fn writing_into(dirs: &AppDirs, output_dir: PathBuf) -> CrashedWrite {
        let input = review_folder();
        let mut config = JobConfig::new(input.path().to_path_buf());
        config.output_dir = output_dir;
        CrashedWrite::of(dirs, input, config)
    }

    fn of(dirs: &AppDirs, input: TempDir, config: JobConfig) -> CrashedWrite {
        let review = Review::start(dirs, config.clone());
        let group = review.group("a_1.jpg").id;
        review.engine.confirm(review.job, group).unwrap();
        let job = review.job;
        review.shutdown();
        edit_db(&dirs.db(), "UPDATE jobs SET status = 'writing'");
        CrashedWrite {
            input,
            config,
            job,
            group,
        }
    }

    /// Starts an engine on the crashed database, waits for the job's write,
    /// and checks that it wrote the confirmed group, and that alone; returns
    /// the engine and the group's stack.
    fn resume(&self, dirs: &AppDirs) -> (Engine, PathBuf) {
        let engine = start(dirs);
        let events = engine.events();
        let job = self.job;
        let seen = wait_for(&events, is_status(job, JobStatus::Completed));
        assert_eq!(
            status_changes(&seen),
            [
                (job, JobStatus::Queued),
                (job, JobStatus::Writing),
                (job, JobStatus::Completed)
            ]
        );
        assert!(errors(&seen).is_empty(), "{seen:#?}");
        let groups = engine.groups(job).unwrap();
        let (written, others): (Vec<GroupView>, Vec<GroupView>) =
            groups.into_iter().partition(|group| group.id == self.group);
        assert_eq!(written[0].status, GroupStatus::Written);
        let output = written[0].output_path.clone().unwrap();
        assert!(output.is_file(), "{output:?}");
        assert!(
            others
                .iter()
                .all(|group| group.status == GroupStatus::Proposed),
            "{others:#?}"
        );
        (engine, output)
    }
}

impl Review {
    /// Shuts the engine down; the job stays in the database.
    fn shutdown(self) {
        self.engine.shutdown();
    }
}

#[test]
fn shutdown_mid_scan_resumes_to_the_same_result() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    write_lone_shots(input.path(), MANY - 4);
    write_bracket(input.path(), "brk", 1, 0, 4);
    let config = JobConfig::new(input.path().to_path_buf());
    record_exam_puts(&dirs.db());
    let gate = Arc::new(Gate::default());
    let engine = Engine::start(options(&dirs, gate.wake())).unwrap();
    let events = engine.events();
    let job = engine.submit(config.clone()).unwrap();
    wait_for(&events, is_hash_tick(job));
    let opener = {
        let gate = Arc::clone(&gate);
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(2));
            gate.open();
        })
    };
    engine.shutdown();
    opener.join().unwrap();
    let first = active_puts(&dirs.db()).len();
    assert!(first > 0 && first < MANY, "{first}");

    let engine = start(&dirs);
    let events = engine.events();
    let seen = wait_for(&events, is_status(job, JobStatus::ReviewPending));
    assert_eq!(
        status_changes(&seen),
        [
            (job, JobStatus::Queued),
            (job, JobStatus::Scanning),
            (job, JobStatus::ReviewPending)
        ]
    );
    assert!(errors(&seen).is_empty(), "{seen:#?}");
    // The files examined before the quit were not examined again.
    let again = active_puts(&dirs.db()).len() - first;
    assert!(again < MANY, "{again} of {MANY} examined on the resume");
    assert_eq!(again + first, MANY);
    assert_eq!(engine_plan(&engine, job, &config), fresh_plan(&config));
    assert_eq!(engine.groups(job).unwrap().len(), 1);
}

/// Whether some group of the database at `db` records a partial.
fn has_partial(db: &Path) -> bool {
    write_state(db).iter().any(|(partial, _)| partial.is_some())
}

#[test]
fn shutdown_mid_write_resumes_the_write() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let config = JobConfig::new(input.path().to_path_buf());
    // Holds the engine's thread at the first layer of the write, once its
    // partial is recorded.
    let gate = {
        let db = dirs.db();
        Arc::new(Gate::holding(move || {
            on_thread(ENGINE_THREAD) && has_partial(&db)
        }))
    };
    let engine = Engine::start(options(&dirs, gate.wake())).unwrap();
    let events = engine.events();
    let job = engine.submit(config.clone()).unwrap();
    wait_for(&events, is_status(job, JobStatus::ReviewPending));
    engine.confirm_all(job).unwrap();
    engine.write(job).unwrap();
    wait_for(
        &events,
        |event| matches!(event, EngineEvent::JobProgress { progress, .. } if progress.stage == Stage::Write),
    );
    let opener = {
        let gate = Arc::clone(&gate);
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(2));
            gate.open();
        })
    };
    engine.shutdown();
    opener.join().unwrap();

    // The writer removed the stack the quit interrupted, and its record.
    let stored = Store::open_read_only(&dirs.db())
        .unwrap()
        .job(job)
        .unwrap()
        .unwrap();
    assert_eq!(stored.status, JobStatus::Writing);
    assert!(!has_partial(&dirs.db()));
    assert_eq!(file_names(&config.output_dir), Vec::<String>::new());

    let engine = start(&dirs);
    let events = engine.events();
    let seen = wait_for(&events, is_status(job, JobStatus::Completed));
    assert_eq!(
        status_changes(&seen),
        [
            (job, JobStatus::Queued),
            (job, JobStatus::Writing),
            (job, JobStatus::Completed)
        ]
    );
    assert!(errors(&seen).is_empty(), "{seen:#?}");
    let groups = engine.groups(job).unwrap();
    assert!(
        groups
            .iter()
            .all(|group| group.status == GroupStatus::Written),
        "{groups:#?}"
    );
    assert_eq!(file_names(&config.output_dir).len(), groups.len());
}

#[test]
fn crashed_write_leftovers_are_removed_and_rewritten() {
    let (_data, dirs) = data_dir();
    let crashed = CrashedWrite::new(&dirs);
    let leftovers = partial_triple(&crashed.config.output_dir.join("a_1_2.psd.partial"));
    plant(&leftovers);
    record_partial(&dirs.db(), crashed.group, &leftovers[0]);

    let (_engine, output) = crashed.resume(&dirs);
    for leftover in &leftovers {
        assert!(!leftover.exists(), "{leftover:?} is left");
    }
    assert_eq!(
        file_names(&crashed.config.output_dir),
        [output.file_name().unwrap().to_string_lossy()]
    );
    assert!(
        write_state(&dirs.db())
            .iter()
            .all(|(partial, _)| partial.is_none())
    );
}

#[test]
fn resume_never_removes_unrecorded_files() {
    let (_data, dirs) = data_dir();
    let crashed = CrashedWrite::new(&dirs);
    let output_dir = &crashed.config.output_dir;
    // Files the engine did not record: another program's partial and spill
    // files, a finished stack, and a namesake of the recorded partial in
    // another folder.
    let mut foreign = partial_triple(&output_dir.join("x.psd.partial")).to_vec();
    foreign.push(output_dir.join("x_2.psd"));
    foreign.push(crashed.input.path().join("a_1_2.psd.partial"));
    plant(&foreign);
    // A recorded partial that is a folder is no write's: it stays, with what
    // it holds. Its spill file, a regular file, is removed.
    let [partial, layers, merged] = partial_triple(&output_dir.join("a_1_2.psd.partial"));
    let inside = partial.join("kept.txt");
    plant(&[inside.clone(), layers.clone()]);
    foreign.push(inside);
    record_partial(&dirs.db(), crashed.group, &partial);
    let before: BTreeMap<PathBuf, Vec<u8>> = foreign
        .iter()
        .map(|file| (file.clone(), fs::read(file).unwrap()))
        .collect();

    crashed.resume(&dirs);
    for (file, bytes) in &before {
        assert_eq!(&fs::read(file).unwrap(), bytes, "{file:?}");
    }
    assert!(!layers.exists() && !merged.exists());
}

#[test]
fn review_pending_job_reopens_unchanged() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let review = Review::start(&dirs, JobConfig::new(input.path().to_path_buf()));
    let job = review.job;
    review
        .engine
        .confirm(job, review.group("a_1.jpg").id)
        .unwrap();
    review.move_file("b_3.jpg", MoveTarget::Tray);
    let jobs = review.engine.jobs().unwrap();
    let groups = review.groups();
    let files = review.engine.files(job).unwrap();
    let unstacked = review.engine.unstacked(job).unwrap();
    assert_eq!(jobs[0].status, JobStatus::ReviewPending);
    review.shutdown();

    let engine = start(&dirs);
    let events = engine.events();
    assert_eq!(engine.jobs().unwrap(), jobs);
    assert_eq!(engine.groups(job).unwrap(), groups);
    assert_eq!(engine.files(job).unwrap(), files);
    assert_eq!(engine.unstacked(job).unwrap(), unstacked);
    // Nothing was queued for it.
    thread::sleep(Duration::from_millis(300));
    assert_eq!(events.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn engine_restarts_on_the_same_db_after_shutdown() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let review = Review::start(&dirs, JobConfig::new(input.path().to_path_buf()));
    let jobs = review.engine.jobs().unwrap();
    review.shutdown();

    let engine = start(&dirs);
    let events = engine.events();
    assert_eq!(engine.jobs().unwrap(), jobs);
    let next = review_folder();
    let job = engine
        .submit(JobConfig::new(next.path().to_path_buf()))
        .unwrap();
    assert!(job > jobs[0].id);
    let seen = wait_for(&events, is_status(job, JobStatus::ReviewPending));
    assert!(errors(&seen).is_empty(), "{seen:#?}");
    assert_eq!(engine.jobs().unwrap()[0], jobs[0]);
}

/// The lock file of the database `db` (spec §7.2).
fn lock_file(db: &Path) -> PathBuf {
    let mut name = db.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// Two engines on one database would each resume and write the same jobs:
/// the second one is refused before it touches the database.
#[test]
fn a_second_engine_on_one_database_is_busy() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    write_lone_shots(input.path(), MANY);
    let gate = Arc::new(Gate::default());
    let engine = Engine::start(options(&dirs, gate.wake())).unwrap();
    let _open = OpenOnDrop(Arc::clone(&gate));
    let events = engine.events();
    let job = engine
        .submit(JobConfig::new(input.path().to_path_buf()))
        .unwrap();
    // Held while it scans, the job is one a second engine would resume.
    wait_for(&events, is_hash_tick(job));
    let jobs = engine.jobs().unwrap();
    assert_eq!(jobs[0].status, JobStatus::Scanning);

    let Err(error) = Engine::start(options(&dirs, no_wake())) else {
        panic!("a second engine started on the database");
    };
    assert!(
        matches!(&error, EngineError::Busy(db) if *db == dirs.db()),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!("another Photostacker is using {}", dirs.db().display())
    );
    assert_eq!(engine.jobs().unwrap(), jobs);
    assert!(lock_file(&dirs.db()).is_file());

    // The first engine goes on undisturbed.
    gate.open();
    let seen = wait_for(&events, is_status(job, JobStatus::ReviewPending));
    assert!(errors(&seen).is_empty(), "{seen:#?}");
}

#[test]
fn the_database_is_free_again_after_shutdown() {
    let (_data, dirs) = data_dir();
    let first = start(&dirs);
    assert!(first.is_running());
    first.shutdown();
    // The lock file stays for the next holder.
    assert!(lock_file(&dirs.db()).is_file());
    let second = start(&dirs);
    assert!(second.is_running());
    drop(second);
    // A refused start holds nothing either.
    let third = start(&dirs);
    assert!(matches!(
        Engine::start(options(&dirs, no_wake())),
        Err(EngineError::Busy(_))
    ));
    third.shutdown();
    start(&dirs).shutdown();
}

#[test]
fn queued_jobs_resume_in_order() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let held = tempfile::tempdir().unwrap();
    write_lone_shots(held.path(), MANY);
    let last = bracket_folder();
    let gate = Arc::new(Gate::default());
    gate.open();
    let engine = Engine::start(options(&dirs, gate.wake())).unwrap();
    let events = engine.events();
    let config = JobConfig::new(input.path().to_path_buf());
    let written = engine.submit(config.clone()).unwrap();
    wait_for(&events, is_status(written, JobStatus::ReviewPending));
    engine.confirm_all(written).unwrap();

    // The second job's scan is held, and the first job's write and a third
    // job's scan queue up behind it; then the app quits.
    gate.close();
    let scanning = engine
        .submit(JobConfig::new(held.path().to_path_buf()))
        .unwrap();
    wait_for(&events, is_hash_tick(scanning));
    engine.write(written).unwrap();
    let queued = engine
        .submit(JobConfig::new(last.path().to_path_buf()))
        .unwrap();
    let opener = {
        let gate = Arc::clone(&gate);
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(2));
            gate.open();
        })
    };
    engine.shutdown();
    opener.join().unwrap();
    let left: Vec<(JobStatus, bool)> = Store::open_read_only(&dirs.db())
        .unwrap()
        .jobs()
        .unwrap()
        .iter()
        .map(|job| (job.status, job.scan_done))
        .collect();
    assert_eq!(
        left,
        [
            (JobStatus::Queued, true),
            (JobStatus::Scanning, false),
            (JobStatus::Queued, false)
        ]
    );

    // They resume in the order of their ids: the first job's write, not its
    // scan again, then the two scans.
    let engine = start(&dirs);
    let events = engine.events();
    let seen = wait_for(&events, is_status(queued, JobStatus::ReviewPending));
    assert_eq!(
        status_changes(&seen),
        [
            (scanning, JobStatus::Queued),
            (written, JobStatus::Writing),
            (written, JobStatus::Completed),
            (scanning, JobStatus::Scanning),
            (scanning, JobStatus::ReviewPending),
            (queued, JobStatus::Scanning),
            (queued, JobStatus::ReviewPending)
        ]
    );
    assert!(errors(&seen).is_empty(), "{seen:#?}");
    assert_eq!(file_names(&config.output_dir).len(), 2);
}

#[test]
fn job_left_scanning_after_its_scan_resumes_in_review() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    record_exam_puts(&dirs.db());
    let review = Review::start(&dirs, JobConfig::new(input.path().to_path_buf()));
    let job = review.job;
    let groups = review.groups();
    let files = review.engine.files(job).unwrap();
    review.shutdown();
    let puts = active_puts(&dirs.db());
    // The run stopped between saving the scan and settling it.
    edit_db(&dirs.db(), "UPDATE jobs SET status = 'scanning'");

    let engine = start(&dirs);
    let events = engine.events();
    // It is not scanned again: its analysis is saved once.
    assert_eq!(
        status_changes(&events.try_iter().collect::<Vec<_>>()),
        [(job, JobStatus::ReviewPending)]
    );
    assert_eq!(engine.job(job).unwrap().status, JobStatus::ReviewPending);
    assert_eq!(engine.groups(job).unwrap(), groups);
    assert_eq!(engine.files(job).unwrap(), files);
    thread::sleep(Duration::from_millis(300));
    assert_eq!(events.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(active_puts(&dirs.db()), puts);
}

#[test]
fn auto_confirm_job_left_scanning_after_its_scan_resumes_to_its_write() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let mut config = JobConfig::new(input.path().to_path_buf());
    let review = Review::start(&dirs, config.clone());
    let job = review.job;
    review.shutdown();
    // An auto_confirm job whose scan was saved, with its groups proposed.
    config.auto_confirm = true;
    let config_json = serde_json::to_string(&config).unwrap();
    rusqlite::Connection::open(dirs.db())
        .unwrap()
        .execute(
            "UPDATE jobs SET status = 'scanning', config_json = ?1",
            [config_json],
        )
        .unwrap();

    let engine = start(&dirs);
    let events = engine.events();
    let seen = wait_for(&events, is_status(job, JobStatus::Completed));
    assert_eq!(
        status_changes(&seen),
        [
            (job, JobStatus::Queued),
            (job, JobStatus::Writing),
            (job, JobStatus::Completed)
        ]
    );
    assert!(errors(&seen).is_empty(), "{seen:#?}");
    let groups = engine.groups(job).unwrap();
    assert_eq!(groups.len(), 2);
    assert!(
        groups
            .iter()
            .all(|group| group.status == GroupStatus::Written),
        "{groups:#?}"
    );
    assert_eq!(file_names(&config.output_dir).len(), 2);
}

#[test]
fn delete_removes_a_recorded_partial() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let config = JobConfig::new(input.path().to_path_buf());
    let review = Review::start(&dirs, config.clone());
    let group = review.group("a_1.jpg").id;
    let leftovers = partial_triple(&config.output_dir.join("a_1_2.psd.partial"));
    let other = config.output_dir.join("b_1_2.psd.partial");
    plant(&leftovers);
    plant(&[other]);
    record_partial(&dirs.db(), group, &leftovers[0]);

    review.engine.delete(review.job).unwrap();
    for leftover in &leftovers {
        assert!(!leftover.exists(), "{leftover:?} is left");
    }
    assert_eq!(file_names(&config.output_dir), ["b_1_2.psd.partial"]);
}

#[test]
fn a_job_that_cannot_be_resumed_fails_and_the_next_resumes() {
    let (_data, dirs) = data_dir();
    let (first_input, second_input) = (review_folder(), review_folder());
    let review = Review::start(&dirs, JobConfig::new(first_input.path().to_path_buf()));
    let first = review.job;
    let second = review
        .engine
        .submit(JobConfig::new(second_input.path().to_path_buf()))
        .unwrap();
    wait_for(&review.events, is_status(second, JobStatus::ReviewPending));
    review.shutdown();
    // Both were left between their scan and their review, and the store
    // refuses to move the first one on.
    edit_db(
        &dirs.db(),
        &format!(
            "UPDATE jobs SET status = 'scanning';
             CREATE TRIGGER test_refuse BEFORE UPDATE OF status ON jobs
               WHEN old.id = {first} AND new.status = 'review_pending'
               BEGIN SELECT RAISE(ABORT, 'refused by the test'); END;"
        ),
    );

    let engine = start(&dirs);
    let events: Vec<EngineEvent> = engine.events().try_iter().collect();
    assert_eq!(
        status_changes(&events),
        [
            (first, JobStatus::Failed),
            (second, JobStatus::ReviewPending)
        ]
    );
    let errors = errors(&events);
    assert!(
        matches!(errors[..], [EngineEvent::Error { job: Some(job), message }]
            if *job == first && message.contains("refused by the test")),
        "{errors:#?}"
    );
    let failed = engine.job(first).unwrap();
    assert_eq!(failed.status, JobStatus::Failed);
    assert!(
        failed
            .error
            .as_deref()
            .is_some_and(|error| error.contains("refused by the test")),
        "{failed:#?}"
    );
    assert_eq!(engine.job(second).unwrap().status, JobStatus::ReviewPending);
}

#[test]
fn retry_removes_a_stale_recorded_partial() {
    let (_data, dirs) = data_dir();
    let crashed = CrashedWrite::new(&dirs);
    edit_db(&dirs.db(), "UPDATE jobs SET status = 'cancelled'");
    let leftovers = partial_triple(&crashed.config.output_dir.join("a_1_2.psd.partial"));
    plant(&leftovers);
    record_partial(&dirs.db(), crashed.group, &leftovers[0]);

    let engine = start(&dirs);
    let events = engine.events();
    // A cancelled job is not resumed: its leftovers wait for its write.
    assert!(leftovers.iter().all(|leftover| leftover.is_file()));
    engine.retry(crashed.job).unwrap();
    let seen = wait_for(&events, is_status(crashed.job, JobStatus::Completed));
    assert!(errors(&seen).is_empty(), "{seen:#?}");
    for leftover in &leftovers {
        assert!(!leftover.exists(), "{leftover:?} is left");
    }
    let group = engine
        .groups(crashed.job)
        .unwrap()
        .into_iter()
        .find(|group| group.id == crashed.group)
        .unwrap();
    assert_eq!(group.status, GroupStatus::Written);
    assert_eq!(
        file_names(&crashed.config.output_dir),
        [group
            .output_path
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()]
    );
}

#[test]
fn delete_never_removes_a_recorded_photo() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let config = JobConfig::new(input.path().to_path_buf());
    let review = Review::start(&dirs, config.clone());
    let (a, b) = (review.group("a_1.jpg").id, review.group("b_1.jpg").id);
    // A record that names a photo, and one outside the output folder: no
    // write makes either.
    let photo = config.output_dir.join("IMG_1.jpg");
    let outside = partial_triple(&input.path().join("b_1_2.psd.partial"));
    plant(std::slice::from_ref(&photo));
    plant(&outside);
    record_partial(&dirs.db(), a, &photo);
    record_partial(&dirs.db(), b, &outside[0]);
    let before = snapshot(input.path());

    review.engine.delete(review.job).unwrap();
    assert_eq!(snapshot(input.path()), before);
    assert!(photo.is_file());
}

/// Gives `dir` the Unix permission `mode` until dropped, then 0o755 again.
#[cfg(unix)]
struct Mode(PathBuf);

#[cfg(unix)]
impl Mode {
    fn set(dir: &Path, mode: u32) -> Mode {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(mode)).unwrap();
        Mode(dir.to_path_buf())
    }
}

#[cfg(unix)]
impl Drop for Mode {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

#[cfg(unix)]
#[test]
fn unremovable_leftover_fails_the_write_and_keeps_its_record() {
    let (_data, dirs) = data_dir();
    let crashed = CrashedWrite::new(&dirs);
    let output_dir = &crashed.config.output_dir;
    let leftovers = partial_triple(&output_dir.join("a_1_2.psd.partial"));
    plant(&leftovers);
    record_partial(&dirs.db(), crashed.group, &leftovers[0]);
    let read_only = Mode::set(output_dir, 0o555);
    if fs::write(output_dir.join("probe"), b"").is_ok() {
        // A privileged user ignores the permission: nothing can fail here.
        fs::remove_file(output_dir.join("probe")).unwrap();
        return;
    }

    let engine = start(&dirs);
    let events = engine.events();
    let job = crashed.job;
    // The failure's error follows its status.
    let seen = wait_for(&events, |event| matches!(event, EngineEvent::Error { .. }));
    assert_eq!(
        status_changes(&seen),
        [
            (job, JobStatus::Queued),
            (job, JobStatus::Writing),
            (job, JobStatus::Failed)
        ]
    );
    let error = engine.job(job).unwrap().error.unwrap();
    assert!(
        error.contains("cannot remove the unfinished stack"),
        "{error}"
    );
    assert_eq!(
        seen.last(),
        Some(&EngineEvent::Error {
            job: Some(job),
            message: error
        })
    );
    assert!(leftovers.iter().all(|leftover| leftover.is_file()));
    // The record stays for the next try, and nothing was written.
    let partials: Vec<PathBuf> = write_state(&dirs.db())
        .into_iter()
        .filter_map(|(partial, _)| partial)
        .collect();
    assert_eq!(partials, [leftovers[0].clone()]);
    assert!(
        engine
            .groups(job)
            .unwrap()
            .iter()
            .all(|group| group.output_path.is_none())
    );

    // Once the folder is writable again, the retry removes them and writes.
    drop(read_only);
    engine.retry(job).unwrap();
    let seen = wait_for(&events, is_status(job, JobStatus::Completed));
    assert!(errors(&seen).is_empty(), "{seen:#?}");
    assert!(leftovers.iter().all(|leftover| !leftover.exists()));
    assert!(!has_partial(&dirs.db()));
    assert_eq!(file_names(output_dir).len(), 1);
}

/// An output folder that cannot be used, such as one on a drive that is not
/// mounted, fails the write before any recorded leftover is forgotten: the
/// leftovers come back with the drive, and the next write removes them.
#[cfg(unix)]
#[test]
fn unusable_output_folder_keeps_the_recorded_partials() {
    let (_data, dirs) = data_dir();
    let root = tempfile::tempdir().unwrap();
    let drive = root.path().join("drive");
    let crashed = CrashedWrite::writing_into(&dirs, drive.join("stacked"));
    let leftovers = partial_triple(&crashed.config.output_dir.join("a_1_2.psd.partial"));
    plant(&leftovers);
    record_partial(&dirs.db(), crashed.group, &leftovers[0]);
    // The drive is unmounted: the output folder is gone, and its mount point
    // cannot be made again.
    let unplugged = root.path().join("unplugged");
    fs::rename(&drive, &unplugged).unwrap();
    let read_only = Mode::set(root.path(), 0o555);
    if fs::create_dir(&drive).is_ok() {
        // A privileged user ignores the permission: nothing can fail here.
        fs::remove_dir(&drive).unwrap();
        return;
    }

    let engine = start(&dirs);
    let events = engine.events();
    let job = crashed.job;
    let seen = wait_for(&events, |event| matches!(event, EngineEvent::Error { .. }));
    assert_eq!(
        status_changes(&seen),
        [
            (job, JobStatus::Queued),
            (job, JobStatus::Writing),
            (job, JobStatus::Failed)
        ]
    );
    let error = engine.job(job).unwrap().error.unwrap();
    assert!(
        error.starts_with("output folder ") && error.contains("cannot be created"),
        "{error}"
    );
    let partials: Vec<PathBuf> = write_state(&dirs.db())
        .into_iter()
        .filter_map(|(partial, _)| partial)
        .collect();
    assert_eq!(partials, [leftovers[0].clone()]);

    // Once the drive is back, the retry removes them and writes.
    drop(read_only);
    fs::rename(&unplugged, &drive).unwrap();
    engine.retry(job).unwrap();
    let seen = wait_for(&events, is_status(job, JobStatus::Completed));
    assert!(errors(&seen).is_empty(), "{seen:#?}");
    assert!(leftovers.iter().all(|leftover| !leftover.exists()));
    assert!(!has_partial(&dirs.db()));
    assert_eq!(file_names(&crashed.config.output_dir).len(), 1);
}

/// Makes the store of the database at `db` refuse to delete a job, with the
/// error `refused by the test`, before an engine opens it.
fn refuse_deletes(db: &Path) {
    drop(Store::open(db).unwrap());
    edit_db(
        db,
        "CREATE TRIGGER test_refuse BEFORE DELETE ON jobs
           BEGIN SELECT RAISE(ABORT, 'refused by the test'); END;",
    );
}

/// Whether `event` is the error of a refused delete of `job`.
fn is_refused_delete(job: JobId) -> impl Fn(&EngineEvent) -> bool {
    move |event| {
        matches!(event, EngineEvent::Error { job: Some(of), message }
            if *of == job && message.contains("refused by the test"))
    }
}

#[test]
fn delete_that_fails_leaves_the_running_job_cancelled() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    write_lone_shots(input.path(), MANY);
    refuse_deletes(&dirs.db());
    let gate = Arc::new(Gate::default());
    let engine = Engine::start(options(&dirs, gate.wake())).unwrap();
    let _open = OpenOnDrop(Arc::clone(&gate));
    let events = engine.events();
    let job = engine
        .submit(JobConfig::new(input.path().to_path_buf()))
        .unwrap();
    wait_for(&events, is_hash_tick(job));

    engine.delete(job).unwrap();
    gate.open();
    let seen = wait_for(&events, |event| matches!(event, EngineEvent::Error { .. }));
    // The job is not left Scanning, which the next start would scan again.
    assert_eq!(status_changes(&seen), [(job, JobStatus::Cancelled)]);
    assert!(
        matches!(seen.last(), Some(EngineEvent::Error { job: Some(of), message })
            if *of == job && message.contains("refused by the test")),
        "{seen:#?}"
    );
    assert_eq!(engine.job(job).unwrap().status, JobStatus::Cancelled);
    engine.shutdown();

    let engine = start(&dirs);
    let events = engine.events();
    thread::sleep(Duration::from_millis(300));
    assert_eq!(events.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(engine.job(job).unwrap().status, JobStatus::Cancelled);
}

#[test]
fn a_failed_delete_of_a_queued_job_cancels_it() {
    let (_data, dirs) = data_dir();
    let held = tempfile::tempdir().unwrap();
    write_lone_shots(held.path(), 2);
    let queued = bracket_folder();
    refuse_deletes(&dirs.db());
    let gate = Arc::new(Gate::default());
    let engine = Engine::start(options(&dirs, gate.wake())).unwrap();
    let _open = OpenOnDrop(Arc::clone(&gate));
    let events = engine.events();
    // The first job is held scanning while the second waits in the queue.
    let first = engine
        .submit(JobConfig::new(held.path().to_path_buf()))
        .unwrap();
    wait_for(&events, is_hash_tick(first));
    let job = engine
        .submit(JobConfig::new(queued.path().to_path_buf()))
        .unwrap();

    let refused = engine.delete(job);
    assert!(
        matches!(&refused, Err(EngineError::Store(error))
            if error.to_string().contains("refused by the test")),
        "{refused:?}"
    );
    // Its scan is gone from the queue: left Queued, the job would be scanned
    // by the next start.
    assert_eq!(engine.job(job).unwrap().status, JobStatus::Cancelled);
    gate.open();
    let seen = wait_for(&events, is_status(first, JobStatus::ReviewPending));
    let changes: Vec<JobStatus> = status_changes(&seen)
        .into_iter()
        .filter_map(|(of, status)| (of == job).then_some(status))
        .collect();
    assert_eq!(changes, [JobStatus::Cancelled]);
    engine.shutdown();

    let engine = start(&dirs);
    let events = engine.events();
    thread::sleep(Duration::from_millis(300));
    assert_eq!(events.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(engine.job(job).unwrap().status, JobStatus::Cancelled);
}

/// Deletes job 1 of a fresh database whose store refuses it (see
/// [`refuse_deletes`]) while the job's swap runs, its write first queued
/// behind the swap when `write` is set. Returns the engine and the events
/// from the swap's `GroupsChanged` to the delete's error.
fn failed_delete_during_a_swap(dirs: &AppDirs, write: bool) -> (Engine, Vec<EngineEvent>) {
    let input = tempfile::tempdir().unwrap();
    write_raw_bracket(input.path(), "brk", 5);
    refuse_deletes(&dirs.db());
    // Holds the engine's thread at the wake of the swap's GroupsChanged, as
    // `delete_during_a_swap_drops_the_jobs_queued_tasks` does.
    let swapped: Arc<OnceLock<usize>> = Arc::default();
    let gate = {
        let swapped = Arc::clone(&swapped);
        let db = dirs.db();
        Arc::new(Gate::holding(move || {
            on_thread(ENGINE_THREAD) && swapped.get().is_some_and(|&jpg| is_active(&db, 1, jpg))
        }))
    };
    let engine = Engine::start(options(dirs, gate.wake())).unwrap();
    let _open = OpenOnDrop(Arc::clone(&gate));
    let events = engine.events();
    let job = engine
        .submit(JobConfig::new(input.path().to_path_buf()))
        .unwrap();
    assert_eq!(job, 1);
    wait_for(&events, is_status(job, JobStatus::ReviewPending));
    let files = engine.files(job).unwrap();
    let jpg = files
        .iter()
        .position(|file| file.path.ends_with("brk_2.jpg"))
        .unwrap();

    swapped.set(jpg).unwrap();
    engine.swap_active(job, jpg).unwrap();
    let mut seen = wait_for(&events, |event| {
        *event == EngineEvent::GroupsChanged { job }
    });
    seen.clear();
    // The swap is still running.
    if write {
        engine.write(job).unwrap();
    }
    engine.delete(job).unwrap();
    gate.open();
    seen.extend(wait_for(&events, is_refused_delete(job)));
    (engine, seen)
}

#[test]
fn a_failed_delete_during_a_swap_leaves_the_job_in_review() {
    let (_data, dirs) = data_dir();
    let (engine, seen) = failed_delete_during_a_swap(&dirs, false);
    assert_eq!(status_changes(&seen), [], "{seen:#?}");
    assert_eq!(engine.job(1).unwrap().status, JobStatus::ReviewPending);
}

/// The job is Queued for its write, which the delete dropped: left so, the
/// next start would write it.
#[test]
fn a_failed_delete_during_a_swap_cancels_the_write_queued_behind_it() {
    let (_data, dirs) = data_dir();
    let (engine, seen) = failed_delete_during_a_swap(&dirs, true);
    assert_eq!(
        status_changes(&seen),
        [(1, JobStatus::Queued), (1, JobStatus::Cancelled)],
        "{seen:#?}"
    );
    assert_eq!(engine.job(1).unwrap().status, JobStatus::Cancelled);
}

// --- Stage 3 ---

/// The model the Stage-3 jobs name.
const MODEL: &str = "dinov2-small";

/// `image` tinted red: its green and blue halved. Its luminance stays an
/// increasing affine function of the original's, so its normalised hashes
/// barely move, while its mean colour turns red.
fn redden(image: &RgbImage) -> RgbImage {
    let mut red = image.clone();
    for pixel in red.pixels_mut() {
        pixel.0[1] /= 2;
        pixel.0[2] /= 2;
    }
    red
}

/// Whether `image` is mostly red, as [`redden`] makes it: the scenes are
/// grey with a slight tint.
fn is_red(image: &RgbImage) -> bool {
    let (mut red, mut green) = (0u64, 0u64);
    for pixel in image.pixels() {
        red += u64::from(pixel.0[0]);
        green += u64::from(pixel.0[1]);
    }
    red * 10 > green * 13
}

/// The photos of [`review_folder`] with `b_3` reddened. At the default
/// threshold the groups are `{a_1, a_2}` and `{b_1, b_2}`, at distance 0,
/// so not ambiguous; at [`NEAR`] `b_3` joins `b`, at 16 above the floor of
/// 6, so that group is checked, and [`FakeModel`] tells `b_3` apart.
fn stage3_folder() -> TempDir {
    let input = tempfile::tempdir().unwrap();
    let (a, b) = (scene(1, WIDTH, HEIGHT), scene(2, WIDTH, HEIGHT));
    let photos = [
        ("a_1", a.clone()),
        ("a_2", expose(&a, 0.3)),
        ("b_1", b.clone()),
        ("b_2", expose(&b, 0.3)),
        ("b_3", redden(&shift(&b, 0.04, 0.0))),
    ];
    for (name, image) in photos {
        fs::write(input.path().join(format!("{name}.jpg")), plain_jpeg(&image)).unwrap();
    }
    input
}

/// A stand-in for the model: a red image embeds as the first unit vector,
/// any other as the second, so two photos are alike (cosine 1) exactly when
/// both or neither are red. It counts what it embeds, and can fail, give
/// broken embeddings, or wait at a gate.
#[derive(Default)]
struct FakeModel {
    calls: AtomicUsize,
    /// The calls that reached the gate, or ran.
    entered: AtomicUsize,
    images: AtomicUsize,
    error: Option<String>,
    broken: Option<Broken>,
    gate: Option<Arc<Gate>>,
    panics: bool,
    /// When set, the model is available only once this is, as a
    /// [`FakeInstaller`] sets it; otherwise always.
    installed: Option<Arc<AtomicBool>>,
    /// Every call fails, as a model whose load failed does, until the engine
    /// forgets the model.
    unloaded: AtomicBool,
    /// The calls of [`EmbedSource::forget`].
    forgotten: AtomicUsize,
    /// When set, a look at whether the model is available takes its answer,
    /// then waits at this gate before it gives it: a scan held there has
    /// found the model missing, and has not settled yet.
    held_availability: Option<Arc<Gate>>,
    /// The looks at whether the model is available.
    asked: AtomicUsize,
    /// The next [`EmbedSource::forget`] panics, once, as a session's drop
    /// might.
    forget_panics: AtomicBool,
    /// The next look at whether the model is available from a model's
    /// thread (an install's regrouping of a job) panics, once.
    availability_panics: AtomicBool,
}

/// What is wrong with the embeddings of a broken [`FakeModel`].
#[derive(Debug, Clone, Copy)]
enum Broken {
    /// One embedding fewer than the images.
    Count,
    /// The first embedding has one value too many.
    Dims,
    /// The first embedding holds a NaN.
    NaN,
}

impl FakeModel {
    fn new() -> Arc<FakeModel> {
        Arc::new(FakeModel::default())
    }

    fn failing(error: &str) -> Arc<FakeModel> {
        Arc::new(FakeModel {
            error: Some(error.into()),
            ..FakeModel::default()
        })
    }

    /// A model that waits at `gate` before each call.
    fn gated(gate: &Arc<Gate>) -> Arc<FakeModel> {
        Arc::new(FakeModel {
            gate: Some(Arc::clone(gate)),
            ..FakeModel::default()
        })
    }

    /// A model that gives embeddings `broken` so.
    fn broken(broken: Broken) -> Arc<FakeModel> {
        Arc::new(FakeModel {
            broken: Some(broken),
            ..FakeModel::default()
        })
    }

    /// A model that panics at each call.
    fn panicking() -> Arc<FakeModel> {
        Arc::new(FakeModel {
            panics: true,
            ..FakeModel::default()
        })
    }

    /// Waits until a call reached the gate.
    fn wait_entered(&self) {
        let deadline = Instant::now() + TIMEOUT;
        while self.entered.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "the model was never called");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn images(&self) -> usize {
        self.images.load(Ordering::SeqCst)
    }
}

impl EmbedSource for FakeModel {
    fn embed(&self, model: &ModelSpec, images: &[RgbImage]) -> Result<Vec<Vec<f32>>, String> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.gate {
            gate.pass();
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.images.fetch_add(images.len(), Ordering::SeqCst);
        assert!(!self.panics, "the model panicked");
        if self.unloaded.load(Ordering::SeqCst) {
            return Err(format!("cannot load {}: a corrupt file", model.id));
        }
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        let mut embeddings: Vec<Vec<f32>> = images
            .iter()
            .map(|image| {
                let mut embedding = vec![0.0; model.dims];
                embedding[usize::from(!is_red(image))] = 1.0;
                embedding
            })
            .collect();
        match self.broken {
            None => {}
            Some(Broken::Count) => drop(embeddings.pop()),
            Some(Broken::Dims) => embeddings[0].push(0.0),
            Some(Broken::NaN) => embeddings[0][0] = f32::NAN,
        }
        Ok(embeddings)
    }

    fn available(&self, _model: &ModelSpec) -> bool {
        let on_a_model_thread = thread::current()
            .name()
            .is_some_and(|name| name.starts_with("photostacker-model-"));
        assert!(
            !(on_a_model_thread && self.availability_panics.swap(false, Ordering::SeqCst)),
            "the model's availability panicked"
        );
        let available = self
            .installed
            .as_ref()
            .is_none_or(|installed| installed.load(Ordering::SeqCst));
        self.asked.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.held_availability {
            gate.pass();
        }
        available
    }

    fn forget(&self, _model: &ModelSpec) {
        assert!(
            !self.forget_panics.swap(false, Ordering::SeqCst),
            "forgetting the model panicked"
        );
        self.forgotten.fetch_add(1, Ordering::SeqCst);
        self.unloaded.store(false, Ordering::SeqCst);
    }
}

/// An engine whose Stage 3 embeds with `model`.
fn start_with(dirs: &AppDirs, model: &Arc<FakeModel>) -> Engine {
    let mut options = options(dirs, no_wake());
    options.embedder = Some(Arc::clone(model) as Arc<dyn EmbedSource>);
    Engine::start(options).unwrap()
}

/// A job of `input` that names [`MODEL`], at the Hamming threshold
/// `hamming`.
fn ml_config(input: &Path, hamming: u32) -> JobConfig {
    let mut config = JobConfig::new(input.to_path_buf());
    config.ml_model = Some(MODEL.into());
    config.hamming_threshold = hamming;
    config
}

fn warnings(events: &[EngineEvent]) -> Vec<&EngineEvent> {
    events
        .iter()
        .filter(|event| matches!(event, EngineEvent::Warning { .. }))
        .collect()
}

/// The `(done, total)` of each Embed tick among `events`, in order.
fn embed_ticks(events: &[EngineEvent]) -> Vec<(usize, usize)> {
    events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::JobProgress { progress, .. } if progress.stage == Stage::Embed => {
                Some((progress.done, progress.total))
            }
            _ => None,
        })
        .collect()
}

/// The number of embeddings stored for [`MODEL`].
fn stored_embeddings(db: &Path) -> i64 {
    row_counts(db)["embeddings_dinov2_small"]
}

impl Review {
    /// The `(ml_status, ml_score)` of the group that holds `name`.
    fn ml(&self, name: &str) -> (MlStatus, Option<f32>) {
        let group = self.group(name);
        (group.ml_status, group.ml_score)
    }
}

#[test]
fn ambiguous_group_is_split_by_embeddings() {
    let (_data, dirs) = data_dir();
    let input = stage3_folder();
    let model = FakeModel::new();
    let review = Review::on(start_with(&dirs, &model), ml_config(input.path(), NEAR));

    // `b` was checked: b_3 is not alike, so it went to the tray.
    let b = review.group("b_1.jpg");
    assert_eq!(review.names(&b.members), ["b_1.jpg", "b_2.jpg"]);
    assert_eq!((b.ml_status, b.ml_score), (MlStatus::Split, Some(1.0)));
    assert_eq!(review.tray(), ["b_3.jpg"]);
    // `a` is not ambiguous, so it was not.
    assert_eq!(review.ml("a_1.jpg"), (MlStatus::None, None));
    // Only the ambiguous group's photos were embedded, and stored.
    assert_eq!((model.images(), stored_embeddings(&dirs.db())), (3, 3));
    assert_eq!(embed_ticks(&review.scanned), [(1, 3), (2, 3), (3, 3)]);
    assert!(
        warnings(&review.scanned).is_empty(),
        "{:#?}",
        review.scanned
    );
    assert_eq!(review.engine.job(review.job).unwrap().warning, None);
}

#[test]
fn unambiguous_group_is_not_embedded() {
    let (_data, dirs) = data_dir();
    let input = stage3_folder();
    let model = FakeModel::new();
    let review = Review::on(start_with(&dirs, &model), ml_config(input.path(), 8));

    assert_eq!(review.ml("a_1.jpg"), (MlStatus::None, None));
    assert_eq!(review.ml("b_1.jpg"), (MlStatus::None, None));
    assert_eq!(review.tray(), ["b_3.jpg"]);
    assert_eq!((model.calls(), stored_embeddings(&dirs.db())), (0, 0));
    assert!(embed_ticks(&review.scanned).is_empty());
}

#[test]
fn no_model_means_skipped_without_warning() {
    let (_data, dirs) = data_dir();
    let input = stage3_folder();
    // The job names a model that is not installed.
    let review = Review::start(&dirs, ml_config(input.path(), NEAR));
    let b = review.group("b_1.jpg");
    assert_eq!(review.names(&b.members), ["b_1.jpg", "b_2.jpg", "b_3.jpg"]);
    assert_eq!((b.ml_status, b.ml_score), (MlStatus::Skipped, None));
    assert!(
        warnings(&review.scanned).is_empty(),
        "{:#?}",
        review.scanned
    );
    assert_eq!(review.engine.job(review.job).unwrap().warning, None);
    review.shutdown();

    // Nor does a job that names no model use the one the engine has.
    let model = FakeModel::new();
    let mut config = ml_config(input.path(), NEAR);
    config.ml_model = None;
    let review = Review::on(start_with(&dirs, &model), config);
    assert_eq!(review.ml("b_1.jpg"), (MlStatus::Skipped, None));
    assert!(
        warnings(&review.scanned).is_empty(),
        "{:#?}",
        review.scanned
    );
    assert_eq!(model.calls(), 0);
}

#[test]
fn failing_model_means_skipped_with_one_warning() {
    let (_data, dirs) = data_dir();
    let input = stage3_folder();
    let model = FakeModel::failing("the model exploded");
    let review = Review::on(start_with(&dirs, &model), ml_config(input.path(), NEAR));
    let (engine, job) = (&review.engine, review.job);

    let message = "the visual check was skipped: the model exploded".to_string();
    assert_eq!(
        warnings(&review.scanned),
        [&EngineEvent::Warning {
            job: Some(job),
            message: message.clone()
        }]
    );
    assert_eq!(engine.job(job).unwrap().warning, Some(message));
    let b = review.group("b_1.jpg");
    assert_eq!(b.members.len(), 3);
    assert_eq!((b.ml_status, b.ml_score), (MlStatus::Skipped, None));
    assert_eq!((model.calls(), stored_embeddings(&dirs.db())), (1, 0));

    // Stage 3 stays skipped for the job: no new try, no second warning.
    engine.set_thresholds(job, NEAR, WINDOW, Some(0.9)).unwrap();
    assert_eq!(review.ml("b_1.jpg"), (MlStatus::Skipped, None));
    thread::sleep(Duration::from_millis(300));
    assert_eq!(
        review.events.try_recv(),
        Ok(EngineEvent::GroupsChanged { job })
    );
    assert_eq!(review.events.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(model.calls(), 1);
}

#[test]
fn broken_embeddings_mean_skipped_with_one_warning() {
    let input = stage3_folder();
    let dims = photostacker_core::models::model(MODEL).unwrap().dims;
    for (broken, error) in [
        (
            Broken::Count,
            "the model gave 2 embeddings for 3 images".to_string(),
        ),
        (
            Broken::Dims,
            format!(
                "the model gave an embedding of {} values, not {dims}",
                dims + 1
            ),
        ),
        (
            Broken::NaN,
            "the model gave a NaN or infinite value in an embedding".to_string(),
        ),
    ] {
        let (_data, dirs) = data_dir();
        let model = FakeModel::broken(broken);
        let review = Review::on(start_with(&dirs, &model), ml_config(input.path(), NEAR));
        let job = review.job;

        let message = format!("the visual check was skipped: {error}");
        assert_eq!(
            warnings(&review.scanned),
            [&EngineEvent::Warning {
                job: Some(job),
                message: message.clone()
            }],
            "{broken:?}"
        );
        assert_eq!(review.engine.job(job).unwrap().warning, Some(message));
        let b = review.group("b_1.jpg");
        assert_eq!(
            (b.members.len(), b.ml_status, b.ml_score),
            (3, MlStatus::Skipped, None),
            "{broken:?}"
        );
        // A broken embedding is never stored.
        assert_eq!(stored_embeddings(&dirs.db()), 0, "{broken:?}");
    }
}

#[test]
fn photo_that_cannot_be_decoded_again_means_skipped_with_one_warning() {
    let (_data, dirs) = data_dir();
    let input = stage3_folder();
    let model = FakeModel::new();
    let review = Review::on(start_with(&dirs, &model), ml_config(input.path(), 8));
    let (engine, job) = (&review.engine, review.job);
    // b_3 changed since the scan: it no longer decodes.
    fs::write(input.path().join("b_3.jpg"), b"not a jpeg any more").unwrap();

    // Raising the threshold makes `b` wait for embeddings, which its embed
    // task cannot compute.
    engine.set_thresholds(job, NEAR, WINDOW, None).unwrap();
    let mut seen = wait_for(&review.events, |event| {
        matches!(event, EngineEvent::Warning { .. })
    });
    seen.extend(wait_for(&review.events, |event| {
        *event == EngineEvent::GroupsChanged { job }
    }));
    let warned = warnings(&seen);
    let [EngineEvent::Warning { job: of, message }] = warned[..] else {
        panic!("{seen:#?}");
    };
    assert_eq!(*of, Some(job));
    assert!(
        message.starts_with("the visual check was skipped: ") && message.contains("b_3.jpg"),
        "{message}"
    );
    assert_eq!(engine.job(job).unwrap().warning.as_ref(), Some(message));
    let b = review.group("b_1.jpg");
    assert_eq!(
        (b.members.len(), b.ml_status, b.ml_score),
        (3, MlStatus::Skipped, None)
    );
    // Nothing was embedded: the photos are decoded before the batches run.
    assert_eq!((model.calls(), stored_embeddings(&dirs.db())), (0, 0));
    assert!(errors(&seen).is_empty(), "{seen:#?}");
}

/// An installed model file that is not the registry's (zeros of its size)
/// fails to load: Stage 3 is skipped with one warning per job, as for a
/// model that fails to run.
#[cfg(feature = "ml")]
#[test]
fn corrupt_installed_model_means_skipped_with_one_warning() {
    use photostacker_core::models;

    let (_data, dirs) = data_dir();
    let spec = models::model(MODEL).unwrap();
    let file = models::model_file(&dirs.models(), spec);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    photostacker_testkit::sparse_file(&file, spec.size).unwrap();
    assert!(models::installed(&dirs.models(), spec));
    let input = stage3_folder();
    let review = Review::start(&dirs, ml_config(input.path(), NEAR));

    let warned = warnings(&review.scanned);
    let [EngineEvent::Warning { job, message }] = warned[..] else {
        panic!("{:#?}", review.scanned);
    };
    assert_eq!(*job, Some(review.job));
    assert!(
        message.starts_with("the visual check was skipped: cannot load dinov2-small: ")
            && message.contains("does not match dinov2-small"),
        "{message}"
    );
    assert_eq!(review.ml("b_1.jpg"), (MlStatus::Skipped, None));
    // The engine keeps the failed load: a second job fails at once, and is
    // warned about once too.
    let second = review.engine.submit(ml_config(input.path(), NEAR)).unwrap();
    let seen = wait_for(&review.events, is_status(second, JobStatus::ReviewPending));
    assert_eq!(warnings(&seen).len(), 1, "{seen:#?}");
}

#[test]
fn raising_hamming_marks_new_groups_pending_then_resolves() {
    let (_data, dirs) = data_dir();
    let input = stage3_folder();
    let gate = Arc::new(Gate::holding(|| true));
    let model = FakeModel::gated(&gate);
    let review = Review::on(start_with(&dirs, &model), ml_config(input.path(), 8));
    let _open = OpenOnDrop(Arc::clone(&gate));
    let (engine, job) = (&review.engine, review.job);
    assert_eq!(review.tray(), ["b_3.jpg"]);

    // The regrouping is immediate: `b` grows, and waits for its embeddings.
    engine.set_thresholds(job, NEAR, WINDOW, None).unwrap();
    let pending = review.group("b_1.jpg");
    assert_eq!(pending.members.len(), 3);
    assert_eq!(
        (pending.ml_status, pending.ml_score),
        (MlStatus::Pending, None)
    );
    assert_eq!(review.ml("a_1.jpg"), (MlStatus::None, None));
    assert_eq!(
        review.events.try_recv(),
        Ok(EngineEvent::GroupsChanged { job })
    );

    // The embed task computes them in the background, then regroups.
    gate.open();
    let seen = wait_for(&review.events, |event| {
        *event == EngineEvent::GroupsChanged { job }
    });
    assert_eq!(embed_ticks(&seen), [(1, 3), (2, 3), (3, 3)]);
    assert!(
        errors(&seen).is_empty() && warnings(&seen).is_empty(),
        "{seen:#?}"
    );
    let b = review.group("b_1.jpg");
    assert_eq!(review.names(&b.members), ["b_1.jpg", "b_2.jpg"]);
    assert_eq!((b.ml_status, b.ml_score), (MlStatus::Split, Some(1.0)));
    assert_eq!(review.tray(), ["b_3.jpg"]);
    assert_eq!(model.images(), 3);
    assert_eq!(engine.job(job).unwrap().status, JobStatus::ReviewPending);
}

#[test]
fn embeddings_are_reused_after_restart() {
    let (_data, dirs) = data_dir();
    let input = stage3_folder();
    let first = FakeModel::new();
    let review = Review::on(start_with(&dirs, &first), ml_config(input.path(), NEAR));
    let job = review.job;
    let groups = review.groups();
    assert_eq!(first.images(), 3);
    review.shutdown();

    let second = FakeModel::new();
    let engine = start_with(&dirs, &second);
    assert_eq!(engine.groups(job).unwrap(), groups);
    // Down to the default and up again: the stored embeddings check `b`.
    engine.set_thresholds(job, 8, WINDOW, None).unwrap();
    engine.set_thresholds(job, NEAR, WINDOW, None).unwrap();
    let b = engine
        .groups(job)
        .unwrap()
        .into_iter()
        .find(|group| group.ml_status == MlStatus::Split)
        .unwrap();
    assert_eq!(b.members.len(), 2);
    thread::sleep(Duration::from_millis(300));
    assert_eq!(second.calls(), 0);
}

#[test]
fn pending_groups_get_their_embeddings_after_a_restart() {
    let (_data, dirs) = data_dir();
    let input = stage3_folder();
    let review = Review::on(
        start_with(&dirs, &FakeModel::new()),
        ml_config(input.path(), NEAR),
    );
    let job = review.job;
    let groups = review.groups();
    review.shutdown();
    // As a quit leaves a job whose embed task had not run yet.
    edit_db(
        &dirs.db(),
        "DELETE FROM embeddings_dinov2_small;
         UPDATE groups SET ml_status = 'pending', ml_score = NULL
         WHERE ml_status = 'split';",
    );

    let model = FakeModel::new();
    let engine = start_with(&dirs, &model);
    let events = engine.events();
    let seen = wait_for(&events, |event| {
        *event == EngineEvent::GroupsChanged { job }
    });
    assert!(errors(&seen).is_empty(), "{seen:#?}");
    assert_eq!(engine.groups(job).unwrap(), groups);
    assert_eq!(model.images(), 3);
    assert_eq!(engine.job(job).unwrap().status, JobStatus::ReviewPending);
}

#[test]
fn embed_task_leaves_a_job_sent_to_its_write_as_it_is() {
    let (_data, dirs) = data_dir();
    let (first_input, second_input) = (stage3_folder(), stage3_folder());
    let gate = Arc::new(Gate::holding(|| true));
    let model = FakeModel::gated(&gate);
    let engine = start_with(&dirs, &model);
    let _open = OpenOnDrop(Arc::clone(&gate));
    let events = engine.events();
    let first = engine.submit(ml_config(first_input.path(), 8)).unwrap();
    let second = engine.submit(ml_config(second_input.path(), 8)).unwrap();
    let seen = wait_for(&events, is_status(second, JobStatus::ReviewPending));
    assert!(errors(&seen).is_empty(), "{seen:#?}");

    // The first job's embed task waits in the model, the second's behind it.
    engine.set_thresholds(first, NEAR, WINDOW, None).unwrap();
    model.wait_entered();
    engine.set_thresholds(second, NEAR, WINDOW, None).unwrap();
    // Both jobs are sent to their write with `b` confirmed while Pending.
    for job in [first, second] {
        engine.confirm_all(job).unwrap();
        engine.write(job).unwrap();
    }
    gate.open();
    let seen = wait_for(&events, is_status(second, JobStatus::Completed));
    assert!(errors(&seen).is_empty(), "{seen:#?}");

    // Neither task regrouped its job: the groups confirmed were written.
    for job in [first, second] {
        let mut groups = engine.groups(job).unwrap();
        groups.sort_by_key(|group| group.members.len());
        let outcome: Vec<(usize, GroupStatus, MlStatus, bool)> = groups
            .iter()
            .map(|group| {
                let written = group
                    .output_path
                    .as_ref()
                    .is_some_and(|path| path.is_file());
                (group.members.len(), group.status, group.ml_status, written)
            })
            .collect();
        assert_eq!(
            outcome,
            [
                (2, GroupStatus::Written, MlStatus::None, true),
                (3, GroupStatus::Written, MlStatus::Pending, true)
            ],
            "job {job}"
        );
    }
    // The first task kept the embeddings it computed; the second job had
    // left review before its task ran, which then embedded nothing.
    assert_eq!((model.calls(), model.images()), (1, 3));
    assert_eq!(stored_embeddings(&dirs.db()), 3);
}

#[test]
fn stage3_that_runs_again_clears_the_stored_warning() {
    let (_data, dirs) = data_dir();
    let input = stage3_folder();
    let failing = FakeModel::failing("the model exploded");
    let review = Review::on(start_with(&dirs, &failing), ml_config(input.path(), NEAR));
    let job = review.job;
    assert!(review.engine.job(job).unwrap().warning.is_some());
    review.shutdown();

    // The next run's model works: the job's groups are checked again.
    let model = FakeModel::new();
    let engine = start_with(&dirs, &model);
    let events = engine.events();
    engine.set_thresholds(job, NEAR, WINDOW, None).unwrap();
    assert_eq!(events.try_recv(), Ok(EngineEvent::GroupsChanged { job }));
    let seen = wait_for(&events, |event| {
        *event == EngineEvent::GroupsChanged { job }
    });
    assert!(
        errors(&seen).is_empty() && warnings(&seen).is_empty(),
        "{seen:#?}"
    );
    assert_eq!(engine.job(job).unwrap().warning, None);
    assert!(
        engine
            .groups(job)
            .unwrap()
            .iter()
            .any(|group| group.ml_status == MlStatus::Split)
    );
    assert_eq!(model.images(), 3);
}

#[test]
fn panicking_model_means_skipped_with_one_warning() {
    photostacker_core::guard::install_panic_hook();
    let (_data, dirs) = data_dir();
    let input = stage3_folder();
    let model = FakeModel::panicking();
    let review = Review::on(start_with(&dirs, &model), ml_config(input.path(), NEAR));
    let (engine, job) = (&review.engine, review.job);

    let warned = warnings(&review.scanned);
    let [EngineEvent::Warning { job: of, message }] = warned[..] else {
        panic!("{:#?}", review.scanned);
    };
    assert_eq!(*of, Some(job));
    assert!(
        message.starts_with("the visual check was skipped: internal error while embedding: ")
            && message.contains("the model panicked"),
        "{message}"
    );
    assert_eq!(engine.job(job).unwrap().status, JobStatus::ReviewPending);
    assert_eq!(review.ml("b_1.jpg"), (MlStatus::Skipped, None));
    // The job's Stage 3 stays skipped: the model is not called again.
    engine.set_thresholds(job, NEAR, WINDOW, Some(0.9)).unwrap();
    thread::sleep(Duration::from_millis(300));
    assert_eq!(
        review.events.try_recv(),
        Ok(EngineEvent::GroupsChanged { job })
    );
    assert_eq!(review.events.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(model.calls(), 1);
}

// --- What a UI hears and reads ---

/// The `FileFailed` events among `events`, as `(job, path, message)`.
fn file_failures(events: &[EngineEvent]) -> Vec<(JobId, &Path, &str)> {
    events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::FileFailed { job, path, message } => {
                Some((*job, path.as_path(), message.as_str()))
            }
            _ => None,
        })
        .collect()
}

/// The events received so far.
fn received(events: &Receiver<EngineEvent>) -> Vec<EngineEvent> {
    events.try_iter().collect()
}

#[test]
fn a_file_that_fails_is_reported_while_the_scan_runs() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    let broken = input.path().join("broken.jpg");
    fs::write(&broken, b"not a jpeg").unwrap();
    let engine = start(&dirs);
    let events = engine.events();

    let job = engine
        .submit(JobConfig::new(input.path().to_path_buf()))
        .unwrap();
    let seen = wait_for(&events, is_status(job, JobStatus::ReviewPending));
    let failed = file_failures(&seen);
    let [(of, path, message)] = failed[..] else {
        panic!("{seen:#?}");
    };
    assert_eq!((of, path), (job, broken.as_path()));
    // Reported once: nothing more follows the review.
    thread::sleep(Duration::from_millis(300));
    assert_eq!(file_failures(&received(&events)), []);

    let files = engine.files(job).unwrap();
    let file = files.iter().find(|file| file.path == broken).unwrap();
    assert_eq!(file.error.as_deref(), Some(message));
    assert_eq!(engine.job(job).unwrap().counts.errors, 1);
}

#[test]
fn a_raw_that_gives_way_to_its_jpeg_is_not_reported() {
    let (_data, dirs) = data_dir();
    let input = bracket_folder();
    // A RAW that is a decoy (JPEG bytes named .cr3): it fails, and its JPEG
    // takes its place.
    let jpeg = jpeg_with_exif(&scene(7, WIDTH, HEIGHT), &canon_exif(30, 1).build(), 90);
    fs::write(input.path().join("pair.jpg"), &jpeg).unwrap();
    fs::write(input.path().join("pair.cr3"), &jpeg).unwrap();
    let engine = start(&dirs);
    let events = engine.events();

    let job = engine
        .submit(JobConfig::new(input.path().to_path_buf()))
        .unwrap();
    let mut seen = wait_for(&events, is_status(job, JobStatus::ReviewPending));
    thread::sleep(Duration::from_millis(300));
    seen.extend(received(&events));
    assert_eq!(file_failures(&seen), [], "{seen:#?}");

    // Preconditions: the RAW gave way, and nothing failed.
    let files = engine.files(job).unwrap();
    let id = |name: &str| files.iter().position(|f| f.path.ends_with(name)).unwrap();
    let (raw, jpg) = (id("pair.cr3"), id("pair.jpg"));
    assert_eq!(
        (files[raw].paired_with, files[jpg].paired_with),
        (Some(jpg), None)
    );
    assert!(files.iter().all(|file| file.error.is_none()), "{files:#?}");
}

#[test]
fn a_swap_sends_files_changed_then_groups_changed() {
    let (_data, dirs) = data_dir();
    let input = tempfile::tempdir().unwrap();
    write_raw_bracket(input.path(), "brk", 5);
    // A pair whose JPEG cannot be decoded.
    let lone = plain_jpeg(&scene(50, WIDTH, HEIGHT));
    fs::write(input.path().join("bad.raf"), raf_with_jpeg(&lone)).unwrap();
    fs::write(input.path().join("bad.jpg"), b"not a jpeg").unwrap();
    let review = Review::start(&dirs, JobConfig::new(input.path().to_path_buf()));
    let (engine, job) = (&review.engine, review.job);
    review.drain();

    engine.swap_active(job, review.id("brk_2.jpg")).unwrap();
    let seen = wait_for(&review.events, |event| {
        *event == EngineEvent::GroupsChanged { job }
    });
    assert_eq!(
        seen,
        [
            EngineEvent::FilesChanged { job },
            EngineEvent::GroupsChanged { job }
        ]
    );

    // A swap that is refused changes no file.
    engine.swap_active(job, review.id("bad.jpg")).unwrap();
    let mut seen = wait_for(&review.events, |event| {
        matches!(event, EngineEvent::Error { .. })
    });
    thread::sleep(Duration::from_millis(300));
    seen.extend(received(&review.events));
    assert_eq!(seen.len(), 1, "{seen:#?}");
}

#[test]
fn the_review_snapshot_holds_the_job_its_groups_and_the_tray() {
    let (_data, dirs) = data_dir();
    let input = review_folder();
    let review = Review::start(&dirs, JobConfig::new(input.path().to_path_buf()));
    let (engine, job) = (&review.engine, review.job);
    // A pin and a confirmed group, so every part has something of the review.
    review.move_file("a_2.jpg", MoveTarget::Tray);
    engine.confirm(job, review.group("b_1.jpg").id).unwrap();

    let snapshot = engine.review(job).unwrap();
    assert_eq!(snapshot.job, engine.job(job).unwrap());
    assert_eq!(snapshot.groups, engine.groups(job).unwrap());
    assert_eq!(snapshot.unstacked, engine.unstacked(job).unwrap());
    assert_eq!(
        review.names(&snapshot.unstacked),
        ["a_1.jpg", "a_2.jpg", "b_3.jpg"]
    );
    assert_eq!(snapshot.job.counts.confirmed, 1);
    assert!(matches!(
        engine.review(job + 1),
        Err(EngineError::UnknownJob(_))
    ));
}

#[test]
fn preferences_round_trip_through_the_engine() {
    let (_data, dirs) = data_dir();
    let engine = start(&dirs);
    assert_eq!(engine.pref::<String>("theme").unwrap(), None);
    engine.set_pref("theme", "dark").unwrap();
    engine.set_pref("last_folders", &["/a", "/b"]).unwrap();
    assert_eq!(
        engine.pref::<String>("theme").unwrap().as_deref(),
        Some("dark")
    );
    engine.set_pref("theme", "light").unwrap();
    assert_eq!(
        engine.pref::<String>("theme").unwrap().as_deref(),
        Some("light")
    );
    // A value of another type is an error, not a missing preference.
    assert!(matches!(
        engine.pref::<u32>("theme"),
        Err(EngineError::Store(StoreError::Json(_)))
    ));
    engine.shutdown();

    // They survive a restart of the engine.
    let engine = start(&dirs);
    assert_eq!(
        engine.pref::<String>("theme").unwrap().as_deref(),
        Some("light")
    );
    assert_eq!(
        engine.pref::<Vec<String>>("last_folders").unwrap(),
        Some(vec!["/a".to_owned(), "/b".to_owned()])
    );
}

// --- Models ---

/// Why each address of a failing [`FakeInstaller`]'s download fails.
const FAILED_REASON: &str = "HTTP status 404";

/// A stand-in for `models::install`, so the model commands are tested
/// without HTTP. A download reports a few progress steps, then puts a model
/// file of the registry's size in place, as an import does. It can be made
/// to fail, and its download to wait for its cancel.
struct FakeInstaller {
    /// Set once it installed a model: a [`FakeModel`] whose `installed` is
    /// this flag becomes available.
    installed: Arc<AtomicBool>,
    /// Its downloads fail with [`ModelError::AllUrlsFailed`], its imports
    /// with [`ModelError::HashMismatch`].
    fails: bool,
    /// Its download puts a tenth of the model in its part, reports it, and
    /// waits for its cancel, then at `gate`, before it returns
    /// [`ModelError::Cancelled`].
    holds: AtomicBool,
    gate: Arc<Gate>,
    /// A held download saw its cancel.
    saw_cancel: AtomicBool,
    /// The calls that returned.
    returned: AtomicUsize,
    /// The name of the thread of each call.
    threads: Mutex<Vec<String>>,
    /// The files it was asked to import.
    imported: Mutex<Vec<PathBuf>>,
}

impl FakeInstaller {
    fn new() -> Arc<FakeInstaller> {
        FakeInstaller::with(false, false)
    }

    fn failing() -> Arc<FakeInstaller> {
        FakeInstaller::with(true, false)
    }

    /// An installer whose downloads wait for their cancel.
    fn holding() -> Arc<FakeInstaller> {
        FakeInstaller::with(false, true)
    }

    fn with(fails: bool, holds: bool) -> Arc<FakeInstaller> {
        Arc::new(FakeInstaller {
            installed: Arc::default(),
            fails,
            holds: AtomicBool::new(holds),
            gate: Arc::new(Gate::holding(|| true)),
            saw_cancel: AtomicBool::new(false),
            returned: AtomicUsize::new(0),
            threads: Mutex::default(),
            imported: Mutex::default(),
        })
    }

    fn threads(&self) -> Vec<String> {
        self.threads.lock().unwrap().clone()
    }

    fn imported(&self) -> Vec<PathBuf> {
        self.imported.lock().unwrap().clone()
    }

    /// Records the thread of a call.
    fn enter(&self) {
        let name = thread::current().name().unwrap_or_default().to_owned();
        self.threads.lock().unwrap().push(name);
    }

    fn install(&self, spec: &ModelSpec, models_dir: &Path) -> PathBuf {
        let file = place_model(models_dir, spec);
        self.installed.store(true, Ordering::SeqCst);
        file
    }

    /// The download of a holding installer.
    fn hold(
        &self,
        spec: &ModelSpec,
        models_dir: &Path,
        progress: &dyn Fn(u64, u64),
        cancel: &AtomicBool,
    ) -> Result<PathBuf, ModelError> {
        let part = part_of(models_dir, spec);
        fs::create_dir_all(part.parent().unwrap()).unwrap();
        fs::File::create(&part)
            .unwrap()
            .set_len(spec.size / 10)
            .unwrap();
        progress(spec.size / 10, spec.size);
        poll("the cancel", || cancel.load(Ordering::SeqCst));
        self.saw_cancel.store(true, Ordering::SeqCst);
        self.gate.pass();
        Err(ModelError::Cancelled)
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
        self.enter();
        let result = if self.fails {
            Err(ModelError::AllUrlsFailed(
                spec.urls
                    .iter()
                    .map(|url| UrlFailure {
                        url: (*url).to_owned(),
                        reason: FAILED_REASON.to_owned(),
                    })
                    .collect(),
            ))
        } else if self.holds.load(Ordering::SeqCst) {
            self.hold(spec, models_dir, progress, cancel)
        } else {
            // The second and the third step are within one percent.
            for done in [
                0,
                spec.size / 4,
                spec.size / 4 + 1,
                spec.size / 2,
                spec.size,
            ] {
                progress(done, spec.size);
            }
            Ok(self.install(spec, models_dir))
        };
        self.returned.fetch_add(1, Ordering::SeqCst);
        result
    }

    fn import(
        &self,
        spec: &ModelSpec,
        file: &Path,
        models_dir: &Path,
    ) -> Result<PathBuf, ModelError> {
        self.enter();
        self.imported.lock().unwrap().push(file.to_path_buf());
        let result = if self.fails {
            Err(ModelError::HashMismatch {
                path: file.to_path_buf(),
                id: spec.id,
                expected: spec.sha256,
                actual: "0".repeat(64),
            })
        } else {
            Ok(self.install(spec, models_dir))
        };
        self.returned.fetch_add(1, Ordering::SeqCst);
        result
    }
}

/// Waits until `done` says so.
fn poll(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !done() {
        assert!(Instant::now() < deadline, "{what} never came");
        thread::sleep(Duration::from_millis(5));
    }
}

/// Puts a sparse model file of `spec`'s size in place: the model is
/// installed.
fn place_model(models_dir: &Path, spec: &ModelSpec) -> PathBuf {
    let file = models::model_file(models_dir, spec);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    photostacker_testkit::sparse_file(&file, spec.size).unwrap();
    file
}

/// The unfinished download of `spec`.
fn part_of(models_dir: &Path, spec: &ModelSpec) -> PathBuf {
    models_dir.join(spec.id).join("model.onnx.part")
}

/// An engine whose model commands install with `installer`, and whose
/// Stage 3 embeds with `model` when one is given.
fn start_installing(
    dirs: &AppDirs,
    model: Option<&Arc<FakeModel>>,
    installer: &Arc<FakeInstaller>,
) -> Engine {
    let mut options = options(dirs, no_wake());
    options.embedder = model.map(|model| Arc::clone(model) as Arc<dyn EmbedSource>);
    options.installer = Some(Arc::clone(installer) as Arc<dyn ModelInstall>);
    Engine::start(options).unwrap()
}

/// The registry entry of [`MODEL`].
fn model_spec() -> &'static ModelSpec {
    models::model(MODEL).unwrap()
}

/// What the engine shows of [`MODEL`].
fn model_view(engine: &Engine) -> ModelView {
    engine
        .models()
        .into_iter()
        .find(|view| view.spec.id == MODEL)
        .unwrap()
}

fn idle(installed: bool, disk_bytes: u64) -> ModelView {
    ModelView {
        spec: model_spec(),
        installed,
        activity: ModelActivity::Idle,
        disk_bytes,
    }
}

fn model_changed() -> EngineEvent {
    EngineEvent::ModelChanged {
        model: MODEL.into(),
    }
}

fn download_progress(done: u64) -> EngineEvent {
    EngineEvent::ModelDownloadProgress {
        model: MODEL.into(),
        done,
        total: model_spec().size,
    }
}

fn is_download_progress(event: &EngineEvent) -> bool {
    matches!(event, EngineEvent::ModelDownloadProgress { .. })
}

fn is_model_failure(event: &EngineEvent) -> bool {
    matches!(event, EngineEvent::ModelFailed { .. })
}

/// The failure of [`MODEL`] that `message` says.
fn model_failed(message: String) -> EngineEvent {
    EngineEvent::ModelFailed {
        model: MODEL.into(),
        message,
    }
}

/// Receives events until [`MODEL`] changed `count` times.
fn wait_model_changes(events: &Receiver<EngineEvent>, count: usize) -> Vec<EngineEvent> {
    let mut seen = Vec::new();
    for _ in 0..count {
        seen.extend(wait_for(events, |event| *event == model_changed()));
    }
    seen
}

/// `result` is the refusal [`EngineError::Invalid`] with `message`.
#[track_caller]
fn assert_invalid(result: Result<(), EngineError>, message: &str) {
    match result {
        Err(EngineError::Invalid(refused)) => assert_eq!(refused, message),
        other => panic!("expected Invalid({message:?}), got {other:?}"),
    }
}

/// A file to import, in a folder of its own.
fn model_to_import() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("downloaded.onnx");
    fs::write(&file, b"weights").unwrap();
    (dir, file)
}

#[test]
fn a_download_reports_progress_then_the_model_is_installed() {
    let (_data, dirs) = data_dir();
    let installer = FakeInstaller::new();
    let engine = start_installing(&dirs, None, &installer);
    let events = engine.events();
    let size = model_spec().size;
    assert_eq!(model_view(&engine), idle(false, 0));

    engine.download_model(MODEL).unwrap();
    let seen = wait_model_changes(&events, 2);

    // One progress event per whole percent, the last at the end.
    assert_eq!(
        seen,
        [
            model_changed(),
            download_progress(0),
            download_progress(size / 4),
            download_progress(size / 2),
            download_progress(size),
            model_changed()
        ]
    );
    assert_eq!(model_view(&engine), idle(true, size));
    assert_eq!(installer.threads(), ["photostacker-model-dinov2-small"]);
    // Every model of the registry, in its order; the other one untouched.
    let views = engine.models();
    let ids: Vec<&str> = views.iter().map(|view| view.spec.id).collect();
    assert_eq!(ids, ["dinov2-small", "dinov2-base"]);
    assert!(!views[1].installed);
}

#[test]
fn cancelling_a_download_ends_it_and_keeps_its_part() {
    let (_data, dirs) = data_dir();
    let installer = FakeInstaller::holding();
    let engine = start_installing(&dirs, None, &installer);
    let _open = OpenOnDrop(Arc::clone(&installer.gate));
    let events = engine.events();
    let size = model_spec().size;

    engine.download_model(MODEL).unwrap();
    let mut seen = wait_for(&events, is_download_progress);
    engine.cancel_download(MODEL).unwrap();
    // Cancelling until its thread ends: it waits at the gate.
    assert_eq!(model_view(&engine).activity, ModelActivity::Cancelling);
    installer.gate.open();
    seen.extend(wait_model_changes(&events, 2));

    assert_eq!(
        seen,
        [
            model_changed(),
            download_progress(size / 10),
            model_changed(),
            model_changed()
        ]
    );
    assert!(installer.saw_cancel.load(Ordering::SeqCst));
    // Not installed, and its part is kept.
    assert_eq!(model_view(&engine), idle(false, size / 10));
    assert!(part_of(&dirs.models(), model_spec()).is_file());
    assert_invalid(
        engine.cancel_download(MODEL),
        "the model dinov2-small is not being downloaded",
    );

    // A new download starts.
    installer.holds.store(false, Ordering::SeqCst);
    engine.download_model(MODEL).unwrap();
    seen.extend(wait_model_changes(&events, 2));
    assert!(model_view(&engine).installed);
    assert!(errors(&seen).is_empty(), "{seen:#?}");
}

#[test]
fn a_failed_download_is_a_model_failure_and_nothing_is_installed() {
    let (_data, dirs) = data_dir();
    let installer = FakeInstaller::failing();
    let engine = start_installing(&dirs, None, &installer);
    let events = engine.events();
    // The model's own event, its id not in front of the reason, which names each
    // address by its host alone: no path, no pinned revision.
    let failed = model_failed(format!(
        "cannot download: huggingface.co: {FAILED_REASON}; github.com: {FAILED_REASON}"
    ));

    engine.download_model(MODEL).unwrap();
    let seen = wait_for(&events, is_model_failure);
    let EngineEvent::ModelFailed { message, .. } = &seen[2] else {
        panic!("{seen:#?}");
    };
    assert!(!message.contains('/'), "{message}");

    assert_eq!(seen, [model_changed(), model_changed(), failed.clone()]);
    assert_eq!(model_view(&engine), idle(false, 0));
    assert!(!models::installed(&dirs.models(), model_spec()));
    // It can be tried again.
    engine.download_model(MODEL).unwrap();
    assert_eq!(
        wait_for(&events, is_model_failure),
        [model_changed(), model_changed(), failed]
    );
}

#[test]
fn a_busy_model_refuses_a_second_download_import_or_removal() {
    let (_data, dirs) = data_dir();
    let installer = FakeInstaller::holding();
    let engine = start_installing(&dirs, None, &installer);
    let _open = OpenOnDrop(Arc::clone(&installer.gate));
    let events = engine.events();
    let (_source, file) = model_to_import();
    engine.download_model(MODEL).unwrap();
    wait_for(&events, is_download_progress);

    let busy = "the model dinov2-small is busy";
    assert_invalid(engine.download_model(MODEL), busy);
    assert_invalid(engine.import_model(MODEL, &file), busy);
    assert_invalid(engine.remove_model(MODEL), busy);
    // Still busy while it is cancelling.
    engine.cancel_download(MODEL).unwrap();
    assert_invalid(engine.download_model(MODEL), busy);
    assert_invalid(engine.import_model(MODEL, &file), busy);
    assert_invalid(engine.remove_model(MODEL), busy);
    // A model the registry does not have is refused by every command.
    let unknown = "no model dinov2-large";
    assert_invalid(engine.download_model("dinov2-large"), unknown);
    assert_invalid(engine.cancel_download("dinov2-large"), unknown);
    assert_invalid(engine.import_model("dinov2-large", &file), unknown);
    assert_invalid(engine.remove_model("dinov2-large"), unknown);

    installer.gate.open();
    let seen = wait_model_changes(&events, 2);
    assert!(errors(&seen).is_empty(), "{seen:#?}");
    // The refused commands left the model alone: one download ran, and its
    // part is still there.
    assert_eq!(installer.threads().len(), 1);
    assert!(installer.imported().is_empty());
    assert!(part_of(&dirs.models(), model_spec()).is_file());
}

#[test]
fn importing_a_file_installs_the_model() {
    let (_data, dirs) = data_dir();
    let installer = FakeInstaller::new();
    let engine = start_installing(&dirs, None, &installer);
    let events = engine.events();
    let (_source, file) = model_to_import();

    engine.import_model(MODEL, &file).unwrap();
    let seen = wait_model_changes(&events, 2);

    assert_eq!(seen, [model_changed(), model_changed()]);
    assert_eq!(model_view(&engine), idle(true, model_spec().size));
    assert_eq!(installer.imported(), [file]);
    assert_eq!(installer.threads(), ["photostacker-model-dinov2-small"]);
}

#[test]
fn a_file_that_does_not_match_is_refused() {
    let (_data, dirs) = data_dir();
    let installer = FakeInstaller::failing();
    let engine = start_installing(&dirs, None, &installer);
    let events = engine.events();
    let (_source, file) = model_to_import();

    engine.import_model(MODEL, &file).unwrap();
    let seen = wait_for(&events, is_model_failure);

    let [first, second, EngineEvent::ModelFailed { model, message }] = &seen[..] else {
        panic!("{seen:#?}");
    };
    assert_eq!([first, second], [&model_changed(), &model_changed()]);
    assert_eq!(model, MODEL);
    assert!(
        message.starts_with("cannot import: ") && message.contains("does not match dinov2-small"),
        "{message}"
    );
    assert_eq!(model_view(&engine), idle(false, 0));
}

#[test]
fn removing_a_model_frees_its_folder() {
    let (_data, dirs) = data_dir();
    let model = FakeModel::new();
    let engine = start_installing(&dirs, Some(&model), &FakeInstaller::new());
    let events = engine.events();
    let spec = model_spec();
    place_model(&dirs.models(), spec);
    fs::write(part_of(&dirs.models(), spec), vec![0; 1000]).unwrap();
    // The unfinished part counts too.
    assert_eq!(model_view(&engine), idle(true, spec.size + 1000));

    #[cfg(unix)]
    {
        // A folder whose files cannot be removed: the removal fails, says
        // why, and leaves the model installed.
        let mode = Mode::set(&dirs.models().join(MODEL), 0o555);
        let error = engine.remove_model(MODEL).unwrap_err();
        drop(mode);
        let message = error.to_string();
        let reason = message
            .strip_prefix("cannot remove dinov2-small: ")
            .unwrap_or_else(|| panic!("{message}"));
        let seen = wait_for(&events, is_model_failure);
        assert_eq!(
            seen,
            [
                model_changed(),
                model_failed(format!("cannot remove: {reason}"))
            ]
        );
        assert!(model_view(&engine).installed);
    }

    engine.remove_model(MODEL).unwrap();

    assert!(!dirs.models().join(MODEL).exists());
    assert_eq!(model_view(&engine), idle(false, 0));
    assert_eq!(events.try_recv(), Ok(model_changed()));
    // Stage 3 forgot the model it may have loaded.
    assert!(model.forgotten.load(Ordering::SeqCst) > 0);
    // Removing it again is not an error.
    engine.remove_model(MODEL).unwrap();
}

#[test]
fn installing_a_model_regroups_the_jobs_in_review_that_name_it() {
    let (_data, dirs) = data_dir();
    let input = stage3_folder();
    let installer = FakeInstaller::new();
    let gate = Arc::new(Gate::holding(|| true));
    let model = Arc::new(FakeModel {
        installed: Some(Arc::clone(&installer.installed)),
        gate: Some(Arc::clone(&gate)),
        ..FakeModel::default()
    });
    let review = Review::on(
        start_installing(&dirs, Some(&model), &installer),
        ml_config(input.path(), NEAR),
    );
    let _open = OpenOnDrop(Arc::clone(&gate));
    let (engine, job) = (&review.engine, review.job);
    // The model is not installed: `b` is ambiguous, and skipped.
    assert_eq!(review.ml("b_1.jpg"), (MlStatus::Skipped, None));
    // A job that names no model.
    let mut config = ml_config(input.path(), NEAR);
    config.ml_model = None;
    let other = engine.submit(config).unwrap();
    wait_for(&review.events, is_status(other, JobStatus::ReviewPending));
    let other_groups = engine.groups(other).unwrap();

    engine.download_model(MODEL).unwrap();
    // Regrouped with no command of ours: `b` waits for its embeddings,
    // which the embed task computes once the gate opens, then is checked.
    let mut seen = wait_for(&review.events, |event| {
        *event == EngineEvent::GroupsChanged { job }
    });
    let b = review.group("b_1.jpg");
    assert_eq!(
        (b.members.len(), b.ml_status, b.ml_score),
        (3, MlStatus::Pending, None)
    );
    gate.open();
    seen.extend(wait_for(&review.events, |event| {
        *event == EngineEvent::GroupsChanged { job }
    }));

    let b = review.group("b_1.jpg");
    assert_eq!(review.names(&b.members), ["b_1.jpg", "b_2.jpg"]);
    assert_eq!((b.ml_status, b.ml_score), (MlStatus::Split, Some(1.0)));
    assert_eq!(review.tray(), ["b_3.jpg"]);
    assert_eq!(model.images(), 3);
    // The other job is untouched.
    thread::sleep(Duration::from_millis(300));
    seen.extend(received(&review.events));
    assert!(
        !seen.contains(&EngineEvent::GroupsChanged { job: other }),
        "{seen:#?}"
    );
    assert_eq!(engine.groups(other).unwrap(), other_groups);
    assert!(
        errors(&seen).is_empty() && warnings(&seen).is_empty(),
        "{seen:#?}"
    );
}

#[test]
fn a_scan_that_ends_during_an_install_is_checked_with_the_model() {
    let (_data, dirs) = data_dir();
    let input = stage3_folder();
    let installer = FakeInstaller::new();
    let gate = Arc::new(Gate::holding(|| true));
    let model = Arc::new(FakeModel {
        installed: Some(Arc::clone(&installer.installed)),
        held_availability: Some(Arc::clone(&gate)),
        ..FakeModel::default()
    });
    let engine = start_installing(&dirs, Some(&model), &installer);
    let _open = OpenOnDrop(Arc::clone(&gate));
    let events = engine.events();
    let job = engine.submit(ml_config(input.path(), NEAR)).unwrap();
    // The scan's Stage 3 found no model, and waits before it goes on.
    poll("the scan's look for its model", || {
        model.asked.load(Ordering::SeqCst) > 0
    });

    // The install lists the jobs in review while this one is still scanning.
    engine.download_model(MODEL).unwrap();
    let mut seen = wait_model_changes(&events, 2);
    assert_eq!(engine.job(job).unwrap().status, JobStatus::Scanning);
    gate.open();
    seen.extend(wait_for(&events, is_status(job, JobStatus::ReviewPending)));
    // Once in review it is regrouped with its model, then checked by its
    // embed task.
    for _ in 0..2 {
        seen.extend(wait_for(&events, |event| {
            *event == EngineEvent::GroupsChanged { job }
        }));
    }

    let files = engine.files(job).unwrap();
    let name = |id: &usize| files[*id].path.file_name().unwrap().to_owned();
    let b = engine
        .groups(job)
        .unwrap()
        .into_iter()
        .find(|group| group.members.iter().any(|id| name(id) == "b_1.jpg"))
        .unwrap();
    let mut members: Vec<_> = b.members.iter().map(name).collect();
    members.sort();
    assert_eq!(members, ["b_1.jpg", "b_2.jpg"]);
    assert_eq!((b.ml_status, b.ml_score), (MlStatus::Split, Some(1.0)));
    assert_eq!(model.images(), 3);
    assert!(
        errors(&seen).is_empty() && warnings(&seen).is_empty(),
        "{seen:#?}"
    );
}

#[test]
fn a_model_that_failed_to_load_is_used_after_it_is_installed_again() {
    let (_data, dirs) = data_dir();
    let input = stage3_folder();
    let installer = FakeInstaller::new();
    let model = Arc::new(FakeModel {
        unloaded: AtomicBool::new(true),
        ..FakeModel::default()
    });
    let review = Review::on(
        start_installing(&dirs, Some(&model), &installer),
        ml_config(input.path(), NEAR),
    );
    let (engine, job) = (&review.engine, review.job);
    let message =
        "the visual check was skipped: cannot load dinov2-small: a corrupt file".to_string();
    assert_eq!(
        warnings(&review.scanned),
        [&EngineEvent::Warning {
            job: Some(job),
            message: message.clone()
        }]
    );
    assert_eq!(engine.job(job).unwrap().warning, Some(message));
    assert_eq!(review.ml("b_1.jpg"), (MlStatus::Skipped, None));
    assert_eq!(model.calls(), 1);

    let (_source, file) = model_to_import();
    engine.import_model(MODEL, &file).unwrap();
    // Regrouped, then checked by its embed task.
    let mut seen = wait_for(&review.events, |event| {
        *event == EngineEvent::GroupsChanged { job }
    });
    seen.extend(wait_for(&review.events, |event| {
        *event == EngineEvent::GroupsChanged { job }
    }));

    assert!(
        errors(&seen).is_empty() && warnings(&seen).is_empty(),
        "{seen:#?}"
    );
    assert_eq!(model.forgotten.load(Ordering::SeqCst), 1);
    let b = review.group("b_1.jpg");
    assert_eq!(review.names(&b.members), ["b_1.jpg", "b_2.jpg"]);
    assert_eq!((b.ml_status, b.ml_score), (MlStatus::Split, Some(1.0)));
    assert_eq!(model.calls(), 2);
    // Stage 3 ran for the job: its warning is gone.
    assert_eq!(engine.job(job).unwrap().warning, None);
}

#[test]
fn a_panic_while_an_installed_model_is_put_to_use_leaves_it_idle() {
    let (_data, dirs) = data_dir();
    let model = Arc::new(FakeModel {
        forget_panics: AtomicBool::new(true),
        ..FakeModel::default()
    });
    let engine = start_installing(&dirs, Some(&model), &FakeInstaller::new());
    let events = engine.events();

    engine.download_model(MODEL).unwrap();
    let seen = wait_for(&events, is_model_failure);

    // Its file is in place, but putting it to use failed: the model's own failure.
    let Some(EngineEvent::ModelFailed { message, .. }) = seen.last() else {
        panic!("{seen:#?}");
    };
    assert!(
        message.starts_with("cannot download: internal error: ")
            && message.contains("forgetting the model panicked"),
        "{message}"
    );
    assert_eq!(seen[seen.len() - 2], model_changed(), "{seen:#?}");
    assert_eq!(model_view(&engine).activity, ModelActivity::Idle);
    // Not busy for ever: the next command on it runs.
    engine.remove_model(MODEL).unwrap();
    assert!(!model_view(&engine).installed);
}

#[test]
fn a_panic_in_one_jobs_regrouping_leaves_the_next_job_regrouped() {
    let (_data, dirs) = data_dir();
    let input = stage3_folder();
    let installer = FakeInstaller::new();
    let model = Arc::new(FakeModel {
        installed: Some(Arc::clone(&installer.installed)),
        ..FakeModel::default()
    });
    let review = Review::on(
        start_installing(&dirs, Some(&model), &installer),
        ml_config(input.path(), NEAR),
    );
    let (engine, first) = (&review.engine, review.job);
    let second = engine.submit(ml_config(input.path(), NEAR)).unwrap();
    wait_for(&review.events, is_status(second, JobStatus::ReviewPending));
    // The first job's regrouping, the older one's, panics.
    model.availability_panics.store(true, Ordering::SeqCst);

    engine.download_model(MODEL).unwrap();
    let seen = wait_for(&review.events, |event| {
        *event == EngineEvent::GroupsChanged { job: second }
    });

    let failed: Vec<&EngineEvent> = errors(&seen);
    let [EngineEvent::Error { job, message }] = failed[..] else {
        panic!("{seen:#?}");
    };
    assert_eq!(*job, Some(first));
    assert!(
        message.contains("internal error") && message.contains("availability panicked"),
        "{message}"
    );
    assert!(
        !seen.contains(&EngineEvent::GroupsChanged { job: first }),
        "{seen:#?}"
    );
    assert!(!seen.iter().any(is_model_failure), "{seen:#?}");
    assert_eq!(model_view(engine), idle(true, model_spec().size));
}

#[test]
fn shutdown_cancels_a_download_without_waiting_for_it() {
    let (_data, dirs) = data_dir();
    let installer = FakeInstaller::holding();
    let engine = start_installing(&dirs, None, &installer);
    let _open = OpenOnDrop(Arc::clone(&installer.gate));
    let events = engine.events();
    engine.download_model(MODEL).unwrap();
    let seen = wait_for(&events, is_download_progress);
    assert_eq!(seen.len(), 2, "{seen:#?}");

    // The download waits at the gate once it sees its cancel, so a shutdown
    // that waited for it would never end.
    let shutdown = thread::spawn(move || engine.shutdown());
    poll("the shutdown", || shutdown.is_finished());
    poll("the cancel", || installer.saw_cancel.load(Ordering::SeqCst));
    assert_eq!(installer.returned.load(Ordering::SeqCst), 0);
    // Its store is closed and the database free.
    let next = start(&dirs);
    installer.gate.open();
    poll("the download's end", || {
        installer.returned.load(Ordering::SeqCst) == 1
    });
    thread::sleep(Duration::from_millis(100));

    // The download's end sent nothing: the old engine's channel is closed.
    assert_eq!(events.try_recv(), Err(TryRecvError::Disconnected));
    assert_eq!(model_view(&next), idle(false, model_spec().size / 10));
}

#[cfg(not(feature = "ml"))]
#[test]
fn download_is_refused_without_ml() {
    let (_data, dirs) = data_dir();
    let engine = start(&dirs);
    let (_source, file) = model_to_import();

    let refused = "built without ml";
    assert_invalid(engine.download_model(MODEL), refused);
    assert_invalid(engine.import_model(MODEL, &file), refused);
    // A model can still be listed and removed.
    place_model(&dirs.models(), model_spec());
    assert!(model_view(&engine).installed);
    engine.remove_model(MODEL).unwrap();
    assert_eq!(model_view(&engine), idle(false, 0));
}
