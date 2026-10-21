//! Daemon-restart survival, part 1: keeping the kernel's FUSE connection alive
//! while this process goes away, and handing it to the next daemon.
//!
//! The kernel keeps a FUSE mount — and every file descriptor programs hold on
//! it — alive for as long as *some* process holds the connection's /dev/fuse
//! descriptor. When the last one closes, the connection aborts and every
//! access fails ENOTCONN. So the descriptor is kept in one more place, chosen
//! by how the daemon can go away:
//!
//! - **planned restart** (IPC `RESTART_HANDOFF`, `ncrs-ctl restart`, e.g. after
//!   a package upgrade): the daemon re-executes itself with a dup of the
//!   descriptor left open across exec (`NCRS_HANDOFF_FD`). The PID stays the
//!   same, so the GUI's child handle, the `ncrs.scope` and a service's main PID
//!   all stay valid.
//! - **crash, GUI-spawned daemon** (`ncrs.scope`, or a plain child when
//!   systemd-run is unavailable): a tiny holder process (`ncrs --fd-holder`,
//!   [`run_holder`]) keeps a dup. When the daemon's lease connection to it
//!   drops without a release, it respawns the daemon with its original argv
//!   and hands the descriptor over (SCM_RIGHTS). The holder lives in the
//!   daemon's own scope/cgroup: stopping or killing the *scope* (`systemctl
//!   --user stop ncrs.scope`, systemd-oomd, logout) takes it down with the
//!   daemon — the deliberate "really stop" — and the mount then fails ENOTCONN
//!   exactly as before. A crash, panic or kernel OOM kill of the daemon alone
//!   leaves it running.
//! - **systemd service** (`ncrs.service`, `NOTIFY_SOCKET` set): the unit's
//!   default KillMode=control-group would kill a holder along with the daemon
//!   on every restart, so the descriptor goes into systemd's fd store instead
//!   (`FDSTORE=1`, the unit allows it with `FileDescriptorStoreMax=`). systemd
//!   keeps it across `Restart=on-failure` and `systemctl restart`, and passes
//!   it back in `LISTEN_FDS`.
//!
//! While no daemon serves the connection, programs touching the mount block
//! instead of failing (the kernel queues their requests). That is bounded by
//! the holder's [`ORPHAN_GRACE`]: if nothing takes the connection over in
//! time it lets go, and the mount fails ENOTCONN as it does today.
//!
//! Every reattach is checked against /proc/self/mountinfo: the descriptor must
//! belong to the fuse mount currently on the configured mount point (same
//! device number), never just "some" fuse connection. Nothing here stats the
//! mount point: while nobody serves it, a stat would block forever.
//!
//! The reattached session never sees FUSE_INIT (see PATCHES.md, "fuser:
//! reattach"); requests the previous daemon had read but not answered are
//! re-queued with FUSE_NOTIFY_RESEND (Linux 6.9+, [`send_notify_resend`]).
//! What the kernel still references — node ids, file handles — is restored by
//! `state_snapshot`.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::MutexExt;

/// Env vars a re-executing daemon leaves for its successor (see [`planned_restart`]).
const ENV_FD: &str = "NCRS_HANDOFF_FD";
const ENV_DEV: &str = "NCRS_HANDOFF_DEV";
const ENV_LEASE_FD: &str = "NCRS_HOLDER_LEASE_FD";

/// fd-store entries are named `ncrsfuse-<major>-<minor>` (systemd forbids `:` in FDNAME).
const FDSTORE_PREFIX: &str = "ncrsfuse-";

/// How long the holder keeps a connection no daemon serves before letting it
/// go. Programs on the mount block meanwhile, so this is a trade between a
/// slow restart (keyring unlock, network) and a hung file manager.
pub(crate) const ORPHAN_GRACE: Duration = Duration::from_secs(60);
/// Wait before respawning, so a daemon restarted by someone else (the GUI)
/// gets the connection instead of racing a second one.
const RESPAWN_DELAY: Duration = Duration::from_secs(2);
/// A daemon that keeps dying right after taking over is not retried forever.
const MAX_RESPAWNS: usize = 3;
const RESPAWN_WINDOW: Duration = Duration::from_secs(600);
/// A holder that ends up holding nothing exits after this long.
const HOLDER_IDLE_EXIT: Duration = Duration::from_secs(10);
const HOLDER_MAX_CONNS: usize = 16;

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
    /// The fd-holder process, after the previous daemon died.
    Holder,
}

/// A live connection found by [`try_reattach`], not yet handed to a session.
pub(crate) struct Reattach {
    pub(crate) fd: OwnedFd,
    pub(crate) dev: ConnDev,
    pub(crate) source: Source,
    /// The holder lease this process now owns (exec: inherited; holder: the take connection).
    lease: Option<UnixStream>,
}

