//! Verification tests against the spec-generated fixtures in `tests/fixtures`
//! (see PROVENANCE.md). Damage is applied to temp copies at test time.
use reelarr_par2::{Error, FileStatus, Limits, Outcome, Packets, Report, verify_dir};
use std::{
    fs,
    io::Cursor,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

fn fixture(set: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(set)
}

fn stage(sets: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for set in sets {
        for entry in fs::read_dir(fixture(set)).unwrap() {
            let entry = entry.unwrap();
            fs::copy(entry.path(), dir.path().join(entry.file_name())).unwrap();
        }
    }
    dir
}

fn run(dir: &Path) -> Report {
    verify_dir(
        dir,
        &Limits::default(),
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap()
}

fn flip(path: &Path, offset: usize) {
    let mut bytes = fs::read(path).unwrap();
    bytes[offset] ^= 0x5a;
    fs::write(path, bytes).unwrap();
}

fn file<'a>(report: &'a Report, name: &str) -> &'a reelarr_par2::FileReport {
    report
        .primary()
        .unwrap()
        .files
        .iter()
        .find(|f| f.name == name)
        .unwrap()
}

#[test]
fn intact_set_is_valid() {
    let dir = stage(&["basic"]);
    let report = run(dir.path());
    let set = report.primary().unwrap();
    assert_eq!(report.sets.len(), 1);
    assert_eq!(set.outcome, Outcome::Valid);
    assert_eq!(set.slice_size, 4096);
    assert_eq!(set.recovery_blocks, 4);
    assert_eq!(set.files.len(), 2);
    assert!(
        set.files
            .iter()
            .all(|f| f.status == FileStatus::Intact && !f.renamed())
    );
    assert_eq!(file(&report, "movie.mkv").slices, 25);
    assert_eq!(report.par2_files.len(), 2);
    assert_eq!(report.rejected_packets, 0);
}

#[test]
fn damaged_slices_are_located() {
    let dir = stage(&["basic"]);
    flip(&dir.path().join("movie.mkv"), 5000);
    flip(&dir.path().join("movie.mkv"), 99_999);
    let report = run(dir.path());
    assert_eq!(
        file(&report, "movie.mkv").status,
        FileStatus::Damaged {
            bad_slices: vec![1, 24]
        }
    );
    assert_eq!(file(&report, "notes.nfo").status, FileStatus::Intact);
    assert_eq!(
        report.primary().unwrap().outcome,
        Outcome::Repairable {
            needed: 2,
            available: 4
        }
    );
}

#[test]
fn truncated_file_needs_its_tail() {
    let dir = stage(&["basic"]);
    let path = dir.path().join("movie.mkv");
    let bytes = fs::read(&path).unwrap();
    fs::write(&path, &bytes[..50_000]).unwrap();
    let report = run(dir.path());
    assert_eq!(
        file(&report, "movie.mkv").status,
        FileStatus::Damaged {
            bad_slices: (12..25).collect()
        }
    );
    assert_eq!(
        report.primary().unwrap().outcome,
        Outcome::Insufficient {
            needed: 13,
            available: 4
        }
    );
}

#[test]
fn trailing_junk_needs_no_blocks() {
    let dir = stage(&["basic"]);
    let path = dir.path().join("notes.nfo");
    let mut bytes = fs::read(&path).unwrap();
    bytes.extend_from_slice(b"junk");
    fs::write(&path, bytes).unwrap();
    let report = run(dir.path());
    assert_eq!(
        file(&report, "notes.nfo").status,
        FileStatus::Damaged { bad_slices: vec![] }
    );
    assert_eq!(
        report.primary().unwrap().outcome,
        Outcome::Repairable {
            needed: 0,
            available: 4
        }
    );
}

