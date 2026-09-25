/// Unix domain socket IPC server.
///
/// Clients (e.g. the Nautilus extension) connect and send line-oriented queries:
///
///   STATUS <absolute-local-path>\n   → local|synced|remote|unknown[,shared]
///   DETAIL <absolute-local-path>\n   → status\tsharing\tperms\towner\tsize
///   DETAILDIR <absolute-local-dir>\n → per-child records (0x1e-joined) of
///                                       basename\tstatus\tsharing\tperms\towner\tsize
///   SEARCH <term>\n                  → JSON array of SearchResultGroup
///   WEBURL <absolute-local-path>\n   → Nextcloud web URL for the file
///   ERRORS\n                         → JSON array of SyncError
///   TRANSFERS\n                      → JSON array of TransferProgress
///   JOURNAL\n                        → JSON array of pending JournalEntry
///   CONFLICTS\n                      → JSON array of unresolved ConflictRecord
///   RESOLVE_CONFLICT <id>\n          → "ok" (persisted, so it stays resolved)
///   RESOLVE_ALL_CONFLICTS\n          → "ok"
///   CHANGES\n                        → tab-separated changed paths
///   FILE_CHANGES\n                    → tab-separated A:/path or D:/path entries
///   STORAGE\n                         → JSON {kept_bytes, cached_bytes, remote_used, remote_total}
///   STATE\n                           → paused|syncing|idle (daemon-wide sync state)
///   SUBSCRIBE\n                        → SUBSCRIBED, then a `SNAP\t<state>\x1e<errors>
///                                       \x1e<transfers>\x1e<journal>\x1e<conflicts>`
///                                       line on every change, plus `PING` keepalives.
///                                       Lets the GUI react to state pushes instead of
///                                       polling. Additive — old clients never send it.
///   PAUSE\n / RESUME\n                → ok (suspend/resume background sync)
///   THUMBNAIL <abs-path>\n           → ok | error: <msg>  (fetch NC preview → XDG thumb cache)
///   VERSION <n>\n                     → <daemon-protocol>\t<pkg-version>  (n = extension protocol)
///
/// Protocol v3 (additive) — HELLO, CLIENTS, EVENTS, WATCH, INTEGRATIONS. The
/// normative spec is shell_integration/file-managers/PROTOCOL.md; keep it in
/// step with any change here.
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use crate::{MutexExt, RwLockExt};

const MAX_IPC_CLIENTS: usize = crate::bg::IPC_WORKERS;

/// Starts one of the IPC server's two long-lived threads (state publisher, accept loop).
fn spawn_service(name: &str, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = crate::bg::spawn_service(name, f) {
        log::error!("could not start the {} thread: {}", name, e);
    }
}
const CLIENT_READ_TIMEOUT: Duration = Duration::from_secs(60);
/// Longest request line a client may send. `BufRead::lines()` buffers an
/// unbounded amount before yielding, so a peer that never sends `\n` (or
/// sends a huge one) could otherwise grow that buffer without limit. Well
/// above PATH_MAX (4096) to leave room for percent-encoding and command
/// prefixes, far below "unbounded".
const MAX_IPC_LINE_LEN: usize = 16 * 1024;

/// Counts how often directory-status aggregation takes the O(N_total)
/// whole-status-map fallback because the `ChildrenMap` had no entry for the
/// directory. Believed rare (a cold cache before the first readdir), so it is
/// logged sparsely — the first hit, then every 1000th — to reveal a real hot
/// spot without noise, instead of silently paying a full-map scan per query.
static DIR_STATUS_FALLBACKS: AtomicU64 = AtomicU64::new(0);

fn note_dir_status_fallback(context: &str, dir: &Path) {
    let n = DIR_STATUS_FALLBACKS.fetch_add(1, Ordering::Relaxed) + 1;
    if n == 1 || n % 1000 == 0 {
        log::info!(
            "dir-status children-map miss #{} ({}): {} — O(N) status-map scan",
            n,
            context,
            dir.display()
        );
    }
}

/// IPC protocol version. Bump whenever the daemon⇄extension contract changes
/// (a new command, a changed reply format). The Nautilus extension announces
/// its own copy of this on connect via `VERSION`; a mismatch is logged so a
/// half-updated install (new daemon + old extension, or vice-versa) is obvious.
/// Keep in sync with `PROTOCOL_VERSION` in shell_integration/file-managers/nautilus/syncstate.py.
pub const PROTOCOL_VERSION: u32 = 3;

/// Capabilities advertised in the `HELLO` reply.
pub const CAPABILITIES: &str = "detaildir,events,watch,search,thumbnail,weburl,keep,evict,integrations";

/// Most records one `EVENTS` reply or `WATCH` line carries; the client asks
/// again with the returned `next` to page through the rest.
const EVENTS_PAGE: usize = 1000;

const QUERY_ENCODE: &AsciiSet = &CONTROLS
    .add(b' ').add(b'#').add(b'%').add(b'&').add(b'+').add(b'=').add(b'?');

pub type KeepCallback = Arc<dyn Fn(PathBuf) + Send + Sync>;
pub type EvictCallback = Arc<dyn Fn(PathBuf) + Send + Sync>;
pub type PrefetchCallback = Arc<dyn Fn(PathBuf) + Send + Sync>;
/// Synchronously fetch the Nextcloud preview for a remote path and write it to
/// the XDG thumbnail cache. Returns `true` if the thumbnail is now available.
pub type ThumbnailCallback = Arc<dyn Fn(PathBuf) -> bool + Send + Sync>;
/// Drop every locally cached file copy so it re-downloads fresh, EXCEPT files
/// with a pending upload (unsynced local edits — purging those is data loss).
/// Returns the number of cached files removed, or an error string.
pub type PurgeCallback = Arc<dyn Fn() -> Result<usize, String> + Send + Sync>;
pub type SharedSet = Arc<RwLock<std::collections::HashSet<PathBuf>>>;
pub type FileIdMap = Arc<RwLock<std::collections::HashMap<PathBuf, u64>>>;

#[derive(Clone, Default)]
pub struct FileDetail {
    pub permissions: Option<String>,
    pub owner_id: Option<String>,
    pub owner_display_name: Option<String>,
    pub size: u64,
    pub is_dir: bool,
}

pub type FileDetailMap = Arc<RwLock<std::collections::HashMap<PathBuf, FileDetail>>>;
pub type ChildrenMap = Arc<RwLock<std::collections::HashMap<PathBuf, std::collections::HashSet<PathBuf>>>>;
pub type DirtySet = Arc<Mutex<std::collections::HashSet<PathBuf>>>;

#[derive(Clone)]
pub enum FileChangeKind {
    Added,
    Removed,
    Modified,
    DirAdded,
    DirRemoved,
    Renamed { from: PathBuf },
}

#[derive(Clone)]
pub struct FileChange {
    pub kind: FileChangeKind,
    pub path: PathBuf,
}

pub type FileChangeQueue = Arc<Mutex<Vec<FileChange>>>;

fn abs_path(mount_point: &Path, remote: &Path) -> PathBuf {
    mount_point.join(remote.strip_prefix("/").unwrap_or(remote))
}

/// Wire form of one structural change, shared by `FILE_CHANGES` and `EVENTS`.
fn encode_file_change(c: &FileChange, mount_point: &Path) -> String {
    let abs = abs_path(mount_point, &c.path);
    match &c.kind {
        FileChangeKind::Added => format!("A:{}", abs.display()),
        FileChangeKind::Removed => format!("D:{}", abs.display()),
        FileChangeKind::Modified => format!("M:{}", abs.display()),
        FileChangeKind::DirAdded => format!("DA:{}", abs.display()),
        FileChangeKind::DirRemoved => format!("DD:{}", abs.display()),
        FileChangeKind::Renamed { from } => {
            format!("R:{}\x1e{}", abs_path(mount_point, from).display(), abs.display())
        }
    }
}

fn encode_record(rec: &crate::change_log::ChangeRecord, mount_point: &Path) -> String {
    match rec {
        crate::change_log::ChangeRecord::Status(p) => format!("S:{}", abs_path(mount_point, p).display()),
        crate::change_log::ChangeRecord::File(c) => encode_file_change(c, mount_point),
    }
}

/// `<next>\t<rec>\t<rec>…`, or `<next>\tRESYNC` when the reader fell off the ring.
fn encode_events(res: &crate::change_log::ReadResult, mount_point: &Path) -> String {
    let mut out = res.next.to_string();
    if res.resync {
        out.push_str("\tRESYNC");
        return out;
    }
    for rec in &res.records {
        out.push('\t');
        out.push_str(&encode_record(rec, mount_point));
    }
    out
}

/// A connected client that identified itself with `HELLO`.
#[derive(Clone, serde::Serialize)]
pub struct ClientInfo {
    pub id: String,
    pub proto: u32,
    pub pid: u32,
    #[serde(skip)]
    pub connected_at: std::time::Instant,
    pub connected_secs: u64,
}

