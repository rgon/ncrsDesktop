#!/usr/bin/env bash
# End-to-end data-loss scenarios, run against the live FUSE mount and verified
# against the WebDAV backend directly. The theme is integrity: every byte
# written locally must be retrievable, unchanged, from both the mount and the
# backend; moves/renames must preserve content; deletes must fully propagate.
#
# Uploads are asynchronous (the daemon PUTs on close, in the background) and a
# not-kept file is re-downloaded on the next read, so both mount and backend
# reads are polled to a timeout rather than read once — the guarantee under test
# is "no data is lost", i.e. the bytes become and stay retrievable, not that the
# very first read after write is already consistent.
#
# Args: <mount> <backend-url> <user> <pass>
set -uo pipefail

MOUNT="$1"; URL="$2"; U="$3"; P="$4"
PASS=0; FAIL=0
ok() { echo "  ✓ $1"; PASS=$((PASS + 1)); }
no() { echo "  ✗ $1"; FAIL=$((FAIL + 1)); }

sha() { sha256sum | awk '{print $1}'; }
fuse_sha() { [ -f "$MOUNT/$1" ] && sha256sum "$MOUNT/$1" 2>/dev/null | awk '{print $1}' || echo MISSING; }
dav_sha() { curl -s -u "$U:$P" "${URL}$1" | sha; }
dav_code() { curl -s -o /dev/null -w '%{http_code}' -u "$U:$P" "${URL}$1"; }

# Wait until the mount copy of <path> hashes to <want> (uploads + re-download
# are async), nudging a readdir each round.
wait_fuse_sha() {
    local path="$1" want="$2" t="${3:-45}"
    for _ in $(seq 1 "$t"); do
        [ "$(fuse_sha "$path")" = "$want" ] && return 0
        ls "$MOUNT/$(dirname "$path")" >/dev/null 2>&1
        sleep 1
    done
    return 1
}
# Wait until the backend copy of <path> hashes to <want>.
wait_dav_sha() {
    local path="$1" want="$2" t="${3:-45}"
    for _ in $(seq 1 "$t"); do
        [ "$(dav_sha "$path")" = "$want" ] && return 0
        sleep 1
    done
    return 1
}
# Wait until the mount no longer has <path>.
wait_fuse_gone() {
    local path="$1" t="${2:-30}"
    for _ in $(seq 1 "$t"); do
        [ ! -e "$MOUNT/$path" ] && return 0
        ls "$MOUNT" >/dev/null 2>&1
        sleep 1
    done
    return 1
}
# Wait until the backend no longer has <path> (HTTP 404).
wait_dav_gone() {
    local path="$1" t="${2:-45}"
    for _ in $(seq 1 "$t"); do
        [ "$(dav_code "$path")" = "404" ] && return 0
        sleep 1
    done
    return 1
}

# ── Server-outage simulation ─────────────────────────────────────────────────
# Sever / restore connectivity to the WebDAV host with iptables (needs NET_ADMIN)
# so we can verify that edits made while the server is down are saved locally,
# stay readable from the mount, and sync once the server returns. Blocking by IP
# with a TCP reset makes the daemon's requests fail fast (connection refused)
# rather than hang, mirroring an unreachable server.
DAV_HOST="$(printf '%s' "$URL" | sed -E 's#^[a-z]+://([^/:]+).*#\1#')"
DAV_IP="$(getent hosts "$DAV_HOST" | awk '{print $1; exit}')"
DAV_IP="${DAV_IP:-$DAV_HOST}"
server_down() {
    iptables -I OUTPUT -p tcp -d "$DAV_IP" --dport 80 -j REJECT --reject-with tcp-reset
    echo "    (server unreachable: OUTPUT→${DAV_IP}:80 rejected)"
}
server_up() {
    iptables -D OUTPUT -p tcp -d "$DAV_IP" --dport 80 -j REJECT --reject-with tcp-reset 2>/dev/null || true
    echo "    (server reachable again)"
}
# Unlike server_down (a fast RST), a real "network off" BLACKHOLES packets: they
# are silently DROPped, so the daemon's connect() gets no answer and blocks until
# its connect timeout fires. This is the condition that used to hang a save; use
# it to prove the mount now fails fast and stays responsive.
server_blackhole() {
    iptables -I OUTPUT -p tcp -d "$DAV_IP" --dport 80 -j DROP
    echo "    (server blackholed: OUTPUT→${DAV_IP}:80 dropped, connects hang)"
}
server_unblackhole() {
    iptables -D OUTPUT -p tcp -d "$DAV_IP" --dport 80 -j DROP 2>/dev/null || true
    echo "    (server reachable again)"
}

echo "→ 1. CREATE"
C1="ncrs-e2e create $(date +%s%N)"
printf '%s' "$C1" > "$MOUNT/create.txt"
W1="$(printf '%s' "$C1" | sha)"
wait_fuse_sha create.txt "$W1" && ok "content readable on mount" || no "content not readable on mount (data loss)"
wait_dav_sha  create.txt "$W1" && ok "backend received identical content" || no "backend content missing/differs (data loss)"

echo "→ 2. UPDATE (overwrite in place)"
C2="ncrs-e2e UPDATED $(date +%s%N)"
printf '%s' "$C2" > "$MOUNT/create.txt"
W2="$(printf '%s' "$C2" | sha)"
wait_fuse_sha create.txt "$W2" && ok "mount reflects update" || no "mount update lost (data loss)"
wait_dav_sha  create.txt "$W2" && ok "backend reflects update" || no "backend update lost (data loss)"

echo "→ 3. LARGE FILE (8 MiB, under the 10 MiB chunk threshold)"
head -c 8388608 /dev/urandom > /tmp/big.bin
WB="$(sha < /tmp/big.bin)"
cp /tmp/big.bin "$MOUNT/big.bin"
wait_fuse_sha big.bin "$WB" 90 && ok "large file intact on mount (no truncation)" || no "large file corrupted on mount (data loss)"
wait_dav_sha  big.bin "$WB" 90 && ok "large file intact on backend" || no "large file corrupted on backend (data loss)"

echo "→ 4. RENAME (same directory)"
mv "$MOUNT/create.txt" "$MOUNT/renamed.txt"
wait_fuse_sha renamed.txt "$W2" && ok "rename preserves content on mount" || no "rename lost content (data loss)"
wait_fuse_gone create.txt && ok "old name gone on mount" || no "old name still present on mount"
wait_dav_sha  renamed.txt "$W2" && ok "backend has renamed file with content" || no "backend rename lost content (data loss)"
wait_dav_gone create.txt && ok "backend old name removed" || no "backend old name lingering"

echo "→ 5. MOVE into a subdirectory"
mkdir "$MOUNT/sub"
mv "$MOUNT/renamed.txt" "$MOUNT/sub/moved.txt"
wait_fuse_sha sub/moved.txt "$W2" && ok "move-to-subdir preserves content on mount" || no "move lost content (data loss)"
wait_dav_sha  sub/moved.txt "$W2" && ok "backend reflects moved file" || no "backend move lost content (data loss)"

echo "→ 6. MKDIR + nested create"
mkdir -p "$MOUNT/d1/d2"
printf 'nested-content' > "$MOUNT/d1/d2/n.txt"
WN="$(printf 'nested-content' | sha)"
wait_fuse_sha d1/d2/n.txt "$WN" && ok "nested file readable on mount" || no "nested create failed (data loss)"
wait_dav_sha  d1/d2/n.txt "$WN" && ok "backend has nested file" || no "backend nested file lost (data loss)"

