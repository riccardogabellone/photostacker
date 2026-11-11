//! Stage 1: splits the files into buckets of shots taken with one camera setup,
//! close together in time. Stage 1 never merges anything by itself: it only
//! bounds where Stage 2 may link photos.
//!
//! - A file without usable EXIF (neither a model nor a capture time) goes to
//!   one `no-exif` bucket per folder, so Stage 2 alone decides for it.
//! - Every other file is placed on a timeline by its capture instant, or by its
//!   modification time (with a [`Warning::NoCaptureTime`]), and has a camera
//!   key: folder, model, lens, focal length to 0.1 mm, and which clock placed
//!   it. A capture time without a UTC offset is the camera's wall clock read
//!   as UTC, which can be hours off an mtime, so mtime-placed files only ever
//!   bucket with each other.
//! - Sweeping the timeline, each camera key has at most one open bucket. A file
//!   joins it when it is within the time window of that bucket's last file and
//!   does not restart a bracket sequence; otherwise it opens a new bucket for
//!   its key. Comparing against the key's own last file, never the previous
//!   file overall, keeps two bodies shooting at the same time apart.
//! - A bracket sequence restarts when a shot number does not increase, or,
//!   for exposure-bracket shots without a number, when a bracket value
//!   repeats within the bucket.
//!
//! The earlier Python app (docs/history.md) put every bracketed shot in a
//! `bracket::{model}::{DateTimeOriginal}` bucket, one per second. Here a
//! bracketed shot buckets like any other, by time window, and a restarted
//! bracket sequence starts a new bucket.

use std::borrow::Cow;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use super::FileFacts;
use crate::exif::{BracketMode, ExifData};
use crate::model::Warning;

/// A set of files that Stage 2 may link with each other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bucket {
    /// A readable name, unique in one run: `{folder}|{model}|{lens}|{focal}@{seq}`
    /// with `?` for a missing field, `{folder}|{model}|{lens}|{focal}|mtime@{seq}`
    /// for files placed by their mtime, or `{folder}|no-exif`. `{folder}` is the
    /// parent folder as the file paths spell it, `.` when it is empty (a file
    /// at the root of relative paths). It is display only: buckets compare
    /// the folder paths themselves.
    pub key: String,
    /// File ids, ordered by (time, path); by (mtime, path) in a `no-exif` bucket.
    pub members: Vec<usize>,
}

/// Splits the files `ids` (indices into `facts`, in any order, possibly
/// sparse) into buckets.
///
/// Returns the time-swept buckets in creation order, then the `no-exif`
/// buckets ordered by folder; and a [`Warning::NoCaptureTime`] for every file
/// placed without an EXIF capture time, in ascending id order.
///
/// # Panics
///
/// If an id in `ids` is not a valid index into `facts`.
pub fn bucket(
    facts: &[FileFacts],
    ids: &[usize],
    window_seconds: u32,
) -> (Vec<Bucket>, Vec<(usize, Warning)>) {
    let mut shots = Vec::new();
    let mut no_exif: BTreeMap<&Path, Vec<usize>> = BTreeMap::new();
    let mut warnings = Vec::new();
    for &id in ids {
        let file = &facts[id];
        let folder = parent_folder(&file.path);
        let Some(exif) = usable_exif(file) else {
            no_exif.entry(folder).or_default().push(id);
            warnings.push((id, Warning::NoCaptureTime));
            continue;
        };
        let clock = if exif.capture_instant_ms().is_some() {
            Clock::Exif
        } else {
            warnings.push((id, Warning::NoCaptureTime));
            Clock::Mtime
        };
        shots.push(Shot {
            id,
            path: &file.path,
            time_ms: file.time_ms(),
            camera: camera_key(folder, exif, clock),
            bracket_shot: file.bracket_shot(),
            bracket_value: unnumbered_exposure_value(exif),
        });
    }

    let mut buckets = sweep(shots, window_seconds);
    for (folder, mut members) in no_exif {
        members.sort_by(|&a, &b| {
            (facts[a].mtime_ms, &facts[a].path).cmp(&(facts[b].mtime_ms, &facts[b].path))
        });
        buckets.push(Bucket {
            key: format!("{}|no-exif", folder_label(folder)),
            members,
        });
    }
    warnings.sort_by_key(|&(id, _)| id);
    (buckets, warnings)
}

