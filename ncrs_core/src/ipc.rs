/// Unix domain socket IPC server.
///
/// Clients (e.g. the Nautilus extension) connect and send line-oriented queries:
///
///   STATUS <absolute-local-path>\n
///
/// The server responds with one of:
///   local\n    — file exists in the disk cache and is up to date
///   synced\n   — file is in cache but the dir listing TTL has expired (status uncertain)
///   remote\n   — file is known but has no local copy yet
///   unknown\n  — path is not under the mount point or not seen yet
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

trait MutexExt<T> {
    fn safe_lock(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for Mutex<T> {
    fn safe_lock(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub type KeepCallback = Arc<dyn Fn(PathBuf) + Send + Sync>;
pub type SharedSet = Arc<Mutex<std::collections::HashSet<PathBuf>>>;
pub type FileIdMap = Arc<Mutex<std::collections::HashMap<PathBuf, u64>>>;

#[derive(Clone, Default)]
pub struct FileDetail {
    pub permissions: Option<String>,
    pub owner_id: Option<String>,
    pub owner_display_name: Option<String>,
    pub size: u64,
}

pub type FileDetailMap = Arc<Mutex<std::collections::HashMap<PathBuf, FileDetail>>>;

pub fn socket_path() -> PathBuf {
    std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::runtime_dir().unwrap_or_else(|| PathBuf::from("/tmp"))
        })
        .join("ncrs.sock")
}

/// File-level sync status as seen by the daemon.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Local,
    Synced,
    Remote,
    Downloading,
    Unknown,
}

impl FileStatus {
    fn as_str(self) -> &'static str {
        match self {
            FileStatus::Local => "local",
            FileStatus::Synced => "synced",
            FileStatus::Remote => "remote",
            FileStatus::Downloading => "downloading",
            FileStatus::Unknown => "unknown",
        }
    }
}

/// Thread-safe store of path → status, updated by the FUSE layer.
pub type StatusMap = Arc<Mutex<std::collections::HashMap<PathBuf, FileStatus>>>;

/// Start the IPC socket server in a background thread.
///
/// `mount_point` is the local FUSE mount directory; paths outside it return Unknown.
pub fn start_server(mount_point: PathBuf, status_map: StatusMap, shared_set: SharedSet, fileid_map: FileIdMap, detail_map: FileDetailMap, username: String, base_url: String, keep_cb: Option<KeepCallback>) {
    let sock = socket_path();
    let _ = std::fs::remove_file(&sock);

    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            log::error!("Cannot bind IPC socket {}: {}", sock.display(), e);
            return;
        }
    };
    log::info!("IPC socket listening at {}", sock.display());

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    log::error!("IPC accept error: {}", e);
                    continue;
                }
            };
            let mount = mount_point.clone();
            let map = status_map.clone();
            let shared = shared_set.clone();
            let fids = fileid_map.clone();
            let details = detail_map.clone();
            let uname = username.clone();
            let burl = base_url.clone();
            let cb = keep_cb.clone();
            std::thread::spawn(move || handle_client(stream, mount, map, shared, fids, details, uname, burl, cb));
        }
    });
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
    username: String,
    base_url: String,
    keep_cb: Option<KeepCallback>,
) {
    let mut write_half = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let reader = BufReader::new(stream);

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();

        let reply = if let Some(path_str) = trimmed.strip_prefix("STATUS ") {
            match strip_mount(Path::new(path_str), &mount_point) {
                Some(remote) => {
                    let status = status_map
                        .safe_lock()
                        .get(&remote)
                        .copied()
                        .unwrap_or(FileStatus::Remote)
                        .as_str();
                    let shared = shared_set.safe_lock().contains(&remote);
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
                    let status = status_map.safe_lock()
                        .get(&remote).copied().unwrap_or(FileStatus::Remote).as_str();
                    let is_shared = shared_set.safe_lock().contains(&remote);
                    let detail = detail_map.safe_lock().get(&remote).cloned()
                        .unwrap_or_default();
                    let sharing = if !is_shared {
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
                    format!("{}\t{}\t{}\t{}\t{}", status, sharing, perms, owner, detail.size)
                }
                None => "unknown\t\t\t\t0".to_string(),
            }
        } else if let Some(path_str) = trimmed.strip_prefix("WEBURL ") {
            match strip_mount(Path::new(path_str), &mount_point) {
                Some(remote) => {
                    let parent = remote.parent().unwrap_or(Path::new("/"));
                    let dir = parent.to_string_lossy();
                    match fileid_map.safe_lock().get(&remote) {
                        Some(fid) => format!("{}/apps/files/?dir={}&fileid={}", base_url, dir, fid),
                        None => format!("{}/apps/files/?dir={}", base_url, dir),
                    }
                }
                None => "error: path not under mount".to_string(),
            }
        } else if let Some(path_str) = trimmed.strip_prefix("KEEP ") {
            match (strip_mount(Path::new(path_str), &mount_point), &keep_cb) {
                (Some(remote), Some(cb)) => {
                    let cb = cb.clone();
                    std::thread::spawn(move || {
                        if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(remote))) {
                            log::error!("KEEP callback panicked: {:?}", e);
                        }
                    });
                    "ok".to_string()
                }
                (None, _) => "error: path not under mount".to_string(),
                (_, None) => "error: not supported".to_string(),
            }
        } else {
            "unknown".to_string()
        };

        if writeln!(write_half, "{}", reply).is_err() {
            break;
        }
    }
}
