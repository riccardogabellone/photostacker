//! The Photoshop checkpoint set (spec §12.5), run by `just ps-samples`.
//!
//! Usage: `ps_samples <folder ending in dist/ps-check> [--bigtiff]`
//!
//! 1. The folder is cleared: only the regular files directly in it are
//!    removed, and only when its path ends in `dist/ps-check` and no
//!    component of it is a link (see [`clear`]). It is created when missing.
//!    Its `sources` folder is cleared the same way (see [`clear_sources`]).
//! 2. One stack per sample of [`SAMPLES`] is written with [`write_stack`],
//!    from testkit scenes built in memory. The layers are one scene exposed
//!    at 0, −1 and +1 EV (the first is the top layer), named
//!    `<sample>`, `<sample>-2`, `<sample>-3` with their EV, so the file is
//!    `<sample>_<layers>.<ext>`. 16-bit layers are linear light, tagged
//!    with Photostacker's linear profile, so they look like the 8-bit ones.
//!    Two samples are made from files instead, written to `sources` (where
//!    they stay, to be opened beside the stacks) and read through
//!    `decode::probe` and `decode::full` as the writers read files:
//!    - the gray-source sample's layers are 8-bit gray PNGs with a Gray ICC
//!      profile;
//!    - the Adobe RGB sample's layers are JPEGs with no ICC profile, whose
//!      EXIF marks them as DCF Adobe RGB (ColorSpace 0xFFFF and the Interop
//!      index `R03`), so they take the generated Adobe RGB profile through
//!      `ColourHint::from_exif`. Their EXIF is read from the files, as the
//!      writers read it, and saturated patches and a gray ramp over the
//!      scene make a colour mix-up visible.
//! 3. With `--bigtiff`, a 2-layer 15,000 × 15,000 16-bit TIFF is added: its
//!    estimate is past 4 GB, so it is written as a BigTIFF (about 1.4 GB of
//!    pixels per layer in memory, and a slow write).
//! 4. When `PHOTOSTACKER_FIXTURES` is set, the public Empa D200 NEF ×5
//!    bracket is read in place, analysed and written once as PSD and once as
//!    TIFF, through `analyze` and `write_all`.
//! 5. Every file is read back (testkit's PSD reader, or the `tiff` crate
//!    plus the reader's layer-block parser) and checked: its header, layer
//!    names, embedded ICC profile (and the name of a profile we generate)
//!    and, in a TIFF, its ExtraSamples. One line
//!    per file gives its path, container, layers and size (`?` when the size
//!    cannot be read). Any failure makes the exit status non-zero.
//!
//! The results of opening the files in Photoshop go in
//! `docs/photoshop-checks.md`.

use std::fs::{self, File};
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, anyhow, bail, ensure};
use image::codecs::png::PngEncoder;
use image::{DynamicImage, ExtendedColorType, ImageDecoder, ImageEncoder, ImageReader, RgbImage};
use photostacker_core::decode::{self, ColourHint, FullImage, ImageSource, Probe, Samples};
use photostacker_core::exif::{BracketInfo, BracketMode, ExifData, read_exif};
use photostacker_core::group::Container;
use photostacker_core::icc::{adobe_rgb_profile, rgb_profile};
use photostacker_core::model::{FileKind, OutputFormat, RawTone};
use photostacker_core::write::{Compression16, layer_name};
use photostacker_core::{
    JobConfig, StackInput, WriteOptions, WriteOutcome, analyze, write_all, write_stack,
};
use photostacker_testkit::psd_reader::{self, Layer};
use photostacker_testkit::{
    MakerNoteSpec, NoteStyle, TiffSpec, Value, expose, icc_stub, jpeg_with_exif, scene,
};
use tiff::decoder::{Decoder, DecodingResult, Limits};
use tiff::tags::Tag;

const USAGE: &str = "usage: ps_samples <folder ending in dist/ps-check> [--bigtiff]";
/// The only folder the example clears and writes into.
const CHECK_FOLDER: &str = "dist/ps-check";
/// The folder, in [`CHECK_FOLDER`], of the files the file-based samples are
/// made from.
const SOURCES_FOLDER: &str = "sources";
/// The real bracket, under `PHOTOSTACKER_FIXTURES`.
const REAL_BRACKET: &str = "brackets/Nikon/Empa_MarketMires2_D200_AEB5_NEF";
const REAL_BRACKET_LAYERS: usize = 5;

/// The scene every sample is made of.
const SEED: u64 = 12;
const SIZE: (u32, u32) = (1536, 1024);
/// 16 px past the PSD limit of 30,000 on a side, so the file is a PSB.
const WIDE: (u32, u32) = (30_016, 64);
/// 15,000² px × 3 channels × 2 bytes × (2 layers + 1) = 4.05 GB, past the
/// 4 GB limit of a classic TIFF.
const BIG: (u32, u32) = (15_000, 15_000);
/// The exposures of a 3-layer sample, top layer first, in camera order.
const EVS_3: &[f32] = &[0.0, -1.0, 1.0];
const EVS_2: &[f32] = &[0.0, -1.0];

/// The saturated patches of the Adobe RGB sample, left to right: red,
/// green, blue, cyan, magenta and yellow. Adobe RGB's greens and cyans reach
/// well past sRGB's, so a document read as sRGB gives them other colours
/// than its sources: visibly on a wide-gamut display, and in Photoshop's
/// Lab readout on any.
const PATCHES: [[u8; 3]; 6] = [
    [255, 0, 0],
    [0, 255, 0],
    [0, 0, 255],
    [0, 255, 255],
    [255, 0, 255],
    [255, 255, 0],
];
/// The steps of the Adobe RGB sample's gray ramp, from black to white: flat
/// enough to read one with Photoshop's Info panel.
const RAMP_STEPS: u32 = 16;
/// The quality of the Adobe RGB sample's JPEG sources.
const JPEG_QUALITY: u8 = 95;
/// The profile names of [`rgb_profile`] in linear light and of
/// [`adobe_rgb_profile`].
const LINEAR_PROFILE_NAME: &str = "Photostacker linear sRGB";
const ADOBE_RGB_PROFILE_NAME: &str = "Photostacker Adobe RGB (1998)";

