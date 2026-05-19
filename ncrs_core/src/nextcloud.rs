use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use remotefs::RemoteFs;
use remotefs_webdav::WebDAVFs;

use crate::backend::{
    BackendReadError, BackendWriteError, ChangeCallback, ChangeEvent, ChangeWatcherHandle,
    CloudBackend, HasNotifications, HasPreviews, PutResult, RemoteEntry, Searchable,
};
use crate::{notifications, preview, propfind, search, webdav_ops};

const MAX_POOL_IDLE: usize = 8;
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);
const WS_READ_TIMEOUT: Duration = Duration::from_secs(5);

const PATH_ENCODE: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'#')
    .add(b'%')
    .add(b'?')
    .add(b'[')
    .add(b']')
    .add(b'{')
    .add(b'}');

// -- NextcloudBackend ---------------------------------------------------------

pub struct NextcloudBackend {
    base_url: String,
    webdav_url: String,
    username: String,
    password: String,
    http: reqwest::blocking::Client,
    http_read: reqwest::blocking::Client,
    http3: bool,
    conns: Mutex<Vec<WebDAVFs>>,
}

impl NextcloudBackend {
    pub fn new(
        base_url: String,
        webdav_url: String,
        username: String,
        password: String,
        http: reqwest::blocking::Client,
        http_read: reqwest::blocking::Client,
        http3: bool,
    ) -> Result<Self, String> {
        let mut initial = WebDAVFs::new(&username, &password, &webdav_url);
        initial
            .connect()
            .map_err(|e| format!("WebDAV connect failed: {}", e))?;
        Ok(NextcloudBackend {
            base_url,
            webdav_url,
            username,
            password,
            http,
            http_read,
            http3,
            conns: Mutex::new(vec![initial]),
        })
    }

    pub fn new_offline(
        base_url: String,
        webdav_url: String,
        username: String,
        password: String,
        http: reqwest::blocking::Client,
        http_read: reqwest::blocking::Client,
        http3: bool,
    ) -> Self {
        NextcloudBackend {
            base_url,
            webdav_url,
            username,
            password,
            http,
            http_read,
            http3,
            conns: Mutex::new(Vec::new()),
        }
    }

    fn checkout(&self) -> Result<WebDAVFs, String> {
        if let Some(conn) = self.conns.lock().unwrap_or_else(|e| e.into_inner()).pop() {
            return Ok(conn);
        }
        let mut conn = WebDAVFs::new(&self.username, &self.password, &self.webdav_url);
        conn.connect()
            .map_err(|e| format!("WebDAV connect: {}", e))?;
        Ok(conn)
    }

    fn checkin(&self, conn: WebDAVFs) {
        let mut pool = self.conns.lock().unwrap_or_else(|e| e.into_inner());
        if pool.len() < MAX_POOL_IDLE {
            pool.push(conn);
        }
    }

    fn webdav_file_url(&self, remote_path: &Path) -> String {
        let rel = remote_path.strip_prefix("/").unwrap_or(remote_path);
        let encoded = percent_encoding::utf8_percent_encode(
            &rel.to_string_lossy(),
            PATH_ENCODE,
        )
        .to_string();
        format!(
            "{}/{}",
            self.webdav_url.trim_end_matches('/'),
            encoded
        )
    }
}

