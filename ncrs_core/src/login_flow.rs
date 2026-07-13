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
}
