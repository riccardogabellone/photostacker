//! Domain types shared by every pipeline stage: the job configuration and its
//! validation, file kinds, per-file warnings, the status of a job, and the
//! review and Stage-3 statuses of a group.

use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The folder, inside the input folder, that receives the stacks by default.
const DEFAULT_OUTPUT_SUBDIR: &str = "stacked";

/// The layered file format a stack is written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    /// Photoshop document; PSB is chosen automatically past the PSD limits.
    Psd,
    /// Multi-page layered TIFF.
    Tiff,
}

/// How RAW files are developed before they become layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RawTone {
    /// Gamma-encoded sRGB.
    Srgb,
    /// Linear light.
    Linear,
}

/// Which file of a RAW+JPEG pair (same folder, same stem) is stacked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RawJpegPreference {
    Raw,
    Jpeg,
}

/// The decoder family a file belongs to. Discovery decides it by extension;
/// analysis then lets a still image's content override it (spec §5.1).
/// The store persists its serde form: changing a spelling needs a migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    Jpeg,
    Png,
    Tiff,
    Heic,
    Raw,
}

/// Every parameter of a job. Build it with [`JobConfig::new`], override fields,
/// then call [`JobConfig::validate`] once; the rest of the code trusts it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobConfig {
    pub input_dir: PathBuf,
    pub output_dir: PathBuf,
    pub output_format: OutputFormat,
    /// Also scan the subfolders of `input_dir`.
    pub recursive: bool,
    /// Write every proposed group without waiting for a review.
    pub auto_confirm: bool,
    /// Stage 2: maximum Hamming(pHash) + Hamming(dHash) for two photos to match.
    pub hamming_threshold: u32,
    /// Stage 1: maximum gap between consecutive captures in one time bucket.
    pub time_window_seconds: u32,
    /// Stage 3 runs on a Stage-2 group whose maximum pairwise hash distance is above this.
    pub stage3_ambiguity_floor: u32,
    /// Stage 3 cosine similarity threshold; `None` uses the model's default.
    pub cosine_threshold: Option<f32>,
    pub raw_tone: RawTone,
    pub raw_jpeg_preference: RawJpegPreference,
    /// The Stage-3 model, by its registry id ([`crate::models::MODELS`]);
    /// `None` turns Stage 3 off. A config saved before the field existed
    /// reads as `None`.
    #[serde(default)]
    pub ml_model: Option<String>,
}

impl JobConfig {
    /// A config with the spec defaults; the output goes to `<input>/stacked`.
    pub fn new(input_dir: PathBuf) -> Self {
        let output_dir = input_dir.join(DEFAULT_OUTPUT_SUBDIR);
        Self {
            input_dir,
            output_dir,
            output_format: OutputFormat::Psd,
            recursive: false,
            auto_confirm: false,
            hamming_threshold: 8,
            // Spec §5.2: a window of 4 s left series shot 5 to 30 s apart
            // unstacked.
            time_window_seconds: 30,
            stage3_ambiguity_floor: 6,
            cosine_threshold: None,
            raw_tone: RawTone::Srgb,
            raw_jpeg_preference: RawJpegPreference::Raw,
            ml_model: None,
        }
    }

    /// Checks every parameter. Returns the first problem found.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.input_dir.is_dir() {
            return Err(ConfigError::InputNotADirectory(self.input_dir.clone()));
        }
        if same_dir(&self.input_dir, &self.output_dir) {
            return Err(ConfigError::OutputEqualsInput);
        }
        if self.output_dir.exists() && !self.output_dir.is_dir() {
            return Err(ConfigError::OutputNotADirectory(self.output_dir.clone()));
        }
        self.validate_thresholds()?;
        match &self.ml_model {
            Some(id) if crate::models::model(id).is_none() => {
                Err(ConfigError::UnknownModel(id.clone()))
            }
            _ => Ok(()),
        }
    }

    /// Checks the grouping thresholds alone, without the file system: what
    /// a threshold change in review needs. [`JobConfig::validate`] checks
    /// them too, after the folders.
    pub fn validate_thresholds(&self) -> Result<(), ConfigError> {
        check_range("hamming_threshold", self.hamming_threshold, 0, 40)?;
        check_range("time_window_seconds", self.time_window_seconds, 1, 60)?;
        check_range("stage3_ambiguity_floor", self.stage3_ambiguity_floor, 0, 20)?;
        if let Some(cosine) = self.cosine_threshold {
            check_range("cosine_threshold", cosine, 0.5, 0.99)?;
        }
        Ok(())
    }
}

