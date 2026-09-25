//! The daemon's shared HTTP client set, with an HTTP/3 → HTTP/2 escape hatch.
//!
//! When `http3` is configured, `reqwest`'s `http3_prior_knowledge()` makes a
//! client speak QUIC *only* — it never negotiates down to TLS-over-TCP. That is
//! fine when QUIC works and catastrophic when it does not: a firewall dropping
//! UDP/443, a middlebox, or a server whose QUIC listener is broken makes every
//! WebDAV request fail at the transport layer, which the read path reads as
//! "the server is unreachable" and turns into a mount-wide offline state — while
//! ordinary HTTPS to the same host works perfectly.
//!
//! So we build both pairs up front and keep a latch. The mount-time probe in
//! `NextcloudBackend::new` is the sole arbiter: when it fails over HTTP/3 but
//! succeeds over HTTP/2, it calls [`HttpClients::demote`] and every caller
//! transparently uses HTTP/2 for the rest of the session. Nothing demotes
//! mid-session — once HTTP/3 has worked at startup, a later QUIC failure means
//! the network is down (HTTP/2 would fail the same way), which is the
//! connectivity monitor's business, not a transport verdict. Demotion is
//! one-way on purpose — flapping between transports would be worse than
//! staying on the one we have proven works.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long a persisted demotion keeps subsequent sessions on HTTP/2 before
/// HTTP/3 is given another chance.
///
/// Without persistence every restart re-armed QUIC, and on a network where it
/// keeps failing each session paid one offline blip (failed requests, a probe
/// cycle, the demotion warning) before latching onto HTTP/2 again. A week is
/// long enough that a laptop living on such a network stops paying that tax,
/// and short enough that a fixed path (router change, server upgrade) wins
/// QUIC back on its own.
const H3_DEMOTION_RETRY: Duration = Duration::from_secs(7 * 24 * 3600);

/// A request-building handle that stamps the HTTP version its transport needs
/// onto every request built through it.
///
/// reqwest 0.13's `http3_prior_knowledge()` only *builds* the QUIC connector:
/// a request is routed to it solely when the request itself carries
/// `Version::HTTP_3`. Without the stamp every request silently rides TCP —
/// and with the h3 preference reqwest sets no ALPN on that TCP path, so the
/// "HTTP/3 client" actually speaks HTTP/1.1. This wrapper is what makes the
/// configured transport real, and it exposes the same builder surface as
/// `reqwest::blocking::Client` so call sites read identically.
#[derive(Clone)]
pub struct DavClient {
    client: reqwest::blocking::Client,
    version: Option<reqwest::Version>,
}

impl DavClient {
    fn stamp(&self, rb: reqwest::blocking::RequestBuilder) -> reqwest::blocking::RequestBuilder {
        match self.version {
            Some(v) => rb.version(v),
            None => rb,
        }
    }

    pub fn get<U: reqwest::IntoUrl>(&self, url: U) -> reqwest::blocking::RequestBuilder {
        self.stamp(self.client.get(url))
    }

    pub fn post<U: reqwest::IntoUrl>(&self, url: U) -> reqwest::blocking::RequestBuilder {
        self.stamp(self.client.post(url))
    }

    pub fn put<U: reqwest::IntoUrl>(&self, url: U) -> reqwest::blocking::RequestBuilder {
        self.stamp(self.client.put(url))
    }

    pub fn delete<U: reqwest::IntoUrl>(&self, url: U) -> reqwest::blocking::RequestBuilder {
        self.stamp(self.client.delete(url))
    }

    pub fn request<U: reqwest::IntoUrl>(&self, method: reqwest::Method, url: U) -> reqwest::blocking::RequestBuilder {
        self.stamp(self.client.request(method, url))
    }

    /// `h3 = true` stamps every request with `Version::HTTP_3`; false builds
    /// plain TCP requests. For client sets managed outside [`HttpClients`]
    /// (the notifications and search side-clients).
    pub fn new(client: reqwest::blocking::Client, h3: bool) -> Self {
        DavClient { client, version: h3.then_some(reqwest::Version::HTTP_3) }
    }

