//! Shared `NzbStore` behaviour, run against the in-memory store here and the
//! SQLite store in `reelarr-db` (included there with `#[path]`).
#![allow(dead_code)]

use reelarr_nzb::{JobId, NzbDocument, NzbFile, NzbStore, Segment, SegmentKey, StoreError};
use reelarr_types::MovieId;
use std::future::Future;

/// Two files: three segments (10, 20, 30 bytes) and one (40 bytes).
pub fn document() -> NzbDocument {
    let seg = |n: u32, bytes: u64| Segment {
        message_id: format!("part{n}.{bytes}@example.test"),
        number: n,
        bytes,
    };
    NzbDocument {
        title: Some("Example.Release.2026".into()),
        password: Some("p&ss\u{2713}".into()),
        category: Some("movies".into()),
        files: vec![
            NzbFile {
                subject: "\"example.mkv\" yEnc (1/3)".into(),
                poster: "poster@example.test".into(),
                date: 1_700_000_000,
                groups: vec!["alt.binaries.test".into(), "alt.binaries.other".into()],
                segments: vec![seg(1, 10), seg(2, 20), seg(3, 30)],
            },
            NzbFile {
                subject: "\"example.par2\" yEnc (1/1)".into(),
                poster: "poster@example.test".into(),
                date: u64::from(u32::MAX) + 7,
                groups: vec!["alt.binaries.test".into()],
                segments: vec![seg(1, 40)],
            },
        ],
    }
}

pub fn key(file_index: usize, number: u32) -> SegmentKey {
    SegmentKey { file_index, number }
}

async fn done<S: NzbStore>(store: &S, id: JobId) -> u64 {
    store
        .load(id)
        .await
        .unwrap()
        .unwrap()
        .resume()
        .unwrap()
        .bytes_done
}

/// `fresh` returns an empty store. `movie` must be an ID the store accepts.
pub async fn run<S, F, Fut>(fresh: F, movie: MovieId)
where
    S: NzbStore,
    F: Fn() -> Fut,
    Fut: Future<Output = S>,
{
    roundtrip(&fresh().await, movie).await;
    rejects(&fresh().await).await;
    checkpoints(&fresh().await).await;
    atomic_batches(&fresh().await).await;
    remove(&fresh().await, movie).await;
}

async fn roundtrip<S: NzbStore>(store: &S, movie: MovieId) {
    let (a, b) = (JobId::new(), JobId::new());
    assert!(store.load(a).await.unwrap().is_none());
    store.insert(a, document(), Some(movie)).await.unwrap();
    store.insert(b, document(), None).await.unwrap();
    let loaded = store.load(a).await.unwrap().unwrap();
    assert!(loaded.document == document());
    assert_eq!(loaded.movie_id, Some(movie));
    assert!(loaded.completed.is_empty());
    assert_eq!(store.load(b).await.unwrap().unwrap().movie_id, None);
    let plan = loaded.resume().unwrap();
    assert_eq!((plan.bytes_done, plan.bytes_total), (0, 100));
    assert_eq!(plan.remaining.len(), 4);
}

async fn rejects<S: NzbStore>(store: &S) {
    let id = JobId::new();
    store.insert(id, document(), None).await.unwrap();
    assert!(matches!(
        store.insert(id, document(), None).await,
        Err(StoreError::AlreadyExists)
    ));
    assert!(matches!(
        store
            .insert(JobId::new(), NzbDocument::default(), None)
            .await,
        Err(StoreError::InvalidDocument(_))
    ));
    let mut gap = document();
    gap.files[0].segments.remove(1);
    let gap_id = JobId::new();
    assert!(matches!(
        store.insert(gap_id, gap, None).await,
        Err(StoreError::InvalidDocument(_))
    ));
    assert!(store.load(gap_id).await.unwrap().is_none());
    assert!(matches!(
        store.checkpoint(JobId::new(), &[]).await,
        Err(StoreError::NotFound)
    ));
}

async fn checkpoints<S: NzbStore>(store: &S) {
    let (id, other) = (JobId::new(), JobId::new());
    store.insert(id, document(), None).await.unwrap();
    store.insert(other, document(), None).await.unwrap();
    store
        .checkpoint(
            id,
            &[(key(0, 1), true), (key(1, 1), true), (key(0, 1), true)],
        )
        .await
        .unwrap();
    assert_eq!(done(store, id).await, 50);
    assert_eq!(done(store, other).await, 0, "jobs are isolated");
    // Idempotent repeat, then last update wins within a batch.
    store.checkpoint(id, &[(key(0, 1), true)]).await.unwrap();
    store
        .checkpoint(
            id,
            &[
                (key(0, 3), true),
                (key(0, 3), false),
                (key(0, 2), false),
                (key(0, 2), true),
            ],
        )
        .await
        .unwrap();
    let plan = store.load(id).await.unwrap().unwrap().resume().unwrap();
    assert_eq!(plan.bytes_done, 70);
    assert_eq!(plan.remaining, vec![key(0, 3)]);
    // `false` clears a checkpoint after failed disk reconciliation.
    store.checkpoint(id, &[(key(1, 1), false)]).await.unwrap();
    assert_eq!(done(store, id).await, 30);
    store.checkpoint(id, &[]).await.unwrap();
    assert_eq!(done(store, id).await, 30);
}

async fn atomic_batches<S: NzbStore>(store: &S) {
    let id = JobId::new();
    store.insert(id, document(), None).await.unwrap();
    for bad in [
        key(0, 0),
        key(0, 4),
        key(2, 1),
        key(usize::MAX, 1),
        key(1, 2),
    ] {
        let err = store
            .checkpoint(id, &[(key(0, 1), true), (key(1, 1), true), (bad, true)])
            .await;
        assert!(matches!(err, Err(StoreError::UnknownSegment(k)) if k == bad));
        assert_eq!(done(store, id).await, 0, "rejected batch wrote nothing");
    }
    store.checkpoint(id, &[(key(0, 2), true)]).await.unwrap();
    assert!(
        store
            .checkpoint(id, &[(key(0, 2), false), (key(5, 1), false)])
            .await
            .is_err()
    );
    assert_eq!(done(store, id).await, 20, "rejected clear changed nothing");
}

async fn remove<S: NzbStore>(store: &S, movie: MovieId) {
    let (id, kept) = (JobId::new(), JobId::new());
    store.insert(id, document(), Some(movie)).await.unwrap();
    store.insert(kept, document(), None).await.unwrap();
    store.checkpoint(id, &[(key(0, 1), true)]).await.unwrap();
    store.checkpoint(kept, &[(key(0, 1), true)]).await.unwrap();
    store.remove(id).await.unwrap();
    assert!(store.load(id).await.unwrap().is_none());
    store.remove(id).await.unwrap();
    store.remove(JobId::new()).await.unwrap();
    assert!(matches!(
        store.checkpoint(id, &[(key(0, 1), true)]).await,
        Err(StoreError::NotFound)
    ));
    // The ID is free again, with no checkpoints left over.
    store.insert(id, document(), None).await.unwrap();
    let again = store.load(id).await.unwrap().unwrap();
    assert!(again.completed.is_empty());
    assert_eq!(again.movie_id, None);
    assert_eq!(done(store, kept).await, 10);
}
