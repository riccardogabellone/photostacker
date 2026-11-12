#Requires -Version 7
# Installs libheif and libde265 for the Windows build, from vcpkg:
#
#   pwsh scripts/windows-heif.ps1 [-Root <folder>]
#
# With the MSVC toolchain, libheif-sys never builds libheif with cmake, whatever its
# `embedded-libheif` feature says: it always takes the libraries from vcpkg, through the
# `vcpkg` crate, which finds vcpkg in VCPKG_ROOT and takes the triplet x64-windows-static-md
# (static libraries over the dynamic C runtime). This script owns that vcpkg: a checkout of
# the one commit pinned below (whose ports are libheif 1.23.7 and libde265 1.1.3), by default
# in %LOCALAPPDATA%\photostacker\vcpkg, into which it installs
# `libheif[core,libde265]:x64-windows-static-md`. `core` turns the port's default features
# off: they include `hevc`, the x265 encoder, which is GPL. x265 is never kept: the script
# removes it if an earlier install left it, and fails if it is still there at the end. A
# libheif built with other features is removed and built again, so libde265 is its only
# codec, as on the other systems (spec §3).
#
# The root is a folder of the script's own: one that does not exist yet, an empty one, or
# one this script made before, which it marks with the file `.git\photostacker-vcpkg`
# before it puts anything else there. A run stopped while writing the mark can leave a root
# holding only an empty `.git` folder, and that is taken back too. Any other folder
# (someone's own vcpkg, a folder of files) is refused untouched: the script rewrites its
# checkout by force and deletes what vcpkg installed there.
#
# The first run needs git and the network: it fetches the pinned commit (that commit only,
# not vcpkg's history), bootstraps vcpkg (which downloads vcpkg.exe), and builds the two
# libraries with the MSVC compiler, which takes a few minutes. A later run with all of that
# in place checks it and does nothing else. The last line printed is the root, and it is the
# script's output (what it says on the way is written to the host), so a caller can capture
# it: `$root = & scripts/windows-heif.ps1` in PowerShell, or from another process, whose
# stdout gets the host's lines too, the last line of `pwsh scripts/windows-heif.ps1`.
#
# Nothing is written into the repository: vcpkg writes under its root, and its binary cache
# under %LOCALAPPDATA%\vcpkg. vcpkg runs with the root as its current folder and without
# CMAKE_TOOLCHAIN_FILE: the repository's .cargo\config.toml sets one for cargo's builds,
# and it must not reach vcpkg's own.

param(
    # Where the vcpkg checkout lives: a folder of this script's own, outside the repository.
    [string] $Root = (Join-Path $env:LOCALAPPDATA 'photostacker\vcpkg')
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# The pinned vcpkg commit. Its ports/libheif/vcpkg.json and ports/libde265/vcpkg.json give
# the two versions below: to move the pin, choose a commit, read those two files, and change
# the three constants together.
$VcpkgCommit = 'b58529e33332fbb6dfe20317a82009b78ceeab36'
$LibheifVersion = '1.23.7'
$Libde265Version = '1.1.3'
$VcpkgUrl = 'https://github.com/microsoft/vcpkg'
$Triplet = 'x64-windows-static-md'
$Package = "libheif[core,libde265]:$Triplet"
# What each library must be in vcpkg's list: its version, and its features beyond `core`,
# sorted and comma separated.
$Wanted = @{
    libheif  = @{ Version = $LibheifVersion; Features = 'libde265' }
    libde265 = @{ Version = $Libde265Version; Features = '' }
}

$Repo = Split-Path -Parent $PSScriptRoot
# A relative -Root is taken from PowerShell's current folder, not the process's.
$Root = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Root)
$Vcpkg = Join-Path $Root 'vcpkg.exe'
# The file that marks a root as this script's: inside .git, so no checkout touches it.
$Mark = Join-Path $Root '.git\photostacker-vcpkg'
# What to do when the root this script made is in a state it cannot mend.
$Remedy = "delete the folder $Root (it holds only what this script put there), then run this script again."

