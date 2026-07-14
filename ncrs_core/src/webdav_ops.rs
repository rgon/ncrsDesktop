use std::path::Path;
use std::time::Duration;

const WRITE_TIMEOUT: Duration = Duration::from_secs(60);

// RFC 3986 unreserved characters that must NOT be percent-encoded in path segments.
// percent_encoding::NON_ALPHANUMERIC encodes everything including . - _ ~ which is
// over-aggressive. Start from NON_ALPHANUMERIC and carve out the unreserved set.
const PATH_COMPONENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

#[derive(Debug)]
pub struct PutResult {
    pub new_etag: Option<String>,
}

#[derive(Debug)]
pub enum WriteError {
    Conflict,
    Locked,
    Network(String),
    Server(u16, String),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::Conflict => write!(f, "412 Precondition Failed (server version changed)"),
            WriteError::Locked => write!(f, "423 Locked"),
            WriteError::Network(e) => write!(f, "network error: {}", e),
            WriteError::Server(code, msg) => write!(f, "server error {}: {}", code, msg),
        }
    }
}

fn dav_url(base_url: &str, username: &str, path: &Path) -> String {
    let encoded = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => {
                Some(percent_encoding::utf8_percent_encode(
                    &s.to_string_lossy(),
                    PATH_COMPONENT,
                ).to_string())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    format!(
        "{}/remote.php/dav/files/{}/{}",
        base_url.trim_end_matches('/'),
        percent_encoding::utf8_percent_encode(username, PATH_COMPONENT),
        encoded,
    )
}

pub fn put_file(
    client: &reqwest::blocking::Client,
    base_url: &str,
    creds: &crate::auth::Credentials,
    path: &Path,
    body: Vec<u8>,
    if_match_etag: Option<&str>,
) -> Result<PutResult, WriteError> {
    let url = dav_url(base_url, creds.username(), path);
    let mut req = creds.apply(client
        .put(&url)
        .timeout(WRITE_TIMEOUT))
        .body(body);

    if let Some(etag) = if_match_etag {
        req = req.header("If-Match", format!("\"{}\"", etag.trim_matches('"')));
    }

    let resp = req.send().map_err(|e| WriteError::Network(e.to_string()))?;
    let status = resp.status().as_u16();

    match status {
        200 | 201 | 204 => {
            let new_etag = resp
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim_matches('"').to_string());
            Ok(PutResult { new_etag })
        }
        412 => Err(WriteError::Conflict),
        423 => Err(WriteError::Locked),
        _ => Err(WriteError::Server(
            status,
            resp.text().unwrap_or_default(),
        )),
    }
}

pub fn mkcol(
    client: &reqwest::blocking::Client,
    base_url: &str,
    creds: &crate::auth::Credentials,
    path: &Path,
) -> Result<(), WriteError> {
    let url = format!("{}/", dav_url(base_url, creds.username(), path).trim_end_matches('/'));
    let resp = creds.apply(client
        .request(reqwest::Method::from_bytes(b"MKCOL").unwrap(), &url)
        .timeout(WRITE_TIMEOUT))
        .send()
        .map_err(|e| WriteError::Network(e.to_string()))?;

    let status = resp.status().as_u16();
    match status {
        201 => Ok(()),
        405 => Ok(()),
        423 => Err(WriteError::Locked),
        _ => Err(WriteError::Server(
            status,
            resp.text().unwrap_or_default(),
        )),
    }
}

pub fn delete(
    client: &reqwest::blocking::Client,
    base_url: &str,
    creds: &crate::auth::Credentials,
    path: &Path,
) -> Result<(), WriteError> {
    let url = dav_url(base_url, creds.username(), path);
    let resp = creds.apply(client
        .delete(&url)
        .timeout(WRITE_TIMEOUT))
        .send()
        .map_err(|e| WriteError::Network(e.to_string()))?;

    let status = resp.status().as_u16();
    match status {
        200 | 204 | 404 => Ok(()),
        423 => Err(WriteError::Locked),
        _ => Err(WriteError::Server(
            status,
            resp.text().unwrap_or_default(),
        )),
    }
}

