//! The shared layer encoder (spec §6.1).
//!
//! The PSD/PSB and layered-TIFF writers both store Photoshop layer channels,
//! so one encoder prepares them. It takes one full-resolution layer at a
//! time, compresses each channel and appends it to the spill file
//! `<final>.partial.layers` in the output folder, then frees the layer before
//! the next one arrives (the memory anti-goal). The first layer is also kept,
//! uncompressed, in `<final>.partial.merged`, for the composite.
//!
//! The merged spill is made for one container. For a PSD or PSB on a canvas
//! with alpha, each colour sample is the first layer's blended against white
//! with its pixel's alpha, rounded to nearest: `(c * a + max * (max - a) +
//! max / 2) / max`, `max` 255 or 65535. That is Photoshop's convention for
//! the merged image of a document with transparency, which readers of the
//! merged image (GIMP, ImageMagick) undo (spec §6.2). The alpha is stored as
//! it is. A layered TIFF's image declares unassociated alpha (ExtraSamples
//! 2), so its spill keeps the straight colour (spec §6.3), and so does any
//! canvas without alpha. The layers' channels are always straight.
//!
//! Channel data follows the Adobe Photoshop File Formats Specification
//! ("Channel image data"): a 2-byte compression code, then
//! - PackBits (1): one byte count per row (2 bytes in a PSD, 4 in a PSB),
//!   then each row packed separately;
//! - ZIP with prediction (3): one zlib stream of the rows, each row
//!   delta-encoded on its 16-bit samples.
//!
//! All multi-byte values are big-endian.

use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::ZlibEncoder;
use rayon::prelude::*;

use super::{Compression16, WriteOptions};
use crate::decode::{FullImage, Samples};
use crate::fsio::{create_new, with_suffix};
use crate::group::{CanvasProfile, Container};

/// PSD compression code of PackBits RLE.
const RLE: u16 = 1;
/// PSD compression code of ZIP with prediction.
const ZIP_PREDICTION: u16 = 3;
/// Channel id of the transparency channel.
const ALPHA_ID: i16 = -1;

/// The integer widths of a container: PSD or PSB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LengthWidth {
    /// RLE row byte counts are u16, channel lengths u32.
    Psd,
    /// RLE row byte counts are u32, channel lengths u64.
    Psb,
}

/// One layer as the container writers record it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LayerRecord {
    pub name: String,
    /// In Photoshop's order: alpha first when the canvas has it, then R, G, B.
    pub channels: Vec<ChannelRecord>,
}

/// One compressed channel inside the `.layers` spill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChannelRecord {
    /// -1 alpha, 0 R, 1 G, 2 B.
    pub id: i16,
    /// The PSD compression code: 1 (PackBits) or 3 (ZIP with prediction).
    pub compression: u16,
    /// Where the channel data starts in `.layers`.
    pub offset: u64,
    /// The channel data's length, its 2-byte compression code included.
    pub len: u64,
}

/// Encodes the layers of one stack, one at a time, into the spill files.
pub(crate) struct LayerEncoder {
    shape: Shape,
    widths: LengthWidth,
    opts: WriteOptions,
    layers: SpillFile,
    merged: SpillFile,
    /// The length of `.layers` so far: the next channel's offset.
    layers_len: u64,
    records: Vec<LayerRecord>,
    /// An earlier `add_layer` failed after validation: the spill files may
    /// hold part of a layer, so no further layer is accepted.
    failed: bool,
}

/// What the encoder needs of the canvas.
#[derive(Debug, Clone, Copy)]
struct Shape {
    width: u32,
    height: u32,
    sixteen_bit: bool,
    has_alpha: bool,
    /// The merged spill's colour is blended against white: a PSD or PSB
    /// canvas with alpha (see the module documentation).
    on_white: bool,
}

impl LayerEncoder {
    /// Creates the spill files `<final>.partial.layers` and
    /// `<final>.partial.merged`. Fails with `AlreadyExists` if either exists
    /// (an earlier crash, or another writer); a file that was already there
    /// is left untouched.
    ///
    /// `canvas.container` decides what the merged spill holds (see the
    /// module documentation), so the spill is only good for that container.
    pub(crate) fn new(
        final_path: &Path,
        canvas: &CanvasProfile,
        widths: LengthWidth,
        opts: WriteOptions,
    ) -> io::Result<Self> {
        let layers = SpillFile::create(with_suffix(final_path, ".partial.layers"))?;
        // On error `layers` is dropped, which removes the file just created.
        let merged = SpillFile::create(with_suffix(final_path, ".partial.merged"))?;
        Ok(Self {
            shape: Shape {
                width: canvas.width,
                height: canvas.height,
                sixteen_bit: canvas.sixteen_bit,
                has_alpha: canvas.has_alpha,
                on_white: canvas.has_alpha
                    && matches!(canvas.container, Container::Psd | Container::Psb),
            },
            widths,
            opts,
            layers,
            merged,
            layers_len: 0,
            records: Vec::new(),
            failed: false,
        })
    }

    /// Fails with `InvalidInput` when `image` does not fit the canvas, as
    /// [`LayerEncoder::add_layer`] would before writing anything; the
    /// encoder is left as it was.
    pub(crate) fn check(&self, image: &FullImage) -> io::Result<()> {
        self.shape.check(image)
    }

