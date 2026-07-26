use serde::{Deserialize, Serialize};
use tauri::menu::MenuItem;
use tauri::{AppHandle, EventLoopMessage, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_runtime_wry::Wry;

type WryRuntime = Wry<EventLoopMessage>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginMeta {
    pub id: String,
    pub name: String,
    pub description: String,
    pub icon: String,
    pub version: String,
}

pub trait NcrsPlugin: Send + Sync {
    fn meta(&self) -> PluginMeta;

    fn tray_items(&self, app: &AppHandle) -> Result<Vec<MenuItem<WryRuntime>>, tauri::Error> {
        let _ = app;
        Ok(vec![])
    }

    fn handle_tray_event(&self, app: &AppHandle, id: &str) -> bool {
        let _ = (app, id);
        false
    }
}

pub fn open_main_window(app: &AppHandle) {
    // The window is destroyed (not hidden) when closed to the tray, so the
    // WebKitGTK webview stops eating idle CPU. Rebuild it from the same config
    // the manifest declares before showing. Keep these attributes in sync with
    // the `main` window entry in tauri.conf.json.
    let w = match app.get_webview_window("main") {
        Some(w) => w,
        None => match WebviewWindowBuilder::new(app, "main", WebviewUrl::default())
            .title("ncrs-gui")
            .inner_size(1860.0, 1000.0)
            .resizable(false)
            .minimizable(false)
            .maximizable(false)
            .closable(true)
            .fullscreen(false)
            .decorations(false)
            .transparent(true)
            .visible(false)
            .build()
        {
            Ok(w) => w,
            Err(e) => {
                eprintln!("failed to rebuild main window: {e}");
                return;
            }
        },
    };
    fit_overlay_to_monitor(&w);
    let _ = w.show();
    let _ = w.set_focus();
}

/// Maximize the overlay to the compositor's work area (excludes taskbar/dock).
pub fn fit_overlay_to_monitor(w: &tauri::WebviewWindow) {
    // maximize() sizes the window to the compositor's work area (excludes taskbar/dock)
    // and handles scale factor automatically. The maximizable(false) builder flag only
    // suppresses the UI gesture; programmatic maximize always works.
    let _ = w.maximize();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_meta_serializes_roundtrip() {
        let meta = PluginMeta {
            id: "test_plugin".into(),
            name: "Test Plugin".into(),
            description: "A test plugin".into(),
            icon: "mdiPuzzle".into(),
            version: "0.1.0".into(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: PluginMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "test_plugin");
        assert_eq!(back.name, "Test Plugin");
        assert_eq!(back.version, "0.1.0");
    }
}
