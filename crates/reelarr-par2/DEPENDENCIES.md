# Dependency audit

Checked 2026-11-09 PST with `cargo info` and Cargo.lock. All licenses are
GPL-3.0-compatible permissive terms. No new transitive crates beyond those
already in the workspace lockfile.

| Package | Version | Declared license | Scope |
|---|---|---|---|
| cap-std | 4.0.3 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | direct (already used by reelarr-rar) |
| crc32fast | 1.5.x | MIT OR Apache-2.0 | direct (already in workspace) |
| md-5 | 0.10.6 | MIT OR Apache-2.0 | direct (already in lockfile via sqlx) |
| thiserror | 2.x | MIT OR Apache-2.0 | direct (already in workspace) |
| tempfile | 3.x | MIT OR Apache-2.0 | dev |

Fuzz harness only (`fuzz/`, its own workspace, not built by CI or shipped):
libfuzzer-sys 0.4.13, (MIT OR Apache-2.0) AND NCSA. It needs nightly,
cargo-fuzz and a C++ compiler.
