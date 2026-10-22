//! Daemon-restart survival, part 2: the state the kernel still references
//! after a reattach (part 1, keeping the connection, is `handoff`).
//!
//! A connection taken over from a previous daemon comes with that daemon's
//! node ids and file handles: every dentry the kernel caches names a node id,
//! every file a program holds open names a file handle. Both come from plain
//! counters (`FsCache::allocate_inode`, `next_fh`), so a fresh process would
//! hand the same numbers to *different* files — the one outcome worse than
//! ENOTCONN. This module keeps them in an append-only log in the cache dir
//! (`handoff.log`, one JSON record per line):
//!
//! - every node id, when it is allocated, and every rename that moves or
//!   drops one;
//! - every open file handle with what serving it again needs (path, write
//!   staging file and its identity, original etag, passthrough backing id),
//!   and its release, unlink and rename retargeting;
//! - the start of every streamed (chunked) upload;
//! - a watermark of the handle counter, which directory handles share;
//! - a reservation of node ids, in blocks of [`RESERVE_BLOCK`].
//!
//! Records are plain write(2)s, no fsync: the log only has to outlive this
//! *process* — power loss takes the kernel's connection with it, so there is
//! nothing to reattach to after a reboot. Handle and rename records are written
//! before the reply that depends on them. Node-id records, which come under the
//! cache lock on every lookup of a new name, are queued and written once a
//! second (and before a hand-over); what keeps a crash from handing one of
//! them out again is the reservation, written through before the first id of
//! each block goes out: a restore starts numbering past it, so an id whose
//! record was lost is unknown to the successor (ENOENT), never another file.
//! A hand-over that drained every running request also writes
//! `handoff-handles.json`, the exact volatile per-handle state (dirty flag,
//! streamed-upload progress) the log does not follow; the successor reads it
//! once and deletes it.
//!
//! On reattach the log is replayed into the inode table and `open_files`. A
//! handle whose state cannot be trusted is never guessed at: it comes back
//! failed (EIO on write and close, its upload abandoned) or not at all
//! (`is_lost_fh`: EIO on flush/fsync, and write already EIOs an unknown fh).
//!
//! Lock order: `cache` → LOG and `open_files` → LOG; LOG is a leaf.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::handoff::ConnDev;
use crate::iomode::IoKind;
use crate::{ChunkUploadState, FsCache, MutexExt, NextCloudFs, OpenFile, READ_AHEAD_INITIAL};

const LOG_FILE: &str = "handoff.log";
const HANDLES_FILE: &str = "handoff-handles.json";
/// Rewrite the log down to the live state once it grows past this (and past
/// twice its size after the last rewrite).
const COMPACT_BYTES: u64 = 64 * 1024 * 1024;
/// Queued node-id records are written once they reach this, or on the tick.
const BUF_MAX: usize = 64 * 1024;
/// Node ids are reserved in blocks of this many, so a reservation is written
/// through once per block rather than once per id.
const RESERVE_BLOCK: u64 = 4096;
/// Handle numbers skipped after a restore: directory handles are not logged,
/// only the counter once a second, so the kernel may hold a few above the
/// last watermark.
const FH_GAP: u64 = 1 << 20;
const TICK: Duration = Duration::from_secs(1);

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum IoKindRec {
    Passthrough,
    Cached,
    DirectIo,
}

impl From<IoKind> for IoKindRec {
    fn from(k: IoKind) -> Self {
        match k {
            IoKind::Passthrough => IoKindRec::Passthrough,
            IoKind::Cached => IoKindRec::Cached,
            IoKind::DirectIo => IoKindRec::DirectIo,
        }
    }
}

impl From<IoKindRec> for IoKind {
    fn from(k: IoKindRec) -> Self {
        match k {
            IoKindRec::Passthrough => IoKind::Passthrough,
            IoKindRec::Cached => IoKind::Cached,
            IoKindRec::DirectIo => IoKind::DirectIo,
        }
    }
}

/// Identity of a write staging file: any write, truncate, utimens or the tail
/// rewrite of a streamed upload changes it. Lets a restore tell whether a
/// handle wrote anything after it was logged, which the log does not follow.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
struct StagingId {
    ino: u64,
    size: u64,
    ctime: i64,
    ctime_nsec: i64,
}

impl StagingId {
    fn of(path: &Path) -> Option<Self> {
        let m = std::fs::metadata(path).ok()?;
        Some(StagingId { ino: m.ino(), size: m.len(), ctime: m.ctime(), ctime_nsec: m.ctime_nsec() })
    }
}

/// Everything about an open file handle that does not change while it is open
/// (or changes through a logged event: unlink, rename).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct OpenRecord {
    fh: u64,
    ino: u64,
    path: PathBuf,
    #[serde(default)]
    write_path: Option<PathBuf>,
    #[serde(default)]
    staging: Option<StagingId>,
    /// Dirty when opened (a truncating open).
    #[serde(default)]
    dirty: bool,
    #[serde(default)]
    created: bool,
    #[serde(default)]
    unlinked: bool,
    #[serde(default)]
    original_etag: Option<String>,
    #[serde(default)]
    stream_eligible: bool,
    io_kind: IoKindRec,
    #[serde(default)]
    backing_id: Option<u32>,
    #[serde(default)]
    mime: Option<(String, usize)>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
