//! `photostacker run` (spec §10): one job on an engine of the CLI's own.
//!
//! The job is submitted without `auto_confirm`, so it stops in Review
//! Pending once scanned (Stage 3 included). Its plan is then built from the
//! analysis the engine stored: a dry run prints or writes it and ends there.
//! A run that writes writes the plan file first, names the per-file errors,
//! then confirms every group and writes them through the engine. While the
//! job runs, each stage it enters prints one line, and the engine's warnings
//! print as `warning:` lines.
//!
//! The engine's database is a temporary one, removed at exit, unless `--db`
//! names one; the review thumbnails go beside it. The models are the app's
//! (spec §7.1), shared with the GUI.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow};
use clap::{Args, ValueEnum};
use photostacker_core::analyze::FileRecord;
use photostacker_core::engine::{GroupView, JobId};
use photostacker_core::fsio::write_replace;
use photostacker_core::model::{GroupStatus, JobStatus, OutputFormat, RawJpegPreference, RawTone};
use photostacker_core::{
    AppDirs, Engine, EngineError, EngineEvent, EngineOptions, JobConfig, PlanV1, Progress, Stage,
};

use crate::{Failure, interrupted};

/// The Stage 3 model of a run without `--model` (spec §8.1).
pub(crate) const DEFAULT_MODEL: &str = "dinov2-small";

/// How long the run waits for an engine event before it looks for a
/// Ctrl+C again.
const POLL: Duration = Duration::from_millis(100);

/// The options of `run`. Each one left out keeps the `JobConfig` default.
#[derive(Args)]
pub(crate) struct RunArgs {
    /// The folder of photos.
    input: PathBuf,
    /// Where the stacks go [default: <INPUT>/stacked].
    #[arg(short, long, value_name = "OUTPUT")]
    output: Option<PathBuf>,
    /// The layered file format [default: psd].
    #[arg(long, value_enum)]
    format: Option<FormatArg>,
    /// Also scan the subfolders.
    #[arg(long)]
    recursive: bool,
    /// Stage 2: the largest pHash+dHash distance that links two photos [default: 8].
    #[arg(long, value_name = "N")]
    hamming: Option<u32>,
    /// Stage 1: the longest gap, in seconds, between shots of one series [default: 30].
    #[arg(long, value_name = "S")]
    window: Option<u32>,
    /// Stage 3 checks a group whose largest hash distance is above this [default: 6].
    #[arg(long = "ml-floor", value_name = "N")]
    ml_floor: Option<u32>,
    /// Stage 3: the cosine similarity that keeps two photos together, 0.5 to 0.99 [default: the model's].
    #[arg(long, value_name = "F")]
    cosine: Option<f32>,
    /// Turn Stage 3 off.
    #[arg(long)]
    no_ml: bool,
    /// The Stage 3 model, by its id (see `models list`); Stage 3 runs only once it is installed.
    #[arg(long, value_name = "ID", default_value = DEFAULT_MODEL, conflicts_with = "no_ml")]
    model: String,
    /// Which file of a RAW+JPEG pair is stacked [default: raw].
    #[arg(long = "raw-jpeg", value_enum)]
    raw_jpeg: Option<RawJpegArg>,
    /// How RAW files are developed: sRGB, or linear light for HDR merging [default: srgb].
    #[arg(long = "raw-tone", value_enum)]
    raw_tone: Option<RawToneArg>,
    /// Analyse only: write the plan and no stacks.
    #[arg(long)]
    dry_run: bool,
    /// Also write the plan JSON to FILE; a dry run without it prints the plan to stdout.
    #[arg(long, value_name = "FILE")]
    plan: Option<PathBuf>,
    /// Keep the job in this database, and its review thumbnails in FILE.thumbs [default: a temporary one].
    #[arg(long, value_name = "FILE")]
    db: Option<PathBuf>,
}

/// `--format`; the core enum stays free of clap.
#[derive(Clone, Copy, ValueEnum)]
enum FormatArg {
    Psd,
    Tiff,
}

