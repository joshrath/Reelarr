//! NNTP provider settings: validation, write-only encrypted password, persistence.
use std::{sync::Arc, time::Duration};

use reelarr_db::{
    Database, NntpSettingsError, SqliteNntpStore,
    secrets::{SecretCipher, XChaChaSecretCipher},
};
use reelarr_types::nntp::{NntpServerUpdate, PasswordUpdate};

const USER: &str = "nntp-user-4f1e";
const PASSWORD: &str = "nntp-password-never-in-db-or-logs-93ac";
const OTHER_PASSWORD: &str = "replacement-password-7d20b1";

fn cipher() -> Arc<dyn SecretCipher> {
    Arc::new(XChaChaSecretCipher::new([61; 32]))
}
fn update() -> NntpServerUpdate {
    let mut u = NntpServerUpdate::new("news.example.com");
    u.username = Some(USER.into());
    u.password = PasswordUpdate::Set(PASSWORD.into());
    u
}
fn password_of(server: &reelarr_db::NntpServer) -> Option<String> {
    server
        .config
        .credentials
        .as_ref()
        .map(|c| c.password.expose().to_owned())
}
fn assert_no_plaintext(bytes: &[u8]) {
    for secret in [PASSWORD, OTHER_PASSWORD] {
        assert!(
            !bytes.windows(secret.len()).any(|w| w == secret.as_bytes()),
            "plaintext password found"
        );
    }
}
fn assert_files_have_no_secret(dir: &std::path::Path) {
    for file in std::fs::read_dir(dir).unwrap() {
        match std::fs::read(file.unwrap().path()) {
            Ok(bytes) => assert_no_plaintext(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => panic!("read database file: {e}"),
        }
    }
}

#[tokio::test]
async fn invalid_settings_are_rejected_without_writing_or_echoing() {
    let db = Database::connect(":memory:").await.unwrap();
    let store = SqliteNntpStore::new(&db, cipher());
    type Mutate = Box<dyn Fn(&mut NntpServerUpdate)>;
    let cases: Vec<(&str, Mutate)> = vec![
        ("empty host", Box::new(|u| u.host = "  ".into())),
        ("long host", Box::new(|u| u.host = "a".repeat(254))),
        (
            "host with space",
            Box::new(|u| u.host = "news .example".into()),
        ),
        (
            "host with port",
            Box::new(|u| u.host = "news.example:563".into()),
        ),
        (
            "host url",
            Box::new(|u| u.host = "nntps://news.example".into()),
        ),
        ("port 0", Box::new(|u| u.port = 0)),
        ("plaintext port", Box::new(|u| u.port = 119)),
        ("zero connections", Box::new(|u| u.max_connections = 0)),
        (
            "too many connections",
            Box::new(|u| u.max_connections = 101),
        ),
        (
            "zero connect timeout",
            Box::new(|u| u.connect_timeout_secs = 0),
        ),
        ("huge io timeout", Box::new(|u| u.io_timeout_secs = 301)),
        ("too many retries", Box::new(|u| u.max_retries = 11)),
        (
            "username with space",
            Box::new(|u| u.username = Some(format!("{USER} x"))),
        ),
        (
            "empty username",
            Box::new(|u| u.username = Some(String::new())),
        ),
        (
            "password with space",
            Box::new(|u| u.password = PasswordUpdate::Set(format!("{PASSWORD} x"))),
        ),
        (
            "password with CRLF",
            Box::new(|u| u.password = PasswordUpdate::Set(format!("{PASSWORD}\r\nGROUP x"))),
        ),
        (
            "non-ascii password",
            Box::new(|u| u.password = PasswordUpdate::Set(format!("{PASSWORD}é"))),
        ),
        (
            "overlong password",
            Box::new(|u| {
                u.password = PasswordUpdate::Set(format!("{PASSWORD}{}", "x".repeat(500)))
            }),
        ),
        (
            "empty password",
            Box::new(|u| u.password = PasswordUpdate::Set(String::new())),
        ),
        ("password without username", Box::new(|u| u.username = None)),
        (
            "username without password",
            Box::new(|u| u.password = PasswordUpdate::Clear),
        ),
    ];
    for (name, mutate) in cases {
        let mut u = update();
        mutate(&mut u);
        let error = store.save(&u).await.expect_err(name);
        assert!(
            matches!(error, NntpSettingsError::Invalid(_)),
            "{name}: {error:?}"
        );
        let text = format!("{error} {error:?}");
        assert!(!text.contains(PASSWORD) && !text.contains(USER), "{name}");
        assert!(store.get().await.unwrap().is_none(), "{name} wrote a row");
        assert!(store.resolve(&u).await.is_err(), "{name} resolves");
    }
    // Keep with nothing stored and a username: the password must be entered.
    let mut keep = update();
    keep.password = PasswordUpdate::Keep;
    assert!(matches!(
        store.save(&keep).await,
        Err(NntpSettingsError::PasswordRequired(_))
    ));
    // Anonymous providers are allowed; IPv4/IPv6 literals are valid hosts.
    for host in ["news.example.com", "192.0.2.10", "2001:db8::1"] {
        let mut anon = NntpServerUpdate::new(host);
        anon.password = PasswordUpdate::Clear;
        let saved = store.save(&anon).await.unwrap();
        assert!(!saved.has_password && saved.username.is_none());
    }
    db.pool().close().await;
}

#[tokio::test]
async fn password_unchanged_replaced_and_cleared() {
    let db = Database::connect(":memory:").await.unwrap();
    let store = SqliteNntpStore::new(&db, cipher());
    let saved = store.save(&update()).await.unwrap();
    assert!(saved.has_password);
    assert_eq!(saved.username.as_deref(), Some(USER));
    assert_eq!(saved.host, "news.example.com");
    let first: Vec<u8> = sqlx::query_scalar("SELECT password_ciphertext FROM nntp_server")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_no_plaintext(&first);

    // Keep: other fields change, ciphertext is byte-for-byte unchanged.
    let mut keep = update();
    keep.password = PasswordUpdate::Keep;
    keep.max_connections = 20;
    keep.port = 443;
    keep.host = "NEWS.example.com".into(); // same host, different case
    let saved = store.save(&keep).await.unwrap();
    assert!(saved.has_password);
    assert_eq!((saved.max_connections, saved.port), (20, 443));
    let kept: Vec<u8> = sqlx::query_scalar("SELECT password_ciphertext FROM nntp_server")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(kept, first);
    let loaded = store.load().await.unwrap().unwrap();
    assert_eq!(password_of(&loaded).as_deref(), Some(PASSWORD));
    assert_eq!(loaded.config.max_connections, 20);

    // Keep with a different host is refused and changes nothing.
    let mut moved = keep.clone();
    moved.host = "attacker.example".into();
    assert!(matches!(
        store.save(&moved).await,
        Err(NntpSettingsError::PasswordRequired(_))
    ));
    assert!(matches!(
        store.resolve(&moved).await,
        Err(NntpSettingsError::PasswordRequired(_))
    ));
    assert_eq!(store.get().await.unwrap().unwrap().host, "news.example.com");

    // Replace: new ciphertext, new value.
    let mut replace = keep.clone();
    replace.password = PasswordUpdate::Set(OTHER_PASSWORD.into());
    store.save(&replace).await.unwrap();
    let replaced: Vec<u8> = sqlx::query_scalar("SELECT password_ciphertext FROM nntp_server")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_ne!(replaced, first);
    assert_no_plaintext(&replaced);
    let loaded = store.load().await.unwrap().unwrap();
    assert_eq!(password_of(&loaded).as_deref(), Some(OTHER_PASSWORD));

    // Clear (together with the username): no ciphertext remains.
    let mut clear = keep.clone();
    clear.username = None;
    clear.password = PasswordUpdate::Clear;
    let saved = store.save(&clear).await.unwrap();
    assert!(!saved.has_password);
    let none: Option<Vec<u8>> = sqlx::query_scalar("SELECT password_ciphertext FROM nntp_server")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(none.is_none());
    assert!(
        store
            .load()
            .await
            .unwrap()
            .unwrap()
            .config
            .credentials
            .is_none()
    );

    // Resolve never writes.
    let candidate = store.resolve(&update()).await.unwrap();
    assert_eq!(password_of(&candidate).as_deref(), Some(PASSWORD));
    assert!(!store.get().await.unwrap().unwrap().has_password);

    assert!(store.clear().await.unwrap());
    assert!(!store.clear().await.unwrap());
    assert!(store.get().await.unwrap().is_none());
    assert!(store.load().await.unwrap().is_none());
    db.pool().close().await;
}

#[tokio::test]
async fn persists_across_reopen_without_plaintext_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reelarr.db");
    let db = Database::connect(&path).await.unwrap();
    let store = SqliteNntpStore::new(&db, cipher());
    let mut u = update();
    u.enabled = false;
    u.connect_timeout_secs = 7;
    u.io_timeout_secs = 9;
    u.max_retries = 4;
    u.max_connections = 30;
    store.save(&u).await.unwrap();
    let first: Vec<u8> = sqlx::query_scalar("SELECT password_ciphertext FROM nntp_server")
        .fetch_one(db.pool())
        .await
        .unwrap();
    store.save(&u).await.unwrap();
    let second: Vec<u8> = sqlx::query_scalar("SELECT password_ciphertext FROM nntp_server")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_ne!(first, second, "a fresh nonce is required per write");
    let json: String = sqlx::query_scalar("SELECT settings_json FROM nntp_server")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(!json.contains("password"));
    assert_files_have_no_secret(dir.path());
    db.pool().close().await;
    assert_files_have_no_secret(dir.path());

    let db = Database::connect(&path).await.unwrap();
    let store = SqliteNntpStore::new(&db, cipher());
    let summary = store.get().await.unwrap().unwrap();
    assert!(!summary.enabled && summary.has_password);
    assert_eq!(
        (
            summary.connect_timeout_secs,
            summary.io_timeout_secs,
            summary.max_retries,
            summary.max_connections
        ),
        (7, 9, 4, 30)
    );
    let json = serde_json::to_string(&summary).unwrap();
    assert!(!json.contains(PASSWORD) && !json.contains("ciphertext"));
    let server = store.load().await.unwrap().unwrap();
    assert!(!server.enabled);
    assert_eq!(server.config.port, 563);
    assert_eq!(server.config.timeouts.connect, Duration::from_secs(7));
    assert_eq!(server.config.timeouts.io, Duration::from_secs(9));
    assert_eq!(server.config.retry.max_retries, 4);
    assert_eq!(password_of(&server).as_deref(), Some(PASSWORD));
    assert_eq!(
        server
            .config
            .credentials
            .as_ref()
            .unwrap()
            .username
            .expose(),
        USER
    );

    // Wrong key: full reads fail closed; API-safe reads still work.
    let wrong = SqliteNntpStore::new(&db, Arc::new(XChaChaSecretCipher::new([7; 32])));
    assert_eq!(wrong.load().await.unwrap_err(), NntpSettingsError::Store);
    assert!(wrong.get().await.unwrap().unwrap().has_password);
    db.pool().close().await;
}

