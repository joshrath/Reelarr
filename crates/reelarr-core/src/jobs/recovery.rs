use super::{CompletedStage, Job, JobState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Recovery {
    pub from: JobState,
    pub to: JobState,
    pub resume_hint: Option<CompletedStage>,
}

/// Applied once at startup before workers run, not a user-visible retry transition.
/// Stage executors must re-enter their own stage when its files are present. The caller
/// checks file availability; NZB segment metadata remains in NzbStore, not this snapshot.
pub fn recover_after_restart(job: &mut Job, files_exist: bool) -> Recovery {
    use JobState::*;
    let from = job.state;
    job.state = match from {
        Downloading => Queued,
        Verifying | Repairing | Extracting if !files_exist => Queued,
        Importing => ReadyForImport,
        other => other,
    };
    job.progress.speed_bps = 0;
    job.progress.eta_secs = None;
    job.progress.stage_fraction = None;
    Recovery {
        from,
        to: job.state,
        resume_hint: job.resume_hint,
    }
}
