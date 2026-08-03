use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use std::ffi::OsStr;

use crate::backend::{ChangeEvent, CloudBackend};
use crate::fuse_notify;
use crate::ipc::{DirtySet, FileChange, FileChangeKind, FileChangeQueue};
use crate::{GhostEntry, GhostKind, GhostMap, MutexExt, Throttle};
use fuser::INodeNo;

const CAPABILITIES_TIMEOUT: Duration = Duration::from_secs(10);
const PROPFIND_TIMEOUT: Duration = Duration::from_secs(15);
pub(crate) const REFRESH_DEBOUNCE: Duration = Duration::from_secs(3);
pub(crate) const REFRESH_DEBOUNCE_NO_CHANGE: Duration = Duration::from_secs(30);

pub(crate) fn debounce_cooldown(had_changes: bool) -> Duration {
    if had_changes { REFRESH_DEBOUNCE } else { REFRESH_DEBOUNCE_NO_CHANGE }
}

pub(crate) struct DebounceState {
    pub last_refresh: Instant,
    pub had_changes: bool,
}

pub(crate) type DebounceMap = Arc<Mutex<HashMap<PathBuf, DebounceState>>>;

// -- WebSocket discovery (used by NextcloudBackend) ---------------------------

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
    pre_auth: Option<String>,
}

pub(crate) struct NotifyPushInfo {
    pub ws_url: String,
    pub pre_auth_url: Option<String>,
}

pub(crate) fn discover_endpoints(
    client: &reqwest::blocking::Client,
    base_url: &str,
    creds: &crate::auth::Credentials,
) -> Result<NotifyPushInfo, String> {
    let url = format!("{}/ocs/v2.php/cloud/capabilities?format=json", base_url);
    let resp = creds.apply(client
        .get(&url)
        .timeout(CAPABILITIES_TIMEOUT))
        .header("OCS-APIREQUEST", "true")
        .send()
        .map_err(|e| format!("capabilities request failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("capabilities API returned {}", resp.status()));
    }

    let caps: OcsCapabilities = resp.json().map_err(|e| format!("capabilities parse error: {}", e))?;
    let np = caps.ocs
        .data
        .capabilities
        .notify_push
        .ok_or_else(|| "notify_push capability not found (app not installed?)".to_string())?;
    Ok(NotifyPushInfo {
        ws_url: np.endpoints.websocket,
        pre_auth_url: np.endpoints.pre_auth,
    })
}

const PRE_AUTH_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn fetch_pre_auth_ticket(
    client: &reqwest::blocking::Client,
    pre_auth_url: &str,
    creds: &crate::auth::Credentials,
) -> Result<String, String> {
    let resp = creds.apply(client.post(pre_auth_url).timeout(PRE_AUTH_TIMEOUT))
        .send()
        .map_err(|e| format!("pre_auth request failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("pre_auth returned {}", resp.status()));
    }

    let ticket = resp.text()
        .map_err(|e| format!("pre_auth body read: {}", e))?
        .trim()
        .to_string();
    if ticket.is_empty() {
        return Err("pre_auth returned empty ticket".into());
    }
    Ok(ticket)
}

// -- Cache invalidation utilities ---------------------------------------------

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
            let _ = notifier.inval_inode(INodeNo(ino), 0, 0);
        }
    }
}

// -- Dir diff -----------------------------------------------------------------

pub(crate) struct OldDirSnapshot {
    pub names: Vec<PathBuf>,
    pub etags: HashMap<PathBuf, Option<String>>,
    pub fileids: HashMap<PathBuf, u64>,
    pub is_dir: HashMap<PathBuf, bool>,
}

impl OldDirSnapshot {
    pub(crate) fn of(files: &[crate::backend::RemoteEntry]) -> Self {
        OldDirSnapshot {
            names: files.iter().map(|f| f.path.clone()).collect(),
            etags: files.iter()
                .map(|f| (f.path.clone(), f.change_token.clone()))
                .collect(),
            fileids: files.iter()
                .filter_map(|f| f.ext.int("fileid").map(|fid| (f.path.clone(), fid)))
                .collect(),
            is_dir: files.iter()
                .map(|f| (f.path.clone(), f.is_dir))
                .collect(),
        }
    }
}

