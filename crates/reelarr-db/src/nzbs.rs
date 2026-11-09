//! SQLite implementation of `reelarr_nzb::NzbStore` (#76).
//!
//! Documents are immutable after insert and stored as versioned JSON. Checkpoints
//! are one row per completed segment; each `checkpoint` batch is validated in full
//! and applied in a single `BEGIN IMMEDIATE` transaction, so a batch is all or
//! nothing. Errors never carry SQL text or NZB content.

use std::{
    collections::BTreeSet,
    time::{SystemTime, UNIX_EPOCH},
};

use reelarr_nzb::{
    JobId, NzbDocument, NzbFile, NzbStore, Segment, SegmentKey, StoreError, StoredNzb,
};
use reelarr_types::MovieId;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use crate::Database;

const FORMAT_VERSION: u32 = 1;

/// Cheaply cloned; all clones share the database pool.
#[derive(Clone, Debug)]
pub struct SqliteNzbStore {
    pool: SqlitePool,
}

impl SqliteNzbStore {
    /// Store over an already-migrated database. Does not reconcile; prefer [`Self::open`].
    pub fn new(db: &Database) -> Self {
        Self {
            pool: db.pool().clone(),
        }
    }

    /// Store plus startup reconciliation. Call after migrations and before any
    /// worker can admit jobs.
    pub async fn open(db: &Database) -> Result<Self, StoreError> {
        let store = Self::new(db);
        store.reconcile().await?;
        Ok(store)
    }

    /// Delete NZBs whose job row does not exist: an admission that crashed after
    /// storing the NZB but before inserting the job, or whose rollback failed.
    /// Returns how many were removed. Must not run while admissions are in flight,
    /// because admission stores the NZB before the job row exists.
    pub async fn reconcile(&self) -> Result<u64, StoreError> {
        let done =
            sqlx::query("DELETE FROM nzb_documents WHERE job_id NOT IN (SELECT id FROM jobs)")
                .execute(&self.pool)
                .await
                .map_err(backend)?;
        Ok(done.rows_affected())
    }

    /// The movie a stored NZB was grabbed for, without decoding the document:
    /// `None` when no NZB is stored for the job, `Some(None)` for a manual one.
    pub async fn movie_id(&self, job_id: JobId) -> Result<Option<Option<MovieId>>, StoreError> {
        let row: Option<(Option<i64>,)> =
            sqlx::query_as("SELECT movie_id FROM nzb_documents WHERE job_id = ?")
                .bind(job_id.to_string())
                .fetch_optional(&self.pool)
                .await
                .map_err(backend)?;
        Ok(row.map(|(movie,)| movie.map(MovieId)))
    }
}

