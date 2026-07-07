pub mod auth;
pub mod backend;
pub mod config;
pub mod edit_locally;
pub mod filename_validation;
pub mod fuse_notify;
pub mod ipc;
pub mod mutation_journal;
pub mod nextcloud;
pub mod notifications;
pub mod notify_push;
pub mod preview;
pub mod propfind;
pub mod remote_wipe;
pub mod search;
pub mod webdav_ops;

use std::collections::{HashMap, HashSet};
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

pub use config::{configuration_parser, MountOptions};
use ipc::{FileStatus, StatusMap};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use backend::RemoteEntry;
use serde::{Deserialize, Serialize};

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
    files: Arc<Vec<RemoteEntry>>,
    self_entry: Option<RemoteEntry>,
    etag: Option<String>,
    at: Instant,
    refreshing: bool,
    invalidated: bool,
}

struct PendingDir {
    entries: Vec<RemoteEntry>,
    rx: mpsc::Receiver<RemoteEntry>,
    etag_rx: mpsc::Receiver<Option<String>>,
    self_rx: mpsc::Receiver<RemoteEntry>,
    etag: Option<String>,
    self_entry: Option<RemoteEntry>,
}

struct FileCacheEntry {
    local_path: PathBuf,
    remote_modified: Option<SystemTime>,
    etag: Option<String>,
    kept: bool,
    size: u64,
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


#[derive(Clone, Debug, PartialEq)]
pub enum SyncState {
    Idle,
    Syncing,
    Paused,
    Unmounted,
    Wiped,
    Error(String),
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
) -> Result<(Option<String>, Option<RemoteEntry>, Vec<RemoteEntry>), String> {
    log::debug!("LIST {}", path.display());
    let (tx, rx) = mpsc::channel();
    let c = conn.clone();
    thread::spawn(move || {
        let _permit = c.throttle.acquire();
        let _ = tx.send(c.backend.list_dir(&path, PROPFIND_TIMEOUT)
            .map_err(|e| e.to_string()));
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
    conn: &Arc<ConnInfo>,
    path: PathBuf,
    dest: std::fs::File,
    transfers: Option<TransferMap>,
) -> Result<(), String> {
    log::info!("DOWNLOAD {}", path.display());
    let (tx, rx) = mpsc::channel();
    let c = conn.clone();
    thread::spawn(move || {
        let _permit = c.read_throttle.acquire();
        let mut writer: Box<dyn std::io::Write + Send> = if let Some(tm) = transfers {
            Box::new(ProgressWriter { inner: dest, path: path.clone(), transfer_map: tm, written: 0 })
        } else {
            Box::new(dest)
        };
        let result = c.backend.download_file(&path, &mut *writer, DOWNLOAD_TIMEOUT)
            .map(|_| ())
            .map_err(|e| e.to_string());
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
    kept_dir: PathBuf,
    auto_cache_dir: PathBuf,
    pub(crate) pending_notify: Arc<(Mutex<()>, Condvar)>,
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

    fn get_cached_dir(&mut self, path: &Path, ttl: Duration) -> Option<(Arc<Vec<RemoteEntry>>, bool)> {
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

    fn get_cached_dir_readonly(&self, path: &Path) -> Option<Arc<Vec<RemoteEntry>>> {
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
            .and_then(|e| e.ext.str("permissions").map(str::to_string))
    }

    fn cached_dir_etag(&self, path: &Path) -> Option<String> {
        self.dir_cache.get(path)?.etag.clone()
    }

    fn put_dir_cache(&mut self, path: PathBuf, etag: Option<String>, self_entry: Option<RemoteEntry>, files: Vec<RemoteEntry>) {
        self.dir_cache.insert(path, DirCacheEntry { files: Arc::new(files), self_entry, etag, at: Instant::now(), refreshing: false, invalidated: false });
    }

    fn touch_dir_cache(&mut self, path: &Path) {
        if let Some(entry) = self.dir_cache.get_mut(path) {
            entry.at = Instant::now();
            entry.refreshing = false;
            entry.invalidated = false;
        }
    }

    fn start_pending(&mut self, path: PathBuf, rx: mpsc::Receiver<RemoteEntry>, etag_rx: mpsc::Receiver<Option<String>>, self_rx: mpsc::Receiver<RemoteEntry>) {
        self.pending_dirs.insert(path, PendingDir {
            entries: Vec::new(),
            rx,
            etag_rx,
            self_rx,
            etag: None,
            self_entry: None,
        });
    }

    fn start_pending_and_notify(&mut self, path: PathBuf, rx: mpsc::Receiver<RemoteEntry>, etag_rx: mpsc::Receiver<Option<String>>, self_rx: mpsc::Receiver<RemoteEntry>) -> Arc<(Mutex<()>, Condvar)> {
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

    fn promote_pending(&mut self, path: &Path) -> Option<RemoteEntry> {
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

    fn get_pending_snapshot(&mut self, path: &Path) -> Option<Vec<RemoteEntry>> {
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
        self.find_entry(path).and_then(|e| e.change_token.clone())
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
    files: Vec<RemoteEntry>,
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
    match serde_json::to_vec(&map) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                log::error!("DIR_CACHE write failed {}: {} — cache will be cold on restart", path.display(), e);
            } else {
                log::info!("DIR_CACHE saved {} dirs to {}", map.len(), path.display());
            }
        }
        Err(e) => log::error!("DIR_CACHE serialize failed: {}", e),
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
                    status.safe_lock().insert(f.remote_path.clone(), FileStatus::Remote);
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
                status.safe_lock().insert(f.remote_path.clone(), FileStatus::Remote);
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

fn get_or_list_dir(
    conn: &Arc<ConnInfo>,
    cache: &Arc<Mutex<FsCache>>,
    path: PathBuf,
) -> Result<(Arc<Vec<RemoteEntry>>, Option<RemoteEntry>), String> {
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
                        match conn.backend.dir_change_token(&path, PROPFIND_TIMEOUT) {
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
                match conn2.backend.list_dir_streaming(
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
    conn: &Arc<ConnInfo>,
    cache: &Arc<Mutex<FsCache>>,
    status: &StatusMap,
    dirty: &ipc::DirtySet,
    remote_path: PathBuf,
    transfers: Option<&TransferMap>,
    kept: bool,
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
                        status.safe_lock().insert(remote_path.clone(), FileStatus::Kept);
                        dirty.safe_lock().insert(remote_path);
                        return Ok(new_path);
                    }
                }
            }
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
    let local_path = target_dir.join(rel);
    if let Some(parent) = local_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let file =
        std::fs::File::create(&local_path).map_err(|e| format!("create cache file: {}", e))?;
    if let Err(e) = open_file_timeout(conn, remote_path.clone(), file, transfers.cloned()) {
        if let Some(tm) = transfers { tm.safe_lock().remove(&remote_path); }
        if let Err(rm_err) = std::fs::remove_file(&local_path) {
            log::error!("CRITICAL: cannot remove partial download {}: {} — zeroing to prevent serving corrupt data", local_path.display(), rm_err);
            if let Ok(f) = std::fs::File::create(&local_path) {
                let _ = f.set_len(0);
            }
        }
        cache.safe_lock().file_cache.remove(&remote_path);
        status.safe_lock().insert(remote_path.clone(), FileStatus::Remote);
        dirty.safe_lock().insert(remote_path);
        return Err(e);
    }

    if let Some(tm) = transfers { tm.safe_lock().remove(&remote_path); }

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
    status.safe_lock().insert(remote_path.clone(), final_status);
    dirty.safe_lock().insert(remote_path);
    Ok(local_path)
}

fn keep_locally_recursive(
    conn: &Arc<ConnInfo>,
    cache: &Arc<Mutex<FsCache>>,
    status: &StatusMap,
    dirty: &ipc::DirtySet,
    remote_path: PathBuf,
    transfers: Option<&TransferMap>,
) {
    log::info!("KEEP {}", remote_path.display());

    let known_dir = cache.safe_lock().is_known_directory(&remote_path);

    if known_dir == Some(false) {
        if let Err(e) = ensure_file_cached(conn, cache, status, dirty, remote_path.clone(), transfers, true) {
            log::warn!("keep failed {}: {}", remote_path.display(), e);
        }
        return;
    }

    let (entries, _self_entry) = match get_or_list_dir(conn, cache, remote_path.clone()) {
        Ok(e) => e,
        Err(e) => {
            if known_dir.is_none() {
                if let Err(e2) = ensure_file_cached(conn, cache, status, dirty, remote_path.clone(), transfers, true) {
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
            let r = conn2.backend.list_dir_streaming(
                &path, PROPFIND_TIMEOUT, entry_tx, self_tx,
            );
            r
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
    backend: Arc<dyn crate::backend::CloudBackend>,
    base_url: String,
    webdav_url: String,
    creds: auth::Credentials,
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
    shutdown: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
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
    auto_keep_cached_files: bool,
    read_ahead_bytes: usize,
    cache_streamed_reads: bool,
    exclude_folders: HashSet<PathBuf>,
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

        let cache_dir = dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("ncrs")
            .join(url_to_dir_name(&options.url));
        let kept_dir = cache_dir.join("kept");
        let auto_cache_dir = cache_dir.join("cache");
        std::fs::create_dir_all(&cache_dir)
            .map_err(|e| format!("Cannot create cache dir: {}", e))?;
        std::fs::create_dir_all(&kept_dir)
            .map_err(|e| format!("Cannot create kept dir: {}", e))?;
        std::fs::create_dir_all(&auto_cache_dir)
            .map_err(|e| format!("Cannot create auto-cache dir: {}", e))?;

        let mut stale_count = 0usize;
        if let Ok(entries) = std::fs::read_dir(&cache_dir) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if name.starts_with("write_") {
                        if let Ok(meta) = entry.metadata() {
                            if meta.len() == 0 || !meta.is_file() {
                                let _ = std::fs::remove_file(entry.path());
                                stale_count += 1;
                            }
                        }
                    }
                }
            }
        }
        if stale_count > 0 {
            log::info!("cleaned up {} stale write_* temp files", stale_count);
        }

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

        let base_url = notifications::base_url(&options.url);
        let backend: Arc<dyn crate::backend::CloudBackend> = if options.offline {
            Arc::new(crate::nextcloud::NextcloudBackend::new_offline(
                base_url.clone(),
                options.url.clone(),
                creds.clone(),
                http.clone(),
                http_read.clone(),
                options.http3,
            ))
        } else {
            Arc::new(crate::nextcloud::NextcloudBackend::new(
                base_url.clone(),
                options.url.clone(),
                creds.clone(),
                http.clone(),
                http_read.clone(),
                options.http3,
            )?)
        };

        let conn = Arc::new(ConnInfo {
            backend,
            base_url,
            webdav_url: options.url.clone(),
            creds,
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
            shutdown: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
        });

        Ok(NextCloudFs {
            cache: {
                let c = Arc::new(Mutex::new(FsCache {
                    inodes,
                    paths,
                    next_inode: 2,
                    dir_cache: HashMap::new(),
                    pending_dirs: HashMap::new(),
                    file_cache: HashMap::new(),
                    cache_dir,
                    kept_dir,
                    auto_cache_dir,
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
            auto_keep_cached_files: options.auto_keep_cached_files,
            read_ahead_bytes: options.read_ahead_bytes,
            cache_streamed_reads: options.cache_streamed_reads,
            exclude_folders,
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
        let cache = self.cache.clone();
        let status = self.status.clone();
        let dirty = self.dirty.clone();
        let transfers = self.transfer_map.clone();
        Arc::new(move |remote_path| {
            keep_locally_recursive(&conn, &cache, &status, &dirty, remote_path, Some(&transfers));
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
                if let Err(e) = std::fs::remove_file(&local_path) {
                    log::warn!("evict: failed to remove cached file {}: {} — orphaned on disk", local_path.display(), e);
                }
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

        let is_cached = self.cache.safe_lock().dir_cache.contains_key(&parent_path);
        if !is_cached {
            if let Err(e) = get_or_list_dir(&self.conn, &self.cache, parent_path.clone()) {
                log::debug!("lookup: list {} failed (will return ENOENT): {}", parent_path.display(), e);
            }
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
                if entry.ext.flag("is_shared") {
                    self.shared.safe_lock().insert(target_path.clone());
                }
                if let Some(fid) = entry.ext.int("fileid") {
                    self.fileids.safe_lock().insert(target_path.clone(), fid);
                }
                self.details.safe_lock().insert(target_path.clone(), ipc::FileDetail {
                    permissions: entry.ext.str("permissions").map(str::to_string),
                    owner_id: entry.ext.str("owner_id").map(str::to_string),
                    owner_display_name: entry.ext.str("owner_display_name").map(str::to_string),
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
                    (e.change_token.clone(), e.ext.str("permissions").map(str::to_string))
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
                if let Err(e) = std::fs::copy(local, &wp) {
                    log::error!("open: failed to seed staging file {} from {}: {}", wp.display(), local.display(), e);
                    reply.error(Errno::EIO);
                    return;
                }
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
        let cache = self.cache.clone();
        let status = self.status.clone();
        let dirty = self.dirty.clone();
        let elog = self.error_log.clone();
        let tmap = self.transfer_map.clone();
        let notifier_slot = self.notifier_slot.clone();
        let auto_keep_cached = self.auto_keep_cached_files;
        let read_ahead = self.read_ahead_bytes;
        let cache_streamed = self.cache_streamed_reads;
        let file_total_size = self.cache.safe_lock().find_entry(&path).map(|e| e.size).unwrap_or(0);

        thread::spawn(move || {
            let fetch = std::cmp::max(sz, read_ahead);
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
                            if cache_streamed
                                && off == 0
                                && file_total_size > 0
                                && total_bytes as u64 >= file_total_size
                                && cache.safe_lock().file_cache.get(&path).is_none()
                            {
                                let ss = mtx.lock().unwrap();
                                let data = &ss.data[..file_total_size as usize];
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
                                if std::fs::write(&local_path, data).is_ok() {
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
                                    status.safe_lock().insert(path.clone(), file_status);
                                    dirty.safe_lock().insert(path.clone());
                                    log::info!("stream→cache {} ({}B, {})", path.display(), file_total_size, if kept { "kept" } else { "cached" });
                                    open_files.safe_lock().entry(fh.0).and_modify(|of| {
                                        of.local = Some(local_path);
                                    });
                                }
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
                    match ensure_file_cached(&conn, &cache, &status, &dirty, path.clone(), Some(&tmap), auto_keep_cached) {
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
        let exclude_folders = self.exclude_folders.clone();

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
                        if entry.is_dir && exclude_folders.contains(&entry_path) { continue; }
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
                                let kept_meta = kept_path.metadata().ok().filter(|m| m.len() > 0);
                                let cached_meta = cached_path.metadata().ok().filter(|m| m.len() > 0);
                                if let Some(km) = kept_meta {
                                    status_entries.push((entry_path.clone(), FileStatus::Kept));
                                    cache_entries.push((entry_path.clone(), FileCacheEntry {
                                        local_path: kept_path,
                                        remote_modified: entry.modified,
                                        etag: entry.change_token.clone(),
                                        kept: true,
                                        size: km.len(),
                                    }));
                                } else if let Some(cm) = cached_meta {
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
                                thumb_candidates.push((entry_path, entry.modified, entry.ext.flag("has_preview"), entry.ext.int("fileid")));
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
                            if entry.is_dir && exclude_folders.contains(&entry_path) { continue; }
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
                                    &conn2.creds,
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
                match conn.backend.put_file(&remote_path, body.clone(), etag_ref) {
                    Ok(result) => {
                        tmap.safe_lock().remove(&remote_path);
                        log::info!("PUT {} → new etag {:?}", remote_path.display(), result.new_change_token);
                        let new_size = body.len() as u64;
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
                            if std::fs::write(&keep_path, &body).is_ok() {
                                cache.safe_lock().file_cache.insert(remote_path.clone(), FileCacheEntry {
                                    local_path: keep_path,
                                    remote_modified: Some(SystemTime::now()),
                                    etag: result.new_change_token,
                                    kept: true,
                                    size: body.len() as u64,
                                });
                                smap.safe_lock().insert(remote_path.clone(), FileStatus::Kept);
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
                    Err(backend::BackendWriteError::Conflict) => {
                        tmap.safe_lock().remove(&remote_path);
                        smap.safe_lock().remove(&remote_path);
                        log::warn!("CONFLICT on PUT {} — creating conflicted copy", remote_path.display());
                        push_error(&elog, remote_path.clone(), SyncErrorKind::Conflict, "Server version changed — conflicted copy created".into());
                        let conflict_name = make_conflict_name(&remote_path);
                        match conn.backend.put_file(&conflict_name, body, None) {
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
                            backend::BackendWriteError::Conflict => SyncErrorKind::UploadFailed,
                            backend::BackendWriteError::Locked => SyncErrorKind::Locked,
                            backend::BackendWriteError::Network(_) => SyncErrorKind::NetworkError,
                            backend::BackendWriteError::Forbidden => SyncErrorKind::PermissionDenied,
                            backend::BackendWriteError::QuotaExceeded => SyncErrorKind::QuotaExceeded,
                            backend::BackendWriteError::Server(code, _) => SyncErrorKind::ServerError(*code),
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
        if let Err(e) = std::fs::File::create(&write_path) {
            log::error!("create: cannot create staging file {}: {}", write_path.display(), e);
            reply.error(Errno::EIO);
            return;
        }

        let ino = self.cache.safe_lock().allocate_inode(remote_path.clone());

        let now = SystemTime::now();
        let mut ext = backend::EntryExtensions::default();
        ext.strings.insert("permissions".into(), "RGDNVW".into());
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
        let mut ext = backend::EntryExtensions::default();
        ext.strings.insert("permissions".into(), "RGDNVCK".into());
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
                match conn.backend.mkdir(&remote_path) {
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
                let files: Vec<RemoteEntry> = dir.files.iter().filter(|e| e.path != remote_path).cloned().collect();
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
                match conn.backend.delete(&remote_path) {
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
                let files: Vec<RemoteEntry> = dir.files.iter().filter(|e| e.path != remote_path).cloned().collect();
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
                match conn.backend.delete(&remote_path) {
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
                match conn.backend.rename(&from, &to) {
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
        ;
    let resp = conn.creds.apply(resp)
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
                    let fresh_etag = entry.change_token.as_deref();
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

pub fn mount_ncfs(options: MountOptions, error_log: Option<ErrorLog>, transfer_map: Option<TransferMap>, journal: Option<mutation_journal::SharedJournal>, paused: Option<Arc<AtomicBool>>) -> Result<(), String> {
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
    if let Some(p) = paused {
        Arc::get_mut(&mut filesystem.conn).expect("conn not yet shared").paused = p;
    }
    let keep_cb = filesystem.keep_callback();
    let evict_cb = filesystem.evict_callback();
    let prefetch_cb = filesystem.prefetch_callback();
    let base_url = notifications::base_url(&options.url);
    let ipc_creds = options.credentials()?;
    let file_change_queue: ipc::FileChangeQueue = Arc::new(Mutex::new(Vec::new()));
    let storage_stats: ipc::SharedStorageStats = Arc::new(Mutex::new(ipc::StorageStats::default()));
    ipc::start_server(options.mount_point.clone(), filesystem.status_map(), filesystem.shared_set(), filesystem.fileid_map(), filesystem.detail_map(), filesystem.dirty_set(), ipc_creds, base_url, Some(keep_cb), Some(evict_cb), Some(prefetch_cb), filesystem.error_log(), filesystem.transfer_map(), filesystem.journal(), file_change_queue.clone(), storage_stats.clone());

    let offline_flag = filesystem.is_offline_flag();
    let backend = filesystem.conn.backend.clone();
    let notifier_slot = filesystem.notifier_slot();
    let wipe_flag = Arc::new(AtomicBool::new(false));

    if !options.offline {
        // Replay any journal entries from a previous session
        let replay_journal_ref = filesystem.journal();
        if !replay_journal_ref.safe_lock().is_empty() {
            let j = replay_journal_ref.clone();
            let b = backend.clone();
            let c = filesystem.cache_ref();
            let d = filesystem.dirty_set();
            let el = filesystem.error_log();
            thread::spawn(move || {
                let ctx = mutation_journal::ReplayContext { backend: b };
                mutation_journal::replay_journal(&j, &ctx, &c, &d, &el);
            });
        }

        // Connectivity monitor
        {
            let backend_monitor = backend.clone();
            let offline = offline_flag.clone();
            let journal_for_monitor = filesystem.journal();
            let cache_for_monitor = filesystem.cache_ref();
            let dirty_for_monitor = filesystem.dirty_set();
            let elog_for_monitor = filesystem.error_log();
            let shutdown_monitor = filesystem.shutdown_flag();
            let paused_monitor = filesystem.paused_flag();
            let wipe_flag_monitor = wipe_flag.clone();
            let conn_monitor = filesystem.conn.clone();
            let cache_dir_monitor = filesystem.cache_ref().safe_lock().cache_dir.clone();
            thread::spawn(move || {
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
                            let was_offline = offline.swap(false, Ordering::Relaxed);
                            if was_offline {
                                log::info!("CONNECTIVITY restored — replaying mutation journal");
                                let j = journal_for_monitor.clone();
                                let b = backend_monitor.clone();
                                let c = cache_for_monitor.clone();
                                let d = dirty_for_monitor.clone();
                                let el = elog_for_monitor.clone();
                                thread::spawn(move || {
                                    let ctx = mutation_journal::ReplayContext { backend: b };
                                    mutation_journal::replay_journal(&j, &ctx, &c, &d, &el);
                                });
                            }
                        }
                        backend::ReachabilityStatus::AuthRejected(code) => {
                            log::warn!("CONNECTIVITY: auth rejected (HTTP {}), checking for remote wipe", code);
                            match remote_wipe::check_wipe(&conn_monitor.http, &conn_monitor.base_url, conn_monitor.creds.secret()) {
                                Ok(true) => {
                                    log::warn!("REMOTE WIPE requested by server — executing");
                                    let config_path = config::config_path();
                                    if let Err(e) = remote_wipe::execute_wipe(&cache_dir_monitor, &config_path) {
                                        log::error!("REMOTE_WIPE execution error: {}", e);
                                    }
                                    if let Err(e) = remote_wipe::confirm_wipe(&conn_monitor.http, &conn_monitor.base_url, conn_monitor.creds.secret()) {
                                        log::warn!("REMOTE_WIPE: failed to confirm to server: {}", e);
                                    }
                                    wipe_flag_monitor.store(true, Ordering::Relaxed);
                                    shutdown_monitor.store(true, Ordering::Relaxed);
                                    break;
                                }
                                Ok(false) => {
                                    log::info!("CONNECTIVITY: auth rejected but no wipe pending — token may be revoked");
                                    offline.store(true, Ordering::Relaxed);
                                }
                                Err(e) => {
                                    log::warn!("CONNECTIVITY: wipe check failed: {} — will retry", e);
                                    offline.store(true, Ordering::Relaxed);
                                }
                            }
                        }
                        backend::ReachabilityStatus::Unreachable => {
                            let was_offline = offline.swap(true, Ordering::Relaxed);
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
            let debounce: notify_push::DebounceMap = Arc::new(Mutex::new(HashMap::new()));

            let watcher = backend.start_change_watcher(Box::new(move |event| {
                if watcher_paused.load(Ordering::Relaxed) { return; }
                if watcher_offline.load(Ordering::Relaxed) { return; }
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

            // Sync watcher connection status and pause state
            let np_connected = filesystem.notify_push_connected_flag();
            let sync_offline = offline_flag.clone();
            let sync_paused = filesystem.paused_flag();
            let watcher_shutdown = filesystem.shutdown_flag();
            thread::spawn(move || {
                while !watcher_shutdown.load(Ordering::Relaxed) {
                    np_connected.store(watcher.is_connected(), Ordering::Relaxed);
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
        thread::spawn(move || {
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
            thread::spawn(move || {
                log::info!("FILE_CACHE boot validation: checking {} files", saved_etags.len());
                let mut stale = 0usize;
                for (remote_path, entry) in &saved_etags {
                    if file_shutdown.load(Ordering::Relaxed) {
                        log::info!("FILE_CACHE boot validation: shutdown, aborting");
                        break;
                    }
                    match boot_backend.dir_change_token(remote_path, PROPFIND_TIMEOUT) {
                        Ok(Some(ref new_etag)) if new_etag == &entry.etag => {}
                        Ok(new_etag) => {
                            log::info!("FILE_CACHE stale: {} (etag {:?} → {:?})", remote_path.display(), entry.etag, new_etag);
                            cache.safe_lock().file_cache.remove(remote_path);
                            match ensure_file_cached(&boot_conn, &cache, &status, &dirty, remote_path.clone(), Some(&boot_transfers), entry.kept) {
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
        thread::spawn(move || {
            for p in paths {
                if keep_shutdown.load(Ordering::Relaxed) { break; }
                log::info!("AUTO_KEEP: keeping {}", p.display());
                keep_locally_recursive(&keep_conn, &keep_cache, &keep_status, &keep_dirty, p.clone(), Some(&keep_transfers));
            }
            log::info!("AUTO_KEEP: done");
        });
    }

    // Storage stats update thread
    {
        let stats_cache = filesystem.cache_ref();
        let stats_backend = backend.clone();
        let stats_shutdown = filesystem.shutdown_flag();
        let stats_store = storage_stats;
        thread::spawn(move || {
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
        thread::spawn(move || {
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

    let mp_str = options.mount_point.to_string_lossy().to_string();

    // Classify the mount point before touching it:
    //  - stat fails with ENOTCONN: stale FUSE mount left by a dead process — detach it
    //  - listed as a live fuse mount in /proc/self/mounts: another instance owns it — refuse,
    //    detaching here would steal the mount out from under that instance
    //  - otherwise: plain (or missing) directory — create it and require it empty
    match std::fs::metadata(&options.mount_point) {
        Err(e) if e.raw_os_error() == Some(libc::ENOTCONN) => {
            log::info!("Detaching stale FUSE mount at {}", mp_str);
            let _ = std::process::Command::new("fusermount")
                .args(["-uz", &mp_str])
                .output();
        }
        _ => {
            if is_live_fuse_mount(&options.mount_point) {
                return Err(format!(
                    "{} is already mounted — is another ncrs instance (GUI or systemd service) running?",
                    mp_str
                ));
            }
        }
    }

    if let Err(e) = std::fs::create_dir_all(&options.mount_point) {
        return Err(format!("failed to create mount point {}: {}", options.mount_point.display(), e));
    }
    match std::fs::read_dir(&options.mount_point) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(format!(
                    "mount point {} is not empty — mounting would hide its contents; move them away first",
                    mp_str
                ));
            }
        }
        Err(e) => return Err(format!("cannot read mount point {}: {}", mp_str, e)),
    }

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

    // The session has unmounted; remove the now-empty mount dir so an
    // unmounted state can't be mistaken for an empty share. remove_dir
    // refuses non-empty or still-mounted dirs, so this is safe best-effort.
    let _ = std::fs::remove_dir(&options.mount_point);

    if wipe_flag.load(Ordering::Relaxed) {
        return Err("REMOTE_WIPE".to_string());
    }

    result
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
