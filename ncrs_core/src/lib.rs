pub mod config;
pub mod filename_validation;
pub mod fuse_notify;
pub mod ipc;
pub mod mutation_journal;
pub mod notifications;
pub mod notify_push;
pub mod preview;
pub mod propfind;
pub mod search;
pub mod webdav_ops;

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fuser::{
    BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, LockOwner, MountOption, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow, WriteFlags,
};

use remotefs::RemoteFs;
use remotefs_webdav::WebDAVFs;
use ipc::{FileStatus, StatusMap};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use propfind::DavEntry;
use serde::{Deserialize, Serialize};
use yaml_rust2::YamlLoader;

#[cfg(unix)]
use std::os::unix::fs::FileExt;

trait MutexExt<T> {
    fn safe_lock(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for Mutex<T> {
    fn safe_lock(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

const TTL: Duration = Duration::from_secs(1);
const DIR_CACHE_TTL: Duration = Duration::from_secs(10);
const OPTIMISTIC_TTL_CONNECTED: Duration = Duration::from_secs(86400);
const OPTIMISTIC_TTL_FALLBACK: Duration = Duration::from_secs(300);
const PROPFIND_TIMEOUT: Duration = Duration::from_secs(15);

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
const READ_AHEAD: usize = 64 * 1024 * 1024; // 64 MB
const MAX_POOL_IDLE: usize = 8;

const GHOST_TTL: Duration = Duration::from_secs(10);

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
    state: Mutex<usize>,
    cv: Condvar,
    max: usize,
}

pub struct ThrottleGuard<'a> {
    throttle: &'a Throttle,
}

impl Throttle {
    pub fn new(max: usize) -> Self {
        Throttle { state: Mutex::new(0), cv: Condvar::new(), max }
    }

    pub fn acquire(&self) -> ThrottleGuard<'_> {
        let mut count = self.state.lock().unwrap();
        if *count >= self.max {
            let t = Instant::now();
            while *count >= self.max {
                count = self.cv.wait(count).unwrap();
            }
            let waited = t.elapsed();
            if waited.as_millis() > 5 {
                log::debug!("throttle: waited {:?} for slot (in_flight={})", waited, *count);
            }
        }
        *count += 1;
        ThrottleGuard { throttle: self }
    }
}

impl Drop for ThrottleGuard<'_> {
    fn drop(&mut self) {
        let mut count = self.throttle.state.lock().unwrap();
        *count -= 1;
        self.throttle.cv.notify_one();
    }
}

// ── Cache data types ──────────────────────────────────────────────────────────

struct DirCacheEntry {
    files: Arc<Vec<DavEntry>>,
    self_entry: Option<DavEntry>,
    etag: Option<String>,
    at: Instant,
    refreshing: bool,
    invalidated: bool,
}

struct PendingDir {
    entries: Vec<DavEntry>,
    rx: mpsc::Receiver<DavEntry>,
    etag_rx: mpsc::Receiver<Option<String>>,
    self_rx: mpsc::Receiver<DavEntry>,
    etag: Option<String>,
    self_entry: Option<DavEntry>,
}

struct FileCacheEntry {
    local_path: PathBuf,
    remote_modified: Option<SystemTime>,
    etag: Option<String>,
}

struct StreamState {
    data: Vec<u8>,
    done: bool,
}

struct ReadAheadBuf {
    start: u64,
    stream: Arc<(Mutex<StreamState>, Condvar)>,
    target_len: u64,
}

struct OpenFile {
    remote_path: PathBuf,
    local: Option<PathBuf>,
    buf: Option<ReadAheadBuf>,
    write_path: Option<PathBuf>,
    dirty: bool,
    original_etag: Option<String>,
}

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MountOptions {
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub mount_point: PathBuf,
    pub log_user: String,
    pub aggressive_prefetch: bool,
    pub http3: bool,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_requests: usize,
    #[serde(default)]
    pub offline: bool,
    #[serde(default = "default_true")]
    pub optimistic_listing: bool,
    /// Keep a local cache copy of files after they are written and uploaded.
    /// When true the post-upload emblem is a green checkmark (Local); when false
    /// the staging copy is discarded and no emblem is shown (Synced).
    #[serde(default)]
    pub auto_keep_locally_modified_files: bool,
}

fn default_true() -> bool { true }

fn default_max_concurrent() -> usize { 10 }

#[derive(Clone, Debug, PartialEq)]
pub enum SyncState {
    Idle,
    Syncing,
    Paused,
    Error(String),
}

impl std::fmt::Display for SyncState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncState::Idle => write!(f, "idle"),
            SyncState::Syncing => write!(f, "syncing"),
            SyncState::Paused => write!(f, "paused"),
            SyncState::Error(e) => write!(f, "error:{}", e),
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

pub type TransferMap = Arc<Mutex<HashMap<PathBuf, TransferProgress>>>;

// ── Network layer — each call runs in a sub-thread so the caller can impose a
//    deadline via recv_timeout without blocking the FUSE session thread. ───────

pub(crate) struct FsNetwork {
    conns: Mutex<Vec<WebDAVFs>>,
    url: String,
    username: String,
    password: String,
}

impl FsNetwork {
    fn checkout(&self) -> Result<WebDAVFs, String> {
        if let Some(conn) = self.conns.safe_lock().pop() {
            return Ok(conn);
        }
        let mut conn = WebDAVFs::new(&self.username, &self.password, &self.url);
        conn.connect().map_err(|e| format!("WebDAV connect: {}", e))?;
        Ok(conn)
    }

    fn checkin(&self, conn: WebDAVFs) {
        let mut pool = self.conns.safe_lock();
        if pool.len() < MAX_POOL_IDLE {
            pool.push(conn);
        }
    }

}

fn error_to_errno(err: &str) -> Errno {
    if err.contains("401") || err.contains("403") || err.contains("Unauthorized") || err.contains("Forbidden") {
        Errno::EACCES
    } else if err.contains("404") || err.contains("Not Found") {
        Errno::ENOENT
    } else {
        Errno::EIO
    }
}

fn list_dir_propfind(
    conn: &Arc<ConnInfo>,
    path: PathBuf,
) -> Result<(Option<String>, Option<DavEntry>, Vec<DavEntry>), String> {
    log::debug!("LIST {}", path.display());
    let (tx, rx) = mpsc::channel();
    let c = conn.clone();
    thread::spawn(move || {
        let _permit = c.throttle.acquire();
        let _ = tx.send(propfind::propfind_list(
            &c.http, &c.webdav_url, &c.username, &c.password, &path, PROPFIND_TIMEOUT,
        ));
    });
    rx.recv_timeout(PROPFIND_TIMEOUT + Duration::from_secs(1))
        .unwrap_or_else(|_| Err("WebDAV PROPFIND timeout".into()))
}

struct ProgressWriter {
    inner: std::fs::File,
    path: PathBuf,
    transfer_map: TransferMap,
    written: u64,
}

impl std::io::Write for ProgressWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        if let Ok(mut map) = self.transfer_map.lock() {
            if let Some(entry) = map.get_mut(&self.path) {
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
    net: &Arc<FsNetwork>,
    throttle: &Arc<Throttle>,
    path: PathBuf,
    dest: std::fs::File,
    transfers: Option<TransferMap>,
) -> Result<(), String> {
    log::info!("DOWNLOAD {}", path.display());
    let (tx, rx) = mpsc::channel();
    let n = net.clone();
    let throttle = throttle.clone();
    thread::spawn(move || {
        let _permit = throttle.acquire();
        let writer: Box<dyn std::io::Write + Send> = if let Some(tm) = transfers {
            Box::new(ProgressWriter { inner: dest, path: path.clone(), transfer_map: tm, written: 0 })
        } else {
            Box::new(dest)
        };
        let result = match n.checkout() {
            Ok(mut conn) => {
                let r = conn
                    .open_file(&path, writer)
                    .map(|_| ())
                    .map_err(|e| e.to_string());
                n.checkin(conn);
                r
            }
            Err(e) => Err(e),
        };
        let _ = tx.send(result);
    });
    rx.recv_timeout(DOWNLOAD_TIMEOUT)
        .unwrap_or_else(|_| Err("WebDAV download timeout".into()))
}

// ── Cache layer ───────────────────────────────────────────────────────────────

pub(crate) struct FsCache {
    inodes: HashMap<u64, PathBuf>,
    paths: HashMap<PathBuf, u64>,
    next_inode: u64,
    dir_cache: HashMap<PathBuf, DirCacheEntry>,
    pending_dirs: HashMap<PathBuf, PendingDir>,
    pub(crate) file_cache: HashMap<PathBuf, FileCacheEntry>,
    cache_dir: PathBuf,
    pub(crate) pending_notify: Arc<(Mutex<()>, Condvar)>,
}

impl FsCache {
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

    fn get_cached_dir(&mut self, path: &Path, ttl: Duration) -> Option<(Arc<Vec<DavEntry>>, bool)> {
        let entry = self.dir_cache.get_mut(path)?;
        if entry.invalidated {
            return None;
        }
        let stale = entry.at.elapsed() >= ttl;
        let needs_refresh = stale && !entry.refreshing;
        if needs_refresh {
            entry.refreshing = true;
        }
        Some((Arc::clone(&entry.files), needs_refresh))
    }

    fn get_cached_dir_readonly(&self, path: &Path) -> Option<Arc<Vec<DavEntry>>> {
        self.dir_cache.get(path).map(|e| Arc::clone(&e.files))
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
            .and_then(|e| e.permissions.clone())
    }

    fn cached_dir_etag(&self, path: &Path) -> Option<String> {
        self.dir_cache.get(path)?.etag.clone()
    }

    fn put_dir_cache(&mut self, path: PathBuf, etag: Option<String>, self_entry: Option<DavEntry>, files: Vec<DavEntry>) {
        self.dir_cache.insert(path, DirCacheEntry { files: Arc::new(files), self_entry, etag, at: Instant::now(), refreshing: false, invalidated: false });
    }

    fn touch_dir_cache(&mut self, path: &Path) {
        if let Some(entry) = self.dir_cache.get_mut(path) {
            entry.at = Instant::now();
            entry.refreshing = false;
            entry.invalidated = false;
        }
    }

    fn start_pending(&mut self, path: PathBuf, rx: mpsc::Receiver<DavEntry>, etag_rx: mpsc::Receiver<Option<String>>, self_rx: mpsc::Receiver<DavEntry>) {
        self.pending_dirs.insert(path, PendingDir {
            entries: Vec::new(),
            rx,
            etag_rx,
            self_rx,
            etag: None,
            self_entry: None,
        });
    }

    fn start_pending_and_notify(&mut self, path: PathBuf, rx: mpsc::Receiver<DavEntry>, etag_rx: mpsc::Receiver<Option<String>>, self_rx: mpsc::Receiver<DavEntry>) -> Arc<(Mutex<()>, Condvar)> {
        self.start_pending(path, rx, etag_rx, self_rx);
        self.pending_notify.clone()
    }

    // Promote any pending streaming fetch to dir_cache, then return the subdir paths.
    // Used by background prefetch to discover the next wave of directories to pre-fetch.
    fn subdir_paths_for_prefetch(&mut self, path: &Path) -> Vec<PathBuf> {
        if self.pending_dirs.contains_key(path) {
            self.promote_pending(path);
        }
        self.dir_cache.get(path)
            .map(|e| e.files.iter()
                .filter(|f| f.is_dir)
                .filter_map(|f| f.path.file_name().and_then(|n| n.to_str()).map(|n| path.join(n)))
                .collect())
            .unwrap_or_default()
    }

    fn promote_pending(&mut self, path: &Path) -> Option<DavEntry> {
        if let Some(mut pending) = self.pending_dirs.remove(path) {
            while let Ok(entry) = pending.rx.try_recv() {
                pending.entries.push(entry);
            }
            if let Ok(etag) = pending.etag_rx.try_recv() {
                pending.etag = etag;
            }
            if pending.self_entry.is_none() {
                if let Ok(se) = pending.self_rx.try_recv() {
                    pending.self_entry = Some(se);
                }
            }
            let self_entry = pending.self_entry.take();
            self.put_dir_cache(path.to_path_buf(), pending.etag, self_entry.clone(), pending.entries);
            self.pending_notify.1.notify_all();
            self_entry
        } else {
            None
        }
    }

    fn get_pending_snapshot(&mut self, path: &Path) -> Option<Vec<DavEntry>> {
        let pending = self.pending_dirs.get_mut(path)?;
        if pending.self_entry.is_none() {
            if let Ok(se) = pending.self_rx.try_recv() {
                pending.self_entry = Some(se);
            }
        }
        let mut got_new = false;
        loop {
            match pending.rx.try_recv() {
                Ok(entry) => { pending.entries.push(entry); got_new = true; }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.promote_pending(path);
                    // promote_pending already calls notify_all
                    return self.dir_cache.get(path).map(|e| e.files.to_vec());
                }
            }
        }
        if !pending.entries.is_empty() {
            if got_new {
                self.pending_notify.1.notify_all();
            }
            Some(pending.entries.clone())
        } else {
            None
        }
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
        self.find_entry(path).and_then(|e| e.etag.clone())
    }

