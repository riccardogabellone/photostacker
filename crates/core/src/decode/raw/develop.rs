//! rawler's develop: the binned develop of the thumbnails (see "Chosen
//! calls" in `thumb.rs`), and the size and samples of the full-resolution
//! develop (see `full.rs`).

use image::RgbImage;
use rawler::cfa::CFA;
use rawler::imgop::develop::{Intermediate, ProcessingStep, RawDevelop};
use rawler::imgop::sensor::SensorType;
use rawler::imgop::{Dim2, Point, Rect};
use rawler::rawimage::{
    BlackLevel, RawImage, RawImageData, RawPhotometricInterpretation, WhiteLevel,
};

use super::classify;
use crate::decode::{DecodeError, to_rgb8};

/// The most colour planes a sensor has: three, or four for CYGM and RGBE.
const MAX_PLANES: usize = 4;

/// A rectangle of the raw buffer, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Region {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

/// rawler's default develop without `FujiRotate` (a SuperCCD RAW is refused)
/// and without `SRgb`, which `develop_full` adds for
/// [`RawTone::Srgb`](crate::model::RawTone::Srgb).
pub(super) const DEVELOP_STEPS: [ProcessingStep; 6] = [
    ProcessingStep::Rescale,
    ProcessingStep::Demosaic,
    ProcessingStep::CropActiveArea,
    ProcessingStep::WhiteBalance,
    ProcessingStep::Calibrate,
    ProcessingStep::CropDefault,
];

/// The size `develop_intermediate` gives `raw` with [`DEVELOP_STEPS`],
/// worked out from its headers the way rawler 0.8 does it. What rawler would
/// panic on (`todo!`, failed asserts, slices out of bounds) is an error here
/// instead, so that the probe and the develop refuse the same files.
pub(super) fn developed_size(raw: &RawImage) -> Result<Dim2, DecodeError> {
    if raw.fuji_rotation_width.is_some() {
        return Err(DecodeError::Unsupported(
            "Fujifilm SuperCCD RAW (45° sensor)".into(),
        ));
    }
    if !matches!(raw.cpp, 1 | 3 | 4) {
        return Err(DecodeError::Unsupported(format!(
            "RAW with {} samples per pixel",
            raw.cpp
        )));
    }
    let whole = Rect::new(Point::zero(), raw.dim());
    // A one-sample CFA image is demosaiced over the active area; anything
    // else keeps the size of the buffer.
    let demosaiced = match &raw.photometric {
        RawPhotometricInterpretation::BlackIsZero => {
            return Err(DecodeError::Unsupported("monochrome RAW".into()));
        }
        RawPhotometricInterpretation::Cfa(config) if raw.cpp == 1 => {
            let cfa = &config.cfa;
            let supported = match config.sensor {
                SensorType::Bayer => {
                    cfa.is_rgb() || (cfa.unique_colors() == 4 && config.colors.plane_count() == 4)
                }
                SensorType::Xtrans => cfa.is_rgb(),
            };
            if !supported {
                return Err(DecodeError::Unsupported(format!(
                    "{:?} sensor with CFA {cfa}",
                    config.sensor
                )));
            }
            true
        }
        _ => false,
    };
    let mut size = if demosaiced {
        let roi = raw.active_area.unwrap_or(whole);
        if !inside(roi, raw.dim()) {
            return Err(corrupt_area("active", roi));
        }
        roi.d
    } else {
        raw.dim()
    };
    // CropDefault.
    if let Some(mut crop) = raw.crop_area.or(raw.active_area) {
        // With Demosaic and CropActiveArea among the steps, rawler takes the
        // crop relative to the active area, whether it demosaiced or not.
        if let Some(active) = raw.active_area {
            let within = crop.p.x >= active.p.x
                && crop.p.y >= active.p.y
                && crop.d.w <= active.d.w
                && crop.d.h <= active.d.h;
            if !within {
                return Err(corrupt_area("crop", crop));
            }
            crop = crop.adapt(&active);
        }
        // rawler's check for a half-size (superpixel) demosaic.
        if size.w == raw.active_area.map_or(raw.dim(), |area| area.d).w / 2 {
            crop.scale(0.5);
        }
        if crop.d != size {
            if !inside(crop, size) {
                return Err(corrupt_area("crop", crop));
            }
            size = crop.d;
        }
    }
    if size.w == 0 || size.h == 0 {
        return Err(DecodeError::Corrupt("the RAW image area is empty".into()));
    }
    Ok(size)
}

