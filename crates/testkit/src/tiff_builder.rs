//! Builds synthetic TIFF/EXIF blocks byte by byte, including vendor MakerNotes,
//! so EXIF readers can be tested without real camera files.

/// A TIFF field value; the variant decides the TIFF type code.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// Type 2. A terminating NUL is appended; the count includes it.
    Ascii(String),
    /// Type 3.
    Short(Vec<u16>),
    /// Type 8.
    SShort(Vec<i16>),
    /// Type 4.
    Long(Vec<u32>),
    /// Type 5, (numerator, denominator).
    Rational(Vec<(u32, u32)>),
    /// Type 10, (numerator, denominator).
    SRational(Vec<(i32, i32)>),
    /// Type 7.
    Undefined(Vec<u8>),
}

/// The vendor framing of a MakerNote blob (EXIF tag 0x927C).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteStyle {
    /// The note is a bare IFD; value offsets are relative to the enclosing TIFF header.
    CanonIfd,
    /// `"FUJIFILM"` + u32 LE IFD offset (12), always little-endian; value offsets
    /// are relative to the note start.
    FujifilmHeader,
    /// `"Nikon\0"` + `02 10 00 00`, then an embedded big-endian TIFF header
    /// (`MM\0*` + u32 8) at note+10; value offsets are relative to note+10.
    NikonType3,
    /// `"SONY DSC \0\0\0"` (12 bytes), then the IFD; value offsets are relative to
    /// the enclosing TIFF header.
    SonyDsc,
}

/// A MakerNote: its framing and the entries of its IFD.
#[derive(Debug, Clone, PartialEq)]
pub struct MakerNoteSpec {
    pub style: NoteStyle,
    pub entries: Vec<(u16, Value)>,
}

/// A TIFF block as found in a JPEG APP1 segment or at the start of a TIFF-family file.
#[derive(Debug, Clone, PartialEq)]
pub struct TiffSpec {
    pub little_endian: bool,
    /// IFD0 entries. The ExifIFD pointer (0x8769) is added by [`TiffSpec::build`].
    pub ifd0: Vec<(u16, Value)>,
    /// ExifIFD entries. The MakerNote (0x927C) is added from `maker_note`.
    pub exif: Vec<(u16, Value)>,
    pub maker_note: Option<MakerNoteSpec>,
}

const TAG_EXIF_IFD: u16 = 0x8769;
const TAG_MAKER_NOTE: u16 = 0x927C;
const TAG_INTEROP_IFD: u16 = 0xA005;

impl Value {
    fn type_code(&self) -> u16 {
        match self {
            Value::Ascii(_) => 2,
            Value::Short(_) => 3,
            Value::Long(_) => 4,
            Value::Rational(_) => 5,
            Value::Undefined(_) => 7,
            Value::SShort(_) => 8,
            Value::SRational(_) => 10,
        }
    }

    fn count(&self) -> u32 {
        let n = match self {
            Value::Ascii(s) => s.len() + 1,
            Value::Short(v) => v.len(),
            Value::SShort(v) => v.len(),
            Value::Long(v) => v.len(),
            Value::Rational(v) => v.len(),
            Value::SRational(v) => v.len(),
            Value::Undefined(v) => v.len(),
        };
        u32::try_from(n).expect("value count fits in u32")
    }

    fn encode(&self, le: bool) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Value::Ascii(s) => {
                out.extend_from_slice(s.as_bytes());
                out.push(0);
            }
            Value::Short(v) => v.iter().for_each(|&x| out.extend(u16b(le, x))),
            Value::SShort(v) => v.iter().for_each(|&x| out.extend(u16b(le, x as u16))),
            Value::Long(v) => v.iter().for_each(|&x| out.extend(u32b(le, x))),
            Value::Rational(v) => v.iter().for_each(|&(n, d)| {
                out.extend(u32b(le, n));
                out.extend(u32b(le, d));
            }),
            Value::SRational(v) => v.iter().for_each(|&(n, d)| {
                out.extend(u32b(le, n as u32));
                out.extend(u32b(le, d as u32));
            }),
            Value::Undefined(v) => out.extend_from_slice(v),
        }
        out
    }
}

