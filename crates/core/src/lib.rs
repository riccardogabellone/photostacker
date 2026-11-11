//! Photostacker core: finds series of geometrically identical photos in a folder
//! (exposure brackets, HDR sets, fast bursts) and groups them into stacks.
//!
//! This crate is headless: it never depends on a UI toolkit or on argument parsing.

pub mod analyze;
pub mod appdirs;
pub mod budget;
pub mod decode;
pub mod discover;
pub mod engine;
pub mod exif;
pub mod fsio;
pub mod group;
pub mod guard;
pub mod hash;
pub mod icc;
pub mod imageops;
mod lock;
pub mod model;
pub mod models;
pub mod package;
pub mod pairing;
pub mod plan;
pub mod store;
pub mod thumbs;
pub mod write;

pub use analyze::{
    Analysis, AnalyzeError, AnalyzeHooks, ExamCache, ExamRole, FailureHook, FileExam, FileRecord,
    Progress, ProposedGroup, Stage, analyze, analyze_with,
};
pub use appdirs::AppDirs;
pub use engine::{Engine, EngineError, EngineEvent, EngineOptions};
pub use model::{ConfigError, JobConfig};
pub use plan::{PlanError, PlanGroup, PlanPair, PlanV1, PlanWarning};
pub use store::{Store, StoreError};
pub use write::{
    StackInput, WriteError, WriteObserver, WriteOptions, WriteOutcome, WriteReport, write_all,
    write_group, write_groups, write_stack,
};
