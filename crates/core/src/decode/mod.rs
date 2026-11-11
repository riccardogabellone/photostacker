//! Decoding of photos into small, upright RGB8 thumbnails for hashing, and at
//! full resolution for the writers (see `full.rs`).

use std::fs::File;
use std::io::{self, BufRead, BufReader, Cursor, Read, Seek};
use std::path::Path;

use image::imageops::{
    flip_horizontal_in_place, flip_vertical_in_place, rotate90, rotate180_in_place, rotate270,
};
use image::metadata::Orientation;
use image::{
    DynamicImage, ImageBuffer, ImageDecoder, ImageError, ImageFormat, ImageReader, Limits, Pixel,
    RgbImage,
};

use crate::budget::DECODE_MEMORY_BUDGET;
use crate::model::FileKind;
use crate::{guard, imageops};

mod full;
#[cfg(feature = "heic")]
pub(crate) mod heic;
mod peak;
mod preview;
mod raw;
mod sniff;

pub use full::{ColourHint, FullImage, Probe, Samples, full, probe};
pub(crate) use peak::{exif_read_peak, thumbnail_peak};
pub use sniff::effective_kind;

/// Where the pixels of a decoded image came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageSource {
    /// The file's own decoder: the image itself, or for a RAW whatever rawler
    /// gives (its develop, or the embedded preview it picks for a body it knows).
    Decoded,
    /// The largest JPEG embedded in a RAW that rawler could not decode, found
    /// by our own container parsing (see `preview.rs`).
    EmbeddedPreview,
}

/// An upright thumbnail, and where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Thumbnail {
    pub image: RgbImage,
    pub source: ImageSource,
    /// Why rawler failed, when `source` is [`ImageSource::EmbeddedPreview`];
    /// `None` otherwise.
    pub fallback_reason: Option<String>,
}

impl Thumbnail {
    /// A thumbnail from the file's own decoder.
    fn decoded(image: RgbImage) -> Self {
        Self {
            image,
            source: ImageSource::Decoded,
            fallback_reason: None,
        }
    }
}

/// Why a photo could not be decoded.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("unsupported image: {0}")]
    Unsupported(String),
    #[error("corrupt image: {0}")]
    Corrupt(String),
}

/// The length of an ICC profile header.
const ICC_HEADER_LEN: usize = 128;
/// The profile classes that describe an image's colours: input (`scnr`),
/// display (`mntr`), output (`prtr`) and colour space (`spac`).
const IMAGE_ICC_CLASSES: [&[u8; 4]; 4] = [b"scnr", b"mntr", b"prtr", b"spac"];

/// `icc` when it is an RGB profile that can describe the RGB pixels the
/// decoders give, else `None` (spec §5.5): at least a whole 128-byte header,
/// the data colour space `RGB ` (bytes 16..20) and one of
/// [`IMAGE_ICC_CLASSES`] (bytes 12..16).
///
/// Gray and CMYK sources are decoded to RGB, so their own Gray or CMYK
/// profile must not travel into an RGB document. [`probe`] and [`full`]
/// both pass every profile through here, so they agree.
pub(crate) fn rgb_icc(icc: Option<Vec<u8>>) -> Option<Vec<u8>> {
    icc.filter(|profile| {
        profile.len() >= ICC_HEADER_LEN
            && &profile[16..20] == b"RGB "
            && IMAGE_ICC_CLASSES
                .iter()
                .any(|class| profile[12..16] == class[..])
    })
}

/// Decodes `path`, turns it upright by its EXIF `orientation` (1..=8; any other
/// value means 1) and shrinks it like `PIL.Image.thumbnail((max_side, max_side))`.
///
/// HEIC ignores `orientation`: libheif applies the file's own transforms
/// (`clap`, `irot`, `imir`), which the EXIF Orientation only repeats.
/// Without the `heic` feature it is [`DecodeError::Unsupported`].
///
/// A RAW that rawler cannot decode falls back to its largest embedded JPEG,
/// with [`ImageSource::EmbeddedPreview`] (see `raw/`); every other
/// thumbnail is [`ImageSource::Decoded`].
///
/// The full-resolution image lives only inside this call.
pub fn thumbnail(
    path: &Path,
    kind: FileKind,
    orientation: u16,
    max_side: u32,
) -> Result<Thumbnail, DecodeError> {
    let format = match kind {
        FileKind::Jpeg => ImageFormat::Jpeg,
        FileKind::Png => ImageFormat::Png,
        FileKind::Tiff => ImageFormat::Tiff,
        FileKind::Raw => return raw::raw_thumbnail(path, orientation, max_side),
        #[cfg(feature = "heic")]
        FileKind::Heic => return heic::heic_thumbnail(path, max_side).map(Thumbnail::decoded),
        #[cfg(not(feature = "heic"))]
        FileKind::Heic => {
            return Err(DecodeError::Unsupported("built without heic".into()));
        }
    };
    let decoded = decode_still(path, format)?;
    let upright = orient(to_rgb8(decoded), orientation);
    Ok(Thumbnail::decoded(imageops::pillow_thumbnail(
        &upright, max_side,
    )))
}