#[derive(Clone, Default)]
struct SharedCollector(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedCollector {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn str_to_read_error(e: String) -> BackendReadError {
    if e.contains("404") || e.contains("Not Found") {
        BackendReadError::NotFound
    } else if e.contains("timeout") || e.contains("Timeout") {
        BackendReadError::Timeout
    } else {
        BackendReadError::Network(e)
    }
}

// -- CloudBackend implementation ----------------------------------------------

impl CloudBackend for NextcloudBackend {
    fn list_dir(
        &self,
        path: &Path,
        timeout: Duration,
    ) -> Result<(Option<String>, Option<RemoteEntry>, Vec<RemoteEntry>), BackendReadError> {
        let (etag, self_entry, entries) = propfind::propfind_list(
            &self.http,
            &self.webdav_url,
            &self.username,
            &self.password,
            path,
            timeout,
        )
        .map_err(str_to_read_error)?;
        Ok((
            etag,
            self_entry.map(RemoteEntry::from),
            entries.into_iter().map(RemoteEntry::from).collect(),
        ))
    }

    fn list_dir_streaming(
        &self,
        path: &Path,
        timeout: Duration,
        entry_tx: std::sync::mpsc::Sender<RemoteEntry>,
        self_tx: std::sync::mpsc::Sender<RemoteEntry>,
    ) -> Result<Option<String>, BackendReadError> {
        propfind::propfind_list_streaming(
            &self.http,
            &self.webdav_url,
            &self.username,
            &self.password,
            path,
            timeout,
            entry_tx,
            self_tx,
        )
        .map_err(str_to_read_error)
    }

    fn dir_change_token(
        &self,
        path: &Path,
        timeout: Duration,
    ) -> Result<Option<String>, BackendReadError> {
        propfind::propfind_etag(
            &self.http,
            &self.webdav_url,
            &self.username,
            &self.password,
            path,
            timeout,
        )
        .map_err(str_to_read_error)
    }

    fn download_file(
        &self,
        path: &Path,
        dest: &mut dyn std::io::Write,
        _timeout: Duration,
    ) -> Result<u64, BackendReadError> {
        let mut conn = self.checkout().map_err(BackendReadError::Network)?;
        let collector = SharedCollector::default();
        let result = conn
            .open_file(path, Box::new(collector.clone()))
            .map_err(|e| BackendReadError::Network(e.to_string()));
        self.checkin(conn);
        let size = result?;
        let data = collector.0.lock().unwrap_or_else(|e| e.into_inner());
        dest.write_all(&data)
            .map_err(|e| BackendReadError::Network(e.to_string()))?;
        Ok(size)
    }

    fn read_file_range(
        &self,
        path: &Path,
        offset: u64,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, BackendReadError> {
        let url = self.webdav_file_url(path);
        let end = offset + buf.len() as u64 - 1;
        let resp = self
            .http_read
            .get(&url)
            .timeout(timeout)
            .header("Range", format!("bytes={}-{}", offset, end))
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .map_err(|e| BackendReadError::Network(e.to_string()))?;
        let status = resp.status();
        if status == reqwest::StatusCode::PARTIAL_CONTENT || status.is_success() {
            let bytes = resp.bytes().map_err(|e| BackendReadError::Network(e.to_string()))?;
            let n = bytes.len().min(buf.len());
            buf[..n].copy_from_slice(&bytes[..n]);
            Ok(n)
        } else if status == reqwest::StatusCode::NOT_FOUND {
            Err(BackendReadError::NotFound)
        } else {
            Err(BackendReadError::Server(
                status.as_u16(),
                String::new(),
            ))
        }
    }

    fn put_file(
        &self,
        path: &Path,
        body: Vec<u8>,
        if_match: Option<&str>,
    ) -> Result<PutResult, BackendWriteError> {
        webdav_ops::put_file(
            &self.http,
            &self.base_url,
            &self.username,
            &self.password,
            path,
            body,
            if_match,
        )
        .map(PutResult::from)
        .map_err(BackendWriteError::from)
    }

    fn mkdir(&self, path: &Path) -> Result<(), BackendWriteError> {
        webdav_ops::mkcol(
            &self.http,
            &self.base_url,
            &self.username,
            &self.password,
            path,
        )
        .map_err(BackendWriteError::from)
    }

    fn delete(&self, path: &Path) -> Result<(), BackendWriteError> {
        webdav_ops::delete(
            &self.http,
            &self.base_url,
            &self.username,
            &self.password,
            path,
        )
        .map_err(BackendWriteError::from)
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<(), BackendWriteError> {
        webdav_ops::move_resource(
            &self.http,
            &self.base_url,
            &self.username,
            &self.password,
            from,
            to,
        )
        .map_err(BackendWriteError::from)
    }

    fn is_reachable(&self, timeout: Duration) -> bool {
        propfind::propfind_etag(
            &self.http,
            &self.webdav_url,
            &self.username,
            &self.password,
            Path::new("/"),
            timeout,
        )
        .is_ok()
    }

    fn check_reachability(&self, timeout: Duration) -> crate::backend::ReachabilityStatus {
        use crate::backend::ReachabilityStatus;
        match propfind::propfind_status(
            &self.http,
            &self.webdav_url,
            &self.username,
            &self.password,
            Path::new("/"),
            timeout,
        ) {
            Ok(()) => ReachabilityStatus::Reachable,
            Err(code) if code == 401 || code == 403 => ReachabilityStatus::AuthRejected(code),
            Err(_) => ReachabilityStatus::Unreachable,
        }
    }

    fn start_change_watcher(&self, callback: ChangeCallback) -> Box<dyn ChangeWatcherHandle> {
        let http = self.http.clone();
        let base_url = self.base_url.clone();
        let webdav_url = self.webdav_url.clone();
        let username = self.username.clone();
        let password = self.password.clone();

        let connected = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let connected_ret = connected.clone();
        let shutdown_ret = shutdown.clone();
        let paused_ret = paused.clone();

        std::thread::spawn(move || {
            watcher_loop(
                &http,
                &base_url,
                &webdav_url,
                &username,
                &password,
                &connected,
                &shutdown,
                &paused,
                &callback,
            );
        });

        Box::new(NcChangeWatcher {
            connected: connected_ret,
            shutdown: shutdown_ret,
            paused: paused_ret,
        })
    }

    fn permission_mode(&self, entry: &RemoteEntry) -> u16 {
        let perms = entry.ext.strings.get("permissions").map(|s| s.as_str());
        match perms {
            Some(p) if entry.is_dir => {
                if p.contains('C') || p.contains('K') {
                    0o755
                } else {
                    0o555
                }
            }
            Some(p) if !entry.is_dir => {
                if p.contains('W') || p.contains('N') {
                    0o644
                } else {
                    0o444
                }
            }
            _ => {
                if entry.is_dir {
                    0o755
                } else {
                    0o644
                }
            }
        }
    }

    fn is_shared(&self, entry: &RemoteEntry) -> bool {
        entry
            .ext
            .booleans
            .get("is_shared")
            .copied()
            .unwrap_or(false)
    }

    fn file_id(&self, entry: &RemoteEntry) -> Option<u64> {
        entry.ext.integers.get("fileid").copied()
    }

    fn owner_display_name<'a>(&self, entry: &'a RemoteEntry) -> Option<&'a str> {
        entry.ext.strings.get("owner_display_name").map(|s| s.as_str())
    }

    fn owner_id<'a>(&self, entry: &'a RemoteEntry) -> Option<&'a str> {
        entry.ext.strings.get("owner_id").map(|s| s.as_str())
    }

    fn has_preview(&self, entry: &RemoteEntry) -> bool {
        entry
            .ext
            .booleans
            .get("has_preview")
            .copied()
            .unwrap_or(false)
    }

    fn quota(&self, timeout: Duration) -> Option<(u64, u64)> {
        propfind::propfind_quota(&self.http, &self.webdav_url, &self.username, &self.password, timeout)
            .map_err(|e| log::debug!("quota fetch: {}", e))
            .ok()
    }
}

