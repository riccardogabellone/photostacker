//! The layered TIFF container (spec §6.3).
//!
//! `write_tiff` writes a TIFF 6.0 or BigTIFF file whose image is the
//! composite, the top layer, and whose ImageSourceData tag (37724, Adobe's
//! "Photoshop TIFF Technical Notes") carries the layers, so that Photoshop
//! reopens the file with them. It assembles from the same spill files as the
//! PSD writer, and serialises the layers with the same code. The file is
//! big-endian (`MM`) and is, in order:
//!
//! 1. **Header**: `MM`, 42 and the IFD's 4-byte offset; in a BigTIFF `MM`,
//!    43, the offset size 8, a zero short and the IFD's 8-byte offset. The
//!    offset is written last, once the strips' sizes are known.
//! 2. **Strips** of the composite, read from `.merged` a strip at a time:
//!    the strip's rows of each plane are read at once, interleaved into
//!    chunky samples (16-bit samples big-endian) and differenced
//!    horizontally per component (Predictor 2), and each strip is one zlib
//!    stream (Compression 8, Adobe Deflate) at level 1. A strip
//!    holds as many rows as fit in 1 MiB, at least one and at most the
//!    image. A strip that deflate would expand is stored instead, so no
//!    strip exceeds its stored size. The encoder keeps a TIFF canvas's
//!    `.merged` in straight colour, as ExtraSamples 2 (unassociated alpha)
//!    declares it, where a PSD's with alpha is blended against white
//!    (spec §6.3); `write_tiff` refuses a spill blended for a PSD.
//! 3. **ImageSourceData**: a 36-byte signature, then big-endian Photoshop
//!    blocks, each `8BIM`, a key, a length and the data padded to 4:
//!    - `Layr` (8-bit) or `Lr16` (16-bit): the layer info exactly as the PSD
//!      writer serialises it, with its widths. The length is the data
//!      rounded up to even; the padding to 4 that follows is not counted.
//!    - `LMsk`: the global layer mask info, empty (length 0).
//!
//!    A classic TIFF uses PSD widths and the signature
//!    `Adobe Photoshop Document Data Block\0`. A BigTIFF uses PSB widths, in
//!    which `Layr`, `Lr16` and `LMsk` have 8-byte lengths, and the
//!    signature `Adobe Photoshop Document Data V0002\0`: Photoshop marks PSB
//!    widths with that signature, which the 2002 technical notes predate
//!    (ExifTool's `ProcessDocumentData` reads PSB widths exactly when it
//!    finds it).
//! 4. **Values** that do not fit in their IFD entry, each at an even offset.
//! 5. **The IFD**, at an even offset, its tags ascending: the image's
//!    dimensions, bits per sample, compression, RGB, the strips, samples
//!    per pixel, 72 dpi, chunky planar configuration, `Photostacker
//!    <version>`, the predictor, ExtraSamples 2 (unassociated alpha) on a
//!    canvas with alpha, the ICC profile (34675) when the canvas has one, and
//!    ImageSourceData. There is no Orientation tag: the pixels are upright.
//!    StripOffsets and StripByteCounts are LONG, or LONG8 in a BigTIFF.
//!
//! The file is big-endian because the layer blocks are:
//! Photoshop writes ImageSourceData in the file's own byte order (`8BIMLayr`
//! when it saves Macintosh byte order, `MIB8ryaL` for IBM PC), and the
//! blocks here are the PSD writer's big-endian bytes. Photoshop
//! checkpoint 2 (spec §12.5) will verify that Photoshop reopens the layers.

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::ops::Range;

use flate2::Compression;
use flate2::write::ZlibEncoder;

use super::layers::{LayerRecord, LengthWidth, Spill};
use super::psd::{self, LayerInfo};
use crate::group::{CanvasProfile, Container};

/// The uncompressed size a strip aims at.
const STRIP_BYTES: u64 = 1 << 20;
/// The ImageSourceData signature of blocks with PSD widths, and of blocks
/// with PSB widths.
const DOCUMENT_DATA_BLOCK: &[u8; 36] = b"Adobe Photoshop Document Data Block\0";
const DOCUMENT_DATA_V0002: &[u8; 36] = b"Adobe Photoshop Document Data V0002\0";
/// The largest block of a stored deflate stream (RFC 1951, 3.2.4).
const STORED_BLOCK: usize = 65_535;

// Tags: TIFF 6.0's, then Adobe's ICC profile and ImageSourceData.
const IMAGE_WIDTH: u16 = 256;
const IMAGE_LENGTH: u16 = 257;
const BITS_PER_SAMPLE: u16 = 258;
const COMPRESSION: u16 = 259;
const PHOTOMETRIC: u16 = 262;
const STRIP_OFFSETS: u16 = 273;
const SAMPLES_PER_PIXEL: u16 = 277;
const ROWS_PER_STRIP: u16 = 278;
const STRIP_BYTE_COUNTS: u16 = 279;
const X_RESOLUTION: u16 = 282;
const Y_RESOLUTION: u16 = 283;
const PLANAR_CONFIGURATION: u16 = 284;
const RESOLUTION_UNIT: u16 = 296;
const SOFTWARE: u16 = 305;
const PREDICTOR: u16 = 317;
const EXTRA_SAMPLES: u16 = 338;
const ICC_PROFILE: u16 = 34675;
const IMAGE_SOURCE_DATA: u16 = 37724;

// Field types; LONG8 is BigTIFF's.
const ASCII: u16 = 2;
const SHORT: u16 = 3;
const LONG: u16 = 4;
const RATIONAL: u16 = 5;
const UNDEFINED: u16 = 7;
const LONG8: u16 = 16;

// Field values.
const ADOBE_DEFLATE: u16 = 8;
const PHOTOMETRIC_RGB: u16 = 2;
const CHUNKY: u16 = 1;
const INCH: u16 = 2;
const HORIZONTAL_DIFFERENCING: u16 = 2;
const UNASSOCIATED_ALPHA: u16 = 2;
const DPI: u32 = 72;

