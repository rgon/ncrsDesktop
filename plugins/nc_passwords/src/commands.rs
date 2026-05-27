use tauri::State;

use crate::api::{FolderEntry, PasswordEntry, TagEntry};
use crate::NcPasswordsState;

fn with_client<F, T>(state: &State<NcPasswordsState>, f: F) -> Result<T, String>
where
    F: FnOnce(&crate::api::PasswordsClient) -> Result<T, String>,
{
    let guard = state.client.lock().map_err(|e| e.to_string())?;
    let client = guard.as_ref().ok_or("passwords: not connected")?;
    f(client)
}

#[tauri::command]
pub fn nc_passwords_connect(state: State<NcPasswordsState>) -> Result<(), String> {
    let creds = state
        .credentials
        .lock()
        .map_err(|e| e.to_string())?
        .clone()
        .ok_or("passwords: no credentials available")?;

    let (base_url, user, pass) = creds;
    let mut client = crate::api::PasswordsClient::new(&base_url, &user, &pass);
    client.open_session()?;

    *state.client.lock().map_err(|e| e.to_string())? = Some(client);
    Ok(())
}

#[tauri::command]
pub fn nc_passwords_disconnect(state: State<NcPasswordsState>) -> Result<(), String> {
    let mut guard = state.client.lock().map_err(|e| e.to_string())?;
    if let Some(client) = guard.as_ref() {
        let _ = client.close_session();
    }
    *guard = None;
    Ok(())
}

#[tauri::command]
pub fn nc_passwords_is_connected(state: State<NcPasswordsState>) -> bool {
    state.client.lock().map(|g| g.is_some()).unwrap_or(false)
}

#[tauri::command]
pub fn nc_passwords_list(state: State<NcPasswordsState>) -> Result<Vec<PasswordEntry>, String> {
    with_client(&state, |c| c.list_passwords())
}

#[tauri::command]
pub fn nc_passwords_show(
    state: State<NcPasswordsState>,
    id: String,
) -> Result<PasswordEntry, String> {
    with_client(&state, |c| c.show_password(&id))
}

#[tauri::command]
pub fn nc_passwords_search(
    state: State<NcPasswordsState>,
    term: String,
) -> Result<Vec<PasswordEntry>, String> {
    with_client(&state, |c| {
        let all = c.list_passwords()?;
        let term_lower = term.to_lowercase();
        Ok(all
            .into_iter()
            .filter(|p| {
                !p.trashed
                    && (p.label.to_lowercase().contains(&term_lower)
                        || p.username.to_lowercase().contains(&term_lower)
                        || p.url.to_lowercase().contains(&term_lower)
                        || p.notes.to_lowercase().contains(&term_lower))
            })
            .collect())
    })
}

#[tauri::command]
pub fn nc_passwords_create(
    state: State<NcPasswordsState>,
    label: String,
    username: String,
    password: String,
    url: String,
    folder: Option<String>,
) -> Result<PasswordEntry, String> {
    with_client(&state, |c| {
        c.create_password(&label, &username, &password, &url, folder.as_deref())
    })
}

#[tauri::command]
pub fn nc_passwords_delete(state: State<NcPasswordsState>, id: String) -> Result<(), String> {
    with_client(&state, |c| c.delete_password(&id))
}

#[tauri::command]
pub fn nc_passwords_folders(state: State<NcPasswordsState>) -> Result<Vec<FolderEntry>, String> {
    with_client(&state, |c| c.list_folders())
}

#[tauri::command]
pub fn nc_passwords_tags(state: State<NcPasswordsState>) -> Result<Vec<TagEntry>, String> {
    with_client(&state, |c| c.list_tags())
}

#[tauri::command]
pub fn nc_passwords_favicon_url(
    state: State<NcPasswordsState>,
    domain: String,
) -> Result<String, String> {
    with_client(&state, |c| Ok(c.favicon_url(&domain, 32)))
}
