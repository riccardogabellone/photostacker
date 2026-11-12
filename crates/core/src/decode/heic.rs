//! HEIC/HEIF thumbnails and EXIF through libheif (LGPL-3.0), via `libheif-rs`
//! 3 with `embedded-libheif` (feature `heic`).
//!
//! - **Build.** `libheif-sys` builds libheif itself with cmake, but not its
//!   codecs: it turns every codec on, and cmake would link each codec library
//!   it finds on the build host, x265 and x264 (GPL) among them. The toolchain
//!   file `packaging/cmake/libheif.cmake`, which `.cargo/config.toml` hands to
//!   cmake, hides all of them but libde265, so the x265 and x264 development
//!   files on a build host are ignored. HEVC decoding needs libde265. On
//!   Linux it is the system's shared library (`libde265-dev` on Debian/Ubuntu
//!   to build): the .deb depends on `libde265-0`, and the AppImage carries a
//!   copy. On macOS libheif links the static libde265 that
//!   `scripts/libde265.sh` builds, so the app bundle has no libde265 of its
//!   own. Windows links vcpkg's `libheif[core,libde265]` statically (on MSVC
//!   `libheif-sys` ignores `embedded-libheif` and uses vcpkg). Never `hevc` or
//!   x265. A test checks that libde265 is libheif's only codec.
//! - **Reading.** libheif reads through a [`StreamReader`] over the open file,
//!   so it reads only what it needs: the boxes, then the image or the `Exif`
//!   item. libheif's own file reader takes a narrow `char*` path, which on
//!   Windows means the ANSI code page.
//! - **Pixels.** The primary image is decoded with libheif's transforms
//!   applied (`clap`, `irot`, `imir`); the EXIF Orientation, which repeats
//!   them, is not applied again. Grid images (iPhone tiles) are assembled by
//!   libheif.
//!   - Thumbnails are interleaved 8-bit RGB: libheif drops the alpha and
//!     scales 10/12-bit images to 8 bits.
//!   - Full-resolution images keep the alpha, and a luma deeper than 8 bits
//!     is decoded to 16-bit little-endian samples, which we widen to the full
//!     16-bit range. The ICC profile is the handle's raw colour profile.
//! - **Threads.** rayon already runs one decode per core, so a decode adds
//!   no parallelism of its own. The tiles of a grid image are decoded one
//!   after the other on the calling thread (`set_max_decoding_threads(0)`;
//!   libheif's default is up to 4 threads per image), and the decoding
//!   options give the HEVC decoder one thread (libde265's own default, made
//!   explicit).
//! - **EXIF.** The first `Exif` metadata block of the primary image. It starts
//!   with a big-endian u32: the offset from the end of those 4 bytes to the
//!   TIFF header (ISO/IEC 23008-12, A.2.1). iPhones write 6, with `Exif\0\0`
//!   in between.

use std::fs::File;
use std::io::{self, BufReader};
use std::path::Path;
use std::sync::LazyLock;

use image::RgbImage;
use libheif_rs::{
    ColorSpace, CompressionFormat, DecodingOptions, HeifContext, HeifError, HeifErrorCode,
    ImageHandle, ItemId, LibHeif, RgbChroma, StreamReader,
};

use super::{DecodeError, FullImage, ImageSource, Probe, Samples};
use crate::exif::ExifError;
use crate::imageops;

/// libheif's global state, initialised once (`heif_init`) and never torn down.
static LIBHEIF: LazyLock<LibHeif> = LazyLock::new(LibHeif::new);

/// What this build's libheif is, for a report of the build (`photostacker-cli
/// info`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeifCodecs {
    /// libheif's version, `major.minor.patch`.
    pub version: String,
    /// The ids of its HEVC decoders, in libheif's order of preference
    /// (`libde265`).
    pub hevc_decoders: Vec<String>,
}

/// The most decoders of one format asked of libheif; it has a handful.
const MAX_DECODERS: usize = 16;

/// libheif's version and its HEVC decoders.
pub fn codecs() -> HeifCodecs {
    let [major, minor, patch] = LIBHEIF.version();
    let hevc_decoders = LIBHEIF
        .decoder_descriptors(MAX_DECODERS, Some(CompressionFormat::Hevc))
        .iter()
        .map(|decoder| decoder.id().to_owned())
        .collect();
    HeifCodecs {
        version: format!("{major}.{minor}.{patch}"),
        hevc_decoders,
    }
}

