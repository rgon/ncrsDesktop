use serde::{Deserialize, Serialize};
use tauri::menu::MenuItem;
use tauri::{AppHandle, EventLoopMessage, Manager};
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
