//! The one thread that calls the engine (spec §4.3). A review edit of a large job takes a
//! tenth of a second or more, and a read up to tens of milliseconds, so the UI thread
//! never calls the engine: it sends a [`Request`] and goes on drawing, and reads the
//! [`Reply`] and the engine's events at its next frame.
//!
//! Requests run one at a time, in the order they were sent, with three exceptions that
//! keep the queue short while the user drags a slider or the engine sends a burst of
//! events ([`Queue::push`]):
//! - a read equal to one still waiting is not queued again: the one waiting will read
//!   the newer state anyway;
//! - a [`Request::Thresholds`] replaces the one of the same job still waiting, in its
//!   place: only the last position of the slider matters;
//! - a [`Request::SavePref`] replaces the waiting save of the same preference, in its
//!   place: only the newest value need reach the database.
//!
//! A request that is running is no longer waiting, so an equal read sent meanwhile is
//! queued again: it may have to see a change the running one missed.
//!
//! When the backend is dropped (the window closes), the waiting saves of preferences
//! still run, and every other waiting request is dropped.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};

use crossbeam_channel::Receiver;
use photostacker_core::engine::{
    EngineEvent, FileView, JobId, JobView, ModelView, ReviewView, Wake,
};
use photostacker_core::{Engine, EngineError, JobConfig, guard};

/// The name of the backend's thread.
pub const BACKEND_THREAD: &str = "photostacker-ui-backend";

/// Any engine command, run on the backend thread.
pub type Command = Box<dyn FnOnce(&Engine) -> Result<(), EngineError> + Send>;

/// What the UI asks of the engine.
pub enum Request {
    /// Every job: [`Reply::Jobs`].
    Jobs,
    /// The files of a job's analysis: [`Reply::Files`].
    Files(JobId),
    /// What the review of a job shows: [`Reply::Review`].
    Review(JobId),
    /// The models of Stage 3: [`Reply::Models`].
    Models,
    /// A new job: [`Reply::Submitted`] with the same `token`, always exactly one, also
    /// when the submit panics. The controller hands out the tokens
    /// ([`crate::controller::Controller::submit`]).
    Submit { token: u64, config: JobConfig },
    /// Regroups a job in review with these thresholds and time window
    /// ([`Engine::set_thresholds`]).
    Thresholds {
        job: JobId,
        hamming: u32,
        time_window_seconds: u32,
        cosine: Option<f32>,
    },
    /// Any other command, about `job` when it is one job's.
    Run {
        job: Option<JobId>,
        command: Command,
    },
    /// Saves the preference `key` (`save` calls [`Engine::set_pref`]). Kept when the
    /// backend is dropped, unlike every other waiting request.
    SavePref { key: &'static str, save: Command },
}

impl Request {
    /// The job the request is about, if it is one job's.
    fn job(&self) -> Option<JobId> {
        match self {
            Request::Files(job) | Request::Review(job) | Request::Thresholds { job, .. } => {
                Some(*job)
            }
            Request::Run { job, .. } => *job,
            Request::Jobs | Request::Models | Request::Submit { .. } | Request::SavePref { .. } => {
                None
            }
        }
    }

    /// Whether `newer`, sent while `self` waits, makes `self` pointless: the newer
    /// thresholds of the same job, or a newer save of the same preference.
    fn is_replaced_by(&self, newer: &Request) -> bool {
        match (self, newer) {
            (Request::Thresholds { job: older, .. }, Request::Thresholds { job, .. }) => {
                older == job
            }
            (Request::SavePref { key: older, .. }, Request::SavePref { key, .. }) => older == key,
            _ => false,
        }
    }