impl std::fmt::Debug for Reattach {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reattach").field("dev", &self.dev).field("source", &self.source).finish()
    }
}

/// Who besides this process keeps the connection alive.
enum Keeper {
    Holder(UnixStream),
    FdStore,
    None,
}

struct Live {
    /// This process's own dup (FD_CLOEXEC), separate from the session's: used for
    /// FUSE_NOTIFY_RESEND, closing inherited backing ids and the next handover.
    fd: OwnedFd,
    dev: ConnDev,
    mount_point: PathBuf,
    keeper: Keeper,
}

static LIVE: Mutex<Option<Live>> = Mutex::new(None);

type QuiesceHook = Box<dyn Fn(Duration) -> bool + Send>;
type SnapshotHook = Box<dyn Fn() -> io::Result<()> + Send>;

/// Stops dispatching FUSE requests and waits (up to the budget) for the running
/// ones to finish; true when idle. Installed by the request scheduler.
static QUIESCE: Mutex<Option<QuiesceHook>> = Mutex::new(None);
/// Writes the exact per-handle state for the successor (state_snapshot).
static SNAPSHOT: Mutex<Option<SnapshotHook>> = Mutex::new(None);

pub(crate) fn set_quiesce_hook(f: impl Fn(Duration) -> bool + Send + 'static) {
    *QUIESCE.safe_lock() = Some(Box::new(f));
}

pub(crate) fn set_snapshot_hook(f: impl Fn() -> io::Result<()> + Send + 'static) {
    *SNAPSHOT.safe_lock() = Some(Box::new(f));
}

// ── mountinfo ────────────────────────────────────────────────────────────────

/// The device number of the fuse mount currently on top of `mount_point`.
/// Reads /proc/self/mountinfo, which never sends a request to the mount, so it
/// is safe while nobody serves it.
pub(crate) fn fuse_mount_dev(mount_point: &Path) -> Option<ConnDev> {
    let text = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    parse_mountinfo_dev(&text, mount_point)
}

fn parse_mountinfo_dev(text: &str, mount_point: &Path) -> Option<ConnDev> {
    let want = trim_trailing_slash(mount_point.as_os_str().as_bytes());
    let mut found = None;
    for line in text.lines() {
        let Some((pre, post)) = line.split_once(" - ") else { continue };
        let mut f = pre.split(' ');
        let (Some(_id), Some(_parent), Some(devno), Some(_root), Some(mp)) =
            (f.next(), f.next(), f.next(), f.next(), f.next())
        else {
            continue;
        };
        if unescape_mountinfo(mp) != want {
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

/// Sends `data` in one sendmsg, with `fd` attached as SCM_RIGHTS.
fn send_with_fd(sock: BorrowedFd<'_>, data: &[u8], fd: Option<BorrowedFd<'_>>, name: Option<(&libc::sockaddr_un, libc::socklen_t)>) -> io::Result<()> {
    let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut libc::c_void, iov_len: data.len() };
    // u64 storage keeps the control buffer aligned for cmsghdr.
    let mut cbuf = [0u64; 4];
    // SAFETY: an all-zero msghdr is a valid empty message.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if let Some((addr, len)) = name {
        msg.msg_name = addr as *const libc::sockaddr_un as *mut libc::c_void;
        msg.msg_namelen = len;
    }
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

/// One recvmsg; returns the bytes read and the first descriptor attached, if any
/// (extra descriptors are closed). Received descriptors are FD_CLOEXEC.
fn recv_with_fd(sock: BorrowedFd<'_>, buf: &mut [u8]) -> io::Result<(usize, Option<OwnedFd>)> {
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr() as *mut libc::c_void, iov_len: buf.len() };
    let mut cbuf = [0u64; 8];
    // SAFETY: an all-zero msghdr is a valid empty message.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = std::mem::size_of_val(&cbuf) as _;
    // SAFETY: msg points at live buffers for the duration of the call.
    let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut first = None;
    // SAFETY: walking the control messages the kernel just wrote into `cbuf`.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c) as *const RawFd;
                let payload = (*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                for i in 0..payload / std::mem::size_of::<RawFd>() {
                    let owned = OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(i)));
                    if first.is_none() {
                        first = Some(owned);
                    }
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    Ok((n as usize, first))
}

/// Reads one `\n`-terminated message (request/response protocol: a peer never
/// has two messages in flight, so nothing past the newline is lost).
fn recv_line(sock: &UnixStream) -> io::Result<(Vec<u8>, Option<OwnedFd>)> {
    let mut line = Vec::new();
    let mut fd = None;
    let mut buf = [0u8; 4096];
    loop {
        let (n, got) = recv_with_fd(sock.as_fd(), &mut buf)?;
        if fd.is_none() {
            fd = got;
        }
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed"));
        }
        line.extend_from_slice(&buf[..n]);
        if line.ends_with(b"\n") {
            line.pop();
            return Ok((line, fd));
        }
        if line.len() > 1 << 20 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "message too long"));
        }
    }
}

