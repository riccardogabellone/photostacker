//! The model registry, compiled into the binary (spec §8.1), and the
//! installed model files under the app's models folder.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::lock::try_lock;

/// One model Stage 3 can use: what to download, how to verify it, and what
/// its embeddings are.
///
/// Every model takes the same input (224×224, bicubic resize with no crop,
/// ImageNet mean and std, CHW float32) and pools the CLS token of
/// `last_hidden_state` (spec §8.1, §8.3).
#[derive(Debug, PartialEq)]
pub struct ModelSpec {
    /// The stable id: the models folder, the vec0 table of its embeddings
    /// and `JobConfig::ml_model` all name the model by it.
    pub id: &'static str,
    /// The name shown to the user.
    pub name: &'static str,
    /// The SPDX licence of the weights.
    pub licence: &'static str,
    /// The size of `model.onnx` in bytes.
    pub size: u64,
    /// The SHA-256 of `model.onnx`, lowercase hex.
    pub sha256: &'static str,
    /// Where to download `model.onnx`, tried in order: Hugging Face at a
    /// pinned revision, then our GitHub Release `models-v1`.
    pub urls: &'static [&'static str],
    /// The length of an embedding.
    pub dims: usize,
    /// The Stage-3 cosine threshold when the job sets none.
    pub default_cosine: f32,
}

/// The models of v1, the default first. Their revisions, sizes and hashes
/// were pinned on 2026-11-08 from the Hugging Face API.
pub const MODELS: &[ModelSpec] = &[
    ModelSpec {
        id: "dinov2-small",
        name: "DINOv2 small",
        licence: "Apache-2.0",
        size: 88_459_888,
        sha256: "83141175ec78b4ff9a2bb58a4c7c264ba0054d1c2e122e5a8114b79a8d4179ea",
        urls: &[
            "https://huggingface.co/Xenova/dinov2-small/resolve/c2bb04a51fab207c420665f1946016107bffc701/onnx/model.onnx",
            "https://github.com/riccardogabellone/photostacker/releases/download/models-v1/dinov2-small.onnx",
        ],
        dims: 384,
        // From the calibration of 2026-11-09 on the public bracket sets
        // (docs/calibration.md § "Stage 3"): their positives and negatives
        // overlap, so the rule takes the smallest positive, kept at the
        // range's floor of 0.5. Every cross-set pair is below it.
        default_cosine: 0.50,
    },
    ModelSpec {
        id: "dinov2-base",
        name: "DINOv2 base",
        licence: "Apache-2.0",
        size: 346_555_570,
        sha256: "de9c675d214f0171285f90f1d1d7716bce1c5e280e93826d071d57f7b9314d98",
        urls: &[
            "https://huggingface.co/Xenova/dinov2-base/resolve/d623d125a29e343851bcedff293fdc8b6bda292b/onnx/model.onnx",
            "https://github.com/riccardogabellone/photostacker/releases/download/models-v1/dinov2-base.onnx",
        ],
        dims: 768,
        default_cosine: 0.85,
    },
];

/// The registry entry of `id`, if there is one.
pub fn model(id: &str) -> Option<&'static ModelSpec> {
    MODELS.iter().find(|spec| spec.id == id)
}

/// The host a download address names: `huggingface.co` for
/// `https://huggingface.co/…`, without the path, so it can be shown to the user.
pub fn url_host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split(['/', '?', '#']).next().unwrap_or(rest)
}

/// Where the model `spec` is installed: `<models_dir>/<id>/model.onnx`.
pub fn model_file(models_dir: &Path, spec: &ModelSpec) -> PathBuf {
    models_dir.join(spec.id).join("model.onnx")
}

/// The model is installed: its [`model_file`] exists with exactly
/// `spec.size` bytes. The hash was checked when the file was put there.
pub fn installed(models_dir: &Path, spec: &ModelSpec) -> bool {
    fs::metadata(model_file(models_dir, spec))
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() == spec.size)
}

