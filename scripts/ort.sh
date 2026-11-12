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
# starts again. Two runs at once each download a copy, and put it in place one at a time,
# under the lock folder <cache>/<version>/.install.lock: one that finds the other has
# installed the build keeps that install and drops its own copy.
#
# A .zip (the Windows build) is unpacked with unzip, which Git for Windows has.
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

# CDPATH is cleared: with it exported, cd prints the folder it changes to, which would end
# up in the path.
table="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/onnxruntime.tsv"

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

case "$archive" in
    *.tgz) ;;
    *.zip)
        if ! command -v unzip >/dev/null 2>&1; then
            fail "unzip is not on the PATH, and it is needed to unpack $archive: install it, then run this script again"
        fi
        ;;
    *) fail "$archive is neither a .tgz nor a .zip" ;;
esac

url="https://github.com/microsoft/onnxruntime/releases/download/v$version/$archive"
mkdir -p "$dir"
tmp=$(mktemp -d "$dir/.extract.XXXXXX")
# The seconds a run waits for another to finish putting a build in place (below).
lock_wait=60
locked=0
trap 'rm -rf "$tmp"; if [ "$locked" = 1 ]; then rmdir "$lock"; fi' EXIT
# A signal ends the script through exit, so the EXIT trap removes the temporary folder and
# the lock.
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
    *.zip) unzip -q "$tmp/$archive" -d "$tmp" ;;
esac
if [ ! -s "$tmp/$stem/$library" ]; then
    fail "$archive holds no $stem/$library; nothing was installed"
fi
# The check, the removal and the move are done by one run at a time: a run holds the lock
# folder (mkdir either makes it or fails, atomically) until it exits. A run that waits too
# long never removes the lock, which may be a live run's; it takes another run's install
# if there is one, and otherwise says how to clear a lock left by a run that was killed.
lock="$dir/.install.lock"
waited=0
while ! mkdir "$lock" 2>/dev/null; do
    if [ "$waited" -ge "$lock_wait" ]; then
        if [ -s "$library_file" ]; then
            echo "ONNX Runtime $version is installed in $home, and another run still holds $lock; that install is kept" >&2
            print_path
            exit 0
        fi
        fail "another run has held $lock for $lock_wait seconds, and ONNX Runtime $version is not installed in $home: if no other run of this script is going, delete $lock and run this script again"
    fi
    sleep 1
    waited=$((waited + 1))
done
locked=1
# Another run may have installed the build while this one downloaded it, and may already
# have printed its path: that install is kept, not replaced.
if [ -s "$library_file" ]; then
    echo "ONNX Runtime $version was installed in $home by another run meanwhile; that install is kept" >&2
    print_path
    exit 0
fi
# A folder without the library is no install (nothing loads from it), and mv would move
# into it, not replace it.
rm -rf "$home"
mv "$tmp/$stem" "$home"
echo "Installed ONNX Runtime $version in $home" >&2
print_path
