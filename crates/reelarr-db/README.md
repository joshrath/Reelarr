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

## Indexers and secrets

`SqliteIndexerStore::new(&db, Arc<dyn SecretCipher>)` implements the existing
`reelarr_indexer::IndexerStore`, validates before every upsert, and uses single
SQL statements for atomic CRUD. Non-secret settings are versioned JSON (including
nanosecond-precision durations); `extra_download_hosts` is its own ordered JSON
array. Migration 0007 creates the indexer table.

The default `secrets::XChaChaSecretCipher` encrypts API keys, with a versioned
ciphertext, per-write nonce and record/field-bound associated data. No API keys
are serialized into the settings. Encrypting indexer API keys and NNTP passwords
with XChaCha20-Poly1305 is the decided policy; `SecretCipher` stays an internal
abstraction. The API never returns secrets and logs never show them. The store does not itself load
keys or change server startup: callers initialize the cipher once at startup.
See [deployment](../../docs/deployment.md#indexer-secret-storage-store-integration-70)
for environment/key-file configuration and backup/recovery implications.

`IndexerStore` reads are internal and include `SecretString` for building clients.
Use `list_summaries` / `get_summary` for API-facing reads, never the full config.
They do not decrypt keys, and report `has_api_key` only. No HTTP routes are added.

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
in plain JSON. That is the decided policy: archive passwords stay unencrypted
inside the stored NZB document; encryption covers indexer API keys and NNTP
passwords only.

## NNTP provider settings (#76)

`SqliteNntpStore::new(&db, Arc<dyn SecretCipher>)` stores the single NNTP
provider (migration 0009, one row with `id = 1`). Non-secret settings are
versioned JSON; the AUTHINFO password is encrypted with the same
`SecretCipher` policy as indexer API keys, bound to its record/field as
associated data, and is `NULL` for providers without login. Statements are
string literals (no `format!` SQL).

- `get()` is the API-safe read: it never selects or decrypts the password and
  reports `has_password`.
- `save(&NntpServerUpdate)` validates (implicit TLS only, so port 119 is
  rejected; valid host; 1–100 connections; 1–300 s timeouts; 0–10 retries;
  printable non-whitespace AUTHINFO tokens; username and password together),
  then encrypts and upserts inside one `BEGIN IMMEDIATE` transaction. The
  password is `keep`, `set` or `clear`; `keep` is refused when the host
  changes.
- `resolve(&NntpServerUpdate)` applies an update to the stored row exactly as
  `save` would, without writing, for test-connection.
- `load()` decrypts for trusted pool construction. A wrong key, moved
  ciphertext or out-of-band invalid settings fail closed as `Store`.

Errors are static text; no SQL, bind values or credentials are retained.
