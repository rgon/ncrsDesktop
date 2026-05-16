use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tungstenite::{connect, Message};

use crate::fuse_notify;
use crate::ipc::{DirtySet, FileChange, FileChangeKind, FileChangeQueue};
use crate::{propfind, GhostEntry, GhostKind, GhostMap, MutexExt, Throttle};

const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);
const CAPABILITIES_TIMEOUT: Duration = Duration::from_secs(10);
const PROPFIND_TIMEOUT: Duration = Duration::from_secs(15);
pub(crate) const REFRESH_DEBOUNCE: Duration = Duration::from_secs(3);
pub(crate) const REFRESH_DEBOUNCE_NO_CHANGE: Duration = Duration::from_secs(30);

pub(crate) struct DebounceState {
    pub last_refresh: Instant,
    pub had_changes: bool,
}

type DebounceMap = Arc<Mutex<HashMap<PathBuf, DebounceState>>>;

#[derive(serde::Deserialize)]
struct OcsCapabilities {
    ocs: OcsCapBody,
}

#[derive(serde::Deserialize)]
struct OcsCapBody {
    data: OcsCapData,
}

#[derive(serde::Deserialize)]
struct OcsCapData {
    capabilities: Capabilities,
}

#[derive(serde::Deserialize)]
struct Capabilities {
    notify_push: Option<NotifyPushCap>,
}

#[derive(serde::Deserialize)]
struct NotifyPushCap {
    endpoints: NotifyPushEndpoints,
}

#[derive(serde::Deserialize)]
struct NotifyPushEndpoints {
    websocket: String,
}