    /// Encodes `image` as the next layer, below the ones already added, and
    /// frees it before returning.
    ///
    /// Fails with `InvalidInput`, before writing anything, when the image
    /// does not fit the canvas: other dimensions, alpha on a canvas without
    /// it, 16-bit samples on an 8-bit canvas, or samples that do not match
    /// its size. After any other error the spill files may hold part of the
    /// layer: every later call fails, and the encoder is only good to drop.
    pub(crate) fn add_layer(&mut self, name: &str, image: FullImage) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other(
                "an earlier layer failed to encode; the spill files are incomplete",
            ));
        }
        self.shape.check(&image)?;
        let result = self.encode(name, &image);
        if result.is_err() {
            self.failed = true;
        }
        drop(image);
        result
    }

    /// The layer records, in the order they were added, and the spill files
    /// that hold their data. The files are closed.
    ///
    /// Fails when an earlier `add_layer` failed past validation, so a stack
    /// missing a layer can never be written; the spill files are removed.
    pub(crate) fn finish(mut self) -> io::Result<(Vec<LayerRecord>, Spill)> {
        if self.failed {
            return Err(io::Error::other(
                "an earlier layer failed to encode; the stack is incomplete",
            ));
        }
        self.layers.file = None;
        self.merged.file = None;
        let spill = Spill {
            layers: self.layers,
            merged: self.merged,
            widths: self.widths,
            on_white: self.shape.on_white,
        };
        Ok((self.records, spill))
    }

    /// Compresses the channels of a checked `image` in parallel, appends them
    /// to `.layers` in record order, and writes `.merged` for the first layer.
    fn encode(&mut self, name: &str, image: &FullImage) -> io::Result<()> {
        let shape = self.shape;
        let widths = self.widths;
        let compression = match (shape.sixteen_bit, self.opts.compression16) {
            (true, Compression16::Zip) => ZIP_PREDICTION,
            _ => RLE,
        };
        let ids = shape.record_ids();
        let encoded: Vec<io::Result<Vec<u8>>> = ids
            .par_iter()
            .map(|&id| {
                let view = ChannelView::new(image, id);
                if compression == ZIP_PREDICTION {
                    encode_zip_prediction(&view)
                } else {
                    encode_rle(&view, shape.sixteen_bit, widths)
                }
            })
            .collect();

        let mut channels = Vec::with_capacity(ids.len());
        for (&id, data) in ids.iter().zip(encoded) {
            let data = data?;
            let len = data.len() as u64;
            if widths == LengthWidth::Psd && len > u64::from(u32::MAX) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("a channel of {len} bytes does not fit a PSD; the canvas needs PSB"),
                ));
            }
            self.layers.file().write_all(&data)?;
            channels.push(ChannelRecord {
                id,
                compression,
                offset: self.layers_len,
                len,
            });
            self.layers_len += len;
        }

        if self.records.is_empty() {
            write_merged(self.merged.file(), image, shape)?;
        }
        self.records.push(LayerRecord {
            name: name.to_owned(),
            channels,
        });
        Ok(())
    }
}

impl Shape {
    /// Fails with `InvalidInput` when `image` does not fit this canvas.
    fn check(&self, image: &FullImage) -> io::Result<()> {
        let refuse = |why: String| Err(io::Error::new(io::ErrorKind::InvalidInput, why));
        if (image.width, image.height) != (self.width, self.height) {
            return refuse(format!(
                "the layer is {}x{}, the canvas {}x{}",
                image.width, image.height, self.width, self.height
            ));
        }
        match image.channels {
            3 => {}
            4 if self.has_alpha => {}
            4 => return refuse("the layer has alpha, the canvas has none".into()),
            n => return refuse(format!("the layer has {n} channels, not 3 or 4")),
        }
        let len = match &image.samples {
            Samples::U8(samples) => samples.len(),
            Samples::U16(_) if !self.sixteen_bit => {
                return refuse("the layer is 16-bit, the canvas 8-bit".into());
            }
            Samples::U16(samples) => samples.len(),
        };
        let expected = (image.width as usize)
            .checked_mul(image.height as usize)
            .and_then(|n| n.checked_mul(usize::from(image.channels)));
        if expected != Some(len) {
            return refuse(format!(
                "the layer has {len} samples, not {}x{}x{}",
                image.width, image.height, image.channels
            ));
        }
        Ok(())
    }

    /// The channel ids of a layer record, in Photoshop's order: alpha first.
    fn record_ids(&self) -> &'static [i16] {
        if self.has_alpha {
            &[ALPHA_ID, 0, 1, 2]
        } else {
            &[0, 1, 2]
        }
    }

    /// The channel ids of the merged image, in its order: alpha last.
    fn merged_ids(&self) -> &'static [i16] {
        if self.has_alpha {
            &[0, 1, 2, ALPHA_ID]
        } else {
            &[0, 1, 2]
        }
    }
}

/// One channel of a layer at canvas depth, read row by row from the
/// interleaved samples: no planar copy of the layer is made.
struct ChannelView<'a> {
    image: &'a FullImage,
    /// The channel's sample within a pixel (R 0, G 1, B 2, A 3); `None` for
    /// the alpha of a layer without one, which is opaque.
    sample: Option<usize>,
}

impl<'a> ChannelView<'a> {
    fn new(image: &'a FullImage, id: i16) -> Self {
        let sample = match id {
            ALPHA_ID => (image.channels == 4).then_some(3),
            id => Some(id as usize),
        };
        Self { image, sample }
    }

    fn width(&self) -> usize {
        self.image.width as usize
    }

    fn height(&self) -> usize {
        self.image.height as usize
    }

    /// The interleaved samples of row `y`, and the pixel stride.
    fn span(&self, y: usize) -> (std::ops::Range<usize>, usize) {
        let stride = usize::from(self.image.channels);
        let row = self.width() * stride;
        (y * row..(y + 1) * row, stride)
    }

    /// Row `y` at 16 bits into `row`: 8-bit samples widened by 257, a
    /// missing alpha 65535.
    fn row_u16(&self, y: usize, row: &mut Vec<u16>) {
        row.clear();
        let Some(sample) = self.sample else {
            row.resize(self.width(), u16::MAX);
            return;
        };
        let (span, stride) = self.span(y);
        match &self.image.samples {
            Samples::U16(samples) => {
                row.extend(samples[span].iter().skip(sample).step_by(stride));
            }
            Samples::U8(samples) => row.extend(
                samples[span]
                    .iter()
                    .skip(sample)
                    .step_by(stride)
                    .map(|&v| u16::from(v) * 257),
            ),
        }
    }

    /// Row `y` at 8 bits into `row`, a missing alpha 255. Only for an 8-bit
    /// canvas, whose layers `Shape::check` keeps 8-bit.
    fn row_u8(&self, y: usize, row: &mut Vec<u8>) {
        row.clear();
        let Some(sample) = self.sample else {
            row.resize(self.width(), u8::MAX);
            return;
        };
        let (span, stride) = self.span(y);
        match &self.image.samples {
            Samples::U8(samples) => {
                row.extend(samples[span].iter().skip(sample).step_by(stride));
            }
            Samples::U16(_) => unreachable!("an 8-bit canvas refuses 16-bit layers"),
        }
    }