// -- Change watcher -----------------------------------------------------------

struct NcChangeWatcher {
    connected: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
}

impl ChangeWatcherHandle for NcChangeWatcher {
    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
    fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }
}

impl Drop for NcChangeWatcher {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

fn watcher_loop(
    http: &reqwest::blocking::Client,
    base_url: &str,
    webdav_url: &str,
    username: &str,
    password: &str,
    connected: &Arc<AtomicBool>,
    shutdown: &Arc<AtomicBool>,
    paused: &Arc<AtomicBool>,
    callback: &ChangeCallback,
) {
    let mut reconnect_delay = Duration::from_secs(1);

    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }

        if paused.load(Ordering::Relaxed) {
            connected.store(false, Ordering::Relaxed);
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }

        connected.store(false, Ordering::Relaxed);

        let ws_url = match crate::notify_push::discover_ws_url(http, base_url, username, password) {
            Ok(url) => {
                log::info!("change_watcher: discovered endpoint {}", url);
                url
            }
            Err(e) => {
                log::warn!("change_watcher: discovery failed: {}", e);
                std::thread::sleep(reconnect_delay);
                reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
                continue;
            }
        };

        match watcher_connect_and_listen(
            &ws_url, http, webdav_url, username, password, connected, shutdown, paused, callback,
        ) {
            Ok(()) => {
                log::info!("change_watcher: connection closed cleanly");
                reconnect_delay = Duration::from_secs(1);
            }
            Err(e) => {
                log::warn!("change_watcher: {}", e);
            }
        }

        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        log::info!("change_watcher: reconnecting in {:?}", reconnect_delay);
        std::thread::sleep(reconnect_delay);
        reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
    }
}