echo "→ 7. DELETE file"
rm "$MOUNT/sub/moved.txt"
wait_fuse_gone sub/moved.txt && ok "file deleted on mount" || no "file still present on mount"
wait_dav_gone sub/moved.txt && ok "backend delete propagated" || no "backend delete not propagated"

echo "→ 8. RMDIR (empty directory)"
rmdir "$MOUNT/sub"
wait_fuse_gone sub && ok "directory removed on mount" || no "directory still present on mount"
wait_dav_gone "sub/" && ok "backend rmdir propagated" || no "backend rmdir not propagated"

echo "→ 9. REMOTE → LOCAL propagation (best-effort)"
# Detecting a change made directly on the server needs either notify_push or a
# directory ETag that changes when children change — Nextcloud provides both, a
# plain WebDAV server (rclone) provides neither, so this is informational and
# does NOT gate the suite. It is not a data-loss condition: nothing written
# through the mount is at risk here.
RC="remote-made $(date +%s%N)"
printf '%s' "$RC" > /tmp/remote.txt
RW="$(printf '%s' "$RC" | sha)"
curl -s -u "$U:$P" -T /tmp/remote.txt "${URL}remote.txt" -o /dev/null
if wait_fuse_sha remote.txt "$RW" 20; then
    ok "backend-created file became visible on mount with content"
else
    echo "  ⓘ remote→local change not observed — expected without notify_push /"
    echo "    changing dir ETags on a plain WebDAV server; not a data-loss failure"
fi

echo "→ 10. BATCH rename (20 files, no data loss)"
declare -A want
for i in $(seq 1 20); do
    c="batch-$i-$(date +%s%N)"
    printf '%s' "$c" > "$MOUNT/b$i.txt"
    want[$i]="$(printf '%s' "$c" | sha)"
done
for i in $(seq 1 20); do mv "$MOUNT/b$i.txt" "$MOUNT/r$i.txt"; done
lost=0
for i in $(seq 1 20); do
    wait_fuse_sha "r$i.txt" "${want[$i]}" 60 || lost=$((lost + 1))
done
[ "$lost" = 0 ] && ok "all 20 renamed files keep their content on mount" || no "$lost/20 files lost content (data loss)"
wait_dav_sha r1.txt "${want[1]}" 60 && ok "backend batch sample intact" || no "backend batch sample lost content (data loss)"

echo "→ 11. MIME magic-byte interception (content-type sniffing must not download)"
# GLib 2.80 sniffs the content type of a file with an unknown/ambiguous
# extension by opening it O_NOATIME and reading the first ~16 KiB. On a remote
# mount ncrs must answer that probe with a few synthetic magic bytes instead of
# downloading the file (see mime_magic_bytes() / GLIB_SNIFF_MAX_READ in
# ncrs_core/src/lib.rs). ncrs opens the intercepted handle FOPEN_DIRECT_IO, which
# both keeps the tiny reply out of the page cache (a buffered short read at
# offset 0 would otherwise be cached as EOF and truncate every later read of the
# file) and stops kernel read-ahead from inflating the probe past the guard. We
# probe with GLib's exact 16384-byte read so this reproduces the real GLib path.
od_hex() { od -An -tx1 | tr -d ' \n'; }
# 64 KiB file, unknown extension, with a distinctive printable prefix so a
# *downloaded* read is unmistakably different from the synthetic magic bytes.
{ printf 'NCRS-REAL-DATA--'; head -c 65520 /dev/urandom; } > /tmp/weird.bin
WW="$(sha < /tmp/weird.bin)"
REAL_HEX="$(head -c16 /tmp/weird.bin | od_hex)"
cp /tmp/weird.bin "$MOUNT/weird.ncrstest"
# Upload must complete; the file is not kept locally, so the next read re-fetches
# from the backend unless it is intercepted.
if wait_dav_sha weird.ncrstest "$WW" 90; then
    ls "$MOUNT" >/dev/null 2>&1; sleep 2   # drop any local staging copy

    # The probe is only answered for processes that have GLib's gio loaded
    # (desktop::sniff); a GLib app is simulated by preloading libgio into dd.
    GIO_LIB="$(ls /usr/lib/*/libgio-2.0.so.0 2>/dev/null | head -1)"

    # O_NOATIME probe with GLib's exact 16384-byte read: intercepted → synthetic
    # magic bytes (≠ real content); not intercepted → real downloaded bytes.
    SNIFF_HEX="$(LD_PRELOAD="$GIO_LIB" dd if="$MOUNT/weird.ncrstest" iflag=noatime bs=16384 count=1 2>/dev/null | head -c16 | od_hex)"
    if [ -n "$GIO_LIB" ] && [ -n "$SNIFF_HEX" ] && [ "$SNIFF_HEX" != "$REAL_HEX" ]; then
        ok "O_NOATIME content-type probe from a GLib process intercepted (no download for MIME detection)"
    else
        no "GLib O_NOATIME probe returned real content — file downloaded for MIME detection (regression; gio=${GIO_LIB:-missing})"
    fi

    # A non-GLib tool sharing the O_NOATIME signature (cp, rsync, backups)
    # must get the real bytes, never synthetic ones. (After the probe: a real
    # read may cache the file, and cached files are never intercepted.)
    PLAINTOOL_HEX="$(dd if="$MOUNT/weird.ncrstest" iflag=noatime,fullblock bs=16384 count=1 2>/dev/null | head -c16 | od_hex)"
    [ "$PLAINTOOL_HEX" = "$REAL_HEX" ] && ok "O_NOATIME read from a non-GLib process returns real content" \
        || no "non-GLib O_NOATIME read got synthetic bytes (got ${PLAINTOOL_HEX:-empty})"

    # A plain (no-O_NOATIME) read right after the probe must still return true
    # bytes: the intercept must not leak into ordinary reads, and (regression
    # guard for the page-cache poisoning fixed by FOPEN_DIRECT_IO) the probe must
    # not have truncated the file in the page cache. head does a normal open and
    # keeps read()ing until it has 16 bytes, so it is robust to a short first
    # chunk from the live network stream (which a single `dd count=1` is not).
    PLAIN_HEX="$(head -c16 "$MOUNT/weird.ncrstest" 2>/dev/null | od_hex)"
    [ "$PLAIN_HEX" = "$REAL_HEX" ] && ok "plain read returns real content (intercept scoped to O_NOATIME)" \
        || no "plain read did not return real content (got ${PLAIN_HEX:-empty})"

    # Copies use large buffers and no O_NOATIME; they must receive true content.
    cp "$MOUNT/weird.ncrstest" /tmp/weird.copy
    [ "$(sha < /tmp/weird.copy)" = "$WW" ] && ok "copy of sniffable file is byte-identical (no corruption)" \
        || no "copy corrupted by MIME intercept (data loss)"
else
    no "large unknown-ext file never reached backend (setup failed)"
fi

