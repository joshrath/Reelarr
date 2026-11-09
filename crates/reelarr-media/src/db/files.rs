use reelarr_db::Database;
use reelarr_types::{
    LibraryRootId, MediaFile, MediaFileId, MediaFileStatus, MediaKind, MediaProbe, MovieId,
};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use super::{backend, to_i64, to_u64};
use crate::scan::{MediaFileStore, NewMediaFile};
use crate::store::StoreError;

const COLUMNS: &str =
    "id, movie_id, root_id, path, size_bytes, modified_at, missing_since, probe, kind";

/// [`MediaFileStore`] backed by the `media_files` table. A file's
/// [`MediaFileStatus::Missing`] is stored as `missing_since`, and its probe
/// as JSON.
#[derive(Debug, Clone)]
pub struct SqliteMediaFileStore {
    db: Database,
}

impl SqliteMediaFileStore {
    pub fn new(db: &Database) -> Self {
        Self { db: db.clone() }
    }
}

pub(crate) fn probe_from_json(json: Option<String>) -> Result<Option<MediaProbe>, StoreError> {
    json.map(|j| serde_json::from_str(&j))
        .transpose()
        .map_err(|e| StoreError::Backend(format!("stored probe is not valid JSON: {e}")))
}

fn probe_to_json(probe: &Option<MediaProbe>) -> Result<Option<String>, StoreError> {
    probe
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(backend)
}

fn missing_since(status: &MediaFileStatus) -> Option<i64> {
    match status {
        MediaFileStatus::Present => None,
        MediaFileStatus::Missing { since } => Some(*since),
    }
}

pub(crate) fn kind_from_row(row: &SqliteRow) -> Result<MediaKind, StoreError> {
    let kind: String = row.try_get("kind").map_err(backend)?;
    MediaKind::parse(&kind).ok_or_else(|| StoreError::Backend(format!("unknown media kind {kind}")))
}

fn row_to_file(row: &SqliteRow) -> Result<MediaFile, StoreError> {
    let missing: Option<i64> = row.try_get("missing_since").map_err(backend)?;
    let movie_id: Option<i64> = row.try_get("movie_id").map_err(backend)?;
    Ok(MediaFile {
        id: MediaFileId(row.try_get("id").map_err(backend)?),
        movie_id: movie_id.map(MovieId),
        root_id: LibraryRootId(row.try_get("root_id").map_err(backend)?),
        path: row.try_get("path").map_err(backend)?,
        size_bytes: to_u64(row.try_get("size_bytes").map_err(backend)?, "size_bytes")?,
        modified_at: row.try_get("modified_at").map_err(backend)?,
        status: match missing {
            None => MediaFileStatus::Present,
            Some(since) => MediaFileStatus::Missing { since },
        },
        probe: probe_from_json(row.try_get("probe").map_err(backend)?)?,
        kind: kind_from_row(row)?,
    })
}

impl MediaFileStore for SqliteMediaFileStore {
    async fn insert(&self, file: NewMediaFile) -> Result<MediaFile, StoreError> {
        let done = sqlx::query(
            "INSERT INTO media_files (movie_id, root_id, path, size_bytes, modified_at, probe, kind) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(file.movie_id.map(|m| m.0))
        .bind(file.root_id.0)
        .bind(&file.path)
        .bind(to_i64(file.size_bytes, "size_bytes")?)
        .bind(file.modified_at)
        .bind(probe_to_json(&file.probe)?)
        .bind(file.kind.as_str())
        .execute(self.db.pool())
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(d) if d.is_unique_violation() => {
                StoreError::Backend(format!("media file already stored: {}", file.path))
            }
            _ => backend(e),
        })?;
        Ok(MediaFile {
            id: MediaFileId(done.last_insert_rowid()),
            movie_id: file.movie_id,
            root_id: file.root_id,
            path: file.path,
            size_bytes: file.size_bytes,
            modified_at: file.modified_at,
            status: MediaFileStatus::Present,
            probe: file.probe,
            kind: file.kind,
        })
    }

    async fn update(&self, file: &MediaFile) -> Result<(), StoreError> {
        let done = sqlx::query(
            "UPDATE media_files SET movie_id = ?, root_id = ?, path = ?, size_bytes = ?, \
             modified_at = ?, missing_since = ?, probe = ?, kind = ? WHERE id = ?",
        )
        .bind(file.movie_id.map(|m| m.0))
        .bind(file.root_id.0)
        .bind(&file.path)
        .bind(to_i64(file.size_bytes, "size_bytes")?)
        .bind(file.modified_at)
        .bind(missing_since(&file.status))
        .bind(probe_to_json(&file.probe)?)
        .bind(file.kind.as_str())
        .bind(file.id.0)
        .execute(self.db.pool())
        .await
        .map_err(backend)?;
        if done.rows_affected() == 0 {
            return Err(StoreError::Backend(format!(
                "no media file with id {}",
                file.id.0
            )));
        }
        Ok(())
    }

    async fn get(&self, id: MediaFileId) -> Result<Option<MediaFile>, StoreError> {
        sqlx::query(&format!("SELECT {COLUMNS} FROM media_files WHERE id = ?"))
            .bind(id.0)
            .fetch_optional(self.db.pool())
            .await
            .map_err(backend)?
            .as_ref()
            .map(row_to_file)
            .transpose()
    }

    async fn get_by_path(&self, path: &str) -> Result<Option<MediaFile>, StoreError> {
        sqlx::query(&format!("SELECT {COLUMNS} FROM media_files WHERE path = ?"))
            .bind(path)
            .fetch_optional(self.db.pool())
            .await
            .map_err(backend)?
            .as_ref()
            .map(row_to_file)
            .transpose()
    }

    async fn list_by_root(&self, root_id: LibraryRootId) -> Result<Vec<MediaFile>, StoreError> {
        sqlx::query(&format!(
            "SELECT {COLUMNS} FROM media_files WHERE root_id = ? ORDER BY id"
        ))
        .bind(root_id.0)
        .fetch_all(self.db.pool())
        .await
        .map_err(backend)?
        .iter()
        .map(row_to_file)
        .collect()
    }
}
