pub mod config;
pub mod ipc;
pub mod notifications;
pub mod preview;
pub mod search;

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, Request,
};
use libc::{EIO, ENOENT};
use remotefs::fs::FileType as RemoteFileType;
use remotefs::RemoteFs;
use remotefs_webdav::WebDAVFs;
use ipc::{FileStatus, StatusMap};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use serde::{Deserialize, Serialize};
use yaml_rust2::YamlLoader;

#[cfg(unix)]
use std::os::unix::fs::FileExt;

const TTL: Duration = Duration::from_secs(1);
const DIR_CACHE_TTL: Duration = Duration::from_secs(10);
const PROPFIND_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const READ_AHEAD: usize = 2 * 1024 * 1024; // 2 MB

const PATH_ENCODE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'#')
    .add(b'%')
    .add(b'?')
    .add(b'[')
    .add(b']')
    .add(b'{')
    .add(b'}');

// ── Cache data types ──────────────────────────────────────────────────────────

struct DirCacheEntry {
    files: Vec<remotefs::fs::File>,
    at: Instant,
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
    #[allow(dead_code)]
    remote_path: PathBuf,
    local: Option<PathBuf>,
    buf: Option<ReadAheadBuf>,
}

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MountOptions {
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub mount_point: PathBuf,
    pub log_user: String,
}

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
    webdav: Mutex<WebDAVFs>,
}

fn list_dir_timeout(
    net: &Arc<FsNetwork>,
    path: PathBuf,
) -> Result<Vec<remotefs::fs::File>, String> {
    let (tx, rx) = mpsc::channel();
    let n = net.clone();
    thread::spawn(move || {
        let r = n.webdav.lock().unwrap().list_dir(&path).map_err(|e| e.to_string());
        let _ = tx.send(r);
    });
    rx.recv_timeout(PROPFIND_TIMEOUT)
        .unwrap_or_else(|_| Err("WebDAV PROPFIND timeout".into()))
}

fn open_file_timeout(
    net: &Arc<FsNetwork>,
    path: PathBuf,
    dest: std::fs::File,
) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    let n = net.clone();
    thread::spawn(move || {
        let r = n
            .webdav
            .lock()
            .unwrap()
            .open_file(&path, Box::new(dest))
            .map(|_| ())
            .map_err(|e| e.to_string());
        let _ = tx.send(r);
    });
    rx.recv_timeout(DOWNLOAD_TIMEOUT)
        .unwrap_or_else(|_| Err("WebDAV download timeout".into()))
}

// ── Cache layer ───────────────────────────────────────────────────────────────

struct FsCache {
    inodes: HashMap<u64, PathBuf>,
    paths: HashMap<PathBuf, u64>,
    next_inode: u64,
    dir_cache: HashMap<PathBuf, DirCacheEntry>,
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

    fn check_dir_cache(&self, path: &Path) -> Option<Vec<remotefs::fs::File>> {
        self.dir_cache
            .get(path)
            .filter(|e| e.at.elapsed() < DIR_CACHE_TTL)
            .map(|e| e.files.clone())
    }

    fn put_dir_cache(&mut self, path: PathBuf, files: Vec<remotefs::fs::File>) {
        self.dir_cache.insert(path, DirCacheEntry { files, at: Instant::now() });
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
            .and_then(|e| e.metadata.modified)
    }
}

// ── Shared operation helpers ──────────────────────────────────────────────────

fn decode_remote_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    let decoded = percent_encoding::percent_decode_str(&s)
        .decode_utf8_lossy()
        .into_owned();
    PathBuf::from(decoded)
}

fn get_or_list_dir(
    net: &Arc<FsNetwork>,
    cache: &Arc<Mutex<FsCache>>,
    path: PathBuf,
) -> Result<Vec<remotefs::fs::File>, String> {
    if let Some(files) = cache.lock().unwrap().check_dir_cache(&path) {
        return Ok(files);
    }
    let mut files = list_dir_timeout(net, path.clone())?;
    for f in &mut files {
        f.path = decode_remote_path(&f.path);
    }
    cache.lock().unwrap().put_dir_cache(path, files.clone());
    Ok(files)
}