echo "→ 11b. Thumbnailer guard (the server is the only thumbnail source)"
# A desktop thumbnailer opening an uncached file would download all of it to
# render a preview. ncrs refuses such opens (no network I/O) and fetches the
# server preview instead (desktop::thumbguard). The entrypoint registered
# /usr/local/bin/ncrs-fake-thumbnailer as a GIO thumbnailer.
{ printf 'NCRS-THUMB-DATA-'; head -c 4080 /dev/urandom; } > /tmp/pic.jpg
PIC_HEX="$(head -c16 /tmp/pic.jpg | od_hex)"
# Same setup as scenario 18: make the folder through the mount, then put the
# file straight onto the backend so it was never written (or cached) locally,
# and let read-triggered revalidation surface it.
mkdir "$MOUNT/thumbdir"
for _ in $(seq 1 60); do
    [ "$(curl -s -o /dev/null -w '%{http_code}' -u "$U:$P" -X PROPFIND -H 'Depth: 0' "${URL}thumbdir/")" = "207" ] && break
    sleep 1
done
ls "$MOUNT/thumbdir" >/dev/null 2>&1
curl -s -u "$U:$P" -T /tmp/pic.jpg "${URL}thumbdir/pic.jpg" -o /dev/null
# List and stat only, never read: a read could cache it, and cached files are
# deliberately not guarded. A newly discovered entry is listed before lookups
# resolve it (ghosted first, as in scenario 18), so wait for the stat too.
seen=""
for _ in $(seq 1 90); do
    ls "$MOUNT/thumbdir" 2>/dev/null | grep -qx 'pic.jpg' && [ -e "$MOUNT/thumbdir/pic.jpg" ] && { seen=1; break; }
    sleep 1
done
if [ -n "$seen" ]; then
    if /usr/local/bin/ncrs-fake-thumbnailer -c16 "$MOUNT/thumbdir/pic.jpg" >/dev/null 2>&1; then
        no "thumbnailer read an uncached file (it would download it to render a preview)"
    else
        ok "thumbnailer refused on an uncached file (no download for a preview)"
    fi
    grep -q "THUMBGUARD refused /thumbdir/pic.jpg" "${NCRS_LOG:-/tmp/ncrs.log}" \
        && ok "refusal logged and handed to the server-preview fetch" \
        || no "no THUMBGUARD log line for the refused thumbnailer"
    # The guard is scoped to thumbnailer processes: an ordinary read of the
    # same file right after gets the real bytes.
    [ "$(head -c16 "$MOUNT/thumbdir/pic.jpg" 2>/dev/null | od_hex)" = "$PIC_HEX" ] \
        && ok "ordinary read of the same file returns real content" \
        || no "ordinary read was refused or wrong after the thumbnailer guard"
else
    no "backend-created thumbdir/pic.jpg never appeared on the mount (setup failed)"
fi

echo "→ 12. SERVER DOWN — new edit persists locally and syncs on recovery"
# The whole point of the local cache: a save must succeed and stay readable even
# when the server is unreachable, then upload itself once the server is back.
server_down
OC="offline-created $(date +%s%N)"
printf '%s' "$OC" > "$MOUNT/offline.txt"
OW="$(printf '%s' "$OC" | sha)"
# Readable from the mount immediately, straight from local staging, while down.
wait_fuse_sha offline.txt "$OW" 15 \
    && ok "offline-created file readable on mount while server down" \
    || no "offline-created file not readable while server down (data loss)"
# Still readable after a moment (staging is durable, not a one-shot buffer).
sleep 3
[ "$(fuse_sha offline.txt)" = "$OW" ] \
    && ok "offline-created file stays readable during the outage" \
    || no "offline-created file became unreadable during the outage (data loss)"
server_up
wait_dav_sha offline.txt "$OW" 120 \
    && ok "offline edit synced to backend after recovery" \
    || no "offline edit never synced after recovery (data loss)"

echo "→ 13. SERVER DOWN — overwrite reads back new content, not stale cache"
# Create + fully sync a baseline, read it (to populate any read-cache), then
# overwrite it while the server is down: the mount must return the NEW bytes
# (the pending local write wins over the cached/server copy), and sync later.
BC="baseline $(date +%s%N)"
printf '%s' "$BC" > "$MOUNT/over.txt"
BW="$(printf '%s' "$BC" | sha)"
wait_dav_sha over.txt "$BW" 60 || no "baseline for overwrite never synced (setup failed)"
cat "$MOUNT/over.txt" >/dev/null 2>&1   # populate read path / any cache
server_down
NC2="overwritten-offline $(date +%s%N)"
printf '%s' "$NC2" > "$MOUNT/over.txt"
NW2="$(printf '%s' "$NC2" | sha)"
wait_fuse_sha over.txt "$NW2" 15 \
    && ok "overwrite reads back new content while server down" \
    || no "overwrite returned stale content while server down (staleness/data loss)"
server_up
wait_dav_sha over.txt "$NW2" 120 \
    && ok "offline overwrite synced to backend after recovery" \
    || no "offline overwrite never synced (data loss)"

echo "→ 14. SERVER DOWN — large offline write fully readable from local staging"
# Guards that staging serves arbitrary offsets (multi-chunk reads), not just the
# first bytes, while the server is unreachable — then round-trips on recovery.
server_down
head -c 3145728 /dev/urandom > /tmp/offbig.bin   # 3 MiB
OBW="$(sha < /tmp/offbig.bin)"
cp /tmp/offbig.bin "$MOUNT/offbig.bin"
wait_fuse_sha offbig.bin "$OBW" 30 \
    && ok "large offline file fully readable from staging while server down" \
    || no "large offline file not fully readable while server down (data loss)"
server_up
wait_dav_sha offbig.bin "$OBW" 150 \
    && ok "large offline file synced after recovery" \
    || no "large offline file never synced (data loss)"

echo "→ 15. LOCK-FILE create-then-delete race (LibreOffice .~lock…# pattern)"
# LibreOffice creates a lock file when opening a document and deletes it a moment
# later on close. The DELETE can fire while the lock file's own PUT is still in
# flight; Nextcloud's transactional locking holds the file locked during upload,
# so a racing DELETE comes back 423 and — before the drain fix — surfaced
# "delete failed: resource locked" and left the lock file orphaned on the server.
# The guarantee under test: after a rapid create+delete, the lock file leaves NO
# trace on the backend (no orphan) and none on the mount. Repeat a few times to
# widen the window onto the in-flight PUT.
lock_orphans=0
for i in $(seq 1 5); do
    LF=".~lock.doc-$i-$(date +%s%N).odt#"
    printf 'LOGO,1000,%s' "$i" > "$MOUNT/$LF"   # tiny, like a real LO lock file
    rm -f "$MOUNT/$LF"                            # delete immediately, racing the PUT
    # The delete (possibly deferred past an in-flight PUT then retried) must fully
    # propagate: no lock file may remain on the backend.
    wait_dav_gone "$LF" 60 || { lock_orphans=$((lock_orphans + 1)); echo "    ✗ orphan left on backend: $LF"; }
    wait_fuse_gone "$LF" 30 || { lock_orphans=$((lock_orphans + 1)); echo "    ✗ still visible on mount: $LF"; }
done
[ "$lock_orphans" = 0 ] \
    && ok "rapid lock-file create+delete leaves no orphan on backend or mount (no 423 stall)" \
    || no "$lock_orphans lock-file create+delete races left orphans (resource-locked regression)"

