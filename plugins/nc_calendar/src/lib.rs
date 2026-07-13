pub mod goa;
pub mod secret;

use ncrs_plugin::{NcrsPlugin, PluginMeta};

pub struct NcCalendarPlugin;

impl NcrsPlugin for NcCalendarPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            id: "nc_calendar".into(),
            name: "Calendar".into(),
            description: "Syncs Nextcloud calendar with GNOME Online Accounts".into(),
            icon: "mdiCalendar".into(),
            version: "0.1.0".into(),
        }
    }
}

pub fn setup(_app: &tauri::AppHandle) {}

pub fn set_credentials(base_url: &str, username: &str, password: &str) {
    let base_url = base_url.to_string();
    let username = username.to_string();
    let password = password.to_string();
    tokio::spawn(async move {
        match goa::ensure_account(&base_url, &username) {
            Ok(entry) => {
                if let Err(e) = secret::store_credentials(&entry.id, &password).await {
                    log::error!("nc_calendar: failed to store credentials: {e:#}");
                }
            }
            Err(e) => {
                log::error!("nc_calendar: failed to ensure GOA account: {e:#}");
            }
        }
    });
}

pub fn clear_credentials(base_url: &str) {
    let base_url = base_url.to_string();
    tokio::spawn(async move {
        match goa::remove_managed_account(&base_url) {
            Ok(Some(id)) => {
                if let Err(e) = secret::delete_credentials(&id).await {
                    log::error!("nc_calendar: failed to delete credentials: {e:#}");
                }
            }
            Ok(None) => {}
            Err(e) => {
                log::error!("nc_calendar: failed to remove GOA account: {e:#}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ncrs_plugin::NcrsPlugin;

    #[test]
    fn plugin_meta_has_correct_id() {
        let plugin = NcCalendarPlugin;
        let meta = plugin.meta();
        assert_eq!(meta.id, "nc_calendar");
        assert_eq!(meta.name, "Calendar");
    }

    #[test]
    fn plugin_meta_serializes() {
        let plugin = NcCalendarPlugin;
        let meta = plugin.meta();
        let json = serde_json::to_string(&meta).unwrap();
        let back: ncrs_plugin::PluginMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, meta.id);
        assert_eq!(back.icon, "mdiCalendar");
    }
}
