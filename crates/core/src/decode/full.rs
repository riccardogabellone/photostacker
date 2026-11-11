//! Full-resolution decoding for the writers, and a header-only probe.
//!
//! - **Stills** (JPEG, PNG, TIFF) go through `image`, from the same decoder
//!   as the thumbnails (so a cut-off JPEG is an error here too). 8-bit
//!   colour types give [`Samples::U8`]; 16-bit and float ones (TIFF)
//!   [`Samples::U16`], floats clamped to 0..=1. CMYK JPEGs come as RGB8 from
//!   `image`'s own conversion. The EXIF orientation is applied.
//! - **HEIC** goes through libheif, which applies the file's own transforms;
//!   the EXIF orientation is ignored, as for the thumbnails (see `heic.rs`).
//! - **RAW** is developed by rawler at full size into [`Samples::U16`] RGB,
//!   in the job's [`RawTone`], and carries our generated ICC profile for that
//!   tone (see `icc.rs`). The EXIF orientation is applied. When rawler fails,
//!   the largest embedded JPEG stands in, as [`Samples::U8`] with its own ICC
//!   (see `raw/`).
//!
//! [`probe`] and [`full`] read the dimensions, the sample layout and the ICC
//! profile the same way, so they agree for the same file, tone and hint.
//! Both keep a profile only when it is an RGB one (see `rgb_icc` in
//! `mod.rs`): a Gray or CMYK source is decoded to RGB, so its own profile
//! does not travel. A JPEG left without one takes the profile of its
//! [`ColourHint`]: [`adobe_rgb_profile`] when its EXIF marks it as DCF
//! Adobe RGB.
//!
//! # Memory
//!
//! The full-size buffers that one [`full`] call holds at once, in bytes per
//! pixel (B/px) of the image; the writers hold one such call at a time:
//! - **Stills** are decoded into one buffer of the decoded colour type, such
//!   as 3 B/px for 8-bit RGB. A JPEG's file is held whole as well, from
//!   before the decode to its end.
//!   - image's TIFF decoder decodes into a buffer of its own, as large as the
//!     image, then copies it out: twice the image during the decode (for a
//!     CMYK TIFF, its 4 or 8 B/px buffer and the 3 or 6 B/px RGB).
//!   - Gray becomes RGB, gray with alpha RGBA, and a float TIFF 16-bit, in a
//!     new buffer built while the decoded one is alive: 1 + 3 B/px for 8-bit
//!     gray, 2 + 6 for 16-bit gray, 2 + 4 and 4 + 8 with alpha, 12 + 6 for
//!     float RGB and 16 + 8 for float RGBA.
//! - **Orientations** 5 to 8 turn the RGB(A) image into a new buffer, so it
//!   is held twice; 2 to 4 flip it in place.
//! - **HEIC**: libheif's decoded RGB(A) plane and our copy of its rows,
//!   built while the plane is alive: twice the image, and libheif's own
//!   decode buffers before that.
//! - **RAW**: about 30 B/px for rawler's develop (see "Full resolution" in
//!   `raw/full.rs`).

use std::path::Path;

use image::metadata::Orientation;
use image::{ColorType, DynamicImage, ImageBuffer, ImageDecoder, ImageFormat, Pixel};

#[cfg(feature = "heic")]
use super::heic;
use super::{
    DecodeError, ImageSource, classify, exif_orientation, guarded, orient, raw, rgb_icc,
    still_decoder,
};
use crate::exif::ExifData;
use crate::icc::adobe_rgb_profile;
use crate::model::{FileKind, RawTone};

/// The message that starts the error of a decoder that panicked.
const INTERNAL_ERROR: &str = "internal error";

/// EXIF ColorSpace values: Adobe RGB, as some cameras write it, and
/// uncalibrated, which DCF qualifies with the Interop index.
const COLOUR_SPACE_ADOBE_RGB: u16 = 2;
const COLOUR_SPACE_UNCALIBRATED: u16 = 0xFFFF;
/// The DCF Interop index of an Adobe RGB file.
const INTEROP_INDEX_ADOBE_RGB: &str = "R03";

/// What a file's metadata says about the colours of a JPEG that embeds no
/// RGB profile of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColourHint {
    /// Nothing: an untagged JPEG stays untagged, which readers take as sRGB.
    None,
    /// The EXIF of a DCF camera marks the JPEG as Adobe RGB (1998): its
    /// pixels get [`adobe_rgb_profile`].
    AdobeRgbDcf,
}