#[derive(Default)]
pub struct ClientRegistry {
    next_conn: AtomicU64,
    clients: Mutex<std::collections::HashMap<u64, ClientInfo>>,
}

impl ClientRegistry {
    fn new_conn_id(&self) -> u64 {
        self.next_conn.fetch_add(1, Ordering::Relaxed)
    }

    fn register(&self, conn: u64, info: ClientInfo) {
        self.clients.safe_lock().insert(conn, info);
    }

    fn remove(&self, conn: u64) {
        self.clients.safe_lock().remove(&conn);
    }

    /// Snapshot of connected clients, oldest first.
    pub fn list(&self) -> Vec<ClientInfo> {
        let mut v: Vec<ClientInfo> = self
            .clients
            .safe_lock()
            .values()
            .cloned()
            .map(|mut c| {
                c.connected_secs = c.connected_at.elapsed().as_secs();
                c
            })
            .collect();
        v.sort_by(|a, b| b.connected_secs.cmp(&a.connected_secs));
        v
    }

    /// Whether any client with this id is connected.
    pub fn is_connected(&self, id: &str) -> bool {
        self.clients.safe_lock().values().any(|c| c.id == id)
    }
}

/// State shared by every connection that the protocol-v3 verbs need.
#[derive(Clone)]
pub struct V3Context {
    pub change_log: Arc<crate::change_log::ChangeLog>,
    pub clients: Arc<ClientRegistry>,
    /// Desktop / file-browser profiles (absent in callers that never start one).
    pub desktop: Option<Arc<crate::desktop::Manager>>,
}

/// Peer process id of a Unix-socket connection (0 if unavailable).
fn peer_pid(stream: &std::os::unix::net::UnixStream) -> u32 {
    use std::os::unix::io::AsRawFd;
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into `cred`, a properly
    // sized and aligned ucred owned by this frame.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc == 0 { cred.pid.max(0) as u32 } else { 0 }
}

/// A client id is echoed into logs and JSON; keep it short and printable.
fn sanitize_client_id(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
        .take(64)
        .collect()
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct StorageStats {
    pub kept_bytes: u64,
    pub cached_bytes: u64,
    pub remote_used: u64,
    pub remote_total: u64,
}

pub type SharedStorageStats = Arc<Mutex<StorageStats>>;

pub fn socket_path() -> PathBuf {
    socket_dir().join("ncrs.sock")
}

/// Directory holding the IPC socket.
///
/// `$XDG_RUNTIME_DIR` is per-user and mode 0700, so a socket there is already
/// unreachable by other users. The fallback is not: dropping the socket straight
/// into a shared `/tmp` would let any local user connect to the daemon — the IPC
/// surface exposes file metadata and `PAUSE`/`PURGE_CACHE` — and would let one
/// pre-create the path to deny or intercept the channel. So when there is no
/// runtime dir, use a private per-uid subdirectory instead of the shared root.
fn socket_dir() -> PathBuf {
    if let Ok(d) = std::env::var("XDG_RUNTIME_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    if let Some(d) = dirs::runtime_dir() {
        return d;
    }
    #[cfg(unix)]
    {
        // Safe: getuid cannot fail and touches no memory we own.
        let uid = unsafe { libc::getuid() };
        let dir = std::env::temp_dir().join(format!("ncrs-{}", uid));
        if let Err(e) = create_private_dir(&dir) {
            log::warn!("could not secure IPC socket dir {}: {}", dir.display(), e);
        }
        return dir;
    }
    #[cfg(not(unix))]
    std::env::temp_dir()
}

/// Creates `dir` as 0700, and tightens it if it already exists.
#[cfg(unix)]
fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if !dir.exists() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "IPC socket directory path exists but is not a directory",
        ));
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// File-level sync status as seen by the daemon.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Kept,
    Cached,
    Synced,
    Remote,
    Downloading,
    Uploading,
    /// Written locally but not yet on the server — the upload failed or the
    /// network is down and it is queued in the mutation journal for retry.
    PendingSync,
    Unknown,
}

impl FileStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            FileStatus::Kept => "kept",
            FileStatus::Cached => "cached",
            FileStatus::Synced => "synced",
            FileStatus::Remote => "remote",
            FileStatus::Downloading => "downloading",
            FileStatus::Uploading => "uploading",
            FileStatus::PendingSync => "pending",
            FileStatus::Unknown => "unknown",
        }
    }
}

/// Thread-safe store of path → status, updated by the FUSE layer.
pub type StatusMap = Arc<RwLock<std::collections::HashMap<PathBuf, FileStatus>>>;

fn dir_status_from_children(
    sm: &std::collections::HashMap<PathBuf, FileStatus>,
    dir: &Path,
    children: &std::collections::HashMap<PathBuf, std::collections::HashSet<PathBuf>>,
) -> &'static str {
    let own = sm.get(dir).copied();
    if own == Some(FileStatus::Downloading) {
        return "downloading";
    }
    if own == Some(FileStatus::Uploading) {
        return "uploading";
    }
    let mut total = 0usize;
    let mut local = 0usize;
    let mut all_kept = true;
    let mut uploading = 0usize;
    if let Some(child_set) = children.get(dir) {
        for p in child_set {
            total += 1;
            match sm.get(p).copied().unwrap_or(FileStatus::Remote) {
                FileStatus::Kept | FileStatus::Synced => local += 1,
                FileStatus::Cached => { local += 1; all_kept = false; }
                FileStatus::Uploading => uploading += 1,
                _ => { all_kept = false; }
            }
        }
    } else {
        // ChildrenMap not yet populated for this directory (e.g. readdir in progress
        // or dir accessed only via individual file opens before the first FUSE readdir).
        // Fall back to scanning status_map directly — O(N_total) but only on a cold cache.
        note_dir_status_fallback("dir_status_from_children", dir);
        for (p, s) in sm.iter() {
            if p.parent() == Some(dir) {
                total += 1;
                match s {
                    FileStatus::Kept | FileStatus::Synced => local += 1,
                    FileStatus::Cached => { local += 1; all_kept = false; }
                    FileStatus::Uploading => uploading += 1,
                    _ => { all_kept = false; }
                }
            }
        }
    }
    if uploading > 0 { return "uploading"; }
    if total > 0 && local == total {
        if all_kept { "kept" } else { "cached" }
    }
    else if local > 0 { "partial" }
    else { own.unwrap_or(FileStatus::Remote).as_str() }
}

/// Rolled-up child-status counts for one directory, matching the aggregation
/// in [`dir_status_from_children`] (total children, how many are local, whether
/// all local children are pinned, how many are uploading).
#[derive(Clone, Copy, Default)]
struct DirAgg {
    total: usize,
    local: usize,
    all_kept: bool,
    uploading: usize,
}

/// Build the `DETAILDIR` reply records (one per direct child of `remote_dir`).
/// Each record is `basename\tstatus\tsharing\tperms\towner\tsize`. Kept as a
/// free function so it can be unit-tested without a live socket.
///
/// Directory children need the same rolled-up status as `dir_status_from_children`,
/// which scans the whole status map. Calling it once per subdirectory would be
/// O(subdirs × status_map); instead we aggregate every subdirectory's children
/// in a single pass, making the whole batch O(status_map + children) and keeping
/// the status-map lock held for as little time as possible (the FUSE layer needs it).
fn detaildir_records(
    remote_dir: &Path,
    details: &std::collections::HashMap<PathBuf, FileDetail>,
    sm: &std::collections::HashMap<PathBuf, FileStatus>,
    shared: &std::collections::HashSet<PathBuf>,
    children: &std::collections::HashMap<PathBuf, std::collections::HashSet<PathBuf>>,
    username: &str,
) -> Vec<String> {
    // Use the ChildrenMap index when populated; fall back to an O(N_total) detail_map scan
    // for the rare case where children_map has no entry (e.g. readdir reply.ok() sent but
    // post-reply rebuild not yet run, or directory accessed only via individual lookups).
    let dir_children_owned: Vec<PathBuf>;
    let dir_children: &[PathBuf] = if let Some(c) = children.get(remote_dir) {
        dir_children_owned = c.iter().cloned().collect();
        &dir_children_owned
    } else {
        note_dir_status_fallback("detaildir_records", remote_dir);
        dir_children_owned = details.keys()
            .filter(|p| p.parent() == Some(remote_dir))
            .cloned()
            .collect();
        &dir_children_owned
    };
    if dir_children.is_empty() {
        return Vec::new();
    }

    // Aggregate status for each direct-child directory using the children index
    // (O(dir_size + sum of subdir sizes) instead of two O(N_total) scans).
    let mut dir_agg: std::collections::HashMap<&Path, DirAgg> = std::collections::HashMap::new();
    for child in dir_children {
        if let Some(detail) = details.get(child) {
            if detail.is_dir {
                let agg = dir_agg.entry(child.as_path()).or_insert(DirAgg {
                    all_kept: true,
                    ..DirAgg::default()
                });
                if let Some(grandchildren) = children.get(child) {
                    for gc in grandchildren {
                        agg.total += 1;
                        match sm.get(gc).copied().unwrap_or(FileStatus::Remote) {
                            FileStatus::Kept | FileStatus::Synced => agg.local += 1,
                            FileStatus::Cached => { agg.local += 1; agg.all_kept = false; }
                            FileStatus::Uploading => agg.uploading += 1,
                            _ => { agg.all_kept = false; }
                        }
                    }
                }
            }
        }
    }

    let dir_status = |dir: &Path| -> &'static str {
        let own = sm.get(dir).copied();
        if own == Some(FileStatus::Downloading) {
            return "downloading";
        }
        if own == Some(FileStatus::Uploading) {
            return "uploading";
        }
        let a = dir_agg.get(dir).copied().unwrap_or(DirAgg {
            all_kept: true,
            ..DirAgg::default()
        });
        if a.uploading > 0 {
            "uploading"
        } else if a.total > 0 && a.local == a.total {
            if a.all_kept { "kept" } else { "cached" }
        } else if a.local > 0 {
            "partial"
        } else {
            own.unwrap_or(FileStatus::Remote).as_str()
        }
    };

    let mut records = Vec::new();
    for child in dir_children {
        let detail = match details.get(child) {
            Some(d) => d,
            None => continue,
        };
        let name = match child.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        let status = if detail.is_dir {
            dir_status(child)
        } else {
            sm.get(child).copied().unwrap_or(FileStatus::Remote).as_str()
        };
        let sharing = if !shared.contains(child) {
            ""
        } else {
            match detail.owner_id.as_deref() {
                Some(owner) if owner == username => "Shared by you",
                Some(_) => "Shared with you",
                None => "Shared",
            }
        };
        let perms = detail.permissions.as_deref().unwrap_or("");
        let owner = detail.owner_display_name.as_deref().unwrap_or("");
        records.push(format!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            name, status, sharing, perms, owner, detail.size
        ));
    }
    records
}