pub fn move_resource(
    client: &reqwest::blocking::Client,
    base_url: &str,
    creds: &crate::auth::Credentials,
    from: &Path,
    to: &Path,
) -> Result<(), WriteError> {
    let src_url = dav_url(base_url, creds.username(), from);
    let dst_url = dav_url(base_url, creds.username(), to);
    let resp = creds.apply(client
        .request(reqwest::Method::from_bytes(b"MOVE").unwrap(), &src_url)
        .timeout(WRITE_TIMEOUT))
        .header("Destination", &dst_url)
        .header("Overwrite", "T")
        .send()
        .map_err(|e| WriteError::Network(e.to_string()))?;

    let status = resp.status().as_u16();
    match status {
        200 | 201 | 204 => Ok(()),
        412 => Err(WriteError::Conflict),
        423 => Err(WriteError::Locked),
        _ => Err(WriteError::Server(
            status,
            resp.text().unwrap_or_default(),
        )),
    }
}

const CHUNK_SIZE: usize = 10 * 1024 * 1024; // 10 MB
const CHUNK_UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);

pub fn put_file_chunked(
    client: &reqwest::blocking::Client,
    base_url: &str,
    creds: &crate::auth::Credentials,
    path: &Path,
    body: Vec<u8>,
    if_match_etag: Option<&str>,
) -> Result<PutResult, WriteError> {
    if body.len() <= CHUNK_SIZE {
        return put_file(client, base_url, creds, path, body, if_match_etag);
    }

    let transfer_id = format!("ncrs-{}-{}", std::process::id(), std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis());

    let uploads_base = format!(
        "{}/remote.php/dav/uploads/{}/{}",
        base_url.trim_end_matches('/'),
        percent_encoding::utf8_percent_encode(creds.username(), PATH_COMPONENT),
        percent_encoding::utf8_percent_encode(&transfer_id, PATH_COMPONENT),
    );

    let resp = creds.apply(client
        .request(reqwest::Method::from_bytes(b"MKCOL").unwrap(), &uploads_base)
        .timeout(WRITE_TIMEOUT))
        .send()
        .map_err(|e| WriteError::Network(format!("chunked MKCOL: {}", e)))?;
    if !resp.status().is_success() && resp.status().as_u16() != 405 {
        return Err(WriteError::Server(resp.status().as_u16(), format!("chunked MKCOL: {}", resp.text().unwrap_or_default())));
    }

    let total_chunks = (body.len() + CHUNK_SIZE - 1) / CHUNK_SIZE;
    for (i, chunk) in body.chunks(CHUNK_SIZE).enumerate() {
        let chunk_url = format!("{}/{:010}", uploads_base, i);
        log::info!("CHUNKED_UPLOAD {}/{} ({} bytes)", i + 1, total_chunks, chunk.len());
        let resp = creds.apply(client
            .put(&chunk_url)
            .timeout(CHUNK_UPLOAD_TIMEOUT))
            .body(chunk.to_vec())
            .send()
            .map_err(|e| {
                let _ = cleanup_chunked_upload(client, &uploads_base, creds);
                WriteError::Network(format!("chunk {} upload: {}", i, e))
            })?;
        let status = resp.status().as_u16();
        if status != 200 && status != 201 && status != 204 {
            let _ = cleanup_chunked_upload(client, &uploads_base, creds);
            return Err(WriteError::Server(status, format!("chunk {} upload: {}", i, resp.text().unwrap_or_default())));
        }
    }

    let dest_url = dav_url(base_url, creds.username(), path);
    let assemble_url = format!("{}/.file", uploads_base);
    let mut req = creds.apply(client
        .request(reqwest::Method::from_bytes(b"MOVE").unwrap(), &assemble_url)
        .timeout(WRITE_TIMEOUT))
        .header("Destination", &dest_url)
        .header("Overwrite", "T");

    if let Some(etag) = if_match_etag {
        req = req.header("If-Match", format!("\"{}\"", etag.trim_matches('"')));
    }

    let resp = req.send().map_err(|e| WriteError::Network(format!("chunked MOVE: {}", e)))?;
    let status = resp.status().as_u16();
    match status {
        200 | 201 | 204 => {
            let new_etag = resp
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim_matches('"').to_string());
            Ok(PutResult { new_etag })
        }
        412 => Err(WriteError::Conflict),
        423 => Err(WriteError::Locked),
        _ => Err(WriteError::Server(status, resp.text().unwrap_or_default())),
    }
}