/// The image resource id of the ICC profile in a PSD.
const PSD_ICC: u16 = 1039;
/// ImageSourceData, the layers of a layered TIFF.
const IMAGE_SOURCE_DATA: u16 = 37724;
/// Photoshop image resources, which the TIFF writer never writes.
const PHOTOSHOP_RESOURCES: u16 = 34377;
const DOCUMENT_DATA_BLOCK: &[u8] = b"Adobe Photoshop Document Data Block\0";
const DOCUMENT_DATA_V0002: &[u8] = b"Adobe Photoshop Document Data V0002\0";

static NEVER: AtomicBool = AtomicBool::new(false);

/// The pixels of a sample's layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pixels {
    /// 8-bit RGB without a profile.
    Rgb8,
    /// 8-bit RGB with a soft-edged elliptical alpha that shrinks down the
    /// stack, so each layer's transparency can be told apart.
    Rgba8,
    /// 16-bit RGB in linear light, with the linear profile: the sRGB levels
    /// decoded to linear, so neither byte of a sample copies the other.
    Rgb16Linear,
    /// [`Pixels::Rgb16Linear`] with the alpha of [`Pixels::Rgba8`] at 16
    /// bits.
    Rgba16Linear,
    /// 8-bit gray PNG files with a Gray ICC profile, decoded as the writers
    /// decode files: to RGB pixels, and without the profile, which cannot
    /// describe them.
    GraySource,
    /// 8-bit JPEG files with no ICC profile, whose EXIF marks them as DCF
    /// Adobe RGB, decoded as the writers decode files: with the generated
    /// Adobe RGB profile.
    AdobeRgbSource,
}

impl Pixels {
    fn sixteen_bit(self) -> bool {
        matches!(self, Self::Rgb16Linear | Self::Rgba16Linear)
    }

    fn has_alpha(self) -> bool {
        matches!(self, Self::Rgba8 | Self::Rgba16Linear)
    }

    /// The name of the profile the layers carry, when it is one we generate.
    fn profile_name(self) -> Option<&'static str> {
        match self {
            Self::Rgb16Linear | Self::Rgba16Linear => Some(LINEAR_PROFILE_NAME),
            Self::AdobeRgbSource => Some(ADOBE_RGB_PROFILE_NAME),
            Self::Rgb8 | Self::Rgba8 | Self::GraySource => None,
        }
    }
}

/// One synthetic stack of the set.
#[derive(Debug, Clone, Copy)]
struct Sample {
    name: &'static str,
    format: OutputFormat,
    /// The container the writer must choose.
    container: Container,
    size: (u32, u32),
    pixels: Pixels,
    compression16: Compression16,
    /// The layers' exposures, top layer first.
    evs: &'static [f32],
}

const fn sample(
    name: &'static str,
    format: OutputFormat,
    container: Container,
    pixels: Pixels,
    compression16: Compression16,
) -> Sample {
    Sample {
        name,
        format,
        container,
        size: SIZE,
        pixels,
        compression16,
        evs: EVS_3,
    }
}

/// The set of spec §12.5, plus RGBA PSDs (8- and 16-bit) for the negative
/// layer count, a 16-bit PSB, an RGBA TIFF and a stack of gray sources.
/// 8-bit layers are always PackBits; `compression16` matters at 16 bits.
const SAMPLES: [Sample; 12] = [
    sample(
        "psd-8bit-rle",
        OutputFormat::Psd,
        Container::Psd,
        Pixels::Rgb8,
        Compression16::Rle,
    ),
    sample(
        "psd-8bit-rgba",
        OutputFormat::Psd,
        Container::Psd,
        Pixels::Rgba8,
        Compression16::Rle,
    ),
    sample(
        "psd-16bit-zip",
        OutputFormat::Psd,
        Container::Psd,
        Pixels::Rgb16Linear,
        Compression16::Zip,
    ),
    sample(
        "psd-16bit-rle",
        OutputFormat::Psd,
        Container::Psd,
        Pixels::Rgb16Linear,
        Compression16::Rle,
    ),
    // `Lr16` with a negative layer count.
    sample(
        "psd-16bit-rgba",
        OutputFormat::Psd,
        Container::Psd,
        Pixels::Rgba16Linear,
        Compression16::Zip,
    ),
    // Gray sources stacked into an RGB PSD: no Gray profile may travel.
    sample(
        "psd-8bit-gray-source",
        OutputFormat::Psd,
        Container::Psd,
        Pixels::GraySource,
        Compression16::Rle,
    ),
    // Untagged JPEGs that a DCF camera marked as Adobe RGB: the document
    // embeds the generated Adobe RGB profile.
    sample(
        "psd-8bit-adobe-rgb",
        OutputFormat::Psd,
        Container::Psd,
        Pixels::AdobeRgbSource,
        Compression16::Rle,
    ),
    Sample {
        size: WIDE,
        evs: EVS_2,
        ..sample(
            "psb-wide",
            OutputFormat::Psd,
            Container::Psb,
            Pixels::Rgb8,
            Compression16::Rle,
        )
    },
    // `Lr16` with PSB widths: the container of a large 16-bit RAW bracket.
    Sample {
        size: WIDE,
        evs: EVS_2,
        ..sample(
            "psb-wide-16bit",
            OutputFormat::Psd,
            Container::Psb,
            Pixels::Rgb16Linear,
            Compression16::Zip,
        )
    },
    sample(
        "tiff-8bit",
        OutputFormat::Tiff,
        Container::Tiff,
        Pixels::Rgb8,
        Compression16::Rle,
    ),
    sample(
        "tiff-16bit",
        OutputFormat::Tiff,
        Container::Tiff,
        Pixels::Rgb16Linear,
        Compression16::Zip,
    ),
    // ExtraSamples 2 (unassociated alpha), and a negative count in `Layr`.
    sample(
        "tiff-8bit-rgba",
        OutputFormat::Tiff,
        Container::Tiff,
        Pixels::Rgba8,
        Compression16::Rle,
    ),
];