fn send_json<T: Serialize>(sock: &UnixStream, msg: &T, fd: Option<BorrowedFd<'_>>) -> io::Result<()> {
    let mut data = serde_json::to_vec(msg).map_err(io::Error::other)?;
    data.push(b'\n');
    send_with_fd(sock.as_fd(), &data, fd, None)
}

fn peer_is_us(sock: &UnixStream) -> bool {
    // SAFETY: getsockopt writes at most `len` bytes into `cred`.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    rc == 0 && cred.uid == unsafe { libc::getuid() }
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
    // UNVERIFIED: header-only RESEND write; based on fs/fuse/dev.c fuse_notify()
    // (case FUSE_NOTIFY_RESEND takes no payload), not exercised on a live kernel.
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

// ── systemd fd store ─────────────────────────────────────────────────────────

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
fn sd_notify(state: &str, fd: Option<BorrowedFd<'_>>) -> io::Result<bool> {
    let Some((addr, len)) = notify_socket_addr() else { return Ok(false) };
    let sock = UnixDatagram::unbound()?;
    send_with_fd(sock.as_fd(), state.as_bytes(), fd, Some((&addr, len)))?;
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

pub(crate) fn holder_socket_path() -> PathBuf {
    crate::ipc::socket_dir().join("ncrs-fd-holder.sock")
}

/// The live connection a previous daemon left for this one, if any: from this
/// process's own re-exec, systemd's fd store, or the fd holder — in that order.
/// Only returns a descriptor that is /dev/fuse and belongs to the fuse mount on
/// `mount_point` right now.
pub(crate) fn try_reattach(mount_point: &Path) -> Option<Reattach> {
    if let Some(r) = from_exec_env(mount_point) {
        return Some(r);
    }
    if let Some(r) = from_fd_store(mount_point) {
        return Some(r);
    }
    if !under_systemd_service() {
        return from_holder(mount_point);
    }
    None
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
    let lease_s = std::env::var(ENV_LEASE_FD).ok();
    // Children (fusermount, the holder, a respawn) must not inherit them.
    std::env::remove_var(ENV_FD);
    std::env::remove_var(ENV_DEV);
    std::env::remove_var(ENV_LEASE_FD);
    let lease = lease_s.and_then(|s| s.parse::<RawFd>().ok()).map(|raw| {
        let _ = set_cloexec(raw, true);
        // SAFETY: the previous image of this process left this descriptor open for us.
        unsafe { UnixStream::from_raw_fd(raw) }
    });
    let raw: RawFd = fd_s?.parse().ok()?;
    let _ = set_cloexec(raw, true);
    // SAFETY: the previous image of this process left this descriptor open for us.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let dev = dev_s.as_deref().and_then(ConnDev::parse_colon)?;
    if !validated(&fd, dev, mount_point) {
        return None;
    }
    log::info!("handoff: took over FUSE connection {} from this daemon's previous image", dev.to_colon());
    Some(Reattach { fd, dev, source: Source::Exec, lease })
}

fn from_fd_store(mount_point: &Path) -> Option<Reattach> {
    let mut found = None;
    for (fd, name) in take_listen_fds() {
        let Some(dev) = ConnDev::from_fdstore_name(&name) else { continue };
        if found.is_none() && validated(&fd, dev, mount_point) {
            found = Some(Reattach { fd, dev, source: Source::FdStore, lease: None });
        } else {
            // A connection whose mount is gone: drop it from the store too.
            let _ = sd_notify(&format!("FDSTOREREMOVE=1\nFDNAME={}", name), None);
        }
    }
    if let Some(r) = &found {
        log::info!("handoff: took over FUSE connection {} from the systemd fd store", r.dev.to_colon());
    }
    found
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
enum HolderRequest {
    /// Keep the attached descriptor; this connection is the depositor's lease.
    Deposit { mount_point: PathBuf, dev: ConnDev, exe: PathBuf, args: Vec<String> },
    /// Hand the descriptor over; on success this connection becomes the taker's lease.
    Take { mount_point: PathBuf },
    /// Drop the descriptor: the mount ended cleanly, or the taker cannot use it.
    Release { mount_point: PathBuf },
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Default)]
struct HolderReply {
    ok: bool,
    #[serde(default)]
    err: Option<String>,
    #[serde(default)]
    dev: Option<ConnDev>,
}

fn from_holder(mount_point: &Path) -> Option<Reattach> {
    let sock = UnixStream::connect(holder_socket_path()).ok()?;
    sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
    send_json(&sock, &HolderRequest::Take { mount_point: mount_point.to_path_buf() }, None).ok()?;
    let (line, fd) = recv_line(&sock).ok()?;
    let reply: HolderReply = serde_json::from_slice(&line).ok()?;
    if !reply.ok {
        // "busy": another daemon serves it (prepare_mount_point then refuses to
        // mount over it); "none": nothing held for this mount point.
        log::debug!("handoff: fd holder has nothing to hand over ({:?})", reply.err);
        return None;
    }
    let (Some(fd), Some(dev)) = (fd, reply.dev) else { return None };
    if !validated(&fd, dev, mount_point) {
        let _ = send_json(&sock, &HolderRequest::Release { mount_point: mount_point.to_path_buf() }, None);
        let _ = recv_line(&sock);
        return None;
    }
    sock.set_read_timeout(None).ok();
    log::info!("handoff: took over FUSE connection {} from the fd holder", dev.to_colon());
    Some(Reattach { fd, dev, source: Source::Holder, lease: Some(sock) })
}

/// Gives up on a connection [`try_reattach`] found (its state could not be
/// restored): releases every keeper and aborts the connection, so the mount
/// point reads ENOTCONN, which `prepare_mount_point` then detaches.
pub(crate) fn abandon(r: Reattach, mount_point: &Path) {
    let dev = r.dev;
    log::warn!("handoff: abandoning FUSE connection {}; programs holding files on it get ENOTCONN", r.dev.to_colon());
    match r.source {
        Source::FdStore => {
            let _ = sd_notify(&format!("FDSTOREREMOVE=1\nFDNAME={}", r.dev.fdstore_name()), None);
        }
        Source::Holder | Source::Exec => {
            if let Some(lease) = &r.lease {
                let _ = send_json(lease, &HolderRequest::Release { mount_point: mount_point.to_path_buf() }, None);
                lease.set_read_timeout(Some(Duration::from_secs(2))).ok();
                let _ = recv_line(lease);
            }
        }
    }
    if under_systemd_service() {
        let _ = sd_notify(&format!("FDSTOREREMOVE=1\nFDNAME={}", r.dev.fdstore_name()), None);
    }
    drop(r);
    // The kernel aborts a connection when its last descriptor closes, but a
    // keeper may close its copy late (systemd handles FDSTOREREMOVE on its own
    // time). Until then a stat of the mount point — prepare_mount_point's first
    // step — would block forever, so abort it outright.
    abort_connection(dev);
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

/// The binary to (re)start: the path this process was started from, even if a
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

fn restart_args() -> Vec<String> {
    std::env::args_os().skip(1).map(|a| a.to_string_lossy().into_owned()).collect()
}

/// Called right after a fresh mount: registers the session's connection with a
/// keeper (holder or fd store) so a crash does not end the mount. Returns the
/// connection's id, or None when it cannot be identified (restart survival is
/// then off for this mount, and nothing else changes).
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
    let keeper = new_keeper(fd.as_fd(), dev, mount_point);
    *LIVE.safe_lock() = Some(Live { fd, dev, mount_point: mount_point.to_path_buf(), keeper });
    Some(dev)
}

/// Called after a reattached session is created from `r.fd`'s twin.
pub(crate) fn adopt_reattached(own: OwnedFd, dev: ConnDev, source: Source, lease: Option<UnixStream>, mount_point: &Path) {
    let keeper = match (source, lease) {
        (_, Some(lease)) => Keeper::Holder(lease),
        (Source::FdStore, None) => {
            // Re-store it: systemd handed its copy to us and dropped it from the store.
            store_in_fd_store(own.as_fd(), dev)
        }
        _ => new_keeper(own.as_fd(), dev, mount_point),
    };
    *LIVE.safe_lock() = Some(Live { fd: own, dev, mount_point: mount_point.to_path_buf(), keeper });
}

impl Reattach {
    /// Splits off the parts [`adopt_reattached`] needs once `fd` goes to the session.
    pub(crate) fn into_parts(self) -> (OwnedFd, ConnDev, Source, Option<UnixStream>) {
        (self.fd, self.dev, self.source, self.lease)
    }
}

fn store_in_fd_store(fd: BorrowedFd<'_>, dev: ConnDev) -> Keeper {
    match sd_notify(&format!("FDSTORE=1\nFDNAME={}", dev.fdstore_name()), Some(fd)) {
        Ok(true) => {
            log::info!("handoff: FUSE connection {} kept in the systemd fd store", dev.to_colon());
            Keeper::FdStore
        }
        Ok(false) => Keeper::None,
        Err(e) => {
            log::warn!("handoff: systemd fd store refused the FUSE descriptor ({}) — a crash will end the mount", e);
            Keeper::None
        }
    }
}

fn new_keeper(fd: BorrowedFd<'_>, dev: ConnDev, mount_point: &Path) -> Keeper {
    if under_systemd_service() {
        return store_in_fd_store(fd, dev);
    }
    match deposit_with_holder(fd, dev, mount_point) {
        Ok(lease) => {
            log::info!("handoff: FUSE connection {} kept by the fd holder", dev.to_colon());
            Keeper::Holder(lease)
        }
        Err(e) => {
            log::warn!("handoff: fd holder unavailable ({}) — a crash will end the mount", e);
            Keeper::None
        }
    }
}

fn deposit_with_holder(fd: BorrowedFd<'_>, dev: ConnDev, mount_point: &Path) -> io::Result<UnixStream> {
    let path = holder_socket_path();
    let sock = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(_) => {
            spawn_holder(&path)?;
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                match UnixStream::connect(&path) {
                    Ok(s) => break s,
                    Err(e) if Instant::now() >= deadline => return Err(e),
                    Err(_) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
        }
    };
    sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let req = HolderRequest::Deposit {
        mount_point: mount_point.to_path_buf(),
        dev,
        exe: restart_exe(),
        args: restart_args(),
    };
    send_json(&sock, &req, Some(fd))?;
    let (line, _) = recv_line(&sock)?;
    let reply: HolderReply = serde_json::from_slice(&line).map_err(io::Error::other)?;
    if !reply.ok {
        return Err(io::Error::other(reply.err.unwrap_or_else(|| "refused".into())));
    }
    sock.set_read_timeout(None).ok();
    Ok(sock)
}

fn spawn_holder(socket: &Path) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new("/proc/self/exe");
    cmd.arg("--fd-holder")
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .env_remove(ENV_FD)
        .env_remove(ENV_DEV)
        .env_remove(ENV_LEASE_FD);
    // Its own session, so a hangup or a signal to the GUI's process group does
    // not reach it. It stays in this cgroup (see the module doc).
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    crate::bg::spawn_service("fd-holder-reap", move || {
        let _ = child.wait();
    })?;
    Ok(())
}

/// After a reattached session starts reading: re-queue what the previous
/// daemon left unanswered.
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

/// The session ended cleanly (unmounted): tell the keeper to let go, so the
/// connection is not kept alive for a successor that will never come.
pub(crate) fn release_connection() {
    let Some(live) = LIVE.safe_lock().take() else { return };
    match &live.keeper {
        Keeper::Holder(lease) => {
            let _ = send_json(lease, &HolderRequest::Release { mount_point: live.mount_point.clone() }, None);
            lease.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let _ = recv_line(lease);
        }
        Keeper::FdStore => {
            let _ = sd_notify(&format!("FDSTOREREMOVE=1\nFDNAME={}", live.dev.fdstore_name()), None);
        }
        Keeper::None => {}
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

/// Re-executes this daemon with the connection (and the holder lease, if any)
/// left open across exec. Only returns on failure, with everything restored.
fn planned_restart() -> io::Result<()> {
    if let Some(q) = QUIESCE.safe_lock().as_ref() {
        if !q(Duration::from_secs(3)) {
            log::warn!("handoff: requests still running after 3s — restarting anyway; the kernel resends them");
        }
    } else {
        // UNVERIFIED: without a scheduler quiesce hook, a request mid-execution is
        // cut off by exec and replayed via FUSE_NOTIFY_RESEND (non-idempotent ops
        // like rename/unlink then see their own effect and may fail).
        std::thread::sleep(Duration::from_millis(200));
    }
    if let Some(snap) = SNAPSHOT.safe_lock().as_ref() {
        // Without it the successor would treat every write handle as crashed.
        snap()?;
    }
    let g = LIVE.safe_lock();
    let live = g.as_ref().ok_or_else(|| io::Error::other("not mounted"))?;
    let fd = live.fd.as_raw_fd();
    let lease = match &live.keeper {
        Keeper::Holder(s) => Some(s.as_raw_fd()),
        _ => None,
    };
    set_cloexec(fd, false)?;
    if let Some(l) = lease {
        set_cloexec(l, false)?;
    }
    let exe = restart_exe();
    log::info!("handoff: re-executing {} with FUSE connection {} (fd {})", exe.display(), live.dev.to_colon(), fd);
    let mut cmd = Command::new(&exe);
    cmd.args(std::env::args_os().skip(1)).env(ENV_FD, fd.to_string()).env(ENV_DEV, live.dev.to_colon());
    if let Some(l) = lease {
        cmd.env(ENV_LEASE_FD, l.to_string());
    }
    use std::os::unix::process::CommandExt;
    let err = cmd.exec();
    // Still here: exec failed. Put the descriptors back as they were.
    let _ = set_cloexec(fd, true);
    if let Some(l) = lease {
        let _ = set_cloexec(l, true);
    }
    Err(err)
}

// ── the fd holder process ────────────────────────────────────────────────────

struct Held {
    fd: OwnedFd,
    dev: ConnDev,
    exe: PathBuf,
    args: Vec<String>,
    /// Client connection id of the daemon serving it; None while orphaned.
    lease: Option<u64>,
    orphaned_at: Option<Instant>,
    respawned: bool,
    respawns: Vec<Instant>,
}

#[derive(Default)]
struct HolderState {
    held: HashMap<PathBuf, Held>,
    next_conn: u64,
}

/// Body of `ncrs --fd-holder <socket>`. Exits 0 when it holds nothing.
pub fn run_holder(socket: &Path) -> i32 {
    drop_capabilities();
    // A holder already listening there wins.
    if UnixStream::connect(socket).is_ok() {
        return 0;
    }
    let _ = std::fs::remove_file(socket);
    let listener = match UnixListener::bind(socket) {
        Ok(l) => l,
        Err(e) => {
            log::error!("fd holder: cannot listen on {}: {}", socket.display(), e);
            return 1;
        }
    };
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600));
    }
    log::info!("fd holder: listening on {}", socket.display());
    let state = Arc::new(Mutex::new(HolderState::default()));
    let started = Instant::now();
    {
        let state = state.clone();
        let socket = socket.to_path_buf();
        holder_thread("fd-holder-watch", move || loop {
            std::thread::sleep(Duration::from_secs(1));
            let empty = holder_tick(&mut state.safe_lock(), Instant::now());
            if empty && started.elapsed() > HOLDER_IDLE_EXIT {
                log::info!("fd holder: nothing left to hold, exiting");
                let _ = std::fs::remove_file(&socket);
                std::process::exit(0);
            }
        });
    }
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        if !peer_is_us(&conn) {
            continue;
        }
        // One per daemon (its lease) plus short-lived takes: a pile-up means
        // something is looping, not more daemons to serve.
        if active.load(std::sync::atomic::Ordering::SeqCst) >= HOLDER_MAX_CONNS {
            continue;
        }
        active.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let state = state.clone();
        let active = active.clone();
        holder_thread("fd-holder-conn", move || {
            holder_client(conn, state);
            active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        });
    }
    0
}