/// Compares canonicalised paths when both exist, so `a/b/..` equals `a`;
/// otherwise compares both [`normalise_lexically`], so `a/missing/..` equals
/// `a` too. The writer checks again, on the created folder
/// ([`crate::fsio::ensure_output_dir`]).
fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => normalise_lexically(a) == normalise_lexically(b),
    }
}

/// `path` made absolute, with `.` and `..` resolved over its components and
/// without touching the file system. A `..` at the root stays at the root.
/// Symlinks are not followed, so `link/..` resolves to the link's parent.
fn normalise_lexically(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut normal = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other),
        }
    }
    normal
}

/// Inclusive range check. A NaN is never inside the range, so it is rejected.
fn check_range<T>(field: &'static str, value: T, min: T, max: T) -> Result<(), ConfigError>
where
    T: PartialOrd + fmt::Display,
{
    if value >= min && value <= max {
        Ok(())
    } else {
        Err(ConfigError::OutOfRange {
            field,
            value: value.to_string(),
            min: min.to_string(),
            max: max.to_string(),
        })
    }
}

/// Why a [`JobConfig`] was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("input is not a directory: {}", crate::fsio::display_path(.0))]
    InputNotADirectory(PathBuf),
    #[error("the output folder must differ from the input folder")]
    OutputEqualsInput,
    #[error("output is not a directory: {}", crate::fsio::display_path(.0))]
    OutputNotADirectory(PathBuf),
    #[error("{field} = {value} is out of range ({min} to {max})")]
    OutOfRange {
        field: &'static str,
        value: String,
        min: String,
        max: String,
    },
    #[error("unknown model {0:?} (known: {known})", known = known_models())]
    UnknownModel(String),
}

/// The registry's model ids, for [`ConfigError::UnknownModel`].
fn known_models() -> String {
    let ids: Vec<_> = crate::models::MODELS.iter().map(|spec| spec.id).collect();
    ids.join(", ")
}

/// Where a group is in review and writing (spec §5.4.1, §5.7).
///
/// The store persists its serde form: changing a spelling needs a migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupStatus {
    /// Waiting for the user's decision; never written.
    Proposed,
    /// The user (or `auto_confirm`) accepted it: the next write writes it.
    Confirmed,
    /// The user turned it down; never written.
    Rejected,
    /// Its stack was written.
    Written,
    /// The write left it out: fewer than two of its photos could be layered.
    Skipped,
    /// Its write failed.
    Failed,
}

/// Who made a group (spec §5.4.1).
///
/// The store persists its serde form: changing a spelling needs a migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupOrigin {
    /// Grouping made it; regrouping may change or remove it.
    Auto,
    /// The user made or edited it; regrouping leaves it as it is.
    Manual,
}

/// The outcome of the Stage-3 check on a group (spec §5.4).
///
/// The store persists its serde form: changing a spelling needs a migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MlStatus {
    /// Not checked: the group is not ambiguous (its maximum hash distance
    /// is at or below the floor).
    None,
    /// Checked: its embeddings keep it whole.
    Confirmed,
    /// Checked: it is one part of a Stage-2 group its embeddings split.
    Split,
    /// Ambiguous, but ML is unavailable.
    Skipped,
    /// Ambiguous, and waiting for the embeddings of some members.
    Pending,
}

