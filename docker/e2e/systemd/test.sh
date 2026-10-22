#!/usr/bin/env bash
# Restart survival under the REAL systemd (PID 1 of this container, user
# manager user@1000.service), with ncrs running as the packaged user unit
# (packaging/ncrs.service, unchanged). Mirrors e2e scenario 32, which runs the
# same steps against fake_systemd.py, and adds the start limit. Every step is
# checked against systemd's own state (`systemctl --user show`) and journal.
#
# Runs as the test user inside the container (scripts/e2e-systemd.sh starts
# it); the shell running this script is outside the unit and holds the open
# descriptors, like a program (LibreOffice) would. Every access to the mount
# is bounded, so a hung mount fails the step instead of hanging the test.
set -uo pipefail

MOUNT="$HOME/Nextcloud"
CFG="$HOME/.config/ncrs/config.yaml"
DROPIN="$HOME/.config/systemd/user/ncrs.service.d"
U=testuser; P=testpass
URL="http://127.0.0.1:8090/remote.php/dav/files/${U}/"   # chunkdav, in front of rclone
CDLOG=/var/log/chunkdav.log
SLOW_SECS=8
T=/tmp/sd-e2e; mkdir -p "$T"
PASS=0; FAIL=0
ok() { echo "  ✓ $1"; PASS=$((PASS + 1)); }
no() { echo "  ✗ $1"; FAIL=$((FAIL + 1)); }

bounded() { timeout -s KILL "$@"; }   # exit 137 = the call was still blocked in the kernel
sha() { sha256sum | awk '{print $1}'; }
dav_get() { curl -s -u "$U:$P" "${URL}$1"; }
prop() { systemctl --user show -p "$1" --value ncrs.service; }
state() { echo "MainPID=$(prop MainPID) NRestarts=$(prop NRestarts) ActiveState=$(prop ActiveState) Result=$(prop Result) NFileDescriptorStore=$(prop NFileDescriptorStore)"; }
jlog() { journalctl --user -u ncrs.service -o cat --no-pager 2>/dev/null; }
mark() { JM=$(($(jlog | wc -l) + 1)); }
since() { jlog | tail -n +"$JM"; }
wait_log() {   # <regex> <seconds>: the unit logged it since the mark
    for _ in $(seq 1 "$2"); do since | grep -qE "$1" && return 0; sleep 1; done; return 1
}
quote() { since | grep -E "$1" | tail -1 | sed 's/^/      journal: /'; }
show() {
    echo "      systemd: $(state)"
    since | grep -iE "handoff|SIGTERM|watchdog|restored|error|ncrs.service|Detaching" | tail -10 | sed 's/^/      journal: /'
}
wait_prop() {  # <prop> <value> <seconds>
    for _ in $(seq 1 "$3"); do [ "$(prop "$1")" = "$2" ] && return 0; sleep 1; done; return 1
}
wait_new_pid() {  # <old pid> <seconds>: a different main process is running
    local p
    for _ in $(seq 1 "$2"); do
        p="$(prop MainPID)"
        [ "$p" != 0 ] && [ "$p" != "$1" ] && [ "$(prop ActiveState)" = active ] && return 0
        sleep 1
    done
    return 1
}
# bash, not sh: dash's printf reports every write error as "I/O error".
fd_write() { bounded 10 bash -c 'printf %s "$1" >&7' _ "$1" 2>&1; }   # write through the held fd 7
held() {  # <step> <chunk>: the descriptors opened in step 1 still write and read, the mount answers
    local err
    err="$(fd_write "$2")" && ok "$1: the writer opened in step 1 still writes" \
        || no "$1: the writer opened in step 1 can no longer write (${err:-rc $?})"
    [ "$(bounded 10 python3 -c 'import os; print(os.pread(8, 64, 0).decode(), end="")' 2>/dev/null)" = reader-content ] \
        && ok "$1: the reader opened in step 1 still reads" \
        || no "$1: the reader opened in step 1 can no longer read"
    bounded 10 ls "$MOUNT/hold" >/dev/null 2>&1 && ok "$1: the mount answers" || no "$1: the mount does not answer"
    # One entry: systemd keeps a stored descriptor when it passes it on, and a
    # copy sent again could become a second entry (no kcmp under Docker's seccomp).
    [ "$(prop NFileDescriptorStore)" = 1 ] && ok "$1: systemd's fd store holds exactly one entry" \
        || no "$1: NFileDescriptorStore=$(prop NFileDescriptorStore), want 1"
}
enotconn() {  # <what> <command...>: the command fails ENOTCONN, promptly
    local err rc
    err="$(bounded 10 "${@:2}" 2>&1 >/dev/null)"; rc=$?
    case "$err" in
        *"not connected"*) ok "$1 fails ENOTCONN" ;;
        *) [ "$rc" = 137 ] && no "$1 hangs (killed after 10 s)" || no "$1 did not fail ENOTCONN (rc $rc: ${err:-it succeeded})" ;;
    esac
}

