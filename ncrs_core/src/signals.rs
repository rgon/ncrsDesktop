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
//! interrupted; `mount_ncfs` then starts the `signals` service, which waits
//! for them. On the first one it:
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

/// What a clean unmount attempt found.
enum Unmount {
    Done,
    Busy,
    NotMounted,
}

fn try_unmount(mount_point: &std::path::Path) -> Unmount {
    if !crate::is_live_fuse_mount(mount_point) {
        return Unmount::NotMounted;
    }
    for bin in ["fusermount3", "fusermount"] {
        match std::process::Command::new(bin).arg("-u").arg("--").arg(mount_point).output() {
            Ok(o) if o.status.success() => return Unmount::Done,
            Ok(o) => {
                if !crate::is_live_fuse_mount(mount_point) {
                    return Unmount::NotMounted;
                }
                log::info!("signals: {} -u: {}", bin, String::from_utf8_lossy(&o.stderr).trim());
                return Unmount::Busy;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                log::warn!("signals: cannot run {}: {}", bin, e);
                return Unmount::Busy;
            }
        }
    }
    Unmount::Busy
}

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

/// Starts the `signals` service when [`block_shutdown_signals`] ran; a no-op
/// for library callers. `busy_lanes` counts handles whose release may still
/// have to reach the journal.
pub(crate) fn start_watcher(mount_point: PathBuf, journal: SharedJournal, busy_lanes: impl Fn() -> usize + Send + 'static) {
    if !BLOCKED.load(Ordering::SeqCst) {
        return;
    }
    let started = crate::bg::spawn_service("signals", move || {
        let set = signal_set();
        let Some(sig) = wait_signal(&set, None) else {
            log::error!("signals: sigwaitinfo failed: {}", std::io::Error::last_os_error());
            return;
        };
        log::warn!("signals: received signal {} — writing the journal and unmounting {}", sig, mount_point.display());
        mutation_journal::save_synchronously(&journal);
        let mut logged_busy = false;
        loop {
            match try_unmount(&mount_point) {
                // The session loop returns; mount_ncfs shuts down from there.
                Unmount::Done => break,
                Unmount::NotMounted => flush_and_exit(&journal, &busy_lanes, "mount already detached"),
                Unmount::Busy => {
                    if !logged_busy {
                        log::warn!("signals: {} is busy — still serving, retrying the unmount every {:?}", mount_point.display(), UNMOUNT_RETRY);
                        logged_busy = true;
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
        log::error!("signals: cannot start the signal watcher: {}", e);
    }
}
