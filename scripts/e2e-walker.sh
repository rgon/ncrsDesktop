#!/usr/bin/env bash
# Walker regression harness: FUSE-mount a large WebDAV tree (behind a
# fault-injecting proxy) with a prebuilt ncrs binary, walk it with concurrent
# find / ls -R, and gate on daemon thread count, idle drain, idle CPU and mount
# responsiveness. See docker/e2e/README.md ("Walker harness").
#
# Nothing is compiled in Docker: NCRS_BIN is bind-mounted into an Ubuntu 24.04
# runtime image, so build it on the host (glibc <= 2.39) or pass /usr/bin/ncrs.
#
# Usage: NCRS_BIN=target/release/ncrs [FAULT_RATE=0.3 FAULT_MODE=hash|random|burst]
#        [DIRS=20000] [WALK_SECS=300] [WALK_REPEAT=0|1] [LATENCY_MS=0]
#        [MAX_THREADS=200 IDLE_MAX_THREADS=60 IDLE_MAX_CPU=5] scripts/e2e-walker.sh [label]
# Exit code: the ncrs container's (0 = pass, 1 = criteria failed, 2 = setup failed).
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIR="$ROOT/docker/e2e/walker"
: "${NCRS_BIN:?set NCRS_BIN to the ncrs binary to test}"
NCRS_BIN="$(readlink -f "$NCRS_BIN")"
[ -x "$NCRS_BIN" ] || { echo "NCRS_BIN $NCRS_BIN is not executable"; exit 2; }

export COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-ncrs-walker}"
export DIRS="${DIRS:-20000}" WALK_SECS="${WALK_SECS:-300}"
export FAULT_RATE="${FAULT_RATE:-0}" FAULT_MODE="${FAULT_MODE:-hash}"
LABEL="${1:-${RUN_LABEL:-d${DIRS}-${FAULT_MODE}-r${FAULT_RATE}-w${WALK_SECS}}}"
export RUN_LABEL="$LABEL"
STAMP="$(date +%Y%m%d-%H%M%S)"
export RESULTS_DIR="$DIR/results/${STAMP}-${LABEL}"
mkdir -p "$RESULTS_DIR"

sha256sum "$NCRS_BIN" > "$RESULTS_DIR/ncrs.sha256"
export NCRS_BIN

compose() { docker compose -f "$DIR/docker-compose.yml" "$@"; }
teardown() {
    echo "[e2e-walker] tearing down ${COMPOSE_PROJECT_NAME}"
    # KEEP_IMAGES=1 keeps the built runtime image between back-to-back runs;
    # the default removes it (and the seeded volume) every time.
    if [ "${KEEP_IMAGES:-0}" = "1" ]; then
        compose --profile seed down -v --remove-orphans >/dev/null 2>&1 || true
    else
        compose --profile seed down -v --rmi local --remove-orphans >/dev/null 2>&1 || true
    fi
}
trap teardown EXIT

echo "[e2e-walker] df before: $(df -h / | awk 'NR==2{print $4" free ("$5" used)"}')"
echo "[e2e-walker] seeding ${DIRS} dirs"
compose --profile seed build -q seed || exit 2
compose --profile seed run --rm seed || { echo "[e2e-walker] seeding failed"; exit 2; }

echo "[e2e-walker] running walk: FAULT_MODE=$FAULT_MODE FAULT_RATE=$FAULT_RATE WALK_SECS=$WALK_SECS label=$LABEL"
compose up --build --abort-on-container-exit --exit-code-from ncrs \
    --attach ncrs --attach faultproxy
rc=$?
compose logs --no-color faultproxy > "$RESULTS_DIR/faultproxy.log" 2>&1 || true
echo "[e2e-walker] exit code $rc; results in $RESULTS_DIR"
echo "[e2e-walker] df after: $(df -h / | awk 'NR==2{print $4" free ("$5" used)"}')"
exit "$rc"
