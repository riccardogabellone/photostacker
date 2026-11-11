//! Scenario tests of the stack writers (spec §5.5, §5.6, §6): one per rule,
//! run on synthetic photos written to a temporary folder.
//!
//! The brackets are testkit scenes saved as JPEGs with Canon EXIF (AEB shot
//! numbers and AEBBracketValue) and go through [`analyze`] first. Scenarios
//! about one stack's members build the [`Analysis`] by hand, or call
//! [`write_stack`] directly, so that grouping cannot decide what is tested.
//! Every output is read back with testkit's independent PSD reader or the
//! `tiff` crate.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use image::{ImageBuffer, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};
use photostacker_core::decode::{self, ColourHint, DecodeError, FullImage, Samples};
use photostacker_core::discover::kind_of;
use photostacker_core::group::Container;
use photostacker_core::icc::{adobe_rgb_profile, rgb_profile};
use photostacker_core::model::{OutputFormat, RawTone, Warning};
use photostacker_core::write::{MemberProbe, PredictMember, StackPrediction, predict_stack};
use photostacker_core::{
    Analysis, AnalyzeError, ConfigError, FileRecord, JobConfig, Progress, ProposedGroup,
    StackInput, Stage, WriteError, WriteObserver, WriteOptions, WriteOutcome, analyze, write_all,
    write_group, write_groups, write_stack,
};
use photostacker_testkit::psd_reader::{self, Psd};
use photostacker_testkit::{
    MakerNoteSpec, NoteStyle, TiffSpec, Value, expose, jpeg_with_exif, jpeg_with_icc, scene,
};

const WIDTH: u32 = 640;
const HEIGHT: u32 = 426;
const JPEG_QUALITY: u8 = 92;
/// The image resource id of the ICC profile in a PSD.
const PSD_ICC: u16 = 1039;
/// The minus sign of layer names.
const MINUS: char = '\u{2212}';

// --- Fixtures ---

/// Canon EXIF of AEB shot `shot` (1-based), taken `second` s into the
/// scenario at `ev`: FileInfo says AEB with the shot number, ShotInfo holds
/// AEBBracketValue in Canon's 1/32 EV steps.
fn aeb_exif(second: u32, shot: u16, ev: i16) -> TiffSpec {
    let mut shot_info = vec![0u16; 34];
    shot_info[0] = 68;
    shot_info[17] = (ev * 32) as u16;
    let mut file_info = vec![0u16; 20];
    file_info[0] = 40;
    file_info[3] = 1;
    file_info[5] = shot;
    TiffSpec {
        little_endian: true,
        ifd0: vec![
            (0x010F, Value::Ascii("Canon".into())),
            (0x0110, Value::Ascii("Canon EOS 2000D".into())),
        ],
        exif: vec![
            (
                0x9003,
                Value::Ascii(format!("2026:05:01 10:00:{second:02}")),
            ),
            (0x920A, Value::Rational(vec![(35, 1)])),
        ],
        maker_note: Some(MakerNoteSpec {
            style: NoteStyle::CanonIfd,
            entries: vec![
                (0x0004, Value::Short(shot_info)),
                (0x0093, Value::Short(file_info)),
            ],
        }),
    }
}

/// Writes a bracket of scene 1 to `dir`: `names[i]` is shot `i + 1`, at
/// `evs[i]`, `i` seconds in.
fn bracket(dir: &Path, names: &[&str], evs: &[i16]) {
    let base = scene(1, WIDTH, HEIGHT);
    for (i, (name, &ev)) in names.iter().zip(evs).enumerate() {
        let exif = aeb_exif(i as u32, i as u16 + 1, ev);
        let bytes = jpeg_with_exif(&expose(&base, f32::from(ev)), &exif.build(), JPEG_QUALITY);
        fs::write(dir.join(name), bytes).unwrap();
    }
}

/// Writes AEB shot `shot` (1-based) of scene 1 to `path` at `ev`, with no
/// ICC profile, EXIF ColorSpace `colour_space` and, when given, an Interop
/// IFD whose InteroperabilityIndex is `interop_index`.
fn untagged_shot(path: &Path, shot: u16, ev: i16, colour_space: u16, interop_index: Option<&str>) {
    let mut exif = aeb_exif(u32::from(shot) - 1, shot, ev);
    exif.exif.push((0xA001, Value::Short(vec![colour_space])));
    let block = match interop_index {
        Some(index) => exif.build_with_interop(&[(0x0001, Value::Ascii(index.into()))]),
        None => exif.build(),
    };
    let image = expose(&scene(1, WIDTH, HEIGHT), f32::from(ev));
    fs::write(path, jpeg_with_exif(&image, &block, JPEG_QUALITY)).unwrap();
}

/// The five-shot bracket of the grouping scenarios (EV −2 … +2), whose file
/// names run against the shot order. Returns the names in shot order.
fn bracket_of_five(dir: &Path) -> [&'static str; 5] {
    let names = [
        "IMG_0009.jpg",
        "IMG_0008.jpg",
        "IMG_0007.jpg",
        "IMG_0006.jpg",
        "IMG_0005.jpg",
    ];
    bracket(dir, &names, &[-2, -1, 0, 1, 2]);
    names
}

/// The expected names of the five layers, top first.
fn bracket_of_five_layer_names() -> Vec<String> {
    [
        "IMG_0009 (−2 EV)",
        "IMG_0008 (−1 EV)",
        "IMG_0007 (0 EV)",
        "IMG_0006 (+1 EV)",
        "IMG_0005 (+2 EV)",
    ]
    .map(String::from)
    .to_vec()
}

/// Writes scene `seed` at `width`×`height` as an EXIF-less JPEG, with
/// `icc` embedded when given.
fn plain_jpeg(path: &Path, seed: u64, width: u32, height: u32, icc: Option<&[u8]>) {
    let image = scene(seed, width, height);
    let bytes = match icc {
        Some(icc) => jpeg_with_icc(&image, icc, JPEG_QUALITY),
        None => {
            let mut bytes = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, JPEG_QUALITY)
                .encode_image(&image)
                .unwrap();
            bytes
        }
    };
    fs::write(path, bytes).unwrap();
}

/// An analysis of `paths` (without EXIF) whose groups are `groups`, lists
/// of indices into `paths` in layer order: grouping plays no part.
fn analysis_of(paths: &[PathBuf], groups: &[&[usize]]) -> Analysis {
    let files = paths
        .iter()
        .map(|path| FileRecord {
            path: path.clone(),
            kind: kind_of(path).unwrap(),
            size: fs::metadata(path).unwrap().len(),
            mtime_ms: 0,
            fingerprint: String::new(),
            exif: None,
            hashes: None,
            normalised: None,
            paired_with: None,
            error: None,
        })
        .collect();
    let groups = groups
        .iter()
        .map(|members| ProposedGroup {
            members: members.to_vec(),
            bucket: "test".into(),
            max_distance: 0,
        })
        .collect();
    Analysis {
        files,
        groups,
        unstacked: Vec::new(),
        warnings: Vec::new(),
        skipped: Vec::new(),
    }
}

/// The stack input of an EXIF-less file, as `write_group` builds it.
fn file_input(path: &Path) -> StackInput {
    let kind = kind_of(path).unwrap();
    let owned = path.to_path_buf();
    StackInput {
        stem: path.file_stem().unwrap().to_str().unwrap().to_owned(),
        exif: None,
        probe: decode::probe(path, kind, 1, RawTone::Srgb, ColourHint::None),
        load: Box::new(move || decode::full(&owned, kind, 1, RawTone::Srgb, ColourHint::None)),
    }
}

/// `file_input(path)` whose load gives `load()` instead of the file.
fn input_loading(
    path: &Path,
    load: impl FnOnce() -> Result<FullImage, DecodeError> + Send + 'static,
) -> StackInput {
    StackInput {
        load: Box::new(load),
        ..file_input(path)
    }
}

fn full(path: &Path) -> FullImage {
    let kind = kind_of(path).unwrap();
    decode::full(path, kind, 1, RawTone::Srgb, ColourHint::None).unwrap()
}

// --- Running the writers ---

fn never() -> AtomicBool {
    AtomicBool::new(false)
}

fn ignore(_: Progress) {}

fn analyze_ok(config: &JobConfig) -> Analysis {
    analyze(config, &ignore, &never()).unwrap()
}

/// The id of the file `name` directly in `dir`.
fn id_of(analysis: &Analysis, dir: &Path, name: &str) -> usize {
    let path = dir.join(name);
    analysis
        .files
        .iter()
        .position(|f| f.path == path)
        .unwrap_or_else(|| panic!("{name} not in the analysis"))
}

/// The path, layer count and container of a written outcome.
fn written(outcome: &WriteOutcome) -> (&Path, usize, Container) {
    match outcome {
        WriteOutcome::Written {
            path,
            layers,
            container,
        } => (path, *layers, *container),
        WriteOutcome::Skipped { reason } => panic!("skipped: {reason}"),
    }
}

fn file_name(path: &Path) -> &str {
    path.file_name().unwrap().to_str().unwrap()
}

/// The names of the entries of `dir`, sorted.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn read_psd(path: &Path) -> Psd {
    psd_reader::read(&fs::read(path).unwrap()).unwrap()
}

/// The `luni` names of the layers, top layer first.
fn layer_names(layers: &[psd_reader::Layer]) -> Vec<String> {
    layers
        .iter()
        .rev()
        .map(|layer| layer.name_unicode.clone().unwrap())
        .collect()
}

fn icc_of(psd: &Psd) -> Option<&[u8]> {
    psd.resources
        .iter()
        .find(|(id, _)| *id == PSD_ICC)
        .map(|(_, data)| data.as_slice())
}

/// The planes of `image`'s R, G and B (8-bit samples as `v as u16`).
fn planes(image: &FullImage) -> Vec<Vec<u16>> {
    let channels = usize::from(image.channels);
    let samples: Vec<u16> = match &image.samples {
        Samples::U8(v) => v.iter().map(|&s| u16::from(s)).collect(),
        Samples::U16(v) => v.clone(),
    };
    (0..3)
        .map(|c| samples.iter().skip(c).step_by(channels).copied().collect())
        .collect()
}