fn ensure_file_cached(
    net: &Arc<FsNetwork>,
    cache: &Arc<Mutex<FsCache>>,
    status: &StatusMap,
    remote_path: PathBuf,
) -> Result<PathBuf, String> {
    let (maybe_local, cached_mod, current_mod, cache_dir) = {
        let c = cache.lock().unwrap();
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

    let rel = remote_path.strip_prefix("/").unwrap_or(&remote_path);
    let local_path = cache_dir.join(rel);
    if let Some(parent) = local_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let file =
        std::fs::File::create(&local_path).map_err(|e| format!("create cache file: {}", e))?;
    open_file_timeout(net, remote_path.clone(), file)?;

    {
        let mut c = cache.lock().unwrap();
        let mod_time = c.remote_modified_for(&remote_path);
        c.file_cache.insert(
            remote_path.clone(),
            FileCacheEntry { local_path: local_path.clone(), remote_modified: mod_time },
        );
    }
    status.lock().unwrap().insert(remote_path, FileStatus::Local);
    Ok(local_path)
}

// ── FileAttr helpers ──────────────────────────────────────────────────────────

fn make_file_attr(inode: u64, metadata: &remotefs::fs::Metadata) -> FileAttr {
    let modified = metadata.modified.unwrap_or(UNIX_EPOCH);
    let is_dir = metadata.file_type == RemoteFileType::Directory;
    FileAttr {
        ino: inode,
        size: metadata.size,
        blocks: (metadata.size + 511) / 512,
        atime: modified,
        mtime: modified,
        ctime: modified,
        crtime: modified,
        kind: if is_dir { FileType::Directory } else { FileType::RegularFile },
        perm: if is_dir { 0o755 } else { 0o644 },
        nlink: if is_dir { 2 } else { 1 },
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
}

pub struct NextCloudFs {
    net: Arc<FsNetwork>,
    cache: Arc<Mutex<FsCache>>,
    status: StatusMap,
    conn: Arc<ConnInfo>,
    open_files: Arc<Mutex<HashMap<u64, OpenFile>>>,
    next_fh: Arc<Mutex<u64>>,
    log_user: String,
}

impl NextCloudFs {
    pub fn new(options: MountOptions) -> Result<Self, String> {
        let username = options.username.unwrap_or_default();
        let password = options.password.unwrap_or_default();
        let mut webdav = WebDAVFs::new(&username, &password, &options.url);
        webdav.connect().map_err(|e| format!("WebDAV connect failed: {}", e))?;

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

        let conn = Arc::new(ConnInfo {
            base_url: notifications::base_url(&options.url),
            webdav_url: options.url.clone(),
            username: username.clone(),
            password: password.clone(),
            mount_point: options.mount_point.clone(),
        });

        Ok(NextCloudFs {
            net: Arc::new(FsNetwork { webdav: Mutex::new(webdav) }),
            cache: Arc::new(Mutex::new(FsCache {
                inodes,
                paths,
                next_inode: 2,
                dir_cache: HashMap::new(),
                file_cache: HashMap::new(),
                cache_dir,
            })),
            status,
            conn,
            open_files: Arc::new(Mutex::new(HashMap::new())),
            next_fh: Arc::new(Mutex::new(1)),
            log_user: options.log_user,
        })
    }

    pub fn status_map(&self) -> StatusMap {
        self.status.clone()
    }
}

impl Filesystem for NextCloudFs {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let (parent_path, name_str) = {
            let c = self.cache.lock().unwrap();
            match (c.get_path(parent), name.to_str()) {
                (Some(p), Some(n)) => (p, n.to_string()),
                _ => {
                    reply.error(ENOENT);
                    return;
                }
            }
        };

        log::debug!("[{}] LOOKUP {}/{}", self.log_user, parent_path.display(), name_str);

        let net = self.net.clone();
        let cache = self.cache.clone();
        let status = self.status.clone();

        thread::spawn(move || {
            match get_or_list_dir(&net, &cache, parent_path.clone()) {
                Ok(entries) => {
                    for entry in &entries {
                        let entry_name =
                            entry.path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                        if entry_name == name_str {
                            let target_path = parent_path.join(&name_str);
                            let ino = cache.lock().unwrap().allocate_inode(target_path.clone());
                            let attr = make_file_attr(ino, &entry.metadata);
                            if entry.metadata.file_type != RemoteFileType::Directory {
                                status
                                    .lock()
                                    .unwrap()
                                    .entry(target_path)
                                    .or_insert(FileStatus::Remote);
                            }
                            reply.entry(&TTL, &attr, 0);
                            return;
                        }
                    }
                    reply.error(ENOENT);
                }
                Err(e) => {
                    log::error!("lookup {}/{}: {}", parent_path.display(), name_str, e);
                    reply.error(EIO);
                }
            }
        });
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        if ino == 1 {
            reply.attr(&TTL, &root_attr());
            return;
        }

        let path = match self.cache.lock().unwrap().get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        log::debug!("[{}] GETATTR {}", self.log_user, path.display());

        let parent = path.parent().unwrap_or(Path::new("/")).to_path_buf();
        let file_name =
            path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();

        let net = self.net.clone();
        let cache = self.cache.clone();

        thread::spawn(move || {
            match get_or_list_dir(&net, &cache, parent.clone()) {
                Ok(entries) => {
                    for entry in &entries {
                        if entry
                            .path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("")
                            == file_name
                        {
                            reply.attr(&TTL, &make_file_attr(ino, &entry.metadata));
                            return;
                        }
                    }
                    reply.error(ENOENT);
                }
                Err(e) => {
                    log::error!("getattr {}: {}", parent.display(), e);
                    reply.error(EIO);
                }
            }
        });
    }

    fn open(&mut self, _req: &Request, ino: u64, _flags: i32, reply: ReplyOpen) {
        let (path, local) = {
            let c = self.cache.lock().unwrap();
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
            (path, local)
        };

        let fh = {
            let mut n = self.next_fh.lock().unwrap();
            let fh = *n;
            *n += 1;
            fh
        };

        self.open_files
            .lock()
            .unwrap()
            .insert(fh, OpenFile { remote_path: path, local, buf: None });
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
        let path = match self.cache.lock().unwrap().get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        log::debug!("[{}] READ {} offset={} size={}", self.log_user, path.display(), offset, size);

        let open_files = self.open_files.clone();
        let conn = self.conn.clone();
        let net = self.net.clone();
        let cache = self.cache.clone();
        let status = self.status.clone();

        thread::spawn(move || {
            let off = offset as u64;
            let sz = size as usize;

            // Try serving from open-file state (local cache or read-ahead buffer).
            {
                let files = open_files.lock().unwrap();
                if let Some(of) = files.get(&fh) {
                    if let Some(ref local) = of.local {
                        if let Ok(f) = std::fs::File::open(local) {
                            let mut buf = vec![0u8; sz];
                            match f.read_at(&mut buf, off) {
                                Ok(n) => {
                                    buf.truncate(n);
                                    reply.data(&buf);
                                    return;
                                }
                                Err(_) => {}
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

            // HTTP Range read with read-ahead.
            let fetch = std::cmp::max(sz, READ_AHEAD);
            match range_read_timeout(&conn, &path, off, fetch) {
                Ok(data) => {
                    let end = std::cmp::min(sz, data.len());
                    reply.data(&data[..end]);
                    open_files
                        .lock()
                        .unwrap()
                        .entry(fh)
                        .and_modify(|of| of.buf = Some(ReadAheadBuf { start: off, data }));
                }
                Err(e) => {
                    log::warn!("range read failed, falling back to full download: {}", e);
                    match ensure_file_cached(&net, &cache, &status, path.clone()) {
                        Ok(local) => {
                            if let Ok(f) = std::fs::File::open(&local) {
                                let mut buf = vec![0u8; sz];
                                match f.read_at(&mut buf, off) {
                                    Ok(n) => {
                                        buf.truncate(n);
                                        reply.data(&buf);
                                        open_files.lock().unwrap().entry(fh).and_modify(
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
                            reply.error(EIO);
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
        self.open_files.lock().unwrap().remove(&fh);
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
            let c = self.cache.lock().unwrap();
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

        log::debug!("[{}] READDIR {}", self.log_user, path.display());

        let net = self.net.clone();
        let cache = self.cache.clone();
        let status = self.status.clone();
        let conn = self.conn.clone();

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

            match get_or_list_dir(&net, &cache, path.clone()) {
                Ok(entries) => {
                    let skip = if offset > 2 { (offset - 2) as usize } else { 0 };
                    let mut thumb_candidates: Vec<(PathBuf, Option<SystemTime>)> = Vec::new();
                    for (i, entry) in entries.iter().enumerate().skip(skip) {
                        let name = match entry.path.file_name().and_then(|n| n.to_str()) {
                            Some(n) => n.to_string(),
                            None => continue,
                        };
                        let entry_path = path.join(&name);
                        let is_dir = entry.metadata.file_type == RemoteFileType::Directory;
                        let entry_ino =
                            cache.lock().unwrap().allocate_inode(entry_path.clone());
                        if !is_dir {
                            status
                                .lock()
                                .unwrap()
                                .entry(entry_path.clone())
                                .or_insert(FileStatus::Remote);
                            thumb_candidates.push((entry_path, entry.metadata.modified));
                        }
                        let kind =
                            if is_dir { FileType::Directory } else { FileType::RegularFile };
                        if reply.add(entry_ino, (i + 3) as i64, kind, &name) {
                            break;
                        }
                    }
                    reply.ok();

                    if !thumb_candidates.is_empty() {
                        thread::spawn(move || {
                            preview::prefetch_directory_thumbnails(
                                &conn.base_url,
                                &conn.username,
                                &conn.password,
                                &conn.mount_point,
                                &thumb_candidates,
                            );
                        });
                    }
                }
                Err(e) => {
                    log::error!("readdir {}: {}", path.display(), e);
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

fn range_read_timeout(conn: &Arc<ConnInfo>, path: &Path, offset: u64, size: usize) -> Result<Vec<u8>, String> {
    let (tx, rx) = mpsc::channel();
    let c = conn.clone();
    let p = path.to_path_buf();
    thread::spawn(move || {
        let _ = tx.send(do_range_read(&c, &p, offset, size));
    });
    rx.recv_timeout(DOWNLOAD_TIMEOUT)
        .unwrap_or_else(|_| Err("range read timeout".into()))
}

fn do_range_read(conn: &ConnInfo, path: &Path, offset: u64, size: usize) -> Result<Vec<u8>, String> {
    let url = webdav_file_url(&conn.webdav_url, path);
    let end = offset + size as u64 - 1;
    let client = reqwest::blocking::Client::builder()
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(&url)
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
    ipc::start_server(options.mount_point.clone(), filesystem.status_map());

    let fuse_options = vec![
        MountOption::RO,
        MountOption::FSName("ncrs".to_string()),
        MountOption::AutoUnmount,
    ];

    log::info!(
        "Mounting WebDAV {} at {}",
        options.url,
        options.mount_point.display()
    );

    fuser::mount2(filesystem, &options.mount_point, &fuse_options)
        .map_err(|e| format!("FUSE mount failed: {}", e))
}

// ── Utilities ─────────────────────────────────────────────────────────────────

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

    Ok(MountOptions { url, username, password, mount_point, log_user })
}
