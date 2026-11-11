//! The review of the selected job (spec §5.4.1, §5.7, §9.2): its files, stacks and tray
//! as the engine last gave them, what the user selected, what the sliders show, and the
//! review's commands.
//!
//! A job is opened when it is selected and its analysis is saved, and closed when another
//! job is selected, it leaves the list, or it is scanned again (a retry of a scan that
//! stopped after saving its analysis makes a new one). Opening it reads its files and its
//! review. From then on the events drive the reads: `GroupsChanged` reads the review
//! again, `FilesChanged` the files. A command never reads its own result: an equal read
//! already queued ahead of it would answer before the command runs, and absorb the read
//! sent after it. Its event, sent once the change is stored, asks for the read that sees
//! it.
//!
//! The thresholds a slider sent are remembered for their job until a read of its review
//! shows them stored, or the engine refuses them: a review closed meanwhile and opened
//! again shows them, not the list's older copy of the job.
//!
//! The selection is the controller's own: picking a stack or a photo never reaches the
//! engine. Stack ids are stable while a stack's members are unchanged (§5.4.1), so a
//! selection survives a regrouping that keeps its stacks; each read of the review drops
//! from it what is gone, and a photo no longer in the primary stack.
//!
//! The inspector's checkboxes mark photos of the primary stack for a split. The checks
//! are the controller's too, and hold only while their stack is the primary one with the
//! members it had when the first was checked: selecting another stack, or a read that
//! changes the stack's members, clears them.

use std::collections::{HashMap, HashSet};

use photostacker_core::engine::{
    FileView, GroupId, GroupView, JobId, JobView, MoveTarget, ReviewView, is_stranded,
};
use photostacker_core::model::{GroupStatus, JobStatus};
use photostacker_core::{Engine, EngineError, models};

use super::{Controller, NoticeLevel};
use crate::backend::Request;
use crate::format::sentence;

/// What the user selected in a review.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    /// The selected stacks, in the order they were picked; the last is the primary one.
    pub groups: Vec<GroupId>,
    /// The selected photo, by its id in the job's analysis.
    pub photo: Option<usize>,
}

impl Selection {
    /// The stack picked last, which the inspector shows and the steps move from.
    pub fn primary(&self) -> Option<GroupId> {
        self.groups.last().copied()
    }
}

/// The review of a job, as the engine last gave it.
#[derive(Debug, Clone, PartialEq)]
pub struct Review {
    pub job: JobId,
    /// Empty until the job's files have been read.
    pub files: Vec<FileView>,
    /// By position.
    pub groups: Vec<GroupView>,
    /// The tray, ascending. A photo's [`FileView::pinned`] is not refreshed when it moves
    /// to or from the tray (only the groups are read again then); this is.
    pub unstacked: Vec<usize>,
    pub selection: Selection,
    /// What the sliders show. Set from the job when it is opened; afterwards only the user
    /// changes them.
    pub hamming: u32,
    pub time_window_seconds: u32,
    pub cosine: Option<f32>,
    /// The model's default cosine, when the job names a model.
    pub default_cosine: Option<f32>,
    /// How many warnings each stack has, by position: its members' warnings from the
    /// analysis and its write's. Worked out once per read of the files or the stacks, so
    /// a frame never walks every stack.
    pub warning_counts: Vec<usize>,
    /// What the Write button counts, worked out once per read of the stacks.
    counts: WriteCounts,
    /// What the writing and results views count, worked out once per read of the stacks.
    results: ResultCounts,
    /// Where the stacks confirmed for a write are in `groups` (Confirmed, Written,
    /// Skipped or Failed), in position order: the writing and results views' list.
    listed: Vec<usize>,
    /// The photos the inspector's checkboxes mark in the primary stack, for a split.
    checks: Option<Checks>,
    /// Where the primary stack is in `groups`: found when the selection or the stacks
    /// change, so a frame never looks for it.
    primary_at: Option<usize>,
    /// Each active file of a RAW+JPEG pair, by id, with its passive partners, ascending:
    /// worked out once per read of the files.
    partners: HashMap<usize, Vec<usize>>,
    /// The files the scan could not read (their error is set), by id, ascending: worked
    /// out once per read of the files.
    unreadable: Vec<usize>,
    /// Whether the files have been read since the review was opened.
    files_read: bool,
    /// Whether the groups and the tray have been read since the review was opened.
    groups_read: bool,
    /// Why a read of it failed before it was loaded, as a sentence; until the next try.
    failure: Option<String>,
}

/// The thresholds of a regrouping, with its time window, as a slider sends them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Thresholds {
    hamming: u32,
    time_window_seconds: u32,
    cosine: Option<f32>,
}

