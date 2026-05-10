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
use std::time::{Duration, Instant, UNIX_EPOCH};
use yaml_rust2::YamlLoader;

const TTL: Duration = Duration::from_secs(1);
const DIR_CACHE_TTL: Duration = Duration::from_secs(10);

struct DirCacheEntry {
    files: Vec<remotefs::fs::File>,
    at: Instant,
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
}

impl NextCloudFs {
    pub fn new(options: MountOptions) -> Result<Self, String> {
        let username = options.username.unwrap_or_default();
        let password = options.password.unwrap_or_default();

        let mut webdav_fs = WebDAVFs::new(&username, &password, &options.url);
        webdav_fs
            .connect()
            .map_err(|e| format!("WebDAV connect failed: {}", e))?;

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

        match self.fs.open(&path) {
            Ok(mut stream) => {
                let mut data = Vec::new();
                if let Err(e) = stream.read_to_end(&mut data) {
                    log::error!("read {}: {}", path.display(), e);
                    reply.error(EIO);
                    return;
                }
                let off = offset as usize;
                if off >= data.len() {
                    reply.data(&[]);
                } else {
                    let end = std::cmp::min(off + size as usize, data.len());
                    reply.data(&data[off..end]);
                }
            }
            Err(e) => {
                log::error!("open {}: {}", path.display(), e);
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
