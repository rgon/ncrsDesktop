//! Broadcast change log for IPC clients.
//!
//! The FUSE layer and the notify-push watcher record changes into two producer
//! buffers: the `DirtySet` (paths whose status/metadata changed in place) and
//! the `FileChangeQueue` (structural add/remove/rename events). Those used to be
//! drained directly by the `CHANGES`/`FILE_CHANGES` IPC verbs, which made them
//! single-consumer: with two file-manager processes connected (two Nautilus
//! windows are one process, but Nautilus + Dolphin, or every KDE app that opens
//! a file dialog, are not) each change reached only whichever process polled
//! first, and the others kept stale emblems.
//!
//! This module turns them into a broadcast: a single pump drains both producer
//! buffers into a bounded, sequence-numbered ring, and every consumer reads it
//! through its own cursor. The producers are untouched.
//!
//! - Legacy `CHANGES`/`FILE_CHANGES` keep a cursor per client *process* (keyed
//!   by the peer pid), so the Nautilus extension — which polls from whichever
//!   pool thread is free, each with its own connection — still sees every
//!   change exactly once.
//! - `EVENTS <since>` / `WATCH` let the client hold the cursor itself; falling
//!   off the ring is reported as `RESYNC` instead of silently losing events.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use crate::ipc::{DirtySet, FileChange, FileChangeQueue};
use crate::MutexExt;

/// Records retained. A full-tree invalidation can briefly exceed this; legacy
/// cursors then skip ahead (logged) and `EVENTS`/`WATCH` clients get `RESYNC`.
pub const DEFAULT_CAPACITY: usize = 32_768;

#[derive(Clone)]
pub enum ChangeRecord {
    /// Status/metadata of this remote path changed in place (a `DirtySet` entry).
    Status(PathBuf),
    /// A structural change (a `FileChangeQueue` entry).
    File(FileChange),
}

/// Result of reading the ring from a cursor.
pub struct ReadResult {
    /// Sequence number to pass as `since` next time.
    pub next: u64,
    pub records: Vec<ChangeRecord>,
    /// The cursor pointed before the oldest retained record: some records were
    /// dropped and the client must re-fetch whatever state it caches.
    pub resync: bool,
}

/// Who a legacy (`CHANGES`/`FILE_CHANGES`) cursor belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CursorKey {
    pub pid: u32,
    /// `HELLO` client-id, or empty for a client that never said hello.
    pub client: String,
}

struct LegacyCursor {
    status: u64,
    file: u64,
    /// Open connections sharing this cursor; dropped when it reaches zero.
    refs: usize,
}

struct Ring {
    /// Sequence number of `entries[0]`.
    base: u64,
    entries: VecDeque<ChangeRecord>,
    cap: usize,
    cursors: HashMap<CursorKey, LegacyCursor>,
    /// Connections streaming via `WATCH`/polling via `EVENTS` recently enough
    /// that the ring must keep history for them.
    live_readers: usize,
}

impl Ring {
    fn head(&self) -> u64 {
        self.base + self.entries.len() as u64
    }
}

pub struct ChangeLog {
    inner: Mutex<Ring>,
    cv: Condvar,
}

impl ChangeLog {
    pub fn new(cap: usize) -> Self {
        ChangeLog {
            inner: Mutex::new(Ring {
                base: 0,
                entries: VecDeque::new(),
                cap: cap.max(1),
                cursors: HashMap::new(),
                live_readers: 0,
            }),
            cv: Condvar::new(),
        }
    }

    /// Sequence number the next appended record will get.
    pub fn head(&self) -> u64 {
        self.inner.safe_lock().head()
    }

    pub fn append<I: IntoIterator<Item = ChangeRecord>>(&self, records: I) {
        let mut r = self.inner.safe_lock();
        let before = r.head();
        for rec in records {
            r.entries.push_back(rec);
        }
        if r.head() == before {
            return;
        }
        let overflow = r.entries.len().saturating_sub(r.cap);
        if overflow > 0 {
            r.entries.drain(..overflow);
            r.base += overflow as u64;
        }
        // Nobody could ever read these: keep the sequence moving but drop the
        // payload, so an idle daemon does not pin up to `cap` paths in memory.
        if r.cursors.is_empty() && r.live_readers == 0 {
            let n = r.entries.len() as u64;
            r.entries.clear();
            r.base += n;
        }
        drop(r);
        self.cv.notify_all();
    }

    /// Move everything the producers recorded since the last pump into the ring.
    /// The only place the producer buffers are drained.
    pub fn pump(&self, dirty: &DirtySet, file_changes: &FileChangeQueue) {
        let files: Vec<FileChange> = file_changes.safe_lock().drain(..).collect();
        let paths: Vec<PathBuf> = dirty.safe_lock().drain().collect();
        if files.is_empty() && paths.is_empty() {
            return;
        }
        self.append(
            files
                .into_iter()
                .map(ChangeRecord::File)
                .chain(paths.into_iter().map(ChangeRecord::Status)),
        );
    }

    /// Read up to `max` records starting at `since`.
    pub fn read_since(&self, since: u64, max: usize) -> ReadResult {
        let r = self.inner.safe_lock();
        Self::read_locked(&r, since, max, |_| true)
    }

