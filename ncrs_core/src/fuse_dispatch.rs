//! The `Filesystem` fuser drives: on its one dispatch thread, each request names
//! the inodes it touches (from the in-memory cache, never the network) and is
//! queued on them in `fuse_sched`; the body in lib.rs then runs on a `bg::FUSE`
//! worker. Per inode, requests run one at a time in arrival order, except that
//! consecutive reads run together; requests on other inodes run in parallel.
//!
//! Keys, by inode number (paths change under a rename, inode numbers do not):
//! - rename: old parent, new parent, the source, the replaced target if known,
//!   and every inode under a moved folder that has a request in flight;
//! - unlink, rmdir, create, mkdir: the parent and the child if it has an inode;
//! - lookup: the parent and the child if known, shared;
//! - everything else: its own inode (directory requests: the directory's).
//!
//! Shared: lookup, getattr, getxattr, listxattr, a read-only open, opendir,
//! readdir(plus), releasedir, read. A read is also exclusive on its file handle:
//! `read()` updates the handle's read-ahead state (`next_expected_off`, the
//! windows) in several separate steps, so two reads on one handle must not
//! interleave; reads on different handles of the same file still run together.
//!
//! A folder rename also orders later requests under it: until the rename has run,
//! a request on an inode inside the source or destination tree is queued behind it
//! on the folder's inode as well. Without that, a RELEASE of a file in the folder
//! arriving just after the RENAME could commit in between rename's re-pointing of
//! open handles and its MOVE, and recreate the old folder on the server.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use fuser::{
    BsdFileFlags, FileHandle, Filesystem, INodeNo, KernelConfig, LockOwner, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite,
    ReplyXattr, Request, TimeOrNow, WriteFlags,
};

use crate::fuse_sched::{Mode, Scheduler};
use crate::{bg, MutexExt, NextCloudFs};

/// What a request is ordered on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Key {
    Ino(u64),
    Fh(u64),
}

/// The parts of a fuser `Request` the bodies use, owned so they can travel to a worker.
pub(crate) struct Req {
    pid: u32,
}

impl Req {
    fn of(req: &Request) -> Req {
        Req { pid: req.pid() }
    }

    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }
}

/// A rename in flight, for ordering requests under a moved folder (see the module doc).
struct Move {
    id: u64,
    ino: u64,
    from: PathBuf,
    to: PathBuf,
}

#[derive(Default)]
struct Moves {
    next_id: u64,
    live: Vec<Move>,
}

/// Drops its `Move` when the rename's task ends, however it ends.
struct MoveDone {
    moves: Arc<Mutex<Moves>>,
    id: u64,
}

impl Drop for MoveDone {
    fn drop(&mut self) {
        self.moves.safe_lock().live.retain(|m| m.id != self.id);
    }
}

pub(crate) struct SchedFs {
    fs: Arc<NextCloudFs>,
    sched: Arc<Scheduler<Key>>,
    moves: Arc<Mutex<Moves>>,
}

const X: Mode = Mode::Exclusive;
const S: Mode = Mode::Shared;

impl SchedFs {
    pub(crate) fn new(fs: NextCloudFs) -> SchedFs {
        SchedFs {
            fs: Arc::new(fs),
            sched: Scheduler::new(&bg::FUSE, bg::FUSE_MAX_INFLIGHT),
            moves: Arc::new(Mutex::new(Moves::default())),
        }
    }

    /// Queues `job` on `keys` (plus any moved folder they sit under) and returns.
    fn queue(&self, mut keys: Vec<(Key, Mode)>, job: impl FnOnce(&NextCloudFs) + Send + 'static) {
        self.order_under_moves(&mut keys);
        let fs = Arc::clone(&self.fs);
        self.sched.submit(&keys, move || job(&fs));
    }

    fn order_under_moves(&self, keys: &mut Vec<(Key, Mode)>) {
        let moves = self.moves.safe_lock();
        if moves.live.is_empty() {
            return;
        }
        let c = self.fs.cache.safe_lock();
        let mut extra = Vec::new();
        for (k, _) in keys.iter() {
            let Key::Ino(ino) = *k else { continue };
            let Some(path) = c.get_path(ino) else { continue };
            for m in &moves.live {
                if path.starts_with(&m.from) || path.starts_with(&m.to) {
                    extra.push((Key::Ino(m.ino), S));
                }
            }
        }
        keys.extend(extra);
    }

