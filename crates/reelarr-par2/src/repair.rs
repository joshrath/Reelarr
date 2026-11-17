//! Reconstruct damaged or missing protected files from recovery slices.
//!
//! Writes stay inside the job directory: a temp file is filled, checked against
//! the FileDesc MD5, then renamed over the damaged entry. Intact files are
//! never opened for write. The rename replaces the directory entry with a new
//! inode, so hard-linked view files are not written through.
use crate::{
    Error, Limits, Progress,
    gf::{self, Field},
    packet::{self, Packets, RecoveryLoc},
    verify::{self, FileReport, FileStatus, Outcome, Report, verify_dir},
};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use md5::{Digest, Md5};
use std::{
    collections::{BTreeMap, HashSet, btree_map::Entry as BEntry},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::atomic::AtomicBool,
};

const CHUNK: usize = 64 * 1024;
const TEMP_PREFIX: &str = ".reelarr-repair-";
/// How many windows of recovery rows to try when a matrix is singular.
const MATRIX_ATTEMPTS: usize = 8;

/// What [`repair_dir`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepairReport {
    /// Outcome.
    pub status: RepairStatus,
}

/// Result of a repair attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepairStatus {
    /// Every protected file was already intact.
    AlreadyValid,
    /// These protected files (directory names) were rewritten and now match
    /// their FileDesc MD5.
    Repaired {
        /// Basenames written.
        files: Vec<String>,
    },
    /// Not enough recovery blocks on disk.
    Insufficient {
        /// Recovery blocks needed.
        needed: u64,
        /// Recovery blocks present.
        available: u64,
    },
    /// The main packet or a file's description or checksums are missing.
    Incomplete,
    /// No usable PAR2 data.
    NoParityData,
}

struct MissingSlice {
    file: usize,
    slice: u64,
    constant: u16,
}

