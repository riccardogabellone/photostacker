//! The grouping pipeline as one call (spec §5): discovery, RAW+JPEG pairing,
//! EXIF, thumbnails and hashes, then Stage 1 bucketing and Stage 2 linking.
//! With an [`ExamCache`] ([`analyze_with`]) a scan that stopped resumes
//! where it left off, file by file.
//!
//! # Concurrency and memory
//!
//! The per-file work runs on one rayon pool built for the call, with one
//! thread per core up to [`MAX_WORKER_THREADS`]. What bounds the memory is a
//! [`MemoryBudget`] of [`DECODE_MEMORY_BUDGET`] bytes: each thumbnail decode
//! runs under a permit of its estimated peak (`decode::thumbnail_peak`), so
//! large stills run with fewer others, or alone, instead of all at once. Each
//! EXIF read, of an active or a passive file, runs under a permit of the
//! bytes it reads into memory (`decode::exif_read_peak`: the whole file of a
//! RAW or a TIFF, since no file is memory-mapped), given back before the
//! thumbnail's permit is asked for. So a worker holds at most one permit.
//!
//! Each worker takes one file after the other from a shared counter (a
//! `broadcast` loop, not a `par_iter`), and only that loop waits for a
//! permit, holding none. rawler's nested `par_iter` runs on this same pool,
//! and a worker that waits inside it may run other workers' pieces of that
//! nested work meanwhile, but never another file's job: those are not on
//! the pool's queues to be stolen. So no worker waits for a permit while a
//! permit's work is below it on its stack, which could deadlock.
//!
//! rayon pushes the broadcast jobs one worker after the other, so a worker
//! whose job came late could steal a piece of a file already under way and,
//! waiting inside it, find its own broadcast job there and start its whole
//! loop on top of that file. So every loop first waits at a barrier of all
//! the pool's threads: no file work exists until each worker has taken its
//! own broadcast job from an empty stack, and past the barrier no worker
//! has a broadcast job left to find.
//!
//! Apart from the budget, no lock is held anywhere in the per-file work,
//! progress reports included.
//!
//! [`ThumbnailDecoder`] decodes the thumbnails of Stage 3's embeddings, for
//! the engine, with this same loop on a pool and budget of its own.
//!
//! Results are collected by file index, so the output never depends on how
//! the work was scheduled.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::SystemTime;

use image::RgbImage;
use serde::{Deserialize, Serialize};

use crate::budget::{DECODE_MEMORY_BUDGET, MemoryBudget};
use crate::decode::{
    ImageSource, Thumbnail, effective_kind, exif_read_peak, thumbnail, thumbnail_peak,
};
use crate::discover::{DiscoverError, DiscoverSkip, DiscoveredFile, discover};
use crate::exif::{ExifData, ExifError, read_exif};
use crate::fsio::display_path;
use crate::group::{FileFacts, RegroupInput, Thresholds, bucket, regroup};
use crate::guard;
use crate::hash::{HashPair, hash_pair, hash_pair_normalised};
use crate::model::{ConfigError, FileKind, JobConfig, Warning};
use crate::pairing::pair_raw_jpeg;

/// The long side of the thumbnail that is hashed (spec §5.3).
pub const THUMBNAIL_MAX_SIDE: u32 = 512;

/// The most threads the per-file pool of one analysis gets, however many
/// cores there are: more would mostly wait for the memory budget.
pub const MAX_WORKER_THREADS: usize = 16;

/// The names of the per-file pool's threads: this prefix, then the thread's
/// index.
pub const WORKER_THREAD_PREFIX: &str = "photostacker-analyze-";

/// The names of the threads that decode the thumbnails of Stage 3's
/// embeddings: this prefix, then the thread's index.
pub const DECODER_THREAD_PREFIX: &str = "photostacker-decode-";

/// A step of [`analyze`] or of the stack writers, as reported to their
/// progress callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// The folder scan; reported once, when it is done.
    Discover,
    /// The EXIF of the passive files of RAW+JPEG pairs.
    Metadata,
    /// EXIF, thumbnail and hashes of the active files, then of the partners
    /// of the RAWs that give way to them (spec §5.1). Those partners are only
    /// known once the active files are done, so they then raise the stage's
    /// `total` by their number, and their ticks go on counting from there.
    Hash,
    /// Stage 1 and Stage 2; reported once, when they are done.
    Group,
    /// The writing of the stacks ([`crate::write::write_group`] and
    /// [`crate::write::write_all`]): one tick per layer input, when it is
    /// encoded or left out, across all the groups written. `current` is the
    /// layer's file.
    Write,
    /// The embeddings of Stage 3, computed by the engine for the photos an
    /// ambiguous group waits for (spec §5.4): one tick per photo whose
    /// embedding is stored. `current` is the photo's file.
    Embed,
}

/// One progress report: `done` of `total` items of `stage` are finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub stage: Stage,
    pub done: usize,
    pub total: usize,
    /// The file just finished, in the per-file stages.
    pub current: Option<PathBuf>,
}

/// What the analysis learnt about one discovered file. A file's id is its
/// index in [`Analysis::files`], which is sorted by path.
#[derive(Debug, Clone, PartialEq)]
pub struct FileRecord {
    pub path: PathBuf,
    /// The kind the file was read and decoded as: the extension's, unless a
    /// still image's content has the signature of another still kind (see
    /// [`effective_kind`]). It stays the extension's when the file could not
    /// be read.
    pub kind: FileKind,
    pub size: u64,
    /// The modification time discovery read, in milliseconds since the Unix
    /// epoch: Stage 1 places a file without an EXIF capture time by it.
    pub mtime_ms: i64,
    pub fingerprint: String,
    /// An active file's EXIF, completed by its passive partners; a passive
    /// file's own EXIF.
    pub exif: Option<ExifData>,
    /// The plain hashes ([`hash_pair`]), compatible with Python's `imagehash`.
    /// Set for the active files that hashed; never for a passive file.
    pub hashes: Option<HashPair>,
    /// The brightness-normalised hashes ([`hash_pair_normalised`]) that
    /// grouping links on; set exactly when `hashes` is.
    pub normalised: Option<HashPair>,
    /// For a passive file of a RAW+JPEG pair: the id of its active partner.
    /// After a promotion (see [`analyze`]) the demoted RAW is passive too.
    pub paired_with: Option<usize>,
    /// Why the file could not be analysed; such a file is not grouped.
    pub error: Option<String>,
}

/// A stack proposed by Stage 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedGroup {
    /// File ids in layer order: `[0]` is the top layer.
    pub members: Vec<usize>,
    /// The key of the Stage-1 bucket the group came from.
    pub bucket: String,
    /// The largest normalised hash distance between two members.
    pub max_distance: u32,
}

/// The result of [`analyze`].
#[derive(Debug, Clone, PartialEq)]
pub struct Analysis {
    pub files: Vec<FileRecord>,
    /// In bucket order, then by smallest member id.
    pub groups: Vec<ProposedGroup>,
    /// Hashed active files that are in no group, ascending.
    pub unstacked: Vec<usize>,
    /// Ascending by file id.
    pub warnings: Vec<(usize, Warning)>,
    /// Entries the folder scan could not read; they have no file id.
    pub skipped: Vec<DiscoverSkip>,
}

/// The version of what a cached [`FileExam`] means: an exam put with another
/// format is a miss (spec §7.3). Bump it whenever a cached exam would no
/// longer be what examining its file gives today: an EXIF field added or
/// read differently, the thumbnail or the hashing changed, a warning added
/// or reworded, or [`FileExam`]'s serde form changed.
pub const EXAM_FORMAT: u32 = 1;

/// Which examination of a file an exam is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExamRole {
    /// The EXIF read of a passive file of a RAW+JPEG pair.
    Passive,
    /// The examination of an active file: EXIF, thumbnail and hashes.
    Active,
}

impl ExamRole {
    /// The role's spelling in the store's `exams.role` column. It is
    /// persisted: changing it needs a migration.
    pub fn as_str(self) -> &'static str {
        match self {
            ExamRole::Passive => "passive",
            ExamRole::Active => "active",
        }
    }
}

/// The error a passive file's exam holds as its `hashes`: a passive file only
/// lends its EXIF, and is never hashed.
const PASSIVE_NOT_HASHED: &str = "not hashed: the passive file of a pair";