#[tokio::test]
async fn corrupt_rows_fail_closed() {
    let db = Database::connect(":memory:").await.unwrap();
    let store = SqliteNntpStore::new(&db, cipher());
    store.save(&update()).await.unwrap();
    // Ciphertext for a different record/field does not decrypt here.
    let foreign = XChaChaSecretCipher::new([61; 32])
        .encrypt(b"reelarr:indexers:api_key:x", PASSWORD)
        .unwrap();
    sqlx::query("UPDATE nntp_server SET password_ciphertext = ?")
        .bind(foreign)
        .execute(db.pool())
        .await
        .unwrap();
    assert_eq!(store.load().await.unwrap_err(), NntpSettingsError::Store);
    store.save(&update()).await.unwrap();
    // Out-of-band invalid settings are not trusted.
    sqlx::query(
        "UPDATE nntp_server SET settings_json = json_set(settings_json, '$.max_connections', 0)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    assert_eq!(store.load().await.unwrap_err(), NntpSettingsError::Store);
    sqlx::query("UPDATE nntp_server SET settings_json = json_set(settings_json, '$.version', 9)")
        .execute(db.pool())
        .await
        .unwrap();
    assert_eq!(store.get().await.unwrap_err(), NntpSettingsError::Store);
    // Only one provider row can exist.
    assert!(
        sqlx::query("INSERT INTO nntp_server (id, settings_json) VALUES (2, '{}')")
            .execute(db.pool())
            .await
            .is_err()
    );
    db.pool().close().await;
}

#[tokio::test]
async fn concurrent_saves_never_mix_password_and_settings() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::connect(dir.path().join("reelarr.db"))
        .await
        .unwrap();
    let store = SqliteNntpStore::new(&db, cipher());
    let mut tasks = Vec::new();
    for i in 0..8u16 {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            let mut u = update();
            u.max_connections = i + 1;
            u.password = PasswordUpdate::Set(format!("secret-{}", i + 1));
            for _ in 0..5 {
                store.save(&u).await.unwrap();
                let s = store.load().await.unwrap().unwrap();
                let n = s.config.max_connections;
                assert_eq!(password_of(&s).unwrap(), format!("secret-{n}"));
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    db.pool().close().await;
}

#[tokio::test]
async fn diagnostics_and_sql_tracing_contain_no_secrets() {
    #[derive(Clone)]
    struct Sink(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let output = bytes.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || Sink(output.clone()))
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    tracing::info!("nntp diagnostics capture active");
    let db = Database::connect(":memory:").await.unwrap();
    let store = SqliteNntpStore::new(&db, cipher());
    let u = update();
    let summary = store.save(&u).await.unwrap();
    let server = store.load().await.unwrap().unwrap();
    let mut bad = update();
    bad.password = PasswordUpdate::Set(format!("{PASSWORD} with space"));
    let error = store.save(&bad).await.unwrap_err();
    let diagnostics = format!("{store:?} {u:?} {bad:?} {summary:?} {server:?} {error} {error:?}");
    assert!(!diagnostics.contains(PASSWORD));
    assert!(!diagnostics.contains(USER));
    assert!(diagnostics.contains("REDACTED"));
    db.pool().close().await;
    let logs = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("nntp diagnostics capture active"));
    assert!(!logs.contains(PASSWORD));
    assert!(
        !logs.contains("INSERT INTO nntp_server"),
        "statement logging is disabled"
    );
}