    /// True when requests built through this handle go out over QUIC.
    pub fn is_h3(&self) -> bool {
        self.version == Some(reqwest::Version::HTTP_3)
    }
}

/// How many downloads may run at once, and so how many read clients each
/// transport gets: `read_throttle` has this many slots, and slot `i` always uses
/// read client `i`.
///
/// One client per slot, because a client is one connection: reqwest's h3 pool
/// keeps exactly one QUIC connection per host, so with a single read client every
/// download shared one congestion controller, one UDP socket and one runtime
/// thread (4 parallel readers got ~31 MB/s together, where one stream alone bursts
/// to 36-44 MB/s). Tying a connection to a slot makes it 1:1 — two concurrent
/// downloads never share one — and bounds the connections by the slots.
///
/// Each client costs one reqwest runtime thread (and, over HTTP/3, one UDP socket)
/// for the life of the mount; `bg::MAX_HTTP_CLIENT_THREADS` counts them. They are
/// built once, at mount, for both transports, and never added to afterwards.
pub const DOWNLOAD_CONNECTIONS: usize = 8;

#[derive(Clone)]
pub struct HttpClients {
    /// Preferred clients: HTTP/3 when configured, otherwise clones of the
    /// HTTP/2 set (a `Client` is `Arc`-based, so the clone shares one pool).
    pref: reqwest::blocking::Client,
    /// One read client per download slot; see [`DOWNLOAD_CONNECTIONS`].
    read_pref: Arc<[reqwest::blocking::Client]>,
    /// Always-usable HTTP/2 clients, and the target of a demotion.
    h2: reqwest::blocking::Client,
    read_h2: Arc<[reqwest::blocking::Client]>,
    demoted: Arc<AtomicBool>,
    http3: bool,
    /// Where a demotion is recorded so the next session starts on HTTP/2
    /// instead of re-discovering the broken transport. `None` = session-only.
    marker: Option<PathBuf>,
}

impl HttpClients {
    /// `pref`/`read_pref` must be the HTTP/3 clients when `http3` is true; pass
    /// clones of the HTTP/2 set when it is false. The read sets hold one client per
    /// download slot and must not be empty.
    pub fn new(
        pref: reqwest::blocking::Client,
        read_pref: Vec<reqwest::blocking::Client>,
        h2: reqwest::blocking::Client,
        read_h2: Vec<reqwest::blocking::Client>,
        http3: bool,
    ) -> Self {
        assert!(!read_pref.is_empty() && !read_h2.is_empty(), "a read client set cannot be empty");
        HttpClients {
            pref,
            read_pref: read_pref.into(),
            h2,
            read_h2: read_h2.into(),
            demoted: Arc::new(AtomicBool::new(false)),
            http3,
            marker: None,
        }
    }

    /// Persist demotions to `path`, and honour a demotion a previous session
    /// recorded there: a marker younger than [`H3_DEMOTION_RETRY`] starts this
    /// session on HTTP/2 outright; an older (or unreadable) one is removed so
    /// HTTP/3 gets retried. A no-op when HTTP/3 is not configured.
    pub fn with_demotion_marker(mut self, path: PathBuf) -> Self {
        if !self.http3 {
            return self;
        }
        match read_marker_age(&path) {
            Some(age) if age < H3_DEMOTION_RETRY => {
                self.demoted.store(true, Ordering::Relaxed);
                let retry_in = H3_DEMOTION_RETRY - age;
                log::info!(
                    "HTTP/3 was found unusable on this network {}h ago — starting on HTTP/2                      (retrying HTTP/3 in ~{}h; delete {} or set `http3: false` to decide manually)",
                    age.as_secs() / 3600,
                    retry_in.as_secs() / 3600,
                    path.display()
                );
            }
            Some(_) => {
                let _ = std::fs::remove_file(&path);
                log::info!("HTTP/3 demotion marker expired — giving QUIC another chance this session");
            }
            None => {}
        }
        self.marker = Some(path);
        self
    }

