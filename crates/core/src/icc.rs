//! Generated ICC profiles for developed RAW layers and untagged Adobe RGB
//! JPEGs.
//!
//! rawler develops a RAW into sRGB primaries: either gamma-encoded with the
//! sRGB curve, or linear light (see `decode/raw/full.rs`). The layer carries
//! one of two profiles, built here byte by byte so that Photoshop reads its
//! pixels correctly. A DCF camera set to Adobe RGB writes JPEGs without a
//! profile, marked only in their EXIF (see `ColourHint` in
//! `decode/full.rs`); they carry a third one, for Adobe RGB (1998). All
//! three follow ICC.1:2001-04 (version 2.1): a display (`mntr`) RGB profile
//! with an XYZ PCS, matrix/TRC model.
//!
//! - **Header** (128 bytes): the profile size, CMM 0, version 2.1, a fixed
//!   creation date (2026-01-01 00:00:00, so the bytes never change),
//!   `acsp`, rendering intent 0 (perceptual) and the D50 PCS illuminant.
//! - **Tag table:** the tag count, then one (signature, offset, size) entry
//!   per tag. Each element starts on a 4-byte boundary.
//! - **Tags:**
//!   - `desc` (textDescriptionType): "Photostacker sRGB", "Photostacker
//!     linear sRGB" or "Photostacker Adobe RGB (1998)", with empty Unicode
//!     and ScriptCode parts;
//!   - `cprt` (textType): "Public domain";
//!   - `wtpt` (XYZType): D50, the PCS white;
//!   - `rXYZ`, `gXYZ`, `bXYZ` (XYZType): the sRGB or Adobe RGB (1998)
//!     primaries, Bradford-adapted from D65 to D50;
//!   - `rTRC`, `gTRC`, `bTRC` (curveType), one element shared by the three
//!     tags. An ICC TRC maps the encoded device value to linear light. For
//!     sRGB, it is the IEC 61966-2-1 sRGB decoding sampled at 1024 points,
//!     entry `i` at `i / 1023`. For linear, it is the empty curve (count 0),
//!     which ICC defines as the identity. For Adobe RGB (1998), it is one
//!     entry, a u8Fixed8 gamma: 563 / 256 = 2.19921875, the 2 51/256 of the
//!     Adobe RGB (1998) specification.
//!
//! XYZ numbers are s15Fixed16: signed, with 16 fraction bits.

use crate::model::RawTone;

/// The D50 white of the PCS, XYZ.
const D50: [f64; 3] = [0.9642, 1.0, 0.8249];
/// The sRGB primaries as XYZ under D50 (Bradford-adapted from D65): the
/// columns of the linear-sRGB-to-PCS matrix.
const RED: [f64; 3] = [0.4361, 0.2225, 0.0139];
const GREEN: [f64; 3] = [0.3851, 0.7169, 0.0971];
const BLUE: [f64; 3] = [0.1431, 0.0606, 0.7141];
/// The Adobe RGB (1998) primaries as XYZ under D50 (Bradford-adapted from
/// D65).
const ADOBE_RED: [f64; 3] = [0.60974, 0.31111, 0.01947];
const ADOBE_GREEN: [f64; 3] = [0.20528, 0.62567, 0.06087];
const ADOBE_BLUE: [f64; 3] = [0.14919, 0.06322, 0.74457];
/// The Adobe RGB (1998) gamma as a u8Fixed8 number.
const ADOBE_GAMMA: u16 = 563;

/// The creation date of every profile: year, month, day, hours, minutes, seconds.
const CREATED: [u16; 6] = [2026, 1, 1, 0, 0, 0];
const HEADER_LEN: usize = 128;
/// The points of the sRGB tone curve.
const SRGB_CURVE_POINTS: u16 = 1024;

/// The ICC profile of a RAW layer developed in `tone`: deterministic, a few
/// kilobytes at most. See the module doc for its contents.
pub fn rgb_profile(tone: RawTone) -> Vec<u8> {
    let (description, curve) = match tone {
        RawTone::Srgb => ("Photostacker sRGB", srgb_curve()),
        RawTone::Linear => ("Photostacker linear sRGB", Vec::new()),
    };
    display_profile(description, [RED, GREEN, BLUE], &curve)
}