/// `--raw-jpeg`; the core enum stays free of clap.
#[derive(Clone, Copy, ValueEnum)]
enum RawJpegArg {
    Raw,
    Jpeg,
}

/// `--raw-tone`; the core enum stays free of clap.
#[derive(Clone, Copy, ValueEnum)]
enum RawToneArg {
    Srgb,
    Linear,
}

impl RunArgs {
    /// The run's job config, as its plan records it: the defaults,
    /// overridden by the options given. A run that writes confirms every
    /// group, so it has `auto_confirm`; a dry run has not. Not validated yet.
    fn job_config(&self) -> JobConfig {
        let mut config = JobConfig::new(self.input.clone());
        if let Some(output) = &self.output {
            config.output_dir = output.clone();
        }
        if let Some(format) = self.format {
            config.output_format = match format {
                FormatArg::Psd => OutputFormat::Psd,
                FormatArg::Tiff => OutputFormat::Tiff,
            };
        }
        config.recursive = self.recursive;
        if let Some(hamming) = self.hamming {
            config.hamming_threshold = hamming;
        }
        if let Some(window) = self.window {
            config.time_window_seconds = window;
        }
        if let Some(floor) = self.ml_floor {
            config.stage3_ambiguity_floor = floor;
        }
        config.cosine_threshold = self.cosine;
        config.ml_model = (!self.no_ml).then(|| self.model.clone());
        if let Some(preference) = self.raw_jpeg {
            config.raw_jpeg_preference = match preference {
                RawJpegArg::Raw => RawJpegPreference::Raw,
                RawJpegArg::Jpeg => RawJpegPreference::Jpeg,
            };
        }
        if let Some(tone) = self.raw_tone {
            config.raw_tone = match tone {
                RawToneArg::Srgb => RawTone::Srgb,
                RawToneArg::Linear => RawTone::Linear,
            };
        }
        config.auto_confirm = !self.dry_run;
        config
    }
}

pub(crate) fn run(args: &RunArgs) -> Result<(), Failure> {
    let config = args.job_config();
    config
        .validate()
        .map_err(|e| Failure::Usage(e.to_string()))?;
    let session = Session::start(args.db.as_deref())?;
    let engine = &session.engine;
    // The engine waits in Review Pending, so the plan file and the error
    // lines come before any stack.
    let job = engine
        .submit(JobConfig {
            auto_confirm: false,
            ..config.clone()
        })
        .map_err(submit_failure)?;
    let mut watch = Watch::new(engine, job);

    match watch.wait_for(&[
        JobStatus::ReviewPending,
        JobStatus::Failed,
        JobStatus::Cancelled,
    ])? {
        JobStatus::ReviewPending => {}
        JobStatus::Failed => {
            return Err(Failure::Runtime(anyhow!(
                "cannot analyse {}: {}",
                config.input_dir.display(),
                job_error(engine, job)
            )));
        }
        _ => return Err(cancelled()),
    }
    // A Ctrl+C that came as the scan ended finds nothing to cancel.
    if interrupted() {
        return Err(cancelled());
    }
    let analysis = engine.analysis(job).map_err(engine_failure)?;
    let plan = PlanV1::from_analysis(&config, &analysis);
    let found = format!(
        "{} files, {} groups, {} unstacked, {} errors",
        analysis.files.len(),
        plan.groups.len(),
        plan.unstacked.len(),
        plan.errors.len()
    );

    if args.dry_run {
        eprintln!("{found}");
        let json = plan_json(&plan).map_err(Failure::Runtime)?;
        return write_plan(args.plan.as_deref(), &json).map_err(Failure::Runtime);
    }
    // The plan file is written before any stack, so a failure to write it
    // stops the run with nothing written.
    if let Some(file) = &args.plan {
        let json = plan_json(&plan).map_err(Failure::Runtime)?;
        write_plan(Some(file), &json).map_err(Failure::Runtime)?;
    }
    // The plan file is optional here, so the per-file errors are named too.
    for error in &plan.errors {
        eprintln!("error: {}: {}", error.file.display(), error.error);
    }

    // With no group the write writes nothing, and the output folder is
    // neither created nor checked.
    engine.confirm_all(job).map_err(engine_failure)?;
    engine.write(job).map_err(engine_failure)?;
    let status = watch.wait_for(&[
        JobStatus::Completed,
        JobStatus::Failed,
        JobStatus::Cancelled,
    ])?;
    let groups = engine.groups(job).map_err(engine_failure)?;
    let report = Report::of(&analysis.files, &groups);
    for line in &report.lines {
        eprintln!("{line}");
    }
    // A write that fails does so before its first stack (an unusable output
    // folder); should one fail later, the stacks it wrote are still named.
    if status == JobStatus::Failed {
        print_paths(&report.written).map_err(Failure::Runtime)?;
        return Err(Failure::Runtime(anyhow!(
            "cannot write the stacks: {}",
            job_error(engine, job)
        )));
    }
    eprintln!(
        "{found}; {} written, {} skipped, {} failed",
        report.written.len(),
        report.skipped,
        report.failed
    );
    print_paths(&report.written).map_err(Failure::Runtime)?;
    if status == JobStatus::Cancelled {
        return Err(cancelled());
    }
    if report.failed > 0 {
        return Err(Failure::Runtime(anyhow!(
            "{} of {} stacks could not be written",
            report.failed,
            report.written.len() + report.skipped + report.failed
        )));
    }
    Ok(())
}

