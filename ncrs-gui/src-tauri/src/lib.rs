use std::sync::{Arc, Mutex};
use std::thread;

use tauri::{
    image::Image,
    menu::{MenuBuilder, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent, TrayIconId},
    AppHandle, Emitter, EventLoopMessage, Listener, Manager, State,
};
use tauri_plugin_opener::OpenerExt;
use tauri::async_runtime::spawn;
use tauri_plugin_notification::NotificationExt;
use tokio::time::{sleep, Duration};

use ncrs_core::{mutation_journal::{self, SharedJournal, JournalEntry, ConflictRecord}, notifications::NcNotification, search::{SearchProvider, SearchResultGroup}, ipc::StorageStats, ErrorLog, MountOptions, SyncError, SyncState, TransferMap, TransferProgress};

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
    /// True when the daemon we're mirroring belongs to somebody else (found
    /// already running at attach time, e.g. the ncrs systemd service) rather
    /// than one we spawned ourselves — so quit/remount must leave its mount
    /// and process alone instead of tearing it down.
    pub attached: std::sync::atomic::AtomicBool,
    /// Set to true to cancel an in-progress login flow poll loop.
    pub login_flow_cancel: Arc<std::sync::atomic::AtomicBool>,
    /// Set when the GUI detects a server auth failure (e.g. 401 from notifications API).
    /// Survives attached_poll_loop state overwrites; cleared on successful auth.
    pub auth_error: Mutex<Option<String>>,
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
            auth_error: Mutex::new(None),
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

/// Resolve the `ncrs` daemon binary: prefer one next to our own executable
/// (dev builds, where both land in the same target/{debug,release} dir),
/// falling back to a bare `ncrs` resolved via PATH (the installed /usr/bin
/// case).
fn ncrs_binary_path() -> std::path::PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("ncrs");
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    std::path::PathBuf::from("ncrs")
}

/// Spawn the ncrs daemon, preferring a systemd-managed transient scope over a
/// bare child process — this gets it the same supervision the `ncrs.service`
/// path already has (its own cgroup, `systemctl --user status ncrs.scope`,
/// `journalctl --user` capturing its output instead of it being lost in
/// ncrs-gui's own stdout) without needing that unit pre-enabled.
///
/// `systemd-run --scope` execs directly into the target rather than forking a
/// wrapper that lingers, so the `Child` this returns on success — whichever
/// branch is taken — always *is* the ncrs process itself.
///
/// Falls back to a plain spawn if `systemd-run` is missing, or if the user
/// session bus is unreachable: `systemd-run` itself then exits almost
/// immediately (well before ncrs could open its IPC socket), which is
/// detectable, unlike a spawn() failure — the D-Bus round-trip happens after
/// the fork, so `Command::spawn()` alone cannot tell the two cases apart.
fn spawn_ncrs_daemon(ncrs_bin: &std::path::Path) -> std::io::Result<std::process::Child> {
    if let Ok(mut child) = std::process::Command::new("systemd-run")
        .args(["--user", "--scope", "--collect", "--unit=ncrs", "--"])
        .arg(ncrs_bin)
        .spawn()
    {
        thread::sleep(Duration::from_millis(300));
        match child.try_wait() {
            Ok(None) => return Ok(child), // still running — handed off cleanly
            _ => log::warn!("systemd-run exited immediately — falling back to a plain spawn"),
        }
    } else {
        log::info!("systemd-run unavailable — falling back to a plain spawn");
    }
    std::process::Command::new(ncrs_bin).spawn()
}

// ── Tauri commands ────────────────────────────────────────────────────────────

#[tauri::command]
fn close_window(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        // Destroy (not hide) so the WebKitGTK webview process is torn down and
        // stops burning idle CPU/memory while we sit in the tray. open_main_window
        // rebuilds it on demand; ExitRequested keeps the process alive meanwhile.
        let _ = w.destroy();
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
        SyncState::Error(ref e) => format!("error:{}", e),
        SyncState::Degraded(ref r) => format!("degraded:{}", r),
        SyncState::Offline => "offline".into(),
    }
}

// A plain `fusermount -u` fails with EBUSY while some process still has a
// file open under the mount (e.g. LibreOffice keeping a document open). This
// retries a few times — most such busy states (a lock file mid-release, a
// save that just finished) clear within a second or two — but deliberately
// never falls back to a lazy `-uz` detach: a lazy unmount frees the path
// immediately even though it's still in use, so a *new* path-based write from
// whatever still has it open (e.g. a save-as-temp-then-rename) lands straight
// on the real underlying directory instead of erroring, bypassing ncrs
// entirely. Returns true once a clean unmount succeeds, false if it's still
// busy after retrying — callers must not force it silently.
fn try_clean_unmount(mp: &str) -> bool {
    const ATTEMPTS: u32 = 5;
    for attempt in 0..ATTEMPTS {
        let clean = std::process::Command::new("fusermount")
            .args(["-u", mp])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if clean {
            return true;
        }
        if attempt + 1 < ATTEMPTS {
            log::info!("unmount {} busy, retrying ({}/{})", mp, attempt + 1, ATTEMPTS);
            thread::sleep(Duration::from_millis(400));
        }
    }
    log::warn!(
        "unmount {} still busy after {} attempts — leaving it mounted rather than force-detaching",
        mp, ATTEMPTS
    );
    false
}

#[tauri::command]
async fn remount(app: AppHandle, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    // If a FUSE mount is active, unmount it before restarting so a changed
    // mount path (or any other saved config change) takes effect cleanly.
    let needs_unmount = !state.attached.load(std::sync::atomic::Ordering::Relaxed)
        && !matches!(
            *state.sync_state.lock().unwrap(),
            SyncState::Unmounted | SyncState::Error(_) | SyncState::Wiped
        );

    if needs_unmount {
        let mp = state
            .mount_options
            .lock()
            .unwrap()
            .as_ref()
            .map(|o| o.mount_point.to_string_lossy().into_owned());

        if let Some(mp) = mp {
            log::info!("remount: unmounting {} before restart", mp);
            let mp2 = mp.clone();
            let clean = tokio::task::spawn_blocking(move || try_clean_unmount(&mp2))
                .await
                .unwrap_or(false);
            if !clean {
                let msg = format!(
                    "{} is still in use by another program — close whatever has a file open there and try again",
                    mp
                );
                *state.sync_state.lock().unwrap() = SyncState::Error(msg.clone());
                app.emit("sync-state-changed", format!("error:{}", msg)).ok();
                return Err(msg);
            }
            // Wait for the FUSE thread to register the unmount (up to 5 s).
            for _ in 0..50 {
                if matches!(
                    *state.sync_state.lock().unwrap(),
                    SyncState::Unmounted | SyncState::Error(_)
                ) {
                    break;
                }
                sleep(Duration::from_millis(100)).await;
            }
        }
    }

    *state.sync_state.lock().unwrap() = SyncState::Idle;
    state.paused.store(false, std::sync::atomic::Ordering::Relaxed);
    app.emit("sync-state-changed", "idle").ok();
    let s = (*state).clone();
    spawn(start_ncfs_daemon(app, s));
    Ok(())
}

