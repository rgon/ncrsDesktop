use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use yaml_rust2::YamlLoader;

use crate::auth::Credentials;

const DEFAULT_READ_AHEAD: usize = 64 * 1024 * 1024; // 64 MB

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize)]
pub struct MountOptions {
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub bearer_token: Option<String>,
    pub auth_command: Option<String>,
    pub mount_point: PathBuf,
    pub log_user: String,
    pub aggressive_prefetch: bool,
    pub http3: bool,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_requests: usize,
    #[serde(default)]
    pub offline: bool,
    #[serde(default = "default_true")]
    pub optimistic_listing: bool,
    #[serde(default)]
    pub auto_keep_locally_modified_files: bool,
    #[serde(default)]
    pub auto_keep_cached_files: bool,
    #[serde(default = "default_read_ahead")]
    pub read_ahead_bytes: usize,
    #[serde(default)]
    pub cache_streamed_reads: bool,
    #[serde(default = "default_cache_max_size")]
    pub cache_max_size_bytes: u64,
    #[serde(default = "default_cache_purge_days")]
    pub cache_auto_purge_days: u32,
    #[serde(default = "default_cache_cleanup_interval")]
    pub cache_cleanup_interval_secs: u64,
    #[serde(default)]
    pub keep_paths: Vec<String>,
    #[serde(default)]
    pub exclude_folders: Vec<String>,
}

impl std::fmt::Debug for MountOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MountOptions")
            .field("url", &self.url)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .field("bearer_token", &self.bearer_token.as_ref().map(|_| "[REDACTED]"))
            .field("auth_command", &self.auth_command)
            .field("mount_point", &self.mount_point)
            .finish_non_exhaustive()
    }
}

impl MountOptions {
    pub fn credentials(&self) -> Result<Credentials, String> {
        let username = self.username.clone().unwrap_or_default();
        let token = self.resolve_bearer_token();
        if let Some(token) = token {
            Ok(Credentials::Bearer { username, token })
        } else if let Some(ref password) = self.password {
            if password.is_empty() {
                return Err("password is empty — configure password, bearer_token, or auth_command".into());
            }
            Ok(Credentials::Basic { username, password: password.clone() })
        } else {
            Err("no credentials configured — set password, bearer_token, or auth_command".into())
        }
    }

    fn resolve_bearer_token(&self) -> Option<String> {
        if let Some(ref cmd) = self.auth_command {
            match std::process::Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .output()
            {
                Ok(output) if output.status.success() => {
                    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if token.is_empty() {
                        log::warn!("auth_command produced empty output");
                        None
                    } else {
                        Some(token)
                    }
                }
                Ok(output) => {
                    log::error!(
                        "auth_command failed ({}): {}",
                        output.status,
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                    None
                }
                Err(e) => {
                    log::error!("auth_command execution error: {}", e);
                    None
                }
            }
        } else {
            self.bearer_token.as_ref().filter(|t| !t.is_empty()).cloned()
        }
    }
}

fn default_true() -> bool { true }

fn default_max_concurrent() -> usize { 10 }

fn default_read_ahead() -> usize { DEFAULT_READ_AHEAD }

fn default_cache_max_size() -> u64 { 32 * 1024 * 1024 * 1024 } // 32 GB

fn default_cache_purge_days() -> u32 { 10 }

fn default_cache_cleanup_interval() -> u64 { 3600 }

// ── YAML parser ───────────────────────────────────────────────────────────────

pub fn configuration_parser(yaml_conf: &str) -> Result<MountOptions, String> {
    let docs =
        YamlLoader::load_from_str(yaml_conf).map_err(|e| format!("YAML parse error: {}", e))?;

    if docs.is_empty() {
        return Err("Empty config file".to_string());
    }

    let doc = &docs[0];

    let url = doc["url"]
        .as_str()
        .ok_or("Missing 'url' in config")?
        .to_string();
    let username = doc["username"].as_str().map(str::to_string);
    let password = doc["password"].as_str().map(str::to_string);
    let bearer_token = doc["bearer_token"].as_str().map(str::to_string);
    let auth_command = doc["auth_command"].as_str().map(str::to_string);
    let mount_point =
        PathBuf::from(doc["mount_point"].as_str().unwrap_or("/media/ncrs_mount"));
    let log_user = doc["user"].as_str().unwrap_or("default_user").to_string();
    let aggressive_prefetch = doc["aggressive_prefetch"].as_bool().unwrap_or(false);
    let http3 = doc["http3"].as_bool().unwrap_or(false);
    let max_concurrent_requests = doc["max_concurrent_requests"].as_i64().unwrap_or(10) as usize;
    let optimistic_listing = doc["optimistic_listing"].as_bool().unwrap_or(true);
    let auto_keep_locally_modified_files = doc["auto_keep_locally_modified_files"].as_bool().unwrap_or(false);
    let auto_keep_cached_files = doc["auto_keep_cached_files"].as_bool().unwrap_or(false);
    let read_ahead_bytes = doc["read_ahead_bytes"].as_i64().map(|v| v as usize).unwrap_or(DEFAULT_READ_AHEAD);
    let cache_streamed_reads = doc["cache_streamed_reads"].as_bool().unwrap_or(false);
    let cache_max_size_bytes = doc["cache_max_size_bytes"].as_i64().map(|v| v as u64).unwrap_or_else(default_cache_max_size);
    let cache_auto_purge_days = doc["cache_auto_purge_days"].as_i64().map(|v| v as u32).unwrap_or_else(default_cache_purge_days);
    let cache_cleanup_interval_secs = doc["cache_cleanup_interval_secs"].as_i64().map(|v| v as u64).unwrap_or_else(default_cache_cleanup_interval);
    let keep_paths = doc["keep_paths"].as_vec()
        .map(|v| v.iter().filter_map(|item| item.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let exclude_folders = doc["exclude_folders"].as_vec()
        .map(|v| v.iter().filter_map(|item| item.as_str().map(str::to_string)).collect())
        .unwrap_or_default();

    Ok(MountOptions { url, username, password, bearer_token, auth_command, mount_point, log_user, aggressive_prefetch, http3, max_concurrent_requests, offline: false, optimistic_listing, auto_keep_locally_modified_files, auto_keep_cached_files, read_ahead_bytes, cache_streamed_reads, cache_max_size_bytes, cache_auto_purge_days, cache_cleanup_interval_secs, keep_paths, exclude_folders })
}

// ── Config file loading ───────────────────────────────────────────────────────

pub const DEFAULT_CONFIG: &str = r#"# ncRS Desktop configuration
# Generated on first run — fill in your Nextcloud credentials.

# Full WebDAV URL, e.g. https://cloud.example.com/remote.php/dav/files/USERNAME/
url: ""

username: ""

# Authentication: provide ONE of password, bearer_token, or auth_command.
password: ""
# bearer_token: ""
# auth_command: "secret-tool lookup xdg:schema-id org.freedesktop.Secret.Generic label authd"

# Local directory where the WebDAV tree will be mounted.
mount_point: ""

# Label used in log lines (usually your local username).
user: ""
"#;

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("ncrs")
        .join("config.yaml")
}

/// Set config file permissions to 0600 (owner read/write only).
/// Non-fatal: logs a warning on failure rather than aborting startup.
pub fn restrict_config_permissions(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        if let Err(e) = std::fs::set_permissions(path, perms) {
            log::warn!("could not set 0600 on {}: {}", path.display(), e);
        }
    }
}

pub fn load_config() -> Result<MountOptions, String> {
    let path = config_path();

    if !path.exists() {
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("Cannot create config dir {}: {}", dir.display(), e))?;
        std::fs::write(&path, DEFAULT_CONFIG)
            .map_err(|e| format!("Cannot write default config: {}", e))?;
        restrict_config_permissions(&path);
        return Err(format!(
            "Created default config at {}. Please fill it in and restart.",
            path.display()
        ));
    }