/// Removes the model's folder `<models_dir>/<id>/`, with its model file and
/// any unfinished download. A model that is not there is already removed.
///
/// A model being downloaded or imported is refused with
/// [`io::ErrorKind::ResourceBusy`]. The lock is held until the folder is
/// gone, its lock file included (an open file can be removed on Unix, and on
/// Windows, where Rust opens files with delete sharing), so a download or
/// import that starts meanwhile is refused as busy. One that opens the lock
/// file just after it is removed locks a new one: the removal then fails
/// (the folder is not empty) or takes that file too, and the download fails
/// when its folder vanishes. Neither installs an unverified file.
pub fn remove(spec: &ModelSpec, models_dir: &Path) -> io::Result<()> {
    let folder = models_dir.join(spec.id);
    if !folder.exists() {
        return Ok(());
    }
    let Some(_lock) = try_lock(&folder)? else {
        return Err(io::Error::new(
            io::ErrorKind::ResourceBusy,
            format!("{} is being downloaded or imported", spec.id),
        ));
    };
    match fs::remove_dir_all(folder) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_matches_the_pinned_values() {
        let pinned = [
            (
                "dinov2-small",
                88_459_888,
                "83141175ec78b4ff9a2bb58a4c7c264ba0054d1c2e122e5a8114b79a8d4179ea",
                [
                    "https://huggingface.co/Xenova/dinov2-small/resolve/c2bb04a51fab207c420665f1946016107bffc701/onnx/model.onnx",
                    "https://github.com/riccardogabellone/photostacker/releases/download/models-v1/dinov2-small.onnx",
                ],
                384,
                0.50,
            ),
            (
                "dinov2-base",
                346_555_570,
                "de9c675d214f0171285f90f1d1d7716bce1c5e280e93826d071d57f7b9314d98",
                [
                    "https://huggingface.co/Xenova/dinov2-base/resolve/d623d125a29e343851bcedff293fdc8b6bda292b/onnx/model.onnx",
                    "https://github.com/riccardogabellone/photostacker/releases/download/models-v1/dinov2-base.onnx",
                ],
                768,
                0.85,
            ),
        ];
        assert_eq!(MODELS.len(), pinned.len());
        // The default cosines: dinov2-small's from the calibration
        // (docs/calibration.md § "Stage 3"), dinov2-base's the value the
        // earlier Python app used with it.
        for (spec, (id, size, sha256, urls, dims, cosine)) in MODELS.iter().zip(pinned) {
            assert_eq!(
                (spec.id, spec.size, spec.sha256, spec.urls, spec.dims),
                (id, size, sha256, &urls[..], dims)
            );
            assert_eq!(spec.licence, "Apache-2.0");
            assert_eq!(spec.default_cosine, cosine, "{id}");
        }
        // The default model comes first.
        assert_eq!(MODELS[0].id, "dinov2-small");
    }

    #[test]
    fn a_url_shows_as_its_host() {
        assert_eq!(url_host(MODELS[0].urls[0]), "huggingface.co");
        assert_eq!(url_host(MODELS[0].urls[1]), "github.com");
        assert_eq!(url_host("http://example.org?x=1"), "example.org");
        assert_eq!(url_host("example.org"), "example.org");
    }

    #[test]
    fn models_are_found_by_id() {
        assert_eq!(model("dinov2-base").map(|spec| spec.dims), Some(768));
        assert_eq!(model("dinov2-large"), None);
        assert_eq!(model(""), None);
    }

    /// A sparse file of `len` bytes at the model's file, so the real sizes
    /// cost no disk.
    fn place_model_file(models_dir: &Path, spec: &ModelSpec, len: u64) {
        let path = model_file(models_dir, spec);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        photostacker_testkit::sparse_file(&path, len).unwrap();
    }

    #[test]
    fn installed_needs_the_file_with_its_exact_size() {
        let models_dir = tempfile::tempdir().unwrap();
        let spec = &MODELS[0];
        assert!(!installed(models_dir.path(), spec));
        place_model_file(models_dir.path(), spec, spec.size - 1);
        assert!(!installed(models_dir.path(), spec));
        place_model_file(models_dir.path(), spec, spec.size);
        assert!(installed(models_dir.path(), spec));
        assert!(!installed(models_dir.path(), &MODELS[1]));
        assert_eq!(
            model_file(models_dir.path(), spec),
            models_dir.path().join("dinov2-small").join("model.onnx")
        );
    }

    #[test]
    fn remove_frees_the_folder() {
        let models_dir = tempfile::tempdir().unwrap();
        let spec = &MODELS[0];
        place_model_file(models_dir.path(), spec, spec.size);
        let part = models_dir.path().join(spec.id).join("model.onnx.part");
        fs::write(&part, b"unfinished").unwrap();
        // The other model is left alone.
        place_model_file(models_dir.path(), &MODELS[1], 1);

        remove(spec, models_dir.path()).unwrap();
        assert!(!models_dir.path().join(spec.id).exists());
        assert!(!installed(models_dir.path(), spec));
        assert!(model_file(models_dir.path(), &MODELS[1]).exists());
        // Removing it again is not an error.
        remove(spec, models_dir.path()).unwrap();
    }

    #[test]
    fn remove_refuses_a_model_being_installed() {
        let models_dir = tempfile::tempdir().unwrap();
        let spec = &MODELS[0];
        place_model_file(models_dir.path(), spec, spec.size);
        let held = try_lock(&models_dir.path().join(spec.id)).unwrap().unwrap();

        let error = remove(spec, models_dir.path()).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::ResourceBusy);
        assert!(installed(models_dir.path(), spec));
        drop(held);
        remove(spec, models_dir.path()).unwrap();
        assert!(!models_dir.path().join(spec.id).exists());
    }
}
