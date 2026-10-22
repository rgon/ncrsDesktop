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

echo "→ 28. READ-YOUR-OWN-WRITES while a file is open (LibreOffice zip-save pattern)"
# Regression for the corrupted LibreOffice save. LibreOffice builds a zip in a temp file
# on the mount and, after each entry, re-reads the partially written last page before
# writing it out whole. getattr used to report the dir-cache size (0 for a new file) and
# read() never served the handle's own staged bytes, so the kernel shrank the file and
# the re-read came back empty: the page was then written with stale buffer contents.
mkdir -p "$MOUNT/ryw"
head -c 60000 /dev/urandom | tr -dc 'a-zA-Z0-9' | head -c 10000 > /tmp/ryw.src
tail -c 1808 /tmp/ryw.src > /tmp/ryw.tail
RYW="$(cat /tmp/ryw.src)"
exec 3<>"$MOUNT/ryw/temp.bin"
printf '%s' "${RYW:0:8192}" >&3
printf '%s' "${RYW:8192}" >&3
# Opening /proc/<pid>/fd/3 is a second open of the file: another handle, same inode.
dd if=/proc/$$/fd/3 bs=4096 skip=0 count=1 2>/dev/null | cmp -s - <(head -c 4096 /tmp/ryw.src) \
    && ok "a second handle reads a full page the writer wrote" \
    || no "a second handle did not get a full page the writer wrote"
cmp -s "$MOUNT/ryw/temp.bin" /tmp/ryw.src \
    && ok "a second handle reads the writer's bytes before they are committed" \
    || no "a second handle did not see the uncommitted bytes"
exec 3>&-
wait_dav_sha ryw/temp.bin "$(sha < /tmp/ryw.src)" 60 \
    && ok "the staged-then-re-read file reached the backend byte-exact" \
    || no "backend copy of the re-read file differs (corrupted save)"

# The LibreOffice save itself, step for step: entries of awkward lengths appended to one
# handle, and after each one the size is checked and the partial last page is re-read
# and rewritten whole. Any wrong answer here becomes stale bytes in the saved file.
if command -v python3 >/dev/null; then
    python3 - "$MOUNT/ryw/lo.bin" /tmp/ryw.lo.expect <<'PY'
import os, sys
path, expect_path = sys.argv[1], sys.argv[2]
def blob(n, salt):
    return bytes(((i * 7 + salt) & 0xff) for i in range(n))
fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o644)
model, bad = bytearray(), []
for k, n in enumerate([4096, 4143, 16839, 13832, 30173, 23545, 29211, 13693, 3000, 1999, 41000]):
    chunk = blob(n, k + 3)
    os.pwrite(fd, chunk, len(model))
    model += chunk
    size = os.fstat(fd).st_size
    if size != len(model):
        bad.append("entry %d: fstat says %d, %d were written" % (k, size, len(model)))
    page = len(model) // 4096 * 4096
    want = bytes(model[page:])
    got = os.pread(fd, 4096, page)
    if got != want:
        bad.append("entry %d: re-read of the last page returned %d bytes, %d were written" % (k, len(got), len(want)))
    # LibreOffice writes that page back whole, with the next entry's first bytes behind it.
    rewrite = got + blob(4096 - len(got), 50 + k) if len(got) < 4096 else got
    os.pwrite(fd, rewrite, page)
    model[page:] = rewrite
whole = os.pread(fd, len(model) + 4096, 0)
if whole != bytes(model):
    bad.append("the file read back differs from what was written")
os.close(fd)
after = os.stat(path).st_size
if after != len(model):
    bad.append("after close the size is %d, %d were written" % (after, len(model)))
open(expect_path, "wb").write(bytes(model))
for b in bad[:6]:
    print("    " + b)
sys.exit(1 if bad else 0)
PY
    rc=$?
else
    rc=2
    echo "    python3 is missing from the e2e image"
fi
[ "$rc" = 0 ] && ok "LibreOffice's write / size / page re-read / page rewrite loop gets true answers throughout" \
    || no "LibreOffice's save loop was given a wrong size or page re-read (rc=$rc)"
wait_dav_sha ryw/lo.bin "$(sha < /tmp/ryw.lo.expect)" 60 \
    && ok "the file that loop produced reached the backend byte-exact" \
    || no "the file from the LibreOffice save loop is corrupt on the backend"

# In place over an existing, already-uploaded file: the kernel knows the right size,
# so this isolates read() — it used to answer from the server with the OLD bytes.
head -c 20000 /dev/urandom > /tmp/ryw.old
cp /tmp/ryw.old "$MOUNT/ryw/existing.bin"
wait_dav_sha ryw/existing.bin "$(sha < /tmp/ryw.old)" 60 || no "setup: existing.bin never reached the backend"
head -c 100 /dev/zero | tr '\0' X > /tmp/ryw.patch
cp /tmp/ryw.old /tmp/ryw.expect1
dd if=/tmp/ryw.patch of=/tmp/ryw.expect1 bs=1 seek=19900 conv=notrunc 2>/dev/null
exec 4<>"$MOUNT/ryw/existing.bin"
dd if=/tmp/ryw.patch bs=1 seek=19900 conv=notrunc >&4 2>/dev/null
dd if=/proc/$$/fd/4 bs=1 skip=19900 count=100 2>/dev/null | cmp -s - /tmp/ryw.patch \
    && ok "an in-place overwrite reads back as written through a second handle" \
    || no "an in-place overwrite read back the OLD bytes through a second handle"
dd if=/proc/$$/fd/4 bs=1 skip=0 count=100 2>/dev/null | cmp -s - <(head -c 100 /tmp/ryw.old) \
    && ok "untouched bytes of the file still read back correctly" \
    || no "untouched bytes read back wrong after the in-place write"
exec 4>&-
wait_dav_sha ryw/existing.bin "$(sha < /tmp/ryw.expect1)" 60 \
    && ok "the in-place edit reached the backend byte-exact" \
    || no "backend copy of the in-place edit differs"