/// The decoded channels R, G and B of a read-back layer.
fn layer_planes(layer: &psd_reader::Layer) -> Vec<Vec<u16>> {
    (0..3)
        .map(|id| {
            let (_, samples) = layer.channels.iter().find(|(c, _)| *c == id).unwrap();
            samples.clone()
        })
        .collect()
}

// --- Scenarios ---

#[test]
fn bracket_of_five_writes_one_psd_top_layer_first() {
    let dir = tempfile::tempdir().unwrap();
    let names = bracket_of_five(dir.path());
    let config = JobConfig::new(dir.path().to_path_buf());
    let analysis = analyze_ok(&config);
    assert_eq!(analysis.groups.len(), 1, "{:?}", analysis.groups);

    let (outcome, warnings) = write_group(
        &config,
        &analysis,
        0,
        WriteOptions::default(),
        &ignore,
        &never(),
    )
    .unwrap();
    let (path, layers, container) = written(&outcome);
    assert_eq!(file_name(path), "IMG_0009_5.psd");
    assert_eq!((layers, container), (5, Container::Psd));
    assert_eq!(
        path.parent(),
        Some(config.output_dir.canonicalize().unwrap().as_path())
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(entries(&config.output_dir), ["IMG_0009_5.psd"]);

    let psd = read_psd(path);
    assert_eq!((psd.version, psd.depth, psd.layer_count), (1, 8, 5));
    assert_eq!(layer_names(&psd.layers), bracket_of_five_layer_names());
    // The ASCII fallback swaps the minus for a hyphen.
    assert_eq!(psd.layers[4].name_pascal, "IMG_0009 (-2 EV)");
    let top = full(&dir.path().join(names[0]));
    assert_eq!(psd.merged, planes(&top), "the composite is shot 1");
    assert_eq!(layer_planes(&psd.layers[4]), planes(&top));
    let bottom = full(&dir.path().join(names[4]));
    assert_eq!(layer_planes(&psd.layers[0]), planes(&bottom));
}

#[test]
fn existing_output_is_never_overwritten() {
    // A second run adds `-1` and leaves the first file byte-identical.
    let dir = tempfile::tempdir().unwrap();
    bracket_of_five(dir.path());
    let config = JobConfig::new(dir.path().to_path_buf());
    let analysis = analyze_ok(&config);
    let run = || {
        write_all(
            &config,
            &analysis,
            WriteOptions::default(),
            &ignore,
            &never(),
        )
    };
    let first = run().unwrap();
    let (first_path, ..) = written(first.outcomes[0].1.as_ref().unwrap());
    assert_eq!(file_name(first_path), "IMG_0009_5.psd");
    let first_bytes = fs::read(first_path).unwrap();
    let second = run().unwrap();
    let (second_path, ..) = written(second.outcomes[0].1.as_ref().unwrap());
    assert_eq!(file_name(second_path), "IMG_0009_5-1.psd");
    assert_eq!(fs::read(first_path).unwrap(), first_bytes);
    assert_eq!(
        layer_names(&read_psd(second_path).layers),
        bracket_of_five_layer_names()
    );
    assert_eq!(
        entries(&config.output_dir),
        ["IMG_0009_5-1.psd", "IMG_0009_5.psd"]
    );

    // An unrelated file under the final name is left alone.
    let dir = tempfile::tempdir().unwrap();
    bracket_of_five(dir.path());
    let config = JobConfig::new(dir.path().to_path_buf());
    fs::create_dir(&config.output_dir).unwrap();
    let user_file = config.output_dir.join("IMG_0009_5.psd");
    fs::write(&user_file, b"the user's own file").unwrap();
    let analysis = analyze_ok(&config);
    let report = write_all(
        &config,
        &analysis,
        WriteOptions::default(),
        &ignore,
        &never(),
    )
    .unwrap();
    let (path, ..) = written(report.outcomes[0].1.as_ref().unwrap());
    assert_eq!(file_name(path), "IMG_0009_5-1.psd");
    assert_eq!(fs::read(&user_file).unwrap(), b"the user's own file");
    assert_eq!(
        entries(&config.output_dir),
        ["IMG_0009_5-1.psd", "IMG_0009_5.psd"]
    );

    // Leftovers of a crashed run under the provisional names are neither
    // reused nor removed: the partial moves on to the first free name, and
    // the output still takes the first free final name.
    let dir = tempfile::tempdir().unwrap();
    bracket_of_five(dir.path());
    let config = JobConfig::new(dir.path().to_path_buf());
    fs::create_dir(&config.output_dir).unwrap();
    let leftovers = ["IMG_0009_5.psd.partial", "IMG_0009_5-1.psd.partial.merged"];
    for name in leftovers {
        fs::write(config.output_dir.join(name), b"crashed").unwrap();
    }
    let analysis = analyze_ok(&config);
    let report = write_all(
        &config,
        &analysis,
        WriteOptions::default(),
        &ignore,
        &never(),
    )
    .unwrap();
    let (path, ..) = written(report.outcomes[0].1.as_ref().unwrap());
    assert_eq!(file_name(path), "IMG_0009_5.psd");
    for name in leftovers {
        assert_eq!(fs::read(config.output_dir.join(name)).unwrap(), b"crashed");
    }
    assert_eq!(
        entries(&config.output_dir),
        [
            "IMG_0009_5-1.psd.partial.merged",
            "IMG_0009_5.psd",
            "IMG_0009_5.psd.partial"
        ]
    );
}

#[test]
fn failed_write_leaves_no_partial_or_spill() {
    // A cancel from the progress callback, after the first layer.
    let dir = tempfile::tempdir().unwrap();
    bracket_of_five(dir.path());
    let config = JobConfig::new(dir.path().to_path_buf());
    fs::create_dir(&config.output_dir).unwrap();
    fs::write(config.output_dir.join("notes.txt"), b"keep").unwrap();
    let analysis = analyze_ok(&config);
    let cancel = never();
    let progress = |p: Progress| {
        if p.stage == Stage::Write && p.done == 1 {
            cancel.store(true, Ordering::Relaxed);
        }
    };
    let result = write_all(
        &config,
        &analysis,
        WriteOptions::default(),
        &progress,
        &cancel,
    );
    assert!(matches!(result, Err(WriteError::Cancelled)), "{result:?}");
    assert_eq!(entries(&config.output_dir), ["notes.txt"]);

    // A read-only output folder: the group fails, and nothing is left.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        bracket_of_five(dir.path());
        let config = JobConfig::new(dir.path().to_path_buf());
        fs::create_dir(&config.output_dir).unwrap();
        let analysis = analyze_ok(&config);
        let set_mode = |mode| {
            fs::set_permissions(&config.output_dir, fs::Permissions::from_mode(mode)).unwrap();
        };
        set_mode(0o555);
        let probe = config.output_dir.join("probe");
        if fs::write(&probe, b"").is_ok() {
            // Root ignores the mode; the scenario cannot be staged.
            fs::remove_file(&probe).unwrap();
            println!("SKIPPED: the read-only folder is writable (running as root?)");
        } else {
            let report = write_all(
                &config,
                &analysis,
                WriteOptions::default(),
                &ignore,
                &never(),
            )
            .unwrap();
            assert_eq!(report.outcomes.len(), 1);
            let (group, outcome) = &report.outcomes[0];
            assert_eq!(*group, 0);
            let error = outcome.as_ref().unwrap_err();
            assert!(error.contains("ermission denied"), "{error}");
            assert!(entries(&config.output_dir).is_empty());
        }
        set_mode(0o755);
    }
}

/// Spec §5.6: `JobConfig::validate()`, which `analyze` runs first, rejects
/// an output that exists and is not a folder, and leaves it untouched.
#[test]
fn output_that_is_a_file_fails_validation() {
    let dir = tempfile::tempdir().unwrap();
    bracket_of_five(dir.path());
    let mut config = JobConfig::new(dir.path().to_path_buf());
    config.output_dir = dir.path().join("stacked.psd");
    fs::write(&config.output_dir, b"the user's own file").unwrap();
    let expected = ConfigError::OutputNotADirectory(config.output_dir.clone());
    assert_eq!(config.validate(), Err(expected.clone()));
    match analyze(&config, &ignore, &never()) {
        Err(AnalyzeError::Config(error)) => assert_eq!(error, expected),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        fs::read(&config.output_dir).unwrap(),
        b"the user's own file"
    );
}

#[test]
fn unusable_output_folder_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    bracket_of_five(dir.path());
    let mut config = JobConfig::new(dir.path().to_path_buf());
    let analysis = analyze_ok(&config);
    config.output_dir = dir.path().join("IMG_0009.jpg");
    let before = fs::read(&config.output_dir).unwrap();
    let result = write_all(
        &config,
        &analysis,
        WriteOptions::default(),
        &ignore,
        &never(),
    );
    assert!(
        matches!(result, Err(WriteError::OutputDir(_))),
        "{result:?}"
    );
    let result = write_group(
        &config,
        &analysis,
        0,
        WriteOptions::default(),
        &ignore,
        &never(),
    );
    assert!(
        matches!(result, Err(WriteError::OutputDir(_))),
        "{result:?}"
    );
    assert_eq!(fs::read(&config.output_dir).unwrap(), before);
}

