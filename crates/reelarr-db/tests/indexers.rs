use std::{sync::Arc, time::Duration};

use reelarr_db::{
    Database, SqliteIndexerStore,
    secrets::{SecretCipher, SecretError, XChaChaSecretCipher},
};
use reelarr_indexer::{
    InMemoryIndexerStore, IndexerConfig, IndexerError, IndexerStore, SecretString, Url,
};
use sqlx::Row;

const API_KEY: &str = "indexer-secret-never-in-db-or-diagnostics-8b7c21";
fn config(name: &str) -> IndexerConfig {
    IndexerConfig::new(
        name,
        Url::parse("https://indexer.example/").unwrap(),
        SecretString::new(API_KEY),
    )
}
fn cipher() -> Arc<dyn SecretCipher> {
    Arc::new(XChaChaSecretCipher::new([47; 32]))
}
fn assert_config(actual: &IndexerConfig, expected: &IndexerConfig) {
    assert_eq!(actual.name, expected.name);
    assert_eq!(actual.base_url, expected.base_url);
    assert_eq!(actual.api_path, expected.api_path);
    assert_eq!(actual.api_key, expected.api_key);
    assert_eq!(actual.categories, expected.categories);
    assert_eq!(actual.enabled, expected.enabled);
    assert_eq!(actual.allow_http, expected.allow_http);
    assert_eq!(actual.extra_download_hosts, expected.extra_download_hosts);
    assert_eq!(actual.connect_timeout, expected.connect_timeout);
    assert_eq!(actual.request_timeout, expected.request_timeout);
    assert_eq!(actual.minimum_interval, expected.minimum_interval);
    assert_eq!(actual.caps_ttl, expected.caps_ttl);
    assert_eq!(actual.max_response_bytes, expected.max_response_bytes);
    assert_eq!(actual.max_pages, expected.max_pages);
    assert_eq!(actual.max_results, expected.max_results);
}

