//! The plan file, version 1: the proposed groups of an [`Analysis`] as JSON,
//! for `photostacker-cli run --dry-run --plan` and for regression checks.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::analyze::{Analysis, relative_to};
use crate::model::JobConfig;

/// A plan file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanV1 {
    /// Always [`PlanV1::VERSION`].
    pub version: u32,
    pub input_dir: PathBuf,
    pub config: JobConfig,
    pub groups: Vec<PlanGroup>,
    pub unstacked: Vec<PathBuf>,
    pub paired: Vec<PlanPair>,
    pub errors: Vec<PlanError>,
    pub warnings: Vec<PlanWarning>,
}

/// A proposed stack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanGroup {
    /// In layer order: the first file is the top layer.
    pub files: Vec<PathBuf>,
    pub bucket: String,
    /// The largest normalised hash distance between two members.
    pub max_distance: u32,
}

/// A RAW+JPEG pair: the passive file stays out of grouping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanPair {
    pub passive: PathBuf,
    pub active: PathBuf,
}

/// A file that could not be analysed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanError {
    pub file: PathBuf,
    pub error: String,
}

/// A non-fatal remark about a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanWarning {
    pub file: PathBuf,
    /// The warning's stable code, its serde tag ([`Warning::code`]).
    ///
    /// [`Warning::code`]: crate::model::Warning::code
    pub code: String,
    /// The warning as displayed.
    pub warning: String,
}

impl PlanV1 {
    /// The plan format version this type reads and writes.
    pub const VERSION: u32 = 1;

    /// The plan of `analysis`, which ran on `config`.
    ///
    /// Paths inside `config.input_dir` are relative to it; any other path is
    /// kept as recorded. Every path is written with `/` separators, so a plan
    /// reads the same on every platform. Groups keep their layer order; the
    /// other lists follow the file ids, which are sorted by path, and
    /// `errors` (the failed files plus the entries the scan skipped) is
    /// sorted by path.
    pub fn from_analysis(config: &JobConfig, analysis: &Analysis) -> Self {
        let files = &analysis.files;
        let path = |id: usize| plan_path(&relative_to(&config.input_dir, &files[id].path));

        let groups = analysis
            .groups
            .iter()
            .map(|group| PlanGroup {
                files: group.members.iter().map(|&id| path(id)).collect(),
                bucket: group.bucket.clone(),
                max_distance: group.max_distance,
            })
            .collect();
        let paired = files
            .iter()
            .enumerate()
            .filter_map(|(id, file)| {
                file.paired_with.map(|active| PlanPair {
                    passive: path(id),
                    active: path(active),
                })
            })
            .collect();
        let mut errors: Vec<PlanError> = files
            .iter()
            .enumerate()
            .filter_map(|(id, file)| {
                file.error.as_ref().map(|error| PlanError {
                    file: path(id),
                    error: error.clone(),
                })
            })
            .chain(analysis.skipped.iter().map(|skip| PlanError {
                file: plan_path(&relative_to(&config.input_dir, &skip.path)),
                error: skip.reason.clone(),
            }))
            .collect();
        errors.sort_by(|a, b| a.file.cmp(&b.file));
        let warnings = analysis
            .warnings
            .iter()
            .map(|(id, warning)| PlanWarning {
                file: path(*id),
                code: warning.code().to_owned(),
                warning: warning.to_string(),
            })
            .collect();
        // The config's paths too: its other fields hold none.
        let mut plan_config = config.clone();
        plan_config.input_dir = plan_path(&config.input_dir);
        plan_config.output_dir = plan_path(&config.output_dir);

        Self {
            version: Self::VERSION,
            input_dir: plan_path(&config.input_dir),
            config: plan_config,
            groups,
            unstacked: analysis.unstacked.iter().map(|&id| path(id)).collect(),
            paired,
            errors,
            warnings,
        }
    }
}

/// `path` with `/` separators on Windows, where `\` and `/` both separate.
/// A verbatim path (`\\?\…`) is kept: there `/` is no separator.
#[cfg(windows)]
fn plan_path(path: &Path) -> PathBuf {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::Component;

    if let Some(Component::Prefix(prefix)) = path.components().next()
        && prefix.kind().is_verbatim()
    {
        return path.to_path_buf();
    }
    // UTF-16 units, so a name that is not valid Unicode survives unchanged.
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .map(|unit| {
            if unit == u16::from(b'\\') {
                u16::from(b'/')
            } else {
                unit
            }
        })
        .collect();
    PathBuf::from(OsString::from_wide(&wide))
}

