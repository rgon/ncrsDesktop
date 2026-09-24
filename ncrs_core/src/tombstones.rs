//! Local unlinks an open still being resolved could have missed.
//!
//! open() may finish on a worker while unlink() (or a rename over the file)
//! runs on the dispatch thread. unlink marks every handle it finds in
//! `open_files`, but a handle registered after that scan is not there yet.
//! The only thing that may make such a handle "unlinked" is positive evidence
//! that *this* mount removed the file, never the file's absence from a
//! listing: a listing also loses names whose upload failed transiently, whose
//! rename has not reached the server, or that another client deleted — and an
//! unlinked handle's writes are thrown away at release.
//!
//! The evidence is a tombstone: the inode and a generation from a counter
//! that only grows, recorded under the cache lock at the point unlink edits
//! the listing. open() takes a snapshot of the counter under that same lock
//! when it resolves the inode, before any hop to a worker. Registration then
//! inserts the handle into `open_files` and only afterwards reads the
//! tombstones; unlink bumps the counter and only afterwards marks handles.
//! So either unlink's scan finds the handle, or the registration finds a
//! tombstone newer than its snapshot.
//!
//! Tombstones are keyed by inode, not path: after `rm g; mv f g` a pending
//! open of `f` lives at `g` and must not be taken for the removed file. A
//! re-create keeps the path's inode (`allocate_inode`), which is fine: a new
//! open of the re-created file takes its snapshot after the tombstone.
//!
//! A tombstone is kept only while some open whose snapshot predates it is
//! still in flight; later opens never look at it.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use crate::MutexExt;

#[derive(Default)]
pub(crate) struct Tombstones {
    generation: u64,
    /// Inode → generation of its latest local unlink.
    by_ino: HashMap<u64, u64>,
    /// The same, generation → inode, so pruning pops the oldest from the
    /// front instead of scanning every tombstone on each unlink: `rm -rf`
    /// of N files under an old in-flight open was O(N²) on the dispatch
    /// thread, under the cache lock.
    by_gen: BTreeMap<u64, u64>,
    /// Snapshots of opens not registered yet, counted per generation. Its own
    /// lock, never held while taking another, so a snapshot may be dropped
    /// anywhere, the cache lock included.
    in_flight: Arc<Mutex<BTreeMap<u64, usize>>>,
}

/// An open's view of the counter; dropped once the handle is registered or
/// the open failed.
pub(crate) struct OpenSnapshot {
    generation: u64,
    in_flight: Arc<Mutex<BTreeMap<u64, usize>>>,
}

impl Drop for OpenSnapshot {
    fn drop(&mut self) {
        let mut m = self.in_flight.safe_lock();
        if let Some(n) = m.get_mut(&self.generation) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.generation);
            }
        }
    }
}

impl Tombstones {
    /// Called with the cache locked, where open() resolves the inode.
    pub(crate) fn snapshot(&self) -> OpenSnapshot {
        *self.in_flight.safe_lock().entry(self.generation).or_insert(0) += 1;
        OpenSnapshot { generation: self.generation, in_flight: Arc::clone(&self.in_flight) }
    }

    /// Records that this mount removed inode `ino` (unlink, or a rename over
    /// it). Called with the cache locked, where the listing is edited.
    pub(crate) fn record(&mut self, ino: u64) {
        self.prune();
        self.generation += 1;
        if let Some(older) = self.by_ino.insert(ino, self.generation) {
            self.by_gen.remove(&older);
        }
        self.by_gen.insert(self.generation, ino);
    }

    /// Whether this mount removed `ino` after `snap` was taken.
    pub(crate) fn removed_since(&mut self, ino: u64, snap: &OpenSnapshot) -> bool {
        let hit = self.by_ino.get(&ino).is_some_and(|&g| g > snap.generation);
        self.prune();
        hit
    }

    /// Drops the tombstones no open in flight can still need: an open cares
    /// only about those newer than its snapshot.
    fn prune(&mut self) {
        if self.by_gen.is_empty() {
            return;
        }
        let oldest = self.in_flight.safe_lock().keys().next().copied();
        match oldest {
            None => {
                self.by_ino.clear();
                self.by_gen.clear();
            }
            Some(oldest) => {
                while let Some((&g, &ino)) = self.by_gen.first_key_value() {
                    if g > oldest {
                        break;
                    }
                    self.by_gen.pop_first();
                    self.by_ino.remove(&ino);
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.by_ino.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_unlink_after_the_snapshot_counts() {
        let mut t = Tombstones::default();
        t.record(7);
        let snap = t.snapshot();
        assert!(!t.removed_since(7, &snap), "unlinked before the open resolved it");
        t.record(8);
        assert!(t.removed_since(8, &snap));
        assert!(!t.removed_since(9, &snap), "another inode");
        drop(snap);
    }

    #[test]
    fn many_unlinks_under_an_old_open_stay_cheap() {
        let mut t = Tombstones::default();
        let old = t.snapshot();
        let start = std::time::Instant::now();
        for ino in 0..50_000 {
            t.record(ino);
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(1), "{:?}", start.elapsed());
        assert_eq!(t.len(), 50_000);
        assert!(t.removed_since(49_999, &old));
        // Re-unlinking an inode keeps one tombstone for it.
        t.record(7);
        assert_eq!(t.len(), 50_000);
        drop(old);
        t.record(1);
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn tombstones_go_once_no_older_open_is_in_flight() {
        let mut t = Tombstones::default();
        let old = t.snapshot();
        t.record(1);
        let newer = t.snapshot();
        t.record(2);
        assert_eq!(t.len(), 2);
        drop(old);
        t.record(3);
        // `newer` still needs 2 and 3, not 1.
        assert_eq!(t.len(), 2);
        assert!(t.removed_since(2, &newer) && t.removed_since(3, &newer));
        drop(newer);
        t.record(4);
        assert_eq!(t.len(), 1, "only the newest, recorded after the prune");
        let s = t.snapshot();
        assert!(!t.removed_since(4, &s));
        assert_eq!(t.len(), 0);
    }
}
