use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use reelarr_core::*;
use reelarr_db::{Database, SqliteJobStore};
use sqlx::Row;

#[path = "../../reelarr-core/tests/support/import.rs"]
mod import;
#[path = "../../reelarr-core/tests/support/job_store.rs"]
mod job_store;

#[derive(Default)]
struct SqliteFactory {
    directories: Mutex<Vec<tempfile::TempDir>>,
}

#[async_trait]
impl job_store::StoreFactory for SqliteFactory {
    async fn create(&self, events: EventHub, interval: Duration) -> Arc<dyn JobStore> {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::connect(dir.path().join("reelarr.db"))
            .await
            .unwrap();
        let store = SqliteJobStore::with_options(&db, events, interval);
        self.directories.lock().unwrap().push(dir);
        Arc::new(store)
    }
}

#[tokio::test]
async fn sqlite_store_and_import_contracts_match_in_memory() {
    let factory = SqliteFactory::default();
    job_store::run(&factory).await;
    import::run(&factory).await;
}

async fn reach(store: &dyn JobStore, state: JobState) -> Job {
    use JobState::*;
    let job = store
        .insert(Job::new(JobKind::Nzb, "recovery fixture"))
        .await
        .unwrap();
    let route: &[JobState] = match state {
        Queued => &[],
        Downloading => &[Downloading],
        Paused => &[Downloading, Paused],
        Verifying => &[Downloading, Verifying],
        Repairing => &[Downloading, Verifying, Repairing],
        Extracting => &[Downloading, Verifying, Repairing, Extracting],
        ReadyForImport => &[
            Downloading,
            Verifying,
            Repairing,
            Extracting,
            ReadyForImport,
        ],
        Importing => &[
            Downloading,
            Verifying,
            Repairing,
            Extracting,
            ReadyForImport,
            Importing,
        ],
        Completed => &[
            Downloading,
            Verifying,
            Repairing,
            Extracting,
            ReadyForImport,
            Importing,
            Completed,
        ],
        Failed => &[Downloading, Verifying, Failed],
        Cancelled => &[Cancelled],
    };
    for to in route {
        let failure = (*to == Failed).then(|| Failure {
            code: FailureCode::MissingArticles,
            message: "missing".into(),
            retryable: true,
        });
        store.transition(job.id, *to, failure).await.unwrap();
    }
    store.get(job.id).await.unwrap()
}

#[tokio::test]
async fn every_durable_field_and_failure_code_round_trips_on_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reelarr.db");
    let db = Database::connect(&path).await.unwrap();
    let store = SqliteJobStore::new(&db);
    let mut new = Job::new(JobKind::ExternalDownload, "Quotes' and Unicode 雪");
    new.created_at = 123;
    new.category = Some("movies".into());
    new.priority = Priority::Force;
    new.queue_position = u64::MAX;
    let job = store.insert(new).await.unwrap();
    store
        .transition(job.id, JobState::Downloading, None)
        .await
        .unwrap();
    let paused = store
        .transition(job.id, JobState::Paused, None)
        .await
        .unwrap();
    let mut expected = vec![paused];
    for code in [
        FailureCode::MissingArticles,
        FailureCode::RepairFailed,
        FailureCode::ExtractFailed,
        FailureCode::PasswordRequired,
        FailureCode::DiskFull,
        FailureCode::ServerAuth,
        FailureCode::InvalidNzb,
        FailureCode::Duplicate,
        FailureCode::NoMatch,
        FailureCode::PathUnmapped,
        FailureCode::Other,
    ] {
        let job = reach(&store, JobState::Verifying).await;
        let failed = store
            .transition(
                job.id,
                JobState::Failed,
                Some(Failure {
                    code,
                    message: format!("failure {code:?}"),
                    retryable: code != FailureCode::NoMatch,
                }),
            )
            .await
            .unwrap();
        expected.push(failed);
    }
    drop(store);
    db.pool().close().await;
    drop(db);
    let db = Database::connect(&path).await.unwrap();
    let store = SqliteJobStore::new(&db);
    for mut before in expected {
        let after = store.get(before.id).await.unwrap();
        assert!(after.last_event_seq >= before.last_event_seq);
        before.last_event_seq = after.last_event_seq; // a reserved sequence lease is skipped on reopen
        assert_eq!(after, before);
    }
    let resumed = store.resume(job.id).await.unwrap();
    assert_eq!(resumed.state, JobState::Downloading);
    assert_eq!(resumed.attempts, 1);
    db.pool().close().await;
}