/// Decodes a JPEG, PNG or TIFF (see [`still_decoder`]).
fn decode_still(path: &Path, format: ImageFormat) -> Result<DynamicImage, DecodeError> {
    DynamicImage::from_decoder(still_decoder(path, format)?).map_err(classify)
}

/// The decoder of a JPEG, PNG or TIFF, its headers read and no pixel yet.
/// Like Pillow, the file's signature picks the codec; the extension's
/// `format` is only the fallback for an unknown one.
fn still_decoder(path: &Path, format: ImageFormat) -> Result<Box<dyn ImageDecoder>, DecodeError> {
    let mut reader = ImageReader::new(BufReader::new(File::open(path)?)).with_guessed_format()?;
    let format = reader.format().unwrap_or(format);
    if format != ImageFormat::Jpeg {
        reader.set_format(format);
        return open_decoder(reader);
    }
    // The JPEG decoder reads the whole file into memory anyway.
    let mut bytes = Vec::new();
    reader.into_inner().read_to_end(&mut bytes)?;
    jpeg_decoder(bytes)
}

/// Decodes a whole JPEG held in memory (see [`jpeg_decoder`]).
fn decode_jpeg(bytes: Vec<u8>) -> Result<DynamicImage, DecodeError> {
    DynamicImage::from_decoder(jpeg_decoder(bytes)?).map_err(classify)
}

/// The decoder of a whole JPEG held in memory; a cut-off one is
/// [`DecodeError::Corrupt`] (see [`check_jpeg_complete`]).
fn jpeg_decoder(bytes: Vec<u8>) -> Result<Box<dyn ImageDecoder>, DecodeError> {
    check_jpeg_complete(&bytes).map_err(|why| DecodeError::Corrupt(why.to_owned()))?;
    open_decoder(ImageReader::with_format(
        Cursor::new(bytes),
        ImageFormat::Jpeg,
    ))
}

/// The decoder of `reader`, which has its format set, with the limits
/// [`ImageReader::decode`] gives it, but taken from the decode memory budget
/// ([`DECODE_MEMORY_BUDGET`], 2 GiB) instead of image's default 512 MiB:
/// the image's bytes T are reserved from the budget, and the
/// decoder gets what is left. It refuses, as [`DecodeError::Unsupported`]:
/// - any image whose decoded size T is over 2 GiB, before anything is
///   allocated: about 358 MP of 16-bit RGB or 716 MP of 8-bit;
/// - a TIFF over about 1 GiB, because image's TIFF decoder needs a buffer of
///   its own as large as the image: about 179 MP of 16-bit RGB or 358 MP of
///   8-bit;
/// - a TIFF whose largest strip or tile is over 2 GiB − 2T, the decoder's
///   intermediate buffer. A single-strip uncompressed TIFF is therefore
///   capped near 683 MiB: about 119 MP of 16-bit RGB or 239 MP of 8-bit.
///
/// For a CMYK TIFF the decoder's buffer holds 4 or 8 bytes per pixel, so its
/// caps are a little lower.
///
/// These limits bound one decode. How many decodes run at once is up to the
/// analysis' [`MemoryBudget`](crate::budget::MemoryBudget) (see
/// [`thumbnail_peak`]).
fn open_decoder<R>(mut reader: ImageReader<R>) -> Result<Box<dyn ImageDecoder>, DecodeError>
where
    R: BufRead + Seek + 'static,
{
    let mut limits = Limits::default();
    limits.max_alloc = Some(DECODE_MEMORY_BUDGET);
    // The decoder is built with the whole budget, as `decode()` builds it
    // with the reader's limits.
    reader.limits(limits.clone());
    let mut decoder = reader.into_decoder().map_err(classify)?;
    limits.reserve(decoder.total_bytes()).map_err(classify)?;
    decoder.set_limits(limits).map_err(classify)?;
    Ok(Box::new(decoder))
}

