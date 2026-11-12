//! The embedder of Stage 3 (spec §8.3): a registry model run on ONNX
//! Runtime's CPU execution provider. Only the `ml` feature builds it.
//!
//! Every registry model takes the same input and gives the same output
//! (spec §8.1): images resized to 224×224 (bicubic, no crop), scaled to
//! 0..1, normalised with the ImageNet mean and std, as CHW float32; the
//! embedding is the CLS token of `last_hidden_state`, L2-normalised.

use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::{Mutex, OnceLock, PoisonError};

use image::RgbImage;
use image::imageops::{self, FilterType};
use ort::environment::Environment;
use ort::session::Session;
use ort::value::Tensor;

use super::install::hash_copy;
use super::runtime::{environment_library, library_problem};
use super::{ModelError, ModelSpec};
use crate::guard;

/// The side of the square image every model takes.
const SIDE: u32 = 224;
/// The images run through the model at a time.
const BATCH: usize = 8;
/// The ImageNet mean and std of each channel, R, G, B.
const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const STD: [f32; 3] = [0.229, 0.224, 0.225];
/// The output holding one row per token, the CLS token first.
const OUTPUT: &str = "last_hidden_state";

/// A loaded model, ready to embed images. It can be shared between threads;
/// one batch runs through the model at a time.
pub struct Embedder {
    /// ONNX Runtime runs a session through `&mut`, hence the lock.
    session: Mutex<Session>,
    /// The name of the model's first input, which takes the pixels.
    input: String,
    /// The embedding length the registry gives.
    dims: usize,
}

impl Embedder {
    /// Loads the model file at `path` as the model `spec`. The file must be
    /// the registry's (its SHA-256 is checked first, else
    /// [`ModelError::HashMismatch`]). ONNX Runtime missing or failing to
    /// load ([`environment_library`]), or refusing the model, gives
    /// [`ModelError::Runtime`].
    pub fn load(spec: &ModelSpec, path: &Path) -> Result<Embedder, ModelError> {
        let actual = hash_copy(File::open(path)?, io::sink())?;
        if actual != spec.sha256 {
            return Err(ModelError::HashMismatch {
                path: path.to_path_buf(),
                id: spec.id,
                expected: spec.sha256,
                actual,
            });
        }
        let session = session(&environment()?, path)?;
        let input = session
            .inputs()
            .first()
            .ok_or_else(|| ModelError::Shape(format!("{} takes no input", spec.id)))?
            .name()
            .to_owned();
        Ok(Embedder {
            session: Mutex::new(session),
            input,
            dims: spec.dims,
        })
    }

    /// The embeddings of `images`, in order, each of the registry's `dims`
    /// and L2-normalised. The images run in batches of 8.
    ///
    /// A model whose `last_hidden_state` is not `[batch, tokens, dims]`
    /// gives [`ModelError::Shape`].
    pub fn embed(&self, images: &[RgbImage]) -> Result<Vec<Vec<f32>>, ModelError> {
        let mut embeddings = Vec::with_capacity(images.len());
        for batch in images.chunks(BATCH) {
            embeddings.extend(self.embed_batch(batch)?);
        }
        Ok(embeddings)
    }

    fn embed_batch(&self, batch: &[RgbImage]) -> Result<Vec<Vec<f32>>, ModelError> {
        let side = SIDE as usize;
        let mut pixels = Vec::with_capacity(batch.len() * 3 * side * side);
        for image in batch {
            push_chw(&mut pixels, image);
        }
        let input = Tensor::from_array(([batch.len(), 3, side, side], pixels))?;

        let mut session = self.session.lock().unwrap_or_else(PoisonError::into_inner);
        let outputs = session.run(ort::inputs![self.input.as_str() => input])?;
        let output = outputs
            .get(OUTPUT)
            .ok_or_else(|| ModelError::Shape(format!("the model has no output {OUTPUT}")))?;
        let (shape, data) = output.try_extract_tensor::<f32>()?;
        cls_embeddings(shape, data, batch.len(), self.dims)
    }
}

