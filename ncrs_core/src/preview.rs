use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

// Characters that must be percent-encoded in file: URI path segments.
// Matches what GLib's g_filename_to_uri encodes so our XDG cache keys
// agree with what Nautilus (GIO) computes when looking up thumbnails.
const FILE_PATH_ENCODE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

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
    // Percent-encode the path so the URI matches what GLib/Nautilus produces.
    // Without this, paths with spaces produce a different MD5 hash than Nautilus
    // expects, so pre-fetched thumbnails are never found in the XDG cache.
    let encoded = utf8_percent_encode(&full.to_string_lossy(), FILE_PATH_ENCODE).to_string();
    format!("file://{}", encoded)
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

/// Remove any XDG fail-cache entries for `file_uri` so Nautilus retries
/// thumbnailing after the daemon successfully pre-fetches a preview.
/// Each entry lives at `thumbnails/fail/<appname>/<md5>.png`.
fn evict_fail_cache(file_uri: &str) {
    let hash = format!("{:x}", md5::compute(file_uri));
    let filename = format!("{}.png", hash);
    let fail_root = dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("thumbnails")
        .join("fail");
    let Ok(apps) = std::fs::read_dir(&fail_root) else { return };
    for app in apps.flatten() {
        let entry = app.path().join(&filename);
        if entry.exists() {
            let _ = std::fs::remove_file(&entry);
            log::debug!("evicted fail cache {}", entry.display());
        }
    }
}

/// Public accessor for the XDG thumbnail cache path for a given file URI.
/// The thumbnailer script uses this to find what the daemon has pre-fetched.
pub fn xdg_thumbnail_path(file_uri: &str) -> PathBuf {
    xdg_thumb_path(file_uri)
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
        return;
    }
    // Remove any stale fail-cache entries so Nautilus picks up the thumbnail
    // instead of indefinitely skipping the file because of a past failure.
    evict_fail_cache(&uri);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uri_encodes_spaces() {
        let mount = Path::new("/home/user/ncrs/logoclc");
        let remote = Path::new("/marketing/Content by LOGO/photo.jpg");
        let uri = file_uri(mount, remote);
        assert_eq!(
            uri,
            "file:///home/user/ncrs/logoclc/marketing/Content%20by%20LOGO/photo.jpg"
        );
    }

    #[test]
    fn file_uri_encodes_special_chars() {
        let mount = Path::new("/mnt");
        let remote = Path::new("/dir/file#1.jpg");
        let uri = file_uri(mount, remote);
        assert!(!uri.contains(' '), "URI must not contain raw spaces");
        assert!(uri.contains("%23"), "# must be encoded as %23");
    }

    #[test]
    fn file_uri_plain_ascii_unchanged() {
        let mount = Path::new("/home/user/ncrs");
        let remote = Path::new("/docs/report.pdf");
        let uri = file_uri(mount, remote);
        assert_eq!(uri, "file:///home/user/ncrs/docs/report.pdf");
    }

    #[test]
    fn xdg_thumbnail_path_matches_encoded_uri() {
        // Thumbnail path must be keyed on the percent-encoded URI so Nautilus/GIO
        // can find it. Before the fix file_uri() returned an unencoded URI whose
        // MD5 hash differed from what Nautilus computed for paths with spaces.
        let mount = Path::new("/mnt/ncrs");
        let remote = Path::new("/my folder/photo.jpg");
        let uri = file_uri(mount, remote);
        assert!(uri.contains("%20"), "space must be encoded so Nautilus finds the thumbnail");
        let path = xdg_thumbnail_path(&uri);
        let expected_hash = format!("{:x}", md5::compute(uri.as_bytes()));
        assert_eq!(path.file_stem().unwrap().to_str().unwrap(), expected_hash);
    }
}