/// Where a job is in its life (spec §5.7).
///
/// The store persists its serde form: changing a spelling needs a migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Waiting for its scan, or once scanned for its write.
    Queued,
    /// Being analysed.
    Scanning,
    /// Analysed; its groups wait for the user's review.
    ReviewPending,
    /// Its confirmed groups are being written.
    Writing,
    /// Its confirmed groups were written.
    Completed,
    /// Its scan or its write failed; the job's error says why.
    Failed,
    /// The user cancelled it.
    Cancelled,
}

/// A non-fatal remark about one file. Per-file failures are not warnings: they
/// are recorded as the file's error.
///
/// The store persists its serde form: changing a spelling needs a migration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Warning {
    /// The file has no EXIF capture time; its modification time was used.
    NoCaptureTime,
    /// The file's EXIF could not be read. It was grouped with the EXIF of its
    /// paired file (spec §5.1), or without EXIF when it has none.
    ExifUnreadable { reason: String },
    /// rawler could not decode the RAW, so it was hashed from the largest
    /// JPEG embedded in it. `reason` is rawler's error.
    RawPreviewOnly { reason: String },
    /// The active RAW of this file's pair could not be decoded, or only from
    /// its embedded preview, so this file was stacked in its place (spec
    /// §5.1). `reason` is the RAW's error, or rawler's error behind its
    /// preview.
    PromotedOverRaw { reason: String },
    /// The file's upright resolution is not the stack's canvas resolution, so
    /// it was left out of the stack (spec §5.5).
    ResolutionMismatch {
        width: u32,
        height: u32,
        canvas_width: u32,
        canvas_height: u32,
    },
    /// The file's ICC profile differs from the stack's reference member's.
    /// The reference profile was embedded and the pixels were not converted
    /// (spec §5.5).
    IccMismatch,
    /// The file could not be read when its stack was written, or no longer
    /// matched what its probe said, so it was left out of the stack (spec
    /// §5.6). `reason` is the decoder's error or what changed.
    LayerUnreadable { reason: String },
}

