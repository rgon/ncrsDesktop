/// Slot shared between the FUSE filesystem struct and all background threads
/// that need to send kernel notifications.  `None` until the FUSE session is
/// established; writes are gated by the Mutex.
pub type NotifierSlot = std::sync::Arc<std::sync::Mutex<Option<fuser::Notifier>>>;