/// Writes the layered TIFF of `canvas` to `out`, which must be at its start
/// (TIFF offsets count from the start of the file), from the layer
/// `records` (top layer first, as the encoder made them) and the `spill`
/// files that hold their data.
///
/// `canvas.container` must be `Tiff` or `BigTiff`, and the layers must have
/// been encoded with its widths: `Psd` for a TIFF, `Psb` for a BigTIFF.
/// Fails with `InvalidInput`, before writing anything, when the canvas, the
/// records, the spill files and `out` do not fit together or do not fit the
/// container, which includes a classic TIFF that could pass 4 GiB and a
/// merged spill blended against white for a PSD.
pub(crate) fn write_tiff(
    out: &mut File,
    canvas: &CanvasProfile,
    records: &[LayerRecord],
    spill: &Spill,
) -> io::Result<()> {
    let layout = Layout::new(canvas)?;
    psd::check_spill_widths(spill, layout.widths)?;
    if spill.merged_on_white() {
        return Err(psd::invalid(
            "the merged image was blended against white for a PSD or PSB; \
             a TIFF's image keeps straight colour"
                .into(),
        ));
    }
    let info = LayerInfo::new(canvas, records, layout.widths)?;
    let mut layers = File::open(spill.layers_path())?;
    let mut merged = File::open(spill.merged_path())?;
    psd::check_spill(&layers, info.channel_data(), "layers")?;
    psd::check_spill(&merged, psd::merged_len(canvas), "merged")?;
    if out.stream_position()? != 0 {
        return Err(psd::invalid(
            "a TIFF must begin at the start of its file, as its offsets do".into(),
        ));
    }
    check_classic_len(layout.big, file_len_bound(canvas, &layout, &info)?)?;

    let mut tiff = Counted {
        inner: BufWriter::with_capacity(psd::COPY_CHUNK, &mut *out),
        pos: 0,
    };
    write_header(&mut tiff, layout.big)?;
    let (strip_offsets, strip_counts) = write_strips(&mut tiff, &mut merged, &layout)?;
    tiff.align()?;
    let source_at = tiff.pos;
    write_source_data(&mut tiff, canvas, &layout, &info, &mut layers)?;
    let source_len = source_data_len(&layout, &info);
    debug_assert_eq!(tiff.pos - source_at, source_len);
    let fields = fields(
        canvas,
        &layout,
        &strip_offsets,
        &strip_counts,
        (source_at, source_len),
    )?;
    let ifd = write_ifd(&mut tiff, &fields, layout.big)?;

    // The header's IFD offset, now that it is known. Seeking flushes.
    let mut out = tiff.inner;
    out.seek(SeekFrom::Start(if layout.big { 8 } else { 4 }))?;
    out.write_all(&offset(layout.big, ifd)?)?;
    out.seek(SeekFrom::End(0))?;
    out.flush()
}

/// How the composite is laid out in a container.
struct Layout {
    /// BigTIFF rather than classic TIFF.
    big: bool,
    /// The widths of the layer blocks.
    widths: LengthWidth,
    width: u32,
    height: u32,
    sixteen_bit: bool,
    /// Samples per pixel: 3, or 4 with alpha.
    samples: usize,
    /// The bytes of one row of one plane of `.merged`.
    plane_row: usize,
    /// The bytes of one composite row.
    row_bytes: u64,
    rows_per_strip: u32,
    strips: u32,
}

impl Layout {
    /// Fails with `InvalidInput` for a PSD or PSB canvas, an empty canvas,
    /// or a side past the limit of its layer widths: 30,000 pixels for a
    /// classic TIFF, whose layers have PSD widths, and 300,000 for a
    /// BigTIFF, as `psd::check_canvas` limits a PSD and a PSB.
    fn new(canvas: &CanvasProfile) -> io::Result<Self> {
        let (big, widths, max_side, name) = match canvas.container {
            Container::Tiff => (false, LengthWidth::Psd, psd::PSD_MAX_SIDE, "classic TIFF"),
            Container::BigTiff => (true, LengthWidth::Psb, psd::PSB_MAX_SIDE, "BigTIFF"),
            other => {
                return Err(psd::invalid(format!(
                    "a {other:?} canvas is not a TIFF or BigTIFF"
                )));
            }
        };
        let (width, height) = (canvas.width, canvas.height);
        if width == 0 || height == 0 || width > max_side || height > max_side {
            return Err(psd::invalid(format!(
                "a {width}x{height} canvas does not fit a {name}, whose sides are 1 to {max_side}"
            )));
        }
        let samples = usize::from(psd::channel_count(canvas));
        let sample_bytes = if canvas.sixteen_bit { 2 } else { 1 };
        let row_bytes = u64::from(width) * samples as u64 * sample_bytes;
        let rows_per_strip = rows_per_strip(row_bytes, height);
        Ok(Self {
            big,
            widths,
            width,
            height,
            sixteen_bit: canvas.sixteen_bit,
            samples,
            plane_row: width as usize * sample_bytes as usize,
            row_bytes,
            rows_per_strip,
            strips: height.div_ceil(rows_per_strip),
        })
    }

    /// The rows of strip `k`: `rows_per_strip` of them, fewer in the last.
    fn strip_rows(&self, k: u32) -> Range<u32> {
        let start = k * self.rows_per_strip;
        start..(start + self.rows_per_strip).min(self.height)
    }
}

/// The rows that fit in 1 MiB, at least one and at most `height`.
fn rows_per_strip(row_bytes: u64, height: u32) -> u32 {
    // At most `height`, so the cast cannot truncate.
    (STRIP_BYTES / row_bytes).clamp(1, u64::from(height)) as u32
}

/// An upper bound of the file's length, known before anything is written:
/// every strip at its stored size, which `compress_strip` never exceeds,
/// and a padding byte before every word-aligned item.
fn file_len_bound(canvas: &CanvasProfile, layout: &Layout, info: &LayerInfo) -> io::Result<u64> {
    let header = if layout.big { 16 } else { 8 };
    let strips: u64 = (0..layout.strips)
        .map(|k| stored_len(layout.strip_rows(k).len() as u64 * layout.row_bytes))
        .sum();
    let source_len = source_data_len(layout, info);
    // The values' sizes do not depend on the offsets, so zeros stand in.
    let zeros = vec![0; layout.strips as usize];
    let fields = fields(canvas, layout, &zeros, &zeros, (0, source_len))?;
    let inline = inline_size(layout.big);
    let values: u64 = fields
        .iter()
        .map(|field| match &field.value {
            Value::Bytes(bytes) if bytes.len() > inline => 1 + bytes.len() as u64,
            _ => 0,
        })
        .sum();
    let entries = fields.len() as u64;
    let ifd = if layout.big {
        8 + 20 * entries + 8
    } else {
        2 + 12 * entries + 4
    };
    Ok(header + strips + 1 + source_len + values + 1 + ifd)
}

/// Fails with `InvalidInput` when a classic TIFF could pass 4 GiB, past the
/// reach of its 4-byte offsets and counts. The canvas profile's size estimate
/// picks BigTIFF long before; compression that expands a noisy stack is what
/// is left.
fn check_classic_len(big: bool, len_bound: u64) -> io::Result<()> {
    if !big && len_bound > u64::from(u32::MAX) {
        return Err(psd::invalid(format!(
            "the TIFF may take {len_bound} bytes, past a classic TIFF's 4 GiB; it needs BigTIFF"
        )));
    }
    Ok(())
}

/// A writer that counts the bytes written: the position in the file.
struct Counted<W> {
    inner: W,
    pos: u64,
}

impl<W: Write> Write for Counted<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<W: Write> Counted<W> {
    /// Pads to an even position: TIFF values start on a word boundary.
    fn align(&mut self) -> io::Result<()> {
        if self.pos % 2 == 1 {
            self.write_all(&[0])?;
        }
        Ok(())
    }
}

/// The header, with an IFD offset of 0 that `write_tiff` fills in last.
fn write_header(out: &mut impl Write, big: bool) -> io::Result<()> {
    out.write_all(b"MM")?;
    if big {
        out.write_all(&43u16.to_be_bytes())?;
        out.write_all(&8u16.to_be_bytes())?; // the offset size
        out.write_all(&0u16.to_be_bytes())?;
        out.write_all(&0u64.to_be_bytes())
    } else {
        out.write_all(&42u16.to_be_bytes())?;
        out.write_all(&0u32.to_be_bytes())
    }
}