/// The prefix some writers put between the offset and the TIFF header.
const EXIF_PREFIX: &[u8] = b"Exif\0\0";

/// The primary image of a HEIF file, upright by its own transforms and shrunk
/// like `PIL.Image.thumbnail((max_side, max_side))`.
pub(crate) fn heic_thumbnail(path: &Path, max_side: u32) -> Result<RgbImage, DecodeError> {
    let primary = open_primary(path, DecodeError::Io, classify)?;
    let image = LIBHEIF
        .decode(
            &primary.handle,
            ColorSpace::Rgb(RgbChroma::Rgb),
            Some(one_thread_options()?),
        )
        .map_err(classify)?;
    let planes = image.planes();
    let plane = planes
        .interleaved
        .ok_or_else(|| DecodeError::Corrupt("libheif returned no interleaved RGB plane".into()))?;
    let rgb = packed_rgb(plane.data, plane.width, plane.height, plane.stride)?;
    drop(image);
    Ok(imageops::pillow_thumbnail(&rgb, max_side))
}

/// The TIFF block of the first `Exif` metadata block of the primary image;
/// `None` when it has none.
pub(crate) fn heic_exif(path: &Path) -> Result<Option<Vec<u8>>, ExifError> {
    let io_error = |source| ExifError::Io {
        what: "opening the file and reading its size",
        source,
    };
    let primary = open_primary(path, io_error, heif_error)?;
    let handle = &primary.handle;
    let mut ids: [ItemId; 1] = [0];
    if handle.metadata_block_ids(&mut ids, b"Exif") == 0 {
        return Ok(None);
    }
    // libheif-rs reports an empty block as a bare usage error; the offset
    // check names it instead.
    let block = match handle.metadata_size(ids[0]) {
        0 => Vec::new(),
        _ => handle.metadata(ids[0]).map_err(heif_error)?,
    };
    tiff_of_exif_block(block).map(Some)
}

/// The TIFF block inside an `Exif` metadata block: the bytes after the 4-byte
/// big-endian offset and the `offset` bytes it skips. A leftover `Exif\0\0`
/// prefix is dropped too, for writers that put it in front with offset 0.
fn tiff_of_exif_block(mut block: Vec<u8>) -> Result<Vec<u8>, ExifError> {
    let Some(&[a, b, c, d]) = block.get(..4) else {
        return Err(ExifError::Heif {
            what: format!(
                "the Exif block ({} bytes) ends before its 4-byte TIFF header offset",
                block.len()
            ),
        });
    };
    let offset = u32::from_be_bytes([a, b, c, d]);
    let start = usize::try_from(offset)
        .ok()
        .and_then(|offset| offset.checked_add(4))
        .filter(|&start| start <= block.len())
        .ok_or_else(|| ExifError::Heif {
            what: format!(
                "the Exif block's TIFF header offset {offset} is past its {} bytes",
                block.len()
            ),
        })?;
    let start = if block[start..].starts_with(EXIF_PREFIX) {
        start + EXIF_PREFIX.len()
    } else {
        start
    };
    block.drain(..start);
    Ok(block)
}

/// The primary image of an open HEIF file, and the context that reads it.
/// The handle comes first so that it is dropped first: the context owns the
/// reader that libheif reads the file through.
struct Primary {
    handle: ImageHandle,
    _context: HeifContext<'static>,
}

/// Opens `path` (see [`read_context`]) and its primary image. An I/O error
/// goes through `io_error`, a libheif one through `heif_error`.
fn open_primary<E>(
    path: &Path,
    io_error: impl Fn(io::Error) -> E,
    heif_error: impl Fn(HeifError) -> E,
) -> Result<Primary, E> {
    let file = File::open(path).map_err(&io_error)?;
    let len = file.metadata().map_err(&io_error)?.len();
    let context = read_context(file, len).map_err(&heif_error)?;
    let handle = context.primary_image_handle().map_err(&heif_error)?;
    Ok(Primary {
        handle,
        _context: context,
    })
}

