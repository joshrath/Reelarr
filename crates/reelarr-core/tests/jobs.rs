use std::{sync::Arc, time::Duration};

use reelarr_core::*;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};

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

fn failure(code: FailureCode) -> Failure {
    Failure {
        code,
        message: "independent test fixture".into(),
        retryable: true,
    }
}

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

#[tokio::test]
async fn pause_resume_and_attempts_remember_the_exact_state() {
    for downloading in [false, true] {
        let store = InMemoryJobStore::default();
        let job = store.insert(Job::new(JobKind::Nzb, "pause")).await.unwrap();
        if downloading {
            store
                .transition(job.id, JobState::Downloading, None)
                .await
                .unwrap();
        }
        let before = store.get(job.id).await.unwrap();
        let paused = store
            .transition(job.id, JobState::Paused, None)
            .await
            .unwrap();
        assert_eq!(paused.resume_state, Some(before.state));
        let wrong = if downloading {
            JobState::Queued
        } else {
            JobState::Downloading
        };
        assert!(store.transition(job.id, wrong, None).await.is_err());
        let resumed = store.resume(job.id).await.unwrap();
        assert_eq!(resumed.state, before.state);
        assert_eq!(resumed.attempts, before.attempts);
        assert_eq!(resumed.resume_state, None);
        assert!(store.resume(job.id).await.is_err());
    }
}

#[tokio::test]
async fn failed_transition_is_atomic_and_retry_clears_failure() {
    let store = InMemoryJobStore::default();
    let created = store
        .insert(Job::new(JobKind::Nzb, "atomic"))
        .await
        .unwrap();
    let mut receiver = store.events().subscribe();
    assert!(
        store
            .transition(
                created.id,
                JobState::Failed,
                Some(failure(FailureCode::Other))
            )
            .await
            .is_err()
    );
    assert_eq!(store.get(created.id).await.unwrap(), created);
    assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
    let downloading = store
        .transition(created.id, JobState::Downloading, None)
        .await
        .unwrap();
    assert!(
        store
            .transition(created.id, JobState::Failed, None)
            .await
            .is_err()
    );
    assert!(
        store
            .transition(
                created.id,
                JobState::Verifying,
                Some(failure(FailureCode::Other))
            )
            .await
            .is_err()
    );
    assert_eq!(store.get(created.id).await.unwrap(), downloading);
    let codes = [
        FailureCode::MissingArticles,
        FailureCode::RepairFailed,
        FailureCode::ExtractFailed,
        FailureCode::PasswordRequired,
        FailureCode::DiskFull,
        FailureCode::ServerAuth,
        FailureCode::InvalidNzb,
        FailureCode::Duplicate,
        FailureCode::NoMatch,
        FailureCode::PathUnmapped,
        FailureCode::Other,
    ];
    for (index, code) in codes.into_iter().enumerate() {
        let failed = store
            .transition(created.id, JobState::Failed, Some(failure(code)))
            .await
            .unwrap();
        assert_eq!(failed.failure.unwrap().code, code);
        assert!(
            store
                .transition(created.id, JobState::Cancelled, None)
                .await
                .is_err()
        );
        let retry = store
            .transition(created.id, JobState::Queued, None)
            .await
            .unwrap();
        assert!(retry.failure.is_none());
        let active = store
            .transition(created.id, JobState::Downloading, None)
            .await
            .unwrap();
        assert_eq!(active.attempts as usize, index + 2);
    }
    store
        .transition(created.id, JobState::Queued, None)
        .await
        .unwrap(); // all servers unreachable
    assert_eq!(store.get(created.id).await.unwrap().failure, None);
}