    let yaml = std::fs::read_to_string(&path)
        .map_err(|e| format!("Cannot read {}: {}", path.display(), e))?;

    let opts = configuration_parser(&yaml)?;

    if opts.url.is_empty() {
        return Err(format!(
            "Config at {} is incomplete (url is empty). Please fill it in.",
            path.display()
        ));
    }

    Ok(opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_config(auth_line: &str) -> MountOptions {
        let yaml = format!(
            "url: \"https://cloud.example.com/remote.php/dav/files/user/\"\nusername: \"user\"\n{}\nmount_point: \"/mnt/nc\"\nuser: test\n",
            auth_line,
        );
        configuration_parser(&yaml).unwrap()
    }

    #[test]
    fn credentials_basic_ok() {
        let opts = minimal_config("password: \"s3cret\"");
        let creds = opts.credentials().unwrap();
        assert!(!creds.is_bearer());
        assert_eq!(creds.username(), "user");
        assert_eq!(creds.secret(), "s3cret");
    }

    #[test]
    fn credentials_bearer_ok() {
        let opts = minimal_config("bearer_token: \"ey.jwt.tok\"");
        let creds = opts.credentials().unwrap();
        assert!(creds.is_bearer());
        assert_eq!(creds.username(), "user");
        assert_eq!(creds.secret(), "ey.jwt.tok");
    }

    #[test]
    fn credentials_missing_errors() {
        let opts = minimal_config("");
        assert!(opts.credentials().is_err());
    }

    #[test]
    fn credentials_empty_password_errors() {
        let opts = minimal_config("password: \"\"");
        assert!(opts.credentials().is_err());
    }

    #[test]
    fn credentials_empty_bearer_token_errors() {
        let opts = minimal_config("bearer_token: \"\"");
        assert!(opts.credentials().is_err());
    }

    #[test]
    fn credentials_bearer_takes_precedence() {
        let opts = minimal_config("password: \"pw\"\nbearer_token: \"tok\"");
        let creds = opts.credentials().unwrap();
        assert!(creds.is_bearer());
        assert_eq!(creds.secret(), "tok");
    }

    #[test]
    fn debug_redacts_secrets() {
        let opts = minimal_config("password: \"super-secret\"");
        let debug = format!("{:?}", opts);
        assert!(!debug.contains("super-secret"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn credentials_debug_redacts_secrets() {
        let creds = Credentials::Basic {
            username: "user".into(),
            password: "super-secret".into(),
        };
        let debug = format!("{:?}", creds);
        assert!(!debug.contains("super-secret"));
        assert!(debug.contains("[REDACTED]"));

        let creds = Credentials::Bearer {
            username: "user".into(),
            token: "ey.jwt.secret".into(),
        };
        let debug = format!("{:?}", creds);
        assert!(!debug.contains("ey.jwt.secret"));
        assert!(debug.contains("[REDACTED]"));
    }
}