impl Thresholds {
    /// The thresholds `job` has stored.
    fn of(job: &JobView) -> Thresholds {
        Thresholds {
            hamming: job.config.hamming_threshold,
            time_window_seconds: job.config.time_window_seconds,
            cosine: job.config.cosine_threshold,
        }
    }
}

/// The photos checked in a stack, with the stack's members when the first was checked:
/// the checks hold only while that stack is the primary one and its members stay as
/// they were.
#[derive(Debug, Clone, PartialEq)]
struct Checks {
    group: GroupId,
    members: Vec<usize>,
    /// In layer order.
    files: Vec<usize>,
}

/// What the Write button counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriteCounts {
    /// The Confirmed stacks of 2 photos or more that have no file yet: the next write
    /// writes them.
    pub to_write: usize,
    /// The stacks not decided yet, which a write leaves alone.
    pub proposed: usize,
}

/// What the writing and results views count of a job's stacks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResultCounts {
    pub written: usize,
    pub skipped: usize,
    pub failed: usize,
    /// The stacks a retry writes again (the engine's rule, [`is_stranded`]).
    pub stranded: usize,
    /// The stacks a write left alone: not decided, or rejected.
    pub proposed: usize,
    pub rejected: usize,
}

/// Whether the centre of `job` shows its write and what the write made of its stacks
/// (`views::results`), rather than what its state says: while it is written and once it
/// is done (a done job that wrote nothing says so); queued, failed or cancelled, only
/// once a stack of it was confirmed for a write (Confirmed, Written, Skipped or Failed).
/// A job with none was never sent to a write: it was cancelled or failed before its
/// review (during its scan's visual check, after its analysis was saved), or it is
/// queued for its scan again. Read from the job's counts, so the centre does not wait
/// for its review to know.
pub fn shows_results(job: &JobView) -> bool {
    let counts = &job.counts;
    match job.status {
        JobStatus::Writing | JobStatus::Completed => true,
        JobStatus::Queued | JobStatus::Failed | JobStatus::Cancelled => {
            counts.confirmed + counts.written + counts.skipped + counts.failed > 0
        }
        JobStatus::ReviewPending | JobStatus::Scanning => false,
    }
}

/// Whether `status` is a stack's that was confirmed for a write: waiting for it, or
/// written, skipped or failed by it.
fn confirmed_for_a_write(status: GroupStatus) -> bool {
    match status {
        GroupStatus::Confirmed
        | GroupStatus::Written
        | GroupStatus::Skipped
        | GroupStatus::Failed => true,
        GroupStatus::Proposed | GroupStatus::Rejected => false,
    }
}

impl Review {
    /// The review of `job`, before anything is read: the sliders show `pending`, the
    /// thresholds last sent for it and not stored yet, or else its own.
    fn of(job: &JobView, pending: Option<Thresholds>) -> Review {
        let thresholds = pending.unwrap_or_else(|| Thresholds::of(job));
        Review {
            job: job.id,
            files: Vec::new(),
            groups: Vec::new(),
            unstacked: Vec::new(),
            selection: Selection::default(),
            hamming: thresholds.hamming,
            time_window_seconds: thresholds.time_window_seconds,
            cosine: thresholds.cosine,
            default_cosine: job
                .config
                .ml_model
                .as_deref()
                .and_then(models::model)
                .map(|spec| spec.default_cosine),
            warning_counts: Vec::new(),
            counts: WriteCounts::default(),
            results: ResultCounts::default(),
            listed: Vec::new(),
            checks: None,
            primary_at: None,
            partners: HashMap::new(),
            unreadable: Vec::new(),
            files_read: false,
            groups_read: false,
            failure: None,
        }
    }

    /// Why the review could not be read, before it was loaded: the centre says so, with
    /// `Try again` ([`Controller::read_review_again`]).
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// Whether the files, the stacks and the tray have all been read once.
    pub fn loaded(&self) -> bool {
        self.files_read && self.groups_read
    }

    fn group(&self, id: GroupId) -> Option<&GroupView> {
        self.groups.iter().find(|group| group.id == id)
    }

    /// The primary stack of the selection, which the inspector shows.
    pub fn primary(&self) -> Option<&GroupView> {
        self.groups.get(self.primary_index()?)
    }

    /// Where the primary stack is in [`Review::groups`].
    pub fn primary_index(&self) -> Option<usize> {
        self.primary_at.filter(|&at| at < self.groups.len())
    }

