//! The PSD/PSB container (spec §6.2).
//!
//! `write_psd` assembles one document from the layer encoder's records and
//! spill files, after the Adobe Photoshop File Formats Specification. All
//! values are big-endian. The file is, in order:
//!
//! 1. **File header** (26 bytes): `8BPS`, version 1 (PSD) or 2 (PSB), six
//!    zero bytes, the channel count (3, or 4 with alpha), height, width, the
//!    depth (8 or 16) and colour mode 3 (RGB).
//! 2. **Colour mode data**: its 4-byte length, 0.
//! 3. **Image resources**: a 4-byte length, then blocks of `8BIM`, the id, an
//!    empty Pascal name (two zero bytes), a 4-byte size and the data padded to
//!    even. The blocks are 1005 ResolutionInfo at 72 dpi, 1039 the ICC
//!    profile when the canvas has one, and 1057 VersionInfo.
//! 4. **Layer and mask information**: a long length (4 bytes, 8 in a PSB),
//!    then the layer info, the global layer mask info (length 0), and for a
//!    16-bit document the layer info again as an `8BIM` `Lr16` tagged block,
//!    the main layer info then having length 0. The layer info is the layer
//!    count (negative on a canvas with alpha: the merged image's alpha is its
//!    transparency), the records bottom layer first, then each layer's
//!    channel data in the same order, zero-padded to a multiple of 4.
//!    `LayerInfo`, `write_layer_info` and `write_layer_block` serialise it;
//!    the layered-TIFF writer shares them for its ImageSourceData.
//! 5. **Image data**: compression 0 (raw), then the `.merged` spill: the top
//!    layer alone, R, G, B, then alpha on a canvas with alpha. There the
//!    colour is blended against white with each pixel's alpha, Photoshop's
//!    convention for a document with transparency, which readers of the
//!    merged image undo (the layer encoder does it, spec §6.2); the layers
//!    keep their straight colour. Where layers below show through a partly
//!    transparent top layer, Photoshop's own composite would show them too:
//!    the merged image does not.
//!
//! Padding follows what Photoshop writes where the specification is vague:
//! - the layer info length counts its padding to 4;
//! - a layer record's `luni` length counts its padding to 4;
//! - the `Lr16` length is its data rounded up to even, as the specification
//!   says, and the padding to 4 that follows is not counted, as in
//!   Photoshop's own 16-bit files (CS3 and CC 2017 both write an `Lr16` of
//!   length 1218 followed by two zero bytes).
//!
//! The encoder adds layers top first, so the records and their channel data
//! are written in the reverse of the encoder's order: Photoshop stores the
//! bottom layer first. Channel and merged data are copied from the spill
//! files in 1 MiB chunks; nothing is loaded whole.

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};

use super::layers::{LayerRecord, LengthWidth, Spill};
use crate::group::{CanvasProfile, Container};

/// The size of the copy buffer, and of the output buffer.
pub(super) const COPY_CHUNK: usize = 1 << 20;
/// The longest side of a PSD; the PSB limit is ten times larger.
pub(super) const PSD_MAX_SIDE: u32 = 30_000;
pub(super) const PSB_MAX_SIDE: u32 = 300_000;
/// Image resource ids.
const RESOLUTION_INFO: u16 = 1005;
const ICC_PROFILE: u16 = 1039;
const VERSION_INFO: u16 = 1057;
/// The writer and reader names of VersionInfo.
pub(crate) const APP_NAME: &str = "Photostacker";
/// Colour mode RGB.
const RGB_MODE: u16 = 3;
/// Image data compression: raw.
const RAW: u16 = 0;
/// 72 dpi as a 16.16 fixed-point number.
const DPI_72: u32 = 72 << 16;
/// ResolutionInfo units: resolution in pixels per inch, size in inches.
const PIXELS_PER_INCH: u16 = 1;
const INCHES: u16 = 1;

/// Writes the document of `canvas` to `out`, at its current position, from
/// the layer `records` (top layer first, as the encoder made them) and the
/// `spill` files that hold their data.
///
/// `canvas.container` must be `Psd` or `Psb`, and the records and the spill
/// must come from an encoder made for that canvas, with its widths (the
/// encoder's `LengthWidth`). Fails with `InvalidInput`, before writing
/// anything, when the canvas, the records or the spill files do not fit
/// together or do not fit the container, which includes a PSD of more than
/// `i32::MAX` bytes and a merged spill made for a TIFF canvas with alpha.
pub(crate) fn write_psd(
    out: &mut File,
    canvas: &CanvasProfile,
    records: &[LayerRecord],
    spill: &Spill,
) -> io::Result<()> {
    let widths = check_canvas(canvas)?;
    check_spill_widths(spill, widths)?;
    if spill.merged_on_white() != canvas.has_alpha {
        return Err(invalid(
            "the merged image was encoded for another container or canvas: a PSD or PSB \
             stores it blended against white exactly when the canvas has alpha"
                .into(),
        ));
    }
    let layer_info = LayerInfo::new(canvas, records, widths)?;
    let mut layers = File::open(spill.layers_path())?;
    let mut merged = File::open(spill.merged_path())?;
    check_spill(&layers, layer_info.channel_data(), "layers")?;
    let merged_len = merged_len(canvas);
    check_spill(&merged, merged_len, "merged")?;

    let resources = image_resources(canvas);
    let resources_len = u32::try_from(resources.len())
        .map_err(|_| invalid("the image resources exceed 4 GiB".into()))?;
    let section_len = layer_and_mask_len(&layer_info, widths, canvas.sixteen_bit);
    // Every PSD length field fits once the whole file does.
    check_file_len(
        widths,
        file_len(widths, u64::from(resources_len), section_len, merged_len),
    )?;

    let mut out = BufWriter::with_capacity(COPY_CHUNK, out);
    write_header(&mut out, canvas, widths)?;
    out.write_all(&0u32.to_be_bytes())?; // colour mode data
    out.write_all(&resources_len.to_be_bytes())?;
    out.write_all(&resources)?;

    out.write_all(&long(widths, section_len)?)?;
    if canvas.sixteen_bit {
        out.write_all(&long(widths, 0)?)?; // the layer info is in Lr16
        out.write_all(&0u32.to_be_bytes())?; // global layer mask info
        write_layer_block(&mut out, b"Lr16", widths, &layer_info, &mut layers)?;
    } else {
        out.write_all(&long(widths, layer_info.padded_len())?)?;
        write_layer_info(&mut out, &layer_info, &mut layers)?;
        out.write_all(&0u32.to_be_bytes())?; // global layer mask info
    }

    out.write_all(&RAW.to_be_bytes())?;
    copy_exact(&mut merged, &mut out, merged_len)?;
    out.flush()
}

