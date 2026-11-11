//! Scenario tests: one per pipeline rule of spec §5, run end to end through
//! [`analyze`] on synthetic photos written to a temporary folder.
//!
//! The photos are testkit scenes saved as JPEGs with Canon EXIF. Shots of one
//! scenario are 1 s apart unless the rule needs otherwise. Where a scenario
//! relies on hash distances, it first asserts them as a precondition, so a
//! failure says whether the fixture or the pipeline is at fault.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use chrono::NaiveDate;
use image::RgbImage;
use photostacker_core::decode::thumbnail;
use photostacker_core::exif::BracketMode;
use photostacker_core::group::BRACKET_RUN_MAX_DISTANCE;
use photostacker_core::hash::{HashPair, distance, hash_pair, hash_pair_normalised};
use photostacker_core::model::{FileKind, RawJpegPreference, Warning};
use photostacker_core::{
    Analysis, AnalyzeError, ConfigError, JobConfig, PlanPair, PlanV1, Progress, Stage, analyze,
};
use photostacker_testkit::{
    MakerNoteSpec, NoteStyle, TiffSpec, UNKNOWN_MAKE, UNKNOWN_MODEL, Value, exif_value_field,
    expose, jpeg_with_exif, scene, shift, tiff_raw_with_jpegs,
};

const WIDTH: u32 = 640;
const HEIGHT: u32 = 426;
const JPEG_QUALITY: u8 = 92;
/// The default `hamming_threshold`.
const THRESHOLD: u32 = 8;
const BODY_A: &str = "Canon EOS 2000D";
const BODY_B: &str = "Canon EOS 6D";
/// An APP1 Exif payload whose TIFF header is broken: the JPEG around it is
/// whole, but its EXIF cannot be read.
const BROKEN_TIFF: &[u8] = b"XX\x2a\x00\x08\x00\x00\x00";
/// How a `Warning::ExifUnreadable` begins in the plan.
const UNREADABLE_EXIF_PREFIX: &str =
    "unreadable EXIF, grouped with the paired file's EXIF if any, else without: ";
/// How a `Warning::PromotedOverRaw` begins and ends in the plan.
const PROMOTED_PREFIX: &str = "the paired RAW could not be decoded (";
const PROMOTED_SUFFIX: &str = "); this file was stacked instead";

// --- Fixtures ---

/// Canon EXIF for a shot taken `second` seconds into the scenario. With a
/// `bracket_shot`, the MakerNote says AEB with that shot number.
fn canon_exif(model: &str, second: u32, bracket_shot: Option<u16>) -> TiffSpec {
    canon_exif_with_note(model, second, bracket_shot.map(|shot| (1, shot)))
}

/// Canon EXIF whose MakerNote says bracketing is off.
fn canon_exif_bracket_off(model: &str, second: u32) -> TiffSpec {
    canon_exif_with_note(model, second, Some((0, 0)))
}

/// Canon EXIF with, when `file_info` is set, a MakerNote whose FileInfo
/// holds that (BracketMode, BracketShotNumber).
fn canon_exif_with_note(model: &str, second: u32, file_info: Option<(u16, u16)>) -> TiffSpec {
    let time = format!("2026:05:01 10:{:02}:{:02}", second / 60, second % 60);
    let maker_note = file_info.map(|(mode, shot)| {
        // FileInfo: word 0 the byte count, word 3 BracketMode (0 = Off,
        // 1 = AEB), word 5 BracketShotNumber.
        let mut file_info = vec![0u16; 20];
        file_info[0] = 40;
        file_info[3] = mode;
        file_info[5] = shot;
        MakerNoteSpec {
            style: NoteStyle::CanonIfd,
            entries: vec![(0x0093, Value::Short(file_info))],
        }
    });
    TiffSpec {
        little_endian: true,
        ifd0: vec![
            (0x010F, Value::Ascii("Canon".into())),
            (0x0110, Value::Ascii(model.into())),
        ],
        exif: vec![
            (0x9003, Value::Ascii(time)),
            (0x920A, Value::Rational(vec![(35, 1)])),
        ],
        maker_note,
    }
}

/// Writes `image` as `name` (a path relative to `dir`, written with `/`) with
/// the EXIF of `exif`, or none.
fn write_jpeg(dir: &Path, name: &str, image: &RgbImage, exif: Option<&TiffSpec>) -> PathBuf {
    let path = dir.join(native(name));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let bytes = match exif {
        Some(spec) => jpeg_with_exif(image, &spec.build(), JPEG_QUALITY),
        None => plain_jpeg(image),
    };
    fs::write(&path, bytes).unwrap();
    path
}

fn plain_jpeg(image: &RgbImage) -> Vec<u8> {
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, JPEG_QUALITY)
        .encode_image(image)
        .unwrap();
    bytes
}

/// `rel` (written with `/`) as a platform-native relative path.
fn native(rel: &str) -> PathBuf {
    rel.split('/').collect()
}

fn natives(rels: &[&str]) -> Vec<PathBuf> {
    rels.iter().map(|rel| native(rel)).collect()
}

/// Writes a five-shot bracket of scene 1 (EV −2 … +2, shots 1 … 5, 1 s
/// apart) and returns its file names in shot order. The names run against
/// the shot order, so only the shot number can put the layers right.
fn bracket_of_five(dir: &Path) -> [&'static str; 5] {
    let names = [
        "IMG_0009.jpg",
        "IMG_0008.jpg",
        "IMG_0007.jpg",
        "IMG_0006.jpg",
        "IMG_0005.jpg",
    ];
    let base = scene(1, WIDTH, HEIGHT);
    for (i, (name, ev)) in names.iter().zip([-2.0, -1.0, 0.0, 1.0, 2.0]).enumerate() {
        let shot = i as u16 + 1;
        let exif = canon_exif(BODY_A, i as u32, Some(shot));
        write_jpeg(dir, name, &expose(&base, ev), Some(&exif));
    }
    names
}

/// The hashes `hasher` gives the thumbnails of `files` (relative to `dir`),
/// decoded the way the pipeline decodes them.
fn hashes_of(dir: &Path, files: &[&str], hasher: fn(&RgbImage) -> HashPair) -> Vec<HashPair> {
    files
        .iter()
        .map(|name| {
            let thumb = thumbnail(&dir.join(native(name)), FileKind::Jpeg, 1, 512)
                .unwrap()
                .image;
            hasher(&thumb)
        })
        .collect()
}

fn max_of_pairs(hashes: &[HashPair]) -> u32 {
    let mut max = 0;
    for (i, &a) in hashes.iter().enumerate() {
        for &b in &hashes[i + 1..] {
            max = max.max(distance(a, b));
        }
    }
    max
}

/// The largest distance between two of `files` (relative to `dir`) by the
/// normalised hashes, which the pipeline links on.
fn max_pairwise_distance(dir: &Path, files: &[&str]) -> u32 {
    max_of_pairs(&hashes_of(dir, files, hash_pair_normalised))
}

/// The same by the plain hashes, which grouping no longer uses.
fn max_plain_distance(dir: &Path, files: &[&str]) -> u32 {
    max_of_pairs(&hashes_of(dir, files, hash_pair))
}

/// The largest normalised distance between two consecutive `files`.
fn max_consecutive_distance(dir: &Path, files: &[&str]) -> u32 {
    let hashes = hashes_of(dir, files, hash_pair_normalised);
    hashes
        .windows(2)
        .map(|pair| distance(pair[0], pair[1]))
        .max()
        .unwrap_or(0)
}

// --- Running the pipeline ---

fn config(dir: &Path) -> JobConfig {
    JobConfig::new(dir.to_path_buf())
}

fn run_config(config: &JobConfig) -> Analysis {
    analyze(config, &|_| {}, &AtomicBool::new(false)).unwrap()
}

fn plan_of(config: &JobConfig) -> PlanV1 {
    PlanV1::from_analysis(config, &run_config(config))
}

fn plan(dir: &Path) -> PlanV1 {
    plan_of(&config(dir))
}

fn group_files(plan: &PlanV1) -> Vec<Vec<PathBuf>> {
    plan.groups.iter().map(|g| g.files.clone()).collect()
}

fn error_files(plan: &PlanV1) -> Vec<PathBuf> {
    plan.errors.iter().map(|e| e.file.clone()).collect()
}

/// The warnings of the file `rel` in `plan`, in plan order.
fn warnings_of<'a>(plan: &'a PlanV1, rel: &str) -> Vec<&'a str> {
    plan.warnings
        .iter()
        .filter(|w| w.file == native(rel))
        .map(|w| w.warning.as_str())
        .collect()
}

