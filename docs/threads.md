# Threads in the ncrs daemon

Every OS thread the daemon creates comes from [`ncrs_core/src/bg.rs`](../ncrs_core/src/bg.rs). It is either a worker of one of the fixed pools below or one of the named long-lived services. `std::thread::spawn` is banned everywhere else by `ncrs_core/clippy.toml`, and CI enforces this with `clippy -D clippy::disallowed_methods`. `std::thread::scope` stays allowed, because it joins its threads before returning.

This makes the thread count a constant known at compile time, `bg::MAX_THREADS`. It equals the pool worker caps (191) + `MAX_SERVICES` (24) + the scoped fan-outs (`MAX_SCOPED_THREADS`, 50) + the main thread + reqwest's runtime threads (≤ 8), which is **274**. fuser adds its session thread (`fuser-0`) and one `fuser-bg` thread. Workers exist only while there is work, so an idle mount holds no pool threads. `HEALTH` over IPC and a once-a-minute `HEALTH` log line report the live numbers.

Why this matters: 0.1.76 spawned a detached thread per FUSE request and per background revalidation, and took its concurrency permit *inside* the thread. During a `find /` over a server answering 500, 9,800 of those threads parked on a 10-slot throttle. The daemon reached 10,160 threads and ~900 load average (2026-09-24; see `docs/plans/2026-09-24-thread-leak-5xx-walker.md`).

## Graph

```mermaid
graph LR
  K[kernel request] --> F0[fuser-0: FUSE dispatch — never blocks on the network]
  F0 -->|submit_owning(reply), EAGAIN if full| RD[[readdir ×24, q512]]
  F0 -->|parent listing not cached: with_child(reply), EAGAIN if full| ME[[meta ×16, q1024]]
  ME -->|starts or joins the listing; one deadline| LI
  ME -->|open: stage current content| RE
  F0 -->|run_read_job(reply), EAGAIN if full| RE[[read ×32, q2048]]
  F0 -->|submit_mutation — ticket taken first, FIFO, never refused| MU[[mutate ×16, q∞]]
  F0 -->|write that fills a chunk: per-handle lane, reply in the job| UP[[upload ×4, q1024]]
  F0 -->|seed a staging file, flush/fsync: per-handle lane| DK[[disk ×4, q4096]]
  F0 & MU -->|journal changed: group commit| JO[[journal ×1, q4]]
  UP -->|chunk PUT, retries| SRV
  F0 -->|notify_later| NO[[notify ×1, q8192]]
  RD -->|cold listing: per-pid token bucket| WK{walkers}
  RD -->|miss / stale| LI[[list ×16, q2048]]
  RD -->|cached hit| BG[[bg ×4, q256: revalidate, purge, abort]]
  RD -->|dir has files| TH[[thumb ×2, q64]]
  LI & BG & TH -->|breaker open or path cooling down: skip| HB{backoff + breaker}
  LI & BG & MU -->|Throttle: bounded waits| SRV[(server)]
  BG & LI & MU & RE -->|kernel cache invalidation| NO
  NO --> KC[(kernel inode cache)]
  IPC[ipc-accept] -->|refuse over 64| IC[[ipc ×64, q0]]
  IC -->|KEEP / PREFETCH| US[[user ×4, q4096]]
  CM[connectivity] & PW[push-watch] -->|replay, reconnect revalidation| HK[[housekeeping ×3, q16]]
```

## Pools

