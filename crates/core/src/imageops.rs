// Parts of this file are ported from Pillow 12.2.0: `src/libImaging/Resample.c`
// (the convolution resample), `src/libImaging/Reduce.c` (the box reduce) and
// `src/PIL/Image.py` (the thumbnail size and reduce factor).
//
//     Copyright © 1997-2011 by Secret Labs AB
//     Copyright © 1995-2011 by Fredrik Lundh and contributors
//     Copyright © 2010 by Jeffrey 'Alex' Clark and contributors
//
// Pillow is licensed under the MIT-CMU License.
// See THIRD-PARTY-NOTICES.md for the full licence.

//! Pixel operations that reproduce Pillow, so hashes computed here match the
//! ones Python's `imagehash` computes on a Pillow thumbnail.

use std::borrow::Cow;
use std::ops::Range;

use image::{GrayImage, Luma, RgbImage};

/// The `reducing_gap` default of `PIL.Image.thumbnail`.
const REDUCING_GAP: f64 = 2.0;

/// `PIL.Image.thumbnail((max_side, max_side))` with its defaults, BICUBIC and
/// `reducing_gap=2.0`. Returns a copy when both sides already fit.
///
/// Like Pillow it shrinks in two steps: an integer box [`reduce`] that keeps
/// at least `reducing_gap` times the target size, then a bicubic resample.
pub fn pillow_thumbnail(img: &RgbImage, max_side: u32) -> RgbImage {
    let (width, height) = img.dimensions();
    let Some((target_w, target_h)) = thumbnail_size(width, height, max_side) else {
        return img.clone();
    };
    let fx = reduce_factor(width, target_w);
    let fy = reduce_factor(height, target_h);
    if fx == 1 && fy == 1 {
        return resample(img, width as f32, height as f32, target_w, target_h);
    }
    // Pillow then resamples the box (0, 0, width / fx, height / fy) of the
    // reduced image, not the whole of it: when fx does not divide the width,
    // the last reduced column (a partial block) only counts for the fraction
    // of a pixel it stands for. Pillow hands that box to C as 32-bit floats.
    let reduced = reduce(img, fx, fy);
    let box_w = (f64::from(width) / f64::from(fx)) as f32;
    let box_h = (f64::from(height) / f64::from(fy)) as f32;
    resample(&reduced, box_w, box_h, target_w, target_h)
}

/// Pillow's `convert("L")`: ITU-R 601-2 luma in 16-bit fixed point.
pub fn luma_pillow(img: &RgbImage) -> GrayImage {
    GrayImage::from_fn(img.width(), img.height(), |x, y| {
        let [r, g, b] = img.get_pixel(x, y).0.map(u32::from);
        let luma = (r * 19595 + g * 38470 + b * 7471 + 0x8000) >> 16;
        Luma([luma as u8])
    })
}

/// `PIL.Image.resize((width, height), LANCZOS)` of an `L` image, as imagehash
/// calls it: no `reducing_gap`, so a single Lanczos-3 convolution of the
/// whole image. A resize to the same size is a copy, as in Pillow.
///
/// # Panics
///
/// If `height` is 0. A `width` of 0 panics too in debug builds (a debug
/// assertion); a release build returns an empty image.
pub fn pillow_resize_lanczos(img: &GrayImage, width: u32, height: u32) -> GrayImage {
    let (in_w, in_h) = img.dimensions();
    let (box_w, box_h) = (in_w as f32, in_h as f32);
    let pixels = pillow_resample::<1>(
        img.as_raw(),
        in_w,
        in_h,
        box_w,
        box_h,
        width,
        height,
        LANCZOS,
    );
    GrayImage::from_raw(width, height, pixels).expect("the resample fills width × height")
}

/// The size `PIL.Image.thumbnail` shrinks a `width`×`height` image to, or
/// `None` when it already fits in `max_side`×`max_side`. Verbatim from
/// Pillow's `preserve_aspect_ratio`, Python float semantics included.
fn thumbnail_size(width: u32, height: u32, max_side: u32) -> Option<(u32, u32)> {
    // Pillow divides by zero on a zero size; a 1-pixel box is the closest sane reading.
    let max_side = max_side.max(1);
    if (max_side >= width && max_side >= height) || width == 0 || height == 0 {
        return None;
    }
    let side = f64::from(max_side);
    let aspect = f64::from(width) / f64::from(height);
    // Pillow's `x / y >= aspect`, with x == y == max_side.
    if 1.0 >= aspect {
        let w = round_aspect(side * aspect, |n| (aspect - n / side).abs());
        Some((w, max_side))
    } else {
        let h = round_aspect(side / aspect, |n| {
            if n == 0.0 {
                0.0
            } else {
                (aspect - side / n).abs()
            }
        });
        Some((max_side, h))
    }
}

