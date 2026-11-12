//! End-to-end tests of the `photostacker-cli` binary: synthetic brackets are
//! written to a temporary folder and the CLI is run on them.

use std::fs;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use assert_cmd::Command;
use assert_cmd::assert::Assert;
use photostacker_core::appdirs::DATA_DIR_ENV;
use photostacker_core::model::RawTone;
use photostacker_core::models::MODELS;
use photostacker_core::{AppDirs, Engine, EngineOptions, PlanV1, Store};
use photostacker_testkit::{
    MakerNoteSpec, NoteStyle, TiffSpec, Value, expose, jpeg_with_exif, psd_reader, scene,
};

const WIDTH: u32 = 640;
const HEIGHT: u32 = 426;
const JPEG_QUALITY: u8 = 92;

/// Canon EXIF for AEB shot `shot` (1-based), taken `shot - 1` seconds after
/// 10:`minute`:00.
fn canon_bracket_exif(minute: u8, shot: u16) -> TiffSpec {
    // FileInfo: word 0 the byte count, word 3 BracketMode (1 = AEB),
    // word 5 BracketShotNumber.
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
                Value::Ascii(format!("2026:05:01 10:{minute:02}:0{}", shot - 1)),
            ),
            (0x920A, Value::Rational(vec![(35, 1)])),
        ],
        maker_note: Some(MakerNoteSpec {
            style: NoteStyle::CanonIfd,
            entries: vec![(0x0093, Value::Short(file_info))],
        }),
    }
}

/// Writes a three-shot bracket (EV −1, 0, +1) of one scene into `dir` under
/// `names`, in shot order.
fn write_bracket(dir: &Path, names: [&str; 3]) {
    write_bracket_of(dir, names, 1, 0);
}

/// [`write_bracket`] of scene `seed`, shot from 10:`minute`:00 on. Brackets
/// a minute apart are never one group.
fn write_bracket_of(dir: &Path, names: [&str; 3], seed: u64, minute: u8) {
    fs::create_dir_all(dir).unwrap();
    let base = scene(seed, WIDTH, HEIGHT);
    for (i, (name, ev)) in names.iter().zip([-1.0, 0.0, 1.0]).enumerate() {
        let exif = canon_bracket_exif(minute, i as u16 + 1).build();
        let bytes = jpeg_with_exif(&expose(&base, ev), &exif, JPEG_QUALITY);
        fs::write(dir.join(name), bytes).unwrap();
    }
}

const BRACKET: [&str; 3] = ["IMG_0001.jpg", "IMG_0002.jpg", "IMG_0003.jpg"];
const SECOND_BRACKET: [&str; 3] = ["IMG_0004.jpg", "IMG_0005.jpg", "IMG_0006.jpg"];

/// The CLI with the app data in `data` (spec §7.1), so no test touches the
/// user's real data.
fn photostacker_in(data: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_photostacker-cli"));
    command.env(DATA_DIR_ENV, data);
    command
}

/// The CLI with the app data in a fresh temporary folder of its own, removed
/// when the value is dropped. It derefs to its [`Command`].
fn photostacker() -> Photostacker {
    let data = tempfile::tempdir().unwrap();
    Photostacker {
        command: photostacker_in(data.path()),
        data,
    }
}

struct Photostacker {
    command: Command,
    data: tempfile::TempDir,
}

impl Photostacker {
    fn data(&self) -> &Path {
        self.data.path()
    }
}

impl Deref for Photostacker {
    type Target = Command;

    fn deref(&self) -> &Command {
        &self.command
    }
}

impl DerefMut for Photostacker {
    fn deref_mut(&mut self) -> &mut Command {
        &mut self.command
    }
}

fn read_plan(path: &Path) -> PlanV1 {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn paths(names: &[&str]) -> Vec<PathBuf> {
    names.iter().map(PathBuf::from).collect()
}

/// The tool is `photostacker-cli`: a name that differs from the app's
/// (`Photostacker`) by more than case, so the two files can sit side by side
/// on Windows and macOS, whose file names ignore case.
#[test]
fn the_tool_is_named_photostacker_cli() {
    photostacker()
        .arg("--version")
        .assert()
        .success()
        .stdout(format!("photostacker-cli {}\n", env!("CARGO_PKG_VERSION")));

    let output = photostacker()
        .arg("--help")
        .assert()
        .success()
        .get_output()
        .clone();
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("Usage: photostacker-cli"), "{help}");
}

