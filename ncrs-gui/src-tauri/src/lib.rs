use std::path::PathBuf;

use tauri::{
    image::Image,
    menu::{Menu, MenuBuilder, MenuItem},
    tray::{TrayIconBuilder, TrayIconId},
    AppHandle, EventLoopMessage, Manager, WindowEvent, 
    PhysicalSize
};
use tauri_plugin_notification::NotificationExt;

use std::sync::{Arc, Mutex};
use std::thread;
use tauri::async_runtime::spawn;
use tokio::time::{sleep, Duration};

use ncrs_core::{mount_ncfs, MountOptions, SyncState};

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

#[tauri::command]
fn close_window(app: AppHandle) {
    // Get the main window by its label
    let main_window = app.get_webview_window("main").unwrap();
    // Hide the main window instead of closing it
    main_window.hide().unwrap();
    println!("Main window closed");
}

fn rerender_tray_menu(
    app: AppHandle,
    sync_state: SyncState,
) -> Result<tauri::menu::Menu<tauri_runtime_wry::Wry<EventLoopMessage>>, tauri::Error> {
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

#[derive(Debug, Clone, PartialEq)]
enum AppEntrypoint {
    Main,
    Settings,
}

fn open_main_window(app: &AppHandle, entrypoint:AppEntrypoint) {
    println!("Opening main window... Entry point: {:?}", entrypoint);

    // Open the main window
    let main_window = app.get_webview_window("main").unwrap();

    // Get monitor size
    let monitor = main_window.primary_monitor().unwrap();

    let size = if monitor.is_some() {
        monitor.unwrap().size().clone()
    } else {
        PhysicalSize::new(1860u32, 1000u32) // Default
    };

    println!("Monitor size: {:?}", size);

    // Set the size of the main window
    main_window.set_size(size).unwrap();

    // Also, tauri's implementation does not allow getting tray click events on wayland

    // Set the position to the top right corner of the screen
    // main_window.set_position(PhysicalPosition { 
    //     x: 1500,
    //     y: 20
    // }).unwrap();

    // Wayland does not support explicitly setting the window position, so we need to build a transparent, decorationless window
    // and position the 'virtual' window in the top right corner of the screen.

    // Show window
    main_window.show().unwrap();

    // Optionally, you can also focus the window
    main_window.set_focus().unwrap();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let sync_state = SyncState::Idle;
    let sync_state_pointer = Arc::new(Mutex::new(sync_state));
    let sync_state_pointer_clone = sync_state_pointer.clone();
    let sync_state_pointer_clone2 = sync_state_pointer.clone();

    let tray_icon_id: Arc<Mutex<Option<TrayIconId>>> = Arc::new(Mutex::new(None));
    let tray_icon_id_clone = tray_icon_id.clone();
    let tray_icon_id_clone2 = tray_icon_id.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            greet,
            close_window
        ])
        // move sync_state to the app state
        .setup(move |app| {
            // Spawn setup as a non-blocking task
            spawn(run_ncfs_client(app.handle().clone()));

            let menu: Menu<tauri_runtime_wry::Wry<EventLoopMessage>> = rerender_tray_menu(
                app.handle().clone(),
                sync_state_pointer_clone.lock().unwrap().clone(),
            )?;
            let icon = load_icon(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/icons/tray_icon.idle.png"
            ));

            let tray_icon = TrayIconBuilder::new()
                .menu(&menu)
                .icon(icon)
                .show_menu_on_left_click(true)
                .build(app)
                .unwrap();

            // Set the tray icon id to the app state
            tray_icon_id_clone
                .lock()
                .unwrap()
                .replace(tray_icon.id().clone());

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
                // let main_window = app.get_webview_window("main").unwrap();
                // main_window.show().unwrap();
                open_main_window(app, AppEntrypoint::Main);
            }
            "pause" => {
                let tray = app
                    .tray_by_id(tray_icon_id_clone2.lock().unwrap().as_ref().unwrap())
                    .unwrap();

                // Toggle state
                if *sync_state_pointer_clone2.lock().unwrap() == SyncState::Paused {
                    // Set sync state to Idle
                    *sync_state_pointer_clone2.lock().unwrap() = SyncState::Idle;
                } else {
                    // Set sync state to Paused
                    *sync_state_pointer_clone2.lock().unwrap() = SyncState::Paused;
                }

                let new_icon: Option<Image> = match *sync_state_pointer_clone2.lock().unwrap() {
                    SyncState::Idle => Some(load_icon(concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/icons/tray_icon.idle.png"
                    ))),
                    SyncState::Paused => Some(load_icon(concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/icons/tray_icon.paused.png"
                    ))),
                    SyncState::Syncing => Some(load_icon(concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/icons/tray_icon.syncing.png"
                    ))),
                    SyncState::Error(_) => Some(load_icon(concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/icons/tray_icon.error.png"
                    ))),
                };

                app.notification()
                    .builder()
                    .title("NCRS Sync Status")
                    .body(if  *sync_state_pointer_clone2.lock().unwrap() == SyncState::Paused {
                        "Sync paused"
                    } else {
                        "Sync resumed"
                    })
                    // * The sound resource name. Only available on mobile.
                    // .sound("default")
                    .show()
                    .unwrap();

                // Set icon
                tray.set_icon(new_icon).unwrap();

                // Refresh menu
                let menu: Menu<tauri_runtime_wry::Wry<EventLoopMessage>> = rerender_tray_menu(
                    app.app_handle().clone(),
                    sync_state_pointer_clone2.lock().unwrap().clone(),
                )
                .unwrap();
                tray.set_menu(Some(menu)).unwrap();
            }
            "settings" => {
                println!("settings menu item was clicked");
                // Open settings window or dialog
                open_main_window(app, AppEntrypoint::Settings);
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

fn load_icon(path: &'static str) -> Image<'static> {
    Image::from_path(std::path::Path::new(path)).expect("Failed to load icon image")
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
            username: Some("testuser".to_string()),
            password: Some("pass".to_string()),
            mount_point: PathBuf::from("/media/rgon/ncrsDesktop/".to_string()),
            log_user: user,
        })
        .unwrap();

        println!("Mounted");
    })
    .join()
    .unwrap();

    println!("Exited");

    // set_complete(
    //     app.clone(),
    //     app.state::<Mutex<SetupState>>(),
    //     "backend".to_string(),
    // )
    // .await?;

    Ok(())
}
