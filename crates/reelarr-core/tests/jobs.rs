use async_trait::async_trait;
use reelarr_core::*;
use std::{sync::Arc, time::Duration};
mod support {
    pub mod import;
    pub mod job_store;
}
struct MemoryFactory;
#[async_trait]
impl support::job_store::StoreFactory for MemoryFactory {
    async fn create(&self, events: EventHub, interval: Duration) -> Arc<dyn JobStore> {
        Arc::new(InMemoryJobStore::new(events, interval))
    }
}
#[tokio::test]
async fn in_memory_store_contract() {
    support::job_store::run(&MemoryFactory).await;
}
#[tokio::test]
async fn in_memory_import_contract() {
    support::import::run(&MemoryFactory).await;
}

fn failure(code: FailureCode) -> Failure {
    Failure {
        code,
        message: "independent test fixture".into(),
        retryable: true,
    }
}

const STATES: [JobState; 11] = [
    JobState::Queued,
    JobState::Downloading,
    JobState::Paused,
    JobState::Verifying,
    JobState::Repairing,
    JobState::Extracting,
    JobState::ReadyForImport,
    JobState::Importing,
    JobState::Completed,
    JobState::Failed,
    JobState::Cancelled,
];

#[test]
fn transition_matrix_covers_every_pair_and_context() {
    use JobState::*;
    let edges = [
        (Queued, Downloading),
        (Queued, Paused),
        (Queued, Cancelled),
        (Downloading, Verifying),
        (Downloading, Paused),
        (Downloading, Failed),
        (Downloading, Queued),
        (Downloading, Cancelled),
        (Paused, Queued),
        (Paused, Cancelled),
        (Verifying, Repairing),
        (Verifying, Extracting),
        (Verifying, ReadyForImport),
        (Verifying, Failed),
        (Verifying, Cancelled),
        (Repairing, Extracting),
        (Repairing, Failed),
        (Repairing, Cancelled),
        (Extracting, ReadyForImport),
        (Extracting, Failed),
        (Extracting, Cancelled),
        (ReadyForImport, Importing),
        (ReadyForImport, Cancelled),
        (Importing, Completed),
        (Importing, Failed),
        (Importing, ReadyForImport),
        (Importing, Cancelled),
        (Failed, Queued),
    ];
    for from in STATES {
        for to in STATES {
            let mut job = Job::new(JobKind::Nzb, "matrix");
            job.state = from;
            job.resume_state = Some(Queued);
            job.failure = Some(failure(FailureCode::Other));
            assert_eq!(
                validate_transition(&job, to).is_ok(),
                edges.contains(&(from, to)),
                "{from:?} -> {to:?}"
            );
        }
    }
    for remembered in [Some(Queued), Some(Downloading), Some(Extracting), None] {
        let mut job = Job::new(JobKind::Nzb, "paused");
        job.state = Paused;
        job.resume_state = remembered;
        for to in STATES {
            let legal =
                to == Cancelled || (matches!(to, Queued | Downloading) && remembered == Some(to));
            assert_eq!(
                validate_transition(&job, to).is_ok(),
                legal,
                "paused({remembered:?}) -> {to:?}"
            );
        }
    }
    for code in [
        FailureCode::NoMatch,
        FailureCode::Other,
        FailureCode::PathUnmapped,
    ] {
        let mut job = Job::new(JobKind::Nzb, "manual import");
        job.state = Failed;
        job.failure = Some(failure(code));
        assert_eq!(
            validate_transition(&job, ReadyForImport).is_ok(),
            code == FailureCode::NoMatch
        );
    }
}

#[test]
fn job_ids_are_uuidv7_and_parse_without_accepting_other_versions() {
    let first = JobId::new();
    let second = JobId::new();
    assert_eq!(first.as_uuid().get_version_num(), 7);
    assert!(first < second);
    assert_eq!(first.to_string().parse::<JobId>().unwrap(), first);
    assert!(
        "00000000-0000-4000-8000-000000000000"
            .parse::<JobId>()
            .is_err()
    );
    assert!("invalid".parse::<JobId>().is_err());
}

