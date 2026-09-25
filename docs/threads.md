# Threads in the ncrs daemon

Every OS thread the daemon creates comes from [`ncrs_core/src/bg.rs`](../ncrs_core/src/bg.rs). It is either a worker of one of the fixed pools below or one of the named long-lived services. `std::thread::spawn` is banned everywhere else by `ncrs_core/clippy.toml`, and CI enforces this with `clippy -D clippy::disallowed_methods`. `std::thread::scope` stays allowed, because it joins its threads before returning.

This makes the thread count a constant known at compile time, `bg::MAX_THREADS`. It equals the pool worker caps (176) + `MAX_SERVICES` (24) + the scoped fan-outs (`MAX_SCOPED_THREADS`, 57) + the main thread + reqwest's runtime threads (`MAX_HTTP_CLIENT_THREADS`, 23: 7 singletons plus one read client per download slot per transport — see `http_clients::DOWNLOAD_CONNECTIONS`), which is **281**. fuser adds its session thread (`fuser-0`) and one `fuser-bg` thread. Workers exist only while there is work, so an idle mount holds no pool threads. `HEALTH` over IPC and a once-a-minute `HEALTH` log line report the live numbers.

Why this matters: 0.1.76 spawned a detached thread per FUSE request and per background revalidation, and took its concurrency permit *inside* the thread. During a `find /` over a server answering 500, 9,800 of those threads parked on a 10-slot throttle. The daemon reached 10,160 threads and ~900 load average (2026-09-24; see `docs/plans/2026-09-24-thread-leak-5xx-walker.md`).

## Graph

```mermaid
graph LR
  K[kernel request] --> F0[fuser-0: FUSE dispatch — never blocks on the network]
  F0 -->|submit_owning(reply), EAGAIN if full| RD[[readdir ×24, q512]]
  F0 -->|run_read_job(reply), EAGAIN if full| RE[[read ×32, q2048]]
  RE -->|window body, after the reply| ST[[stream ×8, q8]]
  F0 -->|look-ahead, slot taken first| ST
  F0 -->|submit_mutation — ticket taken first, FIFO, never refused| MU[[mutate ×16, q∞]]
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
| `read` | 32 | 2048 | EAGAIN | jobs that own a READ reply: opening a range stream to its first bytes, waits on read-ahead windows, the bounded whole-file fallback. Deep so bursts queue instead of bouncing (`cp` treats EAGAIN as fatal); a job dequeued after 30 s answers EAGAIN at once, so a backlog drains at dequeue speed |
| `stream` | 8 (= `DOWNLOAD_CONNECTIONS`) | 8 | foreground: runs on in its `read` worker; look-ahead: dropped | read-ahead window bodies after their READ was answered, and look-ahead windows (whose slot is taken before submission, so they never wait for one here) |
| `dns` | 2 | 64 | lookup fails with a typed refusal (read path: EAGAIN, never offline) | host lookups for every reqwest client the daemon builds (`http_clients::PooledResolver`: one lookup in flight per host, 45 s cache, last good answer on failure), instead of each client runtime's own blocking pool. `ncrs-open` and the GUI keep reqwest's resolver in their own processes |
| `list` | 16 | 2048 | error (stale listing served if cached) | streaming lists, soft-TTL refreshes |
| `bg` | 4 | 256 | dropped | revalidation on read, prefetch, GIO temp purge, chunk-upload abort |
| `mutate` | 16 | unbounded | journal replays it | PUT/MKCOL/DELETE/MOVE commits (`PathSeq` FIFO; see `path_seq.rs`) |
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
| read-ahead window segments (`WindowPump::run`, `lib.rs`) | up to `MAX_WINDOW_SEGMENTS` − 1 = 3 per window | `SEGMENT_SCOPE_WIDTH` 7 in total: each holds a `read_throttle` slot beyond its window's own |

## How long a READ can go unanswered

Every path from `read()` to a reply, worst case (typical is milliseconds):

| Path | Bound |
|---|---|
| cached / staging / in-window hit | inline on `fuser-0`, no wait |
| pool full | EAGAIN at once |
| queued in `read` | until a worker dequeues it; if that took > 30 s, EAGAIN at once |
| range open | offline-blip wait ≤ 15 s + `RANGE_OPEN_BUDGET` 30 s (+ ≤ 15 s header wait overrun) → EAGAIN if no slot or no headers |
| first bytes | + `FIRST_BYTES_DEADLINE` 30 s (+ ≤ 15 s one read) → EAGAIN when slow, EIO when truncated |
| whole-file fallback (server answered the range with an error) | + `READ_FALLBACK_BUDGET` 60 s |
| wait on a window | 60 s without progress, 120 s in all → EAGAIN; a window that stopped (superseded, no slot, server slow) → EAGAIN; one that broke → EIO; never a short reply unless the prefix reaches the proven end of file |

So a READ is answered in at most ~165 s from dequeue. A window body holds no reply and runs on `stream`; it ends within its stall rules (a read with no byte for 15 s, or the window as a whole under 64 KiB per 15 s → resume on another connection, ≤ 2 stall resumes + 1 retry, each open ≤ 30 s), and stops within ~1 s of being superseded once no READ is parked on it.

## Rules

- **Never block `fuser-0` on the network or the kernel.** Anything that can wait goes to a pool. A job that owns a FUSE reply uses `submit_owning` so a refusal is still answered.
- **Take concurrency permits with a deadline** (`Throttle::acquire_timeout`). `Throttle::acquire` is only for `mutate` jobs, which must finish, and never hold a permit across a retry sleep — or while waiting for another permit: in 0.1.77 three read-ahead streams each kept their `read_throttle` slot while waiting, untimed, for another to resume, and no slot was ever released again.
- **A 5xx is an answer, not an outage.** It feeds `backoff.rs` (per-path cooldown and a server-wide breaker), never the offline flag.
- **Adding a pool or a service:** add it to `bg.rs` (and `POOL_SIZES`) and to the tables above. `scripts/check-thread-sites.sh` fails CI if a name is missing here.
