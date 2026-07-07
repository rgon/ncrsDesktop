use std::sync::{Arc, Mutex};
use std::thread;

use tauri::{
    image::Image,
    menu::{MenuBuilder, MenuItem},
    tray::{TrayIconBuilder, TrayIconId},
    AppHandle, Emitter, EventLoopMessage, Listener, Manager, State, WindowEvent,
};
use tauri_plugin_opener::OpenerExt;
use tauri::async_runtime::spawn;
use tauri_plugin_notification::NotificationExt;
use tokio::time::{sleep, Duration};

use ncrs_core::{mount_ncfs, mutation_journal::{self, SharedJournal, JournalEntry, ConflictRecord}, notifications::NcNotification, search::{SearchProvider, SearchResultGroup}, ipc::StorageStats, ErrorLog, MountOptions, SyncError, SyncState, TransferMap, TransferProgress};

mod plugins;

// ── Shared app state ─────────────────────────────────────────────────────────

pub struct AppState {
    pub sync_state: Mutex<SyncState>,
    pub mount_options: Mutex<Option<MountOptions>>,
    pub notifications: Mutex<Vec<NcNotification>>,
    pub error_log: ErrorLog,
    pub transfer_map: TransferMap,
    pub journal: SharedJournal,
    pub paused: Arc<std::sync::atomic::AtomicBool>,
    /// True when mirroring an external daemon (systemd service) over IPC
    /// instead of owning the mount in-process.
    pub attached: std::sync::atomic::AtomicBool,
    /// Set to true to cancel an in-progress login flow poll loop.
    pub login_flow_cancel: Arc<std::sync::atomic::AtomicBool>,
}

impl Default for AppState {
    fn default() -> Self {
        let tmp_dir = std::env::temp_dir().join("ncrs_default_journal");
        let _ = std::fs::create_dir_all(&tmp_dir);
        AppState {
            sync_state: Mutex::new(SyncState::Idle),
            mount_options: Mutex::new(None),
            notifications: Mutex::new(Vec::new()),
            error_log: Arc::new(Mutex::new(std::collections::VecDeque::new())),
            transfer_map: Arc::new(Mutex::new(std::collections::HashMap::new())),
            journal: Arc::new(Mutex::new(mutation_journal::MutationJournal::load_or_create(&tmp_dir))),
            paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            attached: std::sync::atomic::AtomicBool::new(false),
            login_flow_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

// ── IPC client (attach mode) ──────────────────────────────────────────────────

/// Send line verbs to a running daemon's IPC socket; one reply line per verb.
/// Returns None when no daemon is listening (absent or stale socket).
fn ipc_request(verbs: &[&str]) -> Option<Vec<String>> {
    use std::io::{BufRead, Write};
    let sock = ncrs_core::ipc::socket_path();
    let stream = std::os::unix::net::UnixStream::connect(&sock).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(2))).ok()?;
    let mut writer = stream.try_clone().ok()?;
    let mut reader = std::io::BufReader::new(stream);
    let mut replies = Vec::with_capacity(verbs.len());
    for verb in verbs {
        writeln!(writer, "{}", verb).ok()?;
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        replies.push(line.trim().to_string());
    }
    Some(replies)
}

// ── Tauri commands ────────────────────────────────────────────────────────────

#[tauri::command]
fn close_window(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        w.hide().unwrap();
    }
}

#[tauri::command]
fn get_sync_state(state: State<Arc<AppState>>) -> String {
    match *state.sync_state.lock().unwrap() {
        SyncState::Idle => "idle".into(),
        SyncState::Syncing => "syncing".into(),
        SyncState::Paused => "paused".into(),
        SyncState::Unmounted => "unmounted".into(),
        SyncState::Wiped => "wiped".into(),
        SyncState::Error(ref e) => format!("error: {}", e),
    }
}

#[tauri::command]
async fn remount(app: AppHandle, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    {
        let ss = state.sync_state.lock().unwrap();
        if !matches!(*ss, SyncState::Unmounted | SyncState::Error(_)) {
            return Err("Can only remount when unmounted or in error".into());
        }
    }
    *state.sync_state.lock().unwrap() = SyncState::Idle;
    state.paused.store(false, std::sync::atomic::Ordering::Relaxed);
    app.emit("sync-state-changed", "idle").ok();
    let s = (*state).clone();
    spawn(start_ncfs_daemon(app, s));
    Ok(())
}

#[derive(serde::Serialize)]
pub struct UserInfo {
    pub username: String,
    pub server_url: String,
    pub mount_point: String,
    pub avatar_url: String,
}