fn discover_ws_url(
    client: &reqwest::blocking::Client,
    base_url: &str,
    username: &str,
    password: &str,
) -> Result<String, String> {
    let url = format!("{}/ocs/v2.php/cloud/capabilities?format=json", base_url);
    let resp = client
        .get(&url)
        .timeout(CAPABILITIES_TIMEOUT)
        .basic_auth(username, Some(password))
        .header("OCS-APIREQUEST", "true")
        .send()
        .map_err(|e| format!("capabilities request failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("capabilities API returned {}", resp.status()));
    }

    let caps: OcsCapabilities = resp.json().map_err(|e| format!("capabilities parse error: {}", e))?;
    caps.ocs
        .data
        .capabilities
        .notify_push
        .map(|np| np.endpoints.websocket)
        .ok_or_else(|| "notify_push capability not found (app not installed?)".into())
}

pub(crate) fn invalidate_all_dirs(
    cache: &Mutex<crate::FsCache>,
    dirty: &DirtySet,
    notifier_slot: &fuse_notify::NotifierSlot,
) {
    let mut c = cache.safe_lock();
    let mut invalidated_paths = Vec::new();
    for (path, entry) in c.dir_cache.iter_mut() {
        entry.invalidated = true;
        entry.refreshing = false;
        invalidated_paths.push(path.clone());
    }
    let inodes: Vec<u64> = invalidated_paths.iter()
        .filter_map(|p| c.get_inode(p))
        .collect();
    drop(c);

    {
        let mut ds = dirty.safe_lock();
        for p in &invalidated_paths {
            ds.insert(p.clone());
        }
    }

    if let Some(notifier) = notifier_slot.safe_lock().as_ref() {
        for ino in inodes {
            if let Err(e) = notifier.notify_inval_inode(ino, 0, 0) {
                log::debug!("notify_inval_inode({}) failed: {}", ino, e);
            }
        }
    }
}

pub(crate) struct OldDirSnapshot {
    pub names: Vec<PathBuf>,
    pub etags: HashMap<PathBuf, Option<String>>,
    pub fileids: HashMap<PathBuf, u64>,
    pub is_dir: HashMap<PathBuf, bool>,
}

struct InvalidateResult {
    dirs: Vec<(PathBuf, OldDirSnapshot)>,
    ids_recognized: bool,
    active_listings: usize,
}

fn invalidate_dirs_for_fileids(
    cache: &Arc<Mutex<crate::FsCache>>,
    ids: &[u64],
    dirty: &DirtySet,
    notifier_slot: &fuse_notify::NotifierSlot,
    debounce: &DebounceMap,
) -> InvalidateResult {
    let now = Instant::now();
    let recently_refreshed: std::collections::HashSet<PathBuf> = {
        let db = debounce.safe_lock();
        db.iter()
            .filter(|(_, state)| {
                let cooldown = if state.had_changes { REFRESH_DEBOUNCE } else { REFRESH_DEBOUNCE_NO_CHANGE };
                now.duration_since(state.last_refresh) < cooldown
            })
            .map(|(p, _)| p.clone())
            .collect()
    };

    let mut c = cache.safe_lock();
    let id_set: std::collections::HashSet<u64> = ids.iter().copied().collect();

    let dir_self_fids: std::collections::HashSet<u64> = c.dir_cache.values()
        .filter_map(|entry| entry.self_entry.as_ref().and_then(|se| se.fileid))
        .filter(|fid| id_set.contains(fid))
        .collect();

    let mut invalidated: Vec<(PathBuf, OldDirSnapshot)> = Vec::new();
    let mut id_resolutions: Vec<String> = Vec::new();
    let mut matched_dirs = 0usize;
    let mut debounced_dirs = 0usize;

    for (dir_path, entry) in c.dir_cache.iter_mut() {
        let self_hit = entry.self_entry.as_ref().and_then(|se| se.fileid)
            .filter(|fid| id_set.contains(fid));
        let child_hits: Vec<(&PathBuf, u64)> = entry.files.iter()
            .filter_map(|f| {
                let fid = f.fileid.filter(|fid| id_set.contains(fid))?;
                if f.is_dir && dir_self_fids.contains(&fid) {
                    return None;
                }
                Some((&f.path, fid))
            })
            .collect();

        if self_hit.is_none() && child_hits.is_empty() {
            continue;
        }

        matched_dirs += 1;

        if let Some(fid) = self_hit {
            id_resolutions.push(format!("{} → dir {}", fid, dir_path.display()));
        }
        for (path, fid) in &child_hits {
            id_resolutions.push(format!("{} → {}", fid, path.display()));
        }

        if recently_refreshed.contains(dir_path) {
            debounced_dirs += 1;
            log::debug!("notify_push: skipping re-invalidation of {} (refreshed <{}s ago)", dir_path.display(), REFRESH_DEBOUNCE.as_secs());
            continue;
        }

        let names: Vec<PathBuf> = entry.files.iter().map(|f| f.path.clone()).collect();
        let etags: HashMap<PathBuf, Option<String>> = entry.files.iter()
            .map(|f| (f.path.clone(), f.etag.clone()))
            .collect();
        let fileids: HashMap<PathBuf, u64> = entry.files.iter()
            .filter_map(|f| f.fileid.map(|fid| (f.path.clone(), fid)))
            .collect();
        let is_dir: HashMap<PathBuf, bool> = entry.files.iter()
            .map(|f| (f.path.clone(), f.is_dir))
            .collect();
        entry.invalidated = true;
        entry.refreshing = false;
        invalidated.push((dir_path.clone(), OldDirSnapshot { names, etags, fileids, is_dir }));
    }
    let invalidated_inodes: Vec<u64> = invalidated.iter()
        .filter_map(|(p, _)| c.get_inode(p))
        .collect();
    let active_listings = c.pending_dirs.len();
    drop(c);

    {
        let mut ds = dirty.safe_lock();
        for (p, _) in &invalidated {
            ds.insert(p.clone());
        }
    }

    if let Some(notifier) = notifier_slot.safe_lock().as_ref() {
        for ino in &invalidated_inodes {
            if let Err(e) = notifier.notify_inval_inode(*ino, 0, 0) {
                log::debug!("notify_inval_inode({}) failed: {}", ino, e);
            }
        }
    }

    if matched_dirs > 0 {
        log::info!("notify_push: file_id {:?} → [{}] → {} dirs invalidated, {} debounced",
            ids, id_resolutions.join(", "), invalidated.len(), debounced_dirs);
    } else {
        log::info!("notify_push: file_id {:?} → no cached dirs matched", ids);
    }

    InvalidateResult { dirs: invalidated, ids_recognized: matched_dirs > 0, active_listings }
}

fn refresh_one_dir(
    dir_path: PathBuf,
    old_snap: OldDirSnapshot,
    client: reqwest::blocking::Client,
    webdav_url: String,
    username: String,
    password: String,
    cache: Arc<Mutex<crate::FsCache>>,
    dirty: DirtySet,
    throttle: Arc<Throttle>,
    notifier_slot: fuse_notify::NotifierSlot,
    debounce: DebounceMap,
    ghost_entries: GhostMap,
    file_change_queue: FileChangeQueue,
) {
    let now = Instant::now();
    {
        let db = debounce.safe_lock();
        if let Some(state) = db.get(&dir_path) {
            let cooldown = if state.had_changes { REFRESH_DEBOUNCE } else { REFRESH_DEBOUNCE_NO_CHANGE };
            if now.duration_since(state.last_refresh) < cooldown {
                log::debug!("notify_push: skipping refresh for {} (debounce, had_changes={})", dir_path.display(), state.had_changes);
                return;
            }
        }
    }

    let _permit = throttle.acquire();
    let result = propfind::propfind_list(
        &client, &webdav_url, &username, &password, &dir_path, PROPFIND_TIMEOUT,
    );

        match result {
            Ok((etag, self_entry, fresh_files)) => {
                let new_names: std::collections::HashSet<&PathBuf> =
                    fresh_files.iter().map(|f| &f.path).collect();
                let old_set: std::collections::HashSet<&PathBuf> =
                    old_snap.names.iter().collect();

                let raw_removed: Vec<&PathBuf> = old_set.difference(&new_names).copied().collect();
                let raw_added: Vec<&PathBuf> = new_names.difference(&old_set).copied().collect();

                let modified: Vec<&Path> = fresh_files.iter().filter_map(|f| {
                    if f.is_dir { return None; }
                    let old_etag = old_snap.etags.get(&f.path)?;
                    if old_etag.as_deref() != f.etag.as_deref() {
                        Some(f.path.as_path())
                    } else {
                        None
                    }
                }).collect();

                // Detect renames by matching fileids between removed and added
                static RENAME_PAIR_COUNTER: AtomicU64 = AtomicU64::new(1);
                let new_fids: HashMap<u64, &PathBuf> = fresh_files.iter()
                    .filter_map(|f| f.fileid.map(|fid| (fid, &f.path)))
                    .collect();
                let added_set: std::collections::HashSet<&PathBuf> = raw_added.iter().copied().collect();
                let mut renames: Vec<(PathBuf, PathBuf, bool)> = Vec::new();
                let mut removed: Vec<&PathBuf> = Vec::new();

                for p in &raw_removed {
                    if let Some(&old_fid) = old_snap.fileids.get(*p) {
                        if let Some(&new_path) = new_fids.get(&old_fid) {
                            if added_set.contains(new_path) {
                                let is_dir = old_snap.is_dir.get(*p).copied().unwrap_or(false);
                                renames.push(((*p).clone(), new_path.clone(), is_dir));
                                log::info!("notify_push: rename detected {} → {} (fileid={})", p.display(), new_path.display(), old_fid);
                                continue;
                            }
                        }
                    }
                    removed.push(p);
                }

                let rename_targets: std::collections::HashSet<PathBuf> = renames.iter().map(|(_, to, _)| to.clone()).collect();
                let added: Vec<&PathBuf> = raw_added.into_iter()
                    .filter(|p| !rename_targets.contains(*p))
                    .collect();

                if !removed.is_empty() {
                    log::info!("notify_push: {} removed from {}: {:?}", removed.len(), dir_path.display(), removed);
                }
                if !added.is_empty() {
                    log::info!("notify_push: {} added to {}: {:?}", added.len(), dir_path.display(), added);
                }
                if !modified.is_empty() {
                    log::info!("notify_push: {} file(s) modified in {}: {:?}", modified.len(), dir_path.display(), modified);
                }

                let listing_changed = !added.is_empty() || !removed.is_empty() || !renames.is_empty();
                let had_changes = listing_changed || !modified.is_empty();

                let mut c = cache.safe_lock();
                let parent_ino = c.get_inode(&dir_path).unwrap_or(1);

                // Move file_cache entries for renames before put_dir_cache
                let mut cache_moved = false;
                for (old_path, new_path, is_dir) in &renames {
                    if !is_dir {
                        if let Some(entry) = c.file_cache.remove(old_path) {
                            c.file_cache.insert(new_path.clone(), entry);
                            cache_moved = true;
                            log::info!("file_cache: moved {} → {}", old_path.display(), new_path.display());
                        }
                    }
                }

                // Create VisibleDelete ghosts BEFORE put_dir_cache (need old attrs)
                {
                    let old_entries = c.get_cached_dir_readonly(&dir_path);
                    let mut ghosts = ghost_entries.safe_lock();

                    // Ghosts for true removals
                    for p in &removed {
                        if let Some(old_entry) = old_entries.as_ref()
                            .and_then(|entries| entries.iter().find(|e| &e.path == *p))
                        {
                            let ino = c.get_inode(p).unwrap_or(1);
                            let attr = crate::make_file_attr(ino, old_entry);
                            ghosts.insert((*p).clone(), GhostEntry {
                                kind: GhostKind::VisibleDelete { attr },
                                created_at: Instant::now(),
                                rename_pair_id: None,
                            });
                            log::info!("ghost: VisibleDelete for {} (ino={})", p.display(), ino);
                        }
                    }

                    // Paired ghosts for renames
                    for (old_path, new_path, _) in &renames {
                        let pair_id = RENAME_PAIR_COUNTER.fetch_add(1, Ordering::Relaxed);
                        if let Some(old_entry) = old_entries.as_ref()
                            .and_then(|entries| entries.iter().find(|e| &e.path == old_path))
                        {
                            let ino = c.get_inode(old_path).unwrap_or(1);
                            let attr = crate::make_file_attr(ino, old_entry);
                            ghosts.insert(old_path.clone(), GhostEntry {
                                kind: GhostKind::VisibleDelete { attr },
                                created_at: Instant::now(),
                                rename_pair_id: Some(pair_id),
                            });
                            ghosts.insert(new_path.clone(), GhostEntry {
                                kind: GhostKind::HiddenAdd,
                                created_at: Instant::now(),
                                rename_pair_id: Some(pair_id),
                            });
                            log::info!("ghost: rename pair {} ↔ {} (pair_id={})", old_path.display(), new_path.display(), pair_id);
                        }
                    }
                }

                let delete_targets: Vec<(u64, String)> = removed.iter().filter_map(|p| {
                    let child_ino = c.get_inode(p).unwrap_or(0);
                    p.file_name().map(|n| (child_ino, n.to_string_lossy().into_owned()))
                }).collect();
                let modified_inodes: Vec<u64> = modified.iter()
                    .filter_map(|p| c.get_inode(p))
                    .collect();

                // Collect owned paths before put_dir_cache moves fresh_files
                let added_owned: Vec<PathBuf> = added.iter().map(|p| (*p).clone()).collect();
                let removed_owned: Vec<PathBuf> = removed.iter().map(|p| (*p).clone()).collect();
                let modified_owned: Vec<PathBuf> = modified.iter().map(|p| p.to_path_buf()).collect();

                // Build is_dir map for added entries before fresh_files is moved
                let added_is_dir: HashMap<PathBuf, bool> = fresh_files.iter()
                    .filter(|f| added_owned.contains(&f.path))
                    .map(|f| (f.path.clone(), f.is_dir))
                    .collect();

                c.put_dir_cache(dir_path.clone(), etag, self_entry, fresh_files);

                // Create HiddenAdd ghosts AFTER put_dir_cache (file is now in cache)
                {
                    let mut ghosts = ghost_entries.safe_lock();
                    for p in &added_owned {
                        ghosts.insert(p.clone(), GhostEntry {
                            kind: GhostKind::HiddenAdd,
                            created_at: Instant::now(),
                            rename_pair_id: None,
                        });
                        log::info!("ghost: HiddenAdd for {}", p.display());
                    }
                }

                if cache_moved {
                    crate::save_file_cache(&cache);
                }

                drop(c);

                // Populate file change queue for Nautilus extension
                if !added_owned.is_empty() || !removed_owned.is_empty() || !modified_owned.is_empty() || !renames.is_empty() {
                    let mut q = file_change_queue.safe_lock();
                    for p in &added_owned {
                        let is_dir = added_is_dir.get(p).copied().unwrap_or(false);
                        let kind = if is_dir { FileChangeKind::DirAdded } else { FileChangeKind::Added };
                        q.push(FileChange { kind, path: p.clone() });
                    }
                    for p in &removed_owned {
                        let is_dir = old_snap.is_dir.get(p).copied().unwrap_or(false);
                        let kind = if is_dir { FileChangeKind::DirRemoved } else { FileChangeKind::Removed };
                        q.push(FileChange { kind, path: p.clone() });
                    }
                    for p in &modified_owned {
                        q.push(FileChange { kind: FileChangeKind::Modified, path: p.clone() });
                    }
                    for (old_path, new_path, _) in &renames {
                        q.push(FileChange {
                            kind: FileChangeKind::Renamed { from: old_path.clone() },
                            path: new_path.clone(),
                        });
                    }
                }

                if let Some(notifier) = notifier_slot.safe_lock().as_ref() {
                    if listing_changed {
                        if let Err(e) = notifier.notify_inval_inode(parent_ino, 0, 0) {
                            log::warn!("notify_inval_inode(parent={}) failed: {}", parent_ino, e);
                        }
                    }
                    for (child_ino, name) in &delete_targets {
                        if let Err(e) = notifier.notify_delete(parent_ino, *child_ino, name.as_bytes()) {
                            log::debug!("notify_delete({}/{}) failed (dentry likely expired): {}", parent_ino, name, e);
                        }
                    }
                    for ino in &modified_inodes {
                        if let Err(e) = notifier.notify_inval_inode(*ino, 0, 0) {
                            log::debug!("notify_inval_inode({}) for modified file failed: {}", ino, e);
                        }
                    }
                }

                dirty.safe_lock().insert(dir_path.clone());
                debounce.safe_lock().insert(dir_path, DebounceState {
                    last_refresh: Instant::now(),
                    had_changes,
                });
            }
            Err(e) => {
                log::warn!("notify_push: proactive refresh {} failed: {}", dir_path.display(), e);
            }
        }
}

fn proactive_refresh(
    dirs: Vec<(PathBuf, OldDirSnapshot)>,
    client: &reqwest::blocking::Client,
    webdav_url: &str,
    username: &str,
    password: &str,
    cache: &Arc<Mutex<crate::FsCache>>,
    dirty: &DirtySet,
    throttle: &Arc<Throttle>,
    notifier_slot: &fuse_notify::NotifierSlot,
    debounce: &DebounceMap,
    ghost_entries: &GhostMap,
    file_change_queue: &FileChangeQueue,
) {
    let n = dirs.len();
    log::info!("proactive_refresh: starting {} dirs in parallel", n);
    let t_pr = Instant::now();
    let handles: Vec<_> = dirs.into_iter().map(|(dir_path, old_snap)| {
        let client = client.clone();
        let webdav_url = webdav_url.to_owned();
        let username = username.to_owned();
        let password = password.to_owned();
        let cache = Arc::clone(cache);
        let dirty = Arc::clone(dirty);
        let throttle = Arc::clone(throttle);
        let notifier_slot = Arc::clone(notifier_slot);
        let debounce = Arc::clone(debounce);
        let ghosts = Arc::clone(ghost_entries);
        let fcq = Arc::clone(file_change_queue);
        std::thread::spawn(move || {
            refresh_one_dir(dir_path, old_snap, client, webdav_url, username, password,
                            cache, dirty, throttle, notifier_slot, debounce, ghosts, fcq);
        })
    }).collect();
    for h in handles { let _ = h.join(); }
    log::info!("proactive_refresh: done {} dirs in {:?}", n, t_pr.elapsed());
}

fn resolve_and_invalidate(
    ids: &[u64],
    client: &reqwest::blocking::Client,
    webdav_url: &str,
    username: &str,
    password: &str,
    cache: &Arc<Mutex<crate::FsCache>>,
    dirty: &DirtySet,
    notifier_slot: &fuse_notify::NotifierSlot,
) {
    match propfind::resolve_fileids(client, webdav_url, username, password, ids, PROPFIND_TIMEOUT) {
        Ok(paths) => {
            let parent_dirs: std::collections::HashSet<PathBuf> = paths.iter()
                .filter_map(|p| p.parent().map(Path::to_path_buf))
                .collect();

            if parent_dirs.is_empty() {
                log::info!("notify_push: SEARCH returned no results for {:?}", ids);
                return;
            }

            let mut c = cache.safe_lock();
            let mut inodes_to_invalidate = Vec::new();
            for dir in &parent_dirs {
                if let Some(entry) = c.dir_cache.get_mut(dir) {
                    entry.invalidated = true;
                    entry.refreshing = false;
                    if let Some(ino) = c.get_inode(dir) {
                        inodes_to_invalidate.push(ino);
                    }
                    log::info!("notify_push: resolved file_id → invalidated dir {}", dir.display());
                } else {
                    log::debug!("notify_push: resolved parent {} not in dir_cache, skipping", dir.display());
                }
            }
            drop(c);

            {
                let mut ds = dirty.safe_lock();
                for dir in &parent_dirs {
                    ds.insert(dir.clone());
                }
            }

            if let Some(notifier) = notifier_slot.safe_lock().as_ref() {
                for ino in &inodes_to_invalidate {
                    if let Err(e) = notifier.notify_inval_inode(*ino, 0, 0) {
                        log::debug!("notify_inval_inode({}) failed: {}", ino, e);
                    }
                }
            }
        }
        Err(e) => {
            log::warn!("notify_push: SEARCH resolve failed: {} — falling back to invalidate_all", e);
            invalidate_all_dirs(cache, dirty, notifier_slot);
        }
    }
}

pub(crate) fn start(
    client: reqwest::blocking::Client,
    base_url: String,
    webdav_url: String,
    username: String,
    password: String,
    cache: Arc<Mutex<crate::FsCache>>,
    dirty: DirtySet,
    is_offline: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    active_streams: Arc<AtomicUsize>,
    deferred_invalidation: Arc<AtomicBool>,
    throttle: Arc<Throttle>,
    notifier_slot: fuse_notify::NotifierSlot,
    ghost_entries: GhostMap,
    file_change_queue: FileChangeQueue,
) {
    std::thread::spawn(move || {
        run_loop(&client, &base_url, &webdav_url, &username, &password, &cache, &dirty, &is_offline, &connected, &active_streams, &deferred_invalidation, &throttle, &notifier_slot, &ghost_entries, &file_change_queue);
    });
}

fn run_loop(
    client: &reqwest::blocking::Client,
    base_url: &str,
    webdav_url: &str,
    username: &str,
    password: &str,
    cache: &Arc<Mutex<crate::FsCache>>,
    dirty: &DirtySet,
    is_offline: &Arc<AtomicBool>,
    connected: &AtomicBool,
    active_streams: &Arc<AtomicUsize>,
    deferred_invalidation: &Arc<AtomicBool>,
    throttle: &Arc<Throttle>,
    notifier_slot: &fuse_notify::NotifierSlot,
    ghost_entries: &GhostMap,
    file_change_queue: &FileChangeQueue,
) {
    let debounce: DebounceMap = Arc::new(Mutex::new(HashMap::new()));
    let mut reconnect_delay = Duration::from_secs(1);

    loop {
        connected.store(false, Ordering::Relaxed);

        if is_offline.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }

        let ws_url = match discover_ws_url(client, base_url, username, password) {
            Ok(url) => {
                log::info!("notify_push: discovered endpoint {}", url);
                url
            }
            Err(e) => {
                log::warn!("notify_push: discovery failed: {}", e);
                log::info!("notify_push: retrying in {:?}", reconnect_delay);
                std::thread::sleep(reconnect_delay);
                reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
                continue;
            }
        };

        match connect_and_listen(&ws_url, client, webdav_url, username, password, cache, dirty, connected, active_streams, deferred_invalidation, throttle, notifier_slot, &debounce, ghost_entries, file_change_queue) {
            Ok(()) => {
                log::info!("notify_push: connection closed cleanly");
                reconnect_delay = Duration::from_secs(1);
            }
            Err(e) => {
                log::warn!("notify_push: {}", e);
            }
        }

        log::info!("notify_push: reconnecting in {:?}", reconnect_delay);
        std::thread::sleep(reconnect_delay);
        reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
    }
}

