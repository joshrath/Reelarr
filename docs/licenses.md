# Bundled tools and redistribution

Reelarr is GPL-3.0-or-later; retain the repository's [LICENSE](../LICENSE).
Rust crates are inventoried in Cargo.lock; license/advisory policy is tracked in
[cargo-deny #31](https://github.com/joshrath/Reelarr/issues/31). That check does not
cover Debian packages. The application is Rust; the sole external media tool is
FFmpeg (`ffmpeg` and `ffprobe`), executed as a subprocess, **never linked** into
the Rust binary. No unrar, RAR, 7-Zip/p7zip, libarchive/bsdtar or par2 is bundled.
None is an optional runtime dependency; any future exception needs approval.

## Exact image inventory

Security updates change Debian package revisions. Rather than claiming a stale
fixed version, every image records the **actual installed version and flags** in
`/usr/share/reelarr/licenses/` (also exported as CI's `runtime-licenses` artifact):

| File | Contents |
| --- | --- |
| `packages.tsv` | Every installed Debian package and exact version, including ffmpeg, tini, util-linux and all shared-library dependencies |
| `ffmpeg-version.txt` | `ffmpeg -version` with upstream/Debian version and library versions |
| `ffmpeg-buildconf.txt` | Complete, unabridged `ffmpeg -buildconf` configure flags |
| `ffmpeg-license.txt` | `ffmpeg -L`: actual build's license and warranty notice |
| `tini-version.txt` | `tini --version` |
| `README.md` | This inventory and redistribution guidance |

The base is Debian bookworm-slim. Debian's bookworm ffmpeg package is currently
**7:5.1.9-0+deb12u1** ([package and source downloads](https://packages.debian.org/bookworm/ffmpeg));
the generated inventory is authoritative for each image/architecture/security
update. Debian enables `--enable-gpl`; its FFmpeg build is **GPL-2.0-or-later**,
not LGPL-only. It is redistributable subject to GPL compliance. Both image build
and smoke validation reject **`--enable-nonfree`**. This is not a patent license
or legal opinion. Architecture-specific configure flags must come from the
recorded build output, not from the developer workstation's FFmpeg.

`ffplay` and `qt-faststart`, although included by Debian's ffmpeg package, are
removed: only the ffmpeg/ffprobe CLI pair is needed. Their FFmpeg shared libraries
remain OS-package dependencies; this does not link them into Reelarr.

## Bootstrap and base OS

- **tini**, Debian bookworm package 0.19.0-1 (actual version recorded above):
  MIT license; preserve `/usr/share/doc/tini/copyright`, its copyright and license
  text. Upstream: https://github.com/krallin/tini . It is PID 1, not a media tool.
- **util-linux / setpriv**, bookworm 2.38.1 series (exact revision in `packages.tsv`):
  setpriv is GPL-2.0-or-later. Preserve `/usr/share/doc/util-linux/copyright` and
  Debian's detailed per-file license inventory (the package has mixed licenses).
- **Debian base essentials**: dash (entrypoint shell), coreutils (id/stat/chown),
  dpkg, grep and their dependencies; CA certificates for TLS; FFmpeg's codec,
  font and transport shared libraries. Licenses vary (GPL, LGPL, BSD, MIT, etc.).
  Exact package versions are in `packages.tsv`; **all** Debian copyright files in
  `/usr/share/doc/*/copyright` and `/usr/share/common-licenses/` are retained.
  No curl, compiler, cargo, extraction or repair utility is added to the runtime.

## When redistributing an image

1. Keep the GPL text, FFmpeg warranty/license notices, tini MIT notice and all
   package copyright/license files. Do not strip `/usr/share/doc` or the generated
   inventory. Include this inventory alongside a distributed image.
2. Provide the complete corresponding source (including Debian patches/build
   scripts, and sources for relevant GPL/LGPL dependencies) for the **exact
   distributed binaries**, in the manner required by each license. Merely linking
   an upstream homepage is not sufficient. For online distribution make matching
   source available alongside the image; preserve the exact Debian source packages
   using the versions in `packages.tsv`, https://sources.debian.org/ and, for older
   versions, https://snapshot.debian.org/. Keep Reelarr's exact source/build files
   with it. An upstream FFmpeg tarball alone omits Debian modifications.
3. Preserve LGPL rights/relocation or relinking requirements where applicable;
   do not impose additional restrictions on those components. Review every
   bundled package's copyright file before a public binary release.

This PR produces an inventory and preserves notices; it does **not** publish an
image or purport to fulfill a distributor's separate source-hosting obligations.
