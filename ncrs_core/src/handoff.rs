//! Daemon-restart survival, part 1: keeping the kernel's FUSE connection alive
//! while this process goes away, and handing it to the next daemon. The design
//! note is docs/restart-survival.md.
//!
//! The kernel keeps a FUSE mount — and every file descriptor programs hold on
//! it — alive for as long as *some* process holds the connection's /dev/fuse
//! descriptor. When the last one closes, the connection aborts and every
//! access fails ENOTCONN. So the descriptor is kept in one more place:
//!
//! - **systemd service** (`ncrs.service`, `NOTIFY_SOCKET` set): the descriptor
//!   goes into systemd's fd store (`FDSTORE=1`, allowed by the unit's
//!   `FileDescriptorStoreMax=`). systemd keeps it across `Restart=on-failure`
//!   (a crash, an OOM kill, a watchdog kill) and `systemctl restart`, and
//!   passes it back in `LISTEN_FDS`. A SIGTERM from systemd (stop or restart)
//!   drains the request scheduler and exits without unmounting ([`on_sigterm`]).
//! - **planned restart** (IPC `RESTART_HANDOFF`, e.g. after a package upgrade):
//!   the daemon re-executes itself with a dup of the descriptor left open
//!   across exec (`NCRS_HANDOFF_FD`). The PID stays the same, so the service's
//!   main PID (or a GUI's child handle) stays valid. Works with or without
//!   systemd.
//!
//! Without systemd there is no keeper: a crash ends the mount with ENOTCONN, as
//! before restart survival existed. A separate descriptor-holder process could
//! cover that case; it is not implemented (see the design note).
//!
//! While no daemon serves the connection, programs touching the mount block
//! instead of failing (the kernel queues their requests). That is bounded by
//! the unit: `StartLimitBurst=` fails a daemon that keeps dying, and once the
//! unit is failed (or stopped) systemd drops the stored descriptor and the
//! mount fails ENOTCONN. A successor that takes the connection over but cannot
//! start serving it aborts it ([`ReattachGuard`]), so programs get ENOTCONN
//! instead of waiting on a daemon that will never answer.
//!
//! Every reattach is checked against /proc/self/mountinfo: the descriptor must
//! belong to the fuse mount currently on the configured mount point (same
//! device number), never just "some" fuse connection. Nothing here stats the
//! mount point: while nobody serves it, a stat would block forever.
//!
//! The reattached session never sees FUSE_INIT (see PATCHES.md, "fuser:
//! reattach"); requests the previous daemon had read but not answered are
//! re-queued with FUSE_NOTIFY_RESEND (Linux 6.9+, [`send_notify_resend`])
//! before the new session starts reading. What the kernel still references —
//! node ids, file handles — is restored by `state_snapshot`.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::MutexExt;

/// Env vars a re-executing daemon leaves for its successor (see [`planned_restart`]).
const ENV_FD: &str = "NCRS_HANDOFF_FD";
const ENV_DEV: &str = "NCRS_HANDOFF_DEV";
/// Set to "1" when the image that re-executed had the connection in systemd's fd store.
const ENV_STORED: &str = "NCRS_HANDOFF_STORED";

/// fd-store entries are named `ncrsfuse-<major>-<minor>` (systemd forbids `:` in FDNAME).
const FDSTORE_PREFIX: &str = "ncrsfuse-";

/// How long a planned restart or a SIGTERM waits for running requests.
const DRAIN_BUDGET: Duration = Duration::from_secs(3);

/// The kernel's id for one FUSE connection: the device number of its mount
/// (`st_dev` of the mount root, field 3 of /proc/self/mountinfo).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct ConnDev {
    pub(crate) major: u32,
    pub(crate) minor: u32,
}

impl ConnDev {
    fn parse_colon(s: &str) -> Option<Self> {
        let (a, b) = s.split_once(':')?;
        Some(ConnDev { major: a.parse().ok()?, minor: b.parse().ok()? })
    }

    fn to_colon(self) -> String {
        format!("{}:{}", self.major, self.minor)
    }

    fn fdstore_name(self) -> String {
        format!("{}{}-{}", FDSTORE_PREFIX, self.major, self.minor)
    }

    fn from_fdstore_name(name: &str) -> Option<Self> {
        let (a, b) = name.strip_prefix(FDSTORE_PREFIX)?.split_once('-')?;
        Some(ConnDev { major: a.parse().ok()?, minor: b.parse().ok()? })
    }
}

/// Where a reattached connection came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// This process re-executed itself ([`planned_restart`]).
    Exec,
    /// systemd's fd store (`LISTEN_FDS`).
    FdStore,
}

/// A live connection found by [`try_reattach`], not yet handed to a session.
pub(crate) struct Reattach {
    pub(crate) fd: OwnedFd,
    pub(crate) dev: ConnDev,
    pub(crate) source: Source,
    /// systemd's fd store already holds this connection. Passing a stored
    /// descriptor in LISTEN_FDS (or re-executing) does not take it out of the
    /// store, and systemd only recognises a re-sent copy as the same one where
    /// kcmp(2) works (not under Docker's default seccomp profile), so a re-sent
    /// copy can become a second entry — see [`adopt_reattached`].
    pub(crate) stored: bool,
}

impl std::fmt::Debug for Reattach {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reattach").field("dev", &self.dev).field("source", &self.source).finish()
    }
}

/// Who besides this process keeps the connection alive.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Keeper {
    FdStore,
    None,
}

struct Live {
    /// This process's own dup (FD_CLOEXEC), separate from the session's: used for
    /// FUSE_NOTIFY_RESEND, closing inherited backing ids and the next handover.
    fd: OwnedFd,
    dev: ConnDev,
    keeper: Keeper,
}