/// Fails unless the entropy-coded data after the first scan header reaches an
/// end-of-image marker.
///
/// zune-jpeg, as `image` configures it, paints the missing part of a cut-off
/// scan gray and reports success. Pillow raises "image file is truncated"
/// instead, even when only the final EOI is missing, and a cut-off photo must
/// not be hashed as if it were whole.
fn check_jpeg_complete(bytes: &[u8]) -> Result<(), &'static str> {
    const FILL: u8 = 0xFF;
    if !bytes.starts_with(&[FILL, 0xD8]) {
        return Err("not a JPEG: no start-of-image marker");
    }
    let mut pos = 2;
    let scan_start = loop {
        // Like libjpeg, skip stray bytes and fill bytes up to the marker code.
        while bytes.get(pos).is_some_and(|&b| b != FILL) {
            pos += 1;
        }
        while bytes.get(pos) == Some(&FILL) {
            pos += 1;
        }
        let Some(&marker) = bytes.get(pos) else {
            return Err("truncated JPEG: the file ends before the first scan");
        };
        pos += 1;
        match marker {
            0xD9 => return Err("malformed JPEG: end-of-image before any scan"),
            // Standalone markers carry no length. A stuffed FF 00 is no
            // marker at all: libjpeg skips it as stray data, and so do we.
            0x00 | 0x01 | 0xD0..=0xD7 => continue,
            _ => {}
        }
        let Some(&[hi, lo]) = bytes.get(pos..pos + 2) else {
            return Err("truncated JPEG: the file ends inside a segment header");
        };
        pos += usize::from(u16::from_be_bytes([hi, lo]));
        if marker == 0xDA {
            break pos;
        }
    };
    // Inside entropy-coded data a 0xFF is always stuffed (FF 00) or a restart
    // marker, so the last FF D9 of the file is an EOI; it must follow the scan.
    let last_eoi = bytes.windows(2).rposition(|w| w == [FILL, 0xD9]);
    match last_eoi {
        Some(at) if at >= scan_start => Ok(()),
        _ => Err("truncated JPEG: the scan data has no end-of-image marker"),
    }
}

/// Runs `work` on `path`, turning a panic into [`DecodeError::Corrupt`] with
/// the message `{context}: {panic message}`, which is also logged as a
/// warning with the path.
fn guarded<T>(
    path: &Path,
    context: &str,
    work: impl FnOnce() -> Result<T, DecodeError>,
) -> Result<T, DecodeError> {
    guard::catch(work).unwrap_or_else(|panic| {
        let error = format!("{context}: {panic}");
        tracing::warn!(path = %path.display(), %error, "caught a panic while decoding");
        Err(DecodeError::Corrupt(error))
    })
}

fn classify(err: ImageError) -> DecodeError {
    match err {
        // Decoders report data that ends early or makes no sense as I/O errors too.
        ImageError::IoError(e)
            if matches!(
                e.kind(),
                io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData
            ) =>
        {
            DecodeError::Corrupt(e.to_string())
        }
        ImageError::IoError(e) => DecodeError::Io(e),
        ImageError::Unsupported(e) => DecodeError::Unsupported(e.to_string()),
        ImageError::Limits(e) => DecodeError::Unsupported(e.to_string()),
        e @ (ImageError::Decoding(_) | ImageError::Parameter(_) | ImageError::Encoding(_)) => {
            DecodeError::Corrupt(e.to_string())
        }
    }
}

/// RGB8 with Pillow's conversions: 16-bit samples keep their high byte (`>> 8`,
/// where `image` would round), gray is replicated, alpha is dropped.
fn to_rgb8(decoded: DynamicImage) -> RgbImage {
    match decoded {
        DynamicImage::ImageRgb8(rgb) => rgb,
        DynamicImage::ImageLuma16(_)
        | DynamicImage::ImageLumaA16(_)
        | DynamicImage::ImageRgb16(_)
        | DynamicImage::ImageRgba16(_) => {
            let deep = decoded.into_rgb16();
            let (width, height) = deep.dimensions();
            let bytes = deep
                .into_raw()
                .into_iter()
                .map(|v| (v >> 8) as u8)
                .collect();
            RgbImage::from_raw(width, height, bytes).expect("one byte per 16-bit sample")
        }
        other => other.into_rgb8(),
    }
}

/// Applies the EXIF Orientation transform, like
/// [`DynamicImage::apply_orientation`] but to any pixel type; values outside
/// 1..=8 leave the image as is.
fn orient<P>(
    mut image: ImageBuffer<P, Vec<P::Subpixel>>,
    orientation: u16,
) -> ImageBuffer<P, Vec<P::Subpixel>>
where
    P: Pixel + 'static,
{
    match exif_orientation(orientation) {
        Orientation::NoTransforms => image,
        Orientation::FlipHorizontal => {
            flip_horizontal_in_place(&mut image);
            image
        }
        Orientation::Rotate180 => {
            rotate180_in_place(&mut image);
            image
        }
        Orientation::FlipVertical => {
            flip_vertical_in_place(&mut image);
            image
        }
        Orientation::Rotate90 => rotate90(&image),
        Orientation::Rotate270 => rotate270(&image),
        Orientation::Rotate90FlipH => {
            let mut turned = rotate90(&image);
            flip_horizontal_in_place(&mut turned);
            turned
        }
        Orientation::Rotate270FlipH => {
            let mut turned = rotate270(&image);
            flip_horizontal_in_place(&mut turned);
            turned
        }
    }
}

