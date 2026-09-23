//! GNOME Tracker (tracker-miner-fs / localsearch).
//!
//! Tracker skips any directory containing a `.trackerignore`; the FUSE layer
//! overlays a synthetic one onto the mount root (see `trackerignore_entry` in
//! lib.rs). Purely in-process — nothing to apply or undo outside the mount.

use crate::desktop::{Component, ComponentId, DesktopPolicy};

pub struct Tracker;

impl Component for Tracker {
    fn id(&self) -> ComponentId {
        ComponentId::Tracker
    }

    fn contribute(&self, p: &mut DesktopPolicy) {
        p.tracker_ignore = true;
    }
}
