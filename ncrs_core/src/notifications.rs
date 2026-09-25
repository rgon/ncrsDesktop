use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use std::time::Duration;

const API_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NcAction {
    pub label: String,
    pub link: String,
    #[serde(rename = "type")]
    pub action_type: String,
    #[serde(default)]
    pub primary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NcNotification {
    pub notification_id: u64,
    pub app: String,
    #[serde(default)]
    pub user: String,
    pub datetime: String,
    #[serde(default)]
    pub object_type: String,
    #[serde(default)]
    pub object_id: String,
    pub subject: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub link: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub actions: Vec<NcAction>,
}

#[derive(Deserialize)]
struct OcsBody {
    data: Vec<NcNotification>,
}

#[derive(Deserialize)]
struct OcsResponse {
    ocs: OcsBody,
}

/// Extracts the scheme+host from a WebDAV URL, e.g.
/// `https://cloud.example.com/remote.php/dav/files/user/` → `https://cloud.example.com`
pub fn base_url(webdav_url: &str) -> String {
    let after_scheme = webdav_url.find("://").map(|i| i + 3).unwrap_or(0);
    let host_end = webdav_url[after_scheme..]
        .find('/')
        .map(|i| i + after_scheme)
        .unwrap_or(webdav_url.len());
    webdav_url[..host_end].to_string()
}

/// Cached per HTTP/3 variant (both are reachable: the h3 calls fall back to h2).
/// `fetch_notifications` polls every ~30s for the life of the daemon, and each
/// `build()` would spawn a tokio runtime thread, a fresh rustls root store and —
/// with http3 — a new QUIC endpoint/UDP socket.
fn client(http3: bool) -> crate::http_clients::DavClient {
    static H3: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    static H2: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    let cell = if http3 { &H3 } else { &H2 };
    let raw = cell.get_or_init(|| {
        let mut builder = crate::http_clients::with_pooled_dns(reqwest::blocking::Client::builder())
            .timeout(API_TIMEOUT);
        if http3 {
            builder = builder.http3_prior_knowledge();
        }
        let client = builder.build().expect("reqwest client");
        if http3 {
            // Its QUIC endpoint's socket exists now; give it room (see the fn).
            crate::http_clients::raise_quic_socket_buffers();
        }
        client
    });
    // Stamp HTTP/3 requests with their version: reqwest routes a request to
    // the QUIC connector only when the request itself says Version::HTTP_3.
    crate::http_clients::DavClient::new(raw.clone(), http3)
}

pub fn fetch_notifications(
    base: &str,
    creds: &crate::auth::Credentials,
    http3: bool,
) -> Result<Vec<NcNotification>, String> {
    let url = format!(
        "{}/ocs/v2.php/apps/notifications/api/v2/notifications?format=json",
        base
    );
    let send = |h3: bool| {
        creds.apply(client(h3).get(&url))
            .header("OCS-APIREQUEST", "true")
            .send()
    };
    let resp = match send(http3) {
        Ok(r) => r,
        Err(e) if http3 => {
            log::debug!("notifications HTTP/3 failed, retrying with HTTP/2: {e}");
            send(false).map_err(|e| e.to_string())?
        }
        Err(e) => return Err(e.to_string()),
    };

    if !resp.status().is_success() {
        return Err(format!("notifications API returned {}", resp.status()));
    }
    let ocs: OcsResponse = resp.json().map_err(|e| e.to_string())?;
    // `icon` is rendered as <img src> by the GUI and arrived unprocessed, so the
    // server could point the webview at any host it liked. Pin it to the server.
    let notifications = ocs.ocs.data.into_iter()
        .map(|mut n| {
            n.icon = crate::asset_url::inline_asset(&client(http3), base, creds, &n.icon);
            n
        })
        .collect();
    Ok(notifications)
}

pub fn dismiss_notification(
    base: &str,
    creds: &crate::auth::Credentials,
    notification_id: u64,
    http3: bool,
) -> Result<(), String> {
    let url = format!(
        "{}/ocs/v2.php/apps/notifications/api/v2/notifications/{}",
        base, notification_id
    );
    let send = |h3: bool| {
        creds.apply(client(h3).delete(&url))
            .header("OCS-APIREQUEST", "true")
            .send()
    };
    match send(http3) {
        Ok(_) => Ok(()),
        Err(e) if http3 => {
            log::debug!("dismiss HTTP/3 failed, retrying with HTTP/2: {e}");
            send(false).map_err(|e| e.to_string())?;
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}
