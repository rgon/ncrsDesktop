//! KDE Frameworks KIO (Dolphin, KDE file dialogs).
//!
//! - Thumbnails: KIO's PreviewJob uses the same freedesktop cache and key as
//!   GLib, but asks for `large` (256 px) at bigger zoom levels or on HiDPI. A
//!   miss makes the kio-extras thumbnailer read the whole file, so both sizes
//!   are pre-filled from the server preview.
//!
//! Not handled yet, pending measurement on a real KDE session (plan Phase 0):
//! Qt's `QMimeDatabase` content sniffing (it does not use `O_NOATIME`, so the
//! GLib intercept never fires), per-folder view properties
//! (`user.kde.fm.viewproperties` xattr / `.directory`), and KIO `.part` copies.

use crate::desktop::{Component, ComponentId, DesktopPolicy};

pub struct Kio;

impl Component for Kio {
    fn id(&self) -> ComponentId {
        ComponentId::Kio
    }

    fn contribute(&self, p: &mut DesktopPolicy) {
        p.thumbnails.normal = true;
        p.thumbnails.large = true;
    }
}
