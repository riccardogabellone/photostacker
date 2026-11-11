//! EXIF reading: the standard tags that Stage 1 buckets on, plus where the
//! MakerNote sits so the vendor bracket parsers can read it.
//!
//! The reader is our own: the tag set is small, and MakerNote offsets need the
//! enclosing TIFF block, which third-party crates don't expose reliably.

pub(crate) mod container;
mod makernote;
pub(crate) mod tiff;

use std::io;
use std::path::Path;

use chrono::{NaiveDateTime, Timelike};
use serde::{Deserialize, Serialize};

use crate::model::FileKind;
use container::Source;
use makernote::Vendor;
use tiff::Ifd;

/// How a shot took part in a bracketed sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BracketMode {
    Off,
    /// Exposure bracketing (AEB).
    Exposure,
    /// Any other bracketing: white balance, flash, ISO, focus, ...
    Other,
}

/// The bracket fields of one shot. Persisted inside [`ExifData`]'s JSON.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct BracketInfo {
    pub mode: BracketMode,
    /// 1-based position in the sequence, when the camera records it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shot_number: Option<u16>,
    /// The exposure offset of this shot, in EV, as the camera recorded it.
    ///
    /// A recorded 0 is `Some(0.0)`: the 0 EV shot of a bracket (Canon
    /// AEBBracketValue, Nikon ExposureBracketValue). `None` means the camera
    /// did not record it: Sony and Fujifilm notes never do, and Off carries
    /// no value. Without vendor fields, the ExposureMode 2 fallback takes
    /// the exposure bias.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_ev: Option<f32>,
}

/// The normalised EXIF fields of one file.
///
/// The store persists it as JSON, so its serde form is an on-disk format:
/// renaming a field or changing a spelling needs a migration. `None` fields
/// are left out on write and a missing one reads as `None`, so an optional
/// field added later reads as absent from older rows. `orientation` is always
/// written and is required on read, so JSON that is not an `ExifData` (`{}`,
/// another type's object) is rejected instead of reading as "no EXIF".
/// `capture_time` is chrono's ISO form, such as `2026-05-01T10:20:30.250`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ExifData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub make: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lens_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focal_length_mm: Option<f32>,
    /// DateTimeOriginal with SubSecTimeOriginal, in the camera's local time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_time: Option<NaiveDateTime>,
    /// OffsetTimeOriginal: the camera's offset from UTC.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub utc_offset_minutes: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exposure_time_s: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub f_number: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iso: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exposure_bias_ev: Option<f32>,
    /// EXIF ExposureMode: 0 auto, 1 manual, 2 auto bracket.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exposure_mode: Option<u16>,
    /// EXIF Orientation, 1–8; 1 when absent or invalid.
    pub orientation: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bracket: Option<BracketInfo>,
    /// EXIF ColorSpace: 1 sRGB, 0xFFFF uncalibrated; some cameras write 2
    /// for Adobe RGB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub colour_space: Option<u16>,
    /// InteroperabilityIndex, from the Interop IFD: the DCF colour space of
    /// an uncalibrated file, `R98` for sRGB or `R03` for Adobe RGB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interop_index: Option<String>,
}

impl Default for ExifData {
    fn default() -> Self {
        Self {
            make: None,
            model: None,
            lens_model: None,
            focal_length_mm: None,
            capture_time: None,
            utc_offset_minutes: None,
            exposure_time_s: None,
            f_number: None,
            iso: None,
            exposure_bias_ev: None,
            exposure_mode: None,
            orientation: 1,
            bracket: None,
            colour_space: None,
            interop_index: None,
        }
    }
}

impl ExifData {
    /// The capture time as milliseconds since the Unix epoch: the local time
    /// read as UTC, minus the UTC offset when it is known.
    pub fn capture_instant_ms(&self) -> Option<i64> {
        let local_ms = self.capture_time?.and_utc().timestamp_millis();
        let offset_ms = i64::from(self.utc_offset_minutes.unwrap_or(0)) * 60_000;
        Some(local_ms - offset_ms)
    }

    /// Fills every field that `self` lacks with the value of `other`: the
    /// passive file of a RAW+JPEG pair completes its active partner's EXIF
    /// (spec §5.1). A value `self` has always wins.
    ///
    /// - `bracket`: taken whole when `self` has none; otherwise its missing
    ///   `shot_number` and `value_ev` are filled, unless its mode is Off,
    ///   which carries neither.
    /// - `orientation`: an absent tag already reads as 1, so a gap cannot be
    ///   told apart from an upright shot. As a heuristic, `self`'s 1 gives way
    ///   to `other`'s value when that is not 1: the RAW and the camera JPEG of
    ///   one shot share their orientation.
    pub fn fill_missing_from(&mut self, other: &ExifData) {
        fn fill<T: Clone>(own: &mut Option<T>, other: &Option<T>) {
            if own.is_none() {
                own.clone_from(other);
            }
        }
        // Destructured, so a new field cannot be forgotten here.
        let ExifData {
            make,
            model,
            lens_model,
            focal_length_mm,
            capture_time,
            utc_offset_minutes,
            exposure_time_s,
            f_number,
            iso,
            exposure_bias_ev,
            exposure_mode,
            orientation,
            bracket,
            colour_space,
            interop_index,
        } = self;
        if *orientation == 1 {
            *orientation = other.orientation;
        }
        fill(make, &other.make);
        fill(model, &other.model);
        fill(lens_model, &other.lens_model);
        fill(focal_length_mm, &other.focal_length_mm);
        fill(capture_time, &other.capture_time);
        fill(utc_offset_minutes, &other.utc_offset_minutes);
        fill(exposure_time_s, &other.exposure_time_s);
        fill(f_number, &other.f_number);
        fill(iso, &other.iso);
        fill(exposure_bias_ev, &other.exposure_bias_ev);
        fill(exposure_mode, &other.exposure_mode);
        fill(colour_space, &other.colour_space);
        fill(interop_index, &other.interop_index);
        match (bracket, &other.bracket) {
            (own @ None, theirs) => *own = *theirs,
            (Some(own), Some(theirs)) if own.mode != BracketMode::Off => {
                fill(&mut own.shot_number, &theirs.shot_number);
                fill(&mut own.value_ev, &theirs.value_ev);
            }
            _ => {}
        }
    }
}

/// Why the EXIF of a file could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ExifError {
    #[error("I/O error while {what}: {source}")]
    Io {
        what: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("not a JPEG: starts with {found:02X?}")]
    NotJpeg { found: Vec<u8> },
    #[error("malformed JPEG: {what} at offset {offset}")]
    Jpeg { what: &'static str, offset: u64 },
    #[error("truncated JPEG: the data ends inside the {what} at offset {offset}")]
    JpegTruncated { what: &'static str, offset: u64 },
    #[error("not a TIFF block: header {found:02X?}")]
    BadTiffHeader { found: Vec<u8> },
    #[error("{what} at offset {offset} ({len} bytes) is outside the {size}-byte TIFF block")]
    OutOfBounds {
        what: String,
        offset: u64,
        len: u64,
        size: usize,
    },
    #[error("IFD loop: {what} at offset {offset} was already read")]
    IfdLoop { what: &'static str, offset: usize },
    #[error("malformed RAF: {what} at offset {offset}")]
    Raf { what: &'static str, offset: u64 },
    #[error("malformed ISO-BMFF: {what} at offset {offset}")]
    Bmff { what: String, offset: u64 },
    /// libheif could not read a HEIF file, or its `Exif` block is malformed.
    #[error("unreadable HEIF EXIF: {what}")]
    Heif { what: String },
    /// A file whose EXIF needs the whole file in memory is longer than the
    /// memory budget, so it is not read (see `container::read_file`).
    #[error(
        "the file is {len} bytes, too large to read whole (over {} bytes)",
        crate::budget::DECODE_MEMORY_BUDGET
    )]
    TooLarge { len: u64 },
}

/// The MakerNote parsers' view of a MakerNote IFD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NoteCtx<'a> {
    pub tiff: &'a [u8],
    pub little_endian: bool,
    /// Offset of the note's IFD in `tiff`.
    pub ifd_offset: usize,
    /// Added to the value offsets of the note's entries.
    pub base: usize,
}

