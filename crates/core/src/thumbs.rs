//! The review thumbnails (spec §5.3): small JPEGs of the thumbnails the
//! analysis hashed, cached on disk for the review.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use image::RgbImage;
use image::codecs::jpeg::JpegEncoder;

use crate::store::JobId;
use crate::{fsio, imageops};

/// The long side of a review thumbnail.
pub const REVIEW_THUMBNAIL_MAX_SIDE: u32 = 256;

/// The JPEG quality of a review thumbnail.
const JPEG_QUALITY: u8 = 85;

/// The folder of `job`'s review thumbnails within `thumbs_dir`, the folder of
/// them all ([`crate::AppDirs::thumbs`]): `thumbs_dir/<job id>`. Only a path:
/// nothing is looked at.
pub fn job_thumbs_dir(thumbs_dir: &Path, job: JobId) -> PathBuf {
    thumbs_dir.join(job.to_string())
}

/// Where [`write_thumbnail`] puts the thumbnail of `key` in `dir`:
/// `dir/<the first 16 hex digits of the BLAKE3 hash of key>.jpg`.
pub fn thumbnail_path(dir: &Path, key: &[u8]) -> PathBuf {
    let hex = blake3::hash(key).to_hex();
    dir.join(format!("{}.jpg", &hex[..16]))
}

/// Writes `thumb` as a review thumbnail into `dir`, creating `dir` if needed,
/// and returns the file's path, [`thumbnail_path`].
///
/// The image is shrunk to a [`REVIEW_THUMBNAIL_MAX_SIDE`] long side with the
/// analysis' own resampler ([`imageops::pillow_thumbnail`]), keeping its
/// aspect ratio; one that fits already is written as it is. The JPEG, at
/// quality 85, replaces any thumbnail of the same key through
/// [`fsio::write_replace`], so a reader never sees a half-written file.
pub fn write_thumbnail(dir: &Path, key: &[u8], thumb: &RgbImage) -> io::Result<PathBuf> {
    let small = imageops::pillow_thumbnail(thumb, REVIEW_THUMBNAIL_MAX_SIDE);
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY)
        .encode_image(&small)
        .map_err(io::Error::other)?;
    fs::create_dir_all(dir)?;
    let path = thumbnail_path(dir, key);
    fsio::write_replace(&path, &jpeg)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageFormat;
    use photostacker_testkit::scene;
    use std::fs;

    /// The mean absolute difference of two images of one size, per channel.
    fn mean_difference(a: &RgbImage, b: &RgbImage) -> f64 {
        assert_eq!(a.dimensions(), b.dimensions());
        let total: u64 = a
            .as_raw()
            .iter()
            .zip(b.as_raw())
            .map(|(&x, &y)| u64::from(x.abs_diff(y)))
            .sum();
        total as f64 / a.as_raw().len() as f64
    }

    #[test]
    fn thumbnail_is_a_256_px_jpeg() {
        let root = tempfile::tempdir().unwrap();
        // A folder that does not exist yet.
        let dir = root.path().join("thumbs").join("1");
        let name = |key: &[u8]| format!("{}.jpg", &blake3::hash(key).to_hex()[..16]);

        for (key, (width, height), expected) in [
            (&b"landscape"[..], (512, 384), (256, 192)),
            (b"portrait", (300, 400), (192, 256)),
            // One that fits already is not enlarged.
            (b"small", (200, 100), (200, 100)),
        ] {
            let image = scene(1, width, height);
            let path = write_thumbnail(&dir, key, &image).unwrap();
            assert_eq!(path, dir.join(name(key)));
            let bytes = fs::read(&path).unwrap();
            assert_eq!(image::guess_format(&bytes).unwrap(), ImageFormat::Jpeg);
            let written = image::load_from_memory(&bytes).unwrap().to_rgb8();
            assert_eq!(written.dimensions(), expected, "{path:?}");
            let shrunk = crate::imageops::pillow_thumbnail(&image, REVIEW_THUMBNAIL_MAX_SIDE);
            let difference = mean_difference(&written, &shrunk);
            assert!(difference < 6.0, "{path:?}: {difference}");
        }

        // The same key replaces its thumbnail, and no partial file is left.
        let path = write_thumbnail(&dir, b"landscape", &scene(2, 100, 50)).unwrap();
        assert_eq!(path, dir.join(name(b"landscape")));
        let written = image::open(&path).unwrap();
        assert_eq!((written.width(), written.height()), (100, 50));
        let mut names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        let mut expected = [name(b"landscape"), name(b"portrait"), name(b"small")];
        expected.sort();
        assert_eq!(names, expected);
    }
}
