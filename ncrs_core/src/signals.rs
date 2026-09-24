//! Stop signals for the `ncrs` daemon binary.
//!
//! `systemctl stop`, a logout or a plain `kill` send SIGTERM (a terminal
//! Ctrl-C SIGINT, a closed session SIGHUP). With the default action the
//! process died on the spot: the journal's group commit (`DeferredSaves`) had
//! not written the last few changes, and releases queued behind in-flight
//! writes were never journaled, so edits saved moments earlier were lost.
//!
//! The binary blocks those signals before any thread exists
//! ([`block_shutdown_signals`]), so every thread inherits the mask and none is
//! interrupted; `mount_ncfs` then starts the `signals` service first thing,
//! which waits for them. Before the mount exists, the first one writes the
//! journal (once loaded) and exits. Once mounted, it:
//! 1. writes the journal now and switches it to synchronous saves, so what
//!    the process does from here on — including being `SIGKILL`ed when the
//!    stop timeout runs out — can no longer lose a change;
//! 2. unmounts with a plain `fusermount3 -u`. The session loop then returns
//!    and `mount_ncfs` runs its usual shutdown (drain the handle lanes, flush
//!    the journal, remove the IPC socket). Never a lazy unmount: that frees
//!    the path while files are still open, and their later saves would land
//!    on the bare directory (see `prepare_mount_point`). While the mount is
//!    busy the daemon keeps serving and retries every second.
//!
//! If the mount is no longer attached (someone detached it), there is no
//! session end to wait for: drain, flush and exit. A second signal flushes
//! again and exits immediately.
//!
//! Library callers (the GUI) never call [`block_shutdown_signals`], so
//! `mount_ncfs` leaves their signal handling alone.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::mutation_journal::{self, SharedJournal};

static BLOCKED: AtomicBool = AtomicBool::new(false);

const SIGNALS: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

/// How often a busy unmount is retried after a stop signal.
const UNMOUNT_RETRY: Duration = Duration::from_secs(1);

/// How long pending releases may take to reach the journal before an exit
/// that has no session shutdown to run it (matches `mount_ncfs`'s drain).
const DRAIN_FOR: Duration = Duration::from_secs(10);

fn signal_set() -> libc::sigset_t {
    // SAFETY: sigemptyset/sigaddset only write the local set.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in SIGNALS {
            libc::sigaddset(&mut set, s);
        }
        set
    }
}

/// Blocks SIGTERM, SIGINT and SIGHUP on the calling thread. Must run first in
/// `main`, before any thread is spawned, so every later thread inherits the
/// mask and the `signals` service is the only one that ever sees them.
/// (`std::process::Command` resets the mask in its children.)
pub fn block_shutdown_signals() {
    let set = signal_set();
    // SAFETY: plain syscall wrapper on a valid set; no old mask requested.
    let rc = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) };
    if rc == 0 {
        BLOCKED.store(true, Ordering::SeqCst);
    } else {
        log::warn!("signals: cannot block stop signals ({}) — a stop may skip the journal flush", rc);
    }
}

/// Undoes [`block_shutdown_signals`] for a run that will not mount (a
/// diagnostic mode), so Ctrl-C still ends it.
pub fn unblock_shutdown_signals() {
    let set = signal_set();
    // SAFETY: as in block_shutdown_signals.
    unsafe { libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut()) };
    BLOCKED.store(false, Ordering::SeqCst);
}

/// Where `fusermount3` (or the older `fusermount`) may be, in order: `PATH`
/// first, then the usual places, for a service started with a bare `PATH`.
const FUSERMOUNT: [&str; 6] = [
    "fusermount3", "/usr/bin/fusermount3", "/bin/fusermount3",
    "fusermount", "/usr/bin/fusermount", "/bin/fusermount",
];

