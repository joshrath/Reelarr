//! Library folder settings API, and playback of files on an offline folder.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
    response::Response,
};
use http_body_util::BodyExt;
use reelarr_media::db::SqliteMediaFileStore;
use reelarr_media::scan::{MediaFileStore, NewMediaFile};
use reelarr_server::{app, state::AppState};
use reelarr_types::{ApiError, LibraryRootInfo, RootHealth, SessionInfo};
use tower::ServiceExt;

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    cookie: &str,
    extra: &[(&str, &str)],
    body: &str,
) -> Response {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, "application/json");
    for (k, v) in extra {
        req = req.header(*k, *v);
    }
    app.clone()
        .oneshot(req.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap()
}

async fn bytes(resp: Response) -> Vec<u8> {
    resp.into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

fn cookies(resp: &Response) -> String {
    resp.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|h| h.to_str().unwrap().split(';').next().unwrap().to_owned())
        .collect::<Vec<_>>()
        .join("; ")
}

/// Run first-time setup and return a signed-in cookie header.
async fn sign_in(app: &Router) -> String {
    let resp = call(app, "GET", "/api/v1/auth/session", "", &[], "").await;
    let guest = cookies(&resp);
    let info: SessionInfo = serde_json::from_slice(&bytes(resp).await).unwrap();
    let resp = call(
        app,
        "POST",
        "/api/v1/auth/setup",
        &guest,
        &[("x-csrf-token", &info.csrf_token)],
        r#"{"username":"admin","password":"a-long-password"}"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    cookies(&resp)
}

async fn setup() -> (tempfile::TempDir, AppState, Router, String) {
    let tmp = tempfile::tempdir().unwrap();
    let db = reelarr_db::Database::connect(tmp.path().join("reelarr.db"))
        .await
        .unwrap();
    let state = AppState::new(db);
    let router = app(state.clone());
    let cookie = sign_in(&router).await;
    (tmp, state, router, cookie)
}

async fn csrf(app: &Router, cookie: &str) -> String {
    let resp = call(app, "GET", "/api/v1/auth/session", cookie, &[], "").await;
    let info: SessionInfo = serde_json::from_slice(&bytes(resp).await).unwrap();
    info.csrf_token
}

async fn api_error(resp: Response) -> ApiError {
    serde_json::from_slice(&bytes(resp).await).unwrap()
}

#[tokio::test]
async fn roots_require_a_session() {
    let (_tmp, _state, app, _cookie) = setup().await;
    let resp = call(&app, "GET", "/api/v1/library/roots", "", &[], "").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn add_list_check_and_remove_roots() {
    let (tmp, state, app, cookie) = setup().await;
    let token = csrf(&app, &cookie).await;
    let h = [("x-csrf-token", token.as_str())];
    let movies = tmp.path().join("movies");
    std::fs::create_dir(&movies).unwrap();
    std::fs::write(movies.join("a.mkv"), b"x").unwrap();
    let body = format!(r#"{{"path":"{}","kind":"library"}}"#, movies.display());

    let resp = call(&app, "POST", "/api/v1/library/roots", &cookie, &[], &body).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "CSRF is required");

    let resp = call(&app, "POST", "/api/v1/library/roots", &cookie, &h, &body).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let info: LibraryRootInfo = serde_json::from_slice(&bytes(resp).await).unwrap();
    assert_eq!(info.health, RootHealth::Online);
    assert_eq!(info.file_count, 0);
    let id = info.root.id.0;

    let resp = call(&app, "POST", "/api/v1/library/roots", &cookie, &h, &body).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(api_error(resp).await.code, "duplicate_root");
    let resp = call(
        &app,
        "POST",
        "/api/v1/library/roots",
        &cookie,
        &h,
        r#"{"path":"movies","kind":"library"}"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(api_error(resp).await.code, "invalid_path");

    SqliteMediaFileStore::new(&state.db)
        .insert(NewMediaFile {
            movie_id: None,
            root_id: info.root.id,
            path: movies.join("a.mkv").to_string_lossy().into_owned(),
            size_bytes: 1,
            modified_at: 1,
            probe: None,
            kind: Default::default(),
        })
        .await
        .unwrap();

    // The share goes away.
    std::fs::rename(&movies, tmp.path().join("away")).unwrap();
    let uri = format!("/api/v1/library/roots/{id}/check");
    let resp = call(&app, "POST", &uri, &cookie, &h, "").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let info: LibraryRootInfo = serde_json::from_slice(&bytes(resp).await).unwrap();
    assert!(
        matches!(info.health, RootHealth::Offline { .. }),
        "{:?}",
        info.health
    );
    assert_eq!(info.file_count, 1);

    let resp = call(&app, "GET", "/api/v1/library/roots", &cookie, &[], "").await;
    let list: Vec<LibraryRootInfo> = serde_json::from_slice(&bytes(resp).await).unwrap();
    assert_eq!(list, vec![info]);

    let uri = format!("/api/v1/library/roots/{id}");
    let resp = call(&app, "DELETE", &uri, &cookie, &h, "").await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(api_error(resp).await.code, "root_has_files");
    let resp = call(
        &app,
        "DELETE",
        &format!("{uri}?forget_files=true"),
        &cookie,
        &h,
        "",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(
        tmp.path().join("away/a.mkv").exists(),
        "files on disk are kept"
    );
    let resp = call(&app, "DELETE", &uri, &cookie, &h, "").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn playback_of_a_file_on_an_offline_root_is_503() {
    let (tmp, state, app, cookie) = setup().await;
    let token = csrf(&app, &cookie).await;
    let h = [("x-csrf-token", token.as_str())];
    let movies = tmp.path().join("movies");
    std::fs::create_dir(&movies).unwrap();
    std::fs::write(movies.join("a.mp4"), b"0123456789").unwrap();
    let body = format!(r#"{{"path":"{}","kind":"library"}}"#, movies.display());
    let resp = call(&app, "POST", "/api/v1/library/roots", &cookie, &h, &body).await;
    let info: LibraryRootInfo = serde_json::from_slice(&bytes(resp).await).unwrap();
    let file = SqliteMediaFileStore::new(&state.db)
        .insert(NewMediaFile {
            movie_id: None,
            root_id: info.root.id,
            path: movies.join("a.mp4").to_string_lossy().into_owned(),
            size_bytes: 10,
            modified_at: 1,
            probe: None,
            kind: Default::default(),
        })
        .await
        .unwrap();
    let stream = format!("/api/v1/media/{}/stream", file.id.0);
    let resp = call(&app, "GET", &stream, &cookie, &[], "").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(bytes(resp).await, b"0123456789");

    std::fs::rename(&movies, tmp.path().join("away")).unwrap();
    let check = format!("/api/v1/library/roots/{}/check", info.root.id.0);
    call(&app, "POST", &check, &cookie, &h, "").await;
    for uri in [stream, format!("/api/v1/media/{}/playback", file.id.0)] {
        let resp = call(&app, "GET", &uri, &cookie, &[], "").await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        assert_eq!(api_error(resp).await.code, "unavailable");
    }
}