#[tauri::command]
async fn logout(app: AppHandle, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    // Remove stored credentials so the next startup shows the login view.
    if let Ok(opts) = ncrs_core::config::load_config() {
        let base_url = ncrs_core::notifications::base_url(&opts.url);
        nc_gnome_integration::clear_credentials(&base_url);
        if let Some(user) = opts.username.as_deref() {
            if let Err(e) = ncrs_core::config::delete_password_from_keyring(user, &opts.url) {
                log::warn!("logout: keyring delete: {}", e);
            }
        }
    }
    // Clear in-memory state so remount won't reuse stale options.
    *state.mount_options.lock().unwrap() = None;
    *state.sync_state.lock().unwrap() = SyncState::Unmounted;
    *state.auth_error.lock().unwrap() = None;
    app.emit("auth-cleared", ()).ok();
    Ok(())
}

#[derive(serde::Serialize)]
pub struct UserInfo {
    pub username: String,
    pub server_url: String,
    pub mount_point: String,
    /// Kept for compatibility with the field name the frontend already binds;
    /// now a `data:` URI rather than a server URL, because the webview's CSP is
    /// `img-src 'self' data:` and cannot fetch from the server.
    pub avatar_url: String,
}

#[tauri::command]
async fn get_user_info(state: State<'_, Arc<AppState>>) -> Result<Option<UserInfo>, ()> {
    let Some((username, base, server_url, mount_point, creds)) = ({
        let opts = state.mount_options.lock().unwrap();
        opts.as_ref().map(|o| {
            let username = o.username.clone().unwrap_or_else(|| o.log_user.clone());
            (
                username,
                ncrs_core::notifications::base_url(&o.url),
                o.url.clone(),
                o.mount_point.to_string_lossy().into_owned(),
                o.credentials().ok(),
            )
        })
    }) else {
        return Ok(None);
    };

    // Fetched off-thread: it is one small HTTP request, cached in ncrs_core, but
    // it must not block the command thread. A failure just leaves the frontend
    // showing its initials placeholder.
    let avatar_url = match creds {
        Some(creds) => {
            let (b, u) = (base.clone(), username.clone());
            tokio::task::spawn_blocking(move || {
                ncrs_core::asset_url::avatar_data_uri(&b, &creds, &u, 64)
            })
            .await
            .ok()
            .flatten()
            .unwrap_or_default()
        }
        None => String::new(),
    };

    Ok(Some(UserInfo { avatar_url, username, server_url, mount_point }))
}

#[tauri::command]
async fn get_nc_theme(state: State<'_, Arc<AppState>>) -> Result<Option<ncrs_core::login_flow::ThemeColors>, ()> {
    let url = state.mount_options.lock().unwrap().as_ref().map(|o| o.url.clone());
    let Some(url) = url else { return Ok(None) };
    Ok(tokio::task::spawn_blocking(move || ncrs_core::login_flow::fetch_server_theme(&url))
        .await
        .unwrap_or(None))
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
        let http3 = ncrs_core::http3_effective(&opts);
        thread::spawn(move || {
            if let Err(e) =
                ncrs_core::notifications::dismiss_notification(&base, &creds, id, http3)
            {
                log::warn!("dismiss notification {}: {}", id, e);
            }
        });
    }
}

/// Schemes `open_link` will hand to the desktop's URL handler.
///
/// This command is reachable from the webview and is called with strings that
/// originate on the server (`resourceUrl` on a unified-search hit, the login
/// flow URL). `opener`'s Tauri scope does not apply here — that only gates the
/// plugin's own JS commands, not our Rust call — so the allowlist has to live
/// in this function. Without it a compromised server could get `file://`,
/// `smb://` or any registered handler opened by having the user click a search
/// result.
const OPENABLE_SCHEMES: &[&str] = &["http", "https", "mailto"];

#[tauri::command]
fn open_link(url: String, app: AppHandle) -> Result<(), String> {
    if !is_openable_url(&url) {
        log::warn!("open_link: refusing to open {:?}", url);
        return Err("That link cannot be opened.".into());
    }
    app.opener()
        .open_url(&url, None::<&str>)
        .map_err(|e| {
            log::warn!("open_link: {:?}: {}", url, e);
            "Could not open the link in a browser.".into()
        })
}

fn is_openable_url(url: &str) -> bool {
    match url::Url::parse(url) {
        Ok(u) => OPENABLE_SCHEMES.contains(&u.scheme()),
        Err(_) => false,
    }
}