/// The widths of the canvas's container. Fails with `InvalidInput` for a
/// TIFF container, an empty canvas, or a side past the container's limit.
fn check_canvas(canvas: &CanvasProfile) -> io::Result<LengthWidth> {
    let (widths, max_side, name) = match canvas.container {
        Container::Psd => (LengthWidth::Psd, PSD_MAX_SIDE, "PSD"),
        Container::Psb => (LengthWidth::Psb, PSB_MAX_SIDE, "PSB"),
        other => return Err(invalid(format!("a {other:?} canvas is not a PSD or PSB"))),
    };
    let (width, height) = (canvas.width, canvas.height);
    if width == 0 || height == 0 || width > max_side || height > max_side {
        return Err(invalid(format!(
            "a {width}x{height} canvas does not fit a {name}, whose sides are 1 to {max_side}"
        )));
    }
    Ok(widths)
}

/// Fails with `InvalidInput` unless the spill's channels were encoded with
/// the container's `widths`.
pub(super) fn check_spill_widths(spill: &Spill, widths: LengthWidth) -> io::Result<()> {
    if spill.widths() != widths {
        return Err(invalid(format!(
            "the layers were encoded for {:?}, the container needs {widths:?}",
            spill.widths()
        )));
    }
    Ok(())
}

/// Checks that the records describe a stack of this canvas whose channels
/// fill `.layers` in order without gaps, and returns their total length.
fn check_records(
    canvas: &CanvasProfile,
    records: &[LayerRecord],
    widths: LengthWidth,
) -> io::Result<u64> {
    if records.is_empty() {
        return Err(invalid("a document needs at least one layer".into()));
    }
    if records.len() > i16::MAX as usize {
        return Err(invalid(format!(
            "{} layers, past a PSD's {}",
            records.len(),
            i16::MAX
        )));
    }
    let ids: &[i16] = if canvas.has_alpha {
        &[-1, 0, 1, 2]
    } else {
        &[0, 1, 2]
    };
    let mut offset = 0u64;
    for record in records {
        if !record.channels.iter().map(|c| c.id).eq(ids.iter().copied()) {
            return Err(invalid(format!(
                "layer {:?} has channels {:?}, the canvas {ids:?}",
                record.name,
                record.channels.iter().map(|c| c.id).collect::<Vec<_>>()
            )));
        }
        for channel in &record.channels {
            if channel.offset != offset || channel.len < 2 {
                return Err(invalid(format!(
                    "layer {:?}: channel {} at {}+{} does not follow the one before at {offset}",
                    record.name, channel.id, channel.offset, channel.len
                )));
            }
            if widths == LengthWidth::Psd && channel.len > u64::from(u32::MAX) {
                return Err(invalid(format!(
                    "a channel of {} bytes does not fit a PSD; the canvas needs PSB",
                    channel.len
                )));
            }
            offset += channel.len;
        }
    }
    Ok(offset)
}

/// Fails unless the spill file holds exactly `expected` bytes.
pub(super) fn check_spill(file: &File, expected: u64, what: &str) -> io::Result<()> {
    let len = file.metadata()?.len();
    if len != expected {
        return Err(invalid(format!(
            "the {what} spill holds {len} bytes, not the {expected} the stack needs"
        )));
    }
    Ok(())
}

/// The length of the merged image data after its compression code: every
/// channel's plane, uncompressed.
pub(super) fn merged_len(canvas: &CanvasProfile) -> u64 {
    u64::from(canvas.width)
        * u64::from(canvas.height)
        * u64::from(channel_count(canvas))
        * if canvas.sixteen_bit { 2 } else { 1 }
}

pub(super) fn channel_count(canvas: &CanvasProfile) -> u16 {
    if canvas.has_alpha { 4 } else { 3 }
}

fn write_header(
    out: &mut impl Write,
    canvas: &CanvasProfile,
    widths: LengthWidth,
) -> io::Result<()> {
    let version: u16 = match widths {
        LengthWidth::Psd => 1,
        LengthWidth::Psb => 2,
    };
    let depth: u16 = if canvas.sixteen_bit { 16 } else { 8 };
    out.write_all(b"8BPS")?;
    out.write_all(&version.to_be_bytes())?;
    out.write_all(&[0; 6])?;
    out.write_all(&channel_count(canvas).to_be_bytes())?;
    out.write_all(&canvas.height.to_be_bytes())?;
    out.write_all(&canvas.width.to_be_bytes())?;
    out.write_all(&depth.to_be_bytes())?;
    out.write_all(&RGB_MODE.to_be_bytes())
}

/// The image resource blocks: ResolutionInfo, the ICC profile when the
/// canvas has one, and VersionInfo.
fn image_resources(canvas: &CanvasProfile) -> Vec<u8> {
    let mut out = Vec::new();
    resource(&mut out, RESOLUTION_INFO, &resolution_info());
    if let Some(icc) = &canvas.icc {
        resource(&mut out, ICC_PROFILE, icc);
    }
    resource(&mut out, VERSION_INFO, &version_info());
    out
}

/// One resource block, with an empty name, its data padded to even.
fn resource(out: &mut Vec<u8>, id: u16, data: &[u8]) {
    out.extend_from_slice(b"8BIM");
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&[0, 0]); // the empty Pascal name, padded to even
    // The cast would truncate a profile past 4 GiB, but such a block never
    // reaches the file: `write_psd` refuses it with the `u32::try_from` of
    // the whole resources' length, and its file length check would too.
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
    if data.len() % 2 == 1 {
        out.push(0);
    }
}

/// ResolutionInfo (Photoshop API Guide, appendix A): horizontal resolution
/// as 16.16 fixed, its unit, the width unit, then the same vertically.
fn resolution_info() -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    for _ in 0..2 {
        out.extend_from_slice(&DPI_72.to_be_bytes());
        out.extend_from_slice(&PIXELS_PER_INCH.to_be_bytes());
        out.extend_from_slice(&INCHES.to_be_bytes());
    }
    out
}

/// VersionInfo: version 1, `hasRealMergedData` 1, the writer and reader
/// names, file version 1.
fn version_info() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&1u32.to_be_bytes());
    out.push(1);
    unicode_string(&mut out, APP_NAME);
    unicode_string(&mut out, APP_NAME);
    out.extend_from_slice(&1u32.to_be_bytes());
    out
}

/// A Unicode string: a 4-byte count of UTF-16 code units, then the units,
/// without a terminator (as Photoshop writes VersionInfo).
fn unicode_string(out: &mut Vec<u8>, text: &str) {
    let units: Vec<u16> = text.encode_utf16().collect();
    out.extend_from_slice(&(units.len() as u32).to_be_bytes());
    out.extend(units.iter().flat_map(|u| u.to_be_bytes()));
}