/// The embeddings in a `last_hidden_state` of `shape` holding `data`, for
/// `batch` images: each image's CLS row (its first token), L2-normalised.
///
/// The shape must be `[batch, tokens ≥ 1, dims]` with `dims ≥ 1`, else
/// [`ModelError::Shape`]. A NaN or infinity in a CLS row is
/// [`ModelError::Runtime`]: a broken model or runtime, whose embeddings
/// must never be stored.
fn cls_embeddings(
    shape: &[i64],
    data: &[f32],
    batch: usize,
    dims: usize,
) -> Result<Vec<Vec<f32>>, ModelError> {
    let wrong_shape = || {
        ModelError::Shape(format!(
            "{OUTPUT} is {shape:?}, not [{batch}, tokens, {dims}]"
        ))
    };
    let &[rows, tokens, width] = shape else {
        return Err(wrong_shape());
    };
    let len = |dim: i64| usize::try_from(dim).ok();
    let (Some(rows), Some(tokens), Some(width)) = (len(rows), len(tokens), len(width)) else {
        return Err(wrong_shape());
    };
    if rows != batch || tokens == 0 || width != dims || dims == 0 {
        return Err(wrong_shape());
    }
    let row_len = tokens
        .checked_mul(dims)
        .filter(|row_len| row_len.checked_mul(batch) == Some(data.len()))
        .ok_or_else(wrong_shape)?;
    data.chunks_exact(row_len)
        .map(|row| {
            let cls = &row[..dims];
            if cls.iter().all(|value| value.is_finite()) {
                Ok(l2_normalised(cls))
            } else {
                Err(ModelError::Runtime(format!(
                    "the model gave a NaN or infinite value in an embedding ({OUTPUT})"
                )))
            }
        })
        .collect()
}

impl From<ort::Error> for ModelError {
    fn from(error: ort::Error) -> Self {
        ModelError::Runtime(error.to_string())
    }
}

/// The ONNX Runtime session of the model at `path`: the CPU execution
/// provider, with ONNX Runtime's default intra-op threads (one per physical
/// core). Other execution providers (CoreML, DirectML) would be chosen here.
fn session(environment: &Environment, path: &Path) -> ort::Result<Session> {
    Session::builder(environment)?.commit_from_file(path)
}

/// The process's one ONNX Runtime environment (ort allows no second), made
/// on first use from [`environment_library`]. A failure is kept too: the
/// library is looked for once per run.
pub(super) fn environment() -> Result<Environment, ModelError> {
    static ENVIRONMENT: OnceLock<Result<Environment, String>> = OnceLock::new();
    ENVIRONMENT
        .get_or_init(|| create_environment(environment_library()))
        .clone()
        .map_err(ModelError::Runtime)
}

/// Loads ONNX Runtime from `library` and makes the environment, with
/// telemetry off. A library that [`library_problem`] refuses (missing, or
/// needing a newer macOS) is not loaded; one that fails to load is an error
/// too, never a panic: ort reports a failed load as an error, and a panic
/// inside it is caught as a backstop.
fn create_environment(library: &Path) -> Result<Environment, String> {
    if let Some(problem) = library_problem(library) {
        return Err(problem.refusal(library));
    }
    let created = guard::catch(|| {
        ort::init_from(library)
            .map_err(|error| error.to_string())?
            .with_name("photostacker")
            .with_telemetry(false)
            .build()
            .map_err(|error| error.to_string())
    });
    match created {
        Ok(result) => result,
        Err(panic) => Err(format!(
            "ONNX Runtime at {} failed to load: {panic}",
            crate::fsio::display_path(library)
        )),
    }
}

/// Appends `image` as the model's input: resized to 224×224 with a bicubic
/// filter (no crop), each level scaled to 0..1 and normalised with the
/// ImageNet mean and std, one channel plane after another.
fn push_chw(pixels: &mut Vec<f32>, image: &RgbImage) {
    let resized = imageops::resize(image, SIDE, SIDE, FilterType::CatmullRom);
    for channel in 0..3 {
        pixels.extend(
            resized
                .pixels()
                .map(|pixel| (f32::from(pixel[channel]) / 255.0 - MEAN[channel]) / STD[channel]),
        );
    }
}

