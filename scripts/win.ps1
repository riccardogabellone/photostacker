#Requires -Version 7
# Builds the desktop app and the command-line tool natively on Windows, runs the tests there,
# or saves a screen of the app:
#
#   pwsh scripts/win.ps1                         # build both into <repo>\dist\windows
#   pwsh scripts/win.ps1 -Test [nextest arguments]
#   pwsh scripts/win.ps1 -Screenshot <out.png> <screen> [dark|light] [width] [height]
#
# Plain, it builds `Photostacker.exe` and `photostacker-cli.exe` in release mode with the MSVC
# toolchain (stable-x86_64-pc-windows-msvc, which must be installed, and no older than the
# workspace's `rust-version`: `rustup update stable-x86_64-pc-windows-msvc` brings it up to
# date; the machine's default toolchain is left alone), with the default features, Stage 3
# (`ml`) and HEIC among them, and puts them in `<repo>\dist\windows\` beside Microsoft's
# ONNX Runtime (`onnxruntime.dll`, which both look for beside their executable, spec §8.3), in
# the version that scripts\onnxruntime.tsv pins for Windows x64, and that runtime's licence
# files.
#
# -Test runs `cargo nextest run --workspace --locked` with the arguments that follow it (as in
# `pwsh scripts/win.ps1 -Test -E 'test(heic)'`), with ONNX Runtime's library in
# ORT_DYLIB_PATH, so the tests that run a model run. It needs cargo-nextest:
# `cargo +stable-x86_64-pc-windows-msvc install cargo-nextest --locked`. -Screenshot runs the
# screenshot example (`just screenshot` elsewhere); a relative <out.png> is taken from the
# repository's folder, as `just` takes it. Neither touches dist\windows.
#
# HEIC: with the MSVC toolchain, libheif-sys takes libheif and libde265 from vcpkg. The
# script first runs scripts\windows-heif.ps1, which keeps a vcpkg checkout at a pinned commit
# with the two libraries built (the first time it needs git and the network, and takes a few
# minutes; then it takes a second), and passes its root to the build in VCPKG_ROOT.
#
# The MSVC toolchain links with the Visual Studio Build Tools' "Desktop development with
# C++" workload: the MSVC compiler and linker, and the Windows SDK. Without the SDK, rustc
# finds no linker at all and says "linker `link.exe` not found", even where link.exe is
# installed. The SDK's resource compiler embeds the app's icon, which the build requires
# (REQUIRE_WINDOWS_ICON=1).
#
# Every cargo command goes into %LOCALAPPDATA%\photostacker\cargo-target, ONNX Runtime into
# %LOCALAPPDATA%\photostacker\onnxruntime\<version> (where scripts/ort.sh puts it too, so the
# two share one download) and vcpkg into %LOCALAPPDATA%\photostacker\vcpkg, never into the
# repository, which the script changes only under dist\; cargo also writes to its own home
# (%USERPROFILE%\.cargo: the registry of the crates it downloads), outside the repository
# too. Cargo.lock is used as it is (`--locked`). Cargo runs from the repository's folder, so
# it reads .cargo\config.toml. The variables the script sets (CARGO_TARGET_DIR, VCPKG_ROOT,
# ORT_DYLIB_PATH, REQUIRE_WINDOWS_ICON) are set for cargo only: the caller's values, or
# their absence, are put back. The CI Windows runner stays the authoritative Windows build
# (spec §3).
#
# Before anything else it checks what would otherwise fail late: that neither executable is
# running from dist\windows (a locked executable would stop the copy half-way), that the
# MSVC toolchain is installed (so rustup never installs one on its own), that the Windows
# SDK is there, and for -Test that cargo-nextest is.

param(
    # Run the tests instead of building; the arguments after it go to nextest.
    [switch] $Test,
    # Save a screen of the app instead of building; the arguments after it go to the example.
    [switch] $Screenshot
)

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
# The executables the build makes, by the name of their process.
$Executables = [ordered]@{
    'Photostacker'     = Join-Path $TargetDir 'release\Photostacker.exe'
    'photostacker-cli' = Join-Path $TargetDir 'release\photostacker-cli.exe'
}
$Dist = Join-Path $Repo 'dist\windows'

# What installs the Windows SDK, which the MSVC linker needs.
$SdkRemedy = 'Install the Visual Studio Build Tools'' "Desktop development with C++" workload, which brings the Windows SDK, then run this script again.'

# Stops unless the arguments fit the switches: at most one switch, and arguments only after one.
function Assert-Arguments([object[]] $arguments) {
    if ($Test -and $Screenshot) {
        throw 'Pass -Test or -Screenshot, not both.'
    }
    if (-not ($Test -or $Screenshot) -and $arguments.Count -gt 0) {
        throw "Arguments go after -Test or -Screenshot; a build takes none (got: $arguments)."
    }
    if ($Screenshot -and $arguments.Count -lt 2) {
        throw 'Usage: pwsh scripts/win.ps1 -Screenshot <out.png> <screen> [dark|light] [width] [height]'
    }
}

