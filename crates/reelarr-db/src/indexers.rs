use std::{fmt, sync::Arc, time::Duration};

use reelarr_indexer::{IndexerConfig, IndexerError, IndexerStore, StoreFuture, Url};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool, sqlite::SqliteRow};

use crate::{Database, secrets::SecretCipher};

/// Secret-free reads for an API layer. These never select/decrypt API keys.
/// Full IndexerStore reads are for trusted internal client construction only;
/// IndexerConfig intentionally has no serialization implementation.
#[derive(Clone, Serialize)]
pub struct IndexerSummary {
    pub name: String,
    pub enabled: bool,
    pub extra_download_hosts: Vec<String>,
    pub has_api_key: bool,
}
impl fmt::Debug for IndexerSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IndexerSummary")
            .field("enabled", &self.enabled)
            .field("has_api_key", &self.has_api_key)
            .finish_non_exhaustive()
    }
}

/// SQLite adapter sharing the application's pool. Each operation is one SQL
/// statement/snapshot; upsert validates/encodes/encrypts before any database write.
#[derive(Clone)]
pub struct SqliteIndexerStore {
    pool: SqlitePool,
    cipher: Arc<dyn SecretCipher>,
}
impl fmt::Debug for SqliteIndexerStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SqliteIndexerStore { cipher: [REDACTED], .. }")
    }
}
impl SqliteIndexerStore {
    pub fn new(db: &Database, cipher: Arc<dyn SecretCipher>) -> Self {
        Self {
            pool: db.pool().clone(),
            cipher,
        }
    }

    pub async fn list_summaries(&self) -> Result<Vec<IndexerSummary>, IndexerError> {
        sqlx::query("SELECT name, settings_json, extra_download_hosts, length(api_key_ciphertext) > 0 AS has_api_key FROM indexers ORDER BY name")
            .fetch_all(&self.pool).await.map_err(|_| IndexerError::Store)?
            .into_iter().map(summary).collect()
    }

    pub async fn get_summary(&self, name: &str) -> Result<Option<IndexerSummary>, IndexerError> {
        sqlx::query("SELECT name, settings_json, extra_download_hosts, length(api_key_ciphertext) > 0 AS has_api_key FROM indexers WHERE name = ?")
            .bind(name).fetch_optional(&self.pool).await.map_err(|_| IndexerError::Store)?
            .map(summary).transpose()
    }

    fn decode(&self, row: SqliteRow) -> Result<IndexerConfig, IndexerError> {
        let name: String = row.try_get("name").map_err(|_| IndexerError::Store)?;
        let settings: Settings = parse_json(&row, "settings_json")?;
        let hosts: Vec<String> = parse_json(&row, "extra_download_hosts")?;
        let encrypted: Vec<u8> = row
            .try_get("api_key_ciphertext")
            .map_err(|_| IndexerError::Store)?;
        let key = self
            .cipher
            .decrypt(&context(&name), &encrypted)
            .map_err(|_| IndexerError::Store)?;
        let config = settings.into_config(name, key, hosts)?;
        // Corrupt/out-of-band writes must not bypass the allowlist/config policy.
        config.validate().map_err(|_| IndexerError::Store)?;
        Ok(config)
    }
}
fn context(name: &str) -> Vec<u8> {
    [b"reelarr:indexers:api_key:".as_slice(), name.as_bytes()].concat()
}
fn parse_json<T: serde::de::DeserializeOwned>(
    row: &SqliteRow,
    field: &str,
) -> Result<T, IndexerError> {
    let text: String = row.try_get(field).map_err(|_| IndexerError::Store)?;
    serde_json::from_str(&text).map_err(|_| IndexerError::Store)
}
fn summary(row: SqliteRow) -> Result<IndexerSummary, IndexerError> {
    let settings: Settings = parse_json(&row, "settings_json")?;
    Ok(IndexerSummary {
        name: row.try_get("name").map_err(|_| IndexerError::Store)?,
        enabled: settings.enabled,
        extra_download_hosts: parse_json(&row, "extra_download_hosts")?,
        has_api_key: row
            .try_get("has_api_key")
            .map_err(|_| IndexerError::Store)?,
    })
}

