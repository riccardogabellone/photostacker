//! The new-job form (spec §9.2): what the user has typed and chosen until the job is
//! submitted, and which field a refusal of the engine is about. Opened by the toolbar,
//! by Ctrl/Cmd+N, or by a folder dropped on the window; the engine's `JobConfig::validate`
//! is the one judge of what is valid, and its refusal shows under the field at fault.
//!
//! The form's own submissions are told apart from any other [`Request::Submit`] by their
//! tokens: the controller keeps them as [`Submission`]s and takes their answers in itself.
//!
//! A folder whose name is not valid Unicode never enters the form: the engine keeps a
//! job's folders as text, and the form's fields are text too.

use std::fs;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use photostacker_core::engine::JobId;
use photostacker_core::{ConfigError, EngineError, JobConfig};

use super::{Controller, NoticeLevel, View};
use crate::backend::Request;
use crate::format::{display_path, engine_error_text};
use crate::platform::Picked;
use crate::prefs::{self, FormDefaults};

/// The range of the `Hash distance` slider: what `JobConfig::validate` accepts.
pub const HAMMING_RANGE: RangeInclusive<u32> = 0..=40;
/// The range of the `Time window (s)` slider.
pub const WINDOW_RANGE: RangeInclusive<u32> = 1..=60;
/// The range of the `Visual check floor` slider.
pub const FLOOR_RANGE: RangeInclusive<u32> = 0..=20;
/// The range of the `Similarity` slider.
pub const COSINE_RANGE: RangeInclusive<f32> = 0.5..=0.99;

/// What the notice says when only the first of the folders dropped is used.
const FIRST_FOLDER_ONLY: &str = "Only the first folder was used: one job scans one folder.";
/// What the notice says when nothing dropped is a folder or in one.
const NOTHING_USABLE: &str =
    "Nothing dropped could be used: drop a folder of photos, or a photo in it.";
/// What the notice says, after the folder, about a folder whose name is not valid Unicode.
const NOT_UNICODE: &str = "Photostacker can only use folders whose names are valid Unicode.";
/// What the notice says when the desktop has no file dialog to pick a folder with.
const NO_FOLDER_DIALOG: &str =
    "No file dialog is available on this desktop. Type the folder's path instead.";
/// What the photo folder's error says when the field is empty.
const NO_INPUT: &str = "Choose the folder of photos.";

/// A field of the form, which a refusal can be about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Input,
    Output,
    Hamming,
    Window,
    Floor,
    Cosine,
    /// The model of the visual check, which the settings choose.
    Model,
}

/// The new-job form's state.
#[derive(Debug, Clone, PartialEq)]
pub struct JobForm {
    /// The photo folder's field, as typed.
    pub input: String,
    /// The output folder's field, as typed.
    pub output: String,
    /// The output follows `<input>/stacked` until the user edits it.
    pub output_edited: bool,
    /// Every other field: the format, the grouping, the RAWs and the similarity
    /// (`None` uses the model's own threshold). They start as the defaults of new jobs,
    /// which Settings edits with the same widgets.
    pub fields: FormDefaults,
    /// The last refusal: the field it is about, and what it says.
    pub error: Option<(Field, String)>,
    /// The form was submitted and the engine has not answered yet.
    pub submitting: bool,
}

impl JobForm {
    /// A form with `defaults`, on `input` when given. An `input` that is not valid
    /// Unicode would be taken lossily; the controller refuses one before it gets here.
    pub fn new(defaults: &FormDefaults, input: Option<&Path>) -> JobForm {
        let mut form = JobForm {
            input: String::new(),
            output: String::new(),
            output_edited: false,
            fields: defaults.clone(),
            error: None,
            submitting: false,
        };
        if let Some(input) = input {
            form.set_input(&input.to_string_lossy());
        }
        form
    }

    /// Takes `input` as the photo folder's field. The output follows it until edited.
    pub fn set_input(&mut self, input: &str) {
        self.input = input.to_owned();
        if !self.output_edited {
            self.output = default_output(folder_text(&self.input));
        }
        self.clear_folder_error();
    }

