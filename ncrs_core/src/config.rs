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
    /// Maximum age of a cached directory listing that may still be served
    /// optimistically. Past it, the directory etag is checked before the listing is
    /// returned, and the listing is re-fetched if it changed, instead of serving
    /// stale entries with a background refresh.
    /// Applies while notify-push is not delivering; while it is, invalidations
    /// arrive as events and only a 24h backstop applies.
    /// 0 disables the check (always serve from cache when present).
    #[serde(default = "default_dir_cache_max_stale_mins")]
    pub dir_cache_max_stale_mins: u64,
    /// Ceiling on how many directory listings stay in memory (LRU past that).
    #[serde(default = "default_dir_cache_max_dirs")]
    pub dir_cache_max_dirs: usize,
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
    #[serde(default = "default_true")]
    pub cleanup_stale_gio_temps: bool,
    #[serde(default = "default_stale_gio_temp_mins")]
    pub stale_gio_temp_mins: u64,
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

fn default_stale_gio_temp_mins() -> u64 { 10 }

fn default_dir_cache_max_stale_mins() -> u64 { 15 }
/// Ceiling on cached directory listings.
///
/// The cache used to be unbounded, so anything that walked the mount (a file
/// manager, a thumbnailer, a search indexer) permanently added to it: one real
/// account reached 26,614 directories / 398,008 entries, about 150 MB resident,
/// and a 12-second startup re-parsing them. At the ~15 entries per directory
/// that account averages, 5,000 directories is roughly 75,000 entries — a few
/// tens of MB — while still covering far more than anyone browses in a session.
/// 0 disables the limit.
fn default_dir_cache_max_dirs() -> usize { 5_000 }

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
    let url = crate::login_flow::normalize_webdav_url(&url, username.as_deref().unwrap_or(""));
    let password = doc["password"].as_str().map(str::to_string);
    let bearer_token = doc["bearer_token"].as_str().map(str::to_string);
    let auth_command = doc["auth_command"].as_str().map(str::to_string);
    let mount_point =
        PathBuf::from(doc["mount_point"].as_str().unwrap_or("/media/ncrs_mount"));
    let log_user = doc["user"].as_str().unwrap_or("default_user").to_string();
    let aggressive_prefetch = doc["aggressive_prefetch"].as_bool().unwrap_or(false);
    let http3 = doc["http3"].as_bool().unwrap_or(true);
    let max_concurrent_requests = doc["max_concurrent_requests"].as_i64().unwrap_or(10) as usize;
    let optimistic_listing = doc["optimistic_listing"].as_bool().unwrap_or(true);
    let dir_cache_max_stale_mins = doc["dir_cache_max_stale_mins"].as_i64().map(|v| v.max(0) as u64).unwrap_or_else(default_dir_cache_max_stale_mins);
    let dir_cache_max_dirs = doc["dir_cache_max_dirs"].as_i64().map(|v| v.max(0) as usize).unwrap_or_else(default_dir_cache_max_dirs);
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
    let cleanup_stale_gio_temps = doc["cleanup_stale_gio_temps"].as_bool().unwrap_or(true);
    let stale_gio_temp_mins = doc["stale_gio_temp_mins"].as_i64().map(|v| v as u64).unwrap_or(10);

    Ok(MountOptions { url, username, password, bearer_token, auth_command, mount_point, log_user, aggressive_prefetch, http3, max_concurrent_requests, offline: false, optimistic_listing, dir_cache_max_stale_mins, dir_cache_max_dirs, auto_keep_locally_modified_files, auto_keep_cached_files, read_ahead_bytes, cache_streamed_reads, cache_max_size_bytes, cache_auto_purge_days, cache_cleanup_interval_secs, keep_paths, exclude_folders, cleanup_stale_gio_temps, stale_gio_temp_mins })
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

# Before showing a directory whose cached listing is older than this many minutes,
# ask the server whether it changed, so the first listing is already current. An
# unchanged directory costs one small request. Only applies while push
# notifications are down; while they work, a 24-hour backstop applies. 0 disables.
# dir_cache_max_stale_mins: 15
"#;

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("ncrs")
        .join("config.yaml")
}

