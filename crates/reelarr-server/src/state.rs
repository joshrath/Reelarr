use reelarr_core::auth::{AuthService, InMemoryAuthService};
use reelarr_db::{Database, SqliteJobStore};
use reelarr_media::db::{DbMediaLookup, SqliteLibraryRootStore};
use reelarr_media::health::HealthChecker;
use reelarr_stream::MediaLookup;
use std::sync::Arc;

/// Shared application state; clones share database, jobs, authentication and media lookup services.
#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub jobs: Arc<SqliteJobStore>,
    pub auth: Arc<dyn AuthService>,
    /// Finds media files for playback.
    pub media: Arc<dyn MediaLookup>,
    /// Library folders and their last health check.
    pub library_roots: SqliteLibraryRootStore,
    pub root_health: HealthChecker,
}

impl AppState {
    pub fn new(db: Database) -> Self {
        Self {
            jobs: Arc::new(SqliteJobStore::new(&db)),
            auth: Arc::new(InMemoryAuthService::default()),
            media: Arc::new(DbMediaLookup::new(&db)),
            library_roots: SqliteLibraryRootStore::new(&db),
            root_health: HealthChecker::default(),
            db,
        }
    }
}
