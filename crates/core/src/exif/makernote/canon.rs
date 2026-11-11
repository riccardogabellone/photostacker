//! Canon MakerNote bracket fields.
//!
//! Canon keeps them in two binary sub-tables of int16 words. Word 0 of each
//! table is its byte count; the indices below are word indices.
//! - FileInfo (0x0093): word 3 BracketMode, word 4 BracketValue, word 5
//!   BracketShotNumber.
//! - ShotInfo (0x0004): word 16 AutoExposureBracketing, word 17 AEBBracketValue.
//!
//! Neither table alone is reliable on real files, so any "on" signal wins:
//! the EOS 400D keeps FileInfo BracketMode Off while ShotInfo says AEB on,
//! and the 450D and 1D Mk IV keep ShotInfo Off while FileInfo says AEB.
//! FileInfo's BracketValue is not used: the shot's EV comes from ShotInfo's
//! AEBBracketValue.

use crate::exif::tiff::Ifd;
use crate::exif::{BracketInfo, BracketMode, NoteCtx};

const TAG_SHOT_INFO: u16 = 0x0004;
const TAG_FILE_INFO: u16 = 0x0093;

// FileInfo word indices.
const FILE_INFO_BRACKET_MODE: usize = 3;
const FILE_INFO_BRACKET_SHOT_NUMBER: usize = 5;
// ShotInfo word indices.
const SHOT_INFO_AUTO_EXPOSURE_BRACKETING: usize = 16;
const SHOT_INFO_AEB_BRACKET_VALUE: usize = 17;

/// The bracket fields of a Canon note.
///
/// - Mode: Other when FileInfo BracketMode is 2 (FEB), 3 (ISO) or 4 (WB);
///   else Exposure when FileInfo BracketMode is 1 (AEB) or ShotInfo
///   AutoExposureBracketing is non-zero (−1 included); else Off when either
///   field was read and says off (BracketMode 0, AutoExposureBracketing 0).
/// - `shot_number`: FileInfo BracketShotNumber when ≥ 1, else
///   AutoExposureBracketing when it is 1–3.
/// - `value_ev` is AEBBracketValue whenever ShotInfo has it, 0 included:
///   the 0 EV shot of a bracket records 0. It is `None` only when the word
///   was not read.
/// - Off carries neither a shot number nor a value: only a note whose
///   readable fields all say off is *not bracketing*.
///
/// `None` when neither field can be read (a BracketMode outside 0–4 counts as
/// unread), or the IFD cannot be read. A table that cannot be read, or ends
/// before a word, leaves that word absent.
pub(crate) fn canon(ctx: &NoteCtx) -> Option<BracketInfo> {
    let note = Ifd::parse(
        ctx.tiff,
        ctx.little_endian,
        ctx.ifd_offset,
        ctx.base,
        "Canon MakerNote",
    )
    .ok()?;
    let table = |tag| note.int16s(tag).ok().flatten().unwrap_or_default();
    let file_info = table(TAG_FILE_INFO);
    let shot_info = table(TAG_SHOT_INFO);
    let word = |table: &[i16], index: usize| table.get(index).copied();

    let bracket_mode = word(&file_info, FILE_INFO_BRACKET_MODE).filter(|m| (0..=4).contains(m));
    let aeb = word(&shot_info, SHOT_INFO_AUTO_EXPOSURE_BRACKETING);
    let mode = match (bracket_mode, aeb) {
        (Some(2..=4), _) => BracketMode::Other,
        (Some(1), _) => BracketMode::Exposure,
        (_, Some(aeb)) if aeb != 0 => BracketMode::Exposure,
        (None, None) => return None,
        // Every field that was read says off.
        _ => BracketMode::Off,
    };
    if mode == BracketMode::Off {
        return Some(BracketInfo {
            mode,
            shot_number: None,
            value_ev: None,
        });
    }
    // Positions are 1-based: 0 or a negative number is no position.
    let position = |n: i16| u16::try_from(n).ok().filter(|&n| n >= 1);
    let shot_number = word(&file_info, FILE_INFO_BRACKET_SHOT_NUMBER)
        .and_then(position)
        .or_else(|| aeb.filter(|n| (1..=3).contains(n)).and_then(position));
    Some(BracketInfo {
        mode,
        shot_number,
        value_ev: word(&shot_info, SHOT_INFO_AEB_BRACKET_VALUE).map(canon_ev),
    })
}