#[test]
fn member_unreadable_at_write_time_is_excluded() {
    // Three shots; the top one is cut off after the analysis. The stack is
    // named after the first layer actually written, with the count written.
    let dir = tempfile::tempdir().unwrap();
    let names = ["IMG_0101.jpg", "IMG_0102.jpg", "IMG_0103.jpg"];
    bracket(dir.path(), &names, &[0, -1, 1]);
    let config = JobConfig::new(dir.path().to_path_buf());
    let analysis = analyze_ok(&config);
    assert_eq!(analysis.groups.len(), 1, "{:?}", analysis.groups);
    let cut = dir.path().join(names[0]);
    let bytes = fs::read(&cut).unwrap();
    fs::write(&cut, &bytes[..bytes.len() / 2]).unwrap();

    let report = write_all(
        &config,
        &analysis,
        WriteOptions::default(),
        &ignore,
        &never(),
    )
    .unwrap();
    let (path, layers, _) = written(report.outcomes[0].1.as_ref().unwrap());
    assert_eq!((file_name(path), layers), ("IMG_0102_2.psd", 2));
    let cut_id = id_of(&analysis, dir.path(), names[0]);
    assert!(
        matches!(
            report.warnings.as_slice(),
            [(id, Warning::LayerUnreadable { .. })] if *id == cut_id
        ),
        "{:?}",
        report.warnings
    );
    let psd = read_psd(path);
    assert_eq!(
        layer_names(&psd.layers),
        [format!("IMG_0102 ({MINUS}1 EV)"), "IMG_0103 (+1 EV)".into()]
    );
    assert_eq!(psd.merged, planes(&full(&dir.path().join(names[1]))));
    assert_eq!(entries(&config.output_dir), ["IMG_0102_2.psd"]);

    // Two shots, one cut off: nothing is left to stack.
    let dir = tempfile::tempdir().unwrap();
    let names = ["IMG_0201.jpg", "IMG_0202.jpg"];
    bracket(dir.path(), &names, &[0, 1]);
    let config = JobConfig::new(dir.path().to_path_buf());
    let analysis = analyze_ok(&config);
    assert_eq!(analysis.groups.len(), 1, "{:?}", analysis.groups);
    let cut = dir.path().join(names[1]);
    let bytes = fs::read(&cut).unwrap();
    fs::write(&cut, &bytes[..bytes.len() / 2]).unwrap();
    let report = write_all(
        &config,
        &analysis,
        WriteOptions::default(),
        &ignore,
        &never(),
    )
    .unwrap();
    assert!(
        matches!(report.outcomes[0].1, Ok(WriteOutcome::Skipped { .. })),
        "{:?}",
        report.outcomes
    );
    let cut_id = id_of(&analysis, dir.path(), names[1]);
    assert!(
        matches!(
            report.warnings.as_slice(),
            [(id, Warning::LayerUnreadable { .. })] if *id == cut_id
        ),
        "{:?}",
        report.warnings
    );
    assert!(entries(&config.output_dir).is_empty());
}

#[test]
fn layer_whose_file_changed_since_the_probe_is_excluded() {
    // A decode that disagrees with its probe on size, depth,
    // alpha or colour profile is left out, so it can neither fail the
    // encoder nor carry a profile the output does not embed.
    let dir = tempfile::tempdir().unwrap();
    let paths: Vec<PathBuf> = ["a.jpg", "b.jpg", "c.jpg"]
        .iter()
        .map(|name| dir.path().join(name))
        .collect();
    for (seed, path) in (1..).zip(&paths) {
        plain_jpeg(path, seed, WIDTH, HEIGHT, None);
    }
    let small = dir.path().join("small.jpg");
    plain_jpeg(&small, 4, WIDTH / 2, HEIGHT / 2, None);

    let widen = |image: FullImage| {
        let Samples::U8(v) = &image.samples else {
            unreachable!()
        };
        FullImage {
            samples: Samples::U16(v.iter().map(|&s| u16::from(s) * 257).collect()),
            ..image
        }
    };
    let add_alpha = |image: FullImage| {
        let Samples::U8(v) = &image.samples else {
            unreachable!()
        };
        let rgba = v.chunks(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect();
        FullImage {
            channels: 4,
            samples: Samples::U8(rgba),
            ..image
        }
    };
    let with_icc = |image: FullImage| FullImage {
        icc: Some(rgb_profile(RawTone::Linear)),
        ..image
    };
    type Change = Box<dyn Fn(FullImage) -> FullImage + Send + Sync>;
    let small_image = full(&small);
    let cases: [(Change, &str); 4] = [
        (
            Box::new(move |_| small_image.clone()),
            "it is now 320×213, the canvas 640×426",
        ),
        (Box::new(widen), "it is now 16-bit"),
        (Box::new(add_alpha), "it now has alpha"),
        (Box::new(with_icc), "its colour profile is different"),
    ];
    for (change, what) in cases {
        let out = tempfile::tempdir().unwrap();
        let b = full(&paths[1]);
        let inputs = vec![
            file_input(&paths[0]),
            input_loading(&paths[1], move || Ok(change(b))),
            file_input(&paths[2]),
        ];
        let ticks = Mutex::new(Vec::new());
        let (outcome, warnings) = write_stack(
            out.path(),
            OutputFormat::Psd,
            WriteOptions::default(),
            inputs,
            &|i| ticks.lock().unwrap().push(i),
            &never(),
        )
        .unwrap();
        let (path, layers, _) = written(&outcome);
        assert_eq!((file_name(path), layers), ("a_2.psd", 2), "{what}");
        let reason = format!("the file changed since it was probed: {what}");
        assert_eq!(warnings, [(1, Warning::LayerUnreadable { reason })]);
        assert_eq!(layer_names(&read_psd(path).layers), ["a", "c"]);
        assert_eq!(*ticks.lock().unwrap(), [0, 1, 2]);
        assert_eq!(entries(out.path()), ["a_2.psd"]);
    }
}

/// Three EXIF-less 640×426 JPEGs `a`, `b` and `c` in `dir`.
fn three_jpegs(dir: &Path) -> Vec<PathBuf> {
    let paths: Vec<PathBuf> = ["a.jpg", "b.jpg", "c.jpg"]
        .iter()
        .map(|name| dir.join(name))
        .collect();
    for (seed, path) in (1..).zip(&paths) {
        plain_jpeg(path, seed, WIDTH, HEIGHT, None);
    }
    paths
}

#[test]
fn failed_probe_is_excluded_and_ticked() {
    let dir = tempfile::tempdir().unwrap();
    let paths = three_jpegs(dir.path());
    let unprobed = StackInput {
        stem: "x".into(),
        exif: None,
        probe: Err(DecodeError::Corrupt("no header".into())),
        load: Box::new(|| panic!("an input that failed its probe is never loaded")),
    };
    let inputs = vec![
        file_input(&paths[0]),
        unprobed,
        file_input(&paths[1]),
        file_input(&paths[2]),
    ];
    let out = tempfile::tempdir().unwrap();
    let ticks = Mutex::new(Vec::new());
    let (outcome, warnings) = write_stack(
        out.path(),
        OutputFormat::Psd,
        WriteOptions::default(),
        inputs,
        &|i| ticks.lock().unwrap().push(i),
        &never(),
    )
    .unwrap();
    let (path, layers, _) = written(&outcome);
    assert_eq!((file_name(path), layers), ("a_3.psd", 3));
    let reason = "corrupt image: no header".to_owned();
    assert_eq!(warnings, [(1, Warning::LayerUnreadable { reason })]);
    // The failed probe ticks first, then the layers in order.
    assert_eq!(*ticks.lock().unwrap(), [1, 0, 2, 3]);
    assert_eq!(layer_names(&read_psd(path).layers), ["a", "b", "c"]);
}

#[test]
fn layer_the_encoder_refuses_is_excluded() {
    // An image that matches its probe yet does not fit the
    // encoder (a wrong sample count, a channel count other than 3 or 4) is
    // refused before anything is written, so only that layer is left out.
    let short = |image: FullImage| {
        let Samples::U8(mut v) = image.samples else {
            unreachable!()
        };
        v.truncate(v.len() - 3);
        FullImage {
            samples: Samples::U8(v),
            ..image
        }
    };
    let five_channels = |image: FullImage| {
        let Samples::U8(v) = &image.samples else {
            unreachable!()
        };
        let samples = v.chunks(3).flat_map(|p| [p[0], p[1], p[2], 0, 0]).collect();
        FullImage {
            channels: 5,
            samples: Samples::U8(samples),
            ..image
        }
    };
    type Change = fn(FullImage) -> FullImage;
    let cases: [(Change, String); 2] = [
        (
            short,
            format!(
                "the layer has {} samples, not {WIDTH}x{HEIGHT}x3",
                WIDTH * HEIGHT * 3 - 3
            ),
        ),
        (five_channels, "the layer has 5 channels, not 3 or 4".into()),
    ];
    for (change, reason) in cases {
        let dir = tempfile::tempdir().unwrap();
        let paths = three_jpegs(dir.path());
        let b = full(&paths[1]);
        let inputs = vec![
            file_input(&paths[0]),
            input_loading(&paths[1], move || Ok(change(b))),
            file_input(&paths[2]),
        ];
        let out = tempfile::tempdir().unwrap();
        let (outcome, warnings) = write_stack(
            out.path(),
            OutputFormat::Psd,
            WriteOptions::default(),
            inputs,
            &|_| {},
            &never(),
        )
        .unwrap();
        let (path, layers, _) = written(&outcome);
        assert_eq!((file_name(path), layers), ("a_2.psd", 2), "{reason}");
        assert_eq!(warnings, [(1, Warning::LayerUnreadable { reason })]);
        assert_eq!(layer_names(&read_psd(path).layers), ["a", "c"]);
        assert_eq!(entries(out.path()), ["a_2.psd"]);
    }
}

#[test]
fn stem_that_is_not_a_file_name_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let paths = three_jpegs(dir.path());
    // A backslash and a colon are refused only on Windows: there they make a
    // separator, a drive prefix or an NTFS stream.
    let mut refused = vec!["", ".", "..", "a/b", "a/", "../a", "/a"];
    if cfg!(windows) {
        refused.extend([r"a\b", r"..\a", "C:", "C:a", "a:b"]);
    }
    for stem in refused {
        let out = tempfile::tempdir().unwrap();
        let mut inputs: Vec<StackInput> = paths
            .iter()
            .map(|path| input_loading(path, || panic!("nothing is loaded")))
            .collect();
        inputs[1].stem = stem.into();
        let ticks = Mutex::new(Vec::new());
        let result = write_stack(
            out.path(),
            OutputFormat::Psd,
            WriteOptions::default(),
            inputs,
            &|i| ticks.lock().unwrap().push(i),
            &never(),
        );
        match result {
            Err(WriteError::Io(error)) => {
                assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{stem:?}");
            }
            other => panic!("{stem:?}: {other:?}"),
        }
        assert!(ticks.lock().unwrap().is_empty(), "{stem:?}");
        assert!(entries(out.path()).is_empty(), "{stem:?}");
    }
}