/// The camera setup a shot was taken with. Shots of different setups never
/// share a bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CameraKey<'a> {
    folder: &'a Path,
    model: Option<&'a str>,
    lens: Option<&'a str>,
    /// The focal length in tenths of a millimetre.
    focal_tenths: Option<i64>,
    clock: Clock,
}

/// The clock that placed a shot on the timeline. The two are not comparable,
/// so shots of different clocks never share a bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Clock {
    /// The EXIF capture instant.
    Exif,
    /// The file's modification time: the file has no EXIF capture time.
    Mtime,
}

impl CameraKey<'_> {
    /// The key of the run's `seq`-th bucket.
    fn label(&self, seq: usize) -> String {
        let focal = self.focal_tenths.map_or_else(
            || "?".to_owned(),
            |tenths| format!("{:.1}", tenths as f64 / 10.0),
        );
        let clock = match self.clock {
            Clock::Exif => "",
            Clock::Mtime => "|mtime",
        };
        format!(
            "{}|{}|{}|{focal}{clock}@{seq}",
            folder_label(self.folder),
            self.model.unwrap_or("?"),
            self.lens.unwrap_or("?"),
        )
    }
}

/// A file placed on the timeline.
struct Shot<'a> {
    id: usize,
    path: &'a Path,
    time_ms: i64,
    camera: CameraKey<'a>,
    /// The position in a bracket sequence; `None` when not bracketing, or when
    /// the camera did not record it.
    bracket_shot: Option<u16>,
    /// The bracket value, in hundredths of an EV, of an exposure-bracket shot
    /// without a shot number; `None` for any other shot.
    bracket_value: Option<i32>,
}

/// The bucket a camera key is filling, and what its shots were.
struct OpenBucket {
    index: usize,
    last_time_ms: i64,
    /// The number of the bucket's last numbered bracket shot: a shot without
    /// a number leaves it as it was.
    last_bracket_shot: Option<u16>,
    /// The values, in hundredths of an EV, of the bucket's exposure-bracket
    /// shots without a shot number: the values its sequence has taken so far.
    bracket_values: Vec<i32>,
}

impl OpenBucket {
    /// The bucket `shot` opens, as the first shot of a new sequence.
    fn start(index: usize, shot: &Shot<'_>) -> Self {
        Self {
            index,
            last_time_ms: shot.time_ms,
            last_bracket_shot: shot.bracket_shot,
            bracket_values: shot.bracket_value.into_iter().collect(),
        }
    }

    /// Records `shot` joining the bucket. A shot without a number (bracketing
    /// off, or not recorded) keeps the number the bucket remembers, and only
    /// an unnumbered exposure-bracket shot adds a value.
    fn add(&mut self, shot: &Shot<'_>) {
        self.last_time_ms = shot.time_ms;
        if shot.bracket_shot.is_some() {
            self.last_bracket_shot = shot.bracket_shot;
        }
        self.bracket_values.extend(shot.bracket_value);
    }
}

/// Walks the shots in (time, path) order, keeping one open bucket per camera key.
fn sweep(mut shots: Vec<Shot<'_>>, window_seconds: u32) -> Vec<Bucket> {
    let window_ms = i64::from(window_seconds) * 1000;
    shots.sort_by(|a, b| (a.time_ms, a.path).cmp(&(b.time_ms, b.path)));

    let mut buckets: Vec<Bucket> = Vec::new();
    let mut open: HashMap<CameraKey<'_>, OpenBucket> = HashMap::new();
    for shot in shots {
        let index = match open.entry(shot.camera) {
            Entry::Occupied(mut entry) if continues(entry.get(), &shot, window_ms) => {
                entry.get_mut().add(&shot);
                entry.get().index
            }
            entry => {
                // The swept buckets come first in the output, so the next index
                // is also the run-wide sequence number.
                let seq = buckets.len();
                buckets.push(Bucket {
                    key: shot.camera.label(seq),
                    members: Vec::new(),
                });
                entry.insert_entry(OpenBucket::start(seq, &shot));
                seq
            }
        };
        buckets[index].members.push(shot.id);
    }
    buckets
}

