//! Vendor MakerNote parsers: the bracket fields that Canon, Nikon, Sony and
//! Fujifilm record in their notes.
//!
//! Every parser is lenient: a note it cannot read gives `None`, never `Err`,
//! so the file falls back to the standard EXIF tags.

mod canon;
mod fujifilm;
mod nikon;
mod sony;

use super::tiff::Ifd;
use super::{BracketInfo, NoteCtx};

/// The camera makers whose MakerNote we read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Vendor {
    Canon,
    Fujifilm,
    Nikon,
    Sony,
}

impl Vendor {
    /// The vendor of an EXIF Make, by its lowercase prefix; `None` for any other make.
    pub(crate) fn from_make(make: &str) -> Option<Self> {
        let make = make.to_ascii_lowercase();
        [
            ("canon", Self::Canon),
            ("fujifilm", Self::Fujifilm),
            ("nikon", Self::Nikon),
            ("sony", Self::Sony),
        ]
        .into_iter()
        .find_map(|(prefix, vendor)| make.starts_with(prefix).then_some(vendor))
    }
}

/// The bracket fields of the note in `ctx`, read by the parser for `make`.
/// `None` for an unknown make, or a note without usable bracket fields.
pub(crate) fn parse(make: &str, ctx: &NoteCtx) -> Option<BracketInfo> {
    match Vendor::from_make(make)? {
        Vendor::Canon => canon::canon(ctx),
        Vendor::Fujifilm => fujifilm::fujifilm(ctx),
        Vendor::Nikon => nikon::nikon(ctx),
        Vendor::Sony => sony::sony(ctx),
    }
}

/// The first element of a BYTE, SHORT or LONG value that fits in a u16.
/// Absent when the tag is missing, mistyped, too large or unreadable.
fn u16_field(note: &Ifd, tag: u16) -> Option<u16> {
    note.uint(tag)
        .ok()
        .flatten()
        .and_then(|v| u16::try_from(v).ok())
}

/// The bracket that [`crate::exif::read_exif`] reports for a JPEG whose EXIF
/// has Make `make`, the ExifIFD entries `exif` and the MakerNote `note`. Runs
/// in both outer byte orders, which must agree.
#[cfg(test)]
fn read_jpeg_bracket(
    make: &str,
    exif: &[(u16, photostacker_testkit::Value)],
    note: Option<photostacker_testkit::MakerNoteSpec>,
) -> Option<BracketInfo> {
    use crate::model::FileKind;
    use photostacker_testkit::{TiffSpec, Value, jpeg_with_exif};

    let [le, be] = [true, false].map(|little_endian| {
        let tiff = TiffSpec {
            little_endian,
            ifd0: vec![(0x010F, Value::Ascii(make.into()))],
            exif: exif.to_vec(),
            maker_note: note.clone(),
        }
        .build();
        let img = image::RgbImage::from_pixel(8, 8, image::Rgb([90, 120, 150]));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.jpg");
        std::fs::write(&path, jpeg_with_exif(&img, &tiff, 90)).unwrap();
        crate::exif::read_exif(&path, FileKind::Jpeg)
            .unwrap()
            .unwrap()
            .bracket
    });
    assert_eq!(le, be, "{make}");
    le
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exif::BracketMode;
    use photostacker_testkit::{TiffSpec, Value};

    #[test]
    fn vendor_is_chosen_by_lowercase_make_prefix() {
        let cases = [
            ("Canon", Some(Vendor::Canon)),
            ("CANON INC.", Some(Vendor::Canon)),
            ("FUJIFILM", Some(Vendor::Fujifilm)),
            ("Fujifilm Corporation", Some(Vendor::Fujifilm)),
            ("NIKON CORPORATION", Some(Vendor::Nikon)),
            ("Nikon", Some(Vendor::Nikon)),
            ("SONY", Some(Vendor::Sony)),
            ("Sony Ericsson", Some(Vendor::Sony)),
            ("Leica Camera AG", None),
            ("OLYMPUS", None),
            ("Fuji", None),
            (" Canon", None),
            ("", None),
        ];
        for (make, expected) in cases {
            assert_eq!(Vendor::from_make(make), expected, "{make:?}");
        }
    }

    #[test]
    fn parse_dispatches_on_make() {
        // A Canon note IFD (as a whole TIFF block) with FileInfo BracketMode AEB.
        let mut file_info = vec![0i16; 6];
        file_info[0] = 12;
        file_info[3] = 1;
        file_info[5] = 2;
        let tiff = TiffSpec {
            little_endian: true,
            ifd0: vec![(0x0093, Value::SShort(file_info))],
            exif: Vec::new(),
            maker_note: None,
        }
        .build();
        let ctx = NoteCtx {
            tiff: &tiff,
            little_endian: true,
            ifd_offset: 8,
            base: 0,
        };
        let canon = parse("Canon", &ctx).unwrap();
        assert_eq!(canon.mode, BracketMode::Exposure);
        assert_eq!(canon.shot_number, Some(2));
        assert_eq!(parse("Leica", &ctx), None);
        assert_eq!(parse("", &ctx), None);
        // A Canon note has none of the other vendors' bracket tags.
        for make in ["NIKON CORPORATION", "SONY", "FUJIFILM"] {
            assert_eq!(parse(make, &ctx), None, "{make}");
        }
    }
}
