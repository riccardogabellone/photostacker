#Requires -Version 7
# Builds the desktop app natively on Windows, so it can be tried by hand:
#
#   pwsh scripts/win.ps1
#
# It builds `Photostacker.exe` with the MSVC toolchain (stable-x86_64-pc-windows-msvc,
# which must be installed, and no older than the workspace's `rust-version`:
# `rustup update stable-x86_64-pc-windows-msvc` brings it up to date; the machine's
# default toolchain is left alone), with Stage 3
# (`ml`) and without HEIC, and puts it in `<repo>\dist\windows\` beside Microsoft's
# ONNX Runtime (`onnxruntime.dll`, which the app looks for beside its executable,
# spec §8.3), in the version that scripts\onnxruntime.tsv pins for Windows x64, and that
# runtime's licence files.
#
# The MSVC toolchain links with the Visual Studio Build Tools' "Desktop development with
# C++" workload: the MSVC compiler and linker, and the Windows SDK. Without the SDK, rustc
# finds no linker at all and says "linker `link.exe` not found", even where link.exe is
# installed.
#
# It only builds: the tests have been run on Linux only so far, through `just`
# (AGENTS.md). The build goes into %LOCALAPPDATA%\photostacker\cargo-target
# and ONNX Runtime into %LOCALAPPDATA%\photostacker\onnxruntime\<version> (where
# scripts/ort.sh puts it too, so the two share one download), never into the
# repository, which the script changes only under dist\; cargo also writes to its own home
# (%USERPROFILE%\.cargo: the registry of the crates it downloads), outside the repository
# too. Cargo.lock is used as it is (`--locked`). CARGO_TARGET_DIR is set for the build only:
# the caller's value, or its absence, is put back. The CI Windows runner stays the
# authoritative Windows build (spec §3).
#
# Before anything else it checks what would otherwise fail late: that Photostacker is not
# running from dist\windows (its locked executable would stop the copy half-way), that the
# MSVC toolchain is installed (so rustup never installs one on its own), and that the
# Windows SDK is there.

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
# Invoke-WebRequest's progress bar slows the download down a lot.
$ProgressPreference = 'SilentlyContinue'

$Toolchain = 'stable-x86_64-pc-windows-msvc'

# The version and SHA-256 of Microsoft's ONNX Runtime release for Windows x64: the
# `windows x86_64` row of scripts\onnxruntime.tsv, the table of pinned builds that
# scripts/ort.sh reads too (spec §8.3).
function Get-OnnxRuntimePin {
    $table = Join-Path $PSScriptRoot 'onnxruntime.tsv'
    foreach ($line in Get-Content -LiteralPath $table) {
        if ($line -match '^\s*(#|$)') {
            continue
        }
        $fields = $line -split "`t"
        if ($fields.Count -eq 6 -and $fields[0] -eq 'windows' -and $fields[1] -eq 'x86_64') {
            return [pscustomobject]@{ Version = $fields[2]; Sha256 = $fields[4] }
        }
    }
    throw "$table pins no ONNX Runtime build for windows x86_64"
}

$OrtPin = Get-OnnxRuntimePin
$OrtVersion = $OrtPin.Version
$OrtFolder = "onnxruntime-win-x64-$OrtVersion"
$OrtUrl = "https://github.com/microsoft/onnxruntime/releases/download/v$OrtVersion/$OrtFolder.zip"
$OrtSha256 = $OrtPin.Sha256

$Repo = Split-Path -Parent $PSScriptRoot
$DataRoot = Join-Path $env:LOCALAPPDATA 'photostacker'
$OrtDir = Join-Path $DataRoot "onnxruntime\$OrtVersion"
$OrtHome = Join-Path $OrtDir $OrtFolder
$OrtLibrary = Join-Path $OrtHome 'lib\onnxruntime.dll'
$TargetDir = Join-Path $DataRoot 'cargo-target'
$Exe = Join-Path $TargetDir 'release\Photostacker.exe'
$Dist = Join-Path $Repo 'dist\windows'

# What installs the Windows SDK, which the MSVC linker needs.
$SdkRemedy = 'Install the Visual Studio Build Tools'' "Desktop development with C++" workload, which brings the Windows SDK, then run this script again.'

# Stops when Photostacker runs from dist\windows: the build would replace its files, and
# Windows keeps a running executable locked, so the copy would stop half-way.
function Assert-NotRunning {
    $exe = Join-Path $Dist 'Photostacker.exe'
    $running = Get-Process -Name 'Photostacker' -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and ($_.Path -ieq $exe) }
    if ($running) {
        throw 'Close Photostacker first: dist\windows\Photostacker.exe is running.'
    }
}

# Stops unless the MSVC toolchain is installed: `cargo +<toolchain>` would make rustup
# install it on its own, unasked.
function Assert-Toolchain {
    $installed = rustup toolchain list | ForEach-Object { ($_ -split '\s+')[0] }
    if ($LASTEXITCODE -ne 0) {
        throw 'rustup cannot list its toolchains: is rustup installed?'
    }
    if ($installed -notcontains $Toolchain) {
        throw "The $Toolchain toolchain is not installed: run ``rustup toolchain install $Toolchain`` first."
    }
}

