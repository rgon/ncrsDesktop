#!/usr/bin/env bash
# End-to-end data-loss scenarios, run against the live FUSE mount and verified
# against the WebDAV backend directly. The theme is integrity: every byte
# written locally must be retrievable, unchanged, from both the mount and the
# backend; moves/renames must preserve content; deletes must fully propagate.
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

# Wait until the backend copy of <path> hashes to <want> (uploads are async).
wait_dav_sha() {
    local path="$1" want="$2" t="${3:-45}"
    for _ in $(seq 1 "$t"); do
        [ "$(dav_sha "$path")" = "$want" ] && return 0
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
[ "$(fuse_sha create.txt)" = "$W1" ] && ok "read-after-write matches on mount" || no "read-after-write mismatch (data loss)"
wait_dav_sha create.txt "$W1" && ok "backend received identical content" || no "backend content missing/differs (data loss)"

echo "→ 2. UPDATE (overwrite in place)"
C2="ncrs-e2e UPDATED $(date +%s%N)"
printf '%s' "$C2" > "$MOUNT/create.txt"
W2="$(printf '%s' "$C2" | sha)"
[ "$(fuse_sha create.txt)" = "$W2" ] && ok "mount reflects update" || no "mount update mismatch (data loss)"
wait_dav_sha create.txt "$W2" && ok "backend reflects update" || no "backend update lost (data loss)"

echo "→ 3. LARGE FILE (8 MiB, under the 10 MiB chunk threshold)"
head -c 8388608 /dev/urandom > /tmp/big.bin
WB="$(sha < /tmp/big.bin)"
cp /tmp/big.bin "$MOUNT/big.bin"
[ "$(fuse_sha big.bin)" = "$WB" ] && ok "large file intact on mount (no truncation)" || no "large file corrupted on mount (data loss)"
wait_dav_sha big.bin "$WB" 90 && ok "large file intact on backend" || no "large file corrupted on backend (data loss)"

echo "→ 4. RENAME (same directory)"
mv "$MOUNT/create.txt" "$MOUNT/renamed.txt"
[ "$(fuse_sha renamed.txt)" = "$W2" ] && ok "rename preserves content" || no "rename lost content (data loss)"
[ ! -e "$MOUNT/create.txt" ] && ok "old name gone on mount" || no "old name still present on mount"
wait_dav_sha renamed.txt "$W2" && ok "backend has renamed file with content" || no "backend rename lost content (data loss)"
wait_dav_gone create.txt && ok "backend old name removed" || no "backend old name lingering"

echo "→ 5. MOVE into a subdirectory"
mkdir "$MOUNT/sub"
mv "$MOUNT/renamed.txt" "$MOUNT/sub/moved.txt"
[ "$(fuse_sha sub/moved.txt)" = "$W2" ] && ok "move-to-subdir preserves content" || no "move lost content (data loss)"
wait_dav_sha sub/moved.txt "$W2" && ok "backend reflects moved file" || no "backend move lost content (data loss)"

echo "→ 6. MKDIR + nested create"
mkdir -p "$MOUNT/d1/d2"
printf 'nested-content' > "$MOUNT/d1/d2/n.txt"
WN="$(printf 'nested-content' | sha)"
[ "$(fuse_sha d1/d2/n.txt)" = "$WN" ] && ok "nested file created on mount" || no "nested create failed (data loss)"
wait_dav_sha d1/d2/n.txt "$WN" && ok "backend has nested file" || no "backend nested file lost (data loss)"

echo "→ 7. DELETE file"
rm "$MOUNT/sub/moved.txt"
[ ! -e "$MOUNT/sub/moved.txt" ] && ok "file deleted on mount" || no "file still present on mount"
wait_dav_gone sub/moved.txt && ok "backend delete propagated" || no "backend delete not propagated"

echo "→ 8. RMDIR (empty directory)"
rmdir "$MOUNT/sub"
[ ! -e "$MOUNT/sub" ] && ok "directory removed on mount" || no "directory still present on mount"
wait_dav_gone "sub/" && ok "backend rmdir propagated" || no "backend rmdir not propagated"

echo "→ 9. REMOTE → LOCAL propagation"
RC="remote-made $(date +%s%N)"
printf '%s' "$RC" > /tmp/remote.txt
RW="$(printf '%s' "$RC" | sha)"
curl -s -u "$U:$P" -T /tmp/remote.txt "${URL}remote.txt" -o /dev/null
appeared=0
for _ in $(seq 1 45); do
    ls "$MOUNT" >/dev/null 2>&1  # trigger a readdir / revalidation
    [ "$(fuse_sha remote.txt)" = "$RW" ] && { appeared=1; break; }
    sleep 1
done
[ "$appeared" = 1 ] && ok "backend-created file becomes visible on mount with content" || no "remote file never appeared on mount"

echo "→ 10. BATCH rename (20 files, no data loss)"
declare -A want
for i in $(seq 1 20); do
    c="batch-$i-$(date +%s%N)"
    printf '%s' "$c" > "$MOUNT/b$i.txt"
    want[$i]="$(printf '%s' "$c" | sha)"
done
sync
for i in $(seq 1 20); do mv "$MOUNT/b$i.txt" "$MOUNT/r$i.txt"; done
lost=0
for i in $(seq 1 20); do
    [ "$(fuse_sha r$i.txt)" = "${want[$i]}" ] || lost=$((lost + 1))
done
[ "$lost" = 0 ] && ok "all 20 renamed files keep their content on mount" || no "$lost/20 files lost content (data loss)"
wait_dav_sha r1.txt "${want[1]}" && ok "backend batch sample intact" || no "backend batch sample lost content (data loss)"

echo
echo "e2e results: ${PASS} passed, ${FAIL} failed"
[ "$FAIL" -eq 0 ]
