use std::path::{Component, Path};

use reelarr_db::Database;
use reelarr_types::{
    LibraryRoot, LibraryRootId, LibraryRootInfo, LibraryRootKind, NewLibraryRoot, RootHealth,
};
// `COLUMNS` is a compile-time constant, so these `format!`ed queries carry no
// caller-supplied SQL; values are always bound with `?`.
use sqlx::sqlite::SqliteRow;
use sqlx::{AssertSqlSafe, Row};

use super::{backend, to_i64, to_u64};
use crate::store::StoreError;

/// Errors from managing library roots. Messages are shown to the user.
#[derive(Debug, thiserror::Error)]
pub enum RootError {
    #[error("{0}")]
    InvalidPath(String),
    #[error("{path} is already a library folder")]
    Duplicate { path: String },
    #[error("{path} overlaps the existing library folder {existing}")]
    Overlaps { path: String, existing: String },
    #[error("library folder not found")]
    NotFound,
    #[error(
        "this folder has {count} files in the library; confirm to forget them (files on disk are not touched)"
    )]
    HasFiles { count: u64 },
    #[error(transparent)]
    Store(#[from] StoreError),
}

const COLUMNS: &str = "r.id, r.path, r.kind, r.rescan_interval_secs, r.health, r.health_reason, \
     r.health_since, r.health_checked_at";

/// Library roots in the `library_roots` table, including their last health
/// check.
#[derive(Debug, Clone)]
pub struct SqliteLibraryRootStore {
    db: Database,
}

fn kind_str(kind: LibraryRootKind) -> &'static str {
    match kind {
        LibraryRootKind::Library => "library",
        LibraryRootKind::ManualImport => "manual_import",
    }
}

fn row_to_root(row: &SqliteRow) -> Result<LibraryRoot, StoreError> {
    let kind: String = row.try_get("kind").map_err(backend)?;
    let interval: Option<i64> = row.try_get("rescan_interval_secs").map_err(backend)?;
    Ok(LibraryRoot {
        id: LibraryRootId(row.try_get("id").map_err(backend)?),
        path: row.try_get("path").map_err(backend)?,
        kind: match kind.as_str() {
            "library" => LibraryRootKind::Library,
            "manual_import" => LibraryRootKind::ManualImport,
            other => return Err(StoreError::Backend(format!("unknown root kind {other}"))),
        },
        rescan_interval_secs: interval
            .map(|v| to_u64(v, "rescan_interval_secs"))
            .transpose()?,
    })
}

fn row_to_health(row: &SqliteRow) -> Result<RootHealth, StoreError> {
    let health: String = row.try_get("health").map_err(backend)?;
    let reason: Option<String> = row.try_get("health_reason").map_err(backend)?;
    let since: Option<i64> = row.try_get("health_since").map_err(backend)?;
    let reason = reason.unwrap_or_default();
    Ok(match health.as_str() {
        "online" => RootHealth::Online,
        "offline" => RootHealth::Offline {
            since: since.unwrap_or(0),
            reason,
        },
        "permission_denied" => RootHealth::PermissionDenied { reason },
        _ => RootHealth::Unknown,
    })
}

/// Validates and normalizes a root path: absolute, no `.` or `..`, no
/// trailing slash.
pub(crate) fn normalize_root_path(path: &str) -> Result<String, RootError> {
    let trimmed = path.trim();
    let p = Path::new(trimmed);
    if trimmed.is_empty() || !p.is_absolute() {
        return Err(RootError::InvalidPath(format!(
            "library folder must be an absolute path, got {trimmed:?}"
        )));
    }
    if p.components()
        .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
    {
        return Err(RootError::InvalidPath(format!(
            "library folder must not contain . or .. ({trimmed})"
        )));
    }
    let normalized: std::path::PathBuf = p.components().collect();
    Ok(normalized.to_string_lossy().into_owned())
}

impl SqliteLibraryRootStore {
    pub fn new(db: &Database) -> Self {
        Self { db: db.clone() }
    }