/// `max(min(floor(v), ceil(v), key=key), 1)`; on a tie Python's `min` keeps
/// the first candidate, the floor.
fn round_aspect(value: f64, key: impl Fn(f64) -> f64) -> u32 {
    let (floor, ceil) = (value.floor(), value.ceil());
    let best = if key(ceil) < key(floor) { ceil } else { floor };
    (best as u32).max(1)
}

/// `int(src / target / reducing_gap) or 1`, as in `PIL.Image.resize`.
fn reduce_factor(src: u32, target: u32) -> u32 {
    ((f64::from(src) / f64::from(target) / REDUCING_GAP) as u32).max(1)
}

/// `PIL.Image.reduce((fx, fy))`: the mean of each `fx`×`fy` block. The blocks
/// of the last column and row may be partial; they average the pixels present.
///
/// The arithmetic is Pillow's `Reduce.c` for 8-bit images: the sum plus half
/// the pixel count, times a 24-bit fixed-point reciprocal of that count.
fn reduce(img: &RgbImage, fx: u32, fy: u32) -> RgbImage {
    let (width, height) = img.dimensions();
    let src = img.as_raw();
    let row_len = width as usize * 3;
    RgbImage::from_fn(width.div_ceil(fx), height.div_ceil(fy), |ox, oy| {
        let (x0, y0) = (ox * fx, oy * fy);
        let (x1, y1) = ((x0 + fx).min(width), (y0 + fy).min(height));
        let count = (x1 - x0) * (y1 - y0);
        let mut sums = [count / 2; 3];
        for y in y0..y1 {
            let row = &src[y as usize * row_len..][..row_len];
            let (pixels, _) = row[x0 as usize * 3..x1 as usize * 3].as_chunks::<3>();
            for pixel in pixels {
                for (sum, &v) in sums.iter_mut().zip(pixel) {
                    *sum += u32::from(v);
                }
            }
        }
        let multiplier = pillow_reciprocal(count);
        image::Rgb(sums.map(|sum| (sum.wrapping_mul(multiplier) >> 24) as u8))
    })
}

/// `division_UINT32(count, 8)` of Pillow's `Reduce.c`: `2^32 / (256 * count)`
/// computed in single precision and truncated. For a power-of-two count it is
/// exact, which matches the shift Pillow's 2×2 and 4×4 fast paths use.
fn pillow_reciprocal(count: u32) -> u32 {
    (4_294_967_296.0_f32 / (256 * count) as f32) as u32
}

/// Pillow's BICUBIC (Catmull-Rom, `a = -0.5`) resample of the box
/// `(0, 0, box_w, box_h)` of `src` to `target_w`×`target_h`.
fn resample(src: &RgbImage, box_w: f32, box_h: f32, target_w: u32, target_h: u32) -> RgbImage {
    let (in_w, in_h) = src.dimensions();
    let pixels = pillow_resample::<3>(
        src.as_raw(),
        in_w,
        in_h,
        box_w,
        box_h,
        target_w,
        target_h,
        BICUBIC,
    );
    RgbImage::from_raw(target_w, target_h, pixels).expect("the resample fills target_w × target_h")
}

/// A resampling kernel of Pillow's `Resample.c` and its support radius.
#[derive(Clone, Copy)]
struct Filter {
    kernel: fn(f64) -> f64,
    support: f64,
}

const BICUBIC: Filter = Filter {
    kernel: bicubic_kernel,
    support: 2.0,
};

const LANCZOS: Filter = Filter {
    kernel: lanczos_kernel,
    support: 3.0,
};

fn bicubic_kernel(x: f64) -> f64 {
    const A: f64 = -0.5;
    let x = x.abs();
    if x < 1.0 {
        ((A + 2.0) * x - (A + 3.0)) * x * x + 1.0
    } else if x < 2.0 {
        (((x - 5.0) * x + 8.0) * x - 4.0) * A
    } else {
        0.0
    }
}

fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        return 1.0;
    }
    let x = x * std::f64::consts::PI;
    x.sin() / x
}

fn lanczos_kernel(x: f64) -> f64 {
    if (-3.0..3.0).contains(&x) {
        sinc(x) * sinc(x / 3.0)
    } else {
        0.0
    }
}

/// Fractional bits of Pillow's 8-bit fixed-point coefficients: 8 bits stay
/// for the result and 2 for the overshoot of kernels with negative lobes.
const PRECISION_BITS: u32 = 32 - 8 - 2;

/// The 1-D weights of one axis: for each output pixel, its first source pixel
/// and how many it reads, then `ksize` fixed-point weights (zero-padded).
struct Coefficients {
    ksize: usize,
    bounds: Vec<(usize, usize)>,
    weights: Vec<i32>,
}

