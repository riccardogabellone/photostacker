//! `photostacker-cli info` (spec §10): what this build holds and whether the
//! visual check can run here, in four lines, for a package's smoke test and
//! for a bug report. It is a report: whatever it finds, it succeeds, and it
//! creates no folder and no file.

use std::path::Path;

use photostacker_core::AppDirs;
use photostacker_core::fsio::display_path;

use crate::{Failure, print_lines};

pub(crate) fn info() -> Result<(), Failure> {
    let dirs = AppDirs::resolve();
    print_lines(&[
        format!("photostacker-cli {}", env!("CARGO_PKG_VERSION")),
        heic_line(),
        visual_check_line(),
        data_line(dirs.as_ref().map(|dirs| dirs.data.as_path())),
    ])
}

/// libheif's version and its HEVC decoders.
#[cfg(feature = "heic")]
fn heic_line() -> String {
    let codecs = photostacker_core::decode::heic::codecs();
    let decoders = if codecs.hevc_decoders.is_empty() {
        "none".to_owned()
    } else {
        codecs.hevc_decoders.join(", ")
    };
    format!("heic: libheif {}, HEVC decoder {decoders}", codecs.version)
}

#[cfg(not(feature = "heic"))]
fn heic_line() -> String {
    "heic: not built in".to_owned()
}

/// ONNX Runtime's version and the library it was loaded from, or why it
/// cannot be used, in the words a model load would use. Loading it is what
/// tells: this makes the process's environment, as the visual check does.
#[cfg(feature = "ml")]
fn visual_check_line() -> String {
    use photostacker_core::models::{ModelError, runtime};

    match runtime::loaded_version() {
        Ok(version) => format!(
            "visual check: ONNX Runtime {version} at {}",
            display_path(runtime::environment_library())
        ),
        Err(ModelError::Runtime(reason)) => format!("visual check: {reason}"),
        Err(error) => format!("visual check: {error}"),
    }
}

#[cfg(not(feature = "ml"))]
fn visual_check_line() -> String {
    "visual check: not built in".to_owned()
}

/// The app's data folder, or `none` when there is no home folder to put it
/// in.
fn data_line(data: Option<&Path>) -> String {
    match data {
        Some(data) => format!("data: {}", display_path(data)),
        None => "data: none".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_home_folder_there_is_no_data_folder() {
        assert_eq!(data_line(None), "data: none");
        assert_eq!(
            data_line(Some(Path::new("/srv/photostacker"))),
            format!("data: {}", Path::new("/srv/photostacker").display())
        );
    }
}
