use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use yaml_rust2::YamlLoader;

const DEFAULT_READ_AHEAD: usize = 64 * 1024 * 1024; // 64 MB

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MountOptions {
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
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

    Ok(MountOptions { url, username, password, mount_point, log_user, aggressive_prefetch, http3, max_concurrent_requests, offline: false, optimistic_listing, auto_keep_locally_modified_files, auto_keep_cached_files, read_ahead_bytes, cache_streamed_reads, cache_max_size_bytes, cache_auto_purge_days, cache_cleanup_interval_secs, keep_paths, exclude_folders })
}

// ── Config file loading ───────────────────────────────────────────────────────

const DEFAULT_CONFIG: &str = r#"# ncRS Desktop configuration
# Generated on first run — fill in your Nextcloud credentials.

# Full WebDAV URL, e.g. https://cloud.example.com/remote.php/dav/files/USERNAME/
url: ""

username: ""
password: ""

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

pub fn load_config() -> Result<MountOptions, String> {
    let path = config_path();

    if !path.exists() {
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("Cannot create config dir {}: {}", dir.display(), e))?;
        std::fs::write(&path, DEFAULT_CONFIG)
            .map_err(|e| format!("Cannot write default config: {}", e))?;
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
