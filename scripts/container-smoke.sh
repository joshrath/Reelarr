#!/usr/bin/env bash
# Requires Docker, curl and Python 3 ON THE HOST, not in the runtime image.
set -Eeuo pipefail
image=${1:-reelarr:ci}
suffix="$$-$RANDOM"
name="reelarr-smoke-$suffix"
volume="$name-config"
media_volume="$name-media"
work=$(mktemp -d)
cleanup() {
    status=$?
    if (( status != 0 )); then docker logs "$name" >&2 || true; fi
    docker rm -fv "$name" "$name-default" "$name-bad" "$name-extra" >/dev/null 2>&1 || true
    docker volume rm "$volume" "$media_volume" >/dev/null 2>&1 || true
    rm -rf "$work"
    exit "$status"
}
trap cleanup EXIT
uid=23456
gid=23457
# Also check the image metadata and out-of-box non-root path, not just bootstrap.
test "$(docker image inspect -f '{{.Config.User}}' "$image")" = 1000:1000
docker run -d --name "$name-default" "$image"
wait_healthy() {
    local container=$1
    for _ in $(seq 1 90); do
        state=$(docker inspect -f '{{.State.Status}}' "$container")
        test "$state" = running || { docker logs "$container"; return 1; }
        if test "$(docker inspect -f '{{.State.Health.Status}}' "$container")" = healthy; then return; fi
        sleep 1
    done
    docker inspect "$container"; return 1
}
wait_healthy "$name-default"
docker exec "$name-default" sh -ec 'test "$(id -u):$(id -g)" = 1000:1000; test -w /config; test -s /config/reelarr.db'
docker rm -fv "$name-default"

docker volume create "$volume" >/dev/null
docker volume create "$media_volume" >/dev/null
# Simulate a pre-existing NAS-owned library, mounted read-only. Bootstrap must
# neither chown it nor require write access to adopt/read existing files.
docker run --rm --user 0:0 --entrypoint sh -v "$media_volume:/media" "$image" -ec \
    'printf existing-movie > /media/existing.mkv; chown 23456:23457 /media/existing.mkv; chmod 640 /media/existing.mkv'
# A fresh root-owned volume tests bootstrap's ownership repair. Use a nondefault
# port to exercise the Rust probe's environment handling, without host collisions.
docker run -d --name "$name" --user 0:0 \
    -e PUID="$uid" -e PGID="$gid" -e REELARR_PORT=7987 \
    -v "$volume:/config" -v "$media_volume:/media:ro" -p 127.0.0.1::7987 "$image"
wait_healthy "$name"
port=$(docker inspect -f '{{(index (index .NetworkSettings.Ports "7987/tcp") 0).HostPort}}' "$name")
curl -fsS "http://127.0.0.1:$port/health" | python3 -c 'import json,sys; assert json.load(sys.stdin)["status"] == "ok"'
curl -fsS "http://127.0.0.1:$port/" | grep -q 'aria-label="Main"'
for asset in wasm js css; do curl -fsS -o /dev/null "http://127.0.0.1:$port/pkg/reelarr.$asset"; done
curl -fsS -o /dev/null "http://127.0.0.1:$port/fonts/bebas-neue-400.woff2"
# docker exec's default user is the image/override user, not the server user.
# Inspect the real server's kernel credentials to verify privilege dropping.
docker exec "$name" sh -ec '
    found=0
    for file in /proc/[0-9]*/status; do
        if grep -q "^Name:.*reelarr$" "$file"; then
            awk -v u=23456 -v g=23457 '\''/^Uid:/ {if ($2!=u || $3!=u || $4!=u || $5!=u) exit 1; uid=1} /^Gid:/ {if ($2!=g || $3!=g || $4!=g || $5!=g) exit 1; gid=1} END {if (!uid || !gid) exit 1}'\'' "$file"
            found=1
        fi
    done
    test "$found" = 1
    test "$(stat -c %u:%g /config/reelarr.db)" = 23456:23457
    /usr/local/share/reelarr/check-runtime.sh