echo "→ systemd $(systemctl --version | head -1 | awk '{print $2}'), kernel $(uname -r), user manager $(systemctl --user is-system-running 2>&1)"

# ── setup: backend content, a log level that shows the hand-off lines ────────
for _ in $(seq 1 30); do
    [ "$(curl -s -o /dev/null -w '%{http_code}' -u "$U:$P" -X PROPFIND -H 'Depth: 0' "$URL")" = 207 ] && break
    sleep 1
done
head -c 2000000 /dev/urandom > "$T/slow.bin"
curl -s -u "$U:$P" -X MKCOL "${URL}hold/" -o /dev/null
curl -s -u "$U:$P" -T "$T/slow.bin" "${URL}hold/slowpoke.bin" -o /dev/null
printf 'reader-content' | curl -s -u "$U:$P" -T - "${URL}hold/r.txt" -o /dev/null
# The unit sets no RUST_LOG, so the daemon logs warnings only; the hand-off
# lines asserted on are info. Set it in the manager's environment, not the unit.
systemctl --user set-environment RUST_LOG=info

echo "→ 1. systemctl --user start ncrs: the mount appears and parks its connection in the fd store"
mark
systemctl --user start ncrs.service
for _ in $(seq 1 30); do bounded 5 mountpoint -q "$MOUNT" && [ "$(prop NFileDescriptorStore)" = 1 ] && break; sleep 1; done
if ! bounded 5 mountpoint -q "$MOUNT" || [ "$(prop NFileDescriptorStore)" != 1 ]; then
    no "start: the mount never appeared, or systemd's fd store never got its connection"; show
    echo; echo "systemd e2e: ${PASS} passed, ${FAIL} failed"; exit 1
fi
ok "start: mounted by pid $(prop MainPID); systemd holds NFileDescriptorStore=1"
wait_log "kept in the systemd fd store" 5 && quote "kept in the systemd fd store"
exec 7>"$MOUNT/hold/w.txt"
exec 8<"$MOUNT/hold/r.txt"
err="$(fd_write p0-)" && ok "start: a writer and a reader are open on the mount; part 0 written" \
    || no "start: cannot write part 0 (${err})"

echo "→ 2. IPC RESTART_HANDOFF: the daemon re-executes itself, systemd sees no restart"
mark; p0="$(prop MainPID)"; n0="$(prop NRestarts)"
reply="$(printf 'RESTART_HANDOFF\n' | timeout 10 nc -U -N "$XDG_RUNTIME_DIR/ncrs.sock" 2>/dev/null)"
if [ "$reply" = ok ] && wait_log "took over FUSE connection .* from this daemon's previous image" 20 \
    && wait_log "handoff: restored .* open handles" 20 \
    && [ "$(prop MainPID)" = "$p0" ] && [ "$(prop NRestarts)" = "$n0" ] && [ "$(prop ActiveState)" = active ]; then
    ok "re-exec: MainPID ${p0} unchanged, NRestarts ${n0}, its new image took the connection over"
    quote "previous image"; quote "handoff: restored"
else
    no "re-exec: no re-exec took the connection over (reply '${reply}', MainPID ${p0} → $(prop MainPID))"; show
fi
held "re-exec" "p1-"

echo "→ 3. systemctl --user restart ncrs: SIGTERM hand-over through the fd store"
mark; p1="$(prop MainPID)"
systemctl --user restart ncrs.service
if wait_new_pid "$p1" 30 && wait_log "took over FUSE connection .* from the systemd fd store" 30 \
    && since | grep -q "SIGTERM: drained; exact handle snapshot written" \
    && wait_log "handoff: restored .* \([1-9][0-9]* exact" 10; then
    ok "restart: MainPID ${p1} → $(prop MainPID); drained, took the connection from the fd store, restored the write handle exactly"
    quote "SIGTERM: drained"; quote "from the systemd fd store"; quote "handoff: restored"
else
    no "restart: the connection or the exact handle state did not reach the next daemon"; show
fi
held "restart" "p2-"

