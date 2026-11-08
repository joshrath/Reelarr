//! Bounded, clean-room stored-only RAR4/RAR5 extraction.
//!
//! Open an explicitly ordered volume set with [`VolumeSet::open`], then call
//! [`VolumeSet::extract`]. Success returns an owned private directory containing
//! verified payload only. Failure/cancellation removes staging automatically.
//! No native extractor is invoked; see the crate README and SOURCES.md.
#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![doc = include_str!("../README.md")]
mod format;
use blake2s_simd::blake2sp;
use cap_std::{
    ambient_authority,
    fs::{Dir, File, OpenOptions},
};
use format::{Block, Entry};
use std::{
    collections::{HashMap, HashSet},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};
use tempfile::TempDir;
use unicode_normalization::UnicodeNormalization;

/// Stable extraction failures; unsupported input is never silently extracted.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A documented but unimplemented format feature.
    #[error("unsupported archive: {reason}")]
    Unsupported {
        /// Human-readable unsupported feature.
        reason: &'static str,
    },
    /// Malformed structural metadata.
    #[error("corrupt archive: {0}")]
    Corrupt(&'static str),
    /// Missing bytes in a header or stream.
    #[error("truncated archive")]
    Truncated,
    /// Header, part or full-file digest mismatch.
    #[error("checksum mismatch: {0}")]
    Checksum(&'static str),
    /// Strict policy requires a final-file digest.
    #[error("missing usable integrity checksum")]
    MissingIntegrityChecksum,
    /// Nonsequential, missing, duplicated or mixed volumes/continuations.
    #[error("invalid volume set: {0}")]
    Volumes(&'static str),
    /// Path is unsafe or ambiguous across supported filesystems.
    #[error("unsafe archive path")]
    UnsafePath,
    /// Case/Unicode-normalized paths collide or overlap a file.
    #[error("colliding archive paths")]
    Collision,
    /// A caller-supplied resource budget was exceeded.
    #[error("archive limit exceeded: {0}")]
    Limit(&'static str),
    /// Cooperative cancellation observed before publication.
    #[error("extraction cancelled")]
    Cancelled,
    /// Filesystem I/O failed.
    #[error("archive I/O error: {0}")]
    Io(std::io::Error),
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        if let Some(signal) = e.get_ref().and_then(|v| v.downcast_ref::<ReadSignal>()) {
            return match signal {
                ReadSignal::Cancelled => Self::Cancelled,
                ReadSignal::Limit => Self::Limit("input bytes"),
            };
        }
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            Self::Truncated
        } else {
            Self::Io(e)
        }
    }
}
/// Result used by this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Budgets cover all volumes together. Streaming uses a fixed 64 KiB buffer.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Maximum number of volumes/open input handles.
    pub max_volumes: usize,
    /// Maximum individual header allocation (RAR5 itself caps size vints at 3 bytes).
    pub max_header_bytes: usize,
    /// Maximum headers, including ignored metadata (work budget).
    pub max_headers: u64,
    /// Maximum logical files plus explicit directories.
    pub max_entries: usize,
    /// Maximum sum of bytes read, including metadata and skipped service streams.
    pub max_input_bytes: u64,
    /// Maximum cumulative payload bytes on disk, including partial output.
    pub max_output_bytes: u64,
    /// Maximum individual logical file size.
    pub max_file_bytes: u64,
    /// Maximum nested path components.
    pub max_path_depth: usize,
    /// Maximum UTF-8 bytes in a full normalized path.
    pub max_path_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_volumes: 1000,
            max_header_bytes: 2 * 1024 * 1024,
            max_headers: 100_000,
            max_entries: 10_000,
            max_input_bytes: 100 * 1024 * 1024 * 1024,
            max_output_bytes: 100 * 1024 * 1024 * 1024,
            max_file_bytes: 50 * 1024 * 1024 * 1024,
            max_path_depth: 32,
            max_path_bytes: 4096,
        }
    }
}
/// Manifest entry. Directories have zero bytes; paths are relative to extraction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractedEntry {
    /// Validated relative path; no metadata/sidecar streams are restored.
    pub path: PathBuf,
    /// Verified payload bytes.
    pub bytes: u64,
    /// Whether this is an explicit directory entry.
    pub directory: bool,
}
/// Throttling is left to the caller. Called after each bounded read/write chunk.
#[derive(Clone, Copy, Debug)]
pub struct Progress {
    /// Zero-based volume currently being consumed.
    pub volume: usize,
    /// Cumulative extracted bytes (includes an in-progress file).
    pub bytes_written: u64,
    /// Completed logical entries.
    pub entries_done: usize,
}
/// Owned verified output. Dropping this removes payload unless [`Self::keep`] is called.
#[derive(Debug)]
pub struct Extraction {
    directory: TempDir,
    manifest: Vec<ExtractedEntry>,
}
impl Extraction {
    /// Private verified directory, created underneath the caller's output parent.
    pub fn path(&self) -> &Path {
        self.directory.path()
    }
    /// Logical payload manifest, excluding service data.
    pub fn manifest(&self) -> &[ExtractedEntry] {
        &self.manifest
    }
    /// Transfer cleanup responsibility to the caller (#14 owns final publication).
    pub fn keep(self) -> PathBuf {
        self.directory.keep()
    }
}
/// Explicitly ordered volume set. No directory scanning or SFX signature search.
/// Filenames must use `name.partNN.rar` (starting at 1) or `name.rar/name.r00/...`.
#[derive(Debug)]
pub struct VolumeSet {
    files: Vec<std::fs::File>,
    names: Naming,
    limits: Limits,
}
#[derive(Debug)]
struct Naming {
    new: bool,
}
impl VolumeSet {
    /// Open regular files, validate naming/order/identity and bound total input.
    /// Pass only trusted source paths; input is never modified.
    pub fn open(paths: &[PathBuf], limits: Limits) -> Result<Self> {
        if paths.is_empty() {
            return Err(Error::Volumes("empty set"));
        }
        if paths.len() > limits.max_volumes {
            return Err(Error::Limit("volumes"));
        }
        let (parent, stem, width) = first_name(&paths[0])?;
        let mut files = Vec::new();
        let mut seen = HashSet::new();
        let mut total = 0u64;
        for (i, p) in paths.iter().enumerate() {
            if std::fs::canonicalize(p.parent().unwrap_or(Path::new(".")))? != parent {
                return Err(Error::Volumes("mixed volume directories"));
            }
            let expected = if width > 0 {
                format!("{stem}.part{:0width$}.rar", i + 1)
            } else if i == 0 {
                format!("{stem}.rar")
            } else {
                let n = i - 1;
                if n >= 900 {
                    return Err(Error::Volumes("old-style volume range"));
                }
                format!(
                    "{stem}.{}{:02}",
                    char::from(b'r' + (n / 100) as u8),
                    n % 100
                )
            };
            if p.file_name().and_then(|n| n.to_str()) != Some(expected.as_str()) {
                return Err(Error::Volumes(
                    "missing, reordered, duplicate or mixed filename",
                ));
            }
            if !seen.insert(std::fs::canonicalize(p).map_err(source_io)?) {
                return Err(Error::Volumes("duplicate input file"));
            }
            let file = std::fs::File::open(p).map_err(source_io)?;
            let metadata = file.metadata()?;
            if !metadata.is_file() {
                return Err(Error::Volumes("input is not a regular file"));
            }
            total = total
                .checked_add(metadata.len())
                .ok_or(Error::Limit("input bytes"))?;
            if total > limits.max_input_bytes {
                return Err(Error::Limit("input bytes"));
            }
            files.push(file);
        }
        Ok(Self {
            files,
            names: Naming { new: width > 0 },
            limits,
        })
    }
    /// Extract into a fresh private staging directory under a canonicalized parent.
    /// Capability-relative file operations confine all archive writes to staging.
    /// No existing files are overwritten. All writers close before success.
    /// The output parent must be trusted against rename/replacement by other actors.
    /// Cancellation is checked for every header and at most each 64 KiB of input.
    pub fn extract(
        self,
        output_parent: &Path,
        cancel: &AtomicBool,
        mut progress: impl FnMut(Progress),
    ) -> Result<Extraction> {
        check_cancel(cancel)?;
        let parent = std::fs::canonicalize(output_parent)?;
        let directory = tempfile::Builder::new()
            .prefix(".reelarr-rar-")
            .tempdir_in(parent)?;
        let dir = Dir::open_ambient_dir(directory.path(), ambient_authority())?;
        let limits = &self.limits;
        let mut state = State {
            dir,
            limits,
            cancel,
            progress: &mut progress,
            paths: HashMap::new(),
            explicit: HashSet::new(),
            manifest: Vec::new(),
            active: None,
            total: 0,
            headers: 0,
            entries: 0,
        };
        let mut input = 0;
        let mut format = None;
        let count = self.files.len();
        for (index, file) in self.files.into_iter().enumerate() {
            let mut r = Input {
                file,
                crc: crc32fast::Hasher::new(),
                input: &mut input,
                limit: limits.max_input_bytes,
                cancel,
            };
            let mut signature = [0; 7];
            r.read_exact(&mut signature)?;
            let v5 = if signature == format::RAR4 {
                false
            } else if signature == format::RAR5[..7] {
                let mut last = [0];
                r.read_exact(&mut last)?;
                if last[0] != 0 {
                    return Err(Error::Unsupported {
                        reason: "archive signature or SFX",
                    });
                }
                true
            } else {
                return Err(Error::Unsupported {
                    reason: "archive signature or SFX",
                });
            };
            if format.is_some_and(|f| f != v5) {
                return Err(Error::Volumes("mixed RAR versions"));
            }
            format = Some(v5);
            let mut main = false;
            let mut first_file = true;
            let mut split_on_this_volume = false;
            loop {
                check_cancel(cancel)?;
                state.headers = state
                    .headers
                    .checked_add(1)
                    .ok_or(Error::Limit("headers/work"))?;
                if state.headers > limits.max_headers {
                    return Err(Error::Limit("headers/work"));
                }
                let archive_crc = r.crc.clone().finalize();
                let block = if v5 {
                    format::read5(&mut r, limits)?
                } else {
                    format::read4(&mut r, limits)?
                };
                if !main && !matches!(block, Block::Main { .. }) {
                    return Err(Error::Corrupt("main header must follow signature"));
                }
                match block {
                    Block::Main {
                        volume,
                        number,
                        new_names,
                    } => {
                        if main {
                            return Err(Error::Corrupt("duplicate main header"));
                        }
                        main = true;
                        if (!volume && count > 1)
                            || (v5 && number != index as u64)
                            || (!v5 && volume && ((index == 0) != (number == 0)))
                        {
                            return Err(Error::Volumes("volume header order/first-volume flags"));
                        }
                        if volume && new_names.is_some_and(|new| new != self.names.new) {
                            return Err(Error::Volumes("volume naming/header mismatch"));
                        }
                    }
                    Block::File(entry) => {
                        if split_on_this_volume {
                            return Err(Error::Volumes("file after split part before next volume"));
                        }
                        if entry.before && (!first_file || index == 0) {
                            return Err(Error::Volumes("unexpected continued file"));
                        }
                        if state.active.is_some() && !first_file {
                            return Err(Error::Volumes("continuation not first file"));
                        }
                        first_file = false;
                        split_on_this_volume = entry.after;
                        state.file(entry, &mut r, index)?;
                    }
                    Block::Skip(bytes) => state.skip(&mut r, bytes, index)?,
                    Block::End { more, crc, number } => {
                        if crc.is_some_and(|c| c != archive_crc) {
                            return Err(Error::Checksum("RAR4 volume"));
                        }
                        if !v5 && count > 1 && number.is_none() {
                            return Err(Error::Unsupported {
                                reason: "RAR4 multivolume requires end volume numbers",
                            });
                        }
                        if number.is_some_and(|n| n != index as u64) {
                            return Err(Error::Volumes("RAR4 end volume number"));
                        }
                        if more != (index + 1 < count) {
                            return Err(Error::Volumes("missing next volume or extra volume"));
                        }
                        if state.active.is_some() && (!split_on_this_volume || !more) {
                            return Err(Error::Volumes("missing split continuation"));
                        }
                        // Stored volume generators can zero-pad to the requested volume size.
                        // Consume padding under the input budget; never parse hidden records.
                        let mut padding = [0; 65536];
                        loop {
                            let n = r.read(&mut padding)?;
                            if n == 0 {
                                break;
                            }
                            if padding[..n].iter().any(|b| *b != 0) {
                                return Err(Error::Unsupported {
                                    reason: "nonzero trailing archive data",
                                });
                            }
                            state.tick(index)?;
                        }
                        break;
                    }
                }
            }
        }
        check_cancel(cancel)?;
        if state.active.is_some() {
            return Err(Error::Volumes("unfinished split file"));
        }
        let manifest = std::mem::take(&mut state.manifest);
        drop(state);
        Ok(Extraction {
            directory,
            manifest,
        })
    }
}
fn source_io(e: std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        Error::Volumes("missing volume file")
    } else {
        e.into()
    }
}
fn first_name(p: &Path) -> Result<(PathBuf, String, usize)> {
    let parent = std::fs::canonicalize(p.parent().unwrap_or(Path::new(".")))?;
    let name = p
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or(Error::Volumes("non-UTF8 volume name"))?;
    let stem = name
        .strip_suffix(".rar")
        .ok_or(Error::Volumes("first volume must end in .rar"))?;
    if let Some((base, digits)) = stem.rsplit_once(".part") {
        if digits.is_empty()
            || !digits.bytes().all(|b| b.is_ascii_digit())
            || digits.parse::<u64>().ok() != Some(1)
            || base.is_empty()
        {
            return Err(Error::Volumes("invalid first part number"));
        }
        Ok((parent, base.into(), digits.len()))
    } else if stem.is_empty() {
        Err(Error::Volumes("empty volume stem"))
    } else {
        Ok((parent, stem.into(), 0))
    }
}
fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}
#[derive(Debug, thiserror::Error)]
enum ReadSignal {
    #[error("cancelled")]
    Cancelled,
    #[error("input limit")]
    Limit,
}
struct Input<'a> {
    file: std::fs::File,
    crc: crc32fast::Hasher,
    input: &'a mut u64,
    limit: u64,
    cancel: &'a AtomicBool,
}
impl Read for Input<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(std::io::Error::other(ReadSignal::Cancelled));
        }
        let length = buffer.len().min(65536);
        let n = self.file.read(&mut buffer[..length])?;
        *self.input = self
            .input
            .checked_add(n as u64)
            .ok_or_else(|| std::io::Error::other(ReadSignal::Limit))?;
        if *self.input > self.limit {
            return Err(std::io::Error::other(ReadSignal::Limit));
        }
        self.crc.update(&buffer[..n]);
        Ok(n)
    }
}
struct Active {
    entry: Entry,
    file: File,
    bytes: u64,
    crc: crc32fast::Hasher,
    hash: blake2sp::State,
}
struct State<'a, F> {
    dir: Dir,
    limits: &'a Limits,
    cancel: &'a AtomicBool,
    progress: &'a mut F,
    paths: HashMap<String, (bool, String)>,
    explicit: HashSet<String>,
    manifest: Vec<ExtractedEntry>,
    active: Option<Active>,
    total: u64,
    headers: u64,
    entries: usize,
}
impl<F: FnMut(Progress)> State<'_, F> {
    fn tick(&mut self, volume: usize) -> Result<()> {
        (self.progress)(Progress {
            volume,
            bytes_written: self.total,
            entries_done: self.manifest.len(),
        });
        check_cancel(self.cancel)
    }
    fn register(&mut self, path: &str, directory: bool) -> Result<()> {
        let key = collision_key(path);
        if !self.explicit.insert(key.clone()) {
            return Err(Error::Collision);
        }
        let parts: Vec<_> = key.split('/').collect();
        let originals: Vec<_> = path.split('/').collect();
        for n in 1..=parts.len() {
            let p = parts[..n].join("/");
            let is_dir = n < parts.len() || directory;
            let original = originals[..n].join("/");
            if let Some((old, spelling)) = self.paths.get(&p) {
                if !old || !is_dir || spelling != &original {
                    return Err(Error::Collision);
                }
            } else {
                if self.paths.len()
                    >= self
                        .limits
                        .max_entries
                        .saturating_mul(self.limits.max_path_depth)
                {
                    return Err(Error::Limit("implicit directories"));
                }
                self.paths.insert(p, (is_dir, original));
            }
        }
        Ok(())
    }
    fn file(&mut self, entry: Entry, r: &mut impl Read, volume: usize) -> Result<()> {
        safe_path(&entry.name, self.limits)?;
        if entry.size > self.limits.max_file_bytes {
            return Err(Error::Limit("per-file bytes"));
        }
        if entry.directory {
            if self.active.is_some()
                || entry.before
                || entry.after
                || entry.size != 0
                || entry.packed != 0
            {
                return Err(Error::Corrupt("directory stream"));
            }
            verify_hash(
                &entry,
                &crc32fast::Hasher::new(),
                &blake2sp::State::new(),
                false,
            )?;
            self.new_entry()?;
            self.register(&entry.name, true)?;
            self.dir.create_dir_all(&entry.name)?;
            self.manifest.push(ExtractedEntry {
                path: entry.name.into(),
                bytes: 0,
                directory: true,
            });
            return self.tick(volume);
        }
        match &self.active {
            Some(a)
                if !entry.before
                    || a.entry.name != entry.name
                    || a.entry.size != entry.size
                    || a.entry.identity != entry.identity =>
            {
                return Err(Error::Volumes("split continuation identity mismatch"));
            }
            None if entry.before => return Err(Error::Volumes("missing preceding file part")),
            _ => {}
        }
        if self.active.is_none() {
            self.new_entry()?;
            self.register(&entry.name, false)?;
            if entry.size > self.limits.max_output_bytes.saturating_sub(self.total) {
                return Err(Error::Limit("total output bytes"));
            }
            if let Some(parent) = Path::new(&entry.name).parent()
                && !parent.as_os_str().is_empty()
            {
                self.dir.create_dir_all(parent)?;
            }
            let file = self
                .dir
                .open_with(&entry.name, OpenOptions::new().write(true).create_new(true))?;
            self.active = Some(Active {
                entry: entry.clone(),
                file,
                bytes: 0,
                crc: crc32fast::Hasher::new(),
                hash: blake2sp::State::new(),
            });
        }
        let a = self.active.as_ref().unwrap();
        if entry.packed > entry.size.saturating_sub(a.bytes) {
            return Err(Error::Corrupt("stored size exceeds declared file size"));
        }
        let mut crc = crc32fast::Hasher::new();
        let mut hash = blake2sp::State::new();
        let mut left = entry.packed;
        let mut buffer = [0; 65536];
        while left > 0 {
            check_cancel(self.cancel)?;
            let n = left.min(buffer.len() as u64) as usize;
            r.read_exact(&mut buffer[..n])?;
            let a = self.active.as_mut().unwrap();
            a.file.write_all(&buffer[..n])?;
            a.bytes += n as u64;
            self.total += n as u64;
            crc.update(&buffer[..n]);
            hash.update(&buffer[..n]);
            a.crc.update(&buffer[..n]);
            a.hash.update(&buffer[..n]);
            left -= n as u64;
            self.tick(volume)?;
        }
        if entry.after {
            verify_hash(&entry, &crc, &hash, true)?;
        } else {
            let a = self.active.take().unwrap();
            if a.bytes != entry.size {
                return Err(Error::Corrupt("stored file size mismatch"));
            }
            verify_hash(&entry, &a.crc, &a.hash, true)?;
            a.file.sync_all()?;
            drop(a.file);
            self.manifest.push(ExtractedEntry {
                path: entry.name.into(),
                bytes: a.bytes,
                directory: false,
            });
        }
        self.tick(volume)
    }
    fn new_entry(&mut self) -> Result<()> {
        self.entries = self.entries.checked_add(1).ok_or(Error::Limit("entries"))?;
        if self.entries > self.limits.max_entries {
            return Err(Error::Limit("entries"));
        }
        Ok(())
    }
    fn skip(&mut self, r: &mut impl Read, mut bytes: u64, volume: usize) -> Result<()> {
        let mut buf = [0; 65536];
        while bytes > 0 {
            check_cancel(self.cancel)?;
            let n = bytes.min(buf.len() as u64) as usize;
            r.read_exact(&mut buf[..n])?;
            bytes -= n as u64;
            self.tick(volume)?;
        }
        Ok(())
    }
}
fn verify_hash(
    e: &Entry,
    crc: &crc32fast::Hasher,
    hash: &blake2sp::State,
    required: bool,
) -> Result<()> {
    if required && e.crc.is_none() && e.hash.is_none() {
        return Err(Error::MissingIntegrityChecksum);
    }
    if e.crc.is_some_and(|c| c != crc.clone().finalize()) {
        return Err(Error::Checksum("file/part CRC32"));
    }
    if e.hash
        .as_ref()
        .is_some_and(|h| h.as_slice() != hash.finalize().as_bytes())
    {
        return Err(Error::Checksum("file/part BLAKE2sp"));
    }
    Ok(())
}
fn collision_key(s: &str) -> String {
    s.nfkc().flat_map(char::to_lowercase).collect()
}
fn safe_path(s: &str, limits: &Limits) -> Result<()> {
    if s.len() > limits.max_path_bytes {
        return Err(Error::Limit("path bytes"));
    }
    if s.is_empty() || s.contains('\\') {
        return Err(Error::UnsafePath);
    }
    if s.split('/').count() > limits.max_path_depth {
        return Err(Error::Limit("path depth"));
    }
    for p in s.split('/') {
        if p.is_empty()
            || p == "."
            || p == ".."
            || p.len() > 255
            || p.starts_with(' ')
            || p.ends_with(['.', ' '])
            || p.chars().any(|c| {
                c.is_control()
                    || ":<>\"|?*".contains(c)
                    || ('\u{200b}'..='\u{200f}').contains(&c)
                    || ('\u{fdd0}'..='\u{fdef}').contains(&c)
                    || (c as u32 & 0xffff) >= 0xfffe
                    || c == '\u{feff}'
                    || ('\u{202a}'..='\u{202e}').contains(&c)
                    || ('\u{2066}'..='\u{2069}').contains(&c)
            })
        {
            return Err(Error::UnsafePath);
        }
        // NFKC prevents compatibility characters from aliasing reserved names.
        let normalized = collision_key(p);
        if normalized != p.to_lowercase() {
            return Err(Error::UnsafePath);
        }
        let base = normalized.split('.').next().unwrap_or("");
        if ["con", "prn", "aux", "nul", "clock$", "conin$", "conout$"].contains(&base)
            || (base.len() == 4
                && (base.starts_with("com") || base.starts_with("lpt"))
                && base.as_bytes()[3].is_ascii_digit())
        {
            return Err(Error::UnsafePath);
        }
    }
    Ok(())
}
