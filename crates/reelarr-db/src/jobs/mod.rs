//! Durable lifecycle snapshots, volatile progress, and crash-safe event sequence leases.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use reelarr_core::{
    EventHub, Failure, Job, JobEvent, JobEventKind, JobId, JobState, JobStore, JobStoreError,
    Priority, Progress, TransitionError, apply_transition, recover_after_restart,
};
use sqlx::{SqliteConnection, SqlitePool};
use tokio::sync::Mutex;

use crate::Database;
mod codec;
use codec::{Snapshot, backend, decode, invalid, save, uint};

/// Reserve 256 sequence values at a time, never write progress values to SQLite.
const SEQ_LEASE: u64 = 256;

#[derive(Debug)]
struct Cached {
    progress: Progress,
    updated_at: i64,
    last_seq: u64,
    durable_seq: u64,
    reserved: u64,
    last_progress_event: Option<Instant>,
}

impl Cached {
    fn matches(&self, snapshot: &Snapshot) -> bool {
        self.durable_seq == snapshot.durable_seq && self.reserved == snapshot.reserved
    }
    fn from_snapshot(snapshot: &Snapshot, last_progress_event: Option<Instant>) -> Self {
        Self {
            progress: snapshot.job.progress.clone(),
            updated_at: snapshot.job.updated_at,
            last_seq: snapshot.job.last_event_seq,
            durable_seq: snapshot.durable_seq,
            reserved: snapshot.reserved,
            last_progress_event,
        }
    }
}

#[derive(Debug)]
pub(crate) struct JobRuntime {
    cache: Mutex<HashMap<JobId, Cached>>,
    events: EventHub,
    progress_interval: Duration,
}

/// Why [`SqliteJobStore::reorder`] changed nothing.
#[derive(Debug, thiserror::Error)]
pub enum ReorderError {
    #[error("job listed more than once: {0}")]
    Duplicate(JobId),
    #[error("job not found: {0}")]
    NotFound(JobId),
    #[error("job has finished and is no longer queued: {0}")]
    Finished(JobId),
    #[error(transparent)]
    Store(#[from] JobStoreError),
}

/// Clones and constructors using the same Database share one progress cache/event hub.
/// Independent database connections still serialize lifecycle changes with BEGIN IMMEDIATE.
#[derive(Clone, Debug)]
pub struct SqliteJobStore {
    pool: SqlitePool,
    runtime: Arc<JobRuntime>,
}

impl SqliteJobStore {
    pub fn new(db: &Database) -> Self {
        Self::with_options(db, EventHub::default(), Duration::from_millis(250))
    }

    /// The first store for a Database selects the shared hub/throttle configuration.
    pub fn with_options(db: &Database, events: EventHub, progress_interval: Duration) -> Self {
        let runtime = db
            .jobs
            .get_or_init(|| {
                Arc::new(JobRuntime {
                    cache: Mutex::new(HashMap::new()),
                    events,
                    progress_interval,
                })
            })
            .clone();
        Self {
            pool: db.pool().clone(),
            runtime,
        }
    }

    fn overlay(&self, snapshot: &mut Snapshot, cache: &HashMap<JobId, Cached>) -> Option<Instant> {
        if let Some(cached) = cache.get(&snapshot.job.id).filter(|c| c.matches(snapshot)) {
            snapshot.job.progress = cached.progress.clone();
            snapshot.job.last_event_seq = cached.last_seq;
            snapshot.job.updated_at = snapshot.job.updated_at.max(cached.updated_at);
            cached.last_progress_event
        } else {
            None
        }
    }

    fn allocate(snapshot: &mut Snapshot) -> Result<(), JobStoreError> {
        let next = snapshot
            .job
            .last_event_seq
            .checked_add(1)
            .ok_or(JobStoreError::SequenceExhausted(snapshot.job.id))?;
        if next > snapshot.reserved {
            snapshot.reserved = next.saturating_add(SEQ_LEASE - 1);
        }
        snapshot.job.last_event_seq = next;
        snapshot.job.updated_at = now().max(snapshot.job.updated_at);
        Ok(())
    }

    fn publish(&self, job: &Job, kind: JobEventKind) {
        self.runtime.events.publish(JobEvent {
            job_id: job.id,
            seq: job.last_event_seq,
            at: job.updated_at,
            kind,
        });
    }