/// Where the MakerNote blob (tag 0x927C) sits in the TIFF block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MakerNoteLoc {
    offset: usize,
    len: usize,
    /// The byte order of the enclosing TIFF block.
    little_endian: bool,
}

/// Where the MakerNote of a file sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MakerNoteSource<'a> {
    /// The 0x927C value inside the decoded TIFF block. Its framing is
    /// vendor-specific: [`note_ctx`] builds the [`NoteCtx`] from the Make.
    InTiff(MakerNoteLoc),
    /// The CMT3 box of a CR3: a whole TIFF block whose IFD0 is the Canon
    /// MakerNote IFD. The context is complete: the block, its byte order and
    /// IFD0 offset from its header, and base 0.
    Cr3Cmt3(NoteCtx<'a>),
}

/// The decoded EXIF of a file, before any bracket inference. Borrows the file
/// or block it came from.
struct Decoded<'a> {
    data: ExifData,
    maker_note: Option<MakerNoteSource<'a>>,
}

// IFD0 tags.
const TAG_MAKE: u16 = 0x010F;
const TAG_MODEL: u16 = 0x0110;
const TAG_ORIENTATION: u16 = 0x0112;
const TAG_EXIF_IFD: u16 = 0x8769;
// ExifIFD tags.
const TAG_EXPOSURE_TIME: u16 = 0x829A;
const TAG_F_NUMBER: u16 = 0x829D;
const TAG_ISO: u16 = 0x8827;
const TAG_DATE_TIME_ORIGINAL: u16 = 0x9003;
const TAG_OFFSET_TIME_ORIGINAL: u16 = 0x9011;
const TAG_EXPOSURE_BIAS: u16 = 0x9204;
const TAG_FOCAL_LENGTH: u16 = 0x920A;
const TAG_MAKER_NOTE: u16 = 0x927C;
const TAG_SUB_SEC_TIME_ORIGINAL: u16 = 0x9291;
const TAG_COLOUR_SPACE: u16 = 0xA001;
const TAG_INTEROP_IFD: u16 = 0xA005;
const TAG_EXPOSURE_MODE: u16 = 0xA402;
const TAG_LENS_MODEL: u16 = 0xA434;
// Interop IFD tags.
const TAG_INTEROP_INDEX: u16 = 0x0001;

/// EXIF ExposureMode value for auto bracketing.
const EXPOSURE_MODE_AUTO_BRACKET: u16 = 2;

// MakerNote framings.
const SONY_DSC_HEADER: &[u8] = b"SONY DSC \0\0\0";
const FUJIFILM_HEADER: &[u8] = b"FUJIFILM";
const NIKON_HEADER: &[u8] = b"Nikon\0";
/// Where the embedded TIFF header of a Nikon type-3 note starts.
const NIKON_TIFF_HEADER_AT: usize = 10;

/// Reads the EXIF of `path`. `Ok(None)` means the file carries no EXIF (or its
/// container is not supported yet).
///
/// Each standard tag is read on its own: one whose value cannot be parsed
/// leaves only its own field `None`. A container, TIFF header, IFD0 or
/// ExifIFD (pointer or entry table) that cannot be read is an [`ExifError`].
pub fn read_exif(path: &Path, kind: FileKind) -> Result<Option<ExifData>, ExifError> {
    let Some(source) = container::exif_source(path, kind)? else {
        return Ok(None);
    };
    let decoded = match &source {
        Source::Tiff(block) => decode(block)?,
        Source::Cr3(file) => match decode_cr3(file)? {
            Some(decoded) => decoded,
            None => return Ok(None),
        },
    };
    let Decoded {
        mut data,
        maker_note,
    } = decoded;
    // The vendor bracket fields come first; the fallback only fills a gap.
    if let (Some(make), Some(note)) = (data.make.as_deref(), maker_note) {
        let ctx = match (note, &source) {
            (MakerNoteSource::InTiff(loc), Source::Tiff(block)) => note_ctx(make, block, loc),
            (MakerNoteSource::Cr3Cmt3(ctx), _) => Some(ctx),
            // `decode_cr3` never yields an in-TIFF note.
            (MakerNoteSource::InTiff(_), Source::Cr3(_)) => None,
        };
        data.bracket = ctx.and_then(|ctx| makernote::parse(make, &ctx));
    }
    apply_exposure_mode_fallback(&mut data);
    Ok(Some(data))
}

/// The context of the MakerNote at `loc` in `tiff`, by the framing of the
/// vendor of `make` (P = the note's start in `tiff`):
/// - Canon: the bare IFD at P; value offsets from the TIFF header.
/// - Sony: `SONY DSC \0\0\0`, then the IFD at P+12; or a bare IFD at P. Value
///   offsets from the TIFF header.
/// - Fujifilm: `FUJIFILM` + u32 LE IFD offset from P; always little-endian;
///   value offsets from P.
/// - Nikon (type 3): `Nikon\0` + 4 version bytes, then a TIFF header at P+10
///   (either byte order) whose IFD offset and value offsets count from P+10.
///
/// `None` for another make, a header that does not match, or an IFD whose
/// entry count is not inside the note.
fn note_ctx<'a>(make: &str, tiff: &'a [u8], loc: MakerNoteLoc) -> Option<NoteCtx<'a>> {
    let p = loc.offset;
    let end = p.checked_add(loc.len)?;
    let note = tiff.get(p..end)?;
    let outer = loc.little_endian;
    // Every header is checked against `note`, so `p + its length` is in bounds.
    let (ifd_offset, base, little_endian) = match Vendor::from_make(make)? {
        Vendor::Canon => (p, 0, outer),
        Vendor::Sony if note.starts_with(SONY_DSC_HEADER) => (p + SONY_DSC_HEADER.len(), 0, outer),
        Vendor::Sony => (p, 0, outer),
        Vendor::Fujifilm => {
            if !note.starts_with(FUJIFILM_HEADER) {
                return None;
            }
            let ifd = note.get(FUJIFILM_HEADER.len()..FUJIFILM_HEADER.len() + 4)?;
            let ifd = u32::from_le_bytes(ifd.try_into().ok()?);
            (p.checked_add(usize::try_from(ifd).ok()?)?, p, true)
        }
        Vendor::Nikon => {
            if !note.starts_with(NIKON_HEADER) {
                return None;
            }
            let header = tiff::header(note.get(NIKON_TIFF_HEADER_AT..)?).ok()?;
            let base = p + NIKON_TIFF_HEADER_AT;
            (
                base.checked_add(header.ifd0_offset)?,
                base,
                header.little_endian,
            )
        }
    };
    // The IFD's entry count must lie inside the note.
    (ifd_offset.checked_add(2)? <= end).then_some(NoteCtx {
        tiff,
        little_endian,
        ifd_offset,
        base,
    })
}

/// Reads a CR3 file: IFD0 tags from CMT1, Exif tags from CMT2, and the
/// MakerNote context from CMT3. A missing CMT box contributes nothing; `None`
/// when the file has none of them.
fn decode_cr3(file: &[u8]) -> Result<Option<Decoded<'_>>, ExifError> {
    let Some(cmt) = container::cr3_cmt(file)? else {
        return Ok(None);
    };
    let mut data = ExifData::default();
    // Only IFD0 of each block is read: neither a CMT1 ExifIFD pointer nor a
    // CMT2 Interop IFD pointer is followed.
    if let Some(tiff) = cmt.cmt1 {
        read_ifd0_tags(&first_ifd(tiff, "CMT1 IFD0")?, &mut data);
    }
    if let Some(tiff) = cmt.cmt2 {
        read_exif_tags(&first_ifd(tiff, "CMT2 ExifIFD")?, &mut data);
    }
    // Like an unlocatable 0x927C, a CMT3 without a TIFF header is ignored.
    let maker_note = cmt.cmt3.and_then(|tiff| {
        let header = tiff::header(tiff).ok()?;
        Some(MakerNoteSource::Cr3Cmt3(NoteCtx {
            tiff,
            little_endian: header.little_endian,
            ifd_offset: header.ifd0_offset,
            base: 0,
        }))
    });
    Ok(Some(Decoded { data, maker_note }))
}

