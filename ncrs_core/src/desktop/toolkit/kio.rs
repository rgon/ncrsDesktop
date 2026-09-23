//! KDE Frameworks KIO (Dolphin, KDE file dialogs).
//!
//! - Thumbnails: KIO's PreviewJob uses the same freedesktop cache and key as
//!   GLib, but asks for `large` (256 px) at bigger zoom levels or on HiDPI. A
//!   miss makes the kio-extras thumbnailer read the whole file, so both sizes
//!   are pre-filled from the server preview, and the thumbnail worker
//!   (`kioworker …/kio/thumbnail.so`, which hosts every ThumbnailCreator
//!   plugin in-process) is refused on uncached files (see `thumbguard`).
//!
//! Not handled yet, pending measurement on a real KDE session (plan Phase 0):
//! Qt's `QMimeDatabase` content sniffing. Once its read signature is known it
//! becomes a `SniffProbe` here scoped with
//! `ProcessMatch::LinksLibrary("libKF6KIOCore.so")` (or `libQt6Core.so`); without
//! a distinctive signature no process condition can tell a probe from a real
//! read. Also pending: per-folder view properties
//! (`user.kde.fm.viewproperties` xattr / `.directory`), and KIO `.part` copies.

use crate::desktop::process::ProcessMatch;
use crate::desktop::{Component, ComponentId, DesktopPolicy};

pub struct Kio;

impl Component for Kio {
    fn id(&self) -> ComponentId {
        ComponentId::Kio
    }

    fn contribute(&self, p: &mut DesktopPolicy) {
        p.thumbnails.normal = true;
        p.thumbnails.large = true;
        // KF6 kioworker and KF5 kioslave5 both name the plugin path.
        p.add_thumbnailer(ProcessMatch::CmdlineContains("/kio/thumbnail.so"));
    }
}
