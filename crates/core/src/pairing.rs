//! RAW+JPEG pairing: a RAW and a JPEG or HEIC of the same shot (same folder,
//! same file stem, or the same Pixel stem before `.RAW-NN`) count as one
//! photo. One file of each pair is active; the others are passive and stay out
//! of hashing, grouping and writing.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::Path;

use crate::discover::DiscoveredFile;
use crate::model::{FileKind, RawJpegPreference};

/// Pairs the RAW and JPEG/HEIC files of `files` that share a parent folder and
/// a case-insensitive file stem, after [`pair_stem`] cuts a Pixel `.RAW-NN`
/// suffix.
///
/// Such a set needs at least one RAW and at least one JPEG or HEIC; PNG and
/// TIFF files never take part. The active file is the first RAW (by the order
/// of `files`) with [`RawJpegPreference::Raw`], or with
/// [`RawJpegPreference::Jpeg`] the first JPEG, else the first HEIC. Every other
/// member of the set is passive.
///
/// The result has one entry per file: `Some(i)` marks a passive file paired
/// with the active file `files[i]`, `None` an active or unpaired file.
pub fn pair_raw_jpeg(files: &[DiscoveredFile], pref: RawJpegPreference) -> Vec<Option<usize>> {
    // Members of each candidate set, as indices into `files` in input order.
    let mut sets: HashMap<(Option<&Path>, OsString), Vec<usize>> = HashMap::new();
    for (i, file) in files.iter().enumerate() {
        if !matches!(file.kind, FileKind::Raw | FileKind::Jpeg | FileKind::Heic) {
            continue;
        }
        let Some(stem) = file.path.file_stem() else {
            continue;
        };
        sets.entry((file.path.parent(), pair_key(stem)))
            .or_default()
            .push(i);
    }

    let mut paired = vec![None; files.len()];
    for members in sets.values() {
        let first = |kind: FileKind| members.iter().copied().find(|&i| files[i].kind == kind);
        let (Some(raw), Some(still)) = (
            first(FileKind::Raw),
            first(FileKind::Jpeg).or_else(|| first(FileKind::Heic)),
        ) else {
            continue;
        };
        let active = match pref {
            RawJpegPreference::Raw => raw,
            RawJpegPreference::Jpeg => still,
        };
        for &i in members {
            if i != active {
                paired[i] = Some(active);
            }
        }
    }
    paired
}

/// The part of a file stem that pairs. A Google Pixel names the two files of
/// one shot `PXL_….RAW-01.MP.COVER.jpg` and `PXL_….RAW-02.ORIGINAL.dng`, so a
/// stem is cut at a case-insensitive `.RAW-` followed by two ASCII digits and
/// then a `.` or the end, when the part before it is not empty. Any other
/// stem pairs whole.
pub(crate) fn pair_stem(stem: &str) -> &str {
    const MARKER: &[u8] = b".RAW-";
    let bytes = stem.as_bytes();
    let is_pixel_suffix = |at: usize| {
        let rest = &bytes[at..];
        rest.len() >= MARKER.len() + 2
            && rest[..MARKER.len()].eq_ignore_ascii_case(MARKER)
            && rest[MARKER.len()..MARKER.len() + 2]
                .iter()
                .all(u8::is_ascii_digit)
            && rest.get(MARKER.len() + 2).is_none_or(|&next| next == b'.')
    };
    // A found `at` holds an ASCII `.`, so slicing there is on a char boundary.
    match (1..bytes.len()).find(|&at| is_pixel_suffix(at)) {
        Some(at) => &stem[..at],
        None => stem,
    }
}

