use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};


use crate::auth::Credentials;
use crate::backend::{
    BackendReadError, BackendWriteError, ChangeCallback, ChangeEvent, ChangeWatcherHandle,
    CloudBackend, HasNotifications, HasPreviews, PutResult, RemoteEntry, Searchable,
};
use crate::{notifications, preview, propfind, search, webdav_ops};

/// Timeout for the credential probe run once at startup.
const CONNECT_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Pause between the connectivity probe's first failure and its one retry.
/// Long enough for a fresh QUIC dial to escape whatever killed the first
/// attempt (an idled-out connection, a NAT rebind, a congested uplink),
/// short enough that a real outage is still declared well inside one
/// monitor cycle.
const PROBE_RETRY_DELAY: Duration = Duration::from_secs(2);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);
const WS_READ_TIMEOUT: Duration = Duration::from_secs(5);
// The read loop swallows read timeouts, so without a keepalive a silently dead
// socket — NAT or VPN idle timeout, suspend/resume, a middlebox dropping the flow —
// leaves `connected` true forever while no event ever arrives. Ping once the link
// has been quiet and treat a missing pong as a disconnect, so the reconnect path
// (and the cache revalidation hanging off it) actually runs.
const WS_PING_IDLE: Duration = Duration::from_secs(30);
const WS_PONG_TIMEOUT: Duration = Duration::from_secs(10);

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
    creds: Credentials,
    clients: crate::http_clients::HttpClients,
}

impl NextcloudBackend {
    pub fn new(
        base_url: String,
        webdav_url: String,
        creds: Credentials,
        clients: crate::http_clients::HttpClients,
    ) -> Result<Self, String> {
        // Probe the credentials once up front so a bad password fails at mount
        // time rather than on the user's first listing. This used to be
        // `WebDAVFs::connect()`; it now goes through the same PROPFIND path as
        // every other request, so there is one HTTP client, one TLS stack and
        // one set of redirect rules for the whole daemon.
        //
        // Depth 0 deliberately: this only has to answer "do these credentials
        // work". The first version used `propfind_list`, which is Depth 1 — a
        // full listing of the account root, fetched and then thrown away. On a
        // large root that measured 4.5 s of blocking startup before the mount
        // came up, and the listing was not even kept.
        let mut startup = propfind::propfind_status(
            &clients.get(),
            &webdav_url,
            &creds,
            std::path::Path::new("/"),
            CONNECT_PROBE_TIMEOUT,
        );
        // HTTP/3 is judged here, once, at mount time — not per request. A
        // transport-level failure over QUIC while plain HTTPS answers the same
        // probe means this network/server pairing cannot do HTTP/3 at all
        // (UDP/443 filtered, a middlebox, a broken listener), so demote before
        // the mount comes up and run the whole session on HTTP/2. Mid-session
        // QUIC errors are NOT a demotion signal: once the transport has proven
        // itself at startup, a later failure means the network is down and
        // HTTP/2 would fail just the same — that is the connectivity monitor's
        // outage handling, not a transport problem.
        if matches!(startup, Err(0)) && clients.http3_active() {
            startup = propfind::propfind_status(
                &clients.h2(),
                &webdav_url,
                &creds,
                std::path::Path::new("/"),
                CONNECT_PROBE_TIMEOUT,
            );
            if startup.is_ok() {
                clients.demote();
            }
        }
        startup.map_err(|code| match code {
            0 => "WebDAV connect failed: server unreachable".to_string(),
            401 | 403 => format!("WebDAV connect failed: credentials rejected (HTTP {})", code),
            other => format!("WebDAV connect failed: HTTP {}", other),
        })?;

        Ok(NextcloudBackend { base_url, webdav_url, creds, clients })
    }