/// The transform of an EXIF Orientation value; values outside 1..=8 mean none.
fn exif_orientation(orientation: u16) -> Orientation {
    u8::try_from(orientation)
        .ok()
        .and_then(Orientation::from_exif)
        .unwrap_or(Orientation::NoTransforms)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use image::{DynamicImage, ImageBuffer, Rgb, Rgba};

    use super::*;

    const RED: Rgb<u8> = Rgb([255, 0, 0]);

    fn save(dir: &Path, name: &str, image: &DynamicImage) -> PathBuf {
        let path = dir.join(name);
        image.save(&path).unwrap();
        path
    }

    /// A 4×2 PNG, black but for a red top-left pixel.
    fn red_corner_png(dir: &Path) -> PathBuf {
        let mut img = RgbImage::new(4, 2);
        img.put_pixel(0, 0, RED);
        save(dir, "corner.png", &DynamicImage::ImageRgb8(img))
    }

    fn red_pixels(img: &RgbImage) -> Vec<(u32, u32)> {
        img.enumerate_pixels()
            .filter(|(_, _, p)| **p == RED)
            .map(|(x, y, _)| (x, y))
            .collect()
    }

    /// A noisy 8-bit image, so a JPEG of it has a long entropy-coded segment.
    fn noise(width: u32, height: u32) -> RgbImage {
        let mut state = 0x2545_f491_u32;
        RgbImage::from_fn(width, height, |_, _| {
            let mut next = || {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            };
            Rgb([next(), next(), next()])
        })
    }

    #[test]
    fn orientation_6_rotates_90_cw() {
        let dir = tempfile::tempdir().unwrap();
        let path = red_corner_png(dir.path());
        let thumb = thumbnail(&path, FileKind::Png, 6, 512).unwrap().image;
        assert_eq!(thumb.dimensions(), (2, 4));
        assert_eq!(red_pixels(&thumb), [(1, 0)]);
    }

    #[test]
    fn every_orientation_matches_pillow_exif_transpose() {
        // Pillow 12.2.0: ImageOps.exif_transpose of the same 4×2 image per Orientation tag.
        let cases = [
            (1, (4, 2), (0, 0)),
            (2, (4, 2), (3, 0)),
            (3, (4, 2), (3, 1)),
            (4, (4, 2), (0, 1)),
            (5, (2, 4), (0, 0)),
            (6, (2, 4), (1, 0)),
            (7, (2, 4), (1, 3)),
            (8, (2, 4), (0, 3)),
        ];
        let dir = tempfile::tempdir().unwrap();
        let path = red_corner_png(dir.path());
        for (orientation, size, red) in cases {
            let thumb = thumbnail(&path, FileKind::Png, orientation, 512)
                .unwrap()
                .image;
            assert_eq!(thumb.dimensions(), size, "orientation {orientation}");
            assert_eq!(red_pixels(&thumb), [red], "orientation {orientation}");
        }
    }

    #[test]
    fn out_of_range_orientation_means_upright() {
        let dir = tempfile::tempdir().unwrap();
        let path = red_corner_png(dir.path());
        for orientation in [0, 9, 256, u16::MAX] {
            let thumb = thumbnail(&path, FileKind::Png, orientation, 512)
                .unwrap()
                .image;
            assert_eq!(thumb.dimensions(), (4, 2), "orientation {orientation}");
            assert_eq!(red_pixels(&thumb), [(0, 0)], "orientation {orientation}");
        }
    }

    #[test]
    fn sixteen_bit_png_decodes() {
        // `>> 8` keeps the high byte: 0x12FF gives 0x12, where rounding would give 0x13.
        let img = ImageBuffer::from_pixel(3, 2, Rgb([0x12FF_u16, 0xAB80, 0x00FF]));
        let dir = tempfile::tempdir().unwrap();
        let path = save(dir.path(), "deep.png", &DynamicImage::ImageRgb16(img));
        let thumb = thumbnail(&path, FileKind::Png, 1, 512).unwrap().image;
        assert_eq!(thumb.dimensions(), (3, 2));
        assert!(thumb.pixels().all(|p| *p == Rgb([0x12, 0xAB, 0x00])));
    }

    #[test]
    fn sixteen_bit_tiff_decodes() {
        let img = ImageBuffer::from_pixel(3, 2, Rgb([0xFFFF_u16, 0x0100, 0x80FF]));
        let dir = tempfile::tempdir().unwrap();
        let path = save(dir.path(), "deep.tif", &DynamicImage::ImageRgb16(img));
        let thumb = thumbnail(&path, FileKind::Tiff, 1, 512).unwrap().image;
        assert!(thumb.pixels().all(|p| *p == Rgb([0xFF, 0x01, 0x80])));
    }

    #[test]
    fn alpha_is_dropped_not_composited() {
        let img = ImageBuffer::from_pixel(2, 2, Rgba([0x40FF_u16, 0x8000, 0xC000, 0]));
        let dir = tempfile::tempdir().unwrap();
        let path = save(dir.path(), "clear.png", &DynamicImage::ImageRgba16(img));
        let thumb = thumbnail(&path, FileKind::Png, 1, 512).unwrap().image;
        assert!(thumb.pixels().all(|p| *p == Rgb([0x40, 0x80, 0xC0])));

        let img = ImageBuffer::from_pixel(2, 2, Rgba([7_u8, 8, 9, 0]));
        let path = save(dir.path(), "clear8.png", &DynamicImage::ImageRgba8(img));
        let thumb = thumbnail(&path, FileKind::Png, 1, 512).unwrap().image;
        assert!(thumb.pixels().all(|p| *p == Rgb([7, 8, 9])));
    }

    #[test]
    fn grayscale_becomes_gray_rgb() {
        let img = ImageBuffer::from_pixel(2, 2, image::Luma([0x9A00_u16]));
        let dir = tempfile::tempdir().unwrap();
        let path = save(dir.path(), "gray.png", &DynamicImage::ImageLuma16(img));
        let thumb = thumbnail(&path, FileKind::Png, 1, 512).unwrap().image;
        assert!(thumb.pixels().all(|p| *p == Rgb([0x9A, 0x9A, 0x9A])));
    }

    #[test]
    fn every_still_codec_decodes() {
        // Codecs are opt-in features of `image`; a missing one only fails at run time.
        let flat = RgbImage::from_pixel(64, 48, Rgb([200, 100, 50]));
        let dir = tempfile::tempdir().unwrap();
        for (name, kind) in [
            ("a.jpg", FileKind::Jpeg),
            ("a.png", FileKind::Png),
            ("a.tif", FileKind::Tiff),
        ] {
            let path = save(dir.path(), name, &DynamicImage::ImageRgb8(flat.clone()));
            let thumb = thumbnail(&path, kind, 1, 512)
                .unwrap_or_else(|e| panic!("{name}: {e}"))
                .image;
            assert_eq!(thumb.dimensions(), (64, 48), "{name}");
            let p = thumb.get_pixel(32, 24).0;
            let close = p
                .iter()
                .zip([200, 100, 50])
                .all(|(&a, b)| a.abs_diff(b) <= 3);
            assert!(close, "{name}: {p:?}");
        }
    }

    #[test]
    fn large_photo_is_shrunk_to_max_side() {
        let dir = tempfile::tempdir().unwrap();
        let path = save(
            dir.path(),
            "big.jpg",
            &DynamicImage::ImageRgb8(noise(1200, 800)),
        );
        let thumb = thumbnail(&path, FileKind::Jpeg, 1, 512).unwrap().image;
        assert_eq!(thumb.dimensions(), (512, 341));
        let rotated = thumbnail(&path, FileKind::Jpeg, 8, 512).unwrap().image;
        assert_eq!(rotated.dimensions(), (341, 512));
    }

    #[test]
    fn content_decides_the_codec_not_the_extension() {
        let dir = tempfile::tempdir().unwrap();
        let png = red_corner_png(dir.path());
        let misnamed = dir.path().join("really-a-png.jpg");
        fs::rename(&png, &misnamed).unwrap();
        let thumb = thumbnail(&misnamed, FileKind::Jpeg, 1, 512).unwrap().image;
        assert_eq!(red_pixels(&thumb), [(0, 0)]);
    }

    #[test]
    fn corrupt_jpeg_is_decode_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = save(
            dir.path(),
            "whole.jpg",
            &DynamicImage::ImageRgb8(noise(256, 256)),
        );
        let bytes = fs::read(&path).unwrap();
        // Pillow 12.2.0 raises "image file is truncated" for each of these cuts,
        // including the one that loses only the final EOI marker.
        let len = bytes.len();
        for keep in [len - 1, len - 2, len / 2, 600, 3] {
            let cut = dir.path().join(format!("cut{keep}.jpg"));
            fs::write(&cut, &bytes[..keep]).unwrap();
            let err = thumbnail(&cut, FileKind::Jpeg, 1, 512).unwrap_err();
            assert!(
                matches!(err, DecodeError::Corrupt(_)),
                "{keep} bytes: {err:?}"
            );
        }
    }

    #[test]
    fn bytes_after_the_eoi_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = save(
            dir.path(),
            "tail.jpg",
            &DynamicImage::ImageRgb8(noise(64, 64)),
        );
        let mut bytes = fs::read(&path).unwrap();
        bytes.extend_from_slice(&[0x00, 0x12, 0xFF, 0xD8, 0xAB]);
        fs::write(&path, &bytes).unwrap();
        assert_eq!(
            thumbnail(&path, FileKind::Jpeg, 1, 512)
                .unwrap()
                .image
                .dimensions(),
            (64, 64)
        );
    }

    #[test]
    fn an_embedded_thumbnail_does_not_complete_a_cut_jpeg() {
        // The EOI of a preview JPEG inside APP1 comes before the main scan; it
        // must not make a truncated main image look whole.
        let dir = tempfile::tempdir().unwrap();
        let preview = save(dir.path(), "p.jpg", &DynamicImage::ImageRgb8(noise(8, 8)));
        let preview = fs::read(&preview).unwrap();
        let main = save(
            dir.path(),
            "m.jpg",
            &DynamicImage::ImageRgb8(noise(128, 128)),
        );
        let main = fs::read(&main).unwrap();

        let app1_len = u16::try_from(2 + preview.len()).unwrap().to_be_bytes();
        let mut whole = vec![0xFF, 0xD8, 0xFF, 0xE1, app1_len[0], app1_len[1]];
        whole.extend_from_slice(&preview);
        whole.extend_from_slice(&main[2..]);
        let path = dir.path().join("with-preview.jpg");
        fs::write(&path, &whole).unwrap();
        assert_eq!(
            thumbnail(&path, FileKind::Jpeg, 1, 512)
                .unwrap()
                .image
                .dimensions(),
            (128, 128)
        );

        fs::write(&path, &whole[..whole.len() - 200]).unwrap();
        let err = thumbnail(&path, FileKind::Jpeg, 1, 512).unwrap_err();
        assert!(matches!(err, DecodeError::Corrupt(_)), "{err:?}");
    }

    #[test]
    fn jpeg_with_ff00_in_header_area_is_complete() {
        // A stuffed FF 00 between two header segments is a stray byte pair to
        // libjpeg, not a marker: read as one, the next marker's FF xx would be
        // taken for a segment length and the walk would run off the file.
        // Fill bytes may come before the 00, as before any marker code.
        let dir = tempfile::tempdir().unwrap();
        let whole_path = save(
            dir.path(),
            "whole.jpg",
            &DynamicImage::ImageRgb8(noise(32, 32)),
        );
        let whole = fs::read(&whole_path).unwrap();
        let first_len = usize::from(u16::from_be_bytes([whole[4], whole[5]]));
        let after_first = 4 + first_len;
        assert_eq!(whole[after_first], 0xFF, "precondition: a marker follows");
        let expected = thumbnail(&whole_path, FileKind::Jpeg, 1, 512)
            .unwrap()
            .image;
        for stray in [&[0xFF, 0x00][..], &[0xFF, 0xFF, 0x00]] {
            let mut bytes = whole.clone();
            bytes.splice(after_first..after_first, stray.iter().copied());
            assert_eq!(check_jpeg_complete(&bytes), Ok(()), "{stray:02X?}");
            // The decoder skips the stray bytes too: the image is whole.
            let path = dir.path().join("stray.jpg");
            fs::write(&path, &bytes).unwrap();
            let thumb = thumbnail(&path, FileKind::Jpeg, 1, 512)
                .unwrap_or_else(|e| panic!("{stray:02X?}: {e}"))
                .image;
            assert_eq!(thumb, expected, "{stray:02X?}");
        }
    }

    #[test]
    fn guarded_turns_a_panic_into_a_corrupt_error() {
        let path = Path::new("x.jpg");
        assert_eq!(guarded(path, "internal error", || Ok(7)).unwrap(), 7);
        // An error passes through as it is.
        let err = guarded(path, "internal error", || -> Result<u8, DecodeError> {
            Err(DecodeError::Unsupported("no codec".into()))
        })
        .unwrap_err();
        assert!(
            matches!(&err, DecodeError::Unsupported(why) if why == "no codec"),
            "{err:?}"
        );
        let err = guarded(path, "internal error", || -> Result<u8, DecodeError> {
            panic!("tile 3 is out of range")
        })
        .unwrap_err();
        let want = "internal error: tile 3 is out of range";
        assert!(
            matches!(&err, DecodeError::Corrupt(why) if why == want),
            "{err:?}"
        );
    }

    #[test]
    fn corrupt_png_and_tiff_are_decode_errors() {
        let dir = tempfile::tempdir().unwrap();
        for (name, kind) in [("whole.png", FileKind::Png), ("whole.tif", FileKind::Tiff)] {
            let path = save(dir.path(), name, &DynamicImage::ImageRgb8(noise(128, 128)));
            let bytes = fs::read(&path).unwrap();
            fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();
            let err = thumbnail(&path, kind, 1, 512).unwrap_err();
            assert!(matches!(err, DecodeError::Corrupt(_)), "{name}: {err:?}");
        }
    }

    #[test]
    fn garbage_is_decode_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("garbage.jpg");
        fs::write(&path, b"this is not a photo at all").unwrap();
        let err = thumbnail(&path, FileKind::Jpeg, 1, 512).unwrap_err();
        assert!(matches!(err, DecodeError::Corrupt(_)), "{err:?}");
    }

    #[cfg(not(feature = "heic"))]
    #[test]
    fn heic_without_feature_is_unsupported() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/heic/sample.heic");
        assert_eq!(
            crate::discover::kind_of(&path),
            Some(FileKind::Heic),
            "HEIC files are still found"
        );
        let err = thumbnail(&path, FileKind::Heic, 1, 512).unwrap_err();
        assert!(
            matches!(&err, DecodeError::Unsupported(why) if why == "built without heic"),
            "{err:?}"
        );
        assert!(
            crate::exif::read_exif(&path, FileKind::Heic)
                .unwrap()
                .is_none()
        );
    }

    #[cfg(feature = "heic")]
    #[test]
    fn heic_kind_goes_to_libheif() {
        // libheif knows no PNG: a PNG named as a HEIC is not decoded as a still.
        let dir = tempfile::tempdir().unwrap();
        let path = red_corner_png(dir.path());
        let err = thumbnail(&path, FileKind::Heic, 1, 512).unwrap_err();
        assert!(
            matches!(err, DecodeError::Unsupported(_) | DecodeError::Corrupt(_)),
            "{err:?}"
        );
    }

    #[test]
    fn raw_kind_goes_to_the_raw_decoder() {
        // rawler knows no PNG: a PNG named as a RAW is not decoded as a still.
        let dir = tempfile::tempdir().unwrap();
        let path = red_corner_png(dir.path());
        let err = thumbnail(&path, FileKind::Raw, 1, 512).unwrap_err();
        assert!(matches!(err, DecodeError::Unsupported(_)), "{err:?}");
    }

    #[test]
    fn missing_file_is_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = thumbnail(&dir.path().join("gone.jpg"), FileKind::Jpeg, 1, 512).unwrap_err();
        assert!(matches!(err, DecodeError::Io(_)), "{err:?}");
    }

    #[test]
    fn rgb_icc_keeps_only_rgb_profiles_of_the_four_classes() {
        use photostacker_testkit::icc_stub;

        for class in [b"mntr", b"scnr", b"prtr", b"spac"] {
            let rgb = icc_stub(class, b"RGB ");
            assert_eq!(rgb_icc(Some(rgb.clone())), Some(rgb), "{class:?}");
        }
        // Our own RAW profiles are RGB display profiles.
        for tone in [crate::model::RawTone::Srgb, crate::model::RawTone::Linear] {
            let ours = crate::icc::rgb_profile(tone);
            assert_eq!(rgb_icc(Some(ours.clone())), Some(ours), "{tone:?}");
        }
        for space in [b"GRAY", b"CMYK", b"Lab ", b"XYZ "] {
            let profile = icc_stub(b"mntr", space);
            assert_eq!(rgb_icc(Some(profile)), None, "{space:?}");
        }
        // A device link, abstract or named-colour profile describes no image.
        for class in [b"link", b"abst", b"nmcl", b"\0\0\0\0"] {
            let profile = icc_stub(class, b"RGB ");
            assert_eq!(rgb_icc(Some(profile)), None, "{class:?}");
        }
        // Shorter than a header, however it starts.
        let header = icc_stub(b"mntr", b"RGB ");
        assert_eq!(rgb_icc(Some(header[..127].to_vec())), None);
        assert_eq!(
            rgb_icc(Some(header[..128].to_vec())),
            Some(header[..128].to_vec())
        );
        assert_eq!(rgb_icc(Some(Vec::new())), None);
        assert_eq!(rgb_icc(None), None);
    }

    /// An uncompressed RGB8 TIFF whose header declares `width × height` pixels
    /// in one-row strips, followed by only 64 bytes of strip data that every
    /// strip points at. One-row strips keep each read under image's
    /// intermediate-buffer limit, so only the decoding-buffer budget decides.
    fn tiff_header_only(width: u32, height: u32) -> Vec<u8> {
        use photostacker_testkit::{TiffSpec, Value};
        let rows = usize::try_from(height).unwrap();
        let spec = |strip_offset: u32| TiffSpec {
            little_endian: true,
            ifd0: vec![
                (256, Value::Long(vec![width])),
                (257, Value::Long(vec![height])),
                (258, Value::Short(vec![8, 8, 8])),
                (259, Value::Short(vec![1])),
                (262, Value::Short(vec![2])),
                (273, Value::Long(vec![strip_offset; rows])),
                (277, Value::Short(vec![3])),
                (278, Value::Long(vec![1])),
                (279, Value::Long(vec![width * 3; rows])),
                (284, Value::Short(vec![1])),
            ],
            exif: Vec::new(),
            maker_note: None,
        };
        // The offsets' value has the same length whatever they hold.
        let len = u32::try_from(spec(0).build().len()).unwrap();
        let mut bytes = spec(len).build();
        bytes.extend_from_slice(&[0x55; 64]);
        bytes
    }

    #[test]
    fn decode_limits_are_the_memory_budget() {
        use crate::model::RawTone;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.tif");
        let corrupt = |err: DecodeError| matches!(err, DecodeError::Corrupt(_));
        let refused = |err: DecodeError| matches!(err, DecodeError::Unsupported(_));

        // 10000×10000 RGB8 is 286 MiB. `ImageReader::decode`, with image's
        // default 512 MiB, refuses it: the TIFF decoder needs a buffer of its
        // own as large. Within the budget it gets as far as the missing
        // strip data.
        fs::write(&path, tiff_header_only(10_000, 10_000)).unwrap();
        let reference = ImageReader::open(&path)
            .unwrap()
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap_err();
        assert!(matches!(reference, ImageError::Limits(_)), "{reference:?}");
        assert!(corrupt(
            thumbnail(&path, FileKind::Tiff, 1, 512).unwrap_err()
        ));
        assert!(corrupt(
            full(&path, FileKind::Tiff, 1, RawTone::Srgb, ColourHint::None).unwrap_err()
        ));

        // 20000×20000 RGB8 is 1144 MiB: within the budget as an image, but
        // with the TIFF decoder's own buffer it is over. The limit fails it
        // before any strip data is read. The probe reads only the header: it
        // may accept what `full` refuses (the writers exclude a member whose
        // load fails), never the reverse.
        fs::write(&path, tiff_header_only(20_000, 20_000)).unwrap();
        assert!(refused(
            thumbnail(&path, FileKind::Tiff, 1, 512).unwrap_err()
        ));
        assert!(refused(
            full(&path, FileKind::Tiff, 1, RawTone::Srgb, ColourHint::None).unwrap_err()
        ));
        let probed = probe(&path, FileKind::Tiff, 6, RawTone::Srgb, ColourHint::None).unwrap();
        assert_eq!((probed.width, probed.height), (20_000, 20_000));

        // 30000×30000 RGB8 is 2575 MiB, over the budget itself.
        fs::write(&path, tiff_header_only(30_000, 30_000)).unwrap();
        assert!(refused(
            thumbnail(&path, FileKind::Tiff, 1, 512).unwrap_err()
        ));
        assert!(refused(
            full(&path, FileKind::Tiff, 1, RawTone::Srgb, ColourHint::None).unwrap_err()
        ));
    }

    /// The thumbnail of a testkit 16-bit RGB TIFF of `width × height`, in
    /// strips of `rows_per_strip` rows, written to a temporary folder. Its
    /// red ramps along x and its green along y, so the top-left corner is
    /// black and the bottom-right yellow, both with half blue.
    fn rgb16_tiff_thumbnail(width: u32, height: u32, rows_per_strip: u32) -> RgbImage {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.tif");
        photostacker_testkit::write_rgb16_tiff(&path, width, height, rows_per_strip).unwrap();
        let thumb = thumbnail(&path, FileKind::Tiff, 1, 512)
            .unwrap_or_else(|e| panic!("{width}×{height}: {e}"))
            .image;
        let (w, h) = thumb.dimensions();
        let near =
            |got: Rgb<u8>, want: [u8; 3]| got.0.iter().zip(want).all(|(&a, b)| a.abs_diff(b) <= 4);
        assert!(
            near(*thumb.get_pixel(0, 0), [0, 0, 0x80]),
            "{:?}",
            thumb.get_pixel(0, 0)
        );
        let corner = *thumb.get_pixel(w - 1, h - 1);
        assert!(near(corner, [0xFF, 0xFF, 0x80]), "{corner:?}");
        thumb
    }

    // These two hold 500–750 MB each: `.config/nextest.toml` runs them one
    // at a time (test group `big-memory`).

    #[test]
    fn tiff_over_256_mib_decodes() {
        // 8000×6000 16-bit RGB is 288 MB: image's default limits refused a
        // TIFF over 256 MiB, as its decoder holds a buffer of its own as large.
        let thumb = rgb16_tiff_thumbnail(8000, 6000, 16);
        assert_eq!(thumb.dimensions(), (512, 384));
    }

    #[test]
    fn single_strip_tiff_over_171_mib_decodes() {
        // One uncompressed strip of 192 MB: image's default limits refused a
        // strip over 512 MiB less twice the image, so a single strip over
        // about 171 MiB.
        let thumb = rgb16_tiff_thumbnail(8000, 4000, 4000);
        assert_eq!(thumb.dimensions(), (512, 256));
    }
}
