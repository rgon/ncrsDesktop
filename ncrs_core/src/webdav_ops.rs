use std::path::Path;
use std::time::Duration;

const WRITE_TIMEOUT: Duration = Duration::from_secs(60);

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
                    percent_encoding::NON_ALPHANUMERIC,
                ).to_string())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    format!(
        "{}/remote.php/dav/files/{}/{}",
        base_url.trim_end_matches('/'),
        percent_encoding::utf8_percent_encode(username, percent_encoding::NON_ALPHANUMERIC),
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
    let url = dav_url(base_url, username, path);
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
        200 | 204 => Ok(()),
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
