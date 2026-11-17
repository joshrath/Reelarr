//! Shared indexer clients for NZB fetches (#71).
//!
//! One [`NewznabClient`] per indexer, built from the stored settings
//! ([`SqliteIndexerStore`], API key decrypted in memory only) and reused, so
//! every fetch for an indexer shares its rate gate and connection pool. A
//! client is rebuilt only when that indexer's stored settings changed. The
//! client enforces the indexer's download-host policy (same origin, or HTTPS
//! to an exact `extra_download_hosts` entry) on the URL and every redirect.
use std::{collections::HashMap, sync::Arc};

use reelarr_db::SqliteIndexerStore;
use reelarr_indexer::{IndexerConfig, IndexerError, IndexerStore, NewznabClient};
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct IndexerClients {
    store: SqliteIndexerStore,
    clients: Arc<Mutex<HashMap<String, (IndexerConfig, NewznabClient)>>>,
}

impl IndexerClients {
    pub fn new(store: SqliteIndexerStore) -> Self {
        Self {
            store,
            clients: Arc::default(),
        }
    }

    /// The shared client for `name`, or `None` if no such indexer is stored.
    pub async fn client(&self, name: &str) -> Result<Option<NewznabClient>, IndexerError> {
        let Some(config) = self.store.get(name).await? else {
            self.clients.lock().await.remove(name);
            return Ok(None);
        };
        let mut clients = self.clients.lock().await;
        if let Some((cached, client)) = clients.get(name)
            && same_settings(cached, &config)
        {
            return Ok(Some(client.clone()));
        }
        let client = NewznabClient::new(config.clone())?;
        clients.insert(name.to_owned(), (config, client.clone()));
        Ok(Some(client))
    }

    /// Drop a cached client (for the indexer settings API to call on save).
    pub async fn invalidate(&self, name: &str) {
        self.clients.lock().await.remove(name);
    }
}

fn same_settings(a: &IndexerConfig, b: &IndexerConfig) -> bool {
    a.name == b.name
        && a.base_url == b.base_url
        && a.api_path == b.api_path
        && a.api_key.expose() == b.api_key.expose()
        && a.categories == b.categories
        && a.enabled == b.enabled
        && a.allow_http == b.allow_http
        && a.extra_download_hosts == b.extra_download_hosts
        && a.connect_timeout == b.connect_timeout
        && a.request_timeout == b.request_timeout
        && a.minimum_interval == b.minimum_interval
        && a.caps_ttl == b.caps_ttl
        && a.max_response_bytes == b.max_response_bytes
        && a.max_pages == b.max_pages
        && a.max_results == b.max_results
}