echo "→ 16. SERVER-SIDE EDIT — stale local copy must not be served at the new size"
# Reproduces the "file is corrupt" report: create + sync a file (so ncrs keeps a
# local copy), read it (populate the read fast-path), then overwrite it DIRECTLY
# on the backend with different, LARGER content — exactly what Nextcloud Office
# does when you edit the file server-side. The mount must then serve the NEW
# bytes (matching the new size), never the stale local copy at the refreshed
# size, which is what makes a ZIP-based odt/xlsx read back as corrupt.
#
# Observing a server-made change requires the mount to notice the file's new
# change_token, which on a plain WebDAV server (rclone, no notify_push and dir
# ETags that don't change on a child edit) is best-effort — same limitation as
# scenario 9 — so a non-observation here is informational, NOT a suite failure.
# On Nextcloud (changing dir ETags) this actively verifies the fix.
SC="server-edit-baseline $(date +%s%N)"
printf '%s' "$SC" > "$MOUNT/sedit.txt"
SW="$(printf '%s' "$SC" | sha)"
if wait_dav_sha sedit.txt "$SW" 60 && wait_fuse_sha sedit.txt "$SW" 45; then
    cat "$MOUNT/sedit.txt" >/dev/null 2>&1   # populate the read fast-path / local copy
    # Overwrite server-side with different, larger content (new size + new ETag).
    NCS="server-edited-larger-$(date +%s%N)-$(head -c 4096 /dev/urandom | base64 | tr -d '\n')"
    printf '%s' "$NCS" > /tmp/sedit.new
    NSW="$(sha < /tmp/sedit.new)"
    curl -s -u "$U:$P" -T /tmp/sedit.new "${URL}sedit.txt" -o /dev/null
    if wait_fuse_sha sedit.txt "$NSW" 90; then
        # Whatever we got must be the COMPLETE new version — never a stale-content /
        # new-size mix (the corruption). wait_fuse_sha already hashed the full read.
        ok "server-side edit served as complete new content (no stale copy at new size)"
    else
        GOT="$(fuse_sha sedit.txt)"
        if [ "$GOT" = "$SW" ]; then
            echo "  ⓘ server-side edit not observed on mount — expected on plain WebDAV"
            echo "    (no notify_push / unchanged dir ETag); not a corruption/data-loss failure"
        else
            no "mount served neither old nor new full content (got ${GOT:0:12}…) — corruption"
        fi
    fi
else
    no "baseline for server-side edit never synced (setup failed)"
fi

echo "→ 17. NETWORK BLACKHOLED (packets dropped) — mount fails fast, never hangs the save"
# Reproduces the original report: open a document, drop the network, save. With a
# blackholed server, connect() gets no answer and hangs until it times out. Before
# the fix, a save that does a read-modify-write (LibreOffice re-reads the .ods
# during save) blocked on the full 120s download timeout — one or more times — so
# the app froze "until the network came back"; the daemon only noticed it was
# offline on its next 30s poll. (Note: unlike the server_down scenarios above,
# this DROPs packets rather than sending an RST, so connect() actually hangs — the
# condition connect_timeout + the eager offline flip are meant to bound.)
#
# The files must be mount-VISIBLE but NOT locally cached, so a read genuinely hits
# the network. Creating them on the backend directly (curl) would not do: a plain
# WebDAV server does not invalidate the mount's dir cache on a server-side child
# create (same limitation as scenarios 9/16), so the mount would not even see
# them. Instead create them THROUGH the mount, let them sync, then drop the local
# staging — a not-kept file (auto_keep_cached_files: false) is re-downloaded on the
# next read. Do NOT read them before the blackhole, or the read would cache them.
BH1="blackhole-1-$(date +%s%N)"; BH2="blackhole-2-$(date +%s%N)"
printf '%s' "$BH1" > "$MOUNT/bh1.txt"; BW1="$(printf '%s' "$BH1" | sha)"
printf '%s' "$BH2" > "$MOUNT/bh2.txt"; BW2="$(printf '%s' "$BH2" | sha)"
if wait_dav_sha bh1.txt "$BW1" 90 && wait_dav_sha bh2.txt "$BW2" 90; then
    ls "$MOUNT" >/dev/null 2>&1; sleep 2   # drop staging so a read must hit the network

    server_blackhole
    # (a) First read of a non-cached file must return within a bounded time
    # (connect timeout + a few retries), not hang out the full 120s download
    # timeout. It fails (offline, uncached) — that is fine; the guarantee is that
    # it RETURNS, and doing so flips the daemon offline.
    t0=$(date +%s)
    timeout 60 cat "$MOUNT/bh1.txt" >/dev/null 2>&1
    e1=$(( $(date +%s) - t0 ))
    if [ "$e1" -lt 60 ]; then
        ok "read of non-cached file returns fast while blackholed (${e1}s, not a 120s hang)"
    else
        no "read hung past the bound while blackholed (${e1}s) — offline fast-fail regression"
    fi
    # (b) With the offline flag now engaged by (a), the next read must be near-
    # instant — the signature of the eager flip: do_range_read_stream and the
    # ensure_file_cached fallback both short-circuit instead of each paying the
    # connect wait + retries.
    t0=$(date +%s)
    timeout 30 cat "$MOUNT/bh2.txt" >/dev/null 2>&1
    e2=$(( $(date +%s) - t0 ))
    if [ "$e2" -lt 10 ]; then
        ok "second read short-circuits once offline engaged (${e2}s) — flag flipped eagerly"
    else
        no "second read still blocked (${e2}s) — offline flag did not engage on the first failure"
    fi
    server_unblackhole
    # Mount recovers: once the connectivity monitor re-probes and clears offline,
    # the file re-downloads and reads back byte-identical.
    wait_fuse_sha bh1.txt "$BW1" 120 \
        && ok "blackholed file readable again (byte-identical) after connectivity restored" \
        || no "file not readable after connectivity restored (recovery regression)"
else
    no "blackhole test files never synced to backend (setup failed)"
fi

echo "→ 18. REMOTE ADD in a cached subdir — read-triggered revalidation surfaces it"
# The missed-notify-push case: a file lands on the server in a directory whose
# listing this client already cached, and no push event ever arrives (client was
# offline when it happened / plain WebDAV has no notify_push). Every readdir now
# probes the directory's own ETag in the background and re-lists on mismatch, so
# simply looking at the directory must surface the file — well before the dir
# TTL (10s in this suite) would have, and without a cache purge.
#
# The mechanism under test needs the server to change the dir ETag when a direct
# child is added (rclone derives ETags from mtime, and a child create touches the
# dir mtime). Verify that precondition first; if this server doesn't provide it,
# report informationally like scenarios 9/16 instead of failing the suite.
mkdir "$MOUNT/revdir"
# MKCOL propagates asynchronously — wait for the collection on the backend
# before caching its listing and PUTting into it.
for _ in $(seq 1 30); do
    [ "$(curl -s -o /dev/null -w '%{http_code}' -u "$U:$P" -X PROPFIND -H 'Depth: 0' "${URL}revdir/")" = "207" ] && break
    sleep 1
done
# Cache the (empty) listing from the server. A listing served before the MKCOL
# lands comes from the local placeholder, which carries no ETag and is dropped
# once the MKCOL succeeds; the next look then re-lists instead of probing. Keep
# looking until the daemon has fetched the folder's listing itself.
for _ in $(seq 1 30); do
    ls "$MOUNT/revdir" >/dev/null 2>&1
    grep -q "PROPFIND_STREAM /revdir done" "${NCRS_LOG:-/tmp/ncrs.log}" && break
    sleep 0.5
