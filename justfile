# Dev recipes. Run them at the repo root (see AGENTS.md).

# A `.env` file beside this one, if there is one, sets variables for the recipes. It is
# not committed. Its main use is CARGO_TARGET_DIR, for a checkout on a slow or shared
# drive (a Windows drive seen from WSL, a network share): build onto a local disk.
set dotenv-load

# Microsoft's ONNX Runtime, which the embedder loads at run time (spec §8.3). `just ort`
# fetches the Linux x64 build; elsewhere set ORT_DYLIB_PATH, or put the library beside
# the executable.
ort_version := "1.31.1"
ort_dir := cache_directory() / "onnxruntime" / ort_version
ort_sha256 := "c849e36a1ef3e22d4ff217b9d1ce4a3cf5b9467b975610913ae959ad5b16bdc9"
ort_folder := "onnxruntime-linux-x64-" + ort_version
ort_library := ort_dir / ort_folder / "lib/libonnxruntime.so"
# A developer's own ORT_DYLIB_PATH is kept. Without one, Linux gets the library of
# `just ort`, and other systems none (empty counts as unset).
export ORT_DYLIB_PATH := env_var_or_default('ORT_DYLIB_PATH', if os() == "linux" { ort_library } else { "" })

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

# Download Microsoft's ONNX Runtime (Linux x64) once into the user's cache folder, verified; a no-op when it is there.
# It is extracted into a temporary folder and renamed into place whole, so an interrupted run leaves nothing that looks installed.
ort:
    #!/usr/bin/env bash
    set -euo pipefail
    if test -s "{{ort_library}}"; then
        echo "ONNX Runtime {{ort_version}} is at {{ort_library}}"
        exit 0
    fi
    mkdir -p "{{ort_dir}}"
    tmp=$(mktemp -d "{{ort_dir}}/.extract.XXXXXX")
    trap 'rm -rf "$tmp"' EXIT
    archive="$tmp/{{ort_folder}}.tgz"
    curl --fail --location --silent --show-error --retry 3 -o "$archive" \
        "https://github.com/microsoft/onnxruntime/releases/download/v{{ort_version}}/{{ort_folder}}.tgz"
    echo "{{ort_sha256}}  $archive" | sha256sum --check --quiet
    tar -xzf "$archive" -C "$tmp"
    test -s "$tmp/{{ort_folder}}/lib/libonnxruntime.so"
    # A folder without the library is no install: mv would nest into it.
    rm -rf "{{ort_dir}}/{{ort_folder}}"
    mv "$tmp/{{ort_folder}}" "{{ort_dir}}/{{ort_folder}}"
    echo "ONNX Runtime {{ort_version}} is at {{ort_library}}"
