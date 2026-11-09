//! Download supervisor scheduling over a scripted runner: ordering,
//! concurrency, errors, pause/resume, restart recovery, idle drain and
//! shutdown. `downloads_worker.rs` covers the real NZB worker wiring.
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use reelarr_core::{
    CompletedDownload, DownloadSource, EventHub, Job, JobId, JobKind, JobState, JobStore, Priority,
};
use reelarr_db::{
    Database, NntpServer, SqliteJobStore, SqliteNntpStore, SqliteNzbStore, SqliteRecoveryPaths,
    secrets::XChaChaSecretCipher,
};
use reelarr_nzb::{
    StoreError,
    worker::{CancelToken, Outcome, RecoveryPaths, WorkerConfig, WorkerError},
};
use reelarr_server::downloads::{
    BuildError, JobRunner, RunnerFactory, StartError, Supervisor, SupervisorConfig,
};
use reelarr_types::nntp::NntpServerUpdate;
use tokio::sync::Semaphore;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Script {
    /// Wait for `release`, then finish at ReadyForImport.
    Block,
    /// Finish at once.
    Complete,
    Panic,
    /// Return a worker infrastructure error.
    Error,
}

#[derive(Clone, Debug)]
struct Start {
    id: JobId,
    host: String,
    state: JobState,
}

#[derive(Default)]
struct Shared {
    starts: Mutex<Vec<Start>>,
    finished: Mutex<Vec<(JobId, &'static str)>>,
    builds: Mutex<Vec<String>>,
    retired: Mutex<Vec<String>>,
    scripts: Mutex<HashMap<JobId, Script>>,
    gates: Mutex<HashMap<JobId, Arc<Semaphore>>>,
    running: AtomicUsize,
    max_running: AtomicUsize,
    per_job: Mutex<HashMap<JobId, usize>>,
    overlap: AtomicUsize,
    /// Extra time a cancelled run takes before returning.
    stop_delay: Mutex<Duration>,
}
impl Shared {
    fn gate(&self, id: JobId) -> Arc<Semaphore> {
        self.gates
            .lock()
            .unwrap()
            .entry(id)
            .or_insert_with(|| Arc::new(Semaphore::new(0)))
            .clone()
    }
    fn release(&self, id: JobId) {
        self.gate(id).add_permits(1);
    }
    fn script(&self, id: JobId, script: Script) {
        self.scripts.lock().unwrap().insert(id, script);
    }
    fn starts(&self) -> Vec<Start> {
        self.starts.lock().unwrap().clone()
    }
    fn started(&self) -> Vec<JobId> {
        self.starts().into_iter().map(|s| s.id).collect()
    }
    fn starts_of(&self, id: JobId) -> usize {
        self.starts().iter().filter(|s| s.id == id).count()
    }
    fn finished_as(&self, id: JobId) -> Vec<&'static str> {
        self.finished
            .lock()
            .unwrap()
            .iter()
            .filter(|(j, _)| *j == id)
            .map(|(_, how)| *how)
            .collect()
    }
}

struct Factory(Arc<Shared>, Arc<SqliteJobStore>);
#[async_trait]
impl RunnerFactory for Factory {
    async fn build(&self, server: NntpServer) -> Result<Arc<dyn JobRunner>, BuildError> {
        let host = server.config.host.clone();
        self.0.builds.lock().unwrap().push(host.clone());
        Ok(Arc::new(Runner {
            host,
            shared: self.0.clone(),
            jobs: self.1.clone(),
        }))
    }
}

struct Runner {
    host: String,
    shared: Arc<Shared>,
    jobs: Arc<SqliteJobStore>,
}

struct Running<'a>(&'a Shared, JobId);
impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.running.fetch_sub(1, Ordering::SeqCst);
        *self.0.per_job.lock().unwrap().get_mut(&self.1).unwrap() -= 1;
    }
}