/// `area` lies within an image of `size`.
fn inside(area: Rect, size: Dim2) -> bool {
    area.p.x.checked_add(area.d.w).is_some_and(|r| r <= size.w)
        && area.p.y.checked_add(area.d.h).is_some_and(|b| b <= size.h)
}

fn corrupt_area(which: &str, area: Rect) -> DecodeError {
    DecodeError::Corrupt(format!("the RAW {which} area {area:?} is out of bounds"))
}

/// rawler's develop as interleaved 16-bit RGB, and its size: each sample is
/// `round(clamp(v, 0, 1) × 65535)`, and a monochrome develop becomes gray RGB.
pub(super) fn rgb16(developed: Intermediate) -> Result<(Dim2, Vec<u16>), DecodeError> {
    let quantize = |v: f32| (v.clamp(0.0, 1.0) * 65535.0).round() as u16;
    match developed {
        Intermediate::ThreeColor(pixels) => {
            let samples = pixels.data.as_flattened().iter();
            Ok((pixels.dim(), samples.map(|&v| quantize(v)).collect()))
        }
        Intermediate::Monochrome(pixels) => {
            let mut samples = Vec::with_capacity(pixels.data.len() * 3);
            for &v in &pixels.data {
                samples.extend([quantize(v); 3]);
            }
            Ok((pixels.dim(), samples))
        }
        // Calibrate maps four planes to RGB; it cannot get here.
        Intermediate::FourColor(_) => Err(DecodeError::Corrupt(
            "the developed RAW has four colour planes".into(),
        )),
    }
}

/// Names the plane of a sample and scales its value to 0..1, from the
/// sample's position in the whole buffer (row, column, sample within the
/// pixel) and its raw value. `None` skips the sample.
type Sampler = Box<dyn Fn(usize, usize, usize, f32) -> Option<(usize, f32)>>;

/// Develops `raw` binned down to about `2 × max_side` on its long side:
/// binning of the raw samples for the demosaic, then rawler's white balance,
/// camera matrix and sRGB curve.
pub(super) fn develop_binned(mut raw: RawImage, max_side: u32) -> Result<RgbImage, DecodeError> {
    if raw.fuji_rotation_width.is_some() {
        return Err(DecodeError::Unsupported(
            "Fujifilm SuperCCD RAW (45° sensor) without an embedded preview".into(),
        ));
    }
    let (base, planes, sampler): (usize, usize, Sampler) = match &raw.photometric {
        RawPhotometricInterpretation::Cfa(config) => {
            let base = match config.sensor {
                SensorType::Bayer => 2,
                SensorType::Xtrans => 3,
            };
            let planes = config.colors.plane_count();
            let sampler = cfa_sampler(
                config.cfa.clone(),
                config.colors.plane_lookup_table(),
                planes,
                raw.blacklevel.as_bayer_array(),
                raw.whitelevel.as_bayer_array(),
            )?;
            (base, planes, sampler)
        }
        RawPhotometricInterpretation::LinearRaw => {
            let sampler = linear_sampler(
                raw.blacklevel.as_vec(),
                raw.whitelevel.as_vec(),
                raw.width,
                raw.cpp,
            )?;
            (2, raw.cpp, sampler)
        }
        RawPhotometricInterpretation::BlackIsZero => {
            return Err(DecodeError::Unsupported(
                "monochrome RAW without an embedded preview".into(),
            ));
        }
    };
    if !(1..=MAX_PLANES).contains(&planes) {
        return Err(DecodeError::Unsupported(format!(
            "RAW with {planes} colour planes"
        )));
    }
    let len = match &raw.data {
        RawImageData::Integer(pixels) => pixels.len(),
        RawImageData::Float(pixels) => pixels.len(),
    };
    let needed = raw
        .width
        .checked_mul(raw.height)
        .and_then(|n| n.checked_mul(raw.cpp));
    if needed.is_none_or(|n| len < n) {
        return Err(DecodeError::Corrupt(
            "the RAW buffer is smaller than its size".into(),
        ));
    }
    let region = crop_region(&raw);
    let block = base * bin_factor(region.width.max(region.height), base, max_side);
    let (binned, width, height) = match &raw.data {
        RawImageData::Integer(pixels) => {
            bin(pixels, raw.width, raw.cpp, region, block, planes, sampler)
        }
        RawImageData::Float(pixels) => {
            bin(pixels, raw.width, raw.cpp, region, block, planes, sampler)
        }
    };
    if width == 0 || height == 0 {
        return Err(DecodeError::Corrupt("the RAW image area is empty".into()));
    }
    // This drops the full-size samples; from here on everything is binned size.
    raw.data = RawImageData::Float(binned);
    raw.width = width;
    raw.height = height;
    raw.cpp = planes;
    raw.photometric = RawPhotometricInterpretation::LinearRaw;
    raw.active_area = None;
    raw.crop_area = None;
    // The binned planes are already scaled, as after `apply_scaling`.
    raw.blacklevel = BlackLevel::zero(1, 1, planes);
    raw.whitelevel = WhiteLevel::new(vec![1; planes]);
    let steps = [
        ProcessingStep::WhiteBalance,
        ProcessingStep::Calibrate,
        ProcessingStep::SRgb,
    ];
    let developed = RawDevelop::new_with(&steps)
        .develop_intermediate(&raw)
        .map_err(classify)?;
    let image = developed
        .to_dynamic_image()
        .ok_or_else(|| DecodeError::Corrupt("the developed RAW has no pixels".into()))?;
    Ok(to_rgb8(image))
}