    /// The passive files of the RAW+JPEG pair whose active file is `file`, ascending:
    /// none for a file that is in no pair, or passive itself.
    pub fn partners(&self, file: usize) -> &[usize] {
        self.partners.get(&file).map_or(&[], Vec::as_slice)
    }

    /// The files the scan could not read, by id, ascending: those whose error is set.
    pub fn unreadable(&self) -> &[usize] {
        &self.unreadable
    }

    /// The photos checked in the primary stack, in layer order.
    pub fn checked(&self) -> &[usize] {
        self.checks.as_ref().map_or(&[], |checks| &checks.files)
    }

    /// What the writing and results views count, as of the last read of the stacks.
    pub fn results(&self) -> ResultCounts {
        self.results
    }

    /// Where the stacks confirmed for a write are in [`Review::groups`], in position
    /// order: waiting for it (Confirmed), or written, skipped or failed by it.
    pub fn listed(&self) -> &[usize] {
        &self.listed
    }

    /// Pairs each active file with its passive partners, and finds the files that could
    /// not be read, from the files.
    fn read_files(&mut self) {
        self.partners.clear();
        for file in &self.files {
            if let Some(active) = file.paired_with {
                self.partners.entry(active).or_default().push(file.idx);
            }
        }
        self.unreadable = self
            .files
            .iter()
            .filter(|file| file.error.is_some())
            .map(|file| file.idx)
            .collect();
    }

    /// Follows a change of the selection or of the stacks: finds the primary stack's
    /// place, and drops the checks once it is not their stack any more, or its members
    /// changed.
    fn follow_the_selection(&mut self) {
        let primary = self.selection.primary();
        self.primary_at =
            primary.and_then(|id| self.groups.iter().position(|group| group.id == id));
        let still = self.checks.as_ref().is_some_and(|checks| {
            Some(checks.group) == primary
                && self
                    .primary()
                    .is_some_and(|group| group.members == checks.members)
        });
        if !still {
            self.checks = None;
        }
    }

    /// Checks `file` of the primary stack, or unchecks it.
    fn toggle_check(&mut self, file: usize) {
        let Some(group) = self.primary().filter(|group| group.members.contains(&file)) else {
            return;
        };
        let (id, members) = (group.id, group.members.clone());
        let checks = self.checks.get_or_insert_with(|| Checks {
            group: id,
            members,
            files: Vec::new(),
        });
        if let Some(at) = checks.files.iter().position(|&checked| checked == file) {
            checks.files.remove(at);
        } else {
            checks.files.push(file);
            let order = &checks.members;
            checks
                .files
                .sort_by_key(|checked| order.iter().position(|member| member == checked));
        }
    }

    /// The stack that holds `file`, if one does.
    fn group_of(&self, file: usize) -> Option<&GroupView> {
        self.groups
            .iter()
            .find(|group| group.members.contains(&file))
    }

    /// Takes the stacks and the tray `view` gives, and drops from the selection the
    /// stacks that are gone, then a photo that is not where a selected photo can be: in
    /// the primary stack, or in the tray while no stack is selected.
    fn read(&mut self, view: ReviewView) {
        self.groups = view.groups;
        self.unstacked = view.unstacked;
        self.groups_read = true;
        self.count_warnings();
        self.count_decisions();
        self.count_results();
        let groups = &self.groups;
        self.selection
            .groups
            .retain(|id| groups.iter().any(|group| group.id == *id));
        if let Some(photo) = self.selection.photo {
            let kept = match self.selection.primary().and_then(|id| self.group(id)) {
                Some(primary) => primary.members.contains(&photo),
                None => self.unstacked.contains(&photo),
            };
            if !kept {
                self.selection.photo = None;
            }
        }
        self.follow_the_selection();
    }

    /// Counts each stack's warnings: its members' (a member the files do not hold yet
    /// has none) and its write's.
    fn count_warnings(&mut self) {
        let files = &self.files;
        self.warning_counts = self
            .groups
            .iter()
            .map(|group| {
                let analysis: usize = group
                    .members
                    .iter()
                    .map(|&member| files.get(member).map_or(0, |file| file.warnings.len()))
                    .sum();
                analysis + group.warnings.len()
            })
            .collect();
    }

    /// Counts the Confirmed stacks the next write writes (without a file, of 2 photos or
    /// more: the write skips a stack of one) and the Proposed ones.
    fn count_decisions(&mut self) {
        let count = |wanted: fn(&GroupView) -> bool| {
            self.groups.iter().filter(|group| wanted(group)).count()
        };
        self.counts = WriteCounts {
            to_write: count(|group| {
                group.status == GroupStatus::Confirmed
                    && group.output_path.is_none()
                    && group.members.len() >= 2
            }),
            proposed: count(|group| group.status == GroupStatus::Proposed),
        };
    }