enum Record {
    Base { dev: ConnDev, mount_point: PathBuf },
    Ino { s: u64, ino: u64, path: PathBuf },
    /// Node ids below `upto` may have been handed out, logged or not.
    Reserve { s: u64, upto: u64 },
    Mv { s: u64, ino: u64, path: PathBuf, #[serde(default)] dropped: Option<u64> },
    Fh { s: u64, next: u64 },
    Open { s: u64, rec: OpenRecord },
    Close { s: u64, fh: u64 },
    Unlinked { s: u64, fh: u64 },
    Retarget { s: u64, fh: u64, path: PathBuf },
    Stream { s: u64, write_path: PathBuf, uploads_base: String },
}

/// Exact volatile state of one handle, written by a planned restart.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct HandleState {
    fh: u64,
    path: PathBuf,
    dirty: bool,
    total_written: u64,
    stream_eligible: bool,
    #[serde(default)]
    chunk: Option<ChunkRec>,
    upload_failed: bool,
    unlinked: bool,
    created: bool,
    #[serde(default)]
    original_etag: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct ChunkRec {
    uploads_base: String,
    next_index: u64,
    bytes_confirmed: u64,
}

#[derive(Serialize, Deserialize, Debug)]
struct HandlesSnapshot {
    dev: ConnDev,
    mount_point: PathBuf,
    /// Log sequence number when captured: a handle opened later is not in it.
    s: u64,
    handles: Vec<HandleState>,
}

// ── the log writer ───────────────────────────────────────────────────────────

struct Writer {
    file: File,
    path: PathBuf,
    /// Records not written yet (node ids only; see the module doc).
    buf: Vec<u8>,
    seq: u64,
    bytes: u64,
    /// Size right after the last compaction: the next one waits for twice that.
    compacted_bytes: u64,
    dev: ConnDev,
    mount_point: PathBuf,
    /// Node ids below this may have been handed out (a durable `Reserve`).
    reserved_upto: u64,
    /// Live handles' records with their sequence number, for compaction.
    opens: HashMap<u64, (u64, OpenRecord)>,
    /// write_path → (seq, uploads_base) of handles that started a streamed upload.
    streams: HashMap<PathBuf, (u64, String)>,
    last_fh_mark: u64,
    /// While a compaction writes the new file off-lock: every record written to
    /// the old file meanwhile, to append to the new one before it replaces it.
    tail: Option<Vec<u8>>,
    failed: bool,
}

static LOG: Mutex<Option<Writer>> = Mutex::new(None);

impl Writer {
    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Queues one record; `sync` writes it (and everything queued before it)
    /// now. Only node-id records are left queued: the ticker writes them.
    fn append(&mut self, rec: &Record, sync: bool) {
        if self.failed {
            return;
        }
        if let Err(e) = serde_json::to_writer(&mut self.buf, rec) {
            log::warn!("handoff log: cannot encode a record: {}", e);
            return;
        }
        self.buf.push(b'\n');
        if sync || self.buf.len() >= BUF_MAX {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.failed || self.buf.is_empty() {
            return;
        }
        if let Err(e) = self.file.write_all(&self.buf) {
            // A log with a gap could map an id to the wrong file after a
            // restart; one that is missing just means "mount afresh".
            log::error!("handoff log: write failed ({}) — restart survival disabled for this mount", e);
            self.failed = true;
            let _ = std::fs::remove_file(&self.path);
            return;
        }
        self.bytes += self.buf.len() as u64;
        if let Some(t) = self.tail.as_mut() {
            t.extend_from_slice(&self.buf);
        }
        self.buf.clear();
    }

    /// The current state as a compacted log (cache lock held by the caller):
    /// cheap copies, written out later by [`LogImage::write`] without any lock.
    /// Sequence numbers of handle records are kept, so a handles snapshot
    /// taken before still lines up.
    fn image(&mut self, c: &FsCache) -> LogImage {
        self.reserved_upto = self.reserved_upto.max(c.next_inode);
        let mut inodes: Vec<(u64, PathBuf)> = c.inodes.iter().filter(|(i, _)| **i != 1).map(|(i, p)| (*i, p.clone())).collect();
        inodes.sort_by_key(|(i, _)| *i);
        LogImage {
            dev: self.dev,
            mount_point: self.mount_point.clone(),
            s: self.next_seq(),
            fh_mark: self.last_fh_mark,
            reserved_upto: self.reserved_upto,
            inodes,
            opens: self.opens.values().cloned().collect(),
            streams: self.streams.iter().map(|(wp, (s, b))| (wp.clone(), *s, b.clone())).collect(),
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Grow-only threshold: a log whose live state alone is past COMPACT_BYTES
/// would otherwise be rewritten on every tick.
fn needs_compaction(bytes: u64, compacted_bytes: u64) -> bool {
    bytes > COMPACT_BYTES.max(compacted_bytes.saturating_mul(2))
}

/// A compacted log: Base, the handle-counter watermark, the node-id
/// reservation, every node id, the live handles and their streams.
struct LogImage {
    dev: ConnDev,
    mount_point: PathBuf,
    s: u64,
    fh_mark: u64,
    reserved_upto: u64,
    inodes: Vec<(u64, PathBuf)>,
    opens: Vec<(u64, OpenRecord)>,
    streams: Vec<(PathBuf, u64, String)>,
}

impl LogImage {
    /// Writes the image to `path` (truncating it) and returns the file, still
    /// buffered, for the caller to append to and flush.
    fn write(&self, path: &Path) -> io::Result<io::BufWriter<File>> {
        let mut out = io::BufWriter::new(File::create(path)?);
        let put = |out: &mut io::BufWriter<File>, r: &Record| -> io::Result<()> {
            serde_json::to_writer(&mut *out, r).map_err(io::Error::other)?;
            out.write_all(b"\n")
        };
        let s = self.s;
        put(&mut out, &Record::Base { dev: self.dev, mount_point: self.mount_point.clone() })?;
        put(&mut out, &Record::Fh { s, next: self.fh_mark })?;
        put(&mut out, &Record::Reserve { s, upto: self.reserved_upto })?;
        for (ino, path) in &self.inodes {
            put(&mut out, &Record::Ino { s, ino: *ino, path: path.clone() })?;
        }
        for (s, rec) in &self.opens {
            put(&mut out, &Record::Open { s: *s, rec: rec.clone() })?;
        }
        for (wp, s, base) in &self.streams {
            put(&mut out, &Record::Stream { s: *s, write_path: wp.clone(), uploads_base: base.clone() })?;
        }
        Ok(out)
    }
}

fn finish_file(out: io::BufWriter<File>) -> io::Result<File> {
    out.into_inner().map_err(|e| e.into_error())
}

fn with_log(f: impl FnOnce(&mut Writer)) {
    if let Some(w) = LOG.safe_lock().as_mut() {
        f(w)
    }
}

// ── hooks called from the filesystem ─────────────────────────────────────────

/// `FsCache::allocate_inode` handed out `ino` (cache lock held). Its record is
/// only queued; what keeps a crash from reusing the number is the reservation,
/// written through before the first id of each block goes out.
pub(crate) fn inode_allocated(ino: u64, path: &Path) {
    with_log(|w| log_inode(w, ino, path));
}

fn log_inode(w: &mut Writer, ino: u64, path: &Path) {
    if ino >= w.reserved_upto {
        w.reserved_upto = reservation_after(ino);
        let s = w.next_seq();
        let upto = w.reserved_upto;
        w.append(&Record::Reserve { s, upto }, true);
    }
    let s = w.next_seq();
    w.append(&Record::Ino { s, ino, path: path.to_path_buf() }, false);
}

/// The reservation that covers handing out `ino`: the end of its block.
fn reservation_after(ino: u64) -> u64 {
    (ino / RESERVE_BLOCK + 1) * RESERVE_BLOCK
}

/// rename() moved `ino` to `to`, dropping the node id that was there (cache lock held).
pub(crate) fn inode_moved(ino: u64, to: &Path, dropped: Option<u64>) {
    with_log(|w| {
        let s = w.next_seq();
        w.append(&Record::Mv { s, ino, path: to.to_path_buf(), dropped }, true);
    });
}

/// open()/create() inserted `of` under `fh` (open_files lock held), before
/// replying. `backing_id` is the passthrough backing the reply carries.
pub(crate) fn handle_opened(fh: u64, of: &OpenFile, backing_id: Option<u32>) {
    with_log(|w| {
        let rec = OpenRecord {
            fh,
            ino: of.ino,
            path: of.remote_path.clone(),
            staging: of.write_path.as_deref().and_then(StagingId::of),
            write_path: of.write_path.clone(),
            dirty: of.dirty,
            created: of.created,
            unlinked: of.unlinked,
            original_etag: of.original_etag.clone(),
            stream_eligible: of.stream_eligible,
            io_kind: of.io_kind.into(),
            backing_id,
            mime: of.mime_detect_ct.clone().map(|ct| (ct, of.mime_detect_max_read)),
        };
        let s = w.next_seq();
        w.append(&Record::Open { s, rec: rec.clone() }, true);
        w.opens.insert(fh, (s, rec));
    });
}

/// release() removed `fh` from open_files — or found nothing there, in which
/// case this returns what a restore that could not rebuild the handle still
/// holds for it, for release() to undo.
pub(crate) fn handle_released(fh: u64) -> Option<LostHandle> {
    with_log(|w| {
        if let Some((_, rec)) = w.opens.remove(&fh) {
            if let Some(wp) = rec.write_path {
                w.streams.remove(&wp);
            }
            let s = w.next_seq();
            w.append(&Record::Close { s, fh }, true);
        }
    });
    release_restored_backing(fh);
    LOST.safe_lock().as_mut().and_then(|lost| lost.remove(&fh))
}

/// unlink() marked an open handle's file deleted.
pub(crate) fn handle_unlinked(fh: u64) {
    with_log(|w| {
        if let Some((_, rec)) = w.opens.get_mut(&fh) {
            rec.unlinked = true;
            let s = w.next_seq();
            w.append(&Record::Unlinked { s, fh }, true);
        }
    });
}

/// rename() moved an open handle's file: it now commits to `path`.
pub(crate) fn handle_retargeted(fh: u64, path: &Path) {
    with_log(|w| {
        if let Some((_, rec)) = w.opens.get_mut(&fh) {
            rec.path = path.to_path_buf();
            let s = w.next_seq();
            w.append(&Record::Retarget { s, fh, path: path.to_path_buf() }, true);
        }
    });
}

/// A streamed upload opened its server-side session, before any chunk is sent
/// (from then on the staging file no longer holds the whole content).
pub(crate) fn stream_started(write_path: &Path, uploads_base: &str) {
    with_log(|w| {
        let s = w.next_seq();
        w.append(&Record::Stream { s, write_path: write_path.to_path_buf(), uploads_base: uploads_base.to_string() }, true);
        w.streams.insert(write_path.to_path_buf(), (s, uploads_base.to_string()));
    });
}

/// What a handle restored as unusable still holds: the inode's I/O mode (the
/// kernel keeps the inode in it until the handle is released) and, for a
/// created file, its place in `uploading`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LostHandle {
    pub(crate) ino: u64,
    pub(crate) io_kind: IoKind,
    pub(crate) uploading: Option<PathBuf>,
}

/// Handles restored as unusable: flush and fsync on them report EIO, so a
/// program does not see a successful close for data that never reaches the server.
static LOST: Mutex<Option<HashMap<u64, LostHandle>>> = Mutex::new(None);

pub(crate) fn is_lost_fh(fh: u64) -> bool {
    LOST.safe_lock().as_ref().is_some_and(|s| s.contains_key(&fh))
}

/// Staging files restored handles still need; NextCloudFs::new must not
/// delete them as orphans. Filled by [`stage`] before the filesystem is built.
static PROTECTED: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);

pub(crate) fn protects_staging(path: &Path) -> bool {
    PROTECTED.safe_lock().as_ref().is_some_and(|s| s.contains(path))
}

/// Passthrough backing ids a previous daemon registered, by restored handle.
#[derive(Default)]
struct RestoredBacking {
    by_fh: HashMap<u64, u32>,
    users: HashMap<u32, usize>,
}

static RESTORED_BACKING: Mutex<Option<RestoredBacking>> = Mutex::new(None);

fn release_restored_backing(fh: u64) {
    let close = {
        let mut g = RESTORED_BACKING.safe_lock();
        let Some(rb) = g.as_mut() else { return };
        let Some(id) = rb.by_fh.remove(&fh) else { return };
        let n = rb.users.entry(id).or_insert(1);
        *n -= 1;
        (*n == 0).then(|| {
            rb.users.remove(&id);
            id
        })
    };
    if let Some(id) = close {
        crate::handoff::close_backing_id(id);
    }
}

// ── starting and stopping the log ────────────────────────────────────────────

/// A fresh mount on connection `dev`: start a new log holding every node id
/// already allocated. The cache lock is held across the dump and the switch-on,
/// so no allocation slips between them.
pub(crate) fn start_fresh(cache_dir: &Path, dev: ConnDev, mount_point: &Path, cache: &Mutex<FsCache>) {
    let _ = std::fs::remove_file(cache_dir.join(HANDLES_FILE));
    let path = cache_dir.join(LOG_FILE);
    let c = cache.safe_lock();
    let mut g = LOG.safe_lock();
    *g = None;
    let mut image = LogImage {
        dev,
        mount_point: mount_point.to_path_buf(),
        s: 1,
        fh_mark: 0,
        reserved_upto: 0,
        inodes: Vec::new(),
        opens: Vec::new(),
        streams: Vec::new(),
    };
    image.reserved_upto = c.next_inode;
    image.inodes = c.inodes.iter().filter(|(i, _)| **i != 1).map(|(i, p)| (*i, p.clone())).collect();
    image.inodes.sort_by_key(|(i, _)| *i);
    let file = match image.write(&path).and_then(finish_file) {
        Ok(f) => f,
        Err(e) => {
            log::warn!("handoff log: cannot write {} ({}) — restart survival disabled", path.display(), e);
            let _ = std::fs::remove_file(&path);
            return;
        }
    };
    let bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
    *g = Some(Writer {
        file,
        path,
        buf: Vec::new(),
        seq: image.s,
        bytes,
        compacted_bytes: bytes,
        dev,
        mount_point: mount_point.to_path_buf(),
        reserved_upto: image.reserved_upto,
        opens: HashMap::new(),
        streams: HashMap::new(),
        last_fh_mark: 0,
        tail: None,
        failed: false,
    });
}

/// After a restore (cache lock held, see [`apply`]): keep appending to the log
/// the restore came from. `next_inode` is the restored counter: nothing above
/// it is reserved yet, so the first allocation writes a new reservation.
fn start_resumed(cache_dir: &Path, dev: ConnDev, mount_point: &Path, r: &Restored, next_inode: u64) {
    let path = cache_dir.join(LOG_FILE);
    let file = match OpenOptions::new().append(true).open(&path) {
        Ok(f) => f,
        Err(e) => {
            log::warn!("handoff log: cannot reopen {} ({}) — a further restart will mount afresh", path.display(), e);
            return;
        }
    };
    let bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
    let opens = r.handles.iter().map(|(fh, h)| (*fh, (h.seq, h.rec.clone()))).collect();
    let streams = r.streams.clone();
    *LOG.safe_lock() = Some(Writer {
        file,
        path,
        buf: Vec::new(),
        seq: r.max_seq,
        bytes,
        compacted_bytes: 0,
        dev,
        mount_point: mount_point.to_path_buf(),
        reserved_upto: next_inode,
        opens,
        streams,
        last_fh_mark: r.fh_mark,
        tail: None,
        failed: false,
    });
}

/// The mount ended cleanly: nothing will reattach, so drop the log.
pub(crate) fn stop(cache_dir: &Path) {
    if let Some(w) = LOG.safe_lock().as_mut() {
        w.failed = true; // nothing left to flush into a file about to go
    }
    *LOG.safe_lock() = None;
    let _ = std::fs::remove_file(cache_dir.join(LOG_FILE));
    let _ = std::fs::remove_file(cache_dir.join(HANDLES_FILE));
}

/// Writes every queued record: before this process hands the connection over.
pub(crate) fn flush_log() {
    with_log(|w| w.flush());
}

/// Once a second: log the handle counter when it moved (directory handles are
/// not logged one by one), write queued records, and compact a log that grew large.
pub(crate) fn spawn_ticker(cache: Arc<Mutex<FsCache>>, next_fh: Arc<Mutex<u64>>, shutdown: Arc<AtomicBool>) {
    let _ = crate::bg::spawn_service("handoff-log", move || loop {
        std::thread::sleep(TICK);
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        let next = *next_fh.safe_lock();
        let mut big = false;
        with_log(|w| {
            if next != w.last_fh_mark {
                let s = w.next_seq();
                w.append(&Record::Fh { s, next }, false);
                w.last_fh_mark = next;
            }
            w.flush();
            big = !w.failed && w.tail.is_none() && needs_compaction(w.bytes, w.compacted_bytes);
        });
        if big {
            compact(&cache);
        }
    });
}

/// Rewrites the log down to the live state. Only copying the state happens
/// under the cache lock; the old file stays complete (and in use) until the new
/// one, plus whatever was logged while it was written, replaces it.
fn compact(cache: &Mutex<FsCache>) {
    let (image, path) = {
        let c = cache.safe_lock();
        let mut g = LOG.safe_lock();
        let Some(w) = g.as_mut() else { return };
        w.flush();
        if w.failed {
            return;
        }
        w.tail = Some(Vec::new());
        (w.image(&c), w.path.clone())
    };
    let tmp = path.with_extension("log.tmp");
    let written = image.write(&tmp);
    drop(image);
    let mut g = LOG.safe_lock();
    let Some(w) = g.as_mut() else {
        let _ = std::fs::remove_file(&tmp);
        return;
    };
    w.flush();
    let tail = w.tail.take().unwrap_or_default();
    let swapped = written.and_then(|mut out| {
        out.write_all(&tail)?;
        let f = finish_file(out)?;
        if w.failed {
            return Err(io::Error::other("log failed meanwhile"));
        }
        std::fs::rename(&tmp, &path)?;
        drop(f);
        OpenOptions::new().append(true).open(&path)
    });
    match swapped {
        Ok(file) => {
            w.file = file;
            w.bytes = w.file.metadata().map(|m| m.len()).unwrap_or(0);
            w.compacted_bytes = w.bytes;
            log::info!("handoff log: compacted to {} bytes", w.bytes);
        }
        Err(e) => {
            // The old file is still whole: keep using it, and wait for it to
            // double before trying again.
            let _ = std::fs::remove_file(&tmp);
            w.compacted_bytes = w.bytes;
            log::warn!("handoff log: compaction failed ({}) — still logging to the uncompacted file", e);
        }
    }
}

/// Before a hand-over: with `drained`, the exact volatile state of every open
/// handle, for the successor to restore instead of the log's conservative
/// rules. Without it (requests were still running, so a snapshot could miss
/// one's write), any earlier snapshot is removed and the successor infers.
pub(crate) fn write_handles_snapshot(cache_dir: &Path, open_files: &Mutex<HashMap<u64, OpenFile>>, drained: bool) -> io::Result<()> {
    let path = cache_dir.join(HANDLES_FILE);
    if !drained {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let files = open_files.safe_lock();
    let (s, dev, mount_point) = {
        let mut g = LOG.safe_lock();
        match g.as_mut() {
            Some(w) if !w.failed => {
                w.flush();
                (w.seq, w.dev, w.mount_point.clone())
            }
            _ => return Err(io::Error::other("handoff log unavailable")),
        }
    };
    let handles = files
        .iter()
        .map(|(fh, of)| HandleState {
            fh: *fh,
            path: of.remote_path.clone(),
            dirty: of.dirty,
            total_written: of.total_written,
            stream_eligible: of.stream_eligible,
            chunk: of.chunk_upload.as_ref().map(|c| ChunkRec {
                uploads_base: c.uploads_base.clone(),
                next_index: c.next_index,
                bytes_confirmed: c.bytes_confirmed,
            }),
            upload_failed: of.upload_failed,
            unlinked: of.unlinked,
            created: of.created,
            original_etag: of.original_etag.clone(),
        })
        .collect();
    drop(files);
    let snap = HandlesSnapshot { dev, mount_point, s, handles };
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&snap).map_err(io::Error::other)?)?;
    std::fs::rename(&tmp, &path)
}

// ── restore ──────────────────────────────────────────────────────────────────

struct RestoredHandle {
    seq: u64,
    rec: OpenRecord,
    exact: Option<HandleState>,
}

/// The previous daemon's state, replayed from its log.
pub(crate) struct Restored {
    inodes: BTreeMap<u64, PathBuf>,
    handles: BTreeMap<u64, RestoredHandle>,
    streams: HashMap<PathBuf, (u64, String)>,
    max_seq: u64,
    max_ino: u64,
    reserved: u64,
    max_fh: u64,
    fh_mark: u64,
}

impl Restored {
    /// The first node id a restored filesystem may hand out: past every id the
    /// log names and every reserved one, so an id whose record was still
    /// queued when the previous daemon died is unknown (ENOENT), never reused.
    fn next_inode(&self) -> u64 {
        self.max_ino.saturating_add(1).max(self.reserved).max(2)
    }
}

impl std::fmt::Debug for Restored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Restored({} node ids, {} handles)", self.inodes.len(), self.handles.len())
    }
}