#[tauri::command]
fn get_user_info(state: State<Arc<AppState>>) -> Option<UserInfo> {
    let opts = state.mount_options.lock().unwrap();
    opts.as_ref().map(|o| {
        let username = o.username.clone().unwrap_or_else(|| o.log_user.clone());
        let base = ncrs_core::notifications::base_url(&o.url);
        UserInfo {
            avatar_url: format!("{}/index.php/avatar/{}/64", base, username),
            username,
            server_url: o.url.clone(),
            mount_point: o.mount_point.to_string_lossy().into_owned(),
        }
    })
}

#[tauri::command]
fn open_mount_folder(state: State<Arc<AppState>>, app: AppHandle) {
    let mount = {
        let opts = state.mount_options.lock().unwrap();
        opts.as_ref().map(|o| o.mount_point.clone())
    };
    if let Some(path) = mount {
        app.opener()
            .open_path(path.to_string_lossy().as_ref(), None::<&str>)
            .ok();
    }
}

#[tauri::command]
fn get_notifications(state: State<Arc<AppState>>) -> Vec<NcNotification> {
    state.notifications.lock().unwrap().clone()
}

#[tauri::command]
fn dismiss_notification(state: State<Arc<AppState>>, id: u64) {
    let opts = {
        let g = state.mount_options.lock().unwrap();
        g.clone()
    };
    if let Some(opts) = opts {
        state.notifications.lock().unwrap().retain(|n| n.notification_id != id);
        let base = ncrs_core::notifications::base_url(&opts.url);
        let creds = match opts.credentials() {
            Ok(c) => c,
            Err(e) => { log::warn!("dismiss notification: {}", e); return; }
        };
        let http3 = opts.http3;
        thread::spawn(move || {
            if let Err(e) =
                ncrs_core::notifications::dismiss_notification(&base, &creds, id, http3)
            {
                log::warn!("dismiss notification {}: {}", id, e);
            }
        });
    }
}

#[tauri::command]
fn open_link(url: String, app: AppHandle) {
    app.opener().open_url(&url, None::<&str>).ok();
}

#[tauri::command]
fn reveal_in_file_manager(path: String, app: AppHandle) {
    app.opener().reveal_item_in_dir(&path).ok();
}

#[tauri::command]
fn get_errors(state: State<Arc<AppState>>) -> Vec<SyncError> {
    state.error_log.lock().unwrap().iter().cloned().collect()
}

#[tauri::command]
fn clear_errors(state: State<Arc<AppState>>) {
    state.error_log.lock().unwrap().clear();
}

#[tauri::command]
fn get_transfers(state: State<Arc<AppState>>) -> Vec<TransferProgress> {
    state.transfer_map.lock().unwrap().values().cloned().collect()
}

#[tauri::command]
fn get_pending_mutations(state: State<Arc<AppState>>) -> Vec<JournalEntry> {
    state.journal.lock().unwrap().entries().iter().cloned().collect()
}

#[tauri::command]
fn get_conflicts(state: State<Arc<AppState>>) -> Vec<ConflictRecord> {
    state.journal.lock().unwrap().unresolved_conflicts().into_iter().cloned().collect()
}

#[tauri::command]
fn resolve_conflict(state: State<Arc<AppState>>, id: u64) {
    state.journal.lock().unwrap().resolve_conflict(id);
}

#[tauri::command]
async fn get_storage_stats() -> Result<StorageStats, String> {
    tokio::task::spawn_blocking(|| {
        let sock = ncrs_core::ipc::socket_path();
        let stream = std::os::unix::net::UnixStream::connect(&sock)
            .map_err(|e| format!("ipc connect: {}", e))?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).ok();
        use std::io::{BufRead, Write};
        let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
        writeln!(writer, "STORAGE").map_err(|e| e.to_string())?;
        let mut reader = std::io::BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).map_err(|e| e.to_string())?;
        serde_json::from_str(line.trim()).map_err(|e| format!("parse: {}", e))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn fetch_search_providers(
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<SearchProvider>, String> {
    let opts = state
        .mount_options
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| "not connected".to_string())?;
    let base = ncrs_core::notifications::base_url(&opts.url);
    let creds = opts.credentials()?;
    let http3 = opts.http3;

    tokio::task::spawn_blocking(move || {
        ncrs_core::search::fetch_providers(&base, &creds, http3)
    })
    .await
    .map_err(|e| format!("fetch providers failed: {}", e))?
}

#[tauri::command]
async fn search_nextcloud(
    state: State<'_, Arc<AppState>>,
    term: String,
    provider_ids: Vec<String>,
) -> Result<Vec<SearchResultGroup>, String> {
    let opts = state
        .mount_options
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| "not connected".to_string())?;
    let base = ncrs_core::notifications::base_url(&opts.url);
    let creds = opts.credentials()?;
    let http3 = opts.http3;
    let mount_point = opts.mount_point.clone();

    let mut groups = tokio::task::spawn_blocking(move || {
        ncrs_core::search::search_filtered(&base, &creds, &term, http3, &provider_ids)
    })
    .await
    .map_err(|e| format!("search task failed: {}", e))??;

    for group in &mut groups {
        if group.provider_id == "files" {
            for entry in &mut group.entries {
                if let Some(dir) = extract_dir_param(&entry.resource_url) {
                    let file_path = if dir == "/" {
                        format!("/{}", entry.title)
                    } else {
                        format!("{}/{}", dir, entry.title)
                    };
                    let rel = file_path.strip_prefix('/').unwrap_or(&file_path);
                    entry.local_path = Some(mount_point.join(rel).to_string_lossy().into_owned());
                }
            }
        }
    }

    Ok(groups)
}

