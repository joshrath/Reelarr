# Core job contracts (#29)

This crate owns the job lifecycle. It does not execute NZBs or import media. This
PR implements the typed contracts and in-memory store; the **sqlx JobStore adapter
and job-table migration are a follow-up after #39 merges**. All persistence
implementations/migrations belong in `reelarr-db`.

## Executors and scheduling (#11)

- `JobId::new()` generates a UUIDv7 on the server; parsing rejects other versions.
- `JobKind::{Nzb, ExternalDownload}` identify the initial download backends.
- `Job::new` starts Queued, with Normal priority and queue position zero.
- `JobStore::transition` validates and atomically changes state. Failed transitions
  require a `Failure`, and non-failure transitions reject one. Invalid operations
  leave the snapshot and event sequence untouched.
- `Priority` sorts Force, High, Normal, Low; `list` sorts by priority, queue position,
  creation time, then id. `set_queue_order` supports drag reordering. Consumers can
  filter this list to active/queued jobs without losing the scheduling order.
- Pause only from Queued/Downloading. `resume` atomically restores that exact state;
  it does not increment attempts. Attempts increment when Queued starts Downloading.
- Failed -> Queued is an explicit retry, even if a failure was not automatically
  retryable (a user may have fixed the cause). Only Failed(NoMatch) may transition
  directly back to ReadyForImport for manual matching.
- `resume_hint` records the last fully completed processing stage. At startup,
  before workers run, call `recover` with file availability: Downloading -> Queued;
  Verifying/Repairing/Extracting stay at their stage if files exist, otherwise Queued;
  Importing -> ReadyForImport. Other states stay put. Hints are never discarded.
- NZB file lists, segments and per-segment completion stay in `reelarr-nzb`'s own
  `NzbStore`, keyed by JobId. Persist segment checkpoints in batches, not per event;
  its eventual sqlx adapter also lives in `reelarr-db`.

## Events and progress

`store.events().subscribe()` returns a Tokio broadcast receiver. Events are a
**lossy UI/SSE feed, not a work queue**. The store allocates per-job seq values
starting at 1, including when nobody is subscribed. Mutations and sends are
serialized so simultaneous writers cannot deliver out-of-order sequences for one
job. Different jobs have independent sequences.

Subscribe **before** fetching a snapshot. On `RecvError::Lagged` or a sequence gap,
refetch that job (`get`) or all affected jobs (`list`), discard buffered events with
`seq <= snapshot.last_event_seq`, then consume newer events. A missing receiver is
not an error. Future durable adapters must atomically persist state/ordering and
sequence allocation before publishing; sequence allocation must remain monotonic
across restarts independently of progress persistence.

Progress updates always replace the in-memory snapshot. Broadcasts are throttled
(default 250 ms per job); the next update after the interval publishes the newest
values, not a backlog. No timer/background worker is required. Fractions must be
finite and in [0, 1]; bytes_total=0 means unknown. Processing stages can use
stage_fraction, and articles_missing drives repair decisions. Terminal jobs reject
late progress. State changes are never throttled.

`reelarr-types::job` contains only WASM-safe serde DTO mirrors: JobState, Progress,
and the JobEvent wire shape. Convert core events with `reelarr_types::JobEvent::from`.
Job ids are canonical strings, states are snake_case, and event kind is tagged by
`kind.type` (`created`, `state_changed`, `progress`, `message`). Timestamps are Unix
seconds. No Tokio or UUID RNG dependency is added to the browser crate.

## Import and cleanup (#15, #21)

The executor calls `run_import` after repair/extraction reaches ReadyForImport.
It passes `CompletedDownload`, an `ImportHandler` implemented in reelarr-media, a
source-specific `CleanupRegistry`, and #21's explicit `PathMapping` result. The
orchestrator does not guess whether a remote path was mapped just because it
exists on disk. Unmapped or non-absolute paths fail with PathUnmapped **before**
calling the handler. It claims Importing first to stay inside the agreed graph.

Reelarr-initiated downloads must retain their MovieId in executor metadata. Only
external/manual downloads may omit it for parser matching. BuiltinNzb without a
movie id fails before import rather than silently falling back to matching.

Imported -> Completed; generic Failed -> Failed(Other) with the supplied reason and
retryability; NoMatch -> Failed(NoMatch), with manual re-entry to ReadyForImport.
The media handler owns media-file persistence and the returned MediaFileId; the
orchestrator only updates the job lifecycle.

Register `SourceCleanup` per `DownloadSource` (external client names are exact keys).
It is invoked once after Completed. Cleanup errors (including a missing handler)
are logged and emitted as Message events; completion is never undone. Implementors
must treat absent and partly empty source folders as success. Repeated or
simultaneous `run_import` calls cannot duplicate import/cleanup during a live run.
After a hard crash during Importing, import can be retried, so the media handler
must make its own file/database work retry-safe. Cleanup is best-effort: a crash
between completion and cleanup can skip cleanup, rather than endanger imported
files with an uncoordinated retry.

All code and fixtures are clean-room implementations. No code/tests from reference
applications are used.