done
T_CACHE=$(date +%s)
ETAG_BEFORE="$(curl -s -u "$U:$P" -X PROPFIND -H 'Depth: 0' "${URL}revdir/" | grep -o '<[^>]*getetag>[^<]*' | head -1)"
RVC="revalidate-me $(date +%s%N)"
printf '%s' "$RVC" > /tmp/rev.txt
RVW="$(printf '%s' "$RVC" | sha)"
curl -s -u "$U:$P" -T /tmp/rev.txt "${URL}revdir/rev.txt" -o /dev/null
ETAG_AFTER="$(curl -s -u "$U:$P" -X PROPFIND -H 'Depth: 0' "${URL}revdir/" | grep -o '<[^>]*getetag>[^<]*' | head -1)"
sleep 3                                      # get past the just-fetched suppression
seen=""
for _ in $(seq 1 25); do
    ls "$MOUNT/revdir" >/dev/null 2>&1       # each read schedules a background probe
    if [ "$(fuse_sha revdir/rev.txt)" = "$RVW" ]; then seen=$(( $(date +%s) - T_CACHE )); break; fi
    sleep 1
done
# What proves the read-triggered path did this is the daemon's own diff line, not
# the clock: an addition the background refresh discovers is deliberately ghosted
# for GHOST_TTL (10s, GhostKind::HiddenAdd) so a file another client is still
# uploading is never shown half-written. Visibility therefore cannot beat
# GHOST_TTL, which is why a sub-TTL bound is the wrong assertion — it can only be
# met by never ghosting. The bound here is that constant plus slack.
if [ -n "$seen" ] && grep -q "proactive_refresh: .* added to /revdir" "${NCRS_LOG:-/tmp/ncrs.log}"; then
    ok "server-added file surfaced by read-triggered revalidation (${seen}s, ghosted first)"
elif [ -n "$seen" ]; then
    no "server-added file appeared after ${seen}s but no revalidation diff logged — TTL expiry, not revalidation"
elif [ "$ETAG_BEFORE" = "$ETAG_AFTER" ]; then
    echo "  ⓘ server did not change the dir ETag on child add (before==after) — the"
    echo "    revalidation probe has nothing to observe on this server; not a failure"
else
    no "dir ETag changed (${ETAG_BEFORE} → ${ETAG_AFTER}) but mount never surfaced the file (stale cache)"
fi

echo "→ 19. MAX-STALE WINDOW — the first listing after the window is already current"
# dir_cache_max_stale_mins (1 in this suite) promises that a directory nobody has
# looked at for that long is checked against the server *before* its listing is
# shown — rather than served stale and corrected on a second look, which is what
# scenario 18 covers. Three directories are prepared and aged past the window with
# a single wait, then each is listed exactly once:
#   staleadd   — changed on the server behind the daemon's back
#   stalesame  — untouched: must still be served from cache after one cheap probe
#   staledown  — listed while the server is unreachable
MAXSTALE_WINDOW=60                       # dir_cache_max_stale_mins: 1
NCRS_LOG="${NCRS_LOG:-/tmp/ncrs.log}"
STALE_DIRS="staleadd stalesame staledown"

# The directory ETag the daemon's pre-serve probe compares against. This must be
# the *named* property request the daemon sends: an allprop PROPFIND omits getetag
# on some servers (rclone), which looks identical to "no ETag" but isn't the same
# question. Empty output means the server exposes no ETag for the collection, so
# the daemon holds no token for it and can only re-list.
dav_dir_etag() {
    curl -s -u "$U:$P" -X PROPFIND -H 'Depth: 0' -H 'Content-Type: application/xml' \
        --data '<?xml version="1.0"?><d:propfind xmlns:d="DAV:"><d:prop><d:getetag /></d:prop></d:propfind>' \
        "${URL}$1/" \
        | sed -n 's#.*<[Dd]:getetag>\([^<]*\)</[Dd]:getetag>.*#\1#p' | head -1
}

for d in $STALE_DIRS; do
    mkdir -p "$MOUNT/$d"
    # MKCOL propagates asynchronously — wait for the collection before seeding it.
    for _ in $(seq 1 30); do
        [ "$(curl -s -o /dev/null -w '%{http_code}' -u "$U:$P" -X PROPFIND -H 'Depth: 0' "${URL}$d/")" = "207" ] && break
        sleep 1
    done
    printf '%s' "seed-$d" > "$MOUNT/$d/seed.txt"
    wait_dav_sha "$d/seed.txt" "$(printf '%s' "seed-$d" | sha)" 45 >/dev/null
done
# Cache all three listings. This is when each max-stale window starts, so nothing
# below may list these directories again until the wait is over.
for d in $STALE_DIRS; do ls "$MOUNT/$d" >/dev/null 2>&1; done

ETAG_BEFORE="$(dav_dir_etag staleadd)"
SAME_ETAG="$(dav_dir_etag stalesame)"
# The change the daemon cannot possibly know about: a direct PUT to the backend,
# no push event, no local write.
SA="ncrs-e2e late-arrival $(date +%s%N)"
printf '%s' "$SA" > /tmp/staleadd.txt
SAW="$(printf '%s' "$SA" | sha)"
curl -s -u "$U:$P" -T /tmp/staleadd.txt "${URL}staleadd/late.txt" -o /dev/null
ETAG_AFTER="$(dav_dir_etag staleadd)"
# Precondition: without this the assertions below would blame the daemon for a
# file that never reached the server.
if [ "$(dav_code staleadd/late.txt)" = "200" ]; then
    ok "setup: direct backend PUT landed (invisible to the daemon)"
else
    no "setup: direct backend PUT never landed (HTTP $(dav_code staleadd/late.txt))"
fi

echo "    (aging the three cached listings past ${MAXSTALE_WINDOW}s — no listing during the wait)"
sleep $((MAXSTALE_WINDOW + 5))

LOG_MARK=$(( $([ -f "$NCRS_LOG" ] && wc -l < "$NCRS_LOG" || echo 0) + 1 ))
log_since() { tail -n +"$LOG_MARK" "$NCRS_LOG" 2>/dev/null; }

# ── The guarantee: correct on the FIRST listing, not the second ──────────────
FIRST_LS="$(ls "$MOUNT/staleadd" 2>/dev/null)"
if [ -z "$ETAG_AFTER" ] || [ "$ETAG_BEFORE" != "$ETAG_AFTER" ]; then
    # Either the server exposes no collection ETag — so the daemon holds no token
    # and must re-list — or it moved. Both mean the change is observable, so the
    # first listing has no excuse for being stale.
    if printf '%s\n' "$FIRST_LS" | grep -qx 'late.txt'; then
        ok "server-added file present in the FIRST listing after the window"
    else
        no "first listing after the window was stale (no late.txt) — window not enforced"
    fi
    # Content, not just the name: the re-list must carry real entries.
    wait_fuse_sha staleadd/late.txt "$SAW" 30 \
        && ok "server-added file reads back byte-identical through the mount" \
        || no "server-added file not readable through the mount (data loss)"
else
    echo "  ⓘ server keeps a collection ETag but did not move it on child add"
    echo "    (${ETAG_BEFORE}) — the pre-serve probe has nothing to observe here"
