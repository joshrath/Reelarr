# reelarr-par2

Clean-room PAR2 2.0 **verification** for Reelarr (no repair yet; see #38).

```rust,ignore
let report = reelarr_par2::verify_dir(job_dir, &Limits::default(), &cancel, &mut |p| { /* progress */ })?;
match report.primary().map(|s| s.outcome) {
    Some(Outcome::Valid) => {}
    Some(Outcome::Repairable { needed, available }) => {}
    Some(Outcome::Insufficient { needed, available }) => {}
    Some(Outcome::Incomplete) | None => {}
}
```

- Reads the regular files directly inside one directory through a cap-std
  `Dir`: no recursion, symlinks ignored, nothing outside opened, nothing written.
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

Fuzzing: `cd fuzz && cargo +nightly fuzz run packets` (seed with
`tests/fixtures/*/*.par2`). A bounded deterministic mutation test runs in
`cargo test`.