/// The opt-in BigTIFF sample (`--bigtiff`).
const BIGTIFF: Sample = Sample {
    size: BIG,
    evs: EVS_2,
    ..sample(
        "bigtiff-16bit",
        OutputFormat::Tiff,
        Container::BigTiff,
        Pixels::Rgb16Linear,
        Compression16::Zip,
    )
};

/// A written file, and what reading it back must find.
struct Written {
    path: PathBuf,
    container: Container,
    /// The layer names, top layer first.
    names: Vec<String>,
    /// The top layer's probe: the canvas size, depth, alpha and ICC profile.
    probe: Probe,
    /// The name the embedded profile carries, when it is one we generate.
    profile_name: Option<&'static str>,
}

fn main() -> Result<()> {
    let (dir, bigtiff) = parse_args()?;
    clear(&dir)?;
    let sources = clear_sources(&dir)?;

    let mut samples = SAMPLES.to_vec();
    if bigtiff {
        samples.push(BIGTIFF);
    }
    let mut written = Vec::new();
    let mut failures = Vec::new();
    for sample in samples {
        match write_sample(&dir, &sources, sample) {
            Ok(file) => written.push(file),
            Err(e) => failures.push(format!("{}: {e:#}", sample.name)),
        }
    }
    match std::env::var_os("PHOTOSTACKER_FIXTURES") {
        Some(root) => {
            if let Err(e) = write_real_bracket(&dir, Path::new(&root), &mut written) {
                failures.push(format!("{REAL_BRACKET}: {e:#}"));
            }
        }
        None => println!("note: PHOTOSTACKER_FIXTURES is not set, so the real bracket is skipped"),
    }

    for file in &written {
        let checked = check(file);
        println!(
            "{}  {}  {} layers  {}",
            file.path.display(),
            container_name(file.container),
            file.names.len(),
            size_text(&file.path)
        );
        if let Err(e) = checked {
            println!("  FAILED: {e:#}");
            failures.push(format!("{}: {e:#}", file.path.display()));
        }
    }
    if !failures.is_empty() {
        bail!(
            "{} checkpoint failures:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
    Ok(())
}

fn parse_args() -> Result<(PathBuf, bool)> {
    let mut dir = None;
    let mut bigtiff = false;
    for arg in std::env::args_os().skip(1) {
        if arg == "--bigtiff" {
            bigtiff = true;
        } else if dir.is_none() {
            dir = Some(PathBuf::from(arg));
        } else {
            bail!(USAGE);
        }
    }
    Ok((dir.ok_or_else(|| anyhow!(USAGE))?, bigtiff))
}

/// Creates `dir`, or removes the regular files directly in it. Anything
/// else (folders, links) is left alone. Nothing is removed unless the path
/// ends in [`CHECK_FOLDER`] and no component of it is a link: its canonical
/// form must be its absolute form, which also refuses a `..` in it.
///
/// On Windows the canonical form is a verbatim `\\?\` path, which an
/// ordinary absolute path never equals, so an ordinary path is refused
/// there, after a missing folder was created; only a path given in its
/// verbatim form can pass. Run it from WSL (`just ps-samples`), or give it
/// the folder in that form.
///
/// The guard checks paths, not open folders, so two residuals remain. A
/// link swapped into the path between the check and the removals is
/// followed (a walk by file descriptor, `openat` with `O_NOFOLLOW`, would
/// close that race). And a missing folder is created before the check, so
/// through a linked parent `create_dir_all` can make an empty folder before
/// the refusal. The folder is in the developer's own checkout, where
/// neither is worth that code.
fn clear(dir: &Path) -> Result<()> {
    // Rebuilt from its components, so a trailing `/` is dropped: with it,
    // `symlink_metadata` would follow a final link.
    let dir: PathBuf = dir.components().collect();
    ensure!(
        dir.ends_with(CHECK_FOLDER),
        "refusing to clear {}: the folder must end in {CHECK_FOLDER}",
        dir.display()
    );
    match fs::symlink_metadata(&dir) {
        // `symlink_metadata` does not follow a link, so a link is not a dir.
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => bail!(
            "refusing to clear {}: it is not a folder, or is a link",
            dir.display()
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
        }
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", dir.display())),
    }
    let canonical =
        fs::canonicalize(&dir).with_context(|| format!("cannot resolve {}", dir.display()))?;
    let absolute = std::path::absolute(&dir)
        .with_context(|| format!("cannot make {} absolute", dir.display()))?;
    ensure!(
        canonical == absolute,
        "refusing to clear {}: it resolves to {}, through a link or a ..",
        dir.display(),
        canonical.display()
    );
    remove_files_in(&dir)
}

/// The [`SOURCES_FOLDER`] of `dir`, a folder that [`clear`] has passed:
/// created when missing, else its regular files are removed, as [`clear`]
/// removes them. A `sources` that is a link, or not a folder, is refused.
fn clear_sources(dir: &Path) -> Result<PathBuf> {
    let sources = dir.join(SOURCES_FOLDER);
    match fs::symlink_metadata(&sources) {
        Ok(meta) if meta.is_dir() => remove_files_in(&sources)?,
        Ok(_) => bail!(
            "refusing to clear {}: it is not a folder, or is a link",
            sources.display()
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(&sources)
                .with_context(|| format!("cannot create {}", sources.display()))?;
        }
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", sources.display())),
    }
    Ok(sources)
}

/// Removes the regular files directly in `dir`, and nothing else.
fn remove_files_in(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("cannot list {}", dir.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            fs::remove_file(entry.path())
                .with_context(|| format!("cannot remove {}", entry.path().display()))?;
        }
    }
    Ok(())
}

// --- The synthetic samples ---

/// Writes `sample` into `dir`, with its source files, if any, in `sources`.
fn write_sample(dir: &Path, sources: &Path, sample: Sample) -> Result<Written> {
    let inputs = sample_inputs(sample, sources)?;
    let names = inputs
        .iter()
        .map(|input| layer_name(&input.stem, input.exif.as_ref()))
        .collect();
    let opts = WriteOptions {
        compression16: sample.compression16,
    };
    let (outcome, warnings) = write_stack(dir, sample.format, opts, inputs, &|_| {}, &NEVER)?;
    // A warning means a layer was left out, e.g. a probe that does not match
    // its load: the sample would not test what it says.
    ensure!(warnings.is_empty(), "warnings: {warnings:?}");
    let (path, layers, container) = written_of(outcome)?;
    ensure!(
        layers == sample.evs.len(),
        "{layers} layers written of {}",
        sample.evs.len()
    );
    ensure!(
        container == sample.container,
        "written as {} instead of {}",
        container_name(container),
        container_name(sample.container)
    );
    Ok(Written {
        path,
        container,
        names,
        probe: probe_of(sample),
        profile_name: sample.pixels.profile_name(),
    })
}

/// The layers of `sample`, built in memory, or for [`Pixels::GraySource`]
/// and [`Pixels::AdobeRgbSource`] written into `sources` and decoded from
/// there.
fn sample_inputs(sample: Sample, sources: &Path) -> Result<Vec<StackInput>> {
    (0..sample.evs.len())
        .map(|layer| {
            let stem = match layer {
                0 => sample.name.to_owned(),
                _ => format!("{}-{}", sample.name, layer + 1),
            };
            let ev = sample.evs[layer];
            match sample.pixels {
                Pixels::GraySource => {
                    // A PNG has no EXIF: the bracket is made up, as in memory.
                    let path = gray_source(sources, &stem, sample, layer)?;
                    let exif = Some(bracket_exif(layer, ev));
                    Ok(file_input(stem, path, FileKind::Png, exif))
                }
                Pixels::AdobeRgbSource => {
                    let path = adobe_rgb_source(sources, &stem, sample, layer)?;
                    let exif = read_exif(&path, FileKind::Jpeg)
                        .with_context(|| format!("cannot read the EXIF of {}", path.display()))?;
                    Ok(file_input(stem, path, FileKind::Jpeg, exif))
                }
                Pixels::Rgb8 | Pixels::Rgba8 | Pixels::Rgb16Linear | Pixels::Rgba16Linear => {
                    Ok(StackInput {
                        stem,
                        exif: Some(bracket_exif(layer, ev)),
                        probe: Ok(probe_of(sample)),
                        load: Box::new(move || Ok(layer_image(sample, layer))),
                    })
                }
            }
        })
        .collect()
}

/// The layer input of the file at `path`, of `kind` with `exif`, probed and
/// decoded as the writers probe and decode a file: upright by its EXIF
/// orientation (1 without EXIF), and with the profile its EXIF calls for
/// when it is an untagged JPEG ([`ColourHint::from_exif`]).
///
/// It mirrors the private `MemberDecoding` of `write/mod.rs`, which
/// `write_group` uses for every member: a change there must be made here
/// too, or the samples stop testing what the writers do.
fn file_input(stem: String, path: PathBuf, kind: FileKind, exif: Option<ExifData>) -> StackInput {
    let orientation = exif.as_ref().map_or(1, |exif| exif.orientation);
    let hint = ColourHint::from_exif(exif.as_ref());
    StackInput {
        stem,
        probe: decode::probe(&path, kind, orientation, RawTone::Srgb, hint),
        exif,
        load: Box::new(move || decode::full(&path, kind, orientation, RawTone::Srgb, hint)),
    }
}

/// Writes layer `layer` of `sample` into `sources` as `<stem>.png`: the
/// scene at the layer's EV in 8-bit gray, with a Gray ICC profile in its
/// iCCP chunk. Returns its path.
fn gray_source(sources: &Path, stem: &str, sample: Sample, layer: usize) -> Result<PathBuf> {
    let (width, height) = sample.size;
    let rgb = expose(&scene(SEED, width, height), sample.evs[layer]);
    let gray = DynamicImage::ImageRgb8(rgb).into_luma8();
    let icc = icc_stub(b"mntr", b"GRAY");
    let mut bytes = Vec::new();
    let mut encoder = PngEncoder::new(&mut bytes);
    encoder.set_icc_profile(icc.clone())?;
    encoder.write_image(gray.as_raw(), width, height, ExtendedColorType::L8)?;
    let path = sources.join(format!("{stem}.png"));
    fs::write(&path, bytes).with_context(|| format!("cannot write {}", path.display()))?;
    // The sample tests something only if the decoder sees the profile.
    ensure!(
        embedded_icc(&path)? == Some(icc),
        "{} lost its Gray profile",
        path.display()
    );
    Ok(path)
}

/// Writes layer `layer` of `sample` into `sources` as `<stem>.jpg`: the
/// [`colour_chart`] at the layer's EV, with no ICC profile and the EXIF of
/// [`adobe_rgb_exif`]. Returns its path.
fn adobe_rgb_source(sources: &Path, stem: &str, sample: Sample, layer: usize) -> Result<PathBuf> {
    let (width, height) = sample.size;
    let ev = sample.evs[layer];
    let image = expose(&colour_chart(width, height), ev);
    let bytes = jpeg_with_exif(&image, &adobe_rgb_exif(layer, ev), JPEG_QUALITY);
    let path = sources.join(format!("{stem}.jpg"));
    fs::write(&path, bytes).with_context(|| format!("cannot write {}", path.display()))?;
    // The sample tests the EXIF's profile only if the file has none of its
    // own, which would win.
    ensure!(
        embedded_icc(&path)?.is_none(),
        "{} has an ICC profile",
        path.display()
    );
    Ok(path)
}

/// The ICC profile embedded in the image file at `path`, as `image`'s
/// decoder reads it.
fn embedded_icc(path: &Path) -> Result<Option<Vec<u8>>> {
    let mut decoder = ImageReader::open(path)?
        .with_guessed_format()?
        .into_decoder()?;
    Ok(decoder.icc_profile()?)
}

/// The scene, with a row of the saturated [`PATCHES`] across its upper part
/// and a gray ramp of [`RAMP_STEPS`] steps across its lower part. Each patch
/// takes the middle two thirds of its sixth of the width.
fn colour_chart(width: u32, height: u32) -> RgbImage {
    let mut image = scene(SEED, width, height);
    let columns = PATCHES.len() as u32;
    let patches = height / 8..height * 3 / 8;
    let ramp = height * 5 / 8..height * 7 / 8;
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        if patches.contains(&y) {
            let column = x * columns / width;
            // Where x falls in its column, in sixths of the column.
            let within = (x * columns % width) * 6 / width;
            if (1..5).contains(&within) {
                pixel.0 = PATCHES[column as usize];
            }
        } else if ramp.contains(&y) {
            let step = x * RAMP_STEPS / width;
            let level = (step * 255 / (RAMP_STEPS - 1)) as u8;
            pixel.0 = [level; 3];
        }
    }
    image
}