fi
# Prove the window is what corrected it, rather than a coincidental refresh.
if log_since | grep -q "DIR_HARD_EXPIRED $MOUNT/staleadd\|DIR_HARD_EXPIRED /staleadd"; then
    ok "aged listing was withheld pending a re-check (DIR_HARD_EXPIRED logged)"
else
    no "no DIR_HARD_EXPIRED for /staleadd — the aged listing was served unchecked"
fi

# ── The cost: an unchanged directory must not pay a full re-list ─────────────
LS_SAME="$(ls "$MOUNT/stalesame" 2>/dev/null)"
if printf '%s\n' "$LS_SAME" | grep -qx 'seed.txt'; then
    ok "aged but unchanged listing served intact"
else
    no "aged unchanged listing came back wrong: [${LS_SAME}]"
fi
if [ -z "$SAME_ETAG" ]; then
    echo "  ⓘ server exposes no collection ETag, so the daemon holds no token and"
    echo "    re-lists instead of probing — correct here, just not the cheap path"
elif log_since | grep -q "LIST_ETAG_CONFIRMED.*stalesame"; then
    ok "unchanged dir confirmed by etag probe alone (no full re-list)"
else
    no "unchanged dir was fully re-listed — the etag pre-check did not run"
fi

# ── The failure mode: past the window with the server unreachable ───────────
server_down
LS_DOWN="$(ls "$MOUNT/staledown" 2>/dev/null)"; rc=$?
server_up
if [ "$rc" -eq 0 ] && printf '%s\n' "$LS_DOWN" | grep -qx 'seed.txt'; then
    ok "aged listing still served from cache while the server was unreachable"
else
    no "aged listing failed with the server unreachable (rc=$rc) — a blip became an error"
fi
sleep 3   # let the connectivity monitor clear the offline flag before scenario 20

echo "→ 20. ORPHANED WRITE after a busy-mount force-detach is adopted, not refused"
# Reproduces the historical bug: something (e.g. an app with an open document)
# keeps the mount busy, a plain "fusermount -u" therefore can't complete, and
# the mount gets force-detached anyway (what the GUI used to do as a silent
# fallback) — exposing the real underlying directory while still "in use". A
# save landing on that exposed directory used to permanently block the next
# mount ("not empty"); it must now be adopted as a pending upload instead.
ADOPT_DIR="adopt19"
mkdir -p "$MOUNT/$ADOPT_DIR"
C19="ncrs-e2e orphan-setup $(date +%s%N)"
printf '%s' "$C19" > "$MOUNT/$ADOPT_DIR/orphan.txt"
W19="$(printf '%s' "$C19" | sha)"
wait_fuse_sha "$ADOPT_DIR/orphan.txt" "$W19" && ok "setup file synced before the forced detach" \
    || no "setup file never synced — scenario precondition failed"

DAEMON_PID="$(pgrep -f 'ncrs --config' | head -1)"
if [ -z "$DAEMON_PID" ]; then
    no "could not find the running ncrs daemon — cannot run this scenario"
else
    # Hold the mount busy the way an app with an open document would: a
    # process with its cwd inside the mount makes a plain unmount EBUSY.
    ( cd "$MOUNT" && sleep 25 ) &
    HOLDER=$!
    sleep 1

    fusermount3 -u "$MOUNT" 2>/dev/null \
        && no "clean unmount succeeded while busy (scenario precondition failed)" \
        || echo "    (clean unmount correctly refused: mount is busy)"
    fusermount3 -uz "$MOUNT" 2>/dev/null || umount -l "$MOUNT" 2>/dev/null || true
    kill "$DAEMON_PID" 2>/dev/null || true
    for _ in $(seq 1 20); do
        kill -0 "$DAEMON_PID" 2>/dev/null || break
        sleep 1
    done

    # The write a belated save from that still-open document would make —
    # straight onto the now-exposed real directory, bypassing ncrs entirely.
    # The FUSE-created subdirectory only existed on the server, so recreate
    # it on the real filesystem before writing (mirrors a real app whose OS
    # directory cache still resolves the path after a lazy unmount).
    mkdir -p "$MOUNT/$ADOPT_DIR"
    NEW19="ncrs-e2e new-during-detach $(date +%s%N)"
    printf '%s' "$NEW19" > "$MOUNT/$ADOPT_DIR/new_during_detach.txt"
    WN19="$(printf '%s' "$NEW19" | sha)"

    kill "$HOLDER" 2>/dev/null || true
    wait "$HOLDER" 2>/dev/null || true

    RUST_LOG="${RUST_LOG:-info}" ncrs --config "$HOME/.config/ncrs/config.yaml" >/tmp/ncrs_restart19.log 2>&1 &
    DAEMON=$!

    remounted=1
    for _ in $(seq 1 30); do
        mountpoint -q "$MOUNT" && { remounted=0; break; }
        kill -0 "$DAEMON" 2>/dev/null || break
        sleep 1
    done
    if [ "$remounted" -eq 0 ]; then
        ok "ncrs remounted on the previously-orphaned mount point instead of refusing"
    else
        no "ncrs failed to remount after the orphaned write (adoption not working)"
        tail -60 /tmp/ncrs_restart19.log
    fi

    wait_dav_sha "$ADOPT_DIR/new_during_detach.txt" "$WN19" 60 \
        && ok "orphaned write reached the backend (adopted, not lost)" \
        || no "orphaned write never reached the backend (data loss)"
fi

echo "→ 21. CONNECTIVITY BLIP — uncached dir lists after the blip instead of appearing empty"
# Reproduces the field report (2026-09-01): a momentary outage (a QUIC-only path
# before the HTTP/2 demotion, a Wi-Fi roam) flips the daemon offline; a directory
# whose listing is not in the bounded dir cache then errored instantly, which a
# file manager renders as a folder with 0 items — and caches. An uncached listing
# arriving inside the offline grace window must wait the blip out and then really
# list. Setup: mkdir through the mount patches only the PARENT's cached listing,
# so the new dir is visible while its own listing stays uncached; the file inside
# it is created backend-side so only a real PROPFIND can ever return it.
BLIPDIR="blipdir-$(date +%s%N)"
mkdir "$MOUNT/$BLIPDIR"
blip_ready=1
for _ in $(seq 1 60); do
    [ "$(dav_code "$BLIPDIR/")" != "404" ] && { blip_ready=0; break; }
    sleep 1
done
printf 'blip payload' > /tmp/blip.txt
curl -s -u "$U:$P" -T /tmp/blip.txt "${URL}${BLIPDIR}/blip.txt" -o /dev/null
if [ "$blip_ready" -ne 0 ] || [ "$(dav_code "$BLIPDIR/blip.txt")" = "404" ]; then
    no "scenario 21 setup failed (MKCOL or backend PUT never landed)"