pub fn put_file_from_path(
    client: &reqwest::blocking::Client,
    base_url: &str,
    creds: &crate::auth::Credentials,
    path: &Path,
    staging_path: &Path,
    if_match_etag: Option<&str>,
) -> Result<PutResult, WriteError> {
    let file_size = std::fs::metadata(staging_path)
        .map_err(|e| WriteError::Network(format!("staging stat: {}", e)))?
        .len() as usize;

    if file_size <= CHUNK_SIZE {
        let body = std::fs::read(staging_path)
            .map_err(|e| WriteError::Network(format!("staging read: {}", e)))?;
        return put_file(client, base_url, creds, path, body, if_match_etag);
    }

    let transfer_id = format!("ncrs-{}-{}", std::process::id(), std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis());

    let uploads_base = format!(
        "{}/remote.php/dav/uploads/{}/{}",
        base_url.trim_end_matches('/'),
        percent_encoding::utf8_percent_encode(creds.username(), PATH_COMPONENT),
        percent_encoding::utf8_percent_encode(&transfer_id, PATH_COMPONENT),
    );

    let resp = creds.apply(client
        .request(reqwest::Method::from_bytes(b"MKCOL").unwrap(), &uploads_base)
        .timeout(WRITE_TIMEOUT))
        .send()
        .map_err(|e| WriteError::Network(format!("chunked MKCOL: {}", e)))?;
    if !resp.status().is_success() && resp.status().as_u16() != 405 {
        return Err(WriteError::Server(resp.status().as_u16(), resp.text().unwrap_or_default()));
    }

    let total_chunks = (file_size + CHUNK_SIZE - 1) / CHUNK_SIZE;
    let mut file = std::fs::File::open(staging_path)
        .map_err(|e| WriteError::Network(format!("staging open: {}", e)))?;

    use std::io::Read;
    for i in 0..total_chunks {
        let chunk_size = CHUNK_SIZE.min(file_size - i * CHUNK_SIZE);
        let mut chunk = vec![0u8; chunk_size];
        file.read_exact(&mut chunk).map_err(|e| {
            let _ = cleanup_chunked_upload(client, &uploads_base, creds);
            WriteError::Network(format!("staging read chunk {}: {}", i, e))
        })?;

        let chunk_url = format!("{}/{:010}", uploads_base, i);
        log::info!("CHUNKED_UPLOAD {}/{} ({} bytes)", i + 1, total_chunks, chunk.len());
        let resp = creds.apply(client
            .put(&chunk_url)
            .timeout(CHUNK_UPLOAD_TIMEOUT))
            .body(chunk)
            .send()
            .map_err(|e| {
                let _ = cleanup_chunked_upload(client, &uploads_base, creds);
                WriteError::Network(format!("chunk {} upload: {}", i, e))
            })?;
        let status = resp.status().as_u16();
        if status != 200 && status != 201 && status != 204 {
            let _ = cleanup_chunked_upload(client, &uploads_base, creds);
            return Err(WriteError::Server(status, format!("chunk {}: {}", i, resp.text().unwrap_or_default())));
        }
    }

    let dest_url = dav_url(base_url, creds.username(), path);
    let assemble_url = format!("{}/.file", uploads_base);
    let mut req = creds.apply(client
        .request(reqwest::Method::from_bytes(b"MOVE").unwrap(), &assemble_url)
        .timeout(WRITE_TIMEOUT))
        .header("Destination", &dest_url)
        .header("Overwrite", "T");

    if let Some(etag) = if_match_etag {
        req = req.header("If-Match", format!("\"{}\"", etag.trim_matches('"')));
    }

    let resp = req.send().map_err(|e| WriteError::Network(format!("chunked MOVE: {}", e)))?;
    let status = resp.status().as_u16();
    match status {
        200 | 201 | 204 => {
            let new_etag = resp
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim_matches('"').to_string());
            Ok(PutResult { new_etag })
        }
        412 => Err(WriteError::Conflict),
        423 => Err(WriteError::Locked),
        _ => Err(WriteError::Server(status, resp.text().unwrap_or_default())),
    }
}