/// The CFA sampler: the plane is `lookup` of the CFA colour, and the levels
/// repeat over the 2×2 positions, as in rawler's `apply_scaling`
/// (`correct_blacklevel_cfa`). A colour that is no plane (lookup 255) is skipped.
fn cfa_sampler(
    cfa: CFA,
    lookup: [usize; 256],
    planes: usize,
    black: [f32; 4],
    white: [f32; 4],
) -> Result<Sampler, DecodeError> {
    let range = level_ranges(&black, &white)?;
    Ok(Box::new(move |row, col, _, value| {
        let plane = lookup[cfa.color_at(row, col)];
        let at = (row % 2) * 2 + col % 2;
        (plane < planes).then(|| (plane, scaled(value, black[at], range[at])))
    }))
}

/// The `LinearRaw` sampler: sample `s` is plane `s`, and the levels repeat
/// over the flat sample sequence, as in rawler's `apply_scaling`
/// (`correct_blacklevel`).
fn linear_sampler(
    black: Vec<f32>,
    white: Vec<f32>,
    width: usize,
    cpp: usize,
) -> Result<Sampler, DecodeError> {
    if black.is_empty() || black.len() != white.len() {
        // rawler skips the scaling then, which leaves the samples unusable.
        return Err(DecodeError::Unsupported(format!(
            "LinearRaw RAW with {} black and {} white levels",
            black.len(),
            white.len()
        )));
    }
    let range = level_ranges(&black, &white)?;
    Ok(Box::new(move |row, col, sample, value| {
        let at = ((row * width + col) * cpp + sample) % black.len();
        Some((sample, scaled(value, black[at], range[at])))
    }))
}

/// `white − black` per level; an empty range would divide by zero.
fn level_ranges(black: &[f32], white: &[f32]) -> Result<Vec<f32>, DecodeError> {
    let ranges: Vec<f32> = white.iter().zip(black).map(|(w, b)| w - b).collect();
    if ranges.iter().all(|&r| r > 0.0 && r.is_finite()) {
        Ok(ranges)
    } else {
        Err(DecodeError::Corrupt(format!(
            "RAW white levels {white:?} are not above black levels {black:?}"
        )))
    }
}

/// rawler's scaling: below black clips to 0, white maps to 1, above is kept.
fn scaled(value: f32, black: f32, range: f32) -> f32 {
    (value - black).max(0.0) / range
}

/// The largest `k ≥ 1` for which a `base·k` cell keeps the binned long side
/// at least `2 × max_side`; 1 when even `k = 1` falls short.
fn bin_factor(long_side: usize, base: usize, max_side: u32) -> usize {
    let target = 2 * max_side.max(1) as usize;
    (long_side / (base * target)).max(1)
}

