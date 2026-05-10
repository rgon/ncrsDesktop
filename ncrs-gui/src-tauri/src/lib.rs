use std::sync::{Arc, Mutex};
use std::thread;

use tauri::{
    image::Image,
    menu::{MenuBuilder, MenuItem},
    tray::{TrayIconBuilder, TrayIconId},
    AppHandle, Emitter, EventLoopMessage, Manager, State, WindowEvent,
    PhysicalSize,
};
use tauri_plugin_opener::OpenerExt;
use tauri::async_runtime::spawn;
use tauri_plugin_notification::NotificationExt;
use tokio::time::{sleep, Duration};

use ncrs_core::{mount_ncfs, notifications::NcNotification, MountOptions, SyncState};

// ── Shared app state ─────────────────────────────────────────────────────────

pub struct AppState {
    pub sync_state: Mutex<SyncState>,
    pub mount_options: Mutex<Option<MountOptions>>,
    pub notifications: Mutex<Vec<NcNotification>>,
}

impl Default for AppState {
    fn default() -> Self {
        AppState {
            sync_state: Mutex::new(SyncState::Idle),
            mount_options: Mutex::new(None),
            notifications: Mutex::new(Vec::new()),
        }
    }
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
        SyncState::Error(ref e) => format!("error: {}", e),
    }
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
        let user = opts.username.unwrap_or_default();
        let pass = opts.password.unwrap_or_default();
        thread::spawn(move || {
            if let Err(e) =
                ncrs_core::notifications::dismiss_notification(&base, &user, &pass, id)
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

// ── Tray helpers ──────────────────────────────────────────────────────────────

fn rerender_tray_menu(
    app: &AppHandle,
    sync_state: &SyncState,
) -> Result<tauri::menu::Menu<tauri_runtime_wry::Wry<EventLoopMessage>>, tauri::Error> {
    let pause_text = match sync_state {
        SyncState::Idle => "Pause Sync",
        SyncState::Paused => "Resume Sync",
        SyncState::Syncing => "Pause Sync",
        SyncState::Error(_) => "Sync Error",
    };

    let about_i = MenuItem::with_id(app, "about", "Open main Dialog", true, None::<&str>)?;
    let pause_i = MenuItem::with_id(app, "pause", pause_text, true, None::<&str>)?;
    let settings_i = MenuItem::with_id(app, "settings", "Settings", true, None::<&str>)?;
    let quit_i = MenuItem::with_id(app, "quit", "Exit Nextcloud", true, None::<&str>)?;

    MenuBuilder::new(app)
        .id("tray-menu")
        .item(&about_i)
        .item(&pause_i)
        .separator()
        .item(&settings_i)
        .item(&quit_i)
        .build()
}

fn load_icon(path: &'static str) -> Image<'static> {
    Image::from_path(std::path::Path::new(path)).expect("Failed to load icon image")
}

fn open_main_window(app: &AppHandle) {
    let main_window = match app.get_webview_window("main") {
        Some(w) => w,
        None => return,
    };
    let monitor = main_window.primary_monitor().unwrap();
    if let Some(m) = monitor {
        let _ = main_window.set_size(*m.size());
    } else {
        let _ = main_window.set_size(PhysicalSize::new(1860u32, 1000u32));
    }
    main_window.show().unwrap();
    main_window.set_focus().unwrap();
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
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            close_window,
            get_sync_state,
            get_user_info,
            open_mount_folder,
            get_notifications,
            dismiss_notification,
            open_link,
        ])
        .setup(move |app| {
            spawn(start_ncfs_daemon(app.handle().clone(), app_state_setup));

            let initial_state = SyncState::Idle;
            let menu = rerender_tray_menu(app.handle(), &initial_state)?;
            let icon = load_icon(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/icons/tray_icon.idle.png"
            ));

            let tray = TrayIconBuilder::new()
                .menu(&menu)
                .icon(icon)
                .show_menu_on_left_click(true)
                .build(app)
                .unwrap();

            tray_id_setup.lock().unwrap().replace(tray.id().clone());
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

                let icon_path: &'static str = match new_state {
                    SyncState::Idle => concat!(env!("CARGO_MANIFEST_DIR"), "/icons/tray_icon.idle.png"),
                    SyncState::Paused => concat!(env!("CARGO_MANIFEST_DIR"), "/icons/tray_icon.paused.png"),
                    SyncState::Syncing => concat!(env!("CARGO_MANIFEST_DIR"), "/icons/tray_icon.syncing.png"),
                    SyncState::Error(_) => concat!(env!("CARGO_MANIFEST_DIR"), "/icons/tray_icon.error.png"),
                };

                let _ = tray.set_icon(Some(load_icon(icon_path)));

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
            "settings" => open_main_window(app),
            "quit" => app.exit(0),
            _ => {}
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

    // FUSE mount thread
    let fuse_opts = opts.clone();
    thread::spawn(move || match mount_ncfs(fuse_opts) {
        Ok(()) => log::info!("FUSE unmounted cleanly"),
        Err(e) => log::error!("FUSE error: {}", e),
    });

    // Notification polling thread
    let poll_state = state.clone();
    let poll_app = app.clone();
    let poll_url = opts.url.clone();
    let poll_user = opts.username.clone().unwrap_or_default();
    let poll_pass = opts.password.clone().unwrap_or_default();
    thread::spawn(move || {
        let base = ncrs_core::notifications::base_url(&poll_url);
        loop {
            match ncrs_core::notifications::fetch_notifications(&base, &poll_user, &poll_pass) {
                Ok(notifs) => {
                    *poll_state.notifications.lock().unwrap() = notifs.clone();
                    poll_app.emit("notifications-updated", notifs).ok();
                }
                Err(e) => log::warn!("fetch notifications: {}", e),
            }
            std::thread::sleep(std::time::Duration::from_secs(30));
        }
    });

    Ok(())
}
