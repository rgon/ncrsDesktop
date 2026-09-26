pub mod asset_url;
pub mod auth;
pub mod backend;
pub mod backoff;
pub mod bg;
pub mod walkers;
pub mod config;
pub mod desktop;
pub mod login_flow;
pub mod edit_locally;
pub mod filename_validation;
pub mod fuse_notify;
pub mod change_log;
pub mod ipc;
pub mod http_clients;
mod iomode;
mod path_seq;
pub mod mutation_journal;
pub mod nextcloud;
pub mod notifications;
pub mod notify_push;
pub mod preview;
pub mod propfind;
pub mod remote_wipe;
pub mod search;
mod seccomp_harden;
pub mod webdav_ops;

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fuser::{
    BackingId, BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, InitFlags, KernelConfig, LockOwner, MountOption, OpenFlags, RenameFlags,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry,
    ReplyOpen, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};

pub use config::{configuration_parser, MountOptions};

/// The transport the out-of-band clients — unified search, notifications —
/// should use for this server right now.
///
/// [`MountOptions::http3`] is only what the user configured. Once the mount-time
/// probe has found QUIC unusable on this network the daemon runs on HTTP/2 and
/// records it, but those clients are built outside [`http_clients::HttpClients`]
/// and cannot see that through the options alone. Ask this instead: a QUIC-only
/// client on a network that blocks UDP/443 fails every call it makes.
pub fn http3_effective(options: &MountOptions) -> bool {
    http_clients::http3_available(options.http3, &h3_demotion_marker(&options.url))
}

/// Resolve a server-supplied remote path to its location inside the mount.
///
/// Every caller of this feeds the result to something that acts on it — the
/// desktop file manager, `xdg-open` — from a path the *server* chose: a search
/// hit's directory, the `pathForUser` of an edit-locally token. Two ways out of
/// the mount have to be closed, and only one of them is obvious:
///
/// - a `..` segment walks up from inside;
/// - a path that is still absolute after trimming makes [`Path::join`] discard
///   the mount point entirely and return the server's path verbatim. Trimming
///   only the first `/` (`strip_prefix`) leaves `//etc/passwd` absolute, which
///   is how this was missed.
///
/// `None` for anything that does not name a file inside `mount_point`.
pub fn mount_local_path(mount_point: &Path, remote_path: &str) -> Option<PathBuf> {
    let rel = remote_path.trim_start_matches('/');
    if rel.is_empty() || rel.split(['/', '\\']).any(|seg| seg == ".." || seg == ".") {
        return None;
    }
    let joined = mount_point.join(rel);
    // The checks above already guarantee this; it is asserted rather than
    // assumed because the cost of being wrong here is arbitrary-path access.
    joined.starts_with(mount_point).then_some(joined)
}
use ipc::{FileStatus, StatusMap};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use backend::RemoteEntry;
use serde::{Deserialize, Serialize};

#[cfg(unix)]
use std::os::unix::fs::FileExt;

pub(crate) trait MutexExt<T> {
    fn safe_lock(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for Mutex<T> {
    fn safe_lock(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub(crate) trait RwLockExt<T> {
    fn safe_read(&self) -> RwLockReadGuard<'_, T>;
    fn safe_write(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T> RwLockExt<T> for RwLock<T> {
    fn safe_read(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(|e| e.into_inner())
    }
    fn safe_write(&self) -> RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(|e| e.into_inner())
    }
}

const TTL: Duration = Duration::from_secs(30);
const DIR_CACHE_TTL: Duration = Duration::from_secs(10);
const OPTIMISTIC_TTL_CONNECTED: Duration = Duration::from_secs(86400);
const OPTIMISTIC_TTL_FALLBACK: Duration = Duration::from_secs(300);
const PROPFIND_TIMEOUT: Duration = Duration::from_secs(15);

// While notify-push is live, invalidations arrive as events, a dead socket is
// caught by the watcher's own keepalive, and a reconnect revalidates the tree by
// etag — so the configured window would only buy probes nothing needs. It is not
// switched off entirely: the *server* can fail to emit a notify_file at all, as
// changes landing through an external storage mount or `occ` may never produce
// one, and no amount of client-side detection sees those. Hence a long backstop.
const CONNECTED_MAX_STALE_FLOOR: Duration = Duration::from_secs(86400);
// How long a directory whose forced re-list failed is served from cache before the
// next attempt, so an unreachable server costs one timeout per minute per directory
// instead of one per readdir.
const EXPIRY_RETRY_COOLDOWN: Duration = Duration::from_secs(60);

fn effective_max_stale(configured: Option<Duration>, notify_push_connected: &AtomicBool) -> Option<Duration> {
    let base = configured?;
    if notify_push_connected.load(Ordering::Relaxed) {
        Some(base.max(CONNECTED_MAX_STALE_FLOOR))
    } else {
        Some(base)
    }
}

fn effective_dir_ttl(optimistic_listing: bool, notify_push_connected: &AtomicBool) -> Duration {
    if !optimistic_listing {
        return DIR_CACHE_TTL;
    }
    if notify_push_connected.load(Ordering::Relaxed) {
        OPTIMISTIC_TTL_CONNECTED
    } else {
        OPTIMISTIC_TTL_FALLBACK
    }
}
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a download waits for one of the `read_throttle` slots.
const DOWNLOAD_SLOT_WAIT: Duration = Duration::from_secs(30);

/// Longest any single operation on the *read* client may go without progress:
/// waiting for the response headers, or one `Read::read` of a body.
///
/// This is what bounds a stalled read-ahead body, and it has to be a client-level
/// setting. On reqwest 0.13's blocking client a request's own `.timeout()` becomes
/// both the async *total* deadline for the whole exchange and the bound on each
/// blocking read call, so a value short enough to catch a stall would also kill a
/// healthy 64 MB window on a slow link. `blocking::ClientBuilder::timeout`, by
/// contrast, is never handed to the async client: it only bounds each blocking
/// wait (`execute_request`, `Response::read`) and resets on every call, which is a
/// true per-read stall timeout — and, because it sits above the transport, it
/// holds over QUIC exactly as over TCP. Range streams therefore carry no request
/// timeout of their own and get this one; `download_file`/`read_file_range` keep
/// their explicit per-request timeouts, which take precedence over it.
///
/// The 0.1.77 hang this closes: four readers on one 256 MB file, one QUIC
/// connection dies, and every stream's body read fails together.
const READ_STALL_TIMEOUT: Duration = Duration::from_secs(15);
/// QUIC idle timeout for the read clients: quinn's and Chrome's default. See the
/// read client's construction for why it is not shorter.
const H3_MAX_IDLE: Duration = Duration::from_secs(30);
/// Per-stream QUIC receive window of the read clients.
///
/// quinn-proto keeps at most 1024 separate out-of-order spans per stream
/// (`MAX_CHUNKS` in its assembler) and past that closes the *whole connection*
/// with `INTERNAL_ERROR: too many gaps in stream buffer`, killing every stream on
/// it. Each lost packet the server keeps sending past leaves one gap, so the
/// window caps how many can pile up: at worst every other ~1200-byte packet is
/// missing, 2 MiB / 2400 B ≈ 870 spans, under the limit. 4 MiB (≈1750) was not:
/// with several parallel connections on a lossy path it tripped within seconds
/// of a cold read, under BBR and CUBIC alike, and every window then failed with
/// EIO. 2 MiB still allows ~170 MB/s per stream at 12 ms RTT, and measured
/// faster than 4 MiB here (34-37 vs 31-33 MiB/s) since nothing is torn down.
const H3_STREAM_RECEIVE_WINDOW: u64 = 2 * 1024 * 1024;
/// How long a warm QUIC read connection is reused: just under H3_MAX_IDLE, so the
/// pool never hands out a connection quinn is about to close for idleness.
const H3_READ_POOL_IDLE: Duration = Duration::from_secs(28);
/// How long a FUSE READ waits for a `read_throttle` slot before it is answered
/// EAGAIN. Replaces an untimed `acquire()`, which is how a READ went unanswered
/// forever (the reader stuck in D state in `folio_wait_bit_common`).
const READ_SLOT_WAIT: Duration = Duration::from_secs(15);
/// Budget for opening one range stream: slot waits, attempts and backoff. The
/// final attempt can overrun it by at most one READ_STALL_TIMEOUT header wait.
const RANGE_OPEN_BUDGET: Duration = Duration::from_secs(30);
/// Budget for the bytes a READ is actually waiting for, once headers are in.
const FIRST_BYTES_DEADLINE: Duration = Duration::from_secs(30);
/// Longest a job may sit in the `read` pool's queue and still be worth running.
/// Past this the kernel has waited long enough; answer EAGAIN and move on, so a
/// backlog drains in bounded time instead of each queued read doing its full wait.
const READ_QUEUE_MAX_WAIT: Duration = Duration::from_secs(30);
/// Longest a READ's whole-file fallback download (after its range read failed
/// with a server answer) may take, slot wait and retries included.
const READ_FALLBACK_BUDGET: Duration = Duration::from_secs(60);
/// Error `do_range_read_stream` returns when no `read_throttle` slot freed up in
/// time. Deliberately matches neither `is_transient_network_err` nor
/// `is_timeout_err`: a busy daemon is not an unreachable server, so it must not
/// flip the mount offline or fall back to a whole-file download.
const READ_SLOTS_BUSY_ERR: &str = "range read: all read slots busy";
/// Error `do_range_read_stream` returns when the server sent no response headers
/// within READ_STALL_TIMEOUT on any attempt. Like READ_SLOTS_BUSY_ERR it matches
/// neither network-down classifier: a slow server is not an unreachable one.
///
/// The header wait and the body-stall bound are one setting on reqwest's blocking
/// client (a request's own `.timeout()` would instead become a total deadline on
/// the body too), so a longer header budget is not available without losing the
/// stall watchdog. The read path answers it with EAGAIN and a connectivity probe.
const READ_HEADER_TIMEOUT_ERR: &str = "range read: server sent no response headers in time";
/// Prefix of `read_exact_from_stream`'s error when the body stalled or crawled
/// before the first bytes a READ needs arrived.
const READ_BODY_SLOW_PREFIX: &str = "range read too slow";
/// Error `get_or_list_dir` returns when the path it was asked to list turned out
/// to be a file (see `FsCache::not_dirs`).
const NOT_A_DIRECTORY_ERR: &str = "not a directory";
/// Error for a lookup the DNS pool refused (`http_clients::is_dns_refusal`).
/// Matches neither network-down classifier: answered EAGAIN, never offline.
const DNS_BUSY_ERR: &str = "range read: name lookup refused (lookup pool busy)";

/// Whether a range-open error means "try again shortly" (daemon busy, server slow,
/// offline blip) rather than a wrong answer: waiters get EAGAIN for these.
fn is_retry_later_err(e: &str) -> bool {
    e == READ_SLOTS_BUSY_ERR || e == READ_HEADER_TIMEOUT_ERR || e == OFFLINE_READ_ERR || e == DNS_BUSY_ERR
        || read_err_is_network_down(e)
}

/// Asks the connectivity monitor to probe the server now rather than at its next
/// tick. How a read that only proved "slow", not "gone", gets reachability decided
/// by the one component that owns that verdict.
fn request_probe(conn: &ConnInfo) {
    conn.probe_soon.store(true, Ordering::Relaxed);
}
// Bounds only the TCP/TLS connect phase, independent of the (longer) per-request
// body timeouts. Keeps a legitimately slow large download alive while making a
// dead network surface in seconds instead of after the full request timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const GHOST_TTL: Duration = Duration::from_secs(10);
// Files at or below this size are downloaded eagerly in open() so that parallel
// open() calls run parallel downloads.  This makes MIME magic-byte detection
// (which GLib 2.80 triggers for any file with an unrecognised extension) run in
// parallel rather than sequentially, cutting per-directory latency from O(N*RTT)
// to O(RTT) for any set of small files opened concurrently.

#[derive(Clone, Copy)]
pub(crate) enum GhostKind {
    HiddenAdd,
    VisibleDelete { attr: FileAttr },
}

#[derive(Clone)]
pub(crate) struct GhostEntry {
    pub kind: GhostKind,
    pub created_at: Instant,
    pub rename_pair_id: Option<u64>,
}

pub(crate) type GhostMap = Arc<Mutex<HashMap<PathBuf, GhostEntry>>>;

const PATH_ENCODE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'#')
    .add(b'%')
    .add(b'?')
    .add(b'[')
    .add(b']')
    .add(b'{')
    .add(b'}');

// ── HTTP request throttle ────────────────────────────────────────────────────

pub struct Throttle {
    state: Mutex<Slots>,
    cv: Condvar,
    max: usize,
}

/// Which of a throttle's slots are taken. Slots are numbered so a holder can own
/// a resource per slot — `read_throttle` slot `i` downloads over read client `i`
/// (see `http_clients::DOWNLOAD_CONNECTIONS`).
struct Slots {
    in_use: usize,
    busy: Vec<bool>,
    /// Until when each slot is quarantined (see [`Throttle::quarantine`]).
    bad_until: Vec<Option<Instant>>,
    /// Next-fit cursor: where the search for a free slot starts. Rotating it
    /// spreads work across slots instead of always reusing the lowest one.
    next: usize,
}

impl Slots {
    /// Takes a free slot: the first healthy one from the cursor on, or — when every
    /// free slot is quarantined — the one whose quarantine ends soonest, so a
    /// caller is never refused a slot that exists.
    fn take(&mut self) -> usize {
        let n = self.busy.len();
        let now = Instant::now();
        let healthy = (0..n)
            .map(|i| (self.next + i) % n)
            .find(|&i| !self.busy[i] && self.bad_until[i].is_none_or(|t| t <= now));
        let slot = healthy.unwrap_or_else(|| {
            (0..n)
                .filter(|&i| !self.busy[i])
                .min_by_key(|&i| self.bad_until[i])
                .expect("in_use < max implies a free slot")
        });
        self.busy[slot] = true;
        self.in_use += 1;
        self.next = (slot + 1) % n;
        slot
    }
}

/// A held throttle slot, given back on drop. RAII is the only way to release one,
/// so a slot cannot outlive its holder on any path, unwinding included.
pub struct ThrottleGuard<'a> {
    throttle: &'a Throttle,
    slot: usize,
}

impl ThrottleGuard<'_> {
    /// This permit's slot number, in `0..max`. No other live permit of the same
    /// throttle has the same one.
    pub fn slot(&self) -> usize {
        self.slot
    }

    /// Turns the permit into one that can move to another thread's job, keeping
    /// the slot held throughout. `t` must be the throttle this permit came from.
    fn into_owned(self, t: &Arc<Throttle>) -> OwnedSlot {
        assert!(std::ptr::eq(self.throttle, Arc::as_ptr(t)), "a permit only converts against its own throttle");
        let slot = self.slot;
        // The guard is a reference and an index: forgetting it releases nothing and
        // leaks nothing; the returned OwnedSlot now owns the release.
        std::mem::forget(self);
        OwnedSlot { throttle: Some(Arc::clone(t)), slot }
    }
}

/// A held throttle slot that owns its throttle, so it can travel into a pool job.
/// Released on drop like a [`ThrottleGuard`], or turned back into one with `bind`.
pub struct OwnedSlot {
    throttle: Option<Arc<Throttle>>,
    slot: usize,
}

impl OwnedSlot {
    /// The same held slot as a guard borrowing `t` (which must be the throttle it
    /// came from). Exactly one of the two ever releases it.
    fn bind(mut self, t: &Throttle) -> ThrottleGuard<'_> {
        let own = self.throttle.take().expect("an OwnedSlot holds its throttle until bound or dropped");
        assert!(std::ptr::eq(Arc::as_ptr(&own), t), "a slot binds only to its own throttle");
        ThrottleGuard { throttle: t, slot: self.slot }
    }
}

impl Drop for OwnedSlot {
    fn drop(&mut self) {
        if let Some(t) = self.throttle.take() {
            t.release(self.slot);
        }
    }
}

impl Throttle {
    pub fn new(max: usize) -> Self {
        Throttle {
            state: Mutex::new(Slots { in_use: 0, busy: vec![false; max], bad_until: vec![None; max], next: 0 }),
            cv: Condvar::new(),
            max,
        }
    }

    /// Keeps `slot` out of use for `dur` while any other slot is free.
    ///
    /// For `read_throttle`, a slot is a QUIC connection (see
    /// `http_clients::DOWNLOAD_CONNECTIONS`). One that just stalled or timed out is
    /// usually silently dead, and reqwest only drops it once quinn's idle timeout
    /// closes it — so resuming on the same slot waits out another stall. Quarantining
    /// it for H3_MAX_IDLE sends the retry to a different connection and, by the time
    /// the slot is healthy again, its old connection has been closed and replaced.
    pub fn quarantine(&self, slot: usize, dur: Duration) {
        let mut st = self.state.safe_lock();
        if let Some(b) = st.bad_until.get_mut(slot) {
            *b = Some(Instant::now() + dur);
        }
    }

    /// For callers on the FUSE dispatch thread, which must never wait unbounded for a slot.
    pub fn acquire_timeout(&self, timeout: Duration) -> Option<ThrottleGuard<'_>> {
        self.acquire_leaving(0, timeout)
    }

    /// Like [`acquire_timeout`](Self::acquire_timeout), but only takes a slot while
    /// at least `spare` others stay free afterwards.
    ///
    /// For speculative work (a sequential reader's look-ahead window, a window's
    /// extra segments) that must never be the reason a foreground read waits for a
    /// slot: it only runs on capacity nobody is asking for. A zero `timeout` is a
    /// pure try.
    pub fn acquire_leaving(&self, spare: usize, timeout: Duration) -> Option<ThrottleGuard<'_>> {
        let limit = self.max.saturating_sub(spare);
        let st = self.state.safe_lock();
        let (mut st, _) = self.cv
            .wait_timeout_while(st, timeout, |s| s.in_use >= limit)
            .unwrap_or_else(|e| e.into_inner());
        if st.in_use >= limit {
            return None;
        }
        let slot = st.take();
        Some(ThrottleGuard { throttle: self, slot })
    }

    pub fn acquire(&self) -> ThrottleGuard<'_> {
        let mut st = self.state.safe_lock();
        if st.in_use >= self.max {
            let t = Instant::now();
            while st.in_use >= self.max {
                st = self.cv.wait(st).unwrap();
            }
            let waited = t.elapsed();
            if waited.as_millis() > 5 {
                log::debug!("throttle: waited {:?} for slot (in_flight={})", waited, st.in_use);
            }
        }
        let slot = st.take();
        ThrottleGuard { throttle: self, slot }
    }
}

impl Drop for ThrottleGuard<'_> {
    fn drop(&mut self) {
        self.throttle.release(self.slot);
    }
}

impl Throttle {
    fn release(&self, slot: usize) {
        let mut st = self.state.safe_lock();
        st.busy[slot] = false;
        st.in_use -= 1;
        // notify_all, not notify_one: waiters no longer share one predicate (an
        // `acquire_leaving` caller needs more than one free slot), so the single
        // waiter notify_one picked could be one that goes straight back to sleep,
        // leaving a foreground read that *could* have run parked until its timeout.
        self.cv.notify_all();
    }

    /// A free slot right now (leaving `spare` others free), as an [`OwnedSlot`]
    /// that can move into a job. Never waits.
    fn try_acquire_owned(self: &Arc<Self>, spare: usize) -> Option<OwnedSlot> {
        self.acquire_leaving(spare, Duration::ZERO).map(|g| g.into_owned(self))
    }
}

// ── Read-ahead memory budget ─────────────────────────────────────────────────

/// Bytes every read-ahead window together may reserve. A handle can hold its
/// current window, its look-ahead, the window it just left (`prev_buf`) and —
/// until that window's pump notices — a superseded one, each up to the 64 MB
/// ceiling (plus, when segmented, one segment of transient copy), so without a
/// global cap a few
/// busy readers could pin gigabytes. When the budget is spent, new windows shrink
/// (never below what the READ waiting on them needs) and look-aheads are skipped;
/// no reply ever waits on it.
const READ_AHEAD_BUDGET: u64 = 512 * 1024 * 1024;
static READ_AHEAD_RESERVED: AtomicU64 = AtomicU64::new(0);

/// A share of READ_AHEAD_BUDGET, returned on drop. Lives inside the window's
/// `StreamState`, so it is held exactly as long as the window's bytes can be.
struct BufferReservation(u64);

impl BufferReservation {
    /// Up to `want` bytes of what is left, but never less than `floor` (capped at
    /// `want`): a foreground window must be able to serve the READ that opened it
    /// even when the budget is spent, so the floor may overdraw it slightly.
    fn up_to(want: u64, floor: u64) -> BufferReservation {
        let floor = floor.min(want);
        let mut grant = 0;
        let _ = READ_AHEAD_RESERVED.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
            grant = READ_AHEAD_BUDGET.saturating_sub(used).min(want).max(floor);
            Some(used + grant)
        });
        BufferReservation(grant)
    }

    /// All of `want`, or nothing: for speculative windows.
    fn exactly(want: u64) -> Option<BufferReservation> {
        READ_AHEAD_RESERVED
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                (used + want <= READ_AHEAD_BUDGET).then_some(used + want)
            })
            .ok()
            .map(|_| BufferReservation(want))
    }
}

impl BufferReservation {
    /// Grows this reservation to `want`, all or nothing.
    fn grow_to(&mut self, want: u64) -> bool {
        if want <= self.0 {
            return true;
        }
        let extra = want - self.0;
        let ok = READ_AHEAD_RESERVED
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                (used + extra <= READ_AHEAD_BUDGET).then_some(used + extra)
            })
            .is_ok();
        if ok {
            self.0 = want;
        }
        ok
    }

    /// Gives back everything above `want`.
    fn shrink_to(&mut self, want: u64) {
        if want < self.0 {
            READ_AHEAD_RESERVED.fetch_sub(self.0 - want, Ordering::SeqCst);
            self.0 = want;
        }
    }
}

impl Drop for BufferReservation {
    fn drop(&mut self) {
        READ_AHEAD_RESERVED.fetch_sub(self.0, Ordering::SeqCst);
    }
}

/// Recent download rate of *segmented* windows (all segments together), in bytes
/// per second; 0 = no recent sample. An exponentially weighted average,
/// mount-wide. Only segmented windows feed it: a single-stream window measures one
/// connection, not the link, and letting those in meant one slow sample switched
/// segmenting off and nothing could ever switch it back on.
static WINDOW_THROUGHPUT: AtomicU64 = AtomicU64::new(0);
/// When WINDOW_THROUGHPUT last got a sample, in ms since THROUGHPUT_EPOCH.
static WINDOW_THROUGHPUT_AT: AtomicU64 = AtomicU64::new(0);
/// Every this many windows one is segmented regardless of the estimate, so a
/// link that got faster is noticed.
static WINDOWS_SINCE_PROBE: AtomicU64 = AtomicU64::new(0);
const SEGMENT_PROBE_EVERY: u64 = 8;
/// An estimate older than this is forgotten ("unknown" segments again).
const THROUGHPUT_MAX_AGE: Duration = Duration::from_secs(60);
/// Below this measured rate a window is never split into segments: on a slow link
/// segments only divide the same bandwidth into more streams that each look
/// stalled, and their extra requests buy nothing.
const SEGMENT_MIN_THROUGHPUT: u64 = 1024 * 1024;

fn throughput_clock_ms() -> u64 {
    static THROUGHPUT_EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    THROUGHPUT_EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Folds one finished segmented window's rate into WINDOW_THROUGHPUT. Only
/// windows big enough to say something (≥ 1 MiB over ≥ 100 ms) count.
fn record_window_throughput(bytes: u64, elapsed: Duration, segments: usize) {
    if segments < 2 || bytes < 1024 * 1024 || elapsed < Duration::from_millis(100) {
        return;
    }
    let rate = (bytes as f64 / elapsed.as_secs_f64()) as u64;
    let _ = WINDOW_THROUGHPUT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
        Some(if old == 0 { rate } else { (old * 3 + rate) / 4 })
    });
    WINDOW_THROUGHPUT_AT.store(throughput_clock_ms().max(1), Ordering::Relaxed);
}

/// Whether the next large window should be segmented: yes when the estimate is
/// unknown or stale (THROUGHPUT_MAX_AGE), when it says the link is fast, and for
/// one window in SEGMENT_PROBE_EVERY anyway, so the estimate keeps being measured.
fn segments_worthwhile() -> bool {
    let at = WINDOW_THROUGHPUT_AT.load(Ordering::Relaxed);
    let fresh = at != 0 && throughput_clock_ms().saturating_sub(at) < THROUGHPUT_MAX_AGE.as_millis() as u64;
    let r = WINDOW_THROUGHPUT.load(Ordering::Relaxed);
    !fresh || r >= SEGMENT_MIN_THROUGHPUT
        || WINDOWS_SINCE_PROBE.fetch_add(1, Ordering::Relaxed) % SEGMENT_PROBE_EVERY == 0
}

// ── Cache data types ──────────────────────────────────────────────────────────

struct DirCacheEntry {
    files: Arc<Vec<RemoteEntry>>,
    self_entry: Option<RemoteEntry>,
    etag: Option<String>,
    at: Instant,
    // Wall-clock fetch time, used only for the dir_cache_max_stale_mins check.
    // `at` is monotonic: it neither survives a restart nor advances across
    // suspend, so it cannot answer "how old is this listing really?".
    fetched_at: SystemTime,
    refreshing: bool,
    invalidated: bool,
    // Set when the entry exceeded dir_cache_max_stale_mins. Behaves like a cache
    // miss until a fresh PROPFIND replaces it: like `invalidated`, the partial
    // incremental stream is NOT served for it, because the kernel's continuation
    // readdir pages are answered straight from this (still stale) entry and would
    // splice a fresh prefix onto a stale suffix.
    hard_expired: bool,
    // When the forced re-list last failed. While a server is unreachable but the
    // daemon has not yet flipped offline, re-expiring on every readdir would stall
    // each one for the full PROPFIND timeout; this holds the check off briefly so
    // the cost is one attempt per cooldown rather than per listing.
    expiry_retry_after: Option<Instant>,
    // Value of ACCESS_TICK when this listing was last read, for LRU eviction.
    //
    // A counter rather than a clock: `lookup` and `getattr` touch this on every
    // path resolution, and a monotonic `fetch_add` is far cheaper than asking the
    // OS for the time on a hot path. It is also an `Atomic` so the `&self`
    // read path (`get_cached_dir_readonly`) can record an access too — a
    // directory that only ever gets stat'ed is still in use.
    last_access: AtomicU64,
}

/// Source of LRU ordering for the dir cache. Wraps after 2^64 accesses, which
/// at a billion listings a second takes ~580 years.
static ACCESS_TICK: AtomicU64 = AtomicU64::new(0);

fn next_access_tick() -> u64 {
    ACCESS_TICK.fetch_add(1, Ordering::Relaxed)
}

struct PendingDir {
    entries: Vec<RemoteEntry>,
    rx: mpsc::Receiver<RemoteEntry>,
    etag_rx: mpsc::Receiver<Result<Option<String>, String>>,
    self_rx: mpsc::Receiver<RemoteEntry>,
    etag: Option<String>,
    self_entry: Option<RemoteEntry>,
    failed: Option<String>,
}

struct FileCacheEntry {
    local_path: PathBuf,
    remote_modified: Option<SystemTime>,
    etag: Option<String>,
    kept: bool,
    size: u64,
}

/// One read-ahead window's bytes, shared by the job(s) filling it and the reads
/// waiting on it.
///
/// Readers only ever look at `data`, the contiguous prefix from the window's
/// start: its length is the window's watermark. A window fetched as several
/// concurrent segments (see `segment_plan`) parks bytes that arrive ahead of the
/// watermark in `ahead` until the prefix reaches them, so readers never see a hole.
struct StreamState {
    data: Vec<u8>,
    done: bool,
    /// Per-segment bytes not yet contiguous with `data`. Every segment covers a
    /// disjoint part of the window, so `data` plus these never exceed the window.
    ahead: Vec<SegmentBuf>,
    /// A segment gave up for good (the transport kept failing, or the server
    /// answered wrongly): `data` can never grow past the hole it left, so the
    /// window's other segments stop too, and waiters inside the hole get EIO.
    broken: bool,
    /// A segment stopped for a reason trying again can fix — superseded, no slot
    /// free for a resume, the server too slow. Like `broken` it halts the window,
    /// but waiters inside the hole get EAGAIN. Never a short reply either way.
    stopped: bool,
    /// Window-relative offset where the file ends, when the window's last request
    /// ran into the end of the body the server said the file has (Content-Range
    /// total). Only `at_eof` may use it: a later segment can reach the end while an
    /// earlier one left a hole, and the end is only the prefix's end once the
    /// prefix actually gets there.
    eof_len: Option<usize>,
    /// READs parked on this window (`wait_on_window`). A window with waiters is
    /// never stopped for being superseded: its tail is exactly what they want.
    waiters: usize,
    /// Start of the current progress period and the bytes received by then, for
    /// the window-wide trickle rule (MIN_BODY_PROGRESS).
    progress_mark: (Instant, u64),
    /// This window's share of READ_AHEAD_BUDGET, given back when the window goes.
    budget: BufferReservation,
}

#[derive(Default)]
struct SegmentBuf {
    /// Window offset of `buf[0]`.
    at: usize,
    buf: Vec<u8>,
}

impl StreamState {
    /// `plan` is the window's segments (their lengths sum to at most `budget`).
    /// Every buffer is sized once, up front, to exactly what it will hold: grown by
    /// doubling, a window's peak allocation was up to twice its size.
    fn new(mut first: Vec<u8>, plan: &[Segment], budget: BufferReservation) -> Self {
        let window: u64 = plan.iter().map(|s| s.len).sum();
        first.reserve_exact((window as usize).saturating_sub(first.len()));
        StreamState {
            data: first,
            done: false,
            ahead: plan.iter().map(|_| SegmentBuf::default()).collect(),
            broken: false,
            stopped: false,
            eof_len: None,
            waiters: 0,
            progress_mark: (Instant::now(), 0),
            budget,
        }
    }

    /// Whether `data` can still grow.
    fn halted(&self) -> bool {
        self.broken || self.stopped
    }

    /// Whether the prefix ends exactly where the server said the file does.
    fn at_eof(&self) -> bool {
        self.eof_len == Some(self.data.len())
    }

    /// Records `bytes` that segment `seg` fetched at window offset `at`. A segment
    /// always pushes its own bytes in order, so `at` continues its previous push.
    fn push(&mut self, seg: usize, at: usize, bytes: &[u8], seg_left: usize) {
        let sb = &mut self.ahead[seg];
        if sb.buf.is_empty() && at == self.data.len() {
            // The segment at the watermark: straight onto the prefix.
            self.data.extend_from_slice(bytes);
            self.absorb();
        } else {
            if sb.buf.is_empty() {
                sb.at = at;
                // What is left of this segment, exactly (see `new`).
                sb.buf.reserve_exact(seg_left);
            }
            debug_assert_eq!(sb.at + sb.buf.len(), at, "a segment pushes its bytes in order");
            sb.buf.extend_from_slice(bytes);
        }
    }

    /// Moves parked segment bytes onto the prefix for as long as one starts
    /// exactly where the prefix ends.
    fn absorb(&mut self) {
        loop {
            let end = self.data.len();
            let Some(sb) = self.ahead.iter_mut().find(|sb| !sb.buf.is_empty() && sb.at == end) else {
                break;
            };
            let mut moved = std::mem::take(&mut sb.buf);
            sb.at += moved.len();
            self.data.append(&mut moved);
        }
    }

    /// Bytes received so far, contiguous or not, for progress reporting.
    fn received(&self) -> usize {
        self.data.len() + self.ahead.iter().map(|sb| sb.buf.len()).sum::<usize>()
    }
}

/// One segment's share of a window: window-relative start and length, and
/// whether the file may legitimately end inside it (only the window's last).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Segment {
    at: u64,
    len: u64,
    last: bool,
}

/// Most segments one window is fetched as.
const MAX_WINDOW_SEGMENTS: usize = 4;
/// Smallest segment worth its own request; a window is split only when every
/// segment gets at least this much, so probes and small windows stay one GET.
const SEGMENT_MIN_BYTES: u64 = 4 * 1024 * 1024;
/// Segment boundaries are rounded to this, keeping them page- and
/// read-request-aligned.
const SEGMENT_ALIGN: u64 = 64 * 1024;
/// `read_throttle` slots an extra segment leaves free: segments speed up one
/// reader, and must never be why another reader's first READ waits for a slot.
const SEGMENT_SPARE_SLOTS: usize = 2;

/// How to fetch a `target`-byte window with at most `slots` concurrent requests.
///
/// `span` is how much of the window the file actually has (None when the size is
/// unknown). A window splits only when its size is known — a segment past the end
/// would be a 416 — into up to `slots` (and MAX_WINDOW_SEGMENTS) equal, aligned
/// parts of at least SEGMENT_MIN_BYTES each, the first large enough for the
/// `need` bytes the waiting READ asked for. Otherwise it is the single request the
/// window always was: `target` bytes from its start, ending wherever the file does.
/// Pure so the policy is testable.
fn segment_plan(target: usize, span: Option<u64>, slots: usize, need: usize) -> Vec<Segment> {
    let single = vec![Segment { at: 0, len: target as u64, last: true }];
    let Some(span) = span.map(|s| s.min(target as u64)) else { return single };
    let k = slots.min(MAX_WINDOW_SEGMENTS).min((span / SEGMENT_MIN_BYTES) as usize);
    if k < 2 {
        return single;
    }
    let per = (span / k as u64).div_ceil(SEGMENT_ALIGN) * SEGMENT_ALIGN;
    if per < need as u64 {
        return single;
    }
    let mut segs = Vec::with_capacity(k);
    let mut at = 0;
    while at < span {
        let len = per.min(span - at);
        segs.push(Segment { at, len, last: false });
        at += len;
    }
    if let Some(l) = segs.last_mut() {
        l.last = true;
    }
    segs
}

/// Next read-ahead window for a handle.
///
/// Doubles while access stays sequential, up to `ceiling`; a seek resets to
/// `READ_AHEAD_INITIAL`. Pure so the policy can be tested without a live mount.
fn next_read_ahead_window(current: usize, sequential: bool, ceiling: usize) -> usize {
    if sequential {
        current.max(READ_AHEAD_INITIAL).saturating_mul(2).min(ceiling)
    } else {
        READ_AHEAD_INITIAL.min(ceiling)
    }
}

/// Whether a read at `off..off+sz` inside the window `win_start..+win_len` makes
/// the next window due: the reader is sequential, has reached the window's second
/// half, and the file goes on past this window. Pure so the policy is testable.
fn lookahead_due(win_start: u64, win_len: u64, off: u64, sz: usize, sequential: bool, file_size: u64) -> bool {
    let win_end = win_start.saturating_add(win_len);
    sequential
        && file_size > 0
        && win_end < file_size
        && off >= win_start
        && off.saturating_add(sz as u64) >= win_start.saturating_add(win_len / 2)
}

/// What a look-ahead job fetches, decided under the `open_files` lock.
struct LookaheadPlan {
    /// The window whose reader made this due. The job installs its window only
    /// while this is still the handle's current one.
    after: Arc<(Mutex<StreamState>, Condvar)>,
    start: u64,
    len: usize,
    /// The window's memory, reserved all or nothing: a look-ahead only runs on
    /// budget nobody else needs.
    budget: BufferReservation,
}

/// If this read makes a look-ahead due (see `lookahead_due`) and none is in flight
/// or waiting, marks one in flight and returns what to fetch. The window grows
/// along the same ramp a foreground fetch would have taken.
fn plan_lookahead(of: &mut OpenFile, off: u64, sz: usize, sequential: bool, file_size: u64, ceiling: usize) -> Option<LookaheadPlan> {
    if of.lookahead_inflight || of.next_buf.is_some() {
        return None;
    }
    let ra = of.buf.as_ref()?;
    if !lookahead_due(ra.start, ra.target_len, off, sz, sequential, file_size) {
        return None;
    }
    let start = ra.start + ra.target_len;
    let after = Arc::clone(&ra.stream);
    let window = streaming_jump(of.read_ahead_window, ra.start, true, file_size, ceiling)
        .unwrap_or_else(|| next_read_ahead_window(of.read_ahead_window, true, ceiling));
    // Never ask past the end: a range starting beyond EOF is a 416, and a window
    // sized to the file's rest ends exactly where `short_reply_ok` allows a short reply.
    let len = window.min((file_size - start) as usize);
    let budget = BufferReservation::exactly(len as u64)?;
    of.read_ahead_window = window;
    of.lookahead_inflight = true;
    Some(LookaheadPlan { after, start, len, budget })
}

/// Makes the look-ahead window the current one once a read lands in it and no
/// longer in the current window.
fn promote_lookahead(of: &mut OpenFile, off: u64) {
    let covers = move |b: &ReadAheadBuf| off >= b.start && off < b.start + b.target_len;
    if of.buf.as_ref().is_some_and(covers) {
        return;
    }
    if of.next_buf.as_ref().is_some_and(covers) {
        // The old window stays reachable as `prev_buf`: READs for its tail can still
        // be in flight (the kernel issues them asynchronously, in any order), and
        // without it each would miss both windows and start a fresh GET.
        of.prev_buf = std::mem::replace(&mut of.buf, of.next_buf.take());
    }
}

/// How far into the current window a READ must be before the previous window is
/// let go even though it is not finished: by then no READ for its tail can still
/// be in flight.
const PREV_WINDOW_KEEP: u64 = 4 * 1024 * 1024;

/// Drops `prev_buf` (and with it, once its pump and waiters let go, its share of
/// the read-ahead budget) as soon as nothing can still need it: the READ landed
/// in the current window and the old one is finished, or it has no READ parked on
/// it and this READ is PREV_WINDOW_KEEP into the current window.
fn retire_prev_window(of: &mut OpenFile, off: u64) {
    let (Some(prev), Some(buf)) = (of.prev_buf.as_ref(), of.buf.as_ref()) else { return };
    if off < buf.start || off >= buf.start + buf.target_len {
        return;
    }
    let (done, waiters) = {
        let ss = prev.stream.0.lock().unwrap();
        (ss.done, ss.waiters)
    };
    if done || (waiters == 0 && off >= buf.start + PREV_WINDOW_KEEP) {
        of.prev_buf = None;
    }
}

/// A handle with no READ for this long counts as idle (a paused player).
const HANDLE_IDLE: Duration = Duration::from_secs(10);

/// Under read-ahead memory pressure (half the budget reserved), lets idle handles
/// give back their speculative windows — the look-ahead and the previous window —
/// so a paused player cannot starve everyone else's look-ahead. Its current window
/// stays: that is what it reads first when it resumes. Checked opportunistically
/// from `read()`, at most once a second, and only under pressure, so the common
/// path costs one atomic load.
fn release_idle_windows(ofs: &mut HashMap<u64, OpenFile>) {
    static LAST_SWEEP_MS: AtomicU64 = AtomicU64::new(0);
    if READ_AHEAD_RESERVED.load(Ordering::Relaxed) < READ_AHEAD_BUDGET / 2 {
        return;
    }
    let now = throughput_clock_ms();
    let last = LAST_SWEEP_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) < 1000
        || LAST_SWEEP_MS.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err()
    {
        return;
    }
    for of in ofs.values_mut() {
        if of.last_read.elapsed() >= HANDLE_IDLE && (of.next_buf.is_some() || of.prev_buf.is_some()) {
            // Their pumps see they are no longer wanted (no READ is parked on an
            // idle handle) and stop within SUPERSEDE_CHECK_EVERY.
            of.next_buf = None;
            of.prev_buf = None;
        }
    }
}

/// The window that follows a handle's first one, when that first one showed a
/// straight read through a large file; `None` to keep the normal ramp.
///
/// Every handle opens at READ_AHEAD_INITIAL, and the next window is where a
/// straight read gets fast: one that has read sequentially from offset 0 through
/// its 1 MB window (or through half of it, when the look-ahead fires) of a file of
/// at least SEQUENTIAL_START_MIN_FILE jumps straight to READ_AHEAD_SEQUENTIAL_START
/// instead of ramping 2, 4, 8 MB — three round trips saved.
///
/// Deciding from the *first* read instead cannot tell a streamer from a probe:
/// the kernel inflates any app read of 64 KiB or more at offset 0 into a 128 KiB
/// request, so a header probe, a thumbnailer's first look or a MIME sniff would
/// each have paid 8 MB (and, segmented, two download slots). Those read a few KiB
/// to a few hundred KiB and seek away, so they never qualify here, and the 1 MB
/// opening window is never segmented (see `segment_plan`). Thumbnailers opening an
/// uncached file are refused before any read anyway (`desktop::thumbguard`).
fn streaming_jump(current: usize, window_start: u64, sequential: bool, file_size: u64, ceiling: usize) -> Option<usize> {
    (sequential && window_start == 0 && current <= READ_AHEAD_INITIAL && file_size >= SEQUENTIAL_START_MIN_FILE)
        .then(|| READ_AHEAD_SEQUENTIAL_START.min(ceiling))
}

/// Whether a `read()` may answer a `sz`-byte request at `off` with only `avail`
/// bytes.
///
/// POSIX lets `read(2)` come back short, but a short FUSE *reply* does not mean the
/// same thing: on a page-cached handle the kernel records it as end-of-file for the
/// whole inode, and every later read past `off + avail` returns 0 bytes without ever
/// reaching us. The file reads as truncated from that offset on — a player hits it
/// one read-ahead window into the track, sees the track end and skips to the next
/// one, and only a fresh `open()` clears it. The MIME-magic path dodges the same
/// trap with FOPEN_DIRECT_IO; see `mime_magic_bytes`.
///
/// So a short reply is only ever safe when it really is the end of the file.
/// Anything else has to be fetched in full or fail — never quietly truncated.
/// `file_size` of 0 means "unknown", which proves nothing and so allows nothing.
fn short_reply_ok(off: u64, avail: usize, sz: usize, file_size: u64) -> bool {
    avail >= sz || (file_size > 0 && off.saturating_add(avail as u64) >= file_size)
}

/// `read_at` retried until `buf` is full or the file ends.
///
/// A single `read_at` may return fewer bytes than asked for without being at EOF,
/// and handing that straight to `reply.data` is exactly the truncation
/// `short_reply_ok` exists to prevent.
fn read_at_full(f: &std::fs::File, buf: &mut [u8], off: u64) -> std::io::Result<usize> {
    let mut filled = 0usize;
    while filled < buf.len() {
        match f.read_at(&mut buf[filled..], off + filled as u64) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// First read-ahead window on a handle, and the window a seek falls back to.
///
/// Small enough that a stray seek costs ~1 MB instead of the full configured
/// read-ahead, large enough to cover a metadata probe in one round trip. Six
/// doublings reach a 64 MB ceiling, so sustained playback still ends up with the
/// same large window it had before.
const READ_AHEAD_INITIAL: usize = 1024 * 1024;

/// Second window of a handle that read straight through its first one from
/// offset 0 of a large file; see `streaming_jump`.
const READ_AHEAD_SEQUENTIAL_START: usize = 8 * 1024 * 1024;
/// Smallest file that gets the jump: below this, 8 MB is most of the file and the
/// ramp gets there almost as fast.
const SEQUENTIAL_START_MIN_FILE: u64 = 32 * 1024 * 1024;

struct ReadAheadBuf {
    start: u64,
    stream: Arc<(Mutex<StreamState>, Condvar)>,
    target_len: u64,
}

// State for the bounded chunked-upload streaming path (see write()'s
// stream_eligible handling): once a purely-sequential write on a file with no
// pre-existing content crosses one CHUNK_SIZE, chunks are pushed to Nextcloud
// as they fill instead of the whole file landing on local disk first. There is
// deliberately no journal entry for this: unlike the legacy staging path, a
// crash or a non-sequential write while a session is open cannot be resumed
// or safely unwound (the already-uploaded prefix is no longer held locally),
// so those cases fail the handle outright rather than risk silent corruption
// or data loss. A plain sequential copy — the case this exists for — never
// hits either path.
#[derive(Clone)]
struct ChunkUploadState {
    uploads_base: String,
    next_index: u64,
    bytes_confirmed: u64,
}

struct OpenFile {
    remote_path: PathBuf,
    local: Option<PathBuf>,
    buf: Option<ReadAheadBuf>,
    write_path: Option<PathBuf>,
    dirty: bool,
    original_etag: Option<String>,
    // Bytes actually appended so far via the streaming fast path in write().
    // Also doubles as "expected next offset" for detecting a sequential write.
    total_written: u64,
    // Candidate for chunked streaming: starts true only when there is no
    // pre-existing content to seed from, and is permanently cleared on the
    // first non-sequential write or explicit truncate.
    stream_eligible: bool,
    chunk_upload: Option<ChunkUploadState>,
    // Freshness of the on-disk cached copy, decided ONCE at open() and pinned for
    // the whole handle. The read fast-paths that serve on-disk cache bytes
    // (of.local, the sync file_cache) consult this instead of re-checking
    // file_cache_matches_remote() on every read. Re-checking per read let a
    // concurrent dir-cache refresh flip the verdict mid-scan, so a single
    // sequential read would splice a stale prefix onto a freshly-fetched suffix
    // (or vice-versa) and hash to neither the old nor the new file — the
    // corruption scenario 16 reproduces. Pinning gives each handle one
    // consistent source: fresh → serve the cached copy throughout; stale → never
    // touch it, re-download via the network path (whose read-ahead buffer only
    // ever holds current bytes, so it needs no gate).
    cache_fresh: bool,
    // Present when this fh was opened with O_NOATIME|O_NOFOLLOW — GLib's exclusive
    // MIME magic-byte detection signature.  read() at offset 0 returns magic bytes
    // derived from this content type without touching the network.
    mime_detect_ct: Option<String>,
    // Largest offset-0 read answered with magic bytes, from the matching
    // desktop::sniff::SniffProbe (0 when mime_detect_ct is None).
    mime_detect_max_read: usize,
    // Offset the next read would start at to continue sequentially, i.e. the end
    // of the previous read on this handle. Updated on every read, whatever served
    // it, so a run of buffer hits still counts as sequential.
    next_expected_off: u64,
    // Current read-ahead window, grown while reads stay sequential and reset to
    // READ_AHEAD_INITIAL on a seek. Capped by the configured `read_ahead_bytes`.
    //
    // The window used to be a flat `read_ahead_bytes` (64 MB by default), fetched
    // in full on *every* buffer miss. That is efficient for straight playback and
    // ruinous for anything that seeks: a player opening a 129 MB FLAC (header,
    // seektable, then the playback position) missed the single per-handle buffer
    // about five times and pulled ~281 MB — 2.2x the file — before it could start.
    read_ahead_window: usize,
    // The window after `buf`, fetched while a sequential reader is still working
    // through `buf` so crossing the boundary costs no round trip; promoted into
    // `buf` by the first read that lands in it. At most one per handle, so a handle
    // holds at most two windows. Dropped whenever a fetch replaces `buf` (a seek).
    next_buf: Option<ReadAheadBuf>,
    // The window before `buf`, kept after a promotion so READs for its tail that
    // were still in flight are served from it (see `promote_lookahead`). Replaced
    // at the next promotion, dropped when a fetch replaces `buf`.
    prev_buf: Option<ReadAheadBuf>,
    // When the last READ on this handle arrived; see `release_idle_windows`.
    last_read: Instant,
    // A look-ahead job for this handle is queued, connecting or streaming. Cleared
    // by the job itself on every exit; keeps a second one from starting meanwhile.
    lookahead_inflight: bool,
    // Inode and kernel I/O mode this handle was opened with, returned to
    // io_modes in release(). A Passthrough handle's reads bypass ncrs entirely.
    ino: u64,
    io_kind: iomode::IoKind,
    // A write on this handle already failed with EIO after a chunk reached the server;
    // release() aborts the chunked session instead of assembling an incomplete file.
    upload_failed: bool,
    // Made by create() and not uploaded yet: the server has no copy until release().
    created: bool,
    // The path was deleted while this handle was open; release() must not re-create it.
    unlinked: bool,
    // UploadOrder generation when opened, for etag chaining (see UploadOrder::etag_for).
    opened_gen: u64,
}

/// Ordering and etag bookkeeping shared by the workers that change files on the server.
#[derive(Clone, Default)]
struct UploadOrder {
    seq: Arc<path_seq::PathSeq>,
    // Etag each path got from this daemon's latest upload, with the generation it was
    // recorded at: a handle opened before that upload must send it, not its stale etag.
    committed: Arc<Mutex<HashMap<PathBuf, (u64, Option<String>)>>>,
    gen: Arc<AtomicU64>,
}

impl UploadOrder {
    fn generation(&self) -> u64 {
        self.gen.load(Ordering::SeqCst)
    }

    fn etag_for(&self, path: &Path, opened_gen: u64, original: Option<String>) -> Option<String> {
        match self.committed.safe_lock().get(path) {
            Some((g, etag)) if *g > opened_gen => etag.clone(),
            _ => original,
        }
    }

    fn record(&self, path: &Path, etag: Option<String>) {
        let g = self.gen.fetch_add(1, Ordering::SeqCst) + 1;
        self.committed.safe_lock().insert(path.to_path_buf(), (g, etag));
    }

    fn moved(&self, from: &Path, to: &Path) {
        let mut c = self.committed.safe_lock();
        if let Some(v) = c.remove(from) {
            c.insert(to.to_path_buf(), v);
        }
    }

    fn forget(&self, path: &Path) {
        self.committed.safe_lock().remove(path);
    }

    /// Ticket for creating, replacing or removing `path` inside its parent.
    fn ticket_entry(&self, path: &Path) -> path_seq::Ticket {
        let parent = path.parent().unwrap_or(Path::new("/"));
        self.seq.ticket(&[(path, path_seq::Access::Exclusive), (parent, path_seq::Access::Shared)])
    }
}

fn plain_open_flags(kind: iomode::IoKind) -> FopenFlags {
    match kind {
        iomode::IoKind::DirectIo => FopenFlags::FOPEN_DIRECT_IO,
        _ => FopenFlags::empty(),
    }
}

/// Appends `data` to a chunk-streaming tail staging file, creating it if this
/// is the first write on the handle. A plain append (not write_at(offset))
/// works here because a write routed onto the streaming fast path is always
/// contiguous with what's already in the tail file.
fn append_to_tail_file(wp: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(wp)?;
    f.write_all(data)
}

/// Rewrites the tail staging file down to just the bytes from `skip` onward,
/// right after the first `skip` bytes have been PUT to the server as a
/// completed chunk — this is what keeps the tail bounded to ~CHUNK_SIZE
/// regardless of the total file size.
fn shrink_tail_file(wp: &Path, skip: u64) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut leftover = Vec::new();
    {
        let mut src = std::fs::File::open(wp)?;
        src.seek(SeekFrom::Start(skip))?;
        src.read_to_end(&mut leftover)?;
    }
    let tmp = wp.with_extension("tmp");
    {
        let mut dst = std::fs::File::create(&tmp)?;
        dst.write_all(&leftover)?;
    }
    std::fs::rename(&tmp, wp)
}

#[derive(Clone, Debug, PartialEq)]
pub enum SyncState {
    Idle,
    Syncing,
    Paused,
    Unmounted,
    Wiped,
    Error(String),
    /// Partially operational: sync works but a subsystem (e.g. notify_push) is unavailable.
    Degraded(String),
    /// Not operational at all: the server is unreachable, so WebDAV, notify_push
    /// and every upload are down together. Distinct from `Degraded`, which
    /// promises the rest of sync still works.
    Offline,
}

impl std::fmt::Display for SyncState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncState::Idle => write!(f, "idle"),
            SyncState::Syncing => write!(f, "syncing"),
            SyncState::Paused => write!(f, "paused"),
            SyncState::Unmounted => write!(f, "unmounted"),
            SyncState::Wiped => write!(f, "wiped"),
            SyncState::Error(e) => write!(f, "error:{}", e),
            SyncState::Degraded(r) => write!(f, "degraded:{}", r),
            SyncState::Offline => write!(f, "offline"),
        }
    }
}

// ── Error log ────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SyncErrorKind {
    UploadFailed,
    Conflict,
    PermissionDenied,
    NetworkError,
    QuotaExceeded,
    InvalidFilename,
    ServerError(u16),
    Locked,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyncError {
    pub path: PathBuf,
    pub kind: SyncErrorKind,
    pub message: String,
    pub timestamp_ms: u64,
}

pub type ErrorLog = Arc<Mutex<std::collections::VecDeque<SyncError>>>;

const MAX_ERROR_LOG: usize = 50;

pub fn push_error(log: &ErrorLog, path: PathBuf, kind: SyncErrorKind, message: String) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let err = SyncError { path, kind, message, timestamp_ms: ts };
    let mut q = log.safe_lock();
    // Suppress duplicate consecutive entries for the same path+kind to prevent
    // retry storms (e.g. disk-full failures re-attempted every second) from
    // flooding both the error log and the GUI.
    if let Some(last) = q.back() {
        if last.path == err.path && std::mem::discriminant(&last.kind) == std::mem::discriminant(&err.kind) {
            return;
        }
    }
    if q.len() >= MAX_ERROR_LOG { q.pop_front(); }
    q.push_back(err);
}

// ── Transfer progress ────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TransferDirection {
    Download,
    Upload,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransferProgress {
    pub path: PathBuf,
    pub direction: TransferDirection,
    pub bytes_done: u64,
    pub total_bytes: u64,
}

/// One transfer's slot in the [`TransferMap`]: its path plus a stream id.
///
/// Whole-file transfers (uploads, full downloads) are one per path and use id 0
/// ([`whole_file_transfer`]). Each read-ahead stream takes a fresh id
/// ([`next_stream_id`]). Keyed by path alone, concurrent streams of one file —
/// four readers of a 256 MB file, each on its own handle — overwrote and removed
/// each other's entries, which is how a stream stuck for minutes sat in TRANSFERS
/// as the only survivor while its stuck siblings had vanished from it.
pub type TransferKey = (PathBuf, u64);

pub type TransferMap = Arc<Mutex<HashMap<TransferKey, TransferProgress>>>;

/// The key of `path`'s whole-file transfer (see [`TransferKey`]).
pub fn whole_file_transfer(path: &Path) -> TransferKey {
    (path.to_path_buf(), 0)
}

/// A fresh, never-zero id for one read-ahead stream's [`TransferKey`].
fn next_stream_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The transfers as IPC `TRANSFERS` reports them: one entry per path, sorted by
/// path, exactly the shape the per-path map used to produce.
///
/// Clients key rows by path (the GUI's list is a keyed `{#each}`, which throws on
/// a duplicate key, and its mirror map is keyed by path too), so concurrent streams
/// of one file are summed into one row rather than listed apiece. An upload
/// outranks downloads of the same path: it is the newer state of the file.
pub fn transfer_snapshot(map: &TransferMap) -> Vec<TransferProgress> {
    let mut by_path: std::collections::BTreeMap<PathBuf, TransferProgress> = std::collections::BTreeMap::new();
    for tp in map.safe_lock().values() {
        match by_path.get_mut(&tp.path) {
            None => {
                by_path.insert(tp.path.clone(), tp.clone());
            }
            Some(agg) => {
                let agg_up = matches!(agg.direction, TransferDirection::Upload);
                let tp_up = matches!(tp.direction, TransferDirection::Upload);
                if agg_up == tp_up {
                    agg.bytes_done += tp.bytes_done;
                    agg.total_bytes += tp.total_bytes;
                } else if tp_up {
                    *agg = tp.clone();
                }
            }
        }
    }
    by_path.into_values().collect()
}

/// True for a rendered [`backend::BackendReadError::Server`]: the server
/// answered with an HTTP error status. Not a transport failure, so never a
/// reason to go offline or to retry in the foreground.
fn is_server_error(e: &str) -> bool {
    backend::server_error_code(e).is_some()
}

fn is_transient_network_err(e: &str) -> bool {
    // A server that answered is not a transport failure, however its message
    // (which embeds the path) happens to read.
    if is_server_error(e) {
        return false;
    }
    e.starts_with("network:")
        // A listing body that broke off mid-stream: worth one retry.
        || e.starts_with(backend::TRUNCATED_PREFIX)
        || e.contains("connection reset")
        || e.contains("Connection reset")
        || e.contains("Connection refused")
        || e.contains("broken pipe")
        || e.contains("Broken pipe")
        // reqwest wraps all transport-layer send failures (dropped SYN, TLS failure,
        // connection dropped mid-request) as "error sending request" — these are always
        // transient and should return EAGAIN from FUSE (not EIO, which makes Nautilus
        // permanently mark the mount as inaccessible).
        || e.contains("error sending request")
        // Routing and reachability failures (EHOSTUNREACH / ENETUNREACH)
        || e.contains("No route to host")
        || e.contains("Network is unreachable")
        // HTTP/2 stream resets (server-side RST_STREAM during keepalive)
        || e.contains("stream error")
        || e.contains("connection closed before message completed")
}

fn is_timeout_err(e: &str) -> bool {
    if is_server_error(e) {
        return false;
    }
    // Match the shapes our own timeouts render as ("timeout", "PROPFIND timeout
    // for /x", "WebDAV download timeout", reqwest's "operation timed out") — not
    // the bare word, which also matches a directory called `timeout`.
    e == "timeout" || e.contains("timed out") || e.starts_with("PROPFIND timeout")
        || e.ends_with(" timeout")
}

/// Retries a single chunked-upload backend call (open/put-chunk/finish) a
/// bounded number of times on transient failures — network blip, 5xx/408/429,
/// or a momentary lock — with exponential backoff, mirroring the retry
/// already used for PROPFIND/downloads/range-reads. Streamed uploads have no
/// journal entry to fall back on (see `ChunkUploadState`'s doc comment), so
/// without this a single blip mid-copy aborted the whole file with EIO even
/// though the request would have succeeded moments later — this is what
/// surfaced to users as GVFS's "Error splicing file: Input/output error" on
/// large raw-photo copies, where more chunks means more chances to hit one.
pub(crate) fn retry_chunk_write<T>(
    op_name: &str,
    mut f: impl FnMut() -> Result<T, backend::BackendWriteError>,
) -> Result<T, backend::BackendWriteError> {
    const MAX_RETRIES: u32 = 2;
    let mut delay = Duration::from_millis(500);
    let mut attempt = 0u32;
    loop {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) if attempt < MAX_RETRIES && e.is_transient() => {
                log::warn!("{} failed (attempt {}/{}): {} — retrying in {:?}",
                    op_name, attempt + 1, MAX_RETRIES + 1, e, delay);
                thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_secs(4));
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// True when a read-path error string means we could not reach the server at all
/// (connect/read timeout or a transport-level failure) rather than an
/// application-level rejection (401/403/404) or a server that answered. Used to
/// flip the daemon offline eagerly so a burst of reads during a read-modify-write
/// save doesn't each block on a dead network before the connectivity monitor's
/// periodic probe notices. The monitor re-probes every 5s while offline and
/// clears the flag once the server is reachable again.
fn read_err_is_network_down(e: &str) -> bool {
    is_timeout_err(e) || is_transient_network_err(e)
}

/// How long a read will wait for connectivity to come back before giving up.
/// Sized to cover one full connectivity-monitor cycle while offline: the monitor
/// sleeps 5s, then spends up to a 5s probe timeout.
const OFFLINE_READ_GRACE: Duration = Duration::from_secs(15);

/// The error a read returns once it has waited out [`OFFLINE_READ_GRACE`].
///
/// The wording matters: [`error_to_errno`] classifies it, and "timed out" lands it
/// on the `ETIMEDOUT` arm. It must never fall through to `EIO`, which makes
/// Nautilus permanently mark the whole mount inaccessible (see the note on
/// [`is_transient_network_err`]) and tells apps the file is corrupt rather than
/// momentarily unavailable — that is what turned a brief outage into mpv's
/// "Failed to recognize file format".
const OFFLINE_READ_ERR: &str = "network: offline — timed out waiting for connectivity";

/// Flip the daemon offline, recording when, so reads can distinguish a fresh blip
/// from a sustained outage.
fn mark_offline(is_offline: &AtomicBool, since: &Mutex<Option<Instant>>) {
    if !is_offline.swap(true, Ordering::Relaxed) {
        *since.safe_lock() = Some(Instant::now());
        // The network — or its DNS view (VPN, split horizon) — may have changed:
        // re-resolve on the next request instead of trusting a 45 s-fresh answer.
        crate::http_clients::expire_fresh_dns();
    }
}

/// Clear the offline flag. Returns whether the daemon *was* offline.
fn mark_online(is_offline: &AtomicBool, since: &Mutex<Option<Instant>>) -> bool {
    let was_offline = is_offline.swap(false, Ordering::Relaxed);
    if was_offline {
        *since.safe_lock() = None;
    }
    was_offline
}

/// Block while the daemon is offline, but only for the remainder of
/// [`OFFLINE_READ_GRACE`] measured from the moment connectivity was lost.
///
/// A read that arrives during a two-second blip should produce data, not an error
/// — the connectivity monitor re-probes every 5s and will usually have cleared the
/// flag well inside the window. Anchoring the deadline to when we went offline
/// (rather than to when this read arrived) is what keeps the original fail-fast
/// behaviour for a real outage: once we have been offline past the grace period,
/// every subsequent read returns immediately instead of each stalling in turn,
/// which is what would wreck an app doing a read-modify-write save.
///
/// Returns true if the daemon is online by the time we stop waiting.
fn wait_out_offline_blip(conn: &ConnInfo) -> bool {
    if !conn.is_offline.load(Ordering::Relaxed) {
        return true;
    }
    let deadline = match *conn.offline_since.safe_lock() {
        Some(since) => since + OFFLINE_READ_GRACE,
        // Offline with no recorded transition (set before this bookkeeping existed,
        // e.g. an offline-mode mount): treat it as a real outage, not a blip.
        None => return false,
    };
    while Instant::now() < deadline {
        if conn.shutdown.load(Ordering::Relaxed) {
            return false;
        }
        thread::sleep(Duration::from_millis(250));
        if !conn.is_offline.load(Ordering::Relaxed) {
            return true;
        }
    }
    false
}

/// A read-only view of the daemon's connectivity, for a caller that renders
/// status (the GUI, and the IPC state word).
///
/// Shares the flag the connectivity monitor flips rather than mirroring it, so a
/// status can never lag behind the daemon.
#[derive(Clone)]
pub struct OfflineStatus {
    is_offline: Arc<AtomicBool>,
    since: Arc<Mutex<Option<Instant>>>,
}

impl Default for OfflineStatus {
    fn default() -> Self {
        OfflineStatus {
            is_offline: Arc::new(AtomicBool::new(false)),
            since: Arc::new(Mutex::new(None)),
        }
    }
}

impl OfflineStatus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the outage has lasted long enough to be worth reporting.
    ///
    /// The same blip tolerance [`wait_out_offline_blip`] gives a read, and for the
    /// same reason: a single failed request flips the flag eagerly, and the
    /// connectivity monitor usually clears it within one 5s cycle. A status that
    /// reacted instantly would flash a failure for every hiccup.
    pub fn settled(&self) -> bool {
        if !self.is_offline.load(Ordering::Relaxed) {
            return false;
        }
        match *self.since.safe_lock() {
            Some(since) => since.elapsed() >= OFFLINE_READ_GRACE,
            // Offline with no recorded transition (an offline-mode mount): a
            // standing state, not a blip.
            None => true,
        }
    }
}

#[cfg(test)]
impl OfflineStatus {
    /// Offline as of `ago` in the past.
    pub(crate) fn offline_for(ago: Duration) -> Self {
        let st = OfflineStatus::new();
        st.is_offline.store(true, Ordering::Relaxed);
        *st.since.safe_lock() = Instant::now().checked_sub(ago);
        st
    }
}

/// Return the minimal magic byte sequence that identifies a given MIME type.
///
/// # Background — GLib 2.80 MIME detection and FUSE latency
///
/// GLib 2.80 (shipped in Ubuntu 24.04, Fedora 40, and later) removed the
/// `user.xdg.mime.type` extended-attribute check it previously used for fast
/// MIME type resolution.  For any file whose extension is unrecognised or
/// ambiguous, GLib now falls back to *magic-byte detection*: it opens the file
/// and reads up to 16 KB from the start to inspect the binary signature.
///
/// On a local filesystem that read is a microsecond.  On a WebDAV-backed FUSE
/// mount each read triggers a full round-trip download from the remote server
/// (~280 ms over a typical home internet connection).  A directory with dozens
/// of files that have unusual extensions (e.g. Spanish tax forms with numeric
/// extensions like `.036`, `.190`, `.349`) therefore takes 10+ seconds to list
/// in Nautilus — one download per file, serialised by GLib's detection loop.
///
/// # Detection signal — O_NOATIME
///
/// GLib's magic-byte path always opens files with `O_NOATIME | O_NOFOLLOW |
/// O_CLOEXEC`.  Normal applications (text editors, media players, scripts)
/// never combine these flags on a read-only open.  This gives us a reliable
/// way to identify the opens without tracking per-process intent.
///
/// Note: the Linux kernel handles `O_NOFOLLOW` at the VFS layer (it fails the
/// open if the final path component is a symlink) and strips the flag before
/// forwarding the request to the FUSE driver.  `O_CLOEXEC` is always handled
/// by the kernel fd table and is never visible to FUSE.  So by the time the
/// open request arrives in our handler only `O_NOATIME` remains as the
/// distinguishing flag.
///
/// # Our fix
///
/// When `open()` sees a read-only, non-locally-cached file opened with
/// `O_NOATIME`, it marks the file handle with `mime_detect_ct` — the
/// content-type string already present in our in-memory dir cache (fetched
/// from the `{DAV:}getcontenttype` property during the previous PROPFIND).
/// If the server did not supply a content-type, we fall back to
/// `"application/octet-stream"` so the handle is always marked.
///
/// When `read()` is then called at offset 0 on such a handle, instead of
/// initiating any network I/O we return the few bytes from this table that
/// match the magic signature for the content-type.  GLib receives a valid
/// answer, classifies the file, and closes the handle — without us ever
/// touching the network.  The whole exchange takes microseconds instead of
/// hundreds of milliseconds.
///
/// # Guard against corrupting file copies
///
/// `O_NOATIME` is also set by copy tools (`cp`, Nautilus's `g_file_copy`) on
/// the source file to avoid updating its access time, so those opens also
/// trigger `mime_detect_ct` in `open()`.  Without a guard, `read()` would
/// serve magic bytes in place of real content, producing a tiny corrupted
/// destination file.
///
/// The distinguishing signal is the **read size**: GLib always requests
/// exactly 16384 bytes (`MAGIC_BYTES_BUFFER_SIZE` in `gcontenttype.c`)
/// regardless of the file's actual size.  Copy tools use much larger buffers
/// (`cp` uses 131072 bytes; GIO's `g_file_copy` uses 65536 bytes).
///
/// The catch is **kernel read-ahead**: GLib's single 16384-byte `read()` at
/// offset 0 is inflated by the kernel into a larger FUSE `read` request to
/// pre-fill the read-ahead window.  Measured on Linux 6.x this caps at exactly
/// 32768 bytes (one 8-page window) for the initial read of *any* file — even a
/// multi-GB one — because a magic-detection open reads once and closes, so the
/// sequential read-ahead ramp never grows past the first window.  A file larger
/// than 16 KiB therefore arrives here with `sz` in `(16384, 32768]`, so the old
/// `sz <= 16384` guard rejected it and every such file was fully downloaded
/// (~300 ms each) just to answer a MIME query — the regression this fixes.
///
/// `read()` intercepts when `off == 0 && sz <= GLIB_SNIFF_MAX_READ` (32768).
/// That covers the read-ahead-inflated magic read while staying safely below
/// the smallest copy buffer (GIO's 65536), so copies still fall through to the
/// real network-fetch path.  (In practice copy tools do not even set
/// `O_NOATIME` on the source, so `mime_detect_ct` is never set for them — the
/// size guard is defence in depth.)
///
/// # Forward-compatibility
///
/// Any content-type not listed below falls through to `b"# text\n"`, which
/// GLib recognises as `text/plain`.  This is an acceptable fallback: it
/// prevents the download and gives the file a usable icon.  GLib's
/// extension-based detection (which runs before magic-byte detection) already
/// handles the vast majority of common file types, so this function is only
/// reached for genuinely obscure extensions.
/// Upper bound on the FUSE `read` size that still counts as a GLib magic-byte
/// MIME-detection probe (see [`mime_magic_bytes`]).  GLib asks for 16384 bytes,
/// but kernel read-ahead inflates the initial read up to one 8-page window
/// (32768 bytes) regardless of file size; copy tools use ≥65536-byte buffers.

pub fn mime_magic_bytes(content_type: &str) -> &'static [u8] {
    let ct = content_type.split(';').next().unwrap_or(content_type).trim();
    match ct {
        "application/pdf"                   => b"%PDF-",
        "image/jpeg"                        => b"\xFF\xD8\xFF\xE0",
        "image/png"                         => b"\x89PNG\r\n\x1a\n",
        "image/gif"                         => b"GIF89a",
        // RIFF-container formats need the form-type at offset 8; bare "RIFF"
        // resolves to application/x-riff (and bare "BM" is short printable ASCII
        // that GLib reads as text) — both verified against Gio.content_type_guess.
        "image/webp"                        => b"RIFF\x00\x00\x00\x00WEBP",
        "image/bmp"                         => b"BM\x00\x00\x00\x00\x00\x00\x00\x00",
        "image/tiff"                        => b"II*\x00",
        // TIFF-based camera RAW. Nextcloud reports image/x-dcraw for raw formats
        // (.srw/.cr2/.nef/.arw/.dng/…), and the freedesktop MIME db often has no
        // glob for the extension (e.g. .srw), so without this arm GLib sniffs the
        // synthetic bytes, falls through to `# text\n`, and shows the file as
        // text/plain. Real raw files are TIFF containers, so returning TIFF magic
        // makes GLib classify them as image/tiff — an image type — which is enough
        // for Nautilus to show an image icon and pick up the server-side preview
        // the daemon prefetches into the XDG thumbnail cache. Big-endian magic
        // (MM) mirrors the actual on-disk header of these files.
        "image/x-dcraw"                     => b"MM\x00*",
        "application/zip"
        | "application/x-zip-compressed"
        | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        | "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        | "application/epub+zip"            => b"PK\x03\x04",
        "application/gzip"
        | "application/x-gzip"             => b"\x1f\x8b",
        "application/x-bzip2"              => b"BZh",
        "application/x-7z-compressed"      => b"7z\xbc\xaf'\x1c",
        "application/x-rar-compressed"
        | "application/vnd.rar"            => b"Rar!\x1a\x07",
        "application/ogg"
        | "audio/ogg"
        | "video/ogg"                      => b"OggS",
        // ISO-BMFF: the major brand at offset 8 is what disambiguates the subtype,
        // so a bare "ftyp" box resolves to application/octet-stream — every arm
        // below must carry a real brand (verified against Gio.content_type_guess).
        "video/mp4"                        => b"\x00\x00\x00\x18ftypisom\x00\x00\x02\x00",
        "audio/mp4"                        => b"\x00\x00\x00\x18ftypM4A \x00\x00\x00\x00",
        "video/quicktime"                  => b"\x00\x00\x00\x14ftypqt  \x00\x00\x00\x00",
        "image/heic"
        | "image/heif"                     => b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00",
        "audio/mpeg"                       => b"\xFF\xFB",
        "audio/flac"                       => b"fLaC",
        "audio/wav"                        => b"RIFF\x00\x00\x00\x00WAVE",
        "image/svg+xml"                    => b"<svg xmlns=\"http://www.w3.org/2000/svg\">",
        "image/x-icon"
        | "image/vnd.microsoft.icon"       => b"\x00\x00\x01\x00\x01\x00",
        "application/postscript"           => b"%!PS-Adobe-3.0",
        "application/x-deb"
        | "application/vnd.debian.binary-package" => b"!<arch>\ndebian-binary",
        "application/vnd.ms-excel"
        | "application/msword"
        | "application/vnd.ms-powerpoint" => b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1",
        // Text-based application/* subtypes: keep them classifiable as text so an
        // extension-unknown .json/.xml/… is editable text, not opaque binary.
        "application/json"
        | "application/xml"
        | "application/javascript"
        | "application/yaml"
        | "application/toml"
        | "application/x-tex"              => b"# text\n",
        // Category-aware fallback. The old code returned `# text\n` for EVERY
        // unmapped type, so any content-type absent from this table — and whose
        // extension the freedesktop MIME db doesn't know, forcing GLib to sniff —
        // was shown as editable text/plain (e.g. .srw before image/x-dcraw was
        // added). Dispatch on the top-level type instead so an unknown binary
        // never masquerades as text: images stay images (thumbnailable), media
        // stays media, and everything else becomes generic application/octet-stream
        // rather than text. Each branch's bytes are verified to resolve to the
        // intended category via Gio.content_type_guess.
        _ => match ct.split('/').next().unwrap_or("") {
            "image" => b"II*\x00",                          // → image/tiff
            "video" => b"\x00\x00\x00\x18ftypisom\x00\x00\x02\x00", // → video/mp4
            "audio" => b"\xFF\xFB",                          // → audio/mpeg
            "text"  => b"# text\n",                          // → text/plain
            _       => b"\x00\x00\x00\x00",                  // → application/octet-stream
        },
    }
}

fn error_to_errno(err: &str) -> Errno {
    // Typed backend errors first: their messages embed the path, so the
    // substring heuristics below would read a directory named `2404` as a 404.
    if let Some(code) = backend::server_error_code(err) {
        return match code {
            401 | 403 => Errno::EACCES,
            404 | 410 => Errno::ENOENT,
            507 => Errno::ENOSPC,
            // The server is struggling (or throttling us): "try again", never EIO,
            // which makes Nautilus mark the whole mount inaccessible.
            429 | 500..=599 => Errno::EAGAIN,
            _ => Errno::EIO,
        };
    }
    if err == "not found" {
        return Errno::ENOENT;
    }
    if err.starts_with("network:") || err.starts_with(backend::TRUNCATED_PREFIX) {
        // Includes OFFLINE_READ_ERR ("… timed out waiting for connectivity").
        return if is_timeout_err(err) { Errno::ETIMEDOUT } else { Errno::EAGAIN };
    }
    if err.contains("401") || err.contains("403") || err.contains("Unauthorized") || err.contains("Forbidden") {
        Errno::EACCES
    } else if err.contains("404") || err.contains("Not Found") {
        Errno::ENOENT
    } else if err.contains("No space left on device") || err.contains("os error 28") {
        // Return the real ENOSPC so callers stop retrying immediately; EIO would
        // cause thumbnail generators and media apps to spin in a 1-per-second loop.
        Errno::ENOSPC
    } else if is_timeout_err(err) {
        Errno::ETIMEDOUT
    } else if is_transient_network_err(err) {
        Errno::EAGAIN
    } else {
        Errno::EIO
    }
}

/// The daemon's load-shedding state, registered once per process so the IPC
/// server (which has no `ConnInfo`) can report it.
struct HealthSources {
    breaker: Arc<backoff::ServerBreaker>,
    backoff: Arc<backoff::PathBackoff>,
    walkers: Arc<walkers::WalkerTracker>,
}
static HEALTH: std::sync::OnceLock<HealthSources> = std::sync::OnceLock::new();

#[derive(Serialize)]
struct HealthReport {
    threads: usize,
    max_threads: usize,
    pools: Vec<bg::PoolStats>,
    breaker: Option<backoff::BreakerStats>,
    paths_backing_off: usize,
    walkers: Vec<walkers::WalkerStats>,
}

fn process_threads() -> usize {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("Threads:").map(|v| v.trim().parse().unwrap_or(0))))
        .unwrap_or(0)
}

fn health_report() -> HealthReport {
    let now = Instant::now();
    let h = HEALTH.get();
    HealthReport {
        threads: process_threads(),
        max_threads: bg::MAX_THREADS,
        pools: bg::stats(),
        breaker: h.map(|h| h.breaker.stats(now)),
        paths_backing_off: h.map_or(0, |h| h.backoff.len()),
        walkers: h.map(|h| h.walkers.active(now)).unwrap_or_default(),
    }
}

/// Applies the settings panel's walker limit to the running daemon (IPC
/// `WALKER_LIMIT`). False before a mount has registered its tracker.
pub fn set_walker_limit(enabled: bool, per_sec: u32) -> bool {
    match HEALTH.get() {
        Some(h) => {
            h.walkers.set_limit(enabled, per_sec);
            true
        }
        None => false,
    }
}

/// The running daemon's walker limit, as `on:<n>` / `off:<n>`.
pub fn walker_limit_status() -> Option<String> {
    HEALTH.get().map(|h| {
        let (on, n) = h.walkers.limit();
        format!("{}:{}", if on { "on" } else { "off" }, n)
    })
}

/// JSON for the IPC `HEALTH` command: threads, pools, breaker, walkers.
pub fn health_json() -> String {
    serde_json::to_string(&health_report()).unwrap_or_else(|_| "{}".to_string())
}

/// Logs one HEALTH line a minute — at INFO whenever something is worth
/// knowing (refused jobs, an open breaker, a walker, threads near the cap),
/// else at DEBUG. The 0.1.76 incident left nothing in the journal that said
/// which threads were piling up; this line would have.
fn health_log_loop(shutdown: Arc<AtomicBool>) {
    let mut last_rejected: u64 = 0;
    while !shutdown.load(Ordering::Relaxed) {
        for _ in 0..60 {
            if shutdown.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_secs(1));
        }
        let r = health_report();
        let rejected: u64 = r.pools.iter().map(|p| p.rejected).sum();
        let busy: Vec<String> = r.pools.iter()
            .filter(|p| p.active > 0 || p.queued > 0)
            .map(|p| format!("{} {}/{}+{}q", p.name, p.active, p.max_workers, p.queued))
            .collect();
        let walkers: Vec<String> = r.walkers.iter()
            .map(|w| format!("{} {}/min", w.chain, w.uncached_last_min))
            .collect();
        let open = r.breaker.as_ref().is_some_and(|b| b.open);
        let line = format!(
            "HEALTH threads={}/{} pools=[{}] refused+{} breaker={} backing_off={} walkers=[{}]",
            r.threads, r.max_threads, busy.join(", "), rejected - last_rejected.min(rejected),
            if open { "open" } else { "closed" }, r.paths_backing_off, walkers.join("; "),
        );
        let notable = rejected > last_rejected || open || !r.walkers.is_empty() || r.threads * 4 > r.max_threads * 3;
        if notable {
            log::info!("{}", line);
        } else {
            log::debug!("{}", line);
        }
        last_rejected = rejected;
    }
}

/// Starts one of the daemon's fixed, named long-lived threads.
fn start_service(name: &str, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = bg::spawn_service(name, f) {
        log::error!("could not start the {} thread: {}", name, e);
    }
}

/// Errno for "the parent listing is unavailable" after a failed re-list: only
/// the server saying the parent is gone (404) justifies ENOENT for the child.
fn relist_errno(err: Option<&str>) -> Errno {
    match err {
        Some(e) => error_to_errno(e),
        None => Errno::ENOENT,
    }
}

/// Queues a server mutation. Its `PathSeq` ticket was taken on the FUSE thread
/// before this call, which is what keeps the FIFO pool deadlock-free. The
/// queue is unbounded, so this only fails if the OS refuses a thread; the
/// operation is then still in the journal and replays on the next reconnect.
fn submit_mutation(job: impl FnOnce() + Send + 'static) {
    if let Err(r) = bg::MUTATION.submit(job) {
        log::error!("{} — mutation left in the journal for replay", r);
    }
}

/// Sends a kernel cache notification from the single notify worker, never the
/// FUSE dispatch thread (a notify the kernel blocks on would deadlock it).
pub(crate) fn notify_later(job: impl FnOnce() + Send + 'static) {
    if let Err(r) = bg::NOTIFY.submit(job) {
        log::warn!("{} — kernel cache notification dropped; the entry stays cached until it times out", r);
    }
}

/// How long a read parked on a read-ahead window waits with the window making no
/// progress. It must outlast what the window's pump can legitimately spend
/// recovering before bytes flow again: a full stall (READ_STALL_TIMEOUT), opening a
/// resume on another connection (RANGE_OPEN_BUDGET), plus slack. A fixed 30 s from
/// parking — the old rule — failed readers whose window was mid-resume.
const WINDOW_WAIT_IDLE: Duration = Duration::from_secs(
    READ_STALL_TIMEOUT.as_secs() + RANGE_OPEN_BUDGET.as_secs() + 15,
);
/// Longest a read waits on a window at all, progress or not.
const WINDOW_WAIT_MAX: Duration = Duration::from_secs(120);

/// Counts a READ as parked on a window for as long as it waits (see
/// `StreamState::waiters`), however the wait ends.
struct Parked<'a>(&'a Mutex<StreamState>);

impl<'a> Parked<'a> {
    fn new(m: &'a Mutex<StreamState>) -> Self {
        m.lock().unwrap().waiters += 1;
        Parked(m)
    }
}

impl Drop for Parked<'_> {
    fn drop(&mut self) {
        let mut ss = self.0.lock().unwrap_or_else(|e| e.into_inner());
        ss.waiters = ss.waiters.saturating_sub(1);
    }
}

/// Answers a READ for `off..off+sz` from the window at `start` once its bytes are
/// in, waiting (off the FUSE thread) while they arrive.
///
/// A short reply latches EOF on the inode, so the window's end is only ever served
/// short when it provably is the file's end: the prefix reached the end the server
/// stated (`at_eof`), or the dir-cache size says so (`short_reply_ok`) — never when
/// the window halted with a hole, and never on an unknown size. A window that broke
/// is EIO (the transport gave up); one that stopped, or has not delivered in time,
/// is EAGAIN, since trying again can work.
fn wait_on_window(reply: ReplyData, shared: &Arc<(Mutex<StreamState>, Condvar)>, start: u64, off: u64, sz: usize, file_size: u64) {
    let (ref mtx, ref cv) = **shared;
    // Declared before the lock guard, so it drops (and takes the lock) after it.
    let _parked = Parked::new(mtx);
    let mut guard = mtx.lock().unwrap();
    let parked = Instant::now();
    let mut last_len = guard.data.len();
    let mut idle_deadline = parked + WINDOW_WAIT_IDLE;
    loop {
        let o = (off - start) as usize;
        if start + guard.data.len() as u64 >= off + sz as u64 {
            reply.data(&guard.data[o..o + sz]);
            return;
        }
        if guard.done || guard.halted() {
            let avail = guard.data.len().saturating_sub(o);
            if !guard.halted() && (guard.at_eof() || short_reply_ok(off, avail, sz, file_size)) {
                let e = guard.data.len().min(o.saturating_add(sz));
                reply.data(if o < guard.data.len() { &guard.data[o..e] } else { &[] });
                return;
            }
            // The window promised these bytes and then stopped early. Replying with
            // what did arrive tells the kernel the file ends here and truncates it
            // for every handle, so fail the read instead.
            let errno = if guard.broken { Errno::EIO } else { Errno::EAGAIN };
            log::warn!(
                "read-ahead window ended {} bytes short of offset {} (broken={}, stopped={}) — {:?}, not EOF",
                sz.saturating_sub(avail), off, guard.broken, guard.stopped, errno,
            );
            reply.error(errno);
            return;
        }
        if guard.data.len() > last_len {
            last_len = guard.data.len();
            idle_deadline = Instant::now() + WINDOW_WAIT_IDLE;
        }
        let deadline = idle_deadline.min(parked + WINDOW_WAIT_MAX);
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            log::warn!("read wait timeout at offset {} after {:?} — EAGAIN", off, parked.elapsed());
            reply.error(Errno::EAGAIN);
            return;
        }
        guard = cv.wait_timeout(guard, remaining).unwrap().0;
    }
}

/// Runs a read that has to wait for bytes off the FUSE dispatch thread. When
/// the read pool is full the kernel gets EAGAIN rather than an unbounded thread.
///
/// A job that only starts after sitting in the queue for READ_QUEUE_MAX_WAIT is
/// answered EAGAIN instead of run: every job ahead of it is itself bounded, so
/// this caps how long any queued READ can go unanswered.
fn run_read_job(reply: ReplyData, job: impl FnOnce(ReplyData) + Send + 'static) {
    let queued_at = Instant::now();
    let job = move |reply: ReplyData| {
        let waited = queued_at.elapsed();
        if waited > READ_QUEUE_MAX_WAIT {
            log::warn!("read job waited {:?} in the queue — answering EAGAIN", waited);
            reply.error(Errno::EAGAIN);
            return;
        }
        job(reply)
    };
    if let Err((_, reply)) = bg::READ.submit_owning(reply, job) {
        reply.error(Errno::EAGAIN);
    }
}

/// Lists `path` from the server, retrying transport failures. Runs on the
/// caller's thread: every caller is already a background worker, and each
/// attempt is bounded by `PROPFIND_TIMEOUT`, so the thread this used to spawn
/// (and orphan when its outer deadline fired) bought nothing but a leak.
///
/// A 5xx is not retried here — the server answered, and asking again at once
/// is how a struggling server stays down. The permit is taken per attempt so
/// the backoff sleep never holds a request slot.
fn list_dir_propfind(
    conn: &Arc<ConnInfo>,
    path: PathBuf,
) -> Result<(Option<String>, Option<RemoteEntry>, Vec<RemoteEntry>), String> {
    log::debug!("LIST {}", path.display());
    const MAX_RETRIES: u32 = 2;
    let mut delay = Duration::from_millis(500);
    let mut attempt = 0u32;
    loop {
        let result = match conn.throttle.acquire_timeout(PROPFIND_TIMEOUT) {
            Some(_permit) => conn.backend.list_dir(&path, PROPFIND_TIMEOUT).map_err(|e| e.to_string()),
            None => Err(format!("PROPFIND timeout for {} (no request slot)", path.display())),
        };
        match result {
            Err(e) if attempt < MAX_RETRIES && (is_transient_network_err(&e) || is_timeout_err(&e)) => {
                log::warn!("PROPFIND {} failed (attempt {}/{}): {} — retrying in {:?}",
                    path.display(), attempt + 1, MAX_RETRIES + 1, e, delay);
                thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_secs(8));
                attempt += 1;
            }
            other => return other,
        }
    }
}

struct ProgressWriter {
    inner: std::fs::File,
    key: TransferKey,
    transfer_map: TransferMap,
    written: u64,
}

impl std::io::Write for ProgressWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        if let Ok(mut map) = self.transfer_map.lock() {
            if let Some(entry) = map.get_mut(&self.key) {
                entry.bytes_done = self.written;
            }
        }
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn open_file_timeout(
    conn: &Arc<ConnInfo>,
    path: PathBuf,
    dest: std::fs::File,
    transfers: Option<TransferMap>,
) -> Result<(), String> {
    open_file_within(conn, path, dest, transfers, None)
}

/// [`open_file_timeout`], with the slot wait and the download both cut short to
/// fit `deadline` when one is given.
fn open_file_within(
    conn: &Arc<ConnInfo>,
    path: PathBuf,
    dest: std::fs::File,
    transfers: Option<TransferMap>,
    deadline: Option<Instant>,
) -> Result<(), String> {
    log::info!("DOWNLOAD {}", path.display());
    let left = |cap: Duration| deadline.map_or(cap, |d| cap.min(d.saturating_duration_since(Instant::now())));
    // Runs on the caller's thread: the request carries its own DOWNLOAD_TIMEOUT,
    // so a helper thread only added one that outlived its caller's deadline.
    let Some(_permit) = conn.read_throttle.acquire_timeout(left(DOWNLOAD_SLOT_WAIT)) else {
        return Err("WebDAV download timeout (no download slot)".into());
    };
    let timeout = left(DOWNLOAD_TIMEOUT);
    if timeout.is_zero() {
        return Err("WebDAV download timeout (no time left)".into());
    }
    let mut writer: Box<dyn std::io::Write + Send> = if let Some(tm) = transfers {
        Box::new(ProgressWriter { inner: dest, key: whole_file_transfer(&path), transfer_map: tm, written: 0 })
    } else {
        Box::new(dest)
    };
    conn.backend.download_file(&path, &mut *writer, timeout, _permit.slot())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

// ── Cache layer ───────────────────────────────────────────────────────────────

pub(crate) struct FsCache {
    inodes: HashMap<u64, PathBuf>,
    paths: HashMap<PathBuf, u64>,
    next_inode: u64,
    dir_cache: HashMap<PathBuf, DirCacheEntry>,
    /// LRU ceiling on `dir_cache`; 0 means unbounded.
    dir_cache_max_dirs: usize,
    pending_dirs: HashMap<PathBuf, PendingDir>,
    pub(crate) file_cache: HashMap<PathBuf, FileCacheEntry>,
    cache_dir: PathBuf,
    kept_dir: PathBuf,
    auto_cache_dir: PathBuf,
    pub(crate) pending_notify: Arc<(Mutex<()>, Condvar)>,
    // Full paths of files whose PUT is in flight. put_dir_cache preserves
    // these entries so a concurrent PROPFIND refresh doesn't evict them
    // before the upload completes, which would cause ENOENT on stat().
    pub(crate) uploading: HashSet<PathBuf>,
    // Full paths of files whose DELETE is in flight. put_dir_cache filters
    // these out so a racing PROPFIND refresh can't re-add them before the
    // server DELETE completes.
    pub(crate) deleting: HashSet<PathBuf>,
    // Paths a listing found to be files, not directories (the PROPFIND's self
    // entry had no collection type). put_dir_cache records them instead of
    // caching an empty "directory", and get_or_list_dir turns that into
    // NOT_A_DIRECTORY_ERR for whoever asked it to list a file.
    pub(crate) not_dirs: HashSet<PathBuf>,
    // Set once the user unlinks the synthetic `.trackerignore` overlay entry
    // (see trackerignore_entry()). While set, put_dir_cache stops re-adding it
    // to the root listing — mirrors the old real-file semantics ("stays opted
    // out only until the next mount") without ever touching the backend.
    trackerignore_hidden: bool,
}

impl FsCache {
    fn storage_totals(&self) -> (u64, u64) {
        let mut kept = 0u64;
        let mut cached = 0u64;
        for e in self.file_cache.values() {
            if e.kept { kept += e.size; } else { cached += e.size; }
        }
        (kept, cached)
    }

    fn get_path(&self, inode: u64) -> Option<PathBuf> {
        self.inodes.get(&inode).cloned()
    }

    fn get_inode(&self, path: &Path) -> Option<u64> {
        self.paths.get(path).copied()
    }

    fn allocate_inode(&mut self, path: PathBuf) -> u64 {
        if let Some(ino) = self.paths.get(&path) {
            return *ino;
        }
        let ino = self.next_inode;
        self.next_inode += 1;
        self.paths.insert(path.clone(), ino);
        self.inodes.insert(ino, path);
        ino
    }

    fn get_cached_dir(&mut self, path: &Path, ttl: Duration, max_stale: Option<Duration>) -> Option<(Arc<Vec<RemoteEntry>>, bool)> {
        let tick = next_access_tick();
        let entry = self.dir_cache.get_mut(path)?;
        entry.last_access.store(tick, Ordering::Relaxed);
        if entry.invalidated || entry.hard_expired {
            return None;
        }
        if let Some(t) = entry.expiry_retry_after {
            if t > Instant::now() {
                return Some((Arc::clone(&entry.files), false));
            }
            entry.expiry_retry_after = None;
        }
        if let Some(max) = max_stale {
            // elapsed() errors when fetched_at is in the future, which happens when
            // the clock is stepped backwards after the cache was written (NTP fixing
            // a bad RTC, a restored image). Treat that as maximally old: the listing's
            // real age is unknown, and trusting it is the failure this guards against.
            let age = entry.fetched_at.elapsed().unwrap_or(Duration::MAX);
            if age >= max {
                entry.hard_expired = true;
                log::info!("DIR_HARD_EXPIRED {} — cached listing is {}s old (max {}s), re-listing before serving",
                    path.display(), age.as_secs(), max.as_secs());
                return None;
            }
        }
        let stale = entry.at.elapsed() >= ttl;
        let needs_refresh = stale && !entry.refreshing;
        if needs_refresh {
            entry.refreshing = true;
        }
        Some((Arc::clone(&entry.files), needs_refresh))
    }

    fn get_cached_dir_readonly(&self, path: &Path) -> Option<Arc<Vec<RemoteEntry>>> {
        self.dir_cache.get(path).map(|e| {
            e.last_access.store(next_access_tick(), Ordering::Relaxed);
            Arc::clone(&e.files)
        })
    }

    /// Returns the NC oc:permissions string for a directory by looking it up in its
    /// parent's cached listing.  Returns None if the entry is not yet cached (in
    /// which case the caller should allow the operation and let the server enforce).
    pub(crate) fn nc_dir_perms(&self, dir_inode: u64) -> Option<String> {
        let dir_path = self.get_path(dir_inode)?;
        let parent = dir_path.parent()?.to_path_buf();
        self.get_cached_dir_readonly(&parent)?
            .iter()
            .find(|e| e.path == dir_path)
            .and_then(|e| e.ext.str("permissions").map(str::to_string))
    }

    fn cached_dir_etag(&self, path: &Path) -> Option<String> {
        self.dir_cache.get(path)?.etag.clone()
    }

    fn put_dir_cache(&mut self, path: PathBuf, etag: Option<String>, self_entry: Option<RemoteEntry>, mut files: Vec<RemoteEntry>) {
        // A PROPFIND of a *file* answers 207 with just the file itself, which
        // reads as an empty listing. Cached, it would make the file an empty
        // directory: getattr answers make_dir_attr for it once its parent
        // listing is gone, and KEEP/PREFETCH find nothing to fetch in it.
        if self_entry.as_ref().is_some_and(|se| !se.is_dir) {
            log::info!("{} is a file, not a directory — not caching its listing", path.display());
            self.dir_cache.remove(&path);
            self.not_dirs.insert(path);
            return;
        }
        self.not_dirs.remove(&path);
        // Re-merge any in-flight uploads missing from the server listing so
        // that concurrent PROPFIND refreshes don't produce ENOENT on stat().
        if let Some(old) = self.dir_cache.get(&path) {
            for old_entry in old.files.iter() {
                if self.uploading.contains(&old_entry.path)
                    && !files.iter().any(|f| f.path == old_entry.path)
                {
                    files.push(old_entry.clone());
                }
            }
        }
        // Filter out files whose DELETE is still in flight: a racing PROPFIND
        // that completes before the server DELETE must not re-surface them.
        files.retain(|f| !self.deleting.contains(&f.path));
        // Overlay the synthetic `.trackerignore` marker onto every fresh root
        // listing (see trackerignore_entry()). Purely local — the server never
        // sees this entry — so it survives PROPFIND refreshes for free instead
        // of depending on a real write that a stale/evicted listing could lose.
        // Only while a profile with the Tracker component is enabled.
        if path == Path::new("/") && !self.trackerignore_hidden
            && desktop::policy().tracker_ignore
            && !files.iter().any(|f| f.path == trackerignore_path())
        {
            files.push(trackerignore_entry());
        }
        self.dir_cache.insert(path, DirCacheEntry {
            files: Arc::new(files), self_entry, etag,
            at: Instant::now(), fetched_at: SystemTime::now(),
            refreshing: false, invalidated: false, hard_expired: false,
            expiry_retry_after: None,
            last_access: AtomicU64::new(next_access_tick()),
        });
        self.evict_dir_cache();
    }

    /// Drops least-recently-used directory listings once the cache exceeds
    /// `dir_cache_max_dirs`.
    ///
    /// The dir cache had no bound at all: every directory anything ever listed
    /// stayed resident for the life of the process. On a large account that meant
    /// 26k directories / 398k entries — ~150 MB of listings, most of them a source
    /// tree some indexer walked once and will never look at again — plus a 12 s
    /// startup spent parsing them back off disk.
    ///
    /// Eviction is by last *access*, not last fetch: `fetched_at` is refreshed by
    /// cheap staleness probes, so it says nothing about whether anyone is using
    /// the listing. Evicting is always safe — the next readdir re-lists.
    fn evict_dir_cache(&mut self) {
        let max = self.dir_cache_max_dirs;
        if max == 0 || self.dir_cache.len() <= max {
            return;
        }
        // Evict down to 90% so a cache sitting at the ceiling doesn't re-sort on
        // every single insert.
        let target = max - max / 10;
        // Parents of files whose PUT is still in flight. Such an entry exists
        // *only* in its parent's cached listing — the server does not have the
        // file yet — so dropping that listing makes a file the user just created
        // vanish until the upload finishes. `put_dir_cache` re-merges uploads
        // from the old entry, which is exactly what eviction would destroy.
        let upload_parents: HashSet<&Path> = self.uploading.iter()
            .filter_map(|p| p.parent())
            .collect();
        let mut candidates: Vec<(u64, PathBuf)> = self.dir_cache.iter()
            // Never evict a listing mid-refresh: its `refreshing` flag is the
            // interlock stopping a second concurrent PROPFIND for the same dir.
            .filter(|(_, e)| !e.refreshing)
            .filter(|(p, _)| !upload_parents.contains(p.as_path()))
            .map(|(p, e)| (e.last_access.load(Ordering::Relaxed), p.clone()))
            .collect();
        candidates.sort_unstable_by_key(|(tick, _)| *tick);

        let to_drop = self.dir_cache.len().saturating_sub(target);
        let mut dropped = 0usize;
        for (_, path) in candidates.into_iter().take(to_drop) {
            self.dir_cache.remove(&path);
            dropped += 1;
        }
        if dropped > 0 {
            log::info!(
                "DIR_CACHE evicted {} least-recently-used listings ({} of max {} remain)",
                dropped, self.dir_cache.len(), max
            );
        }
    }

    /// Patch a single file entry's size in its parent's dir cache, returning true
    /// if an entry was found and its size actually changed. Used by the read path
    /// to reconcile the getattr size with the bytes the server is actually serving
    /// after a server-side edit the dir listing hasn't picked up (see the caller).
    fn set_entry_size(&mut self, path: &Path, size: u64) -> bool {
        let parent = path.parent().unwrap_or(Path::new("/")).to_path_buf();
        if let Some(dir) = self.dir_cache.get_mut(&parent) {
            let mut files = (*dir.files).clone();
            if let Some(entry) = files.iter_mut().find(|e| e.path == path) {
                if entry.size == size {
                    return false;
                }
                entry.size = size;
                entry.modified = Some(SystemTime::now());
                dir.files = Arc::new(files);
                return true;
            }
        }
        false
    }

    fn touch_dir_cache(&mut self, path: &Path) {
        if let Some(entry) = self.dir_cache.get_mut(path) {
            entry.at = Instant::now();
            entry.fetched_at = SystemTime::now();
            entry.refreshing = false;
            entry.invalidated = false;
            entry.hard_expired = false;
        }
    }

    fn start_pending(&mut self, path: PathBuf, rx: mpsc::Receiver<RemoteEntry>, etag_rx: mpsc::Receiver<Result<Option<String>, String>>, self_rx: mpsc::Receiver<RemoteEntry>) {
        self.pending_dirs.insert(path, PendingDir {
            entries: Vec::new(),
            rx,
            etag_rx,
            self_rx,
            etag: None,
            self_entry: None,
            failed: None,
        });
    }

    fn start_pending_and_notify(&mut self, path: PathBuf, rx: mpsc::Receiver<RemoteEntry>, etag_rx: mpsc::Receiver<Result<Option<String>, String>>, self_rx: mpsc::Receiver<RemoteEntry>) -> Arc<(Mutex<()>, Condvar)> {
        self.start_pending(path, rx, etag_rx, self_rx);
        self.pending_notify.clone()
    }

    // Promote any pending streaming fetch to dir_cache, then return the subdir paths.
    // Used by background prefetch to discover the next wave of directories to pre-fetch.
    fn subdir_paths_for_prefetch(&mut self, path: &Path) -> Vec<PathBuf> {
        if self.pending_dirs.contains_key(path) {
            let _ = self.promote_pending(path);
        }
        self.dir_cache.get(path)
            .map(|e| e.files.iter()
                .filter(|f| f.is_dir)
                .filter_map(|f| f.path.file_name().and_then(|n| n.to_str()).map(|n| path.join(n)))
                .collect())
            .unwrap_or_default()
    }

    fn promote_pending(&mut self, path: &Path) -> Result<Option<RemoteEntry>, String> {
        if let Some(mut pending) = self.pending_dirs.remove(path) {
            while let Ok(entry) = pending.rx.try_recv() {
                pending.entries.push(entry);
            }
            match pending.etag_rx.try_recv() {
                Ok(Ok(etag)) => pending.etag = etag,
                Ok(Err(e)) => pending.failed = Some(e),
                Err(_) => {}
            }
            if let Some(e) = pending.failed {
                self.pending_notify.1.notify_all();
                return Err(e);
            }
            if pending.self_entry.is_none() {
                if let Ok(se) = pending.self_rx.try_recv() {
                    pending.self_entry = Some(se);
                }
            }
            let self_entry = pending.self_entry.take();
            self.put_dir_cache(path.to_path_buf(), pending.etag, self_entry.clone(), pending.entries);
            self.pending_notify.1.notify_all();
            Ok(self_entry)
        } else {
            Ok(None)
        }
    }

    fn get_pending_snapshot(&mut self, path: &Path) -> Result<Option<Vec<RemoteEntry>>, String> {
        if !self.pending_dirs.contains_key(path) {
            return Ok(None);
        }
        {
            let pending = self.pending_dirs.get_mut(path).unwrap();
            if pending.self_entry.is_none() {
                if let Ok(se) = pending.self_rx.try_recv() {
                    pending.self_entry = Some(se);
                }
            }
        }
        let mut got_new = false;
        loop {
            match self.pending_dirs.get_mut(path).unwrap().rx.try_recv() {
                Ok(entry) => {
                    self.pending_dirs.get_mut(path).unwrap().entries.push(entry);
                    got_new = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    return match self.promote_pending(path) {
                        Err(e) => Err(e),
                        Ok(_) => Ok(self.dir_cache.get(path).map(|e| e.files.to_vec())),
                    };
                }
            }
        }
        let has_entries = self.pending_dirs.get(path).map_or(false, |p| !p.entries.is_empty());
        if has_entries {
            if got_new {
                self.pending_notify.1.notify_all();
            }
            Ok(self.pending_dirs.get(path).map(|p| p.entries.clone()))
        } else {
            Ok(None)
        }
    }

    /// After a failed re-list of a hard-expired directory, drop the hard-expiry
    /// flag and hand back the stale listing: a network blip must not turn a cached
    /// directory into an EIO. The soft TTL still drives a background refresh.
    fn take_hard_expired_fallback(&mut self, path: &Path) -> Option<(Arc<Vec<RemoteEntry>>, Option<RemoteEntry>)> {
        let entry = self.dir_cache.get_mut(path)?;
        if !entry.hard_expired || entry.invalidated {
            return None;
        }
        entry.hard_expired = false;
        entry.expiry_retry_after = Some(Instant::now() + EXPIRY_RETRY_COOLDOWN);
        Some((Arc::clone(&entry.files), entry.self_entry.clone()))
    }

    /// Mark a listing whose etag was just confirmed unchanged as current: the
    /// cached data is provably up to date, so it must be served rather than forced
    /// through a re-list. Unlike `touch_dir_cache` this leaves the invalidation
    /// state alone — an invalidation that landed while the etag was being probed
    /// still wins. Returns false when that happened, i.e. the caller must re-list
    /// anyway.
    fn confirm_dir_fresh(&mut self, path: &Path) -> bool {
        match self.dir_cache.get_mut(path) {
            Some(entry) if !entry.invalidated => {
                entry.at = Instant::now();
                entry.fetched_at = SystemTime::now();
                entry.expiry_retry_after = None;
                // `refreshing` is deliberately left alone: it is a single-flight guard
                // owned by whichever thread set it, and validation confirms directories
                // it never started a refresh for.
                entry.hard_expired = false;
                true
            }
            _ => false,
        }
    }

    /// Mark a directory whose etag no longer matches as untrusted, so it is re-listed
    /// before being served even if the re-list that was supposed to follow fails.
    fn invalidate_dir(&mut self, path: &Path) {
        if let Some(entry) = self.dir_cache.get_mut(path) {
            entry.invalidated = true;
        }
    }

    /// A partial stream snapshot may only be served while the cached listing behind
    /// it is still trusted: readdir's continuation pages read that listing directly
    /// (see `readdir_common`), so a fresh prefix would be spliced onto a stale
    /// suffix at shifted offsets.
    fn may_serve_pending_snapshot(&self, path: &Path) -> bool {
        self.dir_cache.get(path).map_or(true, |e| !e.invalidated && !e.hard_expired)
    }

    fn clear_refreshing(&mut self, path: &Path) {
        if let Some(entry) = self.dir_cache.get_mut(path) {
            entry.refreshing = false;
        }
    }

    fn is_known_directory(&self, path: &Path) -> Option<bool> {
        let parent = path.parent()?;
        let name = path.file_name()?.to_str()?;
        let dc = self.dir_cache.get(parent)?;
        Some(dc.files.iter().any(|f| {
            f.path.file_name().and_then(|n| n.to_str()).unwrap_or("") == name && f.is_dir
        }))
    }

    fn remote_modified_for(&self, path: &Path) -> Option<SystemTime> {
        self.find_entry(path).and_then(|e| e.modified)
    }

    fn remote_etag_for(&self, path: &Path) -> Option<String> {
        self.find_entry(path).and_then(|e| e.change_token.clone())
    }

    /// True when the locally cached copy of `path` still matches the server
    /// version recorded in the directory cache, so a read may be served from it.
    /// Prefer the change_token (etag) — it changes on every server-side edit
    /// (e.g. a Nextcloud Office save); fall back to the remote mtime when either
    /// side lacks a token. When neither a token nor an mtime is available on both
    /// sides (offline, or a server without etags) assume fresh, so this never
    /// breaks offline reads. Serving a stale copy at the dir-cache's newer size is
    /// what makes a just-edited odt/xlsx look corrupt, so the read fast-paths gate
    /// on this before returning local bytes.
    fn file_cache_matches_remote(&self, path: &Path) -> bool {
        match self.file_cache.get(path) {
            None => false,
            Some(fc) => match (fc.etag.as_deref(), self.remote_etag_for(path).as_deref()) {
                (Some(cached), Some(current)) => cached == current,
                _ => match (fc.remote_modified, self.remote_modified_for(path)) {
                    (Some(cached), Some(current)) => cached == current,
                    _ => true,
                },
            },
        }
    }

    fn find_entry(&self, path: &Path) -> Option<&RemoteEntry> {
        let parent = path.parent().unwrap_or(Path::new("/"));
        let name = path.file_name()?.to_str()?;
        self.dir_cache
            .get(parent)?
            .files
            .iter()
            .find(|e| {
                e.path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    == name
            })
    }

}

// ── Dir cache persistence ────────────────────────────────────────────────────

const DIR_CACHE_FILE: &str = "dir_cache.json";

#[derive(Serialize, Deserialize)]
struct PersistedDirEntry {
    etag: Option<String>,
    self_entry: Option<RemoteEntry>,
    /// `Arc` so that saving can snapshot a listing with a refcount bump instead
    /// of deep-copying every entry, and loading can hand the same allocation
    /// straight to `DirCacheEntry`.
    files: Arc<Vec<RemoteEntry>>,
    /// Unix seconds when this listing was fetched, so its real age survives a
    /// restart and the max-stale check isn't fooled into treating it as fresh.
    #[serde(default)]
    fetched_at: Option<u64>,
}

use std::sync::atomic::AtomicU64;

/// Last time a change asked for a save, in unix millis.
static SAVE_DIRTY_AT: AtomicU64 = AtomicU64::new(0);
/// First unserved change of the current burst, in unix millis. Bounds how long
/// a continuous stream of listings can keep postponing the write.
static SAVE_FIRST_DIRTY_AT: AtomicU64 = AtomicU64::new(0);
/// Whether a saver thread is already armed.
static SAVE_ARMED: AtomicBool = AtomicBool::new(false);

const SAVE_DEBOUNCE: Duration = Duration::from_secs(5);
/// Cap on total deferral, so a busy tree still gets persisted.
const SAVE_MAX_DEFER: Duration = Duration::from_secs(60);

/// A toolkit atomic-write temp (GIO's `.goutputstream-*` / `.xdp-*` today),
/// per the active desktop profiles — see `desktop::toolkit::gio`.
fn is_gio_temp_file(name: &str) -> bool {
    desktop::policy().is_hidden_temp(name)
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Coalesces save requests: one saver thread waits until the cache has been
/// quiet for `SAVE_DEBOUNCE` and then writes once.
///
/// The previous version armed a fresh 5 s timer for every request more than 2 s
/// apart, so a trickle of listings produced a full write every couple of
/// seconds — two complete rewrites of the cache landed within seconds of a
/// startup where the user did nothing at all.
fn schedule_save_dir_cache(cache: &Arc<Mutex<FsCache>>) {
    let now = unix_millis();
    SAVE_DIRTY_AT.store(now, Ordering::Relaxed);
    if SAVE_ARMED.swap(true, Ordering::AcqRel) {
        return;
    }
    SAVE_FIRST_DIRTY_AT.store(now, Ordering::Relaxed);

    let cache = cache.clone();
    let submitted = bg::HOUSEKEEPING.submit(move || {
        loop {
            thread::sleep(SAVE_DEBOUNCE);
            let now = unix_millis();
            let quiet_for = now.saturating_sub(SAVE_DIRTY_AT.load(Ordering::Relaxed));
            let waited = now.saturating_sub(SAVE_FIRST_DIRTY_AT.load(Ordering::Relaxed));
            if quiet_for >= SAVE_DEBOUNCE.as_millis() as u64
                || waited >= SAVE_MAX_DEFER.as_millis() as u64
            {
                break;
            }
        }
        // Disarm before writing, so a change made *during* the write arms a new
        // saver rather than being dropped until the next unrelated change.
        SAVE_ARMED.store(false, Ordering::Release);
        save_dir_cache_now(&cache);
    });
    if submitted.is_err() {
        // Disarm so the next change retries; staying armed would stop saves for good.
        SAVE_ARMED.store(false, Ordering::Release);
    }
}

/// Serialises the dir cache as a map without materialising one: the snapshot is
/// a Vec of pairs, and `collect_map` streams it straight into the writer.
struct DirCacheSnapshot(Vec<(String, PersistedDirEntry)>);

impl Serialize for DirCacheSnapshot {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_map(self.0.iter().map(|(k, v)| (k, v)))
    }
}

fn save_dir_cache_now(cache: &Mutex<FsCache>) {
    // Snapshot under the lock *without* deep-copying any listing: `files` is an
    // Arc, so cloning it is a refcount bump. The previous version cloned every
    // RemoteEntry in the cache and then serialised into one contiguous Vec<u8>,
    // which on a large account meant a ~430 MB copy plus a ~180 MB buffer — RSS
    // spiked past 1 GB on every save, twice within seconds of startup.
    let (path, snapshot) = {
        let c = cache.safe_lock();
        let snapshot: Vec<(String, PersistedDirEntry)> = c.dir_cache.iter()
            .map(|(k, v)| {
                (k.to_string_lossy().into_owned(), PersistedDirEntry {
                    etag: v.etag.clone(),
                    self_entry: v.self_entry.clone(),
                    files: Arc::clone(&v.files),
                    fetched_at: v.fetched_at.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs()),
                })
            })
            .collect();
        (c.cache_dir.join(DIR_CACHE_FILE), snapshot)
    };

    let count = snapshot.len();
    // Stream into a sibling temp file and rename over the real one: the JSON is
    // never all resident, and a crash or a full disk mid-write leaves the
    // previous cache intact rather than a truncated file that fails to parse.
    let tmp = path.with_extension("json.tmp");
    let file = match std::fs::File::create(&tmp) {
        Ok(f) => f,
        Err(e) => {
            log::error!("DIR_CACHE create failed {}: {} — cache will be cold on restart", tmp.display(), e);
            return;
        }
    };
    let mut w = std::io::BufWriter::with_capacity(1 << 20, file);
    if let Err(e) = serde_json::to_writer(&mut w, &DirCacheSnapshot(snapshot)) {
        log::error!("DIR_CACHE serialize failed: {}", e);
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    match w.into_inner() {
        Ok(f) => {
            if let Err(e) = f.sync_all() {
                log::error!("DIR_CACHE sync failed {}: {}", tmp.display(), e);
                let _ = std::fs::remove_file(&tmp);
                return;
            }
        }
        Err(e) => {
            log::error!("DIR_CACHE flush failed {}: {}", tmp.display(), e);
            let _ = std::fs::remove_file(&tmp);
            return;
        }
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        log::error!("DIR_CACHE rename failed {}: {} — cache will be cold on restart", path.display(), e);
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    log::info!("DIR_CACHE saved {} dirs to {}", count, path.display());
}

fn load_dir_cache(cache: &Mutex<FsCache>) {
    let path = {
        let c = cache.safe_lock();
        c.cache_dir.join(DIR_CACHE_FILE)
    };
    // Read-then-parse rather than streaming from the file. Streaming would hold
    // ~180 MB less at the peak, but serde_json's reader path measured ~1.9x
    // slower than the slice path on a real 182 MB cache (21 s vs 11 s, same
    // build), and this parse is on the critical path to having the mount up.
    // The buffer is a few-second transient; the steady-state size is what
    // actually mattered, and that is fixed by the entry layout instead.
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(_) => return,
    };
    let map: HashMap<String, PersistedDirEntry> = match serde_json::from_slice(&data) {
        Ok(m) => m,
        Err(e) => {
            log::warn!("DIR_CACHE load failed: {}", e);
            return;
        }
    };
    drop(data);
    let mut c = cache.safe_lock();

    // Restore at most `dir_cache_max_dirs` listings, most recently fetched first.
    // Without this the whole persisted cache is parsed and inserted only for
    // eviction to throw most of it away moments later — on a large account that
    // was 26k listings and a 12-second startup before the mount came up.
    let mut entries: Vec<(String, PersistedDirEntry)> = map.into_iter().collect();
    let max = c.dir_cache_max_dirs;
    let total = entries.len();
    if max > 0 && total > max {
        // Newest fetch first. `fetched_at` is the only ordering the file carries;
        // access ticks do not survive a restart.
        entries.sort_unstable_by(|a, b| b.1.fetched_at.cmp(&a.1.fetched_at));
        entries.truncate(max);
        log::info!(
            "DIR_CACHE restoring the {} most recent of {} persisted listings (dir_cache_max_dirs)",
            max, total
        );
    }
    // Oldest fetch first, because the insert loop below stamps each entry with
    // the next access tick: restoring newest-first would hand the most recently
    // used listings the *lowest* ticks and make them the first ones evicted.
    entries.sort_unstable_by(|a, b| a.1.fetched_at.cmp(&b.1.fetched_at));

    let mut count = 0usize;
    for (k, v) in entries {
        let dir_path = PathBuf::from(&k);
        if c.dir_cache.contains_key(&dir_path) {
            continue;
        }
        // Deliberately no inode pre-allocation here. Assigning one to every
        // cached file cost two owned PathBufs each (`paths` and `inodes`) —
        // ~150 MB on a large account — for directories the user may never open,
        // and bought nothing: inode numbers are not stable across restarts
        // anyway (the counter restarts and HashMap iteration order is random).
        // `lookup` and the offset-0 leg of `readdir` already allocate on
        // demand, and every `get_inode` caller handles a miss.
        // Cache files written before dir_cache_max_stale_mins existed carry no
        // fetch time: treat them as ancient so the first listing re-lists rather
        // than trusting a listing of unknown age.
        let fetched_at = v.fetched_at
            .and_then(|secs| UNIX_EPOCH.checked_add(Duration::from_secs(secs)))
            .unwrap_or(UNIX_EPOCH);
        c.dir_cache.insert(dir_path, DirCacheEntry {
            last_access: AtomicU64::new(next_access_tick()),
            files: v.files,
            self_entry: v.self_entry,
            etag: v.etag,
            at: Instant::now(),
            fetched_at,
            refreshing: false,
            invalidated: false,
            hard_expired: false,
            expiry_retry_after: None,
        });
        count += 1;
    }
    log::info!("DIR_CACHE loaded {} dirs from {}", count, path.display());
}

// ── File cache persistence ───────────────────────────────────────────────────

const FILE_CACHE_FILE: &str = "file_cache.json";

#[derive(Serialize, Deserialize)]
struct PersistedFileEntry {
    etag: Option<String>,
    #[serde(default)]
    kept: bool,
    #[serde(default)]
    size: u64,
}

pub(crate) fn save_file_cache(cache: &Mutex<FsCache>) {
    let c = cache.safe_lock();
    let path = c.cache_dir.join(FILE_CACHE_FILE);
    let map: HashMap<String, PersistedFileEntry> = c.file_cache.iter()
        .filter_map(|(k, v)| {
            v.etag.as_ref()?;
            Some((k.to_string_lossy().into_owned(), PersistedFileEntry { etag: v.etag.clone(), kept: v.kept, size: v.size }))
        })
        .collect();
    drop(c);
    match serde_json::to_vec(&map) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                log::error!("FILE_CACHE write failed {}: {} — cached files won't survive restart", path.display(), e);
            } else {
                log::info!("FILE_CACHE saved {} entries to {}", map.len(), path.display());
            }
        }
        Err(e) => log::error!("FILE_CACHE serialize failed: {}", e),
    }
}

struct LoadedCacheEntry {
    etag: String,
    kept: bool,
    #[allow(dead_code)]
    size: u64,
}

fn load_file_cache(cache: &Mutex<FsCache>) -> HashMap<PathBuf, LoadedCacheEntry> {
    let path = {
        let c = cache.safe_lock();
        c.cache_dir.join(FILE_CACHE_FILE)
    };
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(_) => return HashMap::new(),
    };
    let map: HashMap<String, PersistedFileEntry> = match serde_json::from_slice(&data) {
        Ok(m) => m,
        Err(e) => {
            log::warn!("FILE_CACHE load failed: {}", e);
            return HashMap::new();
        }
    };
    let c = cache.safe_lock();
    let mut result = HashMap::new();
    let mut migrated = 0usize;
    for (k, v) in map {
        let remote_path = PathBuf::from(&k);
        if let Some(etag) = v.etag {
            let rel = remote_path.strip_prefix("/").unwrap_or(&remote_path);
            let kept_path = c.kept_dir.join(rel);
            let cache_path = c.auto_cache_dir.join(rel);
            let legacy_path = c.cache_dir.join(rel);
            if let Ok(m) = kept_path.metadata() { if m.len() > 0 {
                let sz = if v.size > 0 { v.size } else { m.len() };
                result.insert(remote_path, LoadedCacheEntry { etag, kept: true, size: sz });
                continue;
            }}
            if let Ok(m) = cache_path.metadata() { if m.len() > 0 {
                let sz = if v.size > 0 { v.size } else { m.len() };
                result.insert(remote_path, LoadedCacheEntry { etag, kept: v.kept, size: sz });
                continue;
            }}
            if let Ok(lm) = legacy_path.metadata() { if lm.len() > 0 {
                let target = if v.kept { &c.kept_dir } else { &c.kept_dir };
                let new_path = target.join(rel);
                if let Some(parent) = new_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if std::fs::rename(&legacy_path, &new_path).is_ok() {
                    migrated += 1;
                    let sz = if v.size > 0 { v.size } else { lm.len() };
                    result.insert(remote_path, LoadedCacheEntry { etag, kept: true, size: sz });
                }
            }}
        }
    }
    if migrated > 0 {
        log::info!("FILE_CACHE migrated {} legacy files to kept/", migrated);
    }
    log::info!("FILE_CACHE loaded {} entries", result.len());
    result
}

// ── Cache cleanup ───────────────────────────────────────────────────────────

fn run_cache_cleanup(
    cache: &Arc<Mutex<FsCache>>,
    status: &StatusMap,
    dirty: &ipc::DirtySet,
    max_bytes: u64,
    purge_days: u32,
) {
    let auto_cache_dir = cache.safe_lock().auto_cache_dir.clone();

    struct CachedFile {
        path: PathBuf,
        remote_path: PathBuf,
        size: u64,
        accessed: SystemTime,
    }

    let mut files: Vec<CachedFile> = Vec::new();
    let mut total_size: u64 = 0;

    fn walk_dir(dir: &Path, base: &Path, files: &mut Vec<CachedFile>, total: &mut u64) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk_dir(&path, base, files, total);
            } else if let Ok(meta) = entry.metadata() {
                let size = meta.len();
                let accessed = meta.accessed().unwrap_or(meta.modified().unwrap_or(SystemTime::UNIX_EPOCH));
                let rel = path.strip_prefix(base).unwrap_or(&path);
                let remote_path = PathBuf::from("/").join(rel);
                *total += size;
                files.push(CachedFile { path, remote_path, size, accessed });
            }
        }
    }

    walk_dir(&auto_cache_dir, &auto_cache_dir, &mut files, &mut total_size);

    if files.is_empty() {
        return;
    }

    let mut evicted = 0usize;
    let mut freed: u64 = 0;

    if purge_days > 0 {
        let cutoff = SystemTime::now() - Duration::from_secs(purge_days as u64 * 86400);
        let mut i = 0;
        while i < files.len() {
            if files[i].accessed < cutoff {
                let f = files.swap_remove(i);
                if std::fs::remove_file(&f.path).is_ok() {
                    let mut c = cache.safe_lock();
                    c.file_cache.remove(&f.remote_path);
                    drop(c);
                    status.safe_write().insert(f.remote_path.clone(), FileStatus::Remote);
                    dirty.safe_lock().insert(f.remote_path);
                    total_size -= f.size;
                    freed += f.size;
                    evicted += 1;
                }
            } else {
                i += 1;
            }
        }
    }

    if max_bytes > 0 && total_size > max_bytes {
        files.sort_by_key(|f| f.accessed);
        for f in files {
            if total_size <= max_bytes { break; }
            if std::fs::remove_file(&f.path).is_ok() {
                let mut c = cache.safe_lock();
                c.file_cache.remove(&f.remote_path);
                drop(c);
                status.safe_write().insert(f.remote_path.clone(), FileStatus::Remote);
                dirty.safe_lock().insert(f.remote_path);
                total_size -= f.size;
                freed += f.size;
                evicted += 1;
            }
        }
    }

    if evicted > 0 {
        save_file_cache(cache);
        log::info!("CACHE_CLEANUP evicted {} files, freed {:.1}MB", evicted, freed as f64 / 1_048_576.0);
    }
}

// ── Shared operation helpers ──────────────────────────────────────────────────

struct DirDetailArcs {
    shared: ipc::SharedSet,
    fileids: ipc::FileIdMap,
    details: ipc::FileDetailMap,
    children: ipc::ChildrenMap,
    dirty: ipc::DirtySet,
}

fn apply_dir_detail_maps(path: &Path, entries: &[RemoteEntry], arcs: &DirDetailArcs) {
    let mut shared_paths = Vec::new();
    let mut fileid_paths: Vec<(PathBuf, u64)> = Vec::new();
    let mut detail_entries: Vec<(PathBuf, ipc::FileDetail)> = Vec::new();
    for entry in entries {
        let Some(name) = entry.path.file_name().and_then(|n| n.to_str()) else { continue };
        let ep = path.join(name);
        if entry.ext.flag("is_shared") { shared_paths.push(ep.clone()); }
        if let Some(fid) = entry.ext.int("fileid") { fileid_paths.push((ep.clone(), fid)); }
        detail_entries.push((ep, ipc::FileDetail {
            permissions: entry.ext.str("permissions").map(str::to_string),
            owner_id: entry.ext.str("owner_id").map(str::to_string),
            owner_display_name: entry.ext.str("owner_display_name").map(str::to_string),
            size: entry.size,
            is_dir: entry.is_dir,
        }));
    }
    { let mut sh = arcs.shared.safe_write(); for p in shared_paths { sh.insert(p); } }
    { let mut fi = arcs.fileids.safe_write(); for (p, fid) in fileid_paths { fi.insert(p, fid); } }
    {
        let mut dt = arcs.details.safe_write();
        let mut cm = arcs.children.safe_write();
        for (p, d) in detail_entries {
            if let Some(parent) = p.parent() {
                cm.entry(parent.to_path_buf())
                    .or_insert_with(std::collections::HashSet::new)
                    .insert(p.clone());
            }
            dt.insert(p, d);
        }
    }
    arcs.dirty.safe_lock().insert(path.to_path_buf());
}

fn get_or_list_dir(
    conn: &Arc<ConnInfo>,
    cache: &Arc<Mutex<FsCache>>,
    path: PathBuf,
    dir_maps: Option<DirDetailArcs>,
) -> Result<(Arc<Vec<RemoteEntry>>, Option<RemoteEntry>), String> {
    match list_dir_cached_or_fresh(conn, cache, path.clone(), dir_maps) {
        Ok(v) => Ok(v),
        Err(e) => {
            // Only a server we could not reach justifies falling back to a listing we
            // already decided is too old to trust. A 404/401 is an answer: surfacing
            // it beats rendering a directory that no longer exists.
            if is_unreachable_listing_error(&e) {
                if let Some((files, self_entry)) = cache.safe_lock().take_hard_expired_fallback(&path) {
                    log::warn!("HARD_EXPIRED_FALLBACK {}: {} — serving the stale listing", path.display(), e);
                    return Ok((files, self_entry));
                }
            }
            Err(e)
        }
    }
}

/// True when a failed listing means the server could not be reached (transport
/// failure or timeout, including our own "PROPFIND timeout" give-up) rather than a
/// server that answered with a rejection.
fn is_unreachable_listing_error(e: &str) -> bool {
    // A 5xx is an answer, but not one about the directory: a listing we already
    // have beats an error the walker can do nothing with.
    is_transient_network_err(e) || is_timeout_err(e)
        || backend::server_error_code(e).is_some_and(|c| c >= 500 || c == 429)
}

/// Feeds one listing outcome to the per-path backoff and the server breaker.
/// Only answers count: a transport failure says nothing about the server's
/// health and is the connectivity monitor's business.
fn note_listing_outcome(conn: &ConnInfo, path: &Path, outcome: Result<(), &str>) {
    let now = Instant::now();
    match outcome {
        Ok(()) => {
            conn.breaker.record(false, now);
            conn.backoff.clear(path);
        }
        Err(e) => match backend::server_error_code(e) {
            Some(code) if backoff::is_struggling(code) => {
                conn.breaker.record(true, now);
                let cooldown = conn.backoff.record_failure(path, code, now);
                log::info!("LIST_BACKOFF {} — server answered {}, not asking again for {:?}", path.display(), code, cooldown);
            }
            Some(_) => conn.breaker.record(false, now),
            // A body that broke off mid-listing is the classic sign of a server
            // timing out under load: count it against the server, not the path.
            None if e.starts_with(backend::TRUNCATED_PREFIX) => conn.breaker.record(true, now),
            None => {}
        },
    }
}

/// Background re-validation of a listing past its soft TTL: a cheap Depth-0
/// etag probe first, a full re-list only if the directory changed. Runs on a
/// `bg::LISTING` worker; the caller already served the cached listing.
fn soft_refresh_dir(conn: &Arc<ConnInfo>, cache: &Arc<Mutex<FsCache>>, path: PathBuf, dm: Option<DirDetailArcs>) {
    let old_etag = cache.safe_lock().cached_dir_etag(&path);
    if let Some(ref old) = old_etag {
        let probe = match conn.throttle.acquire_timeout(PROPFIND_TIMEOUT) {
            Some(_permit) => conn.backend.dir_change_token(&path, PROPFIND_TIMEOUT).map_err(|e| e.to_string()),
            None => Err("PROPFIND timeout (no request slot for the etag probe)".to_string()),
        };
        match probe {
            Ok(Some(ref new_etag)) if new_etag == old => {
                note_listing_outcome(conn, &path, Ok(()));
                log::debug!("ETAG_MATCH {} — skipping full re-list", path.display());
                cache.safe_lock().touch_dir_cache(&path);
                return;
            }
            Ok(_) => note_listing_outcome(conn, &path, Ok(())),
            Err(e) => {
                note_listing_outcome(conn, &path, Err(&e));
                log::debug!("etag check {}: {}", path.display(), e);
                if is_server_error(&e) {
                    // The server is failing this directory; a full listing would too.
                    cache.safe_lock().clear_refreshing(&path);
                    return;
                }
            }
        }
    }
    match list_dir_propfind(conn, path.clone()) {
        Ok((etag, self_entry, fresh)) => {
            note_listing_outcome(conn, &path, Ok(()));
            if let Some(ref arcs) = dm {
                apply_dir_detail_maps(&path, &fresh, arcs);
            }
            cache.safe_lock().put_dir_cache(path, etag, self_entry, fresh);
        }
        Err(e) => {
            note_listing_outcome(conn, &path, Err(&e));
            log::debug!("background refresh {}: {}", path.display(), e);
            cache.safe_lock().clear_refreshing(&path);
        }
    }
}

fn list_dir_cached_or_fresh(
    conn: &Arc<ConnInfo>,
    cache: &Arc<Mutex<FsCache>>,
    path: PathBuf,
    dir_maps: Option<DirDetailArcs>,
) -> Result<(Arc<Vec<RemoteEntry>>, Option<RemoteEntry>), String> {
    if conn.is_offline.load(Ordering::Relaxed) {
        {
            let c = cache.safe_lock();
            if let Some(entry) = c.dir_cache.get(&path) {
                return Ok((Arc::clone(&entry.files), entry.self_entry.clone()));
            }
        }
        // Not cached, so failing here renders the directory empty in a file
        // manager. The offline flag flips on a single failed probe (a QUIC-only
        // network path does this routinely before the HTTP/2 demotion kicks in),
        // and the connectivity monitor usually clears it within seconds — give
        // this listing the same blip grace a read gets instead of trusting a
        // flag that may already be stale. A sustained outage still fails fast:
        // past the grace window wait_out_offline_blip returns immediately.
        if !wait_out_offline_blip(conn) {
            return Err(format!("{} not available offline", path.display()));
        }
        // Back online — fall through to a real listing.
    }
    let t0 = Instant::now();
    let ttl = effective_dir_ttl(conn.optimistic_listing, &conn.notify_push_connected);
    let max_stale = effective_max_stale(conn.dir_cache_max_stale, &conn.notify_push_connected);
    {
        let mut c = cache.safe_lock();
        if let Some((files, needs_refresh)) = c.get_cached_dir(&path, ttl, max_stale) {
            let self_entry = c.dir_cache.get(&path).and_then(|e| e.self_entry.clone());
            log::info!("LIST_CACHED {} ({} entries, refresh={}) in {:?}", path.display(), files.len(), needs_refresh, t0.elapsed());
            if needs_refresh {
                let now = Instant::now();
                if conn.breaker.is_open(now) || conn.backoff.blocked(&path, now).is_some() {
                    // Serving what we have is the whole point of backing off.
                    c.clear_refreshing(&path);
                } else {
                    let submitted = {
                        let conn = conn.clone();
                        let cache = cache.clone();
                        let path = path.clone();
                        let dm = dir_maps;
                        bg::LISTING.submit(move || soft_refresh_dir(&conn, &cache, path, dm))
                    };
                    if submitted.is_err() {
                        c.clear_refreshing(&path);
                    }
                }
            }
            return Ok((files, self_entry));
        }
    }
    // This directory just failed on the server: don't ask again until its
    // cooldown passes. Any listing we hold — even one marked stale — beats an
    // error, and without one the caller gets a fast "try again" (EAGAIN).
    if let Some((left, code)) = conn.backoff.blocked(&path, Instant::now()) {
        let c = cache.safe_lock();
        if let Some(entry) = c.dir_cache.get(&path) {
            log::debug!("LIST_BACKOFF_STALE {} — serving the cached listing for {:?} more", path.display(), left);
            return Ok((Arc::clone(&entry.files), entry.self_entry.clone()));
        }
        return Err(format!("{}{}: {} (cooling down {:?} after a server error)",
            backend::SERVER_ERROR_PREFIX, code, path.display(), left));
    }

    // The listing is past dir_cache_max_stale_mins, so it may not be served before
    // we know whether it is still current. Ask for the directory etag first: that is
    // a Depth-0 PROPFIND whose cost is independent of how many entries the directory
    // holds, so the common "nothing changed" case costs one small round trip instead
    // of a full re-list. Only a mismatch (or a failed probe) falls through to one.
    let expired_etag = {
        let mut c = cache.safe_lock();
        match c.dir_cache.get_mut(&path) {
            // `refreshing` doubles as the single-prober guard: a second reader racing
            // on the same directory goes straight to the full listing and joins the
            // in-flight PROPFIND there rather than issuing a duplicate probe.
            Some(e) if e.hard_expired && !e.invalidated && !e.refreshing => {
                let etag = e.etag.clone();
                if etag.is_some() {
                    e.refreshing = true;
                }
                etag
            }
            _ => None,
        }
    };
    if let Some(old_etag) = expired_etag {
        // Reached from lookup/getattr/readdir on the FUSE dispatch thread.
        let probe = match conn.throttle.acquire_timeout(PROPFIND_TIMEOUT) {
            Some(_permit) => conn.backend.dir_change_token(&path, PROPFIND_TIMEOUT),
            None => Err(backend::BackendReadError::Timeout),
        };
        match &probe {
            Ok(_) => note_listing_outcome(conn, &path, Ok(())),
            Err(e) => note_listing_outcome(conn, &path, Err(&e.to_string())),
        }
        let mut c = cache.safe_lock();
        match probe {
            Ok(Some(ref new_etag)) if *new_etag == old_etag => {
                let confirmed = c.confirm_dir_fresh(&path);
                c.clear_refreshing(&path);
                if confirmed {
                    if let Some(entry) = c.dir_cache.get(&path) {
                        log::info!("LIST_ETAG_CONFIRMED {} ({} entries) in {:?}", path.display(), entry.files.len(), t0.elapsed());
                        return Ok((Arc::clone(&entry.files), entry.self_entry.clone()));
                    }
                }
                // Invalidated while the probe was in flight — re-list after all.
            }
            Ok(_) => {
                log::info!("LIST_ETAG_CHANGED {} — re-listing before serving", path.display());
                c.clear_refreshing(&path);
            }
            Err(e) => {
                log::debug!("expiry etag check {}: {}", path.display(), e);
                c.clear_refreshing(&path);
            }
        }
    }

    // Check if there's already an in-progress incremental fetch
    {
        let mut c = cache.safe_lock();
        match c.get_pending_snapshot(&path) {
            Err(e) => return Err(e),
            // The freshness check comes after the call, not before: a completed fetch
            // is promoted by get_pending_snapshot itself, and that promotion clears
            // the flags — so what matters is whether the listing is trusted *now*.
            Ok(Some(snapshot)) if c.may_serve_pending_snapshot(&path) => {
                let self_entry = c.pending_dirs.get(&path).and_then(|p| p.self_entry.clone());
                return Ok((Arc::new(snapshot), self_entry));
            }
            Ok(_) => {}
        }
    }

    // Start incremental streaming fetch — unless another thread already started one
    let (already_pending, was_invalidated) = {
        let mut c = cache.safe_lock();
        // Both invalidated and hard-expired dirs still hold a stale listing that
        // readdir's continuation pages read directly, so a partial stream snapshot
        // must not be served for them: wait for the full listing to be promoted.
        let was_inv = c.dir_cache.get(&path).map_or(false, |e| e.invalidated || e.hard_expired);
        if c.dir_cache.contains_key(&path) {
            if let Some((files, _)) = c.get_cached_dir(&path, ttl, max_stale) {
                let se = c.dir_cache.get(&path).and_then(|e| e.self_entry.clone());
                return Ok((files, se));
            }
        }
        (if c.pending_dirs.contains_key(&path) {
            true
        } else {
            let (entry_tx, entry_rx) = mpsc::channel();
            let (etag_tx, etag_rx) = mpsc::channel::<Result<Option<String>, String>>();
            let (self_tx, self_rx) = mpsc::channel();
            c.start_pending(path.clone(), entry_rx, etag_rx, self_rx);

            let conn2 = conn.clone();
            let path2 = path.clone();
            let pending_notify2 = c.pending_notify.clone();
            // The worker owns the senders: the pending entry stays "in flight"
            // (its channel connected) for exactly as long as the fetch is queued
            // or running, which is what lets later readers join instead of
            // starting a duplicate.
            let submitted = bg::LISTING.submit(move || {
                let result = match conn2.throttle.acquire_timeout(PROPFIND_TIMEOUT) {
                    Some(_permit) => conn2.backend
                        .list_dir_streaming(&path2, PROPFIND_TIMEOUT, entry_tx, self_tx)
                        .map_err(|e| e.to_string()),
                    None => Err(format!("PROPFIND timeout for {} (no request slot)", path2.display())),
                };
                match &result {
                    Ok(_) => note_listing_outcome(&conn2, &path2, Ok(())),
                    Err(e) => {
                        note_listing_outcome(&conn2, &path2, Err(e));
                        // readdir reports the same failure to the user; keep this one quiet.
                        log::debug!("incremental list {}: {}", path2.display(), e);
                    }
                }
                let _ = etag_tx.send(result);
                // Wake any threads waiting in get_or_list_dir for this path.
                pending_notify2.1.notify_all();
            });
            if submitted.is_err() {
                // Nothing will ever complete this entry; drop it so the next
                // reader can try again once the pool has room.
                c.pending_dirs.remove(&path);
                return Err(format!("network: listing {} deferred — too many listings in flight", path.display()));
            }
            false
        }, was_inv)
    };
    if already_pending {
        log::info!("LIST_JOIN {} — waiting for existing fetch", path.display());
    }

    // Block until first entries arrive or PROPFIND completes/times out.
    let deadline = Instant::now() + PROPFIND_TIMEOUT;
    let mut poll_iters = 0u32;
    let pending_notify = cache.safe_lock().pending_notify.clone();
    loop {
        {
            let mut c = cache.safe_lock();
            match c.get_pending_snapshot(&path) {
                Err(e) => return Err(e),
                Ok(Some(snapshot)) if !snapshot.is_empty() && !was_invalidated => {
                    if poll_iters > 2 { log::debug!("LIST_STREAM_WAIT {} iters before stream", poll_iters); }
                    log::info!("LIST_STREAM {} ({} entries) in {:?}", path.display(), snapshot.len(), t0.elapsed());
                    let se = c.pending_dirs.get(&path).and_then(|p| p.self_entry.clone());
                    return Ok((Arc::new(snapshot), se));
                }
                Ok(_) => {}
            }
            if c.not_dirs.remove(&path) {
                return Err(format!("{}: {}", NOT_A_DIRECTORY_ERR, path.display()));
            }
            if c.dir_cache.get(&path).map_or(false, |e| !e.invalidated && !e.hard_expired) {
                let se = c.dir_cache.get(&path).and_then(|e| e.self_entry.clone());
                if poll_iters > 2 { log::debug!("LIST_PROMOTED_WAIT {} iters for {}", poll_iters, path.display()); }
                log::info!("LIST_PROMOTED {} in {:?}", path.display(), t0.elapsed());
                return c.get_cached_dir(&path, ttl, max_stale)
                    .map(|(f, _)| (f, se))
                    .ok_or_else(|| format!("PROPFIND returned empty for {}", path.display()));
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        poll_iters += 1;
        // Wait on condvar instead of fixed sleep so we wake immediately when
        // a pending PROPFIND delivers its first entries or completes.
        let guard = pending_notify.0.lock().unwrap();
        let _ = pending_notify.1.wait_timeout(guard, Duration::from_millis(50)).unwrap();
    }

    // Our wait is over but the fetch may not be. Drain what arrived, and:
    //  * promote only a *finished* stream — caching a half-received listing as
    //    complete would hide the rest of the directory until the next refresh;
    //  * never drop the pending entry of a fetch still in flight (its sender is
    //    connected). Dropping it here used to let the next readdir start a
    //    duplicate fetch while this one sat in the throttle queue, which under
    //    a slow or failing server snowballed into thousands of threads.
    let mut c = cache.safe_lock();
    let (finished, partial) = if let Some(pending) = c.pending_dirs.get_mut(&path) {
        let mut disconnected = false;
        loop {
            match pending.rx.try_recv() {
                Ok(entry) => pending.entries.push(entry),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => { disconnected = true; break; }
            }
        }
        (disconnected, (!pending.entries.is_empty()).then(|| (pending.entries.clone(), pending.self_entry.clone())))
    } else {
        (false, None)
    };
    if finished {
        let se = c.promote_pending(&path)?;
        if c.not_dirs.remove(&path) {
            return Err(format!("{}: {}", NOT_A_DIRECTORY_ERR, path.display()));
        }
        if let Some((files, _)) = c.get_cached_dir(&path, ttl, max_stale) {
            return Ok((files, se));
        }
    } else if let Some((entries, se)) = partial.filter(|_| !was_invalidated) {
        log::info!("LIST_STREAM_PARTIAL {} ({} entries so far, still streaming) in {:?}", path.display(), entries.len(), t0.elapsed());
        return Ok((Arc::new(entries), se));
    } else {
        log::warn!("PROPFIND timeout {} — no entries yet after {:?}; the fetch stays in flight", path.display(), t0.elapsed());
    }
    Err(format!("PROPFIND timeout for {}", path.display()))
}

pub(crate) fn ensure_file_cached(
    conn: &Arc<ConnInfo>,
    cache: &Arc<Mutex<FsCache>>,
    status: &StatusMap,
    dirty: &ipc::DirtySet,
    remote_path: PathBuf,
    transfers: Option<&TransferMap>,
    kept: bool,
) -> Result<PathBuf, String> {
    ensure_file_cached_within(conn, cache, status, dirty, remote_path, transfers, kept, None)
}

/// [`ensure_file_cached`], with every slot wait, attempt and retry fitted inside
/// `deadline` when one is given — for a caller holding a FUSE reply, where three
/// full attempts (30 s slot wait + 120 s download each) were ~7.5 minutes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ensure_file_cached_within(
    conn: &Arc<ConnInfo>,
    cache: &Arc<Mutex<FsCache>>,
    status: &StatusMap,
    dirty: &ipc::DirtySet,
    remote_path: PathBuf,
    transfers: Option<&TransferMap>,
    kept: bool,
    deadline: Option<Instant>,
) -> Result<PathBuf, String> {
    let (maybe_local, was_kept, cached_mod, current_mod, target_dir, file_size) = {
        let c = cache.safe_lock();
        let entry = c.file_cache.get(&remote_path);
        let maybe_local = entry
            .filter(|e| e.local_path.metadata().map_or(false, |m| m.len() > 0))
            .map(|e| e.local_path.clone());
        let was_kept = entry.map_or(false, |e| e.kept);
        let cached_mod = entry.and_then(|e| e.remote_modified);
        let current_mod = c.remote_modified_for(&remote_path);
        let file_size = c.find_entry(&remote_path).map(|e| e.size).unwrap_or(0);
        let target_dir = if kept { c.kept_dir.clone() } else { c.auto_cache_dir.clone() };
        (maybe_local, was_kept, cached_mod, current_mod, target_dir, file_size)
    };

    if let Some(local) = maybe_local {
        if cached_mod == current_mod {
            if kept && !was_kept {
                let rel = remote_path.strip_prefix("/").unwrap_or(&remote_path);
                let new_path = target_dir.join(rel);
                if local != new_path {
                    if let Some(parent) = new_path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if std::fs::rename(&local, &new_path).is_ok() {
                        let mut c = cache.safe_lock();
                        if let Some(entry) = c.file_cache.get_mut(&remote_path) {
                            entry.local_path = new_path.clone();
                            entry.kept = true;
                        }
                        drop(c);
                        save_file_cache(cache);
                        status.safe_write().insert(remote_path.clone(), FileStatus::Kept);
                        dirty.safe_lock().insert(remote_path);
                        return Ok(new_path);
                    }
                }
            }
            return Ok(local);
        }
    }

    status.safe_write().insert(remote_path.clone(), FileStatus::Downloading);
    dirty.safe_lock().insert(remote_path.clone());

    if let Some(tm) = transfers {
        tm.safe_lock().insert(whole_file_transfer(&remote_path), TransferProgress {
            path: remote_path.clone(),
            direction: TransferDirection::Download,
            bytes_done: 0,
            total_bytes: file_size,
        });
    }

    let rel = remote_path.strip_prefix("/").unwrap_or(&remote_path);
    let local_path = target_dir.join(rel);
    if let Some(parent) = local_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    const DOWNLOAD_RETRIES: u32 = 2;
    let mut dl_delay = Duration::from_millis(500);
    let mut dl_attempt = 0u32;
    loop {
        let file =
            std::fs::File::create(&local_path).map_err(|e| format!("create cache file: {}", e))?;
        match open_file_within(conn, remote_path.clone(), file, transfers.cloned(), deadline) {
            Ok(()) => break,
            Err(e) => {
                if let Some(tm) = transfers { tm.safe_lock().remove(&whole_file_transfer(&remote_path)); }
                if let Err(rm_err) = std::fs::remove_file(&local_path) {
                    log::error!("CRITICAL: cannot remove partial download {}: {} — zeroing to prevent serving corrupt data", local_path.display(), rm_err);
                    if let Ok(f) = std::fs::File::create(&local_path) {
                        let _ = f.set_len(0);
                    }
                }
                let time_left = deadline.is_none_or(|d| d.saturating_duration_since(Instant::now()) > dl_delay);
                if dl_attempt < DOWNLOAD_RETRIES && time_left && is_transient_network_err(&e) {
                    log::warn!("download {} failed (attempt {}/{}): {} — retrying in {:?}",
                        remote_path.display(), dl_attempt + 1, DOWNLOAD_RETRIES + 1, e, dl_delay);
                    if let Some(tm) = transfers {
                        tm.safe_lock().insert(whole_file_transfer(&remote_path), TransferProgress {
                            path: remote_path.clone(),
                            direction: TransferDirection::Download,
                            bytes_done: 0,
                            total_bytes: file_size,
                        });
                    }
                    thread::sleep(dl_delay);
                    dl_delay = (dl_delay * 2).min(Duration::from_secs(4));
                    dl_attempt += 1;
                    continue;
                }
                cache.safe_lock().file_cache.remove(&remote_path);
                status.safe_write().insert(remote_path.clone(), FileStatus::Remote);
                dirty.safe_lock().insert(remote_path);
                return Err(e);
            }
        }
    }

    if let Some(tm) = transfers { tm.safe_lock().remove(&whole_file_transfer(&remote_path)); }

    let final_status = if kept { FileStatus::Kept } else { FileStatus::Cached };
    {
        let mut c = cache.safe_lock();
        let mod_time = c.remote_modified_for(&remote_path);
        let etag = c.remote_etag_for(&remote_path);
        c.file_cache.insert(
            remote_path.clone(),
            FileCacheEntry { local_path: local_path.clone(), remote_modified: mod_time, etag, kept, size: std::fs::metadata(&local_path).map(|m| m.len()).unwrap_or(0) },
        );
    }
    save_file_cache(cache);
    status.safe_write().insert(remote_path.clone(), final_status);
    dirty.safe_lock().insert(remote_path);
    Ok(local_path)
}

/// Keeps a file, or a directory and everything under it. Returns whether the
/// path itself was kept: the file downloaded, or the directory listed. A child
/// that fails inside a kept directory is logged, not reported here.
fn keep_locally_recursive(
    conn: &Arc<ConnInfo>,
    cache: &Arc<Mutex<FsCache>>,
    status: &StatusMap,
    dirty: &ipc::DirtySet,
    remote_path: PathBuf,
    transfers: Option<&TransferMap>,
) -> bool {
    log::info!("KEEP {}", remote_path.display());

    let keep_file = |why: Option<&str>| -> bool {
        match ensure_file_cached(conn, cache, status, dirty, remote_path.clone(), transfers, true) {
            Ok(_) => true,
            Err(e) => {
                match why {
                    Some(w) => log::warn!("keep failed {}: {} / {}", remote_path.display(), w, e),
                    None => log::warn!("keep failed {}: {}", remote_path.display(), e),
                }
                false
            }
        }
    };

    let known_dir = cache.safe_lock().is_known_directory(&remote_path);

    if known_dir == Some(false) {
        return keep_file(None);
    }

    let (entries, self_entry) = match get_or_list_dir(conn, cache, remote_path.clone(), None) {
        Ok(e) => e,
        Err(e) => {
            // Type unknown and the listing failed: it may well be a file (a
            // file's listing now fails with NOT_A_DIRECTORY_ERR).
            if known_dir.is_none() {
                return keep_file(Some(&e));
            }
            log::warn!("keep dir failed {}: {}", remote_path.display(), e);
            return false;
        }
    };
    // A listing cached before files were refused as directories (or answered
    // from one) can still describe a file as an empty directory.
    if self_entry.as_ref().is_some_and(|se| !se.is_dir) {
        cache.safe_lock().dir_cache.remove(&remote_path);
        return keep_file(None);
    }

    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for entry in entries.iter() {
        let name = match entry.path.file_name() {
            Some(n) => n.to_string_lossy().to_string(),
            None => continue,
        };
        let child = remote_path.join(&name);
        if entry.is_dir {
            dirs.push(child);
        } else {
            files.push(child);
        }
    }

    for chunk in files.chunks(bg::KEEP_SCOPE_WIDTH) {
        std::thread::scope(|s| {
            for path in chunk {
                s.spawn(|| {
                    if let Err(e) = ensure_file_cached(conn, cache, status, dirty, path.clone(), transfers, true) {
                        log::warn!("keep failed {}: {}", path.display(), e);
                    }
                });
            }
        });
        std::thread::sleep(Duration::from_millis(50));
    }

    for dir in dirs {
        keep_locally_recursive(conn, cache, status, dirty, dir, transfers);
    }
    true
}

fn prefetch_list_dir(conn: &ConnInfo, cache: &Mutex<FsCache>, path: &Path) {
    {
        let c = cache.safe_lock();
        if c.get_cached_dir_readonly(path).is_some() || c.pending_dirs.contains_key(path) {
            return;
        }
    }
    log::info!("PREFETCH_LIST {}", path.display());
    let _permit = conn.prefetch_throttle.acquire();
    match conn.backend.list_dir(path, PROPFIND_TIMEOUT) {
        Ok((etag, self_entry, files)) => {
            cache.safe_lock().put_dir_cache(path.to_path_buf(), etag, self_entry, files);
        }
        Err(e) => log::warn!("prefetch {}: {}", path.display(), e),
    }
}

// How many levels of subdirectory prefetch to chain after a readdir.
// depth=0 means: fetch the dir itself, no further chaining.
// depth=1 means: also kick off prefetch for discovered subdirs.
// Only chains when the directory has ≤ PREFETCH_CHAIN_THRESHOLD subdirs,
// to avoid overwhelming the server for huge directories like chat archives.
const PREFETCH_CHAIN_DEPTH: u32 = 1;
const PREFETCH_CHAIN_THRESHOLD: usize = 60;

// Start a background streaming PROPFIND for `path` without blocking.
// Uses pending_dirs so any concurrent READDIR joins the in-flight fetch
// rather than starting a duplicate request. When `chain_depth > 0` and the
// directory is small, kicks off prefetches for its subdirs after completion.
fn start_background_propfind(
    conn: &Arc<ConnInfo>,
    cache: &Arc<Mutex<FsCache>>,
    path: PathBuf,
    chain_depth: u32,
) {
    if conn.shutdown.load(Ordering::Relaxed) || conn.paused.load(Ordering::Relaxed) { return; }
    // Prefetch is speculative: it is the first thing to go when the server struggles.
    let now = Instant::now();
    if conn.breaker.is_open(now) || conn.backoff.blocked(&path, now).is_some() {
        return;
    }
    // Check and register under one lock, so two callers can't both start a fetch.
    let (entry_tx, entry_rx) = mpsc::channel();
    let (etag_tx, etag_rx) = mpsc::channel();
    let (self_tx, self_rx) = mpsc::channel();
    let pending_notify2 = {
        let mut c = cache.safe_lock();
        if c.dir_cache.contains_key(&path) || c.pending_dirs.contains_key(&path) {
            return;
        }
        c.start_pending_and_notify(path.clone(), entry_rx, etag_rx, self_rx)
    };
    let conn2 = conn.clone();
    let cache2 = cache.clone();
    let path_for_reject = path.clone();
    let submitted = bg::BACKGROUND.submit(move || {
        let result = match conn2.prefetch_throttle.acquire_timeout(PROPFIND_TIMEOUT) {
            // The prefetch slot is released at the end of this arm, before chaining children.
            Some(_permit) => conn2.backend
                .list_dir_streaming(&path, PROPFIND_TIMEOUT, entry_tx, self_tx)
                .map_err(|e| e.to_string()),
            None => Err(format!("PROPFIND timeout for {} (no prefetch slot)", path.display())),
        };
        match result {
            Ok(etag) => {
                note_listing_outcome(&conn2, &path, Ok(()));
                let _ = etag_tx.send(Ok(etag));
            }
            Err(e) => {
                note_listing_outcome(&conn2, &path, Err(&e));
                log::debug!("bg propfind {}: {}", path.display(), e);
                let _ = etag_tx.send(Err(e));
                pending_notify2.1.notify_all();
                schedule_save_dir_cache(&cache2);
                return;
            }
        }
        // Wake threads waiting in get_or_list_dir for this path.
        pending_notify2.1.notify_all();
        // Chain: kick off next-level prefetches now that our slot is free.
        // Skipped for large dirs to avoid spawning hundreds of threads.
        if chain_depth > 0 {
            let children = cache2.safe_lock().subdir_paths_for_prefetch(&path);
            if children.len() <= PREFETCH_CHAIN_THRESHOLD {
                for child in children {
                    start_background_propfind(&conn2, &cache2, child, chain_depth - 1);
                }
            }
        }
        schedule_save_dir_cache(&cache2);
    });
    if submitted.is_err() {
        cache.safe_lock().pending_dirs.remove(&path_for_reject);
    }
}

// ── FileAttr helpers ──────────────────────────────────────────────────────────

// Map Nextcloud oc:permissions flags to POSIX mode bits.
// G=read, W=write(file), C=create(dir), D=delete, N=rename, V=move.
// Directories always have execute set so the kernel can traverse them.
pub(crate) fn perms_to_mode(permissions: Option<&str>, is_dir: bool) -> u16 {
    let perms = match permissions {
        Some(p) if !p.is_empty() => p,
        _ => return if is_dir { 0o755 } else { 0o644 },
    };
    let r = perms.contains('G');
    let w = if is_dir {
        perms.contains('C') || perms.contains('D') || perms.contains('N') || perms.contains('V')
    } else {
        perms.contains('W')
    };
    if is_dir {
        match (r, w) {
            (true,  true)  => 0o755,
            (true,  false) => 0o555,
            _              => 0o000,
        }
    } else {
        match (r, w) {
            (true,  true)  => 0o644,
            (true,  false) => 0o444,
            _              => 0o000,
        }
    }
}

// The mounting process's uid/gid never change, so resolve them once instead of
// calling getuid/getgid on every attribute built (readdirplus builds one per
// directory entry).
fn proc_uid() -> u32 {
    static UID: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *UID.get_or_init(|| unsafe { libc::getuid() })
}
fn proc_gid() -> u32 {
    static GID: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *GID.get_or_init(|| unsafe { libc::getgid() })
}

pub(crate) fn make_file_attr(inode: u64, entry: &RemoteEntry) -> FileAttr {
    let modified = entry.modified.unwrap_or(UNIX_EPOCH);
    FileAttr {
        ino: INodeNo(inode),
        size: entry.size,
        blocks: (entry.size + 511) / 512,
        atime: modified,
        mtime: modified,
        ctime: modified,
        crtime: modified,
        kind: if entry.is_dir { FileType::Directory } else { FileType::RegularFile },
        perm: perms_to_mode(entry.ext.str("permissions"), entry.is_dir),
        nlink: if entry.is_dir { 2 } else { 1 },
        uid: proc_uid(),
        gid: proc_gid(),
        rdev: 0,
        flags: 0,
        blksize: 512,
    }
}

fn make_dir_attr(inode: u64) -> FileAttr {
    FileAttr {
        ino: INodeNo(inode),
        size: 0,
        blocks: 0,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind: FileType::Directory,
        perm: 0o755,
        nlink: 2,
        uid: proc_uid(),
        gid: proc_gid(),
        rdev: 0,
        flags: 0,
        blksize: 512,
    }
}

/// Attributes for a regular file we have no listing entry for. Used where the
/// caller already knows the target is a file — reaching for `make_dir_attr`
/// there would report S_IFDIR for a plain file.
fn make_unknown_file_attr(inode: u64, size: u64) -> FileAttr {
    FileAttr {
        ino: INodeNo(inode),
        size,
        blocks: (size + 511) / 512,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind: FileType::RegularFile,
        perm: 0o644,
        nlink: 1,
        uid: proc_uid(),
        gid: proc_gid(),
        rdev: 0,
        flags: 0,
        blksize: 512,
    }
}

fn root_attr() -> FileAttr {
    FileAttr {
        ino: INodeNo(1),
        size: 0,
        blocks: 0,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind: FileType::Directory,
        perm: 0o755,
        nlink: 2,
        uid: proc_uid(),
        gid: proc_gid(),
        rdev: 0,
        flags: 0,
        blksize: 512,
    }
}

// ── Filesystem ────────────────────────────────────────────────────────────────

struct ConnInfo {
    backend: Arc<dyn crate::backend::CloudBackend>,
    base_url: String,
    webdav_url: String,
    creds: auth::Credentials,
    mount_point: PathBuf,
    clients: crate::http_clients::HttpClients,
    optimistic_listing: bool,
    // None when dir_cache_max_stale_mins is 0 (check disabled).
    dir_cache_max_stale: Option<Duration>,
    notify_push_connected: Arc<AtomicBool>,
    throttle: Arc<Throttle>,
    read_throttle: Arc<Throttle>,
    prefetch_throttle: Arc<Throttle>,
    is_offline: Arc<AtomicBool>,
    /// When `is_offline` last went false→true. Lets a read tell a momentary blip
    /// (worth waiting out) from a sustained outage (fail fast) — see
    /// [`wait_out_offline_blip`]. `None` while online.
    offline_since: Arc<Mutex<Option<Instant>>>,
    active_streams: Arc<AtomicUsize>,
    deferred_invalidation: Arc<AtomicBool>,
    /// Set by a read that saw the server go slow; the connectivity monitor probes
    /// at its next 1 s tick instead of waiting out its interval (`request_probe`).
    probe_soon: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    // Live on/off switch for kernel FUSE_PASSTHROUGH, hot-toggleable over IPC
    // (PASSTHROUGH_ON/OFF) without a remount. Seeded from MountOptions.fuse_passthrough.
    passthrough_enabled: Arc<AtomicBool>,
    // Sticky per-session verdict: flipped false the first time open_backing()
    // fails (missing CAP_SYS_ADMIN, kernel <6.9, etc.) so every later open()
    // just falls back to a normal reply instead of re-probing and re-logging.
    passthrough_capable: Arc<AtomicBool>,
    /// Per-directory cooldown after the server failed a listing (see `backoff.rs`).
    backoff: Arc<backoff::PathBackoff>,
    /// Server-wide 5xx breaker: pauses background listing work while open.
    breaker: Arc<backoff::ServerBreaker>,
    /// Per-process accounting and rate limit for uncached listings (see `walkers.rs`).
    walkers: Arc<walkers::WalkerTracker>,
}

/// Clears an in-progress flag on drop, so a panicking worker cannot latch it.
struct ReleaseOnDrop(Arc<AtomicBool>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

struct StreamActiveGuard {
    counter: Arc<AtomicUsize>,
    deferred: Arc<AtomicBool>,
    cache: Arc<Mutex<FsCache>>,
    dirty: ipc::DirtySet,
    notifier_slot: fuse_notify::NotifierSlot,
}
impl Drop for StreamActiveGuard {
    fn drop(&mut self) {
        if self.counter.fetch_sub(1, Ordering::Relaxed) == 1 {
            if self.deferred.swap(false, Ordering::Relaxed) {
                log::info!("stream ended — applying deferred dir cache invalidation");
                notify_push::invalidate_all_dirs(&self.cache, &self.dirty, &self.notifier_slot);
            }
        }
    }
}

/// One open directory stream. `snapshot` is the listing the offset-0 page was
/// built from, pinned for the life of the handle so the continuation pages the
/// kernel asks for are paged out of exactly the vector their offsets index —
/// even if the dir cache evicts or refreshes the listing in between.
struct OpenDir {
    path: PathBuf,
    snapshot: Option<Arc<Vec<RemoteEntry>>>,
}

pub struct NextCloudFs {
    cache: Arc<Mutex<FsCache>>,
    status: StatusMap,
    dirty: ipc::DirtySet,
    shared: ipc::SharedSet,
    fileids: ipc::FileIdMap,
    details: ipc::FileDetailMap,
    children_map: ipc::ChildrenMap,
    conn: Arc<ConnInfo>,
    open_files: Arc<Mutex<HashMap<u64, OpenFile>>>,
    uploads: UploadOrder,
    io_modes: Mutex<iomode::InodeIoModes<BackingId>>,
    open_dirs: Arc<Mutex<HashMap<u64, OpenDir>>>,
    next_fh: Arc<Mutex<u64>>,
    error_log: ErrorLog,
    transfer_map: TransferMap,
    journal: mutation_journal::SharedJournal,
    notifier_slot: fuse_notify::NotifierSlot,
    ghost_entries: GhostMap,
    // Shared between notify-push refreshes and read-triggered revalidation so
    // the two paths never double-probe the same directory within a cooldown.
    refresh_debounce: notify_push::DebounceMap,
    file_change_queue: ipc::FileChangeQueue,
    log_user: String,
    aggressive_prefetch: bool,
    auto_keep_locally_modified_files: bool,
    auto_keep_cached_files: bool,
    read_ahead_bytes: usize,
    cache_streamed_reads: bool,
    exclude_folders: HashSet<PathBuf>,
    thumb_inflight: Arc<Mutex<HashSet<PathBuf>>>,
    cleanup_stale_gio_temps: bool,
}


impl NextCloudFs {
    pub fn new(options: MountOptions) -> Result<Self, String> {
        let creds = options.credentials()?;

        let exclude_folders: HashSet<PathBuf> = options.exclude_folders.iter().map(|s| {
            let s = s.trim();
            if s.starts_with('/') { PathBuf::from(s) } else { PathBuf::from(format!("/{}", s)) }
        }).collect();
        for kp in &options.keep_paths {
            let kp = kp.trim();
            let kp_path = if kp.starts_with('/') { PathBuf::from(kp) } else { PathBuf::from(format!("/{}", kp)) };
            for ep in &exclude_folders {
                if kp_path.starts_with(ep) || ep.starts_with(&kp_path) {
                    panic!("invalid config: path {:?} is both in keep_paths and exclude_folders", kp);
                }
            }
        }

        let cache_dir = ncrs_cache_dir(&options.url);
        let kept_dir = cache_dir.join("kept");
        let auto_cache_dir = cache_dir.join("cache");
        std::fs::create_dir_all(&cache_dir)
            .map_err(|e| format!("Cannot create cache dir: {}", e))?;
        std::fs::create_dir_all(&kept_dir)
            .map_err(|e| format!("Cannot create kept dir: {}", e))?;
        std::fs::create_dir_all(&auto_cache_dir)
            .map_err(|e| format!("Cannot create auto-cache dir: {}", e))?;

        let journal_arc: mutation_journal::SharedJournal =
            Arc::new(Mutex::new(mutation_journal::MutationJournal::load_or_create(&cache_dir)));

        // Remove write_* staging files not referenced by any pending journal entry.
        // Files in the journal still need their staging data for upload replay;
        // everything else is orphaned (upload completed, non-dirty open, coalesced, etc.).
        {
            let j = journal_arc.safe_lock();
            let referenced: std::collections::HashSet<PathBuf> = j.entries()
                .iter()
                .filter_map(|e| e.op.staging_path().map(Path::to_path_buf))
                .collect();
            let mut stale_count = 0usize;
            if let Ok(dir_entries) = std::fs::read_dir(&cache_dir) {
                for entry in dir_entries.flatten() {
                    if entry.file_name().to_str().map_or(false, |n| n.starts_with("write_")) {
                        let path = entry.path();
                        if !referenced.contains(&path) {
                            let _ = std::fs::remove_file(&path);
                            stale_count += 1;
                        }
                    }
                }
            }
            if stale_count > 0 {
                log::info!("cleaned up {} orphaned write_* staging files at startup", stale_count);
            }
        }

        let mut inodes = HashMap::new();
        let mut paths = HashMap::new();
        inodes.insert(1, PathBuf::from("/"));
        paths.insert(PathBuf::from("/"), 1);

        let status: StatusMap = Arc::new(RwLock::new(HashMap::new()));
        let dirty: ipc::DirtySet = Arc::new(Mutex::new(std::collections::HashSet::new()));
        let shared: ipc::SharedSet = Arc::new(RwLock::new(std::collections::HashSet::new()));
        let fileids: ipc::FileIdMap = Arc::new(RwLock::new(HashMap::new()));
        let details: ipc::FileDetailMap = Arc::new(RwLock::new(HashMap::new()));
        let children_map: ipc::ChildrenMap = Arc::new(RwLock::new(HashMap::new()));

        let base_url = notifications::base_url(&options.url);
        let use_http3 = options.http3 && !options.offline;

        // Fail fast when the network is gone. Without a connect timeout a dead
        // route makes each request block for the full per-request timeout
        // (WRITE_TIMEOUT 60s / DOWNLOAD_TIMEOUT 120s), which freezes a
        // synchronous read()-driven save until the first op finally errors. A
        // short connect timeout lets an interface-down failure surface in
        // seconds so the offline fallback (serve cache + journal the write)
        // engages promptly instead.
        //
        // The read client also disables idle connection reuse
        // (pool_max_idle_per_host = 0). connect_timeout only bounds the CONNECT
        // phase; a request that reuses a warm pooled connection whose network has
        // since gone silent (packets dropped, not refused) skips connect entirely
        // and blocks on the full DOWNLOAD_TIMEOUT — the exact "save hangs until the
        // network is back" report, since a read()-driven save reuses the same
        // connection opening the file just warmed. Forcing every foreground read to
        // connect fresh makes connect_timeout effective, so a blackholed read fails
        // in seconds and flips the daemon offline. The cost is negligible here: a
        // read fetches a 64 MiB read-ahead window per request, so there is roughly
        // one connect per file open, not per read(). The metadata/write client keeps
        // its idle pool — those requests run off the FUSE read path (background PUTs,
        // the connectivity probe's own 5s timeout), so a stale warm connection there
        // never freezes an app.
        //
        // Both transports are built up front. `http3_prior_knowledge()` makes a client
        // QUIC-only, so when UDP/443 is blocked or the server's QUIC listener is broken
        // every request fails at the transport layer and the daemon mistakes that for
        // "server unreachable". The HTTP/2 pair is the escape hatch the connectivity
        // probe demotes to; see `http_clients`.
        //
        // The read side is not one client but DOWNLOAD_CONNECTIONS of them, one per
        // `read_throttle` slot (see `http_clients::DOWNLOAD_CONNECTIONS`): reqwest's
        // h3 pool keeps a single QUIC connection per host, so one client meant every
        // download shared one congestion controller, one UDP socket and one runtime
        // thread. They are built once (`build_read_clients`) and live as long as the
        // mount — except the HTTP/2 set under HTTP/3, built only on a demotion.
        let build_meta = |http3: bool| -> Result<reqwest::blocking::Client, String> {
            let mut meta = crate::http_clients::with_pooled_dns(reqwest::blocking::Client::builder())
                .pool_max_idle_per_host(16)
                .connect_timeout(CONNECT_TIMEOUT);
            if http3 {
                meta = meta.http3_prior_knowledge();
            }
            meta.build().map_err(|e| format!("HTTP client: {}", e))
        };
        let http_h2 = build_meta(false)?;
        let clients = if use_http3 {
            let pref = build_meta(true)?;
            let reads = build_read_clients(true)?;
            // Building them bound their QUIC endpoints' UDP sockets; reqwest gives
            // no way to size them, so find and enlarge them now.
            crate::http_clients::raise_quic_socket_buffers();
            // The HTTP/2 read set is only the demotion fallback: built on first use
            // after a demotion instead of idling eight runtime threads all session.
            crate::http_clients::HttpClients::with_lazy_h2_reads(
                pref, reads, http_h2, || build_read_clients(false), READ_STALL_TIMEOUT,
            )
        } else {
            let reads = build_read_clients(false)?;
            crate::http_clients::HttpClients::new(http_h2.clone(), reads.clone(), http_h2, reads, false)
        }
        // Remember a demotion across restarts (per server, in its cache dir):
        // re-arming QUIC every session made each restart on a QUIC-hostile
        // network pay one offline blip before latching onto HTTP/2 again.
        .with_demotion_marker(h3_demotion_marker(&options.url));

        let max_req = if options.max_concurrent_requests == 0 { 10 } else { options.max_concurrent_requests };
        log::info!("HTTP throttle: max {} concurrent requests", max_req);
        let is_offline = Arc::new(AtomicBool::new(options.offline));

        let backend: Arc<dyn crate::backend::CloudBackend> = if options.offline {
            Arc::new(crate::nextcloud::NextcloudBackend::new_offline(
                base_url.clone(),
                options.url.clone(),
                creds.clone(),
                clients.clone(),
            ))
        } else {
            Arc::new(crate::nextcloud::NextcloudBackend::new(
                base_url.clone(),
                options.url.clone(),
                creds.clone(),
                clients.clone(),
            )?)
        };

        let conn = Arc::new(ConnInfo {
            backend,
            base_url,
            webdav_url: options.url.clone(),
            creds,
            mount_point: options.mount_point.clone(),
            clients,
            offline_since: Arc::new(Mutex::new(options.offline.then(Instant::now))),
            throttle: Arc::new(Throttle::new(max_req)),
            // One slot per read client: a slot's index picks its connection, so a
            // connection is never shared by two concurrent downloads.
            read_throttle: Arc::new(Throttle::new(crate::http_clients::DOWNLOAD_CONNECTIONS)),
            prefetch_throttle: Arc::new(Throttle::new(5)),
            is_offline,
            optimistic_listing: options.optimistic_listing,
            dir_cache_max_stale: (options.dir_cache_max_stale_mins > 0)
                .then(|| Duration::from_secs(options.dir_cache_max_stale_mins.saturating_mul(60))),
            notify_push_connected: Arc::new(AtomicBool::new(false)),
            active_streams: Arc::new(AtomicUsize::new(0)),
            deferred_invalidation: Arc::new(AtomicBool::new(false)),
            probe_soon: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            passthrough_enabled: Arc::new(AtomicBool::new(options.fuse_passthrough)),
            passthrough_capable: Arc::new(AtomicBool::new(true)),
            backoff: Arc::new(backoff::PathBackoff::new()),
            breaker: Arc::new(backoff::ServerBreaker::new()),
            // Set from the settings panel (walker_rate_limit); NCRS_WALKER_LIMIT=off
            // still forces it off. Off keeps the accounting and warnings.
            walkers: Arc::new(walkers::WalkerTracker::new(
                options.walker_rate_limit
                    && !matches!(std::env::var("NCRS_WALKER_LIMIT").as_deref(), Ok("off" | "0" | "false")),
                options.walker_listings_per_sec,
            )),
        });
        let _ = HEALTH.set(HealthSources {
            breaker: conn.breaker.clone(),
            backoff: conn.backoff.clone(),
            walkers: conn.walkers.clone(),
        });

        Ok(NextCloudFs {
            cache: {
                let c = Arc::new(Mutex::new(FsCache {
                    inodes,
                    paths,
                    next_inode: 2,
                    dir_cache: HashMap::new(),
                    dir_cache_max_dirs: options.dir_cache_max_dirs,
                    pending_dirs: HashMap::new(),
                    file_cache: HashMap::new(),
                    cache_dir,
                    kept_dir,
                    auto_cache_dir,
                    pending_notify: Arc::new((Mutex::new(()), Condvar::new())),
                    uploading: HashSet::new(),
                    deleting: HashSet::new(),
                    not_dirs: HashSet::new(),
                    trackerignore_hidden: false,
                }));
                load_dir_cache(&c);
                c
            },
            status,
            dirty,
            shared,
            fileids,
            details,
            children_map,
            conn,
            open_files: Arc::new(Mutex::new(HashMap::new())),
            uploads: UploadOrder::default(),
            io_modes: Mutex::new(iomode::InodeIoModes::default()),
            open_dirs: Arc::new(Mutex::new(HashMap::new())),
            next_fh: Arc::new(Mutex::new(1)),
            error_log: Arc::new(Mutex::new(std::collections::VecDeque::new())),
            transfer_map: Arc::new(Mutex::new(HashMap::new())),
            journal: journal_arc,
            notifier_slot: Arc::new(Mutex::new(None)),
            ghost_entries: Arc::new(Mutex::new(HashMap::new())),
            refresh_debounce: Arc::new(Mutex::new(HashMap::new())),
            file_change_queue: Arc::new(Mutex::new(Vec::new())),
            log_user: options.log_user,
            aggressive_prefetch: options.aggressive_prefetch,
            auto_keep_locally_modified_files: options.auto_keep_locally_modified_files,
            auto_keep_cached_files: options.auto_keep_cached_files,
            read_ahead_bytes: options.read_ahead_bytes,
            cache_streamed_reads: options.cache_streamed_reads,
            exclude_folders,
            thumb_inflight: Arc::new(Mutex::new(HashSet::new())),
            cleanup_stale_gio_temps: options.cleanup_stale_gio_temps,
        })
    }

    pub fn status_map(&self) -> StatusMap {
        self.status.clone()
    }

    pub fn shared_set(&self) -> ipc::SharedSet {
        self.shared.clone()
    }

    pub fn fileid_map(&self) -> ipc::FileIdMap {
        self.fileids.clone()
    }

    pub fn detail_map(&self) -> ipc::FileDetailMap {
        self.details.clone()
    }

    pub fn children_map(&self) -> ipc::ChildrenMap {
        self.children_map.clone()
    }

    pub fn dirty_set(&self) -> ipc::DirtySet {
        self.dirty.clone()
    }

    /// Companion to [`Self::is_offline_flag`]: the two must be updated together
    /// via `mark_offline`/`mark_online` so the read grace window stays accurate.
    pub(crate) fn offline_since_slot(&self) -> Arc<Mutex<Option<Instant>>> {
        self.conn.offline_since.clone()
    }

    pub fn is_offline_flag(&self) -> Arc<AtomicBool> {
        self.conn.is_offline.clone()
    }

    pub fn notify_push_connected_flag(&self) -> Arc<AtomicBool> {
        self.conn.notify_push_connected.clone()
    }

    pub fn active_streams(&self) -> Arc<AtomicUsize> {
        self.conn.active_streams.clone()
    }

    pub fn deferred_invalidation(&self) -> Arc<AtomicBool> {
        self.conn.deferred_invalidation.clone()
    }

    pub fn shutdown_flag(&self) -> Arc<AtomicBool> {
        self.conn.shutdown.clone()
    }

    pub fn paused_flag(&self) -> Arc<AtomicBool> {
        self.conn.paused.clone()
    }

    pub fn passthrough_enabled_flag(&self) -> Arc<AtomicBool> {
        self.conn.passthrough_enabled.clone()
    }

    pub fn passthrough_capable_flag(&self) -> Arc<AtomicBool> {
        self.conn.passthrough_capable.clone()
    }

    pub(crate) fn conn(&self) -> Arc<ConnInfo> {
        self.conn.clone()
    }

    pub(crate) fn throttle(&self) -> Arc<Throttle> {
        self.conn.throttle.clone()
    }

    pub(crate) fn cache_ref(&self) -> Arc<Mutex<FsCache>> {
        self.cache.clone()
    }

    pub fn error_log(&self) -> ErrorLog {
        self.error_log.clone()
    }

    pub fn transfer_map(&self) -> TransferMap {
        self.transfer_map.clone()
    }

    /// Fetches `plan`'s window in the background as `fh`'s look-ahead (see
    /// `OpenFile::next_buf`). Never blocks the caller: the slot is only *tried*
    /// for here, on the calling thread, and the job goes to the `stream` pool — so
    /// a look-ahead never waits for a slot on a worker a READ could need. No free
    /// slot, or a full pool, just means no look-ahead this time.
    fn start_lookahead(&self, fh: u64, path: &Path, file_size: u64, plan: LookaheadPlan) {
        let clear = || {
            self.open_files.safe_lock().entry(fh).and_modify(|of| of.lookahead_inflight = false);
        };
        let Some(slot) = self.conn.read_throttle.try_acquire_owned(LOOKAHEAD_SPARE_SLOTS) else {
            clear();
            return;
        };
        let conn = self.conn.clone();
        let open_files = self.open_files.clone();
        let tmap = self.transfer_map.clone();
        let cache = self.cache.clone();
        let dirty = self.dirty.clone();
        let notifier_slot = self.notifier_slot.clone();
        let path = path.to_path_buf();
        let submitted = bg::STREAM.submit(move || {
            // Clears `lookahead_inflight` on every exit from here on, unwinding included.
            let _inflight = LookaheadInflight { open_files: Arc::clone(&open_files), fh };
            conn.active_streams.fetch_add(1, Ordering::Relaxed);
            let _stream_guard = StreamActiveGuard {
                counter: Arc::clone(&conn.active_streams),
                deferred: Arc::clone(&conn.deferred_invalidation),
                cache,
                dirty,
                notifier_slot,
            };
            run_lookahead(&conn, &open_files, &tmap, &path, fh, file_size, plan, slot);
        });
        // A refused job was dropped unrun, and with it the slot and the budget.
        if submitted.is_err() {
            clear();
        }
    }

    pub fn journal(&self) -> mutation_journal::SharedJournal {
        self.journal.clone()
    }

    pub fn notifier_slot(&self) -> fuse_notify::NotifierSlot {
        self.notifier_slot.clone()
    }

    pub(crate) fn ghost_entries(&self) -> GhostMap {
        self.ghost_entries.clone()
    }

    pub(crate) fn refresh_debounce(&self) -> notify_push::DebounceMap {
        self.refresh_debounce.clone()
    }

    pub fn file_change_queue(&self) -> ipc::FileChangeQueue {
        self.file_change_queue.clone()
    }

    pub fn keep_callback(&self) -> ipc::KeepCallback {
        let conn = self.conn.clone();
        let cache = self.cache.clone();
        let status = self.status.clone();
        let dirty = self.dirty.clone();
        let transfers = self.transfer_map.clone();
        Arc::new(move |remote_path| {
            keep_locally_recursive(&conn, &cache, &status, &dirty, remote_path, Some(&transfers))
        })
    }

    /// Evict a file, or a folder and every cached file under it: remove the local
    /// copies, set them back to remote, and drop the directories left empty in
    /// the kept and auto-cache trees.
    pub fn evict_callback(&self) -> ipc::EvictCallback {
        let cache = self.cache.clone();
        let status = self.status.clone();
        let dirty = self.dirty.clone();
        let journal = self.journal.clone();
        Arc::new(move |remote_path| {
            // Like purge: a file with a queued upload keeps its local bytes, which
            // may be the only copy of an unsynced edit. Journal lock alone, first.
            let pending: std::collections::HashSet<PathBuf> = {
                let j = journal.safe_lock();
                j.entries().iter().filter(|e| e.op.staging_path().is_some()).map(|e| e.op.path().to_path_buf()).collect()
            };
            let (removed, roots) = {
                let mut c = cache.safe_lock();
                let under: Vec<PathBuf> = c.file_cache.keys()
                    .filter(|p| p.starts_with(&remote_path) && !pending.contains(*p))
                    .cloned()
                    .collect();
                let removed: Vec<(PathBuf, PathBuf)> = under.into_iter()
                    .filter_map(|p| c.file_cache.remove(&p).map(|e| (p, e.local_path)))
                    .collect();
                (removed, [c.kept_dir.clone(), c.auto_cache_dir.clone()])
            };
            for (_, local_path) in &removed {
                match std::fs::remove_file(local_path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => log::warn!("evict: failed to remove cached file {}: {} — orphaned on disk", local_path.display(), e),
                }
                if let (Some(parent), Some(root)) = (local_path.parent(), roots.iter().find(|r| local_path.starts_with(r))) {
                    remove_empty_parents(parent, root);
                }
            }
            // The folder's own mirror in each tree, including empty subfolders.
            let rel = remote_path.strip_prefix("/").unwrap_or(&remote_path);
            if !rel.as_os_str().is_empty() {
                for root in &roots {
                    let mirror = root.join(rel);
                    remove_empty_tree(&mirror);
                    if let Some(parent) = mirror.parent() {
                        remove_empty_parents(parent, root);
                    }
                }
            }
            save_file_cache(&cache);
            {
                let mut st = status.safe_write();
                for (p, _) in &removed {
                    st.insert(p.clone(), FileStatus::Remote);
                }
                if !pending.contains(&remote_path) {
                    st.insert(remote_path.clone(), FileStatus::Remote);
                }
            }
            let mut d = dirty.safe_lock();
            for (p, _) in removed {
                d.insert(p);
            }
        })
    }

    /// Drop every locally cached file copy so subsequent reads re-download fresh
    /// content — the recovery for a cache tainted by a past bug. A file with a
    /// pending upload is skipped: its staged bytes are the only copy of an
    /// unsynced edit, so purging it would be data loss. Directory listings are
    /// invalidated too, so stale sizes/etags are re-fetched. Orphaned write-staging
    /// files — no longer referenced by the journal and not backing a handle a
    /// client still has open — are reclaimed too; anything still in either set is
    /// left untouched to avoid destroying an in-flight or unsynced edit.
    pub fn purge_callback(&self) -> ipc::PurgeCallback {
        let cache = self.cache.clone();
        let status = self.status.clone();
        let dirty = self.dirty.clone();
        let journal = self.journal.clone();
        let open_files = self.open_files.clone();
        let notifier_slot = self.notifier_slot.clone();
        Arc::new(move || {
            // Paths with a queued Put must be preserved — their local bytes are
            // unsynced. Collect them under the journal lock alone to avoid nesting.
            let (protected, staged): (std::collections::HashSet<PathBuf>, std::collections::HashSet<PathBuf>) = {
                let j = journal.safe_lock();
                let mut protected = std::collections::HashSet::new();
                let mut staged = std::collections::HashSet::new();
                for e in j.entries() {
                    if let Some(staging_path) = e.op.staging_path() {
                        protected.insert(e.op.path().to_path_buf());
                        staged.insert(staging_path.to_path_buf());
                    }
                }
                (protected, staged)
            };
            let to_remove: Vec<(PathBuf, PathBuf)> = {
                let c = cache.safe_lock();
                c.file_cache.iter()
                    .filter(|(p, _)| !protected.contains(*p))
                    .map(|(p, e)| (p.clone(), e.local_path.clone()))
                    .collect()
            };
            let mut purged = 0usize;
            for (remote, local) in &to_remove {
                match std::fs::remove_file(local) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        log::warn!("purge: failed to remove cached file {}: {}", local.display(), e);
                        continue;
                    }
                }
                cache.safe_lock().file_cache.remove(remote);
                status.safe_write().insert(remote.clone(), FileStatus::Remote);
                dirty.safe_lock().insert(remote.clone());
                purged += 1;
            }
            save_file_cache(&cache);

            // Reclaim orphaned write_<fh> staging files: skip anything the journal
            // still needs for upload replay, and anything whose fh a client still
            // holds open (write() may have succeeded once and not yet flushed, or
            // failed mid-write leaving the handle dirty) — an unparsable filename
            // is left alone rather than guessed at.
            let mut staging_purged = 0usize;
            let cache_dir = cache.safe_lock().cache_dir.clone();
            let open_fhs: std::collections::HashSet<u64> = open_files.safe_lock().keys().copied().collect();
            if let Ok(dir_entries) = std::fs::read_dir(&cache_dir) {
                for entry in dir_entries.flatten() {
                    let path = entry.path();
                    let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
                    let Some(fh_str) = name.strip_prefix("write_") else { continue };
                    if staged.contains(&path) {
                        continue;
                    }
                    let fh: u64 = match fh_str.parse() {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    if open_fhs.contains(&fh) {
                        continue;
                    }
                    match std::fs::remove_file(&path) {
                        Ok(()) => staging_purged += 1,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => log::warn!("purge: failed to remove orphaned staging file {}: {}", path.display(), e),
                    }
                }
            }

            // Drop in-memory directory listings so the next access re-PROPFINDs
            // fresh metadata rather than trusting possibly-stale cached sizes/etags.
            notify_push::invalidate_all_dirs(&cache, &dirty, &notifier_slot);
            log::info!(
                "purge: cleared {} cached file(s) ({} protected by pending upload), {} orphaned staging file(s)",
                purged, protected.len(), staging_purged
            );
            Ok(purged + staging_purged)
        })
    }

    pub fn prefetch_callback(&self) -> ipc::PrefetchCallback {
        let conn = self.conn.clone();
        let cache = self.cache.clone();
        Arc::new(move |remote_path| {
            prefetch_list_dir(&conn, &cache, &remote_path);
        })
    }

    pub fn thumbnail_callback(&self) -> ipc::ThumbnailCallback {
        let conn = self.conn.clone();
        let fileids = self.fileids.clone();
        let thumb_inflight = self.thumb_inflight.clone();
        Arc::new(move |remote_path: PathBuf| {
            let already_inflight = !thumb_inflight.safe_lock().insert(remote_path.clone());
            if !already_inflight {
                let fileid = fileids.safe_read().get(&remote_path).copied();
                let mount_path = conn.mount_point.join(
                    remote_path.strip_prefix("/").unwrap_or(&remote_path)
                );
                let mtime = std::fs::metadata(&mount_path).ok()
                    .and_then(|m| m.modified().ok());
                crate::preview::prefetch_thumbnail(
                    &conn.clients.get(),
                    &conn.base_url,
                    &conn.creds,
                    &conn.mount_point,
                    &remote_path,
                    mtime,
                    fileid,
                );
                thumb_inflight.safe_lock().remove(&remote_path);
            }
            let uri = crate::preview::file_uri(&conn.mount_point, &remote_path);
            crate::preview::xdg_thumbnail_path(&uri).exists()
        })
    }
}

/// Unifies the plain and "plus" directory replies so `readdir` and
/// `readdirplus` share one implementation. `readdirplus` bundles each entry's
/// attributes into the directory read, sparing the kernel a `lookup`+`getattr`
/// round-trip per file; the plain reply simply ignores the supplied attribute.
/// Entry attrs use TTL=0 so the kernel re-stats on open — dir-cache sizes can
/// lag a concurrent upload, and a stale size cached for 30 s would truncate reads.
enum DirReply {
    Plain(ReplyDirectory),
    Plus(ReplyDirectoryPlus),
}

impl DirReply {
    fn add(&mut self, ino: INodeNo, offset: u64, kind: FileType, name: &str, attr: &FileAttr) -> bool {
        match self {
            DirReply::Plain(r) => r.add(ino, offset, kind, name),
            // TTL=0 forces the kernel to re-stat each entry after listing;
            // dir-cache sizes can be stale (written before a concurrent upload
            // completes) so trusting them for 30 s would cause truncated reads.
            DirReply::Plus(r) => r.add(ino, offset, name, &Duration::ZERO, attr, Generation(0)),
        }
    }
    fn ok(self) {
        match self {
            DirReply::Plain(r) => r.ok(),
            DirReply::Plus(r) => r.ok(),
        }
    }
    fn error(self, err: Errno) {
        match self {
            DirReply::Plain(r) => r.error(err),
            DirReply::Plus(r) => r.error(err),
        }
    }
}

impl NextCloudFs {
    /// Called from write() once the tail staging file has accumulated at
    /// least one full CHUNK_SIZE of unsent bytes. Lazily opens the
    /// chunked-upload session on the very first graduation, PUTs as many full
    /// chunks as the tail currently holds (normally exactly one — FUSE writes
    /// are far smaller than CHUNK_SIZE, so the tail crosses the threshold by a
    /// small margin each time), and rewrites the tail file down to just the
    /// leftover bytes. Must be called without `open_files` locked: this
    /// performs network I/O, and that mutex guards every open handle in the
    /// mount, not just this one.
    ///
    /// On failure, returns the most recent server-confirmed session state
    /// alongside the error (rather than just discarding it) — including one
    /// opened by *this* call, if the MKCOL succeeded but a subsequent chunk
    /// PUT then failed. The caller must store it back into `of.chunk_upload`
    /// even on the error path, otherwise a freshly-opened session the caller
    /// never learns the `uploads_base` of leaks server-side: release()'s
    /// abandoned-session cleanup can only abort a session it knows about.
    fn graduate_chunk(
        &self,
        remote_path: &Path,
        wp: &Path,
        mut state: Option<ChunkUploadState>,
        total_written: u64,
    ) -> Result<ChunkUploadState, (String, Option<ChunkUploadState>)> {
        loop {
            let bytes_confirmed = state.as_ref().map_or(0, |s| s.bytes_confirmed);
            let tail_len = total_written - bytes_confirmed;
            if tail_len < webdav_ops::CHUNK_SIZE as u64 {
                return state.clone().ok_or((
                    "graduate_chunk called with nothing to graduate".to_string(),
                    state,
                ));
            }

            if state.is_none() {
                let session = retry_chunk_write("chunked-upload open", || {
                    self.conn.backend.open_chunked_upload(remote_path)
                }).map_err(|e| (e.to_string(), None))?;
                state = Some(ChunkUploadState {
                    uploads_base: session.uploads_base,
                    next_index: 0,
                    bytes_confirmed: 0,
                });
            }
            // Snapshot the session as the server last confirmed it, before
            // attempting this chunk — on failure below this is what the
            // caller needs to be able to find and abort the session later.
            let confirmed_state = state.clone();
            let s = state.as_mut().expect("just ensured Some above");

            let mut chunk = vec![0u8; webdav_ops::CHUNK_SIZE];
            {
                use std::io::Read;
                let mut f = std::fs::File::open(wp)
                    .map_err(|e| (format!("staging read: {}", e), confirmed_state.clone()))?;
                f.read_exact(&mut chunk)
                    .map_err(|e| (format!("staging read: {}", e), confirmed_state.clone()))?;
            }

            let session = backend::ChunkedUploadSession { uploads_base: s.uploads_base.clone() };
            let index = s.next_index;
            retry_chunk_write("chunk upload", || {
                self.conn.backend.put_chunk(&session, index, chunk.clone())
            }).map_err(|e| (e.to_string(), confirmed_state.clone()))?;
            let s = state.as_mut().expect("just ensured Some above");
            s.next_index += 1;
            s.bytes_confirmed += webdav_ops::CHUNK_SIZE as u64;

            shrink_tail_file(wp, webdav_ops::CHUNK_SIZE as u64)
                .map_err(|e| (format!("tail rewrite: {}", e), state.clone()))?;
        }
    }

    /// Journals the end of a streamed upload and hands it to a worker, so a failed finish
    /// is retried from the journal and release() never waits on the network.
    fn commit_streamed(
        &self,
        remote_path: PathBuf,
        tail_path: PathBuf,
        original_etag: Option<String>,
        opened_gen: u64,
        total_len: u64,
        cs: ChunkUploadState,
    ) -> Result<(), Errno> {
        log::info!(
            "[{}] COMMIT (streamed) {} size={} etag={:?}",
            self.log_user, remote_path.display(), total_len, original_etag,
        );
        if let Ok(f) = std::fs::File::open(&tail_path) {
            if let Err(e) = f.sync_all() {
                log::warn!("release: fsync tail {} failed: {}", tail_path.display(), e);
            }
        }
        let parent = remote_path.parent().unwrap_or(Path::new("/")).to_path_buf();
        {
            let mut c = self.cache.safe_lock();
            if let Some(dir) = c.dir_cache.get_mut(&parent) {
                let mut files = (*dir.files).clone();
                if let Some(e) = files.iter_mut().find(|e| e.path == remote_path) {
                    e.size = total_len;
                }
                dir.files = Arc::new(files);
            }
        }
        let seq = self.journal.safe_lock().enqueue(mutation_journal::MutationOp::FinishChunked {
            remote_path: remote_path.clone(),
            uploads_base: cs.uploads_base.clone(),
            next_index: cs.next_index,
            bytes_confirmed: cs.bytes_confirmed,
            total_len,
            tail_path: tail_path.clone(),
            if_match_etag: original_etag.clone(),
        });
        self.journal.safe_lock().supersede_uploads(&remote_path, seq);
        self.dirty.safe_lock().insert(remote_path.clone());
        if self.conn.is_offline.load(Ordering::Relaxed) {
            self.status.safe_write().insert(remote_path.clone(), FileStatus::PendingSync);
            return Ok(());
        }
        self.status.safe_write().insert(remote_path.clone(), FileStatus::Uploading);
        self.cache.safe_lock().uploading.insert(remote_path.clone());

        let conn = self.conn.clone();
        let cache = self.cache.clone();
        let dirty = self.dirty.clone();
        let elog = self.error_log.clone();
        let journal = self.journal.clone();
        let smap = self.status.clone();
        let uploads = self.uploads.clone();
        let ticket = uploads.ticket_entry(&remote_path);
        submit_mutation(move || {
            ticket.wait();
            if !journal.safe_lock().claim(seq) {
                log::debug!("streamed finish of {} skipped — superseded or replayed", remote_path.display());
                if !journal.safe_lock().has_pending_put(&remote_path) {
                    cache.safe_lock().uploading.remove(&remote_path);
                }
                return;
            }
            let etag = uploads.etag_for(&remote_path, opened_gen, original_etag);
            let _permit = conn.throttle.acquire();
            let result = mutation_journal::finish_chunked(
                &*conn.backend, &cs.uploads_base, cs.next_index, cs.bytes_confirmed, total_len,
                &tail_path, &remote_path, etag.as_deref(),
            );
            cache.safe_lock().uploading.remove(&remote_path);
            match result {
                Ok(result) => {
                    log::info!("PUT (streamed) {} → new etag {:?}", remote_path.display(), result.new_change_token);
                    uploads.record(&remote_path, result.new_change_token.clone());
                    {
                        let mut c = cache.safe_lock();
                        if let Some(dir) = c.dir_cache.get_mut(&parent) {
                            let mut files = (*dir.files).clone();
                            if let Some(entry) = files.iter_mut().find(|e| e.path == remote_path) {
                                entry.change_token = result.new_change_token;
                                entry.size = total_len;
                                entry.modified = Some(SystemTime::now());
                            }
                            dir.files = Arc::new(files);
                            dir.at = Instant::now() - (DIR_CACHE_TTL + Duration::from_secs(1));
                        }
                    }
                    // The tail is only the end of the file, so it can never become a kept copy.
                    smap.safe_write().insert(remote_path.clone(), FileStatus::Synced);
                    let _ = std::fs::remove_file(&tail_path);
                    journal.safe_lock().remove(seq);
                }
                Err(backend::BackendWriteError::Conflict) => {
                    // Every chunk is already on the server: assemble ours as a conflicted copy.
                    let conflict_name = make_conflict_name(&remote_path);
                    let session = backend::ChunkedUploadSession { uploads_base: cs.uploads_base.clone() };
                    match conn.backend.finish_chunked_upload(&session, &conflict_name, None) {
                        Ok(_) => log::info!("conflicted copy assembled as {}", conflict_name.display()),
                        Err(e) => log::error!("failed to assemble conflicted copy {}: {}", conflict_name.display(), e),
                    }
                    push_error(&elog, remote_path.clone(), SyncErrorKind::Conflict, "Server version changed — conflicted copy created".into());
                    smap.safe_write().remove(&remote_path);
                    let _ = std::fs::remove_file(&tail_path);
                    journal.safe_lock().remove(seq);
                }
                Err(ref e) if e.is_transient() => {
                    if e.is_network_down() {
                        mark_offline(&conn.is_offline, &conn.offline_since);
                    }
                    log::warn!("streamed PUT {} deferred — {} (queued for retry)", remote_path.display(), e);
                    smap.safe_write().insert(remote_path.clone(), FileStatus::PendingSync);
                    journal.safe_lock().mark_deferred(seq, e.to_string());
                }
                Err(e) => {
                    log::error!("streamed PUT {} failed at finish: {}", remote_path.display(), e);
                    let kind = match &e {
                        backend::BackendWriteError::Forbidden => SyncErrorKind::PermissionDenied,
                        backend::BackendWriteError::QuotaExceeded => SyncErrorKind::QuotaExceeded,
                        backend::BackendWriteError::Server(code, _) => SyncErrorKind::ServerError(*code),
                        _ => SyncErrorKind::UploadFailed,
                    };
                    push_error(&elog, remote_path.clone(), kind, format!("Streamed upload could not complete — please retry the copy: {}", e));
                    smap.safe_write().insert(remote_path.clone(), FileStatus::PendingSync);
                    if matches!(e, backend::BackendWriteError::Server(404, _)) {
                        // The session is gone; nothing left to retry from.
                        conn.backend.abort_chunked_upload(&backend::ChunkedUploadSession { uploads_base: cs.uploads_base.clone() });
                        let _ = std::fs::remove_file(&tail_path);
                        journal.safe_lock().remove(seq);
                    } else {
                        journal.safe_lock().mark_failed(seq, e.to_string());
                    }
                }
            }
            dirty.safe_lock().insert(remote_path.clone());
            dirty.safe_lock().insert(parent);
        });
        Ok(())
    }

    /// Uploads a handle's staged bytes once release() has removed it. FLUSH is sent on
    /// every close() of any fd sharing the handle, so only RELEASE marks the end of a file.
    fn commit_released(&self, fh: FileHandle, of: OpenFile) -> Result<(), Errno> {
        let OpenFile { remote_path, write_path, original_etag, chunk_upload, total_written, opened_gen, .. } = of;
        let Some(write_path) = write_path else { return Ok(()) };
        if let Some(chunk_state) = chunk_upload {
            return self.commit_streamed(remote_path, write_path, original_etag, opened_gen, total_written, chunk_state);
        }
        let upload_size = match std::fs::metadata(&write_path) {
            Ok(m) => m.len(),
            Err(_) => {
                log::error!("release: staging file missing at {}", write_path.display());
                return Err(Errno::EIO);
            }
        };
        log::info!("[{}] COMMIT {} size={} etag={:?}", self.log_user, remote_path.display(), upload_size, original_etag);

        // Durability: force the staged bytes to stable storage BEFORE the PUT is
        // recorded in the journal. The write() handler opens the staging file per
        // call without fsync, so without this a crash or power loss could leave a
        // journal entry pointing at a staging file whose contents never reached
        // disk — the local edit would be silently lost on recovery. fsync failure
        // is non-fatal: the save still succeeds, we just log that durability could
        // not be guaranteed.
        if let Ok(f) = std::fs::File::open(&write_path) {
            if let Err(e) = f.sync_all() {
                log::warn!("release: fsync staging {} failed: {}", write_path.display(), e);
            }
        }

        // Update dir_cache size synchronously so getattr returns the correct size
        // before the background PUT thread has a chance to run.
        {
            let mut c = self.cache.safe_lock();
            let parent = remote_path.parent().unwrap_or(Path::new("/")).to_path_buf();
            if let Some(dir) = c.dir_cache.get_mut(&parent) {
                let mut files = (*dir.files).clone();
                if let Some(e) = files.iter_mut().find(|e| e.path == remote_path) {
                    e.size = upload_size;
                }
                dir.files = Arc::new(files);
            }
        }

        let seq = self.journal.safe_lock().enqueue(
            mutation_journal::MutationOp::Put {
                remote_path: remote_path.clone(),
                staging_path: write_path.clone(),
                if_match_etag: original_etag.clone(),
            },
        );
        self.journal.safe_lock().supersede_uploads(&remote_path, seq);

        if !self.conn.is_offline.load(Ordering::Relaxed) {
            self.status.safe_write().insert(remote_path.clone(), FileStatus::Uploading);
            self.dirty.safe_lock().insert(remote_path.clone());
        } else {
            // Offline: the edit is saved locally and journaled; show it as pending
            // sync until connectivity returns and the queued PUT is replayed.
            self.status.safe_write().insert(remote_path.clone(), FileStatus::PendingSync);
            self.dirty.safe_lock().insert(remote_path.clone());
        }

        if !self.conn.is_offline.load(Ordering::Relaxed) {
            let conn = self.conn.clone();
            let cache = self.cache.clone();
            let dirty = self.dirty.clone();
            let open_files = self.open_files.clone();
            let elog = self.error_log.clone();
            let tmap = self.transfer_map.clone();
            let journal = self.journal.clone();
            let smap = self.status.clone();
            let auto_keep = self.auto_keep_locally_modified_files;

            // Guard this path in the uploading set so put_dir_cache doesn't
            // evict it from a concurrent PROPFIND refresh before the PUT lands.
            self.cache.safe_lock().uploading.insert(remote_path.clone());
            let uploads = self.uploads.clone();
            let ticket = uploads.ticket_entry(&remote_path);

            submit_mutation(move || {
                ticket.wait();
                if !journal.safe_lock().claim(seq) {
                    log::debug!("PUT {} skipped — superseded or replayed", remote_path.display());
                    if !journal.safe_lock().has_pending_put(&remote_path) {
                        cache.safe_lock().uploading.remove(&remote_path);
                    }
                    return;
                }
                let original_etag = uploads.etag_for(&remote_path, opened_gen, original_etag);
                let _permit = conn.throttle.acquire();
                tmap.safe_lock().insert(whole_file_transfer(&remote_path), TransferProgress {
                    path: remote_path.clone(),
                    direction: TransferDirection::Upload,
                    bytes_done: 0,
                    total_bytes: upload_size,
                });
                let etag_ref = original_etag.as_deref();
                match conn.backend.put_file_from_path(&remote_path, &write_path, etag_ref) {
                    Ok(result) => {
                        tmap.safe_lock().remove(&whole_file_transfer(&remote_path));
                        cache.safe_lock().uploading.remove(&remote_path);
                        log::info!("PUT {} → new etag {:?}", remote_path.display(), result.new_change_token);
                        uploads.record(&remote_path, result.new_change_token.clone());
                        let new_size = upload_size;
                        {
                            let mut c = cache.safe_lock();
                            let parent = remote_path.parent().unwrap_or(Path::new("/")).to_path_buf();
                            if let Some(dir) = c.dir_cache.get_mut(&parent) {
                                let mut files = (*dir.files).clone();
                                if let Some(entry) = files.iter_mut().find(|e| e.path == remote_path) {
                                    entry.change_token = result.new_change_token.clone();
                                    entry.size = new_size;
                                    entry.modified = Some(SystemTime::now());
                                }
                                dir.files = Arc::new(files);
                                // Expire the cache so the next readdir triggers a PROPFIND
                                // and populates NC-assigned properties (permissions, fileid, owner).
                                dir.at = Instant::now() - (DIR_CACHE_TTL + Duration::from_secs(1));
                            }
                        }
                        if let Some(of) = open_files.safe_lock().get_mut(&fh.0) {
                            of.dirty = false;
                            of.original_etag = result.new_change_token.clone();
                        }
                        if auto_keep {
                            let rel = remote_path.strip_prefix("/").unwrap_or(&remote_path);
                            let keep_path = cache.safe_lock().kept_dir.join(rel);
                            let mut kept = false;
                            if let Some(parent) = keep_path.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            if std::fs::copy(&write_path, &keep_path).is_ok() {
                                cache.safe_lock().file_cache.insert(remote_path.clone(), FileCacheEntry {
                                    local_path: keep_path,
                                    remote_modified: Some(SystemTime::now()),
                                    etag: result.new_change_token,
                                    kept: true,
                                    size: upload_size,
                                });
                                smap.safe_write().insert(remote_path.clone(), FileStatus::Kept);
                                kept = true;
                            }
                            if !kept {
                                smap.safe_write().insert(remote_path.clone(), FileStatus::Synced);
                            }
                        } else {
                            smap.safe_write().insert(remote_path.clone(), FileStatus::Synced);
                        }
                        let _ = std::fs::remove_file(&write_path);
                        dirty.safe_lock().insert(remote_path.clone());
                        dirty.safe_lock().insert(remote_path.parent().unwrap_or(Path::new("/")).to_path_buf());
                        journal.safe_lock().remove(seq);
                    }
                    Err(backend::BackendWriteError::Conflict) => {
                        tmap.safe_lock().remove(&whole_file_transfer(&remote_path));
                        cache.safe_lock().uploading.remove(&remote_path);
                        smap.safe_write().remove(&remote_path);
                        log::warn!("CONFLICT on PUT {} — creating conflicted copy", remote_path.display());
                        push_error(&elog, remote_path.clone(), SyncErrorKind::Conflict, "Server version changed — conflicted copy created".into());
                        let conflict_name = make_conflict_name(&remote_path);
                        match conn.backend.put_file_from_path(&conflict_name, &write_path, None) {
                            Ok(_) => log::info!("conflicted copy uploaded as {}", conflict_name.display()),
                            Err(e) => log::error!("failed to upload conflict copy: {}", e),
                        }
                        if let Some(of) = open_files.safe_lock().get_mut(&fh.0) {
                            of.dirty = false;
                        }
                        dirty.safe_lock().insert(remote_path.clone());
                        dirty.safe_lock().insert(remote_path.parent().unwrap_or(Path::new("/")).to_path_buf());
                        journal.safe_lock().remove(seq);
                        let _ = std::fs::remove_file(&write_path);
                    }
                    Err(ref e) => {
                        tmap.safe_lock().remove(&whole_file_transfer(&remote_path));
                        cache.safe_lock().uploading.remove(&remote_path);
                        // Keep the local edit: the staging file and journal entry stay put,
                        // so the content survives and the mutation is retried. Surface it as
                        // PendingSync rather than dropping the status, so the UI shows the
                        // file is saved locally but not yet on the server.
                        smap.safe_write().insert(remote_path.clone(), FileStatus::PendingSync);
                        if e.is_transient() {
                            // Server down/overloaded/timed out or resource locked. Retry
                            // indefinitely (no attempt-budget cost) with no user-facing
                            // error — the PendingSync marker already conveys the state, and
                            // the local edit stays safely staged until the server is back.
                            //
                            // Flip offline NOW rather than waiting up to 30s for the
                            // connectivity monitor's next poll: this upload just proved the
                            // network is down, and a save is typically a burst of ops
                            // (write→flush plus read-modify-write reads). Flagging offline
                            // here makes every following op in the same save take the
                            // instant cache/journal path instead of each blocking on its own
                            // connect timeout. The monitor re-probes every 5s while offline
                            // and clears the flag (and replays the journal) once the server
                            // is back, so a brief hiccup self-heals quickly.
                            if e.is_network_down() {
                                mark_offline(&conn.is_offline, &conn.offline_since);
                            }
                            log::warn!("PUT {} deferred — {} (queued for retry)", remote_path.display(), e);
                            journal.safe_lock().mark_deferred(seq, e.to_string());
                        } else {
                            // Permanent — the server refuses this write (permission, quota,
                            // malformed) and retrying cannot help. Flag it so the user can
                            // act; still keep the local copy staged and let the journal's
                            // attempt budget decide when to give up.
                            let kind = match e {
                                backend::BackendWriteError::Forbidden => SyncErrorKind::PermissionDenied,
                                backend::BackendWriteError::QuotaExceeded => SyncErrorKind::QuotaExceeded,
                                backend::BackendWriteError::Server(code, _) => SyncErrorKind::ServerError(*code),
                                _ => SyncErrorKind::UploadFailed,
                            };
                            log::error!("PUT {} failed permanently: {}", remote_path.display(), e);
                            push_error(&elog, remote_path.clone(), kind, e.to_string());
                            journal.safe_lock().mark_failed(seq, e.to_string());
                        }
                        dirty.safe_lock().insert(remote_path.clone());
                    }
                }
            });
        }
        Ok(())
    }

    /// Cached listing for `dir`, re-listing once if it is not resident.
    ///
    /// The dir cache is bounded, so a listing an inode was handed out from can
    /// be gone by the time the kernel asks about that inode again. Every caller
    /// that used to treat a miss as "does not exist" has to re-list first.
    fn relist_if_missing(&self, dir: &Path) -> Option<Arc<Vec<RemoteEntry>>> {
        self.parent_listing(dir, None).ok()
    }

    /// The listing of `dir` to answer a child lookup from: the cached one if
    /// resident, else whatever a (possibly joined, still streaming) fetch
    /// returns. When `want` is named and not in a partial listing yet, waits —
    /// up to `PROPFIND_TIMEOUT` — for the stream to finish before answering.
    ///
    /// Reading the finished-listing cache after `get_or_list_dir` (what this
    /// used to do) missed every listing still streaming: `find` read a wide
    /// directory from the partial snapshot, then got ENOENT stat'ing a child
    /// it had just been shown.
    fn parent_listing(&self, dir: &Path, want: Option<&str>) -> Result<Arc<Vec<RemoteEntry>>, Option<String>> {
        if let Some(files) = self.cache.safe_lock().get_cached_dir_readonly(dir) {
            return Ok(files);
        }
        let files = match get_or_list_dir(&self.conn, &self.cache, dir.to_path_buf(), None) {
            Ok((files, _)) => files,
            Err(e) => {
                log::debug!("re-list {} failed: {}", dir.display(), e);
                // A listing may have landed meanwhile (another reader's fetch).
                return self.cache.safe_lock().get_cached_dir_readonly(dir).ok_or(Some(e));
            }
        };
        let Some(name) = want else { return Ok(files) };
        let has = |f: &[RemoteEntry]| f.iter().any(|e| e.path.file_name().and_then(|n| n.to_str()) == Some(name));
        if has(&files) {
            return Ok(files);
        }
        let deadline = Instant::now() + PROPFIND_TIMEOUT;
        let notify = self.cache.safe_lock().pending_notify.clone();
        loop {
            {
                let mut c = self.cache.safe_lock();
                if let Some(done) = c.get_cached_dir_readonly(dir) {
                    return Ok(done);
                }
                match c.get_pending_snapshot(dir) {
                    Ok(Some(partial)) if has(&partial) => return Ok(Arc::new(partial)),
                    Ok(Some(_)) => {}
                    // No fetch in flight any more and nothing cached: the partial
                    // listing we got is all there is.
                    Ok(None) => return Ok(files),
                    Err(e) => return Err(Some(e)),
                }
            }
            if Instant::now() >= deadline {
                return Ok(files);
            }
            let guard = notify.0.lock().unwrap_or_else(|e| e.into_inner());
            let _ = notify.1.wait_timeout(guard, Duration::from_millis(50));
        }
    }

    fn readdir_common(&self, pid: u32, ino: INodeNo, fh: u64, offset: u64, reply: DirReply) {
        let (path, parent_ino) = {
            let c = self.cache.safe_lock();
            let path = match c.get_path(ino.0) {
                Some(p) => p,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            };
            let parent_ino = if ino.0 == 1 {
                1
            } else {
                let parent = path.parent().unwrap_or(Path::new("/"));
                c.get_inode(parent).unwrap_or(1)
            };
            (path, parent_ino)
        };

        log::info!("[{}] READDIR {}", self.log_user, path.display());

        let cache = self.cache.clone();
        let status = self.status.clone();
        let shared = self.shared.clone();
        let fileids = self.fileids.clone();
        let details = self.details.clone();
        let children_map = self.children_map.clone();
        let dirty = self.dirty.clone();
        let conn = self.conn.clone();
        let aggressive_prefetch = self.aggressive_prefetch;
        let exclude_folders = self.exclude_folders.clone();
        let thumb_inflight = self.thumb_inflight.clone();
        let cleanup_stale_gio_temps = self.cleanup_stale_gio_temps;
        let elog = self.error_log.clone();
        let notifier_slot = self.notifier_slot.clone();
        let ghost_entries = self.ghost_entries.clone();
        let refresh_debounce = self.refresh_debounce.clone();
        let file_change_queue = self.file_change_queue.clone();
        let open_dirs = self.open_dirs.clone();

        // A bounded pool worker, with the reply travelling in the job: fuser's one
        // dispatch thread never blocks on a directory's network round trip, and
        // a crawl can queue at most `bg::READDIR` workers + its queue — not the
        // one-thread-per-call (then sleep-polling for a permit) it used to.
        let submitted = bg::READDIR.submit_owning(reply, move |mut reply| {
            if offset == 0 {
                let dot_attr = make_dir_attr(ino.0);
                if reply.add(ino, 1, FileType::Directory, ".", &dot_attr) {
                    reply.ok();
                    return;
                }
                let dotdot_attr = make_dir_attr(parent_ino);
                if reply.add(INodeNo(parent_ino), 2, FileType::Directory, "..", &dotdot_attr) {
                    reply.ok();
                    return;
                }
            }

            // Continuation pages (offset > 0): serve directly from cache, skip all heavy work.
            if offset > 0 {
                let skip = (offset - 2) as usize;
                // Page out of the listing the offset-0 page was built from. The
                // offsets the kernel is resuming index *that* vector, and the dir
                // cache is bounded, so by now it may have been evicted or
                // replaced by a refresh. Serving an empty page on a miss (what
                // this did before) silently truncated the directory to whatever
                // the first page had already delivered — the kernel reads an
                // empty page as end-of-directory.
                let pinned = {
                    let od = open_dirs.safe_lock();
                    od.get(&fh).filter(|d| d.path == path).and_then(|d| d.snapshot.clone())
                };
                let entries = match pinned {
                    Some(e) => Some(e),
                    // No pinned snapshot (a handle whose offset-0 page we never
                    // served, e.g. after a rewinddir): fall back to the cache,
                    // re-listing if it is gone.
                    None => match cache.safe_lock().get_cached_dir_readonly(&path) {
                        Some(e) => Some(e),
                        None => match get_or_list_dir(&conn, &cache, path.clone(), None) {
                            Ok((files, _)) => Some(files),
                            Err(e) => {
                                log::warn!("readdir continuation {}: {}", path.display(), e);
                                None
                            }
                        },
                    },
                };
                // Collect under a short lock, then reply outside it so concurrent
                // getattr/lookup calls aren't blocked while the kernel drains pages.
                let rows: Vec<(INodeNo, u64, FileType, String, FileAttr)> = match entries {
                    Some(entries) => {
                        let mut c = cache.safe_lock();
                        // Filtered then enumerated, so the indices match the
                        // offset-0 page's — it enumerates a listing the GIO temps
                        // have already been dropped from.
                        entries.iter()
                            .filter(|e| {
                                let name = e.path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                                !is_gio_temp_file(name)
                            })
                            .enumerate()
                            .skip(skip)
                            .filter_map(|(i, entry)| {
                                let name = entry.path.file_name().and_then(|n| n.to_str())?.to_string();
                                let entry_path = path.join(&name);
                                if entry.is_dir && exclude_folders.contains(&entry_path) { return None; }
                                // Allocate rather than `get_inode(..).unwrap_or(1)`:
                                // inodes are assigned on demand, and a page served
                                // without its offset-0 leg (a re-list after
                                // eviction) can be the first to name these paths.
                                // Falling back to 1 would hand the kernel the root
                                // inode for a child entry.
                                let ino = c.allocate_inode(entry_path.clone());
                                let kind = if entry.is_dir { FileType::Directory } else { FileType::RegularFile };
                                let attr = make_file_attr(ino, entry);
                                Some((INodeNo(ino), (i + 3) as u64, kind, name, attr))
                            })
                            .collect()
                    }
                    None => vec![],
                };
                for (ino, off, kind, name, attr) in rows {
                    if reply.add(ino, off, kind, &name, &attr) { break; }
                }
                reply.ok();
                return;
            }

            // A listing we'll have to fetch counts against the requesting
            // process's budget; a walker over it waits here, on this pool
            // worker — never on the dispatch thread, never for cached folders.
            let cold = cache.safe_lock().dir_cache.get(&path).is_none_or(|e| e.invalidated || e.hard_expired);
            if cold {
                let wait = conn.walkers.note_uncached(pid, Instant::now());
                if !wait.is_zero() {
                    thread::sleep(wait);
                }
            }

            let t_readdir = Instant::now();
            match get_or_list_dir(&conn, &cache, path.clone(), Some(DirDetailArcs {
                shared: shared.clone(),
                fileids: fileids.clone(),
                details: details.clone(),
                children: children_map.clone(),
                dirty: dirty.clone(),
            })) {
                Ok((entries, self_entry)) => {
                    log::info!("READDIR {} get_or_list_dir returned {} entries in {:?}", path.display(), entries.len(), t_readdir.elapsed());

                    // Cached listings are served for speed, not trusted for
                    // correctness: every read also probes the server etag in the
                    // background and, on mismatch, refreshes + notifies. Catches
                    // changes made while this client was offline, for which no
                    // notify-push event will ever arrive.
                    if !conn.is_offline.load(Ordering::Relaxed) && !conn.paused.load(Ordering::Relaxed)
                        && !conn.walkers.is_walker(pid, Instant::now())
                    {
                        notify_push::revalidate_dir_on_read(
                            &path, &conn.backend, &cache, &dirty, &conn.active_streams,
                            &conn.throttle, &notifier_slot, &refresh_debounce,
                            &ghost_entries, &file_change_queue,
                            &notify_push::ServerHealth { breaker: conn.breaker.clone(), backoff: conn.backoff.clone() },
                        );
                    }

                    // GIO writes files atomically via a .goutputstream-* / .xdp-* temp
                    // that is renamed to the final name within seconds. Any such file
                    // visible in a PROPFIND is either an orphan (app crashed) or is about
                    // to be renamed imminently. Delete them from the server immediately
                    // (when the flag is on) and never include them in the listing.
                    let gio_temps: Vec<PathBuf> = entries.iter()
                        .filter(|e| {
                            let name = e.path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                            is_gio_temp_file(name)
                        })
                        .map(|e| e.path.clone())
                        .collect();
                    if !gio_temps.is_empty() {
                        if cleanup_stale_gio_temps {
                            log::info!("purging {} GIO temp(s) in {}", gio_temps.len(), path.display());
                            let conn2 = conn.clone();
                            let _ = bg::BACKGROUND.submit(move || {
                                for p in gio_temps {
                                    if let Err(e) = conn2.backend.delete(&p) {
                                        log::debug!("GIO temp delete {}: {}", p.display(), e);
                                    }
                                }
                            });
                        }
                    }
                    let entries: Arc<Vec<RemoteEntry>> = Arc::new(entries.iter()
                        .filter(|e| {
                            let name = e.path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                            !is_gio_temp_file(name)
                        })
                        .cloned()
                        .collect());
                    // Pin it to this handle: the kernel will come back for the
                    // rest of the listing at offsets that index this exact
                    // vector, and nothing else keeps it alive.
                    {
                        let mut od = open_dirs.safe_lock();
                        if let Some(d) = od.get_mut(&fh) {
                            if d.path == path {
                                d.snapshot = Some(Arc::clone(&entries));
                            }
                        }
                    }

                    // Kick off background PROPFINDs for child dirs (opt-in via
                    // aggressive_prefetch; off by default until the serialisation
                    // issues in proactive_refresh / poll loop are fixed).
                    if aggressive_prefetch && offset == 0 {
                        for entry in entries.iter().filter(|e| e.is_dir) {
                            if let Some(name) = entry.path.file_name().and_then(|n| n.to_str()) {
                                start_background_propfind(&conn, &cache, path.join(name), PREFETCH_CHAIN_DEPTH);
                            }
                        }
                    }

                    let mut thumb_candidates: Vec<(PathBuf, Option<SystemTime>, bool, Option<u64>)> = Vec::new();

                    // Build per-entry metadata vectors and allocate inodes.
                    // Map updates (retain+insert on shared/fileids/details/status) are
                    // deferred to after reply.ok() so the kernel gets the directory listing
                    // back without waiting for O(total_cached) retain() scans.
                    let (shared_paths, fileid_paths, detail_entries, status_entries, cache_entries) = {
                        let (kept_dir, auto_cache_dir) = {
                            let mut c = cache.safe_lock();
                            for entry in entries.iter() {
                                if let Some(name) = entry.path.file_name().and_then(|n| n.to_str()) {
                                    c.allocate_inode(path.join(name));
                                }
                            }
                            (c.kept_dir.clone(), c.auto_cache_dir.clone())
                        };

                        let mut shared_paths = Vec::new();
                        let mut fileid_paths = Vec::new();
                        let mut detail_entries = Vec::new();
                        let mut status_entries = Vec::new();
                        let mut cache_entries = Vec::new();

                        if let Some(ref se) = self_entry {
                            if se.ext.flag("is_shared") { shared_paths.push(path.clone()); }
                            if let Some(fid) = se.ext.int("fileid") { fileid_paths.push((path.clone(), fid)); }
                            detail_entries.push((path.clone(), ipc::FileDetail {
                                permissions: se.ext.str("permissions").map(str::to_string),
                                owner_id: se.ext.str("owner_id").map(str::to_string),
                                owner_display_name: se.ext.str("owner_display_name").map(str::to_string),
                                size: se.size,
                                is_dir: se.is_dir,
                            }));
                        }

                        for entry in entries.iter() {
                            let name = match entry.path.file_name().and_then(|n| n.to_str()) {
                                Some(n) => n,
                                None => continue,
                            };
                            let entry_path = path.join(name);
                            if entry.ext.flag("is_shared") { shared_paths.push(entry_path.clone()); }
                            if let Some(fid) = entry.ext.int("fileid") { fileid_paths.push((entry_path.clone(), fid)); }
                            detail_entries.push((entry_path.clone(), ipc::FileDetail {
                                permissions: entry.ext.str("permissions").map(str::to_string),
                                owner_id: entry.ext.str("owner_id").map(str::to_string),
                                owner_display_name: entry.ext.str("owner_display_name").map(str::to_string),
                                size: entry.size,
                                is_dir: entry.is_dir,
                            }));
                            if !entry.is_dir {
                                let rel = entry_path.strip_prefix("/").unwrap_or(&entry_path);
                                let kept_path = kept_dir.join(rel);
                                let cached_path = auto_cache_dir.join(rel);
                                // Lazy: only stat cached_path if kept_path misses — most
                                // files are Remote so this saves one stat(2) per entry.
                                if let Some(km) = kept_path.metadata().ok().filter(|m| m.len() > 0) {
                                    status_entries.push((entry_path.clone(), FileStatus::Kept));
                                    cache_entries.push((entry_path.clone(), FileCacheEntry {
                                        local_path: kept_path,
                                        remote_modified: entry.modified,
                                        etag: entry.change_token.clone(),
                                        kept: true,
                                        size: km.len(),
                                    }));
                                } else if let Some(cm) = cached_path.metadata().ok().filter(|m| m.len() > 0) {
                                    status_entries.push((entry_path.clone(), FileStatus::Cached));
                                    cache_entries.push((entry_path.clone(), FileCacheEntry {
                                        local_path: cached_path,
                                        remote_modified: entry.modified,
                                        etag: entry.change_token.clone(),
                                        kept: false,
                                        size: cm.len(),
                                    }));
                                } else {
                                    status_entries.push((entry_path.clone(), FileStatus::Remote));
                                }
                                // The synthetic `.trackerignore` overlay entry has no
                                // backend fileid to request a preview for.
                                if entry_path != trackerignore_path() {
                                    thumb_candidates.push((entry_path, entry.modified, entry.ext.flag("has_preview"), entry.ext.int("fileid")));
                                }
                            }
                        }

                        (shared_paths, fileid_paths, detail_entries, status_entries, cache_entries)
                    };

                    // Collect inode/kind/name under a short lock, then call reply.add()
                    // outside it so concurrent getattr/lookup aren't blocked for all N entries.
                    let reply_rows: Vec<(INodeNo, u64, FileType, String, FileAttr)> = {
                        let c = cache.safe_lock();
                        entries.iter().enumerate().filter_map(|(i, entry)| {
                            let name = entry.path.file_name().and_then(|n| n.to_str())?.to_string();
                            let entry_path = path.join(&name);
                            if entry.is_dir && exclude_folders.contains(&entry_path) { return None; }
                            let ino = c.get_inode(&entry_path).unwrap_or(1);
                            let kind = if entry.is_dir { FileType::Directory } else { FileType::RegularFile };
                            let attr = make_file_attr(ino, entry);
                            Some((INodeNo(ino), (i + 3) as u64, kind, name, attr))
                        }).collect()
                    };
                    for (ino, off, kind, name, attr) in reply_rows {
                        if reply.add(ino, off, kind, &name, &attr) { break; }
                    }

                    // Pre-populate detail/shared/fileid maps before reply.ok() so that
                    // Nautilus extension DETAIL queries arriving immediately after the
                    // kernel receives the listing find fresh data. No eviction here;
                    // the post-reply block uses the ChildrenMap index for O(old_dir_size)
                    // targeted removal of stale entries.
                    {
                        let mut sh = shared.safe_write();
                        for p in &shared_paths { sh.insert(p.clone()); }
                    }
                    {
                        let mut fi = fileids.safe_write();
                        for (p, fid) in &fileid_paths { fi.insert(p.clone(), *fid); }
                    }
                    {
                        let mut dt = details.safe_write();
                        for (p, d) in &detail_entries { dt.insert(p.clone(), d.clone()); }
                    }

                    log::info!("READDIR {} reply.ok() at {:?}", path.display(), t_readdir.elapsed());
                    reply.ok();
                    schedule_save_dir_cache(&cache);

                    // ── Map updates (after reply.ok()) ────────────────────────────────
                    // The pre-reply block already inserted all fresh entries into shared,
                    // fileids, and details. Here we only need to evict entries that are in
                    // the OLD child set but absent from the fresh PROPFIND (truly deleted
                    // files), and rebuild children_map[path] atomically with details.
                    {
                        // V7: take the old child set without cloning — swaps in an empty
                        // HashSet, transfers ownership of the old set, no heap allocation.
                        let old_child_set: HashSet<PathBuf> = {
                            let mut cm = children_map.safe_write();
                            cm.get_mut(&path).map(std::mem::take).unwrap_or_default()
                        };

                        // Build fresh direct-child set from PROPFIND result.
                        let new_set: HashSet<PathBuf> = detail_entries
                            .iter()
                            .filter_map(|(p, _)| {
                                if p.parent() == Some(path.as_path()) { Some(p.clone()) } else { None }
                            })
                            .collect();

                        // V6: evict only truly stale children (deleted/moved away).
                        // Fresh entries are already in the maps from the pre-reply block.
                        {
                            let mut sh = shared.safe_write();
                            for p in &old_child_set { if !new_set.contains(p) { sh.remove(p); } }
                        }
                        {
                            let mut fi = fileids.safe_write();
                            for p in &old_child_set { if !new_set.contains(p) { fi.remove(p); } }
                        }
                        // details + children_map: evict stale, rebuild children_map atomically.
                        // Combined scope prevents DETAILDIR from observing fresh detail_map
                        // paired with stale children_map. Lock order (dt before cm) matches
                        // apply_dir_detail_maps to prevent deadlock.
                        {
                            let mut dt = details.safe_write();
                            let mut cm = children_map.safe_write();
                            for p in &old_child_set { if !new_set.contains(p) { dt.remove(p); } }
                            let entry = cm.entry(path.clone())
                                .or_insert_with(std::collections::HashSet::new);
                            // Keep concurrent lookup() insertions (not in old_child_set)
                            // and surviving files (in both old and new). Add fresh entries.
                            entry.retain(|p| !old_child_set.contains(p) || new_set.contains(p));
                            entry.extend(new_set);
                        }
                        // Collect paths with in-flight or just-completed upload status before
                        // we clear smap.  These must (a) survive the PROPFIND status reset so
                        // the upload emblem persists, and (b) be dirtied individually so
                        // Nautilus re-queries their NC properties from the fresh detail_map.
                        let upload_paths: Vec<PathBuf> = {
                            let st = status.safe_read();
                            status_entries.iter()
                                .filter_map(|(p, _)| {
                                    if matches!(st.get(p),
                                        Some(FileStatus::Uploading) | Some(FileStatus::Synced))
                                    {
                                        Some(p.clone())
                                    } else {
                                        None
                                    }
                                })
                                .collect()
                        };
                        // status: evict dir + old children (O(old_dir_size)), re-insert fresh
                        {
                            let mut st = status.safe_write();
                            st.remove(&path);
                            for p in &old_child_set { st.remove(p); }
                            let upload_set: std::collections::HashSet<&PathBuf> =
                                upload_paths.iter().collect();
                            for (p, s) in &status_entries {
                                // Don't overwrite Uploading/Synced with Remote: those entries
                                // track in-flight and just-completed uploads.
                                if !upload_set.contains(p) {
                                    st.insert(p.clone(), *s);
                                }
                            }
                        }
                        {
                            let mut c = cache.safe_lock();
                            for (p, fc) in cache_entries { c.file_cache.entry(p).or_insert(fc); }
                        }
                        {
                            // Insert only the directory itself; children get their own
                            // dirty notifications via notify_push when they change.
                            // Inserting all N children here floods the CHANGES queue and
                            // triggers a cascade of GIO attribute invalidations in Nautilus.
                            let mut d = dirty.safe_lock();
                            d.insert(path.clone());
                            // Additionally dirty recently uploaded files so Nautilus re-queries
                            // them and picks up NC properties from the freshly populated detail_map.
                            for p in upload_paths {
                                d.insert(p);
                            }
                        }
                    }

                    // Thumbnails are the most expendable work there is: skip them
                    // outright while the server is failing requests.
                    if offset == 0 && !thumb_candidates.is_empty() && !conn.breaker.is_open(Instant::now())
                        && !conn.walkers.is_walker(pid, Instant::now())
                    {
                        let already = {
                            let mut inf = thumb_inflight.safe_lock();
                            !inf.insert(path.clone())
                        };
                        if !already {
                            let conn2 = conn.clone();
                            let (inflight2, path2) = (thumb_inflight.clone(), path.clone());
                            let submitted = bg::THUMB.submit(move || {
                                thread::sleep(Duration::from_millis(200));
                                preview::prefetch_directory_thumbnails(
                                    &conn2.clients.get(),
                                    &conn2.base_url,
                                    &conn2.creds,
                                    &conn2.mount_point,
                                    &thumb_candidates,
                                    &conn2.active_streams,
                                );
                                inflight2.safe_lock().remove(&path2);
                            });
                            if submitted.is_err() {
                                thumb_inflight.safe_lock().remove(&path);
                            }
                        }
                    }
                }
                Err(e) => {
                    let server_code = backend::server_error_code(&e);
                    let kind = match server_code {
                        Some(401 | 403) => SyncErrorKind::PermissionDenied,
                        Some(code) => SyncErrorKind::ServerError(code),
                        None if error_to_errno(&e).code() == libc::EACCES => SyncErrorKind::PermissionDenied,
                        None => SyncErrorKind::NetworkError,
                    };
                    if server_code.is_some_and(backoff::is_struggling) {
                        // The per-path cooldown already limits this to once per
                        // window per directory; during a storm the breaker's own
                        // message stands in for thousands of per-folder entries.
                        log::warn!("readdir {}: {}", path.display(), e);
                        if !conn.breaker.is_open(Instant::now()) {
                            push_error(&elog, path.clone(), kind, e.clone());
                        }
                    } else {
                        log::error!("readdir {}: {}", path.display(), e);
                        push_error(&elog, path.clone(), kind, e.clone());
                    }
                    reply.error(error_to_errno(&e));
                }
            }
        });
        if let Err((_, reply)) = submitted {
            reply.error(Errno::EAGAIN);
        }
    }
}

impl Filesystem for NextCloudFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let (parent_path, name_str) = {
            let c = self.cache.safe_lock();
            match (c.get_path(parent.0), name.to_str()) {
                (Some(p), Some(n)) => (p, n.to_string()),
                _ => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        let full_path = parent_path.join(&name_str);
        if self.exclude_folders.contains(&full_path) {
            reply.error(Errno::ENOENT);
            return;
        }
        {
            let mut ghosts = self.ghost_entries.safe_lock();
            if let Some(kind) = ghosts.get(&full_path)
                .filter(|g| g.created_at.elapsed() < GHOST_TTL)
                .map(|g| g.kind)
            {
                match kind {
                    GhostKind::HiddenAdd => { reply.error(Errno::ENOENT); return; }
                    GhostKind::VisibleDelete { attr } => { reply.entry(&Duration::ZERO, &attr, Generation(0)); return; }
                }
            }
            ghosts.remove(&full_path);
        }

        let entries = match self.parent_listing(&parent_path, Some(&name_str)) {
            Ok(files) => files,
            // We don't know the parent's contents, so we can't say the name is
            // absent: a walker told ENOENT believes it, while EAGAIN/EIO says "ask later".
            Err(e) => { reply.error(relist_errno(e.as_deref())); return; }
        };

        let found = entries.iter().find(|e| {
            e.path.file_name().and_then(|n| n.to_str()).unwrap_or("") == name_str
        });

        if let Some(entry) = found {
            let target_path = parent_path.join(&name_str);
            let ino = self.cache.safe_lock().allocate_inode(target_path.clone());
            let attr = make_file_attr(ino, entry);
            if entry.ext.flag("is_shared") {
                self.shared.safe_write().insert(target_path.clone());
            }
            if let Some(fid) = entry.ext.int("fileid") {
                self.fileids.safe_write().insert(target_path.clone(), fid);
            }
            self.details.safe_write().insert(target_path.clone(), ipc::FileDetail {
                permissions: entry.ext.str("permissions").map(str::to_string),
                owner_id: entry.ext.str("owner_id").map(str::to_string),
                owner_display_name: entry.ext.str("owner_display_name").map(str::to_string),
                size: entry.size,
                is_dir: entry.is_dir,
            });
            if let Some(parent) = target_path.parent() {
                self.children_map.safe_write()
                    .entry(parent.to_path_buf())
                    .or_insert_with(std::collections::HashSet::new)
                    .insert(target_path.clone());
            }
            if !entry.is_dir {
                self.status.safe_write().entry(target_path).or_insert(FileStatus::Remote);
            }
            reply.entry(&TTL, &attr, Generation(0));
            return;
        }
        reply.error(Errno::ENOENT);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        if ino.0 == 1 {
            reply.attr(&TTL, &root_attr());
            return;
        }

        let path = match self.cache.safe_lock().get_path(ino.0) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        {
            let ghosts = self.ghost_entries.safe_lock();
            if let Some(ghost) = ghosts.get(&path) {
                if ghost.created_at.elapsed() < GHOST_TTL {
                    if let GhostKind::VisibleDelete { attr } = ghost.kind {
                        reply.attr(&Duration::ZERO, &attr);
                        return;
                    }
                }
            }
        }

        let parent = path.parent().unwrap_or(Path::new("/")).to_path_buf();
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();

        let cached = {
            let c = self.cache.safe_lock();
            match c.get_cached_dir_readonly(&parent) {
                Some(files) => Some(files),
                None => {
                    if c.dir_cache.contains_key(&path) || path == Path::new("/") {
                        drop(c);
                        reply.attr(&TTL, &make_dir_attr(ino.0));
                        return;
                    }
                    None
                }
            }
            // Arc is cloned by get_cached_dir_readonly; guard drops here.
        };
        // A missing parent listing does not mean the file is gone: the dir cache
        // is bounded, so the listing this inode was born from may simply have
        // been evicted. Re-list before answering — `lookup` does the same.
        // Reporting ENOENT here instead made a directory whose parent had aged
        // out fail every stat(), which a file manager renders as an empty folder.
        let entries = match cached {
            Some(files) => files,
            None => {
                let want = path.file_name().and_then(|n| n.to_str()).map(str::to_string);
                match self.parent_listing(&parent, want.as_deref()) {
                    Ok(files) => files,
                    Err(e) => { reply.error(relist_errno(e.as_deref())); return; }
                }
            }
        };

        for entry in entries.iter() {
            if entry.path.file_name().and_then(|n| n.to_str()).unwrap_or("") == file_name {
                reply.attr(&TTL, &make_file_attr(ino.0, entry));
                return;
            }
        }
        reply.error(Errno::ENOENT);
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        // The MIME-type xattr a toolkit checks before sniffing (GLib:
        // `user.xdg.mime.type`), served while that toolkit's probe is declared
        // by an enabled profile (desktop::sniff).
        if !desktop::policy().serves_mime_xattr(name.as_encoded_bytes()) {
            reply.error(Errno::ENODATA);
            return;
        }
        let path = match self.cache.safe_lock().get_path(ino.0) {
            Some(p) => p,
            None => { reply.error(Errno::ENOENT); return; }
        };
        let parent = path.parent().unwrap_or(Path::new("/")).to_path_buf();
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        // An evicted parent listing must not turn into ENODATA: without
        // user.xdg.mime.type GLib falls back to magic-byte sniffing, which
        // downloads the file just to identify it.
        let entries = match self.relist_if_missing(&parent) {
            Some(e) => e,
            None => { reply.error(Errno::ENODATA); return; }
        };
        let ct = entries.iter()
            .find(|e| e.path.file_name().and_then(|n| n.to_str()).unwrap_or("") == file_name)
            .and_then(|e| e.content_type.clone());
        match ct {
            Some(ct) => {
                let bytes = ct.as_bytes().to_vec();
                if size == 0 {
                    reply.size(bytes.len() as u32);
                } else if size as usize >= bytes.len() {
                    reply.data(&bytes);
                } else {
                    reply.error(Errno::ERANGE);
                }
            }
            None => reply.error(Errno::ENODATA),
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let path = match self.cache.safe_lock().get_path(ino.0) {
            Some(p) => p,
            None => { reply.error(Errno::ENOENT); return; }
        };
        let parent = path.parent().unwrap_or(Path::new("/")).to_path_buf();
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        let entries = match self.relist_if_missing(&parent) {
            Some(e) => e,
            None => { if size == 0 { reply.size(0); } else { reply.data(b""); } return; }
        };
        let has_ct = entries.iter()
            .find(|e| e.path.file_name().and_then(|n| n.to_str()).unwrap_or("") == file_name)
            .map(|e| e.content_type.is_some())
            .unwrap_or(false);
        let mut list: Vec<u8> = Vec::new();
        if has_ct {
            for name in desktop::policy().sniff_probes.iter().filter_map(|p| p.xattr) {
                if !list.split(|b| *b == 0).any(|n| n == name.as_bytes()) {
                    list.extend_from_slice(name.as_bytes());
                    list.push(0);
                }
            }
        }
        let list = list.as_slice();
        if size == 0 {
            reply.size(list.len() as u32);
        } else if size as usize >= list.len() {
            reply.data(list);
        } else {
            reply.error(Errno::ERANGE);
        }
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let (path, local, etag, nc_permissions, remote_size) = {
            let c = self.cache.safe_lock();
            let path = match c.get_path(ino.0) {
                Some(p) => p,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            };
            let local = c
                .file_cache
                .get(&path)
                .filter(|e| e.local_path.metadata().map_or(false, |m| m.len() > 0))
                .map(|e| e.local_path.clone());
            let parent = path.parent().unwrap_or(Path::new("/")).to_path_buf();
            let (etag, nc_permissions, remote_size) = c.get_cached_dir_readonly(&parent)
                .and_then(|files| files.iter().find(|e| e.path == path).map(|e| {
                    (e.change_token.clone(), e.ext.str("permissions").map(str::to_string), e.size)
                }))
                .unwrap_or((None, None, 0));
            (path, local, etag, nc_permissions, remote_size)
        };

        let writable = flags.0 & (libc::O_WRONLY | libc::O_RDWR | libc::O_APPEND) != 0;
        // FUSE_ATOMIC_O_TRUNC is negotiated, so a truncating open arrives here instead of
        // as a separate setattr and must start from an empty staging file.
        let truncating = writable && flags.0 & libc::O_TRUNC != 0;

        // The synthetic `.trackerignore` overlay entry is read-only — there is
        // nothing behind it to write through to.
        if writable && path == trackerignore_path() {
            reply.error(Errno::EACCES);
            return;
        }

        if writable && perms_to_mode(nc_permissions.as_deref(), false) & 0o200 == 0
            && nc_permissions.is_some()
        {
            reply.error(Errno::EACCES);
            return;
        }

        // A desktop thumbnailer opening an uncached file would download all of
        // it just to render a preview the server already renders. Refuse it —
        // no network I/O — and fill the thumbnail cache from the server
        // preview instead (desktop::thumbguard). Cached files cost nothing to
        // read, so thumbnailers may still use those.
        if !writable && local.is_none() {
            let policy = desktop::policy();
            if let Some(who) = desktop::thumbguard::thumbnailer_process(req.pid(), &policy.thumbnailer_guard) {
                log::info!("THUMBGUARD refused {} to {}; fetching the server preview", path.display(), who);
                let fetch = self.thumbnail_callback();
                let remote = path.clone();
                // Off the FUSE worker: the prefetch stats and touches the file
                // through this same mount. A dropped job costs one thumbnail.
                let _ = bg::THUMB.submit(move || {
                    fetch(remote);
                });
                reply.error(Errno::EACCES);
                return;
            }
        }

        let fh = {
            let mut n = self.next_fh.safe_lock();
            let fh = *n;
            *n += 1;
            fh
        };

        // Pin the cached-copy freshness for this handle's lifetime (see OpenFile).
        // file_cache_matches_remote() returns false when the file is not cached,
        // and true when offline (no remote etag to compare) so offline reads of a
        // kept copy are never forced into an unsatisfiable re-download.
        let cache_fresh = self.cache.safe_lock().file_cache_matches_remote(&path);

        // Whether the staging file starts out holding the file's current content.
        let mut seeded = false;
        let write_path = if writable {
            let cache_dir = self.cache.safe_lock().cache_dir.clone();
            let wp = cache_dir.join(format!("write_{}", fh));
            // Stage the current content before any write: a write that does not start at
            // 0 (O_APPEND, an in-place edit) would otherwise upload a zero-filled prefix.
            let staged = if truncating {
                std::fs::File::create(&wp).map(|_| ()).map_err(|e| e.to_string())
            } else if let Some(local) = local.as_ref().filter(|_| cache_fresh) {
                seeded = true;
                std::fs::copy(local, &wp).map(|_| ()).map_err(|e| e.to_string())
            } else if remote_size > 0 {
                seeded = true;
                std::fs::File::create(&wp)
                    .map_err(|e| e.to_string())
                    .and_then(|dest| open_file_timeout(&self.conn, path.clone(), dest, Some(self.transfer_map.clone())))
            } else {
                Ok(())
            };
            if let Err(e) = staged {
                log::error!("open: cannot stage current content of {} for writing: {}", path.display(), e);
                let _ = std::fs::remove_file(&wp);
                reply.error(Errno::EIO);
                return;
            }
            Some(wp)
        } else {
            None
        };

        // Intercept GLib 2.80+ magic-byte detection opens. See the doc-comment on
        // mime_magic_bytes() for the full explanation. Summary: GLib opens files with
        // O_NOATIME when it cannot determine the MIME type from the extension alone.
        // On a remote FUSE mount that would trigger a WebDAV download (~280 ms) per
        // file. We detect the flag, look up the content-type that was already fetched
        // during the PROPFIND, and mark the handle so read() can respond with synthetic
        // magic bytes instead. O_NOFOLLOW and O_CLOEXEC are both stripped by the kernel
        // before the request reaches FUSE, so O_NOATIME is the only reliable signal.
        // Which probes count is declared per toolkit profile (desktop::sniff).
        let probe_max_read = if !writable && local.is_none() {
            desktop::policy().sniff_probe_for_open(flags.0, req.pid()).map(|p| p.max_read)
        } else {
            None
        };
        let mime_detect_ct = if probe_max_read.is_some() {
            let parent = path.parent().unwrap_or(Path::new("/")).to_path_buf();
            let ct = self.cache.safe_lock()
                .get_cached_dir_readonly(&parent)
                .and_then(|entries| {
                    entries.iter().find(|e| e.path == path)
                        .and_then(|e| e.content_type.clone())
                });
            // Fall back to octet-stream when the server did not supply a content-type
            // so that every O_NOATIME open is intercepted — no file is ever downloaded
            // solely to satisfy GLib's magic-byte check.
            Some(ct.map(|c| c.to_string())
                .unwrap_or_else(|| "application/octet-stream".to_string()))
        } else {
            None
        };

        // A MIME-detect handle answers GLib's probe with a few synthetic magic
        // bytes — far fewer than the (read-ahead-inflated) size the kernel asked
        // for. A buffered short read at offset 0 is recorded by the kernel as an
        // EOF for that page, poisoning the inode's page cache: every later
        // *buffered* read of the same file then returns only those few bytes,
        // truncating real content (a data-loss bug, since GLib sniffs a file the
        // user is about to open). FOPEN_DIRECT_IO keeps this handle's reads out
        // of the page cache entirely, so the short reply cannot poison it — and
        // as a bonus the kernel stops inflating the probe read via read-ahead, so
        // it always arrives within the GLIB_SNIFF_MAX_READ guard at its true size.
        let mime_detect = mime_detect_ct.is_some();

        // Kernel FUSE_PASSTHROUGH: only offered for a read-only open already
        // backed by a complete, fresh local cache copy — never for a handle
        // that may be served from the in-flight streaming buffer (of.buf) or
        // the write staging path, both of which require ncrs to stay on the
        // data path (see the read()/write() handlers). io_modes then decides
        // whether this open may actually use it (see iomode.rs).
        let grant = if mime_detect {
            iomode::IoGrant::DirectIo
        } else {
            let passthrough = if !writable
                && cache_fresh
                && self.conn.passthrough_enabled.load(Ordering::Relaxed)
                && self.conn.passthrough_capable.load(Ordering::Relaxed)
            {
                let reply = &reply;
                local.as_ref().and_then(|lp| {
                    let file = iomode::FileId::of(&std::fs::metadata(lp).ok()?);
                    Some((file, move || std::fs::File::open(lp).and_then(|f| reply.open_backing(f))))
                })
            } else {
                None
            };
            let (grant, err) = self.io_modes.safe_lock().acquire(ino.0, passthrough);
            if let Some(e) = err {
                // Sticky for the rest of this session — a missing
                // CAP_SYS_ADMIN or a pre-6.9 kernel will not fix itself
                // mid-run, so don't retry (and re-log) on every open().
                if self.conn.passthrough_capable.swap(false, Ordering::Relaxed) {
                    log::info!(
                        "FUSE passthrough unavailable ({}) — falling back to buffered reads for the rest of this session",
                        e
                    );
                }
            }
            grant
        };
        match grant.kind() {
            iomode::IoKind::Passthrough => log::debug!("FUSE passthrough granted for {}", path.display()),
            iomode::IoKind::DirectIo if !mime_detect => {
                log::debug!("open {}: inode already in passthrough mode — using direct I/O", path.display())
            }
            _ => {}
        }

        self.open_files.safe_lock().insert(
            fh,
            OpenFile {
                remote_path: path,
                local: local.clone(),
                buf: None,
                write_path,
                // A truncating open changes the file even if nothing is written after it.
                dirty: truncating,
                original_etag: etag,
                mime_detect_ct,
                mime_detect_max_read: probe_max_read.unwrap_or(0),
                cache_fresh,
                next_expected_off: 0,
                read_ahead_window: READ_AHEAD_INITIAL,
                next_buf: None,
                prev_buf: None,
                lookahead_inflight: false,
                last_read: Instant::now(),
                total_written: 0,
                // Only an empty staging file can stream from offset 0.
                stream_eligible: !seeded,
                chunk_upload: None,
                ino: ino.0,
                io_kind: grant.kind(),
                upload_failed: false,
                created: false,
                unlinked: false,
                opened_gen: self.uploads.generation(),
            },
        );
        // io_modes keeps the BackingId alive until the inode's last passthrough
        // handle is released (the crate warns dropping it right after replying
        // can make the kernel return EIO).
        match grant {
            iomode::IoGrant::Passthrough(id) => reply.opened_passthrough(FileHandle(fh), FopenFlags::empty(), &id),
            iomode::IoGrant::Cached => reply.opened(FileHandle(fh), FopenFlags::empty()),
            iomode::IoGrant::DirectIo => reply.opened(FileHandle(fh), FopenFlags::FOPEN_DIRECT_IO),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let path = match self.cache.safe_lock().get_path(ino.0) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let off = offset;
        let sz = size as usize;

        // The synthetic `.trackerignore` overlay entry (see trackerignore_entry()):
        // served straight from a static buffer, zero network I/O, no staging file.
        if path == trackerignore_path() {
            let start = (off as usize).min(TRACKERIGNORE_CONTENT.len());
            let end = start.saturating_add(sz).min(TRACKERIGNORE_CONTENT.len());
            reply.data(&TRACKERIGNORE_CONTENT[start..end]);
            return;
        }

        // What the dir cache believes this file's length is. Used throughout the
        // read paths below to tell a legitimate end-of-file short reply from a
        // truncating one; see `short_reply_ok`.
        let file_size = self.cache.safe_lock().find_entry(&path).map(|e| e.size).unwrap_or(0);

        // Note where the next sequential read would start, and remember whether
        // *this* read continues the previous one. Done here, before any of the
        // fast paths below can return, so a run of read-ahead-buffer hits still
        // counts as sequential access when a later read finally misses.
        let sequential = {
            let mut ofs = self.open_files.safe_lock();
            release_idle_windows(&mut ofs);
            match ofs.get_mut(&fh.0) {
                Some(of) => {
                    let continues = of.next_expected_off == off;
                    of.next_expected_off = off.saturating_add(sz as u64);
                    of.last_read = Instant::now();
                    continues
                }
                None => false,
            }
        };

        // A locally-written-but-not-yet-uploaded edit is the newest version of the
        // file and MUST win over any cached/kept copy or the server copy. Serve it
        // straight from the pending PUT's staging file. This is what lets edits
        // survive a server outage (read your own writes while offline) and fixes
        // the save-then-reopen EIO — including when a stale cache entry for the
        // path exists, which is why this runs before every other source below. A
        // rename rewrites the queued Put's remote_path to the destination (see
        // MutationJournal::enqueue), so a reopen of the renamed-to path resolves
        // here too. The staging file is removed the instant the PUT succeeds, so
        // this returns None again as soon as the file is safely on the server.
        {
            let staging = self.journal.safe_lock().pending_put_staging(&path);
            if let Some(sp) = staging {
                if let Ok(f) = std::fs::File::open(&sp) {
                    let mut buf = vec![0u8; sz];
                    // The staging file *is* the current content, so its length is the
                    // file's length and a short read here is a real EOF.
                    if let Ok(n) = read_at_full(&f, &mut buf, off) {
                        buf.truncate(n);
                        reply.data(&buf);
                        return;
                    }
                }
            }
        }

        // Serve from open-file state synchronously (no thread spawn).
        {
            let lookahead_ceiling = self.read_ahead_bytes.max(READ_AHEAD_INITIAL);
            let mut files = self.open_files.safe_lock();
            if let Some(of) = files.get_mut(&fh.0) {
                promote_lookahead(of, off);
                retire_prev_window(of, off);
                // GLib 2.80+ MIME detection: serve synthetic magic bytes instead of
                // downloading the file. The content-type was captured at open() from the
                // PROPFIND dir cache. Zero network I/O; see mime_magic_bytes() for details.
                //
                // Guard: GLib's 16384-byte magic-detection read is inflated by
                // kernel read-ahead up to GLIB_SNIFF_MAX_READ (32768) for the
                // initial read of a file, while copy tools use ≥65536-byte
                // buffers. Intercepting reads up to that bound keeps copies
                // correct. See mime_magic_bytes() for the full rationale.
                if let Some(ref ct) = of.mime_detect_ct {
                    if off == 0 && sz <= of.mime_detect_max_read {
                        let magic = mime_magic_bytes(ct);
                        let end = magic.len().min(sz);
                        reply.data(&magic[..end]);
                        return;
                    }
                    // Copy-sized read (sz > GLIB_SNIFF_MAX_READ) on a handle that was opened while the
                    // file was NOT in the local cache (mime_detect_ct being set proves this).
                    // If we are offline, we have no real content to serve — fail now rather
                    // than letting the file_cache return a stale/poisoned entry or letting
                    // ensure_file_cached produce an EIO after a wasted round-trip.
                    if self.conn.is_offline.load(Ordering::Relaxed) {
                        reply.error(Errno::EACCES);
                        return;
                    }
                }
                // Only serve the cached local copy if it was fresh at open() —
                // otherwise a file edited server-side (e.g. in Nextcloud Office)
                // would be served at the stale content but the NEW getattr size,
                // which reads back as a corrupt archive. The verdict is pinned per
                // handle (of.cache_fresh) so a mid-scan dir-cache refresh cannot
                // flip it and splice stale+fresh bytes. When stale, fall through to
                // the network fetch below, which re-downloads.
                if let Some(ref local) = of.local {
                    if of.cache_fresh {
                        if let Ok(f) = std::fs::File::open(local) {
                            let mut buf = vec![0u8; sz];
                            if let Ok(n) = read_at_full(&f, &mut buf, off) {
                                // A cached copy shorter than the file — a partial or
                                // truncated download — must not be replied short; that
                                // latches EOF on the inode. Fall through and fetch.
                                if short_reply_ok(off, n, sz, file_size) {
                                    buf.truncate(n);
                                    reply.data(&buf);
                                    return;
                                }
                            }
                        }
                    }
                }
                // A READ for the tail of the window the reader just left (see
                // `promote_lookahead`) is served from that window, not refetched.
                let covers = |b: &ReadAheadBuf| off >= b.start && off < b.start + b.target_len;
                let window = if !of.buf.as_ref().is_some_and(covers) && of.prev_buf.as_ref().is_some_and(covers) {
                    of.prev_buf.as_ref()
                } else {
                    of.buf.as_ref()
                };
                if let Some(ra) = window {
                    let (ref mtx, ref _cv) = *ra.stream;
                    let ss = mtx.lock().unwrap();
                    let available = ra.start + ss.data.len() as u64;
                    if off >= ra.start && off + sz as u64 <= available {
                        let s = (off - ra.start) as usize;
                        reply.data(&ss.data[s..s + sz]);
                        drop(ss);
                        // Rare (once per window) and non-blocking: a pool submit.
                        let la = plan_lookahead(of, off, sz, sequential, file_size, lookahead_ceiling);
                        drop(files);
                        if let Some(la) = la {
                            self.start_lookahead(fh.0, &path, file_size, la);
                        }
                        return;
                    }
                    // Data within target range but not yet downloaded — wait in thread
                    if off >= ra.start && off + sz as u64 <= ra.start + ra.target_len && !ss.done {
                        let shared = Arc::clone(&ra.stream);
                        let start = ra.start;
                        drop(ss);
                        // A reader outrunning the download is exactly the one the
                        // look-ahead helps most: the next GET overlaps this one.
                        let la = plan_lookahead(of, off, sz, sequential, file_size, lookahead_ceiling);
                        drop(files);
                        if let Some(la) = la {
                            self.start_lookahead(fh.0, &path, file_size, la);
                        }
                        run_read_job(reply, move |reply| wait_on_window(reply, &shared, start, off, sz, file_size));
                        return;
                    }
                    // Condition 3: the request starts inside this window but runs past
                    // the end of what the window will ever hold — a read straddling the
                    // read-ahead boundary, or GLib's MIME probe asking for 16 KiB of a
                    // small, fully prefetched file.
                    //
                    // The window can only answer such a read short, and a short reply is
                    // safe *only* when it lands on the real end of the file (see
                    // `short_reply_ok`). Otherwise the kernel latches EOF on the inode
                    // and the rest of the file becomes unreadable on every handle until
                    // a fresh open() — which is how a 30 MB FLAC read in 16 KiB chunks
                    // used to stop dead at exactly 2 MiB, the first window boundary.
                    // So serve the end-of-file case, and let every other straddling read
                    // fall through to the network fetch below, which re-reads from `off`
                    // and satisfies the request in full.
                    if off >= ra.start && off < ra.start + ra.target_len {
                        let o = (off - ra.start) as usize;
                        let window_end = ra.start + ra.target_len;
                        // The straddle runs into the following window (the look-ahead,
                        // or the current one when this is `prev_buf`): when this window
                        // is complete and the next already holds the rest, stitch the
                        // two instead of re-fetching (and discarding the look-ahead).
                        let following = if of.prev_buf.as_ref().is_some_and(|p| Arc::ptr_eq(&p.stream, &ra.stream)) {
                            of.buf.as_ref()
                        } else {
                            of.next_buf.as_ref()
                        };
                        if let Some(nb) = following {
                            if nb.start == window_end && ss.done && !ss.halted() && ra.start + ss.data.len() as u64 == window_end {
                                // Lock order is always the earlier window, then the later
                                // one; a pump only ever holds its own stream's lock.
                                let ns = nb.stream.0.lock().unwrap();
                                let need = (off + sz as u64 - window_end) as usize;
                                if ns.data.len() >= need {
                                    let mut out = Vec::with_capacity(sz);
                                    out.extend_from_slice(&ss.data[o..]);
                                    out.extend_from_slice(&ns.data[..need]);
                                    reply.data(&out);
                                    drop(ns);
                                    drop(ss);
                                    drop(files);
                                    return;
                                }
                            }
                        }
                        if ss.done && !ss.halted() {
                            let avail = ss.data.len().saturating_sub(o);
                            if ss.at_eof() || short_reply_ok(off, avail, sz, file_size) {
                                let e = ss.data.len().min(o.saturating_add(sz));
                                reply.data(&ss.data[o..e]);
                                drop(ss);
                                drop(files);
                                return;
                            }
                        } else if short_reply_ok(off, (window_end - off) as usize, sz, file_size) {
                            // Still downloading, and even a complete window stops short
                            // of this request — but only because the file itself ends
                            // inside it. Wait for the bytes and serve what there is.
                            let shared = Arc::clone(&ra.stream);
                            let start = ra.start;
                            drop(ss);
                            drop(files);
                            run_read_job(reply, move |reply| wait_on_window(reply, &shared, start, off, sz, file_size));
                            return;
                        }
                    }
                    drop(ss);
                }
            }
        }

        // Check file_cache synchronously too — but only when this handle's cached
        // copy was fresh at open() (of.cache_fresh, pinned; see the of.local path
        // above for why re-checking per read corrupts). A file edited server-side
        // (Nextcloud Office, another client) leaves this entry stale; serving it at
        // the refreshed getattr size makes a ZIP-based format (odt/xlsx/…) look
        // corrupt. When stale, skip it and fall through to the network fetch.
        {
            let cache_fresh = self.open_files.safe_lock().get(&fh.0).map_or(false, |of| of.cache_fresh);
            let cached_local = if cache_fresh {
                let c = self.cache.safe_lock();
                c.file_cache.get(&path)
                    .filter(|fc| fc.local_path.metadata().map_or(false, |m| m.len() > 0))
                    .map(|fc| fc.local_path.clone())
            } else {
                None
            };
            if let Some(ref local) = cached_local {
                if let Ok(f) = std::fs::File::open(local) {
                    let mut buf = vec![0u8; sz];
                    if let Ok(n) = read_at_full(&f, &mut buf, off) {
                        // Same rule as the of.local path: a cached copy that is
                        // shorter than the file must be refetched, not replied short.
                        if short_reply_ok(off, n, sz, file_size) {
                            buf.truncate(n);
                            reply.data(&buf);
                            self.open_files.safe_lock().entry(fh.0).and_modify(|of| of.local = Some(local.clone()));
                            return;
                        }
                    }
                }
            }
        }

        // Network fetch — only this path needs a thread.
        let open_files = self.open_files.clone();
        let conn = self.conn.clone();
        let cache = self.cache.clone();
        let status = self.status.clone();
        let dirty = self.dirty.clone();
        let elog = self.error_log.clone();
        let tmap = self.transfer_map.clone();
        let notifier_slot = self.notifier_slot.clone();
        let auto_keep_cached = self.auto_keep_cached_files;
        let read_ahead = self.read_ahead_bytes;
        let cache_streamed = self.cache_streamed_reads;
        let file_total_size = file_size;
        let ino_u64 = ino.0;
        // True when the file was not locally cached at open() time. For such handles
        // there is no guarantee the file_cache has valid content, so the ensure_file_cached
        // fallback must be skipped — a range-read failure means the copy must fail.
        let is_mime_detect_open = self.open_files.safe_lock()
            .get(&fh.0)
            .map_or(false, |of| of.mime_detect_ct.is_some());

        // Size the read-ahead for the access pattern rather than always fetching
        // the configured maximum. Sequential reads double the window up to
        // `read_ahead_bytes`, so streaming ends up with the same large window as
        // before; a seek drops back to READ_AHEAD_INITIAL, so probing a file's
        // header or seektable costs ~1 MB instead of 64 MB apiece.
        let fetch = {
            let ceiling = read_ahead.max(READ_AHEAD_INITIAL);
            let mut ofs = self.open_files.safe_lock();
            let window = match ofs.get_mut(&fh.0) {
                Some(of) => {
                    let jump = of.buf.as_ref().and_then(|b| {
                        streaming_jump(of.read_ahead_window, b.start, sequential, file_size, ceiling)
                    });
                    of.read_ahead_window = jump.unwrap_or_else(|| {
                        next_read_ahead_window(of.read_ahead_window, sequential, ceiling)
                    });
                    of.read_ahead_window
                }
                None => READ_AHEAD_INITIAL.min(ceiling),
            };
            std::cmp::max(sz, window)
        };

        run_read_job(reply, move |reply| {
            let use_throttle = fetch > sz;
            let _stream_guard = if use_throttle {
                conn.active_streams.fetch_add(1, Ordering::Relaxed);
                Some(StreamActiveGuard {
                    counter: Arc::clone(&conn.active_streams),
                    deferred: Arc::clone(&conn.deferred_invalidation),
                    cache: Arc::clone(&cache),
                    dirty: dirty.clone(),
                    notifier_slot: notifier_slot.clone(),
                })
            } else {
                None
            };
            // A large window of a file whose size we know is fetched as several
            // concurrent segments when spare slots allow; see `open_window`.
            let req = WindowRequest {
                start: off,
                target: fetch,
                need: sz,
                file_size: file_total_size,
                spare: 0,
                deadline: Instant::now() + RANGE_OPEN_BUDGET,
                primary: None,
                reserved: None,
            };
            match open_window(&conn, &path, req) {
                Ok(OpenedWindow { mut resp, permit, extras, plan, target, total, budget }) => {
                    let t0 = Instant::now();
                    // The authoritative current size, read from the response headers
                    // before the body is consumed (see the reconciliation after
                    // reply.data below).
                    let server_total = total;
                    match read_exact_from_stream(&mut resp, sz, Instant::now() + FIRST_BYTES_DEADLINE) {
                        Ok(first) => {
                            // Content-Range is authoritative for the file's length, with
                            // the dir-cache size as fallback. A body that stops before
                            // either is a truncated transfer, not end-of-file, and
                            // replying it short would latch EOF on the inode for every
                            // handle (see `short_reply_ok`). Fail instead — transiently,
                            // so the caller retries against real data.
                            let total_known = server_total.unwrap_or(file_total_size);
                            if total_known > 0 && !short_reply_ok(off, first.len(), sz, total_known) {
                                log::warn!(
                                    "range read {} returned {} of {} bytes at offset {} (file is {}) — truncated transfer",
                                    path.display(), first.len(), sz, off, total_known,
                                );
                                push_error(&elog, path.clone(), SyncErrorKind::NetworkError,
                                    "download truncated".to_string());
                                reply.error(Errno::EIO);
                                return;
                            }
                            reply.data(&first);
                            // Reconcile the getattr size with reality. On a plain
                            // WebDAV server (rclone) a child edit does not bump the
                            // parent dir's ETag, so the background dir refresh takes
                            // the ETAG_MATCH fast-path and never re-lists — the
                            // dir-cache entry keeps the pre-edit size. But this GET
                            // returned the CURRENT bytes, so the kernel (holding the
                            // stale, smaller i_size from a TTL-cached getattr) clamps
                            // a buffered read and a server-edited file reads back
                            // truncated: new content at the old size, hashing to
                            // neither version and looking corrupt. When the true size
                            // differs from what we cached, patch the dir entry (so the
                            // next getattr is correct) and flush the kernel's stale
                            // attr+data cache for this inode. The invalidation MUST run
                            // off this thread and after the reply: notify_inval_inode
                            // on the very inode a read is in flight for deadlocks
                            // against the kernel inode lock (this is why notify_push
                            // only ever invalidates from its own background thread).
                            // Skip while our own upload is in flight — the PUT
                            // completion owns the size then.
                            if let Some(total) = server_total {
                                let stale = {
                                    let c = cache.safe_lock();
                                    !c.uploading.contains(&path) && file_total_size != total
                                };
                                if stale && cache.safe_lock().set_entry_size(&path, total) {
                                    log::info!("read: reconciled {} size {} → {} (server-side change)", path.display(), file_total_size, total);
                                    let ns = notifier_slot.clone();
                                    notify_later(move || {
                                        // Let the triggering read fully release the
                                        // inode before invalidating; if this still
                                        // blocks it harms nothing (detached, holds no
                                        // locks) and the attr TTL is the backstop.
                                        thread::sleep(Duration::from_millis(50));
                                        if let Some(notifier) = ns.safe_lock().as_ref() {
                                            let _ = notifier.inval_inode(INodeNo(ino_u64), 0, 0);
                                        }
                                    });
                                }
                            }
                            let tkey: TransferKey = (path.clone(), next_stream_id());
                            tmap.safe_lock().insert(tkey.clone(), TransferProgress {
                                path: path.clone(),
                                direction: TransferDirection::Download,
                                bytes_done: first.len() as u64,
                                total_bytes: target,
                            });
                            let shared = Arc::new((
                                Mutex::new(StreamState::new(first, &plan, budget)),
                                Condvar::new(),
                            ));
                            open_files.safe_lock().entry(fh.0).and_modify(|of| {
                                of.buf = Some(ReadAheadBuf {
                                    start: off,
                                    stream: Arc::clone(&shared),
                                    target_len: target,
                                });
                                // A fetch here means the reader left both windows (a
                                // seek, or a boundary the look-ahead did not cover):
                                // the look-ahead is for a position nobody is at.
                                of.next_buf = None;
                                of.prev_buf = None;
                            });
                            // The READ is answered; the rest of the window is a body with
                            // no reply attached, so it moves off this `read` worker.
                            let finish_shared = Arc::clone(&shared);
                            let finish_open_files = Arc::clone(&open_files);
                            let finish_path = path.clone();
                            let finish = Box::new(move |total_bytes: usize| {
                                // Held until the body is done: defers dir invalidation.
                                let _stream_guard = _stream_guard;
                                let shared = finish_shared;
                                let open_files = finish_open_files;
                                let path = finish_path;
                            let (ref mtx, _) = *shared;
                            let total_ms = t0.elapsed().as_millis();
                            if total_ms > 0 {
                                let mbps = total_bytes as f64 / 1_048_576.0 / (total_ms as f64 / 1000.0);
                                log::info!("stream read {}B total={}ms {:.1}MB/s", total_bytes, total_ms, mbps);
                            }
                            if cache_streamed
                                && off == 0
                                && file_total_size > 0
                                && total_bytes as u64 >= file_total_size
                                && cache.safe_lock().file_cache.get(&path).is_none()
                            {
                                let (target_dir, kept) = {
                                    let c = cache.safe_lock();
                                    if auto_keep_cached {
                                        (c.kept_dir.clone(), true)
                                    } else {
                                        (c.auto_cache_dir.clone(), false)
                                    }
                                };
                                let rel = path.strip_prefix("/").unwrap_or(&path);
                                let local_path = target_dir.join(rel);
                                if let Some(parent) = local_path.parent() {
                                    let _ = std::fs::create_dir_all(parent);
                                }
                                // Stream lock held only for the write: read() takes open_files then
                                // this lock, so it must never be held while taking cache/open_files.
                                let written = {
                                    let ss = mtx.lock().unwrap();
                                    std::fs::write(&local_path, &ss.data[..file_total_size as usize]).is_ok()
                                };
                                if written {
                                    let mut c = cache.safe_lock();
                                    let mod_time = c.remote_modified_for(&path);
                                    let etag = c.remote_etag_for(&path);
                                    c.file_cache.insert(path.clone(), FileCacheEntry {
                                        local_path: local_path.clone(),
                                        remote_modified: mod_time,
                                        etag,
                                        kept,
                                        size: file_total_size,
                                    });
                                    drop(c);
                                    save_file_cache(&cache);
                                    let file_status = if kept { FileStatus::Kept } else { FileStatus::Cached };
                                    status.safe_write().insert(path.clone(), file_status);
                                    dirty.safe_lock().insert(path.clone());
                                    log::info!("stream→cache {} ({}B, {})", path.display(), file_total_size, if kept { "kept" } else { "cached" });
                                    open_files.safe_lock().entry(fh.0).and_modify(|of| {
                                        of.local = Some(local_path);
                                    });
                                }
                            }
                            });
                            PumpJob {
                                conn: Arc::clone(&conn),
                                path: path.clone(),
                                fh: fh.0,
                                open_files: Arc::clone(&open_files),
                                tmap: tmap.clone(),
                                tkey,
                                shared,
                                start: off,
                                spare: 0,
                                total,
                                resp,
                                permit: permit.into_owned(&conn.read_throttle),
                                extras: extras.into_iter().map(|(p, t)| (p.into_owned(&conn.read_throttle), t)).collect(),
                                plan,
                                finish,
                            }
                            .spawn();
                        }
                        Err(e) if e.starts_with(READ_BODY_SLOW_PREFIX) => {
                            // Slow, not gone: another connection next time, a probe now,
                            // and "try again" for the caller — never offline.
                            log::warn!("stream read first bytes: {} (slot {})", e, permit.slot());
                            conn.read_throttle.quarantine(permit.slot(), H3_MAX_IDLE);
                            request_probe(&conn);
                            reply.error(Errno::EAGAIN);
                        }
                        Err(e) => {
                            log::warn!("stream read first bytes failed: {}", e);
                            if read_err_is_network_down(&e) {
                                mark_offline(&conn.is_offline, &conn.offline_since);
                            }
                            push_error(&elog, path.clone(), SyncErrorKind::NetworkError, format!("download failed: {}", e));
                            reply.error(Errno::EIO);
                        }
                    }
                }
                Err(e) if e == DNS_BUSY_ERR => {
                    log::warn!("read {} at {}: {} — EAGAIN", path.display(), off, e);
                    reply.error(Errno::EAGAIN);
                }
                Err(e) if e == READ_HEADER_TIMEOUT_ERR => {
                    // Already probed and quarantined in do_range_read_stream.
                    log::warn!("read {} at {}: {} — EAGAIN", path.display(), off, e);
                    reply.error(Errno::EAGAIN);
                }
                Err(e) if e == READ_SLOTS_BUSY_ERR => {
                    // Every read slot stayed taken for READ_SLOT_WAIT. The server may
                    // be fine — this daemon is just saturated — so neither go offline
                    // nor fall back to a whole-file download (which needs the same
                    // slots). "Try again", like a full read pool.
                    log::warn!("read {} at {}: no read slot within {:?} — EAGAIN", path.display(), off, READ_SLOT_WAIT);
                    reply.error(Errno::EAGAIN);
                }
                Err(e) => {
                    // This read just proved the server is unreachable. Flip offline
                    // now so the rest of the save's reads/writes take the instant
                    // cache/journal path instead of each waiting out its own connect
                    // timeout; the connectivity monitor clears the flag (and replays
                    // the journal) within ~5s of the server coming back.
                    if read_err_is_network_down(&e) {
                        mark_offline(&conn.is_offline, &conn.offline_since);
                    }
                    // If we are offline (either the flip above or the connectivity
                    // monitor set it), the ensure_file_cached fallback below can only
                    // fail after burning its own connect-timeout retries — there is no
                    // reachable server to download from. Fail fast instead so an app
                    // doing a read-modify-write save is not stalled once per read; the
                    // pending edit is still safe in staging + the journal.
                    if conn.is_offline.load(Ordering::Relaxed) {
                        log::warn!("read {} failed while offline: {}", path.display(), e);
                        push_error(&elog, path.clone(), SyncErrorKind::NetworkError, format!("offline: {}", e));
                        reply.error(error_to_errno(&e));
                        return;
                    }
                    // For handles opened while the file was not cached (mime-detect opens),
                    // skip ensure_file_cached entirely — the file_cache may hold a stale or
                    // poisoned entry and we have no valid content to offer. Propagate the
                    // range-read error so the copy fails cleanly.
                    if is_mime_detect_open {
                        log::error!("range read failed on uncached file {}: {}", path.display(), e);
                        push_error(&elog, path.clone(), SyncErrorKind::NetworkError, format!("download failed: {}", e));
                        reply.error(error_to_errno(&e));
                        return;
                    }
                    log::warn!("range read failed, falling back to full download: {}", e);
                    // Bounded: this job holds the READ's reply (see READ_FALLBACK_BUDGET).
                    let within = Some(Instant::now() + READ_FALLBACK_BUDGET);
                    match ensure_file_cached_within(&conn, &cache, &status, &dirty, path.clone(), Some(&tmap), auto_keep_cached, within) {
                        Ok(local) => {
                            if let Ok(f) = std::fs::File::open(&local) {
                                let mut buf = vec![0u8; sz];
                                // The whole file was just downloaded, so a short fill
                                // is its real end. A bare `read_at` may come back short
                                // anywhere, and a short reply latches EOF on the inode.
                                match read_at_full(&f, &mut buf, off) {
                                    Ok(n) => {
                                        buf.truncate(n);
                                        reply.data(&buf);
                                        open_files.safe_lock().entry(fh.0).and_modify(
                                            |of| of.local = Some(local),
                                        );
                                        return;
                                    }
                                    Err(_) => {}
                                }
                            }
                            reply.error(Errno::EIO);
                        }
                        Err(e2) => {
                            log::error!("fallback download failed {}: {}", path.display(), e2);
                            push_error(&elog, path.clone(), SyncErrorKind::NetworkError, format!("download failed: {}", e2));
                            reply.error(error_to_errno(&e2));
                        }
                    }
                }
            }
        });
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        // The last close of the handle: the only point where no further write can arrive.
        let Some(of) = self.open_files.safe_lock().remove(&fh.0) else {
            reply.ok();
            return;
        };
        self.io_modes.safe_lock().release(of.ino, of.io_kind);
        reply.ok();

        if of.upload_failed || of.unlinked {
            // A write already returned EIO after chunks reached the server (assembling them
            // would publish an incomplete file), or the file was deleted while open
            // (committing would re-create it). Tear any session down instead.
            if of.unlinked {
                if let Some(ref wp) = of.write_path {
                    let _ = std::fs::remove_file(wp);
                }
            }
            self.cache.safe_lock().uploading.remove(&of.remote_path);
            if let Some(cs) = of.chunk_upload {
                log::warn!(
                    "release: fh {} streamed upload of {} failed mid-copy ({}) — abandoning it, the copy must be retried",
                    fh.0, of.remote_path.display(), cs.uploads_base,
                );
                let backend = self.conn.backend.clone();
                let _ = bg::BACKGROUND.submit(move || {
                    backend.abort_chunked_upload(&backend::ChunkedUploadSession { uploads_base: cs.uploads_base });
                });
            }
            return;
        }
        if !of.dirty {
            if let Some(ref wp) = of.write_path {
                // Opened writable but never written — discard the staging copy and the
                // create() guard; no PUT will follow.
                let _ = std::fs::remove_file(wp);
                self.cache.safe_lock().uploading.remove(&of.remote_path);
            }
            return;
        }
        if let Err(e) = self.commit_released(fh, of) {
            log::warn!("release: commit of fh {} failed: {:?}", fh.0, e);
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let path = match self.cache.safe_lock().get_path(ino.0) {
            Some(p) => p,
            None => { reply.error(Errno::ENOENT); return; }
        };
        let fh = {
            let mut n = self.next_fh.safe_lock();
            let fh = *n;
            *n += 1;
            fh
        };
        self.open_dirs.safe_lock().insert(fh, OpenDir { path, snapshot: None });
        reply.opened(FileHandle(fh), FopenFlags::empty());
    }

    fn releasedir(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _flags: OpenFlags, reply: ReplyEmpty) {
        self.open_dirs.safe_lock().remove(&fh.0);
        reply.ok();
    }

    fn readdir(&self, req: &Request, ino: INodeNo, fh: FileHandle, offset: u64, reply: ReplyDirectory) {
        self.readdir_common(req.pid(), ino, fh.0, offset, DirReply::Plain(reply));
    }

    fn readdirplus(&self, req: &Request, ino: INodeNo, fh: FileHandle, offset: u64, reply: ReplyDirectoryPlus) {
        self.readdir_common(req.pid(), ino, fh.0, offset, DirReply::Plus(reply));
    }

    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        // readdirplus bundles each entry's attributes into the directory read
        // (readdir + lookup in one), and READDIRPLUS_AUTO lets the kernel fall
        // back to plain readdir when a listing does not stat its entries. A file
        // manager stats every file, so this removes a lookup+getattr round-trip
        // per entry on large directories; `ls` keeps using plain readdir. Best
        // effort — silently ignored if the running kernel lacks support.
        let _ = config.add_capabilities(InitFlags::FUSE_DO_READDIRPLUS | InitFlags::FUSE_READDIRPLUS_AUTO);
        // Deliver O_TRUNC to open() rather than as a follow-up setattr, so open() knows
        // not to stage (download) content that is about to be discarded.
        let _ = config.add_capabilities(InitFlags::FUSE_ATOMIC_O_TRUNC);
        // Negotiate FUSE_PASSTHROUGH support unconditionally — cheap and
        // harmless if unused. Whether any given open() actually hands the
        // kernel a backing fd is decided per-open against passthrough_enabled/
        // passthrough_capable, not here; this only makes the *option* available
        // for the life of the mount. Both calls are required: set_max_stack_depth
        // alone does NOT add FUSE_PASSTHROUGH to the requested capability set —
        // add_capabilities is what actually asks the kernel for it, and it only
        // succeeds if the kernel already advertised support (6.9+). Best effort:
        // silently ignored otherwise.
        //
        // Depth 2 (the kernel's hard max) rather than 1: depth 1 only covers a
        // backing file on a plain (non-stacked) filesystem. The cache dir is
        // frequently on a stacked fs in practice (e.g. a container root on
        // overlay2, or the user's home on an overlay/bind mount) — with depth
        // 1, open_backing() on such a file fails ELOOP ("too many levels").
        // There is no reason not to request the max: it costs nothing when
        // the backing fs isn't stacked.
        let _ = config.add_capabilities(InitFlags::FUSE_PASSTHROUGH);
        let _ = config.set_max_stack_depth(2);
        Ok(())
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        if let Some(new_size) = size {
            if let Some(fh) = fh {
                let fh_raw = fh.0;
                let mut files = self.open_files.safe_lock();
                if let Some(of) = files.get_mut(&fh_raw) {
                    if let Some(cs) = &of.chunk_upload {
                        // A streamed upload is in progress: already-sent chunks are
                        // gone from local disk, so only a no-op truncate (to the
                        // length already accounted for) can be honored — anything
                        // else would need to shrink/extend bytes we no longer have.
                        if new_size != of.total_written {
                            log::error!(
                                "setattr: truncate on fh {} to {} while a streamed upload is in progress ({} bytes already sent) — aborting handle",
                                fh_raw, new_size, cs.bytes_confirmed,
                            );
                            of.upload_failed = true;
                            reply.error(Errno::EIO);
                            return;
                        }
                    } else {
                        // No chunk sent yet, so the tail file still holds 100% of
                        // the content — safe to truncate directly and stop trying
                        // to stream this handle (a resize is not a sequential
                        // append the fast path can reason about).
                        of.stream_eligible = false;
                        let wp = of.write_path.get_or_insert_with(|| {
                            let cache_dir = self.cache.safe_lock().cache_dir.clone();
                            cache_dir.join(format!("write_{}", fh_raw))
                        });
                        if !wp.exists() {
                            let seed_ok = if let Some(ref local) = of.local {
                                std::fs::copy(local, &wp).is_ok()
                            } else {
                                std::fs::File::create(&wp).is_ok()
                            };
                            if !seed_ok {
                                log::error!("setattr: cannot create staging file {}", wp.display());
                                reply.error(Errno::EIO);
                                return;
                            }
                        }
                        match std::fs::OpenOptions::new().write(true).open(&wp) {
                            Ok(f) => {
                                if let Err(e) = f.set_len(new_size) {
                                    log::error!("setattr: truncate staging file failed: {}", e);
                                    reply.error(Errno::EIO);
                                    return;
                                }
                            }
                            Err(e) => {
                                log::error!("setattr: open staging file for truncate failed: {}", e);
                                reply.error(Errno::EIO);
                                return;
                            }
                        }
                        of.total_written = new_size;
                    }
                    of.dirty = true;
                }
            }
            let path = self.cache.safe_lock().get_path(ino.0);
            let lookup_entry = |p: &Path| -> Option<RemoteEntry> {
                let parent = p.parent().unwrap_or(Path::new("/")).to_path_buf();
                self.cache.safe_lock().get_cached_dir_readonly(&parent)
                    .and_then(|files| files.iter().find(|e| e.path == *p).cloned())
            };
            let entry = path.as_ref().and_then(|p| {
                // The parent listing can have been evicted since this inode was
                // handed out; re-list rather than answer from nothing.
                lookup_entry(p).or_else(|| {
                    let parent = p.parent().unwrap_or(Path::new("/")).to_path_buf();
                    if let Err(e) = get_or_list_dir(&self.conn, &self.cache, parent.clone(), None) {
                        log::debug!("setattr: re-list {} failed: {}", parent.display(), e);
                    }
                    lookup_entry(p)
                })
            });
            let mut attr = match entry {
                Some(ref e) => make_file_attr(ino.0, e),
                // This branch only runs for a size change, i.e. a truncate of a
                // regular file: a directory attr here would tell the writer its
                // own file is a directory.
                None => make_unknown_file_attr(ino.0, new_size),
            };
            attr.size = new_size;
            reply.attr(&TTL, &attr);
        } else {
            self.getattr(_req, ino, None, reply);
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let path = match self.cache.safe_lock().get_path(ino.0) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        log::debug!("[{}] WRITE {} offset={} len={}", self.log_user, path.display(), offset, data.len());

        let mut files = self.open_files.safe_lock();
        let of = match files.get_mut(&fh.0) {
            Some(of) => of,
            None => {
                reply.error(Errno::EIO);
                return;
            }
        };

        let wp = of.write_path.get_or_insert_with(|| {
            let cache_dir = self.cache.safe_lock().cache_dir.clone();
            cache_dir.join(format!("write_{}", fh.0))
        }).clone();
        if !wp.exists() {
            let seed_ok = if let Some(ref local) = of.local {
                std::fs::copy(local, &wp).is_ok()
            } else {
                std::fs::File::create(&wp).is_ok()
            };
            if !seed_ok {
                log::error!("write: cannot create staging file {}", wp.display());
                reply.error(Errno::EIO);
                return;
            }
        }

        // Bounded chunked streaming: a freshly-written file, written purely
        // sequentially from offset 0 while online, never keeps more than ~1
        // chunk of unsent bytes on local disk — completed CHUNK_SIZE chunks are
        // pushed to Nextcloud's chunked-upload extension as they fill, instead
        // of the whole file landing on disk before any upload starts. See
        // ChunkUploadState's doc comment for why any deviation once a chunk has
        // actually been sent fails the handle instead of trying to reconcile.
        //
        // `is_offline` only gates *starting* a fresh session (chunk_upload is
        // still None): it is a mount-wide flag that unrelated traffic on any
        // other file handle (a read, a PROPFIND) can flip momentarily, and once
        // a session has a chunk sitting on the server there is no local-only
        // fallback for it — bailing out here on every such blip would abort a
        // perfectly healthy in-progress upload of file B just because file A's
        // read hit a transient error at the same moment. Once streaming has
        // actually begun, let the tail keep buffering locally and let the next
        // real network call (graduate_chunk, which now retries transient
        // failures) be the one to decide whether the server is actually gone.
        let blocked_by_offline =
            of.chunk_upload.is_none() && self.conn.is_offline.load(Ordering::Relaxed);
        if of.stream_eligible && offset == of.total_written && !blocked_by_offline {
            if let Err(e) = append_to_tail_file(&wp, data) {
                log::error!("write to staging tail file: {}", e);
                reply.error(Errno::EIO);
                return;
            }
            of.dirty = true;
            of.total_written += data.len() as u64;

            let bytes_confirmed = of.chunk_upload.as_ref().map_or(0, |c| c.bytes_confirmed);
            if of.total_written - bytes_confirmed >= webdav_ops::CHUNK_SIZE as u64 {
                let total_written = of.total_written;
                let existing_session = of.chunk_upload.clone();
                let had_session = existing_session.is_some();
                drop(files);
                match self.graduate_chunk(&path, &wp, existing_session, total_written) {
                    Ok(new_state) => {
                        if let Some(of) = self.open_files.safe_lock().get_mut(&fh.0) {
                            of.chunk_upload = Some(new_state);
                        }
                    }
                    Err((e, None)) if !had_session => {
                        // The session never opened (e.g. a server without chunked uploads
                        // answers MKCOL with 404): nothing was sent, so the tail still holds
                        // every byte. Keep it as a whole-file staging copy for release().
                        log::warn!("write: chunked upload unavailable for {} ({}) — staging the whole file instead", path.display(), e);
                        if let Some(of) = self.open_files.safe_lock().get_mut(&fh.0) {
                            of.stream_eligible = false;
                        }
                    }
                    Err((e, partial_state)) => {
                        log::warn!("write: chunk upload failed for {}, aborting handle: {}", path.display(), e);
                        // Persist whatever session state the server actually confirmed
                        // (including a session opened by this very call) so release()'s
                        // abandoned-session cleanup can still find and abort it instead
                        // of leaking it server-side.
                        if let Some(of) = self.open_files.safe_lock().get_mut(&fh.0) {
                            of.chunk_upload = partial_state;
                            of.upload_failed = true;
                        }
                        reply.error(Errno::EIO);
                        return;
                    }
                }
            }
            reply.written(data.len() as u32);
            return;
        }

        if of.chunk_upload.is_some() {
            // At least one chunk is already sitting on the server with nothing
            // local to reconstruct it from — a non-sequential write, a resize,
            // or going offline mid-copy cannot be reconciled here. Fail the
            // write rather than misplace bytes in the small tail file or lose
            // the already-uploaded prefix silently. A plain sequential copy
            // never reaches this.
            log::error!(
                "write: non-sequential write on {} after chunked upload had begun (offset={}, expected={}) — aborting handle",
                path.display(), offset, of.total_written,
            );
            of.upload_failed = true;
            reply.error(Errno::EIO);
            return;
        }
        if of.stream_eligible {
            // First deviation before any chunk was ever sent — the tail file
            // already holds 100% of the content written so far, so there is
            // nothing to reconcile; just stop trying to stream this handle.
            of.stream_eligible = false;
        }

        match std::fs::OpenOptions::new().write(true).create(true).open(&wp) {
            Ok(f) => {
                match f.write_at(data, offset) {
                    Ok(n) => {
                        of.dirty = true;
                        reply.written(n as u32);
                    }
                    Err(e) => {
                        log::error!("write to staging file: {}", e);
                        reply.error(Errno::EIO);
                    }
                }
            }
            Err(e) => {
                log::error!("open staging file: {}", e);
                reply.error(Errno::EIO);
            }
        }
    }

    fn flush(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
        // Durability only. FLUSH is sent on every close() of any fd sharing this handle —
        // a forked child or a shell's per-command redirect exiting mid-write included — so
        // it is never the end of the file: committing here uploaded half-written snapshots
        // and deleted the staging file under a writer. release() commits.
        let staged = self.open_files.safe_lock().get(&fh.0).filter(|of| of.dirty).and_then(|of| {
            let streamed_size = of.chunk_upload.as_ref().map(|_| of.total_written);
            of.write_path.clone().map(|wp| (wp, of.remote_path.clone(), streamed_size))
        });
        if let Some((wp, remote_path, streamed_size)) = staged {
            if let Ok(f) = std::fs::File::open(&wp) {
                if let Err(e) = f.sync_all() {
                    log::warn!("flush: fsync staging {} failed: {}", wp.display(), e);
                }
            }
            if let Some(size) = streamed_size.or_else(|| std::fs::metadata(&wp).ok().map(|m| m.len())) {
                let mut c = self.cache.safe_lock();
                let parent = remote_path.parent().unwrap_or(Path::new("/")).to_path_buf();
                if let Some(dir) = c.dir_cache.get_mut(&parent) {
                    let mut files = (*dir.files).clone();
                    if let Some(e) = files.iter_mut().find(|e| e.path == remote_path) {
                        e.size = size;
                    }
                    dir.files = Arc::new(files);
                }
            }
        }
        reply.ok();
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        let wp = self.open_files.safe_lock().get(&fh.0).and_then(|of| of.write_path.clone());
        if let Some(Ok(f)) = wp.map(std::fs::File::open) {
            if let Err(e) = f.sync_all() {
                log::warn!("fsync: staging {} failed: {}", fh.0, e);
            }
        }
        reply.ok();
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let parent_path = match self.cache.safe_lock().get_path(parent.0) {
            Some(p) => p,
            None => { reply.error(Errno::ENOENT); return; }
        };
        let file_name = name.to_string_lossy().to_string();
        let full_path = parent_path.join(&file_name);

        // The synthetic `.trackerignore` overlay entry (see trackerignore_entry()):
        // reachable here only once the user has unlink()'d it, which is a
        // deliberate opt back into desktop indexing for the rest of this mount —
        // refuse recreation rather than silently reinstating it.
        if full_path == trackerignore_path() {
            reply.error(Errno::EACCES);
            return;
        }

        {
            // Released before taking `cache`: refreshes lock `cache` then `ghost_entries`.
            let ghost = self.ghost_entries.safe_lock().remove(&full_path);
            if let Some(ghost) = ghost {
                if ghost.created_at.elapsed() < GHOST_TTL {
                    if let GhostKind::HiddenAdd = ghost.kind {
                        let mut c = self.cache.safe_lock();
                        if let Some(entries) = c.get_cached_dir_readonly(&parent_path) {
                            if let Some(entry) = entries.iter().find(|e| e.path == full_path) {
                                let ino = c.get_inode(&full_path)
                                    .unwrap_or_else(|| c.allocate_inode(full_path.clone()));
                                let attr = make_file_attr(ino, entry);
                                drop(c);
                                let fh = { let mut n = self.next_fh.safe_lock(); let fh = *n; *n += 1; fh };
                                let io_kind = self.io_modes.safe_lock().acquire_plain(ino);
                                self.open_files.safe_lock().insert(fh, OpenFile {
                                    remote_path: PathBuf::new(),
                                    local: None, buf: None, write_path: None,
                                    dirty: false, original_etag: None,
                                    mime_detect_ct: None,
                                    mime_detect_max_read: 0,
                                    cache_fresh: true,
                                    next_expected_off: 0,
                                    read_ahead_window: READ_AHEAD_INITIAL,
                                    next_buf: None,
                                    prev_buf: None,
                                    lookahead_inflight: false,
                                    last_read: Instant::now(),
                                    total_written: 0,
                                    stream_eligible: false,
                                    chunk_upload: None,
                                    ino,
                                    io_kind,
                                    upload_failed: false,
                                    created: false,
                                    unlinked: false,
                                    opened_gen: self.uploads.generation(),
                                });
                                log::info!("ghost create: {} (inotify trigger)", full_path.display());
                                reply.created(&TTL, &attr, Generation(0), FileHandle(fh), plain_open_flags(io_kind));
                                return;
                            }
                        }
                    }
                }
            }
        }

        if let Err(e) = filename_validation::validate(name) {
            log::warn!("create rejected: {}", e);
            push_error(&self.error_log, PathBuf::from(&file_name), SyncErrorKind::InvalidFilename, e.to_string());
            reply.error(Errno::from_i32(e.to_errno()));
            return;
        }

        let remote_path = full_path;

        let fh = {
            let mut n = self.next_fh.safe_lock();
            let fh = *n;
            *n += 1;
            fh
        };

        let cache_dir = self.cache.safe_lock().cache_dir.clone();
        let write_path = cache_dir.join(format!("write_{}", fh));
        if let Err(e) = std::fs::File::create(&write_path) {
            log::error!("create: cannot create staging file {}: {}", write_path.display(), e);
            reply.error(Errno::EIO);
            return;
        }

        let ino = self.cache.safe_lock().allocate_inode(remote_path.clone());

        let now = SystemTime::now();
        let mut ext = backend::EntryExtensions::default();
        ext.set_str("permissions", "RGDNVW");
        let new_entry = RemoteEntry {
            path: remote_path.clone(),
            is_dir: false,
            size: 0,
            modified: Some(now),
            change_token: None,
            content_type: None,
            ext,
        };

        {
            let mut c = self.cache.safe_lock();
            if let Some(dir) = c.dir_cache.get_mut(&parent_path) {
                let mut files = (*dir.files).clone();
                files.push(new_entry.clone());
                dir.files = Arc::new(files);
            }
            // Guard against concurrent PROPFIND refreshes evicting this entry
            // before flush() adds it to uploading (same logic as put_dir_cache).
            c.uploading.insert(remote_path.clone());
        }

        let io_kind = self.io_modes.safe_lock().acquire_plain(ino);
        self.open_files.safe_lock().insert(
            fh,
            OpenFile {
                remote_path,
                local: None,
                buf: None,
                write_path: Some(write_path),
                dirty: false,
                original_etag: None,
                mime_detect_ct: None,
                mime_detect_max_read: 0,
                cache_fresh: true,
                next_expected_off: 0,
                read_ahead_window: READ_AHEAD_INITIAL,
                next_buf: None,
                prev_buf: None,
                lookahead_inflight: false,
                last_read: Instant::now(),
                total_written: 0,
                stream_eligible: true,
                chunk_upload: None,
                ino,
                io_kind,
                upload_failed: false,
                created: true,
                unlinked: false,
                opened_gen: self.uploads.generation(),
            },
        );

        let attr = make_file_attr(ino, &new_entry);
        reply.created(&TTL, &attr, Generation(0), FileHandle(fh), plain_open_flags(io_kind));
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        if is_trash_dir(name) {
            reply.error(Errno::EPERM);
            return;
        }

        if let Err(e) = filename_validation::validate(name) {
            log::warn!("mkdir rejected: {}", e);
            let full_name = name.to_string_lossy().into_owned();
            push_error(&self.error_log, PathBuf::from(&full_name), SyncErrorKind::InvalidFilename, e.to_string());
            reply.error(Errno::from_i32(e.to_errno()));
            return;
        }

        let parent_path = match self.cache.safe_lock().get_path(parent.0) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let dir_name = name.to_string_lossy().to_string();
        let remote_path = parent_path.join(&dir_name);

        {
            // Released before taking `cache`: refreshes lock `cache` then `ghost_entries`.
            let ghost = self.ghost_entries.safe_lock().remove(&remote_path);
            if let Some(ghost) = ghost {
                if ghost.created_at.elapsed() < GHOST_TTL {
                    if let GhostKind::HiddenAdd = ghost.kind {
                        let ino = self.cache.safe_lock().allocate_inode(remote_path.clone());
                        log::info!("ghost mkdir: {} (inotify trigger)", remote_path.display());
                        reply.entry(&TTL, &make_dir_attr(ino), Generation(0));
                        return;
                    }
                }
            }
        }

        let now = SystemTime::now();
        let mut ext = backend::EntryExtensions::default();
        ext.set_str("permissions", "RGDNVCK");
        let new_entry = RemoteEntry {
            path: remote_path.clone(),
            is_dir: true,
            size: 0,
            modified: Some(now),
            change_token: None,
            content_type: None,
            ext,
        };
        let ino = {
            let mut c = self.cache.safe_lock();
            let ino = c.allocate_inode(remote_path.clone());
            if let Some(dir) = c.dir_cache.get_mut(&parent_path) {
                let mut files = (*dir.files).clone();
                files.push(new_entry);
                dir.files = Arc::new(files);
            }
            // Listing the new folder before its MKCOL reaches the server must not 404.
            c.put_dir_cache(remote_path.clone(), None, None, Vec::new());
            ino
        };
        self.dirty.safe_lock().insert(parent_path);
        reply.entry(&TTL, &make_dir_attr(ino), Generation(0));

        let seq = self.journal.safe_lock().enqueue(
            mutation_journal::MutationOp::MkDir { path: remote_path.clone() },
        );

        if !self.conn.is_offline.load(Ordering::Relaxed) {
            let conn = self.conn.clone();
            let journal = self.journal.clone();
            let elog = self.error_log.clone();
            let cache = self.cache.clone();
            let ticket = self.uploads.ticket_entry(&remote_path);
            submit_mutation(move || {
                ticket.wait();
                let _permit = conn.throttle.acquire();
                match conn.backend.mkdir(&remote_path) {
                    Ok(()) => {
                        log::info!("MKCOL {}", remote_path.display());
                        journal.safe_lock().remove(seq);
                        // The server has the folder now: stop answering from the placeholder
                        // listing unless something was already created in it locally.
                        let mut c = cache.safe_lock();
                        if c.dir_cache.get(&remote_path).is_some_and(|d| d.etag.is_none() && d.files.is_empty()) {
                            c.dir_cache.remove(&remote_path);
                        }
                    }
                    Err(e) => {
                        log::error!("MKCOL {} failed (journaled): {}", remote_path.display(), e);
                        push_error(&elog, remote_path, SyncErrorKind::ServerError(0), format!("mkdir failed: {}", e));
                        journal.safe_lock().mark_failed(seq, e.to_string());
                    }
                }
            });
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        // Guard: NC 'D' (delete) flag must be present on the parent directory.
        // Blocks unlink even when the parent directory mode is 0o755 due to having 'C'.
        if let Some(p) = self.cache.safe_lock().nc_dir_perms(parent.0) {
            if !p.contains('D') {
                reply.error(Errno::EACCES);
                return;
            }
        }

        let parent_path = match self.cache.safe_lock().get_path(parent.0) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let file_name = name.to_string_lossy().to_string();
        let remote_path = parent_path.join(&file_name);

        // The synthetic `.trackerignore` overlay entry (see trackerignore_entry()):
        // there is nothing on the server to delete, so just stop put_dir_cache
        // from re-adding it and drop it from the current root listing. No
        // journal entry, no background DELETE — this never reaches the backend.
        if remote_path == trackerignore_path() {
            let mut c = self.cache.safe_lock();
            c.trackerignore_hidden = true;
            if let Some(dir) = c.dir_cache.get_mut(&parent_path) {
                let files: Vec<RemoteEntry> = dir.files.iter().filter(|e| e.path != remote_path).cloned().collect();
                dir.files = Arc::new(files);
            }
            drop(c);
            let child_ino = self.cache.safe_lock().get_inode(&remote_path).unwrap_or(0);
            reply.ok();
            // Off the fuser worker thread and after the reply: notify_* blocks on the
            // kernel dentry/inode lock and this is the only FUSE request-servicing
            // thread (n_threads=1), so calling it inline here can deadlock the whole
            // mount against a concurrent lookup on the same directory. See the read()
            // handler's notify_inval_inode comment for the full explanation.
            let notifier_slot = self.notifier_slot.clone();
            notify_later(move || {
                if let Some(notifier) = notifier_slot.safe_lock().as_ref() {
                    let _ = notifier.inval_inode(INodeNo(parent.0), 0, 0);
                    if child_ino != 0 {
                        let _ = notifier.delete(INodeNo(parent.0), INodeNo(child_ino), OsStr::new(&file_name));
                    }
                }
            });
            return;
        }

        {
            let mut ghosts = self.ghost_entries.safe_lock();
            if let Some(ghost) = ghosts.remove(&remote_path) {
                if ghost.created_at.elapsed() < GHOST_TTL {
                    if let GhostKind::VisibleDelete { .. } = ghost.kind {
                        log::info!("ghost unlink: {} (inotify trigger)", remote_path.display());
                        reply.ok();
                        return;
                    }
                }
            }
        }

        {
            let mut c = self.cache.safe_lock();
            if let Some(dir) = c.dir_cache.get_mut(&parent_path) {
                let files: Vec<RemoteEntry> = dir.files.iter().filter(|e| e.path != remote_path).cloned().collect();
                dir.files = Arc::new(files);
            }
            // Guard against racing PROPFIND refreshes re-surfacing this file
            // before the server DELETE completes (mirrors the `uploading` guard).
            c.deleting.insert(remote_path.clone());
            // Evict cached bytes immediately so a re-inserted dir entry can't serve stale content.
            c.file_cache.remove(&remote_path);
        }

        // Keep in-memory maps consistent with the delete so DETAILDIR/STATUS no longer
        // return stale records for the deleted path before the next readdir of the parent.
        if let Some(set) = self.children_map.safe_write().get_mut(&parent_path) {
            set.remove(&remote_path);
        }
        self.details.safe_write().remove(&remote_path);
        self.status.safe_write().remove(&remote_path);
        self.shared.safe_write().remove(&remote_path);
        self.fileids.safe_write().remove(&remote_path);

        self.dirty.safe_lock().insert(parent_path);
        let child_ino = self.cache.safe_lock().get_inode(&remote_path).unwrap_or(0);
        reply.ok();

        // Tell the kernel about the deletion so that other processes (e.g. Nautilus)
        // invalidate their dentry cache immediately, without waiting for the background
        // PROPFIND to complete.  Matches what proactive_refresh does for remote changes.
        //
        // MUST run off this thread and after the reply: this is the only FUSE
        // request-servicing thread (n_threads=1), and notify_inval_inode/delete block
        // on the kernel dentry/inode lock. Calling them inline here deadlocks the
        // whole mount as soon as another process has a lookup pending against the
        // same directory — see the identical fix and full explanation on read().
        {
            let notifier_slot = self.notifier_slot.clone();
            let file_name = file_name.clone();
            notify_later(move || {
                if let Some(notifier) = notifier_slot.safe_lock().as_ref() {
                    let _ = notifier.inval_inode(INodeNo(parent.0), 0, 0);
                    if child_ino != 0 {
                        let _ = notifier.delete(INodeNo(parent.0), INodeNo(child_ino), OsStr::new(&file_name));
                    }
                }
            });
        }

        // A handle still open on the deleted file must not re-create it when it is released.
        for of in self.open_files.safe_lock().values_mut() {
            if of.remote_path == remote_path {
                of.unlinked = true;
            }
        }

        let seq = self.journal.safe_lock().enqueue(
            mutation_journal::MutationOp::Unlink { path: remote_path.clone() },
        );

        if !self.conn.is_offline.load(Ordering::Relaxed) {
            let conn = self.conn.clone();
            let journal = self.journal.clone();
            let elog = self.error_log.clone();
            let cache = self.cache.clone();
            let uploads = self.uploads.clone();
            let ticket = uploads.ticket_entry(&remote_path);
            submit_mutation(move || {
                ticket.wait();
                uploads.forget(&remote_path);
                // Hold the DELETE until the file's own upload has drained. LibreOffice
                // (and similar apps) create a lock file, then delete it a moment later;
                // its PUT may still be in flight, and Nextcloud's transactional locking
                // holds the file locked during upload, so a racing DELETE comes back 423.
                // The enqueue above already coalesced away a still-queued Put, but a Put
                // already dispatched by flush() lives in the `uploading` guard, so wait on
                // that too — same drain the live MOVE uses for its source.
                let pending = || {
                    cache.safe_lock().uploading.contains(&remote_path)
                        || journal.safe_lock().has_pending_put(&remote_path)
                };
                if pending() {
                    log::info!("DELETE {}: waiting for in-flight PUT to drain", remote_path.display());
                    let deadline = std::time::Instant::now() + Duration::from_secs(30);
                    while pending() && std::time::Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(50));
                    }
                }
                let _permit = conn.throttle.acquire();
                match conn.backend.delete(&remote_path) {
                    Ok(()) => {
                        log::info!("DELETE {}", remote_path.display());
                        journal.safe_lock().remove(seq);
                        cache.safe_lock().deleting.remove(&remote_path);
                    }
                    Err(backend::BackendWriteError::Server(404, _)) => {
                        log::debug!("DELETE {} — already gone (idempotent)", remote_path.display());
                        journal.safe_lock().remove(seq);
                        cache.safe_lock().deleting.remove(&remote_path);
                    }
                    Err(e) if e.is_transient() => {
                        // Server briefly down/overloaded or the resource is locked (423).
                        // Keep the Unlink journaled and let the connectivity monitor's
                        // replay retry it — no user-facing error, since nothing is wrong
                        // with the delete itself. Matches the PUT path and journal replay.
                        // Leave the path in `deleting` — the journal will replay the DELETE
                        // and clear it on success.
                        log::warn!("DELETE {} deferred — {} (queued for retry)", remote_path.display(), e);
                        journal.safe_lock().mark_deferred(seq, e.to_string());
                    }
                    Err(e) => {
                        log::error!("DELETE {} failed (journaled): {}", remote_path.display(), e);
                        push_error(&elog, remote_path.clone(), SyncErrorKind::ServerError(0), format!("delete failed: {}", e));
                        journal.safe_lock().mark_failed(seq, e.to_string());
                        cache.safe_lock().deleting.remove(&remote_path);
                    }
                }
            });
        } else {
            // Offline: the DELETE is journaled for later replay. Clear the guard now
            // so it doesn't persist across a reconnect unnecessarily — the journal
            // replay will re-issue the DELETE against the live server.
            self.cache.safe_lock().deleting.remove(&remote_path);
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if let Some(p) = self.cache.safe_lock().nc_dir_perms(parent.0) {
            if !p.contains('D') {
                reply.error(Errno::EACCES);
                return;
            }
        }

        let parent_path = match self.cache.safe_lock().get_path(parent.0) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let dir_name = name.to_string_lossy().to_string();
        let remote_path = parent_path.join(&dir_name);

        {
            let mut ghosts = self.ghost_entries.safe_lock();
            if let Some(ghost) = ghosts.remove(&remote_path) {
                if ghost.created_at.elapsed() < GHOST_TTL {
                    if let GhostKind::VisibleDelete { .. } = ghost.kind {
                        log::info!("ghost rmdir: {} (inotify trigger)", remote_path.display());
                        reply.ok();
                        return;
                    }
                }
            }
        }

        {
            let mut c = self.cache.safe_lock();
            if let Some(dir) = c.dir_cache.get_mut(&parent_path) {
                let files: Vec<RemoteEntry> = dir.files.iter().filter(|e| e.path != remote_path).cloned().collect();
                dir.files = Arc::new(files);
            }
            c.dir_cache.remove(&remote_path);
            // Guard against racing PROPFIND refreshes re-surfacing this directory
            // before the server DELETE completes (mirrors the `uploading` guard in
            // unlink and the `deleting` guard added for files).
            c.deleting.insert(remote_path.clone());
        }
        if let Some(set) = self.children_map.safe_write().get_mut(&parent_path) {
            set.remove(&remote_path);
        }
        self.details.safe_write().remove(&remote_path);
        self.status.safe_write().remove(&remote_path);
        self.shared.safe_write().remove(&remote_path);
        self.fileids.safe_write().remove(&remote_path);

        self.dirty.safe_lock().insert(parent_path);
        let child_ino = self.cache.safe_lock().get_inode(&remote_path).unwrap_or(0);
        reply.ok();

        // MUST run off this thread and after the reply — same deadlock hazard as
        // unlink()/read(): notify_inval_inode/delete block on the kernel dentry/inode
        // lock, and this is the only FUSE request-servicing thread (n_threads=1).
        {
            let notifier_slot = self.notifier_slot.clone();
            let dir_name = dir_name.clone();
            notify_later(move || {
                if let Some(notifier) = notifier_slot.safe_lock().as_ref() {
                    let _ = notifier.inval_inode(INodeNo(parent.0), 0, 0);
                    if child_ino != 0 {
                        let _ = notifier.delete(INodeNo(parent.0), INodeNo(child_ino), OsStr::new(&dir_name));
                    }
                }
            });
        }

        let seq = self.journal.safe_lock().enqueue(
            mutation_journal::MutationOp::RmDir { path: remote_path.clone() },
        );

        if !self.conn.is_offline.load(Ordering::Relaxed) {
            let conn = self.conn.clone();
            let journal = self.journal.clone();
            let cache = self.cache.clone();
            let elog = self.error_log.clone();
            // Waits for every upload inside the folder (they hold it Shared).
            let ticket = self.uploads.ticket_entry(&remote_path);
            submit_mutation(move || {
                ticket.wait();
                let _permit = conn.throttle.acquire();
                match conn.backend.delete(&remote_path) {
                    Ok(()) => {
                        log::info!("RMDIR {}", remote_path.display());
                        journal.safe_lock().remove(seq);
                        cache.safe_lock().deleting.remove(&remote_path);
                    }
                    Err(backend::BackendWriteError::Server(404, _)) => {
                        log::debug!("RMDIR {} — already gone (idempotent)", remote_path.display());
                        journal.safe_lock().remove(seq);
                        cache.safe_lock().deleting.remove(&remote_path);
                    }
                    Err(e) if e.is_transient() => {
                        log::warn!("RMDIR {} deferred — {} (queued for retry)", remote_path.display(), e);
                        journal.safe_lock().mark_deferred(seq, e.to_string());
                    }
                    Err(e) => {
                        log::error!("RMDIR {} failed (journaled): {}", remote_path.display(), e);
                        push_error(&elog, remote_path.clone(), SyncErrorKind::ServerError(0), format!("rmdir failed: {}", e));
                        journal.safe_lock().mark_failed(seq, e.to_string());
                        cache.safe_lock().deleting.remove(&remote_path);
                    }
                }
            });
        } else {
            self.cache.safe_lock().deleting.remove(&remote_path);
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        if let Err(e) = filename_validation::validate(newname) {
            log::warn!("rename rejected: {}", e);
            let full_name = newname.to_string_lossy().into_owned();
            push_error(&self.error_log, PathBuf::from(&full_name), SyncErrorKind::InvalidFilename, e.to_string());
            reply.error(Errno::from_i32(e.to_errno()));
            return;
        }

        // Guard: same-dir rename requires 'N'; cross-dir move requires 'V' on source.
        {
            let c = self.cache.safe_lock();
            let src_perms = c.nc_dir_perms(parent.0);
            if let Some(ref p) = src_perms {
                let cross_dir = parent.0 != newparent.0;
                let required = if cross_dir { 'V' } else { 'N' };
                if !p.contains(required) {
                    reply.error(Errno::EACCES);
                    return;
                }
            }
        }

        let (old_parent_path, new_parent_path) = {
            let c = self.cache.safe_lock();
            match (c.get_path(parent.0), c.get_path(newparent.0)) {
                (Some(a), Some(b)) => (a, b),
                _ => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        let old_name = name.to_string_lossy().to_string();
        let new_name = newname.to_string_lossy().to_string();
        let from = old_parent_path.join(&old_name);
        let to = new_parent_path.join(&new_name);

        {
            let mut ghosts = self.ghost_entries.safe_lock();
            let matched = {
                let from_ghost = ghosts.get(&from);
                let to_ghost = ghosts.get(&to);
                if let (Some(fg), Some(tg)) = (from_ghost, to_ghost) {
                    fg.created_at.elapsed() < GHOST_TTL
                        && tg.created_at.elapsed() < GHOST_TTL
                        && fg.rename_pair_id.is_some()
                        && fg.rename_pair_id == tg.rename_pair_id
                        && matches!(fg.kind, GhostKind::VisibleDelete { .. })
                        && matches!(tg.kind, GhostKind::HiddenAdd)
                } else {
                    false
                }
            };
            if matched {
                ghosts.remove(&from);
                ghosts.remove(&to);
                log::info!("ghost rename: {} → {} (inotify trigger)", from.display(), to.display());
                reply.ok();
                return;
            }
        }

        {
            let mut c = self.cache.safe_lock();
            let mut moved_entry = None;
            if let Some(dir) = c.dir_cache.get_mut(&old_parent_path) {
                let (keep, removed): (Vec<_>, Vec<_>) = dir.files.iter().cloned().partition(|e| e.path != from);
                dir.files = Arc::new(keep);
                moved_entry = removed.into_iter().next();
            }
            if let Some(mut entry) = moved_entry {
                // rename() can arrive at the FUSE dispatcher concurrently with flush() or
                // even before it (multi-threaded fuser dispatches ops in parallel).  When
                // the source was just created via create(), its dir-cache size is 0 until
                // flush() does its synchronous update — which may not have run yet.  Read
                // the staging file's actual size so the optimistic update shows the right
                // byte count immediately.
                let staged_size = self.open_files.safe_lock()
                    .values()
                    .find(|of| of.remote_path == from)
                    .and_then(|of| of.write_path.as_ref())
                    .and_then(|wp| std::fs::metadata(wp).ok())
                    .map(|m| m.len());
                if let Some(sz) = staged_size {
                    entry.size = sz;
                }
                entry.path = to.clone();
                if let Some(dir) = c.dir_cache.get_mut(&new_parent_path) {
                    let mut files = (*dir.files).clone();
                    // Remove any existing entry for the target path (overwrite semantics).
                    files.retain(|f| f.path != to);
                    files.push(entry);
                    dir.files = Arc::new(files);
                }
            }
            // Keep the inode-to-path map consistent with the rename. Without this,
            // getattr(ino) resolves the inode to the old source path, finds nothing in
            // dir_cache, and returns ENOENT — causing the renamed file to disappear.
            if let Some(ino) = c.paths.remove(&from) {
                c.inodes.insert(ino, to.clone());
                if let Some(displaced) = c.paths.insert(to.clone(), ino) {
                    if displaced != ino {
                        c.inodes.remove(&displaced);
                    }
                }
            }
        }
        // Keep in-memory maps consistent with the rename so DETAILDIR/STATUS reflect the new
        // path immediately, without waiting for the next readdir of either directory.
        {
            let mut cm = self.children_map.safe_write();
            if let Some(set) = cm.get_mut(&old_parent_path) { set.remove(&from); }
            cm.entry(new_parent_path.clone())
                .or_insert_with(HashSet::new)
                .insert(to.clone());
        }
        {
            let mut dt = self.details.safe_write();
            if let Some(detail) = dt.remove(&from) { dt.insert(to.clone(), detail); }
        }
        {
            let mut st = self.status.safe_write();
            if let Some(s) = st.remove(&from) { st.insert(to.clone(), s); }
        }
        {
            let mut sh = self.shared.safe_write();
            if sh.remove(&from) { sh.insert(to.clone()); }
        }
        {
            let mut fi = self.fileids.safe_write();
            if let Some(fid) = fi.remove(&from) { fi.insert(to.clone(), fid); }
        }

        let same_parent = old_parent_path == new_parent_path;
        self.dirty.safe_lock().insert(old_parent_path);
        if !same_parent {
            self.dirty.safe_lock().insert(new_parent_path);
        }
        reply.ok();

        // Handles still open under the source now commit to the destination. One made by
        // create() means the server has no copy of the source yet, so there is nothing to MOVE.
        let mut uncommitted_source = false;
        {
            let mut files = self.open_files.safe_lock();
            for of in files.values_mut() {
                if let Ok(suffix) = of.remote_path.strip_prefix(&from) {
                    if suffix.as_os_str().is_empty() {
                        uncommitted_source |= of.created && !of.unlinked;
                        of.remote_path = to.clone();
                    } else {
                        of.remote_path = to.join(suffix);
                    }
                }
            }
        }
        if uncommitted_source && !self.journal.safe_lock().has_pending_put(&from) {
            log::info!("rename {} → {}: source not uploaded yet — retargeted, no MOVE needed", from.display(), to.display());
            let mut c = self.cache.safe_lock();
            if c.uploading.remove(&from) {
                c.uploading.insert(to.clone());
            }
            return;
        }

        let seq = self.journal.safe_lock().enqueue(
            mutation_journal::MutationOp::Rename { from: from.clone(), to: to.clone() },
        );

        if !self.conn.is_offline.load(Ordering::Relaxed) {
            let conn = self.conn.clone();
            let journal = self.journal.clone();
            let elog = self.error_log.clone();
            let cache = self.cache.clone();
            let uploads = self.uploads.clone();
            let root = Path::new("/");
            let ticket = uploads.seq.ticket(&[
                (from.as_path(), path_seq::Access::Exclusive),
                (to.as_path(), path_seq::Access::Exclusive),
                (from.parent().unwrap_or(root), path_seq::Access::Shared),
                (to.parent().unwrap_or(root), path_seq::Access::Shared),
            ]);
            submit_mutation(move || {
                // Runs after every earlier change to either path, e.g. the source's upload.
                ticket.wait();
                // If the source was just created via create() + flush(), the PUT runs
                // asynchronously and the file may not yet exist on the server when this
                // MOVE fires.  Wait until BOTH the in-flight upload guard is cleared AND
                // no Put for the source is still queued in the journal before sending the
                // MOVE, so the server has the file content in place first.  The `uploading`
                // guard only covers PUTs started by the online flush path; offline-created
                // files (or ones behind a backlog) sit in the journal with no guard, so the
                // journal check is what stops the MOVE from racing ahead of a queued PUT and
                // getting a 404 for a source that was never uploaded yet.
                let source_pending = || {
                    cache.safe_lock().uploading.contains(&from)
                        || journal.safe_lock().has_pending_put(&from)
                };
                if source_pending() {
                    log::info!("MOVE {} → {}: waiting for source PUT to complete", from.display(), to.display());
                }
                let deadline = std::time::Instant::now() + Duration::from_secs(30);
                loop {
                    if !source_pending() {
                        break;
                    }
                    if std::time::Instant::now() >= deadline {
                        log::warn!("MOVE {} → {}: timed out waiting for source PUT", from.display(), to.display());
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                let _permit = conn.throttle.acquire();
                log::info!("MOVE {} → {}: sending WebDAV MOVE", from.display(), to.display());
                match conn.backend.rename(&from, &to) {
                    Ok(()) => {
                        log::info!("MOVE {} → {}", from.display(), to.display());
                        uploads.moved(&from, &to);
                        journal.safe_lock().remove(seq);
                    }
                    Err(backend::BackendWriteError::Server(404, _)) => {
                        // Source is gone on the server. Reconcile the same way the journal
                        // replay does (MoveSourceGone) instead of surfacing a misleading
                        // "rename failed" error and leaving the entry to retry forever.
                        log::warn!("MOVE {} → {}: source gone on server (404) — recording move conflict", from.display(), to.display());
                        let mut j = journal.safe_lock();
                        j.add_conflict(mutation_journal::ConflictKind::MoveSourceGone {
                            from: from.clone(),
                            to: to.clone(),
                        });
                        j.remove(seq);
                    }
                    Err(e) => {
                        log::error!("MOVE {} → {} failed (journaled): {}", from.display(), to.display(), e);
                        push_error(&elog, from, SyncErrorKind::ServerError(0), format!("rename failed: {}", e));
                        journal.safe_lock().mark_failed(seq, e.to_string());
                    }
                }
            });
        }
    }
}

/// The read clients for one transport: one per download slot (see
/// `http_clients::DOWNLOAD_CONNECTIONS`), each its own connection.
fn build_read_clients(http3: bool) -> Result<Vec<reqwest::blocking::Client>, String> {
    let read_builder = || {
        // `timeout` here is the per-operation stall bound, not a total deadline;
        // see READ_STALL_TIMEOUT for why it must live on the client.
        let mut read = crate::http_clients::with_pooled_dns(reqwest::blocking::Client::builder())
            .pool_max_idle_per_host(0)
            .tcp_nodelay(true)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(READ_STALL_TIMEOUT);
        if http3 {
            // The h3 pool ignores pool_max_idle_per_host and connect_timeout
            // never reaches the QUIC connector, so the TCP read client's
            // "connect fresh so a dead path fails in seconds" trick has no QUIC
            // equivalent. It used to be approximated with `pool_idle_timeout(1s)`,
            // which made every window of a rate-limited reader (a media player)
            // pay a fresh QUIC+TLS handshake (120-280 ms to this server) and a
            // new slow start. Worse, reqwest 0.13's h3 pool stamps a connection's
            // idle clock when it is *checked out*, not when the request ends, so
            // even a connection busy streaming for over a second counted as
            // expired at the next window.
            //
            // That fail-fast guarantee now comes from the read path itself: the
            // client-level READ_STALL_TIMEOUT bounds the header wait and every
            // body read, FIRST_BYTES_DEADLINE bounds what a READ waits for, and a
            // stalled window aborts and resumes on a fresh request. So warm
            // connections — and their congestion state — are kept for just under
            // `http3_max_idle_timeout`, past which quinn would close them anyway.
            // (reqwest exposes no QUIC keep-alive, so an idle connection does
            // expire after 30 s of silence; the pool drops it as invalid.)
            //
            // `http3_max_idle_timeout` used to also be set to 5s here, to
            // reproduce the TCP connect_timeout's fail-fast bound. But unlike
            // connect_timeout — which only bounds the connect phase —
            // max_idle_timeout governs an *already-established* connection's
            // tolerance for silence in either direction for its entire
            // lifetime. At 5s, any gap that long during an active read-ahead
            // stream (a slow server response under load, a brief network
            // hiccup, this process not being scheduled promptly for a few
            // seconds) tore down an otherwise-healthy QUIC connection —
            // observed live as "read-ahead stream broke: request or response
            // body error" during ordinary playback, not just on a dead path.
            // quinn's own upstream default is 30s (`quinn_proto::TransportConfig`),
            // which is also what Chrome's QUIC stack uses; restoring that
            // gives a real connection enough slack to survive realistic
            // jitter without materially weakening dead-path detection — the
            // periodic connectivity probe (see `http_clients`/offline
            // handling) doesn't depend on any single read's timeout, and a
            // still-broken stream now retries in-process (see the read-ahead
            // body loop) instead of surfacing straight to the caller.
            //
            // The stream receive window is also bumped from quinn's default
            // (~1.25 MB, sized for 100 Mbps at 100 ms RTT), but no further than
            // H3_STREAM_RECEIVE_WINDOW: a larger one lets enough loss gaps pile
            // up for quinn to close the connection. BBR is a better fit than the default CUBIC for
            // exactly this kind of path (real-world jitter / non-congestion
            // loss, which CUBIC misreads as congestion and backs off from
            // unnecessarily).
            read = read.http3_prior_knowledge()
                .pool_idle_timeout(H3_READ_POOL_IDLE)
                .http3_max_idle_timeout(H3_MAX_IDLE)
                .http3_stream_receive_window(H3_STREAM_RECEIVE_WINDOW)
                .http3_congestion_bbr();
        }
        read
    };
    (0..crate::http_clients::DOWNLOAD_CONNECTIONS)
        .map(|_| read_builder().build().map_err(|e| format!("HTTP read client: {}", e)))
        .collect()
}

// ── HTTP Range reads ─────────────────────────────────────────────────────────

fn webdav_file_url(base: &str, remote_path: &Path) -> String {
    let rel = remote_path.strip_prefix("/").unwrap_or(remote_path);
    let encoded = utf8_percent_encode(&rel.to_string_lossy(), PATH_ENCODE).to_string();
    format!("{}/{}", base.trim_end_matches('/'), encoded)
}

/// The `read_throttle` slot a range request goes out on.
enum Slot<'a> {
    /// Take one (timed, see `do_range_read_stream`), leaving `spare` free.
    Take { spare: usize },
    /// Already taken by the caller, who sized the request knowing it had this slot.
    /// Used for the first attempt; a retry gives it back and takes one like `Take`.
    Held { permit: ThrottleGuard<'a>, spare: usize },
}

/// Opens a range GET for `size` bytes at `offset`, retrying transport failures.
///
/// Every attempt holds a `read_throttle` slot and goes out on that slot's own read
/// client, so it never shares a connection with another download. Slots are taken
/// with a timeout while at least `spare` others stay free (0 for a foreground
/// read). Everything here — slot waits, attempts, backoff — is bounded by
/// `deadline`, so a caller holding a FUSE reply always gets an answer; running out
/// of slots returns [`READ_SLOTS_BUSY_ERR`].
fn do_range_read_stream<'a>(
    conn: &'a ConnInfo,
    path: &Path,
    offset: u64,
    size: usize,
    slot: Slot<'a>,
    deadline: Instant,
) -> Result<(reqwest::blocking::Response, ThrottleGuard<'a>), String> {
    // Offline is not a verdict on this read yet: give a blip the remainder of the
    // grace window to clear before refusing, and refuse with a *transient* error so
    // callers retry instead of treating the file as unreadable.
    if !wait_out_offline_blip(conn) {
        return Err(OFFLINE_READ_ERR.into());
    }
    let url = webdav_file_url(&conn.webdav_url, path);
    let end = offset + size as u64 - 1;
    let mut delay = Duration::from_millis(500);
    let (mut held, spare) = match slot {
        Slot::Take { spare } => (None, spare),
        Slot::Held { permit, spare } => (Some(permit), spare),
    };
    for attempt in 0u32..=2 {
        // Timed, never `acquire()`: an untimed wait here left READs unanswered for
        // good once every slot's holder was itself stuck.
        let permit = match held.take() {
            Some(p) => p,
            None => {
                let wait = READ_SLOT_WAIT.min(deadline.saturating_duration_since(Instant::now()));
                match conn.read_throttle.acquire_leaving(spare, wait) {
                    Some(p) => p,
                    None => return Err(READ_SLOTS_BUSY_ERR.into()),
                }
            }
        };
        // Re-read the client each attempt so a demotion to HTTP/2 (see `http_clients`)
        // takes effect on the retry rather than only on the next read.
        //
        // No `.timeout()`: that would also be a total deadline on the body. The read
        // client's READ_STALL_TIMEOUT bounds the header wait and every body read.
        let req = conn.clients.read(permit.slot())
            .get(&url)
            .header("Range", format!("bytes={}-{}", offset, end));
        match conn.creds.apply(req).send() {
            Ok(resp) => {
                let status = resp.status();
                // A 206 is the range asked for. A 200 is the whole file from byte 0:
                // fine for a range that starts there (every consumer stops reading at
                // its own length), wrong bytes for any other offset.
                if status == reqwest::StatusCode::PARTIAL_CONTENT {
                    // A 206 must be the range asked for: starting anywhere else would
                    // put the wrong bytes at every offset of the window.
                    let range = resp.headers()
                        .get(reqwest::header::CONTENT_RANGE)
                        .and_then(|v| v.to_str().ok())
                        .and_then(parse_content_range_span);
                    return match range {
                        Some((first, last)) if first == offset && last <= end => Ok((resp, permit)),
                        other => Err(format!(
                            "range read for bytes {}-{} got Content-Range {:?}", offset, end, other,
                        )),
                    };
                }
                return if status == reqwest::StatusCode::OK && offset == 0 {
                    Ok((resp, permit))
                } else {
                    Err(format!("range read returned {} for offset {}", status, offset))
                };
            }
            Err(e) if crate::http_clients::is_dns_refusal(&e) => {
                // Our own lookup pool could not take the lookup: a busy daemon, not an
                // unreachable server. Never a reason to go offline.
                return Err(DNS_BUSY_ERR.into());
            }
            Err(e) if e.is_timeout() => {
                // No response headers within READ_STALL_TIMEOUT. On QUIC that is most
                // often a silently dead connection, so steer the retry to another one
                // (see Throttle::quarantine). It is not proof the server is gone — a
                // busy PHP backend can be this slow — so it must not flip the mount
                // offline: callers see READ_HEADER_TIMEOUT_ERR, and the connectivity
                // monitor is asked to probe now and decide.
                conn.read_throttle.quarantine(permit.slot(), H3_MAX_IDLE);
                request_probe(conn);
                let time_left = deadline.saturating_duration_since(Instant::now()) > delay;
                if attempt < 2 && time_left {
                    log::warn!("range read {} got no response headers within {:?} on slot {} (attempt {}/3) — retrying on another connection",
                        path.display(), READ_STALL_TIMEOUT, permit.slot(), attempt + 1);
                    drop(permit);
                    continue;
                }
                return Err(READ_HEADER_TIMEOUT_ERR.into());
            }
            Err(e) => {
                let msg = e.to_string();
                let time_left = deadline.saturating_duration_since(Instant::now()) > delay;
                if attempt < 2 && time_left && (is_transient_network_err(&msg) || is_timeout_err(&msg)) {
                    log::warn!("range read {} failed (attempt {}/3): {} — retrying in {:?}",
                        path.display(), attempt + 1, msg, delay);
                    drop(permit);
                    thread::sleep(delay);
                    delay = (delay * 2).min(Duration::from_secs(4));
                    continue;
                }
                return Err(msg);
            }
        }
    }
    unreachable!()
}

/// Parse the first and last byte out of a `Content-Range: bytes 0-499/1234`
/// header.
fn parse_content_range_span(v: &str) -> Option<(u64, u64)> {
    let span = v.trim().strip_prefix("bytes")?.trim_start().split('/').next()?;
    let (a, b) = span.split_once('-')?;
    let (a, b) = (a.trim().parse::<u64>().ok()?, b.trim().parse::<u64>().ok()?);
    (a <= b).then_some((a, b))
}

/// Parse the total length out of a `Content-Range: bytes 0-499/1234` header.
/// Returns None for an unknown total (`*`) or a malformed value.
fn parse_content_range_total(v: &str) -> Option<u64> {
    let total = v.rsplit('/').next()?.trim();
    if total == "*" {
        return None;
    }
    total.parse::<u64>().ok()
}

/// What `open_window` is asked to open.
struct WindowRequest<'a> {
    /// File offset of the window's first byte.
    start: u64,
    /// Bytes wanted; the budget may shrink it (see `reserved`).
    target: usize,
    /// What the waiting READ wants from the window's start (0 for a look-ahead).
    need: usize,
    /// Dir-cache size, 0 = unknown.
    file_size: u64,
    /// `read_throttle` slots to leave free (0 for a foreground read).
    spare: usize,
    deadline: Instant,
    /// A slot the caller already holds — a look-ahead takes one before it is even
    /// queued, so it never waits for one on a worker.
    primary: Option<ThrottleGuard<'a>>,
    /// The window's memory, if the caller reserved it already (a look-ahead does,
    /// all or nothing); otherwise `open_window` reserves what the budget allows.
    reserved: Option<BufferReservation>,
}

/// A window whose first request is open, with the slots for the rest of it.
struct OpenedWindow<'a> {
    /// The first segment's response, and the slot it holds.
    resp: reqwest::blocking::Response,
    permit: ThrottleGuard<'a>,
    /// One more slot per further segment in `plan`, each already taken, each with
    /// the token for the thread that will fetch it.
    extras: Vec<(ThrottleGuard<'a>, bg::SegmentThread)>,
    plan: Vec<Segment>,
    /// Bytes the window will hold at most: its `ReadAheadBuf::target_len`.
    target: u64,
    /// The file's length as this response states it: the Content-Range total, or
    /// the Content-Length of a 200 for a window at offset 0.
    total: Option<u64>,
    budget: BufferReservation,
}

/// Takes the slots and memory for a window and opens its first request.
///
/// The first slot is waited for (bounded by READ_SLOT_WAIT and the deadline,
/// leaving `spare` free) unless the caller brings one; the extra ones for a large
/// window's other segments are only *tried* — slot and segment-thread token both
/// taken if free right now, never waited for — so a READ is never held up by the
/// fan-out. With nothing extra free the window is the single request it always
/// was. A response whose Content-Range disagrees with the dir-cache size drops the
/// fan-out, so a stale size can neither send a segment past the end nor stop the
/// window short.
fn open_window<'a>(conn: &'a ConnInfo, path: &Path, req: WindowRequest<'a>) -> Result<OpenedWindow<'a>, String> {
    let WindowRequest { start, target, need, file_size, spare, deadline, primary, reserved } = req;
    if !wait_out_offline_blip(conn) {
        return Err(OFFLINE_READ_ERR.into());
    }
    let primary = match primary {
        Some(p) => p,
        None => {
            let wait = READ_SLOT_WAIT.min(deadline.saturating_duration_since(Instant::now()));
            conn.read_throttle.acquire_leaving(spare, wait)
                .ok_or_else(|| READ_SLOTS_BUSY_ERR.to_string())?
        }
    };
    let budget = reserved.unwrap_or_else(|| {
        BufferReservation::up_to(target as u64, need.max(READ_AHEAD_INITIAL) as u64)
    });
    let mut budget = budget;
    let target = (budget.0 as usize).min(target).max(need.min(target));
    let span = (file_size > start).then(|| file_size - start);
    let max_segments = if segments_worthwhile() { MAX_WINDOW_SEGMENTS } else { 1 };
    let most = segment_plan(target, span, max_segments, need).len();
    let mut extras = Vec::new();
    while extras.len() + 1 < most {
        let Some(token) = bg::SegmentThread::try_take() else { break };
        match conn.read_throttle.acquire_leaving(spare.max(SEGMENT_SPARE_SLOTS), Duration::ZERO) {
            Some(p) => extras.push((p, token)),
            None => break,
        }
    }
    let mut plan = segment_plan(target, span, 1 + extras.len(), need);
    // Moving a parked segment onto the prefix briefly holds its bytes twice, so a
    // segmented window's peak is the window plus its largest segment. Reserve that
    // too, or fetch the window as one segment.
    if plan.len() > 1 {
        let window: u64 = plan.iter().map(|s| s.len).sum();
        let peak = window + plan.iter().map(|s| s.len).max().unwrap_or(0);
        if !budget.grow_to(peak) {
            plan = segment_plan(target, span, 1, need);
        }
    }
    // Rounding can make fewer segments than slots; hand the surplus straight back.
    extras.truncate(plan.len() - 1);
    let (resp, permit) = do_range_read_stream(
        conn, path, start, plan[0].len as usize, Slot::Held { permit: primary, spare }, deadline,
    )?;
    let total = resp.headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_content_range_total)
        .or_else(|| if start == 0 && resp.status() == reqwest::StatusCode::OK { resp.content_length() } else { None });
    if plan.len() > 1 && (resp.status() != reqwest::StatusCode::PARTIAL_CONTENT || total != Some(file_size)) {
        log::info!(
            "{}: server size {:?} differs from cached {} (or no 206) — fetching this window as one segment",
            path.display(), total, file_size,
        );
        extras.clear();
        plan.truncate(1);
        plan[0].last = true;
    }
    let target: u64 = plan.iter().map(|s| s.len).sum();
    // Keep only what this window can actually use (see the peak note above).
    let peak = if plan.len() > 1 { target + plan.iter().map(|s| s.len).max().unwrap_or(0) } else { target };
    budget.shrink_to(peak);
    Ok(OpenedWindow { resp, permit, extras, plan, target, total, budget })
}

/// Transport breaks resumed per segment. A body read error mid-stream is usually
/// a transient blip (dropped connection, reset stream) rather than the server
/// actually having nothing left to give — resume with a fresh Range request for
/// exactly the missing tail instead of giving up immediately. Readers parked on
/// the window would otherwise get EIO (see `wait_on_window`), which every player
/// has to notice and recover from itself; VLC in particular does this slowly
/// enough to look like a stall.
///
/// The budget is per run of trouble, not per segment: it refills once a resumed
/// stream has delivered BODY_RETRY_REFILL_BYTES, so a long segment that loses
/// its connection now and then keeps going, while one that breaks over and over
/// without progress still gives up after this many tries.
const MAX_BODY_RETRIES: u32 = 3;
/// Bytes a resumed stream must deliver before its break budget refills.
const BODY_RETRY_REFILL_BYTES: u64 = 4 * 1024 * 1024;
/// Stalls resumed per segment. Kept separate from MAX_BODY_RETRIES: a stall costs
/// READ_STALL_TIMEOUT already, so it resumes without backoff, and a QUIC stream
/// that went silent says nothing about whether a fresh request will.
const MAX_STALL_RESUMES: u32 = 2;
/// Least a window (all its segments together) must receive per READ_STALL_TIMEOUT
/// to count as flowing. The per-read timeout resets on every byte, so on its own a
/// body trickling a few bytes at a time would hold its slot for as long as it
/// liked. ~4 KiB/s: far below any link worth streaming over, so a slow link stays
/// slow rather than turning into resumes and errors.
const MIN_BODY_PROGRESS: u64 = 64 * 1024;
/// How often a pump checks, by time, whether anyone still wants its window. It
/// also checks every 2 MB; the timer covers a slow body.
const SUPERSEDE_CHECK_EVERY: Duration = Duration::from_secs(1);

/// Fetches the rest of one read-ahead window into its shared buffer.
struct WindowPump<'a> {
    conn: &'a ConnInfo,
    path: &'a Path,
    fh: u64,
    open_files: &'a Mutex<HashMap<u64, OpenFile>>,
    tmap: &'a TransferMap,
    /// This window's own TRANSFERS entry, removed when the pump ends.
    tkey: TransferKey,
    shared: &'a Arc<(Mutex<StreamState>, Condvar)>,
    /// File offset of the window's first byte.
    start: u64,
    /// How many `read_throttle` slots resume requests must leave free (see
    /// `do_range_read_stream`).
    spare: usize,
    /// The file's length as the server stated it (`OpenedWindow::total`).
    total: Option<u64>,
}

/// Marks a window finished — `done`, waiters woken, TRANSFERS entry gone — when
/// its pump ends, however it ends (unwinding included), so no reader can be left
/// waiting on a window nothing fills any more.
///
/// A window that ends short of its target without the prefix reaching the proven
/// end of file — a segment panicked, or exited on a path that marked nothing — is
/// also marked stopped, so its waiters get EAGAIN rather than EIO or a short reply.
struct WindowDone<'p, 'a> {
    pump: &'p WindowPump<'a>,
    /// Bytes the window was planned to hold.
    target: u64,
}

impl Drop for WindowDone<'_, '_> {
    fn drop(&mut self) {
        let (ref mtx, ref cv) = **self.pump.shared;
        {
            let mut ss = mtx.lock().unwrap_or_else(|e| e.into_inner());
            let short = (ss.data.len() as u64) < self.target && !ss.at_eof() && self.pump.total.is_some();
            if !ss.halted() && (std::thread::panicking() || short) {
                ss.stopped = true;
            }
            ss.done = true;
        }
        cv.notify_all();
        self.pump.tmap.safe_lock().remove(&self.pump.tkey);
    }
}

/// How one body read went.
enum BodyRead {
    Bytes(usize),
    End,
    /// Failed or crawled; `stalled` = no (or too little) progress for
    /// READ_STALL_TIMEOUT, as opposed to the transport breaking.
    Failed { stalled: bool, why: String },
}

impl<'a> WindowPump<'a> {
    /// Whether nobody can read this window any more: the handle was released, or
    /// a seek or promotion replaced its buffer (as the current window or the
    /// look-ahead) — and no READ is still parked on it. Waiters count as readers:
    /// a sequential reader's async READs for the old window's tail are often still
    /// parked when the look-ahead gets promoted, and stopping then failed them.
    fn superseded(&self) -> bool {
        if self.shared.0.lock().unwrap().waiters > 0 {
            return false;
        }
        let ofs = self.open_files.safe_lock();
        let Some(of) = ofs.get(&self.fh) else { return true };
        let holds = |b: &Option<ReadAheadBuf>| b.as_ref().is_some_and(|b| Arc::ptr_eq(&b.stream, self.shared));
        !(holds(&of.buf) || holds(&of.next_buf) || holds(&of.prev_buf))
    }

    /// A segment gave up for good: nothing past its hole can ever reach readers,
    /// and those waiting inside it get EIO.
    fn break_window(&self) {
        let (ref mtx, ref cv) = **self.shared;
        mtx.lock().unwrap().broken = true;
        cv.notify_all();
    }

    /// A segment stopped for a reason retrying can fix (superseded, no slot for a
    /// resume, the server too slow): halts the window like `break_window`, but
    /// waiters inside the hole get EAGAIN. Every early exit of a segment marks the
    /// window one way or the other, so a hole can never look like the file's end.
    fn stop_window(&self) {
        let (ref mtx, ref cv) = **self.shared;
        mtx.lock().unwrap().stopped = true;
        cv.notify_all();
    }

    /// The window-wide trickle rule: once a READ_STALL_TIMEOUT period is over, the
    /// window as a whole — all its segments together — must have received
    /// MIN_BODY_PROGRESS in it. Measured per window, not per segment: a slow link
    /// shared by segments and readers is still progress, where a per-segment
    /// floor turned every stream on a ~256 kbit/s link into a "stall".
    fn window_crawling(&self) -> Option<String> {
        let mut ss = self.shared.0.lock().unwrap();
        let (at, base) = ss.progress_mark;
        if at.elapsed() < READ_STALL_TIMEOUT {
            return None;
        }
        let received = ss.received() as u64;
        let gained = received.saturating_sub(base);
        ss.progress_mark = (Instant::now(), received);
        (gained < MIN_BODY_PROGRESS).then(|| format!("window received only {} bytes in {:?}", gained, at.elapsed()))
    }

    /// Fetches the window: `first` is the open first segment, `extras` one held
    /// slot (and segment-thread token) per further segment of `plan`. The first
    /// segment runs on this thread; each other one on its own scoped thread, which
    /// owns its slot, token, client and response. Returns the bytes the window
    /// ended up holding.
    ///
    /// Why nothing can leak (every path, including errors, stalls, supersede,
    /// release and unwinding):
    /// - threads: `std::thread::scope` joins every segment thread before `run`
    ///   returns, and each holds a `bg::SegmentThread` token from start to end, so
    ///   all windows together never run more than `bg::SEGMENT_SCOPE_WIDTH`;
    /// - slots and connections: a slot is a `ThrottleGuard` owned by the one
    ///   segment that uses it and dropped when that segment returns or gives it
    ///   back to resume; its client is the mount's fixed per-slot client, only
    ///   borrowed per request;
    /// - responses: owned by the segment's stack, dropped on return;
    /// - memory: the window's `BufferReservation` lives in its `StreamState`, and
    ///   `data` + `ahead` hold disjoint parts of one window, sized exactly;
    /// - waiters: `WindowDone` marks the window done on every exit.
    ///
    /// And every segment exits in bounded time: each body read is bounded by
    /// READ_STALL_TIMEOUT and must deliver MIN_BODY_PROGRESS per such period, each
    /// resume is bounded by RANGE_OPEN_BUDGET, resumes are counted, and a segment
    /// stops within SUPERSEDE_CHECK_EVERY (plus one read) once the window is
    /// superseded or another segment broke it.
    fn run(
        self,
        first: (reqwest::blocking::Response, ThrottleGuard<'a>),
        extras: Vec<(ThrottleGuard<'a>, bg::SegmentThread)>,
        plan: &[Segment],
    ) -> usize {
        let _done = WindowDone { pump: &self, target: plan.iter().map(|s| s.len).sum() };
        let already = self.shared.0.lock().unwrap().data.len() as u64;
        let t0 = Instant::now();
        let (resp, permit) = first;
        std::thread::scope(|scope| {
            let pump = &self;
            for (i, ((permit, token), seg)) in extras.into_iter().zip(plan.iter().skip(1).copied()).enumerate() {
                scope.spawn(move || {
                    let _token = token;
                    pump.segment(i + 1, seg, None, permit, 0)
                });
            }
            pump.segment(0, plan[0], Some(resp), permit, already);
        });
        let ss = self.shared.0.lock().unwrap();
        record_window_throughput((ss.received() as u64).saturating_sub(already), t0.elapsed(), plan.len());
        ss.data.len()
    }

    /// One body read. A read that times out (READ_STALL_TIMEOUT without a byte on
    /// this connection) is a stall; the window-wide trickle rule is applied by the
    /// caller.
    fn read_body(r: &mut reqwest::blocking::Response, buf: &mut [u8]) -> BodyRead {
        use std::io::Read;
        match r.read(buf) {
            Ok(0) => BodyRead::End,
            Ok(n) => BodyRead::Bytes(n),
            Err(e) => BodyRead::Failed { stalled: io_is_timeout(&e), why: e.to_string() },
        }
    }

    /// Fetches one segment into the window, resuming it on breaks and stalls. An
    /// extra segment (`resp` None) first opens its own request on `permit`.
    fn segment(
        &self,
        idx: usize,
        seg: Segment,
        resp: Option<reqwest::blocking::Response>,
        permit: ThrottleGuard<'a>,
        mut got: u64,
    ) {
        let (ref mtx, ref cv) = **self.shared;
        let mut slot = permit.slot();
        let mut _permit = Some(permit);
        let mut resp = match resp {
            Some(r) => Some(r),
            None => {
                let held = _permit.take().expect("set just above");
                let deadline = Instant::now() + RANGE_OPEN_BUDGET;
                match do_range_read_stream(self.conn, self.path, self.start + seg.at, seg.len as usize,
                    Slot::Held { permit: held, spare: self.spare }, deadline)
                {
                    // Only a 206 is the part asked for; a 200 would be the file from byte 0.
                    Ok((r, p)) if r.status() == reqwest::StatusCode::PARTIAL_CONTENT => {
                        slot = p.slot();
                        _permit = Some(p);
                        Some(r)
                    }
                    Ok((r, _)) => {
                        log::warn!("segment {} of {} got {} instead of 206", idx, self.path.display(), r.status());
                        None
                    }
                    Err(e) => {
                        log::warn!("segment {} of {} failed to open: {}", idx, self.path.display(), e);
                        // A busy daemon or a slow server is worth retrying; anything
                        // else is the server answering wrongly.
                        if is_retry_later_err(&e) {
                            self.stop_window();
                        } else {
                            self.break_window();
                        }
                        return;
                    }
                }
            }
        };
        if resp.is_none() {
            self.break_window();
            return;
        }
        let mut chunk = vec![0u8; 256 * 1024];
        let mut since_check = 0usize;
        let mut last_check = Instant::now();
        let mut retries = 0u32;
        let mut got_at_break = 0u64;
        let mut stalls = 0u32;
        while let Some(r) = resp.as_mut() {
            if got >= seg.len {
                break;
            }
            let want = ((seg.len - got) as usize).min(chunk.len());
            let outcome = match Self::read_body(r, &mut chunk[..want]) {
                BodyRead::Bytes(n) => match self.window_crawling() {
                    // Keep the bytes, then treat the crawl as a stall.
                    Some(why) => {
                        let mut ss = mtx.lock().unwrap();
                        ss.push(idx, (seg.at + got) as usize, &chunk[..n], (seg.len - got) as usize);
                        drop(ss);
                        got += n as u64;
                        cv.notify_all();
                        BodyRead::Failed { stalled: true, why }
                    }
                    None => BodyRead::Bytes(n),
                },
                other => other,
            };
            match outcome {
                BodyRead::End => {
                    // The end of the body. Legitimate only in the window's last
                    // segment (the file ends there); anywhere else it is a hole.
                    if got < seg.len && !seg.last {
                        log::warn!("segment {} of {} ended {} bytes early", idx, self.path.display(), seg.len - got);
                        self.break_window();
                    }
                    break;
                }
                BodyRead::Bytes(n) => {
                    let halted = {
                        let mut ss = mtx.lock().unwrap();
                        ss.push(idx, (seg.at + got) as usize, &chunk[..n], (seg.len - got) as usize);
                        ss.halted()
                    };
                    got += n as u64;
                    cv.notify_all();
                    if halted {
                        break;
                    }
                    if retries > 0 && got - got_at_break >= BODY_RETRY_REFILL_BYTES {
                        retries = 0;
                    }
                    since_check += n;
                    if since_check >= 2 * 1024 * 1024 || last_check.elapsed() >= SUPERSEDE_CHECK_EVERY {
                        since_check = 0;
                        last_check = Instant::now();
                        let received = mtx.lock().unwrap().received() as u64;
                        if let Ok(mut tm) = self.tmap.lock() {
                            if let Some(tp) = tm.get_mut(&self.tkey) {
                                tp.bytes_done = received;
                            }
                        }
                        if self.superseded() {
                            log::debug!("read-ahead stream for {} superseded, stopping early",
                                self.path.display());
                            self.stop_window();
                            break;
                        }
                    }
                }
                BodyRead::Failed { stalled, why } => {
                    if got >= seg.len {
                        break;
                    }
                    let remaining = seg.len - got;
                    let budget_left = if stalled { stalls < MAX_STALL_RESUMES } else { retries < MAX_BODY_RETRIES };
                    // Abort this response and hand its slot back *before* waiting for
                    // a new one. Holding it across that wait was the 0.1.77 hang: one
                    // QUIC connection died under three streams, each kept its slot
                    // while waiting (untimed) for another, and with every slot held by
                    // a waiter nothing was ever released again.
                    resp = None;
                    _permit = None;
                    if stalled {
                        // Most likely a silently dead QUIC connection: resume on
                        // another one (see Throttle::quarantine).
                        self.conn.read_throttle.quarantine(slot, H3_MAX_IDLE);
                    }
                    let halted = mtx.lock().unwrap().halted();
                    if halted {
                        break;
                    }
                    if self.superseded() {
                        self.stop_window();
                        break;
                    }
                    if !budget_left {
                        // Stops the window short of its target; readers parked in the
                        // hole never get a phantom EOF (see `wait_on_window`). Out of
                        // stall resumes, the server is slow, not wrong: EAGAIN.
                        log::warn!("read-ahead segment {} of {} gave up after {} of {} bytes (stalled={}): {}",
                            idx, self.path.display(), got, seg.len, stalled, why);
                        if stalled { self.stop_window() } else { self.break_window() }
                        break;
                    }
                    if stalled {
                        stalls += 1;
                        log::warn!(
                            "read-ahead segment {} of {} stalled on slot {} after {} of {} bytes ({}) — resuming on another connection (stall {}/{})",
                            idx, self.path.display(), slot, got, seg.len, why, stalls, MAX_STALL_RESUMES,
                        );
                    } else {
                        retries += 1;
                        got_at_break = got;
                        log::warn!(
                            "read-ahead segment {} of {} broke after {} of {} bytes (retry {}/{}): {}",
                            idx, self.path.display(), got, seg.len, retries, MAX_BODY_RETRIES, why,
                        );
                        thread::sleep(Duration::from_millis(300 * retries as u64));
                    }
                    let deadline = Instant::now() + RANGE_OPEN_BUDGET;
                    match do_range_read_stream(self.conn, self.path, self.start + seg.at + got, remaining as usize,
                        Slot::Take { spare: self.spare }, deadline)
                    {
                        // Only a 206 is the tail we asked for. A 200 is the whole file
                        // from byte 0, and appending it here would corrupt the window.
                        Ok((new_resp, new_permit)) if new_resp.status() == reqwest::StatusCode::PARTIAL_CONTENT => {
                            slot = new_permit.slot();
                            resp = Some(new_resp);
                            _permit = Some(new_permit);
                        }
                        Ok((new_resp, _)) => {
                            log::warn!("read-ahead resume for {} got {} instead of 206, giving up", self.path.display(), new_resp.status());
                            self.break_window();
                        }
                        Err(resume_err) => {
                            log::warn!("read-ahead resume for {} failed: {}", self.path.display(), resume_err);
                            if is_retry_later_err(&resume_err) {
                                self.stop_window();
                            } else {
                                self.break_window();
                            }
                        }
                    }
                }
            }
        }
        // The window's last segment reached the end of the file the server stated.
        // Recorded as a position, not a verdict: `at_eof` only honours it once the
        // prefix actually reaches it, since an earlier segment may have left a hole.
        if seg.last && self.total == Some(self.start + seg.at + got) {
            mtx.lock().unwrap().eof_len = Some((seg.at + got) as usize);
            cv.notify_all();
        }
    }
}

/// A window body handed off to the `stream` pool once its READ was answered, with
/// everything it needs owned so it can outlive the job that opened it.
struct PumpJob {
    conn: Arc<ConnInfo>,
    path: PathBuf,
    fh: u64,
    open_files: Arc<Mutex<HashMap<u64, OpenFile>>>,
    tmap: TransferMap,
    tkey: TransferKey,
    shared: Arc<(Mutex<StreamState>, Condvar)>,
    start: u64,
    spare: usize,
    total: Option<u64>,
    resp: reqwest::blocking::Response,
    permit: OwnedSlot,
    extras: Vec<(OwnedSlot, bg::SegmentThread)>,
    plan: Vec<Segment>,
    /// Runs after the body, with the bytes the window ended up holding.
    finish: Box<dyn FnOnce(usize) + Send>,
}

impl PumpJob {
    fn run(self) {
        let PumpJob { conn, path, fh, open_files, tmap, tkey, shared, start, spare, total, resp, permit, extras, plan, finish } = self;
        let permit = permit.bind(&conn.read_throttle);
        let extras = extras.into_iter().map(|(s, t)| (s.bind(&conn.read_throttle), t)).collect();
        let n = WindowPump {
            conn: &conn,
            path: &path,
            fh,
            open_files: &open_files,
            tmap: &tmap,
            tkey,
            shared: &shared,
            start,
            spare,
            total,
        }
        .run((resp, permit), extras, &plan);
        finish(n);
    }

    /// Runs the body on the `stream` pool, or right here when that pool is full —
    /// either way it runs, and either way the READ it came from is already answered.
    fn spawn(self) {
        if let Err((_, job)) = bg::STREAM.submit_owning(self, PumpJob::run) {
            job.run();
        }
    }
}

/// How long a look-ahead may spend opening its request. Short: it is only worth
/// anything if it lands before the reader reaches the boundary.
const LOOKAHEAD_OPEN_BUDGET: Duration = Duration::from_secs(10);
/// `read_throttle` slots a look-ahead leaves free for foreground reads, so it can
/// never be what a READ on another handle is waiting behind.
const LOOKAHEAD_SPARE_SLOTS: usize = 1;

/// Clears a handle's `lookahead_inflight` when the look-ahead job ends, however
/// it ends.
struct LookaheadInflight {
    open_files: Arc<Mutex<HashMap<u64, OpenFile>>>,
    fh: u64,
}

impl Drop for LookaheadInflight {
    fn drop(&mut self) {
        self.open_files.safe_lock().entry(self.fh).and_modify(|of| of.lookahead_inflight = false);
    }
}

/// Body of a look-ahead job (on the `stream` pool): open the next window's range
/// on the slot taken at submit, install it as the handle's `next_buf` if the reader
/// is still where it was, then pump it.
///
/// Every step can only give up, never block for long: the slot is already held,
/// opening is bounded by LOOKAHEAD_OPEN_BUDGET and the body by the pump's stall
/// rules. Nothing here fails a read — a look-ahead that never lands just leaves
/// the boundary to the ordinary network fetch.
#[allow(clippy::too_many_arguments)]
fn run_lookahead(
    conn: &ConnInfo,
    open_files: &Mutex<HashMap<u64, OpenFile>>,
    tmap: &TransferMap,
    path: &Path,
    fh: u64,
    file_size: u64,
    plan: LookaheadPlan,
    slot: OwnedSlot,
) {
    let LookaheadPlan { after, start, len, budget } = plan;
    let primary = slot.bind(&conn.read_throttle);
    if conn.is_offline.load(Ordering::Relaxed) {
        return;
    }
    let req = WindowRequest {
        start,
        target: len,
        need: 0,
        file_size,
        spare: LOOKAHEAD_SPARE_SLOTS,
        deadline: Instant::now() + LOOKAHEAD_OPEN_BUDGET,
        primary: Some(primary),
        reserved: Some(budget),
    };
    let opened = match open_window(conn, path, req) {
        // Only a 206 is the window asked for; a 200 would be the file from byte 0.
        Ok(w) if w.resp.status() == reqwest::StatusCode::PARTIAL_CONTENT => w,
        Ok(w) => {
            log::debug!("look-ahead {} at {}: got {}, not 206 — skipped", path.display(), start, w.resp.status());
            return;
        }
        Err(e) => {
            log::debug!("look-ahead {} at {} skipped: {}", path.display(), start, e);
            return;
        }
    };
    let OpenedWindow { resp, permit, extras, plan: segments, target, total, budget } = opened;
    let shared = Arc::new((
        Mutex::new(StreamState::new(Vec::new(), &segments, budget)),
        Condvar::new(),
    ));
    let installed = {
        let mut ofs = open_files.safe_lock();
        match ofs.get_mut(&fh) {
            Some(of) if of.next_buf.is_none()
                && of.buf.as_ref().is_some_and(|b| Arc::ptr_eq(&b.stream, &after)) =>
            {
                of.next_buf = Some(ReadAheadBuf {
                    start,
                    stream: Arc::clone(&shared),
                    target_len: target,
                });
                true
            }
            // Released, or the reader seeked while this was connecting.
            _ => false,
        }
    };
    if !installed {
        log::debug!("look-ahead {} at {} no longer wanted", path.display(), start);
        return;
    }
    let tkey: TransferKey = (path.to_path_buf(), next_stream_id());
    tmap.safe_lock().insert(tkey.clone(), TransferProgress {
        path: path.to_path_buf(),
        direction: TransferDirection::Download,
        bytes_done: 0,
        total_bytes: target,
    });
    let got = WindowPump {
        conn,
        path,
        fh,
        open_files,
        tmap,
        tkey,
        shared: &shared,
        start,
        spare: LOOKAHEAD_SPARE_SLOTS,
        total,
    }
    .run((resp, permit), extras, &segments);
    log::debug!("look-ahead {} at {}: {} of {} bytes", path.display(), start, got, target);
}

/// Reads up to `need` bytes, stopping early only at the end of the body.
///
/// Each `read` is bounded by the read client's READ_STALL_TIMEOUT; `deadline`
/// bounds the sum, so a body trickling in just fast enough to dodge the stall
/// timeout still cannot hold the READ that is waiting on these bytes.
fn read_exact_from_stream(resp: &mut reqwest::blocking::Response, need: usize, deadline: Instant) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut buf = vec![0u8; need];
    let mut filled = 0;
    while filled < need {
        if Instant::now() >= deadline {
            // Worded to match neither network-down classifier: a slow body is not
            // an unreachable server and must not flip the mount offline.
            return Err(format!("{}: {} of {} bytes within {:?}", READ_BODY_SLOW_PREFIX, filled, need, FIRST_BYTES_DEADLINE));
        }
        match resp.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            // A stall is a slow server, not a dead network: keep it off the
            // network-down classifiers (see READ_BODY_SLOW_PREFIX).
            Err(e) if io_is_timeout(&e) => {
                return Err(format!("{}: body stalled {:?} after {} of {} bytes", READ_BODY_SLOW_PREFIX, READ_STALL_TIMEOUT, filled, need));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

/// Remove `dir` if it holds nothing but (recursively) empty directories. A
/// directory that still has a file in it, anywhere below, is left alone.
fn remove_empty_tree(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                remove_empty_tree(&entry.path());
            }
        }
        let _ = std::fs::remove_dir(dir);
    }
}

/// Remove `start` and its ancestors while they are empty, stopping below `stop`,
/// which is never removed.
fn remove_empty_parents(start: &Path, stop: &Path) {
    let mut cur = start;
    while cur != stop && cur.starts_with(stop) {
        if std::fs::remove_dir(cur).is_err() {
            break;
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => break,
        }
    }
}

/// Whether a body read failed because a timeout ran out, judged from the error
/// itself rather than its text: reqwest wraps its `TimedOut` in an `io::Error` of
/// kind `Other`, so neither the kind nor a string match is reliable on its own.
fn io_is_timeout(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::TimedOut
        || e.get_ref()
            .and_then(|inner| inner.downcast_ref::<reqwest::Error>())
            .is_some_and(|re| re.is_timeout())
}


// ── Mount ─────────────────────────────────────────────────────────────────────

fn is_trash_dir(name: &OsStr) -> bool {
    let s = name.to_string_lossy();
    s.starts_with(".Trash")
}

// How far a reconnect follows changed branches down before handing the rest to
// background re-fetches. Each level costs one PROPFIND per *changed* directory, so
// an unchanged tree costs nothing and a busy one stays bounded.
const RECONNECT_VALIDATE_DEPTH: u32 = 3;

/// Validate a cached subtree against the server by etag. One Depth-1 PROPFIND of
/// `path` carries the etag of every child directory, so all of them are checked in
/// a single request; a matching etag proves that child unchanged, because Nextcloud
/// propagates collection etags up the tree. Only branches that actually differ are
/// descended into (while `depth` allows) or handed to a background re-fetch — the
/// alternative, dropping the cache wholesale, would make the next readdir of every
/// directory pay a full listing.
fn validate_subtree(conn: &Arc<ConnInfo>, cache: &Arc<Mutex<FsCache>>, path: PathBuf, depth: u32) -> bool {
    if conn.shutdown.load(Ordering::Relaxed) || conn.is_offline.load(Ordering::Relaxed) {
        return false;
    }
    let has_cache = cache.safe_lock().dir_cache.contains_key(&path);
    if !has_cache {
        return false;
    }
    match list_dir_propfind(conn, path.clone()) {
        Ok((etag, self_entry, fresh_files)) => {
            let mut changed_paths: Vec<PathBuf> = Vec::new();
            let mut matched = 0usize;
            {
                let mut c = cache.safe_lock();
                for entry in &fresh_files {
                    if !entry.is_dir {
                        continue;
                    }
                    let child_path = entry.path.clone();
                    // Nothing cached for this child, so there is nothing to validate:
                    // its first readdir will list it anyway.
                    if !c.dir_cache.contains_key(&child_path) {
                        continue;
                    }
                    let fresh_etag = entry.change_token.as_deref();
                    let cached_etag = c.dir_cache.get(&child_path).and_then(|e| e.etag.clone());
                    match (fresh_etag, cached_etag.as_deref()) {
                        (Some(f), Some(c_etag)) if f == c_etag => {
                            matched += 1;
                            // Etag unchanged: the cached listing is current. Restart the
                            // max-stale window so the next readdir is served from cache
                            // instead of blocking on a re-list of a directory that was
                            // just proved up to date.
                            c.confirm_dir_fresh(&child_path);
                        }
                        _ => changed_paths.push(child_path),
                    }
                }
                // Entries handed to start_background_propfind must leave dir_cache
                // first — it early-returns for a path that is still cached. Entries
                // we recurse into keep their listing but are marked untrusted, so a
                // descent that fails (or never finishes) cannot leave a directory we
                // know has changed being served from cache.
                if depth <= 1 {
                    for p in &changed_paths {
                        c.dir_cache.remove(p);
                    }
                } else {
                    for p in &changed_paths {
                        c.invalidate_dir(p);
                    }
                }
                c.put_dir_cache(path.clone(), etag, self_entry, fresh_files);
            }
            log::info!("VALIDATE {}: {} dirs unchanged, {} changed (depth {})",
                path.display(), matched, changed_paths.len(), depth);
            schedule_save_dir_cache(cache);
            for p in changed_paths {
                if depth > 1 {
                    if !validate_subtree(conn, cache, p.clone(), depth - 1) {
                        // The descent could not confirm this branch. It stays
                        // invalidated either way — nothing stale is served — and when
                        // we are still running a background re-list refills it without
                        // waiting for a readdir. During shutdown or offline, leaving it
                        // invalidated is enough: the next readdir re-lists.
                        if conn.shutdown.load(Ordering::Relaxed) || conn.is_offline.load(Ordering::Relaxed) {
                            continue;
                        }
                        cache.safe_lock().dir_cache.remove(&p);
                        start_background_propfind(conn, cache, p, 0);
                    }
                } else {
                    start_background_propfind(conn, cache, p, 0);
                }
            }
            true
        }
        Err(e) => {
            log::warn!("VALIDATE {} failed: {} — cache served as-is", path.display(), e);
            false
        }
    }
}

fn boot_validate_root(conn: &Arc<ConnInfo>, cache: &Arc<Mutex<FsCache>>) {
    validate_subtree(conn, cache, PathBuf::from("/"), 1);
}

/// Close the gap a dropped push connection leaves behind: notify-push has no
/// replay, so anything that happened while the socket was down is simply gone.
///
/// This deliberately does not take the tempting shortcut of probing root's etag and
/// declaring the whole tree current on a match. Root's *cached* etag is refreshed
/// whenever root is listed, which can happen long after a child was last listed —
/// so a matching root etag says nothing about whether that child's cached listing
/// is still good. The per-child comparison in `validate_subtree` is sound precisely
/// because each child's etag was recorded together with its listing, and it costs
/// the same single request (Depth-1 instead of Depth-0) for the whole level.
fn revalidate_after_reconnect(conn: &Arc<ConnInfo>, cache: &Arc<Mutex<FsCache>>) {
    log::info!("RECONNECT_VALIDATE: revalidating cached listings by etag");
    validate_subtree(conn, cache, PathBuf::from("/"), RECONNECT_VALIDATE_DEPTH);
}

fn build_fuse_options() -> Vec<MountOption> {
    vec![
        MountOption::FSName("ncrs".to_string()),
        MountOption::DefaultPermissions,
    ]
}

/// `offline` and `hpb_connected` are out-parameters for a caller that renders
/// status (the GUI): `offline` is *shared* with the filesystem, so it tracks the
/// connectivity monitor live, while `hpb_connected` is mirrored by the watcher
/// poll. A caller needs both to tell a total outage (server unreachable) from a
/// partial one (notify_push down, WebDAV fine).
pub fn mount_ncfs(options: MountOptions, error_log: Option<ErrorLog>, transfer_map: Option<TransferMap>, journal: Option<mutation_journal::SharedJournal>, paused: Option<Arc<AtomicBool>>, hpb_connected: Option<Arc<AtomicBool>>, offline: Option<OfflineStatus>) -> Result<(), String> {
    // Must run before any other thread is spawned (see seccomp_harden::install).
    seccomp_harden::install();

    // Must run before anything that touches shared resources (the IPC socket,
    // cache dirs, journal): a refused second instance must leave the running
    // daemon's state untouched.
    let cache_dir = ncrs_cache_dir(&options.url);
    let adopted = prepare_mount_point(&options.mount_point, &cache_dir)?;

    let mut filesystem = NextCloudFs::new(options.clone())?;
    if let Some(el) = error_log {
        filesystem.error_log = el;
    }
    if let Some(tm) = transfer_map {
        filesystem.transfer_map = tm;
    }
    if let Some(j) = journal {
        filesystem.journal = j;
    }
    // One shared pause flag for the FUSE connection, background workers, and
    // the IPC PAUSE/RESUME verbs — callers without their own flag get one.
    let paused_flag = paused.unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
    Arc::get_mut(&mut filesystem.conn).expect("conn not yet shared").paused = paused_flag.clone();
    // Same for the offline flag and its transition timestamp: hand the caller the
    // *same* pair the connectivity monitor flips, so its view of reachability can
    // never lag behind the daemon's. Both must be installed together — `settled`
    // reads the timestamp to tell a blip from an outage.
    let offline_status = offline.unwrap_or_default();
    offline_status.is_offline.store(options.offline, Ordering::Relaxed);
    *offline_status.since.safe_lock() = None;
    {
        let conn = Arc::get_mut(&mut filesystem.conn).expect("conn not yet shared");
        conn.is_offline = offline_status.is_offline.clone();
        conn.offline_since = offline_status.since.clone();
    }

    // Files that were written straight onto the (unmounted) real mount-point
    // directory during a previous session — see prepare_mount_point — are
    // queued into the journal now that it (and the backend connection) exist.
    adopt_orphaned_files(&filesystem, adopted);

    let keep_cb = filesystem.keep_callback();
    let evict_cb = filesystem.evict_callback();
    let prefetch_cb = filesystem.prefetch_callback();
    let thumbnail_cb = filesystem.thumbnail_callback();
    let purge_cb = filesystem.purge_callback();
    let base_url = notifications::base_url(&options.url);
    let ipc_creds = options.credentials()?;
    let file_change_queue = filesystem.file_change_queue();
    let storage_stats: ipc::SharedStorageStats = Arc::new(Mutex::new(ipc::StorageStats::default()));
    let offline_flag = filesystem.is_offline_flag();
    // Desktop / file-browser profiles: resolve which browsers are installed
    // (or explicitly toggled) and apply their components.
    let desktop_manager = Arc::new(desktop::Manager::for_service(options.mount_point.clone()));
    desktop_manager.spawn_refresher();
    ipc::start_server(options.mount_point.clone(), filesystem.status_map(), filesystem.shared_set(), filesystem.fileid_map(), filesystem.detail_map(), filesystem.children_map(), filesystem.dirty_set(), ipc_creds, base_url, Some(keep_cb), Some(evict_cb), Some(prefetch_cb), Some(thumbnail_cb), Some(purge_cb), filesystem.error_log(), filesystem.transfer_map(), filesystem.journal(), file_change_queue.clone(), storage_stats.clone(), paused_flag.clone(), offline_status.clone(), filesystem.passthrough_enabled_flag(), filesystem.passthrough_capable_flag(), Some(desktop_manager.clone()));

    let backend = filesystem.conn.backend.clone();
    let notifier_slot = filesystem.notifier_slot();
    let wipe_flag = Arc::new(AtomicBool::new(false));
    // Serializes journal replays so the startup replay and the connectivity
    // monitor's periodic retry never run concurrently over the same queue.
    let replay_active = Arc::new(AtomicBool::new(false));

    if !options.offline {
        // Replay any journal entries from a previous session
        let replay_journal_ref = filesystem.journal();
        if !replay_journal_ref.safe_lock().is_empty() {
            let j = replay_journal_ref.clone();
            let b = backend.clone();
            let c = filesystem.cache_ref();
            let d = filesystem.dirty_set();
            let el = filesystem.error_log();
            let smap = filesystem.status_map();
            let active = replay_active.clone();
            if !active.swap(true, Ordering::Relaxed) {
                // Released when the job ends — or is dropped unrun by a full pool.
                let running = ReleaseOnDrop(active);
                let _ = bg::HOUSEKEEPING.submit(move || {
                    let _running = running;
                    let ctx = mutation_journal::ReplayContext { backend: b, status: smap };
                    mutation_journal::replay_journal(&j, &ctx, &c, &d, &el);
                });
            }
        }

        // Connectivity monitor
        {
            let backend_monitor = backend.clone();
            let offline = offline_flag.clone();
            let offline_since_monitor = filesystem.offline_since_slot();
            let journal_for_monitor = filesystem.journal();
            let cache_for_monitor = filesystem.cache_ref();
            let dirty_for_monitor = filesystem.dirty_set();
            let elog_for_monitor = filesystem.error_log();
            let status_for_monitor = filesystem.status_map();
            let replay_active_monitor = replay_active.clone();
            let shutdown_monitor = filesystem.shutdown_flag();
            let paused_monitor = filesystem.paused_flag();
            let wipe_flag_monitor = wipe_flag.clone();
            let conn_monitor = filesystem.conn.clone();
            let cache_dir_monitor = filesystem.cache_ref().safe_lock().cache_dir.clone();
            start_service("connectivity", move || {
                loop {
                    if shutdown_monitor.load(Ordering::Relaxed) {
                        log::info!("CONNECTIVITY monitor: shutdown, exiting");
                        break;
                    }
                    let currently_offline = offline.load(Ordering::Relaxed);
                    let interval = if currently_offline { Duration::from_secs(5) } else { Duration::from_secs(30) };
                    let mut slept = Duration::ZERO;
                    while slept < interval {
                        if shutdown_monitor.load(Ordering::Relaxed) { break; }
                        // A failed read flips the flag eagerly, between probes. Cut the
                        // online-cadence sleep short so the 5s offline cadence starts when
                        // the daemon *became* offline, not when this loop would next look:
                        // snoozing out the rest of a 30s interval eats the whole
                        // OFFLINE_READ_GRACE window, and every read and uncached listing
                        // waiting out the blip then times out before the first re-probe.
                        if !currently_offline && offline.load(Ordering::Relaxed) { break; }
                        // A read saw the server slow down (see `request_probe`).
                        if conn_monitor.probe_soon.swap(false, Ordering::Relaxed) { break; }
                        thread::sleep(Duration::from_secs(1));
                        slept += Duration::from_secs(1);
                    }
                    if shutdown_monitor.load(Ordering::Relaxed) {
                        log::info!("CONNECTIVITY monitor: shutdown, exiting");
                        break;
                    }
                    if paused_monitor.load(Ordering::Relaxed) { continue; }

                    match backend_monitor.check_reachability(Duration::from_secs(5)) {
                        backend::ReachabilityStatus::Reachable => {
                            let was_offline = mark_online(&offline, &offline_since_monitor);
                            // Replay whenever there is queued work — both right after
                            // connectivity is restored AND periodically while online, so a
                            // PendingSync entry from a failed upload is retried on its own
                            // without needing an offline→online transition or a restart.
                            let has_pending = !journal_for_monitor.safe_lock().is_empty();
                            if (was_offline || has_pending)
                                && !replay_active_monitor.swap(true, Ordering::Relaxed)
                            {
                                if was_offline {
                                    log::info!("CONNECTIVITY restored — replaying mutation journal");
                                } else {
                                    log::info!("CONNECTIVITY: retrying pending mutations");
                                }
                                let j = journal_for_monitor.clone();
                                let b = backend_monitor.clone();
                                let c = cache_for_monitor.clone();
                                let d = dirty_for_monitor.clone();
                                let el = elog_for_monitor.clone();
                                let smap = status_for_monitor.clone();
                                let running = ReleaseOnDrop(replay_active_monitor.clone());
                                let _ = bg::HOUSEKEEPING.submit(move || {
                                    let _running = running;
                                    let ctx = mutation_journal::ReplayContext { backend: b, status: smap };
                                    mutation_journal::replay_journal(&j, &ctx, &c, &d, &el);
                                });
                            }
                        }
                        backend::ReachabilityStatus::AuthRejected(code) => {
                            log::warn!("CONNECTIVITY: auth rejected (HTTP {}), checking for remote wipe", code);
                            match remote_wipe::check_wipe(&conn_monitor.clients.get(), &conn_monitor.base_url, conn_monitor.creds.secret()) {
                                Ok(true) => {
                                    log::warn!("REMOTE WIPE requested by server — executing");
                                    let config_path = config::config_path();
                                    if let Err(e) = remote_wipe::execute_wipe(&cache_dir_monitor, &config_path) {
                                        log::error!("REMOTE_WIPE execution error: {}", e);
                                    }
                                    if let Err(e) = remote_wipe::confirm_wipe(&conn_monitor.clients.get(), &conn_monitor.base_url, conn_monitor.creds.secret()) {
                                        log::warn!("REMOTE_WIPE: failed to confirm to server: {}", e);
                                    }
                                    wipe_flag_monitor.store(true, Ordering::Relaxed);
                                    shutdown_monitor.store(true, Ordering::Relaxed);
                                    break;
                                }
                                Ok(false) => {
                                    log::info!("CONNECTIVITY: auth rejected but no wipe pending — token may be revoked");
                                    mark_offline(&offline, &offline_since_monitor);
                                }
                                Err(e) => {
                                    log::warn!("CONNECTIVITY: wipe check failed: {} — will retry", e);
                                    mark_offline(&offline, &offline_since_monitor);
                                }
                            }
                        }
                        backend::ReachabilityStatus::Unreachable => {
                            let was_offline = offline.load(Ordering::Relaxed);
                            mark_offline(&offline, &offline_since_monitor);
                            if !was_offline {
                                log::warn!("CONNECTIVITY lost — serving from cache");
                            }
                        }
                    }
                }
            });
        }

        // Change watcher (replaces notify_push::start)
        {
            let watcher_backend = backend.clone();
            let watcher_cache = filesystem.cache_ref();
            let watcher_dirty = filesystem.dirty_set();
            let watcher_active = filesystem.active_streams();
            let watcher_deferred = filesystem.deferred_invalidation();
            let watcher_throttle = filesystem.throttle();
            let watcher_notifier = notifier_slot.clone();
            let watcher_ghosts = filesystem.ghost_entries();
            let watcher_fcq = file_change_queue.clone();
            let watcher_paused = filesystem.paused_flag();
            let watcher_offline = offline_flag.clone();
            let debounce = filesystem.refresh_debounce();

            let watcher_backoff = filesystem.conn.backoff.clone();
            let watcher = backend.start_change_watcher(Box::new(move |event| {
                if watcher_paused.load(Ordering::Relaxed) { return; }
                if watcher_offline.load(Ordering::Relaxed) { return; }
                // The server just told us these changed: a cooldown from an
                // earlier failure no longer says anything about them.
                if event.invalidate_all {
                    watcher_backoff.clear_all();
                } else {
                    for dir in &event.invalidated_dirs {
                        watcher_backoff.clear(dir);
                    }
                }
                notify_push::handle_change_event(
                    event,
                    &watcher_backend,
                    &watcher_cache,
                    &watcher_dirty,
                    &watcher_active,
                    &watcher_deferred,
                    &watcher_throttle,
                    &watcher_notifier,
                    &debounce,
                    &watcher_ghosts,
                    &watcher_fcq,
                );
            }));

            log::info!("notify_push: starting change watcher");
            // Sync watcher connection status and pause state
            let np_connected = filesystem.notify_push_connected_flag();
            let sync_offline = offline_flag.clone();
            let sync_paused = filesystem.paused_flag();
            let watcher_shutdown = filesystem.shutdown_flag();
            let revalidate_conn = filesystem.conn();
            let revalidate_cache = filesystem.cache_ref();
            let revalidating = Arc::new(AtomicBool::new(false));
            start_service("push-watch", move || {
                let mut was_connected = false;
                // Reconnects are detected by generation, not by observing the flag go
                // false: a drop and re-auth can complete inside one poll interval and
                // would otherwise be invisible. Generation 1 is the initial connect,
                // already covered by boot validation; anything beyond it left a gap.
                let mut last_generation = 0u64;
                while !watcher_shutdown.load(Ordering::Relaxed) {
                    let connected = watcher.is_connected();
                    np_connected.store(connected, Ordering::Relaxed);
                    if let Some(ref ext) = hpb_connected {
                        ext.store(connected, Ordering::Relaxed);
                    }
                    if connected != was_connected {
                        if connected {
                            log::info!("notify_push: high-performance backend connected");
                        } else {
                            log::warn!("notify_push: high-performance backend disconnected — directory listings revalidate on read");
                        }
                        was_connected = connected;
                    }
                    let generation = watcher.connect_generation();
                    if generation != last_generation {
                        // Events that fired while the socket was down are gone for good,
                        // so revalidate by etag before trusting the cache again. Runs off
                        // the poll thread: the descent can take a while.
                        if generation > 1 && !revalidating.swap(true, Ordering::Relaxed) {
                            let rc = revalidate_conn.clone();
                            let rcache = revalidate_cache.clone();
                            let guard = ReleaseOnDrop(revalidating.clone());
                            let _ = bg::HOUSEKEEPING.submit(move || {
                                // Held to the end of the closure, and released even if
                                // the revalidation panics — a latched guard would
                                // silently disable every later reconnect.
                                let _release = guard;
                                revalidate_after_reconnect(&rc, &rcache);
                            });
                        }
                        last_generation = generation;
                    }
                    watcher.set_paused(
                        sync_offline.load(Ordering::Relaxed)
                            || sync_paused.load(Ordering::Relaxed),
                    );
                    thread::sleep(Duration::from_secs(2));
                }
                drop(watcher);
            });
        }
    }

    // Validate root-level dirs at boot via a single PROPFIND /.
    // Compare child etags against cached dir_cache entries: matching
    // etags prove the subdirectory hasn't changed, so we keep serving
    // cached data. Mismatches get invalidated for re-fetch on next readdir.
    if !options.offline {
        let boot_conn = filesystem.conn.clone();
        let boot_cache = filesystem.cache_ref();
        let boot_shutdown = filesystem.shutdown_flag();
        start_service("boot-validate", move || {
            if !boot_shutdown.load(Ordering::Relaxed) {
                boot_validate_root(&boot_conn, &boot_cache);
            }
        });
    }

    // Validate cached files on boot — re-download if etag changed
    if !options.offline {
        let saved_etags = load_file_cache(&filesystem.cache_ref());
        if !saved_etags.is_empty() {
            let boot_backend = backend.clone();
            let boot_conn = filesystem.conn();
            let cache = filesystem.cache_ref();
            let status = filesystem.status_map();
            let dirty = filesystem.dirty_set();
            let boot_transfers = filesystem.transfer_map();
            let file_shutdown = filesystem.shutdown_flag();
            start_service("boot-files", move || {
                let total = saved_etags.len();
                let conn_throttle_width = boot_conn.throttle.max.min(bg::BOOT_SCOPE_WIDTH);
                log::info!("FILE_CACHE boot validation: checking {} files in parallel", total);
                let stale = Arc::new(AtomicUsize::new(0));
                let jobs: Vec<Box<dyn FnOnce() + Send>> = saved_etags.into_iter().map(|(remote_path, entry)| {
                    let conn = boot_conn.clone();
                    let cache = cache.clone();
                    let status = status.clone();
                    let dirty = dirty.clone();
                    let transfers = boot_transfers.clone();
                    let shutdown = file_shutdown.clone();
                    let backend = boot_backend.clone();
                    let stale = stale.clone();
                    Box::new(move || {
                        if shutdown.load(Ordering::Relaxed) { return; }
                        let _permit = conn.throttle.acquire();
                        if shutdown.load(Ordering::Relaxed) { return; }
                        match backend.dir_change_token(&remote_path, PROPFIND_TIMEOUT) {
                            Ok(Some(ref new_etag)) if new_etag == &entry.etag => {}
                            Ok(new_etag) => {
                                log::info!("FILE_CACHE stale: {} (etag {:?} → {:?})", remote_path.display(), entry.etag, new_etag);
                                cache.safe_lock().file_cache.remove(&remote_path);
                                match ensure_file_cached(&conn, &cache, &status, &dirty, remote_path.clone(), Some(&transfers), entry.kept) {
                                    Ok(_) => log::info!("FILE_CACHE re-downloaded {}", remote_path.display()),
                                    Err(e) => log::warn!("FILE_CACHE re-download {} failed: {}", remote_path.display(), e),
                                }
                                stale.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => {
                                log::debug!("FILE_CACHE etag check {} failed: {}", remote_path.display(), e);
                            }
                        }
                    }) as Box<dyn FnOnce() + Send>
                }).collect();
                // One thread per cached file used to start all at once; now at most
                // as many as there are request slots, which is all that could run anyway.
                bg::run_chunked(jobs, conn_throttle_width);
                log::info!("FILE_CACHE boot validation done: {}/{} stale", stale.load(Ordering::Relaxed), total);
            });
        }
    }

    // Auto-keep configured paths
    if !options.keep_paths.is_empty() {
        let keep_conn = filesystem.conn();
        let keep_cache = filesystem.cache_ref();
        let keep_status = filesystem.status_map();
        let keep_dirty = filesystem.dirty_set();
        let keep_transfers = filesystem.transfer_map();
        let keep_shutdown = filesystem.shutdown_flag();
        let paths: Vec<PathBuf> = options.keep_paths.iter().map(|s| {
            let s = s.trim();
            if s.starts_with('/') { PathBuf::from(s) } else { PathBuf::from(format!("/{}", s)) }
        }).collect();
        log::info!("AUTO_KEEP: {} configured paths", paths.len());
        start_service("auto-keep", move || {
            for p in paths {
                if keep_shutdown.load(Ordering::Relaxed) { break; }
                log::info!("AUTO_KEEP: keeping {}", p.display());
                keep_locally_recursive(&keep_conn, &keep_cache, &keep_status, &keep_dirty, p.clone(), Some(&keep_transfers));
            }
            log::info!("AUTO_KEEP: done");
        });
    }

    {
        let health_shutdown = filesystem.shutdown_flag();
        start_service("health-log", move || health_log_loop(health_shutdown));
    }

    // Storage stats update thread
    {
        let stats_cache = filesystem.cache_ref();
        let stats_backend = backend.clone();
        let stats_shutdown = filesystem.shutdown_flag();
        let stats_store = storage_stats;
        start_service("storage-stats", move || {
            loop {
                let (kept, cached) = stats_cache.safe_lock().storage_totals();
                let (remote_used, remote_total) = stats_backend
                    .quota(Duration::from_secs(10))
                    .unwrap_or((0, 0));
                {
                    let mut s = stats_store.safe_lock();
                    s.kept_bytes = kept;
                    s.cached_bytes = cached;
                    s.remote_used = remote_used;
                    s.remote_total = remote_total;
                }
                let mut slept = Duration::ZERO;
                let interval = Duration::from_secs(60);
                while slept < interval {
                    if stats_shutdown.load(Ordering::Relaxed) { return; }
                    thread::sleep(Duration::from_secs(5));
                    slept += Duration::from_secs(5);
                }
            }
        });
    }

    // Cache cleanup thread
    if options.cache_max_size_bytes > 0 || options.cache_auto_purge_days > 0 {
        let cleanup_cache = filesystem.cache_ref();
        let cleanup_status = filesystem.status_map();
        let cleanup_dirty = filesystem.dirty_set();
        let cleanup_shutdown = filesystem.shutdown_flag();
        let cleanup_paused = filesystem.paused_flag();
        let max_bytes = options.cache_max_size_bytes;
        let purge_days = options.cache_auto_purge_days;
        let cleanup_interval = Duration::from_secs(options.cache_cleanup_interval_secs);
        start_service("cache-cleanup", move || {
            log::info!("CACHE_CLEANUP thread started (max={}GB, purge={}d, interval={}s)",
                max_bytes as f64 / (1024.0 * 1024.0 * 1024.0), purge_days, cleanup_interval.as_secs());
            run_cache_cleanup(&cleanup_cache, &cleanup_status, &cleanup_dirty, max_bytes, purge_days);
            loop {
                let mut slept = Duration::ZERO;
                while slept < cleanup_interval {
                    if cleanup_shutdown.load(Ordering::Relaxed) { return; }
                    thread::sleep(Duration::from_secs(10));
                    slept += Duration::from_secs(10);
                }
                if cleanup_shutdown.load(Ordering::Relaxed) { return; }
                if cleanup_paused.load(Ordering::Relaxed) { continue; }
                run_cache_cleanup(&cleanup_cache, &cleanup_status, &cleanup_dirty, max_bytes, purge_days);
            }
        });
    }

    let fuse_options = build_fuse_options();

    log::info!(
        "Mounting WebDAV {} at {}",
        options.url,
        options.mount_point.display()
    );

    let shutdown_flag = filesystem.shutdown_flag();

    let fuse_config = {
        let mut c = Config::default();
        c.mount_options = fuse_options;
        c
    };
    let session = fuser::Session::new(filesystem, &options.mount_point, &fuse_config)
        .map_err(|e| format!("FUSE session init failed: {}", e))?;

    // The kernel accepted the mount — record that ncrs now owns this exact
    // path so a later restart on it (rather than a fresh setup elsewhere) is
    // allowed to adopt any leftovers instead of refusing. See prepare_mount_point.
    write_mount_marker(&cache_dir, &options.mount_point);

    *notifier_slot.safe_lock() = Some(session.notifier());
    log::info!("FUSE notifier ready");

    let bg = session.spawn().map_err(|e| format!("FUSE session spawn failed: {}", e))?;

    let result = bg.guard.join().map_err(|panic_payload| {
        let msg = panic_payload
            .downcast_ref::<&str>().map(|s| s.to_string())
            .or_else(|| panic_payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| format!("{:?}", panic_payload));
        format!("FUSE session thread panicked: {}", msg)
    })
    .and_then(|r| r.map_err(|e| format!("FUSE session failed: {}", e)));

    shutdown_flag.store(true, Ordering::Relaxed);
    log::info!("FUSE session ended — shutdown signal sent to background threads");

    // Remove the IPC socket so a subsequent remount doesn't mistake the
    // still-running server thread for an external daemon and enter attach mode.
    let _ = std::fs::remove_file(crate::ipc::socket_path());

    // The session has unmounted; remove the now-empty mount dir so an
    // unmounted state can't be mistaken for an empty share. remove_dir
    // refuses non-empty or still-mounted dirs, so this is safe best-effort.
    let _ = std::fs::remove_dir(&options.mount_point);

    if wipe_flag.load(Ordering::Relaxed) {
        return Err("REMOTE_WIPE".to_string());
    }

    result
}

// Classify and prepare the mount point:
//  - stat fails with ENOTCONN: stale FUSE mount left by a dead process — detach it
//  - listed as a live fuse mount in /proc/self/mounts: another instance owns it — refuse,
//    detaching here would steal the mount out from under that instance
//  - otherwise: plain (or missing) directory — create it and require it empty, unless
//    ncrs previously owned this exact path (see the mount marker below), in which case
//    leftovers are adopted as orphaned local edits instead — see adopt_orphaned_files.
fn prepare_mount_point(mount_point: &Path, cache_dir: &Path) -> Result<Vec<AdoptedFile>, String> {
    let mp_str = mount_point.to_string_lossy().to_string();

    match std::fs::metadata(mount_point) {
        Err(e) if e.raw_os_error() == Some(libc::ENOTCONN) => {
            log::info!("Detaching stale FUSE mount at {}", mp_str);
            let _ = std::process::Command::new("fusermount")
                .args(["-uz", &mp_str])
                .output();
        }
        _ => {
            if is_live_fuse_mount(mount_point) {
                return Err(format!(
                    "{} is already mounted — is another ncrs instance (GUI or systemd service) running?",
                    mp_str
                ));
            }
        }
    }

    if let Err(e) = std::fs::create_dir_all(mount_point) {
        return Err(format!("failed to create mount point {}: {}", mp_str, e));
    }
    let is_empty = match std::fs::read_dir(mount_point) {
        Ok(mut entries) => entries.next().is_none(),
        Err(e) => return Err(format!("cannot read mount point {}: {}", mp_str, e)),
    };
    if is_empty {
        return Ok(Vec::new());
    }

    let canon_mp = mount_point.canonicalize().unwrap_or_else(|_| mount_point.to_path_buf());
    let previously_owned = read_mount_marker(cache_dir).map_or(false, |m| m == canon_mp);
    if !previously_owned {
        return Err(format!(
            "mount point {} is not empty — mounting would hide its contents; move them away first",
            mp_str
        ));
    }

    log::warn!(
        "{} is not empty, but ncrs previously mounted here — treating leftovers as orphaned local edits",
        mp_str
    );
    let leftovers = scan_leftovers(mount_point).map_err(|e| {
        format!(
            "mount point {} has leftover content that was not auto-adopted: {} — move it away first",
            mp_str, e
        )
    })?;
    relocate_leftovers(mount_point, cache_dir, &leftovers)
}

// True if the path appears as a mounted fuse filesystem in /proc/self/mounts.
// A stale (dead-process) mount also appears here, so callers must rule that
// out first via the ENOTCONN stat check.
fn is_live_fuse_mount(mp: &Path) -> bool {
    let canon = mp.canonicalize().unwrap_or_else(|_| mp.to_path_buf());
    // /proc mount entries escape space/tab/newline/backslash as octal
    let escaped = canon
        .to_string_lossy()
        .replace('\\', "\\134")
        .replace(' ', "\\040")
        .replace('\t', "\\011")
        .replace('\n', "\\012");
    std::fs::read_to_string("/proc/self/mounts")
        .map(|mounts| {
            mounts.lines().any(|line| {
                let mut fields = line.split_whitespace();
                let _source = fields.next();
                matches!(
                    (fields.next(), fields.next()),
                    (Some(dir), Some(fstype)) if dir == escaped && fstype.starts_with("fuse")
                )
            })
        })
        .unwrap_or(false)
}

// ── Orphan-write adoption ────────────────────────────────────────────────────
// If ncrs is torn down (or force-detached, e.g. a lazy `fusermount -uz` while
// a file is still open) while an app holds a file open under the mount, a
// later save from that app can land directly on the real, now-exposed
// mount-point directory instead of going through FUSE. On the next start this
// used to be an unconditional refusal ("mount point is not empty"). When the
// mount marker below proves ncrs previously owned this exact path, leftovers
// are instead treated as orphaned local edits and queued for upload — see
// prepare_mount_point and adopt_orphaned_files.

const ADOPT_MAX_FILES: usize = 200;
const ADOPT_MAX_BYTES: u64 = 500 * 1024 * 1024;

fn ncrs_cache_dir(url: &str) -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("ncrs")
        .join(url_to_dir_name(url))
}

/// Where an HTTP/3 demotion for `url`'s server is recorded, and read back from.
fn h3_demotion_marker(url: &str) -> PathBuf {
    ncrs_cache_dir(url).join("h3_demoted")
}

fn mount_marker_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("last_mount_point")
}

// The mount point ncrs most recently mounted successfully, if any. Compared
// against the target mount point to distinguish "first-time setup / a
// different configured path" (must still reject a non-empty directory) from
// "restarting on a path we already own" (safe to adopt leftovers on).
fn read_mount_marker(cache_dir: &Path) -> Option<PathBuf> {
    std::fs::read_to_string(mount_marker_path(cache_dir))
        .ok()
        .map(|s| PathBuf::from(s.trim()))
}

fn write_mount_marker(cache_dir: &Path, mount_point: &Path) {
    let canon = mount_point.canonicalize().unwrap_or_else(|_| mount_point.to_path_buf());
    let _ = std::fs::create_dir_all(cache_dir);
    if let Err(e) = std::fs::write(mount_marker_path(cache_dir), canon.to_string_lossy().as_bytes()) {
        log::warn!("failed to record mount marker for {}: {}", canon.display(), e);
    }
}

/// GNOME Tracker's file miner (and other desktop indexers that follow the same
/// `.trackerignore`/`.nomedia` convention) skips a directory's content when it
/// finds one — this is what stops a recursive index crawl from fanning out
/// across the whole remote tree the moment the mount lands under an indexed
/// location (an XDG special folder, `$HOME` itself, …). See the
/// `bg::READDIR` pool for the other half of that fix: this marker keeps the
/// crawl from starting at all; the pool keeps a crawl that starts anyway (a
/// different indexer, `find`, a backup tool) from spawning an unbounded number
/// of readdir workers.
///
/// Synthesized purely at the FUSE layer — never PUT to the backend — because a
/// real write through the mount turned out to need an actual byte written
/// (`std::fs::write(path, b"")` never issues a `write(2)` for an empty buffer,
/// so the upload path never even ran) and, once fixed, would still depend on
/// the dir cache never evicting/missing the entry before the upload lands.
/// `put_dir_cache` (the single choke point every root listing passes through,
/// fresh or stale) splices this entry back in on every call, so it can never
/// go missing the way a real write's local copy could. The trade-off: unlike
/// a real file, this entry is local to this mount and does not protect a
/// second desktop mounting the same account — only a real write on the server
/// could do that.
///
/// Tracker still sees it through its own directory walk: readdir on the mount
/// root is served by ncrs regardless of whether an entry is a real remote file
/// or this synthetic one, so the listing looks identical from the outside.
///
/// "Opting out": the user can unlink() it, which sets `trackerignore_hidden`
/// for the rest of this mount's lifetime (see `unlink`) — matching the old
/// real-file behavior of never being recreated once deleted within a single
/// mount, but without ever touching the network to do so.
fn trackerignore_path() -> PathBuf {
    PathBuf::from("/.trackerignore")
}

const TRACKERIGNORE_CONTENT: &[u8] = b"\n";

fn trackerignore_entry() -> RemoteEntry {
    RemoteEntry {
        path: trackerignore_path(),
        is_dir: false,
        size: TRACKERIGNORE_CONTENT.len() as u64,
        modified: None,
        change_token: None,
        content_type: Some(backend::intern("text/plain")),
        ext: backend::EntryExtensions::default(),
    }
}

// Junk apps leave next to an open document — safe to discard rather than upload.
fn is_lock_junk(name: &str) -> bool {
    (name.starts_with(".~lock.") && name.ends_with('#')) || name.starts_with(".goutputstream-")
}

#[derive(Debug)]
pub(crate) struct AdoptedFile {
    pub remote_path: PathBuf,
    pub staging_path: PathBuf,
}

enum LeftoverEntry {
    Dir(PathBuf),
    File(PathBuf),
    Junk(PathBuf),
}

// Read-only walk of `root`: collects every leftover entry (paths relative to
// `root`), or fails — without touching anything — if the content doesn't look
// like a plausible stray app-write (too much of it, or an entry type we don't
// recognize, e.g. a symlink or socket).
fn scan_leftovers(root: &Path) -> Result<Vec<LeftoverEntry>, String> {
    let mut out = Vec::new();
    let mut total_bytes = 0u64;
    scan_leftovers_rec(root, &PathBuf::new(), &mut out, &mut total_bytes)?;
    Ok(out)
}

fn scan_leftovers_rec(
    abs_dir: &Path,
    rel_dir: &Path,
    out: &mut Vec<LeftoverEntry>,
    total_bytes: &mut u64,
) -> Result<(), String> {
    let entries = std::fs::read_dir(abs_dir)
        .map_err(|e| format!("cannot read {}: {}", abs_dir.display(), e))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read {}: {}", abs_dir.display(), e))?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();
        let rel_path = if rel_dir.as_os_str().is_empty() {
            PathBuf::from(&name)
        } else {
            rel_dir.join(&name)
        };
        let abs_path = entry.path();
        let file_type = entry.file_type()
            .map_err(|e| format!("cannot stat {}: {}", abs_path.display(), e))?;

        if is_lock_junk(&name_str) {
            out.push(LeftoverEntry::Junk(rel_path));
        } else if file_type.is_dir() {
            out.push(LeftoverEntry::Dir(rel_path.clone()));
            scan_leftovers_rec(&abs_path, &rel_path, out, total_bytes)?;
        } else if file_type.is_file() {
            *total_bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
            out.push(LeftoverEntry::File(rel_path));
        } else {
            return Err(format!("unexpected entry {}", abs_path.display()));
        }

        if out.len() > ADOPT_MAX_FILES || *total_bytes > ADOPT_MAX_BYTES {
            return Err(format!(
                "too much leftover content ({} entries, {} bytes so far)",
                out.len(), *total_bytes
            ));
        }
    }
    Ok(())
}

// Moves every leftover file into cache_dir (same-filesystem rename, so this
// is cheap) and removes the now-empty leftover directories/junk so the mount
// point ends up empty. Processing in reverse of the scan order handles each
// directory's descendants before the directory itself, since scan_leftovers
// always pushes a Dir entry immediately before the entries found within it.
fn relocate_leftovers(
    mount_point: &Path,
    cache_dir: &Path,
    entries: &[LeftoverEntry],
) -> Result<Vec<AdoptedFile>, String> {
    let _ = std::fs::create_dir_all(cache_dir);
    let mut adopted = Vec::new();
    let mut next_id: u64 = 0;
    let pid = std::process::id();
    for entry in entries.iter().rev() {
        match entry {
            LeftoverEntry::Junk(rel) => {
                let _ = std::fs::remove_file(mount_point.join(rel));
            }
            LeftoverEntry::File(rel) => {
                let abs = mount_point.join(rel);
                next_id += 1;
                let staging_path = cache_dir.join(format!("adopted_{}_{}", pid, next_id));
                std::fs::rename(&abs, &staging_path)
                    .map_err(|e| format!("failed to adopt {}: {}", abs.display(), e))?;
                adopted.push(AdoptedFile {
                    remote_path: PathBuf::from("/").join(rel),
                    staging_path,
                });
            }
            LeftoverEntry::Dir(rel) => {
                let abs = mount_point.join(rel);
                std::fs::remove_dir(&abs)
                    .map_err(|e| format!("failed to clear leftover directory {}: {}", abs.display(), e))?;
            }
        }
    }
    Ok(adopted)
}

// Queues every adopted file as a normal pending upload. Ancestor directories
// are created remotely first (root-to-leaf) if they don't already exist —
// mirroring what a normal mkdir()-then-write() through FUSE would have
// established for a file created the ordinary way. If the remote file/dir
// already exists, the current etag is used as the Put's if_match_etag so a
// concurrent remote change is caught by the existing conflict machinery
// (ConflictKind::EditConflict) on replay, instead of being silently clobbered.
fn adopt_orphaned_files(fs: &NextCloudFs, adopted: Vec<AdoptedFile>) {
    if adopted.is_empty() {
        return;
    }
    log::warn!(
        "adopting {} locally-modified file(s) found in the mount point after an interrupted session: {}",
        adopted.len(),
        adopted.iter().map(|a| a.remote_path.display().to_string()).collect::<Vec<_>>().join(", "),
    );

    let mut ensured_dirs: HashSet<PathBuf> = HashSet::new();
    let mut newly_created_dirs: HashSet<PathBuf> = HashSet::new();
    let mut dir_listing_cache: HashMap<PathBuf, Vec<RemoteEntry>> = HashMap::new();

    let list_cached = |dir: PathBuf, cache: &mut HashMap<PathBuf, Vec<RemoteEntry>>| -> Vec<RemoteEntry> {
        if let Some(e) = cache.get(&dir) {
            return e.clone();
        }
        match list_dir_propfind(&fs.conn, dir.clone()) {
            Ok((_, _, entries)) => {
                cache.insert(dir, entries.clone());
                entries
            }
            Err(e) => {
                log::warn!("adopt: could not list {}: {}", dir.display(), e);
                Vec::new()
            }
        }
    };

    for AdoptedFile { remote_path, staging_path } in adopted {
        let mut ancestors: Vec<PathBuf> = Vec::new();
        let mut cur = remote_path.parent().unwrap_or(Path::new("/")).to_path_buf();
        while cur != Path::new("/") {
            ancestors.push(cur.clone());
            cur = cur.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("/"));
        }
        ancestors.reverse();

        for dir in &ancestors {
            if ensured_dirs.contains(dir) {
                continue;
            }
            let parent = dir.parent().unwrap_or(Path::new("/")).to_path_buf();
            let exists = !newly_created_dirs.contains(&parent)
                && list_cached(parent, &mut dir_listing_cache).iter().any(|e| e.is_dir && e.path == *dir);
            if !exists {
                let seq = fs.journal.safe_lock().enqueue(
                    mutation_journal::MutationOp::MkDir { path: dir.clone() },
                );
                log::info!("adopt: queued MKCOL {} (seq {})", dir.display(), seq);
                newly_created_dirs.insert(dir.clone());
            }
            ensured_dirs.insert(dir.clone());
        }

        let parent = remote_path.parent().unwrap_or(Path::new("/")).to_path_buf();
        let if_match_etag = if newly_created_dirs.contains(&parent) {
            None
        } else {
            list_cached(parent, &mut dir_listing_cache)
                .iter()
                .find(|e| !e.is_dir && e.path == remote_path)
                .and_then(|e| e.change_token.clone())
        };

        let seq = fs.journal.safe_lock().enqueue(mutation_journal::MutationOp::Put {
            remote_path: remote_path.clone(),
            staging_path,
            if_match_etag: if_match_etag.clone(),
        });
        if !fs.conn.is_offline.load(Ordering::Relaxed) {
            fs.status.safe_write().insert(remote_path.clone(), FileStatus::Uploading);
        } else {
            fs.status.safe_write().insert(remote_path.clone(), FileStatus::PendingSync);
        }
        fs.dirty.safe_lock().insert(remote_path.clone());
        log::info!(
            "adopt: queued PUT {} (seq {}, if_match_etag={:?})",
            remote_path.display(), seq, if_match_etag,
        );
    }
}

// ── Utilities ─────────────────────────────────────────────────────────────────

pub(crate) fn make_conflict_name(path: &Path) -> PathBuf {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = path.extension().and_then(|e| e.to_str());
    let now = chrono_timestamp();
    let conflict = match ext {
        Some(e) => format!("{} (conflicted copy {}).{}", stem, now, e),
        None => format!("{} (conflicted copy {})", stem, now),
    };
    path.with_file_name(conflict)
}

fn chrono_timestamp() -> String {
    let dur = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs();
    let days = secs / 86400;
    let day_secs = secs % 86400;
    let hours = day_secs / 3600;
    let mins = (day_secs % 3600) / 60;
    let s = day_secs % 60;
    let mut y = 1970i32;
    let mut remaining = days;
    loop {
        let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) { 366 } else { 365 };
        if remaining < days_in_year { break; }
        remaining -= days_in_year;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut m = 0;
    for md in &month_days {
        if remaining < *md { break; }
        remaining -= *md;
        m += 1;
    }
    format!("{:04}-{:02}-{:02} {:02}-{:02}-{:02}", y, m + 1, remaining + 1, hours, mins, s)
}

fn url_to_dir_name(url: &str) -> String {
    url.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '.' { c } else { '_' })
        .collect()
}


#[cfg(test)]
mod tests;
