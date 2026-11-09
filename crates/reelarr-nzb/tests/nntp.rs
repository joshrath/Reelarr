//! Independent, locally scripted RFC conversations. All certificates and keys
//! are generated in memory per mock, never downloaded or committed.
use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use reelarr_nzb::{
    Segment,
    nntp::{Credentials, Error, Pool, Secret, ServerConfig},
};
use rustls::{RootCertStore, ServerConfig as TlsConfig, pki_types::PrivatePkcs8KeyDer};
use std::{
    collections::VecDeque,
    io::{self, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::Notify,
    task::{JoinHandle, JoinSet},
    time::{sleep, timeout},
};
use tokio_rustls::TlsAcceptor;

const USER: &str = "private-username-92";
const PASS: &str = "private-password-71";
const ID: &str = "part@example.test";
const BODY: &[u8] = b"first\r\n..leading-dot\r\n\r\nlast\r\n";
fn segment() -> Segment {
    Segment {
        message_id: ID.into(),
        number: 1,
        bytes: u64::MAX,
    }
}

#[derive(Clone)]
enum Reply {
    Body(Vec<u8>),
    NotFound,
    DropBody,
    Hang,
    Drip,
    Raw(Vec<u8>),
    Temporary,
}
struct Options {
    greeting: Vec<u8>,
    user_code: u16,
    pass_code: u16,
    greeting_delay: Duration,
    body_delay: Duration,
    replies: Vec<Reply>,
    cert_name: String,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            greeting: b"201 reader ready\r\n".to_vec(),
            user_code: 381,
            pass_code: 281,
            greeting_delay: Duration::ZERO,
            body_delay: Duration::ZERO,
            replies: vec![],
            cert_name: "localhost".into(),
        }
    }
}
#[derive(Default)]
struct Stats {
    accepted: AtomicUsize,
    commands: AtomicUsize,
    users: AtomicUsize,
    passwords: AtomicUsize,
    bodies: AtomicUsize,
    active_bodies: AtomicUsize,
    peak_bodies: AtomicUsize,
    quits: AtomicUsize,
    body_started: Notify,
}
struct Mock {
    port: u16,
    roots: RootCertStore,
    stats: Arc<Stats>,
    task: JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Mock {
    async fn start(options: Options) -> Self {
        let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca_key = KeyPair::generate().unwrap();
        let ca_cert = ca.self_signed(&ca_key).unwrap();
        let issuer = Issuer::new(ca, ca_key);
        let mut leaf = CertificateParams::new(vec![options.cert_name.clone()]).unwrap();
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
        let stats = Arc::new(Stats::default());
        let task_stats = stats.clone();
        let replies = Arc::new(Mutex::new(VecDeque::from(options.replies.clone())));
        let options = Arc::new(options);
        let task = tokio::spawn(async move {
            let acceptor = TlsAcceptor::from(Arc::new(tls));
            let mut children = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (tcp, _) = accepted.unwrap();
                        task_stats.accepted.fetch_add(1, Ordering::SeqCst);
                        let (acceptor, options, stats, replies) = (acceptor.clone(), options.clone(), task_stats.clone(), replies.clone());
                        children.spawn(async move {
                            let Ok(stream) = acceptor.accept(tcp).await else { return; };
                            let mut stream = BufReader::new(stream);
                            sleep(options.greeting_delay).await;
                            if stream.get_mut().write_all(&options.greeting).await.is_err() { return; }
                            loop {
                                let mut line = String::new();
                                if !matches!(stream.read_line(&mut line).await, Ok(n) if n > 0) { return; }
                                stats.commands.fetch_add(1, Ordering::SeqCst);
                                let response = if line.starts_with("AUTHINFO USER ") {
                                    assert_eq!(line, format!("AUTHINFO USER {USER}\r\n"));
                                    stats.users.fetch_add(1, Ordering::SeqCst);
                                    format!("{} echoed {USER} {PASS}\r\n", options.user_code).into_bytes()
                                } else if line.starts_with("AUTHINFO PASS ") {
                                    assert_eq!(line, format!("AUTHINFO PASS {PASS}\r\n"));
                                    stats.passwords.fetch_add(1, Ordering::SeqCst);
                                    format!("{} echoed {USER} {PASS}\r\n", options.pass_code).into_bytes()
                                } else if line.starts_with("BODY ") {
                                    assert_eq!(line, format!("BODY <{ID}>\r\n"));
                                    stats.bodies.fetch_add(1, Ordering::SeqCst);
                                    let active = stats.active_bodies.fetch_add(1, Ordering::SeqCst)+1;
                                    stats.peak_bodies.fetch_max(active, Ordering::SeqCst);
                                    stats.body_started.notify_one();
                                    sleep(options.body_delay).await;
                                    let reply = replies.lock().unwrap().pop_front().unwrap_or(Reply::Body(BODY.to_vec()));
                                    let header = format!("222 0 <{ID}> echoed {USER} {PASS}\r\n");
                                    let response = match reply {
                                        Reply::Body(body) => [header.as_bytes(), &body, b".\r\n"].concat(),
                                        Reply::NotFound => format!("430 absent {USER} {PASS}\r\n").into_bytes(),
                                        Reply::Raw(bytes) => bytes,
                                        Reply::Temporary => b"400 service discontinued\r\n".to_vec(),
                                        Reply::DropBody => {
                                            let _ = stream.get_mut().write_all(&[header.as_bytes(), b"partial\r\n"].concat()).await;
                                            stats.active_bodies.fetch_sub(1, Ordering::SeqCst);
                                            return;
                                        }
                                        Reply::Hang => {
                                            let _ = stream.get_mut().write_all(header.as_bytes()).await;
                                            // Wait for the client to close; avoids sleep-only leaks.
                                            let _ = stream.read_line(&mut String::new()).await;
                                            stats.active_bodies.fetch_sub(1, Ordering::SeqCst);
                                            return;
                                        }
                                        Reply::Drip => {
                                            let _ = stream.get_mut().write_all(header.as_bytes()).await;
                                            for _ in 0..30 {
                                                sleep(Duration::from_millis(15)).await;
                                                if stream.get_mut().write_all(b"x\r\n").await.is_err() { break; }
                                            }
                                            stats.active_bodies.fetch_sub(1, Ordering::SeqCst);
                                            return;
                                        }
                                    };
                                    stats.active_bodies.fetch_sub(1, Ordering::SeqCst);
                                    response
                                } else if line == format!("STAT <{ID}>\r\n") {
                                    format!("223 0 <{ID}> exists\r\n").into_bytes()
                                } else if line == "QUIT\r\n" {
                                    stats.quits.fetch_add(1, Ordering::SeqCst);
                                    let _ = stream.get_mut().write_all(b"205 bye\r\n").await;
                                    let _ = stream.get_mut().shutdown().await;
                                    return;
                                } else { panic!("unexpected client command"); };
                                if stream.get_mut().write_all(&response).await.is_err() { return; }
                            }
                        });
                    }
                    Some(result) = children.join_next(), if !children.is_empty() => { result.unwrap(); }
                }
            }
        });
        Self {
            port,
            roots,
            stats,
            task,
        }
    }
    fn config(&self) -> ServerConfig {
        let mut config = ServerConfig::new("localhost");
        config.port = self.port;
        config.max_connections = 2;
        config.timeouts.connect = Duration::from_secs(2);
        config.timeouts.io = Duration::from_secs(2);
        config.timeouts.operation = Duration::from_secs(5);
        config.timeouts.acquire = Duration::from_secs(1);
        config.retry.max_retries = 0;
        config.retry.initial_backoff = Duration::from_millis(10);
        config.retry.max_backoff = Duration::from_millis(40);
        config
    }
    fn pool(&self, config: ServerConfig) -> Pool {
        Pool::with_root_certificates(config, self.roots.clone()).unwrap()
    }
    fn assert_healthy(&self) {
        assert!(!self.task.is_finished(), "mock task panicked");
    }
}
fn credentials() -> Credentials {
    Credentials {
        username: Secret::new(USER),
        password: Secret::new(PASS),
    }
}

