//! Locates the TIFF block(s) that carry a file's EXIF: the APP1 `Exif\0\0`
//! segment of a JPEG (also the JPEG preview embedded in a Fujifilm RAF), the
//! file itself for the TIFF family, the CMT boxes of a Canon CR3, or the
//! `Exif` metadata block of a HEIF file (read by libheif, feature `heic`).
//!
//! It also locates the JPEG previews of RAF and CR3 files for the RAW preview
//! fallback (`decode/preview.rs`), and reads whole files for both
//! ([`read_file`]). No file is memory-mapped: one truncated or removed while
//! mapped would kill the process (spec §4.3).

use std::fs::File;
use std::io::{self, BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::Path;

use super::ExifError;
use crate::budget::{WholeFileError, read_whole_file};
use crate::model::FileKind;

/// The TIFF-family extensions (lowercase, no dot): the file itself is a TIFF block.
const TIFF_FAMILY: &[&str] = &[
    "tif", "tiff", "cr2", "nef", "nrw", "arw", "srf", "sr2", "dng", "orf", "rw2", "pef", "srw",
    "3fr", "iiq", "erf", "kdc", "mos",
];

/// The start of an APP1 segment that carries EXIF.
const EXIF_PREFIX: &[u8] = b"Exif\0\0";

/// The magic at the start of every RAF file.
pub(crate) const RAF_MAGIC: &[u8] = b"FUJIFILMCCD-RAW ";
/// Where the RAF header keeps the embedded JPEG's offset and length (u32 BE each).
const RAF_JPEG_OFFSET_AT: usize = 84;
const RAF_JPEG_LENGTH_AT: usize = 88;

/// The extended type of the CR3 `moov/uuid` box that holds the CMT boxes:
/// 85c0b687-820f-11e0-8111-f4ce462b6a48.
const CANON_UUID: [u8; 16] = [
    0x85, 0xC0, 0xB6, 0x87, 0x82, 0x0F, 0x11, 0xE0, 0x81, 0x11, 0xF4, 0xCE, 0x46, 0x2B, 0x6A, 0x48,
];

/// The extended type of the top-level CR3 `uuid` box that holds the `PRVW`
/// preview: eaf42b5e-1c98-4b88-b9fb-b7dc406e4d16.
const PREVIEW_UUID: [u8; 16] = [
    0xEA, 0xF4, 0x2B, 0x5E, 0x1C, 0x98, 0x4B, 0x88, 0xB9, 0xFB, 0xB7, 0xDC, 0x40, 0x6E, 0x4D, 0x16,
];
/// The bytes between the preview `uuid` box's extended type and its `PRVW` child.
const PREVIEW_UUID_PREFIX_LEN: usize = 8;
/// The `PRVW` payload before its JPEG: u32 0, u16 1, u16 width, u16 height,
/// u16 1, then the u32 JPEG length (big-endian).
const PRVW_HEADER_LEN: usize = 16;
const PRVW_JPEG_LENGTH_AT: usize = 12;

const SOI: [u8; 2] = [0xFF, 0xD8];
const APP1: u8 = 0xE1;
const SOS: u8 = 0xDA;
const EOI: u8 = 0xD9;

/// Where the EXIF of a file lives, as bytes in memory.
pub(crate) enum Source {
    /// One TIFF block: IFD0, the ExifIFD and the MakerNote inside it. The
    /// copied APP1 payload of a JPEG (or of a RAF's embedded JPEG), or the
    /// whole of a TIFF-family file, whose metadata may sit anywhere, often at
    /// the end.
    Tiff(Vec<u8>),
    /// A whole CR3 file, whose EXIF is split over its CMT boxes (see [`cr3_cmt`]).
    Cr3(Vec<u8>),
}

/// The EXIF source of `path`; `None` when the file has no EXIF or its
/// container is not supported (PNG, some RAW formats, and HEIC without the
/// `heic` feature). A TIFF-family, RAF or CR3 file is read whole
/// ([`read_file`]); a JPEG only up to its EXIF segment.
pub(crate) fn exif_source(path: &Path, kind: FileKind) -> Result<Option<Source>, ExifError> {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase);
    match (kind, ext.as_deref()) {
        (FileKind::Jpeg, _) => Ok(jpeg_exif(BufReader::new(open(path)?))?.map(Source::Tiff)),
        (FileKind::Tiff | FileKind::Raw, Some(ext)) if TIFF_FAMILY.contains(&ext) => {
            Ok(Some(Source::Tiff(read_file(path)?)))
        }
        (FileKind::Raw, Some("raf")) => Ok(raf_exif(&read_file(path)?)?.map(Source::Tiff)),
        (FileKind::Raw, Some("cr3")) => Ok(Some(Source::Cr3(read_file(path)?))),
        #[cfg(feature = "heic")]
        (FileKind::Heic, _) => Ok(crate::decode::heic::heic_exif(path)?.map(Source::Tiff)),
        _ => Ok(None),
    }
}