# The same through ONE handle, with explicit offsets (bash can't seek a shared fd):
# write in place, read the write back, and read an earlier edit that is already on the server.
if command -v python3 >/dev/null; then
    python3 - "$MOUNT/ryw/existing.bin" /tmp/ryw.old <<'PY'
import os, sys
fd = os.open(sys.argv[1], os.O_RDWR)
old = open(sys.argv[2], "rb").read()
patch = b"Y" * 100
os.pwrite(fd, patch, 5000)
checks = [
    ("the bytes just written at 5000", os.pread(fd, 100, 5000) == patch),
    ("an untouched range", os.pread(fd, 100, 0) == old[:100]),
    ("the earlier edit at 19900", os.pread(fd, 100, 19900) == b"X" * 100),
    ("the file size", os.fstat(fd).st_size == 20000),
]
os.close(fd)
bad = [n for n, ok in checks if not ok]
for n in bad:
    print("    wrong: " + n)
sys.exit(1 if bad else 0)
PY
    rc=$?
else
    rc=2
fi
cp /tmp/ryw.expect1 /tmp/ryw.expect2
head -c 100 /dev/zero | tr '\0' Y | dd of=/tmp/ryw.expect2 bs=1 seek=5000 conv=notrunc 2>/dev/null
[ "$rc" = 0 ] && ok "one handle reads back its own in-place write, an earlier edit and the true size" \
    || no "an in-place write was not read back correctly through its own handle (rc=$rc)"
wait_dav_sha ryw/existing.bin "$(sha < /tmp/ryw.expect2)" 60 \
    && ok "both in-place edits reached the backend byte-exact" \
    || no "backend copy after the second in-place edit differs"

echo "→ 29. LARGE LIBREOFFICE SAVE over chunked upload — a rewrite of the unsent tail must not fail"
# Regression for "General input/output error" saving a 33 MB .odt. A new file written
# sequentially streams to the server in 10 MiB chunks once it passes one chunk. LibreOffice
# then goes back to the start of the 4 KiB block it is in the middle of and writes that
# block again (seen live: offset=15527936 with 15531976 written). Those bytes were still
# in the local tail, yet any out-of-order write after the first chunk failed the handle
# with EIO. rclone has no chunked uploads, so this runs a second daemon against
# chunkdav.py, which adds Nextcloud's uploads collection in front of the same server.
# Streaming normally starts at 100 MiB; the threshold is lowered to one chunk here.
CD_PORT=8090
CD_LOG=/tmp/chunkdav.log
CD_HOME=/tmp/ncrs-chunked
CD_MOUNT=/mnt/ncrs-chunked
CD_NLOG=/tmp/ncrs-chunked.log
CD_HOST="$(printf '%s' "$URL" | sed -E 's#^https?://([^/]+)/.*#\1#')"
: > "$CD_LOG"
python3 /e2e/chunkdav.py "$CD_PORT" "$CD_HOST" "$CD_LOG" 2>/tmp/chunkdav.err &
CD_PID=$!
mkdir -p "$CD_MOUNT" "$CD_HOME/.config/ncrs" "$CD_HOME/run" "$CD_HOME/cache"
chmod 700 "$CD_HOME/run"
cat > "$CD_HOME/.config/ncrs/config.yaml" <<EOF
url: "http://127.0.0.1:${CD_PORT}/remote.php/dav/files/${U}/"
username: "${U}"
password: "${P}"
user: "${U}"
mount_point: "${CD_MOUNT}"
allow_insecure_http: true
http3: false
optimistic_listing: false
auto_keep_cached_files: false
stream_upload_threshold_mb: 10
# Streaming only happens while the disk is low; a reserve far above any real disk makes
# this mount stream regardless, so the checks below exercise the streaming path.
stream_disk_reserve_mb: 99999999
EOF
cd_ncrs() { HOME="$CD_HOME" XDG_RUNTIME_DIR="$CD_HOME/run" XDG_CACHE_HOME="$CD_HOME/cache" \
    XDG_CONFIG_HOME="$CD_HOME/.config" "$@"; }
for _ in $(seq 1 20); do
    curl -s -o /dev/null -u "$U:$P" -X PROPFIND -H 'Depth: 0' \
        "http://127.0.0.1:${CD_PORT}/remote.php/dav/files/${U}/" && break
    sleep 0.5
done
cd_start() {
    RUST_LOG="${RUST_LOG:-info}" cd_ncrs ncrs --config "$CD_HOME/.config/ncrs/config.yaml" >>"$CD_NLOG" 2>&1 &
    CD_DAEMON=$!
    for _ in $(seq 1 60); do
        mountpoint -q "$CD_MOUNT" && return 0
        kill -0 "$CD_DAEMON" 2>/dev/null || return 1
        sleep 1
    done
    return 1
}
: > "$CD_NLOG"
cd_start
if ! mountpoint -q "$CD_MOUNT"; then
    no "setup: the chunked-upload mount never appeared"
    tail -20 "$CD_NLOG" | sed 's/^/    /'
    tail -5 /tmp/chunkdav.err 2>/dev/null | sed 's/^/    /'
else
    mkdir -p "$CD_MOUNT/chunked"
    # Setup guard: a plain sequential copy must really go through chunks, or the
    # LibreOffice case below would pass without ever reaching the streaming path.
    head -c $((16 * 1048576)) /dev/urandom > /tmp/chunked.seq
    cp /tmp/chunked.seq "$CD_MOUNT/chunked/seq.bin"; crc=$?
    if wait_dav_sha chunked/seq.bin "$(sha < /tmp/chunked.seq)" 90 && [ "$crc" -eq 0 ]; then
        if grep -q "ASSEMBLED .* -> .*/chunked/seq.bin (HTTP 2" "$CD_LOG"; then
            ok "setup: a 16 MiB sequential copy streamed through chunked upload byte-exact"
        else
            no "setup: the sequential copy never used chunked upload — the scenario below would not test the streaming path"
            tail -10 "$CD_LOG" | sed 's/^/    /'
        fi
    else
        no "setup: a 16 MiB sequential copy over chunked upload did not reach the backend intact (cp rc=$crc)"
        grep -E "seq.bin" "$CD_NLOG" | tail -10 | sed 's/^/    /'
        tail -15 /tmp/chunkdav.err 2>/dev/null | sed 's/^/    chunkdav: /'
    fi

    python3 - "$CD_MOUNT/chunked/lo-save.tmp" /tmp/chunked.lo.expect <<'PY'
