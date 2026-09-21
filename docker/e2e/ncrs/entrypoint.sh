#!/usr/bin/env bash
# e2e entrypoint: wait for the WebDAV server, mount it with the ncrs CLI daemon,
# run the data-loss scenarios, and exit with their status. Tears the mount down
# on the way out.
set -uo pipefail

WEBDAV_HOST="${WEBDAV_HOST:-webdav}"
USER="${WEBDAV_USER:-testuser}"
PASS="${WEBDAV_PASS:-testpass}"
URL="http://${WEBDAV_HOST}/remote.php/dav/files/${USER}/"
MOUNT=/mnt/ncrs
LOG=/tmp/ncrs.log

echo "[e2e] waiting for WebDAV at ${URL}"
code=""
for _ in $(seq 1 60); do
    code="$(curl -s -o /dev/null -w '%{http_code}' -u "$USER:$PASS" \
        -X PROPFIND -H 'Depth: 0' "$URL" || true)"
    [ "$code" = "207" ] && { echo "[e2e] WebDAV ready"; break; }
    sleep 1
done
if [ "$code" != "207" ]; then
    echo "[e2e] FAIL: WebDAV never became ready (last HTTP $code)"
    exit 1
fi

mkdir -p "$MOUNT" "$HOME/.config/ncrs"
cat > "$HOME/.config/ncrs/config.yaml" <<EOF
# ncRS e2e config — plain WebDAV backend (not Nextcloud)
url: "${URL}"
username: "${USER}"
password: "${PASS}"
user: "${USER}"
mount_point: "${MOUNT}"
# The e2e WebDAV server is a container on the compose network, not loopback,
# so it needs the explicit opt-in for its deliberately plain-http setup.
allow_insecure_http: true
http3: false
optimistic_listing: false
auto_keep_cached_files: false
# Scenario 19 ages cached listings past this window; 1 minute keeps that wait
# short enough for CI while still exercising the real code path.
dir_cache_max_stale_mins: 1
EOF

echo "[e2e] mounting ncrs daemon"
RUST_LOG="${RUST_LOG:-info}" ncrs --config "$HOME/.config/ncrs/config.yaml" >"$LOG" 2>&1 &
DAEMON=$!

cleanup() {
    fusermount3 -u "$MOUNT" 2>/dev/null || umount "$MOUNT" 2>/dev/null || true
    kill "$DAEMON" 2>/dev/null || true
}
trap cleanup EXIT

for _ in $(seq 1 60); do
    mountpoint -q "$MOUNT" && break
    kill -0 "$DAEMON" 2>/dev/null || { echo "[e2e] FAIL: daemon exited early"; tail -60 "$LOG"; exit 1; }
    sleep 1
done
if ! mountpoint -q "$MOUNT"; then
    echo "[e2e] FAIL: mount did not appear"
    tail -60 "$LOG"
    exit 1
fi
echo "[e2e] mounted at ${MOUNT}"

# Scenario 19 asserts on the daemon's own log lines (DIR_HARD_EXPIRED /
# LIST_ETAG_CONFIRMED), so it needs to know where the log is.
export NCRS_LOG="$LOG"
/e2e/scenarios.sh "$MOUNT" "$URL" "$USER" "$PASS"
rc=$?

if [ "$rc" -ne 0 ]; then
    echo "[e2e] ===== ncrs daemon log (tail) ====="
    tail -100 "$LOG"
fi
exit "$rc"