    /// Adds a root. Rejects relative paths, duplicates and roots nested
    /// inside (or containing) an existing root, since a file must belong to
    /// exactly one root.
    pub async fn add(&self, new: NewLibraryRoot) -> Result<LibraryRoot, RootError> {
        let path = normalize_root_path(&new.path)?;
        if new.rescan_interval_secs == Some(0) {
            return Err(RootError::InvalidPath(
                "rescan interval must be at least one second".into(),
            ));
        }
        for existing in self.list().await? {
            if existing.path == path {
                return Err(RootError::Duplicate { path });
            }
            let (a, b) = (Path::new(&existing.path), Path::new(&path));
            if a.starts_with(b) || b.starts_with(a) {
                return Err(RootError::Overlaps {
                    path,
                    existing: existing.path,
                });
            }
        }
        let interval = new
            .rescan_interval_secs
            .map(|v| to_i64(v, "rescan_interval_secs"))
            .transpose()?;
        let done = sqlx::query(
            "INSERT INTO library_roots (path, kind, rescan_interval_secs) VALUES (?, ?, ?)",
        )
        .bind(&path)
        .bind(kind_str(new.kind))
        .bind(interval)
        .execute(self.db.pool())
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(d) if d.is_unique_violation() => {
                RootError::Duplicate { path: path.clone() }
            }
            _ => RootError::Store(backend(e)),
        })?;
        Ok(LibraryRoot {
            id: LibraryRootId(done.last_insert_rowid()),
            path,
            kind: new.kind,
            rescan_interval_secs: new.rescan_interval_secs,
        })
    }

    /// All roots, ordered by id.
    pub async fn list(&self) -> Result<Vec<LibraryRoot>, StoreError> {
        sqlx::query(AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM library_roots r ORDER BY r.id"
        )))
        .fetch_all(self.db.pool())
        .await
        .map_err(backend)?
        .iter()
        .map(row_to_root)
        .collect()
    }

    pub async fn get(&self, id: LibraryRootId) -> Result<Option<LibraryRoot>, StoreError> {
        sqlx::query(AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM library_roots r WHERE r.id = ?"
        )))
        .bind(id.0)
        .fetch_optional(self.db.pool())
        .await
        .map_err(backend)?
        .as_ref()
        .map(row_to_root)
        .transpose()
    }

    /// Roots with their health and the number of files the library knows in
    /// each, for the settings page.
    pub async fn list_info(&self) -> Result<Vec<LibraryRootInfo>, StoreError> {
        let rows = sqlx::query(AssertSqlSafe(format!(
            "SELECT {COLUMNS}, (SELECT COUNT(*) FROM media_files f WHERE f.root_id = r.id) \
             AS file_count FROM library_roots r ORDER BY r.id"
        )))
        .fetch_all(self.db.pool())
        .await
        .map_err(backend)?;
        rows.iter()
            .map(|row| {
                Ok(LibraryRootInfo {
                    root: row_to_root(row)?,
                    health: row_to_health(row)?,
                    health_checked_at: row.try_get("health_checked_at").map_err(backend)?,
                    file_count: to_u64(row.try_get("file_count").map_err(backend)?, "file_count")?,
                })
            })
            .collect()
    }

    pub async fn health(&self, id: LibraryRootId) -> Result<Option<RootHealth>, StoreError> {
        sqlx::query(AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM library_roots r WHERE r.id = ?"
        )))
        .bind(id.0)
        .fetch_optional(self.db.pool())
        .await
        .map_err(backend)?
        .as_ref()
        .map(row_to_health)
        .transpose()
    }

    /// Records a health check. An `Offline` root that was already offline
    /// keeps its original `since`, so the UI can say how long a share has
    /// been down.
    pub async fn set_health(
        &self,
        id: LibraryRootId,
        health: &RootHealth,
        checked_at: i64,
    ) -> Result<(), StoreError> {
        let (state, reason, since) = match health {
            RootHealth::Unknown => ("unknown", None, None),
            RootHealth::Online => ("online", None, None),
            RootHealth::Offline { since, reason } => {
                ("offline", Some(reason.as_str()), Some(*since))
            }
            RootHealth::PermissionDenied { reason } => {
                ("permission_denied", Some(reason.as_str()), None)
            }
        };
        sqlx::query(
            "UPDATE library_roots SET \
             health_since = CASE WHEN health = 'offline' AND ?1 = 'offline' \
                 THEN COALESCE(health_since, ?3) ELSE ?3 END, \
             health = ?1, health_reason = ?2, health_checked_at = ?4 WHERE id = ?5",
        )
        .bind(state)
        .bind(reason)
        .bind(since)
        .bind(checked_at)
        .bind(id.0)
        .execute(self.db.pool())
        .await
        .map_err(backend)?;
        Ok(())
    }

    /// Removes a root. If the library knows files under it, this fails with
    /// [`RootError::HasFiles`] unless `forget_files` is set, in which case
    /// their rows are deleted. Files on disk are never touched.
    pub async fn remove(&self, id: LibraryRootId, forget_files: bool) -> Result<(), RootError> {
        let mut tx = self.db.pool().begin().await.map_err(backend)?;
        let count: i64 = sqlx::query("SELECT COUNT(*) AS n FROM media_files WHERE root_id = ?")
            .bind(id.0)
            .fetch_one(&mut *tx)
            .await
            .map_err(backend)?
            .try_get("n")
            .map_err(backend)?;
        if count > 0 && !forget_files {
            return Err(RootError::HasFiles {
                count: count as u64,
            });
        }
        sqlx::query("DELETE FROM media_files WHERE root_id = ?")
            .bind(id.0)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        let done = sqlx::query("DELETE FROM library_roots WHERE id = ?")
            .bind(id.0)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        if done.rows_affected() == 0 {
            return Err(RootError::NotFound);
        }
        tx.commit().await.map_err(backend)?;
        Ok(())
    }
}