#[derive(Default)]
struct Replayer {
    inodes: BTreeMap<u64, PathBuf>,
    paths: HashMap<PathBuf, u64>,
    handles: BTreeMap<u64, RestoredHandle>,
    streams: HashMap<PathBuf, (u64, String)>,
    max_seq: u64,
    max_ino: u64,
    reserved: u64,
    max_fh: u64,
    fh_mark: u64,
    based: bool,
}

impl Replayer {
    fn set(&mut self, ino: u64, path: PathBuf) {
        if let Some(prev) = self.inodes.insert(ino, path.clone()) {
            if prev != path && self.paths.get(&prev) == Some(&ino) {
                self.paths.remove(&prev);
            }
        }
        if let Some(other) = self.paths.insert(path, ino) {
            // Another node id held this path: like rename()'s displaced inode,
            // it no longer resolves (ENOENT, never this path's file).
            if other != ino {
                self.inodes.remove(&other);
            }
        }
    }

    fn apply(&mut self, rec: Record, dev: ConnDev, mount_point: &Path) -> Result<(), String> {
        if !self.based {
            return match rec {
                Record::Base { dev: d, mount_point: m } if d == dev && m == mount_point => {
                    self.based = true;
                    Ok(())
                }
                Record::Base { dev: d, mount_point: m } => Err(format!(
                    "log is for connection {}:{} on {}, not this one",
                    d.major, d.minor, m.display()
                )),
                _ => Err("log does not start with its connection".into()),
            };
        }
        match rec {
            Record::Base { .. } => return Err("second Base record".into()),
            Record::Ino { s, ino, path } => {
                self.max_seq = self.max_seq.max(s);
                self.max_ino = self.max_ino.max(ino);
                self.set(ino, path);
            }
            Record::Mv { s, ino, path, dropped } => {
                self.max_seq = self.max_seq.max(s);
                self.max_ino = self.max_ino.max(ino);
                self.set(ino, path);
                if let Some(d) = dropped.filter(|d| *d != ino) {
                    if let Some(p) = self.inodes.remove(&d) {
                        if self.paths.get(&p) == Some(&d) {
                            self.paths.remove(&p);
                        }
                    }
                }
            }
            Record::Reserve { s, upto } => {
                self.max_seq = self.max_seq.max(s);
                self.reserved = self.reserved.max(upto);
            }
            Record::Fh { s, next } => {
                self.max_seq = self.max_seq.max(s);
                self.fh_mark = self.fh_mark.max(next);
            }
            Record::Open { s, rec } => {
                self.max_seq = self.max_seq.max(s);
                self.max_fh = self.max_fh.max(rec.fh);
                self.handles.insert(rec.fh, RestoredHandle { seq: s, rec, exact: None });
            }
            Record::Close { s, fh } => {
                self.max_seq = self.max_seq.max(s);
                self.handles.remove(&fh);
            }
            Record::Unlinked { s, fh } => {
                self.max_seq = self.max_seq.max(s);
                if let Some(h) = self.handles.get_mut(&fh) {
                    h.rec.unlinked = true;
                }
            }
            Record::Retarget { s, fh, path } => {
                self.max_seq = self.max_seq.max(s);
                if let Some(h) = self.handles.get_mut(&fh) {
                    h.rec.path = path;
                }
            }
            Record::Stream { s, write_path, uploads_base } => {
                self.max_seq = self.max_seq.max(s);
                self.streams.insert(write_path, (s, uploads_base));
            }
        }
        Ok(())
    }

