//! The JPEGs embedded in a RAW file, found by our own container parsing: the
//! fallback for RAWs that rawler cannot decode, most often because it does
//! not know the camera body (spec §5.3).
//!
//! The container is told by content, as rawler does, never by extension:
//! - **RAF** (`FUJIFILMCCD-RAW ` magic): the JPEG the header points at.
//! - **CR3** (an ISO-BMFF `ftyp` box first): the JPEG of the `PRVW` box.
//! - **TIFF family** (`II` or `MM`): IFD0, its `next` chain and the SubIFDs
//!   (0x014A) of each, one level deep. In each IFD the candidates are
//!   `JPEGInterchangeFormat`/`Length` (0x0201/0x0202), and the one strip of
//!   an IFD with Compression 6 or 7.
//! - Anything else has no preview we know of.
//!
//! The candidates come largest first, by pixel count; a tie keeps the walk
//! order. The callers try them in that order. A candidate whose bytes overlap
//! one before it in that order is dropped, so the candidates never hold more
//! than the file's bytes, however many IFDs point at the same range. A
//! candidate's size comes from
//! its first frame header (SOF0, SOF1 or SOF2), read without decoding. A
//! candidate that does not start with a JPEG SOI, or whose first frame header
//! is another kind (such as the lossless SOF3 of CR2 and DNG raw data) or
//! missing, is skipped.
//!
//! Every offset and length comes from the file and is bounds-checked, never
//! trusted. In the TIFF walk, one bad pointer drops only what it points at:
//! - a JPEG outside the file, or a tag that cannot be read, drops that
//!   candidate;
//! - a SubIFD or next IFD that cannot be read, or that was read before (a
//!   loop), drops that IFD; a bad next pointer ends the chain;
//! - once [`MAX_IFDS`] IFDs have been read, the walk ends.
//!
//! The other candidates stay. Only a TIFF header or an IFD0 that cannot be
//! read is an error, as is a malformed RAF header or CR3 box: those
//! containers hold one candidate. The whole file is read into memory by
//! [`container::read_file`], never mapped (spec §4.3).

use std::cmp::Reverse;
use std::ops::Range;
use std::path::Path;

use super::DecodeError;
use crate::exif::ExifError;
use crate::exif::container;
use crate::exif::tiff::{self, Ifd};

const TAG_COMPRESSION: u16 = 0x0103;
const TAG_STRIP_OFFSETS: u16 = 0x0111;
const TAG_STRIP_BYTE_COUNTS: u16 = 0x0117;
const TAG_SUB_IFDS: u16 = 0x014A;
const TAG_JPEG_OFFSET: u16 = 0x0201;
const TAG_JPEG_LENGTH: u16 = 0x0202;
/// Compression 6 (old-style JPEG, as in CR2) and 7 (JPEG, as in DNG).
const JPEG_COMPRESSIONS: [u32; 2] = [6, 7];
/// The most IFDs read from one file. Real RAWs have fewer than ten; the cap
/// keeps a hostile file from making the walk quadratic.
const MAX_IFDS: usize = 64;

const SOI: [u8; 2] = [0xFF, 0xD8];

/// The JPEGs embedded in the RAW at `path`, largest first, copied out of the
/// file; empty when it has none. See the module doc.
pub(crate) fn embedded_jpegs(path: &Path) -> Result<Vec<Vec<u8>>, DecodeError> {
    let file = container::read_file(path).map_err(from_exif)?;
    Ok(jpegs(&file)?.into_iter().map(<[u8]>::to_vec).collect())
}

/// The first of [`embedded_jpegs`], the only one copied out of the file.
pub(crate) fn largest_embedded_jpeg(path: &Path) -> Result<Option<Vec<u8>>, DecodeError> {
    let file = container::read_file(path).map_err(from_exif)?;
    Ok(jpegs(&file)?.first().map(|jpeg| jpeg.to_vec()))
}