#[tokio::test]
async fn startup_recovery_reopens_every_state_and_checks_real_artifacts() {
    use JobState::*;
    let states = [
        Queued,
        Downloading,
        Paused,
        Verifying,
        Repairing,
        Extracting,
        ReadyForImport,
        Importing,
        Completed,
        Failed,
        Cancelled,
    ];
    for files_exist in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reelarr.db");
        let db = Database::connect(&path).await.unwrap();
        let store = SqliteJobStore::new(&db);
        let mut expected = Vec::new();
        for state in states {
            let job = reach(&store, state).await;
            let artifacts = dir.path().join(job.id.to_string());
            if files_exist {
                tokio::fs::create_dir(&artifacts).await.unwrap();
                tokio::fs::write(artifacts.join("part.bin"), "durable stage artifact")
                    .await
                    .unwrap();
            }
            store
                .set_recovery_path(job.id, Some(&artifacts))
                .await
                .unwrap();
            expected.push(job);
        }
        drop(store);
        db.pool().close().await;
        drop(db);
        let db = Database::connect(&path).await.unwrap();
        let store = SqliteJobStore::new(&db);
        store.recover_on_startup().await.unwrap();
        for mut before in expected {
            recover_after_restart(&mut before, files_exist);
            let after = store.get(before.id).await.unwrap();
            assert_eq!(after.state, before.state);
            assert_eq!(after.resume_state, before.resume_state);
            assert_eq!(after.resume_hint, before.resume_hint);
            assert_eq!(after.failure, before.failure);
            assert_eq!(after.attempts, before.attempts);
        }
        db.pool().close().await;
    }
}

#[tokio::test]
async fn unknown_empty_and_removed_artifacts_conservatively_requeue() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::connect(dir.path().join("reelarr.db"))
        .await
        .unwrap();
    let store = SqliteJobStore::new(&db);
    for state in [
        JobState::Verifying,
        JobState::Repairing,
        JobState::Extracting,
    ] {
        let unknown = reach(&store, state).await;
        let empty = reach(&store, state).await;
        let empty_path = dir.path().join(empty.id.to_string());
        tokio::fs::create_dir(&empty_path).await.unwrap();
        store
            .set_recovery_path(empty.id, Some(&empty_path))
            .await
            .unwrap();
        store.recover_on_startup().await.unwrap();
        assert_eq!(store.get(unknown.id).await.unwrap().state, JobState::Queued);
        assert_eq!(store.get(empty.id).await.unwrap().state, JobState::Queued);
    }
    db.pool().close().await;
}

#[tokio::test]
async fn illegal_transition_does_not_change_any_snapshot_or_sequence_columns() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::connect(dir.path().join("reelarr.db"))
        .await
        .unwrap();
    let store = SqliteJobStore::new(&db);
    let job = reach(&store, JobState::Verifying).await;
    let row_before = durable_markers(&db, job.id).await;
    assert!(matches!(
        store.transition(job.id, JobState::Downloading, None).await,
        Err(JobStoreError::Transition(_))
    ));
    assert_eq!(store.get(job.id).await.unwrap(), job);
    assert_eq!(durable_markers(&db, job.id).await, row_before);
    let mut receiver = store.events().subscribe();
    assert!(
        store
            .transition(job.id, JobState::Failed, None)
            .await
            .is_err()
    );
    assert!(receiver.try_recv().is_err());
    assert_eq!(store.get(job.id).await.unwrap(), job);
    db.pool().close().await;
}

async fn durable_markers(db: &Database, id: JobId) -> (Vec<u8>, Vec<u8>, i64) {
    let row = sqlx::query("SELECT event_seq, seq_reserved, updated_at FROM jobs WHERE id = ?")
        .bind(id.to_string())
        .fetch_one(db.pool())
        .await
        .unwrap();
    (
        row.get("event_seq"),
        row.get("seq_reserved"),
        row.get("updated_at"),
    )
}

