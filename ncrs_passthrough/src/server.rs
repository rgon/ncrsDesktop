//! The helper's side: request validation, the id bookkeeping, and the receive
//! loop. Everything here runs after the helper's seccomp filter is installed,
//! so it sticks to recvmsg/sendto/fstat/fstatfs/fcntl/ioctl/close and the
//! allocator (see `main.rs` for the allowlist).

use std::collections::BTreeSet;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

use crate::proto::{self, Request};

/// `FUSE_DEV_IOC_BACKING_OPEN` = `_IOW(229, 1, struct fuse_backing_map)` (16 bytes).
pub const FUSE_DEV_IOC_BACKING_OPEN: u64 = (1 << 30) | (16 << 16) | (229 << 8) | 1;
/// `FUSE_DEV_IOC_BACKING_CLOSE` = `_IOW(229, 2, uint32_t)`.
pub const FUSE_DEV_IOC_BACKING_CLOSE: u64 = (1 << 30) | (4 << 16) | (229 << 8) | 2;

/// `FUSE_SUPER_MAGIC` (include/uapi/linux/magic.h).
const FUSE_SUPER_MAGIC: i64 = 0x6573_5546;

#[repr(C)]
struct FuseBackingMap {
    fd: i32,
    flags: u32,
    padding: u64,
}

/// What the helper asks of the kernel; a trait so the protocol can be tested
/// without /dev/fuse or the capability.
pub trait Kernel {
    fn is_fuse_dev(&self, fd: BorrowedFd<'_>) -> bool;
    fn backing_open(&mut self, fuse: BorrowedFd<'_>, file: BorrowedFd<'_>) -> Result<u32, i32>;
    fn backing_close(&mut self, fuse: BorrowedFd<'_>, id: u32) -> Result<(), i32>;
}

/// The real thing: ioctls on the connection's /dev/fuse descriptor.
pub struct DevFuse;

impl Kernel for DevFuse {
    fn is_fuse_dev(&self, fd: BorrowedFd<'_>) -> bool {
        is_fuse_dev(fd)
    }

    fn backing_open(&mut self, fuse: BorrowedFd<'_>, file: BorrowedFd<'_>) -> Result<u32, i32> {
        let map = FuseBackingMap { fd: file.as_raw_fd(), flags: 0, padding: 0 };
        // SAFETY: the ioctl reads one fuse_backing_map.
        let rc = unsafe { libc::ioctl(fuse.as_raw_fd(), FUSE_DEV_IOC_BACKING_OPEN as _, &map as *const FuseBackingMap) };
        if rc < 0 {
            return Err(errno());
        }
        Ok(rc as u32)
    }

    fn backing_close(&mut self, fuse: BorrowedFd<'_>, id: u32) -> Result<(), i32> {
        // SAFETY: the ioctl reads one u32.
        let rc = unsafe { libc::ioctl(fuse.as_raw_fd(), FUSE_DEV_IOC_BACKING_CLOSE as _, &id as *const u32) };
        if rc < 0 {
            return Err(errno());
        }
        Ok(())
    }
}

fn errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)
}

fn fstat(fd: BorrowedFd<'_>) -> Result<libc::stat, i32> {
    // SAFETY: fstat only writes the provided struct.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(errno());
    }
    Ok(st)
}

/// /dev/fuse: character device 10:229.
pub fn is_fuse_dev(fd: BorrowedFd<'_>) -> bool {
    fstat(fd).is_ok_and(|st| {
        (st.st_mode & libc::S_IFMT) == libc::S_IFCHR && libc::major(st.st_rdev) == 10 && libc::minor(st.st_rdev) == 229
    })
}

/// What a backing file must be: a regular file, owned by the helper's real
/// uid, opened read-only (not O_PATH), and not itself on a FUSE filesystem.
/// The daemon only ever registers its own complete cache copies, so this
/// bounds a compromised daemon to files the user could read anyway, and keeps
/// it from stacking FUSE on FUSE. (A path check against the cache directory
/// would add little: that directory would come from the same daemon.)
pub fn validate_backing_fd(fd: BorrowedFd<'_>, uid: u32) -> Result<(), i32> {
    let st = fstat(fd)?;
    if (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
        return Err(libc::EINVAL);
    }
    if st.st_uid != uid {
        return Err(libc::EPERM);
    }
    // SAFETY: F_GETFL on a descriptor this process holds.
    let fl = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if fl < 0 {
        return Err(errno());
    }
    if fl & libc::O_PATH != 0 {
        return Err(libc::EINVAL);
    }
    if fl & libc::O_ACCMODE != libc::O_RDONLY {
        return Err(libc::EACCES);
    }
    // SAFETY: fstatfs only writes the provided struct.
    let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(fd.as_raw_fd(), &mut sfs) } != 0 {
        return Err(errno());
    }
    if sfs.f_type as i64 == FUSE_SUPER_MAGIC {
        return Err(libc::ELOOP);
    }
    Ok(())
}