/// [`embedded_jpegs`] over the bytes of a whole file. Every candidate is a
/// slice of `file`, and no two of them overlap, so copying them all out
/// takes at most the file's length.
fn jpegs(file: &[u8]) -> Result<Vec<&[u8]>, DecodeError> {
    let candidates = if file.starts_with(container::RAF_MAGIC) {
        vec![container::raf_jpeg(file).map_err(from_exif)?]
    } else if file.get(4..8) == Some(&b"ftyp"[..]) {
        Vec::from_iter(container::cr3_prvw(file).map_err(from_exif)?)
    } else if file.starts_with(b"II") || file.starts_with(b"MM") {
        tiff_candidates(file)?
    } else {
        Vec::new()
    };
    let mut sized: Vec<(u64, &[u8])> = candidates
        .into_iter()
        .filter_map(|jpeg| {
            let (width, height) = jpeg_dimensions(jpeg)?;
            Some((u64::from(width) * u64::from(height), jpeg))
        })
        .collect();
    // A stable sort: a tie keeps the walk order.
    sized.sort_by_key(|&(pixels, _)| Reverse(pixels));
    // Several IFDs may point at the same bytes (a hostile file, up to 128
    // times): keep a candidate only when it overlaps none kept before it.
    let mut kept: Vec<(Range<usize>, &[u8])> = Vec::new();
    for (_, jpeg) in sized {
        let range = range_in(file, jpeg);
        if kept
            .iter()
            .all(|(other, _)| range.end <= other.start || other.end <= range.start)
        {
            kept.push((range, jpeg));
        }
    }
    Ok(kept.into_iter().map(|(_, jpeg)| jpeg).collect())
}

/// Where `part`, a slice of `file`, lies in it.
fn range_in(file: &[u8], part: &[u8]) -> Range<usize> {
    let start = part.as_ptr() as usize - file.as_ptr() as usize;
    start..start + part.len()
}

/// The JPEG candidates of a TIFF-family file, in walk order: each IFD of the
/// chain, then its SubIFDs. Only a header or an IFD0 that cannot be read is
/// an error; past IFD0, a bad pointer loses only what it points at.
fn tiff_candidates(file: &[u8]) -> Result<Vec<&[u8]>, DecodeError> {
    let header = tiff::header(file).map_err(from_exif)?;
    let mut walk = Walk {
        file,
        little_endian: header.little_endian,
        read: Vec::new(),
    };
    let mut found = Vec::new();
    let mut ifd = match header.ifd0_offset {
        0 => None,
        offset => Some(walk.ifd0(offset)?),
    };
    while let Some(current) = ifd {
        collect(&current, file, &mut found);
        let subifds = current.uints(TAG_SUB_IFDS).ok().flatten();
        for offset in subifds.unwrap_or_default() {
            if walk.is_full() {
                break;
            }
            if let Some(subifd) = walk.ifd(offset as usize) {
                collect(&subifd, file, &mut found);
            }
        }
        // A next-IFD pointer that cannot be read ends the chain, and so does
        // a full walk.
        ifd = match current.next_offset() {
            _ if walk.is_full() => None,
            Ok(0) | Err(_) => None,
            Ok(next) => walk.ifd(next),
        };
    }
    Ok(found)
}

/// The IFDs of one TIFF walk, each read once.
struct Walk<'a> {
    file: &'a [u8],
    little_endian: bool,
    /// The offsets of the IFDs read so far.
    read: Vec<usize>,
}

impl<'a> Walk<'a> {
    /// Reads IFD0, the root of the walk: without it there is no walk.
    fn ifd0(&mut self, offset: usize) -> Result<Ifd<'a>, DecodeError> {
        self.read.push(offset);
        Ifd::parse(self.file, self.little_endian, offset, 0, "IFD0").map_err(from_exif)
    }

    /// Whether [`MAX_IFDS`] IFDs have been read: the walk then ends.
    fn is_full(&self) -> bool {
        self.read.len() >= MAX_IFDS
    }

    /// Reads the IFD at `offset`. `None` when it cannot be read or when it
    /// was read before (a loop). Never called once the walk is full.
    fn ifd(&mut self, offset: usize) -> Option<Ifd<'a>> {
        debug_assert!(!self.is_full(), "the walk ends once it is full");
        if self.is_full() || self.read.contains(&offset) {
            return None;
        }
        self.read.push(offset);
        Ifd::parse(self.file, self.little_endian, offset, 0, "IFD").ok()
    }
}

/// Adds the JPEG candidates of `ifd` to `found`. A tag that cannot be read,
/// or a candidate that lies outside the file, adds nothing.
fn collect<'a>(ifd: &Ifd<'a>, file: &'a [u8], found: &mut Vec<&'a [u8]>) {
    let uint = |tag| ifd.uint(tag).ok().flatten();
    if let (Some(offset), Some(len)) = (uint(TAG_JPEG_OFFSET), uint(TAG_JPEG_LENGTH)) {
        found.extend(bytes_at(file, offset, len));
    }
    if uint(TAG_COMPRESSION).is_some_and(|c| JPEG_COMPRESSIONS.contains(&c)) {
        let uints = |tag| ifd.uints(tag).ok().flatten();
        let (offsets, counts) = (uints(TAG_STRIP_OFFSETS), uints(TAG_STRIP_BYTE_COUNTS));
        if let (Some(&[offset]), Some(&[count])) = (offsets.as_deref(), counts.as_deref()) {
            found.extend(bytes_at(file, offset, count));
        }
    }
}

