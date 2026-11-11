//! How paths, sizes, counts, statuses, errors and a photo's exposure read on screen.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use photostacker_core::engine::JobCounts;
use photostacker_core::exif::{BracketInfo, BracketMode};
use photostacker_core::model::{GroupStatus, JobStatus};
use photostacker_core::models::url_host;
use photostacker_core::write::format_ev;
use photostacker_core::{EngineError, Stage};

use crate::controller::MlState;

/// One mebibyte, the unit sizes are shown in.
const MIB: u64 = 1 << 20;

/// What the user reads when a command fails. The failures that come from acting on a
/// screen the engine has already moved past read as plain sentences, with no internal id;
/// any other is the engine's own words (a refusal's, `Invalid`, are worded for the user),
/// as a sentence.
pub fn engine_error_text(error: &EngineError) -> String {
    match error {
        EngineError::WrongState { status, .. } => format!(
            "That job is {} now, so this no longer applies.",
            status_phrase(*status)
        ),
        EngineError::UnknownGroup(_) => {
            "That stack changed while you were deciding. Look again.".to_owned()
        }
        EngineError::UnknownJob(_) => "That job is gone.".to_owned(),
        EngineError::UnknownFile { .. } => "That photo is no longer in the job.".to_owned(),
        other => sentence(&other.to_string()),
    }
}

/// `text` as a sentence: a capital first letter and a full stop at the end, unless it is
/// empty or ends with its own punctuation.
pub fn sentence(text: &str) -> String {
    let mut sentence = capitalised(text.trim_end());
    if !sentence.is_empty() && !sentence.ends_with(['.', '!', '?']) {
        sentence.push('.');
    }
    sentence
}

/// `text` with its first letter upper case: an error's text starts a sentence on screen.
pub fn capitalised(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// A job's status as its chip reads it.
pub fn job_status_text(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Queued => "Queued",
        JobStatus::Scanning => "Scanning",
        JobStatus::ReviewPending => "Review",
        JobStatus::Writing => "Writing",
        JobStatus::Completed => "Done",
        JobStatus::Failed => "Failed",
        JobStatus::Cancelled => "Cancelled",
    }
}

/// A stack's status as its chip, and its row's name, read it.
pub fn group_status_text(status: GroupStatus) -> &'static str {
    match status {
        GroupStatus::Proposed => "Proposed",
        GroupStatus::Confirmed => "Confirmed",
        GroupStatus::Rejected => "Rejected",
        GroupStatus::Written => "Written",
        GroupStatus::Skipped => "Skipped",
        GroupStatus::Failed => "Failed",
    }
}

/// What a running task is doing, as it reads before its count.
pub fn stage_text(stage: Stage) -> &'static str {
    match stage {
        Stage::Discover => "Finding photos",
        Stage::Metadata => "Reading metadata",
        Stage::Hash => "Examining photos",
        Stage::Group => "Grouping",
        Stage::Write => "Writing stacks",
        Stage::Embed => "Visual check",
    }
}

/// `count` and the noun that goes with it: `1 photo`, `8 photos`.
pub fn count(count: usize, one: &str, many: &str) -> String {
    let noun = if count == 1 { one } else { many };
    format!("{count} {noun}")
}

/// What a job holds once scanned: `412 photos · 38 stacks`, and how many photos could
/// not be read when some could not.
pub fn job_totals(counts: &JobCounts) -> String {
    let mut text = format!(
        "{} · {}",
        count(counts.files, "photo", "photos"),
        count(counts.groups(), "stack", "stacks")
    );
    if counts.errors > 0 {
        text.push_str(&format!(" · {} could not be read", counts.errors));
    }
    text
}

/// `done` of `total` as a whole percentage, rounded down, so 100 % means done.
pub fn percent(done: usize, total: usize) -> usize {
    (done.min(total) * 100).checked_div(total).unwrap_or(0)
}

/// A job's status as it reads after "That job is", in the chips' words.
fn status_phrase(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Queued => "queued",
        JobStatus::Scanning => "scanning",
        JobStatus::ReviewPending => "in review",
        JobStatus::Writing => "writing",
        JobStatus::Completed => "done",
        JobStatus::Failed => "failed",
        JobStatus::Cancelled => "cancelled",
    }
}

