//! An independent PSD/PSB reader for the writer's round-trip tests.
//!
//! It parses bytes only, after the Adobe Photoshop File Formats
//! Specification, and shares no code with `photostacker-core`. It is strict
//! where a lenient reader would hide a writer bug: every section must be
//! consumed exactly, padding must be zeros, and every channel must decode to
//! exactly its rectangle.
//!
//! Supported: RGB at 8 or 16 bits; channel compression 0 (raw), 1 (PackBits),
//! 2 (ZIP) and 3 (ZIP with prediction); layers in the layer-info section or
//! in an `Lr16` tagged block when the layer-info length is 0; PSB's wider
//! lengths and row counts.
//!
//! [`read_layer_block`] reads the layers of a layered TIFF: the value of its
//! ImageSourceData tag (37724), with the same layer-info parser.

use std::io::Read;

/// A parsed document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Psd {
    /// 1 for PSD, 2 for PSB.
    pub version: u16,
    pub width: u32,
    pub height: u32,
    /// Bits per sample: 8 or 16.
    pub depth: u16,
    /// The channel count of the header, which the merged image has.
    pub channels: u16,
    /// Image resources as `(id, data)`, in file order, padding removed.
    pub resources: Vec<(u16, Vec<u8>)>,
    /// The layer count as stored, sign included: negative means the merged
    /// image's first alpha channel is its transparency. 0 without layers.
    pub layer_count: i16,
    /// The layers in file order: the bottom layer first.
    pub layers: Vec<Layer>,
    /// The merged image, one plane per channel; 8-bit samples as `v as u16`.
    pub merged: Vec<Vec<u16>>,
}

/// One layer record with its decoded channels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layer {
    /// The Pascal name, each byte as one char (the writer keeps it ASCII).
    pub name_pascal: String,
    /// The `luni` name, when the record has one.
    pub name_unicode: Option<String>,
    /// Top, left, bottom, right.
    pub rect: (i32, i32, i32, i32),
    /// `(id, samples)` in record order; 8-bit samples as `v as u16`.
    pub channels: Vec<(i16, Vec<u16>)>,
    /// The blend mode key, such as `*b"norm"`.
    pub blend: [u8; 4],
    pub opacity: u8,
    /// 0 for base clipping, 1 for non-base.
    pub clipping: u8,
    /// The flag bits: bit 0 transparency protected, bit 1 hidden, and so on.
    pub flags: u8,
}

/// Tagged-block keys whose length is 8 bytes in a PSB (the specification's
/// list under "Additional Layer Information").
const PSB_LONG_KEYS: [&[u8; 4]; 13] = [
    b"LMsk", b"Lr16", b"Lr32", b"Layr", b"Mt16", b"Mt32", b"Mtrn", b"Alph", b"FMsk", b"lnk2",
    b"FEid", b"FXid", b"PxSD",
];

/// The signature of an ImageSourceData value whose blocks have PSD widths.
const DOCUMENT_DATA_BLOCK: &[u8; 36] = b"Adobe Photoshop Document Data Block\0";
/// The signature of one whose blocks have PSB widths.
const DOCUMENT_DATA_V0002: &[u8; 36] = b"Adobe Photoshop Document Data V0002\0";