static LIVE: Mutex<Option<Live>> = Mutex::new(None);

type QuiesceHook = Box<dyn Fn(Duration) -> bool + Send>;
type ResumeHook = Box<dyn Fn() + Send>;
type SnapshotHook = Box<dyn Fn(bool) -> io::Result<()> + Send>;

/// Stops dispatching FUSE requests and waits (up to the budget) for the running
/// ones to finish; true when idle. Installed by the request scheduler.
static QUIESCE: Mutex<Option<QuiesceHook>> = Mutex::new(None);
/// Lifts that pause again (a planned restart that failed).
static RESUME: Mutex<Option<ResumeHook>> = Mutex::new(None);
/// With true, writes the exact per-handle state for the successor; with false,
/// removes any such state, so the successor infers it (state_snapshot).
static SNAPSHOT: Mutex<Option<SnapshotHook>> = Mutex::new(None);

pub(crate) fn set_quiesce_hook(f: impl Fn(Duration) -> bool + Send + 'static) {
    *QUIESCE.safe_lock() = Some(Box::new(f));
}

pub(crate) fn set_resume_hook(f: impl Fn() + Send + 'static) {
    *RESUME.safe_lock() = Some(Box::new(f));
}

pub(crate) fn set_snapshot_hook(f: impl Fn(bool) -> io::Result<()> + Send + 'static) {
    *SNAPSHOT.safe_lock() = Some(Box::new(f));
}

// ── mountinfo ────────────────────────────────────────────────────────────────

/// The device number of the fuse mount currently on top of `mount_point`.
/// Reads /proc/self/mountinfo, which never sends a request to the mount, so it
/// is safe while nobody serves it.
pub(crate) fn fuse_mount_dev(mount_point: &Path) -> Option<ConnDev> {
    let text = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    parse_mountinfo_dev(&text, &mount_point_spellings(mount_point))
}

/// The configured path and, when it differs, its canonical form: mountinfo
/// lists the path the kernel resolved (a symlinked home shows its target).
/// Only the parent is canonicalized — resolving the mount point itself would
/// send a request to the mount, which blocks while nobody serves it.
fn mount_point_spellings(mount_point: &Path) -> Vec<Vec<u8>> {
    let mut out = vec![trim_trailing_slash(mount_point.as_os_str().as_bytes()).to_vec()];
    if let (Some(parent), Some(name)) = (mount_point.parent(), mount_point.file_name()) {
        if let Ok(canon) = parent.canonicalize() {
            let c = canon.join(name).into_os_string().into_vec();
            if !out.contains(&c) {
                out.push(c);
            }
        }
    }
    out
}

fn parse_mountinfo_dev(text: &str, mount_point: &[Vec<u8>]) -> Option<ConnDev> {
    let mut found = None;
    for line in text.lines() {
        let Some((pre, post)) = line.split_once(" - ") else { continue };
        let mut f = pre.split(' ');
        let (Some(_id), Some(_parent), Some(devno), Some(_root), Some(mp)) =
            (f.next(), f.next(), f.next(), f.next(), f.next())
        else {
            continue;
        };
        let mp = unescape_mountinfo(mp);
        if !mount_point.iter().any(|want| *want == mp) {
            continue;
        }
        // Later lines are mounted on top of earlier ones: the last entry for the
        // path is what a lookup there reaches, fuse or not.
        let fstype = post.split(' ').next().unwrap_or("");
        found = if fstype == "fuse" || fstype.starts_with("fuse.") {
            ConnDev::parse_colon(devno)
        } else {
            None
        };
    }
    found
}

fn trim_trailing_slash(b: &[u8]) -> &[u8] {
    if b.len() > 1 && b.ends_with(b"/") {
        &b[..b.len() - 1]
    } else {
        b
    }
}

/// mountinfo escapes space, tab, newline and backslash as `\ooo` octal.
fn unescape_mountinfo(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 4 <= b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            let v = (b[i + 1] - b'0') as u32 * 64 + (b[i + 2] - b'0') as u32 * 8 + (b[i + 3] - b'0') as u32;
            out.push(v as u8);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

// ── fd plumbing ──────────────────────────────────────────────────────────────

fn is_dev_fuse(fd: BorrowedFd<'_>) -> bool {
    // SAFETY: fstat only writes the provided struct.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return false;
    }
    (st.st_mode & libc::S_IFMT) == libc::S_IFCHR && libc::major(st.st_rdev) == 10 && libc::minor(st.st_rdev) == 229
}