/// What examining one file gave, as an [`ExamCache`] keeps it.
///
/// - An **active** file's exam: the kind it was read as, its own EXIF
///   completed by its passive partners' (`exif`) and its own EXIF alone
///   (`own_exif`), its warnings in order ([`Warning::ExifUnreadable`], then
///   [`Warning::RawPreviewOnly`]), and its plain and normalised hashes, or
///   its error. A file that could not be read at all, or whose EXIF reading
///   panicked, has no EXIF in either field. Only an exam that hashed is put
///   into an [`ExamCache`].
/// - A **passive** file's exam: the kind it was read as, the EXIF it lends
///   (its own, in both fields, or none when it has none), no warnings, and
///   the `hashes` error `not hashed: the passive file of a pair`. A passive
///   file whose EXIF could not be read has no exam.
///
/// The store keeps its serde form, `hashes` externally tagged
/// (`{"Ok":[plain, normalised]}` or `{"Err":"…"}`): changing it needs a new
/// [`EXAM_FORMAT`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileExam {
    pub kind: FileKind,
    pub exif: Option<ExifData>,
    pub own_exif: Option<ExifData>,
    pub warnings: Vec<Warning>,
    /// `(plain, normalised)`: [`hash_pair`] and [`hash_pair_normalised`].
    pub hashes: Result<(HashPair, HashPair), String>,
}

/// Where [`analyze_with`] looks up a file's exam before examining the file,
/// and puts each exam worth keeping. Called from the worker threads,
/// concurrently.
///
/// No method may panic or fail the analysis: an implementation swallows its
/// own errors (logging them if it likes). A `get` that fails is a miss, and a
/// `put` or `thumbnail` that fails just costs that file's work next time.
pub trait ExamCache: Sync {
    /// The exam put for `role` and `file`'s path, but only when it examined
    /// the same content ([`DiscoveredFile::fingerprint`]) with today's
    /// [`EXAM_FORMAT`]. A cache that cannot serve a hit, such as one whose
    /// thumbnail file is gone, returns `None`: the file is examined again.
    fn get(&self, role: ExamRole, file: &DiscoveredFile) -> Option<FileExam>;

    /// Keeps `exam`, made from `file` in `role`, replacing any exam of that
    /// role and path. Only exams that may be reused are put: an active exam
    /// that hashed, with as `thumb` the thumbnail its hashes were taken from
    /// (512 px long side at most), and a passive file's EXIF read that
    /// succeeded, with no `thumb`. A failed exam is never put, so its file is
    /// examined again on the next run, which heals a passing read error.
    fn put(&self, role: ExamRole, file: &DiscoveredFile, exam: &FileExam, thumb: Option<&RgbImage>);

    /// Hands over the thumbnail (512 px long side at most) that `file` was
    /// hashed from when it was promoted over its RAW (spec §5.1). Promotion
    /// is never cached, so this is the promoted file's only thumbnail; it is
    /// called again on every run that promotes the file. Does nothing by
    /// default.
    fn thumbnail(&self, _file: &DiscoveredFile, _thumb: &RgbImage) {}
}

/// The callback of [`AnalyzeHooks::file_failed`]: a file's path and its
/// error.
pub type FailureHook<'a> = dyn Fn(&Path, &str) + Sync + 'a;

/// What [`analyze_with`] reports to, is stopped by, and caches in.
pub struct AnalyzeHooks<'a> {
    /// See [`analyze`].
    pub progress: &'a (dyn Fn(Progress) + Sync),
    /// See [`analyze`].
    pub cancel: &'a AtomicBool,
    pub cache: Option<&'a dyn ExamCache>,
    /// Called with a file's path and its error, once for each file that ends
    /// the analysis with an error ([`FileRecord::error`]), as soon as that is
    /// known: at its examination when it has no passive partner that could
    /// take its place, else once its partners have been tried (step 4 of
    /// [`analyze`]). Never for a RAW that gives way to its partner, nor for a
    /// passive file, which never fails. Called from the worker threads,
    /// concurrently, with no lock held, or from the analysis' own thread
    /// after the partners' pass; a file that a cancelled analysis never
    /// finished is not reported.
    pub file_failed: Option<&'a FailureHook<'a>>,
}

/// Why [`analyze`] stopped without a result.
#[derive(Debug, thiserror::Error)]
pub enum AnalyzeError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Discover(#[from] DiscoverError),
    #[error("the analysis was cancelled")]
    Cancelled,
    #[error("cannot start the worker threads: {0}")]
    ThreadPool(#[from] rayon::ThreadPoolBuildError),
}

/// Runs the grouping pipeline on `config`:
///
/// 1. `validate`, then `discover` (leaving out the output folder), then
///    RAW+JPEG pairing.
/// 2. The EXIF of every passive file, in parallel.
/// 3. For every active file, in parallel: its EXIF, completed by its passive
///    partners' (spec §5.1), then the 512 px thumbnail and its plain and
///    normalised hashes.
/// 4. In parallel again, the passive JPEG and HEIC partners of every active
///    RAW that failed or was hashed only from its embedded preview, examined
///    the same way. The first of a RAW's partners by id that hashes takes its
///    place: it becomes active with [`Warning::PromotedOverRaw`], and the RAW
///    becomes its passive partner, with its own EXIF and no error or
///    warning. When none hashes, the RAW keeps its error or its preview.
/// 5. Stage 1 buckets, then in each bucket Stage 2 links and layer order
///    ([`regroup`], with Stage 3 off).
///
/// Steps 2 to 4 first sniff each file's content ([`effective_kind`]); that
/// kind reads its EXIF, decodes its image and goes into its record.
///
/// An active file that cannot be read or whose image cannot be decoded gets
/// an error and stays out of grouping; the rest goes on. An unreadable EXIF
/// is not fatal: an active file is grouped with its passive partners' EXIF in
/// its place, or without any, and gets [`Warning::ExifUnreadable`]; a passive
/// file just contributes nothing. An active RAW that rawler cannot decode is
/// hashed from its largest embedded JPEG when it has one, and gets
/// [`Warning::RawPreviewOnly`]. A panic in a file's work is caught too: an
/// active file gets `internal error while <step>: <message>` as its error,
/// and a passive file contributes nothing.
///
/// `progress` is called concurrently from the worker threads, with no lock
/// held. Within a per-file stage each `done` value from 1 to the stage's last
/// `total` comes exactly once, but the reports may arrive out of order. The
/// [`Stage::Hash`] total grows by the number of partners step 4 examines.
/// `cancel` is checked before each file and after each parallel step, and
/// then gives [`AnalyzeError::Cancelled`].
///
/// This is [`analyze_with`] without an exam cache.
pub fn analyze(
    config: &JobConfig,
    progress: &(dyn Fn(Progress) + Sync),
    cancel: &AtomicBool,
) -> Result<Analysis, AnalyzeError> {
    analyze_with(
        config,
        &AnalyzeHooks {
            progress,
            cancel,
            cache: None,
            file_failed: None,
        },
    )
}

/// [`analyze`], with the exam cache `hooks.cache` when there is one, so that
/// a scan that was cancelled or failed resumes where it stopped (spec §7.3).
///
/// Before it reads a passive file's EXIF (step 2), or examines an active
/// file (step 3), the analysis asks the cache for the file's exam. A hit
/// takes the place of the work: the file is neither read nor decoded, and
/// still ticks its progress. An active file's hit is taken only while its
/// passive partners complete its own EXIF into the very EXIF the exam holds;
/// otherwise the file is examined again, since its orientation, and so its
/// hashes, may come from them. Each fresh exam worth keeping is put before
/// its tick, so a cancelled analysis keeps the exams of the files it
/// finished. A failed one is never put (an active file that did not hash, a
/// passive file whose EXIF could not be read), so the next run examines that
/// file again. The partners of demoted RAWs (step 4) are never cached: they
/// are few, and what they give depends on their RAW's exam. Each one that is
/// promoted hands its thumbnail to [`ExamCache::thumbnail`] instead.
///
/// `hooks.file_failed` hears of each file that ends with an error as soon
/// as that is known, before the analysis returns (see
/// [`AnalyzeHooks::file_failed`]).
///
/// The result is the one [`analyze`] gives, hit or miss.
pub fn analyze_with(config: &JobConfig, hooks: &AnalyzeHooks) -> Result<Analysis, AnalyzeError> {
    let AnalyzeHooks {
        progress,
        cancel,
        cache,
        file_failed,
    } = *hooks;
    config.validate()?;
    let (discovered, skipped) = discover(
        &config.input_dir,
        config.recursive,
        std::slice::from_ref(&config.output_dir),
    )?;
    tracing::info!(
        input = %display_path(&config.input_dir),
        files = discovered.len(),
        skipped = skipped.len(),
        "discover: done"
    );
    progress(Progress {
        stage: Stage::Discover,
        done: discovered.len(),
        total: discovered.len(),
        current: None,
    });

    let partners = pair_raw_jpeg(&discovered, config.raw_jpeg_preference);
    let mut files: Vec<FileRecord> = discovered
        .iter()
        .zip(&partners)
        .map(|(file, &paired_with)| FileRecord::new(file, paired_with))
        .collect();

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(worker_threads())
        .thread_name(|i| format!("{WORKER_THREAD_PREFIX}{i}"))
        .build()?;
    let budget = MemoryBudget::new(DECODE_MEMORY_BUDGET);
    let work = Work {
        pool: &pool,
        budget: &budget,
        files: &discovered,
        progress,
        cancel,
        cache,
        file_failed,
    };
    read_passive_exif(&work, &mut files)?;
    let file_warnings = hash_active_files(&work, &mut files)?;

    // Grouping sees paths relative to the input folder, so bucket keys (and
    // the plan that shows them) do not depend on where the folder lives.
    let facts: Vec<FileFacts> = discovered
        .iter()
        .zip(&files)
        .map(|(file, record)| FileFacts {
            path: relative_to(&config.input_dir, &file.path),
            exif: record.exif.clone(),
            mtime_ms: record.mtime_ms,
        })
        .collect();
    let normalised: Vec<Option<HashPair>> = files.iter().map(|f| f.normalised).collect();
    let mut grouped = group(&facts, &normalised, config);
    // A stable sort by id keeps each file's own warnings, in their order,
    // before its Stage-1 one.
    let mut warnings = file_warnings;
    warnings.append(&mut grouped.warnings);
    warnings.sort_by_key(|&(id, _)| id);
    tracing::info!(
        hashed = grouped.hashed,
        groups = grouped.groups.len(),
        unstacked = grouped.unstacked.len(),
        "group: done"
    );
    progress(Progress {
        stage: Stage::Group,
        done: grouped.hashed,
        total: grouped.hashed,
        current: None,
    });

    Ok(Analysis {
        files,
        groups: grouped.groups,
        unstacked: grouped.unstacked,
        warnings,
        skipped,
    })
}

impl FileRecord {
    fn new(file: &DiscoveredFile, paired_with: Option<usize>) -> Self {
        Self {
            path: file.path.clone(),
            kind: file.kind,
            size: file.size,
            mtime_ms: unix_millis(file.mtime),
            fingerprint: file.fingerprint.clone(),
            exif: None,
            hashes: None,
            normalised: None,
            paired_with,
            error: None,
        }
    }
}

/// How many threads the per-file pool gets: one per core, up to
/// [`MAX_WORKER_THREADS`].
fn worker_threads() -> usize {
    let cores = thread::available_parallelism().map_or(1, NonZeroUsize::get);
    cores.min(MAX_WORKER_THREADS)
}

/// What every parallel step of one analysis shares.
struct Work<'a> {
    pool: &'a rayon::ThreadPool,
    /// The decodes' memory budget (see "Concurrency and memory").
    budget: &'a MemoryBudget,
    files: &'a [DiscoveredFile],
    progress: &'a (dyn Fn(Progress) + Sync),
    cancel: &'a AtomicBool,
    cache: Option<&'a dyn ExamCache>,
    /// See [`AnalyzeHooks::file_failed`].
    file_failed: Option<&'a FailureHook<'a>>,
}

