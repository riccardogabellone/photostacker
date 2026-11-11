//! Bracket-run links (spec §5.3): the shots of one
//! exposure bracket link to each other on the strength of the camera data,
//! with the hash distance only as a sanity check.
//!
//! The shots of a wide bracket can be farther apart than the Stage-2
//! threshold even after brightness normalisation, because clipping erases
//! detail the hashes see. When the camera says each shot is part of an
//! exposure bracket, consecutive shots of the bucket link up to the much
//! looser [`BRACKET_RUN_MAX_DISTANCE`], which still keeps out a frame of
//! another scene.
//!
//! Only shots that record a shot number or a finite bracket value take part.
//! Those are the shots Stage 1 can tell apart: it closes a bucket when a shot
//! number does not increase or a value repeats, so their runs never span two
//! sequences as Stage 1 delimits them (until a value or shot number
//! repeats). Exposure shots with neither (a vendor note that records
//! neither, or the ExposureMode 2 fallback without an exposure bias) can sit
//! in one bucket across several sequences, and a distance of 40 is too loose
//! to keep a reframed shot out; they link by the threshold alone.

use super::FileFacts;
use crate::exif::BracketMode;
use crate::hash::{HashPair, distance};

/// The largest hash distance at which two consecutive shots of a bracket run
/// still link. Different scenes stay at least 48 apart after normalisation
/// on the calibration sets (`docs/calibration.md`).
pub const BRACKET_RUN_MAX_DISTANCE: u32 = 40;

/// The bracket-run links of one bucket: each pair of consecutive hashed
/// run members (exposure-bracket shots with a shot number or a finite
/// bracket value), in the bucket's order, whose distance is at most
/// [`BRACKET_RUN_MAX_DISTANCE`]. Every other member, and every member without
/// a hash, is skipped over: the shots on either side of it are consecutive.
///
/// # Panics
///
/// If a member of `bucket` is not a valid index into `facts` and `hashes`.
pub fn bracket_run_links(
    bucket: &[usize],
    facts: &[FileFacts],
    hashes: &[Option<HashPair>],
) -> Vec<(usize, usize)> {
    let run: Vec<(usize, HashPair)> = bucket
        .iter()
        .filter(|&&id| is_run_member(&facts[id]))
        .filter_map(|&id| hashes[id].map(|hash| (id, hash)))
        .collect();
    run.windows(2)
        .filter(|pair| distance(pair[0].1, pair[1].1) <= BRACKET_RUN_MAX_DISTANCE)
        .map(|pair| (pair[0].0, pair[1].0))
        .collect()
}