/// The EXIF of shot `layer + 1` of a Canon AEB at `ev`, from a DCF camera
/// set to Adobe RGB: ColorSpace 0xFFFF (uncalibrated) with the Interop index
/// `R03`. Canon's MakerNote tables are int16 words, word 0 their byte count:
/// ShotInfo word 17 is the AEB value in 1/32 EV, FileInfo word 3 the
/// bracket mode (1 is AEB) and word 5 the shot number.
fn adobe_rgb_exif(layer: usize, ev: f32) -> Vec<u8> {
    let mut shot_info = vec![0u16; 34];
    shot_info[0] = 68;
    shot_info[17] = (ev * 32.0) as i16 as u16;
    let mut file_info = vec![0u16; 20];
    file_info[0] = 40;
    file_info[3] = 1;
    file_info[5] = layer as u16 + 1;
    let spec = TiffSpec {
        little_endian: true,
        ifd0: vec![(0x010F, Value::Ascii("Canon".into()))],
        exif: vec![(0xA001, Value::Short(vec![0xFFFF]))],
        maker_note: Some(MakerNoteSpec {
            style: NoteStyle::CanonIfd,
            entries: vec![
                (0x0004, Value::Short(shot_info)),
                (0x0093, Value::Short(file_info)),
            ],
        }),
    };
    spec.build_with_interop(&[(0x0001, Value::Ascii("R03".into()))])
}