/// A path as the user knows it: lossy where it is not valid Unicode, and without the
/// Windows verbatim prefix the core's canonical paths carry ([`without_verbatim_prefix`]).
/// For display only: a lossy result may not open.
pub fn display_path(path: &Path) -> String {
    without_verbatim_prefix(&path.to_string_lossy()).into_owned()
}

/// `path` as the desktop's shell takes it, to show it in the file manager: without the
/// Windows verbatim prefix ([`without_verbatim_prefix`]), which the engine's canonical
/// output paths carry and the shell's calls do not take. A path that is not valid
/// Unicode is left as it is.
pub fn openable_path(path: &Path) -> PathBuf {
    match path.to_str() {
        Some(text) => PathBuf::from(without_verbatim_prefix(text).as_ref()),
        None => path.to_path_buf(),
    }
}

/// `text`, a path, without the Windows verbatim prefix: `\\?\C:\a` is `C:\a`, and
/// `\\?\UNC\host\share\a` is `\\host\share\a`; any other text (a volume's verbatim path
/// among them, which has no other form) is left as it is. Plain string work, the same on
/// every platform.
pub fn without_verbatim_prefix(text: &str) -> Cow<'_, str> {
    let Some(rest) = text.strip_prefix(r"\\?\") else {
        return Cow::Borrowed(text);
    };
    if let Some(share) = rest.strip_prefix(r"UNC\") {
        return Cow::Owned(format!(r"\\{share}"));
    }
    let mut chars = rest.chars();
    let drive = chars
        .next()
        .is_some_and(|letter| letter.is_ascii_alphabetic())
        && chars.next() == Some(':');
    Cow::Borrowed(if drive { rest } else { text })
}

/// The last component of a folder's path, such as `trip` for `/photos/trip`, or of a
/// file's, its name; a root, which has none, reads as its whole path.
pub fn folder_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || display_path(path),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// `text` cut to `budget` characters by putting `…` in place of its middle, so both
/// ends stay: a path keeps its root and its folder's name. The end gets the odd
/// character, since a path's end names what it is. `text` is returned whole when it
/// fits; a budget of 1 or less gives the `…` alone.
pub fn truncate_middle(text: &str, budget: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= budget {
        return text.to_owned();
    }
    let kept = budget.saturating_sub(1);
    let head = kept / 2;
    let tail = kept - head;
    let mut truncated: String = chars[..head].iter().collect();
    truncated.push('…');
    truncated.extend(&chars[chars.len() - tail..]);
    truncated
}

/// How far a download of `total` bytes is at `done`, in whole percent, rounded down so
/// 100 means done: 0 for a download of nothing.
pub fn download_percent(done: u64, total: u64) -> u8 {
    // In 128 bits, so no size overflows; at most 100, so the cast keeps it.
    let percent = (u128::from(done.min(total)) * 100)
        .checked_div(u128::from(total))
        .unwrap_or(0);
    percent as u8
}

