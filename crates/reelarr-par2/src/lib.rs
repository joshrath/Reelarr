//! Clean-room PAR2 2.0 verification and repair for Reelarr, written from the public
//! [PAR2 2.0 specification](https://parchive.github.io/doc/Parity%20Volume%20Set%20Specification%20v2.0.html).
//! No code from par2cmdline, libpar2, rust-par2 or SABnzbd was used.
//!
//! This crate verifies a flat directory against `.par2` packets (all
//! treated as hostile), checks every protected file in one flat directory by
//! its full MD5 and per-slice MD5/CRC32, finds renamed or obfuscated files by
//! their 16 KiB hash and size, and says whether the set is valid, repairable
//! with the recovery blocks present, or not. [`repair_dir`] reconstructs
//! damaged or missing protected files from recovery slices.
//!
//! Slices are checked only at their aligned offsets. Data shifted by inserted
//! or deleted bytes is reported as damaged rather than searched for.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod gf;
mod packet;
mod repair;
mod verify;

pub use packet::{FileDesc, Main, Packets, SetPackets, SliceSum};
pub use repair::{RepairReport, RepairStatus, repair_dir};
pub use verify::{FileReport, FileStatus, Outcome, Progress, Report, SetReport, verify_dir};

/// A recovery set ID (MD5 of the main packet body).
pub type SetId = [u8; 16];
/// A file ID (MD5 of the 16 KiB hash, length and name).
pub type FileId = [u8; 16];
/// A raw MD5 digest.
pub type Md5Sum = [u8; 16];

/// Budgets that bound work and memory on hostile input.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Largest packet accepted, header included.
    pub max_packet_bytes: u64,
    /// Largest main, file description or IFSC packet body kept in memory.
    pub max_metadata_packet_bytes: u64,
    /// Valid packets accepted across all PAR2 files.
    pub max_packets: u64,
    /// Distinct recovery sets.
    pub max_sets: usize,
    /// Files per recovery set.
    pub max_files: usize,
    /// Longest file name in a description packet.
    pub max_name_bytes: usize,
    /// Largest slice size a main packet may declare.
    pub max_slice_bytes: u64,
    /// Directory entries examined.
    pub max_dir_entries: usize,
    /// Bytes examined while scanning one PAR2 file, including packet bodies and
    /// resync searches. The rest of that file is skipped; packets already
    /// accepted are kept. Caps work on a multi-gigabyte junk `.par2`.
    pub max_scan_bytes: u64,
    /// Total recovery-slice bytes plus reconstructed slice bytes kept in memory
    /// during repair.
    pub max_repair_bytes: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_packet_bytes: 1 << 30,
            max_metadata_packet_bytes: 16 << 20,
            max_packets: 1_000_000,
            max_sets: 16,
            max_files: 10_000,
            max_name_bytes: 4096,
            max_slice_bytes: 256 << 20,
            max_dir_entries: 100_000,
            max_scan_bytes: 8 << 30,
            max_repair_bytes: 2 << 30,
        }
    }
}

/// Why verification stopped. Corrupt PAR2 data is never an error; it is
/// skipped and counted in the report.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The cancel flag was set.
    #[error("cancelled")]
    Cancelled,
    /// A budget in [`Limits`] was exceeded.
    #[error("PAR2 limit exceeded: {0}")]
    Limit(&'static str),
    /// Reading the directory or a file failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