fn is_promoted_over_raw(warning: &str) -> bool {
    warning.starts_with(PROMOTED_PREFIX) && warning.ends_with(PROMOTED_SUFFIX)
}

fn pair(passive: &str, active: &str) -> PlanPair {
    PlanPair {
        passive: native(passive),
        active: native(active),
    }
}

fn id_of(analysis: &Analysis, dir: &Path, rel: &str) -> usize {
    let path = dir.join(native(rel));
    analysis
        .files
        .iter()
        .position(|f| f.path == path)
        .unwrap_or_else(|| panic!("{rel} was not discovered"))
}

// --- Stage 2: hashing (spec §5.3) and layer order (§5.6) ---

#[test]
fn bracket_of_five_is_one_group() {
    let dir = tempfile::tempdir().unwrap();
    let names = bracket_of_five(dir.path());

    let measured = max_pairwise_distance(dir.path(), &names);
    eprintln!("bracket_of_five: max pairwise distance {measured}");
    assert!(
        measured <= THRESHOLD,
        "precondition: the five exposures must hash within {THRESHOLD}, got {measured}; \
         adjust the testkit scene contrast, not the threshold"
    );

    let plan = plan(dir.path());
    assert_eq!(group_files(&plan), [natives(&names)]);
    assert_eq!(plan.groups[0].max_distance, measured);
    assert!(plan.unstacked.is_empty(), "{:?}", plan.unstacked);
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    // Bucket keys name the folder relative to the input: "." for its root.
    assert_eq!(plan.groups[0].bucket, ".|Canon EOS 2000D|?|35.0@0");
}

#[test]
fn reframed_shot_is_not_merged() {
    let dir = tempfile::tempdir().unwrap();
    let base = scene(1, WIDTH, HEIGHT);
    write_jpeg(
        dir.path(),
        "a.jpg",
        &base,
        Some(&canon_exif(BODY_A, 0, None)),
    );
    let reframed = shift(&base, 0.12, 0.0);
    write_jpeg(
        dir.path(),
        "b.jpg",
        &reframed,
        Some(&canon_exif(BODY_A, 1, None)),
    );

    let measured = max_pairwise_distance(dir.path(), &["a.jpg", "b.jpg"]);
    eprintln!("reframed_shot: distance {measured}");
    assert!(
        measured > THRESHOLD,
        "precondition: the reframed shot must hash farther than {THRESHOLD}, got {measured}"
    );

    let plan = plan(dir.path());
    assert!(plan.groups.is_empty(), "{:?}", plan.groups);
    assert_eq!(plan.unstacked, natives(&["a.jpg", "b.jpg"]));
}

// --- Stage 1: bucketing (spec §5.2) ---

#[test]
fn two_brackets_back_to_back() {
    let dir = tempfile::tempdir().unwrap();
    let mut expected = Vec::new();
    for (bracket, seed) in [(0u32, 1u64), (1, 2)] {
        let base = scene(seed, WIDTH, HEIGHT);
        let mut names = Vec::new();
        for (i, ev) in [-1.0, 0.0, 1.0].into_iter().enumerate() {
            let name = format!("IMG_{bracket}{i}.jpg");
            // 1 s apart throughout: only the shot-number reset separates them.
            let exif = canon_exif(BODY_A, bracket * 3 + i as u32, Some(i as u16 + 1));
            write_jpeg(dir.path(), &name, &expose(&base, ev), Some(&exif));
            names.push(native(&name));
        }
        expected.push(names);
    }

    let plan = plan(dir.path());
    assert_eq!(group_files(&plan), expected);
    // The reset rule split them in Stage 1, whatever the hashes say.
    assert_ne!(plan.groups[0].bucket, plan.groups[1].bucket);
}

#[test]
fn off_shot_does_not_hide_a_bracket_reset() {
    let dir = tempfile::tempdir().unwrap();
    let base = scene(1, WIDTH, HEIGHT);
    // One scene, 1 s apart: shots 1, 2, 3, a shot with bracketing off, then
    // 1, 2, 3 again. Only the reset after the Off shot separates the brackets.
    let shots = [Some(1), Some(2), Some(3), None, Some(1), Some(2), Some(3)];
    let evs = [-1.0, 0.0, 1.0, 0.0, -1.0, 0.0, 1.0];
    let mut names = Vec::new();
    for (i, (shot, ev)) in shots.into_iter().zip(evs).enumerate() {
        let name = format!("IMG_{i}.jpg");
        let second = i as u32;
        let exif = match shot {
            Some(_) => canon_exif(BODY_A, second, shot),
            None => canon_exif_bracket_off(BODY_A, second),
        };
        write_jpeg(dir.path(), &name, &expose(&base, ev), Some(&exif));
        names.push(name);
    }
    let all: Vec<&str> = names.iter().map(String::as_str).collect();
    let measured = max_pairwise_distance(dir.path(), &all);
    eprintln!("off_shot_reset: max pairwise distance {measured}");
    assert!(
        measured <= THRESHOLD,
        "precondition: all seven must hash within {THRESHOLD}, got {measured}"
    );

    let plan = plan(dir.path());
    // The Off shot stays with the first bracket and, unnumbered, goes last.
    assert_eq!(group_files(&plan), [natives(&all[..4]), natives(&all[4..])]);
    assert_ne!(plan.groups[0].bucket, plan.groups[1].bucket);
}

#[test]
fn interleaved_bodies_stay_separate() {
    let dir = tempfile::tempdir().unwrap();
    let base = scene(1, WIDTH, HEIGHT);
    // Two bodies shoot the same scene, alternating every second, without
    // bracket numbers: only the camera key keeps them apart.
    let mut names = Vec::new();
    for (i, ev) in [-1.0, 0.0, 1.0].into_iter().enumerate() {
        let second = 2 * i as u32;
        for (body, model, offset) in [("a", BODY_A, 0), ("b", BODY_B, 1)] {
            let name = format!("{body}{i}.jpg");
            let exif = canon_exif(model, second + offset, None);
            write_jpeg(dir.path(), &name, &expose(&base, ev), Some(&exif));
            names.push(name);
        }
    }
    let all: Vec<&str> = names.iter().map(String::as_str).collect();
    let measured = max_pairwise_distance(dir.path(), &all);
    eprintln!("interleaved_bodies: max pairwise distance {measured}");
    assert!(
        measured <= THRESHOLD,
        "precondition: all six must hash within {THRESHOLD}, got {measured}"
    );

    let plan = plan(dir.path());
    assert_eq!(
        group_files(&plan),
        [
            natives(&["a0.jpg", "a1.jpg", "a2.jpg"]),
            natives(&["b0.jpg", "b1.jpg", "b2.jpg"])
        ]
    );
}

#[test]
fn the_default_time_window_is_30_seconds() {
    // Spec §5.2: series shot 5 to 30 s apart stack by
    // default. Two shots of one scene, 20 s apart.
    let dir = tempfile::tempdir().unwrap();
    let base = scene(1, WIDTH, HEIGHT);
    write_jpeg(
        dir.path(),
        "a.jpg",
        &base,
        Some(&canon_exif(BODY_A, 0, None)),
    );
    write_jpeg(
        dir.path(),
        "b.jpg",
        &base,
        Some(&canon_exif(BODY_A, 20, None)),
    );

    assert_eq!(
        group_files(&plan(dir.path())),
        [natives(&["a.jpg", "b.jpg"])]
    );

    // At 4 s they are too far apart to be compared.
    let mut narrow = config(dir.path());
    narrow.time_window_seconds = 4;
    let plan = plan_of(&narrow);
    assert!(plan.groups.is_empty(), "{:?}", plan.groups);
    assert_eq!(plan.unstacked, natives(&["a.jpg", "b.jpg"]));
}

#[test]
fn shots_beyond_the_time_window_are_not_merged() {
    let dir = tempfile::tempdir().unwrap();
    let base = scene(1, WIDTH, HEIGHT);
    // A window of 4 s; the second shot comes 10 s later.
    write_jpeg(
        dir.path(),
        "a.jpg",
        &base,
        Some(&canon_exif(BODY_A, 0, None)),
    );
    write_jpeg(
        dir.path(),
        "b.jpg",
        &base,
        Some(&canon_exif(BODY_A, 10, None)),
    );

    let mut narrow = config(dir.path());
    narrow.time_window_seconds = 4;
    let plan = plan_of(&narrow);
    assert!(plan.groups.is_empty(), "{:?}", plan.groups);
    assert_eq!(plan.unstacked, natives(&["a.jpg", "b.jpg"]));

    // Inside the window the same two shots stack.
    let mut wide = config(dir.path());
    wide.time_window_seconds = 10;
    assert_eq!(group_files(&plan_of(&wide)), [natives(&["a.jpg", "b.jpg"])]);
}