    /// Overlays a planned restart's exact handle states onto handles the log
    /// says were open when it was taken.
    fn overlay(&mut self, snap: HandlesSnapshot, dev: ConnDev, mount_point: &Path) {
        if snap.dev != dev || snap.mount_point != mount_point {
            return;
        }
        for hs in snap.handles {
            if let Some(h) = self.handles.get_mut(&hs.fh) {
                if h.seq <= snap.s {
                    h.exact = Some(hs);
                }
            }
        }
    }

    fn finish(mut self) -> Restored {
        let open_staging: HashSet<PathBuf> = self.handles.values().filter_map(|h| h.rec.write_path.clone()).collect();
        self.streams.retain(|wp, _| open_staging.contains(wp));
        Restored {
            inodes: self.inodes,
            handles: self.handles,
            streams: self.streams,
            max_seq: self.max_seq,
            max_ino: self.max_ino,
            reserved: self.reserved,
            max_fh: self.max_fh,
            fh_mark: self.fh_mark,
        }
    }
}

/// Replays the log a previous daemon left for connection `dev`. An error means
/// the connection's ids cannot be trusted and it must not be reattached.
pub(crate) fn load(cache_dir: &Path, dev: ConnDev, mount_point: &Path) -> Result<Restored, String> {
    let path = cache_dir.join(LOG_FILE);
    let f = File::open(&path).map_err(|e| format!("{}: {}", path.display(), e))?;
    let mut rp = Replayer::default();
    // A record the previous daemon died while writing can only be the last one.
    let mut torn: Option<String> = None;
    for line in BufReader::new(f).split(b'\n') {
        let line = line.map_err(|e| e.to_string())?;
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<Record>(&line) {
            Ok(rec) => {
                if let Some(e) = torn.take() {
                    return Err(format!("corrupt record in the middle of the log: {}", e));
                }
                rp.apply(rec, dev, mount_point)?;
            }
            Err(e) => torn = Some(e.to_string()),
        }
    }
    if !rp.based {
        return Err("empty log".into());
    }
    let snap_path = cache_dir.join(HANDLES_FILE);
    if let Ok(data) = std::fs::read(&snap_path) {
        // One-shot: it describes the moment of one hand-over. Left in place, a
        // later crash restore would overlay it onto handles that moved on since.
        let _ = std::fs::remove_file(&snap_path);
        match serde_json::from_slice::<HandlesSnapshot>(&data) {
            Ok(snap) => rp.overlay(snap, dev, mount_point),
            Err(e) => log::warn!("handoff: ignoring unreadable {}: {}", HANDLES_FILE, e),
        }
    }
    Ok(rp.finish())
}