pub(crate) struct DirDiff {
    pub removed: Vec<PathBuf>,
    pub added: Vec<PathBuf>,
    pub modified: Vec<PathBuf>,
    pub renames: Vec<(PathBuf, PathBuf, bool)>,
}

pub(crate) fn compute_dir_diff(old_snap: &OldDirSnapshot, fresh_files: &[crate::backend::RemoteEntry]) -> DirDiff {
    use std::collections::HashSet;
    let new_names: HashSet<&PathBuf> = fresh_files.iter().map(|f| &f.path).collect();
    let old_set: HashSet<&PathBuf> = old_snap.names.iter().collect();

    let raw_removed: Vec<PathBuf> = old_set.difference(&new_names).map(|p| (*p).clone()).collect();
    let raw_added: Vec<PathBuf> = new_names.difference(&old_set).map(|p| (*p).clone()).collect();

    let modified: Vec<PathBuf> = fresh_files.iter().filter_map(|f| {
        if f.is_dir { return None; }
        let old_etag = old_snap.etags.get(&f.path)?;
        if old_etag.as_deref() != f.change_token.as_deref() {
            Some(f.path.clone())
        } else {
            None
        }
    }).collect();

    let new_fids: HashMap<u64, &PathBuf> = fresh_files.iter()
        .filter_map(|f| f.ext.int("fileid").map(|fid| (fid, &f.path)))
        .collect();
    let added_set: HashSet<&PathBuf> = raw_added.iter().collect();
    let mut renames: Vec<(PathBuf, PathBuf, bool)> = Vec::new();
    let mut removed: Vec<PathBuf> = Vec::new();

    for p in &raw_removed {
        if let Some(&old_fid) = old_snap.fileids.get(p) {
            if let Some(&new_path) = new_fids.get(&old_fid) {
                if added_set.contains(new_path) {
                    let is_dir = old_snap.is_dir.get(p).copied().unwrap_or(false);
                    renames.push((p.clone(), new_path.clone(), is_dir));
                    continue;
                }
            }
        }
        removed.push(p.clone());
    }

    let rename_targets: std::collections::HashSet<PathBuf> = renames.iter().map(|(_, to, _)| to.clone()).collect();
    let added: Vec<PathBuf> = raw_added.into_iter().filter(|p| !rename_targets.contains(p)).collect();

    DirDiff { removed, added, modified, renames }
}

// -- Change event handler (FUSE-layer entry point) ----------------------------

pub(crate) fn handle_change_event(
    event: ChangeEvent,
    backend: &Arc<dyn CloudBackend>,
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
    if active_streams.load(Ordering::Relaxed) > 0 {
        deferred_invalidation.store(true, Ordering::Relaxed);
        log::debug!("change_event: deferred (streaming active)");
        return;
    }

    if event.invalidate_all {
        log::info!("change_event: invalidating all dirs");
        invalidate_all_dirs(cache, dirty, notifier_slot);
        return;
    }

    if event.invalidated_dirs.is_empty() {
        return;
    }

    let dirs_to_refresh = invalidate_dirs_by_path(
        &event.invalidated_dirs, cache, dirty, notifier_slot, debounce,
    );

    if !dirs_to_refresh.is_empty() {
        proactive_refresh(
            dirs_to_refresh, backend, cache, dirty, throttle,
            notifier_slot, debounce, ghost_entries, file_change_queue,
        );
    }
}

// -- Invalidate specific dirs by path -----------------------------------------