/// The `len` bytes at `offset` of `file`, when they are all inside it.
fn bytes_at(file: &[u8], offset: u32, len: u32) -> Option<&[u8]> {
    let start = offset as usize;
    file.get(start..start.checked_add(len as usize)?)
}

/// The width and height in the first frame header of `jpeg`, when that is
/// SOF0, SOF1 or SOF2 (baseline, extended or progressive Huffman) with no
/// zero side. `None` when `jpeg` does not start with SOI, when the header
/// segments end (at SOS, EOI or the end of the data) before a frame header,
/// or when the first frame header is of another kind.
pub(crate) fn jpeg_dimensions(jpeg: &[u8]) -> Option<(u32, u32)> {
    if !jpeg.starts_with(&SOI) {
        return None;
    }
    let mut pos = SOI.len();
    loop {
        if *jpeg.get(pos)? != 0xFF {
            return None;
        }
        // Any number of 0xFF fill bytes may precede the marker code.
        while jpeg.get(pos) == Some(&0xFF) {
            pos += 1;
        }
        let marker = *jpeg.get(pos)?;
        pos += 1;
        match marker {
            // Standalone markers carry no length.
            0x01 | 0xD0..=0xD7 => continue,
            // A stuffed byte, another SOI, EOI or SOS: no frame header first.
            0x00 | 0xD8 | 0xD9 | 0xDA => return None,
            _ => {}
        }
        let len = usize::from(u16::from_be_bytes([*jpeg.get(pos)?, *jpeg.get(pos + 1)?]));
        match marker {
            0xC0..=0xC2 => {
                // Length, precision, then height and width.
                let frame = jpeg.get(pos..pos + 7)?;
                let height = u32::from(u16::from_be_bytes([frame[3], frame[4]]));
                let width = u32::from(u16::from_be_bytes([frame[5], frame[6]]));
                return (width > 0 && height > 0).then_some((width, height));
            }
            // The other frame headers: lossless, hierarchical or arithmetic.
            0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => return None,
            _ if len < 2 => return None,
            _ => pos += len,
        }
    }
}

