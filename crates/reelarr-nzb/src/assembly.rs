//! Disk-backed assembly of verified yEnc parts in any arrival order.
//!
//! This module performs **blocking** filesystem I/O. Async workers must run it
//! in `tokio::task::spawn_blocking`; no NNTP or runtime policy lives here. The
//! caller supplies a private job directory, not writable/renamable by untrusted
//! processes for the lifetime of the operation. An atomically created private
//! child directory, `create_new` files and validated basenames prevent archive
//! names or pre-existing symlinks from redirecting writes. No existing file is
//! overwritten. There is at most one temporary part plus one bounded output.
//!
//! Completion requires every NZB segment, contiguous non-overlapping coverage,
//! and all advertised checksums. No sidecar state file is used. Restart recovery
//! must revalidate disk artifacts; this API deliberately doesn't trust existing
//! sparse output or persisted checkpoints as evidence of valid bytes.
use crate::{
    JobId, NzbFile, NzbStore, SegmentKey, StoreError,
    yenc::{BodyEncoding, DecodeError, DecodeLimits, Decoder, PartInfo},
};
use crc32fast::Hasher;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    ops::Range,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use thiserror::Error;

static NEXT: AtomicU64 = AtomicU64::new(0);

/// Assembly errors never include attacker-controlled filenames or article text.
#[derive(Debug, Error)]
pub enum AssemblyError {
    /// Invalid or corrupt encoded part.
    #[error(transparent)]
    Decode(#[from] DecodeError),
    /// Filesystem operation failed.
    #[error("assembly I/O failed")]
    Io(#[from] std::io::Error),
    /// A part disagrees with NZB or previously received headers.
    #[error("inconsistent assembly metadata")]
    Metadata,
    /// Duplicate part or overlapping byte interval.
    #[error("duplicate or overlapping part")]
    Overlap,
    /// Missing part, hole or incorrect final length.
    #[error("assembly is incomplete")]
    Incomplete,
    /// Shared database checkpoint failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Owns a new private staging/output directory. Incomplete files are removed on
/// drop. Keep a single instance per logical NZB file; its map is bounded by the
/// NZB's segment count. Dropping is best-effort cleanup, not crash recovery.
pub struct Assembler {
    dir: PathBuf,
    expected: NzbFile,
    limits: DecodeLimits,
    output: Option<File>,
    info: Option<PartInfo>,
    ranges: BTreeMap<u64, u64>,
    parts: BTreeSet<u32>,
    crc: Option<u32>,
    keep: bool,
}
impl Assembler {
    /// Create private storage beneath an existing trusted job directory.
    /// NZB counts are capped at 250,000 and numbering must be contiguous.
    pub fn new(
        job_dir: &Path,
        expected: &NzbFile,
        limits: DecodeLimits,
    ) -> Result<Self, AssemblyError> {
        if expected.segments.is_empty()
            || expected.segments.len() > 250_000
            || expected
                .segments
                .iter()
                .enumerate()
                .any(|(i, s)| s.number as usize != i + 1)
        {
            return Err(AssemblyError::Metadata);
        }
        let job_dir = fs::canonicalize(job_dir)?;
        if !job_dir.is_dir() {
            return Err(AssemblyError::Metadata);
        }
        for _ in 0..64 {
            let dir = job_dir.join(format!(
                ".reelarr-assembly-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&dir) {
                Ok(()) => {
                    return Ok(Self {
                        dir,
                        expected: expected.clone(),
                        limits,
                        output: None,
                        info: None,
                        ranges: BTreeMap::new(),
                        parts: BTreeSet::new(),
                        crc: None,
                        keep: false,
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(AssemblyError::Metadata)
    }
    /// Decode one article to a temporary file, validate it fully, then copy at
    /// its declared offset. A rejected part never becomes completed; retry is
    /// safe. NZB `bytes` are encoded article estimates, not decoded lengths.
    pub fn add_part(
        &mut self,
        number: u32,
        mut body: impl Read,
        encoding: BodyEncoding,
    ) -> Result<PartInfo, AssemblyError> {
        if number == 0 || number as usize > self.expected.segments.len() {
            return Err(AssemblyError::Metadata);
        }
        if self.parts.contains(&number) {
            return Err(AssemblyError::Overlap);
        }
        // The temporary name lives in a distinct subdirectory so even an
        // attachment literally named "part" cannot collide with staging.
        let stage_dir = self.dir.join(".stage");
        fs::create_dir(&stage_dir)?;
        let stage = Stage(stage_dir);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(stage.0.join("part"))?;
        let mut decoder = Decoder::new(&mut file, self.limits, encoding);
        let mut buffer = [0; 16 * 1024];
        loop {
            let n = body.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            decoder.push(&buffer[..n])?;
        }
        let (_, info) = decoder.finish()?;
        if info.name.eq_ignore_ascii_case(".stage") {
            return Err(AssemblyError::Metadata);
        }
        if info.part != number
            || info
                .total
                .is_some_and(|t| t as usize != self.expected.segments.len())
            || (!info.multipart && self.expected.segments.len() != 1)
        {
            return Err(AssemblyError::Metadata);
        }
        if let Some(prior) = &self.info
            && (prior.name != info.name
                || prior.file_size != info.file_size
                || prior.multipart != info.multipart)
        {
            return Err(AssemblyError::Metadata);
        }
        // Neighbor lookups avoid quadratic CPU cost for hostile high-part-count NZBs.
        if self
            .ranges
            .range(..=info.start)
            .next_back()
            .is_some_and(|(_, &end)| end > info.start)
            || self
                .ranges
                .range(info.start..)
                .next()
                .is_some_and(|(&start, _)| start < info.end)
        {
            return Err(AssemblyError::Overlap);
        }
        if self.crc.zip(info.file_crc32).is_some_and(|(a, b)| a != b) {
            return Err(AssemblyError::Metadata);
        }
        if self.output.is_none() {
            let output = OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(self.dir.join(&info.name))?;
            self.output = Some(output);
            self.info = Some(info.clone());
        }
        let output = self.output.as_mut().ok_or(AssemblyError::Metadata)?;
        // Repeat set_len on retry if a prior allocation failed; cleanup still
        // owns the created file even on this error path.
        output.set_len(info.file_size)?;
        output.seek(SeekFrom::Start(info.start))?;
        file.seek(SeekFrom::Start(0))?;
        let copied = std::io::copy(&mut file, output)?;
        if copied != info.end - info.start {
            return Err(AssemblyError::Metadata);
        }
        output.flush()?;
        // Metadata is only committed after all writes succeed. Full fsync and
        // directory durability happen at finish before checkpointing.
        self.ranges.insert(info.start, info.end);
        self.parts.insert(number);
        self.crc = self.crc.or(info.file_crc32);
        Ok(info)
    }
    /// Number of validated, written parts (not yet a durable DB checkpoint).
    pub fn completed_parts(&self) -> usize {
        self.parts.len()
    }
    /// Remaining NZB segment numbers, in NZB order.
    pub fn missing_parts(&self) -> impl Iterator<Item = u32> + '_ {
        self.expected
            .segments
            .iter()
            .map(|s| s.number)
            .filter(|n| !self.parts.contains(n))
    }
    /// Require all parts and exact coverage, stream full-file CRC in disk order,
    /// then fsync bytes and directory entries. Only this receipt can checkpoint.
    pub fn finish(mut self) -> Result<CompletedFile, AssemblyError> {
        if self.parts.len() != self.expected.segments.len() {
            return Err(AssemblyError::Incomplete);
        }
        let info = self.info.as_ref().ok_or(AssemblyError::Incomplete)?;
        let mut cursor = 0;
        for (&start, &end) in &self.ranges {
            if start != cursor {
                return Err(AssemblyError::Incomplete);
            }
            cursor = end;
        }
        if cursor != info.file_size {
            return Err(AssemblyError::Incomplete);
        }
        let output = self.output.as_mut().ok_or(AssemblyError::Incomplete)?;
        if output.metadata()?.len() != info.file_size {
            return Err(AssemblyError::Incomplete);
        }
        if let Some(expected) = self.crc {
            let mut hasher = Hasher::new();
            let mut buf = [0; 64 * 1024];
            output.seek(SeekFrom::Start(0))?;
            loop {
                let n = output.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            if hasher.finalize() != expected {
                return Err(DecodeError::Crc.into());
            }
        }
        output.sync_all()?;
        File::open(&self.dir)?.sync_all()?;
        let parent = self.dir.parent().ok_or(AssemblyError::Metadata)?;
        File::open(parent)?.sync_all()?;
        let result = CompletedFile {
            path: self.dir.join(&info.name),
            expected: self.expected.clone(),
            whole_crc_verified: self.crc.is_some(),
        };
        self.keep = true;
        Ok(result)
    }
}
impl Assembler {
    /// Keep an incomplete file for later PAR2 repair instead of deleting it.
    /// The file is sized to its declared length; byte ranges never written are
    /// zero (sparse where the filesystem allows) and listed in
    /// [`PartialFile::missing`]. Written ranges hold validated part bytes, but
    /// the whole file has no CRC guarantee. `None` when no part was written.
    pub fn keep_partial(mut self) -> Result<Option<PartialFile>, AssemblyError> {
        let (Some(info), Some(output)) = (self.info.clone(), self.output.as_mut()) else {
            return Ok(None);
        };
        output.set_len(info.file_size)?;
        output.sync_all()?;
        let mut missing = Vec::new();
        let mut cursor = 0;
        for (&start, &end) in &self.ranges {
            if start > cursor {
                missing.push(cursor..start);
            }
            cursor = cursor.max(end);
        }
        if cursor < info.file_size {
            missing.push(cursor..info.file_size);
        }
        self.keep = true;
        Ok(Some(PartialFile {
            path: self.dir.join(&info.name),
            file_size: info.file_size,
            missing,
        }))
    }
}

/// An incomplete file kept by [`Assembler::keep_partial`]. The caller owns the
/// file and its private directory and must move or remove them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartialFile {
    /// File inside the assembler's private directory.
    pub path: PathBuf,
    /// Declared whole-file size (the file's length on disk).
    pub file_size: u64,
    /// Zero-filled ranges that were never written, ascending.
    pub missing: Vec<Range<u64>>,
}

struct Stage(PathBuf);
impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0.join("part"));
        let _ = fs::remove_dir(&self.0);
    }
}
impl Drop for Assembler {
    fn drop(&mut self) {
        if !self.keep {
            self.output.take();
            if let Some(info) = &self.info {
                let _ = fs::remove_file(self.dir.join(&info.name));
            }
            let _ = fs::remove_dir(&self.dir);
        }
    }
}

/// Successful durable assembly. The source remains owned by the download job;
/// it must not be changed or deleted before database checkpoint/import completes.
pub struct CompletedFile {
    path: PathBuf,
    expected: NzbFile,
    whole_crc_verified: bool,
}
impl CompletedFile {
    /// Final single-file path inside the private job directory.
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// False means the sender omitted a whole-file CRC, NOT a verified checksum.
    /// Multipart part CRCs are mandatory and have always been verified.
    pub fn whole_crc_verified(&self) -> bool {
        self.whole_crc_verified
    }
    /// Checkpoint all segments atomically after durable completion. The stored
    /// file must match the original NZB file. Callers must reconcile artifacts
    /// before trusting these checkpoints after restart; no state file is added.
    pub async fn checkpoint(
        &self,
        store: &impl NzbStore,
        job_id: JobId,
        file_index: usize,
    ) -> Result<(), AssemblyError> {
        let stored = store.load(job_id).await?.ok_or(StoreError::NotFound)?;
        if stored.document.files.get(file_index) != Some(&self.expected) {
            return Err(AssemblyError::Metadata);
        }
        let updates: Vec<_> = self
            .expected
            .segments
            .iter()
            .map(|s| {
                (
                    SegmentKey {
                        file_index,
                        number: s.number,
                    },
                    true,
                )
            })
            .collect();
        store.checkpoint(job_id, &updates).await?;
        Ok(())
    }
}
