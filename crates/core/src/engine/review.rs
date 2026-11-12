//! The review of a job in Review Pending (spec §5.4.1, §5.7): the decisions,
//! the live regrouping, the manual edits and the swap of a pair's active
//! file.
//!
//! An edit reads the job's files, groups and pins into a [`Draft`], changes
//! them, regroups the rest around the manual groups and the pinned photos,
//! and stores it all in one transaction ([`Store::apply_review`]). It holds
//! the writer's lock from the read to the write, so two edits never
//! interleave. Stage 3 checks the regrouping with the embeddings stored;
//! when some of its groups wait for embeddings, the edit queues the job's
//! embed task (`stage3.rs`).

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use super::scan::StoreCache;
use super::stage3::Embeddings;
use super::worker::{Settle, Task, TaskError};
use super::{Engine, EngineError, EngineEvent, GroupId, JobId, Shared, lock};
use crate::analyze::{ExamCache, FileRecord, examine_active, read_passive, relative_to};
use crate::discover::DiscoveredFile;
use crate::exif::ExifData;
use crate::fsio::display_path;
use crate::group::{
    ExistingGroup, FileFacts, ManualGroup, RegroupInput, Regrouped, Thresholds,
    max_pairwise_distance, order_layers, regroup,
};
use crate::hash::HashPair;
use crate::model::{GroupOrigin, GroupStatus, JobConfig, JobStatus};
use crate::models;
use crate::store::{ManualRow, ReviewChange, Store, Swap};

/// Where [`Engine::move_file`] moves a photo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveTarget {
    /// Into this group of the job.
    Group(GroupId),
    /// Into a new manual group of its own.
    NewGroup,
    /// Into the tray, pinned there.
    Tray,
}

impl Engine {
    /// Confirms `group` of `job`, whether it is Proposed or Rejected.
    pub fn confirm(&self, job: JobId, group: GroupId) -> Result<(), EngineError> {
        self.decide(job, group, GroupStatus::Confirmed)
    }

    /// Rejects `group` of `job`, whether it is Proposed or Confirmed.
    pub fn reject(&self, job: JobId, group: GroupId) -> Result<(), EngineError> {
        self.decide(job, group, GroupStatus::Rejected)
    }

    /// Confirms every Proposed group of `job`.
    pub fn confirm_all(&self, job: JobId) -> Result<(), EngineError> {
        self.decide_all(job, GroupStatus::Confirmed)
    }

    /// Rejects every Proposed group of `job`.
    pub fn reject_all(&self, job: JobId) -> Result<(), EngineError> {
        self.decide_all(job, GroupStatus::Rejected)
    }

    /// Regroups `job` with the Stage-2 threshold `hamming`, the Stage-1 time
    /// window `time_window_seconds` and the Stage-3 threshold `cosine`
    /// (`None`: the model's default), which its config, with the thresholds
    /// validated, then keeps. It works from the store alone, so the folders
    /// need not be there. The manual groups and the pinned photos stay as they
    /// are; an automatic group whose members are unchanged keeps its id and
    /// its decision (spec §5.4.1).
    ///
    /// Stage 3 checks the ambiguous groups with the embeddings stored. A
    /// group with a member that has none yet is `Pending`, and an embed task
    /// is queued that computes them and regroups (spec §5.4), as after any
    /// review edit.
    pub fn set_thresholds(
        &self,
        job: JobId,
        hamming: u32,
        time_window_seconds: u32,
        cosine: Option<f32>,
    ) -> Result<(), EngineError> {
        self.shared.edit(job, |draft| {
            let mut config = draft.config.clone();
            config.hamming_threshold = hamming;
            config.time_window_seconds = time_window_seconds;
            config.cosine_threshold = cosine;
            config.validate_thresholds()?;
            draft.config = config;
            draft.config_changed = true;
            Ok(true)
        })
    }

