//! Per-handle FIFO for the write-path work taken off the FUSE dispatch thread.
//!
//! `write`, a truncating `setattr`, `flush`, `fsync` and `release` of one file
//! handle must take effect in the order the kernel sent them: a flush answered
//! before the write in front of it has landed reports durability for bytes
//! still in flight, and a write that overtakes the chunk graduation ahead of
//! it appends to a tail file that is being rewritten. With no writeback cache
//! the kernel already serializes write, flush, fsync and truncate per inode
//! (each takes the inode lock), but not the page-cache writeback of a shared
//! mapping, which can have several WRITEs in flight at once. So the order is
//! enforced here rather than assumed.
//!
//! A lane exists while its handle has work queued or running. A step is either
//! done right away under a [`LaneGuard`] (the lane was idle and the step does
//! no I/O at all) or handed to a pool; the next step starts only once the
//! previous one has finished, on whichever thread finished it.
//!
//! A step the pool refuses goes to its spill pool if it has one, and is told
//! which of the two it runs on ([`Ran`]). The write path gives every step that
//! touches the disk a pool that never refuses at the end of that chain
//! (`bg::DISK`, `bg::DISK_SLOW` and `bg::MUTATION` have unbounded queues),
//! so no staging-file I/O runs on the FUSE dispatch thread. Only when both
//! refuse, which for
//! those pools means the OS could not start a worker thread for either, does
//! the step run on the thread that tried to start it, as the
//! last resort that still drops and reorders nothing; that thread can be the
//! FUSE dispatch thread (a `run` on an idle lane, or a [`LaneGuard`] dropped
//! there), so a step that would touch the network does only its local part
//! there. Pool-full is therefore never an error for the kernel.
//!
//! A step's reply goes out after the lane has moved on when nothing waits
//! behind it (the lane is retired first), and before the next step starts when
//! something does, so replies leave in the order the requests came in. The
//! kernel sends a handle's next write, or its FLUSH and RELEASE, only after
//! that reply, so a plain sequential writer always finds the lane idle: its
//! write goes straight to a pool worker with nothing ahead of it, and only the
//! steps that really overlap (writeback of a shared mapping) queue in the
//! lane. Steps queued behind one another are
//! started in a loop, never by recursion, however many are refused in a row.

use std::collections::{HashMap, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};

use crate::bg::Pool;

/// Where a lane step runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ran {
    /// On a worker of the pool it was queued for.
    OnPool,
    /// The pool refused it, so on a worker of its spill pool.
    OnSpill,
    /// Both refused it, so on the thread that started it, which may be the
    /// FUSE dispatch thread: no network here.
    OnCaller,
}

type Job = Box<dyn FnOnce(Ran) -> Box<dyn FnOnce()> + Send + 'static>;

struct Step {
    pool: &'static Pool,
    /// Where the step goes when `pool` refuses it, before the caller; a pool
    /// that never refuses (`bg::MUTATION`) for a step that must not run there.
    spill: Option<&'static Pool>,
    job: Job,
}

#[derive(Default)]
pub(crate) struct FhLanes {
    // A handle is present while its lane is busy; the deque holds what waits.
    lanes: Mutex<HashMap<u64, VecDeque<Step>>>,
}

/// An idle lane taken for work on the current thread; dropping it starts
/// whatever queued behind it meanwhile.
pub(crate) struct LaneGuard {
    lanes: Arc<FhLanes>,
    fh: u64,
}

impl Drop for LaneGuard {
    fn drop(&mut self) {
        self.lanes.advance(self.fh);
    }
}

// Starts the lane's next step if the current one unwinds: a panicking job
// must not wedge every later write on its handle. Disarmed on the normal path,
// where the caller takes the next step itself.
struct OnUnwind(Option<(Arc<FhLanes>, u64)>);

impl Drop for OnUnwind {
    fn drop(&mut self) {
        if let Some((lanes, fh)) = self.0.take() {
            lanes.advance(fh);
        }
    }
}

impl FhLanes {
    pub fn new() -> Arc<Self> {
        Arc::new(FhLanes::default())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, VecDeque<Step>>> {
        self.lanes.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Takes `fh`'s lane for work done on this thread, if nothing is queued
    /// or running on it.
    pub fn claim(self: &Arc<Self>, fh: u64) -> Option<LaneGuard> {
        let mut l = self.lock();
        if l.contains_key(&fh) {
            return None;
        }
        l.insert(fh, VecDeque::new());
        Some(LaneGuard { lanes: self.clone(), fh })
    }

    /// Runs `job` on `pool` once everything already queued on `fh` has finished,
    /// then what it returns (its reply). `job` is told where it runs: if the
    /// pool refuses it, it runs on the thread that started it (see the module
    /// doc), which may be this one, before `run` returns.
    pub fn run<D: FnOnce() + 'static>(
        self: &Arc<Self>,
        fh: u64,
        pool: &'static Pool,
        job: impl FnOnce(Ran) -> D + Send + 'static,
    ) {
        self.run_or_spill(fh, pool, None, job)
    }

    /// `run`, but a step `pool` refuses goes to `spill` before it would run
    /// on the caller. Still in the lane's order: the lane waits for it
    /// wherever it runs.
    pub fn run_or_spill<D: FnOnce() + 'static>(
        self: &Arc<Self>,
        fh: u64,
        pool: &'static Pool,
        spill: Option<&'static Pool>,
        job: impl FnOnce(Ran) -> D + Send + 'static,
    ) {
        let step = Step { pool, spill, job: Box::new(move |ran| Box::new(job(ran)) as Box<dyn FnOnce()>) };
        {
            let mut l = self.lock();
            if let Some(q) = l.get_mut(&fh) {
                q.push_back(step);
                return;
            }
            l.insert(fh, VecDeque::new());
        }
        self.start(fh, step);
    }