#[test]
fn dry_run_writes_plan_file() {
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("photos");
    write_bracket(&input, BRACKET);
    let plan_file = root.path().join("groups.json");

    let output = photostacker()
        .arg("run")
        .arg(&input)
        .arg("--dry-run")
        .arg("--plan")
        .arg(&plan_file)
        .assert()
        .success()
        .get_output()
        .clone();

    let plan = read_plan(&plan_file);
    assert_eq!(plan.groups.len(), 1, "{plan:?}");
    assert_eq!(plan.groups[0].files, paths(&BRACKET));
    assert!(plan.unstacked.is_empty() && plan.errors.is_empty());
    // With a plan file, stdout stays empty; the summary goes to stderr.
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("3 files, 1 groups, 0 unstacked, 0 errors"),
        "{stderr}"
    );
}

#[test]
fn plan_to_stdout_when_no_file() {
    let root = tempfile::tempdir().unwrap();
    write_bracket(root.path(), BRACKET);

    let output = photostacker()
        .arg("run")
        .arg(root.path())
        .arg("--dry-run")
        .assert()
        .success()
        .get_output()
        .clone();

    let plan: PlanV1 = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan.version, PlanV1::VERSION);
    assert_eq!(plan.groups.len(), 1);
    assert_eq!(plan.groups[0].files, paths(&BRACKET));
}

#[test]
fn invalid_threshold_exits_2() {
    let root = tempfile::tempdir().unwrap();
    let assert = photostacker()
        .arg("run")
        .arg(root.path())
        .args(["--dry-run", "--hamming", "99"])
        .assert()
        .code(2)
        .stdout("");
    assert_stderr_contains(&assert, "hamming_threshold = 99 is out of range (0 to 40)");
}

/// Review Focus 2: a non-ASCII folder and file names survive into the plan.
#[test]
fn cli_handles_non_ascii_paths() {
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("Viaggio Śląsk");
    let names = ["Śląsk 1.jpg", "Śląsk 2.jpg", "Śląsk 3.jpg"];
    write_bracket(&input, names);
    let plan_file = root.path().join("plan ę.json");

    photostacker()
        .arg("run")
        .arg(&input)
        .arg("--dry-run")
        .arg("--plan")
        .arg(&plan_file)
        .assert()
        .success();

    let plan = read_plan(&plan_file);
    assert_eq!(plan.input_dir, input);
    assert_eq!(plan.config.input_dir, input);
    assert_eq!(plan.groups.len(), 1, "{plan:?}");
    assert_eq!(plan.groups[0].files, paths(&names));
}

/// Failing to write the plan is a runtime failure.
#[test]
fn unwritable_plan_exits_1() {
    let root = tempfile::tempdir().unwrap();
    let plan_file = root.path().join("missing-folder").join("groups.json");
    let assert = photostacker()
        .arg("run")
        .arg(root.path())
        .arg("--dry-run")
        .arg("--plan")
        .arg(&plan_file)
        .assert()
        .code(1);
    assert_stderr_contains(&assert, "cannot write the plan to");
}

/// An existing plan file is replaced, and no `.partial` is left behind.
#[test]
fn plan_file_is_replaced_without_leftovers() {
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("photos");
    write_bracket(&input, BRACKET);
    let plan_file = root.path().join("groups.json");
    fs::write(&plan_file, b"an older plan").unwrap();

    photostacker()
        .arg("run")
        .arg(&input)
        .arg("--dry-run")
        .arg("--plan")
        .arg(&plan_file)
        .assert()
        .success();

    assert_eq!(read_plan(&plan_file).groups.len(), 1);
    assert!(!root.path().join("groups.json.partial").exists());
}

/// An input folder that passes `validate` but cannot be listed
/// is a runtime failure.
#[cfg(unix)]
#[test]
fn unreadable_input_root_exits_1() {
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("photos");
    write_bracket(&input, BRACKET);
    // Declared after `root`, so the folder is unlocked before it is removed,
    // even when the run or an assertion panics.
    let _unlock = chmod(&input, 0o000);
    if fs::read_dir(&input).is_ok() {
        // Root ignores permissions, so the folder stays readable.
        eprintln!("skipped: running as root, chmod 000 does not lock the folder");
        return;
    }

    let output = photostacker()
        .arg("run")
        .arg(&input)
        .arg("--dry-run")
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("cannot analyse"), "{stderr}");
    // The operating system's reason is kept.
    assert!(stderr.contains("ermission denied"), "{stderr}");
}