/// The EXIF of shot `layer + 1` of an exposure bracket at `ev`, which gives
/// the layer name its EV, `(0 EV)` included.
fn bracket_exif(layer: usize, ev: f32) -> ExifData {
    ExifData {
        bracket: Some(BracketInfo {
            mode: BracketMode::Exposure,
            shot_number: Some(layer as u16 + 1),
            value_ev: Some(ev),
        }),
        ..ExifData::default()
    }
}

/// What every layer of `sample` decodes to, as the writer checks it. A gray
/// source's Gray profile is dropped, so it has none; an Adobe RGB source
/// takes the generated Adobe RGB profile.
fn probe_of(sample: Sample) -> Probe {
    Probe {
        width: sample.size.0,
        height: sample.size.1,
        sixteen_bit: sample.pixels.sixteen_bit(),
        has_alpha: sample.pixels.has_alpha(),
        icc: icc_of(sample.pixels),
    }
}

fn icc_of(pixels: Pixels) -> Option<Vec<u8>> {
    match pixels {
        Pixels::Rgb16Linear | Pixels::Rgba16Linear => Some(rgb_profile(RawTone::Linear)),
        Pixels::AdobeRgbSource => Some(adobe_rgb_profile()),
        Pixels::Rgb8 | Pixels::Rgba8 | Pixels::GraySource => None,
    }
}

/// Layer `layer` of `sample`: the scene at the layer's EV.
fn layer_image(sample: Sample, layer: usize) -> FullImage {
    let (width, height) = sample.size;
    let ev = sample.evs[layer];
    let base = scene(SEED, width, height);
    let (channels, samples) = match sample.pixels {
        Pixels::Rgb8 => (3, Samples::U8(expose(&base, ev).into_raw())),
        Pixels::Rgba8 => (4, Samples::U8(with_alpha(&expose(&base, ev), layer))),
        Pixels::Rgb16Linear => (3, Samples::U16(linear_16(&base, ev))),
        Pixels::Rgba16Linear => {
            let rgba = with_alpha_16(&linear_16(&base, ev), sample.size, layer);
            (4, Samples::U16(rgba))
        }
        Pixels::GraySource | Pixels::AdobeRgbSource => {
            unreachable!("the layers of source files are decoded from them")
        }
    };
    FullImage {
        width,
        height,
        channels,
        samples,
        icc: icc_of(sample.pixels),
        source: ImageSource::Decoded,
    }
}

/// The opacity, 0 to 1, of pixel (`x`, `y`) of a `size` layer `layer`:
/// opaque inside an ellipse whose radius shrinks with `layer`, fading to
/// transparent over a fifth of the half-size, and transparent in the
/// corners.
fn opacity(x: u32, y: u32, (w, h): (u32, u32), layer: usize) -> f32 {
    let radius = 0.95 - 0.15 * layer as f32;
    let du = (x as f32 + 0.5) / w as f32 * 2.0 - 1.0;
    let dv = (y as f32 + 0.5) / h as f32 * 2.0 - 1.0;
    ((radius - (du * du + dv * dv).sqrt()) / 0.2).clamp(0.0, 1.0)
}

/// `img` as RGBA, with the [`opacity`] of `layer` as its alpha.
fn with_alpha(img: &RgbImage, layer: usize) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(img.as_raw().len() / 3 * 4);
    for (x, y, pixel) in img.enumerate_pixels() {
        rgba.extend_from_slice(&pixel.0);
        rgba.push((opacity(x, y, img.dimensions(), layer) * 255.0).round() as u8);
    }
    rgba
}

