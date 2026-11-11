//! RAW thumbnails and full-resolution develops through `rawler` 0.8 (LGPL-2.1).
//!
//! - `thumb.rs`: the thumbnails, from rawler's previews or a binned develop.
//! - `develop.rs`: the binned develop, and the size and samples of rawler's
//!   full develop.
//! - `full.rs`: the full-resolution develop and its probe, for the writers.
//! - Here: what they share, the embedded-JPEG fallback among it.
//!
//! # What rawler 0.8 offers
//!
//! Read from the rawler 0.8.0 sources (`src/lib.rs`, `src/decoders/mod.rs`,
//! `src/imgop/develop.rs`, `src/rawimage.rs`):
//!
//! 1. **Embedded images.** `rawler::get_decoder(&RawSource)` picks a decoder
//!    from the file's content (magic bytes, then the TIFF `Make`), never from
//!    its extension. Its `Decoder` has two accessors, each `Ok(None)` when the
//!    file has no such image:
//!    - `preview_image`: the largest preview the decoder knows. CR2: the
//!      full-size JPEG of IFD0. CR3: the `PreviewBig` track. NEF: the largest
//!      `JPEGInterchangeFormat`. ARW: the IFD0 one. RAF: the embedded JPEG.
//!      RW2: `JpegData`. PEF: the MakerNote preview. DNG: the first preview
//!      SubIFD. 3FR: IFD0.
//!    - `thumbnail_image`: a small one; only DNG implements it.
//!
//!    ORF, CRW, MRW, SRW, ERF, KDC, MOS and IIQ implement neither, so they
//!    always take the develop.
//! 2. **Develop.** `RawDevelop::default().develop_intermediate(&RawImage)`
//!    runs, in `f32`: rescale, full-size demosaic (PPG for Bayer, bilinear for
//!    X-Trans), crop, white balance and camera matrix, sRGB curve. It has no
//!    half-size switch, and a 24 MP RAW passes through several full-size `f32`
//!    buffers (well over 500 MB).
//! 3. **Rotation.** Neither result is rotated. `preview_image` decodes the
//!    embedded JPEG as stored, and the develop only records
//!    `RawImage::orientation`. Cameras store previews in sensor orientation.
//!    The portrait fixtures all come out landscape until `orientation` is
//!    applied: Canon R10 CR3, Canon 1D Mk IV CR2, Nikon Z 30 NEF, Sony
//!    ILCE-6400A ARW and Fujifilm X-H2 RAF. So `orientation` applies to both
//!    paths.
//!
//! # Fallback: our own embedded preview
//!
//! `get_decoder` picks a decoder by the camera's Make and Model, so a body
//! rawler does not know fails before `preview_image` is reached, though its
//! RAW usually embeds a large JPEG. When rawler fails for any reason (an error
//! or a caught panic, at any step), [`raw_thumbnail`] takes the JPEGs that
//! `preview.rs` finds in the container, largest first, and uses the first
//! that decodes: a broken largest preview gives way to the next one. It
//! shrinks that JPEG and turns it upright by our EXIF `orientation`, like
//! rawler's previews. Such a thumbnail is
//! [`ImageSource::EmbeddedPreview`](super::ImageSource::EmbeddedPreview),
//! whatever its size: it can be below `max_side` when the develop of a known
//! body failed after rawler skipped a small preview. rawler's own previews
//! stay [`ImageSource::Decoded`](super::ImageSource::Decoded), since rawler
//! supports those bodies. With no usable JPEG, rawler's error stands: alone
//! when the RAW has no embedded JPEG, else followed by the fallback's own
//! error (see [`or_fallback`]).
//!
//! [`raw_full`] and [`raw_probe`] fall back to the largest JPEG at full size:
//! RGB8, upright by our EXIF `orientation`,
//! [`ImageSource::EmbeddedPreview`](super::ImageSource::EmbeddedPreview),
//! with only the JPEG's own ICC profile.
//! For a CR3 that is the 1620×1080 `PRVW`. They take the largest one only,
//! with no retry, so that the probe and the full decode always agree on it.
//!
//! Both take the same decision: the JPEG stands in only when
//! rawler cannot probe the file, that is when its dummy decode or
//! [`developed_size`](develop::developed_size) fails. A RAW that probes as
//! a develop but whose data then fails to decode or develop is an error from
//! [`raw_full`], not the JPEG, so the writer excludes it rather than
//! stacking 8-bit sRGB pixels under a 16-bit (perhaps linear) profile.
//!
//! # Extensions
//!
//! `RAW_EXTENSIONS` (in `discover`) is the specification's list (§5.1) minus what rawler
//! cannot decode:
//!
//! `cr2 cr3 crw nef nrw arw srf sr2 raf orf rw2 pef dng srw 3fr iiq erf kdc mrw mos`
//!
//! - All but `srf` and `sr2` are in `rawler::decoders::supported_extensions()`.
//! - `srf` (Sony DSC-F828) and `sr2` (DSC-R1) are missing from that list.
//!   Still, `get_decoder` hands their TIFF (Make `SONY`) to the ARW decoder,
//!   which has an SRF path and both cameras in its camera table.
//! - `x3f` is dropped. rawler lists it, but its X3F decoder fails on every
//!   file ("X3F decoding not implemented yet"), has no preview, and its
//!   `raw_metadata` is `todo!()`. Its camera table has no Sigma.
//! - rawler also reads `ari crm dcr dcs fff mef ori qtk raw rwl`. The specification's
//!   list does not have them, and trimming it adds nothing.
//!
//! # Panics
//!
//! rawler panics on some input. It has many `todo!()`, `unimplemented!()`
//! and `assert!` arms, and it indexes slices with offsets read from headers.
//! Only its `decode` entry point catches panics; `get_decoder`,
//! `preview_image`, `raw_image` and `RawDevelop` do not. So every rawler call
//! here runs inside [`guarded`], and a panic becomes
//! [`DecodeError::Corrupt`], logged as a warning with the file's path. That
//! relies on `panic = "unwind"`, which the workspace profiles pin. The panic
//! hook prints the message to stderr too, unless
//! [`install_panic_hook`](crate::guard::install_panic_hook) keeps it quiet.

