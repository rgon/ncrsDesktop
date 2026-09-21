use reqwest::blocking::Client;
use serde::Deserialize;
use std::time::Duration;

pub struct LoginFlowInit {
    pub login_url: String,
    pub poll_token: String,
    pub poll_endpoint: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoginResult {
    pub server: String,
    #[serde(rename = "loginName")]
    pub login_name: String,
    #[serde(rename = "appPassword")]
    pub app_password: String,
}

#[derive(Deserialize)]
struct InitResponse {
    poll: PollInfo,
    login: String,
}

#[derive(Deserialize)]
struct PollInfo {
    token: String,
    endpoint: String,
}

fn build_client() -> Result<Client, String> {
    Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())
}

/// True when `host` names this machine — the only place a plaintext
/// connection cannot be redirected or intercepted between here and the
/// server, since it never leaves the box.
pub(crate) fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// Rejects a server URL that would send credentials over plaintext to a
/// real network host. `https://` is always fine. `http://` is only fine
/// when the host is loopback (the daemon talking to a server on the same
/// machine, e.g. local dev or the Docker e2e suite) — anywhere else, a
/// plaintext connection can be read or MITM'd by anything on the path, so it
/// takes an explicit `allow_insecure_http: true` opt-in in the config file
/// (the interactive login flow never allows it, and has none to opt into).
pub fn validate_server_scheme(server_url: &str, allow_insecure_http: bool) -> Result<(), String> {
    let url = url::Url::parse(base_server(server_url))
        .map_err(|e| format!("server URL {:?} is not a valid URL: {}", server_url, e))?;
    match url.scheme() {
        "https" => Ok(()),
        "http" => {
            let host = url.host_str().unwrap_or("");
            if is_loopback_host(host) || allow_insecure_http {
                Ok(())
            } else {
                Err(format!(
                    "refusing to use plaintext http:// for {:?} — credentials would be sent unencrypted; \
                     only loopback addresses (127.0.0.1, ::1, localhost) may use http, \
                     use https:// or set allow_insecure_http if you really mean it",
                    server_url
                ))
            }
        }
        other => Err(format!(
            "unsupported URL scheme {:?} in {:?} — use https://",
            other, server_url
        )),
    }
}

/// Strip any path that shouldn't be part of the server base URL.
/// Handles cases where the user pastes a full WebDAV or index.php URL.
fn base_server(server_url: &str) -> &str {
    let s = server_url.trim_end_matches('/');
    // Strip /remote.php/…, /webdav/…, /index.php/…, /dav/…
    for prefix in ["/remote.php", "/index.php", "/webdav", "/dav/"] {
        if let Some(i) = s.find(prefix) {
            return &s[..i];
        }
    }
    s
}

pub fn init_login_flow(server_url: &str) -> Result<LoginFlowInit, String> {
    // The interactive login flow has no config to opt into plaintext, so this
    // never allows it — a real server must be https.
    validate_server_scheme(server_url, false)?;
    let server = base_server(server_url);
    let url = format!("{}/index.php/login/v2", server);
    let client = build_client()?;
    let resp = client
        .post(&url)
        .header("OCS-APIRequest", "true")
        .header("User-Agent", "NCRS Desktop")
        .send()
        .map_err(|e| format!("login flow init: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!(
            "server returned HTTP {} — check the server URL",
            resp.status()
        ));
    }
    let init: InitResponse = resp
        .json()
        .map_err(|e| format!("login flow parse: {}", e))?;
    Ok(LoginFlowInit {
        login_url: init.login,
        poll_token: init.poll.token,
        poll_endpoint: init.poll.endpoint,
    })
}

/// Returns None when the user has not yet authorized the app (HTTP 404 from Nextcloud).
pub fn poll_login_flow(endpoint: &str, token: &str) -> Result<Option<LoginResult>, String> {
    let client = build_client()?;
    let body = format!("token={}", percent_encoding::utf8_percent_encode(token, percent_encoding::NON_ALPHANUMERIC));
    let resp = client
        .post(endpoint)
        .header("OCS-APIRequest", "true")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .map_err(|e| format!("poll: {}", e))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(format!("poll: HTTP {}", resp.status()));
    }
    let result: LoginResult = resp
        .json()
        .map_err(|e| format!("poll parse: {}", e))?;
    Ok(Some(result))
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ThemeColors {
    pub color: String,
    pub color_text: String,
}