    /// Takes `output` as the output folder's field. Left empty, the output is the
    /// default, and follows the input again.
    pub fn set_output(&mut self, output: &str) {
        self.output = output.to_owned();
        self.output_edited = !folder_text(output).is_empty();
        self.clear_folder_error();
    }

    /// The job the form describes. Stage 3 runs with `model` when one is given (the
    /// controller's choice, [`Controller::ml_state`]); otherwise neither the model nor the
    /// similarity, which is then hidden, is sent.
    pub fn config(&self, model: Option<&str>) -> JobConfig {
        let mut config = JobConfig::new(PathBuf::from(folder_text(&self.input)));
        let output = folder_text(&self.output);
        if !output.is_empty() {
            config.output_dir = PathBuf::from(output);
        }
        let fields = &self.fields;
        config.output_format = fields.output_format;
        config.recursive = fields.recursive;
        config.auto_confirm = fields.auto_confirm;
        config.hamming_threshold = fields.hamming_threshold;
        config.time_window_seconds = fields.time_window_seconds;
        config.stage3_ambiguity_floor = fields.stage3_ambiguity_floor;
        config.raw_tone = fields.raw_tone;
        config.raw_jpeg_preference = fields.raw_jpeg_preference;
        if let Some(model) = model {
            config.ml_model = Some(model.to_owned());
            config.cosine_threshold = fields.cosine_threshold;
        }
        config
    }

    /// A refusal about a folder no longer holds once a folder changes.
    fn clear_folder_error(&mut self) {
        if matches!(self.error, Some((Field::Input | Field::Output, _))) {
            self.error = None;
        }
    }
}

/// The folder a field's text names: without the whitespace around it, nor one pair of
/// quotes, which a path copied from a file manager or a terminal may have.
fn folder_text(text: &str) -> &str {
    let text = text.trim();
    ['"', '\'']
        .iter()
        .find_map(|quote| text.strip_prefix(*quote)?.strip_suffix(*quote))
        .unwrap_or(text)
}

/// The output's text while it follows `input`: the engine's default, `<input>/stacked`.
fn default_output(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    let config = JobConfig::new(PathBuf::from(input));
    config.output_dir.to_string_lossy().into_owned()
}

/// Which field `error`, the engine's refusal of a job on `input`, is about; `None` when it
/// is about none (the engine stopped, the database failed).
pub fn field_of(error: &EngineError, input: &Path) -> Option<Field> {
    match error {
        EngineError::Config(config) => match config {
            ConfigError::InputNotADirectory(_) => Some(Field::Input),
            ConfigError::OutputEqualsInput | ConfigError::OutputNotADirectory(_) => {
                Some(Field::Output)
            }
            ConfigError::OutOfRange { field, .. } => match *field {
                "hamming_threshold" => Some(Field::Hamming),
                "time_window_seconds" => Some(Field::Window),
                "stage3_ambiguity_floor" => Some(Field::Floor),
                "cosine_threshold" => Some(Field::Cosine),
                _ => None,
            },
            ConfigError::UnknownModel(_) => Some(Field::Model),
        },
        // The engine names the folder it cannot keep. The form never sends such a folder
        // (its fields are text), but a config sent by other means can meet this.
        EngineError::NotUtf8(path) if path == input => Some(Field::Input),
        EngineError::NotUtf8(_) => Some(Field::Output),
        _ => None,
    }
}

/// What a refusal says under its field, in the form's own words (`Photo folder`,
/// `Output folder`, a slider's range), never the engine's (`input`, `directory`, a
/// field's name in the code); any refusal no field words is the engine's, as a sentence.
pub fn error_text(error: &EngineError) -> String {
    match error {
        EngineError::Config(config) => match config {
            ConfigError::InputNotADirectory(input) if input.as_os_str().is_empty() => {
                NO_INPUT.to_owned()
            }
            ConfigError::InputNotADirectory(folder) | ConfigError::OutputNotADirectory(folder) => {
                format!("This is not a folder: {}", display_path(folder))
            }
            ConfigError::OutputEqualsInput => {
                "The output folder must not be the photo folder.".to_owned()
            }
            ConfigError::OutOfRange { min, max, .. } => {
                format!("Choose a value from {min} to {max}.")
            }
            ConfigError::UnknownModel(id) => {
                format!(
                    "This build has no model {id} for the visual check: choose one in Settings."
                )
            }
        },
        EngineError::NotUtf8(folder) => format!("{}: {NOT_UNICODE}", display_path(folder)),
        other => engine_error_text(other),
    }
}

