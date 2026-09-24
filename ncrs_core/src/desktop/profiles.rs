//! The "support X" table. Two kinds of profile, each with one toggle:
//!
//! - **Toolkit** profiles (GIO, KIO) own what a desktop's libraries and
//!   services do to the mount — MIME sniffing, thumbnail sizes, the indexer.
//!   They are independent of any file browser: GTK apps on a KDE desktop still
//!   sniff through GLib, so the GIO profile stays on wherever GLib is installed.
//! - **Browser** profiles (Nautilus, Dolphin, Nemo) add a shell adapter and
//!   *require* their toolkit profile, which is then kept on while they are.
//!
//! Adding a browser = one entry here (reusing a toolkit, or adding one with its
//! components) plus its shell adapter package.

use super::detect::DetectEnv;
use super::ComponentId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileKind {
    Toolkit,
    Browser,
}

/// How to tell whether a profile's software is installed. Any match counts.
pub struct Detect {
    /// Executable names looked up in the bin dirs.
    pub binaries: &'static [&'static str],
    /// `.desktop` file names looked up under `<data dir>/applications`.
    pub desktop_files: &'static [&'static str],
    /// Shared-library file names looked up under the lib dirs (multiarch too).
    pub libraries: &'static [&'static str],
}

impl Detect {
    pub fn is_installed(&self, env: &DetectEnv) -> bool {
        self.binaries.iter().any(|b| env.find_binary(b).is_some())
            || self.desktop_files.iter().any(|d| env.has_desktop_file(d))
            || self.libraries.iter().any(|l| env.path_exists(&format!("@lib/{}", l)))
    }
}

/// The out-of-process shell adapter (emblems, context menu) for a browser.
pub struct AdapterDescriptor {
    /// `HELLO` client-ids the adapter announces.
    pub client_ids: &'static [&'static str],
    /// Files whose presence means the adapter is installed (see
    /// [`DetectEnv::path_exists`] for the `~/` and `@lib/` prefixes).
    pub installed_paths: &'static [&'static str],
    /// A system package the adapter needs at runtime but the `.deb` only
    /// Suggests, so installing ncrs never drags one desktop's bindings (and
    /// browser upgrades) onto another's users.
    pub runtime: Option<RuntimeNeed>,
}

/// A runtime package, and the files whose presence means it is installed.
pub struct RuntimeNeed {
    pub package: &'static str,
    pub paths: &'static [&'static str],
}

impl AdapterDescriptor {
    pub const NONE: AdapterDescriptor = AdapterDescriptor { client_ids: &[], installed_paths: &[], runtime: None };

    pub fn is_installed(&self, env: &DetectEnv) -> bool {
        self.installed_paths.iter().any(|p| env.path_exists(p))
    }

    /// The runtime package to install before the adapter can load, if missing.
    pub fn missing_package(&self, env: &DetectEnv) -> Option<&'static str> {
        let need = self.runtime.as_ref()?;
        (!need.paths.iter().any(|p| env.path_exists(p))).then_some(need.package)
    }
}

pub struct Profile {
    /// Stable id used over IPC and in the stored state.
    pub id: &'static str,
    pub kind: ProfileKind,
    pub name: &'static str,
    /// One line for the GUI: what enabling this profile does.
    pub summary: &'static str,
    pub detect: Detect,
    pub components: &'static [ComponentId],
    /// Ids of (toolkit) profiles kept enabled while this one is.
    pub requires: &'static [&'static str],
    pub adapter: AdapterDescriptor,
}