else
    # An uncached file whose failed read flips the offline flag eagerly (same
    # trick as scenario 17); server_down RSTs so the flip is near-instant and
    # the grace window opens at a known moment.
    BF="blipflip-$(date +%s%N)"
    printf '%s' "$BF" > "$MOUNT/bflip21.txt"; WBF="$(printf '%s' "$BF" | sha)"
    wait_dav_sha bflip21.txt "$WBF" 90
    ls "$MOUNT" >/dev/null 2>&1; sleep 2   # drop staging so the read must hit the network

    server_down
    # The failed read flips the offline flag ~2-3s in (after its RST retries) and
    # then blocks out its OWN 15s read grace — so it must run in the background:
    # waiting for it to return would consume the entire offline grace window
    # before the listing under test even starts.
    timeout 30 cat "$MOUNT/bflip21.txt" >/dev/null 2>&1 &
    FLIP21=$!
    sleep 4                       # flag is set by now; the grace window is freshly opened
    ( sleep 3; server_up ) &
    UNBLIP=$!
    t0=$(date +%s)
    LS_BLIP="$(timeout 40 ls "$MOUNT/$BLIPDIR" 2>/dev/null)"; rc=$?
    eb=$(( $(date +%s) - t0 ))
    kill "$FLIP21" 2>/dev/null; wait "$FLIP21" 2>/dev/null
    wait "$UNBLIP" 2>/dev/null || true
    if [ "$rc" -eq 0 ] && printf '%s\n' "$LS_BLIP" | grep -qx 'blip.txt'; then
        ok "uncached dir listed through a connectivity blip (waited ${eb}s, not \"0 items\")"
    else
        no "uncached dir failed across a blip (rc=$rc after ${eb}s: [$LS_BLIP]) — renders as an empty folder"
    fi
    sleep 3   # let the monitor settle online before the suite ends
fi

echo "→ 23. MID-WRITE FLUSHES — closing an inherited fd must not commit a half-written file"
# Every close() of an fd referring to an open file sends FUSE FLUSH, including a child that
# inherited the writer's fd and exits (shell groups; helpers a file manager forks mid-copy).
# v0.1.74 treated each FLUSH as the end of the file: it uploaded the partial snapshot and
# deleted the staging file, so the server kept a truncated or scrambled copy while every
# writer reported success. Only RELEASE (the last close) may commit. The 25 MiB cases also
# cover a server without chunked uploads (rclone): the first chunk session fails with no
# bytes sent, so the handle must fall back to a whole-file upload instead of EIO.
mw_write() {  # <src> <dest>: one dd child per MiB, each exiting mid-write; 1 if any child failed
    local src=$1 dest=$2 i rc=0 n=$(( $(stat -c %s "$1") / 1048576 ))
    { for i in $(seq 0 $((n - 1))); do
          dd if="$src" bs=1M skip="$i" count=1 status=none || rc=1
      done; } > "$dest" || rc=1
    return "$rc"
}
for spec in mw6:6:multi mw25:25:multi cp25:25:cp; do
    IFS=: read -r name mib mode <<<"$spec"
    src="/tmp/$name.src"
    head -c $((mib * 1048576)) /dev/urandom > "$src"
    want="$(sha < "$src")"
    if [ "$mode" = multi ]; then mw_write "$src" "$MOUNT/$name.bin"; else cp "$src" "$MOUNT/$name.bin"; fi
    wrc=$?
    if wait_dav_sha "$name.bin" "$want" 90; then
        if [ "$wrc" -eq 0 ]; then
            ok "$name ($mode, ${mib} MiB): server copy byte-identical, every writer succeeded"
        else
            no "$name ($mode, ${mib} MiB): server copy intact but a writer failed (rc=$wrc)"
        fi
    else
        got=$(curl -s -u "$U:$P" "${URL}$name.bin" | wc -c)
        no "$name ($mode, ${mib} MiB): server copy is not what was written (writers rc=$wrc, server ${got} of $((mib * 1048576)) bytes)"
    fi
done

echo "→ 24. APPEND to a file with no local copy — the existing content must be kept"
# Opening an uncached file for writing without O_TRUNC left the staging file empty, so an
# append (its writes land at the old size) uploaded a zero-filled prefix.
AB="append-base $(date +%s%N)"
printf '%s' "$AB" > "$MOUNT/append.txt"
if wait_dav_sha append.txt "$(printf '%s' "$AB" | sha)" 60; then
    printf '%s' "-tail" >> "$MOUNT/append.txt"
    if wait_dav_sha append.txt "$(printf '%s-tail' "$AB" | sha)" 60; then
        ok "append to an uncached file kept its existing content on the server"
    else
        no "append to an uncached file lost its existing content (server: $(curl -s -u "$U:$P" "${URL}append.txt" | od -An -c | head -1))"
    fi
else
    no "setup: append.txt never reached the backend"
fi

echo "→ 25. RENAME while still open — the upload follows the new name"
# GLib's save writes a temp file and renames it before its last close. The upload happens
# at that close, so it must land under the new name, and the MOVE of a temp that was never
# uploaded must not 404 (the close comes after the old 30 s MOVE wait expired).
exec 8>"$MOUNT/rn_tmp.txt"
printf 'renamed-while-open' >&8
mv "$MOUNT/rn_tmp.txt" "$MOUNT/rn_final.txt"
sleep 35
exec 8>&-
if wait_dav_sha rn_final.txt "$(printf 'renamed-while-open' | sha)" 60; then
    ok "file renamed while open was uploaded under its new name"
else
    no "file renamed while open is missing under its new name (HTTP $(dav_code rn_final.txt))"
fi
sleep 2
[ "$(dav_code rn_tmp.txt)" = "404" ] && ok "no temp name left behind on the server" \
    || no "the temp name was re-created on the server (HTTP $(dav_code rn_tmp.txt))"

echo "→ 26. RAPID RE-SAVES of one file — the last save wins, no conflicted copies"
# Every save commits its own upload; they must reach the server in save order, and a save
# opened before the previous upload finished must chain its etag instead of 412-ing.
mkdir "$MOUNT/resave"
printf 'v0' > "$MOUNT/resave/doc.txt"
wait_dav_sha resave/doc.txt "$(printf 'v0' | sha)" 60 >/dev/null || no "setup: resave/doc.txt never reached the backend"
for i in 1 2 3 4 5 6; do
    head -c 3000000 /dev/urandom > "/tmp/resave_$i"
    cp "/tmp/resave_$i" "$MOUNT/resave/doc.txt"
done
if wait_dav_sha resave/doc.txt "$(sha < /tmp/resave_6)" 90; then
    ok "six back-to-back saves: the server holds the last one"
else
    got=$(dav_sha resave/doc.txt); which="an older save or partial file"
    for i in 1 2 3 4 5; do [ "$got" = "$(sha < "/tmp/resave_$i")" ] && which="save #$i"; done
    no "six back-to-back saves: the server ended with ${which}, not the last save"
fi
sleep 3
conflicts=$(curl -s -u "$U:$P" -X PROPFIND -H 'Depth: 1' "${URL}resave/" | grep -o 'conflicted' | wc -l)
[ "$conflicts" = 0 ] && ok "no conflicted copies from back-to-back saves" \
    || no "$conflicts conflicted cop(ies) created by back-to-back saves"

echo "→ 27. NEW FOLDER listed and filled right away — no 'not found', children land inside"
# mkdir replied before its MKCOL reached the server and cached no listing, so listing the
# folder straight away PROPFINDed a folder the server did not have yet.
nf_fail=0
for i in 1 2 3 4 5 6 7 8; do
    d="newfolder_$i"
    mkdir "$MOUNT/$d" && ls "$MOUNT/$d" >/dev/null 2>&1 && printf 'child %s' "$i" > "$MOUNT/$d/c.txt" \
        || nf_fail=$((nf_fail + 1))