/// What the dialog asks before the model `name`, of `size` bytes, is downloaded from
/// `urls`: its size, and every host it goes through, in the order the download tries them,
/// each once (a host is tried only when the one before cannot be reached). "Through", not
/// "from": both services redirect a download to file servers of their own.
pub fn download_question(name: &str, size: u64, urls: &[&str]) -> String {
    let mut hosts: Vec<&str> = Vec::new();
    for host in urls.iter().map(|url| url_host(url)) {
        if !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    let mut through = hosts.first().copied().unwrap_or_default().to_owned();
    for host in hosts.iter().skip(1) {
        through.push_str(&format!(", or through {host} if that cannot be reached"));
    }
    format!(
        "Download {name}? {} through {through}. Nothing else is sent or received.",
        mib(size)
    )
}

/// What the ML chip and the status bar say of the visual check.
pub fn ml_state_text(state: &MlState) -> String {
    match state {
        MlState::NotBuilt | MlState::NoRuntime(_) => "ML unavailable".to_owned(),
        MlState::Off => "ML off".to_owned(),
        MlState::Missing(_) => "ML: no model".to_owned(),
        MlState::Downloading { percent, .. } => format!("ML {percent} %"),
        MlState::Ready(_) => "ML ready".to_owned(),
    }
}

/// A size in bytes as whole mebibytes, rounded to the nearest, written `MB` as file
/// managers do: `84 MB`.
pub fn mib(bytes: u64) -> String {
    let rounded = bytes / MIB + u64::from(bytes % MIB >= MIB / 2);
    format!("{rounded} MB")
}

/// The minus sign of a UTC offset, the one exposures are written with.
const MINUS: char = '\u{2212}';

/// A positive `value` rounded to one decimal, a trailing `.0` trimmed: `2.8`, `8`.
fn one_decimal(value: f32) -> String {
    let tenths = (f64::from(value) * 10.0).round() as u64;
    match tenths % 10 {
        0 => (tenths / 10).to_string(),
        tenth => format!("{}.{tenth}", tenths / 10),
    }
}

/// An exposure in EV exactly as a layer name writes it: the core's own formatter
/// (`write::format_ev`, spec §5.6), so the two never drift: `−0.7`, `0`, `+1`.
pub fn ev(value: f32) -> String {
    format_ev(value)
}

/// A stack's exposures, from the lowest to the highest: `EV −2..+2`, or `EV +1` when they
/// are one value as the layer names write it.
pub fn ev_range(lowest: f32, highest: f32) -> String {
    let (low, high) = (ev(lowest), ev(highest));
    if low == high {
        format!("EV {low}")
    } else {
        format!("EV {low}..{high}")
    }
}

/// `value` when it is a positive number: what a camera records of an exposure.
fn positive(value: f32) -> Option<f32> {
    (value.is_finite() && value > 0.0).then_some(value)
}

/// An exposure time as cameras show it: a fraction of a second as `1/250 s` when it is
/// one (within 1 %), else in seconds with one decimal: `0.3 s`, `2 s`.
pub fn exposure_time(seconds: f32) -> Option<String> {
    let seconds = positive(seconds)?;
    let per_second = 1.0 / seconds;
    let whole = per_second.round();
    if seconds < 1.0 && whole >= 2.0 && (per_second - whole).abs() <= 0.01 * whole {
        Some(format!("1/{whole} s"))
    } else {
        Some(format!("{} s", one_decimal(seconds)))
    }
}

/// An aperture: `f/8`, `f/2.8`.
pub fn aperture(f_number: f32) -> Option<String> {
    positive(f_number).map(|f_number| format!("f/{}", one_decimal(f_number)))
}

/// A focal length: `35 mm`, `24.5 mm`.
pub fn focal_length(millimetres: f32) -> Option<String> {
    positive(millimetres).map(|millimetres| format!("{} mm", one_decimal(millimetres)))
}

/// A picture's size in pixels: `6000 × 4000`.
pub fn resolution(width: u32, height: u32) -> String {
    format!("{width} \u{d7} {height}")
}

/// A bit depth: `16-bit`, `8-bit`.
pub fn depth(sixteen_bit: bool) -> &'static str {
    if sixteen_bit { "16-bit" } else { "8-bit" }
}

/// The camera that took a photo: its model, after its make unless the model already
/// starts with it (`Canon EOS 2000D`, not `Canon Canon EOS 2000D`).
pub fn camera(make: Option<&str>, model: Option<&str>) -> Option<String> {
    let make = make.map(str::trim).filter(|make| !make.is_empty());
    let model = model.map(str::trim).filter(|model| !model.is_empty());
    match (make, model) {
        (Some(make), Some(model)) if model.to_lowercase().starts_with(&make.to_lowercase()) => {
            Some(model.to_owned())
        }
        (Some(make), Some(model)) => Some(format!("{make} {model}")),
        (Some(only), None) | (None, Some(only)) => Some(only.to_owned()),
        (None, None) => None,
    }
}

/// A camera's offset from UTC: `UTC+02:00`, `UTC−05:30`, `UTC`.
pub fn utc_offset(minutes: i32) -> String {
    if minutes == 0 {
        return "UTC".to_owned();
    }
    let sign = if minutes < 0 { MINUS } else { '+' };
    let minutes = minutes.unsigned_abs();
    format!("UTC{sign}{:02}:{:02}", minutes / 60, minutes % 60)
}

