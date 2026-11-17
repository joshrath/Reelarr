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
//! downloads stay `Downloading` and are requeued by the next startup. No new
//! import starts; running imports are not interrupted and are joined too.
//!
//! **Admission.** [`Supervisor::enqueue_nzb`] parses and stores an NZB and
//! creates its `Queued` job (the NZB worker's own admission order: NZB
//! first, then the job), places it behind any reordered queue, and then
//! calls [`Supervisor::wake`]. Upload and indexer enqueue in the job API both
//! go through it; there is no second dispatch path.
//!
//! **Import.** With an [`ImportHandler`] set ([`Supervisor::set_import_handler`];
//! the server sets the media library's), every NZB job in `ReadyForImport`
//! is imported: downloads that just finished, and jobs that were already
//! waiting when the server last stopped (restart recovery also turns an
//! interrupted `Importing` back into `ReadyForImport`). Imports do not need
//! NNTP settings and run beside downloads, at most
//! `max_concurrent_imports` at once, each claiming the job with
//! `ReadyForImport → Importing` first ([`attempt_import`]). The download's
//! folder is the one the worker recorded as the job's recovery path; the movie
//! comes from the stored NZB.
//!
//! - `Imported`: the job is `Completed`, then the source cleanup for its
//!   download source runs (built-in NZB: staging and published folder). Cleanup
//!   runs only after `Completed`, never after a failure or a cancel.
//! - Retryable failure: the job goes back to `ReadyForImport` and waits
//!   `import_retry` (1, 5, 15 minutes, then every 30 minutes) before the next
//!   attempt. The wait is kept in memory; a restart tries again at once.
//! - Terminal failure: `Failed` with the handler's code (`Other` if none).
//! - No match: `Failed` with `NoMatch`.
//!
//! Without a handler a finished download stops at `ReadyForImport`.
//!
//! **Discard.** [`Supervisor::discard`] removes a cancelled job's staging
//! directory and unfinished publication. If the job's download run is still
//! stopping, or an import of it is still running (from either path above),
//! it is deferred until that task has returned, so files are never removed
//! under a running worker or importer. An import registers before it re-reads
//! the job, so a cancel that commits after the re-read always finds it.
//!
//! The job HTTP API is in [`crate::jobs`].

mod runner;

pub use runner::{
    ArticleConnector, BuildError, JobRunner, NzbRunnerFactory, PoolConnector, RunnerFactory,
    ServerWorker, articles_per_job, server_worker,
};

use std::{
    collections::{HashMap, HashSet},
    io,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use reelarr_core::{
    CleanupRegistry, CompletedDownload, DownloadSource, Failure, FailureCode, ImportAttempt,
    ImportHandler, Job, JobId, JobKind, JobState, JobStore, JobStoreError, PathMapping, Priority,
    attempt_import,
};
use reelarr_db::{SqliteJobStore, SqliteNntpStore, SqliteNzbStore};
use reelarr_nzb::{
    Segment, nntp,
    worker::{
        ArticleSource, CancelToken, EnqueueError, NzbWorker, Outcome, WorkerConfig, WorkerError,
    },
};
use reelarr_types::MovieId;
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
    /// Imports running at once (at least 1). Separate from downloads.
    pub max_concurrent_imports: usize,
    /// Wait before each retry of a retryable import failure: the n-th retry
    /// waits entry n, and the last entry repeats.
    pub import_retry: Vec<Duration>,
}

/// The agreed import retry schedule: 1, 5 and 15 minutes, then every 30.
pub const IMPORT_RETRY: [Duration; 4] = [
    Duration::from_secs(60),
    Duration::from_secs(5 * 60),
    Duration::from_secs(15 * 60),
    Duration::from_secs(30 * 60),
];

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            max_concurrent_jobs: 2,
            poll_interval: Duration::from_secs(5),
            error_backoff: Duration::from_secs(30),
            max_error_backoff: Duration::from_secs(600),
            max_concurrent_imports: 1,
            import_retry: IMPORT_RETRY.to_vec(),
        }
    }
}