/// The run's engine, and the temporary folder that holds its database
/// without `--db`. The fields drop in order: the engine shuts down before
/// the folder is removed.
struct Session {
    engine: Engine,
    _scratch: tempfile::TempDir,
}

impl Session {
    /// Starts the engine on the database `db`, with its review thumbnails in
    /// `<db>.thumbs/`; or, without one, on a database in a new temporary
    /// folder, with the thumbnails there too. Never the app's own database
    /// or thumbnails, which belong to the GUI: a job id of the CLI's could
    /// name one of its jobs. The models are the app's; with no app folder
    /// (no home folder) there are none: the empty temporary folder stands
    /// in for theirs.
    fn start(db: Option<&Path>) -> Result<Session, Failure> {
        let scratch = tempfile::Builder::new()
            .prefix("photostacker-")
            .tempdir()
            .context("cannot create a temporary folder for the job database")
            .map_err(Failure::Runtime)?;
        let (db, thumbs_dir) = match db {
            Some(db) => {
                let mut thumbs = db.as_os_str().to_owned();
                thumbs.push(".thumbs");
                (db.to_path_buf(), PathBuf::from(thumbs))
            }
            None => (
                scratch.path().join("photostacker.db"),
                scratch.path().join("thumbs"),
            ),
        };
        let models_dir = match AppDirs::resolve() {
            Some(app) => app.models(),
            None => scratch.path().join("models"),
        };
        let engine = Engine::start(EngineOptions {
            db: db.clone(),
            thumbs_dir,
            models_dir,
            embedder: None,
            installer: None,
            wake: Arc::new(|| {}),
        })
        .map_err(|error| match error {
            // It names the database already.
            EngineError::Busy(_) => Failure::Runtime(anyhow!(error)),
            other => Failure::Runtime(
                anyhow!(other).context(format!("cannot open the job database {}", db.display())),
            ),
        })?;
        Ok(Session {
            engine,
            _scratch: scratch,
        })
    }
}

/// Follows one job through the engine's events: prints a line each time
/// it enters a new stage, never one per file, and each warning; cancels it
/// at the first Ctrl+C.
struct Watch<'a> {
    engine: &'a Engine,
    job: JobId,
    /// The last stage printed.
    stage: Option<Stage>,
    /// The job was cancelled on a Ctrl+C.
    cancelled: bool,
}