/// JSON needs UTF-8 paths, so a non-UTF-8 input folder is a
/// runtime failure whose message names the problem.
#[cfg(unix)]
#[test]
fn non_utf8_path_exits_1_with_a_clear_message() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let root = tempfile::tempdir().unwrap();
    let input = root.path().join(OsStr::from_bytes(b"bad-\xFF-name"));
    fs::create_dir(&input).unwrap();
    let assert = photostacker()
        .arg("run")
        .arg(&input)
        .arg("--dry-run")
        .assert()
        .code(1)
        .stdout("");
    assert_stderr_contains(&assert, "is not valid UTF-8");
}

/// The lines of stdout, each a path.
fn stdout_paths(assert: &Assert) -> Vec<PathBuf> {
    String::from_utf8(assert.get_output().stdout.clone())
        .unwrap()
        .lines()
        .map(PathBuf::from)
        .collect()
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

/// Asserts that `path` is a PSD of the three photos `names`, in shot order
/// from the top layer down.
fn assert_stack_of(path: &Path, names: [&str; 3]) {
    let psd = psd_reader::read(&fs::read(path).unwrap()).unwrap();
    assert_eq!((psd.width, psd.height), (WIDTH, HEIGHT));
    // The reader lists the bottom layer first.
    let layers: Vec<String> = psd
        .layers
        .iter()
        .rev()
        .map(|layer| layer.name_unicode.clone().unwrap())
        .collect();
    assert_eq!(layers.len(), names.len(), "{layers:?}");
    for (layer, name) in layers.iter().zip(names) {
        let stem = Path::new(name).file_stem().unwrap().to_str().unwrap();
        assert!(layer.starts_with(stem), "{layer:?} is not {stem}");
    }
}

#[test]
fn run_writes_one_psd_per_group_and_prints_paths() {
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("photos");
    write_bracket(&input, BRACKET);
    write_bracket_of(&input, SECOND_BRACKET, 2, 5);

    let assert = photostacker().arg("run").arg(&input).assert().success();

    // The default output folder, `<input>/stacked`; the paths are absolute.
    let output = input.join("stacked").canonicalize().unwrap();
    assert_eq!(
        stdout_paths(&assert),
        [output.join("IMG_0001_3.psd"), output.join("IMG_0004_3.psd")]
    );
    assert_eq!(entries(&output), ["IMG_0001_3.psd", "IMG_0004_3.psd"]);
    assert_stack_of(&output.join("IMG_0001_3.psd"), BRACKET);
    assert_stack_of(&output.join("IMG_0004_3.psd"), SECOND_BRACKET);
    assert_stderr_contains(&assert, "write: writing 6 layers");
    assert_stderr_contains(
        &assert,
        "6 files, 2 groups, 0 unstacked, 0 errors; 2 written, 0 skipped, 0 failed",
    );
}

/// A second run writes next to the first stack, never over it.
#[test]
fn run_twice_never_overwrites() {
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("photos");
    write_bracket(&input, BRACKET);
    let output = root.path().join("out");
    let run = || {
        photostacker()
            .arg("run")
            .arg(&input)
            .arg("-o")
            .arg(&output)
            .assert()
            .success()
    };

    let first = stdout_paths(&run());
    let first_bytes = fs::read(&first[0]).unwrap();
    let second = stdout_paths(&run());

    let output = output.canonicalize().unwrap();
    assert_eq!(first, [output.join("IMG_0001_3.psd")]);
    assert_eq!(second, [output.join("IMG_0001_3-1.psd")]);
    assert_eq!(fs::read(&first[0]).unwrap(), first_bytes);
    assert_eq!(entries(&output), ["IMG_0001_3-1.psd", "IMG_0001_3.psd"]);
}

/// `--raw-tone` reaches the job config, which the plan records.
#[test]
fn raw_tone_flag_maps_to_config() {
    let root = tempfile::tempdir().unwrap();
    write_bracket(root.path(), BRACKET);
    let plan_of = |extra: &[&str]| -> PlanV1 {
        let output = photostacker()
            .arg("run")
            .arg(root.path())
            .arg("--dry-run")
            .args(extra)
            .assert()
            .success()
            .get_output()
            .clone();
        serde_json::from_slice(&output.stdout).unwrap()
    };

    assert_eq!(plan_of(&[]).config.raw_tone, RawTone::Srgb);
    assert_eq!(
        plan_of(&["--raw-tone", "linear"]).config.raw_tone,
        RawTone::Linear
    );
    assert_eq!(
        plan_of(&["--raw-tone", "srgb"]).config.raw_tone,
        RawTone::Srgb
    );
}

#[test]
fn format_tiff_writes_tif() {
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("photos");
    write_bracket(&input, BRACKET);
    let output = root.path().join("out");

    let assert = photostacker()
        .arg("run")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .args(["--format", "tiff"])
        .assert()
        .success();

    let path = output.canonicalize().unwrap().join("IMG_0001_3.tif");
    assert_eq!(stdout_paths(&assert), std::slice::from_ref(&path));
    // A big-endian classic TIFF.
    assert_eq!(fs::read(&path).unwrap()[..4], *b"MM\0*");
}

/// In write mode, `--plan` writes the plan, with `auto_confirm` set, as well
/// as the stacks; stdout carries only the paths.
#[test]
fn run_with_plan_writes_the_plan_and_the_stacks() {
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("photos");
    write_bracket(&input, BRACKET);
    let output = root.path().join("out");
    let plan_file = root.path().join("groups.json");

    let assert = photostacker()
        .arg("run")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--plan")
        .arg(&plan_file)
        .assert()
        .success();

    let plan = read_plan(&plan_file);
    assert!(plan.config.auto_confirm);
    assert_eq!(plan.groups[0].files, paths(&BRACKET));
    let path = output.canonicalize().unwrap().join("IMG_0001_3.psd");
    assert_eq!(stdout_paths(&assert), [path]);
}

/// With no group there is nothing to write: the output folder is neither
/// created nor checked, so one that could not be created fails nothing.
#[test]
fn run_without_groups_creates_no_output_folder() {
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("photos");
    fs::create_dir(&input).unwrap();
    // One photo alone is no group.
    let exif = canon_bracket_exif(0, 1).build();
    let photo = jpeg_with_exif(&scene(1, WIDTH, HEIGHT), &exif, JPEG_QUALITY);
    fs::write(input.join("IMG_0001.jpg"), photo).unwrap();
    let output = root.path().join("out");
    // Its parent is a file, so this output passes validation but could
    // not be created.
    let file = root.path().join("notes.txt");
    fs::write(&file, b"a file").unwrap();
    let uncreatable = file.join("out");

    for extra in [
        vec![],
        vec!["-o".into(), output.clone().into_os_string()],
        vec!["-o".into(), uncreatable.into_os_string()],
    ] {
        let assert = photostacker()
            .arg("run")
            .arg(&input)
            .args(&extra)
            .assert()
            .success()
            .stdout("");
        assert_stderr_contains(
            &assert,
            "1 files, 0 groups, 1 unstacked, 0 errors; 0 written, 0 skipped, 0 failed",
        );
        let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
        assert!(!stderr.contains("write: "), "{stderr}");
    }
    assert_eq!(entries(&input), ["IMG_0001.jpg"]);
    assert!(!output.exists());
}

/// In write mode the plan file is optional, so every per-file error, and
/// every entry the scan could not read, gets an `error: <file>: <reason>`
/// line on stderr, relative to the input and in path order.
#[test]
fn run_names_each_per_file_error() {
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("photos");
    write_bracket(&input, BRACKET);
    let whole = fs::read(input.join(BRACKET[0])).unwrap();
    fs::write(input.join("aa-cut.jpg"), &whole[..whole.len() / 2]).unwrap();
    fs::write(input.join("zz-garbage.jpg"), b"not a photo at all").unwrap();
    let mut failed = vec!["aa-cut.jpg"];
    // A dangling symlink is an entry the scan cannot read; where the account
    // may not make one, the rest of the test still runs.
    let gone = root.path().join("gone.jpg");
    if photostacker_testkit::symlink_file(&gone, &input.join("mm-dangling.jpg")) {
        failed.push("mm-dangling.jpg");
    }
    failed.push("zz-garbage.jpg");

    let assert = photostacker()
        .arg("run")
        .arg(&input)
        .arg("-o")
        .arg(root.path().join("out"))
        .assert()
        .success();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    let errors: Vec<&str> = stderr
        .lines()
        .filter(|line| line.starts_with("error: "))
        .collect();
    assert_eq!(errors.len(), failed.len(), "{stderr}");
    for (line, name) in errors.iter().zip(&failed) {
        let prefix = format!("error: {name}: ");
        assert!(line.starts_with(&prefix), "{line:?} is not {prefix:?}…");
        assert!(line.len() > prefix.len(), "{line:?} gives no reason");
    }
    let path = root.path().join("out").canonicalize().unwrap();
    assert_eq!(stdout_paths(&assert), [path.join("IMG_0001_3.psd")]);
    let summary = format!(
        "{} files, 1 groups, 0 unstacked, {} errors; 1 written, 0 skipped, 0 failed",
        BRACKET.len() + 2,
        failed.len()
    );
    assert!(stderr.contains(&summary), "{summary:?} not in {stderr}");
}

/// rawler logs recoverable trouble through `log`, at error level too (a
/// file too short to sniff). By default none of it reaches stderr: the core
/// reports such a file itself.
#[test]
fn rawler_logs_stay_off_stderr_by_default() {
    let input = tempfile::tempdir().unwrap();
    fs::write(input.path().join("IMG_0001.cr2"), b"\x49\x49\x2A").unwrap();
    let exif = canon_bracket_exif(0, 1).build();
    let photo = jpeg_with_exif(&scene(1, WIDTH, HEIGHT), &exif, JPEG_QUALITY);
    fs::write(input.path().join("IMG_0002.jpg"), photo).unwrap();

    let assert = photostacker()
        .env_remove("RUST_LOG")
        .arg("run")
        .arg(input.path())
        .arg("--dry-run")
        .assert()
        .success();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("2 files, 0 groups, 1 unstacked, 1 errors"),
        "{stderr}"
    );
    assert!(!stderr.contains("ERROR"), "{stderr}");
    assert!(!stderr.contains("log.target"), "{stderr}");
}