    /// Row `y` at canvas depth into `row`: [`ChannelView::row_u16`] on a
    /// 16-bit canvas, the 8-bit samples as they are on an 8-bit one.
    fn row_values(&self, y: usize, sixteen_bit: bool, row: &mut Vec<u16>) {
        if sixteen_bit {
            self.row_u16(y, row);
            return;
        }
        row.clear();
        let Some(sample) = self.sample else {
            row.resize(self.width(), u16::from(u8::MAX));
            return;
        };
        let (span, stride) = self.span(y);
        match &self.image.samples {
            Samples::U8(samples) => row.extend(
                samples[span]
                    .iter()
                    .skip(sample)
                    .step_by(stride)
                    .map(|&v| u16::from(v)),
            ),
            Samples::U16(_) => unreachable!("an 8-bit canvas refuses 16-bit layers"),
        }
    }

    /// Row `y` as big-endian bytes at canvas depth into `row`; `values` is
    /// scratch space for a 16-bit canvas.
    fn row_bytes(&self, y: usize, sixteen_bit: bool, values: &mut Vec<u16>, row: &mut Vec<u8>) {
        if sixteen_bit {
            self.row_u16(y, values);
            row.clear();
            row.extend(values.iter().flat_map(|v| v.to_be_bytes()));
        } else {
            self.row_u8(y, row);
        }
    }
}

/// The channel's data in PackBits (compression 1): the code, one byte count
/// per row, then the packed rows. 16-bit rows are packed as their
/// big-endian bytes.
fn encode_rle(view: &ChannelView, sixteen_bit: bool, widths: LengthWidth) -> io::Result<Vec<u8>> {
    let count_size = match widths {
        LengthWidth::Psd => 2,
        LengthWidth::Psb => 4,
    };
    let mut out = RLE.to_be_bytes().to_vec();
    let counts_start = out.len();
    out.resize(counts_start + view.height() * count_size, 0);
    let (mut values, mut row) = (Vec::new(), Vec::new());
    for y in 0..view.height() {
        view.row_bytes(y, sixteen_bit, &mut values, &mut row);
        let row_start = out.len();
        packbits(&row, &mut out);
        let packed = out.len() - row_start;
        let count = &mut out[counts_start + y * count_size..][..count_size];
        match widths {
            LengthWidth::Psd => {
                let packed = u16::try_from(packed).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "a row packs to {packed} bytes, past a PSD row count; \
                             the canvas needs PSB"
                        ),
                    )
                })?;
                count.copy_from_slice(&packed.to_be_bytes());
            }
            LengthWidth::Psb => {
                // A PSB row is at most 300,000 16-bit samples: it always fits.
                let packed = u32::try_from(packed).map_err(io::Error::other)?;
                count.copy_from_slice(&packed.to_be_bytes());
            }
        }
    }
    Ok(out)
}

/// The channel's data in ZIP with prediction (compression 3): the code,
/// then one zlib stream of every row, each delta-encoded on its 16-bit
/// samples (the first against 0) and written big-endian.
fn encode_zip_prediction(view: &ChannelView) -> io::Result<Vec<u8>> {
    let code = ZIP_PREDICTION.to_be_bytes().to_vec();
    // Level 1: about seven times faster than the default for ~13% larger
    // channels.
    let mut zlib = ZlibEncoder::new(code, Compression::fast());
    let (mut values, mut row) = (Vec::new(), Vec::new());
    for y in 0..view.height() {
        view.row_u16(y, &mut values);
        row.clear();
        let mut previous = 0u16;
        for &value in &values {
            row.extend_from_slice(&value.wrapping_sub(previous).to_be_bytes());
            previous = value;
        }
        zlib.write_all(&row)?;
    }
    zlib.finish()
}

/// Writes `image` uncompressed and planar at canvas depth to `file`, row by
/// row: R, G, B, then alpha when the canvas has it. With `shape.on_white`
/// each colour sample is blended against white with its pixel's alpha, and
/// the alpha is written as it is.
fn write_merged(file: &mut File, image: &FullImage, shape: Shape) -> io::Result<()> {
    let mut out = BufWriter::with_capacity(1 << 20, file);
    let alpha = ChannelView::new(image, ALPHA_ID);
    let (mut values, mut alphas, mut row) = (Vec::new(), Vec::new(), Vec::new());
    for &id in shape.merged_ids() {
        let view = ChannelView::new(image, id);
        let blend = shape.on_white && id != ALPHA_ID;
        for y in 0..view.height() {
            if blend {
                view.row_values(y, shape.sixteen_bit, &mut values);
                alpha.row_values(y, shape.sixteen_bit, &mut alphas);
                row_on_white(&values, &alphas, shape.sixteen_bit, &mut row);
            } else {
                view.row_bytes(y, shape.sixteen_bit, &mut values, &mut row);
            }
            out.write_all(&row)?;
        }
    }
    out.flush()
}

/// The colour samples `values` of a row, blended against white with the
/// row's `alphas`, as big-endian bytes at canvas depth into `row`.
fn row_on_white(values: &[u16], alphas: &[u16], sixteen_bit: bool, row: &mut Vec<u8>) {
    row.clear();
    let pairs = values.iter().zip(alphas);
    if sixteen_bit {
        row.extend(pairs.flat_map(|(&c, &a)| on_white(c, a, u16::MAX).to_be_bytes()));
    } else {
        // At most 255 on an 8-bit canvas, so the cast keeps every value.
        row.extend(pairs.map(|(&c, &a)| on_white(c, a, u16::from(u8::MAX)) as u8));
    }
}

/// The colour sample `c` blended against white with the alpha `a`, both at
/// most `max`, rounded to nearest: `(c * a + max * (max - a) + max / 2) /
/// max` (spec §6.2). An opaque `a = max` gives `c`, a transparent `a = 0`
/// gives `max`, white. The arithmetic is in u64, wide enough at 16 bits.
fn on_white(c: u16, a: u16, max: u16) -> u16 {
    let (c, a, max) = (u64::from(c), u64::from(a), u64::from(max));
    // The numerator is at most max * max + max / 2, so the result is at
    // most `max`.
    ((c * a + max * (max - a) + max / 2) / max) as u16
}

/// The two spill files of a finished encoder, closed. Both are removed when
/// it is dropped.
#[derive(Debug)]
pub(crate) struct Spill {
    layers: SpillFile,
    merged: SpillFile,
    /// The widths the channels were encoded with.
    widths: LengthWidth,
    /// `.merged`'s colour is blended against white (a PSD or PSB canvas
    /// with alpha).
    on_white: bool,
}

