//! Golden parity of pHash/dHash with Python's `imagehash` (spec §12.3).
//!
//! `fixtures/hash/` and its `golden.json` come from
//! `scripts/gen_hash_golden.py`, whose `fingerprint` hashes Pillow's upright
//! 512 px thumbnail (Pillow 12.2.0, imagehash 4.3.2).
//!
//! Parity covers the plain hashes only (`FileRecord::hashes`). Grouping links
//! on the brightness-normalised hashes (spec §5.3), which `imagehash` does not
//! compute, so they have no golden here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use image::{Rgb, RgbImage};
use photostacker_core::decode::thumbnail;
use photostacker_core::discover::kind_of;
use photostacker_core::exif::read_exif;
use photostacker_core::hash::{HashPair, distance, hash_pair};
use photostacker_core::imageops::{luma_pillow, pillow_resize_lanczos, pillow_thumbnail};
use serde::Deserialize;

/// The thumbnail size of the reference `fingerprint`.
const MAX_SIDE: u32 = 512;

#[derive(Deserialize)]
struct Golden {
    hashes: BTreeMap<String, PythonHash>,
    fractional_box_thumbnail: PixelGolden,
    /// PNG fixture -> Pillow's "9x8" and "32x32" Lanczos resizes of its luma
    /// thumbnail, as hex bytes.
    luma_resizes: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Deserialize)]
struct PythonHash {
    phash: String,
    dhash: String,
    phash_bits: String,
    dhash_bits: String,
}

impl PythonHash {
    fn pair(&self) -> HashPair {
        let parse = |hex: &str| u64::from_str_radix(hex, 16).expect("golden hex");
        HashPair {
            phash: parse(&self.phash),
            dhash: parse(&self.dhash),
        }
    }
}

#[derive(Deserialize)]
struct PixelGolden {
    source_size: (u32, u32),
    thumbnail_size: (u32, u32),
    /// `[x, y, r, g, b]`.
    samples: Vec<[u32; 5]>,
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hash")
}

fn golden() -> Golden {
    let text = std::fs::read_to_string(fixtures().join("golden.json")).expect("golden.json");
    serde_json::from_str(&text).expect("golden.json parses")
}

/// The analyze pipeline: EXIF orientation, the 512 px thumbnail, the hashes.
fn rust_hash(name: &str) -> HashPair {
    let path = fixtures().join(name);
    let kind = kind_of(&path).expect("supported extension");
    let orientation = read_exif(&path, kind)
        .expect("EXIF reads")
        .map_or(1, |exif| exif.orientation);
    let thumb = thumbnail(&path, kind, orientation, MAX_SIDE)
        .expect("decodes")
        .image;
    hash_pair(&thumb)
}

#[test]
fn hash_parity_with_python() {
    let golden = golden();
    assert_eq!(golden.hashes.len(), 40);
    let mut histogram = BTreeMap::<u32, usize>::new();
    println!(
        "{:<24} {:>5} {:>5} {:>5}",
        "fixture", "pHash", "dHash", "total"
    );
    for (name, python) in &golden.hashes {
        let (rust, python) = (rust_hash(name), python.pair());
        let p = (rust.phash ^ python.phash).count_ones();
        let d = (rust.dhash ^ python.dhash).count_ones();
        let total = distance(rust, python);
        assert_eq!(total, p + d);
        println!("{name:<24} {p:>5} {d:>5} {total:>5}");
        *histogram.entry(total).or_default() += 1;
    }
    let n = golden.hashes.len();
    let within_2: usize = histogram.range(..=2).map(|(_, count)| count).sum();
    println!("distance distribution (distance: fixtures): {histogram:?}");
    println!(
        "<= 2: {within_2}/{n} ({:.1}%)",
        100.0 * within_2 as f64 / n as f64
    );
    let worst = *histogram.keys().last().expect("fixtures");
    assert!(
        20 * within_2 >= 19 * n,
        "fewer than 95% within 2: {histogram:?}"
    );
    assert!(
        worst <= 4,
        "a fixture is {worst} away from Python: {histogram:?}"
    );
}

