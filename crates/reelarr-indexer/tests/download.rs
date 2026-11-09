//! Download-host allowlist (#44) and NZB fetching, against local mock exchanges only.
use reelarr_indexer::*;
use std::{collections::BTreeMap, time::Duration};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, query_param},
};

const CAPS: &str = include_str!("fixtures/caps.xml");
const KEY: &str = "fixture-only-key";
const NZB: &str = "<?xml version='1.0' encoding='utf-8'?>\n<!DOCTYPE nzb PUBLIC '-//newzBin//DTD NZB 1.1//EN' 'http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd'>\n<nzb xmlns='http://www.newzbin.com/DTD/2003/nzb'><file poster='p' date='1' subject='s'><groups><group>alt.test</group></groups><segments><segment bytes='1' number='1'>a@b</segment></segments></file></nzb>";

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
fn https_config(hosts: &[&str]) -> IndexerConfig {
    let mut config = IndexerConfig::new(
        "cdn",
        Url::parse("https://indexer.example.org").unwrap(),
        SecretString::new(KEY),
    );
    config.extra_download_hosts = hosts.iter().map(|h| h.to_string()).collect();
    config
}
fn release(url: &str) -> Release {
    Release {
        title: "Independent Fixture".into(),
        guid: "g1".into(),
        download_url: SecretString::new(url),
        size: None,
        pub_date: None,
        categories: vec![],
        imdb_id: None,
        tmdb_id: None,
        grabs: None,
        files: None,
        password: None,
        year: None,
        attributes: BTreeMap::new(),
    }
}
fn url(s: &str) -> Url {
    Url::parse(s).unwrap()
}

#[test]
fn host_normalization_and_validation() {
    assert_eq!(
        normalize_download_host(" DL.Example.NET. ").unwrap(),
        "dl.example.net"
    );
    assert_eq!(
        normalize_download_host("bücher.example").unwrap(),
        "xn--bcher-kva.example"
    );
    for bad in [
        "",
        " ",
        "https://dl.example.net",
        "dl.example.net:443",
        "dl.example.net/path",
        "user@dl.example.net",
        "*.example.net",
        "127.0.0.1",
        "[::1]",
        "localhost",
        "dl..example.net",
        "dl.example.net?x",
        "dl.exa\u{7}mple.net",
        "dl example.net",
    ] {
        assert_eq!(
            normalize_download_host(bad),
            Err(IndexerError::InvalidConfig),
            "{bad:?}"
        );
    }
    let error = normalize_download_host("secret-host-value:1").unwrap_err();
    assert!(!format!("{error} {error:?}").contains("secret-host-value"));

    assert!(https_config(&[]).validate().is_ok());
    assert!(
        https_config(&["dl.example.net", "cdn.example.com"])
            .validate()
            .is_ok()
    );
    for hosts in [
        vec!["DL.example.net"],
        vec!["dl.example.net."],
        vec!["dl.example.net", "dl.example.net"],
        vec!["dl.example.net:8443"],
        vec!["10.0.0.1"],
    ] {
        assert_eq!(
            https_config(&hosts).validate(),
            Err(IndexerError::InvalidConfig),
            "{hosts:?}"
        );
    }
    let many: Vec<String> = (0..=MAX_EXTRA_DOWNLOAD_HOSTS)
        .map(|i| format!("h{i}.example.net"))
        .collect();
    let refs: Vec<&str> = many.iter().map(String::as_str).collect();
    assert!(
        https_config(&refs[..MAX_EXTRA_DOWNLOAD_HOSTS])
            .validate()
            .is_ok()
    );
    assert_eq!(
        https_config(&refs).validate(),
        Err(IndexerError::InvalidConfig)
    );
    // Store validation goes through the same checks.
    let store = InMemoryIndexerStore::default();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    assert_eq!(
        rt.block_on(store.upsert(https_config(&["Bad.Example.net"]))),
        Err(IndexerError::InvalidConfig)
    );
    rt.block_on(store.upsert(https_config(&["dl.example.net"])))
        .unwrap();
    let stored = rt.block_on(store.get("cdn")).unwrap().unwrap();
    assert_eq!(stored.extra_download_hosts, ["dl.example.net"]);
    // Debug output stays redacted and does not need to list hosts.
    assert!(!format!("{stored:?}").contains(KEY));
}