    /// The inode of `name` in `parent`, if one was ever handed out.
    fn child(&self, parent: INodeNo, name: &OsStr) -> Option<u64> {
        let c = self.fs.cache.safe_lock();
        c.get_path(parent.0).and_then(|p| c.get_inode(&p.join(name)))
    }

    /// Parent exclusive, plus the existing child: create, mkdir, unlink, rmdir.
    fn entry_keys(&self, parent: INodeNo, name: &OsStr) -> Vec<(Key, Mode)> {
        let mut keys = vec![(Key::Ino(parent.0), X)];
        keys.extend(self.child(parent, name).map(|c| (Key::Ino(c), X)));
        keys
    }
}

impl Filesystem for SchedFs {
    // Inline on the dispatch thread: no other request can be in flight yet.
    fn init(&mut self, req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        log::info!("FUSE requests run on {} workers, ordered per inode", bg::FUSE_WORKERS);
        self.fs.init(&Req::of(req), config)
    }

    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        // A lookup in a folder with no usable listing waits on its PROPFIND. Taking the
        // folder exclusively parks the other lookups for it in its queue instead of on
        // workers, so a burst of them (a file manager stat'ing every entry) holds one
        // worker rather than all of them, and they then run together from that listing.
        let listed = {
            let c = self.fs.cache.safe_lock();
            c.get_path(parent.0).is_none_or(|p| c.get_cached_dir_readonly(&p).is_some())
        };
        let mut keys = vec![(Key::Ino(parent.0), if listed { S } else { X })];
        keys.extend(self.child(parent, name).map(|c| (Key::Ino(c), S)));
        let (r, name) = (Req::of(req), name.to_os_string());
        self.queue(keys, move |fs| fs.lookup(&r, parent, &name, reply));
    }

    fn getattr(&self, req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        let r = Req::of(req);
        self.queue(vec![(Key::Ino(ino.0), S)], move |fs| fs.getattr(&r, ino, fh, reply));
    }

    fn getxattr(&self, req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let (r, name) = (Req::of(req), name.to_os_string());
        self.queue(vec![(Key::Ino(ino.0), S)], move |fs| fs.getxattr(&r, ino, &name, size, reply));
    }

    fn listxattr(&self, req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let r = Req::of(req);
        self.queue(vec![(Key::Ino(ino.0), S)], move |fs| fs.listxattr(&r, ino, size, reply));
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        // A writable or truncating open stages the file's content: exclusive.
        let writes = flags.0 & (libc::O_WRONLY | libc::O_RDWR | libc::O_APPEND | libc::O_TRUNC) != 0;
        let r = Req::of(req);
        self.queue(vec![(Key::Ino(ino.0), if writes { X } else { S })], move |fs| fs.open(&r, ino, flags, reply));
    }

    fn read(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        flags: OpenFlags,
        lock: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let r = Req::of(req);
        let keys = vec![(Key::Ino(ino.0), S), (Key::Fh(fh.0), X)];
        self.queue(keys, move |fs| fs.read(&r, ino, fh, offset, size, flags, lock, reply));
    }

    fn release(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        flags: OpenFlags,
        lock_owner: Option<LockOwner>,
        flush: bool,
        reply: ReplyEmpty,
    ) {
        let r = Req::of(req);
        self.queue(vec![(Key::Ino(ino.0), X)], move |fs| fs.release(&r, ino, fh, flags, lock_owner, flush, reply));
    }

    fn opendir(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let r = Req::of(req);
        self.queue(vec![(Key::Ino(ino.0), S)], move |fs| fs.opendir(&r, ino, flags, reply));
    }

    fn releasedir(&self, req: &Request, ino: INodeNo, fh: FileHandle, flags: OpenFlags, reply: ReplyEmpty) {
        let r = Req::of(req);
        self.queue(vec![(Key::Ino(ino.0), S)], move |fs| fs.releasedir(&r, ino, fh, flags, reply));
    }

    fn readdir(&self, req: &Request, ino: INodeNo, fh: FileHandle, offset: u64, reply: ReplyDirectory) {
        let r = Req::of(req);
        self.queue(vec![(Key::Ino(ino.0), S)], move |fs| fs.readdir(&r, ino, fh, offset, reply));
    }

    fn readdirplus(&self, req: &Request, ino: INodeNo, fh: FileHandle, offset: u64, reply: ReplyDirectoryPlus) {
        let r = Req::of(req);
        self.queue(vec![(Key::Ino(ino.0), S)], move |fs| fs.readdirplus(&r, ino, fh, offset, reply));
    }

    fn setattr(
        &self,
        req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        crtime: Option<SystemTime>,
        chgtime: Option<SystemTime>,
        bkuptime: Option<SystemTime>,
        flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let r = Req::of(req);
        self.queue(vec![(Key::Ino(ino.0), X)], move |fs| {
            fs.setattr(&r, ino, mode, uid, gid, size, atime, mtime, ctime, fh, crtime, chgtime, bkuptime, flags, reply)
        });
    }

    fn write(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        write_flags: WriteFlags,
        flags: OpenFlags,
        lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let (r, data) = (Req::of(req), data.to_vec());
        self.queue(vec![(Key::Ino(ino.0), X)], move |fs| {
            fs.write(&r, ino, fh, offset, &data, write_flags, flags, lock_owner, reply)
        });
    }

    fn flush(&self, req: &Request, ino: INodeNo, fh: FileHandle, lock_owner: LockOwner, reply: ReplyEmpty) {
        let r = Req::of(req);
        self.queue(vec![(Key::Ino(ino.0), X)], move |fs| fs.flush(&r, ino, fh, lock_owner, reply));
    }

    fn fsync(&self, req: &Request, ino: INodeNo, fh: FileHandle, datasync: bool, reply: ReplyEmpty) {
        let r = Req::of(req);
        self.queue(vec![(Key::Ino(ino.0), X)], move |fs| fs.fsync(&r, ino, fh, datasync, reply));
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let keys = self.entry_keys(parent, name);
        let (r, name) = (Req::of(req), name.to_os_string());
        self.queue(keys, move |fs| fs.create(&r, parent, &name, mode, umask, flags, reply));
    }

    fn mkdir(&self, req: &Request, parent: INodeNo, name: &OsStr, mode: u32, umask: u32, reply: ReplyEntry) {
        let keys = self.entry_keys(parent, name);
        let (r, name) = (Req::of(req), name.to_os_string());
        self.queue(keys, move |fs| fs.mkdir(&r, parent, &name, mode, umask, reply));
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let keys = self.entry_keys(parent, name);
        let (r, name) = (Req::of(req), name.to_os_string());
        self.queue(keys, move |fs| fs.unlink(&r, parent, &name, reply));
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let keys = self.entry_keys(parent, name);
        let (r, name) = (Req::of(req), name.to_os_string());
        self.queue(keys, move |fs| fs.rmdir(&r, parent, &name, reply));
    }

    fn rename(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let mut keys = vec![(Key::Ino(parent.0), X), (Key::Ino(newparent.0), X)];
        let busy = self.sched.busy_keys();
        let tree = {
            let c = self.fs.cache.safe_lock();
            let from = c.get_path(parent.0).map(|p| p.join(name));
            let to = c.get_path(newparent.0).map(|p| p.join(newname));
            let src = from.as_ref().and_then(|p| c.get_inode(p));
            keys.extend(src.map(|i| (Key::Ino(i), X)));
            keys.extend(to.as_ref().and_then(|p| c.get_inode(p)).map(|i| (Key::Ino(i), X)));
            // Requests already queued inside a moved folder finish first.
            if let (Some(src), Some(from)) = (src, from.as_ref()) {
                for k in busy {
                    let Key::Ino(i) = k else { continue };
                    if i != src && c.get_path(i).is_some_and(|p| p.starts_with(from)) {
                        keys.push((k, X));
                    }
                }
            }
            match (src, from, to) {
                (Some(ino), Some(from), Some(to)) => Some((ino, from, to)),
                _ => None,
            }
        };
        // Registered before queueing, so the rename cannot end (and drop it) first.
        let done = tree.map(|(ino, from, to)| {
            let mut m = self.moves.safe_lock();
            m.next_id += 1;
            let id = m.next_id;
            m.live.push(Move { id, ino, from, to });
            MoveDone { moves: Arc::clone(&self.moves), id }
        });
        let (r, name, newname): (Req, OsString, OsString) = (Req::of(req), name.to_os_string(), newname.to_os_string());
        self.queue(keys, move |fs| {
            let _done = done;
            fs.rename(&r, parent, &name, newparent, &newname, flags, reply)
        });
    }
}