/// The EXIF of a RAF file: the APP1 payload of its embedded JPEG preview (see
/// [`raf_jpeg`]). Errors from the JPEG itself report offsets relative to the
/// embedded JPEG.
pub(crate) fn raf_exif(file: &[u8]) -> Result<Option<Vec<u8>>, ExifError> {
    jpeg_exif(Cursor::new(raf_jpeg(file)?))
}

/// The JPEG preview embedded in a RAF file, located by the header's offset
/// (byte 84) and length (byte 88). The bytes are not checked to be a JPEG.
pub(crate) fn raf_jpeg(file: &[u8]) -> Result<&[u8], ExifError> {
    if !file.starts_with(RAF_MAGIC) {
        return Err(ExifError::Raf {
            what: "missing the FUJIFILMCCD-RAW magic",
            offset: 0,
        });
    }
    let (Some(offset), Some(len)) = (
        be_u32(file, RAF_JPEG_OFFSET_AT),
        be_u32(file, RAF_JPEG_LENGTH_AT),
    ) else {
        return Err(ExifError::Raf {
            what: "header ends before the embedded JPEG offset and length",
            offset: file.len() as u64,
        });
    };
    (offset as usize)
        .checked_add(len as usize)
        .and_then(|end| file.get(offset as usize..end))
        .ok_or(ExifError::Raf {
            what: "the embedded JPEG runs past the end of the file",
            offset: u64::from(offset),
        })
}

/// The CMT boxes of a CR3 file. Each payload is a whole TIFF block.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Cr3Cmt<'a> {
    /// IFD0.
    pub cmt1: Option<&'a [u8]>,
    /// The ExifIFD, as the IFD0 of its own block.
    pub cmt2: Option<&'a [u8]>,
    /// The Canon MakerNote IFD, as the IFD0 of its own block.
    pub cmt3: Option<&'a [u8]>,
}

/// Finds the CMT boxes of a CR3 file: top-level `moov`, then its `uuid` child
/// with [`CANON_UUID`], then that box's `CMT1`/`CMT2`/`CMT3` children (the
/// first of each). `None` when there is no such `moov`, `uuid` or CMT box.
///
/// The file must start with an `ftyp` box. Every box walked on the way is
/// bounds-checked; boxes after the ones needed are never read.
pub(crate) fn cr3_cmt(file: &[u8]) -> Result<Option<Cr3Cmt<'_>>, ExifError> {
    let Some(moov) = find(after_ftyp(file)?, |b| b.kind == *b"moov")? else {
        return Ok(None);
    };
    let is_canon = |b: &BmffBox| b.kind == *b"uuid" && b.uuid == Some(CANON_UUID);
    let Some(canon) = find(moov.children(), is_canon)? else {
        return Ok(None);
    };
    let mut cmt = Cr3Cmt::default();
    for child in canon.children() {
        let child = child?;
        let slot = match &child.kind {
            b"CMT1" => &mut cmt.cmt1,
            b"CMT2" => &mut cmt.cmt2,
            b"CMT3" => &mut cmt.cmt3,
            _ => continue,
        };
        slot.get_or_insert(child.payload);
        if cmt.cmt1.is_some() && cmt.cmt2.is_some() && cmt.cmt3.is_some() {
            break;
        }
    }
    Ok((cmt != Cr3Cmt::default()).then_some(cmt))
}

/// The JPEG of a CR3 file's `PRVW` box: the top-level `uuid` box with
/// [`PREVIEW_UUID`], then (after its 8-byte prefix) its first `PRVW` child,
/// whose payload is a 16-byte header and the JPEG of the length it declares.
/// `None` when there is no such `uuid` or `PRVW` box. The bytes are not
/// checked to be a JPEG.
///
/// Like [`cr3_cmt`], the file must start with an `ftyp` box, every box walked
/// on the way is bounds-checked, and no box after the preview is read.
pub(crate) fn cr3_prvw(file: &[u8]) -> Result<Option<&[u8]>, ExifError> {
    let is_preview = |b: &BmffBox| b.kind == *b"uuid" && b.uuid == Some(PREVIEW_UUID);
    let Some(preview) = find(after_ftyp(file)?, is_preview)? else {
        return Ok(None);
    };
    let err = |what: &str, offset: usize| ExifError::Bmff {
        what: what.into(),
        offset: offset as u64,
    };
    let children = preview
        .payload
        .get(PREVIEW_UUID_PREFIX_LEN..)
        .ok_or_else(|| {
            err(
                "preview uuid box ends inside its prefix",
                preview.payload_offset,
            )
        })?;
    let children = Boxes::new(
        children,
        preview.payload_offset + PREVIEW_UUID_PREFIX_LEN,
        Some(*b"uuid"),
    );
    let Some(prvw) = find(children, |b| b.kind == *b"PRVW")? else {
        return Ok(None);
    };
    let len = be_u32(prvw.payload, PRVW_JPEG_LENGTH_AT)
        .ok_or_else(|| err("PRVW box ends inside its header", prvw.payload_offset))?;
    // The length field ends the header, so the payload holds the whole header.
    prvw.payload[PRVW_HEADER_LEN..]
        .get(..len as usize)
        .map(Some)
        .ok_or_else(|| {
            err(
                "PRVW JPEG runs past the end of its box",
                prvw.payload_offset + PRVW_HEADER_LEN,
            )
        })
}

