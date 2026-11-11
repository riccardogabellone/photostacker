//! Where ONNX Runtime's shared library is, whether this system can load it,
//! and the version of the one loaded (spec §8.3). Only the `ml` feature
//! builds it.
//!
//! The library is looked at before it is loaded, so that one the system
//! cannot load is refused in plain words, not in the system loader's: on
//! macOS a library can need a newer macOS than the running one (Microsoft's
//! build for Apple silicon needs macOS 14, while the app runs on macOS 11).
//! That minimum is read from the library's Mach-O headers, and the running
//! version from the system's own `SystemVersion.plist`.

use std::env;
use std::env::consts::{ARCH, DLL_PREFIX, DLL_SUFFIX};
use std::ffi::{CStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::ModelError;
use crate::package;

/// Where ONNX Runtime's shared library is loaded from, the one path of it
/// that the process's environment (`models::embed`), the app's look before
/// its first frame and every message name: `ORT_DYLIB_PATH` when set and not
/// empty; else the platform's file name (`libonnxruntime.so`,
/// `onnxruntime.dll`, `libonnxruntime.dylib`) in the first of the package's
/// library folders that holds it ([`package::library_folders`]); else that
/// name beside the executable, so a message names the first place looked in.
/// It is made absolute against the working folder, and found once: then the
/// same for the life of the process, so what names the loaded library
/// ([`loaded_version`], `photostacker-cli info`) names the one the
/// environment was made from. Asking loads nothing.
pub fn environment_library() -> &'static Path {
    static LIBRARY: OnceLock<PathBuf> = OnceLock::new();
    LIBRARY.get_or_init(|| {
        let folders = package::exe_dir()
            .map(|exe_dir| package::library_folders(&exe_dir))
            .unwrap_or_default();
        let library = find_library(env::var_os("ORT_DYLIB_PATH"), &folders);
        std::path::absolute(&library).unwrap_or(library)
    })
}

/// ONNX Runtime's own version string (`1.31.1`), as the loaded library says
/// it. The process's environment is made first, as a model load makes it,
/// so a library that cannot be used gives the same error a model load would
/// ([`ModelError::Runtime`]: not found, needing a newer macOS, or the
/// loader's reason).
pub fn loaded_version() -> Result<String, ModelError> {
    super::embed::environment()?;
    let library = environment_library();
    version_of_loaded(library).map_err(|reason| {
        ModelError::Runtime(format!(
            "ONNX Runtime at {} did not give its version: {reason}",
            library.display()
        ))
    })
}

/// `OrtGetApiBase`, the entry point ONNX Runtime's library exports, as
/// `onnxruntime_c_api.h` declares it (`ORT_API_CALL`: the system's calling
/// convention) and `ort::sys` does too.
type GetApiBase = unsafe extern "system" fn() -> *const ort::sys::OrtApiBase;

/// The version string of the ONNX Runtime library at `library`, which the
/// process's environment was made from and so is loaded already. It is
/// opened a second time by that same path: the system hands back the image
/// it has loaded (one more reference to it), so nothing is loaded again.
/// `ort` keeps its own handle to itself, and its API table does not hold the
/// version, only the API base does.
fn version_of_loaded(library: &Path) -> Result<String, String> {
    // SAFETY: the environment exists and was made from `library`, so the
    // library at that path is ONNX Runtime and this process has loaded it:
    // opening it again returns the loaded image and runs no initialiser a
    // second time. `OrtGetApiBase` is its documented entry point, of the
    // signature `GetApiBase` gives; `OrtApiBase`'s layout is `ort::sys`'s,
    // generated from the same header. The base pointer and the string
    // pointer are checked for null before they are read. The string is a
    // static of the library, which stays loaded for the life of the process
    // because `ort` holds it; besides, `handle` keeps it loaded until the end
    // of this block, and the string is copied into a `String` (checked as
    // UTF-8) before `handle` is dropped.
    unsafe {
        let handle = libloading::Library::new(library).map_err(|error| error.to_string())?;
        let get_api_base = handle
            .get::<GetApiBase>(b"OrtGetApiBase")
            .map_err(|_| "it has no OrtGetApiBase".to_owned())?;
        let base = get_api_base();
        if base.is_null() {
            return Err("OrtGetApiBase gave a null pointer".into());
        }
        let version = ((*base).GetVersionString)();
        if version.is_null() {
            return Err("GetVersionString gave a null pointer".into());
        }
        CStr::from_ptr(version)
            .to_str()
            .map(str::to_owned)
            .map_err(|_| "its version is not UTF-8".to_owned())
    }
}