#[tokio::test]
async fn verified_tls_auth_body_stat_reuse_and_quit() {
    let mock = Mock::start(Options::default()).await;
    let mut config = mock.config();
    config.credentials = Some(credentials());
    let pool = mock.pool(config);
    let article = pool.body(&segment()).await.unwrap();
    assert_eq!(article.as_bytes(), BODY);
    assert!(!format!("{article:?}").contains("first"));
    assert_eq!(article.into_bytes(), BODY);
    pool.stat(&segment()).await.unwrap();
    assert_eq!(pool.body(&segment()).await.unwrap().as_bytes(), BODY);
    assert_eq!(mock.stats.accepted.load(Ordering::SeqCst), 1);
    assert_eq!(mock.stats.users.load(Ordering::SeqCst), 1);
    assert_eq!(mock.stats.passwords.load(Ordering::SeqCst), 1);
    pool.shutdown().await;
    assert_eq!(mock.stats.quits.load(Ordering::SeqCst), 1);
    assert!(matches!(pool.body(&segment()).await, Err(Error::Closed)));
    mock.assert_healthy();
}

#[tokio::test]
async fn greeting_200_user_only_auth_and_empty_body() {
    let mock = Mock::start(Options {
        greeting: b"200 reader ready\r\n".to_vec(),
        user_code: 281,
        replies: vec![Reply::Body(Vec::new())],
        ..Options::default()
    })
    .await;
    let mut config = mock.config();
    config.credentials = Some(credentials());
    let pool = mock.pool(config);
    assert!(pool.body(&segment()).await.unwrap().as_bytes().is_empty());
    assert_eq!(mock.stats.passwords.load(Ordering::SeqCst), 0);
    mock.assert_healthy();
}