/// A plan that cannot be written stops the run before any stack is written.
#[test]
fn unwritable_plan_exits_1_before_writing() {
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("photos");
    write_bracket(&input, BRACKET);
    let output = root.path().join("out");
    let plan_file = root.path().join("missing-folder").join("groups.json");

    let assert = photostacker()
        .arg("run")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--plan")
        .arg(&plan_file)
        .assert()
        .code(1)
        .stdout("");
    assert_stderr_contains(&assert, "cannot write the plan to");
    assert!(!output.exists());
}

/// Gives a folder the mode 755 back when dropped, so that the temporary
/// folder holding it can be removed even after a panic.
#[cfg(unix)]
struct Unlock<'a>(&'a Path);

#[cfg(unix)]
impl Drop for Unlock<'_> {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;

        let _ = fs::set_permissions(self.0, fs::Permissions::from_mode(0o755));
    }
}

/// Sets the mode of `dir`; the guard restores 755. Declare it after the
/// temporary folder, so it drops first.
#[cfg(unix)]
fn chmod(dir: &Path, mode: u32) -> Unlock<'_> {
    use std::os::unix::fs::PermissionsExt;

    let unlock = Unlock(dir);
    fs::set_permissions(dir, fs::Permissions::from_mode(mode)).unwrap();
    unlock
}

