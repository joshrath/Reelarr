use reelarr_core::SettingsStore;
use reelarr_db::{Database, DatabaseError, SqliteSettingsStore};
use sqlx::{
    Row,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

#[tokio::test]
async fn migrations_run_and_rerun_without_losing_settings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reelarr.db");
    let db = Database::connect(&path).await.unwrap();
    let store = SqliteSettingsStore::new(&db);
    assert!(store.get("theme").await.unwrap().is_none());
    store.set("theme", "dark").await.unwrap();
    db.pool().close().await;

    let db = Database::connect(&path).await.unwrap();
    let row = sqlx::query("SELECT COUNT(*) AS count FROM _sqlx_migrations WHERE success = 1")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(row.get::<i64, _>("count"), 1);
    let store = SqliteSettingsStore::new(&db);
    let first = store.get("theme").await.unwrap().unwrap();
    assert_eq!(first.key, "theme");
    assert_eq!(first.value, "dark");
    assert!(first.updated_at > 0);
    // Force an old timestamp so the update assertion is meaningful without sleeping.
    sqlx::query("UPDATE settings SET updated_at = 1 WHERE key = ?")
        .bind("theme")
        .execute(db.pool())
        .await
        .unwrap();
    store.set("theme", "light").await.unwrap();
    let second = store.get("theme").await.unwrap().unwrap();
    assert_eq!(second.value, "light");
    assert!(second.updated_at > 1);
    store
        .set("quote'key", "雪\n'\"; DROP TABLE settings; --")
        .await
        .unwrap();
    assert_eq!(
        store.get("quote'key").await.unwrap().unwrap().value,
        "雪\n'\"; DROP TABLE settings; --"
    );
    db.pool().close().await;
}

#[tokio::test]
async fn pool_connections_use_wal_foreign_keys_and_busy_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::connect(dir.path().join("reelarr.db"))
        .await
        .unwrap();
    let mut connections = Vec::new();
    for _ in 0..5 {
        let mut conn = db.pool().acquire().await.unwrap();
        let row = sqlx::query("PRAGMA foreign_keys")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(row.get::<i64, _>(0), 1);
        let row = sqlx::query("PRAGMA journal_mode")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(row.get::<String, _>(0), "wal");
        let row = sqlx::query("PRAGMA busy_timeout")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(row.get::<i64, _>(0), 5000);
        connections.push(conn);
    }
    drop(connections);
    sqlx::query("CREATE TABLE parent (id INTEGER PRIMARY KEY)")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("CREATE TABLE child (parent_id INTEGER REFERENCES parent(id))")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(
        sqlx::query("INSERT INTO child VALUES (123)")
            .execute(db.pool())
            .await
            .is_err()
    );
    db.pool().close().await;
}

#[tokio::test]
async fn migration_failure_prevents_a_database_handle() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reelarr.db");
    let pool = SqlitePoolOptions::new()
        .connect_with(
            SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true),
        )
        .await
        .unwrap();
    // An unmanaged, conflicting table must not be silently accepted.
    sqlx::query("CREATE TABLE settings (wrong_column TEXT)")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    assert!(matches!(
        Database::connect(&path).await,
        Err(DatabaseError::Migration(_))
    ));
}
