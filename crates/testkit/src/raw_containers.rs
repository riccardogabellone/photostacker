//! Builds synthetic RAW containers around EXIF payloads and embedded previews:
//! a Fujifilm RAF around an embedded JPEG, a Canon CR3 (ISO-BMFF) around its
//! CMT TIFF blocks or its `PRVW` preview, and a TIFF-family RAW of an unknown
//! body that carries several JPEG previews.

use std::io::Cursor;

use image::RgbImage;

use crate::scene::scene;
use crate::tiff_builder::{Value, ifd_bytes};

/// The magic at the start of every RAF file.
pub const RAF_MAGIC: &[u8; 16] = b"FUJIFILMCCD-RAW ";

/// The UUID of the `moov/uuid` box that holds a CR3's Canon metadata boxes.
pub const CANON_CR3_UUID: [u8; 16] = [
    0x85, 0xC0, 0xB6, 0x87, 0x82, 0x0F, 0x11, 0xE0, 0x81, 0x11, 0xF4, 0xCE, 0x46, 0x2B, 0x6A, 0x48,
];

/// The UUID of the top-level CR3 box that holds the `PRVW` preview box:
/// eaf42b5e-1c98-4b88-b9fb-b7dc406e4d16.
pub const CR3_PREVIEW_UUID: [u8; 16] = [
    0xEA, 0xF4, 0x2B, 0x5E, 0x1C, 0x98, 0x4B, 0x88, 0xB9, 0xFB, 0xB7, 0xDC, 0x40, 0x6E, 0x4D, 0x16,
];

/// The Make of [`tiff_raw_with_jpegs`]: no RAW decoder knows this body.
pub const UNKNOWN_MAKE: &str = "Photostacker";
/// The Model of [`tiff_raw_with_jpegs`].
pub const UNKNOWN_MODEL: &str = "Unknown Body";

/// The UUID of the top-level XMP box of a CR3.
const XMP_UUID: [u8; 16] = [
    0xBE, 0x7A, 0xCF, 0xCB, 0x97, 0xA9, 0x42, 0xE8, 0x9C, 0x71, 0x99, 0x94, 0x91, 0xE3, 0xAF, 0xAC,
];

/// Length of the RAF header the embedded JPEG follows.
const RAF_HEADER_LEN: usize = 148;

/// A RAF file: the 148-byte header, `jpeg` right after it, then a 16-byte CFA
/// header and 32 bytes of CFA data. All header fields are big-endian:
/// - 0: [`RAF_MAGIC`]; 16: format version `0201`; 20: camera ID `FF129502`;
/// - 28: camera name `X-T5`, NUL-padded to 32 bytes; 60: directory version `0100`;
/// - 84: JPEG offset (148); 88: JPEG length;
/// - 92/96: CFA header offset/length; 100/104: CFA offset/length.
pub fn raf_with_jpeg(jpeg: &[u8]) -> Vec<u8> {
    let jpeg_len = be32(jpeg.len());
    let cfa_header_at = be32(RAF_HEADER_LEN + jpeg.len());
    let mut out = Vec::with_capacity(RAF_HEADER_LEN + jpeg.len() + 48);
    out.extend_from_slice(RAF_MAGIC);
    out.extend_from_slice(b"0201");
    out.extend_from_slice(b"FF129502");
    let mut name = [0; 32];
    name[..4].copy_from_slice(b"X-T5");
    out.extend_from_slice(&name);
    out.extend_from_slice(b"0100");
    out.resize(84, 0);
    for field in [
        be32(RAF_HEADER_LEN),
        jpeg_len,
        cfa_header_at,
        16,
        cfa_header_at + 16,
        32,
    ] {
        out.extend_from_slice(&field.to_be_bytes());
    }
    out.resize(RAF_HEADER_LEN, 0);
    out.extend_from_slice(jpeg);
    out.resize(out.len() + 48, 0);
    out
}

/// A CR3 file with plain 32-bit box sizes:
/// - `ftyp` (24 bytes): brand `crx `, version 1, compatible `crx ` `isom`;
/// - `moov`, right after `ftyp`:
///   - `uuid` [`CANON_CR3_UUID`] holding `CNCV`, `CMT1`, `CMT2`, `CMT3`, `CMT4`
///     (an empty little-endian TIFF), in that order;
///   - `mvhd` (version 0, all zero);
/// - `uuid` (XMP) with a small packet;
/// - `mdat` with 64 zero bytes.
///
/// Each CMT box's payload is the given TIFF block, unchanged.
pub fn cr3_with_cmt(cmt1: &[u8], cmt2: &[u8], cmt3: &[u8]) -> Vec<u8> {
    cr3(cmt1, cmt2, cmt3, None)
}

