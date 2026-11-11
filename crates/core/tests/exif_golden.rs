//! Golden check of our EXIF + MakerNote reader against ExifTool on real
//! camera files.
//!
//! The golden comes from `scripts/gen_exif_golden.py` (`exiftool -j -n -G1
//! -a`, ExifTool 13.59) and lives outside the repository, at
//! `$PHOTOSTACKER_FIXTURES/golden/exif-golden.json`. It holds one entry per
//! file under each labelled root; each root is read here from the folder its
//! environment variable names:
//!
//! | label        | environment variable               |
//! |--------------|------------------------------------|
//! | `fixtures`   | `PHOTOSTACKER_FIXTURES`            |
//! | `user-2000d` | `PHOTOSTACKER_USER_PHOTOS_2000D`   |
//! | `user-6d`    | `PHOTOSTACKER_USER_PHOTOS_6D`      |
//!
//! Without `PHOTOSTACKER_FIXTURES` the test prints `SKIPPED` and passes; a
//! root whose variable is unset is skipped with a message.
//!
//! What must agree, per file:
//! - The standard fields equal ExifTool's (`IFD0:` and `ExifIFD:` groups
//!   only): strings trimmed, numbers within 1e-3 absolute *and* 1e-5
//!   relative (exposure times are far below 1e-3), the capture time to the
//!   millisecond including SubSecTimeOriginal, the UTC offset, the numeric
//!   orientation (1 when absent or outside 1–8).
//! - `bracket` equals our documented vendor mapping applied to ExifTool's
//!   numeric MakerNote values (see `expected_bracket`), or the ExposureMode 2
//!   fallback when no vendor field decides; `value_ev` within 1e-3.
//!
//! Known misclassifications that are kept pass the
//! mapping and are printed as `KNOWN` notes: a Fujifilm burst with
//! AutoBracketing on, and a Sony Pixel Shift set (ReleaseMode 5).

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;
use photostacker_core::discover::kind_of;
use photostacker_core::exif::{BracketInfo, BracketMode, ExifData, read_exif};
use serde::Deserialize;
use serde_json::Value;

/// Root label -> the environment variable that holds its WSL path.
const ROOTS: [(&str, &str); 3] = [
    ("fixtures", "PHOTOSTACKER_FIXTURES"),
    ("user-2000d", "PHOTOSTACKER_USER_PHOTOS_2000D"),
    ("user-6d", "PHOTOSTACKER_USER_PHOTOS_6D"),
];

/// Only the public corpus prints every file; the user roots print counts,
/// mismatches and notes.
const VERBOSE_ROOT: &str = "fixtures";

#[derive(Deserialize)]
struct Golden {
    exiftool_version: String,
    roots: BTreeMap<String, GoldenRoot>,
}

#[derive(Deserialize)]
struct GoldenRoot {
    /// Relative path (forward slashes) -> `Group:Tag` -> ExifTool's `-n` value.
    files: BTreeMap<String, BTreeMap<String, Value>>,
}

/// ExifTool's tags of one file.
struct Tags<'a>(&'a BTreeMap<String, Value>);

