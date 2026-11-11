//! Deterministic synthetic scenes for the scenario tests: a photo stand-in
//! whose perceptual hashes survive an exposure change but not a reframing.
//!
//! A scene is a mostly horizontal gradient with shaded shapes painted over
//! it. Every region carries its own steep horizontal gradient, so no two
//! neighbouring areas are flat and equal (a tie a hash bit could flip on).
//! The 5-stop bracket scenario measured a max distance of 8 for seed 1 with
//! shallow, random slopes and 5 with these. All levels stay between
//! [`MIN_LEVEL`] and [`MAX_LEVEL`]: in sRGB an exposure change is close to a
//! multiplication of the levels, which the hashes ignore, and +2 EV of
//! `MAX_LEVEL` still does not clip.
//!
//! Determinism holds per platform: `exp2` and `powf` come from the platform's
//! libm, whose last-bit differences can move a pixel by one level across
//! platforms. The scenario tests therefore assert the hash distances they
//! rely on as preconditions instead of trusting fixed pixels.

use image::{Rgb, RgbImage};

/// The darkest level a scene uses.
pub const MIN_LEVEL: f32 = 48.0;
/// The brightest level a scene uses; +2 EV maps it to about 230.
pub const MAX_LEVEL: f32 = 130.0;

/// How many shapes a scene paints over its background.
const SHAPES: usize = 10;
/// The largest per-channel tint, in levels, so scenes are not pure gray.
const MAX_TINT: f32 = 6.0;
/// A shape's horizontal level change per image width, in absolute value.
const MIN_SLOPE_X: f32 = 35.0;
const MAX_SLOPE_X: f32 = 60.0;
/// A shape's largest vertical level change per image height.
const MAX_SLOPE_Y: f32 = 30.0;
/// The smallest level difference across the background.
const MIN_BACKGROUND_SPAN: f32 = 60.0;
/// The range of the untinted levels: the tint never pushes a channel out of
/// `MIN_LEVEL..=MAX_LEVEL`, so no area clips flat.
const LOW: f32 = MIN_LEVEL + MAX_TINT;
const HIGH: f32 = MAX_LEVEL - MAX_TINT;

/// SplitMix64: a tiny deterministic generator, so the testkit needs no `rand`.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[lo, hi)`, from the top 24 bits.
    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        let unit = (self.next() >> 40) as f32 / (1u64 << 24) as f32;
        lo + (hi - lo) * unit
    }
}

/// A shaded rectangle or ellipse, in coordinates relative to the image size.
struct Shape {
    ellipse: bool,
    center: (f32, f32),
    half_size: (f32, f32),
    /// The level at the centre, and its change per unit of relative x and y.
    level: f32,
    slope: (f32, f32),
}

impl Shape {
    fn random(rng: &mut SplitMix64) -> Self {
        let half_size = (rng.range(0.06, 0.2), rng.range(0.06, 0.2));
        // A steep horizontal slope of either sign: dHash compares horizontal
        // neighbours, and a shallow one would leave near-ties.
        let slope_x = rng.range(MIN_SLOPE_X, MAX_SLOPE_X) * random_sign(rng);
        let slope_y = rng.range(-MAX_SLOPE_Y, MAX_SLOPE_Y);
        // The centre level leaves room for the slopes, so the shape never clips.
        let reach = (slope_x.abs() * half_size.0 + slope_y.abs() * half_size.1).ceil();
        Self {
            ellipse: rng.next() & 1 == 1,
            center: (rng.range(0.1, 0.9), rng.range(0.1, 0.9)),
            half_size,
            level: rng.range(LOW + reach, HIGH - reach),
            slope: (slope_x, slope_y),
        }
    }

    /// The shape's level at `(u, v)`, or `None` outside it.
    fn level_at(&self, u: f32, v: f32) -> Option<f32> {
        let du = (u - self.center.0) / self.half_size.0;
        let dv = (v - self.center.1) / self.half_size.1;
        let inside = if self.ellipse {
            du * du + dv * dv <= 1.0
        } else {
            du.abs() <= 1.0 && dv.abs() <= 1.0
        };
        let level =
            self.level + self.slope.0 * (u - self.center.0) + self.slope.1 * (v - self.center.1);
        inside.then_some(level)
    }
}