import os, sys
path, expect_path = sys.argv[1], sys.argv[2]
total = 16 * 1048576 + 777
data = bytearray(os.urandom(total))
fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
bad = []
def put(buf, off, what):
    try:
        n = os.pwrite(fd, buf, off)
        if n != len(buf):
            bad.append("%s: short write %d of %d at %d" % (what, n, len(buf), off))
    except OSError as e:
        bad.append("%s at offset %d: %s" % (what, off, e))
# Sequential, as LibreOffice streams the zip, stopping mid-block exactly as seen live.
stop, off = 15531976, 0
while off < stop:
    n = min(65536, stop - off)
    put(bytes(data[off:off + n]), off, "sequential write")
    off += n
# Back to the start of that 4 KiB block: rewrite it with changed bytes and carry on
# past the old end, so a fix that skips already-written bytes is caught too.
block = stop // 4096 * 4096
for i in range(block, stop):
    data[i] ^= 0x5a
put(bytes(data[block:block + 8192]), block, "rewrite of the partly written block")
off = block + 8192
while off < total and len(bad) < 3:
    n = min(65536, total - off)
    put(bytes(data[off:off + n]), off, "sequential write after the rewrite")
    off += n
try:
    os.close(fd)
except OSError as e:
    bad.append("close: %s" % e)
open(expect_path, "wb").write(bytes(data))
for b in bad[:4]:
    print("    " + b)
sys.exit(1 if bad else 0)
PY
    lrc=$?
    t_lo="$(stat -c %Y "$CD_MOUNT/chunked/lo-save.tmp" 2>/dev/null)"
    [ "$lrc" = 0 ] && ok "LibreOffice's large save (rewrite of the block in progress) got no write error" \
        || {
            no "LibreOffice's large save failed with a write error (rc=$lrc) — the 'General input/output error'"
            grep -E "lo-save.tmp" "$CD_NLOG" | grep -E "non-sequential|abandon|abort" | tail -4 | sed 's/^/    /'
        }
    wait_dav_sha chunked/lo-save.tmp "$(sha < /tmp/chunked.lo.expect)" 90 \
        && ok "the large LibreOffice save reached the backend byte-exact" \
        || no "the large LibreOffice save never reached the backend intact ($(curl -s -u "$U:$P" "${URL}chunked/lo-save.tmp" | wc -c) of $(stat -c %s /tmp/chunked.lo.expect) bytes)"
    # The assembling MOVE carries the save's mtime (X-OC-Mtime), so the server lists the
    # time the mount reported right after the save, not when the chunks were assembled.
    srv="$(curl -s -u "$U:$P" -X PROPFIND -H 'Depth: 0' "${URL}chunked/lo-save.tmp" \
        | grep -o 'getlastmodified>[^<]*' | head -1 | cut -d'>' -f2)"
    [ -n "$t_lo" ] && [ "$(date -u -d "$srv" +%s 2>/dev/null)" = "$t_lo" ] \
        && ok "the chunked upload kept the save's mtime on the server" \
        || no "the chunked upload's server mtime (${srv:-none}) is not the save's ($(date -u -d "@${t_lo:-0}" +%T))"

    cd_restart() {  # <sed expression for the config>: remount with a changed setting
        fusermount3 -u "$CD_MOUNT" 2>/dev/null || umount "$CD_MOUNT" 2>/dev/null
        kill "$CD_DAEMON" 2>/dev/null; wait "$CD_DAEMON" 2>/dev/null
        sed -i "$1" "$CD_HOME/.config/ncrs/config.yaml"
        cd_start || { no "setup: the chunked-upload mount did not come back ($1)"; tail -20 "$CD_NLOG" | sed 's/^/    /'; return 1; }
    }
    # A tail bigger than one chunk at close (threshold 30 MiB: 20-30 MiB unsent) goes up
    # in chunk-sized pieces read one at a time, not as one body held in memory.
    if cd_restart 's/^stream_upload_threshold_mb: .*/stream_upload_threshold_mb: 30/'; then
        mark=$(wc -l < "$CD_LOG")
        head -c $((50 * 1048576 + 4321)) /dev/urandom > /tmp/chunked.big
        cp /tmp/chunked.big "$CD_MOUNT/chunked/big-tail.bin"
        if wait_dav_sha chunked/big-tail.bin "$(sha < /tmp/chunked.big)" 120; then
            biggest=$(tail -n +"$((mark + 1))" "$CD_LOG" | awk '$1 == "PUT" { if ($3 > m) m = $3 } END { print m + 0 }')
            [ "$biggest" -le 10485760 ] && [ "$biggest" -gt 0 ] \
                && ok "a 20-30 MiB tail at close went up in pieces of at most one chunk, byte-exact" \
                || no "the tail at close went up as a ${biggest}-byte chunk (expected at most 10485760)"
        else
            no "a 50 MiB streamed file with a large tail at close never reached the backend intact"
        fi
    fi
    # With room on the disk (reserve 1 MiB) nothing streams: a 24 MiB file stays local
    # until close, so a rewrite of offset 0 works and no chunk is sent while it is open.
    if cd_restart 's/^stream_upload_threshold_mb: .*/stream_upload_threshold_mb: 10/; s/^stream_disk_reserve_mb: .*/stream_disk_reserve_mb: 1/'; then
        python3 - "$CD_MOUNT/chunked/roomy.bin" /tmp/chunked.roomy.expect "$CD_LOG" "$(wc -l < "$CD_LOG")" <<'PY'
import os, sys
path, expect_path, log, mark = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
data = bytearray(os.urandom(24 * 1048576))
fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
try:
    for off in range(0, len(data), 65536):
        os.pwrite(fd, bytes(data[off:off + 65536]), off)
    data[0:4096] = os.urandom(4096)
    os.pwrite(fd, bytes(data[0:4096]), 0)