# Stops when either executable runs from dist\windows: the build would replace its files,
# and Windows keeps a running executable locked, so the copy would stop half-way.
function Assert-NotRunning {
    foreach ($name in $Executables.Keys) {
        $exe = Join-Path $Dist "$name.exe"
        $running = Get-Process -Name $name -ErrorAction SilentlyContinue |
            Where-Object { $_.Path -and ($_.Path -ieq $exe) }
        if ($running) {
            throw "Close $name first: dist\windows\$name.exe is running."
        }
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

# Stops unless cargo-nextest is installed, which runs the tests.
function Assert-Nextest {
    if (-not (Get-Command cargo-nextest -ErrorAction SilentlyContinue)) {
        throw "cargo-nextest is not installed: run ``cargo +$Toolchain install cargo-nextest --locked``, then run this script again."
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

# libheif and libde265 in vcpkg (scripts\windows-heif.ps1, which stops with its own sentence
# when it cannot); returns vcpkg's root.
function Install-Heif {
    $root = & (Join-Path $PSScriptRoot 'windows-heif.ps1')
    Write-Host "libheif and libde265 are in vcpkg at $root"
    return $root
}

# Gives the environment variable $name back the $value it had, or removes it when it had
# none ($null). Not SetEnvironmentVariable with $null: PowerShell passes it as "", and .NET
# then keeps an empty variable, which cargo reads as set (an empty target folder, or an
# empty ORT_DYLIB_PATH for whatever the session starts next).
function Restore-Variable([string] $name, $value) {
    if ($null -eq $value) {
        Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue
    }
    else {
        [System.Environment]::SetEnvironmentVariable($name, $value)
    }
}

# Runs cargo with $arguments, from the repository whatever the current folder is, with the
# script's variables set for it alone: the caller's values, or their absence, are put back.
# Returns cargo's exit code.
function Invoke-Cargo([string] $vcpkgRoot, [string[]] $arguments) {
    $variables = [ordered]@{
        CARGO_TARGET_DIR     = $TargetDir
        VCPKG_ROOT           = $vcpkgRoot
        ORT_DYLIB_PATH       = $OrtLibrary
        REQUIRE_WINDOWS_ICON = '1'
    }
    $saved = @{}
    foreach ($name in $variables.Keys) {
        $saved[$name] = [System.Environment]::GetEnvironmentVariable($name)
        [System.Environment]::SetEnvironmentVariable($name, $variables[$name])
    }
    Push-Location -LiteralPath $Repo
    try {
        cargo "+$Toolchain" @arguments | Out-Host
        return $LASTEXITCODE
    }
    finally {
        Pop-Location
        foreach ($name in $saved.Keys) {
            Restore-Variable $name $saved[$name]
        }
    }
}

# Builds both executables into $TargetDir.
function Build-App([string] $vcpkgRoot) {
    $code = Invoke-Cargo $vcpkgRoot @('build', '--release', '--locked', '-p', 'photostacker-ui', '-p', 'photostacker-cli')
    # A native command's failure does not throw, whatever $ErrorActionPreference says.
    if ($code -ne 0) {
        throw "cargo build failed with exit code $code"
    }
    foreach ($exe in $Executables.Values) {
        if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
            throw "the build reported success but $exe is not there"
        }
    }
}

# Fills dist\windows with the executables, ONNX Runtime beside them and that runtime's
# licence files, under names that say whose they are, and nothing else.
function Copy-ToDist {
    if (Test-Path -LiteralPath $Dist) {
        Remove-Item -LiteralPath $Dist -Recurse -Force
    }
    New-Item -ItemType Directory -Force -Path $Dist | Out-Null
    foreach ($exe in $Executables.Values) {
        Copy-Item -LiteralPath $exe -Destination $Dist
    }
    Copy-Item -LiteralPath $OrtLibrary -Destination $Dist
    Copy-Item -LiteralPath (Join-Path $OrtHome 'LICENSE') -Destination (Join-Path $Dist 'onnxruntime-LICENSE.txt')
    Copy-Item -LiteralPath (Join-Path $OrtHome 'ThirdPartyNotices.txt') -Destination (Join-Path $Dist 'onnxruntime-ThirdPartyNotices.txt')
}

# The checks, then ONNX Runtime and vcpkg: a wrong argument, a missing tool, a running app, a
# failed download or a wrong digest stops the script in seconds, not after a build of many
# minutes.
Assert-Arguments $args
if (-not ($Test -or $Screenshot)) {
    Assert-NotRunning
}
Assert-Toolchain
Assert-WindowsSdk
if ($Test) {
    Assert-Nextest
}
Install-OnnxRuntime
$vcpkgRoot = Install-Heif

if ($Test) {
    exit (Invoke-Cargo $vcpkgRoot (@('nextest', 'run', '--workspace', '--locked') + $args))
}
if ($Screenshot) {
    exit (Invoke-Cargo $vcpkgRoot (@('run', '--release', '--locked', '-p', 'photostacker-ui', '--example', 'screenshot', '--') + $args))
}

Build-App $vcpkgRoot
# Again: one may have been started during the build.
Assert-NotRunning
Copy-ToDist

Write-Host ''
Write-Host "Built into $Dist"
foreach ($name in $Executables.Keys) {
    $built = Get-Item -LiteralPath (Join-Path $Dist "$name.exe")
    Write-Host ('{0}.exe: {1:N1} MiB ({2:N0} bytes)' -f $name, ($built.Length / 1MB), $built.Length)
}