/// Whether `shot` joins `bucket`: it is within the window of the bucket's last
/// shot and does not restart a bracket sequence. A sequence restarts when the
/// shot number does not increase (a repeat counts), or when an unnumbered
/// exposure-bracket shot repeats a value the sequence already has.
fn continues(bucket: &OpenBucket, shot: &Shot<'_>, window_ms: i64) -> bool {
    // Shots are swept in time order, so the gap is never negative.
    let within_window = shot.time_ms.saturating_sub(bucket.last_time_ms) <= window_ms;
    let bracket_reset = matches!(
        (shot.bracket_shot, bucket.last_bracket_shot),
        (Some(n), Some(p)) if n <= p
    );
    let value_repeat = shot
        .bracket_value
        .is_some_and(|value| bucket.bracket_values.contains(&value));
    within_window && !bracket_reset && !value_repeat
}

/// The bracket value, in hundredths of an EV, of an exposure-bracket shot
/// that has no shot number and a finite value; `None` for any other shot.
fn unnumbered_exposure_value(exif: &ExifData) -> Option<i32> {
    let bracket = exif.bracket?;
    if bracket.mode != BracketMode::Exposure || bracket.shot_number.is_some() {
        return None;
    }
    bracket
        .value_ev
        .filter(|ev| ev.is_finite())
        .map(|ev| (f64::from(ev) * 100.0).round() as i32)
}

/// The EXIF of `file`, unless it has neither a model nor a capture time.
fn usable_exif(file: &FileFacts) -> Option<&ExifData> {
    file.exif
        .as_ref()
        .filter(|exif| exif.model.is_some() || exif.capture_time.is_some())
}

fn parent_folder(path: &Path) -> &Path {
    path.parent().unwrap_or(Path::new(""))
}

/// How a folder reads in a bucket key: `.` for the empty folder.
fn folder_label(folder: &Path) -> Cow<'_, str> {
    if folder.as_os_str().is_empty() {
        Cow::Borrowed(".")
    } else {
        folder.to_string_lossy()
    }
}