/// One layer record: the full-canvas rectangle, the channel records, normal
/// blending at full opacity, and the extra data.
fn layer_record(
    canvas: &CanvasProfile,
    record: &LayerRecord,
    widths: LengthWidth,
) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    // Top, left, bottom, right; the sides are at most 300,000, so they fit.
    for edge in [0, 0, canvas.height, canvas.width] {
        out.extend_from_slice(&(edge as i32).to_be_bytes());
    }
    out.extend_from_slice(&(record.channels.len() as u16).to_be_bytes());
    for channel in &record.channels {
        out.extend_from_slice(&channel.id.to_be_bytes());
        out.extend_from_slice(&long(widths, channel.len)?);
    }
    out.extend_from_slice(b"8BIMnorm");
    // Opacity 255; clipping 0 (base); flags 0 (visible, not protected);
    // the filler.
    out.extend_from_slice(&[255, 0, 0, 0]);
    let extra = extra_data(&record.name);
    out.extend_from_slice(&(extra.len() as u32).to_be_bytes());
    out.extend_from_slice(&extra);
    Ok(out)
}

/// A layer record's extra data: no mask (length 0), no blending ranges
/// (length 0), the Pascal name, then the `luni` block. Its length is a
/// multiple of 4.
fn extra_data(name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&pascal_name(name));
    out.extend_from_slice(&luni_block(name));
    out
}

/// The ASCII fallback of `name` as a Pascal string: `−` (U+2212) becomes
/// `-`, any other non-ASCII character `?`, cut to 255 bytes; then zero
/// padding, so that the length byte and the name fill a multiple of 4.
fn pascal_name(name: &str) -> Vec<u8> {
    let ascii: Vec<u8> = name
        .chars()
        .map(|c| match c {
            '\u{2212}' => b'-',
            c if c.is_ascii() => c as u8,
            _ => b'?',
        })
        .take(255)
        .collect();
    let mut out = Vec::with_capacity(ascii.len() + 4);
    out.push(ascii.len() as u8);
    out.extend_from_slice(&ascii);
    pad_to_4(&mut out);
    out
}

/// The `luni` tagged block of `name`: `8BIM`, `luni`, a 4-byte length (in a
/// PSB too), then the name as a Unicode string, zero-padded to a multiple
/// of 4. The length counts the padding, so a reader that rounds it up to
/// even, as the specification says, and one that rounds it up to 4 both
/// land on the next block.
fn luni_block(name: &str) -> Vec<u8> {
    let mut data = Vec::new();
    unicode_string(&mut data, name);
    pad_to_4(&mut data);
    let mut out = Vec::with_capacity(12 + data.len());
    out.extend_from_slice(b"8BIMluni");
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(&data);
    out
}

/// The layer info of a stack without its length field: the layer count,
/// the records, and the channel data, zero-padded to a multiple of 4. The
/// same bytes serve the PSD layer info section, the PSD `Lr16` block, and
/// the `Layr` or `Lr16` block of a layered TIFF's ImageSourceData, so both
/// writers serialise it here, with `write_layer_info` or
/// `write_layer_block`.
pub(crate) struct LayerInfo {
    /// The layer count as written: negative on a canvas with alpha, which
    /// tells Photoshop that the merged image's first alpha channel is its
    /// transparency.
    count: i16,
    /// Every record, bottom layer first.
    record_bytes: Vec<u8>,
    /// Each layer's channels as `(offset, length)` in `.layers`, bottom
    /// layer first.
    spans: Vec<(u64, u64)>,
    /// The total length of the channel data in `.layers`.
    channel_data: u64,
}

impl LayerInfo {
    /// The layer info of `records`, the encoder's, top layer first. Fails
    /// with `InvalidInput` unless they describe a stack of `canvas` whose
    /// channels fill `.layers` in order, and fit `widths`.
    pub(crate) fn new(
        canvas: &CanvasProfile,
        records: &[LayerRecord],
        widths: LengthWidth,
    ) -> io::Result<Self> {
        let channel_data = check_records(canvas, records, widths)?;
        // Records bottom layer first; each one is small.
        let record_bytes = records
            .iter()
            .rev()
            .map(|record| layer_record(canvas, record, widths))
            .collect::<io::Result<Vec<_>>>()?
            .concat();
        // A record's channels are contiguous in `.layers`.
        let spans = records
            .iter()
            .rev()
            .map(|record| {
                let len = record.channels.iter().map(|c| c.len).sum();
                (record.channels[0].offset, len)
            })
            .collect();
        // At most `i16::MAX` layers, which `check_records` ensures.
        let count = records.len() as i16;
        Ok(Self {
            count: if canvas.has_alpha { -count } else { count },
            record_bytes,
            spans,
            channel_data,
        })
    }

    /// The total length of the channel data, which `.layers` must hold.
    pub(crate) fn channel_data(&self) -> u64 {
        self.channel_data
    }

    fn len(&self) -> u64 {
        2 + self.record_bytes.len() as u64 + self.channel_data
    }

    /// The length with its padding to 4: the bytes `write_layer_info`
    /// writes, and the value of the PSD layer info length field.
    pub(crate) fn padded_len(&self) -> u64 {
        self.len().next_multiple_of(4)
    }

    /// The value of a tagged block's length field (`Lr16`, `Layr`): the
    /// length rounded up to even. The rest of the padding to 4 follows the
    /// data, uncounted.
    fn block_len(&self) -> u64 {
        self.len().next_multiple_of(2)
    }

    /// The bytes `write_layer_block` writes: the signature, the key, the
    /// long length and the padded layer info.
    pub(crate) fn block_size(&self, widths: LengthWidth) -> u64 {
        8 + long_size(widths) + self.padded_len()
    }
}

/// Writes `info`: the count, the records, then their channel data copied
/// from `layers` bottom layer first, zero-padded to a multiple of 4.
pub(crate) fn write_layer_info(
    out: &mut impl Write,
    info: &LayerInfo,
    layers: &mut File,
) -> io::Result<()> {
    out.write_all(&info.count.to_be_bytes())?;
    out.write_all(&info.record_bytes)?;
    for &(start, len) in &info.spans {
        layers.seek(SeekFrom::Start(start))?;
        copy_exact(layers, out, len)?;
    }
    let padding = (info.padded_len() - info.len()) as usize;
    out.write_all(&[0; 3][..padding])
}

