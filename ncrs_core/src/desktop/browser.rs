//! The "support X" table. Adding a browser = one entry here (picking existing
//! components, or adding a new one) plus its shell adapter package.

use super::detect::DetectEnv;
use super::ComponentId;

/// How to tell whether a browser is installed.
pub struct Detect {
    /// Executable names looked up in the bin dirs (any match counts).
    pub binaries: &'static [&'static str],
    /// `.desktop` file names looked up under `<data dir>/applications`.
    pub desktop_files: &'static [&'static str],
}

impl Detect {
    pub fn is_installed(&self, env: &DetectEnv) -> bool {
        self.binaries.iter().any(|b| env.find_binary(b).is_some())
            || self.desktop_files.iter().any(|d| env.has_desktop_file(d))
    }
}

/// The out-of-process shell adapter (emblems, context menu) for a browser.
pub struct AdapterDescriptor {
    /// Distribution package that ships it, if one exists yet.
    pub package: Option<&'static str>,
    /// `HELLO` client-ids the adapter announces.
    pub client_ids: &'static [&'static str],
    /// Files whose presence means the adapter is installed (see
    /// [`DetectEnv::path_exists`] for the `~/` and `@lib/` prefixes).
    pub installed_paths: &'static [&'static str],
}

impl AdapterDescriptor {
    pub const NONE: AdapterDescriptor = AdapterDescriptor { package: None, client_ids: &[], installed_paths: &[] };

    pub fn is_installed(&self, env: &DetectEnv) -> bool {
        self.installed_paths.iter().any(|p| env.path_exists(p))
    }
}

pub struct BrowserProfile {
    /// Stable id used over IPC and in the stored state.
    pub id: &'static str,
    pub name: &'static str,
    /// One line for the GUI: what enabling this profile does.
    pub summary: &'static str,
    pub detect: Detect,
    pub components: &'static [ComponentId],
    pub adapter: AdapterDescriptor,
}

pub static PROFILES: &[BrowserProfile] = &[
    BrowserProfile {
        id: "nautilus",
        name: "Files (Nautilus)",
        summary: "Sync emblems and actions in Files; hides the mount from Tracker; \
                  answers GLib type detection and fills thumbnails without downloading files.",
        detect: Detect { binaries: &["nautilus"], desktop_files: &["org.gnome.Nautilus.desktop"] },
        components: &[ComponentId::Gio, ComponentId::Tracker],
        adapter: AdapterDescriptor {
            package: Some("ncrs-nautilus"),
            client_ids: &["nautilus"],
            installed_paths: &[
                "/usr/share/nautilus-python/extensions/ncrs-syncstate.py",
                "~/.local/share/nautilus-python/extensions/syncstate.py",
            ],
        },
    },
    BrowserProfile {
        id: "dolphin",
        name: "Dolphin (KDE)",
        summary: "Sync emblems and actions in Dolphin and KDE file dialogs; hides the mount from Baloo; \
                  fills normal and large thumbnails without downloading files.",
        detect: Detect { binaries: &["dolphin", "org.kde.dolphin"], desktop_files: &["org.kde.dolphin.desktop"] },
        components: &[ComponentId::Kio, ComponentId::Baloo],
        adapter: AdapterDescriptor {
            package: Some("ncrs-dolphin"),
            client_ids: &["dolphin-kf6", "dolphin-kf5"],
            installed_paths: &[
                "@lib/qt6/plugins/kf6/overlayicon/ncrsoverlayplugin.so",
                "@lib/qt5/plugins/kf5/overlayicon/ncrsoverlayplugin.so",
            ],
        },
    },
    BrowserProfile {
        id: "nemo",
        name: "Nemo (Cinnamon)",
        summary: "Hides the mount from Tracker; answers GLib type detection and fills thumbnails \
                  without downloading files.",
        detect: Detect { binaries: &["nemo"], desktop_files: &["nemo.desktop"] },
        components: &[ComponentId::Gio, ComponentId::Tracker],
        adapter: AdapterDescriptor::NONE,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_ids_are_unique_and_wire_safe() {
        let mut seen = std::collections::HashSet::new();
        for p in PROFILES {
            assert!(seen.insert(p.id), "duplicate profile id {}", p.id);
            assert!(p.id.chars().all(|c| c.is_ascii_lowercase() || c == '-'), "{}", p.id);
            assert!(!p.components.is_empty(), "{} has no components", p.id);
        }
    }
}
