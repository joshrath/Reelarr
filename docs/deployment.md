# NAS libraries: NFS and SMB/CIFS

These examples cover deployment for [external library roots #59](https://github.com/joshrath/Reelarr/issues/59),
not implementation of the settings/scanner/offline-root behavior in that issue.
Existing movie trees can be exposed at `/media/movies` and `/media/movies-4k`
without relocating them. Register those container paths as library roots when
that feature is available. For **adopt-in-place scanning**, keep them read-only;
only opt into write access for explicitly requested import/rename operations.
Never treat an empty or unavailable NAS mount as proof that media was deleted.

## Permissions and prerequisites

Mounts are made by Docker's **host** `local` volume driver, never by Reelarr. The
Docker host must support NFS/CIFS and reach the NAS. No `privileged`, `cap_add`,
`SYS_ADMIN`, in-container mount helper or new runtime package is required. These
Linux Engine examples may need host-specific setup on Docker Desktop/rootless
engines. Rootless setups can use pre-mounted host paths instead.

Set `PUID` and `PGID` to the NAS file owner's numeric UID/GID (for example 1000).
NFS honors server-side identities, ACLs and root-squash; using UID 0 or changing
ownership is not a solution. CIFS `uid`, `gid`, `file_mode` and `dir_mode` map
client-side permissions; server account permissions/ACLs still apply. If the NAS
provides Unix ownership, configure that ownership to agree with the chosen IDs.
The bootstrap adjusts **only `/config`**, never any library or download mount.
A permission-denied library should be fixed on the NAS/host, not with added
container privileges. Keep SQLite `/config` on **local** persistent storage rather
than a network filesystem: SQLite WAL locking is unsuitable for many NAS mounts.

### Shared group access (`REELARR_EXTRA_GIDS`)

Many NAS setups grant media access through a shared Unix group (for example a
`media` group) rather than making every file owned by the Reelarr UID. Bootstrap
normally runs `setpriv --clear-groups`, which drops supplementary groups. Set
`REELARR_EXTRA_GIDS` to a comma-separated list of **numeric** GIDs to keep those
groups after the drop (requires root bootstrap, `user: "0:0"`):

```yaml
environment:
  PUID: "1000"
  PGID: "1000"
  # GID of the shared 'media' group on the NAS/host (getent group media)
  REELARR_EXTRA_GIDS: "1001"
```

Multiple groups are allowed (`"1001,1002"`). Spaces around commas are trimmed.
Values must be positive decimal GIDs without leading zeros; empty entries and
`0` are rejected. Extra GIDs are **never** used for `chown` — ownership repair
still targets only `/config` as `PUID:PGID`. Direct non-root runs (`--user
PUID:PGID`) cannot apply this variable; use Docker `--group-add` instead, or
switch to root bootstrap.

## NFS: Docker local driver

Copy the commented example from `docker-compose.yml`, adjusting address/export:

```yaml
services:
  reelarr:
    # Retain the build/environment/config/download mounts from the base example.
    volumes:
      - movies_nfs:/media/movies:ro
volumes:
  movies_nfs:
    driver: local
    driver_opts:
      type: nfs
      o: addr=192.0.2.10,nfsvers=4,rw,hard
      device: ":/exports/movies"
```

`ro` on the service mount enforces read-only access inside Reelarr even though
the host's NFS mount uses `rw`. To import to this root, deliberately change the
service suffix to `rw` and grant NAS write access to PUID/PGID (and any shared
group listed in `REELARR_EXTRA_GIDS`, such as `media`). `hard` preserves
NFS I/O semantics during outages but can block I/O until the NAS returns; scanner
bounded concurrency/offline-root handling is separate work in #59. Arrange host
boot ordering/network availability before starting Reelarr.

## SMB/CIFS: Docker local driver, no passwords in compose

Docker's local driver passes options directly to the kernel. **It cannot expand
`credentials=/some/file`** (that is a `mount.cifs` userspace feature), nor can it
read a Docker secret as a mount password. Do not put that nonworking option in a
volume definition. See [Docker's CIFS volume documentation](https://docs.docker.com/engine/storage/volumes/#create-cifssamba-volumes)
and [the credentials-file limitation](https://github.com/docker/cli/issues/2802).

For a direct Docker volume, keep credentials in a **host-only, mode-0600 Compose
env file**, outside this repository; for example `/etc/reelarr/smb.env`:

```dotenv
SMB_USERNAME=REPLACE_WITH_NAS_ACCOUNT
SMB_PASSWORD=REPLACE_WITH_NAS_PASSWORD
```

Have the host administrator create it securely; never commit it or mount it in
the container. Use `docker compose --env-file /etc/reelarr/smb.env up --build`.
This is a credentials-file pattern for **Compose interpolation**, not a kernel
`credentials=` option. Docker still stores the resolved mount password in volume
metadata (`docker volume inspect`); protect access to Docker, do not post that
output, and use a least-privilege NAS account. Passwords containing a comma need
special care with kernel mount options; prefer the host-mounted pattern below.

```yaml
services:
  reelarr:
    volumes:
      - movies_smb:/media/movies-4k:ro
volumes:
  movies_smb:
    driver: local
    driver_opts:
      type: cifs
      device: "//192.0.2.20/movies-4k"
      o: "addr=192.0.2.20,vers=3.0,username=${SMB_USERNAME:?set in host-only env file},password=${SMB_PASSWORD:?set in host-only env file},uid=${PUID:-1000},gid=${PGID:-1000},file_mode=0644,dir_mode=0755,rw"
```

No literal passwords are stored in YAML. Change `:ro` to `:rw` only after opting
into modifications and confirming the SMB account can write. Never put a real
credential in shell history, a PR, logs or `.env` tracked by Git.

### Stricter credentials-file option: mount on the host, then bind

If credentials must **not appear in Docker volume metadata**, the built-in
local driver's direct CIFS mode cannot satisfy that. Instead let the host
administrator mount the share, then bind it into Reelarr (also supported by #59).
Create a root-owned mode-0600 file `/etc/smb-credentials/reelarr` with
`username=...` and `password=...` lines (optional `domain=...`). A host `/etc/fstab`
entry can reference it without exposing a password:

```fstab
//192.0.2.20/movies-4k /mnt/reelarr-movies-4k cifs credentials=/etc/smb-credentials/reelarr,vers=3.0,uid=1000,gid=1000,file_mode=0644,dir_mode=0755,ro,_netdev 0 0
```

The host requires `mount.cifs`; **the container does not**. Mount/verify the share
on the host before starting Compose. Then use this service volume instead of the
CIFS driver volume:

```yaml
volumes:
  - type: bind
    source: /mnt/reelarr-movies-4k
    target: /media/movies-4k
    read_only: true
    bind:
      create_host_path: false
```

Match both the host `uid/gid` options and PUID/PGID to your NAS permissions. If
library files are group-readable via a shared `media` (or similar) group, also
set `REELARR_EXTRA_GIDS` to that group's numeric GID so the container process
retains it after bootstrap. For write-enabled imports deliberately change both
the host `ro` and container `read_only`. `create_host_path: false` prevents an
absent directory from being silently created; it does **not** prove the NAS is
mounted. Check `findmnt` on the host and use a NAS-resident sentinel/monitor
before startup. Runtime root-health, adopt-in-place guarantees and
unavailable-item behavior remain tracked in #59.

## Indexer secret storage (store integration, #70)

`reelarr-db` now supplies `SqliteIndexerStore`, with secrets encrypted behind an
internal `SecretCipher` abstraction. It does **not** wire indexer CRUD or secret
initialization into the server; that belongs to the #76 integration. The policy
below is decided: indexer API keys and NNTP passwords are encrypted at rest, the
API never returns them and logs never show them.

The default cipher encrypts each API key with RustCrypto XChaCha20-Poly1305 and
an independently generated 24-byte OS-random nonce. Ciphertext is versioned and
bound to the indexer name/field with AEAD associated data. The non-secret settings
and ordered `extra_download_hosts` JSON array remain readable in the database.
The host list must already be normalized lowercase/IDNA ASCII, unique and at most
16 entries; storage rejects invalid input, never normalizes it silently. Exact
allowlisted hosts permit only HTTPS/default-port downloads (including explicit
443), without extending `allow_http` to them; #71 owns URL enforcement.

At startup, integrations call `XChaChaSecretCipher::from_env()` (or
`from_env_or_data_dir` with the configured data directory):

- If set, `REELARR_SECRET_KEY` must be **standard base64 encoding exactly 32 bytes**.
  Invalid encoding/length fails clearly, with no fallback to a key file. Use your
  deployment's secret injection facilities; never put real keys in Git, command
  history, logs or shared Compose files. Environment values can be visible to
  container administrators; limit access to Docker and diagnostic tooling.
- Otherwise load/generate `$REELARR_DATA_DIR/secret.key` (default
  `/config/secret.key`). The file holds **32 raw bytes**, not base64. Creation
  publishes a fully-written, synced temporary file atomically without overwriting
  an existing key. Concurrent initializers use the same winning key. On Unix it
  must be a regular, non-symlink file, owned by the Reelarr process user, mode
  **0600**; unsafe permissions/ownership and invalid lengths fail, not auto-repair.
  Non-Unix systems must use the environment key. The data directory must already
  exist, be trusted/private, and be writable by Reelarr.

This protects **database copies and backups that do not include the key file**;
it does not protect against compromise of the running process or an attacker
who can also read its key. Back up the key separately in secure storage. **Losing
or changing the key means re-entering secrets**, not regenerating a usable key
for existing ciphertext. There is no automatic key rotation/re-encryption in this
PR. Preserve the key across redeploys; switching between env/file keys requires
the same 32-byte key or an explicit future migration.

`IndexerStore::list/get` return credentials only for trusted internal Newznab
client construction (the non-serializable `IndexerConfig`). They are **not API
DTOs**. API integrations must use `list_summaries/get_summary` or an explicit
secret-free projection; the summaries contain only name, enabled, ordered extra
hosts and `has_api_key`, never ciphertext or plaintext. The store's errors/debug
and database statement logging do not expose secrets or SQL bind values.

## NNTP provider settings (#76)

The server now initializes the secret cipher at startup, after migrations and
before binding: `REELARR_SECRET_KEY` if set, otherwise
`$REELARR_DATA_DIR/secret.key` (created on first start, mode 0600). An invalid
key stops startup with a static error that never includes the value. The same
key protects indexer API keys and the NNTP password, so the backup/recovery
notes above apply to both.

Reelarr talks to **one** NNTP provider (no multi-server fallback yet), always
over implicit TLS (normally port 563) with certificate-chain and hostname
verification against the system trust store. Port 119 (plaintext NNTP) is
rejected. Configure it through the authenticated API:

| Method | Path | Notes |
| --- | --- | --- |
| `GET` | `/api/v1/settings/nntp` | Settings plus `has_password`; 404 `not_configured` if unset |
| `PUT` | `/api/v1/settings/nntp` | Save; CSRF required |
| `DELETE` | `/api/v1/settings/nntp` | Forget the provider and password; CSRF required |
| `POST` | `/api/v1/settings/nntp/test` | TLS + login check of the posted settings, without saving; CSRF required |

The password is write-only. Send `{"action":"set","value":"…"}` to replace it,
`{"action":"clear"}` to remove it, or `{"action":"keep"}` (or omit the field)
to leave it unchanged. `keep` is refused with `password_required` when the host
changes, so a saved password is never sent to a different server without being
re-entered. Bounds: 1–100 connections, 1–300 second connect/IO timeouts, 0–10
retries. The test-connection request fetches no article and the whole attempt
is limited by the connect timeout.

Saved settings apply to new work only. New jobs use a pool built from the new
settings, while downloads already running finish on the pool they started with;
running downloads are never interrupted. The old pool is closed once its last
download ends. With no provider configured (or a disabled one), downloads stay
queued until one is saved.

## Built-in downloader (#76)

The download scheduler starts after restart recovery and before the listener is
bound. It works in `$REELARR_DOWNLOADS_DIR` (default `/downloads/reelarr`, inside
the `/downloads` volume): `incomplete/` holds per-job staging and `complete/`
the finished downloads waiting for import. The folder must belong to Reelarr
alone. `REELARR_MAX_ACTIVE_DOWNLOADS` (default 2) limits how many jobs run at
once; the provider's connections are split evenly between them. Jobs start in
priority order, then queue position. A higher-priority job waits for a free slot
and never interrupts a running one. On shutdown (SIGTERM or Ctrl-C) running
downloads stop at a checkpoint and resume on the next start. Damaged downloads
are checked with PAR2; repair itself is not supported yet.