/// Fetch the Nextcloud server theme color from the public capabilities endpoint.
/// Returns None if the theming app is disabled or the request fails.
pub fn fetch_server_theme(server_url: &str) -> Option<ThemeColors> {
    let server = base_server(server_url);
    let url = format!("{}/ocs/v1.php/cloud/capabilities?format=json", server);
    let client = build_client().ok()?;
    let resp = client
        .get(&url)
        .header("OCS-APIRequest", "true")
        .send()
        .ok()?;
    let v: serde_json::Value = resp.json().ok()?;
    let theming = v.pointer("/ocs/data/capabilities/theming")?;
    Some(ThemeColors {
        color: theming["color"].as_str()?.to_string(),
        color_text: theming["color-text"].as_str()?.to_string(),
    })
}

/// Rejects a `server` field returned by the login-flow poll response if it
/// does not live on the same host as the server the user originally pointed
/// the client at, or if it would downgrade an HTTPS request to plaintext.
///
/// The poll response's `server` field goes on to become the base URL for
/// every future authenticated WebDAV request, carrying the freshly-issued
/// app password (see `webdav_url`). Mirrors the host/scheme pinning
/// `notify_push::validate_endpoint` already applies to the notify_push
/// capability, for the same reason: nothing about a value in a server
/// response is trustworthy just because it arrived over an authenticated
/// connection to *some* server.
pub fn validate_login_server(returned_server: &str, requested_server: &str) -> Result<(), String> {
    // Independent of what was requested: the returned server goes on to carry
    // the app password, so it must never be a plaintext connection to a real
    // host, even if `requested_server` somehow was too.
    validate_server_scheme(returned_server, false)?;

    let requested = url::Url::parse(base_server(requested_server))
        .map_err(|e| format!("requested server URL is not a valid URL: {}", e))?;
    let returned = url::Url::parse(base_server(returned_server))
        .map_err(|e| format!("login flow returned an unparseable server {:?}: {}", returned_server, e))?;

    if requested.scheme() == "https" && returned.scheme() != "https" {
        return Err(format!(
            "login flow returned server {:?} over {:?} while {:?} was requested over https — refusing to send the app password over it",
            returned_server, returned.scheme(), requested_server
        ));
    }

    let requested_host = requested.host_str().unwrap_or("");
    let returned_host = returned.host_str().unwrap_or("");
    if requested_host.is_empty() || !returned_host.eq_ignore_ascii_case(requested_host) {
        return Err(format!(
            "login flow returned server on host {:?}, which is not the requested server {:?} — refusing to send the app password to it",
            returned_host, requested_host
        ));
    }

    Ok(())
}

/// Build the WebDAV files URL from the server and login_name returned by the login flow.
pub fn webdav_url(server: &str, login_name: &str) -> String {
    let server = base_server(server);
    let encoded = percent_encoding::utf8_percent_encode(login_name, percent_encoding::NON_ALPHANUMERIC).to_string();
    format!("{}/remote.php/dav/files/{}/", server, encoded)
}