fn invalidate_dirs_by_path(
    dirs: &[PathBuf],
    cache: &Arc<Mutex<crate::FsCache>>,
    dirty: &DirtySet,
    notifier_slot: &fuse_notify::NotifierSlot,
    debounce: &DebounceMap,
) -> Vec<(PathBuf, OldDirSnapshot)> {
    let now = Instant::now();
    let recently_refreshed: std::collections::HashSet<PathBuf> = {
        let db = debounce.safe_lock();
        db.iter()
            .filter(|(_, state)| {
                let cooldown = debounce_cooldown(state.had_changes);
                now.duration_since(state.last_refresh) < cooldown
            })
            .map(|(p, _)| p.clone())
            .collect()
    };

    let mut c = cache.safe_lock();
    let mut invalidated: Vec<(PathBuf, OldDirSnapshot)> = Vec::new();
    let mut freshly_fetched: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut debounced = 0usize;

    for dir_path in dirs {
        let entry = match c.dir_cache.get_mut(dir_path) {
            Some(e) => e,
            None => {
                log::debug!("change_event: {} not in dir_cache, skipping", dir_path.display());
                continue;
            }
        };

        if recently_refreshed.contains(dir_path) {
            debounced += 1;
            log::debug!("change_event: skipping re-invalidation of {} (refreshed recently)", dir_path.display());
            continue;
        }

        let snapshot = OldDirSnapshot::of(&entry.files);

        let just_fetched = entry.at.elapsed() < Duration::from_secs(1);
        if just_fetched {
            log::debug!("change_event: suppressing kernel inval for {} (fetched {}ms ago)", dir_path.display(), entry.at.elapsed().as_millis());
            freshly_fetched.insert(dir_path.clone());
        } else {
            entry.invalidated = true;
            entry.refreshing = false;
        }
        invalidated.push((dir_path.clone(), snapshot));
    }

    let invalidated_inodes: Vec<u64> = invalidated.iter()
        .filter(|(p, _)| !freshly_fetched.contains(p))
        .filter_map(|(p, _)| c.get_inode(p))
        .collect();
    drop(c);

    {
        let mut ds = dirty.safe_lock();
        for (p, _) in &invalidated {
            ds.insert(p.clone());
        }
    }

    if let Some(notifier) = notifier_slot.safe_lock().as_ref() {
        for ino in &invalidated_inodes {
            let _ = notifier.inval_inode(INodeNo(*ino), 0, 0);
        }
    }

    if !invalidated.is_empty() || debounced > 0 {
        log::info!("change_event: {} dirs invalidated, {} debounced, {} self-notify suppressed",
            invalidated.len() - freshly_fetched.len(), debounced, freshly_fetched.len());
    }

    invalidated
}

// -- Read-triggered revalidation ------------------------------------------------

/// Kicks off a background etag revalidation of `dir_path` after its cached
/// listing was served to a reader. This makes the dir cache a latency
/// optimization rather than a source of truth: readers get the cached answer
/// immediately, and if the cheap Depth-0 etag probe shows the directory
/// changed on the server, the full refresh pipeline (diff, ghost entries,
/// kernel invalidation, IPC change queue) brings the listing up to date.
///
/// Covers changes for which no notify-push event was ever received — events
/// emitted while this client was offline are not replayed by the server.
///
/// Rate limiting relies on the shared debounce map (3s after a refresh that
/// found changes, 30s after one that found none), so at most one probe per
/// directory per cooldown window regardless of readdir frequency.
pub(crate) fn revalidate_dir_on_read(
    dir_path: &Path,
    backend: &Arc<dyn CloudBackend>,
    cache: &Arc<Mutex<crate::FsCache>>,
    dirty: &DirtySet,
    active_streams: &AtomicUsize,
    throttle: &Arc<Throttle>,
    notifier_slot: &fuse_notify::NotifierSlot,
    debounce: &DebounceMap,
    ghost_entries: &GhostMap,
    file_change_queue: &FileChangeQueue,
) {
    // Same guard as handle_change_event: don't perturb active streaming reads.
    // The next readdir after the stream ends revalidates.
    if active_streams.load(Ordering::Relaxed) > 0 {
        return;
    }

    {
        let db = debounce.safe_lock();
        if let Some(state) = db.get(dir_path) {
            if state.last_refresh.elapsed() < debounce_cooldown(state.had_changes) {
                return;
            }
        }
    }

    let old_snap = {
        let c = cache.safe_lock();
        let entry = match c.dir_cache.get(dir_path) {
            Some(e) => e,
            None => return,
        };
        // A listing that was just PROPFINDed (cache miss, TTL refresh, or a
        // notify-push refresh) is already fresh — probing again is pure churn.
        if entry.refreshing || entry.at.elapsed() < Duration::from_secs(2) {
            return;
        }
        OldDirSnapshot::of(&entry.files)
    };

    let dir_path = dir_path.to_path_buf();
    let backend = Arc::clone(backend);
    let cache = Arc::clone(cache);
    let dirty = Arc::clone(dirty);
    let throttle = Arc::clone(throttle);
    let notifier_slot = Arc::clone(notifier_slot);
    let debounce = Arc::clone(debounce);
    let ghosts = Arc::clone(ghost_entries);
    let fcq = Arc::clone(file_change_queue);
    std::thread::spawn(move || {
        refresh_one_dir(dir_path, old_snap, backend, cache, dirty, throttle,
                        notifier_slot, debounce, ghosts, fcq);
    });
}

