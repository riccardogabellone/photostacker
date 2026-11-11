//! The stack writers (spec §5.5, §5.6, §6): the shared layer encoder, the
//! PSD/PSB and layered-TIFF containers, and the stack writer that drives them.
//!
//! [`write_stack`] turns candidate layers into one file:
//!
//! 1. Every input's probe is read; one that failed is left out with
//!    [`Warning::LayerUnreadable`].
//! 2. The canvas profile keeps the members at the canvas resolution and
//!    leaves the others out with [`Warning::ResolutionMismatch`].
//! 3. With fewer than 2 members kept, the stack is skipped.
//! 4. A `.partial` file and the encoder's spill files are opened under a
//!    provisional name: `<first kept stem>_<kept>.<ext>`, or its `-k` form
//!    for the smallest k whose three files are all free (a crashed run's
//!    leftovers are never reused, nor removed).
//! 5. Each kept member, in layer order, is decoded and encoded, one at a
//!    time: the decoded image is moved into the encoder, which frees it
//!    before the next one is decoded. A member whose decode fails, or whose
//!    image no longer matches its probe (size, depth, alpha or colour
//!    profile: the file changed since it was probed), or that the encoder
//!    refuses before writing any of it, is left out with
//!    [`Warning::LayerUnreadable`].
//! 6. With fewer than 2 layers written, the stack is skipped.
//! 7. The container is written into the partial from the spill files. The
//!    embedded ICC profile is the first written layer's, and the
//!    [`Warning::IccMismatch`] warnings compare the written layers with it.
//! 8. The partial is synced and persisted with no clobber under
//!    `<first written stem>_<written>.<ext>`, or the first free `-k` form,
//!    and the folder is synced.
//!
//! On a cancel or an error, the partial and the spill files are removed;
//! nothing else is ever removed or renamed. A stem that is not a file name
//! (empty, `.`, `..`, with a path separator, or on Windows with `:`) is
//! refused before any of it.
//!
//! [`write_groups`] writes the stacks of a job one after the other, each into
//! the output's mirror of its photos' folder, and tells a [`WriteObserver`]
//! as it goes; [`write_all`] and [`write_group`] are built on the same steps.
//!
//! [`predict_stack`] runs steps 1 to 3 on a group's files, probing each one
//! as the writer does, and names the stack it would write: what the
//! inspector shows before anything is decoded.

mod layers;
mod naming;
mod predict;
mod psd;
mod tiff;

use std::cell::RefCell;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub use naming::{format_ev, layer_exposure, layer_name, output_name};
pub use predict::{MemberProbe, PredictMember, StackPrediction, predict_stack};
pub(crate) use psd::APP_NAME;

use crate::analyze::{Analysis, FileRecord, Progress, Stage, Ticker};
use crate::decode::{self, ColourHint, DecodeError, FullImage, Probe, Samples};
use crate::exif::ExifData;
use crate::fsio::{
    OutputDirError, PartialFile, PersistError, SyncDir, ensure_output_dir, sync_dir,
    sync_dir_or_warn,
};
use crate::group::{CanvasProfile, Container, profile};
use crate::model::{FileKind, JobConfig, OutputFormat, RawTone, Warning};
use layers::{LayerEncoder, LengthWidth};

/// The names a stack tries, `<stem>_<N>` then `-1` to `-9999`, before it
/// gives up.
const NAME_ATTEMPTS: u32 = 10_000;
/// The fewest layers a stack is written with.
const MIN_LAYERS: usize = 2;

/// How 16-bit layer channels are compressed. 8-bit channels always use
/// PackBits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Compression16 {
    /// ZIP with prediction (PSD compression 3).
    #[default]
    Zip,
    /// PackBits RLE (PSD compression 1), for the Photoshop checkpoint.
    Rle,
}

/// Options of the stack writers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WriteOptions {
    pub compression16: Compression16,
}

