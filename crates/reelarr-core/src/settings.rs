//! Persistence-independent settings contracts.

use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use tokio::sync::RwLock;

/// A stored setting. `updated_at` is Unix time in whole seconds (UTC).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Setting {
    pub key: String,
    pub value: String,
    pub updated_at: i64,
}

#[derive(Debug, thiserror::Error)]
#[error("settings storage failed: {0}")]
pub struct SettingsError(#[source] pub Box<dyn std::error::Error + Send + Sync>);

/// Reads and atomic upserts. Missing keys return `None`; values are opaque strings.
#[async_trait]
pub trait SettingsStore: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<Setting>, SettingsError>;
    async fn set(&self, key: &str, value: &str) -> Result<(), SettingsError>;
}

#[derive(Default)]
pub struct InMemorySettingsStore {
    settings: RwLock<HashMap<String, Setting>>,
}

#[async_trait]
impl SettingsStore for InMemorySettingsStore {
    async fn get(&self, key: &str) -> Result<Option<Setting>, SettingsError> {
        Ok(self.settings.read().await.get(key).cloned())
    }

    async fn set(&self, key: &str, value: &str) -> Result<(), SettingsError> {
        let updated_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| SettingsError(Box::new(error)))?
            .as_secs() as i64;
        self.settings.write().await.insert(
            key.to_owned(),
            Setting {
                key: key.to_owned(),
                value: value.to_owned(),
                updated_at,
            },
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn settings_round_trip() {
        let store = InMemorySettingsStore::default();
        assert!(store.get("theme").await.unwrap().is_none());
        store.set("theme", "dark").await.unwrap();
        let first = store.get("theme").await.unwrap().unwrap();
        assert_eq!(first.key, "theme");
        assert_eq!(first.value, "dark");
        assert!(first.updated_at > 0);
        store.set("theme", "light").await.unwrap();
        let second = store.get("theme").await.unwrap().unwrap();
        assert_eq!(second.value, "light");
        assert!(second.updated_at >= first.updated_at);
    }
}
