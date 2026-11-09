use std::{net::TcpListener, process::Command};

use reelarr_db::Database;

#[tokio::test]
async fn invalid_migration_stops_startup_before_binding() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::connect(dir.path().join("reelarr.db"))
        .await
        .unwrap();
    // Simulate a previously applied migration that no longer matches the binary.
    sqlx::query("UPDATE _sqlx_migrations SET checksum = X'00'")
        .execute(db.pool())
        .await
        .unwrap();
    db.pool().close().await;

    // Keeping this port occupied distinguishes migration failure from bind failure.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_reelarr"))
        .env("REELARR_DATA_DIR", dir.path())
        .env(
            "REELARR_PORT",
            listener.local_addr().unwrap().port().to_string(),
        )
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Migration"), "{stderr}");
    assert!(!stderr.contains("failed to bind port"), "{stderr}");
}

#[tokio::test]
async fn invalid_job_snapshot_stops_recovery_before_binding() {
    use reelarr_core::{Job, JobKind, JobStore};
    use reelarr_db::SqliteJobStore;
    let dir = tempfile::tempdir().unwrap();
    let db = Database::connect(dir.path().join("reelarr.db"))
        .await
        .unwrap();
    let jobs = SqliteJobStore::new(&db);
    jobs.insert(Job::new(JobKind::Nzb, "corrupt recovery fixture"))
        .await
        .unwrap();
    sqlx::query("UPDATE jobs SET id = 'not-a-uuid'")
        .execute(db.pool())
        .await
        .unwrap();
    db.pool().close().await;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_reelarr"))
        .env("REELARR_DATA_DIR", dir.path())
        .env(
            "REELARR_PORT",
            listener.local_addr().unwrap().port().to_string(),
        )
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("InvalidJobId"), "{stderr}");
    assert!(!stderr.contains("failed to bind port"), "{stderr}");
}

#[tokio::test]
async fn invalid_secret_key_stops_startup_before_binding() {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_reelarr"))
        .env("REELARR_DATA_DIR", dir.path())
        .env("REELARR_SECRET_KEY", "not-base64-and-not-32-bytes")
        .env(
            "REELARR_PORT",
            listener.local_addr().unwrap().port().to_string(),
        )
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("REELARR_SECRET_KEY"), "{stderr}");
    assert!(!stderr.contains("not-base64-and-not-32-bytes"), "{stderr}");
    assert!(!stderr.contains("failed to bind port"), "{stderr}");
}