impl Runner {
    async fn finish(&self, id: JobId, cancel: &CancelToken) -> Result<Outcome, WorkerError> {
        let script = self
            .shared
            .scripts
            .lock()
            .unwrap()
            .get(&id)
            .copied()
            .unwrap_or(Script::Block);
        if script == Script::Error {
            return Err(WorkerError::Nzb(StoreError::Backend));
        }
        let mut job = self.jobs.get(id).await?;
        if job.state == JobState::Queued {
            match self.jobs.transition(id, JobState::Downloading, None).await {
                Ok(j) => job = j,
                Err(_) => return Ok(Outcome::Stopped),
            }
        }
        if script == Script::Panic {
            panic!("scripted panic");
        }
        if script == Script::Block {
            let gate = self.shared.gate(id);
            tokio::select! {
                permit = gate.acquire() => permit.unwrap().forget(),
                () = cancel.cancelled() => {
                    let delay = *self.shared.stop_delay.lock().unwrap();
                    tokio::time::sleep(delay).await;
                    return Ok(Outcome::Stopped);
                }
            }
            job = self.jobs.get(id).await?;
        }
        let path: &[JobState] = match job.state {
            JobState::Downloading => &[JobState::Verifying, JobState::ReadyForImport],
            JobState::Verifying => &[JobState::ReadyForImport],
            JobState::Repairing => &[JobState::Extracting, JobState::ReadyForImport],
            JobState::Extracting => &[JobState::ReadyForImport],
            _ => return Ok(Outcome::Stopped),
        };
        for &to in path {
            if self.jobs.transition(id, to, None).await.is_err() {
                return Ok(Outcome::Stopped);
            }
        }
        Ok(Outcome::ReadyForImport(CompletedDownload {
            job_id: id,
            movie_id: None,
            output_path: PathBuf::from("/downloads/complete/x"),
            category: None,
            source: DownloadSource::BuiltinNzb,
            release_title: job.title,
        }))
    }
}

#[async_trait]
impl JobRunner for Runner {
    async fn run(&self, id: JobId, cancel: &CancelToken) -> Result<Outcome, WorkerError> {
        let state = self.jobs.get(id).await?.state;
        self.shared.starts.lock().unwrap().push(Start {
            id,
            host: self.host.clone(),
            state,
        });
        let now = self.shared.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.shared.max_running.fetch_max(now, Ordering::SeqCst);
        {
            let mut per_job = self.shared.per_job.lock().unwrap();
            let count = per_job.entry(id).or_default();
            *count += 1;
            if *count > 1 {
                self.shared.overlap.fetch_add(1, Ordering::SeqCst);
            }
        }
        let _running = Running(&self.shared, id);
        let result = self.finish(id, cancel).await;
        let how = match &result {
            Ok(Outcome::ReadyForImport(_)) => "ready",
            Ok(Outcome::Failed(_)) => "failed",
            Ok(Outcome::Stopped) => "stopped",
            Err(_) => "error",
        };
        self.shared.finished.lock().unwrap().push((id, how));
        result
    }
    async fn retire(&self) {
        self.shared.retired.lock().unwrap().push(self.host.clone());
    }
}

struct Env {
    _dir: tempfile::TempDir,
    db_path: PathBuf,
    jobs: Arc<SqliteJobStore>,
    nntp: SqliteNntpStore,
    shared: Arc<Shared>,
    supervisor: Supervisor,
}

fn config(max: usize) -> SupervisorConfig {
    SupervisorConfig {
        max_concurrent_jobs: max,
        poll_interval: Duration::from_millis(50),
        error_backoff: Duration::from_millis(200),
        max_error_backoff: Duration::from_millis(400),
        ..SupervisorConfig::default()
    }
}

async fn env_at(dir: tempfile::TempDir, max: usize) -> Env {
    let db_path = dir.path().join("reelarr.db");
    let db = Database::connect(&db_path).await.unwrap();
    let jobs = Arc::new(SqliteJobStore::with_options(
        &db,
        EventHub::default(),
        Duration::ZERO,
    ));
    let nntp = SqliteNntpStore::new(&db, Arc::new(XChaChaSecretCipher::ephemeral().unwrap()));
    let shared = Arc::new(Shared::default());
    let supervisor = Supervisor::new(
        jobs.clone(),
        SqliteNzbStore::new(&db),
        nntp.clone(),
        Arc::new(Factory(shared.clone(), jobs.clone())),
        WorkerConfig::new(dir.path().join("incomplete"), dir.path().join("complete")),
        config(max),
    );
    Env {
        _dir: dir,
        db_path,
        jobs,
        nntp,
        shared,
        supervisor,
    }
}
async fn env(max: usize) -> Env {
    env_at(tempfile::tempdir().unwrap(), max).await
}

