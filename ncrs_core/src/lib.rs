pub mod config;
pub mod ipc;

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry,
    Request,
};
use libc::{EIO, ENOENT};
use remotefs::fs::FileType as RemoteFileType;
use remotefs::RemoteFs;
use remotefs_webdav::WebDAVFs;
use ipc::{FileStatus, StatusMap};
use serde::{Deserialize, Serialize};
use yaml_rust2::YamlLoader;

const TTL: Duration = Duration::from_secs(1);
const DIR_CACHE_TTL: Duration = Duration::from_secs(10);
const PROPFIND_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);

// ── Cache data types ──────────────────────────────────────────────────────────

struct DirCacheEntry {
    files: Vec<remotefs::fs::File>,
    at: Instant,
}

struct FileCacheEntry {
    local_path: PathBuf,
    remote_modified: Option<SystemTime>,
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

fn get_or_list_dir(
    net: &Arc<FsNetwork>,
    cache: &Arc<Mutex<FsCache>>,
    path: PathBuf,
) -> Result<Vec<remotefs::fs::File>, String> {
    if let Some(files) = cache.lock().unwrap().check_dir_cache(&path) {
        return Ok(files);
    }
    let files = list_dir_timeout(net, path.clone())?;
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

pub struct NextCloudFs {
    net: Arc<FsNetwork>,
    cache: Arc<Mutex<FsCache>>,
    status: StatusMap,
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

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
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

        let net = self.net.clone();
        let cache = self.cache.clone();
        let status = self.status.clone();

        thread::spawn(move || {
            match ensure_file_cached(&net, &cache, &status, path.clone()) {
                Ok(local) => match std::fs::read(&local) {
                    Ok(data) => {
                        let off = offset as usize;
                        if off >= data.len() {
                            reply.data(&[]);
                        } else {
                            let end = std::cmp::min(off + size as usize, data.len());
                            reply.data(&data[off..end]);
                        }
                    }
                    Err(e) => {
                        log::error!("read cache {}: {}", local.display(), e);
                        reply.error(EIO);
                    }
                },
                Err(e) => {
                    log::error!("ensure_file_cached {}: {}", path.display(), e);
                    reply.error(EIO);
                }
            }
        });
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
                                .entry(entry_path)
                                .or_insert(FileStatus::Remote);
                        }
                        let kind =
                            if is_dir { FileType::Directory } else { FileType::RegularFile };
                        if reply.add(entry_ino, (i + 3) as i64, kind, &name) {
                            break;
                        }
                    }
                    reply.ok();
                }
                Err(e) => {
                    log::error!("readdir {}: {}", path.display(), e);
                    reply.error(EIO);
                }
            }
        });
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