/// Pillow 12.2.0's 8-bit convolution resample (`ImagingResampleInner` with
/// `precompute_coeffs`, `normalize_coeffs_8bpc` and the `_8bpc` passes of
/// `Resample.c`) of the box `(0, 0, box_w, box_h)` of a packed `C`-channel
/// `in_w`×`in_h` image to `out_w`×`out_h` (both non-zero).
///
/// The arithmetic is Pillow's, so the output is Pillow's byte for byte: f64
/// kernel weights normalised per output pixel, rounded to 22-bit integers,
/// then i32 sums clipped to 8 bits; a horizontal pass over only the source
/// rows the vertical pass reads, then the vertical pass. An axis whose size
/// and box are unchanged is skipped.
#[allow(clippy::too_many_arguments)]
fn pillow_resample<const C: usize>(
    src: &[u8],
    in_w: u32,
    in_h: u32,
    box_w: f32,
    box_h: f32,
    out_w: u32,
    out_h: u32,
    filter: Filter,
) -> Vec<u8> {
    debug_assert!(out_w > 0 && out_h > 0);
    let need_horizontal = out_w != in_w || box_w != out_w as f32;
    let need_vertical = out_h != in_h || box_h != out_h as f32;
    let horizontal = precompute_coeffs(in_w, box_w, out_w, filter);
    let mut vertical = precompute_coeffs(in_h, box_h, out_h, filter);
    let first_row = vertical.bounds[0].0;
    let (last_min, last_len) = vertical.bounds[vertical.bounds.len() - 1];

    let mut pixels = Cow::Borrowed(src);
    let mut width = in_w as usize;
    if need_horizontal {
        for (min, _) in &mut vertical.bounds {
            *min -= first_row;
        }
        let rows = first_row..last_min + last_len;
        pixels = Cow::Owned(resample_horizontal::<C>(src, width, rows, &horizontal));
        width = out_w as usize;
    }
    if need_vertical {
        pixels = Cow::Owned(resample_vertical::<C>(&pixels, width, &vertical));
    }
    pixels.into_owned()
}

/// `precompute_coeffs` + `normalize_coeffs_8bpc` for the span `[0, in1)` of an
/// axis of `in_size` pixels resampled to `out_size`.
fn precompute_coeffs(in_size: u32, in1: f32, out_size: u32, filter: Filter) -> Coefficients {
    let scale = f64::from(in1) / f64::from(out_size);
    let filterscale = if scale < 1.0 { 1.0 } else { scale };
    let support = filter.support * filterscale;
    let ksize = support.ceil() as usize * 2 + 1;
    let ss = 1.0 / filterscale;
    let mut bounds = Vec::with_capacity(out_size as usize);
    let mut weights = vec![0; out_size as usize * ksize];
    let mut raw = Vec::with_capacity(ksize);
    for (xx, fixed) in weights.chunks_exact_mut(ksize).enumerate() {
        let center = (xx as f64 + 0.5) * scale;
        // C's (int) cast truncates toward zero, as `as` does.
        let xmin = ((center - support + 0.5) as i32).max(0);
        let xmax = ((center + support + 0.5) as i32).min(in_size as i32);
        let count = (xmax - xmin).max(0);
        raw.clear();
        raw.extend((0..count).map(|x| (filter.kernel)((f64::from(x + xmin) - center + 0.5) * ss)));
        let total: f64 = raw.iter().sum();
        for (slot, &w) in fixed.iter_mut().zip(&raw) {
            let w = if total != 0.0 { w / total } else { w };
            let scaled = w * f64::from(1u32 << PRECISION_BITS);
            *slot = if w < 0.0 {
                (-0.5 + scaled) as i32
            } else {
                (0.5 + scaled) as i32
            };
        }
        bounds.push((xmin as usize, count as usize));
    }
    Coefficients {
        ksize,
        bounds,
        weights,
    }
}

/// `clip8`: drop the fractional bits and saturate to 0..=255.
fn clip8(sum: i32) -> u8 {
    (sum >> PRECISION_BITS).clamp(0, 255) as u8
}

/// The horizontal pass over the source `rows` of a `width`-wide image.
fn resample_horizontal<const C: usize>(
    src: &[u8],
    width: usize,
    rows: Range<usize>,
    coeffs: &Coefficients,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(rows.len() * coeffs.bounds.len() * C);
    for y in rows {
        let row = &src[y * width * C..][..width * C];
        for (xx, &(xmin, count)) in coeffs.bounds.iter().enumerate() {
            let weights = &coeffs.weights[xx * coeffs.ksize..][..count];
            let mut sums = [1i32 << (PRECISION_BITS - 1); C];
            for (x, &w) in weights.iter().enumerate() {
                let pixel = &row[(xmin + x) * C..][..C];
                for (sum, &v) in sums.iter_mut().zip(pixel) {
                    *sum += i32::from(v) * w;
                }
            }
            out.extend(sums.map(clip8));
        }
    }
    out
}

