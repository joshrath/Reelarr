//! All vectors are independently generated here from the yEnc 1.3 arithmetic.
//! No third-party encoder implementations or fixture binaries are used.
use reelarr_nzb::{
    InMemoryNzbStore, JobId, NzbDocument, NzbFile, NzbStore, Segment,
    assembly::{Assembler, AssemblyError},
    yenc::{BodyEncoding, DecodeError, DecodeLimits, Decoder, validate_filename},
};
use std::{
    fs,
    io::{self, Write},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

// Tiny test-only encoder: +42 modulo 256, optional +64 escapes, physical
// line wrapping without splitting an escape pair, then NNTP dot stuffing.
fn encode(
    data: &[u8],
    name: &str,
    range: Option<(u32, u32, u64, u64)>,
    full_crc: Option<u32>,
    lf: &str,
    wire: bool,
    escape_all: bool,
) -> Vec<u8> {
    let header = match range {
        Some((part, total, start, size)) => format!(
            "=ybegin part={part} total={total} line=32 size={size} name={name}{lf}=ypart begin={} end={}{lf}",
            start + 1,
            start + data.len() as u64
        ),
        None => format!("=ybegin line=32 size={} name={name}{lf}", data.len()),
    };
    let mut out = header.into_bytes();
    let mut line = Vec::new();
    let flush = |line: &mut Vec<u8>, out: &mut Vec<u8>| {
        if wire && line.first() == Some(&b'.') {
            out.push(b'.');
        }
        out.append(line);
        out.extend_from_slice(lf.as_bytes());
    };
    for &byte in data {
        let b = byte.wrapping_add(42);
        if (escape_all && !matches!(b.wrapping_add(64), 10 | 13)) || matches!(b, 0 | 10 | 13 | 61) {
            line.push(b'=');
            line.push(b.wrapping_add(64));
        } else {
            line.push(b);
        }
        if line.len() >= 32 {
            flush(&mut line, &mut out);
        }
    }
    if !line.is_empty() {
        flush(&mut line, &mut out);
    }
    let part = range
        .map(|(p, _, _, _)| format!(" part={p} pcrc32={:08x}", crc32fast::hash(data)))
        .unwrap_or_default();
    let crc = full_crc
        .map(|v| format!(" crc32={v:08x}"))
        .unwrap_or_default();
    out.extend_from_slice(format!("=yend size={}{part}{crc}{lf}", data.len()).as_bytes());
    out
}
fn decode(
    bytes: &[u8],
    chunk: usize,
    encoding: BodyEncoding,
    limits: DecodeLimits,
) -> Result<(Vec<u8>, reelarr_nzb::yenc::PartInfo), DecodeError> {
    let mut decoder = Decoder::new(Vec::new(), limits, encoding);
    for piece in bytes.chunks(chunk) {
        decoder.push(piece)?;
    }
    decoder.finish()
}
fn one(data: &[u8]) -> Vec<u8> {
    encode(
        data,
        "payload.bin",
        None,
        Some(crc32fast::hash(data)),
        "\r\n",
        true,
        false,
    )
}
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Dir(PathBuf);
impl Dir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "reelarr-yenc-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn nzb(count: u32) -> NzbFile {
    NzbFile {
        subject: "untrusted subject".into(),
        poster: "test".into(),
        date: 0,
        groups: vec!["alt.test".into()],
        segments: (1..=count)
            .map(|number| Segment {
                number,
                message_id: format!("{number}@example.org"),
                bytes: 100,
            })
            .collect(),
    }
}
fn assembler(dir: &Dir, count: u32) -> Assembler {
    Assembler::new(&dir.0, &nzb(count), DecodeLimits::default()).unwrap()
}

