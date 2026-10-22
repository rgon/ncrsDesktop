//! The daemon's side: spawning the helper and talking to it.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use crate::proto::{self, Request};

/// The descriptor number the helper finds its socket on.
pub const HELPER_SOCK_FD: RawFd = 3;

/// How long one request may take. The helper answers from a single ioctl, so
/// this only bounds a wedged or stopped helper: a FUSE open then falls back
/// to buffered reads instead of hanging.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// One end of the helper's SOCK_SEQPACKET socketpair.
pub struct Channel(OwnedFd);

impl AsFd for Channel {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl Channel {
    /// A connected pair, both ends close-on-exec.
    pub fn pair() -> io::Result<(Channel, Channel)> {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: socketpair writes two descriptors into `fds`.
        let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: both descriptors were just created and are owned here.
        Ok(unsafe { (Channel(OwnedFd::from_raw_fd(fds[0])), Channel(OwnedFd::from_raw_fd(fds[1]))) })
    }

    pub fn set_reply_timeout(&self, t: Duration) -> io::Result<()> {
        let tv = libc::timeval { tv_sec: t.as_secs() as libc::time_t, tv_usec: t.subsec_micros() as libc::suseconds_t };
        // SAFETY: SO_RCVTIMEO reads one timeval.
        let rc = unsafe {
            libc::setsockopt(
                self.0.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &tv as *const libc::timeval as *const libc::c_void,
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Sends one request and waits for its reply. The outer error is the
    /// transport failing (the helper is gone); the inner one is its answer.
    pub fn call(&self, req: Request, fd: Option<BorrowedFd<'_>>) -> io::Result<Result<u32, i32>> {
        let fds: &[BorrowedFd<'_>] = match &fd {
            Some(f) => std::slice::from_ref(f),
            None => &[],
        };
        self.call_raw_fds(&req.encode(), fds)
    }

    /// [`Channel::call`] with an arbitrary payload (tests send malformed ones).
    pub fn call_raw(&self, payload: &[u8], fd: Option<BorrowedFd<'_>>) -> io::Result<Result<u32, i32>> {
        let fds: &[BorrowedFd<'_>] = match &fd {
            Some(f) => std::slice::from_ref(f),
            None => &[],
        };
        self.call_raw_fds(payload, fds)
    }

    pub fn call_raw_fds(&self, payload: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<Result<u32, i32>> {
        send_with_fds(self.0.as_fd(), payload, fds)?;
        let mut buf = [0u8; proto::REPLY_LEN + 1];
        let n = loop {
            // SAFETY: receives into `buf`.
            let n = unsafe { libc::recv(self.0.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(e);
            }
            break n as usize;
        };
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "passthrough helper exited"));
        }
        proto::decode_reply(&buf[..n]).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed reply from the passthrough helper"))
    }
}

fn send_with_fds(sock: BorrowedFd<'_>, data: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<()> {
    let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut libc::c_void, iov_len: data.len() };
    let mut cbuf = [0u64; 8];
    // SAFETY: an all-zero msghdr is a valid empty message.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        let bytes = std::mem::size_of_val(fds) as u32;
        // SAFETY: CMSG_* compute offsets into `cbuf`, which is checked to be
        // large enough; the header and descriptors written are inside it.
        unsafe {
            let space = libc::CMSG_SPACE(bytes) as usize;
            if space > std::mem::size_of_val(&cbuf) {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "too many descriptors"));
            }
            msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
            msg.msg_controllen = space as _;
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(bytes) as _;
            let data = libc::CMSG_DATA(c) as *mut RawFd;
            for (i, fd) in fds.iter().enumerate() {
                std::ptr::write_unaligned(data.add(i), fd.as_raw_fd());
            }
        }
    }
    // SAFETY: msg points at live buffers for the duration of the call.
    let n = unsafe { libc::sendmsg(sock.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Why no helper is in use; the daemon logs it and keeps buffered reads.
#[derive(Debug)]
pub enum Unavailable {
    /// The helper could not be started (not installed, not executable).
    Spawn(io::Error),
    /// It started but the conversation failed.
    Protocol(io::Error),
    /// It refused the connection's descriptor (errno).
    Refused(i32),
    /// It runs without CAP_SYS_ADMIN (the file capability is missing).
    NoCapability,
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unavailable::Spawn(e) => write!(f, "cannot start it: {}", e),
            Unavailable::Protocol(e) => write!(f, "it failed: {}", e),
            Unavailable::Refused(e) => write!(f, "it refused the FUSE connection: {}", io::Error::from_raw_os_error(*e)),
            Unavailable::NoCapability => write!(f, "it lacks CAP_SYS_ADMIN (its file capability is not set)"),
        }
    }
}

