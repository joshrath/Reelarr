//! Server composition boundary shared with the UI integration.
pub mod api;
pub mod auth;
pub mod state;
pub mod stream;

use axum::{Json, Router, routing::get};
use leptos::config::{LeptosOptions, get_configuration};
use reelarr_types::Health;
use state::AppState;

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(|| async { Json(Health::ok()) }))
        .nest("/api/v1", api::router(state.clone()))
        .with_state(state)
}

/// The API plus the web UI: SSR pages, with `/pkg/*` assets as the fallback.
pub fn app_with_ui(state: AppState, leptos_options: LeptosOptions) -> Router {
    app(state).merge(reelarr_ui::router(leptos_options))
}

/// Leptos settings from the `LEPTOS_*` env vars that `cargo leptos` sets, with
/// defaults so a plain `cargo run` still finds the site in `target/site`.
pub fn leptos_options() -> LeptosOptions {
    let mut options = get_configuration(None)
        .map(|conf| conf.leptos_options)
        .unwrap_or_else(|_| LeptosOptions::builder().output_name("reelarr").build());
    if options.output_name.is_empty() {
        options.output_name = "reelarr".into();
    }
    if std::env::var_os("LEPTOS_SITE_ROOT").is_none() {
        options.site_root = "target/site".into();
    }
    options
}
