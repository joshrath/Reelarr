use async_trait::async_trait;
use reelarr_core::{Setting, SettingsError, SettingsStore};
use sqlx::{Row, SqlitePool};

use crate::Database;

/// Settings adapter using the application's shared database pool.
#[derive(Clone)]
pub struct SqliteSettingsStore {
    pool: SqlitePool,
}

impl SqliteSettingsStore {
    pub fn new(db: &Database) -> Self {
        Self {
            pool: db.pool().clone(),
        }
    }
}

fn storage_error(error: sqlx::Error) -> SettingsError {
    SettingsError(Box::new(error))
}

#[async_trait]
impl SettingsStore for SqliteSettingsStore {
    async fn get(&self, key: &str) -> Result<Option<Setting>, SettingsError> {
        let row = sqlx::query("SELECT key, value, updated_at FROM settings WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(storage_error)?;
        row.map(|row| {
            Ok(Setting {
                key: row.try_get("key")?,
                value: row.try_get("value")?,
                updated_at: row.try_get("updated_at")?,
            })
        })
        .transpose()
        .map_err(storage_error)
    }

    async fn set(&self, key: &str, value: &str) -> Result<(), SettingsError> {
        sqlx::query("INSERT INTO settings (key, value, updated_at) VALUES (?, ?, unixepoch()) \
            ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at")
            .bind(key).bind(value).execute(&self.pool).await.map_err(storage_error)?;
        Ok(())
    }
}
