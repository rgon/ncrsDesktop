# Review: take lookup/getattr misses off the FUSE dispatch thread (2026-09-24)

This is a static review of "keep the cache hit inline, move the miss to a pool, and stop evicting the listings that are still in use". Line numbers refer to the pre-rebase tree (`fix/thread-bounds-5xx-walkers` @ c3feb76); re-locate them by symbol. Claims about kernel behaviour are inferences, not verified against kernel source.

## Verdict

The plan is sound, with the mitigations below.

The biggest real-world freeze, `write()` uploading 10 MB chunks on fuser-0 through `graduate_chunk`, is outside this change and gets a separate PR.

## What blocks fuser-0 today

| # | Path | What blocks | Worst case | Covered here? |
|---|---|---|---|---|
| 1 | `write` → `graduate_chunk` | `open_chunked_upload` + `put_chunk`, a 10 MB PUT with a 300 s timeout | every 10 MB of an online copy | no — separate PR |
| 2 | `lookup` → `parent_listing` | offline grace + stream wait + name wait, and clones the snapshot every 50 ms under the cache lock | ≤45 s | yes |
| 3 | `getattr` → `parent_listing` | same | ≤45 s | yes |
| 4 | `open`, writable, uncached | `open_file_timeout` (30 s slot wait + 120 s download); `fs::copy` of a kept file | 150 s+ | yes (step 6) |
| 5 | `setattr` → `get_or_list_dir` / `getattr` | same as 2 | ≤30 s | yes |
| 6 | `getxattr` / `listxattr` → `relist_if_missing` | same as 2 | ≤30 s | yes |
| 7 | cache-hit lookup/getattr | O(n) name scan; a stat sweep over a wide dir is O(n²) | minutes, CPU-bound | yes (step 1 index) |
| 8 | `release` / `flush` / `fsync` | `sync_all` of the staging file; journal rewrite with 2 fsyncs | seconds | no — separate PR |
| 9 | mutations | `journal.enqueue`: O(journal) plus fsyncs | 10–100s of ms | no |

These are fine as they are:
- readdir (`bg::READDIR`) and read waits (`run_read_job`);
- every kernel notifier call (`notify_later`);
- `PathSeq::ticket` (never waits);
- `statfs`, `access` and `forget`, which aren't implemented.

## Correctness of answering off-thread

- **fuser.** `Reply: Send + 'static`, and replies are written with `writev` on an `Arc<DevFuse>`, so they can be sent from any thread. `ReplyRaw::drop` sends EIO, which covers a panicking job.
- **Kernel locking.** FUSE_PARALLEL_DIROPS is **not** negotiated, so the kernel serializes lookup and readdir per directory. That bounds how many workers one directory can tie up. Keep it off.
- **Ordinary lookups.** `lookup_slow` holds the parent's i_rwsem shared, and unlink/rename/create/notify-delete need it exclusive. So an async reply can't bring back an entry that was just invalidated.
- **Revalidate lookups** don't hold the parent lock. A worker answering from an old snapshot can re-insert daemon maps for a deleted or renamed path. Fix: read the final answer from the cache under the lock just before replying, and skip anything in `c.deleting`. LOW.
- **getattr racing setattr/write.** The kernel's attr_version check drops reordered attrs, so reordering is safe. A *stale size answered by the daemon* for a file with unsent local writes already happens today. Fix it in the shared attr builder, overlaying `OpenFile.total_written` or the pending-PUT staging size. MED.
- **forget.** It isn't implemented and the inode maps never shrink, so an entry reply arriving after a FORGET is harmless.
- **Deadlines.** Every offloaded wait needs a hard deadline, because requests the daemon has already read wait uninterruptibly in the kernel. Use one deadline of about PROPFIND_TIMEOUT from submission.
- **Lock order.** `cache` then `ghost_entries`. The detail maps are never nested with `cache`. `allocate_inode` already runs on READDIR workers.
- **HIGH:** `get_pending_snapshot` clones the whole pending listing under the global cache lock, and `parent_listing` polls it every 50 ms. With N offloaded waiters on one large streaming directory, that creates freezes. This must be fixed first.

