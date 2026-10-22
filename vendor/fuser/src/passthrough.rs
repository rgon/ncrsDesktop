use std::os::fd::AsFd;
use std::os::fd::BorrowedFd;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::Weak;

use log::error;

use crate::dev_fuse::DevFuse;
use crate::ll::ioctl::fuse_backing_map;
use crate::ll::ioctl::fuse_dev_ioc_backing_close;
use crate::ll::ioctl::fuse_dev_ioc_backing_open;

/// Performs `FUSE_DEV_IOC_BACKING_OPEN` / `FUSE_DEV_IOC_BACKING_CLOSE` on the
/// session's behalf. The kernel requires CAP_SYS_ADMIN for both ioctls; a
/// filesystem that runs without it can hand them to a privileged helper
/// process that holds the same /dev/fuse connection.
///
/// ncrs patch (see PATCHES.md, "fuser: backing broker").
pub trait BackingBroker: Send + Sync {
    /// Registers `fd` as a backing file; returns the kernel's backing id.
    fn open(&self, fd: BorrowedFd<'_>) -> std::io::Result<u32>;
    /// Releases a backing id `open` returned.
    fn close(&self, backing_id: u32) -> std::io::Result<()>;
}

static BROKER: RwLock<Option<Arc<dyn BackingBroker>>> = RwLock::new(None);

/// Routes every later [`BackingId`] creation and drop through `broker`
/// (process-wide); `None` restores the direct ioctls.
///
/// ncrs patch (see PATCHES.md, "fuser: backing broker").
pub fn set_backing_broker(broker: Option<Arc<dyn BackingBroker>>) {
    *BROKER.write().unwrap_or_else(|p| p.into_inner()) = broker;
}

fn backing_broker() -> Option<Arc<dyn BackingBroker>> {
    BROKER.read().unwrap_or_else(|p| p.into_inner()).clone()
}

/// A reference to a previously opened fd intended to be used for passthrough
///
/// You can create these via [`ReplyOpen::open_backing()`](crate::ReplyOpen::open_backing)
/// and send them via [`ReplyOpen::opened_passthrough()`](crate::ReplyOpen::opened_passthrough).
///
/// When working with backing IDs you need to ensure that they live "long enough".  A good practice
/// is to create them in the [`Filesystem::open()`](crate::Filesystem::open) impl,
/// store them in the struct of your Filesystem impl, then drop them in the
/// [`Filesystem::release()`](crate::Filesystem::release) impl. Dropping them immediately after
/// sending them in the `Filesystem::open()` impl can lead to the kernel returning EIO when userspace
/// attempts to access the file.
///
/// This is implemented as a safe wrapper around the `backing_id` field of the `fuse_backing_map`
/// struct used by the ioctls involved in fd passthrough.  It is created by performing a
/// `FUSE_DEV_IOC_BACKING_OPEN` ioctl on an fd and has a Drop trait impl which makes a matching
/// `FUSE_DEV_IOC_BACKING_CLOSE` call.  It holds a weak reference on the fuse channel to allow it to
/// make that call (if the channel hasn't already been closed).
#[derive(Debug)]
pub struct BackingId {
    pub(crate) channel: Weak<DevFuse>,
    /// The `backing_id` field passed to and from the kernel
    pub(crate) backing_id: u32,
}

impl BackingId {
    pub(crate) fn create(channel: &Arc<DevFuse>, fd: impl AsFd) -> std::io::Result<Self> {
        if !cfg!(target_os = "linux") {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "backing IDs are only supported on Linux",
            ));
        }

        // ncrs patch ("fuser: backing broker"): a broker, when set, does the ioctl.
        let id = if let Some(broker) = backing_broker() {
            broker.open(fd.as_fd())?
        } else {
            let map = fuse_backing_map {
                fd: fd.as_fd().as_raw_fd() as u32,
                flags: 0,
                padding: 0,
            };
            unsafe { fuse_dev_ioc_backing_open(channel.as_raw_fd(), &map) }? as u32
        };
        Ok(Self {
            channel: Arc::downgrade(channel),
            backing_id: id,
        })
    }

    /// The kernel's id for this backing file, as returned by
    /// `FUSE_DEV_IOC_BACKING_OPEN`. Backing ids belong to the connection, not
    /// the process, so a daemon handing the connection over records them to
    /// let its successor close them (`FUSE_DEV_IOC_BACKING_CLOSE`) once the
    /// handles using them are released.
    ///
    /// ncrs patch (see PATCHES.md, "fuser: reattach").
    pub fn raw_id(&self) -> u32 {
        self.backing_id
    }
}

impl Drop for BackingId {
    fn drop(&mut self) {
        if let Some(ch) = self.channel.upgrade() {
            // ncrs patch ("fuser: backing broker"): a broker, when set, does the ioctl.
            let r = match backing_broker() {
                Some(broker) => broker.close(self.backing_id),
                None => unsafe { fuse_dev_ioc_backing_close(ch.as_raw_fd(), &self.backing_id) }
                    .map(|_| ())
                    .map_err(std::io::Error::from),
            };
            if let Err(e) = r {
                error!("Failed to close backing fd: {e}");
            }
        }
    }
}