impl TiffSpec {
    /// Serialises the block:
    /// - the header (`II*\0` or `MM\0*`, IFD0 at 8);
    /// - IFD0 at 8, then its out-of-line values;
    /// - the ExifIFD right after (present when `exif` or `maker_note` is set),
    ///   then its out-of-line values, the MakerNote among them.
    ///
    /// Every IFD has its entries sorted by tag, a zero next-IFD pointer, and its
    /// out-of-line values in entry order, each padded to an even length.
    pub fn build(&self) -> Vec<u8> {
        self.build_with_leading_data(0)
    }

    /// Like [`TiffSpec::build`], with `len` zero bytes between the header and
    /// IFD0, the way libtiff writes image data before the IFDs.
    pub fn build_with_leading_data(&self, len: usize) -> Vec<u8> {
        let le = self.little_endian;
        assert!(
            self.ifd0.iter().all(|(tag, _)| *tag != TAG_EXIF_IFD),
            "the ExifIFD pointer is added by build()"
        );
        assert!(
            self.maker_note.is_none() || self.exif.iter().all(|(tag, _)| *tag != TAG_MAKER_NOTE),
            "the MakerNote tag comes from maker_note"
        );
        let mut out = Vec::new();
        out.extend_from_slice(if le { b"II" } else { b"MM" });
        out.extend(u16b(le, 42));
        let ifd0_at = 8 + frame_u32(len);
        out.extend(u32b(le, ifd0_at));
        out.resize(out.len() + len, 0);

        let has_exif = !self.exif.is_empty() || self.maker_note.is_some();
        let mut ifd0 = self.ifd0.clone();
        if has_exif {
            // The pointer is inline, so IFD0's size does not depend on its value.
            ifd0.push((TAG_EXIF_IFD, Value::Long(vec![0])));
            let exif_start = ifd0_at + frame_u32(ifd_bytes(&ifd0, le, ifd0_at).0.len());
            set_value(&mut ifd0, TAG_EXIF_IFD, Value::Long(vec![exif_start]));
        }
        out.extend(ifd_bytes(&ifd0, le, ifd0_at).0);
        if !has_exif {
            return out;
        }

        let exif_start = frame_u32(out.len());
        let mut exif = self.exif.clone();
        if let Some(note) = &self.maker_note {
            // A note's length does not depend on where it lands; its offsets may.
            let len = note_bytes(note, le, 0).len();
            exif.push((TAG_MAKER_NOTE, Value::Undefined(vec![0; len])));
            let (_, positions) = ifd_bytes(&exif, le, exif_start);
            let at = positions
                .iter()
                .find(|(tag, _)| *tag == TAG_MAKER_NOTE)
                .map(|&(_, at)| at)
                .expect("a MakerNote is never inline");
            set_value(
                &mut exif,
                TAG_MAKER_NOTE,
                Value::Undefined(note_bytes(note, le, at)),
            );
        }
        out.extend(ifd_bytes(&exif, le, exif_start).0);
        out
    }

    /// Like [`TiffSpec::build`], with an Interoperability IFD of `interop`
    /// entries: the ExifIFD gets the pointer (0xA005), and the Interop IFD
    /// follows the rest of the block.
    pub fn build_with_interop(&self, interop: &[(u16, Value)]) -> Vec<u8> {
        assert!(
            self.exif.iter().all(|(tag, _)| *tag != TAG_INTEROP_IFD),
            "the Interop IFD pointer is added by build_with_interop()"
        );
        let mut spec = self.clone();
        // An inline placeholder, so the block's layout does not depend on it.
        spec.exif.push((TAG_INTEROP_IFD, Value::Long(vec![0])));
        let mut out = spec.build();
        // Every value is padded to an even length, so the IFD lands on a
        // word boundary.
        let interop_at = frame_u32(out.len());
        let field = exif_value_field(&out, TAG_INTEROP_IFD);
        out[field..field + 4].copy_from_slice(&u32b(self.little_endian, interop_at));
        out.extend(ifd_bytes(interop, self.little_endian, interop_at).0);
        out
    }
}

