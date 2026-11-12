//! Everything the views show, and what they ask for (spec §4.3). No drawing code: the
//! views read the controller and call it, and it is tested without a window.
//!
//! The controller never calls the engine after [`Controller::start`]. It sends a
//! [`Request`] to the [`Backend`], whose thread does, and at the start of each frame
//! [`Controller::poll`] takes in the engine's events and the backend's replies. The
//! events drive the reads: a job queued, changing status or deleted asks for the jobs
//! again, a change of the open review's groups or files asks for those again, and a
//! change of a model asks for the models, so what the views show follows the engine
//! without the UI asking on its own.

pub mod form;
mod jobs;
mod models;
mod reveal;
mod review;
mod settings;

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crossbeam_channel::{Receiver, Sender};
use photostacker_core::engine::{EmbedSource, EngineOptions, JobId, ModelInstall, Wake};
use photostacker_core::{AppDirs, Engine, EngineError, EngineEvent, JobConfig};

use crate::backend::{Backend, Reply, Request};
use crate::format::{display_path, engine_error_text, sentence};
use crate::platform::Platform;
use crate::prefs::Prefs;
use form::Submissions;
pub use form::{Field, JobForm};
use jobs::Jobs;
use models::Models;
pub use models::{LibraryProblem, MlState, ModelFailure, OsVersion, RuntimeCheck};
pub use reveal::{REVEAL_THREAD, Reveal, reveal_in_the_background};
use review::Thresholds;
pub use review::{ResultCounts, Review, Selection, WriteCounts, shows_results};

/// What the controller starts with.
pub struct StartOptions {
    /// Where the database, the thumbnails and the models are.
    pub dirs: AppDirs,
    /// Called after each event and reply, from other threads: wakes the window.
    pub wake: Wake,
    pub platform: Arc<dyn Platform>,
    /// Stage 3's embeddings instead of the installed models: for tests.
    pub embedder: Option<Arc<dyn EmbedSource>>,
    /// What installs the models instead of the downloader: for tests.
    pub installer: Option<Arc<dyn ModelInstall>>,
    /// Where ONNX Runtime was looked for, and what keeps it from loading, if anything.
    pub runtime: RuntimeCheck,
    /// The folder of the licence texts the package ships
    /// ([`crate::platform::licences_folder`]), looked for before the first frame; `None`
    /// where there is none, as in a development build: the About card then offers none.
    pub licences: Option<PathBuf>,
}

/// Why the app cannot start.
#[derive(Debug)]
pub enum StartError {
    /// The platform has no home folder to keep the app's data in.
    NoAppDirs,
    /// Another Photostacker, this app or the CLI, is using the database at this path.
    AlreadyRunning(PathBuf),
    /// The engine cannot start on the database at `db`: it cannot be locked or opened.
    /// The error is boxed, to keep the `Result` of [`Controller::start`] small on every
    /// system.
    Engine {
        db: PathBuf,
        error: Box<EngineError>,
    },
    /// A folder of the app's data cannot be created, so the database at `db` is never
    /// opened.
    Io { db: PathBuf, error: io::Error },
    /// The thread that holds the engine cannot be started.
    Thread(io::Error),
}

impl StartError {
    /// The folder of the database the app could not open, for a failure of the database's.
    pub fn data_folder(&self) -> Option<&Path> {
        match self {
            StartError::Engine { db, .. } | StartError::Io { db, .. } => db.parent(),
            StartError::NoAppDirs | StartError::AlreadyRunning(_) | StartError::Thread(_) => None,
        }
    }
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartError::NoAppDirs => write!(f, "there is no home folder to keep the data in"),
            StartError::AlreadyRunning(db) => {
                write!(f, "another Photostacker is using {}", display_path(db))
            }
            StartError::Engine { db, error } => {
                write!(
                    f,
                    "the engine cannot start on {}: {error}",
                    display_path(db)
                )
            }
            StartError::Io { db, error } => write!(
                f,
                "the app's folders for {} cannot be made: {error}",
                display_path(db)
            ),
            StartError::Thread(error) => write!(f, "a thread of the app cannot start: {error}"),
        }
    }
}

impl std::error::Error for StartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StartError::Engine { error, .. } => Some(error.as_ref()),
            StartError::Io { error, .. } | StartError::Thread(error) => Some(error),
            StartError::NoAppDirs | StartError::AlreadyRunning(_) => None,
        }
    }
}

/// What the centre shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// The selected job.
    Job,
    NewJob,
    /// Settings, over the view they were opened from, which `Done` shows again.
    Settings,
}

/// How serious a notice is, the least first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

/// A message for the user, shown once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub level: NoticeLevel,
    pub text: String,
}