    /// Handles with work queued or running, for HEALTH.
    pub fn busy_count(&self) -> usize {
        self.lock().len()
    }

    // Starts `step`, which now owns `fh`'s lane, and every step after it that
    // its pool refuses, on this thread, in order.
    fn start(self: &Arc<Self>, fh: u64, mut step: Step) {
        loop {
            let on_pool = |lanes: Arc<Self>, ran: Ran| move |job: Job| {
                if let Some(next) = lanes.finish_step(fh, job, ran) {
                    lanes.start(fh, next);
                }
            };
            let job = match step.pool.submit_owning(step.job, on_pool(self.clone(), Ran::OnPool)) {
                Ok(()) => return,
                Err((_, job)) => job,
            };
            let job = match step.spill {
                Some(spill) => match spill.submit_owning(job, on_pool(self.clone(), Ran::OnSpill)) {
                    Ok(()) => return,
                    Err((_, job)) => job,
                },
                None => job,
            };
            // Nothing would take it: run it here rather than drop or reorder it.
            match self.finish_step(fh, job, Ran::OnCaller) {
                Some(next) => step = next,
                None => return,
            }
        }
    }

    // Runs one step and sends its reply; returns the step queued behind it,
    // which now owns the lane, or retires the lane (before the reply) if none.
    fn finish_step(self: &Arc<Self>, fh: u64, job: Job, ran: Ran) -> Option<Step> {
        let mut unwind = OnUnwind(Some((self.clone(), fh)));
        let reply = job(ran);
        unwind.0 = None;
        let next = self.next_or_retire(fh);
        // A reply that panics must not lose the steps behind it.
        let _ = catch_unwind(AssertUnwindSafe(reply));
        next
    }

    fn next_or_retire(&self, fh: u64) -> Option<Step> {
        let mut l = self.lock();
        match l.get_mut(&fh).and_then(VecDeque::pop_front) {
            Some(step) => Some(step),
            None => {
                l.remove(&fh);
                None
            }
        }
    }