    pub fn new_offline(
        base_url: String,
        webdav_url: String,
        creds: Credentials,
        clients: crate::http_clients::HttpClients,
    ) -> Self {
        NextcloudBackend { base_url, webdav_url, creds, clients }
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

fn propfind_to_read_error(e: propfind::PropfindError) -> BackendReadError {
    use propfind::PropfindError;
    match e {
        PropfindError::Status { code: 404, .. } => BackendReadError::NotFound,
        PropfindError::Status { code, .. } => BackendReadError::Server(code, e.to_string()),
        PropfindError::Transport { timed_out: true, .. } => BackendReadError::Timeout,
        PropfindError::Transport { .. } => BackendReadError::Network(e.to_string()),
        PropfindError::Body(msg) => BackendReadError::Truncated(msg),
    }
}

/// A probe status that proves the Nextcloud app itself answered, just badly: a
/// 500 (PHP error, DB lock), 507 or 429. Going offline on those would hide a
/// reachable server behind the cache and suppress live sync; the listing path
/// backs off from 5xx on its own. 502/503/504 stay "unreachable": that is a
/// reverse proxy reporting the app behind it down, which is a real outage.
fn answered_while_overloaded(code: u16) -> bool {
    matches!(code, 429 | 500 | 507)
}

// -- CloudBackend implementation ----------------------------------------------

impl CloudBackend for NextcloudBackend {
    fn list_dir(
        &self,
        path: &Path,
        timeout: Duration,
    ) -> Result<(Option<String>, Option<RemoteEntry>, Vec<RemoteEntry>), BackendReadError> {
        let (etag, self_entry, entries) = propfind::propfind_list(
            &self.clients.get(),
            &self.webdav_url,
            &self.creds,
            path,
            timeout,
        )
        .map_err(propfind_to_read_error)?;
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
            &self.clients.get(),
            &self.webdav_url,
            &self.creds,
            path,
            timeout,
            entry_tx,
            self_tx,
        )
        .map_err(propfind_to_read_error)
    }

    fn dir_change_token(
        &self,
        path: &Path,
        timeout: Duration,
    ) -> Result<Option<String>, BackendReadError> {
        propfind::propfind_etag(
            &self.clients.get(),
            &self.webdav_url,
            &self.creds,
            path,
            timeout,
        )
        .map_err(propfind_to_read_error)
    }

    fn download_file(
        &self,
        path: &Path,
        dest: &mut dyn std::io::Write,
        timeout: Duration,
    ) -> Result<u64, BackendReadError> {
        // One path for both auth types. The basic-auth branch used to go through
        // remotefs-webdav, which buffered the entire file in memory before
        // writing it out; this streams straight into `dest`.
        let url = self.webdav_file_url(path);
        let mut resp = self.creds.apply(self.clients.read().get(&url).timeout(timeout))
            .send()
            .map_err(|e| BackendReadError::Network(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            if status == reqwest::StatusCode::NOT_FOUND {
                return Err(BackendReadError::NotFound);
            }
            let body = resp.text().unwrap_or_default();
            return Err(BackendReadError::Server(status.as_u16(), body));
        }
        resp.copy_to(dest)
            .map_err(|e| BackendReadError::Network(e.to_string()))
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
            .clients
            .read()
            .get(&url)
            .timeout(timeout)
            .header("Range", format!("bytes={}-{}", offset, end))
            ;
        let resp = self.creds.apply(resp)
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
        webdav_ops::put_file_chunked(
            &self.clients.get(),
            &self.base_url,
            &self.creds,
            path,
            body,
            if_match,
        )
        .map(PutResult::from)
        .map_err(BackendWriteError::from)
    }

    fn put_file_from_path(
        &self,
        path: &Path,
        staging_path: &Path,
        if_match: Option<&str>,
    ) -> Result<PutResult, BackendWriteError> {
        webdav_ops::put_file_from_path(
            &self.clients.get(),
            &self.base_url,
            &self.creds,
            path,
            staging_path,
            if_match,
        )
        .map(PutResult::from)
        .map_err(BackendWriteError::from)
    }

    fn open_chunked_upload(&self, _path: &Path) -> Result<crate::backend::ChunkedUploadSession, BackendWriteError> {
        webdav_ops::open_chunked_session(&self.clients.get(), &self.base_url, &self.creds)
            .map(|uploads_base| crate::backend::ChunkedUploadSession { uploads_base })
            .map_err(BackendWriteError::from)
    }

    fn put_chunk(
        &self,
        session: &crate::backend::ChunkedUploadSession,
        index: u64,
        body: Vec<u8>,
    ) -> Result<(), BackendWriteError> {
        webdav_ops::put_chunk(&self.clients.get(), &self.creds, &session.uploads_base, index, body)
            .map_err(BackendWriteError::from)
    }

    fn finish_chunked_upload(
        &self,
        session: &crate::backend::ChunkedUploadSession,
        path: &Path,
        if_match: Option<&str>,
    ) -> Result<PutResult, BackendWriteError> {
        webdav_ops::finish_chunked_upload(
            &self.clients.get(),
            &self.base_url,
            &self.creds,
            &session.uploads_base,
            path,
            if_match,
        )
        .map(PutResult::from)
        .map_err(BackendWriteError::from)
    }

    fn abort_chunked_upload(&self, session: &crate::backend::ChunkedUploadSession) {
        webdav_ops::abort_chunked_upload(&self.clients.get(), &self.creds, &session.uploads_base);
    }

    fn mkdir(&self, path: &Path) -> Result<(), BackendWriteError> {
        webdav_ops::mkcol(
            &self.clients.get(),
            &self.base_url,
            &self.creds,
            path,
        )
        .map_err(BackendWriteError::from)
    }

    fn delete(&self, path: &Path) -> Result<(), BackendWriteError> {
        webdav_ops::delete(
            &self.clients.get(),
            &self.base_url,
            &self.creds,
            path,
        )
        .map_err(BackendWriteError::from)
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<(), BackendWriteError> {
        webdav_ops::move_resource(
            &self.clients.get(),
            &self.base_url,
            &self.creds,
            from,
            to,
        )
        .map_err(BackendWriteError::from)
    }

    fn is_reachable(&self, timeout: Duration) -> bool {
        propfind::propfind_etag(
            &self.clients.get(),
            &self.webdav_url,
            &self.creds,
            Path::new("/"),
            timeout,
        )
        .is_ok()
    }

    fn check_reachability(&self, timeout: Duration) -> crate::backend::ReachabilityStatus {
        use crate::backend::ReachabilityStatus;
        let probe = |client: &crate::http_clients::DavClient| {
            propfind::propfind_status(client, &self.webdav_url, &self.creds, Path::new("/"), timeout)
        };
        match probe(&self.clients.get()) {
            Ok(()) => return ReachabilityStatus::Reachable,
            Err(code) if code == 401 || code == 403 => return ReachabilityStatus::AuthRejected(code),
            Err(_) => {}
        }
        // One failed probe is not an outage. QUIC rides UDP, so a connection the
        // server or a NAT quietly idled out fails exactly one request and works
        // again on the next dial — and the probe also shares the uplink with up
        // to 10 concurrent transfers, so a busy revalidation burst can time out a
        // single sample against a perfectly healthy server. Believing that one
        // sample flips the whole mount offline (or, below, demotes HTTP/3 for a
        // week). Retry once before concluding anything: a genuinely down server
        // costs one extra probe per monitor cycle, and a recovered server still
        // answers the first probe, so recovery latency is unchanged.
        log::debug!("CONNECTIVITY probe failed — retrying once in {:?}", PROBE_RETRY_DELAY);
        std::thread::sleep(PROBE_RETRY_DELAY);
        // No HTTP/2 fallback and no demotion here: the transport was proven at
        // mount time (see `NextcloudBackend::new`). A QUIC failure on a mount
        // that has been speaking HTTP/3 all session means the network is down —
        // HTTP/2 would fail the same way — so report the outage and let the
        // monitor's re-probe cadence pick the recovery up.
        match probe(&self.clients.get()) {
            Ok(()) => ReachabilityStatus::Reachable,
            Err(code) if code == 401 || code == 403 => ReachabilityStatus::AuthRejected(code),
            Err(code) if answered_while_overloaded(code) => {
                log::warn!("CONNECTIVITY probe: server answered {} — reachable but struggling, staying online", code);
                ReachabilityStatus::Reachable
            }
            Err(_) => ReachabilityStatus::Unreachable,
        }
    }

    fn start_change_watcher(&self, callback: ChangeCallback) -> Box<dyn ChangeWatcherHandle> {
        let clients = self.clients.clone();
        let base_url = self.base_url.clone();
        let webdav_url = self.webdav_url.clone();
        let creds = self.creds.clone();

        let connected = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let generation = Arc::new(AtomicU64::new(0));
        let connected_ret = connected.clone();
        let shutdown_ret = shutdown.clone();
        let paused_ret = paused.clone();
        let generation_ret = generation.clone();

        std::thread::spawn(move || {
            watcher_loop(
                &clients,
                &base_url,
                &webdav_url,
                &creds,
                &connected,
                &generation,
                &shutdown,
                &paused,
                &callback,
            );
        });

        Box::new(NcChangeWatcher {
            connected: connected_ret,
            generation: generation_ret,
            shutdown: shutdown_ret,
            paused: paused_ret,
        })
    }

    fn permission_mode(&self, entry: &RemoteEntry) -> u16 {
        let perms = entry.ext.permissions.as_deref();
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
        entry.ext.is_shared
    }

    fn file_id(&self, entry: &RemoteEntry) -> Option<u64> {
        entry.ext.fileid
    }

    fn owner_display_name<'a>(&self, entry: &'a RemoteEntry) -> Option<&'a str> {
        entry.ext.owner_display_name.as_deref()
    }

    fn owner_id<'a>(&self, entry: &'a RemoteEntry) -> Option<&'a str> {
        entry.ext.owner_id.as_deref()
    }

    fn has_preview(&self, entry: &RemoteEntry) -> bool {
        entry.ext.has_preview
    }

    fn quota(&self, timeout: Duration) -> Option<(u64, u64)> {
        propfind::propfind_quota(&self.clients.get(), &self.webdav_url, &self.creds, timeout)
            .map_err(|e| log::debug!("quota fetch: {}", e))
            .ok()
    }
}

