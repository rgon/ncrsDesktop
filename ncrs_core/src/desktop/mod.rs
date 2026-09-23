//! Desktop / file-browser integration profiles.
//!
//! Every file browser brings its own expectations about a mount: an indexer
//! that crawls it (Tracker, Baloo), a MIME sniffer that reads file heads
//! (GLib), a thumbnail cache layout, temp files it writes next to documents.
//! On a remote mount each of those can turn into a download storm, so ncrs
//! answers them locally, and *how* it does so depends on the browser.
//!
//! Rather than scattering per-browser special cases through the FUSE layer,
//! "support X" is expressed as a [`browser::BrowserProfile`]: a named bundle of
//! reusable [`Component`]s (a toolkit such as GIO or KIO, an indexer such as
//! Tracker or Baloo) plus a descriptor of its shell adapter. The [`Manager`]
//! resolves which profiles are enabled (by default: the browsers that are
//! installed), activates the union of their components (ref-counted — a
//! component stays on while any enabled profile needs it), and publishes one
//! immutable [`DesktopPolicy`] that the FUSE hot paths read lock-free-ish via
//! [`policy()`].
//!
//! The service owns all of this. The GUI and `ncrs-ctl` only list and toggle
//! whole profiles over IPC (`INTEGRATIONS`, `INTEGRATION_SET`); components are
//! never exposed individually.

pub mod browser;
pub mod detect;
pub mod indexer;
pub mod store;
pub mod toolkit;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use crate::{MutexExt, RwLockExt};
use browser::BrowserProfile;
use detect::DetectEnv;
use store::{Mode, Store};

/// Reusable building blocks shared between browser profiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ComponentId {
    /// GLib/GIO: magic-byte MIME sniffing, GIO atomic-write temps, freedesktop thumbnails.
    Gio,
    /// KDE Frameworks KIO: thumbnail sizes used by Dolphin.
    Kio,
    /// GNOME Tracker indexer.
    Tracker,
    /// KDE Baloo indexer.
    Baloo,
}

/// Which freedesktop thumbnail sizes the service pre-fills from server previews.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ThumbnailSizes {
    /// `~/.cache/thumbnails/normal` (128 px).
    pub normal: bool,
    /// `~/.cache/thumbnails/large` (256 px), used by KIO at larger zoom or HiDPI.
    pub large: bool,
}

/// The merged, immutable view of every active component, read by the FUSE layer.
#[derive(Clone, Debug, PartialEq)]
pub struct DesktopPolicy {
    /// Overlay a synthetic `/.trackerignore` onto the mount root (Tracker).
    pub tracker_ignore: bool,
    /// Answer GLib's `O_NOATIME` magic-byte probes from cached metadata and
    /// expose `user.xdg.mime.type`, so MIME detection never downloads a file.
    pub glib_sniff: bool,
    /// Name prefixes of toolkit atomic-write temps: hidden from listings and,
    /// if configured, purged from the server when found orphaned.
    pub hidden_temp_prefixes: Vec<&'static str>,
    pub thumbnails: ThumbnailSizes,
    /// Components this policy was built from (diagnostics).
    pub components: BTreeSet<ComponentId>,
}

impl DesktopPolicy {
    /// Nothing active.
    pub fn empty() -> Self {
        DesktopPolicy {
            tracker_ignore: false,
            glib_sniff: false,
            hidden_temp_prefixes: Vec::new(),
            thumbnails: ThumbnailSizes::default(),
            components: BTreeSet::new(),
        }
    }

    /// Built from a set of components.
    pub fn from_components<I: IntoIterator<Item = ComponentId>>(ids: I) -> Self {
        let mut p = DesktopPolicy::empty();
        for id in ids {
            component(id).contribute(&mut p);
            p.components.insert(id);
        }
        p
    }

    /// What ncrs did before profiles existed (GIO + Tracker). Used until the
    /// manager has resolved the real set, and by code paths/tests that never
    /// start one, so behaviour is unchanged unless profiles say otherwise.
    pub fn legacy_default() -> Self {
        DesktopPolicy::from_components([ComponentId::Gio, ComponentId::Tracker])
    }

    pub fn is_hidden_temp(&self, name: &str) -> bool {
        self.hidden_temp_prefixes.iter().any(|p| name.starts_with(p))
    }
}

/// Context handed to components when they apply side effects.
pub struct ComponentCtx<'a> {
    pub mount_point: &'a Path,
    pub env: &'a DetectEnv,
    pub store: &'a mut Store,
}

