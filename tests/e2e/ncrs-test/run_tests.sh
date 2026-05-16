#!/bin/bash
set -euo pipefail

MOUNT=/mnt/ncrs
WEBDAV_HOST=webdav
export XDG_CACHE_HOME=/tmp/ncrs-xdg-cache
CACHE_DIR="$XDG_CACHE_HOME/ncrs/http___webdav_remote.php_dav_files_testuser"
JOURNAL="$CACHE_DIR/mutation_journal.json"
PASSED=0
FAILED=0
WEBDAV_IP=""

# ── Helpers ───────────────────────────────────────────────────
go_offline()  { iptables -A OUTPUT -d "$WEBDAV_IP" -j DROP 2>/dev/null || true; }
go_online()   { iptables -D OUTPUT -d "$WEBDAV_IP" -j DROP 2>/dev/null || true; }

wait_synced() {
    # Wait for connectivity monitor (5s offline check) + replay
    sleep 8
}

dav_url() { echo "http://$WEBDAV_HOST/remote.php/dav/files/testuser/$1"; }

dav_get() {
    curl -sf -u testuser:testpass "$(dav_url "$1")"
}

dav_put() {
    curl -sf -u testuser:testpass -T - "$(dav_url "$1")" <<< "$2"
}

dav_delete() {
    curl -sf -u testuser:testpass -X DELETE "$(dav_url "$1")"
}

dav_exists() {
    local status
    status=$(curl -s -o /dev/null -w "%{http_code}" -u testuser:testpass "$(dav_url "$1")")
    [ "$status" = "200" ] || [ "$status" = "207" ]
}

dav_gone() {
    local status
    status=$(curl -s -o /dev/null -w "%{http_code}" -u testuser:testpass "$(dav_url "$1")")
    [ "$status" = "404" ]
}

journal_has() {
    [ -f "$JOURNAL" ] && grep -q "$1" "$JOURNAL"
}

journal_empty() {
    [ ! -f "$JOURNAL" ] || [ "$(cat "$JOURNAL")" = "[]" ]
}

start_ncrs() {
    mkdir -p "$MOUNT" "$CACHE_DIR"
    RUST_LOG=info ncrs \
        --config /tmp/ncrs-config.yaml \
        --mount-point "$MOUNT" \
        &
    NCRS_PID=$!
    sleep 2
    # Wait for FUSE mount to be ready (ls will trigger initial PROPFIND)
    for _i in $(seq 1 10); do
        ls "$MOUNT" >/dev/null 2>&1 && break
        sleep 1
    done
}

stop_ncrs() {
    if [ -n "${NCRS_PID:-}" ]; then
        kill "$NCRS_PID" 2>/dev/null || true
        wait "$NCRS_PID" 2>/dev/null || true
        fusermount3 -u "$MOUNT" 2>/dev/null || true
        NCRS_PID=""
    fi
}

pass() { echo "  PASS: $1"; PASSED=$((PASSED + 1)); }
fail() { echo "  FAIL: $1"; FAILED=$((FAILED + 1)); }

run_test() {
    # Clean state between tests to prevent interference
    rm -f "$JOURNAL" "$CACHE_DIR/conflicts.json" "$CACHE_DIR/dir_cache.json" "$CACHE_DIR/file_cache.json"
    echo ""
    echo "=== Test: $1 ==="
}

cleanup() {
    go_online
    stop_ncrs
}
trap cleanup EXIT

# ── Wait for WebDAV server ────────────────────────────────────
echo "Waiting for WebDAV server..."
for i in $(seq 1 30); do
    if curl -sf -u testuser:testpass "http://$WEBDAV_HOST/remote.php/dav/files/testuser/" > /dev/null 2>&1; then
        echo "WebDAV server ready"
        break
    fi
    sleep 1
done

# Resolve webdav hostname to IP for iptables (DNS name won't work with iptables)
WEBDAV_IP=$(getent hosts "$WEBDAV_HOST" | awk '{print $1}' | head -1)
echo "Resolved $WEBDAV_HOST → $WEBDAV_IP"

# Create a config for ncrs
cat > /tmp/ncrs-config.yaml <<EOF
url: "http://$WEBDAV_HOST/remote.php/dav/files/testuser"
username: testuser
password: testpass
mount_point: "$MOUNT"
user: test
aggressive_prefetch: false
http3: false
max_concurrent_requests: 4
optimistic_listing: false
EOF

mkdir -p "$MOUNT" "$CACHE_DIR"

# ── Test 1: Offline create + write → replay ───────────────────
run_test "Offline file creation and upload replay"

start_ncrs
go_offline
sleep 1

echo "hello_offline" > "$MOUNT/test1.txt" 2>/dev/null && sync
sleep 1

if journal_has "Put"; then
    pass "Journal recorded Put entry"
else
    fail "Journal does not contain Put entry"
fi

go_online
wait_synced

if dav_exists "test1.txt"; then
    content=$(dav_get "test1.txt" || echo "")
    if echo "$content" | grep -q "hello_offline"; then
        pass "File content matches on server"
    else
        fail "File content mismatch: $content"
    fi
else
    fail "File not found on server after replay"
fi

if journal_empty; then
    pass "Journal is empty after replay"
else
    fail "Journal still has entries after replay"
fi

stop_ncrs

# ── Test 2: Offline mkdir ─────────────────────────────────────
run_test "Offline directory creation"