/// IFD0 of a whole TIFF block; `name` labels errors.
fn first_ifd<'a>(tiff: &'a [u8], name: &'static str) -> Result<Ifd<'a>, ExifError> {
    let header = tiff::header(tiff)?;
    Ifd::parse(tiff, header.little_endian, header.ifd0_offset, 0, name)
}

/// Reads IFD0 and the ExifIFD of a TIFF block.
fn decode(tiff: &[u8]) -> Result<Decoded<'static>, ExifError> {
    let header = tiff::header(tiff)?;
    let little_endian = header.little_endian;
    let ifd0 = Ifd::parse(tiff, little_endian, header.ifd0_offset, 0, "IFD0")?;
    let mut data = ExifData::default();
    read_ifd0_tags(&ifd0, &mut data);
    let mut maker_note = None;
    // Unlike the tags, an ExifIFD pointer that cannot be read fails the file.
    if let Some(offset) = ifd0.uint(TAG_EXIF_IFD)? {
        let offset = offset as usize;
        // IFD0 is the only IFD read before this one.
        if offset == header.ifd0_offset {
            return Err(ExifError::IfdLoop {
                what: "ExifIFD",
                offset,
            });
        }
        let exif = Ifd::parse(tiff, little_endian, offset, 0, "ExifIFD")?;
        read_exif_tags(&exif, &mut data);
        data.interop_index = read_interop_index(tiff, little_endian, &exif);
        maker_note = maker_note_loc(&exif, little_endian).map(MakerNoteSource::InTiff);
    }
    Ok(Decoded { data, maker_note })
}

/// The value of `tag`, read on its own by `read`: a tag that cannot be
/// parsed (its value lies outside the block) reads as absent, and is logged
/// at `debug`, so it costs only its own field and never the rest of the
/// file's EXIF.
fn lenient<T>(tag: u16, read: impl FnOnce(u16) -> Result<Option<T>, ExifError>) -> Option<T> {
    read(tag).unwrap_or_else(|error| {
        tracing::debug!(
            tag = format_args!("0x{tag:04X}"),
            %error,
            "EXIF tag dropped: it cannot be parsed"
        );
        None
    })
}

fn read_ifd0_tags(ifd0: &Ifd, data: &mut ExifData) {
    data.make = lenient(TAG_MAKE, |tag| ifd0.ascii(tag));
    data.model = lenient(TAG_MODEL, |tag| ifd0.ascii(tag));
    data.orientation = lenient(TAG_ORIENTATION, |tag| ifd0.uint(tag))
        .and_then(|o| u16::try_from(o).ok())
        .filter(|o| (1..=8).contains(o))
        .unwrap_or(1);
}

fn read_exif_tags(exif: &Ifd, data: &mut ExifData) {
    let ascii = |tag| lenient(tag, |tag| exif.ascii(tag));
    let uint = |tag| lenient(tag, |tag| exif.uint(tag));
    let rational = |tag| lenient(tag, |tag| exif.rational(tag)).map(|v| v as f32);
    data.capture_time = ascii(TAG_DATE_TIME_ORIGINAL).and_then(|text| {
        let subsec = ascii(TAG_SUB_SEC_TIME_ORIGINAL);
        parse_capture_time(&text, subsec.as_deref())
    });
    data.utc_offset_minutes =
        ascii(TAG_OFFSET_TIME_ORIGINAL).and_then(|text| parse_utc_offset(&text));
    data.exposure_time_s = rational(TAG_EXPOSURE_TIME);
    data.f_number = rational(TAG_F_NUMBER);
    data.iso = uint(TAG_ISO);
    data.exposure_bias_ev = rational(TAG_EXPOSURE_BIAS);
    data.focal_length_mm = rational(TAG_FOCAL_LENGTH);
    data.exposure_mode = uint(TAG_EXPOSURE_MODE).and_then(|m| u16::try_from(m).ok());
    data.lens_model = ascii(TAG_LENS_MODEL);
    data.colour_space = uint(TAG_COLOUR_SPACE).and_then(|c| u16::try_from(c).ok());
}

/// The InteroperabilityIndex of the Interop IFD that `exif` points at.
/// Like a tag, a pointer or an IFD that cannot be read only leaves it out.
fn read_interop_index(tiff: &[u8], little_endian: bool, exif: &Ifd) -> Option<String> {
    let offset = lenient(TAG_INTEROP_IFD, |tag| exif.uint(tag))?;
    lenient(TAG_INTEROP_IFD, |_| {
        let interop = Ifd::parse(tiff, little_endian, offset as usize, 0, "InteropIFD")?;
        interop.ascii(TAG_INTEROP_INDEX)
    })
}

/// Where the MakerNote value sits. A note that cannot be located is ignored
/// rather than failing the file: it is optional, and the fallback still applies.
fn maker_note_loc(exif: &Ifd, little_endian: bool) -> Option<MakerNoteLoc> {
    let entry = exif.entry(TAG_MAKER_NOTE)?;
    let range = exif.value_range(entry).ok().flatten()?;
    Some(MakerNoteLoc {
        offset: range.start,
        len: range.len(),
        little_endian,
    })
}

/// DateTimeOriginal (`%Y:%m:%d %H:%M:%S`) plus SubSecTimeOriginal, whose digits
/// are a decimal fraction of a second ("25" is 0.25 s). Unset clocks such as
/// "0000:00:00 00:00:00" give `None`; unusable sub-seconds are ignored.
fn parse_capture_time(text: &str, subsec: Option<&str>) -> Option<NaiveDateTime> {
    let time = NaiveDateTime::parse_from_str(text, "%Y:%m:%d %H:%M:%S").ok()?;
    let nanos = subsec.and_then(|digits| {
        if !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        // At most nanosecond precision, right-padded: "25" -> "250000000".
        let mut nine: String = digits.chars().take(9).collect();
        while nine.len() < 9 {
            nine.push('0');
        }
        nine.parse::<u32>().ok()
    });
    match nanos {
        Some(nanos) => time.with_nanosecond(nanos),
        None => Some(time),
    }
}