use std::path::Path;
use std::sync::Arc;

use rawler::RawlerError;
use rawler::rawsource::RawSource;

use super::DecodeError;
use crate::budget::{DECODE_MEMORY_BUDGET, WholeFileError, read_whole_file};
use crate::decode;

mod develop;
mod full;
mod thumb;

pub(crate) use full::{raw_full, raw_probe};
pub(crate) use thumb::raw_thumbnail;

/// The source rawler reads `path` through: the whole file, read into memory
/// ([`read_whole_file`]). A RAW longer than
/// [`DECODE_MEMORY_BUDGET`](crate::budget::DECODE_MEMORY_BUDGET) is not
/// supported: it is refused before any of it is read, as
/// [`DecodeError::Unsupported`]. An empty RAW is a broken one, not an I/O
/// failure.
///
/// Never `RawSource::new`, which memory-maps the file: a file truncated or
/// removed while mapped kills the process (SIGBUS), so a card pulled during a
/// scan would end the app instead of failing its files (spec §4.3). The
/// workspace's `clippy.toml` forbids that call.
fn raw_source(path: &Path) -> Result<RawSource, DecodeError> {
    let bytes = read_whole_file(path).map_err(|err| match err {
        WholeFileError::TooLarge { len } => DecodeError::Unsupported(format!(
            "the file is {len} bytes, too large to decode (over {DECODE_MEMORY_BUDGET} bytes)"
        )),
        WholeFileError::Io { source, .. } => DecodeError::Io(source),
    })?;
    if bytes.is_empty() {
        return Err(DecodeError::Corrupt("empty RAW file".into()));
    }
    Ok(RawSource::new_from_shared_vec(Arc::new(bytes)).with_path(path))
}

