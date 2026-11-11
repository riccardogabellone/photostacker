//! Perceptual hashes compatible with Python's `imagehash` 4.3, so distance
//! thresholds mean what they mean there (spec §12.3).
//!
//! Both hashes start from Pillow's `convert("L")` of the 512 px thumbnail and
//! a Lanczos-3 resize, as `imagehash.phash` and `imagehash.dhash` do. Every
//! pixel step after decoding reproduces Pillow byte for byte (see
//! [`crate::imageops`]), so an 8-bit RGB PNG gets Python's exact hash inputs.
//! Parity is still near, not guaranteed: zune-jpeg and libjpeg-turbo may
//! decode a JPEG pixel a level or two apart, and the f64 DCT here rounds
//! differently from scipy's FFT, either of which can flip a bit whose
//! comparison was a near tie.
//!
//! [`hash_pair_normalised`] hashes the same thumbnail after a histogram
//! equalisation, so that the shots of a wide exposure bracket hash close
//! together. Grouping links on those; the plain [`hash_pair`] is kept for
//! parity with `imagehash`, which does not normalise.
//!
//! The thumbnail is always RGB8 here. Pillow thumbnails palette (`P`),
//! bilevel (`1`) and CMYK sources in their own mode, so for those inputs the
//! hashes may differ from Python's by more than the measured parity.

use image::{GrayImage, RgbImage};
use serde::{Deserialize, Serialize};

use crate::imageops::{luma_pillow, pillow_resize_lanczos};

/// The side of the hash grid: 8×8 bits.
const HASH_SIZE: usize = 8;
/// pHash resizes to `HASH_SIZE * 4` (imagehash's `highfreq_factor`) a side.
const DCT_SIZE: usize = 32;

/// The pHash and dHash of one thumbnail. Bit 63 (the MSB) of each is element
/// 0 of imagehash's row-major 8×8 boolean array.
///
/// The store persists its serde form (`phash`, `dhash` as u64 numbers):
/// changing it needs a migration.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HashPair {
    pub phash: u64,
    pub dhash: u64,
}

impl HashPair {
    /// `(phash, dhash)` as 16 lowercase hex digits each, the same text as
    /// Python's `str(imagehash.ImageHash)`.
    pub fn to_hex(&self) -> (String, String) {
        (
            format!("{:016x}", self.phash),
            format!("{:016x}", self.dhash),
        )
    }
}

/// `imagehash.phash` and `imagehash.dhash` of `thumb`. An image with a
/// zero-sized side (which Pillow cannot resize) hashes to zeros.
pub fn hash_pair(thumb: &RgbImage) -> HashPair {
    if thumb.width() == 0 || thumb.height() == 0 {
        return HashPair { phash: 0, dhash: 0 };
    }
    let luma = luma_pillow(thumb);
    HashPair {
        phash: phash(&luma),
        dhash: dhash(&luma),
    }
}

/// The pHash and dHash of `thumb` after brightness normalisation: the same
/// grayscale as [`hash_pair`], then [`equalize`], then the same two hashes.
/// Equalising maps the shots of an exposure bracket onto nearly the same
/// levels, so they hash close together where their plain hashes do not
/// (spec §5.3). An image with a zero-sized side hashes to zeros.
pub fn hash_pair_normalised(thumb: &RgbImage) -> HashPair {
    if thumb.width() == 0 || thumb.height() == 0 {
        return HashPair { phash: 0, dhash: 0 };
    }
    let luma = equalize(&luma_pillow(thumb));
    HashPair {
        phash: phash(&luma),
        dhash: dhash(&luma),
    }
}

/// Pillow's `ImageOps.equalize` of an `L` image, without a mask: each level
/// maps to its place in the cumulative histogram, the last used level left
/// out, on a 0–255 scale. An image with at most one level used is returned
/// as is.
pub(crate) fn equalize(gray: &GrayImage) -> GrayImage {
    let mut histogram = [0u64; 256];
    for &level in gray.as_raw() {
        histogram[usize::from(level)] += 1;
    }
    let Some(last) = histogram.iter().rposition(|&count| count != 0) else {
        return gray.clone();
    };
    let total: u64 = histogram.iter().sum();
    let step = (total - histogram[last]) / 255;
    if step == 0 {
        return gray.clone();
    }
    let mut lut = [0u8; 256];
    let mut n = step / 2;
    for (out, &count) in lut.iter_mut().zip(&histogram) {
        // Pillow clips the table to 8 bits when it applies it.
        *out = (n / step).min(255) as u8;
        n += count;
    }
    let mut equalized = gray.clone();
    for level in equalized.iter_mut() {
        *level = lut[usize::from(*level)];
    }
    equalized
}