/// Coalescing push channel for daemon→subscriber state updates.
///
/// A single monitor thread rebuilds the combined state snapshot on a short
/// cadence and calls [`StatePush::publish`]; each `SUBSCRIBE` client blocks in
/// [`StatePush::wait`] and is woken only when the snapshot actually changes. So
/// an idle daemon publishes an unchanged snapshot (a no-op that wakes nobody)
/// and idle subscribers stay parked on a socket read, burning no CPU — the work
/// the GUI used to spend re-polling every 2s disappears.
#[derive(Default)]
struct PushInner {
    generation: u64,
    snapshot: String,
}

pub struct StatePush {
    inner: Mutex<PushInner>,
    cv: Condvar,
}

impl StatePush {
    fn new() -> Self {
        StatePush {
            inner: Mutex::new(PushInner::default()),
            cv: Condvar::new(),
        }
    }

    /// Publish a fresh snapshot. Bumps the generation and wakes waiters only
    /// when it differs from the current one, so unchanged state is free.
    fn publish(&self, snapshot: String) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.snapshot != snapshot {
            g.generation += 1;
            g.snapshot = snapshot;
            drop(g);
            self.cv.notify_all();
        }
    }

    /// Current generation (the snapshot itself is rebuilt fresh by the caller).
    fn current_generation(&self) -> u64 {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).generation
    }

    /// Block until the generation moves past `last`, or `timeout` elapses.
    /// Returns the new `(generation, snapshot)`; `generation == last` on timeout.
    fn wait(&self, last: u64, timeout: Duration) -> (u64, String) {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let (g, _) = self
            .cv
            .wait_timeout_while(g, timeout, |s| s.generation == last)
            .unwrap_or_else(|e| e.into_inner());
        (g.generation, g.snapshot.clone())
    }
}

/// Serialize the journal's pending entries to a JSON array (or `"[]"`). Shared
/// by the `JOURNAL` command handler and the snapshot builders.
fn journal_entries_json(j: &crate::mutation_journal::MutationJournal) -> String {
    let entries: Vec<&crate::mutation_journal::JournalEntry> = j.entries().iter().collect();
    serde_json::to_string(&entries).unwrap_or_else(|_| "[]".to_string())
}

/// Serialize the journal's unresolved conflicts to a JSON array (or `"[]"`).
/// Shared by the `CONFLICTS` command handler and the snapshot builders.
fn conflicts_json(j: &crate::mutation_journal::MutationJournal) -> String {
    serde_json::to_string(&j.unresolved_conflicts()).unwrap_or_else(|_| "[]".to_string())
}

/// Serialize the journal entries and unresolved conflicts together (one lock).
/// This is the expensive part of a snapshot when a big offline backlog has
/// accumulated, so callers cache the result keyed by [`MutationJournal::version`].
fn serialize_journal(journal: &crate::mutation_journal::SharedJournal) -> (String, String) {
    let j = journal.safe_lock();
    (journal_entries_json(&j), conflicts_json(&j))
}

/// The daemon-wide state word, shared by the STATE verb and the pushed snapshot.
///
/// Pause wins: it is the user's own decision, and it stops sync whether or not
/// the server is up. A settled outage comes next — with the server unreachable
/// nothing works, so reporting the queued transfers as "syncing" would claim
/// progress that cannot happen. `settled` (not the raw flag) is what keeps a
/// two-second blip from flashing a failure at the user.
fn state_word(paused: &AtomicBool, offline: &crate::OfflineStatus, active: bool) -> &'static str {
    if paused.load(Ordering::Relaxed) {
        "paused"
    } else if offline.settled() {
        "offline"
    } else if active {
        "syncing"
    } else {
        "idle"
    }
}

/// The cheap (non-journal) snapshot fields: the STATE word, the errors and
/// transfers JSON, and whether any transfer is active. Locks `transfer_map` and
/// `error_log` once each. Cheap to recompute every monitor tick, unlike the
/// journal, so it needs no version cache.
fn collect_cheap_fields(
    paused: &AtomicBool,
    offline: &crate::OfflineStatus,
    transfer_map: &crate::TransferMap,
    error_log: &crate::ErrorLog,
) -> (&'static str, String, String, bool) {
    let transfers: Vec<crate::TransferProgress> = crate::transfer_snapshot(transfer_map);
    let active = !transfers.is_empty();
    let state = state_word(paused, offline, active);
    let errors: Vec<crate::SyncError> = error_log.safe_lock().iter().cloned().collect();
    (
        state,
        serde_json::to_string(&errors).unwrap_or_else(|_| "[]".to_string()),
        serde_json::to_string(&transfers).unwrap_or_else(|_| "[]".to_string()),
        active,
    )
}

/// Join the five snapshot fields into the one-line `SUBSCRIBE` payload. Fields
/// are delimited by 0x1e; serde escapes control chars, so 0x1e never appears
/// inside the JSON and the payload stays on a single line.
fn format_snapshot(
    state: &str,
    errors_json: &str,
    transfers_json: &str,
    journal_json: &str,
    conflicts_json: &str,
) -> String {
    format!(
        "{}\x1e{}\x1e{}\x1e{}\x1e{}",
        state, errors_json, transfers_json, journal_json, conflicts_json
    )
}

/// Build a complete snapshot, serializing the journal fresh. Used for the
/// one-shot initial send to a new subscriber; the monitor loop caches instead.
fn build_state_snapshot(
    paused: &AtomicBool,
    offline: &crate::OfflineStatus,
    transfer_map: &crate::TransferMap,
    error_log: &crate::ErrorLog,
    journal: &crate::mutation_journal::SharedJournal,
) -> String {
    let (journal_json, conflicts_json) = serialize_journal(journal);
    let (state, errors_json, transfers_json, _active) =
        collect_cheap_fields(paused, offline, transfer_map, error_log);
    format_snapshot(state, &errors_json, &transfers_json, &journal_json, &conflicts_json)
}

/// Rebuild the state snapshot on a short cadence and publish it. Fast only while
/// transfers are live. Two costs are avoided when nothing changed: the journal
/// (costly to serialize with a large offline backlog) is re-serialized only when
/// its version moves, and the full payload is assembled/published only when a
/// field actually changed — so an idle tick neither copies the cached journal
/// JSON nor wakes any subscriber.
fn spawn_state_monitor(
    push: Arc<StatePush>,
    paused: Arc<AtomicBool>,
    offline: crate::OfflineStatus,
    transfer_map: crate::TransferMap,
    error_log: crate::ErrorLog,
    journal: crate::mutation_journal::SharedJournal,
) {
    spawn_service("ipc-state", move || {
        let mut cached_version = u64::MAX; // forces a build+publish on the first tick
        let mut journal_json = String::from("[]");
        let mut conflicts_json = String::from("[]");
        let mut last_state = "";
        let mut last_errors_json = String::new();
        let mut last_transfers_json = String::new();
        loop {
            let version = journal.safe_lock().version();
            let journal_changed = version != cached_version;
            if journal_changed {
                let (j, c) = serialize_journal(&journal);
                journal_json = j;
                conflicts_json = c;
                cached_version = version;
            }
            let (state, errors_json, transfers_json, active) =
                collect_cheap_fields(&paused, &offline, &transfer_map, &error_log);
            if journal_changed
                || state != last_state
                || errors_json != last_errors_json
                || transfers_json != last_transfers_json
            {
                push.publish(format_snapshot(
                    state,
                    &errors_json,
                    &transfers_json,
                    &journal_json,
                    &conflicts_json,
                ));
                last_state = state;
                last_errors_json = errors_json;
                last_transfers_json = transfers_json;
            }
            std::thread::sleep(if active {
                Duration::from_millis(500)
            } else {
                Duration::from_secs(2)
            });
        }
    });
}

