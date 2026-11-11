//! What a thumbnail decode, or an EXIF read, is charged against the
//! analysis' memory budget (spec §4.3), estimated from the file's header and
//! length alone.

use std::fs::{self, File};
use std::io::{self, BufReader, Read};
use std::path::Path;

use image::{ImageDecoder, ImageFormat};

#[cfg(feature = "heic")]
use super::heic;
use super::still_decoder;
use crate::model::FileKind;

/// The charge of a decode whose size is unknown: a RAW besides its file's
/// bytes (a develop measured with the file mapped peaked at about 160–210
/// MB; a preview needs less), a HEIC without the `heic` feature, or a file
/// whose header cannot be read.
const DEFAULT_PEAK: u64 = 256 << 20;

/// How many decoded images a still's thumbnail decode is charged: about what
/// it holds at its peak, such as image's TIFF buffer, the decoded image and
/// its RGB8 copy.
const DECODED_COPIES: u64 = 3;

/// The bytes a [`thumbnail`](super::thumbnail) decode of `path`, read as
/// `kind`, is charged:
/// - JPEG, PNG and TIFF: `width × height × channels × bytes_per_sample × 3`,
///   from the header;
/// - HEIC: `width × height × 4 × 3`, from libheif's handle;
/// - RAW: [`DEFAULT_PEAK`] plus the file's length, since rawler reads the
///   whole file into memory and keeps it through the decode (0 when the
///   length cannot be read);
/// - any other header that cannot be read: [`DEFAULT_PEAK`].
///
/// A still too large for `image`'s limits (see `open_decoder`) has an
/// unreadable header here; its decode is refused before anything is
/// allocated, so the default is charged.
pub(crate) fn thumbnail_peak(path: &Path, kind: FileKind) -> u64 {
    let decoded = match kind {
        FileKind::Jpeg => jpeg_decoded_size(path),
        FileKind::Png => still_decoded_size(path, ImageFormat::Png),
        FileKind::Tiff => still_decoded_size(path, ImageFormat::Tiff),
        #[cfg(feature = "heic")]
        FileKind::Heic => heic::heic_probe(path)
            .ok()
            .map(|probe| pixels(probe.width, probe.height).saturating_mul(4)),
        #[cfg(not(feature = "heic"))]
        FileKind::Heic => None,
        FileKind::Raw => return DEFAULT_PEAK.saturating_add(file_len(path)),
    };
    decoded.map_or(DEFAULT_PEAK, |bytes| bytes.saturating_mul(DECODED_COPIES))
}

/// The bytes an EXIF read of `path`, read as `kind`, is charged: the file's
/// length for a RAW or a TIFF, whose EXIF reader takes the whole file into
/// memory (`exif::container::read_file`), else 0. Some RAWs need less (the
/// EXIF of a RAF is a small copied block, and some containers have no EXIF
/// reader); the charge is kept simple. A file whose length cannot be read
/// is charged 0, and its read fails on its own I/O error.
pub(crate) fn exif_read_peak(path: &Path, kind: FileKind) -> u64 {
    match kind {
        FileKind::Raw | FileKind::Tiff => file_len(path),
        FileKind::Jpeg | FileKind::Png | FileKind::Heic => 0,
    }
}

/// The length of the file at `path`; 0 when it cannot be read.
fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |metadata| metadata.len())
}

fn pixels(width: u32, height: u32) -> u64 {
    u64::from(width) * u64::from(height)
}

/// The decoded size of a PNG or TIFF, from the decoder's header read.
fn still_decoded_size(path: &Path, format: ImageFormat) -> Option<u64> {
    still_decoder(path, format)
        .ok()
        .map(|decoder| decoder.total_bytes())
}

/// The decoded size of a JPEG, from its first frame header. image's own
/// decoder would read the whole file for it.
fn jpeg_decoded_size(path: &Path) -> Option<u64> {
    let mut reader = BufReader::new(File::open(path).ok()?);
    let frame = jpeg_frame(&mut reader).ok()??;
    let bytes_per_sample = if frame.precision > 8 { 2 } else { 1 };
    Some(pixels(frame.width, frame.height) * u64::from(frame.components) * bytes_per_sample)
}

/// The fields of a JPEG frame header (SOFn) that size its decoded image.
#[derive(Debug, PartialEq, Eq)]
struct Frame {
    width: u32,
    height: u32,
    components: u8,
    precision: u8,
}