#[tokio::test]
async fn auth_failure_codes_do_not_retry_or_continue_exchange() {
    for (user_code, pass_code, expected, passes) in [
        (481, 281, 481, 0),
        (482, 281, 482, 0),
        (502, 281, 502, 0),
        (381, 481, 481, 1),
        (381, 482, 482, 1),
        (381, 502, 502, 1),
    ] {
        let mock = Mock::start(Options {
            user_code,
            pass_code,
            ..Options::default()
        })
        .await;
        let mut config = mock.config();
        config.credentials = Some(credentials());
        config.retry.max_retries = 3;
        let error = mock.pool(config).body(&segment()).await.unwrap_err();
        assert!(matches!(error, Error::Authentication { code } if code == expected));
        assert!(!error.is_retryable());
        for text in [format!("{error}"), format!("{error:?}")] {
            assert!(!text.contains(USER));
            assert!(!text.contains(PASS));
        }
        assert_eq!(mock.stats.accepted.load(Ordering::SeqCst), 1);
        assert_eq!(mock.stats.passwords.load(Ordering::SeqCst), passes);
        assert_eq!(mock.stats.bodies.load(Ordering::SeqCst), 0);
        mock.assert_healthy();
    }
}

#[tokio::test]
async fn missing_article_is_typed_not_retried_and_connection_stays_usable() {
    let mock = Mock::start(Options {
        replies: vec![Reply::NotFound],
        ..Options::default()
    })
    .await;
    let mut config = mock.config();
    config.retry.max_retries = 3;
    let pool = mock.pool(config);
    assert!(matches!(pool.body(&segment()).await, Err(Error::NotFound)));
    assert_eq!(mock.stats.bodies.load(Ordering::SeqCst), 1);
    pool.stat(&segment()).await.unwrap();
    assert_eq!(pool.body(&segment()).await.unwrap().as_bytes(), BODY);
    assert_eq!(mock.stats.accepted.load(Ordering::SeqCst), 1);
    mock.assert_healthy();
}

#[tokio::test]
async fn article_and_line_limits_drop_socket_without_retry() {
    for (reply, article_limit, line_limit, expected) in [
        (
            Reply::Body(b"1234\r\n5678\r\n".to_vec()),
            10,
            8192,
            "article bytes",
        ),
        (Reply::Body(vec![b'x'; 600]), 8192, 512, "line bytes"),
        (Reply::Raw(vec![b'x'; 600]), 8192, 8192, "line bytes"),
    ] {
        let mock = Mock::start(Options {
            replies: vec![reply],
            ..Options::default()
        })
        .await;
        let mut config = mock.config();
        config.retry.max_retries = 3;
        config.limits.max_article_bytes = article_limit;
        config.limits.max_line_bytes = line_limit;
        let pool = mock.pool(config);
        assert!(matches!(pool.body(&segment()).await, Err(Error::Limit(what)) if what == expected));
        assert_eq!(mock.stats.bodies.load(Ordering::SeqCst), 1);
        // Limits do not poison the pool itself: next request uses a fresh socket.
        pool.stat(&segment()).await.unwrap();
        assert_eq!(mock.stats.accepted.load(Ordering::SeqCst), 2);
        mock.assert_healthy();
    }
}

