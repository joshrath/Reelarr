//! Bounded, resynchronizing PAR2 packet scanner. Every packet is hostile input:
//! the header, length and packet MD5 are validated before the body is used, and
//! a bad packet only costs a forward search for the next magic sequence.
use crate::{Error, FileId, Limits, Md5Sum, SetId};
use md5::{Digest, Md5};
use std::{
    collections::{BTreeMap, HashMap, btree_map::Entry as BEntry},
    io::{self, Read, Seek, SeekFrom},
    sync::atomic::{AtomicBool, Ordering},
};

pub(crate) const MAGIC: &[u8; 8] = b"PAR2\0PKT";
pub(crate) const HEADER: u64 = 64;
pub(crate) const MAIN: &[u8; 16] = b"PAR 2.0\0Main\0\0\0\0";
pub(crate) const FILE_DESC: &[u8; 16] = b"PAR 2.0\0FileDesc";
pub(crate) const IFSC: &[u8; 16] = b"PAR 2.0\0IFSC\0\0\0\0";
pub(crate) const RECOVERY: &[u8; 16] = b"PAR 2.0\0RecvSlic";
const CHUNK: usize = 64 * 1024;

/// Parsed main packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Main {
    /// Slice (block) size in bytes; a positive multiple of 4.
    pub slice_size: u64,
    /// File IDs protected by recovery data, in packet order.
    pub recovery_files: Vec<FileId>,
    /// File IDs described but not protected.
    pub other_files: Vec<FileId>,
}
/// Parsed file description packet. `name` is untrusted bytes, never a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDesc {
    /// MD5 of the whole file.
    pub md5: Md5Sum,
    /// MD5 of the first 16 KiB (or the whole file if shorter).
    pub md5_16k: Md5Sum,
    /// File length in bytes.
    pub length: u64,
    /// Name bytes with trailing NUL padding removed.
    pub name: Vec<u8>,
}
/// One input-file slice checksum pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SliceSum {
    /// MD5 of the zero-padded slice.
    pub md5: Md5Sum,
    /// CRC32 of the zero-padded slice.
    pub crc32: u32,
}

/// Everything valid found for one recovery set.
#[derive(Clone, Debug, Default)]
pub struct SetPackets {
    /// First valid main packet.
    pub main: Option<Main>,
    /// File descriptions by file ID (first valid wins).
    pub files: HashMap<FileId, FileDesc>,
    /// Slice checksums by file ID (first valid wins).
    pub slices: HashMap<FileId, Vec<SliceSum>>,
    /// Recovery slice exponents mapped to their data length.
    pub recovery: BTreeMap<u32, u64>,
    /// Recovery slice bodies (without the exponent), filled only when
    /// [`Packets::keep_recovery_data`] is set.
    pub recovery_data: BTreeMap<u32, Vec<u8>>,
}

/// Packets from any number of PAR2 files, grouped by recovery set.
#[derive(Clone, Debug, Default)]
pub struct Packets {
    /// Recovery sets by ID.
    pub sets: HashMap<SetId, SetPackets>,
    /// When true, store recovery slice bodies for repair. Verify leaves this off
    /// so a directory full of volumes does not balloon memory.
    pub keep_recovery_data: bool,
    /// Valid packets accepted (including duplicates).
    pub accepted: u64,
    /// Corrupt, truncated or malformed packets skipped.
    pub rejected: u64,
    /// Valid packets that disagreed with an earlier packet for the same item.
    pub conflicts: u64,
    /// Valid packets of types this verifier doesn't use (creator, extensions).
    pub unknown: u64,
}

pub(crate) fn md5_of(parts: &[&[u8]]) -> Md5Sum {
    let mut h = Md5::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}
fn id_at(b: &[u8], at: usize) -> [u8; 16] {
    b[at..at + 16].try_into().expect("16 bytes")
}

impl Packets {
    /// Scan one PAR2 stream and merge its packets. Truncation and corruption are
    /// skipped; only cancellation, budgets and I/O errors return `Err`.
    pub fn scan<R: Read + Seek>(
        &mut self,
        mut input: R,
        limits: &Limits,
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        let mut pos = 0u64;
        let mut header = [0u8; HEADER as usize];
        loop {
            check(cancel)?;
            if pos >= limits.max_scan_bytes {
                return Ok(());
            }
            input.seek(SeekFrom::Start(pos))?;
            if !read_full(&mut input, &mut header)? {
                return Ok(());
            }
            match self.packet(&mut input, &header, limits, cancel)? {
                Some(length) => pos += length,
                None => {
                    if header.starts_with(MAGIC) {
                        self.rejected += 1;
                    }
                    match find_magic(&mut input, pos + 1, limits.max_scan_bytes, cancel)? {
                        Some(next) => pos = next,
                        None => return Ok(()),
                    }
                }
            }
        }
    }

