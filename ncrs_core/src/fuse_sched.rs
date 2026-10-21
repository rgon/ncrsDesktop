//! Per-key FIFO scheduling of FUSE requests on a fixed worker pool.
//!
//! fuser runs one dispatch thread. Serving every request on it froze the whole
//! mount behind any slow one (a PROPFIND for an uncached lookup, a chunk upload
//! inside write()). Raising fuser's own thread count is not an option: the
//! request bodies rely on running in arrival order (a RELEASE that is still
//! committing when the RENAME of the same file arrives uploads the save under
//! the temp name), and a mutex per file gives exclusion, not order.
//!
//! So the dispatch thread only *orders*: it names the keys (inodes, handles) a
//! request touches and calls [`Scheduler::submit`], which appends the task to
//! the FIFO queue of every one of those keys and returns at once. A task runs
//! when it is at the head of all of its queues; tasks on disjoint keys run in
//! parallel on the pool. Consecutive [`Mode::Shared`] tasks at the head of a
//! queue run together; an [`Mode::Exclusive`] one runs alone.
//!
//! Deadlock freedom is structural: every queue is appended to by the one
//! dispatch thread, in one global id order, so the oldest live task is at the
//! head of every queue it is on and is always runnable (the same argument as
//! `path_seq.rs`). Only runnable tasks are handed to the pool, so a worker never
//! waits for its turn.
//!
//! The pool is bounded (`bg::FUSE`), and so is the number of tasks in flight:
//! past `max_inflight` the dispatch thread waits for one to finish,
//! which under extreme load is just the old one-at-a-time behaviour again.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::bg;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Runs alongside the other `Shared` tasks at the head of the queue.
    Shared,
    /// Runs alone: waits for every earlier task on the key and holds back every later one.
    Exclusive,
}

type Job = Box<dyn FnOnce() + Send + 'static>;

struct Task<K> {
    keys: Vec<(K, Mode)>,
    // Taken when the task starts; `None` from then until it completes.
    job: Option<Job>,
}

struct State<K> {
    next_id: u64,
    // Live (waiting or running) task ids per key, in submission order.
    queues: HashMap<K, VecDeque<(u64, Mode)>>,
    tasks: HashMap<u64, Task<K>>,
    peak_inflight: usize,
    dispatch_waits: u64,
    // Set by `quiesce`: no new task is taken until then, so requests wait in the kernel.
    paused_until: Option<Instant>,
}