/// Writes `content` to `path` readable only by the owner.
///
/// Public because the config file is written from more than one place — the GUI's
/// login flow and the remote-wipe handler both rewrite it — and every one of them
/// must produce 0600. A plain `std::fs::write` at any of those sites silently
/// undoes it.
///
/// The config file can hold `password:` / `bearer_token:` in cleartext, and
/// `std::fs::write` creates a file at `0666 & ~umask` — 0644 under the usual
/// umask, i.e. world-readable. The mode is applied to the handle before any
/// content is written, and set again afterwards so an existing file that was
/// already too permissive is tightened rather than left as found.
pub fn write_private(path: &std::path::Path, content: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(content)?;
        f.sync_all()?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, content)
    }
}

/// Warn if the config file permissions are broader than 0600.
/// Does not modify the file — just logs so the user knows to run `chmod 0600`.
pub fn warn_config_permissions(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(path) {
            Ok(meta) => {
                let mode = meta.permissions().mode() & 0o777;
                if mode != 0o600 {
                    log::warn!(
                        "config file {} has permissions {:04o} — expected 0600 (owner read/write only). \
                         Run `chmod 0600 {}` to secure your credentials.",
                        path.display(), mode, path.display()
                    );
                }
            }
            Err(e) => log::warn!("could not check permissions on {}: {}", path.display(), e),
        }
    }
}

// ── Keyring ───────────────────────────────────────────────────────────────────

const KEYRING_SERVICE: &str = "ncrs";

/// Build the keyring account key from username + server URL.
/// E.g. "alice@https://cloud.example.com"
fn keyring_account(username: &str, url: &str) -> String {
    // Strip the WebDAV path suffix so the key is stable even if the path changes.
    let server = url
        .find("/remote.php")
        .or_else(|| url.find("/webdav"))
        .map_or(url, |i| &url[..i])
        .trim_end_matches('/');
    format!("{}@{}", username, server)
}

/// Load the app password from the system keyring. Returns `None` if no entry
/// exists or if the keyring is unavailable (headless environment, locked session).
pub fn load_password_from_keyring(username: &str, url: &str) -> Option<String> {
    let account = keyring_account(username, url);
    let entry = match keyring::Entry::new(KEYRING_SERVICE, &account) {
        Ok(e) => e,
        Err(e) => { log::warn!("keyring init for {}: {}", account, e); return None; }
    };
    match entry.get_password() {
        Ok(pw) => {
            log::info!("loaded credentials from keyring for {}", account);
            Some(pw)
        }
        Err(keyring::Error::NoEntry) => None,
        Err(e) => {
            log::warn!("keyring read failed for {}: {}", account, e);
            None
        }
    }
}

/// Persist the app password in the system keyring (GNOME Keyring / KWallet).
pub fn save_password_to_keyring(username: &str, url: &str, password: &str) -> Result<(), String> {
    let account = keyring_account(username, url);
    let entry = keyring::Entry::new(KEYRING_SERVICE, &account)
        .map_err(|e| format!("keyring init for {}: {}", account, e))?;
    // Explicitly delete any existing entry before writing to prevent duplicate
    // secret-service items, which cause get_password() to return stale credentials.
    match entry.delete_password() {
        Ok(()) | Err(keyring::Error::NoEntry) => {}
        Err(e) => log::debug!("keyring pre-delete for {}: {}", account, e),
    }
    entry
        .set_password(password)
        .map_err(|e| format!("keyring save failed for {}: {}", account, e))?;
    log::info!("saved credentials to keyring for {}", account);
    Ok(())
}

/// Remove the stored app password from the system keyring.
pub fn delete_password_from_keyring(username: &str, url: &str) -> Result<(), String> {
    let account = keyring_account(username, url);
    let entry = keyring::Entry::new(KEYRING_SERVICE, &account)
        .map_err(|e| format!("keyring init for {}: {}", account, e))?;
    entry
        .delete_password()
        .map_err(|e| format!("keyring delete failed for {}: {}", account, e))?;
    log::info!("deleted credentials from keyring for {}", account);
    Ok(())
}