/// The vertical pass of a `width`-wide image. Each output row accumulates
/// whole source rows; integer sums make the order irrelevant.
fn resample_vertical<const C: usize>(src: &[u8], width: usize, coeffs: &Coefficients) -> Vec<u8> {
    let row_len = width * C;
    let mut out = Vec::with_capacity(coeffs.bounds.len() * row_len);
    let mut sums = vec![0i32; row_len];
    for (yy, &(ymin, count)) in coeffs.bounds.iter().enumerate() {
        sums.fill(1 << (PRECISION_BITS - 1));
        let weights = &coeffs.weights[yy * coeffs.ksize..][..count];
        for (y, &w) in weights.iter().enumerate() {
            let row = &src[(ymin + y) * row_len..][..row_len];
            for (sum, &v) in sums.iter_mut().zip(row) {
                *sum += i32::from(v) * w;
            }
        }
        out.extend(sums.iter().map(|&sum| clip8(sum)));
    }
    out
}

#[cfg(test)]
mod tests {
    use image::Rgb;

    use super::*;

    #[test]
    fn thumbnail_target_sizes_match_pillow() {
        // Each row printed by Pillow 12.2.0:
        // Image.new('RGB', (W, H)).thumbnail((512, 512)); print(im.size)
        let cases = [
            ((6000, 4000), (512, 341)),
            ((4000, 6000), (341, 512)),
            ((513, 1), (512, 1)),
            ((300, 200), (300, 200)),
            ((1000, 333), (512, 170)),
            ((512, 512), (512, 512)),
            ((1, 1000), (1, 512)),
            ((1024, 768), (512, 384)),
            ((4032, 3024), (512, 384)),
            ((5000, 1), (512, 1)),
        ];
        for ((w, h), expected) in cases {
            let thumb = pillow_thumbnail(&RgbImage::new(w, h), 512);
            assert_eq!(thumb.dimensions(), expected, "{w}x{h}");
        }
    }

    #[test]
    fn thumbnail_of_a_small_image_is_an_unchanged_copy() {
        let img = RgbImage::from_fn(300, 200, |x, y| Rgb([x as u8, y as u8, (x ^ y) as u8]));
        assert_eq!(pillow_thumbnail(&img, 512), img);
    }

    #[test]
    fn thumbnail_of_a_flat_image_stays_flat() {
        // Reduce, the fractional box and the normalised kernel must not bend a flat field.
        let img = RgbImage::from_pixel(4031, 3023, Rgb([10, 200, 30]));
        let thumb = pillow_thumbnail(&img, 512);
        assert_eq!(thumb.dimensions(), (512, 384));
        assert!(thumb.pixels().all(|p| *p == Rgb([10, 200, 30])));
    }

    /// The 7×5 image of `reduce_matches_pillow`, built the same way in Python.
    fn reduce_fixture() -> RgbImage {
        RgbImage::from_fn(7, 5, |x, y| {
            Rgb([
                ((x * 37 + y * 11) % 256) as u8,
                ((x * 5 + y * 71 + 3) % 256) as u8,
                ((x * y * 29 + 200) % 256) as u8,
            ])
        })
    }

    #[test]
    fn reduce_matches_pillow() {
        // Pillow 12.2.0: im.reduce((3, 2)) and im.reduce((7, 5)) on `reduce_fixture`.
        // The last column (1 of 3 source columns) and row (1 of 2) are partial blocks.
        let expected: [[u8; 3]; 9] = [
            [42, 43, 172],
            [153, 58, 130],
            [228, 69, 159],
            [64, 185, 102],
            [175, 200, 149],
            [250, 211, 123],
            [81, 36, 145],
            [192, 51, 66],
            [10, 61, 128],
        ];
        let reduced = reduce(&reduce_fixture(), 3, 2);
        assert_eq!(reduced.dimensions(), (3, 3));
        let got: Vec<[u8; 3]> = reduced.pixels().map(|p| p.0).collect();
        assert_eq!(got, expected);

        let whole = reduce(&reduce_fixture(), 7, 5);
        assert_eq!(whole.dimensions(), (1, 1));
        assert_eq!(whole.get_pixel(0, 0).0, [126, 109, 133]);
    }

    #[test]
    fn luma_matches_pillow() {
        // Pillow 12.2.0: Image.convert('L') of these five pixels.
        let pixels = [
            [255, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
            [255, 255, 255],
            [12, 200, 99],
        ];
        let img = RgbImage::from_fn(5, 1, |x, _| Rgb(pixels[x as usize]));
        let luma = luma_pillow(&img);
        let got: Vec<u8> = luma.pixels().map(|Luma([v])| *v).collect();
        assert_eq!(got, [76, 150, 29, 255, 132]);
    }
}
