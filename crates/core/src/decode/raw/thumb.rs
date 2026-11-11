//! RAW thumbnails: rawler's embedded previews, else a binned develop (see
//! `develop.rs`), else our own embedded-JPEG fallback (see the parent
//! module).
//!
//! # Chosen calls
//!
//! - A `RawSource` over the whole file, read into memory (`raw_source`, never
//!   a memory map), then `rawler::get_decoder`.
//! - `Decoder::preview_image`, then `Decoder::thumbnail_image`: the first
//!   whose long side is at least `max_side` is used. A broken one falls through
//!   to the next source, since the raw data may still be whole.
//! - Otherwise a binned develop, in three steps:
//!   1. `Decoder::raw_image` gives the raw samples, usually `u16`.
//!   2. `bin` (in `develop.rs`) is the demosaic. It reads those samples
//!      directly; rawler's `apply_scaling` is skipped because it makes a
//!      full-size `f32` copy.
//!      Each sample is scaled by its black and white level, exactly as
//!      `apply_scaling` would: per 2×2 Bayer position for a CFA, per sample
//!      for `LinearRaw`. Then each colour plane is averaged over square cells
//!      of the crop area.
//!
//!      The cell side is `base × k`, where `base` is 2 for Bayer and
//!      `LinearRaw` and 3 for X-Trans. Any such cell holds every colour,
//!      because every 2×2 Bayer window and every 3×3 X-Trans window does.
//!      `k` is the largest factor that keeps the binned long side at least
//!      `2 × max_side`, and never below 1 (see `bin_factor`). For 512 that
//!      gives at least 1024, so the size depends on `max_side`, not on the
//!      sensor.
//!   3. The binned planes replace the samples in the `RawImage`, which becomes
//!      a `LinearRaw` image. `RawDevelop` with only `WhiteBalance`, `Calibrate`
//!      and `SRgb` then applies the camera white balance, the camera-to-sRGB
//!      matrix and the sRGB curve. Its 16-bit output becomes 8-bit by keeping
//!      the high byte, as for 16-bit stills.
//! - The image is shrunk to the thumbnail first and turned upright by **our**
//!   EXIF `orientation` last, so no full-size image is rotated (that would
//!   take a second full-size buffer).
//!
//! One call holds one full-size buffer, besides the file's bytes:
//! - on the preview path, the decoded preview (3 B/px);
//! - on the develop path, rawler's raw samples (2 B/px).
//!
//! The binned image and rawler's copies of it are about
//! `(2 × max_side)²` pixels, whatever the sensor.

use std::path::Path;

use image::{DynamicImage, RgbImage};
use rawler::decoders::RawDecodeParams;
use rawler::rawsource::RawSource;

use super::develop::develop_binned;
use super::{classify, guarded, or_fallback, raw_source};
use crate::decode::{DecodeError, ImageSource, Thumbnail, decode_jpeg, orient, preview, to_rgb8};
use crate::imageops;

/// Which image of the RAW file a thumbnail was made from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    Preview,
    Thumbnail,
    Develop,
}

/// The thumbnail of a RAW file, at most `max_side` on its long side and
/// upright by our EXIF `orientation`: from rawler's largest embedded preview
/// of at least `max_side`, else from a binned develop. See the module doc.
///
/// When rawler fails (an error, or a panic caught by [`guarded`]), the
/// thumbnail comes from [`preview_thumbnail`] instead, with rawler's error as
/// its `fallback_reason`. When that finds no usable JPEG either, rawler's
/// error is returned, followed by the fallback's own error when the RAW has
/// an embedded JPEG (see [`or_fallback`]). A file that cannot be read is an
/// I/O error, with no fallback.
pub(crate) fn raw_thumbnail(
    path: &Path,
    orientation: u16,
    max_side: u32,
) -> Result<Thumbnail, DecodeError> {
    let rawler_error = match render(path, orientation, max_side, true) {
        Ok((thumb, _)) => return Ok(Thumbnail::decoded(thumb)),
        Err(err @ DecodeError::Io(_)) => return Err(err),
        Err(err) => err,
    };
    let reason = rawler_error.to_string();
    let thumb = or_fallback(rawler_error, preview_thumbnail(path, orientation, max_side))?;
    Ok(Thumbnail {
        fallback_reason: Some(reason),
        ..thumb
    })
}