    /// Finds the stacks confirmed for a write, and counts what the write made of the
    /// stacks and what it left alone.
    fn count_results(&mut self) {
        let mut results = ResultCounts::default();
        self.listed.clear();
        for (at, group) in self.groups.iter().enumerate() {
            if confirmed_for_a_write(group.status) {
                self.listed.push(at);
            }
            match group.status {
                GroupStatus::Written => results.written += 1,
                GroupStatus::Skipped => results.skipped += 1,
                GroupStatus::Failed => results.failed += 1,
                GroupStatus::Proposed => results.proposed += 1,
                GroupStatus::Rejected => results.rejected += 1,
                GroupStatus::Confirmed => {}
            }
            if is_stranded(group.status, &group.warnings) {
                results.stranded += 1;
            }
        }
        self.results = results;
    }

    /// Clears the selection unless every stack of it is confirmed for a write, which the
    /// writing and the results list, and its photo, if any, is in one of them.
    fn drop_a_selection_not_listed(&mut self) {
        let listed = |id: GroupId| {
            self.group(id)
                .is_some_and(|group| confirmed_for_a_write(group.status))
        };
        let photo_listed = self.selection.photo.is_none_or(|photo| {
            self.group_of(photo)
                .is_some_and(|group| confirmed_for_a_write(group.status))
        });
        if !(self.selection.groups.iter().all(|&id| listed(id)) && photo_listed) {
            self.selection = Selection::default();
            self.follow_the_selection();
        }
    }

    /// Whether the review shows `file`: in a stack or in the tray.
    fn shows(&self, file: usize) -> bool {
        self.group_of(file).is_some() || self.unstacked.contains(&file)
    }

    /// Moves the primary stack `by` places along the list, stopping at its ends, and
    /// selects it alone, with no photo. With no stack selected, a step down selects the
    /// first.
    fn step_stacks(&mut self, by: i32) {
        let last = self.groups.len().saturating_sub(1);
        let at = self
            .selection
            .primary()
            .and_then(|id| self.groups.iter().position(|group| group.id == id));
        let target = match at {
            Some(at) => step(at, by, last),
            None if by > 0 && !self.groups.is_empty() => 0,
            None => return,
        };
        self.selection = Selection {
            groups: vec![self.groups[target].id],
            photo: None,
        };
    }

    /// Moves the selected photo `by` places within the primary stack, stopping at its
    /// ends; with no photo of that stack selected, selects its top one.
    fn step_photos(&mut self, by: i32) {
        let Some(members) = self
            .selection
            .primary()
            .and_then(|id| self.group(id))
            .map(|group| &group.members)
            .filter(|members| !members.is_empty())
        else {
            return;
        };
        let at = self
            .selection
            .photo
            .and_then(|photo| members.iter().position(|&member| member == photo));
        let target = members[at.map_or(0, |at| step(at, by, members.len() - 1))];
        self.selection.photo = Some(target);
    }
}

/// `at` moved `by` places, kept within `0..=last`.
fn step(at: usize, by: i32, last: usize) -> usize {
    let moved = at as i64 + i64::from(by);
    moved.clamp(0, last as i64) as usize
}

/// Whether `job` has its analysis saved, so a review to show: in review, being written or
/// done; or queued, failed or cancelled once its scan is done (the engine's
/// `scan_done`), or once it saved its analysis without finishing, which only a scan
/// stopped during its visual check does, and which holds files (a scan's files are saved
/// with its analysis, and before it nothing is).
fn has_analysis(job: &JobView) -> bool {
    match job.status {
        JobStatus::ReviewPending | JobStatus::Writing | JobStatus::Completed => true,
        JobStatus::Queued | JobStatus::Failed | JobStatus::Cancelled => {
            job.scan_done || job.counts.files > 0
        }
        JobStatus::Scanning => false,
    }
}

impl Controller {
    /// The review of the selected job, once its analysis is saved. Its lists are empty
    /// until the engine's answers come: see [`Review::loaded`].
    pub fn review(&self) -> Option<&Review> {
        self.review.as_ref()
    }

    /// Opens the selected job's review, closing the one of a job no longer selected or no
    /// longer with an analysis. Called when the selection or the jobs list changes. A job
    /// its events say is scanning has none, whatever an older read of the list says.
    pub(super) fn follow_the_selection(&mut self) {
        let selected = self
            .selected()
            .filter(|job| has_analysis(job) && !self.jobs.is_scanning(job.id))
            .map(|job| job.id);
        if self
            .review
            .as_ref()
            .is_some_and(|review| Some(review.job) != selected)
        {
            self.review = None;
        }
        if self.review.is_none()
            && let Some(job) = selected
        {
            self.open_review(job);
        }
    }