/// Hamming distance of the pHashes plus that of the dHashes: 0 to 128.
pub fn distance(a: HashPair, b: HashPair) -> u32 {
    (a.phash ^ b.phash).count_ones() + (a.dhash ^ b.dhash).count_ones()
}

/// `imagehash.phash`: the 8×8 lowest frequencies of the 2-D DCT-II of the
/// 32×32 resize, each compared with their median.
fn phash(luma: &GrayImage) -> u64 {
    let small = pillow_resize_lanczos(luma, DCT_SIZE as u32, DCT_SIZE as u32);
    let pixels: Vec<f64> = small.as_raw().iter().map(|&v| f64::from(v)).collect();
    let coefficients = low_frequency_dct(&pixels);
    let mut sorted = coefficients;
    sorted.sort_by(f64::total_cmp);
    // numpy.median of 64 values: the mean of the two middle ones.
    let half = sorted.len() / 2;
    let median = (sorted[half - 1] + sorted[half]) / 2.0;
    pack_msb_first(coefficients.iter().map(|&c| c > median))
}

/// `scipy.fftpack.dct(dct(pixels, axis=0), axis=1)[:8, :8]`, row-major, for a
/// row-major 32×32 `pixels`. Only `k < 8` is needed per axis, so each
/// coefficient is computed on its own by [`dct_coefficient`].
fn low_frequency_dct(pixels: &[f64]) -> [f64; HASH_SIZE * HASH_SIZE] {
    let n = DCT_SIZE;
    let mut column = [0.0; DCT_SIZE];
    // Axis 0 (down each column): rows k < 8 of the column transforms.
    let mut rows = [[0.0; DCT_SIZE]; HASH_SIZE];
    for col in 0..n {
        for (row, value) in column.iter_mut().enumerate() {
            *value = pixels[row * n + col];
        }
        for (k, out) in rows.iter_mut().enumerate() {
            out[col] = dct_coefficient(&column, k);
        }
    }
    // Axis 1 (along each of those rows): columns l < 8.
    let mut out = [0.0; HASH_SIZE * HASH_SIZE];
    for (k, row) in rows.iter().enumerate() {
        for l in 0..HASH_SIZE {
            out[k * HASH_SIZE + l] = dct_coefficient(row, l);
        }
    }
    out
}

/// Coefficient `k` of scipy's unnormalised DCT-II of `x`,
/// `y[k] = 2 Σ x[n] cos(π k (2n + 1) / 2N)`, for a power-of-two length and
/// `k < x.len()`.
///
/// The sum is folded the way a fast DCT is: with `x[n]` paired to
/// `x[N-1-n]`, an even `k` is coefficient `k / 2` of the half-length DCT of
/// the pair sums, and an odd `k` a half-length sum over the pair differences.
/// That is the same value as the plain sum, but a constant, mirrored or
/// point-symmetric line now gives an exact 0 where the plain sum leaves
/// rounding noise, as scipy's FFT does. It matters because pHash compares
/// with the median: a flat image must have every AC term equal to it.
fn dct_coefficient(x: &[f64], k: usize) -> f64 {
    let n = x.len();
    debug_assert!(n.is_power_of_two() && k < n);
    if k == 0 {
        return 2.0 * x.iter().sum::<f64>();
    }
    let (front, back) = x.split_at(n / 2);
    let pairs = front.iter().zip(back.iter().rev());
    if k.is_multiple_of(2) {
        let sums: Vec<f64> = pairs.map(|(a, b)| a + b).collect();
        return dct_coefficient(&sums, k / 2);
    }
    let angle = |i: usize| std::f64::consts::PI * (k * (2 * i + 1)) as f64 / (2 * n) as f64;
    2.0 * pairs
        .enumerate()
        .map(|(i, (a, b))| (a - b) * angle(i).cos())
        .sum::<f64>()
}

/// `imagehash.dhash`: a 9-wide, 8-high resize, then `px[r][c + 1] > px[r][c]`.
fn dhash(luma: &GrayImage) -> u64 {
    let small = pillow_resize_lanczos(luma, HASH_SIZE as u32 + 1, HASH_SIZE as u32);
    let (rows, _) = small.as_raw().as_chunks::<{ HASH_SIZE + 1 }>();
    pack_msb_first(
        rows.iter()
            .flat_map(|row| row.windows(2).map(|pair| pair[1] > pair[0])),
    )
}

/// Packs 64 row-major booleans with the first as the MSB, which makes
/// `{:016x}` equal imagehash's hex text.
fn pack_msb_first(bits: impl IntoIterator<Item = bool>) -> u64 {
    bits.into_iter()
        .fold(0, |acc, bit| (acc << 1) | u64::from(bit))
}

#[cfg(test)]
mod tests {
    use image::{Luma, Rgb};

    use super::*;

