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
- `put_dir_cache` still keeps only names with an `uploading` guard. A name with only a queued Put or Rename drops out of a refreshed listing: lookups answer ENOENT and stat is wrong until the upload lands. Its data is safe now.
- rename does not re-key `dir_cache`. A directory renamed over a resident empty listing lists as empty until refreshed.
- A stop signal after `prepare_mount_point` adopted stray files but before the journal is set: those files are swept into `recovered/` at the next start, not uploaded.

## Fixed after the last review of eb5e739

- **H-A: a queued streamed upload is the file's content.** `newest_upload(path)` returns the newest Put *or* FinishChunked, and `pending_put_staging` answers only when the newest is a Put, so an older Put's staging is never taken for the content (reads included). A writable non-truncating open whose newest upload is a FinishChunked gets `Seed::AwaitStream`: on the READ worker, `seed_from_queue` polls the journal every 100 ms (re-reading the handle's path each round) until no FinishChunked remains, then downloads. It gives up with EIO after `DOWNLOAD_TIMEOUT`, and at once while offline (logged at ERROR); it never stages empty or older bytes. Because the open waits, its release can no longer supersede an unassembled finish. A kept copy is never fresh while any upload of its file is queued (the listing's etag is still the old version's), which also stops passthrough from serving the old copy. A writable open that does not seed from the kept copy drops it, so the first write cannot seed from it either.
- **L6.** A Pending seed whose staging vanished (uploaded or superseded) asks the journal again (`seed_from_queue`) instead of downloading.
- **M-A: a MOVE landing mid-seed.** `download_seed` looks up the rename's source before the first download. After a 404 it checks again (rename() journals its MOVE after the reply), then tries the source, then the new path. With no source, a 404 for an absent name is asked once more before the file starts empty.
- **L1.** `rename_source_of` undoes each queued Rename, newest to oldest. For that, a Rename no longer rewrites the paths of *earlier Renames*. They stay in their own names, which also fixes their replay: before, `mv a b; mv b c` replayed as MOVE a→c then b→c (404), and `mv d/f e/f; mv e k` moved into a /k that did not exist yet. Other ops are still rewritten as before.
- **M-B: backoff.** `JournalEntry::not_before_ms` (serde default 0). `mark_failed` sets it to 1 min, 5 min, then 25 min. The replay stops at a front entry that is not due yet (later entries may depend on it), and the 30 s connectivity tick retries it. A FinishChunked that runs out of attempts is never aborted. Its tail moves to `recovered/`, and the note records `upload_session` and `tail_offset`. The conflict names the file, the session and where the tail went. The server expires the session. Assembling a conflicted copy somewhere the user can write is not done: there is no known writable location.
- **M-C.** Tombstones keep `by_gen: BTreeMap<gen, ino>` and prune from its front, so each unlink costs O(log n). 50k unlinks under an old open take well under 1 s in a debug build.
- **L2/L3.** The tail marker is written before every cut, holding `{remote_path, offset}`. It lives until the staging file is deleted (`remove_staging_file`, used by `delete_staging` and every release path), so it survives release and the queued finish. The sweep keeps the marker of a named tail. When it deletes an unnamed marked tail, it records a conflict naming the file and offset.
- **L4.** A failed streamed copy's staging is deleted at release, since its writer got EIO; a WARN log gives the byte count. The startup sweep records a conflict when `recovered/` holds more than 4 GB. It never evicts.
- **L5.** The purge's fresh-staging guard allows 1 s of mtime slack.
- **L8.** rename answers EINVAL for `RENAME_EXCHANGE`, `RENAME_WHITEOUT` and unknown bits. For `RENAME_NOREPLACE` it answers EEXIST when the resident listing has `to`, or, with no listing resident, when `to` has an inode. The cache half of rename is `rename_in_cache`, and it is tested.
- **Rename over a dirty writer.** POSIX semantics are kept. The drop is logged at WARN with the path.
- **L9.** If the signals service cannot spawn, `start_watcher` unblocks the stop signals on the calling thread (the daemon's main thread; this is the first thread `mount_ncfs` spawns) and logs at ERROR, so a stop kills the daemon instead of staying pending.
- **Tests added.** Streamed-seed wait and assembly (via the replay); offline EIO; an older Put before a newer stream; a kept copy not fresh while an upload is queued; the L6 re-query; the M-A race (a MOVE landing between downloads); multi-hop `rename_source_of`; backoff and exhaustion of a refused stream (403 on the conflicted-copy name); tombstones at 50k; the marker's content and lifetime; the sweep's conflict for a lost tail; the recovered size warning; rename flags; displaced handles on a rename over a file; the CRIT-1 interleaving (snapshot, then tombstone and scan with no handle, then register → `Local`).

Still open after this round:
- Two writable handles open at once on one file, with one of them streaming, each stage separately. A non-truncating open made *before* the stream's release seeds from the server's old content. If it is released last, its Put supersedes the stream (last close wins). This is pre-existing and needs shared per-inode staging.
- Ops other than Rename are still rewritten into a later rename's names, while that Rename replays after them. For example, an offline Put of `b` then `mv b c` replays as PUT c (If-Match on a missing file gives 412, so a conflicted copy) and then MOVE b→c, which puts the old content at c. This is pre-existing.
- A release racing rename(): a Put enqueued between rename's retarget and its journal entry replays before the MOVE, and the MOVE then replaces it. This is pre-existing and rare.
- A writable open of a file whose streamed upload is queued holds a READ worker for up to `DOWNLOAD_TIMEOUT`.


## Fixed after the review of 6c460fb / 66d306e

- **HIGH (older than this PR): offline journal path rewriting.** A Rename no longer rewrites any earlier entry. Every op replays under the name it had when it was queued, and the replay is FIFO. So the server goes through the same steps as the mount, in the same names. This is the whole correctness argument. Suppose the server starts where the mount started. Replaying the local log verbatim then ends where the mount ended. Coalescing and superseding only drop entries whose effect a later entry fully replaces (see below). The old rewrite moved an op across the Renames after it without commuting it. That is how `rm a; create a; mv a b` became DELETE b, PUT b, MOVE a→b.
  - **The push-forward rule.** A lookup by a file's *current* name walks the journal from newest to oldest (`walk_history`). It carries the name back through each Rename that moved it, and compares each entry with the name the file had when that entry was queued. It is the mirror of `rename_source_of`. The walk ends where the file began: at an Unlink/RmDir of that name (anything older belongs to a deleted file), or at a Rename that moved another file away from that name (`mv a b; create a`: the older entries of `a` belong to `b`). The same rule serves `newest_upload`, `pending_put_staging`, `has_pending_put`, `supersede_uploads` and both coalesces. `forward` goes the other way: it carries an entry's name forward to what the file is called now (None once the file is deleted or a rename replaces it). It serves the purge's protected set (`upload_names`: the queued name and the current one) and the replay's bookkeeping. After an upload lands, the listing entry, the status and the dirty marks move to the current name.
  - **Supersede** drops an older upload of the same file across a Rename too: the MOVE carries the server's copy to the new name, and the newest upload replaces it there. The exception is a create (no etag) that a later Rename moves, because that MOVE needs the file on the server. Otherwise the MOVE hits a false MoveSourceGone. Two uploads of a created file around a rename both upload.
  - **Unlink coalesce** drops the file's uploads (found through its history) only when none carries an etag. The only later entries that depend on them are Renames of the file, and those fail as MoveSourceGone for a file that is deleted anyway (as before). The odd `MkDir == path` clause is gone. **RmDir coalesce** drops the folder's MkDir only when nothing after it needs the folder on the server (`needed_as_parent`): a create in it, or a Rename into, out of or of it. Deletes inside it do not count. Before, `mkdir d; create d/x; mv d/x y; rmdir d` coalesced away the MKCOL that PUT d/x needed.
  - **Etag across MOVE.** Nextcloud keeps a file's etag across a MOVE (its filecache row moves; only the parents' etags change). But the replay does not rely on it. A Put queued before a Rename now replays at its original name, before the MOVE, so its If-Match applies where it was taken. A Put queued after a Rename carries the etag of the listing entry that moved with the file, as before. If a server did change the etag on MOVE, that PUT gets a 412 and the bytes go to a conflicted copy, so nothing is lost. `a_chain_of_renames_with_edits_between_ends_with_the_last_edit` runs against both kinds of server.
  - **Upgrade.** `JournalEntry::queued_names` (serde default false) marks entries saved in the new form. An entry from 0.1.76/0.1.77 replays exactly as stored, which is what 0.1.77 would have done. So its replay is no worse than before, including the old bugs baked into it. Lookups do not carry an old entry's names through old Renames again, since those names already are the Renames' results. A rename made after the upgrade does carry them forward. Tested by `a_journal_written_by_0_1_77_replays_exactly_as_it_did`.
  - **IPC/GUI listing.** It shows the entries as queued, i.e. what the replay will send (`Upload a`, then `Move a → b`). Those are the ops still pending on the server. It is also the GUI's attach-mode replica, whose lookups use the same rule. The wire format only gains the `queued_names` field.
  - **Live workers keep FIFO order too** (`claim_in_order`). A live PUT, streamed finish, MOVE, DELETE, MKCOL or RMDIR claims its own entry first, which fixes the reviewer's LOW: the replay can no longer run it a second time. It then waits until no older entry about its paths (under their names at the time), a folder above them or a path below them is queued (`earlier_related`), and until the `uploading` guard is gone. The wait sleeps on the journal's condvar. After `LIVE_ORDER_WAIT` (30 s) it gives the claim back and leaves the entry to the FIFO replay; before, the MOVE was sent anyway. This replaces the defeated `has_pending_put(from)` guard, and it also covers offline `rm a` followed by a live PUT of a new `a` before the replay reached the DELETE. The comparison runs on path bytes, from a binary search to the entry, so a large backlog stays cheap under the journal lock.
  - **Seed from a queued rename's source first.** `download_seed` used to download the new name first. With `mv a b` over an existing `b` queued, that staged the *old* b, and the edit's PUT (after the MOVE) replaced a's content. Now it stages from the source while the Rename is still queued once the download is done. If the source gives a 404, or the Rename has left the journal, the MOVE landed and the new name is used.
  - **Not done: retargeting a queued create instead of queuing its MOVE.** It is only safe if nothing between the Put and the Rename touches the new name or its parents, and the live PUT worker has already captured the old path. The false MoveSourceGone it would avoid now only comes from a create that is renamed and then deleted.
  - Tests: `journal_replay` (server tree fake with MOVE/DELETE/PUT/MKCOL, etags and chunk sessions). It covers both reviewer cases, a→b→c with edits (etag kept / changed on MOVE), a directory rename with files created before and after, `rm -r; mkdir; …; mv dir dist`, rename-back, rename over existing (edited and not), a created folder renamed before its files land, a streamed finish before its rename, and the 0.1.77 journal. Unit tests cover the lookups (history, vacated names, replacement by rename, deletion, directory chains), supersede, both coalesces, `earlier_related`, and live-order waits (woken by the change, holding the claim, deferred on timeout). There are also seed tests for rename over existing and for a MOVE landing before or during the source download. e2e scenario 30 runs offline `rm a; create a; mv a b` against the live mount.
- **MED: READ pool starvation by stream waiters.** `seed_from_queue` sleeps on the journal's condvar (`MutationJournal::wait_changed`, notified by every journal change). It looks under the same lock it waits on, so a finish leaving the journal wakes it at once instead of on a 100 ms poll. Without a change it looks again every `STREAM_WAIT_RECHECK` (1 s), for the offline and paused flags and a rename of the handle. At most `STREAM_WAITERS_MAX` (4) opens wait at once. A fifth gets EAGAIN before it registers a handle or takes a READ worker, and so does a waiter that finds no free place. An open while offline or paused gets EIO at once. A waiter that sees pause or offline while it waits also gets EIO within a recheck, because the upload cannot land. The place is given back before the download that follows. No new pool.

## Fixed after the review of 30ea5a1..93a98f3

- **H1 (data loss): a claimed entry coalesced away under its waiting worker.** `mv a b` queued in a backlog, a new `a` saved live (its PUT claims and waits for that MOVE), then `mv a c; rm c`: the Unlink coalesce dropped the claimed PUT, the worker's `earlier_related` found no entry and so nothing older, and it sent the new a ahead of the MOVE, which then moved it over b. Two fixes. The Unlink and RmDir coalesces never drop a chain with a claimed entry (upload, Rename or MkDir): the whole history of the file stays and replays in order, and the Unlink deletes on the server what the PUT puts there. And `claim_in_order` looks at its own entry on every round: gone means `Skip`, and the worker sends nothing (supersede already skipped claimed entries). Tests: `a_claimed_create_is_not_coalesced_away_under_its_waiting_worker` (the exact scenario against the tree fake), `a_waiting_live_change_whose_entry_leaves_does_nothing`, `a_delete_never_coalesces_away_an_upload_or_folder_a_worker_has_claimed`.
- **H2 (latency): waits on a newer file's upload guard.** The live MOVE and DELETE no longer look at the `uploading` guard at all: every upload this daemon committed is an older journal entry until it lands, and with H1 fixed a claimed one stays in the journal. The guard is also the new file's (vim's `mv f f~` then a new f, or `rm f` then a new f), which made the MOVE and the new f's PUT wait 30 s on each other. `claim_in_order` lost its `busy` argument. Tests: `a_vim_save_and_an_rm_then_create_never_wait_on_the_new_file`, e2e scenario 31 (both saves must reach the server in under 20 s).
- **M3: waiting workers filling the pool.** `MutationJournal::older_state` says whether the newest older related entry can land soon (`Older::Moving`: claimed, or due with nothing in front of it backing off) or not (`Older::Stuck`: it or an entry in front of it waits out a server backoff, which the FIFO replay stops at). `Stuck` leaves the entry to the replay at once. A change left to the replay is marked `left_to_replay` (not persisted) by `mark_waiting`, which does not touch `last_error`: a wait is no error to show. It kicks the replay (`kick_replay`, a flag the connectivity monitor checks every second between probes), and `remove` kicks again when a live change lands and the new head is such an entry. A worker that starts waiting on an entry nobody claims also kicks once. The wait sleeps on the journal condvar only (no 50 ms poll). Test: `a_change_held_up_by_a_server_backoff_is_left_to_the_replay_at_once`.
- **M1: a Deferred create or rename dropped from `ls` by the next refresh.** The upload guard now lives as long as the upload's entry: a Deferred PUT or streamed finish, a transient or permanent failure, keeps it; `drop_upload_guard` clears it when the live upload lands or is given up, under the name the file has now and only if no newer queued upload of the file needs it; the replay does the same (`settle_local`). Guards move with their file in `rename_in_cache` (a folder rename moves the guards under it; a replaced file's guard goes), and unlink drops the deleted file's guard. Renames get their own overlay, `FsCache::moving` (new name → old name, Rename seq), set by rename() at enqueue and ended when the MOVE lands or is given up, live or in the replay: `put_dir_cache` hides the old name the server still has and keeps (or prefers) the new one. `put_dir_cache` now filters `deleting`/moved-away names before it re-merges guarded entries, so a new file under a name being deleted or moved away stays listed. Side effect: an offline create's guard (never cleared before) now ends when the replay uploads it. Tests: `a_refresh_keeps_a_queued_rename_under_its_new_name_and_a_new_file_under_the_old`, `an_upload_guard_follows_its_file_through_renames`, `the_replay_ends_the_overlays_that_kept_queued_changes_listed`.
- **M2: stale etag map after a rename.** `UploadOrder::moved` runs in rename() (FUSE thread), not after the live MOVE lands, and carries a folder's entries too. The live PUT and streamed-finish success record the etag, listing entry, status, kept copy and dirty marks under `current_name(seq, …)`, i.e. the name the file has now (skipped when it was deleted or replaced meanwhile). So a MOVE left to the replay no longer leaves the map at the old name. Tests: `an_upload_renamed_while_it_ran_lands_under_the_new_name`, `recorded_etags_follow_renames_and_deletes`.
- **M4 (upgrade): 0.1.77 journals.** At load, `flag_old_reorders` flags each old-format upload whose path lies under the target of a later old-format Rename (the shape `[Unlink b, Put b, Rename a→b]`), logs a WARN per file and records one notice (a `PermanentFailure` record, so the GUI's format is unchanged) naming them. Before the replay sends a flagged upload it copies its staging file to `recovered/` with a sidecar (off the journal lock). The entries still replay as stored. Test: `a_0_1_77_upload_under_a_later_renames_target_is_named_and_its_bytes_kept`.
- **LOW, done:**
  - The Unlink coalesce also drops the file's own Renames (file-level, queued after its first upload) of a file created here: `create a; mv a b; rm b` replays as a lone `DELETE b`, with no false MoveSourceGone. A Rename of a folder above it, or one from before its first upload (it moved a server file), stays. unlink() prunes `moving` overlays whose Rename left the journal.
  - Supersede keeps a create only across a Rename of the file itself, not of a folder above it.
  - `rename_source_of` stops where the file began (Unlink/RmDir of its name, or a Rename away from it), like `walk_history`.
  - The replay names a conflicted copy after the file as the user sees it now, in the folder the upload was queued in (the server has that folder at that point of the replay), and uses the current name for `EditConflict.local_path`, the error log, permanent-failure descriptions and the `unsynced/`/`recovered/` names. A PUT answered 409 (Nextcloud's missing folder) is a permanent failure with its bytes kept, like a 404.
  - `upload_names` is one pass over the journal (it scans the live uploads' names only at Renames and deletes).
  - The fake server: DELETE of a missing path answers 404, writes to a locked path 423, PUT/MKCOL/MOVE into a missing folder 409. New replay tests: 412 mid-chain, MoveSourceGone mid-chain, permanent failure mid-chain, created→renamed→deleted, a locked file that waits without costing attempts.
- Verified: `cargo test -p ncrs_core` (490), the clippy gate, `check-thread-sites.sh`, `cargo check -p ncrs-gui`, and `scripts/e2e.sh` 80/80 including the new scenario 31 (a vim-style save and `rm g; new g` both on the server in 0 s).

## Still open (LOW)

- On an old-format persisted rename chain, `rename_source_of` returns the wrong source. 0.1.77 rewrote the Renames' own names, so undoing them is not reliable. The open then fails with EIO, or stages from the wrong path, until the replay runs. This is a one-time upgrade window, offline only. Flagged old uploads now have their bytes kept (M4), but their order is still as stored.
- Offline, a read-only open of a kept file whose newest upload is streamed now gives EIO instead of serving the older version. This is a behaviour change to mention in the release notes. A writable open of such a file while sync is paused now gets EIO too.
- The replay's conflicted copy of an upload whose file was later moved to another folder lands in the folder it was queued in.
- `moving` and upload guards are in memory: after a restart with a queued rename, a refresh before the replay shows the server's names until the replay lands them.
- The live MOVE still counts a transient failure as an attempt (`mark_failed`). This is older than this PR.
- A live worker whose older entry is being sent (`Older::Moving`) still holds its claim, and so the replay's progress past it, for up to `LIVE_ORDER_WAIT` (30 s), e.g. behind a long live upload of the same file.
- The GUI never showed `last_error`; `left_to_replay` is not on the wire either, so the GUI shows a waiting entry only as pending.