/// Parses the value of a TIFF's ImageSourceData tag (37724): a signature,
/// then big-endian `8BIM` blocks, each padded to 4 with zeros: exactly one
/// `Layr` (8-bit) or `Lr16` (16-bit) layer info, and an optional `LMsk`
/// after it. The signature `...Block` gives PSD widths, `...V0002` PSB
/// widths. Returns the layers in file order: the bottom layer first.
pub fn read_layer_block(bytes: &[u8]) -> Result<Vec<Layer>, String> {
    let mut r = Reader::new(bytes, 1);
    r.version = match r.take(36)? {
        signature if signature == DOCUMENT_DATA_BLOCK => 1,
        signature if signature == DOCUMENT_DATA_V0002 => 2,
        signature => {
            return Err(format!(
                "not Photoshop document data: {:?}",
                String::from_utf8_lossy(signature)
            ));
        }
    };
    let mut layers = None;
    let mut mask = false;
    while r.remaining() > 0 {
        let at = r.pos;
        if r.take(4)? != b"8BIM" {
            return Err(format!("a block at offset {at} has no 8BIM"));
        }
        let key: [u8; 4] = r.array()?;
        let len = tagged_len(&mut r, &key)?;
        let mut block = r.sub(len)?;
        r.zeros((4 - len % 4) % 4, "block padding")?;
        let depth = match &key {
            b"Layr" => 8,
            b"Lr16" => 16,
            b"LMsk" if layers.is_some() && !mask => {
                mask = true;
                continue;
            }
            _ => {
                return Err(format!(
                    "unexpected block {} at offset {at}",
                    String::from_utf8_lossy(&key)
                ));
            }
        };
        if layers.is_some() {
            return Err("a second layer block".into());
        }
        // Only the depth matters to the layer info; each record carries
        // its own rectangle.
        let shape = Shape {
            width: 0,
            height: 0,
            depth,
        };
        let (_, block_layers) = read_layer_info(&mut block, shape)?;
        layers = Some(block_layers);
    }
    layers.ok_or_else(|| "no Layr or Lr16 block".into())
}

/// Parses a whole PSD or PSB file.
pub fn read(bytes: &[u8]) -> Result<Psd, String> {
    let mut r = Reader::new(bytes, 1);
    if r.take(4)? != b"8BPS" {
        return Err("not a PSD: the signature is not 8BPS".into());
    }
    let version = r.u16()?;
    if version != 1 && version != 2 {
        return Err(format!("unknown version {version}"));
    }
    r.version = version;
    r.zeros(6, "header reserved bytes")?;
    let channels = r.u16()?;
    let height = r.u32()?;
    let width = r.u32()?;
    let depth = r.u16()?;
    let mode = r.u16()?;
    if mode != 3 {
        return Err(format!("colour mode {mode}, not RGB (3)"));
    }
    if depth != 8 && depth != 16 {
        return Err(format!("depth {depth}, not 8 or 16"));
    }
    let shape = Shape {
        width,
        height,
        depth,
    };

    let colour_mode = r.u32()? as usize;
    r.take(colour_mode)?;

    let resources_len = r.u32()? as usize;
    let resources = read_resources(&mut r.sub(resources_len)?)?;

    let section_len = r.long_len()?;
    let (layer_count, layers) = read_layer_and_mask_info(&mut r.sub(section_len)?, shape)?;
    if layer_count < 0 && channels != 4 {
        return Err(format!(
            "a negative layer count needs a merged alpha channel, but there are {channels} channels"
        ));
    }

    let merged = read_merged(&mut r, shape, channels)?;
    Ok(Psd {
        version,
        width,
        height,
        depth,
        channels,
        resources,
        layer_count,
        layers,
        merged,
    })
}

/// What decoding a channel needs of the document.
#[derive(Debug, Clone, Copy)]
struct Shape {
    width: u32,
    height: u32,
    depth: u16,
}