| Pool | Workers | Queue | Full → | Used for |
|---|---|---|---|---|
| `readdir` | 24 | 512 | EAGAIN to the kernel | `readdir`/`readdirplus` workers; the reply travels in the job |
| `meta` | 16 | 1024 | EAGAIN (getxattr: ENODATA; open's process classification proceeds unclassified) | lookup/getattr/setattr/getxattr/listxattr/open whose parent listing is not cached (a hit is answered on `fuser-0`), and open's process classification when it would read `/proc/<pid>/maps` or `cmdline`. The reply travels in the job; each job has one deadline, `PROPFIND_TIMEOUT` from submission |
| `read` | 32 | 2048 | EAGAIN | read waits on read-ahead streams, range streams; staging a writable open's current content (download, or copy of the cached file) |
| `list` | 16 | 2048 | error (stale listing served if cached) | streaming lists, soft-TTL refreshes |
| `bg` | 4 | 256 | dropped | revalidation on read, prefetch, GIO temp purge |
| `mutate` | 16 | unbounded | journal replays it | PUT/MKCOL/DELETE/MOVE commits (`PathSeq` FIFO; see `path_seq.rs`); abort of an abandoned chunk-upload session (never dropped: a lost abort leaks the session's chunks) |
| `upload` | 4 | 1024 | the write runs on the caller as a plain append; its chunk goes with the handle's next write | the `write()` that fills a 10 MB chunk of a streamed upload, and its PUT (with retries); the reply travels in the job. Queued per handle (`fh_lane.rs`), so a handle's later writes, flush and release wait behind it and nothing else does |
| `disk` | 4 | 4096 | runs on the caller | seeding a staging file from the kept copy (first write, truncate), `flush`/`fsync` of a staging file (reply in the job), and a `release` queued behind a handle's in-flight write |
| `journal` | 1 | 4 | left pending for the next change or the shutdown flush | the journal's group commit (`mutation_journal::DeferredSaves`): staging fsyncs, one durable write of the latest journal, then deleting the staging files it no longer names. A failed write is put back and retried with backoff (100 ms → 5 s) |
| `notify` | 1 | 8192 | dropped (entry times out) | every `inval_inode` / `inval_entry` / `delete` to the kernel |
| `ipc` | 64 | 0 | connection closed | one per connected IPC client |
| `housekeeping` | 3 | 16 | disarmed, retried later | dir-cache saver, journal replay, reconnect revalidation |
| `thumb` | 2 | 64 | dropped | thumbnail prefetch batches; thumbguard's server-preview fetch |
| `user` | 4 | 4096 | dropped | IPC-requested KEEP / PREFETCH |

## Services (`bg::spawn_service`, at most 24 per process lifetime)

| Name | Started by | Exits on |
|---|---|---|
| `connectivity` | `mount_ncfs` | shutdown |
| `push-watch` | `mount_ncfs` | shutdown |
| `push-socket` | `NextcloudBackend::start_change_watcher` | shutdown |
| `boot-validate` | `mount_ncfs` (one-shot) | done |
| `boot-files` | `mount_ncfs` (one-shot; fans out with `bg::run_chunked`) | done |
| `auto-keep` | `mount_ncfs` (one-shot) | done / shutdown |
| `storage-stats` | `mount_ncfs` | shutdown |
| `cache-cleanup` | `mount_ncfs` | shutdown |
| `health-log` | `mount_ncfs` | shutdown |
| `ipc-state` | `ipc::start_ipc_server` | process exit |
| `ipc-accept` | `ipc::start_ipc_server` | process exit |
| `change-log` | `ipc::start_ipc_server` (IPC v3 change-log pump, every 250 ms) | process exit |
| `desktop-refresh` | `desktop::Desktop::spawn_refresher` (re-detects file-browser profiles every 10 min) | process exit |

## Scoped fan-outs (`std::thread::scope`, joined before returning)

| Where | Width | Bound |
|---|---|---|
| thumbnail batch (`preview.rs`) | `THUMB_SCOPE_WIDTH` 8 | per `thumb` worker → 16 |
| keep a folder offline (`keep_locally_recursive`) | `KEEP_SCOPE_WIDTH` 2 | per `user` worker → 8 |
| boot file-cache validation (`bg::run_chunked`) | `BOOT_SCOPE_WIDTH` 16 | once at mount |
| notify-push proactive refresh (`bg::run_chunked`) | `REFRESH_SCOPE_WIDTH` 4 | one event at a time |
| unified search providers (`search.rs`) | `SEARCH_WIDTH` 6 | per search |

## Rules

- **Never block `fuser-0` on the network or the kernel.** Anything that can wait goes to a pool. A job that owns a FUSE reply uses `submit_owning` so a refusal is still answered.
- **Work on one open file goes through its lane** (`fh_lane.rs`): write, truncate, flush, fsync and release of a handle run in the order the kernel sent them, inline when the lane is idle and the step is cheap, else on `upload`/`disk`. A refused step runs on the caller rather than being dropped or reordered, and the caller can be `fuser-0`, so a step is told where it runs (`fh_lane::Ran`) and does no network there: a refused chunk graduation only appends to the tail, and the handle's next write (classified `Graduate` again) sends every full chunk the tail holds, or release commits it as a longer last chunk. So `upload`/`disk` refusals cost some staging disk (and, for `disk`, a local copy or fsync on the caller), never a network wait on `fuser-0` and never an error to the writer. Only a write classified `Graduate` when it was dispatched and running on `upload` PUTs a chunk; the classification is not re-decided later. Queued steps start in a loop, never by recursion, and replies leave in request order.
- **Take concurrency permits with a deadline** (`Throttle::acquire_timeout`). `Throttle::acquire` is only for `mutate` jobs, which must finish, and never hold a permit across a retry sleep.
- **A 5xx is an answer, not an outage.** It feeds `backoff.rs` (per-path cooldown and a server-wide breaker), never the offline flag.
- **Adding a pool or a service:** add it to `bg.rs` (and `POOL_SIZES`) and to the tables above. `scripts/check-thread-sites.sh` fails CI if a name is missing here.