/// The MakerNote blob for a note that starts at `at` in the enclosing TIFF block.
fn note_bytes(note: &MakerNoteSpec, outer_le: bool, at: u32) -> Vec<u8> {
    let mut out = Vec::new();
    match note.style {
        NoteStyle::CanonIfd => out.extend(ifd_bytes(&note.entries, outer_le, at).0),
        NoteStyle::SonyDsc => {
            out.extend_from_slice(b"SONY DSC \0\0\0");
            out.extend(ifd_bytes(&note.entries, outer_le, at + 12).0);
        }
        NoteStyle::FujifilmHeader => {
            out.extend_from_slice(b"FUJIFILM");
            out.extend(u32b(true, 12));
            out.extend(ifd_bytes(&note.entries, true, 12).0);
        }
        NoteStyle::NikonType3 => {
            out.extend_from_slice(b"Nikon\0\x02\x10\0\0");
            out.extend_from_slice(b"MM\0*");
            out.extend(u32b(false, 8));
            out.extend(ifd_bytes(&note.entries, false, 8).0);
        }
    }
    out
}

/// One IFD and its out-of-line values. `start` is the IFD's offset in the frame
/// that value offsets are measured from. Also returns, per out-of-line value,
/// its tag and frame offset.
pub(crate) fn ifd_bytes(
    entries: &[(u16, Value)],
    le: bool,
    start: u32,
) -> (Vec<u8>, Vec<(u16, u32)>) {
    let mut sorted: Vec<&(u16, Value)> = entries.iter().collect();
    sorted.sort_by_key(|(tag, _)| *tag);
    let count = u16::try_from(sorted.len()).expect("at most 65535 entries");
    let table_len = 2 + 12 * sorted.len() + 4;
    let mut table = Vec::with_capacity(table_len);
    let mut data = Vec::new();
    let mut positions = Vec::new();
    table.extend(u16b(le, count));
    for (tag, value) in sorted {
        let bytes = value.encode(le);
        table.extend(u16b(le, *tag));
        table.extend(u16b(le, value.type_code()));
        table.extend(u32b(le, value.count()));
        if bytes.len() <= 4 {
            let mut field = [0; 4];
            field[..bytes.len()].copy_from_slice(&bytes);
            table.extend(field);
        } else {
            let at = start + frame_u32(table_len + data.len());
            positions.push((*tag, at));
            table.extend(u32b(le, at));
            data.extend(bytes);
            if data.len() % 2 == 1 {
                data.push(0);
            }
        }
    }
    table.extend(u32b(le, 0)); // no next IFD
    table.extend(data);
    (table, positions)
}

fn set_value(entries: &mut [(u16, Value)], tag: u16, value: Value) {
    let entry = entries
        .iter_mut()
        .find(|(t, _)| *t == tag)
        .expect("tag present");
    entry.1 = value;
}

fn frame_u32(n: usize) -> u32 {
    u32::try_from(n).expect("TIFF offsets fit in u32")
}

fn u16b(le: bool, v: u16) -> [u8; 2] {
    if le { v.to_le_bytes() } else { v.to_be_bytes() }
}

fn u32b(le: bool, v: u32) -> [u8; 4] {
    if le { v.to_le_bytes() } else { v.to_be_bytes() }
}

/// Where the 4-byte value/offset field of entry `tag` of the IFD at
/// `ifd_offset` sits in `block`, so that a test can point it elsewhere. The
/// byte order comes from the block's header.
///
/// # Panics
/// When that IFD has no `tag` entry.
pub fn ifd_value_field(block: &[u8], ifd_offset: usize, tag: u16) -> usize {
    let count = usize::from(read_u16(block, ifd_offset));
    (0..count)
        .map(|i| ifd_offset + 2 + 12 * i)
        .find(|&entry| read_u16(block, entry) == tag)
        .map(|entry| entry + 8)
        .unwrap_or_else(|| panic!("no tag 0x{tag:04X} in the IFD at {ifd_offset}"))
}

/// [`ifd_value_field`] for an entry of the ExifIFD that IFD0 points at, as
/// in a block built by [`TiffSpec::build`].
pub fn exif_value_field(block: &[u8], tag: u16) -> usize {
    let ifd0 = read_u32(block, 4);
    let pointer = ifd_value_field(block, ifd0, TAG_EXIF_IFD);
    ifd_value_field(block, read_u32(block, pointer), tag)
}