/// A libheif context reading `file` (of `len` bytes) on demand. It decodes
/// the tiles of a grid image on the calling thread (see the module doc).
fn read_context(file: File, len: u64) -> Result<HeifContext<'static>, HeifError> {
    let mut context =
        HeifContext::read_from_reader(Box::new(StreamReader::new(BufReader::new(file), len)))?;
    context.set_max_decoding_threads(0);
    Ok(context)
}

/// Decoding options that give the HEVC decoder one thread (see the module
/// doc).
fn one_thread_options() -> Result<DecodingOptions, DecodeError> {
    let mut options = DecodingOptions::new().ok_or_else(|| {
        DecodeError::Unsupported("libheif could not allocate its decoding options".into())
    })?;
    options.set_num_codec_threads(1);
    Ok(options)
}

/// The primary image of a HEIF file at full resolution, upright by its own
/// transforms: 8-bit when its luma is, otherwise 16-bit (see [`widen_to_16`]);
/// RGBA when it has alpha.
pub(crate) fn heic_full(path: &Path) -> Result<FullImage, DecodeError> {
    let primary = open_primary(path, DecodeError::Io, classify)?;
    let handle = &primary.handle;
    let layout = Layout::of(handle);
    let chroma = match (layout.sixteen_bit, layout.has_alpha) {
        (false, false) => RgbChroma::Rgb,
        (false, true) => RgbChroma::Rgba,
        (true, false) => RgbChroma::HdrRgbLe,
        (true, true) => RgbChroma::HdrRgbaLe,
    };
    let image = LIBHEIF
        .decode(handle, ColorSpace::Rgb(chroma), Some(one_thread_options()?))
        .map_err(classify)?;
    let planes = image.planes();
    let plane = planes
        .interleaved
        .ok_or_else(|| DecodeError::Corrupt("libheif returned no interleaved RGB plane".into()))?;
    // The writers lay out the canvas from `heic_probe`.
    if (plane.width, plane.height) != (handle.width(), handle.height()) {
        return Err(DecodeError::Corrupt(format!(
            "libheif decoded {}x{} pixels for a {}x{} image",
            plane.width,
            plane.height,
            handle.width(),
            handle.height()
        )));
    }
    let channels: u8 = if layout.has_alpha { 4 } else { 3 };
    let bytes_per_sample = if layout.sixteen_bit { 2 } else { 1 };
    let bytes_per_pixel = usize::from(channels) * bytes_per_sample;
    let (len, rows) = rows(
        plane.data,
        plane.width,
        plane.height,
        plane.stride,
        bytes_per_pixel,
    )?;
    let samples = if layout.sixteen_bit {
        let bits = plane.bits_per_pixel;
        if !(8..=16).contains(&bits) {
            return Err(DecodeError::Corrupt(format!(
                "libheif returned {bits}-bit samples for a {}-bit image",
                handle.luma_bits_per_pixel()
            )));
        }
        let mut samples = Vec::with_capacity(len / 2);
        for row in rows {
            // A row holds whole pixels of 2-byte samples: nothing is left over.
            let (le_samples, _) = row.as_chunks::<2>();
            samples.extend(
                le_samples
                    .iter()
                    .map(|&le| widen_to_16(u16::from_le_bytes(le), bits)),
            );
        }
        Samples::U16(samples)
    } else {
        let mut samples = Vec::with_capacity(len);
        rows.for_each(|row| samples.extend_from_slice(row));
        Samples::U8(samples)
    };
    Ok(FullImage {
        width: plane.width,
        height: plane.height,
        channels,
        samples,
        icc: layout.icc,
        source: ImageSource::Decoded,
    })
}

/// What [`heic_full`] would give, from the primary image's handle alone.
pub(crate) fn heic_probe(path: &Path) -> Result<Probe, DecodeError> {
    let primary = open_primary(path, DecodeError::Io, classify)?;
    let handle = &primary.handle;
    let layout = Layout::of(handle);
    Ok(Probe {
        // libheif reports them with `clap`, `irot` and `imir` applied.
        width: handle.width(),
        height: handle.height(),
        sixteen_bit: layout.sixteen_bit,
        has_alpha: layout.has_alpha,
        icc: layout.icc,
    })
}

/// The sample layout of a primary image, shared by [`heic_full`] and
/// [`heic_probe`] so that they always agree.
struct Layout {
    sixteen_bit: bool,
    has_alpha: bool,
    icc: Option<Vec<u8>>,
}