/// Writes the composite's strips from `.merged` and returns their offsets
/// and lengths. Holds one strip at a time: its rows of each plane, raw and
/// compressed.
fn write_strips<W: Write>(
    out: &mut Counted<W>,
    merged: &mut File,
    layout: &Layout,
) -> io::Result<(Vec<u64>, Vec<u64>)> {
    // Room for one strip of each plane, one plane after the other.
    let strip_plane = layout.plane_row * layout.rows_per_strip as usize;
    let mut planes = vec![0; strip_plane * layout.samples];
    let mut row = vec![0u16; layout.width as usize * layout.samples];
    let (mut raw, mut packed) = (Vec::new(), Vec::new());
    let mut offsets = Vec::with_capacity(layout.strips as usize);
    let mut counts = Vec::with_capacity(layout.strips as usize);
    for k in 0..layout.strips {
        let rows = layout.strip_rows(k);
        read_strip_planes(merged, layout, &rows, strip_plane, &mut planes)?;
        raw.clear();
        for r in 0..rows.len() {
            interleave(&planes, layout, strip_plane, r, &mut row);
            predict(&mut row, layout.samples, layout.sixteen_bit);
            if layout.sixteen_bit {
                raw.extend(row.iter().flat_map(|v| v.to_be_bytes()));
            } else {
                // Every value is below 256 on an 8-bit canvas.
                raw.extend(row.iter().map(|&v| v as u8));
            }
        }
        compress_strip(&raw, &mut packed)?;
        offsets.push(out.pos);
        counts.push(packed.len() as u64);
        out.write_all(&packed)?;
    }
    Ok((offsets, counts))
}

/// Reads `rows` of every plane of `.merged` (R, G, B, then alpha, each
/// `height` rows of `plane_row` bytes) into `planes`: plane `p` at
/// `p * strip_plane`. A plane's rows within a strip are contiguous, so each
/// plane takes one seek and one read.
fn read_strip_planes(
    merged: &mut File,
    layout: &Layout,
    rows: &Range<u32>,
    strip_plane: usize,
    planes: &mut [u8],
) -> io::Result<()> {
    let len = rows.len() * layout.plane_row;
    for (plane, buffer) in planes.chunks_exact_mut(strip_plane).enumerate() {
        let first = plane as u64 * u64::from(layout.height) + u64::from(rows.start);
        merged.seek(SeekFrom::Start(first * layout.plane_row as u64))?;
        merged.read_exact(&mut buffer[..len])?;
    }
    Ok(())
}

/// Interleaves row `r` of the strip in `planes` (laid out as
/// `read_strip_planes` leaves them) into `row`'s chunky samples. 16-bit
/// samples in `.merged` are big-endian.
fn interleave(planes: &[u8], layout: &Layout, strip_plane: usize, r: usize, row: &mut [u16]) {
    let at = r * layout.plane_row;
    for (plane, strip) in planes.chunks_exact(strip_plane).enumerate() {
        let bytes = &strip[at..at + layout.plane_row];
        let samples = row[plane..].iter_mut().step_by(layout.samples);
        if layout.sixteen_bit {
            for (sample, &pair) in samples.zip(bytes.as_chunks::<2>().0) {
                *sample = u16::from_be_bytes(pair);
            }
        } else {
            for (sample, &byte) in samples.zip(bytes) {
                *sample = u16::from(byte);
            }
        }
    }
}

/// Horizontal differencing (TIFF 6.0, section 14): every sample but the
/// first pixel's becomes its difference from the same component of the
/// pixel to its left, wrapping at the sample's width.
fn predict(row: &mut [u16], samples: usize, sixteen_bit: bool) {
    let mask = if sixteen_bit { u16::MAX } else { 0xFF };
    for i in (samples..row.len()).rev() {
        row[i] = row[i].wrapping_sub(row[i - samples]) & mask;
    }
}

/// Compresses `raw` into `packed` as one zlib stream: deflate at level 1,
/// or stored blocks when deflate would be larger, so the stream never
/// exceeds `stored_len`.
fn compress_strip(raw: &[u8], packed: &mut Vec<u8>) -> io::Result<()> {
    packed.clear();
    let mut zlib = ZlibEncoder::new(std::mem::take(packed), Compression::fast());
    zlib.write_all(raw)?;
    *packed = zlib.finish()?;
    if packed.len() as u64 > stored_len(raw.len() as u64) {
        stored_zlib(raw, packed);
    }
    Ok(())
}

/// The length of `stored_zlib`'s stream of `raw` bytes: the 2-byte header,
/// the data in blocks of at most 65,535 bytes with a 5-byte header each (at
/// least one block), and the 4-byte Adler-32.
fn stored_len(raw: u64) -> u64 {
    2 + raw + 5 * raw.div_ceil(STORED_BLOCK as u64).max(1) + 4
}

/// `raw` as a zlib stream of stored deflate blocks (RFC 1950 and 1951).
fn stored_zlib(raw: &[u8], packed: &mut Vec<u8>) {
    packed.clear();
    // Deflate with a 32 KiB window, the fastest level, and the check bits
    // that make the pair a multiple of 31.
    packed.extend_from_slice(&[0x78, 0x01]);
    let blocks = raw.len().div_ceil(STORED_BLOCK).max(1);
    for k in 0..blocks {
        let block = &raw[k * STORED_BLOCK..((k + 1) * STORED_BLOCK).min(raw.len())];
        // BFINAL on the last block, BTYPE 00 (stored), then the padding to
        // the byte boundary: the whole header byte.
        packed.push(u8::from(k + 1 == blocks));
        // A block is at most 65,535 bytes. LEN and NLEN are little-endian
        // in deflate, whatever the TIFF's byte order.
        let len = block.len() as u16;
        packed.extend_from_slice(&len.to_le_bytes());
        packed.extend_from_slice(&(!len).to_le_bytes());
        packed.extend_from_slice(block);
    }
    packed.extend_from_slice(&adler32(raw).to_be_bytes());
}

/// The Adler-32 checksum of RFC 1950.
fn adler32(bytes: &[u8]) -> u32 {
    const MODULUS: u32 = 65_521;
    let (mut a, mut b) = (1u32, 0u32);
    // 5552 bytes is the most that cannot overflow `b` between reductions.
    for chunk in bytes.chunks(5552) {
        for &byte in chunk {
            a += u32::from(byte);
            b += a;
        }
        a %= MODULUS;
        b %= MODULUS;
    }
    (b << 16) | a
}

/// Writes the ImageSourceData value: the signature, the layer block, then
/// an empty `LMsk`.
fn write_source_data(
    out: &mut impl Write,
    canvas: &CanvasProfile,
    layout: &Layout,
    info: &LayerInfo,
    layers: &mut File,
) -> io::Result<()> {
    out.write_all(match layout.widths {
        LengthWidth::Psd => DOCUMENT_DATA_BLOCK,
        LengthWidth::Psb => DOCUMENT_DATA_V0002,
    })?;
    let key = if canvas.sixteen_bit { b"Lr16" } else { b"Layr" };
    psd::write_layer_block(out, key, layout.widths, info, layers)?;
    out.write_all(b"8BIMLMsk")?;
    out.write_all(&psd::long(layout.widths, 0)?)
}

/// The length of the ImageSourceData value.
fn source_data_len(layout: &Layout, info: &LayerInfo) -> u64 {
    DOCUMENT_DATA_BLOCK.len() as u64
        + info.block_size(layout.widths)
        + 8
        + psd::long_size(layout.widths)
}

