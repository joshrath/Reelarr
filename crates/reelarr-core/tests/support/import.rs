use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use super::job_store::StoreFactory;
use async_trait::async_trait;
use reelarr_core::*;
use reelarr_types::{MediaFileId, MovieId};
use std::time::Duration;
use tokio::sync::Mutex;

struct ImportProbe {
    store: Arc<dyn JobStore>,
    result: ImportResult,
    requests: Mutex<Vec<CompletedDownload>>,
}

impl ImportProbe {
    fn new(store: Arc<dyn JobStore>, result: ImportResult) -> Self {
        Self {
            store,
            result,
            requests: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl ImportHandler for ImportProbe {
    async fn import(&self, download: CompletedDownload) -> ImportResult {
        assert_eq!(
            self.store.get(download.job_id).await.unwrap().state,
            JobState::Importing
        );
        assert!(download.output_path.is_absolute());
        self.requests.lock().await.push(download);
        // Give a simultaneous claim a chance to encounter Importing.
        tokio::task::yield_now().await;
        self.result.clone()
    }
}

struct CleanupProbe {
    store: Arc<dyn JobStore>,
    calls: AtomicUsize,
    fail: bool,
    remove_directory: bool,
}

impl CleanupProbe {
    fn new(store: Arc<dyn JobStore>, fail: bool, remove_directory: bool) -> Self {
        Self {
            store,
            calls: AtomicUsize::new(0),
            fail,
            remove_directory,
        }
    }
}

#[async_trait]
impl SourceCleanup for CleanupProbe {
    async fn on_imported(&self, id: JobId, path: &Path) -> Result<(), CleanupError> {
        assert_eq!(self.store.get(id).await.unwrap().state, JobState::Completed);
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Err(CleanupError(Box::new(std::io::Error::other(
                "induced cleanup failure",
            ))));
        }
        if self.remove_directory {
            match tokio::fs::remove_dir_all(path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(CleanupError(Box::new(error))),
            }
        }
        Ok(())
    }
}

async fn ready(
    store: &dyn JobStore,
    source: DownloadSource,
    output_path: PathBuf,
) -> CompletedDownload {
    let job = store
        .insert(Job::new(JobKind::Nzb, "Example.2026.1080p"))
        .await
        .unwrap();
    for state in [
        JobState::Downloading,
        JobState::Verifying,
        JobState::Repairing,
        JobState::Extracting,
        JobState::ReadyForImport,
    ] {
        store.transition(job.id, state, None).await.unwrap();
    }
    assert_eq!(
        store.get(job.id).await.unwrap().resume_hint,
        Some(CompletedStage::Extracting)
    );
    CompletedDownload {
        job_id: job.id,
        movie_id: Some(MovieId(42)),
        output_path,
        category: Some("movies".into()),
        source,
        release_title: job.title,
    }
}

fn imported() -> ImportResult {
    ImportResult::Imported {
        media_file_id: MediaFileId(7),
    }
}

async fn imported_maps_to_completed_and_cleans_once_after_completion(factory: &dyn StoreFactory) {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    tokio::fs::create_dir_all(output.join("already-empty-subfolder"))
        .await
        .unwrap();
    tokio::fs::write(output.join("leftover.txt"), "partial folder")
        .await
        .unwrap();
    let store = factory
        .create(EventHub::default(), Duration::from_millis(250))
        .await;
    let download = ready(&*store, DownloadSource::BuiltinNzb, output.clone()).await;
    let handler = ImportProbe::new(store.clone(), imported());
    let cleanup = Arc::new(CleanupProbe::new(store.clone(), false, true));
    let mut cleanups = CleanupRegistry::default();
    cleanups.register(DownloadSource::BuiltinNzb, cleanup.clone());
    let completed = run_import(
        &*store,
        &handler,
        &cleanups,
        download.clone(),
        PathMapping::Local,
    )
    .await
    .unwrap();
    assert_eq!(completed.state, JobState::Completed);
    assert!(completed.failure.is_none());
    assert_eq!(
        handler.requests.lock().await.as_slice(),
        std::slice::from_ref(&download)
    );
    assert_eq!(cleanup.calls.load(Ordering::SeqCst), 1);
    assert!(!output.exists());
    assert_eq!(
        run_import(&*store, &handler, &cleanups, download, PathMapping::Local)
            .await
            .unwrap()
            .state,
        JobState::Completed
    );
    assert_eq!(handler.requests.lock().await.len(), 1);
    assert_eq!(cleanup.calls.load(Ordering::SeqCst), 1);
    // Source implementations themselves must tolerate replay/missing/partly empty dirs.
    cleanup.on_imported(completed.id, &output).await.unwrap();
    tokio::fs::create_dir_all(output.join("empty"))
        .await
        .unwrap();
    cleanup.on_imported(completed.id, &output).await.unwrap();
    assert!(!output.exists());
}

async fn generic_import_failure_and_no_match_have_distinct_outcomes(factory: &dyn StoreFactory) {
    for result in [
        ImportResult::Failed {
            reason: "collision".into(),
            retryable: true,
            code: None,
        },
        ImportResult::Failed {
            reason: "library share is offline".into(),
            retryable: true,
            code: Some(FailureCode::RootOffline),
        },
        ImportResult::NoMatch,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = factory
            .create(EventHub::default(), Duration::from_millis(250))
            .await;
        let mut download = ready(
            &*store,
            DownloadSource::External {
                client: "manual".into(),
            },
            dir.path().to_owned(),
        )
        .await;
        download.movie_id = None; // only external/manual jobs use parser fallback
        let handler = ImportProbe::new(store.clone(), result.clone());
        let cleanup = Arc::new(CleanupProbe::new(store.clone(), false, false));
        let mut cleanups = CleanupRegistry::default();
        cleanups.register(download.source.clone(), cleanup.clone());
        let job = run_import(
            &*store,
            &handler,
            &cleanups,
            download.clone(),
            PathMapping::Local,
        )
        .await
        .unwrap();
        assert_eq!(job.state, JobState::Failed);
        let failure = job.failure.unwrap();
        match result {
            ImportResult::Failed {
                reason,
                retryable,
                code,
            } => {
                assert_eq!(failure.code, code.unwrap_or(FailureCode::Other));
                assert_eq!(failure.message, reason);
                assert_eq!(failure.retryable, retryable);
                assert!(
                    store
                        .transition(job.id, JobState::ReadyForImport, None)
                        .await
                        .is_err()
                );
            }
            ImportResult::NoMatch => {
                assert_eq!(failure.code, FailureCode::NoMatch);
                assert!(!failure.retryable);
                let retried = store
                    .transition(job.id, JobState::ReadyForImport, None)
                    .await
                    .unwrap();
                assert!(retried.failure.is_none());
                let matched = ImportProbe::new(store.clone(), imported());
                assert_eq!(
                    run_import(&*store, &matched, &cleanups, download, PathMapping::Local)
                        .await
                        .unwrap()
                        .state,
                    JobState::Completed
                );
            }
            _ => unreachable!(),
        }
        if failure.code != FailureCode::NoMatch {
            assert_eq!(cleanup.calls.load(Ordering::SeqCst), 0);
        }
        assert!(dir.path().exists());
    }
}

async fn unmapped_or_relative_paths_fail_before_import_or_cleanup(factory: &dyn StoreFactory) {
    for (path, mapping) in [
        (PathBuf::from("/remote/output"), PathMapping::Unmapped),
        (PathBuf::from("relative/output"), PathMapping::Local),
    ] {
        let store = factory
            .create(EventHub::default(), Duration::from_millis(250))
            .await;
        let download = ready(&*store, DownloadSource::BuiltinNzb, path).await;
        let handler = ImportProbe::new(store.clone(), imported());
        let cleanup = Arc::new(CleanupProbe::new(store.clone(), false, false));
        let mut cleanups = CleanupRegistry::default();
        cleanups.register(download.source.clone(), cleanup.clone());
        let failed = run_import(&*store, &handler, &cleanups, download, mapping)
            .await
            .unwrap();
        assert_eq!(failed.state, JobState::Failed);
        assert_eq!(failed.failure.unwrap().code, FailureCode::PathUnmapped);
        assert!(handler.requests.lock().await.is_empty());
        assert_eq!(cleanup.calls.load(Ordering::SeqCst), 0);
    }
}

async fn builtin_jobs_require_the_original_movie_id(factory: &dyn StoreFactory) {
    let store = factory
        .create(EventHub::default(), Duration::from_millis(250))
        .await;
    let mut download = ready(
        &*store,
        DownloadSource::BuiltinNzb,
        PathBuf::from("/downloads/mapped"),
    )
    .await;
    download.movie_id = None;
    let handler = ImportProbe::new(store.clone(), imported());
    let failed = run_import(
        &*store,
        &handler,
        &CleanupRegistry::default(),
        download,
        PathMapping::Local,
    )
    .await
    .unwrap();
    assert_eq!(failed.state, JobState::Failed);
    assert_eq!(failed.failure.unwrap().code, FailureCode::Other);
    assert!(handler.requests.lock().await.is_empty());
}

async fn cleanup_errors_are_messages_not_import_failures_and_are_not_retried(
    factory: &dyn StoreFactory,
) {
    let store = factory
        .create(EventHub::default(), Duration::from_millis(250))
        .await;
    let download = ready(
        &*store,
        DownloadSource::BuiltinNzb,
        PathBuf::from("/downloads/mapped"),
    )
    .await;
    let handler = ImportProbe::new(store.clone(), imported());
    let cleanup = Arc::new(CleanupProbe::new(store.clone(), true, false));
    let mut cleanups = CleanupRegistry::default();
    cleanups.register(download.source.clone(), cleanup.clone());
    let mut receiver = store.events().subscribe();
    let completed = run_import(
        &*store,
        &handler,
        &cleanups,
        download.clone(),
        PathMapping::Local,
    )
    .await
    .unwrap();
    assert_eq!(completed.state, JobState::Completed);
    assert!(completed.failure.is_none());
    let mut messages = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        if let JobEventKind::Message { message } = event.kind {
            messages.push(message);
        }
    }
    assert_eq!(messages.len(), 1);
    assert!(messages[0].contains("induced cleanup failure"));
    run_import(&*store, &handler, &cleanups, download, PathMapping::Local)
        .await
        .unwrap();
    assert_eq!(cleanup.calls.load(Ordering::SeqCst), 1);
}

async fn registry_selects_the_matching_source_and_concurrent_handoffs_import_once(
    factory: &dyn StoreFactory,
) {
    let store = factory
        .create(EventHub::default(), Duration::from_millis(250))
        .await;
    let source = DownloadSource::External {
        client: "example-client".into(),
    };
    let download = ready(&*store, source.clone(), PathBuf::from("/downloads/mapped")).await;
    let handler = ImportProbe::new(store.clone(), imported());
    let right = Arc::new(CleanupProbe::new(store.clone(), false, false));
    let wrong = Arc::new(CleanupProbe::new(store.clone(), true, false));
    let mut cleanups = CleanupRegistry::default();
    cleanups.register(source, right.clone());
    cleanups.register(DownloadSource::BuiltinNzb, wrong.clone());
    let (one, two) = tokio::join!(
        run_import(
            &*store,
            &handler,
            &cleanups,
            download.clone(),
            PathMapping::Local
        ),
        run_import(&*store, &handler, &cleanups, download, PathMapping::Local),
    );
    assert!(one.is_ok() || two.is_ok());
    assert_eq!(store.list().await.unwrap()[0].state, JobState::Completed);
    assert_eq!(handler.requests.lock().await.len(), 1);
    assert_eq!(right.calls.load(Ordering::SeqCst), 1);
    assert_eq!(wrong.calls.load(Ordering::SeqCst), 0);
}

async fn missing_cleanup_is_also_best_effort_and_emits_a_message(factory: &dyn StoreFactory) {
    let store = factory
        .create(EventHub::default(), Duration::from_millis(250))
        .await;
    let download = ready(
        &*store,
        DownloadSource::BuiltinNzb,
        PathBuf::from("/downloads/mapped"),
    )
    .await;
    let handler = ImportProbe::new(store.clone(), imported());
    let mut receiver = store.events().subscribe();
    let job = run_import(
        &*store,
        &handler,
        &CleanupRegistry::default(),
        download,
        PathMapping::Local,
    )
    .await
    .unwrap();
    assert_eq!(job.state, JobState::Completed);
    let mut found = false;
    while let Ok(event) = receiver.try_recv() {
        if let JobEventKind::Message { message } = event.kind {
            found |= message.contains("no source cleanup registered");
        }
    }
    assert!(found);
}

async fn attempt_import_keeps_retryable_failures_ready_and_finishes_the_rest(
    factory: &dyn StoreFactory,
) {
    let retryable = ImportResult::Failed {
        reason: "library share is offline".into(),
        retryable: true,
        code: Some(FailureCode::RootOffline),
    };
    let terminal = ImportResult::Failed {
        reason: "a different file exists".into(),
        retryable: false,
        code: Some(FailureCode::DestinationConflict),
    };
    for result in [retryable, terminal, ImportResult::NoMatch, imported()] {
        let dir = tempfile::tempdir().unwrap();
        let store = factory
            .create(EventHub::default(), Duration::from_millis(250))
            .await;
        let download = ready(&*store, DownloadSource::BuiltinNzb, dir.path().to_owned()).await;
        let handler = ImportProbe::new(store.clone(), result.clone());
        let cleanup = Arc::new(CleanupProbe::new(store.clone(), false, false));
        let mut cleanups = CleanupRegistry::default();
        cleanups.register(download.source.clone(), cleanup.clone());
        let attempt = attempt_import(
            &*store,
            &handler,
            &cleanups,
            download.clone(),
            PathMapping::Local,
        )
        .await
        .unwrap();
        match (&result, attempt) {
            (
                ImportResult::Failed {
                    retryable: true, ..
                },
                ImportAttempt::Retry { job, failure },
            ) => {
                assert_eq!(job.state, JobState::ReadyForImport);
                assert!(job.failure.is_none());
                assert_eq!(failure.code, FailureCode::RootOffline);
                assert!(failure.retryable);
                assert_eq!(cleanup.calls.load(Ordering::SeqCst), 0);
                // The next attempt claims it again.
                let again = ImportProbe::new(store.clone(), imported());
                let done = attempt_import(&*store, &again, &cleanups, download, PathMapping::Local)
                    .await
                    .unwrap();
                assert!(
                    matches!(done, ImportAttempt::Finished(ref j) if j.state == JobState::Completed)
                );
                assert_eq!(cleanup.calls.load(Ordering::SeqCst), 1);
            }
            (ImportResult::Failed { code, .. }, ImportAttempt::Finished(job)) => {
                assert_eq!(job.state, JobState::Failed);
                assert_eq!(job.failure.unwrap().code, code.unwrap());
                assert_eq!(cleanup.calls.load(Ordering::SeqCst), 0);
            }
            (ImportResult::NoMatch, ImportAttempt::Finished(job)) => {
                assert_eq!(job.state, JobState::Failed);
                assert_eq!(job.failure.unwrap().code, FailureCode::NoMatch);
                assert_eq!(cleanup.calls.load(Ordering::SeqCst), 0);
            }
            (ImportResult::Imported { .. }, ImportAttempt::Finished(job)) => {
                assert_eq!(job.state, JobState::Completed);
                assert_eq!(cleanup.calls.load(Ordering::SeqCst), 1);
            }
            (result, attempt) => panic!("{result:?} gave {attempt:?}"),
        }
    }
}

pub async fn run(factory: &dyn StoreFactory) {
    imported_maps_to_completed_and_cleans_once_after_completion(factory).await;
    generic_import_failure_and_no_match_have_distinct_outcomes(factory).await;
    unmapped_or_relative_paths_fail_before_import_or_cleanup(factory).await;
    builtin_jobs_require_the_original_movie_id(factory).await;
    cleanup_errors_are_messages_not_import_failures_and_are_not_retried(factory).await;
    registry_selects_the_matching_source_and_concurrent_handoffs_import_once(factory).await;
    missing_cleanup_is_also_best_effort_and_emits_a_message(factory).await;
    attempt_import_keeps_retryable_failures_ready_and_finishes_the_rest(factory).await;
    a_cancel_during_a_successful_import_completes_and_cleans_up(factory).await;
    a_cancel_during_a_failed_import_stays_cancelled_without_cleanup(factory).await;
    only_a_cancelled_job_can_be_completed_by_its_import(factory).await;
    a_store_error_after_the_claim_does_not_leave_the_job_importing(factory).await;
}

/// Cancels the job while "importing", then answers `result`.
struct CancelDuring {
    store: Arc<dyn JobStore>,
    result: ImportResult,
}

#[async_trait]
impl ImportHandler for CancelDuring {
    async fn import(&self, download: CompletedDownload) -> ImportResult {
        let cancelled = self
            .store
            .transition(download.job_id, JobState::Cancelled, None)
            .await
            .unwrap();
        assert_eq!(cancelled.state, JobState::Cancelled);
        self.result.clone()
    }
}

/// #104: the movie is already in the library, so the import wins over a
/// cancel that landed while it ran.
async fn a_cancel_during_a_successful_import_completes_and_cleans_up(factory: &dyn StoreFactory) {
    for retry_in_place in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("output");
        tokio::fs::create_dir_all(&output).await.unwrap();
        let store = factory
            .create(EventHub::default(), Duration::from_millis(250))
            .await;
        let download = ready(&*store, DownloadSource::BuiltinNzb, output.clone()).await;
        let handler = CancelDuring {
            store: store.clone(),
            result: imported(),
        };
        let cleanup = Arc::new(CleanupProbe::new(store.clone(), false, true));
        let mut cleanups = CleanupRegistry::default();
        cleanups.register(DownloadSource::BuiltinNzb, cleanup.clone());
        let mut receiver = store.events().subscribe();
        let job = if retry_in_place {
            match attempt_import(&*store, &handler, &cleanups, download, PathMapping::Local)
                .await
                .unwrap()
            {
                ImportAttempt::Finished(job) => job,
                other => panic!("{other:?}"),
            }
        } else {
            run_import(&*store, &handler, &cleanups, download, PathMapping::Local)
                .await
                .unwrap()
        };
        assert_eq!(job.state, JobState::Completed);
        assert!(job.failure.is_none());
        assert_eq!(store.get(job.id).await.unwrap().state, JobState::Completed);
        assert_eq!(cleanup.calls.load(Ordering::SeqCst), 1);
        assert!(!output.exists(), "normal cleanup ran");
        let mut states = Vec::new();
        let mut kept = false;
        while let Ok(event) = receiver.try_recv() {
            match event.kind {
                JobEventKind::StateChanged { from, to } => states.push((from, to)),
                JobEventKind::Message { message } => kept |= message.contains("movie was kept"),
                _ => {}
            }
        }
        assert_eq!(
            states,
            [
                (JobState::ReadyForImport, JobState::Importing),
                (JobState::Importing, JobState::Cancelled),
                (JobState::Cancelled, JobState::Completed),
            ]
        );
        assert!(kept);
    }
}

