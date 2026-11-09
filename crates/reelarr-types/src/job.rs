//! WASM-safe job wire DTOs only. Lifecycle logic and UUID generation live in core.
//! JSON uses snake_case states and internally tagged event kinds (`type`).

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Downloading,
    Paused,
    Verifying,
    Repairing,
    Extracting,
    ReadyForImport,
    Importing,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub speed_bps: u64,
    pub eta_secs: Option<u64>,
    pub stage_fraction: Option<f32>,
    pub articles_missing: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobEvent {
    /// Canonical UUIDv7 string; the browser does not generate ids.
    pub job_id: String,
    pub seq: u64,
    /// Unix seconds (UTC).
    pub at: i64,
    pub kind: JobEventKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobEventKind {
    Created,
    StateChanged { from: JobState, to: JobState },
    Progress { progress: Progress },
    Message { message: String },
}

/// Scheduling priority; jobs sort by priority first (Force first), then
/// `queue_position`, then age.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Force,
    High,
    #[default]
    Normal,
    Low,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Nzb,
    ExternalDownload,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    MissingArticles,
    RepairFailed,
    ExtractFailed,
    PasswordRequired,
    DiskFull,
    ServerAuth,
    InvalidNzb,
    Duplicate,
    NoMatch,
    PathUnmapped,
    RootOffline,
    LibraryPermissionDenied,
    NoDefaultRoot,
    DestinationConflict,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub code: FailureCode,
    pub message: String,
    pub retryable: bool,
}

/// One job as the API returns it. Never carries the NZB document, its
/// archive passwords, indexer URLs or NNTP credentials.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobSnapshot {
    /// Canonical UUIDv7 string.
    pub id: String,
    pub kind: JobKind,
    pub state: JobState,
    pub title: String,
    pub priority: Priority,
    pub queue_position: u64,
    pub category: Option<String>,
    pub progress: Progress,
    pub failure: Option<Failure>,
    pub attempts: u32,
    /// Unix seconds (UTC).
    pub created_at: i64,
    pub updated_at: i64,
    /// Where `resume` returns a paused job (`queued` or `downloading`).
    pub resume_state: Option<JobState>,
    /// Watermark: after taking this snapshot, ignore this job's events with
    /// `seq <= last_event_seq`.
    pub last_event_seq: u64,
}

/// All jobs in scheduling order. Also the payload of the SSE `snapshot`
/// event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobList {
    pub jobs: Vec<JobSnapshot>,
}

/// `PUT /jobs/{id}/priority`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetPriorityRequest {
    pub priority: Priority,
}

/// `PUT /jobs/order`: the listed active jobs move to the front of the queue
/// in this order; every other active job keeps its relative order after
/// them. Applied in one transaction. Priority still sorts first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReorderRequest {
    pub job_ids: Vec<String>,
}

/// Query string of `POST /jobs/upload` (the body is the raw NZB).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadNzbParams {
    /// Used when the NZB has no title of its own.
    pub title: Option<String>,
    pub category: Option<String>,
    pub movie_id: Option<i64>,
    pub priority: Option<Priority>,
}

/// `POST /jobs/release`: fetch a release's NZB through the named indexer's
/// shared client and queue it. `download_url` is the release enclosure; it
/// must satisfy that indexer's download-host policy and is never echoed back.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnqueueReleaseRequest {
    pub indexer: String,
    pub download_url: String,
    pub title: String,
    pub category: Option<String>,
    pub movie_id: Option<i64>,
    pub priority: Option<Priority>,
}

impl std::fmt::Debug for EnqueueReleaseRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The enclosure URL usually embeds the indexer API key.
        f.debug_struct("EnqueueReleaseRequest")
            .field("indexer", &self.indexer)
            .field("download_url", &"[REDACTED]")
            .field("title", &self.title)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shape_round_trips() {
        let kinds = [
            JobEventKind::Created,
            JobEventKind::StateChanged {
                from: JobState::Extracting,
                to: JobState::ReadyForImport,
            },
            JobEventKind::Progress {
                progress: Progress {
                    stage_fraction: Some(0.5),
                    articles_missing: 3,
                    ..Progress::default()
                },
            },
            JobEventKind::Message {
                message: "refetch snapshot after lag".into(),
            },
        ];
        for kind in kinds {
            let event = JobEvent {
                job_id: "019a5e00-0000-7000-8000-000000000001".into(),
                seq: 42,
                at: 1700000000,
                kind,
            };
            let json = serde_json::to_value(&event).unwrap();
            assert!(json["kind"]["type"].is_string());
            if let JobEventKind::StateChanged { .. } = &event.kind {
                assert_eq!(json["kind"]["to"], "ready_for_import");
            }
            assert_eq!(serde_json::from_value::<JobEvent>(json).unwrap(), event);
        }
    }

    #[test]
    fn snapshot_and_requests_round_trip() {
        let snapshot = JobSnapshot {
            id: "019a5e00-0000-7000-8000-000000000001".into(),
            kind: JobKind::Nzb,
            state: JobState::Failed,
            title: "Movie".into(),
            priority: Priority::Force,
            queue_position: 3,
            category: Some("movies".into()),
            progress: Progress::default(),
            failure: Some(Failure {
                code: FailureCode::MissingArticles,
                message: "gone".into(),
                retryable: true,
            }),
            attempts: 1,
            created_at: 1,
            updated_at: 2,
            resume_state: None,
            last_event_seq: 9,
        };
        let json = serde_json::to_value(JobList {
            jobs: vec![snapshot.clone()],
        })
        .unwrap();
        assert_eq!(json["jobs"][0]["priority"], "force");
        assert_eq!(json["jobs"][0]["failure"]["code"], "missing_articles");
        assert_eq!(
            serde_json::from_value::<JobList>(json).unwrap().jobs,
            vec![snapshot]
        );
        let params: UploadNzbParams = serde_json::from_str(r#"{"priority":"high"}"#).unwrap();
        assert_eq!(params.priority, Some(Priority::High));
        let release = EnqueueReleaseRequest {
            indexer: "idx".into(),
            download_url: "https://idx.example/get?apikey=SECRET".into(),
            title: "t".into(),
            category: None,
            movie_id: None,
            priority: None,
        };
        assert!(!format!("{release:?}").contains("SECRET"));
    }
}
