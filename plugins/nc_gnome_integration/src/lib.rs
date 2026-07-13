pub mod goa;
pub mod secret;

use ncrs_plugin::{NcrsPlugin, PluginMeta};

pub struct NcGnomeIntegrationPlugin;

impl NcrsPlugin for NcGnomeIntegrationPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            id: "nc_gnome_integration".into(),
            name: "GNOME Integration".into(),
            description: "Registers Nextcloud as a GNOME Online Account for Calendar, Contacts, and more".into(),
            icon: "mdiAccountSync".into(),
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
        // Credentials must land in the keyring BEFORE accounts.conf is written.
        // goa-daemon watches accounts.conf via inotify and immediately tries to
        // authenticate the new account; if the keyring entry doesn't exist yet
        // it marks the account AttentionNeeded=true and won't recover on its own.
        if let Err(e) = secret::store_credentials(goa::NCRS_ACCOUNT_ID, &password).await {
            log::error!("nc_gnome_integration: failed to store credentials: {e:#}");
            return;
        }
        if let Err(e) = goa::ensure_account(&base_url, &username) {
            log::error!("nc_gnome_integration: failed to ensure GOA account: {e:#}");
            return;
        }
        // For accounts that already existed and were in AttentionNeeded state,
        // signal goa-daemon to re-verify now that the keyring entry is fresh.
        goa::trigger_credential_recheck(goa::NCRS_ACCOUNT_ID);
    });
}

pub fn clear_credentials(base_url: &str) {
    let base_url = base_url.to_string();
    tokio::spawn(async move {
        match goa::remove_managed_account(&base_url) {
            Ok(Some(id)) => {
                if let Err(e) = secret::delete_credentials(&id).await {
                    log::error!("nc_gnome_integration: failed to delete credentials: {e:#}");
                }
            }
            Ok(None) => {}
            Err(e) => {
                log::error!("nc_gnome_integration: failed to remove GOA account: {e:#}");
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
        let plugin = NcGnomeIntegrationPlugin;
        let meta = plugin.meta();
        assert_eq!(meta.id, "nc_gnome_integration");
        assert_eq!(meta.name, "GNOME Integration");
    }

    #[test]
    fn plugin_meta_serializes() {
        let plugin = NcGnomeIntegrationPlugin;
        let meta = plugin.meta();
        let json = serde_json::to_string(&meta).unwrap();
        let back: ncrs_plugin::PluginMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, meta.id);
        assert_eq!(back.icon, "mdiAccountSync");
    }
}
