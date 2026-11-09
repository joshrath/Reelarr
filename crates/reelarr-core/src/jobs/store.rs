use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use tokio::sync::RwLock;

use super::{
    EventHub, Failure, Job, JobEvent, JobEventKind, JobId, JobState, Priority, Progress,
    TransitionError, model::now, recover_after_restart, transition::apply_transition,
};

#[derive(Debug, thiserror::Error)]
pub enum JobStoreError {
    #[error("job not found: {0}")]
    NotFound(JobId),
    #[error("job already exists: {0}")]
    AlreadyExists(JobId),
    #[error("new jobs must be unstarted Queued snapshots with no event history")]
    InvalidNewJob,
    #[error(transparent)]
    Transition(#[from] TransitionError),
    #[error("invalid progress counters or stage fraction")]
    InvalidProgress,
    #[error("cannot update progress of a terminal job")]
    TerminalProgress,
    #[error("event sequence exhausted for job {0}")]
    SequenceExhausted(JobId),
    #[error("job storage failed: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Single authority for state changes and per-job seq allocation. Implementations
/// atomically persist state/ordering/seq before publishing events. Progress stays in
/// a hot cache and is broadcast at a bounded rate, not written for every update.
/// reelarr-db owns the SQL adapter/migrations; NZB payload/segment checkpoints
/// belong to NzbStore keyed by JobId and must be batched independently of events.
#[async_trait]
pub trait JobStore: Send + Sync {
    fn events(&self) -> EventHub;
    async fn insert(&self, job: Job) -> Result<Job, JobStoreError>;
    async fn get(&self, id: JobId) -> Result<Job, JobStoreError>;
    /// All snapshots sorted by priority, queue_position, creation time, then id.
    async fn list(&self) -> Result<Vec<Job>, JobStoreError>;
    async fn transition(
        &self,
        id: JobId,
        to: JobState,
        failure: Option<Failure>,
    ) -> Result<Job, JobStoreError>;
    async fn update_progress(&self, id: JobId, progress: Progress) -> Result<Job, JobStoreError>;
    async fn set_queue_order(
        &self,
        id: JobId,
        priority: Priority,
        queue_position: u64,
    ) -> Result<Job, JobStoreError>;
    async fn message(&self, id: JobId, message: String) -> Result<Job, JobStoreError>;
    /// Startup-only, before workers run. May bypass normal transition edges.
    async fn recover(&self, id: JobId, files_exist: bool) -> Result<Job, JobStoreError>;

    async fn resume(&self, id: JobId) -> Result<Job, JobStoreError>;
}

struct StoredJob {
    job: Job,
    last_progress_event: Option<Instant>,
}

pub struct InMemoryJobStore {
    jobs: RwLock<HashMap<JobId, StoredJob>>,
    events: EventHub,
    progress_interval: Duration,
}

impl Default for InMemoryJobStore {
    fn default() -> Self {
        Self::new(EventHub::default(), Duration::from_millis(250))
    }
}

impl InMemoryJobStore {
    pub fn new(events: EventHub, progress_interval: Duration) -> Self {
        Self {
            jobs: RwLock::new(HashMap::new()),
            events,
            progress_interval,
        }
    }

    fn publish(&self, job: &mut Job, kind: JobEventKind, seq: u64) {
        job.last_event_seq = seq;
        job.updated_at = now().max(job.updated_at);
        self.events.publish(JobEvent {
            job_id: job.id,
            seq,
            at: job.updated_at,
            kind,
        });
    }

    async fn mutate(
        &self,
        id: JobId,
        update: impl FnOnce(&mut Job) -> Result<JobEventKind, JobStoreError>,
    ) -> Result<Job, JobStoreError> {
        let mut jobs = self.jobs.write().await;
        let stored = jobs.get_mut(&id).ok_or(JobStoreError::NotFound(id))?;
        let seq = next_seq(&stored.job)?;
        // Never leave a partial mutation behind when validation fails.
        let mut updated = stored.job.clone();
        let event = update(&mut updated)?;
        self.publish(&mut updated, event, seq);
        stored.job = updated;
        Ok(stored.job.clone())
    }
}

fn next_seq(job: &Job) -> Result<u64, JobStoreError> {
    job.last_event_seq
        .checked_add(1)
        .ok_or(JobStoreError::SequenceExhausted(job.id))
}

#[async_trait]
impl JobStore for InMemoryJobStore {
    fn events(&self) -> EventHub {
        self.events.clone()
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
        let mut jobs = self.jobs.write().await;
        if jobs.contains_key(&job.id) {
            return Err(JobStoreError::AlreadyExists(job.id));
        }
        self.publish(&mut job, JobEventKind::Created, 1);
        jobs.insert(
            job.id,
            StoredJob {
                job: job.clone(),
                last_progress_event: None,
            },
        );
        Ok(job)
    }

    async fn get(&self, id: JobId) -> Result<Job, JobStoreError> {
        self.jobs
            .read()
            .await
            .get(&id)
            .map(|j| j.job.clone())
            .ok_or(JobStoreError::NotFound(id))
    }

    async fn list(&self) -> Result<Vec<Job>, JobStoreError> {
        let mut jobs: Vec<_> = self
            .jobs
            .read()
            .await
            .values()
            .map(|j| j.job.clone())
            .collect();
        jobs.sort_by_key(|j| (j.priority, j.queue_position, j.created_at, j.id));
        Ok(jobs)
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
        let mut jobs = self.jobs.write().await;
        let stored = jobs.get_mut(&id).ok_or(JobStoreError::NotFound(id))?;
        if stored.job.state.is_terminal() {
            return Err(JobStoreError::TerminalProgress);
        }
        let instant = Instant::now();
        let emit = stored
            .last_progress_event
            .is_none_or(|last| instant.duration_since(last) >= self.progress_interval);
        let seq = if emit {
            Some(next_seq(&stored.job)?)
        } else {
            None
        };
        stored.job.progress = progress.clone();
        stored.job.updated_at = now().max(stored.job.updated_at);
        if let Some(seq) = seq {
            self.publish(&mut stored.job, JobEventKind::Progress { progress }, seq);
            stored.last_progress_event = Some(instant);
        }
        Ok(stored.job.clone())
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