/// The Adobe RGB (1998) profile of an untagged DCF Adobe RGB JPEG:
/// deterministic, under a kilobyte. See the module doc for its contents.
pub fn adobe_rgb_profile() -> Vec<u8> {
    display_profile(
        "Photostacker Adobe RGB (1998)",
        [ADOBE_RED, ADOBE_GREEN, ADOBE_BLUE],
        &[ADOBE_GAMMA],
    )
}

/// A display profile named `description`, with the D50 XYZ of the red,
/// green and blue `primaries`, and `curve` as the tone curve of all three
/// channels.
fn display_profile(description: &str, primaries: [[f64; 3]; 3], curve: &[u16]) -> Vec<u8> {
    let [red, green, blue] = primaries;
    let elements = [
        text_description(description),
        text("Public domain"),
        xyz(D50),
        xyz(red),
        xyz(green),
        xyz(blue),
        curve_type(curve),
    ];
    // Each tag names the element it points at; the three curves share one.
    let tags: [(&[u8; 4], usize); 9] = [
        (b"desc", 0),
        (b"cprt", 1),
        (b"wtpt", 2),
        (b"rXYZ", 3),
        (b"gXYZ", 4),
        (b"bXYZ", 5),
        (b"rTRC", 6),
        (b"gTRC", 6),
        (b"bTRC", 6),
    ];

    let mut offsets = Vec::with_capacity(elements.len());
    let mut end = HEADER_LEN + 4 + 12 * tags.len();
    for element in &elements {
        offsets.push(end);
        end += element.len().next_multiple_of(4);
    }

    let mut out = header(end);
    out.extend(be32(tags.len()));
    for (signature, element) in tags {
        out.extend(signature);
        out.extend(be32(offsets[element]));
        out.extend(be32(elements[element].len()));
    }
    for element in &elements {
        out.extend(element);
        out.resize(out.len().next_multiple_of(4), 0);
    }
    debug_assert_eq!(out.len(), end);
    out
}

/// The 128-byte header of a profile of `size` bytes.
fn header(size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(size);
    out.extend(be32(size));
    out.extend([0; 4]); // preferred CMM: none
    out.extend(0x0210_0000_u32.to_be_bytes()); // version 2.1.0
    out.extend(b"mntr");
    out.extend(b"RGB ");
    out.extend(b"XYZ ");
    for field in CREATED {
        out.extend(field.to_be_bytes());
    }
    out.extend(b"acsp");
    // Platform, flags, device manufacturer and model (4 bytes each), device
    // attributes (8) and rendering intent (4, perceptual): all zero.
    out.resize(68, 0);
    out.extend(xyz_numbers(D50));
    // Creator, then the reserved bytes, all zero.
    out.resize(HEADER_LEN, 0);
    out
}

/// A textDescriptionType element: the ASCII description with its NUL, then
/// an empty Unicode part (language and count) and an empty ScriptCode part
/// (code, count and its 67-byte field).
fn text_description(ascii: &str) -> Vec<u8> {
    let mut out = Vec::from(*b"desc\0\0\0\0");
    out.extend(be32(ascii.len() + 1));
    out.extend(ascii.as_bytes());
    out.push(0);
    out.extend([0; 4 + 4 + 2 + 1 + 67]);
    out
}

/// A textType element: the ASCII text with its NUL.
fn text(ascii: &str) -> Vec<u8> {
    let mut out = Vec::from(*b"text\0\0\0\0");
    out.extend(ascii.as_bytes());
    out.push(0);
    out
}

/// An XYZType element of one XYZ number.
fn xyz(value: [f64; 3]) -> Vec<u8> {
    let mut out = Vec::from(*b"XYZ \0\0\0\0");
    out.extend(xyz_numbers(value));
    out
}

