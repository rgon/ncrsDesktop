use std::io;
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex};

const FUSE_NOTIFY_INVAL_INODE: i32 = 2;
const FUSE_NOTIFY_STORE: i32 = 3;
const FUSE_NOTIFY_DELETE: i32 = 6;

#[repr(C)]
struct FuseOutHeader {
    len: u32,
    error: i32,
    unique: u64,
}

#[repr(C)]
struct FuseNotifyInvalInodeOut {
    ino: u64,
    off: i64,
    len: i64,
}

#[repr(C)]
struct FuseNotifyStoreOut {
    nodeid: u64,
    offset: u64,
    size: u32,
    padding: u32,
}

#[repr(C)]
struct FuseNotifyDeleteOut {
    parent: u64,
    child: u64,
    namelen: u32,
    padding: u32,
}

pub struct FuseNotifier {
    fd: std::fs::File,
}

unsafe impl Send for FuseNotifier {}
unsafe impl Sync for FuseNotifier {}

impl FuseNotifier {
    pub fn new(fd: std::fs::File) -> Self {
        Self { fd }
    }

    pub fn notify_inval_inode(&self, ino: u64, offset: i64, len: i64) -> io::Result<()> {
        let header = FuseOutHeader {
            len: (std::mem::size_of::<FuseOutHeader>() + std::mem::size_of::<FuseNotifyInvalInodeOut>()) as u32,
            error: FUSE_NOTIFY_INVAL_INODE,
            unique: 0,
        };
        let body = FuseNotifyInvalInodeOut { ino, off: offset, len };

        let header_slice = unsafe {
            std::slice::from_raw_parts(
                &header as *const _ as *const u8,
                std::mem::size_of::<FuseOutHeader>(),
            )
        };
        let body_slice = unsafe {
            std::slice::from_raw_parts(
                &body as *const _ as *const u8,
                std::mem::size_of::<FuseNotifyInvalInodeOut>(),
            )
        };

        let iov = [
            io::IoSlice::new(header_slice),
            io::IoSlice::new(body_slice),
        ];

        let rc = unsafe {
            libc::writev(
                self.fd.as_raw_fd(),
                iov.as_ptr() as *const libc::iovec,
                2,
            )
        };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Push `data` into the kernel's page cache for the given inode at `offset`.
    /// After this call, a userspace read at the same range is served by the kernel
    /// without ever calling our FUSE read() handler — zero network round-trips.
    pub fn notify_store(&self, ino: u64, offset: u64, data: &[u8]) -> io::Result<()> {
        let body_size = std::mem::size_of::<FuseNotifyStoreOut>();
        let header = FuseOutHeader {
            len: (std::mem::size_of::<FuseOutHeader>() + body_size + data.len()) as u32,
            error: FUSE_NOTIFY_STORE,
            unique: 0,
        };
        let body = FuseNotifyStoreOut { nodeid: ino, offset, size: data.len() as u32, padding: 0 };
        let header_slice = unsafe {
            std::slice::from_raw_parts(&header as *const _ as *const u8, std::mem::size_of::<FuseOutHeader>())
        };
        let body_slice = unsafe {
            std::slice::from_raw_parts(&body as *const _ as *const u8, body_size)
        };
        let iov = [io::IoSlice::new(header_slice), io::IoSlice::new(body_slice), io::IoSlice::new(data)];
        let rc = unsafe { libc::writev(self.fd.as_raw_fd(), iov.as_ptr() as *const libc::iovec, 3) };
        if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    pub fn notify_delete(&self, parent: u64, child: u64, name: &[u8]) -> io::Result<()> {
        let body_size = std::mem::size_of::<FuseNotifyDeleteOut>();
        let header = FuseOutHeader {
            len: (std::mem::size_of::<FuseOutHeader>() + body_size + name.len()) as u32,
            error: FUSE_NOTIFY_DELETE,
            unique: 0,
        };
        let body = FuseNotifyDeleteOut {
            parent,
            child,
            namelen: name.len() as u32,
            padding: 0,
        };

        let header_slice = unsafe {
            std::slice::from_raw_parts(
                &header as *const _ as *const u8,
                std::mem::size_of::<FuseOutHeader>(),
            )
        };
        let body_slice = unsafe {
            std::slice::from_raw_parts(
                &body as *const _ as *const u8,
                body_size,
            )
        };

        let iov = [
            io::IoSlice::new(header_slice),
            io::IoSlice::new(body_slice),
            io::IoSlice::new(name),
        ];

        let rc = unsafe {
            libc::writev(
                self.fd.as_raw_fd(),
                iov.as_ptr() as *const libc::iovec,
                3,
            )
        };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

pub type NotifierSlot = Arc<Mutex<Option<Arc<FuseNotifier>>>>;

pub fn new_notifier_slot() -> NotifierSlot {
    Arc::new(Mutex::new(None))
}

pub fn find_fuse_fd() -> Option<i32> {
    let proc_fd = std::path::Path::new("/proc/self/fd");
    let mut best: Option<i32> = None;
    if let Ok(entries) = std::fs::read_dir(proc_fd) {
        for entry in entries.flatten() {
            if let Ok(fd_num) = entry.file_name().to_string_lossy().parse::<i32>() {
                if let Ok(target) = std::fs::read_link(entry.path()) {
                    if target.to_string_lossy().contains("/dev/fuse") {
                        best = Some(best.map_or(fd_num, |prev: i32| prev.max(fd_num)));
                    }
                }
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::FromRawFd;

    fn pipe_notifier() -> (FuseNotifier, std::fs::File) {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let write_file = unsafe { std::fs::File::from_raw_fd(fds[1]) };
        let read_file = unsafe { std::fs::File::from_raw_fd(fds[0]) };
        (FuseNotifier::new(write_file), read_file)
    }

    #[test]
    fn notify_inval_inode_writes_correct_protocol_bytes() {
        use std::io::Read;
        let (notifier, mut reader) = pipe_notifier();

        notifier.notify_inval_inode(42, 0, 0).unwrap();

        let header_size = std::mem::size_of::<FuseOutHeader>();
        let body_size = std::mem::size_of::<FuseNotifyInvalInodeOut>();
        let total = header_size + body_size;

        let mut buf = vec![0u8; total];
        reader.read_exact(&mut buf).unwrap();

        let len = u32::from_ne_bytes(buf[0..4].try_into().unwrap());
        assert_eq!(len as usize, total);

        let error = i32::from_ne_bytes(buf[4..8].try_into().unwrap());
        assert_eq!(error, FUSE_NOTIFY_INVAL_INODE);

        let unique = u64::from_ne_bytes(buf[8..16].try_into().unwrap());
        assert_eq!(unique, 0);

        let ino = u64::from_ne_bytes(buf[16..24].try_into().unwrap());
        assert_eq!(ino, 42);

        let off = i64::from_ne_bytes(buf[24..32].try_into().unwrap());
        assert_eq!(off, 0);

        let notify_len = i64::from_ne_bytes(buf[32..40].try_into().unwrap());
        assert_eq!(notify_len, 0);
    }

    #[test]
    fn notify_delete_writes_correct_protocol_bytes() {
        use std::io::Read;
        let (notifier, mut reader) = pipe_notifier();
        let name = b"removed.txt";

        notifier.notify_delete(10, 42, name).unwrap();

        let header_size = std::mem::size_of::<FuseOutHeader>();
        let body_size = std::mem::size_of::<FuseNotifyDeleteOut>();
        let total = header_size + body_size + name.len();

        let mut buf = vec![0u8; total];
        reader.read_exact(&mut buf).unwrap();

        let len = u32::from_ne_bytes(buf[0..4].try_into().unwrap());
        assert_eq!(len as usize, total);

        let error = i32::from_ne_bytes(buf[4..8].try_into().unwrap());
        assert_eq!(error, FUSE_NOTIFY_DELETE);

        let unique = u64::from_ne_bytes(buf[8..16].try_into().unwrap());
        assert_eq!(unique, 0);

        let parent = u64::from_ne_bytes(buf[16..24].try_into().unwrap());
        assert_eq!(parent, 10);

        let child = u64::from_ne_bytes(buf[24..32].try_into().unwrap());
        assert_eq!(child, 42);

        let namelen = u32::from_ne_bytes(buf[32..36].try_into().unwrap());
        assert_eq!(namelen as usize, name.len());

        let padding = u32::from_ne_bytes(buf[36..40].try_into().unwrap());
        assert_eq!(padding, 0);

        assert_eq!(&buf[40..], name);
    }

    #[test]
    fn notifier_slot_starts_empty() {
        let slot = new_notifier_slot();
        assert!(slot.lock().unwrap().is_none());
    }

    #[test]
    fn notifier_slot_populated_and_usable() {
        use std::io::Read;
        let slot = new_notifier_slot();
        let (notifier, mut reader) = pipe_notifier();
        *slot.lock().unwrap() = Some(Arc::new(notifier));

        if let Some(n) = slot.lock().unwrap().as_ref() {
            n.notify_inval_inode(99, 0, 0).unwrap();
        }

        let total = std::mem::size_of::<FuseOutHeader>() + std::mem::size_of::<FuseNotifyInvalInodeOut>();
        let mut buf = vec![0u8; total];
        reader.read_exact(&mut buf).unwrap();
        let ino = u64::from_ne_bytes(buf[16..24].try_into().unwrap());
        assert_eq!(ino, 99);
    }
}
