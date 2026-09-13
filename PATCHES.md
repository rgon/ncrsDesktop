# PATCHES.md — upstream fixes worth submitting to reqwest

Findings from the 2026-09-01 HTTP/3 investigation (see `GOTCHAS.md` §3). ncrs
works around all of them in-tree (`http_clients::DavClient` stamps request
versions — commit `390b1eb`; `build_pair` in `lib.rs` rebuilds the read
client's fail-fast guarantees with h3 knobs), so nothing here blocks us. But
two of these are genuine upstream defects that other reqwest users are silently
hitting, and both come with a small test that fails today and passes patched.

**Target:** `seanmonstar/reqwest`, affected version `0.13.4` (latest at time of
writing). HTTP/3 support is feature-gated: build and test with

```bash
RUSTFLAGS="--cfg reqwest_unstable" cargo test --features http3 --test http3
```

Tests live in `tests/http3.rs`; `support::server::Http3` is the in-repo h3
test server. Before opening a PR, search the issue tracker for
`http3_prior_knowledge version` and `http3 keep_alive` — link this analysis
either way.

---

## Patch 1 — `http3_prior_knowledge()` never actually sends HTTP/3

### The defect

`ClientBuilder::http3_prior_knowledge()` (src/async_impl/client.rs:1571) only
sets `HttpVersionPref::Http3`, which *builds* the QUIC connector. Request
routing (src/async_impl/client.rs:2639) picks the h3 client **only when the
individual request carries `http::Version::HTTP_3`**:

```rust
let in_flight = match version {
    #[cfg(feature = "http3")]
    http::Version::HTTP_3 if self.inner.h3_client.is_some() => { /* h3 */ }
    _ => { /* hyper over TCP */ }
};
```

A plain `client.get(url).send()` defaults to `HTTP_11` and rides hyper over
TCP. Worse, with the h3 pref reqwest sets **no ALPN** on that TCP path
(client.rs:832-834, "h3 ALPN is not valid over TCP"), so the "HTTP/3 client"
negotiates HTTP/1.1 — not even h2. Nothing fails, nothing warns; the caller
believes they are on QUIC.

That reqwest's own `tests/http3.rs` adds `.version(http::Version::HTTP_3)` to
every request shows the contract exists — it is just documented nowhere on
`http3_prior_knowledge()`, whose name and docs ("Only use HTTP/3") promise the
opposite of what a bare request does.

Field evidence: ncrs shipped `http3_prior_knowledge()` clients from 2026-05-15
(`ec801f1`) to 2026-09-01 (`390b1eb`) — roughly 3.5 months — with every request
on HTTP/1.1, including the reachability probes whose "QUIC failed where HTTP/2
succeeded" comparisons were therefore two TCP probes racing a network blip.

### The failing test (add to `tests/http3.rs`)

```rust
#[tokio::test]
async fn http3_prior_knowledge_routes_requests_over_http3() {
    let server = server::Http3::new().build(|_| async { http::Response::default() });
    let url = format!("https://{}/text", server.addr());

    let res = reqwest::Client::builder()
        .http3_prior_knowledge()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("client builder")
        .get(url)
        // Deliberately NO .version(http::Version::HTTP_3): the builder said
        // "only use HTTP/3", so a bare request must already be HTTP/3.
        .send()
        .await
        .expect("request");

    assert_eq!(res.version(), http::Version::HTTP_3);
}
```

On 0.13.4 this fails at `send()` (the TCP request never reaches the UDP-only
test server); against a dual-stack server it fails the version assertion with
`HTTP/1.1`. With the patch below it passes.

Standalone repro outside reqwest's harness: `ncrs_core/examples/h3probe.rs`
against any h3-capable server — stock 0.13.4 client prints `HTTP/1.1 200 OK`,
`--v3` (per-request version stamp) prints `HTTP/3.0 200 OK`.

### The fix

Route by builder pref, not only by per-request version. Sketch:

```rust
// src/async_impl/client.rs — ClientRef gains:
//     h3_prior_knowledge: bool,   // set from HttpVersionPref::Http3 at build()

let in_flight = match version {
    #[cfg(feature = "http3")]
    http::Version::HTTP_3 if self.inner.h3_client.is_some() => { /* h3, as today */ }
    // A prior-knowledge client has no TCP transport worth speaking of (no ALPN
    // is configured for it), so bare requests must go over QUIC too. `version`
    // here is hyper's HTTP_11 default, indistinguishable from an explicit h1
    // request — acceptable: "prior knowledge" means there is nothing else.
    #[cfg(feature = "http3")]
    _ if self.inner.h3_prior_knowledge && self.inner.h3_client.is_some() => { /* h3 */ }
    _ => { /* hyper over TCP */ }
};
```

Fallback position if maintainers consider the routing behaviour intentional: a
docs patch on `http3_prior_knowledge()` stating that every request must set
`.version(http::Version::HTTP_3)`, plus a `debug_assert!`/warn when a
prior-knowledge client sends a non-h3 request. Either outcome removes the trap.

---

## Patch 2 — no way to enable QUIC keep-alives

### The gap

quinn supports `TransportConfig::keep_alive_interval` (PING frames that keep
NAT/firewall mappings warm and detect a silently dead path before
`max_idle_timeout`). reqwest plumbs several `TransportConfig` knobs —
`http3_max_idle_timeout`, receive/send windows, BBR — but not keep-alive, and
quinn's default is `None` (quinn-proto `config/transport.rs:381`).

Consequence for long-lived clients: after a NAT rebind or uplink switch, a
pooled QUIC connection is locally "alive" but blackholed. Every request rides
it into a timeout until `max_idle_timeout` (default 30 s) finally kills it,
while a TCP pool self-heals in one round trip (RST + hyper's reused-connection
retry). Keep-alives close most of that asymmetry.

### The fix (mechanical)

```rust
// src/async_impl/client.rs
// Config (~line 250):            quic_keep_alive_interval: Option<Duration>,
// Default (~line 377):           quic_keep_alive_interval: None,

/// Set the QUIC keep-alive interval (`quinn`'s
/// `TransportConfig::keep_alive_interval`). Defaults to disabled.
#[cfg(feature = "http3")]
pub fn http3_keep_alive_interval(mut self, value: Duration) -> ClientBuilder {
    self.config.quic_keep_alive_interval = Some(value);
    self
}

// build_h3_connector closure (~line 457): accept the option and apply it —
//     if let Some(interval) = quic_keep_alive_interval {
//         transport_config.keep_alive_interval(Some(interval));
//     }
// and pass `config.quic_keep_alive_interval` at both call sites (~648, ~852).
```

### The proving test (add to `tests/http3.rs`)

Discriminator: a server that accepts exactly **one** QUIC connection, then
stops accepting. Without keep-alives the client's connection idles out and the
second request needs a redial, which the server refuses; with keep-alives the
original connection outlives the silence and the second request succeeds.

```rust
#[tokio::test]
async fn http3_keep_alive_outlives_idle_timeout() {
    // support::server::Http3 would need an `accept_once()` mode (drop the
    // endpoint's Incoming after the first connection); ~10 lines.
    let server = server::Http3::new()
        .accept_once()
        .build(|_| async { http::Response::default() });
    let url = format!("https://{}/text", server.addr());

    let client = reqwest::Client::builder()
        .http3_prior_knowledge()
        .danger_accept_invalid_certs(true)
        .http3_max_idle_timeout(Duration::from_millis(500))
        .http3_keep_alive_interval(Duration::from_millis(100)) // the patch
        .build()
        .expect("client builder");

    let go = |c: &reqwest::Client| c.get(&url).version(http::Version::HTTP_3).send();

    go(&client).await.expect("first request");
    tokio::time::sleep(Duration::from_secs(2)).await; // >> idle timeout
    // Fails on 0.13.4 (connection idled out at 500ms; the redial is refused);
    // passes with keep-alives holding the original connection open.
    go(&client).await.expect("second request on the kept-alive connection");
}
```

Running the same body with the keep-alive line removed documents the
pre-patch behaviour (second request errors), i.e. the issue exists.

Note for ncrs: measurements on 2026-09-01 (`h3probe`) showed QUIC loss
recovery riding out a 9 s UDP blackhole on its own and clean pool redials
across the 30 s/90 s idle boundaries, so we do **not** currently carry a
`[patch.crates-io]` fork for this. File it upstream; adopt the knob for the
read client if the field shows silent-path-death windows in practice.

---

## Related issues worth filing (report, no patch prepared)

1. **`connect_timeout` never reaches the QUIC connector** — nothing in
   `src/async_impl/h3_client/` consults it, so an h3 dial on a blackholed path
   is bounded only by per-request timeouts (or quinn's handshake/idle timeout).
   TCP dials fail in `connect_timeout`; h3 dials should match.
2. **The h3 pool ignores `pool_max_idle_per_host`** —
   `src/async_impl/h3_client/pool.rs` keeps one connection per authority
   regardless; a client built with `pool_max_idle_per_host(0)` (the
   "always connect fresh" pattern) silently pools over h3. Honouring `0`, or
   documenting the divergence, would do.

## Submission checklist

- [ ] Search reqwest issues/PRs for prior art on each item; link them.
- [ ] Fork `seanmonstar/reqwest`, one branch per patch
      (`http3-prior-knowledge-routing`, `http3-keep-alive`).
- [ ] `RUSTFLAGS="--cfg reqwest_unstable" cargo test --features http3 --test http3`
      — new tests red on master, green with the patch.
- [ ] PR description: link the failing test, quote the field evidence above
      (3.5 months of an h3-configured client speaking HTTP/1.1 unnoticed).
- [ ] After either patch lands: drop the corresponding workaround note from
      `GOTCHAS.md` §3 and, for Patch 1, the stamp in `DavClient` can become a
      no-op on that reqwest version.