except OSError as e:
    print("    write: %s" % e)
    sys.exit(1)
sent = [l for l in open(log).read().splitlines()[mark:] if l.startswith("PUT ")]
os.close(fd)
open(expect_path, "wb").write(bytes(data))
if sent:
    print("    %d chunk(s) were sent while the file was still open" % len(sent))
    sys.exit(2)
PY
        rrc=$?
        [ "$rrc" = 0 ] && ok "plenty of disk: a 24 MiB file was not streamed, and rewriting its offset 0 worked" \
            || no "plenty of disk: the 24 MiB file was streamed or its rewrite failed (rc=$rrc)"
        wait_dav_sha chunked/roomy.bin "$(sha < /tmp/chunked.roomy.expect 2>/dev/null)" 90 \
            && ok "plenty of disk: the file reached the backend byte-exact after close" \
            || no "plenty of disk: the file never reached the backend intact"
    fi

    # With stream_uploads off, the whole file is staged locally, so a rewrite of offset 0
    # after more than a threshold's worth was written (an MP4/WAV header patch) must work,
    # even while the disk counts as low (the reserve is set back far above any disk).
    fusermount3 -u "$CD_MOUNT" 2>/dev/null || umount "$CD_MOUNT" 2>/dev/null
    kill "$CD_DAEMON" 2>/dev/null; wait "$CD_DAEMON" 2>/dev/null
    sed -i 's/^stream_disk_reserve_mb: .*/stream_disk_reserve_mb: 99999999/' "$CD_HOME/.config/ncrs/config.yaml"
    echo "stream_uploads: false" >> "$CD_HOME/.config/ncrs/config.yaml"
    if ! cd_start; then
        no "setup: the chunked-upload mount did not come back with stream_uploads off"
        tail -20 "$CD_NLOG" | sed 's/^/    /'
    else
        python3 - "$CD_MOUNT/chunked/header.bin" /tmp/chunked.hdr.expect <<'PY'
import os, sys
path, expect_path = sys.argv[1], sys.argv[2]
data = bytearray(os.urandom(24 * 1048576))
fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
try:
    for off in range(0, len(data), 65536):
        os.pwrite(fd, bytes(data[off:off + 65536]), off)
    data[0:4096] = os.urandom(4096)
    os.pwrite(fd, bytes(data[0:4096]), 0)
    os.close(fd)
except OSError as e:
    print("    write: %s" % e)
    sys.exit(1)
open(expect_path, "wb").write(bytes(data))
PY
        hrc=$?
        [ "$hrc" = 0 ] && ok "streaming off: rewriting offset 0 of a 24 MiB new file got no write error" \
            || no "streaming off: rewriting offset 0 of a 24 MiB new file failed (rc=$hrc)"
        # With streaming on, this rewrite would hit an already-sent chunk and fail.
        wait_dav_sha chunked/header.bin "$(sha < /tmp/chunked.hdr.expect 2>/dev/null)" 90 \
            && ok "streaming off: the header-patched file reached the backend byte-exact" \
            || no "streaming off: the header-patched file never reached the backend intact"
    fi
fi
fusermount3 -u "$CD_MOUNT" 2>/dev/null || umount "$CD_MOUNT" 2>/dev/null
kill "$CD_DAEMON" "$CD_PID" 2>/dev/null
wait "$CD_DAEMON" "$CD_PID" 2>/dev/null
rm -f /tmp/chunked.seq /tmp/chunked.lo.expect /tmp/chunked.hdr.expect /tmp/chunked.big /tmp/chunked.roomy.expect

echo "→ 30. MODIFICATION TIME is the same after the upload as right after the save"
# LibreOffice remembers a document's mtime right after saving it and, before the next save,
# compares it (year down to the second) with the current one; any difference makes it warn
# that the file was modified by someone else. ncrs answered with its own clock at commit
# and, once the listing refreshed, with the time the server received the upload, since no
# upload carried X-OC-Mtime. A save that reaches the server in a later second therefore
# moved the mtime under the editor. Saving while the server is unreachable makes that gap
# certain here; a large upload over a real network does the same.
mt_stat() { stat -c %Y "$MOUNT/$1" 2>/dev/null || echo MISSING; }
mt_dav() {
    curl -s -u "$U:$P" -X PROPFIND -H 'Depth: 0' "${URL}$1" \
        | grep -o 'getlastmodified>[^<]*' | head -1 | cut -d'>' -f2
}
mt_settle() {  # <path> <seconds>: last mtime seen while re-listing past the dir TTL
    local i
    for i in $(seq 1 "$2"); do ls "$MOUNT/$(dirname "$1")" >/dev/null 2>&1; sleep 1; done
    mt_stat "$1"
}
mkdir -p "$MOUNT/mtime"
printf 'v1' > "$MOUNT/mtime/doc.odt"
wait_dav_sha mtime/doc.odt "$(printf 'v1' | sha)" 60 >/dev/null || no "setup: mtime/doc.odt never reached the backend"
server_down
head -c 300000 /dev/urandom > /tmp/mtime.v2
cp /tmp/mtime.v2 "$MOUNT/mtime/doc.odt"
t_saved="$(mt_stat mtime/doc.odt)"          # what LibreOffice records after the save
sleep 3
server_up
wait_dav_sha mtime/doc.odt "$(sha < /tmp/mtime.v2)" 120 >/dev/null \
    || no "setup: the offline save never reached the backend"
t_later="$(mt_settle mtime/doc.odt 15)"     # what it finds before the next save
if [ "$t_saved" = "$t_later" ]; then
    ok "a saved file's mtime did not move once its upload landed (no 'modified by others' warning)"
else
    no "a saved file's mtime moved from $(date -u -d "@$t_saved" +%T) to $(date -u -d "@$t_later" +%T) after its upload landed — LibreOffice warns the file was modified by others"
    echo "    server getlastmodified: $(mt_dav mtime/doc.odt)"
fi

