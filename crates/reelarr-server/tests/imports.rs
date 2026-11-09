//! Import hookup (#76 PR 6): `ReadyForImport` jobs left over from before a
//! restart are imported without NNTP settings, source cleanup runs only after
//! `Completed`, the agreed retry schedule keeps retryable failures waiting in
//! `ReadyForImport`, and a cancel never discards files under a running import
//! (which is not in the download-worker registry).
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use reelarr_core::{
    CompletedDownload, DownloadSource, EventHub, FailureCode, ImportHandler, ImportResult, JobId,
    JobState, JobStore,
};
use reelarr_db::{
    Database, NntpServer, SqliteJobStore, SqliteNntpStore, SqliteNzbStore, SqliteRecoveryPaths,
    secrets::XChaChaSecretCipher,
};
use reelarr_nzb::worker::{RecoveryPaths, WorkerConfig};
use reelarr_server::downloads::{
    BuildError, Discarded, IMPORT_RETRY, JobRunner, NewDownload, RunnerFactory, Supervisor,
    SupervisorConfig,
};
use reelarr_types::{MediaFileId, MovieId};
use tokio::sync::Semaphore;

const NZB: &str = "<?xml version=\"1.0\"?>\n<nzb xmlns=\"http://www.newzbin.com/DTD/2003/nzb\">\
    <head><meta type=\"title\">Leftover.Movie.2020.1080p</meta></head>\
    <file poster=\"tester@example.test\" date=\"1700000000\" subject=\"&quot;a.mkv&quot; yEnc (1/1)\">\
    <groups><group>alt.binaries.test</group></groups>\
    <segments><segment bytes=\"10\" number=\"1\">a@test.example</segment></segments></file></nzb>";

/// Never built: these tests run with no NNTP provider configured.
struct NoFactory;
#[async_trait]
impl RunnerFactory for NoFactory {
    async fn build(&self, _: NntpServer) -> Result<Arc<dyn JobRunner>, BuildError> {
        panic!("imports must not need an NNTP provider");
    }
}

/// What the handler saw while an import ran.
#[derive(Clone, Debug)]
struct Seen {
    download: CompletedDownload,
    at: Instant,
    state: JobState,
    output_existed: bool,
    staging_existed: bool,
    publish_existed: bool,
}

/// Scripted handler: answers in order (the last answer repeats), optionally
/// waiting on a gate before checking the files a second time.
struct Script {
    jobs: Arc<SqliteJobStore>,
    staging: PathBuf,
    output: PathBuf,
    answers: Mutex<VecDeque<ImportResult>>,
    gate: Option<Arc<Semaphore>>,
    seen: Mutex<Vec<Seen>>,
    /// Files checked again after the gate opened.
    after_gate: Mutex<Vec<(bool, bool, bool)>>,
}

impl Script {
    fn files(&self, download: &CompletedDownload) -> (bool, bool, bool) {
        let id = download.job_id.to_string();
        (
            download.output_path.join("a.mkv").exists(),
            self.staging.join(&id).join("part.bin").exists(),
            self.output
                .join(format!(".reelarr-publish-{id}"))
                .join("partial.mkv")
                .exists(),
        )
    }
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl ImportHandler for Script {
    async fn import(&self, download: CompletedDownload) -> ImportResult {
        let state = self.jobs.get(download.job_id).await.unwrap().state;
        let (output_existed, staging_existed, publish_existed) = self.files(&download);
        self.seen.lock().unwrap().push(Seen {
            download: download.clone(),
            at: Instant::now(),
            state,
            output_existed,
            staging_existed,
            publish_existed,
        });
        if let Some(gate) = &self.gate {
            gate.acquire().await.unwrap().forget();
            let files = self.files(&download);
            self.after_gate.lock().unwrap().push(files);
        }
        let mut answers = self.answers.lock().unwrap();
        if answers.len() > 1 {
            answers.pop_front().unwrap()
        } else {
            answers.front().cloned().unwrap()
        }
    }
}

fn imported() -> ImportResult {
    ImportResult::Imported {
        media_file_id: MediaFileId(1),
    }
}

struct Env {
    _dir: tempfile::TempDir,
    staging: PathBuf,
    output: PathBuf,
    jobs: Arc<SqliteJobStore>,
    supervisor: Supervisor,
}

fn config(import_retry: Vec<Duration>) -> SupervisorConfig {
    SupervisorConfig {
        poll_interval: Duration::from_millis(20),
        import_retry,
        ..SupervisorConfig::default()
    }
}

async fn env(config: SupervisorConfig) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::connect(dir.path().join("reelarr.db"))
        .await
        .unwrap();
    let jobs = Arc::new(SqliteJobStore::with_options(
        &db,
        EventHub::default(),
        Duration::ZERO,
    ));
    let staging = dir.path().join("incomplete");
    let output = dir.path().join("complete");
    let supervisor = Supervisor::new(
        jobs.clone(),
        SqliteNzbStore::new(&db),
        SqliteNntpStore::new(&db, Arc::new(XChaChaSecretCipher::ephemeral().unwrap())),
        Arc::new(NoFactory),
        WorkerConfig::new(&staging, &output),
        config,
    );
    Env {
        _dir: dir,
        staging,
        output,
        jobs,
        supervisor,
    }
}

