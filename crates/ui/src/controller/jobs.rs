//! The jobs: the list as the engine last gave it, the selection, and what their running
//! tasks report (their progress, and the photos a scan could not read).
//!
//! A scan or a write reports while its job is Scanning or Writing, and what it reported
//! goes when the job leaves that status. An embed task (spec §5.4) runs on a job in
//! review, whose status it never changes: its progress goes when the job's regrouping,
//! or the warning or error that replaces it, is reported. An embed task can tick on
//! after its job left review (the user clicked Write, then cancelled, while it computed):
//! those ticks are not kept, since nothing would ever drop them.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use photostacker_core::engine::{JobId, JobView};
use photostacker_core::model::JobStatus;
use photostacker_core::{Engine, EngineError, Progress, Stage, thumbs};

use super::{Controller, View};
use crate::backend::Request;
use crate::format::folder_name;

/// The scan errors kept per job; the rest are only counted, so a folder of broken files
/// cannot fill the memory or the screen.
const SCAN_ERRORS_KEPT: usize = 500;

/// What the controller knows of the jobs.
#[derive(Default)]
pub(super) struct Jobs {
    /// Oldest first.
    list: Vec<JobView>,
    /// Whether the engine has given the list once: until then an empty list means
    /// nothing.
    loaded: bool,
    /// Each job's name in the list ([`job_names`]).
    names: HashMap<JobId, String>,
    selected: Option<JobId>,
    /// Each job's status as its last status event gave it: unlike the list, which the
    /// backend reads later, the events are never behind the progress.
    statuses: HashMap<JobId, JobStatus>,
    progress: HashMap<JobId, Progress>,
    scan_errors: HashMap<JobId, ScanErrors>,
    /// The jobs whose delete was sent and has neither ended (`JobDeleted`) nor failed:
    /// removing a large job's thumbnails can take seconds.
    deleting: HashSet<JobId>,
}

/// The photos a running scan could not read, as they are reported.
#[derive(Default)]
struct ScanErrors {
    /// The first [`SCAN_ERRORS_KEPT`]: each photo's path and why.
    kept: Vec<(PathBuf, String)>,
    /// How many came after those.
    dropped: usize,
}

impl Jobs {
    /// Takes `list` as the jobs; a selected job no longer in it is deselected.
    pub(super) fn replace(&mut self, list: Vec<JobView>) {
        if let Some(selected) = self.selected
            && !list.iter().any(|job| job.id == selected)
        {
            self.selected = None;
        }
        self.names = job_names(&list);
        self.list = list;
        self.loaded = true;
    }

    /// Takes `job` as the list's view of it, read after the list was: a job no longer in
    /// the list stays out of it.
    pub(super) fn refresh(&mut self, job: JobView) {
        if let Some(listed) = self.list.iter_mut().find(|listed| listed.id == job.id) {
            *listed = job;
        }
    }

    /// What a scan or a write reported goes once its job is neither scanning nor writing.
    /// A job that starts scanning or writing drops the progress it still had, so that
    /// no earlier tick reads as its task's first.
    pub(super) fn status_changed(&mut self, job: JobId, status: JobStatus) {
        let before = self.statuses.insert(job, status);
        if runs_a_task(status) {
            if !before.is_some_and(runs_a_task) {
                self.progress.remove(&job);
            }
        } else {
            self.forget_running(job);
        }
    }

    /// `job` was just submitted: it is queued, as its status events would say.
    pub(super) fn queued(&mut self, job: JobId) {
        self.statuses.insert(job, JobStatus::Queued);
    }

    pub(super) fn deleted(&mut self, job: JobId) {
        self.statuses.remove(&job);
        self.deleting.remove(&job);
        self.forget_running(job);
    }

    /// A request about `job` failed: a delete of it that was sent may be what failed, and
    /// the job is then still there.
    pub(super) fn request_failed(&mut self, job: JobId) {
        self.deleting.remove(&job);
    }

    /// `job`'s groups changed, or the engine warned or failed about it. For a job that is
    /// neither scanning nor writing, that ends its embed task, whose progress goes. A
    /// write reports its groups changed after every stack, so a Writing job keeps its
    /// progress.
    pub(super) fn reported(&mut self, job: JobId) {
        if !self.is_running(job) {
            self.progress.remove(&job);
        }
    }

    /// Whether `job` is scanning, as its status events say, which a read of the list sent
    /// before its scan started does not.
    pub(super) fn is_scanning(&self, job: JobId) -> bool {
        self.statuses.get(&job) == Some(&JobStatus::Scanning)
    }