/// One reusable integration building block.
pub trait Component: Send + Sync {
    fn id(&self) -> ComponentId;
    /// Fold this component's in-process behaviour into the policy.
    fn contribute(&self, policy: &mut DesktopPolicy);
    /// Apply out-of-process side effects when the component becomes active
    /// (e.g. register an indexer exclusion). Must be idempotent.
    fn activate(&self, _ctx: &mut ComponentCtx) -> Result<(), String> {
        Ok(())
    }
    /// Undo exactly what `activate` did (and nothing the user set themselves).
    fn deactivate(&self, _ctx: &mut ComponentCtx) -> Result<(), String> {
        Ok(())
    }
}

pub const ALL_COMPONENTS: &[ComponentId] = &[ComponentId::Gio, ComponentId::Kio, ComponentId::Tracker, ComponentId::Baloo];

pub fn component(id: ComponentId) -> &'static dyn Component {
    match id {
        ComponentId::Gio => &toolkit::gio::Gio,
        ComponentId::Kio => &toolkit::kio::Kio,
        ComponentId::Tracker => &indexer::tracker::Tracker,
        ComponentId::Baloo => &indexer::baloo::Baloo,
    }
}

// ── Published policy ─────────────────────────────────────────────────────────

fn policy_slot() -> &'static RwLock<Arc<DesktopPolicy>> {
    static SLOT: OnceLock<RwLock<Arc<DesktopPolicy>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(Arc::new(DesktopPolicy::legacy_default())))
}

/// The current policy. Cheap (a read lock and an `Arc` clone); take it once
/// per operation rather than per directory entry.
pub fn policy() -> Arc<DesktopPolicy> {
    policy_slot().safe_read().clone()
}

fn publish(p: DesktopPolicy) {
    let mut slot = policy_slot().safe_write();
    if **slot != p {
        log::info!("desktop policy: components {:?}", p.components);
        *slot = Arc::new(p);
    }
}

// ── Manager ──────────────────────────────────────────────────────────────────

/// One profile as reported over IPC (`INTEGRATIONS`).
#[derive(Clone, Debug, serde::Serialize)]
pub struct ProfileStatus {
    pub id: &'static str,
    pub name: &'static str,
    pub summary: &'static str,
    pub installed: bool,
    pub mode: Mode,
    pub enabled: bool,
    pub adapter_package: Option<&'static str>,
    pub adapter_client_ids: &'static [&'static str],
    pub adapter_installed: bool,
    pub adapter_connected: bool,
}

struct State {
    store: Store,
    /// Components whose side effects are currently applied.
    active: BTreeSet<ComponentId>,
    /// Last detection result per profile id.
    installed: HashMap<&'static str, bool>,
    /// No reconcile has run yet in this process.
    first: bool,
}

pub struct Manager {
    mount_point: PathBuf,
    env: DetectEnv,
    store_path: Option<PathBuf>,
    profiles: &'static [BrowserProfile],
    state: Mutex<State>,
    /// Publish into the process-wide policy slot (off in unit tests, which
    /// inspect [`Manager::current_policy`] instead).
    publish_global: bool,
}

impl Manager {
    /// The service's manager: real environment, state persisted next to config.yaml.
    pub fn for_service(mount_point: PathBuf) -> Self {
        Self::new(mount_point, DetectEnv::from_env(), Some(store::default_path()), browser::PROFILES, true)
    }

    pub fn new(
        mount_point: PathBuf,
        env: DetectEnv,
        store_path: Option<PathBuf>,
        profiles: &'static [BrowserProfile],
        publish_global: bool,
    ) -> Self {
        let store = store_path.as_deref().map(Store::load).unwrap_or_default();
        Manager {
            mount_point,
            env,
            store_path,
            profiles,
            state: Mutex::new(State { store, active: BTreeSet::new(), installed: HashMap::new(), first: true }),
            publish_global,
        }
    }

    /// Detect, resolve and apply. Called at service start and on every listing,
    /// so installing or removing a browser takes effect without a restart.
    pub fn refresh(&self) {
        let mut st = self.state.safe_lock();
        self.detect_locked(&mut st);
        self.reconcile_locked(&mut st);
    }

    fn detect_locked(&self, st: &mut State) {
        for p in self.profiles {
            let installed = p.detect.is_installed(&self.env);
            if st.installed.insert(p.id, installed) != Some(installed) {
                log::info!("desktop profile {}: {}", p.id, if installed { "installed" } else { "not installed" });
            }
        }
    }

    fn enabled_locked(&self, st: &State, p: &BrowserProfile) -> bool {
        match st.store.mode(p.id) {
            Mode::On => true,
            Mode::Off => false,
            Mode::Auto => st.installed.get(p.id).copied().unwrap_or(false),
        }
    }

    fn wanted_components(&self, st: &State) -> BTreeSet<ComponentId> {
        self.profiles
            .iter()
            .filter(|p| self.enabled_locked(st, p))
            .flat_map(|p| p.components.iter().copied())
            .collect()
    }