/// The most notices shown at once: past them, one says how many more the log has, so a
/// failure repeated for every stack does not fill the window with toasts.
pub const NOTICES_SHOWN: usize = 3;

/// `notices`, taken together, as the window shows them: equal ones (the same level and
/// text) once, in the order they first came; past the first [`NOTICES_SHOWN`], one more
/// notice, "And <n> more: see the log.", at the most severe level of those it stands
/// for. Each distinct notice is logged at its level, with how many times it came, shown
/// or not.
fn shown_notices(notices: Vec<Notice>) -> Vec<Notice> {
    let mut distinct: Vec<(Notice, usize)> = Vec::new();
    for notice in notices {
        match distinct.iter_mut().find(|(seen, _)| *seen == notice) {
            Some((_, times)) => *times += 1,
            None => distinct.push((notice, 1)),
        }
    }
    for (notice, times) in &distinct {
        log_notice(notice, *times);
    }
    let mut shown: Vec<Notice> = distinct.into_iter().map(|(notice, _)| notice).collect();
    if shown.len() > NOTICES_SHOWN {
        let hidden = shown.split_off(NOTICES_SHOWN);
        let level = hidden
            .iter()
            .map(|notice| notice.level)
            .max()
            .unwrap_or(NoticeLevel::Info);
        shown.push(Notice {
            level,
            text: format!("And {} more: see the log.", hidden.len()),
        });
    }
    shown
}

/// Logs `notice`, which came `times` times, at its level.
fn log_notice(notice: &Notice, times: usize) {
    let text = &notice.text;
    match notice.level {
        NoticeLevel::Info => tracing::info!(times, "notice: {text}"),
        NoticeLevel::Warning => tracing::warn!(times, "notice: {text}"),
        NoticeLevel::Error => tracing::error!(times, "notice: {text}"),
    }
}

/// What the app asks before it acts. `App` draws the one that is open as a dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Question {
    /// Delete this job, and everything the app keeps for it?
    DeleteJob(JobId),
    /// Download this model of the registry, from where and how large?
    DownloadModel(&'static str),
    /// Quit while a job runs? It stops, and continues at the next start (spec §5.7). The
    /// answer is the window's: `App` closes it after `Quit`.
    Quit,
    /// Write `to_write` stacks of `job` now, leaving `proposed` stacks undecided? The
    /// review of a job ends with its write (spec §5.7), so they can never be confirmed in
    /// it.
    Write {
        job: JobId,
        to_write: usize,
        proposed: usize,
    },
}

/// The app's state between frames.
pub struct Controller {
    pub prefs: Prefs,
    backend: Backend,
    platform: Arc<dyn Platform>,
    /// Wakes the window when the controller's own threads have news.
    wake: Wake,
    /// The folder of every job's review thumbnails ([`AppDirs::thumbs`]).
    thumbs_dir: PathBuf,
    /// The folder of the app's log files ([`AppDirs::logs`]).
    logs_dir: PathBuf,
    /// The folder of the package's licence texts, if it has one.
    licences_dir: Option<PathBuf>,
    jobs: Jobs,
    /// The selected job's review, once its analysis is saved.
    review: Option<Review>,
    /// The thresholds last sent for each job whose review has not shown them stored yet.
    pending_thresholds: HashMap<JobId, Thresholds>,
    view: View,
    /// The view Settings were opened over, which closing them shows again.
    behind_settings: View,
    question: Option<Question>,
    notices: Vec<Notice>,
    /// The notices of the controller's own threads (a reveal's), sent and taken in.
    post: Sender<Notice>,
    posted: Receiver<Notice>,
    /// Why the engine stopped, once it has.
    engine_stopped: Option<String>,
    /// The answers to the submissions of [`Controller::submit`], with their tokens, not
    /// taken yet.
    submitted: Vec<(u64, Result<JobId, EngineError>)>,
    /// The next submission's token. Atomic, so [`Controller::submit`] takes `&self`.
    tokens: AtomicU64,
    /// The new-job form, while it is open.
    form: Option<JobForm>,
    /// The form's submissions the engine has not answered yet.
    submissions: Submissions,
    /// The models of Stage 3, and whether ONNX Runtime is there.
    models: Models,
}

