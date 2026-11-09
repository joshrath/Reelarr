//! The application's single SQLite database and forward-only migrations.

use std::{
    path::Path,
    sync::{Arc, OnceLock},
    time::Duration,
};

use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};

mod jobs;
pub use jobs::SqliteJobStore;

mod settings;
pub use settings::SqliteSettingsStore;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

#[derive(Debug, thiserror::Error)]
pub enum DatabaseError {
    #[error("failed to open SQLite database: {0}")]
    Connect(#[from] sqlx::Error),
    #[error("database migration failed: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
}

/// Cheaply cloned handle to the single shared pool.
#[derive(Clone, Debug)]
pub struct Database {
    pool: SqlitePool,
    jobs: Arc<OnceLock<Arc<jobs::JobRuntime>>>,
}

impl Database {
    /// Opens/creates a database file and applies all embedded migrations.
    /// The parent directory must exist. A migration failure returns no usable handle.
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, DatabaseError> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;
        if let Err(error) = MIGRATOR.run(&pool).await {
            pool.close().await;
            return Err(error.into());
        }
        Ok(Self {
            pool,
            jobs: Arc::new(OnceLock::new()),
        })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}