#[tokio::test]
async fn priority_and_drag_queue_position_control_order() {
    let store = InMemoryJobStore::default();
    let mut ids = Vec::new();
    for (priority, position) in [
        (Priority::Low, 0),
        (Priority::Normal, 1),
        (Priority::High, 2),
        (Priority::Force, 99),
        (Priority::Normal, 0),
    ] {
        let mut job = Job::new(JobKind::Nzb, "order");
        job.priority = priority;
        job.queue_position = position;
        ids.push(store.insert(job).await.unwrap().id);
    }
    assert_eq!(
        store
            .list()
            .await
            .unwrap()
            .iter()
            .map(|j| j.id)
            .collect::<Vec<_>>(),
        [ids[3], ids[2], ids[4], ids[1], ids[0]]
    );
    store
        .set_queue_order(ids[1], Priority::Force, 0)
        .await
        .unwrap();
    assert_eq!(store.list().await.unwrap()[0].id, ids[1]);
    assert!(matches!(
        store.insert(store.get(ids[0]).await.unwrap()).await,
        Err(JobStoreError::InvalidNewJob)
    ));
    let missing = JobId::new();
    assert!(matches!(store.get(missing).await, Err(JobStoreError::NotFound(id)) if id == missing));
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

#[tokio::test]
async fn completed_stage_hints_and_store_recovery_are_retained() {
    let store = InMemoryJobStore::default();
    let job = store.insert(Job::new(JobKind::Nzb, "hint")).await.unwrap();
    for (state, hint) in [
        (JobState::Downloading, None),
        (JobState::Verifying, Some(CompletedStage::Downloading)),
        (JobState::Repairing, Some(CompletedStage::Verifying)),
        (JobState::Extracting, Some(CompletedStage::Repairing)),
    ] {
        let job = store.transition(job.id, state, None).await.unwrap();
        assert_eq!(job.resume_hint, hint);
        if state != JobState::Downloading {
            let recovered = store.recover(job.id, true).await.unwrap();
            assert_eq!(recovered.state, state);
            assert_eq!(recovered.resume_hint, hint);
            assert!(recovered.last_event_seq > job.last_event_seq);
        }
    }
    let recovered = store.recover(job.id, false).await.unwrap();
    assert_eq!(recovered.state, JobState::Queued);
    assert_eq!(recovered.resume_hint, Some(CompletedStage::Repairing));
}

#[tokio::test]
async fn progress_is_cached_and_throttled_while_bad_values_are_rejected() {
    let store = InMemoryJobStore::new(EventHub::new(8), Duration::MAX);
    let job = store
        .insert(Job::new(JobKind::Nzb, "progress"))
        .await
        .unwrap();
    let mut receiver = store.events().subscribe();
    let first = Progress {
        bytes_done: 1,
        bytes_total: 2,
        stage_fraction: Some(0.25),
        articles_missing: 2,
        ..Progress::default()
    };
    let published = store.update_progress(job.id, first.clone()).await.unwrap();
    assert!(
        matches!(receiver.try_recv().unwrap().kind, JobEventKind::Progress { progress } if progress == first)
    );
    let next = Progress {
        bytes_done: 2,
        stage_fraction: Some(0.5),
        ..first
    };
    let cached = store.update_progress(job.id, next.clone()).await.unwrap();
    assert_eq!(cached.last_event_seq, published.last_event_seq);
    assert_eq!(store.get(job.id).await.unwrap().progress, next);
    assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
    for fraction in [f32::NAN, f32::INFINITY, -0.1, 1.1] {
        assert!(matches!(
            store
                .update_progress(
                    job.id,
                    Progress {
                        stage_fraction: Some(fraction),
                        ..Progress::default()
                    }
                )
                .await,
            Err(JobStoreError::InvalidProgress)
        ));
    }
    assert!(matches!(
        store
            .update_progress(
                job.id,
                Progress {
                    bytes_done: 2,
                    bytes_total: 1,
                    ..Progress::default()
                }
            )
            .await,
        Err(JobStoreError::InvalidProgress)
    ));
    store
        .transition(job.id, JobState::Cancelled, None)
        .await
        .unwrap();
    assert!(matches!(
        store.update_progress(job.id, Progress::default()).await,
        Err(JobStoreError::TerminalProgress)
    ));
}

#[tokio::test]
async fn broadcast_lag_requires_snapshot_and_watermark_resync() {
    let store = InMemoryJobStore::new(EventHub::new(2), Duration::ZERO);
    // Subscription comes before the snapshot to avoid a race/gap.
    let mut receiver = store.events().subscribe();
    let job = store.insert(Job::new(JobKind::Nzb, "lag")).await.unwrap();
    store
        .transition(job.id, JobState::Downloading, None)
        .await
        .unwrap();
    store
        .update_progress(
            job.id,
            Progress {
                bytes_done: 1,
                ..Progress::default()
            },
        )
        .await
        .unwrap();
    store.message(job.id, "a message".into()).await.unwrap();
    assert!(matches!(receiver.recv().await, Err(RecvError::Lagged(2))));
    let snapshot = store.get(job.id).await.unwrap();
    assert_eq!(snapshot.last_event_seq, 4);
    assert_eq!(snapshot.progress.bytes_done, 1);
    while let Ok(stale) = receiver.try_recv() {
        assert!(stale.seq <= snapshot.last_event_seq);
    }
    store
        .transition(job.id, JobState::Queued, None)
        .await
        .unwrap();
    assert_eq!(
        receiver.recv().await.unwrap().seq,
        snapshot.last_event_seq + 1
    );
}

#[tokio::test]
async fn sequences_are_per_job_monotonic_even_with_concurrent_writers() {
    let store = Arc::new(InMemoryJobStore::new(EventHub::new(256), Duration::ZERO));
    let mut receiver = store.events().subscribe();
    let first = store.insert(Job::new(JobKind::Nzb, "first")).await.unwrap();
    let second = store
        .insert(Job::new(JobKind::Nzb, "second"))
        .await
        .unwrap();
    assert_eq!(
        (
            receiver.recv().await.unwrap().seq,
            receiver.recv().await.unwrap().seq
        ),
        (1, 1)
    );
    let mut tasks = Vec::new();
    for i in 0..100 {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            store.message(first.id, format!("{i}")).await.unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    for seq in 2..=101 {
        assert_eq!(receiver.recv().await.unwrap().seq, seq);
    }
    assert_eq!(store.get(first.id).await.unwrap().last_event_seq, 101);
    assert_eq!(store.get(second.id).await.unwrap().last_event_seq, 1);
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
