use std::{
    fmt,
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use uuid::Uuid;

/// Server-generated UUIDv7. UI DTOs carry its canonical string, not an RNG dependency.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(Uuid);

impl JobId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
    pub fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for JobId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("job id must be a UUIDv7")]
pub struct InvalidJobId;

impl FromStr for JobId {
    type Err = InvalidJobId;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let id = Uuid::parse_str(value).map_err(|_| InvalidJobId)?;
        if id.get_version_num() != 7 || id.get_variant() != uuid::Variant::RFC4122 {
            return Err(InvalidJobId);
        }
        Ok(Self(id))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    Nzb,
    ExternalDownload,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

impl JobState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// Natural ordering is scheduling order (Force first).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Force,
    High,
    #[default]
    Normal,
    Low,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Progress {
    pub bytes_done: u64,
    /// Zero means unknown; bytes_done may exceed zero in that case.
    pub bytes_total: u64,
    pub speed_bps: u64,
    pub eta_secs: Option<u64>,
    pub stage_fraction: Option<f32>,
    pub articles_missing: u32,
}

impl Progress {
    pub fn is_valid(&self) -> bool {
        (self.bytes_total == 0 || self.bytes_done <= self.bytes_total)
            && self
                .stage_fraction
                .is_none_or(|f| f.is_finite() && (0.0..=1.0).contains(&f))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    /// The library root chosen for an import is offline or unreadable.
    RootOffline,
    /// The library refused the write (read-only share, permissions).
    LibraryPermissionDenied,
    /// Several library roots exist and none is set as the import default,
    /// or there is no library root at all.
    NoDefaultRoot,
    /// A different file already exists at the import destination.
    DestinationConflict,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub code: FailureCode,
    pub message: String,
    pub retryable: bool,
}

/// Last fully completed stage; survives retries/restarts as an executor resume hint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletedStage {
    Downloading,
    Verifying,
    Repairing,
    Extracting,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub id: JobId,
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
    /// Only Paused jobs carry this; Queued or Downloading, never a processing stage.
    pub resume_state: Option<JobState>,
    pub resume_hint: Option<CompletedStage>,
    /// Snapshot watermark. After broadcast lag refetch, ignore events <= this seq.
    pub last_event_seq: u64,
}

impl Job {
    pub fn new(kind: JobKind, title: impl Into<String>) -> Self {
        let now = now();
        Self {
            id: JobId::new(),
            kind,
            state: JobState::Queued,
            title: title.into(),
            priority: Priority::Normal,
            queue_position: 0,
            category: None,
            progress: Progress::default(),
            failure: None,
            attempts: 0,
            created_at: now,
            updated_at: now,
            resume_state: None,
            resume_hint: None,
            last_event_seq: 0,
        }
    }
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