/// [`environment_library`] before it is made absolute, given
/// `ORT_DYLIB_PATH`'s value and the folders to look
/// in, in order.
fn find_library(ort_dylib_path: Option<OsString>, folders: &[PathBuf]) -> PathBuf {
    if let Some(path) = ort_dylib_path.filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    let name = format!("{DLL_PREFIX}onnxruntime{DLL_SUFFIX}");
    folders
        .iter()
        .map(|folder| folder.join(&name))
        .find(|path| path.is_file())
        .or_else(|| folders.first().map(|folder| folder.join(&name)))
        .unwrap_or_else(|| PathBuf::from(name))
}

/// A version of macOS: `14.0`, `13.6.1`. Versions are ordered by their
/// numbers, the major one first.
///
/// The UI's stand-in for a build without `ml` mirrors it: a change here is
/// made there too (`crates/ui/src/controller/models.rs`, `without_ml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OsVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl OsVersion {
    /// The version a Mach-O version word holds: `major << 16 | minor << 8 |
    /// patch`.
    fn from_word(word: u32) -> OsVersion {
        OsVersion {
            major: word >> 16,
            minor: (word >> 8) & 0xFF,
            patch: word & 0xFF,
        }
    }
}

/// `14.0`, or `13.6.1` when there is a patch number.
impl fmt::Display for OsVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)?;
        if self.patch != 0 {
            write!(f, ".{}", self.patch)?;
        }
        Ok(())
    }
}

/// Why ONNX Runtime's library cannot be loaded, found before loading it.
///
/// The UI's stand-in for a build without `ml` mirrors it: a change here is
/// made there too (`crates/ui/src/controller/models.rs`, `without_ml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryProblem {
    /// There is no file at its path.
    Missing,
    /// It needs a newer macOS than the running one.
    NeedsMacos {
        needed: OsVersion,
        running: OsVersion,
    },
}

impl LibraryProblem {
    /// Why the library at `library` is not loaded, as the embedder says it.
    pub(crate) fn refusal(&self, library: &Path) -> String {
        match self {
            LibraryProblem::Missing => {
                format!("ONNX Runtime not found at {}", library.display())
            }
            LibraryProblem::NeedsMacos { needed, running } => format!(
                "ONNX Runtime at {} needs macOS {needed} or later; this is macOS {running}",
                library.display()
            ),
        }
    }
}

/// What keeps the library at `library` from loading, as far as can be told
/// without loading it: [`LibraryProblem::Missing`] when it is not a file; on
/// macOS [`LibraryProblem::NeedsMacos`] when its minimum macOS, for this
/// architecture, is above the running one. Otherwise `None`, also when
/// either version cannot be read, or the running one is `10.16`, which a
/// process in compatibility mode reads on any newer macOS: the load
/// is then tried, and its failure, if any, is the loader's.
pub fn library_problem(library: &Path) -> Option<LibraryProblem> {
    if !library.is_file() {
        return Some(LibraryProblem::Missing);
    }
    let running = running_macos().and_then(known_macos)?;
    let needed = File::open(library)
        .ok()
        .and_then(|mut file| macos_minimum(&mut file, ARCH))?;
    newer_macos(needed, running)
}

/// [`LibraryProblem::NeedsMacos`] when the `needed` macOS is above the
/// `running` one.
fn newer_macos(needed: OsVersion, running: OsVersion) -> Option<LibraryProblem> {
    (needed > running).then_some(LibraryProblem::NeedsMacos { needed, running })
}