/// Decodes Canon's EV encoding (ExifTool `Canon::CanonEv`): 1/32 EV steps,
/// where the fractions 0x0C and 0x14 stand for 1/3 and 2/3 EV.
fn canon_ev(raw: i16) -> f32 {
    let (sign, mut v) = if raw < 0 {
        (-1.0, -i32::from(raw))
    } else {
        (1.0, i32::from(raw))
    };
    let frac = v & 0x1f;
    v -= frac;
    let frac = match frac {
        0x0c => 32.0 / 3.0,
        0x14 => 64.0 / 3.0,
        f => f as f32,
    };
    sign * (v as f32 + frac) / 32.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exif::{MakerNoteSource, decode, note_ctx, read_exif};
    use crate::model::FileKind;
    use photostacker_testkit::{MakerNoteSpec, NoteStyle, TiffSpec, Value, cr3_with_cmt};

    const FILE_INFO: u16 = 0x0093;
    const SHOT_INFO: u16 = 0x0004;

    /// A word table of `len` words: word 0 the byte count, then `set`.
    fn table(len: usize, set: &[(usize, i16)]) -> Vec<i16> {
        let mut words = vec![0; len];
        words[0] = i16::try_from(len * 2).unwrap();
        for &(i, v) in set {
            words[i] = v;
        }
        words
    }

    /// The table as a SHORT value, the type real Canon notes use.
    fn short(words: Vec<i16>) -> Value {
        Value::Short(words.into_iter().map(|w| w as u16).collect())
    }

    fn file_info(set: &[(usize, i16)]) -> (u16, Value) {
        (FILE_INFO, short(table(20, set)))
    }

    fn shot_info(set: &[(usize, i16)]) -> (u16, Value) {
        (SHOT_INFO, short(table(34, set)))
    }

    /// A Canon note with `entries` in an outer TIFF block, in both byte orders.
    fn block(little_endian: bool, entries: &[(u16, Value)]) -> Vec<u8> {
        TiffSpec {
            little_endian,
            ifd0: vec![(0x010F, Value::Ascii("Canon".into()))],
            exif: Vec::new(),
            maker_note: Some(MakerNoteSpec {
                style: NoteStyle::CanonIfd,
                entries: entries.to_vec(),
            }),
        }
        .build()
    }

    /// Parses the note in both byte orders; both must agree.
    fn parse(entries: &[(u16, Value)]) -> Option<BracketInfo> {
        let [le, be] = [true, false].map(|little_endian| {
            let block = block(little_endian, entries);
            let decoded = decode(&block).unwrap();
            let Some(MakerNoteSource::InTiff(loc)) = decoded.maker_note else {
                panic!("{:?}", decoded.maker_note);
            };
            canon(&note_ctx("Canon", &block, loc).unwrap())
        });
        assert_eq!(le, be);
        le
    }

    fn info(mode: BracketMode, shot_number: Option<u16>, value_ev: Option<f32>) -> BracketInfo {
        BracketInfo {
            mode,
            shot_number,
            value_ev,
        }
    }

    #[test]
    fn canon_fileinfo_aeb_shot_2() {
        let bracket = parse(&[
            file_info(&[(3, 1), (4, 1), (5, 2)]),
            shot_info(&[(16, 2), (17, -32)]),
        ]);
        assert_eq!(
            bracket,
            Some(info(BracketMode::Exposure, Some(2), Some(-1.0)))
        );

        // SSHORT tables read the same.
        let bracket = parse(&[
            (FILE_INFO, Value::SShort(table(20, &[(3, 1), (5, 3)]))),
            (SHOT_INFO, Value::SShort(table(34, &[(17, 64)]))),
        ]);
        assert_eq!(
            bracket,
            Some(info(BracketMode::Exposure, Some(3), Some(2.0)))
        );
    }

    #[test]
    fn canon_bracket_off_is_not_bracketing() {
        // Off only when every readable field says off; it carries no shot
        // number or value, so Stage 1 bucketing ignores it.
        for shot in [0, 3] {
            let bracket = parse(&[
                file_info(&[(3, 0), (5, shot)]),
                shot_info(&[(16, 0), (17, -32)]),
            ]);
            assert_eq!(bracket, Some(info(BracketMode::Off, None, None)), "{shot}");
        }
        // One readable field that says off is enough.
        let bracket = parse(&[file_info(&[(3, 0), (5, 3)])]);
        assert_eq!(bracket, Some(info(BracketMode::Off, None, None)));
        let bracket = parse(&[file_info(&[(3, 5), (5, 3)]), shot_info(&[(16, 0)])]);
        assert_eq!(bracket, Some(info(BracketMode::Off, None, None)));
    }

    #[test]
    fn canon_400d_fileinfo_off_with_shotinfo_aeb_on_is_exposure() {
        // The real EOS 400D CR2 (raw.pixls.us 1764): FileInfo BracketMode 0
        // and BracketShotNumber 0, but ShotInfo AutoExposureBracketing −1
        // ("On") and AEBBracketValue 0 while EXIF ExposureMode is 2. Any on
        // signal wins; −1 is not a position, so there is no shot number. The
        // recorded 0 is the shot's EV.
        let bracket = parse(&[
            file_info(&[(3, 0), (5, 0)]),
            shot_info(&[(16, -1), (17, 0)]),
        ]);
        assert_eq!(bracket, Some(info(BracketMode::Exposure, None, Some(0.0))));
        // A ShotInfo position fills the shot number when FileInfo has none.
        let bracket = parse(&[
            file_info(&[(3, 0), (5, 0)]),
            shot_info(&[(16, 3), (17, 0x14)]),
        ])
        .unwrap();
        assert_eq!(
            (bracket.mode, bracket.shot_number),
            (BracketMode::Exposure, Some(3))
        );
    }

    #[test]
    fn canon_450d_fileinfo_aeb_with_shotinfo_off_is_exposure() {
        // The real EOS 450D / 1D Mk IV AEB frames (Empa): FileInfo AEB with
        // BracketShotNumber 0, ShotInfo AutoExposureBracketing 0 ("Off"),
        // AEBBracketValue −2 EV.
        let bracket = parse(&[
            file_info(&[(3, 1), (5, 0)]),
            shot_info(&[(16, 0), (17, -64)]),
        ]);
        assert_eq!(bracket, Some(info(BracketMode::Exposure, None, Some(-2.0))));
        // The 0 EV frame of such a set (also seen on a 2000D JPEG): Exposure
        // without a shot number, at the recorded 0 EV.
        let bracket = parse(&[file_info(&[(3, 1), (5, 0)]), shot_info(&[(16, 0), (17, 0)])]);
        assert_eq!(bracket, Some(info(BracketMode::Exposure, None, Some(0.0))));
    }

    #[test]
    fn canon_wb_bracket_is_other() {
        // 2 FEB, 3 ISO, 4 WB; ShotInfo records AEBBracketValue 0.
        for mode in [2, 3, 4] {
            let bracket = parse(&[file_info(&[(3, mode), (5, 1)]), shot_info(&[])]);
            assert_eq!(
                bracket,
                Some(info(BracketMode::Other, Some(1), Some(0.0))),
                "{mode}"
            );
        }
    }

    #[test]
    fn canon_shotinfo_fallback_without_fileinfo() {
        let bracket = parse(&[shot_info(&[(16, 3), (17, 0x0C)])]).unwrap();
        assert_eq!(bracket.mode, BracketMode::Exposure);
        assert_eq!(bracket.shot_number, Some(3));
        assert!((bracket.value_ev.unwrap() - 1.0 / 3.0).abs() < 1e-6);

        let cases = [
            (-1, info(BracketMode::Exposure, None, Some(-1.0))),
            (1, info(BracketMode::Exposure, Some(1), Some(-1.0))),
            (4, info(BracketMode::Exposure, None, Some(-1.0))),
            (0, info(BracketMode::Off, None, None)),
        ];
        for (aeb, expected) in cases {
            let bracket = parse(&[shot_info(&[(16, aeb), (17, -32)])]);
            assert_eq!(bracket, Some(expected), "{aeb}");
        }
    }

    #[test]
    fn unusable_fileinfo_defers_to_shotinfo() {
        // Too short for BracketMode, or a wrong type: ShotInfo gives the mode
        // and, as AutoExposureBracketing 2, the shot number.
        let short_file_info = (FILE_INFO, short(vec![6, 0, 0]));
        for bad in [
            short_file_info,
            (FILE_INFO, Value::Long(vec![40, 0, 0, 1, 0, 1])),
        ] {
            let bracket = parse(&[bad.clone(), shot_info(&[(16, 2)])]);
            assert_eq!(
                bracket,
                Some(info(BracketMode::Exposure, Some(2), Some(0.0))),
                "{bad:?}"
            );
        }
        // An unknown BracketMode decides nothing, but a readable
        // BracketShotNumber ≥ 1 is still the shot's position.
        for mode in [5, -1] {
            let bracket = parse(&[file_info(&[(3, mode), (5, 1)]), shot_info(&[(16, 2)])]);
            assert_eq!(
                bracket,
                Some(info(BracketMode::Exposure, Some(1), Some(0.0))),
                "{mode}"
            );
        }
        // Without ShotInfo, an unknown BracketMode leaves no decision.
        assert_eq!(parse(&[file_info(&[(3, 5), (5, 1)])]), None);
    }

    #[test]
    fn short_or_missing_words_are_absent() {
        // FileInfo ends before the shot number, so ShotInfo's
        // AutoExposureBracketing 2 gives it; ShotInfo ends before
        // AEBBracketValue.
        let bracket = parse(&[
            (FILE_INFO, short(vec![8, 0, 0, 1])),
            (SHOT_INFO, short(table(17, &[(16, 2)]))),
        ]);
        assert_eq!(bracket, Some(info(BracketMode::Exposure, Some(2), None)));
        let bracket = parse(&[
            (FILE_INFO, short(vec![8, 0, 0, 1])),
            (SHOT_INFO, short(table(17, &[]))),
        ]);
        assert_eq!(bracket, Some(info(BracketMode::Exposure, None, None)));
        // A shot number below 1 is no position.
        for shot in [0, -2] {
            let bracket = parse(&[file_info(&[(3, 1), (5, shot)])]);
            assert_eq!(
                bracket,
                Some(info(BracketMode::Exposure, None, None)),
                "{shot}"
            );
        }
        // No bracket tables at all.
        assert_eq!(parse(&[(0x0001, short(table(10, &[(3, 1)])))]), None);
        assert_eq!(parse(&[(FILE_INFO, Value::Short(Vec::new()))]), None);
    }

    #[test]
    fn unreadable_note_is_none() {
        let tiff = block(true, &[file_info(&[(3, 1), (5, 2)])]);
        let ctx = |ifd_offset, base| NoteCtx {
            tiff: &tiff,
            little_endian: true,
            ifd_offset,
            base,
        };
        assert_eq!(canon(&ctx(tiff.len(), 0)), None);
        assert_eq!(canon(&ctx(usize::MAX, 0)), None);
        // Out-of-line tables past the end are absent fields.
        let decoded = decode(&tiff).unwrap();
        let Some(MakerNoteSource::InTiff(loc)) = decoded.maker_note else {
            panic!()
        };
        assert!(canon(&ctx(loc.offset, 0)).is_some());
        assert_eq!(canon(&ctx(loc.offset, usize::MAX)), None);
    }

    #[test]
    fn canon_ev_thirds() {
        assert!((canon_ev(0x0C) - 1.0 / 3.0).abs() < 1e-6);
        assert!((canon_ev(-0x14) + 2.0 / 3.0).abs() < 1e-6);
        assert_eq!(canon_ev(64), 2.0);
        assert_eq!(canon_ev(-32), -1.0);
        assert!((canon_ev(0x2C) - 4.0 / 3.0).abs() < 1e-6);
        assert_eq!(canon_ev(0x10), 0.5);
        assert!(canon_ev(i16::MIN).is_finite());
    }

    #[test]
    fn canon_in_cr3_cmt3() {
        for little_endian in [true, false] {
            let tiff = |ifd0: Vec<(u16, Value)>| {
                TiffSpec {
                    little_endian,
                    ifd0,
                    exif: Vec::new(),
                    maker_note: None,
                }
                .build()
            };
            let cmt1 = tiff(vec![(0x010F, Value::Ascii("Canon".into()))]);
            // ExposureMode 2: the fallback must not override the note.
            let cmt2 = tiff(vec![
                (0x9204, Value::SRational(vec![(1, 3)])),
                (0xA402, Value::Short(vec![2])),
            ]);
            let cmt3 = tiff(vec![
                file_info(&[(3, 1), (5, 2)]),
                shot_info(&[(16, 2), (17, -32)]),
            ]);
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("x.cr3");
            std::fs::write(&path, cr3_with_cmt(&cmt1, &cmt2, &cmt3)).unwrap();
            let e = read_exif(&path, FileKind::Raw).unwrap().unwrap();
            assert_eq!(
                e.bracket,
                Some(info(BracketMode::Exposure, Some(2), Some(-1.0))),
                "{little_endian}"
            );
        }
    }
}