/// The u16 at `at`, in the byte order of `block`'s header.
fn read_u16(block: &[u8], at: usize) -> u16 {
    let bytes = [block[at], block[at + 1]];
    if block.starts_with(b"II") {
        u16::from_le_bytes(bytes)
    } else {
        u16::from_be_bytes(bytes)
    }
}

/// The u32 at `at`, in the byte order of `block`'s header, as an offset.
fn read_u32(block: &[u8], at: usize) -> usize {
    let bytes: [u8; 4] = block[at..at + 4].try_into().expect("four bytes");
    let value = if block.starts_with(b"II") {
        u32::from_le_bytes(bytes)
    } else {
        u32::from_be_bytes(bytes)
    };
    value as usize
}

/// Encodes `img` as a JPEG at `quality` and inserts an APP1 `Exif\0\0` segment
/// carrying `tiff` right after SOI.
pub fn jpeg_with_exif(img: &image::RgbImage, tiff: &[u8], quality: u8) -> Vec<u8> {
    jpeg_with_segment(img, quality, 0xE1, &[b"Exif\0\0", tiff])
}

/// Encodes `img` as a JPEG at `quality` and inserts an APP2 `ICC_PROFILE\0`
/// segment carrying all of `icc` (chunk 1 of 1) right after SOI.
pub fn jpeg_with_icc(img: &image::RgbImage, icc: &[u8], quality: u8) -> Vec<u8> {
    jpeg_with_segment(img, quality, 0xE2, &[b"ICC_PROFILE\0", &[1, 1], icc])
}

/// Encodes `img` as a JPEG at `quality` and inserts one segment with the
/// `marker` code, its payload the concatenated `parts`, right after SOI.
fn jpeg_with_segment(img: &image::RgbImage, quality: u8, marker: u8, parts: &[&[u8]]) -> Vec<u8> {
    let mut encoded = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, quality)
        .encode_image(img)
        .expect("JPEG encoding of an RgbImage");
    assert_eq!(encoded[..2], [0xFF, 0xD8], "the encoder starts with SOI");
    let payload_len: usize = parts.iter().map(|part| part.len()).sum();
    let segment_len = u16::try_from(2 + payload_len).expect("the payload fits in one segment");
    let mut out = Vec::with_capacity(encoded.len() + 4 + payload_len);
    out.extend_from_slice(&[0xFF, 0xD8, 0xFF, marker]);
    out.extend_from_slice(&segment_len.to_be_bytes());
    parts.iter().for_each(|part| out.extend_from_slice(part));
    out.extend_from_slice(&encoded[2..]);
    out
}