    fn reconcile_locked(&self, st: &mut State) {
        let wanted = self.wanted_components(st);
        // In-process behaviour first: it is instant, while side effects may
        // shell out (balooctl) for a few seconds.
        if self.publish_global {
            publish(DesktopPolicy::from_components(wanted.iter().copied()));
        }
        let to_start: Vec<ComponentId> = wanted.difference(&st.active).copied().collect();
        // On the first pass also stop every unwanted component, so a side
        // effect recorded by a previous run (e.g. a Baloo exclusion for a
        // profile that has since been disabled) is undone. Deactivation only
        // touches what the store says we applied, so this is a no-op otherwise.
        let to_stop: Vec<ComponentId> = if std::mem::take(&mut st.first) {
            ALL_COMPONENTS.iter().copied().filter(|c| !wanted.contains(c)).collect()
        } else {
            st.active.difference(&wanted).copied().collect()
        };
        let mut store_changed = false;
        for id in to_stop {
            let mut ctx = ComponentCtx { mount_point: &self.mount_point, env: &self.env, store: &mut st.store };
            match component(id).deactivate(&mut ctx) {
                Ok(()) => {
                    st.active.remove(&id);
                    store_changed = true;
                }
                Err(e) => log::warn!("desktop component {:?}: deactivate failed: {}", id, e),
            }
        }
        for id in to_start {
            let mut ctx = ComponentCtx { mount_point: &self.mount_point, env: &self.env, store: &mut st.store };
            if let Err(e) = component(id).activate(&mut ctx) {
                // Still counts as active: its in-process behaviour applies, and
                // the next refresh retries the side effect (activate is idempotent).
                log::warn!("desktop component {:?}: activate failed: {}", id, e);
            }
            st.active.insert(id);
            store_changed = true;
        }
        if store_changed {
            self.save_locked(st);
        }
    }

    fn save_locked(&self, st: &State) {
        if let Some(path) = &self.store_path {
            if let Err(e) = st.store.save(path) {
                log::warn!("cannot save desktop profile state to {}: {}", path.display(), e);
            }
        }
    }

    /// Re-detect and report every profile. `is_connected` answers whether an
    /// adapter with a given client-id is connected right now.
    pub fn list(&self, is_connected: &dyn Fn(&str) -> bool) -> Vec<ProfileStatus> {
        let mut st = self.state.safe_lock();
        self.detect_locked(&mut st);
        self.reconcile_locked(&mut st);
        self.profiles
            .iter()
            .map(|p| ProfileStatus {
                id: p.id,
                name: p.name,
                summary: p.summary,
                installed: st.installed.get(p.id).copied().unwrap_or(false),
                mode: st.store.mode(p.id),
                enabled: self.enabled_locked(&st, p),
                adapter_package: p.adapter.package,
                adapter_client_ids: p.adapter.client_ids,
                adapter_installed: p.adapter.is_installed(&self.env),
                adapter_connected: p.adapter.client_ids.iter().any(|id| is_connected(id)),
            })
            .collect()
    }

    /// Store a user choice for one profile and apply it.
    pub fn set(&self, id: &str, mode: Mode) -> Result<(), String> {
        let Some(profile) = self.profiles.iter().find(|p| p.id == id) else {
            return Err(format!("unknown profile {:?}", id));
        };
        let mut st = self.state.safe_lock();
        st.store.set_mode(profile.id, mode);
        self.save_locked(&st);
        log::info!("desktop profile {} set to {:?}", profile.id, mode);
        self.detect_locked(&mut st);
        self.reconcile_locked(&mut st);
        Ok(())
    }

    /// Resolve now, then keep re-detecting in the background so installing or
    /// removing a browser takes effect without a restart even when no client
    /// ever asks for `INTEGRATIONS`.
    pub fn spawn_refresher(self: &Arc<Self>) {
        let m = self.clone();
        std::thread::spawn(move || loop {
            m.refresh();
            std::thread::sleep(std::time::Duration::from_secs(600));
        });
    }

