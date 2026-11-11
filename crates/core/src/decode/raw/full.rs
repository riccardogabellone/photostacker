//! RAW at full resolution for the writers, and its header-only probe.
//!
//! # Full resolution
//!
//! [`raw_full`] develops the whole crop area for the writers, at 16 bits:
//! 1. `Decoder::raw_image` gives the raw samples, and the file's bytes are
//!    dropped.
//! 2. `RawDevelop::new_with(steps).develop_intermediate(&raw)` runs rawler's
//!    own steps on them, in `f32`:
//!    - `Rescale` (black and white levels);
//!    - `Demosaic` over the active area (`CropActiveArea`): PPG for Bayer,
//!      bilinear for X-Trans and four-colour Bayer;
//!    - `WhiteBalance` with the camera's coefficients;
//!    - `Calibrate`, the camera matrix to sRGB primaries, with rawler's
//!      highlight clip;
//!    - `CropDefault`, to the crop area;
//!    - `SRgb`, the sRGB curve, only for [`RawTone::Srgb`].
//!
//!    No step brightens the image automatically. `FujiRotate` is left out,
//!    because a SuperCCD RAW (45° sensor) is refused and takes the fallback.
//! 3. Each sample becomes `round(clamp(v, 0, 1) × 65535)`. rawler's
//!    `to_dynamic_image` would truncate instead, and copy the image once more.
//! 4. Our EXIF `orientation` turns it upright.
//!
//! The layer carries `icc::rgb_profile(tone)`: sRGB, or linear sRGB.
//!
//! Full-size buffers alive at once, in bytes per pixel (B/px):
//! - our raw samples, `u16` (2), held through the develop, which borrows them;
//! - rawler's clone of them, scaled to `f32` (4) by `Rescale` and kept to the
//!   end of the develop;
//! - the demosaic's input, one more `f32` copy (4), until the demosaic returns;
//! - the demosaiced RGB (12);
//! - the calibrated RGB (12), built while the demosaiced one is alive, then
//!   the cropped RGB (12), built while the calibrated one is alive.
//!
//! So the peak is 2 + 4 + 12 + 12 = 30 B/px, during `Calibrate` and
//! `CropDefault`: about 1.27 GB for 42 MP. Measured with `VmHWM`, the
//! largest corpus RAW (ILCE-7RM3, 7952×5304, 42.2 MP) peaks at 1274 MiB for
//! the whole process. After the develop come the `f32` RGB (12) and the
//! `u16` RGB (6), and for orientations 5 to 8 a turned copy of the `u16` RGB
//! (6 + 6).
//!
//! [`raw_probe`] calls `Decoder::raw_image` with `dummy = true`. rawler then
//! parses the headers and reserves the sample buffer without decompressing
//! into it, so the reserved pages are never touched. [`developed_size`]
//! works out the develop's size from the active and crop areas, the way
//! `develop_intermediate` does. It also refuses what the develop would panic
//! on, so the probe and the develop agree. The probe still reads the whole
//! file once, and rawler has no lazier source: `RawSource::new`, its only
//! file-backed constructor, maps the file with `MAP_POPULATE` (hard-coded in
//! rawler 0.8's `rawsource.rs`), and the others take the whole file in a
//! `Vec`. We take the `Vec` (`raw_source`), since a mapped file that is
//! truncated kills the process (spec §4.3). Measured with the map, which
//! reads the file just the same, that cost 0–45 ms per RAW from a local disk
//! in the page cache (45 ms for the 85 MB uncompressed ARW), against 0.4–2.0
//! s for the develop. From a Windows drive mounted in WSL it cost up to 2.2
//! s, all of it I/O.

use std::path::Path;

use image::{DynamicImage, ImageBuffer, ImageDecoder, Rgb};
use rawler::decoders::RawDecodeParams;
use rawler::imgop::Dim2;
use rawler::imgop::develop::{ProcessingStep, RawDevelop};
use rawler::rawimage::RawImage;

