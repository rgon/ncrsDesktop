//! Kernel FUSE passthrough without CAP_SYS_ADMIN in the daemon.
//!
//! Registering a passthrough backing file is the one thing ncrs needs
//! CAP_SYS_ADMIN for. The daemon no longer carries it: `ncrs-passthrough`
//! (crate `ncrs_passthrough`, installed at /usr/libexec/ncrs with
//! `cap_sys_admin=ep`) is spawned per FUSE connection, given a dup of the
//! /dev/fuse descriptor, and does the two backing ioctls on request. fuser
//! reaches it through the vendored "backing broker" hook (PATCHES.md), and
//! [`crate::handoff::close_backing_id`] through [`close_backing_id`].
//!
//! Without the helper, or with a helper that lacks the capability, passthrough
//! is off for the session and reads stay buffered — the same fallback as a
//! daemon without the capability always had.
//!
//! Restart survival: backing ids belong to the connection, not to a process.
//! A planned restart re-executes the daemon, which closes the helper's socket
//! (close-on-exec), so the helper exits; a crash takes it down with
//! PR_SET_PDEATHSIG. The successor spawns a fresh helper on the connection it
//! took over and lets it adopt the ids its predecessor registered, so they can
//! still be closed once their handles are released.

use std::io;
use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ncrs_passthrough::caps::{self, CAP_SYS_ADMIN};
use ncrs_passthrough::client::Helper;

use crate::MutexExt;

/// Where the packages install the helper.
const INSTALLED_HELPER: &str = "/usr/libexec/ncrs/ncrs-passthrough";
/// Overrides the helper path (development, tests). Grants nothing: the helper
/// only gets the capability its own file carries.
const ENV_HELPER: &str = "NCRS_PASSTHROUGH_HELPER";
const HELPER_NAME: &str = "ncrs-passthrough";

struct HelperBroker(Helper);

impl fuser::BackingBroker for HelperBroker {
    fn open(&self, fd: BorrowedFd<'_>) -> io::Result<u32> {
        self.0.open_backing(fd)
    }

    fn close(&self, backing_id: u32) -> io::Result<()> {
        self.0.close_backing(backing_id)
    }
}

static ACTIVE: Mutex<Option<Arc<HelperBroker>>> = Mutex::new(None);

/// The helper to run: `$NCRS_PASSTHROUGH_HELPER`, the installed one, or one
/// next to this executable (a `target/release` build).
fn helper_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(ENV_HELPER).filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let installed = Path::new(INSTALLED_HELPER);
    if installed.is_file() {
        return Some(installed.to_path_buf());
    }
    let sibling = std::env::current_exe().ok()?.parent()?.join(HELPER_NAME);
    sibling.is_file().then_some(sibling)
}

/// Sets passthrough up for the connection `fuse` before the session serves
/// it. `adopt`: backing ids a previous daemon registered on this connection.
/// On failure `capable` is cleared, so opens go straight to buffered reads.
///
/// Must run on the thread that then waits for the session (mount_ncfs): the
/// helper dies with the thread that spawned it.
pub(crate) fn start(fuse: BorrowedFd<'_>, adopt: &[u32], capable: &AtomicBool) {
    stop();
    if caps::effective_has(CAP_SYS_ADMIN) {
        // A leftover `setcap` on this binary (packages before the helper put
        // it there; the current ones remove it). It works, but it is exactly
        // what the helper exists to avoid.
        log::warn!(
            "ncrs itself holds CAP_SYS_ADMIN: FUSE passthrough uses it directly and mounts bypass fusermount3's checks. \
             Only {} should carry it — remove it with: sudo setcap -r {}",
            INSTALLED_HELPER,
            std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "/usr/bin/ncrs".into()),
        );
        return;
    }
    let Some(exe) = helper_path() else {
        capable.store(false, Ordering::Relaxed);
        log::info!("FUSE passthrough unavailable ({} is not installed) — using buffered reads", INSTALLED_HELPER);
        return;
    };
    match Helper::spawn(&exe, fuse, adopt) {
        Ok(h) => {
            log::info!("FUSE passthrough via {} (pid {})", exe.display(), h.pid());
            let b = Arc::new(HelperBroker(h));
            fuser::set_backing_broker(Some(b.clone()));
            *ACTIVE.safe_lock() = Some(b);
        }
        Err(why) => {
            capable.store(false, Ordering::Relaxed);
            log::info!("FUSE passthrough unavailable ({}: {}) — using buffered reads", exe.display(), why);
        }
    }
}

/// Closes a backing id through the helper. None when no helper is in use
/// (the daemon then does the ioctl itself, as with its own capability).
pub(crate) fn close_backing_id(id: u32) -> Option<io::Result<()>> {
    let b = ACTIVE.safe_lock().clone()?;
    Some(b.0.close_backing(id))
}

/// The session ended: let the helper go.
pub(crate) fn stop() {
    let Some(b) = ACTIVE.safe_lock().take() else { return };
    fuser::set_backing_broker(None);
    b.0.shutdown();
}
