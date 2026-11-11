//! A benchmark of the review (spec §4.3, "What a review edit costs"): the
//! engine's review reads and edits, timed on a large synthetic job built
//! straight in the store. It never asserts a time, only that the job groups
//! as built, and it is ignored by default. Run it with
//!
//! ```sh
//! just test --run-ignored only -E 'test(bench_review)' --no-capture
//! ```
//!
//! It prints one line per command, `N=<files> <command>: min <ms>, median
//! <ms>, max <ms>`.
//!
//! The job: N photos in brackets of 5, each bracket 30 s after the last and
//! its shots 0.3 s apart, with a time window of 4 s. A bracket has a pseudo-random pair of hashes, and
//! its shot `j` flips their `j` low bits, so shots 0 and 4 are at distance
//! 8: inside the default Hamming threshold of 8 and above the Stage-3 floor
//! of 6, so every group is ambiguous and Stage 3 checks it with the stored
//! embeddings, one random unit vector per bracket with a little noise per
//! shot. With EXIF (a camera model, no capture time) the photos bucket by
//! their times, a bracket per bucket; without it they make one bucket per
//! folder, whose regrouping is quadratic.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use image::RgbImage;
use photostacker_core::analyze::{Analysis, FileRecord};
use photostacker_core::engine::{
    EmbedSource, Engine, EngineOptions, GroupId, GroupView, JobId, MoveTarget,
};
use photostacker_core::exif::ExifData;
use photostacker_core::hash::HashPair;
use photostacker_core::model::{FileKind, JobStatus, MlStatus};
use photostacker_core::models::ModelSpec;
use photostacker_core::{AppDirs, JobConfig, Store};
use tempfile::TempDir;

/// The photos of a bracket.
const BRACKET: usize = 5;
/// The model the job names, and the length of its embeddings.
const MODEL: &str = "dinov2-small";
const DIMS: usize = 384;
/// The time window of the job and of its edits: shorter than the 28.8 s between
/// brackets, so each bracket is a bucket of its own.
const WINDOW: u32 = 4;
/// How many times each repeated command runs.
const REPEATS: usize = 5;
/// The characters [`cold_review`] adds to the folder's name, so that each
/// file's row is about as long as a camera's full EXIF makes it.
const PAD: usize = 200;

#[test]
#[ignore = "a benchmark: run it on its own, with --run-ignored only"]
fn bench_review() {
    for files in [5_000, 20_000] {
        for pad in [0, PAD] {
            cold_review(files, pad);
        }
    }
    for files in [5_000, 20_000] {
        run(files, true);
    }
    for files in [5_000, 20_000] {
        run(files, false);
    }
}

/// Stage 3's source of embeddings, which is never called: every photo's
/// embedding is stored before the engine starts.
#[derive(Default)]
struct StoredOnly {
    calls: AtomicUsize,
}

impl EmbedSource for StoredOnly {
    fn embed(&self, _: &ModelSpec, _: &[RgbImage]) -> Result<Vec<Vec<f32>>, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err("the benchmark stores every embedding before it starts".into())
    }
}

/// SplitMix64: a small, fixed pseudo-random sequence, so every run builds
/// the same job.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in [-1, 1).
    fn signed(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 23) as f32 - 1.0
    }
}

/// The analysis of `files` photos in `input` (see the module docs).
fn analysis(input: &Path, files: usize, exif: bool, random: &mut Random) -> Analysis {
    let mut records = Vec::with_capacity(files);
    let mut bracket = HashPair { phash: 0, dhash: 0 };
    for i in 0..files {
        let shot = i % BRACKET;
        if shot == 0 {
            bracket = HashPair {
                phash: random.next(),
                dhash: random.next(),
            };
        }
        let flipped = (1u64 << shot) - 1;
        let hashes = HashPair {
            phash: bracket.phash ^ flipped,
            dhash: bracket.dhash ^ flipped,
        };
        records.push(FileRecord {
            path: input.join(format!("IMG_{i:06}.jpg")),
            kind: FileKind::Jpeg,
            size: 1_000_000,
            mtime_ms: ((i / BRACKET) * 30_000 + shot * 300) as i64,
            fingerprint: format!("bench {i}"),
            exif: exif.then(|| ExifData {
                model: Some("Bench Camera".into()),
                ..ExifData::default()
            }),
            hashes: Some(hashes),
            normalised: Some(hashes),
            paired_with: None,
            error: None,
        });
    }
    Analysis {
        files: records,
        groups: Vec::new(),
        unstacked: Vec::new(),
        warnings: Vec::new(),
        skipped: Vec::new(),
    }
}

