use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JournalEntry {
    pub seq: SeqId,
    pub op: MutationOp,
    pub created_at_ms: u64,
    pub attempts: u32,
    pub last_error: Option<String>,
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
}

pub type SharedJournal = Arc<Mutex<MutationJournal>>;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl MutationOp {
    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        match self {
            MutationOp::Put { remote_path, .. } => remote_path,
            MutationOp::MkDir { path } => path,
            MutationOp::Unlink { path } => path,
            MutationOp::RmDir { path } => path,
            MutationOp::Rename { from, .. } => from,
        }
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
            if let MutationOp::Put { staging_path, remote_path, .. } = &entry.op {
                if !staging_path.exists() {
                    log::warn!("JOURNAL: staging file missing for {}, will discard", remote_path.display());
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

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn mark_failed(&mut self, seq: SeqId, error: String) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.seq == seq) {
            entry.attempts += 1;
            entry.last_error = Some(error);
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
    }

    // ── Persistence ──────────────────────────────────────────

    fn save_journal(&self) {
        let list: Vec<&JournalEntry> = self.entries.iter().collect();
        match serde_json::to_vec(&list) {
            Ok(data) => {
                let tmp = self.journal_path.with_extension("tmp");
                if std::fs::write(&tmp, &data).is_ok() {
                    let _ = std::fs::rename(&tmp, &self.journal_path);
                }
            }
            Err(e) => log::error!("JOURNAL: serialize failed: {}", e),
        }
    }

    fn save_conflicts(&self) {
        match serde_json::to_vec(&self.conflicts) {
            Ok(data) => {
                let tmp = self.conflicts_path.with_extension("tmp");
                if std::fs::write(&tmp, &data).is_ok() {
                    let _ = std::fs::rename(&tmp, &self.conflicts_path);
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
                    matches!(&e.op, MutationOp::Put { remote_path, if_match_etag: Some(_), .. } if remote_path == path)
                });
                if !has_prior_server_etag {
                    let before = self.entries.len();
                    self.entries.retain(|e| {
                        !matches!(&e.op, MutationOp::Put { remote_path, .. } if remote_path == path)
                        && !matches!(&e.op, MutationOp::MkDir { path: p } if p == path)
                    });
                    if self.entries.len() < before {
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
            let j = journal.safe_lock();
            match j.peek_front() {
                Some(e) => e.clone(),
                None => {
                    log::info!("JOURNAL: replay complete — queue empty");
                    return;
                }
            }
        };

        if entry.attempts >= MutationJournal::max_attempts() {
            log::warn!("JOURNAL: entry seq={} exceeded max attempts, marking permanent failure", entry.seq);
            let mut j = journal.safe_lock();
            let desc = format!("{:?}: {}", entry.op, entry.last_error.as_deref().unwrap_or("unknown"));
            j.add_conflict(ConflictKind::PermanentFailure { description: desc });
            j.dequeue_front();
            continue;
        }

        match execute_op(&entry, ctx, cache, dirty, error_log) {
            ReplayResult::Ok => {
                let mut j = journal.safe_lock();
                j.dequeue_front();
                if let MutationOp::Put { staging_path, .. } = &entry.op {
                    let _ = std::fs::remove_file(staging_path);
                }
            }
            ReplayResult::Conflict(kind) => {
                let mut j = journal.safe_lock();
                j.add_conflict(kind);
                j.dequeue_front();
                if let MutationOp::Put { staging_path, .. } = &entry.op {
                    let _ = std::fs::remove_file(staging_path);
                }
            }
            ReplayResult::Idempotent => {
                let mut j = journal.safe_lock();
                j.dequeue_front();
                if let MutationOp::Put { staging_path, .. } = &entry.op {
                    let _ = std::fs::remove_file(staging_path);
                }
            }
            ReplayResult::NetworkError(msg) => {
                log::warn!("JOURNAL: replay stopped — network error: {}", msg);
                journal.safe_lock().mark_failed(entry.seq, msg);
                return;
            }
            ReplayResult::ServerError(msg) => {
                journal.safe_lock().mark_failed(entry.seq, msg);
            }
        }
    }
}

enum ReplayResult {
    Ok,
    Conflict(ConflictKind),
    Idempotent,
    NetworkError(String),
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
            let body = match std::fs::read(staging_path) {
                Ok(b) => b,
                Err(e) => {
                    return ReplayResult::Conflict(ConflictKind::PermanentFailure {
                        description: format!("staging file unreadable for {}: {}", remote_path.display(), e),
                    });
                }
            };
            let etag_ref = if_match_etag.as_deref();
            match ctx.backend.put_file(remote_path, body.clone(), etag_ref) {
                Ok(result) => {
                    log::info!("JOURNAL replay: PUT {} → token {:?}", remote_path.display(), result.new_change_token);
                    let mut c = cache.safe_lock();
                    let parent = remote_path.parent().unwrap_or(Path::new("/")).to_path_buf();
                    if let Some(dir) = c.dir_cache.get_mut(&parent) {
                        let mut files = (*dir.files).clone();
                        if let Some(e) = files.iter_mut().find(|e| e.path == *remote_path) {
                            e.change_token = result.new_change_token;
                            e.size = body.len() as u64;
                            e.modified = Some(SystemTime::now());
                        }
                        dir.files = Arc::new(files);
                    }
                    drop(c);
                    dirty.safe_lock().insert(parent);
                    ReplayResult::Ok
                }
                Err(BackendWriteError::Conflict) => {
                    log::warn!("JOURNAL replay: PUT {} conflict — creating conflicted copy", remote_path.display());
                    let conflict_name = crate::make_conflict_name(remote_path);
                    let _ = ctx.backend.put_file(&conflict_name, body, None);
                    crate::push_error(error_log, remote_path.clone(), crate::SyncErrorKind::Conflict, "Server version changed — conflicted copy created".into());
                    ReplayResult::Conflict(ConflictKind::EditConflict {
                        local_path: remote_path.clone(),
                        conflicted_copy_path: conflict_name,
                    })
                }
                Err(BackendWriteError::Network(e)) => ReplayResult::NetworkError(e),
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
                Err(BackendWriteError::Network(e)) => ReplayResult::NetworkError(e),
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
                Err(BackendWriteError::Network(e)) => ReplayResult::NetworkError(e),
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
                Err(BackendWriteError::Network(e)) => ReplayResult::NetworkError(e),
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
                Err(BackendWriteError::Network(e)) => ReplayResult::NetworkError(e),
                Err(e) => ReplayResult::ServerError(e.to_string()),
            }
        }
    }
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
        }];
        let journal_path = dir.join(JOURNAL_FILE);
        fs::write(&journal_path, serde_json::to_vec(&entries).unwrap()).unwrap();

        let j = MutationJournal::load_or_create(&dir);
        assert!(j.is_empty());
        assert_eq!(j.unresolved_conflicts().len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }
}