# Stops when $Root is the repository or inside it: vcpkg writes gigabytes under its root.
function Assert-OutsideRepository {
    $repo = [System.IO.Path]::GetFullPath($Repo).TrimEnd('\') + '\'
    $root = $Root.TrimEnd('\') + '\'
    if ($root.StartsWith($repo, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "The vcpkg root $Root is inside the repository: pass -Root with a folder outside it."
    }
}

# True when $Root holds nothing but an empty `.git` folder that is a folder itself, not a
# junction or a link: the one unmarked state a run of this script leaves besides an empty
# root, when it stopped after making `.git` for the mark and before writing the mark into
# it. The script writes the mark before anything else in the root, so nothing else
# unmarked can be its own. No git command decides it: a `.git` that git cannot read is someone else's too.
function Test-InterruptedStart {
    $entries = @(Get-ChildItem -LiteralPath $Root -Force)
    if ($entries.Count -ne 1) {
        return $false
    }
    $git = $entries[0]
    if ($git.Name -ne '.git' -or -not $git.PSIsContainer) {
        return $false
    }
    if ($git.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
        return $false
    }
    try {
        return @(Get-ChildItem -LiteralPath $git.FullName -Force).Count -eq 0
    }
    catch {
        # A `.git` whose entries cannot be listed is not known to be empty.
        return $false
    }
}

# Stops unless $Root is this script's to use: missing, an empty folder, marked as made by
# it, or holding only the empty `.git` folder that a run of it stopped before the mark
# leaves. Nothing in a refused folder is touched.
function Assert-OwnRoot {
    if (-not (Test-Path -LiteralPath $Root)) {
        return
    }
    if (-not (Test-Path -LiteralPath $Root -PathType Container)) {
        throw "$Root is a file, not a folder: pass -Root with a folder that does not exist yet, or an empty one."
    }
    if (Test-Path -LiteralPath $Mark -PathType Leaf) {
        return
    }
    if (Test-InterruptedStart) {
        return
    }
    if (Get-ChildItem -LiteralPath $Root -Force | Select-Object -First 1) {
        throw "$Root holds files or folders this script did not put there, and nothing in it was changed: pass -Root with a folder that does not exist yet, an empty one, or the one this script made before."
    }
}

# Stops unless git is on the PATH: the checkout is made with it.
function Assert-Git {
    if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
        throw 'git is not on the PATH: install Git for Windows, then run this script again.'
    }
}

# Runs git with $arguments in $Root, and stops if it fails, saying what it was doing.
function Invoke-Git([string] $doing, [string[]] $arguments) {
    git -C $Root @arguments | Out-Host
    # A native command's failure does not throw, whatever $ErrorActionPreference says.
    if ($LASTEXITCODE -ne 0) {
        throw "git could not $doing (exit code $LASTEXITCODE): check the network and run this script again; if it fails again, $Remedy"
    }
}

# The commit $Root's checkout is at, or $null when there is none.
function Get-CheckedOutCommit {
    if (-not (Test-Path -LiteralPath (Join-Path $Root '.git') -PathType Container)) {
        return $null
    }
    $commit = git -C $Root rev-parse --verify --quiet HEAD 2>$null
    if ($LASTEXITCODE -ne 0) {
        return $null
    }
    return $commit
}

# Gives the environment variable $name back the $value it had, or removes it when it had
# none ($null). Not SetEnvironmentVariable with $null: PowerShell passes it as "", and .NET
# then keeps an empty variable, which cargo and vcpkg read as set.
function Restore-Variable([string] $name, $value) {
    if ($null -eq $value) {
        Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue
    }
    else {
        [System.Environment]::SetEnvironmentVariable($name, $value)
    }
}

# Runs vcpkg with $arguments in $Root, with VCPKG_ROOT at $Root and without
# CMAKE_TOOLCHAIN_FILE; the caller's values, or their absence, are put back. It prints what
# vcpkg says, or with -Capture returns it, and stops if vcpkg fails, saying what it was doing.
function Invoke-Vcpkg([string] $doing, [string[]] $arguments, [switch] $Capture) {
    $saved = @{}
    foreach ($name in 'VCPKG_ROOT', 'CMAKE_TOOLCHAIN_FILE') {
        $saved[$name] = [System.Environment]::GetEnvironmentVariable($name)
    }
    $env:VCPKG_ROOT = $Root
    Remove-Item -LiteralPath 'Env:CMAKE_TOOLCHAIN_FILE' -ErrorAction SilentlyContinue
    Push-Location -LiteralPath $Root
    try {
        if ($Capture) {
            $output = & $Vcpkg @arguments
        }
        else {
            & $Vcpkg @arguments | Out-Host
        }
        if ($LASTEXITCODE -ne 0) {
            throw "vcpkg could not $doing (exit code $LASTEXITCODE): read its messages above, fix what they name, then run this script again."
        }
    }
    finally {
        Pop-Location
        foreach ($name in $saved.Keys) {
            Restore-Variable $name $saved[$name]
        }
    }
    if ($Capture) {
        return $output
    }
}

# What vcpkg has installed in $Root: one entry per package and triplet, with its
# package_name, triplet, version and features. Nothing before the bootstrap.
function Get-Installed {
    $entries = [System.Collections.Generic.List[object]]::new()
    if (Test-Path -LiteralPath $Vcpkg -PathType Leaf) {
        $json = Invoke-Vcpkg 'list what is installed' @('list', '--x-json') -Capture
        foreach ($listed in ($json -join "`n" | ConvertFrom-Json).PSObject.Properties) {
            $entries.Add($listed.Value)
        }
    }
    # The comma returns the list as one object: PowerShell would unroll it, to nothing
    # when it is empty.
    return , $entries
}

# The x265 entries of $installed, whatever their triplet.
function Find-X265($installed) {
    return , $installed.Where({ $_.package_name -eq 'x265' })
}

# True when the vcpkg list entry $entry is the build of its library that $Wanted asks for.
function Test-Wanted($entry) {
    $wanted = $Wanted[$entry.package_name]
    $features = (@($entry.features) | Sort-Object) -join ','
    return $entry.triplet -eq $Triplet -and $entry.version -eq $wanted.Version -and $features -eq $wanted.Features
}

# The entries of $installed for libheif or libde265 on $Triplet that are not the build
# $Wanted asks for: another version, or other features.
function Find-Unwanted($installed) {
    return , $installed.Where({
            $Wanted.ContainsKey($_.package_name) -and $_.triplet -eq $Triplet -and -not (Test-Wanted $_)
        })
}

# True when $installed holds libheif and libde265 as $Wanted asks, and no x265.
function Test-Complete($installed) {
    foreach ($name in $Wanted.Keys) {
        $found = $installed.Where({ $_.package_name -eq $name -and (Test-Wanted $_) })
        if ($found.Count -eq 0) {
            return $false
        }
    }
    return (Find-X265 $installed).Count -eq 0
}

# Makes $Root a checkout of $VcpkgCommit with vcpkg.exe bootstrapped. Returns $true when the
# checkout moved from another commit, whose installed packages are then not this pin's.
function Update-Checkout {
    $current = Get-CheckedOutCommit
    $moved = $false
    if ($current -ne $VcpkgCommit) {
        Write-Host "Fetching vcpkg $VcpkgCommit into $Root"
        New-Item -ItemType Directory -Force -Path $Root | Out-Null
        # The mark comes first, so a run that stops from here on leaves a marked root (or
        # only an empty .git, which Assert-OwnRoot takes back too). Then a repository and
        # the remote: `git init` keeps what is there, so each run can mend a stopped one.
        if (-not (Test-Path -LiteralPath $Mark -PathType Leaf)) {
            New-Item -ItemType File -Force -Path $Mark -Value 'A vcpkg checkout of scripts/windows-heif.ps1 (Photostacker).' | Out-Null
        }
        Invoke-Git 'make a repository' @('init', '--quiet')
        $origin = git -C $Root remote get-url origin 2>$null
        if ($LASTEXITCODE -ne 0) {
            Invoke-Git 'add the vcpkg remote' @('remote', 'add', 'origin', $VcpkgUrl)
        }
        elseif ($origin -ne $VcpkgUrl) {
            Invoke-Git 'set the vcpkg remote' @('remote', 'set-url', 'origin', $VcpkgUrl)
        }
        Invoke-Git "fetch vcpkg's commit $VcpkgCommit" @('fetch', '--depth', '1', 'origin', $VcpkgCommit)
        Invoke-Git "check out vcpkg's commit $VcpkgCommit" @('-c', 'advice.detachedHead=false', 'checkout', '--force', 'FETCH_HEAD')
        $moved = $null -ne $current
    }
    # Each commit names the vcpkg.exe it works with: a new commit bootstraps again.
    if ($moved -or -not (Test-Path -LiteralPath $Vcpkg -PathType Leaf)) {
        Write-Host 'Bootstrapping vcpkg'
        & (Join-Path $Root 'bootstrap-vcpkg.bat') -disableMetrics | Out-Host
        if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $Vcpkg -PathType Leaf)) {
            throw "vcpkg's bootstrap failed (exit code $LASTEXITCODE): check the network, then run this script again."
        }
    }
    return $moved
}

