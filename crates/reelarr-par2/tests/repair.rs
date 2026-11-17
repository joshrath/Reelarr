//! Repair tests against the spec-generated fixtures in `tests/fixtures`.
use reelarr_par2::{Error, Limits, Outcome, RepairStatus, repair_dir, verify_dir};
use std::{
    fs,
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

fn run(dir: &Path) -> reelarr_par2::RepairReport {
    repair_dir(
        dir,
        &Limits::default(),
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap()
}

fn original(name: &str) -> Vec<u8> {
    fs::read(fixture("basic").join(name)).unwrap()
}

#[test]
fn intact_set_is_already_valid() {
    let dir = stage(&["basic"]);
    assert_eq!(run(dir.path()).status, RepairStatus::AlreadyValid);
}

#[test]
fn repairs_a_flipped_slice() {
    let dir = stage(&["basic"]);
    let path = dir.path().join("movie.mkv");
    let mut bytes = fs::read(&path).unwrap();
    bytes[4096] ^= 0x5a;
    fs::write(&path, &bytes).unwrap();
    let report = verify_dir(
        dir.path(),
        &Limits::default(),
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap();
    assert!(matches!(
        report.primary().unwrap().outcome,
        Outcome::Repairable {
            needed: 1,
            available: 4
        }
    ));
    let repaired = run(dir.path());
    assert_eq!(
        repaired.status,
        RepairStatus::Repaired {
            files: vec!["movie.mkv".into()]
        }
    );
    assert_eq!(fs::read(&path).unwrap(), original("movie.mkv"));
    assert_eq!(
        fs::read(dir.path().join("notes.nfo")).unwrap(),
        original("notes.nfo")
    );
}

#[test]
fn restores_a_missing_file() {
    let dir = stage(&["basic"]);
    fs::remove_file(dir.path().join("notes.nfo")).unwrap();
    let repaired = run(dir.path());
    match repaired.status {
        RepairStatus::Repaired { files } => assert_eq!(files, vec!["notes.nfo".to_string()]),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        fs::read(dir.path().join("notes.nfo")).unwrap(),
        original("notes.nfo")
    );
    assert_eq!(
        fs::read(dir.path().join("movie.mkv")).unwrap(),
        original("movie.mkv")
    );
}

#[test]
fn does_not_write_through_a_hard_link() {
    let dir = stage(&["basic"]);
    let src = dir.path().join("movie.mkv");
    let link = dir.path().join("movie.linked");
    fs::hard_link(&src, &link).unwrap();
    let original_via_link = fs::read(&link).unwrap();
    let mut bytes = fs::read(&src).unwrap();
    bytes[100] ^= 0xff;
    fs::write(&src, &bytes).unwrap();
    // The hard link shares the damaged inode until repair replaces the entry.
    assert_ne!(fs::read(&link).unwrap(), original("movie.mkv"));
    run(dir.path());
    assert_eq!(fs::read(&src).unwrap(), original("movie.mkv"));
    assert_eq!(fs::read(&link).unwrap(), {
        let mut expected = original_via_link;
        expected[100] ^= 0xff;
        expected
    });
}

#[test]
fn insufficient_without_recovery_volumes() {
    let dir = stage(&["basic"]);
    fs::remove_file(dir.path().join("basic.vol00+04.par2")).unwrap();
    let mut bytes = fs::read(dir.path().join("movie.mkv")).unwrap();
    bytes[0] ^= 1;
    fs::write(dir.path().join("movie.mkv"), bytes).unwrap();
    assert_eq!(
        run(dir.path()).status,
        RepairStatus::Insufficient {
            needed: 1,
            available: 0
        }
    );
}

#[test]
fn cancel_stops_repair() {
    let dir = stage(&["basic"]);
    let mut bytes = fs::read(dir.path().join("movie.mkv")).unwrap();
    bytes[0] ^= 1;
    fs::write(dir.path().join("movie.mkv"), &bytes).unwrap();
    let err = repair_dir(
        dir.path(),
        &Limits::default(),
        &AtomicBool::new(true),
        &mut |_| {},
    );
    assert!(matches!(err, Err(Error::Cancelled)));
    // The damaged file must be untouched.
    assert_eq!(fs::read(dir.path().join("movie.mkv")).unwrap(), bytes);
}