impl Env {
    async fn configure(&self, host: &str) {
        self.nntp.save(&NntpServerUpdate::new(host)).await.unwrap();
        self.supervisor.nntp_settings_changed();
    }
    async fn job(&self, title: &str) -> JobId {
        let id = self
            .jobs
            .insert(Job::new(JobKind::Nzb, title))
            .await
            .unwrap()
            .id;
        self.supervisor.wake();
        id
    }
    async fn ordered(&self, title: &str, priority: Priority, position: u64) -> JobId {
        let id = self.job(title).await;
        self.jobs
            .set_queue_order(id, priority, position)
            .await
            .unwrap();
        id
    }
    async fn state(&self, id: JobId) -> JobState {
        self.jobs.get(id).await.unwrap().state
    }
    async fn wait_state(&self, id: JobId, state: JobState) {
        let jobs = self.jobs.clone();
        wait(&format!("{id} reaches {state:?}"), || {
            let jobs = jobs.clone();
            async move { jobs.get(id).await.unwrap().state == state }
        })
        .await;
    }
    async fn wait_started(&self, n: usize) {
        let shared = self.shared.clone();
        wait(&format!("{n} runs started"), || {
            let shared = shared.clone();
            async move { shared.starts().len() >= n }
        })
        .await;
    }
    async fn wait_idle(&self) {
        let supervisor = self.supervisor.clone();
        wait("no active jobs", || {
            let supervisor = supervisor.clone();
            async move { supervisor.active().is_empty() }
        })
        .await;
    }
}