/// A submission of the form, until the engine answers it.
pub(super) struct Submission {
    /// The token its answer carries.
    token: u64,
    /// The photo folder it submitted.
    input: PathBuf,
    /// Whether the form that sent it is still open: once it is closed, a refusal has no
    /// field to show under.
    form_open: bool,
}

/// The form's submissions waiting for their answers.
pub(super) type Submissions = Vec<Submission>;

impl Controller {
    /// Shows the new-job form, on `input` when given. An open form keeps what was typed
    /// and chosen, but takes `input` as its folder, unless it is being submitted. A
    /// folder whose name is not valid Unicode is refused with a warning, and nothing
    /// opens or changes.
    pub fn open_new_job(&mut self, input: Option<PathBuf>) {
        let input = match input {
            Some(path) => match self.unicode_folder(&path) {
                Some(text) => Some(text.to_owned()),
                None => return,
            },
            None => None,
        };
        match (&mut self.form, input) {
            (Some(form), Some(input)) if !form.submitting => form.set_input(&input),
            (Some(_), _) => {}
            (None, input) => {
                let input = input.as_deref().map(Path::new);
                self.form = Some(JobForm::new(&self.prefs.defaults, input));
            }
        }
        self.view = View::NewJob;
    }

    /// The new-job form, while it is open.
    pub fn form(&mut self) -> Option<&mut JobForm> {
        self.form.as_mut()
    }

    /// Asks for the photo folder with the desktop's dialog, which blocks until it is
    /// closed; a cancelled dialog changes nothing, a desktop without a dialog is a warning
    /// that says to type the path, and a folder whose name is not valid Unicode is refused
    /// with a warning.
    pub fn browse_input(&mut self) {
        if let Some(folder) = self.pick_folder(|form| form.input.as_str())
            && let Some(text) = self.unicode_folder(&folder).map(str::to_owned)
            && let Some(form) = &mut self.form
        {
            form.set_input(&text);
        }
    }

    /// Asks for the output folder, as [`Controller::browse_input`] does.
    pub fn browse_output(&mut self) {
        if let Some(folder) = self.pick_folder(|form| form.output.as_str())
            && let Some(text) = self.unicode_folder(&folder).map(str::to_owned)
            && let Some(form) = &mut self.form
        {
            form.set_output(&text);
        }
    }

