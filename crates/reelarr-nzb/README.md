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
Tokio 1.54.1 is an existing workspace dependency (MIT), also used by NNTP.
The derive graph reuses proc-macro2/quote/syn/unicode-ident already in the lockfile
(MIT OR Apache-2.0; unicode-ident additionally requires Unicode-3.0). These license options
are compatible with GPL-3.0-or-later. The parser alone adds no network/database
dependency; NNTP dependencies are documented below.

Run `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`
and `cargo test --workspace` from the workspace root.

## NNTP transport (#12)

`nntp::Pool` is a lazy single-provider pool using verified implicit TLS (port
563 by default), SNI and the OS trust store. `with_root_certificates` supports
an explicit private CA without weakening hostname/chain verification. There is
no plaintext mode, insecure certificate switch, STARTTLS or OpenSSL dependency.
The transitive `openssl-probe` only locates OS certificate files; it does not
link to OpenSSL.

```rust,no_run
use reelarr_nzb::{Segment, nntp::{Pool, ServerConfig, Credentials, Secret}};

# async fn download(segment: &Segment) -> Result<(), reelarr_nzb::nntp::Error> {
let mut config = ServerConfig::new("news.example.org");
// Obtain these from the application's secret storage, not source code/logs.
config.credentials = Some(Credentials {
    username: Secret::new("username-from-secret-store"),
    password: Secret::new("password-from-secret-store"),
});
config.max_connections = 8;
let pool = Pool::new(config)?; // Loads native roots synchronously, once at setup.
let body = pool.body(segment).await?;
// Pass body.as_bytes() to #13's decoder as BodyEncoding::DotStuffed.
// NNTP retains dot-stuffing and CRLFs, strips the status and lone-dot terminator.
// Never feed this raw wire representation as BodyEncoding::Unstuffed.
assert!(body.as_bytes().len() <= 8 * 1024 * 1024);
pool.shutdown().await; // Optional best-effort QUIT; dropping also closes sockets.
# Ok(()) }
```

BODY and optional STAT use the parser's existing `Segment` locator, revalidated
at the command boundary (public fields can bypass XML validation). No GROUP or
article-number state is required. AUTHINFO credentials use printable ASCII
without whitespace, within the RFC 512-byte command bound; nonportable
whitespace/Unicode tokens are rejected instead of silently normalized. Username
and password are both redacted in Debug/Display. Secrets are not serialized or
zeroized; secret persistence and memory-dump protection are outside this module.
Do not log the original string passed to `Secret::new`.

Defaults: 8 connections; 8 MiB complete wire BODY; 8192-byte physical BODY lines
including CRLF; 512-byte status lines. Bodies are buffered, not streamed, so
budget memory for the configured connection count and article ceiling (plus
line/TLS buffers). NZB declared byte counts never size allocations. The line
reader enforces its cap before appending any untrusted data. A whole operation
is bounded to 90s including pool waits, connection/auth, all retries and backoff;
slot acquisition defaults to 10s, connect/TLS to 15s, and each physical line or
write to 15s. Limits and timeouts are configurable within documented ceilings.

Only complete successes and complete 430 replies return sockets to the pool.
Cancelling a future, timing out, malformed responses, limits and truncated
bodies all discard the socket and release capacity. Authentication failures,
430, TLS validation errors and protocol errors never retry. Transport failures,
incomplete replies, connect/read/write timeouts, and 400/403 responses get at
most two additional attempts by default, with capped exponential backoff and
0–25% jitter. Final errors remain typed, without server text or command values.
A retry restarts the BODY, never exposes a partial result to the worker.
Shutdown wakes waiting callers and rejects new work; already-running operations
are not cancelled and their sockets will not be reused.

Scope: transport only. Core worker/lifecycle wiring, durable checkpoints,
multi-provider priority/fill, SASL, pipelining, ARTICLE compatibility fallback
and NNTP compression remain separate work. No real-provider credentials or
network access are needed for tests.

See [the NNTP source/license ledger](../../docs/nntp.md) for provenance and
resolved dependency licenses.

