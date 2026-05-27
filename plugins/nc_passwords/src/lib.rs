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
            if let Some(w) = app.get_webview_window("main") {
                let monitor = w.primary_monitor().unwrap();
                if let Some(m) = monitor {
                    let _ = w.set_size(*m.size());
                } else {
                    let _ = w.set_size(tauri::PhysicalSize::new(1860u32, 1000u32));
                }
                let _ = w.show();
                let _ = w.set_focus();
            }
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
