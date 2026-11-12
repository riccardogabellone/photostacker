#!/usr/bin/env bash
# This system's packages of Photostacker, built with cargo-packager (packaging/README.md):
#
#   scripts/package.sh deb,appimage   Linux
#   scripts/package.sh nsis,wix       Windows, in Git Bash
#   scripts/package.sh dmg            macOS
#
# 1. Builds the desktop app and photostacker-cli (release, default features).
# 2. Stages in dist/package what a package carries besides the two executables:
#    resources/licenses/ (the eight files of spec §11.3); on Linux and Windows ONNX Runtime's
#    library in resources/, and on Windows the Visual C++ runtime beside it; on macOS the
#    library in frameworks/. The packager copies resources/ into the package's resource
#    folder at the same paths (crates/ui/Cargo.toml).
# 3. Runs `cargo packager` from crates/ui, whose Cargo.toml holds the configuration, into
#    dist/packages.
# 4. On Windows, zips the app's folder too: Photostacker_<version>_x64-portable.zip.
#
# dist/package and dist/packages are emptied first, so a package never carries a stale file
# and dist/packages holds only what this run made. scripts/smoke.sh checks the packages.
#
# Needs cargo, cargo-packager 0.11.8 and cargo-about 0.9.3 (each `cargo install <tool>
# --version <version> --locked`; cargo-about with `--features cli`), curl, and the network
# (ONNX Runtime's archive, the licence files cargo-about fetches, cargo-packager's own
# tools). Cargo builds into
# CARGO_TARGET_DIR when it is set, else into <repo>/target. On Windows also: Visual Studio
# or its Build Tools with the C++ workload (vswhere.exe finds them), VCPKG_ROOT set to the
# vcpkg root that scripts/windows-heif.ps1 prints (libheif comes from there), and PowerShell 7
# (pwsh).
#
# macOS has bash 3.2 and BSD tools: no associative arrays, no mapfile, no readlink -f.
set -euo pipefail

fail() {
    echo "package.sh: $*" >&2
    exit 1
}

if [ "$#" != 1 ] || [ -z "$1" ]; then
    echo "usage: $0 <formats>   (deb,appimage on Linux; nsis,wix on Windows; dmg on macOS)" >&2
    exit 2
fi
formats=$1

# cargo reads .cargo/config.toml from the folder it starts in: every cargo here starts in
# the repository or below it.
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo"

case "$(uname -s)" in
    Linux) os=linux exe= ;;
    Darwin) os=macos exe= ;;
    MINGW* | MSYS*) os=windows exe=.exe ;;
    *) fail "packages are built on Linux, Windows and macOS, not on $(uname -s)" ;;
esac

# A path in the form native programs take: Windows form on Windows.
native() {
    if [ "$os" = windows ]; then
        cygpath -w "$1"
    else
        printf '%s\n' "$1"
    fi
}

for format in $(printf '%s\n' "$formats" | tr ',' ' '); do
    case "$os:$format" in
        linux:deb | linux:appimage | windows:nsis | windows:wix | macos:dmg) ;;
        *) fail "$format is not a format built on $os (deb,appimage on Linux; nsis,wix on Windows; dmg on macOS)" ;;
    esac
done

# The packager's layout of the files (crates/ui/Cargo.toml) was checked against this version.
packager_version=$(cargo packager --version 2>/dev/null | tr -d '\r') ||
    fail "cargo-packager is not installed: cargo install cargo-packager --version 0.11.8 --locked"
if [ "$packager_version" != "cargo-packager 0.11.8" ]; then
    fail "this script is written for cargo-packager 0.11.8, and found ${packager_version:-no version}: cargo install cargo-packager --version 0.11.8 --locked"
fi
cargo about --version >/dev/null 2>&1 ||
    fail "cargo-about is not installed: cargo install cargo-about --version 0.9.3 --locked --features cli"
if [ "$os" = windows ] && [ -z "${VCPKG_ROOT:-}" ]; then
    fail "VCPKG_ROOT is not set: run scripts/windows-heif.ps1 and set VCPKG_ROOT to the folder it prints on its last line"
fi
if [ "$os" = windows ] && ! command -v pwsh >/dev/null 2>&1; then
    fail "pwsh (PowerShell 7) is not on the PATH: it makes the portable zip; Windows PowerShell 5.1 would write entry names with backslashes"
fi

# One absolute build folder for cargo and the packager alike: the packager runs in
# crates/ui, where a relative CARGO_TARGET_DIR would name another folder.
target_dir=${CARGO_TARGET_DIR:-$repo/target}
if [ "$os" = windows ]; then
    target_dir=$(cygpath -u "$target_dir")
