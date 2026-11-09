//! Server-side integration with axum (`ssr` feature).

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderValue, Request, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use leptos::prelude::*;
use leptos_axum::{LeptosRoutes, file_and_error_handler, generate_route_list};

use crate::app::{App, shell};

/// Routes for the web UI, ready for `reelarr-server` to merge.
///
/// Renders every UI route with SSR and serves the compiled site assets
/// (`/pkg/*`) as the fallback. Paths under `/api` never fall through to HTML:
/// unknown API paths get a JSON 404, so the UI fallback can't mask missing
/// endpoints. Every response gets the headers from [`security_headers`].
pub fn router<S>(options: LeptosOptions) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let routes = generate_route_list(App);
    Router::new()
        .leptos_routes(&options, routes, {
            let options = options.clone();
            move || shell(options.clone())
        })
        .fallback(fallback)
        .layer(middleware::from_fn(security_headers))
        .with_state(options)
}

/// Caching and hardening headers for UI responses.
///
/// - SSR pages and errors: `Cache-Control: no-store`, since pages may carry
///   per-session data once sign-in is live.
/// - `/pkg/*` and `/fonts/*` assets: `Cache-Control: no-cache`, so browsers revalidate. Asset
///   file names are not content-hashed yet; switch to `immutable` if they are.
/// - Always: `nosniff`, `Referrer-Policy: same-origin`, and framing denied via
///   CSP `frame-ancestors 'none'` plus `X-Frame-Options: DENY`.
async fn security_headers(req: Request<Body>, next: Next) -> Response {
    let path = req.uri().path();
    let is_asset = path.starts_with("/pkg/") || path.starts_with("/fonts/");
    let mut res = next.run(req).await;
    let headers = res.headers_mut();
    let cache = if is_asset { "no-cache" } else { "no-store" };
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("frame-ancestors 'none'"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    res
}

async fn fallback(state: State<LeptosOptions>, uri: Uri, req: Request<Body>) -> Response {
    if is_api_path(uri.path()) {
        return (
            StatusCode::NOT_FOUND,
            Json(ApiErrorBody {
                error: "not_found",
                message: "No such API endpoint.",
            }),
        )
            .into_response();
    }
    file_and_error_handler(shell)(uri, state, req).await
}

fn is_api_path(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/")
}

#[derive(serde::Serialize)]
struct ApiErrorBody {
    error: &'static str,
    message: &'static str,
}

#[cfg(test)]
mod tests {
    use super::is_api_path;

    #[test]
    fn api_paths_are_detected() {
        assert!(is_api_path("/api"));
        assert!(is_api_path("/api/v1/nope"));
        assert!(!is_api_path("/apiary"));
        assert!(!is_api_path("/"));
    }
}
