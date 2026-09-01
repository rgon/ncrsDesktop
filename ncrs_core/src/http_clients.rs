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

#[derive(Clone)]
pub struct HttpClients {
    /// Preferred clients: HTTP/3 when configured, otherwise clones of the
    /// HTTP/2 pair (a `Client` is `Arc`-based, so the clone shares one pool).
    pref: reqwest::blocking::Client,
    read_pref: reqwest::blocking::Client,
    /// Always-usable HTTP/2 clients, and the target of a demotion.
    h2: reqwest::blocking::Client,
    read_h2: reqwest::blocking::Client,
    demoted: Arc<AtomicBool>,
    http3: bool,
    /// Where a demotion is recorded so the next session starts on HTTP/2
    /// instead of re-discovering the broken transport. `None` = session-only.
    marker: Option<PathBuf>,
}

impl HttpClients {
    /// `pref`/`read_pref` must be the HTTP/3 clients when `http3` is true; pass
    /// clones of the HTTP/2 pair when it is false.
    pub fn new(
        pref: reqwest::blocking::Client,
        read_pref: reqwest::blocking::Client,
        h2: reqwest::blocking::Client,
        read_h2: reqwest::blocking::Client,
        http3: bool,
    ) -> Self {
        HttpClients { pref, read_pref, h2, read_h2, demoted: Arc::new(AtomicBool::new(false)), http3, marker: None }
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

    /// The read client, which deliberately keeps no idle pool (see the comment
    /// at its construction in `lib.rs`).
    pub fn read(&self) -> DavClient {
        if self.demoted.load(Ordering::Relaxed) {
            DavClient { client: self.read_h2.clone(), version: None }
        } else {
            DavClient { client: self.read_pref.clone(), version: self.pref_version() }
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