// -- Change watcher -----------------------------------------------------------

struct NcChangeWatcher {
    connected: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
}

impl ChangeWatcherHandle for NcChangeWatcher {
    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
    fn connect_generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
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
    clients: &crate::http_clients::HttpClients,
    base_url: &str,
    webdav_url: &str,
    creds: &Credentials,
    connected: &Arc<AtomicBool>,
    generation: &Arc<AtomicU64>,
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

        // Resolved per reconnect rather than once at startup, so a mid-session
        // demotion to HTTP/2 is picked up on the next attempt instead of leaving the
        // change watcher stranded on a QUIC transport that has already been proven
        // broken. Demotion is permanent, so re-reading it costs one atomic load.
        let http = clients.get();

        let info = match crate::notify_push::discover_endpoints(&http, base_url, creds) {
            Ok(info) => {
                log::info!("change_watcher: discovered endpoint {}", info.ws_url);
                info
            }
            Err(e) => {
                log::warn!("change_watcher: discovery failed: {}", e);
                std::thread::sleep(reconnect_delay);
                reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
                continue;
            }
        };

        match watcher_connect_and_listen(
            &info, &http, base_url, webdav_url, creds, connected, generation, shutdown, paused, callback,
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
    info: &crate::notify_push::NotifyPushInfo,
    http: &crate::http_clients::DavClient,
    base_url: &str,
    webdav_url: &str,
    creds: &Credentials,
    connected: &Arc<AtomicBool>,
    generation: &Arc<AtomicU64>,
    shutdown: &Arc<AtomicBool>,
    paused: &Arc<AtomicBool>,
    callback: &ChangeCallback,
) -> Result<(), String> {
    use tungstenite::{connect, Message};

    let (ws_user, ws_secret) = if creds.is_bearer() {
        let pre_auth_url = info.pre_auth_url.as_deref()
            .ok_or_else(|| "bearer auth requires notify_push pre_auth endpoint, but server does not advertise it".to_string())?;
        let ticket = crate::notify_push::fetch_pre_auth_ticket(http, pre_auth_url, base_url, creds)?;
        log::info!("change_watcher: obtained pre_auth ticket");
        (String::new(), ticket)
    } else {
        (creds.username().to_string(), creds.secret().to_string())
    };

    // Re-checked immediately before connecting, not just at discovery: the
    // handshake below sends the account credential as its first two frames.
    crate::notify_push::validate_endpoint(
        &info.ws_url,
        base_url,
        crate::notify_push::EndpointKind::WebSocket,
    )?;

    let (mut socket, _) =
        connect(&info.ws_url).map_err(|e| format!("WebSocket connect: {}", e))?;

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
        .send(Message::Text(ws_user.into()))
        .map_err(|e| format!("send username: {}", e))?;
    socket
        .send(Message::Text(ws_secret.into()))
        .map_err(|e| format!("send password: {}", e))?;

    let auth_msg = socket
        .read()
        .map_err(|e| format!("read auth response: {}", e))?;

    match &auth_msg {
        Message::Text(t) if *t == "authenticated" => {
            log::info!("change_watcher: authenticated");
            // Bump before publishing `connected`: a poller that sees the connection
            // must never see a generation that lags behind it.
            generation.fetch_add(1, Ordering::Relaxed);
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

    let mut last_rx = Instant::now();
    let mut ping_sent: Option<Instant> = None;

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
                match ping_sent {
                    Some(sent) if sent.elapsed() >= WS_PONG_TIMEOUT => {
                        connected.store(false, Ordering::Relaxed);
                        return Err(format!(
                            "no pong within {:?} — connection is dead", WS_PONG_TIMEOUT
                        ));
                    }
                    Some(_) => {}
                    None if last_rx.elapsed() >= WS_PING_IDLE => {
                        if let Err(e) = socket.send(Message::Ping(Vec::new().into())) {
                            connected.store(false, Ordering::Relaxed);
                            return Err(format!("send ping: {}", e));
                        }
                        ping_sent = Some(Instant::now());
                    }
                    None => {}
                }
                continue;
            }
            Err(e) => {
                connected.store(false, Ordering::Relaxed);
                return Err(format!("read: {}", e));
            }
            Ok(msg) => msg,
        };
        // Any traffic — event, pong, even a server ping — proves the link is alive.
        last_rx = Instant::now();
        ping_sent = None;
        match msg {
            Message::Text(ref t) if !paused.load(Ordering::Relaxed) => {
                watcher_handle_event(t, http, webdav_url, creds, callback);
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
            Message::Pong(_) => {}
            _ => {}
        }
    }
}

fn watcher_handle_event(
    event: &str,
    http: &crate::http_clients::DavClient,
    webdav_url: &str,
    creds: &Credentials,
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
                    creds,
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
        search::fetch_providers(&self.base_url, &self.creds, self.clients.http3_active())
    }

    fn search(
        &self,
        term: &str,
        provider_ids: &[String],
    ) -> Result<Vec<search::SearchResultGroup>, String> {
        search::search_filtered(
            &self.base_url,
            &self.creds,
            term,
            self.clients.http3_active(),
            provider_ids,
        )
    }
}

// -- HasNotifications ---------------------------------------------------------

impl HasNotifications for NextcloudBackend {
    fn fetch_notifications(&self) -> Result<Vec<notifications::NcNotification>, String> {
        notifications::fetch_notifications(
            &self.base_url,
            &self.creds,
            self.clients.http3_active(),
        )
    }

    fn dismiss_notification(&self, id: u64) -> Result<(), String> {
        notifications::dismiss_notification(
            &self.base_url,
            &self.creds,
            id,
            self.clients.http3_active(),
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
            &self.clients.get(),
            &self.base_url,
            &self.creds,
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
            &self.clients.get(),
            &self.base_url,
            &self.creds,
            mount_point,
            &mapped,
            &zero,
        );
    }
}
