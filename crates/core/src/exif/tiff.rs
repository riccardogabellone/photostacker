//! A bounds-checked TIFF/IFD reader over a byte slice. Every offset and count
//! comes from the file, so every access is checked: hostile input gives `Err`,
//! never a panic.

use std::ops::Range;

use super::ExifError;

/// TIFF header magics: 42 (TIFF), 0x4F52 and 0x5352 (Olympus ORF), 0x55 (Panasonic RW2).
const MAGICS: [u16; 4] = [42, 0x4F52, 0x5352, 0x55];

/// Size of one IFD entry: tag, type, count and the value/offset field.
const ENTRY_LEN: usize = 12;

/// A parsed TIFF header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Header {
    pub little_endian: bool,
    /// Offset of IFD0 from the header start.
    pub ifd0_offset: usize,
}

/// Reads the 8-byte header at the start of `data`.
pub(crate) fn header(data: &[u8]) -> Result<Header, ExifError> {
    let bad = || ExifError::BadTiffHeader {
        found: data[..data.len().min(8)].to_vec(),
    };
    let h = data.get(..8).ok_or_else(bad)?;
    let little_endian = match &h[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return Err(bad()),
    };
    if !MAGICS.contains(&u16_at(h, 2, little_endian)) {
        return Err(bad());
    }
    Ok(Header {
        little_endian,
        ifd0_offset: u32_at(h, 4, little_endian) as usize,
    })
}

/// One 12-byte IFD entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Entry {
    pub tag: u16,
    pub typ: u16,
    pub count: u32,
    /// Where the entry's 4-byte value/offset field sits in the data.
    field_pos: usize,
}

/// One IFD: its entries, read eagerly; values are resolved on demand.
#[derive(Debug)]
pub(crate) struct Ifd<'a> {
    data: &'a [u8],
    little_endian: bool,
    base: usize,
    name: &'static str,
    entries: Vec<Entry>,
    /// Where the next-IFD offset follows the entry table; not yet checked
    /// against the data.
    next_pos: usize,
}

impl<'a> Ifd<'a> {
    /// Reads the IFD at `offset` in `data`. Out-of-line values live at
    /// `base + value offset`. `name` (e.g. "IFD0") labels errors.
    pub(crate) fn parse(
        data: &'a [u8],
        little_endian: bool,
        offset: usize,
        base: usize,
        name: &'static str,
    ) -> Result<Self, ExifError> {
        let count_bytes = checked(data, offset, 2, || format!("{name} entry count"))?;
        let count = usize::from(u16_at(count_bytes, 0, little_endian));
        // `offset + 2` is in bounds, so it cannot overflow.
        let table_start = offset + 2;
        let table = checked(data, table_start, count * ENTRY_LEN, || {
            format!("{name} entry table ({count} entries)")
        })?;
        let entries = table
            .as_chunks::<ENTRY_LEN>()
            .0
            .iter()
            .enumerate()
            .map(|(i, e)| Entry {
                tag: u16_at(e, 0, little_endian),
                typ: u16_at(e, 2, little_endian),
                count: u32_at(e, 4, little_endian),
                field_pos: table_start + i * ENTRY_LEN + 8,
            })
            .collect();
        Ok(Self {
            data,
            little_endian,
            base,
            name,
            entries,
            // The table was bounds-checked, so its end cannot overflow.
            next_pos: table_start + count * ENTRY_LEN,
        })
    }

    pub(crate) fn entry(&self, tag: u16) -> Option<&Entry> {
        self.entries.iter().find(|e| e.tag == tag)
    }

    /// The offset of the next IFD in the chain, from the 4 bytes after the
    /// entry table; 0 ends the chain.
    pub(crate) fn next_offset(&self) -> Result<usize, ExifError> {
        let field = checked(self.data, self.next_pos, 4, || {
            format!("{} next-IFD offset", self.name)
        })?;
        Ok(u32_at(field, 0, self.little_endian) as usize)
    }