/// The top-level boxes of a BMFF file after its first, which must be `ftyp`.
fn after_ftyp(file: &[u8]) -> Result<Boxes<'_>, ExifError> {
    let mut top = Boxes::new(file, 0, None);
    if !top.next().transpose()?.is_some_and(|b| b.kind == *b"ftyp") {
        return Err(ExifError::Bmff {
            what: "the file does not start with an ftyp box".into(),
            offset: 0,
        });
    }
    Ok(top)
}

/// One ISO-BMFF box.
struct BmffBox<'a> {
    kind: [u8; 4],
    /// The extended type of a `uuid` box.
    uuid: Option<[u8; 16]>,
    /// The bytes after the header (size, type, largesize, extended type).
    payload: &'a [u8],
    /// The file offset of `payload`.
    payload_offset: usize,
}

impl<'a> BmffBox<'a> {
    fn children(&self) -> Boxes<'a> {
        Boxes::new(self.payload, self.payload_offset, Some(self.kind))
    }
}

/// The boxes that tile a container (the file or a box's payload), in order.
/// Yields at most one `Err`, then stops. Every box is at least 8 bytes long,
/// so a walk takes at most `len / 8` steps.
struct Boxes<'a> {
    data: &'a [u8],
    pos: usize,
    /// The file offset of `data`.
    base: usize,
    /// The type of the enclosing box; `None` for the file.
    parent: Option<[u8; 4]>,
    failed: bool,
}

impl<'a> Boxes<'a> {
    fn new(data: &'a [u8], base: usize, parent: Option<[u8; 4]>) -> Self {
        Self {
            data,
            pos: 0,
            base,
            parent,
            failed: false,
        }
    }

    /// Reads the box at `self.pos` (which is inside `data`) and moves past it.
    fn read(&mut self) -> Result<BmffBox<'a>, ExifError> {
        let rest = &self.data[self.pos..];
        let offset = self.base + self.pos;
        let parent = || match self.parent {
            Some(kind) => format!("its '{}' parent", kind.escape_ascii()),
            None => "the file".to_owned(),
        };
        let err = |what: String| ExifError::Bmff {
            what,
            offset: offset as u64,
        };
        let kind = rest.get(4..8).and_then(|k| <[u8; 4]>::try_from(k).ok());
        let (Some(size), Some(kind)) = (be_u32(rest, 0), kind) else {
            return Err(err(format!(
                "box header truncated by the end of {}",
                parent()
            )));
        };
        let name = kind.escape_ascii();
        let (size, mut header_len) = match size {
            // The box runs to the end of its container.
            0 => (rest.len() as u64, 8),
            1 => match be_u64(rest, 8) {
                Some(large) => (large, 16),
                None => {
                    return Err(err(format!(
                        "box '{name}' largesize truncated by the end of {}",
                        parent()
                    )));
                }
            },
            n => (u64::from(n), 8),
        };
        if kind == *b"uuid" {
            header_len += 16;
        }
        if size < header_len as u64 {
            return Err(err(format!(
                "box '{name}' is {size} bytes, shorter than its {header_len}-byte header"
            )));
        }
        if size > rest.len() as u64 {
            return Err(err(format!(
                "box '{name}' ({size} bytes) runs past the end of {} ({} bytes left)",
                parent(),
                rest.len()
            )));
        }
        // `header_len <= size <= rest.len()`, so these slices are in bounds.
        let size = size as usize;
        let uuid = (kind == *b"uuid").then(|| {
            let mut uuid = [0; 16];
            uuid.copy_from_slice(&rest[header_len - 16..header_len]);
            uuid
        });
        self.pos += size;
        Ok(BmffBox {
            kind,
            uuid,
            payload: &rest[header_len..size],
            payload_offset: offset + header_len,
        })
    }
}

