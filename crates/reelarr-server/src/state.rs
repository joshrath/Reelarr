use reelarr_core::auth::{AuthService, InMemoryAuthService};
use reelarr_db::Database;
use std::sync::Arc;

/// Shared application state; clones share the database pool and auth service.
#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub auth: Arc<dyn AuthService>,
}

impl AppState {
    pub fn new(db: Database) -> Self {
        Self {
            db,
            auth: Arc::new(InMemoryAuthService::default()),
        }
    }
}