    /// Moves `file`, a photo of `job` that hashed, into another group, a new
    /// group of its own, or the tray.
    ///
    /// - The group it leaves and the group it joins become manual and
    ///   Proposed. A manual group left with fewer than 2 photos dissolves:
    ///   they go to the tray, pinned.
    /// - The tray pins it, so no regrouping takes it out again; leaving the
    ///   tray unpins it.
    /// - A group never holds photos of two folders.
    ///
    /// Moving a photo to where it is changes nothing.
    pub fn move_file(&self, job: JobId, file: usize, to: MoveTarget) -> Result<(), EngineError> {
        self.shared.edit(job, |draft| {
            draft.check_groupable(file)?;
            let source = draft.group_of(file);
            match to {
                MoveTarget::Group(target) => {
                    let index = draft.group_index(target)?;
                    if source == Some(index) {
                        return Ok(false);
                    }
                    let joined = draft.groups[index].members.iter().copied();
                    draft.check_one_folder(joined.chain([file]))?;
                    draft.take(file);
                    let index = draft.group_index(target)?;
                    draft.groups[index].members.push(file);
                    draft.edited(index);
                }
                MoveTarget::NewGroup => {
                    let bucket = match source {
                        Some(index) if draft.groups[index].members == [file] => return Ok(false),
                        Some(index) => draft.groups[index].bucket.clone(),
                        None => String::new(),
                    };
                    draft.take(file);
                    draft.add_group(vec![file], bucket);
                }
                MoveTarget::Tray => {
                    if source.is_none() && draft.pinned.contains(&file) {
                        return Ok(false);
                    }
                    draft.take(file);
                    draft.pinned.insert(file);
                }
            }
            Ok(true)
        })
    }