/// An XYZ number: three s15Fixed16 values.
fn xyz_numbers(value: [f64; 3]) -> impl Iterator<Item = u8> {
    value
        .into_iter()
        .flat_map(|v| ((v * 65536.0).round() as i32).to_be_bytes())
}

/// A curveType element of `entries`: none is the identity curve, one is a
/// u8Fixed8 gamma.
fn curve_type(entries: &[u16]) -> Vec<u8> {
    let mut out = Vec::from(*b"curv\0\0\0\0");
    out.extend(be32(entries.len()));
    out.extend(entries.iter().flat_map(|e| e.to_be_bytes()));
    out
}

/// The sRGB decoding (IEC 61966-2-1) at `SRGB_CURVE_POINTS` evenly spaced
/// encoded values, as 16-bit linear light.
fn srgb_curve() -> Vec<u16> {
    let last = f64::from(SRGB_CURVE_POINTS - 1);
    (0..SRGB_CURVE_POINTS)
        .map(|i| {
            let encoded = f64::from(i) / last;
            let linear = if encoded <= 0.04045 {
                encoded / 12.92
            } else {
                ((encoded + 0.055) / 1.055).powf(2.4)
            };
            (linear * 65535.0).round() as u16
        })
        .collect()
}

/// A big-endian u32 of a length or offset inside a profile.
fn be32(value: usize) -> [u8; 4] {
    u32::try_from(value)
        .expect("a profile is a few kilobytes")
        .to_be_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TONES: [RawTone; 2] = [RawTone::Srgb, RawTone::Linear];

    fn be16(bytes: &[u8], at: usize) -> u16 {
        u16::from_be_bytes(bytes[at..at + 2].try_into().unwrap())
    }

    fn be32(bytes: &[u8], at: usize) -> u32 {
        u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    /// The tag table: (signature, offset, size) per entry.
    fn tags(profile: &[u8]) -> Vec<([u8; 4], usize, usize)> {
        let count = be32(profile, 128) as usize;
        (0..count)
            .map(|i| {
                let at = 132 + 12 * i;
                let sig = profile[at..at + 4].try_into().unwrap();
                (
                    sig,
                    be32(profile, at + 4) as usize,
                    be32(profile, at + 8) as usize,
                )
            })
            .collect()
    }

    /// The data of tag `sig`.
    fn tag<'a>(profile: &'a [u8], sig: &[u8; 4]) -> &'a [u8] {
        let (_, offset, size) = tags(profile)
            .into_iter()
            .find(|(s, _, _)| s == sig)
            .unwrap_or_else(|| panic!("no {} tag", String::from_utf8_lossy(sig)));
        &profile[offset..offset + size]
    }

    /// The entries of the `curv` element of tag `sig`.
    fn curve(profile: &[u8], sig: &[u8; 4]) -> Vec<u16> {
        let data = tag(profile, sig);
        assert_eq!(&data[..4], b"curv");
        assert_eq!(&data[4..8], &[0; 4], "reserved");
        let count = be32(data, 8) as usize;
        assert_eq!(data.len(), 12 + 2 * count);
        (0..count).map(|i| be16(data, 12 + 2 * i)).collect()
    }

    /// The three numbers of the `XYZ ` element of tag `sig`.
    fn xyz(profile: &[u8], sig: &[u8; 4]) -> [f64; 3] {
        let data = tag(profile, sig);
        assert_eq!(&data[..8], b"XYZ \0\0\0\0");
        assert_eq!(data.len(), 20);
        [8, 12, 16].map(|at| f64::from(be32(data, at) as i32) / 65536.0)
    }

    /// The ASCII part of the `desc` (textDescriptionType) element.
    fn description(profile: &[u8]) -> String {
        let data = tag(profile, b"desc");
        assert_eq!(&data[..8], b"desc\0\0\0\0");
        let count = be32(data, 8) as usize;
        assert_eq!(data[12 + count - 1], 0, "the ASCII count includes the NUL");
        // Then empty Unicode (language, count) and ScriptCode (code, count, 67 bytes) parts.
        let rest = &data[12 + count..];
        assert_eq!(rest.len(), 4 + 4 + 2 + 1 + 67);
        assert!(rest.iter().all(|&b| b == 0));
        String::from_utf8(data[12..12 + count - 1].to_vec()).unwrap()
    }

    /// The sRGB decoding function of IEC 61966-2-1: encoded value to linear light.
    fn srgb_to_linear(v: f64) -> f64 {
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    }

    #[test]
    fn profile_size_field_matches_length() {
        for tone in TONES {
            let profile = rgb_profile(tone);
            assert!(profile.len() >= 128, "{tone:?}");
            assert_eq!(be32(&profile, 0) as usize, profile.len(), "{tone:?}");
        }
    }

    #[test]
    fn header_is_a_v2_display_rgb_profile() {
        for tone in TONES {
            let p = rgb_profile(tone);
            assert_eq!(be32(&p, 4), 0, "CMM");
            assert_eq!(be32(&p, 8), 0x0210_0000, "version 2.1");
            assert_eq!(&p[12..16], b"mntr");
            assert_eq!(&p[16..20], b"RGB ");
            assert_eq!(&p[20..24], b"XYZ ");
            let date: Vec<u16> = (0..6).map(|i| be16(&p, 24 + 2 * i)).collect();
            assert_eq!(date, [2026, 1, 1, 0, 0, 0]);
            assert_eq!(&p[36..40], b"acsp");
            assert_eq!(be32(&p, 64), 0, "rendering intent");
            assert_eq!(
                [68, 72, 76].map(|at| be32(&p, at)),
                [0xF6D6, 0x1_0000, 0xD32D],
                "D50 illuminant"
            );
            assert!(p[84..128].iter().all(|&b| b == 0), "reserved");
        }
    }

    #[test]
    fn tag_table_offsets_in_bounds_and_aligned_to_4() {
        for tone in TONES {
            let profile = rgb_profile(tone);
            let tags = tags(&profile);
            let sigs: Vec<&[u8]> = tags.iter().map(|(s, _, _)| s.as_slice()).collect();
            assert_eq!(
                sigs,
                [
                    b"desc", b"cprt", b"wtpt", b"rXYZ", b"gXYZ", b"bXYZ", b"rTRC", b"gTRC", b"bTRC"
                ],
                "{tone:?}"
            );
            let data_start = 132 + 12 * tags.len();
            for (sig, offset, size) in &tags {
                let sig = String::from_utf8_lossy(sig);
                assert_eq!(offset % 4, 0, "{tone:?} {sig}");
                assert!(*offset >= data_start, "{tone:?} {sig}");
                assert!(offset + size <= profile.len(), "{tone:?} {sig}");
            }
            // The three curves share one element.
            let trc: Vec<_> = tags[6..].iter().map(|&(_, o, s)| (o, s)).collect();
            assert_eq!(trc, [trc[0]; 3], "{tone:?}");
        }
    }

    #[test]
    fn text_tags_and_colorants() {
        assert_eq!(
            description(&rgb_profile(RawTone::Srgb)),
            "Photostacker sRGB"
        );
        assert_eq!(
            description(&rgb_profile(RawTone::Linear)),
            "Photostacker linear sRGB"
        );
        for tone in TONES {
            let p = rgb_profile(tone);
            assert_eq!(tag(&p, b"cprt"), b"text\0\0\0\0Public domain\0");
            // s15Fixed16 is 1/65536: each value is the nearest one.
            let close = |got: [f64; 3], want: [f64; 3]| {
                got.iter()
                    .zip(want)
                    .all(|(g, w)| (g - w).abs() <= 0.5 / 65536.0)
            };
            assert!(close(xyz(&p, b"wtpt"), [0.9642, 1.0, 0.8249]));
            assert!(close(xyz(&p, b"rXYZ"), [0.4361, 0.2225, 0.0139]));
            assert!(close(xyz(&p, b"gXYZ"), [0.3851, 0.7169, 0.0971]));
            assert!(close(xyz(&p, b"bXYZ"), [0.1431, 0.0606, 0.7141]));
        }
    }

    #[test]
    fn srgb_curve_endpoints_and_midpoint() {
        let profile = rgb_profile(RawTone::Srgb);
        for sig in [b"rTRC", b"gTRC", b"bTRC"] {
            let curve = curve(&profile, sig);
            assert_eq!(curve.len(), 1024);
            assert_eq!(curve[0], 0);
            assert_eq!(curve[1023], 65535);
            // An ICC TRC maps the encoded device value to linear light, so its
            // value at 0.5 (half way between entries 511 and 512) is the sRGB
            // decoding of 0.5, about 0.214.
            let at_half = (f64::from(curve[511]) + f64::from(curve[512])) / 2.0;
            let want = srgb_to_linear(0.5) * 65535.0;
            assert!((at_half - want).abs() <= 1.0, "{at_half} vs {want}");
            assert!(curve.is_sorted(), "monotonic");
        }
    }

    #[test]
    fn linear_profile_has_identity_curves() {
        let profile = rgb_profile(RawTone::Linear);
        for sig in [b"rTRC", b"gTRC", b"bTRC"] {
            assert_eq!(curve(&profile, sig), Vec::<u16>::new());
        }
    }

    #[test]
    fn adobe_rgb_profile_is_well_formed() {
        let p = adobe_rgb_profile();
        assert_eq!(be32(&p, 0) as usize, p.len(), "size field");
        assert_eq!(be32(&p, 8), 0x0210_0000, "version 2.1");
        assert_eq!(&p[12..24], b"mntrRGB XYZ ");
        assert_eq!(&p[36..40], b"acsp");
        assert_eq!(
            [68, 72, 76].map(|at| be32(&p, at)),
            [0xF6D6, 0x1_0000, 0xD32D],
            "D50 illuminant"
        );

        let tags = tags(&p);
        let sigs: Vec<&[u8]> = tags.iter().map(|(s, _, _)| s.as_slice()).collect();
        assert_eq!(
            sigs,
            [
                b"desc", b"cprt", b"wtpt", b"rXYZ", b"gXYZ", b"bXYZ", b"rTRC", b"gTRC", b"bTRC"
            ]
        );
        let data_start = 132 + 12 * tags.len();
        for (sig, offset, size) in &tags {
            let sig = String::from_utf8_lossy(sig);
            assert_eq!(offset % 4, 0, "{sig}");
            assert!(*offset >= data_start, "{sig}");
            assert!(offset + size <= p.len(), "{sig}");
        }

        assert_eq!(description(&p), "Photostacker Adobe RGB (1998)");
        assert_eq!(tag(&p, b"cprt"), b"text\0\0\0\0Public domain\0");
        let close = |got: [f64; 3], want: [f64; 3]| {
            got.iter()
                .zip(want)
                .all(|(g, w)| (g - w).abs() <= 0.5 / 65536.0)
        };
        assert!(close(xyz(&p, b"wtpt"), [0.9642, 1.0, 0.8249]));
        assert!(close(xyz(&p, b"rXYZ"), [0.60974, 0.31111, 0.01947]));
        assert!(close(xyz(&p, b"gXYZ"), [0.20528, 0.62567, 0.06087]));
        assert!(close(xyz(&p, b"bXYZ"), [0.14919, 0.06322, 0.74457]));
        // One u8Fixed8 gamma, 563 / 256 = 2.19921875, shared by the three.
        for sig in [b"rTRC", b"gTRC", b"bTRC"] {
            assert_eq!(curve(&p, sig), [563]);
        }
        let trc: Vec<_> = tags[6..].iter().map(|&(_, o, s)| (o, s)).collect();
        assert_eq!(trc, [trc[0]; 3]);
        assert_eq!(adobe_rgb_profile(), p, "deterministic");
    }

    #[test]
    fn profiles_are_deterministic() {
        for tone in TONES {
            assert_eq!(rgb_profile(tone), rgb_profile(tone), "{tone:?}");
        }
        assert_ne!(rgb_profile(RawTone::Srgb), rgb_profile(RawTone::Linear));
    }
}