#[tokio::test]
async fn strict_framing_and_article_identity() {
    for reply in [
        b"garbage\r\n".to_vec(),
        b"222 0 <wrong@test>\r\n".to_vec(),
        format!("222 0 <{ID}>\r\n.bad\r\n.\r\n").into_bytes(),
        format!("222 0 <{ID}>\r\nline\n").into_bytes(),
        format!("222 0 <{ID}>\r\nline\0bad\r\n").into_bytes(),
    ] {
        let mock = Mock::start(Options {
            replies: vec![Reply::Raw(reply)],
            ..Options::default()
        })
        .await;
        let mut config = mock.config();
        config.retry.max_retries = 3;
        let pool = mock.pool(config);
        assert!(matches!(
            pool.body(&segment()).await,
            Err(Error::Protocol(_))
        ));
        assert_eq!(mock.stats.bodies.load(Ordering::SeqCst), 1);
        pool.stat(&segment()).await.unwrap();
        assert_eq!(mock.stats.accepted.load(Ordering::SeqCst), 2);
        mock.assert_healthy();
    }
}

#[tokio::test]
async fn invalid_greeting_stops_before_auth() {
    for greeting in [
        b"502 unavailable\r\n".to_vec(),
        b"not NNTP\r\n".to_vec(),
        vec![b'x'; 600],
    ] {
        let mock = Mock::start(Options {
            greeting,
            ..Options::default()
        })
        .await;
        let mut config = mock.config();
        config.credentials = Some(credentials());
        assert!(mock.pool(config).body(&segment()).await.is_err());
        assert_eq!(mock.stats.commands.load(Ordering::SeqCst), 0);
        mock.assert_healthy();
    }
}

#[tokio::test]
async fn tls_rejects_untrusted_ca_and_wrong_hostname_before_credentials() {
    let trusted = Mock::start(Options::default()).await;
    let untrusted = Mock::start(Options::default()).await;
    let wrong_name = Mock::start(Options {
        cert_name: "other.example".into(),
        ..Options::default()
    })
    .await;
    for (mock, roots) in [
        (&untrusted, trusted.roots.clone()),
        (&wrong_name, wrong_name.roots.clone()),
    ] {
        let mut config = mock.config();
        config.credentials = Some(credentials());
        config.retry.max_retries = 3;
        let pool = Pool::with_root_certificates(config, roots).unwrap();
        assert!(matches!(pool.body(&segment()).await, Err(Error::Tls)));
        assert_eq!(mock.stats.accepted.load(Ordering::SeqCst), 1);
        assert_eq!(mock.stats.commands.load(Ordering::SeqCst), 0);
        mock.assert_healthy();
    }
}

#[tokio::test]
async fn greeting_body_and_whole_operation_deadlines() {
    let mock = Mock::start(Options {
        greeting_delay: Duration::from_millis(300),
        ..Options::default()
    })
    .await;
    let mut config = mock.config();
    config.timeouts.io = Duration::from_millis(80);
    assert!(matches!(
        mock.pool(config).body(&segment()).await,
        Err(Error::Timeout("read"))
    ));

    let mock = Mock::start(Options {
        replies: vec![Reply::Hang],
        ..Options::default()
    })
    .await;
    let mut config = mock.config();
    config.timeouts.io = Duration::from_millis(80);
    let pool = mock.pool(config);
    assert!(matches!(
        pool.body(&segment()).await,
        Err(Error::Timeout("read"))
    ));
    pool.stat(&segment()).await.unwrap();
    assert_eq!(mock.stats.accepted.load(Ordering::SeqCst), 2);

    let mock = Mock::start(Options {
        replies: vec![Reply::Drip],
        ..Options::default()
    })
    .await;
    let mut config = mock.config();
    config.timeouts.io = Duration::from_millis(200);
    config.timeouts.operation = Duration::from_millis(180);
    let pool = mock.pool(config);
    assert!(matches!(
        pool.body(&segment()).await,
        Err(Error::Timeout("operation"))
    ));
    pool.stat(&segment()).await.unwrap();
    assert_eq!(mock.stats.accepted.load(Ordering::SeqCst), 2);
    mock.assert_healthy();
}

