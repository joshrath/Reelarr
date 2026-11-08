//! Independent fixtures; no reference-application code or test data reused.
use reelarr_nzb::{
    InMemoryNzbStore, JobId, NzbStore, ParseError, ParseLimits, SegmentKey, StoreError, parse,
};
const VALID: &str = include_str!("fixtures/multi-file.nzb");
fn read(s: &str) -> Result<reelarr_nzb::NzbDocument, ParseError> {
    parse(s.as_bytes(), ParseLimits::default())
}

#[test]
fn valid_unicode_metadata_groups_and_sorted_segments() {
    let doc = read(VALID).unwrap();
    assert_eq!(doc.title.as_deref(), Some("映画 🎬 & café"));
    assert_eq!(doc.password.as_deref(), Some("p&ss✓"));
    assert_eq!(doc.category.as_deref(), Some("movies"));
    assert_eq!(doc.files.len(), 2);
    assert_eq!(doc.files[0].subject, "映画 \"part 1\"");
    assert_eq!(doc.files[0].poster, "author@example.test");
    assert_eq!(doc.files[0].date, 1700000000);
    assert_eq!(doc.files[0].groups.len(), 2);
    assert_eq!(doc.files[0].segments[0].message_id, "part1@example.test");
}
#[test]
fn malformed_and_empty_documents() {
    for text in [
        include_str!("fixtures/malformed.nzb"),
        include_str!("fixtures/empty-file.nzb"),
        "",
        "<nzb/>",
        "<nzb>",
        "<nzb/><nzb/>",
        "outside<nzb/>",
        "<nzb><unknown/></nzb>",
    ] {
        assert!(read(text).is_err(), "{text}");
    }
    assert!(parse([0xff, 0xfe].as_slice(), ParseLimits::default()).is_err());
}
#[test]
fn dtd_and_custom_entities_are_rejected() {
    for text in [
        include_str!("fixtures/doctype.nzb"),
        include_str!("fixtures/entity.nzb"),
    ] {
        assert!(matches!(read(text), Err(ParseError::ForbiddenEntity)));
    }
    assert!(matches!(
        read(&VALID.replace("p&amp;ss&#x2713;", "&custom;")),
        Err(ParseError::ForbiddenEntity)
    ));
    assert!(read(&VALID.replace("author@example.test", "&custom;")).is_err());
}
#[test]
fn segment_validation() {
    for text in [
        include_str!("fixtures/duplicate.nzb"),
        include_str!("fixtures/missing.nzb"),
    ] {
        assert!(read(text).is_err());
    }
    for bad in ["0", "-1", "+1", "4294967296", "abc"] {
        assert!(read(&VALID.replace("number=\"2\"", &format!("number=\"{bad}\""))).is_err());
    }
    for bad in ["0", "-1", "+1", "18446744073709551616", "nan"] {
        assert!(read(&VALID.replace("bytes=\"20\"", &format!("bytes=\"{bad}\""))).is_err());
    }
    assert!(read(&VALID.replace("bytes=\"20\"", "bytes=\"18446744073709551615\"")).is_err());
    for bad in [
        "no-domain",
        "@",
        "left@",
        "@right",
        "a@b@c",
        "bad&#10;@test",
        "&lt;id@test&gt;",
        "",
        "a b@test",
    ] {
        assert!(read(&VALID.replace("part2@example.test", bad)).is_err());
    }
}
#[test]
fn every_resource_limit_and_exact_byte_boundary() {
    let limits = ParseLimits {
        max_bytes: VALID.len() as u64,
        ..Default::default()
    };
    assert!(parse(VALID.as_bytes(), limits).is_ok());
    for limits in [
        ParseLimits {
            max_bytes: VALID.len() as u64 - 1,
            ..Default::default()
        },
        ParseLimits {
            max_depth: 2,
            ..Default::default()
        },
        ParseLimits {
            max_field_bytes: 8,
            ..Default::default()
        },
        ParseLimits {
            max_files: 1,
            ..Default::default()
        },
        ParseLimits {
            max_segments: 2,
            ..Default::default()
        },
    ] {
        assert!(matches!(
            parse(VALID.as_bytes(), limits),
            Err(ParseError::Limit(_))
        ));
    }
    let huge = format!("<nzb><head><meta type=\"title\">{}", "x".repeat(1000));
    assert!(matches!(
        parse(
            huge.as_bytes(),
            ParseLimits {
                max_bytes: 100,
                ..Default::default()
            }
        ),
        Err(ParseError::Limit("input bytes"))
    ));
    // Multiple text/CDATA/entity events must share the field budget.
    assert!(
        parse(
            VALID
                .replace("p&amp;ss&#x2713;", &"x&amp;".repeat(30))
                .as_bytes(),
            ParseLimits {
                max_field_bytes: 50,
                ..Default::default()
            }
        )
        .is_err()
    );
}
#[test]
fn unsupported_namespace_duplicate_attrs_and_missing_fields() {
    for text in [
        VALID.replace("http://www.newzbin.com/DTD/2003/nzb", "https://wrong.test"),
        VALID.replace("<groups>", "<groups xmlns=\"https://wrong.test\">"),
        VALID.replace("date=\"1700000000\"", ""),
        VALID.replace("date=\"1700000000\"", "date=\"1\" date=\"2\""),
        VALID.replace(
            "<groups><group>alt.binaries.example</group><group>alt.example</group></groups>",
            "<groups/>",
        ),
        VALID.replace("encoding=\"UTF-8\"", "encoding=\"UTF-16\""),
    ] {
        assert!(read(&text).is_err());
    }
}
#[test]
fn chunked_read_and_io_error() {
    struct Chunks<'a>(&'a [u8]);
    impl std::io::Read for Chunks<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(1).min(self.0.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }
    assert!(parse(Chunks(VALID.as_bytes()), ParseLimits::default()).is_ok());
    struct Broken;
    impl std::io::Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("test"))
        }
    }
    assert!(matches!(
        parse(Broken, ParseLimits::default()),
        Err(ParseError::Io(_))
    ));
}
#[tokio::test]
async fn store_roundtrip_and_worker_restart_resume() {
    let backend = InMemoryNzbStore::default();
    let id = JobId(1);
    assert!(backend.load(id).await.unwrap().is_none());
    let doc = read(VALID).unwrap();
    backend.insert(id, doc.clone()).await.unwrap();
    assert!(backend.load(id).await.unwrap().unwrap().document == doc);
    assert!(matches!(
        backend.insert(id, doc).await,
        Err(StoreError::AlreadyExists)
    ));
    let first = SegmentKey {
        file_index: 0,
        number: 1,
    };
    let last = SegmentKey {
        file_index: 1,
        number: 1,
    };
    let worker = backend.clone();
    worker
        .checkpoint(id, &[(first, true), (last, true), (first, true)])
        .await
        .unwrap();
    drop(worker); // Simulated worker restart, not a claim of process durability.
    let restarted = backend.clone();
    let plan = restarted.load(id).await.unwrap().unwrap().resume().unwrap();
    assert_eq!(plan.bytes_done, 40);
    assert_eq!(plan.bytes_total, 60);
    assert_eq!(
        plan.remaining,
        vec![SegmentKey {
            file_index: 0,
            number: 2
        }]
    );
    restarted.checkpoint(id, &[(first, false)]).await.unwrap();
    assert_eq!(
        backend
            .load(id)
            .await
            .unwrap()
            .unwrap()
            .resume()
            .unwrap()
            .bytes_done,
        30
    );
}
#[tokio::test]
async fn invalid_batches_are_atomic_and_jobs_isolated() {
    let store = InMemoryNzbStore::default();
    for id in [JobId(1), JobId(2)] {
        store.insert(id, read(VALID).unwrap()).await.unwrap();
    }
    let valid = SegmentKey {
        file_index: 0,
        number: 1,
    };
    for bad in [
        SegmentKey {
            file_index: 0,
            number: 0,
        },
        SegmentKey {
            file_index: 20,
            number: 1,
        },
        SegmentKey {
            file_index: 0,
            number: 3,
        },
    ] {
        assert!(matches!(
            store
                .checkpoint(JobId(1), &[(valid, true), (bad, true)])
                .await,
            Err(StoreError::UnknownSegment(_))
        ));
    }
    assert_eq!(
        store
            .load(JobId(1))
            .await
            .unwrap()
            .unwrap()
            .resume()
            .unwrap()
            .bytes_done,
        0
    );
    store.checkpoint(JobId(1), &[(valid, true)]).await.unwrap();
    assert_eq!(
        store
            .load(JobId(2))
            .await
            .unwrap()
            .unwrap()
            .resume()
            .unwrap()
            .bytes_done,
        0
    );
    assert!(matches!(
        store.checkpoint(JobId(3), &[]).await,
        Err(StoreError::NotFound)
    ));
    assert!(store.insert(JobId(4), Default::default()).await.is_err());
    let mut corrupted = store.load(JobId(1)).await.unwrap().unwrap();
    corrupted.completed.insert(SegmentKey {
        file_index: 90,
        number: 1,
    });
    assert!(corrupted.resume().is_err());
}

#[test]
fn illegal_xml_characters_and_duplicate_declarations() {
    for bad in ["\u{1}", "&#1;", "&#xFFFE;", "&#0;"] {
        assert!(read(&VALID.replace("p&amp;ss&#x2713;", bad)).is_err());
        assert!(read(&VALID.replace("author@example.test", bad)).is_err());
    }
    assert!(read(&format!("<?xml version=\"1.0\"?>{VALID}")).is_err());
    assert!(read(&format!("&#32;{VALID}")).is_err());
}
