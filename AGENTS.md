# AGENTS.md

Instructions for AI coding agents working on this repository. Humans should start with [README.md](README.md).

## Project overview

Photostacker finds series of geometrically identical photos in a folder (exposure brackets, HDR sets, fast bursts) and stacks each series as layers in one PSD/PSB or layered TIFF. It is an offline, MIT-licensed desktop app and command-line tool, written in Rust.

The design specification is [docs/design.md](docs/design.md). The code and the tests cite its sections as `spec §N`. Read the section before changing the behaviour it describes, and change the two together.

- `crates/core/`: the `photostacker-core` library: discovery, EXIF, decoding, hashing, grouping, the store, the engine, the models and the writers. Everything that is not drawing or argv.
- `crates/cli/`: the command-line tool, called the CLI below: binary and crate `photostacker-cli`.
- `crates/ui/`: the desktop app, binary `Photostacker` (`photostacker-ui` crate, egui/eframe), over the core's engine on a thread of its own. Read [crates/ui/DESIGN.md](crates/ui/DESIGN.md) before changing the UI.
  - Its tests: `tests/controller.rs` (no window), `tests/interaction.rs` (`egui_kittest`: the real widgets over a real engine, end-to-end runs included), `tests/thumbs.rs`. `tests/common/` is what they share, and `tests/screens/` the screens of the screenshot example, which `interaction.rs` draws too.
- `crates/testkit/`: dev-only fixtures (synthetic EXIF/TIFF bytes and scenes); a dev-dependency, never published.
- `assets/`: the font, the icons, the logo's sources ([assets/logo/AGENTS.md](assets/logo/AGENTS.md)) and the README's screenshot.
- `packaging/`: how the packages are built and checked ([packaging/README.md](packaging/README.md)); the packager's configuration is in `crates/ui/Cargo.toml`.
- `packaging/cmake/`: the cmake toolchain file of the libheif build ([packaging/cmake/libheif.cmake](packaging/cmake/libheif.cmake)), which keeps libde265 its only codec (spec §11.1).
- `packaging/linux/`: the desktop entry and the AppStream file, named after the app id.
- `packaging/licenses/`: the Free Software Foundation's texts of the LGPL-2.1, LGPL-3.0 and GPL-3.0, unchanged, which every package's `licenses` folder holds, and the template of `THIRD-PARTY-LICENSES.md` (spec §11.3).
- `scripts/`: the Windows build ([scripts/win.ps1](scripts/win.ps1)) and its vcpkg for HEIC ([scripts/windows-heif.ps1](scripts/windows-heif.ps1)), the macOS build's libde265 ([scripts/libde265.sh](scripts/libde265.sh)) and the check of a static library's minimum macOS ([scripts/macos-min.sh](scripts/macos-min.sh)), the pinned ONNX Runtime builds and the script that fetches them ([scripts/ort.sh](scripts/ort.sh)), the packages' build, licences and check ([scripts/package.sh](scripts/package.sh), [scripts/licences.sh](scripts/licences.sh), [scripts/smoke.sh](scripts/smoke.sh)), the stand-in readers ([scripts/standins/](scripts/standins/)), and the scripts that fetch the public camera files and generate the tests' golden files.
- `.github/`: the CI and release workflows ([.github/workflows/](.github/workflows/)), the build inputs they share ([.github/actions/build-inputs/](.github/actions/build-inputs/action.yml)) and Dependabot's configuration for the actions (spec §11.2, §11.3).
- `docs/`: the specification, the [history](docs/history.md), the [calibration](docs/calibration.md) of the thresholds, the [checks in other programs](docs/photoshop-checks.md) and the [EXIF coverage](docs/exif-fixture-coverage.md).

The app's data (the job database, the models, the logs) lives in the platform's data folder, and its thumbnails in the cache folder (spec §7.1). `PHOTOSTACKER_DATA_DIR` moves both, for the app and the CLI alike: set it to a scratch folder for every run you start while developing, so a real user's data is never touched.

## Environment