/// The thumbnail of the largest JPEG embedded in the RAW at `path` that
/// decodes, found by our own parsing (see `preview.rs`), with
/// [`ImageSource::EmbeddedPreview`]. The JPEGs are tried largest first, so
/// a broken one gives way to the next. Like rawler's preview it is shrunk
/// first and turned upright by our EXIF `orientation` last. A RAW without a
/// JPEG is `Ok(None)`; when none decodes, the last one's error is returned.
pub(crate) fn preview_thumbnail(
    path: &Path,
    orientation: u16,
    max_side: u32,
) -> Result<Option<Thumbnail>, DecodeError> {
    let mut last_error = None;
    let mut decoded = None;
    for jpeg in preview::embedded_jpegs(path)? {
        match decode_jpeg(jpeg) {
            Ok(image) => {
                decoded = Some(image);
                break;
            }
            Err(err) => last_error = Some(err),
        }
    }
    let image = match (decoded, last_error) {
        (Some(image), _) => to_rgb8(image),
        (None, Some(err)) => return Err(err),
        (None, None) => return Ok(None),
    };
    let thumb = imageops::pillow_thumbnail(&image, max_side);
    drop(image);
    Ok(Some(Thumbnail {
        image: orient(thumb, orientation),
        source: ImageSource::EmbeddedPreview,
        fallback_reason: None,
    }))
}

/// [`raw_thumbnail`], and where it came from; `use_embedded: false` always
/// develops.
fn render(
    path: &Path,
    orientation: u16,
    max_side: u32,
    use_embedded: bool,
) -> Result<(RgbImage, Origin), DecodeError> {
    let source = raw_source(path)?;
    let (image, origin) = guarded(path, || full_image(&source, max_side, use_embedded))?;
    let thumb = imageops::pillow_thumbnail(&image, max_side);
    drop(image);
    Ok((orient(thumb, orientation), origin))
}

/// The first big enough embedded image, else the binned develop.
fn full_image(
    source: &RawSource,
    max_side: u32,
    use_embedded: bool,
) -> Result<(RgbImage, Origin), DecodeError> {
    let decoder = rawler::get_decoder(source).map_err(classify)?;
    let params = RawDecodeParams::default();
    if use_embedded {
        if let Some(preview) = big_enough(decoder.preview_image(source, &params), max_side) {
            return Ok((preview, Origin::Preview));
        }
        if let Some(thumb) = big_enough(decoder.thumbnail_image(source, &params), max_side) {
            return Ok((thumb, Origin::Thumbnail));
        }
    }
    let raw = decoder
        .raw_image(source, &params, false)
        .map_err(classify)?;
    Ok((develop_binned(raw, max_side)?, Origin::Develop))
}

