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
    Unknown,
}

impl FileStatus {
    fn as_str(self) -> &'static str {
        match self {
            FileStatus::Local => "local",
            FileStatus::Synced => "synced",
            FileStatus::Remote => "remote",
            FileStatus::Unknown => "unknown",
        }
    }
}

/// Thread-safe store of path → status, updated by the FUSE layer.
pub type StatusMap = Arc<Mutex<std::collections::HashMap<PathBuf, FileStatus>>>;

/// Start the IPC socket server in a background thread.
///
/// `mount_point` is the local FUSE mount directory; paths outside it return Unknown.
pub fn start_server(mount_point: PathBuf, status_map: StatusMap) {
    let sock = socket_path();
    let _ = std::fs::remove_file(&sock); // clean up stale socket

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
            std::thread::spawn(move || handle_client(stream, mount, map));
        }
    });
}

fn handle_client(
    stream: std::os::unix::net::UnixStream,
    mount_point: PathBuf,
    status_map: StatusMap,
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
        let status = if let Some(path_str) = trimmed.strip_prefix("STATUS ") {
            let path = Path::new(path_str);
            if path.starts_with(&mount_point) {
                // Strip the mount point to get the remote path.
                let remote = path
                    .strip_prefix(&mount_point)
                    .map(|p| Path::new("/").join(p))
                    .unwrap_or_else(|_| PathBuf::from("/"));
                status_map
                    .lock()
                    .unwrap()
                    .get(&remote)
                    .copied()
                    .unwrap_or(FileStatus::Remote)
            } else {
                FileStatus::Unknown
            }
        } else {
            FileStatus::Unknown
        };

        if writeln!(write_half, "{}", status.as_str()).is_err() {
            break;
        }
    }
}
