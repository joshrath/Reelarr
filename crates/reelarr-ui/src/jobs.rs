//! What The Back Room shows for the job API (`/api/v1/jobs`). Plain data
//! and copy, so it can be tested without a browser.
//!
//! The page keeps the job list the server sent and applies the live feed
//! (`GET /api/v1/jobs/events`) on top: a `snapshot` frame replaces the list,
//! a progress event updates one job in place, and any other event (a new
//! job, a state change, a queue change) re-reads the list, since only the
//! full snapshot carries failures, resume states and the queue order.

use reelarr_types::{
    FailureCode, JobEvent, JobEventKind, JobKind, JobList, JobSnapshot, JobState, Priority,
};

use crate::staff::{format_bytes, format_eta, format_rate, utc_datetime};

pub const JOBS_PATH: &str = "/api/v1/jobs";
pub const JOB_EVENTS_PATH: &str = "/api/v1/jobs/events";

/// Job ids are canonical UUID strings. Anything else is never put in a URL.
pub fn is_job_id(id: &str) -> bool {
    id.len() == 36
        && id.chars().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// A button on a job card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobAction {
    Pause,
    Resume,
    Cancel,
}

impl JobAction {
    /// `POST /api/v1/jobs/{id}/{action}`, or `None` for a malformed id.
    pub fn path(self, id: &str) -> Option<String> {
        let action = match self {
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Cancel => "cancel",
        };
        is_job_id(id).then(|| format!("{JOBS_PATH}/{id}/{action}"))
    }

    /// Whether the server allows this action in `state`.
    pub fn allowed(self, state: JobState) -> bool {
        match self {
            Self::Pause => matches!(state, JobState::Queued | JobState::Downloading),
            Self::Resume => state == JobState::Paused,
            Self::Cancel => !is_finished(state),
        }
    }

    /// Words for the screen reader once it's done.
    pub fn done(self, title: &str) -> String {
        match self {
            Self::Pause => format!("Paused {title}."),
            Self::Resume => format!("Resumed {title}."),
            Self::Cancel => format!("Cancelled {title}."),
        }
    }
}

/// `PUT /api/v1/jobs/{id}/priority`.
pub fn priority_path(id: &str) -> Option<String> {
    is_job_id(id).then(|| format!("{JOBS_PATH}/{id}/priority"))
}

/// Completed, failed and cancelled jobs are finished.
pub fn is_finished(state: JobState) -> bool {
    matches!(
        state,
        JobState::Completed | JobState::Failed | JobState::Cancelled
    )
}

pub fn state_label(state: JobState) -> &'static str {
    match state {
        JobState::Queued => "Queued",
        JobState::Downloading => "Downloading",
        JobState::Paused => "Paused",
        JobState::Verifying => "Verifying",
        JobState::Repairing => "Repairing",
        JobState::Extracting => "Extracting",
        JobState::ReadyForImport => "Ready to import",
        JobState::Importing => "Importing",
        JobState::Completed => "Completed",
        JobState::Failed => "Failed",
        JobState::Cancelled => "Cancelled",
    }
}

/// The colour family for a state. The state is always written out too.
pub fn state_tone(state: JobState) -> &'static str {
    match state {
        JobState::Downloading
        | JobState::Verifying
        | JobState::Repairing
        | JobState::Extracting
        | JobState::Importing => "running",
        JobState::Queued | JobState::ReadyForImport => "waiting",
        JobState::Paused => "paused",
        JobState::Completed => "done",
        JobState::Failed => "failed",
        JobState::Cancelled => "cancelled",
    }
}

/// One line on what the deck is doing.
pub fn state_note(job: &JobSnapshot) -> &'static str {
    match job.state {
        JobState::Queued => "Waiting for a free deck.",
        JobState::Downloading => "Dubbing from Usenet.",
        JobState::Paused => match job.resume_state {
            Some(JobState::Downloading) => "Paused mid-dub. Resume picks up where it stopped.",
            _ => "Paused before it started. Resume puts it back in the queue.",
        },
        JobState::Verifying => "Checking the files against their repair data.",
        JobState::Repairing => "Repairing damaged parts.",
        JobState::Extracting => "Unpacking the archive.",
        JobState::ReadyForImport => "Done downloading. Waiting to go on the shelf.",
        JobState::Importing => "Moving the movie into the library.",
        JobState::Completed => "On the shelf.",
        JobState::Failed => "Stopped with a problem.",
        JobState::Cancelled => "Cancelled.",
    }
}

