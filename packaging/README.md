# Packaging

The packages are built with [cargo-packager](https://github.com/crabnebula-dev/cargo-packager) 0.11.8, configured in `[package.metadata.packager]` of [crates/ui/Cargo.toml](../crates/ui/Cargo.toml). One script builds them on each system, [scripts/package.sh](../scripts/package.sh), and one checks them before anyone installs them, [scripts/smoke.sh](../scripts/smoke.sh). The version is the workspace's, in the root [Cargo.toml](../Cargo.toml).

## What ships where

| Target | Built on | Runs on | HEIC | ONNX Runtime | Visual check |
|---|---|---|---|---|---|
| Linux x86_64 (`.deb`, `.AppImage`) | Ubuntu 22.04 | glibc 2.35 or later | libheif built in; the system's `libde265.so.0` (a dependency of the `.deb`, inside the AppImage) | 1.31.1 | yes |
| Windows x86_64 (NSIS `.exe`, `.msi`, `.zip`) | Windows Server 2025 | Windows 10 or later | libheif and libde265 built in, from vcpkg | 1.31.1, with the Visual C++ runtime beside it | yes |
| macOS arm64 (`.dmg`) | macOS 15 | macOS 11 or later | libheif and libde265 built in | 1.31.1 | macOS 14 or later |
| macOS x86_64 (`.dmg`) | macOS 15 (Intel) | macOS 11 or later | libheif and libde265 built in | 1.19.2 | yes |

The ONNX Runtime of each system is the build [scripts/onnxruntime.tsv](../scripts/onnxruntime.tsv) pins. Microsoft's build for Apple silicon needs macOS 14, so on an older Mac the app runs without the visual check and says why (spec §8.3).

Every package holds the two executables, `Photostacker` (the desktop app) and `photostacker-cli`, and in its resource folder ONNX Runtime's library and a `licenses` folder with eight files (spec §11.3). The resource folder is where the app looks for them (spec §8.3):

| Package | Executables | ONNX Runtime | `licenses` |
|---|---|---|---|
| `.deb`, `.AppImage` | `usr/bin` | `usr/lib/Photostacker/libonnxruntime.so` | `usr/lib/Photostacker/licenses` |
| NSIS `.exe`, `.msi`, `.zip` | the app's folder | `onnxruntime.dll`, beside the executables | beside the executables |
| `.dmg` | `Photostacker.app/Contents/MacOS` | `Contents/Frameworks/libonnxruntime.dylib` | `Contents/Resources/licenses` |

The Windows packages also carry the Visual C++ runtime beside the executables (`msvcp140.dll`, `msvcp140_1.dll`, `vcruntime140.dll`, `vcruntime140_1.dll`), and the AppImage carries `libde265.so.0`. The NSIS installer installs for the current user, into `%LOCALAPPDATA%\Photostacker` unless told otherwise.

The packages land in `dist/packages`:

| Package | File |
|---|---|
| Debian package | `Photostacker_<version>_amd64.deb` (the package's name is `photostacker`) |
| AppImage | `Photostacker_<version>_x86_64.AppImage` |
| NSIS installer | `Photostacker_<version>_x64-setup.exe` |
| MSI | `Photostacker_<version>_x64_en-US.msi` |
| Portable zip | `Photostacker_<version>_x64-portable.zip`: the contents of the app's folder at the zip's root, with no folder around them |
| Disk image | `Photostacker_<version>_aarch64.dmg` on Apple silicon, `Photostacker_<version>_x64.dmg` on Intel |

## Building locally

The packages of a system are built on that system. Besides what [AGENTS.md](../AGENTS.md) lists for a build, the script needs:

```sh
cargo install cargo-packager --version 0.11.8 --locked
cargo install cargo-about --locked --features cli
```

and the network: it downloads ONNX Runtime's archive (once, into the cache folder of `scripts/ort.sh`), the licence files cargo-about fetches, and on first use cargo-packager's own tools (linuxdeploy and its AppImage plugin, NSIS, WiX, create-dmg).

```sh
just package deb,appimage    # Linux
just package dmg             # macOS
scripts/package.sh nsis,wix  # Windows, in Git Bash
```

On Windows, run `pwsh scripts/windows-heif.ps1` first, and set `VCPKG_ROOT` to the folder it prints on its last line: libheif comes from there. The Visual C++ runtime is copied from the redistributable folder of the newest Visual Studio installation that `vswhere.exe` finds: the highest version there that has all four files.

The script:

1. builds both executables (`cargo build --release --locked`), into `CARGO_TARGET_DIR` when it is set, else into `target/`;
2. stages in `dist/package` what the packages carry besides the executables: the `licenses` folder (`THIRD-PARTY-LICENSES.md` made by `scripts/licences.sh`, the script `just licences` runs), ONNX Runtime's library under its plain name, and on Windows the Visual C++ runtime;
3. runs `cargo packager` from `crates/ui`, which copies everything in `dist/package/resources` into the package's resource folder at the same paths, and on macOS `dist/package/frameworks/libonnxruntime.dylib` into `Contents/Frameworks`;
4. on Windows, zips the app's folder with PowerShell's `Compress-Archive`.

It empties `dist/package` and `dist/packages` first, so a package never carries a stale file: build all of a system's formats in one run.

## Checking a package

```sh
just smoke dist/packages/*.deb dist/packages/*.AppImage          # Linux
just smoke dist/packages/*.dmg                                   # macOS
scripts/smoke.sh dist/packages/*.zip dist/packages/*.exe dist/packages/*.msi  # Windows, in Git Bash
```

These install nothing. For each package, `scripts/smoke.sh` unpacks it in a scratch folder (`dpkg -x`, `--appimage-extract`, a read-only `hdiutil attach`, `Expand-Archive`) and checks it. The NSIS installer and the MSI are only checked to exist and not be empty: the portable zip holds the same files, and gets the full check. The checks:

- **contents:** both executables, ONNX Runtime's library and the eight licence files where the app looks for them; on Windows the Visual C++ runtime; in the AppImage `libde265.so.0`, which its executables load;
- **links:** on Linux each executable links only `libde265.so.0` and the C and C++ runtimes; on macOS only libraries under `/usr/lib/` and `/System/Library/`; on Windows no libheif, libde265 or x265 DLL is in the package, and the tool runs with only Windows' own folders on the `PATH`;
- **macOS:** every Mach-O file needs macOS 11.0, but ONNX Runtime on Apple silicon, which needs 14.0;
- **runs:** with `ORT_DYLIB_PATH` unset and a scratch data folder, `photostacker-cli info` names the version in the package's file name, libde265 as the HEVC decoder, and the ONNX Runtime pinned for the system, loaded from the package; a dry run over `crates/core/tests/fixtures/heic` reads every HEIC file there, with no error.

Each check prints one line; the first that fails says what it expected and what it found, and the script exits 1.

**`--install` changes the machine, and is meant for a throwaway one, such as a CI runner.** With it, the script installs the `.deb` with `apt-get` (as root, or through `sudo`) instead of unpacking it, and removes it afterwards; and it installs the NSIS installer silently into a scratch folder, checks the install, and uninstalls it. The NSIS installer ends a running Photostacker without asking and takes over what an earlier one left, so before it checks any package the script refuses when any trace of Photostacker is there for this user (the entry in the system's list of apps, the registry key `HKCU\Software\Riccardo Gabellone\Photostacker`, the Start menu folder) or Photostacker runs. After the uninstall it removes that registry key, which the uninstaller leaves behind (it holds the folder the next install would reuse), and checks that the uninstall entry and the Start menu folder are gone. The MSI is never installed: that needs elevation.

## Releasing

The release workflow is not written yet. The steps it is to take part in:

1. Bump `version` in `[workspace.package]` of the root `Cargo.toml` in a pull request, and merge it.
2. Tag the merge commit on `main` as `vX.Y.Z`, the new version, and push the tag.
3. The workflow builds every package on its system, checks each with `scripts/smoke.sh`, and drafts a GitHub release with them, their SHA-256 checksums and build-provenance attestations.
4. Download the packages from the draft and try them on each system.
5. Publish the draft.

## Desktop identity

The desktop matches a running window to its launcher and icon by the app ID, `io.github.riccardogabellone.Photostacker` (see [crates/ui/src/identity.rs](../crates/ui/src/identity.rs)). cargo-packager names its generated desktop entry and icons after the main binary instead, so that entry is turned off and the Linux packages ship the files in [linux/](linux/), named after the ID:

- `<ID>.desktop`, the launcher entry;
- `<ID>.metainfo.xml`, the AppStream description that software centres show (in the Debian package);
- the icons from [assets/icons/](../assets/icons/) as `hicolor/<size>/apps/<ID>.png`, and the SVG master [assets/logo/logo.svg](../assets/logo/logo.svg) as `scalable`.

The AppImage tooling looks for the desktop entry under the main binary's name, so the AppImage holds the same file as `Photostacker.desktop`. Tests in `identity.rs` check that the desktop entry, the AppStream file and the packager's configuration agree with the app's name, ID and executable.

## Not signed

The packages are not code-signed yet, so the first start asks for a step:

- **macOS** says the app is damaged or from an unidentified developer. Run `xattr -dr com.apple.quarantine /Applications/Photostacker.app`, or open it once and then allow it in System Settings, Privacy & Security, "Open Anyway".
- **Windows** SmartScreen warns about an unrecognised app: choose "More info", then "Run anyway".

Signing needs an Apple Developer ID (with notarisation) and a Windows code-signing certificate; cargo-packager has `macos.signing-identity` and `windows.sign-command` for them.

## The AppImage and FUSE

An AppImage mounts itself with FUSE. Where FUSE is missing (some minimal systems and containers), start it with `--appimage-extract-and-run`, which unpacks it into a temporary folder instead, or unpack it once with `--appimage-extract` and start `squashfs-root/AppRun`.