/// A deterministic `w`×`h` scene: the same `seed` always gives the same pixels.
pub fn scene(seed: u64, w: u32, h: u32) -> RgbImage {
    let mut rng = SplitMix64(seed);
    let tint = [(); 3].map(|()| rng.range(-MAX_TINT, MAX_TINT));
    // The background runs from a dark to a bright end (or back) along a
    // mostly horizontal direction.
    let slack = HIGH - LOW - MIN_BACKGROUND_SPAN;
    let dark = LOW + rng.range(0.0, slack / 2.0);
    let bright = HIGH - rng.range(0.0, slack / 2.0);
    let (from, to) = if random_sign(&mut rng) > 0.0 {
        (dark, bright)
    } else {
        (bright, dark)
    };
    let direction = (rng.range(0.6, 1.0), rng.range(0.0, 0.4));
    let shapes: Vec<Shape> = (0..SHAPES).map(|_| Shape::random(&mut rng)).collect();

    RgbImage::from_fn(w, h, |x, y| {
        let u = (x as f32 + 0.5) / w as f32;
        let v = (y as f32 + 0.5) / h as f32;
        let along = (u * direction.0 + v * direction.1) / (direction.0 + direction.1);
        // Later shapes are painted over earlier ones.
        let level = shapes
            .iter()
            .rev()
            .find_map(|shape| shape.level_at(u, v))
            .unwrap_or(from + (to - from) * along);
        Rgb(tint.map(|t| (level + t).clamp(MIN_LEVEL, MAX_LEVEL).round() as u8))
    })
}

/// +1 or −1, evenly.
fn random_sign(rng: &mut SplitMix64) -> f32 {
    if rng.next() & 1 == 1 { 1.0 } else { -1.0 }
}

/// `img` exposed by `ev` stops: each sRGB channel goes to linear light, is
/// multiplied by 2^ev, clamped to 1 and encoded back to sRGB.
pub fn expose(img: &RgbImage, ev: f32) -> RgbImage {
    let gain = ev.exp2();
    let table: Vec<u8> = (0..=255u8)
        .map(|v| {
            let linear = srgb_to_linear(f32::from(v) / 255.0) * gain;
            (linear_to_srgb(linear.min(1.0)) * 255.0).round() as u8
        })
        .collect();
    RgbImage::from_fn(img.width(), img.height(), |x, y| {
        Rgb(img.get_pixel(x, y).0.map(|c| table[usize::from(c)]))
    })
}

/// `img` reframed: its content moves left by `dx` of the width and up by `dy`
/// of the height (negative values move it right or down), as if the camera
/// had panned. The size is kept; the uncovered strip repeats the edge pixels.
pub fn shift(img: &RgbImage, dx: f32, dy: f32) -> RgbImage {
    let (w, h) = img.dimensions();
    let offset_x = (dx * w as f32).round() as i64;
    let offset_y = (dy * h as f32).round() as i64;
    let source = |at: u32, offset: i64, len: u32| {
        (i64::from(at) + offset).clamp(0, i64::from(len) - 1) as u32
    };
    RgbImage::from_fn(w, h, |x, y| {
        *img.get_pixel(source(x, offset_x, w), source(y, offset_y, h))
    })
}

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_is_deterministic_and_seed_dependent() {
        assert_eq!(scene(1, 64, 48), scene(1, 64, 48));
        assert_ne!(scene(1, 64, 48), scene(2, 64, 48));
        assert_eq!(scene(3, 64, 48).dimensions(), (64, 48));
    }

    #[test]
    fn scene_levels_stay_in_range() {
        let img = scene(7, 128, 96);
        let (min, max) = img
            .pixels()
            .flat_map(|p| p.0)
            .fold((u8::MAX, u8::MIN), |(lo, hi), c| (lo.min(c), hi.max(c)));
        assert!(
            f32::from(min) >= MIN_LEVEL && f32::from(max) <= MAX_LEVEL,
            "{min}..{max}"
        );
        // The scene has real contrast, not one flat level.
        assert!(max - min > 40, "{min}..{max}");
    }

    #[test]
    fn expose_zero_is_identity_and_stops_scale_light() {
        let img = scene(1, 32, 32);
        assert_eq!(expose(&img, 0.0), img);
        let level = |img: &RgbImage| f32::from(img.get_pixel(0, 0).0[0]) / 255.0;
        let ratio = srgb_to_linear(level(&expose(&img, 1.0))) / srgb_to_linear(level(&img));
        assert!((ratio - 2.0).abs() < 0.1, "{ratio}");
        // +10 EV clips to white.
        assert!(expose(&img, 10.0).pixels().all(|p| p.0 == [255; 3]));
    }

    #[test]
    fn shift_moves_content_and_repeats_the_edge() {
        let img = RgbImage::from_fn(10, 4, |x, y| Rgb([x as u8, y as u8, 0]));
        let moved = shift(&img, 0.2, 0.0);
        assert_eq!(moved.get_pixel(0, 1).0, [2, 1, 0]);
        assert_eq!(moved.get_pixel(7, 1).0, [9, 1, 0]);
        assert_eq!(moved.get_pixel(9, 1).0, [9, 1, 0]);
        let down = shift(&img, 0.0, -0.5);
        assert_eq!(down.get_pixel(3, 0).0, [3, 0, 0]);
        assert_eq!(down.get_pixel(3, 3).0, [3, 1, 0]);
        assert_eq!(shift(&img, 0.0, 0.0), img);
    }
}
