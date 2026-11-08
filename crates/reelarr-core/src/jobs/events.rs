use tokio::sync::broadcast;

use super::{JobId, JobState, Progress};

#[derive(Clone, Debug, PartialEq)]
pub struct JobEvent {
    pub job_id: JobId,
    pub seq: u64,
    /// Unix seconds (UTC).
    pub at: i64,
    pub kind: JobEventKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum JobEventKind {
    Created,
    StateChanged { from: JobState, to: JobState },
    Progress { progress: Progress },
    Message { message: String },
}

/// Lossy UI/SSE fanout, not a work queue and never the import handoff.
/// Subscribe before fetching a snapshot. On Lagged or a seq gap refetch that job;
/// discard buffered events <= snapshot.last_event_seq, then resume newer events.
#[derive(Clone, Debug)]
pub struct EventHub {
    sender: broadcast::Sender<JobEvent>,
}

impl EventHub {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity.max(1));
        Self { sender }
    }
    pub fn subscribe(&self) -> broadcast::Receiver<JobEvent> {
        self.sender.subscribe()
    }

    /// Store adapters publish only AFTER successful mutations/commits, serialized per
    /// job with seq allocation. No subscribers is normal and must not fail a mutation.
    pub fn publish(&self, event: JobEvent) {
        let _ = self.sender.send(event);
    }
}

impl Default for EventHub {
    fn default() -> Self {
        Self::new(256)
    }
}