fn connect_and_listen(
    ws_url: &str,
    client: &reqwest::blocking::Client,
    webdav_url: &str,
    username: &str,
    password: &str,
    cache: &Arc<Mutex<crate::FsCache>>,
    dirty: &DirtySet,
    connected: &AtomicBool,
    active_streams: &Arc<AtomicUsize>,
    deferred_invalidation: &AtomicBool,
    throttle: &Arc<Throttle>,
    notifier_slot: &fuse_notify::NotifierSlot,
    debounce: &DebounceMap,
    ghost_entries: &GhostMap,
    file_change_queue: &FileChangeQueue,
) -> Result<(), String> {
    let (mut socket, _response) = connect(ws_url).map_err(|e| format!("WebSocket connect: {}", e))?;

    socket
        .send(Message::Text(username.into()))
        .map_err(|e| format!("send username: {}", e))?;
    socket
        .send(Message::Text(password.into()))
        .map_err(|e| format!("send password: {}", e))?;

    let auth_msg = socket
        .read()
        .map_err(|e| format!("read auth response: {}", e))?;

    match &auth_msg {
        Message::Text(t) if *t == "authenticated" => {
            log::info!("notify_push: authenticated successfully");
            connected.store(true, Ordering::Relaxed);
        }
        Message::Text(t) if t.starts_with("err:") => {
            return Err(format!("auth failed: {}", t));
        }
        other => {
            return Err(format!("unexpected auth response: {:?}", other));
        }
    }

    socket
        .send(Message::Text("listen notify_file_id".into()))
        .map_err(|e| format!("send listen: {}", e))?;
    log::info!("notify_push: subscribed to notify_file_id");

    loop {
        let msg = socket.read().map_err(|e| format!("read: {}", e))?;
        match msg {
            Message::Text(ref t) => {
                handle_event(t.as_ref(), client, webdav_url, username, password, cache, dirty, active_streams, deferred_invalidation, throttle, notifier_slot, debounce, ghost_entries, file_change_queue);
            }
            Message::Close(_) => {
                log::info!("notify_push: server closed connection");
                return Ok(());
            }
            Message::Ping(data) => {
                let _ = socket.send(Message::Pong(data));
            }
            _ => {}
        }
    }
}

