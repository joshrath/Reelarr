# Clean-room source ledger

Implementation date: **2026-11-09 PST**. reelarr-par2 is an original PAR2 2.0
verifier written from the public specification. No code from par2cmdline,
libpar2, rust-par2, SABnzbd, MultiPar or any other PAR2 implementation was
opened, copied or translated. This ledger is provenance, not legal clearance.

| Reference | Use |
|---|---|
| https://parchive.github.io/doc/Parity%20Volume%20Set%20Specification%20v2.0.html | Packet header (magic, length, packet MD5 over set ID onward, set ID, type), main packet (slice size, recovery-set count, file IDs), file description (file ID derivation, full and 16 KiB MD5, length, NUL-padded name), IFSC (per-slice MD5 and CRC32 of zero-padded slices), recovery slice (exponent, data length), creator and unknown-packet skipping, set ID = MD5 of the main body. Recovery uses Reed-Solomon over GF(2^16) with generator 0x0001100B, input-slice constants that are powers of two whose exponents are not 0 mod 3, 5, 17 or 257, little-endian words, and `R_e[j] = sum_i (D_i[j] * (C_i ^ e))`. |
| RFC 1321 (MD5) and the CRC-32 (IEEE 802.3) definition | Digests come from the `md-5` and `crc32fast` crates; no hash code is implemented here. |
| https://docs.rs/cap-std/4.0.3/cap_std/ | Capability-relative, read-only directory access. |
| GitHub issues #74, #14, #38 | Scope, hostile-input rules, verification and repair. |

## Fixtures

`tests/fixtures/gen.py` builds every fixture from the specification alone
(Python `hashlib.md5`, `zlib.crc32`). See `tests/fixtures/PROVENANCE.md`. No
par2 binary was installed or run, consistent with the no-parity-binaries rule
(#61).

## Known limits

- Repair reconstructs missing or damaged slices from recovery packets present
  in the directory. It does not fetch additional volumes itself.
- Slices are checked at aligned offsets only; there is no sliding-window
  search for data shifted by inserted or deleted bytes.
- Recovery blocks are counted (distinct exponents with the set's slice size);
  their data isn't validated beyond the packet MD5.