/// Reads the marker segments of a JPEG up to its first frame header, of any
/// kind, skipping the other segments' data. Like the completeness check in
/// `mod.rs`, stray bytes and fill bytes before a marker code are skipped.
/// `None` when the data does not start with SOI, when a scan, EOI or a bad
/// length comes first, or when a side is 0 (a height set by a later DNL).
fn jpeg_frame(reader: &mut BufReader<File>) -> io::Result<Option<Frame>> {
    const FILL: u8 = 0xFF;
    let mut two = [0; 2];
    reader.read_exact(&mut two)?;
    if two != [FILL, 0xD8] {
        return Ok(None);
    }
    let mut byte = [0; 1];
    loop {
        reader.read_exact(&mut byte)?;
        while byte[0] != FILL {
            reader.read_exact(&mut byte)?;
        }
        while byte[0] == FILL {
            reader.read_exact(&mut byte)?;
        }
        let marker = byte[0];
        match marker {
            // A stuffed FF 00 or a standalone marker: no length follows.
            0x00..=0x01 | 0xD0..=0xD7 => continue,
            // Another SOI, EOI or SOS: no frame header first.
            0xD8..=0xDA => return Ok(None),
            _ => {}
        }
        reader.read_exact(&mut two)?;
        let len = u16::from_be_bytes(two);
        if len < 2 {
            return Ok(None);
        }
        // SOF0..SOF15 but DHT (C4), JPG (C8) and DAC (CC).
        if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
            let mut frame = [0; 6];
            reader.read_exact(&mut frame)?;
            let height = u32::from(u16::from_be_bytes([frame[1], frame[2]]));
            let width = u32::from(u16::from_be_bytes([frame[3], frame[4]]));
            return Ok((width > 0 && height > 0).then_some(Frame {
                width,
                height,
                components: frame[5],
                precision: frame[0],
            }));
        }
        reader.seek_relative(i64::from(len) - 2)?;
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use image::{DynamicImage, GrayImage, ImageBuffer, Rgb, Rgba};
    use photostacker_testkit::{
        TiffSpec, Value, cmyk_jpeg_with_icc, icc_stub, jpeg_with_exif, scene, tiff_raw_with_jpegs,
    };

    use super::*;

    fn save(dir: &Path, name: &str, image: impl Into<DynamicImage>) -> PathBuf {
        let path = dir.join(name);
        image.into().save(&path).unwrap();
        path
    }

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn still_peaks_are_three_decoded_images_from_the_header() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let exif = TiffSpec {
            little_endian: true,
            ifd0: vec![(0x0112, Value::Short(vec![6]))],
            exif: Vec::new(),
            maker_note: None,
        };
        let cases = [
            (
                save(d, "rgb.jpg", scene(1, 64, 48)),
                FileKind::Jpeg,
                64 * 48 * 3,
            ),
            (
                save(d, "gray.jpg", GrayImage::new(40, 30)),
                FileKind::Jpeg,
                40 * 30,
            ),
            // An APP1 segment comes before the frame header.
            (
                write(
                    d,
                    "exif.jpg",
                    &jpeg_with_exif(&scene(2, 32, 16), &exif.build(), 90),
                ),
                FileKind::Jpeg,
                32 * 16 * 3,
            ),
            (
                write(
                    d,
                    "cmyk.jpg",
                    &cmyk_jpeg_with_icc(16, 8, [0, 128, 255, 64], &icc_stub(b"prtr", b"CMYK")),
                ),
                FileKind::Jpeg,
                16 * 8 * 4,
            ),
            (
                save(d, "rgb.png", scene(3, 20, 10)),
                FileKind::Png,
                20 * 10 * 3,
            ),
            (
                save(
                    d,
                    "rgba16.png",
                    DynamicImage::ImageRgba16(ImageBuffer::from_pixel(10, 6, Rgba([1_u16; 4]))),
                ),
                FileKind::Png,
                10 * 6 * 8,
            ),
            (
                save(
                    d,
                    "rgb16.tif",
                    DynamicImage::ImageRgb16(ImageBuffer::from_pixel(10, 6, Rgb([1_u16; 3]))),
                ),
                FileKind::Tiff,
                10 * 6 * 6,
            ),
        ];
        for (path, kind, decoded) in cases {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            assert_eq!(thumbnail_peak(&path, kind), decoded * 3, "{name}");
        }
    }

    #[test]
    fn unknown_sizes_cost_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let jpeg = fs::read(save(d, "whole.jpg", scene(4, 64, 48))).unwrap();
        let sof = jpeg.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
        let cut = write(d, "cut.jpg", &jpeg[..sof]);
        let garbage = write(d, "garbage.jpg", b"this is not a photo at all");
        let gone = d.join("gone.tif");
        for (path, kind) in [
            (&cut, FileKind::Jpeg),
            (&garbage, FileKind::Jpeg),
            (&garbage, FileKind::Png),
            (&garbage, FileKind::Tiff),
            (&gone, FileKind::Tiff),
        ] {
            assert_eq!(
                thumbnail_peak(path, kind),
                DEFAULT_PEAK,
                "{path:?} as {kind:?}"
            );
        }
    }

    #[test]
    fn a_raw_is_charged_its_file_too() {
        // The whole file sits on the heap during its decode.
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let raw = write(d, "x.nef", &tiff_raw_with_jpegs(&[(160, 120), (640, 427)]));
        let len = fs::metadata(&raw).unwrap().len();
        assert_eq!(thumbnail_peak(&raw, FileKind::Raw), DEFAULT_PEAK + len);
        // A RAW is never sized from its header, whatever it holds.
        let png = save(d, "a.png", scene(5, 8, 8));
        let len = fs::metadata(&png).unwrap().len();
        assert_eq!(thumbnail_peak(&png, FileKind::Raw), DEFAULT_PEAK + len);
        // A file whose length cannot be read is charged the default alone.
        assert_eq!(
            thumbnail_peak(&d.join("gone.cr3"), FileKind::Raw),
            DEFAULT_PEAK
        );
    }

    #[test]
    fn exif_reads_charge_whole_file_kinds_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "x.bin", &[7; 1000]);
        for (kind, charged) in [
            (FileKind::Raw, 1000),
            (FileKind::Tiff, 1000),
            (FileKind::Jpeg, 0),
            (FileKind::Png, 0),
            (FileKind::Heic, 0),
        ] {
            assert_eq!(exif_read_peak(&path, kind), charged, "{kind:?}");
        }
        let gone = dir.path().join("gone.tif");
        assert_eq!(exif_read_peak(&gone, FileKind::Tiff), 0);
    }

    #[test]
    fn heic_peak_is_four_bytes_a_pixel_or_the_default() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/heic/sample.heic");
        #[cfg(feature = "heic")]
        assert_eq!(thumbnail_peak(&path, FileKind::Heic), 384 * 640 * 4 * 3);
        #[cfg(not(feature = "heic"))]
        assert_eq!(thumbnail_peak(&path, FileKind::Heic), DEFAULT_PEAK);
    }
}