/// An error of the EXIF readers as a decode error: I/O stays I/O, a file too
/// large to read whole is unsupported, the rest is a malformed file.
fn from_exif(err: ExifError) -> DecodeError {
    match err {
        ExifError::Io { source, .. } => DecodeError::Io(source),
        too_large @ ExifError::TooLarge { .. } => DecodeError::Unsupported(too_large.to_string()),
        other => DecodeError::Corrupt(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use photostacker_testkit::{
        TiffSpec, Value, cr3_with_cmt, cr3_with_prvw, encode_jpeg, ifd_value_field, raf_with_jpeg,
        scene, tiff_raw_with_jpegs,
    };

    use super::*;

    fn jpeg_of(width: u32, height: u32) -> Vec<u8> {
        encode_jpeg(&scene(7, width, height))
    }

    /// The sizes of `found`, in order.
    fn sizes(found: &[&[u8]]) -> Vec<(u32, u32)> {
        found.iter().map(|j| jpeg_dimensions(j).unwrap()).collect()
    }

    fn tiff(ifd0: Vec<(u16, Value)>) -> Vec<u8> {
        TiffSpec {
            little_endian: true,
            ifd0,
            exif: Vec::new(),
            maker_note: None,
        }
        .build()
    }

    #[test]
    fn jpegs_come_largest_first_across_ifds_and_subifds() {
        type Size = (u32, u32);
        let cases: [(&[Size], &[Size]); 5] = [
            // SubIFD 1 (Compression 7), SubIFD 2, then the IFD0 thumbnail.
            (
                &[(160, 120), (1024, 683), (640, 427)],
                &[(1024, 683), (640, 427), (160, 120)],
            ),
            // SubIFD 2 (Compression 6) first.
            (
                &[(160, 120), (300, 200), (800, 533)],
                &[(800, 533), (300, 200), (160, 120)],
            ),
            // The IFD0 thumbnail itself.
            (&[(1200, 800), (300, 200)], &[(1200, 800), (300, 200)]),
            // A tie keeps the walk order: IFD0, then its SubIFDs.
            (&[(300, 200), (200, 300)], &[(300, 200), (200, 300)]),
            (&[(320, 240)], &[(320, 240)]),
        ];
        for (previews, largest_first) in cases {
            let raw = tiff_raw_with_jpegs(previews);
            let found = jpegs(&raw).unwrap();
            assert_eq!(sizes(&found), largest_first, "{previews:?}");
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unknown.nef");
        fs::write(&path, tiff_raw_with_jpegs(&[(160, 120), (1024, 683)])).unwrap();
        let found = embedded_jpegs(&path).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(jpeg_dimensions(&found[0]), Some((1024, 683)));
        assert_eq!(jpeg_dimensions(&found[1]), Some((160, 120)));
        assert!(found.iter().all(|j| image::load_from_memory(j).is_ok()));
    }

    #[test]
    fn bad_pointer_keeps_other_candidates() {
        let mut raw = tiff_raw_with_jpegs(&[(160, 120), (1024, 683)]);
        let subifd_jpeg = jpegs(&raw).unwrap()[0].to_vec();
        assert_eq!(jpeg_dimensions(&subifd_jpeg), Some((1024, 683)));
        // IFD0's JPEGInterchangeFormat (its thumbnail) now points past the end.
        let field = ifd_value_field(&raw, 8, TAG_JPEG_OFFSET);
        let past_the_end = u32::try_from(raw.len()).unwrap() + 1;
        raw[field..field + 4].copy_from_slice(&past_the_end.to_le_bytes());
        assert_eq!(jpegs(&raw).unwrap(), [&subifd_jpeg[..]]);
    }

    #[test]
    fn ifds_pointing_at_the_same_jpeg_give_one_candidate() {
        let mut raw = tiff_raw_with_jpegs(&[(160, 120), (1024, 683), (640, 427)]);
        let u32_at = |raw: &[u8], at: usize| {
            u32::from_le_bytes(raw[at..at + 4].try_into().unwrap()) as usize
        };
        let subifds = u32_at(&raw, ifd_value_field(&raw, 8, TAG_SUB_IFDS));
        let (sub1, sub2) = (u32_at(&raw, subifds), u32_at(&raw, subifds + 4));
        let strip = |raw: &[u8], ifd| {
            (
                ifd_value_field(raw, ifd, TAG_STRIP_OFFSETS),
                ifd_value_field(raw, ifd, TAG_STRIP_BYTE_COUNTS),
            )
        };
        let (offset_field, count_field) = strip(&raw, sub1);
        let (offset, count) = (u32_at(&raw, offset_field), u32_at(&raw, count_field));
        let shared = raw[offset..offset + count].to_vec();
        assert_eq!(jpeg_dimensions(&shared), Some((1024, 683)));
        // IFD0's thumbnail and SubIFD 2's strip now point at SubIFD 1's JPEG;
        // IFD0's runs 16 bytes further, overlapping without being equal.
        let ifd0_fields = (
            ifd_value_field(&raw, 8, TAG_JPEG_OFFSET),
            ifd_value_field(&raw, 8, TAG_JPEG_LENGTH),
        );
        let sub2_fields = strip(&raw, sub2);
        let mut point = |offset_field: usize, count_field: usize, len: usize| {
            raw[offset_field..offset_field + 4].copy_from_slice(&(offset as u32).to_le_bytes());
            raw[count_field..count_field + 4].copy_from_slice(&(len as u32).to_le_bytes());
        };
        point(ifd0_fields.0, ifd0_fields.1, count + 16);
        point(sub2_fields.0, sub2_fields.1, count);
        let found = jpegs(&raw).unwrap();
        assert_eq!(found.len(), 1, "{:?}", sizes(&found));
        // The tie keeps the walk order: IFD0's longer range comes first.
        assert_eq!(found[0], &raw[offset..offset + count + 16]);
    }

    /// A little-endian IFD of LONG entries, each value inline, ending with
    /// the `next` pointer.
    fn long_ifd(entries: &[(u16, u32)], next: u32) -> Vec<u8> {
        let mut out = u16::try_from(entries.len()).unwrap().to_le_bytes().to_vec();
        for &(tag, value) in entries {
            out.extend(tag.to_le_bytes());
            out.extend(4_u16.to_le_bytes());
            out.extend(1_u32.to_le_bytes());
            out.extend(value.to_le_bytes());
        }
        out.extend(next.to_le_bytes());
        out
    }

    #[test]
    fn the_walk_ends_once_it_has_read_max_ifds() {
        assert_eq!(MAX_IFDS, 64);
        // IFD0 lists 70 SubIFDs and has a next IFD. The walk reads IFD0, then
        // SubIFDs 0..=62: SubIFD 62 is the 64th IFD read, so its JPEG is
        // found. SubIFD 63 and the next IFD would be the 65th and later, so
        // theirs are not. `Walk::ifd` asserts that it is not called again
        // once the walk is full, so the walk must end there, in both loops.
        const SUBIFDS: usize = 70;
        let ifd0 = |subifds: Vec<u32>| tiff(vec![(TAG_SUB_IFDS, Value::Long(subifds))]);
        let head_len = ifd0(vec![0; SUBIFDS]).len();
        // Every IFD after IFD0 has two entries: 2 + 2 × 12 + 4 bytes.
        let ifd_len = 30;
        let at = |k: usize| u32::try_from(head_len + k * ifd_len).unwrap();
        let next_ifd = SUBIFDS;
        let previews = [jpeg_of(64, 48), jpeg_of(80, 60), jpeg_of(96, 72)];
        let mut jpeg_at = Vec::new();
        let mut end = at(next_ifd + 1);
        for jpeg in &previews {
            jpeg_at.push(end);
            end += u32::try_from(jpeg.len()).unwrap();
        }
        let jpeg_entries = |i: usize| {
            let len = u32::try_from(previews[i].len()).unwrap();
            [(TAG_JPEG_OFFSET, jpeg_at[i]), (TAG_JPEG_LENGTH, len)]
        };
        let filler = [(0x00FE, 1), (TAG_COMPRESSION, 1)];

        let mut raw = ifd0((0..SUBIFDS).map(at).collect());
        // IFD0's next pointer follows its one entry.
        raw[22..26].copy_from_slice(&at(next_ifd).to_le_bytes());
        for k in 0..SUBIFDS {
            let entries = match k {
                62 => jpeg_entries(0),
                63 => jpeg_entries(1),
                _ => filler,
            };
            raw.extend(long_ifd(&entries, 0));
        }
        raw.extend(long_ifd(&jpeg_entries(2), 0));
        raw.extend(previews.concat());
        assert_eq!(raw.len(), end as usize, "precondition: the layout");

        assert_eq!(sizes(&jpegs(&raw).unwrap()), [(64, 48)]);
    }

    #[test]
    fn raf_header_jpeg_is_found() {
        let jpeg = jpeg_of(800, 600);
        let raf = raf_with_jpeg(&jpeg);
        assert_eq!(jpegs(&raf).unwrap(), [&jpeg[..]]);
    }

    #[test]
    fn cr3_prvw_is_found() {
        let jpeg = jpeg_of(540, 360);
        let cr3 = cr3_with_prvw(&jpeg);
        // Exactly the declared length: the box's padding is left out.
        assert_eq!(jpegs(&cr3).unwrap(), [&jpeg[..]]);
    }

    #[test]
    fn no_jpeg_is_none() {
        let canon = tiff(vec![
            (0x010F, Value::Ascii("Canon".into())),
            (0x0110, Value::Ascii("Canon EOS 5D Mark IV".into())),
        ]);
        let mut not_a_jpeg = tiff_raw_with_jpegs(&[(64, 48)]);
        let soi = not_a_jpeg
            .windows(3)
            .position(|w| w == [0xFF, 0xD8, 0xFF])
            .unwrap();
        not_a_jpeg[soi + 1] = 0x00;
        // A lossless (SOF3) JPEG, as in the raw data of a CR2 or a DNG.
        let lossless = [
            0xFF, 0xD8, 0xFF, 0xC3, 0x00, 0x0B, 0x0C, 0x00, 0x10, 0x00, 0x10, 0x01, 0x01, 0x11,
            0x00, 0xFF, 0xD9,
        ];
        let cases: [(&str, Vec<u8>); 8] = [
            ("TIFF without previews", canon.clone()),
            ("TIFF whose thumbnail is not a JPEG", not_a_jpeg),
            (
                "RAF JPEG without a frame header",
                raf_with_jpeg(&[0xFF, 0xD8, 0xFF, 0xD9]),
            ),
            ("RAF lossless JPEG", raf_with_jpeg(&lossless)),
            (
                "CR3 without a PRVW box",
                cr3_with_cmt(&canon, &canon, &canon),
            ),
            ("a JPEG", jpeg_of(64, 48)),
            ("a PNG signature", b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec()),
            ("empty", Vec::new()),
        ];
        for (name, bytes) in &cases {
            assert!(jpegs(bytes).unwrap().is_empty(), "{name}");
        }
    }

    #[test]
    fn cuts_keep_the_whole_candidates_and_never_panic() {
        let raw = tiff_raw_with_jpegs(&[(160, 120), (1024, 683), (320, 213)]);
        // Each candidate of the whole file, and where it ends in it.
        let whole: Vec<(&[u8], usize)> = jpegs(&raw)
            .unwrap()
            .into_iter()
            .map(|jpeg| {
                (
                    jpeg,
                    jpeg.as_ptr() as usize - raw.as_ptr() as usize + jpeg.len(),
                )
            })
            .collect();
        let first_jpeg_end = whole.iter().map(|&(_, end)| end).min().unwrap();
        for keep in (0..=raw.len()).step_by(13).chain([raw.len()]) {
            match jpegs(&raw[..keep]) {
                // A cut header or IFD0, long before the JPEGs.
                Err(err) => {
                    assert!(keep >= 2 && keep < first_jpeg_end, "{keep}: {err}");
                    assert!(matches!(err, DecodeError::Corrupt(_)), "{keep}: {err:?}");
                }
                // Exactly the candidates that end before the cut, in order.
                Ok(found) => {
                    let expected: Vec<&[u8]> = whole
                        .iter()
                        .filter(|&&(_, end)| end <= keep)
                        .map(|&(jpeg, _)| jpeg)
                        .collect();
                    assert_eq!(found, expected, "{keep}");
                }
            }
        }

        // A JPEG offset at the end of the address space, and IFD loops: no
        // candidate, no error.
        let past_the_end = tiff(vec![
            (0x0201, Value::Long(vec![u32::MAX])),
            (0x0202, Value::Long(vec![16])),
        ]);
        let subifd_is_ifd0 = tiff(vec![(0x014A, Value::Long(vec![8]))]);
        let mut next_is_ifd0 = tiff(vec![(0x0103, Value::Short(vec![1]))]);
        next_is_ifd0[22..26].copy_from_slice(&8u32.to_le_bytes());
        for (name, bytes) in [
            ("JPEG past the end", past_the_end),
            ("SubIFD loop", subifd_is_ifd0),
            ("next IFD loop", next_is_ifd0),
        ] {
            assert!(jpegs(&bytes).unwrap().is_empty(), "{name}");
        }
        // But an IFD0 that cannot be read is an error.
        let mut ifd0_past_the_end = tiff(vec![(0x0103, Value::Short(vec![1]))]);
        ifd0_past_the_end[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        let found = jpegs(&ifd0_past_the_end);
        assert!(matches!(found, Err(DecodeError::Corrupt(_))), "{found:?}");

        // Cut CR3 and RAF previews.
        let jpeg = jpeg_of(64, 48);
        for container in [cr3_with_prvw(&jpeg), raf_with_jpeg(&jpeg)] {
            let end = container
                .windows(jpeg.len())
                .position(|w| w == jpeg)
                .unwrap()
                + jpeg.len();
            for keep in (0..end).step_by(13) {
                // Not every cut is an error (an empty file is no container), but none panics.
                if let Ok(found) = jpegs(&container[..keep]) {
                    assert!(found.is_empty(), "{keep}: found {} JPEGs", found.len());
                }
            }
        }
    }

    #[test]
    fn jpeg_dimensions_read_the_first_baseline_or_progressive_frame() {
        assert_eq!(jpeg_dimensions(&jpeg_of(33, 17)), Some((33, 17)));
        let mut progressive = Vec::new();
        let img = scene(3, 40, 20);
        // SOF2: the same layout as SOF0, another marker.
        let mut baseline = encode_jpeg(&img);
        let sof = baseline.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
        baseline[sof + 1] = 0xC2;
        progressive.extend_from_slice(&baseline);
        assert_eq!(jpeg_dimensions(&progressive), Some((40, 20)));
        for broken in [
            &[][..],
            &[0xFF, 0xD8],
            &[0xFF, 0xD8, 0xFF, 0xC0, 0x00],
            &[0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02, 0xFF, 0xD9],
            &[0x00, 0xD8, 0xFF, 0xC0],
        ] {
            assert_eq!(jpeg_dimensions(broken), None, "{broken:?}");
        }
    }
}