#[tauri::command]
fn get_plugin_metas() -> Vec<ncrs_plugin::PluginMeta> {
    let metas = plugins::all_metas();
    log::info!("get_plugin_metas: returning {} plugin(s): {:?}", metas.len(), metas.iter().map(|m| &m.id).collect::<Vec<_>>());
    metas
}

// ── Login flow commands ───────────────────────────────────────────────────────

#[derive(serde::Serialize)]
pub struct ConfigStatus {
    pub needs_login: bool,
    pub server_url: Option<String>,
}

#[tauri::command]
fn get_config_status() -> ConfigStatus {
    match ncrs_core::config::load_config() {
        Ok(opts) => {
            // Config loaded; check credentials are actually usable.
            if opts.credentials().is_err() {
                let server = ncrs_core::notifications::base_url(&opts.url);
                ConfigStatus { needs_login: true, server_url: Some(server) }
            } else {
                ConfigStatus { needs_login: false, server_url: None }
            }
        }
        Err(_) => ConfigStatus { needs_login: true, server_url: None },
    }
}

#[derive(serde::Serialize, Clone)]
pub struct LoginComplete {
    pub server: String,
    pub login_name: String,
}

/// Initialise the Nextcloud Login Flow v2, open the browser, and start a
/// background poll loop. Returns the login URL so the frontend can show a
/// fallback link in case the browser didn't open.
#[tauri::command]
async fn start_login_flow(
    server_url: String,
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
) -> Result<String, String> {
    // Cancel any previous poll that might still be running.
    state.login_flow_cancel.store(true, std::sync::atomic::Ordering::Relaxed);

    let server = server_url.trim().trim_end_matches('/').to_string();
    if server.is_empty() {
        return Err("Server URL must not be empty".into());
    }

    let init = tokio::task::spawn_blocking({
        let s = server.clone();
        move || ncrs_core::login_flow::init_login_flow(&s)
    })
    .await
    .map_err(|e| format!("task error: {}", e))??;

    // Open the browser so the user can authorise this app.
    if let Err(e) = app.opener().open_url(&init.login_url, None::<&str>) {
        log::warn!("could not open browser for login flow: {}", e);
    }

    let login_url = init.login_url.clone();
    let poll_endpoint = init.poll_endpoint.clone();
    let poll_token = init.poll_token.clone();

    // Reset cancel flag for the new loop.
    state.login_flow_cancel.store(false, std::sync::atomic::Ordering::Relaxed);
    let cancel = state.login_flow_cancel.clone();
    let state_clone = (*state).clone();

    spawn(async move {
        const POLL_INTERVAL_SECS: u64 = 2;
        const TIMEOUT_SECS: u64 = 300; // 5 minutes
        let max_iters = TIMEOUT_SECS / POLL_INTERVAL_SECS;

        for _ in 0..max_iters {
            sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                log::info!("login flow poll cancelled");
                return;
            }

            let ep = poll_endpoint.clone();
            let tok = poll_token.clone();
            let result = tokio::task::spawn_blocking(move || {
                ncrs_core::login_flow::poll_login_flow(&ep, &tok)
            })
            .await;

            match result {
                Ok(Ok(Some(creds))) => {
                    log::info!("login flow succeeded for {}", creds.login_name);

                    if let Err(e) = write_config_from_login(&creds) {
                        log::error!("failed to write config after login: {}", e);
                        app.emit("login-error", e).ok();
                        return;
                    }

                    app.emit(
                        "login-complete",
                        LoginComplete {
                            server: creds.server.clone(),
                            login_name: creds.login_name.clone(),
                        },
                    )
                    .ok();

                    // Start the mount/daemon now that credentials are saved.
                    let _ = spawn(start_ncfs_daemon(app.clone(), state_clone));
                    return;
                }
                Ok(Ok(None)) => {} // Not authorised yet, keep polling.
                Ok(Err(e)) => {
                    log::warn!("login flow poll error: {}", e);
                    // Non-fatal network hiccup — keep polling.
                }
                Err(e) => {
                    log::error!("login flow poll task panic: {}", e);
                    return;
                }
            }
        }

        log::warn!("login flow timed out after {} seconds", TIMEOUT_SECS);
        app.emit("login-error", "Login timed out. Please try again.").ok();
    });

    Ok(login_url)
}

