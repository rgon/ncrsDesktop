use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
        /// The moved file's bytes once the source's upload has landed (under the source's
        /// name) and until this MOVE does: the destination is read from here meanwhile, and
        /// a source gone from the server is uploaded from here instead of lost.
        #[serde(default)]
        staging: Option<PathBuf>,
        /// The version of the file this replaces at `to` that the rename was based on (the
        /// one this mount last served there): the MOVE only overwrites that version, and
        /// a newer one makes the moved file a conflicted copy instead. None when `to` is
        /// new, or nothing is known of its version.
        #[serde(default)]
        if_match: Option<String>,
    },
    /// The end of a streamed upload whose earlier chunks are already on the server:
    /// PUT the kept head as chunk 0 and the tail from chunk `next_index` on, then
    /// assemble the session into `remote_path`.
    FinishChunked {
        remote_path: PathBuf,
        uploads_base: String,
        next_index: u64,
        bytes_confirmed: u64,
        total_len: u64,
        tail_path: PathBuf,
        if_match_etag: Option<String>,
        /// The staging file is laid out at the file's own offsets, sent chunks punched out.
        /// Entries queued by older versions lack it: their tail file starts at
        /// `bytes_confirmed`.
        #[serde(default)]
        sparse: bool,
        /// Length of the file's start kept in the staging file and not sent yet (chunk 0).
        /// Entries queued by older versions sent the head with the rest: 0.
        #[serde(default)]
        head: u64,
        /// The staging file still holds every byte of the file (chunks went up early but
        /// nothing was punched out): reads are served from it, and a session the server no
        /// longer has is uploaded whole from it instead of being lost.
        #[serde(default)]
        whole: bool,
    },
}

/// A queued upload's local bytes, for reads before it reaches the server.
#[derive(Debug, PartialEq)]
pub enum PendingUpload {
    /// A whole-file PUT: the staging file is the file.
    Staged(PathBuf),
    /// A streamed upload's finish: only the first `head` bytes and the tail from
    /// `held_from` are local; `sparse` says they sit at the file's own offsets rather
    /// than the tail starting the file at `held_from`.
    StreamTail { tail: PathBuf, held_from: u64, head: u64, sparse: bool },
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
    /// Re-pointed by retarget_upload: the live worker uploads to the op's current path,
    /// not the one it was started with.
    #[serde(skip)]
    pub retargeted: bool,
    /// The live worker has sent the request that names this upload's destination (see
    /// settle_upload): from here on a rename can only follow it with a MOVE.
    #[serde(skip)]
    pub dest_fixed: bool,
}

/// What a live upload does when it is about to name its destination (settle_upload).
#[derive(Debug, PartialEq)]
pub enum Settled {
    /// Send it to this path with this If-Match.
    Go(PathBuf, Option<String>),
    /// An earlier change to the destination has not reached the server yet.
    Wait,
    /// That earlier change is waiting for a replay; the upload has been put back for one too.
    Deferred,
    /// Superseded, or its file deleted or replaced since: nothing to send.
    Withdrawn,
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
}

