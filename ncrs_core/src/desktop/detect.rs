//! "Is this browser installed?" — the source of every profile's default.
//!
//! Deliberately not based on `XDG_CURRENT_DESKTOP`: a GNOME session with
//! Dolphin installed still has Dolphin (and Baloo) reaching the mount.

use std::path::{Path, PathBuf};

/// Where to look. Injectable so tests can fake an installation.
#[derive(Clone, Debug, Default)]
pub struct DetectEnv {
    /// Directories searched for executables.
    pub bin_dirs: Vec<PathBuf>,
    /// XDG data directories (`<dir>/applications/*.desktop`).
    pub data_dirs: Vec<PathBuf>,
    /// Library roots searched for adapter plugins (e.g. `/usr/lib`).
    pub lib_dirs: Vec<PathBuf>,
}

impl DetectEnv {
    /// The real environment. A systemd user service can start with a minimal
    /// `PATH`, so the standard system and Flatpak export locations are always
    /// searched too.
    pub fn from_env() -> Self {
        let home = dirs::home_dir().unwrap_or_default();
        let mut bin_dirs: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default();
        for d in ["/usr/local/bin", "/usr/bin", "/bin", "/var/lib/flatpak/exports/bin", "/snap/bin"] {
            bin_dirs.push(PathBuf::from(d));
        }
        bin_dirs.push(home.join(".local/share/flatpak/exports/bin"));

        let mut data_dirs = vec![std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"))];
        match std::env::var_os("XDG_DATA_DIRS") {
            Some(v) if !v.is_empty() => data_dirs.extend(std::env::split_paths(&v)),
            _ => data_dirs.extend([PathBuf::from("/usr/local/share"), PathBuf::from("/usr/share")]),
        }
        data_dirs.push(PathBuf::from("/var/lib/flatpak/exports/share"));
        data_dirs.push(home.join(".local/share/flatpak/exports/share"));

        let lib_dirs = vec![PathBuf::from("/usr/lib"), PathBuf::from("/usr/local/lib"), PathBuf::from("/usr/lib64")];
        dedup(&mut bin_dirs);
        dedup(&mut data_dirs);
        DetectEnv { bin_dirs, data_dirs, lib_dirs }
    }

    pub fn find_binary(&self, name: &str) -> Option<PathBuf> {
        self.bin_dirs.iter().map(|d| d.join(name)).find(|p| is_executable(p))
    }

    pub fn has_desktop_file(&self, name: &str) -> bool {
        self.data_dirs.iter().any(|d| d.join("applications").join(name).is_file())
    }

    /// Whether `pattern` exists. A leading `~/` is the home directory; a
    /// leading `@lib/` is tried under every [`DetectEnv::lib_dirs`] entry and
    /// one multiarch level below it (`/usr/lib/x86_64-linux-gnu/...`).
    pub fn path_exists(&self, pattern: &str) -> bool {
        if let Some(rest) = pattern.strip_prefix("~/") {
            return dirs::home_dir().is_some_and(|h| h.join(rest).exists());
        }
        if let Some(rest) = pattern.strip_prefix("@lib/") {
            return self.lib_dirs.iter().any(|lib| {
                lib.join(rest).exists()
                    || std::fs::read_dir(lib)
                        .into_iter()
                        .flatten()
                        .flatten()
                        .any(|e| e.path().join(rest).exists())
            });
        }
        Path::new(pattern).exists()
    }
}

fn dedup(v: &mut Vec<PathBuf>) {
    let mut seen = std::collections::HashSet::new();
    v.retain(|p| seen.insert(p.clone()));
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lib_pattern_finds_multiarch_plugins() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = dir.path().join("x86_64-linux-gnu/qt6/plugins/kf6/overlayicon/ncrsoverlay.so");
        std::fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        std::fs::write(&plugin, b"").unwrap();
        let env = DetectEnv { lib_dirs: vec![dir.path().to_path_buf()], ..Default::default() };
        assert!(env.path_exists("@lib/qt6/plugins/kf6/overlayicon/ncrsoverlay.so"));
        assert!(!env.path_exists("@lib/qt5/plugins/kf5/overlayicon/ncrsoverlay.so"));
    }

    #[test]
    fn from_env_always_searches_system_dirs() {
        let env = DetectEnv::from_env();
        assert!(env.bin_dirs.contains(&PathBuf::from("/usr/bin")));
        assert!(env.data_dirs.iter().any(|d| d.ends_with("share")));
    }
}