impl Spill {
    /// `<final>.partial.layers`: every layer's channel data, in record order
    /// and without gaps.
    pub(crate) fn layers_path(&self) -> &Path {
        &self.layers.path
    }

    /// `<final>.partial.merged`: the first layer, uncompressed and planar at
    /// canvas depth (R, G, B, then alpha when the canvas has it), its colour
    /// blended against white when [`Spill::merged_on_white`] says so.
    pub(crate) fn merged_path(&self) -> &Path {
        &self.merged.path
    }

    /// True when `.merged`'s colour is blended against white, as a PSD or
    /// PSB with alpha needs it; false when it is straight, as a TIFF and
    /// every canvas without alpha need it.
    pub(crate) fn merged_on_white(&self) -> bool {
        self.on_white
    }

    /// The widths the encoder used: PackBits row counts inside the channel
    /// data depend on them, so a container must use the same.
    pub(crate) fn widths(&self) -> LengthWidth {
        self.widths
    }
}

/// A spill file this encoder created. Dropping it closes it, then removes it.
#[derive(Debug)]
struct SpillFile {
    path: PathBuf,
    /// `None` once closed.
    file: Option<File>,
}

impl SpillFile {
    /// Creates `path`, which must not exist: a leftover from an earlier run
    /// is not ours to reuse, or to remove (`AlreadyExists`, whatever is at
    /// the name: [`create_new`]).
    fn create(path: PathBuf) -> io::Result<Self> {
        let file = create_new(&path)?;
        Ok(Self {
            path,
            file: Some(file),
        })
    }

    fn file(&mut self) -> &mut File {
        self.file
            .as_mut()
            .expect("the file is open until the encoder finishes")
    }
}

impl Drop for SpillFile {
    fn drop(&mut self) {
        // Close the file, then remove it, as `PartialFile` does: the space is
        // freed as soon as the name is gone.
        drop(self.file.take());
        let _ = fs::remove_file(&self.path);
    }
}