impl Layout {
    fn of(handle: &ImageHandle) -> Self {
        Self {
            sixteen_bit: handle.luma_bits_per_pixel() > 8,
            has_alpha: handle.has_alpha_channel(),
            // An `nclx` colour box has no raw profile; `rICC` and `prof` are ICC.
            icc: handle.color_profile_raw().map(|profile| profile.data),
        }
    }
}

/// Scales a `bits`-deep sample (8..=16) to 16 bits by bit replication, so
/// that 0 stays 0 and the maximum becomes 65535. A sample over the maximum
/// counts as the maximum.
fn widen_to_16(sample: u16, bits: u8) -> u16 {
    let bits = u32::from(bits);
    let max = (1_u32 << bits) - 1;
    let v = u32::from(sample).min(max);
    // At most 16 bits: `v < 2^bits`, so `v << (16 − bits)` < 2^16.
    ((v << (16 - bits)) | (v >> (2 * bits - 16))) as u16
}

/// Copies the `width × height` RGB8 pixels out of rows `stride` bytes apart.
fn packed_rgb(
    data: &[u8],
    width: u32,
    height: u32,
    stride: usize,
) -> Result<RgbImage, DecodeError> {
    let (len, rows) = rows(data, width, height, stride, 3)?;
    let mut pixels = Vec::with_capacity(len);
    rows.for_each(|row| pixels.extend_from_slice(row));
    RgbImage::from_raw(width, height, pixels)
        .ok_or_else(|| plane_mismatch(data, width, height, stride))
}

/// The `height` rows of `width` pixels of `bytes_per_pixel` bytes each,
/// `stride` bytes apart in `data`, without their padding; and their total
/// length. Fails unless every row is inside `data`.
fn rows(
    data: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    bytes_per_pixel: usize,
) -> Result<(usize, impl Iterator<Item = &[u8]>), DecodeError> {
    let corrupt = || plane_mismatch(data, width, height, stride);
    let row = usize::try_from(width)
        .ok()
        .and_then(|w| w.checked_mul(bytes_per_pixel))
        .ok_or_else(corrupt)?;
    let rows = usize::try_from(height).map_err(|_| corrupt())?;
    // The last row starts at `(rows - 1) × stride` and needs no padding.
    let needed = rows
        .checked_sub(1)
        .and_then(|last| last.checked_mul(stride))
        .and_then(|last_start| last_start.checked_add(row));
    if row == 0 || stride < row || needed.is_none_or(|n| n > data.len()) {
        return Err(corrupt());
    }
    // Every row is inside `data` now, so `row × rows` cannot overflow.
    let lines = data.chunks(stride).take(rows).map(move |line| &line[..row]);
    Ok((row * rows, lines))
}

fn plane_mismatch(data: &[u8], width: u32, height: u32, stride: usize) -> DecodeError {
    DecodeError::Corrupt(format!(
        "libheif's {width}x{height} plane (stride {stride}) does not fit its {} bytes",
        data.len()
    ))
}

/// A file libheif cannot handle is unsupported (unknown brand, codec or
/// feature, or over its security limits); anything else is corrupt.
fn classify(err: HeifError) -> DecodeError {
    match err.code {
        HeifErrorCode::UnsupportedFileType
        | HeifErrorCode::UnsupportedFeature
        | HeifErrorCode::MemoryAllocationError => DecodeError::Unsupported(err.to_string()),
        _ => DecodeError::Corrupt(err.to_string()),
    }
}