    /// Merges `groups` of `job`, two or more, into one manual, Proposed
    /// group, which keeps the id of the first; the others are deleted.
    pub fn merge(&self, job: JobId, groups: &[GroupId]) -> Result<(), EngineError> {
        self.shared.edit(job, |draft| {
            let mut ids: Vec<GroupId> = Vec::new();
            for &id in groups {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            let [first, others @ ..] = &ids[..] else {
                return Err(EngineError::Invalid(
                    "a merge needs 2 stacks or more".into(),
                ));
            };
            if others.is_empty() {
                return Err(EngineError::Invalid(
                    "a merge needs 2 stacks or more".into(),
                ));
            }
            let mut members = Vec::new();
            for &id in &ids {
                members.extend_from_slice(&draft.groups[draft.group_index(id)?].members);
            }
            draft.check_one_folder(members.iter().copied())?;
            for &id in others {
                let index = draft.group_index(id)?;
                draft.groups.remove(index);
                draft.removed.push(id);
            }
            let index = draft.group_index(*first)?;
            draft.groups[index].members = members;
            draft.edited(index);
            Ok(true)
        })
    }

    /// Splits `files` off `group` of `job` into a new manual group; the rest
    /// stays in `group`, which becomes manual. Both are Proposed, and each
    /// must keep 2 photos or more.
    pub fn split(&self, job: JobId, group: GroupId, files: &[usize]) -> Result<(), EngineError> {
        self.shared.edit(job, |draft| {
            let index = draft.group_index(group)?;
            let part: BTreeSet<usize> = files.iter().copied().collect();
            let members = &draft.groups[index].members;
            if let Some(stranger) = part.iter().find(|file| !members.contains(file)) {
                let what = draft.photo_name(*stranger).map_or_else(
                    || "a photo to split off".into(),
                    |name| format!("the photo {name}"),
                );
                return Err(EngineError::Invalid(format!(
                    "{what} is not in stack {}",
                    index + 1
                )));
            }
            let rest: Vec<usize> = members
                .iter()
                .copied()
                .filter(|file| !part.contains(file))
                .collect();
            if part.len() < 2 || rest.len() < 2 {
                return Err(EngineError::Invalid(
                    "a split must leave 2 photos or more on each side".into(),
                ));
            }
            let bucket = draft.groups[index].bucket.clone();
            draft.groups[index].members = rest;
            draft.edited(index);
            draft.add_group(part.into_iter().collect(), bucket);
            Ok(true)
        })
    }

    /// Queues the swap of `file`, the passive file of a pair of `job`, into
    /// the pair's active file. The worker examines it as an active file
    /// (through the exam cache); if it hashes, it takes its partner's place,
    /// in its group or in the tray, and the job is regrouped:
    /// [`EngineEvent::FilesChanged`], then [`EngineEvent::GroupsChanged`],
    /// follow. Otherwise an
    /// [`EngineEvent::Error`] says why, and nothing changes. Refused once the
    /// worker has stopped.
    pub fn swap_active(&self, job: JobId, file: usize) -> Result<(), EngineError> {
        let mut queue = lock(&self.shared.queue);
        queue.check_running()?;
        {
            let writer = lock(&self.shared.writer);
            let stored = writer.job(job)?.ok_or(EngineError::UnknownJob(job))?;
            in_review(job, stored.status)?;
            let files = writer.load_analysis(job)?.files;
            let record = files
                .get(file)
                .ok_or(EngineError::UnknownFile { job, idx: file })?;
            if record.paired_with.is_none() {
                return Err(EngineError::Invalid(format!(
                    "the photo {} is not the one a RAW+JPEG pair leaves out",
                    photo_name(&record.path, &stored.config.input_dir)
                )));
            }
        }
        queue.push(Task::Swap { job, file });
        self.shared.work.notify_one();
        Ok(())
    }

    /// Gives `group` of `job` the decision `status`.
    fn decide(&self, job: JobId, group: GroupId, status: GroupStatus) -> Result<(), EngineError> {
        {
            let writer = lock(&self.shared.writer);
            let stored = writer.job(job)?.ok_or(EngineError::UnknownJob(job))?;
            in_review(job, stored.status)?;
            if !writer.group_in_job(job, group)? {
                return Err(EngineError::UnknownGroup(group));
            }
            writer.set_group_status(group, status)?;
        }
        self.shared.events.send(EngineEvent::GroupsChanged { job });
        Ok(())
    }

    /// Gives every Proposed group of `job` the decision `status`.
    fn decide_all(&self, job: JobId, status: GroupStatus) -> Result<(), EngineError> {
        let decided = {
            let writer = lock(&self.shared.writer);
            let stored = writer.job(job)?.ok_or(EngineError::UnknownJob(job))?;
            in_review(job, stored.status)?;
            writer.decide_proposed(job, status)?
        };
        if decided > 0 {
            self.shared.events.send(EngineEvent::GroupsChanged { job });
        }
        Ok(())
    }
}

impl Shared {
    /// Runs the edit `change` on the draft of `job`, which must be in
    /// review, and stores the draft when `change` says it changed it. A
    /// regrouping that leaves groups waiting for embeddings queues the
    /// job's embed task. Called with no engine lock held.
    pub(super) fn edit(
        &self,
        job: JobId,
        change: impl FnOnce(&mut Draft) -> Result<bool, EngineError>,
    ) -> Result<(), EngineError> {
        let _flush = self.events.flush_on_drop();
        let mut queue = lock(&self.queue);
        let needs_embeddings = {
            let mut writer = lock(&self.writer);
            let mut draft = Draft::load(&writer, job)?;
            in_review(job, draft.status)?;
            if !change(&mut draft)? {
                return Ok(());
            }
            let embeddings = self.stage3.embeddings(&writer, job, &draft.config)?;
            draft.store(&mut writer, None, embeddings.as_ref())?
        };
        self.events.post(EngineEvent::GroupsChanged { job });
        if !needs_embeddings.is_empty() {
            queue.push_embed(job);
            self.work.notify_one();
        }
        Ok(())
    }
}

/// How a refusal names the photo at `path` of the job on `input`: its path inside the
/// job's folder, as the user knows it, never its id. A refusal never starts with it:
/// the app shows a refusal as a sentence, with its first letter capitalised, which would
/// misquote the file's name.
fn photo_name(path: &Path, input: &Path) -> String {
    display_path(&relative_to(input, path))
}

/// `Ok` when a job in `status` is in review.
fn in_review(job: JobId, status: JobStatus) -> Result<(), EngineError> {
    if status == JobStatus::ReviewPending {
        Ok(())
    } else {
        Err(EngineError::WrongState { job, status })
    }
}

/// The swap task (see [`Engine::swap_active`]). The job is not in Review
/// Pending any more when the user asked for its write meanwhile, which
/// queued the write after the swap: the swap still applies. It leaves the
/// job's status as it is.
pub(super) fn swap(
    shared: &Shared,
    job: JobId,
    file: usize,
    stop: &AtomicBool,
) -> Result<Settle, TaskError> {
    let (files, input) = {
        let writer = lock(&shared.writer);
        let Some(stored) = writer.job(job)? else {
            return Ok(Settle::Unchanged);
        };
        (writer.load_analysis(job)?.files, stored.config.input_dir)
    };
    // Nothing to do when the file became active meanwhile (a second swap).
    let Some(demoted) = files.get(file).and_then(|record| record.paired_with) else {
        return Ok(Settle::Unchanged);
    };
    let cache = StoreCache::new(shared, job);
    let cache: &dyn ExamCache = &cache;
    // The demoted file lends its own EXIF, as a passive file does.
    let (_, demoted_exif) = read_passive(&discovered(&files[demoted]), Some(cache));
    // The promoted file's partners: the demoted file and its other partners,
    // in ascending id.
    let partner_exif: Vec<&ExifData> = (0..files.len())
        .filter(|&id| id == demoted || (id != file && files[id].paired_with == Some(demoted)))
        .filter_map(|id| {
            if id == demoted {
                demoted_exif.as_ref()
            } else {
                files[id].exif.as_ref()
            }
        })
        .collect();
    let exam = examine_active(&discovered(&files[file]), &partner_exif, Some(cache));
    if stop.load(Ordering::SeqCst) {
        return Err(TaskError::Stopped);
    }
    let (hashes, normalised) = match exam.hashes {
        Ok(pair) => pair,
        Err(error) => {
            shared.events.send(EngineEvent::Error {
                job: Some(job),
                message: format!(
                    "the photo {} cannot be the active file of its pair: {error}",
                    photo_name(&files[file].path, &input)
                ),
            });
            return Ok(Settle::Unchanged);
        }
    };
    let swap = Swap {
        promoted: file,
        exif: exam.exif.as_ref(),
        hashes,
        normalised,
        warnings: &exam.warnings,
        demoted_exif: demoted_exif.as_ref(),
    };
    let needs_embeddings = {
        let mut writer = lock(&shared.writer);
        let mut draft = Draft::load(&writer, job)?;
        draft.swap(&swap, demoted);
        let embeddings = shared.stage3.embeddings(&writer, job, &draft.config)?;
        draft.store(&mut writer, Some(&swap), embeddings.as_ref())?
    };
    // Both are in the channel before this command's flush calls their wakes.
    // Another thread's flush can still wake the owner between the two, so an
    // owner handles each event when it arrives.
    shared.events.post(EngineEvent::FilesChanged { job });
    shared.events.send(EngineEvent::GroupsChanged { job });
    // The promoted file has no embedding yet. No notify: this runs on the
    // worker itself, which takes its next task before it waits again.
    if !needs_embeddings.is_empty() {
        lock(&shared.queue).push_embed(job);
    }
    Ok(Settle::Unchanged)
}

/// The file of `record` as discovery gave it; the exam cache keys it by its
/// path and fingerprint.
pub(super) fn discovered(record: &FileRecord) -> DiscoveredFile {
    let since_epoch = Duration::from_millis(record.mtime_ms.unsigned_abs());
    let mtime = if record.mtime_ms >= 0 {
        SystemTime::UNIX_EPOCH.checked_add(since_epoch)
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(since_epoch)
    };
    DiscoveredFile {
        path: record.path.clone(),
        kind: record.kind,
        size: record.size,
        mtime: mtime.unwrap_or(SystemTime::UNIX_EPOCH),
        fingerprint: record.fingerprint.clone(),
    }
}

/// Regroups `files` with `config`'s thresholds (spec §5.4.1) around the
/// `manual` groups and the `pinned` photos, matching the `existing`
/// automatic groups. Stage 3 checks the ambiguous groups with `embeddings`,
/// the job's for its model; with `None` (Stage 3 unavailable) an ambiguous
/// group is `Skipped`.
pub(super) fn regroup_job(
    config: &JobConfig,
    files: &[FileRecord],
    manual: &[ManualGroup],
    pinned: &[usize],
    existing: &[ExistingGroup],
    embeddings: Option<&Embeddings>,
) -> Regrouped {
    let facts = facts_of(config, files);
    let hashes = hashes_of(files);
    let embedding = |id: usize| {
        embeddings
            .and_then(|embeddings| embeddings.get(&id))
            .map(Vec::as_slice)
    };
    regroup(&RegroupInput {
        facts: &facts,
        hashes: &hashes,
        thresholds: Thresholds {
            hamming: config.hamming_threshold,
            floor: config.stage3_ambiguity_floor,
            cosine: cosine_threshold(config),
            time_window_seconds: config.time_window_seconds,
        },
        ml: match embeddings {
            Some(_) => Some(&embedding),
            None => None,
        },
        manual,
        pinned,
        existing,
    })
}

/// The Stage-3 cosine threshold of `config`: its own, else its model's
/// default. Without a model Stage 3 is off, and the value is never read.
fn cosine_threshold(config: &JobConfig) -> f32 {
    let default = config
        .ml_model
        .as_deref()
        .and_then(models::model)
        .map_or(1.0, |spec| spec.default_cosine);
    config.cosine_threshold.unwrap_or(default)
}

/// What grouping knows of `files`. Their paths are relative to the input
/// folder, as the analysis groups them, so the bucket keys agree.
fn facts_of(config: &JobConfig, files: &[FileRecord]) -> Vec<FileFacts> {
    files
        .iter()
        .map(|file| FileFacts {
            path: relative_to(&config.input_dir, &file.path),
            exif: file.exif.clone(),
            mtime_ms: file.mtime_ms,
        })
        .collect()
}

fn hashes_of(files: &[FileRecord]) -> Vec<Option<HashPair>> {
    files.iter().map(|file| file.normalised).collect()
}

/// A job's review state, read from the store, as one edit changes it.
pub(super) struct Draft {
    job: JobId,
    status: JobStatus,
    config: JobConfig,
    config_changed: bool,
    files: Vec<FileRecord>,
    facts: Vec<FileFacts>,
    hashes: Vec<Option<HashPair>>,
    /// In position order; the groups the edit makes come last.
    groups: Vec<DraftGroup>,
    /// The stored groups the edit deletes.
    removed: Vec<GroupId>,
    pinned: BTreeSet<usize>,
    stored_pins: BTreeSet<usize>,
}

struct DraftGroup {
    /// `None` for a group the edit makes.
    id: Option<GroupId>,
    origin: GroupOrigin,
    status: GroupStatus,
    /// Its `look again` flag as stored: a regrouping that reuses the group
    /// keeps it. A manual group never has it.
    review_reset: bool,
    /// In layer order.
    members: Vec<usize>,
    bucket: String,
    max_distance: u32,
}

impl Draft {
    pub(super) fn load(store: &Store, job: JobId) -> Result<Draft, EngineError> {
        let stored = store.job(job)?.ok_or(EngineError::UnknownJob(job))?;
        let files = store.files(job)?;
        let groups = store
            .groups(job)?
            .into_iter()
            .map(|group| DraftGroup {
                id: Some(group.id),
                origin: group.origin,
                status: group.status,
                review_reset: group.review_reset,
                members: group.members,
                bucket: group.bucket,
                max_distance: group.max_distance,
            })
            .collect();
        let pinned: BTreeSet<usize> = store.pinned(job)?.into_iter().collect();
        Ok(Draft {
            job,
            status: stored.status,
            facts: facts_of(&stored.config, &files),
            hashes: hashes_of(&files),
            config: stored.config,
            config_changed: false,
            files,
            groups,
            removed: Vec::new(),
            stored_pins: pinned.clone(),
            pinned,
        })
    }

