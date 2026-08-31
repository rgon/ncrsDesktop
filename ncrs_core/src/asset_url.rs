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