fn write_config_from_login(
    creds: &ncrs_core::login_flow::LoginResult,
) -> Result<(), String> {
    let webdav_url = ncrs_core::login_flow::webdav_url(&creds.server, &creds.login_name);

    // Save the app password to the system keyring — not to the config file.
    ncrs_core::config::save_password_to_keyring(
        &creds.login_name,
        &webdav_url,
        &creds.app_password,
    )?;

    let mount_point = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/home"))
        .join("Nextcloud")
        .to_string_lossy()
        .into_owned();

    // No password field — credentials live in the keyring.
    let config = format!(
        "# ncRS Desktop configuration\n\
         url: \"{}\"\n\
         username: \"{}\"\n\
         mount_point: \"{}\"\n\
         user: \"{}\"\n",
        webdav_url,
        creds.login_name,
        mount_point,
        creds.login_name,
    );

    let config_path = ncrs_core::config::config_path();
    if let Some(dir) = config_path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create config dir: {}", e))?;
    }
    std::fs::write(&config_path, config).map_err(|e| format!("write config: {}", e))?;
    ncrs_core::config::warn_config_permissions(&config_path);
    log::info!("config written to {}", config_path.display());
    Ok(())
}

fn extract_dir_param(url: &str) -> Option<String> {
    let query = url.split('?').nth(1)?;
    for pair in query.split('&') {
        if let Some(val) = pair.strip_prefix("dir=") {
            let decoded = val.replace("+", " ");
            let mut out = Vec::new();
            let bytes = decoded.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'%' && i + 2 < bytes.len() {
                    if let Ok(byte) = u8::from_str_radix(
                        std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""),
                        16,
                    ) {
                        out.push(byte);
                        i += 3;
                        continue;
                    }
                }
                out.push(bytes[i]);
                i += 1;
            }
            return Some(String::from_utf8_lossy(&out).into_owned());
        }
    }
    None
}

// ── Tray helpers ──────────────────────────────────────────────────────────────

fn rerender_tray_menu(
    app: &AppHandle,
    sync_state: &SyncState,
) -> Result<tauri::menu::Menu<tauri_runtime_wry::Wry<EventLoopMessage>>, tauri::Error> {
    let about_i = MenuItem::with_id(app, "about", "Open ncRS", true, None::<&str>)?;
    let quit_i = MenuItem::with_id(app, "quit", "Exit Nextcloud", true, None::<&str>)?;

    let mut builder = MenuBuilder::new(app)
        .id("tray-menu")
        .item(&about_i);

    if *sync_state == SyncState::Unmounted {
        let remount_i = MenuItem::with_id(app, "remount", "Remount", true, None::<&str>)?;
        builder = builder.item(&remount_i);
    } else if *sync_state == SyncState::Wiped {
        let wiped_i = MenuItem::with_id(app, "wiped", "Device wiped — reconfigure to reconnect", false, None::<&str>)?;
        builder = builder.item(&wiped_i);
    } else {
        let pause_text = match sync_state {
            SyncState::Idle => "Pause Sync",
            SyncState::Paused => "Resume Sync",
            SyncState::Syncing => "Pause Sync",
            SyncState::Unmounted | SyncState::Wiped => unreachable!(),
            SyncState::Error(_) => "Sync Error",
        };
        let pause_i = MenuItem::with_id(app, "pause", pause_text, true, None::<&str>)?;
        builder = builder.item(&pause_i);
    }

    let plugin_items = plugins::all_tray_items(app)?;
    log::info!("tray menu: {} plugin item(s)", plugin_items.len());
    if !plugin_items.is_empty() {
        builder = builder.separator();
        for item in &plugin_items {
            builder = builder.item(item);
        }
    }

    builder
        .separator()
        .item(&quit_i)
        .build()
}

