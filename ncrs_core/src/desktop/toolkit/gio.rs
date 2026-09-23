//! GLib / GIO (Nautilus, Nemo, Caja, GTK apps).
//!
//! - MIME sniffing: GLib reads the head of files whose extension is ambiguous,
//!   opening them with `O_NOATIME`. The FUSE layer answers those opens with
//!   synthetic magic bytes derived from the PROPFIND content-type and exposes
//!   `user.xdg.mime.type`, so detection never downloads (see
//!   `mime_magic_bytes` in lib.rs and GOTCHAS.md).
//! - Atomic writes: GIO saves through `.goutputstream-*` / `.xdp-*` temps
//!   renamed into place; any that reach the server are orphans, hidden from
//!   listings and optionally purged.
//! - Thumbnails: the freedesktop `normal` cache, keyed by GLib's file URI, is
//!   pre-filled from server previews; every program registered in a
//!   `.thumbnailer` file is refused on uncached files (see `thumbguard`).

use crate::desktop::detect::DetectEnv;
use crate::desktop::sniff::{SniffAnswer, SniffProbe};
use crate::desktop::thumbguard::{thumbnailer_programs, ThumbnailerMatch};
use crate::desktop::{Component, ComponentId, DesktopPolicy};

pub struct Gio;

/// GLib asks for 16 KiB at offset 0; kernel read-ahead can inflate that first
/// read to 32 KiB, while copy tools use ≥ 64 KiB buffers (GOTCHAS.md §2).
pub const GLIB_SNIFF_MAX_READ: usize = 32768;

/// GLib 2.80+ opens with `O_NOATIME | O_NOFOLLOW` to sniff; the kernel strips
/// `O_NOFOLLOW` before FUSE, so `O_NOATIME` is the signal.
pub const GLIB_SNIFF_PROBE: SniffProbe = SniffProbe {
    toolkit: "gio",
    open_flag: libc::O_NOATIME,
    max_read: GLIB_SNIFF_MAX_READ,
    answer: SniffAnswer::MagicFromContentType,
    xattr: Some("user.xdg.mime.type"),
};

impl Component for Gio {
    fn id(&self) -> ComponentId {
        ComponentId::Gio
    }

    fn contribute(&self, p: &mut DesktopPolicy) {
        p.add_sniff_probe(GLIB_SNIFF_PROBE);
        for prefix in [".goutputstream-", ".xdp-"] {
            if !p.hidden_temp_prefixes.contains(&prefix) {
                p.hidden_temp_prefixes.push(prefix);
            }
        }
        p.thumbnails.normal = true;
        for prog in thumbnailer_programs(&DetectEnv::from_env().data_dirs) {
            p.add_thumbnailer(ThumbnailerMatch::Program(prog));
        }
    }
}