/// Normalize a user-supplied `url` config field to the canonical
/// `https://server/remote.php/dav/files/USERNAME/` form.
///
/// Accepts bare domains, sub-path installs, and any partial NC path prefix so
/// users can paste any format and the daemon will connect correctly.  When
/// `username` is empty the URL is returned unchanged (the caller must validate).
pub fn normalize_webdav_url(url: &str, username: &str) -> String {
    let url = url.trim();
    if url.is_empty() || username.trim().is_empty() {
        return url.to_string();
    }
    webdav_url(url, username)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_bare_domain() {
        assert_eq!(
            normalize_webdav_url("https://cloud.example.com", "alice"),
            "https://cloud.example.com/remote.php/dav/files/alice/",
        );
    }

    #[test]
    fn normalize_bare_domain_trailing_slash() {
        assert_eq!(
            normalize_webdav_url("https://cloud.example.com/", "alice"),
            "https://cloud.example.com/remote.php/dav/files/alice/",
        );
    }

    #[test]
    fn normalize_subpath_install() {
        assert_eq!(
            normalize_webdav_url("https://example.com/nextcloud", "alice"),
            "https://example.com/nextcloud/remote.php/dav/files/alice/",
        );
    }

    #[test]
    fn normalize_partial_remote_php() {
        assert_eq!(
            normalize_webdav_url("https://cloud.example.com/remote.php", "alice"),
            "https://cloud.example.com/remote.php/dav/files/alice/",
        );
    }

    #[test]
    fn normalize_partial_remote_php_dav() {
        assert_eq!(
            normalize_webdav_url("https://cloud.example.com/remote.php/dav", "alice"),
            "https://cloud.example.com/remote.php/dav/files/alice/",
        );
    }

    #[test]
    fn normalize_full_url_idempotent() {
        let full = "https://cloud.example.com/remote.php/dav/files/alice/";
        assert_eq!(normalize_webdav_url(full, "alice"), full);
    }

    #[test]
    fn normalize_subpath_full_url_idempotent() {
        let full = "https://example.com/nextcloud/remote.php/dav/files/alice/";
        assert_eq!(normalize_webdav_url(full, "alice"), full);
    }

    #[test]
    fn normalize_special_chars_in_username() {
        let result = normalize_webdav_url("https://cloud.example.com", "alice@domain.com");
        assert_eq!(result, "https://cloud.example.com/remote.php/dav/files/alice%40domain%2Ecom/");
    }

    #[test]
    fn normalize_empty_username_no_change() {
        let url = "https://cloud.example.com";
        assert_eq!(normalize_webdav_url(url, ""), url);
    }

    #[test]
    fn normalize_empty_url_no_change() {
        assert_eq!(normalize_webdav_url("", "alice"), "");
    }

    #[test]
    fn normalize_index_php_prefix() {
        assert_eq!(
            normalize_webdav_url("https://cloud.example.com/index.php/login/v2", "alice"),
            "https://cloud.example.com/remote.php/dav/files/alice/",
        );
    }

    #[test]
    fn login_server_same_host_accepted() {
        assert!(validate_login_server(
            "https://cloud.example.com",
            "https://cloud.example.com",
        ).is_ok());
        // A path suffix on the returned value (canonicalisation) is fine.
        assert!(validate_login_server(
            "https://cloud.example.com/index.php/login/v2",
            "https://cloud.example.com",
        ).is_ok());
    }

    #[test]
    fn login_server_different_host_rejected() {
        assert!(validate_login_server(
            "https://evil.example.net",
            "https://cloud.example.com",
        ).is_err());
    }

    #[test]
    fn login_server_scheme_downgrade_rejected() {
        assert!(validate_login_server(
            "http://cloud.example.com",
            "https://cloud.example.com",
        ).is_err());
    }

    #[test]
    fn validate_server_scheme_accepts_https() {
        assert!(validate_server_scheme("https://cloud.example.com", false).is_ok());
    }

    #[test]
    fn validate_server_scheme_rejects_plaintext_public_host() {
        // The core case from the security report: a user configuring an
        // ordinary http:// server must not be allowed to send credentials
        // over it unless they explicitly opted in.
        let err = validate_server_scheme("http://example.com", false)
            .expect_err("plaintext http to a public host must be rejected");
        assert!(err.contains("plaintext"), "got: {}", err);
        assert!(validate_server_scheme("http://example.com", true).is_ok());
    }

    #[test]
    fn validate_server_scheme_allows_loopback_plaintext() {
        for host in ["http://127.0.0.1:8080", "http://[::1]:8080", "http://localhost:8080"] {
            assert!(validate_server_scheme(host, false).is_ok(), "{} should be allowed", host);
        }
    }

    #[test]
    fn validate_server_scheme_rejects_other_schemes() {
        assert!(validate_server_scheme("ftp://cloud.example.com", false).is_err());
    }

    #[test]
    fn login_server_plaintext_returned_rejected_even_if_requested_was_plaintext() {
        // Defense in depth: even if the requested server were somehow a
        // non-loopback http:// URL, the returned server must still be
        // rejected — nothing about a same-scheme response makes it safe.
        let err = validate_login_server(
            "http://example.com",
            "http://example.com",
        ).expect_err("plaintext login server must be rejected regardless of what was requested");
        assert!(err.contains("plaintext"), "got: {}", err);
    }

    #[test]
    fn init_login_flow_rejects_plaintext_public_host() {
        let err = init_login_flow("http://example.com")
            .expect_err("init_login_flow must refuse a plaintext non-loopback server");
        assert!(err.contains("plaintext"), "got: {}", err);
    }
}