#[test]
fn files_without_capture_time_bucket_apart_from_timed_files() {
    // a.jpg has an EXIF time (10:00:00, no UTC offset). b.jpg and c.jpg, the
    // same body and the same image, have no DateTimeOriginal, and mtimes 2 s
    // and 3 s after a's time read as UTC. The camera clock without an offset
    // and the mtime are different clocks, so b and c stack only together.
    let dir = tempfile::tempdir().unwrap();
    let base = scene(1, WIDTH, HEIGHT);
    write_jpeg(
        dir.path(),
        "a.jpg",
        &base,
        Some(&canon_exif(BODY_A, 0, None)),
    );
    let mut untimed = canon_exif(BODY_A, 0, None);
    untimed.exif.retain(|&(tag, _)| tag != 0x9003);
    for (name, second) in [("b.jpg", 2), ("c.jpg", 3)] {
        let path = write_jpeg(dir.path(), name, &base, Some(&untimed));
        let mtime = NaiveDate::from_ymd_opt(2026, 5, 1)
            .and_then(|day| day.and_hms_opt(10, 0, second))
            .unwrap()
            .and_utc();
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::from(mtime))
            .unwrap();
    }

    let plan = plan(dir.path());
    assert_eq!(group_files(&plan), [natives(&["b.jpg", "c.jpg"])]);
    assert_eq!(plan.groups[0].bucket, ".|Canon EOS 2000D|?|35.0|mtime@1");
    assert_eq!(plan.unstacked, natives(&["a.jpg"]));
    let expected_warning = Warning::NoCaptureTime.to_string();
    for name in ["b.jpg", "c.jpg"] {
        assert_eq!(warnings_of(&plan, name), [expected_warning.as_str()]);
    }
}

#[test]
fn repeated_bracket_values_split_unnumbered_brackets() {
    // Two auto-bracket runs of one scene (EXIF ExposureMode 2, the bias as
    // the bracket value, no shot numbers), 1 s apart throughout: only the
    // repeated 0 EV value separates them.
    let dir = tempfile::tempdir().unwrap();
    let base = scene(1, WIDTH, HEIGHT);
    let mut names = Vec::new();
    for (i, ev) in [0, -1, 1, 0, -1, 1].into_iter().enumerate() {
        let name = format!("IMG_{i}.jpg");
        let mut exif = canon_exif(BODY_A, i as u32, None);
        exif.exif.push((0x9204, Value::SRational(vec![(ev, 1)])));
        exif.exif.push((0xA402, Value::Short(vec![2])));
        write_jpeg(dir.path(), &name, &expose(&base, ev as f32), Some(&exif));
        names.push(name);
    }
    let all: Vec<&str> = names.iter().map(String::as_str).collect();
    let measured = max_pairwise_distance(dir.path(), &all);
    eprintln!("repeated_bracket_values: max pairwise distance {measured}");
    assert!(
        measured <= THRESHOLD,
        "precondition: all six must hash within {THRESHOLD}, got {measured}"
    );

    let plan = plan(dir.path());
    assert_eq!(group_files(&plan), [natives(&all[..3]), natives(&all[3..])]);
    assert_ne!(plan.groups[0].bucket, plan.groups[1].bucket);
}

/// Canon EXIF without a MakerNote for an auto-bracket shot (ExposureMode 2,
/// the bias as its bracket value) taken `ms` milliseconds into the scenario.
fn auto_bracket_exif(ms: u32, bias_ev: i32) -> TiffSpec {
    let mut exif = canon_exif(BODY_A, ms / 1000, None);
    exif.exif
        .push((0x9291, Value::Ascii(format!("{:03}", ms % 1000))));
    exif.exif
        .push((0x9204, Value::SRational(vec![(bias_ev, 1)])));
    exif.exif.push((0xA402, Value::Short(vec![2])));
    exif
}

/// The biases of a wide bracket: far enough apart that the +3 EV shot clips.
const WIDE_BIASES: [i32; 3] = [-3, 0, 3];

#[test]
fn wide_exposure_bracket_groups_at_default_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let base = scene(1, WIDTH, HEIGHT);
    let names = ["IMG_0.jpg", "IMG_1.jpg", "IMG_2.jpg"];
    for (i, (name, ev)) in names.iter().zip(WIDE_BIASES).enumerate() {
        let exif = auto_bracket_exif(i as u32 * 1000, ev);
        write_jpeg(dir.path(), name, &expose(&base, ev as f32), Some(&exif));
    }
    let plain = max_plain_distance(dir.path(), &names);
    let normalised = max_pairwise_distance(dir.path(), &names);
    let consecutive = max_consecutive_distance(dir.path(), &names);
    eprintln!(
        "wide_exposure_bracket: max plain {plain}, max normalised {normalised}, \
         max consecutive normalised {consecutive}"
    );
    assert!(
        plain > THRESHOLD,
        "precondition: the plain hashes must not group the bracket, got {plain}"
    );
    assert!(
        (1..=BRACKET_RUN_MAX_DISTANCE).contains(&consecutive),
        "precondition: consecutive shots must differ and pass the run check, got {consecutive}"
    );

    let plan = plan(dir.path());
    assert_eq!(group_files(&plan), [natives(&names)]);
    assert_eq!(plan.groups[0].max_distance, normalised);
    assert!(plan.unstacked.is_empty(), "{:?}", plan.unstacked);

    // The run links do not depend on the threshold: at 0, where only equal
    // hashes link, the bracket is still one stack.
    let mut strict = config(dir.path());
    strict.hamming_threshold = 0;
    assert_eq!(group_files(&plan_of(&strict)), [natives(&names)]);
}

#[test]
fn back_to_back_unnumbered_brackets_stack_separately() {
    // Two wide auto-bracket runs of one scene, 0.4 s apart throughout. In one
    // bucket, the run links would join the last shot of the first run to the
    // first of the second; only Stage 1's repeated value keeps them apart.
    let dir = tempfile::tempdir().unwrap();
    let base = scene(1, WIDTH, HEIGHT);
    let mut names = Vec::new();
    for (i, ev) in WIDE_BIASES.into_iter().chain(WIDE_BIASES).enumerate() {
        let name = format!("IMG_{i}.jpg");
        let exif = auto_bracket_exif(i as u32 * 400, ev);
        write_jpeg(dir.path(), &name, &expose(&base, ev as f32), Some(&exif));
        names.push(name);
    }
    let all: Vec<&str> = names.iter().map(String::as_str).collect();
    let consecutive = max_consecutive_distance(dir.path(), &all);
    eprintln!("back_to_back_unnumbered: max consecutive normalised {consecutive}");
    assert!(
        consecutive <= BRACKET_RUN_MAX_DISTANCE,
        "precondition: got {consecutive}"
    );

    let plan = plan(dir.path());
    assert_eq!(group_files(&plan), [natives(&all[..3]), natives(&all[3..])]);
    assert_ne!(plan.groups[0].bucket, plan.groups[1].bucket);
}

#[test]
fn valueless_back_to_back_brackets_are_not_run_linked() {
    // Two ExposureMode 2 runs without an exposure bias, so with neither a
    // shot number nor a bracket value: Stage 1 cannot split them, and they
    // share one bucket. The second run is reframed like
    // `reframed_shot_is_not_merged`, which is within the run check's 40.
    let dir = tempfile::tempdir().unwrap();
    let base = scene(1, WIDTH, HEIGHT);
    let reframed = shift(&base, 0.12, 0.0);
    let runs = [(&base, [-1.0, 1.0, 0.0]), (&reframed, [0.0, -1.0, 1.0])];
    let mut names = Vec::new();
    for (run, (image, evs)) in runs.into_iter().enumerate() {
        for (i, ev) in evs.into_iter().enumerate() {
            let shot = run * 3 + i;
            let name = format!("IMG_{shot}.jpg");
            let ms = shot as u32 * 400;
            let mut exif = canon_exif(BODY_A, ms / 1000, None);
            exif.exif
                .push((0x9291, Value::Ascii(format!("{:03}", ms % 1000))));
            exif.exif.push((0xA402, Value::Short(vec![2])));
            write_jpeg(dir.path(), &name, &expose(image, ev), Some(&exif));
            names.push(name);
        }
    }
    let all: Vec<&str> = names.iter().map(String::as_str).collect();
    let within = max_pairwise_distance(dir.path(), &all[..3])
        .max(max_pairwise_distance(dir.path(), &all[3..]));
    let seam = max_pairwise_distance(dir.path(), &all[2..4]);
    eprintln!("valueless_back_to_back: max within runs {within}, across the seam {seam}");
    assert!(within <= THRESHOLD, "precondition: got {within}");
    assert!(
        (THRESHOLD + 1..=BRACKET_RUN_MAX_DISTANCE).contains(&seam),
        "precondition: the seam must be beyond the threshold but within the run check, got {seam}"
    );

    let plan = plan(dir.path());
    assert_eq!(group_files(&plan), [natives(&all[..3]), natives(&all[3..])]);
    // Stage 1 kept them together; only the missing run link keeps them apart.
    assert_eq!(plan.groups[0].bucket, plan.groups[1].bucket);
}