    /// `folder`'s name as the form's text, or, when it is not valid Unicode, nothing and
    /// a warning: the engine keeps a job's folders as text, so it would refuse it, and a
    /// lossy copy would name another folder.
    fn unicode_folder<'a>(&mut self, folder: &'a Path) -> Option<&'a str> {
        let text = folder.to_str();
        if text.is_none() {
            let shown = display_path(folder);
            self.notify(NoticeLevel::Warning, format!("{shown}: {NOT_UNICODE}"));
        }
        text
    }

    /// The folder picked for the field `text` gives, in a dialog that starts in its folder,
    /// or, while it is empty, in the last photo folder of a job. Nothing without a form
    /// open and not being submitted, nor when the dialog is cancelled; nothing and a
    /// warning when the desktop has no dialog.
    fn pick_folder(&mut self, text: fn(&JobForm) -> &str) -> Option<PathBuf> {
        let form = self.form.as_ref().filter(|form| !form.submitting)?;
        let field = folder_text(text(form));
        let start = if field.is_empty() {
            self.prefs.last_input_dir.as_ref().map(PathBuf::from)
        } else {
            Some(PathBuf::from(field))
        };
        match self.platform.pick_folder(start.as_deref()) {
            Picked::Path(folder) => Some(folder),
            Picked::Cancelled => None,
            Picked::Unavailable => {
                self.notify(NoticeLevel::Warning, NO_FOLDER_DIALOG.into());
                None
            }
        }
    }

    /// Submits the form's job, unless it is being submitted already. The answer comes at
    /// a later [`Controller::poll`].
    pub fn submit_form(&mut self) {
        let model = self.job_model();
        let Some(form) = self.form.as_mut().filter(|form| !form.submitting) else {
            return;
        };
        form.error = None;
        form.submitting = true;
        let config = form.config(model);
        let token = self.next_token();
        self.submissions.push(Submission {
            token,
            input: config.input_dir.clone(),
            form_open: true,
        });
        self.send(Request::Submit { token, config });
    }

    /// Closes the form, dropping what it holds, and shows the selected job. A submission
    /// on its way still makes its job, which is then selected.
    pub fn close_form(&mut self) {
        self.form = None;
        for submission in &mut self.submissions {
            submission.form_open = false;
        }
        self.view = View::Job;
    }

    /// Takes the files or folders dropped on the window: the first that is a folder, or
    /// a file's folder, opens the form. One look at each path (whether it is a folder) is
    /// the one file-system call the app's own code makes on the UI thread after start-up
    /// (the rule and its exceptions: crates/ui/DESIGN.md, State and the engine).
    pub fn dropped(&mut self, paths: &[PathBuf]) {
        let mut folders: Vec<PathBuf> = Vec::new();
        for folder in paths.iter().filter_map(|path| dropped_folder(path)) {
            if !folders.contains(&folder) {
                folders.push(folder);
            }
        }
        let mut folders = folders.into_iter();
        let Some(first) = folders.next() else {
            self.notify(NoticeLevel::Warning, NOTHING_USABLE.into());
            return;
        };
        if self.unicode_folder(&first).is_none() {
            return;
        }
        if folders.next().is_some() {
            self.notify(NoticeLevel::Info, FIRST_FOLDER_ONLY.into());
        }
        self.open_new_job(Some(first));
    }

    /// The form's submission whose answer carries `token`, taken out of those waiting;
    /// `None` for an answer that is not the form's.
    pub(super) fn take_submission(&mut self, token: u64) -> Option<Submission> {
        let at = self
            .submissions
            .iter()
            .position(|submission| submission.token == token)?;
        Some(self.submissions.remove(at))
    }

    /// Takes the engine's answer to one of the form's submissions. A new job is selected
    /// (unless a newer form is on screen, or behind Settings) and its folder remembered
    /// for the picker; a refusal shows under its field, or as a notice when no field is
    /// at fault, as long as the form that sent it is open.
    pub(super) fn form_answered(
        &mut self,
        submission: Submission,
        answer: Result<JobId, EngineError>,
    ) {
        match answer {
            Ok(job) => {
                self.remember_input_dir(&submission.input);
                if submission.form_open {
                    self.form = None;
                    self.select_submitted_job(job);
                } else if self.view_behind_settings() != View::NewJob {
                    self.select_submitted_job(job);
                }
            }
            Err(error) if submission.form_open => {
                let at_fault = field_of(&error, &submission.input);
                if let Some(form) = &mut self.form {
                    form.submitting = false;
                    form.error = at_fault.map(|field| (field, error_text(&error)));
                }
                if at_fault.is_none() {
                    self.notify(NoticeLevel::Error, engine_error_text(&error));
                }
            }
            Err(_) => {}
        }
    }

    /// Keeps `input` as the folder the picker opens in next, and saves it. A folder that
    /// is not valid Unicode cannot be saved, and is not kept.
    fn remember_input_dir(&mut self, input: &Path) {
        let Some(dir) = input.to_str() else {
            return;
        };
        let dir = dir.to_owned();
        self.prefs.last_input_dir = Some(dir.clone());
        self.save_pref(prefs::LAST_INPUT_DIR, dir);
    }
}