impl<'a> Iterator for Boxes<'a> {
    type Item = Result<BmffBox<'a>, ExifError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.pos >= self.data.len() {
            return None;
        }
        let result = self.read();
        self.failed = result.is_err();
        Some(result)
    }
}

/// The first box of `boxes` that `want` accepts; an error in a box before it fails.
fn find<'a>(
    boxes: Boxes<'a>,
    want: impl Fn(&BmffBox<'a>) -> bool,
) -> Result<Option<BmffBox<'a>>, ExifError> {
    for b in boxes {
        let b = b?;
        if want(&b) {
            return Ok(Some(b));
        }
    }
    Ok(None)
}

/// The big-endian u32 at `at`, when `data` holds it.
fn be_u32(data: &[u8], at: usize) -> Option<u32> {
    let bytes: [u8; 4] = data.get(at..at.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

/// The big-endian u64 at `at`, when `data` holds it.
fn be_u64(data: &[u8], at: usize) -> Option<u64> {
    let bytes: [u8; 8] = data.get(at..at.checked_add(8)?)?.try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}

/// The whole file at `path`, read into memory for the readers that need all
/// of it: the EXIF of a TIFF-family file may sit anywhere, as may the JPEG
/// previews of the RAW fallback. See [`read_whole_file`]: a file longer than
/// [`DECODE_MEMORY_BUDGET`](crate::budget::DECODE_MEMORY_BUDGET) is refused
/// with [`ExifError::TooLarge`] before anything is allocated. A file that
/// changes while it is read gives bytes the bounds-checked readers turn into
/// an `Err`.
pub(crate) fn read_file(path: &Path) -> Result<Vec<u8>, ExifError> {
    read_whole_file(path).map_err(|err| match err {
        WholeFileError::TooLarge { len } => ExifError::TooLarge { len },
        WholeFileError::Io { what, source } => ExifError::Io { what, source },
    })
}

fn open(path: &Path) -> Result<File, ExifError> {
    File::open(path).map_err(io_error("opening the file"))
}

/// The payload of the first APP1 `Exif\0\0` segment of the JPEG stream `r`,
/// without that prefix; `None` when the header segments end without one.
pub(crate) fn jpeg_exif<R: Read + Seek>(mut r: R) -> Result<Option<Vec<u8>>, ExifError> {
    let mut soi = Vec::with_capacity(2);
    (&mut r)
        .take(2)
        .read_to_end(&mut soi)
        .map_err(io_error("reading the JPEG start"))?;
    if soi != SOI {
        return Err(ExifError::NotJpeg { found: soi });
    }
    let mut pos: u64 = 2;
    loop {
        let mut byte = read_byte(&mut r, &mut pos, "segment marker")?;
        if byte != 0xFF {
            return Err(ExifError::Jpeg {
                what: "expected a segment marker",
                offset: pos - 1,
            });
        }
        // Any number of 0xFF fill bytes may precede the marker code.
        while byte == 0xFF {
            byte = read_byte(&mut r, &mut pos, "segment marker")?;
        }
        match byte {
            // The header segments end at the scan data: EXIF must come before it.
            SOS | EOI => return Ok(None),
            // Standalone markers carry no length.
            0x01 | 0xD0..=0xD7 => continue,
            0x00 => {
                return Err(ExifError::Jpeg {
                    what: "stuffed 0xFF00 where a marker was expected",
                    offset: pos - 2,
                });
            }
            _ => {}
        }
        let mut len = [0; 2];
        read_exact(&mut r, &mut pos, &mut len, "segment length")?;
        let len = u16::from_be_bytes(len);
        if len < 2 {
            return Err(ExifError::Jpeg {
                what: "segment length below 2",
                offset: pos - 2,
            });
        }
        let payload_len = usize::from(len - 2);
        if byte == APP1 && payload_len >= EXIF_PREFIX.len() {
            let mut payload = vec![0; payload_len];
            read_exact(&mut r, &mut pos, &mut payload, "APP1 segment")?;
            if payload.starts_with(EXIF_PREFIX) {
                payload.drain(..EXIF_PREFIX.len());
                return Ok(Some(payload));
            }
        } else {
            // Seeking past the end is allowed; the next read reports the truncation.
            r.seek(SeekFrom::Current(i64::from(len - 2)))
                .map_err(io_error("skipping a JPEG segment"))?;
            pos += u64::from(len - 2);
        }
    }
}

fn read_byte<R: Read>(r: &mut R, pos: &mut u64, what: &'static str) -> Result<u8, ExifError> {
    let mut byte = [0];
    read_exact(r, pos, &mut byte, what)?;
    Ok(byte[0])
}

/// Fills `buf` from `r` at stream offset `*pos` and advances `*pos`.
fn read_exact<R: Read>(
    r: &mut R,
    pos: &mut u64,
    buf: &mut [u8],
    what: &'static str,
) -> Result<(), ExifError> {
    r.read_exact(buf).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => ExifError::JpegTruncated { what, offset: *pos },
        _ => ExifError::Io {
            what: "reading the JPEG",
            source: e,
        },
    })?;
    *pos += buf.len() as u64;
    Ok(())
}