/// A running helper bound to one FUSE connection.
pub struct Helper {
    chan: Mutex<Channel>,
    child: Mutex<Child>,
}

impl Helper {
    /// Starts `exe` for the connection `fuse`, then lets it close `adopt` —
    /// backing ids a previous daemon registered on this connection.
    ///
    /// Call from a thread that lives as long as the mount: the helper sets
    /// PR_SET_PDEATHSIG, which fires when the *thread* that spawned it exits.
    pub fn spawn(exe: &Path, fuse: BorrowedFd<'_>, adopt: &[u32]) -> Result<Helper, Unavailable> {
        let helper = Helper::start(exe).map_err(Unavailable::Spawn)?;
        let ready = (|| {
            match helper.call(Request::Setup, Some(fuse)).map_err(Unavailable::Protocol)? {
                Ok(1) => {}
                Ok(_) => return Err(Unavailable::NoCapability),
                Err(e) => return Err(Unavailable::Refused(e)),
            }
            for &id in adopt {
                if let Err(e) = helper.call(Request::Adopt(id), None).map_err(Unavailable::Protocol)? {
                    return Err(Unavailable::Refused(e));
                }
            }
            Ok(())
        })();
        match ready {
            Ok(()) => Ok(helper),
            Err(e) => {
                helper.shutdown();
                Err(e)
            }
        }
    }

    /// Only starts the process; [`Helper::spawn`] also attaches the connection.
    pub fn start(exe: &Path) -> io::Result<Helper> {
        let (ours, theirs) = Channel::pair()?;
        ours.set_reply_timeout(REPLY_TIMEOUT)?;
        let theirs_raw = theirs.0.as_raw_fd();
        let mut cmd = Command::new(exe);
        cmd.arg(std::process::id().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .env_clear();
        // SAFETY: the closure runs between fork and exec and only makes
        // async-signal-safe calls (dup2, fcntl).
        unsafe {
            cmd.pre_exec(move || {
                if theirs_raw == HELPER_SOCK_FD {
                    let fl = libc::fcntl(HELPER_SOCK_FD, libc::F_GETFD);
                    if fl < 0 || libc::fcntl(HELPER_SOCK_FD, libc::F_SETFD, fl & !libc::FD_CLOEXEC) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                } else if libc::dup2(theirs_raw, HELPER_SOCK_FD) < 0 {
                    // dup2 leaves the new descriptor without FD_CLOEXEC.
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        drop(theirs);
        Ok(Helper { chan: Mutex::new(ours), child: Mutex::new(child) })
    }

    fn lock_chan(&self) -> std::sync::MutexGuard<'_, Channel> {
        self.chan.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// One request and its reply (one in flight at a time: replies carry no
    /// request id).
    pub fn call(&self, req: Request, fd: Option<BorrowedFd<'_>>) -> io::Result<Result<u32, i32>> {
        self.lock_chan().call(req, fd)
    }

    /// [`Helper::call`] with an arbitrary payload (tests send malformed ones).
    pub fn call_raw(&self, payload: &[u8]) -> io::Result<Result<u32, i32>> {
        self.lock_chan().call_raw(payload, None)
    }

    /// The helper's process id.
    pub fn pid(&self) -> u32 {
        self.child.lock().unwrap_or_else(|p| p.into_inner()).id()
    }

    /// Registers `file` (a read-only cache copy) as a backing file.
    pub fn open_backing(&self, file: BorrowedFd<'_>) -> io::Result<u32> {
        self.call(Request::Open, Some(file))?.map_err(io::Error::from_raw_os_error)
    }

    pub fn close_backing(&self, id: u32) -> io::Result<()> {
        self.call(Request::Close(id), None)?.map(|_| ()).map_err(io::Error::from_raw_os_error)
    }

    /// Closes the socket (the helper exits on EOF) and reaps it; its exit
    /// status, unless it had to be killed.
    pub fn shutdown(&self) -> Option<std::process::ExitStatus> {
        // Replace our end with a dead descriptor so the helper sees EOF now.
        if let Ok(f) = std::fs::File::open("/dev/null") {
            *self.lock_chan() = Channel(f.into());
        }
        let mut c = self.child.lock().unwrap_or_else(|p| p.into_inner());
        for _ in 0..100 {
            if let Ok(Some(st)) = c.try_wait() {
                return Some(st);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = c.kill();
        let _ = c.wait();
        None
    }
}
