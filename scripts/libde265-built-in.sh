#!/usr/bin/env bash
# Fails unless libde265 is built into each Linux executable given (spec §3, §11.1):
#
#   scripts/libde265-built-in.sh <executable>...
#
# The packages and CI link the pinned libde265 statically (scripts/libde265.sh). An
# executable whose NEEDED entries (objdump -p) name libde265 was linked against the
# system's shared library instead, as a development build without the pinned libde265's
# variables is. An executable that names no library at all is refused too: every one built
# here links the C library, so an empty list means the reading failed. scripts/package.sh
# runs this before it packages, and CI after its release build; scripts/smoke.sh checks the
# packages' executables with its own, longer list.
#
# Needs objdump (binutils).
set -euo pipefail

fail() {
    echo "libde265-built-in.sh: $*" >&2
    exit 1
}

if [ "$#" = 0 ]; then
    echo "usage: $0 <executable>..." >&2
    exit 2
fi
command -v objdump >/dev/null 2>&1 ||
    fail "objdump (binutils) is not on the PATH, and it reads the libraries an executable needs"

for executable in "$@"; do
    needed=$(objdump -p "$executable" | awk '$1 == "NEEDED" { print $2 }') ||
        fail "objdump could not read $executable"
    if [ -z "$needed" ]; then
        fail "expected $executable to name the libraries it needs, found none"
    fi
    for library in $needed; do
        case "$library" in
            libde265.so*) fail "$executable needs $library, the system's shared libde265, instead of the pinned libde265 built in: build the pinned one with scripts/libde265.sh <prefix>, set CMAKE_PREFIX_PATH=<prefix> and PKG_CONFIG_PATH=<prefix>/lib/pkgconfig, run cargo clean --release -p libheif-sys, and build again" ;;
        esac
    done
    echo "ok: libde265 is built into $executable" >&2
done