impl Work<'_> {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Reports that the file at `path` ends the analysis with `error`, if
    /// there is a hook for it ([`AnalyzeHooks::file_failed`]).
    fn report_failure(&self, path: &Path, error: &str) {
        if let Some(file_failed) = self.file_failed {
            file_failed(path, error);
        }
    }

    /// Hands a promoted file's thumbnail to the cache, if there is one (see
    /// [`ExamCache::thumbnail`]).
    fn thumbnail(&self, file: &DiscoveredFile, thumb: &RgbImage) {
        if let Some(cache) = self.cache {
            cache.thumbnail(file, thumb);
        }
    }

    /// Runs `job` on each of `ids` (with its file) on the pool and returns the
    /// results in the order of `ids`, reporting each finished file as a
    /// `stage` tick. The ticks go on from `done_before` files of `stage`
    /// already reported, with `done_before + ids.len()` as the total.
    ///
    /// Every worker takes the next id from a shared counter until none is
    /// left, so a `job` may wait for a permit of [`Work::budget`] (see
    /// "Concurrency and memory").
    fn per_file<T: Send>(
        &self,
        stage: Stage,
        ids: &[usize],
        done_before: usize,
        job: impl Fn(usize, &DiscoveredFile) -> T + Sync,
    ) -> Result<Vec<T>, AnalyzeError> {
        let ticker = Ticker::new(self.progress, stage, done_before + ids.len(), done_before);
        let next = AtomicUsize::new(0);
        // See "Concurrency and memory": no loop starts before every worker
        // runs its own.
        let all_started = Barrier::new(self.pool.current_num_threads());
        let per_worker: Vec<Vec<(usize, T)>> = self.pool.broadcast(|_| {
            all_started.wait();
            let mut done = Vec::new();
            while !self.cancelled() {
                let at = next.fetch_add(1, Ordering::Relaxed);
                let Some(&id) = ids.get(at) else {
                    break;
                };
                let file = &self.files[id];
                // Gives the events of the file's work, down to the EXIF
                // reader's and rawler's, its path when they are logged at
                // `debug`. Only the warnings name it themselves, since the
                // span is off at the default filter.
                let span = tracing::debug_span!("file", path = %display_path(&file.path)).entered();
                done.push((at, job(id, file)));
                drop(span);
                ticker.tick(&file.path);
            }
            done
        });
        if self.cancelled() {
            return Err(AnalyzeError::Cancelled);
        }
        let mut results: Vec<Option<T>> = (0..ids.len()).map(|_| None).collect();
        for (at, result) in per_worker.into_iter().flatten() {
            results[at] = Some(result);
        }
        // Without a cancel, the workers stop only once the counter is past
        // the last id, so every id was taken.
        Ok(results
            .into_iter()
            .map(|result| result.expect("every id was taken by a worker"))
            .collect())
    }
}

/// Counts the finished files of one stage and reports each one.
pub(crate) struct Ticker<'a> {
    progress: &'a (dyn Fn(Progress) + Sync),
    stage: Stage,
    total: usize,
    done: AtomicUsize,
}

impl<'a> Ticker<'a> {
    /// A ticker of `stage` whose `total` items include `done_before` already
    /// reported; its first tick reports `done_before + 1`.
    pub(crate) fn new(
        progress: &'a (dyn Fn(Progress) + Sync),
        stage: Stage,
        total: usize,
        done_before: usize,
    ) -> Self {
        Self {
            progress,
            stage,
            total,
            done: AtomicUsize::new(done_before),
        }
    }

    /// Each call takes the next `done` value. The callback runs with no lock
    /// held, so two workers may report their values in either order.
    pub(crate) fn tick(&self, current: &Path) {
        let done = self.done.fetch_add(1, Ordering::Relaxed) + 1;
        (self.progress)(Progress {
            stage: self.stage,
            done,
            total: self.total,
            current: Some(current.to_path_buf()),
        });
    }
}

/// Sniffs every passive file and reads its EXIF into its record, or takes
/// both from the file's cached exam. A passive file only lends its EXIF to
/// its active partner, so one that cannot be read, or whose reading
/// panicked, is no error: it simply lends nothing.
fn read_passive_exif(work: &Work, files: &mut [FileRecord]) -> Result<(), AnalyzeError> {
    let passive: Vec<usize> = (0..files.len())
        .filter(|&id| files[id].paired_with.is_some())
        .collect();
    let results = work.per_file(Stage::Metadata, &passive, 0, |_, file| {
        passive_exam(file, work.budget, work.cache)
    })?;
    for (&id, (kind, exif)) in passive.iter().zip(results) {
        files[id].kind = kind;
        files[id].exif = exif;
    }
    tracing::info!(passive = passive.len(), "metadata: done");
    Ok(())
}

/// The kind and the EXIF that the passive `file` lends: from its exam in
/// `cache` when there is one, else read now, under a permit of `budget`
/// ([`read_exif_within`]), and put into `cache` when the read succeeded. A
/// file that cannot be read, or whose reading panicked, lends nothing.
fn passive_exam(
    file: &DiscoveredFile,
    budget: &MemoryBudget,
    cache: Option<&dyn ExamCache>,
) -> (FileKind, Option<ExifData>) {
    if let Some(exam) = cache.and_then(|cache| cache.get(ExamRole::Passive, file)) {
        tracing::debug!(kind = ?exam.kind, "passive file read from the exam cache");
        return (exam.kind, exam.exif);
    }
    let (kind, read) = match effective_kind(&file.path, file.kind) {
        Ok(kind) => {
            let read = step(&file.path, "reading the EXIF", || {
                read_exif_within(&file.path, kind, budget)
            })
            .and_then(|read| read.map_err(|e| format!("unreadable EXIF: {e}")));
            (kind, read)
        }
        Err(e) => (file.kind, Err(format!("cannot read the file: {e}"))),
    };
    match &read {
        Ok(exif) => {
            tracing::debug!(?kind, has_exif = exif.is_some(), "passive file read");
        }
        Err(error) => tracing::debug!(?kind, %error, "passive file lends no EXIF"),
    }
    // A read that failed is not cached: it may pass next time, and its
    // active partner's exam must then see what it lends.
    let Ok(exif) = read else {
        return (kind, None);
    };
    let exam = FileExam {
        kind,
        exif: exif.clone(),
        own_exif: exif,
        warnings: Vec::new(),
        hashes: Err(PASSIVE_NOT_HASHED.to_owned()),
    };
    if let Some(cache) = cache {
        cache.put(ExamRole::Passive, file, &exam, None);
    }
    (exam.kind, exam.exif)
}