impl<'a> Watch<'a> {
    fn new(engine: &'a Engine, job: JobId) -> Self {
        Self {
            engine,
            job,
            stage: None,
            cancelled: false,
        }
    }

    /// Waits for the job to reach one of `statuses`, and returns it.
    fn wait_for(&mut self, statuses: &[JobStatus]) -> Result<JobStatus, Failure> {
        // Every receiver shares the one channel: nothing else takes events.
        let events = self.engine.events();
        loop {
            if interrupted() && !self.cancelled {
                self.cancelled = true;
                self.engine.cancel(self.job).map_err(engine_failure)?;
            }
            // The engine keeps a sender, so the channel never disconnects:
            // an error is a timeout. A worker that dies says so instead.
            if let Ok(event) = events.recv_timeout(POLL)
                && let Some(status) = self.see(event)?
                && statuses.contains(&status)
            {
                return Ok(status);
            }
        }
    }

    /// Prints what `event` says of the job; gives the job's new status, or
    /// the failure of an engine whose worker has stopped.
    fn see(&mut self, event: EngineEvent) -> Result<Option<JobStatus>, Failure> {
        match event {
            EngineEvent::JobStatusChanged { job, status } if job == self.job => Ok(Some(status)),
            EngineEvent::JobProgress { job, progress } if job == self.job => {
                self.progress(&progress);
                Ok(None)
            }
            EngineEvent::Warning { job, message } if job.is_none_or(|job| job == self.job) => {
                eprintln!("warning: {message}");
                Ok(None)
            }
            EngineEvent::EngineStopped { message } => Err(Failure::Runtime(anyhow!(
                "the engine has stopped: {message}"
            ))),
            // An engine error is logged by the engine, and a job's failure
            // is its status, with its error. A file that fails is named
            // from the plan once the scan is done, and a run swaps nothing,
            // so `FileFailed` and `FilesChanged` say nothing more.
            _ => Ok(None),
        }
    }

    fn progress(&mut self, progress: &Progress) {
        if self.stage != Some(progress.stage) {
            self.stage = Some(progress.stage);
            eprintln!("{}", stage_line(progress.stage, progress.total));
        }
    }
}

/// The line printed when the run enters `stage`, of `total` items.
fn stage_line(stage: Stage, total: usize) -> String {
    match stage {
        Stage::Discover => format!("discover: {total} files found"),
        Stage::Metadata => format!("metadata: reading {total} paired files"),
        Stage::Hash => format!("hash: hashing {total} files"),
        Stage::Embed => format!("embed: embedding {total} files"),
        Stage::Group => format!("group: grouped {total} files"),
        Stage::Write => format!("write: writing {total} layers"),
    }
}

fn cancelled() -> Failure {
    Failure::Runtime(anyhow!("cancelled"))
}

/// The error the engine stored for `job`'s failure.
fn job_error(engine: &Engine, job: JobId) -> String {
    match engine.job(job) {
        Ok(view) => view.error.unwrap_or_else(|| "no reason given".into()),
        Err(error) => format!("its error cannot be read: {error}"),
    }
}

fn submit_failure(error: EngineError) -> Failure {
    match error {
        EngineError::Config(error) => Failure::Usage(error.to_string()),
        EngineError::NotUtf8(path) => Failure::Runtime(anyhow!(
            "the path {} is not valid UTF-8, which a job needs",
            path.display()
        )),
        other => engine_failure(other),
    }
}

fn engine_failure(error: EngineError) -> Failure {
    Failure::Runtime(anyhow!(error).context("the engine failed"))
}

/// What the write of a job's groups came to.
struct Report<'a> {
    /// One line per stack skipped or failed, naming the first photo of its
    /// group, in group order; then one per warning of the writing.
    lines: Vec<String>,
    /// The stacks written, in group order.
    written: Vec<&'a Path>,
    skipped: usize,
    failed: usize,
}