/// What became of one stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The stack was written to `path` with `layers` layers.
    Written {
        path: PathBuf,
        layers: usize,
        container: Container,
    },
    /// Fewer than 2 of its photos could be stacked; nothing was written.
    Skipped { reason: String },
}

/// Why writing stopped.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error(transparent)]
    OutputDir(#[from] OutputDirError),
    #[error("cannot write the stack: {0}")]
    Io(#[from] io::Error),
    /// The group's output subfolder, `rel` relative to the output folder,
    /// resolves outside the output folder (see [`write_groups`]).
    #[error("the output subfolder {} leaves the output folder", rel.display())]
    SubfolderOutside { rel: PathBuf },
    #[error("writing was cancelled")]
    Cancelled,
}

/// One candidate layer of a stack, in layer order.
pub struct StackInput {
    /// The file stem: the layer's name, and the output's for the top layer.
    pub stem: String,
    /// Gives the layer name its exposure (see [`layer_name`]).
    pub exif: Option<ExifData>,
    /// What `load` is expected to give, read from the headers only.
    pub probe: Result<Probe, DecodeError>,
    /// Decodes the layer at full resolution; called only when the layer is
    /// encoded, and at most once.
    pub load: Box<dyn FnOnce() -> Result<FullImage, DecodeError> + Send>,
}

/// What [`write_all`] or [`write_groups`] did, group by group.
#[derive(Debug)]
pub struct WriteReport {
    /// One entry per group, by group index: its outcome, or why it failed.
    pub outcomes: Vec<(usize, Result<WriteOutcome, String>)>,
    /// The warnings of every group, by file id, in group order.
    pub warnings: Vec<(usize, Warning)>,
}

/// What [`write_groups`] tells as it goes. Every method does nothing by
/// default. Groups are written one after the other, so the calls come in
/// that order.
pub trait WriteObserver: Sync {
    /// Member `file` (a file id) was left out, encoded, or skipped with its
    /// stack; or its group failed before reaching it. Called exactly as
    /// [`write_all`] ticks its progress: once per member of every group
    /// attempted, unless the writing is cancelled.
    fn layer(&self, _file: usize) {}

    /// The stack of group `group` has its three files: the partial
    /// `partial` (`<final>.partial` under the provisional name) and the
    /// spill files `<partial>.layers` and `<partial>.merged`. Called once
    /// per stack, after the ticks of the members left out by their probe or
    /// by the canvas and before any member is decoded; never for a group
    /// that fails or is skipped before that. The writer removes the three
    /// files itself, unless the process dies first.
    fn partial_created(&self, _group: usize, _partial: &Path) {}

    /// Group `group` is done: its outcome, or why it failed, and its
    /// warnings by file id (none when it failed). Called once per group
    /// attempted, but not for the one a cancel interrupts.
    fn group_done(
        &self,
        _group: usize,
        _outcome: &Result<WriteOutcome, String>,
        _warnings: &[(usize, Warning)],
    ) {
    }
}

/// Writes one stack of `inputs` (top layer first) into the folder `dir`, in
/// `format`, as the module documentation describes.
///
/// The warnings are by position in `inputs`, ascending; each input has at
/// most one. `on_layer(i)` is called once for each input `i`, when it is
/// left out, encoded, or skipped with its stack; it is not called for the
/// inputs not reached after a cancel or an error. `cancel` is checked before
/// each layer is decoded and once all are encoded, and then gives
/// [`WriteError::Cancelled`]. ([`write_all`] ticks the inputs of a group
/// that failed itself, so its progress still reaches its total.)
///
/// Every stem must be a file name: exactly one normal path component of the
/// platform, so not empty, `.` or `..`, and without a separator (`/`, and on
/// Windows `\` too), and on Windows without `:`, which would make a drive
/// prefix or an NTFS stream of it. On Unix `\` is an ordinary character.
/// Otherwise the call fails with an `InvalidInput` [`WriteError::Io`] before
/// anything is loaded, created or ticked.
///
/// A layer the encoder refuses before writing any of it (a sample or
/// channel count that does not fit the canvas) is left out with
/// [`Warning::LayerUnreadable`], like one that fails to decode. An I/O
/// error, or an encoder failure once a layer's data is being written, fails
/// the whole stack with [`WriteError::Io`]. Either way, the partial and the
/// spill files are gone.
pub fn write_stack(
    dir: &Path,
    format: OutputFormat,
    opts: WriteOptions,
    inputs: Vec<StackInput>,
    on_layer: &dyn Fn(usize),
    cancel: &AtomicBool,
) -> Result<(WriteOutcome, Vec<(usize, Warning)>), WriteError> {
    write_stack_with(dir, format, opts, inputs, on_layer, &|_| {}, cancel)
}

/// [`write_stack`], calling `on_partial` with the partial's path once its
/// three files exist, before any layer is decoded (see
/// [`WriteObserver::partial_created`]).
fn write_stack_with(
    dir: &Path,
    format: OutputFormat,
    opts: WriteOptions,
    inputs: Vec<StackInput>,
    on_layer: &dyn Fn(usize),
    on_partial: &dyn Fn(&Path),
    cancel: &AtomicBool,
) -> Result<(WriteOutcome, Vec<(usize, Warning)>), WriteError> {
    if let Some(input) = inputs.iter().find(|input| !is_file_name(&input.stem)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "the layer stem {:?} is not a file name: it is empty, . or .., \
                 or holds a path separator (or on Windows a colon)",
                input.stem
            ),
        )
        .into());
    }
    let total = inputs.len();
    let mut stems = Vec::with_capacity(total);
    let mut exifs = Vec::with_capacity(total);
    let mut loads = Vec::with_capacity(total);
    let mut members: Vec<(usize, Probe)> = Vec::with_capacity(total);
    let mut warnings = Vec::new();
    for (i, input) in inputs.into_iter().enumerate() {
        stems.push(input.stem);
        exifs.push(input.exif);
        loads.push(Some(input.load));
        match input.probe {
            Ok(probe) => members.push((i, probe)),
            Err(error) => {
                warnings.push((i, unreadable(error.to_string())));
                on_layer(i);
            }
        }
    }

    let mut canvas = profile(&members, format);
    for (i, warning) in &canvas.excluded {
        warnings.push((*i, warning.clone()));
        on_layer(*i);
    }
    if canvas.kept.len() < MIN_LAYERS {
        canvas.kept.iter().for_each(|&i| on_layer(i));
        return Ok((skipped(canvas.kept.len(), total), by_input(warnings)));
    }

    let probe_of = |i: usize| {
        &members
            .iter()
            .find(|(member, _)| *member == i)
            .expect("every kept input was probed")
            .1
    };
    let (mut partial, mut encoder) = open_provisional(dir, &stems[canvas.kept[0]], &canvas, opts)?;
    on_partial(partial.path());
    let mut written = Vec::with_capacity(canvas.kept.len());
    for &i in &canvas.kept {
        if cancel.load(Ordering::Relaxed) {
            return Err(WriteError::Cancelled);
        }
        let load = loads[i].take().expect("each kept input is loaded once");
        // A refusal by the encoder's pre-write check leaves it usable, so it
        // only leaves this layer out. `unchanged` already catches what a
        // decoder can give; the check also covers sample and channel counts
        // that an in-memory `load` may get wrong.
        let loaded = load()
            .map_err(|error| error.to_string())
            .and_then(|image| unchanged(image, probe_of(i), &canvas))
            .and_then(|image| match encoder.check(&image) {
                Ok(()) => Ok(image),
                Err(refused) => Err(refused.to_string()),
            });
        match loaded {
            // The encoder takes the image and frees it before returning.
            Ok(image) => {
                encoder.add_layer(&layer_name(&stems[i], exifs[i].as_ref()), image)?;
                written.push(i);
            }
            Err(reason) => warnings.push((i, unreadable(reason))),
        }
        on_layer(i);
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(WriteError::Cancelled);
    }
    if written.len() < MIN_LAYERS {
        return Ok((skipped(written.len(), total), by_input(warnings)));
    }

    // The first written layer is the reference, which may not be the
    // profile's when that one was left out above.
    let reference = probe_of(written[0]).icc.clone();
    let icc_warnings: Vec<(usize, Warning)> = written[1..]
        .iter()
        .filter(|&&i| probe_of(i).icc != reference)
        .map(|&i| (i, Warning::IccMismatch))
        .collect();
    canvas.icc = reference;
    canvas.kept.clone_from(&written);
    canvas.icc_mismatch = !icc_warnings.is_empty();
    canvas.warnings.clone_from(&icc_warnings);
    warnings.extend(icc_warnings);

    let (records, spill) = encoder.finish()?;
    match canvas.container {
        Container::Psd | Container::Psb => {
            psd::write_psd(partial.file(), &canvas, &records, &spill)?;
        }
        Container::Tiff | Container::BigTiff => {
            tiff::write_tiff(partial.file(), &canvas, &records, &spill)?;
        }
    }
    drop(spill);
    let path = persist(
        partial,
        dir,
        &stems[written[0]],
        written.len(),
        canvas.container,
    )?;
    tracing::info!(
        path = %path.display(),
        layers = written.len(),
        container = ?canvas.container,
        "stack written"
    );
    let outcome = WriteOutcome::Written {
        path,
        layers: written.len(),
        container: canvas.container,
    };
    Ok((outcome, by_input(warnings)))
}