    /// Where the value of `entry` sits in the data; `None` for a type this
    /// reader does not know (outside 1–10 and 13).
    pub(crate) fn value_range(&self, entry: &Entry) -> Result<Option<Range<usize>>, ExifError> {
        let Some(size) = type_size(entry.typ) else {
            return Ok(None);
        };
        let len = u64::from(entry.count) * size;
        if len <= 4 {
            // The field itself was bounds-checked with the entry table.
            return Ok(Some(entry.field_pos..entry.field_pos + len as usize));
        }
        let rel = u32_at(self.data, entry.field_pos, self.little_endian);
        let out_of_bounds = || ExifError::OutOfBounds {
            what: format!("{} tag 0x{:04X} value", self.name, entry.tag),
            offset: (self.base as u64).saturating_add(u64::from(rel)),
            len,
            size: self.data.len(),
        };
        let start = self
            .base
            .checked_add(rel as usize)
            .ok_or_else(out_of_bounds)?;
        let len = usize::try_from(len).map_err(|_| out_of_bounds())?;
        range(self.data.len(), start, len)
            .map(Some)
            .ok_or_else(out_of_bounds)
    }

    /// An ASCII value with NULs and spaces trimmed; `None` when absent, of
    /// another type or empty.
    pub(crate) fn ascii(&self, tag: u16) -> Result<Option<String>, ExifError> {
        Ok(self
            .value(tag, &[2])?
            .and_then(|(_, bytes)| trim_text(bytes)))
    }

    /// The first element of a BYTE, SHORT or LONG value.
    pub(crate) fn uint(&self, tag: u16) -> Result<Option<u32>, ExifError> {
        let le = self.little_endian;
        Ok(self.value(tag, &[1, 3, 4])?.map(|(typ, b)| match typ {
            1 => u32::from(b[0]),
            3 => u32::from(u16_at(b, 0, le)),
            _ => u32_at(b, 0, le),
        }))
    }

    /// Every element of a SHORT, LONG or IFD value, such as the offsets of
    /// strips or of SubIFDs.
    pub(crate) fn uints(&self, tag: u16) -> Result<Option<Vec<u32>>, ExifError> {
        let le = self.little_endian;
        Ok(self.value(tag, &[3, 4, 13])?.map(|(typ, bytes)| match typ {
            3 => {
                let (words, _) = bytes.as_chunks::<2>();
                words.iter().map(|w| u32::from(u16_at(w, 0, le))).collect()
            }
            _ => {
                let (longs, _) = bytes.as_chunks::<4>();
                longs.iter().map(|l| u32_at(l, 0, le)).collect()
            }
        }))
    }

    /// The first element of a RATIONAL or SRATIONAL value; `None` when the
    /// denominator is 0.
    pub(crate) fn rational(&self, tag: u16) -> Result<Option<f64>, ExifError> {
        let le = self.little_endian;
        Ok(self.value(tag, &[5, 10])?.and_then(|(typ, b)| {
            let (num, den) = (u32_at(b, 0, le), u32_at(b, 4, le));
            let (num, den) = match typ {
                5 => (f64::from(num), f64::from(den)),
                _ => (f64::from(num as i32), f64::from(den as i32)),
            };
            (den != 0.0).then(|| num / den)
        }))
    }

    /// Every element of a SHORT or SSHORT value as `i16`, bits unchanged: the
    /// int16 word tables of Canon MakerNotes are stored either way.
    pub(crate) fn int16s(&self, tag: u16) -> Result<Option<Vec<i16>>, ExifError> {
        let le = self.little_endian;
        Ok(self.value(tag, &[3, 8])?.map(|(_, bytes)| {
            let (words, _) = bytes.as_chunks::<2>();
            words.iter().map(|w| u16_at(w, 0, le) as i16).collect()
        }))
    }

    /// The type and bytes of a non-empty value of one of `types`.
    fn value(&self, tag: u16, types: &[u16]) -> Result<Option<(u16, &'a [u8])>, ExifError> {
        let Some(entry) = self.entry(tag) else {
            return Ok(None);
        };
        if entry.count == 0 || !types.contains(&entry.typ) {
            return Ok(None);
        }
        let range = self.value_range(entry)?;
        Ok(range.map(|r| (entry.typ, &self.data[r])))
    }
}

