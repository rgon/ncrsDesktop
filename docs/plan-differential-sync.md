# Incremental / Differential Sync — Design Document

**Status:** Draft
**Date:** 2026-05-19

---

## 1. Problem Statement

ncrsDesktop transfers files on-demand via WebDAV. Currently, every `flush()` of
a dirty file triggers a full PUT even when content is byte-identical to the
server version (e.g. editor re-save without modifications). Similarly, every
cache miss triggers a full GET even when the local cache holds a prior version
differing only by a small append.

Nextcloud exposes no delta-sync or rsync-like protocol — the server always
receives/sends whole files over HTTP. However, significant bandwidth and latency
savings are achievable by being smarter about *when* to transfer and *how much*
to transfer (via Range requests for downloads).

### Constraints

- **Server-side:** Nextcloud only supports full-file GET/PUT via WebDAV. No
  rsync, no bsdiff, no block-level server API. Chunked uploads (10 MB chunks)
  are already implemented.
- **Client-side:** FUSE + local cache architecture. Files are written to staging
  files (`cache_dir/write_{fh}`) and uploaded on `flush()`.
- **Dependencies already available:** `md5 0.7`, `crc32fast 1`, `reqwest` with
  Range header support, `serde`/`serde_json` for persistence.

---

## 2. Architecture Overview

A single new module `delta_sync` encapsulates all three phases:

```
ncrs_core/src/
  delta_sync.rs              <-- NEW: orchestration + upload skip logic
  delta_sync/
    content_hash.rs          <-- NEW: whole-file and block-level hashing
    block_index.rs           <-- NEW: persisted block index
  lib.rs                     <-- modified: integration in flush() + ensure_file_cached()
  config.rs                  <-- modified: new config fields
  propfind.rs                <-- modified: parse oc:checksums from PROPFIND
  webdav_ops.rs              <-- modified: add range_download helper
  backend.rs                 <-- modified: add download_file_range to CloudBackend
```

### Entry Points

1. **`should_skip_upload(body, server_checksums, server_size) -> UploadDecision`**
   Called in `flush()` before enqueueing a `MutationOp::Put`.

2. **`try_range_download(backend, path, local_path, old_size, new_size) -> RangeResult`**
   Called in `ensure_file_cached()` when a cached version exists but is stale.

3. **`BlockIndex`** with `update()` and `check_dirty()` for fast pre-check on
   large files.

---

## 3. Phase 1 — Upload Deduplication (Highest ROI)

### Motivation

Many applications trigger a write+flush cycle that results in a PUT even when
file content is byte-identical. This is the most wasteful pattern because uploads
are slower than downloads, each PUT consumes a server-side write + ETag rotation,
and chunked uploads for large files multiply overhead.

### Mechanism

**Nextcloud's `oc:checksums` PROPFIND property** returns server-side checksums
in the format `SHA1:abc123 MD5:def456 ADLER32:789012`. Compare the local file's
MD5 against the server's MD5 to determine content equivalence without any HTTP
transfer.

When `oc:checksums` is unavailable (older NC versions), fall back: if file size
matches remote size AND ETag unchanged since last download, skip the upload.

### Decision Flow in flush()

```
flush() with dirty=true
  |
  +-- Read staging file into memory (already happens)
  +-- Compute MD5 of staging file content
  +-- Look up oc:checksums from dir_cache (parsed from PROPFIND)
  +-- Extract MD5 component, compare with local MD5
  |
  +-- Match    -> log SKIP_UPLOAD, mark clean, remove staging, done
  +-- No match -> proceed with normal PUT
```

### Required Changes

**`propfind.rs`** — Parse `oc:checksums`:
- Add `<oc:checksums />` to PROPFIND XML body
- Add `checksums: Option<String>` to `DavEntry` / `RawResponse`
- Propagate through `RemoteEntry` via `EntryExtensions`

**`delta_sync/content_hash.rs`** — Hashing utilities:
- `md5_hex(data: &[u8]) -> String`
- `extract_md5_from_checksums(checksums: &str) -> Option<&str>`

**`delta_sync.rs`** — Upload skip decision:
```rust
pub struct UploadDecision { pub skip: bool, pub reason: &'static str }

pub fn should_skip_upload(
    body: &[u8],
    server_checksums: Option<&str>,
    server_size: u64,
) -> UploadDecision
```

**`lib.rs`** — Integration in `flush()`:
- After reading staging body, before spawning upload thread
- Also in `mutation_journal::execute_op()` for `MutationOp::Put` (journal replay)

### Configuration

```yaml
delta_sync: true   # master switch, default true
```

### Observability

Add `uploads_skipped: AtomicU64` counter exposed via IPC `STORAGE` response.

### Estimated Effort: ~10 hours

---

## 4. Phase 2 — Download Optimization

### 4.1 Append-Only Range Download

When a cached file becomes stale and `new_size > old_size`, download only the
delta via HTTP `Range: bytes=old_size-`.