    /// Whether `self` and `other` are the same read.
    fn is_same_read(&self, other: &Request) -> bool {
        match (self, other) {
            (Request::Jobs, Request::Jobs) | (Request::Models, Request::Models) => true,
            (Request::Files(a), Request::Files(b)) | (Request::Review(a), Request::Review(b)) => {
                a == b
            }
            _ => false,
        }
    }
}

/// What the engine answered. A successful [`Request::Run`] or [`Request::Thresholds`]
/// has no reply: the engine's events report what changed.
#[derive(Debug)]
pub enum Reply {
    Jobs(Vec<JobView>),
    Files {
        job: JobId,
        files: Vec<FileView>,
    },
    /// Boxed: a review is far larger than any other reply.
    Review {
        job: JobId,
        review: Box<ReviewView>,
    },
    Models(Vec<ModelView>),
    /// The answer to the [`Request::Submit`] of `token`: the new job, or why there is none.
    Submitted {
        token: u64,
        result: Result<JobId, EngineError>,
    },
    /// A request failed; a read of a job that is gone is not a failure, and has no reply.
    /// A submit's failure is its [`Reply::Submitted`], and a read of a review's
    /// [`Reply::ReviewUnreadable`].
    Failed {
        job: Option<JobId>,
        error: EngineError,
    },
    /// A read of the files or the review of `job` failed.
    ReviewUnreadable {
        job: JobId,
        error: EngineError,
    },
}

/// The requests waiting for the thread.
#[derive(Default)]
struct Queue {
    waiting: VecDeque<Request>,
    /// A request is running.
    running: bool,
    /// The backend is being dropped.
    quit: bool,
}

impl Queue {
    /// Queues `request` at the end, unless an equal read is waiting, or it replaces a
    /// waiting request it makes pointless, in that one's place (see the module docs).
    fn push(&mut self, request: Request) {
        let older = self
            .waiting
            .iter_mut()
            .find(|waiting| waiting.is_replaced_by(&request));
        if let Some(older) = older {
            *older = request;
        } else if !self
            .waiting
            .iter()
            .any(|waiting| waiting.is_same_read(&request))
        {
            self.waiting.push_back(request);
        }
    }
}

/// What the handle and the thread share.
struct Shared {
    queue: Mutex<Queue>,
    /// Signalled when a request is queued or the backend quits.
    work: Condvar,
    /// The replies not taken yet, in the order of their requests.
    replies: Mutex<Vec<Reply>>,
}

/// Locks `mutex`, also when a thread panicked while holding it: every holder leaves the
/// queue and the replies consistent.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The handle of the backend thread, which owns the engine.
pub struct Backend {
    shared: Arc<Shared>,
    /// The thread, which gives the engine back when it ends.
    thread: Option<JoinHandle<Engine>>,
    events: Receiver<EngineEvent>,
}

impl Backend {
    /// Hands `engine` to a new thread, [`BACKEND_THREAD`], which serves the requests and
    /// calls `wake` after each reply.
    pub fn start(engine: Engine, wake: Wake) -> io::Result<Backend> {
        let events = engine.events();
        let shared = Arc::new(Shared {
            queue: Mutex::default(),
            work: Condvar::new(),
            replies: Mutex::default(),
        });
        let thread = thread::Builder::new().name(BACKEND_THREAD.into()).spawn({
            let shared = Arc::clone(&shared);
            move || serve(engine, &shared, &wake)
        })?;
        Ok(Backend {
            shared,
            thread: Some(thread),
            events,
        })
    }

    /// Queues `request`. Never waits for the engine.
    pub fn send(&self, request: Request) {
        lock(&self.shared.queue).push(request);
        self.shared.work.notify_one();
    }

    /// The replies ready now, in the order of their requests.
    pub fn replies(&self) -> Vec<Reply> {
        std::mem::take(&mut *lock(&self.shared.replies))
    }

    /// The engine's events sent until now, in order.
    pub fn events(&self) -> Vec<EngineEvent> {
        self.events.try_iter().collect()
    }