    /// Validate and consume one packet whose header is already read. Returns the
    /// packet length on success, `None` for anything malformed.
    fn packet<R: Read>(
        &mut self,
        input: &mut R,
        header: &[u8; HEADER as usize],
        limits: &Limits,
        cancel: &AtomicBool,
    ) -> Result<Option<u64>, Error> {
        if !header.starts_with(MAGIC) {
            return Ok(None);
        }
        let length = u64_at(header, 8);
        if length < HEADER || !length.is_multiple_of(4) || length > limits.max_packet_bytes {
            return Ok(None);
        }
        let expected: Md5Sum = id_at(header, 16);
        let set: SetId = id_at(header, 32);
        let kind: [u8; 16] = id_at(header, 48);
        let body_len = length - HEADER;
        let metadata = [MAIN, FILE_DESC, IFSC].contains(&&kind);
        let mut hasher = Md5::new();
        hasher.update(&header[32..64]);
        let mut body = Vec::new();
        let mut exponent = None;
        if metadata {
            if body_len > limits.max_metadata_packet_bytes {
                return Ok(None);
            }
            body = vec![0; usize::try_from(body_len).map_err(|_| Error::Limit("packet size"))?];
            if !read_full(input, &mut body)? {
                return Ok(None);
            }
            hasher.update(&body);
        } else {
            let collect = self.keep_recovery_data && kind == *RECOVERY;
            if collect && body_len.saturating_sub(4) > limits.max_repair_bytes {
                return Err(Error::Limit("repair data"));
            }
            let mut left = body_len;
            let mut buf = vec![0; CHUNK];
            let mut first = true;
            while left > 0 {
                check(cancel)?;
                let n = usize::try_from(left.min(CHUNK as u64)).expect("chunk fits");
                if !read_full(input, &mut buf[..n])? {
                    return Ok(None);
                }
                if first && kind == *RECOVERY && n >= 4 {
                    exponent = Some(u32_at(&buf, 0));
                }
                if collect {
                    body.extend_from_slice(&buf[..n]);
                }
                first = false;
                hasher.update(&buf[..n]);
                left -= n as u64;
            }
        }
        if <[u8; 16]>::from(hasher.finalize()) != expected {
            return Ok(None);
        }
        self.accepted += 1;
        if self.accepted > limits.max_packets {
            return Err(Error::Limit("packets"));
        }
        if !self.sets.contains_key(&set) {
            if !(metadata || kind == *RECOVERY) {
                self.unknown += 1;
                return Ok(Some(length));
            }
            if self.sets.len() >= limits.max_sets {
                return Err(Error::Limit("recovery sets"));
            }
        }
        let ok = match &kind {
            k if k == MAIN => self.main(set, &body, limits),
            k if k == FILE_DESC => self.file_desc(set, &body, limits)?,
            k if k == IFSC => self.ifsc(set, &body, limits)?,
            k if k == RECOVERY => match exponent {
                Some(e) => {
                    let data_len = body_len - 4;
                    let entry = self.sets.entry(set).or_default();
                    match entry.recovery.entry(e) {
                        BEntry::Vacant(v) => {
                            v.insert(data_len);
                            if self.keep_recovery_data
                                && body.len() as u64 == body_len
                                && body.len() >= 4
                            {
                                let stored = entry
                                    .recovery_data
                                    .values()
                                    .map(|b| b.len() as u64)
                                    .fold(0u64, u64::saturating_add);
                                if stored.saturating_add(data_len) > limits.max_repair_bytes {
                                    return Err(Error::Limit("repair data"));
                                }
                                entry.recovery_data.insert(e, body[4..].to_vec());
                            }
                        }
                        BEntry::Occupied(o) if *o.get() != data_len => self.conflicts += 1,
                        BEntry::Occupied(_) => {}
                    }
                    true
                }
                None => false,
            },
            _ => {
                self.unknown += 1;
                true
            }
        };
        if !ok {
            // Checksummed but structurally invalid: count it as rejected.
            self.accepted -= 1;
            return Ok(None);
        }
        Ok(Some(length))
    }