/// A CR3 file laid out like [`cr3_with_cmt`], with an empty little-endian
/// TIFF in every CMT box and, between the XMP `uuid` and `mdat`, the preview
/// box as Canon bodies write it:
/// - `uuid` [`CR3_PREVIEW_UUID`], then the 8 bytes `00 00 00 00 00 00 00 01`;
/// - `PRVW`: u32 0, u16 1, u16 width, u16 height, u16 1, u32 JPEG length,
///   `jpeg`, then 4 zero bytes of padding. All fields are big-endian.
///
/// The width and height are those of `jpeg`'s header, or 0 when it has none.
pub fn cr3_with_prvw(jpeg: &[u8]) -> Vec<u8> {
    let (width, height) = image::ImageReader::new(Cursor::new(jpeg))
        .with_guessed_format()
        .ok()
        .and_then(|reader| reader.into_dimensions().ok())
        .unwrap_or((0, 0));
    let mut prvw = 0u32.to_be_bytes().to_vec();
    prvw.extend_from_slice(&1u16.to_be_bytes());
    for side in [width, height] {
        let side = u16::try_from(side).expect("a CR3 preview side fits in u16");
        prvw.extend_from_slice(&side.to_be_bytes());
    }
    prvw.extend_from_slice(&1u16.to_be_bytes());
    prvw.extend_from_slice(&be32(jpeg.len()).to_be_bytes());
    prvw.extend_from_slice(jpeg);
    prvw.extend_from_slice(&[0; 4]);

    let mut preview = CR3_PREVIEW_UUID.to_vec();
    preview.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1]);
    preview.extend(bmff_box(b"PRVW", &prvw));
    cr3(EMPTY_TIFF, EMPTY_TIFF, EMPTY_TIFF, Some(&preview))
}

/// A little-endian TIFF block with an empty IFD0.
const EMPTY_TIFF: &[u8] = b"II*\0\x08\0\0\0\0\0\0\0\0\0";