    async fn load(conn: &mut SqliteConnection, id: JobId) -> Result<Snapshot, JobStoreError> {
        sqlx::query("SELECT * FROM jobs WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(conn)
            .await
            .map_err(backend)?
            .ok_or(JobStoreError::NotFound(id))
            .and_then(decode)
    }

    async fn mutate(
        &self,
        id: JobId,
        update: impl FnOnce(&mut Job) -> Result<JobEventKind, JobStoreError> + Send,
    ) -> Result<Job, JobStoreError> {
        let mut cache = self.runtime.cache.lock().await;
        // Lock the writer BEFORE reading. A competing transition observes the committed
        // current state, then validates it, rather than losing an update/busy-snapshot.
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(backend)?;
        let mut snapshot = Self::load(&mut tx, id).await?;
        let last_progress = self.overlay(&mut snapshot, &cache);
        let event = update(&mut snapshot.job)?;
        Self::allocate(&mut snapshot)?;
        save(&mut tx, &snapshot.job, snapshot.reserved, false).await?;
        tx.commit().await.map_err(backend)?;
        snapshot.durable_seq = snapshot.job.last_event_seq;
        cache.insert(id, Cached::from_snapshot(&snapshot, last_progress));
        // No awaits between successful commit, snapshot update and fanout.
        self.publish(&snapshot.job, event);
        Ok(snapshot.job)
    }

    /// Executors register their actual local artifact path once it is known. No
    /// title-based directory guesses; missing registrations conservatively requeue
    /// processing stages. This metadata is not a progress/segment checkpoint.
    pub async fn set_recovery_path(
        &self,
        id: JobId,
        path: Option<&Path>,
    ) -> Result<(), JobStoreError> {
        let path = path
            .map(|p| {
                if !p.is_absolute() {
                    return Err(invalid("recovery path must be local and absolute"));
                }
                p.to_str()
                    .ok_or_else(|| invalid("recovery path must be UTF-8"))
            })
            .transpose()?;
        let _cache = self.runtime.cache.lock().await;
        let changed = sqlx::query("UPDATE jobs SET recovery_path = ? WHERE id = ?")
            .bind(path)
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        if changed.rows_affected() == 0 {
            return Err(JobStoreError::NotFound(id));
        }
        Ok(())
    }

    /// The artifact path last registered with [`Self::set_recovery_path`].
    pub async fn recovery_path(&self, id: JobId) -> Result<Option<PathBuf>, JobStoreError> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT recovery_path FROM jobs WHERE id = ?")
                .bind(id.to_string())
                .fetch_optional(&self.pool)
                .await
                .map_err(backend)?;
        match row {
            Some((path,)) => Ok(path.map(PathBuf::from)),
            None => Err(JobStoreError::NotFound(id)),
        }
    }

    /// Move `front` to the head of the queue, in that order, in one
    /// transaction. Every other unfinished job keeps its relative order after
    /// them; finished jobs are not renumbered. Positions become 0, 1, 2, ...
    /// Priority still sorts before position. Only jobs whose position changed
    /// get a `Queue order updated` event (published after the commit). On any
    /// error nothing is written. Returns the unfinished jobs in their new
    /// order.
    pub async fn reorder(&self, front: &[JobId]) -> Result<Vec<Job>, ReorderError> {
        let mut seen = std::collections::HashSet::new();
        if let Some(dup) = front.iter().find(|id| !seen.insert(**id)) {
            return Err(ReorderError::Duplicate(*dup));
        }
        let mut cache = self.runtime.cache.lock().await;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(backend)?;
        let rows =
            sqlx::query("SELECT * FROM jobs ORDER BY priority, queue_position, created_at, id")
                .fetch_all(&mut *tx)
                .await
                .map_err(backend)?;
        let mut active = Vec::new();
        let mut finished = std::collections::HashSet::new();
        for row in rows {
            let mut snapshot = decode(row)?;
            let last_progress = self.overlay(&mut snapshot, &cache);
            if snapshot.job.state.is_terminal() {
                finished.insert(snapshot.job.id);
            } else {
                active.push((snapshot, last_progress));
            }
        }
        for id in front {
            if finished.contains(id) {
                return Err(ReorderError::Finished(*id));
            }
            if !active.iter().any(|(s, _)| s.job.id == *id) {
                return Err(ReorderError::NotFound(*id));
            }
        }
        let mut ordered = Vec::with_capacity(active.len());
        for id in front {
            let index = active.iter().position(|(s, _)| s.job.id == *id).unwrap();
            ordered.push(active.remove(index));
        }
        ordered.append(&mut active);
        let mut changed = Vec::new();
        for (position, (snapshot, _)) in ordered.iter_mut().enumerate() {
            let position = position as u64;
            if snapshot.job.queue_position == position {
                continue;
            }
            snapshot.job.queue_position = position;
            Self::allocate(snapshot)?;
            save(&mut tx, &snapshot.job, snapshot.reserved, false).await?;
            changed.push(snapshot.job.id);
        }
        tx.commit().await.map_err(backend)?;
        for (snapshot, last_progress) in &mut ordered {
            if changed.contains(&snapshot.job.id) {
                snapshot.durable_seq = snapshot.job.last_event_seq;
                cache.insert(
                    snapshot.job.id,
                    Cached::from_snapshot(snapshot, *last_progress),
                );
                self.publish(
                    &snapshot.job,
                    JobEventKind::Message {
                        message: "Queue order updated".into(),
                    },
                );
            }
        }
        let mut jobs: Vec<Job> = ordered.into_iter().map(|(s, _)| s.job).collect();
        jobs.sort_by_key(|j| (j.priority, j.queue_position, j.created_at, j.id));
        Ok(jobs)
    }