/// Writes group `group` of `analysis` into the output folder of `config`,
/// which is checked first ([`ensure_output_dir`]), or into its subfolder
/// for the group's photos (see [`write_groups`]).
///
/// Each member is decoded as its record's kind, upright by its EXIF
/// orientation (1 without EXIF), a RAW in `config.raw_tone`, and an
/// untagged JPEG with the profile its EXIF calls for (see
/// [`ColourHint::from_exif`]). The warnings are by file id. `progress` gets
/// one [`Stage::Write`] tick per member, with that member's file, as
/// [`write_stack`] calls `on_layer`.
///
/// # Panics
///
/// If `group` is not an index of `analysis.groups`.
pub fn write_group(
    config: &JobConfig,
    analysis: &Analysis,
    group: usize,
    opts: WriteOptions,
    progress: &(dyn Fn(Progress) + Sync),
    cancel: &AtomicBool,
) -> Result<(WriteOutcome, Vec<(usize, Warning)>), WriteError> {
    let output = ensure_output_dir(config)?;
    let members = &analysis.groups[group].members;
    let ticker = Ticker::new(progress, Stage::Write, members.len(), 0);
    let on_file = |id: usize| ticker.tick(&analysis.files[id].path);
    let writer = GroupWriter {
        output: &output,
        config,
        files: &analysis.files,
        opts,
        cancel,
    };
    writer.write(members, &on_file, &|_| {})
}