#[tokio::test]
async fn handshake_timeout_is_bounded() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock = Mock::start(Options::default()).await;
    let mut config = mock.config();
    config.port = listener.local_addr().unwrap().port();
    config.timeouts.connect = Duration::from_millis(100);
    let peer = tokio::spawn(async move {
        let (_tcp, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
    });
    assert!(matches!(
        mock.pool(config).body(&segment()).await,
        Err(Error::Timeout("connect"))
    ));
    peer.abort();
}

#[tokio::test]
async fn dropped_body_and_transient_status_retry_with_backoff_and_fresh_socket() {
    for reply in [Reply::DropBody, Reply::Temporary] {
        let mock = Mock::start(Options {
            replies: vec![reply],
            ..Options::default()
        })
        .await;
        let mut config = mock.config();
        config.retry.max_retries = 2;
        config.retry.initial_backoff = Duration::from_millis(60);
        config.retry.max_backoff = Duration::from_millis(100);
        let start = Instant::now();
        assert_eq!(
            mock.pool(config).body(&segment()).await.unwrap().as_bytes(),
            BODY
        );
        assert!(start.elapsed() >= Duration::from_millis(60));
        assert_eq!(mock.stats.bodies.load(Ordering::SeqCst), 2);
        assert_eq!(mock.stats.accepted.load(Ordering::SeqCst), 2);
        mock.assert_healthy();
    }
}

#[tokio::test]
async fn retry_budget_and_global_deadline_bound_failures() {
    let mock = Mock::start(Options {
        replies: vec![Reply::DropBody; 10],
        ..Options::default()
    })
    .await;
    let mut config = mock.config();
    config.retry.max_retries = 2;
    let error = mock.pool(config).body(&segment()).await.unwrap_err();
    assert!(error.is_retryable());
    assert_eq!(mock.stats.bodies.load(Ordering::SeqCst), 3);
    let mock = Mock::start(Options {
        replies: vec![Reply::Temporary; 10],
        ..Options::default()
    })
    .await;
    let mut config = mock.config();
    config.retry.max_retries = 10;
    config.retry.initial_backoff = Duration::from_secs(1);
    config.retry.max_backoff = Duration::from_secs(1);
    config.timeouts.operation = Duration::from_millis(150);
    assert!(matches!(
        mock.pool(config).body(&segment()).await,
        Err(Error::Timeout("operation"))
    ));
    assert_eq!(mock.stats.bodies.load(Ordering::SeqCst), 1);
    mock.assert_healthy();
}

#[tokio::test]
async fn concurrency_is_limited_and_connections_are_reused() {
    let mock = Mock::start(Options {
        body_delay: Duration::from_millis(40),
        ..Options::default()
    })
    .await;
    let pool = mock.pool(mock.config());
    let mut tasks = JoinSet::new();
    for _ in 0..12 {
        let pool = pool.clone();
        tasks.spawn(async move { pool.body(&segment()).await.unwrap() });
    }
    while let Some(result) = tasks.join_next().await {
        assert_eq!(result.unwrap().as_bytes(), BODY);
    }
    assert_eq!(mock.stats.accepted.load(Ordering::SeqCst), 2);
    assert_eq!(mock.stats.peak_bodies.load(Ordering::SeqCst), 2);
    assert_eq!(mock.stats.bodies.load(Ordering::SeqCst), 12);
    mock.assert_healthy();
}

#[tokio::test]
async fn acquire_timeout_and_cancellation_release_slots_but_not_partial_socket() {
    let mock = Mock::start(Options {
        replies: vec![Reply::Hang],
        ..Options::default()
    })
    .await;
    let mut config = mock.config();
    config.max_connections = 1;
    config.timeouts.acquire = Duration::from_millis(80);
    let pool = mock.pool(config);
    let p = pool.clone();
    let task = tokio::spawn(async move { p.body(&segment()).await });
    timeout(Duration::from_secs(2), mock.stats.body_started.notified())
        .await
        .unwrap();
    assert!(matches!(
        pool.body(&segment()).await,
        Err(Error::Timeout("acquire"))
    ));
    assert_eq!(mock.stats.accepted.load(Ordering::SeqCst), 1);
    task.abort();
    let _ = task.await;
    assert_eq!(pool.body(&segment()).await.unwrap().as_bytes(), BODY);
    assert_eq!(mock.stats.accepted.load(Ordering::SeqCst), 2);
    mock.assert_healthy();
}

