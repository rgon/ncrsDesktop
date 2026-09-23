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
//! - Thumbnails: the freedesktop `normal` cache, keyed by GLib's file URI.

use crate::desktop::{Component, ComponentId, DesktopPolicy};

pub struct Gio;

impl Component for Gio {
    fn id(&self) -> ComponentId {
        ComponentId::Gio
    }

    fn contribute(&self, p: &mut DesktopPolicy) {
        p.glib_sniff = true;
        for prefix in [".goutputstream-", ".xdp-"] {
            if !p.hidden_temp_prefixes.contains(&prefix) {
                p.hidden_temp_prefixes.push(prefix);
            }
        }
        p.thumbnails.normal = true;
    }
}