#[test]
fn restart_recovery_covers_every_state_files_and_resume_hints() {
    use JobState::*;
    for state in STATES {
        for files_exist in [false, true] {
            let mut job = Job::new(JobKind::Nzb, "restart");
            job.state = state;
            job.resume_hint = Some(CompletedStage::Downloading);
            if state == Paused {
                job.resume_state = Some(Downloading);
            }
            job.progress = Progress {
                speed_bps: 100,
                eta_secs: Some(1),
                stage_fraction: Some(0.8),
                ..Progress::default()
            };
            let expected = match state {
                Downloading => Queued,
                Verifying | Repairing | Extracting if !files_exist => Queued,
                Importing => ReadyForImport,
                state => state,
            };
            let recovery = recover_after_restart(&mut job, files_exist);
            assert_eq!(
                (recovery.from, recovery.to),
                (state, expected),
                "{state:?}, files={files_exist}"
            );
            assert_eq!(job.state, expected);
            assert_eq!(recovery.resume_hint, Some(CompletedStage::Downloading));
            assert_eq!(job.resume_hint, recovery.resume_hint);
            assert_eq!(
                job.resume_state,
                if state == Paused {
                    Some(Downloading)
                } else {
                    None
                }
            );
            assert_eq!(job.progress.speed_bps, 0);
            assert_eq!(job.progress.eta_secs, None);
            assert_eq!(job.progress.stage_fraction, None);
        }
    }
}

#[test]
fn core_events_convert_to_wasm_wire_dtos() {
    let id = JobId::new();
    for kind in [
        JobEventKind::Created,
        JobEventKind::StateChanged {
            from: JobState::Verifying,
            to: JobState::ReadyForImport,
        },
        JobEventKind::Progress {
            progress: Progress {
                articles_missing: 4,
                stage_fraction: Some(0.5),
                ..Progress::default()
            },
        },
        JobEventKind::Message {
            message: "hello".into(),
        },
    ] {
        let wire = reelarr_types::JobEvent::from(JobEvent {
            job_id: id,
            seq: 7,
            at: 100,
            kind,
        });
        assert_eq!(wire.job_id, id.to_string());
        assert_eq!(wire.seq, 7);
        let json = serde_json::to_string(&wire).unwrap();
        assert_eq!(
            serde_json::from_str::<reelarr_types::JobEvent>(&json).unwrap(),
            wire
        );
    }
    for state in STATES {
        let _: reelarr_types::JobState = state.into();
    }
}

#[test]
fn job_snapshot_converts_without_executor_details() {
    let mut job = Job::new(JobKind::Nzb, "Movie (2024)");
    job.priority = Priority::High;
    job.queue_position = 4;
    job.state = JobState::Failed;
    job.failure = Some(Failure {
        code: FailureCode::DestinationConflict,
        message: "exists".into(),
        retryable: false,
    });
    job.resume_hint = Some(CompletedStage::Downloading);
    job.last_event_seq = 12;
    let wire = reelarr_types::JobSnapshot::from(job.clone());
    assert_eq!(wire.id, job.id.to_string());
    assert_eq!(wire.priority, reelarr_types::Priority::High);
    assert_eq!(wire.queue_position, 4);
    assert_eq!(wire.state, reelarr_types::JobState::Failed);
    assert_eq!(
        wire.failure.unwrap().code,
        reelarr_types::FailureCode::DestinationConflict
    );
    assert_eq!(wire.last_event_seq, 12);
    let json = serde_json::to_value(reelarr_types::JobSnapshot::from(job)).unwrap();
    assert!(json.get("resume_hint").is_none());
    for priority in [
        Priority::Force,
        Priority::High,
        Priority::Normal,
        Priority::Low,
    ] {
        let wire: reelarr_types::Priority = priority.into();
        assert_eq!(Priority::from(wire), priority);
    }
}