#[test]
fn download_url_policy() {
    let config = https_config(&["dl.example.net"]);
    let endpoint = url("https://indexer.example.org/api");
    let allowed = |s: &str| config.download_url_allowed(&endpoint, &url(s));
    // Same origin: default rule.
    assert!(allowed("https://indexer.example.org/getnzb/1?apikey=k"));
    assert!(!allowed("https://indexer.example.org:8443/getnzb/1"));
    assert!(!allowed("http://indexer.example.org/getnzb/1"));
    // Allowlisted extra host: HTTPS, default port, exact host only.
    assert!(allowed("https://dl.example.net/get/1?apikey=k"));
    assert!(allowed("https://DL.EXAMPLE.NET/get/1"));
    assert!(allowed("https://dl.example.net:443/get/1"));
    assert!(!allowed("http://dl.example.net/get/1"));
    assert!(!allowed("https://dl.example.net:8443/get/1"));
    assert!(!allowed("https://sub.dl.example.net/get/1"));
    assert!(!allowed("https://example.net/get/1"));
    assert!(!allowed("https://dl.example.net.evil.test/get/1"));
    assert!(!allowed("https://other.example.com/get/1"));
    assert!(!allowed("https://user:pw@dl.example.net/get/1"));
    assert!(!allowed("https://dl.example.net/get/1#frag"));
    assert!(!allowed("ftp://dl.example.net/get/1"));
    // Default (empty) list keeps the same-host-only behavior.
    let strict = https_config(&[]);
    assert!(!strict.download_url_allowed(&endpoint, &url("https://dl.example.net/get/1")));
    // allow_http never extends to extra hosts.
    let mut http = https_config(&["dl.example.net"]);
    http.allow_http = true;
    assert!(!http.download_url_allowed(&endpoint, &url("http://dl.example.net/get/1")));
}

async fn search(
    server: &MockServer,
    config: IndexerConfig,
    enclosure: &str,
) -> Result<SearchResults, IndexerError> {
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("t", "caps"))
        .respond_with(ResponseTemplate::new(200).set_body_string(CAPS))
        .mount(server)
        .await;
    let body = format!(
        "<rss version='2.0'><channel><item><title>Independent 1</title><guid>1</guid><enclosure url='{enclosure}' type='application/x-nzb'/></item></channel></rss>"
    );
    Mock::given(query_param("apikey", KEY))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(server)
        .await;
    let client = NewznabClient::new(config).unwrap();
    client
        .search_movie(&MovieQuery {
            title: "Orbit".into(),
            ..Default::default()
        })
        .await
}

#[tokio::test]
async fn search_accepts_allowlisted_and_rejects_other_hosts() {
    let server = MockServer::start().await;
    let mut cfg = config(&server);
    cfg.extra_download_hosts = vec!["dl.example.net".into()];
    let results = search(
        &server,
        cfg,
        "https://dl.example.net/get/1?apikey=fixture-only-key",
    )
    .await
    .unwrap();
    assert_eq!(results.releases.len(), 1);
    assert!(
        results.releases[0]
            .download_url
            .expose()
            .starts_with("https://dl.example.net/")
    );

    let server = MockServer::start().await;
    let error = search(
        &server,
        config(&server),
        "https://dl.example.net/get/1?apikey=fixture-only-key",
    )
    .await
    .unwrap_err();
    assert_eq!(error, IndexerError::UnsafeDownloadUrl);
    assert!(!format!("{error} {error:?}").contains(KEY));

    let server = MockServer::start().await;
    let mut cfg = config(&server);
    cfg.extra_download_hosts = vec!["dl.example.net".into()];
    assert_eq!(
        search(&server, cfg, "http://dl.example.net/get/1")
            .await
            .unwrap_err(),
        IndexerError::UnsafeDownloadUrl
    );
}

#[tokio::test]
async fn fetch_same_origin_and_follow_allowed_redirect() {
    let server = MockServer::start().await;
    Mock::given(path("/getnzb/1"))
        .and(query_param("apikey", KEY))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/files/1.nzb"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/files/1.nzb"))
        .respond_with(ResponseTemplate::new(200).set_body_string(NZB))
        .expect(1)
        .mount(&server)
        .await;
    let client = NewznabClient::new(config(&server)).unwrap();
    let body = client
        .fetch_nzb(&release(&format!("{}/getnzb/1?apikey={KEY}", server.uri())))
        .await
        .unwrap();
    assert_eq!(body, NZB.as_bytes());
}