fn handle_event(
    event: &str,
    client: &reqwest::blocking::Client,
    webdav_url: &str,
    username: &str,
    password: &str,
    cache: &Arc<Mutex<crate::FsCache>>,
    dirty: &DirtySet,
    active_streams: &AtomicUsize,
    deferred_invalidation: &AtomicBool,
    throttle: &Arc<Throttle>,
    notifier_slot: &fuse_notify::NotifierSlot,
    debounce: &DebounceMap,
    ghost_entries: &GhostMap,
    file_change_queue: &FileChangeQueue,
) {
    let trimmed = event.trim();
    if let Some(ids_json) = trimmed.strip_prefix("notify_file_id ") {
        log::info!("notify_push: ← {}", trimmed);
        match serde_json::from_str::<Vec<u64>>(ids_json) {
            Ok(ids) => {
                if active_streams.load(Ordering::Relaxed) > 0 {
                    deferred_invalidation.store(true, Ordering::Relaxed);
                    log::debug!("notify_push: file_id {:?} deferred (streaming active)", ids);
                    return;
                }
                let result = invalidate_dirs_for_fileids(cache, &ids, dirty, notifier_slot, debounce);
                if !result.ids_recognized {
                    log::info!("notify_push: file_id {:?} not in any cached dir — resolving via SEARCH", ids);
                    let client = client.clone();
                    let webdav_url = webdav_url.to_owned();
                    let username = username.to_owned();
                    let password = password.to_owned();
                    let cache = Arc::clone(cache);
                    let dirty = Arc::clone(dirty);
                    let notifier_slot = Arc::clone(notifier_slot);
                    std::thread::spawn(move || {
                        resolve_and_invalidate(
                            &ids, &client, &webdav_url, &username, &password,
                            &cache, &dirty, &notifier_slot,
                        );
                    });
                } else if !result.dirs.is_empty() {
                    if result.active_listings > 0 {
                        log::debug!("notify_push: skipping proactive_refresh ({} active dir fetches — traversal in progress)", result.active_listings);
                    } else {
                        let client = client.clone();
                        let webdav_url = webdav_url.to_owned();
                        let username = username.to_owned();
                        let password = password.to_owned();
                        let cache = Arc::clone(cache);
                        let dirty = Arc::clone(dirty);
                        let throttle = Arc::clone(throttle);
                        let notifier_slot = Arc::clone(notifier_slot);
                        let debounce = Arc::clone(debounce);
                        let ghosts = Arc::clone(ghost_entries);
                        let fcq = Arc::clone(file_change_queue);
                        std::thread::spawn(move || {
                            proactive_refresh(
                                result.dirs, &client, &webdav_url, &username, &password,
                                &cache, &dirty, &throttle, &notifier_slot, &debounce,
                                &ghosts, &fcq,
                            );
                        });
                    }
                }
            }
            Err(e) => {
                log::warn!("notify_push: failed to parse file IDs '{}': {}", ids_json, e);
                invalidate_all_dirs(cache, dirty, notifier_slot);
            }
        }
        return;
    }
    match trimmed {
        "notify_file" => {
            if active_streams.load(Ordering::Relaxed) > 0 {
                deferred_invalidation.store(true, Ordering::Relaxed);
                log::debug!("notify_push: file change event deferred (streaming active)");
                return;
            }
            log::info!("notify_push: file change event — invalidating all dirs");
            invalidate_all_dirs(cache, dirty, notifier_slot);
        }
        "notify_notification" => {
            log::info!("notify_push: notification event");
            std::thread::spawn(|| {
                let _ = std::process::Command::new("notify-send")
                    .args(["--app-name=ncrs", "--icon=nextcloud", "Nextcloud", "You have a new notification"])
                    .output();
            });
        }
        "notify_activity" => {
            log::debug!("notify_push: activity event");
        }
        other => {
            log::debug!("notify_push: unknown event: {}", other);
        }
    }
}
