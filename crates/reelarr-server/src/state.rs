use reelarr_core::auth::{AuthService, InMemoryAuthService};
use reelarr_stream::{InMemoryMediaLookup, MediaLookup};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub auth: Arc<dyn AuthService>,
    /// Finds media files for playback.
    pub media: Arc<dyn MediaLookup>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            auth: Arc::new(InMemoryAuthService::default()),
            media: Arc::new(InMemoryMediaLookup::new()),
        }
    }
}