async fn wait<F, Fut>(what: &str, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..500 {
        if condition().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// Long enough for several scheduler passes (poll interval is 50 ms).
async fn settle() {
    tokio::time::sleep(Duration::from_millis(250)).await;
}

#[tokio::test]
async fn missing_or_disabled_settings_dispatch_nothing() {
    let e = env(2).await;
    e.supervisor.start().await.unwrap();
    let a = e.job("a").await;
    settle().await;
    assert!(e.shared.started().is_empty());
    assert!(e.shared.builds.lock().unwrap().is_empty());
    assert_eq!(e.supervisor.current_generation(), None);
    assert_eq!(e.state(a).await, JobState::Queued);

    let mut disabled = NntpServerUpdate::new("news.disabled.test");
    disabled.enabled = false;
    e.nntp.save(&disabled).await.unwrap();
    e.supervisor.nntp_settings_changed();
    settle().await;
    assert!(e.shared.started().is_empty());
    assert_eq!(e.supervisor.current_generation(), None);

    e.shared.script(a, Script::Complete);
    e.configure("news.one.test").await;
    e.wait_state(a, JobState::ReadyForImport).await;
    assert_eq!(e.shared.starts()[0].host, "news.one.test");
    e.supervisor.shutdown().await;
}

#[tokio::test]
async fn settings_present_at_start_are_used() {
    let e = env(1).await;
    e.nntp
        .save(&NntpServerUpdate::new("news.boot.test"))
        .await
        .unwrap();
    let a = e.job("a").await;
    e.shared.script(a, Script::Complete);
    e.supervisor.start().await.unwrap();
    e.wait_state(a, JobState::ReadyForImport).await;
    assert_eq!(e.supervisor.current_generation(), Some(1));
    assert!(matches!(
        e.supervisor.start().await,
        Err(StartError::AlreadyStarted)
    ));
    e.supervisor.shutdown().await;
    assert_eq!(*e.shared.retired.lock().unwrap(), ["news.boot.test"]);
}

#[tokio::test]
async fn dispatch_order_is_priority_then_queue_position() {
    let e = env(1).await;
    // Inserted out of order; names give the expected order.
    let n3 = e.ordered("normal-3", Priority::Normal, 3).await;
    let l0 = e.ordered("low-0", Priority::Low, 0).await;
    let n1 = e.ordered("normal-1", Priority::Normal, 1).await;
    let h5 = e.ordered("high-5", Priority::High, 5).await;
    let f9 = e.ordered("force-9", Priority::Force, 9).await;
    let h2 = e.ordered("high-2", Priority::High, 2).await;
    for id in [n3, l0, n1, h5, f9, h2] {
        e.shared.script(id, Script::Complete);
    }
    e.configure("news.test").await;
    e.supervisor.start().await.unwrap();
    for id in [n3, l0, n1, h5, f9, h2] {
        e.wait_state(id, JobState::ReadyForImport).await;
    }
    assert_eq!(e.shared.started(), [f9, h2, h5, n1, n3, l0]);
    e.supervisor.shutdown().await;
}

#[tokio::test]
async fn force_priority_does_not_preempt_a_running_job() {
    let e = env(1).await;
    e.configure("news.test").await;
    e.supervisor.start().await.unwrap();
    let low = e.ordered("low", Priority::Low, 0).await;
    e.wait_started(1).await;
    let normal = e.ordered("normal", Priority::Normal, 0).await;
    let force = e.ordered("force", Priority::Force, 0).await;
    settle().await;
    // The low job keeps its slot; nothing else started.
    assert_eq!(e.shared.started(), [low]);
    assert!(e.shared.finished_as(low).is_empty());
    assert_eq!(e.state(low).await, JobState::Downloading);
    e.shared.release(low);
    e.wait_started(2).await;
    assert_eq!(e.shared.started(), [low, force]);
    e.shared.release(force);
    e.shared.release(normal);
    e.wait_state(normal, JobState::ReadyForImport).await;
    assert_eq!(e.shared.started(), [low, force, normal]);
    e.supervisor.shutdown().await;
}

#[tokio::test]
async fn concurrency_is_bounded_and_no_job_runs_twice() {
    let e = env(2).await;
    e.configure("news.test").await;
    e.supervisor.start().await.unwrap();
    let mut ids = Vec::new();
    for i in 0..6 {
        ids.push(e.job(&format!("job-{i}")).await);
    }
    e.wait_started(2).await;
    for _ in 0..20 {
        e.supervisor.wake();
        tokio::task::yield_now().await;
    }
    settle().await;
    assert_eq!(e.shared.started().len(), 2);
    assert_eq!(e.supervisor.active().len(), 2);
    for &id in &ids {
        e.shared.release(id);
    }
    for &id in &ids {
        e.wait_state(id, JobState::ReadyForImport).await;
    }
    assert_eq!(e.shared.max_running.load(Ordering::SeqCst), 2);
    assert_eq!(e.shared.overlap.load(Ordering::SeqCst), 0);
    for &id in &ids {
        assert_eq!(e.shared.starts_of(id), 1, "{id} dispatched more than once");
    }
    let unique: HashSet<_> = e.shared.started().into_iter().collect();
    assert_eq!(unique.len(), ids.len());
    e.supervisor.shutdown().await;
}

#[tokio::test]
async fn a_panicking_run_fails_its_job_and_the_scheduler_continues() {
    let e = env(1).await;
    e.configure("news.test").await;
    e.supervisor.start().await.unwrap();
    let bad = e.job("bad").await;
    e.shared.script(bad, Script::Panic);
    let good = e.job("good").await;
    e.shared.script(good, Script::Complete);
    e.wait_state(bad, JobState::Failed).await;
    e.wait_state(good, JobState::ReadyForImport).await;
    let failure = e.jobs.get(bad).await.unwrap().failure.unwrap();
    assert!(failure.message.contains("crashed"), "{}", failure.message);
    settle().await;
    assert_eq!(e.shared.starts_of(bad), 1);
    e.supervisor.shutdown().await;
}

#[tokio::test]
async fn a_worker_error_holds_the_job_back_without_spinning() {
    let e = env(1).await;
    e.configure("news.test").await;
    e.supervisor.start().await.unwrap();
    let flaky = e.job("flaky").await;
    e.shared.script(flaky, Script::Error);
    let other = e.job("other").await;
    e.shared.script(other, Script::Complete);
    e.wait_state(other, JobState::ReadyForImport).await;
    e.wait_started(2).await;
    // Held back 200 ms, then 400 ms: a handful of attempts, not a spin.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let attempts = e.shared.starts_of(flaky);
    assert!((2..=4).contains(&attempts), "{attempts} attempts");
    assert_eq!(e.state(flaky).await, JobState::Queued);
    // Once the cause is gone, the next attempt succeeds.
    e.shared.script(flaky, Script::Complete);
    e.wait_state(flaky, JobState::ReadyForImport).await;
    e.supervisor.shutdown().await;
}

#[tokio::test]
async fn pause_stops_a_running_job_and_resume_requeues_it_once() {
    let e = env(1).await;
    e.configure("news.test").await;
    e.supervisor.start().await.unwrap();
    let a = e.job("a").await;
    e.wait_started(1).await;
    e.wait_state(a, JobState::Downloading).await;
    // The stopping run lingers, so resume races its exit.
    *e.shared.stop_delay.lock().unwrap() = Duration::from_millis(150);
    e.supervisor.pause(a).await.unwrap();
    assert_eq!(e.state(a).await, JobState::Paused);
    e.supervisor.resume(a).await.unwrap();
    assert_eq!(e.state(a).await, JobState::Downloading);
    e.wait_started(2).await;
    assert_eq!(e.shared.finished_as(a), ["stopped"]);
    assert_eq!(e.shared.overlap.load(Ordering::SeqCst), 0);
    e.shared.release(a);
    e.wait_state(a, JobState::ReadyForImport).await;
    assert_eq!(e.shared.starts_of(a), 2);
    e.supervisor.shutdown().await;
}

#[tokio::test]
async fn pause_racing_dispatch_never_runs_a_paused_job() {
    let e = env(4).await;
    e.configure("news.test").await;
    e.supervisor.start().await.unwrap();
    let mut ids = Vec::new();
    for i in 0..15 {
        let id = e.job(&format!("race-{i}")).await;
        if i % 3 == 1 {
            tokio::task::yield_now().await;
        }
        e.supervisor.pause(id).await.unwrap();
        ids.push(id);
    }
    settle().await;
    e.wait_idle().await;
    for &id in &ids {
        assert_eq!(e.state(id).await, JobState::Paused);
        // A run may have started before the pause landed; it must have stopped.
        for how in e.shared.finished_as(id) {
            assert_eq!(how, "stopped");
        }
    }
    let before = e.shared.started().len();
    settle().await;
    assert_eq!(
        e.shared.started().len(),
        before,
        "paused jobs were redispatched"
    );
    for &id in &ids {
        e.shared.script(id, Script::Complete);
        e.supervisor.resume(id).await.unwrap();
    }
    for &id in &ids {
        e.wait_state(id, JobState::ReadyForImport).await;
    }
    assert_eq!(e.shared.overlap.load(Ordering::SeqCst), 0);
    assert!(e.shared.max_running.load(Ordering::SeqCst) <= 4);
    e.supervisor.shutdown().await;
}

#[tokio::test]
async fn cancel_fires_the_job_token() {
    let e = env(1).await;
    e.configure("news.test").await;
    e.supervisor.start().await.unwrap();
    let a = e.job("a").await;
    let b = e.job("b").await;
    e.shared.script(b, Script::Complete);
    e.wait_state(a, JobState::Downloading).await;
    e.supervisor.cancel(a).await.unwrap();
    e.wait_state(b, JobState::ReadyForImport).await;
    assert_eq!(e.state(a).await, JobState::Cancelled);
    assert_eq!(e.shared.finished_as(a), ["stopped"]);
    assert_eq!(e.shared.starts_of(a), 1);
    e.supervisor.shutdown().await;
}

#[tokio::test]
async fn idle_drain_keeps_running_jobs_on_their_pool() {
    let e = env(2).await;
    e.configure("news.old.test").await;
    e.supervisor.start().await.unwrap();
    let a = e.job("a").await;
    e.wait_started(1).await;
    assert_eq!(e.supervisor.current_generation(), Some(1));

    e.configure("news.new.test").await;
    let supervisor = e.supervisor.clone();
    wait("generation 2", || {
        let s = supervisor.clone();
        async move { s.current_generation() == Some(2) }
    })
    .await;
    assert_eq!(e.supervisor.draining_generations(), [1]);
    let b = e.job("b").await;
    e.wait_started(2).await;
    let starts = e.shared.starts();
    assert_eq!(
        (starts[0].id, starts[0].host.as_str()),
        (a, "news.old.test")
    );
    assert_eq!(
        (starts[1].id, starts[1].host.as_str()),
        (b, "news.new.test")
    );
    // Not pre-empted, and the old pool is still open.
    assert!(e.shared.finished_as(a).is_empty());
    assert!(e.shared.retired.lock().unwrap().is_empty());
    let generations: HashMap<_, _> = e
        .supervisor
        .active()
        .into_iter()
        .map(|j| (j.id, j.generation))
        .collect();
    assert_eq!(generations, HashMap::from([(a, 1), (b, 2)]));

    e.shared.release(a);
    e.wait_state(a, JobState::ReadyForImport).await;
    let shared = e.shared.clone();
    wait("old pool retired", || {
        let shared = shared.clone();
        async move { !shared.retired.lock().unwrap().is_empty() }
    })
    .await;
    assert_eq!(*e.shared.retired.lock().unwrap(), ["news.old.test"]);
    assert!(e.supervisor.draining_generations().is_empty());

    // Removing the provider stops new dispatch; the running job is untouched.
    e.nntp.clear().await.unwrap();
    e.supervisor.nntp_settings_changed();
    let supervisor = e.supervisor.clone();
    wait("no generation", || {
        let s = supervisor.clone();
        async move { s.current_generation().is_none() }
    })
    .await;
    let c = e.job("c").await;
    settle().await;
    assert_eq!(e.shared.starts_of(c), 0);
    assert_eq!(e.state(b).await, JobState::Downloading);
    e.shared.release(b);
    e.wait_state(b, JobState::ReadyForImport).await;
    wait("new pool retired", || {
        let shared = e.shared.clone();
        async move { shared.retired.lock().unwrap().len() == 2 }
    })
    .await;
    assert_eq!(e.state(c).await, JobState::Queued);
    e.supervisor.shutdown().await;
}

#[tokio::test]
async fn an_idle_generation_is_retired_immediately() {
    let e = env(1).await;
    e.configure("news.one.test").await;
    e.supervisor.start().await.unwrap();
    e.configure("news.two.test").await;
    let shared = e.shared.clone();
    wait("first pool retired", || {
        let shared = shared.clone();
        async move { shared.retired.lock().unwrap().len() == 1 }
    })
    .await;
    assert_eq!(*e.shared.retired.lock().unwrap(), ["news.one.test"]);
    assert_eq!(e.supervisor.current_generation(), Some(2));
    e.supervisor.shutdown().await;
}

#[tokio::test]
async fn shutdown_stops_and_joins_every_running_job() {
    let e = env(3).await;
    e.configure("news.test").await;
    e.supervisor.start().await.unwrap();
    *e.shared.stop_delay.lock().unwrap() = Duration::from_millis(200);
    let ids = [e.job("a").await, e.job("b").await, e.job("c").await];
    e.wait_started(3).await;
    for id in ids {
        e.wait_state(id, JobState::Downloading).await;
    }
    e.supervisor.shutdown().await;
    // Joined: every run returned before shutdown did.
    for id in ids {
        assert_eq!(e.shared.finished_as(id), ["stopped"]);
        assert_eq!(e.state(id).await, JobState::Downloading);
    }
    assert_eq!(e.shared.running.load(Ordering::SeqCst), 0);
    assert!(e.supervisor.active().is_empty());
    assert_eq!(*e.shared.retired.lock().unwrap(), ["news.test"]);
    // Nothing runs after shutdown.
    let late = e.job("late").await;
    settle().await;
    assert_eq!(e.shared.starts_of(late), 0);
    e.supervisor.shutdown().await;
}

/// Drive a fresh job to `state` with the store's own transitions.
async fn job_in(jobs: &SqliteJobStore, title: &str, state: JobState) -> JobId {
    use JobState::*;
    let id = jobs.insert(Job::new(JobKind::Nzb, title)).await.unwrap().id;
    let path: &[JobState] = match state {
        Queued => &[],
        Downloading => &[Downloading],
        Paused => &[Downloading, Paused],
        Verifying => &[Downloading, Verifying],
        Repairing => &[Downloading, Verifying, Repairing],
        Extracting => &[Downloading, Verifying, Extracting],
        ReadyForImport => &[Downloading, Verifying, ReadyForImport],
        Importing => &[Downloading, Verifying, ReadyForImport, Importing],
        Completed => &[Downloading, Verifying, ReadyForImport, Importing, Completed],
        Cancelled => &[Cancelled],
        Failed => unreachable!(),
    };
    for &to in path {
        jobs.transition(id, to, None).await.unwrap();
    }
    id
}

#[tokio::test]
async fn restart_recovers_every_stage_before_dispatch() {
    use JobState::*;
    let dir = tempfile::tempdir().unwrap();
    let staging = dir.path().join("staging");
    let full = |name: &str| {
        let path = staging.join(name);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("part.bin"), b"x").unwrap();
        path
    };
    let empty = staging.join("empty");
    std::fs::create_dir_all(&empty).unwrap();

    let expected: Vec<(JobId, JobState, JobState)>;
    {
        let db = Database::connect(dir.path().join("reelarr.db"))
            .await
            .unwrap();
        let jobs = SqliteJobStore::new(&db);
        let paths = SqliteRecoveryPaths::new(jobs.clone());
        let mut cases = Vec::new();
        for (title, state, artifacts, after) in [
            ("queued", Queued, None, Queued),
            ("downloading", Downloading, Some(full("dl")), Queued),
            ("verifying", Verifying, Some(full("v")), Verifying),
            ("verifying-unregistered", Verifying, None, Queued),
            ("repairing", Repairing, Some(full("r")), Repairing),
            (
                "repairing-missing",
                Repairing,
                Some(staging.join("gone")),
                Queued,
            ),
            ("extracting", Extracting, Some(full("x")), Extracting),
            ("extracting-empty", Extracting, Some(empty.clone()), Queued),
            ("ready", ReadyForImport, Some(full("ready")), ReadyForImport),
            ("importing", Importing, Some(full("imp")), ReadyForImport),
            ("paused", Paused, None, Paused),
            ("completed", Completed, None, Completed),
            ("cancelled", Cancelled, None, Cancelled),
        ] {
            let id = job_in(&jobs, title, state).await;
            if let Some(path) = artifacts {
                paths.set_recovery_path(id, Some(&path)).await.unwrap();
            }
            cases.push((id, state, after));
        }
        expected = cases;
        db.pool().close().await;
    }

    // Reopen, as after a crash. Settings exist from the start, so anything
    // dispatched before recovery would be seen in its pre-crash state.
    let e = env_at(dir, 16).await;
    e.nntp
        .save(&NntpServerUpdate::new("news.test"))
        .await
        .unwrap();
    e.supervisor.start().await.unwrap();
    let runnable: Vec<_> = expected
        .iter()
        .filter(|(_, _, after)| matches!(after, Queued | Verifying | Repairing | Extracting))
        .collect();
    e.wait_started(runnable.len()).await;
    settle().await;
    let starts: HashMap<_, _> = e
        .shared
        .starts()
        .into_iter()
        .map(|s| (s.id, s.state))
        .collect();
    assert_eq!(starts.len(), runnable.len());
    for &(id, before, after) in &expected {
        match starts.get(&id) {
            Some(&seen) => assert_eq!(seen, after, "{before:?} job dispatched as {seen:?}"),
            None => {
                assert!(
                    !matches!(after, Queued | Verifying | Repairing | Extracting),
                    "{before:?} job was not dispatched"
                );
                assert_eq!(e.state(id).await, after, "{before:?} recovered wrongly");
            }
        }
    }
    let _ = e.db_path;
    e.supervisor.shutdown().await;
}
