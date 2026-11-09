#!/bin/sh
# Only base-OS bootstrap tools, never download/media helper programs.
set -eu
fail() { echo "reelarr: $*" >&2; exit 1; }
uid=${PUID:-1000}
gid=${PGID:-1000}
for value in "$uid" "$gid"; do
    case "$value" in ''|*[!0-9]*|0*) fail 'PUID/PGID must be positive decimal IDs without leading zeros' ;; esac
    [ "$value" -le 2147483647 ] || fail 'PUID/PGID exceeds supported range'
done
[ ! -L /config ] || fail '/config must not be a symlink'
if [ "$(id -u)" = 0 ]; then
    mkdir -p /config
    # Never touch /media or /downloads, and never traverse symlinks. Limit the
    # ownership change to this volume, only when its root needs changing.
    if [ "$(stat -c %u:%g /config)" != "$uid:$gid" ]; then
        chown -R -P --no-dereference "$uid:$gid" /config
    fi
    exec setpriv --reuid="$uid" --regid="$gid" --clear-groups \
        --bounding-set=-all --inh-caps=-all --ambient-caps=-all --no-new-privs "$@"
fi
[ "$(id -u):$(id -g)" = "$uid:$gid" ] || fail 'set PUID/PGID to match --user, or use --user 0:0 for bootstrap'
[ -d /config ] && [ -w /config ] || fail '/config must exist and be writable by PUID/PGID (or use root bootstrap)'
exec "$@"
