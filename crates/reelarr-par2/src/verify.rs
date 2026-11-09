//! Verify the files of one flat directory against the PAR2 packets found in it.
use crate::{
    Error, FileId, Limits, Md5Sum, SetId,
    packet::{MAGIC, Packets, SetPackets, check, md5_of, read_full},
};
use cap_std::{ambient_authority, fs::Dir};
use md5::{Digest, Md5};
use std::{
    collections::{HashMap, HashSet},
    io::{Read, Seek, SeekFrom},
    path::Path,
    sync::atomic::AtomicBool,
};

const CHUNK: usize = 64 * 1024;
const HASH_16K: u64 = 16 * 1024;

/// Bytes hashed so far. `bytes_total` grows when a damaged file needs a second,
/// per-slice pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    /// Bytes read.
    pub bytes_done: u64,
    /// Bytes planned.
    pub bytes_total: u64,
}

/// State of one protected file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileStatus {
    /// The declared length and full MD5 match. Extra bytes past the end are
    /// ignored: they don't make the file damaged.
    Intact,
    /// Found, but these zero-based slices don't match. An empty list means no
    /// mismatched slice checksum was found (for example the set has no IFSC).
    Damaged {
        /// Mismatching or absent slice indices.
        bad_slices: Vec<u64>,
    },
    /// No candidate file was found; every slice is needed.
    Missing,
    /// The set has no valid description or slice checksums for this file.
    Unverifiable,
}

/// One protected file of a recovery set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileReport {
    /// PAR2 file ID.
    pub file_id: FileId,
    /// Name from the PAR2 description (lossy UTF-8, untrusted, never a path).
    /// Empty when the description is missing.
    pub name: String,
    /// Directory entry that was verified, if any.
    pub found_as: Option<String>,
    /// Declared length.
    pub length: u64,
    /// Slice count.
    pub slices: u64,
    /// Verification result.
    pub status: FileStatus,
}
impl FileReport {
    /// The file was found under a different name (reported, never applied).
    pub fn renamed(&self) -> bool {
        self.found_as
            .as_deref()
            .is_some_and(|found| found != base_name(self.name.as_bytes()).unwrap_or_default())
    }
    /// Slices this file needs from recovery data.
    pub fn needed(&self) -> u64 {
        match &self.status {
            FileStatus::Intact | FileStatus::Unverifiable => 0,
            FileStatus::Damaged { bad_slices } => bad_slices.len() as u64,
            FileStatus::Missing => self.slices,
        }
    }
}

/// Overall verdict for a recovery set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Every protected file is intact.
    Valid,
    /// Damage fits in the recovery blocks present.
    Repairable {
        /// Recovery blocks needed.
        needed: u64,
        /// Distinct usable recovery blocks present.
        available: u64,
    },
    /// More blocks are needed than are present.
    Insufficient {
        /// Recovery blocks needed.
        needed: u64,
        /// Distinct usable recovery blocks present.
        available: u64,
    },
    /// The main packet or some file's description or checksums are missing.
    Incomplete,
}

/// Result for one recovery set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetReport {
    /// Recovery set ID.
    pub set_id: SetId,
    /// Slice size in bytes (0 when the main packet is missing).
    pub slice_size: u64,
    /// Protected files in main-packet order.
    pub files: Vec<FileReport>,
    /// Distinct usable recovery blocks present in the directory.
    pub recovery_blocks: u64,
    /// Verdict.
    pub outcome: Outcome,
}
impl SetReport {
    /// Recovery blocks needed.
    pub fn needed(&self) -> u64 {
        self.files.iter().map(FileReport::needed).sum()
    }
    fn found(&self) -> usize {
        self.files.iter().filter(|f| f.found_as.is_some()).count()
    }
}

/// Everything verification found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Recovery sets, ordered by ID.
    pub sets: Vec<SetReport>,
    /// Directory entries read as PAR2.
    pub par2_files: Vec<String>,
    /// Corrupt, truncated or malformed packets skipped.
    pub rejected_packets: u64,
    /// Valid packets that disagreed with an earlier one.
    pub conflicting_packets: u64,
}
impl Report {
    /// The set that describes this directory: the one with the most files
    /// found, then the most files. `None` when no PAR2 data was usable.
    pub fn primary(&self) -> Option<&SetReport> {
        self.sets
            .iter()
            .max_by_key(|s| (s.found(), s.files.len(), std::cmp::Reverse(s.set_id)))
    }
}

struct Candidate {
    name: String,
    len: u64,
    md5_16k: Option<Md5Sum>,
}

