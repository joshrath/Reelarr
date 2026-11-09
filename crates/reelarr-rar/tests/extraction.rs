mod common;
use common::*;
use reelarr_rar::{Error, Extraction, Limits, VolumeSet};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};
use tempfile::TempDir;

fn run(
    volumes: &[Vec<u8>],
    v5: bool,
    new_names: bool,
    limits: Limits,
) -> reelarr_rar::Result<Extraction> {
    let inputs = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let paths = write_volumes(inputs.path(), volumes, new_names);
    let result =
        VolumeSet::open(&paths, limits)?.extract(output.path(), &AtomicBool::new(false), |_| {});
    if result.is_err() {
        assert_eq!(
            fs::read_dir(output.path()).unwrap().count(),
            0,
            "failed extraction leaked staging"
        );
    }
    if let Ok(ref e) = result {
        assert_eq!(e.manifest().iter().filter(|e| !e.directory).count(), 1);
    }
    let _ = v5;
    result
}
fn write_volumes(parent: &Path, volumes: &[Vec<u8>], new: bool) -> Vec<PathBuf> {
    volumes
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let name = if new {
                format!("a.part{:02}.rar", i + 1)
            } else if i == 0 {
                "a.rar".into()
            } else {
                format!("a.r{:02}", i - 1)
            };
            let p = parent.join(name);
            fs::write(&p, b).unwrap();
            p
        })
        .collect()
}
// Keep the output parent alive while checking successful extraction.
fn extract(bytes: &[u8], limits: Limits) -> (TempDir, reelarr_rar::Result<Extraction>) {
    let parent = tempfile::tempdir().unwrap();
    let p = parent.path().join("a.rar");
    fs::write(&p, bytes).unwrap();
    let out = parent.path().join("out");
    fs::create_dir(&out).unwrap();
    let e = VolumeSet::open(&[p], limits)
        .and_then(|v| v.extract(&out, &AtomicBool::new(false), |_| {}));
    if e.is_err() {
        assert_eq!(fs::read_dir(&out).unwrap().count(), 0);
    }
    (parent, e)
}
#[test]
fn stored_single_empty_and_chunk_boundaries() {
    for v5 in [false, true] {
        for n in [0, 1, 63, 64, 65, 511, 512, 513, 65535, 65536, 65537, 131073] {
            let data: Vec<_> = (0..n).map(|n| (n % 251) as u8).collect();
            let mut e = Spec::new("folder/movie.mkv", &data).hash();
            if !v5 {
                e.high = true;
            }
            let (_parent, result) = extract(&archive(v5, 0, 1, false, &[e]), Limits::default());
            let result = result.unwrap();
            assert_eq!(
                fs::read(result.path().join("folder/movie.mkv")).unwrap(),
                data
            );
        }
    }
}
#[test]
fn stored_split_new_and_old_names() {
    for v5 in [false, true] {
        for new in [false, true] {
            for cut in [0, 1, 64, 513, 65536] {
                let data = vec![42; 70000];
                let mut a = Spec::new("movie", &data[..cut]).hash();
                a.size = data.len() as u64;
                a.after = true;
                let mut b = Spec::new("movie", &data[cut..]).hash();
                b.size = data.len() as u64;
                b.before = true;
                b.crc = Some(crc32fast::hash(&data));
                b.hash = Some(
                    blake2s_simd::blake2sp::blake2sp(&data)
                        .as_bytes()
                        .try_into()
                        .unwrap(),
                );
                let inputs = tempfile::tempdir().unwrap();
                let output = tempfile::tempdir().unwrap();
                let paths = write_volumes(
                    inputs.path(),
                    &[archive(v5, 0, 2, new, &[a]), archive(v5, 1, 2, new, &[b])],
                    new,
                );
                let result = VolumeSet::open(&paths, Limits::default())
                    .unwrap()
                    .extract(output.path(), &AtomicBool::new(false), |_| {})
                    .unwrap();
                assert_eq!(fs::read(result.path().join("movie")).unwrap(), data);
            }
        }
    }
}
#[test]
fn real_tool_fixtures_independent_checksums() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for (stem, new) in [("real4", true), ("real5", true), ("old4", false)] {
        let paths: Vec<_> = (0..3)
            .map(|i| {
                fixtures.join(if new {
                    format!("{stem}.part{}.rar", i + 1)
                } else if i == 0 {
                    format!("{stem}.rar")
                } else {
                    format!("{stem}.r{:02}", i - 1)
                })
            })
            .collect();
        let output = tempfile::tempdir().unwrap();
        let extracted = VolumeSet::open(&paths, Limits::default())
            .unwrap()
            .extract(output.path(), &AtomicBool::new(false), |_| {})
            .unwrap();
        let expected: Vec<_> = (0..10240).map(|n| (n % 256) as u8).collect();
        assert_eq!(
            fs::read(extracted.path().join("payload.txt")).unwrap(),
            expected
        );
    }
}
#[test]
fn independent_blake2sp_known_answers_and_chunking() {
    for line in include_str!("fixtures/blake2sp-kat.txt").lines() {
        let (n, expected) = line.split_once(' ').unwrap();
        let data: Vec<_> = (0..n.parse::<usize>().unwrap()).map(|n| n as u8).collect();
        for chunk in [1, 7, 63, 64, 65, 127, 128, 129, 511, 512, 513, 65536] {
            let mut state = blake2s_simd::blake2sp::State::new();
            for b in data.chunks(chunk) {
                state.update(b);
            }
            assert_eq!(state.finalize().to_hex().as_str(), expected);
        }
    }
    assert_eq!(crc32fast::hash(b"123456789"), 0xcbf43926);
}
#[test]
fn crc_hash_header_and_missing_integrity_errors() {
    for v5 in [false, true] {
        let mut e = Spec::new("x", b"hello").hash();
        e.crc = Some(1);
        assert!(matches!(
            extract(&archive(v5, 0, 1, false, &[e]), Limits::default()).1,
            Err(Error::Checksum(_))
        ));
        let e = Spec::new("x", b"hello").hash();
        let mut bytes = archive(v5, 0, 1, false, &[e]);
        let offset = if v5 { 12 } else { 10 };
        bytes[offset] ^= 1;
        assert!(matches!(
            extract(&bytes, Limits::default()).1,
            Err(Error::Checksum(_))
        ));
    }
    let mut e = Spec::new("x", b"hello").hash();
    e.hash = Some([0; 32]);
    assert!(matches!(
        extract(&archive(true, 0, 1, false, &[e]), Limits::default()).1,
        Err(Error::Checksum("file/part BLAKE2sp"))
    ));
    let mut e = Spec::new("x", b"hello");
    e.crc = None;
    assert!(matches!(
        extract(&archive(true, 0, 1, false, &[e]), Limits::default()).1,
        Err(Error::MissingIntegrityChecksum)
    ));
    let mut e = Spec::new("x", b"hello").hash();
    e.crc = None;
    assert!(
        extract(&archive(true, 0, 1, false, &[e]), Limits::default())
            .1
            .is_ok()
    );
}
#[test]
fn all_unsafe_names_rejected_and_nothing_published() {
    for v5 in [false, true] {
        for name in [
            "",
            "/abs",
            "//server/share",
            "../x",
            "a/../x",
            "a/./x",
            "./x",
            "a//b",
            "a/",
            "C:/x",
            "C:x",
            "x:y",
            "nul",
            "NUL.txt",
            "CON",
            "aux",
            "PRN",
            "com1.mkv",
            "LPT9",
            "conin$",
            "a.",
            "a ",
            "a\0b",
            "a\nb",
            "a\u{7f}",
            "a*",
            "a?",
            "a<b",
            "a>b",
            "a|b",
            "a\"b",
            "a\u{202e}b",
            "a\u{2066}b",
        ] {
            assert!(
                extract(
                    &archive(v5, 0, 1, false, &[Spec::new(name, b"x")]),
                    Limits::default()
                )
                .1
                .is_err(),
                "{v5} {name:?}"
            );
        }
    }
    for name in [
        "../escape",
        "a\\..\\escape",
        "\\root",
        "\\\\server\\x",
        "C:\\x",
    ] {
        assert!(
            extract(
                &archive(false, 0, 1, false, &[Spec::new(name, b"x")]),
                Limits::default()
            )
            .1
            .is_err()
        );
    }
    for name in ["a\\b", "COM¹", "ＮＵＬ", "e\u{301}", "a\u{fffe}"] {
        assert!(
            extract(
                &archive(true, 0, 1, false, &[Spec::new(name, b"x")]),
                Limits::default()
            )
            .1
            .is_err()
        );
    }
}
#[test]
fn collisions_prefix_case_and_unicode() {
    for v5 in [false, true] {
        for (a, b) in [
            ("x", "x"),
            ("X", "x"),
            ("x", "x/y"),
            ("x/y", "x"),
            ("A/x", "a/y"),
        ] {
            assert!(matches!(
                extract(
                    &archive(v5, 0, 1, false, &[Spec::new(a, b"a"), Spec::new(b, b"b")]),
                    Limits::default()
                )
                .1,
                Err(Error::Collision)
            ));
        }
    }
    assert!(matches!(
        extract(
            &archive(
                true,
                0,
                1,
                false,
                &[Spec::new("É", b"a"), Spec::new("é", b"b")]
            ),
            Limits::default()
        )
        .1,
        Err(Error::Collision)
    ));
}
#[test]
fn directories_and_services_are_not_restored_as_files() {
    for v5 in [false, true] {
        let mut dir = Spec::new("folder", b"");
        dir.directory = true;
        dir.attr = 0o040755;
        let mut bytes = archive(
            v5,
            0,
            1,
            false,
            &[dir.clone(), Spec::new("folder/x", b"ok")],
        );
        // Insert a service stream before the end marker; never interpret its path.
        let service = if v5 {
            block5(3, 0, b"ignored metadata", &[], b"large-enough-to-skip")
        } else {
            let mut b = 4u32.to_le_bytes().to_vec();
            b.extend(b"ignored");
            let mut s = block4(0x7a, 0x8000, &b);
            s.extend(b"data");
            s
        };
        let end = if v5 {
            block5(5, 0, &[0], &[], &[])
        } else {
            block4(0x7b, 0x4000, &[])
        };
        if v5 {
            bytes.truncate(bytes.len() - end.len());
        } else {
            bytes.truncate(bytes.len() - 13);
        }
        bytes.extend(service);
        bytes.extend(end);
        let (_parent, result) = extract(&bytes, Limits::default());
        let e = result.unwrap();
        assert!(e.path().join("folder").is_dir());
        assert_eq!(e.manifest().len(), 2);
        assert_eq!(fs::read(e.path().join("folder/x")).unwrap(), b"ok");
        dir.data = vec![1];
        assert!(
            extract(&archive(v5, 0, 1, false, &[dir]), Limits::default())
                .1
                .is_err()
        );
    }
}
#[test]
fn unsupported_methods_solid_encryption_links_and_devices() {
    for v5 in [false, true] {
        let e = Spec::new("x", b"x");
        for attr in [0o120777, 0o020600, 0o060600, 0o010600, 0o140600] {
            let mut e = e.clone();
            e.attr = attr;
            assert!(matches!(
                extract(&archive(v5, 0, 1, false, &[e]), Limits::default()).1,
                Err(Error::Unsupported { .. })
            ));
        }
        let mut compressed = e.clone();
        compressed.method = if v5 { 0x80 } else { 1 };
        assert!(matches!(
            extract(&archive(v5, 0, 1, false, &[compressed]), Limits::default()).1,
            Err(Error::Unsupported { .. })
        ));
        let mut solid = e.clone();
        if v5 {
            solid.method = 0x40
        } else {
            solid.flags = 0x10;
        }
        assert!(matches!(
            extract(&archive(v5, 0, 1, false, &[solid]), Limits::default()).1,
            Err(Error::Unsupported { .. })
        ));
        let mut encrypted = e.clone();
        if v5 {
            encrypted.extra = extra(1, &[0]);
        } else {
            encrypted.flags = 4;
        }
        assert!(matches!(
            extract(&archive(v5, 0, 1, false, &[encrypted]), Limits::default()).1,
            Err(Error::Unsupported { .. })
        ));
        let mut dos = e;
        dos.host = 0;
        dos.attr = 0x40;
        assert!(matches!(
            extract(&archive(v5, 0, 1, false, &[dos]), Limits::default()).1,
            Err(Error::Unsupported { .. })
        ));
    }
    for typ in [4, 5] {
        let mut e = Spec::new("x", b"x");
        e.extra = extra(typ, &[0]);
        assert!(matches!(
            extract(&archive(true, 0, 1, false, &[e]), Limits::default()).1,
            Err(Error::Unsupported { .. })
        ));
    }
    let mut e = Spec::new("x\0encoded", b"x");
    e.flags = 0x200;
    assert!(matches!(
        extract(&archive(false, 0, 1, false, &[e]), Limits::default()).1,
        Err(Error::Unsupported { .. })
    ));
}
#[test]
fn missing_truncated_reordered_duplicated_mixed_volumes() {
    for v5 in [false, true] {
        let mut a = Spec::new("x", b"a");
        a.size = 2;
        a.after = true;
        let mut b = Spec::new("x", b"b");
        b.size = 2;
        b.before = true;
        b.crc = Some(crc32fast::hash(b"ab"));
        let first = archive(v5, 0, 2, true, &[a.clone()]);
        let last = archive(v5, 1, 2, true, &[b.clone()]);
        assert!(matches!(
            run(std::slice::from_ref(&first), v5, true, Limits::default()),
            Err(Error::Volumes(_))
        ));
        for cut in [1, 5, first.len() - 1] {
            assert!(
                run(
                    &[first[..cut].to_vec(), last.clone()],
                    v5,
                    true,
                    Limits::default()
                )
                .is_err()
            );
        }
        let parent = tempfile::tempdir().unwrap();
        let paths = write_volumes(parent.path(), &[first.clone(), last.clone()], true);
        assert!(matches!(
            VolumeSet::open(&[paths[1].clone(), paths[0].clone()], Limits::default()),
            Err(Error::Volumes(_))
        ));
        assert!(matches!(
            VolumeSet::open(&[paths[0].clone(), paths[0].clone()], Limits::default()),
            Err(Error::Volumes(_))
        ));
        b.name = "wrong".into();
        assert!(matches!(
            run(
                &[first.clone(), archive(v5, 1, 2, true, &[b.clone()])],
                v5,
                true,
                Limits::default()
            ),
            Err(Error::Volumes(_))
        ));
        b.name = "x".into();
        b.before = false;
        assert!(matches!(
            run(
                &[first.clone(), archive(v5, 1, 2, true, &[b])],
                v5,
                true,
                Limits::default()
            ),
            Err(Error::Volumes(_))
        ));
        assert!(
            run(
                &[first, archive(!v5, 1, 2, true, &[])],
                v5,
                true,
                Limits::default()
            )
            .is_err()
        );
        a.crc = Some(0);
        assert!(matches!(
            run(
                &[archive(v5, 0, 2, true, &[a]), last],
                v5,
                true,
                Limits::default()
            ),
            Err(Error::Checksum(_))
        ));
    }
}
#[test]
fn all_budgets_and_huge_declared_sizes() {
    for v5 in [false, true] {
        let bytes = archive(v5, 0, 1, false, &[Spec::new("a/b", b"12345678")]);
        let limits = [
            Limits {
                max_header_bytes: 6,
                ..Limits::default()
            },
            Limits {
                max_input_bytes: 1,
                ..Limits::default()
            },
            Limits {
                max_output_bytes: 7,
                ..Limits::default()
            },
            Limits {
                max_file_bytes: 7,
                ..Limits::default()
            },
            Limits {
                max_entries: 0,
                ..Limits::default()
            },
            Limits {
                max_headers: 1,
                ..Limits::default()
            },
            Limits {
                max_path_depth: 1,
                ..Limits::default()
            },
            Limits {
                max_path_bytes: 2,
                ..Limits::default()
            },
            Limits {
                max_volumes: 0,
                ..Limits::default()
            },
        ];
        for l in limits {
            assert!(matches!(extract(&bytes, l).1, Err(Error::Limit(_))));
        }
        for size in [u32::MAX as u64, 1 << 32, 1 << 63, u64::MAX] {
            let mut e = Spec::new("x", b"");
            e.size = size;
            e.high = true;
            assert!(
                extract(
                    &archive(v5, 0, 1, false, &[e]),
                    Limits {
                        max_file_bytes: 2,
                        ..Limits::default()
                    }
                )
                .1
                .is_err()
            );
        }
        let mut e = Spec::new("x", b"hello");
        e.size = 4;
        assert!(
            extract(&archive(v5, 0, 1, false, &[e]), Limits::default())
                .1
                .is_err()
        );
    }
}
#[test]
fn cancellation_and_success_drop_cleanup() {
    let input = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let p = input.path().join("a.rar");
    fs::write(
        &p,
        archive(true, 0, 1, false, &[Spec::new("x", &vec![5; 200000])]),
    )
    .unwrap();
    let cancel = AtomicBool::new(true);
    assert!(matches!(
        VolumeSet::open(std::slice::from_ref(&p), Limits::default())
            .unwrap()
            .extract(output.path(), &cancel, |_| {}),
        Err(Error::Cancelled)
    ));
    cancel.store(false, Ordering::Relaxed);
    let result = VolumeSet::open(std::slice::from_ref(&p), Limits::default())
        .unwrap()
        .extract(output.path(), &cancel, |p| {
            assert!(p.bytes_written <= 65536);
            cancel.store(true, Ordering::Relaxed);
        });
    assert!(matches!(result, Err(Error::Cancelled)));
    assert_eq!(fs::read_dir(output.path()).unwrap().count(), 0);
    cancel.store(false, Ordering::Relaxed);
    let result = VolumeSet::open(&[p], Limits::default())
        .unwrap()
        .extract(output.path(), &cancel, |_| {})
        .unwrap();
    let path = result.path().to_owned();
    drop(result);
    assert!(!path.exists());
}
#[test]
fn truncation_at_every_byte_and_deterministic_garbage_never_panics() {
    let parent = tempfile::tempdir().unwrap();
    let p = parent.path().join("a.rar");
    let out = parent.path().join("out");
    fs::create_dir(&out).unwrap();
    let limits = Limits {
        max_header_bytes: 4096,
        max_input_bytes: 16384,
        max_output_bytes: 1024,
        max_file_bytes: 1024,
        max_headers: 50,
        ..Limits::default()
    };
    for v5 in [false, true] {
        let bytes = archive(v5, 0, 1, false, &[Spec::new("x", b"hello").hash()]);
        for cut in 0..bytes.len() {
            fs::write(&p, &bytes[..cut]).unwrap();
            assert!(
                VolumeSet::open(std::slice::from_ref(&p), limits.clone())
                    .unwrap()
                    .extract(&out, &AtomicBool::new(false), |_| {})
                    .is_err()
            );
        }
        for i in 0..bytes.len() {
            let mut mutated = bytes.clone();
            mutated[i] ^= 0xff;
            fs::write(&p, mutated).unwrap();
            let _ = VolumeSet::open(std::slice::from_ref(&p), limits.clone())
                .unwrap()
                .extract(&out, &AtomicBool::new(false), |_| {});
        }
    }
    let mut rng = 0x12345678u32;
    for n in 0..512 {
        let mut bytes = vec![0; n];
        for b in &mut bytes {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            *b = rng as u8;
        }
        if n > 8 {
            bytes[..8].copy_from_slice(SIG5);
        }
        fs::write(&p, bytes).unwrap();
        let _ = VolumeSet::open(std::slice::from_ref(&p), limits.clone())
            .unwrap()
            .extract(&out, &AtomicBool::new(false), |_| {});
    }
    assert_eq!(fs::read_dir(out).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn capability_operations_do_not_follow_symlinks_outside_staging() {
    use std::os::unix::fs::symlink;
    let input = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("sentinel"), b"untouched").unwrap();
    let p = input.path().join("a.rar");
    fs::write(
        &p,
        archive(
            true,
            0,
            1,
            false,
            &[Spec::new("first", b"ok"), Spec::new("evil/escape", b"bad")],
        ),
    )
    .unwrap();
    let mut inserted = false;
    let result = VolumeSet::open(&[p], Limits::default()).unwrap().extract(
        output.path(),
        &AtomicBool::new(false),
        |progress| {
            if !inserted && progress.entries_done == 1 {
                let staging = fs::read_dir(output.path())
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path();
                symlink(outside.path(), staging.join("evil")).unwrap();
                inserted = true;
            }
        },
    );
    assert!(inserted);
    assert!(result.is_err());
    assert!(!outside.path().join("escape").exists());
    assert_eq!(
        fs::read(outside.path().join("sentinel")).unwrap(),
        b"untouched"
    );
    assert_eq!(fs::read_dir(output.path()).unwrap().count(), 0);
}

#[test]
fn malformed_valid_crc_headers_extras_and_required_blocks() {
    for flags in [4, 32, 64] {
        let mut bytes = SIG5.to_vec();
        bytes.extend(block5(1, 0, &vint(flags), &[], &[]));
        bytes.extend(block5(5, 0, &[0], &[], &[]));
        assert!(extract(&bytes, Limits::default()).1.is_err());
    }
    for extra_data in [
        vec![0],
        vec![128; 10],
        vec![4, 2, 0, 1],
        extra(2, &[1]),
        extra(2, &[0]),
        extra(5, &[0]),
    ] {
        let mut e = Spec::new("x", b"ok");
        e.extra = extra_data;
        assert!(
            extract(&archive(true, 0, 1, false, &[e]), Limits::default())
                .1
                .is_err()
        );
    }
    let mut e = Spec::new("x", b"ok");
    e.flags = 8;
    assert!(matches!(
        extract(&archive(true, 0, 1, false, &[e]), Limits::default()).1,
        Err(Error::Unsupported { .. })
    ));
    let main = block5(1, 0, &[0], &[], &[]);
    let end = block5(5, 0, &[0], &[], &[]);
    let mut bytes = SIG5.to_vec();
    bytes.extend(&main);
    bytes.extend(block5(999, 0, &[], &[], &[]));
    bytes.extend(&end);
    assert!(matches!(
        extract(&bytes, Limits::default()).1,
        Err(Error::Unsupported { .. })
    ));
    let mut bytes = SIG5.to_vec();
    bytes.extend(&main);
    bytes.extend(block5(999, 4, &[], &[], b"skip"));
    bytes.extend(Spec::new("x", b"ok").header(true));
    bytes.extend(&end);
    assert!(extract(&bytes, Limits::default()).1.is_ok());
    let mut bytes = SIG5.to_vec();
    bytes.extend(block5(4, 0, &[0], &[], &[]));
    assert!(matches!(
        extract(&bytes, Limits::default()).1,
        Err(Error::Unsupported { .. })
    ));
    let mut bytes = SIG5.to_vec();
    bytes.extend([0; 4]);
    bytes.extend([128; 3]);
    assert!(matches!(
        extract(&bytes, Limits::default()).1,
        Err(Error::Corrupt(_))
    ));
    let mut bytes = SIG4.to_vec();
    bytes.extend(block4(0x73, 0x80, &[0; 6]));
    assert!(matches!(
        extract(&bytes, Limits::default()).1,
        Err(Error::Unsupported { .. })
    ));
}

#[test]
fn recomputed_header_crc_mutations_reach_parser_fields_without_panics() {
    // Valid outer CRC after mutations reaches deep fields rather than stopping at CRC.
    let parent = tempfile::tempdir().unwrap();
    let p = parent.path().join("a.rar");
    let out = parent.path().join("out");
    fs::create_dir(&out).unwrap();
    let limits = Limits {
        max_header_bytes: 1024,
        max_input_bytes: 8192,
        max_output_bytes: 1024,
        max_file_bytes: 1024,
        max_headers: 25,
        ..Limits::default()
    };
    for v5 in [false, true] {
        let original = Spec::new("folder/x", b"hello").hash().header(v5);
        let header_end = if v5 {
            5 + usize::from(original[4])
        } else {
            usize::from(u16::from_le_bytes([original[5], original[6]]))
        };
        for offset in if v5 { 5 } else { 7 }..header_end {
            for mutation in [0, 127, 128, 255] {
                let mut file = original.clone();
                file[offset] = mutation;
                if v5 {
                    let crc = crc32fast::hash(&file[4..header_end]);
                    file[..4].copy_from_slice(&crc.to_le_bytes());
                } else {
                    let crc = crc32fast::hash(&file[2..header_end]) as u16;
                    file[..2].copy_from_slice(&crc.to_le_bytes());
                }
                let mut bytes = if v5 { SIG5.to_vec() } else { SIG4.to_vec() };
                bytes.extend(if v5 {
                    block5(1, 0, &[0], &[], &[])
                } else {
                    block4(0x73, 0x100, &[0; 6])
                });
                bytes.extend(file);
                bytes.extend(if v5 {
                    block5(5, 0, &[0], &[], &[])
                } else {
                    block4(0x7b, 0x4000, &[])
                });
                fs::write(&p, bytes).unwrap();
                let _ = VolumeSet::open(std::slice::from_ref(&p), limits.clone())
                    .unwrap()
                    .extract(&out, &AtomicBool::new(false), |_| {});
            }
        }
    }
    assert_eq!(fs::read_dir(out).unwrap().count(), 0);
}
