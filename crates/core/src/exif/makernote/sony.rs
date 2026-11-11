//! Sony MakerNote bracket fields, from the `SONY DSC \0\0\0` note or the
//! header-less IFD some models write (the caller's [`NoteCtx`] has already
//! resolved the framing).
//!
//! Only unencrypted tags are read:
//! - 0xB049 ReleaseMode: 0 normal, 2 continuous, 5 exposure bracketing, 6
//!   white balance bracketing, 8 DRO bracketing, 65535 n/a.
//! - 0xB04A SequenceNumber: 0 single, 65535 n/a, else the shot's position.
//!
//! The enciphered blocks (0x2010 and the 0x9xxx tags) are never parsed. No
//! unencrypted tag carries the shot's EV, so `value_ev` is always `None`.

use super::u16_field;
use crate::exif::tiff::Ifd;
use crate::exif::{BracketInfo, BracketMode, NoteCtx};

const TAG_RELEASE_MODE: u16 = 0xB049;
const TAG_SEQUENCE_NUMBER: u16 = 0xB04A;

/// SequenceNumber values that are not a position: "single" and "n/a".
const SEQUENCE_SINGLE: u16 = 0;
const SEQUENCE_NA: u16 = 65535;

/// The bracket fields of a Sony note.
///
/// - ReleaseMode 5 → Exposure; 6 or 8 → Other; 0 or 2 → Off.
/// - `shot_number` is SequenceNumber when the mode is not Off and the value
///   is not 0 or 65535.
/// - Off carries neither a shot number nor a value.
///
/// `None` when ReleaseMode is absent, 65535 (n/a) or any undocumented value,
/// or the IFD cannot be read, so the standard-EXIF fallback applies.
pub(crate) fn sony(ctx: &NoteCtx) -> Option<BracketInfo> {
    let note = Ifd::parse(
        ctx.tiff,
        ctx.little_endian,
        ctx.ifd_offset,
        ctx.base,
        "Sony MakerNote",
    )
    .ok()?;
    let mode = match u16_field(&note, TAG_RELEASE_MODE)? {
        0 | 2 => BracketMode::Off,
        5 => BracketMode::Exposure,
        6 | 8 => BracketMode::Other,
        _ => return None,
    };
    let shot_number = match mode {
        BracketMode::Off => None,
        _ => u16_field(&note, TAG_SEQUENCE_NUMBER)
            .filter(|&n| n != SEQUENCE_SINGLE && n != SEQUENCE_NA),
    };
    Some(BracketInfo {
        mode,
        shot_number,
        value_ev: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exif::makernote::read_jpeg_bracket;
    use crate::exif::{BracketMode, MakerNoteSource, decode, note_ctx};
    use photostacker_testkit::{MakerNoteSpec, NoteStyle, TiffSpec, Value};

    const MAKE: &str = "SONY";
    const RELEASE_MODE: u16 = 0xB049;
    const SEQUENCE_NUMBER: u16 = 0xB04A;

    fn entries(release_mode: Option<u16>, sequence: Option<u16>) -> Vec<(u16, Value)> {
        let mut entries = Vec::new();
        entries.extend(release_mode.map(|m| (RELEASE_MODE, Value::Short(vec![m]))));
        entries.extend(sequence.map(|n| (SEQUENCE_NUMBER, Value::Short(vec![n]))));
        entries
    }

    fn note(style: NoteStyle, entries: Vec<(u16, Value)>) -> MakerNoteSpec {
        MakerNoteSpec { style, entries }
    }

    /// The bracket of a JPEG with a `SONY DSC` note with `entries`.
    fn read(entries: Vec<(u16, Value)>) -> Option<BracketInfo> {
        read_jpeg_bracket(MAKE, &[], Some(note(NoteStyle::SonyDsc, entries)))
    }

    fn read_modes(release_mode: Option<u16>, sequence: Option<u16>) -> Option<BracketInfo> {
        read(entries(release_mode, sequence))
    }

    fn info(mode: BracketMode, shot_number: Option<u16>) -> BracketInfo {
        BracketInfo {
            mode,
            shot_number,
            value_ev: None,
        }
    }

    #[test]
    fn sony_exposure_bracketing_with_sequence() {
        assert_eq!(
            read_modes(Some(5), Some(2)),
            Some(info(BracketMode::Exposure, Some(2)))
        );
        // White balance (6) and DRO (8) bracketing are Other.
        for mode in [6, 8] {
            assert_eq!(
                read_modes(Some(mode), Some(3)),
                Some(info(BracketMode::Other, Some(3))),
                "{mode}"
            );
        }
    }

    #[test]
    fn sequence_sentinels_are_no_shot_number() {
        // 0 is "single", 65535 "n/a"; a missing or mistyped tag is absent.
        for sequence in [Some(0), Some(65535), None] {
            assert_eq!(
                read_modes(Some(5), sequence),
                Some(info(BracketMode::Exposure, None)),
                "{sequence:?}"
            );
        }
        let mistyped = vec![
            (RELEASE_MODE, Value::Short(vec![5])),
            (SEQUENCE_NUMBER, Value::Ascii("2".into())),
        ];
        assert_eq!(read(mistyped), Some(info(BracketMode::Exposure, None)));
    }

    #[test]
    fn sony_continuous_is_off() {
        // 0 normal, 2 continuous: Off, whatever the sequence number says.
        for mode in [0, 2] {
            assert_eq!(
                read_modes(Some(mode), Some(3)),
                Some(info(BracketMode::Off, None)),
                "{mode}"
            );
        }
        // Off is a decision: ExposureMode 2 does not override it.
        let exposure_mode = [(0xA402, Value::Short(vec![2]))];
        let off = note(NoteStyle::SonyDsc, entries(Some(2), Some(1)));
        assert_eq!(
            read_jpeg_bracket(MAKE, &exposure_mode, Some(off)),
            Some(info(BracketMode::Off, None))
        );
    }

    #[test]
    fn sony_unknown_release_mode_returns_none() {
        // 65535 is "n/a"; other values are not documented bracket modes.
        for mode in [Some(65535), None, Some(1), Some(3), Some(7), Some(9)] {
            assert_eq!(read_modes(mode, Some(2)), None, "{mode:?}");
        }
        let mistyped = vec![(RELEASE_MODE, Value::Ascii("5".into()))];
        assert_eq!(read(mistyped), None);
        // So the ExposureMode fallback applies.
        let exif = [
            (0xA402, Value::Short(vec![2])),
            (0x9204, Value::SRational(vec![(-1, 1)])),
        ];
        let unknown = note(NoteStyle::SonyDsc, entries(Some(65535), Some(2)));
        assert_eq!(
            read_jpeg_bracket(MAKE, &exif, Some(unknown)),
            Some(BracketInfo {
                mode: BracketMode::Exposure,
                shot_number: None,
                value_ev: Some(-1.0),
            })
        );
    }

    #[test]
    fn sony_headerless_ifd() {
        // Some models write the bare IFD, with TIFF-relative offsets: the
        // framing the testkit calls `CanonIfd`.
        let read_bare = |release_mode, sequence| {
            let bare = note(NoteStyle::CanonIfd, entries(release_mode, sequence));
            read_jpeg_bracket(MAKE, &[], Some(bare))
        };
        assert_eq!(
            read_bare(Some(5), Some(2)),
            Some(info(BracketMode::Exposure, Some(2)))
        );
        assert_eq!(
            read_bare(Some(2), Some(2)),
            Some(info(BracketMode::Off, None))
        );
        assert_eq!(read_bare(Some(65535), Some(2)), None);
    }

    #[test]
    fn encrypted_blocks_are_ignored() {
        // Enciphered blobs: 0x2010 and the 0x9xxx tags.
        let junk: Vec<u8> = (0u8..=255).cycle().skip(3).take(512).collect();
        let blobs: Vec<(u16, Value)> = [0x2010, 0x9050, 0x9400, 0x940C]
            .into_iter()
            .map(|tag| (tag, Value::Undefined(junk.clone())))
            .collect();
        let cases = [
            entries(Some(5), Some(2)),
            entries(Some(6), Some(1)),
            entries(Some(2), Some(1)),
            entries(Some(65535), Some(2)),
            entries(None, None),
        ];
        for case in cases {
            let mut with_junk = case.clone();
            with_junk.extend(blobs.clone());
            assert_eq!(read(with_junk), read(case.clone()), "{case:?}");
        }
    }

    #[test]
    fn unreadable_note_is_none() {
        let block = TiffSpec {
            little_endian: false,
            ifd0: vec![(0x010F, Value::Ascii(MAKE.into()))],
            exif: Vec::new(),
            maker_note: Some(note(NoteStyle::SonyDsc, entries(Some(5), Some(2)))),
        }
        .build();
        let Some(MakerNoteSource::InTiff(loc)) = decode(&block).unwrap().maker_note else {
            panic!("no in-TIFF note");
        };
        let ctx = note_ctx(MAKE, &block, loc).unwrap();
        assert_eq!(sony(&ctx), Some(info(BracketMode::Exposure, Some(2))));
        for ifd_offset in [block.len(), usize::MAX] {
            assert_eq!(sony(&NoteCtx { ifd_offset, ..ctx }), None);
        }
    }
}