/// Verify the regular files directly inside `dir` (no recursion, symlinks
/// ignored, nothing outside `dir` opened, nothing written).
pub fn verify_dir(
    dir: &Path,
    limits: &Limits,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<Report, Error> {
    let dir = Dir::open_ambient_dir(dir, ambient_authority())?;
    let mut entries = Vec::new();
    for entry in dir.entries()? {
        check(cancel)?;
        if entries.len() >= limits.max_dir_entries {
            return Err(Error::Limit("directory entries"));
        }
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let meta = dir.symlink_metadata(&name)?;
        if meta.is_file() {
            entries.push((name, meta.len()));
        }
    }
    entries.sort();

    let mut packets = Packets::default();
    let mut par2_files = Vec::new();
    let mut candidates = Vec::new();
    for (name, len) in entries {
        let mut file = dir.open(&name)?;
        let mut magic = [0u8; 8];
        let starts = read_full(&mut file, &mut magic)? && magic == *MAGIC;
        if starts || name.to_ascii_lowercase().ends_with(".par2") {
            file.seek(SeekFrom::Start(0))?;
            packets.scan(file, limits, cancel)?;
            par2_files.push(name);
        } else {
            candidates.push(Candidate {
                name,
                len,
                md5_16k: None,
            });
        }
    }

    let mut meter = Meter {
        progress: Progress::default(),
        report: progress,
    };
    let mut ids: Vec<&SetId> = packets.sets.keys().collect();
    ids.sort();
    let mut sets = Vec::new();
    for id in ids {
        let set = &packets.sets[id];
        sets.push(verify_set(
            *id,
            set,
            &dir,
            &mut candidates,
            cancel,
            &mut meter,
        )?);
    }
    Ok(Report {
        sets,
        par2_files,
        rejected_packets: packets.rejected,
        conflicting_packets: packets.conflicts,
    })
}

struct Meter<'a> {
    progress: Progress,
    report: &'a mut dyn FnMut(Progress),
}
impl Meter<'_> {
    fn plan(&mut self, bytes: u64) {
        self.progress.bytes_total = self.progress.bytes_total.saturating_add(bytes);
        (self.report)(self.progress);
    }
    fn read(&mut self, bytes: u64) {
        self.progress.bytes_done = self.progress.bytes_done.saturating_add(bytes);
        (self.report)(self.progress);
    }
}

/// The last component of an untrusted PAR2 name, if it is a usable file name.
pub(crate) fn base_name(name: &[u8]) -> Option<&str> {
    let name = std::str::from_utf8(name).ok()?;
    let base = name.rsplit(['/', '\\']).next()?;
    (!base.is_empty() && base != "." && base != ".." && !base.contains('\0')).then_some(base)
}

fn verify_set(
    set_id: SetId,
    set: &SetPackets,
    dir: &Dir,
    candidates: &mut [Candidate],
    cancel: &AtomicBool,
    meter: &mut Meter<'_>,
) -> Result<SetReport, Error> {
    let recovery_blocks =
        |slice: u64| set.recovery.values().filter(|&&len| len == slice).count() as u64;
    let Some(main) = &set.main else {
        return Ok(SetReport {
            set_id,
            slice_size: 0,
            files: Vec::new(),
            recovery_blocks: 0,
            outcome: Outcome::Incomplete,
        });
    };
    let slice = main.slice_size;
    let by_name: HashMap<&str, usize> = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| (c.name.as_str(), i))
        .collect();
    let mut claimed = HashSet::new();
    let mut files = Vec::new();
    let mut matches: Vec<Option<usize>> = Vec::new();
    // Duplicate IDs in the main packet are listed once.
    let mut seen = HashSet::new();
    let ids: Vec<FileId> = main
        .recovery_files
        .iter()
        .copied()
        .filter(|id| seen.insert(*id))
        .collect();

    for id in &ids {
        let report = |name: String, length, slices, status| FileReport {
            file_id: *id,
            name,
            found_as: None,
            length,
            slices,
            status,
        };
        let Some(desc) = set.files.get(id) else {
            files.push(report(String::new(), 0, 0, FileStatus::Unverifiable));
            matches.push(None);
            continue;
        };
        let name = String::from_utf8_lossy(&desc.name).into_owned();
        let slices = desc.length.div_ceil(slice);
        let sums_ok = match set.slices.get(id) {
            Some(sums) => sums.len() as u64 == slices,
            None => slices == 0,
        };
        if !sums_ok {
            files.push(report(name, desc.length, slices, FileStatus::Unverifiable));
            matches.push(None);
            continue;
        }
        let hit = base_name(&desc.name)
            .and_then(|n| by_name.get(n).copied())
            .filter(|i| claimed.insert(*i));
        files.push(report(name, desc.length, slices, FileStatus::Missing));
        matches.push(hit);
    }
    // Renamed or obfuscated files: same size and 16 KiB hash.
    for (k, id) in ids.iter().enumerate() {
        if matches[k].is_some() || files[k].status == FileStatus::Unverifiable {
            continue;
        }
        let desc = &set.files[id];
        for (i, cand) in candidates.iter_mut().enumerate() {
            check(cancel)?;
            if cand.len != desc.length || claimed.contains(&i) {
                continue;
            }
            if cand.md5_16k.is_none() {
                cand.md5_16k = Some(hash_16k(dir, &cand.name, cand.len)?);
            }
            if cand.md5_16k == Some(desc.md5_16k) {
                claimed.insert(i);
                matches[k] = Some(i);
                break;
            }
        }
    }
    for k in 0..ids.len() {
        if let Some(i) = matches[k] {
            meter.plan(candidates[i].len.min(files[k].length));
        }
    }
    for (k, id) in ids.iter().enumerate() {
        let Some(i) = matches[k] else { continue };
        let desc = &set.files[id];
        let cand = &candidates[i];
        files[k].found_as = Some(cand.name.clone());
        let intact = cand.len >= desc.length
            && full_md5(dir, &cand.name, desc.length, cancel, meter)? == desc.md5;
        files[k].status = if intact {
            FileStatus::Intact
        } else {
            let sums = set.slices.get(id).map(Vec::as_slice).unwrap_or_default();
            meter.plan(cand.len.min(desc.length));
            let bad_slices = bad_slices(dir, &cand.name, slice, desc.length, sums, cancel, meter)?;
            FileStatus::Damaged { bad_slices }
        };
    }

    let available = recovery_blocks(slice);
    let needed: u64 = files.iter().map(FileReport::needed).sum();
    let outcome = if files.iter().any(|f| f.status == FileStatus::Unverifiable) {
        Outcome::Incomplete
    } else if files.iter().all(|f| f.status == FileStatus::Intact) {
        Outcome::Valid
    } else if needed <= available {
        Outcome::Repairable { needed, available }
    } else {
        Outcome::Insufficient { needed, available }
    };
    Ok(SetReport {
        set_id,
        slice_size: slice,
        files,
        recovery_blocks: available,
        outcome,
    })
}

