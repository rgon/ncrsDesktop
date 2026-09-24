# Threads in the ncrs daemon

Every OS thread the daemon creates comes from [`ncrs_core/src/bg.rs`](../ncrs_core/src/bg.rs). It is either a worker of one of the fixed pools below or one of the named long-lived services. `std::thread::spawn` is banned everywhere else by `ncrs_core/clippy.toml`, and CI enforces this with `clippy -D clippy::disallowed_methods`. `std::thread::scope` stays allowed, because it joins its threads before returning.

This makes the thread count a constant known at compile time, `bg::MAX_THREADS`. It equals the pool worker caps + `MAX_SERVICES` (24) + the main thread + reqwest's runtime threads (≤ 8), which is **199**. fuser adds its session thread (`fuser-0`) and one `fuser-bg` thread. Workers exist only while there is work, so an idle mount holds no pool threads. `HEALTH` over IPC and a once-a-minute `HEALTH` log line report the live numbers.

Why this matters: 0.1.76 spawned a detached thread per FUSE request and per background revalidation, and took its concurrency permit *inside* the thread. During a `find /` over a server answering 500, 9,800 of those threads parked on a 10-slot throttle. The daemon reached 10,160 threads and ~900 load average (2026-09-24; see `docs/plans/2026-09-24-thread-leak-5xx-walker.md`).

## Graph

```mermaid
graph LR
  K[kernel request] --> F0[fuser-0: FUSE dispatch — never blocks on the network]
  F0 -->|submit_owning(reply), EAGAIN if full| RD[[readdir ×24, q512]]
  F0 -->|run_read_job(reply), EAGAIN if full| RE[[read ×32, q2048]]
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
| `read` | 32 | 2048 | EAGAIN | read waits on read-ahead streams, range streams |
| `list` | 16 | 2048 | error (stale listing served if cached) | streaming lists, soft-TTL refreshes |
| `bg` | 4 | 256 | dropped | revalidation on read, prefetch, GIO temp purge, chunk-upload abort |
| `mutate` | 16 | unbounded | journal replays it | PUT/MKCOL/DELETE/MOVE commits (`PathSeq` FIFO; see `path_seq.rs`) |
| `notify` | 1 | 8192 | dropped (entry times out) | every `inval_inode` / `inval_entry` / `delete` to the kernel |
| `ipc` | 64 | 0 | connection closed | one per connected IPC client |
| `housekeeping` | 3 | 16 | disarmed, retried later | dir-cache saver, journal replay, reconnect revalidation |
| `thumb` | 2 | 64 | dropped | thumbnail prefetch batches |
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

## Rules

- **Never block `fuser-0` on the network or the kernel.** Anything that can wait goes to a pool. A job that owns a FUSE reply uses `submit_owning` so a refusal is still answered.
- **Take concurrency permits with a deadline** (`Throttle::acquire_timeout`). `Throttle::acquire` is only for `mutate` jobs, which must finish, and never hold a permit across a retry sleep.
- **A 5xx is an answer, not an outage.** It feeds `backoff.rs` (per-path cooldown and a server-wide breaker), never the offline flag.
- **Adding a pool or a service:** add it to `bg.rs` (and `POOL_SIZES`) and to the tables above. `scripts/check-thread-sites.sh` fails CI if a name is missing here.