/// The crop area, else the active area, else the whole buffer; clipped to the
/// buffer. rawler keeps both areas in raw-buffer pixels.
fn crop_region(raw: &RawImage) -> Region {
    let whole = Rect::new(Point::zero(), raw.dim());
    let rect = raw.crop_area.or(raw.active_area).unwrap_or(whole);
    let x = rect.p.x.min(raw.width);
    let y = rect.p.y.min(raw.height);
    Region {
        x,
        y,
        width: rect.d.w.min(raw.width - x),
        height: rect.d.h.min(raw.height - y),
    }
}

/// Averages every plane over the `block`×`block` cells of `region`; the
/// cells that do not fit whole are dropped.
///
/// - `pixels` holds `cpp` samples per pixel and `width` pixels per row.
/// - `sample_at(row, col, sample, value)` gives a sample's plane and scaled
///   value, or `None` to skip it. Row and column are positions in the whole
///   buffer, so the CFA keeps its phase. Planes are below `planes`, which is
///   at most [`MAX_PLANES`].
///
/// Returns the cells' planes interleaved, then the cell grid's width and
/// height. A plane missing from a cell averages to 0. That cannot happen with
/// Bayer cells of side 2k or X-Trans cells of side 3k, wherever they start.
fn bin<T: Copy + Into<f32>>(
    pixels: &[T],
    width: usize,
    cpp: usize,
    region: Region,
    block: usize,
    planes: usize,
    sample_at: impl Fn(usize, usize, usize, f32) -> Option<(usize, f32)>,
) -> (Vec<f32>, usize, usize) {
    let (cells_w, cells_h) = (region.width / block, region.height / block);
    let mut out = Vec::with_capacity(cells_w * cells_h * planes);
    for cy in 0..cells_h {
        let rows = region.y + cy * block..region.y + (cy + 1) * block;
        for cx in 0..cells_w {
            let cols = region.x + cx * block..region.x + (cx + 1) * block;
            let mut sum = [0.0_f32; MAX_PLANES];
            let mut count = [0_u32; MAX_PLANES];
            for row in rows.clone() {
                for col in cols.clone() {
                    let at = (row * width + col) * cpp;
                    for (sample, &value) in pixels[at..at + cpp].iter().enumerate() {
                        if let Some((plane, value)) = sample_at(row, col, sample, value.into()) {
                            sum[plane] += value;
                            count[plane] += 1;
                        }
                    }
                }
            }
            out.extend((0..planes).map(|p| match count[p] {
                0 => 0.0,
                n => sum[p] / n as f32,
            }));
        }
    }
    (out, cells_w, cells_h)
}

#[cfg(test)]
mod tests {
    use rawler::cfa::PlaneColor;
    use rawler::decoders::Camera;
    use rawler::pixarray::{Color2D, PixF32};
    use rawler::rawimage::CFAConfig;

    use super::*;

    /// A 4×4 RGGB mosaic whose samples encode their position: 10·row + col.
    fn positional(width: usize, height: usize) -> Vec<f32> {
        (0..width * height)
            .map(|i| (10 * (i / width) + i % width) as f32)
            .collect()
    }

    /// RGGB plane of a position, the value passed through.
    fn rggb(row: usize, col: usize, _sample: usize, value: f32) -> Option<(usize, f32)> {
        let plane = match (row % 2, col % 2) {
            (0, 0) => 0,
            (1, 1) => 2,
            _ => 1,
        };
        Some((plane, value))
    }

