//! The Stage-3 cosine calibration (spec §12.4). Its numbers are in
//! `docs/calibration.md` § "Stage 3"; `just calibrate-ml` runs it.
//!
//! Usage: `calibrate_ml <model-id> <sets-root> [<distractors-root>]
//! [--check-groups <folder>]... [--window <seconds>]
//! [--check-series <folder>]...`
//!
//! The model is read from `PHOTOSTACKER_MODELS_DIR`, as
//! `<dir>/<model-id>/model.onnx` (the `models/` folder of the data folder
//! that `photostacker-cli models download` installed it in), and ONNX Runtime
//! from `ORT_DYLIB_PATH`, which the justfile sets.
//!
//! Every folder is read in place, its top level only, as a job reads it:
//! `analyze` with the default config gives its active photos that hashed,
//! and each is embedded from its 512 px upright thumbnail, as Stage 3 embeds
//! it. Nothing is written.
//!
//! 1. **Labelled pairs choose the threshold.** Every folder under
//!    `<sets-root>`, at any depth, that holds photos is one true set (one
//!    bracket). The positives are the pairs within a set. The negatives are
//!    the pairs across two sets and, with `<distractors-root>` (one folder of
//!    near-duplicates of other frames, which must not stack), every pair
//!    with one of its photos. The suggested threshold is the midpoint of the
//!    gap between the smallest positive and the largest negative, or the
//!    smallest positive when they overlap (see [`suggested_threshold`]).
//! 2. **Sanity checks only report.** They print aggregate numbers and never
//!    a file name, so they can run on private folders:
//!    - `--check-groups <folder>`: the folder's Stage-2 groups at the
//!      default thresholds, with the time window `--window` (default 30 s, a new job's),
//!      the cosines of the pairs inside them, and how many of them each of
//!      a few thresholds (the suggested one, the model's registry default,
//!      [`CHECK_COSINES`]) splits when Stage 3 runs as the engine runs it
//!      (`group::regroup`: the floor of 6, bracket runs as fixed edges).
//!    - `--check-series <folder>`: the cosines of every pair of the
//!      folder's photos, for one scene shot several times.

use std::collections::HashSet;
use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, bail, ensure};
use image::RgbImage;
use photostacker_core::analyze::THUMBNAIL_MAX_SIDE;
use photostacker_core::decode::thumbnail;
use photostacker_core::group::{FileFacts, RegroupInput, Thresholds, regroup};
use photostacker_core::hash::HashPair;
use photostacker_core::models::embed::Embedder;
use photostacker_core::models::{ModelSpec, model, model_file};
use photostacker_core::{Analysis, JobConfig, analyze};

const USAGE: &str = "usage: calibrate_ml <model-id> <sets-root> [<distractors-root>] \
                     [--check-groups <folder>]... [--window <seconds>] \
                     [--check-series <folder>]...";

/// The range of `JobConfig::cosine_threshold`.
const COSINE_RANGE: (f32, f32) = (0.5, 0.99);

/// The thresholds `--check-groups` applies besides the suggested one and
/// the model's registry default.
const CHECK_COSINES: [f32; 5] = [0.6, 0.7, 0.8, 0.85, 0.9];

static NEVER: AtomicBool = AtomicBool::new(false);

#[derive(Debug, PartialEq)]
struct Args {
    model: &'static ModelSpec,
    sets_root: PathBuf,
    distractors: Option<PathBuf>,
    check_groups: Vec<PathBuf>,
    /// The Stage-1 time window of the `--check-groups` folders.
    window: u32,
    check_series: Vec<PathBuf>,
}

fn main() -> Result<()> {
    let args = parse_args(env::args_os().skip(1))?;
    let spec = args.model;
    let models_dir = env::var_os("PHOTOSTACKER_MODELS_DIR")
        .context("set PHOTOSTACKER_MODELS_DIR to the folder that holds <model-id>/model.onnx")?;
    let path = model_file(Path::new(&models_dir), spec);
    let embedder = Embedder::load(spec, &path)
        .with_context(|| format!("cannot load {} from {}", spec.id, path.display()))?;
    println!(
        "model {} ({} dims), registry default cosine {:.2}",
        spec.id, spec.dims, spec.default_cosine
    );

    let threshold = calibrate(&embedder, spec, &args)?;
    let mut cosines: Vec<f32> = [threshold, spec.default_cosine]
        .into_iter()
        .chain(CHECK_COSINES)
        .collect();
    cosines.sort_by(f32::total_cmp);
    cosines.dedup();
    for (index, folder) in args.check_groups.iter().enumerate() {
        check_groups(&embedder, folder, &cosines, args.window)
            .with_context(|| format!("check-groups #{}", index + 1))?;
    }
    for (index, folder) in args.check_series.iter().enumerate() {
        check_series(&embedder, folder).with_context(|| format!("check-series #{}", index + 1))?;
    }
    Ok(())
}