    /// The job's status when the draft was read.
    pub(super) fn status(&self) -> JobStatus {
        self.status
    }

    /// `Ok` when `file` is a photo of the job that hashed, the only files
    /// that are grouped.
    fn check_groupable(&self, file: usize) -> Result<(), EngineError> {
        let record = self.files.get(file).ok_or(EngineError::UnknownFile {
            job: self.job,
            idx: file,
        })?;
        if record.normalised.is_none() {
            return Err(EngineError::Invalid(format!(
                "the photo {} cannot be stacked: it could not be read, or its RAW+JPEG pair leaves it out",
                photo_name(&record.path, &self.config.input_dir)
            )));
        }
        Ok(())
    }

    /// `Ok` when every one of `members` is in one folder: a group never
    /// spans two (spec §5.1).
    fn check_one_folder(
        &self,
        members: impl IntoIterator<Item = usize>,
    ) -> Result<(), EngineError> {
        let mut folders = members.into_iter().map(|id| self.files[id].path.parent());
        let first = folders.next();
        if folders.all(|folder| Some(folder) == first) {
            Ok(())
        } else {
            Err(EngineError::Invalid(
                "a stack cannot hold photos of two folders".into(),
            ))
        }
    }

    /// How a refusal names photo `file`: its path inside the job's folder;
    /// `None` for a file the job does not have.
    fn photo_name(&self, file: usize) -> Option<String> {
        let record = self.files.get(file)?;
        Some(photo_name(&record.path, &self.config.input_dir))
    }

