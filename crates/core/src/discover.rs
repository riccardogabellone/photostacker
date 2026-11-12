//! Folder scan: finds the supported photos of an input folder and fingerprints
//! each one, so a resumed job can tell whether a file changed.

use std::fs::{self, DirEntry};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::model::FileKind;
use crate::write::APP_NAME;

/// Still-image extensions (lowercase, no dot).
pub const STILL_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "tif", "tiff", "heic", "heif"];

/// RAW extensions (lowercase, no dot): the ones rawler 0.8 decodes. The
/// `decode::raw` module doc says how the list was derived from rawler.
pub const RAW_EXTENSIONS: &[&str] = &[
    "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "raf", "orf", "rw2", "pef", "dng",
    "srw", "3fr", "iiq", "erf", "kdc", "mrw", "mos",
];

/// The decoder family of a file, from its extension (case-insensitive).
/// `None` for an unsupported or missing extension.
pub fn kind_of(path: &Path) -> Option<FileKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => Some(FileKind::Jpeg),
        "png" => Some(FileKind::Png),
        "tif" | "tiff" => Some(FileKind::Tiff),
        "heic" | "heif" => Some(FileKind::Heic),
        raw if RAW_EXTENSIONS.contains(&raw) => Some(FileKind::Raw),
        _ => None,
    }
}

/// How many leading bytes of a file go into its fingerprint.
const FINGERPRINT_PREFIX_LEN: u64 = 64 * 1024;
/// The TIFF tag Software, which marks the TIFFs Photostacker wrote, and its
/// field type ASCII.
const SOFTWARE: u16 = 305;
const ASCII: u16 = 2;

/// One supported file found by [`discover`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredFile {
    /// `input` joined with the file's relative path, exactly as the file system names it.
    pub path: PathBuf,
    pub kind: FileKind,
    pub size: u64,
    pub mtime: SystemTime,
    /// Lowercase hex BLAKE3 of: `size` as u64 LE (8 bytes) ‖ `mtime` as signed
    /// nanoseconds since the Unix epoch, i128 LE (16 bytes) ‖ the first 64 KiB
    /// of the content (the whole file if shorter). It is persisted, so this
    /// layout must not change.
    pub fingerprint: String,
}

/// A file or folder left out of a scan because it could not be read.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoverSkip {
    pub path: PathBuf,
    /// The I/O error, as displayed.
    pub reason: String,
}

/// Lists the supported files of `input`, sorted by path, and the entries that
/// could not be read, also sorted by path.
///
/// - Only the top level is scanned unless `recursive` is set.
/// - Hidden entries are skipped, files and folders alike: a name starting with `.`
///   and, on Windows, the hidden or system attribute.
/// - A subfolder equal to one of `skip_dirs` (compared canonicalised) is not entered;
///   this keeps an output folder inside the input out of the scan.
/// - Symlinked folders are never entered, so link cycles cannot loop. A symlinked
///   file is included with its target's metadata.
/// - Photostacker's own layered TIFFs are skipped, silently: a `.tif` or `.tiff`
///   whose first IFD has a Software tag that starts with `Photostacker`. A file
///   whose header or first IFD cannot be read is kept; decoding decides about it.
/// - Only failing to read `input` itself is an error. An unreadable subfolder or
///   supported file (including one that vanished, or a dangling symlink) becomes a
///   [`DiscoverSkip`] and the scan goes on.
pub fn discover(
    input: &Path,
    recursive: bool,
    skip_dirs: &[PathBuf],
) -> Result<(Vec<DiscoveredFile>, Vec<DiscoverSkip>), DiscoverError> {
    let mut scan = Scan {
        recursive,
        skip_dirs: skip_dirs.iter().map(|d| canonical_or_as_is(d)).collect(),
        pending: Vec::new(),
        files: Vec::new(),
        skipped: Vec::new(),
    };
    scan.read_dir(input).map_err(|source| DiscoverError::Io {
        path: input.to_path_buf(),
        source,
    })?;
    while let Some(dir) = scan.pending.pop() {
        if let Err(e) = scan.read_dir(&dir) {
            scan.skip(dir, &e);
        }
    }
    scan.files.sort_by(|a, b| a.path.cmp(&b.path));
    scan.skipped.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((scan.files, scan.skipped))
}

