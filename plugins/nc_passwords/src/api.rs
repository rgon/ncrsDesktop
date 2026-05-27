use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

const BASE_FOLDER_UUID: &str = "00000000-0000-0000-0000-000000000000";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PasswordEntry {
    pub id: String,
    pub label: String,
    pub username: String,
    pub password: String,
    pub url: String,
    pub notes: String,
    #[serde(default)]
    pub folder: String,
    #[serde(default)]
    pub favorite: bool,
    #[serde(default)]
    pub trashed: bool,
    #[serde(default)]
    pub hidden: bool,
    #[serde(rename = "statusCode", default)]
    pub status_code: i32,
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub updated: u64,
    #[serde(default)]
    pub edited: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderEntry {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub parent: String,
    #[serde(default)]
    pub favorite: bool,
    #[serde(default)]
    pub trashed: bool,
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub updated: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagEntry {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub favorite: bool,
    #[serde(default)]
    pub trashed: bool,
}

pub struct PasswordsClient {
    client: Client,
    base_url: String,
    username: String,
    password: String,
    session_id: Option<String>,
}

impl PasswordsClient {
    pub fn new(server_url: &str, username: &str, password: &str) -> Self {
        let base_url = server_url.trim_end_matches('/').to_string();
        let client = Client::builder()
            .cookie_store(true)
            .build()
            .expect("failed to build HTTP client");
        Self {
            client,
            base_url,
            username: username.to_string(),
            password: password.to_string(),
            session_id: None,
        }
    }

    fn api_url(&self, path: &str) -> String {
        format!("{}/index.php/apps/passwords{}", self.base_url, path)
    }

    fn request(
        &self,
        method: reqwest::Method,
        path: &str,
    ) -> reqwest::blocking::RequestBuilder {
        let mut req = self
            .client
            .request(method, self.api_url(path))
            .basic_auth(&self.username, Some(&self.password))
            .header("OCS-APIREQUEST", "true");
        if let Some(ref sid) = self.session_id {
            req = req.header("X-API-SESSION", sid);
        }
        req
    }

    pub fn open_session(&mut self) -> Result<(), String> {
        let resp = self
            .request(reqwest::Method::GET, "/api/1.0/session/request")
            .send()
            .map_err(|e| format!("session request: {e}"))?;

        if let Some(sid) = resp.headers().get("x-api-session") {
            self.session_id = Some(sid.to_str().unwrap_or("").to_string());
        }

        let resp = self
            .request(reqwest::Method::POST, "/api/1.0/session/open")
            .json(&serde_json::json!({}))
            .send()
            .map_err(|e| format!("session open: {e}"))?;

        if let Some(sid) = resp.headers().get("x-api-session") {
            self.session_id = Some(sid.to_str().unwrap_or("").to_string());
        }

        if !resp.status().is_success() {
            return Err(format!(
                "session open failed: {}",
                resp.status()
            ));
        }

        Ok(())
    }

    pub fn close_session(&self) -> Result<(), String> {
        self.request(reqwest::Method::GET, "/api/1.0/session/close")
            .send()
            .map_err(|e| format!("session close: {e}"))?;
        Ok(())
    }

    pub fn list_passwords(&self) -> Result<Vec<PasswordEntry>, String> {
        let resp = self
            .request(reqwest::Method::GET, "/api/1.0/password/list")
            .send()
            .map_err(|e| format!("list passwords: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("list passwords: {}", resp.status()));
        }

        resp.json::<Vec<PasswordEntry>>()
            .map_err(|e| format!("parse passwords: {e}"))
    }

    pub fn show_password(&self, id: &str) -> Result<PasswordEntry, String> {
        let resp = self
            .request(reqwest::Method::POST, "/api/1.0/password/show")
            .json(&serde_json::json!({ "id": id }))
            .send()
            .map_err(|e| format!("show password: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("show password: {}", resp.status()));
        }

        resp.json::<PasswordEntry>()
            .map_err(|e| format!("parse password: {e}"))
    }

    pub fn find_passwords(
        &self,
        criteria: serde_json::Value,
    ) -> Result<Vec<PasswordEntry>, String> {
        let resp = self
            .request(reqwest::Method::POST, "/api/1.0/password/find")
            .json(&criteria)
            .send()
            .map_err(|e| format!("find passwords: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("find passwords: {}", resp.status()));
        }

        resp.json::<Vec<PasswordEntry>>()
            .map_err(|e| format!("parse passwords: {e}"))
    }

    pub fn create_password(
        &self,
        label: &str,
        username: &str,
        password: &str,
        url: &str,
        folder: Option<&str>,
    ) -> Result<PasswordEntry, String> {
        let mut body = serde_json::json!({
            "password": password,
            "label": label,
            "username": username,
            "url": url,
        });
        if let Some(f) = folder {
            body["folder"] = serde_json::Value::String(f.to_string());
        }

        let resp = self
            .request(reqwest::Method::POST, "/api/1.0/password/create")
            .json(&body)
            .send()
            .map_err(|e| format!("create password: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("create password: {}", resp.status()));
        }

        resp.json::<PasswordEntry>()
            .map_err(|e| format!("parse created password: {e}"))
    }

    pub fn delete_password(&self, id: &str) -> Result<(), String> {
        let resp = self
            .request(reqwest::Method::DELETE, "/api/1.0/password/delete")
            .json(&serde_json::json!({ "id": id }))
            .send()
            .map_err(|e| format!("delete password: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("delete password: {}", resp.status()));
        }
        Ok(())
    }

    pub fn list_folders(&self) -> Result<Vec<FolderEntry>, String> {
        let resp = self
            .request(reqwest::Method::GET, "/api/1.0/folder/list")
            .send()
            .map_err(|e| format!("list folders: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("list folders: {}", resp.status()));
        }

        resp.json::<Vec<FolderEntry>>()
            .map_err(|e| format!("parse folders: {e}"))
    }

    pub fn list_tags(&self) -> Result<Vec<TagEntry>, String> {
        let resp = self
            .request(reqwest::Method::GET, "/api/1.0/tag/list")
            .send()
            .map_err(|e| format!("list tags: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("list tags: {}", resp.status()));
        }

        resp.json::<Vec<TagEntry>>()
            .map_err(|e| format!("parse tags: {e}"))
    }

    pub fn favicon_url(&self, domain: &str, size: u32) -> String {
        format!(
            "{}/index.php/apps/passwords/api/1.0/service/favicon/{}/{}",
            self.base_url, domain, size
        )
    }

    pub fn base_folder_id() -> &'static str {
        BASE_FOLDER_UUID
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_entry_deserializes() {
        let json = r#"{
            "id": "abc-123",
            "label": "Example",
            "username": "user",
            "password": "secret",
            "url": "https://example.com",
            "notes": "",
            "folder": "00000000-0000-0000-0000-000000000000",
            "favorite": true,
            "trashed": false,
            "hidden": false,
            "statusCode": 0,
            "created": 1700000000,
            "updated": 1700000001,
            "edited": 1700000002
        }"#;
        let entry: PasswordEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.id, "abc-123");
        assert_eq!(entry.label, "Example");
        assert!(entry.favorite);
        assert!(!entry.trashed);
    }

    #[test]
    fn folder_entry_deserializes() {
        let json = r#"{
            "id": "folder-1",
            "label": "Work",
            "parent": "00000000-0000-0000-0000-000000000000",
            "favorite": false,
            "trashed": false,
            "created": 1700000000,
            "updated": 1700000001
        }"#;
        let entry: FolderEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.id, "folder-1");
        assert_eq!(entry.label, "Work");
    }

    #[test]
    fn tag_entry_deserializes() {
        let json = r##"{
            "id": "tag-1",
            "label": "important",
            "color": "#ff0000",
            "favorite": true,
            "trashed": false
        }"##;
        let entry: TagEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.label, "important");
        assert_eq!(entry.color, "#ff0000");
    }

    #[test]
    fn favicon_url_format() {
        let client = PasswordsClient::new("https://cloud.example.com", "user", "pass");
        let url = client.favicon_url("github.com", 32);
        assert_eq!(
            url,
            "https://cloud.example.com/index.php/apps/passwords/api/1.0/service/favicon/github.com/32"
        );
    }

    #[test]
    fn base_folder_id_is_zero_uuid() {
        assert_eq!(
            PasswordsClient::base_folder_id(),
            "00000000-0000-0000-0000-000000000000"
        );
    }

    #[test]
    fn password_entry_deserializes_with_minimal_fields() {
        let json = r#"{
            "id": "min-1",
            "label": "Minimal",
            "username": "u",
            "password": "p",
            "url": "",
            "notes": ""
        }"#;
        let entry: PasswordEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.id, "min-1");
        assert_eq!(entry.folder, "");
        assert!(!entry.favorite);
        assert!(!entry.trashed);
        assert_eq!(entry.status_code, 0);
        assert_eq!(entry.created, 0);
    }

    #[test]
    fn client_trims_trailing_slash() {
        let client = PasswordsClient::new("https://cloud.example.com/", "u", "p");
        let url = client.favicon_url("x.com", 16);
        assert!(
            url.starts_with("https://cloud.example.com/index.php/"),
            "URL should not have double slash: {url}"
        );
    }

    #[test]
    fn api_url_format() {
        let client = PasswordsClient::new("https://cloud.example.com", "u", "p");
        let url = client.api_url("/api/1.0/password/list");
        assert_eq!(
            url,
            "https://cloud.example.com/index.php/apps/passwords/api/1.0/password/list"
        );
    }
}