    /// The policy the current profile set resolves to.
    pub fn current_policy(&self) -> DesktopPolicy {
        let st = self.state.safe_lock();
        DesktopPolicy::from_components(self.wanted_components(&st))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use browser::{AdapterDescriptor, Detect};

    static TEST_PROFILES: &[BrowserProfile] = &[
        BrowserProfile {
            id: "nautilus",
            name: "Files",
            summary: "",
            detect: Detect { binaries: &["nautilus"], desktop_files: &[] },
            components: &[ComponentId::Gio, ComponentId::Tracker],
            adapter: AdapterDescriptor::NONE,
        },
        BrowserProfile {
            id: "nemo",
            name: "Nemo",
            summary: "",
            detect: Detect { binaries: &["nemo"], desktop_files: &[] },
            components: &[ComponentId::Gio, ComponentId::Tracker],
            adapter: AdapterDescriptor::NONE,
        },
        BrowserProfile {
            id: "dolphin",
            name: "Dolphin",
            summary: "",
            detect: Detect { binaries: &[], desktop_files: &["org.kde.dolphin.desktop"] },
            components: &[ComponentId::Kio],
            adapter: AdapterDescriptor::NONE,
        },
    ];

    struct Fixture {
        _dir: tempfile::TempDir,
        bin: PathBuf,
        apps: PathBuf,
        env: DetectEnv,
        store: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        let share = dir.path().join("share");
        let apps = share.join("applications");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&apps).unwrap();
        let env = DetectEnv { bin_dirs: vec![bin.clone()], data_dirs: vec![share], lib_dirs: vec![] };
        let store = dir.path().join("desktop-profiles.json");
        Fixture { _dir: dir, bin, apps, env, store }
    }

    fn install_bin(f: &Fixture, name: &str) {
        use std::os::unix::fs::PermissionsExt;
        let p = f.bin.join(name);
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn manager(f: &Fixture) -> Manager {
        Manager::new(PathBuf::from("/mnt/nc"), f.env.clone(), Some(f.store.clone()), TEST_PROFILES, false)
    }

    fn enabled(m: &Manager) -> Vec<&'static str> {
        m.list(&|_| false).into_iter().filter(|p| p.enabled).map(|p| p.id).collect()
    }

    #[test]
    fn auto_follows_installation() {
        let f = fixture();
        install_bin(&f, "nautilus");
        let m = manager(&f);
        assert_eq!(enabled(&m), vec!["nautilus"]);
        // Installing Dolphin later flips its default without a restart.
        std::fs::write(f.apps.join("org.kde.dolphin.desktop"), "[Desktop Entry]\n").unwrap();
        assert_eq!(enabled(&m), vec!["nautilus", "dolphin"]);
        std::fs::remove_file(f.bin.join("nautilus")).unwrap();
        assert_eq!(enabled(&m), vec!["dolphin"]);
    }

    #[test]
    fn non_executable_file_is_not_an_installed_binary() {
        let f = fixture();
        std::fs::write(f.bin.join("nautilus"), "").unwrap();
        assert!(enabled(&manager(&f)).is_empty());
    }

    #[test]
    fn explicit_choice_overrides_detection_and_persists() {
        let f = fixture();
        install_bin(&f, "nautilus");
        let m = manager(&f);
        m.set("nautilus", Mode::Off).unwrap();
        m.set("dolphin", Mode::On).unwrap();
        assert_eq!(enabled(&m), vec!["dolphin"]);
        // A fresh manager (service restart) reads the stored choices back.
        let m2 = manager(&f);
        assert_eq!(enabled(&m2), vec!["dolphin"]);
        // Reset to auto: back to detection.
        m2.set("nautilus", Mode::Auto).unwrap();
        m2.set("dolphin", Mode::Auto).unwrap();
        assert_eq!(enabled(&m2), vec!["nautilus"]);
    }

    #[test]
    fn shared_components_are_ref_counted() {
        let f = fixture();
        install_bin(&f, "nautilus");
        install_bin(&f, "nemo");
        let m = manager(&f);
        m.refresh();
        m.set("nautilus", Mode::Off).unwrap();
        // Nemo still needs GIO + Tracker.
        let p = m.current_policy();
        assert!(p.glib_sniff && p.tracker_ignore);
        m.set("nemo", Mode::Off).unwrap();
        let p = m.current_policy();
        assert!(!p.glib_sniff && !p.tracker_ignore);
    }

    #[test]
    fn unknown_profile_is_rejected() {
        let f = fixture();
        assert!(manager(&f).set("finder", Mode::On).is_err());
    }

    #[test]
    fn legacy_default_matches_pre_profile_behaviour() {
        let p = DesktopPolicy::legacy_default();
        assert!(p.tracker_ignore && p.glib_sniff);
        assert!(p.is_hidden_temp(".goutputstream-ABC123"));
        assert!(p.is_hidden_temp(".xdp-foo"));
        assert!(!p.is_hidden_temp("report.odt"));
        assert!(p.thumbnails.normal && !p.thumbnails.large);
    }

    #[test]
    fn kio_adds_large_thumbnails() {
        let p = DesktopPolicy::from_components([ComponentId::Kio]);
        assert!(p.thumbnails.normal && p.thumbnails.large);
        assert!(!p.glib_sniff);
    }
}
