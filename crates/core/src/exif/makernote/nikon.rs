//! Nikon MakerNote bracket fields, from the type-3 note (`Nikon\0` + an
//! embedded TIFF header; the caller's [`NoteCtx`] has already resolved it).
//!
//! Only unencrypted tags are read:
//! - 0x0089 ShootingMode, a bitfield: bit 4 exposure bracketing, bit 6 white
//!   balance bracketing, bit 8 Active D-Lighting bracketing.
//! - 0x0019 ExposureBracketValue (SRATIONAL, EV).
//!
//! The shot's position in the sequence lives in ShotInfo (0x0091), which is
//! enciphered: it is never parsed, so `shot_number` is always `None`.

use super::u16_field;
use crate::exif::tiff::Ifd;
use crate::exif::{BracketInfo, BracketMode, NoteCtx};

const TAG_EXPOSURE_BRACKET_VALUE: u16 = 0x0019;
const TAG_SHOOTING_MODE: u16 = 0x0089;

// ShootingMode bits.
const EXPOSURE_BRACKETING: u16 = 1 << 4;
const WB_BRACKETING: u16 = 1 << 6;
const ADL_BRACKETING: u16 = 1 << 8;

/// The bracket fields of a Nikon note.
///
/// - ShootingMode bit 4 → Exposure; else bit 6 or bit 8 → Other; else Off.
/// - `value_ev` is ExposureBracketValue, when the mode is not Off.
/// - Off carries neither a shot number nor a value.
///
/// `None` when ShootingMode is absent or unusable, or the IFD cannot be read,
/// so the standard-EXIF fallback applies.
pub(crate) fn nikon(ctx: &NoteCtx) -> Option<BracketInfo> {
    let note = Ifd::parse(
        ctx.tiff,
        ctx.little_endian,
        ctx.ifd_offset,
        ctx.base,
        "Nikon MakerNote",
    )
    .ok()?;
    let shooting_mode = u16_field(&note, TAG_SHOOTING_MODE)?;
    let mode = if shooting_mode & EXPOSURE_BRACKETING != 0 {
        BracketMode::Exposure
    } else if shooting_mode & (WB_BRACKETING | ADL_BRACKETING) != 0 {
        BracketMode::Other
    } else {
        BracketMode::Off
    };
    let value_ev = match mode {
        BracketMode::Off => None,
        _ => note
            .rational(TAG_EXPOSURE_BRACKET_VALUE)
            .ok()
            .flatten()
            .map(|ev| ev as f32),
    };
    Some(BracketInfo {
        mode,
        shot_number: None,
        value_ev,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exif::makernote::read_jpeg_bracket;
    use crate::exif::{BracketMode, MakerNoteSource, decode, note_ctx};
    use photostacker_testkit::{MakerNoteSpec, NoteStyle, TiffSpec, Value};

    const MAKE: &str = "NIKON CORPORATION";
    const EXPOSURE_BRACKET_VALUE: u16 = 0x0019;
    const SHOOTING_MODE: u16 = 0x0089;
    const SHOT_INFO: u16 = 0x0091;

    fn shooting_mode(bits: u16) -> (u16, Value) {
        (SHOOTING_MODE, Value::Short(vec![bits]))
    }

    fn bracket_value(num: i32, den: i32) -> (u16, Value) {
        (EXPOSURE_BRACKET_VALUE, Value::SRational(vec![(num, den)]))
    }

    fn note(entries: Vec<(u16, Value)>) -> MakerNoteSpec {
        MakerNoteSpec {
            style: NoteStyle::NikonType3,
            entries,
        }
    }

    /// The bracket of a JPEG carrying a type-3 note with `entries`.
    fn read(entries: Vec<(u16, Value)>) -> Option<BracketInfo> {
        read_jpeg_bracket(MAKE, &[], Some(note(entries)))
    }

    fn info(mode: BracketMode, shot_number: Option<u16>, value_ev: Option<f32>) -> BracketInfo {
        BracketInfo {
            mode,
            shot_number,
            value_ev,
        }
    }

    #[test]
    fn nikon_exposure_bracketing_bit4() {
        // Bit 0 (continuous) plus bit 4 (exposure bracketing).
        assert_eq!(
            read(vec![shooting_mode(0b1_0001), bracket_value(-1, 1)]),
            Some(info(BracketMode::Exposure, None, Some(-1.0)))
        );
        // Bit 4 wins over the WB and D-Lighting bits.
        assert_eq!(
            read(vec![shooting_mode(0b1_0101_0000), bracket_value(2, 3)]),
            Some(info(
                BracketMode::Exposure,
                None,
                Some((2.0f64 / 3.0) as f32)
            ))
        );
        // The 0 EV shot of a bracket keeps its value.
        assert_eq!(
            read(vec![shooting_mode(0b1_0000), bracket_value(0, 1)]),
            Some(info(BracketMode::Exposure, None, Some(0.0)))
        );
        // No (or an unusable) ExposureBracketValue leaves the value absent.
        for value in [
            None,
            Some((EXPOSURE_BRACKET_VALUE, Value::Ascii("-1".into()))),
            Some(bracket_value(-1, 0)),
        ] {
            let mut entries = vec![shooting_mode(0b1_0000)];
            entries.extend(value.clone());
            assert_eq!(
                read(entries),
                Some(info(BracketMode::Exposure, None, None)),
                "{value:?}"
            );
        }
    }

    #[test]
    fn nikon_wb_bracketing_is_other() {
        // Bit 6 (WB) or bit 8 (D-Lighting), alone or with other drive bits.
        for bits in [0b100_0000, 0b1_0000_0000, 0b1_0100_0001] {
            assert_eq!(
                read(vec![shooting_mode(bits), bracket_value(0, 1)]),
                Some(info(BracketMode::Other, None, Some(0.0))),
                "{bits:#b}"
            );
            assert_eq!(
                read(vec![shooting_mode(bits)]),
                Some(info(BracketMode::Other, None, None)),
                "{bits:#b}"
            );
        }
    }

    #[test]
    fn nikon_no_bracketing_is_off() {
        // Single frame; continuous; every non-bracket bit (0-3, 5, 7) at once.
        for bits in [0, 0b1, 0b1010_1111] {
            assert_eq!(
                read(vec![shooting_mode(bits), bracket_value(-1, 1)]),
                Some(info(BracketMode::Off, None, None)),
                "{bits:#b}"
            );
        }
        // Off is a decision: ExposureMode 2 does not override it.
        let exposure_mode = [(0xA402, Value::Short(vec![2]))];
        assert_eq!(
            read_jpeg_bracket(MAKE, &exposure_mode, Some(note(vec![shooting_mode(0b1)]))),
            Some(info(BracketMode::Off, None, None))
        );
    }

    #[test]
    fn nikon_missing_shooting_mode_returns_none() {
        assert_eq!(read(vec![bracket_value(-1, 1)]), None);
        assert_eq!(read(Vec::new()), None);
        // A mistyped ShootingMode is an absent one.
        assert_eq!(
            read(vec![
                (SHOOTING_MODE, Value::Ascii("16".into())),
                bracket_value(-1, 1)
            ]),
            None
        );
        // So the ExposureMode fallback applies.
        let exif = [
            (0xA402, Value::Short(vec![2])),
            (0x9204, Value::SRational(vec![(1, 1)])),
        ];
        assert_eq!(
            read_jpeg_bracket(MAKE, &exif, Some(note(vec![bracket_value(-1, 1)]))),
            Some(info(BracketMode::Exposure, None, Some(1.0)))
        );
    }

    #[test]
    fn encrypted_shotinfo_is_ignored() {
        // A ShotInfo-like blob: a version string, then enciphered bytes.
        let mut junk = b"0222".to_vec();
        junk.extend((0u8..=255).cycle().skip(7).take(600));
        let shot_info = (SHOT_INFO, Value::Undefined(junk));
        let cases = [
            vec![shooting_mode(0b1_0001), bracket_value(-1, 1)],
            vec![shooting_mode(0b100_0000)],
            vec![shooting_mode(0)],
            vec![bracket_value(-1, 1)],
        ];
        for entries in cases {
            let mut with_junk = entries.clone();
            with_junk.push(shot_info.clone());
            assert_eq!(read(with_junk), read(entries.clone()), "{entries:?}");
        }
    }

    #[test]
    fn little_endian_embedded_header_is_read() {
        // Some bodies write an `II` TIFF header inside the note.
        let embedded = TiffSpec {
            little_endian: true,
            ifd0: vec![shooting_mode(0b1_0000), bracket_value(-2, 3)],
            exif: Vec::new(),
            maker_note: None,
        }
        .build();
        let mut raw = b"Nikon\0\x02\x10\0\0".to_vec();
        raw.extend_from_slice(&embedded);
        let exif = [(0x927C, Value::Undefined(raw))];
        assert_eq!(
            read_jpeg_bracket(MAKE, &exif, None),
            Some(info(
                BracketMode::Exposure,
                None,
                Some((-2.0f64 / 3.0) as f32)
            ))
        );
    }

    #[test]
    fn unreadable_note_or_value_is_lenient() {
        let block = TiffSpec {
            little_endian: true,
            ifd0: vec![(0x010F, Value::Ascii(MAKE.into()))],
            exif: Vec::new(),
            maker_note: Some(note(vec![shooting_mode(0b1_0000), bracket_value(-1, 1)])),
        }
        .build();
        let Some(MakerNoteSource::InTiff(loc)) = decode(&block).unwrap().maker_note else {
            panic!("no in-TIFF note");
        };
        let ctx = note_ctx(MAKE, &block, loc).unwrap();
        assert_eq!(
            nikon(&ctx),
            Some(info(BracketMode::Exposure, None, Some(-1.0)))
        );
        // The out-of-line SRATIONAL past the end is an absent value.
        let bad_base = NoteCtx {
            base: usize::MAX,
            ..ctx
        };
        assert_eq!(
            nikon(&bad_base),
            Some(info(BracketMode::Exposure, None, None))
        );
        // An IFD that cannot be read gives no bracket.
        for ifd_offset in [block.len(), usize::MAX] {
            assert_eq!(nikon(&NoteCtx { ifd_offset, ..ctx }), None);
        }
    }
}