/// `vector` scaled to length 1.
fn normalised(mut vector: Vec<f32>) -> Vec<f32> {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    for value in &mut vector {
        *value /= norm;
    }
    vector
}

/// One embedding per photo: its bracket's random unit vector, with a little
/// noise of its own.
fn embeddings(files: usize, random: &mut Random) -> Vec<Vec<f32>> {
    let mut vectors = Vec::with_capacity(files);
    let mut bracket = Vec::new();
    for i in 0..files {
        if i % BRACKET == 0 {
            bracket = normalised((0..DIMS).map(|_| random.signed()).collect());
        }
        let noisy = bracket
            .iter()
            .map(|value| value + 0.01 * random.signed())
            .collect();
        vectors.push(normalised(noisy));
    }
    vectors
}

/// Stores the job of `files` photos in `input` in the database `db`, in
/// Review Pending with every embedding and no group yet.
fn build_job(db: &Path, input: &Path, files: usize, exif: bool) -> JobId {
    let mut random = Random(0x5EED);
    let mut store = Store::open(db).unwrap();
    let mut config = JobConfig::new(input.to_path_buf());
    config.time_window_seconds = WINDOW;
    config.ml_model = Some(MODEL.into());
    let job = store.insert_job(&config).unwrap();
    store
        .save_analysis(job, &analysis(input, files, exif, &mut random))
        .unwrap();
    let rows = store.file_row_ids(job).unwrap();
    let vectors = embeddings(files, &mut random);
    let stored: Vec<(i64, &[f32])> = rows
        .iter()
        .copied()
        .zip(vectors.iter().map(Vec::as_slice))
        .collect();
    store.put_embeddings(MODEL, &stored).unwrap();
    store.set_scan_done(job, true).unwrap();
    store.set_job_status(job, JobStatus::ReviewPending).unwrap();
    job
}