impl Controller {
    /// Creates the data folders, starts the engine, loads the preferences and reads the
    /// models, then hands the engine to the backend thread and asks for the jobs.
    pub fn start(options: StartOptions) -> Result<Controller, StartError> {
        let StartOptions {
            dirs,
            wake,
            platform,
            embedder,
            installer,
            runtime,
            licences,
        } = options;
        let db = dirs.db();
        let io = |error| StartError::Io {
            db: db.clone(),
            error,
        };
        for folder in [&dirs.data, &dirs.cache] {
            fs::create_dir_all(folder).map_err(io)?;
        }
        let engine = Engine::start(EngineOptions {
            db: db.clone(),
            thumbs_dir: dirs.thumbs(),
            models_dir: dirs.models(),
            embedder,
            installer,
            wake: Arc::clone(&wake),
        })
        .map_err(|error| match error {
            EngineError::Busy(db) => StartError::AlreadyRunning(db),
            error => StartError::Engine {
                db: db.clone(),
                error: Box::new(error),
            },
        })?;
        let prefs = Prefs::load(&engine);
        // Read here, before the first frame, so the ML chip never shows a model missing
        // for a frame while it is installed; from now on its events drive the reads.
        let models = Models::new(runtime, engine.models());
        let backend = Backend::start(engine, Arc::clone(&wake)).map_err(StartError::Thread)?;
        backend.send(Request::Jobs);
        let (post, posted) = crossbeam_channel::unbounded();
        Ok(Controller {
            prefs,
            backend,
            platform,
            wake,
            thumbs_dir: dirs.thumbs(),
            logs_dir: dirs.logs(),
            licences_dir: licences,
            jobs: Jobs::default(),
            review: None,
            pending_thresholds: HashMap::new(),
            view: View::Job,
            behind_settings: View::Job,
            question: None,
            notices: Vec::new(),
            post,
            posted,
            engine_stopped: None,
            submitted: Vec::new(),
            tokens: AtomicU64::new(0),
            form: None,
            submissions: Submissions::new(),
            models,
        })
    }

    /// Takes in the engine's events, the backend's replies and the notices of the
    /// controller's own threads. Called at the start of each frame.
    pub fn poll(&mut self) {
        for event in self.backend.events() {
            self.on_event(event);
        }
        for reply in self.backend.replies() {
            self.on_reply(reply);
        }
        self.notices.extend(self.posted.try_iter());
    }

    /// Queues `request` for the backend thread. Never waits for the engine.
    pub fn send(&self, request: Request) {
        self.backend.send(request);
    }

    /// Submits `config` as a new job and returns the token its answer will carry: the
    /// answer comes, exactly once, in [`Controller::take_submitted`].
    pub fn submit(&self, config: JobConfig) -> u64 {
        let token = self.next_token();
        self.send(Request::Submit { token, config });
        token
    }

    /// The answers to the jobs submitted with [`Controller::submit`] since the last call,
    /// in order, each with its token: the new job's id, or why it was refused. The
    /// new-job form takes in the answers to its own submissions.
    pub fn take_submitted(&mut self) -> Vec<(u64, Result<JobId, EngineError>)> {
        std::mem::take(&mut self.submitted)
    }

    /// A token no submission had before.
    fn next_token(&self) -> u64 {
        self.tokens.fetch_add(1, Ordering::Relaxed)
    }

    /// Opens `question`, replacing the one open.
    pub fn ask(&mut self, question: Question) {
        self.question = Some(question);
    }

    pub fn question(&self) -> Option<&Question> {
        self.question.as_ref()
    }

    /// Closes the open question; `yes` does what it asked, except [`Question::Quit`],
    /// whose answer `App` acts on. Nothing when none is open.
    pub fn answer(&mut self, yes: bool) {
        match self.question.take() {
            Some(Question::DeleteJob(job)) if yes => self.delete(job),
            Some(Question::DownloadModel(id)) if yes => self.download_model(id),
            Some(Question::Write { job, .. }) if yes => {
                self.run(job, move |engine| engine.write(job));
            }
            Some(
                Question::DeleteJob(_)
                | Question::DownloadModel(_)
                | Question::Quit
                | Question::Write { .. },
            )
            | None => {}
        }
    }

    /// The notices since the last call, oldest first, as the window shows them
    /// ([`shown_notices`]): equal ones once, and past the first [`NOTICES_SHOWN`], one
    /// that says how many more the log has. Each is logged at its level.
    pub fn take_notices(&mut self) -> Vec<Notice> {
        shown_notices(std::mem::take(&mut self.notices))
    }

    /// Whether the backend has nothing waiting, running or unread.
    pub fn is_idle(&self) -> bool {
        self.backend.is_idle()
    }

    /// Why the engine stopped, once it has: from then on no job can be started.
    pub fn engine_stopped(&self) -> Option<&str> {
        self.engine_stopped.as_deref()
    }

    pub fn view(&self) -> &View {
        &self.view
    }

    fn notify(&mut self, level: NoticeLevel, text: String) {
        self.notices.push(Notice { level, text });
    }