/// What the embedded-JPEG `fallback` gives once rawler has failed with
/// `rawler_error`: the fallback's value when it has one, else rawler's
/// error. `Ok(None)` from the fallback means that the RAW has no embedded
/// JPEG, and rawler's error stands alone; the fallback's own error (a JPEG
/// that does not decode, a container that cannot be parsed) is appended to
/// it, which keeps rawler's kind of error.
fn or_fallback<T>(
    rawler_error: DecodeError,
    fallback: Result<Option<T>, DecodeError>,
) -> Result<T, DecodeError> {
    let fallback_error = match fallback {
        Ok(Some(value)) => return Ok(value),
        Ok(None) => return Err(rawler_error),
        Err(err) => err,
    };
    let appended = |why: String| format!("{why} (embedded preview: {fallback_error})");
    Err(match rawler_error {
        DecodeError::Unsupported(why) => DecodeError::Unsupported(appended(why)),
        DecodeError::Corrupt(why) => DecodeError::Corrupt(appended(why)),
        // The callers return rawler's I/O errors before any fallback.
        io @ DecodeError::Io(_) => io,
    })
}

fn classify(err: RawlerError) -> DecodeError {
    match err {
        RawlerError::Unsupported { .. } => DecodeError::Unsupported(err.to_string()),
        RawlerError::DecoderFailed(_) => DecodeError::Corrupt(err.to_string()),
    }
}

/// Runs `work` on `path`, turning a panic into [`DecodeError::Corrupt`] (see
/// the module doc).
fn guarded<T>(
    path: &Path,
    work: impl FnOnce() -> Result<T, DecodeError>,
) -> Result<T, DecodeError> {
    // Unwind safety: after a panic, nothing `work` touched is used again but
    // the file's bytes, which rawler only reads. Its global camera table is
    // only read too.
    decode::guarded(path, "the RAW decoder panicked", work)
}

/// Test helpers shared by the tests of the submodules.
#[cfg(test)]
mod support {
    use std::fs;
    use std::path::{Path, PathBuf};

    use image::{Rgb, RgbImage};

    use crate::discover::kind_of;
    use crate::exif::read_exif;
    use crate::model::FileKind;