impl Tags<'_> {
    /// A string tag, trimmed; `None` when absent or empty.
    fn text(&self, tag: &str) -> Option<String> {
        let text = match self.0.get(tag)? {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let text = text.trim_matches(|c: char| c == ' ' || c == '\0');
        (!text.is_empty()).then(|| text.to_owned())
    }

    /// A numeric tag; `None` when absent or not a number (`inf`, `undef`).
    fn num(&self, tag: &str) -> Option<f64> {
        match self.0.get(tag)? {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    fn int(&self, tag: &str) -> Option<i64> {
        self.num(tag).filter(|v| v.fract() == 0.0).map(|v| v as i64)
    }
}

/// Golden floats agree within 1e-3, and within 1e-5 relative to ExifTool's
/// value (which it prints to 10 significant digits).
fn close(ours: f64, exiftool: f64) -> bool {
    let diff = (ours - exiftool).abs();
    diff <= 1e-3 && diff <= 1e-5 * exiftool.abs().max(1e-3)
}

/// The ExifTool capture time in ms since the epoch, read as UTC: the
/// DateTimeOriginal text plus the leading SubSecTimeOriginal digits.
fn expected_capture_ms(t: &Tags) -> Option<i64> {
    let time =
        NaiveDateTime::parse_from_str(&t.text("ExifIFD:DateTimeOriginal")?, "%Y:%m:%d %H:%M:%S")
            .ok()?;
    let mut ms = time.and_utc().timestamp_millis();
    if let Some(sub) = t.text("ExifIFD:SubSecTimeOriginal")
        && sub.bytes().all(|b| b.is_ascii_digit())
    {
        let digits: String = sub.chars().chain("000".chars()).take(3).collect();
        ms += digits.parse::<i64>().expect("three digits");
    }
    Some(ms)
}

/// OffsetTimeOriginal (`±HH:MM`) in minutes east of UTC.
fn expected_offset_minutes(t: &Tags) -> Option<i32> {
    let text = t.text("ExifIFD:OffsetTimeOriginal")?;
    let (sign, rest) = match text.split_at_checked(1)? {
        ("+", rest) => (1, rest),
        ("-", rest) => (-1, rest),
        _ => return None,
    };
    let (h, m) = rest.split_once(':')?;
    let two_digits = |s: &str| s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit());
    if !two_digits(h) || !two_digits(m) {
        return None;
    }
    let (h, m): (i32, i32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(sign * (h * 60 + m))
}

fn info(mode: BracketMode, shot_number: Option<i64>, value_ev: Option<f64>) -> Option<Bracket> {
    Some(match mode {
        // Off carries neither a shot number nor a value.
        BracketMode::Off => Bracket {
            mode,
            shot_number: None,
            value_ev: None,
        },
        _ => Bracket {
            mode,
            shot_number,
            value_ev,
        },
    })
}

/// The expected bracket, comparable to [`BracketInfo`].
#[derive(Debug, Clone, Copy, PartialEq)]
struct Bracket {
    mode: BracketMode,
    shot_number: Option<i64>,
    value_ev: Option<f64>,
}

/// Canon, where any "on" signal wins: FileInfo BracketMode 2–4 Other; else
/// Exposure when BracketMode is 1 or AutoExposureBracketing is non-zero; else
/// Off when either field is present; else undecided. Shot: BracketShotNumber
/// ≥ 1, else AutoExposureBracketing 1–3. EV: AEBBracketValue, 0 included.
fn canon(t: &Tags) -> Option<Bracket> {
    let bracket_mode = t.int("Canon:BracketMode").filter(|m| (0..=4).contains(m));
    let aeb = t.int("Canon:AutoExposureBracketing");
    let mode = match (bracket_mode, aeb) {
        (Some(2..=4), _) => BracketMode::Other,
        (Some(1), _) => BracketMode::Exposure,
        (_, Some(aeb)) if aeb != 0 => BracketMode::Exposure,
        (None, None) => return None,
        _ => BracketMode::Off,
    };
    let shot = t
        .int("Canon:BracketShotNumber")
        .filter(|&n| n >= 1)
        .or_else(|| aeb.filter(|n| (1..=3).contains(n)));
    info(mode, shot, t.num("Canon:AEBBracketValue"))
}

/// Nikon ShootingMode bit 4 Exposure; bit 6 or 8 Other; else Off. EV:
/// ExposureBracketValue. No shot number.
fn nikon(t: &Tags) -> Option<Bracket> {
    let bits = t.int("Nikon:ShootingMode")?;
    let mode = if bits & (1 << 4) != 0 {
        BracketMode::Exposure
    } else if bits & ((1 << 6) | (1 << 8)) != 0 {
        BracketMode::Other
    } else {
        BracketMode::Off
    };
    info(mode, None, t.num("Nikon:ExposureBracketValue"))
}

/// Sony ReleaseMode 0/2 Off, 5 Exposure, 6/8 Other, anything else undecided.
/// Shot: SequenceNumber unless 0 or 65535. No EV.
fn sony(t: &Tags) -> Option<Bracket> {
    let mode = match t.int("Sony:ReleaseMode")? {
        0 | 2 => BracketMode::Off,
        5 => BracketMode::Exposure,
        6 | 8 => BracketMode::Other,
        _ => return None,
    };
    let shot = t
        .int("Sony:SequenceNumber")
        .filter(|&n| n != 0 && n != 65535);
    info(mode, shot, None)
}

/// Fujifilm AutoBracketing 0 Off, 1 Exposure, other Other. Shot:
/// SequenceNumber unless 0. No EV.
fn fujifilm(t: &Tags) -> Option<Bracket> {
    let mode = match t.int("FujiFilm:AutoBracketing")? {
        0 => BracketMode::Off,
        1 => BracketMode::Exposure,
        _ => BracketMode::Other,
    };
    let shot = t.int("FujiFilm:SequenceNumber").filter(|&n| n != 0);
    info(mode, shot, None)
}

/// Our documented mapping (Tasks 7–10) applied to ExifTool's numeric vendor
/// values, by the lowercase Make prefix; without a decision, EXIF
/// ExposureMode 2 is an exposure bracket valued at the exposure bias.
fn expected_bracket(t: &Tags) -> Option<Bracket> {
    let make = t.text("IFD0:Make").unwrap_or_default().to_ascii_lowercase();
    let vendor = if make.starts_with("canon") {
        canon(t)
    } else if make.starts_with("nikon") {
        nikon(t)
    } else if make.starts_with("sony") {
        sony(t)
    } else if make.starts_with("fujifilm") {
        fujifilm(t)
    } else {
        None
    };
    vendor.or_else(|| {
        (t.int("ExifIFD:ExposureMode") == Some(2)).then(|| Bracket {
            mode: BracketMode::Exposure,
            shot_number: None,
            value_ev: t.num("ExifIFD:ExposureCompensation"),
        })
    })
}

/// The R19 known misclassifications this file falls under.
fn known_notes(t: &Tags, bracket: Option<Bracket>) -> Vec<String> {
    let mut notes = Vec::new();
    let exposure = bracket.is_some_and(|b| b.mode == BracketMode::Exposure);
    if exposure && t.int("FujiFilm:DriveMode").is_some_and(|d| d != 0) {
        notes.push(format!(
            "KNOWN (R19): Fujifilm AutoBracketing on with DriveMode {} (a burst) reads as Exposure",
            t.int("FujiFilm:DriveMode").unwrap_or_default()
        ));
    }
    if let Some(info) = t.text("Sony:PixelShiftInfo") {
        // "<group id> <shot> <total> <flags>": a non-zero total is a Pixel Shift set.
        let total = info
            .split_whitespace()
            .nth(2)
            .and_then(|n| n.parse::<u32>().ok());
        if exposure && total.is_some_and(|n| n > 0) {
            notes.push(format!(
                "KNOWN (R19): Sony Pixel Shift ({info}) reads as Exposure via ReleaseMode 5"
            ));
        }
    }
    notes
}

fn opt_f32(ours: Option<f32>, exiftool: Option<f64>) -> bool {
    match (ours, exiftool) {
        (None, None) => true,
        (Some(a), Some(b)) => close(f64::from(a), b),
        _ => false,
    }
}

/// Every field where `ours` and ExifTool's `t` disagree.
fn mismatches(ours: &ExifData, t: &Tags) -> Vec<String> {
    let mut out = Vec::new();
    let mut check = |field: &str, ok: bool, ours: String, exiftool: String| {
        if !ok {
            out.push(format!("{field}: ours {ours}, exiftool {exiftool}"));
        }
    };
    for (field, value, tag) in [
        ("make", &ours.make, "IFD0:Make"),
        ("model", &ours.model, "IFD0:Model"),
        ("lens_model", &ours.lens_model, "ExifIFD:LensModel"),
    ] {
        let expected = t.text(tag);
        check(
            field,
            value.as_deref().map(str::trim) == expected.as_deref(),
            format!("{value:?}"),
            format!("{expected:?}"),
        );
    }
    for (field, value, tag) in [
        (
            "focal_length_mm",
            ours.focal_length_mm,
            "ExifIFD:FocalLength",
        ),
        (
            "exposure_time_s",
            ours.exposure_time_s,
            "ExifIFD:ExposureTime",
        ),
        ("f_number", ours.f_number, "ExifIFD:FNumber"),
        (
            "exposure_bias_ev",
            ours.exposure_bias_ev,
            "ExifIFD:ExposureCompensation",
        ),
    ] {
        let expected = t.num(tag);
        check(
            field,
            opt_f32(value, expected),
            format!("{value:?}"),
            format!("{expected:?}"),
        );
    }
    let iso = t.int("ExifIFD:ISO");
    check(
        "iso",
        ours.iso.map(i64::from) == iso,
        format!("{:?}", ours.iso),
        format!("{iso:?}"),
    );
    let mode = t.int("ExifIFD:ExposureMode");
    check(
        "exposure_mode",
        ours.exposure_mode.map(i64::from) == mode,
        format!("{:?}", ours.exposure_mode),
        format!("{mode:?}"),
    );
    let orientation = t
        .int("IFD0:Orientation")
        .filter(|o| (1..=8).contains(o))
        .unwrap_or(1);
    check(
        "orientation",
        i64::from(ours.orientation) == orientation,
        ours.orientation.to_string(),
        orientation.to_string(),
    );
    let ours_ms = ours
        .capture_time
        .map(|time| time.and_utc().timestamp_millis());
    let capture = expected_capture_ms(t);
    check(
        "capture_time",
        ours_ms == capture,
        format!("{:?}", ours.capture_time),
        format!(
            "{:?} + {:?}",
            t.text("ExifIFD:DateTimeOriginal"),
            t.text("ExifIFD:SubSecTimeOriginal")
        ),
    );
    let offset = expected_offset_minutes(t);
    check(
        "utc_offset_minutes",
        ours.utc_offset_minutes == offset,
        format!("{:?}", ours.utc_offset_minutes),
        format!("{offset:?}"),
    );

    let expected = expected_bracket(t);
    let same = match (ours.bracket, expected) {
        (None, None) => true,
        (
            Some(BracketInfo {
                mode,
                shot_number,
                value_ev,
            }),
            Some(e),
        ) => {
            mode == e.mode
                && shot_number.map(i64::from) == e.shot_number
                && match (value_ev, e.value_ev) {
                    (None, None) => true,
                    (Some(a), Some(b)) => (f64::from(a) - b).abs() <= 1e-3,
                    _ => false,
                }
        }
        _ => false,
    };
    check(
        "bracket",
        same,
        format!("{:?}", ours.bracket),
        format!("{expected:?} (mapped from {})", vendor_fields(t)),
    );
    out
}

/// The vendor tags ExifTool reported, for messages.
fn vendor_fields(t: &Tags) -> String {
    let fields: Vec<String> =
        t.0.iter()
            .filter(|(tag, _)| !tag.starts_with("IFD0:") && !tag.starts_with("ExifIFD:"))
            .map(|(tag, value)| format!("{tag}={value}"))
            .collect();
    if fields.is_empty() {
        format!(
            "no vendor fields; ExposureMode={:?}",
            t.int("ExifIFD:ExposureMode")
        )
    } else {
        fields.join(" ")
    }
}

fn brief(bracket: Option<BracketInfo>) -> String {
    match bracket {
        None => "-".to_owned(),
        Some(b) => {
            let shot = b.shot_number.map_or("-".to_owned(), |n| n.to_string());
            let ev = b.value_ev.map_or("-".to_owned(), |v| format!("{v:+.2}"));
            format!("{:?} shot {shot} ev {ev}", b.mode)
        }
    }
}

/// The supported files under `root`, as golden keys; folders named `golden`
/// or starting with `_` are skipped, as the generator does.
fn files_under(root: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("readable folder") {
            let path = entry.expect("folder entry").path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if path.is_dir() {
                if name != "golden" && !name.starts_with('_') {
                    stack.push(path);
                }
            } else if kind_of(&path).is_some() {
                let rel = path.strip_prefix(root).expect("under root");
                let parts: Vec<_> = rel.iter().map(|p| p.to_string_lossy()).collect();
                out.insert(parts.join("/"));
            }
        }
    }
    out
}

/// Compares one root; returns (files compared, failures).
fn check_root(label: &str, root: &Path, golden: &GoldenRoot) -> (usize, Vec<String>) {
    let verbose = label == VERBOSE_ROOT;
    let mut failures = Vec::new();
    let on_disk = files_under(root);
    let in_golden: BTreeSet<String> = golden.files.keys().cloned().collect();
    if on_disk != in_golden {
        failures.push(format!(
            "{label}: the golden is stale (regenerate it): only on disk {:?}, only in golden {:?}",
            on_disk.difference(&in_golden).collect::<Vec<_>>(),
            in_golden.difference(&on_disk).collect::<Vec<_>>()
        ));
    }
    let (mut notes, mut failed) = (0, 0);
    for (key, tags) in &golden.files {
        let tags = Tags(tags);
        let path: PathBuf = key
            .split('/')
            .fold(root.to_path_buf(), |p, part| p.join(part));
        let kind = kind_of(&path).expect("golden files have supported extensions");
        let ours = match read_exif(&path, kind) {
            Ok(data) => data.unwrap_or_default(),
            Err(err) => {
                println!("  MISMATCH {key}: read_exif failed: {err}");
                failures.push(format!("{label}/{key}: read_exif failed: {err}"));
                failed += 1;
                continue;
            }
        };
        let diffs = mismatches(&ours, &tags);
        failed += usize::from(!diffs.is_empty());
        let known = known_notes(&tags, expected_bracket(&tags));
        notes += known.len();
        if verbose || !diffs.is_empty() || !known.is_empty() {
            let status = if diffs.is_empty() {
                "ok      "
            } else {
                "MISMATCH"
            };
            println!("  {status} {key}  [{}]", brief(ours.bracket));
            for note in &known {
                println!("           {note}");
            }
        }
        for diff in diffs {
            println!("           {diff}");
            failures.push(format!("{label}/{key}: {diff}"));
        }
    }
    let n = golden.files.len();
    println!(
        "{label}: {n} files compared, {} passed, {notes} known (R19) notes",
        n - failed
    );
    (n, failures)
}

#[test]
fn exif_golden_matches_exiftool() {
    let Some(fixtures) = env::var_os("PHOTOSTACKER_FIXTURES") else {
        println!("SKIPPED: PHOTOSTACKER_FIXTURES not set");
        return;
    };
    let golden_path = Path::new(&fixtures).join("golden/exif-golden.json");
    let text = std::fs::read_to_string(&golden_path).unwrap_or_else(|err| {
        panic!(
            "{}: {err}; generate it with scripts/gen_exif_golden.py",
            golden_path.display()
        )
    });
    let golden: Golden = serde_json::from_str(&text).expect("golden JSON parses");
    println!("golden: ExifTool {}", golden.exiftool_version);
    for label in golden.roots.keys() {
        assert!(
            ROOTS.iter().any(|(known, _)| known == label),
            "unknown root label {label:?} in the golden"
        );
    }
    assert!(
        golden.roots.contains_key("fixtures"),
        "the golden has no 'fixtures' root"
    );

    let mut failures = Vec::new();
    for (label, var) in ROOTS {
        let Some(root) = golden.roots.get(label) else {
            println!("SKIPPED root {label}: not in the golden");
            continue;
        };
        let Some(path) = env::var_os(var) else {
            println!("SKIPPED root {label}: {var} not set");
            continue;
        };
        println!("{label}:");
        let (_, root_failures) = check_root(label, Path::new(&path), root);
        failures.extend(root_failures);
    }
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