#[test]
fn missing_file_needs_every_slice() {
    let dir = stage(&["basic"]);
    fs::remove_file(dir.path().join("notes.nfo")).unwrap();
    let report = run(dir.path());
    let nfo = file(&report, "notes.nfo");
    assert_eq!(nfo.status, FileStatus::Missing);
    assert_eq!(nfo.found_as, None);
    assert_eq!(
        report.primary().unwrap().outcome,
        Outcome::Repairable {
            needed: 1,
            available: 4
        }
    );
}

#[test]
fn renamed_files_are_found_by_16k_hash_and_size() {
    let dir = stage(&["basic"]);
    fs::rename(dir.path().join("movie.mkv"), dir.path().join("a8f3k2q")).unwrap();
    // Obfuscated PAR2 names are recognised by their packet magic.
    fs::rename(
        dir.path().join("basic.vol00+04.par2"),
        dir.path().join("zz91x"),
    )
    .unwrap();
    let report = run(dir.path());
    let movie = file(&report, "movie.mkv");
    assert_eq!(movie.status, FileStatus::Intact);
    assert_eq!(movie.found_as.as_deref(), Some("a8f3k2q"));
    assert!(movie.renamed());
    assert_eq!(report.primary().unwrap().recovery_blocks, 4);
    assert_eq!(report.primary().unwrap().outcome, Outcome::Valid);
    // Nothing is renamed on disk.
    assert!(dir.path().join("a8f3k2q").exists());
    assert!(!dir.path().join("movie.mkv").exists());
}

#[test]
fn renamed_and_damaged_after_16k() {
    let dir = stage(&["basic"]);
    fs::rename(dir.path().join("movie.mkv"), dir.path().join("x.bin")).unwrap();
    flip(&dir.path().join("x.bin"), 20_000);
    let report = run(dir.path());
    let movie = file(&report, "movie.mkv");
    assert_eq!(movie.found_as.as_deref(), Some("x.bin"));
    assert_eq!(
        movie.status,
        FileStatus::Damaged {
            bad_slices: vec![4]
        }
    );
}

#[test]
fn without_volumes_damage_is_insufficient() {
    let dir = stage(&["basic"]);
    fs::remove_file(dir.path().join("basic.vol00+04.par2")).unwrap();
    flip(&dir.path().join("movie.mkv"), 0);
    let report = run(dir.path());
    assert_eq!(
        report.primary().unwrap().outcome,
        Outcome::Insufficient {
            needed: 1,
            available: 0
        }
    );
}

#[test]
fn corrupt_packets_are_skipped_and_copies_used() {
    let dir = stage(&["basic"]);
    let index = dir.path().join("basic.par2");
    // Byte 200 sits inside the first file description packet.
    flip(&index, 200);
    let report = run(dir.path());
    assert!(report.rejected_packets >= 1);
    assert_eq!(report.primary().unwrap().outcome, Outcome::Valid);

    // With the only copy corrupt, that file can't be verified.
    fs::remove_file(dir.path().join("basic.vol00+04.par2")).unwrap();
    let report = run(dir.path());
    assert_eq!(report.primary().unwrap().outcome, Outcome::Incomplete);
    assert!(
        report
            .primary()
            .unwrap()
            .files
            .iter()
            .any(|f| f.status == FileStatus::Unverifiable)
    );
}

#[test]
fn duplicated_and_truncated_par2_files() {
    let dir = stage(&["basic"]);
    let index = fs::read(dir.path().join("basic.par2")).unwrap();
    fs::write(
        dir.path().join("dup.par2"),
        [&index[..], &index[..]].concat(),
    )
    .unwrap();
    let vol = fs::read(dir.path().join("basic.vol00+04.par2")).unwrap();
    fs::write(
        dir.path().join("basic.vol00+04.par2"),
        &vol[..vol.len() - 3000],
    )
    .unwrap();
    let report = run(dir.path());
    let set = report.primary().unwrap();
    assert_eq!(set.outcome, Outcome::Valid);
    assert_eq!(report.conflicting_packets, 0);
    // The truncated last recovery packet is dropped; the others still count.
    assert_eq!(set.recovery_blocks, 3);
    assert_eq!(report.rejected_packets, 1);
}

