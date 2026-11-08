//! Explicit one-way adapters keep UI DTOs free of Tokio, UUID RNGs and core logic.

use super::{JobEvent, JobEventKind, JobState, Progress};
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
