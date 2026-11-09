//! Download supervisor (#76 PR 4): runs queued NZB jobs on the built-in
//! worker.
//!
//! **Startup.** [`Supervisor::start`] runs `SqliteJobStore::recover_on_startup`
//! (Downloading → Queued; Verifying/Repairing/Extracting re-enter when their
//! recorded artifacts exist, otherwise Queued; Importing → ReadyForImport) and
//! NZB-store reconciliation *before* anything is dispatched, then loads the
//! NNTP provider and starts the scheduling loop. Call it before the listener
//! is bound; an error stops startup.
//!
//! **Scheduling.** At most `max_concurrent_jobs` jobs run at once. This is a
//! job limit, separate from the provider's connection limit and the worker's
//! per-job article concurrency. Free slots are filled in the job store's
//! order: priority (Force first), then queue position, then age. A running job
//! is never pre-empted, not even for a Force job; Force only goes first for
//! the next free slot. Each running job has an entry in the active registry
//! with its own [`CancelToken`], so a job is never dispatched twice.
//!
//! **NNTP settings generations.** Each loaded provider becomes a generation:
//! one connection pool and one worker. When settings change
//! ([`Supervisor::nntp_settings_changed`]) a new generation is built from
//! `SqliteNntpStore::load`. New dispatches use it; jobs already running finish
//! on the generation they started with (idle drain). The old pool is shut
//! down once its last job ends. With no provider, a disabled one, or one that
//! cannot be loaded, nothing is dispatched and jobs stay where they are.
//!
//! **Pause and cancel.** [`Supervisor::pause`] and [`Supervisor::cancel`]
//! change the job state first, then fire the job's token; the worker stops at
//! its next checkpoint without changing state. Resume puts the job back in
//! the queue.
//!
//! **Errors.** A worker infrastructure error (database, NZB store) leaves the
//! job where it is and holds it back with exponential backoff, so a broken job
//! cannot spin. A panicking run fails the job when its state allows it, and is
//! otherwise held back the same way. Neither stops the supervisor.
//!
//! **Shutdown.** [`Supervisor::shutdown`] stops dispatching, fires every
//! running job's token, joins all job tasks and closes every pool. Stopped
//! downloads stay `Downloading` and are requeued by the next startup.
//!
//! Not here yet: the job HTTP API (PR 5), and the media import hookup and
//! cleanup after import (PR 6). A completed download stops at
//! `ReadyForImport` unless an [`ImportHandler`] was set (tests only so far).

mod runner;

pub use runner::{
    ArticleConnector, BuildError, JobRunner, NzbRunnerFactory, PoolConnector, RunnerFactory,
    ServerWorker, articles_per_job, server_worker,
};

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use reelarr_core::{
    CleanupRegistry, Failure, FailureCode, ImportHandler, Job, JobId, JobKind, JobState, JobStore,
    JobStoreError, PathMapping, run_import,
};
use reelarr_db::{SqliteJobStore, SqliteNntpStore, SqliteNzbStore};
use reelarr_nzb::worker::{CancelToken, Outcome, WorkerError};
use tokio::{
    sync::Notify,
    task::{JoinHandle, JoinSet},
};

/// Scheduling limits.
#[derive(Clone, Debug)]
pub struct SupervisorConfig {
    /// Jobs running at once (at least 1).
    pub max_concurrent_jobs: usize,
    /// Safety-net rescan interval; enqueue, resume and settings changes also
    /// wake the scheduler immediately.
    pub poll_interval: Duration,
    /// First hold-back after a worker error; doubles per repeat.
    pub error_backoff: Duration,
    /// Ceiling for the hold-back.
    pub max_error_backoff: Duration,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            max_concurrent_jobs: 2,
            poll_interval: Duration::from_secs(5),
            error_backoff: Duration::from_secs(30),
            max_error_backoff: Duration::from_secs(600),
        }
    }
}

/// Why [`Supervisor::start`] failed.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("download supervisor already started")]
    AlreadyStarted,
    #[error("restart recovery failed: {0}")]
    Recovery(#[from] JobStoreError),
    #[error("NZB store reconciliation failed: {0}")]
    Reconcile(#[from] reelarr_nzb::StoreError),
}

/// A running job as seen by the registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActiveJob {
    pub id: JobId,
    /// NNTP settings generation the job runs on.
    pub generation: u64,
}

/// Cheap to clone; clones share one scheduler.
#[derive(Clone)]
pub struct Supervisor {
    inner: Arc<Inner>,
}

struct Inner {
    jobs: Arc<SqliteJobStore>,
    nzbs: SqliteNzbStore,
    nntp: SqliteNntpStore,
    factory: Arc<dyn RunnerFactory>,
    import: OnceLock<Arc<dyn ImportHandler>>,
    config: SupervisorConfig,
    wake: Notify,
    reload: AtomicBool,
    started: AtomicBool,
    stop: CancelToken,
    task: Mutex<Option<JoinHandle<()>>>,
    state: Mutex<State>,
}