# Stops unless the Windows SDK is installed: without it rustc finds no linker, minutes into
# the build, and says "linker `link.exe` not found". The SDK is looked for where its
# installer says it is (the registry's `KitsRoot10`, which is where rustc looks too: an
# SDK can be installed outside Program Files) and in Program Files.
function Assert-WindowsSdk {
    $kits = @()
    $keys = @(
        'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows Kits\Installed Roots',
        'HKLM:\SOFTWARE\Microsoft\Windows Kits\Installed Roots'
    )
    foreach ($key in $keys) {
        try {
            $kits += Get-ItemPropertyValue -LiteralPath $key -Name 'KitsRoot10' -ErrorAction Stop
        }
        catch {
            # No such key or value: no SDK is recorded there.
        }
    }
    foreach ($root in @(${env:ProgramFiles(x86)}, $env:ProgramFiles)) {
        if ($root) {
            $kits += Join-Path $root 'Windows Kits\10'
        }
    }
    $found = $kits | Where-Object {
        $_ -and (Test-Path -LiteralPath (Join-Path $_ 'Lib') -PathType Container)
    }
    if (-not $found) {
        throw "The Windows SDK is not installed (no Windows Kits\10\Lib in Program Files, and none where the registry's KitsRoot10 points). $SdkRemedy"
    }
}

# Builds the app into $TargetDir, from the repository whatever the current folder is.
# CARGO_TARGET_DIR is the build's own: the caller's value, or its absence, is put back.
function Build-App {
    $callerTarget = $env:CARGO_TARGET_DIR
    $env:CARGO_TARGET_DIR = $TargetDir
    Push-Location -LiteralPath $Repo
    try {
        cargo "+$Toolchain" build --release --locked -p photostacker-ui --no-default-features --features ml | Out-Host
        # A native command's failure does not throw, whatever $ErrorActionPreference says.
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build failed with exit code $LASTEXITCODE"
        }
    }
    finally {
        Pop-Location
        if ($null -eq $callerTarget) {
            Remove-Item -LiteralPath 'Env:CARGO_TARGET_DIR' -ErrorAction SilentlyContinue
        }
        else {
            $env:CARGO_TARGET_DIR = $callerTarget
        }
    }
    if (-not (Test-Path -LiteralPath $Exe -PathType Leaf)) {
        throw "the build reported success but $Exe is not there"
    }
}

# Downloads and expands ONNX Runtime once. The archive goes into a temporary folder, its
# digest is checked, it is expanded there, and only then is the whole folder moved into
# place: an interrupted run or a wrong download leaves nothing that looks installed.
function Install-OnnxRuntime {
    if (Test-Path -LiteralPath $OrtLibrary -PathType Leaf) {
        Write-Host "ONNX Runtime $OrtVersion is at $OrtHome"
        return
    }
    New-Item -ItemType Directory -Force -Path $OrtDir | Out-Null
    $tmp = Join-Path $OrtDir ".extract.$([System.IO.Path]::GetRandomFileName())"
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        $archive = Join-Path $tmp "$OrtFolder.zip"
        Write-Host "Downloading $OrtUrl"
        Invoke-WebRequest -Uri $OrtUrl -OutFile $archive -MaximumRetryCount 3
        $digest = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($digest -ne $OrtSha256) {
            throw "the ONNX Runtime archive's SHA-256 is $digest, expected $OrtSha256; nothing was installed"
        }
        Expand-Archive -LiteralPath $archive -DestinationPath $tmp
        if (-not (Test-Path -LiteralPath (Join-Path $tmp "$OrtFolder\lib\onnxruntime.dll") -PathType Leaf)) {
            throw "the ONNX Runtime archive holds no $OrtFolder\lib\onnxruntime.dll"
        }
        # A folder without the library is no install: Move-Item would nest into it.
        if (Test-Path -LiteralPath $OrtHome) {
            Remove-Item -LiteralPath $OrtHome -Recurse -Force
        }
        Move-Item -LiteralPath (Join-Path $tmp $OrtFolder) -Destination $OrtHome
    }
    finally {
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
    Write-Host "ONNX Runtime $OrtVersion is at $OrtHome"
}

# Fills dist\windows with the executable, ONNX Runtime beside it and that runtime's
# licence files, under names that say whose they are, and nothing else.
function Copy-ToDist {
    if (Test-Path -LiteralPath $Dist) {
        Remove-Item -LiteralPath $Dist -Recurse -Force
    }
    New-Item -ItemType Directory -Force -Path $Dist | Out-Null
    Copy-Item -LiteralPath $Exe -Destination $Dist
    Copy-Item -LiteralPath $OrtLibrary -Destination $Dist
    Copy-Item -LiteralPath (Join-Path $OrtHome 'LICENSE') -Destination (Join-Path $Dist 'onnxruntime-LICENSE.txt')
    Copy-Item -LiteralPath (Join-Path $OrtHome 'ThirdPartyNotices.txt') -Destination (Join-Path $Dist 'onnxruntime-ThirdPartyNotices.txt')
}

# The checks, then ONNX Runtime: a missing tool, a running app, a failed download or a
# wrong digest stops the script in seconds, not after a build of many minutes.
Assert-NotRunning
Assert-Toolchain
Assert-WindowsSdk
Install-OnnxRuntime
Build-App
# Again: it may have been started during the build.
Assert-NotRunning
Copy-ToDist

$built = Get-Item -LiteralPath (Join-Path $Dist 'Photostacker.exe')
Write-Host ''
Write-Host "Built into $Dist"
Write-Host ('Photostacker.exe: {0:N1} MiB ({1:N0} bytes)' -f ($built.Length / 1MB), $built.Length)