/// A cursor over a byte slice. `version` decides PSB's wider fields.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    version: u16,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8], version: u16) -> Self {
        Self {
            bytes,
            pos: 0,
            version,
        }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if n > self.remaining() {
            return Err(format!(
                "truncated: {n} bytes wanted at offset {}, {} left",
                self.pos,
                self.remaining()
            ));
        }
        let slice = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    /// The next `n` bytes as a reader of their own, of the same version.
    fn sub(&mut self, n: usize) -> Result<Reader<'a>, String> {
        let version = self.version;
        Ok(Reader::new(self.take(n)?, version))
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        Ok(self.take(N)?.try_into().expect("take returns N bytes"))
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn i16(&mut self) -> Result<i16, String> {
        Ok(i16::from_be_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    fn i32(&mut self) -> Result<i32, String> {
        Ok(i32::from_be_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    /// A length that is 4 bytes in a PSD and 8 in a PSB.
    fn long_len(&mut self) -> Result<usize, String> {
        let len = if self.version == 2 {
            self.u64()?
        } else {
            u64::from(self.u32()?)
        };
        usize::try_from(len).map_err(|_| format!("length {len} does not fit memory"))
    }

    /// Skips `n` bytes that must all be zero.
    fn zeros(&mut self, n: usize, what: &str) -> Result<(), String> {
        let at = self.pos;
        if self.take(n)?.iter().any(|&b| b != 0) {
            return Err(format!("{what} at offset {at} are not zero"));
        }
        Ok(())
    }

    /// Fails unless every byte was read.
    fn end(&self, what: &str) -> Result<(), String> {
        if self.remaining() != 0 {
            return Err(format!("{} bytes left over in {what}", self.remaining()));
        }
        Ok(())
    }
}

/// The image resource blocks: `8BIM`, id, Pascal name padded to even, a
/// 4-byte size, then the data padded to even.
fn read_resources(r: &mut Reader) -> Result<Vec<(u16, Vec<u8>)>, String> {
    let mut resources = Vec::new();
    while r.remaining() > 0 {
        if r.take(4)? != b"8BIM" {
            return Err(format!("a resource at offset {} is not 8BIM", r.pos - 4));
        }
        let id = r.u16()?;
        let name_len = usize::from(r.u8()?);
        r.take(name_len)?;
        if (1 + name_len) % 2 == 1 {
            r.zeros(1, "resource name padding")?;
        }
        let size = r.u32()? as usize;
        let data = r.take(size)?.to_vec();
        if size % 2 == 1 {
            r.zeros(1, "resource data padding")?;
        }
        resources.push((id, data));
    }
    Ok(resources)
}

/// The layer and mask information section, without its length: the layer
/// info, the global layer mask info, then tagged blocks, one of which may be
/// `Lr16` when the layer info is empty. Returns the signed layer count
/// (0 without layers) and the layers.
fn read_layer_and_mask_info(r: &mut Reader, shape: Shape) -> Result<(i16, Vec<Layer>), String> {
    if r.remaining() == 0 {
        return Ok((0, Vec::new()));
    }
    let layer_info_len = r.long_len()?;
    let (mut count, mut layers) = if layer_info_len > 0 {
        read_layer_info(&mut r.sub(layer_info_len)?, shape)?
    } else {
        (0, Vec::new())
    };

    let global_mask_len = r.u32()? as usize;
    r.take(global_mask_len)?;

    // Tagged blocks run to the end of the section; fewer than a block
    // header's bytes can only be padding. Photoshop pads each of these
    // blocks to 4 without counting the padding in its length.
    while r.remaining() >= 12 {
        let signature = r.take(4)?;
        if signature != b"8BIM" && signature != b"8B64" {
            return Err(format!(
                "a tagged block at offset {} has no 8BIM",
                r.pos - 4
            ));
        }
        let key: [u8; 4] = r.array()?;
        let len = tagged_len(r, &key)?;
        let mut block = r.sub(len)?;
        let padding = (4 - len % 4) % 4;
        r.zeros(padding, "tagged block padding")?;
        if &key == b"Lr16" {
            if !layers.is_empty() {
                return Err("Lr16 next to a non-empty layer info".into());
            }
            if shape.depth != 16 {
                return Err(format!("Lr16 in a {}-bit document", shape.depth));
            }
            (count, layers) = read_layer_info(&mut block, shape)?;
        }
    }
    let rest = r.remaining();
    r.zeros(rest, "layer and mask info padding")?;
    Ok((count, layers))
}

/// The length of a tagged block with `key`: 8 bytes in a PSB for the keys
/// of [`PSB_LONG_KEYS`], else 4. It must be even.
fn tagged_len(r: &mut Reader, key: &[u8; 4]) -> Result<usize, String> {
    let len = if r.version == 2 && PSB_LONG_KEYS.contains(&key) {
        r.long_len()?
    } else {
        r.u32()? as usize
    };
    if len % 2 == 1 {
        return Err(format!(
            "tagged block {} has odd length {len}",
            String::from_utf8_lossy(key)
        ));
    }
    Ok(len)
}

/// The layer info without its length: the count, the records, then each
/// layer's channel data in record order, then at most 3 zero bytes of
/// padding (to 4 in the layer info section, to 2 in `Lr16`). Returns the
/// signed count, which may be negative, and the layers.
fn read_layer_info(r: &mut Reader, shape: Shape) -> Result<(i16, Vec<Layer>), String> {
    let signed_count = r.i16()?;
    let count = signed_count.unsigned_abs();
    let mut records = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        records.push(read_layer_record(r)?);
    }
    let mut layers = Vec::with_capacity(records.len());
    for record in records {
        let (top, left, bottom, right) = record.rect;
        let rows = usize::try_from(bottom - top).map_err(|_| "negative layer height")?;
        let columns = usize::try_from(right - left).map_err(|_| "negative layer width")?;
        let mut channels = Vec::with_capacity(record.channel_lens.len());
        for &(id, len) in &record.channel_lens {
            let mut data = r.sub(len)?;
            let samples = read_channel(&mut data, shape.depth, rows, columns)?;
            data.end("a layer channel")?;
            channels.push((id, samples));
        }
        layers.push(Layer {
            name_pascal: record.name_pascal,
            name_unicode: record.name_unicode,
            rect: record.rect,
            channels,
            blend: record.blend,
            opacity: record.opacity,
            clipping: record.clipping,
            flags: record.flags,
        });
    }
    let rest = r.remaining();
    if rest > 3 {
        return Err(format!("{rest} bytes left over after the channel data"));
    }
    r.zeros(rest, "layer info padding")?;
    Ok((signed_count, layers))
}

/// A layer record before its channel data is read.
struct Record {
    rect: (i32, i32, i32, i32),
    channel_lens: Vec<(i16, usize)>,
    blend: [u8; 4],
    opacity: u8,
    clipping: u8,
    flags: u8,
    name_pascal: String,
    name_unicode: Option<String>,
}

fn read_layer_record(r: &mut Reader) -> Result<Record, String> {
    let rect = (r.i32()?, r.i32()?, r.i32()?, r.i32()?);
    let channel_count = r.u16()?;
    let mut channel_lens = Vec::with_capacity(usize::from(channel_count));
    for _ in 0..channel_count {
        let id = r.i16()?;
        let len = r.long_len()?;
        channel_lens.push((id, len));
    }
    if r.take(4)? != b"8BIM" {
        return Err("a layer record's blend signature is not 8BIM".into());
    }
    let blend: [u8; 4] = r.array()?;
    let opacity = r.u8()?;
    let clipping = r.u8()?;
    let flags = r.u8()?;
    r.zeros(1, "layer record filler")?;

    let extra_len = r.u32()? as usize;
    let mut extra = r.sub(extra_len)?;
    let mask_len = extra.u32()? as usize;
    extra.take(mask_len)?;
    let blending_len = extra.u32()? as usize;
    extra.take(blending_len)?;

    let name_len = usize::from(extra.u8()?);
    let name_pascal = extra
        .take(name_len)?
        .iter()
        .map(|&b| char::from(b))
        .collect();
    let padding = (4 - (1 + name_len) % 4) % 4;
    extra.zeros(padding, "Pascal name padding")?;

    let mut name_unicode = None;
    while extra.remaining() > 0 {
        let signature = extra.take(4)?;
        if signature != b"8BIM" && signature != b"8B64" {
            return Err("a layer's tagged block has no 8BIM".into());
        }
        let key: [u8; 4] = extra.array()?;
        let len = tagged_len(&mut extra, &key)?;
        let mut block = extra.sub(len)?;
        if &key == b"luni" {
            name_unicode = Some(read_unicode(&mut block)?);
            let rest = block.remaining();
            block.zeros(rest, "luni padding")?;
        }
    }
    Ok(Record {
        rect,
        channel_lens,
        blend,
        opacity,
        clipping,
        flags,
        name_pascal,
        name_unicode,
    })
}

/// A Unicode string: a 4-byte count of UTF-16 code units, then the units.
fn read_unicode(r: &mut Reader) -> Result<String, String> {
    let count = r.u32()? as usize;
    let units: Vec<u16> = r
        .take(count * 2)?
        .chunks(2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
        .collect();
    String::from_utf16(&units).map_err(|e| format!("bad UTF-16 in a Unicode string: {e}"))
}

/// One layer channel: the compression code, then the data of `rows` rows of
/// `columns` samples.
fn read_channel(
    r: &mut Reader,
    depth: u16,
    rows: usize,
    columns: usize,
) -> Result<Vec<u16>, String> {
    let compression = r.u16()?;
    let row_bytes = columns * usize::from(depth / 8);
    let bytes = match compression {
        0 => r.take(rows * row_bytes)?.to_vec(),
        1 => {
            let counts = row_counts(r, rows)?;
            unpack_rows(r, &counts, row_bytes)?
        }
        2 | 3 => {
            let rest = r.remaining();
            let mut bytes = Vec::new();
            flate2::read::ZlibDecoder::new(r.take(rest)?)
                .read_to_end(&mut bytes)
                .map_err(|e| format!("bad zlib stream: {e}"))?;
            if bytes.len() != rows * row_bytes {
                return Err(format!(
                    "zlib channel inflates to {} bytes, not {}",
                    bytes.len(),
                    rows * row_bytes
                ));
            }
            if compression == 3 {
                undo_prediction(&mut bytes, row_bytes, depth);
            }
            bytes
        }
        other => return Err(format!("unknown channel compression {other}")),
    };
    Ok(samples(&bytes, depth))
}

/// The PackBits byte counts of `rows` rows: 2 bytes each, 4 in a PSB.
fn row_counts(r: &mut Reader, rows: usize) -> Result<Vec<usize>, String> {
    (0..rows)
        .map(|_| {
            Ok(if r.version == 2 {
                r.u32()? as usize
            } else {
                usize::from(r.u16()?)
            })
        })
        .collect()
}

/// Unpacks one PackBits row per count; each must give `row_bytes` bytes.
fn unpack_rows(r: &mut Reader, counts: &[usize], row_bytes: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(counts.len() * row_bytes);
    for &count in counts {
        let start = out.len();
        let mut packed = r.sub(count)?;
        while packed.remaining() > 0 {
            let header = packed.u8()? as i8;
            if header >= 0 {
                out.extend_from_slice(packed.take(header as usize + 1)?);
            } else if header != -128 {
                let value = packed.u8()?;
                out.extend(std::iter::repeat_n(
                    value,
                    (1 - isize::from(header)) as usize,
                ));
            }
        }
        if out.len() - start != row_bytes {
            return Err(format!(
                "a PackBits row unpacks to {} bytes, not {row_bytes}",
                out.len() - start
            ));
        }
    }
    Ok(out)
}

/// Undoes ZIP prediction in place: each row's samples are deltas from the
/// previous sample, the first from 0; 16-bit samples are big-endian.
fn undo_prediction(bytes: &mut [u8], row_bytes: usize, depth: u16) {
    for row in bytes.chunks_mut(row_bytes) {
        if depth == 16 {
            let mut previous = 0u16;
            for sample in row.chunks_mut(2) {
                let value = u16::from_be_bytes([sample[0], sample[1]]).wrapping_add(previous);
                sample.copy_from_slice(&value.to_be_bytes());
                previous = value;
            }
        } else {
            let mut previous = 0u8;
            for sample in row.iter_mut() {
                *sample = sample.wrapping_add(previous);
                previous = *sample;
            }
        }
    }
}

/// Big-endian samples at `depth` as u16 values.
fn samples(bytes: &[u8], depth: u16) -> Vec<u16> {
    if depth == 16 {
        bytes
            .chunks(2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
            .collect()
    } else {
        bytes.iter().map(|&b| u16::from(b)).collect()
    }
}

/// The merged image data: the compression code, then every channel's plane
/// (raw), or every row's count and then every packed row (PackBits). It must
/// run exactly to the end of the file.
fn read_merged(r: &mut Reader, shape: Shape, channels: u16) -> Result<Vec<Vec<u16>>, String> {
    let compression = r.u16()?;
    let rows = shape.height as usize;
    let row_bytes = shape.width as usize * usize::from(shape.depth / 8);
    let planes = usize::from(channels);
    let bytes = match compression {
        0 => r.take(planes * rows * row_bytes)?.to_vec(),
        1 => {
            let counts = row_counts(r, planes * rows)?;
            unpack_rows(r, &counts, row_bytes)?
        }
        other => return Err(format!("merged image compression {other}, not 0 or 1")),
    };
    r.end("the file after the merged image")?;
    let plane_bytes = rows * row_bytes;
    Ok((0..planes)
        .map(|c| samples(&bytes[c * plane_bytes..(c + 1) * plane_bytes], shape.depth))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal 2×1 8-bit PSD without layers, built by hand.
    fn tiny_psd() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"8BPS");
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&[0; 6]);
        b.extend_from_slice(&3u16.to_be_bytes()); // channels
        b.extend_from_slice(&1u32.to_be_bytes()); // height
        b.extend_from_slice(&2u32.to_be_bytes()); // width
        b.extend_from_slice(&8u16.to_be_bytes());
        b.extend_from_slice(&3u16.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes()); // colour mode data
        // One resource, id 1005, an odd 3-byte payload padded to 4.
        b.extend_from_slice(&16u32.to_be_bytes());
        b.extend_from_slice(b"8BIM");
        b.extend_from_slice(&1005u16.to_be_bytes());
        b.extend_from_slice(&[0, 0]);
        b.extend_from_slice(&3u32.to_be_bytes());
        b.extend_from_slice(&[7, 8, 9, 0]);
        b.extend_from_slice(&0u32.to_be_bytes()); // no layers
        b.extend_from_slice(&0u16.to_be_bytes()); // raw merged
        b.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
        b
    }

    /// A 2×1 8-bit PSD with `channels` channels and one empty layer record
    /// (no channels, a 0×0 rectangle, an empty name), stored with the
    /// signed `layer_count`.
    fn psd_with_layer_count(channels: u16, layer_count: i16) -> Vec<u8> {
        let mut record = Vec::new();
        record.extend_from_slice(&[0; 16]); // rectangle
        record.extend_from_slice(&0u16.to_be_bytes()); // no channels
        record.extend_from_slice(b"8BIMnorm");
        record.extend_from_slice(&[255, 0, 0, 0]);
        record.extend_from_slice(&12u32.to_be_bytes()); // extra data
        record.extend_from_slice(&[0; 8]); // no mask, no blending ranges
        record.extend_from_slice(&[0, 0, 0, 0]); // empty name, padded to 4
        let mut layer_info = layer_count.to_be_bytes().to_vec();
        layer_info.extend_from_slice(&record);
        layer_info.resize(layer_info.len().next_multiple_of(4), 0);

        let mut b = Vec::new();
        b.extend_from_slice(b"8BPS");
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&[0; 6]);
        b.extend_from_slice(&channels.to_be_bytes());
        b.extend_from_slice(&1u32.to_be_bytes()); // height
        b.extend_from_slice(&2u32.to_be_bytes()); // width
        b.extend_from_slice(&8u16.to_be_bytes());
        b.extend_from_slice(&3u16.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes()); // colour mode data
        b.extend_from_slice(&0u32.to_be_bytes()); // no resources
        b.extend_from_slice(&(4 + layer_info.len() as u32 + 4).to_be_bytes());
        b.extend_from_slice(&(layer_info.len() as u32).to_be_bytes());
        b.extend_from_slice(&layer_info);
        b.extend_from_slice(&0u32.to_be_bytes()); // global layer mask info
        b.extend_from_slice(&0u16.to_be_bytes()); // raw merged
        b.extend(std::iter::repeat_n(9, 2 * usize::from(channels)));
        b
    }

    #[test]
    fn layer_count_keeps_its_sign_and_a_negative_one_needs_alpha() {
        for (channels, count) in [(3, 1), (4, 1), (4, -1)] {
            let psd = read(&psd_with_layer_count(channels, count)).unwrap();
            assert_eq!(psd.layer_count, count);
            assert_eq!(psd.layers.len(), 1);
            assert_eq!(psd.merged.len(), usize::from(channels));
        }
        let error = read(&psd_with_layer_count(3, -1)).unwrap_err();
        assert!(error.contains("negative layer count"), "{error}");
        assert_eq!(read(&tiny_psd()).unwrap().layer_count, 0);
    }

    #[test]
    fn reads_a_hand_built_psd() {
        let psd = read(&tiny_psd()).unwrap();
        assert_eq!((psd.version, psd.width, psd.height), (1, 2, 1));
        assert_eq!((psd.depth, psd.channels), (8, 3));
        assert_eq!(psd.resources, vec![(1005, vec![7, 8, 9])]);
        assert!(psd.layers.is_empty());
        assert_eq!(psd.merged, vec![vec![1, 2], vec![3, 4], vec![5, 6]]);
    }

    #[test]
    fn refuses_bad_signature_truncation_and_trailing_bytes() {
        let good = tiny_psd();
        let mut bad = good.clone();
        bad[0] = b'X';
        assert!(read(&bad).is_err());
        assert!(read(&good[..good.len() - 1]).is_err());
        let mut long = good.clone();
        long.push(0);
        assert!(read(&long).is_err());
    }

    /// An ImageSourceData value with PSD widths: `Layr` holding one 1×1
    /// layer with an empty name and one raw channel, then `LMsk` of
    /// length 0.
    fn tiny_layer_block() -> Vec<u8> {
        let mut info = 1i16.to_be_bytes().to_vec();
        for edge in [0i32, 0, 1, 1] {
            info.extend_from_slice(&edge.to_be_bytes());
        }
        info.extend_from_slice(&1u16.to_be_bytes()); // one channel,
        info.extend_from_slice(&0i16.to_be_bytes()); // red,
        info.extend_from_slice(&3u32.to_be_bytes()); // of 3 bytes
        info.extend_from_slice(b"8BIMnorm");
        info.extend_from_slice(&[255, 0, 0, 0]);
        info.extend_from_slice(&12u32.to_be_bytes());
        info.extend_from_slice(&[0; 12]); // no mask or ranges, empty name
        info.extend_from_slice(&[0, 0, 7]); // raw, one sample
        // 57 bytes: the length is 58 with one padding byte, and two more
        // pad the block to 4, uncounted.
        assert_eq!(info.len(), 57);
        let mut b = DOCUMENT_DATA_BLOCK.to_vec();
        b.extend_from_slice(b"8BIMLayr");
        b.extend_from_slice(&58u32.to_be_bytes());
        b.extend_from_slice(&info);
        b.extend_from_slice(&[0; 3]);
        b.extend_from_slice(b"8BIMLMsk");
        b.extend_from_slice(&0u32.to_be_bytes());
        b
    }

    #[test]
    fn reads_a_hand_built_layer_block() {
        let layers = read_layer_block(&tiny_layer_block()).unwrap();
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].name_pascal, "");
        assert_eq!(layers[0].rect, (0, 0, 1, 1));
        assert_eq!(layers[0].channels, vec![(0, vec![7])]);
        assert_eq!(&layers[0].blend, b"norm");
        assert_eq!((layers[0].clipping, layers[0].flags), (0, 0));
        // The clipping and flags bytes are kept as stored.
        let mut flagged = tiny_layer_block();
        let opacity_at = 36 + 12 + 2 + 16 + 2 + 6 + 8;
        assert_eq!(flagged[opacity_at], 255);
        flagged[opacity_at + 1] = 1;
        flagged[opacity_at + 2] = 2;
        let layers = read_layer_block(&flagged).unwrap();
        assert_eq!((layers[0].clipping, layers[0].flags), (1, 2));

        let mut v0002 = tiny_layer_block();
        v0002[31..36].copy_from_slice(b"V0002");
        // The same lengths read as PSB's 8 bytes no longer line up.
        assert!(read_layer_block(&v0002).is_err());

        let mut bad = tiny_layer_block();
        bad[0] = b'a';
        assert!(read_layer_block(&bad).is_err());
        let mut long = tiny_layer_block();
        long.push(0);
        assert!(read_layer_block(&long).is_err());
        let good = tiny_layer_block();
        let no_layers = &good[..36];
        assert!(read_layer_block(no_layers).is_err());
    }

    #[test]
    fn packbits_rows_must_unpack_to_their_width() {
        // Header -1 (a run of 2) of 0xAB, then a literal of 1.
        let packed = [0xFF, 0xAB, 0x00, 0x01];
        let mut r = Reader::new(&packed, 1);
        assert_eq!(unpack_rows(&mut r, &[4], 3).unwrap(), [0xAB, 0xAB, 0x01]);
        let mut r = Reader::new(&packed, 1);
        assert!(unpack_rows(&mut r, &[4], 4).is_err());
    }
}