fn parse_args(mut args: impl Iterator<Item = OsString>) -> Result<Args> {
    let mut positional = Vec::new();
    let mut check_groups = Vec::new();
    let mut check_series = Vec::new();
    let mut window = JobConfig::new(PathBuf::new()).time_window_seconds;
    while let Some(arg) = args.next() {
        let flag = arg.to_str();
        if !matches!(flag, Some("--check-groups" | "--check-series" | "--window")) {
            positional.push(arg);
            continue;
        }
        let value = args
            .next()
            .with_context(|| format!("{} needs a value\n{USAGE}", arg.to_string_lossy()))?;
        match flag {
            Some("--check-groups") => check_groups.push(PathBuf::from(value)),
            Some("--check-series") => check_series.push(PathBuf::from(value)),
            _ => {
                window = value
                    .to_str()
                    .and_then(|seconds| seconds.parse().ok())
                    .with_context(|| format!("--window needs whole seconds\n{USAGE}"))?;
            }
        }
    }
    let (id, sets_root, distractors) = match positional.as_slice() {
        [id, sets] => (id, sets, None),
        [id, sets, distractors] => (id, sets, Some(PathBuf::from(distractors))),
        _ => bail!(USAGE),
    };
    let id = id.to_string_lossy();
    let model = model(&id).with_context(|| format!("no model {id} in the registry"))?;
    Ok(Args {
        model,
        sets_root: PathBuf::from(sets_root),
        distractors,
        check_groups,
        window,
        check_series,
    })
}

/// The labelled part: prints the numbers of the sets (and distractors) and
/// returns the suggested threshold.
fn calibrate(embedder: &Embedder, spec: &ModelSpec, args: &Args) -> Result<f32> {
    let mut sets = Vec::new();
    for folder in folders(&args.sets_root)? {
        let embeddings = embed_folder(embedder, &JobConfig::new(folder.clone()))?.hashed();
        if embeddings.is_empty() {
            continue;
        }
        let name = folder
            .strip_prefix(&args.sets_root)
            .unwrap_or(&folder)
            .display()
            .to_string();
        ensure!(embeddings.len() >= 2, "set {name} has a single photo");
        sets.push((name, embeddings));
    }
    ensure!(
        sets.len() >= 2,
        "{} holds {} sets; at least two are needed",
        args.sets_root.display(),
        sets.len()
    );

    let mut within = Vec::new();
    for (name, embeddings) in &sets {
        let pairs = pairs_within(embeddings);
        println!(
            "set {name}: {} photos; {}",
            embeddings.len(),
            spread_text(&pairs)
        );
        within.extend(pairs);
    }
    println!("within sets: {}", spread_text(&within));

    let mut cross = Vec::new();
    let mut closest: Option<(f32, &str, &str)> = None;
    for (i, (a_name, a)) in sets.iter().enumerate() {
        for (b_name, b) in &sets[i + 1..] {
            let pairs = pairs_across(a, b);
            let max = pairs.iter().copied().fold(f32::MIN, f32::max);
            if closest.is_none_or(|(value, ..)| max > value) {
                closest = Some((max, a_name, b_name));
            }
            cross.extend(pairs);
        }
    }
    println!("across sets: {}", spread_text(&cross));
    if let Some((value, a, b)) = closest {
        println!("  closest sets: {a} and {b} ({value:.4})");
    }

    let mut negatives = cross;
    if let Some(folder) = &args.distractors {
        let distractors = embed_folder(embedder, &JobConfig::new(folder.clone()))?.hashed();
        let mut pairs = pairs_within(&distractors);
        for (_, set) in &sets {
            pairs.extend(pairs_across(&distractors, set));
        }
        println!(
            "distractors: {} photos; {}",
            distractors.len(),
            spread_text(&pairs)
        );
        negatives.extend(pairs);
    }

    let within_min = spread(&within).expect("two sets of two photos").min;
    let negative_max = spread(&negatives).expect("two sets").max;
    let gap = within_min - negative_max;
    if gap > 0.0 {
        println!("gap: {gap:.4} (within-set min {within_min:.4}, negative max {negative_max:.4})");
    } else {
        println!(
            "overlap: {:.4} (within-set min {within_min:.4}, negative max {negative_max:.4})",
            -gap
        );
    }
    let threshold = suggested_threshold(within_min, negative_max);
    for (label, cosine) in [
        ("suggested threshold", threshold),
        ("registry default", spec.default_cosine),
    ] {
        let split = within.iter().filter(|&&value| value < cosine).count();
        let kept = negatives.iter().filter(|&&value| value >= cosine).count();
        println!(
            "{label} {cosine:.2}: {split} of {} within-set pairs below it, \
             {kept} of {} negative pairs at or above it",
            within.len(),
            negatives.len()
        );
    }
    Ok(threshold)
}