- **Toolchain:** stable Rust, 1.99 or later (`rust-version` in [Cargo.toml](Cargo.toml)), [just](https://github.com/casey/just) 1.15 or later (the `justfile` uses `env()`), [cargo-nextest](https://nexte.st), [cargo-deny](https://github.com/EmbarkStudios/cargo-deny) (`cargo install cargo-deny --locked`) and, to make the licences file of the packages, [cargo-about](https://github.com/EmbarkStudios/cargo-about) 0.9.3 (`cargo install cargo-about --version 0.9.3 --locked --features cli`); to build the packages, [cargo-packager](https://github.com/crabnebula-dev/cargo-packager) 0.11.8 (`cargo install cargo-packager --version 0.11.8 --locked`).
- **Linux packages:** `sudo apt install build-essential cmake pkg-config libde265-dev libegl1 zenity`.
  - `zenity` gives the app its file dialogs where there is no XDG desktop portal; without either, the app says that no file dialog is available.
  - `libimage-exiftool-perl` is needed only to regenerate the EXIF golden file. ExifTool is never a build or run-time dependency.
- **Windows:** `pwsh scripts/win.ps1` builds the desktop app and `photostacker-cli` with the MSVC toolchain (which must be installed, with Visual Studio's "Desktop development with C++" workload and the Windows SDK) into `dist/windows/`, with ONNX Runtime beside them. HEIC is built in: the script first runs [scripts/windows-heif.ps1](scripts/windows-heif.ps1), which builds libheif and libde265 in a vcpkg checkout of its own, at a pinned commit, in `%LOCALAPPDATA%\photostacker\vcpkg`. The first run needs git and the network, and vcpkg's build takes a few minutes (about three on a recent laptop); later runs reuse it. `pwsh scripts/win.ps1 -Test [nextest arguments]` runs the tests natively, as `just test` does (it needs `cargo +stable-x86_64-pc-windows-msvc install cargo-nextest --locked`), and `pwsh scripts/win.ps1 -Screenshot <out.png> <screen> [dark|light] [width] [height]` is `just screenshot`; neither touches `dist/windows/`. The script stops, before cargo runs, when an executable it would replace is running, or the toolchain, the SDK or (for `-Test`) cargo-nextest is missing. The tests pass on Windows as on Linux; the few that need what Windows lacks say so where they are.
- **macOS:** the build has not been run on a Mac yet; these steps follow what the workflows do (spec §11.1).
  - Xcode's command line tools (`xcode-select --install`: the compilers and `otool`), cmake and pkg-config (with Homebrew: `brew install cmake pkgconf`).
  - macOS has no system libde265, so build a static one once: `scripts/libde265.sh <prefix>`, with `<prefix>` any folder outside the repository. It downloads the pinned libde265, checks its SHA-256, builds it for `MACOSX_DEPLOYMENT_TARGET` (11.0 when unset), and checks with `scripts/macos-min.sh` that every object in it runs there. A second run with the library in place does nothing.
  - Then set, for every cargo and `just` command, as the workflows' build inputs do: `CMAKE_PREFIX_PATH=<prefix>` and `PKG_CONFIG_LIBDIR=<prefix>/lib/pkgconfig`, with `<prefix>` as an absolute path (the recipes also read them from `.env`, below). libheif's build finds libde265 there, and only there, and links it statically.
  - `just ort` fetches ONNX Runtime (below). [.cargo/config.toml](.cargo/config.toml) sets `MACOSX_DEPLOYMENT_TARGET` to 11.0, the oldest macOS the app runs on, for rustc, the `cc` crate and cmake, unless the environment sets it.
- **A checkout on a slow or shared drive** (a Windows drive seen from WSL, a network share): put `CARGO_TARGET_DIR=<a folder on the local disk>` in a `.env` file beside the `justfile`. The recipes load it, and it is not committed. Never let two systems build into one target folder.
- **Line endings** are LF everywhere ([.gitattributes](.gitattributes)): the `justfile` and the shell scripts break on CRLF.
- **ONNX Runtime** (core feature `ml`, on by default) is not linked at build time: the embedder loads Microsoft's official shared library at run time, from `ORT_DYLIB_PATH`, or else from beside the executable or the package's library folder (`../lib/Photostacker` on Linux, `../Frameworks` in a macOS app; spec §8.3). The version and SHA-256 of each system's build are pinned in one table, [scripts/onnxruntime.tsv](scripts/onnxruntime.tsv), which every script that fetches one reads: 1.31.1, and 1.19.2 on Intel macOS. On Linux x64 and macOS, run `just ort` once: it calls `scripts/ort.sh`, which downloads and verifies the library into the user's cache folder, and the `justfile` points `ORT_DYLIB_PATH` at it unless you set your own. Without the library, the tests that run a model print `skipped: ONNX Runtime not found at …` and pass. `cargo … --no-default-features` builds without `ml`; the CLI's `models download` and `models import` then exit 2 with "built without ml".
- **HEIC** (core feature `heic`, on by default): on Linux and macOS `libheif-sys` builds libheif with cmake (on Windows it takes libheif from vcpkg: see Windows above), which on its own would link every codec library it finds on the build host, the GPL encoders x265 and x264 among them. [.cargo/config.toml](.cargo/config.toml) sets `CMAKE_TOOLCHAIN_FILE` to [packaging/cmake/libheif.cmake](packaging/cmake/libheif.cmake), which hides all of them but libde265, the HEVC decoder: the system's on Linux, the static build of `scripts/libde265.sh` on macOS. The other codec libraries a host has installed are ignored. A `CMAKE_TOOLCHAIN_FILE` already in the environment takes its place. The test `libde265_is_the_only_codec` fails if libheif has any other codec; the file says how to bring its list up to date when libheif is updated. Without the feature, HEIC files are reported as "built without heic".

## Setup and commands

Run these at the repo root. `<scratch>` is any empty folder outside the repository.

```sh
just                          # list the recipes
just check                    # the quality gate: fmt-check, clippy, test
just fmt-check                # cargo fmt --all --check
just clippy                   # cargo clippy --workspace --all-targets -- -D warnings
just test                     # cargo nextest run --workspace
just test -E 'test(model::)'  # extra arguments go to nextest
just test --no-default-features  # the same tests without ml and heic
just deny                     # cargo deny check: licences, advisories, bans and sources (deny.toml); needs the network
just licences                 # the crates' licences, with their texts (about.toml), into dist/package/resources/licenses/THIRD-PARTY-LICENSES.md; needs the network
just package deb,appimage     # this system's packages into dist/packages (dmg on macOS; scripts/package.sh nsis,wix in Git Bash on Windows); packaging/README.md
just smoke dist/packages/*.deb dist/packages/*.AppImage  # check built packages: contents, links, a run of the CLI from each; installs nothing
# just smoke --install …      really installs the .deb and the NSIS installer: only on a throwaway machine (a CI runner)

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
pwsh scripts/win.ps1          # the desktop app and the CLI, built natively into dist/windows
pwsh scripts/win.ps1 -Test    # the tests; extra arguments go to nextest (e.g. -E 'test(heic)')
pwsh scripts/win.ps1 -Screenshot <out.png> <screen> [dark|light]  # as just screenshot
node scripts/standins/standins.mjs --photocraft <folder>/photocraft-cli.exe  # the samples in GIMP, PhotoCraft and Photopea
```

## Code style

- Use clear and descriptive names.
- Write comments to explain complex logic, not to restate the code.
- Keep functions small and focused on a single task.
- Every job parameter lives in `JobConfig` and is validated there, once, by `JobConfig::validate()`; the rest of the code trusts a validated config.
- The core never depends on the UI: `photostacker-core` must not depend on egui or clap.
- After start-up, the UI thread never calls the engine, and the app's code on it reads no file and lists no folder: the engine's work goes through the backend thread, other file reads through threads of their own. The rule's two exceptions (a dropped path's one folder check, and egui's own lookup of a system font) are in `crates/ui/DESIGN.md`, "State and the engine"; new code may not add a third.
- Record each UI design choice in `crates/ui/DESIGN.md`. Colours come from `theme::Palette` only, and every interactive widget names itself (`WidgetInfo`).
- The EXIF and MakerNote reader is the project's own: do not add exiftool, exiftool-rs, kamadak-exif or nom-exif.
- Licence: MIT. No GPL dependency. LGPL is allowed only for `rawler` (2.1) and libheif/libde265 (3.0) through `libheif-rs`. `just deny` checks the licences, the advisories, the banned crates and the sources against [deny.toml](deny.toml): run it when a dependency is added or updated.
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
- The version is the workspace's: `version` in `[workspace.package]` of [Cargo.toml](Cargo.toml).
- The checks of a pull request, by job name (spec §11.2):
  - [.github/workflows/ci.yml](.github/workflows/ci.yml), on every pull request and every push to `main`:
    - `Format, lints and licences`: `cargo fmt --all --check` and `cargo deny check`, on Ubuntu.
    - `Tests (ubuntu-latest)`, `Tests (windows-latest)`, `Tests (macos-15)`, `Tests (macos-15-intel)`: clippy with warnings as errors, `cargo nextest run --workspace --locked` (on Linux also with `--no-default-features`), a release build of both executables, and on macOS `scripts/macos-min.sh 11.0` on libheif's objects. A step before the tests fails the job when ONNX Runtime or dinov2-small is missing or the runner has fewer than three cores, so the tests that need them never skip there.
  - [.github/workflows/release.yml](.github/workflows/release.yml), on a pull request that changes the packaging (that workflow, `.github/actions/`, `packaging/`, `assets/icons/`, `crates/ui/Cargo.toml`, `crates/ui/build.rs`, `scripts/`, `Cargo.lock` or `.cargo/`): `Package (ubuntu-22.04)`, `Package (windows-latest)`, `Package (macos-15)`, `Package (macos-15-intel)` build and check that system's packages, with no release.
- A release (spec §11.3, [packaging/README.md](packaging/README.md)):
  1. Bump `version` in `[workspace.package]` of the root `Cargo.toml` in a pull request, and merge it.
  2. Tag the merge commit on `main` as `vX.Y.Z`, the new version, and push the tag.
  3. The release workflow checks that the tag is `v` and the workspace's version, builds the packages on four runners (Ubuntu 22.04, Windows, macOS on Apple silicon and on Intel), checks each with `scripts/smoke.sh --install`, attests the build provenance of every file (kept in GitHub's attestation store, not in the release), and creates a draft GitHub release with the packages and `checksums.txt` (their SHA-256).
  4. Download the packages from the draft and try them on each system. Photoshop opens the sample set before each release (spec §12.5, [docs/photoshop-checks.md](docs/photoshop-checks.md)).
  5. Publish the draft.
