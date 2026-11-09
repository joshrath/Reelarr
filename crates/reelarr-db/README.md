# Database foundation

`Database::connect(path)` creates the SQLite file if needed, configures every pool
connection with WAL, foreign keys and a five-second busy timeout, then runs the
embedded forward-only migrations. The parent directory must already exist. A
migration failure closes the pool and returns an error; do not serve requests
until connection succeeds. Clones of `Database` share the same pool.

The server creates `$REELARR_DATA_DIR` (default `/config`) and opens `reelarr.db`
before binding its listener. `/health` remains a liveness endpoint at the root;
feature API routes attach to the stateful router nested at `/api/v1`.

`reelarr-db` owns the single database and **all** migrations. Add numbered `.sql`
files under `migrations/`; never change an already applied migration. `build.rs`
ensures newly added migration files are picked up by incremental builds. No live
DB or query metadata is needed to compile: queries use runtime `sqlx::query`.

`SettingsStore` in `reelarr-core` is async and object-safe, with an in-memory
implementation for domain tests. `SqliteSettingsStore::new(&db)` shares the pool.
Values are opaque strings, missing keys yield `None`, and `set` atomically
upserts the value and its Unix timestamp in whole seconds (`updated_at`).
## Durable jobs (#29)

`SqliteJobStore::new(&db)` implements core's `JobStore` using the same database.
`AppState.jobs` shares it, and server startup runs `recover_on_startup()` after
migrations and before binding. Every non-progress Job field is stored by
`0002_jobs.sql`, including pause state, failure, attempts, resume hint and event
sequence allocation. State and `(priority, queue_position)` indexes support queue
inspection. Unsigned u64 queue positions/sequence values are fixed-width big-endian
BLOBs: SQLite's signed INTEGER range cannot truncate valid domain values, and BLOB
sort order equals unsigned numeric order. Priority is 0=Force through 3=Low.

Transitions acquire `BEGIN IMMEDIATE` before loading the current row, then call
core's `apply_transition` inside the transaction. Validation errors roll back
without changing the row or publishing. Successful commits are followed by cache
updates/event fanout while holding the shared runtime lock. Repeated store
constructors and Database clones share one progress cache/hub; the first
constructor selects the hub/throttle options. There is one live application
Database/runtime, not a cross-process event bus. Independently opened connections
still serialize their lifecycle writes safely with SQLite.

Progress values are **never persisted**, neither per update nor per event. Only
in-memory snapshots keep them, and event fanout is throttled as in core's store.
To prevent sequence reuse after a hard crash without a write for every progress
event, the store durably leases blocks of 256 sequence values. `event_seq` is the
last durable lifecycle/message sequence; `seq_reserved` is the crash-safe upper
watermark. Warm snapshots expose the last actually emitted sequence. After reopen,
cold snapshots skip the rest of the old lease and expose its upper watermark; the
next event is larger. Gaps are expected, and consumers must refetch snapshots on
lag/gaps, not assume event-log replay. Leases use checked/saturating allocation;
exhaustion returns SequenceExhausted rather than wrapping.

## Startup artifact registration

Core Job intentionally contains no executor output path. Once the executor reports
its **actual local artifact path**, the server/queue coordinator calls
`SqliteJobStore::set_recovery_path(job_id, Some(path))` (an absolute UTF-8 path).
This is durable executor metadata, not a segment/progress checkpoint. The NZB
crate itself does not depend on the database adapter. Startup
checks the registered file or nonempty directory; missing/unregistered/empty paths
conservatively requeue Verifying/Repairing/Extracting. Existing artifacts retain
their own stage. Paths are never guessed from titles or deleted. NZB executors
must still reconcile/checksum files before trusting segment checkpoints.

Recovery delegates to core's recovery function for active jobs: Downloading ->
Queued, Importing -> ReadyForImport, and stage-specific file checks as above. Pause
state, failures and last-completed-stage hints survive. Terminal jobs are left
untouched. Filesystem/database/decode errors stop startup before the listener is
bound rather than silently discarding jobs.

All NZB document/segment storage remains behind reelarr-nzb's NzbStore. Its SQL
adapter/migrations belong here when implemented; this adapter does not persist
per-segment completion or introduce a second database.

## NZB store (#76)

`SqliteNzbStore` implements `reelarr_nzb::NzbStore` (migration 0008). The parsed
NZB is stored once as versioned JSON with its `movie_id`; completed segments are
one row each in `nzb_checkpoints`. A `checkpoint` batch is validated in full, then
applied in order in one `BEGIN IMMEDIATE` transaction, so it lands entirely or not
at all. `remove` is idempotent and takes the checkpoints with it.

The worker stores the NZB before inserting its job, so `nzb_documents` has no
foreign key to `jobs`. Deleting a job removes its NZB through a trigger, and
`SqliteNzbStore::open` (run at startup, before workers start) deletes NZBs whose
job row never landed. A later migration that rebuilds `jobs` must recreate the
`nzb_documents_job_deleted` trigger. Stored NZBs can include an archive password
in plain JSON.
