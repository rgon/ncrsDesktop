//! Mirror of the kernel's per-inode FUSE I/O mode (fs/fuse/iomode.c): it EIOs an open that mixes
//! passthrough with cached opens, or gives one inode two backing files. DIRECT_IO opens are always
//! allowed, so they absorb any conflict. The kernel drops a mode before FUSE_RELEASE, so these counts never undershoot.

use std::collections::HashMap;
use std::sync::Arc;

/// On-disk identity of a backing file, so a since-replaced cache copy is never read through a stale backing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileId {
    dev: u64,
    ino: u64,
    len: u64,
    mtime_ns: i128,
}

impl FileId {
    pub(crate) fn of(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        FileId {
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
            mtime_ns: meta.mtime() as i128 * 1_000_000_000 + meta.mtime_nsec() as i128,
        }
    }
}

/// How one open handle was answered; handed back to [`InodeIoModes::release`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IoKind {
    Passthrough,
    Cached,
    DirectIo,
}

pub(crate) enum IoGrant<B> {
    Passthrough(Arc<B>),
    Cached,
    DirectIo,
}

impl<B> IoGrant<B> {
    pub(crate) fn kind(&self) -> IoKind {
        match self {
            IoGrant::Passthrough(_) => IoKind::Passthrough,
            IoGrant::Cached => IoKind::Cached,
            IoGrant::DirectIo => IoKind::DirectIo,
        }
    }
}

enum Mode<B> {
    Passthrough { backing: Arc<B>, file: FileId, opens: usize },
    Cached { opens: usize },
}

pub(crate) struct InodeIoModes<B> {
    modes: HashMap<u64, Mode<B>>,
}

impl<B> Default for InodeIoModes<B> {
    fn default() -> Self {
        InodeIoModes { modes: HashMap::new() }
    }
}

impl<B> InodeIoModes<B> {
    /// `passthrough` is set for an eligible open; `register` runs only if the inode has no backing yet.
    pub(crate) fn acquire<F>(
        &mut self,
        ino: u64,
        passthrough: Option<(FileId, F)>,
    ) -> (IoGrant<B>, Option<std::io::Error>)
    where
        F: FnOnce() -> std::io::Result<B>,
    {
        match self.modes.get_mut(&ino) {
            Some(Mode::Passthrough { backing, file, opens }) => match passthrough {
                Some((f, _)) if f == *file => {
                    *opens += 1;
                    (IoGrant::Passthrough(backing.clone()), None)
                }
                _ => (IoGrant::DirectIo, None),
            },
            Some(Mode::Cached { opens }) => {
                *opens += 1;
                (IoGrant::Cached, None)
            }
            None => {
                let mut err = None;
                if let Some((file, register)) = passthrough {
                    match register() {
                        Ok(b) => {
                            let backing = Arc::new(b);
                            self.modes.insert(ino, Mode::Passthrough { backing: backing.clone(), file, opens: 1 });
                            return (IoGrant::Passthrough(backing), None);
                        }
                        Err(e) => err = Some(e),
                    }
                }
                self.modes.insert(ino, Mode::Cached { opens: 1 });
                (IoGrant::Cached, err)
            }
        }
    }

    /// [`acquire`](Self::acquire) for an open that must not use passthrough.
    pub(crate) fn acquire_plain(&mut self, ino: u64) -> IoKind {
        self.acquire(ino, None::<(FileId, fn() -> std::io::Result<B>)>).0.kind()
    }

    /// The inode's shared backing id is dropped with its last passthrough open.
    pub(crate) fn release(&mut self, ino: u64, kind: IoKind) {
        let now_unused = match (self.modes.get_mut(&ino), kind) {
            (Some(Mode::Passthrough { opens, .. }), IoKind::Passthrough)
            | (Some(Mode::Cached { opens }), IoKind::Cached) => {
                *opens = opens.saturating_sub(1);
                *opens == 0
            }
            _ => false,
        };
        if now_unused {
            self.modes.remove(&ino);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn fid(ino: u64) -> FileId {
        FileId { dev: 1, ino, len: 10, mtime_ns: 5 }
    }

    type Reg = fn() -> std::io::Result<u32>;

    fn ok_reg() -> std::io::Result<u32> {
        Ok(7)
    }

    #[test]
    fn concurrent_passthrough_opens_share_one_backing() {
        let mut m = InodeIoModes::<u32>::default();
        let calls = Cell::new(0);
        let reg = || {
            calls.set(calls.get() + 1);
            Ok(7)
        };
        let (a, _) = m.acquire(1, Some((fid(9), reg)));
        let (b, _) = m.acquire(1, Some((fid(9), reg)));
        assert_eq!(calls.get(), 1, "second open must reuse the registered backing");
        match (a, b) {
            (IoGrant::Passthrough(x), IoGrant::Passthrough(y)) => assert!(Arc::ptr_eq(&x, &y)),
            _ => panic!("both opens should be passthrough"),
        }
    }

    #[test]
    fn cached_open_while_passthrough_active_is_direct_io() {
        let mut m = InodeIoModes::<u32>::default();
        m.acquire(1, Some((fid(9), ok_reg as Reg)));
        let (g, _) = m.acquire(1, None::<(FileId, Reg)>);
        assert_eq!(g.kind(), IoKind::DirectIo);
    }

    #[test]
    fn replaced_cache_copy_is_not_served_through_old_backing() {
        let mut m = InodeIoModes::<u32>::default();
        m.acquire(1, Some((fid(9), ok_reg as Reg)));
        let (g, _) = m.acquire(1, Some((fid(10), ok_reg as Reg)));
        assert_eq!(g.kind(), IoKind::DirectIo);
    }

    #[test]
    fn passthrough_denied_while_cached_opens_exist() {
        let mut m = InodeIoModes::<u32>::default();
        m.acquire(1, None::<(FileId, Reg)>);
        let (g, _) = m.acquire(1, Some((fid(9), ok_reg as Reg)));
        assert_eq!(g.kind(), IoKind::Cached);
    }

    #[test]
    fn mode_resets_after_last_release() {
        let mut m = InodeIoModes::<u32>::default();
        let (a, _) = m.acquire(1, Some((fid(9), ok_reg as Reg)));
        let (b, _) = m.acquire(1, Some((fid(9), ok_reg as Reg)));
        m.release(1, a.kind());
        let (c, _) = m.acquire(1, None::<(FileId, Reg)>);
        assert_eq!(c.kind(), IoKind::DirectIo, "one passthrough open still holds the inode");
        m.release(1, c.kind());
        m.release(1, b.kind());
        let (d, _) = m.acquire(1, None::<(FileId, Reg)>);
        assert_eq!(d.kind(), IoKind::Cached);
    }

    #[test]
    fn failed_registration_falls_back_to_cached_and_reports() {
        let mut m = InodeIoModes::<u32>::default();
        let fail: Reg = || Err(std::io::Error::from_raw_os_error(libc::EPERM));
        let (g, err) = m.acquire(1, Some((fid(9), fail)));
        assert_eq!(g.kind(), IoKind::Cached);
        assert!(err.is_some());
    }

    #[test]
    fn direct_io_release_leaves_mode_untouched() {
        let mut m = InodeIoModes::<u32>::default();
        m.acquire(1, Some((fid(9), ok_reg as Reg)));
        m.release(1, IoKind::DirectIo);
        let (g, _) = m.acquire(1, None::<(FileId, Reg)>);
        assert_eq!(g.kind(), IoKind::DirectIo);
    }
}