/// OffsetTimeOriginal (`±HH:MM`) in minutes east of UTC.
fn parse_utc_offset(text: &str) -> Option<i32> {
    let &[sign, h1, h2, b':', m1, m2] = text.as_bytes() else {
        return None;
    };
    let sign = match sign {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits = [h1, h2, m1, m2];
    if !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let [h1, h2, m1, m2] = digits.map(|d| i32::from(d - b'0'));
    let (hours, minutes) = (h1 * 10 + h2, m1 * 10 + m2);
    (hours < 24 && minutes < 60).then_some(sign * (hours * 60 + minutes))
}

/// Without vendor bracket fields, EXIF ExposureMode 2 (auto bracket) marks an
/// exposure bracket whose value is the shot's exposure bias.
fn apply_exposure_mode_fallback(data: &mut ExifData) {
    if data.bracket.is_none() && data.exposure_mode == Some(EXPOSURE_MODE_AUTO_BRACKET) {
        data.bracket = Some(BracketInfo {
            mode: BracketMode::Exposure,
            shot_number: None,
            value_ev: data.exposure_bias_ev,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use photostacker_testkit::{
        MakerNoteSpec, NoteStyle, TiffSpec, Value, cr3_with_cmt, exif_value_field, ifd_value_field,
        jpeg_with_exif, raf_with_jpeg,
    };
    use std::fs;
    use std::path::PathBuf;

    fn ascii(s: &str) -> Value {
        Value::Ascii(s.into())
    }

    /// The standard tags of the brief's reference shot.
    fn standard(little_endian: bool) -> TiffSpec {
        TiffSpec {
            little_endian,
            ifd0: vec![
                (0x010F, ascii("Canon")),
                (0x0110, ascii("Canon EOS R6")),
                (0x0112, Value::Short(vec![6])),
            ],
            exif: vec![
                (0x9003, ascii("2026:05:01 10:20:30")),
                (0x9291, ascii("25")),
                (0x9011, ascii("+02:00")),
                (0x829A, Value::Rational(vec![(1, 250)])),
                (0x829D, Value::Rational(vec![(8, 1)])),
                (0x8827, Value::Short(vec![100])),
                (0x9204, Value::SRational(vec![(-2, 3)])),
                (0x920A, Value::Rational(vec![(35, 1)])),
                (0xA402, Value::Short(vec![2])),
                (0xA434, ascii("RF35mm F1.8")),
            ],
            maker_note: None,
        }
    }

    fn img() -> image::RgbImage {
        image::RgbImage::from_pixel(16, 16, image::Rgb([90, 120, 150]))
    }

    /// Writes `bytes` to `name` in a fresh temp dir and reads its EXIF.
    fn read_file(name: &str, kind: FileKind, bytes: &[u8]) -> Result<Option<ExifData>, ExifError> {
        let dir = tempfile::tempdir().unwrap();
        let path: PathBuf = dir.path().join(name);
        fs::write(&path, bytes).unwrap();
        read_exif(&path, kind)
    }

    fn read_jpeg(tiff: &[u8]) -> Result<Option<ExifData>, ExifError> {
        read_file("x.jpg", FileKind::Jpeg, &jpeg_with_exif(&img(), tiff, 90))
    }

    fn close(a: Option<f32>, b: f32, tol: f32) -> bool {
        a.is_some_and(|a| (a - b).abs() <= tol)
    }

    fn assert_standard(e: &ExifData) {
        let expected_time = NaiveDate::from_ymd_opt(2026, 5, 1)
            .unwrap()
            .and_hms_milli_opt(10, 20, 30, 250)
            .unwrap();
        assert_eq!(e.make.as_deref(), Some("Canon"));
        assert_eq!(e.model.as_deref(), Some("Canon EOS R6"));
        assert_eq!(e.lens_model.as_deref(), Some("RF35mm F1.8"));
        assert_eq!(e.focal_length_mm, Some(35.0));
        assert_eq!(e.capture_time, Some(expected_time));
        assert_eq!(e.utc_offset_minutes, Some(120));
        assert!(
            close(e.exposure_time_s, 0.004, 1e-9),
            "{:?}",
            e.exposure_time_s
        );
        assert_eq!(e.f_number, Some(8.0));
        assert_eq!(e.iso, Some(100));
        assert!(close(e.exposure_bias_ev, -0.6667, 1e-3));
        assert_eq!(e.exposure_mode, Some(2));
        assert_eq!(e.orientation, 6);
        let naive_ms = expected_time.and_utc().timestamp_millis();
        assert_eq!(e.capture_instant_ms(), Some(naive_ms - 120 * 60_000));
        let bracket = e.bracket.unwrap();
        assert_eq!(bracket.mode, BracketMode::Exposure);
        assert_eq!(bracket.shot_number, None);
        assert!(close(bracket.value_ev, -0.6667, 1e-3));
    }

    #[test]
    fn reads_standard_tags_from_jpeg() {
        let e = read_jpeg(&standard(true).build()).unwrap().unwrap();
        assert_standard(&e);
    }

    #[test]
    fn big_endian_tiff_is_read() {
        let block = standard(false).build();
        assert_standard(&read_file("x.tif", FileKind::Tiff, &block).unwrap().unwrap());
        assert_standard(&read_jpeg(&block).unwrap().unwrap());
    }

    #[test]
    fn tiff_family_raw_files_are_read_as_tiff() {
        let block = standard(true).build();
        for name in [
            "x.cr2", "x.NEF", "x.arw", "x.dng", "x.orf", "x.rw2", "x.3fr",
        ] {
            let e = read_file(name, FileKind::Raw, &block).unwrap();
            assert_eq!(e.and_then(|e| e.make).as_deref(), Some("Canon"), "{name}");
        }
    }

    #[test]
    fn tiff_with_ifd_after_large_strip_data_reads() {
        // libtiff writes the image data first and the IFDs at the end of the file.
        let strip_len = 5 * 1024 * 1024;
        for little_endian in [true, false] {
            let block = standard(little_endian).build_with_leading_data(strip_len);
            assert!(block.len() > strip_len);
            for (name, kind) in [("x.tif", FileKind::Tiff), ("x.dng", FileKind::Raw)] {
                let e = read_file(name, kind, &block).unwrap().unwrap();
                assert_standard(&e);
            }
        }
    }

    #[test]
    fn unsupported_containers_return_none() {
        let block = standard(true).build();
        let cases = [
            ("x.png", FileKind::Png),
            ("x.crw", FileKind::Raw),
            ("x.x3f", FileKind::Raw),
            ("x.mrw", FileKind::Raw),
        ];
        for (name, kind) in cases {
            assert!(read_file(name, kind, &block).unwrap().is_none(), "{name}");
        }
    }

    #[test]
    fn heic_container_goes_to_libheif_only_with_the_feature() {
        // A bare TIFF block is no HEIF file.
        let block = standard(true).build();
        for bytes in [&block[..], b""] {
            let read = read_file("x.heic", FileKind::Heic, bytes);
            if cfg!(feature = "heic") {
                assert!(matches!(read, Err(ExifError::Heif { .. })), "{read:?}");
            } else {
                assert!(read.unwrap().is_none());
            }
        }
    }

    #[test]
    fn no_exif_returns_none() {
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
            .encode_image(&img())
            .unwrap();
        assert!(read_file("x.jpg", FileKind::Jpeg, &jpeg).unwrap().is_none());
    }

    #[test]
    fn garbage_offsets_error_not_panic() {
        let block = standard(true).build();

        // IFD0 claims 0xFFFF entries.
        let mut bad = block.clone();
        bad[8..10].copy_from_slice(&0xFFFFu16.to_le_bytes());
        assert!(read_jpeg(&bad).is_err());
        assert!(read_file("x.tif", FileKind::Tiff, &bad).is_err());

        // Make (IFD0 entry 0, out of line) points past the end: only Make is lost.
        let mut bad = block.clone();
        bad[18..22].copy_from_slice(&0xFFFF_FF00u32.to_le_bytes());
        for e in [
            read_jpeg(&bad).unwrap().unwrap(),
            read_file("x.tif", FileKind::Tiff, &bad).unwrap().unwrap(),
        ] {
            assert_eq!(e.make, None);
            assert_eq!(e.model.as_deref(), Some("Canon EOS R6"));
        }

        // A truncated block.
        assert!(read_jpeg(&block[..block.len() / 2]).is_err());
    }

    #[test]
    fn one_bad_tag_keeps_the_other_fields() {
        for little_endian in [true, false] {
            let mut block = standard(little_endian).build();
            // FNumber is an out-of-line RATIONAL: point it past the end.
            let field = exif_value_field(&block, 0x829D);
            let past_the_end = u32::try_from(block.len()).unwrap() + 64;
            let past_the_end = if little_endian {
                past_the_end.to_le_bytes()
            } else {
                past_the_end.to_be_bytes()
            };
            block[field..field + 4].copy_from_slice(&past_the_end);
            let e = read_jpeg(&block).unwrap().unwrap();
            assert_eq!(e.f_number, None);
            assert_standard(&ExifData {
                f_number: Some(8.0),
                ..e
            });
        }
    }

    #[test]
    fn bad_ifd0_or_exif_ifd_pointers_are_errors() {
        let block = standard(true).build();
        // IFD0 past the end.
        let mut bad = block.clone();
        bad[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(read_jpeg(&bad).is_err());
        // The ExifIFD pointer past the end.
        let mut bad = block.clone();
        let pointer = ifd_value_field(&bad, 8, 0x8769);
        bad[pointer..pointer + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(read_jpeg(&bad).is_err());
    }

    #[test]
    fn exif_pointer_back_to_ifd0_is_a_loop_error() {
        let mut spec = standard(true);
        spec.ifd0.truncate(1); // Make only: entry 0 Make, entry 1 the ExifIFD pointer
        let mut block = spec.build();
        assert_eq!(&block[22..24], &0x8769u16.to_le_bytes());
        block[30..34].copy_from_slice(&8u32.to_le_bytes());
        assert!(matches!(decode(&block), Err(ExifError::IfdLoop { .. })));
    }

    #[test]
    fn interop_index_is_read() {
        for little_endian in [true, false] {
            let mut spec = standard(little_endian);
            spec.exif.push((0xA001, Value::Short(vec![0xFFFF])));
            let block = spec.build_with_interop(&[(0x0001, ascii("R03"))]);
            let e = read_jpeg(&block).unwrap().unwrap();
            assert_eq!(e.colour_space, Some(0xFFFF));
            assert_eq!(e.interop_index.as_deref(), Some("R03"));
            assert_standard(&e);

            // An Interop IFD past the end costs only the index.
            let mut bad = block.clone();
            let field = exif_value_field(&bad, 0xA005);
            let past_the_end = u32::try_from(bad.len()).unwrap() + 64;
            let past_the_end = if little_endian {
                past_the_end.to_le_bytes()
            } else {
                past_the_end.to_be_bytes()
            };
            bad[field..field + 4].copy_from_slice(&past_the_end);
            let e = read_jpeg(&bad).unwrap().unwrap();
            assert_eq!(e.interop_index, None);
            assert_eq!(e.colour_space, Some(0xFFFF));
            assert_standard(&e);
        }
        // Each is read on its own: ColorSpace without an Interop IFD, an
        // Interop IFD without ColorSpace, and neither.
        let mut spec = standard(true);
        spec.exif.push((0xA001, Value::Short(vec![2])));
        let e = read_jpeg(&spec.build()).unwrap().unwrap();
        assert_eq!((e.colour_space, e.interop_index), (Some(2), None));
        let block = standard(true).build_with_interop(&[(0x0001, ascii("R03"))]);
        let e = read_jpeg(&block).unwrap().unwrap();
        assert_eq!(
            (e.colour_space, e.interop_index.as_deref()),
            (None, Some("R03"))
        );
        let e = read_jpeg(&standard(true).build()).unwrap().unwrap();
        assert_eq!((e.colour_space, e.interop_index), (None, None));
    }

    #[test]
    fn orientation_defaults_to_1() {
        let mut spec = standard(true);
        spec.ifd0.retain(|(tag, _)| *tag != 0x0112);
        let e = read_jpeg(&spec.build()).unwrap().unwrap();
        assert_eq!(e.orientation, 1);

        for invalid in [0, 9, 0xFFFF] {
            let mut spec = standard(true);
            spec.ifd0[2] = (0x0112, Value::Short(vec![invalid]));
            assert_eq!(decode(&spec.build()).unwrap().data.orientation, 1);
        }
    }

    #[test]
    fn empty_tiff_block_gives_all_none() {
        let spec = TiffSpec {
            little_endian: true,
            ifd0: Vec::new(),
            exif: Vec::new(),
            maker_note: None,
        };
        let e = read_file("x.tif", FileKind::Tiff, &spec.build())
            .unwrap()
            .unwrap();
        assert_eq!(e, ExifData::default());
        assert_eq!(e.capture_instant_ms(), None);
    }

    #[test]
    fn unusable_strings_and_times_are_none() {
        let mut spec = standard(true);
        spec.ifd0[0] = (0x010F, ascii("   "));
        spec.exif = vec![
            (0x9003, ascii("0000:00:00 00:00:00")),
            (0x9011, ascii("+2:00")),
            (0xA434, ascii("")),
        ];
        let e = decode(&spec.build()).unwrap().data;
        assert_eq!(e.make, None);
        assert_eq!(e.lens_model, None);
        assert_eq!(e.capture_time, None);
        assert_eq!(e.utc_offset_minutes, None);
        assert_eq!(e.capture_instant_ms(), None);
    }

    #[test]
    fn subsec_and_offset_parsing() {
        let time = |subsec: &str, offset: &str| {
            let mut spec = standard(true);
            spec.exif = vec![
                (0x9003, ascii("2026:05:01 10:20:30")),
                (0x9291, ascii(subsec)),
                (0x9011, ascii(offset)),
            ];
            let e = decode(&spec.build()).unwrap().data;
            let base = NaiveDate::from_ymd_opt(2026, 5, 1)
                .unwrap()
                .and_hms_opt(10, 20, 30)
                .unwrap()
                .and_utc()
                .timestamp_millis();
            (
                e.capture_instant_ms().map(|ms| ms - base),
                e.utc_offset_minutes,
            )
        };
        assert_eq!(time("25", "+02:00"), (Some(250 - 120 * 60_000), Some(120)));
        assert_eq!(time("5", "-05:30"), (Some(500 + 330 * 60_000), Some(-330)));
        assert_eq!(time("123456", "+00:00"), (Some(123), Some(0)));
        assert_eq!(time("", "   :  "), (Some(0), None));
        assert_eq!(time("abc", "+14:00"), (Some(-840 * 60_000), Some(840)));
    }

    #[test]
    fn exposure_mode_other_than_auto_bracket_has_no_bracket() {
        for mode in [0, 1] {
            let mut spec = standard(true);
            spec.exif.retain(|(tag, _)| *tag != 0xA402);
            spec.exif.push((0xA402, Value::Short(vec![mode])));
            let e = decode(&spec.build()).unwrap();
            assert_eq!(e.data.exposure_mode, Some(mode));
            let e = read_jpeg(&spec.build()).unwrap().unwrap();
            assert_eq!(e.bracket, None);
        }
    }

    #[test]
    fn maker_note_location_is_recorded_per_style() {
        let headers: [(NoteStyle, &[u8]); 4] = [
            (NoteStyle::CanonIfd, b"\x01\x00"),
            (NoteStyle::FujifilmHeader, b"FUJIFILM\x0C\x00\x00\x00"),
            (
                NoteStyle::NikonType3,
                b"Nikon\x00\x02\x10\x00\x00MM\x00\x2A",
            ),
            (NoteStyle::SonyDsc, b"SONY DSC \x00\x00\x00"),
        ];
        for little_endian in [true, false] {
            for (style, head) in headers {
                let mut spec = standard(little_endian);
                spec.maker_note = Some(MakerNoteSpec {
                    style,
                    entries: vec![(0x0001, Value::Short(vec![1, 2, 3]))],
                });
                let block = spec.build();
                let decoded = decode(&block).unwrap();
                let Some(MakerNoteSource::InTiff(loc)) = decoded.maker_note else {
                    panic!("{style:?}: {:?}", decoded.maker_note);
                };
                assert_eq!(loc.little_endian, little_endian);
                let note = &block[loc.offset..loc.offset + loc.len];
                if style == NoteStyle::CanonIfd && !little_endian {
                    assert!(note.starts_with(b"\x00\x01"), "{style:?}");
                } else {
                    assert!(note.starts_with(head), "{style:?} {little_endian}");
                }
                // The note does not disturb the standard tags.
                assert_eq!(decoded.data.make.as_deref(), Some("Canon"));
            }
        }
        assert_eq!(decode(&standard(true).build()).unwrap().maker_note, None);
    }

    #[test]
    fn maker_note_out_of_bounds_is_ignored() {
        let mut spec = standard(true);
        spec.exif = vec![(0x927C, Value::Undefined(vec![0; 16]))];
        let mut block = spec.build();
        // IFD0 entry 3 is the ExifIFD pointer; the note is ExifIFD entry 0.
        let exif_at = u32::from_le_bytes(block[54..58].try_into().unwrap()) as usize;
        let field = exif_at + 2 + 8;
        block[field..field + 4].copy_from_slice(&0xFFFF_0000u32.to_le_bytes());
        let decoded = decode(&block).unwrap();
        assert_eq!(decoded.maker_note, None);
        assert_eq!(decoded.data.orientation, 6);
    }

    // --- MakerNote contexts and bracket wiring ---

    /// The note IFD of the context tests: one out-of-line SHORT[3], so a
    /// wrong base or IFD offset shows.
    fn note_entries() -> Vec<(u16, Value)> {
        vec![(0x0001, Value::Short(vec![1, 2, 3]))]
    }

    fn with_note(little_endian: bool, style: NoteStyle) -> Vec<u8> {
        let mut spec = standard(little_endian);
        spec.maker_note = Some(MakerNoteSpec {
            style,
            entries: note_entries(),
        });
        spec.build()
    }

    /// The reference shot with `note` as the raw 0x927C bytes.
    fn with_raw_note(little_endian: bool, note: Vec<u8>) -> Vec<u8> {
        let mut spec = standard(little_endian);
        spec.exif.push((0x927C, Value::Undefined(note)));
        spec.build()
    }

    /// The note's start P in `block`, and the context `make` gets for it.
    fn ctx_of<'a>(make: &str, block: &'a [u8]) -> (usize, Option<NoteCtx<'a>>) {
        let Some(MakerNoteSource::InTiff(loc)) = decode(block).unwrap().maker_note else {
            panic!("no in-TIFF note");
        };
        (loc.offset, note_ctx(make, block, loc))
    }

    /// Asserts the context and that the note IFD of [`note_entries`] reads through it.
    fn assert_ctx(ctx: Option<NoteCtx>, ifd_offset: usize, base: usize, little_endian: bool) {
        let ctx = ctx.expect("a note context");
        assert_eq!(
            (ctx.ifd_offset, ctx.base, ctx.little_endian),
            (ifd_offset, base, little_endian)
        );
        let ifd = Ifd::parse(
            ctx.tiff,
            ctx.little_endian,
            ctx.ifd_offset,
            ctx.base,
            "note",
        );
        assert_eq!(ifd.unwrap().int16s(0x0001).unwrap(), Some(vec![1, 2, 3]));
    }

    #[test]
    fn note_ctx_for_canon_and_sony() {
        for le in [true, false] {
            let block = with_note(le, NoteStyle::CanonIfd);
            let (p, ctx) = ctx_of("Canon", &block);
            assert_ctx(ctx, p, 0, le);

            let block = with_note(le, NoteStyle::SonyDsc);
            let (p, ctx) = ctx_of("SONY", &block);
            assert_ctx(ctx, p + 12, 0, le);

            // Some Sony models write the bare IFD, framed like Canon's.
            let block = with_note(le, NoteStyle::CanonIfd);
            let (p, ctx) = ctx_of("SONY", &block);
            assert_ctx(ctx, p, 0, le);
        }
    }

    #[test]
    fn note_ctx_for_fujifilm_is_note_relative_and_little_endian() {
        for le in [true, false] {
            let block = with_note(le, NoteStyle::FujifilmHeader);
            let (p, ctx) = ctx_of("FUJIFILM", &block);
            assert_ctx(ctx, p + 12, p, true);
        }
    }

    #[test]
    fn note_ctx_for_nikon_type3_in_either_byte_order() {
        for le in [true, false] {
            let block = with_note(le, NoteStyle::NikonType3);
            let (p, ctx) = ctx_of("NIKON CORPORATION", &block);
            assert_ctx(ctx, p + 18, p + 10, false);

            // An `II` embedded header, with its IFD at 12 rather than 8.
            let embedded = TiffSpec {
                little_endian: true,
                ifd0: note_entries(),
                exif: Vec::new(),
                maker_note: None,
            }
            .build_with_leading_data(4);
            let mut note = b"Nikon\0\x02\x10\0\0".to_vec();
            note.extend_from_slice(&embedded);
            let block = with_raw_note(le, note);
            let (p, ctx) = ctx_of("NIKON CORPORATION", &block);
            assert_ctx(ctx, p + 22, p + 10, true);
        }
    }

    #[test]
    fn note_ctx_rejects_mismatched_or_truncated_notes() {
        for le in [true, false] {
            let canon = with_note(le, NoteStyle::CanonIfd);
            for make in ["FUJIFILM", "NIKON", "Leica", "OLYMPUS", ""] {
                assert_eq!(ctx_of(make, &canon).1, None, "{make}");
            }
            let fuji = with_note(le, NoteStyle::FujifilmHeader);
            assert_eq!(ctx_of("NIKON", &fuji).1, None);

            let notes: [&[u8]; 9] = [
                // Nikon type 2: no embedded TIFF header.
                b"Nikon\0\x01\x00\x01\x00\x01\x00\x03\x00\x01\x00\x00\x00\x01\x00\x00\x00",
                // Nikon type 3 with a bad byte order mark, or an IFD past the note.
                b"Nikon\0\x02\x10\0\0XX\0*\0\0\0\x08\0\0",
                b"Nikon\0\x02\x10\0\0MM\0*\0\0\x10\0\0\0",
                b"Nikon\0\x02\x10\0\0MM\0*",
                // Fujifilm with its IFD offset past the note, or no room for the IFD.
                b"FUJIFILM\xF0\xFF\xFF\xFF\0\0",
                b"FUJIFILM\x0C\0\0\0",
                b"FUJIFILM\x0C\0",
                // Sony with no room for the IFD after its header.
                b"SONY DSC \0\0\0",
                // Canon with no room for the IFD entry count.
                b"\x01",
            ];
            for note in notes {
                let block = with_raw_note(le, note.to_vec());
                let make = match note[0] {
                    b'N' => "NIKON",
                    b'F' => "FUJIFILM",
                    b'S' => "SONY",
                    _ => "Canon",
                };
                assert_eq!(ctx_of(make, &block).1, None, "{note:?}");
            }
        }
        // A location outside the block.
        let block = with_note(true, NoteStyle::CanonIfd);
        for (offset, len) in [(block.len() - 1, 2), (usize::MAX, 2), (8, usize::MAX)] {
            let loc = MakerNoteLoc {
                offset,
                len,
                little_endian: true,
            };
            assert_eq!(note_ctx("Canon", &block, loc), None, "{offset} {len}");
        }
    }

    /// A Canon note with FileInfo BracketMode `mode`, BracketShotNumber 2,
    /// and ShotInfo AEBBracketValue −1 EV.
    fn canon_bracket_note(mode: u16) -> MakerNoteSpec {
        let mut file_info = vec![0u16; 20];
        file_info[0] = 40;
        file_info[3] = mode;
        file_info[5] = 2;
        let mut shot_info = vec![0u16; 34];
        shot_info[0] = 68;
        shot_info[17] = (-32i16) as u16;
        MakerNoteSpec {
            style: NoteStyle::CanonIfd,
            entries: vec![
                (0x0004, Value::Short(shot_info)),
                (0x0093, Value::Short(file_info)),
            ],
        }
    }

    #[test]
    fn maker_note_bracket_takes_precedence_over_the_fallback() {
        let fallback = Some(BracketInfo {
            mode: BracketMode::Exposure,
            shot_number: None,
            // The ExposureBiasValue rational, as `read_exif_tags` converts it.
            value_ev: Some((-2.0f64 / 3.0) as f32),
        });
        let read = |make: Option<&str>, note: MakerNoteSpec| {
            let mut spec = standard(true);
            match make {
                Some(make) => spec.ifd0[0] = (0x010F, ascii(make)),
                None => {
                    spec.ifd0.remove(0);
                }
            }
            spec.maker_note = Some(note);
            read_jpeg(&spec.build()).unwrap().unwrap().bracket
        };
        // The reference shot has ExposureMode 2 and bias −2/3 EV.
        assert_eq!(
            read(Some("Canon"), canon_bracket_note(1)),
            Some(BracketInfo {
                mode: BracketMode::Exposure,
                shot_number: Some(2),
                value_ev: Some(-1.0),
            })
        );
        // A Canon "Off" is a decision: the fallback does not override it.
        assert_eq!(
            read(Some("Canon"), canon_bracket_note(0)),
            Some(BracketInfo {
                mode: BracketMode::Off,
                shot_number: None,
                value_ev: None,
            })
        );
        // Without usable vendor fields, the fallback applies.
        let no_tables = MakerNoteSpec {
            style: NoteStyle::CanonIfd,
            entries: note_entries(),
        };
        assert_eq!(read(Some("Canon"), no_tables), fallback);
        assert_eq!(read(Some("Leica"), canon_bracket_note(1)), fallback);
        assert_eq!(read(None, canon_bracket_note(1)), fallback);
        let nikon = MakerNoteSpec {
            style: NoteStyle::NikonType3,
            entries: note_entries(),
        };
        assert_eq!(read(Some("NIKON CORPORATION"), nikon), fallback);
    }

    #[test]
    fn fill_missing_from_keeps_own_values_and_fills_gaps() {
        let full = read_jpeg(&standard(true).build()).unwrap().unwrap();
        let bracket = |mode, shot_number, value_ev| {
            Some(BracketInfo {
                mode,
                shot_number,
                value_ev,
            })
        };

        // An empty record takes every field, the orientation (6) included.
        let mut empty = ExifData::default();
        empty.fill_missing_from(&full);
        assert_eq!(empty, full);

        // Own values win; only the gaps are filled.
        let mut own = ExifData {
            model: Some("Own model".into()),
            iso: Some(3200),
            orientation: 8,
            bracket: bracket(BracketMode::Exposure, Some(4), None),
            colour_space: Some(1),
            ..ExifData::default()
        };
        let mut other = full.clone();
        other.bracket = bracket(BracketMode::Other, Some(2), Some(-1.0));
        other.colour_space = Some(0xFFFF);
        other.interop_index = Some("R03".into());
        own.fill_missing_from(&other);
        assert_eq!(own.model.as_deref(), Some("Own model"));
        assert_eq!(own.iso, Some(3200));
        assert_eq!(own.orientation, 8);
        assert_eq!(own.colour_space, Some(1));
        assert_eq!(own.interop_index.as_deref(), Some("R03"));
        assert_eq!(own.make, full.make);
        assert_eq!(own.lens_model, full.lens_model);
        assert_eq!(own.capture_time, full.capture_time);
        assert_eq!(own.exposure_mode, full.exposure_mode);
        // The bracket keeps its mode and shot number and gains the value.
        assert_eq!(
            own.bracket,
            bracket(BracketMode::Exposure, Some(4), Some(-1.0))
        );

        // An Off bracket gains neither a shot number nor a value.
        let mut off = ExifData {
            bracket: bracket(BracketMode::Off, None, None),
            ..ExifData::default()
        };
        off.fill_missing_from(&other);
        assert_eq!(off.bracket, bracket(BracketMode::Off, None, None));

        // Nothing to fill from: unchanged.
        let before = own.clone();
        own.fill_missing_from(&ExifData::default());
        assert_eq!(own, before);
    }

    #[test]
    fn fill_missing_from_takes_the_orientation_only_over_1() {
        let with = |orientation| ExifData {
            orientation,
            ..ExifData::default()
        };
        for (own, other, expected) in [(1, 6, 6), (8, 6, 8), (3, 1, 3), (1, 1, 1)] {
            let mut merged = with(own);
            merged.fill_missing_from(&with(other));
            assert_eq!(merged.orientation, expected, "{own} <- {other}");
        }
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_exif(&dir.path().join("nope.jpg"), FileKind::Jpeg).unwrap_err();
        assert!(matches!(err, ExifError::Io { .. }), "{err}");
        let err = read_exif(&dir.path().join("nope.cr2"), FileKind::Raw).unwrap_err();
        assert!(matches!(err, ExifError::Io { .. }), "{err}");
    }

    #[test]
    fn zero_byte_files_error() {
        assert!(read_file("x.jpg", FileKind::Jpeg, b"").is_err());
        assert!(read_file("x.tif", FileKind::Tiff, b"").is_err());
        assert!(read_file("x.raf", FileKind::Raw, b"").is_err());
        assert!(read_file("x.cr3", FileKind::Raw, b"").is_err());
    }

    // --- RAF ---

    #[test]
    fn reads_exif_from_raf() {
        for little_endian in [true, false] {
            let jpeg = jpeg_with_exif(&img(), &standard(little_endian).build(), 90);
            let raf = raf_with_jpeg(&jpeg);
            for name in ["x.raf", "x.RAF"] {
                let e = read_file(name, FileKind::Raw, &raf).unwrap().unwrap();
                assert_standard(&e);
            }
        }
    }

    #[test]
    fn raf_without_exif_is_none_and_garbage_errors() {
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
            .encode_image(&img())
            .unwrap();
        let raf = raf_with_jpeg(&jpeg);
        assert!(read_file("x.raf", FileKind::Raw, &raf).unwrap().is_none());
        // A TIFF block is no RAF.
        let tiff = standard(true).build();
        let err = read_file("x.raf", FileKind::Raw, &tiff).unwrap_err();
        assert!(matches!(err, ExifError::Raf { offset: 0, .. }), "{err}");
    }

    // --- CR3 ---

    /// The CMT1/CMT2/CMT3 blocks of the reference shot: IFD0, the Exif tags as
    /// the IFD0 of their own block, and a Canon note IFD with one entry.
    fn standard_cmt(little_endian: bool) -> [Vec<u8>; 3] {
        let block = |ifd0: Vec<(u16, Value)>| {
            TiffSpec {
                little_endian,
                ifd0,
                exif: Vec::new(),
                maker_note: None,
            }
            .build()
        };
        let spec = standard(little_endian);
        [
            block(spec.ifd0),
            block(spec.exif),
            block(vec![(0x0001, Value::Short(vec![1, 2, 3]))]),
        ]
    }

    fn standard_cr3(little_endian: bool) -> Vec<u8> {
        let [cmt1, cmt2, cmt3] = standard_cmt(little_endian);
        cr3_with_cmt(&cmt1, &cmt2, &cmt3)
    }

    #[test]
    fn reads_exif_from_cr3() {
        for little_endian in [true, false] {
            let cr3 = standard_cr3(little_endian);
            for name in ["x.cr3", "x.CR3"] {
                let e = read_file(name, FileKind::Raw, &cr3).unwrap().unwrap();
                assert_standard(&e);
            }
        }
        // Each CMT block has its own byte order.
        let [cmt1, _, cmt3] = standard_cmt(true);
        let [_, cmt2, _] = standard_cmt(false);
        let cr3 = cr3_with_cmt(&cmt1, &cmt2, &cmt3);
        assert_standard(&read_file("x.cr3", FileKind::Raw, &cr3).unwrap().unwrap());
    }

    #[test]
    fn cr3_maker_note_is_the_cmt3_block() {
        for little_endian in [true, false] {
            let [cmt1, cmt2, cmt3] = standard_cmt(little_endian);
            let cr3 = cr3_with_cmt(&cmt1, &cmt2, &cmt3);
            let decoded = decode_cr3(&cr3).unwrap().unwrap();
            let Some(MakerNoteSource::Cr3Cmt3(ctx)) = decoded.maker_note else {
                panic!("{:?}", decoded.maker_note);
            };
            assert_eq!(ctx.tiff, &cmt3[..]);
            assert_eq!(ctx.little_endian, little_endian);
            assert_eq!(ctx.ifd_offset, 8);
            assert_eq!(ctx.base, 0);
            // A note parser can read the Canon IFD straight from the context.
            let note = Ifd::parse(
                ctx.tiff,
                ctx.little_endian,
                ctx.ifd_offset,
                ctx.base,
                "Canon",
            )
            .unwrap();
            assert_eq!(note.uint(0x0001).unwrap(), Some(1));
        }
    }

    #[test]
    fn cr3_partial_or_broken_cmt_blocks() {
        let [cmt1, cmt2, cmt3] = standard_cmt(true);

        // An unreadable CMT3 only loses the note.
        let cr3 = cr3_with_cmt(&cmt1, &cmt2, b"garbage");
        let decoded = decode_cr3(&cr3).unwrap().unwrap();
        assert_eq!(decoded.maker_note, None);
        assert_eq!(decoded.data.make.as_deref(), Some("Canon"));
        assert_eq!(decoded.data.iso, Some(100));

        // CMT1 and CMT2 carry the standard tags: a header or an entry table
        // that cannot be read is an error.
        for cr3 in [
            cr3_with_cmt(b"garbage", &cmt2, &cmt3),
            cr3_with_cmt(&cmt1, b"garbage", &cmt3),
            cr3_with_cmt(&cmt1, &cmt2[..cmt2.len() / 2], &cmt3),
        ] {
            assert!(read_file("x.cr3", FileKind::Raw, &cr3).is_err());
        }
        // A cut that loses only the last value (CMT1's Model) loses only that tag.
        let cr3 = cr3_with_cmt(&cmt1[..cmt1.len() - 4], &cmt2, &cmt3);
        let e = read_file("x.cr3", FileKind::Raw, &cr3).unwrap().unwrap();
        assert_eq!(e.model, None);
        assert_eq!(e.make.as_deref(), Some("Canon"));
        assert_eq!(e.iso, Some(100));

        // A CMT1 ExifIFD pointer is not followed: the Exif tags come from CMT2.
        let mut with_pointer = standard(true);
        with_pointer.exif = vec![(0x8827, Value::Short(vec![6400]))];
        let cmt1_with_exif = with_pointer.build();
        let cr3 = cr3_with_cmt(&cmt1_with_exif, &cmt2, &cmt3);
        let e = read_file("x.cr3", FileKind::Raw, &cr3).unwrap().unwrap();
        assert_standard(&e);
    }

    #[test]
    fn cr3_without_cmt_blocks_is_none() {
        // ftyp, then a moov without the Canon uuid.
        let mut cr3 = standard_cr3(true);
        cr3.truncate(24);
        cr3.extend_from_slice(&[0, 0, 0, 16, b'm', b'o', b'o', b'v']);
        cr3.extend_from_slice(&[0, 0, 0, 8, b'm', b'v', b'h', b'd']);
        assert!(read_file("x.cr3", FileKind::Raw, &cr3).unwrap().is_none());
        // A TIFF block is no CR3.
        let tiff = standard(true).build();
        let err = read_file("x.cr3", FileKind::Raw, &tiff).unwrap_err();
        assert!(matches!(err, ExifError::Bmff { offset: 0, .. }), "{err}");
    }

    #[test]
    fn truncated_bmff_errors_not_panics() {
        let cr3 = standard_cr3(true);
        let ftyp_end = 24;
        assert_eq!(&cr3[ftyp_end + 4..ftyp_end + 8], b"moov");
        let moov_len = u32::from_be_bytes(cr3[ftyp_end..ftyp_end + 4].try_into().unwrap());
        let moov_end = ftyp_end + moov_len as usize;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.cr3");
        for n in 0..=cr3.len() {
            fs::write(&path, &cr3[..n]).unwrap();
            let result = read_exif(&path, FileKind::Raw);
            if n == ftyp_end {
                // The file ends cleanly after ftyp: there is no moov.
                assert!(result.unwrap().is_none(), "{n}");
            } else if n < moov_end {
                let err = result.unwrap_err();
                assert!(matches!(err, ExifError::Bmff { .. }), "{n}: {err}");
            } else {
                // Boxes after moov are never read.
                assert_standard(&result.unwrap().unwrap());
            }
        }
    }

    /// The store persists this JSON: changing a spelling needs a migration.
    #[test]
    fn exif_serde_round_trips() {
        let bias = (-2.0f64 / 3.0) as f32;
        let full = ExifData {
            make: Some("Canon".into()),
            model: Some("Canon EOS R6".into()),
            lens_model: Some("RF50mm F1.8 STM".into()),
            focal_length_mm: Some(50.0),
            capture_time: Some(
                NaiveDate::from_ymd_opt(2026, 5, 1)
                    .unwrap()
                    .and_hms_milli_opt(10, 20, 30, 250)
                    .unwrap(),
            ),
            utc_offset_minutes: Some(120),
            exposure_time_s: Some(1.0 / 250.0),
            f_number: Some(8.0),
            iso: Some(100),
            exposure_bias_ev: Some(bias),
            exposure_mode: Some(2),
            orientation: 6,
            bracket: Some(BracketInfo {
                mode: BracketMode::Exposure,
                shot_number: Some(2),
                value_ev: Some(bias),
            }),
            colour_space: Some(0xFFFF),
            interop_index: Some("R03".into()),
        };
        let json = concat!(
            r#"{"make":"Canon","model":"Canon EOS R6","lens_model":"RF50mm F1.8 STM","#,
            r#""focal_length_mm":50.0,"capture_time":"2026-05-01T10:20:30.250","#,
            r#""utc_offset_minutes":120,"exposure_time_s":0.004,"f_number":8.0,"iso":100,"#,
            r#""exposure_bias_ev":-0.6666667,"exposure_mode":2,"orientation":6,"#,
            r#""bracket":{"mode":"exposure","shot_number":2,"value_ev":-0.6666667},"#,
            r#""colour_space":65535,"interop_index":"R03"}"#
        );
        assert_eq!(serde_json::to_string(&full).unwrap(), json);
        assert_eq!(serde_json::from_str::<ExifData>(json).unwrap(), full);

        // `None` fields are left out on write and read back as `None`.
        let sparse = ExifData {
            bracket: Some(BracketInfo {
                mode: BracketMode::Off,
                shot_number: None,
                value_ev: None,
            }),
            ..ExifData::default()
        };
        let json = r#"{"orientation":1,"bracket":{"mode":"off"}}"#;
        assert_eq!(serde_json::to_string(&sparse).unwrap(), json);
        assert_eq!(serde_json::from_str::<ExifData>(json).unwrap(), sparse);

        // JSON that is not an `ExifData` is an error, not "no EXIF": an empty
        // object, or another type's object such as a `HashPair` read from
        // the wrong column. So is a bracket without its mode.
        let hash_pair =
            serde_json::to_string(&crate::hash::HashPair { phash: 1, dhash: 2 }).unwrap();
        for foreign in [
            "{}",
            hash_pair.as_str(),
            r#"{"orientation":1,"bracket":{}}"#,
        ] {
            let error = serde_json::from_str::<ExifData>(foreign).unwrap_err();
            assert!(
                error.to_string().starts_with("missing field"),
                "{foreign}: {error}"
            );
        }
        assert_eq!(
            serde_json::to_string(&BracketMode::Other).unwrap(),
            r#""other""#
        );
    }
}