    #[test]
    fn phash_of_flat_image_is_zero() {
        // A black image: every DCT coefficient is 0, the median too, and the
        // comparison is strict. imagehash 4.3.2 gives 0000000000000000 for both.
        let black = RgbImage::from_pixel(300, 200, Rgb([0, 0, 0]));
        assert_eq!(hash_pair(&black), HashPair { phash: 0, dhash: 0 });
    }

    #[test]
    fn phash_of_flat_gray_sets_only_the_dc_bit() {
        // imagehash 4.3.2 on Image.new('RGB', (300, 200), (v, v, v)), v in 1, 128, 255:
        // phash 8000000000000000, dhash 0000000000000000. The AC coefficients
        // are exactly 0 and only the DC term (element 0, the MSB) is above the median.
        for v in [1, 128, 255] {
            let flat = RgbImage::from_pixel(300, 200, Rgb([v, v, v]));
            let hash = hash_pair(&flat);
            assert_eq!(
                hash,
                HashPair {
                    phash: 1 << 63,
                    dhash: 0
                },
                "v = {v}"
            );
            assert_eq!(
                hash.to_hex(),
                ("8000000000000000".into(), "0000000000000000".into())
            );
        }
    }

    #[test]
    fn dhash_bits_are_row_major_msb_first() {
        // Each case and its hex were checked with imagehash 4.3.2 on the same
        // 9×8 image, which both apps hash as is (a resize to the same size is a copy).
        let gray = |brighter: &[(u32, u32)]| {
            let mut img = RgbImage::from_pixel(9, 8, Rgb([100, 100, 100]));
            for &(x, y) in brighter {
                img.put_pixel(x, y, Rgb([200, 200, 200]));
            }
            img
        };
        // Row 0 steps up between px[0] and px[1]: element 0.
        let row0: Vec<_> = (1..9).map(|x| (x, 0)).collect();
        assert_eq!(hash_pair(&gray(&row0)).to_hex().1, "8000000000000000");
        // Row 7, px[8] > px[7]: element 63.
        assert_eq!(hash_pair(&gray(&[(8, 7)])).to_hex().1, "0000000000000001");
        // Row 1, px[3] > px[2]: element 8 + 2 = 10.
        let row1: Vec<_> = (3..9).map(|x| (x, 1)).collect();
        let middle = hash_pair(&gray(&row1));
        assert_eq!(middle.dhash, 1 << (63 - 10));
        assert_eq!(middle.to_hex().1, "0020000000000000");
    }

    #[test]
    fn tiny_and_one_pixel_wide_images_match_imagehash() {
        // imagehash 4.3.2 on these images (both resizes upscale at least one
        // axis; a one-pixel line makes whole DCT rows or columns exactly 0).
        let cases = [
            ((512, 1), ("aa00000000000000", "a1a1a1a1a1a1a1a1")),
            ((1, 512), ("8000800080008000", "0000000000000000")),
            ((1, 1), ("8000000000000000", "0000000000000000")),
            ((3, 2), ("9832003200230037", "fffffffcf8f0f0f0")),
        ];
        for ((w, h), (phash, dhash)) in cases {
            let img = RgbImage::from_fn(w, h, |x, y| {
                Rgb([
                    ((x * 37 + y * 11) % 256) as u8,
                    ((x * 5 + y * 71 + 3) % 256) as u8,
                    ((x * y * 29 + 200) % 256) as u8,
                ])
            });
            let hex = hash_pair(&img).to_hex();
            assert_eq!((hex.0.as_str(), hex.1.as_str()), (phash, dhash), "{w}x{h}");
        }
    }

    #[test]
    fn resize_to_the_same_size_is_a_copy() {
        let img = GrayImage::from_fn(9, 8, |x, y| Luma([(x * 31 + y * 7) as u8]));
        assert_eq!(pillow_resize_lanczos(&img, 9, 8), img);
    }

    #[test]
    fn empty_image_hashes_to_zero() {
        assert_eq!(
            hash_pair(&RgbImage::new(0, 5)),
            HashPair { phash: 0, dhash: 0 }
        );
    }

    #[test]
    fn equalize_stretches_two_levels() {
        // 255 pixels of 0 and 255 of 1: step = (510 - 255) / 255 = 1, so 0
        // maps to 0 / 1 and 1 to 255 / 1.
        let img = GrayImage::from_fn(255, 2, |_, y| Luma([y as u8]));
        let equalized = equalize(&img);
        assert_eq!(equalized.get_pixel(0, 0).0, [0]);
        assert_eq!(equalized.get_pixel(0, 1).0, [255]);
    }