/// Writes every group of `analysis`, one after the other, as
/// [`write_groups`] does.
///
/// `progress` gets [`Stage::Write`] ticks as [`write_group`] gives them,
/// counted across all the groups: the total is the number of members of all
/// the groups. After a group fails, its members not yet ticked are ticked
/// then, so `done` reaches `total` unless the writing is cancelled.
pub fn write_all(
    config: &JobConfig,
    analysis: &Analysis,
    opts: WriteOptions,
    progress: &(dyn Fn(Progress) + Sync),
    cancel: &AtomicBool,
) -> Result<WriteReport, WriteError> {
    let groups: Vec<Vec<usize>> = analysis.groups.iter().map(|g| g.members.clone()).collect();
    let total = groups.iter().map(Vec::len).sum();
    let observer = Ticking {
        files: &analysis.files,
        ticker: Ticker::new(progress, Stage::Write, total, 0),
    };
    write_groups(config, &analysis.files, &groups, opts, &observer, cancel)
}

/// Writes the stack of each of `groups` (file ids of `files`, top layer
/// first), one after the other, into the output folder of `config`, which
/// is checked once, first ([`ensure_output_dir`]).
///
/// A group goes to the output folder joined with the folder of its first
/// member relative to `config.input_dir`. Members of a group share their
/// folder (spec §5.1), so a group of the top level goes to the output
/// folder itself, and in a recursive run the output mirrors the input's
/// subfolders. The subfolder is created if missing, one component at a
/// time, and each one must resolve inside the output folder: a link out of
/// it fails the group with [`WriteError::SubfolderOutside`] before anything
/// is created past the link.
///
/// It fails only when the folder is unusable ([`WriteError::OutputDir`]) or
/// on a cancel; a group that fails otherwise has its error in
/// [`WriteReport::outcomes`], and the next group is written. `observer`
/// hears of every member, stack and group as [`WriteObserver`] describes;
/// after a group fails, its members not yet reported are reported then.
/// `cancel` is also checked before each group.
///
/// # Panics
///
/// If a file id is not an index of `files`.
pub fn write_groups(
    config: &JobConfig,
    files: &[FileRecord],
    groups: &[Vec<usize>],
    opts: WriteOptions,
    observer: &dyn WriteObserver,
    cancel: &AtomicBool,
) -> Result<WriteReport, WriteError> {
    let output = ensure_output_dir(config)?;
    tracing::info!(
        output = %output.display(),
        stacks = groups.len(),
        "write: writing the stacks"
    );
    let writer = GroupWriter {
        output: &output,
        config,
        files,
        opts,
        cancel,
    };
    let mut report = WriteReport {
        outcomes: Vec::with_capacity(groups.len()),
        warnings: Vec::new(),
    };
    for (group, members) in groups.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(WriteError::Cancelled);
        }
        let reported = RefCell::new(Vec::with_capacity(members.len()));
        let on_file = |id: usize| {
            reported.borrow_mut().push(id);
            observer.layer(id);
        };
        let on_partial = |partial: &Path| observer.partial_created(group, partial);
        let (outcome, warnings) = match writer.write(members, &on_file, &on_partial) {
            Ok((outcome, warnings)) => (Ok(outcome), warnings),
            Err(WriteError::Cancelled) => return Err(WriteError::Cancelled),
            Err(error) => {
                // The members the failure kept from being reached still
                // count, so a progress count reaches its total.
                let reported = reported.borrow();
                for &id in members.iter().filter(|id| !reported.contains(id)) {
                    observer.layer(id);
                }
                (Err(error.to_string()), Vec::new())
            }
        };
        log_unwritten(files, members, &outcome);
        observer.group_done(group, &outcome, &warnings);
        report.outcomes.push((group, outcome));
        report.warnings.extend(warnings);
    }
    Ok(report)
}