/// Byte size of one element of TIFF type `typ` (1–10, and 13: IFD, an offset
/// stored like a LONG).
fn type_size(typ: u16) -> Option<u64> {
    match typ {
        1 | 2 | 6 | 7 => Some(1),
        3 | 8 => Some(2),
        4 | 9 | 13 => Some(4),
        5 | 10 => Some(8),
        _ => None,
    }
}

/// `offset..offset + len` when it fits in `size` bytes.
fn range(size: usize, offset: usize, len: usize) -> Option<Range<usize>> {
    let end = offset.checked_add(len)?;
    (end <= size).then_some(offset..end)
}

/// `len` bytes of `data` at `offset`, or an error naming what was being read.
fn checked(
    data: &[u8],
    offset: usize,
    len: usize,
    what: impl FnOnce() -> String,
) -> Result<&[u8], ExifError> {
    match range(data.len(), offset, len) {
        Some(r) => Ok(&data[r]),
        None => Err(ExifError::OutOfBounds {
            what: what(),
            offset: offset as u64,
            len: len as u64,
            size: data.len(),
        }),
    }
}

/// The u16 at `at`; the caller has bounds-checked `at..at + 2`.
fn u16_at(b: &[u8], at: usize, little_endian: bool) -> u16 {
    let bytes = [b[at], b[at + 1]];
    if little_endian {
        u16::from_le_bytes(bytes)
    } else {
        u16::from_be_bytes(bytes)
    }
}

/// The u32 at `at`; the caller has bounds-checked `at..at + 4`.
fn u32_at(b: &[u8], at: usize, little_endian: bool) -> u32 {
    let bytes = [b[at], b[at + 1], b[at + 2], b[at + 3]];
    if little_endian {
        u32::from_le_bytes(bytes)
    } else {
        u32::from_be_bytes(bytes)
    }
}