use super::develop::{DEVELOP_STEPS, developed_size, rgb16};
use super::{classify, guarded, or_fallback, raw_source};
use crate::decode::full::upright_size;
use crate::decode::{
    DecodeError, FullImage, ImageSource, Probe, Samples, classify as classify_image, jpeg_decoder,
    orient, preview, to_rgb8,
};
use crate::icc;
use crate::model::RawTone;

/// The RAW at `path` at full resolution, for the writers: rawler's develop
/// of the whole crop area as [`Samples::U16`] RGB in `tone`, upright by our
/// EXIF `orientation`, with our generated profile `icc::rgb_profile(tone)`.
/// See "Full resolution" in the module doc.
///
/// The fallback is decided as in [`raw_probe`]. The largest
/// embedded JPEG stands in only when rawler cannot even probe the file (an
/// error, or a panic caught by [`guarded`]): at full size, as
/// [`Samples::U8`], with [`ImageSource::EmbeddedPreview`] and only the JPEG's
/// own ICC profile, which `decode::full` keeps only when it is an RGB one.
/// When there is no usable JPEG, rawler's error is returned, followed by the
/// fallback's own error when the RAW has an embedded JPEG (see
/// [`or_fallback`]).
///
/// A RAW that probes as a develop but then fails to develop (corrupt sample
/// data, a rawler panic) is an error, never the JPEG: the probe has
/// promised 16-bit pixels in our profile. The writer excludes it. A file
/// that cannot be read is an I/O error, with no fallback.
pub(crate) fn raw_full(
    path: &Path,
    orientation: u16,
    tone: RawTone,
) -> Result<FullImage, DecodeError> {
    let rawler_error = match develop_full(path, orientation, tone) {
        Ok(image) => return Ok(image),
        Err(err @ DecodeError::Io(_)) => return Err(err),
        Err(err) => err,
    };
    // Only on this failure path: the probe's dummy decode decides.
    match develop_probe(path, orientation, tone) {
        Ok(_) => Err(rawler_error),
        Err(err @ DecodeError::Io(_)) => Err(err),
        Err(_) => or_fallback(rawler_error, preview_full(path, orientation)),
    }
}

/// What [`raw_full`] gives for the same arguments, from the headers only:
/// the size of rawler's develop after our `orientation`, 16-bit RGB with
/// `icc::rgb_profile(tone)`. When rawler cannot read the headers it gives
/// the embedded JPEG's size and ICC profile, 8-bit, and [`raw_full`] then
/// falls back to that JPEG too: both take the same decision, and give the
/// same kind of error when there is no usable JPEG. See "Full resolution" in
/// the module doc.
pub(crate) fn raw_probe(
    path: &Path,
    orientation: u16,
    tone: RawTone,
) -> Result<Probe, DecodeError> {
    let rawler_error = match develop_probe(path, orientation, tone) {
        Ok(probe) => return Ok(probe),
        Err(err @ DecodeError::Io(_)) => return Err(err),
        Err(err) => err,
    };
    or_fallback(rawler_error, preview_probe(path, orientation))
}