# The time a writer sets explicitly (cp -p, rsync -t, an archive extract) should reach the
# server too, which is what X-OC-Mtime is for.
head -c 5000 /dev/urandom > /tmp/mtime.old
touch -m -d '2020-01-02 03:04:05 UTC' /tmp/mtime.old
cp -p /tmp/mtime.old "$MOUNT/mtime/kept-time.bin"
wait_dav_sha mtime/kept-time.bin "$(sha < /tmp/mtime.old)" 60 >/dev/null \
    || no "setup: mtime/kept-time.bin never reached the backend"
t_kept="$(mt_settle mtime/kept-time.bin 13)"
srv="$(mt_dav mtime/kept-time.bin)"
if [ "$t_kept" = "$(stat -c %Y /tmp/mtime.old)" ] && [ "$(date -u -d "$srv" +%s 2>/dev/null)" = "$t_kept" ]; then
    ok "cp -p kept the source mtime on the mount and on the server"
else
    no "cp -p lost the source mtime 2020-01-02 03:04:05 (mount: $(date -u -d "@$t_kept" '+%F %T' 2>/dev/null || echo "$t_kept"), server: ${srv:-none})"
fi
rm -f /tmp/mtime.v2 /tmp/mtime.old

echo "→ 31. ONE SLOW REQUEST must not freeze the mount; save-by-rename stays ordered"
# The daemon served every FUSE request on one thread, so one request waiting on the
# server (a writable open staging an uncached file, a lookup listing an uncached
# folder) froze every other file on the mount until it returned. Requests now run on
# a small pool, ordered per inode. A third daemon is pointed at chunkdav.py, which
# here holds every GET/PROPFIND of a path containing "slowpoke" for 8 s.
# The second half is the ordering that pool must keep: LibreOffice closes its temp
# file and renames it over the document at once, so RELEASE and RENAME arrive back
# to back. Run concurrently, the rename could slip into the middle of the release's
# commit and the save went up under the temp name.
SC_PORT=8091
SC_LOG=/tmp/chunkdav-slow.log
SC_HOME=/tmp/ncrs-sched
SC_MOUNT=/mnt/ncrs-sched
SC_NLOG=/tmp/ncrs-sched.log
SC_SLOW=8
: > "$SC_LOG"
python3 /e2e/chunkdav.py "$SC_PORT" "$CD_HOST" "$SC_LOG" --slow "slowpoke:$SC_SLOW" 2>/tmp/chunkdav-slow.err &
SC_PID=$!
# Seeded straight on the backend, so the daemon has none of it cached.
head -c 3000000 /dev/urandom > /tmp/sched.slow
curl -s -u "$U:$P" -X MKCOL "${URL}sched/" -o /dev/null
curl -s -u "$U:$P" -X MKCOL "${URL}sched/other/" -o /dev/null
curl -s -u "$U:$P" -X MKCOL "${URL}sched/slowpoke-dir/" -o /dev/null
curl -s -u "$U:$P" -T /tmp/sched.slow "${URL}sched/slowpoke.bin" -o /dev/null
printf 'inside' | curl -s -u "$U:$P" -T - "${URL}sched/slowpoke-dir/x.txt" -o /dev/null
printf 'unrelated' | curl -s -u "$U:$P" -T - "${URL}sched/other/a.txt" -o /dev/null
mkdir -p "$SC_MOUNT" "$SC_HOME/.config/ncrs" "$SC_HOME/run" "$SC_HOME/cache"
chmod 700 "$SC_HOME/run"
cat > "$SC_HOME/.config/ncrs/config.yaml" <<EOF
url: "http://127.0.0.1:${SC_PORT}/remote.php/dav/files/${U}/"
username: "${U}"
password: "${P}"
user: "${U}"
mount_point: "${SC_MOUNT}"
allow_insecure_http: true
http3: false
optimistic_listing: false
auto_keep_cached_files: false
EOF
for _ in $(seq 1 20); do
    curl -s -o /dev/null -u "$U:$P" -X PROPFIND -H 'Depth: 0' \
        "http://127.0.0.1:${SC_PORT}/remote.php/dav/files/${U}/" && break
    sleep 0.5
done
: > "$SC_NLOG"
HOME="$SC_HOME" XDG_RUNTIME_DIR="$SC_HOME/run" XDG_CACHE_HOME="$SC_HOME/cache" XDG_CONFIG_HOME="$SC_HOME/.config" \
    RUST_LOG="${RUST_LOG:-info}" ncrs --config "$SC_HOME/.config/ncrs/config.yaml" >>"$SC_NLOG" 2>&1 &
SC_DAEMON=$!
for _ in $(seq 1 60); do
    mountpoint -q "$SC_MOUNT" && break
    kill -0 "$SC_DAEMON" 2>/dev/null || break
    sleep 1
done
ms_now() { date +%s%3N; }
if ! mountpoint -q "$SC_MOUNT"; then
    no "setup: the slow-proxy mount never appeared"
    tail -20 "$SC_NLOG" | sed 's/^/    /'
