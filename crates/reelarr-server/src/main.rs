use std::net::SocketAddr;

use axum::{Json, Router, routing::get};
use leptos::config::{LeptosOptions, get_configuration};
use reelarr_types::Health;

fn app(leptos_options: LeptosOptions) -> Router {
    Router::new()
        .route("/health", get(health))
        // Web UI: SSR pages plus `/pkg/*` assets as the fallback.
        .merge(reelarr_ui::router(leptos_options))
}

/// Leptos settings from the `LEPTOS_*` env vars that `cargo leptos` sets, with
/// defaults so a plain `cargo run` still finds the site in `target/site`.
fn leptos_options() -> LeptosOptions {
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

async fn health() -> Json<Health> {
    Json(Health::ok())
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let port: u16 = std::env::var("REELARR_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(7979);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind port");
    tracing::info!("Reelarr listening on {addr}");
    axum::serve(listener, app(leptos_options()))
        .await
        .expect("server error");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_app() -> Router {
        app(LeptosOptions::builder()
            .output_name("reelarr")
            .site_root("target/site")
            .build())
    }

    async fn get(path: &str) -> (axum::http::StatusCode, String, String) {
        let res = test_app()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let content_type = res
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            content_type,
            String::from_utf8_lossy(&body).into_owned(),
        )
    }

    #[tokio::test]
    async fn library_page_renders_shell() {
        let (status, content_type, body) = get("/").await;
        assert_eq!(status, 200);
        assert!(content_type.starts_with("text/html"));
        assert!(body.contains(r#"<nav aria-label="Main""#));
        assert!(body.contains(r#"<main id="main-content""#));
        assert!(body.contains("Skip to content"));
        assert!(body.contains(r#"aria-current="page""#));
        assert!(body.contains("/pkg/reelarr"));
    }

    #[tokio::test]
    async fn app_routes_render() {
        for path in ["/search", "/queue", "/settings"] {
            let (status, _, body) = get(path).await;
            assert_eq!(status, 200, "{path}");
            assert!(body.contains("<main"), "{path}");
        }
    }

    #[tokio::test]
    async fn login_page_renders_form() {
        let (status, _, body) = get("/login").await;
        assert_eq!(status, 200);
        assert!(body.contains(r#"for="username""#));
        assert!(body.contains(r#"type="password""#));
        assert!(body.contains(r#"autocomplete="current-password""#));
    }

    #[tokio::test]
    async fn unknown_page_is_404_html() {
        let (status, content_type, body) = get("/no-such-page").await;
        assert_eq!(status, 404);
        assert!(content_type.starts_with("text/html"));
        assert!(body.contains("Page not found"));
    }

    #[tokio::test]
    async fn unknown_api_path_is_json_404() {
        let (status, content_type, body) = get("/api/v1/nope").await;
        assert_eq!(status, 404);
        assert!(content_type.starts_with("application/json"));
        assert!(!body.contains("<html"));
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let res = test_app()
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(res.status().is_success());
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let health: Health = serde_json::from_slice(&body).unwrap();
        assert_eq!(health.status, "ok");
    }
}