/// What a shot's bracket fields say: `Exposure bracket, shot 2, −1 EV`; nothing when the
/// camera was not bracketing. A value shows only for an exposure bracket, as in the layer
/// names: other brackets vary something else.
pub fn bracket_text(bracket: &BracketInfo) -> Option<String> {
    let mut text = match bracket.mode {
        BracketMode::Off => return None,
        BracketMode::Exposure => "Exposure bracket".to_owned(),
        BracketMode::Other => "Bracket of another setting".to_owned(),
    };
    if let Some(shot) = bracket.shot_number {
        text.push_str(&format!(", shot {shot}"));
    }
    if bracket.mode == BracketMode::Exposure
        && let Some(value) = bracket.value_ev.filter(|value| value.is_finite())
    {
        text.push_str(&format!(", {} EV", ev(value)));
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use photostacker_core::exif::ExifData;
    use photostacker_core::write::layer_name;

    use super::*;

    #[test]
    fn a_download_and_the_visual_check_read_in_few_words() {
        assert_eq!(download_percent(43, 100), 43);
        assert_eq!(download_percent(999, 1000), 99);
        assert_eq!(download_percent(2000, 1000), 100);
        assert_eq!(download_percent(0, 0), 0);
        assert_eq!(download_percent(u64::MAX, u64::MAX), 100);
        let id = "dinov2-small";
        for (state, text) in [
            (MlState::NotBuilt, "ML unavailable"),
            (MlState::NoRuntime("/x".into()), "ML unavailable"),
            (MlState::Off, "ML off"),
            (MlState::Missing(id), "ML: no model"),
            (MlState::Downloading { id, percent: 43 }, "ML 43 %"),
            (MlState::Ready(id), "ML ready"),
        ] {
            assert_eq!(ml_state_text(&state), text);
        }
    }

    #[test]
    fn the_download_question_names_every_host_once_in_order() {
        let size = 84 << 20;
        let hugging = "https://huggingface.co/Xenova/dinov2-small/resolve/c2bb/onnx/model.onnx";
        let github = "https://github.com/x/photostacker/releases/download/models-v1/s.onnx";
        let mirror = "https://huggingface.co/mirror/dinov2-small/resolve/main/model.onnx";
        assert_eq!(
            download_question("DINOv2 small", size, &[hugging]),
            "Download DINOv2 small? 84 MB through huggingface.co. Nothing else is sent or \
             received."
        );
        assert_eq!(
            download_question("DINOv2 small", size, &[hugging, github]),
            "Download DINOv2 small? 84 MB through huggingface.co, or through github.com if \
             that cannot be reached. Nothing else is sent or received."
        );
        assert_eq!(
            download_question("DINOv2 small", size, &[hugging, mirror]),
            "Download DINOv2 small? 84 MB through huggingface.co. Nothing else is sent or \
             received."
        );
    }

    #[test]
    fn a_reason_starts_a_sentence() {
        assert_eq!(capitalised("another app"), "Another app");
        assert_eq!(capitalised("ébauche"), "Ébauche");
        assert_eq!(capitalised(""), "");
    }

    #[test]
    fn a_refusal_reads_as_a_sentence() {
        let refusal = |text: &str| engine_error_text(&EngineError::Invalid(text.into()));
        assert_eq!(
            refusal("a stack cannot hold photos of two folders"),
            "A stack cannot hold photos of two folders."
        );
        assert_eq!(refusal("already a sentence."), "Already a sentence.");
        assert_eq!(refusal(""), "");
        // A refusal that names a photo keeps its name as it is.
        assert_eq!(
            refusal("the photo b_3.jpg is not in stack 1"),
            "The photo b_3.jpg is not in stack 1."
        );
    }

    #[test]
    fn exposures_read_as_the_layer_names_write_them() {
        // The core's form, which the layer names take (its rules are tested there).
        let exif = |ev: f32| ExifData {
            exposure_bias_ev: Some(ev),
            ..ExifData::default()
        };
        assert_eq!(
            layer_name("a", Some(&exif(-0.7))),
            format!("a ({} EV)", ev(-0.7))
        );
        assert_eq!(ev_range(-2.0 / 3.0, 2.0), "EV \u{2212}0.7..+2");
        assert_eq!(ev_range(1.0, 1.0), "EV +1");
        // One value as the names write it.
        assert_eq!(ev_range(0.96, 1.04), "EV +1");
    }

    #[test]
    fn exposure_times_read_as_cameras_show_them() {
        assert_eq!(exposure_time(0.004).as_deref(), Some("1/250 s"));
        assert_eq!(exposure_time(1.0 / 8000.0).as_deref(), Some("1/8000 s"));
        assert_eq!(exposure_time(1.0 / 3.0).as_deref(), Some("1/3 s"));
        assert_eq!(exposure_time(0.5).as_deref(), Some("1/2 s"));
        assert_eq!(exposure_time(0.3).as_deref(), Some("0.3 s"));
        assert_eq!(exposure_time(0.4).as_deref(), Some("0.4 s"));
        assert_eq!(exposure_time(1.0).as_deref(), Some("1 s"));
        assert_eq!(exposure_time(1.3).as_deref(), Some("1.3 s"));
        assert_eq!(exposure_time(30.0).as_deref(), Some("30 s"));
        assert_eq!(exposure_time(0.0), None);
        assert_eq!(exposure_time(f32::NAN), None);
    }

    #[test]
    fn apertures_focal_lengths_and_sizes_read_plainly() {
        assert_eq!(aperture(8.0).as_deref(), Some("f/8"));
        assert_eq!(aperture(2.8).as_deref(), Some("f/2.8"));
        assert_eq!(aperture(5.66).as_deref(), Some("f/5.7"));
        assert_eq!(aperture(-1.0), None);
        assert_eq!(focal_length(35.0).as_deref(), Some("35 mm"));
        assert_eq!(focal_length(24.5).as_deref(), Some("24.5 mm"));
        assert_eq!(focal_length(f32::INFINITY), None);
        assert_eq!(resolution(6000, 4000), "6000 \u{d7} 4000");
        assert_eq!(depth(true), "16-bit");
        assert_eq!(depth(false), "8-bit");
    }

    #[test]
    fn a_camera_is_named_once() {
        assert_eq!(
            camera(Some("Canon"), Some("Canon EOS 2000D")).as_deref(),
            Some("Canon EOS 2000D")
        );
        assert_eq!(
            camera(Some("NIKON CORPORATION"), Some("NIKON Z 6")).as_deref(),
            Some("NIKON CORPORATION NIKON Z 6")
        );
        assert_eq!(
            camera(Some("SONY"), Some("ILCE-7RM3")).as_deref(),
            Some("SONY ILCE-7RM3")
        );
        assert_eq!(camera(Some("FUJIFILM "), None).as_deref(), Some("FUJIFILM"));
        assert_eq!(camera(Some(""), Some("X-T5")).as_deref(), Some("X-T5"));
        assert_eq!(camera(None, None), None);
    }

    #[test]
    fn offsets_and_brackets_read_plainly() {
        assert_eq!(utc_offset(120), "UTC+02:00");
        assert_eq!(utc_offset(-330), "UTC\u{2212}05:30");
        assert_eq!(utc_offset(0), "UTC");
        let bracket = |mode, shot_number, value_ev| BracketInfo {
            mode,
            shot_number,
            value_ev,
        };
        assert_eq!(
            bracket_text(&bracket(BracketMode::Exposure, Some(2), Some(-1.0))).as_deref(),
            Some("Exposure bracket, shot 2, \u{2212}1 EV")
        );
        assert_eq!(
            bracket_text(&bracket(BracketMode::Exposure, None, Some(0.0))).as_deref(),
            Some("Exposure bracket, 0 EV")
        );
        assert_eq!(
            bracket_text(&bracket(BracketMode::Other, Some(3), Some(1.0))).as_deref(),
            Some("Bracket of another setting, shot 3")
        );
        assert_eq!(
            bracket_text(&bracket(BracketMode::Exposure, Some(1), Some(f32::NAN))).as_deref(),
            Some("Exposure bracket, shot 1")
        );
        assert_eq!(
            bracket_text(&bracket(BracketMode::Off, Some(1), None)),
            None
        );
    }

    #[test]
    fn a_verbatim_windows_path_is_shown_without_its_prefix() {
        assert_eq!(display_path(Path::new(r"\\?\C:\a")), r"C:\a");
        assert_eq!(
            display_path(Path::new(r"\\?\UNC\host\share")),
            r"\\host\share"
        );
        assert_eq!(display_path(Path::new(r"C:\a")), r"C:\a");
        assert_eq!(display_path(Path::new("/photos/trip")), "/photos/trip");
    }

    #[test]
    fn a_verbatim_windows_path_opens_without_its_prefix() {
        let strip = |text: &str| without_verbatim_prefix(text).into_owned();
        assert_eq!(strip(r"\\?\C:\x"), r"C:\x");
        assert_eq!(strip(r"\\?\d:\out\a_3.psd"), r"d:\out\a_3.psd");
        assert_eq!(strip(r"\\?\UNC\server\share\x"), r"\\server\share\x");
        // Anything else is left alone: a volume's verbatim path has no other form.
        let volume = r"\\?\Volume{0123abcd}\x";
        assert_eq!(strip(volume), volume);
        assert_eq!(strip(r"C:\x"), r"C:\x");
        assert_eq!(strip(r"\\server\share\x"), r"\\server\share\x");
        assert_eq!(strip("/photos/trip"), "/photos/trip");
        // The path the file manager is handed.
        assert_eq!(
            openable_path(Path::new(r"\\?\C:\x")),
            PathBuf::from(r"C:\x")
        );
        assert_eq!(
            openable_path(Path::new("/photos/trip")),
            PathBuf::from("/photos/trip")
        );
    }

    #[test]
    fn a_folder_is_named_by_its_last_component() {
        assert_eq!(folder_name(Path::new("/photos/trip")), "trip");
        assert_eq!(folder_name(Path::new("/")), "/");
    }

    #[test]
    fn a_long_text_is_cut_in_the_middle() {
        assert_eq!(truncate_middle("/photos/trip", 12), "/photos/trip");
        assert_eq!(truncate_middle("/photos/trip", 40), "/photos/trip");
        assert_eq!(truncate_middle("abcdefghij", 5), "ab…ij");
        // The end keeps the odd character.
        assert_eq!(truncate_middle("abcdefghij", 6), "ab…hij");
        assert_eq!(truncate_middle("abcdefghij", 2), "…j");
        assert_eq!(truncate_middle("abcdefghij", 1), "…");
        assert_eq!(truncate_middle("abcdefghij", 0), "…");
        assert_eq!(truncate_middle("", 0), "");
        // Characters, not bytes.
        assert_eq!(truncate_middle("éééééüüüüü", 5), "éé…üü");
        let cut = truncate_middle("/home/you/Pictures/2026/Lake trip", 20);
        assert_eq!(cut.chars().count(), 20);
        assert_eq!(cut, "/home/you…/Lake trip");
    }

    #[test]
    fn sizes_read_in_megabytes() {
        assert_eq!(mib(84 * MIB), "84 MB");
        assert_eq!(mib(84 * MIB + MIB / 2 - 1), "84 MB");
        assert_eq!(mib(84 * MIB + MIB / 2), "85 MB");
        assert_eq!(mib(0), "0 MB");
        assert_eq!(mib(u64::MAX), "17592186044416 MB");
    }

    #[test]
    fn totals_count_photos_and_stacks_and_name_the_unreadable() {
        let mut counts = JobCounts {
            files: 412,
            proposed: 30,
            confirmed: 7,
            rejected: 1,
            ..JobCounts::default()
        };
        assert_eq!(job_totals(&counts), "412 photos · 38 stacks");
        counts.errors = 3;
        assert_eq!(
            job_totals(&counts),
            "412 photos · 38 stacks · 3 could not be read"
        );
        let one = JobCounts {
            files: 1,
            written: 1,
            ..JobCounts::default()
        };
        assert_eq!(job_totals(&one), "1 photo · 1 stack");
    }

    #[test]
    fn a_percentage_reaches_100_only_when_done() {
        assert_eq!(percent(41, 100), 41);
        assert_eq!(percent(199, 200), 99);
        assert_eq!(percent(200, 200), 100);
        assert_eq!(percent(0, 0), 0);
        assert_eq!(percent(usize::MAX, 3), 100);
    }

    #[test]
    fn a_command_the_engine_moved_past_reads_as_a_sentence() {
        let wrong_state = EngineError::WrongState {
            job: 3,
            status: JobStatus::Writing,
        };
        assert_eq!(
            engine_error_text(&wrong_state),
            "That job is writing now, so this no longer applies."
        );
        // In the chips' words.
        let done = EngineError::WrongState {
            job: 3,
            status: JobStatus::Completed,
        };
        assert_eq!(
            engine_error_text(&done),
            "That job is done now, so this no longer applies."
        );
        assert_eq!(
            engine_error_text(&EngineError::UnknownGroup(9)),
            "That stack changed while you were deciding. Look again."
        );
        // No internal id on screen.
        assert_eq!(
            engine_error_text(&EngineError::UnknownJob(7)),
            "That job is gone."
        );
        assert_eq!(
            engine_error_text(&EngineError::UnknownFile { job: 7, idx: 3 }),
            "That photo is no longer in the job."
        );
        // Any other is the engine's words, as a sentence.
        assert_eq!(
            engine_error_text(&EngineError::Stopped),
            "The engine has stopped."
        );
    }
}