impl IndexerStore for SqliteIndexerStore {
    fn list(&self) -> StoreFuture<'_, Vec<IndexerConfig>> {
        Box::pin(async {
            sqlx::query("SELECT name, settings_json, extra_download_hosts, api_key_ciphertext FROM indexers ORDER BY name")
                .fetch_all(&self.pool).await.map_err(|_| IndexerError::Store)?
                .into_iter().map(|row| self.decode(row)).collect()
        })
    }
    fn get<'a>(&'a self, name: &'a str) -> StoreFuture<'a, Option<IndexerConfig>> {
        Box::pin(async move {
            sqlx::query("SELECT name, settings_json, extra_download_hosts, api_key_ciphertext FROM indexers WHERE name = ?")
                .bind(name).fetch_optional(&self.pool).await.map_err(|_| IndexerError::Store)?
                .map(|row| self.decode(row)).transpose()
        })
    }
    fn upsert(&self, config: IndexerConfig) -> StoreFuture<'_, ()> {
        Box::pin(async move {
            config.validate()?;
            let settings =
                serde_json::to_string(&Settings::from(&config)).map_err(|_| IndexerError::Store)?;
            let hosts = serde_json::to_string(&config.extra_download_hosts)
                .map_err(|_| IndexerError::Store)?;
            let encrypted = self
                .cipher
                .encrypt(&context(&config.name), config.api_key.expose())
                .map_err(|_| IndexerError::Store)?;
            sqlx::query("INSERT INTO indexers (name, settings_json, extra_download_hosts, api_key_ciphertext) VALUES (?, ?, ?, ?) \
                ON CONFLICT(name) DO UPDATE SET settings_json = excluded.settings_json, extra_download_hosts = excluded.extra_download_hosts, api_key_ciphertext = excluded.api_key_ciphertext")
                .bind(&config.name).bind(settings).bind(hosts).bind(encrypted)
                .execute(&self.pool).await.map_err(|_| IndexerError::Store)?;
            Ok(())
        })
    }
    fn remove<'a>(&'a self, name: &'a str) -> StoreFuture<'a, bool> {
        Box::pin(async move {
            Ok(sqlx::query("DELETE FROM indexers WHERE name = ?")
                .bind(name)
                .execute(&self.pool)
                .await
                .map_err(|_| IndexerError::Store)?
                .rows_affected()
                > 0)
        })
    }
}

// A typed, versioned non-secret payload, never a serialized IndexerConfig. std's
// Duration serde representation preserves subsecond precision (not rounded ms).
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    version: u8,
    base_url: String,
    api_path: String,
    categories: Vec<u32>,
    enabled: bool,
    allow_http: bool,
    connect_timeout: Duration,
    request_timeout: Duration,
    minimum_interval: Duration,
    caps_ttl: Duration,
    max_response_bytes: usize,
    max_pages: usize,
    max_results: usize,
}
impl From<&IndexerConfig> for Settings {
    fn from(c: &IndexerConfig) -> Self {
        Self {
            version: 1,
            base_url: c.base_url.as_str().into(),
            api_path: c.api_path.clone(),
            categories: c.categories.clone(),
            enabled: c.enabled,
            allow_http: c.allow_http,
            connect_timeout: c.connect_timeout,
            request_timeout: c.request_timeout,
            minimum_interval: c.minimum_interval,
            caps_ttl: c.caps_ttl,
            max_response_bytes: c.max_response_bytes,
            max_pages: c.max_pages,
            max_results: c.max_results,
        }
    }
}
impl Settings {
    fn into_config(
        self,
        name: String,
        api_key: reelarr_indexer::SecretString,
        extra_download_hosts: Vec<String>,
    ) -> Result<IndexerConfig, IndexerError> {
        if self.version != 1 {
            return Err(IndexerError::Store);
        }
        Ok(IndexerConfig {
            name,
            api_key,
            extra_download_hosts,
            base_url: Url::parse(&self.base_url).map_err(|_| IndexerError::Store)?,
            api_path: self.api_path,
            categories: self.categories,
            enabled: self.enabled,
            allow_http: self.allow_http,
            connect_timeout: self.connect_timeout,
            request_timeout: self.request_timeout,
            minimum_interval: self.minimum_interval,
            caps_ttl: self.caps_ttl,
            max_response_bytes: self.max_response_bytes,
            max_pages: self.max_pages,
            max_results: self.max_results,
        })
    }
}