    fn whole(width: usize, height: usize) -> Region {
        Region {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    #[test]
    fn bayer_bins_two_by_two_and_averages_the_greens() {
        let pixels = positional(4, 4);
        let (out, w, h) = bin(&pixels, 4, 1, whole(4, 4), 2, 3, rggb);
        assert_eq!((w, h), (2, 2));
        // Cell (0, 0): R = p(0,0), G = mean(p(0,1), p(1,0)), B = p(1,1).
        assert_eq!(&out[..3], &[0.0, 5.5, 11.0]);
        // Cell (1, 1): R = p(2,2), G = mean(p(2,3), p(3,2)), B = p(3,3).
        assert_eq!(&out[9..], &[22.0, 27.5, 33.0]);
    }

    #[test]
    fn bayer_block_four_averages_every_sample_of_a_plane() {
        let pixels = positional(4, 4);
        let (out, w, h) = bin(&pixels, 4, 1, whole(4, 4), 4, 3, rggb);
        assert_eq!((w, h), (1, 1));
        // R at (0,0) (0,2) (2,0) (2,2); B at (1,1) (1,3) (3,1) (3,3); G the
        // other eight, which sum to 264 − 44 − 88.
        assert_eq!(out, [11.0, 16.5, 22.0]);
    }

    #[test]
    fn bin_region_drops_partial_cells_and_keeps_absolute_cfa_phase() {
        let pixels = positional(6, 5);
        // Starts on a B pixel (row 1, col 1 of RGGB) and has an odd size.
        let region = Region {
            x: 1,
            y: 1,
            width: 5,
            height: 3,
        };
        let (out, w, h) = bin(&pixels, 6, 1, region, 2, 3, rggb);
        assert_eq!((w, h), (2, 1));
        // Cell (0, 0) covers rows 1-2, cols 1-2: B = p(1,1), R = p(2,2).
        assert_eq!(out[0], 22.0);
        assert_eq!(out[1], (12.0 + 21.0) / 2.0);
        assert_eq!(out[2], 11.0);
    }

    #[test]
    fn xtrans_bins_three_by_three_and_six_by_six() {
        const XTRANS: [&str; 6] = ["GGRGGB", "GGBGGR", "BRGRBG", "GGBGGR", "GGRGGB", "RBGBRG"];
        let plane = |row: usize, col: usize| match XTRANS[row % 6].as_bytes()[col % 6] {
            b'R' => 0,
            b'G' => 1,
            _ => 2,
        };
        let sample_at = |row, col, _, value| Some((plane(row, col), value));
        // Red samples are 1, green 2, blue 3: every cell averages to (1, 2, 3).
        let pixels: Vec<f32> = (0..36).map(|i| plane(i / 6, i % 6) as f32 + 1.0).collect();
        let (out, w, h) = bin(&pixels, 6, 1, whole(6, 6), 3, 3, sample_at);
        assert_eq!((w, h), (2, 2));
        assert_eq!(out, [1.0, 2.0, 3.0].repeat(4));
        let (out, w, h) = bin(&pixels, 6, 1, whole(6, 6), 6, 3, sample_at);
        assert_eq!((w, h), (1, 1));
        assert_eq!(out, [1.0, 2.0, 3.0]);
    }

    #[test]
    fn linear_raw_bins_each_channel() {
        // 2×2 pixels of 3 interleaved channels: channel c of pixel i is 100·c + i.
        let pixels: Vec<f32> = (0..4)
            .flat_map(|i| (0..3).map(move |c| (100 * c + i) as f32))
            .collect();
        let pass = |_, _, c, value| Some((c, value));
        let (out, w, h) = bin(&pixels, 2, 3, whole(2, 2), 2, 3, pass);
        assert_eq!((w, h), (1, 1));
        assert_eq!(out, [1.5, 101.5, 201.5]);
    }

    #[test]
    fn bin_reads_u16_samples_through_the_cfa_sampler() {
        let rggb = cfa_sampler(
            CFA::new("RGGB"),
            PlaneColor::new("RGB").plane_lookup_table(),
            3,
            [100.0; 4],
            [1100.0; 4],
        )
        .unwrap();
        // R 600, G 700 and 300, B 1100.
        let pixels: [u16; 4] = [600, 700, 300, 1100];
        let (out, w, h) = bin(&pixels, 2, 1, whole(2, 2), 2, 3, rggb);
        assert_eq!((w, h), (1, 1));
        assert_eq!(out, [0.5, 0.4, 1.0]);
    }

    #[test]
    fn cfa_sampler_scales_by_bayer_position_like_apply_scaling() {
        let sample = cfa_sampler(
            CFA::new("RGGB"),
            PlaneColor::new("RGB").plane_lookup_table(),
            3,
            [100.0, 200.0, 300.0, 400.0],
            [1100.0, 1200.0, 1300.0, 1400.0],
        )
        .unwrap();
        assert_eq!(sample(0, 0, 0, 600.0), Some((0, 0.5)));
        assert_eq!(sample(0, 1, 0, 700.0), Some((1, 0.5)));
        assert_eq!(sample(1, 0, 0, 100.0), Some((1, 0.0)));
        assert_eq!(sample(3, 3, 0, 1900.0), Some((2, 1.5)));
        assert_eq!(sample(2, 2, 0, 1100.0), Some((0, 1.0)));
        // A colour that is no plane is skipped.
        let red_green = cfa_sampler(
            CFA::new("RGGB"),
            PlaneColor::new("RG").plane_lookup_table(),
            2,
            [0.0; 4],
            [1.0; 4],
        )
        .unwrap();
        assert_eq!(red_green(1, 1, 0, 0.5), None);
    }

    #[test]
    fn linear_sampler_repeats_levels_over_the_samples() {
        let sample =
            linear_sampler(vec![10.0, 20.0, 30.0], vec![110.0, 120.0, 130.0], 2, 3).unwrap();
        assert_eq!(sample(0, 1, 2, 80.0), Some((2, 0.5)));
        assert_eq!(sample(1, 0, 0, 5.0), Some((0, 0.0)));
        let single = linear_sampler(vec![0.0], vec![1000.0], 2, 3).unwrap();
        assert_eq!(single(1, 1, 1, 250.0), Some((1, 0.25)));
        assert!(matches!(
            linear_sampler(vec![0.0, 0.0], vec![1.0], 2, 3),
            Err(DecodeError::Unsupported(_))
        ));
    }

    #[test]
    fn white_not_above_black_is_corrupt() {
        let lookup = PlaneColor::new("RGB").plane_lookup_table();
        let flat = cfa_sampler(CFA::new("RGGB"), lookup, 3, [50.0; 4], [50.0; 4]);
        assert!(matches!(flat, Err(DecodeError::Corrupt(_))));
    }

    #[test]
    fn bin_factor_keeps_twice_max_side() {
        // (long side, base, max_side, k)
        for (long, base, max_side, k) in [
            (6000, 2, 512, 2),  // 1500 ≥ 1024; k = 3 would give 1000
            (4000, 2, 512, 1),  // 2000; k = 2 would give 1000
            (7728, 3, 512, 2),  // 1288; k = 3 would give 858
            (14204, 2, 512, 6), // 150 MP IIQ: 1183
            (800, 2, 512, 1),   // already short: still binned once
            (6000, 2, 1024, 1), // 3000; k = 2 would give 1500
        ] {
            assert_eq!(
                bin_factor(long, base, max_side),
                k,
                "{long} {base} {max_side}"
            );
        }
    }

    /// A `width × height` RAW image of `cpp` zero samples per pixel, with
    /// `photometric`, full-range levels and no active or crop area.
    fn raw_image_of(
        width: usize,
        height: usize,
        cpp: usize,
        photometric: RawPhotometricInterpretation,
    ) -> RawImage {
        RawImage::new_with_data(
            Camera::default(),
            RawImageData::Integer(vec![0; width * height * cpp]),
            width * cpp,
            height,
            cpp,
            [1.0; 4],
            photometric,
            Some(BlackLevel::zero(1, 1, cpp)),
            Some(WhiteLevel::new(vec![65535; cpp])),
            false,
        )
    }

    fn cfa(pattern: &str, planes: &str) -> RawPhotometricInterpretation {
        RawPhotometricInterpretation::Cfa(CFAConfig::new(
            &CFA::new(pattern),
            &PlaneColor::new(planes),
        ))
    }

    fn rect(x: usize, y: usize, w: usize, h: usize) -> Rect {
        Rect::new(Point::new(x, y), Dim2::new(w, h))
    }

    #[test]
    fn developed_size_follows_the_active_and_crop_areas() {
        let mut bayer = raw_image_of(100, 80, 1, cfa("RGGB", "RGB"));
        assert_eq!(developed_size(&bayer).unwrap(), Dim2::new(100, 80));
        // A CFA image is demosaiced over its active area.
        bayer.active_area = Some(rect(4, 2, 90, 70));
        assert_eq!(developed_size(&bayer).unwrap(), Dim2::new(90, 70));
        // The crop is in buffer pixels, inside the active area.
        bayer.crop_area = Some(rect(10, 6, 60, 40));
        assert_eq!(developed_size(&bayer).unwrap(), Dim2::new(60, 40));
        // A four-colour Bayer with four planes is supported too.
        let cygm = raw_image_of(100, 80, 1, cfa("CYGM", "CYGM"));
        assert_eq!(developed_size(&cygm).unwrap(), Dim2::new(100, 80));
        // A LinearRaw image keeps its buffer, then takes its crop.
        let mut linear = raw_image_of(100, 80, 3, RawPhotometricInterpretation::LinearRaw);
        linear.active_area = Some(rect(4, 2, 90, 70));
        assert_eq!(developed_size(&linear).unwrap(), Dim2::new(90, 70));
    }

    #[test]
    fn developed_size_refuses_what_the_develop_would_panic_on() {
        let bayer = || raw_image_of(100, 80, 1, cfa("RGGB", "RGB"));
        let with = |change: &dyn Fn(&mut RawImage)| {
            let mut raw = bayer();
            change(&mut raw);
            developed_size(&raw).unwrap_err()
        };
        let unsupported = [
            (
                with(&|raw| raw.fuji_rotation_width = Some(50)),
                "Fujifilm SuperCCD RAW (45° sensor)",
            ),
            (
                developed_size(&raw_image_of(
                    10,
                    10,
                    2,
                    RawPhotometricInterpretation::LinearRaw,
                ))
                .unwrap_err(),
                "RAW with 2 samples per pixel",
            ),
            (
                with(&|raw| raw.photometric = RawPhotometricInterpretation::BlackIsZero),
                "monochrome RAW",
            ),
            // Four colours on three planes.
            (
                with(&|raw| raw.photometric = cfa("RGBE", "RGB")),
                "Bayer sensor with CFA ",
            ),
        ];
        for (err, want) in unsupported {
            assert!(
                matches!(&err, DecodeError::Unsupported(why) if why.starts_with(want)),
                "{want}: {err:?}"
            );
        }
        let corrupt = [
            // An active area past the buffer's right edge.
            (
                with(&|raw| raw.active_area = Some(rect(4, 2, 100, 70))),
                "the RAW active area",
            ),
            // A crop that starts left of the active area.
            (
                with(&|raw| {
                    raw.active_area = Some(rect(4, 2, 90, 70));
                    raw.crop_area = Some(rect(2, 6, 60, 40));
                }),
                "the RAW crop area",
            ),
            // A crop inside the active area's size but past its right edge.
            (
                with(&|raw| {
                    raw.active_area = Some(rect(4, 2, 90, 70));
                    raw.crop_area = Some(rect(10, 6, 85, 40));
                }),
                "the RAW crop area",
            ),
            (
                with(&|raw| raw.active_area = Some(rect(4, 2, 0, 70))),
                "the RAW image area is empty",
            ),
        ];
        for (err, want) in corrupt {
            assert!(
                matches!(&err, DecodeError::Corrupt(why) if why.starts_with(want)),
                "{want}: {err:?}"
            );
        }
    }

    #[test]
    fn rgb16_rounds_and_clamps_every_sample() {
        let step = 1.0 / 65535.0;
        let pixels = vec![
            [0.0, 1.0, 0.5],
            // Below 0 and above 1 clamp; NaN becomes 0.
            [-0.25, 1.5, f32::NAN],
            // To the nearest step.
            [0.3 * step, 0.7 * step, 1000.6 * step],
        ];
        let developed = Intermediate::ThreeColor(Color2D::new_with(pixels, 3, 1));
        let (size, samples) = rgb16(developed).unwrap();
        assert_eq!(size, Dim2::new(3, 1));
        assert_eq!(samples, [0, 65535, 32768, 0, 65535, 0, 0, 1, 1001]);
    }

    #[test]
    fn rgb16_makes_a_monochrome_develop_gray_rgb() {
        let developed = Intermediate::Monochrome(PixF32::new_with(vec![0.25, 1.0, 2.0], 3, 1));
        let (size, samples) = rgb16(developed).unwrap();
        assert_eq!(size, Dim2::new(3, 1));
        assert_eq!(
            samples,
            [
                16384, 16384, 16384, 65535, 65535, 65535, 65535, 65535, 65535
            ]
        );
        // Calibrate never leaves four planes; if it did, that is an error.
        let four = Intermediate::FourColor(Color2D::new_with(vec![[0.5; 4]], 1, 1));
        assert!(matches!(rgb16(four), Err(DecodeError::Corrupt(_))));
    }
}