#[tokio::test]
async fn fetch_rejects_off_list_and_downgrade_redirects_and_unsafe_start() {
    let server = MockServer::start().await;
    Mock::given(path("/off"))
        .respond_with(ResponseTemplate::new(302).insert_header(
            "location",
            "https://elsewhere.example.com/x?apikey=fixture-only-key",
        ))
        .mount(&server)
        .await;
    Mock::given(path("/downgrade"))
        .respond_with(
            ResponseTemplate::new(301).insert_header("location", "http://dl.example.net/x"),
        )
        .mount(&server)
        .await;
    Mock::given(path("/nolocation"))
        .respond_with(ResponseTemplate::new(302))
        .mount(&server)
        .await;
    let mut cfg = config(&server);
    cfg.extra_download_hosts = vec!["dl.example.net".into()];
    let client = NewznabClient::new(cfg).unwrap();
    for (p, expected) in [
        ("/off", IndexerError::UnsafeDownloadUrl),
        ("/downgrade", IndexerError::UnsafeDownloadUrl),
        ("/nolocation", IndexerError::Http { status: 302 }),
    ] {
        let error = client
            .fetch_nzb(&release(&format!("{}{p}?apikey={KEY}", server.uri())))
            .await
            .unwrap_err();
        assert_eq!(error, expected, "{p}");
        assert!(!format!("{error} {error:?}").contains(KEY));
    }
    // The initial URL is checked too: Release fields are public.
    for start in [
        "https://elsewhere.example.com/x",
        "http://dl.example.net/x",
        "not a url",
    ] {
        assert_eq!(
            client.fetch_nzb(&release(start)).await.unwrap_err(),
            IndexerError::UnsafeDownloadUrl
        );
    }
}

#[tokio::test]
async fn fetch_redirect_cap() {
    let server = MockServer::start().await;
    Mock::given(path("/loop"))
        .respond_with(ResponseTemplate::new(307).insert_header("location", "/loop"))
        .expect(MAX_DOWNLOAD_REDIRECTS as u64 + 1)
        .mount(&server)
        .await;
    let client = NewznabClient::new(config(&server)).unwrap();
    assert_eq!(
        client
            .fetch_nzb(&release(&format!("{}/loop", server.uri())))
            .await
            .unwrap_err(),
        IndexerError::TooManyRedirects
    );
}

#[tokio::test]
async fn fetch_size_cap_errors_and_nzb_sanity() {
    let server = MockServer::start().await;
    Mock::given(path("/big"))
        .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(2048)))
        .mount(&server)
        .await;
    Mock::given(path("/html"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html><body>login</body></html>"))
        .mount(&server)
        .await;
    Mock::given(path("/error"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            "<error code='100' description='Incorrect user credentials {KEY}'/>"
        )))
        .mount(&server)
        .await;
    Mock::given(path("/gone"))
        .respond_with(ResponseTemplate::new(404).set_body_string(NZB))
        .mount(&server)
        .await;
    Mock::given(path("/limited"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "30"))
        .mount(&server)
        .await;
    let mut cfg = config(&server);
    cfg.max_response_bytes = 1024;
    let client = NewznabClient::new(cfg).unwrap();
    let fetch = async |p: &str| {
        client
            .fetch_nzb(&release(&format!("{}{p}", server.uri())))
            .await
    };
    assert_eq!(
        fetch("/big").await.unwrap_err(),
        IndexerError::ResponseTooLarge
    );
    assert_eq!(fetch("/html").await.unwrap_err(), IndexerError::InvalidNzb);
    let error = fetch("/error").await.unwrap_err();
    assert_eq!(error, IndexerError::BadApiKey);
    assert!(!format!("{error} {error:?}").contains(KEY));
    assert_eq!(
        fetch("/gone").await.unwrap_err(),
        IndexerError::Http { status: 404 }
    );
    assert!(matches!(
        fetch("/limited").await.unwrap_err(),
        IndexerError::RateLimited { .. }
    ));
    // The cooldown is shared with search traffic on the same client.
    assert!(matches!(
        fetch("/html").await.unwrap_err(),
        IndexerError::RateLimited { .. }
    ));

    let mut disabled = config(&server);
    disabled.enabled = false;
    let client = NewznabClient::new(disabled).unwrap();
    assert_eq!(
        client
            .fetch_nzb(&release(&format!("{}/html", server.uri())))
            .await
            .unwrap_err(),
        IndexerError::Disabled
    );
}