#[test]
fn mixed_sets_report_each_and_pick_the_primary() {
    let dir = stage(&["basic", "other"]);
    let report = run(dir.path());
    assert_eq!(report.sets.len(), 2);
    assert!(report.sets.iter().all(|s| s.outcome == Outcome::Valid));
    assert_eq!(report.primary().unwrap().files.len(), 2);

    // A stray set whose files are absent doesn't win over the real one.
    fs::remove_file(dir.path().join("other.bin")).unwrap();
    let report = run(dir.path());
    assert_eq!(report.primary().unwrap().files.len(), 2);
    assert_eq!(report.primary().unwrap().outcome, Outcome::Valid);
}

#[cfg(unix)]
#[test]
fn symlinks_are_never_followed() {
    let dir = stage(&["basic"]);
    let outside = tempfile::tempdir().unwrap();
    let real = outside.path().join("movie.mkv");
    fs::rename(dir.path().join("movie.mkv"), &real).unwrap();
    std::os::unix::fs::symlink(&real, dir.path().join("movie.mkv")).unwrap();
    let report = run(dir.path());
    assert_eq!(file(&report, "movie.mkv").status, FileStatus::Missing);
}

#[test]
fn cancel_and_progress() {
    let dir = stage(&["basic"]);
    let err = verify_dir(
        dir.path(),
        &Limits::default(),
        &AtomicBool::new(true),
        &mut |_| {},
    );
    assert!(matches!(err, Err(Error::Cancelled)));

    let mut last = Default::default();
    verify_dir(
        dir.path(),
        &Limits::default(),
        &AtomicBool::new(false),
        &mut |p| last = p,
    )
    .unwrap();
    let last: reelarr_par2::Progress = last;
    assert_eq!(last.bytes_done, 101_234);
    assert_eq!(last.bytes_done, last.bytes_total);
}

#[test]
fn budgets_are_enforced() {
    let dir = stage(&["basic"]);
    let limits = Limits {
        max_packets: 3,
        ..Limits::default()
    };
    let err = verify_dir(dir.path(), &limits, &AtomicBool::new(false), &mut |_| {});
    assert!(matches!(err, Err(Error::Limit("packets"))));
}

// ---- Hand-built hostile packets ----