## Streaming yEnc and disk assembly (#13)

`yenc::Decoder<W: Write>` accepts arbitrary byte chunks with `push` and requires
`finish` before output is trusted. It buffers at most one encoded line plus a
decoded line, preserves chunk-split escapes, accepts CRLF/LF, and rejects a
dangling escape at a physical line boundary. `BodyEncoding` explicitly selects
wire dot-stuffing or already-unstuffed BODY input, preventing double unstuffing.
Supply only one attachment per decoder; surrounding prose is ignored but counted
against the input/line caps. UTF-8 filenames are required; legacy non-UTF-8 names
are unsupported and fail clearly. Safe names with spaces are supported.

Default `DecodeLimits`: 8192 physical line bytes, 128 MiB article input, 64 MiB
decoded part, 1 TiB logical file. These are configurable *ceilings*, not quotas:
production workers must lower them to their disk budget, bound concurrent jobs,
and impose network deadlines in #12. An escape pair may make a data line one
byte longer than its advertised `line`; wire dot-stuffing and CR count against
the independent physical line cap. CRC32 is corruption detection, not security.
Single-part checksums are optional under the spec; multipart `pcrc32` is required.
Every advertised checksum is verified, including whole-file CRC in disk order.
The completion receipt explicitly reports when no whole-file checksum was given.

`assembly::Assembler::new(job_dir, &NzbFile, limits)` reuses the parsed segment
list, decodes each article to one temporary disk file, then copies validated
bytes to the declared offset in a sparse output. Overlap checks use ordered
interval neighbors (not quadratic scans); metadata scales only with the capped
NZB segment count. Missing numbers and holes prevent `finish`. A corrupt part
may be retried without checkpointing it. Conflicting filenames, sizes, counts,
part numbers or whole-file CRC declarations are rejected. NZB `Segment::bytes`
is an encoded article estimate and is deliberately not treated as decoded size.

The caller must supply a private job directory and keep it free of concurrent
untrusted filesystem mutation. A fresh mode-0700 child directory and `create_new`
files prevent pre-existing symlinks/files from being followed or overwritten;
unsafe basenames are rejected, not rewritten. `.stage` is additionally reserved
for internal temporary storage. Output is retained at `<job>/<private>/<name>`;
the job owns cleanup after import. Ordinary failure/drop removes unfinished
files best-effort; process crashes can leave artifacts for recovery to reconcile.
Directory fsync currently targets Unix/Linux (the Docker target).

Assembly is blocking: run the **entire** assembler operation in
`tokio::task::spawn_blocking` behind the worker's concurrency limit. After
`finish` fsyncs the output and directory entries, `CompletedFile::checkpoint`
can mark all matching `NzbStore` segments in one atomic batch. Partial progress
is exposed in memory, but is deliberately not durably checkpointed by this API;
crash-safe per-part reconciliation/rehydration remains a worker integration task.
There is no separate state database/file, NNTP connection, or core event policy.

### Clean-room source / fixture ledger

- Specification: Juergen Helbing, **yEncode 1.3 draft**, 2002-03-05
  (<http://www.yenc.org/yenc-draft.1.3.txt>), read via its public protocol-text
  mirror <https://yence.sourceforge.net/docs/protocol/version1_3_draft.html>.
  The canonical site was unavailable during implementation. Only the protocol
  document informed the implementation; no reference-app/codec source was used.
- All yEnc vectors originate from the tiny test-only encoder in `tests/yenc.rs`,
  independently written from the spec's +42 / escape +64 arithmetic. No imported
  fixtures, binaries, proprietary encoders or third-party source translations.
- New dependency: **crc32fast 1.5.4, MIT OR Apache-2.0**, verified by `cargo info`
  and its distributed manifest/licenses. It uses existing **cfg-if 1.0.5,
  MIT OR Apache-2.0**. Both license options are GPL-3.0-or-later compatible.
  No new testing dependency: deterministic seeded round trips and mutation tests
  provide fuzz-style coverage without adding proptest.