#[test]
fn files_without_exif_group_by_hash_within_their_folder() {
    let dir = tempfile::tempdir().unwrap();
    let base = scene(1, WIDTH, HEIGHT);
    let names = ["left/a.jpg", "left/b.jpg", "right/c.jpg", "right/d.jpg"];
    for (name, ev) in names.iter().zip([0.0, 1.0, 0.0, 1.0]) {
        write_jpeg(dir.path(), name, &expose(&base, ev), None);
    }
    let measured = max_pairwise_distance(dir.path(), &names);
    eprintln!("files_without_exif: max pairwise distance {measured}");
    assert!(measured <= THRESHOLD, "precondition: got {measured}");

    let mut recursive = config(dir.path());
    recursive.recursive = true;
    let plan = plan_of(&recursive);
    // One no-exif bucket per folder: the same scene never stacks across folders.
    assert_eq!(
        group_files(&plan),
        [natives(&names[..2]), natives(&names[2..])]
    );
    let buckets: Vec<&str> = plan.groups.iter().map(|g| g.bucket.as_str()).collect();
    assert_eq!(buckets, ["left|no-exif", "right|no-exif"]);
    let warned: Vec<PathBuf> = plan.warnings.iter().map(|w| w.file.clone()).collect();
    assert_eq!(warned, natives(&names));
    let expected_warning = Warning::NoCaptureTime.to_string();
    assert!(plan.warnings.iter().all(|w| w.warning == expected_warning));
}

// --- Discovery (spec §5.1) ---

#[test]
fn output_folder_inside_input_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let base = scene(1, WIDTH, HEIGHT);
    for (name, second) in [("a.jpg", 0), ("b.jpg", 1), ("stacked/c.jpg", 2)] {
        write_jpeg(
            dir.path(),
            name,
            &base,
            Some(&canon_exif(BODY_A, second, None)),
        );
    }
    let mut recursive = config(dir.path());
    recursive.recursive = true;
    let analysis = run_config(&recursive);
    assert_eq!(analysis.files.len(), 2);
    let plan = PlanV1::from_analysis(&recursive, &analysis);
    assert_eq!(group_files(&plan), [natives(&["a.jpg", "b.jpg"])]);
}

#[test]
fn misnamed_still_is_grouped_by_content() {
    // The middle shot of a three-shot bracket is a JPEG named `.heic`: its
    // content, not its extension, picks the codec for its EXIF and its image.
    let dir = tempfile::tempdir().unwrap();
    let base = scene(1, WIDTH, HEIGHT);
    let names = ["IMG_0001.jpg", "IMG_0002.heic", "IMG_0003.jpg"];
    for (i, (name, ev)) in names.iter().zip([-1.0, 0.0, 1.0]).enumerate() {
        let exif = canon_exif(BODY_A, i as u32, Some(i as u16 + 1));
        write_jpeg(dir.path(), name, &expose(&base, ev), Some(&exif));
    }
    let measured = max_pairwise_distance(dir.path(), &names);
    assert!(measured <= THRESHOLD, "precondition: got {measured}");

    let config = config(dir.path());
    let analysis = run_config(&config);
    let misnamed = &analysis.files[id_of(&analysis, dir.path(), "IMG_0002.heic")];
    assert_eq!(misnamed.kind, FileKind::Jpeg);
    assert_eq!(
        misnamed.exif.as_ref().unwrap().model.as_deref(),
        Some(BODY_A)
    );

    let plan = PlanV1::from_analysis(&config, &analysis);
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    assert_eq!(group_files(&plan), [natives(&names)]);
    assert_eq!(plan.groups[0].bucket, ".|Canon EOS 2000D|?|35.0@0");
}

#[test]
fn raw_jpeg_pairs_are_not_duplicated() {
    // The RAWs here are decoys (JPEG bytes named .cr3): rawler cannot decode
    // them and they hold no embedded preview, so each active RAW gives way to
    // its JPEG, which is stacked once. Real RAW pairs are covered by the
    // fixture tests.
    let dir = tempfile::tempdir().unwrap();
    let base = scene(1, WIDTH, HEIGHT);
    for (stem, second) in [("s1", 0), ("s2", 1)] {
        let exif = canon_exif(BODY_A, second, None);
        let jpeg = write_jpeg(dir.path(), &format!("{stem}.jpg"), &base, Some(&exif));
        fs::copy(&jpeg, dir.path().join(format!("{stem}.cr3"))).unwrap();
    }

    let plan = plan(dir.path());
    assert_eq!(
        plan.paired,
        [pair("s1.cr3", "s1.jpg"), pair("s2.cr3", "s2.jpg")]
    );
    assert_eq!(group_files(&plan), [natives(&["s1.jpg", "s2.jpg"])]);
    assert!(plan.unstacked.is_empty(), "{:?}", plan.unstacked);
    // A demoted RAW is no error: the promotion warning carries its reason.
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    assert_eq!(plan.warnings.len(), 2, "{:?}", plan.warnings);
    for name in ["s1.jpg", "s2.jpg"] {
        let warnings = warnings_of(&plan, name);
        assert_eq!(warnings.len(), 1, "{name}: {warnings:?}");
        assert!(is_promoted_over_raw(warnings[0]), "{name}: {warnings:?}");
    }
}

#[test]
fn pixel_raw_and_jpeg_pair() {
    // A Google Pixel names the two files of one shot apart after `.RAW-NN`.
    // The DNG is a decoy (JPEG bytes), so it gives way to its JPEG, which is
    // examined once.
    let dir = tempfile::tempdir().unwrap();
    let jpeg_name = "PXL_20260501_100000000.RAW-01.MP.COVER.jpg";
    let raw_name = "PXL_20260501_100000000.RAW-02.ORIGINAL.dng";
    let exif = canon_exif(BODY_A, 0, None);
    let jpeg = write_jpeg(dir.path(), jpeg_name, &scene(1, WIDTH, HEIGHT), Some(&exif));
    fs::copy(&jpeg, dir.path().join(raw_name)).unwrap();

    let plan = plan(dir.path());
    assert_eq!(plan.paired, [pair(raw_name, jpeg_name)]);
    assert_eq!(plan.unstacked, natives(&[jpeg_name]));
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
}

#[test]
fn raw_without_partner_still_errors() {
    // A lone decoy RAW has no partner to give way to.
    let dir = tempfile::tempdir().unwrap();
    let decoy = plain_jpeg(&scene(1, WIDTH, HEIGHT));
    fs::write(dir.path().join("s1.cr3"), decoy).unwrap();

    let plan = plan(dir.path());
    assert_eq!(error_files(&plan), [native("s1.cr3")]);
    assert!(plan.paired.is_empty(), "{:?}", plan.paired);
    assert!(plan.groups.is_empty(), "{:?}", plan.groups);
    assert!(plan.unstacked.is_empty(), "{:?}", plan.unstacked);
}

#[test]
fn promotion_takes_the_first_partner_that_decodes() {
    // Decoy RAWs (JPEG bytes named .cr3) with several partners each, tried
    // in id order: a.heic sorts before a.jpg. A truncated JPEG cannot decode.
    let dir = tempfile::tempdir().unwrap();
    let base = scene(1, WIDTH, HEIGHT);
    let shot = |second| {
        let exif = canon_exif(BODY_A, second, None).build();
        jpeg_with_exif(&base, &exif, JPEG_QUALITY)
    };
    let (a, b, c) = (shot(0), shot(1), shot(2));
    let truncated = |bytes: &[u8]| bytes[..bytes.len() / 2].to_vec();
    let files = [
        // The HEIC (a JPEG by content) is broken, so the JPEG is promoted.
        ("a.cr3", a.clone()),
        ("a.heic", truncated(&a)),
        ("a.jpg", a),
        // Both partners decode, so the first by id, the HEIC, is promoted.
        ("b.cr3", b.clone()),
        ("b.heic", b.clone()),
        ("b.jpg", b),
        // The only partner is broken too: the RAW keeps its error, and the
        // JPEG stays its passive partner.
        ("c.cr3", c.clone()),
        ("c.jpg", truncated(&c)),
    ];
    for (name, bytes) in files {
        fs::write(dir.path().join(name), bytes).unwrap();
    }

    let config = config(dir.path());
    let analysis = run_config(&config);
    assert_eq!(
        analysis.files[id_of(&analysis, dir.path(), "b.heic")].kind,
        FileKind::Jpeg
    );
    let plan = PlanV1::from_analysis(&config, &analysis);
    assert_eq!(
        plan.paired,
        [
            pair("a.cr3", "a.jpg"),
            pair("a.heic", "a.jpg"),
            pair("b.cr3", "b.heic"),
            pair("b.jpg", "b.heic"),
            pair("c.jpg", "c.cr3"),
        ]
    );
    assert_eq!(group_files(&plan), [natives(&["a.jpg", "b.heic"])]);
    assert_eq!(error_files(&plan), [native("c.cr3")]);
    let warned: Vec<PathBuf> = plan.warnings.iter().map(|w| w.file.clone()).collect();
    assert_eq!(warned, natives(&["a.jpg", "b.heic"]));
    assert!(
        plan.warnings
            .iter()
            .all(|w| is_promoted_over_raw(&w.warning)),
        "{:?}",
        plan.warnings
    );
}