    /// Whether `job` is scanning or writing, as its status events say.
    fn is_running(&self, job: JobId) -> bool {
        self.statuses.get(&job).copied().is_some_and(runs_a_task)
    }

    /// Whether a job is queued, scanning or writing, as the list or, ahead of it, the
    /// events say.
    fn any_working(&self) -> bool {
        let working = |status: JobStatus| status == JobStatus::Queued || runs_a_task(status);
        self.list.iter().any(|job| working(job.status))
            || self.statuses.values().copied().any(working)
    }

    /// Keeps `progress` for `job` while it can still be dropped: a scan's or a write's
    /// while the status events say the job is Scanning or Writing, and an embed task's
    /// while the job is in review (by its events, or by the list for a job none has
    /// moved since the list was read). Any other tick is late, such as an embed task's
    /// after its job was sent to its write, and is not kept: nothing would drop it.
    pub(super) fn progressed(&mut self, job: JobId, progress: Progress) {
        let status = self
            .statuses
            .get(&job)
            .copied()
            .or_else(|| self.job(job).map(|view| view.status));
        let kept = match status {
            Some(JobStatus::Scanning | JobStatus::Writing) => self.is_running(job),
            Some(JobStatus::ReviewPending) => progress.stage == Stage::Embed,
            _ => false,
        };
        if kept {
            self.progress.insert(job, progress);
        }
    }

    pub(super) fn file_failed(&mut self, job: JobId, path: PathBuf, message: String) {
        let errors = self.scan_errors.entry(job).or_default();
        if errors.kept.len() < SCAN_ERRORS_KEPT {
            errors.kept.push((path, message));
        } else {
            errors.dropped += 1;
        }
    }

    fn forget_running(&mut self, job: JobId) {
        self.progress.remove(&job);
        self.scan_errors.remove(&job);
    }

    fn job(&self, id: JobId) -> Option<&JobView> {
        self.list.iter().find(|job| job.id == id)
    }
}

/// Whether a job in `status` runs a task that reports its progress and owns the status:
/// a scan or a write.
fn runs_a_task(status: JobStatus) -> bool {
    matches!(status, JobStatus::Scanning | JobStatus::Writing)
}

/// Each job's name in the list: its folder's name, so that the jobs on folders of one
/// name (a folder scanned again, or cameras' `DCIM`) are told apart, on screen and to
/// screen readers, by their age. The oldest keeps the plain name and each later one gets
/// its rank: `trip`, `trip (2)`, `trip (3)`.
pub(super) fn job_names(list: &[JobView]) -> HashMap<JobId, String> {
    let mut by_age: Vec<&JobView> = list.iter().collect();
    by_age.sort_by_key(|job| job.id);
    let mut seen: HashMap<String, usize> = HashMap::new();
    by_age
        .into_iter()
        .map(|job| {
            let folder = folder_name(&job.config.input_dir);
            let rank = seen.entry(folder.clone()).or_insert(0);
            *rank += 1;
            let name = if *rank == 1 {
                folder
            } else {
                format!("{folder} ({rank})")
            };
            (job.id, name)
        })
        .collect()
}

impl Controller {
    /// Whether the jobs have been read once: before that, no job is no news.
    pub fn jobs_loaded(&self) -> bool {
        self.jobs.loaded
    }

    /// Job `id`'s name in the list ([`job_names`]); empty for a job not in it.
    pub fn job_name(&self, id: JobId) -> &str {
        self.jobs.names.get(&id).map_or("", String::as_str)
    }

    /// Every job, oldest first.
    pub fn jobs(&self) -> &[JobView] {
        &self.jobs.list
    }

    pub fn job(&self, id: JobId) -> Option<&JobView> {
        self.jobs.job(id)
    }

    pub fn selected(&self) -> Option<&JobView> {
        self.jobs.selected.and_then(|id| self.jobs.job(id))
    }

    /// The folder of job `id`'s review thumbnails, the engine's rule for it: a path only,
    /// nothing is looked at.
    pub fn job_thumbs(&self, id: JobId) -> PathBuf {
        thumbs::job_thumbs_dir(&self.thumbs_dir, id)
    }

    /// Selects job `id` and shows it, with its review once its analysis is saved.
    pub fn select_job(&mut self, id: JobId) {
        self.view = View::Job;
        self.choose_job(id);
    }

    /// Selects job `id` without changing what the centre shows.
    pub(super) fn choose_job(&mut self, id: JobId) {
        self.jobs.selected = Some(id);
        self.follow_the_selection();
    }

    /// How far job `id`'s running task is: its scan or write while the job is Scanning or
    /// Writing, or the embed task of a job in review until its regrouping is reported.
    pub fn progress(&self, id: JobId) -> Option<&Progress> {
        self.jobs.progress.get(&id)
    }

