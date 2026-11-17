# reelarr-par2

Clean-room PAR2 2.0 **verification and repair** for Reelarr.

```rust,ignore
let report = reelarr_par2::verify_dir(job_dir, &Limits::default(), &cancel, &mut |p| { /* progress */ })?;
match report.primary().map(|s| s.outcome) {
    Some(Outcome::Valid) => {}
    Some(Outcome::Repairable { needed, available }) => {
        let _ = reelarr_par2::repair_dir(job_dir, &Limits::default(), &cancel, &mut |p| {})?;
    }
    Some(Outcome::Insufficient { needed, available }) => {}
    Some(Outcome::Incomplete) | None => {}
}
```

- Reads the regular files directly inside one directory through a cap-std
  `Dir`: no recursion, symlinks ignored, nothing outside opened. Verify writes
  nothing; repair writes only replaced protected files, via a temp file and
  rename so hard-linked view entries are not written through.
- PAR2 files are found by `.par2` extension or packet magic, so obfuscated
  names work. Every packet is hostile: length, alignment and packet MD5 are
  checked, set and file IDs are recomputed, budgets in `Limits` bound memory and
  work, and a bad packet only triggers a resync to the next magic.
- Each protected file is matched by name (last path component only) or, when
  renamed or obfuscated, by size plus 16 KiB MD5. The rename is reported in
  `FileReport::found_as`, never applied.
- Intact files cost one MD5 pass. Damaged files get a second, per-slice MD5 and
  CRC32 pass that lists the bad slice indices.
- `Report::primary()` picks the set that matches the most files when a folder
  holds several.

## Repair limits

- Repair streams. Recovery data is read from the `.par2` files by offset, and
  present slices are read one 64 KiB chunk at a time into one accumulator per
  missing slice. Peak working memory is `repair_memory_bytes(needed, chunk)`:
  `(needed + 2) × chunk` plus three `needed × needed` matrices of 16-bit words.
  It does not grow with slice size, set size, or how many volumes or sets are
  on disk. `Limits::max_repair_bytes` (default 512 MiB, about 9,000 missing
  slices) refuses a repair that would exceed it before any work starts.
- Only the primary set is repaired, and only `needed` recovery rows are used.
  If that matrix is singular, a few other windows of rows are tried.
- Every rebuilt file must match its FileDesc MD5 before anything is renamed
  into place; a file without a description is refused. Repair runs on one
  thread and is synchronous; callers run it under `spawn_blocking` and stop it
  with the cancel flag, which is checked between chunks and slices.
- Cost is roughly `slice_size × (present + needed) × needed` field
  multiplications plus a full read of the rebuilt files for the MD5 check.
  Faster multiplication tables and threads are a later optimization.

Fuzzing: `cd fuzz && cargo +nightly fuzz run packets` (seed with
`tests/fixtures/*/*.par2`). A bounded deterministic mutation test runs in
`cargo test`.