fn md5(parts: &[&[u8]]) -> [u8; 16] {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

fn packet(set: [u8; 16], kind: &[u8; 16], body: &[u8]) -> Vec<u8> {
    let mut out = b"PAR2\0PKT".to_vec();
    out.extend_from_slice(&(64 + body.len() as u64).to_le_bytes());
    out.extend_from_slice(&md5(&[&set, kind, body]));
    out.extend_from_slice(&set);
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

/// One-file set whose description uses `name`, protecting `content`.
fn hostile_set(name: &[u8], content: &[u8]) -> Vec<u8> {
    let h16 = md5(&[&content[..content.len().min(16384)]]);
    let id = md5(&[&h16, &(content.len() as u64).to_le_bytes(), name]);
    let mut main = 1024u64.to_le_bytes().to_vec();
    main.extend_from_slice(&1u32.to_le_bytes());
    main.extend_from_slice(&id);
    let set = md5(&[&main]);
    let mut desc = id.to_vec();
    desc.extend_from_slice(&md5(&[content]));
    desc.extend_from_slice(&h16);
    desc.extend_from_slice(&(content.len() as u64).to_le_bytes());
    desc.extend_from_slice(name);
    while !desc.len().is_multiple_of(4) {
        desc.push(0);
    }
    let mut ifsc = id.to_vec();
    for chunk in content.chunks(1024) {
        let mut s = chunk.to_vec();
        s.resize(1024, 0);
        ifsc.extend_from_slice(&md5(&[&s]));
        ifsc.extend_from_slice(&crc32fast::hash(&s).to_le_bytes());
    }
    [
        packet(set, b"PAR 2.0\0Main\0\0\0\0", &main),
        packet(set, b"PAR 2.0\0FileDesc", &desc),
        packet(set, b"PAR 2.0\0IFSC\0\0\0\0", &ifsc),
    ]
    .concat()
}

#[test]
fn hostile_names_never_reach_outside_the_directory() {
    let outer = tempfile::tempdir().unwrap();
    let dir = outer.path().join("job");
    fs::create_dir(&dir).unwrap();
    fs::write(outer.path().join("secret"), b"top secret").unwrap();
    for (i, name) in [
        &b"../secret"[..],
        b"/etc/passwd",
        b"..",
        b"a\0b",
        b"\xff\xfe",
    ]
    .iter()
    .enumerate()
    {
        fs::write(
            dir.join(format!("{i}.par2")),
            hostile_set(name, b"top secret"),
        )
        .unwrap();
    }
    let report = run(&dir);
    assert_eq!(report.sets.len(), 5);
    for set in &report.sets {
        assert_eq!(
            set.files[0].status,
            FileStatus::Missing,
            "{:?}",
            set.files[0].name
        );
        assert_eq!(set.files[0].found_as, None);
    }
    // Only a file inside the folder can match: by base name or by content.
    fs::write(dir.join("secret"), b"top secret").unwrap();
    let report = run(&dir);
    for set in &report.sets {
        assert_eq!(set.files[0].status, FileStatus::Intact);
        assert_eq!(set.files[0].found_as.as_deref(), Some("secret"));
    }
}

#[test]
fn spoofed_ids_and_bad_main_packets_are_rejected() {
    // Main packet whose set ID isn't the MD5 of its body.
    let mut main = 1024u64.to_le_bytes().to_vec();
    main.extend_from_slice(&0u32.to_le_bytes());
    let forged = packet([7; 16], b"PAR 2.0\0Main\0\0\0\0", &main);
    // Slice size zero, and one not a multiple of 4.
    let zero = {
        let body = [0u64.to_le_bytes().to_vec(), 0u32.to_le_bytes().to_vec()].concat();
        packet(md5(&[&body]), b"PAR 2.0\0Main\0\0\0\0", &body)
    };
    // File description whose ID doesn't match its contents.
    let mut desc = [1u8; 16].to_vec();
    desc.extend_from_slice(&[0; 40]);
    desc.extend_from_slice(b"name");
    let spoof = packet([2; 16], b"PAR 2.0\0FileDesc", &desc);
    let mut packets = Packets::default();
    packets
        .scan(
            Cursor::new([forged, zero, spoof].concat()),
            &Limits::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(packets.rejected, 3);
    assert!(packets.sets.is_empty());
}

#[test]
fn mutated_packets_never_panic() {
    // Deterministic mutation pass over real packet streams; the cargo-fuzz
    // target in fuzz/ explores the same entry point without bounds.
    let seeds = [
        fs::read(fixture("basic").join("basic.vol00+04.par2")).unwrap(),
        hostile_set(b"x", &[3; 3000]),
    ];
    let mut x = 0x9e37_79b9u32;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        x
    };
    for round in 0..3000 {
        let mut data = seeds[round % 2].clone();
        for _ in 0..1 + next() % 8 {
            let at = next() as usize % data.len();
            match next() % 4 {
                0 => data[at] = next() as u8,
                1 => data.truncate(at.max(1)),
                2 => data[at..].rotate_left(1),
                _ => {
                    let len_at = at - at % 4;
                    if len_at + 8 <= data.len() {
                        data[len_at..len_at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
                    }
                }
            }
        }
        let mut packets = Packets::default();
        let _ = packets.scan(
            Cursor::new(&data),
            &Limits::default(),
            &AtomicBool::new(false),
        );
    }
}