/// The case-insensitive form of the part of a file stem that pairs
/// ([`pair_stem`]). A stem that is not valid Unicode is kept as is, so it only
/// pairs with an identical stem.
fn pair_key(stem: &OsStr) -> OsString {
    match stem.to_str() {
        Some(stem) => pair_stem(stem).to_lowercase().into(),
        None => stem.to_os_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::SystemTime;

    /// A discovered file at `rel` (written with `/`), built one component at a
    /// time so the path is platform-native. Only path and kind matter here.
    fn file(rel: &str, kind: FileKind) -> DiscoveredFile {
        DiscoveredFile {
            path: rel.split('/').collect::<PathBuf>(),
            kind,
            size: 1,
            mtime: SystemTime::UNIX_EPOCH,
            fingerprint: String::from("0"),
        }
    }

    #[test]
    fn raw_preferred_by_default() {
        let files = [
            file("IMG_1.CR2", FileKind::Raw),
            file("IMG_1.jpg", FileKind::Jpeg),
        ];
        assert_eq!(
            pair_raw_jpeg(&files, RawJpegPreference::Raw),
            [None, Some(0)]
        );
    }

    #[test]
    fn jpeg_preference_flips() {
        let files = [
            file("IMG_1.CR2", FileKind::Raw),
            file("IMG_1.jpg", FileKind::Jpeg),
        ];
        assert_eq!(
            pair_raw_jpeg(&files, RawJpegPreference::Jpeg),
            [Some(1), None]
        );
    }

    #[test]
    fn stems_are_case_insensitive_and_folder_scoped() {
        let files = [
            file("a/X.cr3", FileKind::Raw),
            file("a/x.JPG", FileKind::Jpeg),
            file("b/x.jpg", FileKind::Jpeg),
        ];
        assert_eq!(
            pair_raw_jpeg(&files, RawJpegPreference::Raw),
            [None, Some(0), None]
        );
    }

    #[test]
    fn png_and_tiff_never_pair() {
        let files = [
            file("IMG_1.cr2", FileKind::Raw),
            file("IMG_1.png", FileKind::Png),
            file("IMG_1.tif", FileKind::Tiff),
            file("IMG_2.cr2", FileKind::Raw),
            file("IMG_2.png", FileKind::Png),
            file("IMG_3.png", FileKind::Png),
            file("IMG_3.tiff", FileKind::Tiff),
        ];
        for pref in [RawJpegPreference::Raw, RawJpegPreference::Jpeg] {
            assert_eq!(pair_raw_jpeg(&files, pref), [None; 7], "{pref:?}");
        }
    }

    #[test]
    fn raw_heic_jpeg_triple() {
        let files = [
            file("IMG_1.cr3", FileKind::Raw),
            file("IMG_1.heic", FileKind::Heic),
            file("IMG_1.jpg", FileKind::Jpeg),
        ];
        // RAW active: both stills are passive.
        assert_eq!(
            pair_raw_jpeg(&files, RawJpegPreference::Raw),
            [None, Some(0), Some(0)]
        );
        // JPEG active: the Jpeg wins over the Heic even though the Heic comes first.
        assert_eq!(
            pair_raw_jpeg(&files, RawJpegPreference::Jpeg),
            [Some(2), Some(2), None]
        );
    }

    #[test]
    fn heic_is_active_when_no_jpeg() {
        let files = [
            file("IMG_1.cr3", FileKind::Raw),
            file("IMG_1.HEIF", FileKind::Heic),
        ];
        assert_eq!(
            pair_raw_jpeg(&files, RawJpegPreference::Jpeg),
            [Some(1), None]
        );
    }

    #[test]
    fn first_of_each_side_by_path_order_is_active() {
        let files = [
            file("IMG_1.cr2", FileKind::Raw),
            file("IMG_1.dng", FileKind::Raw),
            file("IMG_1.jpeg", FileKind::Jpeg),
            file("IMG_1.jpg", FileKind::Jpeg),
        ];
        assert_eq!(
            pair_raw_jpeg(&files, RawJpegPreference::Raw),
            [None, Some(0), Some(0), Some(0)]
        );
        assert_eq!(
            pair_raw_jpeg(&files, RawJpegPreference::Jpeg),
            [Some(2), Some(2), None, Some(2)]
        );
    }

    #[test]
    fn one_sided_sets_stay_unpaired() {
        let files = [
            file("IMG_1.cr2", FileKind::Raw),
            file("IMG_1.dng", FileKind::Raw),
            file("IMG_2.heic", FileKind::Heic),
            file("IMG_2.jpg", FileKind::Jpeg),
            file("IMG_3.cr2", FileKind::Raw),
        ];
        for pref in [RawJpegPreference::Raw, RawJpegPreference::Jpeg] {
            assert_eq!(pair_raw_jpeg(&files, pref), [None; 5], "{pref:?}");
        }
    }

    #[test]
    fn pixel_raw_and_jpeg_pair() {
        let files = [
            file("PXL_20250101_120000000.RAW-01.MP.COVER.jpg", FileKind::Jpeg),
            file("PXL_20250101_120000000.RAW-02.ORIGINAL.dng", FileKind::Raw),
        ];
        // RAW is the default preference (`JobConfig`).
        assert_eq!(
            pair_raw_jpeg(&files, RawJpegPreference::Raw),
            [Some(1), None]
        );
    }

    #[test]
    fn pixel_suffix_needs_two_digits() {
        let stem_of = |name: &'static str| Path::new(name).file_stem().unwrap().to_str().unwrap();
        assert_eq!(pair_stem(stem_of("a.RAW-x1.jpg")), "a.RAW-x1");
        assert_eq!(pair_stem(stem_of("a.raw-07.jpg")), "a");
        assert_eq!(pair_stem(stem_of(".RAW-01.jpg")), ".RAW-01");
        // Three digits, or a third character before the next dot, is no
        // Pixel suffix either.
        assert_eq!(pair_stem("a.RAW-012"), "a.RAW-012");
        assert_eq!(pair_stem("a.RAW-01x.MP"), "a.RAW-01x.MP");
    }

    #[test]
    fn empty_input() {
        assert_eq!(pair_raw_jpeg(&[], RawJpegPreference::Raw), []);
    }
}