/// How long `command` takes.
fn timed(command: impl FnOnce()) -> Duration {
    let start = Instant::now();
    command();
    start.elapsed()
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

/// Prints the line of `command`, timed `times`.
fn report(files: usize, command: &str, times: &[Duration]) {
    let mut sorted = times.to_vec();
    sorted.sort_unstable();
    let middle = sorted.len() / 2;
    let median = if sorted.len().is_multiple_of(2) {
        (sorted[middle - 1] + sorted[middle]) / 2
    } else {
        sorted[middle]
    };
    // The max shows a slow outlier, which the median of a few runs hides.
    println!(
        "N={files} {command}: min {:.2}, median {:.2}, max {:.2}",
        ms(sorted[0]),
        ms(median),
        ms(sorted[sorted.len() - 1])
    );
}

fn ids(groups: &[GroupView]) -> Vec<GroupId> {
    groups.iter().map(|group| group.id).collect()
}

/// An engine on a new database that holds the job of `files` photos.
/// Dropped in this order: the engine before its folder.
struct Session {
    engine: Engine,
    job: JobId,
    embedder: Arc<StoredOnly>,
    dirs: AppDirs,
    _data: TempDir,
}

impl Session {
    /// The job of `files` photos, with EXIF or without, in a folder whose
    /// name is `pad` characters longer than `photos`.
    fn start(files: usize, exif: bool, pad: usize) -> Session {
        let data = tempfile::tempdir().unwrap();
        let dirs = AppDirs::under(data.path().to_path_buf());
        let input = data.path().join(format!("photos{}", "p".repeat(pad)));
        let job = build_job(&dirs.db(), &input, files, exif);
        let embedder = Arc::new(StoredOnly::default());
        let engine = Engine::start(EngineOptions {
            db: dirs.db(),
            thumbs_dir: dirs.thumbs(),
            models_dir: dirs.models(),
            embedder: Some(Arc::clone(&embedder) as Arc<dyn EmbedSource>),
            installer: None,
            wake: Arc::new(|| {}),
        })
        .unwrap();
        Session {
            engine,
            job,
            embedder,
            dirs,
            _data: data,
        }
    }

    fn edit(&self, hamming: u32) -> Duration {
        timed(|| {
            self.engine
                .set_thresholds(self.job, hamming, WINDOW, None)
                .unwrap()
        })
    }

    /// The reads a review screen makes to draw the job again.
    fn redraw(&self) {
        drop(self.engine.groups(self.job).unwrap());
        drop(self.engine.files(self.job).unwrap());
        drop(self.engine.unstacked(self.job).unwrap());
    }
}

/// The two orderings in which a review read can starve the writer of the
/// page cache all connections share (spec §4.3, "What a review edit
/// costs"), each on a new engine, whose writer holds only its own share of
/// the cache when the first read comes: the first grouping, one `files()`
/// and edits; and the first grouping, then edits each after the reads that
/// draw the job again. Then what SQLite and the process hold in memory.
fn cold_review(files: usize, pad: usize) {
    let hammings = [9, 8, 9, 8, 9, 8];
    {
        let session = Session::start(files, true, pad);
        session.edit(8);
        drop(session.engine.files(session.job).unwrap());
        let edits: Vec<Duration> = hammings.iter().map(|&h| session.edit(h)).collect();
        report(
            files,
            &format!("pad {pad}: edits after the first grouping and files()"),
            &edits,
        );
    }
    let session = Session::start(files, true, pad);
    sqlite_memory_highwater_reset();
    session.edit(8);
    let edits: Vec<Duration> = hammings
        .iter()
        .map(|&h| {
            session.redraw();
            session.edit(h)
        })
        .collect();
    session.redraw();
    report(
        files,
        &format!("pad {pad}: edits after redraws, from the first grouping"),
        &edits,
    );
    let (used, peak) = sqlite_memory();
    println!(
        "N={files} pad {pad}: memory after the review session: SQLite {used:.1} MiB (peak {peak:.1} MiB), resident {} MiB",
        resident_mib().map_or("?".to_string(), |mib| format!("{mib:.0}"))
    );
}

/// What SQLite's allocator holds now and at most since the last reset, in
/// MiB: the page caches of every connection of the process, mostly.
fn sqlite_memory() -> (f64, f64) {
    const MIB: f64 = 1024.0 * 1024.0;
    // SAFETY: both only read SQLite's memory counters.
    let (used, peak) = unsafe {
        (
            rusqlite::ffi::sqlite3_memory_used(),
            rusqlite::ffi::sqlite3_memory_highwater(0),
        )
    };
    (used as f64 / MIB, peak as f64 / MIB)
}

fn sqlite_memory_highwater_reset() {
    // SAFETY: it only resets SQLite's high-water mark.
    unsafe {
        rusqlite::ffi::sqlite3_memory_highwater(1);
    }
}

/// The process's resident memory in MiB, where `/proc` gives it (Linux).
fn resident_mib() -> Option<f64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    let kib: f64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib / 1024.0)
}

