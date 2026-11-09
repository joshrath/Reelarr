//! Fuzz the PAR2 packet scanner: `cargo +nightly fuzz run packets`.
//! Seed corpus: `../tests/fixtures/*/*.par2`.
#![no_main]
use libfuzzer_sys::fuzz_target;
use reelarr_par2::{Limits, Packets};
use std::{io::Cursor, sync::atomic::AtomicBool};

fuzz_target!(|data: &[u8]| {
    let limits = Limits {
        max_packet_bytes: 1 << 20,
        max_metadata_packet_bytes: 1 << 20,
        ..Limits::default()
    };
    let mut packets = Packets::default();
    let _ = packets.scan(Cursor::new(data), &limits, &AtomicBool::new(false));
});