/// On Unix a backslash or a colon is an ordinary character of a file name,
/// so such a stem names the output.
#[cfg(unix)]
#[test]
fn stem_with_backslash_or_colon_is_written_on_unix() {
    let dir = tempfile::tempdir().unwrap();
    let paths = three_jpegs(dir.path());
    for stem in [r"a\b", r"..\a", "a:b"] {
        let out = tempfile::tempdir().unwrap();
        let mut inputs: Vec<StackInput> = paths.iter().map(|path| file_input(path)).collect();
        inputs[0].stem = stem.into();
        let (outcome, warnings) = write_stack(
            out.path(),
            OutputFormat::Psd,
            WriteOptions::default(),
            inputs,
            &|_| {},
            &never(),
        )
        .unwrap();
        let (path, layers, _) = written(&outcome);
        assert_eq!(path.parent(), Some(out.path()), "{stem:?}");
        assert_eq!(file_name(path), format!("{stem}_3.psd"));
        assert_eq!(layers, 3);
        assert!(warnings.is_empty(), "{stem:?}");
    }
}

/// A group fails (its output name is too long for the file system), the
/// next is written; progress still reaches its total.
#[cfg(unix)]
#[test]
fn failed_group_does_not_stop_the_next() {
    let dir = tempfile::tempdir().unwrap();
    // 254 bytes, within the 255-byte name limit; `<stem>_2.psd.partial` is not.
    let long = format!("{}.jpg", "x".repeat(250));
    let paths: Vec<PathBuf> = [long.as_str(), "b.jpg", "c.jpg", "d.jpg"]
        .iter()
        .map(|name| dir.path().join(name))
        .collect();
    for (seed, path) in (1..).zip(&paths) {
        plain_jpeg(path, seed, WIDTH, HEIGHT, None);
    }
    let config = JobConfig::new(dir.path().to_path_buf());
    let analysis = analysis_of(&paths, &[&[0, 1], &[2, 3]]);
    let reports = Mutex::new(Vec::new());
    let progress = |p: Progress| reports.lock().unwrap().push(p);

    let report = write_all(
        &config,
        &analysis,
        WriteOptions::default(),
        &progress,
        &never(),
    )
    .unwrap();
    assert_eq!(report.outcomes.len(), 2);
    let (group, failed) = &report.outcomes[0];
    assert_eq!(*group, 0);
    assert!(failed.is_err(), "{failed:?}");
    let (group, ok) = &report.outcomes[1];
    assert_eq!(*group, 1);
    assert_eq!(file_name(written(ok.as_ref().unwrap()).0), "c_2.psd");
    assert_eq!(entries(&config.output_dir), ["c_2.psd"]);
    // The failed group's members were never reached; write_all ticks them.
    let expected: Vec<Progress> = paths
        .iter()
        .enumerate()
        .map(|(i, path)| Progress {
            stage: Stage::Write,
            done: i + 1,
            total: 4,
            current: Some(path.clone()),
        })
        .collect();
    assert_eq!(reports.into_inner().unwrap(), expected);
}

/// A stack that fails after some of its members ticked has
/// only the others ticked by `write_all`, so every member ticks exactly once
/// and progress still reaches its total.
#[cfg(unix)]
#[test]
fn stack_failing_mid_way_ticks_each_member_once() {
    let dir = tempfile::tempdir().unwrap();
    // 254 bytes, within the 255-byte name limit; `<stem>_2.psd` is not.
    let long_x = format!("{}.jpg", "x".repeat(250));
    let long_y = format!("{}.jpg", "y".repeat(250));
    let names = [
        "gone.jpg",
        long_x.as_str(),
        "b.jpg",
        "cut.jpg",
        long_y.as_str(),
        "c.jpg",
        "d.jpg",
        "e.jpg",
    ];
    let paths: Vec<PathBuf> = names.iter().map(|name| dir.path().join(name)).collect();
    for (seed, path) in (1..).zip(&paths) {
        plain_jpeg(path, seed, WIDTH, HEIGHT, None);
    }
    let config = JobConfig::new(dir.path().to_path_buf());
    let analysis = analysis_of(&paths, &[&[0, 1, 2], &[3, 4, 5], &[6, 7]]);
    // Group 0: `gone` fails its probe and ticks, then the partial of the
    // long stem cannot be created. Group 1: `cut` probes but fails its
    // decode, the other two are encoded, then the final name, after the
    // long stem, cannot be made. Both fail after ticking some members.
    fs::remove_file(&paths[0]).unwrap();
    let bytes = fs::read(&paths[3]).unwrap();
    fs::write(&paths[3], &bytes[..bytes.len() / 2]).unwrap();
    let reports = Mutex::new(Vec::new());
    let progress = |p: Progress| reports.lock().unwrap().push(p);

    let report = write_all(
        &config,
        &analysis,
        WriteOptions::default(),
        &progress,
        &never(),
    )
    .unwrap();
    let groups: Vec<usize> = report.outcomes.iter().map(|(group, _)| *group).collect();
    assert_eq!(groups, [0, 1, 2]);
    for (_, failed) in &report.outcomes[..2] {
        assert!(failed.is_err(), "{failed:?}");
    }
    let (path, ..) = written(report.outcomes[2].1.as_ref().unwrap());
    assert_eq!(file_name(path), "d_2.psd");
    // A failed group reports no warnings, and leaves nothing behind.
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(entries(&config.output_dir), ["d_2.psd"]);
    // In member order, each once: what failed ticked as it was reached,
    // and `write_all` ticked the rest after the failure.
    let expected: Vec<Progress> = paths
        .iter()
        .enumerate()
        .map(|(i, path)| Progress {
            stage: Stage::Write,
            done: i + 1,
            total: paths.len(),
            current: Some(path.clone()),
        })
        .collect();
    assert_eq!(reports.into_inner().unwrap(), expected);
}

#[test]
fn mixed_8_and_16_bit_members_write_16_bit() {
    let dir = tempfile::tempdir().unwrap();
    let deep = dir.path().join("deep.png");
    let shallow = dir.path().join("shallow.png");
    let base = scene(7, WIDTH, HEIGHT);
    let deep_image: ImageBuffer<Rgb<u16>, Vec<u16>> =
        ImageBuffer::from_fn(WIDTH, HEIGHT, |x, y| {
            let p = base.get_pixel(x, y).0;
            // Values an 8-bit image cannot hold.
            Rgb(p.map(|v| u16::from(v) * 256 + 3))
        });
    deep_image
        .save_with_format(&deep, ImageFormat::Png)
        .unwrap();
    let shallow_image: RgbImage = expose(&base, -1.0);
    shallow_image
        .save_with_format(&shallow, ImageFormat::Png)
        .unwrap();
    let mut config = JobConfig::new(dir.path().to_path_buf());
    config.output_format = OutputFormat::Psd;
    let analysis = analysis_of(&[deep.clone(), shallow.clone()], &[&[0, 1]]);

    let (outcome, warnings) = write_group(
        &config,
        &analysis,
        0,
        WriteOptions::default(),
        &ignore,
        &never(),
    )
    .unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    let (path, ..) = written(&outcome);
    assert_eq!(file_name(path), "deep_2.psd");
    let psd = read_psd(path);
    assert_eq!(psd.depth, 16);
    let widened: Vec<Vec<u16>> = planes(&full(&shallow))
        .into_iter()
        .map(|plane| plane.into_iter().map(|v| v * 257).collect())
        .collect();
    assert_eq!(layer_planes(&psd.layers[0]), widened, "v * 257");
    assert_eq!(layer_planes(&psd.layers[1]), planes(&full(&deep)));
    assert_eq!(psd.merged, planes(&full(&deep)));
}

#[test]
fn resolution_mismatch_member_excluded_with_warning() {
    let dir = tempfile::tempdir().unwrap();
    let paths: Vec<PathBuf> = ["a.jpg", "b.jpg", "c.jpg"]
        .iter()
        .map(|name| dir.path().join(name))
        .collect();
    plain_jpeg(&paths[0], 1, WIDTH, HEIGHT, None);
    plain_jpeg(&paths[1], 2, WIDTH / 2, HEIGHT / 2, None);
    plain_jpeg(&paths[2], 3, WIDTH, HEIGHT, None);
    let config = JobConfig::new(dir.path().to_path_buf());
    let analysis = analysis_of(&paths, &[&[0, 1, 2]]);

    let (outcome, warnings) = write_group(
        &config,
        &analysis,
        0,
        WriteOptions::default(),
        &ignore,
        &never(),
    )
    .unwrap();
    let (path, layers, _) = written(&outcome);
    assert_eq!((file_name(path), layers), ("a_2.psd", 2));
    let mismatch = Warning::ResolutionMismatch {
        width: WIDTH / 2,
        height: HEIGHT / 2,
        canvas_width: WIDTH,
        canvas_height: HEIGHT,
    };
    assert_eq!(warnings, [(1, mismatch)]);
    let psd = read_psd(path);
    assert_eq!((psd.width, psd.height), (WIDTH, HEIGHT));
    assert_eq!(layer_names(&psd.layers), ["a", "c"]);
}

