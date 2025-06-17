use std::path::PathBuf;

use tauri::{
    image::Image, menu::{Menu, MenuBuilder, MenuItem}, tray::TrayIconBuilder, AppHandle, EventLoopMessage, Manager, WindowEvent,
    tray::TrayIconId
};

use tauri::async_runtime::spawn;
use tokio::time::{sleep, Duration};
use std::thread;
use std::sync::{Arc, Mutex};

use ncrs_core::{
    mount_ncfs,
    MountOptions,
    SyncState
};

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

fn rerender_tray_menu(app: AppHandle, sync_state: SyncState) -> Result<tauri::menu::Menu<tauri_runtime_wry::Wry<EventLoopMessage>>, tauri::Error> {
    // let tray = app.tray_by_id("main-tray").unwrap();
    
    // let menu_handle = app.menu().unwrap();
    // let pause_handle = menu_handle.get("pause").unwrap();
    // let pause_item = pause_handle.as_menuitem().unwrap();

    // Here you would implement the logic to update the tray icon and menu item text
    // based on the current sync state.
    // For example, if the sync is paused, change the icon and text accordingly.

    let pause_text = match sync_state {
        SyncState::Idle => "Pause Sync",
        SyncState::Paused => "Resume Sync",
        SyncState::Syncing => "Pause Sync",
        SyncState::Error(_) => "Sync Error",
    };

    let about_i = MenuItem::with_id(&app, "about", "Open main Dialog", true, None::<&str>)?;
    let pause_i = MenuItem::with_id(&app, "pause", pause_text, true, None::<&str>)?;
    let settings_i = MenuItem::with_id(&app, "settings", "Settings", true, None::<&str>)?;
    let quit_i = MenuItem::with_id(&app, "quit", "Exit Nextcloud", true, None::<&str>)?;

    return MenuBuilder::new(&app)
        .id("tray-menu")
        .item(&about_i)
        .item(&pause_i)
        .separator()
        .item(&settings_i)
        .item(&quit_i)
        .build();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let sync_state = SyncState::Idle;
    let sync_state_pointer = Arc::new(Mutex::new(sync_state));
    let sync_state_pointer_clone = sync_state_pointer.clone();
    let sync_state_pointer_clone2 = sync_state_pointer.clone();

    let tray_icon_id:Arc<Mutex<Option<TrayIconId>>> = Arc::new(Mutex::new(None));
    let tray_icon_id_clone = tray_icon_id.clone();
    let tray_icon_id_clone2 = tray_icon_id.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_positioner::init())
        .invoke_handler(tauri::generate_handler![greet])
        // move sync_state to the app state
        .setup(move |app| {
            // Spawn setup as a non-blocking task
            spawn(run_ncfs_client(app.handle().clone()));

            let menu: Menu<tauri_runtime_wry::Wry<EventLoopMessage>> = rerender_tray_menu(app.handle().clone(), sync_state_pointer_clone.lock().unwrap().clone())?;
            let icon = load_icon(
                concat!(env!("CARGO_MANIFEST_DIR"), "/icons/tray_icon.idle.png")
            );

            let tray_icon = TrayIconBuilder::new()
                .menu(&menu)
                .icon(icon)
                .show_menu_on_left_click(true)
                .on_tray_icon_event(|app, event| {
                    tauri_plugin_positioner::on_tray_event(app.app_handle(), &event);
                })
                .build(app)
                .unwrap();

            // Set the tray icon id to the app state
            tray_icon_id_clone.lock().unwrap().replace(tray_icon.id().clone());

            Ok(())
        })
        .on_window_event(|app, event| match event {
            WindowEvent::CloseRequested { api, .. } => {
                // Handle window close request
                let main_window = app.get_webview_window("main").unwrap();
                main_window.hide().unwrap();

                println!("Window close requested");
                api.prevent_close(); // Prevent the window from closing
                // app.exit(0);
            }
            WindowEvent::Destroyed => {
                // Handle window destruction
                println!("Window destroyed");
            }
            // Minimized:
            _ => {}
        })
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "about" => {                
                let main_window = app.get_webview_window("main").unwrap();
                main_window.show().unwrap();
            }
            "pause" => {
                let tray = app.tray_by_id(tray_icon_id_clone2.lock().unwrap().as_ref().unwrap()).unwrap();

                // Toggle state
                if *sync_state_pointer_clone2.lock().unwrap() == SyncState::Paused {
                    // Set sync state to Idle
                    *sync_state_pointer_clone2.lock().unwrap() = SyncState::Idle;
                } else {    
                    // Set sync state to Paused
                    *sync_state_pointer_clone2.lock().unwrap() = SyncState::Paused;
                }

                let new_icon:Option<Image> = match *sync_state_pointer_clone2.lock().unwrap() {
                    SyncState::Idle => Some(load_icon(
                        concat!(env!("CARGO_MANIFEST_DIR"), "/icons/tray_icon.idle.png")
                    )),
                    SyncState::Paused => Some(load_icon(
                        concat!(env!("CARGO_MANIFEST_DIR"), "/icons/tray_icon.paused.png")
                    )),
                    SyncState::Syncing => Some(load_icon(
                        concat!(env!("CARGO_MANIFEST_DIR"), "/icons/tray_icon.syncing.png")
                    )),
                    SyncState::Error(_) => Some(load_icon(
                        concat!(env!("CARGO_MANIFEST_DIR"), "/icons/tray_icon.error.png")
                    )),
                };

                // Set icon
                tray.set_icon(new_icon).unwrap();

                // Refresh menu
                let menu: Menu<tauri_runtime_wry::Wry<EventLoopMessage>> = rerender_tray_menu(app.app_handle().clone(), sync_state_pointer_clone2.lock().unwrap().clone()).unwrap();
                tray.set_menu(Some(menu)).unwrap();
            }
            "settings" => {
                println!("settings menu item was clicked");
                // Open main window, but go to url /settings
                let main_window = app.get_webview_window("main").unwrap();
                main_window.show().unwrap();
                // main_window.eval("window.location.href = '/settings';").unwrap();
            }
            "quit" => {
                println!("quit menu item was clicked");
                app.exit(0);
            }
            _ => {
                println!("menu item {:?} not handled", event.id);
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application")
}

fn load_icon(path:&'static str) -> Image<'static> {
    Image::from_path(std::path::Path::new(path))
        .expect("Failed to load icon image")
}

// An async function that does some heavy setup task
async fn run_ncfs_client(_app: AppHandle) -> Result<(), ()> {
    // Fake performing some heavy action for 3 seconds
    println!("Performing really heavy backend setup task...");
    sleep(Duration::from_secs(3)).await;
    println!("Backend setup task completed!");
    // Set the backend task as being completed
    // Commands can be ran as regular functions as long as you take
    // care of the input arguments yourself

    let user = "testlocaluser".to_string();

    thread::spawn(|| {
        mount_ncfs(MountOptions {
            url: "http://example.com/webdav".to_string(),
            username: Some("testuser".to_string()), password: Some("pass".to_string()),
            mount_point: PathBuf::from("/media/rgon/ncrsDesktop/".to_string()),
            log_user: user
        }).unwrap();
    
        println!("Mounted");
    }).join().unwrap();
    
    println!("Exited");

    // set_complete(
    //     app.clone(),
    //     app.state::<Mutex<SetupState>>(),
    //     "backend".to_string(),
    // )
    // .await?;

    Ok(())
}