/// Logs at `info` a stack of `members` that was skipped or failed, by its
/// first photo; [`write_stack`] logs a stack written.
fn log_unwritten(files: &[FileRecord], members: &[usize], outcome: &Result<WriteOutcome, String>) {
    // Called only when the event is logged.
    let first = || {
        members
            .first()
            .map(|&id| files[id].path.display().to_string())
            .unwrap_or_default()
    };
    match outcome {
        Ok(WriteOutcome::Written { .. }) => {}
        Ok(WriteOutcome::Skipped { reason }) => {
            tracing::info!(first = %first(), %reason, "stack skipped");
        }
        Err(error) => tracing::info!(first = %first(), %error, "stack failed"),
    }
}

/// The observer of [`write_all`]: one [`Stage::Write`] tick per member,
/// with its file.
struct Ticking<'a> {
    files: &'a [FileRecord],
    ticker: Ticker<'a>,
}

impl WriteObserver for Ticking<'_> {
    fn layer(&self, file: usize) {
        self.ticker.tick(&self.files[file].path);
    }
}

/// Writes the stacks of one job into its checked output folder.
struct GroupWriter<'a> {
    /// The canonical output folder, from [`ensure_output_dir`].
    output: &'a Path,
    config: &'a JobConfig,
    files: &'a [FileRecord],
    opts: WriteOptions,
    cancel: &'a AtomicBool,
}