/// 16-bit RGB samples of a `size` layer as RGBA, with the [`opacity`] of
/// `layer` as its alpha.
fn with_alpha_16(rgb: &[u16], size: (u32, u32), layer: usize) -> Vec<u16> {
    let width = size.0 as usize;
    let mut rgba = Vec::with_capacity(rgb.len() / 3 * 4);
    for (i, pixel) in rgb.as_chunks::<3>().0.iter().enumerate() {
        let (x, y) = ((i % width) as u32, (i / width) as u32);
        rgba.extend_from_slice(pixel);
        rgba.push((opacity(x, y, size, layer) * 65535.0).round() as u16);
    }
    rgba
}

/// `img`'s sRGB levels decoded to linear light, exposed by `ev` stops and
/// clamped to 1, as 16-bit samples.
fn linear_16(img: &RgbImage, ev: f32) -> Vec<u16> {
    let gain = ev.exp2();
    let table: Vec<u16> = (0..=255u8)
        .map(|v| {
            let linear = (srgb_to_linear(f32::from(v) / 255.0) * gain).min(1.0);
            (linear * 65535.0).round() as u16
        })
        .collect();
    img.as_raw()
        .iter()
        .map(|&v| table[usize::from(v)])
        .collect()
}

/// The IEC 61966-2-1 sRGB decoding.
fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

// --- The real bracket ---

/// Analyses the real bracket under `fixtures` and writes it into `dir` as
/// PSD, then as TIFF, adding each file to `written`.
fn write_real_bracket(dir: &Path, fixtures: &Path, written: &mut Vec<Written>) -> Result<()> {
    let input = fixtures.join(REAL_BRACKET);
    ensure!(input.is_dir(), "{} is not a folder", input.display());
    for format in [OutputFormat::Psd, OutputFormat::Tiff] {
        let mut config = JobConfig::new(input.clone());
        config.output_dir = dir.to_path_buf();
        config.output_format = format;
        let analysis = analyze(&config, &|_| {}, &NEVER)?;
        let [group] = analysis.groups.as_slice() else {
            bail!("{} groups found instead of one", analysis.groups.len());
        };
        ensure!(
            group.members.len() == REAL_BRACKET_LAYERS,
            "a group of {} instead of {REAL_BRACKET_LAYERS}",
            group.members.len()
        );

        let report = write_all(&config, &analysis, WriteOptions::default(), &|_| {}, &NEVER)?;
        for (id, warning) in &report.warnings {
            println!("note: {}: {warning:?}", analysis.files[*id].path.display());
        }
        let [(_, outcome)] = report.outcomes.as_slice() else {
            bail!("{} outcomes instead of one", report.outcomes.len());
        };
        let outcome = outcome.clone().map_err(|e| anyhow!(e))?;
        let (path, layers, container) = written_of(outcome)?;
        ensure!(
            layers == REAL_BRACKET_LAYERS,
            "{layers} layers written of {REAL_BRACKET_LAYERS}"
        );

        let names = group
            .members
            .iter()
            .map(|&id| {
                let file = &analysis.files[id];
                let stem = file.path.file_stem().unwrap_or_default().to_string_lossy();
                layer_name(&stem, file.exif.as_ref())
            })
            .collect();
        let top = &analysis.files[group.members[0]];
        let orientation = top.exif.as_ref().map_or(1, |exif| exif.orientation);
        let hint = ColourHint::from_exif(top.exif.as_ref());
        let probe = decode::probe(&top.path, top.kind, orientation, config.raw_tone, hint)?;
        written.push(Written {
            path,
            container,
            names,
            probe,
            profile_name: None,
        });
    }
    Ok(())
}

/// The path, layer count and container of a written stack.
fn written_of(outcome: WriteOutcome) -> Result<(PathBuf, usize, Container)> {
    match outcome {
        WriteOutcome::Written {
            path,
            layers,
            container,
        } => Ok((path, layers, container)),
        WriteOutcome::Skipped { reason } => bail!("skipped: {reason}"),
    }
}

// --- Reading the files back ---

fn check(file: &Written) -> Result<()> {
    match file.container {
        Container::Psd | Container::Psb => check_psd(file),
        Container::Tiff | Container::BigTiff => check_tiff(file),
    }
}

/// Parses the whole document with testkit's strict reader, every channel
/// decoded, and checks its header, layer count and names.
fn check_psd(file: &Written) -> Result<()> {
    let bytes = fs::read(&file.path)?;
    let psd = psd_reader::read(&bytes).map_err(|e| anyhow!("does not parse: {e}"))?;
    let version = if file.container == Container::Psb {
        2
    } else {
        1
    };
    ensure!(psd.version == version, "version {}", psd.version);
    let probe = &file.probe;
    ensure!(
        (psd.width, psd.height) == (probe.width, probe.height),
        "{}×{} instead of {}×{}",
        psd.width,
        psd.height,
        probe.width,
        probe.height
    );
    ensure!(psd.depth == depth(probe), "{}-bit", psd.depth);
    ensure!(
        psd.channels == u16::from(channels(probe)),
        "{} channels",
        psd.channels
    );
    // Negative with alpha: the merged image's alpha is its transparency.
    let count = file.names.len() as i16;
    let expected = if probe.has_alpha { -count } else { count };
    ensure!(
        psd.layer_count == expected,
        "layer count {} instead of {expected}",
        psd.layer_count
    );
    let icc = psd
        .resources
        .iter()
        .find(|(id, _)| *id == PSD_ICC)
        .map(|(_, data)| data.as_slice());
    check_icc(icc, file)?;
    check_names(&psd.layers, &file.names)
}

