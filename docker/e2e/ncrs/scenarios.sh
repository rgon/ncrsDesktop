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
# downloading the file (see mime_magic_bytes() / MIME_DETECT_MAX_READ in
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

    # O_NOATIME probe with GLib's exact 16384-byte read: intercepted → synthetic
    # magic bytes (≠ real content); not intercepted → real downloaded bytes.
    SNIFF_HEX="$(dd if="$MOUNT/weird.ncrstest" iflag=noatime bs=16384 count=1 2>/dev/null | head -c16 | od_hex)"
    if [ -n "$SNIFF_HEX" ] && [ "$SNIFF_HEX" != "$REAL_HEX" ]; then
        ok "O_NOATIME content-type probe intercepted (no download for MIME detection)"
    else
        no "O_NOATIME probe returned real content — file downloaded for MIME detection (regression)"
    fi

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

echo
echo "e2e results: ${PASS} passed, ${FAIL} failed"
[ "$FAIL" -eq 0 ]