/// The CR3 of [`cr3_with_cmt`]. `preview` is the payload (extended type
/// included) of a `uuid` box put between the XMP box and `mdat`.
fn cr3(cmt1: &[u8], cmt2: &[u8], cmt3: &[u8], preview: Option<&[u8]>) -> Vec<u8> {
    let mut ftyp = Vec::new();
    ftyp.extend_from_slice(b"crx ");
    ftyp.extend_from_slice(&1u32.to_be_bytes());
    ftyp.extend_from_slice(b"crx isom");

    let mut canon = CANON_CR3_UUID.to_vec();
    canon.extend(bmff_box(b"CNCV", b"CanonCR3_001/00.10.00/00.00.00"));
    canon.extend(bmff_box(b"CMT1", cmt1));
    canon.extend(bmff_box(b"CMT2", cmt2));
    canon.extend(bmff_box(b"CMT3", cmt3));
    canon.extend(bmff_box(b"CMT4", EMPTY_TIFF));

    let mut moov = bmff_box(b"uuid", &canon);
    moov.extend(bmff_box(b"mvhd", &[0; 100]));

    let mut xmp = XMP_UUID.to_vec();
    xmp.extend_from_slice(b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/>");

    let mut out = bmff_box(b"ftyp", &ftyp);
    out.extend(bmff_box(b"moov", &moov));
    out.extend(bmff_box(b"uuid", &xmp));
    if let Some(preview) = preview {
        out.extend(bmff_box(b"uuid", preview));
    }
    out.extend(bmff_box(b"mdat", &[0; 64]));
    out
}

/// The side of the 16-bit raw image in IFD1 of [`tiff_raw_with_jpegs`].
const RAW_SIDE: u32 = 8;

/// A little-endian TIFF-family RAW of an unknown body ([`UNKNOWN_MAKE`],
/// [`UNKNOWN_MODEL`]) with one JPEG per `(width, height)` of `previews`, each
/// a [`scene`] (seeds 1, 2, ...) encoded at quality 90:
/// - IFD0 (NewSubfileType 1) holds `previews[0]` as its
///   `JPEGInterchangeFormat`/`Length` (0x0201/0x0202), the way NEF and ARW
///   carry a thumbnail, and lists one SubIFD (0x014A) per further preview.
/// - SubIFD `k` (NewSubfileType 1, YCbCr) holds `previews[k]` as its only
///   strip, with Compression 7 (as in DNG) when `k` is odd and 6 (old-style
///   JPEG, as in CR2) when it is even.
/// - IFD1, IFD0's next IFD, holds an uncompressed 8×8 16-bit CFA strip of
///   zeros; its next IFD is 0.
///
/// The IFDs come first, in that order (IFD0, IFD1, the SubIFDs), then the raw
/// strip, then the JPEGs in `previews` order. The last JPEG ends the file, so
/// any cut leaves an offset pointing past the end.
pub fn tiff_raw_with_jpegs(previews: &[(u32, u32)]) -> Vec<u8> {
    assert!(!previews.is_empty(), "the IFD0 thumbnail is required");
    let jpegs: Vec<Vec<u8>> = previews
        .iter()
        .zip(1..)
        .map(|(&(w, h), seed)| encode_jpeg(&scene(seed, w, h)))
        .collect();
    let raw_strip = vec![0; (RAW_SIDE * RAW_SIDE * 2) as usize];

    // Values have fixed sizes, so a pass with zero offsets lays the file out.
    let ifd_len = |entries: &[(u16, Value)]| ifd_bytes(entries, true, 0).0.len();
    let ifd0_at = 8;
    let placeholder_subifds = vec![0; previews.len() - 1];
    let ifd1_at = ifd0_at + ifd_len(&thumbnail_ifd(&placeholder_subifds, 0, &jpegs[0]));
    let mut at = ifd1_at + ifd_len(&raw_ifd(0, raw_strip.len()));
    let mut sub_at = Vec::new();
    for k in 1..previews.len() {
        sub_at.push(be32(at));
        at += ifd_len(&preview_ifd(k, previews[k], 0, &jpegs[k]));
    }
    let raw_at = at;
    at += raw_strip.len();
    let mut jpeg_at = Vec::new();
    for jpeg in &jpegs {
        jpeg_at.push(be32(at));
        at += jpeg.len();
    }

    let mut out = b"II*\0".to_vec();
    out.extend_from_slice(&be32(ifd0_at).to_le_bytes());
    let thumbnail = thumbnail_ifd(&sub_at, jpeg_at[0], &jpegs[0]);
    out.extend(ifd_with_next(&thumbnail, ifd0_at, ifd1_at));
    out.extend(ifd_with_next(
        &raw_ifd(be32(raw_at), raw_strip.len()),
        ifd1_at,
        0,
    ));
    for k in 1..previews.len() {
        let ifd = preview_ifd(k, previews[k], jpeg_at[k], &jpegs[k]);
        out.extend(ifd_with_next(&ifd, sub_at[k - 1] as usize, 0));
    }
    assert_eq!(out.len(), raw_at, "the IFDs match the layout pass");
    out.extend(raw_strip);
    out.extend(jpegs.concat());
    assert_eq!(out.len(), at, "the data matches the layout pass");
    out
}

/// IFD0 of [`tiff_raw_with_jpegs`]: the thumbnail `jpeg` at `jpeg_at`.
fn thumbnail_ifd(subifds: &[u32], jpeg_at: u32, jpeg: &[u8]) -> Vec<(u16, Value)> {
    let mut entries = vec![
        (0x00FE, Value::Long(vec![1])),
        (0x010F, Value::Ascii(UNKNOWN_MAKE.into())),
        (0x0110, Value::Ascii(UNKNOWN_MODEL.into())),
        (0x0201, Value::Long(vec![jpeg_at])),
        (0x0202, Value::Long(vec![be32(jpeg.len())])),
    ];
    if !subifds.is_empty() {
        entries.push((0x014A, Value::Long(subifds.to_vec())));
    }
    entries
}

/// IFD1 of [`tiff_raw_with_jpegs`]: the uncompressed raw strip at `strip_at`.
fn raw_ifd(strip_at: u32, strip_len: usize) -> Vec<(u16, Value)> {
    vec![
        (0x00FE, Value::Long(vec![0])),
        (0x0100, Value::Long(vec![RAW_SIDE])),
        (0x0101, Value::Long(vec![RAW_SIDE])),
        (0x0102, Value::Short(vec![16])),
        (0x0103, Value::Short(vec![1])),
        (0x0106, Value::Short(vec![32803])),
        (0x0111, Value::Long(vec![strip_at])),
        (0x0115, Value::Short(vec![1])),
        (0x0116, Value::Long(vec![RAW_SIDE])),
        (0x0117, Value::Long(vec![be32(strip_len)])),
    ]
}

/// SubIFD `k` of [`tiff_raw_with_jpegs`]: `jpeg` as its one strip, at `strip_at`.
fn preview_ifd(k: usize, (w, h): (u32, u32), strip_at: u32, jpeg: &[u8]) -> Vec<(u16, Value)> {
    let compression = if k % 2 == 1 { 7 } else { 6 };
    vec![
        (0x00FE, Value::Long(vec![1])),
        (0x0100, Value::Long(vec![w])),
        (0x0101, Value::Long(vec![h])),
        (0x0102, Value::Short(vec![8, 8, 8])),
        (0x0103, Value::Short(vec![compression])),
        (0x0106, Value::Short(vec![6])),
        (0x0111, Value::Long(vec![strip_at])),
        (0x0115, Value::Short(vec![3])),
        (0x0116, Value::Long(vec![h])),
        (0x0117, Value::Long(vec![be32(jpeg.len())])),
    ]
}

/// A little-endian IFD at `start` whose next-IFD pointer is `next`.
fn ifd_with_next(entries: &[(u16, Value)], start: usize, next: usize) -> Vec<u8> {
    let (mut bytes, _) = ifd_bytes(entries, true, be32(start));
    let at = 2 + 12 * entries.len();
    bytes[at..at + 4].copy_from_slice(&be32(next).to_le_bytes());
    bytes
}

/// `img` as a plain JPEG (no APP1 or APP2 segment) at quality 90.
pub fn encode_jpeg(img: &RgbImage) -> Vec<u8> {
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
        .encode_image(img)
        .expect("JPEG encoding of an RgbImage");
    out
}

/// One ISO-BMFF box with a 32-bit size.
fn bmff_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&be32(8 + payload.len()).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    out
}

