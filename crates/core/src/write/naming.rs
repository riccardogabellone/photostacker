//! Layer and output file names (spec §5.6).
//!
//! - A layer is named after its file stem, plus its exposure when it is
//!   known: `IMG_0412 (−2 EV)`. The minus is U+2212; the value has one
//!   decimal, a trailing `.0` trimmed, and a `+` when positive.
//! - A stack is named `<first stem>_<N>.<ext>`, where N is its layer count;
//!   the k-th name tried after a collision is `<first stem>_<N>-k.<ext>`.

use crate::exif::{BracketMode, ExifData};
use crate::group::Container;

/// The minus sign of layer names.
const MINUS: char = '\u{2212}';

/// The name of the layer of the file with `stem` and `exif`.
///
/// The exposure is the bracket's `value_ev` when the shot is part of an
/// exposure bracket ([`BracketMode::Exposure`]) and the camera recorded
/// one; otherwise the exposure bias when it is recorded and non-zero;
/// otherwise there is no suffix. A value that is not finite counts as
/// unrecorded, so a non-finite `value_ev` falls back to the bias. Other
/// brackets (white balance, flash, ISO) vary something else than the
/// exposure, so their value is never shown.
pub fn layer_name(stem: &str, exif: Option<&ExifData>) -> String {
    match exif.and_then(layer_exposure) {
        Some(ev) => format!("{stem} ({} EV)", format_ev(ev)),
        None => stem.to_owned(),
    }
}

/// The file name of a stack of `layers` layers whose top layer has
/// `first_stem`, at name attempt `attempt`: 0 is `<stem>_<N>.<ext>`, k is
/// `<stem>_<N>-k.<ext>`. The extension is `psd`, `psb` or `tif`.
pub fn output_name(first_stem: &str, layers: usize, container: Container, attempt: u32) -> String {
    let extension = match container {
        Container::Psd => "psd",
        Container::Psb => "psb",
        Container::Tiff | Container::BigTiff => "tif",
    };
    match attempt {
        0 => format!("{first_stem}_{layers}.{extension}"),
        k => format!("{first_stem}_{layers}-{k}.{extension}"),
    }
}

/// The exposure, in EV, that the layer name of a file with `exif` shows, if any, before
/// its rounding to one decimal: the rule of [`layer_name`]. The inspector shows a
/// stack's range of exposures by it, so the range and the names agree.
pub fn layer_exposure(exif: &ExifData) -> Option<f32> {
    let bracketed = exif
        .bracket
        .filter(|bracket| bracket.mode == BracketMode::Exposure)
        .and_then(|bracket| bracket.value_ev)
        .filter(|ev| ev.is_finite());
    let bias = exif
        .exposure_bias_ev
        .filter(|&bias| bias != 0.0 && bias.is_finite());
    bracketed.or(bias)
}