    fn group_index(&self, id: GroupId) -> Result<usize, EngineError> {
        self.groups
            .iter()
            .position(|group| group.id == Some(id))
            .ok_or(EngineError::UnknownGroup(id))
    }

    fn group_of(&self, file: usize) -> Option<usize> {
        self.groups
            .iter()
            .position(|group| group.members.contains(&file))
    }

    /// Puts the members of group `index` in layer order and measures them.
    fn refresh(&mut self, index: usize) {
        let group = &mut self.groups[index];
        order_layers(&mut group.members, &self.facts);
        group.max_distance = max_pairwise_distance(&group.members, &self.hashes);
    }

    /// Marks group `index` as edited by the user: manual and Proposed.
    fn edited(&mut self, index: usize) {
        self.groups[index].origin = GroupOrigin::Manual;
        self.groups[index].status = GroupStatus::Proposed;
        self.groups[index].review_reset = false;
        self.refresh(index);
    }

    /// Makes a manual, Proposed group of `members`.
    fn add_group(&mut self, members: Vec<usize>, bucket: String) {
        self.groups.push(DraftGroup {
            id: None,
            origin: GroupOrigin::Manual,
            status: GroupStatus::Proposed,
            review_reset: false,
            members,
            bucket,
            max_distance: 0,
        });
        self.refresh(self.groups.len() - 1);
    }