/// The running macOS as read, unless it is `10.16`, which is unknown: a
/// process in compatibility mode (`SYSTEM_VERSION_COMPAT=1`, or a program
/// built against an SDK older than macOS 11) reads every macOS from 11 on as
/// `10.16`, and a library needing 11.0 would be refused on a Mac that loads
/// it. Unknown, nothing is refused and the load is tried.
fn known_macos(running: OsVersion) -> Option<OsVersion> {
    let compatibility = OsVersion {
        major: 10,
        minor: 16,
        patch: 0,
    };
    (running != compatibility).then_some(running)
}

/// The running macOS, from the system's own version file.
#[cfg(target_os = "macos")]
fn running_macos() -> Option<OsVersion> {
    let plist = std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist").ok()?;
    parse_system_version(&plist)
}

/// Not macOS: no library of this system declares a minimum macOS.
#[cfg(not(target_os = "macos"))]
fn running_macos() -> Option<OsVersion> {
    None
}

/// The `ProductVersion` of macOS's `SystemVersion.plist`, an XML property
/// list in which `<key>ProductVersion</key>` is followed by
/// `<string>15.7.2</string>`: two or three numbers. `None` for anything
/// else.
pub fn parse_system_version(plist: &str) -> Option<OsVersion> {
    let (_, after) = plist.split_once("<key>ProductVersion</key>")?;
    let value = after.trim_start().strip_prefix("<string>")?;
    let (version, _) = value.split_once("</string>")?;
    let numbers = version
        .trim()
        .split('.')
        .map(|number| {
            let digits = !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit());
            digits.then(|| number.parse::<u32>().ok()).flatten()
        })
        .collect::<Option<Vec<u32>>>()?;
    match numbers[..] {
        [major, minor] => Some(OsVersion {
            major,
            minor,
            patch: 0,
        }),
        [major, minor, patch] => Some(OsVersion {
            major,
            minor,
            patch,
        }),
        _ => None,
    }
}

/// The magic of a thin 64-bit Mach-O file, little-endian.
const MH_MAGIC_64: u32 = 0xFEED_FACF;
/// The magics of a fat (universal) file, big-endian: 32-bit offsets, then
/// 64-bit ones.
const FAT_MAGIC: u32 = 0xCAFE_BABE;
const FAT_MAGIC_64: u32 = 0xCAFE_BABF;
const CPU_TYPE_X86_64: u32 = 0x0100_0007;
const CPU_TYPE_ARM64: u32 = 0x0100_000C;
/// The load commands that say the minimum macOS.
const LC_VERSION_MIN_MACOSX: u32 = 0x24;
const LC_BUILD_VERSION: u32 = 0x32;
/// `LC_BUILD_VERSION`'s platform for macOS.
const PLATFORM_MACOS: u32 = 1;
/// More slices than a real fat file has. A Java class file starts with
/// `FAT_MAGIC` too, followed by its version, 45 or more.
const MAX_FAT_SLICES: u32 = 16;
/// The most bytes of load commands read; a real library has some tens of
/// kilobytes.
const MAX_LOAD_COMMANDS_SIZE: u32 = 1 << 20;

/// The minimum macOS of the Mach-O file `file`, for the architecture `arch`
/// (`x86_64`, `aarch64`, as `std::env::consts::ARCH` names them): from its
/// `LC_BUILD_VERSION` for macOS or its `LC_VERSION_MIN_MACOSX`. In a fat
/// file, the slice of `arch` is read, which it seeks to. `None` for anything
/// else: another architecture, a file that is not Mach-O, one cut short or
/// malformed. It reads only headers (the fat header, the slice's Mach-O
/// header and its load commands, whose total size the header gives and of
/// which more than 1 MiB is refused), and never panics.
pub fn macos_minimum(file: &mut (impl Read + Seek), arch: &str) -> Option<OsVersion> {
    let cputype = match arch {
        "x86_64" => CPU_TYPE_X86_64,
        "aarch64" => CPU_TYPE_ARM64,
        _ => return None,
    };
    file.seek(SeekFrom::Start(0)).ok()?;
    let magic: [u8; 4] = read_array(file)?;
    let start = match u32::from_be_bytes(magic) {
        FAT_MAGIC => fat_slice(file, cputype, false)?,
        FAT_MAGIC_64 => fat_slice(file, cputype, true)?,
        _ => 0,
    };
    thin_minimum(file, start, cputype)
}