/// Builds the job of `files` photos, with EXIF or without, and times the
/// review's commands on it.
fn run(files: usize, exif: bool) {
    let session = Session::start(files, exif, 0);
    let (engine, job, dirs) = (&session.engine, session.job, &session.dirs);
    let variant = if exif { "" } else { "no-exif " };
    let line = |command: &str, times: &[Duration]| {
        report(files, &format!("{variant}{command}"), times);
    };
    let brackets = files / BRACKET;

    let first = timed(|| engine.set_thresholds(job, 8, WINDOW, None).unwrap());
    line("set_thresholds 8, first grouping", &[first]);
    let groups = engine.groups(job).unwrap();
    assert_eq!(groups.len(), brackets);
    assert!(
        groups
            .iter()
            .all(|group| group.members.len() == BRACKET && group.ml_status == MlStatus::Confirmed),
        "every bracket is one group, which Stage 3 confirmed"
    );

    let mut unchanged = Vec::new();
    for _ in 0..REPEATS {
        for hamming in [9, 8] {
            unchanged.push(timed(|| {
                engine.set_thresholds(job, hamming, WINDOW, None).unwrap()
            }));
        }
    }
    line("set_thresholds 9|8, groups unchanged", &unchanged);
    assert_eq!(ids(&engine.groups(job).unwrap()), ids(&groups));

    let (mut removed, mut created) = (Vec::new(), Vec::new());
    for _ in 0..3 {
        removed.push(timed(|| {
            engine.set_thresholds(job, 1, WINDOW, None).unwrap()
        }));
        assert_eq!(engine.groups(job).unwrap().len(), 0);
        created.push(timed(|| {
            engine.set_thresholds(job, 8, WINDOW, None).unwrap()
        }));
        assert_eq!(engine.groups(job).unwrap().len(), brackets);
    }
    line("set_thresholds 1, every group removed", &removed);
    line("set_thresholds 8, every group created", &created);

    let reads: [(&str, &dyn Fn()); 5] = [
        ("groups", &|| {
            assert_eq!(engine.groups(job).unwrap().len(), brackets)
        }),
        ("files", &|| {
            assert_eq!(engine.files(job).unwrap().len(), files)
        }),
        ("unstacked", &|| {
            assert!(engine.unstacked(job).unwrap().is_empty())
        }),
        ("job", &|| drop(engine.job(job).unwrap())),
        ("jobs", &|| assert_eq!(engine.jobs().unwrap().len(), 1)),
    ];
    for (name, read) in reads {
        let times: Vec<Duration> = (0..REPEATS).map(|_| timed(read)).collect();
        line(name, &times);
    }

    // As a review screen does: each edit, then the reads that redraw it.
    let mut redrawn = Vec::new();
    for _ in 0..REPEATS {
        for hamming in [9, 8] {
            redrawn.push(timed(|| {
                engine.set_thresholds(job, hamming, WINDOW, None).unwrap()
            }));
            drop(engine.groups(job).unwrap());
            drop(engine.files(job).unwrap());
            drop(engine.unstacked(job).unwrap());
        }
    }
    line(
        "set_thresholds 9|8, each after groups, files, unstacked",
        &redrawn,
    );

    let groups = engine.groups(job).unwrap();
    let mut decisions = Vec::new();
    for _ in 0..REPEATS {
        decisions.push(timed(|| engine.confirm(job, groups[0].id).unwrap()));
        decisions.push(timed(|| engine.reject(job, groups[0].id).unwrap()));
    }
    line("confirm|reject", &decisions);
    line("confirm_all", &[timed(|| engine.confirm_all(job).unwrap())]);

    let moved = groups[0].members[0];
    let target = MoveTarget::Group(groups[1].id);
    line(
        "move_file",
        &[timed(|| engine.move_file(job, moved, target).unwrap())],
    );
    line(
        "set_thresholds 8, after the move",
        &[timed(|| {
            engine.set_thresholds(job, 8, WINDOW, None).unwrap()
        })],
    );
    assert_eq!(engine.groups(job).unwrap().len(), brackets);

    // What an edit reads, through a read-only connection of its own.
    let reader = Store::open_read_only(&dirs.db()).unwrap();
    let rows = reader.file_row_ids(job).unwrap();
    let store_reads: [(&str, &dyn Fn()); 3] = [
        ("store embeddings", &|| {
            assert_eq!(reader.embeddings(MODEL, &rows).unwrap().len(), files);
        }),
        ("store load_analysis", &|| {
            drop(reader.load_analysis(job).unwrap());
        }),
        ("store groups", &|| {
            assert_eq!(reader.groups(job).unwrap().len(), brackets);
        }),
    ];
    for (name, read) in store_reads {
        let times: Vec<Duration> = (0..REPEATS).map(|_| timed(read)).collect();
        line(name, &times);
    }
    assert_eq!(session.embedder.calls.load(Ordering::SeqCst), 0);
}
