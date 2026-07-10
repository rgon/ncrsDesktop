use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

const PREVIEW_SIZE: u32 = 128;
const API_TIMEOUT: Duration = Duration::from_secs(5);
const PNG_SIG: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
const THUMB_BATCH: usize = 2;
// On-demand RAW preview generation (nc:has-preview=false) is expensive server-side
// (ImageMagick decoding). Process one at a time and pause between requests so the
// server keeps PHP workers free for FUSE HTTP operations.
const RAW_THUMB_INTERVAL_SECS: u64 = 2;

const PREVIEWABLE: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "svg", "heic",
    "mp3", "flac", "ogg", "m4a", "wav", "opus",
    "mp4", "mkv", "avi", "mov", "webm",
    "pdf",
];

// RAW camera formats whose MIME types Nextcloud may not report as has_preview=true
// even when a server-side preview provider (Imagick/VIPS) can generate them.
// We attempt a preview fetch for these regardless of the has_preview flag.
const RAW_EXTENSIONS: &[&str] = &[
    "cr2", "cr3",        // Canon
    "nef", "nrw",        // Nikon
    "arw", "srf", "sr2", // Sony
    "srw",               // Samsung
    "orf",               // Olympus
    "raf",               // Fujifilm
    "dng",               // Adobe DNG (universal raw)
    "rw2",               // Panasonic
    "pef", "ptx",        // Pentax
    "x3f",               // Sigma
    "3fr",               // Hasselblad
    "mrw",               // Konica-Minolta
    "erf",               // Epson
    "kdc", "dcr",        // Kodak
    "rwl",               // Leica
    "iiq",               // Phase One
    "raw",               // generic raw
];

pub fn is_previewable(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| PREVIEWABLE.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn is_raw_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| RAW_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
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
    creds: &crate::auth::Credentials,
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
    let resp = creds.apply(req)
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
    creds: &crate::auth::Credentials,
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

    let png = match fetch_preview_bytes(client, base, creds, &remote_path.to_string_lossy(), fileid) {
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
            // Nextcloud's preview API is expected to return PNG; JPEG (FFD8) means
            // the server returned a raw JPEG pass-through instead of a scaled preview.
            let hint = if png.starts_with(&[0xFF, 0xD8]) { " (got JPEG)" } else { "" };
            log::debug!("thumbnail {}: not a valid PNG{}, skipping", remote_path.display(), hint);
            return;
        }
    };

    if let Some(parent) = thumb.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Write to a sibling .tmp then rename so concurrent writers can't corrupt
    // a partially-written PNG that Nautilus has already opened and cached.
    let tmp = thumb.with_extension("png.tmp");
    if let Err(e) = std::fs::write(&tmp, &data) {
        log::debug!("write thumbnail {}: {}", thumb.display(), e);
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &thumb) {
        log::debug!("rename thumbnail {}: {}", thumb.display(), e);
        let _ = std::fs::remove_file(&tmp);
    }
}

pub fn prefetch_directory_thumbnails(
    client: &reqwest::blocking::Client,
    base: &str,
    creds: &crate::auth::Credentials,
    mount_point: &Path,
    entries: &[(PathBuf, Option<SystemTime>, bool, Option<u64>)],
    active_streams: &Arc<AtomicUsize>,
) {
    // nc:has-preview=true → server already has a cached preview, request is fast.
    let server_cached: Vec<_> = entries.iter()
        .filter(|(_, _, has_preview, _)| *has_preview)
        .collect();

    // RAW files the server hasn't pre-cached: on-demand ImageMagick decoding is
    // expensive. Keep them separate so they don't saturate PHP workers.
    let on_demand_raw: Vec<_> = entries.iter()
        .filter(|(path, _, has_preview, _)| !*has_preview && is_raw_image(path))
        .collect();

    // ── Fast path: server-cached previews (batch, 1 s gap) ───────────────────
    for (i, chunk) in server_cached.chunks(THUMB_BATCH).enumerate() {
        if i > 0 {
            std::thread::sleep(Duration::from_secs(1));
        }
        while active_streams.load(Ordering::Relaxed) > 0 {
            std::thread::sleep(Duration::from_millis(500));
        }
        std::thread::scope(|s| {
            for (path, mtime, _, fileid) in chunk {
                s.spawn(|| {
                    prefetch_thumbnail(client, base, creds, mount_point, path, *mtime, *fileid);
                });
            }
        });
    }

    // ── Slow path: on-demand RAW (sequential, generous gap) ──────────────────
    // One request at a time so the server always has workers left for FUSE ops.
    for (i, (path, mtime, _, fileid)) in on_demand_raw.iter().enumerate() {
        if i > 0 {
            std::thread::sleep(Duration::from_secs(RAW_THUMB_INTERVAL_SECS));
        }
        while active_streams.load(Ordering::Relaxed) > 0 {
            std::thread::sleep(Duration::from_millis(500));
        }
        prefetch_thumbnail(client, base, creds, mount_point, path, *mtime, *fileid);
    }
}