/// The offset of the slice of `cputype` in a fat file, read from just after
/// its magic; `wide` for 64-bit offsets.
fn fat_slice(file: &mut impl Read, cputype: u32, wide: bool) -> Option<u64> {
    let count = u32::from_be_bytes(read_array(file)?);
    if count > MAX_FAT_SLICES {
        return None;
    }
    for _ in 0..count {
        // cputype, cpusubtype, offset, size, align (and a reserved word when
        // wide), big-endian.
        let (slice_cputype, offset) = if wide {
            let entry: [u8; 32] = read_array(file)?;
            (be_u32(&entry, 0)?, be_u64(&entry, 8)?)
        } else {
            let entry: [u8; 20] = read_array(file)?;
            (be_u32(&entry, 0)?, u64::from(be_u32(&entry, 8)?))
        };
        if slice_cputype == cputype {
            return Some(offset);
        }
    }
    None
}

/// The minimum macOS of the thin Mach-O file at `start`, when it is one of
/// `cputype`.
fn thin_minimum(file: &mut (impl Read + Seek), start: u64, cputype: u32) -> Option<OsVersion> {
    file.seek(SeekFrom::Start(start)).ok()?;
    // magic, cputype, cpusubtype, filetype, ncmds, sizeofcmds, flags,
    // reserved, little-endian.
    let header: [u8; 32] = read_array(file)?;
    if le_u32(&header, 0)? != MH_MAGIC_64 || le_u32(&header, 4)? != cputype {
        return None;
    }
    let count = le_u32(&header, 16)?;
    let size = le_u32(&header, 20)?;
    if size > MAX_LOAD_COMMANDS_SIZE {
        return None;
    }
    let mut commands = vec![0; usize::try_from(size).ok()?];
    file.read_exact(&mut commands).ok()?;
    minimum_in(&commands, count)
}

/// The minimum macOS that the first `count` load commands in `commands` say.
/// Each command starts with its `cmd` and its `cmdsize`, which counts the
/// whole command; a command smaller than those two words, larger than what
/// is left, or whose size is not a multiple of 8 (malformed in a 64-bit
/// file, which the system's loader refuses), ends the search with nothing.
fn minimum_in(mut commands: &[u8], count: u32) -> Option<OsVersion> {
    for _ in 0..count {
        let cmd = le_u32(commands, 0)?;
        let size = usize::try_from(le_u32(commands, 4)?).ok()?;
        if size < 8 || !size.is_multiple_of(8) {
            return None;
        }
        let command = commands.get(..size)?;
        match cmd {
            // cmd, cmdsize, platform, minos, sdk, ntools.
            LC_BUILD_VERSION if le_u32(command, 8)? == PLATFORM_MACOS => {
                return le_u32(command, 12).map(OsVersion::from_word);
            }
            // cmd, cmdsize, version, sdk.
            LC_VERSION_MIN_MACOSX => return le_u32(command, 8).map(OsVersion::from_word),
            _ => {}
        }
        commands = commands.get(size..)?;
    }
    None
}

fn read_array<const N: usize>(file: &mut impl Read) -> Option<[u8; N]> {
    let mut bytes = [0; N];
    file.read_exact(&mut bytes).ok()?;
    Some(bytes)
}

fn le_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let word = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes(word.try_into().ok()?))
}

fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let word = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes(word.try_into().ok()?))
}

