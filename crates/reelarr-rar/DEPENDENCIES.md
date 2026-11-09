# Dependency audit

Checked 2026-11-08 PST using published Cargo metadata (`cargo info`,
`cargo metadata --locked --format-version 1`). Exact versions are in Cargo.lock.
All license expressions below provide GPL-3.0-compatible permissive terms.
No RAR engine dependency, C archive implementation, binary tool, FFI extractor
or build script invoking an extractor is added. Existing workspace platform
syscall/runtime dependencies remain.

| Package | Version | Declared license | Scope |
|---|---|---|---|
| ambient-authority | 0.0.2 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | new transitive |
| arrayvec | 0.7.8 | MIT OR Apache-2.0 | new transitive |
| blake2s_simd | 1.0.5 | MIT | direct |
| cap-primitives | 4.0.3 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | new transitive |
| cap-std | 4.0.3 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | direct |
| constant_time_eq | 0.4.2 | CC0-1.0 OR MIT-0 OR Apache-2.0 | new transitive |
| crc32fast | 1.5.4 | MIT OR Apache-2.0 | direct |
| fs-set-times | 0.20.3 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | new transitive |
| io-extras | 0.19.0 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | new transitive |
| io-lifetimes | 2.0.4 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | new transitive |
| io-lifetimes | 3.0.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | new transitive |
| maybe-owned | 0.3.4 | MIT OR Apache-2.0 | new transitive |
| rustix-linux-procfs | 0.1.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | new transitive |
| tempfile | 3.27.0 | MIT OR Apache-2.0 | direct |
| thiserror | 2.0.21 | MIT OR Apache-2.0 | direct |
| unicode-normalization | 0.1.26 | (MIT OR Apache-2.0) AND Unicode-3.0 | direct |
| winx | 0.36.4 | Apache-2.0 WITH LLVM-exception | new transitive |

`blake2s_simd` default `std` feature uses Rust hash/SIMD intrinsics, not an
assembly/C build. `cap-std` default features: capability-relative filesystem
operations. `tempfile` manages private staging, now a runtime dependency of this
crate (already present elsewhere). `unicode-normalization` tables use Unicode-3.0
terms; code also offers MIT/Apache-2.0. The target-specific `winx` expression
includes the LLVM exception (permissive, GPL-compatible), not a RAR license.
No unsupported claim of a full legal or security audit is made.