struct Generation {
    id: u64,
    runner: Arc<dyn JobRunner>,
}

#[derive(Default)]
struct State {
    current: Option<Arc<Generation>>,
    generations: u64,
    /// Replaced generations that still have running jobs.
    draining: Vec<Arc<Generation>>,
    active: HashMap<JobId, Active>,
    held: HashMap<JobId, Hold>,
    stopping: bool,
}

struct Active {
    cancel: CancelToken,
    generation: u64,
}

struct Hold {
    until: Instant,
    failures: u32,
}

/// What a job task reports back.
enum Ran {
    Outcome(Outcome),
    Error(WorkerError),
}

type TaskResult = (JobId, Ran);

fn dispatchable(job: &Job) -> bool {
    use JobState::*;
    job.kind == JobKind::Nzb
        && matches!(
            job.state,
            Queued | Downloading | Verifying | Repairing | Extracting
        )
}

impl Supervisor {
    pub fn new(
        jobs: Arc<SqliteJobStore>,
        nzbs: SqliteNzbStore,
        nntp: SqliteNntpStore,
        factory: Arc<dyn RunnerFactory>,
        mut config: SupervisorConfig,
    ) -> Self {
        config.max_concurrent_jobs = config.max_concurrent_jobs.max(1);
        Self {
            inner: Arc::new(Inner {
                jobs,
                nzbs,
                nntp,
                factory,
                import: OnceLock::new(),
                config,
                wake: Notify::new(),
                reload: AtomicBool::new(false),
                started: AtomicBool::new(false),
                stop: CancelToken::new(),
                task: Mutex::new(None),
                state: Mutex::new(State::default()),
            }),
        }
    }

    /// Import each download as soon as it reaches `ReadyForImport` (before
    /// start). The media hookup arrives in PR 6; until then this is for tests.
    pub fn set_import_handler(&self, handler: Arc<dyn ImportHandler>) {
        let _ = self.inner.import.set(handler);
    }

    pub fn config(&self) -> &SupervisorConfig {
        &self.inner.config
    }

    /// Recover, reconcile, load NNTP settings, then start scheduling.
    pub async fn start(&self) -> Result<(), StartError> {
        if self.inner.started.swap(true, Ordering::SeqCst) {
            return Err(StartError::AlreadyStarted);
        }
        // Nothing may run before every interrupted job is reconciled.
        self.inner.jobs.recover_on_startup().await?;
        self.inner.nzbs.reconcile().await?;
        // This load covers any change notified before start.
        self.inner.reload.store(false, Ordering::SeqCst);
        let retired = self.inner.reload_settings().await;
        debug_assert!(retired.is_none());
        let inner = self.inner.clone();
        *lock(&self.inner.task) = Some(tokio::spawn(inner.run_loop()));
        Ok(())
    }

    /// Rescan the queue now (after enqueue or a queue-order change).
    pub fn wake(&self) {
        self.inner.wake.notify_one();
    }

    /// Saved or removed NNTP settings: build a new generation for new jobs.
    pub fn nntp_settings_changed(&self) {
        self.inner.reload.store(true, Ordering::SeqCst);
        self.wake();
    }

    /// Pause a queued or downloading job and stop its run.
    pub async fn pause(&self, id: JobId) -> Result<Job, JobStoreError> {
        let job = self
            .inner
            .jobs
            .transition(id, JobState::Paused, None)
            .await?;
        self.inner.fire(id);
        Ok(job)
    }

    /// Return a paused job to where it was; the scheduler picks it up.
    pub async fn resume(&self, id: JobId) -> Result<Job, JobStoreError> {
        let job = self.inner.jobs.resume(id).await?;
        self.wake();
        Ok(job)
    }

    /// Cancel a job and stop its run. Files are left for the job API's
    /// discard step (PR 5).
    pub async fn cancel(&self, id: JobId) -> Result<Job, JobStoreError> {
        let job = self
            .inner
            .jobs
            .transition(id, JobState::Cancelled, None)
            .await?;
        self.inner.fire(id);
        self.wake();
        Ok(job)
    }

    /// Jobs running right now.
    pub fn active(&self) -> Vec<ActiveJob> {
        let state = lock(&self.inner.state);
        let mut jobs: Vec<_> = state
            .active
            .iter()
            .map(|(&id, a)| ActiveJob {
                id,
                generation: a.generation,
            })
            .collect();
        jobs.sort_by_key(|a| a.id);
        jobs
    }

