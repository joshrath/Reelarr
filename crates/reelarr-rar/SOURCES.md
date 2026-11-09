# Clean-room source ledger

Implementation/research date: **2026-11-08 PST**. This is an original stored-mode
container reader, not a decompressor. No UnRAR engine, UnRAR-derived port or
libarchive implementation was opened, copied or translated. The RAR5 technote's
referral to UnRAR was **not followed**. This ledger is provenance, not legal clearance.

| Reference | Sections / use |
|---|---|
| https://www.rarlab.com/technote.htm | RAR5 signature, general block/extra layouts, bounded vints, main/volume flags and numbering, file flags, compression information (stored only), host attributes, UTF-8 names, service skipping, CRC32/BLAKE2sp records and part-versus-final coverage, redirection/encryption rejection, end flags. Read in full; algorithm-source referrals excluded. |
| https://sources.debian.org/src/rar/2%3A4.0.b3-1/technote.txt/ | RAR 4.00 documentation **only**, not adjacent program files: marker/main/file/common layouts, low-16 CRC32, high sizes, stored method, split flags, host OS, dictionary/directory bits, UTF-8 versus encoded-Unicode flag, unsupported encryption/solid/embedded comments. |
| https://acritum.com/winrar/rar-format | RAR 3.93 format-document mirror, same structural facts as 4.00. Web fetch timed out; public HTML downloaded with curl and read as text. No program source. |
| https://docs.rs/blake2s_simd/1.0.5/blake2s_simd/blake2sp/index.html | Streaming BLAKE2sp API; explicitly **not** sequential BLAKE2s. Use existing independent hash implementation, no new crypto implementation. |
| https://raw.githubusercontent.com/BLAKE2/BLAKE2/master/testvectors/blake2-kat.json | 256 unkeyed BLAKE2sp digest facts for lengths 0..255 extracted to `tests/fixtures/blake2sp-kat.txt`. No hash implementation/source copied. Inputs are bytes 0..length-1. |
| https://www.rarlab.com/download.htm and https://www.rarlab.com/rar/rarlinux-x64-624.tar.gz | Fixture generation **only**. RAR 6.24 supports generation of both RAR4 and RAR5. Only `rar`, `license.txt`, `rar.txt` were unpacked; no UnRAR binary or engine source used. `rar.txt`: -ma4/-ma5, -m0, -htb, -v and -vn switches. Trial license grants a 40-day test period; no license purchase or binary redistribution. See fixture provenance and behavior observations below. |
| https://docs.rs/cap-std/4.0.3/cap_std/ | Capability-relative filesystem API; writes rooted in newly created private staging. Crate metadata/license checked with `cargo info` and `cargo metadata --locked`. |
| crates.io / Cargo package metadata for exact dependency versions | Dependency licenses and feature provenance, recorded in DEPENDENCIES.md. |
| /workspace/reelarr-planning/rar-plan.md; GitHub issues #34 and #14 | Project constraints, staging/integrity policy, source gates and fixture authorization. |

## Authorization

**2026-11-08: Josh approved using the proprietary `rar` tool solely to generate
test fixtures; never in the build, CI runtime dependencies or Docker image;
generated fixtures are committed with provenance.** Approval relayed by the
Project Manager, with Josh's words: “Yes, file the RAR issues and rewrite #14,
and using the rar tool just to generate test fixtures is fine”.

The downloaded binary stays outside the repository at
`/workspace/rar-fixture-tool/rar/rar`; it is not a build/test dependency. No
extractor binary was invoked. Test execution reads committed fixtures only.

## RAR4 documentation gaps resolved by owned-fixture observation

The public RAR4 notes do not specify complete split checksum/end-marker semantics.
We therefore inspected bytes emitted by fixture-generation commands (not decoder
source), using an independently written Python `struct`/`zlib` probe. Observed
RAR 6.24 `-ma4 -m0 -v4k` behavior for our 10,240-byte payload:

| Part | Packed bytes | Header FILE_CRC | Independently calculated part CRC |
|---|---:|---|---|
| 1 | 4008 | 92e7a616 | 92e7a616 |
| 2 | 4008 | c0ef8460 | c0ef8460 |
| 3 | 2224 | bbce3b9d | e97ce44f |

Full content CRC32 is **bbce3b9d**. Thus non-final parts verify their local stored
bytes; the final part verifies all concatenated stored bytes. `HIGH_UNP_SIZE` /
`UNP_SIZE` describe the logical file size across all parts, not each part size.
Header CRCs equal the low 16 bits of zlib CRC32 over header bytes after HEAD_CRC,
including extended-time metadata emitted here. ASCII names and the documented
UTF-8 flag avoid guessing legacy codepages; encoded name pairs are rejected.

End block type **0x7b** is not described in those public RAR4 notes. Fixture
observations: flags **0x400f** for non-final and **0x400e** for final volume;
first four body bytes match CRC32 over the entire preceding volume bytes,
including signature; next two bytes are zero-based volume number. Remaining
seven bytes are zero reserved space in this fixture. Single-volume synthetic
ends test flag 0x4000 without optional fields. We interpret low bits 1 (next),
2 (archive CRC), 8 (volume number) only as demonstrated by these observations;
bit 4's payload is bounded and ignored. Multi-volume RAR4 requires end volume
numbers, first-volume flags and naming agreement; older variants missing these
are explicitly unsupported, not guessed. No sentinel/no-checksum inference is
made for legacy split CRC fields: every advertised CRC must actually match.

RAR5 fixtures additionally show common skip-on-update flag 0x4 on main and end
headers, and zero padding after the end marker to fill a volume. Permit those
flags and bounded all-zero trailing padding; reject nonzero trailing material.
RAR5 checksums and other semantics use the published technote, not inference.

## Search hygiene / excluded material

One web search for RAR4 split checksum/end-marker **documentation** returned
incidental snippets from UnRAR and derived-port source pages. Those pages were
not opened; the snippets were excluded as implementation evidence. No code or
inference from those snippets is used. The owned-fixture observations above
were performed to resolve the gaps independently. This disclosure is deliberate:
we do not claim that the search engine never displayed excluded material.
Compressed RAR4/RAR5 implementation still requires the separate source review.