/// The exam of the active `file`, whose passive partners lend
/// `partner_exif` (in ascending id): its exam in `cache` when that one holds
/// the EXIF they complete its own into, else a fresh one, put into `cache`
/// when it hashed.
fn active_exam(
    file: &DiscoveredFile,
    partner_exif: &[&ExifData],
    budget: &MemoryBudget,
    cache: Option<&dyn ExamCache>,
) -> FileExam {
    // The partners' EXIF is the one input of an exam besides the file.
    if let Some(exam) = cache.and_then(|cache| cache.get(ExamRole::Active, file))
        && exam.exif == complete_exif(exam.own_exif.clone(), partner_exif.iter().copied())
    {
        exam.log("active file taken from the exam cache");
        return exam;
    }
    let (exam, thumb) = examine(file, budget, partner_exif.iter().copied());
    exam.log("active file examined");
    // A failed exam is not cached, so a passing error heals on resume.
    if exam.hashes.is_ok()
        && let Some(cache) = cache
    {
        cache.put(ExamRole::Active, file, &exam, thumb.as_ref());
    }
    exam
}

/// The kind and the EXIF that `file` lends as a passive file, read as step 2
/// of [`analyze_with`] reads them, through `cache`. For one file outside any
/// analysis, with a memory budget of its own: the engine's swap of a pair's
/// active file.
pub(crate) fn read_passive(
    file: &DiscoveredFile,
    cache: Option<&dyn ExamCache>,
) -> (FileKind, Option<ExifData>) {
    let _span = tracing::debug_span!("file", path = %display_path(&file.path)).entered();
    let budget = MemoryBudget::new(DECODE_MEMORY_BUDGET);
    passive_exam(file, &budget, cache)
}

/// The exam of `file` as an active file whose passive partners lend
/// `partner_exif` (in ascending id), made as step 3 of [`analyze_with`]
/// makes it, through `cache`. For one file outside any analysis, with a
/// memory budget of its own: the engine's swap of a pair's active file.
pub(crate) fn examine_active(
    file: &DiscoveredFile,
    partner_exif: &[&ExifData],
    cache: Option<&dyn ExamCache>,
) -> FileExam {
    let _span = tracing::debug_span!("file", path = %display_path(&file.path)).entered();
    let budget = MemoryBudget::new(DECODE_MEMORY_BUDGET);
    active_exam(file, partner_exif, &budget, cache)
}

/// Reads, completes and hashes every active file into its record, or takes
/// its cached exam (see [`analyze_with`]), and returns the warnings of the
/// files that hashed (see [`FileExam`]): each file's together and in their
/// order, by ascending id but for a promoted file's, which stand where its
/// RAW's would ([`analyze`] sorts them by id). Needs the passive files' EXIF
/// in `files` already.
///
/// A demoted active RAW then gives way to the first of its passive partners
/// that examines well (see [`promote_partners`]): that partner becomes the
/// active file, with [`Warning::PromotedOverRaw`], and the RAW one of its
/// passive partners. File ids never change: only the records are rewritten.
fn hash_active_files(
    work: &Work,
    files: &mut [FileRecord],
) -> Result<Vec<(usize, Warning)>, AnalyzeError> {
    let active: Vec<usize> = (0..files.len())
        .filter(|&id| files[id].paired_with.is_none())
        .collect();
    // The passive partners of each active file, ascending: where one lacks a
    // field, the first that has it fills it.
    let mut partners: Vec<Vec<usize>> = vec![Vec::new(); files.len()];
    for (id, file) in files.iter().enumerate() {
        if let Some(active) = file.paired_with {
            partners[active].push(id);
        }
    }

    let records: &[FileRecord] = files;
    let outcomes = work.per_file(Stage::Hash, &active, 0, |id, file| {
        let partner_exif: Vec<&ExifData> = partners[id]
            .iter()
            .filter_map(|&p| records[p].exif.as_ref())
            .collect();
        let exam = active_exam(file, &partner_exif, work.budget, work.cache);
        // A RAW that a partner may still replace is reported, if at all,
        // once its partners have been tried.
        if let Err(error) = &exam.hashes
            && !may_give_way(records, &partners[id], &exam)
        {
            work.report_failure(&file.path, error);
        }
        exam
    })?;
    let mut examined: Vec<Option<FileExam>> = (0..files.len()).map(|_| None).collect();
    for (&id, outcome) in active.iter().zip(outcomes) {
        examined[id] = Some(outcome);
    }
    let mut promotions: Vec<Option<Promotion>> = (0..files.len()).map(|_| None).collect();
    for promotion in promote_partners(work, records, &partners, &examined, active.len())? {
        let raw = promotion.raw;
        tracing::debug!(
            raw = %display_path(&records[raw].path),
            partner = %display_path(&records[promotion.partner].path),
            "partner promoted over its RAW"
        );
        promotions[raw] = Some(promotion);
    }
    tracing::info!(
        active = active.len(),
        promoted = promotions.iter().flatten().count(),
        "hash: done"
    );
    // The files whose partners were tried had they failed: one that failed
    // and kept its error is known to have failed only now.
    let partners_tried: Vec<bool> = examined
        .iter()
        .enumerate()
        .map(|(id, outcome)| {
            outcome
                .as_ref()
                .is_some_and(|outcome| may_give_way(records, &partners[id], outcome))
        })
        .collect();

    let mut warnings = Vec::new();
    for (id, outcome) in examined.into_iter().enumerate() {
        let Some(outcome) = outcome else {
            continue;
        };
        if let Some(promotion) = promotions[id].take() {
            promote(files, &partners[id], id, outcome, promotion, &mut warnings);
            continue;
        }
        let record = &mut files[id];
        record.kind = outcome.kind;
        record.exif = outcome.exif;
        match outcome.hashes {
            Ok((plain, normalised)) => {
                record.hashes = Some(plain);
                record.normalised = Some(normalised);
                warnings.extend(outcome.warnings.into_iter().map(|w| (id, w)));
            }
            // A file that failed shows its error, not its warnings.
            Err(e) => {
                if partners_tried[id] {
                    work.report_failure(&record.path, &e);
                }
                record.error = Some(e);
            }
        }
    }
    Ok(warnings)
}

/// The passive partners among `partners` that may take the place of their
/// active RAW (spec §5.1): its JPEG and HEIC partners, never another RAW.
fn stand_ins<'a>(
    records: &'a [FileRecord],
    partners: &'a [usize],
) -> impl Iterator<Item = usize> + 'a {
    partners
        .iter()
        .copied()
        .filter(|&partner| records[partner].kind != FileKind::Raw)
}

/// Whether the active file examined as `outcome`, with the passive
/// `partners`, gives way to one of them should it be demoted: it is a RAW
/// with a stand-in ([`stand_ins`]), so [`promote_partners`] tries them when
/// it failed or was hashed only from its preview.
fn may_give_way(records: &[FileRecord], partners: &[usize], outcome: &FileExam) -> bool {
    outcome.kind == FileKind::Raw && stand_ins(records, partners).next().is_some()
}

/// A passive file that takes the place of its demoted active RAW.
struct Promotion {
    raw: usize,
    partner: usize,
    /// The partner's kind, its EXIF completed by its new passive partners,
    /// and its hashes.
    kind: FileKind,
    exif: Option<ExifData>,
    /// `(plain, normalised)`, as in [`FileExam::hashes`].
    hashes: (HashPair, HashPair),
    /// The partner's own warnings, then [`Warning::PromotedOverRaw`].
    warnings: Vec<Warning>,
}

