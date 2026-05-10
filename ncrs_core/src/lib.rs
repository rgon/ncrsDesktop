use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry,
    Request,
};
use libc::{EIO, ENOENT};
use remotefs::fs::FileType as RemoteFileType;
use remotefs::RemoteFs;
use remotefs_webdav::WebDAVFs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use yaml_rust2::YamlLoader;

const TTL: Duration = Duration::from_secs(1);
const DIR_CACHE_TTL: Duration = Duration::from_secs(10);

struct DirCacheEntry {
    files: Vec<remotefs::fs::File>,
    at: Instant,
}

struct FileCacheEntry {
    local_path: PathBuf,
    /// `modified` of the remote file at download time; used for invalidation.
    remote_modified: Option<SystemTime>,
}

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

pub struct NextCloudFs {
    fs: WebDAVFs,
    inodes: HashMap<u64, PathBuf>,
    paths: HashMap<PathBuf, u64>,
    next_inode: u64,
    log_user: String,
    dir_cache: HashMap<PathBuf, DirCacheEntry>,
    file_cache: HashMap<PathBuf, FileCacheEntry>,
    cache_dir: PathBuf,
}

impl NextCloudFs {
    pub fn new(options: MountOptions) -> Result<Self, String> {
        let username = options.username.unwrap_or_default();
        let password = options.password.unwrap_or_default();

        let mut webdav_fs = WebDAVFs::new(&username, &password, &options.url);
        webdav_fs
            .connect()
            .map_err(|e| format!("WebDAV connect failed: {}", e))?;

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

        Ok(NextCloudFs {
            fs: webdav_fs,
            inodes,
            paths,
            next_inode: 2,
            log_user: options.log_user,
            dir_cache: HashMap::new(),
            file_cache: HashMap::new(),
            cache_dir,
        })
    }

    fn get_inode(&self, path: &Path) -> Option<u64> {
        self.paths.get(path).copied()
    }

    fn get_path(&self, inode: u64) -> Option<&PathBuf> {
        self.inodes.get(&inode)
    }

    fn allocate_inode(&mut self, path: PathBuf) -> u64 {
        if let Some(inode) = self.paths.get(&path) {
            return *inode;
        }
        let inode = self.next_inode;
        self.next_inode += 1;
        self.paths.insert(path.clone(), inode);
        self.inodes.insert(inode, path);
        inode
    }

    /// Returns the local cached path for `remote_path`, downloading if stale/absent.
    fn ensure_file_cached(&mut self, remote_path: &Path) -> Result<PathBuf, String> {
        // Snapshot cache state without holding a reference into self.
        let (maybe_local, cached_modified) = match self.file_cache.get(remote_path) {
            Some(e) if e.local_path.exists() => (Some(e.local_path.clone()), e.remote_modified),
            _ => (None, None),
        };

        let current_modified = self.remote_modified_for(remote_path);

        if let Some(local) = maybe_local {
            if cached_modified == current_modified {
                return Ok(local);
            }
        }

        // Download to cache.
        let rel = remote_path.strip_prefix("/").unwrap_or(remote_path);
        let local_path = self.cache_dir.join(rel);
        if let Some(parent) = local_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }

        let mut reader = self
            .fs
            .open(remote_path)
            .map_err(|e| format!("open {}: {}", remote_path.display(), e))?;
        let mut data = Vec::new();
        reader.read_to_end(&mut data).map_err(|e| e.to_string())?;
        std::fs::write(&local_path, &data).map_err(|e| e.to_string())?;

        self.file_cache.insert(
            remote_path.to_path_buf(),
            FileCacheEntry {
                local_path: local_path.clone(),
                remote_modified: current_modified,
            },
        );
        Ok(local_path)
    }

    /// Look up the remote `modified` time via the dir listing cache (no extra network call).
    fn remote_modified_for(&mut self, path: &Path) -> Option<SystemTime> {
        let parent = path.parent().unwrap_or(Path::new("/")).to_path_buf();
        let name = path.file_name()?.to_str()?.to_string();
        let entries = self.list_dir_cached(&parent).ok()?;
        entries.iter().find(|e| {
            e.path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                == name
        }).and_then(|e| e.metadata.modified)
    }

    fn list_dir_cached(
        &mut self,
        path: &Path,
    ) -> Result<Vec<remotefs::fs::File>, remotefs::RemoteError> {
        if let Some(entry) = self.dir_cache.get(path) {
            if entry.at.elapsed() < DIR_CACHE_TTL {
                return Ok(entry.files.clone());
            }
        }
        let files = self.fs.list_dir(path)?;
        self.dir_cache.insert(
            path.to_path_buf(),
            DirCacheEntry {
                files: files.clone(),
                at: Instant::now(),
            },
        );
        Ok(files)
    }

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
            kind: if is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
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
}