/// `icc`, the profile embedded in `file`, is exactly the one of its probe,
/// or none when it has none, and carries the name it should.
fn check_icc(icc: Option<&[u8]>, file: &Written) -> Result<()> {
    let describe = |icc: Option<&[u8]>| match icc {
        Some(profile) => format!("a {}-byte ICC profile", profile.len()),
        None => "no ICC profile".to_owned(),
    };
    let expected = file.probe.icc.as_deref();
    ensure!(
        icc == expected,
        "{} instead of {}",
        describe(icc),
        describe(expected)
    );
    if let (Some(profile), Some(name)) = (icc, file.profile_name) {
        ensure!(
            profile
                .windows(name.len())
                .any(|bytes| bytes == name.as_bytes()),
            "the ICC profile is not named {name:?}"
        );
    }
    Ok(())
}

/// Decodes the composite with the `tiff` crate, then parses the layers of
/// its ImageSourceData, and checks the byte order, the signature that goes
/// with the container, and that tag 34377 is absent.
fn check_tiff(file: &Written) -> Result<()> {
    let big = file.container == Container::BigTiff;
    let mut header = [0u8; 4];
    File::open(&file.path)?.read_exact(&mut header)?;
    let version = if big { 43 } else { 42 };
    ensure!(
        header == [b'M', b'M', 0, version],
        "header {header:?}: not a big-endian {}",
        container_name(file.container)
    );

    let mut decoder =
        Decoder::new(BufReader::new(File::open(&file.path)?))?.with_limits(Limits::unlimited());
    let probe = &file.probe;
    let dimensions = decoder.dimensions()?;
    ensure!(
        dimensions == (probe.width, probe.height),
        "{dimensions:?} instead of {}×{}",
        probe.width,
        probe.height
    );
    let samples = match decoder.read_image()? {
        DecodingResult::U8(samples) if !probe.sixteen_bit => samples.len(),
        DecodingResult::U16(samples) if probe.sixteen_bit => samples.len(),
        _ => bail!("the composite is not {}-bit", depth(probe)),
    };
    let expected = probe.width as usize * probe.height as usize * usize::from(channels(probe));
    ensure!(
        samples == expected,
        "{samples} composite samples instead of {expected}"
    );
    ensure!(
        decoder
            .find_tag(Tag::Unknown(PHOTOSHOP_RESOURCES))?
            .is_none(),
        "tag {PHOTOSHOP_RESOURCES} is present"
    );
    let icc = decoder
        .find_tag(Tag::IccProfile)?
        .map(|value| value.into_u8_vec())
        .transpose()?;
    check_icc(icc.as_deref(), file)?;
    // Unassociated alpha (2) exactly when the canvas has alpha.
    let extra = decoder
        .find_tag(Tag::ExtraSamples)?
        .map(|value| value.into_u16_vec())
        .transpose()?;
    let expected = probe.has_alpha.then(|| vec![2]);
    ensure!(
        extra == expected,
        "ExtraSamples {extra:?} instead of {expected:?}"
    );

    let source = decoder.get_tag_u8_vec(Tag::Unknown(IMAGE_SOURCE_DATA))?;
    let signature = if big {
        DOCUMENT_DATA_V0002
    } else {
        DOCUMENT_DATA_BLOCK
    };
    ensure!(
        source.starts_with(signature),
        "the ImageSourceData signature is not {:?}",
        String::from_utf8_lossy(signature)
    );
    let layers = psd_reader::read_layer_block(&source)
        .map_err(|e| anyhow!("the layers do not parse: {e}"))?;
    check_names(&layers, &file.names)
}

/// `layers` (bottom first, as stored) have the Unicode names `names` (top
/// first), and ASCII Pascal names.
fn check_names(layers: &[Layer], names: &[String]) -> Result<()> {
    let found: Vec<String> = layers
        .iter()
        .rev()
        .map(|layer| layer.name_unicode.clone().unwrap_or_default())
        .collect();
    ensure!(found == names, "layer names {found:?} instead of {names:?}");
    ensure!(
        layers.iter().all(|layer| layer.name_pascal.is_ascii()),
        "a Pascal name is not ASCII"
    );
    Ok(())
}

fn depth(probe: &Probe) -> u16 {
    if probe.sixteen_bit { 16 } else { 8 }
}

fn channels(probe: &Probe) -> u8 {
    if probe.has_alpha { 4 } else { 3 }
}

fn container_name(container: Container) -> &'static str {
    match container {
        Container::Psd => "psd",
        Container::Psb => "psb",
        Container::Tiff => "tiff",
        Container::BigTiff => "bigtiff",
    }
}

/// The size of the file at `path`, or `?` when its metadata cannot be read.
fn size_text(path: &Path) -> String {
    fs::metadata(path).map_or_else(|_| "?".to_owned(), |meta| human_size(meta.len()))
}

fn human_size(bytes: u64) -> String {
    format!(
        "{:.1} MiB ({bytes} bytes)",
        bytes as f64 / f64::from(1 << 20)
    )
}

