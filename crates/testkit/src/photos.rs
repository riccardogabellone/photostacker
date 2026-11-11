//! Folders of synthetic photos for the engine and the app's tests: brackets of JPEGs with
//! Canon EXIF, a bracket of RAW+JPEG pairs, lone shots without EXIF, and the photos of a
//! review whose stacks change with the hash threshold.

use std::fs;
use std::path::Path;

use image::RgbImage;

use crate::{
    MakerNoteSpec, NoteStyle, TiffSpec, Value, expose, jpeg_with_exif, raf_with_jpeg, scene, shift,
};

/// The size of every photo written here.
pub const WIDTH: u32 = 640;
pub const HEIGHT: u32 = 426;
pub const JPEG_QUALITY: u8 = 92;

/// Canon EXIF for a shot `second` seconds into the scene, which starts at 10:00:00 (past
/// a minute, the minutes count on), AEB shot `shot`.
pub fn canon_exif(second: u32, shot: u16) -> TiffSpec {
    // FileInfo: word 0 the byte count, word 3 BracketMode (1 = AEB), word 5
    // BracketShotNumber.
    let mut file_info = vec![0u16; 20];
    file_info[0] = 40;
    file_info[3] = 1;
    file_info[5] = shot;
    TiffSpec {
        little_endian: true,
        ifd0: vec![
            (0x010F, Value::Ascii("Canon".into())),
            (0x0110, Value::Ascii("Canon EOS 2000D".into())),
        ],
        exif: vec![
            (
                0x9003,
                Value::Ascii(format!(
                    "2026:05:01 10:{:02}:{:02}",
                    second / 60,
                    second % 60
                )),
            ),
            (0x920A, Value::Rational(vec![(35, 1)])),
        ],
        maker_note: Some(MakerNoteSpec {
            style: NoteStyle::CanonIfd,
            entries: vec![(0x0093, Value::Short(file_info))],
        }),
    }
}

/// `image` as a JPEG without EXIF.
pub fn plain_jpeg(image: &RgbImage) -> Vec<u8> {
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, JPEG_QUALITY)
        .encode_image(image)
        .unwrap();
    bytes
}

/// Writes a bracket of `shots` exposures of scene `seed`, 1 EV apart around
/// 0 and 1 s apart from `second`, as `<prefix>_<n>.jpg`.
pub fn write_bracket(dir: &Path, prefix: &str, seed: u64, second: u32, shots: u16) {
    let base = scene(seed, WIDTH, HEIGHT);
    for shot in 1..=shots {
        let ev = f32::from(shot) - f32::from(shots + 1) / 2.0;
        let exif = canon_exif(second + u32::from(shot) - 1, shot);
        let bytes = jpeg_with_exif(&expose(&base, ev), &exif.build(), JPEG_QUALITY);
        fs::write(dir.join(format!("{prefix}_{shot}.jpg")), bytes).unwrap();
    }
}

/// [`canon_exif`] with an exposure: 1/250 s at f/8, ISO 100.
pub fn canon_exposure_exif(second: u32, shot: u16) -> TiffSpec {
    let mut spec = canon_exif(second, shot);
    spec.exif.extend([
        (0x829A, Value::Rational(vec![(1, 250)])),
        (0x829D, Value::Rational(vec![(8, 1)])),
        (0x8827, Value::Short(vec![100])),
    ]);
    spec
}

/// Writes a bracket of three shots of scene `seed` as `<prefix>_<n>.raf`,
/// each with its JPEG `<prefix>_<n>.jpg`: the same bytes, which the RAF
/// embeds as its preview, with [`canon_exposure_exif`]. rawler knows the
/// RAF's body, so the RAW of each pair is active and hashed from that
/// preview, without a warning.
pub fn write_raw_bracket(dir: &Path, prefix: &str, seed: u64) {
    let base = scene(seed, WIDTH, HEIGHT);
    for shot in 1..=3u16 {
        let ev = f32::from(shot) - 2.0;
        let exif = canon_exposure_exif(u32::from(shot) - 1, shot);
        let jpeg = jpeg_with_exif(&expose(&base, ev), &exif.build(), JPEG_QUALITY);
        fs::write(
            dir.join(format!("{prefix}_{shot}.raf")),
            raf_with_jpeg(&jpeg),
        )
        .unwrap();
        fs::write(dir.join(format!("{prefix}_{shot}.jpg")), jpeg).unwrap();
    }
}

/// Writes `count` unrelated shots without EXIF, `lone_<n>.jpg`.
pub fn write_lone_shots(dir: &Path, count: usize) {
    for n in 0..count {
        let image = scene(100 + n as u64, WIDTH, HEIGHT);
        fs::write(dir.join(format!("lone_{n:02}.jpg")), plain_jpeg(&image)).unwrap();
    }
}

/// Writes five photos without EXIF into `dir`, so one bucket by their modification
/// times: `a_1` and `a_2` (scene `a`, distance 0), `b_1` and `b_2` (scene `b`, distance
/// 0), and `b_3`, scene `b` panned, 16 from `b_1` and `b_2`. At the default Hamming
/// threshold of 8 the stacks are `{a_1, a_2}` and `{b_1, b_2}`, with `b_3` in the tray;
/// at 20, `b_3` joins `b`.
pub fn write_review_photos(dir: &Path, a: u64, b: u64) {
    let (a, b) = (scene(a, WIDTH, HEIGHT), scene(b, WIDTH, HEIGHT));
    let photos = [
        ("a_1", a.clone()),
        ("a_2", expose(&a, 0.3)),
        ("b_1", b.clone()),
        ("b_2", expose(&b, 0.3)),
        ("b_3", shift(&b, 0.04, 0.0)),
    ];
    for (name, image) in photos {
        fs::write(dir.join(format!("{name}.jpg")), plain_jpeg(&image)).unwrap();
    }
}