/// Runs the first `FUSERMOUNT` that exists with `args` and the mount point.
/// NotFound when none does.
pub(crate) fn run_fusermount(args: &[&str], mount_point: &std::path::Path) -> std::io::Result<(&'static str, std::process::Output)> {
    for bin in FUSERMOUNT {
        match std::process::Command::new(bin).args(args).arg("--").arg(mount_point).output() {
            Ok(o) => return Ok((bin, o)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::NotFound, format!("none of {:?} found", FUSERMOUNT)))
}

/// What a clean unmount attempt found.
enum Unmount {
    Done,
    Busy,
    NotMounted,
    /// No unmount tool could be run.
    Failed(String),
}

fn try_unmount(mount_point: &std::path::Path) -> Unmount {
    if !crate::is_live_fuse_mount(mount_point) {
        return Unmount::NotMounted;
    }
    match run_fusermount(&["-u"], mount_point) {
        Ok((_, o)) if o.status.success() => Unmount::Done,
        Ok((bin, o)) => {
            if !crate::is_live_fuse_mount(mount_point) {
                return Unmount::NotMounted;
            }
            log::info!("signals: {} -u: {}", bin, String::from_utf8_lossy(&o.stderr).trim());
            Unmount::Busy
        }
        Err(e) => Unmount::Failed(e.to_string()),
    }
}

/// How many seconds in a row an unmount tool may fail to run before the
/// daemon gives up on a clean unmount, writes the journal and exits.
const UNMOUNT_TOOL_TRIES: u32 = 10;

/// Waits up to `timeout` for a stop signal; the signal number, or `None`.
fn wait_signal(set: &libc::sigset_t, timeout: Option<Duration>) -> Option<libc::c_int> {
    loop {
        let rc = match timeout {
            // SAFETY: `set` is valid and blocked on this thread; no siginfo wanted.
            None => unsafe { libc::sigwaitinfo(set, std::ptr::null_mut()) },
            Some(t) => {
                let ts = libc::timespec { tv_sec: t.as_secs() as libc::time_t, tv_nsec: t.subsec_nanos() as libc::c_long };
                // SAFETY: as above; `ts` outlives the call.
                unsafe { libc::sigtimedwait(set, std::ptr::null_mut(), &ts) }
            }
        };
        if rc > 0 {
            return Some(rc);
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EINTR) => continue,
            _ => return None, // EAGAIN: timed out
        }
    }
}

fn flush_and_exit(journal: &SharedJournal, busy: &dyn Fn() -> usize, why: &str) -> ! {
    let until = Instant::now() + DRAIN_FOR;
    while busy() > 0 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
    }
    mutation_journal::flush_deferred(journal);
    log::warn!("signals: {} — journal written, exiting", why);
    // SAFETY: _exit ends the process without running destructors or atexit
    // handlers, which may be mid-use on other threads.
    unsafe { libc::_exit(0) }
}

/// Where `mount_ncfs` is, as the `signals` service sees it.
enum Phase {
    /// No mount yet: a stop signal writes the journal (once there is one)
    /// and exits, there is nothing to unmount.
    Starting,
    /// `fuser::Session::new` is running: a signal waits for its outcome.
    Mounting,
    /// Mounted at this path; the closure counts handles whose release may
    /// still have to reach the journal.
    Mounted(PathBuf, Box<dyn Fn() -> usize + Send>),
}

struct Watch {
    phase: Mutex<Phase>,
    changed: Condvar,
    journal: Mutex<Option<SharedJournal>>,
}

fn watch() -> &'static Watch {
    static WATCH: OnceLock<Watch> = OnceLock::new();
    WATCH.get_or_init(|| Watch { phase: Mutex::new(Phase::Starting), changed: Condvar::new(), journal: Mutex::new(None) })
}

fn set_phase(p: Phase) {
    let w = watch();
    *w.phase.lock().unwrap_or_else(|e| e.into_inner()) = p;
    w.changed.notify_all();
}

/// The journal a stop signal must write; set as soon as it is loaded.
pub(crate) fn set_journal(journal: SharedJournal) {
    *watch().journal.lock().unwrap_or_else(|e| e.into_inner()) = Some(journal);
}

/// Called right before `fuser::Session::new`: a signal from here on waits
/// for the mount's outcome instead of exiting under it (which would leave a
/// dead mount behind).
pub(crate) fn mounting() {
    set_phase(Phase::Mounting);
}

/// The mount exists: a stop signal from here on unmounts it cleanly.
pub(crate) fn mounted(mount_point: PathBuf, busy_lanes: impl Fn() -> usize + Send + 'static) {
    set_phase(Phase::Mounted(mount_point, Box::new(busy_lanes)));
}

/// `fuser::Session::new` failed: back to exiting on a signal.
pub(crate) fn mount_failed() {
    set_phase(Phase::Starting);
}

