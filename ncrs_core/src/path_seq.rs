//! Per-path FIFO ordering of server mutations: tickets are taken on the FUSE thread in
//! operation order and a worker waits for its ticket before the network call. Ids come from
//! one global counter, so the oldest live ticket is always runnable and waits cannot deadlock.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
#[cfg(test)]
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    /// The path itself is created, replaced, moved or removed.
    Exclusive,
    /// Something inside the path (a directory) changes; runs alongside other `Shared`.
    Shared,
}

#[derive(Default)]
struct State {
    next_id: u64,
    queues: HashMap<PathBuf, VecDeque<(u64, Access)>>,
}

#[derive(Default)]
pub(crate) struct PathSeq {
    state: Mutex<State>,
    cv: Condvar,
}

pub(crate) struct Ticket {
    seq: Arc<PathSeq>,
    id: u64,
    paths: Vec<(PathBuf, Access)>,
}

impl PathSeq {
    /// Queues a ticket on every path; a path listed twice keeps its strongest access.
    pub(crate) fn ticket(self: &Arc<Self>, paths: &[(&Path, Access)]) -> Ticket {
        let mut merged: Vec<(PathBuf, Access)> = Vec::new();
        for (p, a) in paths {
            match merged.iter_mut().find(|(q, _)| q.as_path() == *p) {
                Some(existing) => {
                    if *a == Access::Exclusive {
                        existing.1 = Access::Exclusive;
                    }
                }
                None => merged.push((p.to_path_buf(), *a)),
            }
        }
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.next_id += 1;
        let id = st.next_id;
        for (p, a) in &merged {
            st.queues.entry(p.clone()).or_default().push_back((id, *a));
        }
        Ticket { seq: Arc::clone(self), id, paths: merged }
    }
}

impl Ticket {
    fn ready(&self, st: &State) -> bool {
        self.paths.iter().all(|(p, mine)| {
            let Some(q) = st.queues.get(p) else { return true };
            q.iter()
                .take_while(|(id, _)| *id != self.id)
                .all(|(_, earlier)| *mine == Access::Shared && *earlier == Access::Shared)
        })
    }

    /// Blocks until every earlier conflicting ticket on these paths has been dropped.
    pub(crate) fn wait(&self) {
        let mut st = self.seq.state.lock().unwrap_or_else(|e| e.into_inner());
        while !self.ready(&st) {
            st = self.seq.cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Like `wait`, but gives up after `timeout`; returns whether the ticket is ready.
    #[cfg(test)]
    pub(crate) fn wait_timeout(&self, timeout: Duration) -> bool {
        let st = self.seq.state.lock().unwrap_or_else(|e| e.into_inner());
        let (st, _) = self.seq.cv
            .wait_timeout_while(st, timeout, |st| !self.ready(st))
            .unwrap_or_else(|e| e.into_inner());
        self.ready(&st)
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut st = self.seq.state.lock().unwrap_or_else(|e| e.into_inner());
        for (p, _) in &self.paths {
            if let Some(q) = st.queues.get_mut(p) {
                q.retain(|(id, _)| *id != self.id);
                if q.is_empty() {
                    st.queues.remove(p);
                }
            }
        }
        drop(st);
        self.seq.cv.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use Access::{Exclusive, Shared};

    const SHORT: Duration = Duration::from_millis(50);

    fn p(s: &str) -> &Path {
        Path::new(s)
    }

    #[test]
    fn same_path_runs_in_ticket_order() {
        let seq = Arc::new(PathSeq::default());
        let put = seq.ticket(&[(p("/f"), Exclusive)]);
        let mv = seq.ticket(&[(p("/f"), Exclusive), (p("/g"), Exclusive)]);
        assert!(put.wait_timeout(SHORT));
        assert!(!mv.wait_timeout(SHORT), "a MOVE must wait for the earlier upload of its source");
        drop(put);
        assert!(mv.wait_timeout(SHORT));
    }

    #[test]
    fn child_upload_waits_for_parent_mkcol_but_siblings_run_together() {
        let seq = Arc::new(PathSeq::default());
        let mkcol = seq.ticket(&[(p("/d"), Exclusive), (p("/"), Shared)]);
        let put_a = seq.ticket(&[(p("/d/a"), Exclusive), (p("/d"), Shared)]);
        let put_b = seq.ticket(&[(p("/d/b"), Exclusive), (p("/d"), Shared)]);
        assert!(!put_a.wait_timeout(SHORT));
        drop(mkcol);
        assert!(put_a.wait_timeout(SHORT));
        assert!(put_b.wait_timeout(SHORT), "siblings share the parent and must not serialize");
    }

    #[test]
    fn directory_removal_waits_for_uploads_inside_it() {
        let seq = Arc::new(PathSeq::default());
        let put = seq.ticket(&[(p("/d/a"), Exclusive), (p("/d"), Shared)]);
        let rmdir = seq.ticket(&[(p("/d"), Exclusive), (p("/"), Shared)]);
        assert!(!rmdir.wait_timeout(SHORT));
        drop(put);
        assert!(rmdir.wait_timeout(SHORT));
    }

    #[test]
    fn unrelated_paths_do_not_block() {
        let seq = Arc::new(PathSeq::default());
        let _a = seq.ticket(&[(p("/a"), Exclusive)]);
        let b = seq.ticket(&[(p("/b"), Exclusive)]);
        assert!(b.wait_timeout(SHORT));
    }

    #[test]
    fn duplicate_path_keeps_exclusive_access() {
        let seq = Arc::new(PathSeq::default());
        let first = seq.ticket(&[(p("/d"), Shared)]);
        let second = seq.ticket(&[(p("/d"), Shared), (p("/d"), Exclusive)]);
        assert!(!second.wait_timeout(SHORT));
        drop(first);
        assert!(second.wait_timeout(SHORT));
    }

    #[test]
    fn crossing_multi_path_tickets_never_deadlock() {
        let seq = Arc::new(PathSeq::default());
        let t1 = seq.ticket(&[(p("/a"), Exclusive), (p("/b"), Exclusive)]);
        let t2 = seq.ticket(&[(p("/b"), Exclusive), (p("/a"), Exclusive)]);
        let (tx, rx) = mpsc::channel();
        let h = std::thread::spawn(move || {
            t2.wait();
            tx.send(()).unwrap();
        });
        assert!(rx.recv_timeout(SHORT).is_err(), "t2 is behind t1 on both paths");
        drop(t1);
        rx.recv_timeout(Duration::from_secs(5)).expect("t2 must run once t1 is dropped");
        h.join().unwrap();
    }

    #[test]
    fn dropped_ticket_clears_its_queues() {
        let seq = Arc::new(PathSeq::default());
        drop(seq.ticket(&[(p("/a"), Exclusive)]));
        assert!(seq.state.lock().unwrap().queues.is_empty());
    }
}
