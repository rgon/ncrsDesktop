use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

// -- Entry types --------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RemoteEntry {
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub change_token: Option<String>,
    pub content_type: Option<String>,
    pub ext: EntryExtensions,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct EntryExtensions {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub strings: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub integers: HashMap<String, u64>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub booleans: HashMap<String, bool>,
}

impl EntryExtensions {
    pub fn str(&self, key: &str) -> Option<&str> {
        self.strings.get(key).map(|s| s.as_str())
    }

    pub fn int(&self, key: &str) -> Option<u64> {
        self.integers.get(key).copied()
    }

    pub fn flag(&self, key: &str) -> bool {
        self.booleans.get(key).copied().unwrap_or(false)
    }
}

// -- Error types --------------------------------------------------------------

#[derive(Debug)]
pub enum BackendWriteError {
    Conflict,
    Locked,
    Network(String),
    Forbidden,
    QuotaExceeded,
    Server(u16, String),
}

impl std::fmt::Display for BackendWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict => write!(f, "version conflict"),
            Self::Locked => write!(f, "resource locked"),
            Self::Network(e) => write!(f, "network: {}", e),
            Self::Forbidden => write!(f, "permission denied"),
            Self::QuotaExceeded => write!(f, "quota exceeded"),
            Self::Server(code, msg) => write!(f, "server error {}: {}", code, msg),
        }
    }
}

#[derive(Debug)]
pub enum BackendReadError {
    NotFound,
    Network(String),
    Timeout,
    Server(u16, String),
}

impl std::fmt::Display for BackendReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "not found"),
            Self::Network(e) => write!(f, "network: {}", e),
            Self::Timeout => write!(f, "timeout"),
            Self::Server(code, msg) => write!(f, "server error {}: {}", code, msg),
        }
    }
}

pub struct PutResult {
    pub new_change_token: Option<String>,
}

// -- Change watcher -----------------------------------------------------------

pub struct ChangeEvent {
    pub invalidated_dirs: Vec<PathBuf>,
    pub modified_files: Vec<PathBuf>,
    pub invalidate_all: bool,
}

pub trait ChangeWatcherHandle: Send + Sync + 'static {
    fn is_connected(&self) -> bool;
    fn set_paused(&self, _paused: bool) {}
}

pub type ChangeCallback = Box<dyn Fn(ChangeEvent) + Send + Sync + 'static>;

struct NullWatcher;
impl ChangeWatcherHandle for NullWatcher {
    fn is_connected(&self) -> bool { false }
}

// -- Core backend trait -------------------------------------------------------

pub trait CloudBackend: Send + Sync + 'static {
    // -- Directory listing ----------------------------------------------------

    fn list_dir(
        &self,
        path: &Path,
        timeout: Duration,
    ) -> Result<(Option<String>, Option<RemoteEntry>, Vec<RemoteEntry>), BackendReadError>;

    fn list_dir_streaming(
        &self,
        path: &Path,
        timeout: Duration,
        entry_tx: mpsc::Sender<RemoteEntry>,
        self_tx: mpsc::Sender<RemoteEntry>,
    ) -> Result<Option<String>, BackendReadError>;

    fn dir_change_token(
        &self,
        path: &Path,
        timeout: Duration,
    ) -> Result<Option<String>, BackendReadError>;

    // -- File reading ---------------------------------------------------------

    fn download_file(
        &self,
        path: &Path,
        dest: &mut dyn std::io::Write,
        timeout: Duration,
    ) -> Result<u64, BackendReadError>;

    fn read_file_range(
        &self,
        path: &Path,
        offset: u64,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, BackendReadError>;

    // -- Write operations -----------------------------------------------------

    fn put_file(
        &self,
        path: &Path,
        body: Vec<u8>,
        if_match: Option<&str>,
    ) -> Result<PutResult, BackendWriteError>;

    fn mkdir(&self, path: &Path) -> Result<(), BackendWriteError>;

    fn delete(&self, path: &Path) -> Result<(), BackendWriteError>;

    fn rename(&self, from: &Path, to: &Path) -> Result<(), BackendWriteError>;

    // -- Connectivity ---------------------------------------------------------

    fn is_reachable(&self, timeout: Duration) -> bool;

    // -- Change notifications -------------------------------------------------

    fn start_change_watcher(
        &self,
        _callback: ChangeCallback,
    ) -> Box<dyn ChangeWatcherHandle> {
        Box::new(NullWatcher)
    }

    // -- Entry metadata accessors (with defaults) -----------------------------

    fn permission_mode(&self, entry: &RemoteEntry) -> u16 {
        if entry.is_dir { 0o755 } else { 0o644 }
    }

    fn is_shared(&self, _entry: &RemoteEntry) -> bool {
        false
    }

    fn file_id(&self, _entry: &RemoteEntry) -> Option<u64> {
        None
    }

    fn owner_display_name<'a>(&self, _entry: &'a RemoteEntry) -> Option<&'a str> {
        None
    }

    fn owner_id<'a>(&self, _entry: &'a RemoteEntry) -> Option<&'a str> {
        None
    }

    fn has_preview(&self, _entry: &RemoteEntry) -> bool {
        false
    }
}