/// One IFD entry.
struct Field {
    tag: u16,
    kind: u16,
    count: u64,
    value: Value,
}

enum Value {
    /// Big-endian bytes: in the entry if they fit, else out of line.
    Bytes(Vec<u8>),
    /// Already written at this offset.
    At(u64),
}

fn shorts(tag: u16, values: &[u16]) -> Field {
    Field {
        tag,
        kind: SHORT,
        count: values.len() as u64,
        value: Value::Bytes(values.iter().flat_map(|v| v.to_be_bytes()).collect()),
    }
}

fn long(tag: u16, value: u32) -> Field {
    Field {
        tag,
        kind: LONG,
        count: 1,
        value: Value::Bytes(value.to_be_bytes().to_vec()),
    }
}

fn rational(tag: u16, numerator: u32, denominator: u32) -> Field {
    let mut bytes = numerator.to_be_bytes().to_vec();
    bytes.extend_from_slice(&denominator.to_be_bytes());
    Field {
        tag,
        kind: RATIONAL,
        count: 1,
        value: Value::Bytes(bytes),
    }
}

/// StripOffsets or StripByteCounts: LONG in a classic TIFF, LONG8 in a
/// BigTIFF.
fn strip_values(tag: u16, values: &[u64], big: bool) -> io::Result<Field> {
    let bytes = values
        .iter()
        .map(|&v| offset(big, v))
        .collect::<io::Result<Vec<_>>>()?
        .concat();
    Ok(Field {
        tag,
        kind: if big { LONG8 } else { LONG },
        count: values.len() as u64,
        value: Value::Bytes(bytes),
    })
}

/// The IFD entries, tags ascending. `source_data` is the ImageSourceData
/// value's offset and length.
fn fields(
    canvas: &CanvasProfile,
    layout: &Layout,
    strip_offsets: &[u64],
    strip_counts: &[u64],
    source_data: (u64, u64),
) -> io::Result<Vec<Field>> {
    let depth = if layout.sixteen_bit { 16 } else { 8 };
    let software = format!("{} {}\0", psd::APP_NAME, env!("CARGO_PKG_VERSION"));
    let mut fields = vec![
        long(IMAGE_WIDTH, layout.width),
        long(IMAGE_LENGTH, layout.height),
        shorts(BITS_PER_SAMPLE, &vec![depth; layout.samples]),
        shorts(COMPRESSION, &[ADOBE_DEFLATE]),
        shorts(PHOTOMETRIC, &[PHOTOMETRIC_RGB]),
        strip_values(STRIP_OFFSETS, strip_offsets, layout.big)?,
        shorts(SAMPLES_PER_PIXEL, &[layout.samples as u16]),
        long(ROWS_PER_STRIP, layout.rows_per_strip),
        strip_values(STRIP_BYTE_COUNTS, strip_counts, layout.big)?,
        rational(X_RESOLUTION, DPI, 1),
        rational(Y_RESOLUTION, DPI, 1),
        shorts(PLANAR_CONFIGURATION, &[CHUNKY]),
        shorts(RESOLUTION_UNIT, &[INCH]),
        Field {
            tag: SOFTWARE,
            kind: ASCII,
            count: software.len() as u64,
            value: Value::Bytes(software.into_bytes()),
        },
        shorts(PREDICTOR, &[HORIZONTAL_DIFFERENCING]),
    ];
    if canvas.has_alpha {
        fields.push(shorts(EXTRA_SAMPLES, &[UNASSOCIATED_ALPHA]));
    }
    if let Some(icc) = &canvas.icc {
        fields.push(Field {
            tag: ICC_PROFILE,
            kind: UNDEFINED,
            count: icc.len() as u64,
            value: Value::Bytes(icc.clone()),
        });
    }
    let (source_at, source_len) = source_data;
    fields.push(Field {
        tag: IMAGE_SOURCE_DATA,
        kind: UNDEFINED,
        count: source_len,
        value: Value::At(source_at),
    });
    Ok(fields)
}

/// The bytes of a value inside an IFD entry: 4 in a classic TIFF, 8 in a
/// BigTIFF.
fn inline_size(big: bool) -> usize {
    if big { 8 } else { 4 }
}

/// `value` as an offset or count: 4 bytes in a classic TIFF, 8 in a
/// BigTIFF. Fails with `InvalidInput` when it does not fit 4 bytes, which
/// `check_classic_len` rules out before anything is written.
fn offset(big: bool, value: u64) -> io::Result<Vec<u8>> {
    if big {
        return Ok(value.to_be_bytes().to_vec());
    }
    u32::try_from(value)
        .map(|v| v.to_be_bytes().to_vec())
        .map_err(|_| {
            psd::invalid(format!(
                "{value} does not fit a classic TIFF; it needs BigTIFF"
            ))
        })
}

