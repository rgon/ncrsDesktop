//! Who is reading? Process conditions shared by every per-toolkit rule.
//!
//! Toolkit components describe the processes their rules apply to: the
//! thumbnailer guard ("refuse thumbnailer X") and file-type probes ("answer
//! this probe only from a process that uses toolkit Y"). One matcher, one
//! cache, so a new toolkit (KDE, …) only declares data.
//!
//! The FUSE request carries the caller's pid (a thread id, which `/proc`
//! resolves just the same). Reads of `/proc` are cached briefly per pid: a
//! single directory listing can trigger thousands of opens from one process.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::MutexExt;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ProcessMatch {
    /// A program, by executable name (also matched against the kernel's
    /// 15-byte `comm`, which is what a script interpreter shows).
    Program(String),
    /// A generic host process whose command line names what it runs
    /// (e.g. KIO's `kioworker …/kio/thumbnail.so thumbnail …`).
    CmdlineContains(&'static str),
    /// A process that has a shared library loaded, by file-name prefix
    /// (e.g. `libgio-2.0.so`): "uses this toolkit". Checked in `/proc/<pid>/maps`.
    LinksLibrary(&'static str),
}

impl ProcessMatch {
    pub fn describe(&self) -> String {
        match self {
            ProcessMatch::Program(p) => p.clone(),
            ProcessMatch::CmdlineContains(c) => format!("process running {}", c),
            ProcessMatch::LinksLibrary(l) => format!("process using {}", l),
        }
    }
}

/// How long a pid's classification is trusted. Short enough that pid reuse
/// is irrelevant, long enough to absorb a listing's burst of opens.
const CACHE_TTL: Duration = Duration::from_secs(10);
const CACHE_MAX: usize = 4096;

fn cache() -> &'static Mutex<HashMap<(u32, ProcessMatch), (bool, Instant)>> {
    static C: OnceLock<Mutex<HashMap<(u32, ProcessMatch), (bool, Instant)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached(pid: u32, m: &ProcessMatch) -> Option<bool> {
    let (hit, at) = cache().safe_lock().get(&(pid, m.clone())).copied()?;
    (at.elapsed() < CACHE_TTL).then_some(hit)
}

/// Whether deciding `m` only reads `/proc` files that never take the target's
/// `mmap_lock`: the `exe` link (RCU) and `comm` (task lock). `maps` and
/// `cmdline` do take it, and a process with a thread page-faulting on a file
/// mmapped from this mount holds it while that fault waits for a FUSE read. If
/// the FUSE dispatch thread then reads such a file for another of that
/// process's threads, it queues behind a writer that queued behind the fault,
/// and the fault's read is never dispatched: an unkillable hang (review
/// addendum, 2026-09-24).
fn lock_free(m: &ProcessMatch) -> bool {
    matches!(m, ProcessMatch::Program(_))
}

/// Whether process `pid` satisfies `m` (cached). May read `/proc/<pid>/maps`
/// or `cmdline`: never call it on the FUSE dispatch thread — use `try_matches`.
pub fn matches(pid: u32, m: &ProcessMatch) -> bool {
    if pid == 0 {
        return false;
    }
    if let Some(hit) = cached(pid, m) {
        return hit;
    }
    let key = (pid, m.clone());
    let hit = eval(Path::new("/proc"), pid, m);
    let mut c = cache().safe_lock();
    if c.len() >= CACHE_MAX {
        c.retain(|_, (_, at)| at.elapsed() < CACHE_TTL);
        if c.len() >= CACHE_MAX {
            c.clear();
        }
    }
    c.insert(key, (hit, Instant::now()));
    hit
}

/// `matches` for the FUSE dispatch thread: the answer when it is cached or
/// decidable without the target's `mmap_lock`, else `None` — the caller then
/// asks again from a pool worker with `matches`. Same verdicts either way.
pub fn try_matches(pid: u32, m: &ProcessMatch) -> Option<bool> {
    if pid == 0 {
        return Some(false);
    }
    if let Some(hit) = cached(pid, m) {
        return Some(hit);
    }
    lock_free(m).then(|| matches(pid, m))
}

/// The first of `ms` that `pid` satisfies.
pub fn first_match(pid: u32, ms: &[ProcessMatch]) -> Option<&ProcessMatch> {
    ms.iter().find(|m| matches(pid, m))
}

/// `first_match` via `try_matches`: `None` when a matcher before the first
/// match could not be decided here.
pub fn try_first_match(pid: u32, ms: &[ProcessMatch]) -> Option<Option<&ProcessMatch>> {
    for m in ms {
        if try_matches(pid, m)? {
            return Some(Some(m));
        }
    }
    Some(None)
}

/// Uncached evaluation against a `/proc`-shaped directory. An unreadable or
/// vanished process matches nothing (callers treat "no match" as "read
/// normally", the safe default for data).
pub(crate) fn eval(proc_root: &Path, pid: u32, m: &ProcessMatch) -> bool {
    let dir = proc_root.join(pid.to_string());
    match m {
        ProcessMatch::Program(name) => {
            let exe = std::fs::read_link(dir.join("exe"))
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
            if exe.as_deref() == Some(name.as_str()) {
                return true;
            }
            let comm_name: String = name.chars().take(15).collect();
            std::fs::read_to_string(dir.join("comm")).is_ok_and(|c| c.trim_end() == comm_name)
        }
        ProcessMatch::CmdlineContains(needle) => std::fs::read(dir.join("cmdline"))
            .is_ok_and(|b| String::from_utf8_lossy(&b).replace('\0', " ").contains(needle)),
        ProcessMatch::LinksLibrary(prefix) => std::fs::read_to_string(dir.join("maps")).is_ok_and(|maps| {
            maps.lines().any(|l| {
                l.rsplit('/').next().is_some_and(|file| file.starts_with(prefix)) && l.contains('/')
            })
        }),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn fake_proc(root: &Path, pid: u32, exe: &str, comm: &str, cmdline: &[&str], libs: &[&str]) {
        let d = root.join(pid.to_string());
        std::fs::create_dir_all(&d).unwrap();
        std::os::unix::fs::symlink(exe, d.join("exe")).unwrap();
        std::fs::write(d.join("comm"), format!("{}\n", comm)).unwrap();
        std::fs::write(d.join("cmdline"), cmdline.join("\0")).unwrap();
        let maps: String = libs
            .iter()
            .map(|l| format!("7f0000000000-7f0000001000 r-xp 00000000 fd:01 1234  /usr/lib/x86_64-linux-gnu/{}\n", l))
            .collect();
        std::fs::write(d.join("maps"), format!("5600000-5601000 r-xp 0 fd:01 1 {}\n{}7ffc-7ffd rw-p 0 00:00 0  [stack]\n", exe, maps)).unwrap();
    }

    #[test]
    fn links_library_distinguishes_toolkit_users() {
        let dir = tempfile::tempdir().unwrap();
        fake_proc(dir.path(), 1, "/usr/bin/nautilus", "nautilus", &[], &["libgio-2.0.so.0.8000.0", "libgtk-4.so.1"]);
        fake_proc(dir.path(), 2, "/usr/bin/cp", "cp", &[], &["libc.so.6", "libacl.so.1"]);
        fake_proc(dir.path(), 3, "/usr/bin/dolphin", "dolphin", &[], &["libKF6KIOCore.so.6", "libQt6Core.so.6"]);
        let gio = ProcessMatch::LinksLibrary("libgio-2.0.so");
        let kio = ProcessMatch::LinksLibrary("libKF6KIOCore.so");
        assert!(eval(dir.path(), 1, &gio));
        assert!(!eval(dir.path(), 2, &gio), "cp opens with O_NOATIME but is not a GLib sniffer");
        assert!(!eval(dir.path(), 3, &gio));
        assert!(eval(dir.path(), 3, &kio));
    }

    #[test]
    fn unreadable_process_matches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        for m in [
            ProcessMatch::Program("x".into()),
            ProcessMatch::CmdlineContains("x"),
            ProcessMatch::LinksLibrary("libx"),
        ] {
            assert!(!eval(dir.path(), 42, &m));
        }
        assert!(!matches(0, &ProcessMatch::LinksLibrary("libc")));
    }

    #[test]
    fn try_matches_never_reads_maps_or_cmdline_but_agrees_once_cached() {
        let me = std::process::id();
        let lib = ProcessMatch::LinksLibrary("libncrs-try-matches-probe");
        let cmd = ProcessMatch::CmdlineContains("ncrs-try-matches-probe");
        assert_eq!(try_matches(me, &lib), None, "an uncached maps read is left to a pool worker");
        assert_eq!(try_matches(me, &cmd), None, "so is an uncached cmdline read");
        assert!(!matches(me, &lib));
        assert_eq!(try_matches(me, &lib), Some(false), "a cached verdict is answered at once");
        assert_eq!(try_matches(0, &lib), Some(false));
        // Program matchers read `exe` and `comm` only, so they are decided inline.
        assert!(try_matches(me, &ProcessMatch::Program("definitely-not-this-test".into())).is_some());
        let ms = [ProcessMatch::Program("definitely-not-this-test".into()), cmd.clone()];
        assert_eq!(try_first_match(me, &ms), None, "an undecided matcher before any match leaves it undecided");
        assert!(first_match(me, &ms).is_none());
        assert_eq!(try_first_match(me, &ms), Some(None));
    }

    #[test]
    fn our_own_process_links_libc() {
        // Exercises the real /proc path and the cache.
        let me = std::process::id();
        assert!(matches(me, &ProcessMatch::LinksLibrary("libc.so")));
        assert!(matches(me, &ProcessMatch::LinksLibrary("libc.so")));
        assert!(!matches(me, &ProcessMatch::LinksLibrary("libdefinitely-not-loaded")));
    }
}