/// The state of one [`discover`] call.
struct Scan {
    recursive: bool,
    /// Canonicalised where possible.
    skip_dirs: Vec<PathBuf>,
    /// Subfolders found but not read yet.
    pending: Vec<PathBuf>,
    files: Vec<DiscoveredFile>,
    skipped: Vec<DiscoverSkip>,
}

impl Scan {
    /// Reads the entries of one folder. Failing to list the folder is returned
    /// (entries listed before the failure are kept); a failure on one entry is
    /// recorded as a skip and the listing goes on.
    fn read_dir(&mut self, dir: &Path) -> io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if let Err(e) = self.visit(&entry) {
                self.skip(entry.path(), &e);
            }
        }
        Ok(())
    }

    /// Queues a subfolder, records a supported file, or ignores the entry.
    fn visit(&mut self, entry: &DirEntry) -> io::Result<()> {
        if is_hidden(entry)? {
            return Ok(());
        }
        let path = entry.path();
        // Does not follow symlinks: a symlinked folder is not `is_dir`.
        if entry.file_type()?.is_dir() {
            if self.recursive && !self.skip_dirs.contains(&canonical_or_as_is(&path)) {
                self.pending.push(path);
            }
            return Ok(());
        }
        let Some(kind) = kind_of(&path) else {
            return Ok(());
        };
        // Follows symlinks, so a linked file reports its target's size and mtime.
        let metadata = fs::metadata(&path)?;
        // Ignores folders behind symlinks, FIFOs, sockets and devices.
        if !metadata.is_file() {
            return Ok(());
        }
        if kind == FileKind::Tiff && is_own_tiff(&path) {
            return Ok(());
        }
        let size = metadata.len();
        let mtime = metadata.modified()?;
        let fingerprint = fingerprint(&path, size, mtime)?;
        self.files.push(DiscoveredFile {
            path,
            kind,
            size,
            mtime,
            fingerprint,
        });
        Ok(())
    }

    fn skip(&mut self, path: PathBuf, error: &io::Error) {
        self.skipped.push(DiscoverSkip {
            path,
            reason: error.to_string(),
        });
    }
}

/// The TIFF or BigTIFF at `path` was written by Photostacker (see
/// [`has_own_software`]). `false` when the file cannot be opened.
fn is_own_tiff(path: &Path) -> bool {
    fs::File::open(path).is_ok_and(|file| has_own_software(file) == Some(true))
}