/// Start the IPC socket server in a background thread.
///
/// `mount_point` is the local FUSE mount directory; paths outside it return Unknown.
pub fn start_server(mount_point: PathBuf, status_map: StatusMap, shared_set: SharedSet, fileid_map: FileIdMap, detail_map: FileDetailMap, children_map: ChildrenMap, dirty_set: DirtySet, creds: crate::auth::Credentials, base_url: String, keep_cb: Option<KeepCallback>, evict_cb: Option<EvictCallback>, prefetch_cb: Option<PrefetchCallback>, thumbnail_cb: Option<ThumbnailCallback>, purge_cb: Option<PurgeCallback>, error_log: crate::ErrorLog, transfer_map: crate::TransferMap, journal: crate::mutation_journal::SharedJournal, file_change_queue: FileChangeQueue, storage_stats: SharedStorageStats, paused: Arc<AtomicBool>, offline: crate::OfflineStatus, passthrough_enabled: Arc<AtomicBool>, passthrough_capable: Arc<AtomicBool>, desktop: Option<Arc<crate::desktop::Manager>>) {
    let sock = socket_path();
    let _ = std::fs::remove_file(&sock);

    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            log::error!("Cannot bind IPC socket {}: {}", sock.display(), e);
            return;
        }
    };
    // `bind` applies the umask, so the socket can land group/other-writable —
    // and on Linux, connect(2) is gated by write permission on the socket file.
    // Narrow it to the owner before anyone can reach it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)) {
            log::error!("Cannot secure IPC socket {}: {} — refusing to serve", sock.display(), e);
            let _ = std::fs::remove_file(&sock);
            return;
        }
    }
    log::info!("IPC socket listening at {}", sock.display());

    // Drive daemon→GUI state pushes: the monitor rebuilds the snapshot and
    // subscribers block until it changes.
    let state_push = Arc::new(StatePush::new());
    spawn_state_monitor(
        state_push.clone(),
        paused.clone(),
        offline.clone(),
        transfer_map.clone(),
        error_log.clone(),
        journal.clone(),
    );

    // Broadcast change log: the pump is the only drainer of the producer
    // buffers, and every client reads through its own cursor.
    let v3 = V3Context {
        change_log: Arc::new(crate::change_log::ChangeLog::new(crate::change_log::DEFAULT_CAPACITY)),
        clients: Arc::new(ClientRegistry::default()),
        desktop,
    };
    {
        let log = v3.change_log.clone();
        let dirty = dirty_set.clone();
        let fcq = file_change_queue.clone();
        spawn_service("change-log", move || loop {
            log.pump(&dirty, &fcq);
            std::thread::sleep(Duration::from_millis(250));
        });
    }

    let active = Arc::new(AtomicUsize::new(0));

    spawn_service("ipc-accept", move || {
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    log::error!("IPC accept error: {}", e);
                    continue;
                }
            };
            if active.load(Ordering::Relaxed) >= MAX_IPC_CLIENTS {
                log::warn!("IPC connection limit ({}) reached, rejecting", MAX_IPC_CLIENTS);
                continue;
            }
            let _ = stream.set_read_timeout(Some(CLIENT_READ_TIMEOUT));
            let mount = mount_point.clone();
            let map = status_map.clone();
            let shared = shared_set.clone();
            let fids = fileid_map.clone();
            let details = detail_map.clone();
            let children = children_map.clone();
            let dirty = dirty_set.clone();
            let creds_clone = creds.clone();
            let burl = base_url.clone();
            let cb = keep_cb.clone();
            let ev = evict_cb.clone();
            let pf = prefetch_cb.clone();
            let th = thumbnail_cb.clone();
            let pu = purge_cb.clone();
            let elog = error_log.clone();
            let tmap = transfer_map.clone();
            let jrnl = journal.clone();
            let fcq = file_change_queue.clone();
            let sstats = storage_stats.clone();
            let pause_flag = paused.clone();
            let offline_flag = offline.clone();
            let sp = state_push.clone();
            let pt_enabled = passthrough_enabled.clone();
            let pt_capable = passthrough_capable.clone();
            let active = active.clone();
            let v3c = v3.clone();
            let active_reject = active.clone();
            active.fetch_add(1, Ordering::Relaxed);
            let admitted = crate::bg::IPC.submit(move || {
                handle_client(stream, mount, map, shared, fids, details, children, dirty, creds_clone, burl, cb, ev, pf, th, pu, elog, tmap, jrnl, fcq, sstats, pause_flag, offline_flag, sp, pt_enabled, pt_capable, v3c);
                active.fetch_sub(1, Ordering::Relaxed);
            });
            if admitted.is_err() {
                // The job (and the stream it owned) was dropped: the client sees a
                // closed socket and reconnects later.
                active_reject.fetch_sub(1, Ordering::Relaxed);
            }
        }
    });
}

/// Read one `\n`-terminated line, capped at `max_len` bytes. Returns `Ok(None)`
/// on clean EOF with nothing pending, `Err` if the line exceeds `max_len`
/// before a newline arrives (the caller should drop the connection — the
/// stream position after an overlong line is not a line boundary anymore).
fn read_line_bounded<R: BufRead>(reader: &mut R, max_len: usize) -> std::io::Result<Option<String>> {
    let mut buf = Vec::new();
    for byte in reader.bytes() {
        let b = byte?;
        if b == b'\n' {
            return Ok(Some(String::from_utf8_lossy(&buf).trim_end_matches('\r').to_string()));
        }
        buf.push(b);
        if buf.len() > max_len {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("IPC request line exceeds {} bytes", max_len),
            ));
        }
    }
    if buf.is_empty() {
        Ok(None)
    } else {
        Ok(Some(String::from_utf8_lossy(&buf).trim_end_matches('\r').to_string()))
    }
}

fn strip_mount<'a>(path: &'a Path, mount_point: &Path) -> Option<PathBuf> {
    if path.starts_with(mount_point) {
        Some(
            path.strip_prefix(mount_point)
                .map(|p| Path::new("/").join(p))
                .unwrap_or_else(|_| PathBuf::from("/")),
        )
    } else {
        None
    }
}

fn handle_client(
    stream: std::os::unix::net::UnixStream,
    mount_point: PathBuf,
    status_map: StatusMap,
    shared_set: SharedSet,
    fileid_map: FileIdMap,
    detail_map: FileDetailMap,
    children_map: ChildrenMap,
    dirty_set: DirtySet,
    creds: crate::auth::Credentials,
    base_url: String,
    keep_cb: Option<KeepCallback>,
    evict_cb: Option<EvictCallback>,
    prefetch_cb: Option<PrefetchCallback>,
    thumbnail_cb: Option<ThumbnailCallback>,
    purge_cb: Option<PurgeCallback>,
    error_log: crate::ErrorLog,
    transfer_map: crate::TransferMap,
    journal: crate::mutation_journal::SharedJournal,
    file_change_queue: FileChangeQueue,
    storage_stats: SharedStorageStats,
    paused: Arc<AtomicBool>,
    offline: crate::OfflineStatus,
    state_push: Arc<StatePush>,
    passthrough_enabled: Arc<AtomicBool>,
    passthrough_capable: Arc<AtomicBool>,
    v3: V3Context,
) {
    let mut write_half = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let conn = ConnState::new(&stream, v3);
    let mut reader = BufReader::new(stream);
    handle_client_loop(&mut reader, &mut write_half, &conn, mount_point, status_map, shared_set, fileid_map, detail_map, children_map, dirty_set, creds, base_url, keep_cb, evict_cb, prefetch_cb, thumbnail_cb, purge_cb, error_log, transfer_map, journal, file_change_queue, storage_stats, paused, offline, state_push, passthrough_enabled, passthrough_capable);
}

/// Per-connection bookkeeping for the v3 verbs; undone on drop.
struct ConnState {
    v3: V3Context,
    conn_id: u64,
    pid: u32,
    key: crate::change_log::CursorKey,
    /// `HELLO` client-id, empty until the client says hello.
    client_id: Mutex<String>,
    /// Holds a legacy cursor (only clients that poll CHANGES/FILE_CHANGES or
    /// announce with VERSION do, so a GUI connection pins no history).
    attached: AtomicBool,
    live_reader: AtomicBool,
}