/// Repair damaged or missing protected files in `dir` using recovery slices
/// found alongside them. Paths and writes stay inside `dir`.
pub fn repair_dir(
    dir: &Path,
    limits: &Limits,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<RepairReport, Error> {
    let report = verify_dir(dir, limits, cancel, progress)?;
    repair_from_report(dir, limits, cancel, progress, &report)
}

fn repair_from_report(
    dir: &Path,
    limits: &Limits,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
    report: &Report,
) -> Result<RepairReport, Error> {
    let Some(set) = report.primary() else {
        return Ok(RepairReport {
            status: RepairStatus::NoParityData,
        });
    };
    match set.outcome {
        Outcome::Valid => {
            return Ok(RepairReport {
                status: RepairStatus::AlreadyValid,
            });
        }
        Outcome::Incomplete => {
            return Ok(RepairReport {
                status: RepairStatus::Incomplete,
            });
        }
        Outcome::Insufficient { needed, available } => {
            return Ok(RepairReport {
                status: RepairStatus::Insufficient { needed, available },
            });
        }
        Outcome::Repairable { .. } => {}
    }

    packet::check(cancel)?;
    let mut packets = Packets::default();
    let cap = Dir::open_ambient_dir(dir, ambient_authority())?;
    sweep_temps(&cap)?;
    for (source, name) in report.par2_files.iter().enumerate() {
        packet::check(cancel)?;
        let file = cap.open(name)?;
        packets.scan_source(file, source, limits, cancel)?;
    }
    let Some(stored) = packets.sets.get(&set.set_id) else {
        return Ok(RepairReport {
            status: RepairStatus::Incomplete,
        });
    };
    let slice = set.slice_size;
    if slice == 0 || !slice.is_multiple_of(2) {
        return Ok(RepairReport {
            status: RepairStatus::Incomplete,
        });
    }

    let field = Field::new();
    let constants = gf::slice_constants(&field);
    let mut missing = Vec::new();
    let mut present = Vec::new();
    let mut const_idx = 0usize;
    for (fi, file) in set.files.iter().enumerate() {
        let n = file.slices;
        let n_usize = usize::try_from(n).map_err(|_| Error::Limit("slices"))?;
        if const_idx.saturating_add(n_usize) > constants.len() {
            return Err(Error::Limit("slices"));
        }
        let bad: HashSet<u64> = bad_slices(file).into_iter().filter(|&s| s < n).collect();
        for s in 0..n {
            let constant = constants[const_idx + s as usize];
            if bad.contains(&s) {
                missing.push(MissingSlice {
                    file: fi,
                    slice: s,
                    constant,
                });
            } else {
                present.push((fi, s, constant));
            }
        }
        const_idx += n_usize;
    }
    let needed = missing.len();
    if needed == 0 {
        return Ok(RepairReport {
            status: RepairStatus::AlreadyValid,
        });
    }

    // Only the primary set's rows of the right length; data stays on disk.
    let candidates: Vec<(u32, RecoveryLoc)> = stored
        .recovery
        .iter()
        .filter(|&(_, &len)| len == slice)
        .filter_map(|(&e, _)| stored.recovery_at.get(&e).map(|&loc| (e, loc)))
        .collect();
    if candidates.len() < needed {
        return Ok(RepairReport {
            status: RepairStatus::Insufficient {
                needed: needed as u64,
                available: candidates.len() as u64,
            },
        });
    }
    let chunk = usize::try_from(slice.min(CHUNK as u64)).map_err(|_| Error::Limit("slice size"))?;
    if repair_memory_bytes(needed as u64, chunk as u64) > limits.max_repair_bytes {
        return Err(Error::Limit("repair memory"));
    }

    // Pick `needed` rows whose matrix is invertible, sliding over the
    // candidates a bounded number of times if a window is singular.
    let mut chosen = None;
    for start in 0..=(candidates.len() - needed).min(MATRIX_ATTEMPTS - 1) {
        packet::check(cancel)?;
        let rows = &candidates[start..start + needed];
        let matrix: Vec<Vec<u16>> = rows
            .iter()
            .map(|&(exp, _)| missing.iter().map(|m| field.pow(m.constant, exp)).collect())
            .collect();
        if let Some(inv) = gf::invert(&field, &matrix) {
            chosen = Some((rows, inv));
            break;
        }
    }
    let Some((rows, inverse)) = chosen else {
        return Err(Error::Limit("recovery matrix"));
    };

    let mut sources: BTreeMap<usize, cap_std::fs::File> = BTreeMap::new();
    for &(_, loc) in rows {
        if let BEntry::Vacant(v) = sources.entry(loc.source) {
            v.insert(cap.open(&report.par2_files[loc.source])?);
        }
    }
    let mut open_files: BTreeMap<usize, cap_std::fs::File> = BTreeMap::new();
    for (fi, file) in set.files.iter().enumerate() {
        if let Some(name) = &file.found_as
            && let Ok(f) = cap.open(name)
        {
            open_files.insert(fi, f);
        }
    }

    let files_to_write: Vec<usize> = {
        let mut v: Vec<usize> = missing.iter().map(|m| m.file).collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let mut temps: Vec<String> = Vec::new();
    let result = (|| -> Result<Vec<String>, Error> {
        // Every file we rewrite needs a checksum to prove the result.
        let mut plan = Vec::new();
        for &fi in &files_to_write {
            let file = &set.files[fi];
            let desc = stored
                .files
                .get(&file.file_id)
                .ok_or(Error::Limit("file checksum"))?;
            let Some(target) = output_name(file, Some(desc), &report.par2_files) else {
                return Err(Error::Limit("repair target name"));
            };
            plan.push((fi, target, desc.md5));
        }

        // Seed each temp file with the slices that are still good.
        let mut outputs: BTreeMap<usize, cap_std::fs::File> = BTreeMap::new();
        let mut buf = vec![0u8; chunk];
        for &(fi, _, _) in &plan {
            packet::check(cancel)?;
            let file = &set.files[fi];
            let tmp = format!("{TEMP_PREFIX}{fi}");
            let mut out = cap.open_with(
                &tmp,
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true),
            )?;
            temps.push(tmp);
            out.set_len(file.length)?;
            if let Some(src) = open_files.get_mut(&fi) {
                for s in 0..file.slices {
                    if missing.iter().any(|m| m.file == fi && m.slice == s) {
                        continue;
                    }
                    for off in (0..slice).step_by(chunk) {
                        packet::check(cancel)?;
                        let n = (slice - off).min(chunk as u64) as usize;
                        let take = read_range(src, s, slice, file.length, off, &mut buf[..n])?;
                        if take == 0 {
                            break;
                        }
                        out.seek(SeekFrom::Start(s * slice + off))?;
                        out.write_all(&buf[..take])?;
                    }
                }
            }
            outputs.insert(fi, out);
        }

        // Stream: one accumulator chunk per missing slice, one read buffer,
        // one output buffer. Present slices are read one at a time.
        let words = chunk / 2;
        let mut acc = vec![vec![0u16; words]; needed];
        let mut factors = vec![0u16; needed];
        let mut solved = vec![0u8; chunk];
        let work = (present.len() + needed) as u64 * slice;
        let mut done = 0u64;
        progress(Progress {
            bytes_done: 0,
            bytes_total: work,
        });
        for chunk_off in (0..slice).step_by(chunk) {
            packet::check(cancel)?;
            let n = (slice - chunk_off).min(chunk as u64) as usize;
            let w = n / 2;
            for (r, &(_, loc)) in rows.iter().enumerate() {
                let src = sources.get_mut(&loc.source).expect("opened above");
                src.seek(SeekFrom::Start(loc.offset + chunk_off))?;
                src.read_exact(&mut buf[..n])?;
                for (a, b) in acc[r][..w].iter_mut().zip(buf[..n].as_chunks::<2>().0) {
                    *a = u16::from_le_bytes(*b);
                }
            }
            for &(fi, s, constant) in &present {
                packet::check(cancel)?;
                buf[..n].fill(0);
                if let Some(file) = open_files.get_mut(&fi) {
                    read_range(
                        file,
                        s,
                        slice,
                        set.files[fi].length,
                        chunk_off,
                        &mut buf[..n],
                    )?;
                }
                for (f, &(exp, _)) in factors.iter_mut().zip(rows) {
                    *f = field.pow(constant, exp);
                }
                for (row, &f) in acc.iter_mut().zip(&factors) {
                    if f == 0 {
                        continue;
                    }
                    for (a, b) in row[..w].iter_mut().zip(buf[..n].as_chunks::<2>().0) {
                        let word = u16::from_le_bytes(*b);
                        if word != 0 {
                            *a ^= field.mul(word, f);
                        }
                    }
                }
                done = done.saturating_add(n as u64);
            }
            for (c, miss) in missing.iter().enumerate() {
                packet::check(cancel)?;
                let coeffs = &inverse[c];
                for (k, out) in solved[..n].as_chunks_mut::<2>().0.iter_mut().enumerate() {
                    let mut x = 0u16;
                    for (row, &m) in acc.iter().zip(coeffs) {
                        x ^= field.mul(m, row[k]);
                    }
                    *out = x.to_le_bytes();
                }
                let len = set.files[miss.file].length;
                let start = miss.slice * slice + chunk_off;
                if start < len {
                    let take = (len - start).min(n as u64) as usize;
                    let out = outputs.get_mut(&miss.file).expect("created above");
                    out.seek(SeekFrom::Start(start))?;
                    out.write_all(&solved[..take])?;
                }
                done = done.saturating_add(n as u64);
            }
            progress(Progress {
                bytes_done: done,
                bytes_total: work,
            });
        }

        // Prove every temp before renaming any of them into place.
        for &(fi, _, expected) in &plan {
            packet::check(cancel)?;
            let out = outputs.get_mut(&fi).expect("created above");
            out.sync_all()?;
            out.seek(SeekFrom::Start(0))?;
            let mut hasher = Md5::new();
            loop {
                let got = out.read(&mut buf)?;
                if got == 0 {
                    break;
                }
                hasher.update(&buf[..got]);
            }
            if <[u8; 16]>::from(hasher.finalize()) != expected {
                return Err(Error::Limit("repaired file checksum"));
            }
        }
        drop(outputs);
        let mut written = Vec::new();
        for (fi, target, _) in plan {
            packet::check(cancel)?;
            let tmp = format!("{TEMP_PREFIX}{fi}");
            cap.rename(&tmp, &cap, &target)?;
            temps.retain(|t| t != &tmp);
            written.push(target);
        }
        Ok(written)
    })();
    for tmp in temps {
        let _ = cap.remove_file(tmp);
    }
    result.map(|files| RepairReport {
        status: RepairStatus::Repaired { files },
    })
}

/// Peak working memory [`repair_dir`] budgets for `needed` missing slices
/// processed `chunk` bytes at a time: `needed` accumulators plus two I/O
/// buffers, and three `needed`×`needed` matrices of 16-bit words (the
/// system, its inverse, and the copy made while inverting). Compared
/// against [`Limits::max_repair_bytes`].
pub fn repair_memory_bytes(needed: u64, chunk: u64) -> u64 {
    let buffers = needed.saturating_add(2).saturating_mul(chunk);
    let matrices = needed.saturating_mul(needed).saturating_mul(2 * 3);
    buffers.saturating_add(matrices)
}

fn bad_slices(file: &FileReport) -> Vec<u64> {
    match &file.status {
        FileStatus::Intact | FileStatus::Unverifiable => Vec::new(),
        FileStatus::Missing => (0..file.slices).collect(),
        FileStatus::Damaged { bad_slices } => bad_slices.clone(),
    }
}

fn output_name(
    file: &FileReport,
    desc: Option<&crate::FileDesc>,
    par2_files: &[String],
) -> Option<String> {
    if let Some(found) = &file.found_as {
        return Some(found.clone());
    }
    let raw = desc
        .map(|d| d.name.as_slice())
        .unwrap_or(file.name.as_bytes());
    let name = verify::base_name(raw)?.to_string();
    if par2_files.iter().any(|p| p == &name) {
        return None;
    }
    Some(name)
}

fn sweep_temps(dir: &Dir) -> Result<(), Error> {
    let mut stale = Vec::new();
    for entry in dir.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(TEMP_PREFIX) {
            stale.push(name);
        }
    }
    for name in stale {
        let _ = dir.remove_file(name);
    }
    Ok(())
}