pub struct Server<K: Kernel> {
    kernel: K,
    uid: u32,
    capable: bool,
    fuse: Option<OwnedFd>,
    /// Ids this helper may close: the ones it registered, plus adopted ones.
    ids: BTreeSet<u32>,
    opened_any: bool,
}

impl<K: Kernel> Server<K> {
    /// `uid`: the owner backing files must have (the helper's real uid).
    /// `capable`: whether the helper holds CAP_SYS_ADMIN — reported to the
    /// daemon in the Setup reply, so it can fall back without trying.
    pub fn new(kernel: K, uid: u32, capable: bool) -> Self {
        Server { kernel, uid, capable, fuse: None, ids: BTreeSet::new(), opened_any: false }
    }

    /// Answers one request. `fd` is the descriptor that came with it (the
    /// parser has already checked one came exactly when the op needs one); it
    /// is closed when this returns, except the /dev/fuse one Setup keeps.
    pub fn handle(&mut self, req: Request, fd: Option<OwnedFd>) -> Result<u32, i32> {
        match req {
            Request::Setup => {
                let fd = fd.ok_or(libc::EINVAL)?;
                if self.fuse.is_some() {
                    return Err(libc::EALREADY);
                }
                if !self.kernel.is_fuse_dev(fd.as_fd()) {
                    return Err(libc::ENODEV);
                }
                self.fuse = Some(fd);
                Ok(self.capable as u32)
            }
            Request::Adopt(id) => {
                if self.fuse.is_none() {
                    return Err(libc::ENOTCONN);
                }
                if self.opened_any {
                    return Err(libc::EPERM);
                }
                self.ids.insert(id);
                Ok(0)
            }
            Request::Open => {
                let file = fd.ok_or(libc::EINVAL)?;
                let fuse = self.fuse.as_ref().ok_or(libc::ENOTCONN)?;
                validate_backing_fd(file.as_fd(), self.uid)?;
                self.opened_any = true;
                let id = self.kernel.backing_open(fuse.as_fd(), file.as_fd())?;
                self.ids.insert(id);
                Ok(id)
            }
            Request::Close(id) => {
                let fuse = self.fuse.as_ref().ok_or(libc::ENOTCONN)?;
                if !self.ids.remove(&id) {
                    return Err(libc::ENOENT);
                }
                self.kernel.backing_close(fuse.as_fd(), id)?;
                Ok(0)
            }
        }
    }
}

/// One received message: payload length, the descriptor(s) that came with
/// it, and whether anything was cut off.
struct Received {
    len: usize,
    fds: [Option<OwnedFd>; 1],
    extra_fds: bool,
    truncated: bool,
}

fn recv_one(sock: BorrowedFd<'_>, buf: &mut [u8]) -> io::Result<Option<Received>> {
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr() as *mut libc::c_void, iov_len: buf.len() };
    // Room for exactly one descriptor: more arrive as MSG_CTRUNC, and the
    // kernel drops the ones that do not fit.
    let mut cbuf = [0u64; 4];
    // SAFETY: an all-zero msghdr is a valid empty message.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
    // SAFETY: CMSG_SPACE only computes a size.
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as _;
    let n = loop {
        // SAFETY: msg points at live buffers for the duration of the call.
        let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(e);
        }
        break n as usize;
    };
    let truncated = msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0;
    let mut out = Received { len: n, fds: [None], extra_fds: false, truncated };
    // SAFETY: walks the control buffer the kernel just filled, within msg_controllen.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c) as *const RawFd;
                let count = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / std::mem::size_of::<RawFd>();
                for i in 0..count {
                    let fd = OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(i)));
                    if out.fds[0].is_none() {
                        out.fds[0] = Some(fd);
                    } else {
                        out.extra_fds = true; // dropped (closed) here
                    }
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    if n == 0 && out.fds[0].is_none() && !truncated {
        return Ok(None); // the daemon closed its end
    }
    Ok(Some(out))
}