impl Env {
    /// A finished download waiting for import, as the NZB worker leaves it:
    /// published folder recorded as the recovery path, staging files, and an
    /// unfinished publication folder (what discard removes).
    async fn leftover(&self, movie: Option<MovieId>) -> (JobId, PathBuf) {
        let job = self
            .supervisor
            .enqueue_nzb(
                NZB.as_bytes().to_vec(),
                NewDownload {
                    fallback_title: "fallback".into(),
                    movie_id: movie,
                    ..NewDownload::default()
                },
            )
            .await
            .unwrap();
        let id = job.id;
        for to in [
            JobState::Downloading,
            JobState::Verifying,
            JobState::ReadyForImport,
        ] {
            self.jobs.transition(id, to, None).await.unwrap();
        }
        let published = self.output.join("Leftover.Movie.2020.1080p");
        std::fs::create_dir_all(&published).unwrap();
        std::fs::write(published.join("a.mkv"), b"movie").unwrap();
        let staging = self.staging.join(id.to_string());
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("part.bin"), b"x").unwrap();
        let publish = self.output.join(format!(".reelarr-publish-{id}"));
        std::fs::create_dir_all(&publish).unwrap();
        std::fs::write(publish.join("partial.mkv"), b"x").unwrap();
        SqliteRecoveryPaths::new((*self.jobs).clone())
            .set_recovery_path(id, Some(&published))
            .await
            .unwrap();
        (id, published)
    }

    fn script(&self, answers: Vec<ImportResult>, gate: Option<Arc<Semaphore>>) -> Arc<Script> {
        let script = Arc::new(Script {
            jobs: self.jobs.clone(),
            staging: self.staging.clone(),
            output: self.output.clone(),
            answers: Mutex::new(answers.into()),
            gate,
            seen: Mutex::default(),
            after_gate: Mutex::default(),
        });
        self.supervisor.set_import_handler(script.clone());
        script
    }

    async fn wait_state(&self, id: JobId, state: JobState) {
        let jobs = self.jobs.clone();
        wait(&format!("{id} reaches {state:?}"), || {
            let jobs = jobs.clone();
            async move { jobs.get(id).await.unwrap().state == state }
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

fn exists(path: &Path) -> bool {
    path.exists()
}

#[test]
fn the_default_retry_schedule_is_1_5_15_then_every_30_minutes() {
    let minutes = |m: u64| Duration::from_secs(m * 60);
    assert_eq!(
        IMPORT_RETRY,
        [minutes(1), minutes(5), minutes(15), minutes(30)]
    );
    let config = SupervisorConfig::default();
    assert_eq!(config.import_retry, IMPORT_RETRY.to_vec());
    let delays: Vec<_> = (1..=7).map(|n| config.import_retry_delay(n)).collect();
    assert_eq!(
        delays,
        [
            minutes(1),
            minutes(5),
            minutes(15),
            minutes(30),
            minutes(30),
            minutes(30),
            minutes(30)
        ]
    );
}

#[tokio::test]
async fn leftover_ready_for_import_jobs_import_on_startup_without_nntp() {
    let e = env(config(IMPORT_RETRY.to_vec())).await;
    let (id, published) = e.leftover(Some(MovieId(42))).await;
    let script = e.script(vec![imported()], None);
    // No NNTP provider is configured; the factory would panic if used.
    e.supervisor.start().await.unwrap();
    e.wait_state(id, JobState::Completed).await;

    let seen = script.seen();
    assert_eq!(seen.len(), 1);
    let download = &seen[0].download;
    assert_eq!(download.job_id, id);
    assert_eq!(download.movie_id, Some(MovieId(42)));
    assert_eq!(download.output_path, published);
    assert_eq!(download.source, DownloadSource::BuiltinNzb);
    assert_eq!(download.release_title, "Leftover.Movie.2020.1080p");
    assert_eq!(seen[0].state, JobState::Importing);
    assert!(e.supervisor.active().is_empty(), "no download ran");
    assert_eq!(e.supervisor.current_generation(), None);
    e.supervisor.shutdown().await;
}

#[tokio::test]
async fn an_interrupted_import_is_recovered_and_imported_on_startup() {
    let e = env(config(IMPORT_RETRY.to_vec())).await;
    let (id, _) = e.leftover(Some(MovieId(1))).await;
    // The server stopped mid-import.
    e.jobs
        .transition(id, JobState::Importing, None)
        .await
        .unwrap();
    let script = e.script(vec![imported()], None);
    e.supervisor.start().await.unwrap();
    e.wait_state(id, JobState::Completed).await;
    assert_eq!(script.seen().len(), 1);
    e.supervisor.shutdown().await;
}

#[tokio::test]
async fn source_cleanup_runs_after_completed_and_not_before() {
    let e = env(config(IMPORT_RETRY.to_vec())).await;
    let (id, published) = e.leftover(Some(MovieId(1))).await;
    let staging = e.staging.join(id.to_string());
    let script = e.script(vec![imported()], None);
    e.supervisor.start().await.unwrap();
    e.wait_state(id, JobState::Completed).await;
    let seen = script.seen();
    // While the import ran, nothing had been cleaned up yet.
    assert_eq!(seen[0].state, JobState::Importing);
    assert!(seen[0].output_existed);
    assert!(seen[0].staging_existed);
    // After Completed, the built-in NZB cleanup removed staging and the
    // published folder.
    wait("cleanup after Completed", || {
        let gone = !exists(&published) && !exists(&staging);
        async move { gone }
    })
    .await;
    e.supervisor.shutdown().await;
}

#[tokio::test]
async fn terminal_failures_and_no_match_fail_with_their_code_and_keep_files() {
    for (answer, code, retryable) in [
        (
            ImportResult::Failed {
                reason: "a different file already exists".into(),
                retryable: false,
                code: Some(FailureCode::DestinationConflict),
            },
            FailureCode::DestinationConflict,
            false,
        ),
        (
            ImportResult::Failed {
                reason: "something else".into(),
                retryable: false,
                code: None,
            },
            FailureCode::Other,
            false,
        ),
        (ImportResult::NoMatch, FailureCode::NoMatch, false),
    ] {
        let e = env(config(vec![Duration::from_millis(10)])).await;
        let (id, published) = e.leftover(Some(MovieId(1))).await;
        let script = e.script(vec![answer.clone()], None);
        e.supervisor.start().await.unwrap();
        e.wait_state(id, JobState::Failed).await;
        let job = e.jobs.get(id).await.unwrap();
        let failure = job.failure.unwrap();
        assert_eq!(failure.code, code, "{answer:?}");
        assert_eq!(failure.retryable, retryable);
        // Failed is final for the scheduler: no further attempts.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(script.seen().len(), 1, "{answer:?} was retried");
        // No cleanup after a failure.
        assert!(exists(&published.join("a.mkv")));
        assert!(exists(&e.staging.join(id.to_string()).join("part.bin")));
        e.supervisor.shutdown().await;
    }
}

#[tokio::test]
async fn a_retryable_failure_waits_in_ready_for_import_on_the_retry_schedule() {
    let steps = vec![
        Duration::from_millis(150),
        Duration::from_millis(300),
        Duration::from_millis(100),
    ];
    let e = env(config(steps.clone())).await;
    let (id, published) = e.leftover(Some(MovieId(1))).await;
    let offline = ImportResult::Failed {
        reason: "library folder is unavailable".into(),
        retryable: true,
        code: Some(FailureCode::RootOffline),
    };
    let script = e.script(
        vec![offline.clone(), offline.clone(), offline, imported()],
        None,
    );
    let mut events = e.jobs.events().subscribe();
    e.supervisor.start().await.unwrap();

    // After the first failure the job is back in ReadyForImport, with no
    // failure recorded, and is not retried before its wait is over.
    wait("first attempt", || {
        let n = script.seen().len();
        async move { n >= 1 }
    })
    .await;
    wait("back in ReadyForImport", || {
        let jobs = e.jobs.clone();
        let importing = e.supervisor.importing();
        async move {
            importing.is_empty() && jobs.get(id).await.unwrap().state == JobState::ReadyForImport
        }
    })
    .await;
    let job = e.jobs.get(id).await.unwrap();
    assert!(job.failure.is_none());
    assert!(exists(&published), "no cleanup before Completed");

    e.wait_state(id, JobState::Completed).await;
    let seen = script.seen();
    assert_eq!(seen.len(), 4);
    // Waits follow the schedule; the last entry repeats.
    for (n, expected) in [steps[0], steps[1], steps[2]].into_iter().enumerate() {
        let gap = seen[n + 1].at - seen[n].at;
        assert!(
            gap >= expected,
            "retry {} after {gap:?} < {expected:?}",
            n + 1
        );
    }
    // The reason reached the job's event stream.
    let mut retried = 0;
    while let Ok(event) = events.try_recv() {
        if let reelarr_core::JobEventKind::Message { message } = event.kind
            && message.contains("Import will be retried")
            && message.contains("library folder is unavailable")
        {
            retried += 1;
        }
    }
    assert_eq!(retried, 3);
    e.supervisor.shutdown().await;
}

#[tokio::test]
async fn a_download_without_a_recorded_folder_fails_instead_of_waiting_forever() {
    let e = env(config(IMPORT_RETRY.to_vec())).await;
    let (id, _) = e.leftover(Some(MovieId(1))).await;
    SqliteRecoveryPaths::new((*e.jobs).clone())
        .set_recovery_path(id, None)
        .await
        .unwrap();
    let script = e.script(vec![imported()], None);
    e.supervisor.start().await.unwrap();
    e.wait_state(id, JobState::Failed).await;
    assert!(script.seen().is_empty());
    assert_eq!(
        e.jobs.get(id).await.unwrap().failure.unwrap().code,
        FailureCode::Other
    );
    e.supervisor.shutdown().await;
}

/// The PM's critical case: a leftover import is not in the download-worker
/// registry, yet a cancel + discard while it runs must wait for it.
#[tokio::test]
async fn discard_waits_for_a_leftover_import_outside_the_download_registry() {
    let e = env(config(IMPORT_RETRY.to_vec())).await;
    let (id, _) = e.leftover(Some(MovieId(1))).await;
    let staging = e.staging.join(id.to_string());
    let publish = e.output.join(format!(".reelarr-publish-{id}"));
    let gate = Arc::new(Semaphore::new(0));
    let script = e.script(vec![imported()], Some(gate.clone()));
    e.supervisor.start().await.unwrap();
    wait("import running", || {
        let n = script.seen().len();
        async move { n == 1 }
    })
    .await;
    assert!(e.supervisor.active().is_empty(), "not a download run");
    assert_eq!(e.supervisor.importing(), [id]);

    let cancelled = e.supervisor.cancel(id).await.unwrap();
    assert_eq!(cancelled.state, JobState::Cancelled);
    assert_eq!(
        e.supervisor.discard(id).await.unwrap(),
        Discarded::WhenStopped
    );
    // Repeating it while the import runs changes nothing.
    assert_eq!(
        e.supervisor.discard(id).await.unwrap(),
        Discarded::WhenStopped
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(exists(&staging.join("part.bin")));
    assert!(exists(&publish.join("partial.mkv")));

    // The import finishes reading with every file still in place.
    gate.add_permits(1);
    wait("deferred discard", || {
        let gone = !exists(&staging) && !exists(&publish);
        async move { gone }
    })
    .await;
    assert_eq!(*script.after_gate.lock().unwrap(), [(true, true, true)]);
    let seen = script.seen();
    assert!(seen[0].staging_existed && seen[0].publish_existed);
    // The cancel wins: the job is not completed and no cleanup ran for it.
    assert_eq!(e.jobs.get(id).await.unwrap().state, JobState::Cancelled);
    assert!(e.supervisor.importing().is_empty());
    e.supervisor.shutdown().await;
}

#[tokio::test]
async fn discard_of_a_cancelled_job_with_no_import_running_is_immediate() {
    let e = env(config(IMPORT_RETRY.to_vec())).await;
    let (id, _) = e.leftover(Some(MovieId(1))).await;
    // No handler: nothing imports, so the leftover job just waits.
    e.supervisor.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        e.jobs.get(id).await.unwrap().state,
        JobState::ReadyForImport
    );
    e.supervisor.cancel(id).await.unwrap();
    assert_eq!(e.supervisor.discard(id).await.unwrap(), Discarded::Now);
    assert!(!exists(&e.staging.join(id.to_string())));
    e.supervisor.shutdown().await;
}
