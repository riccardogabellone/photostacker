//! The canvas profile of a stack (spec §5.5): which members fit the canvas,
//! and the resolution, bit depth, alpha, colour profile and container the
//! output is written with.

use serde::{Deserialize, Serialize};

use crate::decode::Probe;
use crate::model::{OutputFormat, Warning};

/// The longest side, in pixels, a PSD may have; past it the output is a PSB.
const PSD_MAX_SIDE: u32 = 30_000;
/// The longest side, in pixels, a classic TIFF may have; past it the output
/// is a BigTIFF. A classic TIFF carries its layers with PSD widths, whose
/// limits a wider canvas would break, so it shares the PSD's side limit.
const TIFF_MAX_SIDE: u32 = PSD_MAX_SIDE;
/// The largest estimated size, in bytes, of a PSD; past it the output is a PSB.
const PSD_MAX_ESTIMATE: u128 = 2_000_000_000;
/// The largest estimated size, in bytes, of a classic TIFF; past it the output
/// is a BigTIFF.
const TIFF_MAX_ESTIMATE: u128 = 4_000_000_000;

/// The file container a stack is written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Container {
    /// Photoshop document, version 1.
    Psd,
    /// Photoshop large document format, version 2.
    Psb,
    /// Classic TIFF, with 4-byte offsets.
    Tiff,
    /// BigTIFF, with 8-byte offsets. `lowercase` makes it `bigtiff`.
    BigTiff,
}

/// How one stack is written, decided from its members' probes.
#[derive(Debug, Clone, PartialEq)]
pub struct CanvasProfile {
    /// The canvas size: the most common upright resolution among the members.
    pub width: u32,
    pub height: u32,
    /// True when any kept member is 16-bit; 8-bit layers are widened.
    pub sixteen_bit: bool,
    /// True when any kept member has alpha; the others get an opaque one.
    pub has_alpha: bool,
    /// The reference member's ICC profile, embedded in the output.
    pub icc: Option<Vec<u8>>,
    pub container: Container,
    /// The ids of the members at the canvas resolution, in layer order. The
    /// first is the reference member.
    pub kept: Vec<usize>,
    /// The members at any other resolution, in layer order, each with its
    /// [`Warning::ResolutionMismatch`].
    pub excluded: Vec<(usize, Warning)>,
    /// Warnings on kept members, in layer order: a [`Warning::IccMismatch`]
    /// for each one whose ICC profile differs from the reference member's.
    pub warnings: Vec<(usize, Warning)>,
    /// True when any two kept members' ICC profiles differ, which is exactly
    /// when `warnings` holds an [`Warning::IccMismatch`].
    pub icc_mismatch: bool,
}

/// Decides the canvas profile of a stack from its members' probes, given as
/// `(id, probe)` in layer order: `[0]` is the top layer.
///
/// The canvas is the most common `(width, height)`; on a tie, the resolution
/// of the earliest member in layer order among the tied ones wins. Members at
/// another resolution are excluded. The reference member is the first kept
/// one. The container is the large one (PSB, BigTIFF) when a side is over
/// 30,000 px or the estimate
/// `width × height × channels × bytes_per_sample × (kept + 1)`, with
/// 3 channels plus one for alpha and 1 or 2 bytes per sample, is over the
/// classic container's limit (2 GB for a PSD, 4 GB for a TIFF).
///
/// With no members, the profile is a 0×0 canvas that keeps nothing, without
/// a colour profile, in `Psd` or `Tiff` by `format`.
pub fn profile(members: &[(usize, Probe)], format: OutputFormat) -> CanvasProfile {
    let (width, height) = modal_resolution(members);
    let (kept, others): (Vec<_>, Vec<_>) = members
        .iter()
        .partition(|(_, probe)| (probe.width, probe.height) == (width, height));

    let excluded = others
        .iter()
        .map(|(id, probe)| {
            let warning = Warning::ResolutionMismatch {
                width: probe.width,
                height: probe.height,
                canvas_width: width,
                canvas_height: height,
            };
            (*id, warning)
        })
        .collect();

    let icc = kept
        .first()
        .and_then(|(_, reference)| reference.icc.clone());
    // The reference itself always matches `icc`, so it never gets a warning.
    let warnings: Vec<(usize, Warning)> = kept
        .iter()
        .filter(|(_, probe)| probe.icc != icc)
        .map(|(id, _)| (*id, Warning::IccMismatch))
        .collect();

    let sixteen_bit = kept.iter().any(|(_, probe)| probe.sixteen_bit);
    let has_alpha = kept.iter().any(|(_, probe)| probe.has_alpha);
    let estimate = estimated_size(width, height, has_alpha, sixteen_bit, kept.len());
    let container = match format {
        OutputFormat::Psd
            if width > PSD_MAX_SIDE || height > PSD_MAX_SIDE || estimate > PSD_MAX_ESTIMATE =>
        {
            Container::Psb
        }
        OutputFormat::Psd => Container::Psd,
        OutputFormat::Tiff
            if width > TIFF_MAX_SIDE || height > TIFF_MAX_SIDE || estimate > TIFF_MAX_ESTIMATE =>
        {
            Container::BigTiff
        }
        OutputFormat::Tiff => Container::Tiff,
    };

    CanvasProfile {
        width,
        height,
        sixteen_bit,
        has_alpha,
        icc,
        container,
        kept: kept.iter().map(|(id, _)| *id).collect(),
        excluded,
        icc_mismatch: !warnings.is_empty(),
        warnings,
    }
}