#[test]
fn orientation_tag_and_rotated_pixels_hash_equal() {
    for (rotated, original) in [
        ("pair1_rotated_o1.jpg", "pair1_original_o6.jpg"),
        ("pair2_rotated_o1.jpg", "pair2_original_o6.jpg"),
    ] {
        let (a, b) = (rust_hash(rotated), rust_hash(original));
        assert_eq!(distance(a, b), 0, "{rotated} vs {original}: {a:?} {b:?}");
    }
}

#[test]
fn golden_hash_hex_is_the_msb_first_bit_array() {
    // The hex text imagehash prints is its row-major boolean array read as a
    // binary number, element 0 first: element 0 is the MSB of our u64.
    let msb_first = |bits: &str| {
        assert_eq!(bits.len(), 64);
        bits.bytes()
            .fold(0u64, |acc, b| (acc << 1) | u64::from(b == b'1'))
    };
    for (name, python) in golden().hashes {
        let pair = python.pair();
        assert_eq!(msb_first(&python.phash_bits), pair.phash, "{name}");
        assert_eq!(msb_first(&python.dhash_bits), pair.dhash, "{name}");
    }
}

#[test]
fn hex_matches_imagehash_text_on_a_fixture() {
    // p05.png is 512×512: no thumbnail resize and no JPEG decoder, so it
    // isolates luma, the Lanczos resizes, the DCT and the bit packing.
    let golden = golden();
    let python = &golden.hashes["p05.png"];
    let (phash, dhash) = rust_hash("p05.png").to_hex();
    assert_eq!(
        (phash.as_str(), dhash.as_str()),
        (python.phash.as_str(), python.dhash.as_str())
    );
}

#[test]
fn thumbnail_fractional_box_matches_pillow() {
    // Rebuilt from the formula in gen_hash_golden.py. 3001×2001 reduces by
    // (2, 2) and resamples the box (0, 0, 1500.5, 1000.5): the last reduced
    // column and row are partial blocks. The resample is a port of Pillow's
    // integer arithmetic, so the pixels must be identical, not merely close.
    let pixels = golden().fractional_box_thumbnail;
    let (w, h) = pixels.source_size;
    let source = RgbImage::from_fn(w, h, |x, y| {
        Rgb([
            (x * 255 / (w - 1)) as u8,
            (y * 255 / (h - 1)) as u8,
            ((x ^ y) & 0xFF) as u8,
        ])
    });
    let thumb = pillow_thumbnail(&source, MAX_SIDE);
    assert_eq!(thumb.dimensions(), pixels.thumbnail_size);
    assert!(pixels.samples.len() > 1000);
    for [x, y, r, g, b] in pixels.samples {
        let got = thumb.get_pixel(x, y).0.map(u32::from);
        assert_eq!(got, [r, g, b], "pixel ({x}, {y})");
    }
}

#[test]
fn hash_resize_stages_match_pillow_on_png_fixtures() {
    // PNGs keep the JPEG decoder out, so this pins the thumbnail, the luma and
    // both Lanczos resizes of imagehash against Pillow's bytes.
    let resizes = golden().luma_resizes;
    assert_eq!(resizes.len(), 20);
    let mut mismatches = Vec::new();
    for (name, expected) in &resizes {
        let path = fixtures().join(name);
        let kind = kind_of(&path).expect("supported extension");
        let luma = luma_pillow(&thumbnail(&path, kind, 1, MAX_SIDE).expect("decodes").image);
        for (size, (w, h)) in [("9x8", (9, 8)), ("32x32", (32, 32))] {
            let hex = &expected[size];
            let want: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
                .collect();
            let got = pillow_resize_lanczos(&luma, w, h).into_raw();
            assert_eq!(got.len(), want.len(), "{name} {size}");
            let differing = got.iter().zip(&want).filter(|(a, b)| a != b).count();
            let worst = got.iter().zip(&want).map(|(a, b)| a.abs_diff(*b)).max();
            if differing > 0 {
                mismatches.push(format!(
                    "{name} {size}: {differing} px differ, max {worst:?}"
                ));
            }
        }
    }
    assert!(mismatches.is_empty(), "{mismatches:#?}");
}