/// Whether the first IFD of the TIFF or BigTIFF `file` has an ASCII Software
/// tag that starts with [`APP_NAME`]; `None` when its header, its first IFD
/// or that tag's value cannot be read.
///
/// The file is read by position, a few small reads checked against its
/// length: its header, the entries of its first IFD up to the Software one,
/// and the start of that tag's value. A layered TIFF can be gigabytes, and
/// it is never read whole, nor memory-mapped (spec §4.3).
///
/// - A classic TIFF: `II` or `MM`, 42 and the IFD's 4-byte offset; the IFD
///   is a 2-byte count of 12-byte entries (tag, type, a 4-byte count, then
///   the value when it fits in 4 bytes, else its 4-byte offset).
/// - A BigTIFF: `II` or `MM`, 43, the offset size 8, a zero short and the
///   IFD's 8-byte offset; the IFD is an 8-byte count of 20-byte entries
///   (tag, type, an 8-byte count, then the value or its 8-byte offset).
///
/// A count past the end of the file ends at the last whole entry. A value
/// shorter than [`APP_NAME`] cannot start with it, and one that long never
/// fits in its entry, so the value is always at its offset.
fn has_own_software(file: impl Read + Seek) -> Option<bool> {
    let mut file = Positional::new(file)?;
    let head = file.bytes(0, 8)?;
    let little_endian = match &head[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let int = |bytes: &[u8]| -> u64 {
        let mut word = [0; 8];
        let len = bytes.len();
        if little_endian {
            word[..len].copy_from_slice(bytes);
            u64::from_le_bytes(word)
        } else {
            word[8 - len..].copy_from_slice(bytes);
            u64::from_be_bytes(word)
        }
    };
    // (IFD0, the width of its count, of an entry, and of an entry's count
    // and value fields.)
    let (ifd0, count_len, entry_len, field_len) = match int(&head[2..4]) {
        42 => (int(&head[4..8]), 2, 12, 4),
        43 if int(&head[4..6]) == 8 => (int(&file.bytes(8, 8)?), 8, 20, 8),
        _ => return None,
    };
    let count = int(&file.bytes(ifd0, count_len)?);
    let table = ifd0 + count_len;
    let count = count.min(file.len.saturating_sub(table) / entry_len);
    let tag = if little_endian {
        SOFTWARE.to_le_bytes()
    } else {
        SOFTWARE.to_be_bytes()
    };
    let Some(entry) = file.entry(table, count, entry_len, tag)? else {
        return Some(false);
    };
    if int(&entry[2..4]) != u64::from(ASCII) {
        return Some(false);
    }
    let len = int(&entry[4..4 + field_len]);
    let name = APP_NAME.as_bytes();
    if len < name.len() as u64 {
        return Some(false);
    }
    let offset = int(&entry[4 + field_len..4 + 2 * field_len]);
    // The whole value must lie in the file, though only its start is read.
    if offset.checked_add(len)? > file.len {
        return None;
    }
    Some(file.bytes(offset, name.len() as u64)? == name)
}

/// The most IFD entries [`Positional::entry`] reads at once.
const ENTRIES_PER_READ: u64 = 4096;

/// A file read by position, every read checked against its length.
struct Positional<R> {
    inner: R,
    len: u64,
}

impl<R: Read + Seek> Positional<R> {
    fn new(mut inner: R) -> Option<Self> {
        let len = inner.seek(SeekFrom::End(0)).ok()?;
        Some(Self { inner, len })
    }

    /// The `n` bytes at `offset`; `None` when the file does not hold them
    /// all, or they cannot be read. `n` is small: the callers bound it.
    fn bytes(&mut self, offset: u64, n: u64) -> Option<Vec<u8>> {
        if offset.checked_add(n)? > self.len {
            return None;
        }
        let mut buf = vec![0; usize::try_from(n).ok()?];
        self.inner.seek(SeekFrom::Start(offset)).ok()?;
        self.inner.read_exact(&mut buf).ok()?;
        Some(buf)
    }

    /// The first of the `count` IFD entries of `entry_len` bytes at `table`
    /// that starts with `tag` (in the file's byte order), read
    /// [`ENTRIES_PER_READ`] at a time; `Some(None)` when there is none. Every
    /// entry must lie in the file.
    fn entry(
        &mut self,
        table: u64,
        count: u64,
        entry_len: u64,
        tag: [u8; 2],
    ) -> Option<Option<Vec<u8>>> {
        let mut read = 0;
        while read < count {
            let n = (count - read).min(ENTRIES_PER_READ);
            let entries = self.bytes(table + read * entry_len, n * entry_len)?;
            let mut chunks = entries.chunks_exact(usize::try_from(entry_len).ok()?);
            if let Some(entry) = chunks.find(|entry| entry[..2] == tag) {
                return Some(Some(entry.to_vec()));
            }
            read += n;
        }
        Some(None)
    }
}

/// See [`DiscoveredFile::fingerprint`] for the layout.
fn fingerprint(path: &Path, size: u64, mtime: SystemTime) -> io::Result<String> {
    let mut prefix = Vec::new();
    fs::File::open(path)?
        .take(FINGERPRINT_PREFIX_LEN)
        .read_to_end(&mut prefix)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&size.to_le_bytes());
    hasher.update(&unix_nanos(mtime).to_le_bytes());
    hasher.update(&prefix);
    Ok(hasher.finalize().to_hex().to_string())
}