    /// The RAW files under `$PHOTOSTACKER_FIXTURES/<sub>`, sorted; `None`
    /// (after printing why) when the variable is unset.
    pub(super) fn fixture_raws(sub: &str) -> Option<Vec<PathBuf>> {
        let Some(root) = std::env::var_os("PHOTOSTACKER_FIXTURES") else {
            println!("SKIPPED: PHOTOSTACKER_FIXTURES not set");
            return None;
        };
        fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
            for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, found);
                } else if kind_of(&path) == Some(FileKind::Raw) {
                    found.push(path);
                }
            }
        }
        let dir = Path::new(&root).join(sub);
        let mut found = Vec::new();
        walk(&dir, &mut found);
        found.sort();
        assert!(!found.is_empty(), "no RAW file under {}", dir.display());
        Some(found)
    }

    /// Our EXIF orientation of `path`, 1 when it has none.
    pub(super) fn orientation_of(path: &Path) -> u16 {
        read_exif(path, FileKind::Raw)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
            .map_or(1, |exif| exif.orientation)
    }

    pub(super) fn name(path: &Path) -> &str {
        path.file_name().unwrap().to_str().unwrap()
    }

    /// A noisy 8-bit image with a red top-left corner block.
    pub(super) fn marked_noise(width: u32, height: u32) -> RgbImage {
        let mut state = 0x2545_f491_u32;
        RgbImage::from_fn(width, height, |x, y| {
            if x < 64 && y < 64 {
                return Rgb([255, 0, 0]);
            }
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let v = (state >> 24) as u8 / 2;
            Rgb([v, v, v])
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::discover::RAW_EXTENSIONS;

    #[test]
    fn raw_extensions_supported_by_rawler() {
        // The list of the module doc.
        let documented = [
            "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "raf", "orf", "rw2", "pef",
            "dng", "srw", "3fr", "iiq", "erf", "kdc", "mrw", "mos",
        ];
        assert_eq!(RAW_EXTENSIONS, documented);
        let listed = rawler::decoders::supported_extensions();
        for ext in RAW_EXTENSIONS {
            // SRF and SR2 are Sony TIFFs that the ARW decoder takes by content;
            // rawler's extension list leaves them out.
            let by_content = matches!(*ext, "srf" | "sr2");
            let upper = ext.to_ascii_uppercase();
            assert!(listed.contains(&upper.as_str()) || by_content, "{ext}");
        }
        // Listed by rawler, but its X3F decoder decodes no image data.
        assert!(listed.contains(&"X3F"));
        assert!(!RAW_EXTENSIONS.contains(&"x3f"));
    }

    #[cfg(unix)]
    #[test]
    fn a_raw_truncated_after_it_is_opened_cannot_fault() {
        // A card pulled during a scan: the file shrinks under rawler's source.
        // Reading a mapped page past the new end would raise SIGBUS and end
        // the test process; bytes on the heap stay as they were read.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unknown.nef");
        let original = photostacker_testkit::tiff_raw_with_jpegs(&[(160, 120), (1024, 683)]);
        fs::write(&path, &original).unwrap();
        let source = raw_source(&path).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(0)
            .unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);
        assert!(source.buf() == original.as_slice());
    }

    #[cfg(unix)]
    #[test]
    fn a_raw_over_the_budget_is_refused_unread() {
        // Sparse, so neither disk space nor memory is used; a read of it
        // would take 2 GiB of heap.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.nef");
        let len = crate::budget::DECODE_MEMORY_BUDGET + 1;
        fs::File::create(&path).unwrap().set_len(len).unwrap();
        let too_large = |err: &DecodeError| match err {
            DecodeError::Unsupported(why) => why.contains("too large to decode"),
            _ => false,
        };
        let err = raw_source(&path).unwrap_err();
        assert!(too_large(&err), "{err:?}");
        // The probe, the first step of a write, refuses it too, and so does
        // its embedded-preview fallback.
        let err = raw_probe(&path, 1, crate::model::RawTone::Linear).unwrap_err();
        assert!(too_large(&err), "{err:?}");
        assert!(err.to_string().contains("embedded preview"), "{err}");
    }

    #[test]
    fn a_rawler_panic_is_a_corrupt_file() {
        let path = Path::new("x.cr3");
        assert_eq!(guarded(path, || Ok(3)).unwrap(), 3);
        let err = guarded(path, || -> Result<u8, DecodeError> {
            panic!("not yet implemented")
        })
        .unwrap_err();
        let want = "the RAW decoder panicked: not yet implemented";
        assert!(
            matches!(&err, DecodeError::Corrupt(why) if why == want),
            "{err:?}"
        );
    }

    #[test]
    fn the_fallback_error_follows_rawler_error_in_its_kind() {
        let unknown_body = || DecodeError::Unsupported("unknown body".into());
        assert_eq!(or_fallback(unknown_body(), Ok(Some(5))).unwrap(), 5);
        // No embedded JPEG: rawler's error alone.
        let err = or_fallback::<u8>(unknown_body(), Ok(None)).unwrap_err();
        assert!(
            matches!(&err, DecodeError::Unsupported(why) if why == "unknown body"),
            "{err:?}"
        );
        // A JPEG that failed: its error follows, and rawler's kind stays.
        let truncated = || Err(DecodeError::Corrupt("truncated JPEG".into()));
        let err = or_fallback::<u8>(unknown_body(), truncated()).unwrap_err();
        let want = "unknown body (embedded preview: corrupt image: truncated JPEG)";
        assert!(
            matches!(&err, DecodeError::Unsupported(why) if why == want),
            "{err:?}"
        );
        let bad_data = DecodeError::Corrupt("bad data".into());
        let err = or_fallback::<u8>(bad_data, truncated()).unwrap_err();
        let want = "bad data (embedded preview: corrupt image: truncated JPEG)";
        assert!(
            matches!(&err, DecodeError::Corrupt(why) if why == want),
            "{err:?}"
        );
    }
}