    /// Job `job` now has `status`, which the list does not say yet. A job scanned again
    /// closes its review: the scan makes a new analysis, whose files the open one does
    /// not hold. A selected job whose analysis this status says is saved opens; the
    /// list's next read opens the others.
    pub(super) fn review_status_changed(&mut self, job: JobId, status: JobStatus) {
        if status == JobStatus::Scanning && self.review_of(job).is_some() {
            self.review = None;
        }
        // Out of its review (its write is on its way): the results list only the stacks
        // confirmed for a write, so a selection of others would show what is not there.
        if status != JobStatus::ReviewPending
            && let Some(review) = self.review_of(job)
        {
            review.drop_a_selection_not_listed();
        }
        let saved = matches!(
            status,
            JobStatus::ReviewPending | JobStatus::Writing | JobStatus::Completed
        );
        if saved && self.review.is_none() && self.selected().is_some_and(|view| view.id == job) {
            self.open_review(job);
        }
    }

    /// Shows the review of `job`, a job of the list, and asks for what it shows.
    fn open_review(&mut self, job: JobId) {
        let Some(view) = self.job(job) else {
            return;
        };
        let review = Review::of(view, self.pending_thresholds.get(&job).copied());
        self.review = Some(review);
        self.read_review(job);
    }

    /// Asks for the files and the review of `job`.
    fn read_review(&self, job: JobId) {
        self.send(Request::Files(job));
        self.send(Request::Review(job));
    }

    /// A read of `job`'s files or review failed, as `error` says. The open review of it,
    /// not loaded yet, keeps why, which the centre shows in place of "Loading the
    /// review…"; once it is loaded, it keeps what it shows, and the failure is a notice.
    pub(super) fn review_unreadable(&mut self, job: JobId, error: EngineError) {
        let text = sentence(&format!("The review cannot be read: {error}"));
        match self.review_of(job) {
            Some(review) if !review.loaded() => review.failure = Some(text),
            _ => self.notify(NoticeLevel::Error, text),
        }
    }

    /// Reads the open review again after a read of it failed (`Try again`).
    pub fn read_review_again(&mut self) {
        if let Some(review) = &mut self.review {
            review.failure = None;
            let job = review.job;
            self.read_review(job);
        }
    }

    /// The jobs list changed: the thresholds pending for a job no longer in it go.
    pub(super) fn forget_the_thresholds_of_deleted_jobs(&mut self) {
        let listed: HashSet<JobId> = self.jobs().iter().map(|job| job.id).collect();
        self.pending_thresholds
            .retain(|job, _| listed.contains(job));
    }

    /// A request about `job` failed: thresholds sent for it may be what was refused, and
    /// will never be stored.
    pub(super) fn review_request_failed(&mut self, job: Option<JobId>) {
        if let Some(job) = job {
            self.pending_thresholds.remove(&job);
        }
    }

    /// The open review, when it is `job`'s.
    fn review_of(&mut self, job: JobId) -> Option<&mut Review> {
        self.review.as_mut().filter(|review| review.job == job)
    }

    /// `job`'s groups changed: the open review reads them again.
    pub(super) fn groups_changed(&mut self, job: JobId) {
        if self.review_of(job).is_some() {
            self.send(Request::Review(job));
        }
    }

    /// `job`'s files changed: the open review reads them again.
    pub(super) fn files_changed(&mut self, job: JobId) {
        if self.review_of(job).is_some() {
            self.send(Request::Files(job));
        }
    }

    /// The engine read `job`'s files.
    pub(super) fn files_read(&mut self, job: JobId, files: Vec<FileView>) {
        if let Some(review) = self.review_of(job) {
            review.files = files;
            review.files_read = true;
            review.count_warnings();
            review.read_files();
        }
    }

    /// The engine read `job`'s review: the open one takes its stacks and tray, and the
    /// list the job as of the same moment, so that a warning the regrouping cleared goes
    /// without waiting for the list's next read.
    pub(super) fn review_read(&mut self, job: JobId, view: ReviewView) {
        if self.pending_thresholds.get(&job) == Some(&Thresholds::of(&view.job)) {
            self.pending_thresholds.remove(&job);
        }
        self.jobs.refresh(view.job.clone());
        if let Some(review) = self.review_of(job) {
            review.read(view);
        }
    }

