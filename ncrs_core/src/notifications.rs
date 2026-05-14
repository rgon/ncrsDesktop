use serde::{Deserialize, Serialize};
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

fn client(http3: bool) -> reqwest::blocking::Client {
    let mut builder = reqwest::blocking::Client::builder()
        .timeout(API_TIMEOUT);
    if http3 {
        builder = builder.http3_prior_knowledge();
    }
    builder.build().expect("reqwest client")
}

pub fn fetch_notifications(
    base: &str,
    username: &str,
    password: &str,
    http3: bool,
) -> Result<Vec<NcNotification>, String> {
    let url = format!(
        "{}/ocs/v2.php/apps/notifications/api/v2/notifications?format=json",
        base
    );
    let resp = client(http3)
        .get(&url)
        .basic_auth(username, Some(password))
        .header("OCS-APIREQUEST", "true")
        .send()
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        return Err(format!("notifications API returned {}", resp.status()));
    }
    let ocs: OcsResponse = resp.json().map_err(|e| e.to_string())?;
    Ok(ocs.ocs.data)
}

pub fn dismiss_notification(
    base: &str,
    username: &str,
    password: &str,
    notification_id: u64,
    http3: bool,
) -> Result<(), String> {
    let url = format!(
        "{}/ocs/v2.php/apps/notifications/api/v2/notifications/{}",
        base, notification_id
    );
    client(http3)
        .delete(&url)
        .basic_auth(username, Some(password))
        .header("OCS-APIREQUEST", "true")
        .send()
        .map_err(|e| e.to_string())?;
    Ok(())
}