/// The folder a dropped path stands for: itself, or a file's folder; `None` for a path
/// that cannot be looked at.
fn dropped_folder(path: &Path) -> Option<PathBuf> {
    let metadata = fs::metadata(path).ok()?;
    if metadata.is_dir() {
        Some(path.to_path_buf())
    } else {
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use photostacker_core::model::{OutputFormat, RawJpegPreference, RawTone};
    use photostacker_core::{ConfigError, EngineError, JobConfig};

    use super::*;
    use crate::prefs::FormDefaults;

    fn empty_form() -> JobForm {
        JobForm::new(&FormDefaults::default(), None)
    }

    /// The text of the default output of `input`.
    fn stacked(input: &str) -> String {
        Path::new(input)
            .join("stacked")
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn the_output_follows_the_input_until_it_is_edited() {
        let mut form = empty_form();
        assert_eq!((form.input.as_str(), form.output.as_str()), ("", ""));

        // A pasted path loses its surrounding spaces and quotes, in the output and the
        // config; the field keeps what was typed.
        form.set_input("  \"/photos/trip\" ");
        assert_eq!(form.input, "  \"/photos/trip\" ");
        assert_eq!(form.output, stacked("/photos/trip"));
        assert_eq!(form.config(None).input_dir, PathBuf::from("/photos/trip"));
        form.set_input("'/photos/city'");
        assert_eq!(form.output, stacked("/photos/city"));

        form.set_output("/stacks");
        assert!(form.output_edited);
        form.set_input("/photos/lake");
        assert_eq!(form.output, "/stacks");
        assert_eq!(form.config(None).output_dir, PathBuf::from("/stacks"));

        // Emptied, the output is the default again, and follows the input once more.
        form.set_output("  ");
        assert!(!form.output_edited);
        assert_eq!(
            form.config(None).output_dir,
            Path::new("/photos/lake").join("stacked")
        );
        form.set_input("/photos/sea");
        assert_eq!(form.output, stacked("/photos/sea"));

        // A form opened on a folder starts from it.
        let opened = JobForm::new(&FormDefaults::default(), Some(Path::new("/photos/trip")));
        assert_eq!(opened.input, "/photos/trip");
        assert_eq!(opened.output, stacked("/photos/trip"));
    }

    #[test]
    fn the_form_starts_from_the_saved_defaults() {
        let defaults = FormDefaults {
            output_format: OutputFormat::Tiff,
            recursive: true,
            auto_confirm: true,
            hamming_threshold: 12,
            time_window_seconds: 9,
            stage3_ambiguity_floor: 3,
            cosine_threshold: Some(0.7),
            raw_tone: RawTone::Linear,
            raw_jpeg_preference: RawJpegPreference::Jpeg,
        };
        let form = JobForm::new(&defaults, Some(Path::new("/photos/trip")));
        assert_eq!((&form.error, form.submitting), (&None, false));

        let mut expected = JobConfig::new(PathBuf::from("/photos/trip"));
        expected.output_format = OutputFormat::Tiff;
        expected.recursive = true;
        expected.auto_confirm = true;
        expected.hamming_threshold = 12;
        expected.time_window_seconds = 9;
        expected.stage3_ambiguity_floor = 3;
        expected.cosine_threshold = Some(0.7);
        expected.raw_tone = RawTone::Linear;
        expected.raw_jpeg_preference = RawJpegPreference::Jpeg;
        expected.ml_model = Some("dinov2-small".into());
        assert_eq!(form.config(Some("dinov2-small")), expected);
    }

    #[test]
    fn ml_names_the_model_only_when_given_one() {
        let mut form = empty_form();
        form.set_input("/photos/trip");
        form.fields.cosine_threshold = Some(0.8);
        let config = form.config(Some("dinov2-base"));
        assert_eq!(config.ml_model.as_deref(), Some("dinov2-base"));
        assert_eq!(config.cosine_threshold, Some(0.8));

        // The controller gives none while the visual check cannot run.
        let config = form.config(None);
        assert_eq!(config.ml_model, None);
        // Stage 3 off, the similarity is not shown, so it is not sent either.
        assert_eq!(config.cosine_threshold, None);
    }

    #[test]
    fn every_slider_limit_gives_a_config_that_validates() {
        let input = tempfile::tempdir().unwrap();
        let mut form = JobForm::new(&FormDefaults::default(), Some(input.path()));
        let ends = |range: std::ops::RangeInclusive<u32>| [*range.start(), *range.end()];
        let cosines = [None, Some(*COSINE_RANGE.start()), Some(*COSINE_RANGE.end())];
        for hamming in ends(HAMMING_RANGE) {
            for window in ends(WINDOW_RANGE) {
                for floor in ends(FLOOR_RANGE) {
                    for cosine in cosines {
                        form.fields.hamming_threshold = hamming;
                        form.fields.time_window_seconds = window;
                        form.fields.stage3_ambiguity_floor = floor;
                        form.fields.cosine_threshold = cosine;
                        let config = form.config(Some("dinov2-small"));
                        assert_eq!(config.validate(), Ok(()), "{config:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn refusals_map_to_their_fields() {
        let input = Path::new("/photos/trip");
        let field = |error: ConfigError| field_of(&EngineError::Config(error), input);
        assert_eq!(
            field(ConfigError::InputNotADirectory(input.into())),
            Some(Field::Input)
        );
        assert_eq!(field(ConfigError::OutputEqualsInput), Some(Field::Output));
        assert_eq!(
            field(ConfigError::OutputNotADirectory(input.join("stacked"))),
            Some(Field::Output)
        );
        assert_eq!(
            field(ConfigError::UnknownModel("nope".into())),
            Some(Field::Model)
        );
        assert_eq!(
            field_of(&EngineError::NotUtf8(input.into()), input),
            Some(Field::Input)
        );
        assert_eq!(
            field_of(&EngineError::NotUtf8(input.join("stacked")), input),
            Some(Field::Output)
        );
        // No field is at fault.
        assert_eq!(field_of(&EngineError::Stopped, input), None);

        // A value out of range, by the field the core names: one past each slider's end.
        let folder = tempfile::tempdir().unwrap();
        let in_range = JobConfig::new(folder.path().to_path_buf());
        let mut configs = Vec::new();
        let mut config = in_range.clone();
        config.hamming_threshold = HAMMING_RANGE.end() + 1;
        configs.push((config, Field::Hamming));
        let mut config = in_range.clone();
        config.time_window_seconds = WINDOW_RANGE.end() + 1;
        configs.push((config, Field::Window));
        let mut config = in_range.clone();
        config.stage3_ambiguity_floor = FLOOR_RANGE.end() + 1;
        configs.push((config, Field::Floor));
        let mut config = in_range;
        config.cosine_threshold = Some(1.0);
        configs.push((config, Field::Cosine));
        for (config, expected) in configs {
            let error = EngineError::Config(config.validate().unwrap_err());
            assert_eq!(field_of(&error, folder.path()), Some(expected), "{error}");
        }
        let unknown = ConfigError::OutOfRange {
            field: "something_new",
            value: "1".into(),
            min: "0".into(),
            max: "0".into(),
        };
        assert_eq!(field(unknown), None);
    }

    #[test]
    fn a_refusal_reads_in_the_forms_words() {
        let config = |error| error_text(&EngineError::Config(error));
        assert_eq!(
            config(ConfigError::InputNotADirectory("/photos/x".into())),
            "This is not a folder: /photos/x"
        );
        assert_eq!(
            config(ConfigError::InputNotADirectory(PathBuf::new())),
            "Choose the folder of photos."
        );
        assert_eq!(
            config(ConfigError::OutputNotADirectory("/photos/x/stacked".into())),
            "This is not a folder: /photos/x/stacked"
        );
        assert_eq!(
            config(ConfigError::OutputEqualsInput),
            "The output folder must not be the photo folder."
        );
        let out_of_range = ConfigError::OutOfRange {
            field: "hamming_threshold",
            value: "41".into(),
            min: "0".into(),
            max: "40".into(),
        };
        assert_eq!(config(out_of_range), "Choose a value from 0 to 40.");
        assert_eq!(
            config(ConfigError::UnknownModel("dinov2-giant".into())),
            "This build has no model dinov2-giant for the visual check: choose one in Settings."
        );
        assert_eq!(
            error_text(&EngineError::NotUtf8("/photos/x".into())),
            "/photos/x: Photostacker can only use folders whose names are valid Unicode."
        );
        // Words the form has none for are the engine's, as a sentence.
        assert_eq!(error_text(&EngineError::Stopped), "The engine has stopped.");
        // Never the code's words.
        for text in [
            config(ConfigError::InputNotADirectory("/x".into())),
            config(ConfigError::OutputEqualsInput),
        ] {
            for word in ["Input", "input", "Output", "directory"] {
                assert!(!text.contains(word), "{text}");
            }
        }
    }
}