/// Writes the values that do not fit in their entries, then the IFD, each
/// at an even offset, and returns the IFD's offset.
fn write_ifd<W: Write>(out: &mut Counted<W>, fields: &[Field], big: bool) -> io::Result<u64> {
    let inline = inline_size(big);
    // Each entry's value field: the bytes themselves when they fit, else
    // the offset of the bytes, written here or already.
    let mut entry_values = Vec::with_capacity(fields.len());
    for field in fields {
        entry_values.push(match &field.value {
            Value::Bytes(bytes) if bytes.len() > inline => {
                out.align()?;
                let at = out.pos;
                out.write_all(bytes)?;
                offset(big, at)?
            }
            Value::Bytes(bytes) => bytes.clone(),
            Value::At(at) => offset(big, *at)?,
        });
    }
    out.align()?;
    let ifd = out.pos;
    if big {
        out.write_all(&(fields.len() as u64).to_be_bytes())?;
    } else {
        // At most 18 entries.
        out.write_all(&(fields.len() as u16).to_be_bytes())?;
    }
    for (field, mut value) in fields.iter().zip(entry_values) {
        out.write_all(&field.tag.to_be_bytes())?;
        out.write_all(&field.kind.to_be_bytes())?;
        out.write_all(&offset(big, field.count)?)?;
        // An inline value is left-justified.
        value.resize(inline, 0);
        out.write_all(&value)?;
    }
    out.write_all(&vec![0; inline])?; // no next IFD
    Ok(ifd)
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read, Write};

    use super::*;
    use crate::decode::FullImage;
    use crate::write::WriteOptions;
    use crate::write::layers::LayerEncoder;
    use crate::write::psd::tests::{
        RLE16, ZIP, assert_layer, canvas, ids, image8, image16, plane, solid8,
    };
    use ::tiff::ColorType;
    use ::tiff::decoder::{Decoder, DecodingResult, Limits};
    use ::tiff::tags::Tag;
    use photostacker_testkit::psd_reader::{self, Layer};

    const MINUS: char = '\u{2212}';
    const IMAGE_SOURCE_DATA: Tag = Tag::Unknown(37724);
    const ICC: Tag = Tag::Unknown(34675);

    /// The widths a container's layers are encoded with.
    fn widths_of(canvas: &CanvasProfile) -> LengthWidth {
        match canvas.container {
            Container::BigTiff | Container::Psb => LengthWidth::Psb,
            _ => LengthWidth::Psd,
        }
    }

    /// Encodes `layers`, top layer first, on `canvas`, writes the TIFF to a
    /// `.partial` file as the stack writer will, and returns its bytes and
    /// the encoder's records.
    fn write_with_records(
        canvas: &CanvasProfile,
        opts: WriteOptions,
        layers: &[(&str, FullImage)],
    ) -> (Vec<u8>, Vec<LayerRecord>) {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("stack.tif");
        let mut encoder = LayerEncoder::new(&final_path, canvas, widths_of(canvas), opts).unwrap();
        for (name, image) in layers {
            encoder.add_layer(name, image.clone()).unwrap();
        }
        let (records, spill) = encoder.finish().unwrap();
        let partial = dir.path().join("stack.tif.partial");
        let mut out = File::create(&partial).unwrap();
        write_tiff(&mut out, canvas, &records, &spill).unwrap();
        drop(out);
        (std::fs::read(partial).unwrap(), records)
    }

    fn write(canvas: &CanvasProfile, opts: WriteOptions, layers: &[(&str, FullImage)]) -> Vec<u8> {
        write_with_records(canvas, opts, layers).0
    }

    fn decoder(bytes: &[u8]) -> Decoder<Cursor<&[u8]>> {
        Decoder::new(Cursor::new(bytes))
            .expect("the tiff crate opens our TIFF")
            .with_limits(Limits::unlimited())
    }

    /// The composite the TIFF must hold: the top layer at canvas depth,
    /// interleaved R, G, B, then alpha when the canvas has it.
    fn composite(top: &FullImage, canvas: &CanvasProfile) -> Vec<u16> {
        let (_, merged_ids) = ids(canvas);
        let planes: Vec<Vec<u16>> = merged_ids
            .iter()
            .map(|&id| plane(top, id, canvas.sixteen_bit))
            .collect();
        (0..planes[0].len())
            .flat_map(|i| planes.iter().map(move |p| p[i]))
            .collect()
    }

    /// The image the tiff crate decodes: dimensions, colour type and
    /// interleaved samples (8-bit ones as `v as u16`).
    fn read_composite(bytes: &[u8]) -> ((u32, u32), ColorType, Vec<u16>) {
        let mut decoder = decoder(bytes);
        let dimensions = decoder.dimensions().unwrap();
        let colortype = decoder.colortype().unwrap();
        let samples = match decoder.read_image().unwrap() {
            DecodingResult::U8(v) => v.into_iter().map(u16::from).collect(),
            DecodingResult::U16(v) => v,
            _ => panic!("neither 8 nor 16 bits"),
        };
        (dimensions, colortype, samples)
    }

    fn source_data(bytes: &[u8]) -> Vec<u8> {
        decoder(bytes).get_tag_u8_vec(IMAGE_SOURCE_DATA).unwrap()
    }

    /// A deterministic hash of `(x, y, c)`: noisy pixels, whose neighbours
    /// differ by anything, so the predictor's differences wrap.
    fn noise(x: u32, y: u32, c: u8) -> u32 {
        let mut h = x.wrapping_mul(73_856_093) ^ y.wrapping_mul(19_349_663) ^ u32::from(c) << 29;
        h ^= h >> 13;
        h = h.wrapping_mul(0x5bd1_e995);
        h ^ (h >> 15)
    }

    fn noisy(width: u32, height: u32, channels: u8, sixteen_bit: bool) -> FullImage {
        if sixteen_bit {
            image16(width, height, channels, |x, y, c| noise(x, y, c) as u16)
        } else {
            image8(width, height, channels, |x, y, c| noise(x, y, c) as u8)
        }
    }

    /// One IFD entry as written: the value field raw (4 bytes in a classic
    /// TIFF, 8 in a BigTIFF).
    struct Field {
        tag: u16,
        kind: u16,
        count: u64,
        value: Vec<u8>,
    }

    fn u16_be(bytes: &[u8], at: usize) -> u16 {
        u16::from_be_bytes(bytes[at..at + 2].try_into().unwrap())
    }

    fn u32_be(bytes: &[u8], at: usize) -> u64 {
        u64::from(u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()))
    }

    fn u64_be(bytes: &[u8], at: usize) -> u64 {
        u64::from_be_bytes(bytes[at..at + 8].try_into().unwrap())
    }

    /// Parses the header and the one IFD by hand: whether the file is a
    /// BigTIFF, and the entries in file order. Checks that there is no
    /// second IFD.
    fn ifd(bytes: &[u8]) -> (bool, Vec<Field>) {
        assert_eq!(&bytes[..2], b"MM", "big-endian");
        let big = match u16_be(bytes, 2) {
            42 => false,
            43 => {
                assert_eq!((u16_be(bytes, 4), u16_be(bytes, 6)), (8, 0));
                true
            }
            other => panic!("TIFF version {other}"),
        };
        let at = if big {
            u64_be(bytes, 8)
        } else {
            u32_be(bytes, 4)
        } as usize;
        assert_eq!(at % 2, 0, "the IFD is word-aligned");
        let (count, mut entry, size, value) = if big {
            (u64_be(bytes, at), at + 8, 20, 8)
        } else {
            (u64::from(u16_be(bytes, at)), at + 2, 12, 4)
        };
        let mut fields = Vec::new();
        for _ in 0..count {
            let count_len = size - 4 - value;
            let count = if big {
                u64_be(bytes, entry + 4)
            } else {
                u32_be(bytes, entry + 4)
            };
            fields.push(Field {
                tag: u16_be(bytes, entry),
                kind: u16_be(bytes, entry + 2),
                count,
                value: bytes[entry + 4 + count_len..entry + size].to_vec(),
            });
            entry += size;
        }
        let next = if big {
            u64_be(bytes, entry)
        } else {
            u32_be(bytes, entry)
        };
        assert_eq!(next, 0, "one IFD");
        (big, fields)
    }

    /// The bytes of `field`'s value: inline, or at its offset, which must
    /// be even.
    fn value<'a>(bytes: &'a [u8], big: bool, field: &'a Field) -> &'a [u8] {
        let unit = match field.kind {
            2 | 7 => 1,
            3 => 2,
            4 => 4,
            5 | 16 => 8,
            other => panic!("unexpected field type {other}"),
        };
        let len = field.count as usize * unit;
        if len <= field.value.len() {
            return &field.value[..len];
        }
        let at = if big {
            u64::from_be_bytes(field.value[..8].try_into().unwrap())
        } else {
            u64::from(u32::from_be_bytes(field.value[..4].try_into().unwrap()))
        } as usize;
        assert_eq!(at % 2, 0, "tag {} is word-aligned", field.tag);
        &bytes[at..at + len]
    }

    fn find(fields: &[Field], tag: u16) -> &Field {
        fields.iter().find(|f| f.tag == tag).unwrap()
    }

    fn shorts(bytes: &[u8]) -> Vec<u16> {
        bytes
            .chunks(2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
            .collect()
    }

    /// Checks `layers`, as the 37724 payload holds them (bottom layer
    /// first), against `expected`, top layer first.
    fn assert_layers(layers: &[Layer], canvas: &CanvasProfile, expected: &[(&str, FullImage)]) {
        let (record_ids, _) = ids(canvas);
        assert_eq!(layers.len(), expected.len());
        for (layer, (name, image)) in layers.iter().rev().zip(expected) {
            assert_layer(layer, name, canvas);
            let channel_ids: Vec<i16> = layer.channels.iter().map(|c| c.0).collect();
            assert_eq!(channel_ids, record_ids, "layer {name}");
            for (id, samples) in &layer.channels {
                assert_eq!(
                    samples,
                    &plane(image, *id, canvas.sixteen_bit),
                    "{name}, channel {id}"
                );
            }
        }
    }

    #[test]
    fn tiff_composite_reads_back_with_tiff_crate() {
        // A small canvas fits one strip; the larger ones need several, the
        // last one short: 1 MiB holds 349 rows of 3000 bytes, and 249 of
        // 4200.
        for (width, height, sixteen_bit, strips, rows) in [
            (9, 5, false, 1, 5),
            (9, 5, true, 1, 5),
            (1000, 700, false, 3, 349),
            (700, 400, true, 2, 249),
        ] {
            let canvas = canvas(width, height, sixteen_bit, false, Container::Tiff);
            let top = noisy(width, height, 3, sixteen_bit);
            let layers = [
                ("top", top.clone()),
                ("below", solid8(width, height, [1, 2, 3])),
            ];
            let bytes = write(&canvas, ZIP, &layers);
            let (dimensions, colortype, samples) = read_composite(&bytes);
            let depth = if sixteen_bit { 16 } else { 8 };
            assert_eq!(dimensions, (width, height));
            assert_eq!(colortype, ColorType::RGB(depth));
            assert!(samples == composite(&top, &canvas), "{width}x{height}");

            let mut decoder = decoder(&bytes);
            let offsets = decoder.get_tag_u64_vec(Tag::StripOffsets).unwrap();
            assert_eq!(offsets.len(), strips, "{width}x{height}");
            assert_eq!(decoder.get_tag_u32(Tag::RowsPerStrip).unwrap(), rows);
        }
    }

    #[test]
    fn tiff_ifd_has_the_fixed_tags_in_order() {
        let software = format!("Photostacker {}\0", env!("CARGO_PKG_VERSION"));
        for container in [Container::Tiff, Container::BigTiff] {
            for (sixteen_bit, has_alpha, icc) in [(false, false, false), (true, true, true)] {
                let mut canvas = canvas(6, 4, sixteen_bit, has_alpha, container);
                if icc {
                    canvas.icc = Some(b"a profile".to_vec());
                }
                let bytes = write(&canvas, ZIP, &[("only", solid8(6, 4, [1, 2, 3]))]);
                let (big, fields) = ifd(&bytes);
                assert_eq!(big, container == Container::BigTiff);
                let tags: Vec<u16> = fields.iter().map(|f| f.tag).collect();
                let mut expected = vec![
                    256, 257, 258, 259, 262, 273, 277, 278, 279, 282, 283, 284, 296, 305, 317,
                ];
                if has_alpha {
                    expected.push(338);
                }
                if icc {
                    expected.push(34675);
                }
                expected.push(37724);
                assert_eq!(tags, expected, "ascending, and no Orientation (274)");

                let short = |tag| shorts(value(&bytes, big, find(&fields, tag)));
                let samples = if has_alpha { 4 } else { 3 };
                let depth = if sixteen_bit { 16 } else { 8 };
                assert_eq!(short(258), vec![depth; samples]);
                assert_eq!(short(259), [8], "Adobe Deflate");
                assert_eq!(short(262), [2], "RGB");
                assert_eq!(short(277), [samples as u16]);
                assert_eq!(short(284), [1], "chunky");
                assert_eq!(short(296), [2], "inches");
                assert_eq!(short(317), [2], "horizontal differencing");
                if has_alpha {
                    assert_eq!(short(338), [2], "unassociated alpha");
                }
                let rational: Vec<u8> = [72u32, 1].iter().flat_map(|v| v.to_be_bytes()).collect();
                for tag in [282, 283] {
                    assert_eq!(find(&fields, tag).kind, 5);
                    assert_eq!(value(&bytes, big, find(&fields, tag)), rational);
                }
                assert_eq!(find(&fields, 305).kind, 2);
                assert_eq!(value(&bytes, big, find(&fields, 305)), software.as_bytes());
                let strip_kind = if big { 16 } else { 4 };
                assert_eq!(find(&fields, 273).kind, strip_kind, "StripOffsets");
                assert_eq!(find(&fields, 279).kind, strip_kind, "StripByteCounts");
                for tag in [34675, 37724] {
                    if let Some(field) = fields.iter().find(|f| f.tag == tag) {
                        assert_eq!(field.kind, 7, "UNDEFINED");
                    }
                }
                // Every value out of line is word-aligned; `value` checks.
                for field in &fields {
                    value(&bytes, big, field);
                }
            }
        }
    }

    #[test]
    fn tiff_has_the_software_tag() {
        // Discovery skips the TIFFs whose Software starts with Photostacker.
        let expected = format!("Photostacker {}", env!("CARGO_PKG_VERSION"));
        for container in [Container::Tiff, Container::BigTiff] {
            let canvas = canvas(4, 2, false, false, container);
            let bytes = write(&canvas, ZIP, &[("only", solid8(4, 2, [1, 2, 3]))]);
            let software = decoder(&bytes).get_tag_ascii_string(Tag::Software);
            assert_eq!(software.unwrap(), expected, "{container:?}");
        }
    }

    #[test]
    fn tiff_layers_parse_from_tag_37724() {
        let names = [
            "IMG_0001 (0 EV)".to_owned(),
            "IMG_0002 (+1 EV)".to_owned(),
            format!("IMG_0003 ({MINUS}1 EV)"),
        ];
        for (sixteen_bit, opts) in [(false, ZIP), (true, ZIP), (true, RLE16)] {
            let canvas = canvas(8, 6, sixteen_bit, false, Container::Tiff);
            let layers = [
                (names[0].as_str(), noisy(8, 6, 3, sixteen_bit)),
                (names[1].as_str(), solid8(8, 6, [10, 200, 10])),
                (
                    names[2].as_str(),
                    image8(8, 6, 3, |x, y, c| (x * 9 + y + u32::from(c)) as u8),
                ),
            ];
            let bytes = write(&canvas, opts, &layers);
            let payload = source_data(&bytes);
            let parsed = psd_reader::read_layer_block(&payload).unwrap();
            assert_layers(&parsed, &canvas, &layers);
            assert_eq!(
                parsed[0].name_pascal, "IMG_0003 (-1 EV)",
                "bottom layer first"
            );

            // The signature, `8BIM` and the key, a 4-byte length that is
            // even, the layer info with a positive count, then `LMsk` of
            // length 0.
            assert_eq!(&payload[..36], b"Adobe Photoshop Document Data Block\0");
            let key: &[u8] = if sixteen_bit {
                b"8BIMLr16"
            } else {
                b"8BIMLayr"
            };
            assert_eq!(&payload[36..44], key, "{opts:?}");
            let len = u64::from(u32::from_be_bytes(payload[44..48].try_into().unwrap()));
            assert_eq!(len % 2, 0);
            assert_eq!(payload[48..50], 3i16.to_be_bytes());
            assert_eq!(payload.len() as u64, 48 + len.next_multiple_of(4) + 12);
            assert_eq!(&payload[payload.len() - 12..], b"8BIMLMsk\0\0\0\0");
        }
    }

    #[test]
    fn tiff_icc_tag_present() {
        let mut with_icc = canvas(4, 2, false, false, Container::Tiff);
        // An odd length, so the next value needs a padding byte.
        let icc = b"not a real profile, 33 bytes long".to_vec();
        assert_eq!(icc.len() % 2, 1);
        with_icc.icc = Some(icc.clone());
        let layers = [("only", solid8(4, 2, [5, 6, 7]))];
        let bytes = write(&with_icc, ZIP, &layers);
        assert_eq!(decoder(&bytes).get_tag_u8_vec(ICC).unwrap(), icc);
        assert_eq!(read_composite(&bytes).2, composite(&layers[0].1, &with_icc));

        let without = canvas(4, 2, false, false, Container::Tiff);
        let bytes = write(&without, ZIP, &layers);
        assert!(decoder(&bytes).find_tag(ICC).unwrap().is_none());
    }

    #[test]
    fn bigtiff_reads_back_with_tiff_crate() {
        for (sixteen_bit, opts) in [(false, ZIP), (true, ZIP), (true, RLE16)] {
            let canvas = canvas(7, 3, sixteen_bit, false, Container::BigTiff);
            let top = noisy(7, 3, 3, sixteen_bit);
            let layers = [("top", top.clone()), ("bottom", solid8(7, 3, [4, 5, 6]))];
            let bytes = write(&canvas, opts, &layers);
            assert_eq!(
                bytes[..8],
                [b'M', b'M', 0, 43, 0, 8, 0, 0],
                "BigTIFF header"
            );
            let (dimensions, colortype, samples) = read_composite(&bytes);
            assert_eq!(dimensions, (7, 3));
            assert_eq!(colortype, ColorType::RGB(if sixteen_bit { 16 } else { 8 }));
            assert_eq!(samples, composite(&top, &canvas));

            // PSB widths: the V0002 signature, and 8-byte lengths for the
            // layer block and `LMsk`, both long keys.
            let payload = source_data(&bytes);
            assert_eq!(&payload[..36], b"Adobe Photoshop Document Data V0002\0");
            let key: &[u8] = if sixteen_bit {
                b"8BIMLr16"
            } else {
                b"8BIMLayr"
            };
            assert_eq!(&payload[36..44], key);
            let len = u64::from_be_bytes(payload[44..52].try_into().unwrap());
            assert_eq!(payload.len() as u64, 52 + len.next_multiple_of(4) + 16);
            assert_eq!(&payload[payload.len() - 16..], b"8BIMLMsk\0\0\0\0\0\0\0\0");
            let parsed = psd_reader::read_layer_block(&payload).unwrap();
            assert_layers(&parsed, &canvas, &layers);
        }
    }

    #[test]
    fn tiff_rgba_has_extra_samples() {
        for (container, sixteen_bit) in [
            (Container::Tiff, false),
            (Container::Tiff, true),
            (Container::BigTiff, false),
        ] {
            let canvas = canvas(5, 3, sixteen_bit, true, container);
            let top = image8(5, 3, 4, |x, y, c| (x * 50 + y + u32::from(c) * 60) as u8);
            let layers = [("rgba", top.clone()), ("rgb", solid8(5, 3, [9, 8, 7]))];
            let bytes = write(&canvas, ZIP, &layers);
            let mut decoder = decoder(&bytes);
            assert_eq!(decoder.get_tag_u16_vec(Tag::ExtraSamples).unwrap(), [2]);
            assert_eq!(decoder.get_tag_u32(Tag::SamplesPerPixel).unwrap(), 4);
            let (_, colortype, samples) = read_composite(&bytes);
            assert_eq!(colortype, ColorType::RGBA(if sixteen_bit { 16 } else { 8 }));
            assert_eq!(samples, composite(&top, &canvas), "alpha last");

            // The count is negative: the composite's alpha is its
            // transparency.
            let payload = source_data(&bytes);
            let count_at = 44
                + if container == Container::BigTiff {
                    8
                } else {
                    4
                };
            assert_eq!(payload[count_at..count_at + 2], (-2i16).to_be_bytes());
            let parsed = psd_reader::read_layer_block(&payload).unwrap();
            assert_layers(&parsed, &canvas, &layers);
        }
    }

    #[test]
    fn refuses_what_does_not_fit_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let fits = canvas(4, 2, false, false, Container::Tiff);
        let encode = |name: &str, widths, layers: &[&str]| {
            let mut encoder =
                LayerEncoder::new(&dir.path().join(name), &fits, widths, ZIP).unwrap();
            for layer in layers {
                encoder.add_layer(layer, solid8(4, 2, [1, 2, 3])).unwrap();
            }
            encoder.finish().unwrap()
        };
        let (records, spill) = encode("a.tif", LengthWidth::Psd, &["a"]);
        let (psb_records, psb_spill) = encode("b.tif", LengthWidth::Psb, &["b"]);
        let (no_records, no_spill) = encode("c.tif", LengthWidth::Psd, &[]);

        let mut psd = fits.clone();
        psd.container = Container::Psd;
        let mut big = fits.clone();
        big.container = Container::BigTiff;
        let mut too_wide = fits.clone();
        too_wide.width = 300_001;
        let mut with_alpha = fits.clone();
        with_alpha.has_alpha = true;
        let mut other_size = fits.clone();
        other_size.height = 3;
        // Encoded for a PSD with alpha, whose merged spill is blended
        // against white.
        let mut psd_alpha = with_alpha.clone();
        psd_alpha.container = Container::Psd;
        let mut encoder =
            LayerEncoder::new(&dir.path().join("d.psd"), &psd_alpha, LengthWidth::Psd, ZIP)
                .unwrap();
        encoder
            .add_layer("d", image8(4, 2, 4, |_, _, c| 60 * c))
            .unwrap();
        let (on_white_records, on_white_spill) = encoder.finish().unwrap();
        let cases: [(&CanvasProfile, &[LayerRecord], &Spill); 8] = [
            (&psd, &records, &spill),
            // A TIFF's layers use PSD widths and a BigTIFF's PSB widths.
            (&fits, &psb_records, &psb_spill),
            (&big, &records, &spill),
            (&too_wide, &records, &spill),
            (&with_alpha, &records, &spill),
            (&other_size, &records, &spill),
            (&fits, &no_records, &no_spill),
            (&with_alpha, &on_white_records, &on_white_spill),
        ];
        for (i, (canvas, records, spill)) in cases.into_iter().enumerate() {
            let path = dir.path().join(format!("out{i}.tif.partial"));
            let mut out = File::create(&path).unwrap();
            let error = write_tiff(&mut out, canvas, records, spill).unwrap_err();
            assert_eq!(
                error.kind(),
                io::ErrorKind::InvalidInput,
                "case {i}: {error}"
            );
            assert_eq!(std::fs::metadata(&path).unwrap().len(), 0, "case {i}");
        }
        // The refusal names the container as the docs do.
        let mut out = File::create(dir.path().join("wide.tif.partial")).unwrap();
        let error = write_tiff(&mut out, &too_wide, &records, &spill).unwrap_err();
        assert!(
            error.to_string().contains("does not fit a classic TIFF,"),
            "{error}"
        );

        // The offsets count from the start of the file, so the writer
        // starts there.
        let path = dir.path().join("late.tif.partial");
        let mut out = File::create(&path).unwrap();
        out.write_all(b"x").unwrap();
        let error = write_tiff(&mut out, &fits, &records, &spill).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{error}");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 1);
    }

    #[test]
    fn classic_tiff_up_to_u32_max_bytes_is_accepted_past_it_refused() {
        let limit = u64::from(u32::MAX);
        assert!(check_classic_len(false, limit).is_ok());
        let error = check_classic_len(false, limit + 1).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(check_classic_len(true, limit * 4).is_ok());

        // The bound counts every byte written, and more only by the
        // strips' room to stay stored and by padding not needed.
        for container in [Container::Tiff, Container::BigTiff] {
            for (sixteen_bit, has_alpha, icc) in [(false, false, false), (true, true, true)] {
                let mut canvas = canvas(300, 50, sixteen_bit, has_alpha, container);
                if icc {
                    canvas.icc = Some(b"odd profile".to_vec());
                }
                let layers = [
                    ("noise", noisy(300, 50, 3, sixteen_bit)),
                    ("flat", solid8(300, 50, [1, 2, 3])),
                ];
                let (bytes, records) = write_with_records(&canvas, ZIP, &layers);
                let layout = Layout::new(&canvas).unwrap();
                let info = LayerInfo::new(&canvas, &records, layout.widths).unwrap();
                let bound = file_len_bound(&canvas, &layout, &info).unwrap();

                let mut decoder = decoder(&bytes);
                let counts = decoder.get_tag_u64_vec(Tag::StripByteCounts).unwrap();
                let rows = u64::from(decoder.get_tag_u32(Tag::RowsPerStrip).unwrap());
                let samples = if has_alpha { 4 } else { 3 };
                let row_bytes = 300 * samples * if sixteen_bit { 2 } else { 1 };
                let stored: u64 = (0..counts.len() as u64)
                    .map(|k| stored_len(rows.min(50 - k * rows) * row_bytes))
                    .sum();
                let slack = stored - counts.iter().sum::<u64>();
                let len = bytes.len() as u64;
                assert!(
                    bound >= len + slack,
                    "{container:?}: {bound} < {len} + {slack}"
                );
                assert!(bound - len - slack <= 8, "{container:?}: padding only");
            }
        }
    }

    #[test]
    fn layout_caps_the_sides_of_each_container() {
        // A classic TIFF carries PSD widths, so it has the PSD's 30,000 px
        // limit; a BigTIFF has the PSB's 300,000.
        for (container, max) in [(Container::Tiff, 30_000), (Container::BigTiff, 300_000)] {
            for (width, height) in [(max, 1), (1, max)] {
                let fits = canvas(width, height, false, false, container);
                assert!(Layout::new(&fits).is_ok(), "{container:?} {width}x{height}");
            }
            for (width, height) in [(max + 1, 1), (1, max + 1), (0, 1), (1, 0)] {
                let refused = canvas(width, height, false, false, container);
                let error = Layout::new(&refused).err().unwrap();
                assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{error}");
                assert!(
                    error.to_string().contains(&format!("sides are 1 to {max}")),
                    "{error}"
                );
            }
        }
    }

    #[test]
    fn sixteen_bit_composite_is_big_endian_in_the_strips() {
        // The tiff crate decodes either byte order, so the strips are read
        // by hand: inflate each one, read its samples big-endian, undo the
        // predictor, and compare with the top layer. A row is 5600 bytes, so
        // 1 MiB holds 187 rows: three strips, the last one short.
        let (width, height) = (700, 400);
        let row_bytes = width as usize * 4 * 2;
        for container in [Container::Tiff, Container::BigTiff] {
            let canvas = canvas(width, height, true, true, container);
            let top = noisy(width, height, 4, true);
            let layers = [
                ("top", top.clone()),
                ("below", solid8(width, height, [1, 2, 3])),
            ];
            let bytes = write(&canvas, ZIP, &layers);
            let (big, fields) = ifd(&bytes);
            let read = |tag| -> Vec<u64> {
                let field = find(&fields, tag);
                let unit = if big { 8 } else { 4 };
                value(&bytes, big, field)
                    .chunks(unit)
                    .map(|b| if big { u64_be(b, 0) } else { u32_be(b, 0) })
                    .collect()
            };
            let (offsets, counts) = (read(273), read(279));
            assert_eq!(offsets.len(), 3, "{container:?}");
            let rows_per_strip = value(&bytes, big, find(&fields, 278));
            assert_eq!(u32_be(rows_per_strip, 0), 187, "a LONG in both");
            let mut samples = Vec::new();
            let mut strip_rows = Vec::new();
            for (offset, count) in offsets.into_iter().zip(counts) {
                let strip = &bytes[offset as usize..(offset + count) as usize];
                let mut inflated = Vec::new();
                flate2::read::ZlibDecoder::new(strip)
                    .read_to_end(&mut inflated)
                    .unwrap();
                assert_eq!(inflated.len() % row_bytes, 0);
                strip_rows.push(inflated.len() / row_bytes);
                for row in inflated.chunks(row_bytes) {
                    let mut values: Vec<u16> =
                        (0..row.len() / 2).map(|i| u16_be(row, 2 * i)).collect();
                    for i in 4..values.len() {
                        values[i] = values[i].wrapping_add(values[i - 4]);
                    }
                    samples.extend(values);
                }
            }
            assert_eq!(strip_rows, [187, 187, 26], "{container:?}");
            assert!(samples == composite(&top, &canvas), "{container:?}");
        }
    }

    #[test]
    fn rows_per_strip_fill_about_one_mib() {
        assert_eq!(rows_per_strip(3000, 700), 349);
        assert_eq!(rows_per_strip(1 << 20, 10), 1);
        // A row past 1 MiB is a strip of its own.
        assert_eq!(rows_per_strip(300_000 * 8, 10), 1);
        // A small image is one strip.
        assert_eq!(rows_per_strip(27, 5), 5);
    }

    #[test]
    fn strips_never_exceed_their_stored_size() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let random: Vec<u8> = (0..200_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let flat = vec![7u8; 200_000];
        for raw in [
            &random[..],
            &flat[..],
            &random[..1],
            &random[..65_535],
            &random[..65_536],
        ] {
            let mut packed = vec![1, 2, 3]; // stale bytes must not survive
            compress_strip(raw, &mut packed).unwrap();
            assert!(packed.len() as u64 <= stored_len(raw.len() as u64));
            let mut inflated = Vec::new();
            flate2::read::ZlibDecoder::new(&packed[..])
                .read_to_end(&mut inflated)
                .unwrap();
            assert!(inflated == raw, "{} bytes", raw.len());
        }
        assert_eq!(stored_len(1), 2 + 1 + 5 + 4);
        assert_eq!(stored_len(65_535), 2 + 65_535 + 5 + 4);
        assert_eq!(stored_len(65_536), 2 + 65_536 + 10 + 4);
    }

    #[test]
    fn stored_zlib_is_a_valid_stream() {
        let raw: Vec<u8> = (0..150_000u32).map(|i| (i * 7 + i / 300) as u8).collect();
        for raw in [&raw[..], &raw[..1], &raw[..65_535], &raw[..65_536]] {
            let mut packed = Vec::new();
            stored_zlib(raw, &mut packed);
            assert_eq!(packed.len() as u64, stored_len(raw.len() as u64));
            let mut inflated = Vec::new();
            flate2::read::ZlibDecoder::new(&packed[..])
                .read_to_end(&mut inflated)
                .unwrap();
            assert!(inflated == raw, "{} bytes", raw.len());
        }
    }
}
