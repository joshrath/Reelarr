//! Repair memory stays bounded by the missing-slice count, not by how many
//! slices or recovery bytes the set has.
//!
//! The set is built here from the PAR2 2.0 specification (packet layout,
//! MD5/CRC32, GF(2^16) recovery with generator 0x1100B), independently of the
//! library's own encoder. A counting global allocator measures the peak heap
//! growth while `repair_dir` runs. This file holds a single test so nothing
//! else allocates concurrently.
use md5::{Digest, Md5};
use reelarr_par2::{Error, Limits, RepairStatus, repair_dir, repair_memory_bytes};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    fs,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

struct Counting;
static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

const SLICE: usize = 256 * 1024;
const SLICES: usize = 64;
const MISSING: [usize; 2] = [3, 41];
const CHUNK: u64 = 64 * 1024;

struct Gf {
    exp: Vec<u16>,
    log: Vec<u16>,
}
impl Gf {
    fn new() -> Self {
        let mut exp = vec![0u16; 65535];
        let mut log = vec![0u16; 65536];
        let mut x: u32 = 1;
        for (i, e) in exp.iter_mut().enumerate() {
            *e = x as u16;
            log[x as usize] = i as u16;
            x <<= 1;
            if x & 0x10000 != 0 {
                x ^= 0x1100B;
            }
        }
        Self { exp, log }
    }
    fn mul(&self, a: u16, b: u16) -> u16 {
        if a == 0 || b == 0 {
            return 0;
        }
        self.exp[(self.log[a as usize] as usize + self.log[b as usize] as usize) % 65535]
    }
    fn pow(&self, a: u16, e: u32) -> u16 {
        if a == 0 {
            return if e == 0 { 1 } else { 0 };
        }
        self.exp[(self.log[a as usize] as u64 * u64::from(e % 65535) % 65535) as usize]
    }
}

fn md5(parts: &[&[u8]]) -> [u8; 16] {
    let mut h = Md5::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

fn packet(set: &[u8; 16], kind: &[u8; 16], body: &[u8]) -> Vec<u8> {
    assert_eq!(body.len() % 4, 0);
    let mut out = b"PAR2\0PKT".to_vec();
    out.extend_from_slice(&(64 + body.len() as u64).to_le_bytes());
    out.extend_from_slice(&md5(&[set, kind, body]));
    out.extend_from_slice(set);
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

fn content() -> Vec<u8> {
    let mut x: u32 = 12345;
    let mut out = Vec::with_capacity(SLICE * SLICES);
    while out.len() < SLICE * SLICES {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// Write `big.bin`, `big.par2` and `big.vol00+02.par2` into `dir`.
fn build(dir: &std::path::Path, data: &[u8]) {
    let gf = Gf::new();
    let name = b"big.bin";
    let h16 = md5(&[&data[..16384]]);
    let len = (data.len() as u64).to_le_bytes();
    let fid = md5(&[&h16, &len, name]);
    let mut main = (SLICE as u64).to_le_bytes().to_vec();
    main.extend_from_slice(&1u32.to_le_bytes());
    main.extend_from_slice(&fid);
    let set = md5(&[&main]);
    let mut index = packet(&set, b"PAR 2.0\0Main\0\0\0\0", &main);
    let mut desc = fid.to_vec();
    desc.extend_from_slice(&md5(&[data]));
    desc.extend_from_slice(&h16);
    desc.extend_from_slice(&len);
    desc.extend_from_slice(b"big.bin\0");
    index.extend(packet(&set, b"PAR 2.0\0FileDesc", &desc));
    let mut ifsc = fid.to_vec();
    for s in data.chunks(SLICE) {
        ifsc.extend_from_slice(&md5(&[s]));
        ifsc.extend_from_slice(&crc32fast::hash(s).to_le_bytes());
    }
    index.extend(packet(&set, b"PAR 2.0\0IFSC\0\0\0\0", &ifsc));

    // Input-slice constants: 2^n for n coprime to 65535's factors.
    let constants: Vec<u16> = (0u32..65535)
        .filter(|n| n % 3 != 0 && n % 5 != 0 && n % 17 != 0 && n % 257 != 0)
        .take(SLICES)
        .map(|n| gf.pow(2, n))
        .collect();
    let mut vol = index.clone();
    for e in 0..2u32 {
        let mut rec = vec![0u16; SLICE / 2];
        for (i, s) in data.chunks(SLICE).enumerate() {
            let f = gf.pow(constants[i], e);
            for (r, w) in rec.iter_mut().zip(s.as_chunks::<2>().0) {
                *r ^= gf.mul(u16::from_le_bytes(*w), f);
            }
        }
        let mut body = e.to_le_bytes().to_vec();
        for w in rec {
            body.extend_from_slice(&w.to_le_bytes());
        }
        vol.extend(packet(&set, b"PAR 2.0\0RecvSlic", &body));
    }
    fs::write(dir.join("big.par2"), &index).unwrap();
    fs::write(dir.join("big.vol00+02.par2"), &vol).unwrap();
    let mut damaged = data.to_vec();
    for &m in &MISSING {
        damaged[m * SLICE..(m + 1) * SLICE].fill(0xEE);
    }
    fs::write(dir.join("big.bin"), &damaged).unwrap();
}

fn repair(dir: &std::path::Path, limits: &Limits) -> Result<RepairStatus, Error> {
    repair_dir(dir, limits, &AtomicBool::new(false), &mut |_| {}).map(|r| r.status)
}

#[test]
fn repair_memory_is_bounded_by_missing_slices() {
    let data = content();
    let needed = MISSING.len() as u64;
    let budget = repair_memory_bytes(needed, CHUNK);

    // One byte under the budget is refused before any work.
    let dir = tempfile::tempdir().unwrap();
    build(dir.path(), &data);
    let tight = Limits {
        max_repair_bytes: budget - 1,
        ..Limits::default()
    };
    assert!(matches!(
        repair(dir.path(), &tight),
        Err(Error::Limit("repair memory"))
    ));

    // The exact budget is enough, and peak heap growth stays near it even
    // though the set holds 16 MiB of data and 512 KiB of recovery.
    let limits = Limits {
        max_repair_bytes: budget,
        ..Limits::default()
    };
    let base = CURRENT.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let status = repair(dir.path(), &limits).unwrap();
    let growth = PEAK.load(Ordering::Relaxed).saturating_sub(base);
    assert_eq!(
        status,
        RepairStatus::Repaired {
            files: vec!["big.bin".into()]
        }
    );
    assert!(fs::read(dir.path().join("big.bin")).unwrap() == data);
    eprintln!("repair heap growth {growth} bytes, budget {budget}");
    // Verification itself may hold one slice; repair adds ~(needed+2) chunks.
    let ceiling = budget + SLICE as u64 + 512 * 1024;
    assert!(
        (growth as u64) < ceiling,
        "repair heap growth {growth} exceeded {ceiling} (budget {budget})"
    );
}