impl SupervisorConfig {
    /// Wait before retry number `retry` (1-based) of an import.
    pub fn import_retry_delay(&self, retry: u32) -> Duration {
        let index = usize::try_from(retry.saturating_sub(1)).unwrap_or(usize::MAX);
        self.import_retry
            .get(index)
            .or(self.import_retry.last())
            .copied()
            .unwrap_or(IMPORT_RETRY[3])
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

/// What to queue with [`Supervisor::enqueue_nzb`].
#[derive(Clone, Debug, Default)]
pub struct NewDownload {
    /// Used when the NZB has no title of its own.
    pub fallback_title: String,
    /// Wins over the NZB's own category.
    pub category: Option<String>,
    pub movie_id: Option<MovieId>,
    pub priority: Priority,
}

/// Why [`Supervisor::enqueue_nzb`] created no job.
#[derive(Debug, thiserror::Error)]
pub enum AdmitError {
    #[error(transparent)]
    Enqueue(#[from] EnqueueError),
    /// The job exists but its queue placement could not be saved.
    #[error(transparent)]
    Jobs(#[from] JobStoreError),
    #[error("NZB admission task failed")]
    Join,
}

/// How [`Supervisor::discard`] handled the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Discarded {
    /// Staging files are gone (or were never there).
    Now,
    /// The job's run is still stopping; files go when it returns.
    WhenStopped,
}

/// Why [`Supervisor::discard`] removed nothing.
#[derive(Debug, thiserror::Error)]
pub enum DiscardError {
    #[error("only cancelled jobs are discarded (job is {0:?})")]
    NotCancelled(JobState),
    #[error(transparent)]
    Jobs(#[from] JobStoreError),
    #[error("removing staging files failed: {0}")]
    Io(#[from] io::Error),
}

/// The admission/discard worker never downloads, so it has no article source.
#[derive(Clone, Copy, Debug)]
struct NoArticles;
impl ArticleSource for NoArticles {
    async fn fetch(&self, _: &Segment) -> Result<Vec<u8>, nntp::Error> {
        Err(nntp::Error::Closed)
    }
}

/// Import wiring: the handler and the cleanup to run after `Completed`.
struct Importer {
    handler: Arc<dyn ImportHandler>,
    cleanups: Arc<CleanupRegistry>,
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
    /// Admission and discard only; never runs a job.
    files: Arc<NzbWorker<SqliteNzbStore, NoArticles>>,
    import: OnceLock<Importer>,
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
    /// Jobs being imported right now (post-download and leftover alike).
    importing: HashSet<JobId>,
    /// Retry wait after a retryable import failure.
    import_held: HashMap<JobId, Hold>,
    /// Cancelled jobs whose staging files go once their run or import returns.
    discard: HashSet<JobId>,
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
type ImportTaskResult = (JobId, Result<ImportAttempt, JobStoreError>);

fn importable(job: &Job) -> bool {
    job.kind == JobKind::Nzb && job.state == JobState::ReadyForImport
}

/// Claim the job and fail it with `failure` (no download to import).
async fn fail_import(
    store: &dyn JobStore,
    id: JobId,
    failure: Failure,
) -> Result<ImportAttempt, JobStoreError> {
    store.transition(id, JobState::Importing, None).await?;
    store
        .transition(id, JobState::Failed, Some(failure))
        .await
        .map(ImportAttempt::Finished)
}

fn dispatchable(job: &Job) -> bool {
    use JobState::*;
    job.kind == JobKind::Nzb
        && matches!(
            job.state,
            Queued | Downloading | Verifying | Repairing | Extracting
        )
}

impl Supervisor {
    /// `worker` must be the same folders and parse limits the factory's
    /// workers use; admission and discard work on them.
    pub fn new(
        jobs: Arc<SqliteJobStore>,
        nzbs: SqliteNzbStore,
        nntp: SqliteNntpStore,
        factory: Arc<dyn RunnerFactory>,
        worker: WorkerConfig,
        mut config: SupervisorConfig,
    ) -> Self {
        config.max_concurrent_jobs = config.max_concurrent_jobs.max(1);
        config.max_concurrent_imports = config.max_concurrent_imports.max(1);
        let store: Arc<dyn JobStore> = jobs.clone();
        let files = Arc::new(NzbWorker::new(store, nzbs.clone(), NoArticles, worker));
        Self {
            inner: Arc::new(Inner {
                jobs,
                nzbs,
                nntp,
                factory,
                files,
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

    /// Import every NZB job in `ReadyForImport` with `handler` (set before
    /// start; a second call is ignored). The built-in NZB source cleanup is
    /// registered with it and runs after `Completed` only.
    pub fn set_import_handler(&self, handler: Arc<dyn ImportHandler>) {
        let mut cleanups = CleanupRegistry::default();
        cleanups.register(
            DownloadSource::BuiltinNzb,
            Arc::new(self.inner.files.cleanup()),
        );
        let _ = self.inner.import.set(Importer {
            handler,
            cleanups: Arc::new(cleanups),
        });
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

    /// Cancel a job and stop its run. Files stay until [`Self::discard`].
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

    /// Admit an NZB as a new `Queued` job, then wake the scheduler. Parsing
    /// runs off the async workers. The job is placed behind any reordered
    /// queue; a non-normal `priority` is applied before the wake.
    pub async fn enqueue_nzb(&self, nzb: Vec<u8>, new: NewDownload) -> Result<Job, AdmitError> {
        let files = self.inner.files.clone();
        let runtime = tokio::runtime::Handle::current();
        let NewDownload {
            fallback_title,
            category,
            movie_id,
            priority,
        } = new;
        let job = tokio::task::spawn_blocking(move || {
            runtime.block_on(files.enqueue(nzb.as_slice(), &fallback_title, category, movie_id))
        })
        .await
        .map_err(|_| AdmitError::Join)??;
        // Wake even if placement fails: the job exists and must not sit unseen.
        let placed = self.place(job, priority).await;
        self.wake();
        placed.map_err(AdmitError::from)
    }

    async fn place(&self, job: Job, priority: Priority) -> Result<Job, JobStoreError> {
        let mut job = self.inner.jobs.append_to_queue(job.id).await?;
        if priority != job.priority {
            job = self
                .inner
                .jobs
                .set_queue_order(job.id, priority, job.queue_position)
                .await?;
        }
        Ok(job)
    }

    /// Remove a cancelled job's staging directory and unfinished publication.
    /// While its download run is still stopping or an import of it is still
    /// running, this is deferred until that task returns. Idempotent.
    pub async fn discard(&self, id: JobId) -> Result<Discarded, DiscardError> {
        let job = self.inner.jobs.get(id).await?;
        if job.state != JobState::Cancelled {
            return Err(DiscardError::NotCancelled(job.state));
        }
        {
            let mut state = lock(&self.inner.state);
            if state.active.contains_key(&id) || state.importing.contains(&id) {
                state.discard.insert(id);
                return Ok(Discarded::WhenStopped);
            }
        }
        self.inner.files.discard(id).await?;
        Ok(Discarded::Now)
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

    /// Jobs being imported right now.
    pub fn importing(&self) -> Vec<JobId> {
        let state = lock(&self.inner.state);
        let mut jobs: Vec<_> = state.importing.iter().copied().collect();
        jobs.sort();
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

    /// Stop dispatching, stop every running job, wait for all of them (and
    /// for running imports) and close every pool. Idempotent.
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
        let mut imports: JoinSet<ImportTaskResult> = JoinSet::new();
        let mut import_jobs: HashMap<tokio::task::Id, JobId> = HashMap::new();
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
            self.dispatch_imports(&mut imports, &mut import_jobs).await;
            tokio::select! {
                biased;
                () = self.stop.cancelled() => break,
                Some(done) = tasks.join_next_with_id(), if !tasks.is_empty() => {
                    if let Some(old) = self.finish(done, &mut task_jobs).await {
                        retire(&mut retiring, old);
                    }
                }
                Some(done) = imports.join_next_with_id(), if !imports.is_empty() => {
                    self.finish_import(done, &mut import_jobs).await;
                }
                Some(_) = retiring.join_next(), if !retiring.is_empty() => {}
                () = self.wake.notified() => {}
                () = tokio::time::sleep(self.config.poll_interval) => {}
            }
        }
        // Graceful stop: no new work, stop and join every running job. Imports
        // cannot be interrupted safely; they are joined, and none starts now.
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
        while let Some(done) = imports.join_next_with_id().await {
            self.finish_import(done, &mut import_jobs).await;
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
                    self.release(id).await;
                    continue;
                }
            }
            tracing::debug!(job = %id, generation = generation.id, "dispatching download");
            let runner = generation.runner.clone();
            // The import starts from the scheduler once this run has returned,
            // so it is never in both registries.
            let handle = tasks.spawn(async move {
                let ran = match runner.run(id, &cancel).await {
                    Ok(outcome) => Ran::Outcome(outcome),
                    Err(error) => Ran::Error(error),
                };
                (id, ran)
            });
            task_jobs.insert(handle.id(), id);
        }
    }

    /// Start imports for `ReadyForImport` jobs, up to the import limit. Needs
    /// no NNTP generation.
    async fn dispatch_imports(
        &self,
        tasks: &mut JoinSet<ImportTaskResult>,
        task_jobs: &mut HashMap<tokio::task::Id, JobId>,
    ) {
        let Some(importer) = self.import.get() else {
            return;
        };
        {
            let state = lock(&self.state);
            if state.stopping || state.importing.len() >= self.config.max_concurrent_imports {
                return;
            }
        }
        let queue = match self.jobs.list().await {
            Ok(jobs) => jobs,
            Err(error) => {
                tracing::error!(%error, "could not read the job queue for import");
                return;
            }
        };
        for job in queue.into_iter().filter(importable) {
            let id = job.id;
            // Register before re-reading the job, so a cancel that commits
            // after the re-read always finds the import and defers discard.
            {
                let mut state = lock(&self.state);
                if state.stopping || state.importing.len() >= self.config.max_concurrent_imports {
                    return;
                }
                if state.importing.contains(&id)
                    // Its download run has not returned yet.
                    || state.active.contains_key(&id)
                    || state
                        .import_held
                        .get(&id)
                        .is_some_and(|h| h.until > Instant::now())
                {
                    continue;
                }
                state.importing.insert(id);
            }
            let download = match self.jobs.get(id).await {
                Ok(job) if importable(&job) => self.completed_download(&job).await,
                other => {
                    if let Err(error) = other {
                        tracing::warn!(job = %id, %error, "could not re-read job before import");
                    }
                    self.release_import(id).await;
                    continue;
                }
            };
            tracing::debug!(job = %id, "starting import");
            let jobs = self.jobs.clone();
            let handler = importer.handler.clone();
            let cleanups = importer.cleanups.clone();
            let handle = tasks.spawn(async move {
                let store: &dyn JobStore = &*jobs;
                let result = match download {
                    Ok(download) => {
                        attempt_import(store, &*handler, &cleanups, download, PathMapping::Local)
                            .await
                    }
                    Err(failure) => fail_import(store, id, failure).await,
                };
                (id, result)
            });
            task_jobs.insert(handle.id(), id);
        }
    }

    /// The completed download of a `ReadyForImport` NZB job, from the folder
    /// the worker recorded and the movie stored with the NZB. `Err` is the
    /// failure to record when the folder was never recorded.
    async fn completed_download(&self, job: &Job) -> Result<CompletedDownload, Failure> {
        let output_path = match self.jobs.recovery_path(job.id).await {
            Ok(Some(path)) => path,
            Ok(None) => {
                return Err(Failure {
                    code: FailureCode::Other,
                    message: "The finished download's folder was not recorded; retry the download"
                        .into(),
                    retryable: true,
                });
            }
            Err(error) => {
                tracing::warn!(job = %job.id, %error, "could not read the download folder");
                return Err(Failure {
                    code: FailureCode::Other,
                    message: "The finished download's folder could not be read".into(),
                    retryable: true,
                });
            }
        };
        // A missing NZB row or unreadable movie leaves `None`; the import
        // then fails as a download without a movie id.
        let movie_id = match self.nzbs.movie_id(job.id).await {
            Ok(movie) => movie.flatten(),
            Err(error) => {
                tracing::warn!(job = %job.id, %error, "could not read the download's movie");
                None
            }
        };
        Ok(CompletedDownload {
            job_id: job.id,
            movie_id,
            output_path,
            category: job.category.clone(),
            source: DownloadSource::BuiltinNzb,
            release_title: job.title.clone(),
        })
    }

    /// Record a finished import and run a discard deferred while it ran.
    async fn finish_import(
        &self,
        done: Result<(tokio::task::Id, ImportTaskResult), tokio::task::JoinError>,
        task_jobs: &mut HashMap<tokio::task::Id, JobId>,
    ) {
        let (id, result) = match done {
            Ok((task, (id, result))) => {
                task_jobs.remove(&task);
                (id, Ok(result))
            }
            Err(error) => {
                let Some(id) = task_jobs.remove(&error.id()) else {
                    return;
                };
                (id, Err(error))
            }
        };
        let retry = match result {
            Ok(Ok(ImportAttempt::Finished(job))) => {
                match job.state {
                    JobState::Completed => tracing::info!(job = %id, "download imported"),
                    JobState::Failed => tracing::info!(
                        job = %id,
                        code = ?job.failure.as_ref().map(|f| f.code),
                        "import failed"
                    ),
                    state => tracing::debug!(job = %id, ?state, "import finished"),
                }
                false
            }
            Ok(Ok(ImportAttempt::Retry { failure, .. })) => {
                tracing::info!(job = %id, code = ?failure.code, "import will be retried");
                true
            }
            Ok(Err(JobStoreError::Transition(error))) => {
                // Cancelled (or otherwise moved on) while importing.
                tracing::debug!(job = %id, %error, "job no longer importable");
                false
            }
            Ok(Err(error)) => {
                tracing::error!(job = %id, %error, "import could not record its result; retrying later");
                true
            }
            Err(error) => {
                tracing::error!(job = %id, %error, "import task did not finish");
                let failure = Failure {
                    code: FailureCode::Other,
                    message: "The importer crashed while processing this job".into(),
                    retryable: true,
                };
                self.jobs
                    .transition(id, JobState::Failed, Some(failure))
                    .await
                    .is_err()
            }
        };
        let deferred = {
            let mut state = lock(&self.state);
            state.importing.remove(&id);
            if retry {
                let failures = state.import_held.get(&id).map_or(0, |h| h.failures) + 1;
                let delay = self.config.import_retry_delay(failures);
                state.import_held.insert(
                    id,
                    Hold {
                        until: Instant::now() + delay,
                        failures,
                    },
                );
            } else {
                state.import_held.remove(&id);
            }
            !state.active.contains_key(&id) && state.discard.remove(&id)
        };
        if deferred {
            self.discard_now(id).await;
        }
    }

    /// Drop a job from the import registry and run a discard deferred meanwhile.
    async fn release_import(&self, id: JobId) {
        let deferred = {
            let mut state = lock(&self.state);
            state.importing.remove(&id);
            !state.active.contains_key(&id) && state.discard.remove(&id)
        };
        if deferred {
            self.discard_now(id).await;
        }
    }

    /// Drop a job from the registry and run a discard deferred while it ran.
    async fn release(&self, id: JobId) {
        let deferred = {
            let mut state = lock(&self.state);
            state.active.remove(&id);
            !state.importing.contains(&id) && state.discard.remove(&id)
        };
        if deferred {
            self.discard_now(id).await;
        }
    }

    async fn discard_now(&self, id: JobId) {
        match self.files.discard(id).await {
            Ok(()) => tracing::debug!(job = %id, "discarded staging files of cancelled job"),
            Err(error) => {
                tracing::warn!(job = %id, %error, "could not discard staging files of cancelled job")
            }
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
        let (retire, deferred) = self.settle(id, hold);
        if deferred {
            self.discard_now(id).await;
        }
        retire
    }

    /// Registry bookkeeping for a finished task: (generation to retire,
    /// whether a deferred discard is due).
    fn settle(&self, id: JobId, hold: bool) -> (Option<Arc<Generation>>, bool) {
        let mut state = lock(&self.state);
        let generation = state.active.remove(&id).map(|a| a.generation);
        let deferred = !state.importing.contains(&id) && state.discard.remove(&id);
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
        let Some(generation) = generation else {
            return (None, deferred);
        };
        if state.active.values().any(|a| a.generation == generation) {
            return (None, deferred);
        }
        let retire = state
            .draining
            .iter()
            .position(|g| g.id == generation)
            .map(|index| state.draining.swap_remove(index));
        (retire, deferred)
    }
}

fn retire(retiring: &mut JoinSet<()>, generation: Arc<Generation>) {
    tracing::info!(generation = generation.id, "retiring idle NNTP pool");
    retiring.spawn(async move { generation.runner.retire().await });
}