    // Starts the next queued step on `fh`, or retires the lane if none waits.
    fn advance(self: &Arc<Self>, fh: u64) {
        if let Some(next) = self.next_or_retire(fh) {
            self.start(fh, next);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Barrier;
    use std::time::{Duration, Instant};

    fn pool(name: &'static str, workers: usize, queue: usize) -> &'static Pool {
        Box::leak(Box::new(Pool::new(name, workers, queue)))
    }

    fn wait_idle(lanes: &FhLanes) {
        let t = Instant::now();
        while lanes.busy_count() > 0 {
            assert!(t.elapsed() < Duration::from_secs(10), "lanes never drained");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn steps_on_one_handle_run_in_submission_order_across_pools() {
        let (a, b) = (pool("t-lane-a", 4, 64), pool("t-lane-b", 4, 64));
        let lanes = FhLanes::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        for i in 0..200 {
            let o = order.clone();
            // Alternate pools and vary the step length: order must not depend on either.
            let p = if i % 3 == 0 { a } else { b };
            lanes.run(7, p, move |_| {
                if i % 7 == 0 {
                    std::thread::sleep(Duration::from_millis(1));
                }
                o.lock().unwrap().push(i);
                || {}
            });
        }
        wait_idle(&lanes);
        assert_eq!(*order.lock().unwrap(), (0..200).collect::<Vec<_>>());
    }

    #[test]
    fn a_busy_lane_cannot_be_claimed_and_later_steps_wait_behind_it() {
        let p = pool("t-lane-claim", 2, 8);
        let lanes = FhLanes::new();
        let gate = Arc::new(Barrier::new(2));
        let g = gate.clone();
        let order = Arc::new(Mutex::new(Vec::new()));
        let o = order.clone();
        lanes.run(1, p, move |_| {
            g.wait();
            o.lock().unwrap().push("graduation");
            || {}
        });
        assert!(lanes.claim(1).is_none(), "an in-flight step keeps the lane");
        let o = order.clone();
        lanes.run(1, p, move |_| {
            o.lock().unwrap().push("flush");
            || {}
        });
        // Other handles are independent.
        let other = lanes.claim(2).expect("an idle handle's lane is free");
        drop(other);
        gate.wait();
        wait_idle(&lanes);
        assert_eq!(*order.lock().unwrap(), vec!["graduation", "flush"]);
        assert!(lanes.claim(1).is_some(), "an idle lane can be claimed again");
    }

    #[test]
    fn steps_queued_behind_an_inline_claim_start_when_it_is_released() {
        let p = pool("t-lane-inline", 1, 8);
        let lanes = FhLanes::new();
        let ran = Arc::new(AtomicUsize::new(0));
        let guard = lanes.claim(3).unwrap();
        let r = ran.clone();
        lanes.run(3, p, move |_| {
            r.fetch_add(1, Ordering::SeqCst);
            || {}
        });
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(ran.load(Ordering::SeqCst), 0, "must wait for the inline step");
        drop(guard);
        wait_idle(&lanes);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_refused_step_runs_on_the_caller_in_order() {
        // One worker, no queue: the second handle's step is refused while the
        // first one's holds the only worker.
        let p = pool("t-lane-full", 1, 0);
        let lanes = FhLanes::new();
        let gate = Arc::new(Barrier::new(2));
        let g = gate.clone();
        lanes.run(1, p, move |_| {
            g.wait();
            || {}
        });
        let caller = std::thread::current().id();
        let ran_on = Arc::new(Mutex::new(None));
        let r = ran_on.clone();
        lanes.run(2, p, move |ran| {
            *r.lock().unwrap() = Some((std::thread::current().id(), ran));
            || {}
        });
        assert_eq!(*ran_on.lock().unwrap(), Some((caller, Ran::OnCaller)), "refused, so it ran inline before run() returned, and knew it");
        gate.wait();
        wait_idle(&lanes);
    }

    #[test]
    fn a_long_run_of_refused_steps_runs_iteratively_with_replies_in_order() {
        // A pool that refuses everything, and more steps queued behind an
        // inline claim than a recursive advance could survive on the stack.
        let never = pool("t-lane-never", 0, 0);
        let lanes = FhLanes::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        let guard = lanes.claim(5).unwrap();
        const N: usize = 50_000;
        for i in 0..N {
            let o = order.clone();
            lanes.run(5, never, move |ran| {
                assert_eq!(ran, Ran::OnCaller);
                move || o.lock().unwrap().push(i)
            });
        }
        drop(guard);
        assert_eq!(lanes.busy_count(), 0, "all ran on the thread that released the lane");
        let order = order.lock().unwrap();
        assert_eq!(order.len(), N);
        assert!(order.iter().copied().eq(0..N), "replies left out of order");
    }

    #[test]
    fn a_refused_step_behind_a_pool_step_replies_after_it() {
        let p = pool("t-lane-order", 1, 8);
        let never = pool("t-lane-order-never", 0, 0);
        let lanes = FhLanes::new();
        let gate = Arc::new(Barrier::new(2));
        let replies = Arc::new(Mutex::new(Vec::new()));
        let (g, r) = (gate.clone(), replies.clone());
        lanes.run(6, p, move |ran| {
            assert_eq!(ran, Ran::OnPool);
            g.wait();
            move || r.lock().unwrap().push("first")
        });
        let r = replies.clone();
        lanes.run(6, never, move |ran| {
            assert_eq!(ran, Ran::OnCaller, "refused: runs on whichever thread finished the step ahead");
            move || r.lock().unwrap().push("second")
        });
        gate.wait();
        wait_idle(&lanes);
        assert_eq!(*replies.lock().unwrap(), vec!["first", "second"]);
    }

    #[test]
    fn the_reply_goes_out_after_the_lane_moved_on() {
        // So the kernel's next request on the handle, which it only sends after
        // this reply, finds an idle lane and is answered inline.
        let p = pool("t-lane-reply", 2, 8);
        let lanes = FhLanes::new();
        let seen = Arc::new(Mutex::new(None));
        let (l, s) = (lanes.clone(), seen.clone());
        lanes.run(4, p, move |_| {
            move || {
                *s.lock().unwrap() = Some(l.claim(4).is_some());
            }
        });
        let t = Instant::now();
        while seen.lock().unwrap().is_none() {
            assert!(t.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(*seen.lock().unwrap(), Some(true), "the lane was still busy when the reply was sent");
        wait_idle(&lanes);
    }

    #[test]
    fn a_panicking_step_does_not_wedge_its_lane() {
        let p = pool("t-lane-panic", 1, 8);
        let lanes = FhLanes::new();
        let ran = Arc::new(AtomicUsize::new(0));
        lanes.run(9, p, |_| -> fn() { panic!("boom") });
        let r = ran.clone();
        lanes.run(9, p, move |_| {
            r.fetch_add(1, Ordering::SeqCst);
            || {}
        });
        wait_idle(&lanes);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }
}