// -- Optional extension traits ------------------------------------------------

pub trait Searchable: CloudBackend {
    fn fetch_search_providers(&self) -> Result<Vec<crate::search::SearchProvider>, String>;

    fn search(
        &self,
        term: &str,
        provider_ids: &[String],
    ) -> Result<Vec<crate::search::SearchResultGroup>, String>;
}

pub trait HasNotifications: CloudBackend {
    fn fetch_notifications(&self) -> Result<Vec<crate::notifications::NcNotification>, String>;
    fn dismiss_notification(&self, id: u64) -> Result<(), String>;
}

pub trait HasPreviews: CloudBackend {
    fn prefetch_thumbnail(
        &self,
        mount_point: &Path,
        remote_path: &Path,
        mtime: Option<SystemTime>,
        entry: &RemoteEntry,
    );

    fn prefetch_directory_thumbnails(
        &self,
        mount_point: &Path,
        entries: &[(PathBuf, Option<SystemTime>, RemoteEntry)],
    );
}

// -- Conversions from existing types ------------------------------------------

impl From<crate::propfind::DavEntry> for RemoteEntry {
    fn from(dav: crate::propfind::DavEntry) -> Self {
        let mut ext = EntryExtensions::default();
        if let Some(p) = dav.permissions {
            ext.strings.insert("permissions".into(), p);
        }
        if let Some(id) = dav.owner_id {
            ext.strings.insert("owner_id".into(), id);
        }
        if let Some(name) = dav.owner_display_name {
            ext.strings.insert("owner_display_name".into(), name);
        }
        if let Some(fid) = dav.fileid {
            ext.integers.insert("fileid".into(), fid);
        }
        ext.booleans.insert("has_preview".into(), dav.has_preview);
        ext.booleans.insert("is_shared".into(), dav.is_shared);

        RemoteEntry {
            path: dav.path,
            is_dir: dav.is_dir,
            size: dav.size,
            modified: dav.modified,
            change_token: dav.etag,
            content_type: dav.content_type,
            ext,
        }
    }
}

impl From<crate::webdav_ops::WriteError> for BackendWriteError {
    fn from(e: crate::webdav_ops::WriteError) -> Self {
        match e {
            crate::webdav_ops::WriteError::Conflict => BackendWriteError::Conflict,
            crate::webdav_ops::WriteError::Locked => BackendWriteError::Locked,
            crate::webdav_ops::WriteError::Network(s) => BackendWriteError::Network(s),
            crate::webdav_ops::WriteError::Server(403, s) => {
                log::debug!("403 → Forbidden: {}", s);
                BackendWriteError::Forbidden
            }
            crate::webdav_ops::WriteError::Server(507, s) => {
                log::debug!("507 → QuotaExceeded: {}", s);
                BackendWriteError::QuotaExceeded
            }
            crate::webdav_ops::WriteError::Server(code, s) => BackendWriteError::Server(code, s),
        }
    }
}

impl From<crate::webdav_ops::PutResult> for PutResult {
    fn from(r: crate::webdav_ops::PutResult) -> Self {
        PutResult { new_change_token: r.new_etag }
    }
}