## Pool and errno

- A new pool, `bg::META`: 16 workers, queue 1024. Don't reuse READDIR: readdir and lookup misses wait on the same slow listings and would starve each other.
- There's no deadlock cycle. META waits on LISTING via `pending_notify` and on Throttle only with timeouts. `walkers.note_uncached` runs on the worker only.
- When a job starts, check the fast path again first, so queued siblings answer at once when the first listing lands.
- On refusal answer EAGAIN (getxattr keeps ENODATA). A timeout or partial listing answers **EAGAIN/ETIMEDOUT, never ENOENT**. Today `parent_listing` answers ENOENT for a file that may exist, which is an existing bug. Use no timed inline wait; only a non-blocking "is it already in the stream" check.

## Answering getattr from a remembered attr

Don't build this now: it would need invalidation hooks at every listing mutation. Add miss/eviction counters to HEALTH first and decide from data.

## Eviction that stays within memory

1. **Refcounted pins inside `FsCache`,** visible to `evict_dir_cache` without another lock:
   - the path of every open directory handle (opendir/releasedir);
   - the parent of every open file handle (open/create/release).

   This is bounded by open fds. Keep `upload_parents`, and consider pinning rename sources.
2. **A walker segment.** A `walker` flag on `DirCacheEntry`, set when the cold fetch came from a pid for which `walkers.is_walker(pid)` is true, and cleared on a non-walker readdir. Eviction takes the walker segment's LRU first while that segment holds more than `max/5`, then the main LRU. The total cap is unchanged.
3. **Rejected:** skeletons for evicted listings (≈40% of a full entry, still unbounded), and pinning whatever the kernel's dentry cache references (thousands of directories after a crawl).
4. `dir_cache.json` keeps its format; pins and segments are runtime-only.

## Existing HIGH data-loss bug (confirmed)

`open()` reads etag, perms and size from the parent listing and falls back to `(None, None, 0)` when that listing was evicted. A writable, non-truncating, uncached open then gets an **unseeded** staging file. O_APPEND or an in-place write uploads a zero-filled prefix, replacing the real content, and If-Match conflict detection is lost. `open()` must resolve the parent (fast path or META) instead of assuming size 0.

## Implementation order

1. **Prerequisites.**
   - `FsCache::pending_find(dir, name) -> Option<RemoteEntry>`: drain `rx`, search by reference, clone one entry.
   - `find_child` with a lazily built name index, for listings with more than ~256 entries (e.g. an `OnceLock<HashMap<Box<str>, u32>>` next to the listing; every mutation swaps the Arc, so the index rebuilds naturally).
   - Replace `parent_listing`'s clone loop, and use a single deadline.
   - A timeout or partial listing answers Unknown, which maps to EAGAIN/ETIMEDOUT.
2. **`MetaCtx`** (Clone): Arcs for conn, cache, ghost_entries, shared, fileids, details, children_map, status, dirty, exclude_folders, open_files and journal. Build it once, and clone it only on a miss.
3. **The split.**
   - `enum Child { Found(RemoteEntry), Absent, Unknown(Option<String>) }`.
   - `resolve_child_cached(&FsCache, dir, name) -> Option<Child>`.
   - `resolve_child_slow(&MetaCtx, dir, name, pid, deadline) -> Child`: fast check → `note_uncached` → start or join the listing (factored out of `list_dir_cached_or_fresh`) → condvar loop on `pending_find` → final answer read under the lock, skipping `deleting`.
   - `attr_for(ctx, ino, &entry)`: `make_file_attr` plus the local-size overlay.
   - `lookup_commit(ctx, path, &entry) -> FileAttr`: inode allocation plus the maps.