    fn read_locked<F: Fn(&ChangeRecord) -> bool>(r: &Ring, since: u64, max: usize, keep: F) -> ReadResult {
        let head = r.head();
        let resync = since < r.base;
        let start = since.clamp(r.base, head);
        let mut records = Vec::new();
        let mut next = start;
        for rec in r.entries.iter().skip((start - r.base) as usize) {
            if records.len() >= max {
                break;
            }
            next += 1;
            if keep(rec) {
                records.push(rec.clone());
            }
        }
        ReadResult { next, records, resync }
    }

    /// Block until the head moves past `seq` or `timeout` elapses; returns the head.
    pub fn wait_past(&self, seq: u64, timeout: Duration) -> u64 {
        let r = self.inner.safe_lock();
        let (r, _) = self
            .cv
            .wait_timeout_while(r, timeout, |r| r.head() <= seq)
            .unwrap_or_else(|e| e.into_inner());
        r.head()
    }

    /// A `WATCH`/`EVENTS` reader appeared: retain history from now on.
    pub fn add_live_reader(&self) {
        self.inner.safe_lock().live_readers += 1;
    }

    pub fn remove_live_reader(&self) {
        let mut r = self.inner.safe_lock();
        r.live_readers = r.live_readers.saturating_sub(1);
    }

    /// Register a connection for `key`, creating its legacy cursor at the
    /// current head if this is the first connection from that client.
    pub fn attach(&self, key: &CursorKey) {
        let mut r = self.inner.safe_lock();
        let head = r.head();
        r.cursors
            .entry(key.clone())
            .or_insert(LegacyCursor { status: head, file: head, refs: 0 })
            .refs += 1;
    }

    /// Drop a connection for `key`; the cursor goes with its last connection.
    pub fn detach(&self, key: &CursorKey) {
        let mut r = self.inner.safe_lock();
        if let Some(c) = r.cursors.get_mut(key) {
            c.refs = c.refs.saturating_sub(1);
            if c.refs == 0 {
                r.cursors.remove(key);
            }
        }
    }

    /// Move a connection from one cursor key to another (after `HELLO`),
    /// carrying the position over so nothing is replayed or lost.
    pub fn rekey(&self, from: &CursorKey, to: &CursorKey) {
        if from == to {
            return;
        }
        let mut r = self.inner.safe_lock();
        let head = r.head();
        let (status, file) = match r.cursors.get_mut(from) {
            Some(c) => {
                c.refs = c.refs.saturating_sub(1);
                let pos = (c.status, c.file);
                if c.refs == 0 {
                    r.cursors.remove(from);
                }
                pos
            }
            None => (head, head),
        };
        r.cursors
            .entry(to.clone())
            .or_insert(LegacyCursor { status, file, refs: 0 })
            .refs += 1;
    }

    /// Legacy `CHANGES`: up to `max` distinct status paths past this client's cursor.
    pub fn take_status(&self, key: &CursorKey, max: usize) -> Vec<PathBuf> {
        let mut r = self.inner.safe_lock();
        let Some(since) = r.cursors.get(key).map(|c| c.status) else {
            return Vec::new();
        };
        if since < r.base {
            log::warn!("IPC CHANGES: client {:?} fell {} record(s) behind the change log", key, r.base - since);
        }
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        let head = r.head();
        let mut next = since.clamp(r.base, head);
        for rec in r.entries.iter().skip((next - r.base) as usize) {
            if let ChangeRecord::Status(p) = rec {
                if !seen.contains(p) {
                    if out.len() >= max {
                        break;
                    }
                    seen.insert(p.clone());
                    out.push(p.clone());
                }
            }
            next += 1;
        }
        if let Some(c) = r.cursors.get_mut(key) {
            c.status = next;
        }
        out
    }