// ── Config loading ────────────────────────────────────────────────────────────

pub fn load_config() -> Result<MountOptions, String> {
    let path = config_path();

    if !path.exists() {
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("Cannot create config dir {}: {}", dir.display(), e))?;
        write_private(&path, DEFAULT_CONFIG.as_bytes())
            .map_err(|e| format!("Cannot write default config: {}", e))?;
        warn_config_permissions(&path);
        return Err(format!(
            "Created default config at {}. Please fill it in and restart.",
            path.display()
        ));
    }

    warn_config_permissions(&path);

    let yaml = std::fs::read_to_string(&path)
        .map_err(|e| format!("Cannot read {}: {}", path.display(), e))?;

    let mut opts = configuration_parser(&yaml)?;

    if opts.url.is_empty() {
        return Err(format!(
            "Config at {} is incomplete (url is empty). Please fill it in.",
            path.display()
        ));
    }

    // If the config file has no credentials, fall back to the system keyring.
    // This is the normal state for accounts created via the interactive login flow.
    let has_file_creds = opts.password.as_deref().is_some_and(|p| !p.is_empty())
        || opts.bearer_token.as_deref().is_some_and(|t| !t.is_empty())
        || opts.auth_command.is_some();

    if !has_file_creds {
        if let Some(ref username) = opts.username.clone() {
            if let Some(pw) = load_password_from_keyring(username, &opts.url) {
                opts.password = Some(pw);
            }
        }
    }

    Ok(opts)
}

// ── Settings editing ──────────────────────────────────────────────────────────

/// The subset of config fields exposed for GUI editing (no credentials).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigSettings {
    pub mount_point: String,
    pub aggressive_prefetch: bool,
    pub http3: bool,
    pub max_concurrent_requests: usize,
    pub optimistic_listing: bool,
    pub dir_cache_max_stale_mins: u64,
    // Defaulted so a settings payload from an older frontend bundle (which does
    // not know this field) still deserializes instead of failing the whole save.
    #[serde(default = "default_dir_cache_max_dirs")]
    pub dir_cache_max_dirs: usize,
    pub auto_keep_locally_modified_files: bool,
    pub auto_keep_cached_files: bool,
    pub read_ahead_bytes: usize,
    pub cache_max_size_bytes: u64,
    pub cache_auto_purge_days: u32,
    pub cache_cleanup_interval_secs: u64,
    pub cache_streamed_reads: bool,
    pub cleanup_stale_gio_temps: bool,
    pub stale_gio_temp_mins: u64,
}

impl Default for ConfigSettings {
    fn default() -> Self {
        ConfigSettings {
            mount_point: dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/home"))
                .join("Nextcloud")
                .to_string_lossy()
                .into_owned(),
            aggressive_prefetch: false,
            http3: true,
            max_concurrent_requests: 10,
            optimistic_listing: true,
            dir_cache_max_stale_mins: default_dir_cache_max_stale_mins(),
            dir_cache_max_dirs: default_dir_cache_max_dirs(),
            auto_keep_locally_modified_files: false,
            auto_keep_cached_files: false,
            read_ahead_bytes: DEFAULT_READ_AHEAD,
            cache_max_size_bytes: default_cache_max_size(),
            cache_auto_purge_days: default_cache_purge_days(),
            cache_cleanup_interval_secs: default_cache_cleanup_interval(),
            cache_streamed_reads: false,
            cleanup_stale_gio_temps: true,
            stale_gio_temp_mins: 10,
        }
    }
}