// Icons are embedded at compile time; the installed binary must not depend
// on the build machine's checkout path (CARGO_MANIFEST_DIR).
#[derive(Clone, Copy)]
enum TrayIcon {
    Idle,
    Paused,
    Syncing,
    Error,
}

impl TrayIcon {
    fn for_state(state: &SyncState) -> Self {
        match state {
            SyncState::Idle => TrayIcon::Idle,
            SyncState::Paused | SyncState::Unmounted | SyncState::Wiped => TrayIcon::Paused,
            SyncState::Syncing => TrayIcon::Syncing,
            SyncState::Error(_) => TrayIcon::Error,
        }
    }
}

fn load_icon(kind: TrayIcon) -> Image<'static> {
    static ICONS: [std::sync::OnceLock<Image<'static>>; 4] = [const { std::sync::OnceLock::new() }; 4];
    let (slot, bytes): (usize, &'static [u8]) = match kind {
        TrayIcon::Idle => (0, include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/icons/tray_icon.idle.png"))),
        TrayIcon::Paused => (1, include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/icons/tray_icon.paused.png"))),
        TrayIcon::Syncing => (2, include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/icons/tray_icon.syncing.png"))),
        TrayIcon::Error => (3, include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/icons/tray_icon.error.png"))),
    };
    ICONS[slot]
        .get_or_init(|| Image::from_bytes(bytes).expect("embedded tray icon is valid PNG"))
        .clone()
}

fn open_main_window(app: &AppHandle) {
    ncrs_plugin::open_main_window(app);
}

// ── Main entry point ──────────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::init();
    let app_state = Arc::new(AppState::default());
    let app_state_setup = app_state.clone();
    let app_state_menu = app_state.clone();

    let tray_icon_id: Arc<Mutex<Option<TrayIconId>>> = Arc::new(Mutex::new(None));
    let tray_id_setup = tray_icon_id.clone();
    let tray_id_menu = tray_icon_id.clone();

    tauri::Builder::default()
        // Must be the first plugin: a second launch (e.g. the app-menu entry
        // while the autostarted instance runs) would spawn a second daemon
        // that steals the FUSE mount — surface the existing window instead.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            open_main_window(app);
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            close_window,
            get_config_status,
            start_login_flow,
            get_sync_state,
            get_user_info,
            open_mount_folder,
            get_notifications,
            dismiss_notification,
            open_link,
            reveal_in_file_manager,
            get_errors,
            clear_errors,
            get_transfers,
            get_pending_mutations,
            get_conflicts,
            resolve_conflict,
            fetch_search_providers,
            search_nextcloud,
            get_storage_stats,
            get_plugin_metas,
            remount,
            nc_passwords::commands::nc_passwords_connect,
            nc_passwords::commands::nc_passwords_disconnect,
            nc_passwords::commands::nc_passwords_is_connected,
            nc_passwords::commands::nc_passwords_list,
            nc_passwords::commands::nc_passwords_show,
            nc_passwords::commands::nc_passwords_search,
            nc_passwords::commands::nc_passwords_create,
            nc_passwords::commands::nc_passwords_delete,
            nc_passwords::commands::nc_passwords_folders,
            nc_passwords::commands::nc_passwords_tags,
            nc_passwords::commands::nc_passwords_favicon_url,
        ])
        .setup(move |app| {
            nc_passwords::setup(app.handle());
            log::info!("plugin setup done: nc_passwords");
            let state_listener = app_state_setup.clone();
            spawn(start_ncfs_daemon(app.handle().clone(), app_state_setup));

            let initial_state = SyncState::Idle;
            let menu = rerender_tray_menu(app.handle(), &initial_state)?;
            let icon = load_icon(TrayIcon::Idle);

            let tray = TrayIconBuilder::new()
                .menu(&menu)
                .icon(icon)
                .show_menu_on_left_click(true)
                .build(app)
                .unwrap();

            tray_id_setup.lock().unwrap().replace(tray.id().clone());

            let tray_id_listener = tray_icon_id.clone();
            let app_handle_listener = app.handle().clone();
            app.listen("sync-state-changed", move |event| {
                let payload = event.payload().trim_matches('"');
                let tray_id = tray_id_listener.lock().unwrap().clone();
                let Some(tray_id) = tray_id else { return };
                let ss = state_listener.sync_state.lock().unwrap().clone();
                let Some(tray) = app_handle_listener.tray_by_id(&tray_id) else { return };

                let icon_kind = match payload {
                    "wiped" => TrayIcon::Error,
                    "unmounted" | "paused" => TrayIcon::Paused,
                    "syncing" => TrayIcon::Syncing,
                    _ => TrayIcon::Idle,
                };
                let _ = tray.set_icon(Some(load_icon(icon_kind)));
                if let Ok(menu) = rerender_tray_menu(&app_handle_listener, &ss) {
                    let _ = tray.set_menu(Some(menu));
                }
            });

            Ok(())
        })
        .on_window_event(|app, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                if let Some(w) = app.get_webview_window("main") {
                    w.hide().unwrap();
                }
                api.prevent_close();
            }
        })
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "about" => open_main_window(app),
            "pause" => {
                let tray_id = tray_id_menu.lock().unwrap().clone();
                let Some(tray_id) = tray_id else { return };
                let Some(tray) = app.tray_by_id(&tray_id) else { return };

                let new_state = {
                    let mut state = app_state_menu.sync_state.lock().unwrap();
                    *state = if *state == SyncState::Paused {
                        SyncState::Idle
                    } else {
                        SyncState::Paused
                    };
                    state.clone()
                };

                app_state_menu.paused.store(
                    new_state == SyncState::Paused,
                    std::sync::atomic::Ordering::Relaxed,
                );

                // Attach mode: the external daemon owns sync — forward the
                // toggle over IPC (the attach poll confirms the new state).
                if app_state_menu.attached.load(std::sync::atomic::Ordering::Relaxed) {
                    let verb = if new_state == SyncState::Paused { "PAUSE" } else { "RESUME" };
                    thread::spawn(move || {
                        if ipc_request(&[verb]).is_none() {
                            log::warn!("could not forward {} to external daemon", verb);
                        }
                    });
                }

                let _ = tray.set_icon(Some(load_icon(TrayIcon::for_state(&new_state))));

                app.notification()
                    .builder()
                    .title("NCRS Sync Status")
                    .body(if new_state == SyncState::Paused { "Sync paused" } else { "Sync resumed" })
                    .show()
                    .unwrap();

                if let Ok(menu) = rerender_tray_menu(app.app_handle(), &new_state) {
                    let _ = tray.set_menu(Some(menu));
                }

                app.emit("sync-state-changed", new_state.to_string()).ok();
            }
            "remount" => {
                let tray_id = tray_id_menu.lock().unwrap().clone();
                let Some(tray_id) = tray_id else { return };
                let Some(tray) = app.tray_by_id(&tray_id) else { return };

                {
                    let ss = app_state_menu.sync_state.lock().unwrap();
                    if !matches!(*ss, SyncState::Unmounted | SyncState::Error(_)) { return; }
                }

                *app_state_menu.sync_state.lock().unwrap() = SyncState::Idle;
                app_state_menu.paused.store(false, std::sync::atomic::Ordering::Relaxed);
                app.emit("sync-state-changed", "idle").ok();

                let _ = tray.set_icon(Some(load_icon(TrayIcon::Idle)));
                if let Ok(menu) = rerender_tray_menu(app.app_handle(), &SyncState::Idle) {
                    let _ = tray.set_menu(Some(menu));
                }

                let remount_state = app_state_menu.clone();
                let remount_app = app.app_handle().clone();
                spawn(start_ncfs_daemon(remount_app, remount_state));
            }
            "quit" => {
                let mount_point = app_state_menu.mount_options.lock().unwrap()
                    .as_ref()
                    .map(|o| o.mount_point.to_string_lossy().to_string());
                // Attach mode: the mount belongs to the external daemon —
                // quitting the tray must not unmount it.
                if app_state_menu.attached.load(std::sync::atomic::Ordering::Relaxed) {
                    log::info!("quit: leaving external daemon's mount untouched");
                    app.exit(0);
                    return;
                }
                if let Some(mp) = mount_point {
                    log::info!("quit: unmounting {}", mp);
                    // Clean unmount first so the dir can be removed below;
                    // fall back to a lazy detach if the mount is busy.
                    let clean = std::process::Command::new("fusermount")
                        .args(["-u", &mp])
                        .output()
                        .map(|o| o.status.success())
                        .unwrap_or(false);
                    if !clean {
                        let _ = std::process::Command::new("fusermount")
                            .args(["-uz", &mp])
                            .output();
                    }
                    // Best-effort: refuses non-empty or still-mounted dirs.
                    let _ = std::fs::remove_dir(&mp);
                }
                app.exit(0);
            }
            other => { plugins::handle_plugin_tray_event(app, other); }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application")
}