#[test]
fn passive_raw_is_never_promoted() {
    // The active a.cr3 is a decoy that fails. Of its partners, a.dng sorts
    // first and would hash (from its embedded preview), but a passive RAW is
    // never a candidate: a.jpg is promoted, and a.dng pairs with it.
    let dir = tempfile::tempdir().unwrap();
    let exif = canon_exif(BODY_A, 0, None).build();
    let jpeg = jpeg_with_exif(&scene(1, WIDTH, HEIGHT), &exif, JPEG_QUALITY);
    fs::write(dir.path().join("a.cr3"), &jpeg).unwrap();
    let dng = dir.path().join("a.dng");
    fs::write(&dng, tiff_raw_with_jpegs(&[(160, 120), (1024, 683)])).unwrap();
    fs::write(dir.path().join("a.jpg"), &jpeg).unwrap();
    assert!(
        thumbnail(&dng, FileKind::Raw, 1, 512).is_ok(),
        "precondition: examined, the DNG would hash"
    );

    let config = config(dir.path());
    let analysis = run_config(&config);
    let jpg = id_of(&analysis, dir.path(), "a.jpg");
    let dng_record = &analysis.files[id_of(&analysis, dir.path(), "a.dng")];
    assert_eq!(dng_record.kind, FileKind::Raw);
    assert_eq!(dng_record.paired_with, Some(jpg));
    assert_eq!((dng_record.hashes, &dng_record.error), (None, &None));

    let plan = PlanV1::from_analysis(&config, &analysis);
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    assert_eq!(
        plan.paired,
        [pair("a.cr3", "a.jpg"), pair("a.dng", "a.jpg")]
    );
    assert_eq!(plan.unstacked, [native("a.jpg")]);
    let warnings = warnings_of(&plan, "a.jpg");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(is_promoted_over_raw(warnings[0]), "{warnings:?}");
    assert!(
        warnings_of(&plan, "a.dng").is_empty(),
        "{:?}",
        plan.warnings
    );
}

#[test]
fn failed_active_still_promotes_nothing() {
    // With raw_jpeg_preference = jpeg the still is active. One that cannot
    // be decoded is an error, and its RAW, which would hash from its embedded
    // preview, stays passive: only a RAW gives way to its partner.
    let dir = tempfile::tempdir().unwrap();
    let raw = tiff_raw_with_jpegs(&[(160, 120), (1024, 683)]);
    let whole = plain_jpeg(&scene(1, WIDTH, HEIGHT));
    let files = [
        // A cut-off JPEG.
        ("a.jpg", whole[..whole.len() / 2].to_vec()),
        ("a.nef", raw.clone()),
        // No known signature, so it stays a HEIC, which it is not.
        ("b.heic", b"not a photo at all".to_vec()),
        ("b.nef", raw),
    ];
    for (name, bytes) in files {
        fs::write(dir.path().join(name), bytes).unwrap();
    }
    assert!(
        thumbnail(&dir.path().join("a.nef"), FileKind::Raw, 1, 512).is_ok(),
        "precondition: examined, the RAW would hash"
    );

    let mut config = config(dir.path());
    config.raw_jpeg_preference = RawJpegPreference::Jpeg;
    let analysis = run_config(&config);
    for name in ["a.nef", "b.nef"] {
        let record = &analysis.files[id_of(&analysis, dir.path(), name)];
        assert!(record.paired_with.is_some(), "{name}");
        assert_eq!((record.hashes, &record.error), (None, &None), "{name}");
    }

    let plan = PlanV1::from_analysis(&config, &analysis);
    assert_eq!(error_files(&plan), natives(&["a.jpg", "b.heic"]));
    assert_eq!(
        plan.paired,
        [pair("a.nef", "a.jpg"), pair("b.nef", "b.heic")]
    );
    assert!(plan.groups.is_empty(), "{:?}", plan.groups);
    assert!(plan.unstacked.is_empty(), "{:?}", plan.unstacked);
    // No promotion, and a failed file shows its error, not its warnings.
    assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
}

#[test]
fn preview_only_raw_is_demoted_when_a_partner_exists() {
    // rawler rejects the unknown body, so the RAW would be hashed only from
    // its embedded preview: its JPEG is stacked instead.
    let raw = tiff_raw_with_jpegs(&[(160, 120), (1024, 683)]);
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.nef"), &raw).unwrap();
    // No EXIF of its own: the RAW's fills it.
    write_jpeg(dir.path(), "a.jpg", &scene(1, WIDTH, HEIGHT), None);
    let rawler_error = thumbnail(&dir.path().join("a.nef"), FileKind::Raw, 1, 512)
        .unwrap()
        .fallback_reason
        .expect("precondition: the RAW is hashed from its embedded preview");

    let config = config(dir.path());
    let analysis = run_config(&config);
    let (nef, jpg) = (
        id_of(&analysis, dir.path(), "a.nef"),
        id_of(&analysis, dir.path(), "a.jpg"),
    );
    let (raw_record, jpeg_record) = (&analysis.files[nef], &analysis.files[jpg]);
    assert_eq!(raw_record.paired_with, Some(jpg));
    assert_eq!((raw_record.hashes, &raw_record.error), (None, &None));
    assert_eq!(
        raw_record.exif.as_ref().unwrap().model.as_deref(),
        Some(UNKNOWN_MODEL)
    );
    assert_eq!(jpeg_record.paired_with, None);
    assert!(jpeg_record.hashes.is_some());
    assert_eq!(
        jpeg_record.exif.as_ref().unwrap().model.as_deref(),
        Some(UNKNOWN_MODEL)
    );

    let promoted = PlanV1::from_analysis(&config, &analysis);
    assert!(promoted.errors.is_empty(), "{:?}", promoted.errors);
    assert_eq!(promoted.paired, [pair("a.nef", "a.jpg")]);
    assert_eq!(promoted.unstacked, [native("a.jpg")]);
    // The RAW's RawPreviewOnly is gone; neither file has a capture time.
    assert!(
        warnings_of(&promoted, "a.nef").is_empty(),
        "{:?}",
        promoted.warnings
    );
    assert_eq!(
        warnings_of(&promoted, "a.jpg"),
        [
            Warning::PromotedOverRaw {
                reason: rawler_error
            }
            .to_string(),
            Warning::NoCaptureTime.to_string()
        ]
    );

    // When the partner cannot decode either, the RAW keeps its preview.
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("b.nef"), &raw).unwrap();
    let whole = plain_jpeg(&scene(1, WIDTH, HEIGHT));
    fs::write(dir.path().join("b.jpg"), &whole[..whole.len() / 2]).unwrap();
    let plan = plan(dir.path());
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    assert_eq!(plan.paired, [pair("b.jpg", "b.nef")]);
    assert_eq!(plan.unstacked, [native("b.nef")]);
    let warnings = warnings_of(&plan, "b.nef");
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(
        warnings[0].starts_with("RAW not decodable ("),
        "{warnings:?}"
    );
    assert!(
        warnings_of(&plan, "b.jpg").is_empty(),
        "{:?}",
        plan.warnings
    );
}