    /// The photos job `id`'s running scan could not read, each with why: the first 500.
    pub fn scan_errors(&self, id: JobId) -> &[(PathBuf, String)] {
        self.jobs
            .scan_errors
            .get(&id)
            .map_or(&[], |errors| errors.kept.as_slice())
    }

    /// How many more photos than [`Controller::scan_errors`] holds job `id`'s scan could
    /// not read.
    pub fn scan_errors_dropped(&self, id: JobId) -> usize {
        self.jobs
            .scan_errors
            .get(&id)
            .map_or(0, |errors| errors.dropped)
    }

    /// Whether the engine is working on a job: one is queued, scanning or writing
    /// ([`Controller::is_working`]), or one reports progress, such as a visual check
    /// computing embeddings for a job in review. The inspector asks for no larger preview
    /// meanwhile: the engine may hold a full-resolution image of its own. Once the engine
    /// has stopped nothing runs, whatever the jobs' last statuses say.
    pub fn engine_busy(&self) -> bool {
        self.engine_stopped.is_none() && (self.is_working() || !self.jobs.progress.is_empty())
    }

    /// Whether a job is queued, scanning or writing, as the list says, or, ahead of it,
    /// the status events; or a submission of the new-job form is not answered yet, whose
    /// job is about to be queued. A close of the window asks first meanwhile.
    pub fn is_working(&self) -> bool {
        self.jobs.any_working() || !self.submissions.is_empty()
    }

    pub fn cancel(&mut self, id: JobId) {
        self.run(id, move |engine| engine.cancel(id));
    }

    pub fn retry(&mut self, id: JobId) {
        self.run(id, move |engine| engine.retry(id));
    }

    /// Whether job `id` is being deleted: its delete was sent, and has neither ended nor
    /// failed. Its row says so, and no command on it is sent meanwhile.
    pub fn is_deleting(&self, id: JobId) -> bool {
        self.jobs.deleting.contains(&id)
    }

    /// Deletes job `id`, unless it is being deleted already.
    pub fn delete(&mut self, id: JobId) {
        self.run(id, move |engine| engine.delete(id));
        self.jobs.deleting.insert(id);
    }

    /// Sends `command`, a command of job `id`, to the backend; nothing while the job is
    /// being deleted, which would refuse it once it is gone.
    pub(super) fn run(
        &self,
        id: JobId,
        command: impl FnOnce(&Engine) -> Result<(), EngineError> + Send + 'static,
    ) {
        if self.is_deleting(id) {
            return;
        }
        self.send(Request::Run {
            job: Some(id),
            command: Box::new(command),
        });
    }
}

#[cfg(test)]
mod tests {
    use photostacker_core::engine::JobCounts;
    use photostacker_core::{JobConfig, Stage};

    use super::*;

    fn progress(stage: Stage) -> Progress {
        Progress {
            stage,
            done: 1,
            total: 2,
            current: None,
        }
    }

    #[test]
    fn scan_errors_keep_the_first_500_and_go_with_the_scan() {
        let mut jobs = Jobs::default();
        // The status event comes before what the scan reports.
        jobs.status_changed(1, JobStatus::Scanning);
        for n in 0..SCAN_ERRORS_KEPT + 3 {
            jobs.file_failed(
                1,
                PathBuf::from(format!("/photos/{n}.jpg")),
                "broken".into(),
            );
        }
        jobs.progressed(1, progress(Stage::Hash));
        let errors = &jobs.scan_errors[&1];
        assert_eq!(errors.kept.len(), SCAN_ERRORS_KEPT);
        assert_eq!(errors.kept[0].0, PathBuf::from("/photos/0.jpg"));
        assert_eq!(errors.dropped, 3);

        // Still scanning: kept.
        jobs.status_changed(1, JobStatus::Scanning);
        assert!(jobs.scan_errors.contains_key(&1) && jobs.progress.contains_key(&1));
        jobs.status_changed(1, JobStatus::ReviewPending);
        assert!(jobs.scan_errors.is_empty() && jobs.progress.is_empty());
    }

    #[test]
    fn an_embeds_progress_on_a_job_in_review_goes_with_its_regrouping() {
        let mut jobs = Jobs::default();
        jobs.status_changed(1, JobStatus::ReviewPending);
        jobs.progressed(1, progress(Stage::Embed));
        assert!(jobs.progress.contains_key(&1));
        // Another job's regrouping leaves it.
        jobs.reported(2);
        assert!(jobs.progress.contains_key(&1));
        jobs.reported(1);
        assert!(jobs.progress.is_empty());
    }

