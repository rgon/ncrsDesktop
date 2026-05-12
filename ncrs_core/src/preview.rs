use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const PREVIEW_SIZE: u32 = 256;
const API_TIMEOUT: Duration = Duration::from_secs(10);
const PNG_SIG: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
const THUMB_BATCH: usize = 8;

const PREVIEWABLE: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "svg", "heic",
    "mp3", "flac", "ogg", "m4a", "wav", "opus",
    "mp4", "mkv", "avi", "mov", "webm",
    "pdf",
];

pub fn is_previewable(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| PREVIEWABLE.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

pub fn file_uri(mount_point: &Path, remote_path: &Path) -> String {
    let rel = remote_path.strip_prefix("/").unwrap_or(remote_path);
    let full = mount_point.join(rel);
    format!("file://{}", full.display())
}

fn xdg_thumb_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("thumbnails")
        .join("normal")
}

fn xdg_thumb_path(file_uri: &str) -> PathBuf {
    let hash = format!("{:x}", md5::compute(file_uri));
    xdg_thumb_dir().join(format!("{}.png", hash))
}

// ── NC preview API ────────────────────────────────────────────────────────────

fn fetch_preview_bytes(
    client: &reqwest::blocking::Client,
    base: &str,
    username: &str,
    password: &str,
    remote_path: &str,
    fileid: Option<u64>,
) -> Result<Vec<u8>, String> {
    log::debug!("GET_THUMB {}", remote_path);
    let url = format!("{}/core/preview", base);
    let size = PREVIEW_SIZE.to_string();
    let req = client.get(&url).timeout(API_TIMEOUT);
    let req = if let Some(fid) = fileid {
        let fid_str = fid.to_string();
        req.query(&[("fileId", &fid_str), ("x", &size), ("y", &size), ("mimeFallback", &"true".to_string()), ("a", &"0".to_string())])
    } else {
        req.query(&[("file", &remote_path.to_string()), ("x", &size), ("y", &size), ("a", &"1".to_string())])
    };
    let resp = req
        .basic_auth(username, Some(password))
        .send()
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        return Err(format!("preview API returned {}", resp.status()));
    }
    resp.bytes().map(|b| b.to_vec()).map_err(|e| e.to_string())
}

// ── XDG thumbnail PNG writer ──────────────────────────────────────────────────

fn inject_png_text_chunks(png: &[u8], entries: &[(&str, &str)]) -> Option<Vec<u8>> {
    if png.len() < 12 || png[..8] != PNG_SIG {
        return None;
    }
    let ihdr_data_len = u32::from_be_bytes(png[8..12].try_into().ok()?) as usize;
    let ihdr_end = 8 + 4 + 4 + ihdr_data_len + 4;
    if ihdr_end > png.len() {
        return None;
    }

    let mut out = Vec::with_capacity(png.len() + entries.len() * 80);
    out.extend_from_slice(&png[..ihdr_end]);

    for (key, val) in entries {
        let mut data = Vec::with_capacity(key.len() + 1 + val.len());
        data.extend_from_slice(key.as_bytes());
        data.push(0);
        data.extend_from_slice(val.as_bytes());

        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(b"tEXt");
        out.extend_from_slice(&data);

        let mut h = crc32fast::Hasher::new();
        h.update(b"tEXt");
        h.update(&data);
        out.extend_from_slice(&h.finalize().to_be_bytes());
    }

    out.extend_from_slice(&png[ihdr_end..]);
    Some(out)
}

// ── Public interface ──────────────────────────────────────────────────────────

pub fn prefetch_thumbnail(
    client: &reqwest::blocking::Client,
    base: &str,
    username: &str,
    password: &str,
    mount_point: &Path,
    remote_path: &Path,
    mtime: Option<SystemTime>,
    fileid: Option<u64>,
) {
    let uri = file_uri(mount_point, remote_path);
    let thumb = xdg_thumb_path(&uri);
    if thumb.exists() {
        return;
    }

    let png = match fetch_preview_bytes(client, base, username, password, &remote_path.to_string_lossy(), fileid) {
        Ok(d) => d,
        Err(e) => {
            log::debug!("thumbnail {}: {}", remote_path.display(), e);
            return;
        }
    };

    let mtime_s = mtime
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|| "0".into());

    let data = match inject_png_text_chunks(&png, &[("Thumb::URI", &uri), ("Thumb::MTime", &mtime_s)]) {
        Some(d) => d,
        None => {
            log::debug!("thumbnail {}: not a valid PNG, skipping", remote_path.display());
            return;
        }
    };

    if let Some(parent) = thumb.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&thumb, data) {
        log::debug!("write thumbnail {}: {}", thumb.display(), e);
    }
}

pub fn prefetch_directory_thumbnails(
    client: &reqwest::blocking::Client,
    base: &str,
    username: &str,
    password: &str,
    mount_point: &Path,
    entries: &[(PathBuf, Option<SystemTime>, bool, Option<u64>)],
) {
    let previewable: Vec<_> = entries.iter()
        .filter(|(_, _, has_preview, _)| *has_preview)
        .collect();

    for chunk in previewable.chunks(THUMB_BATCH) {
        std::thread::scope(|s| {
            for (path, mtime, _, fileid) in chunk {
                s.spawn(|| {
                    prefetch_thumbnail(client, base, username, password, mount_point, path, *mtime, *fileid);
                });
            }
        });
    }
}
