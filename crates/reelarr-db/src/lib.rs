//! The application's single SQLite database and forward-only migrations.

use std::{
    path::Path,
    sync::{Arc, OnceLock},
    time::Duration,
};

use sqlx::{
    ConnectOptions, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};

mod indexers;
pub use indexers::{IndexerSummary, SqliteIndexerStore};

pub mod secrets;

mod jobs;
pub use jobs::SqliteJobStore;

mod nzbs;
pub use nzbs::SqliteNzbStore;

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

/// True for SQLite in-memory paths/URLs. Each connection to these is its own database,
/// so the pool must stay at a single never-recycled connection.
fn is_in_memory_sqlite(path: &Path) -> bool {
    let text = path.to_string_lossy();
    if text == ":memory:" || text == "sqlite::memory:" {
        return true;
    }
    text.to_ascii_lowercase().contains("mode=memory")
}

impl Database {
    /// Opens/creates a database file and applies all embedded migrations.
    /// The parent directory must exist. A migration failure returns no usable handle.
    ///
    /// In-memory paths (`:memory:`, `sqlite::memory:`, or any path/URL containing
    /// `mode=memory`) use a single-connection pool that is never dropped for idle
    /// lifetime, so migrations remain visible to every query.
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, DatabaseError> {
        let path = path.as_ref();
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5))
            .disable_statement_logging();
        let pool_options = if is_in_memory_sqlite(path) {
            SqlitePoolOptions::new()
                .max_connections(1)
                .min_connections(1)
                .idle_timeout(None)
                .max_lifetime(None)
        } else {
            SqlitePoolOptions::new().max_connections(5)
        };
        let pool = pool_options.connect_with(options).await?;
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

#[cfg(test)]
mod tests {
    use super::is_in_memory_sqlite;
    use std::path::Path;

    #[test]
    fn detects_in_memory_sqlite_paths() {
        assert!(is_in_memory_sqlite(Path::new(":memory:")));
        assert!(is_in_memory_sqlite(Path::new("sqlite::memory:")));
        assert!(is_in_memory_sqlite(Path::new(
            "file:memdb1?mode=memory&cache=shared"
        )));
        assert!(is_in_memory_sqlite(Path::new(
            "file:memdb1?MODE=MEMORY&cache=shared"
        )));
        assert!(!is_in_memory_sqlite(Path::new("reelarr.db")));
        assert!(!is_in_memory_sqlite(Path::new("/tmp/reelarr.db")));
    }
}
