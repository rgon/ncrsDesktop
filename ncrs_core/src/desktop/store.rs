//! Persisted profile state: the user's explicit choices, and the out-of-process
//! side effects the service applied (so it only ever undoes its own).
//!
//! Kept in its own service-owned file (`~/.config/ncrs/desktop-profiles.json`)
//! rather than config.yaml, which the GUI rewrites wholesale.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Enabled iff the browser is installed.
    #[default]
    Auto,
    On,
    Off,
}

impl std::str::FromStr for Mode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "auto" => Ok(Mode::Auto),
            "on" => Ok(Mode::On),
            "off" => Ok(Mode::Off),
            other => Err(format!("mode must be on, off or auto (got {:?})", other)),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Store {
    /// Explicit per-profile choices; absent means `auto`.
    #[serde(default)]
    pub modes: BTreeMap<String, Mode>,
    /// Side effects applied by components, keyed by an id the component owns
    /// (e.g. `"baloo.exclude"` → the folder it added).
    #[serde(default)]
    pub applied: BTreeMap<String, String>,
}

pub fn default_path() -> PathBuf {
    crate::config::config_path().with_file_name("desktop-profiles.json")
}

impl Store {
    pub fn load(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                log::warn!("ignoring unreadable {}: {}", path.display(), e);
                Store::default()
            }),
            Err(_) => Store::default(),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        crate::config::write_private(path, &json)
    }

    pub fn mode(&self, profile: &str) -> Mode {
        self.modes.get(profile).copied().unwrap_or_default()
    }

    pub fn set_mode(&mut self, profile: &str, mode: Mode) {
        if mode == Mode::Auto {
            self.modes.remove(profile);
        } else {
            self.modes.insert(profile.to_string(), mode);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_omits_auto() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop-profiles.json");
        let mut s = Store::default();
        s.set_mode("dolphin", Mode::Off);
        s.set_mode("nautilus", Mode::Auto);
        s.applied.insert("baloo.exclude".into(), "/home/u/NC".into());
        s.save(&path).unwrap();
        let back = Store::load(&path);
        assert_eq!(back, s);
        assert!(!back.modes.contains_key("nautilus"));
        assert_eq!(back.mode("nautilus"), Mode::Auto);
    }

    #[test]
    fn corrupt_file_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop-profiles.json");
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(Store::load(&path), Store::default());
    }
}
