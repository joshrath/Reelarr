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
use codec::{Snapshot, backend, decode, invalid, save};

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