    /// The generation new jobs are dispatched to, if any provider is usable.
    pub fn current_generation(&self) -> Option<u64> {
        lock(&self.inner.state).current.as_ref().map(|g| g.id)
    }

    /// Replaced generations still finishing jobs.
    pub fn draining_generations(&self) -> Vec<u64> {
        lock(&self.inner.state)
            .draining
            .iter()
            .map(|g| g.id)
            .collect()
    }

    /// Stop dispatching, stop every running job, wait for all of them and
    /// close every pool. Idempotent.
    pub async fn shutdown(&self) {
        self.inner.stop.cancel();
        let task = lock(&self.inner.task).take();
        if let Some(task) = task
            && let Err(error) = task.await
        {
            tracing::error!(%error, "download supervisor task failed");
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // State is only mutated in short, non-panicking sections.
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Inner {
    fn fire(&self, id: JobId) {
        if let Some(active) = lock(&self.state).active.get(&id) {
            active.cancel.cancel();
        }
    }

    /// Load the provider and swap generations. Returns a generation that is
    /// now idle and must be retired by the caller.
    async fn reload_settings(&self) -> Option<Arc<Generation>> {
        let runner = match self.nntp.load().await {
            Ok(Some(server)) if server.enabled => match self.factory.build(server).await {
                Ok(runner) => Some(runner),
                Err(error) => {
                    tracing::error!(%error, "NNTP provider unusable; downloads stay queued");
                    None
                }
            },
            Ok(Some(_)) => {
                tracing::info!("NNTP provider disabled; downloads stay queued");
                None
            }
            Ok(None) => {
                tracing::info!("no NNTP provider configured; downloads stay queued");
                None
            }
            Err(error) => {
                tracing::error!(%error, "NNTP settings unreadable; downloads stay queued");
                None
            }
        };
        let mut state = lock(&self.state);
        let next = runner.map(|runner| {
            state.generations += 1;
            Arc::new(Generation {
                id: state.generations,
                runner,
            })
        });
        if let Some(generation) = &next {
            tracing::info!(generation = generation.id, "NNTP pool ready for new jobs");
        }
        let old = std::mem::replace(&mut state.current, next)?;
        if state.active.values().any(|a| a.generation == old.id) {
            state.draining.push(old);
            None
        } else {
            Some(old)
        }
    }

    async fn run_loop(self: Arc<Self>) {
        let mut tasks: JoinSet<TaskResult> = JoinSet::new();
        let mut task_jobs: HashMap<tokio::task::Id, JobId> = HashMap::new();
        let mut retiring: JoinSet<()> = JoinSet::new();
        loop {
            if self.stop.is_cancelled() {
                break;
            }
            if self.reload.swap(false, Ordering::SeqCst)
                && let Some(old) = self.reload_settings().await
            {
                retire(&mut retiring, old);
            }
            self.dispatch(&mut tasks, &mut task_jobs).await;
            tokio::select! {
                biased;
                () = self.stop.cancelled() => break,
                Some(done) = tasks.join_next_with_id(), if !tasks.is_empty() => {
                    if let Some(old) = self.finish(done, &mut task_jobs).await {
                        retire(&mut retiring, old);
                    }
                }
                Some(_) = retiring.join_next(), if !retiring.is_empty() => {}
                () = self.wake.notified() => {}
                () = tokio::time::sleep(self.config.poll_interval) => {}
            }
        }
        // Graceful stop: no new work, stop and join every running job.
        let generations = {
            let mut state = lock(&self.state);
            state.stopping = true;
            for active in state.active.values() {
                active.cancel.cancel();
            }
            state.current.take()
        };
        while let Some(done) = tasks.join_next_with_id().await {
            if let Some(old) = self.finish(done, &mut task_jobs).await {
                retire(&mut retiring, old);
            }
        }
        let leftover: Vec<_> = {
            let mut state = lock(&self.state);
            std::mem::take(&mut state.draining)
        };
        for generation in generations.into_iter().chain(leftover) {
            retire(&mut retiring, generation);
        }
        while retiring.join_next().await.is_some() {}
        tracing::info!("download supervisor stopped");
    }

    async fn dispatch(
        &self,
        tasks: &mut JoinSet<TaskResult>,
        task_jobs: &mut HashMap<tokio::task::Id, JobId>,
    ) {
        {
            let state = lock(&self.state);
            if state.current.is_none()
                || state.stopping
                || state.active.len() >= self.config.max_concurrent_jobs
            {
                return;
            }
        }
        let queue = match self.jobs.list().await {
            Ok(jobs) => jobs,
            Err(error) => {
                tracing::error!(%error, "could not read the job queue");
                return;
            }
        };
        for job in queue.into_iter().filter(dispatchable) {
            let id = job.id;
            let cancel = CancelToken::new();
            // Register before re-reading the job, so a pause that commits after
            // the re-read always finds this token to fire.
            let generation = {
                let mut state = lock(&self.state);
                if state.stopping || state.active.len() >= self.config.max_concurrent_jobs {
                    return;
                }
                let Some(generation) = state.current.clone() else {
                    return;
                };
                if state.active.contains_key(&id)
                    || state
                        .held
                        .get(&id)
                        .is_some_and(|h| h.until > Instant::now())
                {
                    continue;
                }
                state.active.insert(
                    id,
                    Active {
                        cancel: cancel.clone(),
                        generation: generation.id,
                    },
                );
                generation
            };
            match self.jobs.get(id).await {
                Ok(job) if dispatchable(&job) => {}
                other => {
                    if let Err(error) = other {
                        tracing::warn!(job = %id, %error, "could not re-read job before dispatch");
                    }
                    lock(&self.state).active.remove(&id);
                    continue;
                }
            }
            tracing::debug!(job = %id, generation = generation.id, "dispatching download");
            let runner = generation.runner.clone();
            let jobs = self.jobs.clone();
            let import = self.import.get().cloned();
            let handle = tasks.spawn(async move {
                let ran = match runner.run(id, &cancel).await {
                    Ok(Outcome::ReadyForImport(download)) => {
                        if let Some(handler) = import {
                            let cleanups = CleanupRegistry::default();
                            let store: &dyn JobStore = &*jobs;
                            if let Err(error) = run_import(
                                store,
                                &*handler,
                                &cleanups,
                                download.clone(),
                                PathMapping::Local,
                            )
                            .await
                            {
                                tracing::warn!(job = %id, %error, "import after download failed");
                            }
                        }
                        Ran::Outcome(Outcome::ReadyForImport(download))
                    }
                    Ok(outcome) => Ran::Outcome(outcome),
                    Err(error) => Ran::Error(error),
                };
                (id, ran)
            });
            task_jobs.insert(handle.id(), id);
        }
    }

    /// Record a finished task. Returns a drained generation to retire.
    async fn finish(
        &self,
        done: Result<(tokio::task::Id, TaskResult), tokio::task::JoinError>,
        task_jobs: &mut HashMap<tokio::task::Id, JobId>,
    ) -> Option<Arc<Generation>> {
        let (id, result) = match done {
            Ok((task, (id, ran))) => {
                task_jobs.remove(&task);
                (id, Ok(ran))
            }
            Err(error) => {
                let id = task_jobs.remove(&error.id())?;
                (id, Err(error))
            }
        };
        let hold = match result {
            Ok(Ran::Outcome(outcome)) => {
                match &outcome {
                    Outcome::ReadyForImport(_) => {
                        tracing::info!(job = %id, "download ready for import")
                    }
                    Outcome::Failed(failure) => {
                        tracing::info!(job = %id, code = ?failure.code, "download failed")
                    }
                    Outcome::Stopped => tracing::debug!(job = %id, "download stopped"),
                }
                false
            }
            Ok(Ran::Error(WorkerError::NotRunnable { state, .. })) => {
                // Paused, cancelled or finished between dispatch and run.
                tracing::debug!(job = %id, ?state, "job no longer runnable");
                false
            }
            Ok(Ran::Error(error)) => {
                tracing::error!(job = %id, %error, "download worker error; holding the job back");
                true
            }
            Err(error) => {
                let message = if error.is_panic() {
                    "The download worker crashed while processing this job"
                } else {
                    "The download worker task was aborted"
                };
                tracing::error!(job = %id, %error, "download task did not finish");
                let failure = Failure {
                    code: FailureCode::Other,
                    message: message.into(),
                    retryable: true,
                };
                self.jobs
                    .transition(id, JobState::Failed, Some(failure))
                    .await
                    .is_err()
            }
        };
        let mut state = lock(&self.state);
        let generation = state.active.remove(&id).map(|a| a.generation);
        if hold {
            let failures = state.held.get(&id).map_or(0, |h| h.failures) + 1;
            let factor = 2u32.saturating_pow(failures.saturating_sub(1).min(16));
            let delay = self
                .config
                .error_backoff
                .saturating_mul(factor)
                .min(self.config.max_error_backoff);
            state.held.insert(
                id,
                Hold {
                    until: Instant::now() + delay,
                    failures,
                },
            );
        } else {
            state.held.remove(&id);
        }
        let generation = generation?;
        if state.active.values().any(|a| a.generation == generation) {
            return None;
        }
        let index = state.draining.iter().position(|g| g.id == generation)?;
        Some(state.draining.swap_remove(index))
    }
}

fn retire(retiring: &mut JoinSet<()>, generation: Arc<Generation>) {
    tracing::info!(generation = generation.id, "retiring idle NNTP pool");
    retiring.spawn(async move { generation.runner.retire().await });
}