'
docker exec --user "$uid:$gid" "$name" sh -ec 'test -w /config; test "$(stat -c %u:%g /media/existing.mkv)" = 23456:23457; test "$(cat /media/existing.mkv)" = existing-movie; ffmpeg -version; ffprobe -version'
# Persist real SQLite data, not just a marker file. Stop before copying all WAL files;
# use the same non-root UID for the write through an ephemeral volume helper.
docker stop "$name" >/dev/null
docker cp "$name:/config/." "$work/before"
python3 - "$work/before/reelarr.db" <<'PY'
import sqlite3, sys
with sqlite3.connect(sys.argv[1]) as db:
    assert db.execute('select count(*) from _sqlx_migrations').fetchone()[0] >= 2
    db.execute("insert into settings(key,value) values ('container-smoke','survives-restart')")
    db.commit()
    db.execute('pragma wal_checkpoint(truncate)')
PY
docker run --rm -i --user "$uid:$gid" --entrypoint sh -v "$volume:/config" "$image" \
    -c 'rm -f /config/reelarr.db-wal /config/reelarr.db-shm; cat > /config/reelarr.db' < "$work/before/reelarr.db"
# The probe must fail when nothing is listening, without initializing a database.
if docker run --rm --entrypoint reelarr "$image" healthcheck; then
    echo 'healthcheck incorrectly passed with no server' >&2; exit 1
fi
docker start "$name" >/dev/null
wait_healthy "$name"
docker exec --user "$uid:$gid" "$name" reelarr healthcheck
docker stop "$name" >/dev/null
docker cp "$name:/config/." "$work/after"
python3 - "$work/after/reelarr.db" <<'PY'
import sqlite3, sys
with sqlite3.connect(sys.argv[1]) as db:
    assert db.execute("select value from settings where key='container-smoke'").fetchone() == ('survives-restart',)
PY
# An invalid data path must fail startup, not become healthy.
if docker run --name "$name-bad" -e REELARR_DATA_DIR=/proc/reelarr-unwritable "$image"; then
    echo 'startup unexpectedly succeeded with unwritable storage' >&2; exit 1
fi
test "$(docker inspect -f '{{.State.Health.Status}}' "$name-bad")" != healthy
# Invalid PUID/PGID must fail closed, never fall back to root.
if docker run --rm --user 0:0 -e PUID=0 "$image"; then exit 1; fi
# Supplementary groups: REELARR_EXTRA_GIDS must appear on the dropped process and
# invalid values must fail closed. Extra GIDs are never used for chown.
extra_a=34567
extra_b=34568
id_out=$(docker run --rm --user 0:0 \
    -e PUID="$uid" -e PGID="$gid" -e REELARR_EXTRA_GIDS="$extra_a, $extra_b" \
    --entrypoint /usr/local/bin/reelarr-entrypoint "$image" id)
echo "$id_out"
echo "$id_out" | grep -q "uid=$uid"
echo "$id_out" | grep -q "gid=$gid"
echo "$id_out" | grep -q "$extra_a"
echo "$id_out" | grep -q "$extra_b"
# Running server must carry the same supplementary groups.
docker run -d --name "$name-extra" --user 0:0 \
    -e PUID="$uid" -e PGID="$gid" -e REELARR_EXTRA_GIDS="$extra_a,$extra_b" \
    -e REELARR_PORT=7988 -v "$volume:/config" -p 127.0.0.1::7988 "$image"
wait_healthy "$name-extra"
docker exec "$name-extra" sh -ec "
    found=0
    for file in /proc/[0-9]*/status; do
        if grep -q '^Name:.*reelarr\$' \"\$file\"; then
            groups=\$(awk '/^Groups:/ {for (i=2; i<=NF; i++) print \$i}' \"\$file\")
            echo \"\$groups\" | grep -qx $extra_a
            echo \"\$groups\" | grep -qx $extra_b
            found=1
        fi
    done
    test \"\$found\" = 1
    test \"\$(stat -c %u:%g /config)\" = $uid:$gid
"
docker rm -fv "$name-extra" >/dev/null
# Invalid REELARR_EXTRA_GIDS must fail closed.
for bad in 0 '1001,' ',1001' '1001,,1002' abc '01' '1001;1002'; do
    if docker run --rm --user 0:0 -e PUID="$uid" -e PGID="$gid" -e REELARR_EXTRA_GIDS="$bad" "$image"; then
        echo "REELARR_EXTRA_GIDS=$bad unexpectedly succeeded" >&2
        exit 1
    fi
done
echo 'Container runtime smoke test passed'
