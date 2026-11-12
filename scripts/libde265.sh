#!/usr/bin/env bash
# libde265, the HEVC decoder that libheif links, built as a static library for the macOS
# build (spec §3, §11.1):
#
#   scripts/libde265.sh <prefix>
#
# On Linux the build takes the system's libde265 (libde265-dev), and on Windows vcpkg's
# (scripts/windows-heif.ps1). macOS has no system libde265, and Homebrew's is built for the
# macOS it runs on, not for the oldest one the app supports. So this script downloads the
# release pinned below from libde265's GitHub releases, checks its SHA-256, and builds it
# with cmake: static, position-independent, a release build, with no tools (no dec265, no
# encoder, no sherlock265, no development tools, no fuzzers) and no SDL; on macOS for
# MACOSX_DEPLOYMENT_TARGET, 11.0 when it is unset. It installs into <prefix>: the library in
# <prefix>/lib/libde265.a, the headers in <prefix>/include/libde265, the pkg-config file in
# <prefix>/lib/pkgconfig/libde265.pc. Then point the build at it:
#
#   CMAKE_PREFIX_PATH=<prefix> PKG_CONFIG_LIBDIR=<prefix>/lib/pkgconfig cargo build ...
#
# libheif's cmake build (through libheif-sys) finds it there, and links it statically, as
# there is no shared one beside it.
#
# A run that finds libde265.a and libde265.pc of this version in <prefix> does nothing.
# cmake installs the .pc file last, so an interrupted install has none, and the next run
# builds again. A <prefix> that holds another version is refused.
#
# On macOS it then checks its own work: every object in libde265.a runs on the deployment
# target (scripts/macos-min.sh), and the .pc file names no -lstdc++, which macOS does not
# have (libde265's template names -lc++ where the C++ library is libc++, as on macOS).
#
# Needs curl, cmake, a C and C++ compiler, and the network; on macOS also otool (the
# Xcode command line tools), and macos-min.sh beside this script. It also runs on Linux, where only the macOS checks are left
# out. bash 3.2 (macOS): no associative arrays, no mapfile, no ${var,,}.
set -euo pipefail

version=1.1.3
archive="libde265-$version.tar.gz"
url="https://github.com/strukturag/libde265/releases/download/v$version/$archive"
sha256=554228bd17788c99a7e63b37ab5634722190e6e2bf60c1dcb01cef328e133905

fail() {
    echo "libde265.sh: $*" >&2
    exit 1
}

if [ "$#" != 1 ] || [ -z "$1" ]; then
    echo "usage: $0 <prefix>" >&2
    exit 2
fi
case "$1" in
    /*) prefix=$1 ;;
    *) prefix="$(pwd)/$1" ;;
esac
library="$prefix/lib/libde265.a"
pc="$prefix/lib/pkgconfig/libde265.pc"
# This script's folder, which holds macos-min.sh. CDPATH is cleared: with it exported, cd
# prints the folder it changes to, which would end up in the path.
scripts=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

case "$(uname -s)" in
    Darwin) os=macos ;;
    Linux) os=linux ;;
    *) fail "this script builds libde265 on macOS (and on Linux, to test it), not on $(uname -s)" ;;
esac

# The version a libde265.pc names, or nothing.
pc_version() {
    sed -n 's/^Version: *//p' "$1" | sed -n 1p
}

if [ -f "$library" ] && [ -f "$pc" ]; then
    installed=$(pc_version "$pc")
    if [ "$installed" = "$version" ]; then
        echo "libde265 $version is already installed in $prefix" >&2
        exit 0
    fi
    fail "$prefix holds libde265 ${installed:-of no known version}, not $version: delete it, or choose another prefix"
fi

for tool in curl cmake; do
    command -v "$tool" >/dev/null 2>&1 || fail "$tool is not on the PATH, and it is needed to build libde265"
done

target=
if [ "$os" = macos ]; then
    command -v otool >/dev/null 2>&1 ||
        fail "otool is not on the PATH, and it checks the build: install the Xcode command line tools (xcode-select --install)"
    target=${MACOSX_DEPLOYMENT_TARGET:-11.0}
fi

tmp=$(mktemp -d "${TMPDIR:-/tmp}/libde265.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
# A signal ends the script through exit, so the EXIT trap removes the temporary folder.
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

echo "Downloading libde265 $version from $url" >&2
curl --fail --location --silent --show-error --retry 3 --output "$tmp/$archive" "$url"
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum <"$tmp/$archive" | cut -d' ' -f1)
else
    actual=$(shasum -a 256 <"$tmp/$archive" | cut -d' ' -f1)
fi
if [ "$actual" != "$sha256" ]; then
    fail "the SHA-256 of $archive is $actual, expected $sha256; nothing was built"
fi
tar -xzf "$tmp/$archive" -C "$tmp"
source_dir="$tmp/libde265-$version"
if [ ! -f "$source_dir/CMakeLists.txt" ]; then
    fail "$archive holds no libde265-$version/CMakeLists.txt; nothing was built"
fi

# The options are those of libde265's CMakeLists.txt. A CMAKE_TOOLCHAIN_FILE in the
# environment, which cmake would read, is the one for libheif's build (.cargo/config.toml):
# it is not for this one.
options=(
    -DCMAKE_BUILD_TYPE=Release
    -DBUILD_SHARED_LIBS=OFF
    -DCMAKE_POSITION_INDEPENDENT_CODE=ON
    -DENABLE_DECODER=OFF
    -DENABLE_ENCODER=OFF
    -DENABLE_SHERLOCK265=OFF
    -DENABLE_INTERNAL_DEVELOPMENT_TOOLS=OFF
    -DWITH_FUZZERS=OFF
    -DENABLE_SDL=OFF
    "-DCMAKE_INSTALL_PREFIX=$prefix"
    -DCMAKE_INSTALL_LIBDIR=lib
    -DCMAKE_INSTALL_INCLUDEDIR=include
)
if [ "$os" = macos ]; then
    options+=("-DCMAKE_OSX_DEPLOYMENT_TARGET=$target")
fi
jobs=$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 2)

echo "Building libde265 $version${target:+ for macOS $target} into $prefix" >&2
unset CMAKE_TOOLCHAIN_FILE
cmake -S "$source_dir" -B "$tmp/build" "${options[@]}" >&2
cmake --build "$tmp/build" --parallel "$jobs" >&2
cmake --install "$tmp/build" >&2

if [ ! -f "$library" ] || [ ! -f "$pc" ]; then
    fail "the install left no $library or no $pc"
fi
if [ "$(pc_version "$pc")" != "$version" ]; then
    fail "the installed $pc names version $(pc_version "$pc"), not $version"
fi

if [ "$os" = macos ]; then
    "$scripts/macos-min.sh" "$target" "$library" >&2
    if grep -q -- '-lstdc++' "$pc"; then
        fail "$pc names -lstdc++, which macOS does not have, so the app's link would fail: $(grep '^Libs.private:' "$pc")"
    fi
    echo "ok: $pc names no -lstdc++: $(grep '^Libs.private:' "$pc")" >&2
fi
echo "Installed libde265 $version in $prefix" >&2
