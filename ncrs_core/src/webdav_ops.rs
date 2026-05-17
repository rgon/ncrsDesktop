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
    username: &str,
    password: &str,
    path: &Path,
    body: Vec<u8>,
    if_match_etag: Option<&str>,
) -> Result<PutResult, WriteError> {
    let url = dav_url(base_url, username, path);
    let mut req = client
        .put(&url)
        .timeout(WRITE_TIMEOUT)
        .basic_auth(username, Some(password))
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
    username: &str,
    password: &str,
    path: &Path,
) -> Result<(), WriteError> {
    let url = format!("{}/", dav_url(base_url, username, path).trim_end_matches('/'));
    let resp = client
        .request(reqwest::Method::from_bytes(b"MKCOL").unwrap(), &url)
        .timeout(WRITE_TIMEOUT)
        .basic_auth(username, Some(password))
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
    username: &str,
    password: &str,
    path: &Path,
) -> Result<(), WriteError> {
    let url = dav_url(base_url, username, path);
    let resp = client
        .delete(&url)
        .timeout(WRITE_TIMEOUT)
        .basic_auth(username, Some(password))
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
    username: &str,
    password: &str,
    from: &Path,
    to: &Path,
) -> Result<(), WriteError> {
    let src_url = dav_url(base_url, username, from);
    let dst_url = dav_url(base_url, username, to);
    let resp = client
        .request(reqwest::Method::from_bytes(b"MOVE").unwrap(), &src_url)
        .timeout(WRITE_TIMEOUT)
        .basic_auth(username, Some(password))
        .header("Destination", &dst_url)
        .header("Overwrite", "F")
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
}
