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
//! So we build both pairs up front and keep a latch. The connectivity probe is
//! the arbiter: when it fails over HTTP/3 but succeeds over HTTP/2, it calls
//! [`HttpClients::demote`] and every caller transparently switches for the rest
//! of the session. Demotion is one-way on purpose — flapping between transports
//! would be worse than staying on the one we have proven works.

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
                    "HTTP/3 was found unusable on this network {}h ago — starting on HTTP/2 \
                     (retrying HTTP/3 in ~{}h; delete {} or set `http3: false` to decide manually)",
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

    /// The metadata/write client every caller should use.
    pub fn get(&self) -> &reqwest::blocking::Client {
        if self.demoted.load(Ordering::Relaxed) { &self.h2 } else { &self.pref }
    }

    /// The read client, which deliberately keeps no idle pool (see the comment
    /// at its construction in `lib.rs`).
    pub fn read(&self) -> &reqwest::blocking::Client {
        if self.demoted.load(Ordering::Relaxed) { &self.read_h2 } else { &self.read_pref }
    }

    /// The HTTP/2 client, for the probe that decides whether to demote.
    pub fn h2(&self) -> &reqwest::blocking::Client {
        &self.h2
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
                "HTTP/3 unusable (QUIC failed where HTTP/2 succeeded) — \
                 falling back to HTTP/2 for the rest of this session; \
                 set `http3: false` in config.yaml to skip this probe on startup"
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