fn watcher_connect_and_listen(
    ws_url: &str,
    http: &reqwest::blocking::Client,
    webdav_url: &str,
    username: &str,
    password: &str,
    connected: &Arc<AtomicBool>,
    shutdown: &Arc<AtomicBool>,
    paused: &Arc<AtomicBool>,
    callback: &ChangeCallback,
) -> Result<(), String> {
    use tungstenite::{connect, Message};

    let (mut socket, _) =
        connect(ws_url).map_err(|e| format!("WebSocket connect: {}", e))?;

    fn set_ws_read_timeout(
        socket: &tungstenite::WebSocket<
            tungstenite::stream::MaybeTlsStream<std::net::TcpStream>,
        >,
        timeout: Option<Duration>,
    ) {
        match socket.get_ref() {
            tungstenite::stream::MaybeTlsStream::Plain(tcp) => {
                let _ = tcp.set_read_timeout(timeout);
            }
            tungstenite::stream::MaybeTlsStream::Rustls(tls) => {
                let _ = tls.get_ref().set_read_timeout(timeout);
            }
            _ => {}
        }
    }

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
            log::info!("change_watcher: authenticated");
            connected.store(true, Ordering::Relaxed);
        }
        Message::Text(t) if t.starts_with("err:") => {
            return Err(format!("auth failed: {}", t));
        }
        other => {
            return Err(format!("unexpected auth response: {:?}", other));
        }
    }

    socket
        .send(Message::Text("listen notify_file_id".into()))
        .map_err(|e| format!("send listen: {}", e))?;

    set_ws_read_timeout(&socket, Some(WS_READ_TIMEOUT));

    loop {
        if shutdown.load(Ordering::Relaxed) {
            let _ = socket.close(None);
            return Ok(());
        }
        let msg = match socket.read() {
            Err(tungstenite::Error::Io(ref e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => return Err(format!("read: {}", e)),
            Ok(msg) => msg,
        };
        match msg {
            Message::Text(ref t) if !paused.load(Ordering::Relaxed) => {
                watcher_handle_event(t, http, webdav_url, username, password, callback);
            }
            Message::Text(_) => {}
            Message::Close(_) => {
                log::info!("change_watcher: server closed connection");
                connected.store(false, Ordering::Relaxed);
                return Ok(());
            }
            Message::Ping(data) => {
                let _ = socket.send(Message::Pong(data));
            }
            _ => {}
        }
    }
}

fn watcher_handle_event(
    event: &str,
    http: &reqwest::blocking::Client,
    webdav_url: &str,
    username: &str,
    password: &str,
    callback: &ChangeCallback,
) {
    let trimmed = event.trim();

    if let Some(ids_json) = trimmed.strip_prefix("notify_file_id ") {
        log::info!("change_watcher: ← {}", trimmed);
        match serde_json::from_str::<Vec<u64>>(ids_json) {
            Ok(ids) => {
                match propfind::resolve_fileids(
                    http,
                    webdav_url,
                    username,
                    password,
                    &ids,
                    Duration::from_secs(15),
                ) {
                    Ok(paths) if !paths.is_empty() => {
                        let mut dirs: Vec<PathBuf> = paths
                            .iter()
                            .filter_map(|p| p.parent().map(|d| d.to_path_buf()))
                            .collect();
                        dirs.sort();
                        dirs.dedup();
                        callback(ChangeEvent {
                            invalidated_dirs: dirs,
                            modified_files: paths,
                            invalidate_all: false,
                        });
                    }
                    Ok(_) => {
                        log::debug!(
                            "change_watcher: file_id {:?} resolved to no paths",
                            ids
                        );
                    }
                    Err(e) => {
                        log::warn!(
                            "change_watcher: resolve_fileids failed: {} — invalidating all",
                            e
                        );
                        callback(ChangeEvent {
                            invalidated_dirs: vec![],
                            modified_files: vec![],
                            invalidate_all: true,
                        });
                    }
                }
            }
            Err(e) => {
                log::warn!("change_watcher: failed to parse IDs '{}': {}", ids_json, e);
                callback(ChangeEvent {
                    invalidated_dirs: vec![],
                    modified_files: vec![],
                    invalidate_all: true,
                });
            }
        }
        return;
    }

    match trimmed {
        "notify_file" => {
            log::info!("change_watcher: file change event — invalidating all");
            callback(ChangeEvent {
                invalidated_dirs: vec![],
                modified_files: vec![],
                invalidate_all: true,
            });
        }
        "notify_notification" => {
            log::info!("change_watcher: notification event (ignored by file watcher)");
        }
        "notify_activity" => {
            log::debug!("change_watcher: activity event");
        }
        other => {
            log::debug!("change_watcher: unknown event: {}", other);
        }
    }
}

// -- Searchable ---------------------------------------------------------------

impl Searchable for NextcloudBackend {
    fn fetch_search_providers(&self) -> Result<Vec<search::SearchProvider>, String> {
        search::fetch_providers(&self.base_url, &self.username, &self.password, self.http3)
    }

    fn search(
        &self,
        term: &str,
        provider_ids: &[String],
    ) -> Result<Vec<search::SearchResultGroup>, String> {
        search::search_filtered(
            &self.base_url,
            &self.username,
            &self.password,
            term,
            self.http3,
            provider_ids,
        )
    }
}

// -- HasNotifications ---------------------------------------------------------

impl HasNotifications for NextcloudBackend {
    fn fetch_notifications(&self) -> Result<Vec<notifications::NcNotification>, String> {
        notifications::fetch_notifications(
            &self.base_url,
            &self.username,
            &self.password,
            self.http3,
        )
    }

    fn dismiss_notification(&self, id: u64) -> Result<(), String> {
        notifications::dismiss_notification(
            &self.base_url,
            &self.username,
            &self.password,
            id,
            self.http3,
        )
    }
}

// -- HasPreviews --------------------------------------------------------------

impl HasPreviews for NextcloudBackend {
    fn prefetch_thumbnail(
        &self,
        mount_point: &Path,
        remote_path: &Path,
        mtime: Option<std::time::SystemTime>,
        entry: &RemoteEntry,
    ) {
        let fileid = self.file_id(entry);
        preview::prefetch_thumbnail(
            &self.http,
            &self.base_url,
            &self.username,
            &self.password,
            mount_point,
            remote_path,
            mtime,
            fileid,
        );
    }

    fn prefetch_directory_thumbnails(
        &self,
        mount_point: &Path,
        entries: &[(PathBuf, Option<std::time::SystemTime>, RemoteEntry)],
    ) {
        let mapped: Vec<(PathBuf, Option<std::time::SystemTime>, bool, Option<u64>)> = entries
            .iter()
            .map(|(p, mtime, entry)| {
                (
                    p.clone(),
                    *mtime,
                    self.has_preview(entry),
                    self.file_id(entry),
                )
            })
            .collect();
        // active_streams check is a FUSE-layer concern, passed as 0 here
        let zero = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        preview::prefetch_directory_thumbnails(
            &self.http,
            &self.base_url,
            &self.username,
            &self.password,
            mount_point,
            &mapped,
            &zero,
        );
    }
}
