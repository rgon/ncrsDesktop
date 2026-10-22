#!/usr/bin/env bash
# Restart survival under the real systemd: boots systemd (255, Ubuntu 24.04) as
# PID 1 of a container, runs ncrs as the packaged user unit
# (packaging/ncrs.service, unchanged) for a lingering test user, and runs
# docker/e2e/systemd/test.sh as that user inside it — re-exec, restart, crash,
# watchdog kill, start limit, stop. The backend is rclone WebDAV (another
# container) behind chunkdav.py (in the systemd container).
#
# Needs docker, /dev/fuse and a prebuilt bookworm binary packed as
# docker/e2e/ncrs/ncrs.deb (see .claude/skills/verify/SKILL.md), or NCRS_DEB=.
# Nothing from the host is mounted into the containers.
#
# Usage: scripts/e2e-systemd.sh        (KEEP=1 leaves the stack up for poking;
#        DOCKER_RUN_OPTS adds flags to the systemd container, e.g.
#        "--security-opt seccomp=unconfined" to let systemd use kcmp(2))
# Exit code: the test's (0 = pass), 2 = setup failed.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEB="${NCRS_DEB:-$ROOT/docker/e2e/ncrs/ncrs.deb}"
P="${E2E_SYSTEMD_PREFIX:-ncrs-sd-e2e}"
NET="$P-net"; DAV="$P-webdav"; SD="$P-systemd"; IMG="$P:latest"
[ -f "$DEB" ] || { echo "[e2e-systemd] no $DEB — build the bookworm binary first (verify skill)"; exit 2; }

teardown() {
    [ "${KEEP:-0}" = 1 ] && { echo "[e2e-systemd] KEEP=1: leaving $SD / $DAV up"; return; }
    docker rm -f "$SD" "$DAV" >/dev/null 2>&1
    docker network rm "$NET" >/dev/null 2>&1
    docker volume rm "$P-data" >/dev/null 2>&1
    docker rmi "$IMG" >/dev/null 2>&1
    rm -rf "$CTX"
    true
}
CTX="$(mktemp -d)"
trap teardown EXIT

# A minimal build context: the repo root would carry the multi-GB target dirs.
cp "$ROOT/docker/e2e/systemd/Dockerfile" "$ROOT/docker/e2e/systemd/test.sh" \
   "$ROOT/docker/e2e/systemd/chunkdav.service" "$ROOT/docker/e2e/systemd/boot.sh" "$ROOT/docker/e2e/ncrs/chunkdav.py" \
   "$ROOT/packaging/ncrs.service" "$CTX/"
cp "$DEB" "$CTX/ncrs.deb"
echo "[e2e-systemd] building $IMG"
docker build -q -t "$IMG" "$CTX" >/dev/null || exit 2

docker network create "$NET" >/dev/null || exit 2
docker run -d --name "$DAV" --network "$NET" --network-alias webdav -v "$P-data:/data" \
    rclone/rclone:1.71 serve webdav /data --addr=:80 \
    --baseurl=/remote.php/dav/files/testuser --user=testuser --pass=testpass >/dev/null || exit 2

# systemd as PID 1 without --privileged: SYS_ADMIN (mounts: FUSE, its own
# cgroup tree), the fuse device, a private cgroup namespace whose cgroup2
# boot.sh remounts writable (systemd delegates a subtree to the user manager),
# tmpfs /run. Docker's default seccomp profile stays on.
echo "[e2e-systemd] booting systemd"
docker run -d --name "$SD" --hostname ncrs-sd --network "$NET" \
    --cap-add SYS_ADMIN --device /dev/fuse --security-opt apparmor=unconfined \
    --cgroupns=private --tmpfs /run --tmpfs /run/lock --tmpfs /tmp \
    ${DOCKER_RUN_OPTS:-} -e container=docker "$IMG" >/dev/null || exit 2

ex() { docker exec "$SD" "$@"; }
asuser() { docker exec -u tester -e XDG_RUNTIME_DIR=/run/user/1000 \
    -e DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus -w /home/tester "$SD" "$@"; }
for _ in $(seq 1 60); do
    st="$(ex systemctl is-system-running 2>/dev/null)"
    case "$st" in running|degraded) break ;; esac
    sleep 1
done
echo "[e2e-systemd] system: ${st:-unknown}"
[ "$st" = degraded ] && ex systemctl --failed --no-legend --plain | sed 's/^/[e2e-systemd]   failed: /'
for _ in $(seq 1 30); do
    asuser systemctl --user is-system-running >/dev/null 2>&1 && break
    case "$(asuser systemctl --user is-system-running 2>/dev/null)" in degraded) break ;; esac
    sleep 1
done
if ! asuser systemctl --user show -p Version --value >/dev/null 2>&1; then
    echo "[e2e-systemd] the user manager for tester never came up"
    ex systemctl status user@1000.service --no-pager 2>&1 | tail -20
    exit 2
fi

start=$(date +%s)
asuser /e2e/test.sh
rc=$?
echo "[e2e-systemd] test took $(($(date +%s) - start)) s, exit $rc"
if [ "$rc" -ne 0 ]; then
    echo "[e2e-systemd] ===== journalctl --user -u ncrs (tail) ====="
    asuser journalctl --user -u ncrs.service --no-pager -o short-precise | tail -80
fi
exit "$rc"