4. **`with_child(pid, path, reply, answer)`.** Try the fast path inline under one cache lock; on a miss, `bg::META.submit_owning(reply, …)`; on refusal, EAGAIN. Use it in lookup, getattr, setattr (both branches), getxattr and listxattr.
5. **`bg::META`.** Add it to `POOLS`, `MAX_THREADS` and `docs/threads.md`.
6. **`open()`.** Resolve a missing parent through `with_child`. Move staging (download or copy) to a pool that owns the `ReplyOpen`. Passthrough stays inline.
7. **Eviction.** Add the pins and the walker segment in `evict_dir_cache`, plus HEALTH counters for misses and evictions.
8. **Separate PR.** Move `graduate_chunk` to a pool with `ReplyWrite`, then take `flush`/`fsync`/`release` `sync_all` and the journal fsyncs off fuser-0.

## Tests

**Unit tests**
- `pending_find` / `find_child` (no clone).
- Eviction: size stays bounded, pinned and main-segment entries survive, pins are released on close.
- Handler logic moved into `*_answer()` functions, since fuser Replies can't be built outside fuser.
- A `FakeBackend: CloudBackend` with per-path latency, to test that:
  - a timeout answers Unknown, never Absent;
  - a late-streamed name is Found;
  - joined fetches cost one PROPFIND;
  - the fast path stays under 1 ms p99 while 16 slow resolvers wait on a 100k-entry stream.

**Walker harness**
- `faultproxy` gains `SLOW_PATH_SUBSTR` + `SLOW_MS`, and PUT throttling. `run_walker.sh` gains `DIR_CACHE_MAX_DIRS`.
- **Scenario "no-freeze":**
  1. Set max_dirs=50, and hold an fd open on a hot directory.
  2. Crawl, while looping a stat into a path the proxy stalls for 20 s.
  3. Probe the hot file (stat, a 64 KB read, `ls`) every 100 ms.
  4. Assert p99 < 200 ms and max < 1 s.
  5. Assert the slow stat returns within PROPFIND_TIMEOUT + 2 s and never ENOENT for an existing file.
- **Scenario "evicted-parent append"** in `scripts/e2e.sh`: after an append, the server content is the original plus the appended bytes.
- **Scenario "upload-freeze"** is gated only after step 8.
- The existing `scripts/e2e.sh` and walker gates must stay green.

## Addendum (rechecked against the rebased tree @ d01ea65)

**New risk from master: MEDIUM, rare, but an unkillable mount hang.**
- `open()` reads `/proc` on fuser-0 through `desktop::thumbguard::thumbnailer_process(req.pid())` and `policy().sniff_probe_for_open(flags, pid)`. Results are cached for 10 s.
- Two matchers read files that need the target process's memory lock (mmap_lock): `LinksLibrary("libgio-2.0.so")` reads `/proc/pid/maps`, and `CmdlineContains` reads `/proc/pid/cmdline`.
- How it deadlocks:
  1. Thread A of an app is page-faulting on a file it mmapped from this mount. It holds the lock for reading and waits for a FUSE read that fuser-0 has to dispatch.
  2. Thread B of the same app is queued for the lock in write mode.
  3. Thread C opens a file. fuser-0 reads its `/proc` entry, queues behind B, and never gets to A's read.
- **Fix:** on fuser-0, use only sources that don't take that lock (`/proc/pid/exe`, `/comm`, `/stat`). Decide `LinksLibrary` from the exe's ELF list of needed libraries, cached per exe file. Or evaluate the matchers in open()'s offloaded path.

**New blocking path:** the first write and a size-changing setattr copy a whole kept file into staging with `std::fs::copy`, inline on fuser-0. That is a disk-bound stall; it belongs with step 8 unless it falls out of the open()/setattr rework.

**Step 6 now includes the `/proc` fix.** The rest of the spec is unchanged.