fn camera_key<'a>(folder: &'a Path, exif: &'a ExifData, clock: Clock) -> CameraKey<'a> {
    CameraKey {
        folder,
        model: exif.model.as_deref(),
        lens: exif.lens_model.as_deref(),
        focal_tenths: exif.focal_length_mm.map(|mm| (mm * 10.0).round() as i64),
        clock,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{NaiveDate, NaiveDateTime, TimeDelta};

    use super::*;
    use crate::exif::{BracketInfo, BracketMode, ExifData};

    const WINDOW_S: u32 = 4;

    fn at(seconds: i64) -> NaiveDateTime {
        let start = NaiveDate::from_ymd_opt(2026, 11, 7)
            .and_then(|d| d.and_hms_opt(10, 0, 0))
            .unwrap();
        start + TimeDelta::seconds(seconds)
    }

    /// A photo from the default camera setup, taken `seconds` after a fixed instant.
    fn photo(path: &str, seconds: i64) -> FileFacts {
        FileFacts {
            path: path.into(),
            exif: Some(ExifData {
                model: Some("Canon EOS 2000D".into()),
                lens_model: Some("EF-S18-55mm f/3.5-5.6 III".into()),
                focal_length_mm: Some(18.0),
                capture_time: Some(at(seconds)),
                ..ExifData::default()
            }),
            mtime_ms: 0,
        }
    }

    fn bracketed(path: &str, seconds: i64, mode: BracketMode, shot: u16) -> FileFacts {
        let mut file = photo(path, seconds);
        exif_mut(&mut file).bracket = Some(BracketInfo {
            mode,
            shot_number: Some(shot),
            value_ev: None,
        });
        file
    }

    /// A shot taken `ms` milliseconds after the fixed instant, in bracket
    /// `mode`, with no shot number and the value `value_ev`.
    fn unnumbered(path: &str, ms: i64, mode: BracketMode, value_ev: Option<f32>) -> FileFacts {
        let mut file = photo(path, 0);
        let exif = exif_mut(&mut file);
        exif.capture_time = Some(at(0) + TimeDelta::milliseconds(ms));
        exif.bracket = Some(BracketInfo {
            mode,
            shot_number: None,
            value_ev,
        });
        file
    }

    fn exif_mut(file: &mut FileFacts) -> &mut ExifData {
        file.exif.as_mut().unwrap()
    }

    /// Buckets every file and returns the members of each bucket.
    fn members_of_all(facts: &[FileFacts]) -> Vec<Vec<usize>> {
        let ids: Vec<usize> = (0..facts.len()).collect();
        let (buckets, _) = bucket(facts, &ids, WINDOW_S);
        buckets.into_iter().map(|b| b.members).collect()
    }

    #[test]
    fn chains_within_window_from_previous_shot() {
        let chain: Vec<FileFacts> = [0, 3, 6, 9]
            .into_iter()
            .enumerate()
            .map(|(i, s)| photo(&format!("a/{i}.jpg"), s))
            .collect();
        assert_eq!(members_of_all(&chain), [vec![0, 1, 2, 3]]);

        let gap = [photo("a/0.jpg", 0), photo("a/1.jpg", 5)];
        assert_eq!(members_of_all(&gap), [vec![0], vec![1]]);

        // The window is inclusive, to the millisecond.
        let edge = [photo("a/0.jpg", 0), photo("a/1.jpg", 4)];
        assert_eq!(members_of_all(&edge), [vec![0, 1]]);
        let mut past_edge = [photo("a/0.jpg", 0), photo("a/1.jpg", 0)];
        exif_mut(&mut past_edge[1]).capture_time = Some(at(4) + TimeDelta::milliseconds(1));
        assert_eq!(members_of_all(&past_edge), [vec![0], vec![1]]);
    }

    #[test]
    fn different_lens_splits() {
        let mut other_lens = photo("a/1.jpg", 1);
        exif_mut(&mut other_lens).lens_model = Some("EF50mm f/1.8 STM".into());
        assert_eq!(
            members_of_all(&[photo("a/0.jpg", 0), other_lens]),
            [vec![0], vec![1]]
        );
    }

    #[test]
    fn focal_length_compares_in_tenths_of_a_mm() {
        let mut facts = [
            photo("a/0.jpg", 0),
            photo("a/1.jpg", 1),
            photo("a/2.jpg", 2),
        ];
        exif_mut(&mut facts[1]).focal_length_mm = Some(18.04);
        exif_mut(&mut facts[2]).focal_length_mm = Some(18.2);
        let (buckets, _) = bucket(&facts, &[0, 1, 2], WINDOW_S);
        assert_eq!(buckets[0].members, [0, 1]);
        assert_eq!(buckets[1].members, [2]);
        assert!(buckets[1].key.ends_with("|18.2@1"), "{}", buckets[1].key);
    }

    #[test]
    fn interleaved_bodies_bucket_separately() {
        let body = |path: &str, seconds: i64, model: &str| {
            let mut file = photo(path, seconds);
            exif_mut(&mut file).model = Some(model.into());
            file
        };
        let facts = [
            body("a/a0.jpg", 0, "A"),
            body("a/a2.jpg", 2, "A"),
            body("a/a4.jpg", 4, "A"),
            body("a/b1.jpg", 1, "B"),
            body("a/b3.jpg", 3, "B"),
            body("a/b5.jpg", 5, "B"),
        ];
        assert_eq!(members_of_all(&facts), [vec![0, 1, 2], vec![3, 4, 5]]);
    }

    #[test]
    fn shot_number_reset_starts_new_bucket() {
        let facts: Vec<FileFacts> = [1, 2, 3, 1, 2, 3]
            .into_iter()
            .enumerate()
            .map(|(i, shot)| {
                bracketed(&format!("a/{i}.jpg"), i as i64, BracketMode::Exposure, shot)
            })
            .collect();
        assert_eq!(members_of_all(&facts), [vec![0, 1, 2], vec![3, 4, 5]]);

        // A repeated shot number also starts a new sequence.
        let repeated = [
            bracketed("a/0.jpg", 0, BracketMode::Other, 1),
            bracketed("a/1.jpg", 1, BracketMode::Other, 1),
        ];
        assert_eq!(members_of_all(&repeated), [vec![0], vec![1]]);
    }

    #[test]
    fn canon_off_never_resets() {
        let facts: Vec<FileFacts> = (0..3)
            .map(|i| bracketed(&format!("a/{i}.jpg"), i, BracketMode::Off, 0))
            .collect();
        assert_eq!(members_of_all(&facts), [vec![0, 1, 2]]);

        // An Off shot's number is not the "last shot number" either: the
        // bucket still compares with the bracketed shot before it.
        let mixed = [
            bracketed("a/0.jpg", 0, BracketMode::Exposure, 1),
            bracketed("a/1.jpg", 1, BracketMode::Off, 5),
            bracketed("a/2.jpg", 2, BracketMode::Exposure, 2),
        ];
        assert_eq!(members_of_all(&mixed), [vec![0, 1, 2]]);
    }

    #[test]
    fn off_shot_keeps_the_bracket_reset_memory() {
        // 1, 2, 3, then a shot with bracketing off, then 1, 2, 3, all 1 s
        // apart: the reset after the Off shot still closes the bucket.
        let mut facts: Vec<FileFacts> = [1, 2, 3, 0, 1, 2, 3]
            .into_iter()
            .enumerate()
            .map(|(i, shot)| {
                let mode = if shot == 0 {
                    BracketMode::Off
                } else {
                    BracketMode::Exposure
                };
                bracketed(&format!("a/{i}.jpg"), i as i64, mode, shot)
            })
            .collect();
        assert_eq!(members_of_all(&facts), [vec![0, 1, 2, 3], vec![4, 5, 6]]);

        // A bracketed shot with no recorded number keeps the memory too.
        exif_mut(&mut facts[3]).bracket = Some(BracketInfo {
            mode: BracketMode::Exposure,
            shot_number: None,
            value_ev: None,
        });
        assert_eq!(members_of_all(&facts), [vec![0, 1, 2, 3], vec![4, 5, 6]]);
    }

    #[test]
    fn repeated_bracket_value_closes_the_bucket() {
        let facts: Vec<FileFacts> = [0.0, -2.0, 2.0, 0.0, -2.0, 2.0]
            .into_iter()
            .enumerate()
            .map(|(i, ev)| {
                let ms = 400 * i as i64;
                unnumbered(&format!("a/{i}.jpg"), ms, BracketMode::Exposure, Some(ev))
            })
            .collect();
        assert_eq!(members_of_all(&facts), [vec![0, 1, 2], vec![3, 4, 5]]);

        // Values compare in hundredths of an EV: −2/3 as recorded and −0.667
        // are the same value.
        let thirds: Vec<FileFacts> = [-2.0 / 3.0, 0.0, 2.0 / 3.0, -0.667]
            .into_iter()
            .enumerate()
            .map(|(i, ev)| {
                let ms = 400 * i as i64;
                unnumbered(&format!("a/{i}.jpg"), ms, BracketMode::Exposure, Some(ev))
            })
            .collect();
        assert_eq!(members_of_all(&thirds), [vec![0, 1, 2], vec![3]]);
    }

    #[test]
    fn bracket_value_repeat_ignores_off_and_numbered_shots() {
        // 0, −2, +2, then an Off shot carrying 0, then 0, −2, +2: the Off shot
        // neither closes the bucket nor clears its values, so the next 0 does.
        let off_between: Vec<FileFacts> = [0.0, -2.0, 2.0, 0.0, 0.0, -2.0, 2.0]
            .into_iter()
            .enumerate()
            .map(|(i, ev)| {
                let mode = if i == 3 {
                    BracketMode::Off
                } else {
                    BracketMode::Exposure
                };
                unnumbered(&format!("a/{i}.jpg"), 400 * i as i64, mode, Some(ev))
            })
            .collect();
        assert_eq!(
            members_of_all(&off_between),
            [vec![0, 1, 2, 3], vec![4, 5, 6]]
        );

        // Other-mode shots, and Exposure shots without a finite value, never
        // close a bucket on their values.
        let valueless = [
            unnumbered("a/0.jpg", 0, BracketMode::Other, Some(0.0)),
            unnumbered("a/1.jpg", 400, BracketMode::Other, Some(0.0)),
            unnumbered("a/2.jpg", 800, BracketMode::Exposure, None),
            unnumbered("a/3.jpg", 1200, BracketMode::Exposure, None),
            unnumbered("a/4.jpg", 1600, BracketMode::Exposure, Some(f32::NAN)),
            unnumbered("a/5.jpg", 2000, BracketMode::Exposure, Some(f32::NAN)),
        ];
        assert_eq!(members_of_all(&valueless), [vec![0, 1, 2, 3, 4, 5]]);

        // A numbered run keeps the shot-number rule: values 0, −1, +1, 0 with
        // shots 1 to 4 stay one bucket.
        let numbered: Vec<FileFacts> = [0.0, -1.0, 1.0, 0.0]
            .into_iter()
            .enumerate()
            .map(|(i, ev)| {
                let shot = i as u16 + 1;
                let mut file =
                    bracketed(&format!("a/{i}.jpg"), i as i64, BracketMode::Exposure, shot);
                exif_mut(&mut file).bracket.as_mut().unwrap().value_ev = Some(ev);
                file
            })
            .collect();
        assert_eq!(members_of_all(&numbered), [vec![0, 1, 2, 3]]);
    }

    #[test]
    fn no_exif_files_bucket_per_folder() {
        let no_exif = |path: &str, mtime_ms: i64| FileFacts {
            path: path.into(),
            exif: None,
            mtime_ms,
        };
        let mut lens_only = no_exif("a/z.png", 10);
        lens_only.exif = Some(ExifData {
            lens_model: Some("EF50mm f/1.8 STM".into()),
            ..ExifData::default()
        });
        let facts = [
            no_exif("b/x.jpg", 50),
            no_exif("a/y.jpg", 1_000_000_000),
            lens_only,
            photo("a/p.jpg", 0),
        ];
        let (buckets, warnings) = bucket(&facts, &[0, 1, 2, 3], WINDOW_S);
        let keys_and_members: Vec<(&str, &[usize])> = buckets
            .iter()
            .map(|b| (b.key.as_str(), b.members.as_slice()))
            .collect();
        assert_eq!(
            keys_and_members,
            [
                (
                    "a|Canon EOS 2000D|EF-S18-55mm f/3.5-5.6 III|18.0@0",
                    &[3][..]
                ),
                ("a|no-exif", &[2, 1][..]),
                ("b|no-exif", &[0][..]),
            ]
        );
        assert_eq!(
            warnings,
            [
                (0, Warning::NoCaptureTime),
                (1, Warning::NoCaptureTime),
                (2, Warning::NoCaptureTime),
            ]
        );
    }

    #[test]
    fn mtime_fallback_warns() {
        let start_ms = at(0).and_utc().timestamp_millis();
        let mut facts = [
            photo("a/0.jpg", 0),
            photo("a/1.jpg", 0),
            photo("a/2.jpg", 0),
        ];
        for (file, mtime_ms) in facts[1..]
            .iter_mut()
            .zip([start_ms + 2_000, start_ms + 60_000])
        {
            exif_mut(file).capture_time = None;
            file.mtime_ms = mtime_ms;
        }
        let (buckets, warnings) = bucket(&facts, &[0, 1, 2], WINDOW_S);
        let members: Vec<&[usize]> = buckets.iter().map(|b| b.members.as_slice()).collect();
        // The mtime-placed files never join the EXIF-timed one, and 58 s
        // apart they do not join each other either.
        assert_eq!(members, [&[0][..], &[1][..], &[2][..]]);
        assert_eq!(
            warnings,
            [(1, Warning::NoCaptureTime), (2, Warning::NoCaptureTime)]
        );
    }

    #[test]
    fn mtime_files_bucket_apart_from_exif_timed_files() {
        // A has an EXIF time and no UTC offset; B and C have no
        // DateTimeOriginal and mtimes 2 s and 3 s after A's time read as UTC.
        // A camera clock without an offset and an mtime are not comparable,
        // however close they look.
        let start_ms = at(0).and_utc().timestamp_millis();
        let mut facts = [
            photo("a/a.jpg", 0),
            photo("a/b.jpg", 0),
            photo("a/c.jpg", 0),
        ];
        for (file, mtime_ms) in facts[1..]
            .iter_mut()
            .zip([start_ms + 2_000, start_ms + 3_000])
        {
            exif_mut(file).capture_time = None;
            file.mtime_ms = mtime_ms;
        }
        let (buckets, _) = bucket(&facts, &[0, 1, 2], WINDOW_S);
        let keys_and_members: Vec<(&str, &[usize])> = buckets
            .iter()
            .map(|b| (b.key.as_str(), b.members.as_slice()))
            .collect();
        assert_eq!(
            keys_and_members,
            [
                (
                    "a|Canon EOS 2000D|EF-S18-55mm f/3.5-5.6 III|18.0@0",
                    &[0][..]
                ),
                (
                    "a|Canon EOS 2000D|EF-S18-55mm f/3.5-5.6 III|18.0|mtime@1",
                    &[1, 2][..]
                ),
            ]
        );
    }

    #[test]
    fn buckets_never_cross_folders() {
        let facts = [
            photo("a/0.jpg", 0),
            photo("b/1.jpg", 0),
            photo("a/2.jpg", 1),
            photo("b/3.jpg", 1),
        ];
        let (buckets, _) = bucket(&facts, &[0, 1, 2, 3], WINDOW_S);
        let keys_and_members: Vec<(&str, &[usize])> = buckets
            .iter()
            .map(|b| (b.key.as_str(), b.members.as_slice()))
            .collect();
        assert_eq!(
            keys_and_members,
            [
                (
                    "a|Canon EOS 2000D|EF-S18-55mm f/3.5-5.6 III|18.0@0",
                    &[0, 2][..]
                ),
                (
                    "b|Canon EOS 2000D|EF-S18-55mm f/3.5-5.6 III|18.0@1",
                    &[1, 3][..]
                ),
            ]
        );
    }

    #[test]
    fn missing_camera_fields_show_as_question_marks() {
        let file = FileFacts {
            path: "a/0.jpg".into(),
            exif: Some(ExifData {
                capture_time: Some(at(0)),
                ..ExifData::default()
            }),
            mtime_ms: 0,
        };
        let (buckets, warnings) = bucket(&[file], &[0], WINDOW_S);
        assert_eq!(buckets[0].key, "a|?|?|?@0");
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn root_folder_shows_as_a_dot() {
        let facts = [photo("0.jpg", 0), no_exif_at_root()];
        let (buckets, _) = bucket(&facts, &[0, 1], WINDOW_S);
        let keys: Vec<&str> = buckets.iter().map(|b| b.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                ".|Canon EOS 2000D|EF-S18-55mm f/3.5-5.6 III|18.0@0",
                ".|no-exif"
            ]
        );
    }

    fn no_exif_at_root() -> FileFacts {
        FileFacts {
            path: "1.jpg".into(),
            exif: None,
            mtime_ms: 0,
        }
    }

    #[test]
    fn ids_index_facts_and_may_be_unsorted_or_sparse() {
        let mut facts: Vec<FileFacts> = (0..4).map(|i| photo(&format!("a/{i}.jpg"), i)).collect();
        // Not in `ids`, so it must not warn.
        exif_mut(&mut facts[0]).capture_time = None;
        let (buckets, warnings) = bucket(&facts, &[3, 1], WINDOW_S);
        let members: Vec<&[usize]> = buckets.iter().map(|b| b.members.as_slice()).collect();
        assert_eq!(members, [&[1, 3][..]]);
        assert!(warnings.is_empty(), "{warnings:?}");
    }
}
