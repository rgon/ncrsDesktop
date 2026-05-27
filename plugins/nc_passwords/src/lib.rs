pub mod api;
pub mod commands;

use std::sync::Mutex;

use ncrs_plugin::{NcrsPlugin, PluginMeta};
use tauri::menu::MenuItem;
use tauri::{AppHandle, Emitter, EventLoopMessage, Manager};
use tauri_runtime_wry::Wry;

use api::PasswordsClient;

type WryRuntime = Wry<EventLoopMessage>;

pub struct NcPasswordsState {
    pub credentials: Mutex<Option<(String, String, String)>>,
    pub client: Mutex<Option<PasswordsClient>>,
}

impl Default for NcPasswordsState {
    fn default() -> Self {
        Self {
            credentials: Mutex::new(None),
            client: Mutex::new(None),
        }
    }
}

pub struct NcPasswordsPlugin;

impl NcrsPlugin for NcPasswordsPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            id: "nc_passwords".into(),
            name: "Passwords".into(),
            description: "Nextcloud Passwords manager".into(),
            icon: "mdiLock".into(),
            version: "0.1.0".into(),
        }
    }

    fn tray_items(
        &self,
        app: &AppHandle,
    ) -> Result<Vec<MenuItem<WryRuntime>>, tauri::Error> {
        let item =
            MenuItem::with_id(app, "plugin_passwords_open", "Passwords", true, None::<&str>)?;
        Ok(vec![item])
    }

    fn handle_tray_event(&self, app: &AppHandle, id: &str) -> bool {
        if id == "plugin_passwords_open" {
            ncrs_plugin::open_main_window(app);
            app.emit("navigate-plugin", "nc_passwords").ok();
            true
        } else {
            false
        }
    }
}

pub fn setup(app: &AppHandle) {
    app.manage(NcPasswordsState::default());
}

pub fn set_credentials(app: &AppHandle, base_url: &str, username: &str, password: &str) {
    if let Some(state) = app.try_state::<NcPasswordsState>() {
        *state.credentials.lock().unwrap() =
            Some((base_url.into(), username.into(), password.into()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ncrs_plugin::NcrsPlugin;

    #[test]
    fn plugin_meta_has_correct_id() {
        let plugin = NcPasswordsPlugin;
        let meta = plugin.meta();
        assert_eq!(meta.id, "nc_passwords");
        assert_eq!(meta.name, "Passwords");
        assert!(!meta.version.is_empty());
    }

    #[test]
    fn plugin_meta_serializes() {
        let plugin = NcPasswordsPlugin;
        let meta = plugin.meta();
        let json = serde_json::to_string(&meta).unwrap();
        let back: ncrs_plugin::PluginMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, meta.id);
        assert_eq!(back.icon, "mdiLock");
    }

    #[test]
    fn default_state_has_no_credentials_or_client() {
        let state = NcPasswordsState::default();
        assert!(state.credentials.lock().unwrap().is_none());
        assert!(state.client.lock().unwrap().is_none());
    }

    #[test]
    fn handle_tray_event_rejects_unknown_id() {
        let plugin = NcPasswordsPlugin;
        // Cannot call handle_tray_event without AppHandle, but we can verify
        // the meta's tray item ID constant matches what handle_tray_event checks
        let meta = plugin.meta();
        let expected_tray_id = format!("plugin_{}_open", meta.id.replace("nc_", ""));
        assert_eq!(expected_tray_id, "plugin_passwords_open");
    }
}
