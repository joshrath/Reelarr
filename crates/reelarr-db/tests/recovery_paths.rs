//! `SqliteRecoveryPaths`: the NZB worker's `RecoveryPaths` over the job row.
use std::path::{Path, PathBuf};

use reelarr_core::{Job, JobKind, JobState, JobStore, JobStoreError};
use reelarr_db::{Database, SqliteJobStore, SqliteRecoveryPaths};
use reelarr_nzb::worker::RecoveryPaths;

async fn verifying(jobs: &SqliteJobStore) -> Job {
    let job = jobs
        .insert(Job::new(JobKind::Nzb, "fixture"))
        .await
        .unwrap();
    for to in [JobState::Downloading, JobState::Verifying] {
        jobs.transition(job.id, to, None).await.unwrap();
    }
    job
}

#[tokio::test]
async fn recorded_path_survives_reopen_and_drives_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let artifacts = dir.path().join("staging").join("job");
    std::fs::create_dir_all(&artifacts).unwrap();
    std::fs::write(artifacts.join("movie.mkv"), b"data").unwrap();
    let db_path = dir.path().join("reelarr.db");

    let (kept, requeued) = {
        let db = Database::connect(&db_path).await.unwrap();
        let jobs = SqliteJobStore::new(&db);
        let paths = SqliteRecoveryPaths::new(jobs.clone());
        let kept = verifying(&jobs).await;
        let requeued = verifying(&jobs).await;
        paths
            .set_recovery_path(kept.id, Some(&artifacts))
            .await
            .unwrap();
        // Set, then clear: a cleared path must not count as artifacts.
        paths
            .set_recovery_path(requeued.id, Some(&artifacts))
            .await
            .unwrap();
        paths.set_recovery_path(requeued.id, None).await.unwrap();
        db.pool().close().await;
        (kept.id, requeued.id)
    };

    let db = Database::connect(&db_path).await.unwrap();
    let jobs = SqliteJobStore::new(&db);
    assert_eq!(jobs.recovery_path(kept).await.unwrap(), Some(artifacts));
    assert_eq!(jobs.recovery_path(requeued).await.unwrap(), None);
    jobs.recover_on_startup().await.unwrap();
    assert_eq!(jobs.get(kept).await.unwrap().state, JobState::Verifying);
    assert_eq!(jobs.get(requeued).await.unwrap().state, JobState::Queued);
}

#[tokio::test]
async fn rejects_relative_paths_and_unknown_jobs() {
    let db = Database::connect(":memory:").await.unwrap();
    let jobs = SqliteJobStore::new(&db);
    let paths = SqliteRecoveryPaths::new(jobs.clone());
    let job = verifying(&jobs).await;

    let error = paths
        .set_recovery_path(job.id, Some(Path::new("relative/dir")))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("absolute"), "{error}");
    assert_eq!(jobs.recovery_path(job.id).await.unwrap(), None);

    let unknown = Job::new(JobKind::Nzb, "never inserted").id;
    let error = paths
        .set_recovery_path(unknown, Some(&PathBuf::from("/tmp/x")))
        .await
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<JobStoreError>(),
        Some(JobStoreError::NotFound(id)) if *id == unknown
    ));
    assert!(matches!(
        jobs.recovery_path(unknown).await,
        Err(JobStoreError::NotFound(_))
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_non_utf8_paths() {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
    let db = Database::connect(":memory:").await.unwrap();
    let jobs = SqliteJobStore::new(&db);
    let paths = SqliteRecoveryPaths::new(jobs.clone());
    let job = verifying(&jobs).await;
    let path = Path::new(OsStr::from_bytes(b"/tmp/\xff"));
    let error = paths
        .set_recovery_path(job.id, Some(path))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("UTF-8"), "{error}");
}