pub static PROFILES: &[Profile] = &[
    Profile {
        id: "gio",
        kind: ProfileKind::Toolkit,
        name: "GTK / GNOME apps (GIO)",
        summary: "Answers GLib file-type detection and fills thumbnails without downloading files; \
                  hides the mount from the Tracker indexer.",
        detect: Detect { binaries: &[], desktop_files: &[], libraries: &["libgio-2.0.so.0"] },
        components: &[ComponentId::Gio, ComponentId::Tracker],
        requires: &[],
        adapter: AdapterDescriptor::NONE,
    },
    Profile {
        id: "kio",
        kind: ProfileKind::Toolkit,
        name: "KDE apps (KIO)",
        summary: "Fills normal and large thumbnails without downloading files; \
                  hides the mount from the Baloo indexer.",
        detect: Detect {
            binaries: &[],
            desktop_files: &[],
            libraries: &["libKF6KIOCore.so.6", "libKF5KIOCore.so.5"],
        },
        components: &[ComponentId::Kio, ComponentId::Baloo],
        requires: &[],
        adapter: AdapterDescriptor::NONE,
    },
    Profile {
        id: "nautilus",
        kind: ProfileKind::Browser,
        name: "Files (Nautilus)",
        summary: "Sync emblems, sharing columns and Keep / Free up space / Open in web actions in Files.",
        detect: Detect { binaries: &["nautilus"], desktop_files: &["org.gnome.Nautilus.desktop"], libraries: &[] },
        components: &[],
        requires: &["gio"],
        adapter: AdapterDescriptor {
            client_ids: &["nautilus"],
            installed_paths: &[
                "/usr/share/nautilus-python/extensions/ncrs-syncstate.py",
                "~/.local/share/nautilus-python/extensions/syncstate.py",
            ],
            // nautilus-python's loader; without it Nautilus ignores the extension.
            runtime: Some(RuntimeNeed {
                package: "python3-nautilus",
                paths: &[
                    "@lib/nautilus/extensions-4/libnautilus-python.so",
                    "@lib/nautilus/extensions-3.0/libnautilus-python.so",
                ],
            }),
        },
    },
    Profile {
        id: "dolphin",
        kind: ProfileKind::Browser,
        name: "Dolphin (KDE)",
        summary: "Sync emblems in Dolphin and KDE file dialogs, and Keep / Free up space / Open in web actions.",
        detect: Detect {
            binaries: &["dolphin", "org.kde.dolphin"],
            desktop_files: &["org.kde.dolphin.desktop"],
            libraries: &[],
        },
        components: &[],
        requires: &["kio"],
        adapter: AdapterDescriptor {
            client_ids: &["dolphin-kf6", "dolphin-kf5"],
            installed_paths: &[
                "@lib/qt6/plugins/kf6/overlayicon/ncrsoverlayplugin.so",
                "@lib/qt5/plugins/kf5/overlayicon/ncrsoverlayplugin.so",
            ],
            // Needs only Qt/KF, which Dolphin itself brings.
            runtime: None,
        },
    },
    Profile {
        id: "nemo",
        kind: ProfileKind::Browser,
        name: "Nemo (Cinnamon)",
        summary: "Keeps the GTK / GNOME apps profile on for Nemo (no emblem adapter yet).",
        detect: Detect { binaries: &["nemo"], desktop_files: &["nemo.desktop"], libraries: &[] },
        components: &[],
        requires: &["gio"],
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
            assert!(!p.components.is_empty() || !p.requires.is_empty(), "{} does nothing", p.id);
        }
    }

    #[test]
    fn requirements_point_at_toolkits() {
        // One level only: browsers require toolkits, toolkits require nothing.
        for p in PROFILES {
            for r in p.requires {
                let target = PROFILES.iter().find(|q| q.id == *r).unwrap_or_else(|| panic!("{} requires unknown {}", p.id, r));
                assert_eq!(target.kind, ProfileKind::Toolkit, "{} requires non-toolkit {}", p.id, r);
            }
            if p.kind == ProfileKind::Toolkit {
                assert!(p.requires.is_empty(), "toolkit {} must not require anything", p.id);
            }
        }
    }

    #[test]
    fn nautilus_adapter_asks_for_python3_nautilus_until_its_loader_exists() {
        let dir = tempfile::tempdir().unwrap();
        let env = DetectEnv { lib_dirs: vec![dir.path().to_path_buf()], ..Default::default() };
        let adapter = &PROFILES.iter().find(|p| p.id == "nautilus").unwrap().adapter;
        assert_eq!(adapter.missing_package(&env), Some("python3-nautilus"));
        // Multiarch layout: <lib>/x86_64-linux-gnu/nautilus/extensions-4/…
        let ext = dir.path().join("x86_64-linux-gnu/nautilus/extensions-4");
        std::fs::create_dir_all(&ext).unwrap();
        std::fs::write(ext.join("libnautilus-python.so"), b"").unwrap();
        assert_eq!(adapter.missing_package(&env), None);
        let dolphin = &PROFILES.iter().find(|p| p.id == "dolphin").unwrap().adapter;
        assert_eq!(dolphin.missing_package(&env), None, "Dolphin brings everything its plugin needs");
    }
}
