//! File-type probes: how a toolkit reads the head of a file to work out its
//! MIME type when the extension is not enough.
//!
//! On the mount such a probe would download the file (or at least its first
//! chunk, per file, for a whole directory listing). A toolkit that sniffs
//! declares its probe here; the FUSE layer recognises the probe's open and
//! answers its first read with synthetic magic bytes derived from the
//! content-type the server already reported in PROPFIND, so the toolkit reaches
//! the same verdict as on the real bytes without any network I/O (see
//! `mime_magic_bytes` in lib.rs and GOTCHAS.md §2).
//!
//! This is separate from thumbnailing (`thumbguard`): the probe comes from the
//! file manager or any app itself, before a thumbnailer is ever chosen, and it
//! must be *answered*, not refused.

/// How the probe's answer is produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SniffAnswer {
    /// Magic bytes chosen from the server-reported content-type.
    MagicFromContentType,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SniffProbe {
    /// Toolkit declaring the probe (for logs).
    pub toolkit: &'static str,
    /// `open(2)` flag that marks the probe, on a read-only open of a file that
    /// is not cached locally (cached files are read for free).
    pub open_flag: i32,
    /// Largest read at offset 0 answered synthetically. A larger read on the
    /// same handle is a real reader (e.g. a copy tool that happens to share
    /// the flag) and gets real bytes.
    pub max_read: usize,
    pub answer: SniffAnswer,
    /// Extended attribute the toolkit checks before sniffing; served with the
    /// content-type so most files never reach the probe at all.
    pub xattr: Option<&'static str>,
}

impl SniffProbe {
    pub fn matches_open(&self, flags: i32) -> bool {
        flags & self.open_flag != 0
    }
}

#[cfg(test)]
mod tests {
    use crate::desktop::toolkit::gio::GLIB_SNIFF_PROBE;

    #[test]
    fn glib_probe_is_recognised_by_o_noatime_only() {
        assert!(GLIB_SNIFF_PROBE.matches_open(libc::O_RDONLY | libc::O_NOATIME));
        assert!(!GLIB_SNIFF_PROBE.matches_open(libc::O_RDONLY));
        // GLib asks for 16 KiB; read-ahead may inflate it, copies use ≥ 64 KiB.
        assert!(GLIB_SNIFF_PROBE.max_read >= 16384 && GLIB_SNIFF_PROBE.max_read < 65536);
    }
}
