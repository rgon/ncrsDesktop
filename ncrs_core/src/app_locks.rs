//! Server locks for documents an application has open on this mount.
//!
//! Editors leave a lock or swap file next to a document while they have it open
//! (LibreOffice `.~lock.<name>#`, Office `~$<name>`, Vim `.<name>.swp`, Kate
//! `.<name>.kate-swp`). Other people's clients never see those as locks, so while
//! one exists, made here by a process that is still running, the document gets a
//! Nextcloud lock (files_lock): a WebDAV token lock with a short timeout that the
//! daemon keeps refreshing. Nobody else can then save over it, and Nextcloud shows
//! who is editing. If the application or the daemon dies, nothing refreshes the lock
//! and the server drops it within LOCK_TIMEOUT.
//!
//! This module is the bookkeeping: which file names are lock files, which locks are
//! held, and with which token, so every request this daemon sends for a locked
//! document can carry it (webdav_ops). The requests themselves and the refresh loop
//! live in lib.rs (`run_app_locks`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::MutexExt;

/// The timeout every lock is taken and refreshed with. Short, so a lock nobody keeps
/// refreshing (the application or this daemon died) is gone within it.
pub const LOCK_TIMEOUT: Duration = Duration::from_secs(60);
/// How often a held lock is refreshed: three chances before it would expire.
pub const REFRESH_EVERY: Duration = Duration::from_secs(20);
/// How long a deleted lock file keeps its document locked. LibreOffice deletes and
/// writes its lock file anew on every save: releasing at once would unlock and relock
/// the document around each save (each changes its etag on the server).
pub const LOCK_FILE_GRACE: Duration = Duration::from_secs(10);

const STATE_FILE: &str = "app_locks.json";

/// The document a lock file stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocName {
    Exact(String),
    /// Office's owner file: `~$` and the name, or for a long name `~$` in place of its
    /// first two characters, so the document is the name or two characters plus it.
    OfficeOwner(String),
}

impl DocName {
    pub fn matches(&self, candidate: &str) -> bool {
        match self {
            DocName::Exact(n) => candidate == n,
            DocName::OfficeOwner(rest) => {
                candidate == rest
                    || (candidate.ends_with(rest.as_str()) && candidate.chars().count() == rest.chars().count() + 2)
            }
        }
    }
}

/// Which application's lock file `name` is, and the document it is for.
pub fn document_for(name: &str) -> Option<(&'static str, DocName)> {
    if let Some(doc) = name.strip_prefix(".~lock.").and_then(|r| r.strip_suffix('#')) {
        return (!doc.is_empty()).then(|| ("LibreOffice", DocName::Exact(doc.to_string())));
    }
    if let Some(rest) = name.strip_prefix("~$") {
        return (!rest.is_empty()).then(|| ("Office", DocName::OfficeOwner(rest.to_string())));
    }
    let hidden = name.strip_prefix('.')?;
    if let Some(doc) = hidden.strip_suffix(".kate-swp") {
        return (!doc.is_empty()).then(|| ("Kate", DocName::Exact(doc.to_string())));
    }
    // Vim takes .swp, then .swo, .swn, … down to .swa when one is already in use.
    let (doc, ext) = hidden.rsplit_once('.')?;
    let vim = ext.len() == 3 && ext.starts_with("sw") && matches!(ext.as_bytes()[2], b'a'..=b'p');
    (vim && !doc.is_empty()).then(|| ("Vim", DocName::Exact(doc.to_string())))
}

/// One document locked (or to be locked) for an application's lock file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Held {
    pub doc: PathBuf,
    pub lock_file: PathBuf,
    pub app: String,
    /// The process (thread group) that made the lock file, and its name then, so a
    /// reused pid is not taken for it.
    pub pid: u32,
    pub comm: String,
    /// The server's lock token; None while the lock still has to be taken (again).
    pub token: Option<String>,
    /// When the server lock was taken: files_lock counts the timeout from then.
    #[serde(skip)]
    pub locked_at: Option<Instant>,
    #[serde(skip)]
    pub refreshed_at: Option<Instant>,
    /// The lock was taken on the version this mount last had of the document, so while
    /// it is held nobody else changes the file: our requests need only its token, not
    /// the etag (which the lock itself, and taking it again, change).
    #[serde(skip)]
    pub vouched: bool,
    /// When the lock file was deleted; the lock goes once LOCK_FILE_GRACE passes
    /// without it coming back.
    #[serde(skip)]
    pub lock_file_gone_at: Option<Instant>,
    /// We got the lock since tracking began, and nobody else took it since: a lock a
    /// listing shows on the document is ours even while the token is being renewed.
    #[serde(skip)]
    pub ours: bool,
}

