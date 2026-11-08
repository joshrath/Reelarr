use reelarr_db::Database;

/// Shared application state. Database clones share one pool.
#[derive(Clone)]
pub struct AppState {
    pub db: Database,
}