#[tauri::command]
fn reveal_in_file_manager(path: String, app: AppHandle) -> Result<(), String> {
    // Checked before handing it over because the file manager's reaction to a
    // path that is not there is to do nothing at all, and the two ways to get
    // here with one — the mount is not up, or the file was removed on the
    // server since the search ran — are the likeliest failures, not the rarest.
    if !std::path::Path::new(&path).exists() {
        log::warn!("reveal_in_file_manager: {:?} is not present", path);
        return Err("That file is not in the mount — is your Nextcloud folder mounted?".into());
    }
    app.opener().reveal_item_in_dir(&path).map_err(|e| {
        log::warn!("reveal_in_file_manager: {:?}: {}", path, e);
        "Could not open the file manager.".into()
    })
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
fn dismiss_error(state: State<Arc<AppState>>, timestamp_ms: u64) {
    state.error_log.lock().unwrap().retain(|e| e.timestamp_ms != timestamp_ms);
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

// The daemon owns (and persists) the conflicts; this app only mirrors them over IPC, so
// resolving must reach the daemon or the next poll brings the conflict straight back.
#[tauri::command]
async fn resolve_conflict(state: State<'_, Arc<AppState>>, id: u64) -> Result<(), ()> {
    state.journal.lock().unwrap().resolve_conflict(id);
    let verb = format!("RESOLVE_CONFLICT {}", id);
    let _ = tokio::task::spawn_blocking(move || ipc_request(&[&verb])).await;
    Ok(())
}

#[tauri::command]
async fn clear_conflicts(state: State<'_, Arc<AppState>>) -> Result<(), ()> {
    state.journal.lock().unwrap().resolve_all_conflicts();
    let _ = tokio::task::spawn_blocking(|| ipc_request(&["RESOLVE_ALL_CONFLICTS"])).await;
    Ok(())
}

/// Drop every locally cached file copy (except files with a pending upload) so
/// reads re-download fresh content. Returns the number of files cleared.
#[tauri::command]
async fn purge_cache() -> Result<u64, String> {
    tokio::task::spawn_blocking(|| match ipc_request(&["PURGE_CACHE"]) {
        Some(replies) => {
            let r = replies.first().map(String::as_str).unwrap_or("");
            if let Some(n) = r.strip_prefix("ok:") {
                n.parse::<u64>().map_err(|_| format!("unexpected reply: {}", r))
            } else if let Some(e) = r.strip_prefix("error:") {
                Err(e.trim().to_string())
            } else {
                Err(format!("unexpected reply: {}", r))
            }
        }
        None => Err("daemon not reachable".to_string()),
    })
    .await
    .map_err(|e| e.to_string())?
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
    let http3 = ncrs_core::http3_effective(&opts);

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
    let http3 = ncrs_core::http3_effective(&opts);
    let mount_point = opts.mount_point.clone();

    let dav_url = opts.url.clone();
    let search_creds = creds.clone();

    let mut groups = tokio::task::spawn_blocking(move || {
        ncrs_core::search::search_filtered(&base, &search_creds, &term, http3, &provider_ids)
    })
    .await
    .map_err(|e| format!("search task failed: {}", e))??;

    // Not gated on `provider_id == "files"`: the URL shape is the real
    // discriminator, and it is not only the Files provider that returns file
    // hits — a `comments` hit links to the commented-on file with the very same
    // `/f/<fileid>`. A provider with neither form (calendar, contacts, settings)
    // yields nothing here and keeps its web link, which is what it wants.
    //
    // Nextcloud ≤ 27 put the containing directory in the URL; since 28 only the
    // file id is there and the path has to come from the server. Read the free
    // one first and collect the rest across every group, so the round trip is
    // one SEARCH for the whole result set rather than one per provider.
    let mut unresolved: Vec<(usize, usize, u64)> = Vec::new();
    for (gi, group) in groups.iter_mut().enumerate() {
        for (ei, entry) in group.entries.iter_mut().enumerate() {
            if let Some(dir) = extract_dir_param(&entry.resource_url) {
                let file_path = if dir == "/" {
                    format!("/{}", entry.title)
                } else {
                    format!("{}/{}", dir, entry.title)
                };
                entry.local_path = local_path_for(&mount_point, &file_path);
                if entry.local_path.is_some() {
                    continue;
                }
            }
            match extract_fileid(&entry.resource_url) {
                Some(id) => unresolved.push((gi, ei, id)),
                // Only worth reporting for the Files provider, where a hit that
                // yields no path means every file result now opens in a browser
                // instead of the file manager. Elsewhere it is just a hit that
                // was never a file.
                None if group.provider_id == "files" => log::warn!(
                    "search: no dir or fileid in {:?} — {:?} cannot be opened locally",
                    entry.resource_url,
                    entry.title,
                ),
                None => {}
            }
        }
    }

    if !unresolved.is_empty() {
        let ids: Vec<u64> = unresolved.iter().map(|(_, _, id)| *id).collect();
        let resolve_creds = creds.clone();
        let paths = tokio::task::spawn_blocking(move || {
            ncrs_core::search::resolve_fileid_paths(&dav_url, &resolve_creds, http3, &ids)
        })
        .await
        .map_err(|e| format!("fileid resolve task failed: {}", e))?;

        match paths {
            Ok(paths) => {
                for (gi, ei, id) in unresolved {
                    let entry = &mut groups[gi].entries[ei];
                    match paths.get(&id) {
                        Some(remote) => {
                            entry.local_path =
                                local_path_for(&mount_point, &remote.to_string_lossy());
                        }
                        // A hit that is not a file after all (a provider whose
                        // URL merely looked like one), or one the account can no
                        // longer see. Either way the web link still works.
                        None => log::debug!(
                            "search: fileid {} ({:?}) has no path on the server",
                            id,
                            entry.title,
                        ),
                    }
                }
            }
            Err(e) => log::warn!(
                "search: resolving file ids failed: {} — those hits open in the web UI",
                e,
            ),
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
    pub needs_setup: bool,
}

#[tauri::command]
fn get_config_status() -> ConfigStatus {
    match ncrs_core::config::load_config() {
        Ok(opts) => {
            if opts.credentials().is_err() {
                let server = ncrs_core::notifications::base_url(&opts.url);
                ConfigStatus { needs_login: true, server_url: Some(server), needs_setup: false }
            } else {
                let needs_setup = opts.mount_point.as_os_str().is_empty();
                ConfigStatus { needs_login: false, server_url: None, needs_setup }
            }
        }
        Err(_) => ConfigStatus { needs_login: true, server_url: None, needs_setup: false },
    }
}

#[tauri::command]
fn get_config_values() -> ncrs_core::config::ConfigSettings {
    match ncrs_core::config::load_config() {
        Ok(opts) => ncrs_core::config::config_settings_from_opts(&opts),
        Err(_) => ncrs_core::config::ConfigSettings::default(),
    }
}

#[tauri::command]
fn save_config_values(values: ncrs_core::config::ConfigSettings) -> Result<(), String> {
    ncrs_core::config::rewrite_config_settings(&values)
}

/// Toggle kernel FUSE_PASSTHROUGH live, without a remount. Persists the choice
/// (so it's also the default next time ncrs mounts) and, if a daemon is
/// currently running — attached or embedded in this process, the IPC socket
/// exists either way — forwards it immediately over IPC.
#[tauri::command]
fn set_passthrough_enabled(enabled: bool) -> Result<(), String> {
    let mut settings = ncrs_core::config::load_config()
        .map(|opts| ncrs_core::config::config_settings_from_opts(&opts))
        .unwrap_or_default();
    settings.fuse_passthrough = enabled;
    ncrs_core::config::rewrite_config_settings(&settings)?;

    let verb = if enabled { "PASSTHROUGH_ON" } else { "PASSTHROUGH_OFF" };
    if ipc_request(&[verb]).is_none() {
        log::warn!("could not forward {} to daemon (not running?)", verb);
    }
    Ok(())
}

/// "on"/"off" (the live toggle) and "capable"/"unavailable" (whether a
/// passthrough open has actually succeeded this session), joined with ':'.
/// None when no daemon is currently reachable over IPC.
#[tauri::command]
fn get_passthrough_status() -> Option<String> {
    ipc_request(&["PASSTHROUGH_STATUS"]).and_then(|r| r.into_iter().next())
}

/// One file-browser profile as reported by the service's `INTEGRATIONS` verb.
/// The service owns detection and every side effect; the GUI only lists
/// profiles and flips their mode.
#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
pub struct Integration {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub installed: bool,
    /// "auto" | "on" | "off"
    pub mode: String,
    pub enabled: bool,
    pub adapter_package: Option<String>,
    #[serde(default)]
    pub adapter_client_ids: Vec<String>,
    pub adapter_installed: bool,
    pub adapter_connected: bool,
}

/// List file-browser profiles. `Ok(None)` when the daemon predates the
/// `INTEGRATIONS` verb (it replies `unknown`), so the UI can ask for an update
/// instead of showing an error.
#[tauri::command]
async fn list_integrations() -> Result<Option<Vec<Integration>>, String> {
    tokio::task::spawn_blocking(|| match ipc_request(&["INTEGRATIONS"]) {
        Some(replies) => {
            let r = replies.first().map(String::as_str).unwrap_or("");
            if r == "unknown" {
                Ok(None)
            } else if let Some(e) = r.strip_prefix("error:") {
                Err(e.trim().to_string())
            } else {
                serde_json::from_str(r).map(Some).map_err(|e| format!("unexpected reply: {}", e))
            }
        }
        None => Err("daemon not reachable".to_string()),
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Set a profile's mode: "on"/"off" override detection, "auto" follows it.
#[tauri::command]
async fn set_integration(id: String, mode: String) -> Result<(), String> {
    if !matches!(mode.as_str(), "on" | "off" | "auto") {
        return Err(format!("invalid mode: {}", mode));
    }
    // The verb is whitespace-delimited; an id with spaces would be misparsed.
    if id.is_empty() || id.contains(char::is_whitespace) {
        return Err(format!("invalid profile id: {:?}", id));
    }
    tokio::task::spawn_blocking(move || {
        match ipc_request(&[&format!("INTEGRATION_SET {} {}", id, mode)]) {
            Some(replies) => {
                let r = replies.first().map(String::as_str).unwrap_or("");
                if r == "ok" {
                    Ok(())
                } else if r == "unknown" {
                    Err("Update the ncrs service to manage file browsers".to_string())
                } else if let Some(e) = r.strip_prefix("error:") {
                    Err(e.trim().to_string())
                } else {
                    Err(format!("unexpected reply: {}", r))
                }
            }
            None => Err("daemon not reachable".to_string()),
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn get_app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
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

                    if let Err(e) = ncrs_core::login_flow::validate_login_server(&creds.server, &server) {
                        log::error!("refusing login: {}", e);
                        app.emit("login-error", e).ok();
                        return;
                    }

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
    // Owner-only, and via the shared helper rather than a local `fs::write`:
    // this file names the server and account, and the previous version created
    // it world-readable and then only *warned* about it.
    ncrs_core::config::write_private(&config_path, config.as_bytes())
        .map_err(|e| format!("write config: {}", e))?;
    log::info!("config written to {}", config_path.display());
    Ok(())
}

/// A search hit's path inside the mount, in the string form the webview and
/// `reveal_in_file_manager` use. See [`ncrs_core::mount_local_path`] for why
/// server-supplied paths are constrained rather than joined directly.
fn local_path_for(mount_point: &std::path::Path, remote_path: &str) -> Option<String> {
    ncrs_core::mount_local_path(mount_point, remote_path)
        .map(|p| p.to_string_lossy().into_owned())
}

/// The file id in a Nextcloud ≥ 28 file hit (`…/index.php/f/1234`), or in the
/// `?fileid=` form the Files app also emits.
///
/// The `f`/`files` segment is required: without it any hit whose URL happens to
/// end in a number would be read as a file id and resolved against the server.
fn extract_fileid(url: &str) -> Option<u64> {
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (url, None),
    };
    if let Some(query) = query {
        if let Some(id) = query
            .split('&')
            .find_map(|pair| pair.strip_prefix("fileid="))
            .and_then(|v| v.parse().ok())
        {
            return Some(id);
        }
    }
    let mut segments = path.trim_end_matches('/').rsplit('/');
    let last = segments.next()?;
    match segments.next()? {
        "f" | "files" => last.parse().ok(),
        _ => None,
    }
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
            SyncState::Idle | SyncState::Degraded(_) | SyncState::Offline => "Pause Sync",
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
    Warning,
}

impl TrayIcon {
    fn for_state(state: &SyncState) -> Self {
        match state {
            SyncState::Idle => TrayIcon::Idle,
            SyncState::Paused | SyncState::Unmounted | SyncState::Wiped => TrayIcon::Paused,
            SyncState::Syncing => TrayIcon::Syncing,
            // A total outage is a failure, not a warning: WebDAV, notify_push and
            // every queued upload are down together.
            SyncState::Error(_) | SyncState::Offline => TrayIcon::Error,
            SyncState::Degraded(_) => TrayIcon::Warning,
        }
    }
}

fn load_icon(kind: TrayIcon) -> Image<'static> {
    static ICONS: [std::sync::OnceLock<Image<'static>>; 5] = [const { std::sync::OnceLock::new() }; 5];
    let (slot, bytes): (usize, &'static [u8]) = match kind {
        TrayIcon::Idle => (0, include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/icons/tray_icon.idle.png"))),
        TrayIcon::Paused => (1, include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/icons/tray_icon.paused.png"))),
        TrayIcon::Syncing => (2, include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/icons/tray_icon.syncing.png"))),
        TrayIcon::Error => (3, include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/icons/tray_icon.error.png"))),
        TrayIcon::Warning => (4, include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/icons/tray_icon.warning.png"))),
    };
    ICONS[slot]
        .get_or_init(|| Image::from_bytes(bytes).expect("embedded tray icon is valid PNG"))
        .clone()
}

fn open_main_window(app: &AppHandle) {
    ncrs_plugin::open_main_window(app);
}

// ── Main entry point ──────────────────────────────────────────────────────────


// ── Shutdown and second-launch diagnostics ────────────────────────────────────

/// Set by the SIGTERM/SIGINT handler; polled by the shutdown watcher thread.
static TERMINATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Async-signal-safe: stores a flag and nothing else.
extern "C" fn on_terminate(_sig: libc::c_int) {
    TERMINATE.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Unmounts on SIGTERM/SIGINT instead of dying with the mount still attached.
///
/// Without this, `systemctl --user stop`, a logout, or a plain `kill` left
/// `~/Nextcloud` as a dead (ENOTCONN) mount until the next start — recoverable,
/// since `prepare_mount_point` detaches a stale mount when it comes back up, but
/// until then every `ls` in the user's home tree trips over it. The tray's Exit
/// item already unmounts cleanly; this routes signals through the same path.
fn install_shutdown_handlers(state: Arc<AppState>) {
    unsafe {
        libc::signal(libc::SIGTERM, on_terminate as *const () as usize);
        libc::signal(libc::SIGINT, on_terminate as *const () as usize);
    }
    thread::spawn(move || {
        while !TERMINATE.load(std::sync::atomic::Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(200));
        }
        log::info!("received termination signal — unmounting before exit");
        graceful_unmount(&state);
        std::process::exit(0);
    });
}

/// The tray-quit unmount, reused for signal shutdown.
fn graceful_unmount(state: &AppState) {
    // Attach mode: the mount belongs to an external daemon, so it is not ours
    // to tear down.
    if state.attached.load(std::sync::atomic::Ordering::Relaxed) {
        log::info!("shutdown: leaving external daemon's mount untouched");
        return;
    }
    let mount_point = state
        .mount_options
        .lock()
        .ok()
        .and_then(|o| o.as_ref().map(|o| o.mount_point.to_string_lossy().to_string()));
    if let Some(mp) = mount_point {
        log::info!("shutdown: unmounting {}", mp);
        // Deliberately not a lazy detach — same reasoning as try_clean_unmount:
        // a busy mount is better left dying with the process (the next start
        // detaches it) than freed while still in use.
        if try_clean_unmount(&mp) {
            let _ = std::fs::remove_dir(&mp);
        }
    }
}

/// Whether another ncRS instance is already serving the IPC socket.
///
/// `tauri-plugin-single-instance` handles the handoff by calling
/// `std::process::exit(0)` in the *second* process — correct, but it prints
/// nothing, so a launch that appears to do nothing at all is impossible to
/// diagnose from a terminal. Detect the same condition ourselves first, purely
/// so we can say what happened.
fn running_instance_socket() -> Option<String> {
    let sock = ncrs_core::ipc::socket_path();
    // Existence alone is not enough: a killed instance can leave the path
    // behind. Only a socket that still accepts a connection means "running".
    match std::os::unix::net::UnixStream::connect(&sock) {
        Ok(_) => Some(sock.to_string_lossy().into_owned()),
        Err(_) => None,
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Say something before the single-instance plugin exits this process, so a
    // second launch is not a silent no-op.
    if let Some(sock) = running_instance_socket() {
        eprintln!(
            "ncRS is already running (IPC socket {sock}) — focusing the existing \
             window instead of starting a second daemon."
        );
    }

    // Logging is handled by tauri-plugin-log (see plugin registration below).
    let app_state = Arc::new(AppState::default());
    install_shutdown_handlers(app_state.clone());
    let app_state_setup = app_state.clone();
    let app_state_menu = app_state.clone();

    let tray_icon_id: Arc<Mutex<Option<TrayIconId>>> = Arc::new(Mutex::new(None));
    let tray_id_setup = tray_icon_id.clone();
    let tray_id_menu = tray_icon_id.clone();

    tauri::Builder::default()
        // Serves the built-in "interface failed to load" page. Registered here
        // rather than shipped in the frontend bundle because the page has to be
        // reachable precisely when the frontend is not.
        .register_uri_scheme_protocol(ncrs_plugin::ERROR_SCHEME, |_ctx, _req| {
            tauri::http::Response::builder()
                .header("Content-Type", "text/html; charset=utf-8")
                .body(ncrs_plugin::load_error_page().into_bytes())
                .unwrap_or_else(|_| {
                    tauri::http::Response::new(b"interface failed to load".to_vec())
                })
        })
        // Must be the first plugin: a second launch (e.g. the app-menu entry
        // while the autostarted instance runs) would spawn a second daemon
        // that steals the FUSE mount — surface the existing window instead.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            open_main_window(app);
        }))
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .build(),
        )
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        // The overlay is sized before it is shown, but on Wayland the monitor
        // (and its scale) is only knowable once the surface maps — re-fit when
        // the compositor reports the real scale factor.
        .on_window_event(|window, event| {
            if matches!(event, tauri::WindowEvent::ScaleFactorChanged { .. })
                && window.label() == "main"
            {
                if let Some(w) = window.app_handle().get_webview_window("main") {
                    ncrs_plugin::fit_overlay_to_monitor(&w);
                }
            }
        })
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            close_window,
            get_config_status,
            start_login_flow,
            get_sync_state,
            get_user_info,
            get_nc_theme,
            open_mount_folder,
            get_notifications,
            dismiss_notification,
            open_link,
            reveal_in_file_manager,
            get_errors,
            clear_errors,
            dismiss_error,
            get_transfers,
            get_pending_mutations,
            get_conflicts,
            resolve_conflict,
            clear_conflicts,
            purge_cache,
            fetch_search_providers,
            search_nextcloud,
            get_storage_stats,
            get_plugin_metas,
            remount,
            logout,
            get_config_values,
            save_config_values,
            set_passthrough_enabled,
            get_passthrough_status,
            list_integrations,
            set_integration,
            get_app_version,
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
            nc_gnome_integration::setup(app.handle());
            log::info!("plugin setup done: nc_gnome_integration");
            let state_listener = app_state_setup.clone();
            spawn(start_ncfs_daemon(app.handle().clone(), app_state_setup));

            let initial_state = SyncState::Idle;
            let menu = rerender_tray_menu(app.handle(), &initial_state)?;
            let icon = load_icon(TrayIcon::Idle);

            let tray = TrayIconBuilder::new()
                .menu(&menu)
                .icon(icon)
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                        open_main_window(tray.app_handle());
                    }
                })
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
                    "wiped" | "offline" => TrayIcon::Error,
                    "unmounted" | "paused" => TrayIcon::Paused,
                    "syncing" => TrayIcon::Syncing,
                    p if p.starts_with("degraded:") => TrayIcon::Warning,
                    _ => TrayIcon::Idle,
                };
                let _ = tray.set_icon(Some(load_icon(icon_kind)));
                if let Ok(menu) = rerender_tray_menu(&app_handle_listener, &ss) {
                    let _ = tray.set_menu(Some(menu));
                }
            });

            Ok(())
        })
        .on_window_event(|_app, _event| {
            // Let the window close normally when dismissed: closing destroys the
            // WebKitGTK webview and frees its idle CPU/memory. The ExitRequested
            // handler in `run` keeps the tray-only process alive with no windows,
            // and open_main_window rebuilds the webview when reopened.
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

                // The daemon always runs out-of-process now (spawned by us or
                // owned externally) — forward the toggle over IPC either way;
                // the subscribe/poll loop mirrors the confirmed state back.
                {
                    let verb = if new_state == SyncState::Paused { "PAUSE" } else { "RESUME" };
                    thread::spawn(move || {
                        if ipc_request(&[verb]).is_none() {
                            log::warn!("could not forward {} to ncrs daemon", verb);
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
                    // Never fall back to a lazy detach: the process is about
                    // to exit either way, and a still-busy mount left in
                    // place just dies with the process into a dead (ENOTCONN)
                    // mount — prepare_mount_point already detaches that
                    // safely on the next start. A lazy unmount instead frees
                    // the path immediately while still in use, so a stray
                    // path-based write from whatever's still open lands
                    // straight on the real underlying directory.
                    if try_clean_unmount(&mp) {
                        // Best-effort: refuses non-empty or still-mounted dirs.
                        let _ = std::fs::remove_dir(&mp);
                    }
                }
                app.exit(0);
            }
            other => { plugins::handle_plugin_tray_event(app, other); }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, event| {
            // Closing the window to the tray destroys the last webview, which
            // would otherwise exit the app and kill the tray icon. Keep the
            // process alive on a window-driven exit (code == None); a real quit
            // comes from the tray "Exit" item via app.exit() (code == Some) and
            // is allowed through.
            if let tauri::RunEvent::ExitRequested { code, api, .. } = event {
                if code.is_none() {
                    api.prevent_exit();
                }
            }
        });
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
    app.emit("mount-ready", ()).ok();

    let base_url = ncrs_core::notifications::base_url(&opts.url);
    let user = opts.username.clone().unwrap_or_default();
    let pass = opts.password.clone().unwrap_or_default();
    nc_passwords::set_credentials(&app, &base_url, &user, &pass);
    nc_gnome_integration::set_credentials(&base_url, &user, &pass);

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    state.attached.store(false, std::sync::atomic::Ordering::Relaxed);
    let external_daemon = tokio::task::spawn_blocking(|| ipc_request(&["STATE"]))
        .await
        .ok()
        .flatten()
        .is_some();

    if external_daemon {
        // Another daemon (e.g. the ncrs systemd user service) already owns
        // the mount — mark it as not ours to tear down, then attach like any
        // other daemon below.
        log::info!("existing ncrs daemon detected — attaching via IPC");
        state.attached.store(true, std::sync::atomic::Ordering::Relaxed);
    } else {
        // No daemon owns the mount yet — spawn one and attach to it the same
        // way. The FUSE loop never runs in this process: only the small,
        // single-purpose `ncrs` binary is granted CAP_SYS_ADMIN (see
        // packaging/maintainer-scripts/postinst), so spawning it here — rather
        // than calling mount_ncfs() in-process — is what makes kernel
        // FUSE_PASSTHROUGH reachable from the GUI's default autostart path at
        // all, and keeps that capability out of this much larger process.
        let ncrs_bin = ncrs_binary_path();
        log::info!("no ncrs daemon found — spawning {}", ncrs_bin.display());
        match tokio::task::spawn_blocking(move || spawn_ncrs_daemon(&ncrs_bin))
            .await
            .unwrap_or_else(|e| Err(std::io::Error::other(e)))
        {
            Ok(mut child) => {
                // Purely bookkeeping so the child never lingers as a zombie;
                // attached_subscribe_loop below is what detects the daemon
                // going away and drives sync_state.
                thread::spawn(move || match child.wait() {
                    Ok(status) => log::info!("ncrs daemon exited: {}", status),
                    Err(e) => log::warn!("ncrs daemon wait() failed: {}", e),
                });
            }
            Err(e) => {
                let msg = format!("failed to start ncrs: {}", e);
                log::error!("{}", msg);
                *state.sync_state.lock().unwrap() = SyncState::Error(msg.clone());
                app.emit("sync-state-changed", format!("error:{}", msg)).ok();
                let _ = shutdown_tx.send(true);
                return Ok(());
            }
        }
        // Give the daemon a moment to open its IPC socket before attaching —
        // attached_subscribe_loop already retries/backs off on its own, but a
        // short poll here avoids logging spurious "disconnected" noise on the
        // very first attempt.
        for _ in 0..50 {
            if ipc_request(&["STATE"]).is_some() {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
    }

    spawn(attached_subscribe_loop(app.clone(), state.clone(), shutdown_tx));

    // Notification polling — async on Tokio runtime, no dedicated OS thread
    match opts.credentials() {
        Err(e) => log::warn!("notification polling disabled: credentials unavailable: {}", e),
        Ok(mut poll_creds) => {
            let poll_state = state.clone();
            let poll_app = app.clone();
            let poll_url = opts.url.clone();
            let poll_opts = opts.clone();
            let notif_shutdown = shutdown_rx.clone();
            log::info!("notification polling starting for {}", ncrs_core::notifications::base_url(&poll_url));
            spawn(async move {
                let base = ncrs_core::notifications::base_url(&poll_url);
                // Ids already on screen; None until the first fetch sets the
                // baseline, so startup doesn't pop every unread notification.
                let mut seen: Option<std::collections::HashSet<u64>> = None;
                loop {
                    if *notif_shutdown.borrow() { break; }
                    let b = base.clone();
                    let c = poll_creds.clone();
                    // Re-read per poll rather than captured once: this loop
                    // starts before the mount-time probe has decided, so a
                    // demotion would otherwise never reach it this session.
                    let poll_http3 = ncrs_core::http3_effective(&poll_opts);
                    match tokio::task::spawn_blocking(move || {
                        ncrs_core::notifications::fetch_notifications(&b, &c, poll_http3)
                    }).await {
                        Ok(Ok(notifs)) => {
                            log::info!("notifications fetched: {} item(s)", notifs.len());
                            *poll_state.auth_error.lock().unwrap() = None;
                            // Pop new ones from here, not the webview: closing the
                            // window destroys it, and the tray process polls alone.
                            if let Some(seen) = &seen {
                                notify_new_notifications(&poll_app, &notifs, seen);
                            }
                            seen = Some(notifs.iter().map(|n| n.notification_id).collect());
                            *poll_state.notifications.lock().unwrap() = notifs.clone();
                            poll_app.emit("notifications-updated", notifs).ok();
                        }
                        Ok(Err(e)) if e.contains("401") || e.contains("Unauthorized") || e.contains("not logged in") => {
                            // Reload credentials from keyring — the password may have been
                            // updated by a new login without the polling loop restarting.
                            match tokio::task::spawn_blocking(ncrs_core::config::load_config).await {
                                Ok(Ok(fresh)) => match fresh.credentials() {
                                    Ok(fresh_creds) if fresh_creds.secret() != poll_creds.secret() => {
                                        log::info!("notification poll: reloaded credentials after 401");
                                        poll_creds = fresh_creds;
                                        // Don't surface as error yet — retry next cycle with fresh creds.
                                    }
                                    _ => {
                                        log::warn!("fetch notifications: {}", e);
                                        *poll_state.auth_error.lock().unwrap() = Some(e.clone());
                                        poll_app.emit("sync-state-changed", "error:authentication failed — re-login required").ok();
                                    }
                                },
                                _ => {
                                    log::warn!("fetch notifications: {}", e);
                                    *poll_state.auth_error.lock().unwrap() = Some(e.clone());
                                    poll_app.emit("sync-state-changed", "error:authentication failed — re-login required").ok();
                                }
                            }
                        }
                        Ok(Err(e)) => log::warn!("fetch notifications: {}", e),
                        Err(e) => log::warn!("notification poll panicked: {}", e),
                    }
                    sleep(Duration::from_secs(30)).await;
                }
            });
        }
    }

    Ok(())
}

/// Raise a desktop notification for each Nextcloud notification not in `seen`,
/// collapsing a burst into a single summary.
fn notify_new_notifications(app: &AppHandle, notifs: &[NcNotification], seen: &std::collections::HashSet<u64>) {
    let fresh: Vec<&NcNotification> = notifs.iter().filter(|n| !seen.contains(&n.notification_id)).collect();
    if fresh.len() > 3 {
        app.notification()
            .builder()
            .title("Nextcloud")
            .body(format!("{} new notifications", fresh.len()))
            .show()
            .ok();
        return;
    }
    for n in fresh {
        app.notification()
            .builder()
            .title(&n.subject)
            .body(&n.message)
            .show()
            .ok();
    }
}

/// Outcome of a single [`run_subscription`] attempt.
enum SubscribeOutcome {
    /// The daemon does not understand SUBSCRIBE (older build) — poll instead.
    Unsupported,
    /// The subscription ended. `was_connected` is true if the handshake ever
    /// completed (a live daemon that then went away — likely a restart); false
    /// if we could not even connect (daemon absent).
    Disconnected { was_connected: bool },
}

/// Open one long-lived push subscription to the daemon and apply every pushed
/// snapshot until the connection drops. Blocking: runs on a spawn_blocking
/// thread that parks on the socket read while idle, so an idle attached GUI
/// burns no CPU (this is what replaces the old 2s poll cadence).
fn run_subscription(app: AppHandle, state: Arc<AppState>) -> SubscribeOutcome {
    use std::io::{BufRead, BufReader, Write};
    let sock = ncrs_core::ipc::socket_path();
    let stream = match std::os::unix::net::UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(_) => return SubscribeOutcome::Disconnected { was_connected: false },
    };
    // Just past twice the daemon's 20s keepalive, so a silent socket (dead
    // daemon) is detected without tripping on a normal idle gap.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(45)));
    let mut write_half = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return SubscribeOutcome::Disconnected { was_connected: false },
    };
    if writeln!(write_half, "SUBSCRIBE").is_err() {
        return SubscribeOutcome::Disconnected { was_connected: false };
    }
    let mut reader = BufReader::new(stream);
    let mut handshake = String::new();
    if reader.read_line(&mut handshake).unwrap_or(0) == 0 {
        return SubscribeOutcome::Disconnected { was_connected: false };
    }
    if handshake.trim() != "SUBSCRIBED" {
        // Old daemon replied "unknown" (or something else) — fall back to polling.
        return SubscribeOutcome::Unsupported;
    }
    log::info!("attached to external daemon via push subscription");

    // Baseline so only newly-appeared errors raise a desktop notification, and
    // each field's last raw JSON so an unchanged field is neither re-parsed nor
    // re-emitted (a snapshot arrives whenever *any* field changes).
    let mut cache = SnapshotCache {
        error_count: state.error_log.lock().unwrap().len(),
        ..SnapshotCache::default()
    };

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return SubscribeOutcome::Disconnected { was_connected: true }, // EOF
            Ok(_) => {}
            Err(_) => return SubscribeOutcome::Disconnected { was_connected: true }, // timeout/error
        }
        let line = line.trim_end();
        if line == "PING" || line.is_empty() {
            continue;
        }
        if let Some(payload) = line.strip_prefix("SNAP\t") {
            apply_snapshot(&app, &state, payload, &mut cache);
        }
    }
}

/// Last-seen per-field snapshot state, so [`apply_snapshot`] only re-parses and
/// re-emits a field whose raw JSON actually changed.
#[derive(Default)]
struct SnapshotCache {
    error_count: usize,
    errors_json: String,
    transfers_json: String,
    journal_json: String,
    conflicts_json: String,
}

/// Map a daemon STATE word to a [`SyncState`] and store it, preserving a
/// GUI-detected auth error against a daemon "idle" (the daemon can serve from
/// cache and look healthy while creds are invalid). Returns the mapped state
/// when the stored state actually changed, so the caller emits sync-state-changed.
/// Shared by the push path ([`apply_snapshot`]) and the legacy [`attached_poll_loop`].
fn apply_state_word(state: &Arc<AppState>, word: &str) -> Option<SyncState> {
    let new_state = match word {
        "paused" => SyncState::Paused,
        "syncing" => SyncState::Syncing,
        "unmounted" => SyncState::Unmounted,
        "wiped" => SyncState::Wiped,
        s if s.starts_with("error:") => SyncState::Error(s["error:".len()..].to_string()),
        s if s.starts_with("degraded:") => SyncState::Degraded(s["degraded:".len()..].to_string()),
        "offline" => SyncState::Offline,
        _ => SyncState::Idle,
    };
    let mut ss = state.sync_state.lock().unwrap();
    let auth_err = state.auth_error.lock().unwrap().is_some();
    // A revoked token trips the daemon's own connectivity probe (its 401 calls
    // mark_offline), so the daemon reports "offline" for a server that is up and
    // answering. Relabelling an auth failure as "server unreachable" is both
    // wrong and a dead end — it hides the Log in button. Hold whatever the GUI
    // already knows instead; the auth error it emitted stays on screen.
    let hold = if auth_err && matches!(new_state, SyncState::Offline) {
        true
    } else {
        // Long-standing case: the daemon serves from cache and looks healthy
        // while the credentials are invalid.
        auth_err && matches!(*ss, SyncState::Error(_)) && matches!(new_state, SyncState::Idle)
    };
    let effective = if hold { ss.clone() } else { new_state.clone() };
    if *ss != effective {
        *ss = effective;
        Some(new_state)
    } else {
        None
    }
}

/// Apply one pushed snapshot — `<state>\x1e<errors>\x1e<transfers>\x1e<journal>\x1e<conflicts>` —
/// to shared state and emit the frontend events. Push-driven equivalent of the
/// old attach poll plus the error/transfer/journal pollers, minus the timers.
fn apply_snapshot(app: &AppHandle, state: &Arc<AppState>, payload: &str, cache: &mut SnapshotCache) {
    let parts: Vec<&str> = payload.split('\x1e').collect();

    // 0: daemon sync state (change-gated inside apply_state_word).
    if let Some(s) = parts.first() {
        if let Some(new_state) = apply_state_word(state, s) {
            app.emit("sync-state-changed", new_state.to_string()).ok();
        }
    }

    // 1: errors — only when the list changed. Desktop-notify newly-added ones.
    if let Some(json) = parts.get(1) {
        if *json != cache.errors_json {
            if let Ok(errors) = serde_json::from_str::<Vec<SyncError>>(json) {
                let count = errors.len();
                if count > cache.error_count {
                    for err in errors.iter().skip(cache.error_count) {
                        app.notification()
                            .builder()
                            .title("ncRS: Sync Error")
                            .body(format!("{}: {}", err.path.display(), err.message))
                            .show()
                            .ok();
                    }
                }
                {
                    let mut log = state.error_log.lock().unwrap();
                    log.clear();
                    log.extend(errors.iter().cloned());
                }
                app.emit("sync-errors-updated", &errors).ok();
                cache.error_count = count;
                cache.errors_json = json.to_string();
            }
        }
    }

    // 2: transfers — only when changed.
    if let Some(json) = parts.get(2) {
        if *json != cache.transfers_json {
            if let Ok(transfers) = serde_json::from_str::<Vec<TransferProgress>>(json) {
                {
                    let mut map = state.transfer_map.lock().unwrap();
                    map.clear();
                    map.extend(transfers.iter().cloned().map(|t| (t.path.clone(), t)));
                }
                app.emit("transfers-updated", &transfers).ok();
                cache.transfers_json = json.to_string();
            }
        }
    }

    // 3 + 4: journal and conflicts. replace_from_remote needs both, so re-parse
    // when either changed, then emit only the event whose field actually moved.
    let journal_raw = parts.get(3).copied().unwrap_or("[]");
    let conflicts_raw = parts.get(4).copied().unwrap_or("[]");
    let jchanged = journal_raw != cache.journal_json;
    let cchanged = conflicts_raw != cache.conflicts_json;
    if jchanged || cchanged {
        if let Ok(entries) = serde_json::from_str::<Vec<JournalEntry>>(journal_raw) {
            let conflicts: Vec<ConflictRecord> =
                serde_json::from_str(conflicts_raw).unwrap_or_default();
            let pending = entries.len();
            state
                .journal
                .lock()
                .unwrap()
                .replace_from_remote(entries, conflicts.clone());
            if jchanged {
                app.emit("journal-updated", pending).ok();
                cache.journal_json = journal_raw.to_string();
            }
            if cchanged {
                app.emit("conflicts-updated", &conflicts).ok();
                cache.conflicts_json = conflicts_raw.to_string();
            }
        }
    }
}

/// Attach-mode driver: keep a push subscription open, reconnecting across daemon
/// restarts, and detach (Unmounted) once the daemon is gone for good. Falls back
/// to [`attached_poll_loop`] for a daemon too old to support SUBSCRIBE.
async fn attached_subscribe_loop(
    app: AppHandle,
    state: Arc<AppState>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
) {
    let mut failures = 0u32;
    loop {
        let outcome = tokio::task::spawn_blocking({
            let app = app.clone();
            let state = state.clone();
            move || run_subscription(app, state)
        })
        .await
        .unwrap_or(SubscribeOutcome::Disconnected { was_connected: false });

        match outcome {
            SubscribeOutcome::Unsupported => {
                log::info!("daemon lacks SUBSCRIBE — using legacy poll loop");
                attached_poll_loop(app, state, shutdown_tx).await;
                return;
            }
            SubscribeOutcome::Disconnected { was_connected } => {
                // A subscription that actually ran means the daemon was alive;
                // treat the drop as a transient restart and reset the strike count.
                if was_connected {
                    failures = 0;
                }
                failures += 1;
                // ~6s of failed reconnects: the daemon is gone. Show Unmounted;
                // the Remount menu re-probes and re-attaches or mounts embedded.
                if failures >= 3 {
                    log::warn!("external ncrs daemon stopped responding — detaching");
                    state.attached.store(false, std::sync::atomic::Ordering::Relaxed);
                    *state.sync_state.lock().unwrap() = SyncState::Unmounted;
                    app.emit("sync-state-changed", "unmounted").ok();
                    let _ = shutdown_tx.send(true);
                    return;
                }
                sleep(Duration::from_secs(2)).await;
            }
        }
    }
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

        let word = replies.first().map(String::as_str).unwrap_or("");
        if let Some(new_state) = apply_state_word(&state, word) {
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

#[cfg(test)]
mod open_link_tests {
    use super::is_openable_url;

    #[test]
    fn allows_web_and_mail_urls() {
        assert!(is_openable_url("https://cloud.example.com/apps/files"));
        assert!(is_openable_url("http://127.0.0.1:18087/index.php"));
        assert!(is_openable_url("mailto:someone@example.com"));
    }

    #[test]
    fn refuses_everything_else() {
        for hostile in [
            "file:///etc/passwd",
            "file:///home/victim/.config/autostart/x.desktop",
            "smb://attacker.example/share",
            "javascript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "",
            "not a url",
            "/etc/passwd",
        ] {
            assert!(!is_openable_url(hostile), "{} must be refused", hostile);
        }
    }
}

#[cfg(test)]
mod search_path_tests {
    use super::{extract_dir_param, extract_fileid, local_path_for};
    use std::path::Path;

    #[test]
    fn reads_the_dir_parameter_of_a_legacy_hit() {
        let url = "https://cloud.example.com/index.php/apps/files/?dir=/Deemix%20Downloads&scrollto=x.flac";
        assert_eq!(extract_dir_param(url).as_deref(), Some("/Deemix Downloads"));
    }

    #[test]
    fn reads_the_fileid_of_a_modern_hit() {
        // What Nextcloud ≥ 28 returns for a files hit — no dir anywhere.
        let url = "https://cloud.example.com/index.php/f/123456";
        assert_eq!(extract_dir_param(url), None);
        assert_eq!(extract_fileid(url), Some(123456));

        assert_eq!(extract_fileid("https://cloud.example.com/apps/files/files/42"), Some(42));
        assert_eq!(extract_fileid("https://cloud.example.com/index.php/f/42/"), Some(42));
        assert_eq!(
            extract_fileid("https://cloud.example.com/index.php/apps/files/?fileid=42&dir=/x"),
            Some(42),
        );
    }

    #[test]
    fn refuses_a_trailing_number_that_is_not_a_fileid() {
        for url in [
            "https://cloud.example.com/index.php/apps/deck/board/7",
            "https://cloud.example.com/index.php/f/not-a-number",
            "https://cloud.example.com/",
            "",
        ] {
            assert_eq!(extract_fileid(url), None, "{} must not yield a fileid", url);
        }
    }

    #[test]
    fn a_non_file_providers_hit_yields_no_path_at_all() {
        // What makes it safe to run the resolution over every provider rather
        // than only `files`: a hit that is not a file matches neither form, so
        // it keeps its web link and costs no lookup.
        for url in [
            "https://cloud.example.com/index.php/apps/calendar/dayGridMonth/now",
            "https://cloud.example.com/index.php/apps/contacts/All%20contacts/x~contacts",
            "https://cloud.example.com/index.php/call/abc123",
            "https://cloud.example.com/index.php/settings/user/security",
            "https://bookmarked.example.org/some/article",
        ] {
            assert_eq!(extract_fileid(url), None, "{} must not yield a fileid", url);
            assert_eq!(extract_dir_param(url), None, "{} must not yield a dir", url);
        }
    }

    #[test]
    fn joins_a_server_path_onto_the_mount() {
        let mount = Path::new("/home/u/Nextcloud");
        assert_eq!(
            local_path_for(mount, "/Deemix Downloads/a.flac").as_deref(),
            Some("/home/u/Nextcloud/Deemix Downloads/a.flac"),
        );
        assert_eq!(
            local_path_for(mount, "top.txt").as_deref(),
            Some("/home/u/Nextcloud/top.txt"),
        );
    }

    #[test]
    fn refuses_a_path_that_climbs_out_of_the_mount() {
        let mount = Path::new("/home/u/Nextcloud");
        for hostile in ["/../.bashrc", "/a/../../b", "..\\..\\x", "/", ""] {
            assert_eq!(local_path_for(mount, hostile), None, "{} must be refused", hostile);
        }
    }
}

#[cfg(test)]
mod sync_state_tests {
    use super::*;

    #[test]
    fn offline_reads_as_a_failure_icon() {
        assert!(matches!(
            TrayIcon::for_state(&SyncState::Offline),
            TrayIcon::Error
        ));
        assert!(matches!(
            TrayIcon::for_state(&SyncState::Degraded("x".into())),
            TrayIcon::Warning
        ));
    }

    #[test]
    fn a_revoked_token_keeps_its_auth_diagnosis() {
        // The connectivity probe's 401 flags the daemon offline too, so the
        // daemon's "offline" word must not bury the auth error — that error is
        // what renders the Log in button.
        let state = Arc::new(AppState::default());
        *state.auth_error.lock().unwrap() = Some("authentication failed".into());
        *state.sync_state.lock().unwrap() =
            SyncState::Error("authentication failed — re-login required".into());
        assert_eq!(apply_state_word(&state, "offline"), None);
        assert!(matches!(*state.sync_state.lock().unwrap(), SyncState::Error(_)));

        // The auth error is emitted as an event without ever being stored, so the
        // hold must not depend on the stored state already being an Error.
        let fresh = Arc::new(AppState::default());
        *fresh.auth_error.lock().unwrap() = Some("authentication failed".into());
        assert_eq!(apply_state_word(&fresh, "offline"), None);
        assert_eq!(*fresh.sync_state.lock().unwrap(), SyncState::Idle);
    }

    #[test]
    fn offline_survives_the_ipc_word_round_trip() {
        // Attach mode mirrors an external daemon: the state word it sends must
        // decode back to Offline, not fall through to the Idle default.
        let state = Arc::new(AppState::default());
        assert_eq!(
            apply_state_word(&state, &SyncState::Offline.to_string()),
            Some(SyncState::Offline)
        );
    }
}
