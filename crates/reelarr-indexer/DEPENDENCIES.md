# New lockfile dependency declarations

Checked 2026-11-08 (PST) using resolved Cargo metadata. Optional and non-Linux packages are included here; a lock entry is not proof of active linkage. No newly added package has an undeclared license. `/` in older manifest declarations means the dual-license OR choice. Existing workspace dependencies are not duplicated in this table.

Runtime TLS uses reqwest `rustls` + `query` with default features off, explicit rustls selection, platform verification and AWS-LC. The active feature tree has no native-tls/openssl-sys. `openssl-probe` locates trust stores; it does not link OpenSSL. AWS-LC is a separately built crypto library, so this is rustls, not a claim of pure-Rust cryptography. Its distributed LICENSE was read: the older OpenSSL-derived portions in this exact release are under Apache-2.0, not the old incompatible OpenSSL license. Other components carry ISC/MIT/BSD-3-Clause/Apache-2.0 (and permissive alternatives).

`webpki-root-certs` (target-specific fallback roots) carries CDLA-Permissive-2.0, not MIT/Apache. Its license was read: distributing the data requires the agreement text, with no results restriction. Preserve it in artifacts that include that data. Other new transitive declarations are permissive GPL-3.0-compatible choices; retain their notices in release packaging.

| Package | Version | Declared license |
|---|---|---|
| assert-json-diff | 2.0.2 | MIT |
| aws-lc-rs | 1.19.0 | ISC AND (Apache-2.0 OR ISC) |
| aws-lc-sys | 0.47.0 | ISC AND (Apache-2.0 OR ISC) AND Apache-2.0 AND MIT AND BSD-3-Clause AND (Apache-2.0 OR ISC OR MIT) AND (Apache-2.0 OR ISC OR MIT-0) |
| base64 | 0.23.1 | MIT OR Apache-2.0 |
| cc | 1.11.1 | MIT OR Apache-2.0 |
| cfg_aliases | 0.2.2 | MIT |
| chacha20 | 0.10.2 | MIT OR Apache-2.0 |
| cmake | 0.1.58 | MIT OR Apache-2.0 |
| combine | 4.6.9 | MIT |
| core-foundation | 0.10.1 | MIT OR Apache-2.0 |
| core-foundation-sys | 0.8.7 | MIT OR Apache-2.0 |
| cpufeatures | 0.3.1 | MIT OR Apache-2.0 |
| deadpool | 0.12.3 | MIT OR Apache-2.0 |
| deadpool-runtime | 0.1.4 | MIT OR Apache-2.0 |
| dunce | 1.0.5 | CC0-1.0 OR MIT-0 OR Apache-2.0 |
| find-msvc-tools | 0.1.15 | MIT OR Apache-2.0 |
| fnv | 1.0.7 | Apache-2.0 / MIT |
| fs_extra | 1.3.0 | MIT |
| getrandom | 0.2.17 | MIT OR Apache-2.0 |
| h2 | 0.4.20 | MIT |
| hermit-abi | 0.5.3 | MIT OR Apache-2.0 |
| hyper-rustls | 0.27.10 | Apache-2.0 OR ISC OR MIT |
| ipnet | 2.12.2 | MIT OR Apache-2.0 |
| jni | 0.22.4 | MIT OR Apache-2.0 |
| jni-macros | 0.22.4 | MIT OR Apache-2.0 |
| jni-sys | 0.4.1 | MIT OR Apache-2.0 |
| jni-sys-macros | 0.4.1 | MIT OR Apache-2.0 |
| jobserver | 0.1.35 | MIT OR Apache-2.0 |
| lru-slab | 0.1.3 | MIT OR Apache-2.0 OR Zlib |
| num_cpus | 1.17.0 | MIT OR Apache-2.0 |
| openssl-probe | 0.2.2 | MIT OR Apache-2.0 |
| pkg-config | 0.3.34 | MIT OR Apache-2.0 |
| quinn | 0.11.12 | MIT OR Apache-2.0 |
| quinn-proto | 0.11.19 | MIT OR Apache-2.0 |
| quinn-udp | 0.5.16 | MIT OR Apache-2.0 |
| rand | 0.10.3 | MIT OR Apache-2.0 |
| rand_core | 0.10.1 | MIT OR Apache-2.0 |
| rand_pcg | 0.10.2 | MIT OR Apache-2.0 |
| reqwest | 0.13.5 | MIT OR Apache-2.0 |
| ring | 0.17.14 | Apache-2.0 AND ISC |
| roxmltree | 0.21.2 | MIT OR Apache-2.0 |
| rustls | 0.23.46 | Apache-2.0 OR ISC OR MIT |
| rustls-native-certs | 0.8.5 | Apache-2.0 OR ISC OR MIT |
| rustls-pki-types | 1.15.1 | MIT OR Apache-2.0 |
| rustls-platform-verifier | 0.7.1 | MIT OR Apache-2.0 |
| rustls-platform-verifier-android | 0.2.0 | MIT OR Apache-2.0 |
| rustls-webpki | 0.103.16 | ISC |
| schannel | 0.1.30 | MIT |
| security-framework | 3.7.0 | MIT OR Apache-2.0 |
| security-framework-sys | 2.17.0 | MIT OR Apache-2.0 |
| shlex | 2.0.1 | MIT OR Apache-2.0 |
| simd_cesu8 | 1.2.0 | Apache-2.0 OR MIT |
| simdutf8 | 0.1.5 | MIT OR Apache-2.0 |
| subtle | 2.6.1 | BSD-3-Clause |
| tinyvec | 1.13.3 | Zlib OR Apache-2.0 OR MIT |
| tokio-rustls | 0.26.6 | MIT OR Apache-2.0 |
| tokio-util | 0.7.20 | MIT |
| tower-http | 0.6.11 | MIT |
| try-lock | 0.2.5 | MIT |
| untrusted | 0.9.0 | ISC |
| want | 0.3.2 | MIT |
| web-time | 1.1.0 | MIT OR Apache-2.0 |
| webpki-root-certs | 1.0.11 | CDLA-Permissive-2.0 |
| windows-sys | 0.52.0 | MIT OR Apache-2.0 |
| windows-targets | 0.52.6 | MIT OR Apache-2.0 |
| windows_aarch64_gnullvm | 0.52.6 | MIT OR Apache-2.0 |
| windows_aarch64_msvc | 0.52.6 | MIT OR Apache-2.0 |
| windows_i686_gnu | 0.52.6 | MIT OR Apache-2.0 |
| windows_i686_gnullvm | 0.52.6 | MIT OR Apache-2.0 |
| windows_i686_msvc | 0.52.6 | MIT OR Apache-2.0 |
| windows_x86_64_gnu | 0.52.6 | MIT OR Apache-2.0 |
| windows_x86_64_gnullvm | 0.52.6 | MIT OR Apache-2.0 |
| windows_x86_64_msvc | 0.52.6 | MIT OR Apache-2.0 |
| wiremock | 0.6.5 | MIT/Apache-2.0 |
| zeroize | 1.9.1 | Apache-2.0 OR MIT |
