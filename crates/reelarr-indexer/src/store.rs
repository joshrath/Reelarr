//! Storage boundary. reelarr-db supplies persistent implementations and migrations.
use crate::{IndexerConfig, IndexerError};
use std::{collections::BTreeMap, future::Future, pin::Pin};
use tokio::sync::RwLock;

/// Object-safe asynchronous storage result, usable by database adapters.
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, IndexerError>> + Send + 'a>>;
/// Store configs by unique name; secret persistence is the adapter's responsibility.
pub trait IndexerStore: Send + Sync {
    fn list(&self) -> StoreFuture<'_, Vec<IndexerConfig>>;
    fn get<'a>(&'a self, name: &'a str) -> StoreFuture<'a, Option<IndexerConfig>>;
    fn upsert(&self, config: IndexerConfig) -> StoreFuture<'_, ()>;
    fn remove<'a>(&'a self, name: &'a str) -> StoreFuture<'a, bool>;
}
/// Volatile implementation for tests/development; never writes a state file.
#[derive(Default)]
pub struct InMemoryIndexerStore {
    configs: RwLock<BTreeMap<String, IndexerConfig>>,
}
impl IndexerStore for InMemoryIndexerStore {
    fn list(&self) -> StoreFuture<'_, Vec<IndexerConfig>> {
        Box::pin(async { Ok(self.configs.read().await.values().cloned().collect()) })
    }
    fn get<'a>(&'a self, name: &'a str) -> StoreFuture<'a, Option<IndexerConfig>> {
        Box::pin(async move { Ok(self.configs.read().await.get(name).cloned()) })
    }
    fn upsert(&self, config: IndexerConfig) -> StoreFuture<'_, ()> {
        Box::pin(async move {
            config.validate()?;
            self.configs
                .write()
                .await
                .insert(config.name.clone(), config);
            Ok(())
        })
    }
    fn remove<'a>(&'a self, name: &'a str) -> StoreFuture<'a, bool> {
        Box::pin(async move { Ok(self.configs.write().await.remove(name).is_some()) })
    }
}
