//! Dev-only fixtures for Photostacker tests: synthetic EXIF/TIFF bytes, synthetic scenes,
//! colour-space fixtures (stub ICC profiles, a CMYK JPEG),
//! an independent PSD/PSB reader for the writer's round trips,
//! a local HTTP server for the model downloader,
//! a tiny hand-encoded ONNX model for the embedder,
//! and folders of bracketed and lone photos for the engine and the app.

pub mod colour;
pub mod http;
pub mod onnx;
pub mod photos;
pub mod psd_reader;
pub mod raw_containers;
pub mod scene;
pub mod tiff_builder;

pub use colour::{cmyk_jpeg_with_icc, icc_stub};
pub use raw_containers::{
    CANON_CR3_UUID, CR3_PREVIEW_UUID, RAF_MAGIC, UNKNOWN_MAKE, UNKNOWN_MODEL, cr3_with_cmt,
    cr3_with_prvw, encode_jpeg, raf_with_jpeg, tiff_raw_with_jpegs,
};
pub use scene::{expose, scene, shift};
pub use tiff_builder::{
    MakerNoteSpec, NoteStyle, TiffSpec, Value, exif_value_field, ifd_value_field, jpeg_with_exif,
    jpeg_with_icc, write_rgb16_tiff,
};