done
landed=0
for i in 1 2 3 4 5 6 7 8; do
    wait_dav_sha "newfolder_$i/c.txt" "$(printf 'child %s' "$i" | sha)" 60 && landed=$((landed + 1))
done
[ "$nf_fail" = 0 ] && ok "8 new folders listed and written immediately without errors" \
    || no "$nf_fail of 8 new folders failed to list or accept a file right after mkdir"
[ "$landed" = 8 ] && ok "all 8 files uploaded into their just-created folders" \
    || no "only $landed of 8 files reached their just-created folders"

echo "→ 22. SERVER EDIT of a KEPT file — the refresh that evicts it must not freeze the mount"
# Regression for the v0.1.73 freeze: a read-triggered refresh that found a locally
# cached (kept) file changed on the server evicted it while holding the cache lock,
# then re-locked that lock to persist the file cache. The refresh thread deadlocked
# on itself and every later getattr/lookup blocked forever. The other scenarios never
# reached it because nothing is kept (auto_keep_cached_files: false), so the eviction
# found no file-cache entry. A sibling is added alongside the edit so the dir ETag
# moves and the refresh gets past its unchanged-ETag short-circuit.
# Every mount access here is SIGKILL-bounded: a hang must fail the suite, not stall it.
SOCK="$(ls "${XDG_RUNTIME_DIR:-/nonexistent}/ncrs.sock" /tmp/ncrs-"$(id -u)"/ncrs.sock 2>/dev/null | head -1)"
ipc() { printf '%s\n' "$1" | timeout 10 nc -U -N "$SOCK" 2>/dev/null; }
bounded() { timeout -s KILL "$@"; }   # exit 137 = the call was still blocked in the kernel
# Scenario 20 restarts the daemon into its own log, so search every daemon log.
daemon_logs() { cat "$NCRS_LOG" /tmp/ncrs_restart19.log 2>/dev/null; }
mount_hung() {
    no "$1 — mount frozen (FUSE request never answered)"
    DPID="$(pgrep -x ncrs | head -1)"
    for t in /proc/"$DPID"/task/*; do
        echo "    $(cat "$t/comm" 2>/dev/null): $(head -4 "$t/stack" 2>/dev/null | awk '{print $2}' | tr '\n' ' ')"
    done
    pkill -9 -x ncrs   # release the blocked requests so teardown cannot hang too
    echo
    echo "e2e results: ${PASS} passed, ${FAIL} failed"
    exit 1
}
mkdir "$MOUNT/relock"
for _ in $(seq 1 30); do
    [ "$(curl -s -o /dev/null -w '%{http_code}' -u "$U:$P" -X PROPFIND -H 'Depth: 0' "${URL}relock/")" = "207" ] && break
    sleep 1
done
RK="kept-baseline $(date +%s%N)"
printf '%s' "$RK" > "$MOUNT/relock/kept.txt"
wait_dav_sha relock/kept.txt "$(printf '%s' "$RK" | sha)" 60 >/dev/null || no "setup: kept.txt never reached the backend"
# The backend has the bytes a moment before the daemon has handled its own PUT
# response, which sets the file's status to synced. A KEEP answered in between
# is overwritten, and STATUS never says kept. Wait for the daemon's side too.
for _ in $(seq 1 30); do
    daemon_logs | grep -q "PUT /relock/kept.txt → new etag" && break
    sleep 1
done
[ -S "$SOCK" ] && [ "$(ipc "KEEP $MOUNT/relock/kept.txt")" = "ok" ] || no "setup: IPC KEEP was not accepted (socket: ${SOCK:-none})"
kept=""
for _ in $(seq 1 60); do
    kept_status="$(ipc "STATUS $MOUNT/relock/kept.txt")"
    if [ "$kept_status" = "kept" ] \
        && [ -n "$(find "$HOME/.cache/ncrs" -path '*/kept/relock/kept.txt' -type f 2>/dev/null)" ]; then
        kept=1; break
    fi
    sleep 1
done
[ -n "$kept" ] && ok "setup: file pinned locally (file-cache entry + kept copy on disk)" \
    || {
        no "setup: KEEP never produced a kept local copy (last STATUS: ${kept_status:-none})"
        echo "    kept copies on disk: $(find "$HOME/.cache/ncrs" -path '*/kept/relock/*' -type f 2>/dev/null | tr '\n' ' ')"
        daemon_logs | grep -E "relock/kept\.txt|keep failed|KEEP callback" | tail -15 | sed 's/^/    /'
    }
sleep 11                                          # age the listing past the 10s dir TTL
bounded 20 ls "$MOUNT/relock" >/dev/null 2>&1     # synchronous re-list: this is old_snap
[ $? -eq 137 ] && mount_hung "re-list before the server edit"
RKN="server-edited $(date +%s%N) $(head -c 2048 /dev/urandom | base64 | tr -d '\n')"
printf '%s' "$RKN" > /tmp/relock.new
curl -s -u "$U:$P" -T /tmp/relock.new "${URL}relock/kept.txt" -o /dev/null
printf 'sibling' > /tmp/relock.sib
curl -s -u "$U:$P" -T /tmp/relock.sib "${URL}relock/sibling.txt" -o /dev/null
sleep 3                                           # past the just-listed suppression, inside the TTL
evicted=""
for _ in $(seq 1 15); do
    bounded 20 ls "$MOUNT/relock" >/dev/null 2>&1  # each read schedules the background refresh
    [ $? -eq 137 ] && mount_hung "listing while the refresh ran"
    if daemon_logs | grep -q "file_cache: evicted stale /relock/kept.txt"; then evicted=1; break; fi
    sleep 1
done
if [ -n "$evicted" ]; then
    ok "refresh evicted the kept file's stale copy (the formerly deadlocking path ran)"
else
    no "refresh never evicted the kept file — scenario did not reach the regression path"
    daemon_logs | grep "relock" | grep -v "READDIR\|LIST_CACHED" | tail -15 | sed 's/^/    /'
fi
sleep 2
bounded 20 stat "$MOUNT/relock/kept.txt" >/dev/null 2>&1; rc=$?
[ "$rc" -eq 137 ] && mount_hung "getattr after the eviction"
bounded 20 ls "$MOUNT/new_after_relock_probe" >/dev/null 2>&1; rc=$?
[ "$rc" -eq 137 ] && mount_hung "lookup after the eviction"
ok "getattr and lookup still answered after the eviction (no cache-lock deadlock)"
got=""
for _ in $(seq 1 45); do
    got="$(bounded 20 sha256sum "$MOUNT/relock/kept.txt" 2>/dev/null | awk '{print $1}')"
    [ "$got" = "$(sha < /tmp/relock.new)" ] && break
    sleep 1
done
[ "$got" = "$(sha < /tmp/relock.new)" ] && ok "evicted kept file re-reads as the new server content" \
    || no "kept file did not re-read as the server edit (got ${got:0:12})"
stuck=0
for w in /sys/fs/fuse/connections/*/waiting; do
    [ -r "$w" ] && [ "$(cat "$w")" != 0 ] && stuck=$((stuck + $(cat "$w")))
done
[ "$stuck" = 0 ] && ok "no FUSE requests left waiting on the daemon" \
    || no "$stuck FUSE request(s) still waiting on the daemon at suite end"

echo
echo "e2e results: ${PASS} passed, ${FAIL} failed"
[ "$FAIL" -eq 0 ]
