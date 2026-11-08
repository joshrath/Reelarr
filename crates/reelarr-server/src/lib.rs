//! Server composition boundary shared with the UI integration.
pub mod auth;
pub mod state;
pub mod stream;

use axum::{Json, Router, routing::get};
use reelarr_types::Health;
use state::AppState;

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(|| async { Json(Health::ok()) }))
        .nest("/api/v1", auth::router().merge(stream::router()))
        .with_state(state)
}
