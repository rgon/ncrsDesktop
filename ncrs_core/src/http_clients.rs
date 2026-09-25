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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    /// A per-request timeout stamped onto every request, for a client standing in
    /// for one whose client-level timeout it does not share (see `h2_reads`). A
    /// caller's own `.timeout()` afterwards still overrides it.
    timeout: Option<Duration>,
}

impl DavClient {
    fn stamp(&self, rb: reqwest::blocking::RequestBuilder) -> reqwest::blocking::RequestBuilder {
        let rb = match self.version {
            Some(v) => rb.version(v),
            None => rb,
        };
        match self.timeout {
            Some(t) => rb.timeout(t),
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
        DavClient { client, version: h3.then_some(reqwest::Version::HTTP_3), timeout: None }
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

type ReadSetBuilder = dyn Fn() -> Result<Vec<reqwest::blocking::Client>, String> + Send + Sync;

#[derive(Clone)]
pub struct HttpClients {
    /// Preferred clients: HTTP/3 when configured, otherwise clones of the
    /// HTTP/2 set (a `Client` is `Arc`-based, so the clone shares one pool).
    pref: reqwest::blocking::Client,
    /// One read client per download slot; see [`DOWNLOAD_CONNECTIONS`].
    read_pref: Arc<[reqwest::blocking::Client]>,
    /// Always-usable HTTP/2 clients, and the target of a demotion.
    h2: reqwest::blocking::Client,
    /// The HTTP/2 read set. While HTTP/3 is active it is only the demotion target,
    /// so it is built on first use after a demotion (once: `OnceLock`) rather than
    /// holding a runtime thread per slot all session.
    read_h2: Arc<std::sync::OnceLock<Arc<[reqwest::blocking::Client]>>>,
    build_read_h2: Option<Arc<ReadSetBuilder>>,
    /// Serializes building the HTTP/2 read set, so two threads never build two
    /// sets (their runtime threads would briefly exceed the budget).
    build_lock: Arc<std::sync::Mutex<()>>,
    /// Stall bound stamped onto the metadata client while it stands in for reads.
    read_fallback_timeout: Duration,
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
        let built = std::sync::OnceLock::new();
        let _ = built.set(read_h2.into());
        HttpClients {
            pref,
            read_pref: read_pref.into(),
            h2,
            read_h2: Arc::new(built),
            build_read_h2: None,
            build_lock: Arc::new(std::sync::Mutex::new(())),
            // Never used: this set is built up front, so there is no fallback.
            read_fallback_timeout: Duration::ZERO,
            demoted: Arc::new(AtomicBool::new(false)),
            http3,
            marker: None,
        }
    }

    /// An HTTP/3 set whose HTTP/2 read clients are built by `build_read_h2` only
    /// if a demotion ever needs them.
    pub fn with_lazy_h2_reads(
        pref: reqwest::blocking::Client,
        read_pref: Vec<reqwest::blocking::Client>,
        h2: reqwest::blocking::Client,
        build_read_h2: impl Fn() -> Result<Vec<reqwest::blocking::Client>, String> + Send + Sync + 'static,
        read_fallback_timeout: Duration,
    ) -> Self {
        assert!(!read_pref.is_empty(), "a read client set cannot be empty");
        HttpClients {
            pref,
            read_pref: read_pref.into(),
            h2,
            read_h2: Arc::new(std::sync::OnceLock::new()),
            build_read_h2: Some(Arc::new(build_read_h2)),
            build_lock: Arc::new(std::sync::Mutex::new(())),
            read_fallback_timeout,
            demoted: Arc::new(AtomicBool::new(false)),
            http3: true,
            marker: None,
        }
    }

    /// The HTTP/2 read set, building it on first use. A failed build is not
    /// remembered — the next read tries again — and meanwhile `read` falls back to
    /// the metadata client with the stall bound stamped on.
    fn h2_reads(&self) -> Option<&Arc<[reqwest::blocking::Client]>> {
        if let Some(set) = self.read_h2.get() {
            return Some(set);
        }
        let _building = self.build_lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(set) = self.read_h2.get() {
            return Some(set);
        }
        match self.build_read_h2.as_ref().map(|b| b()) {
            Some(Ok(v)) if !v.is_empty() => {
                log::info!("built the {} HTTP/2 read clients for the demotion", v.len());
                let _ = self.read_h2.set(v.into());
                self.read_h2.get()
            }
            Some(Err(e)) => {
                log::warn!("could not build the HTTP/2 read clients ({}) — reads use the metadata client until they can be", e);
                None
            }
            _ => None,
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
            DavClient { client: self.h2.clone(), version: None, timeout: None }
        } else {
            DavClient { client: self.pref.clone(), version: self.pref_version(), timeout: None }
        }
    }

    /// The read client for download slot `slot` (a `read_throttle` permit's
    /// [`slot`](crate::ThrottleGuard::slot)), so each slot always talks over its own
    /// connection. See the construction in `lib.rs` for how the set is tuned.
    pub fn read(&self, slot: usize) -> DavClient {
        // Both sets are the same size; the modulo only keeps a stray index in range.
        if self.demoted.load(Ordering::Relaxed) && self.http3 {
            match self.h2_reads() {
                Some(set) => DavClient { client: set[slot % set.len()].clone(), version: None, timeout: None },
                // The HTTP/2 read clients could not be built (this time). The metadata
                // client stands in, with the read clients' stall bound stamped on as a
                // per-request timeout: its own client-level one is reqwest's 30 s
                // default. Per request that bound is a total deadline, so a long body
                // gets cut and resumed — degraded, but every READ bound still holds.
                None => DavClient { client: self.h2.clone(), version: None, timeout: Some(self.read_fallback_timeout) },
            }
        } else if self.demoted.load(Ordering::Relaxed) {
            // Without HTTP/3 the preferred set already is the HTTP/2 one.
            DavClient { client: self.read_pref[slot % self.read_pref.len()].clone(), version: None, timeout: None }
        } else {
            DavClient { client: self.read_pref[slot % self.read_pref.len()].clone(), version: self.pref_version(), timeout: None }
        }
    }

    /// The HTTP/2 client, for the mount-time probe that decides whether to
    /// demote.
    pub fn h2(&self) -> DavClient {
        DavClient { client: self.h2.clone(), version: None, timeout: None }
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

/// reqwest DNS resolver that runs lookups on the shared, bounded `bg::DNS` pool,
/// with one lookup in flight per host and a short positive cache. Every client the
/// daemon (`ncrs`) builds uses it ([`with_pooled_dns`]); `ncrs-open`
/// (`edit_locally`) and the GUI (`login_flow`) build theirs in their own processes,
/// outside the daemon's thread budget, and keep reqwest's default resolver.
///
/// Why pooled: reqwest's default resolver hands each `getaddrinfo` to its client's
/// own tokio runtime's blocking pool, which can grow to 512 threads, and the daemon
/// runs over a dozen client runtimes. Those threads never appeared in
/// `bg::MAX_THREADS`.
///
/// Why single-flight and cached: the read clients keep no idle TCP connections, so
/// every window, segment and resume connects — and resolves — afresh, and reqwest's
/// connect timeout includes the lookup. With a bare two-thread pool, a burst of
/// them queued behind each other past that timeout (and past the connectivity
/// probe's, whose lookup waited in the same queue), which reads as the network
/// being down. Now a host is looked up at most once at a time, everyone asking
/// meanwhile shares the answer, answers are reused for DNS_FRESH, and a failed
/// lookup falls back to the last good answer (up to DNS_STALE_MAX old). So the pool
/// sees roughly one job per host per DNS_FRESH, and nothing waits behind it.
pub struct PooledResolver {
    hosts: std::sync::Mutex<std::collections::HashMap<String, HostEntry>>,
}

/// How long a successful lookup is reused without asking again.
const DNS_FRESH: Duration = Duration::from_secs(45);
/// How old an answer may be and still stand in for a lookup that failed.
const DNS_STALE_MAX: Duration = Duration::from_secs(3600);

type LookupResult = Result<Vec<std::net::SocketAddr>, String>;

#[derive(Default)]
struct HostEntry {
    last_good: Option<(Instant, Vec<std::net::SocketAddr>)>,
    /// Callers waiting on the lookup in flight; `None` when none is running.
    waiting: Option<Vec<tokio::sync::oneshot::Sender<LookupResult>>>,
}

/// The error a lookup reports when the DNS pool refused it. Typed, so the read
/// path can tell "our own pool is busy" from "the network is down" by walking
/// the error's sources ([`is_dns_refusal`]) instead of trusting reqwest's text,
/// which renders every connect failure as "error sending request".
#[derive(Debug)]
pub struct DnsRefused;

impl std::fmt::Display for DnsRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("name lookup refused: the daemon's lookup pool is full")
    }
}

impl std::error::Error for DnsRefused {}

/// Whether `e` failed because the DNS pool refused the lookup.
pub fn is_dns_refusal(e: &reqwest::Error) -> bool {
    let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
    while let Some(err) = src {
        if err.is::<DnsRefused>() {
            return true;
        }
        src = err.source();
    }
    false
}

impl PooledResolver {
    fn new() -> Self {
        PooledResolver { hosts: std::sync::Mutex::new(std::collections::HashMap::new()) }
    }

    /// Finishes the lookup in flight for `host`: records a good answer, falls back
    /// to the last good one on failure, and answers everyone waiting.
    fn complete(&self, host: &str, result: LookupResult) {
        let mut hosts = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        let entry = hosts.entry(host.to_string()).or_default();
        let answer = match result {
            Ok(addrs) if !addrs.is_empty() => {
                entry.last_good = Some((Instant::now(), addrs.clone()));
                Ok(addrs)
            }
            other => match &entry.last_good {
                Some((at, addrs)) if at.elapsed() < DNS_STALE_MAX => {
                    log::warn!("lookup of {} failed ({:?}) — using the answer from {:?} ago", host, other.err(), at.elapsed());
                    Ok(addrs.clone())
                }
                _ => other.and_then(|v| if v.is_empty() { Err("no addresses".to_string()) } else { Ok(v) }),
            },
        };
        for tx in entry.waiting.take().unwrap_or_default() {
            let _ = tx.send(answer.clone());
        }
    }
}

impl reqwest::dns::Resolve for &'static PooledResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let this: &'static PooledResolver = self;
        let host = name.as_str().to_string();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut start = false;
        let fresh = {
            let mut hosts = this.hosts.lock().unwrap_or_else(|e| e.into_inner());
            let entry = hosts.entry(host.clone()).or_default();
            match &entry.last_good {
                Some((at, addrs)) if at.elapsed() < DNS_FRESH => Some(addrs.clone()),
                _ => {
                    match entry.waiting.as_mut() {
                        Some(w) => w.push(tx),
                        None => {
                            entry.waiting = Some(vec![tx]);
                            start = true;
                        }
                    }
                    None
                }
            }
        };
        if let Some(addrs) = fresh {
            return Box::pin(async move { Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs) });
        }
        if start {
            let h = host.clone();
            let submitted = crate::bg::DNS.submit(move || {
                use std::net::ToSocketAddrs;
                // Everyone who asked may have given up already (their request timed
                // out): then there is nobody to answer, and no reason to block a pool
                // thread on getaddrinfo for them.
                let anyone = this.hosts.lock().unwrap_or_else(|e| e.into_inner())
                    .get(&h)
                    .and_then(|e| e.waiting.as_ref())
                    .is_some_and(|w| w.iter().any(|tx| !tx.is_closed()));
                if !anyone {
                    this.complete(&h, Err("lookup abandoned".into()));
                    return;
                }
                // Port 0: reqwest substitutes the URL's port.
                let result = (h.as_str(), 0)
                    .to_socket_addrs()
                    .map(|it| it.collect::<Vec<_>>())
                    .map_err(|e| e.to_string());
                this.complete(&h, result);
            });
            if submitted.is_err() {
                // Answer the waiters now: a stale answer if there is one, else the
                // typed refusal.
                this.complete(&host, Err("refused".into()));
            }
        }
        Box::pin(async move {
            match rx.await {
                Ok(Ok(addrs)) => Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs),
                Ok(Err(e)) if e == "refused" => Err(Box::new(DnsRefused) as Box<dyn std::error::Error + Send + Sync>),
                Ok(Err(e)) => Err(e.into()),
                Err(_) => Err("DNS lookup dropped".into()),
            }
        })
    }
}

/// `builder` with the process-wide [`PooledResolver`].
pub fn with_pooled_dns(builder: reqwest::blocking::ClientBuilder) -> reqwest::blocking::ClientBuilder {
    static RESOLVER: std::sync::OnceLock<PooledResolver> = std::sync::OnceLock::new();
    let r: &'static PooledResolver = RESOLVER.get_or_init(PooledResolver::new);
    builder.dns_resolver(Arc::new(r))
}
