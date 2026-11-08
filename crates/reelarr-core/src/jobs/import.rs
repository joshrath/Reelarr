use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use reelarr_types::{MediaFileId, MovieId};

use super::{Failure, FailureCode, Job, JobId, JobState, JobStore, JobStoreError};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DownloadSource {
    BuiltinNzb,
    External { client: String },
}

/// Emitted by the executor only after repair/extraction. Reelarr-initiated downloads
/// retain their MovieId in executor metadata; only external/manual jobs may omit it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedDownload {
    pub job_id: JobId,
    pub movie_id: Option<MovieId>,
    /// A local container path, mapped by #21 before invoking ImportHandler.
    pub output_path: PathBuf,
    pub category: Option<String>,
    pub source: DownloadSource,
    pub release_title: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportResult {
    Imported { media_file_id: MediaFileId },
    Failed { reason: String, retryable: bool },
    NoMatch,
}

#[async_trait]
pub trait ImportHandler: Send + Sync {
    async fn import(&self, download: CompletedDownload) -> ImportResult;
}

#[derive(Debug, thiserror::Error)]
#[error("source cleanup failed: {0}")]
pub struct CleanupError(#[source] pub Box<dyn std::error::Error + Send + Sync>);

/// Implementations MUST be idempotent: absent or partly empty folders are success.
/// Cleanup is a direct source-specific call after Completed, not a broadcast consumer.
#[async_trait]
pub trait SourceCleanup: Send + Sync {
    async fn on_imported(&self, job_id: JobId, output_path: &Path) -> Result<(), CleanupError>;
}

#[derive(Default)]
pub struct CleanupRegistry {
    handlers: HashMap<DownloadSource, Arc<dyn SourceCleanup>>,
}

impl CleanupRegistry {
    pub fn register(&mut self, source: DownloadSource, handler: Arc<dyn SourceCleanup>) {
        self.handlers.insert(source, handler);
    }
}

/// Explicit result of #21's path-mapping boundary. Core does not infer mappings from
/// whether a remote path happens to exist locally. Local paths must also be absolute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathMapping {
    Local,
    Unmapped,
}

/// Atomically claims ReadyForImport -> Importing before doing any side effects.
/// A repeated call on Completed is a no-op; simultaneous calls cannot duplicate
/// import or cleanup. A crash during import recovers to ReadyForImport, so the media
/// implementation must also make its own file/database work retry-safe.
///
/// The media handler persists media_file_id in its own store; this function changes
/// only the job lifecycle. SQL JobStore persistence follows after PR #39 merges.
pub async fn run_import(
    store: &dyn JobStore,
    handler: &dyn ImportHandler,
    cleanups: &CleanupRegistry,
    download: CompletedDownload,
    mapping: PathMapping,
) -> Result<Job, JobStoreError> {
    let id = download.job_id;
    let snapshot = store.get(id).await?;
    if snapshot.state == JobState::Completed {
        return Ok(snapshot);
    }
    store.transition(id, JobState::Importing, None).await?;
    if mapping == PathMapping::Unmapped || !download.output_path.is_absolute() {
        return store
            .transition(
                id,
                JobState::Failed,
                Some(Failure {
                    code: FailureCode::PathUnmapped,
                    message: "Download output path has no local container mapping".into(),
                    retryable: true,
                }),
            )
            .await;
    }
    if download.source == DownloadSource::BuiltinNzb && download.movie_id.is_none() {
        return store
            .transition(
                id,
                JobState::Failed,
                Some(Failure {
                    code: FailureCode::Other,
                    message: "Reelarr-initiated download is missing its movie id".into(),
                    retryable: false,
                }),
            )
            .await;
    }
    match handler.import(download.clone()).await {
        ImportResult::Imported { .. } => {
            let completed = store.transition(id, JobState::Completed, None).await?;
            let result = match cleanups.handlers.get(&download.source) {
                Some(cleanup) => cleanup.on_imported(id, &download.output_path).await,
                None => Err(CleanupError(Box::new(std::io::Error::other(
                    "no source cleanup registered",
                )))),
            };
            if let Err(error) = result {
                tracing::warn!(job_id = %id, %error, "Best-effort source cleanup failed");
                // A logging/event failure cannot undo successful import.
                if let Err(event_error) = store.message(id, error.to_string()).await {
                    tracing::warn!(job_id = %id, %event_error, "Could not emit cleanup error");
                }
            }
            // Include the cleanup-message watermark when available; Completed is stable.
            Ok(store.get(id).await.unwrap_or(completed))
        }
        ImportResult::Failed { reason, retryable } => {
            store
                .transition(
                    id,
                    JobState::Failed,
                    Some(Failure {
                        code: FailureCode::Other,
                        message: reason,
                        retryable,
                    }),
                )
                .await
        }
        ImportResult::NoMatch => {
            store
                .transition(
                    id,
                    JobState::Failed,
                    Some(Failure {
                        code: FailureCode::NoMatch,
                        message: "No matching movie found".into(),
                        retryable: false,
                    }),
                )
                .await
        }
    }
}
