//! The only place in `ncrs_core` that creates OS threads.
//!
//! Every thread the daemon runs is either a worker of one of the fixed [`Pool`]s
//! below or one of at most [`MAX_SERVICES`] named long-lived services. That makes
//! the daemon's thread count a constant known at compile time,
//! [`MAX_THREADS`], no matter how fast the kernel sends requests or how badly
//! the server answers them. `std::thread::spawn` is banned elsewhere by
//! `clippy.toml` (`disallowed-methods`); `std::thread::scope` stays allowed
//! because it joins its threads before returning.
//!
//! Why: 0.1.76 spawned one detached thread per FUSE request (and more per
//! background revalidation) and took its concurrency permit *inside* the thread.
//! During a `find /` over a server answering 500, those threads parked on the
//! permit faster than they drained, and the daemon reached 10,160 threads. Here
//! the bound is structural: a job that does not fit is refused at submission, on
//! the caller's thread, where it can still pick a fallback (serve the cache,
//! reply EAGAIN, drop a background refresh).
//!
//! Workers start lazily and exit when their queue is empty, so an idle daemon
//! holds no pool threads at all.

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

type Job = Box<dyn FnOnce() + Send + 'static>;

/// A bounded set of lazily started, named worker threads fed by a FIFO queue.
pub struct Pool {
    name: &'static str,
    max_workers: usize,
    queue_cap: usize,
    state: Mutex<State>,
}

struct State {
    active: usize,
    queue: VecDeque<Job>,
    peak_active: usize,
    peak_queued: usize,
    rejected: u64,
    completed: u64,
    panicked: u64,
}

/// A job the pool refused because its workers and queue were full.
///
/// The job was dropped without running. Callers on the FUSE thread must still
/// answer the kernel; background callers just skip the work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a rejected job did not run: reply, fall back, or skip explicitly"]
pub struct Rejected(pub &'static str);

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} pool is full", self.0)
    }
}

/// Counters for one pool, for logs and the IPC `STATS` reply.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PoolStats {
    pub name: &'static str,
    pub max_workers: usize,
    pub active: usize,
    pub queued: usize,
    pub peak_active: usize,
    pub peak_queued: usize,
    pub rejected: u64,
    pub completed: u64,
    pub panicked: u64,
}

