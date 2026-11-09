# Container runtime

The image defaults to **UID/GID 1000:1000** (`USER`, not merely an environment
variable). `tini` remains PID 1 and forwards signals/reaps children. Reelarr and
its FFmpeg/ffprobe subprocesses run unprivileged. No extraction/repair programs
are installed: those features belong in Rust. FFmpeg is never linked to Reelarr.
The shell entrypoint, tini, setpriv and other Debian base-OS utilities are only
container/bootstrap essentials, not application/media dependencies.

## Identity and volumes

- `PUID` / `PGID` default to `1000`; both must be nonzero decimal IDs.
- For a pre-owned `/config`, use `docker run --user 1234:1234 -e PUID=1234
  -e PGID=1234 ...` (or omit all three options for 1000:1000).
- For an initially root-owned bind mount or a different ID, use `--user 0:0`
  **only for bootstrap**, together with `PUID`/`PGID`. Compose demonstrates this:
  `PUID=$(id -u) PGID=$(id -g) docker compose up --build` (do not use root's ID).
  Bootstrap creates `/config` and recursively fixes ownership **only if its root
  owner differs**. It then uses util-linux `setpriv` to drop capabilities, set
  no-new-privileges, and exec the server with those IDs. Supplementary groups are
  cleared by default; set `REELARR_EXTRA_GIDS` (comma-separated numeric GIDs) to
  retain shared NAS groups such as `media`. See [deployment.md](deployment.md).
- Ownership repair never follows symlinks or changes `/media` or `/downloads`.
  A symlink in place of `/config` is rejected. If files inside an already correctly
  owned `/config` have different ownership, fix them explicitly on the host; we
  avoid traversing/chowning a whole database volume on every startup.
- `/config` stores SQLite, WAL/SHM and future configuration. Mount it persistently.
  It must be writable. Mount media/downloads separately and pre-provision their
  permissions for the selected UID/GID. No automatic recursive chown there.
- `REELARR_DATA_DIR` may override the DB directory, but automatic creation/ownership
  repair applies **only to `/config`**; provision any override yourself.
- For rootless Docker or NFS root-squash, pre-own storage and run directly non-root.
  Root bootstrap needs the usual CHOWN/SETUID/SETGID/SETPCAP capabilities until it
  drops them; do not use `--cap-drop ALL` with that mode. Direct non-root mode
  works with `--cap-drop ALL --security-opt no-new-privileges`.

`REELARR_PORT` defaults to 7979. Docker's exec-form HEALTHCHECK uses the same privilege-drop entrypoint (including
when Compose bootstraps as root), then calls the Rust
`reelarr healthcheck` command, which probes `http://127.0.0.1:$REELARR_PORT/health`
with bounded connect/read/write timeouts. It does not open a database or require
curl. Startup failures (including migrations/permissions) cannot pass the probe.

## Validation and licenses

`docker build -t reelarr:ci . && scripts/container-smoke.sh reelarr:ci` checks the
default UID, custom UID/GID, ownership, startup, health, UI assets, FFmpeg as the
custom user, real SQLite data surviving restart, and negative probes/startup.
The script requires Docker, Python 3 and curl **on the host** only. CI runs it and
exports the exact runtime inventory as the `runtime-licenses` artifact.

[License inventory and redistribution obligations](licenses.md).

## Secret key volume

The #70 indexer store provides an injectable encrypted-secret policy, pending
Josh's final decision; server wiring follows separately in #76. Its default
startup initializer uses `REELARR_SECRET_KEY` (standard base64, exactly 32 bytes)
or atomically loads/creates `$REELARR_DATA_DIR/secret.key`, normally
`/config/secret.key` (32 raw bytes). Keep `/config` persistent and private. The
initializer runs **after** privilege drop, so the file is owned by PUID with mode
0600, not by root; pre-provisioned files must have that same ownership/mode.
Never chown library mounts to fix a key-file error. Overrides of the data directory
must be pre-provisioned, as described above. A malformed/unsafe existing key is
never overwritten or silently replaced.

XChaCha20-Poly1305 protects database copies/backups **only when they do not include
the key file**. It is not protection against a compromised container or a full
`/config` backup containing both DB and key. Back up the key separately and
securely; losing the key means re-entering secrets. Do not change/remove an env
key or replace `secret.key` during an upgrade. No API returns stored credentials.
See [deployment.md](deployment.md#indexer-secret-storage-store-integration-70)
for initialization, API-safe read methods and the exact threat model.