pub const PRIORITIES: [Priority; 4] = [
    Priority::Force,
    Priority::High,
    Priority::Normal,
    Priority::Low,
];

pub fn priority_label(priority: Priority) -> &'static str {
    match priority {
        Priority::Force => "Force (start now)",
        Priority::High => "High",
        Priority::Normal => "Normal",
        Priority::Low => "Low",
    }
}

/// The `<select>` value for a priority.
pub fn priority_value(priority: Priority) -> &'static str {
    match priority {
        Priority::Force => "force",
        Priority::High => "high",
        Priority::Normal => "normal",
        Priority::Low => "low",
    }
}

pub fn priority_from_value(value: &str) -> Option<Priority> {
    PRIORITIES.into_iter().find(|p| priority_value(*p) == value)
}

pub fn kind_label(kind: JobKind) -> &'static str {
    match kind {
        JobKind::Nzb => "Usenet",
        JobKind::ExternalDownload => "External",
    }
}

pub fn failure_label(code: FailureCode) -> &'static str {
    match code {
        FailureCode::MissingArticles => "Parts missing from the server",
        FailureCode::RepairFailed => "Repair failed",
        FailureCode::ExtractFailed => "Unpacking failed",
        FailureCode::PasswordRequired => "Archive needs a password",
        FailureCode::DiskFull => "Disk full",
        FailureCode::ServerAuth => "Usenet server sign-in failed",
        FailureCode::InvalidNzb => "Unreadable NZB",
        FailureCode::Duplicate => "Already downloaded",
        FailureCode::NoMatch => "No matching movie",
        FailureCode::PathUnmapped => "Download folder not mapped",
        FailureCode::RootOffline => "Library folder offline",
        FailureCode::LibraryPermissionDenied => "No permission to write to the library",
        FailureCode::NoDefaultRoot => "No library folder to import into",
        FailureCode::DestinationConflict => "A different file is already there",
        FailureCode::Other => "Something went wrong",
    }
}

/// How far along a job is, 0–100, when the server says.
pub fn percent(job: &JobSnapshot) -> Option<u8> {
    let p = &job.progress;
    let fraction = match job.state {
        JobState::Completed => Some(1.0),
        JobState::Verifying | JobState::Repairing | JobState::Extracting | JobState::Importing => {
            p.stage_fraction.map(f64::from)
        }
        _ if p.bytes_total > 0 => Some(p.bytes_done as f64 / p.bytes_total as f64),
        _ => None,
    }?;
    fraction
        .is_finite()
        .then(|| (fraction.clamp(0.0, 1.0) * 100.0).floor() as u8)
}

/// Everything a job card prints, already in words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobCard {
    pub id: String,
    pub title: String,
    pub kind: &'static str,
    pub state: JobState,
    pub state_label: &'static str,
    pub tone: &'static str,
    pub note: &'static str,
    pub priority: Priority,
    pub category: Option<String>,
    pub percent: Option<u8>,
    /// `1.2 GB of 4.0 GB`, or what's done when the total isn't known.
    pub size: Option<String>,
    pub rate: Option<String>,
    pub eta: Option<String>,
    pub missing: Option<String>,
    pub failure: Option<FailureNote>,
    pub attempts: Option<String>,
    pub updated: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureNote {
    pub label: &'static str,
    pub message: String,
    pub retryable: bool,
}

impl JobCard {
    pub fn of(job: &JobSnapshot) -> Self {
        let p = &job.progress;
        let downloading = job.state == JobState::Downloading;
        let size = match (p.bytes_done, p.bytes_total) {
            (0, 0) => None,
            (done, 0) => Some(format!("{} so far", format_bytes(done))),
            (done, total) => Some(format!("{} of {}", format_bytes(done), format_bytes(total))),
        };
        Self {
            id: job.id.clone(),
            title: job.title.clone(),
            kind: kind_label(job.kind),
            state: job.state,
            state_label: state_label(job.state),
            tone: state_tone(job.state),
            note: state_note(job),
            priority: job.priority,
            category: job.category.clone(),
            percent: percent(job),
            size,
            rate: downloading.then(|| format_rate(p.speed_bps)).flatten(),
            eta: downloading
                .then_some(p.eta_secs)
                .flatten()
                .map(|s| format!("{} left", format_eta(s))),
            missing: (p.articles_missing > 0).then(|| match p.articles_missing {
                1 => "1 part missing".to_string(),
                n => format!("{n} parts missing"),
            }),
            failure: job.failure.as_ref().map(|f| FailureNote {
                label: failure_label(f.code),
                message: f.message.clone(),
                retryable: f.retryable,
            }),
            attempts: (job.attempts > 1).then(|| format!("Attempt {}", job.attempts)),
            updated: utc_datetime(job.updated_at),
        }
    }
}