    #[test]
    fn equalize_constant_image_is_identity() {
        for v in [0, 77, 255] {
            let flat = GrayImage::from_pixel(64, 32, Luma([v]));
            assert_eq!(equalize(&flat), flat, "v = {v}");
        }
        // Fewer than 255 pixels outside the last used level: step 0.
        let sparse = GrayImage::from_fn(30, 1, |x, _| Luma([if x == 29 { 255 } else { 0 }]));
        assert_eq!(equalize(&sparse), sparse);
        assert_eq!(equalize(&GrayImage::new(0, 0)), GrayImage::new(0, 0));
    }

    #[test]
    fn equalize_matches_pillow() {
        // Pillow 12.2.0, ImageOps.equalize of this 64×32 L image: each
        // (input level, output level). The top levels clip to 255.
        let img = GrayImage::from_fn(64, 32, |x, y| {
            Luma([(((x * x * 3 + y * 17 + (x * y) % 7) % 50) * 4 + 20) as u8])
        });
        let expected: [(u8, u8); 50] = [
            (20, 0),
            (24, 7),
            (28, 12),
            (32, 18),
            (36, 24),
            (40, 29),
            (44, 35),
            (48, 42),
            (52, 50),
            (56, 55),
            (60, 60),
            (64, 66),
            (68, 72),
            (72, 78),
            (76, 84),
            (80, 89),
            (84, 95),
            (88, 99),
            (92, 105),
            (96, 111),
            (100, 119),
            (104, 124),
            (108, 130),
            (112, 136),
            (116, 141),
            (120, 146),
            (124, 154),
            (128, 162),
            (132, 167),
            (136, 172),
            (140, 178),
            (144, 183),
            (148, 189),
            (152, 195),
            (156, 201),
            (160, 207),
            (164, 212),
            (168, 216),
            (172, 223),
            (176, 231),
            (180, 236),
            (184, 242),
            (188, 247),
            (192, 253),
            (196, 255),
            (200, 255),
            (204, 255),
            (208, 255),
            (212, 255),
            (216, 255),
        ];
        let equalized = equalize(&img);
        let mut pairs: Vec<(u8, u8)> = img
            .as_raw()
            .iter()
            .copied()
            .zip(equalized.as_raw().iter().copied())
            .collect();
        pairs.sort_unstable();
        pairs.dedup();
        assert_eq!(pairs, expected);
    }

    #[test]
    fn normalised_hash_ignores_exposure() {
        // A pinned regression point, not a general property: other scenes
        // and exposures can land elsewhere. A testkit scene keeps its levels
        // in 48–130, where an exposure change is close to a scaling the plain
        // hashes ignore; they drift only once the highlights clip. Measured
        // in 0.1 EV steps, seed 3 first goes past 8 at +3.2 EV (plain 11,
        // normalised 6); seed 1 at +2.9 EV (plain 9, normalised 8) has no
        // margin left.
        let base = photostacker_testkit::scene(3, 512, 341);
        let exposed = photostacker_testkit::expose(&base, 3.2);
        let plain = distance(hash_pair(&base), hash_pair(&exposed));
        let normalised = distance(hash_pair_normalised(&base), hash_pair_normalised(&exposed));
        assert!(plain > 8, "plain distance {plain}");
        assert!(normalised <= 8, "normalised distance {normalised}");
    }

    #[test]
    fn normalised_hash_of_empty_image_is_zero() {
        assert_eq!(
            hash_pair_normalised(&RgbImage::new(5, 0)),
            HashPair { phash: 0, dhash: 0 }
        );
    }

    #[test]
    fn distance_is_symmetric_and_bounded() {
        let hashes = [
            HashPair { phash: 0, dhash: 0 },
            HashPair {
                phash: u64::MAX,
                dhash: u64::MAX,
            },
            HashPair {
                phash: 0x8000_0000_0000_0001,
                dhash: 0x00ff,
            },
            HashPair {
                phash: 0x0123_4567_89ab_cdef,
                dhash: 0xfedc_ba98_7654_3210,
            },
        ];
        for a in hashes {
            assert_eq!(distance(a, a), 0);
            for b in hashes {
                assert_eq!(distance(a, b), distance(b, a));
                assert!(distance(a, b) <= 128);
            }
        }
        assert_eq!(distance(hashes[0], hashes[1]), 128);
        assert_eq!(distance(hashes[0], hashes[2]), 2 + 8);
    }

    /// The store persists this spelling: changing it needs a migration. The
    /// hashes are whole u64 numbers, exact past 2^53.
    #[test]
    fn hash_pair_serde_spelling_is_pinned() {
        let pair = HashPair {
            phash: u64::MAX,
            dhash: 1,
        };
        let json = r#"{"phash":18446744073709551615,"dhash":1}"#;
        assert_eq!(serde_json::to_string(&pair).unwrap(), json);
        assert_eq!(serde_json::from_str::<HashPair>(json).unwrap(), pair);
    }
}