/// #104: a failed import keeps today's behavior: the cancel stands.
async fn a_cancel_during_a_failed_import_stays_cancelled_without_cleanup(
    factory: &dyn StoreFactory,
) {
    for result in [
        ImportResult::Failed {
            reason: "library share is offline".into(),
            retryable: true,
            code: Some(FailureCode::RootOffline),
        },
        ImportResult::Failed {
            reason: "a different file already exists".into(),
            retryable: false,
            code: Some(FailureCode::DestinationConflict),
        },
        ImportResult::NoMatch,
    ] {
        for retry_in_place in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let store = factory
                .create(EventHub::default(), Duration::from_millis(250))
                .await;
            let download = ready(&*store, DownloadSource::BuiltinNzb, dir.path().to_owned()).await;
            let id = download.job_id;
            let handler = CancelDuring {
                store: store.clone(),
                result: result.clone(),
            };
            let cleanup = Arc::new(CleanupProbe::new(store.clone(), false, true));
            let mut cleanups = CleanupRegistry::default();
            cleanups.register(DownloadSource::BuiltinNzb, cleanup.clone());
            let error = if retry_in_place {
                attempt_import(&*store, &handler, &cleanups, download, PathMapping::Local)
                    .await
                    .unwrap_err()
            } else {
                run_import(&*store, &handler, &cleanups, download, PathMapping::Local)
                    .await
                    .unwrap_err()
            };
            assert!(
                matches!(error, JobStoreError::Transition(_)),
                "{result:?}: {error}"
            );
            assert_eq!(store.get(id).await.unwrap().state, JobState::Cancelled);
            assert_eq!(cleanup.calls.load(Ordering::SeqCst), 0);
            assert!(dir.path().exists());
        }
    }
}