/// Signed nanoseconds since the Unix epoch; negative before 1970.
fn unix_nanos(time: SystemTime) -> i128 {
    // A `SystemTime` spans far less than 2^127 ns, so the casts cannot wrap.
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(after) => after.as_nanos() as i128,
        Err(before) => -(before.duration().as_nanos() as i128),
    }
}

/// A name starting with `.` on every platform; on Windows also the hidden or
/// system attribute.
fn is_hidden(entry: &DirEntry) -> io::Result<bool> {
    if entry.file_name().as_encoded_bytes().first() == Some(&b'.') {
        return Ok(true);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
        // On Windows this comes from the directory listing: no extra system call.
        let attributes = entry.metadata()?.file_attributes();
        if attributes & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The canonical form of `path`, or `path` itself when it cannot be resolved
/// (it does not exist yet, for example).
fn canonical_or_as_is(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Why a scan could not run: the input folder itself could not be read.
#[derive(Debug, thiserror::Error)]
pub enum DiscoverError {
    #[error("cannot read {}", path.display())]
    Io { path: PathBuf, source: io::Error },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::AtomicBool;

    use crate::decode::{FullImage, ImageSource, Probe, Samples};
    use crate::group::Container;
    use crate::model::OutputFormat;
    use crate::write::{StackInput, WriteOptions, WriteOutcome, write_stack};
    use photostacker_testkit::{TiffSpec, Value};

    /// `rel` (written with `/`) as a platform-native relative path, built one
    /// component at a time so Windows gets `\` like the paths `discover` returns.
    fn native(rel: &str) -> PathBuf {
        rel.split('/').collect()
    }

    /// Creates `rel` under `root` (and its parent folders) with some bytes.
    fn touch(root: &Path, rel: &str) -> PathBuf {
        let path = root.join(native(rel));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"not really a photo, but enough bytes to hash").unwrap();
        path
    }

    /// Runs a scan that must find nothing to skip.
    fn scan(input: &Path, recursive: bool, skip_dirs: &[PathBuf]) -> Vec<DiscoveredFile> {
        let (files, skipped) = discover(input, recursive, skip_dirs).unwrap();
        assert_eq!(skipped, []);
        files
    }

    /// The discovered paths, relative to `root`.
    fn rel_paths(root: &Path, files: &[DiscoveredFile]) -> Vec<PathBuf> {
        files
            .iter()
            .map(|f| f.path.strip_prefix(root).unwrap().to_path_buf())
            .collect()
    }

    /// The tree shared by the top-level and recursive tests.
    fn mixed_tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for rel in ["a.JPG", "b.cr3", "c.txt", "sub/d.jpg"] {
            touch(dir.path(), rel);
        }
        dir
    }

    #[test]
    fn extension_lists_map_to_kinds() {
        for ext in STILL_EXTENSIONS {
            let kind = kind_of(Path::new(&format!("x.{ext}")));
            assert!(matches!(kind, Some(k) if k != FileKind::Raw), "{ext}");
            let upper = kind_of(Path::new(&format!("x.{}", ext.to_uppercase())));
            assert_eq!(upper, kind, "{ext} uppercase");
        }
        for ext in RAW_EXTENSIONS {
            assert_eq!(kind_of(Path::new(&format!("x.{ext}"))), Some(FileKind::Raw));
            let upper = format!("x.{}", ext.to_uppercase());
            assert_eq!(kind_of(Path::new(&upper)), Some(FileKind::Raw), "{upper}");
        }
        assert_eq!(kind_of(Path::new("x.jpeg")), Some(FileKind::Jpeg));
        assert_eq!(kind_of(Path::new("x.tif")), Some(FileKind::Tiff));
        assert_eq!(kind_of(Path::new("x.heif")), Some(FileKind::Heic));
        assert_eq!(kind_of(Path::new("x.png")), Some(FileKind::Png));
        assert_eq!(kind_of(Path::new("x.txt")), None);
        assert_eq!(kind_of(Path::new("jpg")), None);
    }

    #[test]
    fn finds_supported_files_top_level_only() {
        let dir = mixed_tree();
        let files = scan(dir.path(), false, &[]);
        assert_eq!(
            rel_paths(dir.path(), &files),
            [native("a.JPG"), native("b.cr3")]
        );
        let kinds: Vec<FileKind> = files.iter().map(|f| f.kind).collect();
        assert_eq!(kinds, [FileKind::Jpeg, FileKind::Raw]);
    }

    #[test]
    fn recursive_includes_subfolders() {
        let dir = mixed_tree();
        let files = scan(dir.path(), true, &[]);
        assert_eq!(
            rel_paths(dir.path(), &files),
            [native("a.JPG"), native("b.cr3"), native("sub/d.jpg")]
        );
    }

    #[test]
    fn hidden_files_and_dirs_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        for rel in [".x.jpg", ".cache/y.jpg", "v.jpg"] {
            touch(dir.path(), rel);
        }
        let files = scan(dir.path(), true, &[]);
        assert_eq!(rel_paths(dir.path(), &files), [native("v.jpg")]);
    }

    #[cfg(windows)]
    #[test]
    fn windows_hidden_and_system_files_are_skipped() {
        use std::io::Write;
        use std::os::windows::fs::OpenOptionsExt;

        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "v.jpg");
        for (name, attributes) in [("h.jpg", 0x2_u32), ("s.jpg", 0x4_u32)] {
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .attributes(attributes)
                .open(dir.path().join(name))
                .unwrap()
                .write_all(b"x")
                .unwrap();
        }
        let files = scan(dir.path(), false, &[]);
        assert_eq!(rel_paths(dir.path(), &files), [native("v.jpg")]);
    }

    /// Writes a stack `<stem>_2.tif` of two flat layers `width`×1 into `dir`
    /// with `write_stack`, and returns its path and container: past 30,000
    /// px it is a BigTIFF.
    fn own_tiff(dir: &Path, stem: &str, width: u32) -> (PathBuf, Container) {
        let input = |stem: String| StackInput {
            stem,
            exif: None,
            probe: Ok(Probe {
                width,
                height: 1,
                sixteen_bit: false,
                has_alpha: false,
                icc: None,
            }),
            load: Box::new(move || {
                Ok(FullImage {
                    width,
                    height: 1,
                    channels: 3,
                    samples: Samples::U8(vec![7; width as usize * 3]),
                    icc: None,
                    source: ImageSource::Decoded,
                })
            }),
        };
        let inputs = vec![input(stem.to_owned()), input(format!("{stem}-2"))];
        let never = AtomicBool::new(false);
        let opts = WriteOptions::default();
        let (outcome, _) =
            write_stack(dir, OutputFormat::Tiff, opts, inputs, &|_| {}, &never).unwrap();
        match outcome {
            WriteOutcome::Written {
                path, container, ..
            } => (path, container),
            WriteOutcome::Skipped { reason } => panic!("skipped: {reason}"),
        }
    }

    #[test]
    fn own_tiff_output_is_not_rediscovered() {
        let dir = tempfile::tempdir().unwrap();
        let (own, container) = own_tiff(dir.path(), "own", 8);
        assert_eq!(container, Container::Tiff);
        let (big, container) = own_tiff(dir.path(), "big", 30_001);
        assert_eq!(container, Container::BigTiff);
        assert!(own.is_file() && big.is_file());

        // A plain TIFF, one another program wrote, and a `.tif` that is no
        // TIFF at all (decoding decides about it) are all kept.
        let tiff = |ifd0| {
            TiffSpec {
                little_endian: true,
                ifd0,
                exif: Vec::new(),
                maker_note: None,
            }
            .build()
        };
        let plain = tiff(vec![(0x0100, Value::Long(vec![8]))]);
        fs::write(dir.path().join("plain.tif"), plain).unwrap();
        let other = tiff(vec![(0x0131, Value::Ascii("Adobe Photoshop 27.0".into()))]);
        fs::write(dir.path().join("other.tiff"), other).unwrap();
        touch(dir.path(), "junk.tif");

        let files = scan(dir.path(), false, &[]);
        assert_eq!(
            rel_paths(dir.path(), &files),
            [
                native("junk.tif"),
                native("other.tiff"),
                native("plain.tif")
            ]
        );
    }

    #[test]
    fn own_software_is_read_by_position_and_bounds_checked() {
        use std::io::Cursor;
        let tiff = |little_endian, ifd0| {
            TiffSpec {
                little_endian,
                ifd0,
                exif: Vec::new(),
                maker_note: None,
            }
            .build()
        };
        let software = |text: &str| vec![(SOFTWARE, Value::Ascii(text.into()))];
        let own = |cursor: Vec<u8>| has_own_software(Cursor::new(cursor));
        for little_endian in [true, false] {
            let ours = tiff(little_endian, software("Photostacker 0.1.0"));
            assert_eq!(own(ours), Some(true), "little-endian {little_endian}");
        }
        assert_eq!(
            own(tiff(true, software("Adobe Photoshop 27.0"))),
            Some(false)
        );
        assert_eq!(own(tiff(true, software("Photo"))), Some(false));
        assert_eq!(
            own(tiff(true, vec![(0x0100, Value::Long(vec![8]))])),
            Some(false)
        );
        let short = vec![(SOFTWARE, Value::Short(vec![1; 16]))];
        assert_eq!(own(tiff(true, short)), Some(false));
        // The value runs past the end of the file: entry 0's offset field
        // follows the header (8), the entry count (2) and its tag, type and
        // count (8).
        let mut past_the_end = tiff(true, software("Photostacker 0.1.0"));
        past_the_end[18..22].copy_from_slice(&0xFFFF_FF00_u32.to_le_bytes());
        assert_eq!(own(past_the_end), None);
        // IFD0 past the end, a cut header, and no TIFF at all.
        let mut ifd0_past_the_end = tiff(true, software("Photostacker 0.1.0"));
        ifd0_past_the_end[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(own(ifd0_past_the_end), None);
        assert_eq!(own(b"II*\0".to_vec()), None);
        assert_eq!(own(b"not a TIFF at all".to_vec()), None);
    }

    #[test]
    fn output_dir_inside_input_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        for rel in ["stacked/z.jpg", "keep/k.jpg"] {
            touch(dir.path(), rel);
        }
        // Written in a non-canonical form, so only a canonical comparison matches it.
        let output = dir.path().join("keep").join("..").join("stacked");
        let files = scan(dir.path(), true, &[output]);
        assert_eq!(rel_paths(dir.path(), &files), [native("keep/k.jpg")]);
    }

    #[test]
    fn non_ascii_paths_are_discovered() {
        let dir = tempfile::tempdir().unwrap();
        let expected = touch(dir.path(), "Viaggio Śląsk/IMG 01.jpg");
        let files = scan(dir.path(), true, &[]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, expected);
        assert_eq!(
            files[0].path.as_os_str().as_encoded_bytes(),
            expected.as_os_str().as_encoded_bytes()
        );
    }

    #[test]
    fn fingerprint_changes_when_content_changes() {
        let dir = tempfile::tempdir().unwrap();
        let changed = touch(dir.path(), "a.jpg");
        touch(dir.path(), "b.jpg");

        let before = scan(dir.path(), false, &[]);

        // Same size and the same mtime, so only the content can tell them apart.
        let mut bytes = fs::read(&changed).unwrap();
        bytes[0] ^= 0xFF;
        fs::write(&changed, &bytes).unwrap();
        fs::File::options()
            .write(true)
            .open(&changed)
            .unwrap()
            .set_modified(before[0].mtime)
            .unwrap();

        let after = scan(dir.path(), false, &[]);
        assert_eq!(
            (after[0].size, after[0].mtime),
            (before[0].size, before[0].mtime)
        );
        assert_ne!(after[0].fingerprint, before[0].fingerprint);
        assert_eq!(after[1].fingerprint, before[1].fingerprint);
        assert_eq!(after[1], before[1]);
    }

    /// The store persists fingerprints: changing the layout needs a migration.
    /// The hex strings were computed apart, with Python's `blake3` package.
    #[test]
    fn fingerprint_layout_is_pinned() {
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jpg");
        // Longer than the 64 KiB prefix, so where it is cut is pinned too.
        let content: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(&path, &content).unwrap();
        // A whole number of 100 ns, the unit of NTFS, so every file system
        // here stores it exactly.
        let mtime = SystemTime::UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_700);
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();

        // The layout of `DiscoveredFile::fingerprint`'s doc, rebuilt by hand.
        let layout = |nanos: i128| {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&70_000u64.to_le_bytes());
            bytes.extend_from_slice(&nanos.to_le_bytes());
            bytes.extend_from_slice(&content[..64 * 1024]);
            blake3::hash(&bytes).to_hex().to_string()
        };

        let files = scan(dir.path(), false, &[]);
        assert_eq!((files[0].size, files[0].mtime), (70_000, mtime));
        assert_eq!(files[0].fingerprint, layout(1_700_000_000_123_456_700));
        assert_eq!(
            files[0].fingerprint,
            "857cd1b9ba39c27237147e8fc3ec2b0bd07ee1414f2c380de3a57f5ae2630b63"
        );

        // Before 1970 the nanoseconds are negative.
        let before_epoch = SystemTime::UNIX_EPOCH - Duration::new(1, 500);
        let early = fingerprint(&path, 70_000, before_epoch).unwrap();
        assert_eq!(early, layout(-1_000_000_500));
        assert_eq!(
            early,
            "3fa6ef154ed979be2335d521b766091bd278570285456da385b1bf1d32ddf850"
        );
    }

    #[test]
    fn missing_input_root_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        match discover(&missing, true, &[]) {
            Err(DiscoverError::Io { path, source }) => {
                assert_eq!(path, missing);
                assert_eq!(source.kind(), io::ErrorKind::NotFound);
            }
            other => panic!("expected an Io error, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_subfolder_is_skipped_not_fatal() {
        use std::os::unix::fs::PermissionsExt;

        /// Gives `path` its mode back on drop, so the temp dir can be removed
        /// even when an assertion fails.
        struct RestoreMode(PathBuf, u32);
        impl Drop for RestoreMode {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(self.1));
            }
        }

        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.jpg");
        let locked_file = touch(dir.path(), "c.jpg");
        touch(dir.path(), "locked/b.jpg");
        let locked_dir = dir.path().join("locked");
        // Declared after `dir`, so they drop (and restore) before it is removed.
        let _restore_dir = RestoreMode(locked_dir.clone(), 0o755);
        let _restore_file = RestoreMode(locked_file.clone(), 0o644);
        fs::set_permissions(&locked_dir, fs::Permissions::from_mode(0o000)).unwrap();
        fs::set_permissions(&locked_file, fs::Permissions::from_mode(0o000)).unwrap();

        let (Err(dir_error), Err(file_error)) =
            (fs::read_dir(&locked_dir), fs::File::open(&locked_file))
        else {
            eprintln!("skipped: permissions are not enforced here (running as root?)");
            return;
        };

        let (files, skipped) = discover(dir.path(), true, &[]).unwrap();
        assert_eq!(rel_paths(dir.path(), &files), [native("a.jpg")]);
        assert_eq!(
            skipped,
            [
                DiscoverSkip {
                    path: locked_file,
                    reason: file_error.to_string(),
                },
                DiscoverSkip {
                    path: locked_dir,
                    reason: dir_error.to_string(),
                },
            ]
        );
    }

    #[test]
    fn symlinked_dirs_are_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "sub/d.jpg");
        // A cycle: following it would never end.
        let sub = dir.path().join("sub");
        for name in ["loop", "loop.jpg"] {
            if !photostacker_testkit::symlink_dir(dir.path(), &sub.join(name)) {
                return;
            }
        }
        let files = scan(dir.path(), true, &[]);
        assert_eq!(rel_paths(dir.path(), &files), [native("sub/d.jpg")]);
    }
}