/// Whether the camera says the shot belongs to an exposure bracket, and
/// records where in it: a shot number or a finite bracket value.
fn is_run_member(file: &FileFacts) -> bool {
    file.exif
        .as_ref()
        .and_then(|exif| exif.bracket)
        .is_some_and(|bracket| {
            bracket.mode == BracketMode::Exposure
                && (bracket.shot_number.is_some() || bracket.value_ev.is_some_and(f32::is_finite))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exif::{BracketInfo, ExifData};

    /// A shot whose bracket mode is `mode`, with a bracket value of 0 EV, or
    /// that has no bracket fields.
    fn shot(mode: Option<BracketMode>) -> FileFacts {
        located(mode, None, Some(0.0))
    }

    /// A shot with these bracket fields.
    fn located(
        mode: Option<BracketMode>,
        shot_number: Option<u16>,
        value_ev: Option<f32>,
    ) -> FileFacts {
        FileFacts {
            path: "a.jpg".into(),
            exif: Some(ExifData {
                bracket: mode.map(|mode| BracketInfo {
                    mode,
                    shot_number,
                    value_ev,
                }),
                ..ExifData::default()
            }),
            mtime_ms: 0,
        }
    }

    /// A hash `bits` bits away from `hash(0)`.
    fn hash(bits: u32) -> Option<HashPair> {
        let ones = |n: u32| if n == 0 { 0 } else { u64::MAX >> (64 - n) };
        Some(HashPair {
            phash: ones(bits.min(64)),
            dhash: ones(bits.saturating_sub(64)),
        })
    }

    const EXPOSURE: Option<BracketMode> = Some(BracketMode::Exposure);

    #[test]
    fn run_links_consecutive_exposure_shots_beyond_the_threshold() {
        // 0–20–40 bits: consecutive shots are 20 apart, far beyond the
        // default threshold of 8; the ends are 40 apart and never compared.
        let facts = [shot(EXPOSURE), shot(EXPOSURE), shot(EXPOSURE)];
        let hashes = [hash(0), hash(20), hash(40)];
        assert_eq!(
            bracket_run_links(&[0, 1, 2], &facts, &hashes),
            [(0, 1), (1, 2)]
        );
        // The bucket's order decides what is consecutive.
        assert_eq!(
            bracket_run_links(&[2, 0, 1], &facts, &hashes),
            [(2, 0), (0, 1)]
        );
    }

    #[test]
    fn run_skips_over_off_other_and_unhashed_members() {
        let facts = [
            shot(EXPOSURE),
            shot(Some(BracketMode::Off)),
            shot(EXPOSURE),
            shot(Some(BracketMode::Other)),
            shot(None),
            shot(EXPOSURE),
            shot(EXPOSURE),
        ];
        let hashes = [hash(0), hash(0), hash(20), hash(0), hash(0), None, hash(30)];
        assert_eq!(
            bracket_run_links(&[0, 1, 2, 3, 4, 5, 6], &facts, &hashes),
            [(0, 2), (2, 6)]
        );
    }

    #[test]
    fn run_link_needs_exposure_mode_on_both() {
        let hashes = [hash(0), hash(20)];
        for other in [Some(BracketMode::Off), Some(BracketMode::Other), None] {
            let facts = [shot(EXPOSURE), shot(other)];
            assert!(
                bracket_run_links(&[0, 1], &facts, &hashes).is_empty(),
                "{other:?}"
            );
            let facts = [shot(other), shot(EXPOSURE)];
            assert!(
                bracket_run_links(&[0, 1], &facts, &hashes).is_empty(),
                "{other:?}"
            );
        }
        let no_exif = FileFacts {
            exif: None,
            ..shot(None)
        };
        let facts = [shot(EXPOSURE), no_exif];
        assert!(bracket_run_links(&[0, 1], &facts, &hashes).is_empty());
    }

    #[test]
    fn run_link_needs_a_shot_number_or_value_on_both() {
        let hashes = [hash(0), hash(20)];
        let blind = located(EXPOSURE, None, None);
        let nan = located(EXPOSURE, None, Some(f32::NAN));
        let valued = located(EXPOSURE, None, Some(-2.0));
        let numbered = located(EXPOSURE, Some(3), None);

        assert!(bracket_run_links(&[0, 1], &[blind.clone(), blind.clone()], &hashes).is_empty());
        assert!(bracket_run_links(&[0, 1], &[nan.clone(), nan.clone()], &hashes).is_empty());
        for (a, b) in [(&valued, &blind), (&blind, &numbered), (&nan, &valued)] {
            let facts = [a.clone(), b.clone()];
            assert!(
                bracket_run_links(&[0, 1], &facts, &hashes).is_empty(),
                "{facts:?}"
            );
        }
        for (a, b) in [
            (&valued, &valued),
            (&numbered, &numbered),
            (&valued, &numbered),
        ] {
            let facts = [a.clone(), b.clone()];
            assert_eq!(
                bracket_run_links(&[0, 1], &facts, &hashes),
                [(0, 1)],
                "{facts:?}"
            );
        }
        // A shot without a number or value is skipped over, like an Off shot.
        let facts = [valued.clone(), blind, valued];
        let hashes = [hash(0), hash(5), hash(20)];
        assert_eq!(bracket_run_links(&[0, 1, 2], &facts, &hashes), [(0, 2)]);
    }

    #[test]
    fn run_sanity_rejects_an_unrelated_frame() {
        let facts = [shot(EXPOSURE), shot(EXPOSURE)];
        let at = |bits| [hash(0), hash(bits)];
        assert!(bracket_run_links(&[0, 1], &facts, &at(41)).is_empty());
        // The limit is inclusive, and counts both hashes.
        assert_eq!(bracket_run_links(&[0, 1], &facts, &at(40)), [(0, 1)]);
        // 32 + 8 = 40 links; 32 + 9 = 41 does not.
        let split = [
            hash(0),
            Some(HashPair {
                phash: 0xFFFF_FFFF,
                dhash: 0xFF,
            }),
        ];
        assert_eq!(bracket_run_links(&[0, 1], &facts, &split), [(0, 1)]);
        let split_far = [
            hash(0),
            Some(HashPair {
                phash: 0xFFFF_FFFF,
                dhash: 0x1FF,
            }),
        ];
        assert!(bracket_run_links(&[0, 1], &facts, &split_far).is_empty());
    }
}