#[tokio::test]
async fn shutdown_wakes_waiters_and_never_reuses_busy_connection() {
    let mock = Mock::start(Options {
        replies: vec![Reply::Hang],
        ..Options::default()
    })
    .await;
    let mut config = mock.config();
    config.max_connections = 1;
    let pool = mock.pool(config);
    let p = pool.clone();
    let busy = tokio::spawn(async move { p.body(&segment()).await });
    timeout(Duration::from_secs(2), mock.stats.body_started.notified())
        .await
        .unwrap();
    let p = pool.clone();
    let waiting = tokio::spawn(async move { p.body(&segment()).await });
    pool.shutdown().await;
    assert!(matches!(waiting.await.unwrap(), Err(Error::Closed)));
    busy.abort();
    let _ = busy.await;
    assert!(matches!(pool.stat(&segment()).await, Err(Error::Closed)));
    mock.assert_healthy();
}

#[tokio::test]
async fn direct_segment_and_config_injection_rejected_before_network() {
    let mock = Mock::start(Options::default()).await;
    let pool = mock.pool(mock.config());
    for id in [
        "bad\r\nQUIT",
        "<bad@test>",
        "bad id",
        "über@test",
        "",
        &"x".repeat(249),
    ] {
        let mut s = segment();
        s.message_id = id.into();
        assert!(matches!(
            pool.body(&s).await,
            Err(Error::Config("message ID"))
        ));
    }
    for value in ["bad\r\nQUIT", "bad space", "", &"x".repeat(497)] {
        let mut config = mock.config();
        config.credentials = Some(Credentials {
            username: Secret::new(USER),
            password: Secret::new(value),
        });
        assert!(matches!(
            Pool::with_root_certificates(config, mock.roots.clone()),
            Err(Error::Config(_))
        ));
    }
    let mut config = mock.config();
    config.max_connections = 0;
    assert!(matches!(
        Pool::with_root_certificates(config, mock.roots.clone()),
        Err(Error::Config(_))
    ));
    let mut config = mock.config();
    config.timeouts.io = Duration::ZERO;
    assert!(matches!(
        Pool::with_root_certificates(config, mock.roots.clone()),
        Err(Error::Config(_))
    ));
    let mut config = mock.config();
    config.host = "host\r\nQUIT".into();
    assert!(matches!(
        Pool::with_root_certificates(config, mock.roots.clone()),
        Err(Error::Config(_))
    ));
    assert!(matches!(
        Pool::with_root_certificates(mock.config(), RootCertStore::empty()),
        Err(Error::TrustRoots)
    ));
    assert_eq!(mock.stats.accepted.load(Ordering::SeqCst), 0);
    mock.assert_healthy();
}

