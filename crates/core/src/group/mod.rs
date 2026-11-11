//! Grouping: Stage 1 splits the files into time/camera buckets, and the later
//! stages link the photos of each bucket into stacks. At write time, the
//! canvas profile decides how each stack is written.

mod bucket;
mod canvas;
mod link;
mod order;
mod regroup;
mod runs;

use std::path::PathBuf;

use crate::exif::{BracketMode, ExifData};

pub use bucket::{Bucket, bucket};
pub use canvas::{CanvasProfile, Container, profile};
pub use link::{Linked, link, max_pairwise_distance};
pub use order::order_layers;
pub use regroup::{
    ExistingGroup, ManualGroup, RegroupInput, Regrouped, RegroupedGroup, Thresholds, regroup,
};
pub use runs::{BRACKET_RUN_MAX_DISTANCE, bracket_run_links};

/// What grouping knows about one file. A file's id is its index in the slice
/// of facts handed to the grouping functions.
#[derive(Debug, Clone, PartialEq)]
pub struct FileFacts {
    pub path: PathBuf,
    pub exif: Option<ExifData>,
    /// Modification time, in milliseconds since the Unix epoch.
    pub mtime_ms: i64,
}

impl FileFacts {
    /// Where the file sits on the timeline, in milliseconds since the Unix
    /// epoch: its EXIF capture instant, or else its modification time.
    pub(crate) fn time_ms(&self) -> i64 {
        self.exif
            .as_ref()
            .and_then(ExifData::capture_instant_ms)
            .unwrap_or(self.mtime_ms)
    }

    /// The position in a bracket sequence; `None` when the camera did not
    /// record it. A shot whose bracket mode is off never counts, whatever
    /// number it carries.
    pub(crate) fn bracket_shot(&self) -> Option<u16> {
        self.exif
            .as_ref()
            .and_then(|exif| exif.bracket)
            .filter(|bracket| bracket.mode != BracketMode::Off)
            .and_then(|bracket| bracket.shot_number)
    }
}
