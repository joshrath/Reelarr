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
    get_with_cookie(path, "").await
}

async fn get_with_cookie(path: &str, cookie: &str) -> (axum::http::StatusCode, String, String) {
    let mut req = Request::get(path);
    if !cookie.is_empty() {
        req = req.header("cookie", cookie);
    }
    let res = test_app()
        .await
        .oneshot(req.body(Body::empty()).unwrap())
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
async fn store_floor_renders_shell() {
    let (status, content_type, body) = get("/").await;
    assert_eq!(status, 200);
    assert!(content_type.starts_with("text/html"));
    assert!(body.contains(r#"<nav aria-label="Main""#));
    assert!(body.contains(r#"<main id="main-content""#));
    assert!(body.contains("Skip to content"));
    assert!(body.contains(r#"aria-current="page""#));
    assert!(body.contains("/pkg/reelarr"));
    assert!(body.contains("The Store Floor"));
    assert!(body.contains(r#"<footer class="app-footer""#));
    assert!(body.contains(r#"aria-label="Now playing""#));
    assert!(body.contains(r#"data-theme="80s-vhs""#));
}

#[tokio::test]
async fn app_routes_render() {
    for (path, title) in [
        ("/counter", "The Special Order Counter"),
        ("/records", "The Record Corner"),
        ("/back-room", "The Back Room"),
        ("/employees-only", "Employees Only"),
    ] {
        let (status, _, body) = get(path).await;
        assert_eq!(status, 200, "{path}");
        assert!(body.contains("<main"), "{path}");
        assert!(
            body.contains(&format!(r#"<h1 id="page-title">{title}</h1>"#)),
            "{path}"
        );
    }
}

#[tokio::test]
async fn old_paths_redirect_to_renamed_sections() {
    for (from, to) in [
        ("/search", "/counter"),
        ("/queue", "/back-room"),
        ("/settings", "/employees-only"),
    ] {
        let res = test_app()
            .await
            .oneshot(Request::get(from).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            axum::http::StatusCode::PERMANENT_REDIRECT,
            "{from}"
        );
        assert_eq!(res.headers()["location"], to, "{from}");
    }
}

#[tokio::test]
async fn saved_display_prefs_render_on_the_server() {
    let (_, _, body) = get_with_cookie("/", "reelarr_theme=70s-wood; reelarr_crt=on").await;
    assert!(body.contains(r#"data-theme="70s-wood""#));
    assert!(body.contains(r#"class="crt""#));
    assert!(body.contains(r#"aria-pressed="true""#));
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

#[tokio::test]
async fn ui_responses_carry_security_headers() {
    let res = test_app()
        .await
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let h = res.headers();
    assert_eq!(h["cache-control"], "no-store");
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert_eq!(h["referrer-policy"], "same-origin");
    assert_eq!(h["content-security-policy"], "frame-ancestors 'none'");
    assert_eq!(h["x-frame-options"], "DENY");
}

#[tokio::test]
async fn pkg_assets_revalidate() {
    let res = test_app()
        .await
        .oneshot(
            Request::get("/pkg/reelarr.css")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.headers()["cache-control"], "no-cache");
    assert_eq!(res.headers()["x-content-type-options"], "nosniff");
}

#[tokio::test]
async fn font_assets_revalidate() {
    let res = test_app()
        .await
        .oneshot(
            Request::get("/fonts/plus-jakarta-sans-var.woff2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.headers()["cache-control"], "no-cache");
}

#[tokio::test]
async fn api_responses_keep_their_own_headers() {
    let res = test_app()
        .await
        .oneshot(
            Request::get("/api/v1/auth/session")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(res.status().is_success());
    let h = res.headers();
    assert!(h.get("content-security-policy").is_none());
    assert!(h.get("x-frame-options").is_none());
    let cookies: Vec<_> = h
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect();
    assert!(
        cookies.iter().any(|c| c.starts_with("reelarr_session=")),
        "{cookies:?}"
    );
    assert!(
        cookies.iter().any(|c| c.starts_with("reelarr_csrf=")),
        "{cookies:?}"
    );
}

#[tokio::test]
async fn watch_page_renders_the_vcr_deck() {
    let (status, content_type, body) = get("/watch/42").await;
    assert_eq!(status, 200);
    assert!(content_type.starts_with("text/html"));
    // Playback info is fetched in the browser, so the server sends the deck
    // in its loading state with the transport keys disabled.
    assert!(body.contains(r#"aria-label="VCR player""#));
    assert!(body.contains("Loading tape"));
    assert!(body.contains(r#"aria-label="Seek""#));
    assert!(body.contains(r#"aria-label="Volume""#));
    for key in ["REW", "PLAY", "STOP", "FF", "EJECT", "CRT: ON"] {
        assert!(body.contains(key), "{key}");
    }
    assert!(body.contains("--:--:--"));
    assert!(!body.contains("<video"));
    // The player is part of the Store Floor, and the footer deck stays empty.
    assert!(body.contains(r#"<a href="/" aria-current="page""#));
    assert!(body.contains("No tape in the deck"));
}

#[tokio::test]
async fn store_floor_renders_the_shelf_while_checking() {
    let (status, _, body) = get("/").await;
    assert_eq!(status, 200);
    // The library folders are fetched in the browser, so the server sends
    // the empty shelf with a loading status and no titles.
    assert!(body.contains(r#"aria-labelledby="shelf-title""#));
    assert!(body.contains(r#"class="shelf-board""#));
    assert!(body.contains("Checking the shelves"));
    assert!(body.contains(r#"class="shelf-slot""#));
    assert!(!body.contains(r#"class="clamshell"#));
    assert!(!body.contains("/case/"));
    assert!(body.contains("No tape in the deck"));
}

#[tokio::test]
async fn case_page_renders_the_clamshell_while_loading() {
    let (status, content_type, body) = get("/case/42").await;
    assert_eq!(status, 200);
    assert!(content_type.starts_with("text/html"));
    assert!(body.contains(r#"aria-label="Breadcrumb""#));
    assert!(body.contains("Case #42"));
    assert!(body.contains("Tape #42"));
    assert!(body.contains("Loading case"));
    assert!(body.contains(r#"class="clamshell clamshell-large""#));
    assert!(body.contains("No cover art"));
    // No made-up details, and no Play link until the tape is known.
    assert!(!body.contains("/watch/42"));
    assert!(!body.to_lowercase().contains("synopsis"));
    // The case belongs to the Store Floor, and the footer deck stays empty.
    assert!(body.contains(r#"<a href="/" aria-current="page""#));
    assert!(body.contains("No tape in the deck"));
}

#[tokio::test]
async fn case_page_for_a_bad_id_still_renders_the_frame() {
    let (status, _, body) = get("/case/not-a-number").await;
    assert_eq!(status, 200);
    assert!(body.contains("Case —"));
    assert!(body.contains("Loading case"));
}

#[tokio::test]
async fn counter_renders_the_search_desk() {
    let (status, content_type, body) = get("/counter").await;
    assert_eq!(status, 200);
    assert!(content_type.starts_with("text/html"));
    assert!(body.contains(r#"<h1 id="page-title">The Special Order Counter</h1>"#));
    // A real search form and a labelled field, which works without JS too.
    assert!(body.contains(r#"role="search""#));
    assert!(body.contains(r#"action="/counter""#));
    assert!(body.contains(r#"<label for="counter-q" class="visually-hidden">"#));
    assert!(body.contains(r#"id="counter-q""#));
    assert!(body.contains(r#"type="search""#));
    assert!(body.contains(r#"name="q""#));
    assert!(body.contains(r#"aria-label="Clear search""#));
    assert!(body.contains(r#"aria-pressed="true""#));
    // The session is checked in the browser; the server sends the desk
    // with a loading status, and says plainly that search isn't hooked up.
    assert!(body.contains("Search results &amp; shelf inventory"));
    assert!(body.contains("Checking the counter"));
    assert!(body.contains("Catalog search: not connected yet"));
    assert!(body.contains("Desk clipboard // Active slips"));
    assert!(body.contains("No order slips"));
    // No sample titles, results or clerks from the design.
    for made_up in [
        "Blue Velvet",
        "BLUE VELVET",
        "Longlegs",
        "LONGLEGS",
        "Dune",
        "DUNE",
        "Furiosa",
        "FURIOSA",
        "Cure (1997)",
        "CURE (1997)",
        "Dave",
        "DAVE",
        "Prowlarr",
        "PROWLARR",
        "qBittorrent",
        "SABnzbd",
        "In stock",
        "IN STOCK",
        "/watch/",
        "/case/",
    ] {
        assert!(!body.contains(made_up), "{made_up}");
    }
    // The counter is its own section, and the deck stays empty.
    assert!(body.contains(r#"<a href="/counter" aria-current="page""#));
    assert!(body.contains("No tape in the deck"));
}

#[tokio::test]
async fn counter_keeps_the_query_without_inventing_results() {
    let (status, _, body) = get("/counter?q=stop+making+sense").await;
    assert_eq!(status, 200);
    assert!(body.contains(r#"value="stop making sense""#));
    assert!(!body.contains("1 result"));
    assert!(!body.contains("IN STOCK"));
}