/// Writes an uncompressed little-endian 16-bit RGB TIFF of `width × height`
/// to `path`, in strips of `rows_per_strip` rows, streaming the rows so that
/// no whole image is ever held. Red ramps from 0 to 65535 along x, green
/// along y, and blue is 0x8000.
///
/// # Panics
/// When a side or `rows_per_strip` is 0, or the file is over 4 GiB.
pub fn write_rgb16_tiff(
    path: &std::path::Path,
    width: u32,
    height: u32,
    rows_per_strip: u32,
) -> std::io::Result<()> {
    use std::io::Write;

    assert!(width > 0 && height > 0 && rows_per_strip > 0);
    let row_bytes = u64::from(width) * 6;
    let strips = height.div_ceil(rows_per_strip);
    let first_row = |strip: u32| u64::from(strip) * u64::from(rows_per_strip);
    let byte_counts: Vec<u32> = (0..strips)
        .map(|strip| {
            let rows = u64::from(height) - first_row(strip);
            let rows = rows.min(u64::from(rows_per_strip));
            u32::try_from(rows * row_bytes).expect("a strip under 4 GiB")
        })
        .collect();
    let spec = |data_at: u64| {
        let offsets = (0..strips)
            .map(|strip| {
                let at = data_at + first_row(strip) * row_bytes;
                u32::try_from(at).expect("a classic TIFF under 4 GiB")
            })
            .collect();
        TiffSpec {
            little_endian: true,
            ifd0: vec![
                (256, Value::Long(vec![width])),
                (257, Value::Long(vec![height])),
                (258, Value::Short(vec![16, 16, 16])),
                (259, Value::Short(vec![1])),
                (262, Value::Short(vec![2])),
                (273, Value::Long(offsets)),
                (277, Value::Short(vec![3])),
                (278, Value::Long(vec![rows_per_strip])),
                (279, Value::Long(byte_counts.clone())),
                (284, Value::Short(vec![1])),
            ],
            exif: Vec::new(),
            maker_note: None,
        }
    };
    // The offsets' value has the same length whatever they hold.
    let header_len = spec(0).build().len() as u64;
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    out.write_all(&spec(header_len).build())?;
    let ramp = |at: u32, side: u32| (u64::from(at) * 65535 / u64::from(side - 1).max(1)) as u16;
    let mut row = Vec::with_capacity(usize::try_from(row_bytes).expect("a row in memory"));
    for y in 0..height {
        row.clear();
        for x in 0..width {
            for sample in [ramp(x, width), ramp(y, height), 0x8000] {
                row.extend_from_slice(&sample.to_le_bytes());
            }
        }
        out.write_all(&row)?;
    }
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(little_endian: bool) -> TiffSpec {
        TiffSpec {
            little_endian,
            ifd0: Vec::new(),
            exif: Vec::new(),
            maker_note: None,
        }
    }

    #[test]
    fn single_inline_entry_little_endian() {
        let mut s = spec(true);
        s.ifd0.push((0x0112, Value::Short(vec![6])));
        #[rustfmt::skip]
        let expected: Vec<u8> = vec![
            b'I', b'I', 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00,
            0x01, 0x00,
            0x12, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        assert_eq!(s.build(), expected);
    }

    #[test]
    fn big_endian_out_of_line_value_and_exif_pointer() {
        let mut s = spec(false);
        s.ifd0.push((0x010F, Value::Ascii("Canon".into())));
        s.exif.push((0xA402, Value::Short(vec![2])));
        #[rustfmt::skip]
        let expected: Vec<u8> = vec![
            b'M', b'M', 0x00, 0x2A, 0x00, 0x00, 0x00, 0x08,
            // IFD0 at 8: Make (out of line at 38), ExifIFD pointer (44).
            0x00, 0x02,
            0x01, 0x0F, 0x00, 0x02, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x26,
            0x87, 0x69, 0x00, 0x04, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x2C,
            0x00, 0x00, 0x00, 0x00,
            b'C', b'a', b'n', b'o', b'n', 0x00,
            // ExifIFD at 44.
            0x00, 0x01,
            0xA4, 0x02, 0x00, 0x03, 0x00, 0x00, 0x00, 0x01, 0x00, 0x02, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        assert_eq!(s.build(), expected);
    }

    #[test]
    fn values_are_padded_to_even_offsets() {
        let mut s = spec(true);
        s.ifd0.push((0x010F, Value::Ascii("Nikon".into()))); // 6 bytes, even
        s.ifd0.push((0x0110, Value::Ascii("Z 8".into()))); // 4 bytes, inline
        s.ifd0.push((0x0131, Value::Ascii("Ver.1.00".into()))); // 9 bytes, padded
        s.ifd0.push((0x013B, Value::Ascii("Photo".into()))); // 6 bytes
        let out = s.build();
        // IFD0 at 8 with 4 entries: data starts at 8 + 2 + 48 + 4 = 62.
        let offset = |i: usize| {
            let at = 10 + 12 * i + 8;
            u32::from_le_bytes(out[at..at + 4].try_into().unwrap())
        };
        assert_eq!(offset(0), 62);
        assert_eq!(&out[18 + 12..22 + 12], b"Z 8\0");
        assert_eq!(offset(2), 68);
        assert_eq!(offset(3), 78);
        assert_eq!(&out[78..84], b"Photo\0");
        assert_eq!(out.len(), 84);
    }

    /// Header + IFD0 (only the ExifIFD pointer, to 26) + ExifIFD (only the
    /// MakerNote, whose bytes start at 44) for a note of `note_len` bytes.
    fn note_prefix(little_endian: bool, note_len: u32) -> Vec<u8> {
        let u16b = |v: u16| {
            if little_endian {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        };
        let u32b = |v: u32| {
            if little_endian {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        };
        let mut out = Vec::new();
        out.extend_from_slice(if little_endian { b"II" } else { b"MM" });
        out.extend_from_slice(&u16b(42));
        out.extend_from_slice(&u32b(8));
        out.extend_from_slice(&u16b(1));
        out.extend_from_slice(&u16b(0x8769));
        out.extend_from_slice(&u16b(4));
        out.extend_from_slice(&u32b(1));
        out.extend_from_slice(&u32b(26));
        out.extend_from_slice(&u32b(0));
        out.extend_from_slice(&u16b(1));
        out.extend_from_slice(&u16b(0x927C));
        out.extend_from_slice(&u16b(7));
        out.extend_from_slice(&u32b(note_len));
        out.extend_from_slice(&u32b(44));
        out.extend_from_slice(&u32b(0));
        assert_eq!(out.len(), 44);
        out
    }

    /// A note with one out-of-line entry: tag 1, SHORT [1, 2, 3].
    fn note_spec(little_endian: bool, style: NoteStyle) -> TiffSpec {
        let mut s = spec(little_endian);
        s.maker_note = Some(MakerNoteSpec {
            style,
            entries: vec![(0x0001, Value::Short(vec![1, 2, 3]))],
        });
        s
    }

    fn check_note(little_endian: bool, style: NoteStyle, note: &[u8]) {
        let mut expected = note_prefix(little_endian, note.len() as u32);
        expected.extend_from_slice(note);
        assert_eq!(
            note_spec(little_endian, style).build(),
            expected,
            "{style:?}"
        );
    }

    #[test]
    fn canon_note_is_a_bare_ifd_with_tiff_relative_offsets() {
        // Note at 44; its data at 44 + 18 = 62 (0x3E), in the outer byte order.
        #[rustfmt::skip]
        check_note(true, NoteStyle::CanonIfd, &[
            0x01, 0x00,
            0x01, 0x00, 0x03, 0x00, 0x03, 0x00, 0x00, 0x00, 0x3E, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
            0x01, 0x00, 0x02, 0x00, 0x03, 0x00,
        ]);
        #[rustfmt::skip]
        check_note(false, NoteStyle::CanonIfd, &[
            0x00, 0x01,
            0x00, 0x01, 0x00, 0x03, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x3E,
            0x00, 0x00, 0x00, 0x00,
            0x00, 0x01, 0x00, 0x02, 0x00, 0x03,
        ]);
    }

    #[test]
    fn sony_note_has_a_12_byte_header_and_tiff_relative_offsets() {
        // IFD at 44 + 12 = 56; its data at 56 + 18 = 74 (0x4A).
        #[rustfmt::skip]
        check_note(true, NoteStyle::SonyDsc, &[
            b'S', b'O', b'N', b'Y', b' ', b'D', b'S', b'C', b' ', 0x00, 0x00, 0x00,
            0x01, 0x00,
            0x01, 0x00, 0x03, 0x00, 0x03, 0x00, 0x00, 0x00, 0x4A, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
            0x01, 0x00, 0x02, 0x00, 0x03, 0x00,
        ]);
        // The IFD after the header follows the outer byte order.
        #[rustfmt::skip]
        check_note(false, NoteStyle::SonyDsc, &[
            b'S', b'O', b'N', b'Y', b' ', b'D', b'S', b'C', b' ', 0x00, 0x00, 0x00,
            0x00, 0x01,
            0x00, 0x01, 0x00, 0x03, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x4A,
            0x00, 0x00, 0x00, 0x00,
            0x00, 0x01, 0x00, 0x02, 0x00, 0x03,
        ]);
    }

    #[test]
    fn fujifilm_note_is_little_endian_with_note_relative_offsets() {
        // IFD at note+12; its data at note+30 (0x1E). Little-endian even in a MM block.
        #[rustfmt::skip]
        let note = [
            b'F', b'U', b'J', b'I', b'F', b'I', b'L', b'M', 0x0C, 0x00, 0x00, 0x00,
            0x01, 0x00,
            0x01, 0x00, 0x03, 0x00, 0x03, 0x00, 0x00, 0x00, 0x1E, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
            0x01, 0x00, 0x02, 0x00, 0x03, 0x00,
        ];
        check_note(true, NoteStyle::FujifilmHeader, &note);
        check_note(false, NoteStyle::FujifilmHeader, &note);
    }

    #[test]
    fn nikon_note_embeds_a_big_endian_tiff_header_at_note_plus_10() {
        // IFD at frame offset 8 (note+18); its data at frame offset 26 (0x1A).
        #[rustfmt::skip]
        let note = [
            b'N', b'i', b'k', b'o', b'n', 0x00, 0x02, 0x10, 0x00, 0x00,
            b'M', b'M', 0x00, 0x2A, 0x00, 0x00, 0x00, 0x08,
            0x00, 0x01,
            0x00, 0x01, 0x00, 0x03, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x1A,
            0x00, 0x00, 0x00, 0x00,
            0x00, 0x01, 0x00, 0x02, 0x00, 0x03,
        ];
        check_note(true, NoteStyle::NikonType3, &note);
        check_note(false, NoteStyle::NikonType3, &note);
    }

    #[test]
    fn leading_data_puts_ifd0_after_the_filler() {
        let mut s = spec(true);
        s.ifd0.push((0x010F, Value::Ascii("Canon".into())));
        s.exif.push((0xA402, Value::Short(vec![2])));
        assert_eq!(s.build_with_leading_data(0), s.build());

        let out = s.build_with_leading_data(10);
        let u32_at = |at: usize| u32::from_le_bytes(out[at..at + 4].try_into().unwrap());
        assert_eq!(u32_at(4), 18);
        assert_eq!(&out[8..18], &[0; 10]);
        // IFD0 at 18: Make (out of line), ExifIFD pointer.
        assert_eq!(&out[18..20], &2u16.to_le_bytes());
        assert_eq!(u32_at(20 + 8), 18 + 30);
        assert_eq!(&out[48..54], b"Canon\0");
        assert_eq!(u32_at(32 + 8), 54);
        assert_eq!(&out[54..56], &1u16.to_le_bytes());
        // The tail is the zero-gap block shifted by 10, offsets aside.
        assert_eq!(out.len(), s.build().len() + 10);
    }

    #[test]
    fn value_fields_are_found_in_ifd0_and_the_exif_ifd() {
        for little_endian in [true, false] {
            let mut s = spec(little_endian);
            s.ifd0.push((0x010F, Value::Ascii("Canon".into())));
            s.ifd0.push((0x0112, Value::Short(vec![6])));
            s.exif.push((0x829D, Value::Rational(vec![(8, 1)])));
            s.exif.push((0xA402, Value::Short(vec![2])));
            let out = s.build();
            // IFD0 at 8: Make, Orientation, the ExifIFD pointer.
            assert_eq!(ifd_value_field(&out, 8, 0x010F), 18);
            assert_eq!(ifd_value_field(&out, 8, 0x0112), 30);
            // IFD0 ends at 8 + 2 + 36 + 4 + 6 ("Canon\0") = 56.
            assert_eq!(exif_value_field(&out, 0x829D), 56 + 2 + 8);
            assert_eq!(exif_value_field(&out, 0xA402), 56 + 2 + 12 + 8);
        }
    }

    #[test]
    fn interop_ifd_follows_the_block_and_the_exif_ifd_points_at_it() {
        for little_endian in [true, false] {
            let mut s = spec(little_endian);
            s.exif.push((0xA001, Value::Short(vec![0xFFFF])));
            let out = s.build_with_interop(&[(0x0001, Value::Ascii("R03".into()))]);
            let mut with_pointer = s.clone();
            with_pointer.exif.push((0xA005, Value::Long(vec![0])));
            let start = with_pointer.build().len();
            assert_eq!(read_u32(&out, exif_value_field(&out, 0xA005)), start);
            // One inline entry: count, then tag 0x0001, ASCII, count 4, "R03\0".
            assert_eq!(read_u16(&out, start), 1);
            assert_eq!(read_u16(&out, start + 2), 0x0001);
            assert_eq!(read_u16(&out, start + 4), 2);
            assert_eq!(&out[start + 10..start + 14], b"R03\0");
            assert_eq!(out.len(), start + 2 + 12 + 4);
        }
    }

    #[test]
    #[should_panic(expected = "no tag 0x0110")]
    fn a_missing_value_field_panics() {
        let mut s = spec(true);
        s.ifd0.push((0x010F, Value::Ascii("Canon".into())));
        ifd_value_field(&s.build(), 8, 0x0110);
    }

    #[test]
    fn entries_are_sorted_by_tag() {
        let mut s = spec(true);
        s.ifd0.push((0x0112, Value::Short(vec![1])));
        s.ifd0.push((0x010F, Value::Ascii("ab".into())));
        let out = s.build();
        assert_eq!(&out[10..12], &[0x0F, 0x01]);
        assert_eq!(&out[22..24], &[0x12, 0x01]);
    }

    #[test]
    fn signed_and_rational_encodings() {
        let mut s = spec(false);
        s.ifd0.push((0x0001, Value::SShort(vec![-2])));
        s.ifd0.push((0x0002, Value::SRational(vec![(-2, 3)])));
        s.ifd0.push((0x0003, Value::Rational(vec![(1, 250)])));
        s.ifd0.push((0x0004, Value::Long(vec![7])));
        s.ifd0.push((0x0005, Value::Undefined(vec![9, 8])));
        let out = s.build();
        let entry = |i: usize| &out[10 + 12 * i..22 + 12 * i];
        assert_eq!(entry(0), &[0, 1, 0, 8, 0, 0, 0, 1, 0xFF, 0xFE, 0, 0]);
        // 5 entries: data starts at 8 + 2 + 60 + 4 = 74.
        assert_eq!(entry(1), &[0, 2, 0, 10, 0, 0, 0, 1, 0, 0, 0, 74]);
        assert_eq!(&out[74..82], &[0xFF, 0xFF, 0xFF, 0xFE, 0, 0, 0, 3]);
        assert_eq!(entry(2), &[0, 3, 0, 5, 0, 0, 0, 1, 0, 0, 0, 82]);
        assert_eq!(&out[82..90], &[0, 0, 0, 1, 0, 0, 0, 250]);
        assert_eq!(entry(3), &[0, 4, 0, 4, 0, 0, 0, 1, 0, 0, 0, 7]);
        assert_eq!(entry(4), &[0, 5, 0, 7, 0, 0, 0, 2, 9, 8, 0, 0]);
    }

    #[test]
    fn jpeg_gets_app1_right_after_soi_and_still_decodes() {
        let img = image::RgbImage::from_pixel(16, 8, image::Rgb([200, 100, 50]));
        let tiff = spec(true).build();
        let jpeg = jpeg_with_exif(&img, &tiff, 90);
        let len = (2 + 6 + tiff.len()) as u16;
        assert_eq!(&jpeg[..4], &[0xFF, 0xD8, 0xFF, 0xE1]);
        assert_eq!(&jpeg[4..6], &len.to_be_bytes());
        assert_eq!(&jpeg[6..12], b"Exif\0\0");
        assert_eq!(&jpeg[12..12 + tiff.len()], &tiff[..]);
        let decoded = image::load_from_memory(&jpeg).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (16, 8));
    }

    #[test]
    fn jpeg_gets_app2_icc_right_after_soi_and_still_decodes() {
        let img = image::RgbImage::from_pixel(16, 8, image::Rgb([200, 100, 50]));
        let icc = [7_u8; 40];
        let jpeg = jpeg_with_icc(&img, &icc, 90);
        let len = (2 + 12 + 2 + icc.len()) as u16;
        assert_eq!(&jpeg[..4], &[0xFF, 0xD8, 0xFF, 0xE2]);
        assert_eq!(&jpeg[4..6], &len.to_be_bytes());
        assert_eq!(&jpeg[6..18], b"ICC_PROFILE\0");
        assert_eq!(&jpeg[18..20], &[1, 1]);
        assert_eq!(&jpeg[20..20 + icc.len()], &icc[..]);
        let decoded = image::load_from_memory(&jpeg).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (16, 8));
    }
}