/// Writes `info` as the tagged block `key`: `8BIM`, the key, a long length
/// (`Layr` and `Lr16` are among the keys whose length is 8 bytes in a PSB),
/// and the layer info. The length is the data rounded up to even; the
/// padding to 4 that follows is not counted, as in Photoshop's own files.
pub(crate) fn write_layer_block(
    out: &mut impl Write,
    key: &[u8; 4],
    widths: LengthWidth,
    info: &LayerInfo,
    layers: &mut File,
) -> io::Result<()> {
    out.write_all(b"8BIM")?;
    out.write_all(key)?;
    out.write_all(&long(widths, info.block_len())?)?;
    write_layer_info(out, info, layers)
}

/// The length of the layer and mask information section after its own
/// length field.
fn layer_and_mask_len(layer_info: &LayerInfo, widths: LengthWidth, sixteen_bit: bool) -> u64 {
    let long_len = long_size(widths);
    let global_mask = 4;
    if sixteen_bit {
        // An empty layer info, the global mask, then the `Lr16` block.
        long_len + global_mask + layer_info.block_size(widths)
    } else {
        long_len + layer_info.padded_len() + global_mask
    }
}

/// The size of the whole file: the header, the colour mode data and image
/// resources with their lengths, the layer and mask information with its
/// long length, and the merged image with its compression code.
fn file_len(widths: LengthWidth, resources_len: u64, section_len: u64, merged_len: u64) -> u64 {
    26 + 4 + 4 + resources_len + long_size(widths) + section_len + 2 + merged_len
}

/// Fails with `InvalidInput` when a PSD would exceed `i32::MAX` bytes,
/// Photoshop's 2 GB limit for version 1. PackBits on noisy layers can push
/// a file past the uncompressed estimate that chose PSD over PSB.
fn check_file_len(widths: LengthWidth, file_len: u64) -> io::Result<()> {
    if widths == LengthWidth::Psd && file_len > i32::MAX as u64 {
        return Err(invalid(format!(
            "the document takes {file_len} bytes, past a PSD's {} limit; it needs PSB",
            i32::MAX
        )));
    }
    Ok(())
}

/// The size of a long length field: 4 bytes in a PSD, 8 in a PSB.
pub(super) fn long_size(widths: LengthWidth) -> u64 {
    match widths {
        LengthWidth::Psd => 4,
        LengthWidth::Psb => 8,
    }
}

/// `value` as a long length field. Fails with `InvalidInput` when it does
/// not fit a PSD's 4 bytes.
pub(super) fn long(widths: LengthWidth, value: u64) -> io::Result<Vec<u8>> {
    match widths {
        LengthWidth::Psd => u32::try_from(value)
            .map(|v| v.to_be_bytes().to_vec())
            .map_err(|_| invalid(format!("{value} does not fit a PSD length; it needs PSB"))),
        LengthWidth::Psb => Ok(value.to_be_bytes().to_vec()),
    }
}

/// Copies exactly `len` bytes from `from` to `to`, a chunk at a time.
/// Fails with `UnexpectedEof` if `from` ends first.
pub(super) fn copy_exact(from: &mut File, to: &mut impl Write, len: u64) -> io::Result<()> {
    let mut chunk = vec![0; len.min(COPY_CHUNK as u64) as usize];
    let mut left = len;
    while left > 0 {
        let n = left.min(chunk.len() as u64) as usize;
        from.read_exact(&mut chunk[..n])?;
        to.write_all(&chunk[..n])?;
        left -= n as u64;
    }
    Ok(())
}

fn pad_to_4(bytes: &mut Vec<u8>) {
    bytes.resize(bytes.len().next_multiple_of(4), 0);
}

pub(super) fn invalid(why: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, why)
}