/// `vector` scaled to length 1; a zero vector stays zero.
fn l2_normalised(vector: &[f32]) -> Vec<f32> {
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm == 0.0 {
        return vector.to_vec();
    }
    vector.iter().map(|x| x / norm).collect()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use image::Rgb;
    use photostacker_testkit::onnx::{cls_mean_model, two_token_model};
    use photostacker_testkit::{expose, scene};
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::models::{model, model_file};

    const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
    const STD: [f32; 3] = [0.229, 0.224, 0.225];

    /// A registry entry for the model file `bytes`, with embeddings of
    /// `dims`. The registry's fields are `'static`, so its strings are
    /// leaked: tests only.
    fn spec_for(bytes: &[u8], dims: usize) -> &'static ModelSpec {
        let sha256: String = Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Box::leak(Box::new(ModelSpec {
            id: "cls-mean",
            name: "Channel means",
            licence: "MIT",
            size: bytes.len() as u64,
            sha256: Box::leak(sha256.into_boxed_str()),
            urls: &[],
            dims,
            default_cosine: 0.85,
        }))
    }

    /// `bytes` written as a model file in `dir`.
    fn model_at(dir: &Path, bytes: &[u8]) -> PathBuf {
        let path = dir.join("model.onnx");
        fs::write(&path, bytes).unwrap();
        path
    }

    /// Whether ONNX Runtime's library is there to run models; without it a
    /// test that needs it prints why it is skipped.
    fn onnx_runtime_present() -> bool {
        let library = environment_library();
        let present = library.is_file();
        if !present {
            println!("skipped: ONNX Runtime not found at {}", library.display());
        }
        present
    }

    /// The tiny model loaded with `dims`, or `None` when ONNX Runtime is
    /// missing, in which case loading must fail with a runtime error.
    fn tiny_embedder_with_dims(dir: &Path, dims: usize) -> Option<Embedder> {
        let bytes = cls_mean_model();
        let loaded = Embedder::load(spec_for(&bytes, dims), &model_at(dir, &bytes));
        if onnx_runtime_present() {
            return Some(loaded.unwrap());
        }
        let error = loaded.err().expect("no model runs without ONNX Runtime");
        assert!(
            matches!(&error, ModelError::Runtime(message) if message.contains("not found")),
            "{error:?}"
        );
        None
    }

    /// The tiny model, loaded with its own dims, 3.
    fn tiny_embedder(dir: &Path) -> Option<Embedder> {
        tiny_embedder_with_dims(dir, 3)
    }

    fn solid(width: u32, height: u32, colour: [u8; 3]) -> RgbImage {
        RgbImage::from_pixel(width, height, Rgb(colour))
    }

    /// The tiny model's embedding of a solid `colour`, by hand: each
    /// channel's normalised level, then the vector L2-normalised.
    fn expected_embedding(colour: [u8; 3]) -> Vec<f32> {
        let channels: Vec<f32> = (0..3)
            .map(|c| (f32::from(colour[c]) / 255.0 - MEAN[c]) / STD[c])
            .collect();
        let norm = channels.iter().map(|x| x * x).sum::<f32>().sqrt();
        channels.iter().map(|x| x / norm).collect()
    }

    fn assert_close(actual: &[f32], expected: &[f32]) {
        assert_eq!(actual.len(), expected.len());
        for (a, e) in actual.iter().zip(expected) {
            assert!((a - e).abs() < 1e-4, "{actual:?} is not {expected:?}");
        }
    }

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn solid_colour_gives_the_normalised_channel_means() {
        let dir = tempfile::tempdir().unwrap();
        let Some(embedder) = tiny_embedder(dir.path()) else {
            return;
        };
        let colour = [200, 100, 30];
        // Not 224×224, nor square: the resize has no crop.
        let embeddings = embedder.embed(&[solid(64, 40, colour)]).unwrap();
        assert_eq!(embeddings.len(), 1);
        assert_close(&embeddings[0], &expected_embedding(colour));
        // By hand: (200/255 - 0.485)/0.229, (100/255 - 0.456)/0.224 and
        // (30/255 - 0.406)/0.225 are 1.30705, -0.28501 and -1.28157; their
        // norm is 1.85257.
        assert_close(&embeddings[0], &[0.70553, -0.15385, -0.69178]);
    }

    #[test]
    fn batches_of_more_than_eight_keep_order() {
        let dir = tempfile::tempdir().unwrap();
        let Some(embedder) = tiny_embedder(dir.path()) else {
            return;
        };
        let colours: Vec<[u8; 3]> = (0..11u8)
            .map(|i| [10 + 20 * i, 240 - 15 * i, (u16::from(i) * 37 % 256) as u8])
            .collect();
        // Sizes differ too: every image is resized on its own.
        let images: Vec<RgbImage> = colours
            .iter()
            .enumerate()
            .map(|(i, &colour)| solid(30 + i as u32, 20 + 2 * i as u32, colour))
            .collect();

        let embeddings = embedder.embed(&images).unwrap();

        assert_eq!(embeddings.len(), 11);
        for (embedding, &colour) in embeddings.iter().zip(&colours) {
            assert_close(embedding, &expected_embedding(colour));
        }
    }

    #[test]
    fn no_images_give_no_embeddings() {
        let dir = tempfile::tempdir().unwrap();
        let Some(embedder) = tiny_embedder(dir.path()) else {
            return;
        };
        assert!(embedder.embed(&[]).unwrap().is_empty());
    }

    #[test]
    fn wrong_dims_is_a_shape_error() {
        let dir = tempfile::tempdir().unwrap();
        // The registry says 384, as dinov2-small does; the model gives 3.
        let Some(embedder) = tiny_embedder_with_dims(dir.path(), 384) else {
            return;
        };

        let error = embedder
            .embed(&[solid(8, 8, [1, 2, 3])])
            .expect_err("dims 3 is not 384");

        assert!(matches!(error, ModelError::Shape(_)), "{error:?}");
        assert!(error.to_string().contains("384"), "{error}");
    }

    #[test]
    fn corrupt_model_file_fails_to_load() {
        if !onnx_runtime_present() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // The registry's hash is the corrupt file's, so it is ONNX Runtime
        // that refuses it.
        let bytes = b"\x08\x07 this is not an ONNX model".to_vec();

        let error = Embedder::load(spec_for(&bytes, 3), &model_at(dir.path(), &bytes))
            .err()
            .expect("garbage does not load");

        assert!(matches!(error, ModelError::Runtime(_)), "{error:?}");
    }

    #[test]
    fn a_model_file_with_another_hash_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = cls_mean_model();
        let spec = spec_for(&bytes, 3);
        let path = model_at(dir.path(), &bytes[..bytes.len() - 1]);

        let error = Embedder::load(spec, &path).err().expect("a truncated file");

        match error {
            ModelError::HashMismatch {
                path: refused,
                id,
                expected,
                ..
            } => assert_eq!((refused, id, expected), (path, "cls-mean", spec.sha256)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_missing_model_file_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = cls_mean_model();
        let error = Embedder::load(spec_for(&bytes, 3), &dir.path().join("model.onnx"))
            .err()
            .expect("no file");
        assert!(matches!(error, ModelError::Io(_)), "{error:?}");
    }

    #[test]
    fn a_missing_onnx_runtime_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("libonnxruntime.so");

        let error = create_environment(&library).expect_err("no library");

        assert_eq!(
            error,
            format!("ONNX Runtime not found at {}", library.display())
        );
    }

    #[test]
    fn a_file_that_is_not_onnx_runtime_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("libonnxruntime.so");
        fs::write(&library, b"not a shared library").unwrap();

        assert!(create_environment(&library).is_err());
    }

    #[test]
    fn the_embedding_is_each_image_s_first_token() {
        // Two images, two tokens of 2: token 1 must not leak in.
        let data = [3.0, 4.0, 9.0, 9.0, 0.0, -2.0, 7.0, 7.0];

        let embeddings = cls_embeddings(&[2, 2, 2], &data, 2, 2).unwrap();

        assert_eq!(embeddings, [vec![0.6, 0.8], vec![0.0, -1.0]]);
    }

    #[test]
    fn a_non_finite_embedding_is_a_runtime_error() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            // The second image's CLS row holds it.
            let data = [1.0, 0.0, 0.0, bad, 0.0, 1.0];

            let error = cls_embeddings(&[2, 1, 3], &data, 2, 3).expect_err("not finite");

            assert!(matches!(error, ModelError::Runtime(_)), "{error:?}");
        }
        // A NaN in a token other than the CLS one is not used.
        assert!(cls_embeddings(&[1, 2, 1], &[1.0, f32::NAN], 1, 1).is_ok());
    }

    #[test]
    fn output_shapes_other_than_the_registry_s_are_shape_errors() {
        let shape_error = |shape: &[i64], data: &[f32], batch, dims| {
            let error = cls_embeddings(shape, data, batch, dims).expect_err("a wrong shape");
            assert!(
                matches!(error, ModelError::Shape(_)),
                "{shape:?}: {error:?}"
            );
        };
        shape_error(&[1, 0], &[], 1, 0); // not rank 3
        shape_error(&[1, 1, 0], &[], 1, 0); // dims 0
        shape_error(&[1, 0, 3], &[], 1, 3); // no token
        shape_error(&[2, 1, 3], &[0.0; 6], 1, 3); // another batch
        shape_error(&[1, 1, 3], &[0.0; 3], 1, 4); // other dims
        shape_error(&[1, -1, 3], &[0.0; 3], 1, 3); // a negative dimension
        shape_error(&[1, 2, 3], &[0.0; 3], 1, 3); // data shorter than the shape
    }

    #[test]
    fn a_model_with_more_tokens_embeds_its_first() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = two_token_model();
        let loaded = Embedder::load(spec_for(&bytes, 3), &model_at(dir.path(), &bytes));
        if !onnx_runtime_present() {
            return;
        }
        let embedder = loaded.unwrap();
        let colours = [[200, 100, 30], [20, 150, 240]];
        let images = colours.map(|colour| solid(16, 16, colour));

        let embeddings = embedder.embed(&images).unwrap();

        // Token 1 is token 0 negated: embedding it would flip every sign.
        for (embedding, colour) in embeddings.iter().zip(colours) {
            assert_close(embedding, &expected_embedding(colour));
        }
    }

    #[test]
    fn l2_normalisation_keeps_a_zero_vector_zero() {
        assert_eq!(l2_normalised(&[3.0, 0.0, -4.0]), [0.6, 0.0, -0.8]);
        assert_eq!(l2_normalised(&[0.0; 3]), [0.0; 3]);
    }

    /// The table of pinned ONNX Runtime builds that the scripts fetch (spec
    /// §8.3): every row must be one this build can load. `ort`'s `api-17`
    /// feature asks for ONNX Runtime 1.17 or later.
    #[test]
    fn the_pinned_onnx_runtimes_can_be_loaded_by_this_build() {
        let table = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/onnxruntime.tsv");
        let text = fs::read_to_string(&table)
            .unwrap_or_else(|error| panic!("{}: {error}", table.display()));
        // The scripts split each line on tabs alone: a carriage return would end up in the
        // last column, the library's path.
        assert!(
            !text.contains('\r'),
            "{} holds a carriage return: save it with LF line endings",
            table.display()
        );
        let rows: Vec<Vec<&str>> = text
            .lines()
            .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
            .map(|line| line.split('\t').collect())
            .collect();
        assert!(!rows.is_empty(), "{} pins nothing", table.display());

        let mut systems = Vec::new();
        for row in &rows {
            let [os, arch, version, archive, sha256, _library] = row[..] else {
                panic!("not six tab-separated columns: {row:?}");
            };
            assert!(
                sha256.len() == 64
                    && sha256
                        .chars()
                        .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "not 64 lowercase hex digits: {sha256}"
            );
            assert!(archive.contains(version), "{archive} is not {version}");
            let numbers: Vec<u32> = version
                .split('.')
                .map(|n| {
                    n.parse()
                        .unwrap_or_else(|error| panic!("version {version:?}: {n:?}: {error}"))
                })
                .collect();
            assert!(
                numbers.len() == 3 && (numbers[0], numbers[1]) >= (1, 17),
                "{version} is older than 1.17"
            );
            // The scripts take the first row of a system: a second is never used.
            assert!(!systems.contains(&(os, arch)), "{os} {arch} twice");
            systems.push((os, arch));
        }
    }

    #[test]
    fn the_embedder_can_be_shared_between_threads() {
        fn shareable<T: Send + Sync>() {}
        shareable::<Embedder>();
    }

    /// The real dinov2-small, from an already downloaded models folder: set
    /// `PHOTOSTACKER_MODELS_DIR` to a folder holding
    /// `dinov2-small/model.onnx`. Skipped, and reported so, without it.
    #[test]
    fn real_dinov2_small_separates_scenes() {
        let Some(models_dir) = std::env::var_os("PHOTOSTACKER_MODELS_DIR") else {
            println!("skipped: PHOTOSTACKER_MODELS_DIR not set");
            return;
        };
        if !onnx_runtime_present() {
            return;
        }
        let spec = model("dinov2-small").unwrap();
        let embedder = Embedder::load(spec, &model_file(Path::new(&models_dir), spec)).unwrap();
        let shot = scene(1, 320, 240);
        let images = [
            shot.clone(),
            expose(&shot, 1.0),
            scene(2, 320, 240),
            shot.clone(),
        ];

        let embeddings = embedder.embed(&images).unwrap();

        assert!(embeddings.iter().all(|embedding| embedding.len() == 384));
        let itself = cosine(&embeddings[0], &embeddings[3]);
        assert!((itself - 1.0).abs() <= 1e-5, "cosine with itself {itself}");
        let exposures = cosine(&embeddings[0], &embeddings[1]);
        let scenes = cosine(&embeddings[0], &embeddings[2]);
        println!("dinov2-small: two exposures {exposures:.4}, two scenes {scenes:.4}");
        assert!(exposures > scenes, "{exposures} <= {scenes}");
    }
}