#[tokio::test]
async fn racing_independent_connections_validate_inside_the_write_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reelarr.db");
    let db_one = Database::connect(&path).await.unwrap();
    let db_two = Database::connect(&path).await.unwrap();
    let one = SqliteJobStore::new(&db_one);
    let two = SqliteJobStore::new(&db_two);
    let job = reach(&one, JobState::Queued).await;
    let (a, b) = tokio::join!(
        one.transition(job.id, JobState::Downloading, None),
        two.transition(job.id, JobState::Downloading, None)
    );
    assert_ne!(a.is_ok(), b.is_ok());
    let error = if let Err(error) = a {
        error
    } else {
        b.unwrap_err()
    };
    assert!(matches!(
        error,
        JobStoreError::Transition(TransitionError::Illegal {
            from: JobState::Downloading,
            to: JobState::Downloading
        })
    ));
    assert_eq!(one.get(job.id).await.unwrap().attempts, 1);
    db_one.pool().close().await;
    db_two.pool().close().await;
}

#[tokio::test]
async fn progress_stays_volatile_and_sequence_leases_prevent_reuse_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reelarr.db");
    let db = Database::connect(&path).await.unwrap();
    let store = SqliteJobStore::with_options(&db, EventHub::default(), Duration::ZERO);
    let job = store
        .insert(Job::new(JobKind::Nzb, "progress"))
        .await
        .unwrap();
    let first = store
        .update_progress(
            job.id,
            Progress {
                bytes_done: 1,
                ..Progress::default()
            },
        )
        .await
        .unwrap();
    let before = durable_markers(&db, job.id).await;
    let mut last = first.last_event_seq;
    for bytes in 2..=100 {
        let updated = store
            .update_progress(
                job.id,
                Progress {
                    bytes_done: bytes,
                    ..Progress::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.last_event_seq, last + 1);
        last = updated.last_event_seq;
    }
    assert_eq!(durable_markers(&db, job.id).await, before); // no write per update OR event
    let columns = sqlx::query("PRAGMA table_info(jobs)")
        .fetch_all(db.pool())
        .await
        .unwrap();
    assert!(
        columns
            .iter()
            .all(|r| !r.get::<String, _>("name").starts_with("bytes_"))
    );
    assert_eq!(store.get(job.id).await.unwrap().progress.bytes_done, 100);
    // Cross multiple lease boundaries without persisting progress values.
    for bytes in 101..=600 {
        let updated = store
            .update_progress(
                job.id,
                Progress {
                    bytes_done: bytes,
                    ..Progress::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.last_event_seq, last + 1);
        last = updated.last_event_seq;
    }
    assert_eq!(durable_markers(&db, job.id).await.0, before.0);
    // Constructors using one Database must share the live watermark/cache/hub.
    let same = SqliteJobStore::new(&db);
    assert_eq!(same.get(job.id).await.unwrap().progress.bytes_done, 600);
    drop(same);
    drop(store);
    db.pool().close().await;
    drop(db);
    let db = Database::connect(&path).await.unwrap();
    let store = SqliteJobStore::new(&db);
    let snapshot = store.get(job.id).await.unwrap();
    assert_eq!(snapshot.progress, Progress::default());
    assert!(snapshot.last_event_seq >= last);
    assert!(
        store
            .message(job.id, "after restart".into())
            .await
            .unwrap()
            .last_event_seq
            > last
    );
    db.pool().close().await;
}

#[tokio::test]
async fn unsigned_queue_ordering_and_sequence_exhaustion_do_not_wrap() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::connect(dir.path().join("reelarr.db"))
        .await
        .unwrap();
    let store = SqliteJobStore::new(&db);
    let mut expected = Vec::new();
    for position in [u64::MAX, 0, i64::MAX as u64, i64::MAX as u64 + 1] {
        let mut job = Job::new(JobKind::Nzb, "unsigned");
        job.queue_position = position;
        expected.push((position, store.insert(job).await.unwrap().id));
    }
    expected.sort();
    assert_eq!(
        store
            .list()
            .await
            .unwrap()
            .iter()
            .map(|j| (j.queue_position, j.id))
            .collect::<Vec<_>>(),
        expected
    );
    let id = expected[0].1;
    sqlx::query("UPDATE jobs SET event_seq = ?, seq_reserved = ? WHERE id = ?")
        .bind(u64::MAX.to_be_bytes().to_vec())
        .bind(u64::MAX.to_be_bytes().to_vec())
        .bind(id.to_string())
        .execute(db.pool())
        .await
        .unwrap();
    assert!(matches!(
        store.transition(id, JobState::Downloading, None).await,
        Err(JobStoreError::SequenceExhausted(_))
    ));
    assert_eq!(store.get(id).await.unwrap().state, JobState::Queued);
    db.pool().close().await;
}
