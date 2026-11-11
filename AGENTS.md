# AGENTS.md

Instructions for AI coding agents working on this repository. Humans should start with [README.md](README.md).

## Project overview

Photostacker finds series of geometrically identical photos in a folder (exposure brackets, HDR sets, fast bursts) and stacks each series as layers in one PSD/PSB or layered TIFF. It is an offline, MIT-licensed desktop app and command-line tool, written in Rust.

The design specification is [docs/design.md](docs/design.md). The code and the tests cite its sections as `spec §N`. Read the section before changing the behaviour it describes, and change the two together.

- `crates/core/`: the `photostacker-core` library: discovery, EXIF, decoding, hashing, grouping, the store, the engine, the models and the writers. Everything that is not drawing or argv.
- `crates/cli/`: the command-line tool: binary and crate `photostacker-cli`.
- `crates/ui/`: the desktop app, binary `Photostacker` (`photostacker-ui` crate, egui/eframe), over the core's engine on a thread of its own. Read [crates/ui/DESIGN.md](crates/ui/DESIGN.md) before changing the UI.
  - Its tests: `tests/controller.rs` (no window), `tests/interaction.rs` (`egui_kittest`: the real widgets over a real engine, end-to-end runs included), `tests/thumbs.rs`. `tests/common/` is what they share, and `tests/screens/` the screens of the screenshot example, which `interaction.rs` draws too.
- `crates/testkit/`: dev-only fixtures (synthetic EXIF/TIFF bytes and scenes); a dev-dependency, never published.
- `assets/`: the font, the icons, the logo's sources ([assets/logo/AGENTS.md](assets/logo/AGENTS.md)) and the README's screenshot.
- `packaging/linux/`: the desktop entry, named after the app id.
- `scripts/`: the Windows build ([scripts/win.ps1](scripts/win.ps1)), the pinned ONNX Runtime builds and the script that fetches them ([scripts/ort.sh](scripts/ort.sh)), the stand-in readers ([scripts/standins/](scripts/standins/)), and the scripts that fetch the public camera files and generate the tests' golden files.
- `docs/`: the specification, the [history](docs/history.md), the [calibration](docs/calibration.md) of the thresholds, the [checks in other programs](docs/photoshop-checks.md) and the [EXIF coverage](docs/exif-fixture-coverage.md).

The app's data (the job database, the models, the logs) lives in the platform's data folder, and its thumbnails in the cache folder (spec §7.1). `PHOTOSTACKER_DATA_DIR` moves both, for the app and the CLI alike: set it to a scratch folder for every run you start while developing, so a real user's data is never touched.

## Environment