// Exactly the same behavioral suite exercises both adapters, not separate copies.
async fn behavior(store: &dyn IndexerStore) {
    assert!(store.list().await.unwrap().is_empty());
    assert!(store.get("missing").await.unwrap().is_none());
    assert!(!store.remove("missing").await.unwrap());
    let empty = config("z-empty");
    store.upsert(empty.clone()).await.unwrap();
    assert_config(&store.get(&empty.name).await.unwrap().unwrap(), &empty);

    let mut c = config("a-雪'\"; DROP TABLE indexers; --");
    c.extra_download_hosts = vec![
        "z-cdn.example".into(),
        "xn--bcher-kva.example".into(),
        "a-cdn.example".into(),
    ];
    c.api_path = "/newznab/api".into();
    c.categories = vec![2040, 2000];
    c.enabled = false;
    c.allow_http = true;
    c.base_url = Url::parse("http://private.example:8080/base/").unwrap();
    c.connect_timeout = Duration::new(2, 123);
    c.request_timeout = Duration::new(13, 456);
    c.minimum_interval = Duration::new(1, 789);
    c.caps_ttl = Duration::new(33, 12345);
    c.max_response_bytes = 123456;
    c.max_pages = 7;
    c.max_results = 151;
    store.upsert(c.clone()).await.unwrap();
    let listed = store.list().await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_config(&listed[0], &c);
    assert_config(&listed[1], &empty);
    assert_config(&store.get(&c.name).await.unwrap().unwrap(), &c);

    let invalid_lists: Vec<Vec<String>> = vec![
        vec!["CDN.example".into()],
        vec!["cdn.example.".into()],
        vec![" cdn.example".into()],
        vec!["bücher.example".into()],
        vec!["https://cdn.example".into()],
        vec!["cdn.example:443".into()],
        vec!["*.example".into()],
        vec!["cdn.example/path".into()],
        vec!["user@cdn.example".into()],
        vec!["127.0.0.1".into()],
        vec!["[::1]".into()],
        vec!["localhost".into()],
        vec!["cdn.example\n".into()],
        vec!["cdn.example".into(), "cdn.example".into()],
        (0..17).map(|i| format!("cdn{i}.example")).collect(),
    ];
    for hosts in invalid_lists {
        // Neither a new row nor a partial update to an existing row may survive.
        let mut invalid = c.clone();
        invalid.name = "rejected".into();
        invalid.extra_download_hosts = hosts.clone();
        assert_eq!(
            store.upsert(invalid).await.unwrap_err(),
            IndexerError::InvalidConfig
        );
        assert!(store.get("rejected").await.unwrap().is_none());
        let mut invalid = c.clone();
        invalid.extra_download_hosts = hosts;
        invalid.enabled = true;
        invalid.api_key = SecretString::new("replacement-that-must-not-be-written");
        assert_eq!(
            store.upsert(invalid).await.unwrap_err(),
            IndexerError::InvalidConfig
        );
        assert_config(&store.get(&c.name).await.unwrap().unwrap(), &c);
        assert_eq!(store.list().await.unwrap().len(), 2);
    }
    let mut invalid = config("bad-general-config");
    invalid.api_key = SecretString::new("");
    assert_eq!(
        store.upsert(invalid).await.unwrap_err(),
        IndexerError::InvalidConfig
    );
    assert!(store.get("bad-general-config").await.unwrap().is_none());

    // Host URL policy stays owned by reelarr-indexer, not reinterpreted by storage.
    let endpoint = c.base_url.join(&c.api_path).unwrap();
    assert!(c.download_url_allowed(
        &endpoint,
        &Url::parse("https://z-cdn.example:443/file").unwrap()
    ));
    for url in [
        "http://z-cdn.example/file",
        "https://z-cdn.example:444/file",
        "https://sub.z-cdn.example/file",
        "https://cdn.example/file",
    ] {
        assert!(!c.download_url_allowed(&endpoint, &Url::parse(url).unwrap()));
    }
    c.extra_download_hosts.clear();
    c.api_key = SecretString::new("new-key-on-upsert");
    store.upsert(c.clone()).await.unwrap();
    assert_config(&store.get(&c.name).await.unwrap().unwrap(), &c);
    c.extra_download_hosts = (0..16).map(|i| format!("cdn{i}.example")).collect();
    store.upsert(c.clone()).await.unwrap();
    assert_config(&store.get(&c.name).await.unwrap().unwrap(), &c);
    assert!(store.remove(&c.name).await.unwrap());
    assert!(!store.remove(&c.name).await.unwrap());
    assert!(store.get(&c.name).await.unwrap().is_none());
    assert!(store.remove(&empty.name).await.unwrap());
    assert!(store.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn shared_behavior_in_memory() {
    behavior(&InMemoryIndexerStore::default()).await;
}
#[tokio::test]
async fn shared_behavior_sqlite() {
    let db = Database::connect(":memory:").await.unwrap();
    behavior(&SqliteIndexerStore::new(&db, cipher())).await;
    db.pool().close().await;
}

#[tokio::test]
async fn persists_on_reopen_and_no_plaintext_in_database_or_wal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reelarr.db");
    let db = Database::connect(&path).await.unwrap();
    let store = SqliteIndexerStore::new(&db, cipher());
    let mut c = config("persistent");
    c.extra_download_hosts = vec!["z.example".into(), "a.example".into()];
    store.upsert(c.clone()).await.unwrap();
    let first: Vec<u8> = sqlx::query_scalar("SELECT api_key_ciphertext FROM indexers")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_ne!(first, API_KEY.as_bytes());
    assert!(
        !first
            .windows(API_KEY.len())
            .any(|w| w == API_KEY.as_bytes())
    );
    store.upsert(c.clone()).await.unwrap();
    let second: Vec<u8> = sqlx::query_scalar("SELECT api_key_ciphertext FROM indexers")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_ne!(first, second, "a fresh nonce is required per write");
    let hosts: String = sqlx::query_scalar("SELECT extra_download_hosts FROM indexers")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(hosts, "[\"z.example\",\"a.example\"]");
    assert_files_have_no_secret(dir.path());
    db.pool().close().await;
    assert_files_have_no_secret(dir.path());

    let db = Database::connect(&path).await.unwrap();
    let store = SqliteIndexerStore::new(&db, cipher());
    assert_config(&store.get("persistent").await.unwrap().unwrap(), &c);
    let summaries = store.list_summaries().await.unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].name, "persistent");
    assert!(summaries[0].has_api_key);
    assert_eq!(summaries[0].extra_download_hosts, c.extra_download_hosts);
    let serialized = serde_json::to_string(&summaries).unwrap();
    assert!(!serialized.contains(API_KEY));
    assert!(!serialized.contains("\"api_key\":"));
    assert!(!serialized.contains("ciphertext"));
    assert!(store.get_summary("absent").await.unwrap().is_none());

    let wrong = SqliteIndexerStore::new(&db, Arc::new(XChaChaSecretCipher::new([13; 32])));
    assert_eq!(
        wrong.get("persistent").await.unwrap_err(),
        IndexerError::Store
    );
    assert_eq!(wrong.list().await.unwrap_err(), IndexerError::Store);
    // API-safe reads do not need or decrypt the key, even if the configured key is wrong.
    assert!(
        wrong
            .get_summary("persistent")
            .await
            .unwrap()
            .unwrap()
            .has_api_key
    );
    db.pool().close().await;
}
fn assert_files_have_no_secret(dir: &std::path::Path) {
    for file in std::fs::read_dir(dir).unwrap() {
        // SQLite may delete a transient -wal/-shm/-journal file between the
        // directory listing and the read; a vanished file holds no secret.
        let bytes = match std::fs::read(file.unwrap().path()) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => panic!("read database file: {e}"),
        };
        assert!(
            !bytes
                .windows(API_KEY.len())
                .any(|w| w == API_KEY.as_bytes())
        );
    }
}

