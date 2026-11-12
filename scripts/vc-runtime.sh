#!/usr/bin/env bash
# The Visual C++ runtime DLLs that Windows executables and libraries name (spec §11.3):
#
#   scripts/vc-runtime.sh names <file>...          the runtime DLLs the files name, one per
#                                                  line, in lower case, each once
#   scripts/vc-runtime.sh copy <folder> <file>...  copies those DLLs into <folder>, from the
#                                                  newest Visual Studio's redistributable folder
#   scripts/vc-runtime.sh check <folder>           fails unless <folder> holds every runtime
#                                                  DLL that its Photostacker.exe,
#                                                  photostacker-cli.exe and onnxruntime.dll name
#
# scripts/package.sh runs `copy` for the Windows packages, and scripts/smoke.sh runs `check`
# on each of them. Every development machine and CI runner has the whole Visual C++ runtime
# in System32, so a run there cannot show that a package lacks one of its DLLs; a clean
# Windows has none of them, and the app would not start there. A file names each DLL it
# imports, or loads late, as plain ASCII in its import tables, and no tool of Git Bash reads
# those tables, so the names are read as strings. The runtime's DLLs are those of the MSVC
# redistributable: msvcp*, vcruntime*, concrt* and vcomp*. Windows itself has DLLs that
# match those patterns (msvcp_win.dll, msvcp110_win.dll, the *_clr0400.dll of .NET), which
# no redistributable folder holds: names ending in _win.dll or _clr0400.dll are left out.
# Another runtime DLL's name met elsewhere in a file (in a message) adds a DLL to ship, or,
# if no redistributable version holds it, makes `copy` fail with the list it looked for.
#
# `copy` needs Visual Studio or its Build Tools with the C++ workload (vswhere.exe finds
# them). Run in Git Bash on Windows.
set -euo pipefail

fail() {
    echo "FAILED: $*" >&2
    exit 1
}

usage() {
    echo "usage: $0 names <file>... | $0 copy <folder> <file>... | $0 check <folder>" >&2
    exit 2
}

# The runtime DLLs that the files $@ name, sorted, lower case, each once.
names() {
    local file
    for file in "$@"; do
        if [ ! -f "$file" ] || [ ! -s "$file" ]; then
            fail "expected a file to read the Visual C++ runtime DLLs it names at $file, found none"
        fi
    done
    # grep exits 1 when the files name none, or none is left; that is for the caller to
    # judge. Windows' own DLLs that match (header) are dropped.
    { grep -a -o -h -i -E '(msvcp|vcruntime|concrt|vcomp)[0-9a-z_]*\.dll' "$@" || true; } |
        tr '[:upper:]' '[:lower:]' | { grep -v -E '_(win|clr0400)\.dll$' || true; } | sort -u
}

# Copies into $1 the runtime DLLs that the files after it name, from the redistributable
# folder of the newest Visual Studio installation with the C++ tools: the highest version
# there that has every one of them (in its x64 CRT folder, or OpenMP's for vcomp).
copy_runtime() {
    local dest=$1 needed program_files vswhere vs redist msvc dll file found= from
    shift
    needed=$(names "$@")
    if [ -z "$needed" ]; then
        fail "expected $* to name the Visual C++ runtime DLLs they need, found they name none"
    fi
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
        found=$msvc
        for dll in $needed; do
            from=
            for file in "$redist/$msvc"/x64/Microsoft.VC*.CRT/"$dll" "$redist/$msvc"/x64/Microsoft.VC*.OpenMP/"$dll"; do
                if [ -f "$file" ]; then
                    from=$file
                    break
                fi
            done
            if [ -z "$from" ]; then
                found=
                break
            fi
        done
        if [ -n "$found" ]; then
            break
        fi
    done
    if [ -z "$found" ]; then
        fail "no version in $redist has all of" $needed "in x64/Microsoft.VC*.CRT or x64/Microsoft.VC*.OpenMP: install the C++ workload of Visual Studio"
    fi
    for dll in $needed; do
        for file in "$redist/$found"/x64/Microsoft.VC*.CRT/"$dll" "$redist/$found"/x64/Microsoft.VC*.OpenMP/"$dll"; do
            if [ -f "$file" ]; then
                cp "$file" "$dest/$dll"
                break
            fi
        done
    done
    echo "Visual C++ runtime:" $needed "from $redist/$found" >&2
}

# Each of the folder's two executables and ONNX Runtime must have, in the folder, every
# runtime DLL it names: Windows looks for an executable's DLLs in its own folder first, and
# for ONNX Runtime's in the folder of the executable that loads it. Names are compared without
# regard to case, as Windows compares them.
check() {
    local folder=$1 present file dll needed
    if [ ! -d "$folder" ]; then
        fail "links: expected a folder at $folder, found none"
    fi
    present=$(ls -1 "$folder" | tr '[:upper:]' '[:lower:]')
    for file in Photostacker.exe photostacker-cli.exe onnxruntime.dll; do
        needed=$(names "$folder/$file")
        # Whatever the MSVC toolchain builds with the runtime as a DLL names vcruntime140.dll:
        # a file that names none means this reading no longer finds the names.
        if [ -z "$needed" ]; then
            fail "links: expected $file to name the Visual C++ runtime DLLs it needs, found it names none"
        fi
        for dll in $needed; do
            if ! printf '%s\n' "$present" | grep -Fqx "$dll"; then
                fail "links: expected $dll beside $file, which names it, found the package lacks it in $folder"
            fi
        done
        echo "ok: links: $file names" $needed", and the package has each beside it"
    done
}

case "${1:-}" in
    names)
        shift
        [ "$#" -gt 0 ] || usage
        names "$@"
        ;;
    copy)
        shift
        [ "$#" -gt 1 ] || usage
        copy_runtime "$@"
        ;;
    check)
        [ "$#" = 2 ] || usage
        check "$2"
        ;;
    *) usage ;;
esac
