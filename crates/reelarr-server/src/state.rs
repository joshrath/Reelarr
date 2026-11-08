use reelarr_core::auth::{AuthService, InMemoryAuthService};
use reelarr_db::{Database, SqliteJobStore};
use std::sync::Arc;

/// Shared application state; clones share the database pool, durable job store and auth service.
#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub jobs: Arc<SqliteJobStore>,
    pub auth: Arc<dyn AuthService>,
}

impl AppState {
    pub fn new(db: Database) -> Self {
        Self {
            jobs: Arc::new(SqliteJobStore::new(&db)),
            db,
            auth: Arc::new(InMemoryAuthService::default()),
        }
    }
}