impl<'a> Report<'a> {
    /// The report of `groups`, by position, whose members are ids of `files`.
    fn of(files: &[FileRecord], groups: &'a [GroupView]) -> Self {
        let mut report = Report {
            lines: Vec::new(),
            written: Vec::new(),
            skipped: 0,
            failed: 0,
        };
        for group in groups {
            // A group has at least two members, and the store keeps every
            // member and warning an id of the job's files.
            let first = files[group.members[0]].path.display();
            let reason = group.error.as_deref().unwrap_or_default();
            match (group.status, &group.output_path) {
                (GroupStatus::Written, Some(path)) => report.written.push(path),
                (GroupStatus::Skipped, _) => {
                    report.skipped += 1;
                    report
                        .lines
                        .push(format!("stack of {first} skipped: {reason}"));
                }
                (GroupStatus::Failed, _) => {
                    report.failed += 1;
                    report
                        .lines
                        .push(format!("stack of {first} failed: {reason}"));
                }
                _ => {}
            }
        }
        for group in groups {
            for (file, warning) in &group.warnings {
                let file = files[*file].path.display();
                report.lines.push(format!("warning: {file}: {warning}"));
            }
        }
        report
    }
}

/// Prints each path on its own line to stdout. On Unix a path is printed as
/// its bytes, so one that is not UTF-8 still names its file.
fn print_paths(paths: &[&Path]) -> anyhow::Result<()> {
    let mut stdout = io::stdout().lock();
    paths
        .iter()
        .try_for_each(|path| {
            stdout.write_all(&path_bytes(path))?;
            stdout.write_all(b"\n")
        })
        .and_then(|()| stdout.flush())
        .context("cannot write the stack paths to stdout")
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

/// The plan as pretty JSON with a final newline. JSON strings are UTF-8, so
/// a path that is not fails the serialisation; the error then names it.
fn plan_json(plan: &PlanV1) -> anyhow::Result<String> {
    match serde_json::to_string_pretty(plan) {
        Ok(mut json) => {
            json.push('\n');
            Ok(json)
        }
        Err(error) => Err(match non_utf8_path(plan) {
            Some(path) => anyhow!(
                "cannot write the plan as JSON: the path {} is not valid UTF-8",
                path.display()
            ),
            None => anyhow!(error).context("cannot write the plan as JSON"),
        }),
    }
}

/// The first path of `plan` that is not valid UTF-8.
fn non_utf8_path(plan: &PlanV1) -> Option<&Path> {
    let config = [
        &plan.input_dir,
        &plan.config.input_dir,
        &plan.config.output_dir,
    ];
    let groups = plan.groups.iter().flat_map(|g| &g.files);
    let paired = plan.paired.iter().flat_map(|p| [&p.passive, &p.active]);
    let errors = plan.errors.iter().map(|e| &e.file);
    let warnings = plan.warnings.iter().map(|w| &w.file);
    config
        .into_iter()
        .chain(groups)
        .chain(&plan.unstacked)
        .chain(paired)
        .chain(errors)
        .chain(warnings)
        .map(PathBuf::as_path)
        .find(|path| path.to_str().is_none())
}

/// Writes `json` to `file`, replacing it atomically, or to stdout without one.
fn write_plan(file: Option<&Path>, json: &str) -> anyhow::Result<()> {
    match file {
        Some(file) => write_replace(file, json.as_bytes())
            .with_context(|| format!("cannot write the plan to {}", file.display())),
        None => {
            let mut stdout = io::stdout().lock();
            stdout
                .write_all(json.as_bytes())
                .and_then(|()| stdout.flush())
                .context("cannot write the plan to stdout")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Command};
    use clap::{CommandFactory, Parser};
    use photostacker_core::model::{FileKind, GroupOrigin, MlStatus, Warning};
    use photostacker_core::models;

    fn args_of(argv: &[&str]) -> RunArgs {
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Run(args) => args,
            Command::Models(_) => unreachable!("a run"),
        }
    }

    /// Writing confirms every group (`auto_confirm`); a dry run confirms
    /// none. Stage 3 uses the default model.
    #[test]
    fn options_left_out_keep_the_job_config_defaults() {
        let defaults = JobConfig {
            ml_model: Some(DEFAULT_MODEL.into()),
            ..JobConfig::new("/in".into())
        };
        assert_eq!(
            args_of(&["photostacker", "run", "/in"]).job_config(),
            JobConfig {
                auto_confirm: true,
                ..defaults.clone()
            }
        );
        assert_eq!(
            args_of(&["photostacker", "run", "/in", "--dry-run"]).job_config(),
            defaults
        );
    }

    #[test]
    fn default_model_is_the_registry_default() {
        assert_eq!(models::MODELS[0].id, DEFAULT_MODEL);
    }

    /// The help text spells the defaults out; they must stay the config's.
    #[test]
    fn help_shows_the_job_config_defaults() {
        let help = Cli::command()
            .find_subcommand_mut("run")
            .unwrap()
            .render_help()
            .to_string();
        let line_of = |flag: &str| {
            help.lines()
                .find(|line| line.contains(flag))
                .unwrap_or_else(|| panic!("{flag} not in {help}"))
                .to_owned()
        };
        let defaults = JobConfig::new("/in".into());
        assert_eq!(defaults.output_format, OutputFormat::Psd);
        assert_eq!(defaults.raw_jpeg_preference, RawJpegPreference::Raw);
        assert_eq!(defaults.raw_tone, RawTone::Srgb);
        assert_eq!(defaults.cosine_threshold, None);
        for (flag, default) in [
            ("--hamming", defaults.hamming_threshold.to_string()),
            ("--window", defaults.time_window_seconds.to_string()),
            ("--ml-floor", defaults.stage3_ambiguity_floor.to_string()),
            ("--cosine", "the model's".into()),
            ("--model", DEFAULT_MODEL.into()),
            ("--format", "psd".into()),
            ("--raw-jpeg", "raw".into()),
            ("--raw-tone", "srgb".into()),
        ] {
            let line = line_of(flag);
            assert!(line.contains(&format!("[default: {default}]")), "{line}");
        }
    }

    /// Without `--window`, a run compares photos shot up to 30 s apart, as a new job of
    /// the app does (spec §5.2), and its help says so.
    #[test]
    fn the_window_defaults_to_30_seconds() {
        let args = args_of(&["photostacker", "run", "/in"]);
        assert_eq!(args.job_config().time_window_seconds, 30);
        let help = Cli::command()
            .find_subcommand_mut("run")
            .unwrap()
            .render_help()
            .to_string();
        let line = help.lines().find(|line| line.contains("--window")).unwrap();
        assert!(line.contains("[default: 30]"), "{line}");
    }

    #[test]
    fn options_override_the_job_config() {
        let args = args_of(&[
            "photostacker",
            "run",
            "/in",
            "-o",
            "/out",
            "--format",
            "tiff",
            "--recursive",
            "--hamming",
            "12",
            "--window",
            "9",
            "--ml-floor",
            "3",
            "--cosine",
            "0.9",
            "--model",
            "dinov2-base",
            "--raw-jpeg",
            "jpeg",
            "--raw-tone",
            "linear",
            "--dry-run",
            "--db",
            "/jobs.db",
        ]);
        let mut expected = JobConfig::new("/in".into());
        expected.output_dir = "/out".into();
        expected.output_format = OutputFormat::Tiff;
        expected.recursive = true;
        expected.hamming_threshold = 12;
        expected.time_window_seconds = 9;
        expected.stage3_ambiguity_floor = 3;
        expected.cosine_threshold = Some(0.9);
        expected.ml_model = Some("dinov2-base".into());
        expected.raw_jpeg_preference = RawJpegPreference::Jpeg;
        expected.raw_tone = RawTone::Linear;
        assert_eq!(args.job_config(), expected);
        assert_eq!(args.db.as_deref(), Some(Path::new("/jobs.db")));
    }

    /// `--no-ml` turns Stage 3 off, and naming a model with it is an error.
    #[test]
    fn no_ml_turns_stage_3_off() {
        let args = args_of(&["photostacker", "run", "/in", "--no-ml"]);
        assert_eq!(args.job_config().ml_model, None);
        assert!(
            Cli::try_parse_from(["photostacker", "run", "/in", "--no-ml", "--model", "x"]).is_err()
        );
    }

    fn file(path: &str) -> FileRecord {
        FileRecord {
            path: path.into(),
            kind: FileKind::Jpeg,
            size: 0,
            mtime_ms: 0,
            fingerprint: String::new(),
            exif: None,
            hashes: None,
            normalised: None,
            paired_with: None,
            error: None,
        }
    }

    /// A group of the files `members`, written to `output` or with `error`.
    fn group(
        members: [usize; 2],
        status: GroupStatus,
        output: Option<&str>,
        error: Option<&str>,
    ) -> GroupView {
        GroupView {
            id: 0,
            position: 0,
            status,
            origin: GroupOrigin::Auto,
            review_reset: false,
            members: members.to_vec(),
            bucket: String::new(),
            max_distance: 0,
            ml_status: MlStatus::None,
            ml_score: None,
            output_path: output.map(PathBuf::from),
            error: error.map(str::to_owned),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn report_counts_the_outcomes_and_lists_the_written_stacks_in_order() {
        let files: Vec<FileRecord> = (0..8).map(|i| file(&format!("/in/{i}.jpg"))).collect();
        let mut warned = group([6, 7], GroupStatus::Written, Some("/out/6_2.psd"), None);
        warned.warnings = vec![(7, Warning::IccMismatch)];
        let groups = [
            group([0, 1], GroupStatus::Written, Some("/out/0_2.psd"), None),
            group([2, 3], GroupStatus::Failed, None, Some("disk full")),
            group([4, 5], GroupStatus::Skipped, None, Some("1 of 2 photos")),
            warned,
        ];
        let report = Report::of(&files, &groups);
        assert_eq!((report.skipped, report.failed), (1, 1));
        assert_eq!(
            report.written,
            [Path::new("/out/0_2.psd"), Path::new("/out/6_2.psd")]
        );
        assert_eq!(report.lines.len(), 3, "{:?}", report.lines);
        assert_eq!(report.lines[0], "stack of /in/2.jpg failed: disk full");
        assert_eq!(report.lines[1], "stack of /in/4.jpg skipped: 1 of 2 photos");
        assert!(report.lines[2].starts_with("warning: /in/7.jpg: "));
    }

    /// A worker that dies ends the wait with its message, instead of the
    /// wait polling forever.
    #[test]
    fn a_stopped_engine_fails_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::start(EngineOptions {
            db: dir.path().join("jobs.db"),
            thumbs_dir: dir.path().join("thumbs"),
            models_dir: dir.path().join("models"),
            embedder: None,
            installer: None,
            wake: Arc::new(|| {}),
        })
        .unwrap();
        let mut watch = Watch::new(&engine, 1);
        let seen = watch.see(EngineEvent::EngineStopped {
            message: "a bug".into(),
        });
        match seen {
            Err(Failure::Runtime(error)) => {
                assert_eq!(format!("{error:#}"), "the engine has stopped: a bug");
            }
            _ => panic!("the run goes on"),
        }
    }

    #[test]
    fn non_utf8_path_is_found_only_when_present() {
        let plan = PlanV1::from_analysis(
            &JobConfig::new("/in".into()),
            &photostacker_core::Analysis {
                files: Vec::new(),
                groups: Vec::new(),
                unstacked: Vec::new(),
                warnings: Vec::new(),
                skipped: Vec::new(),
            },
        );
        assert_eq!(non_utf8_path(&plan), None);
        assert!(plan_json(&plan).is_ok());
    }
}