/// The embedded image as RGB8 when it has one whose long side reaches `max_side`.
fn big_enough(embedded: rawler::Result<Option<DynamicImage>>, max_side: u32) -> Option<RgbImage> {
    let image = embedded.ok().flatten()?;
    (image.width().max(image.height()) >= max_side).then(|| to_rgb8(image))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Instant;

    use photostacker_testkit::{
        MakerNoteSpec, NoteStyle, TiffSpec, Value, cr3_with_cmt, jpeg_with_exif, raf_with_jpeg,
        tiff_raw_with_jpegs,
    };

    use super::*;
    use crate::decode::raw::support::{fixture_raws, marked_noise, name, orientation_of};
    use crate::hash::{distance, hash_pair};

    fn is_portrait(img: &RgbImage) -> bool {
        img.height() > img.width()
    }

    /// Every RAW under `sub` yields a 512 thumbnail, upright by our EXIF.
    /// Returns the names of the files whose orientation swaps the sides.
    fn check_fixture_thumbnails(sub: &str) -> Option<Vec<String>> {
        let raws = fixture_raws(sub)?;
        let mut turned = Vec::new();
        println!("| file | path | size | ms |");
        for path in &raws {
            let orientation = orientation_of(path);
            let start = Instant::now();
            let (thumb, origin) = render(path, orientation, 512, true)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let ms = start.elapsed().as_millis();
            let (w, h) = thumb.dimensions();
            println!("| {} | {origin:?} | {w}x{h} | {ms} |", name(path));
            assert_eq!(w.max(h), 512, "{}", path.display());
            // Every fixture sensor is landscape: orientations 5..=8 make it portrait.
            let swaps = (5..=8).contains(&orientation);
            assert_eq!(is_portrait(&thumb), swaps, "{}", path.display());
            if swaps {
                turned.push(name(path).to_owned());
            }
        }
        Some(turned)
    }

    #[test]
    fn raw_thumbnail_long_side_is_512() {
        check_fixture_thumbnails("rawsamples");
    }

    #[test]
    fn portrait_raw_is_portrait() {
        let Some(turned) = check_fixture_thumbnails("rawsamples") else {
            return;
        };
        // The portrait shots of fixture-sources.md, so a lost orientation cannot pass.
        for expected in [
            "IMG_4361.CR3",
            "Fuji-X-H2-Compressed-Raw.RAF",
            "_DSC0028.NEF",
            "KTB00899.ARW",
        ] {
            assert!(
                turned.iter().any(|t| t == expected),
                "{expected}: {turned:?}"
            );
        }
    }

    #[test]
    fn raw_bracket_fixtures_yield_thumbnails() {
        let Some(turned) = check_fixture_thumbnails("brackets") else {
            return;
        };
        // The Canon 1D Mk IV AEB set was shot in portrait.
        assert_eq!(turned.iter().filter(|t| t.starts_with("AH7C")).count(), 7);
    }

    #[test]
    fn raw_binned_develop_frames_like_the_preview() {
        let Some(raws) = fixture_raws("rawsamples") else {
            return;
        };
        println!("| file | develop size | develop ms | distance to preview |");
        for path in &raws {
            let orientation = orientation_of(path);
            let (preview, _) = render(path, orientation, 512, true).unwrap();
            let start = Instant::now();
            let developed = render(path, orientation, 512, false);
            let ms = start.elapsed().as_millis();
            // rawler 0.8 cannot decode Nikon's High Efficiency (TicoRAW) data;
            // that file gets its thumbnail from the preview only.
            if name(path) == "Nikon_Z8_high_efficiency_low.NEF" {
                let err = developed.unwrap_err();
                println!("| {} | {err} | {ms} | - |", name(path));
                assert!(matches!(err, DecodeError::Corrupt(_)), "{err:?}");
                continue;
            }
            let (developed, origin) =
                developed.unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert_eq!(origin, Origin::Develop);
            let (w, h) = developed.dimensions();
            let d = distance(hash_pair(&preview), hash_pair(&developed));
            println!("| {} | {w}x{h} | {ms} | {d} |", name(path));
            assert_eq!(w.max(h), 512, "{}", path.display());
            assert_eq!(
                is_portrait(&developed),
                is_portrait(&preview),
                "{}",
                path.display()
            );
            let aspect = |img: &RgbImage| f64::from(img.width()) / f64::from(img.height());
            let ratio = aspect(&developed) / aspect(&preview);
            assert!(
                (0.97..=1.03).contains(&ratio),
                "{}: {ratio}",
                path.display()
            );
            // A flipped, turned or miscoloured develop is far from the preview.
            assert!(d <= 32, "{}: distance {d}", path.display());
        }
    }

    #[test]
    fn raw_cut_fixtures_are_errors_or_thumbnails_never_panics() {
        let Some(raws) = fixture_raws("rawsamples") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        for path in &raws {
            let bytes = fs::read(path).unwrap();
            for keep in [bytes.len() / 2, 64 * 1024, 16] {
                let cut = dir.path().join(name(path));
                fs::write(&cut, &bytes[..keep]).unwrap();
                // A preview before the cut may still be whole; anything but a panic is fine.
                if let Ok(thumb) = raw_thumbnail(&cut, 1, 512) {
                    let long_side = thumb.image.width().max(thumb.image.height());
                    // rawler's own output is always 512; the embedded-preview
                    // fallback may keep a smaller whole JPEG, such as the
                    // IFD0 thumbnail in front of a cut-off large preview.
                    match thumb.source {
                        ImageSource::Decoded => assert_eq!(long_side, 512),
                        ImageSource::EmbeddedPreview => assert!(long_side <= 512),
                    }
                }
            }
        }
    }

    /// A RAF that rawler takes (an X-T5 with a Fujifilm MakerNote) around `preview`.
    fn synthetic_raf(preview: &RgbImage) -> Vec<u8> {
        let tiff = TiffSpec {
            little_endian: false,
            ifd0: vec![
                (0x010F, Value::Ascii("FUJIFILM".into())),
                (0x0110, Value::Ascii("X-T5".into())),
            ],
            exif: Vec::new(),
            maker_note: Some(MakerNoteSpec {
                style: NoteStyle::FujifilmHeader,
                entries: vec![(0x1000, Value::Ascii("NORMAL ".into()))],
            }),
        }
        .build();
        raf_with_jpeg(&jpeg_with_exif(preview, &tiff, 90))
    }

    #[test]
    fn raf_preview_is_shrunk_then_turned_upright() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.raf");
        fs::write(&path, synthetic_raf(&marked_noise(1200, 800))).unwrap();

        let (thumb, origin) = render(&path, 1, 512, true).unwrap();
        assert_eq!(origin, Origin::Preview);
        assert_eq!(thumb.dimensions(), (512, 341));
        assert!(thumb.get_pixel(5, 5).0[0] > 200);

        // Orientation 6 turns it 90° clockwise: the red corner goes top-right.
        let turned = raw_thumbnail(&path, 6, 512).unwrap();
        // rawler knows the body, so its preview counts as decoded.
        assert_eq!(turned.source, ImageSource::Decoded);
        assert_eq!(turned.fallback_reason, None);
        let turned = turned.image;
        assert_eq!(turned.dimensions(), (341, 512));
        let corner = turned.get_pixel(341 - 6, 5).0;
        assert!(corner[0] > 200 && corner[1] < 60, "{corner:?}");
    }

    #[test]
    fn small_preview_falls_through_to_the_develop() {
        // The preview is too small and the RAF has no real CFA data: the
        // develop is tried, and fails, instead of using the small preview.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.raf");
        fs::write(&path, synthetic_raf(&marked_noise(400, 300))).unwrap();
        let err = render(&path, 1, 512, true).unwrap_err();
        assert!(
            matches!(err, DecodeError::Unsupported(_) | DecodeError::Corrupt(_)),
            "{err:?}"
        );
        // With a smaller `max_side` the same preview is enough.
        let (thumb, origin) = render(&path, 1, 256, true).unwrap();
        assert_eq!(origin, Origin::Preview);
        assert_eq!(thumb.dimensions(), (256, 192));
        // After the failed develop, the small preview is still better than nothing.
        let fallback = raw_thumbnail(&path, 1, 512).unwrap();
        assert_eq!(fallback.source, ImageSource::EmbeddedPreview);
        assert_eq!(fallback.fallback_reason, Some(err.to_string()));
        assert_eq!(fallback.image.dimensions(), (400, 300));
    }

    #[test]
    fn unknown_body_falls_back_to_embedded_preview() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unknown.nef");
        fs::write(&path, tiff_raw_with_jpegs(&[(160, 120), (1024, 683)])).unwrap();
        // rawler picks its decoder by Make and Model, and knows no such body.
        let err = render(&path, 1, 512, true).unwrap_err();
        assert!(matches!(err, DecodeError::Unsupported(_)), "{err:?}");

        let thumb = raw_thumbnail(&path, 1, 512).unwrap();
        assert_eq!(thumb.source, ImageSource::EmbeddedPreview);
        assert_eq!(thumb.fallback_reason, Some(err.to_string()));
        // The 1024×683 SubIFD JPEG, not the 160×120 thumbnail.
        assert_eq!(thumb.image.dimensions(), (512, 342));
        let fallback = preview_thumbnail(&path, 1, 512).unwrap().unwrap();
        assert_eq!(fallback.image, thumb.image);

        // Our orientation turns it upright, as on rawler's preview path.
        let turned = raw_thumbnail(&path, 6, 512).unwrap();
        assert_eq!(turned.source, ImageSource::EmbeddedPreview);
        assert_eq!(turned.image.dimensions(), (342, 512));
        assert_eq!(turned.image, orient(thumb.image, 6));
    }

    #[test]
    fn undecodable_largest_preview_falls_back_to_the_next() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unknown.nef");
        // The 1024×683 JPEG ends the file and fills most of it: zeroing the
        // last quarter of the file corrupts only its scan data, EOI included.
        let mut raw = tiff_raw_with_jpegs(&[(160, 120), (1024, 683)]);
        let cut = raw.len() * 3 / 4;
        raw[cut..].fill(0);
        fs::write(&path, &raw).unwrap();
        let jpegs = preview::embedded_jpegs(&path).unwrap();
        assert_eq!(preview::jpeg_dimensions(&jpegs[0]), Some((1024, 683)));
        assert!(decode_jpeg(jpegs[0].clone()).is_err());

        let rawler = render(&path, 1, 512, true).unwrap_err();
        let thumb = raw_thumbnail(&path, 1, 512).unwrap();
        assert_eq!(thumb.source, ImageSource::EmbeddedPreview);
        assert_eq!(thumb.fallback_reason, Some(rawler.to_string()));
        // The 160×120 IFD0 thumbnail, below `max_side` and kept as is.
        assert_eq!(thumb.image.dimensions(), (160, 120));
        assert_eq!(thumb.image, to_rgb8(decode_jpeg(jpegs[1].clone()).unwrap()));
    }

    #[test]
    fn a_failed_fallback_adds_its_error_to_rawler_s() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unknown.nef");
        // The only JPEG ends the file: zeroing its last bytes takes its EOI.
        let mut raw = tiff_raw_with_jpegs(&[(1024, 683)]);
        let len = raw.len();
        raw[len - 64..].fill(0);
        fs::write(&path, &raw).unwrap();
        let rawler = render(&path, 1, 512, true).unwrap_err();
        let jpeg = preview::embedded_jpegs(&path).unwrap().swap_remove(0);
        let fallback = decode_jpeg(jpeg).unwrap_err();
        assert!(matches!(fallback, DecodeError::Corrupt(_)), "{fallback:?}");

        let err = raw_thumbnail(&path, 1, 512).unwrap_err();
        assert!(matches!(err, DecodeError::Unsupported(_)), "{err:?}");
        let want = format!("{rawler} (embedded preview: {fallback})");
        assert_eq!(err.to_string(), want);
    }

    #[test]
    fn no_embedded_jpeg_keeps_the_rawler_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decoy.cr3");
        // JPEG bytes named as a RAW are no RAW container: nothing to fall back to.
        fs::write(&path, jpeg_with_exif(&marked_noise(600, 400), &[], 90)).unwrap();
        let rawler = render(&path, 1, 512, true).unwrap_err();
        let err = raw_thumbnail(&path, 1, 512).unwrap_err();
        assert_eq!(err.to_string(), rawler.to_string());
        assert!(matches!(preview_thumbnail(&path, 1, 512), Ok(None)));
    }

    #[test]
    fn preview_fallback_matches_rawler_on_fixtures() {
        let Some(raws) = fixture_raws("") else {
            return;
        };
        println!("| file | embedded JPEG | rawler origin | distance |");
        let mut compared = 0;
        for path in &raws {
            let jpegs =
                preview::embedded_jpegs(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let Some(jpeg) = jpegs.first() else {
                println!("| {} | none | - | - |", name(path));
                continue;
            };
            let (w, h) = preview::jpeg_dimensions(jpeg).unwrap();
            let orientation = orientation_of(path);
            let (rawler, origin) = render(path, orientation, 512, true)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let fallback = preview_thumbnail(path, orientation, 512)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
                .expect("the RAW embeds a JPEG");
            assert_eq!(fallback.source, ImageSource::EmbeddedPreview);
            let d = distance(hash_pair(&rawler), hash_pair(&fallback.image));
            println!("| {} | {w}x{h} | {origin:?} | {d} |", name(path));
            assert_eq!(
                is_portrait(&fallback.image),
                is_portrait(&rawler),
                "{}",
                path.display()
            );
            assert!(d <= 32, "{}: distance {d}", path.display());
            compared += 1;
        }
        println!("compared {compared} of {} RAW files", raws.len());
        // Every format of the corpus (CR2, CR3, NEF, ARW, RAF) embeds a JPEG,
        // so a file without one means the parser missed it.
        assert_eq!(compared, raws.len(), "a fixture RAW has no embedded JPEG");
    }

    #[test]
    fn garbage_raw_is_decode_error() {
        let mut state = 0x9E37_79B9_u32;
        let garbage: Vec<u8> = (0..4096)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("garbage.cr2");
        fs::write(&path, garbage).unwrap();
        let err = raw_thumbnail(&path, 1, 512).unwrap_err();
        assert!(
            matches!(err, DecodeError::Unsupported(_) | DecodeError::Corrupt(_)),
            "{err:?}"
        );
    }

    #[test]
    fn synthetic_containers_without_image_data_are_decode_errors() {
        let tiff = TiffSpec {
            little_endian: true,
            ifd0: vec![
                (0x010F, Value::Ascii("Canon".into())),
                (0x0110, Value::Ascii("Canon EOS 5D Mark IV".into())),
            ],
            exif: Vec::new(),
            maker_note: None,
        }
        .build();
        let dir = tempfile::tempdir().unwrap();
        for (name, bytes) in [
            ("tiff-only.cr2", tiff.clone()),
            ("cmt-only.cr3", cr3_with_cmt(&tiff, &tiff, &tiff)),
            ("no-jpeg.raf", raf_with_jpeg(&[0xFF, 0xD8, 0xFF, 0xD9])),
            ("empty.nef", Vec::new()),
        ] {
            let path = dir.path().join(name);
            fs::write(&path, bytes).unwrap();
            let err = raw_thumbnail(&path, 1, 512).unwrap_err();
            assert!(
                matches!(err, DecodeError::Unsupported(_) | DecodeError::Corrupt(_)),
                "{name}: {err:?}"
            );
        }
    }

    #[test]
    fn missing_raw_is_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = raw_thumbnail(&dir.path().join("gone.cr2"), 1, 512).unwrap_err();
        assert!(matches!(err, DecodeError::Io(_)), "{err:?}");
    }
}
