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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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
        HttpClients { pref, read_pref, h2, read_h2, demoted: Arc::new(AtomicBool::new(false)), http3 }
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

    /// Latch onto HTTP/2 for the rest of the session. Logs once.
    pub fn demote(&self) {
        if !self.demoted.swap(true, Ordering::Relaxed) {
            log::warn!(
                "HTTP/3 unusable (QUIC failed where HTTP/2 succeeded) — \
                 falling back to HTTP/2 for the rest of this session; \
                 set `http3: false` in config.yaml to skip this probe on startup"
            );
        }
    }
}