    /// Put a newly admitted job behind every unfinished job that was placed
    /// with [`Self::reorder`] or `set_queue_order`. New jobs start at position
    /// 0, which would otherwise sort them ahead of a reordered queue. A no-op
    /// (no write, no event) while no other unfinished job has a position
    /// above 0.
    pub async fn append_to_queue(&self, id: JobId) -> Result<Job, JobStoreError> {
        let mut cache = self.runtime.cache.lock().await;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(backend)?;
        let mut snapshot = Self::load(&mut tx, id).await?;
        let last_progress = self.overlay(&mut snapshot, &cache);
        let rows = sqlx::query(
            "SELECT queue_position FROM jobs WHERE id != ? \
             AND state NOT IN ('completed', 'failed', 'cancelled')",
        )
        .bind(id.to_string())
        .fetch_all(&mut *tx)
        .await
        .map_err(backend)?;
        let mut max = 0;
        for row in &rows {
            max = max.max(uint(row, "queue_position")?);
        }
        if snapshot.job.state.is_terminal() || max == 0 || snapshot.job.queue_position > max {
            return Ok(snapshot.job);
        }
        snapshot.job.queue_position = max.saturating_add(1);
        Self::allocate(&mut snapshot)?;
        save(&mut tx, &snapshot.job, snapshot.reserved, false).await?;
        tx.commit().await.map_err(backend)?;
        snapshot.durable_seq = snapshot.job.last_event_seq;
        cache.insert(id, Cached::from_snapshot(&snapshot, last_progress));
        self.publish(
            &snapshot.job,
            JobEventKind::Message {
                message: "Queue order updated".into(),
            },
        );
        Ok(snapshot.job)
    }

    /// Run after migrations and before binding/starting workers. Unreadable paths
    /// or database errors stop startup. No files are deleted or otherwise changed.
    pub async fn recover_on_startup(&self) -> Result<(), JobStoreError> {
        for job in self.list().await? {
            if job.state.is_terminal() {
                continue;
            }
            let files_exist = if matches!(
                job.state,
                JobState::Verifying | JobState::Repairing | JobState::Extracting
            ) {
                let path: Option<String> =
                    sqlx::query_scalar("SELECT recovery_path FROM jobs WHERE id = ?")
                        .bind(job.id.to_string())
                        .fetch_one(&self.pool)
                        .await
                        .map_err(backend)?;
                match path {
                    Some(path) => artifacts_exist(Path::new(&path)).await?,
                    None => false,
                }
            } else {
                false
            };
            self.recover(job.id, files_exist).await?;
        }
        Ok(())
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

async fn artifacts_exist(path: &Path) -> Result<bool, JobStoreError> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) if metadata.is_dir() => Ok(tokio::fs::read_dir(path)
            .await
            .map_err(backend)?
            .next_entry()
            .await
            .map_err(backend)?
            .is_some()),
        Ok(metadata) => Ok(metadata.is_file()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(backend(error)),
    }
}

#[async_trait]
impl JobStore for SqliteJobStore {
    fn events(&self) -> EventHub {
        self.runtime.events.clone()
    }

    async fn insert(&self, mut job: Job) -> Result<Job, JobStoreError> {
        if job.state != JobState::Queued
            || job.failure.is_some()
            || job.resume_state.is_some()
            || job.resume_hint.is_some()
            || job.attempts != 0
            || job.last_event_seq != 0
        {
            return Err(JobStoreError::InvalidNewJob);
        }
        if !job.progress.is_valid() {
            return Err(JobStoreError::InvalidProgress);
        }
        let mut cache = self.runtime.cache.lock().await;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(backend)?;
        if sqlx::query("SELECT id FROM jobs WHERE id = ?")
            .bind(job.id.to_string())
            .fetch_optional(&mut *tx)
            .await
            .map_err(backend)?
            .is_some()
        {
            return Err(JobStoreError::AlreadyExists(job.id));
        }
        job.last_event_seq = 1;
        job.updated_at = now().max(job.updated_at);
        save(&mut tx, &job, 1, true).await?;
        tx.commit().await.map_err(backend)?;
        let snapshot = Snapshot {
            job,
            durable_seq: 1,
            reserved: 1,
        };
        cache.insert(snapshot.job.id, Cached::from_snapshot(&snapshot, None));
        self.publish(&snapshot.job, JobEventKind::Created);
        Ok(snapshot.job)
    }