static ENABLED: AtomicBool = AtomicBool::new(true);
static HELD: OnceLock<Mutex<HashMap<PathBuf, Held>>> = OnceLock::new();
static STATE: OnceLock<PathBuf> = OnceLock::new();

fn held() -> &'static Mutex<HashMap<PathBuf, Held>> {
    HELD.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Whether lock files are turned into server locks (config `app_lock_files`, live over
/// IPC APP_LOCKS). Only ever acted on while server locks are honoured at all.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// The token of the lock this daemon holds on `path`, for the requests that change it.
pub fn token_for(path: &Path) -> Option<String> {
    held().safe_lock().get(path).and_then(|h| h.token.clone())
}

/// Whether a server lock on `path` is this daemon's own, which must not make the file
/// read-only here: also between a save replacing the file (the server drops the lock
/// with it) and the lock being taken again, when a listing can still show ours.
pub fn holds(path: &Path) -> bool {
    held().safe_lock().get(path).is_some_and(|h| h.token.is_some() || h.ours)
}

/// Someone else holds the lock on `doc`: theirs is honoured like any other.
pub fn refused(doc: &Path) {
    if let Some(e) = held().safe_lock().get_mut(doc) {
        e.ours = false;
        e.vouched = false;
    }
}

/// What a request changing `path` sends: the lock token, and whether that alone is
/// enough (the lock is vouched for, so no etag condition is needed).
pub fn condition_for(path: &Path) -> (Option<String>, bool) {
    match held().safe_lock().get(path) {
        Some(h) if h.token.is_some() => (h.token.clone(), h.vouched),
        _ => (None, false),
    }
}

/// Whether every change to `path` on the server now is this daemon's own: a version
/// read from it then is ours too.
pub fn vouched(path: &Path) -> bool {
    held().safe_lock().get(path).is_some_and(|h| h.vouched)
}

/// Marks the lock on `doc` as taken on a version this mount knew (or not). Set before
/// the LOCK is sent, so a read racing the etag change it makes still accepts it.
pub fn set_vouched(doc: &Path, on: bool) {
    if let Some(e) = held().safe_lock().get_mut(doc) {
        e.vouched = on;
    }
}

pub fn snapshot() -> Vec<Held> {
    held().safe_lock().values().cloned().collect()
}

/// Starts tracking `doc` for a lock file a process just made; the lock itself is taken
/// by the refresh loop. A second lock file for the same document is ignored.
pub fn track(doc: PathBuf, lock_file: PathBuf, app: &str, pid: u32) -> bool {
    let Some(tgid) = tgid_of(pid) else { return false };
    let comm = comm_of(tgid).unwrap_or_default();
    let mut h = held().safe_lock();
    if let Some(e) = h.get_mut(&doc) {
        // Written anew (LibreOffice does so on every save): the lock stays.
        if e.lock_file_gone_at.take().is_some() {
            e.lock_file = lock_file;
            e.pid = tgid;
            e.comm = comm;
        }
        return false;
    }
    h.insert(doc.clone(), Held {
        doc, lock_file, app: app.to_string(), pid: tgid, comm, token: None, locked_at: None, refreshed_at: None,
        vouched: false, lock_file_gone_at: None, ours: false,
    });
    drop(h);
    save();
    true
}

/// Records the lock taken (or refreshed) for `doc`.
pub fn set_token(doc: &Path, token: Option<String>, now: Instant) {
    if let Some(e) = held().safe_lock().get_mut(doc) {
        if token.is_some() && e.token != token {
            e.locked_at = Some(now);
            e.ours = true;
        }
        if token.is_none() {
            e.locked_at = None;
            e.vouched = false;
        }
        e.refreshed_at = token.as_ref().map(|_| now);
        e.token = token;
    }
    save();
}

pub fn mark_refreshed(doc: &Path, now: Instant) {
    if let Some(e) = held().safe_lock().get_mut(doc) {
        e.refreshed_at = Some(now);
    }
}

/// Stops tracking `doc`; the caller releases the server lock with the token returned.
pub fn untrack(doc: &Path) -> Option<Held> {
    let gone = held().safe_lock().remove(doc);
    save();
    gone
}

/// A tracked lock file was deleted: its document's lock goes after LOCK_FILE_GRACE,
/// unless the lock file comes back first.
pub fn lock_file_removed(lock_file: &Path, now: Instant) {
    if let Some(h) = held().safe_lock().values_mut().find(|h| h.lock_file == lock_file) {
        h.lock_file_gone_at = Some(now);
    }
}

/// The `Timeout` to send when refreshing: files_lock counts it from when the lock was
/// taken, so "LOCK_TIMEOUT more from now" is the time held so far plus LOCK_TIMEOUT.
pub fn refresh_timeout(locked_at: Instant, now: Instant) -> Duration {
    now.saturating_duration_since(locked_at) + LOCK_TIMEOUT
}

/// Whether the process that made a lock file is still running (the same program, not
/// a later one that got its pid).
pub fn process_alive(pid: u32, comm: &str) -> bool {
    comm_of(pid).is_some_and(|c| c == comm)
}

fn comm_of(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{}/comm", pid)).ok().map(|s| s.trim_end().to_string())
}

/// FUSE reports the calling thread; a lock file outlives the thread that made it.
fn tgid_of(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{}/status", pid)).ok()?;
    status.lines().find_map(|l| l.strip_prefix("Tgid:")).and_then(|v| v.trim().parse().ok())
}

/// "LibreOffice on <host>": what other people see as the lock's owner.
pub fn owner_text(app: &str) -> String {
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").map(|h| h.trim().to_string()).unwrap_or_default();
    if host.is_empty() { app.to_string() } else { format!("{} on {}", app, host) }
}

/// Where held locks are written, so a restarted daemon can release them. Returns what
/// the previous run left: every one of those is released at start, and taken again only
/// for an application that is still running.
pub fn init(cache_dir: &Path) -> Vec<Held> {
    let path = cache_dir.join(STATE_FILE);
    let previous = std::fs::read(&path).ok()
        .and_then(|d| serde_json::from_slice::<Vec<Held>>(&d).ok())
        .unwrap_or_default();
    let _ = STATE.set(path);
    previous
}

/// Adopts an entry from the previous run, to be locked again (with a new token).
pub fn readopt(mut h: Held) {
    h.token = None;
    h.locked_at = None;
    h.refreshed_at = None;
    h.vouched = false;
    h.lock_file_gone_at = None;
    h.ours = false;
    held().safe_lock().insert(h.doc.clone(), h);
    save();
}

fn save() {
    let Some(path) = STATE.get() else { return };
    let list = snapshot();
    if let Ok(data) = serde_json::to_vec(&list) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, data).and_then(|()| std::fs::rename(&tmp, path)).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_and_swap_files_name_their_document() {
        assert_eq!(document_for(".~lock.Report Q3.odt#"), Some(("LibreOffice", DocName::Exact("Report Q3.odt".into()))));
        assert_eq!(document_for(".notes.txt.swp"), Some(("Vim", DocName::Exact("notes.txt".into()))));
        assert_eq!(document_for(".notes.txt.swo"), Some(("Vim", DocName::Exact("notes.txt".into()))));
        assert_eq!(document_for(".main.rs.kate-swp"), Some(("Kate", DocName::Exact("main.rs".into()))));
        assert!(matches!(document_for("~$port.docx"), Some(("Office", _))));
        for not_lock in ["report.odt", ".hidden", ".~lock.#", "~$", ".x.swq", ".swp", "notes.swp", ".config.json"] {
            assert_eq!(document_for(not_lock), None, "{not_lock}");
        }
    }

    #[test]
    fn office_owner_files_match_short_and_long_names() {
        let (_, short) = document_for("~$doc.docx").unwrap();
        assert!(short.matches("doc.docx"));
        let (_, long) = document_for("~$port 2026.docx").unwrap();
        assert!(long.matches("Report 2026.docx"), "Office replaces the first two characters of a long name");
        assert!(!long.matches("XReport 2026.docx"));
    }

    #[test]
    fn a_lock_file_written_anew_keeps_the_lock() {
        let doc = PathBuf::from("/t/rewritten.odt");
        let lf = PathBuf::from("/t/.~lock.rewritten.odt#");
        assert!(track(doc.clone(), lf.clone(), "LibreOffice", std::process::id()));
        set_token(&doc, Some("opaquelocktoken:1".into()), Instant::now());
        set_vouched(&doc, true);
        assert_eq!(condition_for(&doc), (Some("opaquelocktoken:1".into()), true));
        lock_file_removed(&lf, Instant::now());
        assert!(!track(doc.clone(), lf.clone(), "LibreOffice", std::process::id()), "the same document is not tracked twice");
        let h = snapshot().into_iter().find(|h| h.doc == doc).unwrap();
        assert!(h.lock_file_gone_at.is_none() && h.token.is_some(), "a save's lock-file rewrite must not release the lock");
        set_token(&doc, None, Instant::now());
        assert_eq!(condition_for(&doc), (None, false), "a dropped lock vouches for nothing");
        assert!(holds(&doc), "until it is taken again, a listing showing the lock shows ours");
        refused(&doc);
        assert!(!holds(&doc), "someone else's lock is honoured");
        untrack(&doc);
    }

    #[test]
    fn a_refresh_extends_the_lock_from_now() {
        let t0 = Instant::now();
        assert_eq!(refresh_timeout(t0, t0 + Duration::from_secs(40)), Duration::from_secs(100),
            "files_lock counts the timeout from when the lock was taken");
    }
}