else
    timeout -s KILL 20 stat "$SC_MOUNT/sched/slowpoke.bin" >/dev/null 2>&1 \
        || no "setup: sched/slowpoke.bin is not visible on the slow-proxy mount"
    # A writable open of an uncached file downloads it before open() returns; a
    # lookup in an uncached folder lists it. Both now wait SC_SLOW seconds on the proxy.
    t0=$(ms_now)
    ( python3 -c 'import sys; f = open(sys.argv[1], "r+b"); f.read(1); f.close()' "$SC_MOUNT/sched/slowpoke.bin"
      echo "$? $(( $(ms_now) - t0 ))" > /tmp/sched.open.rc ) &
    SO_PID=$!
    ( stat "$SC_MOUNT/sched/slowpoke-dir/x.txt" >/dev/null 2>&1
      echo "$? $(( $(ms_now) - t0 ))" > /tmp/sched.lookup.rc ) &
    SL_PID=$!
    sleep 1
    t1=$(ms_now)
    timeout -s KILL 2 ls "$SC_MOUNT/sched/other" >/dev/null 2>&1; lrc=$?
    timeout -s KILL 2 stat "$SC_MOUNT/sched/other/a.txt" >/dev/null 2>&1; src=$?
    t2=$(ms_now)
    if [ "$lrc" = 0 ] && [ "$src" = 0 ] && [ $((t2 - t1)) -lt 2000 ]; then
        ok "ls/stat of an unrelated folder answered in $((t2 - t1)) ms while an open and a lookup waited on the server"
    else
        no "ls/stat of an unrelated folder blocked behind a slow request (ls rc=$lrc, stat rc=$src, $((t2 - t1)) ms; 137 = still blocked when killed)"
    fi
    wait "$SO_PID" "$SL_PID"
    grep -q "SLOW GET .*slowpoke.bin" "$SC_LOG" && grep -q "SLOW PROPFIND .*slowpoke-dir" "$SC_LOG" \
        || no "setup: the proxy never held the slow requests — the check above proved nothing"
    read -r orc oms < /tmp/sched.open.rc
    read -r krc kms < /tmp/sched.lookup.rc
    if [ "$orc" = 0 ] && [ "$krc" = 0 ] && [ "$oms" -ge $(( (SC_SLOW - 2) * 1000 )) ]; then
        ok "the slow open (${oms} ms) and slow lookup (${kms} ms) still completed"
    else
        no "the slow requests did not complete as expected (open rc=$orc ${oms} ms, lookup rc=$krc ${kms} ms)"
        grep -E "slowpoke" "$SC_NLOG" | tail -6 | sed 's/^/    /'
    fi

    # Save-by-rename, 25 times back to back, while other files in the folder are read.
    # The folder is made through the mount: plain WebDAV lists no Nextcloud permissions
    # for a folder made on the server, and the rename guard then refuses with EACCES.
    mkdir -p "$SC_MOUNT/sched/lo"
    for i in 1 2 3 4; do
        head -c 400000 /dev/urandom > "/tmp/sched.peer$i"
        cp "/tmp/sched.peer$i" "$SC_MOUNT/sched/lo/peer$i.bin"
        wait_dav_sha "sched/lo/peer$i.bin" "$(sha < "/tmp/sched.peer$i")" 60 >/dev/null \
            || no "setup: sched/lo/peer$i.bin never reached the backend"
    done
    : > /tmp/sched.readers.bad
    rm -f /tmp/sched.readers.stop
    SR_PIDS=""
    for i in 1 2 3 4; do
        ( want="$(sha < "/tmp/sched.peer$i")"
          while [ ! -e /tmp/sched.readers.stop ]; do
              got="$(timeout -s KILL 30 sha256sum "$SC_MOUNT/sched/lo/peer$i.bin" 2>/dev/null | awk '{print $1}')"
              [ "$got" = "$want" ] || echo "peer$i: ${got:-unreadable}" >> /tmp/sched.readers.bad
          done ) &
        SR_PIDS="$SR_PIDS $!"
    done
    python3 - "$SC_MOUNT/sched/lo" 25 /tmp/sched.doc.expect <<'PY'
import os, sys
d, n, expect = sys.argv[1], int(sys.argv[2]), sys.argv[3]
bad = 0
for i in range(n):
    data = os.urandom(150000 + 997 * i)
    tmp = os.path.join(d, "lu%d.tmp" % i)
    try:
        with open(tmp, "wb") as f:
            f.write(data)
        os.rename(tmp, os.path.join(d, "doc.odt"))
    except OSError as e:
        print("    save %d: %s" % (i, e))
        bad += 1
open(expect, "wb").write(data)
sys.exit(1 if bad else 0)
PY
    prc=$?
    touch /tmp/sched.readers.stop
    wait $SR_PIDS
    [ "$prc" = 0 ] && ok "25 save-by-rename cycles ran without an error" \
        || no "save-by-rename cycles failed (rc=$prc)"
    [ ! -s /tmp/sched.readers.bad ] && ok "parallel reads of the folder's other files stayed byte-exact" \
        || { no "parallel reads returned wrong bytes"; head -4 /tmp/sched.readers.bad | sed 's/^/    /'; }
    want="$(sha < /tmp/sched.doc.expect)"
    wait_dav_sha sched/lo/doc.odt "$want" 90 \
        && ok "the last save reached the backend byte-exact under the document's name" \
        || no "the backend document is not the last save ($(curl -s -u "$U:$P" "${URL}sched/lo/doc.odt" | wc -c) of $(stat -c %s /tmp/sched.doc.expect) bytes)"
    got="$(timeout -s KILL 30 sha256sum "$SC_MOUNT/sched/lo/doc.odt" 2>/dev/null | awk '{print $1}')"
    [ "$got" = "$want" ] && ok "the mount reads the document back as the last save" \
        || no "the mount reads the document back as ${got:-nothing}, not the last save"
    stray=""
    for _ in $(seq 1 30); do
        stray="$(curl -s -u "$U:$P" -X PROPFIND -H 'Depth: 1' "${URL}sched/lo/" | grep -o 'lu[0-9]*\.tmp' | sort -u | tr '\n' ' ')"
        [ -z "$stray" ] && break
        sleep 1
    done
    mstray="$(ls "$SC_MOUNT/sched/lo" 2>/dev/null | grep '\.tmp$' | tr '\n' ' ')"
    [ -z "$stray" ] && [ -z "$mstray" ] && ok "no temp file was left behind on the backend or the mount" \
        || no "temp files left behind (backend: ${stray:-none}; mount: ${mstray:-none})"
fi
fusermount3 -u "$SC_MOUNT" 2>/dev/null || umount "$SC_MOUNT" 2>/dev/null
kill "$SC_DAEMON" "$SC_PID" 2>/dev/null
wait "$SC_DAEMON" "$SC_PID" 2>/dev/null
rm -f /tmp/sched.slow /tmp/sched.peer* /tmp/sched.doc.expect /tmp/sched.*.rc /tmp/sched.readers.*

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