/// The tests, and the fixtures the layered-TIFF tests share.
#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::decode::{FullImage, ImageSource, Samples};
    use crate::write::layers::{ChannelRecord, LayerEncoder};
    use crate::write::{Compression16, WriteOptions};
    use photostacker_testkit::psd_reader::{self, Layer, Psd};

    pub(in crate::write) const ZIP: WriteOptions = WriteOptions {
        compression16: Compression16::Zip,
    };
    pub(in crate::write) const RLE16: WriteOptions = WriteOptions {
        compression16: Compression16::Rle,
    };
    const MINUS: char = '\u{2212}';

    pub(in crate::write) fn canvas(
        width: u32,
        height: u32,
        sixteen_bit: bool,
        has_alpha: bool,
        container: Container,
    ) -> CanvasProfile {
        CanvasProfile {
            width,
            height,
            sixteen_bit,
            has_alpha,
            icc: None,
            container,
            kept: vec![],
            excluded: vec![],
            warnings: vec![],
            icc_mismatch: false,
        }
    }

    fn image(width: u32, height: u32, channels: u8, samples: Samples) -> FullImage {
        FullImage {
            width,
            height,
            channels,
            samples,
            icc: None,
            source: ImageSource::Decoded,
        }
    }

    /// The `(x, y, c)` of every sample, in interleaved order.
    fn coordinates(width: u32, height: u32, channels: u8) -> impl Iterator<Item = (u32, u32, u8)> {
        (0..height)
            .flat_map(move |y| (0..width).flat_map(move |x| (0..channels).map(move |c| (x, y, c))))
    }

    /// An 8-bit image whose sample `c` at `(x, y)` is `f(x, y, c)`.
    pub(in crate::write) fn image8(
        width: u32,
        height: u32,
        channels: u8,
        f: impl Fn(u32, u32, u8) -> u8,
    ) -> FullImage {
        let samples = coordinates(width, height, channels)
            .map(|(x, y, c)| f(x, y, c))
            .collect();
        image(width, height, channels, Samples::U8(samples))
    }

    /// A 16-bit image whose sample `c` at `(x, y)` is `f(x, y, c)`.
    pub(in crate::write) fn image16(
        width: u32,
        height: u32,
        channels: u8,
        f: impl Fn(u32, u32, u8) -> u16,
    ) -> FullImage {
        let samples = coordinates(width, height, channels)
            .map(|(x, y, c)| f(x, y, c))
            .collect();
        image(width, height, channels, Samples::U16(samples))
    }

    pub(in crate::write) fn solid8(width: u32, height: u32, rgb: [u8; 3]) -> FullImage {
        image8(width, height, 3, |_, _, c| rgb[usize::from(c)])
    }

    /// Channel `id` of `image` as the document stores it: 8-bit samples
    /// widened by 257 on a 16-bit canvas, a missing alpha opaque.
    pub(in crate::write) fn plane(image: &FullImage, id: i16, sixteen_bit: bool) -> Vec<u16> {
        let n = usize::from(image.channels);
        let sample = if id == -1 { 3 } else { id as usize };
        if sample >= n {
            let opaque = if sixteen_bit { u16::MAX } else { 255 };
            return vec![opaque; (image.width * image.height) as usize];
        }
        match &image.samples {
            Samples::U8(s) => s
                .iter()
                .skip(sample)
                .step_by(n)
                .map(|&v| u16::from(v) * if sixteen_bit { 257 } else { 1 })
                .collect(),
            Samples::U16(s) => s.iter().skip(sample).step_by(n).copied().collect(),
        }
    }

    /// The channel ids of a layer record (alpha first) and of the merged
    /// image (alpha last).
    pub(in crate::write) fn ids(canvas: &CanvasProfile) -> (Vec<i16>, Vec<i16>) {
        if canvas.has_alpha {
            (vec![-1, 0, 1, 2], vec![0, 1, 2, -1])
        } else {
            (vec![0, 1, 2], vec![0, 1, 2])
        }
    }

    /// Encodes `layers`, top layer first, on `canvas`, writes the document
    /// to a `.partial` file as the stack writer will, and returns its bytes.
    fn write(canvas: &CanvasProfile, opts: WriteOptions, layers: &[(&str, FullImage)]) -> Vec<u8> {
        write_with_records(canvas, opts, layers).0
    }

    /// `write`, with the encoder's records.
    fn write_with_records(
        canvas: &CanvasProfile,
        opts: WriteOptions,
        layers: &[(&str, FullImage)],
    ) -> (Vec<u8>, Vec<LayerRecord>) {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("stack.psd");
        let widths = match canvas.container {
            Container::Psb => LengthWidth::Psb,
            _ => LengthWidth::Psd,
        };
        let mut encoder = LayerEncoder::new(&final_path, canvas, widths, opts).unwrap();
        for (name, image) in layers {
            encoder.add_layer(name, image.clone()).unwrap();
        }
        let (records, spill) = encoder.finish().unwrap();
        let partial = dir.path().join("stack.psd.partial");
        let mut out = File::create(&partial).unwrap();
        write_psd(&mut out, canvas, &records, &spill).unwrap();
        drop(out);
        (std::fs::read(partial).unwrap(), records)
    }

    /// The layer info's length before any padding, counted from the
    /// records after the specification, without the writer's helpers: the
    /// layer count, then per layer its record (rectangle, channel count,
    /// channel records, blend signature and key, opacity to filler, extra
    /// length, extra data) and its channel data. The extra data is the two
    /// empty lengths, the Pascal name (its length byte and at most 255
    /// characters, one byte each, padded to 4) and the `luni` block (12
    /// bytes of header, then a 4-byte count and 2 bytes per UTF-16 unit,
    /// padded to 4).
    fn unpadded_layer_info_len(records: &[LayerRecord], widths: LengthWidth) -> u64 {
        let channel_len_size = match widths {
            LengthWidth::Psd => 4,
            LengthWidth::Psb => 8,
        };
        let per_layer = |r: &LayerRecord| {
            let channels = r.channels.len() as u64;
            let pascal = (1 + r.name.chars().count().min(255) as u64).next_multiple_of(4);
            let luni = 12 + (4 + 2 * r.name.encode_utf16().count() as u64).next_multiple_of(4);
            let extra = 4 + 4 + pascal + luni;
            let data: u64 = r.channels.iter().map(|c| c.len).sum();
            16 + 2 + channels * (2 + channel_len_size) + 8 + 4 + 4 + extra + data
        };
        2 + records.iter().map(per_layer).sum::<u64>()
    }

    fn parse(bytes: &[u8]) -> Psd {
        psd_reader::read(bytes).unwrap()
    }

    /// Checks `psd` against `layers`, top layer first: the file holds them
    /// bottom layer first, and the composite is the top layer.
    fn assert_round_trip(psd: &Psd, canvas: &CanvasProfile, layers: &[(&str, FullImage)]) {
        let (record_ids, merged_ids) = ids(canvas);
        let sixteen_bit = canvas.sixteen_bit;
        assert_eq!((psd.width, psd.height), (canvas.width, canvas.height));
        assert_eq!(psd.depth, if sixteen_bit { 16 } else { 8 });
        assert_eq!(usize::from(psd.channels), merged_ids.len());
        assert_eq!(psd.layers.len(), layers.len());
        let count = layers.len() as i16;
        let signed = if canvas.has_alpha { -count } else { count };
        assert_eq!(psd.layer_count, signed, "negative exactly with alpha");
        for (layer, (name, image)) in psd.layers.iter().rev().zip(layers) {
            assert_layer(layer, name, canvas);
            let channel_ids: Vec<i16> = layer.channels.iter().map(|c| c.0).collect();
            assert_eq!(channel_ids, record_ids, "layer {name}");
            for (id, samples) in &layer.channels {
                assert_eq!(
                    samples,
                    &plane(image, *id, sixteen_bit),
                    "{name}, channel {id}"
                );
            }
        }
        assert_eq!(
            psd.merged,
            merged_planes(&layers[0].1, canvas),
            "the composite is the top layer, on white where it has alpha"
        );
    }

    /// The merged image of a document whose top layer is `top`: its planes
    /// at canvas depth, R, G, B, then alpha when the canvas has it. With
    /// alpha each colour sample is blended against white with its pixel's
    /// alpha, `(c * a + max * (max - a) + max / 2) / max` (spec §6.2).
    fn merged_planes(top: &FullImage, canvas: &CanvasProfile) -> Vec<Vec<u16>> {
        let (_, merged_ids) = ids(canvas);
        let sixteen_bit = canvas.sixteen_bit;
        let mut planes: Vec<Vec<u16>> = merged_ids
            .iter()
            .map(|&id| plane(top, id, sixteen_bit))
            .collect();
        if canvas.has_alpha {
            let max: u64 = if sixteen_bit { 65535 } else { 255 };
            let (colour, alpha) = planes.split_at_mut(3);
            for plane in colour {
                for (c, &a) in plane.iter_mut().zip(&alpha[0]) {
                    let (c64, a64) = (u64::from(*c), u64::from(a));
                    *c = ((c64 * a64 + max * (max - a64) + max / 2) / max) as u16;
                }
            }
        }
        planes
    }

    pub(in crate::write) fn assert_layer(layer: &Layer, name: &str, canvas: &CanvasProfile) {
        assert_eq!(layer.name_unicode.as_deref(), Some(name));
        assert!(layer.name_pascal.is_ascii());
        assert_eq!(
            layer.rect,
            (0, 0, canvas.height as i32, canvas.width as i32)
        );
        assert_eq!(&layer.blend, b"norm");
        assert_eq!(layer.opacity, 255);
        assert_eq!(layer.clipping, 0, "base clipping");
        assert_eq!(layer.flags, 0, "visible, not protected");
    }

    /// The offset of the layer and mask information section's length:
    /// after the 26-byte header, the empty colour mode data and the
    /// resources.
    fn layer_section_offset(bytes: &[u8]) -> usize {
        assert_eq!(bytes[26..30], [0; 4], "empty colour mode data");
        34 + u32::from_be_bytes(bytes[30..34].try_into().unwrap()) as usize
    }

    fn u32_at(bytes: &[u8], at: usize) -> u64 {
        u64::from(u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()))
    }

    fn u64_at(bytes: &[u8], at: usize) -> u64 {
        u64::from_be_bytes(bytes[at..at + 8].try_into().unwrap())
    }

    #[test]
    fn psd_8bit_round_trips_layers_names_order_pixels() {
        let canvas = canvas(8, 6, false, false, Container::Psd);
        let names = [
            "IMG_0001 (0 EV)".to_owned(),
            "IMG_0002 (+1 EV)".to_owned(),
            format!("IMG_0003 ({MINUS}1 EV)"),
        ];
        let colours = [[200, 10, 10], [10, 200, 10], [10, 10, 200]];
        let layers: Vec<(&str, FullImage)> = names
            .iter()
            .zip(colours)
            .map(|(name, rgb)| (name.as_str(), solid8(8, 6, rgb)))
            .collect();
        let (bytes, records) = write_with_records(&canvas, ZIP, &layers);
        let psd = parse(&bytes);

        assert_eq!(psd.version, 1);
        assert_round_trip(&psd, &canvas, &layers);
        // The bottom layer comes first in the file, the top one last.
        assert_eq!(
            psd.layers[0].name_unicode.as_deref(),
            Some(names[2].as_str())
        );
        assert_eq!(psd.layers[0].name_pascal, "IMG_0003 (-1 EV)");
        assert_eq!(psd.layers[2].name_pascal, "IMG_0001 (0 EV)");
        assert_eq!(psd.layers[0].channels[2].1, vec![200; 48]);

        // An 8-bit document keeps its layers in the layer info section,
        // whose length counts its padding to 4, with a positive count.
        let at = layer_section_offset(&bytes);
        let section = u32_at(&bytes, at);
        let layer_info = u32_at(&bytes, at + 4);
        assert_eq!(
            layer_info,
            unpadded_layer_info_len(&records, LengthWidth::Psd).next_multiple_of(4)
        );
        assert_eq!(
            section,
            4 + layer_info + 4,
            "layer info, then a global mask of 0"
        );
        assert_eq!(u32_at(&bytes, at + 8 + layer_info as usize), 0);
        assert_eq!(bytes[at + 8..at + 10], 3i16.to_be_bytes());
    }

    #[test]
    fn psd_16bit_uses_lr16_and_round_trips() {
        let canvas = canvas(9, 5, true, false, Container::Psd);
        let layers = [
            (
                "top",
                image16(9, 5, 3, |x, y, c| {
                    (x * 7000 + y * 300 + u32::from(c)) as u16 ^ 0x8000
                }),
            ),
            (
                "widened",
                image8(9, 5, 3, |x, y, c| (x * 25 + y + u32::from(c)) as u8),
            ),
            ("flat", image16(9, 5, 3, |_, _, c| 0x1234 + u16::from(c))),
        ];
        let mut alignments = Vec::new();
        for opts in [ZIP, RLE16] {
            for count in 1..=layers.len() {
                let layers = &layers[..count];
                let (bytes, records) = write_with_records(&canvas, opts, layers);
                let psd = parse(&bytes);
                assert_eq!(psd.version, 1);
                assert_round_trip(&psd, &canvas, layers);

                // The section holds an empty layer info, a global mask of
                // 0, then the layer info as `8BIM` `Lr16`. Its length is
                // the data rounded up to even; the padding to 4 follows,
                // uncounted, and ends the section.
                let at = layer_section_offset(&bytes);
                let section = u32_at(&bytes, at);
                assert_eq!(u32_at(&bytes, at + 4), 0, "{opts:?}");
                assert_eq!(u32_at(&bytes, at + 8), 0);
                assert_eq!(&bytes[at + 12..at + 20], b"8BIMLr16");
                let data = unpadded_layer_info_len(&records, LengthWidth::Psd);
                assert_eq!(u32_at(&bytes, at + 20), data.next_multiple_of(2));
                assert_eq!(section, 4 + 4 + 8 + 4 + data.next_multiple_of(4));
                let data_at = at + 24;
                assert_eq!(bytes[data_at..data_at + 2], (count as i16).to_be_bytes());
                let section_end = at + 4 + section as usize;
                assert!(
                    bytes[data_at + data as usize..section_end]
                        .iter()
                        .all(|&b| b == 0)
                );
                // The image data follows the section.
                assert_eq!(bytes.len() - section_end, 2 + 9 * 5 * 3 * 2);
                alignments.push(data % 4);
            }
        }
        // Both an aligned and an unaligned `Lr16` were written.
        assert!(alignments.contains(&0), "{alignments:?}");
        assert!(alignments.iter().any(|&a| a != 0), "{alignments:?}");
    }

    #[test]
    fn composite_equals_top_layer() {
        for sixteen_bit in [false, true] {
            let canvas = canvas(7, 4, sixteen_bit, false, Container::Psd);
            let top = image8(7, 4, 3, |x, y, c| (x * 30 + y * 7 + u32::from(c) * 3) as u8);
            let layers = [("top", top.clone()), ("below", solid8(7, 4, [1, 2, 3]))];
            let bytes = write(&canvas, ZIP, &layers);
            let psd = parse(&bytes);
            let merged: Vec<Vec<u16>> = (0..3).map(|id| plane(&top, id, sixteen_bit)).collect();
            assert_eq!(psd.merged, merged);
            // Raw: compression 0, then exactly the planes, to the end.
            let planes = 7 * 4 * 3 * if sixteen_bit { 2 } else { 1 };
            assert_eq!(bytes[bytes.len() - planes - 2..][..2], [0, 0]);
        }
    }

    #[test]
    fn alpha_canvas_round_trips() {
        for (sixteen_bit, container) in [
            (false, Container::Psd),
            (true, Container::Psd),
            (false, Container::Psb),
            (true, Container::Psb),
        ] {
            let canvas = canvas(5, 3, sixteen_bit, true, container);
            let layers = [
                (
                    "rgba",
                    image8(5, 3, 4, |x, y, c| (x * 50 + y + u32::from(c)) as u8),
                ),
                ("rgb", solid8(5, 3, [9, 8, 7])),
            ];
            let bytes = write(&canvas, ZIP, &layers);
            let psd = parse(&bytes);
            assert_eq!(psd.channels, 4);
            assert_round_trip(&psd, &canvas, &layers);
            // The count is negative: the merged alpha is the transparency.
            // It sits in the layer info, or at the start of `Lr16`'s data.
            assert_eq!(psd.layer_count, -2);
            let at = layer_section_offset(&bytes);
            let count_at = match (container, sixteen_bit) {
                (Container::Psd, true) => at + 24,
                (Container::Psd, false) => at + 8,
                // A PSB's section, layer info and `Lr16` lengths are 8 bytes.
                (_, true) => at + 36,
                (_, false) => at + 16,
            };
            assert_eq!(bytes[count_at..count_at + 2], (-2i16).to_be_bytes());
        }
    }

    #[test]
    fn icc_and_version_info_resources_present() {
        let resolution = [0, 0x48, 0, 0, 0, 1, 0, 1, 0, 0x48, 0, 0, 0, 1, 0, 1];
        let name: Vec<u8> = "Photostacker"
            .encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect();
        let mut version = vec![0, 0, 0, 1, 1, 0, 0, 0, 12];
        version.extend_from_slice(&name);
        version.extend_from_slice(&[0, 0, 0, 12]);
        version.extend_from_slice(&name);
        version.extend_from_slice(&[0, 0, 0, 1]);

        let mut with_icc = canvas(4, 2, false, false, Container::Psd);
        // An odd length, so the resource needs its padding byte.
        let icc = b"not a real profile, 33 bytes long".to_vec();
        assert_eq!(icc.len() % 2, 1);
        with_icc.icc = Some(icc.clone());
        let layers = [("only", solid8(4, 2, [5, 6, 7]))];
        let psd = parse(&write(&with_icc, ZIP, &layers));
        assert_eq!(
            psd.resources,
            vec![
                (1005, resolution.to_vec()),
                (1039, icc),
                (1057, version.clone())
            ]
        );

        let without = canvas(4, 2, false, false, Container::Psd);
        let psd = parse(&write(&without, ZIP, &layers));
        assert_eq!(
            psd.resources,
            vec![(1005, resolution.to_vec()), (1057, version)]
        );
    }

    #[test]
    fn non_ascii_layer_name_round_trips_in_luni() {
        let silesia = format!("Śląsk 01 ({MINUS}2 EV)");
        let emoji = "\u{1F304} sunrise".to_owned();
        let long = "é".repeat(300);
        let canvas = canvas(3, 2, false, false, Container::Psd);
        let layers = [
            (silesia.as_str(), solid8(3, 2, [1, 2, 3])),
            (emoji.as_str(), solid8(3, 2, [4, 5, 6])),
            (long.as_str(), solid8(3, 2, [7, 8, 9])),
        ];
        let psd = parse(&write(&canvas, ZIP, &layers));
        let top = &psd.layers[2];
        assert_eq!(top.name_unicode.as_deref(), Some(silesia.as_str()));
        assert_eq!(top.name_pascal, "?l?sk 01 (-2 EV)");
        // A character outside the BMP is two UTF-16 units, and one `?`.
        assert_eq!(psd.layers[1].name_unicode.as_deref(), Some(emoji.as_str()));
        assert_eq!(psd.layers[1].name_pascal, "? sunrise");
        // The Pascal name stops at 255 bytes; `luni` keeps the whole name.
        assert_eq!(psd.layers[0].name_unicode.as_deref(), Some(long.as_str()));
        assert_eq!(psd.layers[0].name_pascal, "?".repeat(255));

        // The exact bytes: 16 code units fill 4 + 32 bytes, a multiple of 4.
        let mut luni = b"8BIMluni".to_vec();
        luni.extend_from_slice(&36u32.to_be_bytes());
        luni.extend_from_slice(&16u32.to_be_bytes());
        luni.extend(silesia.encode_utf16().flat_map(u16::to_be_bytes));
        assert_eq!(luni_block(&silesia), luni);
        let mut pascal = vec![16];
        pascal.extend_from_slice(b"?l?sk 01 (-2 EV)");
        pascal.extend_from_slice(&[0, 0, 0]);
        assert_eq!(pascal_name(&silesia), pascal);
        // An odd count of units is padded to 4, the padding counted.
        let mut odd = b"8BIMluni".to_vec();
        odd.extend_from_slice(&12u32.to_be_bytes());
        odd.extend_from_slice(&[0, 0, 0, 3, 0, b'a', 0, b'b', 0, b'c', 0, 0]);
        assert_eq!(luni_block("abc"), odd);
        assert_eq!(pascal_name("abc"), [3, b'a', b'b', b'c']);
        assert_eq!(pascal_name(""), [0, 0, 0, 0]);
    }

    #[test]
    fn psb_round_trips_with_8_byte_lengths() {
        for (sixteen_bit, opts) in [(false, ZIP), (true, ZIP), (true, RLE16)] {
            let canvas = canvas(7, 3, sixteen_bit, false, Container::Psb);
            let layers = [
                (
                    "top",
                    image8(7, 3, 3, |x, y, c| (x * 30 + y * 5 + u32::from(c)) as u8),
                ),
                ("bottom", solid8(7, 3, [4, 5, 6])),
                (
                    "deep",
                    image16(7, 3, 3, |x, _, c| x as u16 * 9000 + u16::from(c)),
                ),
            ];
            // A 16-bit layer only fits the 16-bit canvas.
            let layers = if sixteen_bit {
                &layers[..]
            } else {
                &layers[..2]
            };
            let (bytes, records) = write_with_records(&canvas, opts, layers);
            let psd = parse(&bytes);
            assert_eq!(psd.version, 2, "{opts:?}");
            assert_round_trip(&psd, &canvas, layers);

            // The section length is 8 bytes, and so are the layer info and
            // `Lr16` lengths inside it, and the channel lengths that
            // `unpadded_layer_info_len` counts.
            let data = unpadded_layer_info_len(&records, LengthWidth::Psb);
            let at = layer_section_offset(&bytes);
            let section = u64_at(&bytes, at);
            let image_data = at + 8 + section as usize;
            let planes = 7 * 3 * 3 * if sixteen_bit { 2 } else { 1 };
            assert_eq!(bytes.len() - image_data, 2 + planes);
            if sixteen_bit {
                assert_eq!(u64_at(&bytes, at + 8), 0);
                assert_eq!(u32_at(&bytes, at + 16), 0);
                assert_eq!(&bytes[at + 20..at + 28], b"8BIMLr16");
                assert_eq!(u64_at(&bytes, at + 28), data.next_multiple_of(2));
                assert_eq!(section, 8 + 4 + 8 + 8 + data.next_multiple_of(4));
            } else {
                assert_eq!(u64_at(&bytes, at + 8), data.next_multiple_of(4));
                assert_eq!(section, 8 + data.next_multiple_of(4) + 4);
            }
        }
    }

    #[test]
    fn psd_crate_reads_our_8bit_psd() {
        let canvas = canvas(8, 6, false, false, Container::Psd);
        let top = image8(8, 6, 3, |x, y, c| {
            (x * 20 + y * 9 + u32::from(c) * 70) as u8
        });
        let layers = [
            ("IMG_0001 (0 EV)", top.clone()),
            ("IMG_0002 (+1 EV)", solid8(8, 6, [10, 200, 10])),
            ("IMG_0003 (-1 EV)", solid8(8, 6, [10, 10, 200])),
        ];
        let bytes = write(&canvas, ZIP, &layers);
        let psd = psd::Psd::from_bytes(&bytes).expect("the psd crate parses our PSD");
        assert_eq!((psd.width(), psd.height()), (8, 6));
        // The crate lists the layers top first.
        let names: Vec<&str> = psd.layers().iter().map(|l| l.name()).collect();
        assert_eq!(
            names,
            ["IMG_0001 (0 EV)", "IMG_0002 (+1 EV)", "IMG_0003 (-1 EV)"]
        );
        let Samples::U8(samples) = &top.samples else {
            unreachable!("an 8-bit layer")
        };
        let rgba: Vec<u8> = samples
            .chunks(3)
            .flat_map(|px| [px[0], px[1], px[2], 255])
            .collect();
        assert_eq!(psd.rgba(), rgba, "the composite");
        assert_eq!(psd.layers()[0].rgba(), rgba, "the top layer's pixels");
    }

    #[test]
    fn psd_up_to_i32_max_bytes_is_accepted_past_it_refused() {
        let limit = i32::MAX as u64;
        assert!(check_file_len(LengthWidth::Psd, limit).is_ok());
        let error = check_file_len(LengthWidth::Psd, limit + 1).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(check_file_len(LengthWidth::Psb, u64::from(u32::MAX) * 4).is_ok());

        // `file_len` counts every byte `write_psd` writes.
        for (sixteen_bit, container) in [
            (false, Container::Psd),
            (true, Container::Psd),
            (false, Container::Psb),
            (true, Container::Psb),
        ] {
            let canvas = canvas(6, 4, sixteen_bit, false, container);
            let widths = match container {
                Container::Psb => LengthWidth::Psb,
                _ => LengthWidth::Psd,
            };
            let bytes = write(&canvas, ZIP, &[("only", solid8(6, 4, [1, 2, 3]))]);
            let resources = u32_at(&bytes, 30);
            let at = layer_section_offset(&bytes);
            let section = match widths {
                LengthWidth::Psd => u32_at(&bytes, at),
                LengthWidth::Psb => u64_at(&bytes, at),
            };
            let merged = 6 * 4 * 3 * if sixteen_bit { 2 } else { 1 };
            assert_eq!(
                file_len(widths, resources, section, merged),
                bytes.len() as u64,
                "{container:?}, 16-bit {sixteen_bit}"
            );
        }
    }

    #[test]
    fn a_long_length_past_u32_is_refused_in_a_psd_only() {
        let max = u64::from(u32::MAX);
        assert_eq!(long(LengthWidth::Psd, max).unwrap(), u32::MAX.to_be_bytes());
        let error = long(LengthWidth::Psd, max + 1).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{error}");
        assert_eq!(
            long(LengthWidth::Psb, max + 1).unwrap(),
            (max + 1).to_be_bytes()
        );
    }

    #[test]
    fn a_channel_past_u32_is_refused_in_a_psd_only() {
        let canvas = canvas(4, 2, false, false, Container::Psd);
        // Records only: the check reads no spill. The green channel is one
        // byte past what a PSD channel length holds.
        let lens = [10, u64::from(u32::MAX) + 1, 10];
        let mut offset = 0;
        let channels = (0..3)
            .zip(lens)
            .map(|(id, len)| {
                let channel = ChannelRecord {
                    id,
                    compression: 1,
                    offset,
                    len,
                };
                offset += len;
                channel
            })
            .collect();
        let records = [LayerRecord {
            name: "huge".into(),
            channels,
        }];
        let error = LayerInfo::new(&canvas, &records, LengthWidth::Psd)
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{error}");
        assert!(error.to_string().contains("does not fit a PSD"), "{error}");
        let info = LayerInfo::new(&canvas, &records, LengthWidth::Psb).unwrap();
        assert_eq!(info.channel_data(), offset);
    }

    #[test]
    fn refuses_what_does_not_fit_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let fits = canvas(4, 2, false, false, Container::Psd);
        let mut encoder =
            LayerEncoder::new(&dir.path().join("a.psd"), &fits, LengthWidth::Psd, ZIP).unwrap();
        encoder.add_layer("a", solid8(4, 2, [1, 2, 3])).unwrap();
        let (records, spill) = encoder.finish().unwrap();
        let (no_records, no_spill) =
            LayerEncoder::new(&dir.path().join("b.psd"), &fits, LengthWidth::Psd, ZIP)
                .unwrap()
                .finish()
                .unwrap();

        let mut tiff = fits.clone();
        tiff.container = Container::Tiff;
        let mut too_wide = fits.clone();
        too_wide.width = 30_001;
        let mut with_alpha = fits.clone();
        with_alpha.has_alpha = true;
        let mut other_size = fits.clone();
        other_size.height = 3;
        // Encoded with PSD widths, written as a PSB.
        let mut psb = fits.clone();
        psb.container = Container::Psb;
        // Encoded for a TIFF with alpha, whose merged spill is straight.
        let mut tiff_alpha = with_alpha.clone();
        tiff_alpha.container = Container::Tiff;
        let mut encoder = LayerEncoder::new(
            &dir.path().join("c.tif"),
            &tiff_alpha,
            LengthWidth::Psd,
            ZIP,
        )
        .unwrap();
        encoder
            .add_layer("c", image8(4, 2, 4, |_, _, c| 60 * c))
            .unwrap();
        let (straight_records, straight_spill) = encoder.finish().unwrap();
        let cases: [(&CanvasProfile, &[LayerRecord], &Spill); 7] = [
            (&tiff, &records, &spill),
            (&psb, &records, &spill),
            (&too_wide, &records, &spill),
            (&with_alpha, &records, &spill),
            (&other_size, &records, &spill),
            (&fits, &no_records, &no_spill),
            (&with_alpha, &straight_records, &straight_spill),
        ];
        for (i, (canvas, records, spill)) in cases.into_iter().enumerate() {
            let path = dir.path().join(format!("out{i}.psd.partial"));
            let mut out = File::create(&path).unwrap();
            let error = write_psd(&mut out, canvas, records, spill).unwrap_err();
            assert_eq!(
                error.kind(),
                io::ErrorKind::InvalidInput,
                "case {i}: {error}"
            );
            assert_eq!(std::fs::metadata(&path).unwrap().len(), 0, "case {i}");
        }
    }
}