- **Toolchain:** stable Rust, 1.99 or later (`rust-version` in [Cargo.toml](Cargo.toml)), [just](https://github.com/casey/just) and [cargo-nextest](https://nexte.st).
- **Linux packages:** `sudo apt install build-essential cmake pkg-config nasm libde265-dev libegl1 zenity`.
  - `zenity` gives the app its file dialogs where there is no XDG desktop portal; without either, the app says that no file dialog is available.
  - `libimage-exiftool-perl` is needed only to regenerate the EXIF golden file. ExifTool is never a build or run-time dependency.
- **Windows:** `pwsh scripts/win.ps1` builds the desktop app with the MSVC toolchain (which must be installed, with Visual Studio's "Desktop development with C++" workload and the Windows SDK) into `dist/windows/`, with ONNX Runtime beside it. It builds without HEIC for now. The script stops, before cargo runs, when the app it would replace is running, or the toolchain or the SDK is missing. The tests have only been run on Linux so far.
- **macOS:** nothing has been built there yet.
- **A checkout on a slow or shared drive** (a Windows drive seen from WSL, a network share): put `CARGO_TARGET_DIR=<a folder on the local disk>` in a `.env` file beside the `justfile`. The recipes load it, and it is not committed. Never let two systems build into one target folder.
- **Line endings** are LF everywhere ([.gitattributes](.gitattributes)): the `justfile` and the shell scripts break on CRLF.
- **ONNX Runtime** (core feature `ml`, on by default) is not linked at build time: the embedder loads Microsoft's official shared library at run time, from `ORT_DYLIB_PATH`, or else from beside the executable or the package's library folder (`../lib/Photostacker` on Linux, `../Frameworks` in a macOS app; spec §8.3). The version and SHA-256 of each system's build are pinned in one table, [scripts/onnxruntime.tsv](scripts/onnxruntime.tsv), which every script that fetches one reads: 1.31.1, and 1.19.2 on Intel macOS. On Linux x64 and macOS, run `just ort` once: it calls `scripts/ort.sh`, which downloads and verifies the library into the user's cache folder, and the `justfile` points `ORT_DYLIB_PATH` at it unless you set your own. Without the library, the tests that run a model print `skipped: ONNX Runtime not found at …` and pass. `cargo … --no-default-features` builds without `ml`; the CLI's `models download` and `models import` then exit 2 with "built without ml".
- **HEIC** (core feature `heic`, on by default): `libheif-sys` builds libheif with cmake but links its codecs from the system, enabling every one cmake finds. libde265 is the HEVC decoder. Never install `libx265-dev` or `libx264-dev` on a build host: libheif would link those GPL encoders, and a test fails if it does. Without the feature, HEIC files are reported as "built without heic".

## Setup and commands

Run these at the repo root. `<scratch>` is any empty folder outside the repository.

```sh
just check                    # the quality gate: fmt-check, clippy, test
just fmt-check                # cargo fmt --all --check
just clippy                   # cargo clippy --workspace --all-targets -- -D warnings
just test                     # cargo nextest run --workspace
just test -E 'test(model::)'  # extra arguments go to nextest
just test --no-default-features  # the same tests without ml and heic

PHOTOSTACKER_DATA_DIR=<scratch> just run          # the desktop app, release build
just run-cli --help                                # the CLI, release build; arguments go to the binary
just run-cli run <input> -o <scratch>/stacks       # group a folder and write its stacks; stdout gets the paths written
just run-cli run <input> --dry-run --plan <scratch>/plan.json  # group only; without --plan the plan goes to stdout
just run-cli run <input> --db <scratch>/jobs.db --no-ml        # keep the job in a database; no visual check
PHOTOSTACKER_DATA_DIR=<scratch> just run-cli models download dinov2-small  # also list, import, remove
PHOTOSTACKER_DATA_DIR=<scratch> just run-cli info  # the HEIC decoder, the ONNX Runtime loaded and its version, the data folder

just ort                      # once, on Linux x64 and macOS: the ONNX Runtime pinned for the system, which the model tests need
just screenshot <out.png> <screen> [dark|light] [width] [height]  # one screen of the real window; an unknown screen lists them all
just screenshot assets/screenshots/ui.png review dark              # the README's screenshot, on synthetic photos only
just ps-samples --bigtiff     # the sample set for the checks in other programs, into dist/ps-check (docs/photoshop-checks.md)
just calibrate-ml dinov2-small <folder of bracket sets>  # the visual check's cosines (docs/calibration.md); set PHOTOSTACKER_MODELS_DIR
```

On Windows:

```powershell
pwsh scripts/win.ps1          # the desktop app, built natively into dist/windows
node scripts/standins/standins.mjs --photocraft <folder>/photocraft-cli.exe  # the samples in GIMP, PhotoCraft and Photopea
```

## Code style

- Use clear and descriptive names.
- Write comments to explain complex logic, not to restate the code.
- Keep functions small and focused on a single task.
- Every job parameter lives in `JobConfig` and is validated there, once, by `JobConfig::validate()`; the rest of the code trusts a validated config.
- The core never depends on the UI: `photostacker-core` must not depend on egui or clap.
- After start-up, the UI thread never calls the engine and does no file I/O: both go through the backend thread.
- Record each UI design choice in `crates/ui/DESIGN.md`. Colours come from `theme::Palette` only, and every interactive widget names itself (`WidgetInfo`).
- The EXIF and MakerNote reader is the project's own: do not add exiftool, exiftool-rs, kamadak-exif or nom-exif.
- Licence: MIT. No GPL dependency. LGPL is allowed only for `rawler` (2.1) and libheif/libde265 (3.0) through `libheif-rs`.
- Code ported from another project keeps its notice: a section with the original licence in [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md), and a header in the file that holds the port.
- Code, comments, docs and commit messages are in English.

## Testing

- Every rule in the specification has a scripted scenario test: pipeline rules in `crates/core/tests/scenarios.rs`, writer rules in `crates/core/tests/write_scenarios.rs`. Add one when you add or change a rule.
- Build fixtures with `photostacker-testkit` instead of committing large binary files.
- The hashes match Python's `imagehash`: `crates/core/tests/hash_parity.rs` checks them against golden files that `scripts/gen_hash_golden.py` generates (`uv run scripts/gen_hash_golden.py crates/core/tests/fixtures/hash`). Regenerating must leave the committed fixtures unchanged.
- The EXIF reader is checked against ExifTool's output on real camera files, which stay outside the repository ([docs/exif-fixture-coverage.md](docs/exif-fixture-coverage.md)).
- After any change to a writer, make the sample set again (`just ps-samples`) and run the stand-in readers ([docs/photoshop-checks.md](docs/photoshop-checks.md)).
- Nothing private goes into the repository: no photo of a person's own, no file name or folder name of theirs, no path of a machine. Docs and examples use placeholders (`<repo>`, `<input>`, `<scratch>`), and work the same on Linux, macOS and Windows.

## Commits, tags and releases

- Work on a branch and open a pull request against `main`.
- One change per commit and per pull request.
- The subject line is a conventional commit (`feat(core): …`, `fix(ui): …`, `docs: …`). The body is a short list of bullets: what changed and why, as few as tell the story. No trailers: no `Co-Authored-By`, no tool or session lines.
- Run `just check` before every commit. Update `docs/design.md` and the README when behaviour changes, and `crates/ui/DESIGN.md` when the UI does.
- The version is the workspace's, in [Cargo.toml](Cargo.toml). A release is a tag `vX.Y.Z` on `main`. The CI and release workflows are not written yet.