/// The sanity check of a folder of real shots, stacks and neighbours mixed:
/// what each of `cosines` does to its Stage-2 groups.
fn check_groups(embedder: &Embedder, folder: &Path, cosines: &[f32], window: u32) -> Result<()> {
    let config = JobConfig {
        time_window_seconds: window,
        ..JobConfig::new(folder.to_path_buf())
    };
    let embedded = embed_folder(embedder, &config)?;
    let analysis = &embedded.analysis;
    let floor = config.stage3_ambiguity_floor;
    let groups = &analysis.groups;
    let ambiguous = groups
        .iter()
        .filter(|group| group.max_distance > floor)
        .count();
    let mut inside = Vec::new();
    let mut inside_ambiguous = Vec::new();
    // The smallest cosine inside each group.
    let mut group_mins = Vec::new();
    for group in groups {
        let members: Vec<Vec<f32>> = group
            .members
            .iter()
            .map(|&id| embedded.embeddings[id].clone().expect("a member hashed"))
            .collect();
        let pairs = pairs_within(&members);
        group_mins.push(pairs.iter().copied().fold(f32::MAX, f32::min));
        if group.max_distance > floor {
            inside_ambiguous.extend(&pairs);
        }
        inside.extend(pairs);
    }
    println!(
        "check-groups (window {window} s): {} photos hashed, {} Stage-2 groups ({} photos), \
         {ambiguous} above the floor of {floor}",
        embedded.hashed().len(),
        groups.len(),
        groups
            .iter()
            .map(|group| group.members.len())
            .sum::<usize>(),
    );
    println!("  inside the groups: {}", spread_text(&inside));
    println!(
        "  inside the groups above the floor: {}",
        spread_text(&inside_ambiguous)
    );

    // Stage 3 as the engine runs it: the same facts as the analysis, the
    // embeddings of every hashed photo.
    let facts: Vec<FileFacts> = analysis
        .files
        .iter()
        .map(|file| FileFacts {
            path: file
                .path
                .strip_prefix(folder)
                .unwrap_or(&file.path)
                .to_path_buf(),
            exif: file.exif.clone(),
            mtime_ms: file.mtime_ms,
        })
        .collect();
    let hashes: Vec<Option<HashPair>> = analysis.files.iter().map(|file| file.normalised).collect();
    let embedding = |id: usize| embedded.embeddings[id].as_deref();
    for &cosine in cosines {
        let regrouped = regroup(&RegroupInput {
            facts: &facts,
            hashes: &hashes,
            thresholds: Thresholds {
                hamming: config.hamming_threshold,
                floor,
                cosine,
                time_window_seconds: config.time_window_seconds,
            },
            ml: Some(&embedding),
            manual: &[],
            pinned: &[],
            existing: &[],
        });
        // Stage 3 only splits, so a Stage-2 group it leaves whole is still
        // there with the same members.
        let kept: HashSet<Vec<usize>> = regrouped
            .groups
            .iter()
            .map(|group| sorted(&group.members))
            .collect();
        let split = groups
            .iter()
            .filter(|group| !kept.contains(&sorted(&group.members)))
            .count();
        let to_tray = regrouped.unstacked.len() - analysis.unstacked.len();
        let with_low_pair = group_mins.iter().filter(|&&min| min < cosine).count();
        println!(
            "  at {cosine:.2}: {with_low_pair} groups have a pair below it; Stage 3 splits \
             {split} of {} groups (floor {floor}, bracket runs fixed), {to_tray} photos to the tray",
            groups.len()
        );
    }
    Ok(())
}