#[test]
fn icc_mismatch_warns_on_differing_kept_member() {
    let dir = tempfile::tempdir().unwrap();
    let srgb = rgb_profile(RawTone::Srgb);
    let linear = rgb_profile(RawTone::Linear);
    let paths: Vec<PathBuf> = ["a.jpg", "b.jpg", "c.jpg"]
        .iter()
        .map(|name| dir.path().join(name))
        .collect();
    plain_jpeg(&paths[0], 1, WIDTH, HEIGHT, Some(&srgb));
    plain_jpeg(&paths[1], 2, WIDTH, HEIGHT, Some(&linear));
    plain_jpeg(&paths[2], 3, WIDTH, HEIGHT, Some(&srgb));
    let config = JobConfig::new(dir.path().to_path_buf());
    let analysis = analysis_of(&paths, &[&[0, 1, 2]]);

    let (outcome, warnings) = write_group(
        &config,
        &analysis,
        0,
        WriteOptions::default(),
        &ignore,
        &never(),
    )
    .unwrap();
    let (path, layers, _) = written(&outcome);
    assert_eq!(layers, 3);
    assert_eq!(warnings, [(1, Warning::IccMismatch)]);
    assert_eq!(icc_of(&read_psd(path)), Some(srgb.as_slice()));
}

#[test]
fn icc_reference_is_the_first_layer_written() {
    // The reference member fails at write time, so the next
    // layer's profile is embedded, and the warnings compare with it.
    let dir = tempfile::tempdir().unwrap();
    let srgb = rgb_profile(RawTone::Srgb);
    let linear = rgb_profile(RawTone::Linear);
    let paths: Vec<PathBuf> = ["a.jpg", "b.jpg", "c.jpg", "d.jpg"]
        .iter()
        .map(|name| dir.path().join(name))
        .collect();
    plain_jpeg(&paths[0], 1, WIDTH, HEIGHT, Some(&srgb));
    plain_jpeg(&paths[1], 2, WIDTH, HEIGHT, Some(&linear));
    plain_jpeg(&paths[2], 3, WIDTH, HEIGHT, Some(&linear));
    plain_jpeg(&paths[3], 4, WIDTH, HEIGHT, Some(&srgb));
    let inputs = vec![
        input_loading(&paths[0], || {
            Err(DecodeError::Corrupt("cut off since the probe".into()))
        }),
        file_input(&paths[1]),
        file_input(&paths[2]),
        file_input(&paths[3]),
    ];
    let out = tempfile::tempdir().unwrap();
    let (outcome, warnings) = write_stack(
        out.path(),
        OutputFormat::Psd,
        WriteOptions::default(),
        inputs,
        &|_| {},
        &never(),
    )
    .unwrap();
    let (path, layers, _) = written(&outcome);
    assert_eq!((file_name(path), layers), ("b_3.psd", 3));
    let unreadable = Warning::LayerUnreadable {
        reason: "corrupt image: cut off since the probe".into(),
    };
    // b and c match the new reference; d, like a, differs from it.
    assert_eq!(warnings, [(0, unreadable), (3, Warning::IccMismatch)]);
    assert_eq!(icc_of(&read_psd(path)), Some(linear.as_slice()));
}

/// Analyses `dir`, writes its one group, and returns the stack's embedded
/// profile; the write must have no warning.
fn icc_of_only_stack(dir: &Path) -> Option<Vec<u8>> {
    let config = JobConfig::new(dir.to_path_buf());
    let analysis = analyze_ok(&config);
    assert_eq!(analysis.groups.len(), 1, "{:?}", analysis.groups);
    let (outcome, warnings) = write_group(
        &config,
        &analysis,
        0,
        WriteOptions::default(),
        &ignore,
        &never(),
    )
    .unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    let (path, layers, _) = written(&outcome);
    assert_eq!(layers, 3);
    icc_of(&read_psd(path)).map(<[u8]>::to_vec)
}

#[test]
fn untagged_adobe_rgb_dcf_jpeg_embeds_adobe_rgb() {
    // DCF marks Adobe RGB as ColorSpace 0xFFFF (uncalibrated) with the
    // Interop index R03; some cameras write ColorSpace 2 instead. Both get
    // the same generated profile, so no layer warns of a mismatch.
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    untagged_shot(&d.join("IMG_0001.jpg"), 1, -1, 0xFFFF, Some("R03"));
    untagged_shot(&d.join("IMG_0002.jpg"), 2, 0, 2, None);
    untagged_shot(&d.join("IMG_0003.jpg"), 3, 1, 0xFFFF, Some("R03"));
    let icc = icc_of_only_stack(d).expect("an embedded profile");
    assert_eq!(icc, adobe_rgb_profile());
    assert!(String::from_utf8_lossy(&icc).contains("Photostacker Adobe RGB (1998)"));
}

/// Writes AEB shot `shot` (1-based) of scene 1 to `path` at `ev`, with EXIF
/// ColorSpace 2 (Adobe RGB, as some DCF cameras mark it) and `icc` embedded
/// after the EXIF, as cameras order them.
fn hinted_shot_with_icc(path: &Path, shot: u16, ev: i16, icc: &[u8]) {
    let mut exif = aeb_exif(u32::from(shot) - 1, shot, ev);
    exif.exif.push((0xA001, Value::Short(vec![2])));
    let image = expose(&scene(1, WIDTH, HEIGHT), f32::from(ev));
    let with_exif = jpeg_with_exif(&image, &exif.build(), JPEG_QUALITY);
    let with_icc = jpeg_with_icc(&image, icc, JPEG_QUALITY);
    // Each puts its segment right after SOI: FF xx, the length (counting
    // itself), the payload.
    let segment_end = |jpeg: &[u8]| 4 + usize::from(u16::from_be_bytes([jpeg[4], jpeg[5]]));
    let app1_end = segment_end(&with_exif);
    let mut bytes = with_exif[..app1_end].to_vec();
    bytes.extend_from_slice(&with_icc[2..segment_end(&with_icc)]);
    bytes.extend_from_slice(&with_exif[app1_end..]);
    fs::write(path, bytes).unwrap();
}

#[test]
fn own_rgb_profile_beats_the_adobe_rgb_dcf_hint() {
    // Every shot's EXIF marks it Adobe RGB, but each embeds an sRGB profile
    // of its own, which it keeps.
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let srgb = rgb_profile(RawTone::Srgb);
    for (shot, ev) in [(1, -1), (2, 0), (3, 1)] {
        hinted_shot_with_icc(&d.join(format!("IMG_000{shot}.jpg")), shot, ev, &srgb);
    }
    assert_eq!(icc_of_only_stack(d), Some(srgb));
}

#[test]
fn adobe_rgb_dcf_jpeg_beside_an_untagged_srgb_one_is_a_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    untagged_shot(&d.join("IMG_0001.jpg"), 1, -1, 0xFFFF, Some("R03"));
    untagged_shot(&d.join("IMG_0002.jpg"), 2, 0, 2, None);
    untagged_shot(&d.join("IMG_0003.jpg"), 3, 1, 1, Some("R98"));
    let config = JobConfig::new(d.to_path_buf());
    let analysis = analyze_ok(&config);
    assert_eq!(analysis.groups.len(), 1, "{:?}", analysis.groups);

    let (outcome, warnings) = write_group(
        &config,
        &analysis,
        0,
        WriteOptions::default(),
        &ignore,
        &never(),
    )
    .unwrap();
    let (path, layers, _) = written(&outcome);
    assert_eq!(layers, 3);
    // The top layer's Adobe RGB is the stack's; the sRGB shot has none.
    let srgb_shot = id_of(&analysis, d, "IMG_0003.jpg");
    assert_eq!(warnings, [(srgb_shot, Warning::IccMismatch)]);
    assert_eq!(
        icc_of(&read_psd(path)),
        Some(adobe_rgb_profile().as_slice())
    );
}

#[test]
fn untagged_srgb_jpeg_stays_untagged() {
    // ColorSpace 1 is sRGB, which an untagged file already means.
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    for (shot, ev) in [(1, -1), (2, 0), (3, 1)] {
        let path = d.join(format!("IMG_000{shot}.jpg"));
        untagged_shot(&path, shot, ev, 1, Some("R98"));
    }
    assert_eq!(icc_of_only_stack(d), None);
}

#[test]
fn tiff_format_writes_layered_tiff() {
    use tiff::decoder::{Decoder, DecodingResult, Limits};
    use tiff::tags::Tag;

    let dir = tempfile::tempdir().unwrap();
    let names = bracket_of_five(dir.path());
    let mut config = JobConfig::new(dir.path().to_path_buf());
    config.output_format = OutputFormat::Tiff;
    let analysis = analyze_ok(&config);
    let report = write_all(
        &config,
        &analysis,
        WriteOptions::default(),
        &ignore,
        &never(),
    )
    .unwrap();
    let (path, layers, container) = written(report.outcomes[0].1.as_ref().unwrap());
    assert_eq!(file_name(path), "IMG_0009_5.tif");
    assert_eq!((layers, container), (5, Container::Tiff));
    assert_eq!(entries(&config.output_dir), ["IMG_0009_5.tif"]);

    let bytes = fs::read(path).unwrap();
    let mut decoder = Decoder::new(std::io::Cursor::new(&bytes))
        .unwrap()
        .with_limits(Limits::unlimited());
    assert_eq!(decoder.dimensions().unwrap(), (WIDTH, HEIGHT));
    let DecodingResult::U8(composite) = decoder.read_image().unwrap() else {
        panic!("not 8-bit");
    };
    let Samples::U8(top) = full(&dir.path().join(names[0])).samples else {
        panic!("not 8-bit");
    };
    assert_eq!(composite, top, "the composite is shot 1");
    let source = decoder.get_tag_u8_vec(Tag::Unknown(37724)).unwrap();
    let layers = psd_reader::read_layer_block(&source).unwrap();
    assert_eq!(layer_names(&layers), bracket_of_five_layer_names());
}

/// The interleaved samples of `image` (8-bit ones as `v as u16`).
fn samples_u16(image: &FullImage) -> Vec<u16> {
    match &image.samples {
        Samples::U8(v) => v.iter().map(|&s| u16::from(s)).collect(),
        Samples::U16(v) => v.clone(),
    }
}