/// Starts the `signals` service when [`block_shutdown_signals`] ran; a no-op
/// for library callers. Called first thing in `mount_ncfs` (right after the
/// seccomp filter, which spawns nothing and must precede every thread), and
/// so the process's first spawned thread: if it cannot start, the calling
/// thread unblocks the signals before any other exists. A stop signal is
/// never left pending through a slow startup: before the mount exists it
/// writes the journal, if loaded, and exits. The signals are kept blocked
/// from `main` on rather than unblocked until the session exists: a thread
/// spawned while they were unblocked would inherit that mask, and a stop
/// signal delivered to it would kill the process with the default action,
/// mid-save, however late in the run.
pub(crate) fn start_watcher() {
    if !BLOCKED.load(Ordering::SeqCst) {
        return;
    }
    let started = crate::bg::spawn_service("signals", move || {
        let set = signal_set();
        let Some(sig) = wait_signal(&set, None) else {
            log::error!("signals: sigwaitinfo failed: {}", std::io::Error::last_os_error());
            return;
        };
        let w = watch();
        let journal = || w.journal.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let (mount_point, busy_lanes) = {
            let mut phase = w.phase.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                match std::mem::replace(&mut *phase, Phase::Starting) {
                    // Holding the lock: no mount can start meanwhile.
                    Phase::Starting => {
                        if let Some(j) = journal() {
                            mutation_journal::flush_deferred(&j);
                        }
                        log::warn!("signals: received signal {} before the mount existed — exiting", sig);
                        // SAFETY: as in flush_and_exit.
                        unsafe { libc::_exit(0) }
                    }
                    Phase::Mounting => {
                        *phase = Phase::Mounting;
                        phase = w.changed.wait(phase).unwrap_or_else(|e| e.into_inner());
                    }
                    Phase::Mounted(mp, busy) => break (mp, busy),
                }
            }
        };
        let Some(journal) = journal() else {
            log::error!("signals: mounted without a journal — exiting");
            // SAFETY: as in flush_and_exit.
            unsafe { libc::_exit(0) }
        };
        log::warn!("signals: received signal {} — writing the journal and unmounting {}", sig, mount_point.display());
        mutation_journal::save_synchronously(&journal);
        let mut logged_busy = false;
        let mut tool_failures = 0;
        loop {
            match try_unmount(&mount_point) {
                // The session loop returns; mount_ncfs shuts down from there.
                Unmount::Done => break,
                Unmount::NotMounted => flush_and_exit(&journal, &*busy_lanes, "mount already detached"),
                Unmount::Busy => {
                    if !logged_busy {
                        log::warn!("signals: {} is busy — still serving, retrying the unmount every {:?}", mount_point.display(), UNMOUNT_RETRY);
                        logged_busy = true;
                    }
                }
                Unmount::Failed(e) => {
                    tool_failures += 1;
                    log::error!("signals: cannot unmount {} ({}), try {}/{}", mount_point.display(), e, tool_failures, UNMOUNT_TOOL_TRIES);
                    if tool_failures >= UNMOUNT_TOOL_TRIES {
                        // Without a way to unmount, the session never ends: exit
                        // with the journal written rather than wait for SIGKILL.
                        flush_and_exit(&journal, &*busy_lanes, "no way to unmount");
                    }
                }
            }
            if let Some(sig) = wait_signal(&set, Some(UNMOUNT_RETRY)) {
                flush_and_exit(&journal, &|| 0, &format!("second signal {}", sig));
            }
        }
        // Unmounted: a second signal while the shutdown runs cuts it short.
        if let Some(sig) = wait_signal(&set, None) {
            flush_and_exit(&journal, &|| 0, &format!("second signal {} during shutdown", sig));
        }
    });
    if let Err(e) = started {
        // Nothing would ever take the blocked signals: a stop would stay
        // pending and the daemon would only die to SIGKILL, with no journal
        // flush at all. Unblocked on this thread (the daemon's main thread,
        // which runs `mount_ncfs` until the end), a stop is delivered here
        // with its default action, and every thread spawned from here on —
        // this is the first `mount_ncfs` spawns — inherits the unblocked
        // mask. A thread an earlier library call may have started keeps its
        // blocked mask, which only means it is never the one picked.
        log::error!("signals: cannot start the signal watcher: {} — stop signals unblocked; a stop kills the daemon without flushing the journal", e);
        unblock_shutdown_signals();
    }
}