/// Whether a finished download is waiting to go on a shelf (or is held
/// because no library folder could be chosen).
pub fn waiting_for_import(jobs: &[JobSnapshot]) -> bool {
    jobs.iter().any(|j| {
        j.state == JobState::ReadyForImport
            || (!is_finished(j.state)
                && j.failure
                    .as_ref()
                    .is_some_and(|f| f.code == FailureCode::NoDefaultRoot))
    })
}

/// Jobs still being worked on, and finished ones, each in server order.
pub fn split(jobs: &[JobSnapshot]) -> (Vec<JobSnapshot>, Vec<JobSnapshot>) {
    jobs.iter().cloned().partition(|j| !is_finished(j.state))
}

/// Telemetry for the lab header, from the jobs themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Telemetry {
    /// Unfinished jobs.
    pub on_decks: usize,
    pub downloading: usize,
    /// Sum of the downloading jobs' speeds, in words.
    pub throughput: Option<String>,
}

pub fn telemetry(jobs: &[JobSnapshot]) -> Telemetry {
    let downloading = jobs.iter().filter(|j| j.state == JobState::Downloading);
    Telemetry {
        on_decks: jobs.iter().filter(|j| !is_finished(j.state)).count(),
        downloading: downloading.clone().count(),
        throughput: format_rate(downloading.map(|j| j.progress.speed_bps).sum()),
    }
}

/// A frame from the live feed.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Snapshot(JobList),
    Job(JobEvent),
}

/// Reads a feed frame by its SSE event name; `None` if it isn't one.
pub fn parse_frame(event: &str, data: &str) -> Option<Frame> {
    match event {
        "snapshot" => serde_json::from_str(data).ok().map(Frame::Snapshot),
        "job" => serde_json::from_str(data).ok().map(Frame::Job),
        _ => None,
    }
}

/// What applying a live event did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// The list was updated in place.
    Updated,
    /// Old news (at or below the job's watermark).
    Ignored,
    /// The list has to be read again to show this.
    Refresh,
}

/// Applies one live event to the list.
pub fn apply_event(jobs: &mut [JobSnapshot], event: &JobEvent) -> Applied {
    let Some(job) = jobs.iter_mut().find(|j| j.id == event.job_id) else {
        // A new job, or one we haven't seen yet.
        return Applied::Refresh;
    };
    if event.seq <= job.last_event_seq {
        return Applied::Ignored;
    }
    job.last_event_seq = event.seq;
    job.updated_at = event.at;
    match &event.kind {
        JobEventKind::Progress { progress } => {
            job.progress = progress.clone();
            Applied::Updated
        }
        JobEventKind::StateChanged { to, .. } => {
            // Show the new state now; the re-read brings any failure.
            job.state = *to;
            Applied::Refresh
        }
        // Messages can carry server-side detail; they're never shown, but
        // one may mean the queue order changed.
        JobEventKind::Created | JobEventKind::Message { .. } => Applied::Refresh,
    }
}

/// The state of the live feed, for the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feed {
    Connecting,
    Live,
    /// The browser is reconnecting on its own.
    Reconnecting,
    /// The feed closed; the list only changes on refresh.
    Stopped,
}