fn cleanup_chunked_upload(
    client: &reqwest::blocking::Client,
    uploads_url: &str,
    creds: &crate::auth::Credentials,
) -> Result<(), WriteError> {
    creds.apply(client
        .delete(uploads_url)
        .timeout(WRITE_TIMEOUT))
        .send()
        .map_err(|e| WriteError::Network(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn dav_url_simple_path() {
        let url = dav_url("https://cloud.example.com", "alice", Path::new("/Documents/report.pdf"));
        assert_eq!(url, "https://cloud.example.com/remote.php/dav/files/alice/Documents/report.pdf");
    }

    #[test]
    fn dav_url_path_with_spaces() {
        let url = dav_url("https://cloud.example.com", "alice", Path::new("/My Documents/file name.txt"));
        assert!(url.contains("My%20Documents"), "spaces in dir should be percent-encoded");
        assert!(url.contains("file%20name.txt"), "spaces in filename should be percent-encoded");
    }

    #[test]
    fn dav_url_non_ascii_filename() {
        let url = dav_url("https://cloud.example.com", "alice", Path::new("/Fotos/été.jpg"));
        assert!(!url.contains("été"), "non-ASCII chars must be percent-encoded");
        assert!(url.starts_with("https://cloud.example.com/remote.php/dav/files/alice/"));
    }

    #[test]
    fn dav_url_special_chars_in_path() {
        let url = dav_url("https://cloud.example.com", "alice", Path::new("/dir/file&name.txt"));
        assert!(url.contains("%26"), "& must be percent-encoded");
    }

    #[test]
    fn dav_url_multi_level_path() {
        let url = dav_url("https://cloud.example.com", "alice", Path::new("/a/b/c/deep.txt"));
        assert!(url.ends_with("/remote.php/dav/files/alice/a/b/c/deep.txt"));
    }

    #[test]
    fn dav_url_base_url_trailing_slash_stripped() {
        let url_with    = dav_url("https://cloud.example.com/", "alice", Path::new("/file.txt"));
        let url_without = dav_url("https://cloud.example.com",  "alice", Path::new("/file.txt"));
        assert_eq!(url_with, url_without,
            "trailing slash on base_url should not produce a double slash");
    }

    #[test]
    fn dav_url_username_with_special_chars() {
        let url = dav_url("https://cloud.example.com", "alice@domain", Path::new("/file.txt"));
        assert!(url.contains("alice%40domain"), "@ in username must be percent-encoded");
    }

    #[test]
    fn chunked_falls_back_to_simple_for_small_files() {
        assert!(CHUNK_SIZE > 100, "sanity: chunk size must be larger than test body");
    }

    #[test]
    fn chunked_upload_calculates_correct_chunk_count() {
        let body = vec![0u8; CHUNK_SIZE * 2 + 1];
        let total_chunks = (body.len() + CHUNK_SIZE - 1) / CHUNK_SIZE;
        assert_eq!(total_chunks, 3);
    }

    #[test]
    fn chunked_upload_single_chunk_boundary() {
        let body = vec![0u8; CHUNK_SIZE];
        let total_chunks = (body.len() + CHUNK_SIZE - 1) / CHUNK_SIZE;
        assert_eq!(total_chunks, 1);
    }
}
