#!/usr/bin/env bash
# Checks built packages before anyone installs them (packaging/README.md):
#
#   scripts/smoke.sh [--install] <package>...
#
# --install really installs on this machine, and is meant for a throwaway one (a CI
# runner): the .deb through apt-get, the NSIS installer through its silent install. Without
# it, nothing is installed and the machine is left as it was.
#
# Each package is opened by its kind, on the system it is for:
#   .deb            dpkg -x into a scratch folder; with --install, apt-get installs it instead
#                   (as root, or through sudo) and removes it afterwards
#   .AppImage       --appimage-extract in a scratch folder; the tool then runs from the tree
#                   with LD_LIBRARY_PATH at the tree's own libraries, as the AppImage's
#                   launcher sets it
#   .dmg            hdiutil attach, read-only, on a scratch mount point, detached on every exit
#   -setup.exe      the NSIS installer: only checked to exist and not be empty, unless
#                   --install: then a silent install into a scratch folder, the checks, a
#                   silent uninstall, and the registry keys the uninstaller leaves are removed.
#                   Before any package is checked, it refuses when any trace of Photostacker
#                   is there for this user (its entry in the system's list of apps, its
#                   registry key, its Start menu folder) or Photostacker.exe runs: the
#                   installer would take over the entry and the Start menu shortcut, the
#                   uninstaller would delete them, and both end a running Photostacker without
#                   asking. So nothing the install touches was there before it, and nothing
#                   needs putting back.
#   .msi            msiexec's administrative unpack (/a) into a scratch folder, which installs
#                   nothing: it only writes the files there, as the MSI would install them.
#                   A real install is never made: it needs elevation
#   -portable.zip   Expand-Archive into a scratch folder; it holds the files the installers
#                   install
# and then checked:
#   contents   both executables; ONNX Runtime's library where the app looks for it and the
#              eight files of the licenses folder (spec §8.3, §11.3); in the AppImage, no
#              libde265 library, since libde265 is built into the executables
#   links      Linux: each executable links only the C and C++ runtimes (one that needs
#              libde265 was built against the system's libde265, not the pinned one); macOS:
#              only the system's own libraries; Windows: every Visual C++ runtime DLL that
#              the two executables and ONNX Runtime name is beside them
#              (scripts/vc-runtime.sh), no libheif, libde265 or x265 DLL is in the package,
#              and the tool runs with a PATH of Windows' own folders only
#   macOS      the minimum macOS of every Mach-O file: 11.0, but 14.0 for ONNX Runtime on
#              Apple silicon
#   runs       `photostacker-cli info` names the version in the package's name, libde265,
#              and the ONNX Runtime of scripts/onnxruntime.tsv for this system, loaded from
#              the package; `photostacker-cli run crates/core/tests/fixtures/heic --dry-run`
#              succeeds, and its plan lists every HEIC file there, with no error
#
# Every check prints one line. The first that fails prints what it expected and what it
# found, and the script exits 1. The tool runs with ORT_DYLIB_PATH unset and its data folder
# in the scratch folder, which is removed at the end.
#
# Needs: Linux: dpkg, objdump (binutils); apt-get for --install. macOS: hdiutil,
# otool, file. Windows (Git Bash): cygpath, PowerShell for the zip and the MSI, msiexec.exe,
# and reg.exe and tasklist.exe for --install.
#
# macOS has bash 3.2 and BSD tools: no associative arrays, no mapfile, no readlink -f.
set -euo pipefail

fail() {
    echo "FAILED: $*" >&2
    exit 1
}

ok() {
    echo "ok: $*"
}

usage() {
    echo "usage: $0 [--install] <package>..." >&2
    echo "  --install  really install the .deb (apt-get) and the NSIS installer on this machine; for a throwaway one, such as a CI runner" >&2
    exit 2
}

install=0
if [ "${1:-}" = --install ]; then
    install=1
    shift
fi
if [ "$#" = 0 ]; then
    usage
fi

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
fixtures="$repo/crates/core/tests/fixtures/heic"

