//! Direct-play HTTP behavior against real files.

use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use http_body_util::BodyExt;
use reelarr_stream::{InMemoryMediaLookup, MediaLookup, PlayableFile};
use reelarr_types::MediaFileId;
use tower::ServiceExt;

struct Fixture {
    _tmp: tempfile::TempDir,
    app: Router,
    data: Vec<u8>,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("library");
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(root.join("Movie (2004)")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let movie = root.join("Movie (2004)/Movie (2004).mp4");
    std::fs::write(&movie, &data).unwrap();
    std::fs::write(outside.join("secret.mp4"), b"secret").unwrap();
    std::os::unix::fs::symlink(outside.join("secret.mp4"), root.join("escape.mp4")).unwrap();

    let lookup = Arc::new(InMemoryMediaLookup::new());
    let add = |id: i64, path: &Path| {
        lookup.insert(PlayableFile {
            id: MediaFileId(id),
            path: path.to_owned(),
            root: root.clone(),
            probe: None,
        })
    };
    add(1, &movie);
    add(2, &root.join("escape.mp4"));
    add(3, &root.join("../outside/secret.mp4"));
    add(4, &root.join("gone.mp4"));
    add(5, &root.join("Movie (2004)"));

    let app = Router::new()
        .route(
            "/media/{id}/stream",
            get(
                |State(l): State<Arc<InMemoryMediaLookup>>,
                 UrlPath(id): UrlPath<i64>,
                 method: Method,
                 headers: HeaderMap| async move {
                    let l: &dyn MediaLookup = &*l;
                    reelarr_stream::stream(l, MediaFileId(id), &method, &headers).await
                },
            ),
        )
        .with_state(lookup);
    Fixture {
        _tmp: tmp,
        app,
        data,
    }
}

async fn send(app: &Router, method: &str, id: i64, headers: &[(&str, &str)]) -> Response {
    let mut req = Request::builder()
        .method(method)
        .uri(format!("/media/{id}/stream"));
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    app.clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn body(resp: Response) -> Vec<u8> {
    resp.into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

fn h(resp: &Response, name: header::HeaderName) -> &str {
    resp.headers()[name].to_str().unwrap()
}

#[tokio::test]
async fn full_get() {
    let f = fixture();
    let resp = send(&f.app, "GET", 1, &[]).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(h(&resp, header::ACCEPT_RANGES), "bytes");
    assert_eq!(h(&resp, header::CONTENT_TYPE), "video/mp4");
    assert_eq!(h(&resp, header::CONTENT_LENGTH), "200000");
    assert!(resp.headers().contains_key(header::ETAG));
    assert!(resp.headers().contains_key(header::LAST_MODIFIED));
    assert_eq!(body(resp).await, f.data);
}

#[tokio::test]
async fn partial_ranges() {
    let f = fixture();
    let resp = send(&f.app, "GET", 1, &[("range", "bytes=100-199")]).await;
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(h(&resp, header::CONTENT_RANGE), "bytes 100-199/200000");
    assert_eq!(h(&resp, header::CONTENT_LENGTH), "100");
    assert_eq!(body(resp).await, f.data[100..200]);

    let resp = send(&f.app, "GET", 1, &[("range", "bytes=-10")]).await;
    assert_eq!(
        h(&resp, header::CONTENT_RANGE),
        "bytes 199990-199999/200000"
    );
    assert_eq!(body(resp).await, f.data[199_990..]);

    // Large open-ended range spans several read chunks.
    let resp = send(&f.app, "GET", 1, &[("range", "bytes=1000-")]).await;
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(body(resp).await, f.data[1000..]);
}

#[tokio::test]
async fn unsatisfiable_range_is_416() {
    let f = fixture();
    let resp = send(&f.app, "GET", 1, &[("range", "bytes=200000-")]).await;
    assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(h(&resp, header::CONTENT_RANGE), "bytes */200000");
    assert!(body(resp).await.is_empty());
}

#[tokio::test]
async fn malformed_or_multi_range_gets_full_body() {
    let f = fixture();
    for r in ["bytes=abc", "bytes=0-1,5-6", "lines=1-2"] {
        let resp = send(&f.app, "GET", 1, &[("range", r)]).await;
        assert_eq!(resp.status(), StatusCode::OK, "{r}");
    }
}

#[tokio::test]
async fn head_has_headers_without_body() {
    let f = fixture();
    let resp = send(&f.app, "HEAD", 1, &[("range", "bytes=0-9")]).await;
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(h(&resp, header::CONTENT_LENGTH), "10");
    assert!(body(resp).await.is_empty());
}

#[tokio::test]
async fn if_range_and_if_none_match() {
    let f = fixture();
    let etag = h(&send(&f.app, "HEAD", 1, &[]).await, header::ETAG).to_owned();

    let resp = send(
        &f.app,
        "GET",
        1,
        &[("range", "bytes=0-9"), ("if-range", &etag)],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);

    let resp = send(
        &f.app,
        "GET",
        1,
        &[("range", "bytes=0-9"), ("if-range", "\"stale\"")],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = send(&f.app, "GET", 1, &[("if-none-match", &etag)]).await;
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn refuses_paths_outside_the_root_and_missing_files() {
    let f = fixture();
    for id in [2, 3, 4, 5, 99] {
        let resp = send(&f.app, "GET", id, &[]).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "id {id}");
        assert!(!body(resp).await.windows(6).any(|w| w == b"secret"));
    }
}