echo "→ 32. DAEMON RESTART — the mount and programs' open files survive restarts under systemd"
# LibreOffice keeps its document open (read-write, with a lock) for the whole session, so
# a restarted daemon used to leave it with a dead descriptor: every save failed with
# ENOTCONN until the program reopened the file. The kernel keeps a FUSE connection alive
# while any process holds /dev/fuse, so the daemon hands it on: re-exec for a planned
# restart, systemd's fd store across a stop/start, a crash or a watchdog kill. A fourth
# daemon runs here under fake_systemd.py (what systemd does for packaging/ncrs.service)
# against chunkdav.py, which holds GETs of "slowpoke" paths so a read is in flight when
# the daemon freezes.
RS_PORT=8092
RS_LOG=/tmp/chunkdav-restart.log
RS_HOME=/tmp/ncrs-restart
RS_MOUNT=/mnt/ncrs-restart
RS_NLOG=/tmp/ncrs-restart.log
RS_FSLOG=/tmp/fake-systemd.log
RS_SLOW=8
: > "$RS_LOG"; : > "$RS_NLOG"; : > "$RS_FSLOG"
python3 /e2e/chunkdav.py "$RS_PORT" "$CD_HOST" "$RS_LOG" --slow "slowpoke:$RS_SLOW" 2>/tmp/chunkdav-restart.err &
RS_PID=$!
head -c 2000000 /dev/urandom > /tmp/restart.slow
curl -s -u "$U:$P" -X MKCOL "${URL}hold/" -o /dev/null
curl -s -u "$U:$P" -T /tmp/restart.slow "${URL}hold/slowpoke.bin" -o /dev/null
printf 'reader-content' | curl -s -u "$U:$P" -T - "${URL}hold/r.txt" -o /dev/null
mkdir -p "$RS_MOUNT" "$RS_HOME/.config/ncrs" "$RS_HOME/run" "$RS_HOME/cache"
chmod 700 "$RS_HOME/run"
cat > "$RS_HOME/.config/ncrs/config.yaml" <<EOF
url: "http://127.0.0.1:${RS_PORT}/remote.php/dav/files/${U}/"
username: "${U}"
password: "${P}"
user: "${U}"
mount_point: "${RS_MOUNT}"
allow_insecure_http: true
http3: false
optimistic_listing: false
auto_keep_cached_files: false
EOF
for _ in $(seq 1 20); do
    curl -s -o /dev/null -u "$U:$P" -X PROPFIND -H 'Depth: 0' \
        "http://127.0.0.1:${RS_PORT}/remote.php/dav/files/${U}/" && break
    sleep 0.5
done
RS_SOCK="$RS_HOME/run/ncrs.sock"
fsd() { python3 /e2e/fake_systemd.py ctl "$RS_HOME/fsd" "$1"; }
rs_pid() { fsd status | sed -n 's/.*pid=\([0-9]*\).*/\1/p'; }
rs_mark() { RS_MARK=$(($(wc -l < "$RS_NLOG") + 1)); RS_FMARK=$(($(wc -l < "$RS_FSLOG") + 1)); }
rs_since() { tail -n +"$RS_MARK" "$RS_NLOG"; }
rs_fs_since() { tail -n +"$RS_FMARK" "$RS_FSLOG"; }
rs_wait_log() {  # <regex> <seconds>: the daemon logged it since the mark
    for _ in $(seq 1 "$2"); do rs_since | grep -qE "$1" && return 0; sleep 1; done; return 1
}
rs_show() { rs_since | grep -iE "handoff|SIGTERM|watchdog|reattach|restored|error" | tail -8 | sed 's/^/    /'
    rs_fs_since | tail -6 | sed 's/^/    fake systemd: /'; }
rs_held() {  # <step> <chunk>: held descriptors write and read, the mount answers
    # Bounded: a mount left unserved would block the write in the kernel and stall the suite.
    bounded 10 bash -c 'printf "%s" "$1" >&7' _ "$2" 2>/dev/null && ok "$1: the descriptor opened before it still writes" \
        || no "$1: the descriptor opened before it can no longer write"
    # One stored copy of the connection: a re-sent one would pile up in systemd's store.
    local stored; stored="$(fsd status | sed -n 's/.*store=\([^ ]*\).*/\1/p' | tr ',' '\n' | grep -c .)"
    [ "$stored" = 1 ] && ok "$1: exactly one copy of the connection is in the fd store" \
        || no "$1: the fd store holds ${stored} copies of the connection"
    [ "$(python3 -c 'import os; print(os.pread(8, 64, 0).decode(), end="")' 2>/dev/null)" = reader-content ] \
        && ok "$1: the descriptor opened before it still reads" \
        || no "$1: the descriptor opened before it can no longer read"
    bounded 10 ls "$RS_MOUNT/hold" >/dev/null 2>&1 && ok "$1: the mount answers" || no "$1: the mount does not answer"
}
HOME="$RS_HOME" XDG_RUNTIME_DIR="$RS_HOME/run" XDG_CACHE_HOME="$RS_HOME/cache" XDG_CONFIG_HOME="$RS_HOME/.config" \
    RUST_LOG="${RUST_LOG:-info}" python3 /e2e/fake_systemd.py run --dir "$RS_HOME/fsd" --log "$RS_FSLOG" \
    --watchdog-sec 6 --restart-sec 1 -- ncrs --config "$RS_HOME/.config/ncrs/config.yaml" >>"$RS_NLOG" 2>&1 &
RS_FSD=$!
for _ in $(seq 1 60); do
    mountpoint -q "$RS_MOUNT" && grep -q "FDSTORE ncrsfuse-.* kept" "$RS_FSLOG" && break
    kill -0 "$RS_FSD" 2>/dev/null || break
    sleep 1
done
if ! mountpoint -q "$RS_MOUNT" || ! grep -q "FDSTORE ncrsfuse-.* kept" "$RS_FSLOG"; then
    no "setup: the supervised mount never appeared, or never put its connection in the fd store"
    tail -20 "$RS_NLOG" | sed 's/^/    /'; tail -10 "$RS_FSLOG" | sed 's/^/    fake systemd: /'