/// rawler's full-size develop, before any fallback.
fn develop_full(path: &Path, orientation: u16, tone: RawTone) -> Result<FullImage, DecodeError> {
    let raw = raw_image(path, false)?;
    let expected = developed_size(&raw)?;
    let mut steps = DEVELOP_STEPS.to_vec();
    if tone == RawTone::Srgb {
        steps.push(ProcessingStep::SRgb);
    }
    let developed = guarded(path, || {
        RawDevelop::new_with(&steps)
            .develop_intermediate(&raw)
            .map_err(classify)
    })?;
    drop(raw);
    let (size, samples) = rgb16(developed)?;
    // A mismatch means `developed_size` no longer mirrors rawler: a bug, not
    // a broken file. The probe succeeded, so `raw_full` does not fall back.
    debug_assert_eq!(size, expected, "developed_size disagrees with rawler");
    if size != expected {
        return Err(DecodeError::Corrupt(format!(
            "internal error: develop size {}x{} differs from the probed {}x{}",
            size.w, size.h, expected.w, expected.h
        )));
    }
    let (width, height) = pixel_size(size)?;
    let image = ImageBuffer::<Rgb<u16>, _>::from_raw(width, height, samples).ok_or_else(|| {
        DecodeError::Corrupt("the developed RAW has fewer pixels than its size".into())
    })?;
    let image = orient(image, orientation);
    Ok(FullImage {
        width: image.width(),
        height: image.height(),
        channels: 3,
        samples: Samples::U16(image.into_raw()),
        icc: Some(icc::rgb_profile(tone)),
        source: ImageSource::Decoded,
    })
}

/// [`raw_probe`] for rawler's develop, before any fallback.
fn develop_probe(path: &Path, orientation: u16, tone: RawTone) -> Result<Probe, DecodeError> {
    let raw = raw_image(path, true)?;
    let (width, height) = pixel_size(developed_size(&raw)?)?;
    let (width, height) = upright_size(width, height, orientation);
    Ok(Probe {
        width,
        height,
        sixteen_bit: true,
        has_alpha: false,
        icc: Some(icc::rgb_profile(tone)),
    })
}

/// rawler's raw image of `path`. With `dummy`, rawler reads the headers and
/// reserves the sample buffer without decompressing anything into it, so
/// its pages are never touched. The file's bytes are dropped on return.
fn raw_image(path: &Path, dummy: bool) -> Result<RawImage, DecodeError> {
    let source = raw_source(path)?;
    guarded(path, || {
        let decoder = rawler::get_decoder(&source).map_err(classify)?;
        decoder
            .raw_image(&source, &RawDecodeParams::default(), dummy)
            .map_err(classify)
    })
}

/// A developed size in pixels as `u32`.
fn pixel_size(size: Dim2) -> Result<(u32, u32), DecodeError> {
    let side = |v: usize| {
        u32::try_from(v).map_err(|_| DecodeError::Corrupt(format!("RAW side of {v} px")))
    };
    Ok((side(size.w)?, side(size.h)?))
}

/// The largest embedded JPEG at full size: RGB8, upright by our EXIF
/// `orientation`, with the JPEG's own ICC profile. `Ok(None)` when the RAW
/// has no embedded JPEG (see `preview.rs`).
fn preview_full(path: &Path, orientation: u16) -> Result<Option<FullImage>, DecodeError> {
    let Some(jpeg) = preview::largest_embedded_jpeg(path)? else {
        return Ok(None);
    };
    let mut decoder = jpeg_decoder(jpeg)?;
    let icc = decoder.icc_profile().map_err(classify_image)?;
    let decoded = DynamicImage::from_decoder(decoder).map_err(classify_image)?;
    let image = orient(to_rgb8(decoded), orientation);
    Ok(Some(FullImage {
        width: image.width(),
        height: image.height(),
        channels: 3,
        samples: Samples::U8(image.into_raw()),
        icc,
        source: ImageSource::EmbeddedPreview,
    }))
}

