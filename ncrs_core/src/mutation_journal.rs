use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const JOURNAL_FILE: &str = "mutation_journal.json";
const CONFLICTS_FILE: &str = "conflicts.json";
const MAX_ATTEMPTS: u32 = 3;

/// How long an entry the server rejected `attempts` times waits before its
/// next try: 1 min, then 5, then 25 (the last before it is given up), so a
/// permission or quota an admin fixes meanwhile still lets it through.
fn retry_backoff(attempts: u32) -> Duration {
    Duration::from_secs(60 * 5u64.saturating_pow(attempts.saturating_sub(1)).min(60))
}

pub type SeqId = u64;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum MutationOp {
    Put {
        remote_path: PathBuf,
        staging_path: PathBuf,
        if_match_etag: Option<String>,
    },
    MkDir {
        path: PathBuf,
    },
    Unlink {
        path: PathBuf,
    },
    RmDir {
        path: PathBuf,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
    },
    /// The end of a streamed upload whose earlier chunks are already on the server:
    /// PUT `tail_path` as chunk `next_index`, then assemble the session into `remote_path`.
    FinishChunked {
        remote_path: PathBuf,
        uploads_base: String,
        next_index: u64,
        bytes_confirmed: u64,
        total_len: u64,
        tail_path: PathBuf,
        if_match_etag: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JournalEntry {
    pub seq: SeqId,
    pub op: MutationOp,
    pub created_at_ms: u64,
    pub attempts: u32,
    pub last_error: Option<String>,
    /// Not retried before this time (ms since the epoch). A server rejection
    /// backs off (`retry_backoff`) instead of the replay spending every
    /// attempt on the same entry within one pass.
    #[serde(default)]
    pub not_before_ms: u64,
    /// Claimed by a live worker or the replay loop; never persisted, so a restart
    /// makes every entry replayable again.
    #[serde(skip)]
    pub in_flight: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ConflictKind {
    EditConflict {
        local_path: PathBuf,
        conflicted_copy_path: PathBuf,
    },
    MoveSourceGone {
        from: PathBuf,
        to: PathBuf,
    },
    MoveDestExists {
        from: PathBuf,
        to: PathBuf,
    },
    PermanentFailure {
        description: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConflictRecord {
    pub id: u64,
    pub kind: ConflictKind,
    pub timestamp_ms: u64,
    pub resolved: bool,
}

pub struct MutationJournal {
    entries: VecDeque<JournalEntry>,
    next_seq: SeqId,
    conflicts: Vec<ConflictRecord>,
    next_conflict_id: u64,
    journal_path: PathBuf,
    conflicts_path: PathBuf,
    /// Monotonic counter bumped on every mutation. Lets a reader — e.g. the IPC
    /// state monitor — skip re-serializing the (potentially large) journal when
    /// nothing changed. Invariant: every mutator must either go through a
    /// `save_*` funnel (which bumps this) or bump it directly, as the
    /// non-persisting `replace_from_remote` does — otherwise a reader goes stale.
    dirty_version: AtomicU64,
    /// Staging files of uploads enqueued since the last save. Their bytes are
    /// forced to disk before any journal naming them is: a journal that
    /// survives a crash while its staging file did not would replay a
    /// truncated upload over the server's good copy.
    unsynced: Vec<PathBuf>,
    /// Set by `defer_saves`: the journal file is written by a worker instead
    /// of by whoever changed the journal (see `DeferredSaves`).
    deferred: Option<Arc<DeferredSaves>>,
    /// A change not yet handed to the deferred saver's next write.
    save_pending: bool,
    /// Staging files the journal stopped naming (superseded, coalesced away,
    /// uploaded). Deleted only once a journal that no longer names them is on
    /// disk: until then the journal a crash would reload still does, and
    /// `load_or_create` drops an entry whose staging is gone — while the newer
    /// entry that replaced it was never written. That lost both versions.
    delete_after_save: Vec<PathBuf>,
    /// Staging files of handles release() has taken out of `open_files` but
    /// not journaled yet: in neither place, a purge would take them for
    /// orphans (see `reserve_staging`). Never persisted.
    reserved: std::collections::HashSet<PathBuf>,
}

pub type SharedJournal = Arc<Mutex<MutationJournal>>;

/// What `MutationJournal::newest_upload` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingUpload {
    /// A whole-file upload, and its staging file.
    Put(PathBuf),
    /// The end of a streamed upload: the file's content exists only as the
    /// server's upload session plus the local tail until it is assembled.
    Stream,
}

/// Group commit for the journal file.
///
/// Every enqueue used to rewrite the journal and fsync it and its directory
/// before returning, and `release` fsynced the staging file first — on the
/// FUSE dispatch thread, for every close of a written file and every mkdir,
/// unlink and rename. Deferred, a change only marks the journal dirty in
/// memory (which is what every reader consults) and one worker writes the
/// latest whole snapshot, so a burst of changes costs one write.
///
/// What a crash can lose is unchanged in kind: changes from the last few
/// milliseconds, the same ones a crash slightly earlier would have lost. Each
/// written snapshot is a state the journal really had, and every staging file
/// it names was fsynced before it was written.
pub struct DeferredSaves {
    // One saver job queued or running at a time.
    armed: AtomicBool,
    // Held from taking a snapshot until it is on disk, so snapshots land in
    // the order they were taken (a late old one never overwrites a newer
    // one) and two writers never share the temp file.
    write: Mutex<()>,
    journal: std::sync::Weak<Mutex<MutationJournal>>,
    pool: &'static crate::bg::Pool,
}

/// Switches `journal` to deferred saves on `pool` (see [`DeferredSaves`]).
pub fn defer_saves(journal: &SharedJournal, pool: &'static crate::bg::Pool) {
    let d = Arc::new(DeferredSaves {
        armed: AtomicBool::new(false),
        write: Mutex::new(()),
        journal: Arc::downgrade(journal),
        pool,
    });
    journal.lock().unwrap_or_else(|e| e.into_inner()).deferred = Some(d);
}

/// Writes any change the deferred saver has not written yet, now, on the
/// calling thread. For shutdown: the process may exit before a queued save runs.
pub fn flush_deferred(journal: &SharedJournal) {
    let d = journal.lock().unwrap_or_else(|e| e.into_inner()).deferred.clone();
    if let Some(d) = d {
        let _ = d.write_pending();
    }
}

impl DeferredSaves {
    fn schedule(self: &Arc<Self>) {
        if self.armed.swap(true, Ordering::SeqCst) {
            return; // the queued or running saver will see this change
        }
        let d = self.clone();
        if let Err(r) = self.pool.submit(move || d.run()) {
            // Left pending: the next change or the shutdown flush writes it.
            self.armed.store(false, Ordering::SeqCst);
            log::warn!("JOURNAL: {} — save deferred to the next change", r);
        }
    }

    fn run(self: &Arc<Self>) {
        let mut retry_in = SAVE_RETRY_MIN;
        loop {
            if self.write_pending().is_err() {
                // Still armed and still pending (write_pending put it back):
                // a full disk must not turn into a busy loop, nor drop the save.
                if self.journal.strong_count() == 0 {
                    self.armed.store(false, Ordering::SeqCst);
                    return;
                }
                std::thread::sleep(retry_in);
                retry_in = (retry_in * 2).min(SAVE_RETRY_MAX);
                continue;
            }
            retry_in = SAVE_RETRY_MIN;
            self.armed.store(false, Ordering::SeqCst);
            // A change made after our snapshot but before disarming found the
            // saver still armed and left it to us.
            let again = match self.journal.upgrade() {
                Some(j) => j.lock().unwrap_or_else(|e| e.into_inner()).save_pending,
                None => false,
            };
            if !again || self.armed.swap(true, Ordering::SeqCst) {
                return;
            }
        }
    }

    /// Writes the latest snapshot if one is pending. On failure everything it
    /// took is put back, so the next attempt (or the shutdown flush) writes it.
    fn write_pending(&self) -> std::io::Result<()> {
        let _w = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let Some(journal) = self.journal.upgrade() else { return Ok(()) };
        let (data, unsynced, deletable, path) = {
            let mut j = journal.lock().unwrap_or_else(|e| e.into_inner());
            if !j.save_pending {
                return Ok(());
            }
            j.save_pending = false;
            let deletable = j.take_deletable();
            (j.serialize_entries(), std::mem::take(&mut j.unsynced), deletable, j.journal_path.clone())
        };
        // Outside the journal lock: nothing here may stall a FUSE handler.
        sync_staging(&unsynced);
        let written = match data {
            Some(data) => write_atomic_durable(&path, &data),
            None => Err(std::io::Error::other("journal serialize failed")),
        };
        match written {
            Ok(()) => {
                delete_staging(&deletable);
                Ok(())
            }
            Err(e) => {
                log::error!("JOURNAL: durable write failed: {} — will retry", e);
                let mut j = journal.lock().unwrap_or_else(|e| e.into_inner());
                j.save_pending = true;
                j.unsynced.extend(unsynced);
                j.delete_after_save.extend(deletable);
                Err(e)
            }
        }
    }
}

/// Backoff between attempts of a deferred save that failed (ENOSPC, EIO).
const SAVE_RETRY_MIN: Duration = Duration::from_millis(100);
const SAVE_RETRY_MAX: Duration = Duration::from_secs(5);

/// Ends deferred saves for the rest of the process: writes what is pending
/// now, and from then on every change is on disk before the call that made
/// it returns. For shutdown, once a stop signal arrives: whatever happens
/// after it — a `SIGKILL` when the unmount stays busy past the stop timeout —
/// loses nothing.
pub fn save_synchronously(journal: &SharedJournal) {
    let d = journal.lock().unwrap_or_else(|e| e.into_inner()).deferred.clone();
    let Some(d) = d else { return };
    // Waits out a deferred write in flight; none can start while this is held
    // (they take `write` before the journal), so no older snapshot can land
    // after the synchronous ones.
    let _w = d.write.lock().unwrap_or_else(|e| e.into_inner());
    let mut j = journal.lock().unwrap_or_else(|e| e.into_inner());
    j.deferred = None;
    j.save_pending = false;
    let _ = j.write_now();
}

fn delete_staging(paths: &[PathBuf]) {
    for p in paths {
        match std::fs::remove_file(p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("JOURNAL: removing staging {} failed: {}", p.display(), e),
        }
    }
}

/// fsyncs staging files before a journal naming them is written. A missing
/// one was already uploaded, superseded or recovered. A failed fsync is not
/// fatal: the save still goes ahead, only its crash-safety is weaker.
fn sync_staging(paths: &[PathBuf]) {
    for p in paths {
        if let Ok(f) = std::fs::File::open(p) {
            if let Err(e) = f.sync_all() {
                log::warn!("JOURNAL: fsync staging {} failed: {}", p.display(), e);
            }
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ── Staging file names ───────────────────────────────────────────────────

/// Tags every staging file this process creates. File handles restart at 1
/// in each process, and the startup sweep keeps the `write_<fh>` files a
/// surviving journal still names; a bare `write_<fh>` would let the new
/// process's first open truncate the bytes of a pending offline upload (and
/// that upload's success delete the new file's staging). No `.`: callers
/// derive temp names with `Path::with_extension`.
pub(crate) fn boot_tag() -> &'static str {
    static TAG: OnceLock<String> = OnceLock::new();
    TAG.get_or_init(|| format!("{}p{}", now_ms(), std::process::id()))
}

/// `write_<boot>_<fh>`: the staging file of handle `fh` of this process.
pub(crate) fn staging_file_name(fh: u64) -> String {
    format!("write_{}_{}", boot_tag(), fh)
}

/// `adopted_<boot>_<n>`: a file adopted from the bare mount point at startup.
pub(crate) fn adopted_file_name(n: u64) -> String {
    format!("adopted_{}_{}", boot_tag(), n)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StagingName {
    /// Created by this process, with its handle (or adoption) number.
    Ours(u64),
    /// Left by an earlier process — including the untagged `write_<fh>` and
    /// `adopted_<pid>_<n>` names of older versions.
    Earlier,
}

/// Classifies a `write_*` / `adopted_*` staging file name; `None` for any
/// other name (temp files such as `write_…seed` included).
pub(crate) fn parse_staging_name(name: &str) -> Option<StagingName> {
    let rest = name.strip_prefix("write_").or_else(|| name.strip_prefix("adopted_"))?;
    let (boot, n) = match rest.rsplit_once('_') {
        Some((boot, n)) => (Some(boot), n),
        None => (None, rest),
    };
    let n: u64 = n.parse().ok()?;
    Some(if boot == Some(boot_tag()) { StagingName::Ours(n) } else { StagingName::Earlier })
}

/// What `recovered/<file>.json` says about `recovered/<file>`, so the user can
/// tell what a kept file was.
#[derive(Serialize, Deserialize)]
pub(crate) struct RecoveredSidecar {
    /// The file it was written as, when known.
    pub remote_path: Option<PathBuf>,
    pub size: u64,
    pub recovered_at_ms: u64,
    pub reason: String,
    /// Its staging file's name.
    pub staging_name: String,
    /// For the end of a streamed upload: the server's upload session holding
    /// the file's first `tail_offset` bytes, until the server expires it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail_offset: Option<u64>,
}

/// Moves staging file `src` into `<cache_dir>/recovered/` with a sidecar
/// (`<file>.json`), and returns where it went. For bytes that are neither on
/// the server nor named by the journal and must not be deleted.
pub(crate) fn move_to_recovered(cache_dir: &Path, src: &Path, remote_path: Option<&Path>, reason: &str) -> Option<PathBuf> {
    move_to_recovered_noted(cache_dir, src, remote_path, reason, None)
}

/// `move_to_recovered` for the tail of a streamed upload, whose first
/// `offset` bytes are in the server's upload session `session`.
fn move_to_recovered_noted(cache_dir: &Path, src: &Path, remote_path: Option<&Path>, reason: &str, session: Option<(&str, u64)>) -> Option<PathBuf> {
    let dir = cache_dir.join(RECOVERED_DIR);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::error!("cannot create {}: {} — leaving {} in place", dir.display(), e, src.display());
        return None;
    }
    let name = src.file_name()?.to_string_lossy().into_owned();
    let mut dest = dir.join(&name);
    if dest.exists() {
        dest = dir.join(format!("{}.{}", name, now_ms()));
    }
    let size = std::fs::metadata(src).map(|m| m.len()).unwrap_or(0);
    if let Err(e) = std::fs::rename(src, &dest) {
        log::error!("cannot move {} to {}: {}", src.display(), dest.display(), e);
        return None;
    }
    let sidecar = RecoveredSidecar {
        remote_path: remote_path.map(Path::to_path_buf),
        size,
        recovered_at_ms: now_ms(),
        reason: reason.to_string(),
        staging_name: name,
        upload_session: session.map(|(s, _)| s.to_string()),
        tail_offset: session.map(|(_, o)| o),
    };
    let json = dest.with_file_name(format!("{}.json", dest.file_name()?.to_string_lossy()));
    if let Err(e) = serde_json::to_vec_pretty(&sidecar).map_err(std::io::Error::other).and_then(|b| std::fs::write(&json, b)) {
        log::warn!("cannot write {}: {}", json.display(), e);
    }
    Some(dest)
}

/// Where the startup sweep keeps staging files no journal entry names.
pub(crate) const RECOVERED_DIR: &str = "recovered";
/// How long `recovered/` keeps a file. Only age evicts: a count or size bound
/// deleted the oldest kept edit to make room, which could be its only copy.
const RECOVERED_KEEP_FOR: Duration = Duration::from_secs(30 * 24 * 3600);

/// `<staging>.tail`: marks a streamed upload's staging file once the first
/// chunk has left it, i.e. once it no longer starts at the file's first byte.
/// Such a tail is worth nothing alone: its earlier chunks are in a server
/// session only the lost process knew. The startup sweep deletes one it finds
/// unnamed instead of keeping it in `recovered/`. A staging file whose first
/// chunk has not been cut from it holds every byte written, and is kept.
pub(crate) fn tail_marker(staging: &Path) -> PathBuf {
    staging.with_extension("tail")
}

/// Startup sweep of `cache_dir`: staging files left by an earlier process
/// that `journal` does not name. They used to be deleted, but a crash can
/// leave real edits there — a written file never closed, bytes an `fsync`
/// made durable (fsync writes no journal record), an upload whose journal
/// save the crash beat. Non-empty ones are moved to `recovered/`, each with a
/// `.json` note (`RecoveredSidecar`), and reported once as a conflict; empty
/// ones, partial temp files and streamed-upload tails (`tail_marker`) carry
/// nothing usable and are deleted. `recovered/` is pruned by age first, so
/// nothing this sweep adds is evicted by it. Returns how many were moved.
pub(crate) fn quarantine_unreferenced_staging(journal: &mut MutationJournal, cache_dir: &Path) -> usize {
    let recovered_dir = cache_dir.join(RECOVERED_DIR);
    prune_recovered(&recovered_dir, SystemTime::now());
    let named: std::collections::HashSet<PathBuf> =
        journal.entries().iter().filter_map(|e| e.op.staging_path().map(Path::to_path_buf)).collect();
    let Ok(dir_entries) = std::fs::read_dir(cache_dir) else { return 0 };
    let dir_entries: Vec<_> = dir_entries.flatten().collect();
    let tails: std::collections::HashSet<PathBuf> = dir_entries.iter()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "tail"))
        .map(|p| p.with_extension(""))
        .collect();
    let (mut moved, mut deleted) = (Vec::new(), 0usize);
    for entry in dir_entries {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
        let kind = parse_staging_name(&name);
        let temp = kind.is_none() && name.starts_with("write_");
        if !(kind == Some(StagingName::Earlier) || temp) {
            continue; // not staging, or this process's own (adopted this boot)
        }
        let path = entry.path();
        if named.contains(&path) {
            continue;
        }
        let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
        if tails.contains(&path) && len > 0 {
            log::warn!("startup: {} ({} bytes) is the end of a streamed upload that never finished — deleted; the copy must be repeated", name, len);
            let _ = std::fs::remove_file(&path);
            continue;
        }
        if temp || len == 0 {
            if std::fs::remove_file(&path).is_ok() {
                deleted += 1;
            }
            continue;
        }
        if let Some(dest) = move_to_recovered(cache_dir, &path, None, "written by a session that ended before it was queued for upload") {
            log::warn!("startup: staging {} ({} bytes) is in no pending upload — kept at {}", name, len, dest.display());
            moved.push(dest);
        }
    }
    if deleted > 0 {
        log::info!("startup: removed {} empty or partial staging file(s)", deleted);
    }
    if !moved.is_empty() {
        journal.add_conflict(ConflictKind::PermanentFailure {
            description: format!(
                "{} locally written file(s) from an interrupted session were not queued for upload; their bytes are kept in {} (each with a .json note), for {} days",
                moved.len(),
                recovered_dir.display(),
                RECOVERED_KEEP_FOR.as_secs() / 86_400,
            ),
        });
    }
    moved.len()
}

/// Deletes what `recovered/` has kept for longer than `RECOVERED_KEEP_FOR`,
/// by the time its note records. A file without a note (kept by an older
/// version) gets one dated now instead: never deleted the first time it is
/// seen. A note whose file is gone goes too.
fn prune_recovered(dir: &Path, now: SystemTime) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let now_ms = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    let keep_ms = RECOVERED_KEEP_FOR.as_millis() as u64;
    for e in rd.flatten() {
        let path = e.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else { continue };
        if let Some(of) = name.strip_suffix(".json") {
            if !dir.join(of).exists() {
                let _ = std::fs::remove_file(&path);
            }
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let note = dir.join(format!("{name}.json"));
        match std::fs::read(&note).ok().and_then(|b| serde_json::from_slice::<RecoveredSidecar>(&b).ok()) {
            Some(s) if now_ms.saturating_sub(s.recovered_at_ms) > keep_ms => {
                log::warn!("startup: deleting {} from {}, kept since {} days ago ({:?})", name, RECOVERED_DIR, now_ms.saturating_sub(s.recovered_at_ms) / 86_400_000, s.remote_path);
                let _ = std::fs::remove_file(&path);
                let _ = std::fs::remove_file(&note);
            }
            Some(_) => {}
            None => {
                let s = RecoveredSidecar {
                    remote_path: None,
                    size: meta.len(),
                    recovered_at_ms: now_ms,
                    reason: "kept by an earlier version".into(),
                    staging_name: name.clone(),
                    upload_session: None,
                    tail_offset: None,
                };
                if let Ok(b) = serde_json::to_vec_pretty(&s) {
                    let _ = std::fs::write(&note, b);
                }
            }
        }
    }
}

/// Atomically and durably replace `path` with `data`: write a temp file, fsync
/// its contents, rename it into place, then fsync the containing directory so
/// the rename itself survives a crash/power-loss. Without the fsyncs the journal
/// could be lost or truncated on power-loss even though the app's save returned
/// success — which would strand the staged bytes with no record to replay them.
fn write_atomic_durable(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        // Directory fsync makes the rename durable. Best-effort: not all
        // filesystems require or support it, so a failure here is not fatal.
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

impl MutationOp {
    pub fn path(&self) -> &Path {
        match self {
            MutationOp::Put { remote_path, .. } => remote_path,
            MutationOp::MkDir { path } => path,
            MutationOp::Unlink { path } => path,
            MutationOp::RmDir { path } => path,
            MutationOp::Rename { from, .. } => from,
            MutationOp::FinishChunked { remote_path, .. } => remote_path,
        }
    }

    /// Local bytes this op still needs; the startup sweep and purge must keep them.
    pub fn staging_path(&self) -> Option<&Path> {
        match self {
            MutationOp::Put { staging_path, .. } => Some(staging_path),
            MutationOp::FinishChunked { tail_path, .. } => Some(tail_path),
            _ => None,
        }
    }

    /// An upload of `path` (whole-file or the end of a streamed one).
    fn is_upload_of(&self, path: &Path) -> bool {
        matches!(self, MutationOp::Put { remote_path, .. } | MutationOp::FinishChunked { remote_path, .. } if remote_path == path)
    }

    fn update_path_prefix(&mut self, old_prefix: &Path, new_prefix: &Path) {
        fn rewrite(p: &mut PathBuf, old: &Path, new: &Path) {
            if let Ok(suffix) = p.strip_prefix(old) {
                *p = new.join(suffix);
            }
        }
        match self {
            MutationOp::Put { remote_path, .. } => rewrite(remote_path, old_prefix, new_prefix),
            MutationOp::MkDir { path } => rewrite(path, old_prefix, new_prefix),
            MutationOp::Unlink { path } => rewrite(path, old_prefix, new_prefix),
            MutationOp::RmDir { path } => rewrite(path, old_prefix, new_prefix),
            MutationOp::Rename { from, to } => {
                rewrite(from, old_prefix, new_prefix);
                rewrite(to, old_prefix, new_prefix);
            }
            MutationOp::FinishChunked { remote_path, .. } => rewrite(remote_path, old_prefix, new_prefix),
        }
    }
}

impl MutationJournal {
    pub fn load_or_create(cache_dir: &Path) -> Self {
        let journal_path = cache_dir.join(JOURNAL_FILE);
        let conflicts_path = cache_dir.join(CONFLICTS_FILE);

        let (mut entries, next_seq) = if journal_path.exists() {
            match std::fs::read(&journal_path).ok().and_then(|d| serde_json::from_slice::<Vec<JournalEntry>>(&d).ok()) {
                Some(list) => {
                    let max_seq = list.iter().map(|e| e.seq).max().unwrap_or(0);
                    let q: VecDeque<JournalEntry> = list.into();
                    (q, max_seq + 1)
                }
                None => {
                    log::warn!("JOURNAL: failed to parse {}, starting fresh", journal_path.display());
                    (VecDeque::new(), 1)
                }
            }
        } else {
            (VecDeque::new(), 1)
        };

        // Validate Put staging files exist
        let mut orphaned = Vec::new();
        for entry in &entries {
            if let Some(staging_path) = entry.op.staging_path() {
                if !staging_path.exists() {
                    log::warn!("JOURNAL: staging file missing for {}, will discard", entry.op.path().display());
                    orphaned.push(entry.seq);
                }
            }
        }
        entries.retain(|e| !orphaned.contains(&e.seq));

        let (conflicts, next_conflict_id) = if conflicts_path.exists() {
            match std::fs::read(&conflicts_path).ok().and_then(|d| serde_json::from_slice::<Vec<ConflictRecord>>(&d).ok()) {
                Some(list) => {
                    let max_id = list.iter().map(|c| c.id).max().unwrap_or(0);
                    (list, max_id + 1)
                }
                None => {
                    log::warn!("JOURNAL: failed to parse {}, starting fresh", conflicts_path.display());
                    (Vec::new(), 1)
                }
            }
        } else {
            (Vec::new(), 1)
        };

        // Record orphaned entries as permanent failures
        let mut journal = MutationJournal {
            entries,
            next_seq,
            conflicts,
            next_conflict_id,
            dirty_version: AtomicU64::new(0),
            unsynced: Vec::new(),
            deferred: None,
            save_pending: false,
            delete_after_save: Vec::new(),
            reserved: std::collections::HashSet::new(),
            journal_path,
            conflicts_path,
        };
        for seq in orphaned {
            journal.add_conflict(ConflictKind::PermanentFailure {
                description: format!("staging file missing for journal entry seq={}", seq),
            });
        }
        if !journal.conflicts.is_empty() {
            journal.save_conflicts();
        }
        if !journal.entries.is_empty() {
            log::info!("JOURNAL: loaded {} pending entries", journal.entries.len());
        }
        journal
    }

    pub fn enqueue(&mut self, op: MutationOp) -> SeqId {
        self.coalesce_before_enqueue(&op);

        // Earlier uploads and directory changes follow the file to its new
        // name. Earlier Renames do not: each is a MOVE the server has yet to
        // make, in the names of its own time. Rewritten, `mv a b; mv b c`
        // replayed as MOVE a→c then b→c (404), and `mv d/f e/f; mv e k`
        // moved into a /k that did not exist yet.
        if let MutationOp::Rename { ref from, ref to } = op {
            for entry in &mut self.entries {
                if !matches!(entry.op, MutationOp::Rename { .. }) {
                    entry.op.update_path_prefix(from, to);
                }
            }
        }

        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries.push_back(JournalEntry {
            seq,
            op,
            created_at_ms: now_ms(),
            attempts: 0,
            last_error: None,
            not_before_ms: 0,
            in_flight: false,
        });
        if let Some(sp) = self.entries.back().and_then(|e| e.op.staging_path()) {
            self.unsynced.push(sp.to_path_buf());
        }
        self.save_journal();
        seq
    }

    pub fn dequeue_front(&mut self) -> Option<JournalEntry> {
        let entry = self.entries.pop_front();
        if entry.is_some() {
            self.save_journal();
        }
        entry
    }

    pub fn peek_front(&self) -> Option<&JournalEntry> {
        self.entries.front()
    }

    /// True while a Put for `path` is still queued (not yet uploaded/removed).
    /// Used to hold back a live MOVE until the source exists on the server.
    pub fn has_pending_put(&self, path: &Path) -> bool {
        self.entries.iter().any(|e| e.op.is_upload_of(path))
    }

    pub fn contains(&self, seq: SeqId) -> bool {
        self.entries.iter().any(|e| e.seq == seq)
    }

    /// The newest queued upload of `path`: it holds the file's current content,
    /// which neither the server nor a kept copy has yet.
    pub fn newest_upload(&self, path: &Path) -> Option<PendingUpload> {
        self.entries.iter().rev().find_map(|e| match &e.op {
            MutationOp::Put { remote_path, staging_path, .. } if remote_path == path => {
                Some(PendingUpload::Put(staging_path.clone()))
            }
            MutationOp::FinishChunked { remote_path, .. } if remote_path == path => Some(PendingUpload::Stream),
            _ => None,
        })
    }

    /// Staging file of the newest queued upload of `path` when that upload is a
    /// whole-file Put. Reads of a locally-written-but-not-yet-uploaded file can
    /// be served from here instead of streaming from a server that does not
    /// have the content yet. None when the newest is a streamed upload: an
    /// older Put's staging is an older version of the file.
    pub fn pending_put_staging(&self, path: &Path) -> Option<PathBuf> {
        match self.newest_upload(path) {
            Some(PendingUpload::Put(staging)) => Some(staging),
            _ => None,
        }
    }

    /// Where the server still has what this mount renamed to `path`, itself
    /// or with a directory above it, while none of the queued Renames has
    /// landed: each one is undone, newest to oldest (`mv d e; mv e/f g/f`
    /// puts `/g/f` at `/d/f` on the server). None when no queued Rename
    /// moved it.
    pub fn rename_source_of(&self, path: &Path) -> Option<PathBuf> {
        let mut at = path.to_path_buf();
        let mut moved = false;
        for e in self.entries.iter().rev() {
            if let MutationOp::Rename { from, to } = &e.op {
                if let Ok(rest) = at.strip_prefix(to) {
                    at = if rest.as_os_str().is_empty() { from.clone() } else { from.join(rest) };
                    moved = true;
                }
            }
        }
        moved.then_some(at)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Marks `seq` as being executed; false when it is gone (superseded, coalesced
    /// away, finished) or another worker already has it.
    pub fn claim(&mut self, seq: SeqId) -> bool {
        match self.entries.iter_mut().find(|e| e.seq == seq) {
            Some(e) if !e.in_flight => {
                e.in_flight = true;
                true
            }
            _ => false,
        }
    }

    /// Drops every not-yet-claimed upload of `path` older than `newest`: each upload
    /// carries the whole file, so only the newest needs to reach the server.
    pub fn supersede_uploads(&mut self, path: &Path, newest: SeqId) {
        let mut stale = Vec::new();
        self.entries.retain(|e| {
            let drop = e.seq < newest && !e.in_flight && e.op.is_upload_of(path);
            if drop {
                stale.push(e.op.clone());
            }
            !drop
        });
        if stale.is_empty() {
            return;
        }
        for op in &stale {
            if let Some(sp) = op.staging_path() {
                self.delete_after_save.push(sp.to_path_buf());
            }
        }
        log::debug!("JOURNAL: {} superseded upload(s) of {} dropped", stale.len(), path.display());
        self.save_journal();
    }

    pub fn mark_failed(&mut self, seq: SeqId, error: String) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.seq == seq) {
            entry.attempts += 1;
            entry.last_error = Some(error);
            entry.in_flight = false;
            entry.not_before_ms = now_ms() + retry_backoff(entry.attempts).as_millis() as u64;
        }
        self.save_journal();
    }

    /// Makes every entry due now, as if its backoff had run out.
    #[cfg(test)]
    pub(crate) fn skip_backoff(&mut self) {
        for e in &mut self.entries {
            e.not_before_ms = 0;
        }
    }

    /// Record a transient failure (network down / unreachable) without counting
    /// it against the attempt budget. Offline editing must be able to retry
    /// indefinitely once connectivity returns — only genuine server rejections
    /// should ever exhaust MAX_ATTEMPTS and become a permanent failure.
    pub fn mark_deferred(&mut self, seq: SeqId, error: String) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.seq == seq) {
            entry.last_error = Some(error);
            entry.in_flight = false;
        }
        self.save_journal();
    }

    pub fn remove(&mut self, seq: SeqId) {
        self.entries.retain(|e| e.seq != seq);
        self.save_journal();
    }

    /// `remove`, and delete `staging` once the journal without `seq` is on
    /// disk (see `delete_after_save`). For an upload that finished or was
    /// given up: deleting first and crashing before the save would reload the
    /// entry without its bytes.
    pub fn remove_discarding(&mut self, seq: SeqId, staging: &Path) {
        self.delete_after_save.push(staging.to_path_buf());
        self.remove(seq);
    }

    /// Deletes `staging` after the next save that no longer names it. The
    /// caller follows with the change that stops naming it (which saves).
    pub fn discard_staging(&mut self, staging: &Path) {
        self.delete_after_save.push(staging.to_path_buf());
    }

    /// The part of `delete_after_save` no current entry names; the rest waits.
    fn take_deletable(&mut self) -> Vec<PathBuf> {
        if self.delete_after_save.is_empty() {
            return Vec::new();
        }
        let named: std::collections::HashSet<&Path> =
            self.entries.iter().filter_map(|e| e.op.staging_path()).collect();
        let (keep, go): (Vec<PathBuf>, Vec<PathBuf>) =
            std::mem::take(&mut self.delete_after_save).into_iter().partition(|p| named.contains(p.as_path()));
        self.delete_after_save = keep;
        go
    }

    pub fn entries(&self) -> &VecDeque<JournalEntry> {
        &self.entries
    }

    /// Marks staging file `p` as about to be journaled. release() calls it
    /// before it takes the handle out of `open_files` and
    /// `unreserve_staging` after the commit enqueued (or gave up on) it, so
    /// at every moment the file is in `open_files`, reserved here, or named
    /// by an entry — what a purge that reads `open_files` first and then
    /// `staging_in_use` relies on.
    pub fn reserve_staging(&mut self, p: &Path) {
        self.reserved.insert(p.to_path_buf());
    }

    pub fn unreserve_staging(&mut self, p: &Path) {
        self.reserved.remove(p);
    }

    /// Every staging file the journal still needs or will delete itself:
    /// named by an entry, waiting for the save that stops naming it (the
    /// journal on disk still does), or reserved by a release in progress.
    pub fn staging_in_use(&self) -> std::collections::HashSet<PathBuf> {
        self.entries.iter()
            .filter_map(|e| e.op.staging_path().map(Path::to_path_buf))
            .chain(self.delete_after_save.iter().cloned())
            .chain(self.reserved.iter().cloned())
            .collect()
    }

    pub fn max_attempts() -> u32 {
        MAX_ATTEMPTS
    }

    // ── Conflicts ────────────────────────────────────────────

    pub fn add_conflict(&mut self, kind: ConflictKind) -> u64 {
        let id = self.next_conflict_id;
        self.next_conflict_id += 1;
        self.conflicts.push(ConflictRecord {
            id,
            kind,
            timestamp_ms: now_ms(),
            resolved: false,
        });
        self.save_conflicts();
        id
    }

    pub fn resolve_conflict(&mut self, id: u64) {
        if let Some(c) = self.conflicts.iter_mut().find(|c| c.id == id) {
            c.resolved = true;
        }
        self.save_conflicts();
    }

    pub fn resolve_all_conflicts(&mut self) {
        for c in &mut self.conflicts {
            c.resolved = true;
        }
        self.save_conflicts();
    }

    pub fn unresolved_conflicts(&self) -> Vec<&ConflictRecord> {
        self.conflicts.iter().filter(|c| !c.resolved).collect()
    }

    pub fn all_conflicts(&self) -> &[ConflictRecord] {
        &self.conflicts
    }

    /// Replace in-memory entries and conflicts from remote state (attach mode).
    /// Does NOT persist to disk — the daemon's files are authoritative.
    pub fn replace_from_remote(&mut self, entries: Vec<JournalEntry>, conflicts: Vec<ConflictRecord>) {
        self.entries = entries.into();
        self.conflicts = conflicts;
        self.dirty_version.fetch_add(1, Ordering::Relaxed);
    }

    /// Monotonic version bumped on every mutation. A reader can cache derived
    /// data (e.g. a serialized snapshot) and rebuild it only when this changes.
    pub fn version(&self) -> u64 {
        self.dirty_version.load(Ordering::Relaxed)
    }

    // ── Recovery ─────────────────────────────────────────────

    /// Move a failed upload's staged bytes out of the volatile write-staging area
    /// into a durable `unsynced/` recovery folder, so a permanent failure never
    /// silently destroys the user's local edit. Returns the recovery path on
    /// success. Named after the remote file (not the opaque `write_N` staging
    /// name) so the user can recognise it; collisions get the staging name as a
    /// disambiguating suffix.
    fn recover_staging(&self, staging_path: &Path, remote_path: &Path) -> Option<PathBuf> {
        let cache_dir = self.journal_path.parent()?;
        let recovery_dir = cache_dir.join("unsynced");
        if std::fs::create_dir_all(&recovery_dir).is_err() {
            return None;
        }
        let base = remote_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "recovered".to_string());
        let mut dest = recovery_dir.join(&base);
        if dest.exists() {
            if let Some(uniq) = staging_path.file_name().and_then(|n| n.to_str()) {
                dest = recovery_dir.join(format!("{}.{}", base, uniq));
            }
        }
        // rename is atomic on the same filesystem; fall back to copy+remove across
        // filesystems (cache dir and staging are normally colocated, so rare).
        if std::fs::rename(staging_path, &dest).is_ok() {
            log::warn!("JOURNAL: preserved unsynced local copy at {}", dest.display());
            return Some(dest);
        }
        match std::fs::copy(staging_path, &dest) {
            Ok(_) => {
                let _ = std::fs::remove_file(staging_path);
                log::warn!("JOURNAL: preserved unsynced local copy at {}", dest.display());
                Some(dest)
            }
            Err(e) => {
                log::error!("JOURNAL: failed to preserve staging {}: {}", staging_path.display(), e);
                None
            }
        }
    }

    // ── Persistence ──────────────────────────────────────────

    fn save_journal(&mut self) {
        self.dirty_version.fetch_add(1, Ordering::Relaxed);
        if let Some(d) = &self.deferred {
            self.save_pending = true;
            d.schedule();
            return;
        }
        let _ = self.write_now();
    }

    /// The synchronous save: staging fsyncs, the journal, then the staging
    /// files it stopped naming. What a failure leaves is retried by the next save.
    fn write_now(&mut self) -> std::io::Result<()> {
        let unsynced = std::mem::take(&mut self.unsynced);
        sync_staging(&unsynced);
        let deletable = self.take_deletable();
        let written = match self.serialize_entries() {
            Some(data) => write_atomic_durable(&self.journal_path, &data),
            None => Err(std::io::Error::other("journal serialize failed")),
        };
        match written {
            Ok(()) => {
                delete_staging(&deletable);
                Ok(())
            }
            Err(e) => {
                log::error!("JOURNAL: durable write failed: {}", e);
                self.unsynced.extend(unsynced);
                self.delete_after_save.extend(deletable);
                Err(e)
            }
        }
    }

    fn serialize_entries(&self) -> Option<Vec<u8>> {
        let list: Vec<&JournalEntry> = self.entries.iter().collect();
        serde_json::to_vec(&list).map_err(|e| log::error!("JOURNAL: serialize failed: {}", e)).ok()
    }

    fn save_conflicts(&self) {
        self.dirty_version.fetch_add(1, Ordering::Relaxed);
        match serde_json::to_vec(&self.conflicts) {
            Ok(data) => {
                if let Err(e) = write_atomic_durable(&self.conflicts_path, &data) {
                    log::error!("JOURNAL: conflicts durable write failed: {}", e);
                }
            }
            Err(e) => log::error!("JOURNAL: conflicts serialize failed: {}", e),
        }
    }

    // ── Coalescing ───────────────────────────────────────────

    fn coalesce_before_enqueue(&mut self, new_op: &MutationOp) {
        match new_op {
            MutationOp::Unlink { path } => {
                // If there's a Put for this path that was a fresh create (no etag),
                // remove it — the file never reached the server
                let has_prior_server_etag = self.entries.iter().any(|e| {
                    matches!(&e.op,
                        MutationOp::Put { remote_path, if_match_etag: Some(_), .. }
                        | MutationOp::FinishChunked { remote_path, if_match_etag: Some(_), .. } if remote_path == path)
                });
                if !has_prior_server_etag {
                    let staging_to_delete: Vec<PathBuf> = self.entries.iter()
                        .filter(|e| e.op.is_upload_of(path))
                        .filter_map(|e| e.op.staging_path().map(Path::to_path_buf))
                        .collect();
                    let before = self.entries.len();
                    self.entries.retain(|e| {
                        !e.op.is_upload_of(path)
                        && !matches!(&e.op, MutationOp::MkDir { path: p } if p == path)
                    });
                    if self.entries.len() < before {
                        self.delete_after_save.extend(staging_to_delete);
                        log::debug!("JOURNAL: coalesced — removed prior ops for {} before Unlink", path.display());
                    }
                }
            }
            MutationOp::RmDir { path } => {
                let before = self.entries.len();
                self.entries.retain(|e| {
                    !matches!(&e.op, MutationOp::MkDir { path: p } if p == path)
                });
                if self.entries.len() < before {
                    log::debug!("JOURNAL: coalesced — removed MkDir for {} before RmDir", path.display());
                }
            }
            _ => {}
        }
    }
}

// ── Replay ────────────────────────────────────────────────────────────────

pub struct ReplayContext {
    pub backend: std::sync::Arc<dyn crate::backend::CloudBackend>,
    /// Path → status map, so a successful replay clears the PendingSync marker
    /// the live upload path set when it first failed.
    pub status: crate::ipc::StatusMap,
}

pub(crate) fn replay_journal(
    journal: &SharedJournal,
    ctx: &ReplayContext,
    cache: &Arc<Mutex<crate::FsCache>>,
    dirty: &crate::ipc::DirtySet,
    error_log: &crate::ErrorLog,
) {
    use crate::MutexExt;

    loop {
        let entry = {
            let mut j = journal.safe_lock();
            match j.peek_front() {
                // A live worker is executing it; later entries may depend on it, so stop.
                Some(e) if e.in_flight => {
                    log::debug!("JOURNAL: replay paused — seq={} is being uploaded live", e.seq);
                    return;
                }
                // Later entries may depend on it too: wait for its backoff.
                Some(e) if e.not_before_ms > now_ms() => {
                    log::debug!("JOURNAL: replay paused — seq={} was rejected {} time(s), next try in {} s", e.seq, e.attempts, (e.not_before_ms - now_ms()) / 1000);
                    return;
                }
                Some(e) => {
                    let e = e.clone();
                    j.claim(e.seq);
                    e
                }
                None => {
                    log::info!("JOURNAL: replay complete — queue empty");
                    return;
                }
            }
        };

        if entry.attempts >= MutationJournal::max_attempts() {
            log::warn!("JOURNAL: entry seq={} exceeded max attempts, marking permanent failure", entry.seq);
            let last_err = entry.last_error.as_deref().unwrap_or("unknown");
            // Preserve the local bytes instead of deleting them — a permanent
            // failure must never silently destroy the user's edit.
            let desc = match &entry.op {
                MutationOp::Put { staging_path, remote_path, .. } => {
                    match journal.safe_lock().recover_staging(staging_path, remote_path) {
                        Some(p) => format!("{:?}: {} — local copy preserved at {}", entry.op, last_err, p.display()),
                        None => format!("{:?}: {}", entry.op, last_err),
                    }
                }
                // Never aborted, and its tail never deleted: the session holds
                // every byte but the tail's, and together they are the only
                // copy of the file. The session is left for the server to
                // expire; the tail goes to `recovered/` with a note naming it.
                MutationOp::FinishChunked { remote_path, uploads_base, bytes_confirmed, tail_path, .. } => {
                    let cache_dir = journal.safe_lock().journal_path.parent().map(Path::to_path_buf);
                    let kept = cache_dir.and_then(|d| move_to_recovered_noted(
                        &d, tail_path, Some(remote_path),
                        "the end of a streamed upload the server refused to assemble",
                        Some((uploads_base, *bytes_confirmed)),
                    ));
                    let _ = std::fs::remove_file(tail_marker(tail_path));
                    format!(
                        "{} could not be assembled on the server ({}). Nothing was deleted: its first {} bytes are in the server's upload session {} until the server expires it, and the rest is kept at {}. Copy the file again, or have an admin assemble the session.",
                        remote_path.display(), last_err, bytes_confirmed, uploads_base,
                        kept.as_deref().unwrap_or(tail_path).display(),
                    )
                }
                _ => format!("{:?}: {}", entry.op, last_err),
            };
            let mut j = journal.safe_lock();
            j.add_conflict(ConflictKind::PermanentFailure { description: desc });
            j.dequeue_front();
            if let MutationOp::Unlink { path } | MutationOp::RmDir { path } = &entry.op {
                cache.safe_lock().deleting.remove(path);
            }
            continue;
        }

        match execute_op(&entry, ctx, cache, dirty, error_log) {
            ReplayResult::Ok => {
                let mut j = journal.safe_lock();
                if let Some(staging_path) = entry.op.staging_path() {
                    j.discard_staging(staging_path);
                }
                j.dequeue_front();
                if let MutationOp::Unlink { path } | MutationOp::RmDir { path } = &entry.op {
                    cache.safe_lock().deleting.remove(path);
                }
            }
            ReplayResult::Conflict(kind) => {
                // Only discard the staged bytes when they are known safe on the
                // server. An EditConflict has already uploaded a conflicted copy,
                // so the local staging is redundant. Any other conflict (e.g. the
                // destination/parent is gone) means the bytes exist ONLY locally —
                // preserve them so the user can recover.
                let mut j = journal.safe_lock();
                if let MutationOp::Put { staging_path, remote_path, .. } = &entry.op {
                    if matches!(kind, ConflictKind::EditConflict { .. }) {
                        j.discard_staging(staging_path);
                    } else {
                        j.recover_staging(staging_path, remote_path);
                    }
                }
                if let MutationOp::FinishChunked { tail_path, .. } = &entry.op {
                    j.discard_staging(tail_path);
                }
                j.add_conflict(kind);
                j.dequeue_front();
            }
            ReplayResult::Idempotent => {
                let mut j = journal.safe_lock();
                if let Some(staging_path) = entry.op.staging_path() {
                    j.discard_staging(staging_path);
                }
                j.dequeue_front();
                if let MutationOp::Unlink { path } | MutationOp::RmDir { path } = &entry.op {
                    cache.safe_lock().deleting.remove(path);
                }
            }
            ReplayResult::Retryable(msg) => {
                log::warn!("JOURNAL: replay stopped — retryable failure: {}", msg);
                // Transient (network down, server 5xx/timeout, locked): do not count
                // against the attempt budget and stop the run — the connectivity
                // monitor replays again later, once the server is back.
                journal.safe_lock().mark_deferred(entry.seq, msg);
                return;
            }
            ReplayResult::ServerError(msg) => {
                journal.safe_lock().mark_failed(entry.seq, msg);
                if let MutationOp::Unlink { path } | MutationOp::RmDir { path } = &entry.op {
                    cache.safe_lock().deleting.remove(path);
                }
            }
        }
    }
}

enum ReplayResult {
    Ok,
    Conflict(ConflictKind),
    Idempotent,
    /// Transient failure — retry later without counting against the attempt budget.
    Retryable(String),
    /// Permanent server rejection — counts toward MAX_ATTEMPTS.
    ServerError(String),
}

fn execute_op(
    entry: &JournalEntry,
    ctx: &ReplayContext,
    cache: &Arc<Mutex<crate::FsCache>>,
    dirty: &crate::ipc::DirtySet,
    error_log: &crate::ErrorLog,
) -> ReplayResult {
    use crate::backend::BackendWriteError;
    use crate::MutexExt;

    match &entry.op {
        MutationOp::Put { remote_path, staging_path, if_match_etag } => {
            let file_size = match std::fs::metadata(staging_path) {
                Ok(m) => m.len(),
                Err(e) => {
                    return ReplayResult::Conflict(ConflictKind::PermanentFailure {
                        description: format!("staging file unreadable for {}: {}", remote_path.display(), e),
                    });
                }
            };
            let etag_ref = if_match_etag.as_deref();
            match ctx.backend.put_file_from_path(remote_path, staging_path, etag_ref) {
                Ok(result) => {
                    log::info!("JOURNAL replay: PUT {} → token {:?}", remote_path.display(), result.new_change_token);
                    let mut c = cache.safe_lock();
                    let parent = remote_path.parent().unwrap_or(Path::new("/")).to_path_buf();
                    if let Some(dir) = c.dir_cache.get_mut(&parent) {
                        let mut files = (*dir.files).clone();
                        if let Some(e) = files.iter_mut().find(|e| e.path == *remote_path) {
                            e.change_token = result.new_change_token;
                            e.size = file_size;
                            e.modified = Some(SystemTime::now());
                        }
                        dir.files = Arc::new(files);
                    }
                    drop(c);
                    // Clear the PendingSync marker the live upload path set on failure.
                    {
                        use crate::RwLockExt;
                        ctx.status.safe_write().insert(remote_path.clone(), crate::ipc::FileStatus::Synced);
                    }
                    dirty.safe_lock().insert(parent);
                    dirty.safe_lock().insert(remote_path.clone());
                    ReplayResult::Ok
                }
                Err(BackendWriteError::Conflict) => {
                    log::warn!("JOURNAL replay: PUT {} conflict — creating conflicted copy", remote_path.display());
                    let conflict_name = crate::make_conflict_name(remote_path);
                    // Only an uploaded copy lets the staging file go (see the
                    // Conflict arm of `replay_journal`).
                    if let Err(e) = ctx.backend.put_file_from_path(&conflict_name, staging_path, None) {
                        log::error!("JOURNAL replay: conflicted copy {} not uploaded: {} — kept queued", conflict_name.display(), e);
                        return conflict_copy_retry(e);
                    }
                    crate::push_error(error_log, remote_path.clone(), crate::SyncErrorKind::Conflict, "Server version changed — conflicted copy created".into());
                    ReplayResult::Conflict(ConflictKind::EditConflict {
                        local_path: remote_path.clone(),
                        conflicted_copy_path: conflict_name,
                    })
                }
                Err(e) if e.is_transient() => ReplayResult::Retryable(e.to_string()),
                Err(BackendWriteError::Server(404, _)) => {
                    ReplayResult::Conflict(ConflictKind::PermanentFailure {
                        description: format!("PUT {} failed: parent directory not found", remote_path.display()),
                    })
                }
                Err(e) => ReplayResult::ServerError(e.to_string()),
            }
        }
        MutationOp::MkDir { path } => {
            match ctx.backend.mkdir(path) {
                Ok(()) => {
                    log::info!("JOURNAL replay: MKCOL {}", path.display());
                    ReplayResult::Ok
                }
                Err(e) if e.is_transient() => ReplayResult::Retryable(e.to_string()),
                Err(e) => ReplayResult::ServerError(e.to_string()),
            }
        }
        MutationOp::Unlink { path } => {
            match ctx.backend.delete(path) {
                Ok(()) => {
                    log::info!("JOURNAL replay: DELETE {}", path.display());
                    ReplayResult::Ok
                }
                Err(BackendWriteError::Server(404, _)) => {
                    log::info!("JOURNAL replay: DELETE {} — already gone (idempotent)", path.display());
                    ReplayResult::Idempotent
                }
                Err(e) if e.is_transient() => ReplayResult::Retryable(e.to_string()),
                Err(e) => ReplayResult::ServerError(e.to_string()),
            }
        }
        MutationOp::RmDir { path } => {
            match ctx.backend.delete(path) {
                Ok(()) => {
                    log::info!("JOURNAL replay: RMDIR {}", path.display());
                    ReplayResult::Ok
                }
                Err(BackendWriteError::Server(404, _)) => {
                    log::info!("JOURNAL replay: RMDIR {} — already gone (idempotent)", path.display());
                    ReplayResult::Idempotent
                }
                Err(e) if e.is_transient() => ReplayResult::Retryable(e.to_string()),
                Err(e) => ReplayResult::ServerError(e.to_string()),
            }
        }
        MutationOp::Rename { from, to } => {
            match ctx.backend.rename(from, to) {
                Ok(()) => {
                    log::info!("JOURNAL replay: MOVE {} → {}", from.display(), to.display());
                    ReplayResult::Ok
                }
                Err(BackendWriteError::Server(404, _)) => {
                    ReplayResult::Conflict(ConflictKind::MoveSourceGone {
                        from: from.clone(),
                        to: to.clone(),
                    })
                }
                Err(BackendWriteError::Conflict) => {
                    ReplayResult::Conflict(ConflictKind::MoveDestExists {
                        from: from.clone(),
                        to: to.clone(),
                    })
                }
                Err(e) if e.is_transient() => ReplayResult::Retryable(e.to_string()),
                Err(e) => ReplayResult::ServerError(e.to_string()),
            }
        }
        MutationOp::FinishChunked { remote_path, uploads_base, next_index, bytes_confirmed, total_len, tail_path, if_match_etag } => {
            let result = finish_chunked(
                &*ctx.backend, uploads_base, *next_index, *bytes_confirmed, *total_len,
                tail_path, remote_path, if_match_etag.as_deref(),
            );
            match result {
                Ok(result) => {
                    log::info!("JOURNAL replay: finished streamed upload {} → token {:?}", remote_path.display(), result.new_change_token);
                    let parent = remote_path.parent().unwrap_or(Path::new("/")).to_path_buf();
                    {
                        let mut c = cache.safe_lock();
                        if let Some(dir) = c.dir_cache.get_mut(&parent) {
                            let mut files = (*dir.files).clone();
                            if let Some(e) = files.iter_mut().find(|e| e.path == *remote_path) {
                                e.change_token = result.new_change_token;
                                e.size = *total_len;
                                e.modified = Some(SystemTime::now());
                            }
                            dir.files = Arc::new(files);
                        }
                    }
                    {
                        use crate::RwLockExt;
                        ctx.status.safe_write().insert(remote_path.clone(), crate::ipc::FileStatus::Synced);
                    }
                    dirty.safe_lock().insert(parent);
                    dirty.safe_lock().insert(remote_path.clone());
                    ReplayResult::Ok
                }
                Err(BackendWriteError::Conflict) => {
                    // Changed on the server meanwhile: every chunk is already there, so
                    // assemble ours as a conflicted copy instead.
                    log::warn!("JOURNAL replay: streamed upload {} conflict — assembling a conflicted copy", remote_path.display());
                    let conflict_name = crate::make_conflict_name(remote_path);
                    let session = crate::backend::ChunkedUploadSession { uploads_base: uploads_base.clone() };
                    if let Err(e) = ctx.backend.finish_chunked_upload(&session, &conflict_name, None) {
                        log::error!("JOURNAL replay: conflicted copy {} not assembled: {} — kept queued", conflict_name.display(), e);
                        return conflict_copy_retry(e);
                    }
                    crate::push_error(error_log, remote_path.clone(), crate::SyncErrorKind::Conflict, "Server version changed — conflicted copy created".into());
                    ReplayResult::Conflict(ConflictKind::EditConflict {
                        local_path: remote_path.clone(),
                        conflicted_copy_path: conflict_name,
                    })
                }
                Err(e) if e.is_transient() => ReplayResult::Retryable(e.to_string()),
                Err(BackendWriteError::Server(404, _)) => {
                    ReplayResult::Conflict(ConflictKind::PermanentFailure {
                        description: format!("streamed upload of {} expired on the server — copy the file again", remote_path.display()),
                    })
                }
                Err(e) => ReplayResult::ServerError(e.to_string()),
            }
        }
    }
}

/// A conflicted copy the server did not take: retried later, a transient
/// failure without costing an attempt.
fn conflict_copy_retry(e: crate::backend::BackendWriteError) -> ReplayResult {
    let msg = format!("conflicted copy not uploaded: {}", e);
    if e.is_transient() { ReplayResult::Retryable(msg) } else { ReplayResult::ServerError(msg) }
}

/// Uploads the tail of a streamed upload as its last chunk and assembles the session.
/// Re-running it is safe: a chunk PUT to the same index replaces the earlier one.
pub(crate) fn finish_chunked(
    backend: &dyn crate::backend::CloudBackend,
    uploads_base: &str,
    next_index: u64,
    bytes_confirmed: u64,
    total_len: u64,
    tail_path: &Path,
    remote_path: &Path,
    if_match: Option<&str>,
) -> Result<crate::backend::PutResult, crate::backend::BackendWriteError> {
    use crate::backend::BackendWriteError;
    let session = crate::backend::ChunkedUploadSession { uploads_base: uploads_base.to_string() };
    let tail_len = total_len.saturating_sub(bytes_confirmed);
    if tail_len > 0 {
        // Server(0, _): a local fault, never transient, so it is not retried forever.
        let on_disk = std::fs::metadata(tail_path)
            .map_err(|e| BackendWriteError::Server(0, format!("staging tail {}: {}", tail_path.display(), e)))?
            .len();
        if on_disk != tail_len {
            return Err(BackendWriteError::Server(0, format!(
                "staging tail is {} bytes, expected {} — refusing to assemble a truncated file",
                on_disk, tail_len,
            )));
        }
        // Streamed from the file, not read into memory: the tail can hold
        // more than one chunk.
        crate::retry_chunk_write("final chunk upload", || backend.put_chunk_from_path(&session, next_index, tail_path, tail_len))?;
    }
    crate::retry_chunk_write("chunked-upload finish", || backend.finish_chunked_upload(&session, remote_path, if_match))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ncrs_journal_test_{}_{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn write_atomic_durable_roundtrips_and_replaces() {
        let dir = temp_dir("atomic_durable");
        let target = dir.join("state.json");

        write_atomic_durable(&target, b"first").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"first");
        // No stray temp file left behind after the rename.
        assert!(!target.with_extension("tmp").exists());

        // Overwrite is atomic and durable.
        write_atomic_durable(&target, b"second-longer").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"second-longer");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn roundtrip_empty() {
        let dir = temp_dir("roundtrip_empty");
        let j = MutationJournal::load_or_create(&dir);
        assert!(j.is_empty());
        assert_eq!(j.unresolved_conflicts().len(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    // ── Staging names ────────────────────────────────────────

    fn staged_put(dir: &Path, name: &str, bytes: &str, etag: Option<&str>) -> (MutationOp, PathBuf) {
        let staging = dir.join(name);
        fs::write(&staging, bytes).unwrap();
        let op = MutationOp::Put {
            remote_path: PathBuf::from("/f.txt"),
            staging_path: staging.clone(),
            if_match_etag: etag.map(str::to_owned),
        };
        (op, staging)
    }

    /// What a restart finds: the journal on disk plus the startup sweep.
    fn restart(dir: &Path) -> MutationJournal {
        let mut j = MutationJournal::load_or_create(dir);
        quarantine_unreferenced_staging(&mut j, dir);
        j
    }

    fn staged_bytes(j: &MutationJournal) -> Vec<String> {
        j.entries().iter().filter_map(|e| e.op.staging_path()).map(|p| fs::read_to_string(p).unwrap()).collect()
    }

    #[test]
    fn staging_names_carry_the_process_and_parse_back() {
        assert_eq!(parse_staging_name(&staging_file_name(7)), Some(StagingName::Ours(7)));
        assert_eq!(parse_staging_name(&adopted_file_name(3)), Some(StagingName::Ours(3)));
        assert_eq!(parse_staging_name("write_7"), Some(StagingName::Earlier), "untagged names are from older versions");
        assert_eq!(parse_staging_name("adopted_4242_1"), Some(StagingName::Earlier));
        assert_eq!(parse_staging_name("write_1790000000000p99_7"), Some(StagingName::Earlier));
        assert_eq!(parse_staging_name("write_7.seed"), None);
        assert_eq!(parse_staging_name("mutation_journal.json"), None);
        assert!(!staging_file_name(1).contains('.'), "with_extension-derived temp names must stay distinct");
    }

    #[test]
    fn a_pending_upload_from_an_earlier_process_survives_and_is_not_reused() {
        let dir = temp_dir("earlier_staging");
        let legacy = dir.join("write_1");
        {
            let mut j = MutationJournal::load_or_create(&dir);
            let (op, _) = staged_put(&dir, "write_1", "offline edit", Some("e1"));
            j.enqueue(op);
        }
        let j = restart(&dir);
        assert_eq!(staged_bytes(&j), vec!["offline edit"]);
        assert_ne!(dir.join(staging_file_name(1)), legacy, "the new process's first handle must not truncate it");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_startup_sweep_quarantines_unnamed_bytes_and_drops_empty_files() {
        let dir = temp_dir("quarantine");
        fs::write(dir.join("write_3"), "unsaved edit").unwrap();
        fs::write(dir.join("write_4"), "").unwrap();
        fs::write(dir.join("write_5.seed"), "partial").unwrap();
        fs::write(dir.join("adopted_77_1"), "old adoption").unwrap();
        fs::write(dir.join(adopted_file_name(1)), "adopted this boot").unwrap();
        fs::write(dir.join("other.bin"), "x").unwrap();
        let mut j = MutationJournal::load_or_create(&dir);
        assert_eq!(quarantine_unreferenced_staging(&mut j, &dir), 2);
        let rec = dir.join(RECOVERED_DIR);
        assert_eq!(fs::read_to_string(rec.join("write_3")).unwrap(), "unsaved edit");
        assert_eq!(fs::read_to_string(rec.join("adopted_77_1")).unwrap(), "old adoption");
        assert!(!dir.join("write_4").exists() && !dir.join("write_5.seed").exists());
        assert!(dir.join(adopted_file_name(1)).exists(), "this process's adoptions are journaled right after");
        assert!(dir.join("other.bin").exists());
        assert_eq!(j.unresolved_conflicts().len(), 1);
        let note: RecoveredSidecar = serde_json::from_slice(&fs::read(rec.join("write_3.json")).unwrap()).unwrap();
        assert_eq!((note.size, note.staging_name.as_str(), note.remote_path), (12, "write_3", None));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_startup_sweep_deletes_a_streamed_tail_but_keeps_a_stream_that_never_lost_a_chunk() {
        let dir = temp_dir("quarantine_tails");
        // Its first chunk left: only the end of the file, useless alone.
        fs::write(dir.join("write_9"), "end of a copy").unwrap();
        fs::write(tail_marker(&dir.join("write_9")), "").unwrap();
        // A stream whose session opened but whose first chunk never went: every byte.
        fs::write(dir.join("write_10"), "the whole file").unwrap();
        let mut j = MutationJournal::load_or_create(&dir);
        assert_eq!(quarantine_unreferenced_staging(&mut j, &dir), 1);
        let rec = dir.join(RECOVERED_DIR);
        assert!(!dir.join("write_9").exists() && !rec.join("write_9").exists());
        assert!(!tail_marker(&dir.join("write_9")).exists());
        assert_eq!(fs::read_to_string(rec.join("write_10")).unwrap(), "the whole file");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn recovered_evicts_only_by_age_and_never_what_this_sweep_added() {
        let dir = temp_dir("recovered_prune");
        let rec = dir.join(RECOVERED_DIR);
        // Many and large is no reason to delete anything.
        for i in 0..100 {
            let src = dir.join(format!("write_{i}"));
            fs::write(&src, "x").unwrap();
            move_to_recovered(&dir, &src, None, "test").unwrap();
        }
        let old = rec.join("write_0");
        let mut note: RecoveredSidecar = serde_json::from_slice(&fs::read(rec.join("write_0.json")).unwrap()).unwrap();
        note.recovered_at_ms = now_ms() - 31 * 86_400_000;
        fs::write(rec.join("write_0.json"), serde_json::to_vec(&note).unwrap()).unwrap();
        // Kept by an older version, without a note, and with an ancient mtime.
        fs::write(rec.join("legacy"), "y").unwrap();
        fs::File::options().write(true).open(rec.join("legacy")).unwrap().set_modified(UNIX_EPOCH + Duration::from_secs(1_000_000)).unwrap();
        fs::write(rec.join("gone.json"), "{}").unwrap();
        // A staging file as old as can be, swept in by this very run.
        fs::write(dir.join("write_1790000000000p1_1"), "new edit").unwrap();
        fs::File::options().write(true).open(dir.join("write_1790000000000p1_1")).unwrap().set_modified(UNIX_EPOCH + Duration::from_secs(1_000_000)).unwrap();
        let mut j = MutationJournal::load_or_create(&dir);
        quarantine_unreferenced_staging(&mut j, &dir);
        assert!(!old.exists() && !rec.join("write_0.json").exists(), "older than 30 days");
        assert!(rec.join("write_99").exists());
        assert!(rec.join("legacy").exists() && rec.join("legacy.json").exists(), "first seen: dated now, kept");
        assert!(!rec.join("gone.json").exists());
        assert_eq!(fs::read_to_string(rec.join("write_1790000000000p1_1")).unwrap(), "new edit");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn enqueue_dequeue_ordering() {
        let dir = temp_dir("enqueue_dequeue");
        let mut j = MutationJournal::load_or_create(&dir);
        let s1 = j.enqueue(MutationOp::MkDir { path: PathBuf::from("/a") });
        let s2 = j.enqueue(MutationOp::MkDir { path: PathBuf::from("/b") });
        assert!(s2 > s1);
        assert_eq!(j.len(), 2);

        let front = j.dequeue_front().unwrap();
        assert_eq!(front.seq, s1);
        assert_eq!(j.len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persistence_survives_reload() {
        let dir = temp_dir("persistence");
        {
            let mut j = MutationJournal::load_or_create(&dir);
            j.enqueue(MutationOp::Unlink { path: PathBuf::from("/foo.txt") });
            j.add_conflict(ConflictKind::PermanentFailure {
                description: "test".into(),
            });
        }
        let j2 = MutationJournal::load_or_create(&dir);
        assert_eq!(j2.len(), 1);
        assert_eq!(j2.unresolved_conflicts().len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn coalesce_unlink_after_fresh_put() {
        let dir = temp_dir("coalesce_unlink");
        let staging = dir.join("staging_1");
        fs::write(&staging, b"data").unwrap();

        let mut j = MutationJournal::load_or_create(&dir);
        j.enqueue(MutationOp::Put {
            remote_path: PathBuf::from("/new.txt"),
            staging_path: staging,
            if_match_etag: None,
        });
        assert_eq!(j.len(), 1);

        j.enqueue(MutationOp::Unlink { path: PathBuf::from("/new.txt") });
        assert_eq!(j.len(), 1);
        let front = j.peek_front().unwrap();
        assert!(matches!(&front.op, MutationOp::Unlink { .. }));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unlink_after_fresh_put_leaves_no_pending_put_to_race() {
        // Race guard for the LibreOffice lock-file case (`.~lock.doc.odt#`):
        // create-then-delete must NOT leave a queued Put that a live DELETE could
        // race — otherwise the DELETE hits the server while the upload still holds
        // Nextcloud's transactional lock and comes back 423. After coalescing, the
        // path must have no pending Put and the fresh-create staging must be gone.
        let dir = temp_dir("unlink_race_guard");
        let staging = dir.join("staging_lock");
        fs::write(&staging, b"lock-bytes").unwrap();
        let path = PathBuf::from("/.~lock.doc.odt#");

        let mut j = MutationJournal::load_or_create(&dir);
        j.enqueue(MutationOp::Put {
            remote_path: path.clone(),
            staging_path: staging.clone(),
            if_match_etag: None,
        });
        assert!(j.has_pending_put(&path), "fresh create should register a pending Put");

        j.enqueue(MutationOp::Unlink { path: path.clone() });
        assert!(!j.has_pending_put(&path), "coalesced Unlink must leave no Put for a DELETE to race");
        assert!(!staging.exists(), "coalesced fresh-create staging must be cleaned up");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn coalesce_rmdir_after_mkdir() {
        let dir = temp_dir("coalesce_rmdir");
        let mut j = MutationJournal::load_or_create(&dir);
        j.enqueue(MutationOp::MkDir { path: PathBuf::from("/mydir") });
        assert_eq!(j.len(), 1);

        j.enqueue(MutationOp::RmDir { path: PathBuf::from("/mydir") });
        assert_eq!(j.len(), 1);
        let front = j.peek_front().unwrap();
        assert!(matches!(&front.op, MutationOp::RmDir { .. }));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_updates_subsequent_paths() {
        let dir = temp_dir("rename_paths");
        let staging = dir.join("staging_2");
        fs::write(&staging, b"data").unwrap();

        let mut j = MutationJournal::load_or_create(&dir);
        j.enqueue(MutationOp::Put {
            remote_path: PathBuf::from("/old_dir/file.txt"),
            staging_path: staging,
            if_match_etag: None,
        });
        j.enqueue(MutationOp::Rename {
            from: PathBuf::from("/old_dir"),
            to: PathBuf::from("/new_dir"),
        });

        let first = &j.entries()[0];
        if let MutationOp::Put { remote_path, .. } = &first.op {
            assert_eq!(remote_path, &PathBuf::from("/new_dir/file.txt"));
        } else {
            panic!("expected Put");
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_put_staging_follows_rename() {
        // Mirrors the LibreOffice save: write temp file (Put), then rename temp onto
        // the final name. enqueue() rewrites the queued Put's remote_path to the
        // destination, so a read of the destination resolves to the temp's staging.
        let dir = temp_dir("pending_put_staging");
        let staging = dir.join("staging_lo");
        fs::write(&staging, b"odf-bytes").unwrap();

        let mut j = MutationJournal::load_or_create(&dir);
        j.enqueue(MutationOp::Put {
            remote_path: PathBuf::from("/docs/lu123.tmp"),
            staging_path: staging.clone(),
            if_match_etag: None,
        });
        assert_eq!(j.pending_put_staging(&PathBuf::from("/docs/lu123.tmp")), Some(staging.clone()));

        j.enqueue(MutationOp::Rename {
            from: PathBuf::from("/docs/lu123.tmp"),
            to: PathBuf::from("/docs/report.odt"),
        });
        // Old path no longer resolves; destination now serves the staging bytes.
        assert!(j.pending_put_staging(&PathBuf::from("/docs/lu123.tmp")).is_none());
        assert_eq!(j.pending_put_staging(&PathBuf::from("/docs/report.odt")), Some(staging));
        assert!(j.has_pending_put(&PathBuf::from("/docs/report.odt")));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_newest_upload_of_a_path_is_its_content() {
        let dir = temp_dir("newest_upload");
        let (put, staging) = staged_put(&dir, "write_old", "old", None);
        let mut j = MutationJournal::load_or_create(&dir);
        let first = j.enqueue(put);
        j.claim(first); // in flight: a newer upload does not supersede it
        let tail = dir.join("write_tail");
        fs::write(&tail, b"end").unwrap();
        let stream = j.enqueue(MutationOp::FinishChunked {
            remote_path: PathBuf::from("/f.txt"),
            uploads_base: "u/1".into(),
            next_index: 1,
            bytes_confirmed: 10,
            total_len: 13,
            tail_path: tail,
            if_match_etag: None,
        });
        assert_eq!(j.newest_upload(Path::new("/f.txt")), Some(PendingUpload::Stream));
        assert_eq!(j.pending_put_staging(Path::new("/f.txt")), None, "the older Put's bytes are an older version");
        // A whole-file upload after the stream is the content again.
        let (put2, staging2) = staged_put(&dir, "write_new", "new", None);
        j.enqueue(put2);
        assert_eq!(j.pending_put_staging(Path::new("/f.txt")), Some(staging2));
        assert_ne!(j.pending_put_staging(Path::new("/f.txt")), Some(staging));
        assert!(j.contains(stream));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rename_source_is_found_across_every_queued_rename() {
        let dir = temp_dir("rename_hops");
        let mut j = MutationJournal::load_or_create(&dir);
        let mv = |j: &mut MutationJournal, a: &str, b: &str| j.enqueue(MutationOp::Rename { from: PathBuf::from(a), to: PathBuf::from(b) });
        // A directory, then a file out of it.
        mv(&mut j, "/d", "/e");
        mv(&mut j, "/e/f", "/g/f");
        assert_eq!(j.rename_source_of(Path::new("/g/f")), Some(PathBuf::from("/d/f")));
        assert_eq!(j.rename_source_of(Path::new("/e/h")), Some(PathBuf::from("/d/h")));
        assert_eq!(j.rename_source_of(Path::new("/x")), None);
        // A file renamed twice; its first MOVE keeps its own names.
        mv(&mut j, "/a", "/b");
        mv(&mut j, "/b", "/c");
        assert_eq!(j.rename_source_of(Path::new("/c")), Some(PathBuf::from("/a")));
        assert!(j.entries().iter().any(|e| matches!(&e.op, MutationOp::Rename { from, to } if from == Path::new("/a") && to == Path::new("/b"))));
        // A file renamed inside a directory that is then renamed.
        mv(&mut j, "/p/f", "/p/g");
        mv(&mut j, "/p", "/q");
        assert_eq!(j.rename_source_of(Path::new("/q/g")), Some(PathBuf::from("/p/f")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mark_failed_increments_attempts() {
        let dir = temp_dir("mark_failed");
        let mut j = MutationJournal::load_or_create(&dir);
        let seq = j.enqueue(MutationOp::MkDir { path: PathBuf::from("/fail") });
        j.mark_failed(seq, "timeout".into());

        let front = j.peek_front().unwrap();
        assert_eq!(front.attempts, 1);
        assert_eq!(front.last_error.as_deref(), Some("timeout"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mark_deferred_does_not_burn_attempts() {
        // Transient network failures must not count toward MAX_ATTEMPTS, so an
        // offline-edited file keeps retrying indefinitely until the server is back.
        let dir = temp_dir("mark_deferred");
        let mut j = MutationJournal::load_or_create(&dir);
        let seq = j.enqueue(MutationOp::MkDir { path: PathBuf::from("/offline") });

        for _ in 0..(MutationJournal::max_attempts() + 5) {
            j.mark_deferred(seq, "network unavailable".into());
        }

        let front = j.peek_front().unwrap();
        assert_eq!(front.attempts, 0, "deferred retries must not increment attempts");
        assert_eq!(front.last_error.as_deref(), Some("network unavailable"));
        assert_eq!(j.len(), 1, "entry must survive for later retry");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn conflict_lifecycle() {
        let dir = temp_dir("conflict_lifecycle");
        let mut j = MutationJournal::load_or_create(&dir);
        let id = j.add_conflict(ConflictKind::EditConflict {
            local_path: PathBuf::from("/a.txt"),
            conflicted_copy_path: PathBuf::from("/a (conflicted copy).txt"),
        });
        assert_eq!(j.unresolved_conflicts().len(), 1);

        j.resolve_conflict(id);
        assert_eq!(j.unresolved_conflicts().len(), 0);
        assert_eq!(j.all_conflicts().len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn orphaned_put_becomes_conflict() {
        let dir = temp_dir("orphaned_put");
        let entries = vec![JournalEntry {
            seq: 1,
            op: MutationOp::Put {
                remote_path: PathBuf::from("/ghost.txt"),
                staging_path: dir.join("nonexistent_staging"),
                if_match_etag: None,
            },
            created_at_ms: now_ms(),
            attempts: 0,
            last_error: None,
            not_before_ms: 0,
            in_flight: false,
        }];
        let journal_path = dir.join(JOURNAL_FILE);
        fs::write(&journal_path, serde_json::to_vec(&entries).unwrap()).unwrap();

        let j = MutationJournal::load_or_create(&dir);
        assert!(j.is_empty());
        assert_eq!(j.unresolved_conflicts().len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    fn put(dir: &Path, name: &str) -> MutationOp {
        let staging = dir.join(format!("write_{}", name));
        fs::write(&staging, name).unwrap();
        MutationOp::Put { remote_path: PathBuf::from("/f.txt"), staging_path: staging, if_match_etag: None }
    }

    #[test]
    fn newer_upload_supersedes_unclaimed_older_ones_but_not_a_running_one() {
        let dir = temp_dir("supersede");
        let mut j = MutationJournal::load_or_create(&dir);
        let running = j.enqueue(put(&dir, "a"));
        assert!(j.claim(running));
        let queued = j.enqueue(put(&dir, "b"));
        let newest = j.enqueue(put(&dir, "c"));
        j.supersede_uploads(Path::new("/f.txt"), newest);
        assert!(j.contains(running), "an upload already on the wire must be left alone");
        assert!(!j.contains(queued), "an older queued upload is superseded");
        assert!(!dir.join("write_b").exists(), "a superseded upload's staging is removed");
        assert!(j.contains(newest));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn claim_is_exclusive_and_released_by_a_retryable_failure() {
        let dir = temp_dir("claim");
        let mut j = MutationJournal::load_or_create(&dir);
        let seq = j.enqueue(put(&dir, "a"));
        assert!(j.claim(seq));
        assert!(!j.claim(seq), "replay and a live worker must not both run one entry");
        j.mark_deferred(seq, "offline".into());
        assert!(j.claim(seq), "a deferred entry becomes claimable again");
        j.remove(seq);
        assert!(!j.claim(seq));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn finish_chunked_survives_reload_and_keeps_its_tail() {
        let dir = temp_dir("finish_chunked");
        let tail = dir.join("write_7");
        fs::write(&tail, b"tail").unwrap();
        let seq = {
            let mut j = MutationJournal::load_or_create(&dir);
            j.enqueue(MutationOp::FinishChunked {
                remote_path: PathBuf::from("/big.bin"),
                uploads_base: "https://h/remote.php/dav/uploads/u/x".into(),
                next_index: 2,
                bytes_confirmed: 20,
                total_len: 24,
                tail_path: tail.clone(),
                if_match_etag: Some("e1".into()),
            })
        };
        let j = MutationJournal::load_or_create(&dir);
        assert!(j.contains(seq));
        assert!(j.has_pending_put(Path::new("/big.bin")));
        assert_eq!(j.entries()[0].op.staging_path(), Some(tail.as_path()));
        assert!(!j.entries()[0].in_flight, "claims are never persisted");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn finish_chunked_with_missing_tail_is_dropped_on_load() {
        let dir = temp_dir("finish_chunked_orphan");
        {
            let mut j = MutationJournal::load_or_create(&dir);
            j.enqueue(MutationOp::FinishChunked {
                remote_path: PathBuf::from("/big.bin"),
                uploads_base: "u".into(),
                next_index: 1,
                bytes_confirmed: 10,
                total_len: 12,
                tail_path: dir.join("gone"),
                if_match_etag: None,
            });
        }
        let j = MutationJournal::load_or_create(&dir);
        assert!(j.is_empty());
        assert_eq!(j.unresolved_conflicts().len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_retargets_a_queued_streamed_finish() {
        let dir = temp_dir("finish_chunked_rename");
        let tail = dir.join("write_9");
        fs::write(&tail, b"").unwrap();
        let mut j = MutationJournal::load_or_create(&dir);
        j.enqueue(MutationOp::FinishChunked {
            remote_path: PathBuf::from("/d/tmp.bin"),
            uploads_base: "u".into(),
            next_index: 1,
            bytes_confirmed: 10,
            total_len: 10,
            tail_path: tail,
            if_match_etag: None,
        });
        j.enqueue(MutationOp::Rename { from: PathBuf::from("/d"), to: PathBuf::from("/e") });
        assert_eq!(j.entries()[0].op.path(), Path::new("/e/tmp.bin"));
        let _ = fs::remove_dir_all(&dir);
    }

    // ── Deferred saves (group commit) ────────────────────────

    fn on_disk(dir: &Path) -> Vec<SeqId> {
        MutationJournal::load_or_create(dir).entries().iter().map(|e| e.seq).collect()
    }

    fn deferred(dir: &Path, pool: &'static crate::bg::Pool) -> SharedJournal {
        let j = Arc::new(Mutex::new(MutationJournal::load_or_create(dir)));
        defer_saves(&j, pool);
        j
    }

    fn wait_saved(j: &SharedJournal, pool: &crate::bg::Pool) {
        let t = std::time::Instant::now();
        while j.lock().unwrap().save_pending || pool.stats().active > 0 {
            assert!(t.elapsed() < std::time::Duration::from_secs(10), "saver never drained");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    fn deferred_saves_land_the_latest_whole_journal() {
        static POOL: crate::bg::Pool = crate::bg::Pool::new("t-journal-save", 1, 16);
        let dir = temp_dir("deferred_latest");
        let j = deferred(&dir, &POOL);
        std::thread::scope(|sc| {
            for t in 0..4 {
                let j = &j;
                sc.spawn(move || {
                    for i in 0..25 {
                        j.lock().unwrap().enqueue(MutationOp::MkDir { path: PathBuf::from(format!("/d{t}_{i}")) });
                    }
                });
            }
        });
        wait_saved(&j, &POOL);
        let mem: Vec<SeqId> = j.lock().unwrap().entries().iter().map(|e| e.seq).collect();
        assert_eq!(mem.len(), 100);
        assert_eq!(on_disk(&dir), mem, "the file holds the journal as it last was");
        // Removing entries is saved the same way.
        let first = mem[0];
        j.lock().unwrap().remove(first);
        wait_saved(&j, &POOL);
        assert_eq!(on_disk(&dir), mem[1..].to_vec());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_change_the_saver_could_not_take_is_written_by_the_shutdown_flush() {
        // No worker can ever start: every scheduled save is refused.
        static POOL: crate::bg::Pool = crate::bg::Pool::new("t-journal-refuse", 0, 0);
        let dir = temp_dir("deferred_refused");
        let j = deferred(&dir, &POOL);
        let seq = j.lock().unwrap().enqueue(MutationOp::MkDir { path: PathBuf::from("/a") });
        assert!(on_disk(&dir).is_empty(), "nothing wrote it yet");
        assert!(!j.lock().unwrap().deferred.as_ref().unwrap().armed.load(Ordering::SeqCst), "a refused saver disarms");
        flush_deferred(&j);
        assert_eq!(on_disk(&dir), vec![seq]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn staging_files_go_to_the_save_that_first_names_them() {
        // Synchronous journal: synced and cleared by the enqueue's own save.
        let dir = temp_dir("unsynced_sync");
        let mut sync = MutationJournal::load_or_create(&dir);
        sync.enqueue(put(&dir, "a"));
        assert!(sync.unsynced.is_empty());
        let _ = fs::remove_dir_all(&dir);

        // Deferred: held for the saver, which syncs them before it writes.
        static POOL: crate::bg::Pool = crate::bg::Pool::new("t-journal-unsynced", 0, 0);
        let dir = temp_dir("unsynced_deferred");
        let j = deferred(&dir, &POOL);
        let op = put(&dir, "b");
        let staging = op.staging_path().unwrap().to_path_buf();
        j.lock().unwrap().enqueue(op);
        j.lock().unwrap().enqueue(MutationOp::MkDir { path: PathBuf::from("/m") });
        assert_eq!(j.lock().unwrap().unsynced, vec![staging], "only uploads carry staging bytes");
        flush_deferred(&j);
        assert!(j.lock().unwrap().unsynced.is_empty());
        assert_eq!(on_disk(&dir).len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    // ── Crash consistency: staging outlives the journal that names it ──

    #[test]
    fn a_crash_before_the_save_after_a_supersede_keeps_the_older_version() {
        static POOL: crate::bg::Pool = crate::bg::Pool::new("t-journal-crash-sup", 0, 0);
        let dir = temp_dir("crash_supersede");
        let j = deferred(&dir, &POOL);
        let (a, a_path) = staged_put(&dir, "write_1", "A", None);
        j.lock().unwrap().enqueue(a);
        flush_deferred(&j);
        let (b, b_path) = staged_put(&dir, "write_2", "B", None);
        {
            let mut g = j.lock().unwrap();
            let seq = g.enqueue(b);
            g.supersede_uploads(Path::new("/f.txt"), seq);
        }
        assert!(a_path.exists(), "the journal on disk still names A: its bytes must stay until it no longer does");

        // Crash here: the saver never ran.
        let reloaded = restart(&dir);
        assert_eq!(staged_bytes(&reloaded), vec!["A"], "A is still queued after the restart");
        assert!(reloaded.unresolved_conflicts().iter().any(|c| matches!(&c.kind, ConflictKind::PermanentFailure { description } if description.contains(RECOVERED_DIR))));
        assert_eq!(fs::read_to_string(dir.join(RECOVERED_DIR).join("write_2")).unwrap(), "B", "and B's bytes are kept, not swept");
        assert!(!b_path.exists());
        drop(reloaded);
        let _ = fs::remove_dir_all(&dir);

        // Without the crash, the save that drops A deletes A's staging.
        let dir = temp_dir("crash_supersede_saved");
        let j = deferred(&dir, &POOL);
        let (a, a_path) = staged_put(&dir, "write_1", "A", None);
        j.lock().unwrap().enqueue(a);
        flush_deferred(&j);
        let (b, b_path2) = staged_put(&dir, "write_2", "B", None);
        {
            let mut g = j.lock().unwrap();
            let seq = g.enqueue(b);
            g.supersede_uploads(Path::new("/f.txt"), seq);
        }
        flush_deferred(&j);
        assert!(!a_path.exists());
        assert_eq!(staged_bytes(&restart(&dir)), vec!["B"]);
        assert!(b_path2.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_crash_before_the_save_after_an_unlink_coalesce_keeps_the_put() {
        static POOL: crate::bg::Pool = crate::bg::Pool::new("t-journal-crash-coal", 0, 0);
        let dir = temp_dir("crash_coalesce");
        let j = deferred(&dir, &POOL);
        let (a, a_path) = staged_put(&dir, "write_1", "A", None);
        j.lock().unwrap().enqueue(a);
        flush_deferred(&j);
        j.lock().unwrap().enqueue(MutationOp::Unlink { path: PathBuf::from("/f.txt") });
        assert!(!j.lock().unwrap().has_pending_put(Path::new("/f.txt")), "the fresh create is coalesced away in memory");
        assert!(a_path.exists());
        assert_eq!(staged_bytes(&restart(&dir)), vec!["A"], "a crash before the save replays the journal it had");
        flush_deferred(&j);
        assert!(!a_path.exists(), "once saved, the coalesced staging goes");
        assert!(staged_bytes(&restart(&dir)).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_crash_before_the_save_after_an_upload_finished_keeps_its_bytes() {
        static POOL: crate::bg::Pool = crate::bg::Pool::new("t-journal-crash-put", 0, 0);
        let dir = temp_dir("crash_put_ok");
        let j = deferred(&dir, &POOL);
        let (a, a_path) = staged_put(&dir, "write_1", "A", None);
        let seq = j.lock().unwrap().enqueue(a);
        flush_deferred(&j);
        j.lock().unwrap().remove_discarding(seq, &a_path);
        assert!(a_path.exists());
        let reloaded = restart(&dir);
        assert_eq!(staged_bytes(&reloaded), vec!["A"], "the entry reloads with its bytes (a re-upload, not a spurious conflict)");
        assert!(reloaded.unresolved_conflicts().is_empty());
        flush_deferred(&j);
        assert!(!a_path.exists());
        assert!(restart(&dir).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_staging_file_still_named_is_never_deleted() {
        let dir = temp_dir("discard_still_named");
        let mut j = MutationJournal::load_or_create(&dir);
        let (a, a_path) = staged_put(&dir, "write_1", "A", None);
        let seq = j.enqueue(a);
        j.discard_staging(&a_path);
        j.mark_deferred(seq, "offline".into()); // a save while the entry still names it
        assert!(a_path.exists());
        j.remove(seq);
        assert!(!a_path.exists(), "deleted by the first save that no longer names it");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_deferred_save_is_retried_not_dropped() {
        static POOL: crate::bg::Pool = crate::bg::Pool::new("t-journal-retry", 1, 4);
        let dir = temp_dir("deferred_retry");
        let j = deferred(&dir, &POOL);
        fs::remove_dir_all(&dir).unwrap(); // every write now fails
        let seq = j.lock().unwrap().enqueue(MutationOp::MkDir { path: PathBuf::from("/a") });
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(j.lock().unwrap().save_pending || POOL.stats().active > 0, "the failed save is still pending");
        fs::create_dir_all(&dir).unwrap();
        wait_saved(&j, &POOL);
        assert_eq!(on_disk(&dir), vec![seq]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn switching_to_synchronous_saves_writes_now_and_every_later_change() {
        static POOL: crate::bg::Pool = crate::bg::Pool::new("t-journal-sync", 0, 0);
        let dir = temp_dir("to_synchronous");
        let j = deferred(&dir, &POOL);
        let a = j.lock().unwrap().enqueue(MutationOp::MkDir { path: PathBuf::from("/a") });
        assert!(on_disk(&dir).is_empty());
        save_synchronously(&j);
        assert_eq!(on_disk(&dir), vec![a]);
        let b = j.lock().unwrap().enqueue(MutationOp::MkDir { path: PathBuf::from("/b") });
        assert_eq!(on_disk(&dir), vec![a, b], "on disk before enqueue returned");
        let _ = fs::remove_dir_all(&dir);
    }
}