    /// Confirms `group` of the open review.
    pub fn confirm(&mut self, group: GroupId) {
        self.review_command(move |engine, job| engine.confirm(job, group));
    }

    /// Rejects `group` of the open review.
    pub fn reject(&mut self, group: GroupId) {
        self.review_command(move |engine, job| engine.reject(job, group));
    }

    /// Confirms every Proposed stack of the open review.
    pub fn confirm_all(&mut self) {
        self.review_command(|engine, job| engine.confirm_all(job));
    }

    /// Rejects every Proposed stack of the open review.
    pub fn reject_all(&mut self) {
        self.review_command(|engine, job| engine.reject_all(job));
    }

    /// Writes the Confirmed stacks of the open review that have no file yet. The review
    /// stays open while the job is written, and after.
    pub fn write(&mut self) {
        self.review_command(|engine, job| engine.write(job));
    }

    /// The review's `Write <n> stacks`: writes at once when no stack is still proposed;
    /// otherwise asks first ([`Question::Write`](super::Question)), since the review of
    /// a job ends with its write and the proposed stacks could never be confirmed in it.
    pub fn write_from_the_review(&mut self) {
        let Some(review) = &self.review else {
            return;
        };
        let WriteCounts { to_write, proposed } = review.counts;
        if proposed == 0 {
            self.write();
        } else {
            let job = review.job;
            self.ask(super::Question::Write {
                job,
                to_write,
                proposed,
            });
        }
    }

    /// Shows `hamming`, `time_window_seconds` and `cosine` on the sliders at once, and
    /// regroups the open review with them (not while its job is being deleted). Only the
    /// newest thresholds of a job wait in the backend's queue, so a drag sends no more
    /// regroupings than the engine can run. They are remembered for the job until a read
    /// shows them stored, or they are refused.
    pub fn set_thresholds(&mut self, hamming: u32, time_window_seconds: u32, cosine: Option<f32>) {
        let deleting = self
            .review
            .as_ref()
            .is_some_and(|review| self.is_deleting(review.job));
        let Some(review) = self.review.as_mut().filter(|_| !deleting) else {
            return;
        };
        review.hamming = hamming;
        review.time_window_seconds = time_window_seconds;
        review.cosine = cosine;
        let job = review.job;
        self.pending_thresholds.insert(
            job,
            Thresholds {
                hamming,
                time_window_seconds,
                cosine,
            },
        );
        self.send(Request::Thresholds {
            job,
            hamming,
            time_window_seconds,
            cosine,
        });
    }

    /// Moves photo `file` of the open review into a stack, a new stack, or the tray.
    pub fn move_photo(&mut self, file: usize, to: MoveTarget) {
        self.review_command(move |engine, job| engine.move_file(job, file, to));
    }

    /// Merges the selected stacks, two or more, into the primary one, which keeps its id;
    /// the primary one alone is then selected. With fewer selected, nothing happens.
    pub fn merge_selected(&mut self) {
        let Some(review) = &mut self.review else {
            return;
        };
        let selection = &review.selection;
        let Some(primary) = selection.primary().filter(|_| selection.groups.len() >= 2) else {
            return;
        };
        let mut groups = vec![primary];
        groups.extend(selection.groups.iter().filter(|&&id| id != primary));
        review.selection = Selection {
            groups: vec![primary],
            photo: None,
        };
        review.follow_the_selection();
        self.review_command(move |engine, job| engine.merge(job, &groups));
    }

    /// Splits `files` off `group` of the open review into a new stack.
    pub fn split(&mut self, group: GroupId, files: &[usize]) {
        let files = files.to_vec();
        self.review_command(move |engine, job| engine.split(job, group, &files));
    }

    /// Checks photo `file` of the primary stack for a split, or unchecks it. A photo of
    /// another stack is not checked. The checks go when another stack becomes the
    /// primary one, or a read changes the members of theirs.
    pub fn toggle_member(&mut self, file: usize) {
        if let Some(review) = &mut self.review {
            review.toggle_check(file);
        }
    }

    /// The photos checked in the primary stack, in layer order.
    pub fn checked_members(&self) -> &[usize] {
        self.review.as_ref().map_or(&[], Review::checked)
    }

    /// Splits the checked photos off the primary stack into a new stack, and clears the
    /// checks. With none checked, nothing happens; the engine refuses a split that leaves
    /// fewer than 2 photos on a side.
    pub fn split_checked(&mut self) {
        let Some(review) = &mut self.review else {
            return;
        };
        let Some(checks) = review.checks.take() else {
            return;
        };
        if !checks.files.is_empty() {
            self.split(checks.group, &checks.files);
        }
    }

