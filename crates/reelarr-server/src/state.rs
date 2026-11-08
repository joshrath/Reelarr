use reelarr_core::auth::{AuthService, InMemoryAuthService};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub auth: Arc<dyn AuthService>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            auth: Arc::new(InMemoryAuthService::default()),
        }
    }
}
