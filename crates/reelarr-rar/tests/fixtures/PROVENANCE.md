# Fixture provenance

Created **2026-11-08 PST**. All archived content is original test content, not
third-party media. Josh authorized the proprietary `rar` binary **only** for
fixture generation on that date; see ../../SOURCES.md. The binary is not committed,
not in the Docker image and never runs in builds, tests or CI.

## Independent producer

Downloaded https://www.rarlab.com/rar/rarlinux-x64-624.tar.gz on 2026-11-08 PST.
Tool: **RAR 6.24, Linux x64, 3 Oct 2023, trial version**. Evaluation use under the
included license's 40-day testing permission; no purchase. Only `rar`,
`license.txt` and command-line documentation `rar.txt` were unpacked. No UnRAR
source/binary or other extractor was used. Generator path outside repository:
`/workspace/rar-fixture-tool/rar/rar`.

Source content recipe, executed in `/workspace/rar-fixtures-source`:

```python
from pathlib import Path
Path("payload.txt").write_bytes(bytes(range(256)) * 40)  # exactly 10,240 bytes
```

Exact commands (archive output paths abbreviated as `$FIXTURES`, meaning
`/workspace/Reelarr-rar-34/crates/reelarr-rar/tests/fixtures`):

```sh
/workspace/rar-fixture-tool/rar/rar a -ma4 -m0 -v4k -ep -idq $FIXTURES/real4.rar payload.txt
/workspace/rar-fixture-tool/rar/rar a -ma5 -m0 -htb -v4k -ep -idq $FIXTURES/real5.rar payload.txt
/workspace/rar-fixture-tool/rar/rar a -ma4 -m0 -vn -v4k -ep -idq $FIXTURES/old4.rar payload.txt
```

The generator emits three volumes per set. Source file timestamps appear in
archive metadata; byte-identical regeneration needs the original timestamp,
but tests validate exact payload, headers and digests rather than timestamps.
CRC32 source content: `bbce3b9d`. Part checksum/end-marker observations are in
SOURCES.md. The RAR5 set uses independent BLAKE2sp part/final digests.

## Hand-built corpus and public hash facts

`../common/mod.rs` is a **test-only, original Rust builder**, not a decoder port.
It emits stored RAR4/RAR5 blocks from the public structural documentation,
including mutated/unsafe headers and checksums. All generated content is
in-memory or temporary test output; builds do not invoke any archive tool.

`blake2sp-kat.txt`: the 256 unkeyed BLAKE2sp known-answer digest facts from
https://raw.githubusercontent.com/BLAKE2/BLAKE2/master/testvectors/blake2-kat.json
retrieved 2026-11-08 PST. Format: decimal byte length then expected hex digest.
Each input is bytes 0..length-1; no implementation code copied.

## Committed fixture SHA-256

- `blake2sp-kat.txt`: `cef96a21523991c2e9a69faedc2d8c97384397e615ddbf8e0f6c7beef59a99b0`
- `old4.r00`: `2238ffca82ec97aca7789858b9bd2ddc09f2cd9620e58a06308d3747d3a6b62a`
- `old4.r01`: `7cb0d9ff871d185c6e35d3badc375ecb1527ef46e6f44d8c9cbe5d1fd0f20bef`
- `old4.rar`: `669a81681eee6721b9bc22c78e4caf15484f067fb42e4fd23e2ae185ae305a33`
- `real4.part1.rar`: `f09901f941e7bdf93c1f52319c2d193be552c149f6d03e8cb5eecae2f30bbb11`
- `real4.part2.rar`: `2d1f56638b64a9625069c2020dcdf6c94a0b6b4e531e87b2a35a0745d13c0ffb`
- `real4.part3.rar`: `63d56cab40cc504e09fa8fe3f8d66fcbfc0dbadc755864833567227e0e17ca28`
- `real5.part1.rar`: `aa034ca67594595fc952bab5c65c062cb69ab657163439d72250a20ecccc7d22`
- `real5.part2.rar`: `1253041bf3a757a567371fa3e00a798cea1cd405a70e59ba870b50e9af007951`
- `real5.part3.rar`: `eac1768e4694b405ace007e7130b3494484acc830651b50ae84d2d7992954a29`