#[test]
fn promoted_partner_keeps_its_own_exif_first() {
    // The RAW, an unknown body hashed only from its preview, gives way to its
    // JPEG. The JPEG's own EXIF is the base: its model stays, and the RAW's
    // EXIF fills only its gaps, here the make that the JPEG lacks.
    let dir = tempfile::tempdir().unwrap();
    let raw = tiff_raw_with_jpegs(&[(160, 120), (1024, 683)]);
    fs::write(dir.path().join("a.nef"), raw).unwrap();
    let own = TiffSpec {
        little_endian: true,
        ifd0: vec![(0x0110, Value::Ascii(BODY_A.into()))],
        exif: vec![(0x9003, Value::Ascii("2026:05:01 10:00:00".into()))],
        maker_note: None,
    };
    write_jpeg(dir.path(), "a.jpg", &scene(1, WIDTH, HEIGHT), Some(&own));

    let analysis = run_config(&config(dir.path()));
    let jpg = &analysis.files[id_of(&analysis, dir.path(), "a.jpg")];
    assert_eq!(jpg.paired_with, None, "precondition: the JPEG is promoted");
    let exif = jpg.exif.as_ref().unwrap();
    assert_eq!(exif.model.as_deref(), Some(BODY_A));
    assert_eq!(exif.make.as_deref(), Some(UNKNOWN_MAKE));
    assert!(exif.capture_time.is_some(), "{exif:?}");
    // The demoted RAW keeps its own EXIF alone.
    let nef = &analysis.files[id_of(&analysis, dir.path(), "a.nef")];
    let raw_exif = nef.exif.as_ref().unwrap();
    assert_eq!(raw_exif.model.as_deref(), Some(UNKNOWN_MODEL));
    assert_eq!(raw_exif.capture_time, None);
}

#[test]
fn preview_only_raw_without_partner_keeps_preview() {
    // rawler knows no "Photostacker Unknown Body", so both RAWs (the same
    // bytes, with no partner to give way to) are hashed from their largest
    // embedded JPEG, and say so.
    let dir = tempfile::tempdir().unwrap();
    let raw = tiff_raw_with_jpegs(&[(160, 120), (1024, 683)]);
    for name in ["a.nef", "b.nef"] {
        fs::write(dir.path().join(name), &raw).unwrap();
    }

    let config = config(dir.path());
    let analysis = run_config(&config);
    let a = &analysis.files[id_of(&analysis, dir.path(), "a.nef")];
    assert_eq!(a.kind, FileKind::Raw);
    assert_eq!(
        a.exif.as_ref().unwrap().model.as_deref(),
        Some(UNKNOWN_MODEL)
    );

    let plan = PlanV1::from_analysis(&config, &analysis);
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    assert_eq!(group_files(&plan), [natives(&["a.nef", "b.nef"])]);
    for name in ["a.nef", "b.nef"] {
        let warnings: Vec<&str> = plan
            .warnings
            .iter()
            .filter(|w| w.file == native(name))
            .map(|w| w.warning.as_str())
            .collect();
        // The RAW has no capture time either, so its mtime was used.
        assert_eq!(warnings.len(), 2, "{name}: {warnings:?}");
        assert!(
            warnings[0].starts_with("RAW not decodable (")
                && warnings[0].ends_with("); its embedded preview was used"),
            "{name}: {warnings:?}"
        );
        assert_eq!(warnings[1], Warning::NoCaptureTime.to_string());
    }
}

#[test]
fn passive_exif_fills_active_gaps() {
    let dir = tempfile::tempdir().unwrap();
    // The active JPEG has no LensModel.
    let jpeg_exif = canon_exif(BODY_A, 0, None);
    write_jpeg(
        dir.path(),
        "s1.jpg",
        &scene(1, WIDTH, HEIGHT),
        Some(&jpeg_exif),
    );
    // The passive RAW is only a TIFF block: its EXIF is read, it is never decoded.
    let raw_exif = TiffSpec {
        little_endian: true,
        ifd0: vec![
            (0x010F, Value::Ascii("Canon".into())),
            (0x0110, Value::Ascii("Some other model".into())),
        ],
        exif: vec![(0xA434, Value::Ascii("X".into()))],
        maker_note: None,
    };
    fs::write(dir.path().join("s1.cr2"), raw_exif.build()).unwrap();

    let mut config = config(dir.path());
    config.raw_jpeg_preference = RawJpegPreference::Jpeg;
    let analysis = run_config(&config);
    let active = &analysis.files[id_of(&analysis, dir.path(), "s1.jpg")];
    let exif = active.exif.as_ref().unwrap();
    assert_eq!(exif.lens_model.as_deref(), Some("X"));
    // The active file's own values win.
    assert_eq!(exif.model.as_deref(), Some(BODY_A));
    assert!(exif.capture_time.is_some());
    // The passive file keeps its own EXIF and stays out of grouping.
    let raw_id = id_of(&analysis, dir.path(), "s1.cr2");
    let passive = &analysis.files[raw_id];
    assert_eq!(
        passive.paired_with,
        Some(id_of(&analysis, dir.path(), "s1.jpg"))
    );
    assert_eq!(
        passive.exif.as_ref().unwrap().model.as_deref(),
        Some("Some other model")
    );
    assert_eq!(passive.hashes, None);
    assert!(!analysis.unstacked.contains(&raw_id));
}

#[test]
fn unreadable_passive_exif_is_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let exif = canon_exif(BODY_A, 0, None);
    write_jpeg(dir.path(), "s1.jpg", &scene(1, WIDTH, HEIGHT), Some(&exif));
    // Not a TIFF block, so its EXIF cannot be read; as a passive file it is
    // never decoded either.
    fs::write(dir.path().join("s1.cr2"), b"not a raw file at all").unwrap();

    let mut config = config(dir.path());
    config.raw_jpeg_preference = RawJpegPreference::Jpeg;
    let plan = plan_of(&config);
    assert_eq!(
        plan.paired,
        [PlanPair {
            passive: native("s1.cr2"),
            active: native("s1.jpg"),
        }]
    );
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    assert_eq!(plan.unstacked, [native("s1.jpg")]);
}

#[test]
fn passive_exif_stands_in_for_unreadable_active_exif() {
    let dir = tempfile::tempdir().unwrap();
    let base = scene(1, WIDTH, HEIGHT);
    // The active JPEG's EXIF cannot be read; its passive RAW (only a TIFF
    // block, never decoded) has the whole Canon EXIF.
    let broken = jpeg_with_exif(&base, BROKEN_TIFF, JPEG_QUALITY);
    fs::write(dir.path().join("s1.jpg"), broken).unwrap();
    fs::write(
        dir.path().join("s1.cr2"),
        canon_exif(BODY_A, 0, None).build(),
    )
    .unwrap();
    // An unpaired shot of the same camera, 1 s later.
    write_jpeg(
        dir.path(),
        "s2.jpg",
        &expose(&base, 1.0),
        Some(&canon_exif(BODY_A, 1, None)),
    );

    let mut config = config(dir.path());
    config.raw_jpeg_preference = RawJpegPreference::Jpeg;
    let analysis = run_config(&config);
    let active = &analysis.files[id_of(&analysis, dir.path(), "s1.jpg")];
    let passive = &analysis.files[id_of(&analysis, dir.path(), "s1.cr2")];
    assert!(passive.exif.is_some());
    assert_eq!(active.exif, passive.exif);
    assert!(active.hashes.is_some());

    let plan = PlanV1::from_analysis(&config, &analysis);
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    // With the stand-in EXIF, s1 buckets with s2 by camera and time, not as no-exif.
    assert_eq!(group_files(&plan), [natives(&["s1.jpg", "s2.jpg"])]);
    assert_eq!(plan.groups[0].bucket, ".|Canon EOS 2000D|?|35.0@0");
    // Only the unreadable EXIF is reported: the stand-in has a capture time.
    let warnings: Vec<(&Path, &str)> = plan
        .warnings
        .iter()
        .map(|w| (w.file.as_path(), w.warning.as_str()))
        .collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(warnings[0].0, native("s1.jpg"));
    assert!(
        warnings[0].1.starts_with(UNREADABLE_EXIF_PREFIX),
        "{warnings:?}"
    );
}

/// Canon EXIF with a LensModel, which `canon_exif` leaves out.
fn canon_exif_with_lens(model: &str, second: u32, lens: &str) -> TiffSpec {
    let mut spec = canon_exif(model, second, None);
    spec.exif.push((0xA434, Value::Ascii(lens.into())));
    spec
}

/// Writes the active `s1.jpg` (no LensModel) and its passive partners
/// `s1.cr2`, which lends nothing, and `s1.heic`, JPEG bytes whose EXIF has
/// the LensModel `X`. Returns the path of `s1.heic`.
fn pair_with_a_jpeg_named_heic(dir: &Path) -> PathBuf {
    let base = scene(1, WIDTH, HEIGHT);
    write_jpeg(dir, "s1.jpg", &base, Some(&canon_exif(BODY_A, 0, None)));
    fs::write(dir.join("s1.cr2"), b"not a raw file at all").unwrap();
    let lens = canon_exif_with_lens(BODY_A, 0, "X");
    let heic = dir.join("s1.heic");
    fs::write(&heic, jpeg_with_exif(&base, &lens.build(), JPEG_QUALITY)).unwrap();
    heic
}

