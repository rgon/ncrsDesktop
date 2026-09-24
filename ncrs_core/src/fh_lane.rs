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
//! done right away under a [`LaneGuard`] (the lane was idle and the step is
//! cheap) or handed to a pool; the next step starts only once the previous one
//! has finished, on whichever thread finished it. A step the pool refuses runs
//! on the thread that tried to start it, so nothing is dropped or reordered.
//!
//! A pool step returns its reply instead of sending it, and the lane moves on
//! before the reply goes out. The kernel sends a handle's next write, or its
//! FLUSH and RELEASE, only after that reply, so a plain sequential writer always
//! finds the lane idle and is answered on the dispatch thread; only the steps
//! that really overlap (writeback of a shared mapping) queue.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::bg::Pool;

type Job = Box<dyn FnOnce() -> Box<dyn FnOnce()> + Send + 'static>;

struct Step {
    pool: &'static Pool,
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

// Starts the lane's next step when the current one ends, unwinding included:
// a panicking job must not wedge every later write on its handle.
struct Advance(Arc<FhLanes>, u64);

impl Drop for Advance {
    fn drop(&mut self) {
        self.0.advance(self.1);
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
    /// then what it returns (its reply) once the lane has moved on.
    pub fn run<D: FnOnce() + 'static>(
        self: &Arc<Self>,
        fh: u64,
        pool: &'static Pool,
        job: impl FnOnce() -> D + Send + 'static,
    ) {
        let step = Step { pool, job: Box::new(move || Box::new(job()) as Box<dyn FnOnce()>) };
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

    // Starts `step`, which now owns `fh`'s lane.
    fn start(self: &Arc<Self>, fh: u64, step: Step) {
        let lanes = self.clone();
        if let Err((_, job)) = step.pool.submit_owning(step.job, move |job| {
            let next = Advance(lanes, fh);
            let reply = job();
            drop(next);
            reply();
        }) {
            // The pool is full: run it here rather than drop or reorder it.
            let next = Advance(self.clone(), fh);
            let reply = job();
            drop(next);
            reply();
        }
    }

    // Starts the next queued step on `fh`, or retires the lane if none waits.
    fn advance(self: &Arc<Self>, fh: u64) {
        let next = {
            let mut l = self.lock();
            match l.get_mut(&fh).and_then(VecDeque::pop_front) {
                Some(step) => step,
                None => {
                    l.remove(&fh);
                    return;
                }
            }
        };
        self.start(fh, next);
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
            lanes.run(7, p, move || {
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
        lanes.run(1, p, move || {
            g.wait();
            o.lock().unwrap().push("graduation");
            || {}
        });
        assert!(lanes.claim(1).is_none(), "an in-flight step keeps the lane");
        let o = order.clone();
        lanes.run(1, p, move || {
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
        lanes.run(3, p, move || {
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
        lanes.run(1, p, move || {
            g.wait();
            || {}
        });
        let caller = std::thread::current().id();
        let ran_on = Arc::new(Mutex::new(None));
        let r = ran_on.clone();
        lanes.run(2, p, move || {
            *r.lock().unwrap() = Some(std::thread::current().id());
            || {}
        });
        assert_eq!(*ran_on.lock().unwrap(), Some(caller), "refused, so it ran inline before run() returned");
        gate.wait();
        wait_idle(&lanes);
    }

    #[test]
    fn the_reply_goes_out_after_the_lane_moved_on() {
        // So the kernel's next request on the handle, which it only sends after
        // this reply, finds an idle lane and is answered inline.
        let p = pool("t-lane-reply", 2, 8);
        let lanes = FhLanes::new();
        let seen = Arc::new(Mutex::new(None));
        let (l, s) = (lanes.clone(), seen.clone());
        lanes.run(4, p, move || {
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
        lanes.run(9, p, || -> fn() { panic!("boom") });
        let r = ran.clone();
        lanes.run(9, p, move || {
            r.fetch_add(1, Ordering::SeqCst);
            || {}
        });
        wait_idle(&lanes);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }
}