/// [`preview_full`]'s size and ICC profile, from the JPEG's headers.
fn preview_probe(path: &Path, orientation: u16) -> Result<Option<Probe>, DecodeError> {
    let Some(jpeg) = preview::largest_embedded_jpeg(path)? else {
        return Ok(None);
    };
    let mut decoder = jpeg_decoder(jpeg)?;
    let icc = decoder.icc_profile().map_err(classify_image)?;
    let (width, height) = decoder.dimensions();
    let (width, height) = upright_size(width, height, orientation);
    Ok(Some(Probe {
        width,
        height,
        sixteen_bit: false,
        has_alpha: false,
        icc,
    }))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Instant;

    use image::RgbImage;
    use photostacker_testkit::{jpeg_with_exif, jpeg_with_icc, raf_with_jpeg, tiff_raw_with_jpegs};

    use super::*;
    use crate::decode::decode_jpeg;
    use crate::decode::raw::raw_thumbnail;
    use crate::decode::raw::support::{fixture_raws, marked_noise, name, orientation_of};
    use crate::hash::{distance, hash_pair};
    use crate::icc::rgb_profile;
    use crate::imageops;

    fn full_of(path: &Path, orientation: u16, tone: RawTone) -> FullImage {
        raw_full(path, orientation, tone).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    fn probe_of(path: &Path, orientation: u16, tone: RawTone) -> Probe {
        raw_probe(path, orientation, tone).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    fn u16s(image: &FullImage) -> &[u16] {
        match &image.samples {
            Samples::U16(samples) => samples,
            Samples::U8(_) => panic!("8-bit samples, expected 16-bit"),
        }
    }

    fn u8s(image: &FullImage) -> &[u8] {
        match &image.samples {
            Samples::U8(samples) => samples,
            Samples::U16(_) => panic!("16-bit samples, expected 8-bit"),
        }
    }

    /// `image` shrunk to `max_side` as RGB8, 16-bit samples keeping their high byte.
    fn thumbnail_of(image: FullImage, max_side: u32) -> RgbImage {
        let (w, h) = (image.width, image.height);
        let rgb = match image.samples {
            Samples::U8(samples) => RgbImage::from_raw(w, h, samples).unwrap(),
            Samples::U16(samples) => to_rgb8(DynamicImage::ImageRgb16(
                ImageBuffer::from_raw(w, h, samples).unwrap(),
            )),
        };
        imageops::pillow_thumbnail(&rgb, max_side)
    }

    /// rawler 0.8 cannot decode Nikon's High Efficiency (TicoRAW) data.
    const UNDECODABLE: &str = "Nikon_Z8_high_efficiency_low.NEF";

    #[test]
    fn raw_full_dimensions_match_probe_on_fixtures() {
        let Some(raws) = fixture_raws("") else {
            return;
        };
        println!("| file | source | size | probe ms | full ms |");
        for path in &raws {
            let orientation = orientation_of(path);
            let start = Instant::now();
            let probed = probe_of(path, orientation, RawTone::Srgb);
            let probe_ms = start.elapsed().as_millis();
            let start = Instant::now();
            let image = full_of(path, orientation, RawTone::Srgb);
            let full_ms = start.elapsed().as_millis();
            let (w, h) = (image.width, image.height);
            println!(
                "| {} | {:?} | {w}x{h} | {probe_ms} | {full_ms} |",
                name(path),
                image.source
            );
            assert_eq!((probed.width, probed.height), (w, h), "{}", path.display());
            let sixteen_bit = matches!(image.samples, Samples::U16(_));
            assert_eq!(probed.sixteen_bit, sixteen_bit, "{}", path.display());
            assert_eq!(probed.has_alpha, image.channels == 4, "{}", path.display());
            assert_eq!(probed.icc, image.icc, "{}", path.display());
            assert_eq!(image.channels, 3, "{}", path.display());
            if name(path) == UNDECODABLE {
                assert_eq!(image.source, ImageSource::EmbeddedPreview);
                assert_eq!(image.icc, None);
                continue;
            }
            assert_eq!(image.source, ImageSource::Decoded, "{}", path.display());
            assert_eq!(u16s(&image).len(), w as usize * h as usize * 3);
            assert_eq!(image.icc, Some(rgb_profile(RawTone::Srgb)));
        }
    }

    #[test]
    fn raw_full_portrait_is_upright() {
        let Some(raws) = fixture_raws("") else {
            return;
        };
        // The portrait shots of docs/exif-fixture-coverage.md: R10, X-H2,
        // Z 30, ILCE-6400A and one frame of the 1D Mk IV AEB set.
        let portraits = [
            "IMG_4361.CR3",
            "Fuji-X-H2-Compressed-Raw.RAF",
            "_DSC0028.NEF",
            "KTB00899.ARW",
            "AH7C0414.CR2",
        ];
        println!("| file | orientation | size | distance to preview |");
        for want in portraits {
            let path = raws
                .iter()
                .find(|p| name(p) == want)
                .unwrap_or_else(|| panic!("{want} is not in the corpus"));
            let orientation = orientation_of(path);
            assert!((5..=8).contains(&orientation), "{want}: {orientation}");
            let image = full_of(path, orientation, RawTone::Srgb);
            assert_eq!(image.source, ImageSource::Decoded, "{want}");
            let (w, h) = (image.width, image.height);
            assert!(h > w, "{want}: {w}x{h}");
            // Upright, not merely portrait: it frames like rawler's preview,
            // which the thumbnail tests check against our orientation.
            let preview = raw_thumbnail(path, orientation, 512).unwrap().image;
            let d = distance(hash_pair(&preview), hash_pair(&thumbnail_of(image, 512)));
            println!("| {want} | {orientation} | {w}x{h} | {d} |");
            assert!(d <= 32, "{want}: distance {d}");
        }
    }

    #[test]
    fn linear_is_darker_than_srgb_in_midtones() {
        let Some(raws) = fixture_raws("rawsamples") else {
            return;
        };
        let path = raws.iter().find(|p| name(p) == "IMG_3397.CR2").unwrap();
        let orientation = orientation_of(path);
        let mean = |image: &FullImage| {
            let samples = u16s(image);
            samples.iter().map(|&v| f64::from(v)).sum::<f64>() / samples.len() as f64
        };
        let srgb = full_of(path, orientation, RawTone::Srgb);
        let srgb_size = (srgb.width, srgb.height);
        let srgb_mean = mean(&srgb);
        drop(srgb);
        let linear = full_of(path, orientation, RawTone::Linear);
        assert_eq!((linear.width, linear.height), srgb_size);
        assert_eq!(linear.icc, Some(rgb_profile(RawTone::Linear)));
        let linear_mean = mean(&linear);
        println!("mean sample: sRGB {srgb_mean:.0}, linear {linear_mean:.0}");
        assert!(linear_mean < srgb_mean, "{linear_mean} vs {srgb_mean}");
    }

    /// The peak resident set size of this process, from `VmHWM` (in kB).
    #[cfg(target_os = "linux")]
    fn peak_rss_bytes() -> u64 {
        let status = fs::read_to_string("/proc/self/status").unwrap();
        let line = status.lines().find(|l| l.starts_with("VmHWM:")).unwrap();
        let kib: u64 = line
            .trim_start_matches("VmHWM:")
            .trim()
            .trim_end_matches("kB")
            .trim()
            .parse()
            .unwrap();
        kib * 1024
    }

    /// One develop of the largest corpus RAW. nextest runs each test in a
    /// process of its own, so `VmHWM` is this develop's peak.
    #[cfg(target_os = "linux")]
    #[test]
    fn raw_full_peak_rss_is_reported() {
        let Some(raws) = fixture_raws("") else {
            return;
        };
        // The largest RAW that rawler develops; a probe reads only headers.
        let (path, pixels) = raws
            .iter()
            .filter_map(|path| {
                let probed = raw_probe(path, 1, RawTone::Srgb).ok()?;
                let pixels = u64::from(probed.width) * u64::from(probed.height);
                probed.sixteen_bit.then_some((path, pixels))
            })
            .max_by_key(|&(_, pixels)| pixels)
            .unwrap();
        let image = full_of(path, orientation_of(path), RawTone::Srgb);
        assert_eq!(image.source, ImageSource::Decoded);
        let peak = peak_rss_bytes();
        let mib = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);
        eprintln!(
            "peak RSS (VmHWM) after one develop of {} ({}x{}, {:.1} MP): {:.0} MiB",
            name(path),
            image.width,
            image.height,
            pixels as f64 / 1e6,
            mib(peak)
        );
        let limit = 1536 * 1024 * 1024;
        assert!(peak < limit, "peak RSS {:.0} MiB", mib(peak));
    }

    #[test]
    fn raw_full_falls_back_to_embedded_preview() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unknown.nef");
        fs::write(&path, tiff_raw_with_jpegs(&[(160, 120), (1024, 683)])).unwrap();
        // rawler knows no such body: the 1024×683 SubIFD JPEG, at full size.
        for tone in [RawTone::Srgb, RawTone::Linear] {
            let image = full_of(&path, 1, tone);
            assert_eq!(image.source, ImageSource::EmbeddedPreview, "{tone:?}");
            assert_eq!((image.width, image.height, image.channels), (1024, 683, 3));
            assert_eq!(u8s(&image).len(), 1024 * 683 * 3);
            // Only the JPEG's own ICC profile, and this one has none.
            assert_eq!(image.icc, None, "{tone:?}");
        }
        let upright = full_of(&path, 1, RawTone::Srgb);
        let jpeg = preview::embedded_jpegs(&path).unwrap().swap_remove(0);
        assert_eq!(u8s(&upright), to_rgb8(decode_jpeg(jpeg).unwrap()).as_raw());

        // Our orientation turns it upright, as on the thumbnail path.
        let turned = full_of(&path, 6, RawTone::Srgb);
        assert_eq!((turned.width, turned.height), (683, 1024));
        let stored = RgbImage::from_raw(1024, 683, u8s(&upright).to_vec()).unwrap();
        assert_eq!(u8s(&turned), orient(stored, 6).as_raw());

        // The probe agrees, from the JPEG's header.
        for orientation in [1, 6] {
            let probed = probe_of(&path, orientation, RawTone::Srgb);
            let image = full_of(&path, orientation, RawTone::Srgb);
            assert_eq!((probed.width, probed.height), (image.width, image.height));
            assert!(!probed.sixteen_bit && !probed.has_alpha);
            assert_eq!(probed.icc, None);
        }

        // A JPEG's own ICC profile comes along, byte for byte.
        let icc: Vec<u8> = (0..=255_u8).cycle().take(300).collect();
        let raf = dir.path().join("icc.raf");
        let jpeg = jpeg_with_icc(&marked_noise(600, 400), &icc, 90);
        fs::write(&raf, raf_with_jpeg(&jpeg)).unwrap();
        let image = full_of(&raf, 1, RawTone::Srgb);
        assert_eq!(image.source, ImageSource::EmbeddedPreview);
        assert_eq!((image.width, image.height), (600, 400));
        assert_eq!(image.icc.as_deref(), Some(&icc[..]));
        let probed = probe_of(&raf, 1, RawTone::Srgb);
        assert_eq!(probed.icc.as_deref(), Some(&icc[..]));
    }

    #[test]
    fn raw_full_and_raw_probe_keep_io_and_rawler_errors() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone.cr2");
        let err = raw_full(&gone, 1, RawTone::Srgb).unwrap_err();
        assert!(matches!(err, DecodeError::Io(_)), "{err:?}");
        let err = raw_probe(&gone, 1, RawTone::Srgb).unwrap_err();
        assert!(matches!(err, DecodeError::Io(_)), "{err:?}");

        // JPEG bytes named as a RAW: no RAW container, so nothing to fall back to.
        let decoy = dir.path().join("decoy.cr3");
        fs::write(&decoy, jpeg_with_exif(&marked_noise(600, 400), &[], 90)).unwrap();
        let rawler = raw_thumbnail(&decoy, 1, 512).unwrap_err().to_string();
        let err = raw_full(&decoy, 1, RawTone::Srgb).unwrap_err();
        assert_eq!(err.to_string(), rawler);
        let err = raw_probe(&decoy, 1, RawTone::Srgb).unwrap_err();
        assert_eq!(err.to_string(), rawler);

        let empty = dir.path().join("empty.nef");
        fs::write(&empty, b"").unwrap();
        let err = raw_full(&empty, 1, RawTone::Srgb).unwrap_err();
        assert!(matches!(err, DecodeError::Corrupt(_)), "{err:?}");
        let err = raw_probe(&empty, 1, RawTone::Srgb).unwrap_err();
        assert!(matches!(err, DecodeError::Corrupt(_)), "{err:?}");
    }

    #[test]
    fn a_failed_fallback_adds_its_error_to_rawler_s() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unknown.nef");
        // The only JPEG ends the file: zeroing its last bytes takes its EOI,
        // so even its headers are refused (see `check_jpeg_complete`).
        let mut raw = tiff_raw_with_jpegs(&[(1024, 683)]);
        let len = raw.len();
        raw[len - 64..].fill(0);
        fs::write(&path, &raw).unwrap();
        let jpeg = preview::largest_embedded_jpeg(&path).unwrap().unwrap();
        let fallback = jpeg_decoder(jpeg).err().unwrap();
        assert!(matches!(fallback, DecodeError::Corrupt(_)), "{fallback:?}");

        let rawler = develop_full(&path, 1, RawTone::Srgb).unwrap_err();
        let err = raw_full(&path, 1, RawTone::Srgb).unwrap_err();
        assert!(matches!(err, DecodeError::Unsupported(_)), "{err:?}");
        assert_eq!(
            err.to_string(),
            format!("{rawler} (embedded preview: {fallback})")
        );
        let rawler = develop_probe(&path, 1, RawTone::Srgb).unwrap_err();
        let err = raw_probe(&path, 1, RawTone::Srgb).unwrap_err();
        assert!(matches!(err, DecodeError::Unsupported(_)), "{err:?}");
        assert_eq!(
            err.to_string(),
            format!("{rawler} (embedded preview: {fallback})")
        );
    }

    /// `bytes` with noise over a quarter of what follows `from` (a fraction
    /// of the length): in these RAWs, sample data, after every header.
    fn noise_after(bytes: &[u8], from: f64) -> Vec<u8> {
        let mut out = bytes.to_vec();
        let start = (out.len() as f64 * from) as usize;
        let end = start + (out.len() - start) / 4;
        let mut state = 0x9E37_79B9_u32;
        for byte in &mut out[start..end] {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *byte = state as u8;
        }
        out
    }

    #[test]
    fn raw_full_errors_when_only_the_samples_are_corrupt() {
        let Some(raws) = fixture_raws("rawsamples") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        // A rawler panic (NEF) and a rawler error (CR3) in the sample decoder.
        for want in ["DSC_1114.NEF", "IMG_4244.CR3"] {
            let path = raws.iter().find(|p| name(p) == want).unwrap();
            let broken = dir.path().join(want);
            fs::write(&broken, noise_after(&fs::read(path).unwrap(), 0.7)).unwrap();
            // The headers are whole, so the probe promises a 16-bit develop.
            let probed = probe_of(&broken, 1, RawTone::Linear);
            assert!(probed.sixteen_bit, "{want}");
            assert_eq!(probed.icc, Some(rgb_profile(RawTone::Linear)), "{want}");
            // The JPEG is whole too: it once stood in for the develop.
            assert!(matches!(preview_full(&broken, 1), Ok(Some(_))), "{want}");
            let err = raw_full(&broken, 1, RawTone::Linear).unwrap_err();
            println!("{want}: {err}");
            assert!(matches!(err, DecodeError::Corrupt(_)), "{want}: {err:?}");
        }
    }
}
