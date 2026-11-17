use std::sync::Mutex;

use reelarr_core::{Job, JobKind, JobStore};
use reelarr_db::{Database, SqliteJobStore, SqliteNzbStore};
use reelarr_nzb::{JobId, NzbStore, StoreError};
use reelarr_types::MovieId;
use sqlx::Row;

#[path = "../../reelarr-nzb/tests/support/store_contract.rs"]
mod store_contract;
use store_contract::{document, key};

const MOVIE: MovieId = MovieId(42);

async fn file_db(dir: &tempfile::TempDir) -> Database {
    Database::connect(dir.path().join("reelarr.db"))
        .await
        .unwrap()
}

#[tokio::test]
async fn sqlite_store_meets_shared_contract() {
    let dirs = Mutex::new(Vec::new());
    store_contract::run(
        || async {
            let dir = tempfile::tempdir().unwrap();
            let store = SqliteNzbStore::open(&file_db(&dir).await).await.unwrap();
            dirs.lock().unwrap().push(dir);
            store
        },
        MOVIE,
    )
    .await;
}

#[tokio::test]
async fn in_memory_database_meets_shared_contract() {
    store_contract::run(
        || async {
            let db = Database::connect(":memory:").await.unwrap();
            SqliteNzbStore::new(&db)
        },
        MOVIE,
    )
    .await;
}

/// Admission as the worker does it: NZB first, then the job row.
async fn admit(db: &Database, movie: Option<MovieId>) -> JobId {
    let job = Job::new(JobKind::Nzb, "Example".to_owned());
    let id = job.id;
    SqliteNzbStore::new(db)
        .insert(id, document(), movie)
        .await
        .unwrap();
    SqliteJobStore::new(db).insert(job).await.unwrap();
    id
}

#[tokio::test]
async fn persists_document_checkpoints_and_movie_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let (with_movie, without) = {
        let db = file_db(&dir).await;
        let a = admit(&db, Some(MOVIE)).await;
        let b = admit(&db, None).await;
        let store = SqliteNzbStore::open(&db).await.unwrap();
        store
            .checkpoint(
                a,
                &[(key(0, 1), true), (key(1, 1), true), (key(0, 3), true)],
            )
            .await
            .unwrap();
        store.checkpoint(a, &[(key(0, 3), false)]).await.unwrap();
        db.pool().close().await;
        (a, b)
    };
    let db = file_db(&dir).await;
    let store = SqliteNzbStore::open(&db).await.unwrap();
    let loaded = store.load(with_movie).await.unwrap().unwrap();
    assert!(
        loaded.document == document(),
        "document survives byte-for-byte"
    );
    assert_eq!(loaded.movie_id, Some(MOVIE));
    let plan = loaded.resume().unwrap();
    assert_eq!(plan.bytes_done, 50);
    assert_eq!(plan.remaining, vec![key(0, 2), key(0, 3)]);
    let other = store.load(without).await.unwrap().unwrap();
    assert_eq!(other.movie_id, None);
    assert!(other.completed.is_empty());
}

#[tokio::test]
async fn reopen_removes_orphaned_admissions_only() {
    let dir = tempfile::tempdir().unwrap();
    let db = file_db(&dir).await;
    let admitted = admit(&db, Some(MOVIE)).await;
    // Crash between the NZB insert and the job insert: no job row.
    let orphan = JobId::new();
    let store = SqliteNzbStore::new(&db);
    store.insert(orphan, document(), Some(MOVIE)).await.unwrap();
    store
        .checkpoint(orphan, &[(key(0, 1), true)])
        .await
        .unwrap();
    db.pool().close().await;

    let db = file_db(&dir).await;
    let store = SqliteNzbStore::new(&db);
    assert_eq!(store.reconcile().await.unwrap(), 1);
    assert!(store.load(orphan).await.unwrap().is_none());
    assert!(store.load(admitted).await.unwrap().is_some());
    assert_eq!(
        store.reconcile().await.unwrap(),
        0,
        "reconcile is idempotent"
    );
    let leftover: i64 = sqlx::query_scalar("SELECT count(*) FROM nzb_checkpoints WHERE job_id = ?")
        .bind(orphan.to_string())
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(leftover, 0);
}

#[tokio::test]
async fn deleting_a_job_cascades_to_its_nzb_and_checkpoints() {
    let dir = tempfile::tempdir().unwrap();
    let db = file_db(&dir).await;
    let (gone, kept) = (admit(&db, None).await, admit(&db, None).await);
    let store = SqliteNzbStore::open(&db).await.unwrap();
    for id in [gone, kept] {
        store
            .checkpoint(id, &[(key(0, 1), true), (key(0, 2), true)])
            .await
            .unwrap();
    }
    sqlx::query("DELETE FROM jobs WHERE id = ?")
        .bind(gone.to_string())
        .execute(db.pool())
        .await
        .unwrap();
    assert!(store.load(gone).await.unwrap().is_none());
    assert!(matches!(
        store.checkpoint(gone, &[(key(0, 1), true)]).await,
        Err(StoreError::NotFound)
    ));
    let counts = sqlx::query("SELECT job_id, count(*) AS n FROM nzb_checkpoints GROUP BY job_id")
        .fetch_all(db.pool())
        .await
        .unwrap();
    assert_eq!(counts.len(), 1);
    assert_eq!(counts[0].get::<String, _>("job_id"), kept.to_string());
    assert_eq!(counts[0].get::<i64, _>("n"), 2);
    // remove() is idempotent and leaves the job row alone.
    store.remove(kept).await.unwrap();
    store.remove(kept).await.unwrap();
    assert!(store.load(kept).await.unwrap().is_none());
    SqliteJobStore::new(&db).get(kept).await.unwrap();
}

#[tokio::test]
async fn checkpoints_require_their_document() {
    let db = Database::connect(":memory:").await.unwrap();
    let id = JobId::new();
    let bad =
        sqlx::query("INSERT INTO nzb_checkpoints (job_id, file_index, number) VALUES (?, 0, 1)")
            .bind(id.to_string())
            .execute(db.pool())
            .await;
    assert!(bad.is_err(), "FOREIGN KEY to nzb_documents is enforced");
}

#[tokio::test]
async fn concurrent_checkpoint_batches_are_each_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let db = file_db(&dir).await;
    let id = admit(&db, None).await;
    let store = SqliteNzbStore::open(&db).await.unwrap();
    let mut tasks = Vec::new();
    for round in 0..16u32 {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            let complete = round % 2 == 0;
            let batch = [
                (key(0, 1), complete),
                (key(0, 2), complete),
                (key(0, 3), complete),
            ];
            store.checkpoint(id, &batch).await.unwrap();
            // Every committed snapshot holds the whole batch or none of it.
            let seen = store.load(id).await.unwrap().unwrap().completed;
            assert!(
                [0, 3].contains(&seen.iter().filter(|k| k.file_index == 0).count()),
                "partial batch visible: {seen:?}"
            );
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
}

#[tokio::test]
async fn movie_id_reads_the_grab_without_the_document() {
    let db = Database::connect(":memory:").await.unwrap();
    let store = SqliteNzbStore::new(&db);
    let grabbed = admit(&db, Some(MOVIE)).await;
    let manual = admit(&db, None).await;
    assert_eq!(store.movie_id(grabbed).await.unwrap(), Some(Some(MOVIE)));
    assert_eq!(store.movie_id(manual).await.unwrap(), Some(None));
    assert_eq!(store.movie_id(JobId::new()).await.unwrap(), None);
}