/// `ev` rounded to one decimal, `.0` trimmed, signed: `−0.7`, `0`, `+1`: how a layer name
/// writes its exposure, and how the app writes every exposure, so the two never drift.
pub fn format_ev(ev: f32) -> String {
    // Whole tenths, so the rounding is done once and -0.04 is plain 0.
    let tenths = (f64::from(ev) * 10.0).round() as i64;
    let sign = match tenths.signum() {
        -1 => MINUS.to_string(),
        1 => "+".to_owned(),
        _ => String::new(),
    };
    let (whole, tenth) = (tenths.abs() / 10, tenths.abs() % 10);
    match tenth {
        0 => format!("{sign}{whole}"),
        _ => format!("{sign}{whole}.{tenth}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exif::BracketInfo;

    fn bracketed(mode: BracketMode, value_ev: Option<f32>, bias: Option<f32>) -> ExifData {
        ExifData {
            bracket: Some(BracketInfo {
                mode,
                shot_number: Some(1),
                value_ev,
            }),
            exposure_bias_ev: bias,
            ..ExifData::default()
        }
    }

    fn biased(bias: Option<f32>) -> ExifData {
        ExifData {
            exposure_bias_ev: bias,
            ..ExifData::default()
        }
    }

    #[test]
    fn layer_name_formats() {
        let exposure = |ev| bracketed(BracketMode::Exposure, Some(ev), None);
        let cases = [
            (exposure(-2.0), "IMG_0412 (−2 EV)"),
            (exposure(1.0), "IMG_0412 (+1 EV)"),
            (exposure(0.0), "IMG_0412 (0 EV)"),
            (exposure(-0.7), "IMG_0412 (−0.7 EV)"),
            (biased(Some(0.0)), "IMG_0412"),
            (biased(Some(1.0 / 3.0)), "IMG_0412 (+0.3 EV)"),
            // One decimal, rounded; a value that rounds to 0 has no sign.
            (exposure(-2.0 / 3.0), "IMG_0412 (−0.7 EV)"),
            (exposure(2.5), "IMG_0412 (+2.5 EV)"),
            (exposure(-0.04), "IMG_0412 (0 EV)"),
            // The bracket value wins over the bias.
            (
                bracketed(BracketMode::Exposure, Some(-1.0), Some(2.0)),
                "IMG_0412 (−1 EV)",
            ),
            // Without a bracket value, or with bracketing off, the bias.
            (
                bracketed(BracketMode::Exposure, None, Some(-1.0)),
                "IMG_0412 (−1 EV)",
            ),
            (
                bracketed(BracketMode::Off, Some(3.0), Some(1.0)),
                "IMG_0412 (+1 EV)",
            ),
            (bracketed(BracketMode::Off, Some(3.0), None), "IMG_0412"),
            // A value only shows for an exposure bracket; a
            // white-balance (Other) bracket falls back to the bias rule.
            (bracketed(BracketMode::Other, Some(0.0), None), "IMG_0412"),
            (
                bracketed(BracketMode::Other, Some(0.0), Some(1.0)),
                "IMG_0412 (+1 EV)",
            ),
            // A value that is not finite counts as unrecorded: the bias
            // stands in.
            (
                bracketed(BracketMode::Exposure, Some(f32::NAN), Some(1.0)),
                "IMG_0412 (+1 EV)",
            ),
            (
                bracketed(BracketMode::Exposure, Some(-1.0), Some(f32::NAN)),
                "IMG_0412 (−1 EV)",
            ),
            (biased(None), "IMG_0412"),
            (biased(Some(f32::NAN)), "IMG_0412"),
            (exposure(f32::INFINITY), "IMG_0412"),
        ];
        for (exif, expected) in cases {
            assert_eq!(layer_name("IMG_0412", Some(&exif)), expected, "{exif:?}");
        }
        assert_eq!(layer_name("IMG_0412", None), "IMG_0412");
        assert!(layer_name("a", Some(&exposure(-1.0))).contains('\u{2212}'));
    }

    #[test]
    fn layer_exposure_is_the_value_the_name_shows_unrounded() {
        let exposure = |ev| bracketed(BracketMode::Exposure, Some(ev), None);
        assert_eq!(layer_exposure(&exposure(-2.0 / 3.0)), Some(-2.0 / 3.0));
        assert_eq!(layer_exposure(&exposure(0.0)), Some(0.0));
        assert_eq!(
            layer_exposure(&bracketed(BracketMode::Off, Some(3.0), Some(1.0))),
            Some(1.0)
        );
        assert_eq!(layer_exposure(&biased(Some(0.0))), None);
        assert_eq!(layer_exposure(&biased(Some(f32::NAN))), None);
        assert_eq!(layer_exposure(&ExifData::default()), None);
    }

    #[test]
    fn output_name_attempts() {
        assert_eq!(
            output_name("IMG_0001", 5, Container::Psd, 0),
            "IMG_0001_5.psd"
        );
        assert_eq!(
            output_name("IMG_0001", 5, Container::Psd, 1),
            "IMG_0001_5-1.psd"
        );
        assert_eq!(
            output_name("IMG_0001", 5, Container::Psb, 2),
            "IMG_0001_5-2.psb"
        );
        assert_eq!(
            output_name("IMG_0001", 3, Container::Tiff, 0),
            "IMG_0001_3.tif"
        );
        assert_eq!(
            output_name("IMG_0001", 3, Container::BigTiff, 12),
            "IMG_0001_3-12.tif"
        );
        assert_eq!(
            output_name("IMG_0001", 3, Container::Tiff, 9_999),
            "IMG_0001_3-9999.tif"
        );
    }
}