/// Makes `dir` read-only until the guard drops. Returns `None`, with `dir`
/// writable again, when that does not stop writes into it (as root).
#[cfg(unix)]
fn lock(dir: &Path) -> Option<Unlock<'_>> {
    let unlock = chmod(dir, 0o555);
    let probe = dir.join("probe");
    if fs::write(&probe, b"").is_ok() {
        fs::remove_file(probe).unwrap();
        eprintln!("skipped: running as root, chmod 555 does not lock the folder");
        return None;
    }
    Some(unlock)
}

/// Every group is attempted, and any that fails makes the exit code 1.
#[cfg(unix)]
#[test]
fn write_failure_exits_1() {
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("photos");
    write_bracket(&input, BRACKET);
    write_bracket_of(&input, SECOND_BRACKET, 2, 5);
    let output = root.path().join("out");
    fs::create_dir(&output).unwrap();
    let Some(_unlock) = lock(&output) else {
        return;
    };

    let result = photostacker()
        .arg("run")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&result.stderr);
    assert_eq!(result.status.code(), Some(1), "{stderr}");
    assert!(result.stdout.is_empty());
    for first in ["IMG_0001.jpg", "IMG_0004.jpg"] {
        let line = format!("stack of {} failed: ", input.join(first).display());
        assert!(stderr.contains(&line), "{line:?} not in {stderr}");
    }
    let summary = "6 files, 2 groups, 0 unstacked, 0 errors; 0 written, 0 skipped, 2 failed";
    assert!(stderr.contains(summary), "{stderr}");
    assert!(entries(&output).is_empty());
}

