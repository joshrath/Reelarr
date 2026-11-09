//! The same store contract runs against both adapters, including concurrency/lag.
use async_trait::async_trait;
use reelarr_core::*;
use std::{sync::Arc, time::Duration};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};

#[async_trait]
pub trait StoreFactory: Sync {
    async fn create(&self, events: EventHub, interval: Duration) -> Arc<dyn JobStore>;
}

fn failure(code: FailureCode) -> Failure {
    Failure {
        code,
        message: "independent test fixture".into(),
        retryable: true,
    }
}

async fn pause_resume_and_attempts_remember_the_exact_state(factory: &dyn StoreFactory) {
    for downloading in [false, true] {
        let store = factory
            .create(EventHub::default(), Duration::from_millis(250))
            .await;
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

async fn failed_transition_is_atomic_and_retry_clears_failure(factory: &dyn StoreFactory) {
    let store = factory
        .create(EventHub::default(), Duration::from_millis(250))
        .await;
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

async fn priority_and_drag_queue_position_control_order(factory: &dyn StoreFactory) {
    let store = factory
        .create(EventHub::default(), Duration::from_millis(250))
        .await;
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

async fn completed_stage_hints_and_store_recovery_are_retained(factory: &dyn StoreFactory) {
    let store = factory
        .create(EventHub::default(), Duration::from_millis(250))
        .await;
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

async fn progress_is_cached_and_throttled_while_bad_values_are_rejected(
    factory: &dyn StoreFactory,
) {
    let store = factory.create(EventHub::new(8), Duration::MAX).await;
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

async fn broadcast_lag_requires_snapshot_and_watermark_resync(factory: &dyn StoreFactory) {
    let store = factory.create(EventHub::new(2), Duration::ZERO).await;
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

async fn sequences_are_per_job_monotonic_even_with_concurrent_writers(factory: &dyn StoreFactory) {
    let store = factory.create(EventHub::new(256), Duration::ZERO).await;
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

pub async fn run(factory: &dyn StoreFactory) {
    pause_resume_and_attempts_remember_the_exact_state(factory).await;
    failed_transition_is_atomic_and_retry_clears_failure(factory).await;
    priority_and_drag_queue_position_control_order(factory).await;
    completed_stage_hints_and_store_recovery_are_retained(factory).await;
    progress_is_cached_and_throttled_while_bad_values_are_rejected(factory).await;
    broadcast_lag_requires_snapshot_and_watermark_resync(factory).await;
    sequences_are_per_job_monotonic_even_with_concurrent_writers(factory).await;
}
