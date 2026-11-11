//! Colour-space fixtures: stub ICC profiles, and a hand-built CMYK JPEG,
//! which `image`'s encoder cannot write.

/// The length of an ICC profile header.
const ICC_HEADER_LEN: usize = 128;

/// A minimal ICC profile: a 128-byte v2.1 header naming the profile `class`
/// (`mntr`, `prtr`, ...) and data colour `space` (`RGB `, `GRAY`, `CMYK`,
/// ...), and an empty tag table. Decoders pass its bytes through without
/// parsing it; it describes no real colour space.
pub fn icc_stub(class: &[u8; 4], space: &[u8; 4]) -> Vec<u8> {
    let mut profile = vec![0u8; ICC_HEADER_LEN + 4];
    let len = u32::try_from(profile.len()).expect("a stub fits in u32");
    profile[0..4].copy_from_slice(&len.to_be_bytes());
    profile[8..12].copy_from_slice(&[2, 0x10, 0, 0]);
    profile[12..16].copy_from_slice(class);
    profile[16..20].copy_from_slice(space);
    profile[20..24].copy_from_slice(b"XYZ ");
    profile[36..40].copy_from_slice(b"acsp");
    // The tag count after the header stays 0.
    profile
}

/// A baseline JPEG of `width × height` pixels, all of one CMYK colour
/// `cmyk`, with an Adobe APP14 segment (transform 0: CMYK, as Photoshop
/// writes it) and an APP2 `ICC_PROFILE` segment carrying all of `icc`.
///
/// Its four components are sampled 1×1 and quantised by 1. A flat 8×8 block
/// has only a DC coefficient, `8 × (level − 128)`, so every block codes as
/// its DC difference and an end-of-block: the DC table gives each size
/// category 0..=11 a 4-bit code, the AC table gives EOB the code `00`.
pub fn cmyk_jpeg_with_icc(width: u16, height: u16, cmyk: [u8; 4], icc: &[u8]) -> Vec<u8> {
    assert!(width > 0 && height > 0, "a JPEG has at least one pixel");
    let mut out = vec![0xFF, 0xD8];
    segment(&mut out, 0xEE, &[b"Adobe", &[0, 100, 0, 0, 0, 0, 0]]);
    segment(&mut out, 0xE2, &[b"ICC_PROFILE\0", &[1, 1], icc]);
    // One 8-bit quantisation table of ones.
    segment(&mut out, 0xDB, &[&[0], &[1; 64]]);
    let [w_hi, w_lo] = width.to_be_bytes();
    let [h_hi, h_lo] = height.to_be_bytes();
    let components: Vec<u8> = (1..=4).flat_map(|id| [id, 0x11, 0]).collect();
    segment(
        &mut out,
        0xC0,
        &[&[8, h_hi, h_lo, w_hi, w_lo, 4], &components],
    );
    let mut dc_bits = [0u8; 16];
    dc_bits[3] = 12;
    let dc_values: Vec<u8> = (0..=11).collect();
    segment(&mut out, 0xC4, &[&[0x00], &dc_bits, &dc_values]);
    let mut ac_bits = [0u8; 16];
    ac_bits[1] = 2;
    segment(&mut out, 0xC4, &[&[0x10], &ac_bits, &[0x00, 0xF0]]);
    let selectors: Vec<u8> = (1..=4).flat_map(|id| [id, 0x00]).collect();
    segment(&mut out, 0xDA, &[&[4], &selectors, &[0, 63, 0]]);

    let blocks = usize::from(width.div_ceil(8)) * usize::from(height.div_ceil(8));
    let mut bits = BitWriter::default();
    let mut previous = [0i32; 4];
    for _ in 0..blocks {
        for (level, previous) in cmyk.iter().zip(&mut previous) {
            let dc = 8 * (i32::from(*level) - 128);
            let diff = dc - *previous;
            *previous = dc;
            let size = 32 - diff.unsigned_abs().leading_zeros();
            bits.put(size, 4);
            // A negative difference is sent as `diff − 1` in `size` bits.
            let extra = if diff < 0 { diff - 1 } else { diff };
            bits.put(extra as u32 & ((1 << size) - 1), size);
            // End of block.
            bits.put(0b00, 2);
        }
    }
    out.extend(bits.finish());
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

/// Appends a marker segment: `FF code`, its length, then `parts`.
fn segment(out: &mut Vec<u8>, code: u8, parts: &[&[u8]]) {
    let len: usize = 2 + parts.iter().map(|part| part.len()).sum::<usize>();
    let len = u16::try_from(len).expect("the segment fits");
    out.extend_from_slice(&[0xFF, code]);
    out.extend_from_slice(&len.to_be_bytes());
    parts.iter().for_each(|part| out.extend_from_slice(part));
}

/// Entropy-coded bits, most significant first, with a 0 stuffed after
/// every 0xFF byte.
#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    current: u8,
    used: u32,
}

impl BitWriter {
    /// Appends the low `count` bits of `value`.
    fn put(&mut self, value: u32, count: u32) {
        for bit in (0..count).rev() {
            self.current = (self.current << 1) | ((value >> bit) & 1) as u8;
            self.used += 1;
            if self.used == 8 {
                self.flush_byte();
            }
        }
    }

    fn flush_byte(&mut self) {
        self.bytes.push(self.current);
        if self.current == 0xFF {
            self.bytes.push(0x00);
        }
        self.current = 0;
        self.used = 0;
    }

    /// The bytes, the last one padded with 1 bits.
    fn finish(mut self) -> Vec<u8> {
        if self.used > 0 {
            let pad = 8 - self.used;
            self.put((1 << pad) - 1, pad);
        }
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use image::{ImageDecoder, codecs::jpeg::JpegDecoder};

    use super::*;

    #[test]
    fn stub_header_names_class_and_space() {
        let profile = icc_stub(b"prtr", b"CMYK");
        assert_eq!(profile.len(), 132);
        assert_eq!(u32::from_be_bytes(profile[0..4].try_into().unwrap()), 132);
        assert_eq!(&profile[12..16], b"prtr");
        assert_eq!(&profile[16..20], b"CMYK");
        assert_eq!(&profile[36..40], b"acsp");
    }

    #[test]
    fn cmyk_jpeg_decodes_flat_with_its_profile() {
        let icc = icc_stub(b"prtr", b"CMYK");
        // 20×12 is not a whole number of blocks; the levels give DC
        // differences of both signs and a zero one.
        let bytes = cmyk_jpeg_with_icc(20, 12, [0, 128, 255, 64], &icc);
        let mut decoder = JpegDecoder::new(std::io::Cursor::new(&bytes)).unwrap();
        assert_eq!(decoder.dimensions(), (20, 12));
        assert_eq!(decoder.icc_profile().unwrap(), Some(icc));
        let image = image::DynamicImage::from_decoder(decoder)
            .unwrap()
            .into_rgb8();
        let first = *image.get_pixel(0, 0);
        assert!(image.pixels().all(|p| *p == first), "not flat");
    }
}