case "$(uname -s)" in
    Linux) os=linux exe= library=libonnxruntime.so ;;
    Darwin) os=macos exe= library=libonnxruntime.dylib ;;
    MINGW* | MSYS*) os=windows exe=.exe library=onnxruntime.dll ;;
    *) fail "packages are checked on Linux, Windows and macOS, not on $(uname -s)" ;;
esac
case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    arm64 | aarch64) arch=aarch64 ;;
    *) arch=$(uname -m) ;;
esac

# The ONNX Runtime the package must carry: this system's row of scripts/onnxruntime.tsv.
ort_version=
while IFS=$'\t' read -r row_os row_arch row_version _ || [ -n "${row_os:-}" ]; do
    case "$row_os" in '' | '#'*) continue ;; esac
    if [ "$row_os" = "$os" ] && [ "$row_arch" = "$arch" ]; then
        ort_version=$row_version
        break
    fi
done <"$repo/scripts/onnxruntime.tsv"
if [ -z "$ort_version" ]; then
    fail "scripts/onnxruntime.tsv pins no ONNX Runtime build for $os $arch"
fi

# The packages by absolute path, before anything changes folder.
packages=()
for package in "$@"; do
    case "$package" in
        /*) packages+=("$package") ;;
        *) packages+=("$(pwd)/$package") ;;
    esac
done

# A path in the form native programs print and take: Windows form on Windows.
native() {
    if [ "$os" = windows ]; then
        cygpath -w "$1"
    else
        printf '%s\n' "$1"
    fi
}

if [ "$os" = windows ]; then
    # Under the repository: the system's temporary folder may be named in its short 8.3
    # form, which the tool would not print back.
    mkdir -p "$repo/dist"
    scratch=$(mktemp -d "$repo/dist/smoke.XXXXXX")
    if command -v pwsh >/dev/null 2>&1; then
        powershell=pwsh
    else
        powershell=powershell.exe
    fi
    # Windows' own folders: the tool must run with nothing else on the PATH.
    system_root=$(cygpath -u "${SYSTEMROOT:-C:\\Windows}")
    windows_path="$system_root/System32:$system_root"
else
    tmp=${TMPDIR:-/tmp}
    scratch=$(mktemp -d "${tmp%/}/photostacker-smoke.XXXXXX")
fi

# What a failed or interrupted run must not leave behind.
mounted=         # the disk image's mount point, while it may be attached
nsis_dir=        # an NSIS install, until it is uninstalled
nsis_registry=0  # 1 from the NSIS install until its registry keys are cleaned up
deb_installed=0  # the Debian package, installed with --install
sudo=
cleanup() {
    local status=$?
    if [ -n "$mounted" ]; then
        detach "$mounted" || true
    fi
    if [ -n "$nsis_dir" ]; then
        uninstall_nsis "$nsis_dir" >/dev/null 2>&1 || true
    fi
    if [ "$nsis_registry" = 1 ]; then
        nsis_forget >/dev/null 2>&1 || true
    fi
    if [ "$deb_installed" = 1 ]; then
        $sudo apt-get remove -y photostacker >/dev/null 2>&1 || true
    fi
    rm -rf "$scratch" || true
    exit "$status"
}
trap cleanup EXIT
# A signal ends the script through exit, so the EXIT trap cleans up.
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

# The tool must find the package's own ONNX Runtime, never a developer's.
unset ORT_DYLIB_PATH
PHOTOSTACKER_DATA_DIR=$(native "$scratch/data")
export PHOTOSTACKER_DATA_DIR

# Detaches the disk image mounted at $1. A fresh mount may be held for a moment (Spotlight
# indexing it), so it tries again, then forces it.
detach() {
    hdiutil detach "$1" -quiet 2>/dev/null && return 0
    sleep 2
    hdiutil detach "$1" -quiet 2>/dev/null && return 0
    hdiutil detach "$1" -force -quiet 2>/dev/null
}

# Runs a Windows program whose arguments start with `/` (installer switches, reg.exe and
# tasklist.exe options): MSYS would otherwise take them for paths and rewrite them.
windows_command() {
    MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*' "$@"
}

# The registry keys that cargo-packager 0.11.8's NSIS template (installer.nsi) writes for a
# current-user install: the uninstall entry, which Windows lists among the installed apps
# (UNINSTKEY), and HKCU\Software\<publisher>\<product name> with the install folder, which
# the next install would reuse (MANUPRODUCTKEY). The uninstaller removes the first and
# leaves the second. The publisher is the packager's, in crates/ui/Cargo.toml.
nsis_keys() {
    local publisher
    publisher=$(sed -n 's/^publisher = "\(.*\)"$/\1/p' "$repo/crates/ui/Cargo.toml" | sed -n 1p)
    if [ -z "$publisher" ]; then
        fail "--install: expected a publisher in crates/ui/Cargo.toml's packager table, found none"
    fi
    uninstall_key='HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\Photostacker'
    publisher_key="HKCU\\Software\\$publisher"
    product_key="$publisher_key\\Photostacker"
}

# Whether the registry key $1 exists, in the 64-bit view the installer writes to.
key_exists() {
    windows_command reg.exe query "$1" /reg:64 >/dev/null 2>&1
}

# Before a real NSIS install, and before any package is checked: refuses when any trace of
# Photostacker is there for this user, or it runs. The install would take over the
# uninstall entry, the product key and the Start menu shortcut, the uninstall would delete
# them, and the installer and the uninstaller end a running Photostacker without asking.
# Also notes whether the publisher's key is there, so nsis_forget removes it only if the
# install made it.
nsis_precheck() {
    local tasks
    nsis_keys
    # A silent install still makes the Start menu folder (only the desktop shortcut obeys
    # /NS); the uninstaller removes it.
    if [ -z "${APPDATA:-}" ]; then
        fail "--install: expected APPDATA to name this user's roaming folder, found it unset"
    fi
    start_menu="$(cygpath -u "$APPDATA")/Microsoft/Windows/Start Menu/Programs/Photostacker"
    if key_exists "$uninstall_key"; then
        fail "--install: expected no trace of Photostacker for this user, found $uninstall_key (Photostacker is installed); the silent install would take it over and the uninstall would remove it: uninstall it first, or check the installer on a throwaway machine"
    fi
    if key_exists "$product_key"; then
        fail "--install: expected no trace of Photostacker for this user, found $product_key (left by an earlier install); the silent install would take it over and this script would remove it: delete it first, or check the installer on a throwaway machine"
    fi
    if [ -e "$start_menu" ]; then
        fail "--install: expected no trace of Photostacker for this user, found the Start menu folder $(native "$start_menu"); the silent install would overwrite its shortcut and the uninstall would remove it: delete it first, or check the installer on a throwaway machine"
    fi
    tasks=$(windows_command tasklist.exe /FI "IMAGENAME eq Photostacker.exe" /FO CSV /NH 2>/dev/null | tr -d '\r') ||
        fail "--install: tasklist.exe could not list the running programs"
    case "$tasks" in
        *'"Photostacker.exe"'*) fail "--install: expected Photostacker.exe not to be running, found it running; the silent installer and uninstaller would end it without asking: close it first" ;;
    esac
    publisher_key_existed=0
    if key_exists "$publisher_key"; then
        publisher_key_existed=1
    fi
    ok "--install: no trace of Photostacker for this user (no $uninstall_key, no $product_key, no Start menu folder $(native "$start_menu")), and Photostacker.exe is not running"
}

# After the NSIS uninstall: removes the product key the uninstaller leaves, which the
# install made, and the publisher's key if the install made that too. Returns non-zero when
# reg.exe fails (it also runs from the EXIT trap, so it does not exit).
nsis_forget() {
    if key_exists "$product_key"; then
        windows_command reg.exe delete "$product_key" /f /reg:64 >/dev/null || return 1
    fi
    if [ "$publisher_key_existed" = 0 ] && key_exists "$publisher_key"; then
        windows_command reg.exe delete "$publisher_key" /f /reg:64 >/dev/null || return 1
    fi
    nsis_registry=0
}

# Runs NSIS's silent uninstaller of the install in $1 in place (`_?=`, which must come last
# and unquoted), so it ends before this returns; it leaves itself behind.
uninstall_nsis() {
    windows_command "$1/uninstall.exe" /S "_?=$(native "$1")"
}

# Unpacks the MSI $1 into the folder $2 with msiexec's administrative install (/a): it only
# writes the files into $2, as the MSI would install them, plus a copy of the MSI. Nothing
# is installed, no registry key is written and the system's list of apps does not change.
# msiexec returns before it is done unless it is waited for, so PowerShell starts it and
# waits; the command line goes from PowerShell to msiexec, which takes a quoted path, spaces
# and all, both as an argument and as a property's value (TARGETDIR="<folder>"). On a
# failure, the end of msiexec's log ($3) is printed.
unpack_msi() {
    local msi target log
    # Single quotes in a PowerShell string are doubled.
    msi=$(native "$1" | sed "s/'/''/g")
    target=$(native "$2" | sed "s/'/''/g")
    log=$(native "$3" | sed "s/'/''/g")
    windows_command "$powershell" -NoProfile -NonInteractive -Command "
        \$ErrorActionPreference = 'Stop'
        \$arguments = '/a \"$msi\" /qn TARGETDIR=\"$target\" /l*v \"$log\"'
        \$process = Start-Process -FilePath msiexec.exe -ArgumentList \$arguments -Wait -PassThru
        if (\$process.ExitCode -ne 0) {
            if (Test-Path -LiteralPath '$log') {
                Get-Content -LiteralPath '$log' -Tail 40 | Write-Host
            }
            Write-Host \"msiexec \$arguments ended with exit status \$(\$process.ExitCode)\"
        }
        exit \$process.ExitCode"
}

# What is at $1, for a failure's "found".
describe() {
    if [ -L "$1" ]; then
        echo "a link"
    elif [ -d "$1" ]; then
        echo "a folder"
    elif [ -f "$1" ] && [ ! -s "$1" ]; then
        echo "an empty file"
    elif [ -e "$1" ]; then
        echo "something else"
    else
        echo "nothing"
    fi
}

# $1 must be a file that is not empty and not a link: the package carries the file itself.
expect_file() {
    if [ -L "$1" ] || [ ! -f "$1" ] || [ ! -s "$1" ]; then
        fail "$2: expected a file at $1, found $(describe "$1")"
    fi
}

check_contents() {
    local name
    for name in Photostacker photostacker-cli; do
        expect_file "$bin_dir/$name$exe" "contents"
        if [ "$os" != windows ] && [ ! -x "$bin_dir/$name$exe" ]; then
            fail "contents: expected $bin_dir/$name$exe to be executable, found it is not"
        fi
    done
    expect_file "$lib_dir/$library" "contents"
    for name in LICENSE THIRD-PARTY-NOTICES.md THIRD-PARTY-LICENSES.md LGPL-2.1.txt LGPL-3.0.txt \
        GPL-3.0.txt onnxruntime-LICENSE.txt onnxruntime-ThirdPartyNotices.txt; do
        expect_file "$res_dir/licenses/$name" "contents"
    done
    ok "contents: Photostacker$exe and photostacker-cli$exe in $bin_dir, $library in $lib_dir, the eight licence files in $res_dir/licenses"
}

# Linux: the libraries each executable names (NEEDED) are the C and C++ runtimes, which
# the Debian package depends on. libheif and libde265 are built in (scripts/libde265.sh):
# an executable that needs libde265 was built without the pinned one.
check_links_linux() {
    local name dump needed lib
    command -v objdump >/dev/null 2>&1 || fail "links: objdump (binutils) is needed to read what the executables link"
    for name in Photostacker photostacker-cli; do
        dump=$(objdump -p "$bin_dir/$name") || fail "links: objdump could not read $bin_dir/$name"
        needed=$(printf '%s\n' "$dump" | awk '$1 == "NEEDED" { print $2 }')
        if [ -z "$needed" ]; then
            fail "links: expected $name to name the libraries it links, found none"
        fi
        for lib in $needed; do
            case "$lib" in
                libde265.so*) fail "links: expected libde265 built into $name, found it needs $lib: the package was built against the system's libde265 instead of the pinned one. Build the pinned one with scripts/libde265.sh <prefix>, set CMAKE_PREFIX_PATH=<prefix> and PKG_CONFIG_PATH=<prefix>/lib/pkgconfig, run cargo clean --release -p libheif-sys, and build the packages again with scripts/package.sh" ;;
                libm.so.* | libstdc++.so.* | libgcc_s.so.* | libc.so.* | ld-linux* | \
                    libdl.so.* | libpthread.so.* | librt.so.*) ;;
                *) fail "links: expected $name to link only libm, libstdc++, libgcc_s, libc, ld-linux, libdl, libpthread and librt, found $lib" ;;
            esac
        done
        ok "links: $name links only" $needed
    done
}

# macOS: each executable links only the system's own libraries. `otool -L`'s first line
# names the file itself.
check_links_macos() {
    local name libs lib
    for name in Photostacker photostacker-cli; do
        libs=$(otool -L "$bin_dir/$name" | tail -n +2 | awk '{ print $1 }') ||
            fail "links: otool could not read $bin_dir/$name"
        if [ -z "$libs" ]; then
            fail "links: expected $name to name the libraries it links, found none"
        fi
        for lib in $libs; do
            case "$lib" in
                /usr/lib/* | /System/Library/*) ;;
                *) fail "links: expected $name to link only libraries under /usr/lib/ and /System/Library/, found $lib" ;;
            esac
        done
        ok "links: $name links only libraries under /usr/lib/ and /System/Library/"
    done
}

# Windows: every Visual C++ runtime DLL the two executables and ONNX Runtime name is beside
# them, since a clean Windows has none, though every machine this runs on has them all in
# System32 (scripts/vc-runtime.sh says how the names are read); no HEIC library ships as a
# DLL (libheif and libde265 are built in); and the runs below have only Windows' own folders
# on the PATH.
check_links_windows() {
    local found
    "$repo/scripts/vc-runtime.sh" check "$bin_dir" || exit 1
    found=$(find "$root" -type f \( -iname '*heif*.dll' -o -iname '*de265*.dll' -o -iname '*x265*.dll' \))
    if [ -n "$found" ]; then
        fail "links: expected no libheif, libde265 or x265 DLL in the package, found $found"
    fi
    ok "links: no libheif, libde265 or x265 DLL in the package; the tool runs below with PATH=$windows_path only"
}

# The oldest macOS a Mach-O file runs on, as otool writes it: LC_BUILD_VERSION's minos, or
# LC_VERSION_MIN_MACOSX's version.
minimum_macos() {
    otool -l "$1" | awk '
        $1 == "cmd" { cmd = $2 }
        !found && cmd == "LC_BUILD_VERSION" && $1 == "minos" { print $2; found = 1 }
        !found && cmd == "LC_VERSION_MIN_MACOSX" && $1 == "version" { print $2; found = 1 }'
}

check_minimum_macos() {
    local file expected minos count=0
    while IFS= read -r file; do
        case "$(file -b "$file")" in
            *Mach-O*) ;;
            *) continue ;;
        esac
        expected=11.0
        if [ "$(basename "$file")" = libonnxruntime.dylib ] && [ "$arch" = aarch64 ]; then
            expected=14.0
        fi
        minos=$(minimum_macos "$file") || fail "macOS: expected otool -l to read $file, found it failed"
        if [ "$minos" != "$expected" ]; then
            fail "macOS: expected $file to need macOS $expected, found ${minos:-no minimum}"
        fi
        ok "macOS: ${file#"$root"/} needs macOS $minos"
        count=$((count + 1))
    done <<EOF
$(find "$root" -type f)
EOF
    if [ "$count" -lt 3 ]; then
        fail "macOS: expected the two executables and ONNX Runtime among the Mach-O files, found $count Mach-O files"
    fi
}

# Runs photostacker-cli of the package with its arguments, as the package would: on Windows
# with only Windows' own folders on the PATH, from an AppImage's tree with its libraries.
run_cli() {
    if [ "$os" = windows ]; then
        PATH="$windows_path" "$bin_dir/photostacker-cli$exe" "$@"
    elif [ -n "$library_path" ]; then
        LD_LIBRARY_PATH="$library_path" "$bin_dir/photostacker-cli$exe" "$@"
    else
        "$bin_dir/photostacker-cli$exe" "$@"
    fi
}

# Where the tool must say it loaded ONNX Runtime from: the folder of spec §8.3 beside the
# executable's, joined, not resolved. On Linux and macOS the executable's own folder is
# resolved first (links followed); Windows takes it as it is.
expected_library() {
    local exe_dir
    case "$os" in
        linux)
            exe_dir=$(cd "$bin_dir" && pwd -P)
            printf '%s\n' "$exe_dir/../lib/Photostacker/$library"
            ;;
        macos)
            exe_dir=$(cd "$bin_dir" && pwd -P)
            printf '%s\n' "$exe_dir/../Frameworks/$library"
            ;;
        windows) printf '%s\n' "$(native "$bin_dir")\\$library" ;;
    esac
}

# A path as compared: on Windows without regard to case or the kind of slash.
comparable() {
    if [ "$os" = windows ]; then
        printf '%s\n' "$1" | tr '\\' '/' | tr '[:upper:]' '[:lower:]'
    else
        printf '%s\n' "$1"
    fi
}

check_runs() {
    local info status line expected plan errors heic name pattern
    info=$(run_cli info 2>"$scratch/info.log") || {
        status=$?
        cat "$scratch/info.log" >&2
        fail "runs: expected photostacker-cli info to succeed, found exit status $status"
    }
    info=$(printf '%s\n' "$info" | tr -d '\r')

    line=$(printf '%s\n' "$info" | sed -n 1p)
    if [ "$line" != "photostacker-cli $version" ]; then
        fail "runs: expected \"photostacker-cli $version\" from info, found \"$line\""
    fi
    ok "runs: info says photostacker-cli $version"

    line=$(printf '%s\n' "$info" | grep '^heic: ' || true)
    case "$line" in
        "heic: libheif "*", HEVC decoder libde265") ;;
        *) fail "runs: expected \"heic: libheif <version>, HEVC decoder libde265\" from info, found \"$line\"" ;;
    esac
    ok "runs: info says $line"

    line=$(printf '%s\n' "$info" | grep '^visual check: ' || true)
    expected="visual check: ONNX Runtime $ort_version at $(expected_library)"
    if [ "$(comparable "$line")" != "$(comparable "$expected")" ]; then
        fail "runs: expected \"$expected\" from info, found \"$line\""
    fi
    ok "runs: info says $line"

    plan="$scratch/plan.json"
    run_cli run "$(native "$fixtures")" --dry-run >"$plan" 2>"$scratch/run.log" || {
        status=$?
        cat "$scratch/run.log" >&2
        fail "runs: expected photostacker-cli run $fixtures --dry-run to succeed, found exit status $status"
    }
    # The plan lists each file it read and grouped as a string of its own on a line, in a
    # group's files or in unstacked; a file it could not read goes to errors instead, as an
    # object with its file and the error.
    tr -d '\r' <"$plan" >"$plan.lf"
    errors=$(awk '
        /^  "errors": \[\]/ { print; next }
        /^  "errors": \[/ { inside = 1 }
        inside { print }
        inside && /^  \]/ { inside = 0 }' "$plan.lf" | tr '\n' ' ' | tr -s ' ')
    for heic in "$fixtures"/*.heic; do
        name=$(basename "$heic")
        pattern="^[[:space:]]*\"$(printf '%s\n' "$name" | sed 's/[.]/\\./g')\",?$"
        if ! grep -Eq "$pattern" "$plan.lf"; then
            fail "runs: expected the dry run's plan to list $name among the files it read, found it does not; the plan's$errors"
        fi
    done
    if ! grep -Eq '^  "errors": \[\],?$' "$plan.lf"; then
        fail "runs: expected the dry run's plan to have no errors, found$errors"
    fi
    ok "runs: a dry run over $fixtures read every HEIC file there, with no error"
}

# With --install and an NSIS installer to install: the checks that refuse come before any
# package is opened.
nsis_root="$scratch/nsis"
if [ "$os" = windows ] && [ "$install" = 1 ]; then
    for package in "${packages[@]}"; do
        case "$package" in
            *-setup.exe)
                # NSIS takes /D= last and unquoted, spaces and all; but MSYS quotes an
                # argument that has a space, and the installer then does not get the folder
                # as given. So a folder with a space is refused.
                case "$(native "$nsis_root")" in
                    *' '*) fail "--install: expected an install folder without spaces, found $(native "$nsis_root"): MSYS would quote the /D= argument, and the installer would not get the folder as given" ;;
                esac
                nsis_precheck
                break
                ;;
        esac
    done
fi

for package in "${packages[@]}"; do
    name=$(basename "$package")
    echo "== $name"
    if [ ! -f "$package" ]; then
        fail "$name: expected a package at $package, found $(describe "$package")"
    fi
    rest=${name#Photostacker_}
    version=${rest%%_*}
    if [ "$rest" = "$name" ] || [ "$version" = "$rest" ] || [ -z "$version" ]; then
        fail "$name: expected a name Photostacker_<version>_..., found $name"
    fi

    root= bin_dir= lib_dir= res_dir= library_path=
    case "$os:$name" in
        linux:*.deb)
            if [ "$install" = 1 ]; then
                if [ "$(id -u)" != 0 ]; then
                    sudo=sudo
                fi
                deb_installed=1
                DEBIAN_FRONTEND=noninteractive $sudo apt-get install -y "$package" >/dev/null ||
                    fail "$name: expected apt-get to install it, found it failed"
                root=/ bin_dir=/usr/bin lib_dir=/usr/lib/Photostacker
                ok "installed with apt-get"
            else
                root="$scratch/deb"
                dpkg -x "$package" "$root" || fail "$name: expected dpkg -x to unpack it, found it failed"
                bin_dir="$root/usr/bin" lib_dir="$root/usr/lib/Photostacker"
                ok "unpacked with dpkg -x into $root"
            fi
            res_dir=$lib_dir
            ;;
        linux:*.AppImage)
            # A copy, made executable: a downloaded file may have lost its mode.
            { mkdir "$scratch/appimage" && cp "$package" "$scratch/appimage/package.AppImage" &&
                chmod +x "$scratch/appimage/package.AppImage"; } ||
                fail "$name: expected to copy it into $scratch/appimage and make the copy executable, found it failed"
            (cd "$scratch/appimage" && ./package.AppImage --appimage-extract >/dev/null) ||
                fail "$name: expected --appimage-extract to unpack it, found it failed"
            rm "$scratch/appimage/package.AppImage" ||
                fail "$name: expected to remove the copy $scratch/appimage/package.AppImage, found it failed"
            root="$scratch/appimage/squashfs-root"
            bin_dir="$root/usr/bin" lib_dir="$root/usr/lib/Photostacker"
            res_dir=$lib_dir library_path="$root/usr/lib"
            ok "unpacked with --appimage-extract into $root"
            ;;
        macos:*.dmg)
            mkdir "$scratch/dmg" || fail "$name: expected to make the mount point $scratch/dmg, found it failed"
            mounted="$scratch/dmg"
            hdiutil attach -nobrowse -readonly -noautoopen -mountpoint "$mounted" "$package" >/dev/null ||
                fail "$name: expected hdiutil to attach it, found it failed"
            root="$mounted/Photostacker.app"
            bin_dir="$root/Contents/MacOS" lib_dir="$root/Contents/Frameworks"
            res_dir="$root/Contents/Resources"
            ok "attached at $mounted"
            ;;
        windows:*-setup.exe)
            if [ "$install" != 1 ]; then
                if [ ! -s "$package" ]; then
                    fail "$name: expected an installer, found an empty file"
                fi
                ok "$name exists and is not empty; it is not installed here: only --install installs it, which changes this machine (the portable zip holds the same files and gets the full check)"
                continue
            fi
            root=$nsis_root
            nsis_registry=1
            nsis_dir=$root
            # /NS: no desktop shortcut. /D= must come last, unquoted.
            windows_command "$package" /S /NS "/D=$(native "$root")" ||
                fail "$name: expected the silent install to succeed, found exit status $?"
            bin_dir=$root lib_dir=$root res_dir=$root
            ok "installed silently into $(native "$root")"
            ;;
        windows:*.msi)
            # The MSI is never installed, which needs elevation: its administrative unpack
            # gives the files it would install. In cargo-packager 0.11.8's WiX template
            # (main.wxs), the unpack's root is TARGETDIR, the Program Files folder in it is
            # named PFiles, and INSTALLDIR, which holds every file, PFiles\Photostacker.
            root="$scratch/msi"
            # msiexec's status can pass 255, which a shell's status cannot hold: the lines
            # before the failure give it.
            unpack_msi "$package" "$root" "$scratch/msi.log" ||
                fail "$name: expected msiexec /a to unpack it into $(native "$root"), found it failed (the end of its log and its exit status are above; on a developer's machine, where an administrative unpack may need an elevated prompt, run this check from one)"
            bin_dir="$root/PFiles/Photostacker"
            if [ ! -f "$bin_dir/Photostacker.exe" ]; then
                line=$(find "$root" -iname 'Photostacker.exe' | sed -n 1p)
                fail "$name: expected the unpack to put Photostacker.exe in $(native "$bin_dir"), found ${line:-no Photostacker.exe in $(native "$root")}"
            fi
            lib_dir=$bin_dir res_dir=$bin_dir
            ok "unpacked with msiexec /a into $(native "$root"); nothing is installed"
            ;;
        windows:*-portable.zip)
            root="$scratch/zip"
            from=$(native "$package" | sed "s/'/''/g")
            to=$(native "$root" | sed "s/'/''/g")
            MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*' "$powershell" -NoProfile -NonInteractive -Command \
                "\$ErrorActionPreference = 'Stop'; Expand-Archive -LiteralPath '$from' -DestinationPath '$to'" ||
                fail "$name: expected Expand-Archive to unpack it, found it failed"
            bin_dir=$root lib_dir=$root res_dir=$root
            ok "unpacked with Expand-Archive into $(native "$root")"
            ;;
        *) fail "$name: expected a package for $os (.deb, .AppImage; -setup.exe, .msi, -portable.zip; .dmg), found $name" ;;
    esac

    check_contents
    if [ -n "$library_path" ]; then
        # linuxdeploy copies into the AppImage every library the executables link: a
        # libde265 there means they were built against the system's, not the pinned one.
        line=$(find "$root" -name 'libde265.so*' | sed -n 1p)
        if [ -n "$line" ]; then
            fail "contents: expected no libde265 library in the AppImage, as libde265 is built into the executables, found $line"
        fi
        ok "contents: no libde265 library in the AppImage"
    fi
    case "$os" in
        linux) check_links_linux ;;
        macos)
            check_links_macos
            check_minimum_macos
            ;;
        windows) check_links_windows ;;
    esac
    check_runs

    if [ -n "$mounted" ]; then
        detach "$mounted" || fail "$name: expected hdiutil to detach $mounted (tried twice, then with -force), found it failed"
        mounted=
        ok "detached"
    fi
    if [ -n "$nsis_dir" ]; then
        uninstall_nsis "$nsis_dir" || fail "$name: expected the silent uninstall to succeed, found exit status $?"
        if [ -e "$nsis_dir/Photostacker.exe" ]; then
            fail "$name: expected the uninstaller to remove Photostacker.exe, found it is still there"
        fi
        nsis_dir=
        if key_exists "$uninstall_key"; then
            fail "$name: expected the uninstaller to remove $uninstall_key, found it is still there"
        fi
        if [ -e "$start_menu" ]; then
            fail "$name: expected the uninstaller to remove the Start menu folder $(native "$start_menu"), found it is still there"
        fi
        nsis_forget || fail "$name: expected reg.exe to remove $product_key, which the uninstaller leaves, found it failed"
        ok "uninstalled silently; $uninstall_key and the Start menu folder are gone, and $product_key, which the uninstaller leaves, is removed"
    fi
    if [ "$deb_installed" = 1 ]; then
        $sudo apt-get remove -y photostacker >/dev/null || fail "$name: expected apt-get to remove it, found it failed"
        deb_installed=0
        ok "removed with apt-get"
    fi
done
echo "All checks passed."