/// The most common `(width, height)` of `members`; on a tie, the one that
/// appears first. `(0, 0)` when there are no members.
fn modal_resolution(members: &[(usize, Probe)]) -> (u32, u32) {
    let resolution = |probe: &Probe| (probe.width, probe.height);
    let mut best = (0, 0);
    let mut best_count = 0;
    for (_, candidate) in members {
        let count = members
            .iter()
            .filter(|(_, probe)| resolution(probe) == resolution(candidate))
            .count();
        // Strictly greater, so the earliest of tied resolutions stays.
        if count > best_count {
            best = resolution(candidate);
            best_count = count;
        }
    }
    best
}

/// The uncompressed size of the output, in bytes: every layer plus the
/// composite, in 3 colour channels plus an optional alpha. One image of a
/// `u32` canvas fits in u128 with room to spare; the multiplication by the
/// image count saturates, so the estimate never overflows.
fn estimated_size(
    width: u32,
    height: u32,
    has_alpha: bool,
    sixteen_bit: bool,
    layers: usize,
) -> u128 {
    let channels: u128 = if has_alpha { 4 } else { 3 };
    let bytes_per_sample: u128 = if sixteen_bit { 2 } else { 1 };
    let images = layers as u128 + 1;
    let image = u128::from(width) * u128::from(height) * channels * bytes_per_sample;
    image.saturating_mul(images)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(width: u32, height: u32) -> Probe {
        Probe {
            width,
            height,
            sixteen_bit: false,
            has_alpha: false,
            icc: None,
        }
    }

    fn sixteen_bit(width: u32, height: u32) -> Probe {
        Probe {
            sixteen_bit: true,
            ..probe(width, height)
        }
    }

    fn with_alpha(probe: Probe) -> Probe {
        Probe {
            has_alpha: true,
            ..probe
        }
    }

    fn with_icc(probe: Probe, icc: &[u8]) -> Probe {
        Probe {
            icc: Some(icc.to_vec()),
            ..probe
        }
    }

    /// Numbers the probes with ids from 10 up, so ids are never positions.
    fn members(probes: Vec<Probe>) -> Vec<(usize, Probe)> {
        (10..).zip(probes).collect()
    }

    fn mismatch(width: u32, height: u32, canvas_width: u32, canvas_height: u32) -> Warning {
        Warning::ResolutionMismatch {
            width,
            height,
            canvas_width,
            canvas_height,
        }
    }

    #[test]
    fn modal_resolution_wins_and_others_are_excluded() {
        let group = members(vec![
            probe(6000, 4000),
            probe(6000, 4000),
            probe(4000, 3000),
            probe(6000, 4000),
            probe(3000, 2000),
        ]);
        let p = profile(&group, OutputFormat::Psd);
        assert_eq!((p.width, p.height), (6000, 4000));
        assert_eq!(p.kept, [10, 11, 13]);
        assert_eq!(
            p.excluded,
            [
                (12, mismatch(4000, 3000, 6000, 4000)),
                (14, mismatch(3000, 2000, 6000, 4000)),
            ]
        );
        assert_eq!(p.container, Container::Psd);
    }

    #[test]
    fn tie_takes_the_top_layers_resolution() {
        let group = members(vec![
            probe(3000, 2000),
            probe(6000, 4000),
            probe(3000, 2000),
            probe(6000, 4000),
        ]);
        let p = profile(&group, OutputFormat::Psd);
        assert_eq!((p.width, p.height), (3000, 2000));
        assert_eq!(p.kept, [10, 12]);
        assert_eq!(
            p.excluded,
            [
                (11, mismatch(6000, 4000, 3000, 2000)),
                (13, mismatch(6000, 4000, 3000, 2000)),
            ]
        );

        // The top layer is alone at its resolution: the tie is between the
        // two others, and the earlier of them in layer order wins.
        let group = members(vec![
            probe(100, 100),
            probe(200, 200),
            probe(300, 300),
            probe(300, 300),
            probe(200, 200),
        ]);
        let p = profile(&group, OutputFormat::Psd);
        assert_eq!((p.width, p.height), (200, 200));
        assert_eq!(p.kept, [11, 14]);
        let excluded: Vec<usize> = p.excluded.iter().map(|&(id, _)| id).collect();
        assert_eq!(excluded, [10, 12, 13]);
    }

    #[test]
    fn portrait_member_is_a_resolution_mismatch_in_landscape_group() {
        // Probes carry upright dimensions: a portrait shot (Orientation 6 of a
        // 6000×4000 sensor) is 4000×6000 and never joins a landscape canvas.
        let group = members(vec![
            probe(6000, 4000),
            probe(4000, 6000),
            probe(6000, 4000),
            probe(6000, 4000),
        ]);
        let p = profile(&group, OutputFormat::Psd);
        assert_eq!((p.width, p.height), (6000, 4000));
        assert_eq!(p.kept, [10, 12, 13]);
        assert_eq!(p.excluded, [(11, mismatch(4000, 6000, 6000, 4000))]);
    }

    #[test]
    fn any_sixteen_bit_member_makes_the_canvas_sixteen_bit() {
        let group = members(vec![probe(60, 40), sixteen_bit(60, 40), probe(60, 40)]);
        assert!(profile(&group, OutputFormat::Psd).sixteen_bit);

        let group = members(vec![probe(60, 40), probe(60, 40)]);
        assert!(!profile(&group, OutputFormat::Psd).sixteen_bit);

        // Only kept members count.
        let group = members(vec![probe(60, 40), sixteen_bit(40, 60), probe(60, 40)]);
        let p = profile(&group, OutputFormat::Psd);
        assert_eq!(p.kept, [10, 12]);
        assert!(!p.sixteen_bit);
    }

    #[test]
    fn any_alpha_member_gives_the_canvas_alpha() {
        let group = members(vec![probe(60, 40), with_alpha(probe(60, 40))]);
        assert!(profile(&group, OutputFormat::Tiff).has_alpha);

        let group = members(vec![probe(60, 40), probe(60, 40)]);
        assert!(!profile(&group, OutputFormat::Tiff).has_alpha);

        // Only kept members count.
        let group = members(vec![
            probe(60, 40),
            with_alpha(probe(40, 60)),
            probe(60, 40),
        ]);
        assert!(!profile(&group, OutputFormat::Tiff).has_alpha);
    }

    #[test]
    fn icc_from_reference_and_mismatch_flagged() {
        // The top layer is excluded, so the reference is the second member.
        // Its profile is embedded, and every other kept member whose profile
        // differs, including one without a profile, gets an IccMismatch.
        let group = members(vec![
            with_icc(probe(40, 60), b"top"),
            with_icc(probe(60, 40), b"reference"),
            with_icc(probe(60, 40), b"reference"),
            with_icc(probe(60, 40), b"other"),
            probe(60, 40),
        ]);
        let p = profile(&group, OutputFormat::Psd);
        assert_eq!(p.kept, [11, 12, 13, 14]);
        assert_eq!(p.icc.as_deref(), Some(&b"reference"[..]));
        assert!(p.icc_mismatch);
        assert_eq!(
            p.warnings,
            [(13, Warning::IccMismatch), (14, Warning::IccMismatch)]
        );

        // An excluded member's profile does not count.
        let group = members(vec![
            with_icc(probe(60, 40), b"same"),
            with_icc(probe(40, 60), b"other"),
            with_icc(probe(60, 40), b"same"),
        ]);
        let p = profile(&group, OutputFormat::Psd);
        assert_eq!(p.icc.as_deref(), Some(&b"same"[..]));
        assert!(!p.icc_mismatch);
        assert_eq!(p.warnings, []);

        // No profile anywhere is no mismatch.
        let group = members(vec![probe(60, 40), probe(60, 40)]);
        let p = profile(&group, OutputFormat::Psd);
        assert_eq!(p.icc, None);
        assert!(!p.icc_mismatch);
        assert_eq!(p.warnings, []);

        // A reference without a profile against one with a profile.
        let group = members(vec![probe(60, 40), with_icc(probe(60, 40), b"some")]);
        let p = profile(&group, OutputFormat::Psd);
        assert_eq!(p.icc, None);
        assert!(p.icc_mismatch);
        assert_eq!(p.warnings, [(11, Warning::IccMismatch)]);
    }

    #[test]
    fn psb_and_bigtiff_when_side_exceeds_30000() {
        // A classic TIFF carries its layers with PSD widths, so it has the
        // PSD's side limit too.
        for (format, large) in [
            (OutputFormat::Psd, Container::Psb),
            (OutputFormat::Tiff, Container::BigTiff),
        ] {
            for (width, height) in [(30_001, 10), (10, 30_001)] {
                let group = members(vec![probe(width, height), probe(width, height)]);
                assert_eq!(
                    profile(&group, format).container,
                    large,
                    "{format:?} {width}x{height}"
                );
            }
        }
    }

    #[test]
    fn psd_and_tiff_at_exactly_30000() {
        for (format, classic) in [
            (OutputFormat::Psd, Container::Psd),
            (OutputFormat::Tiff, Container::Tiff),
        ] {
            for (width, height) in [(30_000, 10), (10, 30_000)] {
                let group = members(vec![probe(width, height), probe(width, height)]);
                assert_eq!(
                    profile(&group, format).container,
                    classic,
                    "{format:?} {width}x{height}"
                );
            }
        }
    }

    #[test]
    fn psb_when_estimate_exceeds_2gb() {
        // 10000 × 10000 × 4 channels (RGB + alpha) × 1 byte × (4 layers + 1)
        // is exactly 2,000,000,000 bytes: still a PSD.
        let rgba = |width, height| with_alpha(probe(width, height));
        let group = members(vec![rgba(10_000, 10_000); 4]);
        assert_eq!(profile(&group, OutputFormat::Psd).container, Container::Psd);

        // One more row is past the limit.
        let group = members(vec![rgba(10_000, 10_001); 4]);
        assert_eq!(profile(&group, OutputFormat::Psd).container, Container::Psb);

        // Two bytes per sample double the estimate.
        let mut group = members(vec![rgba(10_000, 10_000); 4]);
        group[2].1.sixteen_bit = true;
        assert_eq!(profile(&group, OutputFormat::Psd).container, Container::Psb);

        // An excluded member is not a layer: 5 kept would be 2.4 GB.
        let mut group = members(vec![rgba(10_000, 10_000); 4]);
        group.push((14, rgba(10, 10)));
        let p = profile(&group, OutputFormat::Psd);
        assert_eq!(p.kept.len(), 4);
        assert_eq!(p.container, Container::Psd);
    }

    #[test]
    fn bigtiff_when_estimate_exceeds_4gb() {
        // 10000 × 10000 × 4 channels × 2 bytes × (4 layers + 1) is exactly
        // 4,000,000,000 bytes: still a classic TIFF.
        let rgba16 = |width, height| with_alpha(sixteen_bit(width, height));
        let group = members(vec![rgba16(10_000, 10_000); 4]);
        assert_eq!(
            profile(&group, OutputFormat::Tiff).container,
            Container::Tiff
        );

        let group = members(vec![rgba16(10_000, 10_001); 4]);
        assert_eq!(
            profile(&group, OutputFormat::Tiff).container,
            Container::BigTiff
        );
    }

    #[test]
    fn huge_estimate_does_not_overflow() {
        let group = members(vec![sixteen_bit(u32::MAX, u32::MAX); 1000]);
        assert_eq!(
            profile(&group, OutputFormat::Tiff).container,
            Container::BigTiff
        );
        assert_eq!(profile(&group, OutputFormat::Psd).container, Container::Psb);
    }

    #[test]
    fn empty_group_gives_an_empty_profile() {
        let p = profile(&[], OutputFormat::Psd);
        assert_eq!(
            p,
            CanvasProfile {
                width: 0,
                height: 0,
                sixteen_bit: false,
                has_alpha: false,
                icc: None,
                container: Container::Psd,
                kept: vec![],
                excluded: vec![],
                warnings: vec![],
                icc_mismatch: false,
            }
        );
        assert_eq!(profile(&[], OutputFormat::Tiff).container, Container::Tiff);
    }

    #[test]
    fn container_serializes_in_lowercase() {
        for (container, name) in [
            (Container::Psd, "psd"),
            (Container::Psb, "psb"),
            (Container::Tiff, "tiff"),
            (Container::BigTiff, "bigtiff"),
        ] {
            let json = serde_json::to_string(&container).unwrap();
            assert_eq!(json, format!("\"{name}\""));
            assert_eq!(serde_json::from_str::<Container>(&json).unwrap(), container);
        }
    }
}