    /// Legacy `FILE_CHANGES`: every structural change past this client's cursor.
    pub fn take_file_changes(&self, key: &CursorKey) -> Vec<FileChange> {
        let mut r = self.inner.safe_lock();
        let Some(since) = r.cursors.get(key).map(|c| c.file) else {
            return Vec::new();
        };
        if since < r.base {
            log::warn!("IPC FILE_CHANGES: client {:?} fell {} record(s) behind the change log", key, r.base - since);
        }
        let res = Self::read_locked(&r, since, usize::MAX, |rec| matches!(rec, ChangeRecord::File(_)));
        if let Some(c) = r.cursors.get_mut(key) {
            c.file = res.next;
        }
        res.records
            .into_iter()
            .filter_map(|rec| match rec {
                ChangeRecord::File(f) => Some(f),
                ChangeRecord::Status(_) => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::FileChangeKind;
    use std::sync::Arc;

    fn key(pid: u32) -> CursorKey {
        CursorKey { pid, client: String::new() }
    }

    fn status(p: &str) -> ChangeRecord {
        ChangeRecord::Status(PathBuf::from(p))
    }

    fn added(p: &str) -> ChangeRecord {
        ChangeRecord::File(FileChange { kind: FileChangeKind::Added, path: PathBuf::from(p) })
    }

    #[test]
    fn two_clients_both_see_the_same_change() {
        // The bug this module exists for: CHANGES used to drain a global set,
        // so the second file manager never saw what the first one polled.
        let log = ChangeLog::new(16);
        log.attach(&key(1));
        log.attach(&key(2));
        log.append([status("/a"), added("/b")]);
        assert_eq!(log.take_status(&key(1), 500), vec![PathBuf::from("/a")]);
        assert_eq!(log.take_status(&key(2), 500), vec![PathBuf::from("/a")]);
        assert_eq!(log.take_file_changes(&key(1)).len(), 1);
        assert_eq!(log.take_file_changes(&key(2)).len(), 1);
        // Consumed: nothing is replayed.
        assert!(log.take_status(&key(1), 500).is_empty());
        assert!(log.take_file_changes(&key(2)).is_empty());
    }

    #[test]
    fn connections_of_one_process_share_a_cursor() {
        // Nautilus polls from whichever pool thread is free, each with its own
        // connection. They must not each replay the same FILE_CHANGES: an
        // "A:" replayed after the file was deleted would re-create it.
        let log = ChangeLog::new(16);
        log.attach(&key(7));
        log.attach(&key(7));
        log.append([added("/x")]);
        assert_eq!(log.take_file_changes(&key(7)).len(), 1);
        assert!(log.take_file_changes(&key(7)).is_empty());
        log.detach(&key(7));
        log.append([added("/y")]);
        assert_eq!(log.take_file_changes(&key(7)).len(), 1, "cursor survives while one connection remains");
        log.detach(&key(7));
        log.append([added("/z")]);
        assert!(log.take_file_changes(&key(7)).is_empty(), "cursor dropped with its last connection");
    }

    #[test]
    fn new_client_starts_at_head() {
        let log = ChangeLog::new(16);
        log.attach(&key(1));
        log.append([status("/old")]);
        log.attach(&key(2));
        assert!(log.take_status(&key(2), 500).is_empty());
    }

    #[test]
    fn status_paths_are_deduplicated_and_capped() {
        let log = ChangeLog::new(64);
        log.attach(&key(1));
        log.append([status("/a"), status("/a"), status("/b"), status("/c")]);
        assert_eq!(log.take_status(&key(1), 2), vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(log.take_status(&key(1), 2), vec![PathBuf::from("/c")]);
    }

    #[test]
    fn events_reader_overflow_reports_resync() {
        let log = ChangeLog::new(4);
        log.add_live_reader();
        let since = log.head();
        log.append((0..10).map(|i| status(&format!("/{i}"))));
        let res = log.read_since(since, 100);
        assert!(res.resync);
        assert_eq!(res.records.len(), 4);
        assert_eq!(res.next, log.head());
        let again = log.read_since(res.next, 100);
        assert!(!again.resync && again.records.is_empty());
    }

    #[test]
    fn read_since_pages() {
        let log = ChangeLog::new(64);
        log.add_live_reader();
        log.append((0..5).map(|i| status(&format!("/{i}"))));
        let first = log.read_since(0, 3);
        assert_eq!((first.records.len(), first.next), (3, 3));
        let second = log.read_since(first.next, 3);
        assert_eq!((second.records.len(), second.next), (2, 5));
    }

    #[test]
    fn idle_log_retains_nothing() {
        let log = ChangeLog::new(64);
        log.append([status("/a"), status("/b")]);
        assert_eq!(log.head(), 2);
        assert_eq!(log.inner.safe_lock().entries.len(), 0);
    }

    #[test]
    fn rekey_carries_position() {
        let log = ChangeLog::new(16);
        let anon = key(3);
        let named = CursorKey { pid: 3, client: "nautilus".into() };
        log.attach(&anon);
        log.append([status("/a")]);
        log.rekey(&anon, &named);
        assert_eq!(log.take_status(&named, 10), vec![PathBuf::from("/a")]);
        assert!(log.take_status(&anon, 10).is_empty());
    }

    #[test]
    fn pump_moves_both_producers() {
        let log = ChangeLog::new(16);
        log.attach(&key(1));
        let dirty: DirtySet = Arc::new(Mutex::new(Default::default()));
        let fcq: FileChangeQueue = Arc::new(Mutex::new(Vec::new()));
        dirty.safe_lock().insert(PathBuf::from("/d"));
        fcq.safe_lock().push(FileChange { kind: FileChangeKind::Removed, path: PathBuf::from("/r") });
        log.pump(&dirty, &fcq);
        assert!(dirty.safe_lock().is_empty() && fcq.safe_lock().is_empty());
        assert_eq!(log.take_status(&key(1), 10).len(), 1);
        assert_eq!(log.take_file_changes(&key(1)).len(), 1);
    }

    #[test]
    fn wait_past_wakes_on_append() {
        let log = Arc::new(ChangeLog::new(16));
        let seq = log.head();
        let l2 = log.clone();
        let t = std::thread::spawn(move || l2.wait_past(seq, Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(50));
        log.add_live_reader();
        log.append([status("/w")]);
        assert_eq!(t.join().unwrap(), seq + 1);
    }
}