impl GroupWriter<'_> {
    /// Writes the stack of `members` (file ids, top layer first) into the
    /// output subfolder of its first member ([`output_subfolder`]). It calls
    /// `on_file` with the file id of each member as [`write_stack`] calls
    /// `on_layer`, and `on_partial` as [`write_stack_with`] does. The
    /// warnings are by file id.
    fn write(
        &self,
        members: &[usize],
        on_file: &dyn Fn(usize),
        on_partial: &dyn Fn(&Path),
    ) -> Result<(WriteOutcome, Vec<(usize, Warning)>), WriteError> {
        let dir = match members.first() {
            Some(&first) => {
                output_subfolder(self.output, &self.config.input_dir, &self.files[first].path)?
            }
            None => self.output.to_path_buf(),
        };
        let inputs = members
            .iter()
            .map(|&id| file_input(&self.files[id], self.config.raw_tone))
            .collect();
        let on_layer = |i: usize| on_file(members[i]);
        let (outcome, warnings) = write_stack_with(
            &dir,
            self.config.output_format,
            self.opts,
            inputs,
            &on_layer,
            on_partial,
            self.cancel,
        )?;
        let warnings = warnings
            .into_iter()
            .map(|(i, warning)| (members[i], warning))
            .collect();
        Ok((outcome, warnings))
    }
}

/// The folder for the stack whose first photo is `first`: `output`, the
/// canonical output folder, joined with the folder of `first` relative to
/// `input_dir`, created if missing, and canonical. A photo directly in
/// `input_dir` gets `output` itself.
///
/// Each component is created, then resolved, in turn, and must resolve
/// inside `output`; otherwise (a link out of it, or a `..`) the result is
/// [`WriteError::SubfolderOutside`], and nothing is created past that
/// component. A `first` that is not in `input_dir` is an `InvalidInput`
/// [`WriteError::Io`].
fn output_subfolder(output: &Path, input_dir: &Path, first: &Path) -> Result<PathBuf, WriteError> {
    let rel = first
        .strip_prefix(input_dir)
        .ok()
        .and_then(Path::parent)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} is not in the input folder {}",
                    first.display(),
                    input_dir.display()
                ),
            )
        })?;
    let outside = || WriteError::SubfolderOutside {
        rel: rel.to_path_buf(),
    };
    let mut folder = output.to_path_buf();
    for component in rel.components() {
        let Component::Normal(name) = component else {
            return Err(outside());
        };
        let next = folder.join(name);
        let failed = |what: &str, e: io::Error| {
            io::Error::new(
                e.kind(),
                format!("the output subfolder {} {what}: {e}", next.display()),
            )
        };
        match fs::create_dir(&next) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => {
                return Err(failed("cannot be created", e).into());
            }
            _ => {}
        }
        folder = next
            .canonicalize()
            .map_err(|e| failed("cannot be resolved", e))?;
        if !folder.starts_with(output) {
            return Err(outside());
        }
    }
    Ok(folder)
}

/// The layer input of the file of `record`: probed now, decoded when loaded.
fn file_input(record: &FileRecord, tone: RawTone) -> StackInput {
    let path = record.path.clone();
    let decoding = MemberDecoding::of(record.kind, record.exif.as_ref(), tone);
    StackInput {
        stem: file_stem(&path),
        exif: record.exif.clone(),
        probe: decoding.probe(&path),
        load: Box::new(move || decoding.full(&path)),
    }
}

/// How a member file is probed and decoded. [`file_input`] and
/// [`predict_stack`] both take it from [`MemberDecoding::of`], so a
/// prediction probes each file exactly as the writer does.
#[derive(Debug, Clone, Copy)]
struct MemberDecoding {
    kind: FileKind,
    orientation: u16,
    tone: RawTone,
    hint: ColourHint,
}