impl Filesystem for NextCloudFs {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let parent_path = match self.get_path(parent).cloned() {
            Some(p) => p,
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        let name_str = match name.to_str() {
            Some(s) => s.to_string(),
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        log::debug!("[{}] LOOKUP {}/{}", self.log_user, parent_path.display(), name_str);

        match self.list_dir_cached(&parent_path) {
            Ok(entries) => {
                for entry in &entries {
                    let entry_name = entry
                        .path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("");
                    if entry_name == name_str {
                        let target_path = parent_path.join(&name_str);
                        let inode = self.allocate_inode(target_path);
                        let attr = Self::make_file_attr(inode, &entry.metadata);
                        reply.entry(&TTL, &attr, 0);
                        return;
                    }
                }
                reply.error(ENOENT);
            }
            Err(e) => {
                log::error!("list_dir {}: {}", parent_path.display(), e);
                reply.error(EIO);
            }
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        if ino == 1 {
            reply.attr(&TTL, &Self::root_attr());
            return;
        }

        let path = match self.get_path(ino).cloned() {
            Some(p) => p,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        log::debug!("[{}] GETATTR {}", self.log_user, path.display());

        let parent = path.parent().unwrap_or(Path::new("/")).to_path_buf();
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        match self.list_dir_cached(&parent) {
            Ok(entries) => {
                for entry in &entries {
                    let entry_name = entry
                        .path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("");
                    if entry_name == file_name {
                        let attr = Self::make_file_attr(ino, &entry.metadata);
                        reply.attr(&TTL, &attr);
                        return;
                    }
                }
                reply.error(ENOENT);
            }
            Err(e) => {
                log::error!("list_dir {} (for getattr): {}", parent.display(), e);
                reply.error(EIO);
            }
        }
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
        let path = match self.get_path(ino).cloned() {
            Some(p) => p,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        log::debug!(
            "[{}] READ {} offset={} size={}",
            self.log_user,
            path.display(),
            offset,
            size
        );

        let local = match self.ensure_file_cached(&path) {
            Ok(p) => p,
            Err(e) => {
                log::error!("cache {}: {}", path.display(), e);
                reply.error(EIO);
                return;
            }
        };

        match std::fs::read(&local) {
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
                log::error!("read cache file {}: {}", local.display(), e);
                reply.error(EIO);
            }
        }
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let path = match self.get_path(ino).cloned() {
            Some(p) => p,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        log::debug!("[{}] READDIR {}", self.log_user, path.display());

        // Emit . and .. before the requested offset
        if offset == 0 {
            if reply.add(ino, 1, FileType::Directory, ".") {
                reply.ok();
                return;
            }
            let parent_ino = if ino == 1 {
                1
            } else {
                let parent = path.parent().unwrap_or(Path::new("/"));
                self.get_inode(parent).unwrap_or(1)
            };
            if reply.add(parent_ino, 2, FileType::Directory, "..") {
                reply.ok();
                return;
            }
        }

        let entries = match self.list_dir_cached(&path) {
            Ok(e) => e,
            Err(e) => {
                log::error!("list_dir {}: {}", path.display(), e);
                reply.error(EIO);
                return;
            }
        };

        let skip = if offset > 2 { (offset - 2) as usize } else { 0 };

        for (i, entry) in entries.iter().enumerate().skip(skip) {
            let name = match entry.path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            let entry_path = path.join(&name);
            let entry_ino = self.allocate_inode(entry_path);
            let is_dir = entry.metadata.file_type == RemoteFileType::Directory;
            let kind = if is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            // offset is 1-based: . = 1, .. = 2, entries start at 3
            if reply.add(entry_ino, (i + 3) as i64, kind, &name) {
                break;
            }
        }

        reply.ok();
    }
}

pub fn mount_ncfs(options: MountOptions) -> Result<(), String> {
    let filesystem = NextCloudFs::new(options.clone())?;

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

/// Converts a URL into a safe directory name for use in the cache path.
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
    let mount_point = PathBuf::from(
        doc["mount_point"]
            .as_str()
            .unwrap_or("/media/ncrs_mount"),
    );
    let log_user = doc["user"]
        .as_str()
        .unwrap_or("default_user")
        .to_string();

    Ok(MountOptions {
        url,
        username,
        password,
        mount_point,
        log_user,
    })
}
