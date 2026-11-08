use super::{CompletedStage, Failure, FailureCode, Job, JobState};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TransitionError {
    #[error("illegal job transition {from:?} -> {to:?}")]
    Illegal { from: JobState, to: JobState },
    #[error("a Failed transition requires a failure, and other transitions must not supply one")]
    FailureRequired,
}

/// Validates the exact lifecycle graph, including remembered pause/manual-import context.
/// Retry is an explicit caller action; `retryable` is a scheduling hint, not a prohibition
/// on a user-requested retry after fixing the underlying cause.
pub fn validate_transition(job: &Job, to: JobState) -> Result<(), TransitionError> {
    use JobState::*;
    let from = job.state;
    let allowed = if to == Cancelled {
        !from.is_terminal()
    } else {
        match from {
            Queued => matches!(to, Downloading | Paused),
            Downloading => matches!(to, Verifying | Paused | Failed | Queued),
            Paused => matches!(to, Queued | Downloading) && job.resume_state == Some(to),
            Verifying => matches!(to, Repairing | Extracting | ReadyForImport | Failed),
            Repairing => matches!(to, Extracting | Failed),
            Extracting => matches!(to, ReadyForImport | Failed),
            ReadyForImport => to == Importing,
            Importing => matches!(to, Completed | Failed),
            Failed => {
                to == Queued
                    || (to == ReadyForImport
                        && job
                            .failure
                            .as_ref()
                            .is_some_and(|f| f.code == FailureCode::NoMatch))
            }
            Completed | Cancelled => false,
        }
    };
    if allowed {
        Ok(())
    } else {
        Err(TransitionError::Illegal { from, to })
    }
}

pub(crate) fn apply_transition(
    job: &mut Job,
    to: JobState,
    failure: Option<Failure>,
) -> Result<(), TransitionError> {
    validate_transition(job, to)?;
    if (to == JobState::Failed) != failure.is_some() {
        return Err(TransitionError::FailureRequired);
    }
    let from = job.state;
    if to == JobState::Paused {
        job.resume_state = Some(from);
    } else {
        job.resume_state = None;
    }
    if from == JobState::Queued && to == JobState::Downloading {
        job.attempts = job.attempts.saturating_add(1);
    }
    job.resume_hint = match (from, to) {
        (JobState::Downloading, JobState::Verifying) => Some(CompletedStage::Downloading),
        (
            JobState::Verifying,
            JobState::Repairing | JobState::Extracting | JobState::ReadyForImport,
        ) => Some(CompletedStage::Verifying),
        (JobState::Repairing, JobState::Extracting) => Some(CompletedStage::Repairing),
        (JobState::Extracting, JobState::ReadyForImport) => Some(CompletedStage::Extracting),
        _ => job.resume_hint,
    };
    job.progress.speed_bps = 0;
    job.progress.eta_secs = None;
    if to != JobState::Paused && from != JobState::Paused {
        job.progress.stage_fraction = None;
    }
    job.failure = failure;
    job.state = to;
    Ok(())
}
