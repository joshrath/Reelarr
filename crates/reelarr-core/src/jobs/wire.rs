//! Explicit one-way adapters keep UI DTOs free of Tokio, UUID RNGs and core logic.

use super::{
    Failure, FailureCode, Job, JobEvent, JobEventKind, JobKind, JobState, Priority, Progress,
};
use reelarr_types::job as wire;

impl From<JobState> for wire::JobState {
    fn from(state: JobState) -> Self {
        match state {
            JobState::Queued => Self::Queued,
            JobState::Downloading => Self::Downloading,
            JobState::Paused => Self::Paused,
            JobState::Verifying => Self::Verifying,
            JobState::Repairing => Self::Repairing,
            JobState::Extracting => Self::Extracting,
            JobState::ReadyForImport => Self::ReadyForImport,
            JobState::Importing => Self::Importing,
            JobState::Completed => Self::Completed,
            JobState::Failed => Self::Failed,
            JobState::Cancelled => Self::Cancelled,
        }
    }
}

impl From<Progress> for wire::Progress {
    fn from(value: Progress) -> Self {
        Self {
            bytes_done: value.bytes_done,
            bytes_total: value.bytes_total,
            speed_bps: value.speed_bps,
            eta_secs: value.eta_secs,
            stage_fraction: value.stage_fraction,
            articles_missing: value.articles_missing,
        }
    }
}

impl From<JobEvent> for wire::JobEvent {
    fn from(event: JobEvent) -> Self {
        Self {
            job_id: event.job_id.to_string(),
            seq: event.seq,
            at: event.at,
            kind: match event.kind {
                JobEventKind::Created => wire::JobEventKind::Created,
                JobEventKind::StateChanged { from, to } => wire::JobEventKind::StateChanged {
                    from: from.into(),
                    to: to.into(),
                },
                JobEventKind::Progress { progress } => wire::JobEventKind::Progress {
                    progress: progress.into(),
                },
                JobEventKind::Message { message } => wire::JobEventKind::Message { message },
            },
        }
    }
}

impl From<Priority> for wire::Priority {
    fn from(value: Priority) -> Self {
        match value {
            Priority::Force => Self::Force,
            Priority::High => Self::High,
            Priority::Normal => Self::Normal,
            Priority::Low => Self::Low,
        }
    }
}

impl From<wire::Priority> for Priority {
    fn from(value: wire::Priority) -> Self {
        match value {
            wire::Priority::Force => Self::Force,
            wire::Priority::High => Self::High,
            wire::Priority::Normal => Self::Normal,
            wire::Priority::Low => Self::Low,
        }
    }
}

impl From<JobKind> for wire::JobKind {
    fn from(value: JobKind) -> Self {
        match value {
            JobKind::Nzb => Self::Nzb,
            JobKind::ExternalDownload => Self::ExternalDownload,
        }
    }
}

impl From<FailureCode> for wire::FailureCode {
    fn from(value: FailureCode) -> Self {
        match value {
            FailureCode::MissingArticles => Self::MissingArticles,
            FailureCode::RepairFailed => Self::RepairFailed,
            FailureCode::ExtractFailed => Self::ExtractFailed,
            FailureCode::PasswordRequired => Self::PasswordRequired,
            FailureCode::DiskFull => Self::DiskFull,
            FailureCode::ServerAuth => Self::ServerAuth,
            FailureCode::InvalidNzb => Self::InvalidNzb,
            FailureCode::Duplicate => Self::Duplicate,
            FailureCode::NoMatch => Self::NoMatch,
            FailureCode::PathUnmapped => Self::PathUnmapped,
            FailureCode::RootOffline => Self::RootOffline,
            FailureCode::LibraryPermissionDenied => Self::LibraryPermissionDenied,
            FailureCode::NoDefaultRoot => Self::NoDefaultRoot,
            FailureCode::DestinationConflict => Self::DestinationConflict,
            FailureCode::Other => Self::Other,
        }
    }
}

impl From<Failure> for wire::Failure {
    fn from(value: Failure) -> Self {
        Self {
            code: value.code.into(),
            message: value.message,
            retryable: value.retryable,
        }
    }
}

/// Snapshot for the API. The resume hint is an executor detail and stays
/// server-side.
impl From<Job> for wire::JobSnapshot {
    fn from(job: Job) -> Self {
        Self {
            id: job.id.to_string(),
            kind: job.kind.into(),
            state: job.state.into(),
            title: job.title,
            priority: job.priority.into(),
            queue_position: job.queue_position,
            category: job.category,
            progress: job.progress.into(),
            failure: job.failure.map(Into::into),
            attempts: job.attempts,
            created_at: job.created_at,
            updated_at: job.updated_at,
            resume_state: job.resume_state.map(Into::into),
            last_event_seq: job.last_event_seq,
        }
    }
}