fn backend(_: sqlx::Error) -> StoreError {
    StoreError::Backend
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[derive(Serialize, Deserialize)]
struct DocV1 {
    v: u32,
    title: Option<String>,
    password: Option<String>,
    category: Option<String>,
    files: Vec<FileV1>,
}

#[derive(Serialize, Deserialize)]
struct FileV1 {
    subject: String,
    poster: String,
    date: u64,
    groups: Vec<String>,
    segments: Vec<SegV1>,
}

#[derive(Serialize, Deserialize)]
struct SegV1 {
    id: String,
    n: u32,
    b: u64,
}

fn encode(doc: &NzbDocument) -> Result<String, StoreError> {
    let v1 = DocV1 {
        v: FORMAT_VERSION,
        title: doc.title.clone(),
        password: doc.password.clone(),
        category: doc.category.clone(),
        files: doc
            .files
            .iter()
            .map(|f| FileV1 {
                subject: f.subject.clone(),
                poster: f.poster.clone(),
                date: f.date,
                groups: f.groups.clone(),
                segments: f
                    .segments
                    .iter()
                    .map(|s| SegV1 {
                        id: s.message_id.clone(),
                        n: s.number,
                        b: s.bytes,
                    })
                    .collect(),
            })
            .collect(),
    };
    serde_json::to_string(&v1).map_err(|_| StoreError::Backend)
}

fn decode(text: &str) -> Result<NzbDocument, StoreError> {
    let v1: DocV1 = serde_json::from_str(text).map_err(|_| StoreError::Backend)?;
    if v1.v != FORMAT_VERSION {
        return Err(StoreError::Backend);
    }
    Ok(NzbDocument {
        title: v1.title,
        password: v1.password,
        category: v1.category,
        files: v1
            .files
            .into_iter()
            .map(|f| NzbFile {
                subject: f.subject,
                poster: f.poster,
                date: f.date,
                groups: f.groups,
                segments: f
                    .segments
                    .into_iter()
                    .map(|s| Segment {
                        message_id: s.id,
                        number: s.n,
                        bytes: s.b,
                    })
                    .collect(),
            })
            .collect(),
    })
}

/// Segment numbers are validated contiguous from one at insert, so a key is
/// valid exactly when its file exists and its number is within that file's count.
fn check_key(counts: &[u32], key: SegmentKey) -> Result<(), StoreError> {
    match counts.get(key.file_index) {
        Some(&count) if key.number >= 1 && key.number <= count => Ok(()),
        _ => Err(StoreError::UnknownSegment(key)),
    }
}

fn index_i64(key: SegmentKey) -> Result<i64, StoreError> {
    i64::try_from(key.file_index).map_err(|_| StoreError::UnknownSegment(key))
}

impl NzbStore for SqliteNzbStore {
    async fn insert(
        &self,
        job_id: JobId,
        document: NzbDocument,
        movie_id: Option<MovieId>,
    ) -> Result<(), StoreError> {
        // Same validation as the in-memory store (resume() runs the crate's validator).
        let stored = StoredNzb {
            document,
            completed: BTreeSet::new(),
            movie_id,
        };
        stored.resume()?;
        let counts: Vec<u32> = stored
            .document
            .files
            .iter()
            .map(|f| u32::try_from(f.segments.len()).map_err(|_| StoreError::Backend))
            .collect::<Result<_, _>>()?;
        let counts = serde_json::to_string(&counts).map_err(|_| StoreError::Backend)?;
        let inserted = sqlx::query(
            "INSERT INTO nzb_documents (job_id, movie_id, document, segment_counts, created_at)
             VALUES (?, ?, ?, ?, ?) ON CONFLICT(job_id) DO NOTHING",
        )
        .bind(job_id.to_string())
        .bind(movie_id.map(|m| m.0))
        .bind(encode(&stored.document)?)
        .bind(counts)
        .bind(now())
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        if inserted.rows_affected() == 0 {
            return Err(StoreError::AlreadyExists);
        }
        Ok(())
    }

    async fn remove(&self, job_id: JobId) -> Result<(), StoreError> {
        // Checkpoints go with the document through ON DELETE CASCADE.
        sqlx::query("DELETE FROM nzb_documents WHERE job_id = ?")
            .bind(job_id.to_string())
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn load(&self, job_id: JobId) -> Result<Option<StoredNzb>, StoreError> {
        let id = job_id.to_string();
        // One read transaction, so the document and checkpoints are one snapshot.
        let mut tx = self.pool.begin().await.map_err(backend)?;
        let Some(row) =
            sqlx::query("SELECT movie_id, document FROM nzb_documents WHERE job_id = ?")
                .bind(&id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(backend)?
        else {
            return Ok(None);
        };
        let movie_id = row
            .try_get::<Option<i64>, _>("movie_id")
            .map_err(backend)?
            .map(MovieId);
        let document = decode(row.try_get::<&str, _>("document").map_err(backend)?)?;
        let rows = sqlx::query("SELECT file_index, number FROM nzb_checkpoints WHERE job_id = ?")
            .bind(&id)
            .fetch_all(&mut *tx)
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        let mut completed = BTreeSet::new();
        for row in rows {
            let file_index = usize::try_from(row.try_get::<i64, _>("file_index").map_err(backend)?)
                .map_err(|_| StoreError::Backend)?;
            let number = u32::try_from(row.try_get::<i64, _>("number").map_err(backend)?)
                .map_err(|_| StoreError::Backend)?;
            completed.insert(SegmentKey { file_index, number });
        }
        Ok(Some(StoredNzb {
            document,
            completed,
            movie_id,
        }))
    }

    async fn checkpoint(
        &self,
        job_id: JobId,
        updates: &[(SegmentKey, bool)],
    ) -> Result<(), StoreError> {
        let id = job_id.to_string();
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(backend)?;
        let counts: String =
            sqlx::query_scalar("SELECT segment_counts FROM nzb_documents WHERE job_id = ?")
                .bind(&id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(backend)?
                .ok_or(StoreError::NotFound)?;
        let counts: Vec<u32> = serde_json::from_str(&counts).map_err(|_| StoreError::Backend)?;
        // Validate the whole batch before writing anything.
        for (key, _) in updates {
            check_key(&counts, *key)?;
        }
        // Applied in order inside one transaction: last update wins for duplicates.
        for (key, complete) in updates {
            let sql = if *complete {
                "INSERT INTO nzb_checkpoints (job_id, file_index, number) VALUES (?, ?, ?)
                 ON CONFLICT DO NOTHING"
            } else {
                "DELETE FROM nzb_checkpoints WHERE job_id = ? AND file_index = ? AND number = ?"
            };
            sqlx::query(sql)
                .bind(&id)
                .bind(index_i64(*key)?)
                .bind(i64::from(key.number))
                .execute(&mut *tx)
                .await
                .map_err(backend)?;
        }
        tx.commit().await.map_err(backend)?;
        Ok(())
    }
}
