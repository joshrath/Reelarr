//! WASM-safe job wire DTOs only. Lifecycle logic and UUID generation live in core.
//! JSON uses snake_case states and internally tagged event kinds (`type`).

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub speed_bps: u64,
    pub eta_secs: Option<u64>,
    pub stage_fraction: Option<f32>,
    pub articles_missing: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobEvent {
    /// Canonical UUIDv7 string; the browser does not generate ids.
    pub job_id: String,
    pub seq: u64,
    /// Unix seconds (UTC).
    pub at: i64,
    pub kind: JobEventKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobEventKind {
    Created,
    StateChanged { from: JobState, to: JobState },
    Progress { progress: Progress },
    Message { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shape_round_trips() {
        let kinds = [
            JobEventKind::Created,
            JobEventKind::StateChanged {
                from: JobState::Extracting,
                to: JobState::ReadyForImport,
            },
            JobEventKind::Progress {
                progress: Progress {
                    stage_fraction: Some(0.5),
                    articles_missing: 3,
                    ..Progress::default()
                },
            },
            JobEventKind::Message {
                message: "refetch snapshot after lag".into(),
            },
        ];
        for kind in kinds {
            let event = JobEvent {
                job_id: "019a5e00-0000-7000-8000-000000000001".into(),
                seq: 42,
                at: 1700000000,
                kind,
            };
            let json = serde_json::to_value(&event).unwrap();
            assert!(json["kind"]["type"].is_string());
            if let JobEventKind::StateChanged { .. } = &event.kind {
                assert_eq!(json["kind"]["to"], "ready_for_import");
            }
            assert_eq!(serde_json::from_value::<JobEvent>(json).unwrap(), event);
        }
    }
}