fn be32(n: usize) -> u32 {
    u32::try_from(n).expect("container offsets fit in u32")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_at(b: &[u8], at: usize) -> usize {
        u32::from_be_bytes(b[at..at + 4].try_into().unwrap()) as usize
    }

    #[test]
    fn raf_header_points_at_the_embedded_jpeg() {
        let jpeg = b"\xFF\xD8jpeg\xFF\xD9";
        let raf = raf_with_jpeg(jpeg);
        assert_eq!(&raf[..16], b"FUJIFILMCCD-RAW ");
        assert_eq!(&raf[16..20], b"0201");
        assert_eq!(&raf[20..28], b"FF129502");
        assert_eq!(&raf[28..36], b"X-T5\0\0\0\0");
        assert_eq!(&raf[60..64], b"0100");
        assert_eq!(u32_at(&raf, 84), 148);
        assert_eq!(u32_at(&raf, 88), jpeg.len());
        assert_eq!(&raf[148..148 + jpeg.len()], jpeg);
        let cfa_header = 148 + jpeg.len();
        assert_eq!(u32_at(&raf, 92), cfa_header);
        assert_eq!(u32_at(&raf, 96), 16);
        assert_eq!(u32_at(&raf, 100), cfa_header + 16);
        assert_eq!(u32_at(&raf, 104), 32);
        assert_eq!(raf.len(), cfa_header + 48);
    }

    /// The (type, start, end) of each box in `data`, which must tile it exactly.
    fn boxes(data: &[u8]) -> Vec<([u8; 4], usize, usize)> {
        let mut out = Vec::new();
        let mut at = 0;
        while at < data.len() {
            let size = u32_at(data, at);
            out.push((data[at + 4..at + 8].try_into().unwrap(), at, at + size));
            at += size;
        }
        assert_eq!(at, data.len());
        out
    }

    #[test]
    fn cr3_layout_nests_the_cmt_blocks_in_the_canon_uuid() {
        let (cmt1, cmt2, cmt3) = (&b"II*\0one"[..], &b"MM\0*two!"[..], &b"II*\0three"[..]);
        let cr3 = cr3_with_cmt(cmt1, cmt2, cmt3);

        let top = boxes(&cr3);
        let kinds: Vec<_> = top.iter().map(|b| b.0).collect();
        assert_eq!(kinds, [*b"ftyp", *b"moov", *b"uuid", *b"mdat"]);
        assert_eq!((top[0].1, top[0].2), (0, 24));
        assert_eq!(&cr3[8..24], b"crx \0\0\0\x01crx isom");
        assert_eq!(&cr3[top[2].1 + 8..top[2].1 + 24], &XMP_UUID);

        let (_, moov_start, moov_end) = top[1];
        let moov = boxes(&cr3[moov_start + 8..moov_end]);
        let kinds: Vec<_> = moov.iter().map(|b| b.0).collect();
        assert_eq!(kinds, [*b"uuid", *b"mvhd"]);

        let uuid = &cr3[moov_start + 8 + moov[0].1..moov_start + 8 + moov[0].2];
        assert_eq!(&uuid[8..24], &CANON_CR3_UUID);
        let children = boxes(&uuid[24..]);
        let kinds: Vec<_> = children.iter().map(|b| b.0).collect();
        assert_eq!(kinds, [*b"CNCV", *b"CMT1", *b"CMT2", *b"CMT3", *b"CMT4"]);
        let payload = |i: usize| &uuid[24 + children[i].1 + 8..24 + children[i].2];
        assert_eq!(payload(1), cmt1);
        assert_eq!(payload(2), cmt2);
        assert_eq!(payload(3), cmt3);
        assert!(payload(4).starts_with(b"II*\0\x08\0\0\0"));
    }

    #[test]
    fn cr3_prvw_box_holds_the_jpeg_after_its_header() {
        let jpeg = encode_jpeg(&scene(1, 40, 30));
        let cr3 = cr3_with_prvw(&jpeg);
        let top = boxes(&cr3);
        let kinds: Vec<_> = top.iter().map(|b| b.0).collect();
        assert_eq!(kinds, [*b"ftyp", *b"moov", *b"uuid", *b"uuid", *b"mdat"]);
        let (_, start, end) = top[3];
        assert_eq!(&cr3[start + 8..start + 24], &CR3_PREVIEW_UUID);
        assert_eq!(&cr3[start + 24..start + 32], &[0, 0, 0, 0, 0, 0, 0, 1]);
        let prvw = boxes(&cr3[start + 32..end]);
        assert_eq!(prvw.len(), 1);
        assert_eq!(&prvw[0].0, b"PRVW");
        let payload = &cr3[start + 32 + 8..end];
        let u16_at = |at: usize| u16::from_be_bytes([payload[at], payload[at + 1]]);
        assert_eq!((u16_at(6), u16_at(8)), (40, 30));
        assert_eq!(u32_at(payload, 12), jpeg.len());
        assert_eq!(&payload[16..16 + jpeg.len()], &jpeg[..]);
        assert_eq!(payload.len(), 16 + jpeg.len() + 4);
    }

    #[test]
    fn tiff_raw_places_the_jpegs_last_and_points_at_them() {
        let raw = tiff_raw_with_jpegs(&[(16, 8), (64, 48), (32, 24)]);
        let le32 = |at: usize| u32::from_le_bytes(raw[at..at + 4].try_into().unwrap()) as usize;
        let le16 = |at: usize| u16::from_le_bytes([raw[at], raw[at + 1]]);
        assert_eq!(&raw[..4], b"II*\0");
        let ifd0 = le32(4);
        // The value of tag `tag` in the IFD at `ifd`, when it is one inline LONG or SHORT.
        let value = |ifd: usize, tag: u16| {
            (0..usize::from(le16(ifd))).find_map(|i| {
                let entry = ifd + 2 + 12 * i;
                (le16(entry) == tag).then(|| match le16(entry + 2) {
                    3 => usize::from(le16(entry + 8)),
                    _ => le32(entry + 8),
                })
            })
        };
        let thumb_at = value(ifd0, 0x0201).unwrap();
        let thumb_len = value(ifd0, 0x0202).unwrap();
        assert_eq!(&raw[thumb_at..thumb_at + 2], &[0xFF, 0xD8]);
        // Two SubIFDs: an out-of-line LONG pair.
        let entry_count = usize::from(le16(ifd0));
        let next = le32(ifd0 + 2 + 12 * entry_count);
        assert_eq!(value(next, 0x0103), Some(1), "IFD1 is the raw strip");
        let subifds = (0..entry_count)
            .map(|i| ifd0 + 2 + 12 * i)
            .find(|&e| le16(e) == 0x014A)
            .unwrap();
        assert_eq!(le32(subifds + 4), 2);
        let list = le32(subifds + 8);
        let mut last_end = 0;
        for (k, compression) in [(0, 7), (1, 6)] {
            let sub = le32(list + 4 * k);
            assert_eq!(value(sub, 0x0103), Some(compression));
            let strip = value(sub, 0x0111).unwrap();
            assert_eq!(&raw[strip..strip + 2], &[0xFF, 0xD8]);
            last_end = strip + value(sub, 0x0117).unwrap();
        }
        assert!(thumb_at + thumb_len <= last_end);
        assert_eq!(last_end, raw.len());
    }
}
