/// Diagnostic tool: test the Nextcloud preview API for a given file path.
/// Usage: probe_preview <fuse-path>
/// Example: probe_preview "/home/rgon/ncrs/logoclc/marketing/Content by LOGO/2026/.../IMG_5980.jpeg"
use std::path::{Path, PathBuf};

fn main() {
    let path = std::env::args().nth(1).expect("Usage: probe_preview <fuse-absolute-path>");
    let path = PathBuf::from(&path);

    let cfg_path = ncrs_core::config::config_path();
    let yaml = std::fs::read_to_string(&cfg_path).expect("read config");
    let opts = ncrs_core::config::configuration_parser(&yaml).expect("parse config");

    let creds = if opts.password.as_deref().unwrap_or("").is_empty()
        && opts.bearer_token.as_deref().unwrap_or("").is_empty()
        && opts.auth_command.is_none()
    {
        let pw = ncrs_core::config::load_password_from_keyring(
            opts.username.as_deref().unwrap_or(""),
            &opts.url,
        ).expect("keyring: no password found");
        let opts2 = ncrs_core::config::MountOptions {
            password: Some(pw),
            ..opts.clone()
        };
        opts2.credentials().expect("credentials")
    } else {
        opts.credentials().expect("credentials")
    };

    let base = ncrs_core::notifications::base_url(&opts.url);
    let mount = &opts.mount_point;
    let webdav_url = &opts.url;

    // Derive remote path from the fuse path
    let remote: PathBuf = match path.strip_prefix(mount) {
        Ok(r) => PathBuf::from("/").join(r),
        Err(_) => {
            eprintln!("Path {:?} is not under mount {:?}", path, mount);
            std::process::exit(1);
        }
    };

    println!("mount:      {}", mount.display());
    println!("remote:     {}", remote.display());
    println!("base:       {}", base);
    println!("webdav_url: {}", webdav_url);
    println!("user:       {}", creds.username());

    // Compute what the XDG thumbnail URI and path would be
    let uri = ncrs_core::preview::file_uri(mount, &remote);
    let xdg = ncrs_core::preview::xdg_thumbnail_path(&uri);
    println!("uri:        {}", uri);
    println!("xdg:        {} (exists={})", xdg.display(), xdg.exists());

    // Build an HTTP client (no HTTP/3 for simplicity)
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .expect("build client");

    // ── PROPFIND parent to get fileid and has_preview ────────────────────────
    let parent = remote.parent().unwrap_or(Path::new("/"));
    let filename = remote.file_name().and_then(|n| n.to_str()).unwrap_or("");
    println!("\n--- PROPFIND on parent directory ---");
    println!("path: {}", parent.display());

    let mut found_fileid: Option<u64> = None;
    let mut found_has_preview = false;

    match ncrs_core::propfind::propfind_list(&client, webdav_url, &creds, parent, std::time::Duration::from_secs(15)) {
        Ok((_, _, entries)) => {
            if let Some(entry) = entries.iter().find(|e| {
                e.path.file_name().and_then(|n| n.to_str()) == Some(filename)
            }) {
                found_fileid = entry.fileid;
                found_has_preview = entry.has_preview;
                println!("has_preview: {}", entry.has_preview);
                println!("fileid:      {:?}", entry.fileid);
                println!("path:        {}", entry.path.display());
            } else {
                println!("File '{}' NOT FOUND in PROPFIND response ({} entries returned)", filename, entries.len());
                if !entries.is_empty() {
                    println!("First few entries:");
                    for e in entries.iter().take(5) {
                        println!("  {}", e.path.display());
                    }
                }
            }
        }
        Err(e) => println!("PROPFIND failed: {}", e),
    }

    // ── Full pipeline test: prefetch_thumbnail (fileId→JPEG→PNG→XDG cache) ────
    if let Some(fid) = found_fileid {
        println!("\n--- Full pipeline: prefetch_thumbnail (fileId={}) ---", fid);
        let mtime = std::fs::metadata(&path).ok().and_then(|m| m.modified().ok());
        // Remove existing XDG entry so we can confirm the write
        let _ = std::fs::remove_file(&xdg);
        ncrs_core::preview::prefetch_thumbnail(
            &client, &base, &creds, mount, &remote, mtime, found_fileid,
        );
        let xdg2 = ncrs_core::preview::xdg_thumbnail_path(&uri);
        println!("XDG exists after prefetch: {}", xdg2.exists());
        if xdg2.exists() {
            println!("SUCCESS: thumbnail written to {}", xdg2.display());
        } else {
            println!("FAIL: thumbnail was NOT written");
        }
    } else {
        println!("\n(skipping pipeline test — no fileid from PROPFIND)");
    }

    // ── Raw API diagnostic tests ──────────────────────────────────────────────
    if let Some(fid) = found_fileid {
        println!("\n--- Testing fileId-based preview API (raw, fileId={}) ---", fid);
        probe_api(&client, &base, &creds, remote.to_str().unwrap(), Some(fid), mount, &remote);
    }

    // ── Test path-based preview API with raw remote_path ─────────────────────
    println!("\n--- Testing path-based preview API (as-is path) ---");
    probe_api(&client, &base, &creds, remote.to_str().unwrap(), None, mount, &remote);

    // ── Test path-based with username prefix stripped ─────────────────────────
    // NC /core/preview ?file= wants the path relative to user home, not the WebDAV root.
    // If the WebDAV URL is /remote.php/dav/files/ (no username), remote_path starts
    // with /<username>/ which NC rejects. Try stripping it.
    let stripped = strip_username_from_path(remote.to_str().unwrap(), creds.username());
    if stripped != remote.to_str().unwrap() {
        println!("\n--- Testing path-based preview API (username stripped: {}) ---", stripped);
        probe_api(&client, &base, &creds, &stripped, None, mount, &remote);
    }

    // ── Check the fail cache ──────────────────────────────────────────────────
    let fail_root = dirs::cache_dir().unwrap().join("thumbnails").join("fail");
    let hash = format!("{:x}", md5::compute(uri.as_bytes()));
    let mut fail_found = false;
    if let Ok(apps) = std::fs::read_dir(&fail_root) {
        for app in apps.flatten() {
            let entry = app.path().join(format!("{}.png", hash));
            if entry.exists() {
                println!("\nFAIL CACHE entry: {}", entry.display());
                fail_found = true;
            }
        }
    }
    if !fail_found {
        println!("\nNo fail cache entries for this file.");
    }
    println!("\nhas_preview={}, fileid={:?}", found_has_preview, found_fileid);
}

