use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tungstenite::{connect, Message};

use crate::ipc::DirtySet;
use crate::MutexExt;

const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);
const CAPABILITIES_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(serde::Deserialize)]
struct OcsCapabilities {
    ocs: OcsCapBody,
}

#[derive(serde::Deserialize)]
struct OcsCapBody {
    data: OcsCapData,
}

#[derive(serde::Deserialize)]
struct OcsCapData {
    capabilities: Capabilities,
}

#[derive(serde::Deserialize)]
struct Capabilities {
    notify_push: Option<NotifyPushCap>,
}

#[derive(serde::Deserialize)]
struct NotifyPushCap {
    endpoints: NotifyPushEndpoints,
}

#[derive(serde::Deserialize)]
struct NotifyPushEndpoints {
    websocket: String,
}

fn discover_ws_url(
    client: &reqwest::blocking::Client,
    base_url: &str,
    username: &str,
    password: &str,
) -> Result<String, String> {
    let url = format!("{}/ocs/v2.php/cloud/capabilities?format=json", base_url);
    let resp = client
        .get(&url)
        .timeout(CAPABILITIES_TIMEOUT)
        .basic_auth(username, Some(password))
        .header("OCS-APIREQUEST", "true")
        .send()
        .map_err(|e| format!("capabilities request failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("capabilities API returned {}", resp.status()));
    }

    let caps: OcsCapabilities = resp.json().map_err(|e| format!("capabilities parse error: {}", e))?;
    caps.ocs
        .data
        .capabilities
        .notify_push
        .map(|np| np.endpoints.websocket)
        .ok_or_else(|| "notify_push capability not found (app not installed?)".into())
}

fn invalidate_all_dirs(cache: &Mutex<crate::FsCache>, dirty: &DirtySet) {
    let mut c = cache.safe_lock();
    let expired = Instant::now() - crate::DIR_CACHE_TTL - Duration::from_secs(1);
    let mut paths = Vec::new();
    for (path, entry) in c.dir_cache.iter_mut() {
        entry.at = expired;
        entry.refreshing = false;
        paths.push(path.clone());
    }
    drop(c);
    let mut d = dirty.safe_lock();
    for p in paths {
        d.insert(p);
    }
}

pub(crate) fn start(
    client: reqwest::blocking::Client,
    base_url: String,
    username: String,
    password: String,
    cache: Arc<Mutex<crate::FsCache>>,
    dirty: DirtySet,
    is_offline: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        run_loop(&client, &base_url, &username, &password, &cache, &dirty, &is_offline);
    });
}

fn run_loop(
    client: &reqwest::blocking::Client,
    base_url: &str,
    username: &str,
    password: &str,
    cache: &Arc<Mutex<crate::FsCache>>,
    dirty: &DirtySet,
    is_offline: &Arc<AtomicBool>,
) {
    let mut reconnect_delay = Duration::from_secs(1);

    loop {
        if is_offline.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }

        let ws_url = match discover_ws_url(client, base_url, username, password) {
            Ok(url) => {
                log::info!("notify_push: discovered endpoint {}", url);
                url
            }
            Err(e) => {
                log::warn!("notify_push: {}", e);
                return;
            }
        };

        match connect_and_listen(&ws_url, username, password, cache, dirty) {
            Ok(()) => {
                log::info!("notify_push: connection closed cleanly");
                reconnect_delay = Duration::from_secs(1);
            }
            Err(e) => {
                log::warn!("notify_push: {}", e);
            }
        }

        log::info!("notify_push: reconnecting in {:?}", reconnect_delay);
        std::thread::sleep(reconnect_delay);
        reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
    }
}

fn connect_and_listen(
    ws_url: &str,
    username: &str,
    password: &str,
    cache: &Arc<Mutex<crate::FsCache>>,
    dirty: &DirtySet,
) -> Result<(), String> {
    let (mut socket, _response) = connect(ws_url).map_err(|e| format!("WebSocket connect: {}", e))?;

    socket
        .send(Message::Text(username.into()))
        .map_err(|e| format!("send username: {}", e))?;
    socket
        .send(Message::Text(password.into()))
        .map_err(|e| format!("send password: {}", e))?;

    let auth_msg = socket
        .read()
        .map_err(|e| format!("read auth response: {}", e))?;

    match &auth_msg {
        Message::Text(t) if *t == "authenticated" => {
            log::info!("notify_push: authenticated successfully");
        }
        Message::Text(t) if t.starts_with("err:") => {
            return Err(format!("auth failed: {}", t));
        }
        other => {
            return Err(format!("unexpected auth response: {:?}", other));
        }
    }

    loop {
        let msg = socket.read().map_err(|e| format!("read: {}", e))?;
        match msg {
            Message::Text(ref t) => {
                handle_event(t.as_ref(), cache, dirty);
            }
            Message::Close(_) => {
                log::info!("notify_push: server closed connection");
                return Ok(());
            }
            Message::Ping(data) => {
                let _ = socket.send(Message::Pong(data));
            }
            _ => {}
        }
    }
}

fn handle_event(event: &str, cache: &Arc<Mutex<crate::FsCache>>, dirty: &DirtySet) {
    match event.trim() {
        "notify_file" => {
            log::info!("notify_push: file change event — invalidating dir cache");
            invalidate_all_dirs(cache, dirty);
        }
        "notify_notification" => {
            log::info!("notify_push: notification event");
        }
        "notify_activity" => {
            log::debug!("notify_push: activity event");
        }
        other => {
            log::debug!("notify_push: unknown event: {}", other);
        }
    }
}