/// Threads of the holder *process*. bg::'s lifetime budgets exist to bound the
/// daemon; the holder is a separate, tiny process whose threads are one watcher
/// plus at most HOLDER_MAX_CONNS live connections, so it spawns directly.
#[allow(clippy::disallowed_methods)]
fn holder_thread(name: &str, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name(name.to_string()).spawn(f) {
        log::error!("fd holder: cannot start {} thread: {}", name, e);
    }
}

fn drop_capabilities() {
    // The holder needs no privilege; ncrs's file capability (CAP_SYS_ADMIN for
    // passthrough) would otherwise come along with the exec.
    #[repr(C)]
    struct CapHeader {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CapData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let mut h = CapHeader { version: 0x2008_0522, pid: 0 }; // _LINUX_CAPABILITY_VERSION_3
    let d = [CapData { effective: 0, permitted: 0, inheritable: 0 }; 2];
    // SAFETY: capset reads the header and two data structs.
    let rc = unsafe { libc::syscall(libc::SYS_capset, &mut h as *mut CapHeader, d.as_ptr()) };
    if rc != 0 {
        log::debug!("fd holder: capset failed: {}", io::Error::last_os_error());
    }
}

/// One monitoring pass; returns true when nothing is held.
fn holder_tick(st: &mut HolderState, now: Instant) -> bool {
    st.held.retain(|mp, h| {
        if fuse_mount_dev(mp) != Some(h.dev) {
            log::info!("fd holder: {} is no longer mounted — dropping its connection", mp.display());
            return false;
        }
        if h.lease.is_some() {
            return true;
        }
        let since = *h.orphaned_at.get_or_insert(now);
        if now.duration_since(since) >= ORPHAN_GRACE {
            log::warn!(
                "fd holder: no daemon took over {} within {}s — letting the connection go (programs on it now get ENOTCONN)",
                mp.display(), ORPHAN_GRACE.as_secs(),
            );
            return false;
        }
        if !h.respawned && now.duration_since(since) >= RESPAWN_DELAY {
            h.respawned = true;
            h.respawns.retain(|t| now.duration_since(*t) < RESPAWN_WINDOW);
            if h.respawns.len() >= MAX_RESPAWNS {
                log::warn!("fd holder: daemon for {} died {} times in {} min — not respawning", mp.display(), MAX_RESPAWNS, RESPAWN_WINDOW.as_secs() / 60);
            } else {
                h.respawns.push(now);
                respawn_daemon(&h.exe, &h.args);
            }
        }
        true
    });
    st.held.is_empty()
}

fn respawn_daemon(exe: &Path, args: &[String]) {
    log::warn!("fd holder: daemon died without releasing its FUSE connection — restarting {}", exe.display());
    let mut cmd = Command::new(exe);
    cmd.args(args.iter().map(OsString::from))
        .stdin(Stdio::null())
        .env_remove(ENV_FD)
        .env_remove(ENV_DEV)
        .env_remove(ENV_LEASE_FD);
    match cmd.spawn() {
        Ok(mut child) => holder_thread("fd-holder-reap", move || {
            let _ = child.wait();
        }),
        Err(e) => log::error!("fd holder: cannot restart {}: {}", exe.display(), e),
    }
}

fn holder_client(conn: UnixStream, state: Arc<Mutex<HolderState>>) {
    let id = {
        let mut st = state.safe_lock();
        st.next_conn += 1;
        st.next_conn
    };
    // Mount points this connection is the lease for.
    let mut leases: Vec<PathBuf> = Vec::new();
    loop {
        let (line, fd) = match recv_line(&conn) {
            Ok(x) => x,
            Err(_) => break,
        };
        let req: HolderRequest = match serde_json::from_slice(&line) {
            Ok(r) => r,
            Err(e) => {
                let _ = send_json(&conn, &HolderReply { ok: false, err: Some(e.to_string()), dev: None }, None);
                continue;
            }
        };
        let mut st = state.safe_lock();
        let (reply, give) = handle_holder_request(&mut st, id, req, fd, &mut leases);
        let give_fd = give.and_then(|mp| st.held.get(&mp).map(|h| h.fd.as_fd()));
        let _ = send_json(&conn, &reply, give_fd);
    }
    // Lease connection gone: the daemon exited (or re-exec failed to keep it).
    let mut st = state.safe_lock();
    for mp in leases {
        if let Some(h) = st.held.get_mut(&mp) {
            if h.lease == Some(id) {
                log::info!("fd holder: daemon serving {} went away", mp.display());
                h.lease = None;
                h.orphaned_at = None;
                h.respawned = false;
            }
        }
    }
}

/// The holder protocol's state transitions. Returns the reply and, for a
/// successful take, the mount point whose descriptor to attach.
fn handle_holder_request(
    st: &mut HolderState,
    conn: u64,
    req: HolderRequest,
    fd: Option<OwnedFd>,
    leases: &mut Vec<PathBuf>,
) -> (HolderReply, Option<PathBuf>) {
    match req {
        HolderRequest::Deposit { mount_point, dev, exe, args } => {
            let Some(fd) = fd else {
                return (HolderReply { ok: false, err: Some("no descriptor attached".into()), dev: None }, None);
            };
            if let Some(h) = st.held.get(&mount_point) {
                if h.lease.is_some() && h.lease != Some(conn) && h.dev == dev {
                    return (HolderReply { ok: false, err: Some("busy".into()), dev: None }, None);
                }
            }
            st.held.insert(
                mount_point.clone(),
                Held { fd, dev, exe, args, lease: Some(conn), orphaned_at: None, respawned: false, respawns: Vec::new() },
            );
            leases.push(mount_point);
            (HolderReply { ok: true, err: None, dev: Some(dev) }, None)
        }
        HolderRequest::Take { mount_point } => match st.held.get_mut(&mount_point) {
            None => (HolderReply { ok: false, err: Some("none".into()), dev: None }, None),
            Some(h) if h.lease.is_some() => (HolderReply { ok: false, err: Some("busy".into()), dev: None }, None),
            Some(h) => {
                h.lease = Some(conn);
                h.orphaned_at = None;
                h.respawned = false;
                let dev = h.dev;
                leases.push(mount_point.clone());
                (HolderReply { ok: true, err: None, dev: Some(dev) }, Some(mount_point))
            }
        },
        HolderRequest::Release { mount_point } => {
            let mine = st.held.get(&mount_point).is_some_and(|h| h.lease == Some(conn) || h.lease.is_none());
            if mine {
                st.held.remove(&mount_point);
                log::info!("fd holder: released the connection for {}", mount_point.display());
            }
            leases.retain(|m| m != &mount_point);
            (HolderReply { ok: mine, err: (!mine).then(|| "not the lease holder".into()), dev: None }, None)
        }
    }
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

    #[test]
    fn mountinfo_finds_the_fuse_mount_with_escaped_path() {
        let d = parse_mountinfo_dev(MOUNTINFO, Path::new("/home/u/Next cloud")).unwrap();
        assert_eq!(d, ConnDev { major: 0, minor: 73 });
        // A trailing slash in the configured path still matches.
        assert_eq!(parse_mountinfo_dev(MOUNTINFO, Path::new("/home/u/Next cloud/")), Some(d));
    }

    #[test]
    fn mountinfo_ignores_non_fuse_and_covered_mounts() {
        assert_eq!(parse_mountinfo_dev(MOUNTINFO, Path::new("/mnt/plain")), None);
        // fuse.sshfs mounted first, then covered by a tmpfs: a lookup reaches the tmpfs.
        assert_eq!(parse_mountinfo_dev(MOUNTINFO, Path::new("/mnt/stacked")), None);
        assert_eq!(parse_mountinfo_dev(MOUNTINFO, Path::new("/nowhere")), None);
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
    fn holder_messages_round_trip() {
        let req = HolderRequest::Deposit {
            mount_point: PathBuf::from("/m"),
            dev: ConnDev { major: 0, minor: 9 },
            exe: PathBuf::from("/usr/bin/ncrs"),
            args: vec!["--config".into(), "/c.yaml".into()],
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"op\":\"deposit\""));
        assert_eq!(serde_json::from_str::<HolderRequest>(&s).unwrap(), req);
        let reply: HolderReply = serde_json::from_str("{\"ok\":false}").unwrap();
        assert_eq!(reply, HolderReply::default());
    }

    fn dummy_fd() -> OwnedFd {
        std::fs::File::open("/dev/null").unwrap().into()
    }

    fn deposit(st: &mut HolderState, conn: u64, leases: &mut Vec<PathBuf>) -> HolderReply {
        let req = HolderRequest::Deposit {
            mount_point: PathBuf::from("/m"),
            dev: ConnDev { major: 0, minor: 9 },
            exe: PathBuf::from("/bin/true"),
            args: vec![],
        };
        handle_holder_request(st, conn, req, Some(dummy_fd()), leases).0
    }

    #[test]
    fn holder_hands_over_only_an_orphaned_connection() {
        let mut st = HolderState::default();
        let mut l1 = Vec::new();
        assert!(deposit(&mut st, 1, &mut l1).ok);
        // The depositor still serves it: a second daemon must not get it.
        let mut l2 = Vec::new();
        let (r, give) = handle_holder_request(&mut st, 2, HolderRequest::Take { mount_point: "/m".into() }, None, &mut l2);
        assert_eq!((r.ok, r.err.as_deref(), give), (false, Some("busy"), None));
        // The depositor dies (what holder_client does on EOF).
        st.held.get_mut(Path::new("/m")).unwrap().lease = None;
        let (r, give) = handle_holder_request(&mut st, 2, HolderRequest::Take { mount_point: "/m".into() }, None, &mut l2);
        assert!(r.ok);
        assert_eq!(r.dev, Some(ConnDev { major: 0, minor: 9 }));
        assert_eq!(give, Some(PathBuf::from("/m")));
        assert_eq!(st.held[Path::new("/m")].lease, Some(2));
        assert_eq!(l2, vec![PathBuf::from("/m")]);
    }

    #[test]
    fn holder_release_only_from_the_lease_holder() {
        let mut st = HolderState::default();
        let mut l1 = Vec::new();
        deposit(&mut st, 1, &mut l1);
        let mut l2 = Vec::new();
        let (r, _) = handle_holder_request(&mut st, 2, HolderRequest::Release { mount_point: "/m".into() }, None, &mut l2);
        assert!(!r.ok, "a stranger cannot drop a served connection");
        assert!(st.held.contains_key(Path::new("/m")));
        let (r, _) = handle_holder_request(&mut st, 1, HolderRequest::Release { mount_point: "/m".into() }, None, &mut l1);
        assert!(r.ok);
        assert!(st.held.is_empty());
        assert!(l1.is_empty());
    }

    #[test]
    fn holder_take_of_unknown_mount_is_none() {
        let mut st = HolderState::default();
        let mut l = Vec::new();
        let (r, give) = handle_holder_request(&mut st, 1, HolderRequest::Take { mount_point: "/x".into() }, None, &mut l);
        assert_eq!((r.ok, r.err.as_deref(), give), (false, Some("none"), None));
    }

    #[test]
    fn deposit_without_descriptor_is_refused() {
        let mut st = HolderState::default();
        let mut l = Vec::new();
        let req = HolderRequest::Deposit { mount_point: "/m".into(), dev: ConnDev { major: 0, minor: 1 }, exe: "/x".into(), args: vec![] };
        let (r, _) = handle_holder_request(&mut st, 1, req, None, &mut l);
        assert!(!r.ok);
        assert!(st.held.is_empty());
    }

    #[test]
    fn fd_passing_round_trips_over_a_socketpair() {
        let (a, b) = UnixStream::pair().unwrap();
        let f = dummy_fd();
        send_json(&a, &HolderReply { ok: true, err: None, dev: None }, Some(f.as_fd())).unwrap();
        let (line, got) = recv_line(&b).unwrap();
        let reply: HolderReply = serde_json::from_slice(&line).unwrap();
        assert!(reply.ok);
        let got = got.expect("descriptor attached");
        assert_ne!(got.as_raw_fd(), f.as_raw_fd(), "the receiver gets its own descriptor");
        // SAFETY: F_GETFD on a descriptor this test owns.
        let flags = unsafe { libc::fcntl(got.as_raw_fd(), libc::F_GETFD) };
        assert!(flags & libc::FD_CLOEXEC != 0, "received descriptors must not leak into children");
    }

    #[test]
    fn dev_null_is_not_dev_fuse() {
        assert!(!is_dev_fuse(dummy_fd().as_fd()));
    }
}