// -- Proactive refresh --------------------------------------------------------

fn refresh_one_dir(
    dir_path: PathBuf,
    old_snap: OldDirSnapshot,
    backend: Arc<dyn CloudBackend>,
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
                log::debug!("proactive_refresh: skipping {} (debounce, had_changes={})", dir_path.display(), state.had_changes);
                return;
            }
        }
    }

    let cached_etag = cache.safe_lock().cached_dir_etag(&dir_path);
    if cached_etag.is_some() {
        match backend.dir_change_token(&dir_path, Duration::from_secs(15)) {
            Ok(current_etag) if current_etag == cached_etag => {
                log::debug!("proactive_refresh {}: change_token unchanged {:?}, skipping", dir_path.display(), cached_etag);
                cache.safe_lock().touch_dir_cache(&dir_path);
                debounce.safe_lock().insert(dir_path, DebounceState {
                    last_refresh: Instant::now(),
                    had_changes: false,
                });
                return;
            }
            Ok(ref current_etag) => {
                log::debug!("proactive_refresh {}: change_token changed {:?} → {:?}, proceeding", dir_path.display(), cached_etag, current_etag);
            }
            Err(e) => {
                log::debug!("proactive_refresh {}: change_token check failed ({}), proceeding", dir_path.display(), e);
            }
        }
    }

    let _permit = throttle.acquire();
    let result = backend.list_dir(&dir_path, PROPFIND_TIMEOUT);

    match result {
        Ok((etag, self_entry, fresh_files)) => {
            let diff = compute_dir_diff(&old_snap, &fresh_files);
            static RENAME_PAIR_COUNTER: AtomicU64 = AtomicU64::new(1);

            if !diff.removed.is_empty() {
                log::info!("proactive_refresh: {} removed from {}: {:?}", diff.removed.len(), dir_path.display(), diff.removed);
            }
            if !diff.added.is_empty() {
                log::info!("proactive_refresh: {} added to {}: {:?}", diff.added.len(), dir_path.display(), diff.added);
            }
            if !diff.modified.is_empty() {
                log::info!("proactive_refresh: {} file(s) modified in {}: {:?}", diff.modified.len(), dir_path.display(), diff.modified);
            }

            let listing_changed = !diff.added.is_empty() || !diff.removed.is_empty() || !diff.renames.is_empty();
            let had_changes = listing_changed || !diff.modified.is_empty();

            let mut c = cache.safe_lock();
            let parent_ino = c.get_inode(&dir_path).unwrap_or(1);

            let mut file_cache_changed = false;
            for (old_path, new_path, is_dir) in &diff.renames {
                if !is_dir {
                    if let Some(entry) = c.file_cache.remove(old_path) {
                        c.file_cache.insert(new_path.clone(), entry);
                        file_cache_changed = true;
                        log::info!("file_cache: moved {} → {}", old_path.display(), new_path.display());
                    }
                }
            }

            // A file whose content changed on the server (new etag → flagged
            // `modified`) makes our locally cached copy stale. Evict it — both the
            // map entry and the on-disk file — so the next read re-downloads via
            // ensure_file_cached instead of the read fast-path serving the old bytes
            // at the NEW (dir-cache) size. That size/content mismatch is what makes a
            // ZIP-based format (odt/xlsx/…) opened right after a server-side edit look
            // corrupt. Renames are handled above; only genuine content changes land here.
            for p in &diff.modified {
                if let Some(entry) = c.file_cache.remove(p) {
                    match std::fs::remove_file(&entry.local_path) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => log::warn!("file_cache: failed to remove stale {}: {}", entry.local_path.display(), e),
                    }
                    file_cache_changed = true;
                    log::info!("file_cache: evicted stale {} (modified on server)", p.display());
                }
            }

            {
                let old_entries = c.get_cached_dir_readonly(&dir_path);
                let mut ghosts = ghost_entries.safe_lock();

                for p in &diff.removed {
                    if let Some(old_entry) = old_entries.as_ref()
                        .and_then(|entries| entries.iter().find(|e| &e.path == p))
                    {
                        let ino = c.get_inode(p).unwrap_or(1);
                        let attr = crate::make_file_attr(ino, old_entry);
                        ghosts.insert(p.clone(), GhostEntry {
                            kind: GhostKind::VisibleDelete { attr },
                            created_at: Instant::now(),
                            rename_pair_id: None,
                        });
                        log::info!("ghost: VisibleDelete for {} (ino={})", p.display(), ino);
                    }
                }

                for (old_path, new_path, _) in &diff.renames {
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

            let delete_targets: Vec<(u64, String)> = diff.removed.iter().filter_map(|p| {
                let child_ino = c.get_inode(p).unwrap_or(0);
                p.file_name().map(|n| (child_ino, n.to_string_lossy().into_owned()))
            }).collect();
            let modified_inodes: Vec<u64> = diff.modified.iter()
                .filter_map(|p| c.get_inode(p))
                .collect();

            let added_is_dir: HashMap<PathBuf, bool> = fresh_files.iter()
                .filter(|f| diff.added.contains(&f.path))
                .map(|f| (f.path.clone(), f.is_dir))
                .collect();

            c.put_dir_cache(dir_path.clone(), etag, self_entry, fresh_files);

            {
                let mut ghosts = ghost_entries.safe_lock();
                for p in &diff.added {
                    ghosts.insert(p.clone(), GhostEntry {
                        kind: GhostKind::HiddenAdd,
                        created_at: Instant::now(),
                        rename_pair_id: None,
                    });
                    log::info!("ghost: HiddenAdd for {}", p.display());
                }
            }

            if file_cache_changed {
                crate::save_file_cache(&cache);
            }

            drop(c);

            if !diff.added.is_empty() || !diff.removed.is_empty() || !diff.modified.is_empty() || !diff.renames.is_empty() {
                let mut q = file_change_queue.safe_lock();
                for p in &diff.added {
                    let is_dir = added_is_dir.get(p).copied().unwrap_or(false);
                    let kind = if is_dir { FileChangeKind::DirAdded } else { FileChangeKind::Added };
                    q.push(FileChange { kind, path: p.clone() });
                }
                for p in &diff.removed {
                    let is_dir = old_snap.is_dir.get(p).copied().unwrap_or(false);
                    let kind = if is_dir { FileChangeKind::DirRemoved } else { FileChangeKind::Removed };
                    q.push(FileChange { kind, path: p.clone() });
                }
                for p in &diff.modified {
                    q.push(FileChange { kind: FileChangeKind::Modified, path: p.clone() });
                }
                for (old_path, new_path, _) in &diff.renames {
                    q.push(FileChange {
                        kind: FileChangeKind::Renamed { from: old_path.clone() },
                        path: new_path.clone(),
                    });
                }
            }

            if let Some(notifier) = notifier_slot.safe_lock().as_ref() {
                if listing_changed {
                    let _ = notifier.inval_inode(INodeNo(parent_ino), 0, 0);
                }
                for (child_ino, name) in &delete_targets {
                    let _ = notifier.delete(INodeNo(parent_ino), INodeNo(*child_ino), OsStr::new(name));
                }
                for ino in &modified_inodes {
                    let _ = notifier.inval_inode(INodeNo(*ino), 0, 0);
                }
            }

            dirty.safe_lock().insert(dir_path.clone());
            debounce.safe_lock().insert(dir_path, DebounceState {
                last_refresh: Instant::now(),
                had_changes,
            });
        }
        Err(e) => {
            log::warn!("proactive_refresh: {} failed: {}", dir_path.display(), e);
        }
    }
}