Assert-OutsideRepository
Assert-Git
Assert-OwnRoot

if (Update-Checkout) {
    # Built from another commit's ports: build them again from this pin's.
    Write-Host 'The vcpkg pin moved: removing what the earlier commit installed'
    Remove-Item -LiteralPath (Join-Path $Root 'installed') -Recurse -Force -ErrorAction SilentlyContinue
}

$installed = Get-Installed
if (-not (Test-Complete $installed)) {
    foreach ($entry in (Find-X265 $installed)) {
        # Left by an install with the port's default features: x265 goes, and with it what
        # depends on it (libheif with `hevc`), which is built again without it.
        Write-Host "Removing x265:$($entry.triplet), which is GPL, and what depends on it"
        Invoke-Vcpkg 'remove x265' @('remove', '--classic', '--recurse', "x265:$($entry.triplet)")
    }
    # Another version or other codecs: vcpkg would keep such a build as it is, so it goes
    # first, one at a time, since removing libde265 takes libheif with it.
    $unwanted = Find-Unwanted (Get-Installed)
    while ($unwanted.Count -gt 0) {
        $entry = $unwanted[0]
        $spec = "$($entry.package_name):$($entry.triplet)"
        Write-Host "Removing $spec $($entry.version) [$(@($entry.features) -join ',')], to build it as pinned"
        Invoke-Vcpkg "remove $spec" @('remove', '--classic', '--recurse', $spec)
        $unwanted = Find-Unwanted (Get-Installed)
    }
    Write-Host "Installing $Package (the first build takes a few minutes)"
    Invoke-Vcpkg "install $Package" @('install', '--classic', '--clean-after-build', $Package)
    $installed = Get-Installed
}

Write-Host ''
Invoke-Vcpkg 'list what is installed' @('list')
if ((Find-X265 $installed).Count -ne 0) {
    throw "x265, a GPL encoder, is installed in $Root and must never be linked: $Remedy"
}
if (-not (Test-Complete $installed)) {
    throw "vcpkg in $Root does not hold libheif $LibheifVersion with libde265 as its only codec, and libde265 $Libde265Version, for ${Triplet}: $Remedy"
}
Write-Output $Root