pub fn config_settings_from_opts(opts: &MountOptions) -> ConfigSettings {
    ConfigSettings {
        mount_point: opts.mount_point.to_string_lossy().into_owned(),
        aggressive_prefetch: opts.aggressive_prefetch,
        http3: opts.http3,
        max_concurrent_requests: opts.max_concurrent_requests,
        optimistic_listing: opts.optimistic_listing,
        dir_cache_max_stale_mins: opts.dir_cache_max_stale_mins,
        dir_cache_max_dirs: opts.dir_cache_max_dirs,
        auto_keep_locally_modified_files: opts.auto_keep_locally_modified_files,
        auto_keep_cached_files: opts.auto_keep_cached_files,
        read_ahead_bytes: opts.read_ahead_bytes,
        cache_max_size_bytes: opts.cache_max_size_bytes,
        cache_auto_purge_days: opts.cache_auto_purge_days,
        cache_cleanup_interval_secs: opts.cache_cleanup_interval_secs,
        cache_streamed_reads: opts.cache_streamed_reads,
        cleanup_stale_gio_temps: opts.cleanup_stale_gio_temps,
        stale_gio_temp_mins: opts.stale_gio_temp_mins,
    }
}

/// Rewrite the config file, preserving credentials and server fields, applying
/// only the settings-panel fields from `settings`.
pub fn rewrite_config_settings(settings: &ConfigSettings) -> Result<(), String> {
    let path = config_path();

    let existing = if path.exists() {
        std::fs::read_to_string(&path).unwrap_or_default()
    } else {
        String::new()
    };

    let docs = YamlLoader::load_from_str(&existing).unwrap_or_default();

    let (url, username, password, bearer_token, auth_command, log_user) =
        if let Some(doc) = docs.first() {
            let url = doc["url"].as_str().unwrap_or("").to_string();
            let username = doc["username"].as_str().unwrap_or("").to_string();
            let password = doc["password"]
                .as_str()
                .and_then(|s| if s.is_empty() { None } else { Some(s.to_string()) });
            let bearer_token = doc["bearer_token"]
                .as_str()
                .and_then(|s| if s.is_empty() { None } else { Some(s.to_string()) });
            let auth_command = doc["auth_command"]
                .as_str()
                .and_then(|s| if s.is_empty() { None } else { Some(s.to_string()) });
            let log_user = doc["user"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| username.clone());
            (url, username, password, bearer_token, auth_command, log_user)
        } else {
            (String::new(), String::new(), None, None, None, String::new())
        };

    let mut content = String::from("# ncRS Desktop configuration\n\n");
    content.push_str(&format!("url: {:?}\n", url));
    content.push_str(&format!("username: {:?}\n", username));
    if let Some(ref pw) = password {
        content.push_str(&format!("password: {:?}\n", pw));
    }
    if let Some(ref bt) = bearer_token {
        content.push_str(&format!("bearer_token: {:?}\n", bt));
    }
    if let Some(ref ac) = auth_command {
        content.push_str(&format!("auth_command: {:?}\n", ac));
    }
    content.push_str(&format!("mount_point: {:?}\n", settings.mount_point));
    content.push_str(&format!("user: {:?}\n", log_user));
    content.push('\n');
    content.push_str("# When enabled, ncRS pre-fetches metadata and thumbnails for every entry in a\n");
    content.push_str("# directory as soon as it is listed, even before the files are opened. Speeds\n");
    content.push_str("# up browsing but increases network traffic on large directories.\n");
    content.push_str(&format!("aggressive_prefetch: {}\n", settings.aggressive_prefetch));
    content.push_str("# Prefer HTTP/3 (QUIC). Probed once at mount time: if QUIC fails there while\n");
    content.push_str("# plain HTTPS works, the daemon logs a warning, runs the session on HTTP/2 and\n");
    content.push_str("# remembers the verdict for a week (h3_demoted in the cache dir), so restarts\n");
    content.push_str("# don't re-pay the discovery blip. Set false if your network never does QUIC.\n");
    content.push_str(&format!("http3: {}\n", settings.http3));
    content.push_str(&format!("max_concurrent_requests: {}\n", settings.max_concurrent_requests));
    content.push_str("# When enabled, directory listings are returned immediately from the local cache\n");
    content.push_str("# while a background refresh fetches the latest contents from the server.\n");
    content.push_str("# Keeps the file manager responsive; disable if you need listings to always\n");
    content.push_str("# reflect the current server state before rendering.\n");
    content.push_str(&format!("optimistic_listing: {}\n", settings.optimistic_listing));
    content.push_str("# Before showing a directory whose cached listing is older than this many\n");
    content.push_str("# minutes, ask the server whether it changed, so the first listing is already\n");
    content.push_str("# current. An unchanged directory costs one small request. Only applies while\n");
    content.push_str("# push notifications are down; while they work, a 24-hour backstop applies.\n");
    content.push_str("# 0 disables.\n");
    content.push_str(&format!("dir_cache_max_stale_mins: {}\n", settings.dir_cache_max_stale_mins));
    content.push_str("# Maximum number of directory listings held in memory. Least-recently-used\n");
    content.push_str("# listings are dropped past this; a dropped directory is simply re-listed the\n");
    content.push_str("# next time it is opened. 0 removes the limit (the old unbounded behaviour,\n");
    content.push_str("# which on a large tree can cost hundreds of MB of RAM).\n");
    content.push_str(&format!("dir_cache_max_dirs: {}\n", settings.dir_cache_max_dirs));
    content.push_str(&format!("auto_keep_locally_modified_files: {}\n", settings.auto_keep_locally_modified_files));
    content.push_str(&format!("auto_keep_cached_files: {}\n", settings.auto_keep_cached_files));
    content.push_str(&format!("read_ahead_bytes: {}\n", settings.read_ahead_bytes));
    content.push_str(&format!("cache_max_size_bytes: {}\n", settings.cache_max_size_bytes));
    content.push_str(&format!("cache_auto_purge_days: {}\n", settings.cache_auto_purge_days));
    content.push_str(&format!("cache_cleanup_interval_secs: {}\n", settings.cache_cleanup_interval_secs));
    content.push_str("# When enabled, file data read via streaming (e.g. media playback) is saved to\n");
    content.push_str("# the local cache so subsequent opens are served from disk. Increases disk usage\n");
    content.push_str("# but avoids re-downloading the same file on repeated access.\n");
    content.push_str(&format!("cache_streamed_reads: {}\n", settings.cache_streamed_reads));
    content.push_str("# GTK/GIO apps write files atomically via a .goutputstream-* or .xdp-* temp file\n");
    content.push_str("# that is renamed to the final name within seconds. When enabled, ncRS deletes\n");
    content.push_str("# any such file found on the server immediately and never lists them to the file\n");
    content.push_str("# manager. Orphans from crashed copies are cleaned up on the next directory open.\n");
    content.push_str(&format!("cleanup_stale_gio_temps: {}\n", settings.cleanup_stale_gio_temps));
    content.push_str(&format!("stale_gio_temp_mins: {}\n", settings.stale_gio_temp_mins));

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create config dir: {}", e))?;
    }
    write_private(&path, content.as_bytes()).map_err(|e| format!("write config: {}", e))?;
    warn_config_permissions(&path);
    Ok(())
}

#[cfg(test)]
mod permission_tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn every_config_writer_goes_through_write_private() {
        // Regression guard for the duplication this replaced: the GUI login flow
        // and the remote-wipe handler each had their own `std::fs::write`, so a
        // config created by logging in was 0644 and only warned about. Any new
        // `fs::write` against the config path reintroduces that.
        let sources = [
            include_str!("../../ncrs-gui/src-tauri/src/lib.rs"),
            include_str!("remote_wipe.rs"),
        ];
        for src in sources {
            for (n, line) in src.lines().enumerate() {
                let code = line.split("//").next().unwrap_or("");
                if code.contains("fs::write(") && code.contains("config") {
                    panic!("line {} writes the config directly: {}", n + 1, line.trim());
                }
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn config_is_written_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join("ncrs-perm-test-write");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        let _ = std::fs::remove_file(&path);

        write_private(&path, b"password: \"hunter2\"\n").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "config holding a password must not be readable by others");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "password: \"hunter2\"\n");

        // An already-too-permissive file gets tightened, not left as found.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, b"password: \"other\"\n").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "rewriting must tighten a world-readable config");

        std::fs::remove_dir_all(&dir).ok();
    }
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