fn io_error(what: &'static str) -> impl FnOnce(io::Error) -> ExifError {
    move |source| ExifError::Io { what, source }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn segment(marker: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![0xFF, marker];
        out.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn jpeg(segments: &[Vec<u8>]) -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8];
        for s in segments {
            out.extend_from_slice(s);
        }
        out.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02, 0x12, 0x34, 0xFF, 0xD9]);
        out
    }

    fn exif(data: Vec<u8>) -> Result<Option<Vec<u8>>, ExifError> {
        jpeg_exif(Cursor::new(data))
    }

    #[test]
    fn exif_read_survives_the_file_being_truncated() {
        // A card pulled during a scan: the file shrinks after it was read.
        // A mapped page past the new end would raise SIGBUS when read on
        // Unix; Windows refuses to truncate a mapped file at all.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.tif");
        let tiff = photostacker_testkit::TiffSpec {
            little_endian: true,
            ifd0: vec![
                (0x010F, photostacker_testkit::Value::Ascii("Canon".into())),
                (
                    0x0110,
                    photostacker_testkit::Value::Ascii("Canon EOS 6D".into()),
                ),
            ],
            exif: Vec::new(),
            maker_note: None,
        }
        .build();
        std::fs::write(&path, &tiff).unwrap();
        let data = read_file(&path).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(0)
            .unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        assert_eq!(data, tiff);
        let decoded = crate::exif::decode(&data).unwrap();
        assert_eq!(decoded.data.make.as_deref(), Some("Canon"));
        assert_eq!(decoded.data.model.as_deref(), Some("Canon EOS 6D"));
    }

    #[test]
    fn a_file_over_the_budget_is_too_large() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.tif");
        let len = crate::budget::DECODE_MEMORY_BUDGET + 1;
        // Sparse: no disk space and no memory is used.
        photostacker_testkit::sparse_file(&path, len).unwrap();
        let err = read_file(&path).unwrap_err();
        assert!(
            matches!(err, ExifError::TooLarge { len: l } if l == len),
            "{err}"
        );
        // Its EXIF counts as unreadable.
        let err = crate::exif::read_exif(&path, FileKind::Tiff).unwrap_err();
        assert!(matches!(err, ExifError::TooLarge { .. }), "{err}");
    }

    #[test]
    fn finds_exif_after_other_segments() {
        let data = jpeg(&[
            segment(0xE0, b"JFIF\0\x01\x02"),
            segment(0xE1, b"http://ns.adobe.com/xap/1.0/\0<x/>"),
            segment(0xE1, b"Exif\0\0TIFF"),
        ]);
        assert_eq!(exif(data).unwrap().as_deref(), Some(&b"TIFF"[..]));
    }

    #[test]
    fn fill_bytes_before_a_marker_are_skipped() {
        let mut data = vec![0xFF, 0xD8, 0xFF, 0xFF];
        data.extend(segment(0xE1, b"Exif\0\0ok"));
        assert_eq!(exif(data).unwrap().as_deref(), Some(&b"ok"[..]));
    }

    #[test]
    fn no_exif_before_scan_data_is_none() {
        let mut data = jpeg(&[segment(0xE0, b"JFIF\0")]);
        // An Exif-looking APP1 after SOS belongs to the entropy-coded data.
        data.extend(segment(0xE1, b"Exif\0\0TIFF"));
        assert_eq!(exif(data).unwrap(), None);
        assert_eq!(exif(vec![0xFF, 0xD8, 0xFF, 0xD9]).unwrap(), None);
    }

    #[test]
    fn not_a_jpeg_errors() {
        assert!(exif(b"II*\0\x08\0\0\0".to_vec()).is_err());
        assert!(exif(Vec::new()).is_err());
    }

    #[test]
    fn truncated_or_malformed_segments_error() {
        // The length claims more bytes than the stream has.
        let mut data = vec![0xFF, 0xD8, 0xFF, 0xE1, 0x10, 0x00];
        data.extend_from_slice(b"Exif\0\0");
        assert!(exif(data).is_err());
        // A segment length below 2.
        assert!(exif(vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x01]).is_err());
        // A skipped segment that runs past the end, then nothing.
        assert!(exif(vec![0xFF, 0xD8, 0xFF, 0xE0, 0x7F, 0xFF, 0x00]).is_err());
        // Not a marker where one must be.
        assert!(exif(vec![0xFF, 0xD8, 0x12, 0x34]).is_err());
        // The stream ends between segments.
        assert!(exif(vec![0xFF, 0xD8]).is_err());
    }

    // --- RAF ---

    fn tiny_jpeg() -> Vec<u8> {
        jpeg(&[segment(0xE0, b"JFIF\0"), segment(0xE1, b"Exif\0\0TIFF")])
    }

    #[test]
    fn raf_exif_reads_the_embedded_jpeg() {
        let raf = photostacker_testkit::raf_with_jpeg(&tiny_jpeg());
        assert_eq!(raf_exif(&raf).unwrap().as_deref(), Some(&b"TIFF"[..]));
        let no_exif = photostacker_testkit::raf_with_jpeg(&jpeg(&[segment(0xE0, b"JFIF\0")]));
        assert_eq!(raf_exif(&no_exif).unwrap(), None);
    }

    #[test]
    fn malformed_raf_errors() {
        let jpeg = tiny_jpeg();
        let good = photostacker_testkit::raf_with_jpeg(&jpeg);
        let with_field = |at: usize, value: u32| {
            let mut raf = good.clone();
            raf[at..at + 4].copy_from_slice(&value.to_be_bytes());
            raf
        };
        let mut bad_magic = good.clone();
        bad_magic[0] = b'X';
        let cases = [
            Vec::new(),
            bad_magic,
            good[..91].to_vec(),
            with_field(84, u32::MAX),
            with_field(84, good.len() as u32),
            with_field(88, u32::MAX),
            with_field(88, (good.len() - 148 + 1) as u32),
            // An empty JPEG has no SOI.
            with_field(88, 0),
            // The range is inside the file but is not the JPEG.
            with_field(84, 0),
        ];
        for (i, raf) in cases.iter().enumerate() {
            assert!(raf_exif(raf).is_err(), "case {i}");
        }
        assert!(matches!(
            raf_exif(&good[..91]),
            Err(ExifError::Raf { offset: 91, .. })
        ));
        assert!(matches!(
            raf_exif(&with_field(84, u32::MAX)),
            Err(ExifError::Raf {
                offset: 0xFFFF_FFFF,
                ..
            })
        ));
    }

    #[test]
    fn every_truncation_of_a_raf_errors_until_the_jpeg_is_whole() {
        let jpeg = tiny_jpeg();
        let raf = photostacker_testkit::raf_with_jpeg(&jpeg);
        let jpeg_end = 148 + jpeg.len();
        for n in 0..=raf.len() {
            let result = raf_exif(&raf[..n]);
            if n < jpeg_end {
                assert!(result.is_err(), "{n}");
            } else {
                assert_eq!(result.unwrap().as_deref(), Some(&b"TIFF"[..]), "{n}");
            }
        }
    }

    // --- CR3 (ISO-BMFF) ---

    /// A box with a 32-bit size.
    fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = ((8 + payload.len()) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }

    /// A box with size 1 and a 64-bit largesize.
    fn bx_large(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = 1u32.to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(&((16 + payload.len()) as u64).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    /// A box with size 0: it runs to the end of its parent.
    fn bx_to_end(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = 0u32.to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }

    fn uuid_box(uuid: &[u8; 16], children: &[u8]) -> Vec<u8> {
        let mut payload = uuid.to_vec();
        payload.extend_from_slice(children);
        bx(b"uuid", &payload)
    }

    fn ftyp() -> Vec<u8> {
        bx(b"ftyp", b"crx \0\0\0\x01crx isom")
    }

    fn file(boxes: &[Vec<u8>]) -> Vec<u8> {
        let mut out = ftyp();
        boxes.iter().for_each(|b| out.extend_from_slice(b));
        out
    }

    const OTHER_UUID: [u8; 16] = [0xAB; 16];

    #[test]
    fn canon_uuid_matches_the_testkit_fixture() {
        assert_eq!(CANON_UUID, photostacker_testkit::CANON_CR3_UUID);
    }

    #[test]
    fn cr3_cmt_finds_the_boxes_of_the_canon_uuid() {
        let cr3 = photostacker_testkit::cr3_with_cmt(b"one", b"two", b"three");
        let cmt = cr3_cmt(&cr3).unwrap().unwrap();
        assert_eq!(cmt.cmt1, Some(&b"one"[..]));
        assert_eq!(cmt.cmt2, Some(&b"two"[..]));
        assert_eq!(cmt.cmt3, Some(&b"three"[..]));

        // A decoy uuid comes first; the first box of each kind wins.
        let canon = [
            bx(b"CNCV", b"CanonCR3"),
            bx(b"CMT2", b"two"),
            bx(b"CMT1", b"one"),
            bx(b"CMT3", b"three"),
            bx(b"CMT1", b"second"),
        ]
        .concat();
        let moov = [
            uuid_box(&OTHER_UUID, &bx(b"CMT1", b"decoy")),
            bx(b"mvhd", &[0; 8]),
            uuid_box(&CANON_UUID, &canon),
        ]
        .concat();
        let cr3 = file(&[
            uuid_box(&OTHER_UUID, b"xmp"),
            bx(b"moov", &moov),
            bx(b"mdat", b"data"),
        ]);
        let cmt = cr3_cmt(&cr3).unwrap().unwrap();
        assert_eq!(
            cmt,
            Cr3Cmt {
                cmt1: Some(b"one"),
                cmt2: Some(b"two"),
                cmt3: Some(b"three"),
            }
        );
    }

    #[test]
    fn cr3_box_sizes_may_be_64_bit_or_to_the_end() {
        let all = Cr3Cmt {
            cmt1: Some(b"one"),
            cmt2: Some(b"two"),
            cmt3: Some(b"three"),
        };
        let canon = |last: Vec<u8>| {
            let mut children = [bx_large(b"CMT1", b"one"), bx(b"CMT2", b"two")].concat();
            children.extend(last);
            children
        };
        let canon_box = |children: &[u8]| {
            let mut payload = CANON_UUID.to_vec();
            payload.extend_from_slice(children);
            payload
        };
        let cases = [
            // 64-bit moov, size-0 CMT3 as the last child of the uuid.
            file(&[bx_large(
                b"moov",
                &uuid_box(&CANON_UUID, &canon(bx_to_end(b"CMT3", b"three"))),
            )]),
            // A 64-bit uuid; a size-0 moov as the last box of the file.
            file(&[bx_to_end(
                b"moov",
                &bx_large(b"uuid", &canon_box(&canon(bx(b"CMT3", b"three")))),
            )]),
            // A size-0 uuid as the last child of moov, then more top-level boxes.
            file(&[
                bx(
                    b"moov",
                    &[
                        bx(b"mvhd", &[0; 4]),
                        bx_to_end(b"uuid", &canon_box(&canon(bx(b"CMT3", b"three")))),
                    ]
                    .concat(),
                ),
                bx(b"mdat", b"data"),
            ]),
        ];
        for (i, cr3) in cases.iter().enumerate() {
            assert_eq!(cr3_cmt(cr3).unwrap(), Some(all), "case {i}");
        }
    }

    #[test]
    fn cr3_without_canon_metadata_is_none() {
        let cases = [
            ftyp(),
            file(&[bx(b"mdat", b"data")]),
            file(&[bx(b"moov", &bx(b"mvhd", &[0; 8]))]),
            file(&[bx(b"moov", &uuid_box(&OTHER_UUID, &bx(b"CMT1", b"x")))]),
            file(&[bx(b"moov", &uuid_box(&CANON_UUID, &bx(b"CNCV", b"x")))]),
            // CMT boxes outside moov do not count.
            file(&[uuid_box(&CANON_UUID, &bx(b"CMT1", b"x"))]),
        ];
        for (i, cr3) in cases.iter().enumerate() {
            assert_eq!(cr3_cmt(cr3).unwrap(), None, "case {i}");
        }
    }

    #[test]
    fn preview_uuid_matches_the_testkit_fixture() {
        assert_eq!(PREVIEW_UUID, photostacker_testkit::CR3_PREVIEW_UUID);
    }

    /// A `PRVW` payload: the 16-byte header declaring `declared` JPEG bytes, then `rest`.
    fn prvw(declared: u32, rest: &[u8]) -> Vec<u8> {
        let mut payload = [0, 0, 0, 0, 0, 1, 0, 8, 0, 6, 0, 1].to_vec();
        payload.extend_from_slice(&declared.to_be_bytes());
        payload.extend_from_slice(rest);
        bx(b"PRVW", &payload)
    }

    fn preview_box(children: &[u8]) -> Vec<u8> {
        let mut payload = vec![0, 0, 0, 0, 0, 0, 0, 1];
        payload.extend_from_slice(children);
        uuid_box(&PREVIEW_UUID, &payload)
    }

    #[test]
    fn cr3_prvw_takes_the_declared_length() {
        let cr3 = photostacker_testkit::cr3_with_prvw(b"\xFF\xD8jpeg\xFF\xD9");
        assert_eq!(cr3_prvw(&cr3).unwrap(), Some(&b"\xFF\xD8jpeg\xFF\xD9"[..]));
        // Other children may come first; padding after the JPEG is left out.
        let children = [bx(b"THMB", b"x"), prvw(3, b"abcdef")].concat();
        let cr3 = file(&[bx(b"moov", b""), preview_box(&children)]);
        assert_eq!(cr3_prvw(&cr3).unwrap(), Some(&b"abc"[..]));
    }

    #[test]
    fn cr3_without_a_preview_is_none() {
        let cases = [
            ftyp(),
            photostacker_testkit::cr3_with_cmt(b"one", b"two", b"three"),
            file(&[preview_box(&bx(b"THMB", b"x"))]),
            // A PRVW box in another uuid does not count.
            file(&[uuid_box(
                &OTHER_UUID,
                &[&[0; 8][..], &prvw(0, b"")].concat(),
            )]),
        ];
        for (i, cr3) in cases.iter().enumerate() {
            assert_eq!(cr3_prvw(cr3).unwrap(), None, "case {i}");
        }
    }

    #[test]
    fn malformed_prvw_errors_not_panic() {
        let cases = [
            ("no ftyp first", preview_box(&prvw(0, b""))),
            ("prefix cut", file(&[uuid_box(&PREVIEW_UUID, &[0; 4])])),
            ("header cut", file(&[preview_box(&bx(b"PRVW", &[0; 15]))])),
            (
                "JPEG past the box",
                file(&[preview_box(&prvw(7, b"abcdef"))]),
            ),
            (
                "JPEG length u32::MAX",
                file(&[preview_box(&prvw(u32::MAX, b"abc"))]),
            ),
        ];
        for (name, cr3) in &cases {
            let err = cr3_prvw(cr3).unwrap_err();
            assert!(matches!(err, ExifError::Bmff { .. }), "{name}: {err}");
        }
    }

    #[test]
    fn malformed_boxes_error_not_panic() {
        let raw = |size: u32, kind: &[u8; 4], rest: &[u8]| {
            let mut out = size.to_be_bytes().to_vec();
            out.extend_from_slice(kind);
            out.extend_from_slice(rest);
            out
        };
        let canon_with = |children: &[u8]| bx(b"moov", &uuid_box(&CANON_UUID, children));
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty file", Vec::new()),
            ("no ftyp first", bx(b"moov", &bx(b"mvhd", &[0; 8]))),
            ("truncated top-level header", file(&[vec![0, 0, 0, 8]])),
            ("size below the header", file(&[raw(4, b"moov", &[0; 8])])),
            ("size 7", file(&[raw(7, b"moov", &[0; 8])])),
            ("truncated largesize", file(&[raw(1, b"moov", &[0; 4])])),
            (
                "largesize below the header",
                file(&[raw(1, b"moov", &8u64.to_be_bytes())]),
            ),
            (
                "largesize u64::MAX",
                file(&[raw(1, b"moov", &u64::MAX.to_be_bytes())]),
            ),
            ("size u32::MAX", file(&[raw(u32::MAX, b"moov", &[0; 8])])),
            ("moov past the file", file(&[raw(64, b"moov", &[0; 8])])),
            (
                "uuid shorter than its UUID",
                file(&[bx(b"moov", &raw(20, b"uuid", &[0; 12]))]),
            ),
            (
                "size-0 uuid shorter than its UUID",
                file(&[bx(b"moov", &raw(0, b"uuid", &[0; 12]))]),
            ),
            ("truncated child header", file(&[bx(b"moov", &[0, 0, 0])])),
            (
                // CMT1 runs past the uuid box, though not past the file.
                "child past its parent",
                file(&[
                    canon_with(&raw(1000, b"CMT1", b"one")),
                    bx(b"mdat", &[0; 2000]),
                ]),
            ),
            (
                "largesize child past its parent",
                file(&[
                    canon_with(&raw(1, b"CMT1", &1000u64.to_be_bytes())),
                    bx(b"mdat", &[0; 2000]),
                ]),
            ),
            (
                "bad box while CMT boxes are still missing",
                file(&[canon_with(
                    &[bx(b"CMT1", b"one"), raw(3, b"THMB", b"")].concat(),
                )]),
            ),
        ];
        for (name, cr3) in &cases {
            let err = cr3_cmt(cr3).unwrap_err();
            assert!(matches!(err, ExifError::Bmff { .. }), "{name}: {err}");
        }
        // Offsets are absolute in the file: the bad CMT1 box sits after ftyp
        // (24), the moov header (8) and the uuid header with its UUID (24).
        let err = cr3_cmt(&cases[13].1).unwrap_err();
        assert!(matches!(err, ExifError::Bmff { offset: 56, .. }), "{err}");
    }
}