#[test]
fn single_part_all_bytes_and_critical_escapes() {
    let bytes: Vec<_> = (0..=255).collect();
    for all in [false, true] {
        for lf in ["\n", "\r\n"] {
            let input = encode(
                &bytes,
                "a size=999 file.bin",
                None,
                Some(crc32fast::hash(&bytes)),
                lf,
                true,
                all,
            );
            for chunk in [1, 2, 3, 17, 128, input.len()] {
                let (out, info) = decode(
                    &input,
                    chunk,
                    BodyEncoding::DotStuffed,
                    DecodeLimits::default(),
                )
                .unwrap();
                assert_eq!(out, bytes);
                assert_eq!(info.name, "a size=999 file.bin");
                assert_eq!(
                    (info.start, info.end, info.part, info.total),
                    (0, 256, 1, Some(1))
                );
                assert!(info.file_crc32.is_some());
            }
        }
    }
}
#[test]
fn deterministic_fuzz_style_round_trips_and_chunk_boundaries() {
    let mut seed = 0x13579bdf_u64;
    for len in (0..128).chain([255, 256, 511, 1024, 65536]) {
        let data: Vec<u8> = (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect();
        let input = one(&data);
        let (out, _) = decode(
            &input,
            (seed as usize % 37) + 1,
            BodyEncoding::DotStuffed,
            DecodeLimits::default(),
        )
        .unwrap();
        assert_eq!(out, data);
    }
}
#[test]
fn dot_stuffing_is_explicit_and_exactly_once() {
    // Four encodes as a dot; leading two dots must retain one after unstuffing.
    let input = one(&[4, 4, 0]);
    assert!(input.windows(3).any(|w| w == b"..."));
    assert_eq!(
        decode(&input, 1, BodyEncoding::DotStuffed, DecodeLimits::default())
            .unwrap()
            .0,
        [4, 4, 0]
    );
    let plain = encode(
        &[4, 4, 0],
        "payload.bin",
        None,
        Some(crc32fast::hash(&[4, 4, 0])),
        "\n",
        false,
        false,
    );
    assert_eq!(
        decode(&plain, 1, BodyEncoding::Unstuffed, DecodeLimits::default())
            .unwrap()
            .0,
        [4, 4, 0]
    );
    assert!(decode(&plain, 2, BodyEncoding::DotStuffed, DecodeLimits::default()).is_err());
    let mut with_end = input.clone();
    with_end.extend_from_slice(b".\r\n");
    decode(
        &with_end,
        1,
        BodyEncoding::DotStuffed,
        DecodeLimits::default(),
    )
    .unwrap();
    with_end.extend_from_slice(b"more\r\n");
    assert!(
        decode(
            &with_end,
            1,
            BodyEncoding::DotStuffed,
            DecodeLimits::default()
        )
        .is_err()
    );
    assert!(
        decode(
            b".\r\n",
            1,
            BodyEncoding::DotStuffed,
            DecodeLimits::default()
        )
        .is_err()
    );
}
#[test]
fn prose_optional_checksums_and_unterminated_trailer() {
    let mut input = b"Some explanatory prose\n".to_vec();
    input.extend(encode(b"hello", "safe.bin", None, None, "\n", false, false));
    input.pop(); // trailer need not have LF
    let (out, info) = decode(&input, 1, BodyEncoding::Unstuffed, DecodeLimits::default()).unwrap();
    assert_eq!(out, b"hello");
    assert_eq!(info.file_crc32, None);
    input.extend_from_slice(b"\nend prose\n");
    decode(&input, 3, BodyEncoding::Unstuffed, DecodeLimits::default()).unwrap();
    input.extend(one(b"second attachment"));
    assert!(decode(&input, 3, BodyEncoding::Unstuffed, DecodeLimits::default()).is_err());
}
#[test]
fn corruption_truncation_and_poisoned_decoder() {
    let mut input = one(b"hello");
    let index = input.windows(2).position(|w| w == b"\r\n").unwrap() + 2;
    input[index] ^= 1;
    assert!(matches!(
        decode(&input, 7, BodyEncoding::DotStuffed, DecodeLimits::default()),
        Err(DecodeError::Crc)
    ));
    let input = one(b"hello");
    for end in 0..input.windows(6).position(|w| w == b"=yend ").unwrap() {
        assert!(
            decode(
                &input[..end],
                5,
                BodyEncoding::DotStuffed,
                DecodeLimits::default()
            )
            .is_err()
        );
    }
    let mut d = Decoder::new(Vec::new(), DecodeLimits::default(), BodyEncoding::Unstuffed);
    assert!(d.push(b"=ybegin line=32 size=1 name=f\n=\n").is_err());
    assert!(matches!(d.push(b"x"), Err(DecodeError::Failed)));
    assert!(matches!(d.finish(), Err(DecodeError::Failed)));
}
#[test]
fn malformed_headers_ranges_and_trailers_never_panic() {
    for input in [
        "=ybegin line=0 size=1 name=x\n",
        "=ybegin line=-1 size=1 name=x\n",
        "=ybegin line=32 line=32 size=1 name=x\n",
        "=ybegin line=32 size=18446744073709551616 name=x\n",
        "=ybegin part=0 line=32 size=1 name=x\n",
        "=ybegin part=2 total=1 line=32 size=1 name=x\n",
        "=ybegin total=1 line=32 size=1 name=x\n",
        "=ybegin line=32 size=1\n",
        "=ybegin part=1 line=32 size=1 name=x\n=ypart begin=0 end=1\n",
        "=ybegin part=1 line=32 size=1 name=x\n=ypart begin=2 end=1\n",
        "=ybegin part=1 line=32 size=1 name=x\n=ypart begin=1 end=2\n",
        "=ybegin part=1 line=32 size=1 name=x\n*\n",
        "=ybegin line=32 size=1 name=x\n*\n=yend size=2\n",
        "=ybegin line=32 size=1 name=x\n*\n=yend size=1 crc32=xx\n",
        "=ybegin line=32 size=1 name=x\n*\n=yend size=1 part=1\n",
        "=ybegin part=1 line=32 size=1 name=x\n=ypart begin=1 end=1\n*\n=yend size=1 part=1\n",
        "=ybegin line=32 size=1 name=x\n**\n=yend size=2\n",
        "=ybegin line=32 size=1 name=x\n\0\n=yend size=1\n",
    ] {
        assert!(
            decode(
                input.as_bytes(),
                1,
                BodyEncoding::Unstuffed,
                DecodeLimits::default()
            )
            .is_err(),
            "{input:?}"
        );
    }
    // Mutation fuzz-style: arbitrary hostile bytes cannot panic; hard caps
    // keep error paths bounded. No assertions that CRC-free data is authentic.
    let valid = one(b"test mutation input");
    for i in 0..valid.len() {
        for byte in [0, 10, 13, 61, 255] {
            let mut bytes = valid.clone();
            bytes[i] = byte;
            let _ = decode(&bytes, 1, BodyEncoding::DotStuffed, DecodeLimits::default());
        }
    }
}
#[test]
fn hard_limits_enforced_even_in_ignored_text() {
    let bytes = one(b"12345");
    for limits in [
        DecodeLimits {
            max_line_bytes: 8,
            ..DecodeLimits::default()
        },
        DecodeLimits {
            max_input_bytes: 10,
            ..DecodeLimits::default()
        },
        DecodeLimits {
            max_part_bytes: 4,
            ..DecodeLimits::default()
        },
        DecodeLimits {
            max_file_bytes: 4,
            ..DecodeLimits::default()
        },
    ] {
        assert!(matches!(
            decode(&bytes, 1, BodyEncoding::DotStuffed, limits),
            Err(DecodeError::Limit(_))
        ));
    }
    let limits = DecodeLimits {
        max_line_bytes: 8,
        ..DecodeLimits::default()
    };
    assert!(matches!(
        decode(
            b"long preamble without newlines",
            1,
            BodyEncoding::Unstuffed,
            limits
        ),
        Err(DecodeError::Limit("line"))
    ));
    let bytes = encode(b"12345", "x", Some((1, 1, 0, 5)), None, "\n", false, false);
    let limits = DecodeLimits {
        max_part_bytes: 4,
        ..DecodeLimits::default()
    };
    assert!(matches!(
        decode(&bytes, 1, BodyEncoding::Unstuffed, limits),
        Err(DecodeError::Limit("part"))
    ));
}
#[test]
fn malicious_filenames_rejected() {
    for name in [
        "",
        ".",
        "..",
        "../x",
        "/tmp/x",
        "a/b",
        "a\\b",
        "C:x",
        "x\0y",
        "x\u{7f}",
        "CON",
        "nul.txt",
        "COM1.bin",
        "lpt9",
        "LPT¹.txt",
        "x.",
        "x ",
        " x",
        "a:b",
        "x?",
        "x*",
        "a|b",
    ] {
        assert!(validate_filename(name).is_err(), "{name:?}");
    }
    assert!(validate_filename(&"x".repeat(256)).is_err());
    for name in ["résumé.bin", "my movie.mkv", "company.txt", "com10.bin"] {
        validate_filename(name).unwrap();
    }
    for name in ["../escape", "CON.txt", "a\\b", "a\0b", "x."] {
        assert!(
            decode(
                &encode(b"a", name, None, None, "\n", false, false),
                1,
                BodyEncoding::Unstuffed,
                DecodeLimits::default()
            )
            .is_err()
        );
    }
}

#[test]
fn multipart_out_of_order_checksums_and_offsets() {
    let dir = Dir::new();
    let mut a = assembler(&dir, 3);
    let data: Vec<_> = (0..=255).cycle().take(1024 * 1024).collect();
    let ranges = [(0, 300_000), (300_000, 700_000), (700_000, data.len())];
    for i in [2, 0, 1] {
        let (start, end) = ranges[i];
        let bytes = encode(
            &data[start..end],
            "movie.mkv",
            Some((i as u32 + 1, 3, start as u64, data.len() as u64)),
            Some(crc32fast::hash(&data)),
            "\r\n",
            true,
            false,
        );
        a.add_part(i as u32 + 1, bytes.as_slice(), BodyEncoding::DotStuffed)
            .unwrap();
    }
    assert_eq!(a.completed_parts(), 3);
    assert_eq!(a.missing_parts().count(), 0);
    let done = a.finish().unwrap();
    assert!(done.whole_crc_verified());
    assert!(done.path().starts_with(&dir.0));
    assert_eq!(fs::read(done.path()).unwrap(), data);
}
#[test]
fn missing_parts_holes_overlaps_duplicates_and_mismatched_headers() {
    let part = |n, start, size, name| {
        encode(
            b"abc",
            name,
            Some((n, 2, start, size)),
            None,
            "\n",
            false,
            false,
        )
    };
    let dir = Dir::new();
    let mut a = assembler(&dir, 2);
    a.add_part(1, part(1, 0, 6, "x").as_slice(), BodyEncoding::Unstuffed)
        .unwrap();
    assert_eq!(a.missing_parts().collect::<Vec<_>>(), [2]);
    assert!(matches!(
        a.add_part(1, part(1, 0, 6, "x").as_slice(), BodyEncoding::Unstuffed),
        Err(AssemblyError::Overlap)
    ));
    assert!(matches!(
        a.add_part(2, part(2, 2, 6, "x").as_slice(), BodyEncoding::Unstuffed),
        Err(AssemblyError::Overlap)
    ));
    assert!(matches!(a.finish(), Err(AssemblyError::Incomplete)));
    let mut a = assembler(&dir, 2);
    a.add_part(1, part(1, 0, 7, "x").as_slice(), BodyEncoding::Unstuffed)
        .unwrap();
    a.add_part(2, part(2, 4, 7, "x").as_slice(), BodyEncoding::Unstuffed)
        .unwrap();
    assert!(matches!(a.finish(), Err(AssemblyError::Incomplete)));
    for bytes in [
        part(2, 3, 7, "x"),
        part(2, 3, 6, "other"),
        part(1, 3, 6, "x"),
        encode(b"abc", "x", Some((2, 3, 3, 6)), None, "\n", false, false),
    ] {
        let mut a = assembler(&dir, 2);
        a.add_part(1, part(1, 0, 6, "x").as_slice(), BodyEncoding::Unstuffed)
            .unwrap();
        assert!(matches!(
            a.add_part(2, bytes.as_slice(), BodyEncoding::Unstuffed),
            Err(AssemblyError::Metadata)
        ));
    }
}
#[test]
fn multipart_crc_failure_then_retry_and_whole_crc_failure() {
    let dir = Dir::new();
    let mut a = assembler(&dir, 2);
    let mut bad = encode(b"abc", "x", Some((1, 2, 0, 6)), None, "\n", false, false);
    let pos = bad.windows(7).position(|w| w == b"pcrc32=").unwrap() + 7;
    bad[pos] = if bad[pos] == b'0' { b'1' } else { b'0' };
    assert!(matches!(
        a.add_part(1, bad.as_slice(), BodyEncoding::Unstuffed),
        Err(AssemblyError::Decode(DecodeError::Crc))
    ));
    assert_eq!(a.completed_parts(), 0);
    for (n, start) in [(1, 0), (2, 3)] {
        a.add_part(
            n,
            encode(
                b"abc",
                "x",
                Some((n, 2, start, 6)),
                Some(0),
                "\n",
                false,
                false,
            )
            .as_slice(),
            BodyEncoding::Unstuffed,
        )
        .unwrap();
    }
    assert!(matches!(
        a.finish(),
        Err(AssemblyError::Decode(DecodeError::Crc))
    ));
    assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0); // corrupt output discarded
}
#[test]
fn missing_multipart_total_is_supported() {
    let bytes = encode(b"abc", "x", Some((1, 1, 0, 3)), None, "\n", false, false);
    let mut bytes = bytes;
    let pos = bytes.windows(8).position(|w| w == b" total=1").unwrap();
    bytes.drain(pos..pos + 8);
    let (_, info) = decode(
        bytes.as_slice(),
        1,
        BodyEncoding::Unstuffed,
        DecodeLimits::default(),
    )
    .unwrap();
    assert_eq!(info.total, None);
    let dir = Dir::new();
    let mut a = assembler(&dir, 1);
    a.add_part(1, bytes.as_slice(), BodyEncoding::Unstuffed)
        .unwrap();
    assert!(!a.finish().unwrap().whole_crc_verified());
}
#[test]
fn empty_single_file_and_sink_io_errors() {
    let dir = Dir::new();
    let mut a = assembler(&dir, 1);
    a.add_part(1, one(b"").as_slice(), BodyEncoding::DotStuffed)
        .unwrap();
    assert_eq!(fs::metadata(a.finish().unwrap().path()).unwrap().len(), 0);
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("test sink failure"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut d = Decoder::new(Broken, DecodeLimits::default(), BodyEncoding::DotStuffed);
    assert!(matches!(d.push(&one(b"abc")), Err(DecodeError::Io(_))));
    assert!(matches!(d.finish(), Err(DecodeError::Failed)));
}
#[tokio::test]
async fn checkpoint_only_after_durable_complete_file_and_matching_nzb() {
    let dir = Dir::new();
    let mut a = assembler(&dir, 1);
    let store = InMemoryNzbStore::default();
    let job = JobId::new();
    store
        .insert(
            job,
            NzbDocument {
                files: vec![nzb(1)],
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    a.add_part(1, one(b"payload").as_slice(), BodyEncoding::DotStuffed)
        .unwrap();
    assert!(store.load(job).await.unwrap().unwrap().completed.is_empty());
    let done = a.finish().unwrap();
    assert!(done.checkpoint(&store, job, 1).await.is_err());
    done.checkpoint(&store, job, 0).await.unwrap();
    assert_eq!(store.load(job).await.unwrap().unwrap().completed.len(), 1);
}

#[test]
fn exact_limits_and_declared_line_length() {
    let data = b"12345";
    let bytes = one(data);
    let limits = DecodeLimits {
        max_input_bytes: bytes.len() as u64,
        max_part_bytes: 5,
        max_file_bytes: 5,
        ..DecodeLimits::default()
    };
    assert_eq!(
        decode(&bytes, 1, BodyEncoding::DotStuffed, limits)
            .unwrap()
            .0,
        data
    );
    let input = b"=ybegin line=2 size=4 name=x\n****\n=yend size=4\n";
    assert!(matches!(
        decode(input, 1, BodyEncoding::Unstuffed, DecodeLimits::default()),
        Err(DecodeError::Limit("data line"))
    ));
}
#[test]
fn conflicting_file_crc_and_trailer_metadata_rejected() {
    let dir = Dir::new();
    let mut a = assembler(&dir, 2);
    let first = encode(b"abc", "x", Some((1, 2, 0, 6)), Some(1), "\n", false, false);
    let second = encode(b"abc", "x", Some((2, 2, 3, 6)), Some(2), "\n", false, false);
    a.add_part(1, first.as_slice(), BodyEncoding::Unstuffed)
        .unwrap();
    assert!(matches!(
        a.add_part(2, second.as_slice(), BodyEncoding::Unstuffed),
        Err(AssemblyError::Metadata)
    ));
    for trailer in [
        "part=2 pcrc32=352441c2",
        "part=1 total=3 pcrc32=352441c2",
        "part=1 pcrc32=00000000",
    ] {
        let mut bytes = encode(b"abc", "x", Some((1, 2, 0, 6)), None, "\n", false, false);
        let pos = bytes.windows(6).position(|w| w == b"=yend ").unwrap();
        bytes.truncate(pos);
        bytes.extend(format!("=yend size=3 {trailer}\n").as_bytes());
        assert!(decode(&bytes, 1, BodyEncoding::Unstuffed, DecodeLimits::default()).is_err());
    }
}
#[test]
fn bad_input_io_and_unsafe_disk_names_do_not_leave_output() {
    struct Broken;
    impl io::Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("test input failure"))
        }
    }
    let dir = Dir::new();
    {
        let mut a = assembler(&dir, 1);
        assert!(matches!(
            a.add_part(1, Broken, BodyEncoding::Unstuffed),
            Err(AssemblyError::Io(_))
        ));
        for name in ["../outside", ".stage", "CON.txt"] {
            let bytes = encode(b"abc", name, None, None, "\n", false, false);
            assert!(
                a.add_part(1, bytes.as_slice(), BodyEncoding::Unstuffed)
                    .is_err()
            );
            assert_eq!(a.completed_parts(), 0);
        }
        a.add_part(1, one(b"valid retry").as_slice(), BodyEncoding::DotStuffed)
            .unwrap();
    }
    assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
}
#[cfg(unix)]
#[test]
fn existing_symlink_is_never_followed_or_overwritten() {
    use std::os::unix::fs::symlink;
    let dir = Dir::new();
    let mut a = assembler(&dir, 1);
    // Simulate an already present file entry. Concurrent mutation is outside
    // the private-job-directory contract, but create_new still fails closed.
    let child = fs::read_dir(&dir.0)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let target = dir.0.join("untouched");
    fs::write(&target, b"keep me").unwrap();
    let link = child.join("payload.bin");
    symlink(&target, &link).unwrap();
    assert!(matches!(
        a.add_part(1, one(b"no overwrite").as_slice(), BodyEncoding::DotStuffed),
        Err(AssemblyError::Io(_))
    ));
    assert_eq!(fs::read(&target).unwrap(), b"keep me");
    fs::remove_file(link).unwrap();
    a.add_part(1, one(b"safe").as_slice(), BodyEncoding::DotStuffed)
        .unwrap();
    assert_eq!(fs::read(a.finish().unwrap().path()).unwrap(), b"safe");
}