/// An output folder that cannot be created is a runtime failure.
#[cfg(unix)]
#[test]
fn unusable_output_folder_exits_1() {
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("photos");
    write_bracket(&input, BRACKET);
    let parent = root.path().join("locked");
    fs::create_dir(&parent).unwrap();
    let Some(_unlock) = lock(&parent) else {
        return;
    };

    let result = photostacker()
        .arg("run")
        .arg(&input)
        .arg("-o")
        .arg(parent.join("out"))
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&result.stderr);
    assert_eq!(result.status.code(), Some(1), "{stderr}");
    assert!(result.stdout.is_empty());
    assert!(stderr.contains("cannot be created"), "{stderr}");
}

/// Without `--db` the engine's database lives in a temporary folder that is
/// gone when the run ends; the app's own database is never made.
#[test]
fn run_uses_an_ephemeral_db_by_default() {
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("photos");
    write_bracket(&input, BRACKET);
    let tmp = root.path().join("tmp");
    fs::create_dir(&tmp).unwrap();

    let mut cli = photostacker();
    let data = cli.data().to_path_buf();
    let assert = cli
        .env("TMPDIR", &tmp)
        .env("TMP", &tmp)
        .env("TEMP", &tmp)
        .arg("run")
        .arg(&input)
        .arg("-o")
        .arg(root.path().join("out"))
        .assert()
        .success();

    assert_eq!(stdout_paths(&assert).len(), 1);
    assert!(entries(&tmp).is_empty(), "{:?}", entries(&tmp));
    assert!(!AppDirs::under(data.clone()).db().exists());
    assert!(entries(&data).is_empty(), "{:?}", entries(&data));
}

/// `--db` keeps the jobs in that database, and the review thumbnails in the
/// folder beside it, `<db>.thumbs/`.
#[test]
fn run_with_db_keeps_the_job() {
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("photos");
    write_bracket(&input, BRACKET);
    let db = root.path().join("jobs.db");

    for _ in 0..2 {
        photostacker()
            .arg("run")
            .arg(&input)
            .arg("--dry-run")
            .arg("--db")
            .arg(&db)
            .assert()
            .success();
    }

    let jobs = Store::open_read_only(&db).unwrap().jobs().unwrap();
    assert_eq!(jobs.len(), 2, "{jobs:?}");
    assert!(jobs.iter().all(|job| job.config.input_dir == input));
    let thumbs = root.path().join("jobs.db.thumbs");
    let job_folders: Vec<String> = jobs.iter().map(|job| job.id.to_string()).collect();
    assert_eq!(entries(&thumbs), job_folders);
}

/// A database another engine holds (the app, or another run) is refused
/// before the run submits anything; the lock is the operating system's, so
/// an engine of this process holds it against the CLI's.
#[test]
fn run_on_a_database_in_use_exits_1() {
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("photos");
    write_bracket(&input, BRACKET);
    let db = root.path().join("jobs.db");
    let holder = Engine::start(EngineOptions {
        db: db.clone(),
        thumbs_dir: root.path().join("thumbs"),
        models_dir: root.path().join("models"),
        embedder: None,
        installer: None,
        wake: Arc::new(|| {}),
    })
    .unwrap();

    let assert = photostacker()
        .arg("run")
        .arg(&input)
        .arg("--db")
        .arg(&db)
        .assert()
        .code(1)
        .stdout("");

    assert_stderr_contains(
        &assert,
        &format!("error: another Photostacker is using {}", db.display()),
    );
    assert_eq!(holder.jobs().unwrap(), []);
    assert!(!input.join("stacked").exists());
}

/// `--no-ml` turns Stage 3 off in the job's config; by default it names
/// `dinov2-small`, and `--model` and `--cosine` set it.
#[test]
fn no_ml_flag_disables_stage_3() {
    let root = tempfile::tempdir().unwrap();
    write_bracket(root.path(), BRACKET);
    let config_of = |extra: &[&str]| -> serde_json::Value {
        let output = photostacker()
            .arg("run")
            .arg(root.path())
            .arg("--dry-run")
            .args(extra)
            .assert()
            .success()
            .get_output()
            .clone();
        let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        plan["config"].clone()
    };

    assert_eq!(config_of(&["--no-ml"])["ml_model"], serde_json::Value::Null);
    let defaults = config_of(&[]);
    assert_eq!(defaults["ml_model"], "dinov2-small");
    assert_eq!(defaults["cosine_threshold"], serde_json::Value::Null);
    let chosen = config_of(&["--model", "dinov2-base", "--cosine", "0.9"]);
    assert_eq!(chosen["ml_model"], "dinov2-base");
    assert!((chosen["cosine_threshold"].as_f64().unwrap() - 0.9).abs() < 1e-6);
}