impl MemberDecoding {
    /// The decoding of a file of `kind` with `exif`: as its kind, upright by
    /// its EXIF orientation (1 without EXIF), a RAW in `tone`, and an
    /// untagged JPEG with the profile its EXIF calls for (see
    /// [`ColourHint::from_exif`]).
    fn of(kind: FileKind, exif: Option<&ExifData>, tone: RawTone) -> Self {
        Self {
            kind,
            orientation: exif.map_or(1, |exif| exif.orientation),
            tone,
            hint: ColourHint::from_exif(exif),
        }
    }

    fn probe(self, path: &Path) -> Result<Probe, DecodeError> {
        decode::probe(path, self.kind, self.orientation, self.tone, self.hint)
    }

    fn full(self, path: &Path) -> Result<FullImage, DecodeError> {
        decode::full(path, self.kind, self.orientation, self.tone, self.hint)
    }
}

/// The layer stem of the file at `path`: its file name without the last
/// extension, empty when it has none (which [`is_file_name`] refuses).
fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `stem` can stand for a file name in a folder: `dir.join(stem)` names an
/// entry of `dir`. It must be exactly one normal path component equal to
/// itself, which refuses the empty stem, `.`, `..`, any separator of the
/// platform and, on Windows, a prefix (`C:`). On Windows `:` is refused
/// anywhere, since `a:b` would name the NTFS stream `b` of the file `a`.
fn is_file_name(stem: &str) -> bool {
    if cfg!(windows) && stem.contains(':') {
        return false;
    }
    let mut components = Path::new(stem).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(name)), None) => name == OsStr::new(stem),
        _ => false,
    }
}

fn unreadable(reason: String) -> Warning {
    Warning::LayerUnreadable { reason }
}

fn skipped(usable: usize, total: usize) -> WriteOutcome {
    WriteOutcome::Skipped {
        reason: format!(
            "{usable} of {total} photos could be read at the canvas resolution; \
             a stack needs at least {MIN_LAYERS}"
        ),
    }
}

/// `warnings` sorted by input position; each input's own order is kept.
fn by_input(mut warnings: Vec<(usize, Warning)>) -> Vec<(usize, Warning)> {
    warnings.sort_by_key(|&(i, _)| i);
    warnings
}

/// `image` when it is what its kept member's `probe` said, and so fits the
/// canvas; otherwise what changed since the probe.
fn unchanged(image: FullImage, probe: &Probe, canvas: &CanvasProfile) -> Result<FullImage, String> {
    let sixteen_bit = matches!(image.samples, Samples::U16(_));
    let has_alpha = image.channels == 4;
    // A kept member's probe has the canvas size, so this compares both.
    let change = if (image.width, image.height) != (canvas.width, canvas.height) {
        format!(
            "it is now {}×{}, the canvas {}×{}",
            image.width, image.height, canvas.width, canvas.height
        )
    } else if sixteen_bit != probe.sixteen_bit {
        format!("it is now {}-bit", if sixteen_bit { 16 } else { 8 })
    } else if has_alpha != probe.has_alpha {
        if has_alpha {
            "it now has alpha".to_owned()
        } else {
            "it no longer has alpha".to_owned()
        }
    } else if image.icc != probe.icc {
        "its colour profile is different".to_owned()
    } else {
        return Ok(image);
    };
    Err(format!("the file changed since it was probed: {change}"))
}

/// The integer widths of `container`'s layer data.
fn widths(container: Container) -> LengthWidth {
    match container {
        Container::Psd | Container::Tiff => LengthWidth::Psd,
        Container::Psb | Container::BigTiff => LengthWidth::Psb,
    }
}

