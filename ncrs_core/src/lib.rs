pub mod config;
pub mod filename_validation;
pub mod ipc;
pub mod notifications;
pub mod notify_push;
pub mod preview;
pub mod propfind;
pub mod search;
pub mod webdav_ops;

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow,
};
use libc::{EACCES, EIO, ENOENT};
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
const PROPFIND_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const READ_AHEAD: usize = 8 * 1024 * 1024; // 8 MB
const MAX_POOL_IDLE: usize = 8;

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
        while *count >= self.max {
            count = self.cv.wait(count).unwrap();
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
}

struct ReadAheadBuf {
    start: u64,
    data: Vec<u8>,
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
}

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

// ── Network layer — each call runs in a sub-thread so the caller can impose a
//    deadline via recv_timeout without blocking the FUSE session thread. ───────

struct FsNetwork {
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

fn error_to_errno(err: &str) -> i32 {
    if err.contains("401") || err.contains("403") || err.contains("Unauthorized") || err.contains("Forbidden") {
        EACCES
    } else if err.contains("404") || err.contains("Not Found") {
        ENOENT
    } else {
        EIO
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

fn open_file_timeout(
    net: &Arc<FsNetwork>,
    throttle: &Arc<Throttle>,
    path: PathBuf,
    dest: std::fs::File,
) -> Result<(), String> {
    log::info!("DOWNLOAD {}", path.display());
    let (tx, rx) = mpsc::channel();
    let n = net.clone();
    let throttle = throttle.clone();
    thread::spawn(move || {
        let _permit = throttle.acquire();
        let result = match n.checkout() {
            Ok(mut conn) => {
                let r = conn
                    .open_file(&path, Box::new(dest))
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
    file_cache: HashMap<PathBuf, FileCacheEntry>,
    cache_dir: PathBuf,
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

    fn get_cached_dir(&mut self, path: &Path) -> Option<(Arc<Vec<DavEntry>>, bool)> {
        let entry = self.dir_cache.get_mut(path)?;
        let stale = entry.at.elapsed() >= DIR_CACHE_TTL;
        let needs_refresh = stale && !entry.refreshing;
        if needs_refresh {
            entry.refreshing = true;
        }
        Some((Arc::clone(&entry.files), needs_refresh))
    }

    fn get_cached_dir_readonly(&self, path: &Path) -> Option<Arc<Vec<DavEntry>>> {
        self.dir_cache.get(path).map(|e| Arc::clone(&e.files))
    }

    fn cached_dir_etag(&self, path: &Path) -> Option<String> {
        self.dir_cache.get(path)?.etag.clone()
    }

    fn put_dir_cache(&mut self, path: PathBuf, etag: Option<String>, self_entry: Option<DavEntry>, files: Vec<DavEntry>) {
        self.dir_cache.insert(path, DirCacheEntry { files: Arc::new(files), self_entry, etag, at: Instant::now(), refreshing: false });
    }

    fn touch_dir_cache(&mut self, path: &Path) {
        if let Some(entry) = self.dir_cache.get_mut(path) {
            entry.at = Instant::now();
            entry.refreshing = false;
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
        loop {
            match pending.rx.try_recv() {
                Ok(entry) => pending.entries.push(entry),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.promote_pending(path);
                    return self.dir_cache.get(path).map(|e| e.files.to_vec());
                }
            }
        }
        if !pending.entries.is_empty() {
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
        let parent = path.parent().unwrap_or(Path::new("/"));
        let name = path.file_name()?.to_str()?.to_string();
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
            .and_then(|e| e.modified)
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

fn save_dir_cache(cache: &Mutex<FsCache>) {
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
        });
        count += 1;
    }
    log::info!("DIR_CACHE loaded {} dirs from {}", count, path.display());
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
    {
        let mut c = cache.safe_lock();
        if let Some((files, needs_refresh)) = c.get_cached_dir(&path) {
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
    let already_pending = {
        let mut c = cache.safe_lock();
        if c.dir_cache.contains_key(&path) {
            if let Some((files, _)) = c.get_cached_dir(&path) {
                let se = c.dir_cache.get(&path).and_then(|e| e.self_entry.clone());
                return Ok((files, se));
            }
        }
        if c.pending_dirs.contains_key(&path) {
            true
        } else {
            let (entry_tx, entry_rx) = mpsc::channel();
            let (etag_tx, etag_rx) = mpsc::channel();
            let (self_tx, self_rx) = mpsc::channel();
            c.start_pending(path.clone(), entry_rx, etag_rx, self_rx);

            let conn2 = conn.clone();
            let path2 = path.clone();
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
            });
            false
        }
    };
    if already_pending {
        log::info!("LIST_JOIN {} — waiting for existing fetch", path.display());
    }

    // Block until first entries arrive or PROPFIND completes/times out.
    let deadline = Instant::now() + PROPFIND_TIMEOUT;
    loop {
        {
            let mut c = cache.safe_lock();
            if let Some(snapshot) = c.get_pending_snapshot(&path) {
                if !snapshot.is_empty() {
                    log::info!("LIST_STREAM {} ({} entries) in {:?}", path.display(), snapshot.len(), t0.elapsed());
                    let se = c.pending_dirs.get(&path).and_then(|p| p.self_entry.clone());
                    return Ok((Arc::new(snapshot), se));
                }
            }
            if c.dir_cache.contains_key(&path) {
                let se = c.dir_cache.get(&path).and_then(|e| e.self_entry.clone());
                log::info!("LIST_PROMOTED {} in {:?}", path.display(), t0.elapsed());
                return c.get_cached_dir(&path)
                    .map(|(f, _)| (f, se))
                    .ok_or_else(|| format!("PROPFIND returned empty for {}", path.display()));
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
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
        if let Some((files, _)) = c.get_cached_dir(&path) {
            return Ok((files, se));
        }
    } else {
        log::warn!("PROPFIND timeout {} — removing stale pending (no entries yet, elapsed {:?})", path.display(), t0.elapsed());
        c.pending_dirs.remove(&path);
    }
    Err(format!("PROPFIND timeout for {}", path.display()))
}

fn ensure_file_cached(
    net: &Arc<FsNetwork>,
    throttle: &Arc<Throttle>,
    cache: &Arc<Mutex<FsCache>>,
    status: &StatusMap,
    dirty: &ipc::DirtySet,
    remote_path: PathBuf,
) -> Result<PathBuf, String> {
    let (maybe_local, cached_mod, current_mod, cache_dir) = {
        let c = cache.safe_lock();
        let entry = c.file_cache.get(&remote_path);
        let maybe_local = entry
            .filter(|e| e.local_path.exists())
            .map(|e| e.local_path.clone());
        let cached_mod = entry.and_then(|e| e.remote_modified);
        let current_mod = c.remote_modified_for(&remote_path);
        (maybe_local, cached_mod, current_mod, c.cache_dir.clone())
    };

    if let Some(local) = maybe_local {
        if cached_mod == current_mod {
            return Ok(local);
        }
    }

    status.safe_lock().insert(remote_path.clone(), FileStatus::Downloading);
    dirty.safe_lock().insert(remote_path.clone());

    let rel = remote_path.strip_prefix("/").unwrap_or(&remote_path);
    let local_path = cache_dir.join(rel);
    if let Some(parent) = local_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let file =
        std::fs::File::create(&local_path).map_err(|e| format!("create cache file: {}", e))?;
    if let Err(e) = open_file_timeout(net, throttle, remote_path.clone(), file) {
        status.safe_lock().insert(remote_path.clone(), FileStatus::Remote);
        dirty.safe_lock().insert(remote_path);
        return Err(e);
    }

    {
        let mut c = cache.safe_lock();
        let mod_time = c.remote_modified_for(&remote_path);
        c.file_cache.insert(
            remote_path.clone(),
            FileCacheEntry { local_path: local_path.clone(), remote_modified: mod_time },
        );
    }
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
) {
    log::info!("KEEP {}", remote_path.display());

    let known_dir = cache.safe_lock().is_known_directory(&remote_path);

    if known_dir == Some(false) {
        if let Err(e) = ensure_file_cached(net, &conn.throttle, cache, status, dirty, remote_path.clone()) {
            log::warn!("keep failed {}: {}", remote_path.display(), e);
        }
        return;
    }

    let (entries, _self_entry) = match get_or_list_dir(conn, cache, remote_path.clone()) {
        Ok(e) => e,
        Err(e) => {
            if known_dir.is_none() {
                if let Err(e2) = ensure_file_cached(net, &conn.throttle, cache, status, dirty, remote_path.clone()) {
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
                    if let Err(e) = ensure_file_cached(net, &conn.throttle, cache, status, dirty, path.clone()) {
                        log::warn!("keep failed {}: {}", path.display(), e);
                    }
                });
            }
        });
        std::thread::sleep(Duration::from_millis(50));
    }

    for dir in dirs {
        keep_locally_recursive(conn, net, cache, status, dirty, dir);
    }
}

fn prefetch_list_dir(conn: &ConnInfo, cache: &Mutex<FsCache>, path: &Path) {
    {
        let mut c = cache.safe_lock();
        if c.get_cached_dir(path).is_some() || c.pending_dirs.contains_key(path) {
            return;
        }
    }
    log::info!("PREFETCH_LIST {}", path.display());
    let _permit = conn.throttle.acquire();
    match propfind::propfind_list(&conn.http, &conn.webdav_url, &conn.username, &conn.password, path, PROPFIND_TIMEOUT) {
        Ok((etag, self_entry, files)) => {
            cache.safe_lock().put_dir_cache(path.to_path_buf(), etag, self_entry, files);
        }
        Err(e) => log::warn!("prefetch {}: {}", path.display(), e),
    }
}

// ── FileAttr helpers ──────────────────────────────────────────────────────────

fn make_file_attr(inode: u64, entry: &DavEntry) -> FileAttr {
    let modified = entry.modified.unwrap_or(UNIX_EPOCH);
    FileAttr {
        ino: inode,
        size: entry.size,
        blocks: (entry.size + 511) / 512,
        atime: modified,
        mtime: modified,
        ctime: modified,
        crtime: modified,
        kind: if entry.is_dir { FileType::Directory } else { FileType::RegularFile },
        perm: if entry.is_dir { 0o755 } else { 0o644 },
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
        ino: inode,
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
        ino: 1,
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
    http_read: reqwest::blocking::Client,
    throttle: Arc<Throttle>,
    is_offline: Arc<AtomicBool>,
}

pub struct NextCloudFs {
    net: Arc<FsNetwork>,
    cache: Arc<Mutex<FsCache>>,
    status: StatusMap,
    dirty: ipc::DirtySet,
    shared: ipc::SharedSet,
    fileids: ipc::FileIdMap,
    details: ipc::FileDetailMap,
    ipc_populated: Arc<Mutex<std::collections::HashSet<PathBuf>>>,
    deferred_readdir: Arc<Mutex<std::collections::HashSet<PathBuf>>>,
    conn: Arc<ConnInfo>,
    open_files: Arc<Mutex<HashMap<u64, OpenFile>>>,
    next_fh: Arc<Mutex<u64>>,
    log_user: String,
    aggressive_prefetch: bool,
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
            .pool_max_idle_per_host(4);
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
            is_offline,
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
                }));
                load_dir_cache(&c);
                c
            },
            status,
            dirty,
            shared,
            fileids,
            details,
            ipc_populated: Arc::new(Mutex::new(std::collections::HashSet::new())),
            deferred_readdir: Arc::new(Mutex::new(std::collections::HashSet::new())),
            conn,
            open_files: Arc::new(Mutex::new(HashMap::new())),
            next_fh: Arc::new(Mutex::new(1)),
            aggressive_prefetch: options.aggressive_prefetch,
            log_user: options.log_user,
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

    pub fn conn_http(&self) -> reqwest::blocking::Client {
        self.conn.http.clone()
    }

    pub(crate) fn cache_ref(&self) -> Arc<Mutex<FsCache>> {
        self.cache.clone()
    }

    pub fn keep_callback(&self) -> ipc::KeepCallback {
        let conn = self.conn.clone();
        let net = self.net.clone();
        let cache = self.cache.clone();
        let status = self.status.clone();
        let dirty = self.dirty.clone();
        Arc::new(move |remote_path| {
            keep_locally_recursive(&conn, &net, &cache, &status, &dirty, remote_path);
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
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let (parent_path, name_str) = {
            let c = self.cache.safe_lock();
            match (c.get_path(parent), name.to_str()) {
                (Some(p), Some(n)) => (p, n.to_string()),
                _ => {
                    reply.error(ENOENT);
                    return;
                }
            }
        };

        let is_cached = self.cache.safe_lock().dir_cache.contains_key(&parent_path);
        if !is_cached {
            let _ = get_or_list_dir(&self.conn, &self.cache, parent_path.clone());
        }

        let mut c = self.cache.safe_lock();
        let entries = match c.get_cached_dir(&parent_path) {
            Some((files, _)) => files,
            None => {
                reply.error(ENOENT);
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
                reply.entry(&TTL, &attr, 0);
                return;
            }
        }
        reply.error(ENOENT);
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        if ino == 1 {
            reply.attr(&TTL, &root_attr());
            return;
        }

        let path = match self.cache.safe_lock().get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        let parent = path.parent().unwrap_or(Path::new("/")).to_path_buf();
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();

        let c = self.cache.safe_lock();
        let entries = match c.get_cached_dir_readonly(&parent) {
            Some(files) => files,
            None => {
                if c.dir_cache.contains_key(&path) || path == Path::new("/") {
                    drop(c);
                    reply.attr(&TTL, &make_dir_attr(ino));
                } else {
                    reply.error(ENOENT);
                }
                return;
            }
        };

        for entry in entries.iter() {
            if entry.path.file_name().and_then(|n| n.to_str()).unwrap_or("") == file_name {
                reply.attr(&TTL, &make_file_attr(ino, entry));
                return;
            }
        }
        reply.error(ENOENT);
    }

    fn open(&mut self, _req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
        let (path, local, etag) = {
            let c = self.cache.safe_lock();
            let path = match c.get_path(ino) {
                Some(p) => p,
                None => {
                    reply.error(ENOENT);
                    return;
                }
            };
            let local = c
                .file_cache
                .get(&path)
                .filter(|e| e.local_path.exists())
                .map(|e| e.local_path.clone());
            let parent = path.parent().unwrap_or(Path::new("/")).to_path_buf();
            let etag = c.get_cached_dir_readonly(&parent).and_then(|files| {
                files.iter().find(|e| e.path == path).and_then(|e| e.etag.clone())
            });
            (path, local, etag)
        };

        let writable = flags & (libc::O_WRONLY | libc::O_RDWR | libc::O_APPEND) != 0;

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
        reply.opened(fh, 0);
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyData,
    ) {
        let path = match self.cache.safe_lock().get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        log::debug!("[{}] READ {} offset={} size={}", self.log_user, path.display(), offset, size);

        let off = offset as u64;
        let sz = size as usize;

        // Serve from open-file state synchronously (no thread spawn).
        {
            let files = self.open_files.safe_lock();
            if let Some(of) = files.get(&fh) {
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
                    let buf_end = ra.start + ra.data.len() as u64;
                    if off >= ra.start && off + sz as u64 <= buf_end {
                        let s = (off - ra.start) as usize;
                        reply.data(&ra.data[s..s + sz]);
                        return;
                    }
                }
            }
        }

        // Check file_cache synchronously too.
        {
            let cached_local = self.cache.safe_lock().file_cache.get(&path)
                .filter(|fc| fc.local_path.exists())
                .map(|fc| fc.local_path.clone());
            if let Some(ref local) = cached_local {
                if let Ok(f) = std::fs::File::open(local) {
                    let mut buf = vec![0u8; sz];
                    if let Ok(n) = f.read_at(&mut buf, off) {
                        buf.truncate(n);
                        reply.data(&buf);
                        self.open_files.safe_lock().entry(fh).and_modify(|of| of.local = Some(local.clone()));
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

        thread::spawn(move || {
            let fetch = std::cmp::max(sz, READ_AHEAD);
            match do_range_read(&conn, &path, off, fetch) {
                Ok(data) => {
                    let end = std::cmp::min(sz, data.len());
                    reply.data(&data[..end]);
                    open_files
                        .safe_lock()
                        .entry(fh)
                        .and_modify(|of| of.buf = Some(ReadAheadBuf { start: off, data }));
                }
                Err(e) => {
                    log::warn!("range read failed, falling back to full download: {}", e);
                    match ensure_file_cached(&net, &conn.throttle, &cache, &status, &dirty, path.clone()) {
                        Ok(local) => {
                            if let Ok(f) = std::fs::File::open(&local) {
                                let mut buf = vec![0u8; sz];
                                match f.read_at(&mut buf, off) {
                                    Ok(n) => {
                                        buf.truncate(n);
                                        reply.data(&buf);
                                        open_files.safe_lock().entry(fh).and_modify(
                                            |of| of.local = Some(local),
                                        );
                                        return;
                                    }
                                    Err(_) => {}
                                }
                            }
                            reply.error(EIO);
                        }
                        Err(e2) => {
                            log::error!("fallback download failed {}: {}", path.display(), e2);
                            reply.error(error_to_errno(&e2));
                        }
                    }
                }
            }
        });
    }

    fn release(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.open_files.safe_lock().remove(&fh);
        reply.ok();
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let (path, parent_ino) = {
            let c = self.cache.safe_lock();
            let path = match c.get_path(ino) {
                Some(p) => p,
                None => {
                    reply.error(ENOENT);
                    return;
                }
            };
            let parent_ino = if ino == 1 {
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
        let ipc_populated = self.ipc_populated.clone();
        let deferred_readdir = self.deferred_readdir.clone();
        let conn = self.conn.clone();
        let aggressive_prefetch = self.aggressive_prefetch;

        thread::spawn(move || {
            if offset == 0 {
                if reply.add(ino, 1, FileType::Directory, ".") {
                    reply.ok();
                    return;
                }
                if reply.add(parent_ino, 2, FileType::Directory, "..") {
                    reply.ok();
                    return;
                }
            }

            let should_defer = deferred_readdir.safe_lock().remove(&path);
            let has_data = {
                let c = cache.safe_lock();
                c.dir_cache.contains_key(&path) || c.pending_dirs.contains_key(&path)
            };
            if should_defer && !has_data {
                log::info!("READDIR_DEFERRED {} — returning empty, bg PROPFIND", path.display());
                let conn2 = conn.clone();
                let cache2 = cache.clone();
                let path2 = path.clone();
                let shared2 = shared.clone();
                let fileids2 = fileids.clone();
                let details2 = details.clone();
                let status2 = status.clone();
                let dirty2 = dirty.clone();
                let ipc_populated2 = ipc_populated.clone();
                thread::spawn(move || {
                    match get_or_list_dir(&conn2, &cache2, path2.clone()) {
                        Ok((entries, self_entry)) => {
                            if !ipc_populated2.safe_lock().contains(&path2) {
                                {
                                    let mut c = cache2.safe_lock();
                                    let mut sh = shared2.safe_lock();
                                    let mut fi = fileids2.safe_lock();
                                    let mut dt = details2.safe_lock();
                                    let mut st = status2.safe_lock();
                                    if let Some(ref se) = self_entry {
                                        if se.is_shared { sh.insert(path2.clone()); }
                                        if let Some(fid) = se.fileid { fi.insert(path2.clone(), fid); }
                                        dt.insert(path2.clone(), ipc::FileDetail {
                                            permissions: se.permissions.clone(),
                                            owner_id: se.owner_id.clone(),
                                            owner_display_name: se.owner_display_name.clone(),
                                            size: se.size,
                                            is_dir: se.is_dir,
                                        });
                                    }
                                    for entry in entries.iter() {
                                        let name = match entry.path.file_name().and_then(|n| n.to_str()) {
                                            Some(n) => n,
                                            None => continue,
                                        };
                                        let ep = path2.join(name);
                                        c.allocate_inode(ep.clone());
                                        if entry.is_shared { sh.insert(ep.clone()); }
                                        if let Some(fid) = entry.fileid { fi.insert(ep.clone(), fid); }
                                        dt.insert(ep.clone(), ipc::FileDetail {
                                            permissions: entry.permissions.clone(),
                                            owner_id: entry.owner_id.clone(),
                                            owner_display_name: entry.owner_display_name.clone(),
                                            size: entry.size,
                                            is_dir: entry.is_dir,
                                        });
                                        if !entry.is_dir {
                                            let rel = ep.strip_prefix("/").unwrap_or(&ep);
                                            let local_path = c.cache_dir.join(rel);
                                            if local_path.exists() {
                                                st.insert(ep.clone(), FileStatus::Local);
                                                c.file_cache.entry(ep).or_insert(FileCacheEntry {
                                                    local_path,
                                                    remote_modified: entry.modified,
                                                });
                                            } else {
                                                st.entry(ep).or_insert(FileStatus::Remote);
                                            }
                                        }
                                    }
                                }
                                {
                                    let mut d = dirty2.safe_lock();
                                    d.insert(path2.clone());
                                    for entry in entries.iter() {
                                        if let Some(name) = entry.path.file_name().and_then(|n| n.to_str()) {
                                            d.insert(path2.join(name));
                                        }
                                    }
                                }
                                ipc_populated2.safe_lock().insert(path2.clone());
                                log::info!("READDIR_DEFERRED {} completed: {} entries", path2.display(), entries.len());
                            }
                        }
                        Err(e) => log::warn!("bg readdir {}: {}", path2.display(), e),
                    }
                });
                reply.ok();
                return;
            }

            let t_readdir = Instant::now();
            match get_or_list_dir(&conn, &cache, path.clone()) {
                Ok((entries, self_entry)) => {
                    log::info!("READDIR {} get_or_list_dir returned {} entries in {:?}", path.display(), entries.len(), t_readdir.elapsed());
                    let mut thumb_candidates: Vec<(PathBuf, Option<SystemTime>, bool, Option<u64>)> = Vec::new();

                    let already_populated = ipc_populated.safe_lock().contains(&path);
                    if !already_populated {
                        let mut c = cache.safe_lock();
                        let mut sh = shared.safe_lock();
                        let mut fi = fileids.safe_lock();
                        let mut dt = details.safe_lock();
                        let mut st = status.safe_lock();

                        if let Some(ref se) = self_entry {
                            if se.is_shared {
                                sh.insert(path.clone());
                            }
                            if let Some(fid) = se.fileid {
                                fi.insert(path.clone(), fid);
                            }
                            dt.insert(path.clone(), ipc::FileDetail {
                                permissions: se.permissions.clone(),
                                owner_id: se.owner_id.clone(),
                                owner_display_name: se.owner_display_name.clone(),
                                size: se.size,
                                is_dir: se.is_dir,
                            });
                        }

                        for entry in entries.iter() {
                            let name = match entry.path.file_name().and_then(|n| n.to_str()) {
                                Some(n) => n,
                                None => continue,
                            };
                            let entry_path = path.join(name);
                            c.allocate_inode(entry_path.clone());
                            if entry.is_shared {
                                sh.insert(entry_path.clone());
                            }
                            if let Some(fid) = entry.fileid {
                                fi.insert(entry_path.clone(), fid);
                            }
                            dt.insert(entry_path.clone(), ipc::FileDetail {
                                permissions: entry.permissions.clone(),
                                owner_id: entry.owner_id.clone(),
                                owner_display_name: entry.owner_display_name.clone(),
                                size: entry.size,
                                is_dir: entry.is_dir,
                            });
                            if !entry.is_dir {
                                let rel = entry_path.strip_prefix("/").unwrap_or(&entry_path);
                                let local_path = c.cache_dir.join(rel);
                                if local_path.exists() {
                                    st.insert(entry_path.clone(), FileStatus::Local);
                                    c.file_cache.entry(entry_path.clone()).or_insert(FileCacheEntry {
                                        local_path,
                                        remote_modified: entry.modified,
                                    });
                                } else {
                                    st.entry(entry_path.clone()).or_insert(FileStatus::Remote);
                                }
                                thumb_candidates.push((entry_path, entry.modified, entry.has_preview, entry.fileid));
                            }
                        }

                        {
                            let mut d = dirty.safe_lock();
                            d.insert(path.clone());
                            for entry in entries.iter() {
                                if let Some(name) = entry.path.file_name().and_then(|n| n.to_str()) {
                                    d.insert(path.join(name));
                                }
                            }
                        }
                        ipc_populated.safe_lock().insert(path.clone());
                        log::info!("READDIR {} populated IPC maps: {} entries, self_entry={}", path.display(), entries.len(), self_entry.is_some());
                    }

                    let skip = if offset > 2 { (offset - 2) as usize } else { 0 };
                    {
                        let c = cache.safe_lock();
                        for (i, entry) in entries.iter().enumerate().skip(skip) {
                            let name = match entry.path.file_name().and_then(|n| n.to_str()) {
                                Some(n) => n,
                                None => continue,
                            };
                            let entry_path = path.join(name);
                            let entry_ino = c.get_inode(&entry_path).unwrap_or(1);
                            let kind =
                                if entry.is_dir { FileType::Directory } else { FileType::RegularFile };
                            if reply.add(entry_ino, (i + 3) as i64, kind, name) {
                                break;
                            }
                        }
                    }

                    if offset == 0 {
                        let mut dr = deferred_readdir.safe_lock();
                        for entry in entries.iter() {
                            if entry.is_dir {
                                if let Some(name) = entry.path.file_name().and_then(|n| n.to_str()) {
                                    dr.insert(path.join(name));
                                }
                            }
                        }
                    }

                    log::info!("READDIR {} reply.ok() at {:?}", path.display(), t_readdir.elapsed());
                    reply.ok();
                    save_dir_cache(&cache);
                    log::info!("READDIR {} save_dir_cache done at {:?}", path.display(), t_readdir.elapsed());

                    if offset == 0 {
                        let child_dirs: Vec<PathBuf> = entries.iter()
                            .filter(|e| e.is_dir)
                            .filter_map(|e| e.path.file_name().and_then(|n| n.to_str()).map(|n| path.join(n)))
                            .collect();
                        if aggressive_prefetch && !child_dirs.is_empty() {
                            let conn_pf = conn.clone();
                            let cache_pf = cache.clone();
                            thread::spawn(move || {
                                log::info!("PREFETCH_CHILDREN {} dirs from {}", path.display(), child_dirs.len());
                                for chunk in child_dirs.chunks(10) {
                                    let handles: Vec<_> = chunk.iter().map(|dir| {
                                        let c = conn_pf.clone();
                                        let ca = cache_pf.clone();
                                        let d = dir.clone();
                                        thread::spawn(move || {
                                            let _ = get_or_list_dir(&c, &ca, d);
                                        })
                                    }).collect();
                                    for h in handles {
                                        let _ = h.join();
                                    }
                                }
                                save_dir_cache(&cache_pf);

                                if aggressive_prefetch {
                                    let mut child_thumbs: Vec<(PathBuf, Option<SystemTime>, bool, Option<u64>)> = Vec::new();
                                    let mut c = cache_pf.safe_lock();
                                    for dir in &child_dirs {
                                        if let Some((entries, _)) = c.get_cached_dir(dir) {
                                            for entry in entries.iter() {
                                                if !entry.is_dir && entry.has_preview {
                                                    if let Some(name) = entry.path.file_name().and_then(|n| n.to_str()) {
                                                        child_thumbs.push((dir.join(name), entry.modified, true, entry.fileid));
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    drop(c);
                                    if !child_thumbs.is_empty() {
                                        log::info!("PREFETCH_CHILD_THUMBS {} thumbnails", child_thumbs.len());
                                        preview::prefetch_directory_thumbnails(
                                            &conn_pf.http,
                                            &conn_pf.base_url,
                                            &conn_pf.username,
                                            &conn_pf.password,
                                            &conn_pf.mount_point,
                                            &child_thumbs,
                                        );
                                    }
                                }
                            });
                        }

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
        &mut self,
        _req: &Request,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        if let Some(new_size) = size {
            if let Some(fh) = fh {
                let mut files = self.open_files.safe_lock();
                if let Some(of) = files.get_mut(&fh) {
                    let wp = of.write_path.get_or_insert_with(|| {
                        let cache_dir = self.cache.safe_lock().cache_dir.clone();
                        cache_dir.join(format!("write_{}", fh))
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
            let attr = make_dir_attr(ino);
            reply.attr(&TTL, &attr);
        } else {
            self.getattr(_req, ino, reply);
        }
    }

    fn write(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let path = match self.cache.safe_lock().get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        log::debug!("[{}] WRITE {} offset={} len={}", self.log_user, path.display(), offset, data.len());

        let mut files = self.open_files.safe_lock();
        let of = match files.get_mut(&fh) {
            Some(of) => of,
            None => {
                reply.error(EIO);
                return;
            }
        };

        let wp = of.write_path.get_or_insert_with(|| {
            let cache_dir = self.cache.safe_lock().cache_dir.clone();
            cache_dir.join(format!("write_{}", fh))
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
                match f.write_at(data, offset as u64) {
                    Ok(n) => {
                        of.dirty = true;
                        reply.written(n as u32);
                    }
                    Err(e) => {
                        log::error!("write to staging file: {}", e);
                        reply.error(EIO);
                    }
                }
            }
            Err(e) => {
                log::error!("open staging file: {}", e);
                reply.error(EIO);
            }
        }
    }

    fn flush(&mut self, _req: &Request, _ino: u64, fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        let (remote_path, write_path, original_etag) = {
            let files = self.open_files.safe_lock();
            match files.get(&fh) {
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

        let body = match std::fs::read(&write_path) {
            Ok(b) => b,
            Err(e) => {
                log::error!("read staging file for flush: {}", e);
                reply.error(EIO);
                return;
            }
        };

        let conn = self.conn.clone();
        let cache = self.cache.clone();
        let dirty = self.dirty.clone();
        let open_files = self.open_files.clone();

        thread::spawn(move || {
            let _permit = conn.throttle.acquire();
            let etag_ref = original_etag.as_deref();
            match webdav_ops::put_file(&conn.http, &conn.base_url, &conn.username, &conn.password, &remote_path, body.clone(), etag_ref) {
                Ok(result) => {
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
                            dir.at = Instant::now();
                        }
                    }
                    if let Some(of) = open_files.safe_lock().get_mut(&fh) {
                        of.dirty = false;
                        of.original_etag = result.new_etag;
                    }
                    dirty.safe_lock().insert(remote_path.parent().unwrap_or(Path::new("/")).to_path_buf());
                    reply.ok();
                }
                Err(webdav_ops::WriteError::Conflict) => {
                    log::warn!("CONFLICT on PUT {} — creating conflicted copy", remote_path.display());
                    let conflict_name = make_conflict_name(&remote_path);
                    match webdav_ops::put_file(&conn.http, &conn.base_url, &conn.username, &conn.password, &conflict_name, body, None) {
                        Ok(_) => log::info!("conflicted copy uploaded as {}", conflict_name.display()),
                        Err(e) => log::error!("failed to upload conflict copy: {}", e),
                    }
                    dirty.safe_lock().insert(remote_path.parent().unwrap_or(Path::new("/")).to_path_buf());
                    reply.ok();
                }
                Err(e) => {
                    log::error!("PUT {} failed: {}", remote_path.display(), e);
                    reply.error(EIO);
                }
            }
        });
    }

    fn create(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        if let Err(e) = filename_validation::validate(name) {
            log::warn!("create rejected: {}", e);
            reply.error(e.to_errno());
            return;
        }

        let parent_path = match self.cache.safe_lock().get_path(parent) {
            Some(p) => p,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        let file_name = name.to_string_lossy().to_string();
        let remote_path = parent_path.join(&file_name);

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
        reply.created(&TTL, &attr, 0, fh, 0);
    }

    fn mkdir(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        if let Err(e) = filename_validation::validate(name) {
            log::warn!("mkdir rejected: {}", e);
            reply.error(e.to_errno());
            return;
        }

        let parent_path = match self.cache.safe_lock().get_path(parent) {
            Some(p) => p,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        let dir_name = name.to_string_lossy().to_string();
        let remote_path = parent_path.join(&dir_name);

        let conn = self.conn.clone();
        let cache = self.cache.clone();
        let dirty = self.dirty.clone();

        thread::spawn(move || {
            let _permit = conn.throttle.acquire();
            match webdav_ops::mkcol(&conn.http, &conn.base_url, &conn.username, &conn.password, &remote_path) {
                Ok(()) => {
                    log::info!("MKCOL {}", remote_path.display());
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
                    let mut c = cache.safe_lock();
                    let ino = c.allocate_inode(remote_path.clone());
                    if let Some(dir) = c.dir_cache.get_mut(&parent_path) {
                        let mut files = (*dir.files).clone();
                        files.push(new_entry);
                        dir.files = Arc::new(files);
                    }
                    drop(c);
                    dirty.safe_lock().insert(parent_path);
                    reply.entry(&TTL, &make_dir_attr(ino), 0);
                }
                Err(e) => {
                    log::error!("MKCOL {} failed: {}", remote_path.display(), e);
                    reply.error(EIO);
                }
            }
        });
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let parent_path = match self.cache.safe_lock().get_path(parent) {
            Some(p) => p,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        let file_name = name.to_string_lossy().to_string();
        let remote_path = parent_path.join(&file_name);

        let conn = self.conn.clone();
        let cache = self.cache.clone();
        let dirty = self.dirty.clone();

        thread::spawn(move || {
            let _permit = conn.throttle.acquire();
            match webdav_ops::delete(&conn.http, &conn.base_url, &conn.username, &conn.password, &remote_path) {
                Ok(()) => {
                    log::info!("DELETE {}", remote_path.display());
                    let mut c = cache.safe_lock();
                    if let Some(dir) = c.dir_cache.get_mut(&parent_path) {
                        let files: Vec<DavEntry> = dir.files.iter().filter(|e| e.path != remote_path).cloned().collect();
                        dir.files = Arc::new(files);
                    }
                    drop(c);
                    dirty.safe_lock().insert(parent_path);
                    reply.ok();
                }
                Err(e) => {
                    log::error!("DELETE {} failed: {}", remote_path.display(), e);
                    reply.error(EIO);
                }
            }
        });
    }

    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let parent_path = match self.cache.safe_lock().get_path(parent) {
            Some(p) => p,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        let dir_name = name.to_string_lossy().to_string();
        let remote_path = parent_path.join(&dir_name);

        let conn = self.conn.clone();
        let cache = self.cache.clone();
        let dirty = self.dirty.clone();

        thread::spawn(move || {
            let _permit = conn.throttle.acquire();
            match webdav_ops::delete(&conn.http, &conn.base_url, &conn.username, &conn.password, &remote_path) {
                Ok(()) => {
                    log::info!("RMDIR {}", remote_path.display());
                    let mut c = cache.safe_lock();
                    if let Some(dir) = c.dir_cache.get_mut(&parent_path) {
                        let files: Vec<DavEntry> = dir.files.iter().filter(|e| e.path != remote_path).cloned().collect();
                        dir.files = Arc::new(files);
                    }
                    c.dir_cache.remove(&remote_path);
                    drop(c);
                    dirty.safe_lock().insert(parent_path);
                    reply.ok();
                }
                Err(e) => {
                    log::error!("RMDIR {} failed: {}", remote_path.display(), e);
                    reply.error(EIO);
                }
            }
        });
    }

    fn rename(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        if let Err(e) = filename_validation::validate(newname) {
            log::warn!("rename rejected: {}", e);
            reply.error(e.to_errno());
            return;
        }

        let (old_parent_path, new_parent_path) = {
            let c = self.cache.safe_lock();
            match (c.get_path(parent), c.get_path(newparent)) {
                (Some(a), Some(b)) => (a, b),
                _ => {
                    reply.error(ENOENT);
                    return;
                }
            }
        };

        let old_name = name.to_string_lossy().to_string();
        let new_name = newname.to_string_lossy().to_string();
        let from = old_parent_path.join(&old_name);
        let to = new_parent_path.join(&new_name);

        let conn = self.conn.clone();
        let cache = self.cache.clone();
        let dirty = self.dirty.clone();

        thread::spawn(move || {
            let _permit = conn.throttle.acquire();
            match webdav_ops::move_resource(&conn.http, &conn.base_url, &conn.username, &conn.password, &from, &to) {
                Ok(()) => {
                    log::info!("MOVE {} → {}", from.display(), to.display());
                    let mut c = cache.safe_lock();
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
                    drop(c);
                    let same_parent = old_parent_path == new_parent_path;
                    dirty.safe_lock().insert(old_parent_path);
                    if !same_parent {
                        dirty.safe_lock().insert(new_parent_path);
                    }
                    reply.ok();
                }
                Err(e) => {
                    log::error!("MOVE {} → {} failed: {}", from.display(), to.display(), e);
                    reply.error(EIO);
                }
            }
        });
    }
}

// ── HTTP Range reads ─────────────────────────────────────────────────────────

fn webdav_file_url(base: &str, remote_path: &Path) -> String {
    let rel = remote_path.strip_prefix("/").unwrap_or(remote_path);
    let encoded = utf8_percent_encode(&rel.to_string_lossy(), PATH_ENCODE).to_string();
    format!("{}/{}", base.trim_end_matches('/'), encoded)
}

fn do_range_read(conn: &ConnInfo, path: &Path, offset: u64, size: usize) -> Result<Vec<u8>, String> {
    if conn.is_offline.load(Ordering::Relaxed) {
        return Err("file not available offline".into());
    }
    let _permit = conn.throttle.acquire();
    log::debug!("RANGE_READ {} offset={} size={}", path.display(), offset, size);
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
        resp.bytes().map(|b| b.to_vec()).map_err(|e| e.to_string())
    } else {
        Err(format!("range read returned {}", status))
    }
}

// ── Mount ─────────────────────────────────────────────────────────────────────

pub fn mount_ncfs(options: MountOptions) -> Result<(), String> {
    let filesystem = NextCloudFs::new(options.clone())?;
    let keep_cb = filesystem.keep_callback();
    let evict_cb = filesystem.evict_callback();
    let prefetch_cb = filesystem.prefetch_callback();
    let base_url = notifications::base_url(&options.url);
    let username = options.username.clone().unwrap_or_default();
    ipc::start_server(options.mount_point.clone(), filesystem.status_map(), filesystem.shared_set(), filesystem.fileid_map(), filesystem.detail_map(), filesystem.dirty_set(), username, base_url, Some(keep_cb), Some(evict_cb), Some(prefetch_cb));

    // Connectivity monitor
    let offline_flag = filesystem.is_offline_flag();
    if !options.offline {
        let http = filesystem.conn_http();
        let webdav_url = options.url.clone();
        let probe_user = options.username.clone().unwrap_or_default();
        let probe_pass = options.password.clone().unwrap_or_default();
        let offline = offline_flag.clone();
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
                    log::info!("CONNECTIVITY restored");
                } else if !was_offline && !reachable {
                    log::warn!("CONNECTIVITY lost — serving from cache");
                }
            }
        });
    }

    if !options.offline {
        notify_push::start(
            filesystem.conn_http(),
            notifications::base_url(&options.url),
            options.username.clone().unwrap_or_default(),
            options.password.clone().unwrap_or_default(),
            filesystem.cache_ref(),
            filesystem.dirty_set(),
            offline_flag,
        );
    }

    let fuse_options = vec![
        MountOption::FSName("ncrs".to_string()),
        MountOption::AutoUnmount,
    ];

    let mp_str = options.mount_point.to_string_lossy().to_string();
    let _ = std::process::Command::new("fusermount")
        .args(["-uz", &mp_str])
        .output();

    log::info!(
        "Mounting WebDAV {} at {}",
        options.url,
        options.mount_point.display()
    );

    fuser::mount2(filesystem, &options.mount_point, &fuse_options)
        .map_err(|e| format!("FUSE mount failed: {}", e))
}

// ── Utilities ─────────────────────────────────────────────────────────────────

fn make_conflict_name(path: &Path) -> PathBuf {
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

    Ok(MountOptions { url, username, password, mount_point, log_user, aggressive_prefetch, http3, max_concurrent_requests, offline: false })
}