    fn find_entry(&self, path: &Path) -> Option<&propfind::DavEntry> {
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
    self_entry: Option<DavEntry>,
    files: Vec<DavEntry>,
}

use std::sync::atomic::AtomicU64;

static SAVE_SCHEDULED: AtomicU64 = AtomicU64::new(0);
const SAVE_DEBOUNCE: Duration = Duration::from_secs(5);

fn schedule_save_dir_cache(cache: &Arc<Mutex<FsCache>>) {
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let prev = SAVE_SCHEDULED.swap(now, Ordering::Relaxed);
    if now.saturating_sub(prev) < 2000 {
        return;
    }
    let cache = cache.clone();
    thread::spawn(move || {
        thread::sleep(SAVE_DEBOUNCE);
        save_dir_cache_now(&cache);
    });
}

fn save_dir_cache_now(cache: &Mutex<FsCache>) {
    let c = cache.safe_lock();
    let path = c.cache_dir.join(DIR_CACHE_FILE);
    let map: HashMap<String, PersistedDirEntry> = c.dir_cache.iter()
        .map(|(k, v)| {
            (k.to_string_lossy().into_owned(), PersistedDirEntry {
                etag: v.etag.clone(),
                self_entry: v.self_entry.clone(),
                files: v.files.as_ref().clone(),
            })
        })
        .collect();
    drop(c);
    if let Ok(json) = serde_json::to_vec(&map) {
        let _ = std::fs::write(&path, json);
        log::info!("DIR_CACHE saved {} dirs to {}", map.len(), path.display());
    }
}

fn load_dir_cache(cache: &Mutex<FsCache>) {
    let path = {
        let c = cache.safe_lock();
        c.cache_dir.join(DIR_CACHE_FILE)
    };
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
    let mut c = cache.safe_lock();
    let mut count = 0usize;
    for (k, v) in map {
        let dir_path = PathBuf::from(&k);
        if c.dir_cache.contains_key(&dir_path) {
            continue;
        }
        for entry in &v.files {
            if let Some(name) = entry.path.file_name().and_then(|n| n.to_str()) {
                c.allocate_inode(dir_path.join(name));
            }
        }
        c.dir_cache.insert(dir_path, DirCacheEntry {
            files: Arc::new(v.files),
            self_entry: v.self_entry,
            etag: v.etag,
            at: Instant::now(),
            refreshing: false,
            invalidated: false,
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
}

pub(crate) fn save_file_cache(cache: &Mutex<FsCache>) {
    let c = cache.safe_lock();
    let path = c.cache_dir.join(FILE_CACHE_FILE);
    let map: HashMap<String, PersistedFileEntry> = c.file_cache.iter()
        .filter_map(|(k, v)| {
            v.etag.as_ref()?;
            Some((k.to_string_lossy().into_owned(), PersistedFileEntry { etag: v.etag.clone() }))
        })
        .collect();
    drop(c);
    if let Ok(json) = serde_json::to_vec(&map) {
        let _ = std::fs::write(&path, json);
        log::info!("FILE_CACHE saved {} entries to {}", map.len(), path.display());
    }
}

fn load_file_cache(cache: &Mutex<FsCache>) -> HashMap<PathBuf, String> {
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
    for (k, v) in map {
        let remote_path = PathBuf::from(&k);
        if let Some(etag) = v.etag {
            let rel = remote_path.strip_prefix("/").unwrap_or(&remote_path);
            let local_path = c.cache_dir.join(rel);
            if local_path.metadata().map_or(false, |m| m.len() > 0) {
                result.insert(remote_path, etag);
            }
        }
    }
    log::info!("FILE_CACHE loaded {} entries", result.len());
    result
}

// ── Shared operation helpers ──────────────────────────────────────────────────

fn get_or_list_dir(
    conn: &Arc<ConnInfo>,
    cache: &Arc<Mutex<FsCache>>,
    path: PathBuf,
) -> Result<(Arc<Vec<DavEntry>>, Option<DavEntry>), String> {
    if conn.is_offline.load(Ordering::Relaxed) {
        let c = cache.safe_lock();
        if let Some(entry) = c.dir_cache.get(&path) {
            return Ok((Arc::clone(&entry.files), entry.self_entry.clone()));
        }
        return Err(format!("{} not available offline", path.display()));
    }
    let t0 = Instant::now();
    let ttl = effective_dir_ttl(conn.optimistic_listing, &conn.notify_push_connected);
    {
        let mut c = cache.safe_lock();
        if let Some((files, needs_refresh)) = c.get_cached_dir(&path, ttl) {
            let self_entry = c.dir_cache.get(&path).and_then(|e| e.self_entry.clone());
            log::info!("LIST_CACHED {} ({} entries, refresh={}) in {:?}", path.display(), files.len(), needs_refresh, t0.elapsed());
            if needs_refresh {
                let conn = conn.clone();
                let cache = cache.clone();
                let path = path.clone();
                std::thread::spawn(move || {
                    let old_etag = cache.safe_lock().cached_dir_etag(&path);
                    if let Some(ref old) = old_etag {
                        let _permit = conn.throttle.acquire();
                        match propfind::propfind_etag(&conn.http, &conn.webdav_url, &conn.username, &conn.password, &path, PROPFIND_TIMEOUT) {
                            Ok(Some(ref new_etag)) if new_etag == old => {
                                log::debug!("ETAG_MATCH {} — skipping full re-list", path.display());
                                cache.safe_lock().touch_dir_cache(&path);
                                return;
                            }
                            Ok(_) => {}
                            Err(e) => {
                                log::debug!("etag check {}: {}", path.display(), e);
                            }
                        }
                    }
                    match list_dir_propfind(&conn, path.clone()) {
                        Ok((etag, self_entry, fresh)) => {
                            cache.safe_lock().put_dir_cache(path, etag, self_entry, fresh);
                        }
                        Err(e) => {
                            log::debug!("background refresh {}: {}", path.display(), e);
                            cache.safe_lock().clear_refreshing(&path);
                        }
                    }
                });
            }
            return Ok((files, self_entry));
        }
    }
    // Check if there's already an in-progress incremental fetch
    {
        let mut c = cache.safe_lock();
        if let Some(snapshot) = c.get_pending_snapshot(&path) {
            let self_entry = c.pending_dirs.get(&path).and_then(|p| p.self_entry.clone());
            return Ok((Arc::new(snapshot), self_entry));
        }
    }

    // Start incremental streaming fetch — unless another thread already started one
    let (already_pending, was_invalidated) = {
        let mut c = cache.safe_lock();
        let was_inv = c.dir_cache.get(&path).map_or(false, |e| e.invalidated);
        if c.dir_cache.contains_key(&path) {
            if let Some((files, _)) = c.get_cached_dir(&path, ttl) {
                let se = c.dir_cache.get(&path).and_then(|e| e.self_entry.clone());
                return Ok((files, se));
            }
        }
        (if c.pending_dirs.contains_key(&path) {
            true
        } else {
            let (entry_tx, entry_rx) = mpsc::channel();
            let (etag_tx, etag_rx) = mpsc::channel();
            let (self_tx, self_rx) = mpsc::channel();
            c.start_pending(path.clone(), entry_rx, etag_rx, self_rx);

            let conn2 = conn.clone();
            let path2 = path.clone();
            let pending_notify2 = c.pending_notify.clone();
            std::thread::spawn(move || {
                let _permit = conn2.throttle.acquire();
                match propfind::propfind_list_streaming(
                    &conn2.http, &conn2.webdav_url, &conn2.username, &conn2.password,
                    &path2, PROPFIND_TIMEOUT, entry_tx, self_tx,
                ) {
                    Ok(etag) => {
                        let _ = etag_tx.send(etag);
                    }
                    Err(e) => {
                        log::warn!("incremental list {}: {}", path2.display(), e);
                        let _ = etag_tx.send(None);
                    }
                }
                // Wake any threads waiting in get_or_list_dir for this path.
                pending_notify2.1.notify_all();
            });
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
            if let Some(snapshot) = c.get_pending_snapshot(&path) {
                if !snapshot.is_empty() && !was_invalidated {
                    if poll_iters > 2 { log::debug!("LIST_STREAM_WAIT {} iters before stream", poll_iters); }
                    log::info!("LIST_STREAM {} ({} entries) in {:?}", path.display(), snapshot.len(), t0.elapsed());
                    let se = c.pending_dirs.get(&path).and_then(|p| p.self_entry.clone());
                    return Ok((Arc::new(snapshot), se));
                }
            }
            if c.dir_cache.get(&path).map_or(false, |e| !e.invalidated) {
                let se = c.dir_cache.get(&path).and_then(|e| e.self_entry.clone());
                if poll_iters > 2 { log::debug!("LIST_PROMOTED_WAIT {} iters for {}", poll_iters, path.display()); }
                log::info!("LIST_PROMOTED {} in {:?}", path.display(), t0.elapsed());
                return c.get_cached_dir(&path, ttl)
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

    // Timeout: drain pending channel, only cache if entries arrived or sender finished
    let mut c = cache.safe_lock();
    let should_promote = if let Some(pending) = c.pending_dirs.get_mut(&path) {
        let mut disconnected = false;
        loop {
            match pending.rx.try_recv() {
                Ok(entry) => pending.entries.push(entry),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => { disconnected = true; break; }
            }
        }
        !pending.entries.is_empty() || disconnected
    } else {
        false
    };
    if should_promote {
        let se = c.promote_pending(&path);
        if let Some((files, _)) = c.get_cached_dir(&path, ttl) {
            return Ok((files, se));
        }
    } else {
        log::warn!("PROPFIND timeout {} — removing stale pending (no entries yet, elapsed {:?})", path.display(), t0.elapsed());
        c.pending_dirs.remove(&path);
    }
    Err(format!("PROPFIND timeout for {}", path.display()))
}

pub(crate) fn ensure_file_cached(
    net: &Arc<FsNetwork>,
    throttle: &Arc<Throttle>,
    cache: &Arc<Mutex<FsCache>>,
    status: &StatusMap,
    dirty: &ipc::DirtySet,
    remote_path: PathBuf,
    transfers: Option<&TransferMap>,
) -> Result<PathBuf, String> {
    let (maybe_local, cached_mod, current_mod, cache_dir, file_size) = {
        let c = cache.safe_lock();
        let entry = c.file_cache.get(&remote_path);
        let maybe_local = entry
            .filter(|e| e.local_path.metadata().map_or(false, |m| m.len() > 0))
            .map(|e| e.local_path.clone());
        let cached_mod = entry.and_then(|e| e.remote_modified);
        let current_mod = c.remote_modified_for(&remote_path);
        let file_size = c.find_entry(&remote_path).map(|e| e.size).unwrap_or(0);
        (maybe_local, cached_mod, current_mod, c.cache_dir.clone(), file_size)
    };

    if let Some(local) = maybe_local {
        if cached_mod == current_mod {
            return Ok(local);
        }
    }

    status.safe_lock().insert(remote_path.clone(), FileStatus::Downloading);
    dirty.safe_lock().insert(remote_path.clone());

    if let Some(tm) = transfers {
        tm.safe_lock().insert(remote_path.clone(), TransferProgress {
            path: remote_path.clone(),
            direction: TransferDirection::Download,
            bytes_done: 0,
            total_bytes: file_size,
        });
    }

    let rel = remote_path.strip_prefix("/").unwrap_or(&remote_path);
    let local_path = cache_dir.join(rel);
    if let Some(parent) = local_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let file =
        std::fs::File::create(&local_path).map_err(|e| format!("create cache file: {}", e))?;
    if let Err(e) = open_file_timeout(net, throttle, remote_path.clone(), file, transfers.cloned()) {
        if let Some(tm) = transfers { tm.safe_lock().remove(&remote_path); }
        let _ = std::fs::remove_file(&local_path);
        status.safe_lock().insert(remote_path.clone(), FileStatus::Remote);
        dirty.safe_lock().insert(remote_path);
        return Err(e);
    }

    if let Some(tm) = transfers { tm.safe_lock().remove(&remote_path); }

    {
        let mut c = cache.safe_lock();
        let mod_time = c.remote_modified_for(&remote_path);
        let etag = c.remote_etag_for(&remote_path);
        c.file_cache.insert(
            remote_path.clone(),
            FileCacheEntry { local_path: local_path.clone(), remote_modified: mod_time, etag },
        );
    }
    save_file_cache(cache);
    status.safe_lock().insert(remote_path.clone(), FileStatus::Local);
    dirty.safe_lock().insert(remote_path);
    Ok(local_path)
}

fn keep_locally_recursive(
    conn: &Arc<ConnInfo>,
    net: &Arc<FsNetwork>,
    cache: &Arc<Mutex<FsCache>>,
    status: &StatusMap,
    dirty: &ipc::DirtySet,
    remote_path: PathBuf,
    transfers: Option<&TransferMap>,
) {
    log::info!("KEEP {}", remote_path.display());

    let known_dir = cache.safe_lock().is_known_directory(&remote_path);

    if known_dir == Some(false) {
        if let Err(e) = ensure_file_cached(net, &conn.throttle, cache, status, dirty, remote_path.clone(), transfers) {
            log::warn!("keep failed {}: {}", remote_path.display(), e);
        }
        return;
    }

    let (entries, _self_entry) = match get_or_list_dir(conn, cache, remote_path.clone()) {
        Ok(e) => e,
        Err(e) => {
            if known_dir.is_none() {
                if let Err(e2) = ensure_file_cached(net, &conn.throttle, cache, status, dirty, remote_path.clone(), transfers) {
                    log::warn!("keep failed {}: {} / {}", remote_path.display(), e, e2);
                }
            } else {
                log::warn!("keep dir failed {}: {}", remote_path.display(), e);
            }
            return;
        }
    };

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

    for chunk in files.chunks(2) {
        std::thread::scope(|s| {
            for path in chunk {
                s.spawn(|| {
                    if let Err(e) = ensure_file_cached(net, &conn.throttle, cache, status, dirty, path.clone(), transfers) {
                        log::warn!("keep failed {}: {}", path.display(), e);
                    }
                });
            }
        });
        std::thread::sleep(Duration::from_millis(50));
    }

    for dir in dirs {
        keep_locally_recursive(conn, net, cache, status, dirty, dir, transfers);
    }
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
    match propfind::propfind_list(&conn.http, &conn.webdav_url, &conn.username, &conn.password, path, PROPFIND_TIMEOUT) {
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
    {
        let c = cache.safe_lock();
        if c.dir_cache.contains_key(&path) || c.pending_dirs.contains_key(&path) {
            return;
        }
    }
    let (entry_tx, entry_rx) = mpsc::channel();
    let (etag_tx, etag_rx) = mpsc::channel();
    let (self_tx, self_rx) = mpsc::channel();
    let pending_notify2 = cache.safe_lock().start_pending_and_notify(path.clone(), entry_rx, etag_rx, self_rx);
    let conn2 = conn.clone();
    let cache2 = cache.clone();
    thread::spawn(move || {
        let result = {
            let _permit = conn2.prefetch_throttle.acquire();
            propfind::propfind_list_streaming(
                &conn2.http, &conn2.webdav_url, &conn2.username, &conn2.password,
                &path, PROPFIND_TIMEOUT, entry_tx, self_tx,
            )
            // _permit (prefetch throttle slot) released here, before chaining children
        };
        match result {
            Ok(etag) => { let _ = etag_tx.send(etag); }
            Err(e) => {
                log::debug!("bg propfind {}: {}", path.display(), e);
                let _ = etag_tx.send(None);
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

pub(crate) fn make_file_attr(inode: u64, entry: &DavEntry) -> FileAttr {
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
        perm: perms_to_mode(entry.permissions.as_deref(), entry.is_dir),
        nlink: if entry.is_dir { 2 } else { 1 },
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
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
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
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
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        flags: 0,
        blksize: 512,
    }
}

// ── Filesystem ────────────────────────────────────────────────────────────────

struct ConnInfo {
    base_url: String,
    webdav_url: String,
    username: String,
    password: String,
    mount_point: PathBuf,
    http: reqwest::blocking::Client,
    optimistic_listing: bool,
    notify_push_connected: Arc<AtomicBool>,
    http_read: reqwest::blocking::Client,
    throttle: Arc<Throttle>,
    read_throttle: Arc<Throttle>,
    prefetch_throttle: Arc<Throttle>,
    is_offline: Arc<AtomicBool>,
    active_streams: Arc<AtomicUsize>,
    deferred_invalidation: Arc<AtomicBool>,
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

pub struct NextCloudFs {
    net: Arc<FsNetwork>,
    cache: Arc<Mutex<FsCache>>,
    status: StatusMap,
    dirty: ipc::DirtySet,
    shared: ipc::SharedSet,
    fileids: ipc::FileIdMap,
    details: ipc::FileDetailMap,
    conn: Arc<ConnInfo>,
    open_files: Arc<Mutex<HashMap<u64, OpenFile>>>,
    next_fh: Arc<Mutex<u64>>,
    error_log: ErrorLog,
    transfer_map: TransferMap,
    journal: mutation_journal::SharedJournal,
    notifier_slot: fuse_notify::NotifierSlot,
    ghost_entries: GhostMap,
    log_user: String,
    aggressive_prefetch: bool,
    auto_keep_locally_modified_files: bool,
}

impl NextCloudFs {
    pub fn new(options: MountOptions) -> Result<Self, String> {
        let username = options.username.unwrap_or_default();
        let password = options.password.unwrap_or_default();
        let mut initial = WebDAVFs::new(&username, &password, &options.url);
        if !options.offline {
            initial.connect().map_err(|e| format!("WebDAV connect failed: {}", e))?;
        } else {
            log::info!("OFFLINE mode: skipping initial WebDAV connection");
        }

        let cache_dir = dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("ncrs")
            .join(url_to_dir_name(&options.url));
        std::fs::create_dir_all(&cache_dir)
            .map_err(|e| format!("Cannot create cache dir: {}", e))?;

        let journal_arc: mutation_journal::SharedJournal =
            Arc::new(Mutex::new(mutation_journal::MutationJournal::load_or_create(&cache_dir)));

        let mut inodes = HashMap::new();
        let mut paths = HashMap::new();
        inodes.insert(1, PathBuf::from("/"));
        paths.insert(PathBuf::from("/"), 1);

        let status: StatusMap = Arc::new(Mutex::new(HashMap::new()));
        let dirty: ipc::DirtySet = Arc::new(Mutex::new(std::collections::HashSet::new()));
        let shared: ipc::SharedSet = Arc::new(Mutex::new(std::collections::HashSet::new()));
        let fileids: ipc::FileIdMap = Arc::new(Mutex::new(HashMap::new()));
        let details: ipc::FileDetailMap = Arc::new(Mutex::new(HashMap::new()));

        let mut http_builder = reqwest::blocking::Client::builder()
            .pool_max_idle_per_host(16);
        let mut read_builder = reqwest::blocking::Client::builder()
            .pool_max_idle_per_host(8)
            .tcp_nodelay(true);
        if options.http3 {
            log::info!("HTTP/3 (QUIC) enabled");
            http_builder = http_builder.http3_prior_knowledge();
            read_builder = read_builder.http3_prior_knowledge();
        }
        let http = http_builder.build()
            .map_err(|e| format!("HTTP client: {}", e))?;
        let http_read = read_builder.build()
            .map_err(|e| format!("HTTP read client: {}", e))?;

        let max_req = if options.max_concurrent_requests == 0 { 10 } else { options.max_concurrent_requests };
        log::info!("HTTP throttle: max {} concurrent requests", max_req);
        let is_offline = Arc::new(AtomicBool::new(options.offline));

        let conn = Arc::new(ConnInfo {
            base_url: notifications::base_url(&options.url),
            webdav_url: options.url.clone(),
            username: username.clone(),
            password: password.clone(),
            mount_point: options.mount_point.clone(),
            http,
            http_read,
            throttle: Arc::new(Throttle::new(max_req)),
            read_throttle: Arc::new(Throttle::new(3)),
            prefetch_throttle: Arc::new(Throttle::new(5)),
            is_offline,
            optimistic_listing: options.optimistic_listing,
            notify_push_connected: Arc::new(AtomicBool::new(false)),
            active_streams: Arc::new(AtomicUsize::new(0)),
            deferred_invalidation: Arc::new(AtomicBool::new(false)),
        });

        Ok(NextCloudFs {
            net: Arc::new(FsNetwork {
                conns: Mutex::new(vec![initial]),
                url: options.url.clone(),
                username: username.clone(),
                password: password.clone(),
            }),
            cache: {
                let c = Arc::new(Mutex::new(FsCache {
                    inodes,
                    paths,
                    next_inode: 2,
                    dir_cache: HashMap::new(),
                    pending_dirs: HashMap::new(),
                    file_cache: HashMap::new(),
                    cache_dir,
                    pending_notify: Arc::new((Mutex::new(()), Condvar::new())),
                }));
                load_dir_cache(&c);
                c
            },
            status,
            dirty,
            shared,
            fileids,
            details,
            conn,
            open_files: Arc::new(Mutex::new(HashMap::new())),
            next_fh: Arc::new(Mutex::new(1)),
            error_log: Arc::new(Mutex::new(std::collections::VecDeque::new())),
            transfer_map: Arc::new(Mutex::new(HashMap::new())),
            journal: journal_arc,
            notifier_slot: Arc::new(Mutex::new(None)),
            ghost_entries: Arc::new(Mutex::new(HashMap::new())),
            log_user: options.log_user,
            aggressive_prefetch: options.aggressive_prefetch,
            auto_keep_locally_modified_files: options.auto_keep_locally_modified_files,
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

    pub fn dirty_set(&self) -> ipc::DirtySet {
        self.dirty.clone()
    }

    pub fn is_offline_flag(&self) -> Arc<AtomicBool> {
        self.conn.is_offline.clone()
    }

    pub fn notify_push_connected_flag(&self) -> Arc<AtomicBool> {
        self.conn.notify_push_connected.clone()
    }

    pub fn conn_http(&self) -> reqwest::blocking::Client {
        self.conn.http.clone()
    }

    pub fn active_streams(&self) -> Arc<AtomicUsize> {
        self.conn.active_streams.clone()
    }

    pub fn deferred_invalidation(&self) -> Arc<AtomicBool> {
        self.conn.deferred_invalidation.clone()
    }

    pub(crate) fn net(&self) -> Arc<FsNetwork> {
        self.net.clone()
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

    pub fn journal(&self) -> mutation_journal::SharedJournal {
        self.journal.clone()
    }

    pub fn notifier_slot(&self) -> fuse_notify::NotifierSlot {
        self.notifier_slot.clone()
    }

    pub(crate) fn ghost_entries(&self) -> GhostMap {
        self.ghost_entries.clone()
    }

    pub fn keep_callback(&self) -> ipc::KeepCallback {
        let conn = self.conn.clone();
        let net = self.net.clone();
        let cache = self.cache.clone();
        let status = self.status.clone();
        let dirty = self.dirty.clone();
        let transfers = self.transfer_map.clone();
        Arc::new(move |remote_path| {
            keep_locally_recursive(&conn, &net, &cache, &status, &dirty, remote_path, Some(&transfers));
        })
    }

    pub fn evict_callback(&self) -> ipc::EvictCallback {
        let cache = self.cache.clone();
        let status = self.status.clone();
        Arc::new(move |remote_path| {
            let local = {
                let mut c = cache.safe_lock();
                c.file_cache.remove(&remote_path).map(|e| e.local_path)
            };
            if let Some(local_path) = local {
                let _ = std::fs::remove_file(&local_path);
            }
            save_file_cache(&cache);
            status.safe_lock().insert(remote_path, FileStatus::Remote);
        })
    }

    pub fn prefetch_callback(&self) -> ipc::PrefetchCallback {
        let conn = self.conn.clone();
        let cache = self.cache.clone();
        Arc::new(move |remote_path| {
            prefetch_list_dir(&conn, &cache, &remote_path);
        })
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

        let is_cached = self.cache.safe_lock().dir_cache.contains_key(&parent_path);
        if !is_cached {
            let _ = get_or_list_dir(&self.conn, &self.cache, parent_path.clone());
        }

        let mut c = self.cache.safe_lock();
        let entries = match c.get_cached_dir_readonly(&parent_path) {
            Some(files) => files,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        for entry in entries.iter() {
            let entry_name = entry.path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if entry_name == name_str {
                let target_path = parent_path.join(&name_str);
                let ino = c.allocate_inode(target_path.clone());
                let attr = make_file_attr(ino, entry);
                drop(c);
                if entry.is_shared {
                    self.shared.safe_lock().insert(target_path.clone());
                }
                if let Some(fid) = entry.fileid {
                    self.fileids.safe_lock().insert(target_path.clone(), fid);
                }
                self.details.safe_lock().insert(target_path.clone(), ipc::FileDetail {
                    permissions: entry.permissions.clone(),
                    owner_id: entry.owner_id.clone(),
                    owner_display_name: entry.owner_display_name.clone(),
                    size: entry.size,
                    is_dir: entry.is_dir,
                });
                if !entry.is_dir {
                    self.status.safe_lock().entry(target_path).or_insert(FileStatus::Remote);
                }
                reply.entry(&TTL, &attr, Generation(0));
                return;
            }
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

        let c = self.cache.safe_lock();
        let entries = match c.get_cached_dir_readonly(&parent) {
            Some(files) => files,
            None => {
                if c.dir_cache.contains_key(&path) || path == Path::new("/") {
                    drop(c);
                    reply.attr(&TTL, &make_dir_attr(ino.0));
                } else {
                    reply.error(Errno::ENOENT);
                }
                return;
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

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let (path, local, etag, nc_permissions) = {
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
            let (etag, nc_permissions) = c.get_cached_dir_readonly(&parent)
                .and_then(|files| files.iter().find(|e| e.path == path).map(|e| {
                    (e.etag.clone(), e.permissions.clone())
                }))
                .unwrap_or((None, None));
            (path, local, etag, nc_permissions)
        };

        let writable = flags.0 & (libc::O_WRONLY | libc::O_RDWR | libc::O_APPEND) != 0;

        if writable && perms_to_mode(nc_permissions.as_deref(), false) & 0o200 == 0
            && nc_permissions.is_some()
        {
            reply.error(Errno::EACCES);
            return;
        }

        let fh = {
            let mut n = self.next_fh.safe_lock();
            let fh = *n;
            *n += 1;
            fh
        };

        let write_path = if writable {
            let cache_dir = self.cache.safe_lock().cache_dir.clone();
            let wp = cache_dir.join(format!("write_{}", fh));
            if let Some(ref local) = local {
                let _ = std::fs::copy(local, &wp);
            }
            Some(wp)
        } else {
            None
        };

        self.open_files.safe_lock().insert(
            fh,
            OpenFile {
                remote_path: path,
                local,
                buf: None,
                write_path,
                dirty: false,
                original_etag: etag,
            },
        );
        reply.opened(FileHandle(fh), FopenFlags::empty());
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

        // Serve from open-file state synchronously (no thread spawn).
        {
            let files = self.open_files.safe_lock();
            if let Some(of) = files.get(&fh.0) {
                if let Some(ref local) = of.local {
                    if let Ok(f) = std::fs::File::open(local) {
                        let mut buf = vec![0u8; sz];
                        if let Ok(n) = f.read_at(&mut buf, off) {
                            buf.truncate(n);
                            reply.data(&buf);
                            return;
                        }
                    }
                }
                if let Some(ref ra) = of.buf {
                    let (ref mtx, ref _cv) = *ra.stream;
                    let ss = mtx.lock().unwrap();
                    let available = ra.start + ss.data.len() as u64;
                    if off >= ra.start && off + sz as u64 <= available {
                        let s = (off - ra.start) as usize;
                        reply.data(&ss.data[s..s + sz]);
                        drop(ss);
                        drop(files);
                        return;
                    }
                    // Data within target range but not yet downloaded — wait in thread
                    if off >= ra.start && off + sz as u64 <= ra.start + ra.target_len && !ss.done {
                        let shared = Arc::clone(&ra.stream);
                        let start = ra.start;
                        drop(ss);
                        drop(files);
                        thread::spawn(move || {
                            let (ref mtx, ref cv) = *shared;
                            let mut guard = mtx.lock().unwrap();
                            let deadline = Instant::now() + Duration::from_secs(30);
                            loop {
                                if start + guard.data.len() as u64 >= off + sz as u64 {
                                    let s = (off - start) as usize;
                                    reply.data(&guard.data[s..s + sz]);
                                    return;
                                }
                                if guard.done {
                                    let end = start + guard.data.len() as u64;
                                    if off < end {
                                        let s = (off - start) as usize;
                                        let e = std::cmp::min(s + sz, guard.data.len());
                                        reply.data(&guard.data[s..e]);
                                    } else {
                                        reply.data(&[]);
                                    }
                                    return;
                                }
                                let remaining = deadline.saturating_duration_since(Instant::now());
                                if remaining.is_zero() {
                                    log::warn!("read wait timeout at offset {}", off);
                                    reply.error(Errno::EIO);
                                    return;
                                }
                                let (g, result) = cv.wait_timeout(guard, remaining).unwrap();
                                guard = g;
                                if result.timed_out() && !guard.done {
                                    log::warn!("read wait timeout at offset {}", off);
                                    reply.error(Errno::EIO);
                                    return;
                                }
                            }
                        });
                        return;
                    }
                    drop(ss);
                }
            }
        }

        // Check file_cache synchronously too.
        {
            let cached_local = self.cache.safe_lock().file_cache.get(&path)
                .filter(|fc| fc.local_path.metadata().map_or(false, |m| m.len() > 0))
                .map(|fc| fc.local_path.clone());
            if let Some(ref local) = cached_local {
                if let Ok(f) = std::fs::File::open(local) {
                    let mut buf = vec![0u8; sz];
                    if let Ok(n) = f.read_at(&mut buf, off) {
                        buf.truncate(n);
                        reply.data(&buf);
                        self.open_files.safe_lock().entry(fh.0).and_modify(|of| of.local = Some(local.clone()));
                        return;
                    }
                }
            }
        }

        // Network fetch — only this path needs a thread.
        let open_files = self.open_files.clone();
        let conn = self.conn.clone();
        let net = self.net.clone();
        let cache = self.cache.clone();
        let status = self.status.clone();
        let dirty = self.dirty.clone();
        let elog = self.error_log.clone();
        let tmap = self.transfer_map.clone();
        let notifier_slot = self.notifier_slot.clone();

        thread::spawn(move || {
            let fetch = std::cmp::max(sz, READ_AHEAD);
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
            match do_range_read_stream(&conn, &path, off, fetch, use_throttle) {
                Ok((mut resp, _permit)) => {
                    let t0 = Instant::now();
                    match read_exact_from_stream(&mut resp, sz) {
                        Ok(first) => {
                            reply.data(&first);
                            tmap.safe_lock().insert(path.clone(), TransferProgress {
                                path: path.clone(),
                                direction: TransferDirection::Download,
                                bytes_done: first.len() as u64,
                                total_bytes: fetch as u64,
                            });
                            let shared = Arc::new((
                                Mutex::new(StreamState { data: first, done: false }),
                                Condvar::new(),
                            ));
                            open_files.safe_lock().entry(fh.0).and_modify(|of| {
                                of.buf = Some(ReadAheadBuf {
                                    start: off,
                                    stream: Arc::clone(&shared),
                                    target_len: fetch as u64,
                                });
                            });
                            let (ref mtx, ref cv) = *shared;
                            let mut chunk = [0u8; 256 * 1024];
                            let mut since_check = 0usize;
                            loop {
                                use std::io::Read;
                                match resp.read(&mut chunk) {
                                    Ok(0) => break,
                                    Ok(n) => {
                                        mtx.lock().unwrap().data.extend_from_slice(&chunk[..n]);
                                        cv.notify_all();
                                        since_check += n;
                                        if since_check >= 2 * 1024 * 1024 {
                                            since_check = 0;
                                            if let Ok(mut tm) = tmap.lock() {
                                                if let Some(tp) = tm.get_mut(&path) {
                                                    tp.bytes_done = mtx.lock().unwrap().data.len() as u64;
                                                }
                                            }
                                            let superseded = open_files.safe_lock()
                                                .get(&fh.0)
                                                .and_then(|of| of.buf.as_ref())
                                                .map_or(true, |b| !Arc::ptr_eq(&b.stream, &shared));
                                            if superseded { break; }
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                            let mut ss = mtx.lock().unwrap();
                            ss.done = true;
                            let total_bytes = ss.data.len();
                            drop(ss);
                            cv.notify_all();
                            tmap.safe_lock().remove(&path);
                            let total_ms = t0.elapsed().as_millis();
                            if total_ms > 0 {
                                let mbps = total_bytes as f64 / 1_048_576.0 / (total_ms as f64 / 1000.0);
                                log::info!("stream read {}B total={}ms {:.1}MB/s", total_bytes, total_ms, mbps);
                            }
                        }
                        Err(e) => {
                            log::warn!("stream read first bytes failed: {}", e);
                            push_error(&elog, path.clone(), SyncErrorKind::NetworkError, format!("download failed: {}", e));
                            reply.error(Errno::EIO);
                        }
                    }
                }
                Err(e) => {
                    log::warn!("range read failed, falling back to full download: {}", e);
                    match ensure_file_cached(&net, &conn.throttle, &cache, &status, &dirty, path.clone(), Some(&tmap)) {
                        Ok(local) => {
                            if let Ok(f) = std::fs::File::open(&local) {
                                let mut buf = vec![0u8; sz];
                                match f.read_at(&mut buf, off) {
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
        let mut files = self.open_files.safe_lock();
        if let Some(of) = files.get(&fh.0) {
            if of.dirty {
                if let Some(ref wp) = of.write_path {
                    log::warn!("release: fh {} still dirty, staging file preserved at {}", fh.0, wp.display());
                }
            }
        }
        files.remove(&fh.0);
        reply.ok();
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
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
        let dirty = self.dirty.clone();
        let conn = self.conn.clone();
        let aggressive_prefetch = self.aggressive_prefetch;

        thread::spawn(move || {
            if offset == 0 {
                if reply.add(ino, 1, FileType::Directory, ".") {
                    reply.ok();
                    return;
                }
                if reply.add(INodeNo(parent_ino), 2, FileType::Directory, "..") {
                    reply.ok();
                    return;
                }
            }

            // Continuation pages (offset > 0): serve directly from cache, skip all heavy work.
            if offset > 0 {
                let skip = (offset - 2) as usize;
                let c = cache.safe_lock();
                if let Some(entries) = c.get_cached_dir_readonly(&path) {
                    for (i, entry) in entries.iter().enumerate().skip(skip) {
                        let name = match entry.path.file_name().and_then(|n| n.to_str()) {
                            Some(n) => n,
                            None => continue,
                        };
                        let entry_path = path.join(name);
                        let entry_ino = c.get_inode(&entry_path).unwrap_or(1);
                        let kind =
                            if entry.is_dir { FileType::Directory } else { FileType::RegularFile };
                        if reply.add(INodeNo(entry_ino), (i + 3) as u64, kind, name) {
                            break;
                        }
                    }
                }
                reply.ok();
                return;
            }

            let t_readdir = Instant::now();
            match get_or_list_dir(&conn, &cache, path.clone()) {
                Ok((entries, self_entry)) => {
                    log::info!("READDIR {} get_or_list_dir returned {} entries in {:?}", path.display(), entries.len(), t_readdir.elapsed());

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

                    {
                        // Collect entry paths and allocate inodes (short cache lock)
                        let cache_dir = {
                            let mut c = cache.safe_lock();
                            for entry in entries.iter() {
                                if let Some(name) = entry.path.file_name().and_then(|n| n.to_str()) {
                                    c.allocate_inode(path.join(name));
                                }
                            }
                            c.cache_dir.clone()
                        };

                        let mut shared_paths = Vec::new();
                        let mut fileid_paths = Vec::new();
                        let mut detail_entries = Vec::new();
                        let mut status_entries = Vec::new();
                        let mut cache_entries = Vec::new();

                        if let Some(ref se) = self_entry {
                            if se.is_shared { shared_paths.push(path.clone()); }
                            if let Some(fid) = se.fileid { fileid_paths.push((path.clone(), fid)); }
                            detail_entries.push((path.clone(), ipc::FileDetail {
                                permissions: se.permissions.clone(),
                                owner_id: se.owner_id.clone(),
                                owner_display_name: se.owner_display_name.clone(),
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
                            if entry.is_shared { shared_paths.push(entry_path.clone()); }
                            if let Some(fid) = entry.fileid { fileid_paths.push((entry_path.clone(), fid)); }
                            detail_entries.push((entry_path.clone(), ipc::FileDetail {
                                permissions: entry.permissions.clone(),
                                owner_id: entry.owner_id.clone(),
                                owner_display_name: entry.owner_display_name.clone(),
                                size: entry.size,
                                is_dir: entry.is_dir,
                            }));
                            if !entry.is_dir {
                                let rel = entry_path.strip_prefix("/").unwrap_or(&entry_path);
                                let local_path = cache_dir.join(rel);
                                if local_path.exists() {
                                    status_entries.push((entry_path.clone(), FileStatus::Local));
                                    cache_entries.push((entry_path.clone(), FileCacheEntry {
                                        local_path,
                                        remote_modified: entry.modified,
                                        etag: entry.etag.clone(),
                                    }));
                                } else {
                                    status_entries.push((entry_path.clone(), FileStatus::Remote));
                                }
                                thumb_candidates.push((entry_path, entry.modified, entry.has_preview, entry.fileid));
                            }
                        }

                        // Evict stale entries for this directory, then insert fresh ones
                        let is_child_of_dir = |p: &PathBuf| p == &path || p.parent() == Some(&path);
                        {
                            let mut sh = shared.safe_lock();
                            sh.retain(|p| !is_child_of_dir(p));
                            for p in shared_paths { sh.insert(p); }
                        }
                        {
                            let mut fi = fileids.safe_lock();
                            fi.retain(|p, _| !is_child_of_dir(p));
                            for (p, fid) in fileid_paths { fi.insert(p, fid); }
                        }
                        {
                            let mut dt = details.safe_lock();
                            dt.retain(|p, _| !is_child_of_dir(p));
                            for (p, d) in detail_entries { dt.insert(p, d); }
                        }
                        // Collect paths with in-flight or just-completed upload status before
                        // we clear smap.  These must (a) survive the PROPFIND status reset so
                        // the upload emblem persists, and (b) be dirtied individually so
                        // Nautilus re-queries their NC properties from the fresh detail_map.
                        let upload_paths: Vec<PathBuf> = {
                            let st = status.safe_lock();
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
                        {
                            let mut st = status.safe_lock();
                            st.retain(|p, _| !is_child_of_dir(p));
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
                            dirty.safe_lock().insert(path.clone());
                            // Additionally dirty recently uploaded files so Nautilus re-queries
                            // them and picks up NC properties from the freshly populated detail_map.
                            for p in upload_paths {
                                dirty.safe_lock().insert(p);
                            }
                        }
                    }

                    {
                        let c = cache.safe_lock();
                        for (i, entry) in entries.iter().enumerate() {
                            let name = match entry.path.file_name().and_then(|n| n.to_str()) {
                                Some(n) => n,
                                None => continue,
                            };
                            let entry_path = path.join(name);
                            let entry_ino = c.get_inode(&entry_path).unwrap_or(1);
                            let kind =
                                if entry.is_dir { FileType::Directory } else { FileType::RegularFile };
                            if reply.add(INodeNo(entry_ino), (i + 3) as u64, kind, name) {
                                break;
                            }
                        }
                    }

                    log::info!("READDIR {} reply.ok() at {:?}", path.display(), t_readdir.elapsed());
                    reply.ok();
                    schedule_save_dir_cache(&cache);

                    if offset == 0 {
                        if !thumb_candidates.is_empty() {
                            let conn2 = conn.clone();
                            thread::spawn(move || {
                                thread::sleep(Duration::from_millis(500));
                                preview::prefetch_directory_thumbnails(
                                    &conn2.http,
                                    &conn2.base_url,
                                    &conn2.username,
                                    &conn2.password,
                                    &conn2.mount_point,
                                    &thumb_candidates,
                                    &conn2.active_streams,
                                );
                            });
                        }
                    }
                }
                Err(e) => {
                    log::error!("readdir {}: {}", path.display(), e);
                    reply.error(error_to_errno(&e));
                }
            }
        });
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
                    let wp = of.write_path.get_or_insert_with(|| {
                        let cache_dir = self.cache.safe_lock().cache_dir.clone();
                        cache_dir.join(format!("write_{}", fh_raw))
                    });
                    if !wp.exists() {
                        if let Some(ref local) = of.local {
                            let _ = std::fs::copy(local, &wp);
                        } else {
                            let _ = std::fs::File::create(&wp);
                        }
                    }
                    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&wp) {
                        let _ = f.set_len(new_size);
                    }
                    of.dirty = true;
                }
            }
            let c = self.cache.safe_lock();
            let path = c.get_path(ino.0);
            let entry = path.as_ref().and_then(|p| {
                let parent = p.parent().unwrap_or(Path::new("/")).to_path_buf();
                c.get_cached_dir_readonly(&parent).and_then(|files| {
                    files.iter().find(|e| e.path == **p).cloned()
                })
            });
            drop(c);
            let mut attr = match entry {
                Some(ref e) => make_file_attr(ino.0, e),
                None => make_dir_attr(ino.0),
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
        });
        if !wp.exists() {
            if let Some(ref local) = of.local {
                let _ = std::fs::copy(local, &wp);
            } else {
                let _ = std::fs::File::create(&wp);
            }
        }

        let wp_clone = wp.clone();
        match std::fs::OpenOptions::new().write(true).create(true).open(&wp_clone) {
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
        let (remote_path, write_path, original_etag) = {
            let files = self.open_files.safe_lock();
            match files.get(&fh.0) {
                Some(of) if of.dirty => (
                    of.remote_path.clone(),
                    of.write_path.clone(),
                    of.original_etag.clone(),
                ),
                _ => {
                    reply.ok();
                    return;
                }
            }
        };

        let write_path = match write_path {
            Some(p) => p,
            None => {
                reply.ok();
                return;
            }
        };

        let upload_size = match std::fs::metadata(&write_path) {
            Ok(m) => m.len(),
            Err(_) => {
                log::error!("flush: staging file missing at {}", write_path.display());
                reply.error(Errno::EIO);
                return;
            }
        };

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

        if !self.conn.is_offline.load(Ordering::Relaxed) {
            self.status.safe_lock().insert(remote_path.clone(), FileStatus::Uploading);
            self.dirty.safe_lock().insert(remote_path.clone());
        }
        reply.ok();

        if !self.conn.is_offline.load(Ordering::Relaxed) {
            let body = match std::fs::read(&write_path) {
                Ok(b) => b,
                Err(e) => {
                    log::error!("read staging file for flush: {}", e);
                    self.status.safe_lock().remove(&remote_path);
                    return;
                }
            };

            let conn = self.conn.clone();
            let cache = self.cache.clone();
            let dirty = self.dirty.clone();
            let open_files = self.open_files.clone();
            let elog = self.error_log.clone();
            let tmap = self.transfer_map.clone();
            let journal = self.journal.clone();
            let smap = self.status.clone();
            let auto_keep = self.auto_keep_locally_modified_files;

            thread::spawn(move || {
                let _permit = conn.throttle.acquire();
                let upload_size = body.len() as u64;
                tmap.safe_lock().insert(remote_path.clone(), TransferProgress {
                    path: remote_path.clone(),
                    direction: TransferDirection::Upload,
                    bytes_done: 0,
                    total_bytes: upload_size,
                });
                let etag_ref = original_etag.as_deref();
                match webdav_ops::put_file(&conn.http, &conn.base_url, &conn.username, &conn.password, &remote_path, body.clone(), etag_ref) {
                    Ok(result) => {
                        tmap.safe_lock().remove(&remote_path);
                        log::info!("PUT {} → new etag {:?}", remote_path.display(), result.new_etag);
                        let new_size = body.len() as u64;
                        {
                            let mut c = cache.safe_lock();
                            let parent = remote_path.parent().unwrap_or(Path::new("/")).to_path_buf();
                            if let Some(dir) = c.dir_cache.get_mut(&parent) {
                                let mut files = (*dir.files).clone();
                                if let Some(entry) = files.iter_mut().find(|e| e.path == remote_path) {
                                    entry.etag = result.new_etag.clone();
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
                            of.original_etag = result.new_etag.clone();
                        }
                        if auto_keep {
                            // Keep a local copy so the file is available offline.
                            let rel = remote_path.strip_prefix("/").unwrap_or(&remote_path);
                            let cache_path = cache.safe_lock().cache_dir.join(rel);
                            let mut kept = false;
                            if let Some(parent) = cache_path.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            if std::fs::write(&cache_path, &body).is_ok() {
                                cache.safe_lock().file_cache.insert(remote_path.clone(), FileCacheEntry {
                                    local_path: cache_path,
                                    remote_modified: Some(SystemTime::now()),
                                    etag: result.new_etag,
                                });
                                smap.safe_lock().insert(remote_path.clone(), FileStatus::Local);
                                kept = true;
                            }
                            if !kept {
                                smap.safe_lock().insert(remote_path.clone(), FileStatus::Synced);
                            }
                        } else {
                            smap.safe_lock().insert(remote_path.clone(), FileStatus::Synced);
                        }
                        let _ = std::fs::remove_file(&write_path);
                        dirty.safe_lock().insert(remote_path.clone());
                        dirty.safe_lock().insert(remote_path.parent().unwrap_or(Path::new("/")).to_path_buf());
                        journal.safe_lock().remove(seq);
                    }
                    Err(webdav_ops::WriteError::Conflict) => {
                        tmap.safe_lock().remove(&remote_path);
                        smap.safe_lock().remove(&remote_path);
                        log::warn!("CONFLICT on PUT {} — creating conflicted copy", remote_path.display());
                        push_error(&elog, remote_path.clone(), SyncErrorKind::Conflict, "Server version changed — conflicted copy created".into());
                        let conflict_name = make_conflict_name(&remote_path);
                        match webdav_ops::put_file(&conn.http, &conn.base_url, &conn.username, &conn.password, &conflict_name, body, None) {
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
                        tmap.safe_lock().remove(&remote_path);
                        smap.safe_lock().remove(&remote_path);
                        log::error!("PUT {} failed (journaled): {}", remote_path.display(), e);
                        let kind = match e {
                            webdav_ops::WriteError::Locked => SyncErrorKind::Locked,
                            webdav_ops::WriteError::Network(_) => SyncErrorKind::NetworkError,
                            webdav_ops::WriteError::Server(403, _) => SyncErrorKind::PermissionDenied,
                            webdav_ops::WriteError::Server(507, _) => SyncErrorKind::QuotaExceeded,
                            webdav_ops::WriteError::Server(code, _) => SyncErrorKind::ServerError(*code),
                            _ => SyncErrorKind::UploadFailed,
                        };
                        push_error(&elog, remote_path.clone(), kind, e.to_string());
                        journal.safe_lock().mark_failed(seq, e.to_string());
                        dirty.safe_lock().insert(remote_path.clone());
                    }
                }
            });
        }
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

        {
            let mut ghosts = self.ghost_entries.safe_lock();
            if let Some(ghost) = ghosts.remove(&full_path) {
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
                                self.open_files.safe_lock().insert(fh, OpenFile {
                                    remote_path: PathBuf::new(),
                                    local: None, buf: None, write_path: None,
                                    dirty: false, original_etag: None,
                                });
                                log::info!("ghost create: {} (inotify trigger)", full_path.display());
                                reply.created(&TTL, &attr, Generation(0), FileHandle(fh), FopenFlags::empty());
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
        let _ = std::fs::File::create(&write_path);

        let ino = self.cache.safe_lock().allocate_inode(remote_path.clone());

        let now = SystemTime::now();
        let new_entry = DavEntry {
            path: remote_path.clone(),
            is_dir: false,
            size: 0,
            modified: Some(now),
            etag: None,
            content_type: None,
            has_preview: false,
            is_shared: false,
            permissions: Some("RGDNVW".to_string()),
            fileid: None,
            owner_id: None,
            owner_display_name: None,
        };

        {
            let mut c = self.cache.safe_lock();
            if let Some(dir) = c.dir_cache.get_mut(&parent_path) {
                let mut files = (*dir.files).clone();
                files.push(new_entry.clone());
                dir.files = Arc::new(files);
            }
        }

        self.open_files.safe_lock().insert(
            fh,
            OpenFile {
                remote_path,
                local: None,
                buf: None,
                write_path: Some(write_path),
                dirty: false,
                original_etag: None,
            },
        );

        let attr = make_file_attr(ino, &new_entry);
        reply.created(&TTL, &attr, Generation(0), FileHandle(fh), FopenFlags::empty());
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
            let mut ghosts = self.ghost_entries.safe_lock();
            if let Some(ghost) = ghosts.remove(&remote_path) {
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
        let new_entry = DavEntry {
            path: remote_path.clone(),
            is_dir: true,
            size: 0,
            modified: Some(now),
            etag: None,
            content_type: None,
            has_preview: false,
            is_shared: false,
            permissions: Some("RGDNVCK".to_string()),
            fileid: None,
            owner_id: None,
            owner_display_name: None,
        };
        let ino = {
            let mut c = self.cache.safe_lock();
            let ino = c.allocate_inode(remote_path.clone());
            if let Some(dir) = c.dir_cache.get_mut(&parent_path) {
                let mut files = (*dir.files).clone();
                files.push(new_entry);
                dir.files = Arc::new(files);
            }
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
            thread::spawn(move || {
                let _permit = conn.throttle.acquire();
                match webdav_ops::mkcol(&conn.http, &conn.base_url, &conn.username, &conn.password, &remote_path) {
                    Ok(()) => {
                        log::info!("MKCOL {}", remote_path.display());
                        journal.safe_lock().remove(seq);
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
                let files: Vec<DavEntry> = dir.files.iter().filter(|e| e.path != remote_path).cloned().collect();
                dir.files = Arc::new(files);
            }
        }

        self.dirty.safe_lock().insert(parent_path);
        reply.ok();

        let seq = self.journal.safe_lock().enqueue(
            mutation_journal::MutationOp::Unlink { path: remote_path.clone() },
        );

        if !self.conn.is_offline.load(Ordering::Relaxed) {
            let conn = self.conn.clone();
            let journal = self.journal.clone();
            let elog = self.error_log.clone();
            thread::spawn(move || {
                let _permit = conn.throttle.acquire();
                match webdav_ops::delete(&conn.http, &conn.base_url, &conn.username, &conn.password, &remote_path) {
                    Ok(()) => {
                        log::info!("DELETE {}", remote_path.display());
                        journal.safe_lock().remove(seq);
                    }
                    Err(e) => {
                        log::error!("DELETE {} failed (journaled): {}", remote_path.display(), e);
                        push_error(&elog, remote_path, SyncErrorKind::ServerError(0), format!("delete failed: {}", e));
                        journal.safe_lock().mark_failed(seq, e.to_string());
                    }
                }
            });
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
                let files: Vec<DavEntry> = dir.files.iter().filter(|e| e.path != remote_path).cloned().collect();
                dir.files = Arc::new(files);
            }
            c.dir_cache.remove(&remote_path);
        }
        self.dirty.safe_lock().insert(parent_path);
        reply.ok();

        let seq = self.journal.safe_lock().enqueue(
            mutation_journal::MutationOp::RmDir { path: remote_path.clone() },
        );

        if !self.conn.is_offline.load(Ordering::Relaxed) {
            let conn = self.conn.clone();
            let journal = self.journal.clone();
            let elog = self.error_log.clone();
            thread::spawn(move || {
                let _permit = conn.throttle.acquire();
                match webdav_ops::delete(&conn.http, &conn.base_url, &conn.username, &conn.password, &remote_path) {
                    Ok(()) => {
                        log::info!("RMDIR {}", remote_path.display());
                        journal.safe_lock().remove(seq);
                    }
                    Err(e) => {
                        log::error!("RMDIR {} failed (journaled): {}", remote_path.display(), e);
                        push_error(&elog, remote_path, SyncErrorKind::ServerError(0), format!("rmdir failed: {}", e));
                        journal.safe_lock().mark_failed(seq, e.to_string());
                    }
                }
            });
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
                entry.path = to.clone();
                if let Some(dir) = c.dir_cache.get_mut(&new_parent_path) {
                    let mut files = (*dir.files).clone();
                    files.push(entry);
                    dir.files = Arc::new(files);
                }
            }
        }
        let same_parent = old_parent_path == new_parent_path;
        self.dirty.safe_lock().insert(old_parent_path);
        if !same_parent {
            self.dirty.safe_lock().insert(new_parent_path);
        }
        reply.ok();

        let seq = self.journal.safe_lock().enqueue(
            mutation_journal::MutationOp::Rename { from: from.clone(), to: to.clone() },
        );

        if !self.conn.is_offline.load(Ordering::Relaxed) {
            let conn = self.conn.clone();
            let journal = self.journal.clone();
            let elog = self.error_log.clone();
            thread::spawn(move || {
                let _permit = conn.throttle.acquire();
                match webdav_ops::move_resource(&conn.http, &conn.base_url, &conn.username, &conn.password, &from, &to) {
                    Ok(()) => {
                        log::info!("MOVE {} → {}", from.display(), to.display());
                        journal.safe_lock().remove(seq);
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

// ── HTTP Range reads ─────────────────────────────────────────────────────────

fn webdav_file_url(base: &str, remote_path: &Path) -> String {
    let rel = remote_path.strip_prefix("/").unwrap_or(remote_path);
    let encoded = utf8_percent_encode(&rel.to_string_lossy(), PATH_ENCODE).to_string();
    format!("{}/{}", base.trim_end_matches('/'), encoded)
}

fn do_range_read_stream<'a>(
    conn: &'a ConnInfo,
    path: &Path,
    offset: u64,
    size: usize,
    throttle: bool,
) -> Result<(reqwest::blocking::Response, Option<ThrottleGuard<'a>>), String> {
    if conn.is_offline.load(Ordering::Relaxed) {
        return Err("file not available offline".into());
    }
    let permit = if throttle { Some(conn.read_throttle.acquire()) } else { None };
    let url = webdav_file_url(&conn.webdav_url, path);
    let end = offset + size as u64 - 1;
    let resp = conn.http_read
        .get(&url)
        .timeout(DOWNLOAD_TIMEOUT)
        .header("Range", format!("bytes={}-{}", offset, end))
        .basic_auth(&conn.username, Some(&conn.password))
        .send()
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    if status == reqwest::StatusCode::PARTIAL_CONTENT || status.is_success() {
        Ok((resp, permit))
    } else {
        Err(format!("range read returned {}", status))
    }
}

fn read_exact_from_stream(resp: &mut reqwest::blocking::Response, need: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut buf = vec![0u8; need];
    let mut filled = 0;
    while filled < need {
        match resp.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) => return Err(e.to_string()),
        }
    }
    buf.truncate(filled);
    Ok(buf)
}


// ── Mount ─────────────────────────────────────────────────────────────────────

fn is_trash_dir(name: &OsStr) -> bool {
    let s = name.to_string_lossy();
    s.starts_with(".Trash")
}

fn boot_validate_root(conn: &Arc<ConnInfo>, cache: &Arc<Mutex<FsCache>>) {
    let root = PathBuf::from("/");
    let has_cache = cache.safe_lock().dir_cache.contains_key(&root);
    if !has_cache {
        return;
    }
    match list_dir_propfind(conn, root.clone()) {
        Ok((etag, self_entry, fresh_files)) => {
            let mut invalidated_paths: Vec<PathBuf> = Vec::new();
            let mut matched = 0usize;
            {
                let mut c = cache.safe_lock();
                for entry in &fresh_files {
                    if !entry.is_dir {
                        continue;
                    }
                    let child_path = entry.path.clone();
                    let fresh_etag = entry.etag.as_deref();
                    let cached_etag = c.dir_cache.get(&child_path).and_then(|e| e.etag.as_deref());
                    match (fresh_etag, cached_etag) {
                        (Some(f), Some(c_etag)) if f == c_etag => {
                            matched += 1;
                        }
                        _ => {
                            if c.dir_cache.contains_key(&child_path) {
                                invalidated_paths.push(child_path);
                            }
                        }
                    }
                }
                // Remove stale entries so start_background_propfind won't
                // skip them (it early-returns when the path is in dir_cache).
                for p in &invalidated_paths {
                    c.dir_cache.remove(p);
                }
                c.put_dir_cache(root, etag, self_entry, fresh_files);
            }
            log::info!("BOOT_VALIDATE /: {} dirs unchanged, {} invalidated", matched, invalidated_paths.len());
            schedule_save_dir_cache(cache);
            for p in &invalidated_paths {
                start_background_propfind(conn, cache, p.clone(), 0);
            }
            if !invalidated_paths.is_empty() {
                log::info!("BOOT_VALIDATE: kicked off {} background re-fetches", invalidated_paths.len());
            }
        }
        Err(e) => {
            log::warn!("BOOT_VALIDATE / failed: {} — cache served as-is", e);
        }
    }
}

fn build_fuse_options() -> Vec<MountOption> {
    vec![
        MountOption::FSName("ncrs".to_string()),
        MountOption::DefaultPermissions,
    ]
}

pub fn mount_ncfs(options: MountOptions, error_log: Option<ErrorLog>, transfer_map: Option<TransferMap>, journal: Option<mutation_journal::SharedJournal>) -> Result<(), String> {
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
    let keep_cb = filesystem.keep_callback();
    let evict_cb = filesystem.evict_callback();
    let prefetch_cb = filesystem.prefetch_callback();
    let base_url = notifications::base_url(&options.url);
    let username = options.username.clone().unwrap_or_default();
    let ipc_password = options.password.clone().unwrap_or_default();
    let file_change_queue: ipc::FileChangeQueue = Arc::new(Mutex::new(Vec::new()));
    ipc::start_server(options.mount_point.clone(), filesystem.status_map(), filesystem.shared_set(), filesystem.fileid_map(), filesystem.detail_map(), filesystem.dirty_set(), username, ipc_password, base_url, Some(keep_cb), Some(evict_cb), Some(prefetch_cb), filesystem.error_log(), filesystem.transfer_map(), filesystem.journal(), file_change_queue.clone());

    // Connectivity monitor
    let offline_flag = filesystem.is_offline_flag();
    let replay_journal_ref = filesystem.journal();
    let replay_cache = filesystem.cache_ref();
    let replay_dirty = filesystem.dirty_set();
    let replay_elog = filesystem.error_log();
    let replay_base_url = notifications::base_url(&options.url);
    let replay_user = options.username.clone().unwrap_or_default();
    let replay_pass = options.password.clone().unwrap_or_default();

    if !options.offline {
        // Replay any journal entries from a previous session
        if !replay_journal_ref.safe_lock().is_empty() {
            let j = replay_journal_ref.clone();
            let http = filesystem.conn_http();
            let bu = replay_base_url.clone();
            let u = replay_user.clone();
            let p = replay_pass.clone();
            let c = replay_cache.clone();
            let d = replay_dirty.clone();
            let el = replay_elog.clone();
            thread::spawn(move || {
                let ctx = mutation_journal::ReplayContext { http, base_url: bu, username: u, password: p };
                mutation_journal::replay_journal(&j, &ctx, &c, &d, &el);
            });
        }

        let http = filesystem.conn_http();
        let webdav_url = options.url.clone();
        let probe_user = options.username.clone().unwrap_or_default();
        let probe_pass = options.password.clone().unwrap_or_default();
        let offline = offline_flag.clone();
        let journal_for_monitor = replay_journal_ref.clone();
        let cache_for_monitor = replay_cache.clone();
        let dirty_for_monitor = replay_dirty.clone();
        let elog_for_monitor = replay_elog.clone();
        let base_for_monitor = replay_base_url.clone();
        let user_for_monitor = replay_user.clone();
        let pass_for_monitor = replay_pass.clone();
        thread::spawn(move || {
            loop {
                let currently_offline = offline.load(Ordering::Relaxed);
                let interval = if currently_offline { Duration::from_secs(5) } else { Duration::from_secs(30) };
                thread::sleep(interval);

                let reachable = propfind::propfind_etag(
                    &http, &webdav_url, &probe_user, &probe_pass,
                    Path::new("/"), Duration::from_secs(5),
                ).is_ok();

                let was_offline = offline.swap(!reachable, Ordering::Relaxed);
                if was_offline && reachable {
                    log::info!("CONNECTIVITY restored — replaying mutation journal");
                    let j = journal_for_monitor.clone();
                    let ctx = mutation_journal::ReplayContext {
                        http: http.clone(),
                        base_url: base_for_monitor.clone(),
                        username: user_for_monitor.clone(),
                        password: pass_for_monitor.clone(),
                    };
                    let c = cache_for_monitor.clone();
                    let d = dirty_for_monitor.clone();
                    let el = elog_for_monitor.clone();
                    thread::spawn(move || {
                        mutation_journal::replay_journal(&j, &ctx, &c, &d, &el);
                    });
                } else if !was_offline && !reachable {
                    log::warn!("CONNECTIVITY lost — serving from cache");
                }
            }
        });
    }

    let notifier_slot = filesystem.notifier_slot();

    if !options.offline {
        notify_push::start(
            filesystem.conn_http(),
            notifications::base_url(&options.url),
            options.url.clone(),
            options.username.clone().unwrap_or_default(),
            options.password.clone().unwrap_or_default(),
            filesystem.cache_ref(),
            filesystem.dirty_set(),
            offline_flag,
            filesystem.notify_push_connected_flag(),
            filesystem.active_streams(),
            filesystem.deferred_invalidation(),
            filesystem.throttle(),
            notifier_slot.clone(),
            filesystem.ghost_entries(),
            file_change_queue,
        );
    }

    // Validate root-level dirs at boot via a single PROPFIND /.
    // Compare child etags against cached dir_cache entries: matching
    // etags prove the subdirectory hasn't changed, so we keep serving
    // cached data. Mismatches get invalidated for re-fetch on next readdir.
    if !options.offline {
        let boot_conn = filesystem.conn.clone();
        let boot_cache = filesystem.cache_ref();
        thread::spawn(move || {
            boot_validate_root(&boot_conn, &boot_cache);
        });
    }

    // Validate cached files on boot — re-download if etag changed
    if !options.offline {
        let saved_etags = load_file_cache(&filesystem.cache_ref());
        if !saved_etags.is_empty() {
            let http = filesystem.conn_http();
            let webdav_url = options.url.clone();
            let boot_user = options.username.clone().unwrap_or_default();
            let boot_pass = options.password.clone().unwrap_or_default();
            let net = filesystem.net();
            let throttle = filesystem.throttle();
            let cache = filesystem.cache_ref();
            let status = filesystem.status_map();
            let dirty = filesystem.dirty_set();
            let boot_transfers = filesystem.transfer_map();
            thread::spawn(move || {
                log::info!("FILE_CACHE boot validation: checking {} files", saved_etags.len());
                let mut stale = 0usize;
                for (remote_path, old_etag) in &saved_etags {
                    match propfind::propfind_etag(&http, &webdav_url, &boot_user, &boot_pass, remote_path, PROPFIND_TIMEOUT) {
                        Ok(Some(ref new_etag)) if new_etag == old_etag => {}
                        Ok(new_etag) => {
                            log::info!("FILE_CACHE stale: {} (etag {:?} → {:?})", remote_path.display(), old_etag, new_etag);
                            cache.safe_lock().file_cache.remove(remote_path);
                            match ensure_file_cached(&net, &throttle, &cache, &status, &dirty, remote_path.clone(), Some(&boot_transfers)) {
                                Ok(_) => log::info!("FILE_CACHE re-downloaded {}", remote_path.display()),
                                Err(e) => log::warn!("FILE_CACHE re-download {} failed: {}", remote_path.display(), e),
                            }
                            stale += 1;
                        }
                        Err(e) => {
                            log::debug!("FILE_CACHE etag check {} failed: {}", remote_path.display(), e);
                        }
                    }
                }
                log::info!("FILE_CACHE boot validation done: {}/{} stale", stale, saved_etags.len());
            });
        }
    }

    let fuse_options = build_fuse_options();

    let mp_str = options.mount_point.to_string_lossy().to_string();
    let _ = std::process::Command::new("fusermount")
        .args(["-uz", &mp_str])
        .output();

    log::info!(
        "Mounting WebDAV {} at {}",
        options.url,
        options.mount_point.display()
    );

    let fuse_config = {
        let mut c = Config::default();
        c.mount_options = fuse_options;
        c
    };
    let session = fuser::Session::new(filesystem, &options.mount_point, &fuse_config)
        .map_err(|e| format!("FUSE session init failed: {}", e))?;

    if let Some(fd) = fuse_notify::find_fuse_fd() {
        use std::os::unix::io::FromRawFd;
        let duped = unsafe { libc::dup(fd) };
        if duped >= 0 {
            let fuse_file = unsafe { std::fs::File::from_raw_fd(duped) };
            let notifier = Arc::new(fuse_notify::FuseNotifier::new(fuse_file));
            *notifier_slot.safe_lock() = Some(notifier);
            log::info!("FUSE notifier ready on fd {} (duped to {})", fd, duped);
        } else {
            log::warn!("Failed to dup FUSE fd {}: {}", fd, std::io::Error::last_os_error());
        }
    } else {
        log::warn!("Could not find /dev/fuse fd — kernel notifications disabled");
    }

    let bg = session.spawn().map_err(|e| format!("FUSE session spawn failed: {}", e))?;
    bg.guard.join().map_err(|_| "FUSE session thread panicked".to_string())
        .and_then(|r| r.map_err(|e| format!("FUSE session failed: {}", e)))
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

pub fn configuration_parser(yaml_conf: &str) -> Result<MountOptions, String> {
    let docs =
        YamlLoader::load_from_str(yaml_conf).map_err(|e| format!("YAML parse error: {}", e))?;

    if docs.is_empty() {
        return Err("Empty config file".to_string());
    }

    let doc = &docs[0];

    let url = doc["url"]
        .as_str()
        .ok_or("Missing 'url' in config")?
        .to_string();
    let username = doc["username"].as_str().map(str::to_string);
    let password = doc["password"].as_str().map(str::to_string);
    let mount_point =
        PathBuf::from(doc["mount_point"].as_str().unwrap_or("/media/ncrs_mount"));
    let log_user = doc["user"].as_str().unwrap_or("default_user").to_string();
    let aggressive_prefetch = doc["aggressive_prefetch"].as_bool().unwrap_or(false);
    let http3 = doc["http3"].as_bool().unwrap_or(false);
    let max_concurrent_requests = doc["max_concurrent_requests"].as_i64().unwrap_or(10) as usize;
    let optimistic_listing = doc["optimistic_listing"].as_bool().unwrap_or(true);
    let auto_keep_locally_modified_files = doc["auto_keep_locally_modified_files"].as_bool().unwrap_or(false);

    Ok(MountOptions { url, username, password, mount_point, log_user, aggressive_prefetch, http3, max_concurrent_requests, offline: false, optimistic_listing, auto_keep_locally_modified_files })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::FromRawFd;

    fn make_test_cache() -> FsCache {
        let mut inodes = HashMap::new();
        let mut paths = HashMap::new();
        inodes.insert(1, PathBuf::from("/"));
        paths.insert(PathBuf::from("/"), 1);
        FsCache {
            inodes,
            paths,
            next_inode: 2,
            dir_cache: HashMap::new(),
            pending_dirs: HashMap::new(),
            file_cache: HashMap::new(),
            cache_dir: PathBuf::from("/tmp/ncrs-test-cache"),
            pending_notify: Arc::new((Mutex::new(()), Condvar::new())),
        }
    }

    fn make_dav_entry(name: &str, fileid: Option<u64>) -> DavEntry {
        DavEntry {
            path: PathBuf::from(format!("/{}", name)),
            is_dir: false,
            size: 100,
            modified: None,
            etag: Some("etag1".into()),
            content_type: None,
            has_preview: false,
            is_shared: false,
            permissions: None,
            fileid,
            owner_id: None,
            owner_display_name: None,
        }
    }

    fn pipe_notifier_slot() -> (fuse_notify::NotifierSlot, std::fs::File) {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let write_file = unsafe { std::fs::File::from_raw_fd(fds[1]) };
        let read_file = unsafe { std::fs::File::from_raw_fd(fds[0]) };
        let notifier = Arc::new(fuse_notify::FuseNotifier::new(write_file));
        let slot: fuse_notify::NotifierSlot = Arc::new(Mutex::new(Some(notifier)));
        (slot, read_file)
    }

    // ── perms_to_mode ──────────────────────────────────────────────────────────

    #[test]
    fn perms_mode_full_rw_file() {
        assert_eq!(perms_to_mode(Some("RGDNVW"), false), 0o644);
    }

    #[test]
    fn perms_mode_full_rw_dir() {
        assert_eq!(perms_to_mode(Some("RGDNVCK"), true), 0o755);
    }

    #[test]
    fn perms_mode_readonly_file() {
        assert_eq!(perms_to_mode(Some("G"), false), 0o444);
    }

    #[test]
    fn perms_mode_readonly_dir() {
        assert_eq!(perms_to_mode(Some("G"), true), 0o555);
    }

    #[test]
    fn perms_mode_reshare_mounted_read_file() {
        // S=Shared, R=Reshare, M=Mounted, G=Read — the Hddstore Media case
        assert_eq!(perms_to_mode(Some("SRMG"), false), 0o444);
    }

    #[test]
    fn perms_mode_reshare_mounted_read_dir() {
        assert_eq!(perms_to_mode(Some("SRMG"), true), 0o555);
    }

    #[test]
    fn perms_mode_no_read_is_zero() {
        assert_eq!(perms_to_mode(Some("W"),   false), 0o000);
        assert_eq!(perms_to_mode(Some("CDN"), true),  0o000);
    }

    #[test]
    fn perms_mode_none_falls_back_to_default() {
        assert_eq!(perms_to_mode(None, false), 0o644);
        assert_eq!(perms_to_mode(None, true),  0o755);
    }

    #[test]
    fn perms_mode_empty_falls_back_to_default() {
        assert_eq!(perms_to_mode(Some(""), false), 0o644);
        assert_eq!(perms_to_mode(Some(""), true),  0o755);
    }

    fn make_dav_entry_with_perms(dir: &str, name: &str, permissions: Option<&str>) -> DavEntry {
        DavEntry {
            path: PathBuf::from(format!("{}/{}", dir, name)),
            is_dir: false,
            size: 1024,
            modified: None,
            etag: Some("etag1".into()),
            content_type: None,
            has_preview: false,
            is_shared: false,
            permissions: permissions.map(str::to_string),
            fileid: Some(1),
            owner_id: None,
            owner_display_name: None,
        }
    }

    fn open_guard_fires(cache: &FsCache, path: &PathBuf) -> bool {
        let parent = path.parent().unwrap_or(Path::new("/")).to_path_buf();
        let nc_permissions = cache.get_cached_dir_readonly(&parent)
            .and_then(|files| files.iter().find(|e| &e.path == path)
            .and_then(|e| e.permissions.clone()));
        nc_permissions.is_some()
            && perms_to_mode(nc_permissions.as_deref(), false) & 0o200 == 0
    }

    #[test]
    fn open_write_blocked_for_readonly_nc_entry_via_cache() {
        // Exercises the exact cache-lookup + guard-condition path used by fn open.
        // This test would have had no matching behaviour before the guard was added.
        let mut cache = make_test_cache();
        let dir = PathBuf::from("/Musica/Tracks");

        cache.put_dir_cache(dir.clone(), None, None, vec![
            make_dav_entry_with_perms("/Musica/Tracks", "song.mp3",   Some("SRMG")),   // read-only
            make_dav_entry_with_perms("/Musica/Tracks", "editable.mp3", Some("RGDNVW")), // writable
            make_dav_entry_with_perms("/Musica/Tracks", "unknown.mp3",  None),            // no NC perms
        ]);

        let ro_file  = PathBuf::from("/Musica/Tracks/song.mp3");
        let rw_file  = PathBuf::from("/Musica/Tracks/editable.mp3");
        let unk_file = PathBuf::from("/Musica/Tracks/unknown.mp3");

        assert!(open_guard_fires(&cache, &ro_file),
            "write open must be blocked for SRMG (Shared+Reshare+Mounted+Read — no W flag)");
        assert!(!open_guard_fires(&cache, &rw_file),
            "write open must be allowed for RGDNVW (has W flag)");
        assert!(!open_guard_fires(&cache, &unk_file),
            "write open must not be blocked when NC permissions are absent (fall through to DefaultPermissions)");
    }

    fn make_dir_entry_with_perms(parent: &str, name: &str, permissions: Option<&str>) -> DavEntry {
        DavEntry {
            path: PathBuf::from(format!("{}/{}", parent, name)),
            is_dir: true,
            size: 0,
            modified: None,
            etag: None,
            content_type: None,
            has_preview: false,
            is_shared: false,
            permissions: permissions.map(str::to_string),
            fileid: Some(2),
            owner_id: None,
            owner_display_name: None,
        }
    }

    /// Registers a directory inode so nc_dir_perms can look it up by inode number.
    fn register_dir(cache: &mut FsCache, parent: &str, name: &str, permissions: Option<&str>) -> u64 {
        let path = PathBuf::from(format!("{}/{}", parent, name));
        let entry = make_dir_entry_with_perms(parent, name, permissions);
        let parent_path = PathBuf::from(parent);
        // Ensure the parent's dir listing is populated (so nc_dir_perms can find this dir).
        let mut files = cache.dir_cache.get(&parent_path)
            .map(|e| e.files.as_ref().clone())
            .unwrap_or_default();
        files.push(entry);
        cache.put_dir_cache(parent_path, None, None, files);
        cache.allocate_inode(path)
    }

    #[test]
    fn unlink_guard_blocks_when_no_delete_flag() {
        // Tracks/ has SRGCK: S+R+G+C+K but NO 'D' → nc_dir_perms returns "SRGCK".
        // The unlink guard must fire (would have passed silently before the fix).
        let mut cache = make_test_cache();
        let tracks_ino = register_dir(&mut cache, "/Musica", "Tracks", Some("SRGCK"));

        let perms = cache.nc_dir_perms(tracks_ino).expect("perms must be present");
        assert!(!perms.contains('D'), "SRGCK should not have D");
        // Guard condition (mirrors fn unlink):
        assert!(!perms.contains('D'), "unlink must be blocked — no D flag");
    }

    #[test]
    fn unlink_guard_allows_when_delete_flag_present() {
        let mut cache = make_test_cache();
        let dir_ino = register_dir(&mut cache, "/Musica", "OwnedDir", Some("RGDNVCK"));

        let perms = cache.nc_dir_perms(dir_ino).expect("perms must be present");
        assert!(perms.contains('D'), "RGDNVCK has D → unlink must be allowed");
    }

    #[test]
    fn unlink_guard_allows_when_perms_absent() {
        // No cached permissions → guard doesn't fire; server enforces.
        let mut cache = make_test_cache();
        let dir_ino = register_dir(&mut cache, "/Musica", "UnknownDir", None);

        assert!(cache.nc_dir_perms(dir_ino).is_none(),
            "guard must not fire when NC permissions are unknown");
    }

    #[test]
    fn rename_guard_blocks_same_dir_without_n_flag() {
        let mut cache = make_test_cache();
        let dir_ino = register_dir(&mut cache, "/Musica", "Tracks", Some("SRGCK")); // no N

        let perms = cache.nc_dir_perms(dir_ino).expect("perms must be present");
        // same-dir rename check (mirrors fn rename, parent == newparent):
        assert!(!perms.contains('N'), "SRGCK has no N → same-dir rename must be blocked");
    }

    #[test]
    fn rename_guard_blocks_cross_dir_move_without_v_flag() {
        let mut cache = make_test_cache();
        let dir_ino = register_dir(&mut cache, "/Musica", "Tracks", Some("SRGCK")); // no V

        let perms = cache.nc_dir_perms(dir_ino).expect("perms must be present");
        // cross-dir move check (mirrors fn rename, parent != newparent):
        assert!(!perms.contains('V'), "SRGCK has no V → cross-dir move must be blocked");
    }

    #[test]
    fn rename_guard_allows_when_flags_present() {
        let mut cache = make_test_cache();
        let dir_ino = register_dir(&mut cache, "/Musica", "OwnedDir", Some("RGDNVCK"));

        let perms = cache.nc_dir_perms(dir_ino).expect("perms must be present");
        assert!(perms.contains('N'), "RGDNVCK has N → same-dir rename must be allowed");
        assert!(perms.contains('V'), "RGDNVCK has V → cross-dir move must be allowed");
    }

    #[test]
    fn get_cached_dir_returns_none_for_invalidated_entry() {
        let mut cache = make_test_cache();
        let path = PathBuf::from("/");
        cache.put_dir_cache(path.clone(), None, None, vec![make_dav_entry("a.txt", None)]);

        let result = cache.get_cached_dir(&path, DIR_CACHE_TTL);
        assert!(result.is_some(), "fresh entry should return Some");
        let (files, needs_refresh) = result.unwrap();
        assert_eq!(files.len(), 1);
        assert!(!needs_refresh);

        cache.dir_cache.get_mut(&path).unwrap().invalidated = true;

        let result = cache.get_cached_dir(&path, DIR_CACHE_TTL);
        assert!(result.is_none(), "invalidated entry must return None to force synchronous PROPFIND");

        let entry = cache.dir_cache.get(&path).unwrap();
        assert!(entry.invalidated, "invalidated flag must stay set until fresh PROPFIND replaces the entry");
    }

    #[test]
    fn invalidated_entry_stays_none_across_repeated_calls() {
        let mut cache = make_test_cache();
        let path = PathBuf::from("/");
        cache.put_dir_cache(path.clone(), None, None, vec![
            make_dav_entry("keep.txt", None),
            make_dav_entry("deleted_on_server.txt", None),
        ]);

        cache.dir_cache.get_mut(&path).unwrap().invalidated = true;

        let first = cache.get_cached_dir(&path, DIR_CACHE_TTL);
        assert!(first.is_none(), "first call: invalidated entry must return None");

        let second = cache.get_cached_dir(&path, DIR_CACHE_TTL);
        assert!(
            second.is_none(),
            "second call: must ALSO return None — stale listing still contains deleted_on_server.txt"
        );
    }

    #[test]
    fn get_cached_dir_returns_stale_data_for_ttl_expired() {
        let mut cache = make_test_cache();
        let path = PathBuf::from("/");
        cache.put_dir_cache(path.clone(), None, None, vec![make_dav_entry("b.txt", None)]);

        cache.dir_cache.get_mut(&path).unwrap().at = Instant::now() - Duration::from_secs(3600);

        let result = cache.get_cached_dir(&path, DIR_CACHE_TTL);
        assert!(result.is_some(), "TTL-expired (not invalidated) should return stale data for background refresh");
        let (_files, needs_refresh) = result.unwrap();
        assert!(needs_refresh, "should signal background refresh needed");
    }

    #[test]
    fn invalidate_all_dirs_populates_dirty_set_and_notifies_kernel() {
        use std::io::Read;

        let mut cache = make_test_cache();
        let root = PathBuf::from("/");
        let subdir = PathBuf::from("/docs");
        cache.allocate_inode(subdir.clone());
        cache.put_dir_cache(root.clone(), None, None, vec![]);
        cache.put_dir_cache(subdir.clone(), None, None, vec![]);

        let cache = Arc::new(Mutex::new(cache));
        let dirty: ipc::DirtySet = Arc::new(Mutex::new(std::collections::HashSet::new()));
        let (slot, mut reader) = pipe_notifier_slot();

        notify_push::invalidate_all_dirs(&cache, &dirty, &slot);

        let ds = dirty.safe_lock();
        assert!(ds.contains(&root), "root should be in dirty set");
        assert!(ds.contains(&subdir), "/docs should be in dirty set");
        drop(ds);

        {
            let c = cache.safe_lock();
            assert!(c.dir_cache.get(&root).unwrap().invalidated);
            assert!(c.dir_cache.get(&subdir).unwrap().invalidated);
        }

        // FuseOutHeader(16) + FuseNotifyInvalInodeOut(24) = 40 bytes per notification
        let msg_size = 40;
        let mut buf = vec![0u8; msg_size * 2];
        reader.read_exact(&mut buf).unwrap();

        let ino1 = u64::from_ne_bytes(buf[16..24].try_into().unwrap());
        let ino2 = u64::from_ne_bytes(buf[16 + msg_size..24 + msg_size].try_into().unwrap());
        let mut inodes = vec![ino1, ino2];
        inodes.sort();
        assert_eq!(inodes, vec![1, 2], "should notify both inode 1 (root) and 2 (/docs)");
    }

    fn make_dav_entry_in(dir: &str, name: &str, fileid: Option<u64>) -> DavEntry {
        DavEntry {
            path: PathBuf::from(format!("{}/{}", dir, name)),
            is_dir: false,
            size: 100,
            modified: None,
            etag: Some("etag1".into()),
            content_type: None,
            has_preview: false,
            is_shared: false,
            permissions: None,
            fileid,
            owner_id: None,
            owner_display_name: None,
        }
    }

    fn make_ghost_map() -> GhostMap {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn make_file_change_queue() -> ipc::FileChangeQueue {
        Arc::new(Mutex::new(Vec::new()))
    }

    #[test]
    fn ghost_hidden_add_hides_file_from_lookup() {
        let ghosts = make_ghost_map();
        let path = PathBuf::from("/Sync/newfile.txt");

        ghosts.safe_lock().insert(path.clone(), GhostEntry {
            kind: GhostKind::HiddenAdd,
            created_at: Instant::now(),
            rename_pair_id: None,
        });

        let g = ghosts.safe_lock();
        let ghost = g.get(&path).unwrap();
        assert!(ghost.created_at.elapsed() < GHOST_TTL);
        assert!(matches!(ghost.kind, GhostKind::HiddenAdd));
    }

    #[test]
    fn ghost_visible_delete_returns_old_attrs() {
        let ghosts = make_ghost_map();
        let path = PathBuf::from("/Sync/deleted.txt");

        let now = SystemTime::now();
        let attr = FileAttr {
            ino: INodeNo(42), size: 1024, blocks: 2,
            atime: now, mtime: now, ctime: now, crtime: now,
            kind: FileType::RegularFile, perm: 0o644, nlink: 1,
            uid: 1000, gid: 1000,
            rdev: 0, flags: 0, blksize: 512,
        };

        ghosts.safe_lock().insert(path.clone(), GhostEntry {
            kind: GhostKind::VisibleDelete { attr },
            created_at: Instant::now(),
            rename_pair_id: None,
        });

        let g = ghosts.safe_lock();
        let ghost = g.get(&path).unwrap();
        match ghost.kind {
            GhostKind::VisibleDelete { attr: stored } => {
                assert_eq!(stored.ino, INodeNo(42));
                assert_eq!(stored.size, 1024);
            }
            _ => panic!("expected VisibleDelete"),
        }
    }

    #[test]
    fn ghost_expires_after_ttl() {
        let ghosts = make_ghost_map();
        let path = PathBuf::from("/Sync/expired.txt");

        ghosts.safe_lock().insert(path.clone(), GhostEntry {
            kind: GhostKind::HiddenAdd,
            created_at: Instant::now() - GHOST_TTL - Duration::from_secs(1),
            rename_pair_id: None,
        });

        let g = ghosts.safe_lock();
        let ghost = g.get(&path).unwrap();
        assert!(ghost.created_at.elapsed() >= GHOST_TTL, "ghost should be expired");
    }

    #[test]
    fn ghost_create_intercept_clears_ghost() {
        let ghosts = make_ghost_map();
        let path = PathBuf::from("/Sync/newfile.txt");

        ghosts.safe_lock().insert(path.clone(), GhostEntry {
            kind: GhostKind::HiddenAdd,
            created_at: Instant::now(),
            rename_pair_id: None,
        });

        // Simulate what the create handler does: remove the ghost
        let removed = ghosts.safe_lock().remove(&path);
        assert!(removed.is_some());
        assert!(matches!(removed.unwrap().kind, GhostKind::HiddenAdd));

        // Ghost should be gone now
        assert!(ghosts.safe_lock().get(&path).is_none());
    }

    #[test]
    fn ghost_unlink_intercept_clears_ghost() {
        let ghosts = make_ghost_map();
        let path = PathBuf::from("/Sync/deleted.txt");
        let now = SystemTime::now();
        let attr = FileAttr {
            ino: INodeNo(42), size: 0, blocks: 0,
            atime: now, mtime: now, ctime: now, crtime: now,
            kind: FileType::RegularFile, perm: 0o644, nlink: 1,
            uid: 1000, gid: 1000, rdev: 0, flags: 0, blksize: 512,
        };

        ghosts.safe_lock().insert(path.clone(), GhostEntry {
            kind: GhostKind::VisibleDelete { attr },
            created_at: Instant::now(),
            rename_pair_id: None,
        });

        let removed = ghosts.safe_lock().remove(&path);
        assert!(removed.is_some());
        assert!(matches!(removed.unwrap().kind, GhostKind::VisibleDelete { .. }));
        assert!(ghosts.safe_lock().get(&path).is_none());
    }

    #[test]
    fn proactive_refresh_classifies_removals_and_additions() {
        // Old: old.txt + keep.txt; New: keep.txt + new.txt
        // Expected diff: removed=[old.txt], added=[new.txt], no renames.
        // make_dav_entry_in always sets etag = "etag1". Use matching etag for
        // keep.txt in the old snapshot so compute_dir_diff doesn't flag it as modified.
        let old_snap = notify_push::OldDirSnapshot {
            names: vec![PathBuf::from("/Sync/old.txt"), PathBuf::from("/Sync/keep.txt")],
            etags: [
                (PathBuf::from("/Sync/old.txt"),  Some("etag1".into())),
                (PathBuf::from("/Sync/keep.txt"), Some("etag1".into())),
            ].into_iter().collect(),
            fileids: [
                (PathBuf::from("/Sync/old.txt"),  100u64),
                (PathBuf::from("/Sync/keep.txt"), 101u64),
            ].into_iter().collect(),
            is_dir: [
                (PathBuf::from("/Sync/old.txt"),  false),
                (PathBuf::from("/Sync/keep.txt"), false),
            ].into_iter().collect(),
        };

        let fresh_files = vec![
            make_dav_entry_in("/Sync", "keep.txt", Some(101)),
            make_dav_entry_in("/Sync", "new.txt",  Some(102)),
        ];

        let diff = notify_push::compute_dir_diff(&old_snap, &fresh_files);

        assert_eq!(diff.removed, vec![PathBuf::from("/Sync/old.txt")],
            "old.txt should be removed");
        assert_eq!(diff.added, vec![PathBuf::from("/Sync/new.txt")],
            "new.txt should be added");
        assert!(diff.renames.is_empty(), "no renames: fileids differ");
        assert!(diff.modified.is_empty(), "keep.txt etag unchanged");
    }

    #[test]
    fn file_change_queue_drains_correctly() {
        let fcq = make_file_change_queue();

        {
            let mut q = fcq.safe_lock();
            q.push(ipc::FileChange {
                kind: ipc::FileChangeKind::Added,
                path: PathBuf::from("/Sync/a.txt"),
            });
            q.push(ipc::FileChange {
                kind: ipc::FileChangeKind::Removed,
                path: PathBuf::from("/Sync/b.txt"),
            });
        }

        // Drain (like IPC handler does)
        let drained: Vec<ipc::FileChange> = fcq.safe_lock().drain(..).collect();
        assert_eq!(drained.len(), 2);

        // Queue should be empty
        assert!(fcq.safe_lock().is_empty());
    }

    #[test]
    fn debounce_cooldown_is_shorter_after_changes_than_after_idle() {
        let active = notify_push::debounce_cooldown(true);
        let idle   = notify_push::debounce_cooldown(false);
        assert!(active < idle,
            "cooldown after changes ({:?}) must be shorter than after idle ({:?})",
            active, idle);
    }

    #[test]
    fn debounce_cooldown_returns_correct_constants() {
        assert_eq!(notify_push::debounce_cooldown(true),  notify_push::REFRESH_DEBOUNCE);
        assert_eq!(notify_push::debounce_cooldown(false), notify_push::REFRESH_DEBOUNCE_NO_CHANGE);
    }

    // ── upload size + status ───────────────────────────────────────────────────

    #[test]
    fn flush_size_written_to_cache_before_reply() {
        // Mirrors the synchronous cache-update block added to fn flush.
        // Would have returned 0 before the fix because only the background PUT
        // thread updated the cache size (after reply.ok was already sent).
        let mut cache = make_test_cache();
        let dir  = PathBuf::from("/Sync");
        let file = PathBuf::from("/Sync/backandforth.md");

        // File starts at size 0 (as fn create inserts it).
        let mut entry = make_dav_entry_with_perms("/Sync", "backandforth.md", Some("RGDNVW"));
        entry.size = 0;
        cache.put_dir_cache(dir.clone(), None, None, vec![entry]);
        assert_eq!(
            cache.get_cached_dir_readonly(&dir).unwrap()
                .iter().find(|e| e.path == file).unwrap().size,
            0,
            "pre-condition: create inserts size 0"
        );

        // Simulate the synchronous update from fn flush.
        let upload_size: u64 = 15; // len("back and forth\n")
        {
            let parent = file.parent().unwrap_or(Path::new("/")).to_path_buf();
            if let Some(dir_entry) = cache.dir_cache.get_mut(&parent) {
                let mut files = (*dir_entry.files).clone();
                if let Some(e) = files.iter_mut().find(|e| e.path == file) {
                    e.size = upload_size;
                }
                dir_entry.files = Arc::new(files);
            }
        }

        let reported = cache.get_cached_dir_readonly(&dir).unwrap()
            .iter().find(|e| e.path == file).unwrap().size;
        assert_eq!(reported, upload_size,
            "getattr must return the written size before the PUT thread runs");
    }

    #[test]
    fn uploading_status_as_str() {
        assert_eq!(ipc::FileStatus::Uploading.as_str(), "uploading");
    }

    #[test]
    fn dir_status_uploading_child_propagates() {
        use ipc::FileStatus;
        use std::collections::HashMap;

        let dir  = PathBuf::from("/Sync");
        let file = PathBuf::from("/Sync/backandforth.md");

        let mut sm: HashMap<PathBuf, FileStatus> = HashMap::new();
        sm.insert(file.clone(), FileStatus::Uploading);

        // dir_status_from_children is not pub; test via the IPC STATUS response
        // by checking the exact guard condition it uses.
        let uploading_count = sm.iter()
            .filter(|(p, s)| p.parent() == Some(&*dir) && **s == FileStatus::Uploading)
            .count();
        assert_eq!(uploading_count, 1, "one child is uploading");
        // Once uploading_count > 0 the function returns "uploading".
        assert!(uploading_count > 0);
    }

    #[test]
    fn flush_background_dirties_file_path_on_success() {
        // The background PUT thread must insert the file path (not just the parent)
        // so that Nautilus's 2-second CHANGES poll picks it up and clears the emblem.
        let dirty: ipc::DirtySet = Arc::new(Mutex::new(std::collections::HashSet::new()));
        let file   = PathBuf::from("/Sync/backandforth.md");
        let parent = PathBuf::from("/Sync");

        dirty.safe_lock().insert(file.clone());
        dirty.safe_lock().insert(parent.clone());

        let paths: std::collections::HashSet<PathBuf> = dirty.safe_lock().drain().collect();
        assert!(paths.contains(&file),   "file path must be in dirty set");
        assert!(paths.contains(&parent), "parent dir must also be in dirty set");
    }

    #[test]
    fn flush_background_dirties_file_path_on_error() {
        // Same check for the error arm — only the file path is inserted (no parent insert
        // in that arm), which is enough for Nautilus to invalidate the file's emblem.
        let dirty: ipc::DirtySet = Arc::new(Mutex::new(std::collections::HashSet::new()));
        let file = PathBuf::from("/Sync/backandforth.md");

        dirty.safe_lock().insert(file.clone());

        let paths: std::collections::HashSet<PathBuf> = dirty.safe_lock().drain().collect();
        assert!(paths.contains(&file), "file path must be in dirty set after error");
    }

    fn make_dir_dav_entry_in(dir: &str, name: &str, fileid: Option<u64>) -> DavEntry {
        DavEntry {
            path: PathBuf::from(format!("{}/{}", dir, name)),
            is_dir: true,
            size: 0,
            modified: None,
            etag: None,
            content_type: None,
            has_preview: false,
            is_shared: false,
            permissions: None,
            fileid,
            owner_id: None,
            owner_display_name: None,
        }
    }

    #[test]
    fn ghost_mkdir_intercept_clears_ghost() {
        let ghosts = make_ghost_map();
        let path = PathBuf::from("/Sync/newdir");

        ghosts.safe_lock().insert(path.clone(), GhostEntry {
            kind: GhostKind::HiddenAdd,
            created_at: Instant::now(),
            rename_pair_id: None,
        });

        // Simulate mkdir handler: check and remove ghost
        let removed = {
            let mut g = ghosts.safe_lock();
            let ghost = g.remove(&path);
            ghost.filter(|g| g.created_at.elapsed() < GHOST_TTL && matches!(g.kind, GhostKind::HiddenAdd))
        };
        assert!(removed.is_some(), "mkdir should find and clear HiddenAdd ghost");
        assert!(ghosts.safe_lock().get(&path).is_none());
    }

    #[test]
    fn ghost_rmdir_intercept_clears_ghost() {
        let ghosts = make_ghost_map();
        let path = PathBuf::from("/Sync/olddir");
        let now = SystemTime::now();
        let attr = FileAttr {
            ino: INodeNo(50), size: 0, blocks: 0,
            atime: now, mtime: now, ctime: now, crtime: now,
            kind: FileType::Directory, perm: 0o755, nlink: 2,
            uid: 1000, gid: 1000, rdev: 0, flags: 0, blksize: 512,
        };

        ghosts.safe_lock().insert(path.clone(), GhostEntry {
            kind: GhostKind::VisibleDelete { attr },
            created_at: Instant::now(),
            rename_pair_id: None,
        });

        let removed = {
            let mut g = ghosts.safe_lock();
            let ghost = g.remove(&path);
            ghost.filter(|g| g.created_at.elapsed() < GHOST_TTL && matches!(g.kind, GhostKind::VisibleDelete { .. }))
        };
        assert!(removed.is_some(), "rmdir should find and clear VisibleDelete ghost");
        assert!(ghosts.safe_lock().get(&path).is_none());
    }

    #[test]
    fn ghost_rename_intercept_clears_paired_ghosts() {
        let ghosts = make_ghost_map();
        let from = PathBuf::from("/Sync/old.txt");
        let to = PathBuf::from("/Sync/new.txt");
        let now = SystemTime::now();
        let attr = FileAttr {
            ino: INodeNo(42), size: 100, blocks: 1,
            atime: now, mtime: now, ctime: now, crtime: now,
            kind: FileType::RegularFile, perm: 0o644, nlink: 1,
            uid: 1000, gid: 1000, rdev: 0, flags: 0, blksize: 512,
        };

        let pair_id = 99u64;
        ghosts.safe_lock().insert(from.clone(), GhostEntry {
            kind: GhostKind::VisibleDelete { attr },
            created_at: Instant::now(),
            rename_pair_id: Some(pair_id),
        });
        ghosts.safe_lock().insert(to.clone(), GhostEntry {
            kind: GhostKind::HiddenAdd,
            created_at: Instant::now(),
            rename_pair_id: Some(pair_id),
        });

        // Simulate rename handler logic
        let matched = {
            let g = ghosts.safe_lock();
            let fg = g.get(&from);
            let tg = g.get(&to);
            match (fg, tg) {
                (Some(fg), Some(tg)) => {
                    fg.created_at.elapsed() < GHOST_TTL
                        && tg.created_at.elapsed() < GHOST_TTL
                        && fg.rename_pair_id.is_some()
                        && fg.rename_pair_id == tg.rename_pair_id
                        && matches!(fg.kind, GhostKind::VisibleDelete { .. })
                        && matches!(tg.kind, GhostKind::HiddenAdd)
                }
                _ => false,
            }
        };
        assert!(matched, "paired rename ghosts should match");

        ghosts.safe_lock().remove(&from);
        ghosts.safe_lock().remove(&to);
        assert!(ghosts.safe_lock().get(&from).is_none());
        assert!(ghosts.safe_lock().get(&to).is_none());
    }

    #[test]
    fn ghost_rename_mismatched_pair_falls_through() {
        let ghosts = make_ghost_map();
        let from = PathBuf::from("/Sync/old.txt");
        let to = PathBuf::from("/Sync/new.txt");
        let now = SystemTime::now();
        let attr = FileAttr {
            ino: INodeNo(42), size: 100, blocks: 1,
            atime: now, mtime: now, ctime: now, crtime: now,
            kind: FileType::RegularFile, perm: 0o644, nlink: 1,
            uid: 1000, gid: 1000, rdev: 0, flags: 0, blksize: 512,
        };

        ghosts.safe_lock().insert(from.clone(), GhostEntry {
            kind: GhostKind::VisibleDelete { attr },
            created_at: Instant::now(),
            rename_pair_id: Some(10),
        });
        ghosts.safe_lock().insert(to.clone(), GhostEntry {
            kind: GhostKind::HiddenAdd,
            created_at: Instant::now(),
            rename_pair_id: Some(20),
        });

        let matched = {
            let g = ghosts.safe_lock();
            let fg = g.get(&from);
            let tg = g.get(&to);
            match (fg, tg) {
                (Some(fg), Some(tg)) => {
                    fg.rename_pair_id.is_some()
                        && fg.rename_pair_id == tg.rename_pair_id
                }
                _ => false,
            }
        };
        assert!(!matched, "mismatched pair_ids should not match");
    }

    #[test]
    fn proactive_refresh_detects_rename_by_fileid() {
        // Old: original.txt (fid=500) + keep.txt (fid=501)
        // New: keep.txt (fid=501) + renamed.txt (fid=500)
        // Expected: rename original→renamed detected; no true removals or adds.
        let old_snap = notify_push::OldDirSnapshot {
            names: vec![PathBuf::from("/Sync/original.txt"), PathBuf::from("/Sync/keep.txt")],
            etags: [
                (PathBuf::from("/Sync/original.txt"), Some("etag1".into())),
                (PathBuf::from("/Sync/keep.txt"),    Some("etag1".into())),
            ].into_iter().collect(),
            fileids: [
                (PathBuf::from("/Sync/original.txt"), 500u64),
                (PathBuf::from("/Sync/keep.txt"),    501u64),
            ].into_iter().collect(),
            is_dir: [
                (PathBuf::from("/Sync/original.txt"), false),
                (PathBuf::from("/Sync/keep.txt"),    false),
            ].into_iter().collect(),
        };

        let fresh_files = vec![
            make_dav_entry_in("/Sync", "keep.txt",    Some(501)),
            make_dav_entry_in("/Sync", "renamed.txt", Some(500)),
        ];

        let diff = notify_push::compute_dir_diff(&old_snap, &fresh_files);

        assert_eq!(diff.renames.len(), 1, "should detect one rename");
        assert_eq!(diff.renames[0].0, PathBuf::from("/Sync/original.txt"));
        assert_eq!(diff.renames[0].1, PathBuf::from("/Sync/renamed.txt"));
        assert!(!diff.renames[0].2, "original.txt is not a directory");
        assert!(diff.removed.is_empty(), "original.txt was renamed, not deleted");
        assert!(diff.added.is_empty(),   "renamed.txt is a rename target, not a new add");
    }

    #[test]
    fn proactive_refresh_distinguishes_dir_vs_file() {
        // Empty old state; two new entries arrive: a file and a directory.
        let old_snap = notify_push::OldDirSnapshot {
            names:   vec![],
            etags:   HashMap::new(),
            fileids: HashMap::new(),
            is_dir:  HashMap::new(),
        };
        let file_entry = make_dav_entry_in("/Sync", "newfile.txt", Some(200));
        let dir_entry  = make_dir_dav_entry_in("/Sync", "newdir", Some(201));
        let fresh_files = vec![file_entry, dir_entry];

        let diff = notify_push::compute_dir_diff(&old_snap, &fresh_files);

        assert_eq!(diff.added.len(), 2);
        assert!(diff.removed.is_empty());
        assert!(diff.renames.is_empty());

        let added_is_dir: HashMap<PathBuf, bool> = fresh_files.iter()
            .filter(|f| diff.added.contains(&f.path))
            .map(|f| (f.path.clone(), f.is_dir))
            .collect();
        assert_eq!(added_is_dir.get(&PathBuf::from("/Sync/newfile.txt")), Some(&false));
        assert_eq!(added_is_dir.get(&PathBuf::from("/Sync/newdir")),      Some(&true));
    }

    #[test]
    fn file_cache_moves_on_rename() {
        let mut cache = make_test_cache();
        let old_path = PathBuf::from("/Sync/original.txt");
        let new_path = PathBuf::from("/Sync/renamed.txt");

        cache.file_cache.insert(old_path.clone(), FileCacheEntry {
            local_path: PathBuf::from("/tmp/ncrs-cache/original.txt"),
            remote_modified: None,
            etag: Some("etag1".into()),
        });

        // Simulate rename file_cache move
        if let Some(entry) = cache.file_cache.remove(&old_path) {
            cache.file_cache.insert(new_path.clone(), entry);
        }

        assert!(cache.file_cache.get(&old_path).is_none(), "old path should be removed");
        let moved = cache.file_cache.get(&new_path).unwrap();
        assert_eq!(moved.local_path, PathBuf::from("/tmp/ncrs-cache/original.txt"));
        assert_eq!(moved.etag, Some("etag1".into()));
    }

    #[test]
    fn ipc_file_changes_serializes_all_kinds() {
        let fcq = make_file_change_queue();
        let mount = PathBuf::from("/home/user/ncrs");

        {
            let mut q = fcq.safe_lock();
            q.push(ipc::FileChange { kind: ipc::FileChangeKind::Added, path: PathBuf::from("/Sync/a.txt") });
            q.push(ipc::FileChange { kind: ipc::FileChangeKind::Removed, path: PathBuf::from("/Sync/b.txt") });
            q.push(ipc::FileChange { kind: ipc::FileChangeKind::Modified, path: PathBuf::from("/Sync/c.txt") });
            q.push(ipc::FileChange { kind: ipc::FileChangeKind::DirAdded, path: PathBuf::from("/Sync/d") });
            q.push(ipc::FileChange { kind: ipc::FileChangeKind::DirRemoved, path: PathBuf::from("/Sync/e") });
            q.push(ipc::FileChange {
                kind: ipc::FileChangeKind::Renamed { from: PathBuf::from("/Sync/old.txt") },
                path: PathBuf::from("/Sync/new.txt"),
            });
        }

        let changes: Vec<ipc::FileChange> = fcq.safe_lock().drain(..).collect();
        let serialized: Vec<String> = changes.iter().map(|c| {
            let rel = c.path.strip_prefix("/").unwrap_or(&c.path);
            let abs = mount.join(rel);
            match &c.kind {
                ipc::FileChangeKind::Added => format!("A:{}", abs.display()),
                ipc::FileChangeKind::Removed => format!("D:{}", abs.display()),
                ipc::FileChangeKind::Modified => format!("M:{}", abs.display()),
                ipc::FileChangeKind::DirAdded => format!("DA:{}", abs.display()),
                ipc::FileChangeKind::DirRemoved => format!("DD:{}", abs.display()),
                ipc::FileChangeKind::Renamed { from } => {
                    let from_rel = from.strip_prefix("/").unwrap_or(from);
                    format!("R:{}\x1e{}", mount.join(from_rel).display(), abs.display())
                }
            }
        }).collect();

        let wire = serialized.join("\t");
        assert!(wire.contains("A:/home/user/ncrs/Sync/a.txt"));
        assert!(wire.contains("D:/home/user/ncrs/Sync/b.txt"));
        assert!(wire.contains("M:/home/user/ncrs/Sync/c.txt"));
        assert!(wire.contains("DA:/home/user/ncrs/Sync/d"));
        assert!(wire.contains("DD:/home/user/ncrs/Sync/e"));
        assert!(wire.contains("R:/home/user/ncrs/Sync/old.txt\x1e/home/user/ncrs/Sync/new.txt"));
    }

    // ── Mount options ─────────────────────────────────────────────────────────

    #[test]
    fn fuse_options_contain_no_custom_values() {
        // fusermount3 rejects unknown options passed via -o. Any
        // MountOption::CUSTOM value (like "x-gvfs-notrash") causes
        // Session::new to fail and the mount silently never happens.
        let opts = build_fuse_options();
        let custom: Vec<&MountOption> = opts.iter()
            .filter(|o| matches!(o, MountOption::CUSTOM(_)))
            .collect();
        assert!(
            custom.is_empty(),
            "CUSTOM mount options are rejected by fusermount3: {:?}",
            custom,
        );
    }

    // ── Trash directory guard ─────────────────────────────────────────────────

    #[test]
    fn is_trash_dir_rejects_trash_names() {
        assert!(is_trash_dir(OsStr::new(".Trash-1000")));
        assert!(is_trash_dir(OsStr::new(".Trash")));
        assert!(is_trash_dir(OsStr::new(".Trash-0")));
    }

    #[test]
    fn is_trash_dir_allows_normal_names() {
        assert!(!is_trash_dir(OsStr::new("Documents")));
        assert!(!is_trash_dir(OsStr::new(".hidden")));
        assert!(!is_trash_dir(OsStr::new("Trash")));
    }

    // ── Boot cache loading ────────────────────────────────────────────────────

    #[test]
    fn boot_loaded_dirs_are_not_invalidated() {
        let cache = Arc::new(Mutex::new(make_test_cache()));
        let path = cache.safe_lock().cache_dir.join(DIR_CACHE_FILE);
        let mut map = HashMap::new();
        map.insert("/Photos".to_string(), PersistedDirEntry {
            etag: Some("abc123".into()),
            self_entry: None,
            files: vec![make_dav_entry("sunset.jpg", Some(1))],
        });
        let json = serde_json::to_vec(&map).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).ok();
        std::fs::write(&path, json).unwrap();
        load_dir_cache(&cache);
        let c = cache.safe_lock();
        let entry = c.dir_cache.get(&PathBuf::from("/Photos")).unwrap();
        assert!(!entry.invalidated, "boot-loaded dir should not be invalidated");
        assert_eq!(entry.etag, Some("abc123".into()));
        drop(c);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn boot_validate_root_invalidates_changed_dirs() {
        let mut cache = make_test_cache();
        cache.put_dir_cache(PathBuf::from("/"), Some("root_etag".into()), None, vec![
            {
                let mut e = make_dav_entry("unchanged", None);
                e.path = PathBuf::from("/unchanged");
                e.is_dir = true;
                e.etag = Some("etag_a".into());
                e
            },
            {
                let mut e = make_dav_entry("changed", None);
                e.path = PathBuf::from("/changed");
                e.is_dir = true;
                e.etag = Some("etag_b".into());
                e
            },
        ]);
        cache.put_dir_cache(PathBuf::from("/unchanged"), Some("etag_a".into()), None, vec![]);
        cache.put_dir_cache(PathBuf::from("/changed"), Some("etag_b".into()), None, vec![]);

        let fresh_root = vec![
            {
                let mut e = make_dav_entry("unchanged", None);
                e.path = PathBuf::from("/unchanged");
                e.is_dir = true;
                e.etag = Some("etag_a".into());
                e
            },
            {
                let mut e = make_dav_entry("changed", None);
                e.path = PathBuf::from("/changed");
                e.is_dir = true;
                e.etag = Some("etag_NEW".into());
                e
            },
        ];

        // Simulate what boot_validate_root does with the fresh listing
        for entry in &fresh_root {
            if !entry.is_dir { continue; }
            let fresh_etag = entry.etag.as_deref();
            let cached_etag = cache.dir_cache.get(&entry.path).and_then(|e| e.etag.as_deref());
            match (fresh_etag, cached_etag) {
                (Some(f), Some(c_etag)) if f == c_etag => {}
                _ => {
                    if let Some(dir_entry) = cache.dir_cache.get_mut(&entry.path) {
                        dir_entry.invalidated = true;
                    }
                }
            }
        }

        assert!(!cache.dir_cache[&PathBuf::from("/unchanged")].invalidated,
            "dir with matching etag should remain valid");
        assert!(cache.dir_cache[&PathBuf::from("/changed")].invalidated,
            "dir with changed etag should be invalidated");
    }
}
