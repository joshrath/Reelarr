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

# Optional comma-separated supplementary GIDs for shared NAS 'media' groups.
# Unset/empty keeps setpriv --clear-groups. Never used for chown.
trim_spaces() {
    value=$1
    while [ "$value" != "${value# }" ]; do value=${value# }; done
    while [ "$value" != "${value% }" ]; do value=${value% }; done
    printf '%s' "$value"
}
extra_raw=$(trim_spaces "${REELARR_EXTRA_GIDS-}")
extra_groups=
if [ -n "$extra_raw" ]; then
    case "$extra_raw" in
        *[!0-9,\ ]*) fail 'REELARR_EXTRA_GIDS must be comma-separated numeric GIDs (digits, commas, spaces only)' ;;
    esac
    # Append a sentinel comma so trailing/empty entries are visible to the loop.
    rest=$extra_raw,
    while [ -n "$rest" ]; do
        part=${rest%%,*}
        rest=${rest#*,}
        part=$(trim_spaces "$part")
        case "$part" in
            '') fail 'REELARR_EXTRA_GIDS must not contain empty entries' ;;
            *[!0-9]*|0*) fail 'REELARR_EXTRA_GIDS entries must be positive decimal GIDs without leading zeros' ;;
        esac
        [ "$part" -le 2147483647 ] || fail 'REELARR_EXTRA_GIDS entry exceeds supported range'
        if [ -n "$extra_groups" ]; then
            extra_groups="$extra_groups,$part"
        else
            extra_groups=$part
        fi
    done
    [ -n "$extra_groups" ] || fail 'REELARR_EXTRA_GIDS must not be empty when set'
fi

[ ! -L /config ] || fail '/config must not be a symlink'
if [ "$(id -u)" = 0 ]; then
    mkdir -p /config
    # Never touch /media or /downloads, and never traverse symlinks. Limit the
    # ownership change to this volume, only when its root needs changing.
    # Extra GIDs are never used for chown.
    if [ "$(stat -c %u:%g /config)" != "$uid:$gid" ]; then
        chown -R -P --no-dereference "$uid:$gid" /config
    fi
    if [ -n "$extra_groups" ]; then
        exec setpriv --reuid="$uid" --regid="$gid" --groups="$extra_groups" \
            --bounding-set=-all --inh-caps=-all --ambient-caps=-all --no-new-privs "$@"
    fi
    exec setpriv --reuid="$uid" --regid="$gid" --clear-groups \
        --bounding-set=-all --inh-caps=-all --ambient-caps=-all --no-new-privs "$@"
fi
[ "$(id -u):$(id -g)" = "$uid:$gid" ] || fail 'set PUID/PGID to match --user, or use --user 0:0 for bootstrap'
[ -z "$extra_groups" ] || fail 'REELARR_EXTRA_GIDS requires root bootstrap (--user 0:0); use Docker --group-add for direct non-root'
[ -d /config ] && [ -w /config ] || fail '/config must exist and be writable by PUID/PGID (or use root bootstrap)'
exec "$@"