/// An unknown model or a cosine out of range is a config error.
#[test]
fn bad_ml_options_exit_2() {
    let root = tempfile::tempdir().unwrap();
    for (extra, message) in [
        (["--model", "dinov2-large"], "dinov2-large"),
        (["--cosine", "0.3"], "cosine_threshold"),
    ] {
        let assert = photostacker()
            .arg("run")
            .arg(root.path())
            .arg("--dry-run")
            .args(extra)
            .assert()
            .code(2)
            .stdout("");
        assert_stderr_contains(&assert, message);
    }
}

/// The lines `models list` prints.
fn models_list(data: &Path) -> Vec<String> {
    let output = photostacker_in(data)
        .args(["models", "list"])
        .assert()
        .success()
        .get_output()
        .clone();
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn models_list_shows_both_models_not_installed() {
    let data = tempfile::tempdir().unwrap();
    let lines = models_list(data.path());
    assert_eq!(lines.len(), MODELS.len(), "{lines:?}");
    for (line, spec) in lines.iter().zip(MODELS) {
        assert!(line.starts_with(spec.id), "{line:?}");
        assert!(line.contains(spec.name), "{line:?}");
        assert!(line.contains(" MiB"), "{line:?}");
        assert!(line.ends_with("installed: no"), "{line:?}");
    }
}

/// A file that is not the registry's is refused and nothing is installed: one of
/// another size before it is copied, by its size.
#[cfg(feature = "ml")]
#[test]
fn models_import_refuses_a_wrong_file() {
    let data = tempfile::tempdir().unwrap();
    let file = data.path().join("model.onnx");
    fs::write(&file, b"not a model").unwrap();

    let assert = photostacker_in(data.path())
        .args(["models", "import", "dinov2-small"])
        .arg(&file)
        .assert()
        .code(1)
        .stdout("");
    assert_stderr_contains(&assert, "this file is 0 MB, DINOv2 small is 84 MB");

    assert!(models_list(data.path())[0].ends_with("installed: no"));
    let model_folder = AppDirs::under(data.path().into())
        .models()
        .join("dinov2-small");
    let left = if model_folder.exists() {
        entries(&model_folder)
    } else {
        Vec::new()
    };
    assert!(
        left.iter().all(|name| !name.starts_with("model.onnx")),
        "{left:?}"
    );
    assert_eq!(fs::read(&file).unwrap(), b"not a model");
}

#[test]
fn models_commands_refuse_an_unknown_model() {
    let data = tempfile::tempdir().unwrap();
    for command in [
        &["remove", "dinov2-large"][..],
        &["download", "dinov2-large"],
    ] {
        let assert = photostacker_in(data.path())
            .arg("models")
            .args(command)
            .assert()
            .code(2);
        assert_stderr_contains(&assert, "no model dinov2-large");
    }
}

#[cfg(not(feature = "ml"))]
#[test]
fn models_download_and_import_need_ml() {
    let data = tempfile::tempdir().unwrap();
    for command in [
        &["download", "dinov2-small"][..],
        &["import", "dinov2-small", "model.onnx"],
    ] {
        let assert = photostacker_in(data.path())
            .arg("models")
            .args(command)
            .assert()
            .code(2);
        assert_stderr_contains(&assert, "built without ml");
    }
}

/// Removing a model that is not installed is no error.
#[test]
fn models_remove_of_a_missing_model_succeeds() {
    let data = tempfile::tempdir().unwrap();
    photostacker_in(data.path())
        .args(["models", "remove", "dinov2-base"])
        .assert()
        .success()
        .stdout("");
}

/// The four lines `info` prints with the app's data in `data` and `env` set
/// for the child, which must end with status 0 and print nothing on stderr.
fn info_lines(data: &Path, env: &[(&str, &Path)]) -> Vec<String> {
    let mut command = photostacker_in(data);
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command
        .arg("info")
        .assert()
        .success()
        .stderr("")
        .get_output()
        .clone();
    let lines: Vec<String> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(lines.len(), 4, "{lines:?}");
    lines
}

/// `info` names the tool, the HEIC decoder, the visual check's runtime and the
/// data folder, in that order, and creates nothing: not even the data folder.
#[test]
fn info_names_the_tool_and_its_parts() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");

    let lines = info_lines(&data, &[]);

    assert_eq!(
        lines[0],
        format!("photostacker-cli {}", env!("CARGO_PKG_VERSION"))
    );
    if cfg!(feature = "heic") {
        assert!(lines[1].starts_with("heic: libheif "), "{lines:?}");
        assert!(lines[1].ends_with(", HEVC decoder libde265"), "{lines:?}");
    } else {
        assert_eq!(lines[1], "heic: not built in");
    }
    if cfg!(feature = "ml") {
        assert!(lines[2].starts_with("visual check: "), "{lines:?}");
    } else {
        assert_eq!(lines[2], "visual check: not built in");
    }
    assert_eq!(lines[3], format!("data: {}", data.display()));
    assert!(!data.exists(), "info created {}", data.display());
    assert!(entries(root.path()).is_empty());
}