    /// Whether nothing is waiting, running or unread: no request, no reply, no event.
    pub fn is_idle(&self) -> bool {
        // The queue first: the thread stores a reply before it marks its request done.
        let queue_idle = {
            let queue = lock(&self.shared.queue);
            queue.waiting.is_empty() && !queue.running
        };
        queue_idle && lock(&self.shared.replies).is_empty() && self.events.is_empty()
    }
}

impl Drop for Backend {
    /// Lets the running request finish, then the waiting saves of preferences run (a
    /// size the window settled on as it closed, say), the other waiting requests are
    /// dropped, the thread is joined, and only then is the engine shut down.
    fn drop(&mut self) {
        let dropped: VecDeque<Request> = {
            let mut queue = lock(&self.shared.queue);
            queue.quit = true;
            let (saves, dropped) = std::mem::take(&mut queue.waiting)
                .into_iter()
                .partition(|request| matches!(request, Request::SavePref { .. }));
            queue.waiting = saves;
            dropped
        };
        drop(dropped);
        self.shared.work.notify_all();
        if let Some(thread) = self.thread.take() {
            match thread.join() {
                Ok(engine) => engine.shutdown(),
                Err(_) => tracing::error!("the backend thread panicked"),
            }
        }
    }
}

/// The thread's loop: runs the requests one at a time until the backend quits, then gives
/// the engine back.
fn serve(engine: Engine, shared: &Shared, wake: &Wake) -> Engine {
    while let Some(request) = next_request(shared) {
        let reply = answer(&engine, request);
        let replied = reply.is_some();
        if let Some(reply) = reply {
            lock(&shared.replies).push(reply);
        }
        lock(&shared.queue).running = false;
        if replied {
            wake();
        }
    }
    engine
}

/// Waits for the next request and marks it running; `None` once the backend quits and
/// nothing it kept waits.
fn next_request(shared: &Shared) -> Option<Request> {
    let mut queue = lock(&shared.queue);
    loop {
        // After the quit, only the saves the drop kept are left.
        if let Some(request) = queue.waiting.pop_front() {
            queue.running = true;
            return Some(request);
        }
        if queue.quit {
            return None;
        }
        queue = shared
            .work
            .wait(queue)
            .unwrap_or_else(PoisonError::into_inner);
    }
}

/// Runs `request`. A panic in it is a failure of the request, and the thread goes on: a
/// submit's is its [`Reply::Submitted`], since whoever submitted waits for that one.
fn answer(engine: &Engine, request: Request) -> Option<Reply> {
    let job = request.job();
    let submit = match &request {
        Request::Submit { token, .. } => Some(*token),
        _ => None,
    };
    guard::catch(|| run(engine, request)).unwrap_or_else(|message| {
        tracing::warn!(job, "a request to the engine panicked: {message}");
        let error = EngineError::Invalid(format!("internal error: {message}"));
        Some(match submit {
            Some(token) => Reply::Submitted {
                token,
                result: Err(error),
            },
            None => Reply::Failed { job, error },
        })
    })
}

fn run(engine: &Engine, request: Request) -> Option<Reply> {
    match request {
        Request::Jobs => read(None, engine.jobs(), Reply::Jobs),
        Request::Files(job) => {
            review_read(job, engine.files(job), |files| Reply::Files { job, files })
        }
        Request::Review(job) => review_read(job, engine.review(job), |review| Reply::Review {
            job,
            review: Box::new(review),
        }),
        Request::Models => Some(Reply::Models(engine.models())),
        Request::Submit { token, config } => Some(Reply::Submitted {
            token,
            result: engine.submit(config),
        }),
        Request::Thresholds {
            job,
            hamming,
            time_window_seconds,
            cosine,
        } => failure(
            Some(job),
            engine.set_thresholds(job, hamming, time_window_seconds, cosine),
        ),
        Request::Run { job, command } => failure(job, command(engine)),
        Request::SavePref { key, save } => {
            let saved = save(engine);
            // Also logged: a save the window's close runs has no frame left to read
            // its failure.
            if let Err(error) = &saved {
                tracing::warn!(key, %error, "a preference cannot be saved");
            }
            failure(None, saved)
        }
    }
}

/// The reply to a read: its value, nothing when its job is gone (it was just deleted, and
/// its `JobDeleted` event follows), or the failure.
fn read<T>(
    job: Option<JobId>,
    result: Result<T, EngineError>,
    reply: impl FnOnce(T) -> Reply,
) -> Option<Reply> {
    match result {
        Ok(value) => Some(reply(value)),
        Err(EngineError::UnknownJob(_)) => None,
        Err(error) => Some(Reply::Failed { job, error }),
    }
}

/// The reply to a read of `job`'s review, its files or its stacks, as [`read`]'s, except
/// that its failure is [`Reply::ReviewUnreadable`].
fn review_read<T>(
    job: JobId,
    result: Result<T, EngineError>,
    reply: impl FnOnce(T) -> Reply,
) -> Option<Reply> {
    read(Some(job), result, reply).map(|reply| match reply {
        Reply::Failed { error, .. } => Reply::ReviewUnreadable { job, error },
        reply => reply,
    })
}

/// The reply to a command: nothing when it succeeded.
fn failure(job: Option<JobId>, result: Result<(), EngineError>) -> Option<Reply> {
    result.err().map(|error| Reply::Failed { job, error })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The waiting requests, in order, each as a short description.
    fn waiting(queue: &Queue) -> Vec<String> {
        queue
            .waiting
            .iter()
            .map(|request| match request {
                Request::Jobs => "jobs".to_owned(),
                Request::Files(job) => format!("files {job}"),
                Request::Review(job) => format!("review {job}"),
                Request::Models => "models".to_owned(),
                Request::Submit { .. } => "submit".to_owned(),
                Request::Thresholds { job, hamming, .. } => format!("thresholds {job} {hamming}"),
                Request::Run { .. } => "run".to_owned(),
                Request::SavePref { key, .. } => format!("save {key}"),
            })
            .collect()
    }

    /// A save of the preference `key`, which `marker` names.
    fn save(key: &'static str, marker: &'static str) -> Request {
        Request::SavePref {
            key,
            save: Box::new(move |engine| engine.set_pref(key, marker)),
        }
    }

    fn thresholds(job: JobId, hamming: u32) -> Request {
        Request::Thresholds {
            job,
            hamming,
            time_window_seconds: 30,
            cosine: None,
        }
    }

    fn run() -> Request {
        Request::Run {
            job: Some(1),
            command: Box::new(|_| Ok(())),
        }
    }

    #[test]
    fn equal_reads_and_newer_thresholds_coalesce() {
        let mut queue = Queue::default();
        for request in [
            Request::Review(1),
            thresholds(1, 5),
            Request::Review(1),
            thresholds(2, 9),
            run(),
            thresholds(1, 6),
            Request::Review(2),
            run(),
            Request::Review(1),
            thresholds(1, 7),
            Request::Jobs,
            Request::Jobs,
        ] {
            queue.push(request);
        }
        assert_eq!(
            waiting(&queue),
            [
                "review 1",
                "thresholds 1 7",
                "thresholds 2 9",
                "run",
                "review 2",
                "run",
                "jobs"
            ]
        );
    }

    #[test]
    fn a_newer_save_of_a_preference_replaces_the_waiting_one() {
        let mut queue = Queue::default();
        for request in [
            save("window", "first"),
            Request::Jobs,
            save("theme", "dark"),
            save("window", "second"),
            run(),
            save("window", "third"),
        ] {
            queue.push(request);
        }
        assert_eq!(
            waiting(&queue),
            ["save window", "jobs", "save theme", "run"]
        );
        // In its place, the newest.
        let Some(Request::SavePref { save: newest, .. }) = queue.waiting.pop_front() else {
            panic!("the window's save is first");
        };
        let data = tempfile::tempdir().unwrap();
        let dirs = photostacker_core::AppDirs::under(data.path().to_path_buf());
        let engine = Engine::start(photostacker_core::EngineOptions {
            db: dirs.db(),
            thumbs_dir: dirs.thumbs(),
            models_dir: dirs.models(),
            embedder: None,
            installer: None,
            wake: Arc::new(|| {}),
        })
        .unwrap();
        newest(&engine).unwrap();
        assert_eq!(
            engine.pref::<String>("window").unwrap().as_deref(),
            Some("third")
        );
    }

    #[test]
    fn a_read_taken_by_the_thread_is_queued_again() {
        let mut queue = Queue::default();
        queue.push(Request::Jobs);
        queue.push(thresholds(1, 5));
        // The thread takes both: they run, and wait no longer.
        queue.waiting.clear();
        queue.push(Request::Jobs);
        queue.push(thresholds(1, 6));
        assert_eq!(waiting(&queue), ["jobs", "thresholds 1 6"]);
    }
}