/// The merged image a PSD stores for an RGBA `image` (spec §6.2): each
/// colour plane blended against white with its pixel's alpha, rounded to
/// nearest, then the alpha plane unchanged.
fn on_white(image: &FullImage) -> Vec<Vec<u16>> {
    let max: u64 = match image.samples {
        Samples::U8(_) => 255,
        Samples::U16(_) => 65535,
    };
    let alpha: Vec<u16> = samples_u16(image).into_iter().skip(3).step_by(4).collect();
    let mut merged = planes(image);
    for plane in &mut merged {
        for (c, &a) in plane.iter_mut().zip(&alpha) {
            let (c64, a64) = (u64::from(*c), u64::from(a));
            *c = ((c64 * a64 + max * (max - a64) + max / 2) / max) as u16;
        }
    }
    merged.push(alpha);
    merged
}

/// Spec §6.2 and §6.3: on a canvas with
/// alpha a PSD's merged image is the top layer blended against white, as
/// Photoshop stores it and as readers of the merged image undo it, with the
/// top layer's alpha; the layers keep their straight colour. A layered
/// TIFF's own image is unassociated alpha (ExtraSamples 2), so it keeps the
/// top layer's straight colour.
#[test]
fn merged_image_with_alpha_is_on_white_in_a_psd_and_straight_in_a_tiff() {
    use tiff::decoder::{Decoder, DecodingResult, Limits};

    let (width, height) = (64, 48);
    let base = scene(5, width, height);
    // 8 bits: every alpha from 0 to 255, as 7 is prime to 256. 16 bits:
    // 0, 65535 and 1,536 other values (7,919 is odd, so they differ).
    let alpha8 = |x: u32, y: u32| ((y * width + x) * 7 % 256) as u8;
    let alpha16 = |x: u32, y: u32| match (y * width + x) % 4 {
        0 => 0,
        1 => u16::MAX,
        _ => ((y * width + x) * 7919 % 65536) as u16,
    };
    for sixteen_bit in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (top_path, below_path) = (dir.path().join("top.png"), dir.path().join("below.png"));
        // The layer below has the opposite alpha.
        for (path, ev, below) in [(&top_path, 0.0, false), (&below_path, -1.0, true)] {
            let shot = expose(&base, ev);
            if sixteen_bit {
                let image: ImageBuffer<Rgba<u16>, Vec<u16>> =
                    ImageBuffer::from_fn(width, height, |x, y| {
                        // Values an 8-bit image cannot hold.
                        let [r, g, b] = shot.get_pixel(x, y).0.map(|v| u16::from(v) * 256 + 3);
                        let a = alpha16(x, y);
                        Rgba([r, g, b, if below { u16::MAX - a } else { a }])
                    });
                image.save_with_format(path, ImageFormat::Png).unwrap();
            } else {
                let image = RgbaImage::from_fn(width, height, |x, y| {
                    let [r, g, b] = shot.get_pixel(x, y).0;
                    let a = alpha8(x, y);
                    Rgba([r, g, b, if below { u8::MAX - a } else { a }])
                });
                image.save_with_format(path, ImageFormat::Png).unwrap();
            }
        }
        let top = full(&top_path);
        assert_eq!(top.channels, 4);
        let analysis = analysis_of(&[top_path.clone(), below_path], &[&[0, 1]]);
        let mut config = JobConfig::new(dir.path().to_path_buf());

        config.output_format = OutputFormat::Psd;
        let (outcome, warnings) = write_group(
            &config,
            &analysis,
            0,
            WriteOptions::default(),
            &ignore,
            &never(),
        )
        .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let (path, _, container) = written(&outcome);
        assert_eq!(container, Container::Psd);
        let psd = read_psd(path);
        assert_eq!(
            (psd.channels, psd.layer_count),
            (4, -2),
            "16-bit {sixteen_bit}"
        );
        assert_eq!(psd.depth, if sixteen_bit { 16 } else { 8 });
        assert_eq!(psd.merged, on_white(&top), "16-bit {sixteen_bit}");
        // The top layer itself keeps its straight colour and its alpha.
        let top_layer = &psd.layers[1];
        assert_eq!(layer_planes(top_layer), planes(&top));
        let (_, layer_alpha) = top_layer.channels.iter().find(|(id, _)| *id == -1).unwrap();
        assert_eq!(Some(layer_alpha), on_white(&top).last());

        config.output_format = OutputFormat::Tiff;
        let (outcome, warnings) = write_group(
            &config,
            &analysis,
            0,
            WriteOptions::default(),
            &ignore,
            &never(),
        )
        .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let (path, _, container) = written(&outcome);
        assert_eq!(container, Container::Tiff);
        let bytes = fs::read(path).unwrap();
        let mut decoder = Decoder::new(std::io::Cursor::new(&bytes))
            .unwrap()
            .with_limits(Limits::unlimited());
        let image = match decoder.read_image().unwrap() {
            DecodingResult::U8(v) if !sixteen_bit => v.into_iter().map(u16::from).collect(),
            DecodingResult::U16(v) if sixteen_bit => v,
            _ => panic!("not {}-bit", if sixteen_bit { 16 } else { 8 }),
        };
        assert!(
            image == samples_u16(&top),
            "16-bit {sixteen_bit}: the TIFF's image is the top layer, straight"
        );
    }
}

#[test]
fn write_all_reports_progress_per_layer() {
    // Two groups; the first leaves a member out. Every input ticks once, in
    // order, counted across both groups.
    let dir = tempfile::tempdir().unwrap();
    let paths: Vec<PathBuf> = ["a.jpg", "small.jpg", "b.jpg", "c.jpg", "d.jpg"]
        .iter()
        .map(|name| dir.path().join(name))
        .collect();
    for (seed, path) in (1..).zip(&paths) {
        let (width, height) = if seed == 2 {
            (WIDTH / 2, HEIGHT / 2)
        } else {
            (WIDTH, HEIGHT)
        };
        plain_jpeg(path, seed, width, height, None);
    }
    let config = JobConfig::new(dir.path().to_path_buf());
    let analysis = analysis_of(&paths, &[&[0, 1, 2], &[3, 4]]);
    let reports = Mutex::new(Vec::new());
    let progress = |p: Progress| reports.lock().unwrap().push(p);

    let report = write_all(
        &config,
        &analysis,
        WriteOptions::default(),
        &progress,
        &never(),
    )
    .unwrap();
    let names: Vec<_> = report
        .outcomes
        .iter()
        .map(|(group, outcome)| {
            (
                *group,
                file_name(written(outcome.as_ref().unwrap()).0).to_owned(),
            )
        })
        .collect();
    assert_eq!(names, [(0, "a_2.psd".into()), (1, "c_2.psd".into())]);
    assert!(matches!(
        report.warnings.as_slice(),
        [(1, Warning::ResolutionMismatch { .. })]
    ));
    let reports = reports.into_inner().unwrap();
    // The member left out by the canvas ticks first, then the layers.
    let order = [1, 0, 2, 3, 4];
    let expected: Vec<Progress> = order
        .iter()
        .enumerate()
        .map(|(i, &id)| Progress {
            stage: Stage::Write,
            done: i + 1,
            total: 5,
            current: Some(paths[id].clone()),
        })
        .collect();
    assert_eq!(reports, expected);

    // write_group counts one group alone.
    let reports = Mutex::new(Vec::new());
    let progress = |p: Progress| reports.lock().unwrap().push(p);
    write_group(
        &config,
        &analysis,
        1,
        WriteOptions::default(),
        &progress,
        &never(),
    )
    .unwrap();
    let done: Vec<_> = reports
        .into_inner()
        .unwrap()
        .iter()
        .map(|p| (p.stage, p.done, p.total))
        .collect();
    assert_eq!(done, [(Stage::Write, 1, 2), (Stage::Write, 2, 2)]);
}

#[test]
fn raw_tone_linear_embeds_linear_profile() {
    let Some(root) = std::env::var_os("PHOTOSTACKER_FIXTURES") else {
        println!("SKIPPED: PHOTOSTACKER_FIXTURES not set");
        return;
    };
    let folder = Path::new(&root).join("brackets/Canon/Empa_FORTH1_EOS450D_AEB3");
    let paths: Vec<PathBuf> = ["IMG_7898.CR2", "IMG_7899.CR2"]
        .iter()
        .map(|name| folder.join(name))
        .collect();
    let out = tempfile::tempdir().unwrap();
    let mut config = JobConfig::new(folder.clone());
    config.output_dir = out.path().join("stacked");
    config.raw_tone = RawTone::Linear;
    let analysis = analysis_of(&paths, &[&[0, 1]]);

    let (outcome, warnings) = write_group(
        &config,
        &analysis,
        0,
        WriteOptions::default(),
        &ignore,
        &never(),
    )
    .unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    let (path, layers, container) = written(&outcome);
    assert_eq!(
        (file_name(path), layers, container),
        ("IMG_7898_2.psd", 2, Container::Psd)
    );
    let psd = read_psd(path);
    assert_eq!(psd.depth, 16);
    let icc = icc_of(&psd).expect("an embedded profile");
    assert_eq!(icc, rgb_profile(RawTone::Linear).as_slice());
    let text = String::from_utf8_lossy(icc);
    assert!(
        text.contains("Photostacker linear sRGB"),
        "the desc says linear"
    );
}