fn read_range(
    file: &mut cap_std::fs::File,
    slice: u64,
    slice_size: u64,
    file_len: u64,
    offset: u64,
    buf: &mut [u8],
) -> Result<usize, Error> {
    buf.fill(0);
    let start = slice.saturating_mul(slice_size).saturating_add(offset);
    if start >= file_len {
        return Ok(0);
    }
    let avail = (file_len - start).min(buf.len() as u64) as usize;
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut buf[..avail])?;
    Ok(avail)
}

/// Encode one recovery slice from input slices, using the specification's
/// formula `R_e[j] = sum_i (D_i[j] * (C_i ^ e))` over GF(2^16).
#[cfg(test)]
pub(crate) fn encode_recovery(
    field: &Field,
    constants: &[u16],
    slices: &[&[u8]],
    exponent: u32,
) -> Vec<u8> {
    assert!(!slices.is_empty());
    let n = slices[0].len();
    assert!(n.is_multiple_of(2));
    assert_eq!(constants.len(), slices.len());
    let mut out = vec![0u8; n];
    let factors: Vec<u16> = constants.iter().map(|&c| field.pow(c, exponent)).collect();
    for off in (0..n).step_by(2) {
        let mut acc = 0u16;
        for (i, slice) in slices.iter().enumerate() {
            let word = u16::from_le_bytes([slice[off], slice[off + 1]]);
            acc ^= field.mul(word, factors[i]);
        }
        let bytes = acc.to_le_bytes();
        out[off] = bytes[0];
        out[off + 1] = bytes[1];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_then_solve_recovers_a_missing_slice() {
        let field = Field::new();
        let constants = gf::slice_constants(&field);
        let consts = &constants[..3];
        let s0 = [1u8, 0, 3, 0];
        let s1 = [5u8, 0, 7, 0];
        let s2 = [9u8, 0, 11, 0];
        let slices: [&[u8]; 3] = [&s0, &s1, &s2];
        let rec0 = encode_recovery(&field, consts, &slices, 0);
        // Drop slice 1 and recover it from slices 0 and 2 plus recovery exponent 0.
        let missing = [consts[1]];
        let matrix = vec![vec![field.pow(missing[0], 0)]];
        let inv = gf::invert(&field, &matrix).unwrap();
        for off in (0..4).step_by(2) {
            let rec = u16::from_le_bytes([rec0[off], rec0[off + 1]]);
            let p0 = u16::from_le_bytes([s0[off], s0[off + 1]]);
            let p2 = u16::from_le_bytes([s2[off], s2[off + 1]]);
            let mut acc = rec;
            acc ^= field.mul(p0, field.pow(consts[0], 0));
            acc ^= field.mul(p2, field.pow(consts[2], 0));
            let solved = gf::mul_vec(&field, &inv, &[acc]);
            assert_eq!(solved[0], u16::from_le_bytes([s1[off], s1[off + 1]]));
        }
    }
}