else
    exec 7>"$RS_MOUNT/hold/w.txt"
    exec 8<"$RS_MOUNT/hold/r.txt"
    printf 'p0-' >&7

    # 1. Planned restart: the same process re-executes with the connection.
    rs_mark; p0="$(rs_pid)"
    reply="$(printf 'RESTART_HANDOFF\n' | timeout 10 nc -U -N "$RS_SOCK" 2>/dev/null)"
    if [ "$reply" = ok ] && rs_wait_log "took over FUSE connection .* from this daemon's previous image" 20 \
        && rs_wait_log "handoff: restored .* open handles" 20 && [ "$(rs_pid)" = "$p0" ]; then
        ok "planned restart: pid ${p0} re-executed and its new image took the live connection over"
    else
        no "planned restart: no re-exec took the connection over (reply '${reply}', pid ${p0} → $(rs_pid))"; rs_show
    fi
    rs_held "planned restart" "p1-"

    # 2. systemctl restart: SIGTERM drains and exits without unmounting; the next
    #    daemon gets the connection from the fd store and the exact handle state.
    rs_mark
    fsd restart >/dev/null
    if rs_wait_log "took over FUSE connection .* from the systemd fd store" 30 \
        && rs_since | grep -q "SIGTERM: drained; exact handle snapshot written" \
        && rs_wait_log "handoff: restored .* \([1-9][0-9]* exact" 10; then
        ok "SIGTERM restart: drained, snapshot written, the next daemon (pid $(rs_pid)) restored the write handle exactly"
    else
        no "SIGTERM restart: the connection or the exact handle state did not reach the next daemon"; rs_show
    fi
    rs_held "SIGTERM restart" "p2-"

    # 3. Crash: kill -9. systemd restarts it with the stored connection; handle state
    #    comes from the log alone.
    rs_mark; p2="$(rs_pid)"
    kill -9 "$p2"
    if rs_wait_log "took over FUSE connection .* from the systemd fd store" 30 \
        && rs_wait_log "handoff: restored .* \(0 exact, 0 failed, 0 lost\)" 10 \
        && rs_fs_since | grep -q "ended: signal SIGKILL"; then
        ok "crash: after kill -9 the restarted daemon (pid $(rs_pid)) restored every handle from the log"
    else
        no "crash: no restarted daemon took the connection over with its handles"; rs_show
    fi
    rs_held "crash" "p3-"

    # 4. Watchdog: freeze the daemon with a read in flight. The pings stop, fake systemd
    #    kills and restarts it, and the next daemon has the kernel resend that read.
    #    A mkdir issued during the freeze runs exactly once (no EEXIST from a replay).
    rs_mark; p3="$(rs_pid)"
    ( timeout 90 sha256sum "$RS_MOUNT/hold/slowpoke.bin" 2>/tmp/restart.read.err | awk '{print $1}' > /tmp/restart.read ) &
    rd=$!
    for _ in $(seq 1 20); do grep -q "SLOW GET .*slowpoke.bin" "$RS_LOG" && break; sleep 0.5; done
    kill -STOP "$p3"
    ( timeout 90 mkdir "$RS_MOUNT/hold/frozen-dir" 2>/tmp/restart.mkdir.err; echo $? > /tmp/restart.mkdir.rc ) &
    md=$!
    wait "$rd" "$md"
    if rs_fs_since | grep -q "watchdog timeout: no WATCHDOG=1 from pid $p3" \
        && rs_since | grep -q "asked the kernel to resend requests the previous daemon left unanswered"; then
        ok "watchdog: the frozen daemon was killed and its successor (pid $(rs_pid)) had the kernel resend what it left unanswered"
    else
        no "watchdog: the frozen daemon was not replaced by one that resent its requests"; rs_show
    fi
    [ "$(cat /tmp/restart.read)" = "$(sha < /tmp/restart.slow)" ] \
        && ok "watchdog: the read in flight when the daemon froze completed with the right bytes" \
        || no "watchdog: the read in flight did not complete correctly ($(cat /tmp/restart.read.err 2>/dev/null | head -1))"
    [ "$(cat /tmp/restart.mkdir.rc)" = 0 ] && [ -d "$RS_MOUNT/hold/frozen-dir" ] \
        && ok "watchdog: a mkdir issued during the freeze ran exactly once" \
        || no "watchdog: the mkdir issued during the freeze failed (rc $(cat /tmp/restart.mkdir.rc): $(cat /tmp/restart.mkdir.err))"
    rs_held "watchdog restart" "p4"

    exec 7>&-
    wait_dav_sha hold/w.txt "$(printf 'p0-p1-p2-p3-p4' | sha)" 90 \
        && ok "everything written across all four restarts reached the backend in order" \
        || no "the file written across the restarts is wrong on the backend: '$(curl -s -u "$U:$P" "${URL}hold/w.txt" | head -c 80)'"

    # 5. systemctl stop: the deliberate end. The fd store is dropped and the mount fails
    #    ENOTCONN rather than hanging.
    exec 9>"$RS_MOUNT/hold/after-stop.txt"
    fsd stop >/dev/null
    err="$(bounded 10 stat "$RS_MOUNT/hold" 2>&1 >/dev/null)"
    case "$err" in
        *"not connected"*) ok "stop: after a deliberate stop the mount fails ENOTCONN" ;;
        *) no "stop: the mount did not fail ENOTCONN after a deliberate stop (${err:-it still answers})" ;;
    esac
    # (A read can still be served from the page cache; a write always reaches the daemon.)
    werr="$( (printf 'x' >&9) 2>&1)"
    case "$werr" in
        *"not connected"*) ok "stop: a held descriptor's write fails ENOTCONN once the mount is stopped" ;;
        *) no "stop: a held descriptor's write did not fail ENOTCONN after the stop (${werr:-it succeeded})" ;;
    esac
    exec 8<&- 9>&-
fi
kill "$RS_FSD" 2>/dev/null; wait "$RS_FSD" 2>/dev/null
umount -l "$RS_MOUNT" 2>/dev/null
kill "$RS_PID" 2>/dev/null; wait "$RS_PID" 2>/dev/null
rm -f /tmp/restart.slow /tmp/restart.read* /tmp/restart.mkdir*

echo
echo "e2e results: ${PASS} passed, ${FAIL} failed"
[ "$FAIL" -eq 0 ]