/// Appends `row` packed with PackBits to `out`.
///
/// A run of 2 to 128 equal bytes becomes the header `1 - n` (as i8) and the
/// byte; anything else goes in literals of 1 to 128 bytes, header `n - 1`. A
/// literal ends only where a run of 3 begins: a pair costs no more inside a
/// literal than as a run that splits it. The no-op header 0x80 is never
/// written.
fn packbits(row: &[u8], out: &mut Vec<u8>) {
    let mut i = 0;
    while i < row.len() {
        let run = row[i..]
            .iter()
            .take(128)
            .take_while(|&&b| b == row[i])
            .count();
        if run >= 2 {
            out.push((1 - run as isize) as u8);
            out.push(row[i]);
            i += run;
            continue;
        }
        let start = i;
        while i < row.len() && i - start < 128 {
            if i + 2 < row.len() && row[i] == row[i + 1] && row[i] == row[i + 2] {
                break;
            }
            i += 1;
        }
        out.push((i - start - 1) as u8);
        out.extend_from_slice(&row[start..i]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::ImageSource;
    use std::io::Read;

    fn canvas(width: u32, height: u32, sixteen_bit: bool, has_alpha: bool) -> CanvasProfile {
        CanvasProfile {
            width,
            height,
            sixteen_bit,
            has_alpha,
            icc: None,
            container: Container::Psd,
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

    /// An 8-bit image whose sample `c` at `(x, y)` is `f(x, y, c)`.
    fn image8(width: u32, height: u32, channels: u8, f: impl Fn(u32, u32, u8) -> u8) -> FullImage {
        let mut samples = Vec::new();
        for y in 0..height {
            for x in 0..width {
                for c in 0..channels {
                    samples.push(f(x, y, c));
                }
            }
        }
        image(width, height, channels, Samples::U8(samples))
    }

    /// A 16-bit image whose sample `c` at `(x, y)` is `f(x, y, c)`.
    fn image16(
        width: u32,
        height: u32,
        channels: u8,
        f: impl Fn(u32, u32, u8) -> u16,
    ) -> FullImage {
        let mut samples = Vec::new();
        for y in 0..height {
            for x in 0..width {
                for c in 0..channels {
                    samples.push(f(x, y, c));
                }
            }
        }
        image(width, height, channels, Samples::U16(samples))
    }

    /// One plane of `image`, as u16 values (8-bit samples as they are).
    fn plane(image: &FullImage, c: usize) -> Vec<u16> {
        let n = usize::from(image.channels);
        match &image.samples {
            Samples::U8(s) => s.iter().skip(c).step_by(n).map(|&v| u16::from(v)).collect(),
            Samples::U16(s) => s.iter().skip(c).step_by(n).copied().collect(),
        }
    }

    /// A deterministic xorshift generator, so the random rows are the same
    /// on every run.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// The test-only PackBits decoder: the inverse of `packbits`.
    fn unpackbits(packed: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < packed.len() {
            let header = packed[i] as i8;
            i += 1;
            if header >= 0 {
                let n = header as usize + 1;
                out.extend_from_slice(&packed[i..i + n]);
                i += n;
            } else if header != -128 {
                let n = 1 - isize::from(header);
                out.extend(std::iter::repeat_n(packed[i], n as usize));
                i += 1;
            }
        }
        out
    }

    fn read_file(path: &Path) -> Vec<u8> {
        let mut bytes = Vec::new();
        File::open(path).unwrap().read_to_end(&mut bytes).unwrap();
        bytes
    }

    /// The bytes of one channel record in `.layers`.
    fn channel_bytes<'a>(layers: &'a [u8], record: &ChannelRecord) -> &'a [u8] {
        &layers[record.offset as usize..(record.offset + record.len) as usize]
    }

    /// Decodes one channel's data back to its samples, as u16 values.
    fn decode_channel(
        data: &[u8],
        width: usize,
        height: usize,
        sixteen_bit: bool,
        widths: LengthWidth,
    ) -> Vec<u16> {
        let code = u16::from_be_bytes([data[0], data[1]]);
        let body = &data[2..];
        let row_bytes = if sixteen_bit { 2 * width } else { width };
        let bytes = match code {
            RLE => {
                let count_size = match widths {
                    LengthWidth::Psd => 2,
                    LengthWidth::Psb => 4,
                };
                let (counts, mut rows) = body.split_at(height * count_size);
                let mut bytes = Vec::new();
                for count in counts.chunks(count_size) {
                    let count = count.iter().fold(0usize, |n, &b| n << 8 | usize::from(b));
                    let (row, rest) = rows.split_at(count);
                    let row = unpackbits(row);
                    assert_eq!(row.len(), row_bytes, "a row unpacks to its width");
                    bytes.extend(row);
                    rows = rest;
                }
                assert!(rows.is_empty(), "the row counts cover the channel");
                bytes
            }
            ZIP_PREDICTION => {
                let mut bytes = Vec::new();
                flate2::read::ZlibDecoder::new(body)
                    .read_to_end(&mut bytes)
                    .unwrap();
                assert_eq!(bytes.len(), row_bytes * height);
                for row in bytes.chunks_mut(row_bytes) {
                    let mut previous = 0u16;
                    for sample in row.chunks_mut(2) {
                        let value =
                            u16::from_be_bytes([sample[0], sample[1]]).wrapping_add(previous);
                        sample.copy_from_slice(&value.to_be_bytes());
                        previous = value;
                    }
                }
                bytes
            }
            other => panic!("unexpected compression {other}"),
        };
        if sixteen_bit {
            bytes
                .chunks(2)
                .map(|b| u16::from_be_bytes([b[0], b[1]]))
                .collect()
        } else {
            bytes.into_iter().map(u16::from).collect()
        }
    }

    /// Encodes `images` as layers of `canvas` for the output `final_path`,
    /// then returns the records, the spill and the bytes of both spill files.
    fn encode(
        final_path: &Path,
        canvas: &CanvasProfile,
        widths: LengthWidth,
        opts: WriteOptions,
        images: Vec<FullImage>,
    ) -> (Vec<LayerRecord>, Spill, Vec<u8>, Vec<u8>) {
        let mut encoder = LayerEncoder::new(final_path, canvas, widths, opts).unwrap();
        for (i, image) in images.into_iter().enumerate() {
            encoder.add_layer(&format!("layer {i}"), image).unwrap();
        }
        let (records, spill) = encoder.finish().unwrap();
        let layers = read_file(spill.layers_path());
        let merged = read_file(spill.merged_path());
        (records, spill, layers, merged)
    }

    const ZIP: WriteOptions = WriteOptions {
        compression16: Compression16::Zip,
    };
    const RLE16: WriteOptions = WriteOptions {
        compression16: Compression16::Rle,
    };

    #[test]
    fn packbits_round_trips_and_matches_reference() {
        // The sample of the TIFF 6.0 specification, section 9.
        let sample = [
            0xAA, 0xAA, 0xAA, 0x80, 0x00, 0x2A, 0xAA, 0xAA, 0xAA, 0xAA, 0x80, 0x00, 0x2A, 0x22,
            0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA,
        ];
        let reference = [
            0xFE, 0xAA, 0x02, 0x80, 0x00, 0x2A, 0xFD, 0xAA, 0x03, 0x80, 0x00, 0x2A, 0x22, 0xF7,
            0xAA,
        ];
        let mut packed = Vec::new();
        packbits(&sample, &mut packed);
        assert_eq!(packed, reference);
        assert_eq!(unpackbits(&reference), sample);
        assert_eq!(unpackbits(&packed), sample);

        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let lengths = [0, 1, 2, 3, 127, 128, 129, 130, 255, 256, 257, 1000, 4099];
        for (round, &len) in lengths.iter().cycle().take(lengths.len() * 20).enumerate() {
            // Few distinct values and long stretches, so runs of every
            // length meet literals of every length.
            let mut row = Vec::with_capacity(len);
            while row.len() < len {
                let value = rng.below(if round % 2 == 0 { 3 } else { 256 }) as u8;
                let run = 1 + rng.below(if round % 3 == 0 { 300 } else { 4 }) as usize;
                row.extend(std::iter::repeat_n(value, run.min(len - row.len())));
            }
            let mut packed = vec![0x55];
            packbits(&row, &mut packed);
            let packed = &packed[1..];
            assert_eq!(unpackbits(packed), row, "round {round}, length {len}");
            assert!(packed.len() <= len + len.div_ceil(128), "round {round}");
            assert!(headers_skip_0x80(packed), "round {round}");
        }
    }

    /// True when no header byte of `packed` is the no-op 0x80 (data bytes
    /// may be 0x80).
    fn headers_skip_0x80(packed: &[u8]) -> bool {
        let mut i = 0;
        while i < packed.len() {
            let header = packed[i] as i8;
            if header == -128 {
                return false;
            }
            i += 1 + if header >= 0 { header as usize + 1 } else { 1 };
        }
        true
    }

    #[test]
    fn zip_prediction_round_trips_sixteen_bit() {
        let dir = tempfile::tempdir().unwrap();
        let mut rng = Rng(7);
        let noise: Vec<u16> = (0..9 * 5 * 3).map(|_| rng.next() as u16).collect();
        let first = image16(9, 5, 3, |x, y, c| {
            noise[((y * 9 + x) * 3 + u32::from(c)) as usize]
        });
        let second = image16(9, 5, 3, |x, y, c| {
            (x * 7000 + y * 300 + u32::from(c)) as u16 ^ 0x8000
        });
        let expected = [first.clone(), second.clone()];
        let (records, _spill, layers, _) = encode(
            &dir.path().join("out1.psd"),
            &canvas(9, 5, true, false),
            LengthWidth::Psd,
            ZIP,
            vec![first, second],
        );

        assert_eq!(records.len(), 2);
        for (record, image) in records.iter().zip(&expected) {
            let ids: Vec<i16> = record.channels.iter().map(|c| c.id).collect();
            assert_eq!(ids, [0, 1, 2]);
            for channel in &record.channels {
                assert_eq!(channel.compression, ZIP_PREDICTION);
                let data = channel_bytes(&layers, channel);
                assert_eq!(data[..2], ZIP_PREDICTION.to_be_bytes());
                let values = decode_channel(data, 9, 5, true, LengthWidth::Psd);
                assert_eq!(values, plane(image, channel.id as usize));
            }
        }
        assert_eq!(records[0].name, "layer 0");
        assert_eq!(records[1].name, "layer 1");
        // The channels follow each other in record order, without gaps.
        let mut offset = 0;
        for channel in records.iter().flat_map(|r| &r.channels) {
            assert_eq!(channel.offset, offset);
            offset += channel.len;
        }
        assert_eq!(offset, layers.len() as u64);
    }

    #[test]
    fn zip_prediction_matches_hand_computed_deltas() {
        // Rows [1, 0] and [5, 5]: each row restarts from 0, and 0 - 1 wraps
        // to 0xFFFF.
        let layer = image16(2, 2, 3, |x, y, c| match (c, y, x) {
            (0, 0, 0) => 1,
            (0, 0, 1) => 0,
            (0, 1, _) => 5,
            _ => 0,
        });
        let data = encode_zip_prediction(&ChannelView::new(&layer, 0)).unwrap();
        assert_eq!(data[..2], [0x00, 0x03]);
        let mut deltas = Vec::new();
        flate2::read::ZlibDecoder::new(&data[2..])
            .read_to_end(&mut deltas)
            .unwrap();
        assert_eq!(deltas, [0x00, 0x01, 0xFF, 0xFF, 0x00, 0x05, 0x00, 0x00]);
    }

    #[test]
    fn sixteen_bit_rle_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let layer = image16(40, 3, 3, |x, _, c| {
            if x < 20 {
                0x1234
            } else {
                x as u16 * 999 + u16::from(c)
            }
        });
        let (records, _spill, layers, _) = encode(
            &dir.path().join("out2.psd"),
            &canvas(40, 3, true, false),
            LengthWidth::Psd,
            RLE16,
            vec![layer.clone()],
        );
        for channel in &records[0].channels {
            assert_eq!(channel.compression, RLE);
            let values = decode_channel(
                channel_bytes(&layers, channel),
                40,
                3,
                true,
                LengthWidth::Psd,
            );
            assert_eq!(values, plane(&layer, channel.id as usize));
        }
    }

    #[test]
    fn eight_bit_layers_always_use_packbits() {
        let dir = tempfile::tempdir().unwrap();
        let layer = image8(6, 4, 3, |x, y, c| (x * 40 + y + u32::from(c)) as u8);
        let (records, _spill, layers, _) = encode(
            &dir.path().join("out3.psd"),
            &canvas(6, 4, false, false),
            LengthWidth::Psd,
            ZIP,
            vec![layer.clone()],
        );
        for channel in &records[0].channels {
            assert_eq!(channel.compression, RLE);
            let values = decode_channel(
                channel_bytes(&layers, channel),
                6,
                4,
                false,
                LengthWidth::Psd,
            );
            assert_eq!(values, plane(&layer, channel.id as usize));
        }
    }

    #[test]
    fn eight_bit_layer_widened_into_sixteen_bit_canvas_with_257() {
        let dir = tempfile::tempdir().unwrap();
        let wide = image16(5, 3, 3, |x, _, _| x as u16 * 1000);
        let narrow = image8(5, 3, 3, |x, y, c| {
            [0, 1, 128, 254, 255][x as usize] ^ (y as u8 + c)
        });
        let expected: Vec<Vec<u16>> = (0..3)
            .map(|c| plane(&narrow, c).into_iter().map(|v| v * 257).collect())
            .collect();
        for opts in [ZIP, RLE16] {
            let (records, _spill, layers, _) = encode(
                &dir.path().join("out4.psd"),
                &canvas(5, 3, true, false),
                LengthWidth::Psd,
                opts,
                vec![wide.clone(), narrow.clone()],
            );
            for channel in &records[1].channels {
                let values = decode_channel(
                    channel_bytes(&layers, channel),
                    5,
                    3,
                    true,
                    LengthWidth::Psd,
                );
                assert_eq!(values, expected[channel.id as usize], "{opts:?}");
            }
        }

        // An 8-bit first layer reaches the merged spill widened too.
        let (_, _spill, _, merged) = encode(
            &dir.path().join("out5.psd"),
            &canvas(5, 3, true, false),
            LengthWidth::Psd,
            ZIP,
            vec![narrow.clone(), wide],
        );
        let merged: Vec<u16> = merged
            .chunks(2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(merged, expected.concat());
    }

    #[test]
    fn missing_alpha_becomes_opaque() {
        let dir = tempfile::tempdir().unwrap();
        // 8-bit canvas: an RGBA layer, then an RGB one.
        let rgba = image8(4, 2, 4, |x, _, c| if c == 3 { x as u8 * 60 } else { 9 });
        let rgb = image8(4, 2, 3, |_, y, c| y as u8 + c);
        let (records, _spill, layers, _) = encode(
            &dir.path().join("out6.psd"),
            &canvas(4, 2, false, true),
            LengthWidth::Psd,
            ZIP,
            vec![rgba.clone(), rgb.clone()],
        );
        for record in &records {
            let ids: Vec<i16> = record.channels.iter().map(|c| c.id).collect();
            assert_eq!(ids, [ALPHA_ID, 0, 1, 2], "alpha comes first");
        }
        let alpha = |record: &LayerRecord| {
            decode_channel(
                channel_bytes(&layers, &record.channels[0]),
                4,
                2,
                false,
                LengthWidth::Psd,
            )
        };
        assert_eq!(alpha(&records[0]), plane(&rgba, 3));
        assert_eq!(alpha(&records[1]), vec![255; 8]);
        let red = decode_channel(
            channel_bytes(&layers, &records[1].channels[1]),
            4,
            2,
            false,
            LengthWidth::Psd,
        );
        assert_eq!(red, plane(&rgb, 0));

        // 16-bit canvas: the opaque alpha is 65535, in the layer and in the
        // merged spill, after R, G and B.
        let rgb8 = image8(4, 2, 3, |x, _, _| x as u8);
        let rgba16 = image16(4, 2, 4, |_, _, c| u16::from(c) * 100);
        let (records, _spill, layers, merged) = encode(
            &dir.path().join("out7.psd"),
            &canvas(4, 2, true, true),
            LengthWidth::Psd,
            ZIP,
            vec![rgb8, rgba16],
        );
        let alpha = decode_channel(
            channel_bytes(&layers, &records[0].channels[0]),
            4,
            2,
            true,
            LengthWidth::Psd,
        );
        assert_eq!(alpha, vec![65535; 8]);
        assert_eq!(merged.len(), 4 * 2 * 4 * 2);
        assert!(
            merged[3 * 16..].iter().all(|&b| b == 0xFF),
            "the merged alpha is opaque"
        );
    }

    #[test]
    fn first_layer_goes_to_merged_spill_raw_planar() {
        let dir = tempfile::tempdir().unwrap();
        let first = image8(3, 2, 3, |x, y, c| (10 * c + 3 * y as u8 + x as u8) + 1);
        let second = image8(3, 2, 3, |_, _, _| 200);
        let (_, _spill, _, merged) = encode(
            &dir.path().join("out8.psd"),
            &canvas(3, 2, false, false),
            LengthWidth::Psd,
            ZIP,
            vec![first, second],
        );
        // R plane, then G, then B; rows top to bottom.
        let expected: Vec<u8> = (0..3u8)
            .flat_map(|c| (0..2u8).flat_map(move |y| (0..3u8).map(move |x| 10 * c + 3 * y + x + 1)))
            .collect();
        assert_eq!(merged, expected);

        // 16-bit samples are big-endian.
        let first = image16(2, 1, 3, |x, _, c| 0x0102 * (u16::from(c) + 1) + x as u16);
        let (_, _spill, _, merged) = encode(
            &dir.path().join("out9.psd"),
            &canvas(2, 1, true, false),
            LengthWidth::Psb,
            RLE16,
            vec![first],
        );
        assert_eq!(
            merged,
            [
                0x01, 0x02, 0x01, 0x03, 0x02, 0x04, 0x02, 0x05, 0x03, 0x06, 0x03, 0x07
            ]
        );
    }

    /// The merged spill of 16-bit samples, as values.
    fn merged_u16(merged: &[u8]) -> Vec<u16> {
        merged
            .chunks(2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
            .collect()
    }

    #[test]
    fn merged_spill_of_a_psd_with_alpha_is_blended_against_white() {
        // Spec §6.2: each colour sample is `(c * a + max * (max - a) +
        // max / 2) / max` with its pixel's alpha `a`; the alpha is stored
        // unchanged, last. One pixel per alpha, each the same colour.
        let dir = tempfile::tempdir().unwrap();
        let alphas8 = [0u8, 255, 128, 64];
        let first8 = image8(4, 1, 4, |x, _, c| match c {
            0 => 100,
            1 => 1,
            2 => 200,
            _ => alphas8[x as usize],
        });
        let expected8: [[u8; 4]; 4] = [
            // (100 * 128 + 255 * 127 + 127) / 255 = 177.
            [255, 100, 177, 216],
            // 1 at alpha 128 is 127.502 on white: rounded to 128, not 127.
            [255, 1, 128, 191],
            [255, 200, 227, 241],
            alphas8,
        ];
        let alphas16 = [0u16, 65535, 32768, 1];
        let first16 = image16(4, 1, 4, |x, _, c| match c {
            0 => 1000,
            1 => 1,
            2 => 65535,
            _ => alphas16[x as usize],
        });
        let expected16: [[u16; 4]; 4] = [
            // (1000 * 32768 + 65535 * 32767 + 32767) / 65535 = 33267.
            [65535, 1000, 33267, 65534],
            // 1 at alpha 32768 is 32767.500008 on white: rounded up.
            [65535, 1, 32768, 65534],
            // Full white stays white at every alpha.
            [65535; 4],
            alphas16,
        ];
        for (container, widths) in [
            (Container::Psd, LengthWidth::Psd),
            (Container::Psb, LengthWidth::Psb),
        ] {
            let mut canvas8 = canvas(4, 1, false, true);
            canvas8.container = container;
            let (_, spill, _, merged) = encode(
                &dir.path().join(format!("white8.{container:?}")),
                &canvas8,
                widths,
                ZIP,
                vec![first8.clone(), image8(4, 1, 3, |_, _, _| 7)],
            );
            assert!(spill.merged_on_white());
            assert_eq!(merged, expected8.concat(), "8-bit {container:?}");

            let mut canvas16 = canvas(4, 1, true, true);
            canvas16.container = container;
            let (_, spill, _, merged) = encode(
                &dir.path().join(format!("white16.{container:?}")),
                &canvas16,
                widths,
                RLE16,
                vec![first16.clone()],
            );
            assert!(spill.merged_on_white());
            assert_eq!(
                merged_u16(&merged),
                expected16.concat(),
                "16-bit {container:?}"
            );
        }
    }

    #[test]
    fn merged_spill_keeps_straight_colour_in_a_tiff_and_where_opaque() {
        let dir = tempfile::tempdir().unwrap();
        // A TIFF's image is unassociated alpha (spec §6.3): the colour is
        // the layer's own, whatever its alpha.
        let alphas = [0u8, 255, 128, 64];
        let first = image8(4, 1, 4, |x, _, c| match c {
            0 => 100,
            1 => 1,
            2 => 200,
            _ => alphas[x as usize],
        });
        for (container, widths) in [
            (Container::Tiff, LengthWidth::Psd),
            (Container::BigTiff, LengthWidth::Psb),
        ] {
            let mut tiff = canvas(4, 1, false, true);
            tiff.container = container;
            let (_, spill, _, merged) = encode(
                &dir.path().join(format!("straight.{container:?}")),
                &tiff,
                widths,
                ZIP,
                vec![first.clone()],
            );
            assert!(!spill.merged_on_white());
            assert_eq!(
                merged,
                [[100; 4], [1; 4], [200; 4], alphas].concat(),
                "{container:?}"
            );
        }

        // An opaque first layer on a PSD canvas with alpha keeps its
        // colour: on white at full alpha is the colour itself.
        let opaque = image16(3, 1, 3, |x, _, c| x as u16 * 30_000 + u16::from(c));
        let (_, spill, _, merged) = encode(
            &dir.path().join("opaque.psd"),
            &canvas(3, 1, true, true),
            LengthWidth::Psd,
            ZIP,
            vec![opaque.clone()],
        );
        let expected: Vec<u16> = (0..3)
            .flat_map(|c| plane(&opaque, c))
            .chain([65535; 3])
            .collect();
        assert!(spill.merged_on_white());
        assert_eq!(merged_u16(&merged), expected);

        // The same at 8 bits, where the missing alpha is filled on another path.
        let opaque8 = image8(3, 1, 3, |x, _, c| x as u8 * 100 + c);
        let (_, spill, _, merged) = encode(
            &dir.path().join("opaque8.psd"),
            &canvas(3, 1, false, true),
            LengthWidth::Psd,
            ZIP,
            vec![opaque8],
        );
        assert!(spill.merged_on_white());
        assert_eq!(
            merged,
            [[0, 100, 200], [1, 101, 201], [2, 102, 202], [255; 3]].concat()
        );
    }

    #[test]
    fn spill_files_removed_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("IMG_0001_3.psd");
        let layers = dir.path().join("IMG_0001_3.psd.partial.layers");
        let merged = dir.path().join("IMG_0001_3.psd.partial.merged");
        let canvas = canvas(2, 2, false, false);

        // Dropped without finishing.
        let mut encoder = LayerEncoder::new(&final_path, &canvas, LengthWidth::Psd, ZIP).unwrap();
        encoder
            .add_layer("a", image8(2, 2, 3, |_, _, _| 1))
            .unwrap();
        assert!(layers.exists() && merged.exists());
        drop(encoder);
        assert!(!layers.exists() && !merged.exists());

        // Finished: the spill holds them until it is dropped.
        let mut encoder = LayerEncoder::new(&final_path, &canvas, LengthWidth::Psd, ZIP).unwrap();
        encoder
            .add_layer("a", image8(2, 2, 3, |_, _, _| 1))
            .unwrap();
        let (_, spill) = encoder.finish().unwrap();
        assert_eq!(spill.layers_path(), layers);
        assert_eq!(spill.merged_path(), merged);
        assert!(layers.exists() && merged.exists());
        drop(spill);
        assert!(!layers.exists() && !merged.exists());

        // A spill file left over from an earlier run is not ours: `new`
        // refuses, and leaves it as it was.
        for leftover in [&layers, &merged] {
            fs::write(leftover, b"earlier run").unwrap();
            let error = LayerEncoder::new(&final_path, &canvas, LengthWidth::Psd, ZIP)
                .err()
                .expect("a leftover spill file is refused");
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            assert_eq!(fs::read(leftover).unwrap(), b"earlier run");
            fs::remove_file(leftover).unwrap();
            assert!(!layers.exists() && !merged.exists(), "nothing else is left");
        }
    }

    #[test]
    fn psb_widths_use_u32_row_counts() {
        let dir = tempfile::tempdir().unwrap();
        let layer = image8(300, 4, 3, |x, y, c| if x < 150 { c } else { (x * y) as u8 });
        let canvas = canvas(300, 4, false, false);
        let (psd, _psd_spill, psd_layers, _) = encode(
            &dir.path().join("out10.psd"),
            &canvas,
            LengthWidth::Psd,
            ZIP,
            vec![layer.clone()],
        );
        let (psb, _psb_spill, psb_layers, _) = encode(
            &dir.path().join("out11.psd"),
            &canvas,
            LengthWidth::Psb,
            ZIP,
            vec![layer.clone()],
        );
        for (d, b) in psd[0].channels.iter().zip(&psb[0].channels) {
            // Two more bytes per row count, nothing else.
            assert_eq!(b.len, d.len + 2 * 4);
            let d_data = channel_bytes(&psd_layers, d);
            let b_data = channel_bytes(&psb_layers, b);
            for y in 0..4 {
                let short = u16::from_be_bytes([d_data[2 + 2 * y], d_data[3 + 2 * y]]);
                let long = u32::from_be_bytes(b_data[2 + 4 * y..6 + 4 * y].try_into().unwrap());
                assert_eq!(u32::from(short), long);
            }
            assert_eq!(
                d_data[2 + 2 * 4..],
                b_data[2 + 4 * 4..],
                "the same packed rows"
            );
            let values = decode_channel(b_data, 300, 4, false, LengthWidth::Psb);
            assert_eq!(values, plane(&layer, b.id as usize));
        }
    }

    #[test]
    fn psd_row_count_too_large_is_an_error() {
        // 16-bit noise 33,000 pixels wide packs to more than 65,535 bytes a
        // row, which a PSD row count cannot hold.
        let dir = tempfile::tempdir().unwrap();
        let mut rng = Rng(11);
        let noise: Vec<u16> = (0..33_000 * 3).map(|_| rng.next() as u16).collect();
        let layer = image(33_000, 1, 3, Samples::U16(noise));
        let canvas = canvas(33_000, 1, true, false);
        let mut encoder = LayerEncoder::new(
            &dir.path().join("wide.psd"),
            &canvas,
            LengthWidth::Psd,
            RLE16,
        )
        .unwrap();
        let error = encoder.add_layer("wide", layer.clone()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        // The encoder is spent after a failure past validation, and it
        // cannot finish a stack that misses a layer: the spill is removed.
        assert!(encoder.add_layer("again", layer.clone()).is_err());
        assert!(encoder.finish().is_err());
        assert!(!dir.path().join("wide.psd.partial.layers").exists());
        assert!(!dir.path().join("wide.psd.partial.merged").exists());

        // A PSB holds it.
        let (records, _spill, layers, _) = encode(
            &dir.path().join("out12.psd"),
            &canvas,
            LengthWidth::Psb,
            RLE16,
            vec![layer.clone()],
        );
        let values = decode_channel(
            channel_bytes(&layers, &records[0].channels[2]),
            33_000,
            1,
            true,
            LengthWidth::Psb,
        );
        assert_eq!(values, plane(&layer, 2));
    }

    #[test]
    fn layer_that_does_not_fit_the_canvas_is_invalid_input() {
        let dir = tempfile::tempdir().unwrap();
        let mut encoder = LayerEncoder::new(
            &dir.path().join("out.psd"),
            &canvas(4, 3, false, false),
            LengthWidth::Psd,
            ZIP,
        )
        .unwrap();
        let misfits = [
            image8(3, 4, 3, |_, _, _| 0),
            image8(4, 3, 4, |_, _, _| 0),
            image16(4, 3, 3, |_, _, _| 0),
            image(4, 3, 3, Samples::U8(vec![0; 35])),
            image(4, 3, 2, Samples::U8(vec![0; 24])),
        ];
        for misfit in misfits {
            let error = encoder.add_layer("misfit", misfit).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
        // Nothing was written, and the encoder still takes a layer that fits.
        encoder
            .add_layer("fits", image8(4, 3, 3, |_, _, _| 5))
            .unwrap();
        let (records, spill) = encoder.finish().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(read_file(spill.merged_path()), vec![5; 36]);
    }
}