impl ColourHint {
    /// The hint of a file with `exif`: [`ColourHint::AdobeRgbDcf`] for
    /// ColorSpace 2, or ColorSpace 0xFFFF (uncalibrated) with the Interop
    /// index `R03`; [`ColourHint::None`] otherwise.
    pub fn from_exif(exif: Option<&ExifData>) -> Self {
        let Some(exif) = exif else {
            return Self::None;
        };
        let adobe_rgb = match exif.colour_space {
            Some(COLOUR_SPACE_ADOBE_RGB) => true,
            Some(COLOUR_SPACE_UNCALIBRATED) => {
                exif.interop_index.as_deref() == Some(INTEROP_INDEX_ADOBE_RGB)
            }
            _ => false,
        };
        if adobe_rgb {
            Self::AdobeRgbDcf
        } else {
            Self::None
        }
    }
}

/// Interleaved samples, `channels` per pixel, row after row.
#[derive(Debug, Clone, PartialEq)]
pub enum Samples {
    U8(Vec<u8>),
    U16(Vec<u16>),
}

/// A photo at full resolution, upright (its orientation applied), as RGB or
/// RGBA: gray becomes RGB, gray with alpha RGBA.
#[derive(Debug, Clone, PartialEq)]
pub struct FullImage {
    pub width: u32,
    pub height: u32,
    /// 3 (RGB) or 4 (RGBA).
    pub channels: u8,
    pub samples: Samples,
    /// The embedded ICC profile, byte for byte, when it is an RGB profile;
    /// `None` for any other (a Gray or CMYK one) or none. A JPEG without an
    /// RGB profile has the one its [`ColourHint`] gives, if any.
    pub icc: Option<Vec<u8>>,
    pub source: ImageSource,
}

/// What [`full`] would give, read from the headers only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// After orientation, like [`FullImage::width`].
    pub width: u32,
    pub height: u32,
    /// [`full`] gives [`Samples::U16`].
    pub sixteen_bit: bool,
    /// [`full`] gives 4 channels.
    pub has_alpha: bool,
    /// Like [`FullImage::icc`].
    pub icc: Option<Vec<u8>>,
}

/// Decodes `path` at full resolution and turns it upright by its EXIF
/// `orientation` (1..=8; any other value means 1; ignored for HEIC).
///
/// `tone` is the tone curve of a RAW develop; other kinds ignore it. `hint`
/// gives the profile of a JPEG that has no RGB profile of its own; other
/// kinds ignore it. A panic inside a decoder is [`DecodeError::Corrupt`]
/// with an `internal error: ` message.
pub fn full(
    path: &Path,
    kind: FileKind,
    orientation: u16,
    tone: RawTone,
    hint: ColourHint,
) -> Result<FullImage, DecodeError> {
    let mut image = guarded(path, INTERNAL_ERROR, || match kind {
        FileKind::Jpeg => still_full(path, ImageFormat::Jpeg, orientation),
        FileKind::Png => still_full(path, ImageFormat::Png, orientation),
        FileKind::Tiff => still_full(path, ImageFormat::Tiff, orientation),
        #[cfg(feature = "heic")]
        FileKind::Heic => heic::heic_full(path),
        #[cfg(not(feature = "heic"))]
        FileKind::Heic => Err(DecodeError::Unsupported("built without heic".into())),
        FileKind::Raw => raw::raw_full(path, orientation, tone),
    })?;
    image.icc = member_icc(kind, image.icc.take(), hint);
    Ok(image)
}

/// What [`full`] would give for the same arguments, read from the headers
/// without decoding any pixel. `tone` picks a RAW's ICC profile, `hint` an
/// untagged JPEG's.
///
/// It never refuses a file that [`full`] decodes, but it may accept one that
/// [`full`] then refuses: pixel data that is cut off or corrupt, or a TIFF
/// over the decoder's own buffer limit (see `open_decoder`). The writers
/// exclude a member whose full decode fails.
///
/// For a RAW, `probe` and [`full`] decide the embedded-JPEG fallback the
/// same way: the JPEG stands in for both only when the develop cannot be
/// probed, that is when rawler's dummy decode (the headers only) or our
/// `developed_size` fails. That also covers the monochrome, SuperCCD and
/// unsupported-CFA RAWs whose headers are readable. A RAW that probes as
/// rawler's 16-bit develop but whose data then fails to decode is an error
/// from [`full`], never the JPEG.
pub fn probe(
    path: &Path,
    kind: FileKind,
    orientation: u16,
    tone: RawTone,
    hint: ColourHint,
) -> Result<Probe, DecodeError> {
    let mut probe = guarded(path, INTERNAL_ERROR, || match kind {
        FileKind::Jpeg => still_probe(path, ImageFormat::Jpeg, orientation),
        FileKind::Png => still_probe(path, ImageFormat::Png, orientation),
        FileKind::Tiff => still_probe(path, ImageFormat::Tiff, orientation),
        #[cfg(feature = "heic")]
        FileKind::Heic => heic::heic_probe(path),
        #[cfg(not(feature = "heic"))]
        FileKind::Heic => Err(DecodeError::Unsupported("built without heic".into())),
        FileKind::Raw => raw::raw_probe(path, orientation, tone),
    })?;
    probe.icc = member_icc(kind, probe.icc.take(), hint);
    Ok(probe)
}