    async fn get(&self, id: JobId) -> Result<Job, JobStoreError> {
        let cache = self.runtime.cache.lock().await;
        let mut conn = self.pool.acquire().await.map_err(backend)?;
        let mut snapshot = Self::load(&mut conn, id).await?;
        self.overlay(&mut snapshot, &cache);
        Ok(snapshot.job)
    }

    async fn list(&self) -> Result<Vec<Job>, JobStoreError> {
        let cache = self.runtime.cache.lock().await;
        let rows =
            sqlx::query("SELECT * FROM jobs ORDER BY priority, queue_position, created_at, id")
                .fetch_all(&self.pool)
                .await
                .map_err(backend)?;
        rows.into_iter()
            .map(|row| {
                let mut snapshot = decode(row)?;
                self.overlay(&mut snapshot, &cache);
                Ok(snapshot.job)
            })
            .collect()
    }

    async fn transition(
        &self,
        id: JobId,
        to: JobState,
        failure: Option<Failure>,
    ) -> Result<Job, JobStoreError> {
        self.mutate(id, |job| {
            let from = job.state;
            apply_transition(job, to, failure)?;
            Ok(JobEventKind::StateChanged { from, to })
        })
        .await
    }

    async fn resume(&self, id: JobId) -> Result<Job, JobStoreError> {
        self.mutate(id, |job| {
            let to = job.resume_state.unwrap_or(JobState::Queued);
            if job.state != JobState::Paused {
                return Err(TransitionError::Illegal {
                    from: job.state,
                    to,
                }
                .into());
            }
            apply_transition(job, to, None)?;
            Ok(JobEventKind::StateChanged {
                from: JobState::Paused,
                to,
            })
        })
        .await
    }

    async fn update_progress(&self, id: JobId, progress: Progress) -> Result<Job, JobStoreError> {
        if !progress.is_valid() {
            return Err(JobStoreError::InvalidProgress);
        }
        let mut cache = self.runtime.cache.lock().await;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(backend)?;
        let mut snapshot = Self::load(&mut tx, id).await?;
        let last_progress = self.overlay(&mut snapshot, &cache);
        if snapshot.job.state.is_terminal() {
            return Err(JobStoreError::TerminalProgress);
        }
        let instant = Instant::now();
        let emit = last_progress
            .is_none_or(|last| instant.duration_since(last) >= self.runtime.progress_interval);
        snapshot.job.progress = progress.clone();
        snapshot.job.updated_at = now().max(snapshot.job.updated_at);
        if emit {
            let old_reserved = snapshot.reserved;
            Self::allocate(&mut snapshot)?;
            if old_reserved != snapshot.reserved {
                // Only lease allocation writes; neither progress nor each event is persisted.
                sqlx::query("UPDATE jobs SET seq_reserved = ? WHERE id = ?")
                    .bind(snapshot.reserved.to_be_bytes().to_vec())
                    .bind(id.to_string())
                    .execute(&mut *tx)
                    .await
                    .map_err(backend)?;
            }
        }
        tx.commit().await.map_err(backend)?;
        cache.insert(
            id,
            Cached::from_snapshot(&snapshot, if emit { Some(instant) } else { last_progress }),
        );
        if emit {
            self.publish(&snapshot.job, JobEventKind::Progress { progress });
        }
        Ok(snapshot.job)
    }

    async fn set_queue_order(
        &self,
        id: JobId,
        priority: Priority,
        queue_position: u64,
    ) -> Result<Job, JobStoreError> {
        self.mutate(id, |job| {
            job.priority = priority;
            job.queue_position = queue_position;
            Ok(JobEventKind::Message {
                message: "Queue order updated".into(),
            })
        })
        .await
    }

    async fn message(&self, id: JobId, message: String) -> Result<Job, JobStoreError> {
        self.mutate(id, |_| Ok(JobEventKind::Message { message }))
            .await
    }

    async fn recover(&self, id: JobId, files_exist: bool) -> Result<Job, JobStoreError> {
        self.mutate(id, |job| {
            let recovery = recover_after_restart(job, files_exist);
            if recovery.from != recovery.to {
                Ok(JobEventKind::StateChanged {
                    from: recovery.from,
                    to: recovery.to,
                })
            } else {
                Ok(JobEventKind::Message {
                    message: format!("Restart recovery: re-enter {:?}", recovery.to),
                })
            }
        })
        .await
    }
}