/// The sanity check of one scene shot several times.
fn check_series(embedder: &Embedder, folder: &Path) -> Result<()> {
    let embeddings = embed_folder(embedder, &JobConfig::new(folder.to_path_buf()))?.hashed();
    println!(
        "check-series: {} photos; {}",
        embeddings.len(),
        spread_text(&pairs_within(&embeddings))
    );
    Ok(())
}

/// A folder's analysis and the embeddings of its photos that hashed.
struct Embedded {
    analysis: Analysis,
    /// By file id: `Some` exactly for the files that hashed.
    embeddings: Vec<Option<Vec<f32>>>,
}

impl Embedded {
    /// The embeddings of the photos that hashed, by file id.
    fn hashed(&self) -> Vec<Vec<f32>> {
        self.embeddings.iter().flatten().cloned().collect()
    }
}

/// Analyses the folder of `config` and embeds each photo that hashed from
/// its upright 512 px thumbnail.
fn embed_folder(embedder: &Embedder, config: &JobConfig) -> Result<Embedded> {
    let analysis = analyze(config, &|_| {}, &NEVER)
        .with_context(|| format!("cannot analyse {}", config.input_dir.display()))?;
    let hashed: Vec<usize> = (0..analysis.files.len())
        .filter(|&id| analysis.files[id].normalised.is_some())
        .collect();
    let images = hashed
        .iter()
        .map(|&id| {
            let file = &analysis.files[id];
            let orientation = file.exif.as_ref().map_or(1, |exif| exif.orientation);
            thumbnail(&file.path, file.kind, orientation, THUMBNAIL_MAX_SIDE)
                .map(|thumb| thumb.image)
                .with_context(|| format!("cannot decode {}", file.path.display()))
        })
        .collect::<Result<Vec<RgbImage>>>()?;
    let mut embeddings = vec![None; analysis.files.len()];
    for (id, embedding) in hashed.into_iter().zip(embedder.embed(&images)?) {
        embeddings[id] = Some(embedding);
    }
    Ok(Embedded {
        analysis,
        embeddings,
    })
}

/// `root` and every folder under it, sorted; hidden ones are left out.
fn folders(root: &Path) -> Result<Vec<PathBuf>> {
    let mut found = vec![root.to_path_buf()];
    let mut next = 0;
    while let Some(folder) = found.get(next).cloned() {
        for entry in
            fs::read_dir(&folder).with_context(|| format!("cannot list {}", folder.display()))?
        {
            let entry = entry?;
            let hidden = entry.file_name().to_string_lossy().starts_with('.');
            if entry.file_type()?.is_dir() && !hidden {
                found.push(entry.path());
            }
        }
        next += 1;
    }
    found.sort();
    Ok(found)
}

fn pairs_within(embeddings: &[Vec<f32>]) -> Vec<f32> {
    let mut pairs = Vec::new();
    for (i, a) in embeddings.iter().enumerate() {
        for b in &embeddings[i + 1..] {
            pairs.push(cosine(a, b));
        }
    }
    pairs
}

fn pairs_across(a: &[Vec<f32>], b: &[Vec<f32>]) -> Vec<f32> {
    a.iter()
        .flat_map(|x| b.iter().map(move |y| cosine(x, y)))
        .collect()
}

/// The cosine similarity of two embeddings, as `group::regroup` computes it.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut a_norm, mut b_norm) = (0.0_f64, 0.0_f64, 0.0_f64);
    for (&x, &y) in a.iter().zip(b) {
        let (x, y) = (f64::from(x), f64::from(y));
        dot += x * y;
        a_norm += x * x;
        b_norm += y * y;
    }
    let norms = (a_norm * b_norm).sqrt();
    if norms > 0.0 {
        (dot / norms) as f32
    } else {
        0.0
    }
}

fn sorted(members: &[usize]) -> Vec<usize> {
    let mut members = members.to_vec();
    members.sort_unstable();
    members
}

/// The smallest, middle and largest of some cosines.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Spread {
    pairs: usize,
    min: f32,
    median: f32,
    max: f32,
}

impl fmt::Display for Spread {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} pairs, min {:.4}, median {:.4}, max {:.4}",
            self.pairs, self.min, self.median, self.max
        )
    }
}