impl Warning {
    /// The variant's serde tag, such as `no_capture_time`: a stable code for
    /// the plan's readers, where the displayed text may change.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoCaptureTime => "no_capture_time",
            Self::ExifUnreadable { .. } => "exif_unreadable",
            Self::RawPreviewOnly { .. } => "raw_preview_only",
            Self::PromotedOverRaw { .. } => "promoted_over_raw",
            Self::ResolutionMismatch { .. } => "resolution_mismatch",
            Self::IccMismatch => "icc_mismatch",
            Self::LayerUnreadable { .. } => "layer_unreadable",
        }
    }
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCaptureTime => {
                f.write_str("no EXIF capture time; the file modification time was used")
            }
            Self::ExifUnreadable { reason } => {
                write!(
                    f,
                    "unreadable EXIF, grouped with the paired file's EXIF if any, \
                     else without: {reason}"
                )
            }
            Self::RawPreviewOnly { reason } => {
                write!(
                    f,
                    "RAW not decodable ({reason}); its embedded preview was used"
                )
            }
            Self::PromotedOverRaw { reason } => {
                write!(
                    f,
                    "the paired RAW could not be decoded ({reason}); this file was stacked instead"
                )
            }
            Self::ResolutionMismatch {
                width,
                height,
                canvas_width,
                canvas_height,
            } => {
                write!(
                    f,
                    "resolution {width}×{height} differs from the canvas \
                     {canvas_width}×{canvas_height}; the file was left out of the stack"
                )
            }
            Self::IccMismatch => f.write_str(
                "colour profile differs from the reference layer's; \
                 the reference profile was embedded and the pixels were not converted",
            ),
            Self::LayerUnreadable { reason } => {
                write!(
                    f,
                    "unreadable when the stack was written ({reason}); \
                     the file was left out of the stack"
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Sets one field of a config, so a table can list the cases.
    type Override = fn(&mut JobConfig);

    /// A config whose input is a real (temporary) directory, so only the field
    /// under test can make `validate` fail.
    fn valid_config(input: &tempfile::TempDir) -> JobConfig {
        JobConfig::new(input.path().to_path_buf())
    }

    fn out_of_range_field(result: Result<(), ConfigError>) -> &'static str {
        match result {
            Err(ConfigError::OutOfRange { field, .. }) => field,
            other => panic!("expected OutOfRange, got {other:?}"),
        }
    }

    #[test]
    fn defaults_match_spec() {
        let c = JobConfig::new("/in".into());
        assert_eq!(
            (
                c.hamming_threshold,
                c.time_window_seconds,
                c.stage3_ambiguity_floor
            ),
            (8, 30, 6)
        );
        assert_eq!(c.output_dir, PathBuf::from("/in/stacked"));
        assert_eq!(c.cosine_threshold, None);
        assert_eq!(
            (c.output_format, c.raw_tone, c.raw_jpeg_preference),
            (OutputFormat::Psd, RawTone::Srgb, RawJpegPreference::Raw)
        );
        assert!(!c.recursive && !c.auto_confirm);
        assert_eq!(c.ml_model, None);
    }

    #[test]
    fn defaults_are_valid() {
        let input = tempfile::tempdir().unwrap();
        assert_eq!(valid_config(&input).validate(), Ok(()));
    }

    #[test]
    fn ranges_are_enforced() {
        let input = tempfile::tempdir().unwrap();

        let rejected: [(&str, Override); 6] = [
            ("hamming_threshold", |c| c.hamming_threshold = 41),
            ("time_window_seconds", |c| c.time_window_seconds = 0),
            ("time_window_seconds", |c| c.time_window_seconds = 61),
            ("stage3_ambiguity_floor", |c| c.stage3_ambiguity_floor = 21),
            ("cosine_threshold", |c| c.cosine_threshold = Some(0.49)),
            ("cosine_threshold", |c| c.cosine_threshold = Some(1.0)),
        ];
        for (field, set) in rejected {
            let mut c = valid_config(&input);
            set(&mut c);
            assert_eq!(out_of_range_field(c.validate()), field);
        }

        let accepted: [Override; 8] = [
            |c| c.hamming_threshold = 0,
            |c| c.hamming_threshold = 40,
            |c| c.time_window_seconds = 1,
            |c| c.time_window_seconds = 60,
            |c| c.stage3_ambiguity_floor = 0,
            |c| c.stage3_ambiguity_floor = 20,
            |c| c.cosine_threshold = Some(0.5),
            |c| c.cosine_threshold = Some(0.99),
        ];
        for set in accepted {
            let mut c = valid_config(&input);
            set(&mut c);
            assert_eq!(c.validate(), Ok(()), "{c:?}");
        }
    }

    #[test]
    fn nan_cosine_is_rejected() {
        let input = tempfile::tempdir().unwrap();
        let mut c = valid_config(&input);
        c.cosine_threshold = Some(f32::NAN);
        assert_eq!(out_of_range_field(c.validate()), "cosine_threshold");
    }

    #[test]
    fn out_of_range_reports_value_and_bounds() {
        let input = tempfile::tempdir().unwrap();
        let mut c = valid_config(&input);
        c.hamming_threshold = 41;
        assert_eq!(
            c.validate(),
            Err(ConfigError::OutOfRange {
                field: "hamming_threshold",
                value: "41".into(),
                min: "0".into(),
                max: "40".into(),
            })
        );
    }

    #[test]
    fn output_equal_to_input_is_rejected() {
        let input = tempfile::tempdir().unwrap();
        let mut c = valid_config(&input);
        c.output_dir = input.path().to_path_buf();
        assert_eq!(c.validate(), Err(ConfigError::OutputEqualsInput));
    }

    #[test]
    fn output_equal_to_input_is_rejected_after_canonicalisation() {
        let input = tempfile::tempdir().unwrap();
        std::fs::create_dir(input.path().join("sub")).unwrap();
        let mut c = valid_config(&input);
        // Spelled differently, but the same directory once `..` is resolved.
        c.output_dir = input.path().join("sub").join("..");
        assert_eq!(c.validate(), Err(ConfigError::OutputEqualsInput));
    }

    #[test]
    fn missing_dotdot_output_equals_input_is_rejected() {
        let input = tempfile::tempdir().unwrap();
        let mut c = valid_config(&input);
        // `missing` does not exist, so the path cannot be canonicalised.
        c.output_dir = input.path().join("missing").join("..");
        assert_eq!(c.validate(), Err(ConfigError::OutputEqualsInput));
    }

    #[test]
    fn same_dir_normalises_relative_and_missing_paths() {
        let cwd = std::env::current_dir().unwrap();
        let missing = "photostacker-no-such-dir";
        assert!(!Path::new(missing).exists());
        // One side canonicalises, the other cannot: both are normalised.
        assert!(same_dir(&Path::new(missing).join(".."), &cwd));
        assert!(same_dir(&cwd, &Path::new(".").join(missing).join("..")));
        // Neither side canonicalises: relative equals absolute.
        assert!(same_dir(Path::new(missing), &cwd.join(missing)));
        assert!(!same_dir(Path::new(missing), &cwd));
    }

    #[test]
    fn existing_file_as_output_is_rejected() {
        let input = tempfile::tempdir().unwrap();
        let mut c = valid_config(&input);
        c.output_dir = input.path().join("notes.txt");
        std::fs::write(&c.output_dir, b"not a directory").unwrap();
        assert_eq!(
            c.validate(),
            Err(ConfigError::OutputNotADirectory(c.output_dir.clone()))
        );
    }

    #[test]
    fn missing_input_is_rejected() {
        let parent = tempfile::tempdir().unwrap();
        let missing = parent.path().join("does-not-exist");
        let c = JobConfig::new(missing.clone());
        assert_eq!(c.validate(), Err(ConfigError::InputNotADirectory(missing)));
    }

    #[test]
    fn input_that_is_a_file_is_rejected() {
        let parent = tempfile::tempdir().unwrap();
        let file = parent.path().join("photo.jpg");
        std::fs::write(&file, b"not a directory").unwrap();
        let c = JobConfig::new(file.clone());
        assert_eq!(c.validate(), Err(ConfigError::InputNotADirectory(file)));
    }

    #[test]
    fn config_round_trips_through_json_with_lowercase_enums() {
        let mut c = JobConfig::new("/in".into());
        c.output_format = OutputFormat::Tiff;
        c.raw_tone = RawTone::Linear;
        c.raw_jpeg_preference = RawJpegPreference::Jpeg;
        c.cosine_threshold = Some(0.85);
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains(r#""output_format":"tiff""#), "{json}");
        assert!(json.contains(r#""raw_tone":"linear""#), "{json}");
        assert!(json.contains(r#""raw_jpeg_preference":"jpeg""#), "{json}");
        assert_eq!(serde_json::from_str::<JobConfig>(&json).unwrap(), c);
    }

    #[test]
    fn unknown_model_is_rejected_by_validate() {
        let input = tempfile::tempdir().unwrap();
        let mut c = valid_config(&input);
        c.ml_model = Some("dinov2-large".into());
        let error = c.validate().unwrap_err();
        assert_eq!(error, ConfigError::UnknownModel("dinov2-large".into()));
        assert_eq!(
            error.to_string(),
            r#"unknown model "dinov2-large" (known: dinov2-small, dinov2-base)"#
        );
        for id in ["dinov2-small", "dinov2-base"] {
            c.ml_model = Some(id.into());
            assert_eq!(c.validate(), Ok(()), "{id}");
        }
    }

    /// The store reads back configs saved before `ml_model` existed.
    #[test]
    fn ml_model_round_trips_and_defaults_to_none_when_absent() {
        let mut c = JobConfig::new("/in".into());
        c.ml_model = Some("dinov2-small".into());
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains(r#""ml_model":"dinov2-small""#), "{json}");
        assert_eq!(serde_json::from_str::<JobConfig>(&json).unwrap(), c);

        let mut older = serde_json::to_value(&c).unwrap();
        older.as_object_mut().unwrap().remove("ml_model");
        let read: JobConfig = serde_json::from_value(older).unwrap();
        assert_eq!(read.ml_model, None);
        assert_eq!(
            serde_json::to_string(&JobConfig::new("/in".into())).unwrap(),
            json.replace(r#""ml_model":"dinov2-small""#, r#""ml_model":null"#)
        );
    }

    /// The store persists these spellings: changing one needs a migration.
    #[test]
    fn warning_serde_spellings_are_pinned() {
        let reason = || "x".to_string();
        let cases = [
            (Warning::NoCaptureTime, r#""no_capture_time""#),
            (
                Warning::ExifUnreadable { reason: reason() },
                r#"{"exif_unreadable":{"reason":"x"}}"#,
            ),
            (
                Warning::RawPreviewOnly { reason: reason() },
                r#"{"raw_preview_only":{"reason":"x"}}"#,
            ),
            (
                Warning::PromotedOverRaw { reason: reason() },
                r#"{"promoted_over_raw":{"reason":"x"}}"#,
            ),
            (
                Warning::ResolutionMismatch {
                    width: 1,
                    height: 2,
                    canvas_width: 3,
                    canvas_height: 4,
                },
                r#"{"resolution_mismatch":{"width":1,"height":2,"canvas_width":3,"canvas_height":4}}"#,
            ),
            (Warning::IccMismatch, r#""icc_mismatch""#),
            (
                Warning::LayerUnreadable { reason: reason() },
                r#"{"layer_unreadable":{"reason":"x"}}"#,
            ),
        ];
        for (warning, json) in cases {
            assert_eq!(serde_json::to_string(&warning).unwrap(), json);
            assert_eq!(serde_json::from_str::<Warning>(json).unwrap(), warning);
            // The code is the serde tag: the string itself, or the one key.
            let tag = match serde_json::from_str(json).unwrap() {
                serde_json::Value::String(tag) => tag,
                serde_json::Value::Object(map) => map.keys().next().unwrap().clone(),
                other => panic!("unexpected JSON {other}"),
            };
            assert_eq!(warning.code(), tag);
        }
    }

    /// The store persists these spellings: changing one needs a migration.
    #[test]
    fn file_kind_serde_spellings_are_pinned() {
        let cases = [
            (FileKind::Jpeg, r#""jpeg""#),
            (FileKind::Png, r#""png""#),
            (FileKind::Tiff, r#""tiff""#),
            (FileKind::Heic, r#""heic""#),
            (FileKind::Raw, r#""raw""#),
        ];
        for (kind, json) in cases {
            assert_eq!(serde_json::to_string(&kind).unwrap(), json);
            assert_eq!(serde_json::from_str::<FileKind>(json).unwrap(), kind);
        }
    }

    /// The store persists these spellings: changing one needs a migration.
    #[test]
    fn group_and_job_enum_serde_spellings_are_pinned() {
        fn check<T>(cases: &[(T, &str)])
        where
            T: Serialize + for<'de> Deserialize<'de> + PartialEq + fmt::Debug,
        {
            for (value, json) in cases {
                assert_eq!(&serde_json::to_string(value).unwrap(), json);
                assert_eq!(&serde_json::from_str::<T>(json).unwrap(), value);
            }
        }
        check(&[
            (GroupStatus::Proposed, r#""proposed""#),
            (GroupStatus::Confirmed, r#""confirmed""#),
            (GroupStatus::Rejected, r#""rejected""#),
            (GroupStatus::Written, r#""written""#),
            (GroupStatus::Skipped, r#""skipped""#),
            (GroupStatus::Failed, r#""failed""#),
        ]);
        check(&[
            (GroupOrigin::Auto, r#""auto""#),
            (GroupOrigin::Manual, r#""manual""#),
        ]);
        check(&[
            (MlStatus::None, r#""none""#),
            (MlStatus::Confirmed, r#""confirmed""#),
            (MlStatus::Split, r#""split""#),
            (MlStatus::Skipped, r#""skipped""#),
            (MlStatus::Pending, r#""pending""#),
        ]);
        check(&[
            (JobStatus::Queued, r#""queued""#),
            (JobStatus::Scanning, r#""scanning""#),
            (JobStatus::ReviewPending, r#""review_pending""#),
            (JobStatus::Writing, r#""writing""#),
            (JobStatus::Completed, r#""completed""#),
            (JobStatus::Failed, r#""failed""#),
            (JobStatus::Cancelled, r#""cancelled""#),
        ]);
    }

    #[test]
    fn warning_displays_and_round_trips() {
        let unreadable = Warning::ExifUnreadable {
            reason: "not a TIFF block".into(),
        };
        assert_eq!(
            unreadable.to_string(),
            "unreadable EXIF, grouped with the paired file's EXIF if any, else without: \
             not a TIFF block"
        );
        let preview_only = Warning::RawPreviewOnly {
            reason: "unsupported image: unknown camera".into(),
        };
        assert_eq!(
            preview_only.to_string(),
            "RAW not decodable (unsupported image: unknown camera); its embedded preview was used"
        );
        let json = serde_json::to_string(&preview_only).unwrap();
        assert!(json.contains(r#""raw_preview_only""#), "{json}");
        let promoted = Warning::PromotedOverRaw {
            reason: "unsupported image: unknown camera".into(),
        };
        assert_eq!(
            promoted.to_string(),
            "the paired RAW could not be decoded (unsupported image: unknown camera); \
             this file was stacked instead"
        );
        let json = serde_json::to_string(&promoted).unwrap();
        assert!(json.contains(r#""promoted_over_raw""#), "{json}");
        let resolution = Warning::ResolutionMismatch {
            width: 4000,
            height: 6000,
            canvas_width: 6000,
            canvas_height: 4000,
        };
        assert_eq!(
            resolution.to_string(),
            "resolution 4000×6000 differs from the canvas 6000×4000; \
             the file was left out of the stack"
        );
        let json = serde_json::to_string(&resolution).unwrap();
        assert!(json.contains(r#""resolution_mismatch""#), "{json}");
        assert!(json.contains(r#""canvas_width":6000"#), "{json}");
        assert_eq!(
            Warning::IccMismatch.to_string(),
            "colour profile differs from the reference layer's; \
             the reference profile was embedded and the pixels were not converted"
        );
        let json = serde_json::to_string(&Warning::IccMismatch).unwrap();
        assert_eq!(json, r#""icc_mismatch""#);
        let unreadable_layer = Warning::LayerUnreadable {
            reason: "corrupt image: cut off".into(),
        };
        assert_eq!(
            unreadable_layer.to_string(),
            "unreadable when the stack was written (corrupt image: cut off); \
             the file was left out of the stack"
        );
        let json = serde_json::to_string(&unreadable_layer).unwrap();
        assert!(json.contains(r#""layer_unreadable""#), "{json}");
        for w in [
            Warning::NoCaptureTime,
            unreadable,
            preview_only,
            promoted,
            resolution,
            Warning::IccMismatch,
            unreadable_layer,
        ] {
            assert!(!w.to_string().is_empty());
            let json = serde_json::to_string(&w).unwrap();
            assert_eq!(serde_json::from_str::<Warning>(&json).unwrap(), w);
        }
    }
}
