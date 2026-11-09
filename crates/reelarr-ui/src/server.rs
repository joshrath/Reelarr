//! Server-side integration with axum (`ssr` feature).

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode, Uri},
    response::{IntoResponse, Redirect, Response},
    routing::get,
};
use leptos::prelude::*;
use leptos_axum::{LeptosRoutes, file_and_error_handler, generate_route_list};

use crate::app::{App, REDIRECTS, shell};

/// Routes for the web UI, ready for `reelarr-server` to merge.
///
/// Renders every UI route with SSR and serves the compiled site assets
/// (`/pkg/*`) as the fallback. Paths under `/api` never fall through to HTML:
/// unknown API paths get a JSON 404, so the UI fallback can't mask missing
/// endpoints.
pub fn router<S>(options: LeptosOptions) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let routes = generate_route_list(App);
    let mut router = Router::new();
    for (from, to) in REDIRECTS {
        router = router.route(from, get(move || async move { Redirect::permanent(to) }));
    }
    router
        .leptos_routes(&options, routes, {
            let options = options.clone();
            move || shell(options.clone())
        })
        .fallback(fallback)
        .with_state(options)
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
