# Dev recipes. Run them at the repo root (see AGENTS.md).

# A `.env` file beside this one, if there is one, sets variables for the recipes. It is
# not committed. Its main use is CARGO_TARGET_DIR, for a checkout on a slow or shared
# drive (a Windows drive seen from WSL, a network share): build onto a local disk.
set dotenv-load

# Microsoft's ONNX Runtime, which the embedder loads at run time (spec §8.3). `just ort`
# fetches the build that scripts/onnxruntime.tsv pins for this system (Linux x64, macOS on
# Apple silicon or Intel); elsewhere set ORT_DYLIB_PATH, or put the library beside the
# executable.
# A developer's own ORT_DYLIB_PATH is kept (empty counts as unset). Without one, it is the
# library of `just ort`, whether installed yet or not, and nothing on a system with no
# pinned build. `scripts/ort.sh --where` only names the path: it downloads nothing.
export ORT_DYLIB_PATH := if env('ORT_DYLIB_PATH', '') != '' { env('ORT_DYLIB_PATH') } else { `scripts/ort.sh --where` }

# Pass recipe arguments through as "$@", so quoted filters like -E 'test(model::)' survive.
set positional-arguments

# List the recipes.
default:
    @just --list

# The quality gate before every commit: format check, lints, tests.
check: fmt-check clippy test

# Fail if any file is not rustfmt-formatted.
fmt-check:
    cargo fmt --all --check

# Lint every crate and target; warnings are errors.
clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# Run the workspace tests with nextest; extra arguments go to nextest (e.g. -E 'test(model::)').
test *ARGS:
    cargo nextest run --workspace "$@"

# Not part of `check`: it downloads the advisory database, and `check` works offline.
# Check the dependencies' licences, advisories, bans and sources against deny.toml (spec §11.2).
deny:
    cargo deny check

# scripts/licences.sh runs cargo-about (`cargo install cargo-about --locked --features cli`), which reads
# about.toml and the template in packaging/licenses. It needs the network: it fetches from GitHub the
# licence files that some crates leave out of their package (about.toml's `git` clarifications). Not
# part of `check`. scripts/package.sh runs the same script.
# Write the licences of the Rust crates the packages ship, with their texts, to dist/package/resources/licenses/THIRD-PARTY-LICENSES.md (spec §11.3).
licences:
    scripts/licences.sh

# scripts/package.sh: needs cargo-packager 0.11.8 (`cargo install cargo-packager --version 0.11.8 --locked`), cargo-about and the network. It empties dist/package and dist/packages first.
# Build this system's packages into dist/packages: deb,appimage on Linux, nsis,wix on Windows, dmg on macOS (packaging/README.md).
package FORMATS:
    scripts/package.sh "$1"

# Check built packages before anyone installs them: what they hold, what they link, and a run of the command-line tool from each; --install really installs the .deb and the NSIS installer, for a throwaway machine only (packaging/README.md).
smoke *ARGS:
    scripts/smoke.sh "$@"

# Run the desktop app in release mode; arguments go to the binary.
run *ARGS:
    cargo run --release -p photostacker-ui -- "$@"

# Run the command-line tool, photostacker-cli, in release mode; arguments go to the binary.
run-cli *ARGS:
    cargo run --release -p photostacker-cli -- "$@"

# Save a PNG of one screen of the app in its real window: <out.png> <screen> [dark|light] [width] [height].
screenshot *ARGS:
    cargo run --release -p photostacker-ui --example screenshot -- "$@"

# Write the Photoshop checkpoint set (spec §12.5) into dist/ps-check, clearing it first; extra arguments (e.g. --bigtiff) go to the example.
ps-samples *ARGS:
    cargo run --release -p photostacker-core --example ps_samples -- "{{justfile_directory()}}/dist/ps-check" "$@"

# The Stage-3 cosine calibration (spec §12.4, docs/calibration.md); set PHOTOSTACKER_MODELS_DIR; arguments go to the example.
calibrate-ml *ARGS:
    cargo run --release -p photostacker-core --example calibrate_ml -- "$@"

# scripts/ort.sh extracts it into a temporary folder and renames it into place whole, so an interrupted run leaves nothing that looks installed.
# Download Microsoft's ONNX Runtime for this system once into the user's cache folder, verified, and print its library's path; a no-op when it is there.
ort:
    @scripts/ort.sh