impl ConnState {
    fn new(stream: &std::os::unix::net::UnixStream, v3: V3Context) -> Self {
        let pid = peer_pid(stream);
        let key = crate::change_log::CursorKey { pid };
        ConnState {
            conn_id: v3.clients.new_conn_id(),
            v3,
            pid,
            key,
            client_id: Mutex::new(String::new()),
            attached: AtomicBool::new(false),
            live_reader: AtomicBool::new(false),
        }
    }

    /// Join (or create, at the current head) this process's legacy cursor.
    fn ensure_attached(&self) {
        if !self.attached.swap(true, Ordering::Relaxed) {
            self.v3.change_log.attach(&self.key);
        }
    }

    fn key(&self) -> crate::change_log::CursorKey {
        self.key
    }

    fn mark_live_reader(&self) {
        if !self.live_reader.swap(true, Ordering::Relaxed) {
            self.v3.change_log.add_live_reader();
        }
    }
}

impl Drop for ConnState {
    fn drop(&mut self) {
        if self.attached.load(Ordering::Relaxed) {
            self.v3.change_log.detach(&self.key);
        }
        self.v3.clients.remove(self.conn_id);
        if self.live_reader.load(Ordering::Relaxed) {
            self.v3.change_log.remove_live_reader();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_client_loop(
    reader: &mut BufReader<std::os::unix::net::UnixStream>,
    write_half: &mut std::os::unix::net::UnixStream,
    conn: &ConnState,
    mount_point: PathBuf,
    status_map: StatusMap,
    shared_set: SharedSet,
    fileid_map: FileIdMap,
    detail_map: FileDetailMap,
    children_map: ChildrenMap,
    dirty_set: DirtySet,
    creds: crate::auth::Credentials,
    base_url: String,
    keep_cb: Option<KeepCallback>,
    evict_cb: Option<EvictCallback>,
    prefetch_cb: Option<PrefetchCallback>,
    thumbnail_cb: Option<ThumbnailCallback>,
    purge_cb: Option<PurgeCallback>,
    error_log: crate::ErrorLog,
    transfer_map: crate::TransferMap,
    journal: crate::mutation_journal::SharedJournal,
    file_change_queue: FileChangeQueue,
    storage_stats: SharedStorageStats,
    paused: Arc<AtomicBool>,
    offline: crate::OfflineStatus,
    state_push: Arc<StatePush>,
    passthrough_enabled: Arc<AtomicBool>,
    passthrough_capable: Arc<AtomicBool>,
) {
    loop {
        let line = match read_line_bounded(reader, MAX_IPC_LINE_LEN) {
            Ok(Some(l)) => l,
            Ok(None) => break,
            Err(e) => {
                log::warn!("IPC client sent an oversized or unreadable request: {}", e);
                break;
            }
        };
        let trimmed = line.trim();

        // SUBSCRIBE hands this connection to the push loop: acknowledge, send the
        // current snapshot, then a SNAP line whenever state changes (with a PING
        // keepalive so a dead peer is noticed). It never returns to reading verbs.
        // WATCH hands this connection to the change stream: `WATCHING\t<seq>`,
        // then `EV\t<next>\t<records>` whenever the log moves (same record
        // syntax as EVENTS), plus `PING` keepalives. Never returns to verbs.
        if trimmed == "WATCH" || trimmed.starts_with("WATCH ") {
            let log = &conn.v3.change_log;
            conn.mark_live_reader();
            log.pump(&dirty_set, &file_change_queue);
            let mut since = trimmed
                .strip_prefix("WATCH ")
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or_else(|| log.head());
            if writeln!(write_half, "WATCHING\t{}", since).is_err() {
                return;
            }
            loop {
                let res = log.read_since(since, EVENTS_PAGE);
                if res.resync || !res.records.is_empty() {
                    since = res.next;
                    if writeln!(write_half, "EV\t{}", encode_events(&res, &mount_point)).is_err() {
                        return;
                    }
                    continue;
                }
                since = res.next;
                if log.wait_past(since, Duration::from_secs(20)) <= since
                    && writeln!(write_half, "PING").is_err()
                {
                    return;
                }
            }
        }

        if trimmed == "SUBSCRIBE" {
            if writeln!(write_half, "SUBSCRIBED").is_err() {
                return;
            }
            let mut last_gen = state_push.current_generation();
            let initial = build_state_snapshot(&paused, &offline, &transfer_map, &error_log, &journal);
            if writeln!(write_half, "SNAP\t{}", initial).is_err() {
                return;
            }
            loop {
                let (gen, snap) = state_push.wait(last_gen, Duration::from_secs(20));
                if gen == last_gen {
                    if writeln!(write_half, "PING").is_err() {
                        return;
                    }
                    continue;
                }
                last_gen = gen;
                if writeln!(write_half, "SNAP\t{}", snap).is_err() {
                    return;
                }
            }
        }

        let reply = if let Some(path_str) = trimmed.strip_prefix("STATUS ") {
            match strip_mount(Path::new(path_str), &mount_point) {
                Some(remote) => {
                    let detail = detail_map.safe_read().get(&remote).cloned();
                    let sm = status_map.safe_read();
                    let cm = children_map.safe_read();
                    let status = if detail.as_ref().map_or(false, |d| d.is_dir) {
                        dir_status_from_children(&sm, &remote, &cm)
                    } else {
                        sm.get(&remote).copied().unwrap_or(FileStatus::Remote).as_str()
                    };
                    drop(cm);
                    drop(sm);
                    let shared = shared_set.safe_read().contains(&remote);
                    if shared {
                        format!("{},shared", status)
                    } else {
                        status.to_string()
                    }
                }
                None => "unknown".to_string(),
            }
        } else if let Some(path_str) = trimmed.strip_prefix("DETAIL ") {
            match strip_mount(Path::new(path_str), &mount_point) {
                Some(remote) => {
                    let is_shared = shared_set.safe_read().contains(&remote);
                    let detail = detail_map.safe_read().get(&remote).cloned()
                        .unwrap_or_default();
                    let sm = status_map.safe_read();
                    let cm = children_map.safe_read();
                    let status = if detail.is_dir {
                        dir_status_from_children(&sm, &remote, &cm)
                    } else {
                        sm.get(&remote).copied().unwrap_or(FileStatus::Remote).as_str()
                    };
                    drop(cm);
                    drop(sm);
                    let sharing = if !is_shared {
                        ""
                    } else {
                        match detail.owner_id.as_deref() {
                            Some(owner) if owner == creds.username() => "Shared by you",
                            Some(_) => "Shared with you",
                            None => "Shared",
                        }
                    };
                    let perms = detail.permissions.as_deref().unwrap_or("");
                    let owner = detail.owner_display_name.as_deref().unwrap_or("");
                    log::debug!("IPC DETAIL {} → remote={} status={} shared={} perms={:?} owner={:?}",
                        path_str, remote.display(), status, is_shared, perms, owner);
                    format!("{}\t{}\t{}\t{}\t{}", status, sharing, perms, owner, detail.size)
                }
                None => {
                    log::debug!("IPC DETAIL {} → not under mount", path_str);
                    "unknown\t\t\t\t0".to_string()
                }
            }
        } else if let Some(path_str) = trimmed.strip_prefix("DETAILDIR ") {
            // Batched form of DETAIL: return one record per direct child of the
            // given directory so the Nautilus extension can warm a whole folder
            // with a single round-trip instead of one query per file.
            // Reply: records joined by 0x1e; each record is
            //   basename \t status \t sharing \t perms \t owner \t size
            match strip_mount(Path::new(path_str), &mount_point) {
                Some(remote_dir) => {
                    // Hold the status/detail/shared locks only while building the
                    // record vector; the (potentially multi-MB) join for a huge
                    // directory then runs without blocking the FUSE layer, which
                    // needs these same locks for getattr/readdir.
                    let records = {
                        let shared = shared_set.safe_read();
                        let details = detail_map.safe_read();
                        let sm = status_map.safe_read();
                        let cm = children_map.safe_read();
                        detaildir_records(&remote_dir, &details, &sm, &shared, &cm, creds.username())
                    };
                    log::debug!("IPC DETAILDIR {} → {} children", path_str, records.len());
                    records.join("\x1e")
                }
                None => {
                    log::debug!("IPC DETAILDIR {} → not under mount", path_str);
                    String::new()
                }
            }
        } else if let Some(path_str) = trimmed.strip_prefix("WEBURL ") {
            match strip_mount(Path::new(path_str), &mount_point) {
                Some(remote) => {
                    let is_dir = detail_map.safe_read().get(&remote)
                        .map_or(false, |d| d.is_dir);
                    let dir_path = if is_dir {
                        remote.to_string_lossy().to_string()
                    } else {
                        remote.parent().unwrap_or(Path::new("/"))
                            .to_string_lossy().to_string()
                    };
                    let encoded = utf8_percent_encode(&dir_path, QUERY_ENCODE).to_string();
                    match fileid_map.safe_read().get(&remote) {
                        Some(fid) => format!("{}/apps/files/files/{}?dir={}", base_url, fid, encoded),
                        None => format!("{}/apps/files/files?dir={}", base_url, encoded),
                    }
                }
                None => "error: path not under mount".to_string(),
            }
        } else if trimmed == "CHANGES" {
            // Legacy per-process cursor over the broadcast log (see change_log).
            const MAX_CHANGES: usize = 500;
            conn.ensure_attached();
            conn.v3.change_log.pump(&dirty_set, &file_change_queue);
            let paths = conn.v3.change_log.take_status(&conn.key(), MAX_CHANGES);
            if !paths.is_empty() {
                log::info!("IPC CHANGES → {} dirty paths (pid {})", paths.len(), conn.pid);
            }
            paths.iter()
                .map(|p| abs_path(&mount_point, p).to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("\t")
        } else if trimmed == "FILE_CHANGES" {
            conn.ensure_attached();
            conn.v3.change_log.pump(&dirty_set, &file_change_queue);
            conn.v3.change_log.take_file_changes(&conn.key())
                .iter()
                .map(|c| encode_file_change(c, &mount_point))
                .collect::<Vec<_>>()
                .join("\t")
        } else if trimmed == "EVENTS" || trimmed.starts_with("EVENTS ") {
            // Client-held cursor. Bare `EVENTS` just returns the current head
            // so a new client can start from "now".
            let log = &conn.v3.change_log;
            conn.mark_live_reader();
            log.pump(&dirty_set, &file_change_queue);
            match trimmed.strip_prefix("EVENTS ").map(|s| s.trim().parse::<u64>()) {
                None => log.head().to_string(),
                Some(Ok(since)) => encode_events(&log.read_since(since, EVENTS_PAGE), &mount_point),
                Some(Err(_)) => "error: EVENTS takes a sequence number".to_string(),
            }
        } else if let Some(rest) = trimmed.strip_prefix("HELLO ") {
            // `HELLO <client-id> <proto>` → `OK\t<proto>\t<pkg>\t<mount>\t<caps>`.
            let mut parts = rest.split_whitespace();
            let id = sanitize_client_id(parts.next().unwrap_or(""));
            let proto = parts.next().and_then(|p| p.parse::<u32>().ok());
            match (id.is_empty(), proto) {
                (false, Some(proto)) => {
                    if proto != PROTOCOL_VERSION {
                        log::warn!(
                            "IPC client {} speaks protocol v{}, daemon speaks v{} — update both to the same ncRS release",
                            id, proto, PROTOCOL_VERSION
                        );
                    } else {
                        log::info!("IPC client connected: {} (protocol v{}, pid {})", id, proto, conn.pid);
                    }
                    *conn.client_id.safe_lock() = id.clone();
                    conn.v3.clients.register(conn.conn_id, ClientInfo {
                        id,
                        proto,
                        pid: conn.pid,
                        connected_at: std::time::Instant::now(),
                        connected_secs: 0,
                    });
                    format!("OK\t{}\t{}\t{}\t{}", PROTOCOL_VERSION, env!("CARGO_PKG_VERSION"), mount_point.display(), CAPABILITIES)
                }
                _ => "error: usage: HELLO <client-id> <protocol>".to_string(),
            }
        } else if trimmed == "INTEGRATIONS" {
            // One record per browser profile; detection re-runs first.
            match &conn.v3.desktop {
                Some(m) => {
                    let clients = conn.v3.clients.clone();
                    let list = m.list(&|id| clients.is_connected(id));
                    serde_json::to_string(&list).unwrap_or_else(|_| "[]".to_string())
                }
                None => "[]".to_string(),
            }
        } else if let Some(rest) = trimmed.strip_prefix("INTEGRATION_SET ") {
            let mut parts = rest.split_whitespace();
            match (&conn.v3.desktop, parts.next(), parts.next().map(str::parse::<crate::desktop::store::Mode>)) {
                (None, _, _) => "error: not supported".to_string(),
                (Some(m), Some(id), Some(Ok(mode))) => match m.set(id, mode) {
                    Ok(()) => "ok".to_string(),
                    Err(e) => format!("error: {}", e),
                },
                (_, _, Some(Err(e))) => format!("error: {}", e),
                _ => "error: usage: INTEGRATION_SET <profile> on|off|auto".to_string(),
            }
        } else if trimmed == "CLIENTS" {
            serde_json::to_string(&conn.v3.clients.list()).unwrap_or_else(|_| "[]".to_string())
        } else if let Some(path_str) = trimmed.strip_prefix("KEEP ") {
            match (strip_mount(Path::new(path_str), &mount_point), &keep_cb) {
                (Some(remote), Some(cb)) => {
                    status_map.safe_write().insert(remote.clone(), FileStatus::Downloading);
                    dirty_set.safe_lock().insert(remote.clone());
                    let cb = cb.clone();
                    let sm = status_map.clone();
                    let ds = dirty_set.clone();
                    let r = remote.clone();
                    let _ = crate::bg::USER.submit(move || {
                        if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(remote))) {
                            log::error!("KEEP callback panicked: {:?}", e);
                        }
                        {
                            let mut sm_w = sm.safe_write();
                            if sm_w.get(&r).copied() == Some(FileStatus::Downloading) {
                                sm_w.insert(r.clone(), FileStatus::Kept);
                            }
                        }
                        ds.safe_lock().insert(r);
                    });
                    "ok".to_string()
                }
                (None, _) => "error: path not under mount".to_string(),
                (_, None) => "error: not supported".to_string(),
            }
        } else if let Some(path_str) = trimmed.strip_prefix("EVICT ") {
            match (strip_mount(Path::new(path_str), &mount_point), &evict_cb) {
                (Some(remote), Some(cb)) => {
                    cb(remote.clone());
                    dirty_set.safe_lock().insert(remote);
                    "ok".to_string()
                }
                (None, _) => "error: path not under mount".to_string(),
                (_, None) => "error: not supported".to_string(),
            }
        } else if let Some(path_str) = trimmed.strip_prefix("PREFETCH ") {
            match (strip_mount(Path::new(path_str), &mount_point), &prefetch_cb) {
                (Some(remote), Some(cb)) => {
                    let cb = cb.clone();
                    let _ = crate::bg::USER.submit(move || {
                        if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(remote))) {
                            log::error!("PREFETCH callback panicked: {:?}", e);
                        }
                    });
                    "ok".to_string()
                }
                (None, _) => "error: path not under mount".to_string(),
                (_, None) => "error: not supported".to_string(),
            }
        } else if let Some(path_str) = trimmed.strip_prefix("THUMBNAIL ") {
            match (strip_mount(Path::new(path_str), &mount_point), &thumbnail_cb) {
                (Some(remote), Some(cb)) => {
                    if cb(remote) { "ok".to_string() } else { "error: preview unavailable".to_string() }
                }
                (None, _) => "error: path not under mount".to_string(),
                (_, None) => "error: not supported".to_string(),
            }
        } else if let Some(term) = trimmed.strip_prefix("SEARCH ") {
            if term.is_empty() {
                "[]".to_string()
            } else {
                match crate::search::search_all(&base_url, &creds, term, false) {
                    Ok(results) => serde_json::to_string(&results).unwrap_or_else(|_| "[]".to_string()),
                    Err(e) => {
                        log::error!("IPC SEARCH failed: {}", e);
                        format!("error: {}", e)
                    }
                }
            }
        } else if trimmed == "ERRORS" {
            let errors: Vec<crate::SyncError> = error_log.safe_lock().iter().cloned().collect();
            serde_json::to_string(&errors).unwrap_or_else(|_| "[]".to_string())
        } else if trimmed == "TRANSFERS" {
            let transfers: Vec<crate::TransferProgress> = crate::transfer_snapshot(&transfer_map);
            serde_json::to_string(&transfers).unwrap_or_else(|_| "[]".to_string())
        } else if trimmed == "JOURNAL" {
            journal_entries_json(&journal.safe_lock())
        } else if trimmed == "CONFLICTS" {
            conflicts_json(&journal.safe_lock())
        } else if let Some(id) = trimmed.strip_prefix("RESOLVE_CONFLICT ") {
            match id.trim().parse::<u64>() {
                Ok(id) => {
                    journal.safe_lock().resolve_conflict(id);
                    "ok".to_string()
                }
                Err(_) => "error: bad conflict id".to_string(),
            }
        } else if trimmed == "RESOLVE_ALL_CONFLICTS" {
            journal.safe_lock().resolve_all_conflicts();
            "ok".to_string()
        } else if trimmed == "STORAGE" {
            let stats = storage_stats.safe_lock().clone();
            serde_json::to_string(&stats).unwrap_or_else(|_| "{}".to_string())
        } else if trimmed == "HEALTH" {
            crate::health_json()
        } else if trimmed == "WALKER_LIMIT" {
            crate::walker_limit_status().unwrap_or_else(|| "error: not mounted".to_string())
        } else if let Some(args) = trimmed.strip_prefix("WALKER_LIMIT ") {
            // "WALKER_LIMIT on 10" / "WALKER_LIMIT off 10"
            let mut it = args.split_whitespace();
            let enabled = match it.next() {
                Some("on") => Some(true),
                Some("off") => Some(false),
                _ => None,
            };
            let per_sec = it.next().and_then(|n| n.parse::<u32>().ok())
                .filter(|n| crate::config::WALKER_LISTINGS_PER_SEC_RANGE.contains(n));
            match (enabled, per_sec) {
                (Some(on), Some(n)) if crate::set_walker_limit(on, n) => "ok".to_string(),
                (Some(_), Some(_)) => "error: not mounted".to_string(),
                _ => "error: usage WALKER_LIMIT on|off <listings/s 1-1000>".to_string(),
            }
        } else if trimmed == "STATE" {
            let active = !transfer_map.safe_lock().is_empty();
            state_word(&paused, &offline, active).to_string()
        } else if trimmed == "PAUSE" {
            paused.store(true, Ordering::Relaxed);
            log::info!("sync paused via IPC");
            "ok".to_string()
        } else if trimmed == "RESUME" {
            paused.store(false, Ordering::Relaxed);
            log::info!("sync resumed via IPC");
            "ok".to_string()
        } else if trimmed == "PASSTHROUGH_ON" {
            passthrough_enabled.store(true, Ordering::Relaxed);
            log::info!("FUSE passthrough enabled via IPC");
            "ok".to_string()
        } else if trimmed == "PASSTHROUGH_OFF" {
            passthrough_enabled.store(false, Ordering::Relaxed);
            log::info!("FUSE passthrough disabled via IPC");
            "ok".to_string()
        } else if trimmed == "PASSTHROUGH_STATUS" {
            // "on"/"off" reflects the live toggle; the second field is whether a
            // passthrough attempt has actually succeeded this session (false
            // stays false once CAP_SYS_ADMIN or kernel support is found missing).
            format!(
                "{}:{}",
                if passthrough_enabled.load(Ordering::Relaxed) { "on" } else { "off" },
                if passthrough_capable.load(Ordering::Relaxed) { "capable" } else { "unavailable" },
            )
        } else if trimmed == "PURGE_CACHE" {
            match &purge_cb {
                Some(cb) => match cb() {
                    Ok(n) => {
                        log::info!("cache purged via IPC — {} file(s) cleared", n);
                        format!("ok:{}", n)
                    }
                    Err(e) => {
                        log::error!("PURGE_CACHE failed: {}", e);
                        format!("error: {}", e)
                    }
                },
                None => "error: not supported".to_string(),
            }
        } else if let Some(ver_str) = trimmed.strip_prefix("VERSION ") {
            // A v2 extension announces itself on connect and then polls: start
            // its change cursor here so nothing between connect and first poll is lost.
            conn.ensure_attached();
            // The extension announces its protocol version on connect. Print it
            // and warn if it does not match the daemon so a half-updated install
            // is visible in the logs. Reply with our own protocol + package
            // version so the extension can warn on its side too.
            match ver_str.trim().parse::<u32>() {
                Ok(v) if v == PROTOCOL_VERSION => {
                    log::info!("shell extension connected (protocol v{})", v);
                }
                Ok(v) => {
                    log::warn!(
                        "shell extension protocol v{} does not match daemon protocol v{} — \
                         update ncRS so the daemon and extension are the same release",
                        v, PROTOCOL_VERSION
                    );
                }
                Err(_) => {
                    log::warn!("shell extension sent malformed VERSION: {:?}", ver_str.trim());
                }
            }
            format!("{}\t{}", PROTOCOL_VERSION, env!("CARGO_PKG_VERSION"))
        } else if let Some(msg) = trimmed.strip_prefix("LOG ") {
            let who = conn.client_id.safe_lock().clone();
            log::info!("[{}] {}", if who.is_empty() { "extension" } else { who.as_str() }, msg);
            "ok".to_string()
        } else {
            "unknown".to_string()
        };

        if writeln!(write_half, "{}", reply).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    fn detail(perms: &str, owner_id: &str, owner_name: &str, size: u64, is_dir: bool) -> FileDetail {
        FileDetail {
            permissions: Some(perms.to_string()),
            owner_id: Some(owner_id.to_string()),
            owner_display_name: Some(owner_name.to_string()),
            size,
            is_dir,
        }
    }

    #[test]
    fn status_words_match_the_published_vocabulary() {
        // Every adapter maps the words in status-vocabulary.txt; a status the
        // daemon emits that is not listed there would render as nothing.
        let vocab: HashSet<&str> = include_str!("../../shell_integration/file-managers/status-vocabulary.txt")
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        let emitted = [
            FileStatus::Kept, FileStatus::Cached, FileStatus::Synced, FileStatus::Remote,
            FileStatus::Downloading, FileStatus::Uploading, FileStatus::PendingSync, FileStatus::Unknown,
        ];
        let mut words: HashSet<&str> = emitted.iter().map(|s| s.as_str()).collect();
        words.insert("partial"); // derived for directories by dir_status_from_children
        assert_eq!(words, vocab);
    }

    // ── Protocol v3 wire format ──────────────────────────────────────────────

    #[test]
    fn events_reply_encodes_every_record_kind() {
        use crate::change_log::{ChangeLog, ChangeRecord};
        let log = ChangeLog::new(16);
        log.add_live_reader();
        log.append([
            ChangeRecord::Status(PathBuf::from("/a/b.txt")),
            ChangeRecord::File(FileChange { kind: FileChangeKind::DirAdded, path: PathBuf::from("/d") }),
            ChangeRecord::File(FileChange {
                kind: FileChangeKind::Renamed { from: PathBuf::from("/old") },
                path: PathBuf::from("/new"),
            }),
        ]);
        let reply = encode_events(&log.read_since(0, 100), Path::new("/home/u/NC"));
        assert_eq!(
            reply,
            "3\tS:/home/u/NC/a/b.txt\tDA:/home/u/NC/d\tR:/home/u/NC/old\x1e/home/u/NC/new"
        );
    }

    #[test]
    fn events_reply_signals_resync() {
        let res = crate::change_log::ReadResult { next: 42, records: vec![], resync: true };
        assert_eq!(encode_events(&res, Path::new("/m")), "42\tRESYNC");
    }

    #[test]
    fn client_ids_are_sanitized() {
        assert_eq!(sanitize_client_id("dolphin-kf6/0.1"), "dolphin-kf6/0.1");
        assert_eq!(sanitize_client_id("evil\tid\x1e"), "evilid");
        assert_eq!(sanitize_client_id(&"x".repeat(200)).len(), 64);
    }

    #[test]
    fn client_registry_tracks_connections() {
        let reg = ClientRegistry::default();
        let c = reg.new_conn_id();
        reg.register(c, ClientInfo {
            id: "nautilus".into(),
            proto: PROTOCOL_VERSION,
            pid: 1,
            connected_at: std::time::Instant::now(),
            connected_secs: 0,
        });
        assert!(reg.is_connected("nautilus"));
        assert!(!reg.is_connected("dolphin"));
        let json = serde_json::to_string(&reg.list()).unwrap();
        assert!(json.contains("\"id\":\"nautilus\""));
        reg.remove(c);
        assert!(!reg.is_connected("nautilus"));
    }

    #[test]
    fn peer_pid_is_our_own_for_a_socketpair() {
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        assert_eq!(peer_pid(&a), std::process::id());
    }

    // ── Daemon state word ────────────────────────────────────────────────────

    #[test]
    fn unreachable_server_reports_offline_not_syncing() {
        // The bug: a server that is fully down was reported as a working sync
        // path, so the GUI showed a partial-outage status while nothing worked.
        let paused = AtomicBool::new(false);
        let offline = crate::OfflineStatus::offline_for(Duration::from_secs(60));
        assert_eq!(state_word(&paused, &offline, false), "offline");
        // Queued transfers do not make it "syncing": they cannot progress.
        assert_eq!(state_word(&paused, &offline, true), "offline");
    }

    #[test]
    fn a_blip_does_not_flash_a_failure() {
        // One failed request flips the offline flag eagerly and the connectivity
        // monitor clears it within a 5s cycle. Reporting that instantly would
        // strobe the tray red on every hiccup, so the word only changes once the
        // outage outlives the grace window.
        let paused = AtomicBool::new(false);
        let blip = crate::OfflineStatus::offline_for(Duration::from_secs(1));
        assert_eq!(state_word(&paused, &blip, false), "idle");
        assert_eq!(state_word(&paused, &blip, true), "syncing");
    }

    #[test]
    fn pause_outranks_offline() {
        // Pause is the user's own decision and holds whether or not the server
        // is up, so it must survive an outage rather than be relabelled.
        let paused = AtomicBool::new(true);
        let offline = crate::OfflineStatus::offline_for(Duration::from_secs(60));
        assert_eq!(state_word(&paused, &offline, false), "paused");
    }

    #[test]
    fn reachable_server_keeps_the_old_derivation() {
        let paused = AtomicBool::new(false);
        let online = crate::OfflineStatus::new();
        assert_eq!(state_word(&paused, &online, true), "syncing");
        assert_eq!(state_word(&paused, &online, false), "idle");
    }

    fn build_children(details: &HashMap<PathBuf, FileDetail>) -> HashMap<PathBuf, HashSet<PathBuf>> {
        let mut cm: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
        for path in details.keys() {
            if let Some(parent) = path.parent() {
                cm.entry(parent.to_path_buf())
                    .or_insert_with(HashSet::new)
                    .insert(path.clone());
            }
        }
        cm
    }

    #[test]
    fn detaildir_lists_only_direct_children_with_fields() {
        let mut details = HashMap::new();
        details.insert(PathBuf::from("/dir/a.txt"), detail("RGDNVW", "alice", "Alice", 100, false));
        details.insert(PathBuf::from("/dir/sub"), detail("RGDNVCK", "alice", "Alice", 4096, true));
        details.insert(PathBuf::from("/dir/sub/deep.txt"), detail("RG", "bob", "Bob", 5, false)); // grandchild, excluded
        details.insert(PathBuf::from("/other/x.txt"), detail("RG", "bob", "Bob", 7, false)); // other dir, excluded

        let mut sm = HashMap::new();
        sm.insert(PathBuf::from("/dir/a.txt"), FileStatus::Kept);

        let mut shared = HashSet::new();
        shared.insert(PathBuf::from("/dir/a.txt"));

        let children = build_children(&details);
        let recs = detaildir_records(Path::new("/dir"), &details, &sm, &shared, &children, "alice");
        assert_eq!(recs.len(), 2, "only direct children of /dir");

        let by_name: HashMap<&str, &String> =
            recs.iter().map(|r| (r.split('\t').next().unwrap(), r)).collect();

        // a.txt: kept, shared by you (owner == username), perms mapped verbatim, size 100
        assert_eq!(by_name["a.txt"], &"a.txt\tkept\tShared by you\tRGDNVW\tAlice\t100".to_string());
        // sub: directory status derived from children (none in sm → remote), not shared
        let sub = by_name["sub"];
        assert!(sub.starts_with("sub\t"), "record starts with basename");
        assert!(sub.contains("\t\t"), "empty sharing field when not shared");
        assert!(sub.ends_with("\t4096"), "size preserved");
    }

    #[test]
    fn detaildir_empty_dir_yields_no_records() {
        let details = HashMap::new();
        let sm = HashMap::new();
        let shared = HashSet::new();
        let children = HashMap::new();
        let recs = detaildir_records(Path::new("/dir"), &details, &sm, &shared, &children, "alice");
        assert!(recs.is_empty());
    }

    #[test]
    fn detaildir_dir_status_matches_per_child_reference() {
        // /d has two subdirs; each subdir's status must equal what the
        // per-subdirectory reference (dir_status_from_children) would compute.
        let mut details = HashMap::new();
        details.insert(PathBuf::from("/d/allkept"), detail("", "a", "A", 0, true));
        details.insert(PathBuf::from("/d/mixed"), detail("", "a", "A", 0, true));
        details.insert(PathBuf::from("/d/uploading"), detail("", "a", "A", 0, true));
        details.insert(PathBuf::from("/d/f.txt"), detail("", "a", "A", 3, false));

        let mut sm = HashMap::new();
        // allkept: every child pinned → "kept"
        sm.insert(PathBuf::from("/d/allkept/a"), FileStatus::Kept);
        sm.insert(PathBuf::from("/d/allkept/b"), FileStatus::Synced);
        // mixed: one cached, one remote → "partial"
        sm.insert(PathBuf::from("/d/mixed/a"), FileStatus::Cached);
        sm.insert(PathBuf::from("/d/mixed/b"), FileStatus::Remote);
        // uploading: one uploading child → "uploading"
        sm.insert(PathBuf::from("/d/uploading/a"), FileStatus::Uploading);
        sm.insert(PathBuf::from("/d/uploading/b"), FileStatus::Kept);
        // the flat file
        sm.insert(PathBuf::from("/d/f.txt"), FileStatus::Kept);

        // Build children map including subdir grandchildren for status aggregation
        let mut children: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
        children.entry(PathBuf::from("/d")).or_insert_with(HashSet::new).insert(PathBuf::from("/d/allkept"));
        children.entry(PathBuf::from("/d")).or_insert_with(HashSet::new).insert(PathBuf::from("/d/mixed"));
        children.entry(PathBuf::from("/d")).or_insert_with(HashSet::new).insert(PathBuf::from("/d/uploading"));
        children.entry(PathBuf::from("/d")).or_insert_with(HashSet::new).insert(PathBuf::from("/d/f.txt"));
        children.entry(PathBuf::from("/d/allkept")).or_insert_with(HashSet::new).insert(PathBuf::from("/d/allkept/a"));
        children.entry(PathBuf::from("/d/allkept")).or_insert_with(HashSet::new).insert(PathBuf::from("/d/allkept/b"));
        children.entry(PathBuf::from("/d/mixed")).or_insert_with(HashSet::new).insert(PathBuf::from("/d/mixed/a"));
        children.entry(PathBuf::from("/d/mixed")).or_insert_with(HashSet::new).insert(PathBuf::from("/d/mixed/b"));
        children.entry(PathBuf::from("/d/uploading")).or_insert_with(HashSet::new).insert(PathBuf::from("/d/uploading/a"));
        children.entry(PathBuf::from("/d/uploading")).or_insert_with(HashSet::new).insert(PathBuf::from("/d/uploading/b"));

        let shared = HashSet::new();
        let recs = detaildir_records(Path::new("/d"), &details, &sm, &shared, &children, "a");
        let status_of = |name: &str| -> String {
            recs.iter()
                .find(|r| r.starts_with(&format!("{}\t", name)))
                .unwrap()
                .split('\t')
                .nth(1)
                .unwrap()
                .to_string()
        };
        assert_eq!(status_of("allkept"), "kept");
        assert_eq!(status_of("mixed"), "partial");
        assert_eq!(status_of("uploading"), "uploading");
        assert_eq!(status_of("f.txt"), "kept");

        // Cross-check every subdir against the per-child reference implementation.
        for sub in ["/d/allkept", "/d/mixed", "/d/uploading"] {
            let name = sub.rsplit('/').next().unwrap();
            assert_eq!(
                status_of(name),
                dir_status_from_children(&sm, Path::new(sub), &children),
                "batched status for {} must match dir_status_from_children",
                sub
            );
        }
    }

    #[test]
    fn detaildir_shared_with_you_when_owner_differs() {
        let mut details = HashMap::new();
        details.insert(PathBuf::from("/dir/f"), detail("RG", "bob", "Bob", 1, false));
        let sm = HashMap::new();
        let mut shared = HashSet::new();
        shared.insert(PathBuf::from("/dir/f"));
        let children = build_children(&details);
        let recs = detaildir_records(Path::new("/dir"), &details, &sm, &shared, &children, "alice");
        assert_eq!(recs.len(), 1);
        assert!(recs[0].contains("\tShared with you\t"));
    }
}

#[cfg(test)]
mod socket_permission_tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn socket_dir_falls_back_to_a_private_per_uid_dir_not_shared_tmp() {
        use std::os::unix::fs::PermissionsExt;
        // Emulate a daemon started without a runtime dir (cron, a systemd unit
        // without PAM). The socket must not land in the shared /tmp root.
        let uid = unsafe { libc::getuid() };
        let expected = std::env::temp_dir().join(format!("ncrs-{}", uid));
        std::fs::remove_dir_all(&expected).ok();

        assert!(create_private_dir(&expected).is_ok());
        let mode = std::fs::metadata(&expected).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "fallback socket dir must be owner-only");

        // Tightens a pre-existing permissive directory rather than trusting it.
        std::fs::set_permissions(&expected, std::fs::Permissions::from_mode(0o777)).unwrap();
        create_private_dir(&expected).unwrap();
        let mode = std::fs::metadata(&expected).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);

        assert_ne!(expected, std::env::temp_dir(), "must not be the shared tmp root");
        std::fs::remove_dir_all(&expected).ok();
    }

    #[test]
    #[cfg(unix)]
    fn create_private_dir_refuses_a_non_directory_path() {
        let p = std::env::temp_dir().join("ncrs-sockdir-is-a-file");
        std::fs::write(&p, b"x").unwrap();
        assert!(create_private_dir(&p).is_err(), "a planted file must not be accepted");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn socket_path_is_inside_the_socket_dir() {
        assert_eq!(socket_path().parent().unwrap(), socket_dir().as_path());
        assert_eq!(socket_path().file_name().unwrap(), "ncrs.sock");
    }
}