    /// Takes `file` out of where it is: out of its group, which the user so
    /// edits, and which dissolves when it is left with fewer than 2 photos;
    /// or out of the tray, unpinned.
    fn take(&mut self, file: usize) {
        let Some(index) = self.group_of(file) else {
            self.pinned.remove(&file);
            return;
        };
        self.groups[index].members.retain(|&member| member != file);
        if self.groups[index].members.len() < 2 {
            // Its photos go to the tray, where no regrouping takes them.
            let group = self.groups.remove(index);
            self.pinned.extend(group.members);
            self.removed.extend(group.id);
        } else {
            self.edited(index);
        }
    }

    /// Makes `swap.promoted` the active file of its pair instead of
    /// `demoted`, as [`Store::swap_active`] will: its photo stays where the
    /// demoted file was, in its group (with its decision) or pinned.
    fn swap(&mut self, swap: &Swap, demoted: usize) {
        let promoted = swap.promoted;
        for record in &mut self.files {
            if record.paired_with == Some(demoted) {
                record.paired_with = Some(promoted);
            }
        }
        let record = &mut self.files[promoted];
        record.paired_with = None;
        record.exif = swap.exif.cloned();
        record.hashes = Some(swap.hashes);
        record.normalised = Some(swap.normalised);
        record.error = None;
        let record = &mut self.files[demoted];
        record.paired_with = Some(promoted);
        record.exif = swap.demoted_exif.cloned();
        record.hashes = None;
        record.normalised = None;
        record.error = None;
        self.facts = facts_of(&self.config, &self.files);
        self.hashes = hashes_of(&self.files);
        for index in 0..self.groups.len() {
            let members = &mut self.groups[index].members;
            if let Some(slot) = members.iter_mut().find(|member| **member == demoted) {
                *slot = promoted;
                self.refresh(index);
            }
        }
        if self.pinned.remove(&demoted) {
            self.pinned.insert(promoted);
        }
    }

