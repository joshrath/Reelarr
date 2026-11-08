//! Mock exchanges and fixtures written for Reelarr using only the public API spec.
use reelarr_indexer::*;
use std::time::{Duration, SystemTime};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, query_param},
};

const CAPS: &str = include_str!("fixtures/caps.xml");
const MOVIE: &str = include_str!("fixtures/movie.xml");
const EMPTY: &str = include_str!("fixtures/empty.xml");
const KEY: &str = "fixture-only-key";
fn config(server: &MockServer) -> IndexerConfig {
    let mut config = IndexerConfig::new(
        "fixture",
        Url::parse(&server.uri()).unwrap(),
        SecretString::new(KEY),
    );
    config.allow_http = true;
    config.minimum_interval = Duration::ZERO;
    config
}
fn query() -> MovieQuery {
    MovieQuery {
        title: "Orbit & Ocean".into(),
        year: Some(2025),
        imdb_id: Some("tt0000123".into()),
        ..Default::default()
    }
}
async fn caps(server: &MockServer, body: &str) {
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("t", "caps"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .expect(1)
        .mount(server)
        .await;
}
async fn movie(server: &MockServer, body: &str) {
    Mock::given(query_param("t", "movie"))
        .and(query_param("apikey", KEY))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(server)
        .await;
}
fn page(offset: u64, total: u64, ids: &[&str]) -> String {
    let items = ids.iter().map(|id| format!("<item><title>Independent {id}</title><guid>{id}</guid><enclosure url='/nzb/{id}' length='42' type='application/x-nzb'/></item>")).collect::<String>();
    format!(
        "<rss version='2.0' xmlns:other='http://www.newznab.com/DTD/2010/feeds/attributes/'><channel><other:response offset='{offset}' total='{total}'/>{items}</channel></rss>"
    )
}

#[tokio::test]
async fn capabilities_are_typed_and_cached_across_clones() {
    let server = MockServer::start().await;
    caps(&server, CAPS).await;
    let client = NewznabClient::new(config(&server)).unwrap();
    let cloned = client.clone();
    let (a, b) = tokio::join!(client.capabilities(), cloned.capabilities());
    let a = a.unwrap();
    assert_eq!(a, b.unwrap());
    assert!(a.movie.supported_params.contains("tmdbid"));
    assert_eq!((a.default_limit, a.max_limit), (2, 4));
    assert_eq!(a.categories[1].parent_id, Some(2000));
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].url.query_pairs().any(|(k, _)| k == "apikey"));
}
#[tokio::test]
async fn movie_id_search_parses_rss_attributes_and_redacts_debug() {
    let server = MockServer::start().await;
    caps(&server, CAPS).await;
    Mock::given(query_param("t", "movie"))
        .and(query_param("imdbid", "0000123"))
        .and(query_param("cat", "2000"))
        .and(query_param("limit", "2"))
        .and(query_param("offset", "0"))
        .and(query_param("apikey", KEY))
        .respond_with(ResponseTemplate::new(200).set_body_string(MOVIE))
        .expect(1)
        .mount(&server)
        .await;
    let result = NewznabClient::new(config(&server))
        .unwrap()
        .search_movie(&query())
        .await
        .unwrap();
    assert!(!result.truncated);
    let r = &result.releases[0];
    assert_eq!(r.title, "Orbit & Ocean.2025.1080p");
    assert_eq!(r.size, Some(8_000_000_001));
    assert_eq!(r.categories, vec![2000, 2040]);
    assert_eq!(r.imdb_id.as_deref(), Some("0000123"));
    assert_eq!(r.tmdb_id, Some(456));
    assert_eq!(
        (r.grabs, r.files, r.password, r.year),
        (Some(12), Some(44), Some(0), Some(2025))
    );
    assert_eq!(
        r.pub_date.as_deref(),
        Some("Mon, 02 Nov 2026 12:34:56 +0000")
    );
    assert!(r.download_url.expose().contains(KEY));
    assert_eq!(r.attributes["language"], vec!["English"]);
    assert!(!format!("{result:?}").contains(KEY));
    assert_eq!(r.download_url.to_string(), "[REDACTED]");
}
#[tokio::test]
async fn tmdb_extension_only_when_advertised() {
    let server = MockServer::start().await;
    caps(&server, CAPS).await;
    Mock::given(query_param("t", "movie"))
        .and(query_param("tmdbid", "456"))
        .respond_with(ResponseTemplate::new(200).set_body_string(MOVIE))
        .expect(1)
        .mount(&server)
        .await;
    let mut q = query();
    q.imdb_id = None;
    q.tmdb_id = Some(456);
    NewznabClient::new(config(&server))
        .unwrap()
        .search_movie(&q)
        .await
        .unwrap();
}
#[tokio::test]
async fn title_year_fallback_does_not_send_unadvertised_id() {
    let server = MockServer::start().await;
    caps(
        &server,
        &CAPS.replace("q,imdbid,tmdbid,cat,limit,offset", "imdbid"),
    )
    .await;
    Mock::given(query_param("t", "search"))
        .and(query_param("q", "Orbit & Ocean 2025"))
        .respond_with(ResponseTemplate::new(200).set_body_string(MOVIE))
        .expect(1)
        .mount(&server)
        .await;
    let mut q = query();
    q.imdb_id = None;
    q.tmdb_id = Some(456);
    NewznabClient::new(config(&server))
        .unwrap()
        .search_movie(&q)
        .await
        .unwrap();
    for r in server.received_requests().await.unwrap() {
        assert!(!r.url.query_pairs().any(|(k, _)| k == "tmdbid"));
    }
}
#[tokio::test]
async fn empty_movie_results_fall_back_to_general_search() {
    let server = MockServer::start().await;
    caps(&server, CAPS).await;
    movie(&server, EMPTY).await;
    Mock::given(query_param("t", "search"))
        .and(query_param("q", "Orbit & Ocean 2025"))
        .respond_with(ResponseTemplate::new(200).set_body_string(MOVIE))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        NewznabClient::new(config(&server))
            .unwrap()
            .search_movie(&query())
            .await
            .unwrap()
            .releases
            .len(),
        1
    );
}
#[tokio::test]
async fn unsupported_movie_error_falls_back_but_auth_does_not() {
    let server = MockServer::start().await;
    caps(&server, CAPS).await;
    movie(
        &server,
        "<error code='203' description='No movie function'/>",
    )
    .await;
    Mock::given(query_param("t", "search"))
        .respond_with(ResponseTemplate::new(200).set_body_string(MOVIE))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        NewznabClient::new(config(&server))
            .unwrap()
            .search_movie(&query())
            .await
            .unwrap()
            .releases
            .len(),
        1
    );
}
#[tokio::test]
async fn paginates_offsets_and_deduplicates_guid() {
    let server = MockServer::start().await;
    caps(&server, CAPS).await;
    for (offset, ids) in [(0, vec!["a", "b"]), (2, vec!["b", "c"])] {
        Mock::given(query_param("t", "movie"))
            .and(query_param("offset", offset.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_string(page(offset, 4, &ids)))
            .expect(1)
            .mount(&server)
            .await;
    }
    let results = NewznabClient::new(config(&server))
        .unwrap()
        .search_movie(&query())
        .await
        .unwrap();
    assert_eq!(
        results
            .releases
            .iter()
            .map(|r| r.guid.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "b", "c"]
    );
    assert!(!results.truncated);
}
#[tokio::test]
async fn repeated_offset_terminates_without_a_loop() {
    let server = MockServer::start().await;
    caps(&server, CAPS).await;
    movie(&server, &page(0, 100, &["a", "b"])).await;
    let r = NewznabClient::new(config(&server))
        .unwrap()
        .search_movie(&query())
        .await
        .unwrap();
    assert!(r.truncated);
    assert_eq!(r.releases.len(), 2);
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}
#[tokio::test]
async fn repeated_guids_and_page_result_limits_terminate() {
    for (max_pages, max_results) in [(1, 100), (20, 1), (20, 100)] {
        let server = MockServer::start().await;
        caps(&server, CAPS).await;
        Mock::given(query_param("t", "movie"))
            .respond_with(|r: &wiremock::Request| {
                let offset = r
                    .url
                    .query_pairs()
                    .find(|(k, _)| k == "offset")
                    .unwrap()
                    .1
                    .parse()
                    .unwrap();
                ResponseTemplate::new(200).set_body_string(page(offset, 100, &["a", "b"]))
            })
            .mount(&server)
            .await;
        let mut c = config(&server);
        c.max_pages = max_pages;
        c.max_results = max_results;
        let r = NewznabClient::new(c)
            .unwrap()
            .search_movie(&query())
            .await
            .unwrap();
        assert!(r.truncated);
        assert!(r.releases.len() <= max_results);
        assert!(server.received_requests().await.unwrap().len() <= 3);
    }
}
#[tokio::test]
async fn typed_xml_errors_on_http_200_never_leak_descriptions() {
    for (code, expected) in [
        (100, IndexerError::BadApiKey),
        (101, IndexerError::AccountSuspended),
        (102, IndexerError::Unauthorized),
        (200, IndexerError::InvalidParameters),
        (300, IndexerError::NotFound),
        (910, IndexerError::ApiDisabled),
        (987, IndexerError::Api { code: 987 }),
    ] {
        let server = MockServer::start().await;
        caps(&server, CAPS).await;
        movie(
            &server,
            &format!("<error code='{code}' description='https://echo.invalid/?apikey={KEY}'/>"),
        )
        .await;
        let error = NewznabClient::new(config(&server))
            .unwrap()
            .search_movie(&query())
            .await
            .unwrap_err();
        assert_eq!(error, expected);
        assert!(!format!("{error:?} {error}").contains(KEY));
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }
}
#[tokio::test]
async fn caps_errors_are_parsed_too() {
    let server = MockServer::start().await;
    caps(&server, include_str!("fixtures/bad-key.xml")).await;
    assert_eq!(
        NewznabClient::new(config(&server))
            .unwrap()
            .capabilities()
            .await
            .unwrap_err(),
        IndexerError::BadApiKey
    );
}
#[tokio::test]
async fn retry_after_and_quota_cooldown_shared_by_clones() {
    for date in [false, true] {
        let server = MockServer::start().await;
        caps(&server, CAPS).await;
        let retry = if date {
            httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(120))
        } else {
            "120".into()
        };
        Mock::given(query_param("t", "movie"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", retry))
            .expect(1)
            .mount(&server)
            .await;
        let client = NewznabClient::new(config(&server)).unwrap();
        for c in [client.clone(), client] {
            let e = c.search_movie(&query()).await.unwrap_err();
            assert!(
                matches!(e,IndexerError::RateLimited{retry_after} if retry_after > Duration::from_secs(118))
            );
        }
    }
    let server = MockServer::start().await;
    caps(&server, CAPS).await;
    movie(
        &server,
        "<error code='500' description='Provider quota extension'/>",
    )
    .await;
    assert!(matches!(
        NewznabClient::new(config(&server))
            .unwrap()
            .search_movie(&query())
            .await
            .unwrap_err(),
        IndexerError::RateLimited { .. }
    ));
}
#[tokio::test]
async fn http_status_redirects_malformed_and_unsafe_urls() {
    for (status, body, error) in [
        (503, "temporarily down", IndexerError::Http { status: 503 }),
        (302, "", IndexerError::Http { status: 302 }),
        (200, "<rss", IndexerError::MalformedXml),
        (200, "<html/>", IndexerError::MalformedXml),
        (
            200,
            "<!DOCTYPE rss [<!ENTITY key 'expanded'>]><rss version='2.0'><channel/></rss>",
            IndexerError::MalformedXml,
        ),
    ] {
        let server = MockServer::start().await;
        caps(&server, CAPS).await;
        Mock::given(query_param("t", "movie"))
            .respond_with(
                ResponseTemplate::new(status)
                    .set_body_string(body)
                    .insert_header(
                        "Location",
                        "https://redirect.invalid/?apikey=fixture-only-key",
                    ),
            )
            .mount(&server)
            .await;
        assert_eq!(
            NewznabClient::new(config(&server))
                .unwrap()
                .search_movie(&query())
                .await
                .unwrap_err(),
            error
        );
    }
    let server = MockServer::start().await;
    caps(&server, CAPS).await;
    movie(
        &server,
        &MOVIE.replace("/api?t=get", "https://attacker.invalid/api?t=get"),
    )
    .await;
    assert_eq!(
        NewznabClient::new(config(&server))
            .unwrap()
            .search_movie(&query())
            .await
            .unwrap_err(),
        IndexerError::UnsafeDownloadUrl
    );
}
#[tokio::test]
async fn response_size_and_deadline_are_enforced() {
    let server = MockServer::start().await;
    caps(&server, CAPS).await;
    movie(&server, &"x".repeat(2048)).await;
    let mut c = config(&server);
    c.max_response_bytes = 1024;
    assert_eq!(
        NewznabClient::new(c)
            .unwrap()
            .search_movie(&query())
            .await
            .unwrap_err(),
        IndexerError::ResponseTooLarge
    );
    let server = MockServer::start().await;
    Mock::given(query_param("t", "caps"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(CAPS)
                .set_delay(Duration::from_millis(200)),
        )
        .mount(&server)
        .await;
    let mut c = config(&server);
    c.connect_timeout = Duration::from_millis(10);
    c.request_timeout = Duration::from_millis(30);
    assert_eq!(
        NewznabClient::new(c)
            .unwrap()
            .capabilities()
            .await
            .unwrap_err(),
        IndexerError::Timeout
    );
}
#[tokio::test]
async fn minimum_interval_is_enforced_between_requests() {
    let server = MockServer::start().await;
    caps(&server, CAPS).await;
    movie(&server, MOVIE).await;
    let mut c = config(&server);
    c.minimum_interval = Duration::from_millis(60);
    let start = tokio::time::Instant::now();
    NewznabClient::new(c)
        .unwrap()
        .search_movie(&query())
        .await
        .unwrap();
    assert!(start.elapsed() >= Duration::from_millis(60));
}
#[tokio::test]
async fn config_and_store_validation_and_debug_redaction() {
    let server = MockServer::start().await;
    let c = config(&server);
    let store: Box<dyn IndexerStore> = Box::<InMemoryIndexerStore>::default();
    store.upsert(c.clone()).await.unwrap();
    assert_eq!(store.list().await.unwrap().len(), 1);
    assert_eq!(
        store
            .get("fixture")
            .await
            .unwrap()
            .unwrap()
            .api_key
            .expose(),
        KEY
    );
    assert!(store.remove("fixture").await.unwrap());
    assert!(!store.remove("fixture").await.unwrap());
    for variant in 0..8 {
        let mut invalid = c.clone();
        match variant {
            0 => invalid.allow_http = false,
            1 => invalid.api_key = SecretString::new(""),
            2 => invalid.api_path = "//evil.invalid/api".into(),
            3 => invalid.max_pages = 0,
            4 => invalid.request_timeout = Duration::ZERO,
            5 => invalid.name = "".into(),
            6 => {
                invalid.base_url =
                    Url::parse(&format!("https://user:{KEY}@example.invalid/?apikey={KEY}"))
                        .unwrap()
            }
            _ => invalid.api_path = format!("/api?apikey={KEY}"),
        }
        assert_eq!(invalid.validate(), Err(IndexerError::InvalidConfig));
        assert!(!format!("{invalid:?}").contains(KEY));
        assert_eq!(
            store.upsert(invalid).await.unwrap_err(),
            IndexerError::InvalidConfig
        );
    }
}
#[tokio::test]
async fn disabled_invalid_queries_and_transport_errors_are_safe() {
    let server = MockServer::start().await;
    let mut c = config(&server);
    c.enabled = false;
    assert_eq!(
        NewznabClient::new(c)
            .unwrap()
            .search_movie(&query())
            .await
            .unwrap_err(),
        IndexerError::Disabled
    );
    let client = NewznabClient::new(config(&server)).unwrap();
    let mut q = query();
    q.imdb_id = Some("tt123&apikey=bad".into());
    assert_eq!(
        client.search_movie(&q).await.unwrap_err(),
        IndexerError::InvalidQuery
    );
    assert!(server.received_requests().await.unwrap().is_empty());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let mut c = IndexerConfig::new(
        "closed",
        Url::parse(&format!("http://{addr}")).unwrap(),
        SecretString::new(KEY),
    );
    c.allow_http = true;
    c.minimum_interval = Duration::ZERO;
    let e = NewznabClient::new(c)
        .unwrap()
        .capabilities()
        .await
        .unwrap_err();
    assert_eq!(e, IndexerError::Transport);
    assert!(!format!("{e} {e:?}").contains(KEY));
}

#[tokio::test]
async fn explicit_capability_refresh_and_caps_limit_clamping() {
    let server = MockServer::start().await;
    Mock::given(query_param("t", "caps"))
        .respond_with(ResponseTemplate::new(200).set_body_string(CAPS))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(query_param("t", "movie"))
        .and(query_param("limit", "4"))
        .respond_with(ResponseTemplate::new(200).set_body_string(MOVIE))
        .expect(1)
        .mount(&server)
        .await;
    let client = NewznabClient::new(config(&server)).unwrap();
    client.capabilities().await.unwrap();
    client.invalidate_capabilities().await;
    let mut q = query();
    q.limit = Some(9999);
    client.search_movie(&q).await.unwrap();
}

#[tokio::test]
async fn unsupported_is_not_mistaken_for_empty_when_fallback_unavailable() {
    let server = MockServer::start().await;
    caps(
        &server,
        &CAPS.replace("<search available=\"yes\"", "<search available=\"no\""),
    )
    .await;
    movie(&server, "<error code='202' description='Unsupported'/>").await;
    assert_eq!(
        NewznabClient::new(config(&server))
            .unwrap()
            .search_movie(&query())
            .await
            .unwrap_err(),
        IndexerError::Unsupported
    );
}

#[tokio::test]
async fn empty_inconsistent_pages_and_overflow_are_truncated() {
    for (offset, total, ids) in [(0, 100, vec![]), (u64::MAX, u64::MAX, vec!["overflow"])] {
        let server = MockServer::start().await;
        caps(&server, CAPS).await;
        movie(&server, &page(offset, total, &ids)).await;
        let mut q = query();
        q.offset = offset;
        assert!(
            NewznabClient::new(config(&server))
                .unwrap()
                .search_movie(&q)
                .await
                .unwrap()
                .truncated
        );
    }
}

#[tokio::test]
async fn chunked_response_without_content_length_is_bounded() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let worker = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        loop {
            let mut buffer = [0; 1024];
            let count = stream.read(&mut buffer).await.unwrap();
            if count == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..count]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n80\r\n{}\r\n0\r\n\r\n",
            "x".repeat(128)
        );
        let _ = stream.write_all(response.as_bytes()).await;
    });
    let mut c = IndexerConfig::new(
        "chunked",
        Url::parse(&format!("http://{addr}")).unwrap(),
        SecretString::new(KEY),
    );
    c.allow_http = true;
    c.max_response_bytes = 64;
    assert_eq!(
        NewznabClient::new(c)
            .unwrap()
            .capabilities()
            .await
            .unwrap_err(),
        IndexerError::ResponseTooLarge
    );
    worker.await.unwrap();
}

#[tokio::test]
async fn malformed_numeric_and_missing_required_fields_are_errors() {
    for body in [
        MOVIE.replace("8000000001", "-1"),
        MOVIE.replace("<guid isPermaLink=\"false\">orbit-ocean-01</guid>", ""),
        MOVIE.replace("/api?t=get", "javascript:bad"),
        MOVIE.replace("/api?t=get", "//evil.invalid/get"),
    ] {
        let server = MockServer::start().await;
        caps(&server, CAPS).await;
        movie(&server, &body).await;
        let error = NewznabClient::new(config(&server))
            .unwrap()
            .search_movie(&query())
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            IndexerError::MalformedXml | IndexerError::UnsafeDownloadUrl
        ));
    }
}