    /// The version requests must carry for the preferred transport, while it
    /// is the active one.
    fn pref_version(&self) -> Option<reqwest::Version> {
        if self.http3_active() { Some(reqwest::Version::HTTP_3) } else { None }
    }

    /// The metadata/write client every caller should use.
    pub fn get(&self) -> DavClient {
        if self.demoted.load(Ordering::Relaxed) {
            DavClient { client: self.h2.clone(), version: None }
        } else {
            DavClient { client: self.pref.clone(), version: self.pref_version() }
        }
    }

    /// The read client for download slot `slot` (a `read_throttle` permit's
    /// [`slot`](crate::ThrottleGuard::slot)), so each slot always talks over its own
    /// connection. See the construction in `lib.rs` for how the set is tuned.
    pub fn read(&self, slot: usize) -> DavClient {
        // Both sets are the same size; the modulo only keeps a stray index in range.
        if self.demoted.load(Ordering::Relaxed) {
            DavClient { client: self.read_h2[slot % self.read_h2.len()].clone(), version: None }
        } else {
            DavClient { client: self.read_pref[slot % self.read_pref.len()].clone(), version: self.pref_version() }
        }
    }

    /// The HTTP/2 client, for the mount-time probe that decides whether to
    /// demote.
    pub fn h2(&self) -> DavClient {
        DavClient { client: self.h2.clone(), version: None }
    }

    /// True while requests still go out over QUIC — i.e. HTTP/3 is configured
    /// and has not yet been proven broken.
    pub fn http3_active(&self) -> bool {
        self.http3 && !self.demoted.load(Ordering::Relaxed)
    }

    /// Latch onto HTTP/2 for the rest of the session, recording the demotion
    /// for the next session when a marker path is configured. Logs once.
    pub fn demote(&self) {
        if !self.demoted.swap(true, Ordering::Relaxed) {
            log::warn!(
                "HTTP/3 unusable on this network (QUIC failed where HTTP/2 succeeded) — \
                 running this session on HTTP/2; \
                 set `http3: false` in config.yaml to stop probing QUIC at startup"
            );
            if let Some(ref path) = self.marker {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                if let Err(e) = std::fs::write(path, now.to_string()) {
                    log::debug!("could not persist HTTP/3 demotion to {}: {}", path.display(), e);
                }
            }
        }
    }
}

/// Whether HTTP/3 is usable for the server whose demotion marker is `path`.
///
/// [`HttpClients`] answers this for the daemon's own requests, but the search
/// and notification clients are built outside it (`search::client`,
/// `notifications::client`) and used to read the raw config flag instead. On a
/// network where QUIC is blocked that made them the only part of the app still
/// dialling a QUIC-only client: the mount demoted and worked while every search
/// and every notification poll failed for the whole session.
///
/// The marker is the shared signal on purpose — it is written by whichever
/// process owns the mount, so this also answers correctly in the GUI's attached
/// mode, where there is no in-process `HttpClients` to consult at all.
pub fn http3_available(http3_configured: bool, marker: &Path) -> bool {
    if !http3_configured {
        return false;
    }
    !matches!(read_marker_age(marker), Some(age) if age < H3_DEMOTION_RETRY)
}

