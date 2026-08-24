use std::sync::atomic::Ordering;
use std::sync::Arc;

use tauri::State;

use crate::api::{FolderEntry, PasswordEntry, PasswordsClient, TagEntry};
use crate::NcPasswordsState;

/// Clone the client handle out from under the lock, then run the request on
/// the blocking pool.
///
/// Two things matter here. The command is `async`, so Tauri resolves it off
/// the main thread — a synchronous `#[tauri::command]` runs inline on the GTK
/// event loop, and one slow request there freezes the whole webview. And the
/// mutex is released before the request starts, so a stalled call no longer
/// blocks every other passwords command behind it.
async fn with_client<F, T>(state: &State<'_, NcPasswordsState>, f: F) -> Result<T, String>
where
    F: FnOnce(&PasswordsClient) -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    let client = client_handle(state)?;
    tauri::async_runtime::spawn_blocking(move || f(&client))
        .await
        .map_err(|e| format!("passwords task: {e}"))?
}

fn client_handle(state: &State<'_, NcPasswordsState>) -> Result<Arc<PasswordsClient>, String> {
    state
        .client
        .lock()
        .map_err(|e| e.to_string())?
        .clone()
        .ok_or_else(|| "passwords: not connected".to_string())
}

#[tauri::command]
pub async fn nc_passwords_connect(state: State<'_, NcPasswordsState>) -> Result<(), String> {
    let creds = state
        .credentials
        .lock()
        .map_err(|e| e.to_string())?
        .clone()
        .ok_or("passwords: no credentials available")?;

    let (base_url, user, pass) = creds;
    let client = tauri::async_runtime::spawn_blocking(move || {
        let mut client = PasswordsClient::new(&base_url, &user, &pass);
        client.open_session()?;
        Ok::<_, String>(client)
    })
    .await
    .map_err(|e| format!("passwords task: {e}"))??;

    *state.client.lock().map_err(|e| e.to_string())? = Some(Arc::new(client));
    state.connected.store(true, Ordering::Relaxed);
    Ok(())
}

#[tauri::command]
pub async fn nc_passwords_disconnect(state: State<'_, NcPasswordsState>) -> Result<(), String> {
    // Drop the local session first: whether the server ever hears about it is
    // best-effort, but this side must not stay "connected" either way.
    let client = state.client.lock().map_err(|e| e.to_string())?.take();
    state.connected.store(false, Ordering::Relaxed);

    if let Some(client) = client {
        tauri::async_runtime::spawn_blocking(move || {
            let _ = client.close_session();
        })
        .await
        .map_err(|e| format!("passwords task: {e}"))?;
    }
    Ok(())
}

/// Stays synchronous — the frontend calls it on mount to decide what to render,
/// and it must answer immediately. It reads the flag rather than the client
/// mutex precisely so an in-flight request can never stall it.
#[tauri::command]
pub fn nc_passwords_is_connected(state: State<NcPasswordsState>) -> bool {
    state.connected.load(Ordering::Relaxed)
}

#[tauri::command]
pub async fn nc_passwords_list(
    state: State<'_, NcPasswordsState>,
) -> Result<Vec<PasswordEntry>, String> {
    with_client(&state, |c| c.list_passwords()).await
}

#[tauri::command]
pub async fn nc_passwords_show(
    state: State<'_, NcPasswordsState>,
    id: String,
) -> Result<PasswordEntry, String> {
    with_client(&state, move |c| c.show_password(&id)).await
}

#[tauri::command]
pub async fn nc_passwords_search(
    state: State<'_, NcPasswordsState>,
    term: String,
) -> Result<Vec<PasswordEntry>, String> {
    with_client(&state, move |c| {
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
    .await
}

#[tauri::command]
pub async fn nc_passwords_create(
    state: State<'_, NcPasswordsState>,
    label: String,
    username: String,
    password: String,
    url: String,
    folder: Option<String>,
) -> Result<PasswordEntry, String> {
    with_client(&state, move |c| {
        c.create_password(&label, &username, &password, &url, folder.as_deref())
    })
    .await
}

#[tauri::command]
pub async fn nc_passwords_delete(
    state: State<'_, NcPasswordsState>,
    id: String,
) -> Result<(), String> {
    with_client(&state, move |c| c.delete_password(&id)).await
}

#[tauri::command]
pub async fn nc_passwords_folders(
    state: State<'_, NcPasswordsState>,
) -> Result<Vec<FolderEntry>, String> {
    with_client(&state, |c| c.list_folders()).await
}

#[tauri::command]
pub async fn nc_passwords_tags(
    state: State<'_, NcPasswordsState>,
) -> Result<Vec<TagEntry>, String> {
    with_client(&state, |c| c.list_tags()).await
}

/// Pure string formatting, so it stays synchronous — it takes the client lock
/// only long enough to clone the handle, never across a request.
#[tauri::command]
pub fn nc_passwords_favicon_url(
    state: State<NcPasswordsState>,
    domain: String,
) -> Result<String, String> {
    let client = state
        .client
        .lock()
        .map_err(|e| e.to_string())?
        .clone()
        .ok_or_else(|| "passwords: not connected".to_string())?;
    Ok(client.favicon_url(&domain, 32))
}
