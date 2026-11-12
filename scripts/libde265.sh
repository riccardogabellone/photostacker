#!/usr/bin/env bash
# libde265, the HEVC decoder that libheif links, built as a static library for the Linux
# and macOS builds (spec §3, §11.1):
#
#   scripts/libde265.sh <prefix>
#
# The packages of every system link one libde265 release, built in: on Windows vcpkg's
# (scripts/windows-heif.ps1), on Linux and macOS this script's. A Linux distribution has its
# own release of libde265, with its own patches (Ubuntu 22.04 has 1.0.8), as a shared
# library; macOS has none, and Homebrew's is built for the macOS it runs on, not for the
# oldest one the app supports. So this script downloads the release pinned below from
# libde265's GitHub releases, checks its SHA-256, and builds it with cmake: static,
# position-independent, a release build, with no tools (no dec265, no encoder, no
# sherlock265, no development tools, no fuzzers) and no SDL; on macOS for
# MACOSX_DEPLOYMENT_TARGET, 11.0 when it is unset, and without the AVX2 and AVX-512 kernels
# (the options below say why). It installs into <prefix>: the library in
# <prefix>/lib/libde265.a, the headers in <prefix>/include/libde265, the pkg-config file in
# <prefix>/lib/pkgconfig/libde265.pc. Then point the build at it:
#
#   Linux:  CMAKE_PREFIX_PATH=<prefix> PKG_CONFIG_PATH=<prefix>/lib/pkgconfig cargo build ...
#   macOS:  CMAKE_PREFIX_PATH=<prefix> PKG_CONFIG_LIBDIR=<prefix>/lib/pkgconfig cargo build ...
#
# libheif's cmake build (through libheif-sys) finds it there before the system's folders,
# and links it statically, as there is no shared one beside it; on Linux that holds even
# where the system's libde265-dev is installed. On Linux PKG_CONFIG_PATH adds the folder to
# pkg-config's search, where PKG_CONFIG_LIBDIR would replace it: other crates of the build
# ask pkg-config about the system's libraries. libheif's cmake build keeps the libde265 it
# found first, so after these variables are set, changed or removed, run
# `cargo clean -p libheif-sys` and `cargo clean --release -p libheif-sys` before the build.
#
# A run that finds libde265.a and libde265.pc of this version in <prefix> builds nothing.
# cmake installs the .pc file last, so an interrupted install has none, and the next run
# builds again. A <prefix> that holds another version is refused.
#
# On macOS it then checks the library, built by this run or by an earlier one: every object
# in libde265.a runs on the deployment target (scripts/macos-min.sh), none needs the CPU
# detection of clang's runtime library, which the app's link does not have, and the .pc
# file names no -lstdc++, which macOS does not have (libde265's template names -lc++ where
# the C++ library is libc++, as on macOS).
#
# Needs curl, cmake, a C and C++ compiler, and the network; on macOS also otool and nm (the
# Xcode command line tools), and macos-min.sh beside this script. On Linux the macOS checks
# are left out. bash 3.2 (macOS): no associative arrays, no mapfile, no ${var,,}.
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
    *) fail "this script builds libde265 on Linux and macOS, not on $(uname -s)" ;;
esac

# The version a libde265.pc names, or nothing.
pc_version() {
    sed -n 's/^Version: *//p' "$1" | sed -n 1p
}

target=
if [ "$os" = macos ]; then
    for tool in otool nm; do
        command -v "$tool" >/dev/null 2>&1 ||
            fail "$tool is not on the PATH, and it checks the build: install the Xcode command line tools (xcode-select --install)"
    done
    target=${MACOSX_DEPLOYMENT_TARGET:-11.0}
fi

# macOS: the checks of the library in <prefix>. $1 ends each failure: what to do about a
# library that an earlier run built, nothing for the one this run built.
check_macos() {
    local hint=$1 undefined cpu_detection
    "$scripts/macos-min.sh" "$target" "$library" >&2 ||
        fail "$library did not pass scripts/macos-min.sh for macOS $target (the line above says why)$hint"
    # The symbols behind __builtin_cpu_supports and __builtin_cpu_init on x86 (the build's
    # options, below, say why no object may need them): a later release that asks for them
    # elsewhere fails here, not in the app's link. nm writes each name last on its line,
    # after a `U` (GNU's) or alone (Apple's), with two underscores before `cpu` in ELF and
    # three in Mach-O.
    undefined=$(nm -u "$library") || fail "nm could not read $library$hint"
    cpu_detection=$(printf '%s\n' "$undefined" | sed 's/.*[[:space:]]//' | grep -E '^_{2,3}cpu_(model|indicator_init|features2)$' | sort -u | tr '\n' ' ' || true)
    if [ -n "$cpu_detection" ]; then
        fail "$library needs the CPU detection of clang's runtime library, which the app's link does not have: ${cpu_detection% }$hint"
    fi
    echo "ok: no object in $library needs the CPU detection of clang's runtime library" >&2
    if grep -q -- '-lstdc++' "$pc"; then
        fail "$pc names -lstdc++, which macOS does not have, so the app's link would fail: $(grep '^Libs.private:' "$pc")$hint"
    fi
    echo "ok: $pc names no -lstdc++: $(grep '^Libs.private:' "$pc")" >&2
}

if [ -f "$library" ] && [ -f "$pc" ]; then
    installed=$(pc_version "$pc")
    if [ "$installed" = "$version" ]; then
        echo "libde265 $version is already installed in $prefix" >&2
        if [ "$os" = macos ]; then
            check_macos "; an earlier run built this library: delete $prefix and run this script again"
        fi
        exit 0
    fi
    fail "$prefix holds libde265 ${installed:-of no known version}, not $version: delete it, or choose another prefix"
fi

for tool in curl cmake; do
    command -v "$tool" >/dev/null 2>&1 || fail "$tool is not on the PATH, and it is needed to build libde265"
done

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
    # libde265 chooses its AVX2 and AVX-512 kernels at run time with __builtin_cpu_supports,
    # which on x86 needs two symbols of clang's runtime library (___cpu_model and
    # ___cpu_indicator_init). rustc links a macOS executable without that library
    # (-nodefaultlibs), so on an Intel Mac the app's link fails with the two undefined.
    # Without these kernels the SSE4.1 ones remain, which ask the CPU with cpuid alone. On
    # Apple silicon the two options change nothing: they are read only in an x86 build.
    options+=(-DENABLE_AVX2=OFF -DENABLE_AVX512=OFF)
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
    check_macos ""
fi
echo "Installed libde265 $version in $prefix" >&2