async fn start_ncfs_daemon(app: AppHandle, state: Arc<AppState>) -> Result<(), ()> {
    sleep(Duration::from_millis(500)).await;

    let opts = match ncrs_core::config::load_config() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("ncrs config error: {}", e);
            app.notification()
                .builder()
                .title("ncRS: configuration missing")
                .body(&e)
                .show()
                .ok();
            return Ok(());
        }
    };

    *state.mount_options.lock().unwrap() = Some(opts.clone());

    let base_url = ncrs_core::notifications::base_url(&opts.url);
    let user = opts.username.clone().unwrap_or_default();
    let pass = opts.password.clone().unwrap_or_default();
    nc_passwords::set_credentials(&app, &base_url, &user, &pass);

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    state.attached.store(false, std::sync::atomic::Ordering::Relaxed);
    let external_daemon = tokio::task::spawn_blocking(|| ipc_request(&["STATE"]))
        .await
        .ok()
        .flatten()
        .is_some();

    if external_daemon {
        // Attach mode: another daemon (e.g. the ncrs systemd user service)
        // already owns the mount — mirror its state over IPC instead of
        // mounting a second time.
        log::info!("existing ncrs daemon detected — attaching via IPC");
        state.attached.store(true, std::sync::atomic::Ordering::Relaxed);
        spawn(attached_poll_loop(app.clone(), state.clone(), shutdown_tx));
    } else {
        // FUSE mount thread — share error_log and transfer_map with the core
        let fuse_opts = opts.clone();
        let error_log = state.error_log.clone();
        let transfer_map = state.transfer_map.clone();
        let journal = state.journal.clone();
        let fuse_app = app.clone();
        let fuse_state = state.clone();
        let fuse_paused = state.paused.clone();
        thread::spawn(move || {
            let result = mount_ncfs(fuse_opts, Some(error_log), Some(transfer_map), Some(journal), Some(fuse_paused));
            match &result {
                Ok(()) => {
                    log::info!("FUSE unmounted cleanly");
                    *fuse_state.sync_state.lock().unwrap() = SyncState::Unmounted;
                    fuse_app.emit("sync-state-changed", "unmounted").ok();
                }
                Err(e) if e == "REMOTE_WIPE" => {
                    log::warn!("FUSE unmounted due to remote wipe");
                    *fuse_state.sync_state.lock().unwrap() = SyncState::Wiped;
                    fuse_app.emit("sync-state-changed", "wiped").ok();
                    fuse_app
                        .notification()
                        .builder()
                        .title("Nextcloud: Device Wiped")
                        .body("This device has been remotely wiped. All cached data has been deleted and credentials cleared.")
                        .show()
                        .ok();
                }
                Err(e) => {
                    log::error!("FUSE error: {}", e);
                    *fuse_state.sync_state.lock().unwrap() = SyncState::Error(e.clone());
                    fuse_app.emit("sync-state-changed", format!("error: {}", e)).ok();
                }
            }
            let _ = shutdown_tx.send(true);
        });
    }

    // Notification polling — async on Tokio runtime, no dedicated OS thread
    if let Ok(poll_creds) = opts.credentials() {
        let poll_state = state.clone();
        let poll_app = app.clone();
        let poll_url = opts.url.clone();
        let poll_http3 = opts.http3;
        let notif_shutdown = shutdown_rx.clone();
        spawn(async move {
            let base = ncrs_core::notifications::base_url(&poll_url);
            loop {
                if *notif_shutdown.borrow() { break; }
                let b = base.clone();
                let c = poll_creds.clone();
                match tokio::task::spawn_blocking(move || {
                    ncrs_core::notifications::fetch_notifications(&b, &c, poll_http3)
                }).await {
                    Ok(Ok(notifs)) => {
                        *poll_state.notifications.lock().unwrap() = notifs.clone();
                        poll_app.emit("notifications-updated", notifs).ok();
                    }
                    Ok(Err(e)) => log::warn!("fetch notifications: {}", e),
                    Err(e) => log::warn!("notification poll panicked: {}", e),
                }
                sleep(Duration::from_secs(30)).await;
            }
        });
    }

    // Error log polling — check every 2s, emit event + desktop notification on new errors
    let err_state = state.clone();
    let err_app = app.clone();
    let err_shutdown = shutdown_rx.clone();
    spawn(async move {
        let mut prev_count = 0usize;
        loop {
            if *err_shutdown.borrow() { break; }
            sleep(Duration::from_secs(2)).await;
            let errors: Vec<SyncError> = err_state.error_log.lock().unwrap().iter().cloned().collect();
            let count = errors.len();
            if count != prev_count {
                if count > prev_count {
                    for err in errors.iter().skip(prev_count) {
                        err_app.notification()
                            .builder()
                            .title("ncRS: Sync Error")
                            .body(&format!("{}: {}", err.path.display(), err.message))
                            .show()
                            .ok();
                    }
                }
                err_app.emit("sync-errors-updated", &errors).ok();
                prev_count = count;
            }
        }
    });

    // Transfer progress polling — 500ms when active, 2s when idle
    let xfer_state = state.clone();
    let xfer_app = app.clone();
    let xfer_shutdown = shutdown_rx.clone();
    spawn(async move {
        let mut was_active = false;
        loop {
            if *xfer_shutdown.borrow() { break; }
            let transfers: Vec<TransferProgress> = xfer_state.transfer_map.lock().unwrap().values().cloned().collect();
            let active = !transfers.is_empty();
            if active || was_active {
                xfer_app.emit("transfers-updated", &transfers).ok();
            }
            was_active = active;
            if active {
                sleep(Duration::from_millis(500)).await;
            } else {
                sleep(Duration::from_secs(2)).await;
            }
        }
    });

    // Journal + conflicts polling
    let jrnl_state = state.clone();
    let jrnl_app = app.clone();
    let jrnl_shutdown = shutdown_rx.clone();
    spawn(async move {
        let mut prev_pending = 0usize;
        let mut prev_conflicts = 0usize;
        loop {
            if *jrnl_shutdown.borrow() { break; }
            sleep(Duration::from_secs(2)).await;
            let j = jrnl_state.journal.lock().unwrap();
            let pending = j.len();
            let conflicts: Vec<ConflictRecord> = j.unresolved_conflicts().into_iter().cloned().collect();
            let conflict_count = conflicts.len();
            drop(j);
            if pending != prev_pending {
                jrnl_app.emit("journal-updated", pending).ok();
                prev_pending = pending;
            }
            if conflict_count != prev_conflicts {
                jrnl_app.emit("conflicts-updated", &conflicts).ok();
                prev_conflicts = conflict_count;
            }
        }
    });

    Ok(())
}