/// `path` as it is: on Unix `\` is a character of a name, not a separator.
#[cfg(not(windows))]
fn plan_path(path: &Path) -> PathBuf {
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::{FileRecord, ProposedGroup};
    use crate::discover::DiscoverSkip;
    use crate::model::{FileKind, Warning};

    fn record(path: &Path) -> FileRecord {
        FileRecord {
            path: path.to_path_buf(),
            kind: FileKind::Jpeg,
            size: 1,
            mtime_ms: 0,
            fingerprint: "0".into(),
            exif: None,
            hashes: None,
            normalised: None,
            paired_with: None,
            error: None,
        }
    }

    #[test]
    fn paths_are_relative_inside_the_input_and_errors_merge_sorted() {
        let input = PathBuf::from("/photos/trip");
        let config = JobConfig::new(input.clone());
        let mut files: Vec<FileRecord> = ["a.cr3", "a.jpg", "b.jpg", "sub/c.jpg"]
            .iter()
            .map(|rel| record(&input.join(rel)))
            .collect();
        files[1].paired_with = Some(0);
        files[0].error = Some("broken".into());
        files.push(record(Path::new("/elsewhere/d.jpg")));
        let analysis = Analysis {
            files,
            groups: vec![ProposedGroup {
                members: vec![3, 2],
                bucket: "k".into(),
                max_distance: 4,
            }],
            unstacked: vec![4],
            warnings: vec![(4, Warning::NoCaptureTime)],
            skipped: vec![DiscoverSkip {
                path: input.join("a.png"),
                reason: "denied".into(),
            }],
        };

        let plan = PlanV1::from_analysis(&config, &analysis);
        assert_eq!(plan.version, PlanV1::VERSION);
        assert_eq!(plan.input_dir, input);
        assert_eq!(
            plan.groups[0].files,
            [Path::new("sub").join("c.jpg"), PathBuf::from("b.jpg")]
        );
        assert_eq!(plan.unstacked, [PathBuf::from("/elsewhere/d.jpg")]);
        assert_eq!(
            plan.paired,
            [PlanPair {
                passive: "a.jpg".into(),
                active: "a.cr3".into()
            }]
        );
        let errors: Vec<(&Path, &str)> = plan
            .errors
            .iter()
            .map(|e| (e.file.as_path(), e.error.as_str()))
            .collect();
        assert_eq!(
            errors,
            [
                (Path::new("a.cr3"), "broken"),
                (Path::new("a.png"), "denied")
            ]
        );
        assert_eq!(plan.warnings[0].warning, Warning::NoCaptureTime.to_string());
    }

    #[test]
    fn plan_warning_has_a_stable_code() {
        let input = PathBuf::from("/photos");
        let config = JobConfig::new(input.clone());
        let reason = || "x".to_string();
        let warnings = [
            (Warning::NoCaptureTime, "no_capture_time"),
            (
                Warning::ExifUnreadable { reason: reason() },
                "exif_unreadable",
            ),
            (
                Warning::RawPreviewOnly { reason: reason() },
                "raw_preview_only",
            ),
            (
                Warning::PromotedOverRaw { reason: reason() },
                "promoted_over_raw",
            ),
            (
                Warning::ResolutionMismatch {
                    width: 1,
                    height: 2,
                    canvas_width: 3,
                    canvas_height: 4,
                },
                "resolution_mismatch",
            ),
            (Warning::IccMismatch, "icc_mismatch"),
            (
                Warning::LayerUnreadable { reason: reason() },
                "layer_unreadable",
            ),
        ];
        let analysis = Analysis {
            files: vec![record(&input.join("a.jpg"))],
            groups: Vec::new(),
            unstacked: vec![0],
            warnings: warnings.iter().map(|(w, _)| (0, w.clone())).collect(),
            skipped: Vec::new(),
        };

        let plan = PlanV1::from_analysis(&config, &analysis);
        let codes: Vec<&str> = plan.warnings.iter().map(|w| w.code.as_str()).collect();
        let expected: Vec<&str> = warnings.iter().map(|&(_, code)| code).collect();
        assert_eq!(codes, expected);
        let json = serde_json::to_string(&plan.warnings[0]).unwrap();
        assert_eq!(
            json,
            format!(
                r#"{{"file":"a.jpg","code":"no_capture_time","warning":"{}"}}"#,
                Warning::NoCaptureTime
            )
        );
    }

    /// Windows paths are written with `/`, so a plan reads the same on every
    /// platform; a verbatim path, where `/` is no separator, is kept.
    #[cfg(windows)]
    #[test]
    fn plan_paths_use_forward_slashes() {
        let input = PathBuf::from(r"C:\photos\trip");
        let config = JobConfig::new(input.clone());
        let mut files: Vec<FileRecord> = [r"sub\a.jpg", r"sub\b.jpg"]
            .iter()
            .map(|rel| record(&input.join(rel)))
            .collect();
        files[1].paired_with = Some(0);
        files.push(record(Path::new(r"D:\elsewhere\c.jpg")));
        files[2].error = Some("broken".into());
        let analysis = Analysis {
            files,
            groups: vec![ProposedGroup {
                members: vec![0],
                bucket: "k".into(),
                max_distance: 0,
            }],
            unstacked: vec![0],
            warnings: vec![(0, Warning::NoCaptureTime)],
            skipped: vec![DiscoverSkip {
                path: input.join(r"locked\d.jpg"),
                reason: "denied".into(),
            }],
        };

        let plan = PlanV1::from_analysis(&config, &analysis);
        let json = serde_json::to_string(&plan).unwrap();
        assert!(!json.contains('\\'), "{json}");
        assert!(json.contains(r#""input_dir":"C:/photos/trip""#), "{json}");
        assert!(
            json.contains(r#""output_dir":"C:/photos/trip/stacked""#),
            "{json}"
        );
        for path in [
            r#""sub/a.jpg""#,
            r#""sub/b.jpg""#,
            r#""D:/elsewhere/c.jpg""#,
            r#""locked/d.jpg""#,
        ] {
            assert!(json.contains(path), "{path} in {json}");
        }

        let verbatim = Path::new(r"\\?\C:\photos\a.jpg");
        assert_eq!(plan_path(verbatim), verbatim);
    }

    /// On Unix `\` is a character of a name, so paths are kept as written.
    #[cfg(not(windows))]
    #[test]
    fn plan_paths_use_forward_slashes() {
        assert_eq!(plan_path(Path::new("a/b.jpg")), Path::new("a/b.jpg"));
    }
}