/// Examines, in a second parallel pass, the partners of every demoted RAW
/// (spec §5.1), and returns one promotion per RAW whose partner examined well.
///
/// An active RAW is demoted when it failed or was hashed only from its
/// embedded preview (see [`FileExam::demote_reason`]) and has a passive JPEG
/// or HEIC partner; a passive RAW is never promoted. Each such partner is
/// examined like an active file, its EXIF gaps filled by its partners once
/// promoted (the RAW by its own EXIF, and the RAW's other partners) in
/// ascending id. The first partner by id that hashes is promoted; when none
/// does, the RAW keeps its outcome and its partners stay passive. These
/// examinations are never cached (see [`analyze_with`]); each promoted
/// partner's thumbnail goes to [`ExamCache::thumbnail`].
///
/// The pass reports to [`Stage::Hash`] after the `done_before` active files,
/// so that stage's total grows by the number of partners examined here.
fn promote_partners(
    work: &Work,
    records: &[FileRecord],
    partners: &[Vec<usize>],
    examined: &[Option<FileExam>],
    done_before: usize,
) -> Result<Vec<Promotion>, AnalyzeError> {
    let demote_reason = |raw: usize| {
        examined[raw]
            .as_ref()
            .filter(|outcome| outcome.kind == FileKind::Raw)
            .and_then(FileExam::demote_reason)
    };
    // Grouped by RAW, and each RAW's partners in ascending id.
    let candidates: Vec<usize> = (0..records.len())
        .filter(|&raw| demote_reason(raw).is_some())
        .flat_map(|raw| stand_ins(records, &partners[raw]))
        .collect();
    let raw_of = |partner: usize| {
        records[partner]
            .paired_with
            .expect("a candidate is a passive file, so it has a RAW")
    };

    let outcomes = work.per_file(Stage::Hash, &candidates, done_before, |id, file| {
        let raw = raw_of(id);
        let raw_exif = examined[raw].as_ref().and_then(|e| e.own_exif.as_ref());
        let mut new_partners: Vec<usize> = partners[raw]
            .iter()
            .copied()
            .filter(|&p| p != id)
            .chain([raw])
            .collect();
        new_partners.sort_unstable();
        let partner_exif = new_partners.into_iter().filter_map(|p| {
            if p == raw {
                raw_exif
            } else {
                records[p].exif.as_ref()
            }
        });
        let (examined, thumb) = examine(file, work.budget, partner_exif);
        examined.log("partner of a demoted RAW examined");
        (examined, thumb)
    })?;

    let mut promotions: Vec<Promotion> = Vec::new();
    for (&partner, (outcome, thumb)) in candidates.iter().zip(outcomes) {
        let raw = raw_of(partner);
        if promotions.last().is_some_and(|p| p.raw == raw) {
            continue;
        }
        // `examine` gives a thumbnail exactly when it hashed.
        let (Ok(hashes), Some(reason), Some(thumb)) = (outcome.hashes, demote_reason(raw), thumb)
        else {
            continue;
        };
        work.thumbnail(&work.files[partner], &thumb);
        let mut warnings = outcome.warnings;
        warnings.push(Warning::PromotedOverRaw {
            reason: reason.to_owned(),
        });
        promotions.push(Promotion {
            raw,
            partner,
            kind: outcome.kind,
            exif: outcome.exif,
            hashes,
            warnings,
        });
    }
    Ok(promotions)
}

/// Swaps the roles of the demoted RAW `raw` (examined as `outcome`, with its
/// passive `partners`) and the partner of `promotion`, and adds the promoted
/// file's warnings to `warnings`. The RAW becomes a passive file like any
/// other: its own EXIF, no hashes, no error and no warnings.
fn promote(
    files: &mut [FileRecord],
    partners: &[usize],
    raw: usize,
    outcome: FileExam,
    promotion: Promotion,
    warnings: &mut Vec<(usize, Warning)>,
) {
    let active = promotion.partner;
    let record = &mut files[raw];
    record.kind = outcome.kind;
    record.exif = outcome.own_exif;
    record.paired_with = Some(active);
    // The RAW's other partners now pair with the promoted file.
    for &other in partners.iter().filter(|&&p| p != active) {
        files[other].paired_with = Some(active);
    }
    let record = &mut files[active];
    record.kind = promotion.kind;
    record.exif = promotion.exif;
    let (plain, normalised) = promotion.hashes;
    record.hashes = Some(plain);
    record.normalised = Some(normalised);
    record.paired_with = None;
    warnings.extend(promotion.warnings.into_iter().map(|w| (active, w)));
}

impl FileExam {
    /// Logs at `debug`, in the file's span (see [`Work::per_file`]), what
    /// examining it gave: its kind, then its warnings or its error.
    fn log(&self, what: &str) {
        match &self.hashes {
            Ok(_) => tracing::debug!(
                kind = ?self.kind,
                warnings = ?self.warnings,
                "{what}: hashed"
            ),
            Err(error) => tracing::debug!(kind = ?self.kind, %error, "{what}: failed"),
        }
    }

    /// Why a RAW with this outcome gives way to its partner, if it does: its
    /// error, or rawler's error when it was hashed from its embedded preview.
    fn demote_reason(&self) -> Option<&str> {
        match &self.hashes {
            Err(error) => Some(error),
            Ok(_) => self.warnings.iter().find_map(|warning| match warning {
                Warning::RawPreviewOnly { reason } => Some(reason.as_str()),
                _ => None,
            }),
        }
    }
}

/// Sniffs `file`'s kind, reads its EXIF, fills its gaps from `partner_exif`
/// ([`complete_exif`]), then decodes the upright thumbnail and hashes it.
/// Unreadable EXIF counts as none, so the partners' EXIF then stands in for
/// it whole. A RAW that rawler cannot decode but whose embedded JPEG gave the
/// thumbnail is hashed and warned about. A file that cannot be read gets an
/// error, and a step that panics an internal error (see [`step`]).
///
/// The EXIF is read under a permit of `budget` ([`read_exif_within`]), given
/// back before the thumbnail is decoded under a permit of its estimated
/// peak, which is given back before hashing. The thumbnail is returned with
/// the exam when it was hashed, and dropped otherwise.
fn examine<'a>(
    file: &DiscoveredFile,
    budget: &MemoryBudget,
    partner_exif: impl IntoIterator<Item = &'a ExifData>,
) -> (FileExam, Option<RgbImage>) {
    let failed = |kind, error| {
        let exam = FileExam {
            kind,
            exif: None,
            own_exif: None,
            warnings: Vec::new(),
            hashes: Err(error),
        };
        (exam, None)
    };
    let kind = match effective_kind(&file.path, file.kind) {
        Ok(kind) => kind,
        Err(e) => return failed(file.kind, format!("cannot read the file: {e}")),
    };
    let read = match step(&file.path, "reading the EXIF", || {
        read_exif_within(&file.path, kind, budget)
    }) {
        Ok(read) => read,
        Err(internal) => return failed(kind, internal),
    };
    let (own_exif, exif_warning) = match read {
        Ok(exif) => (exif, None),
        Err(e) => (
            None,
            Some(Warning::ExifUnreadable {
                reason: e.to_string(),
            }),
        ),
    };
    let exif = complete_exif(own_exif.clone(), partner_exif);
    let mut warnings = Vec::from_iter(exif_warning);
    let orientation = exif.as_ref().map_or(1, |e| e.orientation);
    let decoded = decode_thumbnail(&file.path, kind, orientation, budget);
    let (hashes, thumb) = match decoded {
        Err(error) => (Err(error), None),
        Ok(thumb) => {
            if thumb.source == ImageSource::EmbeddedPreview {
                debug_assert!(
                    thumb.fallback_reason.is_some(),
                    "an embedded-preview thumbnail carries rawler's error"
                );
                warnings.push(Warning::RawPreviewOnly {
                    reason: thumb.fallback_reason.clone().unwrap_or_default(),
                });
            }
            let hashes = step(&file.path, "hashing the image", || {
                (hash_pair(&thumb.image), hash_pair_normalised(&thumb.image))
            });
            let hashed = hashes.is_ok().then_some(thumb.image);
            (hashes, hashed)
        }
    };
    let exam = FileExam {
        kind,
        exif,
        own_exif,
        warnings,
        hashes,
    };
    (exam, thumb)
}

/// The EXIF of the file at `path`, read as `kind` under a permit of `budget`
/// for the bytes the read holds ([`exif_read_peak`]), which is given back
/// before it returns, panic or not. The caller holds no other permit, and no
/// decode is below it on its stack (see "Concurrency and memory").
fn read_exif_within(
    path: &Path,
    kind: FileKind,
    budget: &MemoryBudget,
) -> Result<Option<ExifData>, ExifError> {
    let _permit = budget.acquire(exif_read_peak(path, kind));
    read_exif(path, kind)
}

/// The upright thumbnail (a [`THUMBNAIL_MAX_SIDE`] long side) of the file at
/// `path`, read as `kind` and turned by the EXIF `orientation`, decoded
/// under a permit of `budget` for its estimated peak, which is given back
/// before it returns. A decode that fails, or panics, gives its error.
fn decode_thumbnail(
    path: &Path,
    kind: FileKind,
    orientation: u16,
    budget: &MemoryBudget,
) -> Result<Thumbnail, String> {
    step(path, "decoding the image", || {
        let _permit = budget.acquire(thumbnail_peak(path, kind));
        thumbnail(path, kind, orientation, THUMBNAIL_MAX_SIDE)
    })
    .and_then(|decoded| decoded.map_err(|e| format!("cannot decode the image: {e}")))
}