pub type SharedJournal = Arc<Mutex<MutationJournal>>;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
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
            MutationOp::Rename { staging, .. } => staging.as_deref(),
            _ => None,
        }
    }

    /// Whether this op changes `path` or a folder above it, so an upload of `path`
    /// queued after it has to wait for it.
    fn touches(&self, path: &Path) -> bool {
        match self {
            MutationOp::Put { remote_path: p, .. } | MutationOp::FinishChunked { remote_path: p, .. }
            | MutationOp::Unlink { path: p } => p == path,
            MutationOp::MkDir { path: p } | MutationOp::RmDir { path: p } => path.starts_with(p),
            MutationOp::Rename { from, to, .. } => path.starts_with(from) || path.starts_with(to),
        }
    }

    /// Whether this op, queued after an upload of `path`, makes that upload pointless:
    /// a newer upload of it, its delete, or another file moved over it.
    fn replaces(&self, path: &Path) -> bool {
        match self {
            MutationOp::Put { remote_path: p, .. } | MutationOp::FinishChunked { remote_path: p, .. }
            | MutationOp::Unlink { path: p } => p == path,
            MutationOp::RmDir { path: p } => path.starts_with(p),
            MutationOp::Rename { to, .. } => path.starts_with(to),
            MutationOp::MkDir { .. } => false,
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
            MutationOp::Rename { from, to, .. } => {
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

        // Validate Put staging files exist. A MOVE still has to run without its copy.
        let mut orphaned = Vec::new();
        for entry in &mut entries {
            if let MutationOp::Rename { staging, .. } = &mut entry.op {
                if staging.as_ref().is_some_and(|s| !s.exists()) {
                    *staging = None;
                }
                continue;
            }
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

        // A live worker uploads under the name it was given, so its entry keeps that name
        // and this MOVE follows it; pending_upload() follows the MOVE to its bytes.
        // An upload moved onto the destination itself replaces the version the rename was
        // based on, so it is conditional on it like the MOVE would have been.
        if let MutationOp::Rename { ref from, ref to, ref if_match, .. } = op {
            for entry in self.entries.iter_mut().filter(|e| !e.in_flight && !e.retargeted) {
                entry.op.update_path_prefix(from, to);
                if let MutationOp::Put { remote_path, if_match_etag: cond @ None, .. }
                    | MutationOp::FinishChunked { remote_path, if_match_etag: cond @ None, .. } = &mut entry.op
                {
                    if remote_path == to {
                        cond.clone_from(if_match);
                    }
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
            in_flight: false,
            retargeted: false,
            dest_fixed: false,
        });
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

    /// Where a reader finds the newest not-yet-uploaded content of `path`, newest entry
    /// first: a queued whole-file PUT or streamed finish for it. A delete of the path (or
    /// of a folder above it) ends the search, since what is queued behind it belongs to a
    /// file that is gone, unless a rename onto the path was seen first: enqueue() re-points
    /// a moved file's queued uploads at the new name in place, so they sit behind such a
    /// delete in the queue (a save that deletes the document, then renames its temp file
    /// over it). An upload a live worker holds keeps its own name instead, so a rename onto
    /// the path also looks for the source's upload, and for the copy the source's finished
    /// upload left the MOVE. The one case left: a rename over a file whose own older upload
    /// is still queued can read that upload's bytes, as enqueue() no longer tells the two apart.
    pub fn pending_upload(&self, path: &Path) -> Option<PendingUpload> {
        let mut names = vec![path.to_path_buf()];
        let mut renamed_onto = false;
        for e in self.entries.iter().rev() {
            match &e.op {
                MutationOp::Put { remote_path, staging_path, .. } if names.contains(remote_path) => {
                    return Some(PendingUpload::Staged(staging_path.clone()));
                }
                MutationOp::FinishChunked { remote_path, tail_path, whole: true, .. } if names.contains(remote_path) => {
                    return Some(PendingUpload::Staged(tail_path.clone()));
                }
                MutationOp::FinishChunked { remote_path, tail_path, bytes_confirmed, sparse, head, .. } if names.contains(remote_path) => {
                    return Some(PendingUpload::StreamTail { tail: tail_path.clone(), held_from: *bytes_confirmed, head: *head, sparse: *sparse });
                }
                MutationOp::Rename { from, to, staging, .. } if path.starts_with(to) => {
                    if let Some(s) = staging.as_ref().filter(|_| path == to) {
                        return Some(PendingUpload::Staged(s.clone()));
                    }
                    names.push(from.join(path.strip_prefix(to).unwrap_or(Path::new(""))));
                    renamed_onto = true;
                }
                MutationOp::Unlink { path: gone } if gone == path && !renamed_onto => return None,
                MutationOp::RmDir { path: gone } if path.starts_with(gone) && !renamed_onto => return None,
                _ => {}
            }
        }
        None
    }

    /// Re-points the queued upload of a file the server has never seen (`from`, just renamed
    /// to `to`) at `to`, and moves it behind everything queued so far. Its upload then lands
    /// straight under the new name: no MOVE, so no moment where the server has the file only
    /// under the old one, or where a refresh shows `to`'s previous version. Only while no
    /// request has named `from` yet (settle_upload), i.e. while nothing can create it on the
    /// server. Returns whether it did; otherwise the rename needs its MOVE.
    ///
    /// `base` is the version of `to` the save replaces: the upload overwrites only that one.
    pub fn retarget_upload(&mut self, from: &Path, to: &Path, base: Option<String>) -> bool {
        let ups: Vec<usize> = self.entries.iter().enumerate()
            .filter(|(_, e)| e.op.is_upload_of(from))
            .map(|(i, _)| i)
            .collect();
        let [i] = ups[..] else { return false };
        let e = &self.entries[i];
        if e.dest_fixed || !matches!(e.op, MutationOp::Put { if_match_etag: None, .. }) {
            return false;
        }
        // Anything else queued for the old name (a delete, a move) would need it on the server.
        if self.entries.iter().enumerate().any(|(k, e)| k != i && e.op.touches(from)) {
            return false;
        }
        let Some(mut e) = self.entries.remove(i) else { return false };
        if let MutationOp::Put { remote_path, if_match_etag, .. } = &mut e.op {
            *remote_path = to.to_path_buf();
            *if_match_etag = base;
        }
        e.retargeted = true;
        self.entries.push_back(e);
        self.save_journal();
        true
    }

    /// Called by a live upload right before the request that names its destination. A
    /// retargeted upload goes to its current path, after every earlier change to that path
    /// and only while nothing later replaces it; any other upload keeps the path it started
    /// with (`started_as`), whatever a rename of a folder above it rewrote in the entry.
    pub fn settle_upload(&mut self, seq: SeqId, started_as: &Path, if_match: Option<String>) -> Settled {
        let Some(i) = self.entries.iter().position(|e| e.seq == seq) else { return Settled::Withdrawn };
        if !self.entries[i].retargeted {
            self.entries[i].dest_fixed = true;
            return Settled::Go(started_as.to_path_buf(), if_match);
        }
        let dest = self.entries[i].op.path().to_path_buf();
        if self.entries.iter().skip(i + 1).any(|e| e.op.replaces(&dest)) {
            let gone = self.entries.remove(i);
            if let Some(sp) = gone.as_ref().and_then(|e| e.op.staging_path()) {
                let _ = std::fs::remove_file(sp);
            }
            self.save_journal();
            return Settled::Withdrawn;
        }
        if let Some(before) = self.entries.iter().take(i).find(|e| e.op.touches(&dest)) {
            if before.in_flight || before.last_error.is_none() {
                return Settled::Wait;
            }
            self.mark_deferred(seq, format!("waiting for an earlier change to {} to reach the server", dest.display()));
            return Settled::Deferred;
        }
        let e = &mut self.entries[i];
        e.dest_fixed = true;
        let etag = match &e.op {
            MutationOp::Put { if_match_etag, .. } => if_match_etag.clone(),
            _ => None,
        };
        Settled::Go(dest, etag)
    }

    /// Gives a finished upload of `uploaded_as` to the MOVE queued after it, if any, as the
    /// destination's copy until that MOVE lands (see MutationOp::Rename). Returns whether it
    /// did, in which case the caller must keep `staging`.
    pub fn hand_staging_to_rename(&mut self, uploaded_as: &Path, staging: &Path) -> bool {
        let Some(slot) = self.entries.iter_mut().find_map(|e| match &mut e.op {
            MutationOp::Rename { from, staging: slot @ None, .. } if from == uploaded_as => Some(slot),
            _ => None,
        }) else {
            return false;
        };
        *slot = Some(staging.to_path_buf());
        self.save_journal();
        true
    }

    /// The version of `path` the newest queued change to it was based on, when one is
    /// queued: a save replacing `path` before that change lands is based on the same
    /// version (rebase() moves it on once the change lands). Some(None) when that version
    /// is unknown, or the change replaces `path` with another file.
    pub fn pending_base(&self, path: &Path) -> Option<Option<String>> {
        self.entries.iter().rev().find_map(|e| match &e.op {
            MutationOp::Put { remote_path, if_match_etag, .. } | MutationOp::FinishChunked { remote_path, if_match_etag, .. }
                if remote_path == path => Some(if_match_etag.clone()),
            op if op.replaces(path) => Some(None),
            _ => None,
        })
    }

    /// `path` went from version `sent` to `new` on the server through one of our changes:
    /// the changes queued after it that were based on `sent` are based on `new` now.
    pub fn rebase(&mut self, path: &Path, sent: Option<&str>, new: Option<&str>) {
        let Some(sent) = sent else { return };
        let mut changed = false;
        for e in self.entries.iter_mut() {
            let cond = match &mut e.op {
                MutationOp::Put { remote_path, if_match_etag, .. } | MutationOp::FinishChunked { remote_path, if_match_etag, .. }
                    if remote_path == path => if_match_etag,
                MutationOp::Rename { to, if_match, .. } if to == path => if_match,
                _ => continue,
            };
            if cond.as_deref() == Some(sent) {
                *cond = new.map(str::to_string);
                changed = true;
            }
        }
        if changed {
            self.save_journal();
        }
    }

    /// The copy a queued MOVE holds of its file (see hand_staging_to_rename).
    pub fn rename_staging(&self, seq: SeqId) -> Option<PathBuf> {
        self.entries.iter().find(|e| e.seq == seq).and_then(|e| match &e.op {
            MutationOp::Rename { staging, .. } => staging.clone(),
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
            entry.dest_fixed = false;
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
            entry.dest_fixed = false;
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

    fn save_journal(&self) {
        self.dirty_version.fetch_add(1, Ordering::Relaxed);
        let list: Vec<&JournalEntry> = self.entries.iter().collect();
        match serde_json::to_vec(&list) {
            Ok(data) => {
                if let Err(e) = write_atomic_durable(&self.journal_path, &data) {
                    log::error!("JOURNAL: durable write failed: {}", e);
                }
            }
            Err(e) => log::error!("JOURNAL: serialize failed: {}", e),
        }
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
            // A streamed tail is not the whole file, so it is not worth preserving; a
            // staging file that still holds every byte is.
            let recovered = match &entry.op {
                MutationOp::FinishChunked { uploads_base, tail_path, whole, remote_path, .. } => {
                    ctx.backend.abort_chunked_upload(&crate::backend::ChunkedUploadSession { uploads_base: uploads_base.clone() });
                    if *whole {
                        journal.safe_lock().recover_staging(tail_path, remote_path)
                    } else {
                        let _ = std::fs::remove_file(tail_path);
                        None
                    }
                }
                _ => recovered,
            };
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

        let result = execute_op(&entry, ctx, cache, dirty, error_log);
        // What was queued behind it on the version it replaced is on its version now.
        let landed = match (&result, &entry.op) {
            (ReplayResult::Uploaded(new), MutationOp::Put { remote_path, if_match_etag, .. } | MutationOp::FinishChunked { remote_path, if_match_etag, .. }) => {
                Some((remote_path, if_match_etag, new.clone()))
            }
            (ReplayResult::Ok, MutationOp::Rename { to, if_match, .. }) => Some((to, if_match, None)),
            _ => None,
        };
        if let Some((path, sent, new)) = landed {
            journal.safe_lock().rebase(path, sent.as_deref(), new.as_deref());
        }
        match result {
            ReplayResult::Ok | ReplayResult::Uploaded(_) => {
                let mut j = journal.safe_lock();
                j.dequeue_front();
                release_upload_guard(&j, cache, &entry.op);
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
                // A MOVE's copy only stood in for bytes the server already has under the source.
                if let MutationOp::FinishChunked { tail_path, remote_path, whole: true, .. } = &entry.op {
                    if matches!(kind, ConflictKind::EditConflict { .. }) {
                        let _ = std::fs::remove_file(tail_path);
                    } else {
                        journal.safe_lock().recover_staging(tail_path, remote_path);
                    }
                } else if let MutationOp::FinishChunked { tail_path: sp, .. } | MutationOp::Rename { staging: Some(sp), .. } = &entry.op {
                    let _ = std::fs::remove_file(sp);
                }
                let mut j = journal.safe_lock();
                j.add_conflict(kind);
                j.dequeue_front();
                release_upload_guard(&j, cache, &entry.op);
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

/// Drops the listing guard (FsCache::uploading) a replayed upload or MOVE leaves behind,
/// unless another queued upload of the same file still needs it: replay is the only thing
/// that finishes an upload queued offline, and a stale guard pins the file's listing.
fn release_upload_guard(j: &MutationJournal, cache: &Arc<Mutex<crate::FsCache>>, op: &MutationOp) {
    use crate::MutexExt;
    let path = match op {
        MutationOp::Put { remote_path, .. } | MutationOp::FinishChunked { remote_path, .. } => remote_path,
        MutationOp::Rename { to, .. } => to,
        _ => return,
    };
    if j.pending_upload(path).is_none() {
        cache.safe_lock().uploading.remove(path);
    }
}

/// How a queued MOVE ended (see send_move).
pub(crate) enum MoveOutcome {
    Moved,
    /// The destination was newer than the version the rename replaced: the moved file
    /// went up under this conflicted-copy name, and the destination kept the server's.
    ConflictCopy(PathBuf),
    /// Neither the source nor a local copy of it is left.
    SourceGone,
    Failed(crate::backend::BackendWriteError),
}

/// Sends a queued MOVE of `from` onto `to`, overwriting only the version `if_match`
/// names. When the source's upload landed but the source is gone again, its local copy
/// (`staging`) goes to the destination instead, under the same condition.
pub(crate) fn send_move(
    backend: &dyn crate::backend::CloudBackend,
    from: &Path,
    to: &Path,
    staging: Option<&Path>,
    if_match: Option<&str>,
) -> MoveOutcome {
    use crate::backend::BackendWriteError;
    let staging = staging.filter(|s| s.exists());
    let (sent, from_copy) = match (backend.rename(from, to, if_match), staging) {
        (Err(BackendWriteError::Server(404, _)), Some(s)) => {
            log::warn!("MOVE {} → {}: source gone on the server — uploading the destination from the local copy", from.display(), to.display());
            (backend.put_file_from_path(to, s, if_match).map(|_| ()), true)
        }
        (r, _) => (r, false),
    };
    match sent {
        Ok(()) => MoveOutcome::Moved,
        Err(BackendWriteError::Conflict) => {
            let copy = crate::make_conflict_name(to);
            let saved = match (from_copy, staging) {
                (true, Some(s)) => backend.put_file_from_path(&copy, s, None).map(|_| ()),
                _ => match (backend.rename(from, &copy, None), staging) {
                    (Err(BackendWriteError::Server(404, _)), Some(s)) => backend.put_file_from_path(&copy, s, None).map(|_| ()),
                    (r, _) => r,
                },
            };
            match saved {
                Ok(()) => MoveOutcome::ConflictCopy(copy),
                Err(BackendWriteError::Server(404, _)) => MoveOutcome::SourceGone,
                Err(e) => MoveOutcome::Failed(e),
            }
        }
        Err(BackendWriteError::Server(404, _)) => MoveOutcome::SourceGone,
        Err(e) => MoveOutcome::Failed(e),
    }
}

enum ReplayResult {
    Ok,
    /// An upload landed, making the file this version.
    Uploaded(Option<String>),
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
            let saved_mtime = crate::staged_mtime(staging_path).unwrap_or_else(SystemTime::now);
            match ctx.backend.put_file_from_path(remote_path, staging_path, etag_ref) {
                Ok(result) => {
                    log::info!("JOURNAL replay: PUT {} → token {:?}", remote_path.display(), result.new_change_token);
                    let new_etag = result.new_change_token.clone();
                    let mut c = cache.safe_lock();
                    let parent = remote_path.parent().unwrap_or(Path::new("/")).to_path_buf();
                    if let Some(dir) = c.dir_cache.get_mut(&parent) {
                        let mut files = (*dir.files).clone();
                        if let Some(e) = files.iter_mut().find(|e| e.path == *remote_path) {
                            e.change_token = result.new_change_token;
                            e.size = file_size;
                            e.modified = Some(saved_mtime);
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
                    ReplayResult::Uploaded(new_etag)
                }
                Err(BackendWriteError::Conflict) => {
                    log::warn!("JOURNAL replay: PUT {} conflict — creating conflicted copy", remote_path.display());
                    let conflict_name = crate::make_conflict_name(remote_path);
                    // The staging file is deleted only once the copy is on the server.
                    match ctx.backend.put_file_from_path(&conflict_name, staging_path, None) {
                        Ok(_) => {
                            crate::push_error(error_log, remote_path.clone(), crate::SyncErrorKind::Conflict, "Server version changed — conflicted copy created".into());
                            ReplayResult::Conflict(ConflictKind::EditConflict {
                                local_path: remote_path.clone(),
                                conflicted_copy_path: conflict_name,
                            })
                        }
                        Err(e) if e.is_transient() => ReplayResult::Retryable(format!("conflicted copy {} not uploaded: {}", conflict_name.display(), e)),
                        Err(e) => ReplayResult::ServerError(format!("conflicted copy {} not uploaded: {}", conflict_name.display(), e)),
                    }
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
        MutationOp::Rename { from, to, staging, if_match } => {
            match send_move(&*ctx.backend, from, to, staging.as_deref(), if_match.as_deref()) {
                MoveOutcome::Moved => {
                    log::info!("JOURNAL replay: MOVE {} → {}", from.display(), to.display());
                    dirty.safe_lock().insert(to.parent().unwrap_or(Path::new("/")).to_path_buf());
                    ReplayResult::Ok
                }
                MoveOutcome::ConflictCopy(copy) => {
                    log::warn!("JOURNAL replay: MOVE {} → {}: {} changed on the server — moved to {}", from.display(), to.display(), to.display(), copy.display());
                    crate::push_error(error_log, to.clone(), crate::SyncErrorKind::Conflict, "Server version changed — conflicted copy created".into());
                    dirty.safe_lock().insert(to.parent().unwrap_or(Path::new("/")).to_path_buf());
                    ReplayResult::Conflict(ConflictKind::EditConflict {
                        local_path: to.clone(),
                        conflicted_copy_path: copy,
                    })
                }
                MoveOutcome::SourceGone => {
                    ReplayResult::Conflict(ConflictKind::MoveSourceGone {
                        from: from.clone(),
                        to: to.clone(),
                    })
                }
                MoveOutcome::Failed(e) if e.is_transient() => ReplayResult::Retryable(e.to_string()),
                MoveOutcome::Failed(e) => ReplayResult::ServerError(e.to_string()),
            }
        }
        MutationOp::FinishChunked { remote_path, uploads_base, next_index, bytes_confirmed, total_len, tail_path, if_match_etag, sparse, head, whole } => {
            let saved_mtime = crate::staged_mtime(tail_path).unwrap_or_else(SystemTime::now);
            let result = finish_chunked(
                &*ctx.backend, uploads_base, *next_index, *bytes_confirmed, *total_len,
                tail_path, *sparse, *head, *whole, remote_path, if_match_etag.as_deref(),
            );
            match result {
                Ok(result) => {
                    log::info!("JOURNAL replay: finished streamed upload {} → token {:?}", remote_path.display(), result.new_change_token);
                    let new_etag = result.new_change_token.clone();
                    let parent = remote_path.parent().unwrap_or(Path::new("/")).to_path_buf();
                    {
                        let mut c = cache.safe_lock();
                        if let Some(dir) = c.dir_cache.get_mut(&parent) {
                            let mut files = (*dir.files).clone();
                            if let Some(e) = files.iter_mut().find(|e| e.path == *remote_path) {
                                e.change_token = result.new_change_token;
                                e.size = *total_len;
                                e.modified = Some(saved_mtime);
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
                    ReplayResult::Uploaded(new_etag)
                }
                Err(BackendWriteError::Conflict) => {
                    // Changed on the server meanwhile: every chunk is already there, so
                    // assemble ours as a conflicted copy instead.
                    log::warn!("JOURNAL replay: streamed upload {} conflict — assembling a conflicted copy", remote_path.display());
                    let conflict_name = crate::make_conflict_name(remote_path);
                    match assemble_conflicted_copy(&*ctx.backend, uploads_base, &conflict_name, *total_len, tail_path, *whole) {
                        Ok(()) => {
                            crate::push_error(error_log, remote_path.clone(), crate::SyncErrorKind::Conflict, "Server version changed — conflicted copy created".into());
                            ReplayResult::Conflict(ConflictKind::EditConflict {
                                local_path: remote_path.clone(),
                                conflicted_copy_path: conflict_name,
                            })
                        }
                        Err(e) if e.is_transient() => ReplayResult::Retryable(format!("conflicted copy {} not assembled: {}", conflict_name.display(), e)),
                        Err(e) => ReplayResult::ServerError(format!("conflicted copy {} not assembled: {}", conflict_name.display(), e)),
                    }
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

/// Reads `tail_len` bytes of the staging file from offset `from` in `chunk`-sized pieces
/// (the last may be shorter) and hands each to `put` with its index from 0, so a tail of
/// any length is never in memory whole. The file must be exactly `from + tail_len` long.
fn each_tail_chunk<E>(
    tail_path: &Path,
    from: u64,
    tail_len: u64,
    chunk: usize,
    local: impl Fn(String) -> E,
    mut put: impl FnMut(u64, Vec<u8>) -> Result<(), E>,
) -> Result<(), E> {
    use std::os::unix::fs::FileExt;
    let fault = |e: std::io::Error| local(format!("staging tail {}: {}", tail_path.display(), e));
    let f = std::fs::File::open(tail_path).map_err(fault)?;
    let len = f.metadata().map_err(fault)?.len();
    if len != from + tail_len {
        return Err(local(format!(
            "staging file is {} bytes, expected {} — refusing to assemble a truncated file",
            len, from + tail_len,
        )));
    }
    let (mut off, mut index) = (from, 0u64);
    while off < from + tail_len {
        let mut body = vec![0u8; (chunk as u64).min(from + tail_len - off) as usize];
        f.read_exact_at(&mut body, off).map_err(fault)?;
        off += body.len() as u64;
        put(index, body)?;
        index += 1;
    }
    Ok(())
}

/// Assembles a streamed upload's chunks as `conflict_name`, the conflicted copy of a file
/// that changed on the server meanwhile; from a staging file that holds the whole file,
/// uploaded whole when the session is gone.
pub(crate) fn assemble_conflicted_copy(
    backend: &dyn crate::backend::CloudBackend,
    uploads_base: &str,
    conflict_name: &Path,
    total_len: u64,
    tail_path: &Path,
    whole: bool,
) -> Result<(), crate::backend::BackendWriteError> {
    let session = crate::backend::ChunkedUploadSession { uploads_base: uploads_base.to_string() };
    match backend.finish_chunked_upload(&session, conflict_name, None, None, total_len) {
        Err(e) if whole && session_gone(&e) => {
            log::warn!("conflicted copy {}: upload session gone ({}) — uploading it whole", conflict_name.display(), e);
            backend.put_file_settled(tail_path, &mut || Some((conflict_name.to_path_buf(), None))).map(|_| ())
        }
        r => r.map(|_| ()),
    }
}

/// Whether a chunked-upload request failed because the server no longer has the session:
/// its chunks expired (Nextcloud collects unfinished uploads after a day) or were assembled.
fn session_gone(e: &crate::backend::BackendWriteError) -> bool {
    matches!(e, crate::backend::BackendWriteError::Server(404 | 409, _))
}

/// Whether the file the server lists is this upload's assembly: the size it was given and
/// the mtime it was stamped with (X-OC-Mtime, whole seconds), and no longer the version the
/// assembly was conditioned on. A MOVE whose answer was lost (a timeout, a dropped
/// connection) lands all the same, and its retry then finds the session gone (404) or the
/// destination changed (412): this tells that apart from a real failure or conflict.
fn is_our_assembly(
    listed: &crate::backend::RemoteEntry,
    total_len: u64,
    mtime: Option<SystemTime>,
    if_match: Option<&str>,
) -> bool {
    let secs = |t: SystemTime| t.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs());
    // Without an mtime of ours nothing tells this upload from another file of that size.
    let Some(ours) = mtime.and_then(secs) else { return false };
    !listed.is_dir
        && listed.size == total_len
        && listed.modified.and_then(secs) == Some(ours)
        && (if_match.is_none() || listed.change_token.as_deref().map(|t| t.trim_matches('"')) != if_match.map(|t| t.trim_matches('"')))
}

/// Uploads the tail of a streamed upload as its last chunks and assembles the session.
/// Re-running it is safe: a chunk PUT to the same index replaces the earlier one. With
/// `whole` (the staging file holds every byte), a session the server no longer has is
/// replaced by an ordinary upload of the whole staging file.
pub(crate) fn finish_chunked(
    backend: &dyn crate::backend::CloudBackend,
    uploads_base: &str,
    next_index: u64,
    bytes_confirmed: u64,
    total_len: u64,
    tail_path: &Path,
    sparse: bool,
    head: u64,
    whole: bool,
    remote_path: &Path,
    if_match: Option<&str>,
) -> Result<crate::backend::PutResult, crate::backend::BackendWriteError> {
    let mtime = crate::staged_mtime(tail_path);
    let result = send_chunked_finish(backend, uploads_base, next_index, bytes_confirmed, total_len, tail_path, sparse, head, remote_path, if_match, mtime);
    match result {
        Err(e) if matches!(e, crate::backend::BackendWriteError::Conflict | crate::backend::BackendWriteError::Server(404, _)) => {
            // An earlier attempt (this call's, or a worker's before a replay) may have landed.
            let listed = backend.list_dir(remote_path, Duration::from_secs(30)).ok().and_then(|(_, me, _)| me);
            if let Some(listed) = listed.filter(|l| is_our_assembly(l, total_len, mtime, if_match)) {
                log::info!("streamed upload {}: the server already has it ({}) — an earlier assembly landed", remote_path.display(), e);
                return Ok(crate::backend::PutResult { new_change_token: listed.change_token });
            }
            if whole && session_gone(&e) {
                return put_whole_instead(backend, tail_path, remote_path, if_match, &e);
            }
            Err(e)
        }
        Err(e) if whole && session_gone(&e) => put_whole_instead(backend, tail_path, remote_path, if_match, &e),
        r => r,
    }
}

fn put_whole_instead(
    backend: &dyn crate::backend::CloudBackend,
    tail_path: &Path,
    remote_path: &Path,
    if_match: Option<&str>,
    why: &crate::backend::BackendWriteError,
) -> Result<crate::backend::PutResult, crate::backend::BackendWriteError> {
    log::warn!("streamed upload {}: upload session gone ({}) — uploading the whole file from its staging copy", remote_path.display(), why);
    backend.put_file_settled(tail_path, &mut || Some((remote_path.to_path_buf(), if_match.map(str::to_string))))
}

#[allow(clippy::too_many_arguments)]
fn send_chunked_finish(
    backend: &dyn crate::backend::CloudBackend,
    uploads_base: &str,
    next_index: u64,
    bytes_confirmed: u64,
    total_len: u64,
    tail_path: &Path,
    sparse: bool,
    head: u64,
    remote_path: &Path,
    if_match: Option<&str>,
    // Read before the final chunk: the tail's mtime is the save's, and it is the one the
    // commit already reported for the file.
    mtime: Option<SystemTime>,
) -> Result<crate::backend::PutResult, crate::backend::BackendWriteError> {
    use crate::backend::BackendWriteError;
    let session = crate::backend::ChunkedUploadSession { uploads_base: uploads_base.to_string() };
    let tail_len = total_len.saturating_sub(bytes_confirmed);
    if tail_len > 0 {
        // Server(0, _): a local fault, never transient, so it is not retried forever.
        let from = if sparse { bytes_confirmed } else { 0 };
        each_tail_chunk(tail_path, from, tail_len, crate::webdav_ops::CHUNK_SIZE, |m| BackendWriteError::Server(0, m), |i, body| {
            crate::retry_chunk_write("final chunk upload", || backend.put_chunk(&session, next_index + i, body.clone()))
        })?;
    }
    // The head was kept for header rewrites until the file was closed: it goes now, as
    // chunk 0, which the server assembles first. After the tail, whose length check
    // catches a truncated staging file before anything is sent.
    if head > 0 {
        use std::os::unix::fs::FileExt;
        let mut body = vec![0u8; head.min(total_len) as usize];
        std::fs::File::open(tail_path)
            .and_then(|f| f.read_exact_at(&mut body, 0))
            .map_err(|e| BackendWriteError::Server(0, format!("staging head {}: {}", tail_path.display(), e)))?;
        crate::retry_chunk_write("head chunk upload", || backend.put_chunk(&session, 0, body.clone()))?;
    }
    crate::retry_chunk_write("chunked-upload finish", || backend.finish_chunked_upload(&session, remote_path, if_match, mtime, total_len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn a_tail_is_uploaded_in_chunk_sized_pieces_read_one_at_a_time() {
        let dir = temp_dir("tail-chunks");
        let tail = dir.join("write_1");
        let data: Vec<u8> = (0..25u8).collect();
        fs::write(&tail, &data).unwrap();
        let mut got = Vec::new();
        each_tail_chunk(&tail, 0, 25, 10, |m| m, |i, body| {
            got.push((i, body));
            Ok::<(), String>(())
        }).unwrap();
        assert_eq!(got, vec![(0, data[..10].to_vec()), (1, data[10..20].to_vec()), (2, data[20..].to_vec())]);
        // A sparse staging file: the tail starts at its own offset in the file.
        got.clear();
        each_tail_chunk(&tail, 12, 13, 10, |m| m, |i, body| {
            got.push((i, body));
            Ok::<(), String>(())
        }).unwrap();
        assert_eq!(got, vec![(0, data[12..22].to_vec()), (1, data[22..].to_vec())]);
        // A tail shorter than the journal says is never assembled.
        let err = each_tail_chunk(&tail, 0, 30, 10, |m| m, |_, _| Ok::<(), String>(())).unwrap_err();
        assert!(err.contains("refusing to assemble"), "{}", err);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lost_assembly_answer_is_recognised_by_size_and_mtime() {
        let t = |s: u64| std::time::UNIX_EPOCH + Duration::from_secs(s);
        let listed = |size: u64, secs: u64, etag: &str| crate::backend::RemoteEntry {
            path: PathBuf::from("/big.bin"),
            is_dir: false,
            size,
            modified: Some(t(secs)),
            change_token: Some(etag.into()),
            content_type: None,
            ext: Default::default(),
        };
        // Our size and stamped mtime, and not the version the MOVE was conditioned on.
        assert!(is_our_assembly(&listed(500, 1_000, "new"), 500, Some(t(1_000)), Some("old")));
        assert!(is_our_assembly(&listed(500, 1_000, "new"), 500, Some(t(1_000)), None));
        // Still the version it replaces: a real conflict or a MOVE that never ran.
        assert!(!is_our_assembly(&listed(500, 1_000, "old"), 500, Some(t(1_000)), Some("\"old\"")));
        // Someone else's file: another size or another mtime.
        assert!(!is_our_assembly(&listed(499, 1_000, "new"), 500, Some(t(1_000)), Some("old")));
        assert!(!is_our_assembly(&listed(500, 1_001, "new"), 500, Some(t(1_000)), Some("old")));
        // Nothing of ours to compare with.
        assert!(!is_our_assembly(&listed(500, 1_000, "new"), 500, None, None));
    }

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
            staging: None,
            if_match: None,
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
    fn pending_upload_follows_rename() {
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
        assert_eq!(j.pending_upload(Path::new("/docs/lu123.tmp")), Some(PendingUpload::Staged(staging.clone())));

        // Some saves delete the document before renaming the temp file over it.
        j.enqueue(MutationOp::Unlink { path: PathBuf::from("/docs/report.odt") });
        j.enqueue(MutationOp::Rename {
            from: PathBuf::from("/docs/lu123.tmp"),
            to: PathBuf::from("/docs/report.odt"),
            staging: None,
            if_match: None,
        });
        // Old path no longer resolves; destination now serves the staging bytes.
        assert!(j.pending_upload(Path::new("/docs/lu123.tmp")).is_none());
        assert_eq!(j.pending_upload(Path::new("/docs/report.odt")), Some(PendingUpload::Staged(staging)));
        assert!(j.has_pending_put(&PathBuf::from("/docs/report.odt")));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn retarget_sends_a_new_files_upload_to_its_new_name() {
        // The LibreOffice save once the temp file is closed: its upload is queued (or its
        // chunks are going up) when the temp file is renamed over the document.
        let dir = temp_dir("retarget_new_file");
        let staging = dir.join("staging_lo");
        fs::write(&staging, b"odf-bytes").unwrap();
        let (tmp, doc) = (PathBuf::from("/docs/lu123.tmp"), PathBuf::from("/docs/report.odt"));

        let mut j = MutationJournal::load_or_create(&dir);
        let seq = j.enqueue(MutationOp::Put { remote_path: tmp.clone(), staging_path: staging.clone(), if_match_etag: None });
        // Some saves delete the document before renaming the temp file over it.
        j.enqueue(MutationOp::Unlink { path: doc.clone() });
        assert!(j.claim(seq), "the live worker has it, but has not named a destination yet");
        assert!(j.retarget_upload(&tmp, &doc, None));

        assert!(matches!(&j.entries()[0].op, MutationOp::Unlink { .. }), "the upload goes behind the delete");
        assert!(matches!(&j.entries()[1].op, MutationOp::Put { remote_path, .. } if *remote_path == doc));
        assert_eq!(j.pending_upload(&doc), Some(PendingUpload::Staged(staging.clone())));
        assert!(j.pending_upload(&tmp).is_none());

        // The worker waits for the delete, then sends it to the document.
        assert_eq!(j.settle_upload(seq, &tmp, None), Settled::Wait);
        let del = j.entries()[0].seq;
        j.remove(del);
        assert_eq!(j.settle_upload(seq, &tmp, None), Settled::Go(doc.clone(), None));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn retarget_refused_once_the_upload_names_its_source() {
        let dir = temp_dir("retarget_refused");
        let staging = dir.join("staging_a");
        fs::write(&staging, b"a").unwrap();
        let (tmp, doc) = (PathBuf::from("/a.tmp"), PathBuf::from("/a.odt"));

        let mut j = MutationJournal::load_or_create(&dir);
        let seq = j.enqueue(MutationOp::Put { remote_path: tmp.clone(), staging_path: staging.clone(), if_match_etag: None });
        assert!(j.claim(seq));
        assert_eq!(j.settle_upload(seq, &tmp, None), Settled::Go(tmp.clone(), None));
        assert!(!j.retarget_upload(&tmp, &doc, None), "the PUT naming the temp file is on the wire: only a MOVE can follow it");

        // An edit of a file the server has is not a new file: its rename stays a MOVE.
        j.remove(seq);
        j.enqueue(MutationOp::Put { remote_path: tmp.clone(), staging_path: staging, if_match_etag: Some("e1".into()) });
        assert!(!j.retarget_upload(&tmp, &doc, None));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn retargeted_upload_withdrawn_when_something_later_replaces_it() {
        let dir = temp_dir("retarget_withdrawn");
        let staging = dir.join("staging_old");
        let newer = dir.join("staging_new");
        fs::write(&staging, b"old").unwrap();
        fs::write(&newer, b"new").unwrap();
        let (tmp, doc) = (PathBuf::from("/b.tmp"), PathBuf::from("/b.odt"));

        let mut j = MutationJournal::load_or_create(&dir);
        let seq = j.enqueue(MutationOp::Put { remote_path: tmp.clone(), staging_path: staging.clone(), if_match_etag: None });
        assert!(j.claim(seq), "its chunks are going up, so supersede_uploads cannot drop it");
        assert!(j.retarget_upload(&tmp, &doc, None));
        // The next save lands before the first one's chunks are all up.
        let next = j.enqueue(MutationOp::Put { remote_path: doc.clone(), staging_path: newer.clone(), if_match_etag: None });
        j.supersede_uploads(&doc, next);

        assert_eq!(j.settle_upload(seq, &tmp, None), Settled::Withdrawn);
        assert!(!j.contains(seq));
        assert!(!staging.exists(), "the withdrawn upload's bytes go");
        assert_eq!(j.pending_upload(&doc), Some(PendingUpload::Staged(newer)));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_after_an_upload_on_the_wire_keeps_the_source_readable_as_its_destination() {
        // The temp file's PUT already named it when the rename came: a MOVE follows it,
        // and until that lands the document is read from the PUT's bytes.
        let dir = temp_dir("move_follows_upload");
        let staging = dir.join("staging_c");
        fs::write(&staging, b"c").unwrap();
        let (tmp, doc) = (PathBuf::from("/c.tmp"), PathBuf::from("/c.odt"));

        let mut j = MutationJournal::load_or_create(&dir);
        let put = j.enqueue(MutationOp::Put { remote_path: tmp.clone(), staging_path: staging.clone(), if_match_etag: None });
        assert!(j.claim(put));
        assert_eq!(j.settle_upload(put, &tmp, None), Settled::Go(tmp.clone(), None));
        let mv = j.enqueue(MutationOp::Rename { from: tmp.clone(), to: doc.clone(), staging: None, if_match: None });

        assert!(j.has_pending_put(&tmp), "the upload keeps its name, so the MOVE waits for it");
        assert_eq!(j.pending_upload(&doc), Some(PendingUpload::Staged(staging.clone())));

        // The PUT lands: its bytes stay the document's until the MOVE does.
        assert!(j.hand_staging_to_rename(&tmp, &staging));
        j.remove(put);
        assert_eq!(j.pending_upload(&doc), Some(PendingUpload::Staged(staging.clone())));
        assert_eq!(j.rename_staging(mv), Some(staging.clone()));

        // A restart keeps the MOVE, even if its copy is gone by then.
        fs::remove_file(&staging).unwrap();
        let j = MutationJournal::load_or_create(&dir);
        assert!(j.contains(mv), "the MOVE still has to run");
        assert_eq!(j.rename_staging(mv), None);

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
            retargeted: false,
            dest_fixed: false,
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
                sparse: false,
                head: 0,
                whole: false,
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
    fn a_queued_streamed_finish_tells_reads_where_its_tail_is() {
        let dir = temp_dir("pending_stream_finish");
        let tail = dir.join("write_3");
        fs::write(&tail, b"x").unwrap();
        let mut j = MutationJournal::load_or_create(&dir);
        assert_eq!(j.pending_upload(Path::new("/v.mkv")), None);
        for (sparse, confirmed) in [(false, 10), (true, 20)] {
            j.enqueue(MutationOp::FinishChunked {
                remote_path: PathBuf::from("/v.mkv"),
                uploads_base: "u".into(),
                next_index: 1,
                bytes_confirmed: confirmed,
                total_len: 30,
                tail_path: tail.clone(),
                if_match_etag: None,
                sparse,
                head: if sparse { 10 } else { 0 },
                whole: false,
            });
        }
        assert_eq!(
            j.pending_upload(Path::new("/v.mkv")),
            Some(PendingUpload::StreamTail { tail: tail.clone(), held_from: 20, head: 10, sparse: true }),
            "the newest finish",
        );
        // One whose staging file kept every byte is read like a whole-file save.
        let full = dir.join("write_5");
        j.enqueue(MutationOp::FinishChunked {
            remote_path: PathBuf::from("/v.mkv"),
            uploads_base: "u".into(),
            next_index: 3,
            bytes_confirmed: 20,
            total_len: 30,
            tail_path: full.clone(),
            if_match_etag: None,
            sparse: true,
            head: 10,
            whole: true,
        });
        assert_eq!(j.pending_upload(Path::new("/v.mkv")), Some(PendingUpload::Staged(full)));
        // A newer whole-file save of the same path wins over the streamed finish...
        let staged = dir.join("write_4");
        j.enqueue(MutationOp::Put { remote_path: PathBuf::from("/v.mkv"), staging_path: staged.clone(), if_match_etag: Some("e".into()) });
        assert_eq!(j.pending_upload(Path::new("/v.mkv")), Some(PendingUpload::Staged(staged)));
        // ...and once the file is deleted, nothing queued for it is its content (a file
        // created there next is not that one), even while those uploads stay queued.
        j.enqueue(MutationOp::Unlink { path: PathBuf::from("/v.mkv") });
        assert_eq!(j.pending_upload(Path::new("/v.mkv")), None);

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
                sparse: false,
                head: 0,
                whole: false,
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
            sparse: false,
            head: 0,
            whole: false,
        });
        j.enqueue(MutationOp::Rename { from: PathBuf::from("/d"), to: PathBuf::from("/e"), staging: None, if_match: None });
        assert_eq!(j.entries()[0].op.path(), Path::new("/e/tmp.bin"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_save_over_a_document_is_conditional_on_the_version_it_replaces() {
        // LibreOffice's save: the temp file is renamed over the document it opened at e1.
        let dir = temp_dir("retarget_base");
        let staging = dir.join("staging_lo");
        fs::write(&staging, b"odf").unwrap();
        let (tmp, doc) = (PathBuf::from("/docs/lu1.tmp"), PathBuf::from("/docs/report.odt"));
        let mut j = MutationJournal::load_or_create(&dir);
        let seq = j.enqueue(MutationOp::Put { remote_path: tmp.clone(), staging_path: staging, if_match_etag: None });
        assert!(j.retarget_upload(&tmp, &doc, Some("e1".into())));
        assert!(j.claim(seq));
        assert_eq!(j.settle_upload(seq, &tmp, None), Settled::Go(doc.clone(), Some("e1".into())),
            "it overwrites the document only while the server still has the version it was based on");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn back_to_back_saves_chain_onto_the_version_the_first_one_made() {
        let dir = temp_dir("rebase_chain");
        let (a, b) = (dir.join("a"), dir.join("b"));
        fs::write(&a, b"1").unwrap();
        fs::write(&b, b"2").unwrap();
        let doc = PathBuf::from("/report.odt");
        let mut j = MutationJournal::load_or_create(&dir);
        assert_eq!(j.pending_base(&doc), None, "nothing queued");
        let first = j.enqueue(MutationOp::Put { remote_path: doc.clone(), staging_path: a, if_match_etag: Some("e1".into()) });
        assert!(j.claim(first));
        // A second save while the first is on the wire is based on the same version...
        assert_eq!(j.pending_base(&doc), Some(Some("e1".into())));
        let second = j.enqueue(MutationOp::Put { remote_path: doc.clone(), staging_path: b, if_match_etag: Some("e1".into()) });
        // ...until the first lands and makes the document e2.
        j.remove(first);
        j.rebase(&doc, Some("e1"), Some("e2"));
        let cond = j.entries().iter().find(|e| e.seq == second).map(|e| match &e.op {
            MutationOp::Put { if_match_etag, .. } => if_match_etag.clone(),
            _ => None,
        });
        assert_eq!(cond, Some(Some("e2".into())), "a 412 here would turn our own second save into a conflicted copy");
        // A save based on another version is left alone.
        j.rebase(&doc, Some("e9"), Some("e10"));
        assert_eq!(j.pending_base(&doc), Some(Some("e2".into())));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_upload_moved_onto_the_destination_carries_the_renames_condition() {
        // Offline: the temp file's upload is queued and an older version of it blocks
        // the retarget, so enqueue() re-points the queued upload in place.
        let dir = temp_dir("rename_cond");
        let staging = dir.join("s");
        fs::write(&staging, b"x").unwrap();
        let mut j = MutationJournal::load_or_create(&dir);
        j.enqueue(MutationOp::Put { remote_path: PathBuf::from("/t.tmp"), staging_path: staging, if_match_etag: None });
        j.enqueue(MutationOp::Rename { from: PathBuf::from("/t.tmp"), to: PathBuf::from("/doc"), staging: None, if_match: Some("e1".into()) });
        assert!(matches!(&j.entries()[0].op, MutationOp::Put { remote_path, if_match_etag: Some(e), .. }
            if remote_path == Path::new("/doc") && e == "e1"));
        assert_eq!(j.pending_base(Path::new("/doc")), Some(None),
            "after the MOVE the document is the moved file, whose version is not known yet");
        let _ = fs::remove_dir_all(&dir);
    }
}