    fn main(&mut self, set: SetId, body: &[u8], limits: &Limits) -> bool {
        // The recovery set ID is defined as the MD5 of the main packet body.
        if body.len() < 12 || !(body.len() - 12).is_multiple_of(16) || md5_of(&[body]) != set {
            return false;
        }
        let slice_size = u64_at(body, 0);
        let count = u32_at(body, 8) as usize;
        let total = (body.len() - 12) / 16;
        if slice_size == 0
            || !slice_size.is_multiple_of(4)
            || slice_size > limits.max_slice_bytes
            || count > total
            || total > limits.max_files
        {
            return false;
        }
        let ids: Vec<FileId> = (0..total).map(|i| id_at(body, 12 + 16 * i)).collect();
        let main = Main {
            slice_size,
            recovery_files: ids[..count].to_vec(),
            other_files: ids[count..].to_vec(),
        };
        let entry = self.sets.entry(set).or_default();
        match &entry.main {
            None => entry.main = Some(main),
            Some(existing) if *existing != main => self.conflicts += 1,
            Some(_) => {}
        }
        true
    }

    fn file_desc(&mut self, set: SetId, body: &[u8], limits: &Limits) -> Result<bool, Error> {
        if body.len() < 56 {
            return Ok(false);
        }
        let id = id_at(body, 0);
        let mut name = &body[56..];
        while let [rest @ .., 0] = name {
            name = rest;
        }
        if name.is_empty() || name.len() > limits.max_name_bytes {
            return Ok(false);
        }
        let desc = FileDesc {
            md5: id_at(body, 16),
            md5_16k: id_at(body, 32),
            length: u64_at(body, 48),
            name: name.to_vec(),
        };
        // File ID = MD5(16 KiB hash, length, name); reject spoofed IDs.
        if md5_of(&[&desc.md5_16k, &desc.length.to_le_bytes(), &desc.name]) != id {
            return Ok(false);
        }
        let entry = self.sets.entry(set).or_default();
        match entry.files.get(&id) {
            None => {
                if entry.files.len() >= limits.max_files {
                    return Err(Error::Limit("files"));
                }
                entry.files.insert(id, desc);
            }
            Some(existing) if *existing != desc => self.conflicts += 1,
            Some(_) => {}
        }
        Ok(true)
    }

    fn ifsc(&mut self, set: SetId, body: &[u8], limits: &Limits) -> Result<bool, Error> {
        if body.len() < 16 || !(body.len() - 16).is_multiple_of(20) {
            return Ok(false);
        }
        let id = id_at(body, 0);
        let sums: Vec<SliceSum> = body[16..]
            .as_chunks::<20>()
            .0
            .iter()
            .map(|c| SliceSum {
                md5: id_at(c, 0),
                crc32: u32_at(c, 16),
            })
            .collect();
        let entry = self.sets.entry(set).or_default();
        match entry.slices.get(&id) {
            None => {
                if entry.slices.len() >= limits.max_files {
                    return Err(Error::Limit("files"));
                }
                entry.slices.insert(id, sums);
            }
            Some(existing) if *existing != sums => self.conflicts += 1,
            Some(_) => {}
        }
        Ok(true)
    }
}

pub(crate) fn check(cancel: &AtomicBool) -> Result<(), Error> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}
/// Fill `buf` completely; `false` on a clean or partial EOF.
pub(crate) fn read_full<R: Read>(input: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    let mut read = 0;
    while read < buf.len() {
        match input.read(&mut buf[read..]) {
            Ok(0) => return Ok(false),
            Ok(n) => read += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}
/// Byte-wise forward search for the packet magic starting at `from`, stopping
/// at `limit` so a huge junk file cannot be scanned end-to-end.
fn find_magic<R: Read + Seek>(
    input: &mut R,
    from: u64,
    limit: u64,
    cancel: &AtomicBool,
) -> Result<Option<u64>, Error> {
    if from >= limit {
        return Ok(None);
    }
    input.seek(SeekFrom::Start(from))?;
    let mut window: Vec<u8> = Vec::with_capacity(CHUNK + MAGIC.len());
    let mut base = from;
    let mut buf = vec![0; CHUNK];
    loop {
        check(cancel)?;
        let examined = base + window.len() as u64;
        if examined >= limit {
            return Ok(None);
        }
        let want = usize::try_from((limit - examined).min(CHUNK as u64)).expect("chunk fits");
        let n = loop {
            match input.read(&mut buf[..want]) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                other => break other?,
            }
        };
        if n == 0 {
            return Ok(None);
        }
        window.extend_from_slice(&buf[..n]);
        if let Some(i) = window.windows(MAGIC.len()).position(|w| w == MAGIC) {
            return Ok(Some(base + i as u64));
        }
        let keep = MAGIC.len() - 1;
        let drop = window.len().saturating_sub(keep);
        window.drain(..drop);
        base += drop as u64;
    }
}