    /// Makes `file`, the passive file of a pair, the pair's active file. The engine reads
    /// it first, so its result comes later, with `FilesChanged`.
    pub fn swap_active(&mut self, file: usize) {
        self.review_command(move |engine, job| engine.swap_active(job, file));
    }

    /// Selects `group` alone, with no photo; with `extend`, adds it to the selection as
    /// the primary stack, or takes it out if it was selected, and clears the photo too. A
    /// stack the review does not show is not selected.
    pub fn select_group(&mut self, group: GroupId, extend: bool) {
        let Some(review) = self
            .review
            .as_mut()
            .filter(|review| review.group(group).is_some())
        else {
            return;
        };
        let selection = &mut review.selection;
        selection.photo = None;
        if !extend {
            selection.groups = vec![group];
        } else if let Some(at) = selection.groups.iter().position(|&id| id == group) {
            selection.groups.remove(at);
        } else {
            selection.groups.push(group);
        }
        review.follow_the_selection();
    }

    /// Selects photo `file`, and its stack alone, if it is in one; a photo of the tray
    /// leaves no stack selected. A photo the review does not show is not selected.
    pub fn select_photo(&mut self, file: usize) {
        let Some(review) = self.review.as_mut().filter(|review| review.shows(file)) else {
            return;
        };
        let groups = review
            .group_of(file)
            .map(|group| vec![group.id])
            .unwrap_or_default();
        review.selection = Selection {
            groups,
            photo: Some(file),
        };
        review.follow_the_selection();
    }

    /// Moves the selection `stacks` stacks along the list, then `photos` photos within
    /// the primary stack (see [`Review`]'s steps): the arrow keys.
    pub fn step_selection(&mut self, stacks: i32, photos: i32) {
        let Some(review) = &mut self.review else {
            return;
        };
        if stacks != 0 {
            review.step_stacks(stacks);
        }
        if photos != 0 {
            review.step_photos(photos);
        }
        review.follow_the_selection();
    }

    pub fn clear_selection(&mut self) {
        if let Some(review) = &mut self.review {
            review.selection = Selection::default();
            review.follow_the_selection();
        }
    }

    /// Confirms each selected stack, in the order they were picked.
    pub fn confirm_selected(&mut self) {
        for group in self.selected_groups() {
            self.confirm(group);
        }
    }

    /// Rejects each selected stack, in the order they were picked.
    pub fn reject_selected(&mut self) {
        for group in self.selected_groups() {
            self.reject(group);
        }
    }

    /// What the Write button counts in the open review, as of its last read; nothing
    /// without one.
    pub fn write_counts(&self) -> WriteCounts {
        self.review
            .as_ref()
            .map_or_else(WriteCounts::default, |review| review.counts)
    }

    fn selected_groups(&self) -> Vec<GroupId> {
        self.review
            .as_ref()
            .map(|review| review.selection.groups.clone())
            .unwrap_or_default()
    }