/// Before NextCloudFs::new: protect the restored handles' staging files from
/// its orphan sweep.
pub(crate) fn stage(r: &Restored) {
    let set = r.handles.values().filter_map(|h| h.rec.write_path.clone()).collect();
    *PROTECTED.safe_lock() = Some(set);
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct ApplyReport {
    pub(crate) inodes: usize,
    pub(crate) handles: usize,
    pub(crate) exact: usize,
    pub(crate) failed: usize,
    pub(crate) lost: usize,
}

enum Rebuilt {
    /// Usable; `exact` when a planned restart recorded its state.
    Ok { exact: bool },
    /// Restored so writes and close fail and release abandons its upload.
    Failed,
}

/// How a handle's write state comes back. Pure, for testing.
#[derive(Debug, PartialEq)]
struct WriteState {
    dirty: bool,
    total_written: u64,
    stream_eligible: bool,
    chunk: Option<ChunkRec>,
    upload_failed: bool,
    unlinked: bool,
    created: bool,
    path: PathBuf,
    original_etag: Option<String>,
}

fn write_state(h: &RestoredHandle, staging_now: Option<StagingId>, stream: Option<&str>) -> Result<WriteState, String> {
    let rec = &h.rec;
    let now = staging_now.ok_or_else(|| "its staging file is gone".to_string())?;
    if let Some(x) = &h.exact {
        return Ok(WriteState {
            dirty: x.dirty,
            total_written: x.total_written,
            stream_eligible: x.stream_eligible,
            chunk: x.chunk.clone(),
            upload_failed: x.upload_failed,
            unlinked: x.unlinked,
            created: x.created,
            path: x.path.clone(),
            original_etag: x.original_etag.clone(),
        });
    }
    let base = WriteState {
        // Not tracked by the log: written since open iff the staging file changed.
        dirty: rec.dirty || rec.staging != Some(now),
        total_written: now.size,
        stream_eligible: false,
        chunk: None,
        upload_failed: false,
        unlinked: rec.unlinked,
        created: rec.created,
        path: rec.path.clone(),
        original_etag: rec.original_etag.clone(),
    };
    match stream {
        // Chunks may be on the server that are no longer in the staging file, and
        // how far it got is not known: fail the handle loudly (EIO on write and
        // close), abandon the session on release. Never assemble a guess.
        Some(uploads_base) => Ok(WriteState {
            dirty: true,
            total_written: 0,
            chunk: Some(ChunkRec { uploads_base: uploads_base.to_string(), next_index: 0, bytes_confirmed: 0 }),
            upload_failed: true,
            ..base
        }),
        // No session was ever opened: the staging file holds every byte, so it
        // carries on as a plain (whole-file) staged write.
        None => Ok(base),
    }
}

fn rebuild(h: &RestoredHandle, stream: Option<&str>, gen: u64) -> Result<(OpenFile, Rebuilt), String> {
    let rec = &h.rec;
    let mut of = OpenFile {
        remote_path: rec.path.clone(),
        // Read-only handles re-fetch lazily: freshness of a cached copy was
        // pinned per handle at open and is not known any more.
        local: None,
        buf: None,
        write_path: rec.write_path.clone(),
        dirty: rec.dirty,
        original_etag: rec.original_etag.clone(),
        total_written: 0,
        stream_eligible: false,
        chunk_upload: None,
        cache_fresh: false,
        mime_detect_ct: rec.mime.as_ref().map(|(ct, _)| ct.clone()),
        mime_detect_max_read: rec.mime.as_ref().map_or(0, |(_, n)| *n),
        next_expected_off: 0,
        read_ahead_window: READ_AHEAD_INITIAL,
        next_buf: None,
        prev_buf: None,
        last_read: Instant::now(),
        lookahead_inflight: false,
        ino: rec.ino,
        io_kind: rec.io_kind.into(),
        upload_failed: false,
        created: rec.created,
        unlinked: rec.unlinked,
        opened_gen: gen,
    };
    let Some(wp) = rec.write_path.as_deref() else {
        return Ok((of, Rebuilt::Ok { exact: false }));
    };
    let ws = write_state(h, StagingId::of(wp), stream)?;
    of.remote_path = ws.path;
    of.dirty = ws.dirty;
    of.total_written = ws.total_written;
    of.stream_eligible = ws.stream_eligible;
    of.chunk_upload = ws.chunk.map(|c| ChunkUploadState {
        uploads_base: c.uploads_base,
        next_index: c.next_index,
        bytes_confirmed: c.bytes_confirmed,
    });
    of.upload_failed = ws.upload_failed;
    of.unlinked = ws.unlinked;
    of.created = ws.created;
    of.original_etag = ws.original_etag;
    let outcome = if of.upload_failed { Rebuilt::Failed } else { Rebuilt::Ok { exact: h.exact.is_some() } };
    Ok((of, outcome))
}

/// Installs the restored node ids and handles into a freshly built filesystem,
/// before its session serves anything, and resumes the log where the previous
/// daemon left it. The cache lock is held until the log is back on, so no node
/// id is allocated unlogged in between.
pub(crate) fn apply(fs: &NextCloudFs, r: &Restored, cache_dir: &Path, dev: ConnDev, mount_point: &Path) -> ApplyReport {
    let mut report = ApplyReport { inodes: r.inodes.len(), ..Default::default() };
    let gen = fs.uploads.generation();
    let mut lost = HashMap::new();
    let mut uploading = Vec::new();
    {
        let mut files = fs.open_files.safe_lock();
        let mut modes = fs.io_modes.safe_lock();
        let mut backing = RestoredBacking::default();
        for (fh, h) in &r.handles {
            let stream = h.rec.write_path.as_ref().and_then(|wp| r.streams.get(wp)).map(|(_, b)| b.as_str());
            // The kernel keeps the inode in this handle's I/O mode until it is
            // released, whatever happens to the handle here.
            let kind: IoKind = h.rec.io_kind.into();
            modes.adopt(h.rec.ino, kind);
            if kind == IoKind::Passthrough {
                if let Some(id) = h.rec.backing_id {
                    backing.by_fh.insert(*fh, id);
                    *backing.users.entry(id).or_insert(0) += 1;
                }
            }
            // A created file has no server copy until its handle is released: keep
            // refreshes from dropping its listing entry, as create() does.
            let guard = (h.rec.created && !h.rec.unlinked && h.rec.write_path.is_some()).then(|| h.rec.path.clone());
            uploading.extend(guard.clone());
            match rebuild(h, stream, gen) {
                Ok((of, outcome)) => {
                    match outcome {
                        Rebuilt::Ok { exact } => report.exact += exact as usize,
                        Rebuilt::Failed => {
                            report.failed += 1;
                            log::warn!(
                                "handoff: fh {} ({}) was streaming an upload when the previous daemon died — its writes and close now fail; the copy must be retried",
                                fh, h.rec.path.display(),
                            );
                        }
                    }
                    files.insert(*fh, of);
                    report.handles += 1;
                }
                Err(why) => {
                    log::warn!("handoff: cannot restore fh {} ({}): {} — it now fails with EIO", fh, h.rec.path.display(), why);
                    // Not in open_files, so release() learns from this what to undo.
                    lost.insert(*fh, LostHandle { ino: h.rec.ino, io_kind: kind, uploading: guard });
                    report.lost += 1;
                }
            }
        }
        *RESTORED_BACKING.safe_lock() = Some(backing);
    }
    *LOST.safe_lock() = Some(lost);
    *PROTECTED.safe_lock() = None;
    {
        let mut n = fs.next_fh.safe_lock();
        *n = (*n).max(r.max_fh.saturating_add(1)).max(r.fh_mark.saturating_add(FH_GAP));
    }
    let mut c = fs.cache.safe_lock();
    c.inodes.clear();
    c.paths.clear();
    c.inodes.insert(1, PathBuf::from("/"));
    c.paths.insert(PathBuf::from("/"), 1);
    for (ino, path) in &r.inodes {
        if *ino == 1 {
            continue;
        }
        c.inodes.insert(*ino, path.clone());
        c.paths.insert(path.clone(), *ino);
    }
    c.next_inode = c.next_inode.max(r.next_inode());
    c.uploading.extend(uploading);
    start_resumed(cache_dir, dev, mount_point, r, c.next_inode);
    drop(c);
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEV: ConnDev = ConnDev { major: 0, minor: 73 };

    fn mp() -> PathBuf {
        PathBuf::from("/home/u/Nextcloud")
    }

    fn open_rec(fh: u64, ino: u64, path: &str) -> OpenRecord {
        OpenRecord {
            fh,
            ino,
            path: PathBuf::from(path),
            write_path: None,
            staging: None,
            dirty: false,
            created: false,
            unlinked: false,
            original_etag: Some("e1".into()),
            stream_eligible: false,
            io_kind: IoKindRec::Cached,
            backing_id: None,
            mime: None,
        }
    }

    fn replay(records: Vec<Record>) -> Result<Replayer, String> {
        let mut rp = Replayer::default();
        for r in records {
            rp.apply(r, DEV, &mp())?;
        }
        Ok(rp)
    }

    fn base() -> Record {
        Record::Base { dev: DEV, mount_point: mp() }
    }

    #[test]
    fn records_round_trip_as_json_lines() {
        let recs = vec![
            base(),
            Record::Ino { s: 1, ino: 2, path: "/a b\nc".into() },
            Record::Mv { s: 2, ino: 2, path: "/d".into(), dropped: Some(3) },
            Record::Open { s: 3, rec: open_rec(5, 2, "/d") },
            Record::Stream { s: 4, write_path: "/c/write_5".into(), uploads_base: "u".into() },
        ];
        for r in recs {
            let line = serde_json::to_string(&r).unwrap();
            assert!(!line.contains('\n'), "one record per line: {}", line);
            assert_eq!(serde_json::from_str::<Record>(&line).unwrap(), r);
        }
    }

    #[test]
    fn a_log_for_another_connection_is_refused() {
        let other = Record::Base { dev: ConnDev { major: 0, minor: 74 }, mount_point: mp() };
        assert!(replay(vec![other]).is_err());
        let elsewhere = Record::Base { dev: DEV, mount_point: "/mnt/x".into() };
        assert!(replay(vec![elsewhere]).is_err());
        assert!(replay(vec![Record::Ino { s: 1, ino: 2, path: "/a".into() }]).is_err());
    }

    #[test]
    fn rename_moves_the_node_id_and_drops_the_displaced_one() {
        // LibreOffice's save: tmp (ino 3) renamed over the document (ino 2).
        let rp = replay(vec![
            base(),
            Record::Ino { s: 1, ino: 2, path: "/doc.odt".into() },
            Record::Ino { s: 2, ino: 3, path: "/lu1.tmp".into() },
            Record::Mv { s: 3, ino: 3, path: "/doc.odt".into(), dropped: Some(2) },
        ])
        .unwrap();
        assert_eq!(rp.inodes.get(&3), Some(&PathBuf::from("/doc.odt")));
        assert_eq!(rp.inodes.get(&2), None, "the replaced document's node id must not resolve to the new file");
        assert_eq!(rp.paths.get(Path::new("/lu1.tmp")), None);
        assert_eq!(rp.max_ino, 3);
    }

    #[test]
    fn handles_follow_open_close_unlink_and_retarget() {
        let rp = replay(vec![
            base(),
            Record::Open { s: 1, rec: open_rec(5, 2, "/a") },
            Record::Open { s: 2, rec: open_rec(6, 3, "/b") },
            Record::Retarget { s: 3, fh: 5, path: "/z".into() },
            Record::Unlinked { s: 4, fh: 5 },
            Record::Close { s: 5, fh: 6 },
            Record::Fh { s: 6, next: 40 },
        ])
        .unwrap();
        assert_eq!(rp.handles.keys().copied().collect::<Vec<_>>(), vec![5]);
        let h = &rp.handles[&5].rec;
        assert_eq!(h.path, PathBuf::from("/z"));
        assert!(h.unlinked);
        assert_eq!(rp.max_fh, 6, "a closed handle's number is still never reused");
        assert_eq!(rp.fh_mark, 40);
        assert_eq!(rp.max_seq, 6);
    }

    #[test]
    fn snapshot_overlays_only_handles_open_when_it_was_taken() {
        let mut rp = replay(vec![
            base(),
            Record::Open { s: 1, rec: open_rec(5, 2, "/a") },
            Record::Open { s: 9, rec: open_rec(6, 3, "/b") },
        ])
        .unwrap();
        let hs = |fh| HandleState {
            fh,
            path: "/a".into(),
            dirty: true,
            total_written: 7,
            stream_eligible: false,
            chunk: None,
            upload_failed: false,
            unlinked: false,
            created: false,
            original_etag: None,
        };
        rp.overlay(HandlesSnapshot { dev: DEV, mount_point: mp(), s: 5, handles: vec![hs(5), hs(6)] }, DEV, &mp());
        assert!(rp.handles[&5].exact.is_some());
        assert!(rp.handles[&6].exact.is_none(), "fh 6 was opened after the snapshot, so its state there is not its own");
    }

    fn staging(size: u64, ctime: i64) -> StagingId {
        StagingId { ino: 9, size, ctime, ctime_nsec: 0 }
    }

    fn write_handle(staging_at_open: StagingId) -> RestoredHandle {
        let mut rec = open_rec(5, 2, "/a");
        rec.write_path = Some("/c/write_5".into());
        rec.staging = Some(staging_at_open);
        RestoredHandle { seq: 1, rec, exact: None }
    }

    #[test]
    fn an_untouched_write_handle_stays_clean() {
        // LibreOffice keeps the document open O_RDWR and saves through a temp
        // file: closing that fd later must not upload the stale staging copy.
        let h = write_handle(staging(100, 10));
        let ws = write_state(&h, Some(staging(100, 10)), None).unwrap();
        assert!(!ws.dirty);
        assert!(!ws.upload_failed);
    }

    #[test]
    fn a_written_handle_is_dirty_even_with_the_same_size() {
        let h = write_handle(staging(100, 10));
        let ws = write_state(&h, Some(staging(100, 11)), None).unwrap();
        assert!(ws.dirty);
        assert_eq!(ws.total_written, 100);
        assert!(!ws.stream_eligible, "a restored handle stages the whole file");
    }

    #[test]
    fn a_handle_that_started_streaming_comes_back_failed() {
        let h = write_handle(staging(0, 10));
        let ws = write_state(&h, Some(staging(4096, 12)), Some("https://x/uploads/u/abc")).unwrap();
        assert!(ws.upload_failed);
        assert!(ws.dirty);
        assert_eq!(ws.chunk.as_ref().map(|c| c.uploads_base.as_str()), Some("https://x/uploads/u/abc"));
    }

    #[test]
    fn a_handle_without_its_staging_file_is_lost() {
        let h = write_handle(staging(100, 10));
        assert!(write_state(&h, None, None).is_err());
    }

    #[test]
    fn a_planned_restart_restores_streaming_exactly() {
        let mut h = write_handle(staging(0, 10));
        h.exact = Some(HandleState {
            fh: 5,
            path: "/a".into(),
            dirty: true,
            total_written: 300,
            stream_eligible: true,
            chunk: Some(ChunkRec { uploads_base: "u".into(), next_index: 2, bytes_confirmed: 200 }),
            upload_failed: false,
            unlinked: false,
            created: true,
            original_etag: None,
        });
        let ws = write_state(&h, Some(staging(100, 30)), Some("u")).unwrap();
        assert!(!ws.upload_failed);
        assert!(ws.stream_eligible);
        assert_eq!(ws.chunk, Some(ChunkRec { uploads_base: "u".into(), next_index: 2, bytes_confirmed: 200 }));
        assert_eq!(ws.total_written, 300);
    }

    #[test]
    fn load_tolerates_a_torn_last_record_but_not_a_corrupt_middle() {
        let dir = tempfile::tempdir().unwrap();
        let good = |r: &Record| serde_json::to_string(r).unwrap();
        let lines = [good(&base()), good(&Record::Ino { s: 1, ino: 2, path: "/a".into() })];
        std::fs::write(dir.path().join(LOG_FILE), format!("{}\n{}\n{{\"t\":\"ino\",\"s\":2,", lines[0], lines[1])).unwrap();
        let r = load(dir.path(), DEV, &mp()).unwrap();
        assert_eq!(r.inodes.get(&2), Some(&PathBuf::from("/a")));

        std::fs::write(dir.path().join(LOG_FILE), format!("{}\ngarbage\n{}\n", lines[0], lines[1])).unwrap();
        assert!(load(dir.path(), DEV, &mp()).is_err());
    }

    #[test]
    fn a_restore_numbers_past_the_reservation_not_just_the_logged_ids() {
        // ids 2..=4 logged, 5 handed out with its record still queued at the crash:
        // the reservation alone must keep the successor from reusing 5.
        let rp = replay(vec![
            base(),
            Record::Reserve { s: 1, upto: reservation_after(2) },
            Record::Ino { s: 2, ino: 2, path: "/a".into() },
            Record::Ino { s: 3, ino: 4, path: "/b".into() },
        ])
        .unwrap();
        let r = rp.finish();
        assert_eq!(r.max_ino, 4);
        assert_eq!(r.next_inode(), RESERVE_BLOCK);
        assert!(r.next_inode() > 5);
        assert_eq!(r.inodes.get(&5), None, "an unlogged id stays unknown (ENOENT)");
    }

    #[test]
    fn reservations_cover_whole_blocks() {
        assert_eq!(reservation_after(2), RESERVE_BLOCK);
        assert_eq!(reservation_after(RESERVE_BLOCK - 1), RESERVE_BLOCK);
        assert_eq!(reservation_after(RESERVE_BLOCK), 2 * RESERVE_BLOCK);
        // A log without any reservation (older daemon) still numbers past its ids.
        let r = replay(vec![base(), Record::Ino { s: 1, ino: 9, path: "/a".into() }]).unwrap().finish();
        assert_eq!(r.next_inode(), 10);
    }

    #[test]
    fn compaction_waits_for_the_log_to_double() {
        assert!(!needs_compaction(COMPACT_BYTES, 0));
        assert!(needs_compaction(COMPACT_BYTES + 1, 0));
        // Live state alone is 100 MiB: rewriting it every tick would never end.
        let live = 100 * 1024 * 1024;
        assert!(!needs_compaction(live + 1, live));
        assert!(!needs_compaction(2 * live, live));
        assert!(needs_compaction(2 * live + 1, live));
    }

    fn writer_at(dir: &Path) -> Writer {
        let path = dir.join(LOG_FILE);
        Writer {
            file: OpenOptions::new().create(true).append(true).open(&path).unwrap(),
            path,
            buf: Vec::new(),
            seq: 0,
            bytes: 0,
            compacted_bytes: 0,
            dev: DEV,
            mount_point: mp(),
            reserved_upto: 0,
            opens: HashMap::new(),
            streams: HashMap::new(),
            last_fh_mark: 0,
            tail: None,
            failed: false,
        }
    }

    #[test]
    fn queued_records_reach_the_file_on_flush_and_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer_at(dir.path());
        w.append(&base(), true);
        w.append(&Record::Ino { s: 1, ino: 2, path: "/a".into() }, false);
        let read = || std::fs::read_to_string(dir.path().join(LOG_FILE)).unwrap();
        assert_eq!(read().lines().count(), 1, "a node-id record is only queued");
        w.append(&Record::Mv { s: 2, ino: 2, path: "/b".into(), dropped: None }, true);
        let text = read();
        let kinds: Vec<&str> = text.lines().map(|l| &l[..12]).collect();
        assert_eq!(kinds.len(), 3, "a synchronous record writes what was queued before it: {:?}", kinds);
        assert!(text.lines().nth(1).unwrap().contains("\"ino\""));
        w.tail = Some(Vec::new());
        w.append(&Record::Ino { s: 3, ino: 3, path: "/c".into() }, false);
        drop(w); // a writer that goes away flushes
        assert_eq!(read().lines().count(), 4);
    }

    #[test]
    fn a_compaction_keeps_what_was_logged_while_it_ran() {
        // A writer of its own, not the process-wide LOG that parallel tests write through.
        let dir = tempfile::tempdir().unwrap();
        let mut c = crate::tests::make_test_cache();
        c.inodes.insert(900_002, "/a".into());
        c.paths.insert("/a".into(), 900_002);
        c.next_inode = 900_003;
        let mut w = writer_at(dir.path());
        // What compact() does under the locks, then a record arrives mid-write.
        w.tail = Some(Vec::new());
        let (image, path) = (w.image(&c), w.path.clone());
        log_inode(&mut w, 900_003, Path::new("/late"));
        let tmp = path.with_extension("log.tmp");
        let mut out = image.write(&tmp).unwrap();
        w.flush();
        out.write_all(&w.tail.take().unwrap()).unwrap();
        finish_file(out).unwrap();
        std::fs::rename(&tmp, &path).unwrap();
        drop(w);
        let r = load(dir.path(), DEV, &mp()).unwrap();
        assert_eq!(r.inodes.get(&900_002), Some(&PathBuf::from("/a")));
        assert_eq!(r.inodes.get(&900_003), Some(&PathBuf::from("/late")), "logged during the compaction");
        assert!(r.next_inode() >= reservation_after(900_003));
    }

    #[test]
    fn the_handles_snapshot_is_read_once() {
        let dir = tempfile::tempdir().unwrap();
        let text = serde_json::to_string(&base()).unwrap() + "\n";
        std::fs::write(dir.path().join(LOG_FILE), text).unwrap();
        let snap = HandlesSnapshot { dev: DEV, mount_point: mp(), s: 0, handles: vec![] };
        std::fs::write(dir.path().join(HANDLES_FILE), serde_json::to_vec(&snap).unwrap()).unwrap();
        load(dir.path(), DEV, &mp()).unwrap();
        assert!(!dir.path().join(HANDLES_FILE).exists(), "a later crash restore must not overlay it again");
    }

    #[test]
    fn an_undrained_hand_over_leaves_no_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(HANDLES_FILE), b"{}").unwrap();
        let files = Mutex::new(HashMap::new());
        write_handles_snapshot(dir.path(), &files, false).unwrap();
        assert!(!dir.path().join(HANDLES_FILE).exists());
        write_handles_snapshot(dir.path(), &files, false).unwrap(); // nothing to remove: still fine
    }

    #[test]
    fn load_applies_the_handles_snapshot_and_drops_streams_of_closed_handles() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = open_rec(5, 2, "/a");
        rec.write_path = Some(dir.path().join("write_5"));
        let recs = [
            base(),
            Record::Open { s: 1, rec },
            Record::Stream { s: 2, write_path: dir.path().join("write_5"), uploads_base: "u".into() },
            Record::Stream { s: 3, write_path: dir.path().join("write_9"), uploads_base: "v".into() },
        ];
        let text: String = recs.iter().map(|r| serde_json::to_string(r).unwrap() + "\n").collect();
        std::fs::write(dir.path().join(LOG_FILE), text).unwrap();
        let snap = HandlesSnapshot {
            dev: DEV,
            mount_point: mp(),
            s: 3,
            handles: vec![HandleState {
                fh: 5,
                path: "/a".into(),
                dirty: true,
                total_written: 1,
                stream_eligible: true,
                chunk: None,
                upload_failed: false,
                unlinked: false,
                created: false,
                original_etag: None,
            }],
        };
        std::fs::write(dir.path().join(HANDLES_FILE), serde_json::to_vec(&snap).unwrap()).unwrap();
        let r = load(dir.path(), DEV, &mp()).unwrap();
        assert!(r.handles[&5].exact.is_some());
        assert_eq!(r.streams.len(), 1, "only the open handle's stream is kept");
        assert_eq!(r.max_seq, 3);
    }
}