fn send_reply(sock: BorrowedFd<'_>, r: Result<u32, i32>) -> io::Result<()> {
    let b = proto::encode_reply(r);
    // SAFETY: sends the 8 bytes of `b`.
    let n = unsafe { libc::send(sock.as_raw_fd(), b.as_ptr() as *const libc::c_void, b.len(), libc::MSG_NOSIGNAL) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Serves requests until the daemon closes its end (Ok) or the socket fails.
pub fn serve<K: Kernel>(sock: BorrowedFd<'_>, server: &mut Server<K>) -> io::Result<()> {
    // One byte more than a request, so an over-long message shows as a length
    // mismatch rather than only as MSG_TRUNC.
    let mut buf = [0u8; proto::REQ_LEN + 1];
    loop {
        let Some(mut m) = recv_one(sock, &mut buf)? else { return Ok(()) };
        let nfds = m.fds[0].is_some() as usize + m.extra_fds as usize;
        let r = proto::parse_request(&buf[..m.len], nfds, m.truncated).and_then(|req| server.handle(req, m.fds[0].take()));
        // Any descriptor not kept by Setup is closed here, before the reply.
        drop(m);
        send_reply(sock, r)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, OpenOptions};

    /// Pretends every descriptor is /dev/fuse and hands out sequential ids.
    #[derive(Default)]
    struct Mock {
        next: u32,
        closed: Vec<u32>,
    }

    impl Kernel for Mock {
        fn is_fuse_dev(&self, _fd: BorrowedFd<'_>) -> bool {
            true
        }
        fn backing_open(&mut self, _fuse: BorrowedFd<'_>, _file: BorrowedFd<'_>) -> Result<u32, i32> {
            self.next += 1;
            Ok(self.next)
        }
        fn backing_close(&mut self, _fuse: BorrowedFd<'_>, id: u32) -> Result<(), i32> {
            self.closed.push(id);
            Ok(())
        }
    }

    fn uid() -> u32 {
        // SAFETY: getuid cannot fail.
        unsafe { libc::getuid() }
    }

    fn tmp_file() -> (tempfile::TempDir, std::path::PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("f");
        std::fs::write(&p, b"data").unwrap();
        (d, p)
    }

    fn setup() -> Server<Mock> {
        let mut s = Server::new(Mock::default(), uid(), true);
        let fake_fuse: OwnedFd = File::open("/dev/null").unwrap().into();
        assert_eq!(s.handle(Request::Setup, Some(fake_fuse)), Ok(1));
        s
    }

    #[test]
    fn dev_null_is_not_dev_fuse() {
        assert!(!is_fuse_dev(File::open("/dev/null").unwrap().as_fd()));
    }

    #[test]
    fn read_only_regular_file_of_own_uid_is_accepted() {
        let (_d, p) = tmp_file();
        assert_eq!(validate_backing_fd(File::open(&p).unwrap().as_fd(), uid()), Ok(()));
    }

    #[test]
    fn directory_is_refused() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(validate_backing_fd(File::open(d.path()).unwrap().as_fd(), uid()), Err(libc::EINVAL));
    }

    #[test]
    fn char_device_is_refused() {
        assert_eq!(validate_backing_fd(File::open("/dev/null").unwrap().as_fd(), uid()), Err(libc::EINVAL));
    }

    #[test]
    fn writable_file_is_refused() {
        let (_d, p) = tmp_file();
        let rw = OpenOptions::new().read(true).write(true).open(&p).unwrap();
        assert_eq!(validate_backing_fd(rw.as_fd(), uid()), Err(libc::EACCES));
        let wo = OpenOptions::new().write(true).open(&p).unwrap();
        assert_eq!(validate_backing_fd(wo.as_fd(), uid()), Err(libc::EACCES));
    }

    #[test]
    fn o_path_descriptor_is_refused() {
        use std::os::unix::fs::OpenOptionsExt;
        let (_d, p) = tmp_file();
        let f = OpenOptions::new().read(true).custom_flags(libc::O_PATH).open(&p).unwrap();
        // fstat works on O_PATH (Linux 3.6+); the flag check refuses it.
        assert_eq!(validate_backing_fd(f.as_fd(), uid()), Err(libc::EINVAL));
    }

    #[test]
    fn file_of_another_uid_is_refused() {
        let (_d, p) = tmp_file();
        assert_eq!(validate_backing_fd(File::open(&p).unwrap().as_fd(), uid().wrapping_add(1)), Err(libc::EPERM));
    }

    #[test]
    fn requests_before_setup_are_refused() {
        let mut s = Server::new(Mock::default(), uid(), true);
        let (_d, p) = tmp_file();
        assert_eq!(s.handle(Request::Open, Some(File::open(&p).unwrap().into())), Err(libc::ENOTCONN));
        assert_eq!(s.handle(Request::Close(1), None), Err(libc::ENOTCONN));
        assert_eq!(s.handle(Request::Adopt(1), None), Err(libc::ENOTCONN));
    }

    #[test]
    fn second_setup_is_refused() {
        let mut s = setup();
        assert_eq!(s.handle(Request::Setup, Some(File::open("/dev/null").unwrap().into())), Err(libc::EALREADY));
    }

    #[test]
    fn setup_reports_a_missing_capability() {
        let mut s = Server::new(Mock::default(), uid(), false);
        assert_eq!(s.handle(Request::Setup, Some(File::open("/dev/null").unwrap().into())), Ok(0));
    }

    #[test]
    fn setup_with_a_non_fuse_descriptor_is_refused() {
        let mut s = Server::new(DevFuse, uid(), true);
        assert_eq!(s.handle(Request::Setup, Some(File::open("/dev/null").unwrap().into())), Err(libc::ENODEV));
    }

    #[test]
    fn open_then_close_round_trips_and_only_once() {
        let mut s = setup();
        let (_d, p) = tmp_file();
        let id = s.handle(Request::Open, Some(File::open(&p).unwrap().into())).unwrap();
        assert_eq!(s.handle(Request::Close(id), None), Ok(0));
        assert_eq!(s.kernel.closed, vec![id]);
        assert_eq!(s.handle(Request::Close(id), None), Err(libc::ENOENT));
        assert_eq!(s.kernel.closed, vec![id]);
    }

    #[test]
    fn close_of_an_unknown_id_is_refused() {
        let mut s = setup();
        assert_eq!(s.handle(Request::Close(77), None), Err(libc::ENOENT));
        assert!(s.kernel.closed.is_empty());
    }

    #[test]
    fn rejected_open_registers_nothing() {
        let mut s = setup();
        let d = tempfile::tempdir().unwrap();
        assert_eq!(s.handle(Request::Open, Some(File::open(d.path()).unwrap().into())), Err(libc::EINVAL));
        assert_eq!(s.kernel.next, 0);
    }

    #[test]
    fn adopted_ids_are_closable_but_only_adoptable_before_the_first_open() {
        let mut s = setup();
        assert_eq!(s.handle(Request::Adopt(40), None), Ok(0));
        let (_d, p) = tmp_file();
        s.handle(Request::Open, Some(File::open(&p).unwrap().into())).unwrap();
        assert_eq!(s.handle(Request::Adopt(41), None), Err(libc::EPERM));
        assert_eq!(s.handle(Request::Close(40), None), Ok(0));
        assert_eq!(s.handle(Request::Close(41), None), Err(libc::ENOENT));
    }

    #[test]
    fn serve_loop_over_a_socketpair() {
        use crate::client::Channel;
        let (ours, theirs) = Channel::pair().unwrap();
        let t = std::thread::spawn(move || {
            let mut s = Server::new(Mock::default(), uid(), true);
            serve(theirs.as_fd(), &mut s).map(|()| s.kernel.closed)
        });
        let fake_fuse = File::open("/dev/null").unwrap();
        assert_eq!(ours.call(Request::Setup, Some(fake_fuse.as_fd())).unwrap(), Ok(1));
        let (_d, p) = tmp_file();
        let id = ours.call(Request::Open, Some(File::open(&p).unwrap().as_fd())).unwrap().unwrap();
        // A malformed message gets EINVAL and the loop keeps serving.
        assert_eq!(ours.call_raw(&[1, 2, 3], None).unwrap(), Err(libc::EINVAL));
        // Two descriptors where one is allowed: refused, both closed.
        let a = File::open(&p).unwrap();
        let b = File::open(&p).unwrap();
        assert_eq!(ours.call_raw_fds(&Request::Open.encode(), &[a.as_fd(), b.as_fd()]).unwrap(), Err(libc::EINVAL));
        assert_eq!(ours.call(Request::Close(id + 100), None).unwrap(), Err(libc::ENOENT));
        assert_eq!(ours.call(Request::Close(id), None).unwrap(), Ok(0));
        drop(ours);
        assert_eq!(t.join().unwrap().unwrap(), vec![id]);
    }
}