fn set_cloexec(fd: RawFd, on: bool) -> io::Result<()> {
    // SAFETY: F_GETFD/F_SETFD on a descriptor this process owns.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let new = if on { flags | libc::FD_CLOEXEC } else { flags & !libc::FD_CLOEXEC };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, new) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Sends `data` in one sendmsg to `name`, with `fd` attached as SCM_RIGHTS.
fn send_with_fd(sock: BorrowedFd<'_>, data: &[u8], fd: Option<BorrowedFd<'_>>, name: (&libc::sockaddr_un, libc::socklen_t)) -> io::Result<()> {
    let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut libc::c_void, iov_len: data.len() };
    // u64 storage keeps the control buffer aligned for cmsghdr.
    let mut cbuf = [0u64; 4];
    // SAFETY: an all-zero msghdr is a valid empty message.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_name = name.0 as *const libc::sockaddr_un as *mut libc::c_void;
    msg.msg_namelen = name.1;
    if let Some(fd) = fd {
        // SAFETY: CMSG_* only compute offsets into `cbuf`, which is larger than
        // CMSG_SPACE(sizeof(int)); the header written is inside it.
        unsafe {
            let space = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as libc::c_uint) as usize;
            debug_assert!(space <= std::mem::size_of_val(&cbuf));
            msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
            msg.msg_controllen = space as _;
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as libc::c_uint) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(c) as *mut RawFd, fd.as_raw_fd());
        }
    }
    // SAFETY: msg points at live buffers for the duration of the call.
    let n = unsafe { libc::sendmsg(sock.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n as usize != data.len() {
        return Err(io::Error::new(io::ErrorKind::WriteZero, "short sendmsg"));
    }
    Ok(())
}

// ── kernel notifications ─────────────────────────────────────────────────────

/// `fuse_notify_code::FUSE_NOTIFY_RESEND` (include/uapi/linux/fuse.h, 7.40).
const FUSE_NOTIFY_RESEND: i32 = 7;

/// The 16-byte `fuse_out_header` of a payload-less notification: `unique == 0`
/// marks a notification, `error` carries its code.
fn notify_header(code: i32) -> [u8; 16] {
    let mut h = [0u8; 16];
    h[0..4].copy_from_slice(&16u32.to_ne_bytes());
    h[4..8].copy_from_slice(&code.to_ne_bytes());
    h
}

/// Asks the kernel to re-queue every request a previous reader took off this
/// connection but never answered (Linux 6.9+); they arrive again with
/// `FUSE_UNIQUE_RESEND` set in their unique id, which replies echo unchanged.
/// Older kernels answer EINVAL: those requests then stay pending until the
/// calling program is interrupted.
pub(crate) fn send_notify_resend(fd: BorrowedFd<'_>) -> io::Result<()> {
    let h = notify_header(FUSE_NOTIFY_RESEND);
    // Header-only write: fs/fuse/dev.c fuse_notify() takes no payload for
    // FUSE_NOTIFY_RESEND (exercised by e2e scenario 32's watchdog step).
    // SAFETY: writes the 16 bytes of `h`.
    let n = unsafe { libc::write(fd.as_raw_fd(), h.as_ptr() as *const libc::c_void, h.len()) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `FUSE_DEV_IOC_BACKING_CLOSE` = `_IOW(229, 2, uint32_t)`.
const FUSE_DEV_IOC_BACKING_CLOSE: libc::c_ulong = (1 << 30) | (4 << 16) | (229 << 8) | 2;

/// Closes a passthrough backing id a previous daemon registered on this
/// connection, once the last handle it restored for it is released.
pub(crate) fn close_backing_id(id: u32) {
    let g = LIVE.safe_lock();
    let Some(live) = g.as_ref() else { return };
    // SAFETY: the ioctl reads one u32 from `id`.
    let rc = unsafe { libc::ioctl(live.fd.as_raw_fd(), FUSE_DEV_IOC_BACKING_CLOSE as _, &id as *const u32) };
    if rc < 0 {
        log::debug!("handoff: closing inherited backing id {} failed: {}", id, io::Error::last_os_error());
    }
}

// ── systemd ──────────────────────────────────────────────────────────────────

fn notify_socket_addr() -> Option<(libc::sockaddr_un, libc::socklen_t)> {
    let path = std::env::var_os("NOTIFY_SOCKET")?;
    let bytes = path.as_bytes();
    if bytes.is_empty() || !(bytes[0] == b'/' || bytes[0] == b'@') {
        return None;
    }
    // SAFETY: an all-zero sockaddr_un is valid.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= addr.sun_path.len() {
        return None;
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (i, b) in bytes.iter().enumerate() {
        addr.sun_path[i] = *b as libc::c_char;
    }
    if bytes[0] == b'@' {
        addr.sun_path[0] = 0; // abstract namespace
    }
    let len = std::mem::size_of::<libc::sa_family_t>() + bytes.len();
    Some((addr, len as libc::socklen_t))
}

/// sd_notify(3) with an optional descriptor. Ok(false) when not under systemd.
pub(crate) fn sd_notify(state: &str, fd: Option<BorrowedFd<'_>>) -> io::Result<bool> {
    let Some((addr, len)) = notify_socket_addr() else { return Ok(false) };
    let sock = UnixDatagram::unbound()?;
    send_with_fd(sock.as_fd(), state.as_bytes(), fd, (&addr, len))?;
    Ok(true)
}

fn under_systemd_service() -> bool {
    notify_socket_addr().is_some()
}

/// Descriptors systemd passed in `LISTEN_FDS` that look like ours, as (fd, name).
/// Takes ownership of every passed descriptor and unsets the LISTEN_* variables.
fn take_listen_fds() -> Vec<(OwnedFd, String)> {
    let pid_ok = std::env::var("LISTEN_PID").ok().and_then(|p| p.parse::<u32>().ok()) == Some(std::process::id());
    let count: i32 = std::env::var("LISTEN_FDS").ok().and_then(|n| n.parse().ok()).unwrap_or(0);
    let names: Vec<String> = std::env::var("LISTEN_FDNAMES").map(|n| n.split(':').map(str::to_string).collect()).unwrap_or_default();
    std::env::remove_var("LISTEN_PID");
    std::env::remove_var("LISTEN_FDS");
    std::env::remove_var("LISTEN_FDNAMES");
    if !pid_ok || count <= 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for i in 0..count {
        let raw = 3 + i; // SD_LISTEN_FDS_START
        let _ = set_cloexec(raw, true);
        // SAFETY: systemd hands these descriptors to this process to own.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let name = names.get(i as usize).cloned().unwrap_or_default();
        if name.starts_with(FDSTORE_PREFIX) {
            out.push((fd, name));
        }
        // Anything else is not ours to keep; dropping closes it.
    }
    out
}

// ── finding a connection to reattach ─────────────────────────────────────────

/// True in the image a planned restart exec'd into (before [`try_reattach`]
/// consumes the hand-off variables). The process already carries everything
/// the first image installed once per process, such as its seccomp filter.
pub(crate) fn is_exec_successor() -> bool {
    std::env::var_os(ENV_FD).is_some()
}

/// The live connection a previous daemon left for this one, if any: from this
/// process's own re-exec or systemd's fd store, in that order. Only returns a
/// descriptor that is /dev/fuse and belongs to the fuse mount on `mount_point`
/// right now.
pub(crate) fn try_reattach(mount_point: &Path) -> Option<Reattach> {
    if let Some(r) = from_exec_env(mount_point) {
        return Some(r);
    }
    from_fd_store(mount_point)
}

fn validated(fd: &OwnedFd, dev: ConnDev, mount_point: &Path) -> bool {
    if !is_dev_fuse(fd.as_fd()) {
        log::warn!("handoff: inherited descriptor is not /dev/fuse — ignoring it");
        return false;
    }
    match fuse_mount_dev(mount_point) {
        Some(d) if d == dev => true,
        other => {
            log::warn!(
                "handoff: connection {} is not the fuse mount on {} (found {:?}) — mounting afresh",
                dev.to_colon(), mount_point.display(), other.map(ConnDev::to_colon),
            );
            false
        }
    }
}

fn from_exec_env(mount_point: &Path) -> Option<Reattach> {
    let fd_s = std::env::var(ENV_FD).ok();
    let dev_s = std::env::var(ENV_DEV).ok();
    let stored = std::env::var(ENV_STORED).is_ok_and(|v| v == "1");
    // Children (fusermount, a further re-exec) must not inherit them.
    std::env::remove_var(ENV_FD);
    std::env::remove_var(ENV_DEV);
    std::env::remove_var(ENV_STORED);
    let raw: RawFd = fd_s?.parse().ok()?;
    let _ = set_cloexec(raw, true);
    // SAFETY: the previous image of this process left this descriptor open for us.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let dev = dev_s.as_deref().and_then(ConnDev::parse_colon)?;
    if !validated(&fd, dev, mount_point) {
        return None;
    }
    log::info!("handoff: took over FUSE connection {} from this daemon's previous image", dev.to_colon());
    Some(Reattach { fd, dev, source: Source::Exec, stored })
}

fn from_fd_store(mount_point: &Path) -> Option<Reattach> {
    let mut found: Option<Reattach> = None;
    for (fd, name) in take_listen_fds() {
        let Some(dev) = ConnDev::from_fdstore_name(&name) else { continue };
        let taken = found.as_ref().map(|r| r.dev);
        if taken.is_none() && validated(&fd, dev, mount_point) {
            found = Some(Reattach { fd, dev, source: Source::FdStore, stored: true });
        } else if store_entry_is_stale(dev, taken) {
            // A connection whose mount is gone: drop it from the store too.
            let _ = sd_notify(&format!("FDSTOREREMOVE=1\nFDNAME={}", name), None);
        }
        // A second copy of the connection just taken: only this process's copy
        // is closed. FDSTOREREMOVE goes by name and would drop the stored copy
        // the successor of this daemon needs as well.
    }
    if let Some(r) = &found {
        log::info!("handoff: took over FUSE connection {} from the systemd fd store", r.dev.to_colon());
    }
    found
}

/// Whether a LISTEN_FDS entry for `dev` should be removed from the store, given
/// the connection already taken from it (if any): yes unless it is a copy of
/// that same connection.
fn store_entry_is_stale(dev: ConnDev, taken: Option<ConnDev>) -> bool {
    taken != Some(dev)
}

/// Gives up on a connection [`try_reattach`] found (its state could not be
/// restored): drops it from the fd store and aborts it, so the mount point
/// reads ENOTCONN, which `prepare_mount_point` then detaches.
pub(crate) fn abandon(r: Reattach) {
    let dev = r.dev;
    drop(r);
    give_up(dev);
}

fn give_up(dev: ConnDev) {
    log::warn!("handoff: abandoning FUSE connection {}; programs holding files on it get ENOTCONN", dev.to_colon());
    if under_systemd_service() {
        let _ = sd_notify(&format!("FDSTOREREMOVE=1\nFDNAME={}", dev.fdstore_name()), None);
    }
    // The kernel aborts a connection when its last descriptor closes, but
    // systemd handles FDSTOREREMOVE on its own time. Until then a stat of the
    // mount point — prepare_mount_point's first step — would block forever, so
    // abort it outright.
    abort_connection(dev);
}

/// Armed once a reattached connection is being set up: if this daemon fails
/// before its session serves the connection (a config, keyring or filesystem
/// setup error), dropping the guard aborts the connection instead of leaving it
/// in the fd store for a successor that would hit the same error — programs
/// on the mount then get ENOTCONN rather than blocking.
pub(crate) struct ReattachGuard(Option<ConnDev>);

// When the armed guard was created, and for which connection: programs on the mount
// wait while it stays armed.
static TAKEOVER_SINCE: Mutex<Option<(Instant, ConnDev)>> = Mutex::new(None);

/// How long this daemon has held a taken-over connection without serving it yet.
pub(crate) fn takeover_pending_for() -> Option<Duration> {
    TAKEOVER_SINCE.safe_lock().map(|(t, _)| t.elapsed())
}

/// The watchdog's way out of a takeover that never gets to serving (a hung keyring,
/// auth command or network call during startup): gives the connection up, so programs
/// on the mount get ENOTCONN now instead of waiting on watchdog kills that come too far
/// apart for StartLimitBurst ever to end it, and exits non-zero so systemd starts a
/// fresh daemon on an empty fd store.
pub(crate) fn give_up_stalled_takeover() {
    // Taken under the lock: a takeover that reached serving meanwhile disarmed its guard,
    // and then there is nothing to give up and no reason to exit.
    let Some((_, dev)) = TAKEOVER_SINCE.safe_lock().take() else { return };
    give_up(dev);
    std::process::exit(1)
}

impl ReattachGuard {
    pub(crate) fn new(dev: Option<ConnDev>) -> Self {
        if let Some(d) = dev {
            *TAKEOVER_SINCE.safe_lock() = Some((Instant::now(), d));
        }
        ReattachGuard(dev)
    }

    /// The session serves the connection now.
    pub(crate) fn disarm(&mut self) {
        self.0 = None;
        *TAKEOVER_SINCE.safe_lock() = None;
    }
}

impl Drop for ReattachGuard {
    fn drop(&mut self) {
        *TAKEOVER_SINCE.safe_lock() = None;
        if let Some(dev) = self.0.take() {
            log::warn!("handoff: this daemon failed before serving the connection it took over");
            give_up(dev);
        }
    }
}

/// Writes fusectl's `abort` for this connection: every pending and future
/// request fails ENOTCONN, whoever still holds a descriptor.
fn abort_connection(dev: ConnDev) {
    // fusectl names a connection by the kernel's dev_t, MKDEV(major, minor).
    // UNVERIFIED: fusectl mounted at /sys/fs/fuse/connections (systemd does) and
    // its directory named "%u" of fc->dev (fs/fuse/control.c fuse_ctl_add_conn).
    let id = ((dev.major as u64) << 20) | dev.minor as u64;
    let path = format!("/sys/fs/fuse/connections/{}/abort", id);
    if let Err(e) = std::fs::write(&path, b"1") {
        log::warn!("handoff: cannot abort FUSE connection {} via {}: {}", dev.to_colon(), path, e);
    }
}

// ── adopting the connection this daemon serves ───────────────────────────────

/// The binary to re-exec: the path this process was started from, even if a
/// package upgrade has since replaced it (`/proc/self/exe` would still be the
/// old, deleted image).
fn restart_exe() -> PathBuf {
    if let Ok(p) = std::env::current_exe() {
        let s = p.as_os_str().as_bytes();
        let trimmed = s.strip_suffix(b" (deleted)").unwrap_or(s);
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(trimmed));
        if path.is_file() {
            return path;
        }
    }
    PathBuf::from("/proc/self/exe")
}

/// Called right after a fresh mount: keeps a dup of the session's connection
/// (for a planned restart) and, under systemd, parks one in the fd store so a
/// crash does not end the mount. Returns the connection's id, or None when it
/// cannot be identified (restart survival is then off for this mount, and
/// nothing else changes).
pub(crate) fn adopt_fresh(session_fd: BorrowedFd<'_>, mount_point: &Path) -> Option<ConnDev> {
    let Some(dev) = fuse_mount_dev(mount_point) else {
        log::warn!("handoff: cannot identify the FUSE connection on {} — restart survival disabled", mount_point.display());
        return None;
    };
    let fd = match session_fd.try_clone_to_owned() {
        Ok(fd) => fd,
        Err(e) => {
            log::warn!("handoff: cannot dup the FUSE descriptor ({}) — restart survival disabled", e);
            return None;
        }
    };
    let keeper = new_keeper(fd.as_fd(), dev);
    *LIVE.safe_lock() = Some(Live { fd, dev, keeper });
    Some(dev)
}

/// Called after a reattached session is created from the connection's twin.
/// When the store already holds it (`stored`: taken from LISTEN_FDS, or a
/// re-exec of an image that had stored it) nothing is sent: systemd keeps
/// stored descriptors when it passes them on, and a re-sent copy is only
/// deduplicated where kcmp(2) is allowed — under Docker's default seccomp
/// profile the real systemd 255 kept it as a second entry. Otherwise (no
/// systemd, or a re-exec from an image that predates this flag) it is stored
/// as after a fresh mount.
pub(crate) fn adopt_reattached(own: OwnedFd, dev: ConnDev, stored: bool) {
    let keeper = if reattached_keeper_is_store(stored, under_systemd_service()) {
        log::info!("handoff: FUSE connection {} stays in the systemd fd store", dev.to_colon());
        Keeper::FdStore
    } else {
        new_keeper(own.as_fd(), dev)
    };
    *LIVE.safe_lock() = Some(Live { fd: own, dev, keeper });
}

/// A reattached connection needs no new FDSTORE=1 when the store holds it and
/// this process still talks to that store.
fn reattached_keeper_is_store(stored: bool, under_systemd: bool) -> bool {
    stored && under_systemd
}

fn new_keeper(fd: BorrowedFd<'_>, dev: ConnDev) -> Keeper {
    match sd_notify(&format!("FDSTORE=1\nFDNAME={}", dev.fdstore_name()), Some(fd)) {
        Ok(true) => {
            log::info!("handoff: FUSE connection {} kept in the systemd fd store", dev.to_colon());
            Keeper::FdStore
        }
        Ok(false) => {
            log::info!(
                "handoff: not running as a systemd service — a crash ends the mount (planned restarts still keep it; see docs/restart-survival.md)"
            );
            Keeper::None
        }
        Err(e) => {
            log::warn!("handoff: systemd fd store refused the FUSE descriptor ({}) — a crash will end the mount", e);
            Keeper::None
        }
    }
}

/// Before a reattached session starts reading: re-queue what the previous
/// daemon left unanswered. Sent first so nothing this daemon reads itself is
/// in what the kernel resends (that would run twice).
pub(crate) fn resend_in_flight() {
    let g = LIVE.safe_lock();
    let Some(live) = g.as_ref() else { return };
    match send_notify_resend(live.fd.as_fd()) {
        Ok(()) => log::info!("handoff: asked the kernel to resend requests the previous daemon left unanswered"),
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => log::warn!(
            "handoff: kernel lacks FUSE_NOTIFY_RESEND (< 6.9) — requests the previous daemon was serving stay blocked until their programs are interrupted"
        ),
        Err(e) => log::warn!("handoff: FUSE_NOTIFY_RESEND failed: {}", e),
    }
}

/// The session ended for good (unmounted, or the connection was aborted): tell
/// the keeper to let go, so the connection is not kept for a successor that
/// would find nothing to serve.
pub(crate) fn release_connection() {
    let Some(live) = LIVE.safe_lock().take() else { return };
    if live.keeper == Keeper::FdStore {
        let _ = sd_notify(&format!("FDSTOREREMOVE=1\nFDNAME={}", live.dev.fdstore_name()), None);
    }
}

/// Drains running requests for a hand-over and leaves the successor the best
/// state there is: the exact handle snapshot when nothing is still running,
/// otherwise none (a snapshot taken mid-request could miss that request's
/// write, so the successor infers dirtiness from the staging files instead).
fn drain_for_handover() -> io::Result<bool> {
    let drained = match QUIESCE.safe_lock().as_ref() {
        Some(q) => q(DRAIN_BUDGET),
        // Only without the scheduler (never in the daemon).
        None => false,
    };
    if !drained {
        log::warn!(
            "handoff: requests still running after {}s — handing over without the exact handle snapshot; the kernel resends them",
            DRAIN_BUDGET.as_secs()
        );
    }
    crate::state_snapshot::flush_log();
    if let Some(snap) = SNAPSHOT.safe_lock().as_ref() {
        snap(drained)?;
    }
    Ok(drained)
}

/// Undoes [`drain_for_handover`] after a hand-over that did not happen.
fn undo_handover() {
    if let Some(snap) = SNAPSHOT.safe_lock().as_ref() {
        if let Err(e) = snap(false) {
            log::warn!("handoff: cannot remove the handle snapshot: {}", e);
        }
    }
    if let Some(r) = RESUME.safe_lock().as_ref() {
        r();
    }
}

// ── planned restart (re-exec) ────────────────────────────────────────────────

/// IPC entry point: re-execute this daemon in place, keeping the mount and every
/// program's open files. Replies before the exec happens.
pub fn request_planned_restart() -> Result<(), String> {
    if LIVE.safe_lock().is_none() {
        return Err("no FUSE connection to hand over (not mounted, or restart survival unavailable)".into());
    }
    crate::bg::spawn_service("handoff-restart", || {
        // Let the IPC reply reach the client before this image is replaced.
        std::thread::sleep(Duration::from_millis(150));
        if let Err(e) = planned_restart() {
            log::error!("handoff: planned restart failed, still running: {}", e);
        }
    })
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Re-executes this daemon with the connection left open across exec. Only
/// returns on failure, with the descriptor, the snapshot and the scheduler
/// back as they were.
fn planned_restart() -> io::Result<()> {
    if let Err(e) = drain_for_handover() {
        undo_handover();
        return Err(e);
    }
    let (fd, dev, stored) = match LIVE.safe_lock().as_ref() {
        Some(l) => (l.fd.as_raw_fd(), l.dev, l.keeper == Keeper::FdStore),
        None => {
            undo_handover();
            return Err(io::Error::other("not mounted"));
        }
    };
    if let Err(e) = set_cloexec(fd, false) {
        undo_handover();
        return Err(e);
    }
    let exe = restart_exe();
    log::info!("handoff: re-executing {} with FUSE connection {} (fd {})", exe.display(), dev.to_colon(), fd);
    let mut set = vec![(ENV_FD, fd.to_string()), (ENV_DEV, dev.to_colon())];
    if stored {
        set.push((ENV_STORED, "1".to_string()));
    }
    let err = exec_in_place(&exe, &set);
    // Still here: exec failed. A later crash restore must not trust a snapshot
    // of this moment, and the paused scheduler must serve again.
    let _ = set_cloexec(fd, true);
    undo_handover();
    Err(err)
}

/// execve(2) of `exe` with this process's arguments and environment plus `set`, done
/// directly rather than through std's Command::exec: that resets SIGPIPE to its default
/// and clears the calling thread's signal mask before exec, so a SIGPIPE or SIGTERM in
/// that window (or for good, if the exec then fails) would end the daemon "cleanly",
/// which systemd does not restart. Kept as is, the new image also starts with SIGTERM
/// already blocked. Only returns on failure.
fn exec_in_place(exe: &Path, set: &[(&str, String)]) -> io::Error {
    use std::ffi::CString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let cstr = |b: Vec<u8>| CString::new(b).map_err(|_| io::Error::other("NUL byte in an exec argument"));
    // Never inherited: the handover variables are set fresh, and systemd's belong to the
    // start that handed them over.
    let drop_var = |k: &std::ffi::OsStr| {
        [ENV_FD, ENV_DEV, ENV_STORED, "LISTEN_FDS", "LISTEN_PID", "LISTEN_FDNAMES"].iter().any(|n| k == std::ffi::OsStr::new(n))
    };
    let built = (|| -> io::Result<(CString, Vec<CString>, Vec<CString>)> {
        let path = cstr(exe.as_os_str().as_bytes().to_vec())?;
        let mut argv = vec![path.clone()];
        for a in std::env::args_os().skip(1) {
            argv.push(cstr(a.into_vec())?);
        }
        let mut envp = Vec::new();
        for (k, v) in std::env::vars_os().filter(|(k, _)| !drop_var(k)) {
            let mut kv = k.into_vec();
            kv.push(b'=');
            kv.extend(v.into_vec());
            envp.push(cstr(kv)?);
        }
        for (k, v) in set {
            envp.push(cstr(format!("{}={}", k, v).into_bytes())?);
        }
        Ok((path, argv, envp))
    })();
    let (path, argv, envp) = match built {
        Ok(b) => b,
        Err(e) => return e,
    };
    let mut argp: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    argp.push(std::ptr::null());
    let mut envpp: Vec<*const libc::c_char> = envp.iter().map(|e| e.as_ptr()).collect();
    envpp.push(std::ptr::null());
    // SAFETY: every pointer is to a NUL-terminated CString alive across the call, and
    // both arrays end in a null pointer as execve requires.
    unsafe { libc::execve(path.as_ptr(), argp.as_ptr(), envpp.as_ptr()) };
    io::Error::last_os_error()
}

// ── SIGTERM (systemd stop / restart) ─────────────────────────────────────────

fn term_set() -> libc::sigset_t {
    // SAFETY: sigemptyset/sigaddset initialise the set they are given.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        set
    }
}

/// Blocks SIGTERM in the calling thread, and so in every thread it starts
/// later. Call first thing in `main`, before any thread exists; the
/// `sigterm` service thread then takes it with sigwait. (jemalloc's
/// background threads block every signal themselves; spawned children get
/// an empty mask from std::process.)
pub fn block_sigterm() {
    let set = term_set();
    // SAFETY: changes this thread's signal mask only.
    let rc = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) };
    if rc != 0 {
        log::warn!("cannot block SIGTERM ({}) — a systemd stop kills the daemon outright", io::Error::from_raw_os_error(rc));
    }
}