fi
case "$target_dir" in
    /*) ;;
    *) target_dir="$repo/$target_dir" ;;
esac
CARGO_TARGET_DIR=$(native "$target_dir")
export CARGO_TARGET_DIR
release="$target_dir/release"

stage="$repo/dist/package"
out="$repo/dist/packages"
rm -rf "$stage" "$out"
mkdir -p "$stage/resources/licenses" "$out"
licenses="$stage/resources/licenses"

# The Visual C++ runtime that ONNX Runtime and the app need, from the redistributable folder
# of the newest Visual Studio installation: the highest version there that has all four
# files, copied into $1.
vc_runtime_files="msvcp140.dll msvcp140_1.dll vcruntime140.dll vcruntime140_1.dll"
copy_vc_runtime() {
    local dest=$1 program_files vswhere vs redist msvc crt file complete found=
    program_files=$(printenv 'ProgramFiles(x86)' || true)
    vswhere="$(cygpath -u "${program_files:-C:\\Program Files (x86)}")/Microsoft Visual Studio/Installer/vswhere.exe"
    if [ ! -f "$vswhere" ]; then
        fail "vswhere.exe is not at $vswhere: install Visual Studio or its Build Tools, with the C++ workload"
    fi
    # Among several installations, the newest with the C++ tools, which install the
    # redistributable folder.
    vs=$("$vswhere" -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 \
        -property installationPath | tr -d '\r')
    if [ -z "$vs" ]; then
        fail "vswhere.exe found no Visual Studio installation with the C++ tools: install the C++ workload of Visual Studio or its Build Tools"
    fi
    redist="$(cygpath -u "$vs")/VC/Redist/MSVC"
    # Version folders (14.44.35112) and others (v143); sort -V puts the highest first.
    for msvc in $(ls -1 "$redist" 2>/dev/null | sort -rV); do
        for crt in "$redist/$msvc"/x64/Microsoft.VC*.CRT; do
            [ -d "$crt" ] || continue
            complete=1
            for file in $vc_runtime_files; do
                [ -f "$crt/$file" ] || complete=0
            done
            if [ "$complete" = 1 ]; then
                found=$crt
                break 2
            fi
        done
    done
    if [ -z "$found" ]; then
        fail "no version in $redist has all of $vc_runtime_files in x64/Microsoft.VC*.CRT: install the C++ workload of Visual Studio"
    fi
    for file in $vc_runtime_files; do
        cp "$found/$file" "$dest/$file"
    done
    echo "Visual C++ runtime: $found" >&2
}

echo "Building Photostacker and photostacker-cli into $target_dir" >&2
cargo build --release --locked -p photostacker-ui -p photostacker-cli

# The version in the packages' names, as the built tool states it.
version_line=$("$release/photostacker-cli$exe" --version | tr -d '\r')
version=${version_line#photostacker-cli }
case "$version_line:$version" in
    "photostacker-cli "*:?*) ;;
    *) fail "photostacker-cli --version printed \"$version_line\", not \"photostacker-cli <version>\"" ;;
esac
case "$version" in
    *[!0-9A-Za-z.+-]*) fail "photostacker-cli --version printed \"$version_line\", not \"photostacker-cli <version>\"" ;;
esac
echo "Version $version" >&2

echo "Staging $stage" >&2
cp LICENSE THIRD-PARTY-NOTICES.md "$licenses/"
cp packaging/licenses/LGPL-2.1.txt packaging/licenses/LGPL-3.0.txt packaging/licenses/GPL-3.0.txt "$licenses/"
scripts/licences.sh

# ONNX Runtime: this system's pinned build (scripts/onnxruntime.tsv), installed if missing.
# Its library may be a link to a versioned file: the package gets the file itself, under
# the plain name the app looks for. The archive's root, two folders up, holds its licence
# files.
library=$(scripts/ort.sh)
if [ "$os" = windows ]; then
    library=$(cygpath -u "$library")
fi
ort_home=$(dirname "$(dirname "$library")")
cp "$ort_home/LICENSE" "$licenses/onnxruntime-LICENSE.txt"
cp "$ort_home/ThirdPartyNotices.txt" "$licenses/onnxruntime-ThirdPartyNotices.txt"
case "$os" in
    linux) cp -L "$library" "$stage/resources/libonnxruntime.so" ;;
    windows)
        cp -L "$library" "$stage/resources/onnxruntime.dll"
        copy_vc_runtime "$stage/resources"
        ;;
    macos)
        mkdir -p "$stage/frameworks"
        cp -L "$library" "$stage/frameworks/libonnxruntime.dylib"
        ;;
esac

echo "Packaging $formats into $out" >&2
# --binaries-dir names the folder that cargo-packager 0.11.8 also finds by itself, from
# `cargo metadata` and CARGO_TARGET_DIR.
(cd crates/ui && cargo packager --release --formats "$formats" \
    --binaries-dir "$(native "$release")" --out-dir "$(native "$out")")

if [ "$os" = windows ]; then
    # The portable zip: the contents of the app's folder, as the installer fills it, at
    # the zip's root, with no folder around them. Git Bash has no zip; PowerShell 7's
    # Compress-Archive makes it (Windows PowerShell 5.1's writes entry names with
    # backslashes, which other systems' unzip tools misread).
    portable="$stage/portable"
    mkdir -p "$portable"
    cp "$release/Photostacker.exe" "$release/photostacker-cli.exe" "$portable/"
    cp -R "$stage/resources/." "$portable/"
    zip="$out/Photostacker_${version}_x64-portable.zip"
    # Single quotes in a PowerShell string are doubled.
    from=$(native "$portable" | sed "s/'/''/g")
    to=$(native "$zip" | sed "s/'/''/g")
    MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*' pwsh -NoProfile -NonInteractive -Command \
        "\$ErrorActionPreference = 'Stop'; Compress-Archive -Path '$from\\*' -DestinationPath '$to'"
fi

# The packager's intermediate files; the packages stay.
rm -rf "$out/.cargo-packager"
echo "Packages:" >&2
for package in "$out"/*; do
    printf '%s\n' "$package"
done