/// The bound is not loosened; a failure reports the peak.
#[cfg(target_os = "linux")]
#[test]
fn write_memory_stays_near_one_layer() {
    use std::io::BufWriter;
    use tiff::encoder::{Compression, TiffEncoder, colortype};

    const W: u32 = 6000;
    const H: u32 = 4000;
    const LAYERS: u64 = 6;
    const LIMIT_MIB: u64 = 700;

    /// Writes a 16-bit RGB TIFF strip by strip, so no whole image is ever
    /// held: a gradient with noise in its low bits, different per `seed`.
    /// PackBits, because the tiff crate's Deflate and LZW strips do not
    /// decode back (tiff 0.11.3).
    fn write_tiff16(path: &Path, seed: u64) {
        let file = BufWriter::new(fs::File::create(path).unwrap());
        let mut encoder = TiffEncoder::new(file)
            .unwrap()
            .with_compression(Compression::Packbits);
        let mut image = encoder.new_image::<colortype::RGB16>(W, H).unwrap();
        image.rows_per_strip(16).unwrap();
        let mut index = 0u64;
        loop {
            let count = image.next_strip_sample_count();
            if count == 0 {
                break;
            }
            let strip: Vec<u16> = (index..index + count)
                .map(|i| {
                    let (pixel, channel) = (i / 3, i % 3);
                    let (x, y) = (pixel % u64::from(W), pixel / u64::from(W));
                    let gradient = x * 8 + y * 4 + channel * 5000 + seed * 3000;
                    let noise = (i ^ seed).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58;
                    (gradient + noise) as u16
                })
                .collect();
            image.write_strip(&strip).unwrap();
            index += count;
        }
        image.finish().unwrap();
    }

    fn vm_hwm_kib() -> u64 {
        let status = fs::read_to_string("/proc/self/status").unwrap();
        let line = status.lines().find(|l| l.starts_with("VmHWM:")).unwrap();
        line.split_whitespace().nth(1).unwrap().parse().unwrap()
    }

    let dir = tempfile::tempdir().unwrap();
    let inputs: Vec<StackInput> = (0..LAYERS)
        .map(|seed| {
            let path = dir.path().join(format!("L{seed}.tif"));
            write_tiff16(&path, seed);
            file_input(&path)
        })
        .collect();
    let before = vm_hwm_kib();
    let out = dir.path().join("out");
    fs::create_dir(&out).unwrap();
    let (outcome, warnings) = write_stack(
        &out,
        OutputFormat::Psd,
        WriteOptions::default(),
        inputs,
        &|_| {},
        &never(),
    )
    .unwrap();
    let peak = vm_hwm_kib();
    println!(
        "VmHWM: {} MiB before write_stack, {} MiB after (limit {LIMIT_MIB} MiB)",
        before / 1024,
        peak / 1024
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    let (path, layers, container) = written(&outcome);
    assert_eq!(
        (file_name(path), layers, container),
        ("L0_6.psd", 6, Container::Psd)
    );
    assert!(
        peak < LIMIT_MIB * 1024,
        "VmHWM {} MiB is over {LIMIT_MIB} MiB: more than one layer was held",
        peak / 1024
    );
}

// --- Output subfolders and the write observer ---

/// The three-shot bracket `IMG_0001` to `IMG_0003` of the subfolder
/// scenarios, in `dir`, created first.
fn bracket_of_three_in(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    bracket(
        dir,
        &["IMG_0001.jpg", "IMG_0002.jpg", "IMG_0003.jpg"],
        &[0, -1, 1],
    );
}

/// The folder of group `group`'s first photo, relative to `input`, with
/// `/` between its components.
fn group_folder(analysis: &Analysis, input: &Path, group: usize) -> String {
    let first = &analysis.files[analysis.groups[group].members[0]].path;
    let rel = first.parent().unwrap().strip_prefix(input).unwrap();
    let parts: Vec<&str> = rel.iter().map(|part| part.to_str().unwrap()).collect();
    parts.join("/")
}

#[test]
fn recursive_stacks_mirror_their_subfolder() {
    // The same names in `a`, `b` and at the top: each stack goes to the
    // output's mirror of its photos' folder, so none of them collide.
    let dir = tempfile::tempdir().unwrap();
    for sub in ["", "a", "b"] {
        bracket_of_three_in(&dir.path().join(sub));
    }
    let mut config = JobConfig::new(dir.path().to_path_buf());
    config.recursive = true;
    let analysis = analyze_ok(&config);
    assert_eq!(analysis.groups.len(), 3, "{:?}", analysis.groups);

    let report = write_all(
        &config,
        &analysis,
        WriteOptions::default(),
        &ignore,
        &never(),
    )
    .unwrap();
    let output = config.output_dir.canonicalize().unwrap();
    let mut paths: Vec<PathBuf> = report
        .outcomes
        .iter()
        .map(|(_, outcome)| written(outcome.as_ref().unwrap()).0.to_path_buf())
        .collect();
    paths.sort();
    let stack = "IMG_0001_3.psd";
    assert_eq!(
        paths,
        [
            output.join(stack),
            output.join("a").join(stack),
            output.join("b").join(stack),
        ]
    );
    assert_eq!(entries(&output), [stack, "a", "b"]);
    for sub in ["a", "b"] {
        assert_eq!(entries(&output.join(sub)), [stack]);
    }

    // write_group mirrors the folder too.
    let group = (0..analysis.groups.len())
        .find(|&g| group_folder(&analysis, dir.path(), g) == "b")
        .unwrap();
    let (outcome, _) = write_group(
        &config,
        &analysis,
        group,
        WriteOptions::default(),
        &ignore,
        &never(),
    )
    .unwrap();
    assert_eq!(
        written(&outcome).0,
        output.join("b").join("IMG_0001_3-1.psd")
    );
}

#[cfg(unix)]
#[test]
fn subfolder_link_leaving_the_output_is_refused() {
    // `stacked/a` links out of the output folder: the stacks of `a` and of
    // `a/deep` fail, nothing is created outside, and `b` is written.
    let dir = tempfile::tempdir().unwrap();
    for sub in ["a", "a/deep", "b"] {
        bracket_of_three_in(&dir.path().join(sub));
    }
    let outside = tempfile::tempdir().unwrap();
    let mut config = JobConfig::new(dir.path().to_path_buf());
    config.recursive = true;
    fs::create_dir(&config.output_dir).unwrap();
    std::os::unix::fs::symlink(outside.path(), config.output_dir.join("a")).unwrap();
    let analysis = analyze_ok(&config);
    assert_eq!(analysis.groups.len(), 3, "{:?}", analysis.groups);

    let report = write_all(
        &config,
        &analysis,
        WriteOptions::default(),
        &ignore,
        &never(),
    )
    .unwrap();
    assert_eq!(report.outcomes.len(), 3);
    for (group, outcome) in &report.outcomes {
        match group_folder(&analysis, dir.path(), *group).as_str() {
            folder @ ("a" | "a/deep") => assert_eq!(
                outcome.as_ref().unwrap_err(),
                &format!("the output subfolder {folder} leaves the output folder")
            ),
            "b" => assert_eq!(
                file_name(written(outcome.as_ref().unwrap()).0),
                "IMG_0001_3.psd"
            ),
            other => panic!("a group in {other:?}"),
        }
    }
    assert!(entries(outside.path()).is_empty());
    assert_eq!(entries(&config.output_dir.join("b")), ["IMG_0001_3.psd"]);
}

/// Records every observer call, in order.
#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<String>>,
}

impl Recorder {
    fn push(&self, event: String) {
        self.events.lock().unwrap().push(event);
    }
}

impl WriteObserver for Recorder {
    fn layer(&self, file: usize) {
        self.push(format!("layer {file}"));
    }

    fn partial_created(&self, group: usize, partial: &Path) {
        let with = |suffix: &str| {
            let mut path = partial.as_os_str().to_owned();
            path.push(suffix);
            PathBuf::from(path)
        };
        let files = [partial.to_path_buf(), with(".layers"), with(".merged")];
        let existing = files.iter().filter(|file| file.is_file()).count();
        self.push(format!(
            "partial {group} {} ({existing} files)",
            file_name(partial)
        ));
    }

    fn group_done(
        &self,
        group: usize,
        outcome: &Result<WriteOutcome, String>,
        warnings: &[(usize, Warning)],
    ) {
        let name = file_name(written(outcome.as_ref().unwrap()).0).to_owned();
        self.push(format!("done {group} {name} ({} warnings)", warnings.len()));
    }
}

#[test]
fn partial_created_comes_before_any_layer() {
    let dir = tempfile::tempdir().unwrap();
    let mut paths = three_jpegs(dir.path());
    for (seed, name) in [(4, "d.jpg"), (5, "e.jpg")] {
        let path = dir.path().join(name);
        plain_jpeg(&path, seed, WIDTH, HEIGHT, None);
        paths.push(path);
    }
    let config = JobConfig::new(dir.path().to_path_buf());
    let analysis = analysis_of(&paths, &[]);
    let recorder = Recorder::default();

    let report = write_groups(
        &config,
        &analysis.files,
        &[vec![0, 1, 2], vec![3, 4]],
        WriteOptions::default(),
        &recorder,
        &never(),
    )
    .unwrap();
    assert_eq!(report.outcomes.len(), 2);
    assert_eq!(
        recorder.events.into_inner().unwrap(),
        [
            "partial 0 a_3.psd.partial (3 files)",
            "layer 0",
            "layer 1",
            "layer 2",
            "done 0 a_3.psd (0 warnings)",
            "partial 1 d_2.psd.partial (3 files)",
            "layer 3",
            "layer 4",
            "done 1 d_2.psd (0 warnings)",
        ]
    );
    assert_eq!(entries(&config.output_dir), ["a_3.psd", "d_2.psd"]);
}

// --- Predicting a stack ---

/// The members of group `group` of `analysis`, as [`predict_stack`] takes
/// them.
fn predict_members(analysis: &Analysis, group: usize) -> Vec<PredictMember<'_>> {
    analysis.groups[group]
        .members
        .iter()
        .map(|&id| {
            let file = &analysis.files[id];
            PredictMember {
                id,
                path: &file.path,
                kind: file.kind,
                exif: file.exif.as_ref(),
            }
        })
        .collect()
}

/// The prediction of group `group` of `analysis`, in the format and RAW
/// tone of `config`, as `write_group` would write it.
fn predict_group(config: &JobConfig, analysis: &Analysis, group: usize) -> StackPrediction {
    let members = predict_members(analysis, group);
    predict_stack(&members, config.output_format, config.raw_tone, &never())
        .expect("a prediction that is never stopped gives one")
}

/// The probe of a `width`×`height` RGB member without alpha.
fn member_probe(width: u32, height: u32, sixteen_bit: bool) -> MemberProbe {
    MemberProbe {
        width,
        height,
        sixteen_bit,
        has_alpha: false,
    }
}

