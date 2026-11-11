//! Fujifilm MakerNote bracket fields, from the `FUJIFILM` note (always
//! little-endian, value offsets from the note start; the caller's
//! [`NoteCtx`] has already resolved it).
//!
//! - 0x1100 AutoBracketing: 0 off, 1 on (exposure), 2 flash/no-flash, 6 pixel
//!   shift.
//! - 0x1101 SequenceNumber: the shot's position, 0 when there is none.
//!
//! The note records no per-shot EV, so `value_ev` is always `None`.

use super::u16_field;
use crate::exif::tiff::Ifd;
use crate::exif::{BracketInfo, BracketMode, NoteCtx};

const TAG_AUTO_BRACKETING: u16 = 0x1100;
const TAG_SEQUENCE_NUMBER: u16 = 0x1101;

/// The bracket fields of a Fujifilm note.
///
/// - AutoBracketing 0 → Off; 1 → Exposure; 2, 6 or any other value → Other.
/// - `shot_number` is SequenceNumber when the mode is not Off and the value
///   is not 0.
/// - Off carries neither a shot number nor a value.
///
/// `None` when AutoBracketing is absent or unusable, or the IFD cannot be
/// read, so the standard-EXIF fallback applies.
pub(crate) fn fujifilm(ctx: &NoteCtx) -> Option<BracketInfo> {
    let note = Ifd::parse(
        ctx.tiff,
        ctx.little_endian,
        ctx.ifd_offset,
        ctx.base,
        "Fujifilm MakerNote",
    )
    .ok()?;
    let mode = match u16_field(&note, TAG_AUTO_BRACKETING)? {
        0 => BracketMode::Off,
        1 => BracketMode::Exposure,
        _ => BracketMode::Other,
    };
    let shot_number = match mode {
        BracketMode::Off => None,
        _ => u16_field(&note, TAG_SEQUENCE_NUMBER).filter(|&n| n != 0),
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
    use crate::exif::{BracketMode, MakerNoteSource, decode, note_ctx, read_exif};
    use crate::model::FileKind;
    use photostacker_testkit::{
        MakerNoteSpec, NoteStyle, TiffSpec, Value, jpeg_with_exif, raf_with_jpeg,
    };

    const MAKE: &str = "FUJIFILM";
    const AUTO_BRACKETING: u16 = 0x1100;
    const SEQUENCE_NUMBER: u16 = 0x1101;

    fn entries(auto_bracketing: Option<u16>, sequence: Option<u16>) -> Vec<(u16, Value)> {
        let mut entries = Vec::new();
        entries.extend(auto_bracketing.map(|m| (AUTO_BRACKETING, Value::Short(vec![m]))));
        entries.extend(sequence.map(|n| (SEQUENCE_NUMBER, Value::Short(vec![n]))));
        entries
    }

    fn note(entries: Vec<(u16, Value)>) -> MakerNoteSpec {
        MakerNoteSpec {
            style: NoteStyle::FujifilmHeader,
            entries,
        }
    }

    /// The bracket of a JPEG with a Fujifilm note with `entries`; the note is
    /// little-endian inside either outer byte order.
    fn read(entries: Vec<(u16, Value)>) -> Option<BracketInfo> {
        read_jpeg_bracket(MAKE, &[], Some(note(entries)))
    }

    fn read_modes(auto_bracketing: Option<u16>, sequence: Option<u16>) -> Option<BracketInfo> {
        read(entries(auto_bracketing, sequence))
    }

    fn info(mode: BracketMode, shot_number: Option<u16>) -> BracketInfo {
        BracketInfo {
            mode,
            shot_number,
            value_ev: None,
        }
    }

    #[test]
    fn fuji_auto_bracketing_on_with_sequence() {
        assert_eq!(
            read_modes(Some(1), Some(3)),
            Some(info(BracketMode::Exposure, Some(3)))
        );
        // 0 is no position; a missing or mistyped SequenceNumber is absent.
        assert_eq!(
            read_modes(Some(1), Some(0)),
            Some(info(BracketMode::Exposure, None))
        );
        assert_eq!(
            read_modes(Some(1), None),
            Some(info(BracketMode::Exposure, None))
        );
        let mistyped = vec![
            (AUTO_BRACKETING, Value::Short(vec![1])),
            (SEQUENCE_NUMBER, Value::Ascii("3".into())),
        ];
        assert_eq!(read(mistyped), Some(info(BracketMode::Exposure, None)));
    }

    #[test]
    fn fuji_off() {
        assert_eq!(
            read_modes(Some(0), Some(3)),
            Some(info(BracketMode::Off, None))
        );
        // Off is a decision: ExposureMode 2 does not override it.
        let exposure_mode = [(0xA402, Value::Short(vec![2]))];
        assert_eq!(
            read_jpeg_bracket(MAKE, &exposure_mode, Some(note(entries(Some(0), None)))),
            Some(info(BracketMode::Off, None))
        );
    }

    #[test]
    fn fuji_pixel_shift_is_other() {
        // 2 flash/no-flash, 6 pixel shift, and any undocumented value.
        for mode in [2, 6, 3, 65535] {
            assert_eq!(
                read_modes(Some(mode), Some(2)),
                Some(info(BracketMode::Other, Some(2))),
                "{mode}"
            );
        }
    }

    #[test]
    fn fuji_missing_auto_bracketing_returns_none() {
        assert_eq!(read_modes(None, Some(3)), None);
        assert_eq!(
            read(vec![(AUTO_BRACKETING, Value::Ascii("1".into()))]),
            None
        );
        // So the ExposureMode fallback applies.
        let exif = [
            (0xA402, Value::Short(vec![2])),
            (0x9204, Value::SRational(vec![(1, 3)])),
        ];
        assert_eq!(
            read_jpeg_bracket(MAKE, &exif, Some(note(entries(None, Some(3))))),
            Some(BracketInfo {
                mode: BracketMode::Exposure,
                shot_number: None,
                value_ev: Some((1.0f64 / 3.0) as f32),
            })
        );
    }

    #[test]
    fn fuji_note_offsets_are_note_relative() {
        // SHORT[3] values do not fit the entry: they sit after the IFD, at
        // offsets that count from the note start.
        let out_of_line = vec![
            (AUTO_BRACKETING, Value::Short(vec![1, 0, 0])),
            (SEQUENCE_NUMBER, Value::Short(vec![2, 0, 0])),
        ];
        let expected = Some(info(BracketMode::Exposure, Some(2)));
        assert_eq!(read(out_of_line.clone()), expected);

        for little_endian in [true, false] {
            let block = TiffSpec {
                little_endian,
                ifd0: vec![(0x010F, Value::Ascii(MAKE.into()))],
                exif: Vec::new(),
                maker_note: Some(note(out_of_line.clone())),
            }
            .build();
            let Some(MakerNoteSource::InTiff(loc)) = decode(&block).unwrap().maker_note else {
                panic!("no in-TIFF note");
            };
            let ctx = note_ctx(MAKE, &block, loc).unwrap();
            assert_eq!(ctx.base, loc.offset);
            assert_eq!(fujifilm(&ctx), expected, "{little_endian}");
            // Read from the TIFF header instead, the values are other bytes.
            assert_ne!(fujifilm(&NoteCtx { base: 0, ..ctx }), expected);
            // An unreadable AutoBracketing is an absent one; so is an
            // unreadable IFD.
            assert_eq!(
                fujifilm(&NoteCtx {
                    base: usize::MAX,
                    ..ctx
                }),
                None
            );
            for ifd_offset in [block.len(), usize::MAX] {
                assert_eq!(fujifilm(&NoteCtx { ifd_offset, ..ctx }), None);
            }
        }
    }

    #[test]
    fn fuji_in_raf() {
        for little_endian in [true, false] {
            let tiff = TiffSpec {
                little_endian,
                ifd0: vec![(0x010F, Value::Ascii(MAKE.into()))],
                exif: Vec::new(),
                maker_note: Some(note(entries(Some(1), Some(2)))),
            }
            .build();
            let img = image::RgbImage::from_pixel(8, 8, image::Rgb([90, 120, 150]));
            let raf = raf_with_jpeg(&jpeg_with_exif(&img, &tiff, 90));
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("x.raf");
            std::fs::write(&path, raf).unwrap();
            let e = read_exif(&path, FileKind::Raw).unwrap().unwrap();
            assert_eq!(
                e.bracket,
                Some(info(BracketMode::Exposure, Some(2))),
                "{little_endian}"
            );
        }
    }
}