# Clean up from any previous run
dav_delete "testdir/" || true

start_ncrs
go_offline
sleep 1

mkdir "$MOUNT/testdir" 2>/dev/null || true
sleep 1

if journal_has "MkDir"; then
    pass "Journal recorded MkDir entry"
else
    fail "Journal does not contain MkDir entry"
fi

go_online
wait_synced

if dav_exists "testdir/"; then
    pass "Directory exists on server"
else
    fail "Directory not found on server"
fi

stop_ncrs

# ── Test 3: Offline delete ────────────────────────────────────
run_test "Offline file deletion"

# Pre-create file on server
dav_put "todelete.txt" "delete_me"

start_ncrs

# Wait until file appears in FUSE listing
for _i in $(seq 1 20); do
    [ -f "$MOUNT/todelete.txt" ] && break
    sleep 1
done

go_offline
sleep 1

rm "$MOUNT/todelete.txt" 2>/dev/null || true
sleep 1

if journal_has "Unlink"; then
    pass "Journal recorded Unlink entry"
else
    fail "Journal does not contain Unlink entry"
fi

go_online
wait_synced

if dav_gone "todelete.txt"; then
    pass "File deleted from server"
else
    fail "File still exists on server"
fi

stop_ncrs

# ── Test 4: Idempotent delete (already gone) ─────────────────
run_test "Idempotent delete — file already deleted on server"

dav_put "ephemeral.txt" "temp"

start_ncrs

# Wait until file appears in FUSE listing
for _i in $(seq 1 20); do
    [ -f "$MOUNT/ephemeral.txt" ] && break
    sleep 1
done

# Delete on server directly first
dav_delete "ephemeral.txt" || true

go_offline
sleep 1

# Delete locally (file is already gone on server but ncrs doesn't know)
rm "$MOUNT/ephemeral.txt" 2>/dev/null || true
sleep 1

go_online
wait_synced

if journal_empty; then
    pass "Journal drained cleanly (404 → idempotent)"
else
    fail "Journal still has entries"
fi

stop_ncrs

# ── Test 5: Crash recovery ───────────────────────────────────
run_test "Crash recovery — journal survives kill"

start_ncrs
go_offline
sleep 1

echo "crash_data" > "$MOUNT/crash.txt" 2>/dev/null && sync
sleep 1

if journal_has "Put"; then
    pass "Journal has Put before crash"
else
    fail "No Put entry before crash"
fi

# Simulate crash
kill -9 "$NCRS_PID" 2>/dev/null || true
wait "$NCRS_PID" 2>/dev/null || true
fusermount3 -u "$MOUNT" 2>/dev/null || true
sleep 1

# Verify journal persisted on disk
if [ -f "$JOURNAL" ] && journal_has "crash.txt"; then
    pass "Journal file persists on disk with crash.txt entry"
else
    fail "Journal file missing or doesn't contain crash.txt"
fi

# Restart
go_online
start_ncrs
wait_synced

if dav_exists "crash.txt"; then
    content=$(dav_get "crash.txt" || echo "")
    if echo "$content" | grep -q "crash_data"; then
        pass "Crash recovery: file content correct on server"
    else
        fail "Crash recovery: content mismatch: $content"
    fi
else
    fail "Crash recovery: file not found on server"
fi

stop_ncrs

# ── Test 6: Conflict — local edit + concurrent external edit ─────
run_test "Conflict — local and external concurrent edit"

dav_delete "conflict.txt" || true

dav_put "conflict.txt" "server_v1"

start_ncrs

# Wait until the file appears in the FUSE listing (dir_cache populated with ETag)
for _i in $(seq 1 20); do
    [ -f "$MOUNT/conflict.txt" ] && break
    sleep 1
done

go_offline
sleep 1

# Local edit while offline — journals a Put with the cached ETag as If-Match
echo "local_edit" > "$MOUNT/conflict.txt" && sync
sleep 1

if journal_has "Put"; then
    pass "Journal recorded local Put"
else
    fail "Journal missing Put entry"
fi

# External edit: advance the server's ETag so the journaled If-Match will fail
dav_put "conflict.txt" "server_v2"

go_online
wait_synced

# Server file must contain the external edit (server wins)
server_content=$(dav_get "conflict.txt" || echo "")
if echo "$server_content" | grep -q "server_v2"; then
    pass "Server retains external edit after conflict"
else
    fail "Server content wrong after conflict: got '$server_content'"
fi

# A conflicted copy must have been uploaded for the local edit
if curl -sf -u testuser:testpass \
    -X PROPFIND -H "Depth: 1" \
    "$(dav_url "")" \
    --data '<d:propfind xmlns:d="DAV:"><d:prop><d:displayname/></d:prop></d:propfind>' \
    2>/dev/null | grep -q "conflicted copy"; then
    pass "Conflicted copy uploaded to server"
else
    fail "No conflicted copy found on server"
fi

if journal_empty; then
    pass "Journal drained after conflict resolution"
else
    fail "Journal still has entries after conflict"
fi

stop_ncrs

# ── Summary ───────────────────────────────────────────────────
echo ""
echo "=================================="
echo "Results: $PASSED passed, $FAILED failed"
echo "=================================="

if [ "$FAILED" -gt 0 ]; then
    exit 1
fi
echo "ALL TESTS PASSED"