async fn only_a_cancelled_job_can_be_completed_by_its_import(factory: &dyn StoreFactory) {
    let store = factory
        .create(EventHub::default(), Duration::from_millis(250))
        .await;
    let download = ready(
        &*store,
        DownloadSource::BuiltinNzb,
        PathBuf::from("/downloads/mapped"),
    )
    .await;
    let id = download.job_id;
    // ReadyForImport (and Importing) are not reconciled.
    for state in [JobState::ReadyForImport, JobState::Importing] {
        if state == JobState::Importing {
            store.transition(id, state, None).await.unwrap();
        }
        let before = store.get(id).await.unwrap();
        assert!(matches!(
            store.complete_cancelled_import(id).await,
            Err(JobStoreError::Transition(TransitionError::Illegal { .. }))
        ));
        assert_eq!(store.get(id).await.unwrap(), before);
    }
    store
        .transition(id, JobState::Cancelled, None)
        .await
        .unwrap();
    let completed = store.complete_cancelled_import(id).await.unwrap();
    assert_eq!(completed.state, JobState::Completed);
    assert_eq!(store.get(id).await.unwrap(), completed);
    // Completed is terminal: no second reconcile, no cancel.
    assert!(store.complete_cancelled_import(id).await.is_err());
    assert!(
        store
            .transition(id, JobState::Cancelled, None)
            .await
            .is_err()
    );
    assert!(matches!(
        store.complete_cancelled_import(JobId::new()).await,
        Err(JobStoreError::NotFound(_))
    ));
}

