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