    #[test]
    fn a_scan_or_a_write_keeps_its_progress_and_scan_errors_when_its_groups_change() {
        let mut jobs = Jobs::default();
        jobs.status_changed(1, JobStatus::Writing);
        jobs.progressed(1, progress(Stage::Write));
        jobs.status_changed(2, JobStatus::Scanning);
        jobs.progressed(2, progress(Stage::Hash));
        jobs.file_failed(2, PathBuf::from("/photos/a.jpg"), "broken".into());
        jobs.reported(1);
        jobs.reported(2);
        assert!(jobs.progress.contains_key(&1) && jobs.progress.contains_key(&2));
        assert!(jobs.scan_errors.contains_key(&2));

        // Once it has left Writing, the job is no longer one whose regrouping it waits for.
        jobs.status_changed(1, JobStatus::Completed);
        jobs.progressed(1, progress(Stage::Embed));
        jobs.reported(1);
        assert!(!jobs.progress.contains_key(&1));
    }

    #[test]
    fn a_job_that_starts_scanning_or_writing_drops_an_earlier_progress() {
        let mut jobs = Jobs::default();
        // The job was queued for its write while its embed task still ticked.
        jobs.status_changed(1, JobStatus::Queued);
        jobs.progressed(1, progress(Stage::Embed));
        jobs.status_changed(1, JobStatus::Writing);
        assert!(!jobs.progress.contains_key(&1));

        // Once writing, a repeated status keeps the write's own progress.
        jobs.progressed(1, progress(Stage::Write));
        jobs.status_changed(1, JobStatus::Writing);
        assert_eq!(jobs.progress[&1].stage, Stage::Write);
    }

    #[test]
    fn a_tick_of_a_job_that_left_its_task_or_its_review_is_not_kept() {
        let mut jobs = Jobs::default();
        jobs.status_changed(1, JobStatus::ReviewPending);
        jobs.progressed(1, progress(Stage::Embed));
        assert!(jobs.progress.contains_key(&1));
        // Sent to its write, then cancelled, while its embed task still ticks.
        jobs.status_changed(1, JobStatus::Queued);
        assert!(!jobs.progress.contains_key(&1));
        jobs.progressed(1, progress(Stage::Embed));
        assert!(!jobs.progress.contains_key(&1));
        jobs.status_changed(1, JobStatus::Cancelled);
        jobs.progressed(1, progress(Stage::Embed));
        assert!(jobs.progress.is_empty());

        // A job in review as the list gives it, which no event has moved since: only an
        // embed task reports on it.
        jobs.replace(vec![job_on(2, "/photos/trip")]);
        jobs.progressed(2, progress(Stage::Hash));
        assert!(jobs.progress.is_empty());
        jobs.progressed(2, progress(Stage::Embed));
        assert!(jobs.progress.contains_key(&2));
        // A scan's tick counts only once its status event says it is scanning.
        let mut listed = job_on(3, "/photos/city");
        listed.status = JobStatus::Scanning;
        jobs.replace(vec![listed]);
        jobs.progressed(3, progress(Stage::Hash));
        assert!(!jobs.progress.contains_key(&3));
    }

    fn job_on(id: JobId, input: &str) -> JobView {
        JobView {
            id,
            created_at: "2026-11-11T10:00:00.000Z".into(),
            status: JobStatus::ReviewPending,
            scan_done: true,
            config: JobConfig::new(PathBuf::from(input)),
            error: None,
            warning: None,
            counts: JobCounts::default(),
        }
    }

    #[test]
    fn jobs_on_folders_of_one_name_are_numbered_by_age() {
        let list = [
            job_on(1, "/photos/2026/trip"),
            job_on(2, "/photos/city"),
            job_on(3, "/backup/trip"),
            job_on(4, "/photos/2026/trip"),
        ];
        let names = job_names(&list);
        assert_eq!(names[&1], "trip");
        assert_eq!(names[&2], "city");
        assert_eq!(names[&3], "trip (2)");
        assert_eq!(names[&4], "trip (3)");

        // Once the oldest is gone, the next one keeps the plain name.
        let names = job_names(&list[1..]);
        assert_eq!(names[&2], "city");
        assert_eq!(names[&3], "trip");
        assert_eq!(names[&4], "trip (2)");
    }

    #[test]
    fn a_job_that_leaves_the_list_is_deselected() {
        let mut jobs = Jobs {
            selected: Some(1),
            ..Jobs::default()
        };
        jobs.replace(Vec::new());
        assert_eq!(jobs.selected, None);
    }
}