impl Pool {
    pub const fn new(name: &'static str, max_workers: usize, queue_cap: usize) -> Self {
        Pool {
            name,
            max_workers,
            queue_cap,
            state: Mutex::new(State {
                active: 0,
                queue: VecDeque::new(),
                peak_active: 0,
                peak_queued: 0,
                rejected: 0,
                completed: 0,
                panicked: 0,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Runs `job` on a worker, starting one if the pool is below its cap, else
    /// queues it. Never blocks. Refuses the job when the queue is full.
    pub fn submit(&'static self, job: impl FnOnce() + Send + 'static) -> Result<(), Rejected> {
        let job: Job = Box::new(job);
        let mut st = self.lock();
        if st.active < self.max_workers {
            st.active += 1;
            st.peak_active = st.peak_active.max(st.active);
            drop(st);
            return self.start_worker(job);
        }
        if st.queue.len() < self.queue_cap {
            st.queue.push_back(job);
            st.peak_queued = st.peak_queued.max(st.queue.len());
            return Ok(());
        }
        st.rejected += 1;
        let rejected = st.rejected;
        drop(st);
        // Power-of-two sampling keeps a sustained storm to a handful of lines.
        if rejected.is_power_of_two() {
            log::warn!("{} pool full ({} workers, {} queued): refused job #{}", self.name, self.max_workers, self.queue_cap, rejected);
        }
        Err(Rejected(self.name))
    }

    /// Like [`submit`](Self::submit) for a job that owns something the caller
    /// must still dispose of when the pool refuses it — typically a FUSE reply,
    /// which has to be answered (EAGAIN, a cached page) rather than dropped.
    pub fn submit_owning<T: Send + 'static>(
        &'static self,
        value: T,
        job: impl FnOnce(T) + Send + 'static,
    ) -> Result<(), (Rejected, T)> {
        let slot = std::sync::Arc::new(Mutex::new(Some(value)));
        let in_job = slot.clone();
        let submitted = self.submit(move || {
            let taken = in_job.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(v) = taken {
                job(v);
            }
        });
        submitted.map_err(|r| {
            // The job was dropped unrun, so the value is still in the slot.
            let v = slot.lock().unwrap_or_else(|e| e.into_inner()).take().expect("refused job cannot have run");
            (r, v)
        })
    }

    #[allow(clippy::disallowed_methods)] // the one sanctioned spawn site for pool workers
    fn start_worker(&'static self, first: Job) -> Result<(), Rejected> {
        let spawned = std::thread::Builder::new()
            .name(format!("ncrs-{}", self.name))
            .spawn(move || self.work(first));
        match spawned {
            Ok(_detached) => Ok(()),
            Err(e) => {
                // The OS is out of threads. Give the slot back; the job is lost.
                let mut st = self.lock();
                st.active -= 1;
                st.rejected += 1;
                drop(st);
                log::error!("{} pool: could not start a worker: {}", self.name, e);
                Err(Rejected(self.name))
            }
        }
    }

    fn work(&'static self, first: Job) {
        // Returns the worker slot even if something below unwinds past
        // `catch_unwind` (e.g. a panicking `Drop`), so the cap never leaks.
        struct Slot(&'static Pool, bool);
        impl Drop for Slot {
            fn drop(&mut self) {
                if self.1 {
                    self.0.lock().active -= 1;
                }
            }
        }
        let mut slot = Slot(self, true);
        let mut job = first;
        loop {
            let panicked = catch_unwind(AssertUnwindSafe(job)).is_err();
            let mut st = self.lock();
            st.completed += 1;
            if panicked {
                st.panicked += 1;
                log::error!("{} pool: a job panicked; the worker carries on", self.name);
            }
            match st.queue.pop_front() {
                Some(next) => job = next,
                None => {
                    st.active -= 1;
                    slot.1 = false;
                    return;
                }
            }
        }
    }

    pub fn stats(&self) -> PoolStats {
        let st = self.lock();
        PoolStats {
            name: self.name,
            max_workers: self.max_workers,
            active: st.active,
            queued: st.queue.len(),
            peak_active: st.peak_active,
            peak_queued: st.peak_queued,
            rejected: st.rejected,
            completed: st.completed,
            panicked: st.panicked,
        }
    }

    pub const fn max_workers(&self) -> usize {
        self.max_workers
    }
}

// ── The daemon's pools ───────────────────────────────────────────────────────
//
// Sizes are upper bounds on threads, not targets: workers exist only while
// there is work. Queues absorb bursts; what overflows them is refused.

/// FUSE readdir/readdirplus workers. A reply travels with each job, so the
/// dispatch thread never blocks on the network. Concurrency is bounded by
/// kernel callers anyway (each blocked `readdir(3)` is one caller).
pub static READDIR: Pool = Pool::new("readdir", READDIR_WORKERS, 512);
pub const READDIR_WORKERS: usize = 24;

/// FUSE read-path jobs that own a reply: opening a range stream up to its first
/// bytes, and waits on read-ahead windows. Each is time-bounded (see the table in
/// docs/threads.md), and the queue holds at most one more round of them, so a
/// queued READ is answered within one job's bound. Window bodies do not run here
/// (see [`STREAM`]): a slow body must not be what a READ is queued behind.
pub static READ: Pool = Pool::new("read", READ_WORKERS, READ_WORKERS);
pub const READ_WORKERS: usize = 32;

/// Read-ahead window bodies (and a sequential reader's look-ahead window) after
/// the READ that started them was answered. Each holds a `read_throttle` slot when
/// it starts, so live bodies are bounded by the download slots; a foreground body
/// the pool refuses runs on in its `read` worker instead.
pub static STREAM: Pool = Pool::new("stream", STREAM_WORKERS, STREAM_WORKERS);
pub const STREAM_WORKERS: usize = crate::http_clients::DOWNLOAD_CONNECTIONS;

/// Host lookups for every reqwest client (`http_clients::PooledResolver`).
/// reqwest's default resolver runs getaddrinfo on each client's own tokio
/// blocking pool — up to 512 threads per client runtime, a dozen-plus runtimes —
/// which no static budget could count. Here it is two threads for the process.
pub static DNS: Pool = Pool::new("dns", DNS_WORKERS, 64);
pub const DNS_WORKERS: usize = 2;

/// Directory fetches from the server (streaming lists, TTL refreshes,
/// prefetch). Each fetch also takes a `Throttle` slot, so these are the only
/// threads that ever wait on one while listing.
pub static LISTING: Pool = Pool::new("list", LISTING_WORKERS, 2048);
pub const LISTING_WORKERS: usize = 16;

/// Best-effort work that can be dropped under load: revalidation, thumbnails,
/// temp-file purges, IPC-triggered prefetch. Small on purpose.
pub static BACKGROUND: Pool = Pool::new("bg", BACKGROUND_WORKERS, 256);
pub const BACKGROUND_WORKERS: usize = 4;

/// Server mutations (PUT/MKCOL/DELETE/MOVE commits). Never refused: every job
/// holds a `PathSeq` ticket taken on the FUSE thread in submission order, and
/// the queue is FIFO, so the oldest live ticket always belongs to a running
/// job and waits cannot deadlock (see `path_seq.rs`).
pub static MUTATION: Pool = Pool::new("mutate", MUTATION_WORKERS, usize::MAX);
pub const MUTATION_WORKERS: usize = 16;

/// Kernel cache notifications (`inval_inode`, `inval_entry`, `delete`). One
/// worker: a notification the kernel blocks on costs one thread and one queue,
/// never the FUSE dispatch thread (see the 2026-09-21 unlink deadlock).
pub static NOTIFY: Pool = Pool::new("notify", NOTIFY_WORKERS, 8192);
pub const NOTIFY_WORKERS: usize = 1;

/// Connected IPC clients (GUI, file-manager extensions). No queue: a client
/// over the cap is refused and reconnects later.
pub static IPC: Pool = Pool::new("ipc", IPC_WORKERS, 0);
pub const IPC_WORKERS: usize = 64;

/// Debounced savers and other self-rearming chores (each guarded by its own
/// "armed" flag, so at most one of each is ever queued).
pub static HOUSEKEEPING: Pool = Pool::new("housekeeping", HOUSEKEEPING_WORKERS, 16);
pub const HOUSEKEEPING_WORKERS: usize = 3;

/// Thumbnail prefetch batches. Each batch paces itself (hundreds of ms per
/// item), so they get their own workers instead of starving `BACKGROUND`.
pub static THUMB: Pool = Pool::new("thumb", THUMB_WORKERS, 64);
pub const THUMB_WORKERS: usize = 2;

/// Work a user asked for over IPC (keep a folder offline, prefetch one): can
/// run for minutes, so it gets its own workers and a deep queue rather than
/// competing with droppable background work.
pub static USER: Pool = Pool::new("user", USER_WORKERS, 4096);
pub const USER_WORKERS: usize = 4;

pub static POOLS: [&Pool; 12] = [&READDIR, &READ, &STREAM, &DNS, &LISTING, &BACKGROUND, &MUTATION, &NOTIFY, &IPC, &HOUSEKEEPING, &THUMB, &USER];

/// Named long-lived threads: the FUSE session, connectivity monitor, push
/// watcher, IPC accept loop, savers, cleanup. Fixed in number.
pub const MAX_SERVICES: usize = 24;

/// Width of the one-shot `std::thread::scope` fan-outs, which join before
/// returning and so never outlive their caller:
pub const THUMB_SCOPE_WIDTH: usize = 8; // per `thumb` worker (preview.rs THUMB_BATCH)
pub const KEEP_SCOPE_WIDTH: usize = 2; // per `user` worker (keep_locally_recursive)
pub const BOOT_SCOPE_WIDTH: usize = 16; // boot file-cache validation, once
pub const REFRESH_SCOPE_WIDTH: usize = 4; // notify-push proactive refresh, one at a time
pub const SEARCH_WIDTH: usize = 6; // unified-search providers per search
/// Extra segments of read-ahead windows (lib.rs `WindowPump::run`), across all
/// windows at once. Enforced by [`SegmentThread`] tokens, not by the slots: a
/// segment gives its slot back while it resumes, so slots alone would let new
/// windows start more segment threads meanwhile.
pub const SEGMENT_SCOPE_WIDTH: usize = crate::http_clients::DOWNLOAD_CONNECTIONS - 1;

static SEGMENT_THREADS: AtomicUsize = AtomicUsize::new(0);

/// The right to run one scoped segment thread, held for the thread's whole life
/// (moved into it, dropped as it returns). Only ever *tried* for: a window with no
/// token free is fetched without that segment, never waits for one.
pub struct SegmentThread(());

impl SegmentThread {
    pub fn try_take() -> Option<SegmentThread> {
        SEGMENT_THREADS
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| (n < SEGMENT_SCOPE_WIDTH).then_some(n + 1))
            .ok()
            .map(|_| SegmentThread(()))
    }
}

impl Drop for SegmentThread {
    fn drop(&mut self) {
        SEGMENT_THREADS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Upper bound on scoped threads alive at once (assuming one search at a time).
pub const MAX_SCOPED_THREADS: usize = THUMB_WORKERS * THUMB_SCOPE_WIDTH
    + USER_WORKERS * KEEP_SCOPE_WIDTH
    + BOOT_SCOPE_WIDTH
    + REFRESH_SCOPE_WIDTH
    + SEARCH_WIDTH
    + SEGMENT_SCOPE_WIDTH;

/// Every thread the daemon can ever have: pool workers + services + scoped
/// fan-outs + the main thread + reqwest's internal runtime threads.
pub const MAX_THREADS: usize = {
    let mut n = 0;
    let mut i = 0;
    while i < POOL_SIZES.len() {
        n += POOL_SIZES[i];
        i += 1;
    }
    n + MAX_SERVICES + MAX_SCOPED_THREADS + 1 + MAX_HTTP_CLIENT_THREADS
};

/// reqwest's blocking client runs one runtime thread per client, alive as long as
/// the client. ncrs builds a fixed set, all at startup or first use and never more:
/// a generous 8 for the singletons (metadata per transport, previews, push,
/// notifications, search, assets, one read client per transport), plus the
/// read clients beyond the first for each transport — one per download slot, for
/// HTTP/3 and for the HTTP/2 fallback (see `http_clients::DOWNLOAD_CONNECTIONS`).
pub const MAX_HTTP_CLIENT_THREADS: usize = 8 + 2 * (crate::http_clients::DOWNLOAD_CONNECTIONS - 1);

const POOL_SIZES: [usize; 12] = [
    READDIR_WORKERS,
    READ_WORKERS,
    STREAM_WORKERS,
    DNS_WORKERS,
    LISTING_WORKERS,
    BACKGROUND_WORKERS,
    MUTATION_WORKERS,
    NOTIFY_WORKERS,
    IPC_WORKERS,
    HOUSEKEEPING_WORKERS,
    THUMB_WORKERS,
    USER_WORKERS,
];

/// A lifetime cap on how many threads a class may ever start.
struct Budget {
    used: AtomicUsize,
    max: usize,
}

impl Budget {
    const fn new(max: usize) -> Self {
        Budget { used: AtomicUsize::new(0), max }
    }

    fn take(&self) -> bool {
        self.used
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| (n < self.max).then_some(n + 1))
            .is_ok()
    }
}

static SERVICES: Budget = Budget::new(MAX_SERVICES);

/// Starts a named long-lived thread. Refuses once [`MAX_SERVICES`] have been
/// started over the process lifetime, which keeps a restart loop from quietly
/// growing the thread count.
pub fn spawn_service<T: Send + 'static>(
    name: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> std::io::Result<std::thread::JoinHandle<T>> {
    spawn_within(&SERVICES, name, f)
}

#[allow(clippy::disallowed_methods)] // the one sanctioned spawn site for services
fn spawn_within<T: Send + 'static>(
    budget: &Budget,
    name: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> std::io::Result<std::thread::JoinHandle<T>> {
    if !budget.take() {
        log::error!("refusing to start service thread {}: {} already started", name, budget.max);
        return Err(std::io::Error::other("service thread budget exhausted"));
    }
    std::thread::Builder::new().name(format!("ncrs-{}", name)).spawn(f)
}

/// Runs `jobs` at most `width` at a time on scoped threads, returning once all
/// are done. For one-shot fan-outs (boot validation) that must finish together.
pub fn run_chunked(jobs: Vec<Box<dyn FnOnce() + Send>>, width: usize) {
    let mut jobs = jobs.into_iter();
    loop {
        let batch: Vec<_> = jobs.by_ref().take(width.max(1)).collect();
        if batch.is_empty() {
            return;
        }
        std::thread::scope(|s| {
            for job in batch {
                s.spawn(job);
            }
        });
    }
}

pub fn stats() -> Vec<PoolStats> {
    POOLS.iter().map(|p| p.stats()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Barrier};
    use std::time::{Duration, Instant};

    fn leak(p: Pool) -> &'static Pool {
        Box::leak(Box::new(p))
    }

    fn wait_idle(p: &Pool) {
        let t = Instant::now();
        while p.stats().active > 0 {
            assert!(t.elapsed() < Duration::from_secs(10), "pool never drained: {:?}", p.stats());
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn never_exceeds_its_worker_cap() {
        let p = leak(Pool::new("t-cap", 3, 10_000));
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        for _ in 0..500 {
            let (live, peak) = (live.clone(), peak.clone());
            p.submit(move || {
                let n = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(n, Ordering::SeqCst);
                std::thread::sleep(Duration::from_micros(200));
                live.fetch_sub(1, Ordering::SeqCst);
            })
            .unwrap();
        }
        wait_idle(p);
        assert!(peak.load(Ordering::SeqCst) <= 3);
        let s = p.stats();
        assert_eq!((s.completed, s.rejected, s.queued, s.active), (500, 0, 0, 0));
        assert!(s.peak_active <= 3);
    }

    #[test]
    fn refuses_when_workers_and_queue_are_full() {
        let p = leak(Pool::new("t-full", 1, 1));
        let gate = Arc::new(Barrier::new(2));
        let g = gate.clone();
        p.submit(move || {
            g.wait();
        })
        .unwrap(); // the worker
        p.submit(|| {}).unwrap(); // the queue slot
        assert_eq!(p.submit(|| {}), Err(Rejected("t-full")));
        gate.wait();
        wait_idle(p);
        let s = p.stats();
        assert_eq!((s.completed, s.rejected), (2, 1));
    }

    #[test]
    fn submit_owning_hands_the_value_back_when_refused() {
        let p = leak(Pool::new("t-own", 1, 0));
        let gate = Arc::new(Barrier::new(2));
        let g = gate.clone();
        p.submit_owning(7u32, move |v| {
            assert_eq!(v, 7);
            g.wait();
        })
        .unwrap();
        match p.submit_owning(String::from("reply"), |_| panic!("must not run")) {
            Err((Rejected("t-own"), v)) => assert_eq!(v, "reply"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        gate.wait();
        wait_idle(p);
    }

    #[test]
    fn zero_capacity_queue_admits_only_worker_slots() {
        let p = leak(Pool::new("t-noq", 2, 0));
        let gate = Arc::new(Barrier::new(3));
        for _ in 0..2 {
            let g = gate.clone();
            p.submit(move || {
                g.wait();
            })
            .unwrap();
        }
        assert!(p.submit(|| {}).is_err());
        gate.wait();
        wait_idle(p);
    }

    #[test]
    fn a_panicking_job_keeps_the_worker_and_the_queue_running() {
        let p = leak(Pool::new("t-panic", 1, 16));
        let ran = Arc::new(AtomicBool::new(false));
        let r = ran.clone();
        p.submit(|| panic!("boom")).unwrap();
        p.submit(move || r.store(true, Ordering::SeqCst)).unwrap();
        wait_idle(p);
        assert!(ran.load(Ordering::SeqCst));
        let s = p.stats();
        assert_eq!((s.panicked, s.completed, s.active), (1, 2, 0));
    }

    #[test]
    fn workers_exit_when_idle_and_restart_on_demand() {
        let p = leak(Pool::new("t-idle", 2, 4));
        p.submit(|| {}).unwrap();
        wait_idle(p);
        assert_eq!(p.stats().active, 0);
        let ran = Arc::new(AtomicBool::new(false));
        let r = ran.clone();
        p.submit(move || r.store(true, Ordering::SeqCst)).unwrap();
        wait_idle(p);
        assert!(ran.load(Ordering::SeqCst));
    }

    #[test]
    fn runs_queued_jobs_in_submission_order() {
        let p = leak(Pool::new("t-fifo", 1, 64));
        let order = Arc::new(Mutex::new(Vec::new()));
        for i in 0..50 {
            let o = order.clone();
            p.submit(move || o.lock().unwrap().push(i)).unwrap();
        }
        wait_idle(p);
        assert_eq!(*order.lock().unwrap(), (0..50).collect::<Vec<_>>());
    }

    #[test]
    fn process_thread_count_stays_bounded_under_a_submit_storm() {
        fn threads() -> usize {
            std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|s| s.lines().find(|l| l.starts_with("Threads:")).map(|l| l[8..].trim().parse().unwrap()))
                .unwrap_or(0)
        }
        let p = leak(Pool::new("t-storm", 4, 100_000));
        let before = threads();
        let peak = Arc::new(AtomicUsize::new(0));
        for _ in 0..20_000 {
            let peak = peak.clone();
            let _ = p.submit(move || {
                peak.fetch_max(threads(), Ordering::Relaxed);
            });
        }
        wait_idle(p);
        // Other tests run concurrently, so allow them some slack; the unbounded
        // 0.1.76 behaviour would add thousands here.
        assert!(peak.load(Ordering::Relaxed) <= before + 4 + 64, "peak {} vs before {}", peak.load(Ordering::Relaxed), before);
    }

    #[test]
    fn service_budget_is_enforced() {
        let budget = Budget::new(3);
        let handles: Vec<_> = (0..3).map(|_| spawn_within(&budget, "t-svc", || {}).unwrap()).collect();
        assert!(spawn_within(&budget, "t-svc", || {}).is_err());
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn run_chunked_runs_everything_with_bounded_width() {
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicUsize::new(0));
        let jobs: Vec<Box<dyn FnOnce() + Send>> = (0..37)
            .map(|_| {
                let (live, peak, done) = (live.clone(), peak.clone(), done.clone());
                Box::new(move || {
                    let n = live.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(n, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(2));
                    live.fetch_sub(1, Ordering::SeqCst);
                    done.fetch_add(1, Ordering::SeqCst);
                }) as Box<dyn FnOnce() + Send>
            })
            .collect();
        run_chunked(jobs, 5);
        assert_eq!(done.load(Ordering::SeqCst), 37);
        assert!(peak.load(Ordering::SeqCst) <= 5);
    }

    #[test]
    fn max_threads_adds_up() {
        let pools: usize = POOLS.iter().map(|p| p.max_workers()).sum();
        assert_eq!(MAX_THREADS, pools + MAX_SERVICES + MAX_SCOPED_THREADS + 1 + MAX_HTTP_CLIENT_THREADS);
    }
}