pub(crate) struct Scheduler<K> {
    pool: &'static bg::Pool,
    max_inflight: usize,
    state: Mutex<State<K>>,
    room: Condvar,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SchedStats {
    pub inflight: usize,
    pub peak_inflight: usize,
    pub busy_keys: usize,
    /// Times the dispatch thread had to wait for room.
    pub dispatch_waits: u64,
}

impl<K: Copy + Eq + Hash + Send + 'static> Scheduler<K> {
    /// `pool`'s queue must hold `max_inflight` jobs, so a runnable task is never refused.
    pub(crate) fn new(pool: &'static bg::Pool, max_inflight: usize) -> Arc<Self> {
        Arc::new(Scheduler {
            pool,
            max_inflight: max_inflight.max(1),
            state: Mutex::new(State {
                next_id: 0,
                queues: HashMap::new(),
                tasks: HashMap::new(),
                peak_inflight: 0,
                dispatch_waits: 0,
                paused_until: None,
            }),
            room: Condvar::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State<K>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queues `job` behind every earlier task on `keys` (a key listed twice keeps its
    /// strongest mode). No keys: runs as soon as a worker is free. Blocks only while
    /// `max_inflight` tasks are already live.
    pub(crate) fn submit(self: &Arc<Self>, keys: &[(K, Mode)], job: impl FnOnce() + Send + 'static) {
        let mut merged: Vec<(K, Mode)> = Vec::with_capacity(keys.len());
        for &(k, m) in keys {
            match merged.iter_mut().find(|(q, _)| *q == k) {
                Some(existing) => {
                    if m == Mode::Exclusive {
                        existing.1 = Mode::Exclusive;
                    }
                }
                None => merged.push((k, m)),
            }
        }
        let mut st = self.lock();
        while let Some(until) = st.paused_until {
            let now = Instant::now();
            if now >= until {
                st.paused_until = None;
                break;
            }
            st = self.room.wait_timeout(st, until - now).unwrap_or_else(|e| e.into_inner()).0;
        }
        if st.tasks.len() >= self.max_inflight {
            st.dispatch_waits += 1;
            let waits = st.dispatch_waits;
            if waits.is_power_of_two() {
                log::warn!("fuse scheduler full ({} requests in flight): dispatch waits (#{})", self.max_inflight, waits);
            }
            while st.tasks.len() >= self.max_inflight {
                st = self.room.wait(st).unwrap_or_else(|e| e.into_inner());
            }
        }
        st.next_id += 1;
        let id = st.next_id;
        for &(k, m) in &merged {
            st.queues.entry(k).or_default().push_back((id, m));
        }
        st.tasks.insert(id, Task { keys: merged, job: Some(Box::new(job)) });
        st.peak_inflight = st.peak_inflight.max(st.tasks.len());
        let start = if ready(&st, id) { take_job(&mut st, id) } else { None };
        drop(st);
        if let Some(job) = start {
            self.start(id, job);
        }
    }

    /// Hands a runnable task to the pool. Never called with the state lock held: a job
    /// the pool drops unrun completes through `Done`'s drop, which takes the lock.
    fn start(self: &Arc<Self>, id: u64, job: Job) {
        let done = Done { sched: Arc::clone(self), id };
        let submitted = self.pool.submit(move || {
            let _done = done;
            job();
        });
        if let Err(r) = submitted {
            // The pool's queue is sized to max_inflight, so only a failed thread start
            // lands here. The job (and any reply it owned, which answers EIO on drop) is
            // gone; `Done` has already released its keys.
            log::error!("fuse scheduler: {} — request dropped", r);
        }
    }

    fn complete(self: &Arc<Self>, id: u64) {
        let mut st = self.lock();
        let Some(task) = st.tasks.remove(&id) else { return };
        let mut runnable: Vec<(u64, Job)> = Vec::new();
        for (k, _) in &task.keys {
            let Some(q) = st.queues.get_mut(k) else { continue };
            if let Some(pos) = q.iter().position(|(t, _)| *t == id) {
                q.remove(pos);
            }
            if q.is_empty() {
                st.queues.remove(k);
                continue;
            }
            // Only a head that has not started yet can have been waiting on this task
            // here: a started head means nothing behind it was blocked by `id` on this
            // key. From a waiting head, the candidates are it and, if it is shared, the
            // shared run behind it.
            let candidates: Vec<u64> = {
                let q = &st.queues[k];
                let head = q[0];
                if st.tasks.get(&head.0).is_some_and(|t| t.job.is_none()) {
                    continue;
                }
                if head.1 == Mode::Exclusive {
                    vec![head.0]
                } else {
                    q.iter().take_while(|(_, m)| *m == Mode::Shared).map(|(t, _)| *t).collect()
                }
            };
            for c in candidates {
                if st.tasks.get(&c).is_some_and(|t| t.job.is_some()) && ready(&st, c) {
                    if let Some(job) = take_job(&mut st, c) {
                        runnable.push((c, job));
                    }
                }
            }
        }
        drop(st);
        self.room.notify_all();
        for (c, job) in runnable {
            self.start(c, job);
        }
    }

    /// For a planned daemon restart: stops taking new tasks (the dispatch thread waits, so
    /// further requests stay queued in the kernel for the next daemon) and waits up to
    /// `budget` for every task in flight to finish; true when none is left. The pause
    /// lifts by itself a few seconds after the budget, so a restart that then fails
    /// leaves the mount working again rather than stuck.
    pub(crate) fn quiesce(&self, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        let mut st = self.lock();
        st.paused_until = Some(deadline + Duration::from_secs(5));
        while !st.tasks.is_empty() {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            st = self.room.wait_timeout(st, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
        }
        true
    }

    /// Keys with a task waiting or running on them right now.
    pub(crate) fn busy_keys(&self) -> Vec<K> {
        self.lock().queues.keys().copied().collect()
    }

    #[cfg(test)]
    pub(crate) fn stats(&self) -> SchedStats {
        let st = self.lock();
        SchedStats {
            inflight: st.tasks.len(),
            peak_inflight: st.peak_inflight,
            busy_keys: st.queues.len(),
            dispatch_waits: st.dispatch_waits,
        }
    }
}

/// Whether every task ahead of `id` on each of its keys is compatible with it.
fn ready<K: Eq + Hash>(st: &State<K>, id: u64) -> bool {
    let Some(task) = st.tasks.get(&id) else { return false };
    task.keys.iter().all(|(k, mine)| {
        let Some(q) = st.queues.get(k) else { return true };
        q.iter()
            .take_while(|(t, _)| *t != id)
            .all(|(_, earlier)| *mine == Mode::Shared && *earlier == Mode::Shared)
    })
}

fn take_job<K>(st: &mut State<K>, id: u64) -> Option<Job> {
    st.tasks.get_mut(&id).and_then(|t| t.job.take())
}

/// Completes its task when dropped: after the job returns, if it panics, or if the
/// pool drops it unrun — so a key is never left held by a task that will not finish.
struct Done<K: Copy + Eq + Hash + Send + 'static> {
    sched: Arc<Scheduler<K>>,
    id: u64,
}

impl<K: Copy + Eq + Hash + Send + 'static> Drop for Done<K> {
    fn drop(&mut self) {
        self.sched.complete(self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn sched(name: &'static str, workers: usize, inflight: usize) -> Arc<Scheduler<u32>> {
        let pool: &'static bg::Pool = Box::leak(Box::new(bg::Pool::new(name, workers, inflight)));
        Scheduler::new(pool, inflight)
    }

    fn wait_drained(s: &Scheduler<u32>) {
        let t = Instant::now();
        while s.stats().inflight > 0 {
            assert!(t.elapsed() < Duration::from_secs(20), "scheduler never drained: {:?}", s.stats());
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn quiesce_waits_for_running_tasks_and_holds_new_ones_back_until_it_lifts() {
        let s = sched("t-quiesce", 2, 64);
        let (go, gate) = std::sync::mpsc::channel::<()>();
        let done = Arc::new(Mutex::new(Vec::new()));
        let d = done.clone();
        s.submit(&[(1, Mode::Exclusive)], move || {
            gate.recv().unwrap();
            d.lock().unwrap().push(1);
        });
        assert!(!s.quiesce(Duration::from_millis(50)), "not idle while a task runs");
        go.send(()).unwrap();
        assert!(s.quiesce(Duration::from_millis(500)), "idle once it finished");
        assert_eq!(*done.lock().unwrap(), vec![1]);
        // While paused a submit (the dispatch thread) waits; the pause lifts on its own.
        let paused_at = Instant::now();
        std::thread::scope(|scope| {
            let d = done.clone();
            let s2 = s.clone();
            scope.spawn(move || s2.submit(&[(2, Mode::Exclusive)], move || d.lock().unwrap().push(2)));
            std::thread::sleep(Duration::from_millis(300));
            assert_eq!(done.lock().unwrap().len(), 1, "nothing is taken while paused");
        });
        wait_drained(&s);
        assert_eq!(*done.lock().unwrap(), vec![1, 2]);
        assert!(paused_at.elapsed() >= Duration::from_secs(4), "the pause lasted until it lifted");
    }

    /// Tiny deterministic xorshift, so a failure reproduces.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[test]
    fn exclusive_tasks_on_one_key_run_in_submission_order() {
        let s = sched("t-sched-fifo", 8, 4096);
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut rng = Rng(7);
        for i in 0..300 {
            let o = order.clone();
            let us = rng.below(200);
            s.submit(&[(1, Mode::Exclusive)], move || {
                std::thread::sleep(Duration::from_micros(us));
                o.lock().unwrap().push(i);
            });
        }
        wait_drained(&s);
        assert_eq!(*order.lock().unwrap(), (0..300).collect::<Vec<_>>());
    }

    #[test]
    fn tasks_on_different_keys_run_in_parallel() {
        let s = sched("t-sched-par", 4, 64);
        // Each task waits for the other to have started: serialised, neither finishes.
        let (tx_a, rx_a) = mpsc::channel::<()>();
        let (tx_b, rx_b) = mpsc::channel::<()>();
        let ok = Arc::new(AtomicUsize::new(0));
        let ok_a = ok.clone();
        s.submit(&[(1, Mode::Exclusive)], move || {
            tx_a.send(()).unwrap();
            if rx_b.recv_timeout(Duration::from_secs(5)).is_ok() {
                ok_a.fetch_add(1, Ordering::SeqCst);
            }
        });
        let ok_b = ok.clone();
        s.submit(&[(2, Mode::Exclusive)], move || {
            tx_b.send(()).unwrap();
            if rx_a.recv_timeout(Duration::from_secs(5)).is_ok() {
                ok_b.fetch_add(1, Ordering::SeqCst);
            }
        });
        wait_drained(&s);
        assert_eq!(ok.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_slow_task_holds_back_only_its_own_keys() {
        let s = sched("t-sched-slow", 4, 64);
        let (release_tx, release_rx) = mpsc::channel::<()>();
        s.submit(&[(1, Mode::Exclusive)], move || {
            let _ = release_rx.recv_timeout(Duration::from_secs(10));
        });
        let behind = Arc::new(AtomicUsize::new(0));
        let b = behind.clone();
        s.submit(&[(1, Mode::Shared)], move || {
            b.fetch_add(1, Ordering::SeqCst);
        });
        let (done_tx, done_rx) = mpsc::channel::<()>();
        for k in 2..10 {
            let d = done_tx.clone();
            s.submit(&[(k, Mode::Exclusive), (k + 100, Mode::Shared)], move || d.send(()).unwrap());
        }
        for _ in 2..10 {
            done_rx.recv_timeout(Duration::from_secs(5)).expect("unrelated task blocked behind a slow one");
        }
        assert_eq!(behind.load(Ordering::SeqCst), 0, "a task overtook the slow one on its key");
        release_tx.send(()).unwrap();
        wait_drained(&s);
        assert_eq!(behind.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn consecutive_shared_tasks_run_together() {
        let s = sched("t-sched-shared", 8, 64);
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        for _ in 0..4 {
            let (live, peak) = (live.clone(), peak.clone());
            s.submit(&[(1, Mode::Shared)], move || {
                let n = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(n, Ordering::SeqCst);
                // Wait (bounded) until all four are in.
                let t = Instant::now();
                while live.load(Ordering::SeqCst) < 4 && t.elapsed() < Duration::from_secs(5) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                std::thread::sleep(Duration::from_millis(5));
                live.fetch_sub(1, Ordering::SeqCst);
            });
        }
        wait_drained(&s);
        assert_eq!(peak.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn an_exclusive_task_waits_for_running_readers_and_later_readers_wait_for_it() {
        let s = sched("t-sched-rw", 8, 64);
        let log = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let gate_rx = Arc::new(Mutex::new(gate_rx));
        let started = Arc::new(AtomicUsize::new(0));
        for name in ["r1-end", "r2-end"] {
            let (log, gate_rx, started) = (log.clone(), gate_rx.clone(), started.clone());
            s.submit(&[(1, Mode::Shared)], move || {
                started.fetch_add(1, Ordering::SeqCst);
                let _ = gate_rx.lock().unwrap().recv_timeout(Duration::from_secs(10));
                log.lock().unwrap().push(name);
            });
        }
        let l = log.clone();
        s.submit(&[(1, Mode::Exclusive)], move || l.lock().unwrap().push("writer"));
        let l = log.clone();
        s.submit(&[(1, Mode::Shared)], move || l.lock().unwrap().push("r3"));
        let t = Instant::now();
        while started.load(Ordering::SeqCst) < 2 {
            assert!(t.elapsed() < Duration::from_secs(5), "readers never started together");
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(Duration::from_millis(30));
        assert!(log.lock().unwrap().is_empty(), "writer or a later reader ran while readers held the key");
        gate_tx.send(()).unwrap();
        gate_tx.send(()).unwrap();
        wait_drained(&s);
        let log = log.lock().unwrap();
        assert_eq!(&log[2..], &["writer", "r3"]);
    }

    #[test]
    fn a_multi_key_task_waits_for_the_head_of_every_queue() {
        let s = sched("t-sched-multi", 8, 64);
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let log = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let l = log.clone();
        s.submit(&[(2, Mode::Exclusive)], move || {
            let _ = gate_rx.recv_timeout(Duration::from_secs(10));
            l.lock().unwrap().push("on-2");
        });
        let l = log.clone();
        s.submit(&[(1, Mode::Exclusive), (2, Mode::Exclusive)], move || l.lock().unwrap().push("on-1-and-2"));
        // Queued behind the two-key task on key 1: must not overtake it.
        let l = log.clone();
        s.submit(&[(1, Mode::Shared)], move || l.lock().unwrap().push("on-1"));
        std::thread::sleep(Duration::from_millis(30));
        assert!(log.lock().unwrap().is_empty());
        gate_tx.send(()).unwrap();
        wait_drained(&s);
        assert_eq!(*log.lock().unwrap(), vec!["on-2", "on-1-and-2", "on-1"]);
    }

    #[test]
    fn interleaved_multi_key_tasks_never_deadlock_and_keep_per_key_order() {
        // Cycles of overlapping key sets — the shape that deadlocks lock-per-key
        // schemes acquiring in different orders — plus random mixes, random modes.
        let s = sched("t-sched-mix", 8, 512);
        let clock = Arc::new(AtomicU64::new(0));
        let n = 3000usize;
        let spans = Arc::new(Mutex::new(vec![(0u64, 0u64); n]));
        let mut sets: Vec<Vec<(u32, Mode)>> = Vec::with_capacity(n);
        let mut rng = Rng(0x9e3779b97f4a7c15);
        for i in 0..n {
            let mode = |r: &mut Rng| if r.below(3) == 0 { Mode::Exclusive } else { Mode::Shared };
            let keys = if i % 3 == 0 {
                // a→b, b→c, c→a ... as in rename(dir1, dir2) / rename(dir2, dir3) / ...
                let a = (i / 3 % 5) as u32;
                vec![(a, Mode::Exclusive), ((a + 1) % 5, Mode::Exclusive)]
            } else {
                let k = 1 + rng.below(3) as usize;
                (0..k).map(|_| (rng.below(8) as u32, mode(&mut rng))).collect()
            };
            sets.push(keys);
        }
        for (i, keys) in sets.iter().enumerate() {
            let (clock, spans) = (clock.clone(), spans.clone());
            let us = rng.below(50);
            s.submit(keys, move || {
                let start = clock.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_micros(us));
                let end = clock.fetch_add(1, Ordering::SeqCst);
                spans.lock().unwrap()[i] = (start, end);
            });
        }
        wait_drained(&s);
        let spans = spans.lock().unwrap();
        // Strongest mode per key per task, as submit merges them.
        let strongest = |set: &[(u32, Mode)], k: u32| {
            set.iter().filter(|(q, _)| *q == k).map(|(_, m)| *m).fold(None, |acc, m| match (acc, m) {
                (Some(Mode::Exclusive), _) | (_, Mode::Exclusive) => Some(Mode::Exclusive),
                _ => Some(Mode::Shared),
            })
        };
        for k in 0..8u32 {
            let on_k: Vec<(usize, Mode)> = (0..n).filter_map(|i| strongest(&sets[i], k).map(|m| (i, m))).collect();
            for (x, &(a, ma)) in on_k.iter().enumerate() {
                for &(b, mb) in &on_k[x + 1..] {
                    if ma == Mode::Exclusive || mb == Mode::Exclusive {
                        assert!(spans[a].1 < spans[b].0, "key {}: task {} ({:?}) overlapped or followed task {} ({:?})", k, a, ma, b, mb);
                    }
                }
            }
        }
    }

    #[test]
    fn dispatch_waits_for_room_when_full() {
        let s = sched("t-sched-room", 2, 2);
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let gate_rx = Arc::new(Mutex::new(gate_rx));
        for k in 0..2 {
            let g = gate_rx.clone();
            s.submit(&[(k, Mode::Exclusive)], move || {
                let _ = g.lock().unwrap().recv_timeout(Duration::from_secs(10));
            });
        }
        std::thread::scope(|scope| {
            let (sub_tx, sub_rx) = mpsc::channel::<()>();
            let s2 = s.clone();
            scope.spawn(move || {
                s2.submit(&[(9, Mode::Exclusive)], || {});
                sub_tx.send(()).unwrap();
            });
            assert!(sub_rx.recv_timeout(Duration::from_millis(100)).is_err(), "submit did not wait for room");
            gate_tx.send(()).unwrap();
            sub_rx.recv_timeout(Duration::from_secs(5)).expect("submit never got room");
            gate_tx.send(()).unwrap();
        });
        wait_drained(&s);
        assert!(s.stats().dispatch_waits >= 1);
    }

    #[test]
    fn a_panicking_task_releases_its_keys() {
        let s = sched("t-sched-panic", 2, 16);
        s.submit(&[(1, Mode::Exclusive)], || panic!("boom"));
        let ran = Arc::new(AtomicUsize::new(0));
        let r = ran.clone();
        s.submit(&[(1, Mode::Exclusive)], move || {
            r.fetch_add(1, Ordering::SeqCst);
        });
        wait_drained(&s);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert_eq!(s.stats().busy_keys, 0);
    }

    #[test]
    fn duplicate_keys_keep_the_strongest_mode() {
        let s = sched("t-sched-dup", 4, 16);
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        s.submit(&[(1, Mode::Shared)], move || {
            let _ = gate_rx.recv_timeout(Duration::from_secs(10));
        });
        let ran = Arc::new(AtomicUsize::new(0));
        let r = ran.clone();
        // Shared + Exclusive on the same key = Exclusive: must wait for the reader.
        s.submit(&[(1, Mode::Shared), (1, Mode::Exclusive)], move || {
            r.fetch_add(1, Ordering::SeqCst);
        });
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        gate_tx.send(()).unwrap();
        wait_drained(&s);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn busy_keys_lists_only_live_keys() {
        let s = sched("t-sched-busy", 2, 16);
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        s.submit(&[(5, Mode::Exclusive), (6, Mode::Shared)], move || {
            let _ = gate_rx.recv_timeout(Duration::from_secs(10));
        });
        let mut busy = s.busy_keys();
        busy.sort();
        assert_eq!(busy, vec![5, 6]);
        gate_tx.send(()).unwrap();
        wait_drained(&s);
        assert!(s.busy_keys().is_empty());
    }
}
