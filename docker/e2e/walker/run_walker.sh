#!/usr/bin/env bash
# ncrs container entrypoint for the walker harness: mount the (fault-proxied)
# WebDAV tree, walk it with concurrent find/ls -R while sampling the daemon's
# threads / RSS / CPU, idle for IDLE_SECS, then evaluate pass/fail.
#
# Env (defaults in docker-compose.yml): DAV_URL, PROXY, WALK_SECS, WALK_REPEAT
# (1 = keep re-walking the already-cached tree until WALK_SECS), WALK_FINDS
# (concurrent finds, default 2), IDLE_SECS, SAMPLE_SECS, MAX_THREADS,
# IDLE_MAX_THREADS, IDLE_MAX_CPU, OPTIMISTIC_LISTING, DIR_CACHE_MAX_STALE_MINS.
set -uo pipefail

URL="${DAV_URL:-http://faultproxy:8080/remote.php/dav/files/testuser/}"
USER="${WEBDAV_USER:-testuser}"
PASS="${WEBDAV_PASS:-testpass}"
PROXY="${PROXY:-http://faultproxy:8080}"
MOUNT=/mnt/ncrs
RES=/results
LOG=/tmp/ncrs.log
WALK_SECS="${WALK_SECS:-300}"
WALK_REPEAT="${WALK_REPEAT:-0}"
WALK_FINDS="${WALK_FINDS:-2}"
DONE=/tmp/walk_done
mkdir -p "$RES" "$MOUNT" "$HOME/.config/ncrs"
rm -f "$RES"/*.csv "$RES"/*.json "$RES"/*.jsonl "$RES"/*.log "$DONE"

say() { echo "[walker] $*"; }

say "ncrs binary: $(ls -la /usr/local/bin/ncrs | awk '{print $5" bytes"}') sha256=$(sha256sum /usr/local/bin/ncrs | cut -c1-16)"
say "waiting for WebDAV via proxy at ${URL}"
code=""
for _ in $(seq 1 60); do
    code="$(curl -s -o /dev/null -w '%{http_code}' -u "$USER:$PASS" -X PROPFIND -H 'Depth: 0' "$URL" || true)"
    [ "$code" = "207" ] && break
    sleep 1
done
[ "$code" = "207" ] || { say "FAIL: WebDAV never became ready (last HTTP $code)"; exit 2; }
curl -s -X POST "$PROXY/__reset" >/dev/null

cat > "$HOME/.config/ncrs/config.yaml" <<CFG
url: "${URL}"
username: "${USER}"
password: "${PASS}"
user: "${USER}"
mount_point: "${MOUNT}"
allow_insecure_http: true
http3: false
optimistic_listing: ${OPTIMISTIC_LISTING:-false}
auto_keep_cached_files: false
dir_cache_max_stale_mins: ${DIR_CACHE_MAX_STALE_MINS:-15}
CFG

say "mounting"
RUST_LOG="${RUST_LOG:-info}" ncrs --config "$HOME/.config/ncrs/config.yaml" >"$LOG" 2>&1 &
DAEMON=$!
cleanup() {
    [ -n "${WALKERS:-}" ] && kill $WALKERS 2>/dev/null
    fusermount3 -u "$MOUNT" 2>/dev/null || umount -l "$MOUNT" 2>/dev/null || true
    kill "$DAEMON" 2>/dev/null || true
}
trap cleanup EXIT
for _ in $(seq 1 60); do
    mountpoint -q "$MOUNT" && break
    kill -0 "$DAEMON" 2>/dev/null || { say "FAIL: daemon exited early"; tail -60 "$LOG"; exit 2; }
    sleep 1
done
mountpoint -q "$MOUNT" || { say "FAIL: mount did not appear"; tail -60 "$LOG"; exit 2; }
say "mounted (daemon pid $DAEMON); walking for up to ${WALK_SECS}s (finds=$WALK_FINDS repeat=$WALK_REPEAT)"

# No-op unless the proxy runs with FAULT_ARMED=0 (faults held off until mounted).
curl -s -X POST "$PROXY/__arm" >/dev/null
python3 /walker/sampler.py sample "$DAEMON" "$RES" "$DONE" &
SAMPLER=$!

# One walker: run <cmd> once (or repeatedly with WALK_REPEAT=1) until the
# deadline. Records passes + exit codes to $RES/walk_<name>.txt.
walk() {
    local name="$1"; shift
    local deadline=$(( $(date +%s) + WALK_SECS )) passes=0 rc=0
    local err="/tmp/walk_${name}.err"
    : > "$err"
    while :; do
        local left=$(( deadline - $(date +%s) ))
        [ "$left" -le 0 ] && break
        timeout -k 10 "$left" "$@" >/dev/null 2>>"$err"
        rc=$?
        passes=$((passes + 1))
        [ "$WALK_REPEAT" = "1" ] || break
        [ "$rc" = "124" ] && break
    done
    # Errors accumulate over all passes; split by errno text.
    local enoent eagain eio other total
    total=$(wc -l < "$err")
    enoent=$(grep -c 'No such file or directory' "$err")
    eagain=$(grep -c 'Resource temporarily unavailable' "$err")
    eio=$(grep -c 'Input/output error' "$err")
    other=$(( total - enoent - eagain - eio ))
    echo "passes=$passes last_rc=$rc errors=$total ENOENT=$enoent EAGAIN=$eagain EIO=$eio other=$other" > "$RES/walk_${name}.txt"
    grep -v 'No such file or directory\|Resource temporarily unavailable\|Input/output error' "$err" | head -5 >> "$RES/walk_${name}.txt"
    grep 'No such file or directory' "$err" | head -10 >> "$RES/walk_${name}.txt"
    grep 'Resource temporarily unavailable\|Input/output error' "$err" | head -5 >> "$RES/walk_${name}.txt"
}

T_WALK=$(date +%s)
WALKERS=""
for i in $(seq 1 "$WALK_FINDS"); do
    walk "find$i" find "$MOUNT" -name no-such-file & WALKERS="$WALKERS $!"
done
walk "lsR" ls -R "$MOUNT" & WALKERS="$WALKERS $!"
wait $WALKERS
WALKERS=""
WALK_ELAPSED=$(( $(date +%s) - T_WALK ))
touch "$DONE"
say "walk finished after ${WALK_ELAPSED}s; idling ${IDLE_SECS:-60}s"
for f in "$RES"/walk_*.txt; do say "  $(basename "$f" .txt): $(head -1 "$f")"; done
wait "$SAMPLER"

# ---- end-of-run facts -------------------------------------------------------
responsive=false; detail=""
t0=$(date +%s%N)
if out=$(timeout 10 ls "$MOUNT" 2>&1); then
    responsive=true; detail="ls ok in $(( ($(date +%s%N) - t0) / 1000000 ))ms, $(printf '%s\n' "$out" | wc -l) entries"
else
    detail="ls failed rc=$?: $(printf '%s' "$out" | head -c 200)"
fi
detail=$(printf '%s' "$detail" | tr -d '"\\\n')
alive=false; kill -0 "$DAEMON" 2>/dev/null && alive=true
cnt() { grep -cE "$1" "$LOG" 2>/dev/null || true; }
proxy_final="$(curl -s --max-time 5 "$PROXY/__stats" || echo null)"
[ -n "$proxy_final" ] || proxy_final=null
walk_json="{"
for f in "$RES"/walk_*.txt; do
    n=$(basename "$f" .txt); n=${n#walk_}
    walk_json="$walk_json\"$n\": \"$(head -1 "$f")\", "
done
walk_json="$walk_json\"elapsed_s\": $WALK_ELAPSED}"
cat > /tmp/facts.json <<JSON
{
 "mount_responsive": $responsive,
 "mount_responsive_detail": "$detail",
 "daemon_alive": $alive,
 "walk": $walk_json,
 "log_counts": {
  "incremental list": $(cnt 'incremental list'),
  "readdir .*500": $(cnt 'readdir .*500'),
  "CONNECTIVITY lost": $(cnt 'CONNECTIVITY lost'),
  "proactive_refresh": $(cnt 'proactive_refresh'),
  "LIST_BACKOFF": $(cnt 'LIST_BACKOFF'),
  "SERVER_BREAKER open": $(cnt 'SERVER_BREAKER open'),
  "SERVER_BREAKER closed": $(cnt 'SERVER_BREAKER closed'),
  "SERVER_BREAKER": $(cnt 'SERVER_BREAKER'),
  "WALKER": $(cnt 'WALKER pid='),
  "HEALTH": $(cnt 'HEALTH threads='),
  "pool full": $(cnt 'pool full'),
  "CONNECTIVITY restored": $(cnt 'CONNECTIVITY (restored|regained|back)'),
  "offline mentions": $(cnt "[Oo]ffline"),
  "panicked": $(cnt 'panicked'),
  "ERROR": $(cnt ' ERROR '),
  "WARN": $(cnt ' WARN '),
  "log_lines": $(wc -l < "$LOG")
 },
 "proxy_final": $proxy_final
}
JSON
cp /tmp/facts.json "$RES/facts.json"
for k in HEALTH 'WALKER pid=' SERVER_BREAKER LIST_BACKOFF 'pool full' 'CONNECTIVITY lost'; do
    grep -E "$k" "$LOG" | head -3 > "$RES/sample_$(echo "$k" | tr -c 'A-Za-z' _).txt"
    grep -E "$k" "$LOG" | tail -3 >> "$RES/sample_$(echo "$k" | tr -c 'A-Za-z' _).txt"
done
say "===== ncrs log (tail 40) ====="
tail -40 "$LOG" | cut -c1-300
python3 /walker/sampler.py summarize "$RES" /tmp/facts.json
rc=$?
tail -5000 "$LOG" > "$RES/ncrs.log.tail"
gzip -c "$LOG" > "$RES/ncrs.log.gz"
chmod -R a+rwX "$RES" 2>/dev/null
exit "$rc"
