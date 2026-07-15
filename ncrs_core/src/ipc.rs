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
///   CHANGES\n                        → tab-separated changed paths
///   FILE_CHANGES\n                    → tab-separated A:/path or D:/path entries
///   STORAGE\n                         → JSON {kept_bytes, cached_bytes, remote_used, remote_total}
///   STATE\n                           → paused|syncing|idle (daemon-wide sync state)
///   PAUSE\n / RESUME\n                → ok (suspend/resume background sync)
///   THUMBNAIL <abs-path>\n           → ok | error: <msg>  (fetch NC preview → XDG thumb cache)
///   VERSION <n>\n                     → <daemon-protocol>\t<pkg-version>  (n = extension protocol)
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use crate::{MutexExt, RwLockExt};

const MAX_IPC_CLIENTS: usize = 64;
const CLIENT_READ_TIMEOUT: Duration = Duration::from_secs(60);

/// IPC protocol version. Bump whenever the daemon⇄extension contract changes
/// (a new command, a changed reply format). The Nautilus extension announces
/// its own copy of this on connect via `VERSION`; a mismatch is logged so a
/// half-updated install (new daemon + old extension, or vice-versa) is obvious.
/// Keep in sync with `PROTOCOL_VERSION` in shell_integration/nautilus/syncstate.py.
pub const PROTOCOL_VERSION: u32 = 2;

const QUERY_ENCODE: &AsciiSet = &CONTROLS
    .add(b' ').add(b'#').add(b'%').add(b'&').add(b'+').add(b'=').add(b'?');

pub type KeepCallback = Arc<dyn Fn(PathBuf) + Send + Sync>;
pub type EvictCallback = Arc<dyn Fn(PathBuf) + Send + Sync>;
pub type PrefetchCallback = Arc<dyn Fn(PathBuf) + Send + Sync>;
/// Synchronously fetch the Nextcloud preview for a remote path and write it to
/// the XDG thumbnail cache. Returns `true` if the thumbnail is now available.
pub type ThumbnailCallback = Arc<dyn Fn(PathBuf) -> bool + Send + Sync>;
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

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct StorageStats {
    pub kept_bytes: u64,
    pub cached_bytes: u64,
    pub remote_used: u64,
    pub remote_total: u64,
}

pub type SharedStorageStats = Arc<Mutex<StorageStats>>;

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
    Kept,
    Cached,
    Synced,
    Remote,
    Downloading,
    Uploading,
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
    let dir_children = match children.get(remote_dir) {
        Some(c) => c,
        None => return Vec::new(),
    };

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

