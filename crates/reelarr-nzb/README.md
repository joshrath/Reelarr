# reelarr-nzb: parser and resume boundary

Clean-room implementation using the public NZB XML structure
(<https://www.newzbin.com/DTD/nzb/nzb-1.1.dtd>) and XML 1.0 rules;
all fixtures were authored for this crate. No reference-app source was used.

`parse(Read, ParseLimits)` reads XML events with quick-xml, not a document tree.
Default ceilings: 32 MiB total input, depth 32, 64 KiB per field, 10,000 files,
250,000 total segments. The byte ceiling applies to the reader, including malformed
or unterminated events, and bounds temporary XML event allocation. Field limits
are checked after decoding an event and across consecutive text/entity/CDATA
chunks; they do not impose a smaller preallocation limit on quick-xml itself.
Use a suitably small total byte cap for untrusted uploads. Parsing is synchronous;
server callers should use a bounded blocking worker, not block the async executor.

Supports UTF-8, the standard default NZB namespace (or unqualified elements),
head title/password/category, file subject/poster/date, groups, and contiguous
positive segment numbers with positive u64 byte counts. Segments are sorted by
number; file order is retained. Empty documents/files, unknown structure,
namespace rebinding, invalid message IDs, DTDs and custom entities are rejected.
Predefined XML entities and numeric character references are supported. Subjects
and metadata are not filesystem names. Password-bearing documents intentionally
have no Debug implementation; do not log or serialize them indiscriminately.

## Lifecycle and persistence integration

Core owns jobs, states, ordering, events and scheduling. This crate defines no
job state machine. `JobId(u128)` is a temporary opaque bridge: replace it with
`reelarr-core::JobId` after Core Lead's lifecycle PR lands. No ID generation is
performed here.

`NzbStore` is an async, Send-future, statically dispatched storage boundary.
`reelarr-db` must implement it in the single shared SQLite database, including
migrations; no SQL or private state file belongs here. Insert is non-overwriting,
load returns a consistent snapshot, and checkpoint batches are atomic and
idempotent. Documents are immutable to keep segment addresses stable. False
checkpoints support clearing entries that fail filesystem reconciliation.

The in-memory implementation is for tests only. Cloned handles model a worker
restart against an existing backend; they do **not** model process-persistent
storage. SQLite integration and true process/crash recovery await reelarr-db.
Persist segment completion only after verified bytes are durably written. On a
real restart, reconcile/checksum artifacts before trusting stored completion.
`StoredNzb::resume` validates the snapshot and reports pending addresses and
encoded article bytes done/total (not decoded media bytes).

## Dependencies / licenses

Exact versions verified through `cargo info`, package manifests and bundled
license files: quick-xml 0.42.0 (MIT), thiserror 2.0.21 and its derive helper
(MIT OR Apache-2.0). quick-xml's memchr dependency is 2.8.3 (MIT OR Unlicense).
Tokio 1.54.1 is an existing workspace dependency (MIT), used here only in tests.
The derive graph reuses proc-macro2/quote/syn/unicode-ident already in the lockfile
(MIT OR Apache-2.0; unicode-ident additionally requires Unicode-3.0). These license options
are compatible with GPL-3.0-or-later. No new native, network or database dependency.

Run `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`
and `cargo test --workspace` from the workspace root.
