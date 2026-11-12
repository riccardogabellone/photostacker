#!/usr/bin/env bash
# Checks that every object in static libraries runs on a macOS as old as <target> (spec §11.1):
#
#   scripts/macos-min.sh <target> <archive>...      e.g. scripts/macos-min.sh 11.0 libheif.a
#
# The package check (scripts/smoke.sh) reads the minimum macOS of the final executables
# only, which the linker takes from the deployment target: an object compiled for a newer
# macOS inside a static library would not show there, and would fail on an older Mac. So
# this reads each object of each archive with `otool -l`: LC_BUILD_VERSION's minos, or
# LC_VERSION_MIN_MACOSX's version. It fails with a sentence when an archive is missing, holds
# no object, or holds an object that needs a newer macOS than <target> or names none; it
# prints one line per archive that passes.
#
# Needs otool (the Xcode command line tools). bash 3.2 (macOS): no associative arrays, no
# mapfile.
set -euo pipefail

fail() {
    echo "macos-min.sh: $*" >&2
    exit 1
}

if [ "$#" -lt 2 ]; then
    echo "usage: $0 <target> <archive>...   (e.g. $0 11.0 libheif.a)" >&2
    exit 2
fi
target=$1
shift
case "$target" in
    '' | *[!0-9.]* | .* | *. | *..*)
        echo "usage: $0 <target> <archive>...   (the target is a macOS version, such as 11.0, not \"$target\")" >&2
        exit 2
        ;;
esac
command -v otool >/dev/null 2>&1 ||
    fail "otool is not on the PATH, and it reads the archives: install the Xcode command line tools (xcode-select --install)"

for archive in "$@"; do
    if [ ! -f "$archive" ]; then
        fail "expected a static library at $archive, found none"
    fi
    # One line per object: its name and the oldest macOS it runs on, or "none". otool -l
    # heads each member with a line "<archive>(<member>):"; a member with no load command
    # (the symbol table) is no object. The last field says whether that version is newer
    # than the target, compared number by number (11.0 = 11 < 11.1 < 12).
    report=$(otool -l "$archive" | awk -v target="$target" '
        function newer(version,    a, b, n, i, x, y) {
            n = split(version, a, ".")
            split(target, b, ".")
            if (n < 3) n = 3
            for (i = 1; i <= n; i++) {
                x = (a[i] == "" ? 0 : a[i] + 0)
                y = (b[i] == "" ? 0 : b[i] + 0)
                if (x != y) return (x > y)
            }
            return 0
        }
        function done() {
            if (member != "" && commands)
                print member, (minos == "" ? "none" : minos), (minos == "" || newer(minos) ? "bad" : "ok")
        }
        /\(.*\):$/ { done(); member = $0; sub(/^.*\(/, "", member); sub(/\):$/, "", member); commands = 0; minos = ""; next }
        $1 == "Load" && $2 == "command" { commands = 1 }
        $1 == "cmd" { cmd = $2 }
        minos == "" && cmd == "LC_BUILD_VERSION" && $1 == "minos" { minos = $2 }
        minos == "" && cmd == "LC_VERSION_MIN_MACOSX" && $1 == "version" { minos = $2 }
        END { done() }') || fail "otool -l could not read $archive"
    objects=$(printf '%s\n' "$report" | grep -c . || true)
    if [ "$objects" = 0 ]; then
        fail "expected objects in $archive, found none"
    fi
    # The last two fields are the version and the verdict; the name is the rest.
    wrong=$(printf '%s\n' "$report" | awk '
        $NF == "bad" {
            version = $(NF - 1)
            name = $0
            sub(/ [^ ]+ [^ ]+$/, "", name)
            print "  " name (version == "none" ? " names no minimum macOS" : " needs macOS " version)
        }')
    if [ -n "$wrong" ]; then
        fail "expected every object in $archive to run on macOS $target, found:
$wrong"
    fi
    echo "ok: all $objects objects in $archive run on macOS $target"
done