/// Writes scene `seed` at `width`×`height` to `path` as a 16-bit RGB TIFF.
fn tiff16(path: &Path, seed: u64, width: u32, height: u32) {
    let base = scene(seed, width, height);
    let image: ImageBuffer<Rgb<u16>, Vec<u16>> = ImageBuffer::from_fn(width, height, |x, y| {
        Rgb(base.get_pixel(x, y).0.map(|v| u16::from(v) * 256 + 3))
    });
    image.save_with_format(path, ImageFormat::Tiff).unwrap();
}

#[test]
fn a_prediction_matches_what_the_writer_writes() {
    // An 8-bit JPEG bracket, with its EXIF, written as a PSD.
    let dir = tempfile::tempdir().unwrap();
    bracket_of_five(dir.path());
    let config = JobConfig::new(dir.path().to_path_buf());
    let analysis = analyze_ok(&config);
    assert_eq!(analysis.groups.len(), 1, "{:?}", analysis.groups);
    let prediction = predict_group(&config, &analysis, 0);
    let (outcome, warnings) = write_group(
        &config,
        &analysis,
        0,
        WriteOptions::default(),
        &ignore,
        &never(),
    )
    .unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    let (path, layers, container) = written(&outcome);
    assert_eq!(prediction.file_name.as_deref(), Some(file_name(path)));
    assert_eq!(prediction.file_name.as_deref(), Some("IMG_0009_5.psd"));
    assert_eq!(
        (prediction.kept.len(), prediction.container),
        (layers, container)
    );
    assert_eq!(prediction.kept, analysis.groups[0].members);
    let psd = read_psd(path);
    assert_eq!(prediction.canvas, Some((psd.width, psd.height)));
    assert_eq!(prediction.sixteen_bit, psd.depth == 16);
    assert_eq!(prediction.has_alpha, psd.channels == 4);
    assert!(!prediction.sixteen_bit && !prediction.has_alpha && !prediction.icc_mismatch);
    let expected: Vec<_> = analysis.groups[0]
        .members
        .iter()
        .map(|&id| (id, Ok(member_probe(WIDTH, HEIGHT, false))))
        .collect();
    assert_eq!(prediction.members, expected);

    // A 16-bit TIFF bracket written as a layered TIFF.
    let dir = tempfile::tempdir().unwrap();
    let paths: Vec<PathBuf> = ["a.tif", "b.tif", "c.tif"]
        .iter()
        .map(|name| dir.path().join(name))
        .collect();
    for (seed, path) in (1..).zip(&paths) {
        tiff16(path, seed, WIDTH, HEIGHT);
    }
    let mut config = JobConfig::new(dir.path().to_path_buf());
    config.output_format = OutputFormat::Tiff;
    let analysis = analysis_of(&paths, &[&[0, 1, 2]]);
    let prediction = predict_group(&config, &analysis, 0);
    let (outcome, warnings) = write_group(
        &config,
        &analysis,
        0,
        WriteOptions::default(),
        &ignore,
        &never(),
    )
    .unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    let (path, layers, container) = written(&outcome);
    assert_eq!(prediction.file_name.as_deref(), Some(file_name(path)));
    assert_eq!(prediction.file_name.as_deref(), Some("a_3.tif"));
    assert_eq!(
        (prediction.kept.len(), prediction.container),
        (layers, container)
    );
    assert_eq!(container, Container::Tiff);
    let bytes = fs::read(path).unwrap();
    let mut decoder = tiff::decoder::Decoder::new(std::io::Cursor::new(&bytes)).unwrap();
    assert_eq!(prediction.canvas, Some(decoder.dimensions().unwrap()));
    let composite = decoder.read_image().unwrap();
    assert!(
        matches!(composite, tiff::decoder::DecodingResult::U16(_)),
        "the stack is 16-bit"
    );
    assert!(prediction.sixteen_bit && !prediction.has_alpha);
    let expected: Vec<_> = (0..3)
        .map(|id| (id, Ok(member_probe(WIDTH, HEIGHT, true))))
        .collect();
    assert_eq!(prediction.members, expected);
}

#[test]
fn a_prediction_names_the_member_that_cannot_be_read() {
    let dir = tempfile::tempdir().unwrap();
    let mut paths = three_jpegs(dir.path());
    let broken = dir.path().join("broken.jpg");
    fs::write(&broken, b"not a photo").unwrap();
    paths.insert(1, broken);
    let config = JobConfig::new(dir.path().to_path_buf());
    let analysis = analysis_of(&paths, &[&[0, 1, 2, 3]]);

    let prediction = predict_group(&config, &analysis, 0);
    let ids: Vec<usize> = prediction.members.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, [0, 1, 2, 3]);
    let (_, Err(reason)) = &prediction.members[1] else {
        panic!("the broken member probed: {:?}", prediction.members[1]);
    };
    for (_, probe) in [0, 2, 3].map(|i| &prediction.members[i]) {
        assert_eq!(probe, &Ok(member_probe(WIDTH, HEIGHT, false)));
    }
    assert_eq!(prediction.kept, [0, 2, 3]);
    assert_eq!(prediction.file_name.as_deref(), Some("a_3.psd"));

    // The reason is the writer's.
    let (outcome, warnings) = write_group(
        &config,
        &analysis,
        0,
        WriteOptions::default(),
        &ignore,
        &never(),
    )
    .unwrap();
    let reason = reason.clone();
    assert_eq!(warnings, [(1, Warning::LayerUnreadable { reason })]);
    assert_eq!(file_name(written(&outcome).0), "a_3.psd");
}

#[test]
fn a_prediction_of_mixed_resolutions_keeps_the_most_common() {
    let dir = tempfile::tempdir().unwrap();
    let names = ["a.jpg", "s.jpg", "b.jpg", "t.jpg", "c.jpg"];
    let paths: Vec<PathBuf> = names.iter().map(|name| dir.path().join(name)).collect();
    for (seed, path) in (1..).zip(&paths) {
        let small = file_name(path).starts_with(['s', 't']);
        let (width, height) = if small {
            (WIDTH / 2, HEIGHT / 2)
        } else {
            (WIDTH, HEIGHT)
        };
        plain_jpeg(path, seed, width, height, None);
    }
    let config = JobConfig::new(dir.path().to_path_buf());
    // Layer order against the ids: `c` is on top.
    let analysis = analysis_of(&paths, &[&[4, 3, 2, 1, 0]]);

    let prediction = predict_group(&config, &analysis, 0);
    let full = member_probe(WIDTH, HEIGHT, false);
    let small = member_probe(WIDTH / 2, HEIGHT / 2, false);
    assert_eq!(
        prediction.members,
        [
            (4, Ok(full.clone())),
            (3, Ok(small.clone())),
            (2, Ok(full.clone())),
            (1, Ok(small)),
            (0, Ok(full)),
        ]
    );
    assert_eq!(prediction.canvas, Some((WIDTH, HEIGHT)));
    assert_eq!(prediction.kept, [4, 2, 0]);
    assert_eq!(prediction.container, Container::Psd);
    assert_eq!(prediction.file_name.as_deref(), Some("c_3.psd"));
}

#[test]
fn a_prediction_with_one_kept_member_has_no_file_name() {
    let dir = tempfile::tempdir().unwrap();
    let paths: Vec<PathBuf> = ["a.jpg", "s.jpg", "broken.jpg"]
        .iter()
        .map(|name| dir.path().join(name))
        .collect();
    plain_jpeg(&paths[0], 1, WIDTH, HEIGHT, None);
    plain_jpeg(&paths[1], 2, WIDTH / 2, HEIGHT / 2, None);
    fs::write(&paths[2], b"not a photo").unwrap();
    let config = JobConfig::new(dir.path().to_path_buf());
    let analysis = analysis_of(&paths, &[&[0, 1], &[2]]);

    // A tie: the top layer's resolution wins, and keeps one member.
    let prediction = predict_group(&config, &analysis, 0);
    assert_eq!(prediction.canvas, Some((WIDTH, HEIGHT)));
    assert_eq!(prediction.kept, [0]);
    assert_eq!(prediction.file_name, None);

    // No member probed: no canvas, nothing kept, no name.
    let prediction = predict_group(&config, &analysis, 1);
    assert!(matches!(prediction.members.as_slice(), [(2, Err(_))]));
    assert_eq!(prediction.canvas, None);
    assert!(prediction.kept.is_empty());
    assert_eq!(prediction.file_name, None);
    assert_eq!(prediction.container, Container::Psd);
    assert!(!prediction.sixteen_bit && !prediction.has_alpha && !prediction.icc_mismatch);
}

#[test]
fn a_wide_canvas_is_predicted_as_psb() {
    // A side over 30,000 px makes the large container of either format.
    let dir = tempfile::tempdir().unwrap();
    let paths: Vec<PathBuf> = ["wide_a.png", "wide_b.png"]
        .iter()
        .map(|name| dir.path().join(name))
        .collect();
    for path in &paths {
        RgbImage::new(30_001, 2)
            .save_with_format(path, ImageFormat::Png)
            .unwrap();
    }
    let mut config = JobConfig::new(dir.path().to_path_buf());
    let analysis = analysis_of(&paths, &[&[0, 1]]);
    for (format, container, name) in [
        (OutputFormat::Psd, Container::Psb, "wide_a_2.psb"),
        (OutputFormat::Tiff, Container::BigTiff, "wide_a_2.tif"),
    ] {
        config.output_format = format;
        let prediction = predict_group(&config, &analysis, 0);
        assert_eq!(prediction.canvas, Some((30_001, 2)), "{format:?}");
        assert_eq!(prediction.container, container);
        assert_eq!(prediction.file_name.as_deref(), Some(name));
    }
}

#[test]
fn a_stopped_prediction_gives_none() {
    let dir = tempfile::tempdir().unwrap();
    let paths = three_jpegs(dir.path());
    let analysis = analysis_of(&paths, &[&[0, 1, 2]]);
    let members = predict_members(&analysis, 0);
    let stop = AtomicBool::new(true);
    let prediction = predict_stack(&members, OutputFormat::Psd, RawTone::Srgb, &stop);
    assert_eq!(prediction, None);
}
