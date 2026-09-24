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

## Follow-ups (after the PR #92 code review)

Fixed in PR #92: H1 (a writable open is registered before its content is staged), M1 (no promotion before the listing's result), M2 (offline edit of a kept copy whose parent was evicted), M3 (the name index waits for 8 lookups of one listing version), M4 (tests), L1 (a slow lookup re-checks unlink and ghosts after writing the IPC maps), L2 (the size overlay skips unlinked handles and takes the largest writer), L6 (a full META pool no longer fails a plain read-only open).

Fixed after the independent review of `7840335`:
- **L6 went too far.** On a full pool the open proceeded as a plain read. But only an open whose sniff probe (GLib's `O_NOATIME`) or thumbnailer matcher (KIO's `CmdlineContains`) could not be decided inline ever reaches the pool. So that fallback brought back one download per file in exactly the listing storms that fill the pool. Now such an open gets EAGAIN (`open_unclassified`), and GLib falls back to the extension-based type. It never gets synthetic bytes, because `rsync --open-noatime` and `tar` send the same flag. An open that matches no probe is decided inline and never waits on the pool. Deciding `LinksLibrary` from the exe's ELF `DT_NEEDED` was not done. `DT_NEEDED` lists only direct dependencies, while `maps` also shows transitive and `dlopen`ed libraries: a GTK app gets `libgio` through `libgtk`. So the verdicts would differ from the matcher's.
- **Open racing unlink before registration.** `insert_open_file` now marks the handle unlinked when the parent listing is resident and no longer has the name, unless an upload of it is in flight (`listed_absent`). This is safe against `rm f; echo x > f` because create() puts the name back in the listing. A DELETE in flight without a resident listing still says nothing. An unlinked handle's staging job does not download.
- **Lookup identity.** A worker's re-check compares the file id and etag, not just the name. An unlink + re-create between pick and commit is picked again, so the new file never gets the old file's id, details or inode. The re-check (and its second `find_child` and ghost lock) is skipped on the inline path, where fuser-0 already serializes against unlink. Ghosts are checked again after the map writes.
- **Staging after a rename.** The staging job downloads from the handle's current path, and falls back to the opened path while the rename's MOVE has not landed.
- **Kept copy with an evicted parent, read-only.** It is served from the copy at once while offline or with the breaker open. Otherwise it waits at most `KEPT_COPY_RESOLVE_WITHIN` (2 s), not the 15 s listing timeout. Trade-off: a copy the server changed meanwhile can be served one more time.
- **Pin on the old parent.** When a rename runs between `insert_open_file` pinning the parent and handing the pin to the handle, the pin now moves to the handle's current parent (`adopt_pin`).

Left for later:
- **L3.** The size overlay covers open handles and `uploading`, but not a file that was closed and whose PUT is still queued in the journal. A `stat` in that window can show the server's old size. Overlaying `pending_put_staging`'s size in `attr_for` would fix it, at the cost of a journal lock per stat.
- **L4, L5, L7, L9.** Deferred as recorded in the PR #92 review.
- **Open racing unlink, without a listing.** Fixed when the parent listing is resident (see above). If it was evicted again by registration time, the handle still misses the unlink: `deleting` cannot be trusted there (`rm f; echo x > f; echo y >> f`). A per-path generation bumped by unlink would handle it. A file whose PUT is queued in the journal but not in flight, and whose parent was re-listed from the server before the PUT landed, is missing from the listing. An open of it would be marked unlinked. Lookups already answer ENOENT for it.
- **Rename of a directory with children open.** `move_inode` remaps only the renamed path, so a child's inode still resolves to its old path. `retarget_open_files` fixes registered handles, but a child opened during the rename is registered under the old path (pre-existing).
- **release() bookkeeping is not unit-tested.** release() gives back the writer count, the pin and the io mode, and it can only be driven through a FUSE `Request`. Factoring that bookkeeping into a function shared with `OpenUndo` would let tests cover it, but it touches the write path's release.

## Fixed after the final data-safety review of c402df2

Rule: a user's saved bytes are always on the server, in a staging file the journal names, or in `recovered/` (or `unsynced/`).