/// The profile [`probe`] and [`full`] give a file of `kind` that embeds
/// `icc`: its own when it is an RGB one (see `rgb_icc`); else, for a JPEG,
/// the one `hint` gives.
fn member_icc(kind: FileKind, icc: Option<Vec<u8>>, hint: ColourHint) -> Option<Vec<u8>> {
    rgb_icc(icc).or_else(|| {
        let adobe_rgb = kind == FileKind::Jpeg && hint == ColourHint::AdobeRgbDcf;
        adobe_rgb.then(adobe_rgb_profile)
    })
}

fn still_full(
    path: &Path,
    format: ImageFormat,
    orientation: u16,
) -> Result<FullImage, DecodeError> {
    let mut decoder = still_decoder(path, format)?;
    let icc = decoder.icc_profile().map_err(classify)?;
    let color = decoder.color_type();
    let decoded = DynamicImage::from_decoder(decoder).map_err(classify)?;
    // Each conversion is a move when the decoded type already matches.
    Ok(match (sixteen_bit(color), color.has_alpha()) {
        (false, false) => upright(decoded.into_rgb8(), orientation, Samples::U8, icc),
        (false, true) => upright(decoded.into_rgba8(), orientation, Samples::U8, icc),
        (true, false) => upright(decoded.into_rgb16(), orientation, Samples::U16, icc),
        (true, true) => upright(decoded.into_rgba16(), orientation, Samples::U16, icc),
    })
}

fn still_probe(path: &Path, format: ImageFormat, orientation: u16) -> Result<Probe, DecodeError> {
    let mut decoder = still_decoder(path, format)?;
    let icc = decoder.icc_profile().map_err(classify)?;
    let color = decoder.color_type();
    let (width, height) = decoder.dimensions();
    let (width, height) = upright_size(width, height, orientation);
    Ok(Probe {
        width,
        height,
        sixteen_bit: sixteen_bit(color),
        has_alpha: color.has_alpha(),
        icc,
    })
}

/// 16 bits per sample, or a float.
fn sixteen_bit(color: ColorType) -> bool {
    color.bytes_per_pixel() / color.channel_count() > 1
}

/// The size of a `width × height` image once `orientation` has turned it
/// upright: width and height swap when it turns the image a quarter.
pub(super) fn upright_size(width: u32, height: u32, orientation: u16) -> (u32, u32) {
    let quarter_turn = matches!(
        exif_orientation(orientation),
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    );
    if quarter_turn {
        (height, width)
    } else {
        (width, height)
    }
}

