//! The ML models of Stage 3 (spec §8): the registry and the installed files,
//! always compiled, and behind the `ml` feature the verified download and
//! import of a model, where ONNX Runtime is and whether it can be loaded, and
//! the embedder that runs it.

#[cfg(feature = "ml")]
pub mod embed;
#[cfg(feature = "ml")]
pub mod install;
mod lock;
mod registry;
#[cfg(feature = "ml")]
pub mod runtime;

use std::fmt;
use std::io;
use std::path::PathBuf;

pub use registry::{MODELS, ModelSpec, installed, model, model_file, remove, url_host};

/// A download address that failed, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlFailure {
    pub url: String,
    pub reason: String,
}

impl fmt::Display for UrlFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.url, self.reason)
    }
}

/// `failures`, each `<url>: <reason>`, joined with "; ".
fn joined(failures: &[UrlFailure]) -> String {
    let failures: Vec<String> = failures.iter().map(UrlFailure::to_string).collect();
    failures.join("; ")
}

/// `bytes` in whole mebibytes, rounded to the nearest, as the app shows sizes
/// (`MB`, as file managers write it).
fn megabytes(bytes: u64) -> u64 {
    const MIB: u64 = 1 << 20;
    bytes / MIB + u64::from(bytes % MIB >= MIB / 2)
}

/// Why a model could not be downloaded, imported, loaded or run.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    /// Every URL of the model failed; one entry per URL, in order, with its
    /// reason. Nothing was installed.
    #[error("every download URL failed: {}", joined(.0))]
    AllUrlsFailed(Vec<UrlFailure>),
    /// The file to import, or the installed file to load, is not the
    /// registry's file of the model.
    #[error(
        "{} does not match {id}: its SHA-256 is {actual}, the registry's is {expected}",
        path.display()
    )]
    HashMismatch {
        path: PathBuf,
        id: &'static str,
        expected: &'static str,
        actual: String,
    },
    /// The file to import (at `path`) is `size` bytes, not the `expected`
    /// size of the model named `name`: refused before anything is copied.
    #[error(
        "this file is {} MB, {name} is {} MB",
        megabytes(*size),
        megabytes(*expected)
    )]
    WrongSize {
        path: PathBuf,
        name: &'static str,
        size: u64,
        expected: u64,
    },
    /// The download was cancelled; its `.part` file is kept, so the next
    /// download resumes it.
    #[error("the download was cancelled")]
    Cancelled,
    /// Another download, import or removal of the model holds its lock,
    /// in this process or another.
    #[error("{0} is already being downloaded, imported or removed")]
    Busy(&'static str),
    /// The model's inputs or outputs are not what the registry says, such
    /// as an embedding length other than its `dims`.
    #[error("unexpected model shape: {0}")]
    Shape(String),
    /// ONNX Runtime could not load or run the model.
    #[error("ONNX Runtime: {0}")]
    Runtime(String),
    /// A local file could not be read or written.
    #[error(transparent)]
    Io(#[from] io::Error),
}