- **CRIT-1: an open marked unlinked because its listing lost the name.** Only positive evidence counts now. unlink, rmdir and a rename over a file record a tombstone (inode + a generation that only grows) under the cache lock, where they edit the listing (`tombstones.rs`). open() takes a snapshot of the generation under that same lock when it resolves the inode. Registration inserts the handle, then checks for a tombstone newer than its snapshot. unlink bumps the generation, then marks handles, matched by inode. `listed_absent` is gone. A tombstone is kept only while an older open is still in flight. Defence in depth: `OpenFile::unlinked` is `Unlinked::{No, Local, Unverified}`. Only `Local` drops a written handle's bytes. `Unverified` (an unlink whose inode was unknown) moves them to `recovered/` with a note, and records a conflict. The writable-open seed changed too: a queued upload's staging file comes first. A name missing from a resident listing is downloaded, not assumed empty. A 404 is staged from a pending Rename's source (the file itself or a parent directory), and otherwise starts empty (another client deleted it; the edit re-creates it). Scenarios (a)–(d) are tested, plus a local unlink during staging and `rm f; echo x > f`.
- **M-4.** That same seed code falls back to another path only on a 404, and only to the source of a Rename still in the journal.
- **H-1: purge vs release.** release() reserves the staging file in the journal before it takes the handle out of `open_files`, and gives the reservation back once the commit has journaled it. Purge reads `open_files` and then `staging_in_use()` (entries + delete_after_save + reserved) right before its scan. It also skips this process's staging files that are newer than the purge's start.
- **H-2: conflicted-copy failure.** The live PUT, the replay and the streamed finish discard staging only when the conflicted copy was uploaded or assembled. Otherwise the entry stays queued: a transient failure is deferred, a permanent one counts as an attempt, and after the budget a Put's bytes go to `unsynced/`.
- **M-1: `recovered/`.** Eviction is by age only (30 days, from the note's date). It runs before the sweep adds anything, and a file without a note gets one dated now. Every kept file has `<file>.json` (remote path when known, size, time, reason). A streamed tail is marked `<staging>.tail` once its first chunk has been cut from it. The sweep deletes a marked tail and keeps an unmarked stream, which still holds every byte. A streamed handle that failed before any chunk was confirmed keeps its bytes in `recovered/` at release.
- **M-2.** A META refusal answers EAGAIN only when the flags match a sniff probe. Any other open is opened plain. Such thumbnailer downloads are bounded by `read_throttle` (3 downloads), not by the thumbnail fetch permit, which covers only server previews. No separate classify pool.
- **M-3.** The `signals` service starts right after the seccomp filter. Before `Session::new` a stop signal flushes the journal (once loaded) and `_exit`s. During `Session::new` it waits for the result. After that, it unmounts as before. The mask stays blocked from `main` on: a thread spawned while the signals were unblocked would inherit that mask, and a signal delivered to it would kill the process with the default action.
- **M-5.** The last chunk is streamed from the tail file (`put_chunk_from_path`), and `shrink_tail_file` copies through a buffer. Once the tail holds `TAIL_CAP_CHUNKS` (2) unsent chunks, a refused graduation spills to `mutate` instead of appending on the caller. The lane keeps the order.
- **LOW.** After its re-picks, a lookup answers with its last pick, not ENOENT. A local create's entry gets a strictly increasing creation time, which tells `rm f; touch f` apart in `lookup_recheck`. fusermount is also looked for at `/usr/bin` and `/bin`. After 10 failed attempts to run it, the daemon writes the journal and exits. e2e scenario 29 SIGKILLs the daemon the moment a save returns.

Still open:
- A pending streamed upload (`FinishChunked`) is not a seed source. A writable non-truncating open during that window stages the server's older content, and its upload supersedes the streamed one.
- `put_dir_cache` still keeps only names with an `uploading` guard. A name with only a queued Put or Rename drops out of a refreshed listing: lookups answer ENOENT and stat is wrong until the upload lands. Its data is safe now.
- rename does not re-key `dir_cache`. A directory renamed over a resident empty listing lists as empty until refreshed.
- A stop signal after `prepare_mount_point` adopted stray files but before the journal is set: those files are swept into `recovered/` at the next start, not uploaded.
