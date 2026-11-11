//! Content sniffing for still images: the file's signature, not its
//! extension, picks the codec of a JPEG, PNG, TIFF or HEIC (spec §5.1).

use std::fs::File;
use std::io::{self, Read};
use std::iter;
use std::path::Path;

use crate::model::FileKind;

/// How many leading bytes are read: enough for an `ftyp` box with four
/// compatible brands.
const SNIFF_LEN: u64 = 32;

const JPEG_SIGNATURE: &[u8] = b"\xFF\xD8\xFF";
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
/// Classic TIFF (`*`) and BigTIFF (`+`), little- and big-endian.
const TIFF_SIGNATURES: [&[u8]; 4] = [b"II*\x00", b"MM\x00*", b"II+\x00", b"MM\x00+"];
/// The `ftyp` brands of a HEIF image or image sequence.
const HEIF_BRANDS: [&[u8; 4]; 10] = [
    b"heic", b"heix", b"hevc", b"hevx", b"heim", b"heis", b"hevm", b"hevs", b"mif1", b"msf1",
];

/// The kind to decode `path` as. For a still kind (`Jpeg`, `Png`, `Tiff`,
/// `Heic`) a known signature in the first 32 bytes wins over the extension's
/// `kind`; an unknown signature keeps `kind`. `Raw` is never overridden, and
/// its file is not read: the TIFF-based RAWs start with the TIFF signature.
pub fn effective_kind(path: &Path, kind: FileKind) -> io::Result<FileKind> {
    if kind == FileKind::Raw {
        return Ok(kind);
    }
    let mut head = Vec::new();
    File::open(path)?.take(SNIFF_LEN).read_to_end(&mut head)?;
    Ok(still_kind_of(&head).unwrap_or(kind))
}

/// The still kind whose signature `head` starts with, if any.
fn still_kind_of(head: &[u8]) -> Option<FileKind> {
    if head.starts_with(JPEG_SIGNATURE) {
        Some(FileKind::Jpeg)
    } else if head.starts_with(PNG_SIGNATURE) {
        Some(FileKind::Png)
    } else if TIFF_SIGNATURES.iter().any(|sig| head.starts_with(sig)) {
        Some(FileKind::Tiff)
    } else if is_heif(head) {
        Some(FileKind::Heic)
    } else {
        None
    }
}

/// Whether `head` starts with an `ftyp` box whose major brand, or one of its
/// compatible brands within `head`, is a HEIF brand.
///
/// The box is four 4-byte words (a u32 BE size, `ftyp`, the major brand, the
/// minor version), then compatible brands up to the end of the box; a size
/// of 0 means the box runs to the end of the file.
///
/// A size of 1 announces a 64-bit largesize after the type, which is not
/// parsed: the box then counts as shorter than its header, so the largesize's
/// high word stands where the major brand would be and no compatible brand is
/// read. Such a file keeps its extension's kind. An `ftyp` box is a few dozen
/// bytes, and no HEIF writer gives it a largesize.
fn is_heif(head: &[u8]) -> bool {
    let (words, _) = head.as_chunks::<4>();
    let [size, box_type, major, _minor, brands @ ..] = words else {
        return false;
    };
    if box_type != b"ftyp" {
        return false;
    }
    let box_len = match u32::from_be_bytes(*size) {
        0 => head.len(),
        len => usize::try_from(len).unwrap_or(usize::MAX),
    };
    let compatible = brands.iter().take(box_len.saturating_sub(16) / 4);
    iter::once(major)
        .chain(compatible)
        .any(|brand| HEIF_BRANDS.contains(&brand))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    fn sniffed(name: &str, kind: FileKind, bytes: &[u8]) -> FileKind {
        let dir = tempfile::tempdir().unwrap();
        effective_kind(&write(dir.path(), name, bytes), kind).unwrap()
    }

    const JPEG_HEAD: &[u8] =
        b"\xFF\xD8\xFF\xE0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00";
    const PNG_HEAD: &[u8] =
        b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR\x00\x00\x00\x04\x00\x00\x00\x02";

    #[test]
    fn jpeg_named_heic_is_jpeg() {
        assert_eq!(sniffed("x.heic", FileKind::Heic, JPEG_HEAD), FileKind::Jpeg);
    }

    #[test]
    fn heic_named_jpg_is_heic() {
        let heic = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/heic/sample.heic");
        let bytes = fs::read(heic).unwrap();
        assert_eq!(sniffed("x.jpg", FileKind::Jpeg, &bytes), FileKind::Heic);
    }

    #[test]
    fn heif_brand_among_the_compatible_brands_is_heic() {
        // An `ftyp` box of 24 bytes: major brand `xxxx`, minor version 0, then
        // the compatible brands `isom` and `msf1`.
        let mut ftyp = b"\x00\x00\x00\x18ftypxxxx\x00\x00\x00\x00isommsf1".to_vec();
        ftyp.extend_from_slice(b"\x00\x00\x00\x08free");
        assert_eq!(sniffed("x.png", FileKind::Png, &ftyp), FileKind::Heic);
    }

    #[test]
    fn png_named_tif_is_png() {
        assert_eq!(sniffed("x.tif", FileKind::Tiff, PNG_HEAD), FileKind::Png);
    }

    #[test]
    fn every_tiff_signature_is_tiff() {
        for head in [
            b"II*\x00\x08\x00\x00\x00",
            b"MM\x00*\x00\x00\x00\x08",
            b"II+\x00\x08\x00\x00\x00",
            b"MM\x00+\x00\x08\x00\x00",
        ] {
            assert_eq!(
                sniffed("x.jpg", FileKind::Jpeg, head),
                FileKind::Tiff,
                "{head:?}"
            );
        }
    }

    #[test]
    fn raw_extension_is_never_overridden() {
        for head in [b"II*\x00\x08\x00\x00\x00".as_slice(), JPEG_HEAD, PNG_HEAD] {
            assert_eq!(sniffed("x.nef", FileKind::Raw, head), FileKind::Raw);
        }
    }

    #[test]
    fn unknown_signature_keeps_kind() {
        // Garbage; an `ftyp` box whose brands are not HEIF ones (an MP4); and
        // a 16-byte `ftyp` box with no compatible brands, so the `mif1` of the
        // box after it is no brand.
        let mp4 = b"\x00\x00\x00\x18ftypisom\x00\x00\x02\x00isommp41";
        let next_box = b"\x00\x00\x00\x10ftypisom\x00\x00\x02\x00\x00\x00\x00\x08mif1";
        for head in [b"this is not a photo at all".as_slice(), mp4, next_box] {
            assert_eq!(sniffed("x.png", FileKind::Png, head), FileKind::Png);
            assert_eq!(sniffed("x.heic", FileKind::Heic, head), FileKind::Heic);
        }
    }

    #[test]
    fn short_file_keeps_kind() {
        // 3 bytes: the start of a PNG, TIFF or HEIC signature is no signature.
        for head in [b"\x89PN", b"II*", b"MM\x00", b"\x00\x00\x00"] {
            assert_eq!(
                sniffed("x.jpg", FileKind::Jpeg, head),
                FileKind::Jpeg,
                "{head:?}"
            );
        }
        assert_eq!(sniffed("x.jpg", FileKind::Jpeg, b""), FileKind::Jpeg);
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = effective_kind(&dir.path().join("gone.jpg"), FileKind::Jpeg).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