    /// Takes in `event` as if the engine had sent it: for the tests of what the window
    /// shows of an event no test can make the engine send (`EngineStopped`).
    #[cfg(test)]
    pub(crate) fn take_event(&mut self, event: EngineEvent) {
        self.on_event(event);
    }

    fn on_event(&mut self, event: EngineEvent) {
        match event {
            EngineEvent::JobQueued { job } => {
                self.jobs.queued(job);
                self.backend.send(Request::Jobs);
            }
            EngineEvent::JobStatusChanged { job, status } => {
                self.jobs.status_changed(job, status);
                self.review_status_changed(job, status);
                self.backend.send(Request::Jobs);
            }
            EngineEvent::JobDeleted { job } => {
                self.jobs.deleted(job);
                self.backend.send(Request::Jobs);
            }
            EngineEvent::JobProgress { job, progress } => self.jobs.progressed(job, progress),
            EngineEvent::FileFailed { job, path, message } => {
                self.jobs.file_failed(job, path, message);
            }
            // The engine's words, as sentences.
            EngineEvent::Warning { job, message } => {
                self.task_reported(job);
                self.notify(NoticeLevel::Warning, sentence(&message));
            }
            EngineEvent::Error { job, message } => {
                self.task_reported(job);
                self.notify(NoticeLevel::Error, sentence(&message));
            }
            EngineEvent::GroupsChanged { job } => {
                self.jobs.reported(job);
                self.groups_changed(job);
            }
            EngineEvent::FilesChanged { job } => self.files_changed(job),
            EngineEvent::EngineStopped { message } => {
                let text = format!("The engine has stopped, so no job can run: {message}");
                self.notify(NoticeLevel::Error, text);
                self.engine_stopped = Some(message);
            }
            EngineEvent::ModelChanged { .. } => self.backend.send(Request::Models),
            EngineEvent::ModelDownloadProgress { model, done, total } => {
                self.models.progressed(&model, done, total);
            }
            EngineEvent::ModelFailed { model, message } => self.models.failed(&model, &message),
        }
    }

    /// A warning or an error about `job`, when it is one job's: see [`Jobs::reported`].
    fn task_reported(&mut self, job: Option<JobId>) {
        if let Some(job) = job {
            self.jobs.reported(job);
        }
    }

    fn on_reply(&mut self, reply: Reply) {
        match reply {
            Reply::Jobs(list) => {
                self.jobs.replace(list);
                self.forget_the_thresholds_of_deleted_jobs();
                self.follow_the_selection();
            }
            Reply::Files { job, files } => self.files_read(job, files),
            Reply::Review { job, review } => self.review_read(job, *review),
            // The form's own, by its token; any other is for whoever submitted it.
            Reply::Submitted { token, result } => match self.take_submission(token) {
                Some(submission) => self.form_answered(submission, result),
                None => self.submitted.push((token, result)),
            },
            Reply::Failed { job, error } => {
                if let Some(job) = job {
                    self.jobs.request_failed(job);
                }
                self.review_request_failed(job);
                self.notify(NoticeLevel::Error, engine_error_text(&error));
            }
            Reply::ReviewUnreadable { job, error } => self.review_unreadable(job, error),
            Reply::Models(views) => self.models.read(views),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(level: NoticeLevel, text: &str) -> Notice {
        Notice {
            level,
            text: text.into(),
        }
    }

    #[test]
    fn equal_notices_of_one_poll_are_shown_once() {
        let failed = notice(NoticeLevel::Error, "The disk is full.");
        let warned = notice(NoticeLevel::Warning, "The disk is full.");
        let shown = shown_notices(vec![
            failed.clone(),
            warned.clone(),
            failed.clone(),
            failed.clone(),
        ]);
        // The same text at another level is another notice.
        assert_eq!(shown, [failed, warned]);
        assert_eq!(shown_notices(Vec::new()), []);
    }

    #[test]
    fn past_three_notices_one_says_how_many_more() {
        let first: Vec<Notice> = (1..=3)
            .map(|n| notice(NoticeLevel::Info, &format!("Notice {n}.")))
            .collect();
        assert_eq!(shown_notices(first.clone()), first);

        let mut many = first.clone();
        many.push(notice(NoticeLevel::Warning, "Notice 4."));
        many.push(notice(NoticeLevel::Error, "Notice 5."));
        many.push(notice(NoticeLevel::Error, "Notice 5."));
        let mut expected = first;
        // Two more, the worst of them an error.
        expected.push(notice(NoticeLevel::Error, "And 2 more: see the log."));
        assert_eq!(shown_notices(many), expected);
    }
}