/// Age of the demotion marker at `path`, or `None` when there is no readable
/// marker. A timestamp in the future (clock stepped backwards since it was
/// written) reads as age zero — still demoted — rather than as garbage.
fn read_marker_age(path: &Path) -> Option<Duration> {
    let content = std::fs::read_to_string(path).ok()?;
    let ts = match content.trim().parse::<u64>() {
        Ok(ts) => ts,
        // Unreadable content: report it as ancient so the caller removes it
        // and retries HTTP/3, instead of trusting a file we cannot interpret.
        Err(_) => return Some(Duration::MAX),
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Some(Duration::from_secs(now.saturating_sub(ts)))
}

/// UDP buffer size asked for on the QUIC sockets; see [`raise_quic_socket_buffers`].
pub const QUIC_SOCKET_BUFFER: usize = 4 * 1024 * 1024;

/// Raises `SO_RCVBUF`/`SO_SNDBUF` to [`QUIC_SOCKET_BUFFER`] on every IPv4/IPv6
/// datagram socket this process owns. Call it after building an HTTP/3 client.
///
/// Why: reqwest 0.13's H3 connector binds its own `quinn::Endpoint` on `[::]:0`
/// and offers no knob for the socket, so every QUIC socket kept the kernel default
/// receive buffer (~208 KiB). A download bursting at tens of MB/s over a few ms of
/// scheduling delay overruns that, and the kernel drops the datagrams (thousands
/// counted by `ss -uanem` on the live daemon), which QUIC then treats as loss and
/// backs off from. The kernel caps the value at `net.core.rmem_max`/`wmem_max`;
/// the effective size is read back and logged once.
///
/// reqwest hides the socket, so this finds it: walk `/proc/self/fd`, keep the
/// sockets whose `SO_TYPE` is `SOCK_DGRAM` and `SO_DOMAIN` inet/inet6. Setting a
/// buffer size is idempotent and harmless on any other datagram socket (a resolver's
/// transient one), and an fd that closes or is reused mid-scan just fails a
/// syscall. Runs at client build time — mount setup, or a side client's first use
/// — never on the FUSE thread.
pub fn raise_quic_socket_buffers() {
    use std::os::unix::ffi::OsStrExt;
    static LOGGED: AtomicBool = AtomicBool::new(false);

    fn get_int(fd: libc::c_int, opt: libc::c_int) -> Option<libc::c_int> {
        let mut v: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: `v`/`len` are valid for the duration of the call and sized for
        // an int option; a stale or non-socket fd only makes the call fail.
        let r = unsafe {
            libc::getsockopt(fd, libc::SOL_SOCKET, opt, &mut v as *mut _ as *mut libc::c_void, &mut len)
        };
        (r == 0).then_some(v)
    }
    fn set_int(fd: libc::c_int, opt: libc::c_int, v: libc::c_int) -> bool {
        // SAFETY: as above; the kernel copies the int and keeps no pointer.
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                opt,
                &v as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            ) == 0
        }
    }

    let Ok(dir) = std::fs::read_dir("/proc/self/fd") else { return };
    let want = QUIC_SOCKET_BUFFER as libc::c_int;
    let mut tuned = 0usize;
    let mut effective = None;
    for ent in dir.flatten() {
        let Some(fd) = ent.file_name().to_str().and_then(|n| n.parse::<libc::c_int>().ok()) else {
            continue;
        };
        // Cheap pre-filter before any syscall on the fd: the link reads "socket:[ino]".
        match std::fs::read_link(ent.path()) {
            Ok(target) if target.as_os_str().as_bytes().starts_with(b"socket:") => {}
            _ => continue,
        }
        if get_int(fd, libc::SO_TYPE) != Some(libc::SOCK_DGRAM) {
            continue;
        }
        if !matches!(get_int(fd, libc::SO_DOMAIN), Some(libc::AF_INET) | Some(libc::AF_INET6)) {
            continue;
        }
        let rcv_ok = set_int(fd, libc::SO_RCVBUF, want);
        let snd_ok = set_int(fd, libc::SO_SNDBUF, want);
        if rcv_ok || snd_ok {
            tuned += 1;
            effective = Some((get_int(fd, libc::SO_RCVBUF), get_int(fd, libc::SO_SNDBUF)));
        }
    }
    if let Some((rcv, snd)) = effective {
        if !LOGGED.swap(true, Ordering::Relaxed) {
            // Linux reports twice the size set (it counts its bookkeeping overhead),
            // capped by net.core.rmem_max / wmem_max.
            log::info!(
                "QUIC UDP buffers: asked {} KiB on {} socket(s); kernel reports rcv={:?} snd={:?} bytes \
                 (capped by net.core.rmem_max/wmem_max)",
                QUIC_SOCKET_BUFFER / 1024, tuned, rcv, snd,
            );
        }
    }
}
