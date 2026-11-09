use reelarr_db::Database;
use reelarr_types::{LibraryRootId, MediaFileId, MediaKind, MovieId};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use super::files::kind_from_row;
use super::{backend, now_secs, to_i64, to_u64};
use crate::import::TransferMode;
use crate::store::StoreError;

/// Where one job's import got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerState {
    /// Written before any file is touched.
    Pending,
    /// The file is in the library and recorded.
    Done,
}

/// One job's import, so a repeated call can resume or return the same result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerEntry {
    pub job_id: String,
    pub state: LedgerState,
    pub movie_id: MovieId,
    pub root_id: LibraryRootId,
    pub kind: MediaKind,
    pub source_path: String,
    pub dest_path: String,
    pub mode: TransferMode,
    pub size_bytes: u64,
    pub media_file_id: Option<MediaFileId>,
}

/// The `import_ledger` table.
///
/// `job_id` is not a foreign key to `jobs`, so a jobs-table rebuild cannot
/// cascade-delete a pending import. A later pass should delete orphans:
/// `DELETE FROM import_ledger WHERE job_id NOT IN (SELECT id FROM jobs)`.
#[derive(Debug, Clone)]
pub struct SqliteImportLedger {
    db: Database,
}

fn mode_str(mode: TransferMode) -> &'static str {
    match mode {
        TransferMode::HardlinkOrCopy => "hardlink_or_copy",
        TransferMode::Copy => "copy",
        TransferMode::Move => "move",
    }
}

fn row_to_entry(row: &SqliteRow) -> Result<LedgerEntry, StoreError> {
    let state: String = row.try_get("state").map_err(backend)?;
    let mode: String = row.try_get("mode").map_err(backend)?;
    let media_file_id: Option<i64> = row.try_get("media_file_id").map_err(backend)?;
    Ok(LedgerEntry {
        job_id: row.try_get("job_id").map_err(backend)?,
        state: match state.as_str() {
            "pending" => LedgerState::Pending,
            "done" => LedgerState::Done,
            other => return Err(StoreError::Backend(format!("unknown ledger state {other}"))),
        },
        movie_id: MovieId(row.try_get("movie_id").map_err(backend)?),
        root_id: LibraryRootId(row.try_get("root_id").map_err(backend)?),
        kind: kind_from_row(row)?,
        source_path: row.try_get("source_path").map_err(backend)?,
        dest_path: row.try_get("dest_path").map_err(backend)?,
        mode: match mode.as_str() {
            "hardlink_or_copy" => TransferMode::HardlinkOrCopy,
            "copy" => TransferMode::Copy,
            "move" => TransferMode::Move,
            other => {
                return Err(StoreError::Backend(format!(
                    "unknown transfer mode {other}"
                )));
            }
        },
        size_bytes: to_u64(row.try_get("size_bytes").map_err(backend)?, "size_bytes")?,
        media_file_id: media_file_id.map(MediaFileId),
    })
}

impl SqliteImportLedger {
    pub fn new(db: &Database) -> Self {
        Self { db: db.clone() }
    }

    pub async fn get(&self, job_id: &str) -> Result<Option<LedgerEntry>, StoreError> {
        sqlx::query(
            "SELECT job_id, state, movie_id, root_id, kind, source_path, dest_path, mode, \
             size_bytes, media_file_id FROM import_ledger WHERE job_id = ?",
        )
        .bind(job_id)
        .fetch_optional(self.db.pool())
        .await
        .map_err(backend)?
        .as_ref()
        .map(row_to_entry)
        .transpose()
    }

    /// Records the plan for a job before any file is touched, replacing an
    /// earlier plan for the same job.
    pub async fn put_pending(&self, entry: &LedgerEntry) -> Result<(), StoreError> {
        let now = now_secs();
        sqlx::query(
            "INSERT INTO import_ledger (job_id, state, movie_id, root_id, kind, source_path, \
             dest_path, mode, size_bytes, media_file_id, created_at, updated_at) \
             VALUES (?, 'pending', ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?) \
             ON CONFLICT (job_id) DO UPDATE SET state = 'pending', movie_id = excluded.movie_id, \
             root_id = excluded.root_id, kind = excluded.kind, source_path = excluded.source_path, \
             dest_path = excluded.dest_path, mode = excluded.mode, \
             size_bytes = excluded.size_bytes, media_file_id = NULL, updated_at = excluded.updated_at",
        )
        .bind(&entry.job_id)
        .bind(entry.movie_id.0)
        .bind(entry.root_id.0)
        .bind(entry.kind.as_str())
        .bind(&entry.source_path)
        .bind(&entry.dest_path)
        .bind(mode_str(entry.mode))
        .bind(to_i64(entry.size_bytes, "size_bytes")?)
        .bind(now)
        .bind(now)
        .execute(self.db.pool())
        .await
        .map_err(backend)?;
        Ok(())
    }

    pub async fn mark_done(&self, job_id: &str, file: MediaFileId) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE import_ledger SET state = 'done', media_file_id = ?, updated_at = ? \
             WHERE job_id = ?",
        )
        .bind(file.0)
        .bind(now_secs())
        .bind(job_id)
        .execute(self.db.pool())
        .await
        .map_err(backend)?;
        Ok(())
    }
}