/// Text with leading NULs/spaces dropped, cut at the first NUL, trailing spaces
/// dropped; `None` when nothing is left.
fn trim_text(bytes: &[u8]) -> Option<String> {
    let start = bytes.iter().position(|&b| b != 0 && b != b' ')?;
    let rest = &bytes[start..];
    let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
    let text = String::from_utf8_lossy(&rest[..end]);
    Some(text.trim_end_matches(' ').to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use photostacker_testkit::{TiffSpec, Value};

    fn block(little_endian: bool, ifd0: Vec<(u16, Value)>) -> Vec<u8> {
        TiffSpec {
            little_endian,
            ifd0,
            exif: Vec::new(),
            maker_note: None,
        }
        .build()
    }

    fn ifd0(data: &[u8]) -> Result<Ifd<'_>, ExifError> {
        let h = header(data)?;
        Ifd::parse(data, h.little_endian, h.ifd0_offset, 0, "IFD0")
    }

    #[test]
    fn header_accepts_tiff_orf_and_rw2_magics() {
        let cases: [(&[u8], bool, usize); 6] = [
            (b"II\x2A\x00\x08\x00\x00\x00", true, 8),
            (b"MM\x00\x2A\x00\x00\x00\x10", false, 16),
            (b"IIRO\x08\x00\x00\x00", true, 8),
            (b"IIRS\x08\x00\x00\x00", true, 8),
            (b"MMOR\x00\x00\x00\x08", false, 8),
            (b"IIU\x00\x18\x00\x00\x00", true, 24),
        ];
        for (bytes, little_endian, ifd0_offset) in cases {
            let expected = Header {
                little_endian,
                ifd0_offset,
            };
            assert_eq!(header(bytes).unwrap(), expected, "{bytes:?}");
        }
    }

    #[test]
    fn header_rejects_bad_order_magic_and_short_input() {
        for bytes in [
            &b"XX\x2A\x00\x08\x00\x00\x00"[..],
            b"II\x2B\x00\x08\x00\x00\x00",
            b"MM\x2A\x00\x00\x00\x00\x08",
            b"II\x2A\x00\x08\x00",
            b"",
        ] {
            assert!(header(bytes).is_err(), "{bytes:?}");
        }
    }

    #[test]
    fn reads_typed_values_in_both_byte_orders() {
        for little_endian in [true, false] {
            let data = block(
                little_endian,
                vec![
                    (0x0001, Value::Ascii("  Canon \0".into())),
                    (0x0002, Value::Ascii("abc".into())),
                    (0x0003, Value::Short(vec![6, 7])),
                    (0x0004, Value::Long(vec![70_000])),
                    (0x0005, Value::Rational(vec![(1, 250)])),
                    (0x0006, Value::SRational(vec![(-2, 3)])),
                ],
            );
            let ifd = ifd0(&data).unwrap();
            assert_eq!(ifd.ascii(0x0001).unwrap().as_deref(), Some("Canon"));
            assert_eq!(ifd.ascii(0x0002).unwrap().as_deref(), Some("abc"));
            assert_eq!(ifd.uint(0x0003).unwrap(), Some(6));
            assert_eq!(ifd.uint(0x0004).unwrap(), Some(70_000));
            assert_eq!(ifd.rational(0x0005).unwrap(), Some(0.004));
            assert!((ifd.rational(0x0006).unwrap().unwrap() + 2.0 / 3.0).abs() < 1e-12);
            assert_eq!(ifd.ascii(0x0099).unwrap(), None);
        }
    }

    #[test]
    fn mismatched_types_empty_strings_and_zero_denominators_are_none() {
        let data = block(
            true,
            vec![
                (0x0001, Value::Short(vec![1])),
                (0x0002, Value::Ascii(" \0 ".into())),
                (0x0003, Value::Rational(vec![(1, 0)])),
                (0x0004, Value::Short(Vec::new())),
            ],
        );
        let ifd = ifd0(&data).unwrap();
        assert_eq!(ifd.ascii(0x0001).unwrap(), None);
        assert_eq!(ifd.rational(0x0001).unwrap(), None);
        assert_eq!(ifd.uint(0x0002).unwrap(), None);
        assert_eq!(ifd.ascii(0x0002).unwrap(), None);
        assert_eq!(ifd.rational(0x0003).unwrap(), None);
        assert_eq!(ifd.uint(0x0004).unwrap(), None);
    }

    #[test]
    fn int16s_reads_short_and_sshort_arrays() {
        for little_endian in [true, false] {
            let data = block(
                little_endian,
                vec![
                    (0x0001, Value::Short(vec![8, 0xFFFF, 0x8000, 7])),
                    (0x0002, Value::SShort(vec![-32, 64])),
                    (0x0003, Value::SShort(vec![-1])),
                    (0x0004, Value::Long(vec![1, 2])),
                    (0x0005, Value::Short(Vec::new())),
                ],
            );
            let ifd = ifd0(&data).unwrap();
            let words = ifd.int16s(0x0001).unwrap();
            assert_eq!(words, Some(vec![8, -1, i16::MIN, 7]));
            assert_eq!(ifd.int16s(0x0002).unwrap(), Some(vec![-32, 64]));
            assert_eq!(ifd.int16s(0x0003).unwrap(), Some(vec![-1]));
            assert_eq!(ifd.int16s(0x0004).unwrap(), None);
            assert_eq!(ifd.int16s(0x0005).unwrap(), None);
            assert_eq!(ifd.int16s(0x0099).unwrap(), None);
        }
        // An out-of-line array past the end is an error.
        let mut data = block(true, vec![(0x0001, Value::Short(vec![1, 2, 3]))]);
        data[18..22].copy_from_slice(&0xFFFF_FF00u32.to_le_bytes());
        assert!(ifd0(&data).unwrap().int16s(0x0001).is_err());
    }

    #[test]
    fn unknown_type_has_no_value() {
        let mut data = block(true, vec![(0x0001, Value::Short(vec![1]))]);
        data[12..14].copy_from_slice(&99u16.to_le_bytes()); // entry 0 type
        let ifd = ifd0(&data).unwrap();
        let entry = *ifd.entry(0x0001).unwrap();
        assert_eq!(ifd.value_range(&entry).unwrap(), None);
        assert_eq!(ifd.uint(0x0001).unwrap(), None);
    }

    #[test]
    fn entry_count_past_the_end_errors() {
        let mut data = block(true, vec![(0x0001, Value::Short(vec![1]))]);
        data[8..10].copy_from_slice(&0xFFFFu16.to_le_bytes());
        assert!(ifd0(&data).is_err());
    }

    #[test]
    fn ifd_offset_past_the_end_errors() {
        let data = block(true, vec![(0x0001, Value::Short(vec![1]))]);
        for offset in [data.len(), data.len() - 1, usize::MAX] {
            assert!(
                Ifd::parse(&data, true, offset, 0, "IFD0").is_err(),
                "{offset}"
            );
        }
    }

    #[test]
    fn value_offset_past_the_end_errors() {
        let mut data = block(true, vec![(0x0001, Value::Ascii("Canon".into()))]);
        data[18..22].copy_from_slice(&0xFFFF_FF00u32.to_le_bytes()); // entry 0 offset
        let ifd = ifd0(&data).unwrap();
        assert!(ifd.ascii(0x0001).is_err());
    }

    #[test]
    fn huge_count_and_base_overflow_error() {
        let mut data = block(true, vec![(0x0001, Value::Long(vec![1, 2]))]);
        data[14..18].copy_from_slice(&u32::MAX.to_le_bytes()); // entry 0 count
        let ifd = ifd0(&data).unwrap();
        assert!(ifd.uint(0x0001).is_err());

        let data = block(true, vec![(0x0001, Value::Long(vec![1, 2]))]);
        let ifd = Ifd::parse(&data, true, 8, usize::MAX, "IFD0").unwrap();
        assert!(ifd.uint(0x0001).is_err());
    }

    #[test]
    fn uints_read_short_long_and_ifd_arrays() {
        for little_endian in [true, false] {
            let mut data = block(
                little_endian,
                vec![
                    (0x0001, Value::Short(vec![7, 0xFFFF])),
                    (0x0002, Value::Long(vec![1, 70_000, 3])),
                    (0x0003, Value::Long(vec![9])),
                    (0x0004, Value::Rational(vec![(1, 2)])),
                ],
            );
            // Entry 2 (tag 3) as type 13, IFD.
            let at = 10 + 2 * 12 + 2;
            let ifd_type = if little_endian { [13, 0] } else { [0, 13] };
            data[at..at + 2].copy_from_slice(&ifd_type);
            let ifd = ifd0(&data).unwrap();
            assert_eq!(ifd.uints(0x0001).unwrap(), Some(vec![7, 0xFFFF]));
            assert_eq!(ifd.uints(0x0002).unwrap(), Some(vec![1, 70_000, 3]));
            assert_eq!(ifd.uints(0x0003).unwrap(), Some(vec![9]));
            assert_eq!(ifd.uints(0x0004).unwrap(), None);
            assert_eq!(ifd.uints(0x0099).unwrap(), None);
        }
    }

    #[test]
    fn next_offset_follows_the_entry_table() {
        let mut data = block(true, vec![(0x0001, Value::Short(vec![1]))]);
        let ifd = ifd0(&data).unwrap();
        assert_eq!(ifd.next_offset().unwrap(), 0);
        data[22..26].copy_from_slice(&0x1234u32.to_le_bytes());
        assert_eq!(ifd0(&data).unwrap().next_offset().unwrap(), 0x1234);
        // The table fits, but the next-IFD offset is cut.
        let cut = &data[..24];
        assert!(ifd0(cut).unwrap().next_offset().is_err());
    }

    #[test]
    fn inline_values_ignore_base() {
        let data = block(true, vec![(0x0001, Value::Short(vec![5]))]);
        let ifd = Ifd::parse(&data, true, 8, usize::MAX, "IFD0").unwrap();
        assert_eq!(ifd.uint(0x0001).unwrap(), Some(5));
    }
}