/// Strip the leading /<username> component from a path if present.
fn strip_username_from_path<'a>(path: &'a str, username: &str) -> &'a str {
    let prefix = format!("/{}/", username);
    if path.starts_with(&prefix) {
        &path[username.len() + 1..]  // keep the leading /
    } else if path == format!("/{}", username) {
        "/"
    } else {
        path
    }
}

fn probe_api(
    client: &reqwest::blocking::Client,
    base: &str,
    creds: &ncrs_core::auth::Credentials,
    remote_path: &str,
    fileid: Option<u64>,
    mount: &Path,
    remote: &Path,
) {
    let url = format!("{}/core/preview", base);
    let size = "256";
    let req = client.get(&url);
    let req = if let Some(fid) = fileid {
        let fid_str = fid.to_string();
        req.query(&[("fileId", &fid_str as &str), ("x", size), ("y", size), ("a", "0")])
    } else {
        req.query(&[("file", remote_path), ("x", size), ("y", size), ("a", "1")])
    };
    let req = creds.apply(req);

    if fileid.is_some() {
        println!("GET {}/core/preview?fileId={}&...", base, fileid.unwrap());
    } else {
        println!("GET {}/core/preview?file={}...", base, &remote_path[..remote_path.len().min(60)]);
    }

    match req.send() {
        Err(e) => println!("Network error: {}", e),
        Ok(resp) => {
            let status = resp.status();
            let ct = resp.headers().get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("unknown")
                .to_string();
            let content_length = resp.headers().get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            println!("Status:         {}", status);
            println!("Content-Type:   {}", ct);
            if let Some(len) = content_length {
                println!("Content-Length: {} bytes", len);
            }
            match resp.bytes() {
                Err(e) => println!("Read error: {}", e),
                Ok(bytes) => {
                    println!("Body length:    {} bytes", bytes.len());
                    if bytes.len() >= 8 {
                        println!("First 8 bytes:  {:02x?}", &bytes[..8]);
                        let is_png = &bytes[..8] == b"\x89PNG\r\n\x1a\n";
                        let is_jpeg = bytes[0] == 0xFF && bytes[1] == 0xD8;
                        let is_html = bytes.starts_with(b"<!") || bytes.starts_with(b"<html");
                        let fmt = if is_png { "PNG ✓" }
                            else if is_jpeg { "JPEG" }
                            else if is_html { "HTML (auth/redirect error?)" }
                            else { "unknown" };
                        println!("Format:         {}", fmt);
                        // If it's a valid PNG, write it to XDG cache as a test
                        if is_png {
                            let uri = ncrs_core::preview::file_uri(mount, remote);
                            let xdg = ncrs_core::preview::xdg_thumbnail_path(&uri);
                            if !xdg.exists() {
                                if let Some(parent) = xdg.parent() {
                                    let _ = std::fs::create_dir_all(parent);
                                }
                                match std::fs::write(&xdg, &bytes) {
                                    Ok(_) => println!("Written to XDG cache: {}", xdg.display()),
                                    Err(e) => println!("Failed to write XDG cache: {}", e),
                                }
                            } else {
                                println!("XDG cache already exists, not overwriting.");
                            }
                        }
                    } else if !bytes.is_empty() {
                        println!("Body (short):   {:?}", std::str::from_utf8(&bytes).unwrap_or("(binary)"));
                    }
                }
            }
        }
    }
}