/// Decodes thumbnails outside an analysis, as the analysis decodes them: on
/// a per-file pool of its own, under a memory budget of
/// [`DECODE_MEMORY_BUDGET`] bytes, with the loop that never waits for a
/// permit on top of another file's decode (see "Concurrency and memory").
/// For the embeddings of Stage 3, which the engine computes from the very
/// thumbnail the photo was hashed from.
pub(crate) struct ThumbnailDecoder {
    pool: rayon::ThreadPool,
    budget: MemoryBudget,
}

impl ThumbnailDecoder {
    /// A decoder with one thread per core, up to [`MAX_WORKER_THREADS`],
    /// named [`DECODER_THREAD_PREFIX`] and the thread's index.
    pub(crate) fn new() -> Result<Self, AnalyzeError> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(worker_threads())
            .thread_name(|i| format!("{DECODER_THREAD_PREFIX}{i}"))
            .build()?;
        Ok(Self {
            pool,
            budget: MemoryBudget::new(DECODE_MEMORY_BUDGET),
        })
    }

    /// The upright thumbnail of each of `files`, turned by the EXIF
    /// orientation at the same index of `orientations`, in order; or each
    /// one's error. `cancel` is checked before each file:
    /// [`AnalyzeError::Cancelled`] once it is set.
    pub(crate) fn decode(
        &self,
        files: &[DiscoveredFile],
        orientations: &[u16],
        cancel: &AtomicBool,
    ) -> Result<Vec<Result<RgbImage, String>>, AnalyzeError> {
        assert_eq!(files.len(), orientations.len(), "one orientation per file");
        let quiet = |_: Progress| {};
        let work = Work {
            pool: &self.pool,
            budget: &self.budget,
            files,
            progress: &quiet,
            cancel,
            cache: None,
            file_failed: None,
        };
        let ids: Vec<usize> = (0..files.len()).collect();
        work.per_file(Stage::Embed, &ids, 0, |id, file| {
            decode_thumbnail(&file.path, file.kind, orientations[id], &self.budget)
                .map(|thumb| thumb.image)
        })
    }
}

/// `own` with its gaps filled from each of `partners` in turn, or, without
/// `own`, the first partner's EXIF completed by the others. This is the EXIF
/// an active file is examined and grouped with (spec §5.1).
fn complete_exif<'a>(
    own: Option<ExifData>,
    partners: impl IntoIterator<Item = &'a ExifData>,
) -> Option<ExifData> {
    let mut exif = own;
    for other in partners {
        match &mut exif {
            Some(own) => own.fill_missing_from(other),
            None => exif = Some(other.clone()),
        }
    }
    exif
}

/// Runs one step of the analysis of the file `path`. A panic in it (a
/// decoder or parser bug on this input) is logged as a warning and becomes
/// the error `internal error while <what>: <panic message>`: an active
/// file's, while a passive file just lends nothing. The analysis goes on.
fn step<T>(path: &Path, what: &str, work: impl FnOnce() -> T) -> Result<T, String> {
    guard::catch(work).map_err(|message| {
        tracing::warn!(path = %display_path(path), error = %message, "caught a panic while {what}");
        format!("internal error while {what}: {message}")
    })
}

/// The outcome of Stage 1 and Stage 2.
struct Grouped {
    groups: Vec<ProposedGroup>,
    unstacked: Vec<usize>,
    warnings: Vec<(usize, Warning)>,
    /// How many files took part.
    hashed: usize,
}

/// Groups the hashed files with [`regroup`], Stage 3 off: Stage 1 buckets,
/// Stage 2 links each bucket (by the threshold and by its bracket runs), and
/// each stack gets its layer order. `hashes` are the normalised ones. Only
/// active files without an error have hashes, so only they are grouped.
fn group(facts: &[FileFacts], hashes: &[Option<HashPair>], config: &JobConfig) -> Grouped {
    let ids: Vec<usize> = (0..hashes.len())
        .filter(|&id| hashes[id].is_some())
        .collect();
    // `regroup` buckets the same files again, but keeps no warnings.
    let (_, warnings) = bucket(facts, &ids, config.time_window_seconds);
    let regrouped = regroup(&RegroupInput {
        facts,
        hashes,
        thresholds: Thresholds {
            hamming: config.hamming_threshold,
            // No group is above this floor, so Stage 3 never runs and the
            // cosine threshold is never read.
            floor: u32::MAX,
            cosine: 1.0,
            time_window_seconds: config.time_window_seconds,
        },
        ml: None,
        manual: &[],
        pinned: &[],
        existing: &[],
    });
    let groups = regrouped
        .groups
        .into_iter()
        .map(|group| ProposedGroup {
            members: group.members,
            bucket: group.bucket,
            max_distance: group.max_distance,
        })
        .collect();
    Grouped {
        groups,
        unstacked: regrouped.unstacked,
        warnings,
        hashed: ids.len(),
    }
}

/// `path` relative to `base` when it lies inside it, else `path` as is.
pub(crate) fn relative_to(base: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(base)
        .map_or_else(|_| path.to_path_buf(), Path::to_path_buf)
}

