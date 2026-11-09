//! Server composition boundary shared with the UI integration.
pub mod api;
pub mod auth;
pub mod library;
pub mod nntp;
pub mod state;
pub mod stream;

use std::path::Path;

use axum::{Json, Router, routing::get};
use leptos::config::{Env, LeptosOptions, get_configuration};
use reelarr_types::Health;
use state::AppState;

/// `/health` and the JSON API, before state is attached.
fn api_routes(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/health", get(|| async { Json(Health::ok()) }))
        .nest("/api/v1", api::router(state.clone()))
}

/// The API without the web UI.
pub fn app(state: AppState) -> Router {
    api_routes(&state).with_state(state)
}

/// The API plus the web UI: SSR pages, with `/pkg/*` assets as the fallback.
pub fn app_with_ui(state: AppState, leptos_options: LeptosOptions) -> Router {
    api_routes(&state)
        .merge(reelarr_ui::router::<AppState>(leptos_options))
        .with_state(state)
}

/// Loads Leptos settings.
///
/// When any `LEPTOS_*` variable is set (as `cargo leptos` and the Docker image
/// do), they are the source of truth and a bad value is an error. With none
/// set, defaults let a plain `cargo run` find the site in `target/site`.
/// The listening port always comes from `REELARR_PORT`; `LEPTOS_SITE_ADDR` is
/// ignored.
pub fn leptos_options() -> Result<LeptosOptions, String> {
    let configured = std::env::vars_os().any(|(k, _)| k.to_string_lossy().starts_with("LEPTOS_"));
    let mut options = if configured {
        get_configuration(None)
            .map(|conf| conf.leptos_options)
            .map_err(|e| format!("invalid LEPTOS_* configuration: {e}"))?
    } else {
        LeptosOptions::builder()
            .output_name("reelarr")
            .site_root("target/site")
            .build()
    };
    apply_defaults(&mut options, std::env::var_os("LEPTOS_SITE_ROOT").is_some());
    Ok(options)
}

/// Fills in what a partial `LEPTOS_*` configuration leaves out, such as when
/// only `LEPTOS_ENV` is set.
fn apply_defaults(options: &mut LeptosOptions, site_root_set: bool) {
    if options.output_name.is_empty() {
        options.output_name = "reelarr".into();
    }
    if !site_root_set {
        options.site_root = "target/site".into();
    }
}

/// Logs where the UI is served from and checks that the compiled WebAssembly
/// exists. In production a missing bundle is an error; in development it is a
/// warning, since `cargo run` without `cargo leptos build` is still useful
/// for API work.
pub fn check_site(options: &LeptosOptions) -> Result<(), String> {
    let wasm = Path::new(options.site_root.as_ref())
        .join(options.site_pkg_dir.as_ref())
        .join(format!("{}.wasm", options.output_name));
    tracing::info!(site_root = %options.site_root, env = ?options.env, "serving web UI");
    if wasm.is_file() {
        return Ok(());
    }
    let message = format!(
        "web UI bundle not found at {}; run `cargo leptos build`",
        wasm.display()
    );
    match options.env {
        Env::PROD => Err(message),
        Env::DEV => {
            tracing::warn!("{message}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_config_keeps_default_site_root() {
        let mut options = LeptosOptions::builder()
            .output_name("")
            .site_root("elsewhere")
            .build();
        apply_defaults(&mut options, false);
        assert_eq!(options.output_name.as_ref(), "reelarr");
        assert_eq!(options.site_root.as_ref(), "target/site");

        let mut options = LeptosOptions::builder()
            .output_name("reelarr")
            .site_root("/usr/share/reelarr/site")
            .build();
        apply_defaults(&mut options, true);
        assert_eq!(options.site_root.as_ref(), "/usr/share/reelarr/site");
    }

    #[test]
    fn missing_bundle_is_fatal_only_in_prod() {
        let mut options = LeptosOptions::builder()
            .output_name("reelarr")
            .site_root("/nonexistent")
            .build();
        options.env = Env::DEV;
        assert!(check_site(&options).is_ok());
        options.env = Env::PROD;
        assert!(check_site(&options).is_err());
    }
}
