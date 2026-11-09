//! NNTP settings API: auth/CSRF, write-only password, validation and
//! test-connection against a local implicit-TLS mock with an in-memory CA.
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
    response::Response,
};
use http_body_util::BodyExt;
use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use reelarr_nzb::nntp::RootCertStore;
use reelarr_server::{
    app,
    state::{AppState, NntpTrust},
};
use reelarr_types::{
    ApiError, SessionInfo,
    nntp::{NntpServerSettings, NntpTestOutcome, NntpTestResult},
};
use rustls::{ServerConfig as TlsConfig, pki_types::PrivatePkcs8KeyDer};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    task::JoinHandle,
};
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt;

const USER: &str = "api-user-5b2c";
const PASSWORD: &str = "api-password-never-returned-or-logged-e81f";
const NEW_PASSWORD: &str = "rotated-password-never-returned-0c3d";

struct Mock {
    port: u16,
    roots: RootCertStore,
    password: String,
    commands: Arc<AtomicUsize>,
    articles: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
fn ca() -> (rcgen::Certificate, Issuer<'static, KeyPair>) {
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let key = KeyPair::generate().unwrap();
    let cert = ca.self_signed(&key).unwrap();
    (cert, Issuer::new(ca, key))
}
impl Mock {
    async fn start(password: &str, greeting_delay: Duration) -> Self {
        let (ca_cert, issuer) = ca();
        let mut leaf = CertificateParams::new(vec!["localhost".into()]).unwrap();
        leaf.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let leaf_key = KeyPair::generate().unwrap();
        let leaf_cert = leaf.signed_by(&leaf_key, &issuer).unwrap();
        let tls =
            TlsConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![leaf_cert.der().clone()],
                    PrivatePkcs8KeyDer::from(leaf_key.serialize_der()).into(),
                )
                .unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(ca_cert.der().clone()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let commands = Arc::new(AtomicUsize::new(0));
        let articles = Arc::new(AtomicUsize::new(0));
        let (c, a, expected) = (commands.clone(), articles.clone(), password.to_owned());
        let task = tokio::spawn(async move {
            let acceptor = TlsAcceptor::from(Arc::new(tls));
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                let (acceptor, c, a, expected) =
                    (acceptor.clone(), c.clone(), a.clone(), expected.clone());
                tokio::spawn(async move {
                    let Ok(stream) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let mut stream = BufReader::new(stream);
                    tokio::time::sleep(greeting_delay).await;
                    if stream
                        .get_mut()
                        .write_all(b"200 mock ready\r\n")
                        .await
                        .is_err()
                    {
                        return;
                    }
                    loop {
                        let mut line = String::new();
                        if !matches!(stream.read_line(&mut line).await, Ok(n) if n > 0) {
                            return;
                        }
                        c.fetch_add(1, Ordering::SeqCst);
                        let reply: &[u8] = if line == format!("AUTHINFO USER {USER}\r\n") {
                            b"381 more\r\n"
                        } else if line.starts_with("AUTHINFO USER ") {
                            b"481 no\r\n"
                        } else if line == format!("AUTHINFO PASS {expected}\r\n") {
                            b"281 ok\r\n"
                        } else if line.starts_with("AUTHINFO PASS ") {
                            b"481 rejected\r\n"
                        } else if line == "QUIT\r\n" {
                            let _ = stream.get_mut().write_all(b"205 bye\r\n").await;
                            return;
                        } else {
                            a.fetch_add(1, Ordering::SeqCst);
                            b"500 unexpected\r\n"
                        };
                        if stream.get_mut().write_all(reply).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Self {
            port,
            roots,
            password: password.into(),
            commands,
            articles,
            task,
        }
    }
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    cookie: &str,
    csrf: Option<&str>,
    body: &str,
) -> Response {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = csrf {
        req = req.header("x-csrf-token", token);
    }
    app.clone()
        .oneshot(req.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap()
}
async fn text(resp: Response) -> String {
    String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap()
}
fn cookies(resp: &Response) -> String {
    resp.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|h| h.to_str().unwrap().split(';').next().unwrap().to_owned())
        .collect::<Vec<_>>()
        .join("; ")
}
/// Returns (cookie header, csrf token) for a signed-in admin.
async fn sign_in(app: &Router) -> (String, String) {
    let resp = call(app, "GET", "/api/v1/auth/session", "", None, "").await;
    let guest = cookies(&resp);
    let info: SessionInfo = serde_json::from_str(&text(resp).await).unwrap();
    let resp = call(
        app,
        "POST",
        "/api/v1/auth/setup",
        &guest,
        Some(&info.csrf_token),
        r#"{"username":"admin","password":"a-long-password"}"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let cookie = cookies(&resp);
    let info: SessionInfo = serde_json::from_str(&text(resp).await).unwrap();
    (cookie, info.csrf_token)
}
async fn setup(trust: NntpTrust) -> (tempfile::TempDir, Router, String, String) {
    let tmp = tempfile::tempdir().unwrap();
    let db = reelarr_db::Database::connect(tmp.path().join("reelarr.db"))
        .await
        .unwrap();
    let mut state = AppState::new(db);
    state.nntp_trust = trust;
    let router = app(state);
    let (cookie, csrf) = sign_in(&router).await;
    (tmp, router, cookie, csrf)
}
fn settings_json(
    host: &str,
    port: u16,
    password: serde_json::Value,
    extra: serde_json::Value,
) -> String {
    let mut v = serde_json::json!({
        "host": host, "port": port, "username": USER, "password": password,
        "connect_timeout_secs": 2, "io_timeout_secs": 2,
    });
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    v.to_string()
}
fn set(p: &str) -> serde_json::Value {
    serde_json::json!({"action": "set", "value": p})
}
fn keep() -> serde_json::Value {
    serde_json::json!({"action": "keep"})
}
fn no_secret(s: &str) {
    assert!(
        !s.contains(PASSWORD) && !s.contains(NEW_PASSWORD),
        "secret leaked: {s}"
    );
}

#[derive(Clone)]
struct Sink(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn endpoints_require_session_and_csrf() {
    let (_tmp, app, cookie, csrf) = setup(NntpTrust::Native).await;
    let body = settings_json(
        "news.example.com",
        563,
        set(PASSWORD),
        serde_json::json!({}),
    );
    for (method, uri) in [
        ("GET", "/api/v1/settings/nntp"),
        ("PUT", "/api/v1/settings/nntp"),
        ("DELETE", "/api/v1/settings/nntp"),
        ("POST", "/api/v1/settings/nntp/test"),
    ] {
        let resp = call(&app, method, uri, "", None, &body).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{method} {uri}");
        if method != "GET" {
            let resp = call(&app, method, uri, &cookie, None, &body).await;
            assert_eq!(
                resp.status(),
                StatusCode::FORBIDDEN,
                "{method} {uri} without CSRF"
            );
            let resp = call(&app, method, uri, &cookie, Some("wrong-token"), &body).await;
            assert_eq!(
                resp.status(),
                StatusCode::FORBIDDEN,
                "{method} {uri} bad CSRF"
            );
        }
    }
    // Nothing was saved by the rejected requests.
    let resp = call(&app, "GET", "/api/v1/settings/nntp", &cookie, None, "").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let e: ApiError = serde_json::from_str(&text(resp).await).unwrap();
    assert_eq!(e.code, "not_configured");
    let resp = call(
        &app,
        "PUT",
        "/api/v1/settings/nntp",
        &cookie,
        Some(&csrf),
        &body,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn password_is_write_only_and_can_be_kept_replaced_or_cleared() {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let output = bytes.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || Sink(output.clone()))
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    tracing::info!("nntp api capture active");

    let (tmp, app, cookie, csrf) = setup(NntpTrust::Native).await;
    let put = |body: String| {
        let (app, cookie, csrf) = (app.clone(), cookie.clone(), csrf.clone());
        async move {
            call(
                &app,
                "PUT",
                "/api/v1/settings/nntp",
                &cookie,
                Some(&csrf),
                &body,
            )
            .await
        }
    };
    let get = || {
        let (app, cookie) = (app.clone(), cookie.clone());
        async move { call(&app, "GET", "/api/v1/settings/nntp", &cookie, None, "").await }
    };

    let resp = put(settings_json(
        "news.example.com",
        563,
        set(PASSWORD),
        serde_json::json!({"max_connections": 12}),
    ))
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = text(resp).await;
    no_secret(&body);
    assert!(!body.contains("\"password\""));
    let saved: NntpServerSettings = serde_json::from_str(&body).unwrap();
    assert!(saved.has_password && saved.enabled);
    assert_eq!(saved.username.as_deref(), Some(USER));
    assert_eq!(saved.max_connections, 12);
    let body = text(get().await).await;
    no_secret(&body);
    assert_eq!(
        serde_json::from_str::<NntpServerSettings>(&body).unwrap(),
        saved
    );

    // Unchanged: password omitted entirely means keep.
    let resp = put(
        serde_json::json!({"host": "news.example.com", "username": USER, "enabled": false})
            .to_string(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let s: NntpServerSettings = serde_json::from_str(&text(resp).await).unwrap();
    assert!(s.has_password && !s.enabled);
    // Keep against a different host is refused.
    let resp = put(settings_json(
        "evil.example.com",
        563,
        keep(),
        serde_json::json!({}),
    ))
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let e: ApiError = serde_json::from_str(&text(resp).await).unwrap();
    assert_eq!(e.code, "password_required");
    // Replace.
    let resp = put(settings_json(
        "news.example.com",
        563,
        set(NEW_PASSWORD),
        serde_json::json!({}),
    ))
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    no_secret(&text(resp).await);
    // Clear (with the username).
    let resp = put(serde_json::json!({"host": "news.example.com", "username": null, "password": {"action": "clear"}}).to_string()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let s: NntpServerSettings = serde_json::from_str(&text(resp).await).unwrap();
    assert!(!s.has_password && s.username.is_none());

    // Invalid settings: rejected, static messages, no echo.
    for body in [
        settings_json(
            "news.example.com",
            119,
            set(PASSWORD),
            serde_json::json!({}),
        ),
        settings_json("news.example.com", 0, set(PASSWORD), serde_json::json!({})),
        settings_json("bad host", 563, set(PASSWORD), serde_json::json!({})),
        settings_json(
            "news.example.com",
            563,
            set(&format!("{PASSWORD} x")),
            serde_json::json!({}),
        ),
        settings_json(
            "news.example.com",
            563,
            set(PASSWORD),
            serde_json::json!({"max_connections": 1000}),
        ),
        settings_json(
            "news.example.com",
            563,
            set(PASSWORD),
            serde_json::json!({"connect_timeout_secs": 0}),
        ),
        settings_json(
            "news.example.com",
            563,
            set(PASSWORD),
            serde_json::json!({"max_retries": 50}),
        ),
    ] {
        let resp = put(body).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = text(resp).await;
        no_secret(&body);
        assert_eq!(
            serde_json::from_str::<ApiError>(&body).unwrap().code,
            "invalid_settings"
        );
    }
    // Malformed bodies: serde's message is never returned.
    for body in [
        format!(r#"{{"host":"h","password":"{PASSWORD}"}}"#),
        format!(r#"{{"host":"h","password":{{"action":"{PASSWORD}"}}}}"#),
        format!(r#"{{"host":"h","{PASSWORD}":1}}"#),
        format!(r#"{{"host":"h","port":70000,"username":"{USER}"}}"#),
    ] {
        let resp = put(body).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = text(resp).await;
        no_secret(&body);
        assert_eq!(
            serde_json::from_str::<ApiError>(&body).unwrap().code,
            "invalid_body"
        );
    }
    // Still the last good state.
    assert!(
        !serde_json::from_str::<NntpServerSettings>(&text(get().await).await)
            .unwrap()
            .has_password
    );

    let resp = call(
        &app,
        "DELETE",
        "/api/v1/settings/nntp",
        &cookie,
        Some(&csrf),
        "",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(get().await.status(), StatusCode::NOT_FOUND);

    // Database files never contain either password.
    for file in std::fs::read_dir(tmp.path()).unwrap() {
        if let Ok(bytes) = std::fs::read(file.unwrap().path()) {
            for secret in [PASSWORD, NEW_PASSWORD] {
                assert!(!bytes.windows(secret.len()).any(|w| w == secret.as_bytes()));
            }
        }
    }
    let logs = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("nntp api capture active"));
    assert!(logs.contains("NNTP settings saved"));
    no_secret(&logs);
}

async fn probe(app: &Router, cookie: &str, csrf: &str, body: String) -> (StatusCode, String) {
    let resp = call(
        app,
        "POST",
        "/api/v1/settings/nntp/test",
        cookie,
        Some(csrf),
        &body,
    )
    .await;
    let status = resp.status();
    let body = text(resp).await;
    no_secret(&body);
    (status, body)
}
fn result(body: &str) -> NntpTestResult {
    serde_json::from_str(body).unwrap()
}

#[tokio::test]
async fn test_connection_trusted_untrusted_auth_failure_and_keep() {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let output = bytes.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || Sink(output.clone()))
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let mock = Mock::start(PASSWORD, Duration::ZERO).await;
    let (_tmp, app, cookie, csrf) = setup(NntpTrust::Roots(Arc::new(mock.roots.clone()))).await;

    // Trusted CA + right password: connected, no article command sent, nothing saved.
    let (status, body) = probe(
        &app,
        &cookie,
        &csrf,
        settings_json("localhost", mock.port, set(PASSWORD), serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let r = result(&body);
    assert!(r.ok, "{body}");
    assert_eq!(r.outcome, NntpTestOutcome::Connected);
    assert_eq!(mock.articles.load(Ordering::SeqCst), 0);
    assert_eq!(
        mock.commands.load(Ordering::SeqCst),
        3,
        "USER, PASS, QUIT only"
    );
    let resp = call(&app, "GET", "/api/v1/settings/nntp", &cookie, None, "").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "test must not save");

    // Wrong password.
    let (_, body) = probe(
        &app,
        &cookie,
        &csrf,
        settings_json(
            "localhost",
            mock.port,
            set(NEW_PASSWORD),
            serde_json::json!({}),
        ),
    )
    .await;
    let r = result(&body);
    assert!(!r.ok);
    assert_eq!(r.outcome, NntpTestOutcome::AuthRejected);

    // Save, then test with keep: the stored (encrypted) password is used.
    let resp = call(
        &app,
        "PUT",
        "/api/v1/settings/nntp",
        &cookie,
        Some(&csrf),
        &settings_json(
            "localhost",
            mock.port,
            set(mock.password.as_str()),
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let (_, body) = probe(
        &app,
        &cookie,
        &csrf,
        settings_json("localhost", mock.port, keep(), serde_json::json!({})),
    )
    .await;
    assert_eq!(result(&body).outcome, NntpTestOutcome::Connected);
    // Keep against a different host is refused before any connection.
    let before = mock.commands.load(Ordering::SeqCst);
    let (status, body) = probe(
        &app,
        &cookie,
        &csrf,
        settings_json("127.0.0.1", mock.port, keep(), serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        serde_json::from_str::<ApiError>(&body).unwrap().code,
        "password_required"
    );
    assert_eq!(mock.commands.load(Ordering::SeqCst), before);
    // Invalid candidate: 400, no connection.
    let (status, body) = probe(
        &app,
        &cookie,
        &csrf,
        settings_json("localhost", 119, set(PASSWORD), serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        serde_json::from_str::<ApiError>(&body).unwrap().code,
        "invalid_settings"
    );

    // Untrusted: the server's CA is not in this state's roots.
    let other = Mock::start(PASSWORD, Duration::ZERO).await;
    let (_, body) = probe(
        &app,
        &cookie,
        &csrf,
        settings_json(
            "localhost",
            other.port,
            set(PASSWORD),
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(result(&body).outcome, NntpTestOutcome::TlsRejected);
    assert_eq!(
        other.commands.load(Ordering::SeqCst),
        0,
        "no credentials before TLS verifies"
    );

    // Unreachable: closed port.
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let (_, body) = probe(
        &app,
        &cookie,
        &csrf,
        settings_json("localhost", port, set(PASSWORD), serde_json::json!({})),
    )
    .await;
    let r = result(&body);
    assert!(matches!(r.outcome, NntpTestOutcome::Unreachable), "{body}");

    let logs = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    no_secret(&logs);
    assert!(!logs.contains(USER), "username stays out of logs");
}

#[tokio::test]
async fn test_connection_is_bounded_by_connect_timeout() {
    let mock = Mock::start(PASSWORD, Duration::from_secs(10)).await;
    let (_tmp, app, cookie, csrf) = setup(NntpTrust::Roots(Arc::new(mock.roots.clone()))).await;
    let started = Instant::now();
    let (status, body) = probe(
        &app,
        &cookie,
        &csrf,
        settings_json(
            "localhost",
            mock.port,
            set(PASSWORD),
            serde_json::json!({"connect_timeout_secs": 1, "io_timeout_secs": 30}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let r = result(&body);
    assert_eq!(r.outcome, NntpTestOutcome::Timeout);
    assert!(!r.ok);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
}
