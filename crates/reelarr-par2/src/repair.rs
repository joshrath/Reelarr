//! Reconstruct damaged or missing protected files from recovery slices.
//!
//! Writes stay inside the job directory: a temp file is filled, checked against
//! the FileDesc MD5, then renamed over the damaged entry. Intact files are
//! never opened for write. The rename replaces the directory entry with a new
//! inode, so hard-linked view files are not written through.
use crate::{
    Error, Limits, Progress,
    gf::{self, Field},
    packet::{self, Packets},
    verify::{self, FileReport, FileStatus, Outcome, Report, verify_dir},
};
use cap_std::{ambient_authority, fs::Dir};
use md5::{Digest, Md5};
use std::{
    collections::{BTreeMap, HashSet},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::atomic::AtomicBool,
};

const CHUNK: usize = 64 * 1024;
const TEMP_PREFIX: &str = ".reelarr-repair-";

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
    let mut packets = Packets {
        keep_recovery_data: true,
        ..Packets::default()
    };
    let cap = Dir::open_ambient_dir(dir, ambient_authority())?;
    sweep_temps(&cap)?;
    for name in &report.par2_files {
        packet::check(cancel)?;
        let file = cap.open(name)?;
        packets.scan(file, limits, cancel)?;
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
    let slice_usize = usize::try_from(slice).map_err(|_| Error::Limit("slice size"))?;

    let field = Field::new();
    let constants = gf::slice_constants(&field);
    let mut missing = Vec::new();
    let mut const_idx = 0usize;
    for (fi, file) in set.files.iter().enumerate() {
        let n = file.slices;
        if const_idx.saturating_add(n as usize) > constants.len() {
            return Err(Error::Limit("slices"));
        }
        for s in bad_slices(file) {
            if s >= n {
                continue;
            }
            missing.push(MissingSlice {
                file: fi,
                slice: s,
                constant: constants[const_idx + s as usize],
            });
        }
        const_idx += n as usize;
    }
    let needed = missing.len();
    if needed == 0 {
        return Ok(RepairReport {
            status: RepairStatus::AlreadyValid,
        });
    }

    let recovery: Vec<(u32, &[u8])> = stored
        .recovery_data
        .iter()
        .filter(|(_, data)| data.len() == slice_usize)
        .map(|(&e, data)| (e, data.as_slice()))
        .take(needed)
        .collect();
    if recovery.len() < needed {
        return Ok(RepairReport {
            status: RepairStatus::Insufficient {
                needed: needed as u64,
                available: stored
                    .recovery_data
                    .values()
                    .filter(|data| data.len() == slice_usize)
                    .count() as u64,
            },
        });
    }
    let stored_bytes = recovery.iter().map(|(_, d)| d.len() as u64).sum::<u64>();
    let reconstruct = (needed as u64).saturating_mul(slice);
    if stored_bytes.saturating_add(reconstruct) > limits.max_repair_bytes {
        return Err(Error::Limit("repair data"));
    }

    let mut matrix = vec![vec![0u16; needed]; needed];
    for (r, &(exp, _)) in recovery.iter().enumerate() {
        for (c, miss) in missing.iter().enumerate() {
            matrix[r][c] = field.pow(miss.constant, exp);
        }
    }
    let Some(inverse) = gf::invert(&field, &matrix) else {
        return Err(Error::Limit("recovery matrix"));
    };

    let mut present = Vec::new();
    const_idx = 0;
    for (fi, file) in set.files.iter().enumerate() {
        let n = file.slices;
        let bad: HashSet<u64> = bad_slices(file).into_iter().collect();
        for s in 0..n {
            if !bad.contains(&s) {
                present.push((fi, s, constants[const_idx + s as usize]));
            }
        }
        const_idx += n as usize;
    }
    let present_factors: Vec<Vec<u16>> = recovery
        .iter()
        .map(|&(exp, _)| {
            present
                .iter()
                .map(|&(_, _, constant)| field.pow(constant, exp))
                .collect()
        })
        .collect();

    let mut open_files: BTreeMap<usize, cap_std::fs::File> = BTreeMap::new();
    for (fi, file) in set.files.iter().enumerate() {
        if let Some(name) = &file.found_as
            && let Ok(f) = cap.open(name)
        {
            open_files.insert(fi, f);
        }
    }

    let mut rebuilt = vec![vec![0u8; slice_usize]; needed];
    let work = (present.len() + needed) as u64 * slice;
    progress(Progress {
        bytes_done: 0,
        bytes_total: work,
    });
    for chunk_off in (0..slice_usize).step_by(CHUNK) {
        packet::check(cancel)?;
        let chunk_len = (slice_usize - chunk_off).min(CHUNK);
        let mut present_data = Vec::with_capacity(present.len());
        for &(fi, slice_i, _) in &present {
            let mut buf = vec![0u8; chunk_len];
            if let Some(file) = open_files.get_mut(&fi) {
                read_range(
                    file,
                    slice_i,
                    slice,
                    set.files[fi].length,
                    chunk_off,
                    &mut buf,
                )?;
            }
            present_data.push(buf);
        }
        for off in (0..chunk_len).step_by(2) {
            let mut rhs = vec![0u16; needed];
            for (r, &(_, rec)) in recovery.iter().enumerate() {
                let mut acc = u16::from_le_bytes([rec[chunk_off + off], rec[chunk_off + off + 1]]);
                for (i, data) in present_data.iter().enumerate() {
                    let word = u16::from_le_bytes([data[off], data[off + 1]]);
                    acc ^= field.mul(word, present_factors[r][i]);
                }
                rhs[r] = acc;
            }
            let solved = gf::mul_vec(&field, &inverse, &rhs);
            for (c, word) in solved.iter().enumerate() {
                let bytes = word.to_le_bytes();
                rebuilt[c][chunk_off + off] = bytes[0];
                rebuilt[c][chunk_off + off + 1] = bytes[1];
            }
        }
        progress(Progress {
            bytes_done: (chunk_off as u64 + chunk_len as u64)
                .saturating_mul((present.len() + needed) as u64),
            bytes_total: work,
        });
    }

    let files_to_write: Vec<usize> = {
        let mut v: Vec<usize> = missing.iter().map(|m| m.file).collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let mut temps = Vec::new();
    let result = (|| -> Result<Vec<String>, Error> {
        let mut written = Vec::new();
        for fi in files_to_write {
            packet::check(cancel)?;
            let file = &set.files[fi];
            let Some(target) =
                output_name(file, stored.files.get(&file.file_id), &report.par2_files)
            else {
                continue;
            };
            let expected_md5 = stored.files.get(&file.file_id).map(|d| d.md5);
            let tmp = format!("{TEMP_PREFIX}{fi}");
            {
                let mut out = cap.create(&tmp)?;
                temps.push(tmp.clone());
                let mut hasher = Md5::new();
                let mut copied = 0u64;
                for s in 0..file.slices {
                    packet::check(cancel)?;
                    let mut buf = vec![0u8; slice_usize];
                    if let Some(idx) = missing.iter().position(|m| m.file == fi && m.slice == s) {
                        buf.copy_from_slice(&rebuilt[idx]);
                    } else if let Some(src) = open_files.get_mut(&fi) {
                        read_range(src, s, slice, file.length, 0, &mut buf)?;
                    }
                    let remain = file.length.saturating_sub(copied);
                    let take = remain.min(slice) as usize;
                    out.write_all(&buf[..take])?;
                    hasher.update(&buf[..take]);
                    copied += take as u64;
                }
                if copied != file.length {
                    return Err(Error::Limit("repaired file length"));
                }
                if let Some(expected) = expected_md5
                    && <[u8; 16]>::from(hasher.finalize()) != expected
                {
                    return Err(Error::Limit("repaired file checksum"));
                }
                out.sync_all()?;
            }
            cap.rename(&tmp, &cap, &target)?;
            temps.pop();
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
    offset: usize,
    buf: &mut [u8],
) -> Result<(), Error> {
    buf.fill(0);
    let start = slice
        .saturating_mul(slice_size)
        .saturating_add(offset as u64);
    if start >= file_len {
        return Ok(());
    }
    let avail = (file_len - start).min(buf.len() as u64) as usize;
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut buf[..avail])?;
    Ok(())
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