fn hash_16k(dir: &Dir, name: &str, len: u64) -> Result<Md5Sum, Error> {
    let mut file = dir.open(name)?;
    let mut buf = vec![0; usize::try_from(len.min(HASH_16K)).expect("16 KiB fits")];
    if !read_full(&mut file, &mut buf)? {
        // The file shrank under us; this digest can't match anything real.
        return Ok(md5_of(&[b"reelarr-par2: short read"]));
    }
    Ok(md5_of(&[&buf]))
}

fn full_md5(
    dir: &Dir,
    name: &str,
    length: u64,
    cancel: &AtomicBool,
    meter: &mut Meter<'_>,
) -> Result<Md5Sum, Error> {
    let mut file = dir.open(name)?.take(length);
    let mut hasher = Md5::new();
    let mut buf = vec![0; CHUNK];
    loop {
        check(cancel)?;
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        meter.read(n as u64);
    }
    Ok(hasher.finalize().into())
}

/// Aligned per-slice check. Slices past the end of a short file are bad; the
/// final partial slice is zero-padded as the specification requires.
fn bad_slices(
    dir: &Dir,
    name: &str,
    slice: u64,
    length: u64,
    sums: &[crate::SliceSum],
    cancel: &AtomicBool,
    meter: &mut Meter<'_>,
) -> Result<Vec<u64>, Error> {
    let mut file = dir.open(name)?.take(length);
    let mut bad = Vec::new();
    let mut buf = vec![0; CHUNK];
    let mut eof = false;
    for (index, sum) in sums.iter().enumerate() {
        let index = index as u64;
        let Some(start) = index.checked_mul(slice).filter(|&s| s < length) else {
            bad.push(index);
            continue;
        };
        let want = slice.min(length - start);
        let mut md5 = Md5::new();
        let mut crc = crc32fast::Hasher::new();
        let mut got = 0u64;
        while got < want && !eof {
            check(cancel)?;
            let n = usize::try_from((want - got).min(CHUNK as u64)).expect("chunk fits");
            let read = file.read(&mut buf[..n])?;
            if read == 0 {
                eof = true;
                break;
            }
            md5.update(&buf[..read]);
            crc.update(&buf[..read]);
            got += read as u64;
            meter.read(read as u64);
        }
        if got < want {
            bad.push(index);
            continue;
        }
        let mut pad = slice - want;
        let zeros = [0u8; 4096];
        while pad > 0 {
            let n = pad.min(zeros.len() as u64) as usize;
            md5.update(&zeros[..n]);
            crc.update(&zeros[..n]);
            pad -= n as u64;
        }
        if <[u8; 16]>::from(md5.finalize()) != sum.md5 || crc.finalize() != sum.crc32 {
            bad.push(index);
        }
    }
    Ok(bad)
}