#[test]
fn passive_still_is_read_as_its_sniffed_kind() {
    // s1.heic is sniffed as a JPEG, recorded so, and its EXIF is read as a
    // JPEG's, so its LensModel fills the active file's gap. Read as a HEIC,
    // it would lend nothing.
    let dir = tempfile::tempdir().unwrap();
    pair_with_a_jpeg_named_heic(dir.path());

    let mut config = config(dir.path());
    config.raw_jpeg_preference = RawJpegPreference::Jpeg;
    let analysis = run_config(&config);
    let jpg = id_of(&analysis, dir.path(), "s1.jpg");
    let passive = &analysis.files[id_of(&analysis, dir.path(), "s1.heic")];
    assert_eq!(passive.kind, FileKind::Jpeg);
    assert_eq!(passive.paired_with, Some(jpg));
    assert_eq!(passive.hashes, None);
    let passive_exif = passive.exif.as_ref().expect("read as a JPEG");
    assert_eq!(passive_exif.lens_model.as_deref(), Some("X"));
    let active_exif = analysis.files[jpg].exif.as_ref().unwrap();
    assert_eq!(active_exif.lens_model.as_deref(), Some("X"));

    let plan = PlanV1::from_analysis(&config, &analysis);
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    assert_eq!(
        plan.paired,
        [pair("s1.cr2", "s1.jpg"), pair("s1.heic", "s1.jpg")]
    );
}

#[test]
fn passive_file_that_cannot_be_sniffed_lends_nothing() {
    // The same files, but s1.heic vanishes between the scan and the sniff
    // (the discover progress report comes in between), so sniffing it is an
    // I/O error: it keeps its extension's kind, lends no EXIF, and is no
    // error.
    let dir = tempfile::tempdir().unwrap();
    let vanishing = pair_with_a_jpeg_named_heic(dir.path());

    let mut config = config(dir.path());
    config.raw_jpeg_preference = RawJpegPreference::Jpeg;
    let removed = AtomicBool::new(false);
    let progress = |progress: Progress| {
        if progress.stage == Stage::Discover {
            fs::remove_file(&vanishing).unwrap();
            removed.store(true, Ordering::Relaxed);
        }
    };
    let analysis = analyze(&config, &progress, &AtomicBool::new(false)).unwrap();
    assert!(
        removed.load(Ordering::Relaxed),
        "precondition: removed after the scan"
    );
    let jpg = id_of(&analysis, dir.path(), "s1.jpg");
    let passive = &analysis.files[id_of(&analysis, dir.path(), "s1.heic")];
    assert_eq!(passive.kind, FileKind::Heic);
    assert_eq!(passive.paired_with, Some(jpg));
    assert_eq!((&passive.exif, &passive.error), (&None, &None));
    let active_exif = analysis.files[jpg].exif.as_ref().unwrap();
    assert_eq!(active_exif.lens_model, None);

    let plan = PlanV1::from_analysis(&config, &analysis);
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    assert_eq!(plan.unstacked, [native("s1.jpg")]);
}

// --- Failures, cancellation, progress ---

#[test]
fn unreadable_exif_is_a_warning_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let base = scene(1, WIDTH, HEIGHT);
    let broken = jpeg_with_exif(&base, BROKEN_TIFF, JPEG_QUALITY);
    fs::write(dir.path().join("broken.jpg"), broken).unwrap();
    write_jpeg(dir.path(), "plain.jpg", &expose(&base, 1.0), None);

    let config = config(dir.path());
    let analysis = run_config(&config);
    let id = id_of(&analysis, dir.path(), "broken.jpg");
    assert_eq!(analysis.files[id].exif, None);
    assert!(analysis.files[id].hashes.is_some());

    let plan = PlanV1::from_analysis(&config, &analysis);
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    // Without EXIF both share their folder's no-exif bucket, and stack.
    assert_eq!(group_files(&plan), [natives(&["broken.jpg", "plain.jpg"])]);
    let broken_warnings: Vec<&str> = plan
        .warnings
        .iter()
        .filter(|w| w.file == native("broken.jpg"))
        .map(|w| w.warning.as_str())
        .collect();
    assert_eq!(broken_warnings.len(), 2, "{broken_warnings:?}");
    assert!(
        broken_warnings[0].starts_with(UNREADABLE_EXIF_PREFIX),
        "{broken_warnings:?}"
    );
    assert_eq!(broken_warnings[1], Warning::NoCaptureTime.to_string());
}

#[test]
fn one_unreadable_exif_tag_costs_only_its_field() {
    let dir = tempfile::tempdir().unwrap();
    let base = scene(1, WIDTH, HEIGHT);
    write_jpeg(
        dir.path(),
        "a.jpg",
        &base,
        Some(&canon_exif(BODY_A, 0, None)),
    );
    let mut spec = canon_exif(BODY_A, 1, None);
    spec.exif.push((0x829D, Value::Rational(vec![(8, 1)])));
    let mut block = spec.build();
    // FNumber, an out-of-line RATIONAL, points past the end of the block.
    let field = exif_value_field(&block, 0x829D);
    block[field..field + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    let broken = jpeg_with_exif(&expose(&base, 1.0), &block, JPEG_QUALITY);
    fs::write(dir.path().join("b.jpg"), broken).unwrap();

    let config = config(dir.path());
    let analysis = run_config(&config);
    let id = id_of(&analysis, dir.path(), "b.jpg");
    let exif = analysis.files[id]
        .exif
        .as_ref()
        .expect("the other tags read");
    assert_eq!(exif.model.as_deref(), Some(BODY_A));
    assert!(exif.capture_time.is_some());
    assert_eq!(exif.f_number, None);

    let plan = PlanV1::from_analysis(&config, &analysis);
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    // Both keep their camera key and capture time, so they share a bucket.
    assert_eq!(group_files(&plan), [natives(&["a.jpg", "b.jpg"])]);
}

#[test]
fn corrupt_file_is_reported_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    bracket_of_five(dir.path());
    let before = plan(dir.path());
    assert_eq!(before.groups.len(), 1);

    let whole = plain_jpeg(&scene(3, WIDTH, HEIGHT));
    fs::write(dir.path().join("bad.jpg"), &whole[..whole.len() / 2]).unwrap();
    let after = plan(dir.path());
    assert_eq!(after.groups, before.groups);
    assert_eq!(after.unstacked, before.unstacked);
    assert_eq!(error_files(&after), [native("bad.jpg")]);
    assert!(!after.errors[0].error.is_empty());
}

#[test]
fn cancel_stops_analysis() {
    let dir = tempfile::tempdir().unwrap();
    bracket_of_five(dir.path());
    let cancel = AtomicBool::new(false);
    let on_progress = |p: Progress| {
        if p.stage == Stage::Hash {
            cancel.store(true, Ordering::Relaxed);
        }
    };
    let result = analyze(&config(dir.path()), &on_progress, &cancel);
    assert!(matches!(result, Err(AnalyzeError::Cancelled)), "{result:?}");

    // A cancel during the promotion pass stops the analysis too: s1.jpg is
    // only examined there, as the partner of its decoy RAW.
    let dir = tempfile::tempdir().unwrap();
    let jpeg = write_jpeg(dir.path(), "s1.jpg", &scene(1, WIDTH, HEIGHT), None);
    fs::copy(&jpeg, dir.path().join("s1.cr3")).unwrap();
    let cancel = AtomicBool::new(false);
    let on_progress = |p: Progress| {
        if p.current.as_deref() == Some(jpeg.as_path()) && p.stage == Stage::Hash {
            cancel.store(true, Ordering::Relaxed);
        }
    };
    let result = analyze(&config(dir.path()), &on_progress, &cancel);
    assert!(matches!(result, Err(AnalyzeError::Cancelled)), "{result:?}");
}

#[test]
fn invalid_config_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path());
    config.hamming_threshold = 41;
    let result = analyze(&config, &|_| {}, &AtomicBool::new(false));
    assert!(
        matches!(
            result,
            Err(AnalyzeError::Config(ConfigError::OutOfRange { .. }))
        ),
        "{result:?}"
    );
}

