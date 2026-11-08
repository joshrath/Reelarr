use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
    response::Response,
};
use http_body_util::BodyExt;
use reelarr_server::{app, state::AppState};
use reelarr_stream::{InMemoryMediaLookup, PlayableFile};
use reelarr_types::{
    MediaFileId, MediaProbe, MediaStream, PlaybackInfo, PlaybackMethod, SessionInfo, StreamKind,
    UnsupportedKind,
};
use tower::ServiceExt;

const CHROME: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";
const SAFARI: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Safari/605.1.15";

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

fn stream(kind: StreamKind, codec: &str) -> MediaStream {
    MediaStream {
        index: 0,
        kind,
        codec: Some(codec.into()),
        profile: None,
        width: Some(1920),
        height: Some(1080),
        channels: None,
        language: None,
        default: true,
        forced: false,
    }
}

async fn setup() -> (tempfile::TempDir, Router) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_owned();
    std::fs::write(root.join("a.mp4"), b"0123456789").unwrap();
    std::fs::write(root.join("b.mp4"), b"0123456789").unwrap();
    let lookup = Arc::new(InMemoryMediaLookup::new());
    let probe = |video: &str| MediaProbe {
        container: vec!["mov".into(), "mp4".into()],
        duration_ms: Some(5000),
        bit_rate: None,
        streams: vec![
            stream(StreamKind::Video, video),
            stream(StreamKind::Audio, "aac"),
        ],
    };
    lookup.insert(PlayableFile {
        id: MediaFileId(1),
        path: root.join("a.mp4"),
        root: root.clone(),
        probe: Some(probe("h264")),
    });
    lookup.insert(PlayableFile {
        id: MediaFileId(2),
        path: root.join("b.mp4"),
        root,
        probe: Some(probe("hevc")),
    });
    let mut state = AppState::new(reelarr_db::Database::connect(":memory:").await.unwrap());
    state.media = lookup;
    (tmp, app(state))
}

#[tokio::test]
async fn playback_routes_require_a_session() {
    let (_tmp, app) = setup().await;
    for uri in ["/api/v1/media/1/stream", "/api/v1/media/1/playback"] {
        let resp = call(&app, "GET", uri, "", &[], "").await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{uri}");
    }
}

#[tokio::test]
async fn signed_in_user_can_range_stream() {
    let (_tmp, app) = setup().await;
    let cookie = sign_in(&app).await;
    let resp = call(
        &app,
        "GET",
        "/api/v1/media/1/stream",
        &cookie,
        &[("range", "bytes=2-4")],
        "",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(resp.headers()[header::CONTENT_RANGE], "bytes 2-4/10");
    assert_eq!(bytes(resp).await, b"234");

    let resp = call(&app, "GET", "/api/v1/media/9/stream", &cookie, &[], "").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn playback_info_depends_on_browser() {
    let (_tmp, app) = setup().await;
    let cookie = sign_in(&app).await;
    let info = |id: i64, ua: &'static str| {
        let (app, cookie) = (app.clone(), cookie.clone());
        async move {
            let resp = call(
                &app,
                "GET",
                &format!("/api/v1/media/{id}/playback"),
                &cookie,
                &[("user-agent", ua)],
                "",
            )
            .await;
            assert_eq!(resp.status(), StatusCode::OK);
            serde_json::from_slice::<PlaybackInfo>(&bytes(resp).await).unwrap()
        }
    };
    let h264 = info(1, CHROME).await;
    assert_eq!(h264.method, PlaybackMethod::DirectPlay);
    assert_eq!(h264.stream_url.as_deref(), Some("/api/v1/media/1/stream"));
    assert_eq!(h264.mime_type.as_deref(), Some("video/mp4"));

    let hevc_chrome = info(2, CHROME).await;
    assert_eq!(hevc_chrome.method, PlaybackMethod::Unsupported);
    assert_eq!(hevc_chrome.unsupported[0].kind, UnsupportedKind::VideoCodec);
    assert_eq!(info(2, SAFARI).await.method, PlaybackMethod::DirectPlay);
}