**Candidate heuristic** — ALL of:
- Local cached copy exists with known `old_size`
- Server reports `new_size > old_size`
- Extension in allow-list (`.log`, `.csv`, `.txt`, `.jsonl`, `.ndjson`) OR
  size delta < 20% of total

**Flow:**
```
ensure_file_cached(), cache entry exists but etag mismatch
  +-- old_size < new_size AND candidate? -> Range GET bytes=old_size-
  |     +-- 206 Partial Content -> append to local file, update cache, done
  |     +-- 200 or 416         -> fall back to full download
  +-- Otherwise -> full download
```

**Required changes:**
- `backend.rs`: Add `download_file_range()` to `CloudBackend` trait with default
  `NotSupported` return, add `NotSupported` variant to `BackendReadError`
- `nextcloud.rs`: Implement `download_file_range()` using Range header
- `delta_sync.rs`: `try_range_download()` orchestration
- `lib.rs`: Integration in `ensure_file_cached()`

### 4.2 ETag-Conditional Download

Send HEAD with `If-None-Match: "cached_etag"` before downloading. If server
returns 304, the cached file is still valid — just update the `remote_modified`
timestamp. Valuable when dir_cache `modified` drifts but content hasn't changed.

### Estimated Effort: ~12 hours

---

## 5. Phase 3 — Block-Level Local Change Index

### Motivation

For large files (50+ MB), computing whole-file MD5 in the flush path adds
measurable latency. A block-level index allows O(1) dirty detection for the
common "no change" case.

### Design

```rust
const DEFAULT_BLOCK_SIZE: usize = 1024 * 1024; // 1 MB

struct BlockIndex {
    entries: HashMap<PathBuf, BlockIndexEntry>,
    max_entries: usize,
}

struct BlockIndexEntry {
    remote_path: PathBuf,
    block_size: usize,
    total_size: u64,
    blocks: Vec<BlockHash>,
    whole_file_md5: String,
    updated_at_ms: u64,
}

struct BlockHash {
    offset: u64,
    length: u32,
    md5: [u8; 16],
}
```

**Operations:**
- `update(path, data)` — build/update index after download or upload check
- `check_dirty(path, data) -> (bool, changed_count)` — fast pre-check
- `save(cache_dir)` / `load_or_create(cache_dir, max)` — persistence
- LRU eviction when exceeding `max_entries`

**Memory overhead:** ~28 bytes/block. For 10,000 files averaging 100 blocks
each: ~2.8 MB — negligible relative to existing caches.

**Persisted as** `block_index.json` in cache directory, same atomic-rename
pattern as dir_cache/file_cache.

### Integration Points

1. After `ensure_file_cached` succeeds: `block_index.update()`
2. In `flush()` before whole-file MD5: `block_index.check_dirty()` as fast
   pre-check
3. On cache eviction/deletion: `block_index.remove()`

### Configuration

```yaml
delta_sync_block_size: 1048576        # 1 MB, must be power of 2
delta_sync_max_index_entries: 10000   # max files tracked
```

### Estimated Effort: ~14 hours

---

## 6. Phase Sequencing

```
Phase 1: Upload Dedup     Phase 2: Download Opt     Phase 3: Block Index
─────────────────────      ────────────────────      ──────────────────
propfind checksums         backend range trait        block_index module
content_hash module        nextcloud range impl       lib.rs integration
skip logic + flush         delta_sync range logic     config fields
config field               ensure_file_cached

[Independent]              [Independent]              [Reuses Phase 1 content_hash]
```

Phases 1 and 2 can be developed in parallel. Phase 3 reuses Phase 1's
`content_hash` module.

**Recommended order:** Phase 1 (highest ROI, simplest) -> Phase 2 -> Phase 3.

---

## 7. Risk Analysis

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| NC doesn't return `oc:checksums` for some files | Medium | Graceful fallback — only skip when checksum available |
| Range GET returns wrong data (proxy/CDN) | Low | Validate size; on mismatch, delete and full download |
| Block index grows unbounded | Low | LRU eviction at configurable cap |
| MD5 collision | Negligible | Non-adversarial use; size check as additional guard |
| MD5 latency on large files | Low | Phase 3 block index provides fast pre-check |

---

## 8. Testing Strategy

### Unit Tests
- `md5_hex()` known vectors
- `extract_md5_from_checksums()` various formats, missing MD5, empty/malformed
- `should_skip_upload()` all decision branches
- `BlockIndex::check_dirty()` — identical, single block changed, size changed, not indexed
- `BlockIndex` persistence round-trip and LRU eviction

### Integration Tests
- Mock backend with known checksums; verify flush skips upload
- Mock backend returning 206 for Range; verify append behavior
- Full cycle: write -> flush (uploaded) -> write same -> flush (skipped)

### Manual Verification
- Open large file in LibreOffice, save without changes, verify no PUT in logs
- Append to large log on server, open locally, verify Range GET in logs
- Monitor `uploads_skipped` via IPC STORAGE command