/// Wraps a store and fails the listed transitions (each entry once) with a
/// backend error, as a database outage would.
struct FlakyStore {
    inner: Arc<dyn JobStore>,
    fail: std::sync::Mutex<Vec<JobState>>,
}

impl FlakyStore {
    fn new(inner: Arc<dyn JobStore>, fail: &[JobState]) -> Self {
        Self {
            inner,
            fail: std::sync::Mutex::new(fail.to_vec()),
        }
    }
}

#[async_trait]
impl JobStore for FlakyStore {
    fn events(&self) -> EventHub {
        self.inner.events()
    }
    async fn insert(&self, job: Job) -> Result<Job, JobStoreError> {
        self.inner.insert(job).await
    }
    async fn get(&self, id: JobId) -> Result<Job, JobStoreError> {
        self.inner.get(id).await
    }
    async fn list(&self) -> Result<Vec<Job>, JobStoreError> {
        self.inner.list().await
    }
    async fn transition(
        &self,
        id: JobId,
        to: JobState,
        failure: Option<Failure>,
    ) -> Result<Job, JobStoreError> {
        let injected = {
            let mut fail = self.fail.lock().unwrap();
            fail.iter()
                .position(|s| *s == to)
                .map(|i| fail.remove(i))
                .is_some()
        };
        if injected {
            return Err(JobStoreError::Backend(Box::new(std::io::Error::other(
                "injected store failure",
            ))));
        }
        self.inner.transition(id, to, failure).await
    }
    async fn complete_cancelled_import(&self, id: JobId) -> Result<Job, JobStoreError> {
        self.inner.complete_cancelled_import(id).await
    }
    async fn update_progress(&self, id: JobId, progress: Progress) -> Result<Job, JobStoreError> {
        self.inner.update_progress(id, progress).await
    }
    async fn set_queue_order(
        &self,
        id: JobId,
        priority: Priority,
        queue_position: u64,
    ) -> Result<Job, JobStoreError> {
        self.inner
            .set_queue_order(id, priority, queue_position)
            .await
    }
    async fn message(&self, id: JobId, message: String) -> Result<Job, JobStoreError> {
        self.inner.message(id, message).await
    }
    async fn recover(&self, id: JobId, files_exist: bool) -> Result<Job, JobStoreError> {
        self.inner.recover(id, files_exist).await
    }
    async fn resume(&self, id: JobId) -> Result<Job, JobStoreError> {
        self.inner.resume(id).await
    }
}

