#!/usr/bin/env bash
# Microsoft's ONNX Runtime for this system, which Photostacker loads at run time (spec §8.3):
# the build that scripts/onnxruntime.tsv pins for it, kept in the user's cache folder.
#
# Usage (bash on Linux and macOS, Git Bash on Windows; with curl):
#   scripts/ort.sh           install the build if it is missing, then print its library's path
#   scripts/ort.sh --where   print the library's path only; nothing is downloaded or written
#
# stdout gets the path and nothing else, so callers can capture it; messages go to stderr.
# With no build pinned for this system, --where prints nothing and succeeds, and an install
# fails.
#
# Each build goes into <cache>/<version>/<archive name without .tgz or .zip>/, the folder the
# archive unpacks to. <cache> is ${XDG_CACHE_HOME:-~/.cache}/onnxruntime on Linux,
# ~/Library/Caches/onnxruntime on macOS, and %LOCALAPPDATA%\photostacker\onnxruntime on
# Windows, where scripts/win.ps1 installs the same build. On Windows the path is printed in
# Windows form, which native programs and Git Bash both take.
#
# The archive is downloaded into a temporary folder beside the build's, its SHA-256 checked,
# unpacked there, and only then is the build's folder renamed into place whole: an
# interrupted run or a wrong download leaves nothing that looks installed, and the next run
# starts again.
#
# macOS has bash 3.2: no associative arrays, no mapfile, no ${var,,}.
set -euo pipefail

fail() {
    echo "$*" >&2
    exit 1
}

where=0
case "$#:${1:-}" in
    0:) ;;
    1:--where) where=1 ;;
    *)
        echo "usage: $0 [--where]" >&2
        exit 2
        ;;
esac

table="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/onnxruntime.tsv"

# The table's names for this system.
case "$(uname -s)" in
    Linux) os=linux ;;
    Darwin) os=macos ;;
    MINGW* | MSYS*) os=windows ;;
    *) os=$(uname -s | tr '[:upper:]' '[:lower:]') ;;
esac
case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    arm64 | aarch64) arch=aarch64 ;;
    *) arch=$(uname -m) ;;
esac

# This system's row: os, arch, version, archive, sha256, library.
found=0
while IFS=$'\t' read -r row_os row_arch version archive sha256 library || [ -n "${row_os:-}" ]; do
    case "$row_os" in '' | '#'*) continue ;; esac
    if [ "$row_os" = "$os" ] && [ "$row_arch" = "$arch" ]; then
        found=1
        break
    fi
done <"$table"
if [ "$found" = 0 ]; then
    if [ "$where" = 1 ]; then
        exit 0
    fi
    fail "no ONNX Runtime build is pinned for $os $arch"
fi

case "$os" in
    linux) cache="${XDG_CACHE_HOME:-$HOME/.cache}/onnxruntime" ;;
    macos) cache="$HOME/Library/Caches/onnxruntime" ;;
    windows)
        if [ -z "${LOCALAPPDATA:-}" ]; then
            fail "LOCALAPPDATA is not set"
        fi
        local_app_data=$LOCALAPPDATA
        if command -v cygpath >/dev/null 2>&1; then
            local_app_data=$(cygpath -u "$LOCALAPPDATA")
        fi
        cache="$local_app_data/photostacker/onnxruntime"
        ;;
    *) fail "no cache folder is known for $os" ;;
esac

stem=${archive%.tgz}
stem=${stem%.zip}
dir="$cache/$version"
home="$dir/$stem"
library_file="$home/$library"

print_path() {
    if [ "$os" = windows ] && command -v cygpath >/dev/null 2>&1; then
        cygpath -w "$library_file"
    else
        printf '%s\n' "$library_file"
    fi
}

if [ "$where" = 1 ] || [ -s "$library_file" ]; then
    print_path
    exit 0
fi

url="https://github.com/microsoft/onnxruntime/releases/download/v$version/$archive"
mkdir -p "$dir"
tmp=$(mktemp -d "$dir/.extract.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
# A signal ends the script through exit, so the EXIT trap removes the temporary folder.
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

echo "Downloading ONNX Runtime $version from $url" >&2
curl --fail --location --silent --show-error --retry 3 --output "$tmp/$archive" "$url"
# Read from stdin: sha256sum escapes a file name with a backslash in it.
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum <"$tmp/$archive" | cut -d' ' -f1)
else
    actual=$(shasum -a 256 <"$tmp/$archive" | cut -d' ' -f1)
fi
if [ "$actual" != "$sha256" ]; then
    fail "the SHA-256 of $archive is $actual, expected $sha256; nothing was installed"
fi

case "$archive" in
    *.tgz) tar -xzf "$tmp/$archive" -C "$tmp" ;;
    *.zip)
        if command -v unzip >/dev/null 2>&1; then
            unzip -q "$tmp/$archive" -d "$tmp"
        else
            tar -xf "$tmp/$archive" -C "$tmp"
        fi
        ;;
    *) fail "$archive is neither a .tgz nor a .zip" ;;
esac
if [ ! -s "$tmp/$stem/$library" ]; then
    fail "$archive holds no $stem/$library; nothing was installed"
fi
# A folder without the library is no install, and mv would move into it, not replace it.
rm -rf "$home"
mv "$tmp/$stem" "$home"
echo "Installed ONNX Runtime $version in $home" >&2
print_path
