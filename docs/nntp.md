# NNTP source and dependency ledger (#12)

Checked 8 November 2026 (PST). This is an independent implementation; no source
from SABnzbd, NZBGet, Radarr/Sonarr or other reference applications was read,
copied or translated. Test conversations were authored here and certificates
are generated in memory per test with rcgen. No fixture binaries or proprietary
tools are involved.

## Protocol sources

- [RFC 3977](https://www.rfc-editor.org/rfc/rfc3977): sections 3.1–3.2
  (512-byte initial responses, CRLF, dot-stuffing), 3.6 (Message-ID bounds),
  5.1 (200/201 greeting), 5.4 (QUIT), 6.2.3 (BODY), 6.2.4 (STAT).
- [RFC 4643](https://www.rfc-editor.org/rfc/rfc4643): section 2.3,
  AUTHINFO USER/PASS over TLS; continue only after 381, accept 281, classify
  481/482/502 without forwarding peer diagnostics.
- [RFC 4642](https://www.rfc-editor.org/rfc/rfc4642) and its update
  [RFC 8143](https://www.rfc-editor.org/rfc/rfc8143): verified implicit TLS,
  port 563, SNI, hostname and certificate-chain verification. Modern rustls
  defaults, explicitly restricted to TLS 1.2/1.3; no legacy ciphers/plaintext.

Dependency API documentation (not protocol algorithm sources):
[RootCertStore](https://docs.rs/rustls/0.23.46/rustls/struct.RootCertStore.html),
[tokio-rustls](https://docs.rs/tokio-rustls/0.26.6/tokio_rustls/),
[rustls-native-certs](https://docs.rs/rustls-native-certs/0.8.5/rustls_native_certs/),
and [rcgen](https://docs.rs/rcgen/0.14.10/rcgen/).

## Dependency licenses

Resolved versions checked using cargo metadata and published package license
files in the downloaded Cargo registry. All direct additions are GPL-3.0
compatible. Existing dependencies remain locked; we do not introduce
`native-tls`, OpenSSL or `webpki-roots`. Feature defaults are disabled for
rustls/tokio-rustls and rcgen; the selected crypto provider is ring.

| Dependency | Resolved version | SPDX / selected compatible terms |
|---|---|---|
| tokio (workspace, now a runtime dependency) | 1.54.1 | MIT |
| tracing (workspace) | 0.1.44 | MIT |
| rustls | 0.23.46 | Apache-2.0 OR ISC OR MIT (MIT available) |
| tokio-rustls | 0.26.6 | MIT OR Apache-2.0 |
| rustls-native-certs | 0.8.5 | Apache-2.0 OR ISC OR MIT (MIT available) |
| rcgen (test only) | 0.14.10 | MIT OR Apache-2.0 |
| tracing-subscriber (workspace, test only) | 0.3.23 | MIT |
| thiserror (already present) | 2.0.21 | MIT OR Apache-2.0 |

Important already-locked TLS transitives: ring 0.17.14 (Apache-2.0 AND ISC,
see its LICENSE, LICENSE-BoringSSL and LICENSE-other-bits), rustls-webpki
0.103.16 (ISC), rustls-pki-types 1.15.1 (MIT OR Apache-2.0), and openssl-probe
0.2.2 (MIT OR Apache-2.0; file discovery only, not cryptographic linkage).

New lockfile packages are test-certificate dependencies (some optional):
rcgen 0.14.10; time 0.3.55, time-core 0.1.9, time-macros 0.2.32; yasna 0.6.0;
deranged 0.5.9, num-conv 0.2.2, powerfmt 0.2.1; asn1-rs 0.7.2,
asn1-rs-derive 0.6.0, asn1-rs-impl 0.2.0; autocfg 1.5.1, bit-vec 0.9.1,
data-encoding 2.11.1, der-parser 10.0.0, minimal-lexical 0.2.1, nom 7.1.3,
num-bigint 0.4.8, num-integer 0.1.47, num-traits 0.2.19, oid-registry 0.8.1,
rusticata-macros 4.1.0, synstructure 0.13.2 and x509-parser 0.18.1.
Their declared terms are MIT, Apache-2.0 OR MIT (including legacy
`MIT/Apache-2.0` spelling), or MIT OR Apache-2.0, all compatible.
`cargo tree -p reelarr-nzb --edges normal,dev` identifies the active subset;
lockfile presence does not imply an optional package is compiled.

This check is not a complete distribution SBOM or security advisory audit;
the project's cargo-deny CI issue (#31) and release notices remain separate.

## Validation

The TLS mock tests cover 200/201 greetings, USER-only and USER/PASS success,
481/482/502 rejection, untrusted CA and hostname mismatch before authentication,
BODY/STAT/QUIT, empty and multiline bodies, raw dot-stuff preservation, typed
430 without retry, strict framing/response identity, line and article ceilings,
greeting/handshake/hung-body/whole-operation deadlines, a dropped mid-body
connection, bounded retries/backoff, concurrency/reuse, acquisition timeout,
cancellation, shutdown/waiter wakeup and credential redaction in errors and
captured tracing output (including deliberate server credential echoes).
