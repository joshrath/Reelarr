use axum::Router;

use crate::state::AppState;

/// Versioned JSON API; feature modules attach their routes here.
pub fn router() -> Router<AppState> {
    Router::new()
}
