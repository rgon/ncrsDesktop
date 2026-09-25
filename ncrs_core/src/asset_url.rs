//! Resolution of server-supplied asset URLs that end up in the webview.
//!
//! Nextcloud hands us URL strings for things the GUI renders directly — unified
//! search result icons and thumbnails, notification icons. Those strings reach
//! `<img src=…>`, so whatever host they name is a host the user's browser engine
//! will contact: it discloses the user's IP and the fact that they are running
//! this app to a third party the user never configured, and it does so silently,
//! on every search keystroke or notification poll.
//!
//! No credential is attached, so this is a privacy leak rather than a credential
//! leak — but there is also no legitimate reason for an asset the GUI renders to
//! live anywhere except the configured server. So assets are pinned to the
//! server's origin here.
//!
//! Note the deliberate asymmetry with a *link*: `resourceUrl` on a search hit is
//! handed to the user's browser when they click it, and pointing off-site is a
//! real feature there (the Bookmarks app returns the bookmarked address). Links
//! keep going through [`crate::search::absolutize`] and the scheme allowlist in
//! the GUI's `open_link`; only rendered assets are pinned.

/// Resolves a server-supplied asset URL, returning an empty string for anything
/// that is not on the configured server.
///
/// - empty in, empty out
/// - server-relative (`/apps/files/img/app.svg`) → prefixed with `base`
/// - absolute on the same origin as `base` → kept
/// - anything else (foreign host, other scheme, unparseable) → dropped
pub fn same_origin_asset(base: &str, url: &str) -> String {
    if url.is_empty() {
        return String::new();
    }
    // A data: URI carries its own bytes and contacts nothing, so it is safe and
    // is what the server uses for small inline provider icons.
    if url.starts_with("data:image/") {
        return url.to_string();
    }
    if url.starts_with('/') {
        // Protocol-relative (`//host/x`) is NOT server-relative: it inherits the
        // scheme and names its own host.
        if url.starts_with("//") {
            return String::new();
        }
        return format!("{}{}", base.trim_end_matches('/'), url);
    }
    match (url::Url::parse(base), url::Url::parse(url)) {
        (Ok(b), Ok(u)) => {
            let same_host = match (b.host_str(), u.host_str()) {
                (Some(bh), Some(uh)) => uh.eq_ignore_ascii_case(bh),
                _ => false,
            };
            if same_host
                && u.scheme() == b.scheme()
                && u.port_or_known_default() == b.port_or_known_default()
            {
                url.to_string()
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}


// -- Inlining assets so the webview never makes the request --------------------
//
// Pinning an asset to the server's origin (above) stops the *leak*, but the
// webview still has to fetch it, which means the CSP has to permit some remote
// origin — and the only legitimate one is the user's own server, which a static
// `img-src` cannot name. So instead of widening the policy, the daemon fetches
// the asset itself and hands the GUI a `data:` URI. The webview then contacts
// nothing, `img-src 'self' data:` is enough, and the policy fails *closed*: if
// anything here goes wrong the image is simply absent rather than fetched from
// somewhere unexpected.
//
// This also makes icons work on servers that require authentication for them,
// which a bare `<img src>` in the webview never could.

/// Ceiling on an inlined asset. These are app icons and avatars; anything
/// larger is not one, and a data: URI costs ~4/3 its bytes in the IPC payload.
const MAX_ASSET_BYTES: usize = 256 * 1024;

/// How many resolved assets to remember. Icons repeat heavily — typically one
/// per search provider — so a small cache turns a per-keystroke fetch storm into
/// one request per distinct icon per session.
const CACHE_CAP: usize = 512;

const ASSET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, Option<String>>> {
    static C: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, Option<String>>>,
    > = std::sync::OnceLock::new();
    C.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Content types allowed to be inlined.
///
/// SVG is included deliberately: script inside an SVG does **not** execute when
/// the SVG is the source of an `<img>`, which is the only way these are rendered.
fn is_inlinable_image(content_type: &str) -> bool {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    matches!(
        ct.as_str(),
        "image/png"
            | "image/jpeg"
            | "image/gif"
            | "image/webp"
            | "image/svg+xml"
            | "image/bmp"
            | "image/x-icon"
            | "image/vnd.microsoft.icon"
    )
}

/// Builds a `data:` URI, or `None` if the type is not an inlinable image or the
/// payload is too large / empty.
pub fn to_data_uri(content_type: &str, bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() || bytes.len() > MAX_ASSET_BYTES || !is_inlinable_image(content_type) {
        return None;
    }
    let ct = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    use base64::Engine as _;
    Some(format!(
        "data:{};base64,{}",
        ct,
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

/// Resolves a server-supplied asset URL to something the webview can render
/// without contacting anyone: a `data:` URI, or an empty string.
///
/// Failures are deliberately quiet — a missing icon is cosmetic, and the GUI
/// already renders nothing when the field is empty.
pub fn inline_asset(
    client: &crate::http_clients::DavClient,
    base: &str,
    creds: &crate::auth::Credentials,
    raw_url: &str,
) -> String {
    // Origin check first: never issue a request for something off the server.
    let url = same_origin_asset(base, raw_url);
    if url.is_empty() {
        return String::new();
    }
    // Already inline.
    if url.starts_with("data:") {
        return url;
    }

    if let Ok(c) = cache().lock() {
        if let Some(hit) = c.get(&url) {
            return hit.clone().unwrap_or_default();
        }
    }

    let resp = match creds.apply(client.get(&url).timeout(ASSET_TIMEOUT)).send() {
        Ok(r) => r,
        // Transient: do NOT cache, or one blip hides icons for the whole session.
        Err(e) => {
            log::debug!("asset inline: request failed for {}: {}", url, e);
            return String::new();
        }
    };
    let status = resp.status();
    if !status.is_success() {
        if status.is_server_error() {
            return String::new();
        }
        // 4xx is a property of the asset, so remember it.
        remember(&url, None);
        return String::new();
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = match resp.bytes() {
        Ok(b) => b,
        Err(_) => return String::new(),
    };

    let data_uri = to_data_uri(&content_type, &bytes);
    if data_uri.is_none() {
        log::debug!(
            "asset inline: refusing {} ({} bytes, type {:?})",
            url,
            bytes.len(),
            content_type
        );
    }
    remember(&url, data_uri.clone());
    data_uri.unwrap_or_default()
}

fn remember(url: &str, value: Option<String>) {
    if let Ok(mut c) = cache().lock() {
        if c.len() < CACHE_CAP || c.contains_key(url) {
            c.insert(url.to_string(), value);
        }
    }
}


/// Shared client for asset fetches, mirroring `notifications::client`: building
/// one per call would spawn a tokio thread and a fresh rustls root store each
/// time.
fn asset_client() -> crate::http_clients::DavClient {
    static C: std::sync::OnceLock<reqwest::blocking::Client> = std::sync::OnceLock::new();
    let raw = C.get_or_init(|| {
        crate::http_clients::with_pooled_dns(reqwest::blocking::Client::builder())
            .timeout(ASSET_TIMEOUT)
            .build()
            .expect("reqwest client")
    });
    // Plain TCP: one-off asset fetches gain nothing from QUIC and this keeps
    // the avatar path independent of the transport experiment entirely.
    crate::http_clients::DavClient::new(raw.clone(), false)
}

/// Fetches the account's avatar as a `data:` URI.
///
/// The GUI used to render `<img src="{server}/index.php/avatar/{user}/64">`
/// directly. That stopped working the moment the webview got a real CSP, because
/// `img-src 'self' data:` (deliberately) forbids reaching out to the server — the
/// avatar silently fell back to the initials placeholder. Inlining it here is the
/// same treatment search and notification icons already get.
pub fn avatar_data_uri(
    base_url: &str,
    creds: &crate::auth::Credentials,
    username: &str,
    size: u32,
) -> Option<String> {
    let encoded = percent_encoding::utf8_percent_encode(
        username,
        percent_encoding::NON_ALPHANUMERIC,
    );
    let url = format!("{}/index.php/avatar/{}/{}", base_url.trim_end_matches('/'), encoded, size);
    let inlined = inline_asset(&asset_client(), base_url, creds, &url);
    if inlined.is_empty() { None } else { Some(inlined) }
}

#[cfg(test)]
mod tests {
    use super::same_origin_asset;

    const BASE: &str = "https://cloud.example.com";

    #[test]
    fn keeps_assets_on_the_configured_server() {
        assert_eq!(
            same_origin_asset(BASE, "/apps/files/img/app.svg"),
            "https://cloud.example.com/apps/files/img/app.svg",
        );
        assert_eq!(
            same_origin_asset(BASE, "https://cloud.example.com/avatar/alice/32"),
            "https://cloud.example.com/avatar/alice/32",
        );
        // Host match is case-insensitive, as DNS is.
        assert_eq!(
            same_origin_asset(BASE, "https://Cloud.Example.COM/x.png"),
            "https://Cloud.Example.COM/x.png",
        );
        // Inline icons contact nothing.
        assert_eq!(
            same_origin_asset(BASE, "data:image/svg+xml;base64,PHN2Zy8+"),
            "data:image/svg+xml;base64,PHN2Zy8+",
        );
        assert_eq!(same_origin_asset(BASE, ""), "");
    }

    #[test]
    fn drops_assets_pointing_off_the_configured_server() {
        for hostile in [
            "https://tracker.example/pixel.png",
            // Suffix lookalike: naive `starts_with`/`contains` checks pass this.
            "https://cloud.example.com.tracker.example/p.png",
            // Prefix lookalike.
            "https://evil-cloud.example.com/p.png",
            // The base host appearing only in a query or userinfo position.
            "https://tracker.example/p.png?from=cloud.example.com",
            "https://cloud.example.com@tracker.example/p.png",
            // Protocol-relative borrows the scheme but names a foreign host.
            "//tracker.example/p.png",
            // Scheme downgrade on the right host still leaks over cleartext.
            "http://cloud.example.com/p.png",
            // A different port is a different origin for a rendered asset.
            "https://cloud.example.com:8443/p.png",
            // Non-image schemes have no business in an <img src>.
            "javascript:alert(1)",
            "file:///etc/passwd",
            "data:text/html,<script>alert(1)</script>",
            "not a url",
        ] {
            assert_eq!(same_origin_asset(BASE, hostile), "", "{} must be dropped", hostile);
        }
    }

    #[test]
    fn a_plain_http_server_keeps_its_own_assets() {
        // Loopback/dev and the Docker e2e run the server over http.
        let base = "http://127.0.0.1:18087";
        assert_eq!(
            same_origin_asset(base, "/apps/x/icon.svg"),
            "http://127.0.0.1:18087/apps/x/icon.svg",
        );
        assert_eq!(
            same_origin_asset(base, "http://127.0.0.1:18087/a.png"),
            "http://127.0.0.1:18087/a.png",
        );
        assert_eq!(same_origin_asset(base, "http://tracker.example/p.png"), "");
    }
}

#[cfg(test)]
mod inline_tests {
    use super::{is_inlinable_image, to_data_uri, MAX_ASSET_BYTES};

    #[test]
    fn builds_a_data_uri_for_real_image_types() {
        // 1x1 GIF.
        let gif = b"GIF89a\x01\x00\x01\x00\x00\xff\x00,";
        let uri = to_data_uri("image/gif", gif).expect("gif inlines");
        assert!(uri.starts_with("data:image/gif;base64,"), "{}", uri);
        // Parameters on the header are stripped.
        let uri = to_data_uri("image/svg+xml; charset=utf-8", b"<svg/>").expect("svg inlines");
        assert!(uri.starts_with("data:image/svg+xml;base64,"), "{}", uri);
        // Case-insensitive.
        assert!(to_data_uri("IMAGE/PNG", b"x").is_some());
    }

    #[test]
    fn refuses_non_images_and_oversized_payloads() {
        // An HTML or script body must never become an <img> source.
        assert_eq!(to_data_uri("text/html", b"<script>alert(1)</script>"), None);
        assert_eq!(to_data_uri("application/javascript", b"alert(1)"), None);
        assert_eq!(to_data_uri("", b"x"), None);
        // Empty and oversized.
        assert_eq!(to_data_uri("image/png", b""), None);
        let huge = vec![0u8; MAX_ASSET_BYTES + 1];
        assert_eq!(to_data_uri("image/png", &huge), None);
        // Exactly at the cap is allowed.
        let at_cap = vec![0u8; MAX_ASSET_BYTES];
        assert!(to_data_uri("image/png", &at_cap).is_some());
    }

    #[test]
    fn image_type_allowlist_is_closed() {
        for ok in ["image/png", "image/jpeg", "image/svg+xml", "image/x-icon"] {
            assert!(is_inlinable_image(ok), "{}", ok);
        }
        for bad in ["text/html", "image/", "application/octet-stream", "", "imagepng"] {
            assert!(!is_inlinable_image(bad), "{}", bad);
        }
    }
}
