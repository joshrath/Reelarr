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
            ImportResult::Failed { reason, retryable } => {
                assert_eq!(failure.code, FailureCode::Other);
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

pub async fn run(factory: &dyn StoreFactory) {
    imported_maps_to_completed_and_cleans_once_after_completion(factory).await;
    generic_import_failure_and_no_match_have_distinct_outcomes(factory).await;
    unmapped_or_relative_paths_fail_before_import_or_cleanup(factory).await;
    builtin_jobs_require_the_original_movie_id(factory).await;
    cleanup_errors_are_messages_not_import_failures_and_are_not_retried(factory).await;
    registry_selects_the_matching_source_and_concurrent_handoffs_import_once(factory).await;
    missing_cleanup_is_also_best_effort_and_emits_a_message(factory).await;
}
