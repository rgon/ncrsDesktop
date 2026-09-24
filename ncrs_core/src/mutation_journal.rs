use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const JOURNAL_FILE: &str = "mutation_journal.json";
const CONFLICTS_FILE: &str = "conflicts.json";
const MAX_ATTEMPTS: u32 = 3;

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
}

pub type SharedJournal = Arc<Mutex<MutationJournal>>;

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
        d.write_pending();
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
        loop {
            self.write_pending();
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

    fn write_pending(&self) {
        let _w = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let Some(journal) = self.journal.upgrade() else { return };
        let (data, unsynced, path) = {
            let mut j = journal.lock().unwrap_or_else(|e| e.into_inner());
            if !j.save_pending {
                return;
            }
            j.save_pending = false;
            (j.serialize_entries(), std::mem::take(&mut j.unsynced), j.journal_path.clone())
        };
        // Outside the journal lock: nothing here may stall a FUSE handler.
        sync_staging(&unsynced);
        if let Some(data) = data {
            if let Err(e) = write_atomic_durable(&path, &data) {
                log::error!("JOURNAL: durable write failed: {}", e);
            }
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

/// Where the startup sweep keeps staging files no journal entry names.
pub(crate) const RECOVERED_DIR: &str = "recovered";
const RECOVERED_KEEP_FILES: usize = 64;
const RECOVERED_KEEP_BYTES: u64 = 2 << 30;

/// Startup sweep of `cache_dir`: staging files left by an earlier process
/// that `journal` does not name. They used to be deleted, but a crash can
/// leave real edits there — a written file never closed, bytes an `fsync`
/// made durable (fsync writes no journal record), an upload whose journal
/// save the crash beat. Non-empty ones are moved to `recovered/` (bounded,
/// newest kept) and reported once as a conflict; empty ones and partial
/// temp files carry nothing and are deleted. Returns how many were moved.
pub(crate) fn quarantine_unreferenced_staging(journal: &mut MutationJournal, cache_dir: &Path) -> usize {
    let named: std::collections::HashSet<PathBuf> =
        journal.entries().iter().filter_map(|e| e.op.staging_path().map(Path::to_path_buf)).collect();
    let recovered_dir = cache_dir.join(RECOVERED_DIR);
    let (mut moved, mut deleted) = (Vec::new(), 0usize);
    let Ok(dir_entries) = std::fs::read_dir(cache_dir) else { return 0 };
    for entry in dir_entries.flatten() {
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
        if temp || len == 0 {
            if std::fs::remove_file(&path).is_ok() {
                deleted += 1;
            }
            continue;
        }
        if std::fs::create_dir_all(&recovered_dir).is_err() {
            log::error!("startup: cannot create {} — leaving {} in place", recovered_dir.display(), path.display());
            continue;
        }
        let mut dest = recovered_dir.join(&name);
        if dest.exists() {
            dest = recovered_dir.join(format!("{}.{}", name, now_ms()));
        }
        match std::fs::rename(&path, &dest) {
            Ok(()) => {
                log::warn!("startup: staging {} ({} bytes) is in no pending upload — kept at {}", name, len, dest.display());
                moved.push(dest);
            }
            Err(e) => log::error!("startup: cannot move {} to {}: {}", path.display(), dest.display(), e),
        }
    }
    if deleted > 0 {
        log::info!("startup: removed {} empty or partial staging file(s)", deleted);
    }
    if !moved.is_empty() {
        prune_recovered(&recovered_dir);
        journal.add_conflict(ConflictKind::PermanentFailure {
            description: format!(
                "{} locally written file(s) from an interrupted session were not queued for upload; their bytes are kept in {}",
                moved.len(),
                recovered_dir.display(),
            ),
        });
    }
    moved.len()
}

/// Keeps the newest files of `recovered/` within the count and size bounds.
fn prune_recovered(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok().filter(|m| m.is_file())?;
            Some((m.modified().unwrap_or(UNIX_EPOCH), m.len(), e.path()))
        })
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    let mut bytes = 0u64;
    for (i, (_, len, path)) in files.iter().enumerate() {
        bytes = bytes.saturating_add(*len);
        // The newest one is always kept, however large.
        if i > 0 && (i >= RECOVERED_KEEP_FILES || bytes > RECOVERED_KEEP_BYTES) {
            log::warn!("startup: {} over its bounds — deleting the oldest recovered file {}", RECOVERED_DIR, path.display());
            let _ = std::fs::remove_file(path);
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

        if let MutationOp::Rename { ref from, ref to } = op {
            for entry in &mut self.entries {
                entry.op.update_path_prefix(from, to);
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

    /// Staging file backing a still-queued Put for `path`, if any. Reads of a
    /// locally-written-but-not-yet-uploaded file can be served from here instead
    /// of streaming from a server that does not have the content yet.
    pub fn pending_put_staging(&self, path: &Path) -> Option<PathBuf> {
        self.entries.iter().rev().find_map(|e| match &e.op {
            MutationOp::Put { remote_path, staging_path, .. } if remote_path == path => {
                Some(staging_path.clone())
            }
            _ => None,
        })
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
                let _ = std::fs::remove_file(sp);
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
        }
        self.save_journal();
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

    pub fn entries(&self) -> &VecDeque<JournalEntry> {
        &self.entries
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
        sync_staging(&std::mem::take(&mut self.unsynced));
        if let Some(data) = self.serialize_entries() {
            if let Err(e) = write_atomic_durable(&self.journal_path, &data) {
                log::error!("JOURNAL: durable write failed: {}", e);
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
                        for sp in staging_to_delete { let _ = std::fs::remove_file(&sp); }
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
            // Preserve the local bytes instead of deleting them — a permanent
            // failure must never silently destroy the user's edit.
            let recovered = if let MutationOp::Put { staging_path, remote_path, .. } = &entry.op {
                journal.safe_lock().recover_staging(staging_path, remote_path)
            } else {
                None
            };
            // A streamed tail is not the whole file, so it is not worth preserving.
            if let MutationOp::FinishChunked { uploads_base, tail_path, .. } = &entry.op {
                ctx.backend.abort_chunked_upload(&crate::backend::ChunkedUploadSession { uploads_base: uploads_base.clone() });
                let _ = std::fs::remove_file(tail_path);
            }
            let mut j = journal.safe_lock();
            let last_err = entry.last_error.as_deref().unwrap_or("unknown");
            let desc = match recovered {
                Some(p) => format!("{:?}: {} — local copy preserved at {}", entry.op, last_err, p.display()),
                None => format!("{:?}: {}", entry.op, last_err),
            };
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
                j.dequeue_front();
                if let Some(staging_path) = entry.op.staging_path() {
                    let _ = std::fs::remove_file(staging_path);
                }
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
                if let MutationOp::Put { staging_path, remote_path, .. } = &entry.op {
                    if matches!(kind, ConflictKind::EditConflict { .. }) {
                        let _ = std::fs::remove_file(staging_path);
                    } else {
                        journal.safe_lock().recover_staging(staging_path, remote_path);
                    }
                }
                if let MutationOp::FinishChunked { tail_path, .. } = &entry.op {
                    let _ = std::fs::remove_file(tail_path);
                }
                let mut j = journal.safe_lock();
                j.add_conflict(kind);
                j.dequeue_front();
            }
            ReplayResult::Idempotent => {
                let mut j = journal.safe_lock();
                j.dequeue_front();
                if let Some(staging_path) = entry.op.staging_path() {
                    let _ = std::fs::remove_file(staging_path);
                }
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
                    let _ = ctx.backend.put_file_from_path(&conflict_name, staging_path, None);
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
                    let _ = ctx.backend.finish_chunked_upload(&session, &conflict_name, None);
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
        let tail = std::fs::read(tail_path)
            .map_err(|e| BackendWriteError::Server(0, format!("staging tail {}: {}", tail_path.display(), e)))?;
        if tail.len() as u64 != tail_len {
            return Err(BackendWriteError::Server(0, format!(
                "staging tail is {} bytes, expected {} — refusing to assemble a truncated file",
                tail.len(), tail_len,
            )));
        }
        crate::retry_chunk_write("final chunk upload", || backend.put_chunk(&session, next_index, tail.clone()))?;
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
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn recovered_keeps_the_newest_files_within_bounds() {
        let dir = temp_dir("recovered_prune");
        for i in 0..(RECOVERED_KEEP_FILES + 3) {
            fs::write(dir.join(format!("f{i}")), "x").unwrap();
            let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000 + i as u64);
            fs::File::options().write(true).open(dir.join(format!("f{i}"))).unwrap().set_modified(t).unwrap();
        }
        prune_recovered(&dir);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), RECOVERED_KEEP_FILES);
        assert!(!dir.join("f0").exists() && dir.join(format!("f{}", RECOVERED_KEEP_FILES + 2)).exists());
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
}