struct RejectEncryption;
impl SecretCipher for RejectEncryption {
    fn encrypt(&self, _: &[u8], _: &str) -> Result<Vec<u8>, SecretError> {
        Err(SecretError::Encrypt)
    }
    fn decrypt(&self, _: &[u8], _: &[u8]) -> Result<SecretString, SecretError> {
        Err(SecretError::Decrypt)
    }
}
#[tokio::test]
async fn encryption_failure_leaves_no_partial_write_and_sql_failure_is_redacted() {
    let db = Database::connect(":memory:").await.unwrap();
    let store = SqliteIndexerStore::new(&db, cipher());
    store.upsert(config("existing")).await.unwrap();
    let broken = SqliteIndexerStore::new(&db, Arc::new(RejectEncryption));
    assert_eq!(
        broken.upsert(config("absent")).await.unwrap_err(),
        IndexerError::Store
    );
    assert_eq!(
        broken.upsert(config("existing")).await.unwrap_err(),
        IndexerError::Store
    );
    assert!(store.get("absent").await.unwrap().is_none());
    assert_eq!(
        store
            .get("existing")
            .await
            .unwrap()
            .unwrap()
            .api_key
            .expose(),
        API_KEY
    );
    sqlx::query("DROP TABLE indexers")
        .execute(db.pool())
        .await
        .unwrap();
    let error = store.upsert(config("fail")).await.unwrap_err();
    assert_eq!(error, IndexerError::Store);
    assert!(!format!("{error} {error:?} {store:?}").contains(API_KEY));
    db.pool().close().await;
}

#[tokio::test]
async fn ciphertext_is_bound_to_record_and_corrupt_rows_fail_closed() {
    let db = Database::connect(":memory:").await.unwrap();
    let store = SqliteIndexerStore::new(&db, cipher());
    store.upsert(config("one")).await.unwrap();
    store.upsert(config("two")).await.unwrap();
    sqlx::query("UPDATE indexers SET api_key_ciphertext = (SELECT api_key_ciphertext FROM indexers WHERE name = 'one') WHERE name = 'two'")
        .execute(db.pool()).await.unwrap();
    assert_eq!(store.get("two").await.unwrap_err(), IndexerError::Store);
    sqlx::query("UPDATE indexers SET extra_download_hosts = '[\"*.example\"]' WHERE name = 'one'")
        .execute(db.pool())
        .await
        .unwrap();
    assert_eq!(store.get("one").await.unwrap_err(), IndexerError::Store);
    db.pool().close().await;
}

#[tokio::test]
async fn concurrent_reads_never_observe_mixed_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::connect(dir.path().join("reelarr.db"))
        .await
        .unwrap();
    let store = SqliteIndexerStore::new(&db, cipher());
    let mut tasks = Vec::new();
    for i in 0..8 {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            let mut c = config("shared");
            c.api_key = SecretString::new(format!("secret-{i}"));
            c.api_path = format!("/api/{i}");
            c.extra_download_hosts = vec![format!("cdn{i}.example")];
            for _ in 0..8 {
                store.upsert(c.clone()).await.unwrap();
                let read = store.get("shared").await.unwrap().unwrap();
                let id = read.api_path.strip_prefix("/api/").unwrap();
                assert_eq!(read.api_key.expose(), format!("secret-{id}"));
                assert_eq!(read.extra_download_hosts, vec![format!("cdn{id}.example")]);
                for read in store.list().await.unwrap() {
                    let id = read.api_path.strip_prefix("/api/").unwrap();
                    assert_eq!(read.api_key.expose(), format!("secret-{id}"));
                }
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let row = sqlx::query("SELECT COUNT(*) AS count FROM indexers")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(row.get::<i64, _>("count"), 1);
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
    tracing::info!("indexer diagnostics capture active");
    let db = Database::connect(":memory:").await.unwrap();
    let store = SqliteIndexerStore::new(&db, cipher());
    let c = config("redacted");
    store.upsert(c.clone()).await.unwrap();
    let summary = store.get_summary("redacted").await.unwrap().unwrap();
    let mut bad = c.clone();
    bad.extra_download_hosts = vec![API_KEY.into()];
    let error = store.upsert(bad).await.unwrap_err();
    let diagnostics = format!(
        "{store:?} {c:?} {summary:?} {error} {error:?} {:?}",
        XChaChaSecretCipher::new([47; 32])
    );
    assert!(!diagnostics.contains(API_KEY));
    assert!(diagnostics.contains("REDACTED"));
    db.pool().close().await;
    let logs = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("indexer diagnostics capture active"));
    assert!(!logs.contains(API_KEY));
    assert!(
        !logs.contains("INSERT INTO indexers"),
        "statement logging is disabled"
    );
}