/// #105: a job-store write that fails after the claim puts the job back in
/// ReadyForImport (a retry for the scheduler), or Failed when that write
/// fails too; it never stays Importing unless every write fails.
async fn a_store_error_after_the_claim_does_not_leave_the_job_importing(
    factory: &dyn StoreFactory,
) {
    use JobState::*;
    let offline = ImportResult::Failed {
        reason: "library share is offline".into(),
        retryable: true,
        code: Some(FailureCode::RootOffline),
    };
    let terminal = ImportResult::Failed {
        reason: "a different file already exists".into(),
        retryable: false,
        code: Some(FailureCode::DestinationConflict),
    };
    // (handler answer, failing writes, state after attempt_import)
    let cases = [
        (imported(), vec![Completed], ReadyForImport),
        (offline.clone(), vec![ReadyForImport], ReadyForImport),
        (terminal.clone(), vec![Failed], ReadyForImport),
        (ImportResult::NoMatch, vec![Failed], ReadyForImport),
        (imported(), vec![Completed, ReadyForImport], Failed),
        (offline, vec![ReadyForImport, ReadyForImport], Failed),
        (terminal, vec![Failed, ReadyForImport, Failed], Importing),
        (
            imported(),
            vec![Completed, ReadyForImport, Failed],
            Importing,
        ),
    ];
    for (answer, fail, after) in cases {
        let what = format!("{answer:?} failing {fail:?}");
        let inner = factory
            .create(EventHub::default(), Duration::from_millis(250))
            .await;
        let download = ready(
            &*inner,
            DownloadSource::BuiltinNzb,
            PathBuf::from("/downloads/mapped"),
        )
        .await;
        let id = download.job_id;
        let store = FlakyStore::new(inner.clone(), &fail);
        let handler = ImportProbe::new(inner.clone(), answer.clone());
        let cleanup = Arc::new(CleanupProbe::new(inner.clone(), false, false));
        let mut cleanups = CleanupRegistry::default();
        cleanups.register(DownloadSource::BuiltinNzb, cleanup.clone());
        let attempt = attempt_import(
            &store,
            &handler,
            &cleanups,
            download.clone(),
            PathMapping::Local,
        )
        .await;
        let job = inner.get(id).await.unwrap();
        assert_eq!(job.state, after, "{what}");
        assert_eq!(cleanup.calls.load(Ordering::SeqCst), 0, "{what}");
        match (after, attempt) {
            (ReadyForImport, Ok(ImportAttempt::Retry { job, failure })) => {
                assert_eq!(job.state, ReadyForImport);
                assert!(failure.retryable);
                assert!(!failure.message.contains("injected"), "{what}");
            }
            (Failed, Ok(ImportAttempt::Finished(job))) => {
                let failure = job.failure.unwrap();
                assert_eq!(failure.code, FailureCode::Other);
                assert!(failure.retryable);
                assert!(!failure.message.contains("injected"), "{what}");
            }
            (Importing, Err(JobStoreError::Backend(_))) => {}
            (_, attempt) => panic!("{what}: {attempt:?}"),
        }
        if after == ReadyForImport {
            // The next attempt (the store is healthy again) finishes normally.
            // (a retryable answer just waits again).
            let done = attempt_import(&store, &handler, &cleanups, download, PathMapping::Local)
                .await
                .unwrap();
            let expected = match answer {
                ImportResult::Imported { .. } => Completed,
                ImportResult::Failed {
                    retryable: true, ..
                } => ReadyForImport,
                _ => Failed,
            };
            let state = match done {
                ImportAttempt::Finished(job) | ImportAttempt::Retry { job, .. } => job.state,
            };
            assert_eq!(state, expected, "{what}");
        }
    }
    // run_import reports the store error, but releases the claim too.
    let inner = factory
        .create(EventHub::default(), Duration::from_millis(250))
        .await;
    let download = ready(
        &*inner,
        DownloadSource::BuiltinNzb,
        PathBuf::from("/downloads/mapped"),
    )
    .await;
    let store = FlakyStore::new(inner.clone(), &[Completed]);
    let handler = ImportProbe::new(inner.clone(), imported());
    let error = run_import(
        &store,
        &handler,
        &CleanupRegistry::default(),
        download.clone(),
        PathMapping::Local,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, JobStoreError::Backend(_)));
    assert_eq!(
        inner.get(download.job_id).await.unwrap().state,
        ReadyForImport
    );
}