/// The spread of `values`; `None` when there are none. The median of an
/// even count is the mean of the two middle values.
fn spread(values: &[f32]) -> Option<Spread> {
    let mut sorted = values.to_vec();
    sorted.sort_by(f32::total_cmp);
    let (&min, &max) = (sorted.first()?, sorted.last()?);
    let middle = sorted.len() / 2;
    let median = if sorted.len() % 2 == 1 {
        sorted[middle]
    } else {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    };
    Some(Spread {
        pairs: sorted.len(),
        min,
        median,
        max,
    })
}

fn spread_text(values: &[f32]) -> String {
    spread(values).map_or_else(|| "no pairs".to_owned(), |spread| spread.to_string())
}

/// The threshold the labelled pairs suggest: the midpoint of the gap
/// between the smallest positive `within_min` and the largest negative
/// `negative_max`, or `within_min` when they overlap. It is rounded to
/// 0.01, but never above `within_min`, so it keeps every positive pair;
/// then it is kept within 0.5–0.99, the range a job accepts.
fn suggested_threshold(within_min: f32, negative_max: f32) -> f32 {
    let raw = if within_min > negative_max {
        (within_min + negative_max) / 2.0
    } else {
        within_min
    };
    let mut threshold = (raw * 100.0).round() / 100.0;
    if threshold > within_min {
        threshold = (within_min * 100.0).floor() / 100.0;
    }
    threshold.clamp(COSINE_RANGE.0, COSINE_RANGE.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_threshold_is_the_middle_of_the_gap() {
        assert_eq!(suggested_threshold(0.90, 0.70), 0.80);
        // The midpoint 0.833 rounds to 0.83.
        assert_eq!(suggested_threshold(0.866, 0.80), 0.83);
    }

    #[test]
    fn with_an_overlap_the_threshold_keeps_every_positive_pair() {
        // The smallest positive, rounded down: 0.88 would split it.
        assert_eq!(suggested_threshold(0.876, 0.93), 0.87);
        assert_eq!(suggested_threshold(0.87, 0.93), 0.87);
    }

    #[test]
    fn a_narrow_gap_never_rounds_above_the_smallest_positive() {
        // The midpoint 0.8795 rounds to 0.88, above the positive 0.8796:
        // the threshold is rounded down instead.
        assert_eq!(suggested_threshold(0.8796, 0.8794), 0.87);
    }

    #[test]
    fn the_threshold_stays_in_the_range_a_job_accepts() {
        assert_eq!(suggested_threshold(0.40, 0.10), 0.5);
        assert_eq!(suggested_threshold(1.0, 0.999), 0.99);
    }

    #[test]
    fn the_spread_has_the_middle_value() {
        let odd = spread(&[0.9, 0.5, 0.7]).unwrap();
        assert_eq!(
            odd,
            Spread {
                pairs: 3,
                min: 0.5,
                median: 0.7,
                max: 0.9
            }
        );
        let even = spread(&[0.9, 0.5, 0.7, 0.6]).unwrap();
        assert_eq!((even.min, even.max, even.pairs), (0.5, 0.9, 4));
        assert!((even.median - 0.65).abs() < 1e-6, "{}", even.median);
        assert_eq!(spread(&[]), None);
    }

    fn os(args: &[&str]) -> impl Iterator<Item = OsString> {
        args.iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn arguments_are_parsed() {
        let args = parse_args(os(&[
            "dinov2-small",
            "sets",
            "--check-groups",
            "a",
            "distractors",
            "--check-series",
            "b",
            "--check-groups",
            "c",
            "--window",
            "60",
        ]))
        .unwrap();
        assert_eq!(
            args,
            Args {
                model: model("dinov2-small").unwrap(),
                sets_root: "sets".into(),
                distractors: Some("distractors".into()),
                check_groups: vec!["a".into(), "c".into()],
                window: 60,
                check_series: vec!["b".into()],
            }
        );
        let defaults = parse_args(os(&["dinov2-base", "sets"])).unwrap();
        assert_eq!((defaults.distractors, defaults.window), (None, 30));
    }

    #[test]
    fn bad_arguments_are_refused() {
        assert!(parse_args(os(&["dinov2-small"])).is_err());
        assert!(parse_args(os(&["dinov2-small", "a", "b", "c"])).is_err());
        assert!(parse_args(os(&["dinov2-large", "sets"])).is_err());
        assert!(parse_args(os(&["dinov2-small", "sets", "--check-groups"])).is_err());
        assert!(parse_args(os(&["dinov2-small", "sets", "--window", "4.5"])).is_err());
    }
}