echo "→ 4. kill -9 the main process: systemd restarts it with the stored connection"
mark; p2="$(prop MainPID)"; n2="$(prop NRestarts)"
kill -9 "$p2"
if wait_new_pid "$p2" 30 && wait_log "took over FUSE connection .* from the systemd fd store" 30 \
    && wait_log "handoff: restored .* \(0 exact, 0 failed, 0 lost\)" 10 \
    && [ "$(prop NRestarts)" = $((n2 + 1)) ]; then
    ok "crash: NRestarts ${n2} → $(prop NRestarts), MainPID ${p2} → $(prop MainPID) took the connection back via LISTEN_FDS"
    quote "status=9/KILL"; quote "from the systemd fd store"; quote "handoff: restored"
else
    no "crash: no restarted daemon took the connection over with its handles"; show
fi
held "crash" "p3-"

echo "→ 5. watchdog: a frozen daemon is killed by systemd and its successor resends what it left unanswered"
# A drop-in shortens the 90 s WatchdogSec; LimitCORE=0 keeps the SIGABRT from
# writing a core of a 100 MB daemon through the host's core_pattern.
mkdir -p "$DROPIN"
printf '[Service]\nWatchdogSec=10\nLimitCORE=0\n' > "$DROPIN/watchdog.conf"
systemctl --user daemon-reload
# Clears the start-limit counter, so each step's starts are its own.
systemctl --user reset-failed ncrs.service
mark; p3="$(prop MainPID)"
systemctl --user restart ncrs.service
if wait_new_pid "$p3" 30 && wait_log "took over FUSE connection .* from the systemd fd store" 30 \
    && [ "$(prop WatchdogUSec)" = 10s ]; then
    ok "watchdog: restarted after daemon-reload with WatchdogUSec=$(prop WatchdogUSec), the connection kept"
else
    no "watchdog: the restart with the shorter watchdog did not keep the connection"; show
fi
mark; p4="$(prop MainPID)"; n4="$(prop NRestarts)"
: > "$T/read"; : > "$T/read.err"
( bounded 90 sha256sum "$MOUNT/hold/slowpoke.bin" 2>"$T/read.err" | awk '{print $1}' > "$T/read" ) &
rd=$!
cd_lines=$(wc -l < "$CDLOG")
for _ in $(seq 1 40); do tail -n +"$((cd_lines + 1))" "$CDLOG" | grep -q "SLOW GET .*slowpoke.bin" && break; sleep 0.5; done
kill -STOP "$p4"
stopped_at=$(date +%s)
( bounded 90 mkdir "$MOUNT/hold/frozen-dir" 2>"$T/mkdir.err"; echo $? > "$T/mkdir.rc" ) &
md=$!
wait "$rd" "$md"
if wait_new_pid "$p4" 30 && wait_log "Failed with result 'watchdog'" 5 \
    && since | grep -q "asked the kernel to resend requests the previous daemon left unanswered" \
    && [ "$(prop NRestarts)" = $((n4 + 1)) ]; then
    ok "watchdog: pid ${p4} frozen, killed by systemd and restarted (NRestarts ${n4} → $(prop NRestarts)); pid $(prop MainPID) had the kernel resend"
    quote "Watchdog timeout"; quote "Killing process"; quote "Failed with result 'watchdog'"; quote "asked the kernel to resend"
else
    no "watchdog: the frozen daemon was not replaced by one that resent its requests"; show
fi
[ "$(cat "$T/read")" = "$(sha < "$T/slow.bin")" ] \
    && ok "watchdog: the read in flight when the daemon froze completed with the right bytes ($(($(date +%s) - stopped_at)) s after the freeze)" \
    || no "watchdog: the read in flight did not complete correctly ($(head -1 "$T/read.err"))"
[ "$(cat "$T/mkdir.rc")" = 0 ] && bounded 10 test -d "$MOUNT/hold/frozen-dir" \
    && ok "watchdog: a mkdir issued during the freeze ran exactly once" \
    || no "watchdog: the mkdir issued during the freeze failed (rc $(cat "$T/mkdir.rc"): $(cat "$T/mkdir.err"))"
held "watchdog" "p4"
rm -f "$DROPIN/watchdog.conf"; systemctl --user daemon-reload

exec 7>&-
want="$(printf 'p0-p1-p2-p3-p4' | sha)"
for _ in $(seq 1 90); do [ "$(dav_get hold/w.txt | sha)" = "$want" ] && break; sleep 1; done
[ "$(dav_get hold/w.txt | sha)" = "$want" ] \
    && ok "everything written across the four restarts reached the backend in order" \
    || no "the file written across the restarts is wrong on the backend: '$(dav_get hold/w.txt | head -c 80)'"

