//! The web UI as mounted by the server.

use axum::{Router, body::Body, http::Request};
use http_body_util::BodyExt;
use leptos::config::LeptosOptions;
use reelarr_server::{app_with_ui, state::AppState};
use reelarr_types::Health;
use tower::ServiceExt;

async fn test_app() -> Router {
    app_with_ui(
        AppState::new(reelarr_db::Database::connect(":memory:").await.unwrap()),
        LeptosOptions::builder()
            .output_name("reelarr")
            .site_root("target/site")
            .build(),
    )
}

async fn get(path: &str) -> (axum::http::StatusCode, String, String) {
    let res = test_app()
        .await
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
async fn api_routes_still_reach_the_api() {
    let (status, content_type, _) = get("/api/v1/auth/session").await;
    assert!(status.is_success(), "{status}");
    assert!(content_type.starts_with("application/json"));
}

#[tokio::test]
async fn health_returns_ok() {
    let (status, _, body) = get("/health").await;
    assert!(status.is_success());
    let health: Health = serde_json::from_str(&body).unwrap();
    assert_eq!(health.status, "ok");
}
