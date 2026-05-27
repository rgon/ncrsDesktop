use ncrs_plugin::{NcrsPlugin, PluginMeta};
use tauri::menu::MenuItem;
use tauri::{AppHandle, EventLoopMessage};
use tauri_runtime_wry::Wry;

type WryRuntime = Wry<EventLoopMessage>;

pub fn all_plugins() -> Vec<Box<dyn NcrsPlugin>> {
    vec![Box::new(nc_passwords::NcPasswordsPlugin)]
}

pub fn all_metas() -> Vec<PluginMeta> {
    all_plugins().iter().map(|p| p.meta()).collect()
}

pub fn all_tray_items(app: &AppHandle) -> Result<Vec<MenuItem<WryRuntime>>, tauri::Error> {
    let mut items = Vec::new();
    for plugin in all_plugins() {
        items.extend(plugin.tray_items(app)?);
    }
    Ok(items)
}

pub fn handle_plugin_tray_event(app: &AppHandle, id: &str) -> bool {
    for plugin in all_plugins() {
        if plugin.handle_tray_event(app, id) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_registered_plugins() {
        let metas = all_metas();
        assert!(!metas.is_empty());
        assert!(metas.iter().any(|m| m.id == "nc_passwords"));
    }

    #[test]
    fn plugin_ids_are_unique() {
        let metas = all_metas();
        let ids: Vec<&str> = metas.iter().map(|m| m.id.as_str()).collect();
        let unique: std::collections::HashSet<&str> = ids.iter().copied().collect();
        assert_eq!(ids.len(), unique.len(), "duplicate plugin IDs");
    }
}
