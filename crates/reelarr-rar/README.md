# reelarr-rar

Original GPL-3.0-or-later, stored-only RAR4/RAR5 container reader. No external
extractor, compression engine or native archive library. [Source ledger](SOURCES.md),
[dependency audit](DEPENDENCIES.md), [fixtures](tests/fixtures/PROVENANCE.md).

```rust,no_run
use reelarr_rar::{VolumeSet, Limits};
use std::{path::{Path, PathBuf}, sync::atomic::AtomicBool};
let set = VolumeSet::open(&[
    PathBuf::from("download/movie.part1.rar"),
    PathBuf::from("download/movie.part2.rar"),
], Limits::default())?;
let verified = set.extract(Path::new("job/staging"), &AtomicBool::new(false), |p| {
    // Forward/throttle p.bytes_written and p.entries_done through core's API.
})?;
// Only now is this path a verified extraction; #14 owns final publication/import.
assert!(!verified.manifest().is_empty());
let path = verified.keep(); // Caller now owns cleanup, or omit to delete on drop.
# Ok::<(), reelarr_rar::Error>(())
```

## Support matrix

- Signature at offset zero; RAR4 stored 0x30, unpack version 20..50 (only
  stored semantics, **not** compressed algorithms), RAR5 stored method 0 with
  algorithm version 0 or 1. 64-bit declared sizes are parsed before budgeting.
- Explicitly ordered sets: `.partN.rar` / zero-padded `.partNN.rar`, starting
  at 1, or `.rar/.r00/.r01/.../.z99`. Filename stem, width, parent, sequence,
  first-volume/naming flags, RAR5 volume numbers and RAR4 end volume numbers are
  checked. No directory auto-discovery (the caller supplies the bounded set).
- Split files must continue as the first file in the next volume, with matching
  name, logical size, host, attributes and compression identity. Intermediate
  part digests cover each part; the last digest covers the entire logical file.
  Sets have no universal cryptographic archive ID: metadata/sequence and digest
  verification detect inconsistent sets, not malicious recombination with
  intentionally matching metadata and recomputed checksums.
- Standard CRC32, low-16 RAR4 header CRCs, full RAR5 header CRCs and BLAKE2sp.
  **All advertised file/part digests must match**. Each part and final logical
  file requires at least one usable digest; otherwise `MissingIntegrityChecksum`.
  Empty directories may omit a digest. CRC is accidental-integrity protection,
  not authentication. RAR4 split/end semantics qualified against the committed
  generated fixtures; older checksum/header variants are not claimed supported.
- Explicit directories; bounded service/optional metadata skipped without
  restoration. Quick-open caches are ignored in both parsing and extraction.
- ASCII RAR4 legacy names, documented RAR4 UTF-8 names without encoded pairs,
  portable RAR5 UTF-8 names. RAR4 backslashes normalize to separators; RAR5
  backslashes are conservatively rejected. No lossy filename conversion.

Compressed, solid, encrypted, SFX, unknown-size, file-version, mapped-Unicode,
encoded legacy-Unicode, link/redirection/device and unknown required semantics
produce typed errors. Legacy RAR4 embedded comments and multivolume headers
without first/end-number evidence are unsupported. Trailing zero padding is
allowed under the input budget; other trailing data is unsupported.

## Safety and integration contract

All payload is written through a `cap_std::fs::Dir` capability into a fresh
private (0700 on Unix) `TempDir` under the **canonicalized trusted output parent**.
Untrusted archive names cannot escape that directory; create-new files never
overwrite another entry. Symlinks/hardlinks/reparse points/devices are never
created. No permissions, owners, times, ACLs, alternate streams or service
payload are restored. No recursive extraction. Paths reject absolute/UNC/drive,
parent/current/empty components, controls/NUL/bidi overrides, ambiguous Unicode,
Windows reserved names, forbidden characters and trailing dots/spaces. A
Unicode-normalized lowercase namespace rejects duplicates, case aliases and
file/directory-prefix collisions before their writes.

Callers must keep source files and the output parent trusted against same-user
rename/replacement, retain failed input themselves, and never advertise staging
as ready before success. Staging is removed on error or cooperative cancellation;
success closes and syncs all payload writers before returning `Extraction`.
Dropping `Extraction` removes it unless `keep` transfers ownership. A process
crash can leave an orphan private `.reelarr-rar-*` directory: #14 must reap these
on startup. This primitive does not set ready markers, publish atomically into
the library, perform repair, or emit import events; #14 owns those actions.

`Limits` caps open volumes, header allocation, headers/work, explicit and implicit
entries, input including skipped streams/padding, logical per-file/total output,
path depth and path bytes. The output byte limit is a **payload disk budget**,
not an OS free-space reservation; I/O/ENOSPC propagates and staging is removed.
Memory is bounded by one header, two 64-KiB stream buffers at most, bounded hash
states and path/manifest maps. Metadata is bounded by entries × path depth × path
bytes. Choose smaller budgets for hostile inputs. Cancellation checks precede
headers and every <=64 KiB input read and payload chunk. It does not interrupt a
blocked filesystem call or a trusted caller's blocking progress callback. Run
on a blocking worker when integrating with async orchestration; no hidden tasks.

## Tests and development

`cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace`. Test builders are independently authored from public
notes; committed RAR 6.24-generated split fixtures supply an independent producer.
Known-answer BLAKE2sp vectors, empty/stream-boundary payloads, high-size fields,
corruption, unsupported features, unsafe paths/collisions, volume identity,
limits, cancellation/cleanup, every-byte truncation and deterministic mutations
exercise the failure paths. No fixture generator executes during builds/tests.
Production hardening should additionally run sustained coverage-guided fuzzing
and cross-platform/ARM64 validation; deterministic tests are not a security audit.