#[test]
fn progress_reports_every_stage() {
    let dir = tempfile::tempdir().unwrap();
    let base = scene(1, WIDTH, HEIGHT);
    for (name, second) in [("s1.jpg", 0), ("s2.jpg", 1)] {
        write_jpeg(
            dir.path(),
            name,
            &base,
            Some(&canon_exif(BODY_A, second, None)),
        );
    }
    // Only a TIFF block: as the active RAW it cannot decode, so its JPEG is
    // examined in the promotion pass.
    let raw = canon_exif(BODY_A, 0, None).build();
    fs::write(dir.path().join("s1.cr2"), raw).unwrap();

    let reports = Mutex::new(Vec::new());
    let on_progress = |p: Progress| reports.lock().unwrap().push(p);
    analyze(&config(dir.path()), &on_progress, &AtomicBool::new(false)).unwrap();
    let reports = reports.into_inner().unwrap();

    let stages: Vec<Stage> = reports.iter().map(|p| p.stage).collect();
    assert_eq!(
        stages,
        [
            Stage::Discover,
            Stage::Metadata,
            Stage::Hash,
            Stage::Hash,
            Stage::Hash,
            Stage::Group
        ]
    );
    assert_eq!((reports[0].done, reports[0].total), (3, 3));
    assert_eq!(reports[1].current, Some(dir.path().join("s1.jpg")));
    assert_eq!((reports[1].done, reports[1].total), (1, 1));
    // The callback runs unlocked on the workers: each `done` comes once, in
    // any order.
    let mut hashed: Vec<(usize, usize)> = reports[2..4].iter().map(|p| (p.done, p.total)).collect();
    hashed.sort_unstable();
    assert_eq!(hashed, [(1, 2), (2, 2)]);
    let mut current: Vec<PathBuf> = reports[2..4]
        .iter()
        .map(|p| p.current.clone().unwrap())
        .collect();
    current.sort();
    assert_eq!(
        current,
        [dir.path().join("s1.cr2"), dir.path().join("s2.jpg")]
    );
    // The promotion pass comes after them: it grows the Hash total and goes
    // on counting, so no `done` repeats within the stage.
    assert_eq!((reports[4].done, reports[4].total), (3, 3));
    assert_eq!(reports[4].current, Some(dir.path().join("s1.jpg")));
    assert_eq!((reports[5].done, reports[5].total), (2, 2));
}

#[test]
fn analysis_is_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    bracket_of_five(dir.path());
    write_jpeg(dir.path(), "z.jpg", &scene(2, WIDTH, HEIGHT), None);
    // Two promotions in the second pass: the JPEG of a decoy RAW (JPEG bytes
    // named .cr3), and that of a RAW hashed only from its preview.
    let exif = canon_exif(BODY_A, 30, None);
    let decoy = write_jpeg(
        dir.path(),
        "pair.jpg",
        &scene(3, WIDTH, HEIGHT),
        Some(&exif),
    );
    fs::copy(&decoy, dir.path().join("pair.cr3")).unwrap();
    let raw = tiff_raw_with_jpegs(&[(160, 120), (1024, 683)]);
    fs::write(dir.path().join("preview.nef"), raw).unwrap();
    write_jpeg(dir.path(), "preview.jpg", &scene(4, WIDTH, HEIGHT), None);
    let config = config(dir.path());
    let first = run_config(&config);
    let promoted = first
        .warnings
        .iter()
        .filter(|(_, w)| matches!(w, Warning::PromotedOverRaw { .. }))
        .count();
    assert_eq!(promoted, 2, "precondition: {:?}", first.warnings);
    // Promoted, demoted and passive files alike: the normalised hashes are
    // there exactly when the plain ones are.
    for file in &first.files {
        assert_eq!(
            file.normalised.is_some(),
            file.hashes.is_some(),
            "{}",
            file.path.display()
        );
    }
    for _ in 0..3 {
        assert_eq!(run_config(&config), first);
    }
}

// --- Plan file ---

#[test]
fn plan_json_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    bracket_of_five(dir.path());
    // A decoy RAW pair whose JPEG, without EXIF, is promoted (paired,
    // unstacked, warnings) and a lone decoy RAW (errors), so every list of
    // the plan has an entry.
    let jpeg = write_jpeg(dir.path(), "pair.jpg", &scene(2, WIDTH, HEIGHT), None);
    fs::copy(&jpeg, dir.path().join("pair.cr3")).unwrap();
    fs::copy(&jpeg, dir.path().join("lone.cr3")).unwrap();

    let plan = plan(dir.path());
    assert_eq!(plan.version, 1);
    assert_eq!(plan.input_dir, dir.path());
    assert_eq!(plan.config, config(dir.path()));
    assert_eq!(plan.groups.len(), 1);
    assert_eq!(plan.unstacked, [native("pair.jpg")]);
    assert_eq!(plan.paired, [pair("pair.cr3", "pair.jpg")]);
    assert_eq!(error_files(&plan), [native("lone.cr3")]);
    // PromotedOverRaw, then NoCaptureTime, each with its stable code.
    let codes: Vec<&str> = plan.warnings.iter().map(|w| w.code.as_str()).collect();
    assert_eq!(codes, ["promoted_over_raw", "no_capture_time"], "{codes:?}");

    let json = serde_json::to_string_pretty(&plan).unwrap();
    assert!(json.contains(r#""version": 1"#), "{json}");
    let back: PlanV1 = serde_json::from_str(&json).unwrap();
    assert_eq!(back, plan);
}

// --- Public bracket sets (spec §5.3, docs/calibration.md) ---

/// The folders `<fixtures>/brackets/<vendor>/<set>`, sorted.
fn public_bracket_sets(root: &Path) -> Vec<PathBuf> {
    let subfolders = |dir: &Path| -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_dir())
            .collect();
        dirs.sort();
        dirs
    };
    subfolders(&root.join("brackets"))
        .iter()
        .flat_map(|vendor| subfolders(vendor))
        .collect()
}

#[test]
fn public_brackets_group_at_the_default_threshold() {
    let Some(root) = std::env::var_os("PHOTOSTACKER_FIXTURES") else {
        println!("SKIPPED: PHOTOSTACKER_FIXTURES not set");
        return;
    };
    let root = PathBuf::from(root);
    let sets = public_bracket_sets(&root);
    assert!(!sets.is_empty(), "no sets under <fixtures>/brackets");
    let mut not_exposure = Vec::new();
    let mut by_set: Vec<Vec<HashPair>> = Vec::new();
    for set in &sets {
        let label = set.strip_prefix(root.join("brackets")).unwrap().display();
        let analysis = run_config(&config(set));
        let errors: Vec<_> = analysis
            .files
            .iter()
            .filter_map(|f| f.error.as_ref())
            .collect();
        assert!(errors.is_empty(), "{label}: {errors:?}");
        // In capture order, which is the order the bracket runs link in.
        let mut ids: Vec<usize> = (0..analysis.files.len())
            .filter(|&id| analysis.files[id].hashes.is_some())
            .collect();
        ids.sort_by_key(|&id| {
            let file = &analysis.files[id];
            (file.exif.as_ref().and_then(|e| e.capture_time), &file.path)
        });
        let plain: Vec<HashPair> = ids
            .iter()
            .filter_map(|&id| analysis.files[id].hashes)
            .collect();
        let normalised: Vec<HashPair> = ids
            .iter()
            .filter_map(|&id| analysis.files[id].normalised)
            .collect();
        let consecutive = normalised
            .windows(2)
            .map(|pair| distance(pair[0], pair[1]))
            .max()
            .unwrap_or(0);
        println!(
            "{label}: {} files, plain max {}, normalised max {}, normalised consecutive max {consecutive}, \
             {} group(s)",
            ids.len(),
            max_of_pairs(&plain),
            max_of_pairs(&normalised),
            analysis.groups.len()
        );
        by_set.push(normalised);

        let exposure = !ids.is_empty()
            && ids.iter().all(|&id| {
                analysis.files[id]
                    .exif
                    .as_ref()
                    .and_then(|e| e.bracket)
                    .is_some_and(|b| b.mode == BracketMode::Exposure)
            });
        if !exposure {
            not_exposure.push(label.to_string());
            continue;
        }
        assert_eq!(
            ids.len(),
            analysis.files.len(),
            "{label}: every file is hashed"
        );
        assert_eq!(analysis.groups.len(), 1, "{label}: {:?}", analysis.groups);
        let mut members = analysis.groups[0].members.clone();
        members.sort_unstable();
        let mut all = ids.clone();
        all.sort_unstable();
        assert_eq!(members, all, "{label}");
    }
    println!("not read as exposure brackets, so not checked: {not_exposure:?}");

    // Different scenes must stay beyond the run check's reach.
    let across = by_set
        .iter()
        .enumerate()
        .flat_map(|(i, a)| by_set[i + 1..].iter().map(move |b| (a, b)))
        .flat_map(|(a, b)| {
            a.iter()
                .flat_map(|&x| b.iter().map(move |&y| distance(x, y)))
        })
        .min();
    println!("normalised min across sets: {across:?}");
    if let Some(across) = across {
        assert!(across > BRACKET_RUN_MAX_DISTANCE, "{across}");
    }
}