/// `image` turned upright, as a [`FullImage`] whose samples `wrap` makes.
fn upright<P>(
    image: ImageBuffer<P, Vec<P::Subpixel>>,
    orientation: u16,
    wrap: fn(Vec<P::Subpixel>) -> Samples,
    icc: Option<Vec<u8>>,
) -> FullImage
where
    P: Pixel + 'static,
{
    let image = orient(image, orientation);
    FullImage {
        width: image.width(),
        height: image.height(),
        channels: P::CHANNEL_COUNT,
        samples: wrap(image.into_raw()),
        icc,
        source: ImageSource::Decoded,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use image::ImageEncoder;
    use image::codecs::png::PngEncoder;
    use image::{
        DynamicImage, GrayAlphaImage, GrayImage, ImageBuffer, Luma, LumaA, Rgb, RgbImage, Rgba,
    };
    use photostacker_testkit::{
        TiffSpec, Value, cmyk_jpeg_with_icc, icc_stub, jpeg_with_exif, jpeg_with_icc,
        raf_with_jpeg, scene, tiff_raw_with_jpegs,
    };

    use super::*;
    use crate::exif::read_exif;

    const RED: Rgb<u8> = Rgb([255, 0, 0]);

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    fn save(dir: &Path, name: &str, image: impl Into<DynamicImage>) -> PathBuf {
        let path = dir.join(name);
        image.into().save(&path).unwrap();
        path
    }

    fn full_of(path: &Path, kind: FileKind, orientation: u16) -> FullImage {
        full(path, kind, orientation, RawTone::Srgb, ColourHint::None)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    fn u8s(image: &FullImage) -> &[u8] {
        match &image.samples {
            Samples::U8(samples) => samples,
            Samples::U16(_) => panic!("16-bit samples, expected 8-bit"),
        }
    }

    fn u16s(image: &FullImage) -> &[u16] {
        match &image.samples {
            Samples::U16(samples) => samples,
            Samples::U8(_) => panic!("8-bit samples, expected 16-bit"),
        }
    }

    /// The samples of the pixel at (`x`, `y`).
    fn pixel<T>(image: &FullImage, samples: &[T], x: u32, y: u32) -> Vec<T>
    where
        T: Copy,
    {
        let channels = usize::from(image.channels);
        let at = (y as usize * image.width as usize + x as usize) * channels;
        samples[at..at + channels].to_vec()
    }

    fn near<T: Copy + Into<i64>>(got: &[T], want: &[i64], tolerance: i64) -> bool {
        got.len() == want.len()
            && got
                .iter()
                .zip(want)
                .all(|(&a, &b)| (a.into() - b).abs() <= tolerance)
    }

    /// Every sample equals `want` repeated, pixel after pixel.
    fn all_pixels<T: Copy + PartialEq + std::fmt::Debug>(samples: &[T], want: &[T]) {
        assert_eq!(samples.len() % want.len(), 0);
        for (i, got) in samples.chunks_exact(want.len()).enumerate() {
            assert_eq!(got, want, "pixel {i}");
        }
    }

    /// A 40×20 JPEG with EXIF Orientation 6: black, with a red 10×10 block at
    /// its stored top-left corner.
    fn oriented_jpeg(dir: &Path) -> PathBuf {
        let mut img = RgbImage::new(40, 20);
        for y in 0..10 {
            for x in 0..10 {
                img.put_pixel(x, y, RED);
            }
        }
        let tiff = TiffSpec {
            little_endian: true,
            ifd0: vec![(0x0112, Value::Short(vec![6]))],
            exif: Vec::new(),
            maker_note: None,
        };
        write(dir, "turned.jpg", &jpeg_with_exif(&img, &tiff.build(), 95))
    }

    /// A dummy RGB display profile: the decoders return its bytes without
    /// parsing them, and its header passes `rgb_icc`.
    fn dummy_icc() -> Vec<u8> {
        icc_stub(b"mntr", b"RGB ")
    }

    /// A 16×8 8-bit gray PNG whose iCCP chunk carries `icc`.
    fn gray_png_with_icc(dir: &Path, name: &str, icc: &[u8]) -> PathBuf {
        let mut bytes = Vec::new();
        let mut encoder = PngEncoder::new(&mut bytes);
        encoder.set_icc_profile(icc.to_vec()).unwrap();
        let gray = GrayImage::from_pixel(16, 8, Luma([0x60]));
        encoder
            .write_image(gray.as_raw(), 16, 8, image::ExtendedColorType::L8)
            .unwrap();
        write(dir, name, &bytes)
    }

    #[test]
    fn full_jpeg_is_rgb8_with_icc() {
        let dir = tempfile::tempdir().unwrap();
        let icc = dummy_icc();
        let flat = RgbImage::from_pixel(32, 16, Rgb([200, 100, 50]));
        let path = write(dir.path(), "icc.jpg", &jpeg_with_icc(&flat, &icc, 95));
        let image = full_of(&path, FileKind::Jpeg, 1);
        assert_eq!((image.width, image.height, image.channels), (32, 16, 3));
        assert_eq!(image.icc.as_deref(), Some(&icc[..]));
        assert_eq!(image.source, ImageSource::Decoded);
        let samples = u8s(&image);
        assert_eq!(samples.len(), 32 * 16 * 3);
        assert!(near(&pixel(&image, samples, 16, 8), &[200, 100, 50], 3));

        let plain = save(dir.path(), "plain.jpg", flat);
        assert_eq!(full_of(&plain, FileKind::Jpeg, 1).icc, None);
    }

    #[test]
    fn full_png16_stays_sixteen_bit() {
        let dir = tempfile::tempdir().unwrap();
        let want = [0x12FF_u16, 0xAB80, 0x00FF];
        let deep = ImageBuffer::from_pixel(3, 2, Rgb(want));
        for (name, kind) in [("deep.png", FileKind::Png), ("deep.tif", FileKind::Tiff)] {
            let path = save(dir.path(), name, DynamicImage::ImageRgb16(deep.clone()));
            let image = full_of(&path, kind, 1);
            assert_eq!(
                (image.width, image.height, image.channels),
                (3, 2, 3),
                "{name}"
            );
            all_pixels(u16s(&image), &want);
        }
    }

    #[test]
    fn full_float_tiff_becomes_sixteen_bit() {
        let dir = tempfile::tempdir().unwrap();
        let float = ImageBuffer::from_pixel(2, 2, Rgb([0.0_f32, 0.5, 1.0]));
        let path = save(dir.path(), "float.tif", DynamicImage::ImageRgb32F(float));
        let image = full_of(&path, FileKind::Tiff, 1);
        assert_eq!(image.channels, 3);
        assert_eq!(u16s(&image).len(), 2 * 2 * 3);
        for got in u16s(&image).as_chunks::<3>().0 {
            assert!(near(got.as_slice(), &[0, 32768, 65535], 1), "{got:?}");
        }
    }

    #[test]
    fn full_gray_becomes_rgb() {
        let dir = tempfile::tempdir().unwrap();

        let path = save(
            dir.path(),
            "g8.png",
            GrayImage::from_pixel(2, 2, Luma([0x9A])),
        );
        let image = full_of(&path, FileKind::Png, 1);
        assert_eq!(image.channels, 3);
        all_pixels(u8s(&image), &[0x9A, 0x9A, 0x9A]);

        let gray16 = ImageBuffer::from_pixel(2, 2, Luma([0x9A12_u16]));
        let path = save(dir.path(), "g16.png", DynamicImage::ImageLuma16(gray16));
        let image = full_of(&path, FileKind::Png, 1);
        assert_eq!(image.channels, 3);
        all_pixels(u16s(&image), &[0x9A12, 0x9A12, 0x9A12]);

        let gray_alpha = GrayAlphaImage::from_pixel(2, 2, LumaA([0x40, 0x80]));
        let path = save(dir.path(), "ga8.png", gray_alpha);
        let image = full_of(&path, FileKind::Png, 1);
        assert_eq!(image.channels, 4);
        all_pixels(u8s(&image), &[0x40, 0x40, 0x40, 0x80]);

        let gray_alpha16 = ImageBuffer::from_pixel(2, 2, LumaA([0x4001_u16, 0x8002]));
        let path = save(
            dir.path(),
            "ga16.png",
            DynamicImage::ImageLumaA16(gray_alpha16),
        );
        let image = full_of(&path, FileKind::Png, 1);
        assert_eq!(image.channels, 4);
        all_pixels(u16s(&image), &[0x4001, 0x4001, 0x4001, 0x8002]);
    }

    #[test]
    fn full_rgba_png_keeps_alpha() {
        let dir = tempfile::tempdir().unwrap();

        let rgba = ImageBuffer::from_pixel(2, 2, Rgba([7_u8, 8, 9, 0]));
        let path = save(dir.path(), "a8.png", DynamicImage::ImageRgba8(rgba));
        let image = full_of(&path, FileKind::Png, 1);
        assert_eq!(image.channels, 4);
        all_pixels(u8s(&image), &[7, 8, 9, 0]);

        let want = [0x40FF_u16, 0x8000, 0xC000, 0x1234];
        let rgba16 = ImageBuffer::from_pixel(2, 2, Rgba(want));
        let path = save(dir.path(), "a16.png", DynamicImage::ImageRgba16(rgba16));
        let image = full_of(&path, FileKind::Png, 1);
        assert_eq!(image.channels, 4);
        all_pixels(u16s(&image), &want);
    }

    #[test]
    fn full_decode_applies_orientation() {
        let dir = tempfile::tempdir().unwrap();
        let path = oriented_jpeg(dir.path());
        let orientation = read_exif(&path, FileKind::Jpeg)
            .unwrap()
            .unwrap()
            .orientation;
        assert_eq!(orientation, 6);
        let image = full_of(&path, FileKind::Jpeg, orientation);
        assert_eq!((image.width, image.height), (20, 40));
        // 90° clockwise: the stored top-left block is now at the top-right.
        let samples = u8s(&image);
        assert!(near(&pixel(&image, samples, 15, 5), &[255, 0, 0], 40));
        assert!(near(&pixel(&image, samples, 4, 5), &[0, 0, 0], 40));
        assert!(near(&pixel(&image, samples, 15, 35), &[0, 0, 0], 40));
    }

    #[test]
    fn every_orientation_turns_sixteen_bit_rgba_too() {
        // Pillow 12.2.0: ImageOps.exif_transpose of a 4×2 image with a marked
        // top-left pixel, per Orientation tag (as in the thumbnail tests).
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
        let marker = [0xFFFF_u16, 0x1234, 0x0001, 0x8000];
        let mut img = ImageBuffer::from_pixel(4, 2, Rgba([0_u16, 0, 0, 0xFFFF]));
        img.put_pixel(0, 0, Rgba(marker));
        let dir = tempfile::tempdir().unwrap();
        let path = save(dir.path(), "mark.png", DynamicImage::ImageRgba16(img));
        for (orientation, size, at) in cases {
            let image = full_of(&path, FileKind::Png, orientation);
            assert_eq!(
                (image.width, image.height),
                size,
                "orientation {orientation}"
            );
            let samples = u16s(&image);
            let marked: Vec<(u32, u32)> = (0..image.height)
                .flat_map(|y| (0..image.width).map(move |x| (x, y)))
                .filter(|&(x, y)| pixel(&image, samples, x, y) == marker)
                .collect();
            assert_eq!(marked, [at], "orientation {orientation}");
        }
    }

    #[test]
    fn probe_matches_full_dimensions() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let rgb = DynamicImage::ImageRgb8(scene(3, 24, 16));
        let icc_jpg = write(
            d,
            "icc.jpg",
            &jpeg_with_icc(&rgb.to_rgb8(), &dummy_icc(), 90),
        );
        let turned_jpg = oriented_jpeg(d);
        let rgb_png = save(d, "rgb.png", rgb.clone());
        let gray_png = save(d, "gray.png", rgb.to_luma8());
        let gray_alpha_png = save(d, "gray_alpha.png", rgb.to_luma_alpha8());
        let gray_alpha16_png = save(d, "gray_alpha16.png", rgb.to_luma_alpha16());
        let rgba_png = save(d, "rgba.png", rgb.to_rgba8());
        let deep_png = save(d, "deep.png", rgb.to_rgba16());
        let rgb_tif = save(d, "rgb.tif", rgb.clone());
        let deep_tif = save(d, "deep.tif", rgb.to_rgb16());
        let float_tif = save(d, "float.tif", rgb.to_rgb32f());
        let stills = [
            (icc_jpg, FileKind::Jpeg, 1, (24, 16)),
            (turned_jpg, FileKind::Jpeg, 6, (20, 40)),
            (rgb_png, FileKind::Png, 8, (16, 24)),
            (gray_png, FileKind::Png, 5, (16, 24)),
            (gray_alpha_png, FileKind::Png, 1, (24, 16)),
            (gray_alpha16_png, FileKind::Png, 6, (16, 24)),
            (rgba_png, FileKind::Png, 4, (24, 16)),
            (deep_png, FileKind::Png, 3, (24, 16)),
            (rgb_tif, FileKind::Tiff, 7, (16, 24)),
            (deep_tif, FileKind::Tiff, 1, (24, 16)),
            (float_tif, FileKind::Tiff, 2, (24, 16)),
        ];
        #[cfg(feature = "heic")]
        let heic = {
            let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/heic");
            [
                (dir.join("sample.heic"), FileKind::Heic, 6, (384, 640)),
                (dir.join("deep-no-exif.heic"), FileKind::Heic, 1, (64, 48)),
            ]
        };
        #[cfg(not(feature = "heic"))]
        let heic = [];
        for (path, kind, orientation, size) in stills.into_iter().chain(heic) {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let probed = probe(&path, kind, orientation, RawTone::Srgb, ColourHint::None)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            let image = full_of(&path, kind, orientation);
            assert_eq!((probed.width, probed.height), size, "{name}");
            assert_eq!((image.width, image.height), size, "{name}");
            let sixteen_bit = matches!(image.samples, Samples::U16(_));
            assert_eq!(probed.sixteen_bit, sixteen_bit, "{name}");
            assert_eq!(probed.has_alpha, image.channels == 4, "{name}");
            assert_eq!(probed.icc, image.icc, "{name}");
        }
    }

    #[test]
    fn only_an_rgb_profile_travels() {
        // Gray and CMYK sources decode to RGB pixels, which their own Gray or
        // CMYK profile cannot describe: probe and full both drop it, for a
        // still and for a RAW's embedded JPEG alike.
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let gray_icc = icc_stub(b"mntr", b"GRAY");
        let cmyk_icc = icc_stub(b"prtr", b"CMYK");
        let gray_png = gray_png_with_icc(d, "gray.png", &gray_icc);
        let cmyk = cmyk_jpeg_with_icc(16, 8, [0, 128, 255, 64], &cmyk_icc);
        let cmyk_jpg = write(d, "cmyk.jpg", &cmyk);
        let gray_profiled = jpeg_with_icc(&scene(4, 64, 32), &gray_icc, 90);
        let raf = write(d, "unknown.raf", &raf_with_jpeg(&gray_profiled));
        let cases = [
            (&gray_png, FileKind::Png, &gray_icc, ImageSource::Decoded),
            (&cmyk_jpg, FileKind::Jpeg, &cmyk_icc, ImageSource::Decoded),
            (&raf, FileKind::Raw, &gray_icc, ImageSource::EmbeddedPreview),
        ];
        for (path, kind, icc, source) in cases {
            let name = path.file_name().unwrap().to_string_lossy();
            // Precondition: the decoder sees the profile.
            if kind != FileKind::Raw {
                let format = ImageFormat::from_path(path).unwrap();
                let mut decoder = still_decoder(path, format).unwrap();
                let seen = decoder.icc_profile().unwrap();
                assert_eq!(seen.as_ref(), Some(icc), "{name}");
            }
            let probed = probe(path, kind, 1, RawTone::Srgb, ColourHint::None)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            let image = full_of(path, kind, 1);
            assert_eq!(image.source, source, "{name}");
            assert_eq!(image.channels, 3, "{name}");
            assert_eq!(probed.icc, None, "{name}");
            assert_eq!(image.icc, None, "{name}");
        }
    }

    #[test]
    fn colour_hint_marks_adobe_rgb_dcf_exif() {
        let exif = |colour_space, interop_index: Option<&str>| ExifData {
            colour_space,
            interop_index: interop_index.map(String::from),
            ..ExifData::default()
        };
        let cases = [
            (exif(Some(2), None), ColourHint::AdobeRgbDcf),
            (exif(Some(2), Some("R98")), ColourHint::AdobeRgbDcf),
            (exif(Some(0xFFFF), Some("R03")), ColourHint::AdobeRgbDcf),
            (exif(Some(0xFFFF), Some("R98")), ColourHint::None),
            (exif(Some(0xFFFF), None), ColourHint::None),
            (exif(Some(1), Some("R03")), ColourHint::None),
            (exif(None, Some("R03")), ColourHint::None),
            (ExifData::default(), ColourHint::None),
        ];
        for (exif, hint) in cases {
            assert_eq!(ColourHint::from_exif(Some(&exif)), hint, "{exif:?}");
        }
        assert_eq!(ColourHint::from_exif(None), ColourHint::None);
    }

    #[test]
    fn adobe_rgb_hint_tags_only_an_untagged_jpeg() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let flat = RgbImage::from_pixel(16, 8, Rgb([200, 100, 50]));
        let plain_jpg = save(d, "plain.jpg", flat.clone());
        let icc_jpg = write(d, "icc.jpg", &jpeg_with_icc(&flat, &dummy_icc(), 95));
        let gray_icc = icc_stub(b"mntr", b"GRAY");
        let gray_jpg = write(d, "gray.jpg", &jpeg_with_icc(&flat, &gray_icc, 95));
        let png = save(d, "plain.png", flat);
        let adobe = Some(adobe_rgb_profile());
        let cases = [
            (
                &plain_jpg,
                FileKind::Jpeg,
                ColourHint::AdobeRgbDcf,
                adobe.clone(),
            ),
            (&plain_jpg, FileKind::Jpeg, ColourHint::None, None),
            // A JPEG's own RGB profile wins; a Gray one does not count.
            (
                &icc_jpg,
                FileKind::Jpeg,
                ColourHint::AdobeRgbDcf,
                Some(dummy_icc()),
            ),
            (&gray_jpg, FileKind::Jpeg, ColourHint::AdobeRgbDcf, adobe),
            // DCF is a JPEG rule.
            (&png, FileKind::Png, ColourHint::AdobeRgbDcf, None),
        ];
        for (path, kind, hint, want) in cases {
            let name = path.file_name().unwrap().to_string_lossy();
            let probed = probe(path, kind, 1, RawTone::Srgb, hint).unwrap();
            let image = full(path, kind, 1, RawTone::Srgb, hint).unwrap();
            assert_eq!(probed.icc, want, "{name} {hint:?}");
            assert_eq!(image.icc, want, "{name} {hint:?}");
        }
    }

    #[test]
    fn probe_reads_no_pixels() {
        // A PNG cut inside its image data still has every header chunk.
        let dir = tempfile::tempdir().unwrap();
        let path = save(dir.path(), "cut.png", scene(5, 256, 128));
        let bytes = fs::read(&path).unwrap();
        fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();
        let probed = probe(&path, FileKind::Png, 6, RawTone::Srgb, ColourHint::None).unwrap();
        assert_eq!((probed.width, probed.height), (128, 256));
        assert!(!probed.sixteen_bit && !probed.has_alpha);
        let err = full(&path, FileKind::Png, 6, RawTone::Srgb, ColourHint::None).unwrap_err();
        assert!(matches!(err, DecodeError::Corrupt(_)), "{err:?}");
    }

    #[test]
    fn corrupt_still_full_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        for (name, kind) in [
            ("whole.jpg", FileKind::Jpeg),
            ("whole.png", FileKind::Png),
            ("whole.tif", FileKind::Tiff),
        ] {
            let path = save(dir.path(), name, scene(9, 256, 256));
            let bytes = fs::read(&path).unwrap();
            fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();
            let err = full(&path, kind, 1, RawTone::Srgb, ColourHint::None).unwrap_err();
            assert!(matches!(err, DecodeError::Corrupt(_)), "{name}: {err:?}");
        }
        // A cut-off JPEG fails before any pixel is read, as in the thumbnails.
        let cut = dir.path().join("whole.jpg");
        let err = probe(&cut, FileKind::Jpeg, 1, RawTone::Srgb, ColourHint::None).unwrap_err();
        assert!(matches!(err, DecodeError::Corrupt(_)), "{err:?}");

        let garbage = write(dir.path(), "garbage.jpg", b"this is not a photo at all");
        let err = full(&garbage, FileKind::Jpeg, 1, RawTone::Srgb, ColourHint::None).unwrap_err();
        assert!(matches!(err, DecodeError::Corrupt(_)), "{err:?}");
        let err = probe(&garbage, FileKind::Jpeg, 1, RawTone::Srgb, ColourHint::None).unwrap_err();
        assert!(matches!(err, DecodeError::Corrupt(_)), "{err:?}");
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone.jpg");
        for kind in [FileKind::Jpeg, FileKind::Png, FileKind::Tiff] {
            let err = full(&gone, kind, 1, RawTone::Srgb, ColourHint::None).unwrap_err();
            assert!(matches!(err, DecodeError::Io(_)), "{kind:?}: {err:?}");
            let err = probe(&gone, kind, 1, RawTone::Srgb, ColourHint::None).unwrap_err();
            assert!(matches!(err, DecodeError::Io(_)), "{kind:?}: {err:?}");
        }
    }

    #[test]
    fn raw_kind_falls_back_to_its_jpeg_never_to_a_still_decode() {
        let dir = tempfile::tempdir().unwrap();
        // rawler knows no such body, so its largest JPEG (1024×683) stands in.
        let nef = write(
            dir.path(),
            "unknown.nef",
            &tiff_raw_with_jpegs(&[(160, 120), (1024, 683)]),
        );
        let image = full(&nef, FileKind::Raw, 6, RawTone::Linear, ColourHint::None).unwrap();
        assert_eq!((image.width, image.height, image.channels), (683, 1024, 3));
        assert_eq!(image.source, ImageSource::EmbeddedPreview);
        let probed = probe(&nef, FileKind::Raw, 6, RawTone::Linear, ColourHint::None).unwrap();
        assert_eq!((probed.width, probed.height), (683, 1024));
        assert!(!probed.sixteen_bit && !probed.has_alpha);

        // A PNG named as a RAW is no RAW: rawler's error, not a still decode.
        let png = save(dir.path(), "a.png", scene(1, 8, 8));
        let err = full(&png, FileKind::Raw, 1, RawTone::Srgb, ColourHint::None).unwrap_err();
        assert!(matches!(err, DecodeError::Unsupported(_)), "{err:?}");
        let err = probe(&png, FileKind::Raw, 1, RawTone::Srgb, ColourHint::None).unwrap_err();
        assert!(matches!(err, DecodeError::Unsupported(_)), "{err:?}");
    }

    #[cfg(not(feature = "heic"))]
    #[test]
    fn heic_full_without_feature_is_unsupported() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/heic/sample.heic");
        let unsupported = |err: DecodeError| match err {
            DecodeError::Unsupported(why) => why == "built without heic",
            _ => false,
        };
        assert!(unsupported(
            full(&path, FileKind::Heic, 1, RawTone::Srgb, ColourHint::None).unwrap_err()
        ));
        assert!(unsupported(
            probe(&path, FileKind::Heic, 1, RawTone::Srgb, ColourHint::None).unwrap_err()
        ));
    }

    #[cfg(feature = "heic")]
    #[test]
    fn heic_full_is_upright_and_ten_bit_becomes_sixteen() {
        let heic = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/heic");

        // Stored 640×384 with `irot`: red top-right and green bottom-right once
        // upright. The EXIF Orientation (6 in the file) must not turn it again.
        let path = heic.join("sample.heic");
        let image = full_of(&path, FileKind::Heic, 6);
        assert_eq!(image, full_of(&path, FileKind::Heic, 1));
        assert_eq!((image.width, image.height, image.channels), (384, 640, 3));
        assert_eq!(image.source, ImageSource::Decoded);
        let samples = u8s(&image);
        let (w, h) = (image.width, image.height);
        assert!(near(&pixel(&image, samples, w - 10, 10), &[255, 0, 0], 24));
        assert!(near(
            &pixel(&image, samples, w - 10, h - 10),
            &[0, 255, 0],
            24
        ));

        // 64×48 behind a `clap` crop, 10-bit: red ramps along x, green along
        // y, blue is half way.
        let path = heic.join("deep-no-exif.heic");
        let image = full_of(&path, FileKind::Heic, 1);
        assert_eq!((image.width, image.height, image.channels), (64, 48, 3));
        let samples = u16s(&image);
        let tolerance = 12 * 257;
        assert!(near(
            &pixel(&image, samples, 0, 0),
            &[0, 0, 0x8020],
            tolerance
        ));
        assert!(near(
            &pixel(&image, samples, 63, 0),
            &[0xFFFF, 0, 0x8020],
            tolerance
        ));
        assert!(near(
            &pixel(&image, samples, 0, 47),
            &[0, 0xFFFF, 0x8020],
            tolerance
        ));
        // 10 bits widened by replication: the low 6 bits repeat the top 6.
        assert!(
            samples.iter().all(|&v| v & 0x3F == v >> 10),
            "not bit-replicated"
        );
    }
}