#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}
#[tokio::test(flavor = "current_thread")]
async fn credentials_and_server_echoes_never_escape_diagnostics_or_trace() {
    let capture = Capture(Arc::new(Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(capture.clone())
        .finish();
    // Global in this integration-test binary: avoids callsite-interest races
    // against concurrent tests with no thread-local subscriber.
    tracing::subscriber::set_global_default(subscriber).unwrap();
    let mock = Mock::start(Options {
        replies: vec![Reply::Temporary, Reply::NotFound],
        ..Options::default()
    })
    .await;
    let mut config = mock.config();
    config.credentials = Some(credentials());
    config.retry.max_retries = 1;
    let debug = format!(
        "{config:?} {} {:?}",
        config.credentials.as_ref().unwrap().password,
        Secret::new(PASS)
    );
    let error = mock.pool(config).body(&segment()).await.unwrap_err();
    let diagnostics = format!("{debug} {error} {error:?}");
    let trace = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(trace.contains("verified NNTP connection ready"));
    assert!(trace.contains("retrying transient NNTP failure"));
    for text in [&diagnostics, &trace] {
        assert!(!text.contains(USER));
        assert!(!text.contains(PASS));
        assert!(!text.contains(ID));
    }
    mock.assert_healthy();
}

#[tokio::test]
async fn tls_body_feeds_yenc_with_exactly_one_dot_unstuff() {
    use reelarr_nzb::yenc::{BodyEncoding, DecodeLimits, Decoder};
    // Decoded 0x04 becomes encoded '.', requiring wire '..'. 0xd6 becomes
    // encoded 0x00 and must be escaped; include an ordinary non-UTF-8 byte too.
    let decoded = [4_u8, 214, 128];
    let mut body = b"=ybegin line=128 size=3 name=transport.bin\r\n..=\x40\xaa\r\n".to_vec();
    body.extend_from_slice(
        format!("=yend size=3 crc32={:08x}\r\n", crc32fast::hash(&decoded)).as_bytes(),
    );
    let mock = Mock::start(Options {
        replies: vec![Reply::Body(body.clone())],
        ..Options::default()
    })
    .await;
    let article = mock.pool(mock.config()).body(&segment()).await.unwrap();
    assert_eq!(article.as_bytes(), body);
    let mut decoder = Decoder::new(
        Vec::new(),
        DecodeLimits::default(),
        BodyEncoding::DotStuffed,
    );
    // Fragment independently of TLS records and physical NNTP lines.
    for chunk in article.as_bytes().chunks(3) {
        decoder.push(chunk).unwrap();
    }
    let (output, _) = decoder.finish().unwrap();
    assert_eq!(output, decoded);
    mock.assert_healthy();
}

#[tokio::test]
async fn probe_authenticates_fetches_no_article_and_quits() {
    let mock = Mock::start(Options::default()).await;
    let mut config = mock.config();
    config.credentials = Some(credentials());
    let pool = mock.pool(config);
    pool.probe().await.unwrap();
    // A second probe opens a fresh socket: probe connections are never pooled.
    pool.probe().await.unwrap();
    assert_eq!(mock.stats.accepted.load(Ordering::SeqCst), 2);
    assert_eq!(mock.stats.users.load(Ordering::SeqCst), 2);
    assert_eq!(mock.stats.passwords.load(Ordering::SeqCst), 2);
    assert_eq!(mock.stats.bodies.load(Ordering::SeqCst), 0);
    assert_eq!(mock.stats.quits.load(Ordering::SeqCst), 2);
    // users + passwords + quits only: no BODY/STAT/GROUP.
    assert_eq!(mock.stats.commands.load(Ordering::SeqCst), 6);
    mock.assert_healthy();
}

#[tokio::test]
async fn probe_reports_auth_failure_without_credentials_in_diagnostics() {
    let mock = Mock::start(Options {
        pass_code: 481,
        ..Options::default()
    })
    .await;
    let mut config = mock.config();
    config.credentials = Some(credentials());
    config.retry.max_retries = 3;
    let error = mock.pool(config).probe().await.unwrap_err();
    assert!(matches!(error, Error::Authentication { code: 481 }));
    for text in [format!("{error}"), format!("{error:?}")] {
        assert!(!text.contains(USER) && !text.contains(PASS));
    }
    assert_eq!(
        mock.stats.accepted.load(Ordering::SeqCst),
        1,
        "never retried"
    );
    assert_eq!(mock.stats.bodies.load(Ordering::SeqCst), 0);
    mock.assert_healthy();
}

#[tokio::test]
async fn probe_rejects_untrusted_certificate_before_credentials() {
    let trusted = Mock::start(Options::default()).await;
    let untrusted = Mock::start(Options::default()).await;
    let mut config = untrusted.config();
    config.credentials = Some(credentials());
    let pool = Pool::with_root_certificates(config, trusted.roots.clone()).unwrap();
    assert!(matches!(pool.probe().await, Err(Error::Tls)));
    assert_eq!(untrusted.stats.commands.load(Ordering::SeqCst), 0);
    untrusted.assert_healthy();
}

#[tokio::test]
async fn probe_is_bounded_by_connect_timeout_in_total() {
    // Each greeting line arrives within the io budget's reach, but the whole
    // exchange must still finish inside the connect timeout.
    let mock = Mock::start(Options {
        greeting_delay: Duration::from_millis(400),
        ..Options::default()
    })
    .await;
    let mut config = mock.config();
    config.credentials = Some(credentials());
    config.timeouts.connect = Duration::from_millis(150);
    config.timeouts.io = Duration::from_secs(2);
    let started = Instant::now();
    assert!(matches!(
        mock.pool(config).probe().await,
        Err(Error::Timeout(_))
    ));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(mock.stats.users.load(Ordering::SeqCst), 0);
    mock.assert_healthy();
}

#[tokio::test]
async fn probe_after_shutdown_is_closed() {
    let mock = Mock::start(Options::default()).await;
    let pool = mock.pool(mock.config());
    pool.shutdown().await;
    assert!(matches!(pool.probe().await, Err(Error::Closed)));
    assert_eq!(mock.stats.accepted.load(Ordering::SeqCst), 0);
}