impl Feed {
    pub fn label(self) -> &'static str {
        match self {
            Self::Connecting => "Connecting",
            Self::Live => "Live",
            Self::Reconnecting => "Reconnecting",
            Self::Stopped => "Not live",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reelarr_types::{Failure, Progress};

    const ID: &str = "019a5e00-0000-7000-8000-000000000001";

    fn job(state: JobState) -> JobSnapshot {
        JobSnapshot {
            id: ID.into(),
            kind: JobKind::Nzb,
            state,
            title: "Test job".into(),
            priority: Priority::Normal,
            queue_position: 1,
            category: None,
            progress: Progress::default(),
            failure: None,
            attempts: 1,
            created_at: 0,
            updated_at: 0,
            resume_state: None,
            last_event_seq: 5,
        }
    }

    fn event(seq: u64, kind: JobEventKind) -> JobEvent {
        JobEvent {
            job_id: ID.into(),
            seq,
            at: 60,
            kind,
        }
    }

    #[test]
    fn only_uuid_ids_make_paths() {
        assert!(is_job_id(ID));
        assert!(!is_job_id("../../settings/nntp"));
        assert!(!is_job_id("019a5e00-0000-7000-8000-00000000000g"));
        assert_eq!(
            JobAction::Pause.path(ID).as_deref(),
            Some("/api/v1/jobs/019a5e00-0000-7000-8000-000000000001/pause")
        );
        assert_eq!(JobAction::Cancel.path("1/cancel?x"), None);
        assert_eq!(
            priority_path(ID).as_deref(),
            Some("/api/v1/jobs/019a5e00-0000-7000-8000-000000000001/priority")
        );
        assert_eq!(priority_path("nope"), None);
    }

    #[test]
    fn actions_follow_the_lifecycle() {
        use JobState::*;
        for state in [Queued, Downloading] {
            assert!(JobAction::Pause.allowed(state));
        }
        for state in [Paused, Verifying, Importing, Completed] {
            assert!(!JobAction::Pause.allowed(state));
        }
        assert!(JobAction::Resume.allowed(Paused));
        assert!(!JobAction::Resume.allowed(Queued));
        for state in [Queued, Downloading, Paused, Extracting, ReadyForImport] {
            assert!(JobAction::Cancel.allowed(state), "{state:?}");
        }
        for state in [Completed, Failed, Cancelled] {
            assert!(!JobAction::Cancel.allowed(state));
            assert!(is_finished(state));
        }
        assert_eq!(JobAction::Cancel.done("X"), "Cancelled X.");
    }

    #[test]
    fn every_state_has_words() {
        use JobState::*;
        for state in [
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
        ] {
            assert!(!state_label(state).is_empty());
            assert!(!state_tone(state).is_empty());
            assert!(!state_note(&job(state)).is_empty());
        }
        assert_eq!(state_label(ReadyForImport), "Ready to import");
        let mut paused = job(Paused);
        paused.resume_state = Some(Downloading);
        assert!(state_note(&paused).contains("where it stopped"));
    }

    #[test]
    fn priorities_round_trip_through_the_select() {
        for p in PRIORITIES {
            assert_eq!(priority_from_value(priority_value(p)), Some(p));
            assert!(!priority_label(p).is_empty());
        }
        assert_eq!(priority_from_value("urgent"), None);
    }

    #[test]
    fn percent_uses_bytes_or_the_stage() {
        let mut j = job(JobState::Downloading);
        assert_eq!(percent(&j), None);
        j.progress.bytes_total = 1000;
        j.progress.bytes_done = 785;
        assert_eq!(percent(&j), Some(78));
        j.state = JobState::Extracting;
        assert_eq!(percent(&j), None);
        j.progress.stage_fraction = Some(0.5);
        assert_eq!(percent(&j), Some(50));
        j.progress.stage_fraction = Some(f32::NAN);
        assert_eq!(percent(&j), None);
        assert_eq!(percent(&job(JobState::Completed)), Some(100));
        assert_eq!(percent(&job(JobState::Failed)), None);
    }

    #[test]
    fn card_words_come_from_the_snapshot() {
        let mut j = job(JobState::Downloading);
        j.progress = Progress {
            bytes_done: 1_200_000_000,
            bytes_total: 4_000_000_000,
            speed_bps: 48_200_000,
            eta_secs: Some(725),
            stage_fraction: None,
            articles_missing: 3,
        };
        j.attempts = 2;
        let card = JobCard::of(&j);
        assert_eq!(card.size.as_deref(), Some("1.2 GB of 4.0 GB"));
        assert_eq!(card.rate.as_deref(), Some("48.2 MB/s"));
        assert_eq!(card.eta.as_deref(), Some("12m 05s left"));
        assert_eq!(card.missing.as_deref(), Some("3 parts missing"));
        assert_eq!(card.attempts.as_deref(), Some("Attempt 2"));
        assert_eq!(card.kind, "Usenet");
        assert_eq!(card.percent, Some(30));

        // Speed and time left only show while downloading.
        j.state = JobState::Paused;
        let card = JobCard::of(&j);
        assert_eq!(card.rate, None);
        assert_eq!(card.eta, None);

        let mut failed = job(JobState::Failed);
        failed.failure = Some(Failure {
            code: FailureCode::DiskFull,
            message: "No space left".into(),
            retryable: true,
        });
        let card = JobCard::of(&failed);
        let failure = card.failure.unwrap();
        assert_eq!(failure.label, "Disk full");
        assert!(failure.retryable);
        assert_eq!(card.size, None);
        assert_eq!(card.attempts, None);
    }

    #[test]
    fn telemetry_counts_real_jobs_only() {
        let mut a = job(JobState::Downloading);
        a.progress.speed_bps = 1_000_000;
        let mut b = job(JobState::Downloading);
        b.progress.speed_bps = 2_500_000;
        let t = telemetry(&[a, b, job(JobState::Queued), job(JobState::Completed)]);
        assert_eq!(t.on_decks, 3);
        assert_eq!(t.downloading, 2);
        assert_eq!(t.throughput.as_deref(), Some("3.5 MB/s"));
        let empty = telemetry(&[]);
        assert_eq!(empty.on_decks, 0);
        assert_eq!(empty.throughput, None);
    }

    #[test]
    fn import_waits_are_spotted() {
        assert!(!waiting_for_import(&[job(JobState::Downloading)]));
        assert!(waiting_for_import(&[job(JobState::ReadyForImport)]));
        let mut held = job(JobState::Importing);
        held.failure = Some(Failure {
            code: FailureCode::NoDefaultRoot,
            message: "no default".into(),
            retryable: true,
        });
        assert!(waiting_for_import(&[held.clone()]));
        held.state = JobState::Failed;
        assert!(!waiting_for_import(&[held]));
    }

    #[test]
    fn split_keeps_server_order() {
        let mut done = job(JobState::Completed);
        done.title = "done".into();
        let (active, finished) = split(&[job(JobState::Queued), done, job(JobState::Paused)]);
        assert_eq!(active.len(), 2);
        assert_eq!(finished[0].title, "done");
    }

    #[test]
    fn frames_parse_by_event_name() {
        let list = r#"{"jobs":[]}"#;
        assert_eq!(
            parse_frame("snapshot", list),
            Some(Frame::Snapshot(JobList { jobs: vec![] }))
        );
        let ev = serde_json::to_string(&event(6, JobEventKind::Created)).unwrap();
        assert!(matches!(parse_frame("job", &ev), Some(Frame::Job(_))));
        assert_eq!(parse_frame("job", "{}"), None);
        assert_eq!(parse_frame("other", list), None);
    }

    #[test]
    fn events_apply_in_place_or_ask_for_a_refresh() {
        let mut jobs = vec![job(JobState::Downloading)];
        let progress = Progress {
            bytes_done: 10,
            bytes_total: 20,
            ..Progress::default()
        };
        // At or below the watermark: ignored.
        assert_eq!(
            apply_event(
                &mut jobs,
                &event(
                    5,
                    JobEventKind::Progress {
                        progress: progress.clone()
                    }
                )
            ),
            Applied::Ignored
        );
        assert_eq!(jobs[0].progress.bytes_done, 0);
        assert_eq!(
            apply_event(
                &mut jobs,
                &event(
                    7,
                    JobEventKind::Progress {
                        progress: progress.clone()
                    }
                )
            ),
            Applied::Updated
        );
        assert_eq!(jobs[0].progress, progress);
        assert_eq!(jobs[0].last_event_seq, 7);
        assert_eq!(jobs[0].updated_at, 60);
        assert_eq!(
            apply_event(
                &mut jobs,
                &event(
                    8,
                    JobEventKind::StateChanged {
                        from: JobState::Downloading,
                        to: JobState::Verifying
                    }
                )
            ),
            Applied::Refresh
        );
        assert_eq!(jobs[0].state, JobState::Verifying);
        assert_eq!(
            apply_event(
                &mut jobs,
                &event(
                    9,
                    JobEventKind::Message {
                        message: "Queue order updated".into()
                    }
                )
            ),
            Applied::Refresh
        );
        // Unknown job: re-read.
        let mut other = event(1, JobEventKind::Created);
        other.job_id = "019a5e00-0000-7000-8000-000000000002".into();
        assert_eq!(apply_event(&mut jobs, &other), Applied::Refresh);
    }

    #[test]
    fn feed_states_have_words() {
        for feed in [
            Feed::Connecting,
            Feed::Live,
            Feed::Reconnecting,
            Feed::Stopped,
        ] {
            assert!(!feed.label().is_empty());
        }
    }
}