fn proactive_refresh(
    dirs: Vec<(PathBuf, OldDirSnapshot)>,
    backend: &Arc<dyn CloudBackend>,
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
        let backend = Arc::clone(backend);
        let cache = Arc::clone(cache);
        let dirty = Arc::clone(dirty);
        let throttle = Arc::clone(throttle);
        let notifier_slot = Arc::clone(notifier_slot);
        let debounce = Arc::clone(debounce);
        let ghosts = Arc::clone(ghost_entries);
        let fcq = Arc::clone(file_change_queue);
        std::thread::spawn(move || {
            refresh_one_dir(dir_path, old_snap, backend, cache, dirty, throttle,
                            notifier_slot, debounce, ghosts, fcq);
        })
    }).collect();
    for h in handles { let _ = h.join(); }
    log::info!("proactive_refresh: done {} dirs in {:?}", n, t_pr.elapsed());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{EntryExtensions, RemoteEntry};

    fn make_entry(path: &str, is_dir: bool, etag: Option<&str>, fileid: Option<u64>) -> RemoteEntry {
        let mut ext = EntryExtensions::default();
        if let Some(fid) = fileid {
            ext.integers.insert("fileid".into(), fid);
        }
        RemoteEntry {
            path: PathBuf::from(path),
            is_dir,
            size: 0,
            modified: None,
            change_token: etag.map(String::from),
            content_type: None,
            ext,
        }
    }

    #[test]
    fn diff_add_remove() {
        let old = OldDirSnapshot {
            names: vec![PathBuf::from("/a.txt"), PathBuf::from("/b.txt")],
            etags: [
                (PathBuf::from("/a.txt"), Some("e1".into())),
                (PathBuf::from("/b.txt"), Some("e2".into())),
            ].into_iter().collect(),
            fileids: HashMap::new(),
            is_dir: [
                (PathBuf::from("/a.txt"), false),
                (PathBuf::from("/b.txt"), false),
            ].into_iter().collect(),
        };
        let fresh = vec![
            make_entry("/a.txt", false, Some("e1"), None),
            make_entry("/c.txt", false, Some("e3"), None),
        ];
        let diff = compute_dir_diff(&old, &fresh);
        assert_eq!(diff.removed, vec![PathBuf::from("/b.txt")]);
        assert_eq!(diff.added, vec![PathBuf::from("/c.txt")]);
        assert!(diff.modified.is_empty());
        assert!(diff.renames.is_empty());
    }

    #[test]
    fn diff_modified() {
        let old = OldDirSnapshot {
            names: vec![PathBuf::from("/a.txt")],
            etags: [(PathBuf::from("/a.txt"), Some("e1".into()))].into_iter().collect(),
            fileids: HashMap::new(),
            is_dir: [(PathBuf::from("/a.txt"), false)].into_iter().collect(),
        };
        let fresh = vec![make_entry("/a.txt", false, Some("e2"), None)];
        let diff = compute_dir_diff(&old, &fresh);
        assert!(diff.removed.is_empty());
        assert!(diff.added.is_empty());
        assert_eq!(diff.modified, vec![PathBuf::from("/a.txt")]);
    }

    #[test]
    fn diff_rename_by_fileid() {
        let old = OldDirSnapshot {
            names: vec![PathBuf::from("/old.txt")],
            etags: [(PathBuf::from("/old.txt"), Some("e1".into()))].into_iter().collect(),
            fileids: [(PathBuf::from("/old.txt"), 42)].into_iter().collect(),
            is_dir: [(PathBuf::from("/old.txt"), false)].into_iter().collect(),
        };
        let fresh = vec![make_entry("/new.txt", false, Some("e1"), Some(42))];
        let diff = compute_dir_diff(&old, &fresh);
        assert!(diff.removed.is_empty());
        assert!(diff.added.is_empty());
        assert_eq!(diff.renames.len(), 1);
        assert_eq!(diff.renames[0].0, PathBuf::from("/old.txt"));
        assert_eq!(diff.renames[0].1, PathBuf::from("/new.txt"));
    }
}