/// Start the IPC socket server in a background thread.
///
/// `mount_point` is the local FUSE mount directory; paths outside it return Unknown.
pub fn start_server(mount_point: PathBuf, status_map: StatusMap, shared_set: SharedSet, fileid_map: FileIdMap, detail_map: FileDetailMap, children_map: ChildrenMap, dirty_set: DirtySet, creds: crate::auth::Credentials, base_url: String, keep_cb: Option<KeepCallback>, evict_cb: Option<EvictCallback>, prefetch_cb: Option<PrefetchCallback>, thumbnail_cb: Option<ThumbnailCallback>, error_log: crate::ErrorLog, transfer_map: crate::TransferMap, journal: crate::mutation_journal::SharedJournal, file_change_queue: FileChangeQueue, storage_stats: SharedStorageStats, paused: Arc<AtomicBool>) {
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

    let active = Arc::new(AtomicUsize::new(0));

    std::thread::spawn(move || {
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
            let elog = error_log.clone();
            let tmap = transfer_map.clone();
            let jrnl = journal.clone();
            let fcq = file_change_queue.clone();
            let sstats = storage_stats.clone();
            let pause_flag = paused.clone();
            let active = active.clone();
            active.fetch_add(1, Ordering::Relaxed);
            std::thread::spawn(move || {
                handle_client(stream, mount, map, shared, fids, details, children, dirty, creds_clone, burl, cb, ev, pf, th, elog, tmap, jrnl, fcq, sstats, pause_flag);
                active.fetch_sub(1, Ordering::Relaxed);
            });
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
    children_map: ChildrenMap,
    dirty_set: DirtySet,
    creds: crate::auth::Credentials,
    base_url: String,
    keep_cb: Option<KeepCallback>,
    evict_cb: Option<EvictCallback>,
    prefetch_cb: Option<PrefetchCallback>,
    thumbnail_cb: Option<ThumbnailCallback>,
    error_log: crate::ErrorLog,
    transfer_map: crate::TransferMap,
    journal: crate::mutation_journal::SharedJournal,
    file_change_queue: FileChangeQueue,
    storage_stats: SharedStorageStats,
    paused: Arc<AtomicBool>,
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
            const MAX_CHANGES: usize = 500;
            let mut set = dirty_set.safe_lock();
            let total = set.len();
            let paths: Vec<PathBuf> = if total <= MAX_CHANGES {
                set.drain().collect()
            } else {
                let batch: Vec<PathBuf> = set.iter().take(MAX_CHANGES).cloned().collect();
                for p in &batch {
                    set.remove(p);
                }
                batch
            };
            drop(set);
            if !paths.is_empty() {
                log::info!("IPC CHANGES → {} dirty paths (of {} total)", paths.len(), total);
            }
            if paths.is_empty() {
                String::new()
            } else {
                paths.iter()
                    .map(|p| {
                        let rel = p.strip_prefix("/").unwrap_or(p);
                        mount_point.join(rel).to_string_lossy().into_owned()
                    })
                    .collect::<Vec<_>>()
                    .join("\t")
            }
        } else if trimmed == "FILE_CHANGES" {
            let changes: Vec<FileChange> = {
                let mut q = file_change_queue.safe_lock();
                q.drain(..).collect()
            };
            if changes.is_empty() {
                String::new()
            } else {
                changes.iter()
                    .map(|c| {
                        let rel = c.path.strip_prefix("/").unwrap_or(&c.path);
                        let abs = mount_point.join(rel);
                        match &c.kind {
                            FileChangeKind::Added => format!("A:{}", abs.display()),
                            FileChangeKind::Removed => format!("D:{}", abs.display()),
                            FileChangeKind::Modified => format!("M:{}", abs.display()),
                            FileChangeKind::DirAdded => format!("DA:{}", abs.display()),
                            FileChangeKind::DirRemoved => format!("DD:{}", abs.display()),
                            FileChangeKind::Renamed { from } => {
                                let from_rel = from.strip_prefix("/").unwrap_or(from);
                                format!("R:{}\x1e{}", mount_point.join(from_rel).display(), abs.display())
                            }
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\t")
            }
        } else if let Some(path_str) = trimmed.strip_prefix("KEEP ") {
            match (strip_mount(Path::new(path_str), &mount_point), &keep_cb) {
                (Some(remote), Some(cb)) => {
                    status_map.safe_write().insert(remote.clone(), FileStatus::Downloading);
                    dirty_set.safe_lock().insert(remote.clone());
                    let cb = cb.clone();
                    let sm = status_map.clone();
                    let ds = dirty_set.clone();
                    let r = remote.clone();
                    std::thread::spawn(move || {
                        if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(remote))) {
                            log::error!("KEEP callback panicked: {:?}", e);
                        }
                        if sm.safe_read().get(&r).copied() == Some(FileStatus::Downloading) {
                            sm.safe_write().insert(r.clone(), FileStatus::Kept);
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
                    std::thread::spawn(move || {
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
            let transfers: Vec<crate::TransferProgress> = transfer_map.safe_lock().values().cloned().collect();
            serde_json::to_string(&transfers).unwrap_or_else(|_| "[]".to_string())
        } else if trimmed == "JOURNAL" {
            let j = journal.safe_lock();
            let entries: Vec<&crate::mutation_journal::JournalEntry> = j.entries().iter().collect();
            serde_json::to_string(&entries).unwrap_or_else(|_| "[]".to_string())
        } else if trimmed == "CONFLICTS" {
            let j = journal.safe_lock();
            let conflicts = j.unresolved_conflicts();
            serde_json::to_string(&conflicts).unwrap_or_else(|_| "[]".to_string())
        } else if trimmed == "STORAGE" {
            let stats = storage_stats.safe_lock().clone();
            serde_json::to_string(&stats).unwrap_or_else(|_| "{}".to_string())
        } else if trimmed == "STATE" {
            // Same derivation the GUI uses: pause wins, then transfer activity.
            if paused.load(Ordering::Relaxed) {
                "paused".to_string()
            } else if !transfer_map.safe_lock().is_empty() {
                "syncing".to_string()
            } else {
                "idle".to_string()
            }
        } else if trimmed == "PAUSE" {
            paused.store(true, Ordering::Relaxed);
            log::info!("sync paused via IPC");
            "ok".to_string()
        } else if trimmed == "RESUME" {
            paused.store(false, Ordering::Relaxed);
            log::info!("sync resumed via IPC");
            "ok".to_string()
        } else if let Some(ver_str) = trimmed.strip_prefix("VERSION ") {
            // The extension announces its protocol version on connect. Print it
            // and warn if it does not match the daemon so a half-updated install
            // is visible in the logs. Reply with our own protocol + package
            // version so the extension can warn on its side too.
            match ver_str.trim().parse::<u32>() {
                Ok(v) if v == PROTOCOL_VERSION => {
                    log::info!("Nautilus extension connected (protocol v{})", v);
                }
                Ok(v) => {
                    log::warn!(
                        "Nautilus extension protocol v{} does not match daemon protocol v{} — \
                         update ncRS so the daemon and Nautilus extension are the same release",
                        v, PROTOCOL_VERSION
                    );
                }
                Err(_) => {
                    log::warn!("Nautilus extension sent malformed VERSION: {:?}", ver_str.trim());
                }
            }
            format!("{}\t{}", PROTOCOL_VERSION, env!("CARGO_PKG_VERSION"))
        } else if let Some(msg) = trimmed.strip_prefix("LOG ") {
            log::info!("[nautilus] {}", msg);
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