echo "→ 6. start limit: a daemon that fails right after taking the connection over"
cp "$CFG" "$T/config.good"
# No credentials: the config parses, the connection is taken over, then
# NextCloudFs::new fails (exit 1) — Restart=on-failure keeps retrying.
sed -e '/^password:/d' "$T/config.good" > "$CFG"; echo 'auth_command: "false"' >> "$CFG"
exec 9>"$MOUNT/hold/limit.txt"
systemctl --user reset-failed ncrs.service
mark; n6="$(prop NRestarts)"; t6=$(date +%s)
systemctl --user restart ncrs.service
# When automatic restarts run into the limit, systemd 255 logs "Start request
# repeated too quickly" but keeps the last failure as Result (exit-code);
# start-limit-hit is what a manual start refused by the limit reports.
if wait_prop ActiveState failed 90 && wait_log "Start request repeated too quickly" 5; then
    ok "start limit: the unit ended ActiveState=failed Result=$(prop Result) after $(($(date +%s) - t6)) s (NRestarts ${n6} → $(prop NRestarts))"
    quote "Start request repeated too quickly"; quote "Failed with result"
else
    no "start limit: the unit did not fail on its start limit"; show
fi
wait_log "failed before serving the connection it took over" 1 \
    && ok "start limit: the first failing daemon gave the connection up itself" && quote "failed before serving" \
    || { no "start limit: no failing daemon gave the connection up"; show; }
[ "$(prop NFileDescriptorStore)" = 0 ] && ok "start limit: the failed unit's fd store is empty" \
    || no "start limit: NFileDescriptorStore=$(prop NFileDescriptorStore) on the failed unit"
# The first failing daemon aborted the connection; the next one, started with
# an empty fd store, found the mount ENOTCONN and detached it (fusermount -uz)
# before failing too. New lookups then see the bare mount-point directory.
if bounded 10 mountpoint -q "$MOUNT"; then
    no "start limit: the dead mount is still mounted on $MOUNT"
else
    ok "start limit: the dead mount was detached ($(bounded 10 stat "$MOUNT/hold" 2>&1 | sed 's/.*: //'))"
    quote "Detaching stale FUSE mount"
fi
enotconn "start limit: a write on a descriptor held across the failure" bash -c 'printf x >&9'
exec 8<&- 9>&-

echo "→ 7. reset-failed + start with the good config, then systemctl --user stop"
cp "$T/config.good" "$CFG"
systemctl --user reset-failed ncrs.service
mark
systemctl --user start ncrs.service
for _ in $(seq 1 30); do [ "$(bounded 5 cat "$MOUNT/hold/r.txt" 2>/dev/null)" = reader-content ] && break; sleep 1; done
if [ "$(bounded 5 cat "$MOUNT/hold/r.txt" 2>/dev/null)" = reader-content ] && [ "$(prop ActiveState)" = active ]; then
    ok "recover: the unit runs again (pid $(prop MainPID)) and the mount serves files"
    quote "Detaching stale FUSE mount"
else
    no "recover: the mount did not come back after reset-failed + start"; show
fi
exec 7>>"$MOUNT/hold/w.txt"
err="$(fd_write -p5)" || no "recover: cannot append part 5 (${err})"
exec 7>&-
want="$(printf 'p0-p1-p2-p3-p4-p5' | sha)"
for _ in $(seq 1 60); do [ "$(dav_get hold/w.txt | sha)" = "$want" ] && break; sleep 1; done
[ "$(dav_get hold/w.txt | sha)" = "$want" ] \
    && ok "the backend file holds everything written across all steps ('$(dav_get hold/w.txt)')" \
    || no "the backend file is wrong after all steps: '$(dav_get hold/w.txt | head -c 80)'"
exec 9>"$MOUNT/hold/after-stop.txt"
mark
systemctl --user stop ncrs.service
[ "$(prop ActiveState)" = inactive ] && [ "$(prop NFileDescriptorStore)" = 0 ] \
    && ok "stop: ActiveState=inactive, the fd store dropped" || no "stop: $(state)"
enotconn "stop: a stat of the mount" stat "$MOUNT/hold"
# (A read can still be served from the page cache; a write always reaches the daemon.)
enotconn "stop: a held descriptor's write" bash -c 'printf x >&9'
exec 9>&-

echo
echo "systemd e2e: ${PASS} passed, ${FAIL} failed"
[ "$FAIL" -eq 0 ]