    /// Sends `command` about the open review's job; nothing without one. A job no longer
    /// in review refuses it, and the refusal is a notice.
    fn review_command(
        &self,
        command: impl FnOnce(&Engine, JobId) -> Result<(), EngineError> + Send + 'static,
    ) {
        if let Some(review) = &self.review {
            let job = review.job;
            self.run(job, move |engine| command(engine, job));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use photostacker_core::JobConfig;
    use photostacker_core::engine::JobCounts;

    use super::*;

    /// A job with `status` whose analysis holds 10 photos, and whose stacks are `counts`.
    fn job(status: JobStatus, counts: JobCounts) -> JobView {
        JobView {
            id: 1,
            created_at: String::new(),
            status,
            scan_done: true,
            config: JobConfig::new(PathBuf::from("photos")),
            error: None,
            warning: None,
            counts: JobCounts {
                files: 10,
                ..counts
            },
        }
    }

    #[test]
    fn a_job_shows_write_results_once_a_stack_was_confirmed_for_a_write() {
        use JobStatus::{Cancelled, Completed, Failed, Queued, Writing};
        // Stopped in its scan's visual check, after its analysis was saved: its stacks
        // are all proposed, and none was ever confirmed for a write.
        let never_confirmed = JobCounts {
            proposed: 2,
            ..JobCounts::default()
        };
        assert!(!shows_results(&job(Cancelled, never_confirmed)));
        assert!(!shows_results(&job(Failed, never_confirmed)));
        // Retried, it is queued for its scan, not for a write.
        assert!(!shows_results(&job(Queued, never_confirmed)));
        // Done without a stack confirmed: it says that no stack was written.
        assert!(shows_results(&job(Completed, never_confirmed)));
        assert!(shows_results(&job(Writing, never_confirmed)));

        // Cancelled after its write wrote a stack, with one left to write.
        let one_written = JobCounts {
            written: 1,
            confirmed: 1,
            ..JobCounts::default()
        };
        assert!(shows_results(&job(Cancelled, one_written)));
        // Each status of a stack confirmed for a write counts, alone.
        for counts in [
            JobCounts {
                confirmed: 1,
                ..JobCounts::default()
            },
            JobCounts {
                skipped: 1,
                ..JobCounts::default()
            },
            JobCounts {
                failed: 1,
                ..JobCounts::default()
            },
        ] {
            for status in [Queued, Failed, Cancelled] {
                assert!(shows_results(&job(status, counts)), "{status:?} {counts:?}");
            }
        }
        // A job in review shows its review; one scanning, its scan.
        assert!(!shows_results(&job(JobStatus::ReviewPending, one_written)));
        assert!(!shows_results(&job(JobStatus::Scanning, one_written)));
    }

    /// A controller on a data folder of its own whose one job, on `/photos/trip`, is in
    /// review, read once.
    fn controller_with_a_job_in_review(data: &std::path::Path) -> (Controller, JobId) {
        use std::sync::Arc;

        use photostacker_core::{AppDirs, Store};

        use crate::controller::{RuntimeCheck, StartOptions};
        use crate::platform::Native;

        let dirs = AppDirs::under(data.to_path_buf());
        std::fs::create_dir_all(&dirs.data).unwrap();
        let job = {
            let store = Store::open(&dirs.db()).unwrap();
            let job = store
                .insert_job(&JobConfig::new(PathBuf::from("/photos/trip")))
                .unwrap();
            store.set_scan_done(job, true).unwrap();
            store.set_job_status(job, JobStatus::ReviewPending).unwrap();
            job
        };
        let mut controller = Controller::start(StartOptions {
            dirs,
            wake: Arc::new(|| {}),
            platform: Arc::new(Native),
            embedder: None,
            installer: None,
            runtime: RuntimeCheck {
                path: PathBuf::from("/opt/onnxruntime/lib/libonnxruntime.so"),
                found: true,
            },
        })
        .unwrap();
        poll_until(&mut controller, |controller| controller.jobs_loaded());
        (controller, job)
    }

    /// Polls `controller` until `done` holds.
    fn poll_until(controller: &mut Controller, done: impl Fn(&Controller) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !done(controller) {
            assert!(std::time::Instant::now() < deadline, "timed out");
            std::thread::sleep(std::time::Duration::from_millis(5));
            controller.poll();
        }
    }

    #[test]
    fn a_review_that_cannot_be_read_says_so_and_can_be_read_again() {
        let data = tempfile::tempdir().unwrap();
        let (mut controller, job) = controller_with_a_job_in_review(data.path());
        // The backend is held, so the review's reads wait.
        let (release, held) = std::sync::mpsc::channel::<()>();
        controller.send(Request::Run {
            job: None,
            command: Box::new(move |_| {
                let _ = held.recv();
                Ok(())
            }),
        });
        controller.select_job(job);
        assert!(controller.review().is_some_and(|review| !review.loaded()));

        // A read of it fails: the review says so, and no notice repeats it.
        let error = EngineError::Invalid("the disk is unreadable".into());
        controller.review_unreadable(job, error);
        let review = controller.review().unwrap();
        assert_eq!(
            review.failure(),
            Some("The review cannot be read: the disk is unreadable.")
        );
        assert!(!review.loaded());
        assert!(controller.take_notices().is_empty());

        // Try again: the failure goes, and the reads come.
        controller.read_review_again();
        assert_eq!(controller.review().unwrap().failure(), None);
        release.send(()).unwrap();
        poll_until(&mut controller, |controller| {
            controller.review().is_some_and(Review::loaded)
        });

        // Once it is loaded, a read that fails is a notice; the review keeps what it shows.
        let error = EngineError::Invalid("the disk is unreadable".into());
        controller.review_unreadable(job, error);
        assert_eq!(controller.review().unwrap().failure(), None);
        assert_eq!(controller.take_notices().len(), 1);
    }

    #[test]
    fn a_step_stops_at_the_ends() {
        assert_eq!(step(0, -1, 3), 0);
        assert_eq!(step(1, 1, 3), 2);
        assert_eq!(step(2, 5, 3), 3);
        assert_eq!(step(2, i32::MIN, 3), 0);
        assert_eq!(step(0, i32::MAX, 0), 0);
    }
}