fn sigterm_blocked() -> bool {
    // SAFETY: reads this thread's signal mask into `cur`.
    unsafe {
        let mut cur: libc::sigset_t = std::mem::zeroed();
        libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut cur) == 0 && libc::sigismember(&cur, libc::SIGTERM) == 1
    }
}

/// Starts the `sigterm` service thread, if `main` blocked SIGTERM (an embedder
/// that did not keeps its own signal handling).
pub(crate) fn spawn_sigterm_thread() {
    if !sigterm_blocked() {
        return;
    }
    let r = crate::bg::spawn_service("sigterm", || loop {
        let set = term_set();
        let mut sig: libc::c_int = 0;
        // SAFETY: waits for a signal in `set`, which every thread blocks.
        if unsafe { libc::sigwait(&set, &mut sig) } == 0 && sig == libc::SIGTERM {
            on_sigterm();
        }
    });
    if let Err(e) = r {
        log::warn!("cannot start the SIGTERM thread ({}) — a systemd stop kills the daemon outright", e);
        // SAFETY: changes this thread's signal mask only; with no thread to take
        // it, SIGTERM must keep its default action.
        unsafe {
            let set = term_set();
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
        }
    }
}

/// systemd stops or restarts the service. With the connection in the fd store,
/// this daemon hands over like a planned restart — drain, snapshot only if
/// drained, flush the log — and exits 0 *without unmounting*: the successor
/// (a restart) takes the connection from the store, and a plain stop lets
/// systemd drop the store, which ends the mount. Without a keeper, SIGTERM
/// does what it always did: the default action ends the process.
fn on_sigterm() {
    let keeper = LIVE.safe_lock().as_ref().map(|l| l.keeper);
    if keeper != Some(Keeper::FdStore) {
        log::info!("SIGTERM: exiting (no fd store keeps the mount)");
        // SAFETY: restores the default action and re-raises in this thread,
        // which ends the process exactly as an unhandled SIGTERM would.
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            let set = term_set();
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
            libc::raise(libc::SIGTERM);
        }
        std::process::exit(143);
    }
    let _ = sd_notify("STOPPING=1", None);
    match drain_for_handover() {
        Ok(true) => log::info!("SIGTERM: drained; exact handle snapshot written — exiting with the FUSE connection kept in the systemd fd store"),
        Ok(false) => log::info!("SIGTERM: exiting with the FUSE connection kept in the systemd fd store (no handle snapshot)"),
        Err(e) => log::warn!("SIGTERM: handle snapshot failed ({}) — exiting with the FUSE connection kept in the systemd fd store", e),
    }
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTINFO: &str = "\
22 1 0:21 / /proc rw,nosuid shared:12 - proc proc rw
95 29 0:73 / /home/u/Next\\040cloud rw,nosuid,nodev,relatime shared:50 - fuse ncrs rw,user_id=1000,group_id=1000,default_permissions
96 29 0:74 / /mnt/plain rw shared:51 - ext4 /dev/sda1 rw
97 29 0:75 / /mnt/stacked rw shared:52 - fuse.sshfs host: rw
98 97 0:76 / /mnt/stacked rw shared:53 - tmpfs tmpfs rw
";

    fn one(p: &str) -> Vec<Vec<u8>> {
        mount_point_spellings(Path::new(p))
    }

    #[test]
    fn mountinfo_finds_the_fuse_mount_with_escaped_path() {
        let d = parse_mountinfo_dev(MOUNTINFO, &one("/home/u/Next cloud")).unwrap();
        assert_eq!(d, ConnDev { major: 0, minor: 73 });
        // A trailing slash in the configured path still matches.
        assert_eq!(parse_mountinfo_dev(MOUNTINFO, &one("/home/u/Next cloud/")), Some(d));
    }

    #[test]
    fn mountinfo_ignores_non_fuse_and_covered_mounts() {
        assert_eq!(parse_mountinfo_dev(MOUNTINFO, &one("/mnt/plain")), None);
        // fuse.sshfs mounted first, then covered by a tmpfs: a lookup reaches the tmpfs.
        assert_eq!(parse_mountinfo_dev(MOUNTINFO, &one("/mnt/stacked")), None);
        assert_eq!(parse_mountinfo_dev(MOUNTINFO, &one("/nowhere")), None);
    }

    #[test]
    fn mountinfo_matches_a_mount_point_configured_through_a_symlink() {
        // ~/Nextcloud where ~ is a symlink: mountinfo lists the resolved path.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("home");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mp = link.join("Nextcloud");
        let listed = real.join("Nextcloud");
        let text = format!("95 29 0:80 / {} rw - fuse ncrs rw\n", listed.display());
        let spellings = mount_point_spellings(&mp);
        assert_eq!(spellings.len(), 2, "literal and canonical spelling");
        assert_eq!(parse_mountinfo_dev(&text, &spellings), Some(ConnDev { major: 0, minor: 80 }));
        // The mount point itself is never resolved: it does not even exist here.
        assert!(!mp.exists());
    }

    #[test]
    fn unescape_handles_all_mountinfo_escapes() {
        assert_eq!(unescape_mountinfo("a\\040b\\011c\\012d\\134e"), b"a b\tc\nd\\e".to_vec());
        // Not an escape: left as is.
        assert_eq!(unescape_mountinfo("x\\09"), b"x\\09".to_vec());
    }

    #[test]
    fn conn_dev_round_trips_through_env_and_fdstore_names() {
        let d = ConnDev { major: 0, minor: 73 };
        assert_eq!(ConnDev::parse_colon(&d.to_colon()), Some(d));
        assert_eq!(d.fdstore_name(), "ncrsfuse-0-73");
        assert!(!d.fdstore_name().contains(':'), "systemd rejects ':' in FDNAME");
        assert_eq!(ConnDev::from_fdstore_name(&d.fdstore_name()), Some(d));
        assert_eq!(ConnDev::from_fdstore_name("other-0-73"), None);
    }

    #[test]
    fn a_second_copy_of_the_taken_connection_is_not_removed_from_the_store() {
        let a = ConnDev { major: 0, minor: 327 };
        let b = ConnDev { major: 0, minor: 99 };
        // Nothing taken yet (the entry failed validation): stale.
        assert!(store_entry_is_stale(a, None));
        // The same connection again (systemd kept a re-sent copy as a second
        // entry): removing it by name would drop the live copy too.
        assert!(!store_entry_is_stale(a, Some(a)));
        // A different connection whose mount is gone: stale.
        assert!(store_entry_is_stale(b, Some(a)));
    }

    #[test]
    fn a_reattached_connection_already_in_the_store_is_not_sent_again() {
        assert!(reattached_keeper_is_store(true, true));
        // Not stored (no systemd before the re-exec, or an older image): store it now.
        assert!(!reattached_keeper_is_store(false, true));
        // No notify socket: nothing to keep it in.
        assert!(!reattached_keeper_is_store(true, false));
    }

    #[test]
    fn resend_notification_is_a_bare_out_header() {
        let h = notify_header(FUSE_NOTIFY_RESEND);
        assert_eq!(u32::from_ne_bytes(h[0..4].try_into().unwrap()), 16, "len covers the header only");
        assert_eq!(i32::from_ne_bytes(h[4..8].try_into().unwrap()), 7);
        assert_eq!(u64::from_ne_bytes(h[8..16].try_into().unwrap()), 0, "unique 0 marks a notification");
    }

    #[test]
    fn backing_close_ioctl_number_matches_the_uapi_header() {
        // _IOW(229, 2, uint32_t) as computed by <asm-generic/ioctl.h>.
        assert_eq!(FUSE_DEV_IOC_BACKING_CLOSE, 0x4004_e502);
    }

    #[test]
    fn sd_notify_datagram_carries_the_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify");
        let server = UnixDatagram::bind(&path).unwrap();
        // SAFETY: an all-zero sockaddr_un is valid.
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let b = path.as_os_str().as_bytes();
        for (i, c) in b.iter().enumerate() {
            addr.sun_path[i] = *c as libc::c_char;
        }
        let len = (std::mem::size_of::<libc::sa_family_t>() + b.len()) as libc::socklen_t;
        let f: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        let sock = UnixDatagram::unbound().unwrap();
        send_with_fd(sock.as_fd(), b"FDSTORE=1\nFDNAME=ncrsfuse-0-1", Some(f.as_fd()), (&addr, len)).unwrap();
        let mut buf = [0u8; 64];
        let mut cbuf = [0u64; 8];
        let mut iov = libc::iovec { iov_base: buf.as_mut_ptr() as *mut libc::c_void, iov_len: buf.len() };
        // SAFETY: an all-zero msghdr is a valid empty message; buffers outlive the call.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = std::mem::size_of_val(&cbuf) as _;
        let n = unsafe { libc::recvmsg(server.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
        assert_eq!(&buf[..n as usize], b"FDSTORE=1\nFDNAME=ncrsfuse-0-1");
        // SAFETY: reading the control message the kernel just wrote.
        let got = unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            assert!(!c.is_null(), "descriptor attached");
            assert_eq!(((*c).cmsg_level, (*c).cmsg_type), (libc::SOL_SOCKET, libc::SCM_RIGHTS));
            OwnedFd::from_raw_fd(std::ptr::read_unaligned(libc::CMSG_DATA(c) as *const RawFd))
        };
        assert_ne!(got.as_raw_fd(), f.as_raw_fd(), "the receiver gets its own descriptor");
    }

    #[test]
    fn dev_null_is_not_dev_fuse() {
        let f: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        assert!(!is_dev_fuse(f.as_fd()));
    }
}