/// Without ONNX Runtime the visual check's line says why it cannot run, and
/// `info` still succeeds.
#[cfg(feature = "ml")]
#[test]
fn info_says_why_the_visual_check_cannot_run() {
    let root = tempfile::tempdir().unwrap();
    let nowhere = root.path().join("nowhere").join("libonnxruntime.so");

    let lines = info_lines(root.path(), &[("ORT_DYLIB_PATH", &nowhere)]);

    assert_eq!(
        lines[2],
        format!(
            "visual check: ONNX Runtime not found at {}",
            nowhere.display()
        )
    );
}

/// With ONNX Runtime installed, the line names its version and the library
/// that was loaded. The child inherits `ORT_DYLIB_PATH`, which is what is
/// looked at: without it the child would look beside its own executable,
/// not where this test's process would.
#[cfg(feature = "ml")]
#[test]
fn info_names_the_loaded_runtime() {
    let library = std::env::var_os("ORT_DYLIB_PATH")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    let Some(library) = library.filter(|library| library.is_file()) else {
        println!("skipped: ORT_DYLIB_PATH is not set to an ONNX Runtime library");
        return;
    };
    let data = tempfile::tempdir().unwrap();

    let lines = info_lines(data.path(), &[]);

    assert!(
        lines[2].starts_with("visual check: ONNX Runtime 1."),
        "{lines:?}"
    );
    let absolute = std::path::absolute(&library).unwrap();
    assert!(
        lines[2].ends_with(&format!(" at {}", absolute.display())),
        "{lines:?}"
    );
}

/// Ctrl+C while the stacks are written cancels the job: the run says so,
/// exits 1, and leaves no unfinished stack nor spill file behind.
#[cfg(unix)]
#[test]
fn ctrl_c_cancels_and_cleans_up() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;

    // Enough brackets, large enough, that the writing is still going when
    // the signal arrives after its first layer.
    const BRACKETS: u8 = 8;
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("photos");
    fs::create_dir(&input).unwrap();
    for bracket in 0..BRACKETS {
        let base = scene(u64::from(bracket) + 1, 2400, 1600);
        for (i, ev) in [-1.0, 0.0, 1.0].into_iter().enumerate() {
            let shot = i as u16 + 1;
            let exif = canon_bracket_exif(bracket * 2, shot).build();
            let bytes = jpeg_with_exif(&expose(&base, ev), &exif, JPEG_QUALITY);
            let name = format!("IMG_{:04}.jpg", u16::from(bracket) * 3 + shot);
            fs::write(input.join(name), bytes).unwrap();
        }
    }
    let output = root.path().join("out");
    let data = tempfile::tempdir().unwrap();

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_photostacker-cli"))
        .env(DATA_DIR_ENV, data.path())
        .arg("run")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = String::new();
    let mut signalled = false;
    for line in BufReader::new(child.stderr.take().unwrap()).lines() {
        let line = line.unwrap();
        if !signalled && line.starts_with("write: ") {
            let killed = std::process::Command::new("kill")
                .args(["-INT", &child.id().to_string()])
                .status()
                .unwrap();
            assert!(killed.success());
            signalled = true;
        }
        stderr.push_str(&line);
        stderr.push('\n');
    }
    let status = child.wait().unwrap();

    assert!(signalled, "no write line in {stderr}");
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("error: cancelled"), "{stderr}");
    let left = entries(&output);
    assert!(left.len() < usize::from(BRACKETS), "{left:?}");
    assert!(
        left.iter().all(|name| !name.contains(".partial")),
        "{left:?}"
    );
}

fn assert_stderr_contains(assert: &Assert, needle: &str) {
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains(needle),
        "{needle:?} not in stderr: {stderr}"
    );
}