// Attach mode: mirror the external daemon's state over IPC into the same
// shared structures the embedded mount would fill, so the tray, events, and
// frontend commands behave identically in both modes.
async fn attached_poll_loop(
    app: AppHandle,
    state: Arc<AppState>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
) {
    let mut failures = 0u32;
    loop {
        sleep(Duration::from_secs(2)).await;

        let replies = tokio::task::spawn_blocking(|| ipc_request(&["STATE", "ERRORS", "TRANSFERS", "JOURNAL", "CONFLICTS"]))
            .await
            .ok()
            .flatten();
        let Some(replies) = replies else {
            failures += 1;
            // ~6s of silence: the daemon is gone. Show Unmounted; the Remount
            // menu re-probes and either re-attaches (service restarted) or
            // mounts embedded (mount point now free).
            if failures >= 3 {
                log::warn!("external ncrs daemon stopped responding — detaching");
                state.attached.store(false, std::sync::atomic::Ordering::Relaxed);
                *state.sync_state.lock().unwrap() = SyncState::Unmounted;
                app.emit("sync-state-changed", "unmounted").ok();
                let _ = shutdown_tx.send(true);
                return;
            }
            continue;
        };
        failures = 0;

        let new_state = match replies.first().map(String::as_str) {
            Some("paused") => SyncState::Paused,
            Some("syncing") => SyncState::Syncing,
            _ => SyncState::Idle,
        };
        let changed = {
            let mut ss = state.sync_state.lock().unwrap();
            if *ss != new_state {
                *ss = new_state.clone();
                true
            } else {
                false
            }
        };
        if changed {
            app.emit("sync-state-changed", new_state.to_string()).ok();
        }

        // The error/transfer pollers read these maps and emit the usual
        // events (and desktop notifications), same as in embedded mode.
        if let Some(json) = replies.get(1) {
            if let Ok(errors) = serde_json::from_str::<Vec<SyncError>>(json) {
                let mut log = state.error_log.lock().unwrap();
                log.clear();
                log.extend(errors);
            }
        }
        if let Some(json) = replies.get(2) {
            if let Ok(transfers) = serde_json::from_str::<Vec<TransferProgress>>(json) {
                let mut map = state.transfer_map.lock().unwrap();
                map.clear();
                map.extend(transfers.into_iter().map(|t| (t.path.clone(), t)));
            }
        }

        // Mirror journal and conflicts from the daemon so get_pending_mutations,
        // get_conflicts, and the journal poller events work in attach mode.
        if let Some(json) = replies.get(3) {
            if let Ok(entries) = serde_json::from_str::<Vec<JournalEntry>>(json) {
                let conflicts: Vec<ConflictRecord> = replies
                    .get(4)
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_default();
                state.journal.lock().unwrap().replace_from_remote(entries, conflicts);
            }
        }
    }
}