// The tests run on every system: their root is canonical, so on Windows a
// verbatim path, which `clear` accepts (see there). Two are Unix only, each
// saying why; the link tests skip where the account may not make links.
#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    use photostacker_testkit::symlink_dir;

    use super::*;

    /// A temporary root, canonical so that its own path has no link, with
    /// `victim/precious.txt` in it.
    fn root_with_victim() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let victim = root.join("victim");
        fs::create_dir(&victim).unwrap();
        let precious = victim.join("precious.txt");
        fs::write(&precious, b"keep").unwrap();
        (temp, root, precious)
    }

    #[test]
    fn a_final_link_is_refused() {
        let (_temp, root, precious) = root_with_victim();
        fs::create_dir_all(root.join("x/dist")).unwrap();
        if !symlink_dir(&root.join("victim"), &root.join("x/dist/ps-check")) {
            return;
        }
        assert!(clear(&root.join("x/dist/ps-check")).is_err());
        assert!(precious.exists());
    }

    // Unix only: it is about a trailing `/`, after which Unix follows a final
    // link; the tests' root is a verbatim path on Windows, in which a `/` is
    // part of a name, not a separator.
    #[cfg(unix)]
    #[test]
    fn a_final_link_with_a_trailing_slash_is_refused() {
        let (_temp, root, precious) = root_with_victim();
        fs::create_dir_all(root.join("x/dist")).unwrap();
        symlink(root.join("victim"), root.join("x/dist/ps-check")).unwrap();
        let path = PathBuf::from(format!("{}/x/dist/ps-check/", root.display()));
        assert!(clear(&path).is_err());
        assert!(precious.exists());
    }

    #[test]
    fn a_linked_intermediate_folder_is_refused() {
        let (_temp, root, _) = root_with_victim();
        let precious = root.join("victim/ps-check/precious.txt");
        fs::create_dir(root.join("victim/ps-check")).unwrap();
        fs::write(&precious, b"keep").unwrap();
        fs::create_dir(root.join("x")).unwrap();
        if !symlink_dir(&root.join("victim"), &root.join("x/dist")) {
            return;
        }
        assert!(clear(&root.join("x/dist/ps-check")).is_err());
        assert!(clear(&root.join("x/dist/ps-check/")).is_err());
        assert!(precious.exists());
    }

    #[test]
    fn another_folder_is_refused() {
        let (_temp, root, precious) = root_with_victim();
        assert!(clear(&root.join("victim")).is_err());
        assert!(clear(&root.join("x/dist/ps-check/..")).is_err());
        assert!(precious.exists());
    }

    // Unix only: it passes the folder with a trailing `/`, which the tests'
    // verbatim root on Windows would take as part of the folder's name.
    #[cfg(unix)]
    #[test]
    fn a_real_folder_loses_only_its_regular_files() {
        let (_temp, root, _) = root_with_victim();
        let dir = root.join("x/dist/ps-check");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("old.psd"), b"old").unwrap();
        fs::write(dir.join("sub/kept.txt"), b"kept").unwrap();
        symlink(root.join("victim/precious.txt"), dir.join("link.txt")).unwrap();
        clear(&PathBuf::from(format!("{}/", dir.display()))).unwrap();
        assert!(!dir.join("old.psd").exists());
        assert!(dir.join("sub/kept.txt").exists());
        assert!(dir.join("link.txt").symlink_metadata().is_ok());
        assert!(root.join("victim/precious.txt").exists());
    }

    #[test]
    fn the_size_of_an_unreadable_file_is_a_question_mark() {
        let (_temp, root, precious) = root_with_victim();
        assert_eq!(size_text(&precious), "0.0 MiB (4 bytes)");
        assert_eq!(size_text(&root.join("missing.psd")), "?");
    }

    #[test]
    fn a_missing_folder_is_created() {
        let (_temp, root, _) = root_with_victim();
        let dir = root.join("x/dist/ps-check");
        clear(&dir).unwrap();
        assert!(dir.is_dir());
    }

    #[test]
    fn the_sources_folder_is_created_then_loses_only_its_regular_files() {
        let (_temp, root, _) = root_with_victim();
        let dir = root.join("x/dist/ps-check");
        clear(&dir).unwrap();
        let sources = clear_sources(&dir).unwrap();
        assert_eq!(sources, dir.join("sources"));
        assert!(sources.is_dir());
        fs::create_dir(sources.join("sub")).unwrap();
        fs::write(sources.join("old.jpg"), b"old").unwrap();
        clear_sources(&dir).unwrap();
        assert!(!sources.join("old.jpg").exists());
        assert!(sources.join("sub").is_dir());
    }

    #[test]
    fn a_linked_sources_folder_is_refused() {
        let (_temp, root, precious) = root_with_victim();
        let dir = root.join("x/dist/ps-check");
        clear(&dir).unwrap();
        if !symlink_dir(&root.join("victim"), &dir.join("sources")) {
            return;
        }
        assert!(clear_sources(&dir).is_err());
        assert!(precious.exists());
    }

    #[test]
    fn the_adobe_rgb_sample_embeds_the_generated_profile() {
        let temp = tempfile::tempdir().unwrap();
        let (dir, sources) = (temp.path().join("out"), temp.path().join("sources"));
        fs::create_dir(&dir).unwrap();
        fs::create_dir(&sources).unwrap();
        let sample = SAMPLES
            .iter()
            .find(|sample| sample.name == "psd-8bit-adobe-rgb")
            .expect("an Adobe RGB sample in SAMPLES");
        // Small, so the test is quick; everything else is the example's.
        let sample = Sample {
            size: (192, 128),
            ..*sample
        };
        let file = write_sample(&dir, &sources, sample).unwrap();
        let adobe_rgb = adobe_rgb_profile();
        check(&file).unwrap();
        let psd = psd_reader::read(&fs::read(&file.path).unwrap()).unwrap();
        let icc = psd.resources.iter().find(|(id, _)| *id == PSD_ICC);
        assert_eq!(icc.map(|(_, data)| data), Some(&adobe_rgb));
        assert!(String::from_utf8_lossy(&adobe_rgb).contains("Photostacker Adobe RGB (1998)"));
        // The EVs come from the bracket in the files' EXIF.
        assert_eq!(
            file.names,
            [
                "psd-8bit-adobe-rgb (0 EV)",
                "psd-8bit-adobe-rgb-2 (−1 EV)",
                "psd-8bit-adobe-rgb-3 (+1 EV)",
            ]
        );
        // The profile comes from the EXIF alone: no source embeds one, and
        // the core's probe of each, with the hint of its EXIF, gives it.
        let jpegs: Vec<PathBuf> = fs::read_dir(&sources)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(jpegs.len(), 3, "{jpegs:?}");
        for path in jpegs {
            assert_eq!(embedded_icc(&path).unwrap(), None, "{}", path.display());
            let exif = read_exif(&path, FileKind::Jpeg).unwrap();
            let input = file_input("source".into(), path.clone(), FileKind::Jpeg, exif);
            let probe = input.probe.unwrap();
            assert_eq!(probe.icc.as_ref(), Some(&adobe_rgb), "{}", path.display());
        }
    }
}