/// Opens the partial and the spill files at the first provisional name,
/// `<stem>_<kept>.<ext>` at attempt 0, 1, ..., whose three files are all
/// free. A taken one (`AlreadyExists`) is never touched: the partial just
/// created for that attempt is dropped, which removes it, and the next
/// attempt is tried.
fn open_provisional(
    dir: &Path,
    stem: &str,
    canvas: &CanvasProfile,
    opts: WriteOptions,
) -> io::Result<(PartialFile, LayerEncoder)> {
    for attempt in 0..NAME_ATTEMPTS {
        let name = output_name(stem, canvas.kept.len(), canvas.container, attempt);
        let path = dir.join(name);
        let partial = match PartialFile::create(&path) {
            Ok(partial) => partial,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        match LayerEncoder::new(&path, canvas, widths(canvas.container), opts) {
            Ok(encoder) => return Ok((partial, encoder)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(no_free_name(stem, canvas.kept.len(), canvas.container))
}

/// Persists `partial` with no clobber under the first free final name,
/// `<stem>_<layers>.<ext>` at attempt 0, 1, ..., then syncs `dir` so the
/// name survives a crash. A failed folder sync is logged, not returned
/// ([`sync_dir_or_warn`]): the stack is complete under its final name.
fn persist(
    partial: PartialFile,
    dir: &Path,
    stem: &str,
    layers: usize,
    container: Container,
) -> io::Result<PathBuf> {
    persist_with(partial, dir, stem, layers, container, sync_dir)
}

/// [`persist`], syncing the folder with `sync`: [`sync_dir`], or in tests a
/// sync that fails.
fn persist_with(
    mut partial: PartialFile,
    dir: &Path,
    stem: &str,
    layers: usize,
    container: Container,
    sync: SyncDir,
) -> io::Result<PathBuf> {
    for attempt in 0..NAME_ATTEMPTS {
        let path = dir.join(output_name(stem, layers, container, attempt));
        match partial.persist_no_clobber(&path) {
            Ok(path) => {
                sync_dir_or_warn(dir, sync);
                return Ok(path);
            }
            Err(PersistError { error, file }) if error.kind() == io::ErrorKind::AlreadyExists => {
                partial = file;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(no_free_name(stem, layers, container))
}

fn no_free_name(stem: &str, layers: usize, container: Container) -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "{} and its next {} numbered names are all taken",
            output_name(stem, layers, container, 0),
            NAME_ATTEMPTS - 1
        ),
    )
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn persist_succeeds_when_the_folder_sync_fails() {
        // The stack is complete under its final name once linked; the
        // failed sync is only logged.
        let dir = tempfile::tempdir().unwrap();
        let mut partial = PartialFile::create(&dir.path().join("a_2.psd")).unwrap();
        partial.file().write_all(b"stack").unwrap();
        let fails: SyncDir = |_| Err(io::Error::other("injected sync failure"));
        let path = persist_with(partial, dir.path(), "a", 2, Container::Psd, fails).unwrap();
        assert_eq!(path, dir.path().join("a_2.psd"));
        assert_eq!(fs::read(&path).unwrap(), b"stack");
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["a_2.psd"]);
    }

    #[test]
    fn a_file_name_is_one_normal_component() {
        for stem in ["IMG_0001", "a b", ".hidden", "a.b", "a..b", "…", "IMG (1)"] {
            assert!(is_file_name(stem), "{stem:?}");
        }
        for stem in ["", ".", "..", "a/b", "/a", "a/", "./a", "a/.", "../a", "/"] {
            assert!(!is_file_name(stem), "{stem:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn on_unix_a_backslash_or_a_colon_is_an_ordinary_character() {
        for stem in [r"a\b", r"..\a", r"\", "a:b", "C:"] {
            assert!(is_file_name(stem), "{stem:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_a_backslash_a_prefix_or_a_colon_is_refused() {
        for stem in [
            r"a\b",
            r"..\a",
            r"\",
            r"a\",
            "C:",
            "C:a",
            "a:b",
            "a:",
            ":",
            r"\\?\C:",
            r"\\server\share",
        ] {
            assert!(!is_file_name(stem), "{stem:?}");
        }
    }
}