    /// Regroups the draft around its manual groups and pinned photos, with
    /// `embeddings` for Stage 3 (see [`regroup_job`]). Its `removed` holds
    /// the stored groups the edit deleted too.
    pub(super) fn regroup(&self, embeddings: Option<&Embeddings>) -> Regrouped {
        let existing: Vec<ExistingGroup> = self
            .groups
            .iter()
            .filter(|group| group.origin == GroupOrigin::Auto)
            .filter_map(|group| {
                Some(ExistingGroup {
                    id: group.id?,
                    members: group.members.clone(),
                    status: group.status,
                    review_reset: group.review_reset,
                })
            })
            .collect();
        // Regrouping reads only the members of a manual group. One the edit
        // made has no row yet: 0, which no row has, stands for its id.
        let manual: Vec<ManualGroup> = self
            .groups
            .iter()
            .filter(|group| group.origin == GroupOrigin::Manual)
            .map(|group| ManualGroup {
                id: group.id.unwrap_or(0),
                members: group.members.clone(),
            })
            .collect();
        let pinned: Vec<usize> = self.pinned.iter().copied().collect();
        let mut regrouped = regroup_job(
            &self.config,
            &self.files,
            &manual,
            &pinned,
            &existing,
            embeddings,
        );
        regrouped.removed.extend(&self.removed);
        regrouped.removed.sort_unstable();
        regrouped
    }

    /// Regroups the draft (see [`Draft::regroup`]) and stores it, with
    /// `swap` when it swapped a pair. Returns the files whose embeddings
    /// the stored groups wait for.
    pub(super) fn store(
        self,
        store: &mut Store,
        swap: Option<&Swap>,
        embeddings: Option<&Embeddings>,
    ) -> Result<Vec<usize>, EngineError> {
        let regrouped = self.regroup(embeddings);
        let manual: Vec<ManualRow> = self
            .groups
            .into_iter()
            .filter(|group| group.origin == GroupOrigin::Manual)
            .map(|group| ManualRow {
                id: group.id,
                members: group.members,
                status: group.status,
                bucket: group.bucket,
                max_distance: group.max_distance,
            })
            .collect();
        let pins: Vec<(usize, bool)> = self
            .stored_pins
            .difference(&self.pinned)
            .map(|&file| (file, false))
            .chain(
                self.pinned
                    .difference(&self.stored_pins)
                    .map(|&file| (file, true)),
            )
            .collect();
        store.apply_review(
            self.job,
            &ReviewChange {
                config: self.config_changed.then_some(&self.config),
                swap,
                pins: &pins,
                regrouped: &regrouped,
                manual: &manual,
            },
        )?;
        Ok(regrouped.needs_embeddings)
    }
}