fn be_u64(bytes: &[u8], at: usize) -> Option<u64> {
    let word = bytes.get(at..at.checked_add(8)?)?;
    Some(u64::from_be_bytes(word.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Cursor;

    use super::*;

    fn version(major: u32, minor: u32, patch: u32) -> OsVersion {
        OsVersion {
            major,
            minor,
            patch,
        }
    }

    fn words(values: &[u32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    /// A version word: `major << 16 | minor << 8`.
    fn word(major: u32, minor: u32) -> u32 {
        (major << 16) | (minor << 8)
    }

    /// `LC_BUILD_VERSION` for `platform`, with the SDK 15.0 and no tools.
    fn build_version(platform: u32, major: u32, minor: u32) -> Vec<u8> {
        words(&[
            LC_BUILD_VERSION,
            24,
            platform,
            word(major, minor),
            word(15, 0),
            0,
        ])
    }

    /// `LC_VERSION_MIN_MACOSX`, with the SDK 15.0.
    fn version_min(major: u32, minor: u32) -> Vec<u8> {
        words(&[LC_VERSION_MIN_MACOSX, 16, word(major, minor), word(15, 0)])
    }

    /// `LC_UUID`: a command that says nothing of the macOS.
    fn uuid() -> Vec<u8> {
        words(&[0x1B, 24, 1, 2, 3, 4])
    }

    /// A thin 64-bit Mach-O library of `cputype` with `commands`.
    fn thin(cputype: u32, commands: &[Vec<u8>]) -> Vec<u8> {
        let body = commands.concat();
        let count = u32::try_from(commands.len()).unwrap();
        let size = u32::try_from(body.len()).unwrap();
        // MH_DYLIB is filetype 6.
        let mut file = words(&[MH_MAGIC_64, cputype, 0, 6, count, size, 0, 0]);
        file.extend(body);
        file
    }

    /// A fat file of `slices`, each at its own 4 KiB boundary; `wide` with
    /// 64-bit offsets.
    fn fat(slices: &[(u32, Vec<u8>)], wide: bool) -> Vec<u8> {
        let magic = if wide { FAT_MAGIC_64 } else { FAT_MAGIC };
        let mut file: Vec<u8> = [magic, u32::try_from(slices.len()).unwrap()]
            .iter()
            .flat_map(|value| value.to_be_bytes())
            .collect();
        let offset = |index: usize| 4096 * (index as u64 + 1);
        for (index, (cputype, slice)) in slices.iter().enumerate() {
            let size = slice.len() as u64;
            file.extend(cputype.to_be_bytes());
            file.extend(0u32.to_be_bytes());
            if wide {
                file.extend(offset(index).to_be_bytes());
                file.extend(size.to_be_bytes());
                file.extend(12u32.to_be_bytes());
                file.extend(0u32.to_be_bytes());
            } else {
                file.extend(u32::try_from(offset(index)).unwrap().to_be_bytes());
                file.extend(u32::try_from(size).unwrap().to_be_bytes());
                file.extend(12u32.to_be_bytes());
            }
        }
        for (index, (_, slice)) in slices.iter().enumerate() {
            file.resize(usize::try_from(offset(index)).unwrap(), 0);
            file.extend(slice);
        }
        file
    }

    fn minimum(bytes: &[u8], arch: &str) -> Option<OsVersion> {
        macos_minimum(&mut Cursor::new(bytes), arch)
    }

    #[test]
    fn a_thin_arm64_library_says_its_build_version() {
        let file = thin(
            CPU_TYPE_ARM64,
            &[uuid(), build_version(PLATFORM_MACOS, 14, 0)],
        );
        assert_eq!(minimum(&file, "aarch64"), Some(version(14, 0, 0)));
        // A thin file of another architecture says nothing of this one.
        assert_eq!(minimum(&file, "x86_64"), None);
    }

    #[test]
    fn a_thin_x86_64_library_says_its_version_min() {
        let file = thin(CPU_TYPE_X86_64, &[version_min(11, 0), uuid()]);
        assert_eq!(minimum(&file, "x86_64"), Some(version(11, 0, 0)));
        assert_eq!(minimum(&file, "aarch64"), None);
    }

    #[test]
    fn a_fat_library_gives_the_slice_of_the_asked_architecture() {
        let slices = [
            (
                CPU_TYPE_X86_64,
                thin(CPU_TYPE_X86_64, &[version_min(11, 0)]),
            ),
            (
                CPU_TYPE_ARM64,
                thin(CPU_TYPE_ARM64, &[build_version(PLATFORM_MACOS, 14, 0)]),
            ),
        ];
        for wide in [false, true] {
            let file = fat(&slices, wide);
            assert_eq!(minimum(&file, "x86_64"), Some(version(11, 0, 0)));
            assert_eq!(minimum(&file, "aarch64"), Some(version(14, 0, 0)));
            assert_eq!(minimum(&file, "riscv64"), None);
        }
        // A fat file without the asked slice.
        let file = fat(&slices[..1], false);
        assert_eq!(minimum(&file, "aarch64"), None);
    }

    #[test]
    fn the_file_is_read_from_its_start() {
        let file = thin(CPU_TYPE_ARM64, &[build_version(PLATFORM_MACOS, 14, 0)]);
        let mut cursor = Cursor::new(file);
        cursor.set_position(10);
        assert_eq!(
            macos_minimum(&mut cursor, "aarch64"),
            Some(version(14, 0, 0))
        );
    }

    #[test]
    fn anything_but_a_mach_o_file_gives_no_version() {
        let mut elf = b"\x7fELF\x02\x01\x01\0".to_vec();
        elf.resize(64, 0);
        assert_eq!(minimum(&elf, "x86_64"), None);
        assert_eq!(minimum(&[], "x86_64"), None);
        assert_eq!(minimum(b"\xcf\xfa", "x86_64"), None);
        // A Java class file shares the fat magic; its version is no count of
        // slices.
        let class = [0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 0x34, 0, 0x0A];
        assert_eq!(minimum(&class, "aarch64"), None);
    }

    #[test]
    fn a_broken_mach_o_file_gives_no_version() {
        let commands = [uuid(), build_version(PLATFORM_MACOS, 14, 0)];
        let whole = thin(CPU_TYPE_ARM64, &commands);
        // Cut in the middle of a load command.
        assert_eq!(minimum(&whole[..32 + 30], "aarch64"), None);
        // A load command whose size is 0, or less than its own two words.
        for size in [0, 4] {
            let file = thin(
                CPU_TYPE_ARM64,
                &[
                    words(&[0x1B, size, 0, 0]),
                    build_version(PLATFORM_MACOS, 14, 0),
                ],
            );
            assert_eq!(minimum(&file, "aarch64"), None, "cmdsize {size}");
        }
        // A load command whose size is not a multiple of 8, which a 64-bit
        // file never has, before the version command.
        let file = thin(
            CPU_TYPE_ARM64,
            &[words(&[0x1B, 12, 0]), build_version(PLATFORM_MACOS, 14, 0)],
        );
        assert_eq!(minimum(&file, "aarch64"), None, "cmdsize 12");
        // A load command larger than the commands' total size.
        let file = thin(CPU_TYPE_ARM64, &[words(&[0x1B, 64, 0, 0, 0, 0])]);
        assert_eq!(minimum(&file, "aarch64"), None);
        // More commands counted than there are.
        let mut file = thin(CPU_TYPE_ARM64, &[uuid()]);
        file[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(minimum(&file, "aarch64"), None);
        // Load commands said to take more than 1 MiB.
        let mut file = whole.clone();
        file[20..24].copy_from_slice(&(MAX_LOAD_COMMANDS_SIZE + 1).to_le_bytes());
        assert_eq!(minimum(&file, "aarch64"), None);
        // A build version for another platform (2 is iOS).
        let file = thin(CPU_TYPE_ARM64, &[build_version(2, 17, 0)]);
        assert_eq!(minimum(&file, "aarch64"), None);
        // A fat file whose slice is past the end, or one cut in its header.
        let slices = [(CPU_TYPE_ARM64, whole.clone())];
        let fat_file = fat(&slices, true);
        assert_eq!(minimum(&fat_file[..4096], "aarch64"), None);
        assert_eq!(minimum(&fat_file[..20], "aarch64"), None);
        // No prefix of a good file makes it panic.
        for len in 0..fat_file.len() {
            let _ = minimum(&fat_file[..len], "aarch64");
        }
    }

    #[test]
    fn the_system_version_is_read_from_its_plist() {
        let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>ProductBuildVersion</key>
	<string>24G325</string>
	<key>ProductName</key>
	<string>macOS</string>
	<key>ProductUserVisibleVersion</key>
	<string>15.7.3</string>
	<key>ProductVersion</key>
	<string>15.7.2</string>
</dict>
</plist>
"#;
        assert_eq!(parse_system_version(plist), Some(version(15, 7, 2)));
        let two = "<key>ProductVersion</key>\n\t<string>13.6</string>";
        assert_eq!(parse_system_version(two), Some(version(13, 6, 0)));
    }

    #[test]
    fn a_plist_without_a_version_gives_none() {
        assert_eq!(parse_system_version("<dict></dict>"), None);
        for value in ["15", "15.x", "+15.1", "15..1", "1.2.3.4", ""] {
            let plist = format!("<key>ProductVersion</key><string>{value}</string>");
            assert_eq!(parse_system_version(&plist), None, "{value:?}");
        }
        let unclosed = "<key>ProductVersion</key><string>15.7";
        assert_eq!(parse_system_version(unclosed), None);
    }

    #[test]
    fn versions_are_ordered_by_their_numbers_and_shown_short() {
        assert!(version(13, 6, 1) < version(14, 0, 0));
        assert!(version(11, 0, 0) < version(11, 0, 1));
        assert!(version(10, 16, 0) < version(11, 0, 0));
        assert_eq!(version(14, 0, 0).to_string(), "14.0");
        assert_eq!(version(13, 6, 1).to_string(), "13.6.1");
        assert_eq!(OsVersion::from_word(0x000D_0401), version(13, 4, 1));
    }

    #[test]
    fn macos_10_16_is_an_unknown_running_version() {
        assert_eq!(known_macos(version(10, 16, 0)), None);
        assert_eq!(known_macos(version(10, 15, 7)), Some(version(10, 15, 7)));
        assert_eq!(known_macos(version(11, 0, 0)), Some(version(11, 0, 0)));
    }

    #[test]
    fn a_library_for_a_newer_macos_is_a_problem() {
        let (needed, running) = (version(14, 0, 0), version(13, 6, 0));
        assert_eq!(
            newer_macos(needed, running),
            Some(LibraryProblem::NeedsMacos { needed, running })
        );
        assert_eq!(newer_macos(needed, needed), None);
        assert_eq!(newer_macos(version(11, 0, 0), version(15, 7, 2)), None);
    }

    #[test]
    fn the_refusals_name_the_library() {
        let library = Path::new("/opt/onnxruntime/libonnxruntime.dylib");
        assert_eq!(
            LibraryProblem::Missing.refusal(library),
            format!("ONNX Runtime not found at {}", library.display())
        );
        let problem = LibraryProblem::NeedsMacos {
            needed: version(14, 0, 0),
            running: version(13, 6, 1),
        };
        assert_eq!(
            problem.refusal(library),
            format!(
                "ONNX Runtime at {} needs macOS 14.0 or later; this is macOS 13.6.1",
                library.display()
            )
        );
    }

    /// A folder beside the executable and a package's folder, in that order,
    /// and the library's file name.
    fn two_folders(root: &Path) -> ([PathBuf; 2], String) {
        let folders = [root.join("bin"), root.join("lib").join("Photostacker")];
        for folder in &folders {
            fs::create_dir_all(folder).unwrap();
        }
        (folders, format!("{DLL_PREFIX}onnxruntime{DLL_SUFFIX}"))
    }

    #[test]
    fn the_packaged_folder_wins_when_the_library_is_only_there() {
        let root = tempfile::tempdir().unwrap();
        let (folders, name) = two_folders(root.path());
        fs::write(folders[1].join(&name), b"").unwrap();
        assert_eq!(find_library(None, &folders), folders[1].join(&name));
        // In both, the one beside the executable comes first.
        fs::write(folders[0].join(&name), b"").unwrap();
        assert_eq!(find_library(None, &folders), folders[0].join(&name));
    }

    #[test]
    fn ort_dylib_path_wins_over_both() {
        let root = tempfile::tempdir().unwrap();
        let (folders, name) = two_folders(root.path());
        for folder in &folders {
            fs::write(folder.join(&name), b"").unwrap();
        }
        let chosen = root.path().join("elsewhere").join(&name);
        assert_eq!(
            find_library(Some(chosen.clone().into_os_string()), &folders),
            chosen
        );
        // Empty counts as unset.
        assert_eq!(
            find_library(Some(OsString::new()), &folders),
            folders[0].join(&name)
        );
    }

    #[test]
    fn with_the_library_nowhere_the_name_beside_the_executable_is_given() {
        let root = tempfile::tempdir().unwrap();
        let (folders, name) = two_folders(root.path());
        assert_eq!(find_library(None, &folders), folders[0].join(&name));
        assert_eq!(find_library(None, &[]), PathBuf::from(name));
    }

    #[test]
    fn a_missing_library_is_missing() {
        let root = tempfile::tempdir().unwrap();
        let library = root.path().join("libonnxruntime.dylib");
        assert_eq!(library_problem(&library), Some(LibraryProblem::Missing));
        // A folder is no library either.
        assert_eq!(library_problem(root.path()), Some(LibraryProblem::Missing));
    }

    #[test]
    fn a_library_whose_minimum_cannot_be_read_is_tried() {
        let root = tempfile::tempdir().unwrap();
        let library = root.path().join("libonnxruntime.dylib");
        fs::write(&library, b"not a shared library").unwrap();
        assert_eq!(library_problem(&library), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_running_macos_is_known() {
        let running = running_macos().expect("SystemVersion.plist has a ProductVersion");
        assert!(running.major >= 11, "macOS {running}");
    }

    /// The library the model tests load (the justfile's `ORT_DYLIB_PATH`): the
    /// pinned build of every system needs no newer macOS than the one the
    /// tests run on.
    #[test]
    fn the_library_at_ort_dylib_path_declares_no_newer_macos() {
        let library = environment_library();
        if !library.is_file() {
            println!("skipped: ONNX Runtime not found at {}", library.display());
            return;
        }
        assert_eq!(library_problem(library), None, "{}", library.display());
    }

    /// The version of this system's row of the pinned builds
    /// (`scripts/onnxruntime.tsv`), if it has one.
    fn pinned_version() -> Option<String> {
        let table = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/onnxruntime.tsv");
        let text = fs::read_to_string(&table)
            .unwrap_or_else(|error| panic!("{}: {error}", table.display()));
        text.lines()
            .filter(|line| !line.starts_with('#'))
            .map(|line| line.split('\t').collect::<Vec<_>>())
            .find(|row| row.len() > 2 && row[0] == env::consts::OS && row[1] == ARCH)
            .map(|row| row[2].to_owned())
    }

    /// The loaded library's own answer: `1.<n>.<n>`, exactly the version
    /// pinned for this system when it is the library of `just ort`.
    #[test]
    fn the_loaded_version_is_the_library_s_own() {
        let library = environment_library();
        if !library.is_file() {
            println!("skipped: ONNX Runtime not found at {}", library.display());
            return;
        }
        let version = loaded_version().unwrap();
        let numbers: Vec<&str> = version.split('.').collect();
        assert!(
            numbers.len() == 3
                && numbers[0] == "1"
                && numbers
                    .iter()
                    .all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())),
            "{version}"
        );
        match pinned_version() {
            Some(pinned) => assert_eq!(version, pinned),
            None => println!("no pinned build for {} {ARCH}", env::consts::OS),
        }
    }
}