/// Milliseconds since the Unix epoch; negative before 1970, saturating.
fn unix_millis(time: SystemTime) -> i64 {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_millis()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_millis()).map_or(i64::MIN, |ms| -ms),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RawJpegPreference;
    use photostacker_testkit::{
        TiffSpec, Value, expose, jpeg_with_exif, scene, tiff_raw_with_jpegs,
    };
    use rayon::prelude::*;
    use std::collections::HashMap;
    use std::fs;
    use std::sync::{Mutex, mpsc};
    use std::time::Duration;

    #[test]
    fn per_file_never_waits_for_a_permit_on_top_of_another_file() {
        // Each job holds the whole budget (clamped) while it runs nested
        // rayon work, as a RAW develop under its permit does. A worker that
        // started its file loop inside a piece of another file's nested work
        // would wait for that file's permit forever. The race needs the
        // thread injecting a broadcast to be preempted between two workers'
        // jobs, so several pools run at once to oversubscribe the cores.
        // Without the barrier in `per_file` this stalled within seconds.
        const COPIES: usize = 12;
        const ROUNDS: usize = 2000;
        /// A stall this long, with no round finished anywhere, is a deadlock.
        const STALL: Duration = Duration::from_secs(10);
        static ROUNDS_DONE: AtomicUsize = AtomicUsize::new(0);

        let (finished, outcome) = mpsc::channel();
        for _ in 0..COPIES {
            let finished = finished.clone();
            // On threads of their own, so that a deadlock fails the test
            // instead of hanging it.
            thread::spawn(move || {
                let files: Vec<DiscoveredFile> = (0..16)
                    .map(|i| DiscoveredFile {
                        path: PathBuf::from(format!("{i}.jpg")),
                        kind: FileKind::Jpeg,
                        size: 0,
                        mtime: SystemTime::UNIX_EPOCH,
                        fingerprint: String::new(),
                    })
                    .collect();
                let ids: Vec<usize> = (0..files.len()).collect();
                for threads in [2, 3, 4] {
                    let pool = rayon::ThreadPoolBuilder::new()
                        .num_threads(threads)
                        .build()
                        .unwrap();
                    let budget = MemoryBudget::new(100);
                    let cancel = AtomicBool::new(false);
                    let work = Work {
                        pool: &pool,
                        budget: &budget,
                        files: &files,
                        progress: &|_| {},
                        cancel: &cancel,
                        cache: None,
                        file_failed: None,
                    };
                    for _ in 0..ROUNDS {
                        let sums = work
                            .per_file(Stage::Hash, &ids, 0, |_, _| {
                                let _permit = work.budget.acquire(u64::MAX);
                                (0..1000_u64).into_par_iter().sum::<u64>()
                            })
                            .unwrap();
                        assert!(sums.iter().all(|&sum| sum == 499_500), "{sums:?}");
                        ROUNDS_DONE.fetch_add(1, Ordering::Relaxed);
                    }
                }
                finished.send(()).unwrap();
            });
        }
        drop(finished);
        let mut copies_done = 0;
        let mut rounds_seen = 0;
        while copies_done < COPIES {
            match outcome.recv_timeout(STALL) {
                Ok(()) => copies_done += 1,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let rounds = ROUNDS_DONE.load(Ordering::Relaxed);
                    assert_ne!(
                        rounds, rounds_seen,
                        "per_file deadlocked after {rounds} rounds"
                    );
                    rounds_seen = rounds;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("a copy panicked"),
            }
        }
    }

    #[test]
    fn worker_threads_are_capped() {
        let cores = thread::available_parallelism().map_or(1, NonZeroUsize::get);
        assert_eq!(MAX_WORKER_THREADS, 16);
        assert_eq!(worker_threads(), cores.min(MAX_WORKER_THREADS));
    }

    #[test]
    fn a_panicking_step_becomes_an_internal_error() {
        let path = Path::new("x.jpg");
        assert_eq!(step(path, "hashing the image", || 3), Ok(3));
        assert_eq!(
            step(path, "hashing the image", || -> u8 {
                panic!("index out of bounds")
            }),
            Err("internal error while hashing the image: index out of bounds".into())
        );
    }

    #[test]
    fn unix_millis_handles_both_sides_of_the_epoch() {
        let epoch = SystemTime::UNIX_EPOCH;
        assert_eq!(unix_millis(epoch), 0);
        assert_eq!(unix_millis(epoch + Duration::from_millis(1500)), 1500);
        assert_eq!(unix_millis(epoch - Duration::from_millis(1500)), -1500);
    }

    // --- The exam cache ---

    /// One recorded put: the role's spelling, the file name, and whether a
    /// thumbnail came with it.
    type Put = (&'static str, String, bool);

    /// One recorded miss: the role's spelling and the file name.
    type Miss = (&'static str, String);

    /// An in-memory [`ExamCache`] that keeps each exam as the store does, as
    /// JSON under its role, path and fingerprint, and records every put,
    /// every miss, and every promoted file's thumbnail (name and size).
    #[derive(Default)]
    struct RecordingCache {
        exams: Mutex<HashMap<(ExamRole, PathBuf), (String, String)>>,
        puts: Mutex<Vec<Put>>,
        misses: Mutex<Vec<Miss>>,
        thumbnails: Mutex<Vec<(String, (u32, u32))>>,
    }

    /// The records since the last take, sorted.
    fn take<T: Ord>(records: &Mutex<Vec<T>>) -> Vec<T> {
        let mut records = std::mem::take(&mut *records.lock().unwrap());
        records.sort();
        records
    }

    fn name_of(file: &DiscoveredFile) -> String {
        file.path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    fn assert_hashed_thumbnail(thumb: &RgbImage) {
        let side = thumb.width().max(thumb.height());
        assert!(side <= THUMBNAIL_MAX_SIDE, "the hashed thumbnail: {side}");
    }

    impl RecordingCache {
        fn take_puts(&self) -> Vec<Put> {
            take(&self.puts)
        }

        fn take_misses(&self) -> Vec<Miss> {
            take(&self.misses)
        }

        fn take_thumbnails(&self) -> Vec<(String, (u32, u32))> {
            take(&self.thumbnails)
        }
    }

    impl ExamCache for RecordingCache {
        fn get(&self, role: ExamRole, file: &DiscoveredFile) -> Option<FileExam> {
            let hit = self
                .exams
                .lock()
                .unwrap()
                .get(&(role, file.path.clone()))
                .filter(|(fingerprint, _)| *fingerprint == file.fingerprint)
                .map(|(_, json)| serde_json::from_str(json).unwrap());
            if hit.is_none() {
                self.misses
                    .lock()
                    .unwrap()
                    .push((role.as_str(), name_of(file)));
            }
            hit
        }

        fn put(
            &self,
            role: ExamRole,
            file: &DiscoveredFile,
            exam: &FileExam,
            thumb: Option<&RgbImage>,
        ) {
            if let Some(thumb) = thumb {
                assert_hashed_thumbnail(thumb);
            }
            let json = serde_json::to_string(exam).unwrap();
            self.exams
                .lock()
                .unwrap()
                .insert((role, file.path.clone()), (file.fingerprint.clone(), json));
            self.puts
                .lock()
                .unwrap()
                .push((role.as_str(), name_of(file), thumb.is_some()));
        }

        fn thumbnail(&self, file: &DiscoveredFile, thumb: &RgbImage) {
            assert_hashed_thumbnail(thumb);
            self.thumbnails
                .lock()
                .unwrap()
                .push((name_of(file), thumb.dimensions()));
        }
    }

    /// Canon EXIF with a capture time `second` seconds into the minute.
    fn exif_at(second: u32) -> Vec<u8> {
        TiffSpec {
            little_endian: true,
            ifd0: vec![
                (0x010F, Value::Ascii("Canon".into())),
                (0x0110, Value::Ascii("Canon EOS 2000D".into())),
            ],
            exif: vec![(
                0x9003,
                Value::Ascii(format!("2026:05:01 10:00:{second:02}")),
            )],
            maker_note: None,
        }
        .build()
    }

    fn write_jpeg(dir: &Path, name: &str, image: &RgbImage, second: u32) {
        fs::write(dir.join(name), jpeg_with_exif(image, &exif_at(second), 90)).unwrap();
    }

    /// A folder with every kind of exam: a bracket of three, a lone shot
    /// (`b.jpg`), a file that cannot be decoded (`bad.jpg`), and a pair whose
    /// RAW, hashed only from its preview, gives way to its JPEG.
    fn folder() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let base = scene(1, 640, 480);
        for (i, ev) in [-1.0, 0.0, 1.0].into_iter().enumerate() {
            write_jpeg(
                dir.path(),
                &format!("a{i}.jpg"),
                &expose(&base, ev),
                i as u32,
            );
        }
        write_jpeg(dir.path(), "b.jpg", &scene(2, 640, 480), 30);
        fs::write(dir.path().join("bad.jpg"), b"not a photo").unwrap();
        fs::write(
            dir.path().join("p.nef"),
            tiff_raw_with_jpegs(&[(160, 120), (640, 480)]),
        )
        .unwrap();
        write_jpeg(dir.path(), "p.jpg", &scene(3, 640, 480), 50);
        dir
    }

    fn fresh(config: &JobConfig) -> Analysis {
        analyze(config, &|_| {}, &AtomicBool::new(false)).unwrap()
    }

    fn cached(
        config: &JobConfig,
        cache: &RecordingCache,
        progress: &(dyn Fn(Progress) + Sync),
    ) -> Result<Analysis, AnalyzeError> {
        let cancel = AtomicBool::new(false);
        analyze_with(
            config,
            &AnalyzeHooks {
                progress,
                cancel: &cancel,
                cache: Some(cache),
                file_failed: None,
            },
        )
    }

    fn put(role: &'static str, name: &str, thumb: bool) -> Put {
        (role, name.to_owned(), thumb)
    }

    /// The counts and the files of every progress report, each sorted, since
    /// which file gets which `done` value depends on scheduling.
    fn sorted(reports: Mutex<Vec<Progress>>) -> Vec<String> {
        let mut reports: Vec<String> = reports
            .into_inner()
            .unwrap()
            .iter()
            .flat_map(|p| {
                [
                    format!("{:?} {} of {}", p.stage, p.done, p.total),
                    format!("{:?} {:?}", p.stage, p.current),
                ]
            })
            .collect();
        reports.sort();
        reports
    }

    #[test]
    fn cached_analysis_equals_a_fresh_one() {
        let dir = folder();
        let config = JobConfig::new(dir.path().to_path_buf());
        let fresh_reports = Mutex::new(Vec::new());
        let fresh = analyze(
            &config,
            &|p| fresh_reports.lock().unwrap().push(p),
            &AtomicBool::new(false),
        )
        .unwrap();
        // Preconditions: the RAW gave way to its JPEG, and bad.jpg failed.
        let id = |name: &str| {
            let path = dir.path().join(name);
            fresh.files.iter().position(|f| f.path == path).unwrap()
        };
        assert_eq!(fresh.files[id("p.nef")].paired_with, Some(id("p.jpg")));
        assert!(fresh.files[id("p.jpg")].hashes.is_some());
        assert!(fresh.files[id("bad.jpg")].error.is_some());

        let cache = RecordingCache::default();
        assert_eq!(cached(&config, &cache, &|_| {}).unwrap(), fresh);
        assert_eq!(cache.take_misses().len(), 7);
        // Each file that examined well once, in its role, with its thumbnail
        // when it was hashed. bad.jpg failed, so it is not put. The promoted
        // JPEG's examination as an active file is not cached: only its
        // thumbnail is handed over, and no other file's is.
        assert_eq!(
            cache.take_puts(),
            [
                put("active", "a0.jpg", true),
                put("active", "a1.jpg", true),
                put("active", "a2.jpg", true),
                put("active", "b.jpg", true),
                put("active", "p.nef", true),
                put("passive", "p.jpg", false),
            ]
        );
        let promoted_thumbnail = [("p.jpg".to_owned(), (512, 384))];
        assert_eq!(cache.take_thumbnails(), promoted_thumbnail);

        let reports = Mutex::new(Vec::new());
        let again = cached(&config, &cache, &|p| reports.lock().unwrap().push(p)).unwrap();
        assert_eq!(again, fresh);
        assert_eq!(cache.take_puts(), []);
        // Only the failed file is examined again; the promotion runs again
        // too, and hands its thumbnail over again.
        assert_eq!(cache.take_misses(), [("active", "bad.jpg".to_owned())]);
        assert_eq!(cache.take_thumbnails(), promoted_thumbnail);
        // The hits still tick, as the fresh examinations did.
        assert_eq!(sorted(reports), sorted(fresh_reports));
    }

    #[test]
    fn failed_reads_are_never_cached() {
        // With the JPEG preferred, x.nef is the passive partner of x.jpg, and
        // its EXIF cannot be read.
        let dir = tempfile::tempdir().unwrap();
        write_jpeg(dir.path(), "x.jpg", &scene(1, 640, 480), 0);
        fs::write(dir.path().join("x.nef"), b"not a raw").unwrap();
        fs::write(dir.path().join("bad.jpg"), b"not a photo").unwrap();
        let mut config = JobConfig::new(dir.path().to_path_buf());
        config.raw_jpeg_preference = RawJpegPreference::Jpeg;
        let fresh = fresh(&config);
        let id = |name: &str| {
            let path = dir.path().join(name);
            fresh.files.iter().position(|f| f.path == path).unwrap()
        };
        assert_eq!(fresh.files[id("x.nef")].paired_with, Some(id("x.jpg")));
        assert!(fresh.files[id("bad.jpg")].error.is_some());

        let cache = RecordingCache::default();
        let failures = [
            ("active", "bad.jpg".to_owned()),
            ("passive", "x.nef".to_owned()),
        ];
        assert_eq!(cached(&config, &cache, &|_| {}).unwrap(), fresh);
        assert_eq!(cache.take_puts(), [put("active", "x.jpg", true)]);
        cache.take_misses();
        // Both failures are read again, and still not put.
        assert_eq!(cached(&config, &cache, &|_| {}).unwrap(), fresh);
        assert_eq!(cache.take_puts(), []);
        assert_eq!(cache.take_misses(), failures);
    }

    #[test]
    fn changed_file_is_examined_again() {
        let dir = folder();
        let config = JobConfig::new(dir.path().to_path_buf());
        let cache = RecordingCache::default();
        cached(&config, &cache, &|_| {}).unwrap();
        cache.take_puts();

        write_jpeg(dir.path(), "b.jpg", &scene(4, 640, 480), 30);
        assert_eq!(cached(&config, &cache, &|_| {}).unwrap(), fresh(&config));
        assert_eq!(cache.take_puts(), [put("active", "b.jpg", true)]);
    }

    #[test]
    fn active_file_is_examined_again_when_its_partner_changes() {
        // p.nef's EXIF has no capture time, so p.jpg's completes it: a new
        // time in p.jpg makes p.nef's cached exam stale too.
        let dir = folder();
        let config = JobConfig::new(dir.path().to_path_buf());
        let cache = RecordingCache::default();
        cached(&config, &cache, &|_| {}).unwrap();
        cache.take_puts();

        write_jpeg(dir.path(), "p.jpg", &scene(3, 640, 480), 51);
        assert_eq!(cached(&config, &cache, &|_| {}).unwrap(), fresh(&config));
        assert_eq!(
            cache.take_puts(),
            [put("active", "p.nef", true), put("passive", "p.jpg", false)]
        );
    }

    #[test]
    fn cancelled_analysis_keeps_its_puts() {
        let dir = folder();
        let config = JobConfig::new(dir.path().to_path_buf());
        let cache = RecordingCache::default();
        let cancel = AtomicBool::new(false);
        let cancelled = analyze_with(
            &config,
            &AnalyzeHooks {
                progress: &|p| {
                    if p.stage == Stage::Hash {
                        cancel.store(true, Ordering::Relaxed);
                    }
                },
                cancel: &cancel,
                cache: Some(&cache),
                file_failed: None,
            },
        );
        assert!(matches!(cancelled, Err(AnalyzeError::Cancelled)));
        // Every file taken before the cancel was finished and put before its
        // tick; a0.jpg, the first active file taken, among them.
        let kept = cache.take_puts();
        assert!(kept.contains(&put("active", "a0.jpg", true)), "{kept:?}");

        // Resuming examines only the rest, and ends as a fresh analysis.
        assert_eq!(cached(&config, &cache, &|_| {}).unwrap(), fresh(&config));
        let rest = cache.take_puts();
        assert!(rest.iter().all(|p| !kept.contains(p)), "{kept:?} {rest:?}");
        // Six exams are worth keeping: all but the failed bad.jpg's.
        assert_eq!(kept.len() + rest.len(), 6, "{kept:?} {rest:?}");
    }

    #[test]
    fn file_failed_reports_each_failed_file_once_as_soon_as_it_is_known() {
        // In `folder`, bad.jpg fails with no partner to take its place, and
        // p.nef, hashed from its preview, gives way to p.jpg. r.nef cannot be
        // read and gives way to r.jpg. q.nef cannot be read, and its partner
        // q.jpg cannot be decoded either: the RAW keeps its error.
        let dir = folder();
        fs::write(dir.path().join("q.nef"), b"not a raw").unwrap();
        fs::write(dir.path().join("q.jpg"), b"not a photo").unwrap();
        fs::write(dir.path().join("r.nef"), b"not a raw").unwrap();
        write_jpeg(dir.path(), "r.jpg", &scene(5, 640, 480), 40);
        let config = JobConfig::new(dir.path().to_path_buf());
        let name = |path: &Path| path.file_name().unwrap().to_string_lossy().into_owned();
        let log = Mutex::new(Vec::new());
        let cancel = AtomicBool::new(false);
        let analysis = analyze_with(
            &config,
            &AnalyzeHooks {
                progress: &|p| {
                    if let (Stage::Hash, Some(current)) = (p.stage, &p.current) {
                        log.lock().unwrap().push(format!("tick {}", name(current)));
                    }
                },
                cancel: &cancel,
                cache: None,
                file_failed: Some(&|path, message| {
                    log.lock()
                        .unwrap()
                        .push(format!("failed {}: {message}", name(path)));
                }),
            },
        )
        .unwrap();
        let log = log.into_inner().unwrap();
        let error_of = |file: &str| {
            let record = analysis.files.iter().find(|f| name(&f.path) == file);
            record.unwrap().error.clone().unwrap()
        };
        let failed = |file: &str| format!("failed {file}: {}", error_of(file));
        let failures: Vec<&String> = log.iter().filter(|l| l.starts_with("failed")).collect();
        assert_eq!(failures.len(), 2, "{log:#?}");
        let at = |line: &str| log.iter().position(|l| l == line).expect(line);
        // bad.jpg at its examination, before its tick; q.nef once its
        // partner was tried, after every tick.
        assert!(at(&failed("bad.jpg")) < at("tick bad.jpg"), "{log:#?}");
        assert!(at("tick q.jpg") < at(&failed("q.nef")), "{log:#?}");
        assert_eq!(log.last(), Some(&failed("q.nef")), "{log:#?}");
        // The RAWs that gave way are no failures.
        for raw in ["p.nef", "r.nef"] {
            let record = analysis.files.iter().find(|f| name(&f.path) == raw);
            let record = record.unwrap();
            assert!(record.paired_with.is_some(), "{raw}");
            assert_eq!(record.error, None, "{raw}");
        }
    }

    #[test]
    fn file_exam_json_is_pinned() {
        // The store keeps this JSON: changing it needs a new EXAM_FORMAT.
        let hashed = FileExam {
            kind: FileKind::Raw,
            exif: Some(ExifData {
                orientation: 6,
                ..ExifData::default()
            }),
            own_exif: None,
            warnings: vec![Warning::RawPreviewOnly {
                reason: "unknown body".into(),
            }],
            hashes: Ok((
                HashPair { phash: 1, dhash: 2 },
                HashPair {
                    phash: u64::MAX,
                    dhash: 0,
                },
            )),
        };
        let json = r#"{"kind":"raw","exif":{"orientation":6},"own_exif":null,"warnings":[{"raw_preview_only":{"reason":"unknown body"}}],"hashes":{"Ok":[{"phash":1,"dhash":2},{"phash":18446744073709551615,"dhash":0}]}}"#;
        assert_eq!(serde_json::to_string(&hashed).unwrap(), json);
        assert_eq!(serde_json::from_str::<FileExam>(json).unwrap(), hashed);

        let failed = FileExam {
            kind: FileKind::Jpeg,
            exif: None,
            own_exif: None,
            warnings: Vec::new(),
            hashes: Err("cannot decode the image: corrupt".into()),
        };
        let json = r#"{"kind":"jpeg","exif":null,"own_exif":null,"warnings":[],"hashes":{"Err":"cannot decode the image: corrupt"}}"#;
        assert_eq!(serde_json::to_string(&failed).unwrap(), json);
        assert_eq!(serde_json::from_str::<FileExam>(json).unwrap(), failed);

        assert_eq!(ExamRole::Active.as_str(), "active");
        assert_eq!(ExamRole::Passive.as_str(), "passive");
    }
}