fn heif_error(err: HeifError) -> ExifError {
    ExifError::Heif {
        what: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use chrono::NaiveDate;
    use image::Rgb;

    use super::*;
    use crate::decode::{ColourHint, thumbnail};
    use crate::exif::read_exif;
    use crate::hash::{HashPair, distance, hash_pair};
    use crate::model::{FileKind, RawTone};

    /// A committed fixture; see `tests/fixtures/heic/make_fixtures.py`.
    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/heic")
            .join(name)
    }

    fn near(pixel: &Rgb<u8>, want: [u8; 3], tolerance: u8) -> bool {
        pixel
            .0
            .iter()
            .zip(want)
            .all(|(&a, b)| a.abs_diff(b) <= tolerance)
    }

    #[test]
    fn heic_thumbnail_and_exif() {
        let path = fixture("sample.heic");
        let exif = read_exif(&path, FileKind::Heic)
            .unwrap()
            .expect("an EXIF block");
        let expected_time = NaiveDate::from_ymd_opt(2026, 11, 8)
            .unwrap()
            .and_hms_milli_opt(12, 34, 56, 250)
            .unwrap();
        assert_eq!(exif.make.as_deref(), Some("Photostacker"));
        assert_eq!(exif.model.as_deref(), Some("Fixture HEIC"));
        assert_eq!(exif.capture_time, Some(expected_time));
        assert_eq!(exif.orientation, 6);

        let thumb = thumbnail(&path, FileKind::Heic, exif.orientation, 512)
            .unwrap()
            .image;
        assert!(thumb.width().max(thumb.height()) <= 512);
        // 640x384 stored, 384x640 after `irot`; Pillow 12.2.0 thumbnails that to 307x512.
        assert_eq!(thumb.dimensions(), (307, 512));
    }

    #[test]
    fn heic_hashes_match_the_python_reference() {
        // The reference of `scripts/gen_hash_golden.py` (`fingerprint`), on a
        // pillow-heif 1.3.0 decode (libheif 1.21.2 and libde265 1.0.16, Pillow
        // 12.2.0, imagehash 4.3.2).
        for (name, phash, dhash) in [
            ("sample.heic", 0xda2d_5a2d_522d_5a2d, 0x0606_0004_0806_0009),
            (
                "deep-no-exif.heic",
                0xaa0d_133d_2533_2c7f,
                0xffff_ffff_ffff_ffff,
            ),
        ] {
            let thumb = thumbnail(&fixture(name), FileKind::Heic, 1, 512)
                .unwrap()
                .image;
            let ours = hash_pair(&thumb);
            let d = distance(ours, HashPair { phash, dhash });
            println!(
                "{name}: {:016x} {:016x}, distance {d}",
                ours.phash, ours.dhash
            );
            assert!(d <= 2, "{name}: distance {d}");
        }
    }

    #[test]
    fn libheif_transforms_apply_once_whatever_the_exif_orientation() {
        // Stored: red block top-left, green block top-right. `irot` turns it
        // 90° clockwise: red top-right, green bottom-right. Turning it again
        // by the EXIF Orientation 6 would put red at the bottom-right.
        let path = fixture("sample.heic");
        let upright = thumbnail(&path, FileKind::Heic, 1, 512).unwrap().image;
        for orientation in [0, 3, 6, 8] {
            let thumb = thumbnail(&path, FileKind::Heic, orientation, 512)
                .unwrap()
                .image;
            assert_eq!(thumb, upright, "orientation {orientation}");
        }
        let (w, h) = upright.dimensions();
        assert!(near(upright.get_pixel(w - 10, 10), [255, 0, 0], 24));
        assert!(near(upright.get_pixel(w - 10, h - 10), [0, 255, 0], 24));
        let corner = upright.get_pixel(10, 10);
        assert!(!near(corner, [255, 0, 0], 100) && !near(corner, [0, 255, 0], 100));
    }

    #[test]
    fn ten_bit_heic_without_exif_decodes_to_8_bit() {
        // 64x48 behind a `clap` crop of a 64x64 coded image, 10-bit, no EXIF.
        let path = fixture("deep-no-exif.heic");
        assert!(read_exif(&path, FileKind::Heic).unwrap().is_none());
        let thumb = thumbnail(&path, FileKind::Heic, 1, 512).unwrap().image;
        assert_eq!(thumb.dimensions(), (64, 48));
        // Red ramps along x, green along y, blue is 0x80.
        assert!(near(thumb.get_pixel(0, 0), [0, 0, 128], 12));
        assert!(near(thumb.get_pixel(63, 0), [255, 0, 128], 12));
        assert!(near(thumb.get_pixel(0, 47), [0, 255, 128], 12));
    }

    #[test]
    fn alpha_is_kept_at_full_resolution_and_dropped_from_the_thumbnail() {
        // 64x48, every pixel (200, 100, 50): opaque on the left half, alpha
        // 64 on the right half.
        let path = fixture("alpha.heic");
        let probed =
            crate::decode::probe(&path, FileKind::Heic, 1, RawTone::Srgb, ColourHint::None)
                .unwrap();
        assert_eq!((probed.width, probed.height), (64, 48));
        assert!(probed.has_alpha && !probed.sixteen_bit, "{probed:?}");

        let image =
            crate::decode::full(&path, FileKind::Heic, 1, RawTone::Srgb, ColourHint::None).unwrap();
        assert_eq!((image.width, image.height, image.channels), (64, 48, 4));
        let Samples::U8(samples) = &image.samples else {
            panic!("16-bit samples, expected 8-bit");
        };
        let rgba = |x: usize, y: usize| &samples[(y * 64 + x) * 4..][..4];
        let close =
            |got: &[u8], want: [u8; 4]| got.iter().zip(want).all(|(&a, b)| a.abs_diff(b) <= 8);
        for (x, alpha) in [(8, 255), (24, 255), (40, 64), (56, 64)] {
            for y in [8, 24, 40] {
                let got = rgba(x, y);
                assert!(close(got, [200, 100, 50, alpha]), "({x}, {y}): {got:?}");
            }
        }

        // The thumbnail drops the alpha without compositing it.
        let thumb = thumbnail(&path, FileKind::Heic, 1, 512).unwrap().image;
        assert_eq!(thumb.dimensions(), (64, 48));
        for (x, y) in [(8, 24), (56, 24)] {
            assert!(near(thumb.get_pixel(x, y), [200, 100, 50], 8), "({x}, {y})");
        }
    }

    #[test]
    fn exif_block_offset_is_honoured() {
        let tiff = b"MM\0\x2a\0\0\0\x08".to_vec();
        let block = |offset: u32, before: &[u8]| {
            let mut out = offset.to_be_bytes().to_vec();
            out.extend_from_slice(before);
            out.extend_from_slice(&tiff);
            out
        };
        // iPhones: offset 6, `Exif\0\0` before the header.
        assert_eq!(tiff_of_exif_block(block(6, b"Exif\0\0")).unwrap(), tiff);
        assert_eq!(tiff_of_exif_block(block(0, b"")).unwrap(), tiff);
        assert_eq!(tiff_of_exif_block(block(3, b"abc")).unwrap(), tiff);
        // Writers that put `Exif\0\0` in front but left the offset at 0.
        assert_eq!(tiff_of_exif_block(block(0, b"Exif\0\0")).unwrap(), tiff);
        // An offset to the very end leaves an empty block, which the TIFF reader rejects.
        assert_eq!(tiff_of_exif_block(block(8, b"")).unwrap(), b"");

        for bad in [
            Vec::new(),
            vec![0, 0, 0],
            block(9, b""),
            block(u32::MAX, b""),
        ] {
            let err = tiff_of_exif_block(bad.clone()).unwrap_err();
            assert!(matches!(err, ExifError::Heif { .. }), "{bad:?}: {err}");
        }
    }

    #[test]
    fn decodes_ask_for_one_codec_thread() {
        assert_eq!(one_thread_options().unwrap().num_codec_threads(), 1);
    }

    #[test]
    fn packed_rgb_drops_row_padding_and_rejects_short_planes() {
        // 2x2 pixels in rows of 8 bytes: 6 of pixels, 2 of padding.
        let data = [1, 2, 3, 4, 5, 6, 0, 0, 7, 8, 9, 10, 11, 12, 0, 0];
        let rgb = packed_rgb(&data, 2, 2, 8).unwrap();
        assert_eq!(rgb.into_raw(), [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        // The last row needs no padding.
        assert!(packed_rgb(&data[..14], 2, 2, 8).is_ok());
        for (len, width, height, stride) in [
            (13, 2, 2, 8),
            (16, 2, 2, 5),
            (16, 0, 2, 8),
            (16, 2, 0, 8),
            (16, u32::MAX, 1, usize::MAX),
            (16, 1, 3, usize::MAX),
        ] {
            let err = packed_rgb(&data[..len], width, height, stride).unwrap_err();
            assert!(
                matches!(err, DecodeError::Corrupt(_)),
                "{len} {width}x{height} stride {stride}"
            );
        }
    }

    #[test]
    fn deep_samples_widen_by_bit_replication() {
        for (sample, bits, want) in [
            (0, 10, 0),
            (1023, 10, 0xFFFF),
            (512, 10, 0x8020),
            (1, 10, 0x0040),
            (4095, 12, 0xFFFF),
            (0x800, 12, 0x8008),
            (255, 8, 0xFFFF),
            (0x12, 8, 0x1212),
            (0xABCD, 16, 0xABCD),
            // Over the maximum counts as the maximum.
            (2000, 10, 0xFFFF),
        ] {
            assert_eq!(widen_to_16(sample, bits), want, "{sample} at {bits} bits");
        }
    }

    #[test]
    fn truncated_or_garbage_heic_is_an_error_never_a_panic() {
        let bytes = fs::read(fixture("sample.heic")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cut.heic");
        // Cuts inside the boxes and the coded image must fail; any cut must not panic.
        let must_fail = [0, 3, 16, bytes.len() / 4, bytes.len() / 2];
        for keep in (0..bytes.len()).step_by(7).chain(must_fail) {
            fs::write(&path, &bytes[..keep]).unwrap();
            let thumb = thumbnail(&path, FileKind::Heic, 1, 512);
            let exif = read_exif(&path, FileKind::Heic);
            if must_fail.contains(&keep) {
                let err = thumb.unwrap_err();
                assert!(
                    matches!(err, DecodeError::Corrupt(_) | DecodeError::Unsupported(_)),
                    "{keep}: {err:?}"
                );
                assert!(exif.is_err(), "{keep}: {exif:?}");
                let full =
                    crate::decode::full(&path, FileKind::Heic, 1, RawTone::Srgb, ColourHint::None);
                assert!(full.is_err(), "{keep}: full decode succeeded");
            } else if let Ok(thumb) = thumb {
                assert_eq!(thumb.image.dimensions(), (307, 512), "{keep}");
            }
        }

        let mut state = 0x9E37_79B9_u32;
        let garbage: Vec<u8> = (0..4096)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        fs::write(&path, garbage).unwrap();
        assert!(thumbnail(&path, FileKind::Heic, 1, 512).is_err());
        assert!(
            crate::decode::full(&path, FileKind::Heic, 1, RawTone::Srgb, ColourHint::None).is_err()
        );
        assert!(
            crate::decode::probe(
                &path,
                FileKind::Heic,
                1,
                crate::model::RawTone::Srgb,
                ColourHint::None
            )
            .is_err()
        );
        let err = read_exif(&path, FileKind::Heic).unwrap_err();
        assert!(matches!(err, ExifError::Heif { .. }), "{err}");
    }

    #[test]
    fn missing_heic_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone.heic");
        let err = thumbnail(&gone, FileKind::Heic, 1, 512).unwrap_err();
        assert!(matches!(err, DecodeError::Io(_)), "{err:?}");
        let err = read_exif(&gone, FileKind::Heic).unwrap_err();
        assert!(matches!(err, ExifError::Io { .. }), "{err}");
    }

    #[test]
    fn codecs_name_libheif_s_version_and_its_hevc_decoder() {
        let codecs = codecs();
        let [major, minor, patch] = LIBHEIF.version();
        assert_eq!(codecs.version, format!("{major}.{minor}.{patch}"));
        assert_eq!(major, 1, "{codecs:?}");
        assert_eq!(codecs.hevc_decoders, ["libde265"]);
    }

    #[test]
    fn libde265_is_the_only_codec() {
        // libheif-sys turns every codec on, and cmake would link each codec
        // library it finds on the build host, x265 and x264 (GPL) included;
        // `packaging/cmake/libheif.cmake` hides all of them but libde265. Any
        // extra codec makes a list longer than expected, so the cap on the
        // lists' length does not matter.
        assert_eq!(codecs().hevc_decoders, ["libde265"]);
        let decoders: Vec<String> = LIBHEIF
            .decoder_descriptors(MAX_DECODERS, None)
            .iter()
            .map(|d| d.id().to_owned())
            .collect();
        assert_eq!(decoders, ["libde265"], "the decoders of every format");
        let encoders: Vec<String> = LIBHEIF
            .encoder_descriptors(MAX_DECODERS, None, None)
            .iter()
            .map(|e| e.id().to_owned())
            .collect();
        // `mask` is libheif's own encoder of mask images, built from its own
        // source and registered whatever the build found: it needs no library.
        assert_eq!(encoders, ["mask"], "the encoders of every format");
    }
}
