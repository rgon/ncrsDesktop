use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

const PROPFIND_BODY: &str = r#"<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:" xmlns:nc="http://nextcloud.org/ns" xmlns:oc="http://owncloud.org/ns">
  <d:prop>
    <d:getcontentlength />
    <d:getcontenttype />
    <d:getetag />
    <d:getlastmodified />
    <d:resourcetype />
    <oc:size />
    <oc:permissions />
    <oc:fileid />
    <oc:owner-id />
    <oc:owner-display-name />
    <nc:has-preview />
    <oc:share-types />
  </d:prop>
</d:propfind>"#;

const PROPFIND_ETAG_BODY: &str = r#"<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:" xmlns:nc="http://nextcloud.org/ns" xmlns:oc="http://owncloud.org/ns">
  <d:prop>
    <d:getetag />
  </d:prop>
</d:propfind>"#;

/// Why a PROPFIND failed, kept structured so callers classify on the variant
/// and never on the rendered message (which embeds the path: a directory named
/// `2404` or `timeout` must not read as a 404 or a timeout).
#[derive(Debug)]
pub enum PropfindError {
    /// The server answered, with something other than 207/2xx.
    Status { op: &'static str, path: PathBuf, code: u16, reason: String },
    /// No answer: connect/TLS/reset failure, or the request timed out before
    /// the response headers arrived.
    Transport { op: &'static str, path: PathBuf, timed_out: bool, msg: String },
    /// The server answered 207 but the body broke off or did not parse.
    Body(String),
}

impl std::fmt::Display for PropfindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Status { op, path, code, reason } => {
                write!(f, "{} {} returned {} {}", op, path.display(), code, reason)
            }
            Self::Transport { op, path, msg, .. } => write!(f, "{} {}: {}", op, path.display(), msg),
            Self::Body(msg) => write!(f, "{}", msg),
        }
    }
}

impl From<String> for PropfindError {
    fn from(msg: String) -> Self {
        Self::Body(msg)
    }
}

impl PropfindError {
    fn transport(op: &'static str, path: &std::path::Path, e: reqwest::Error) -> Self {
        Self::Transport { op, path: path.to_path_buf(), timed_out: e.is_timeout(), msg: e.to_string() }
    }

    fn status(op: &'static str, path: &std::path::Path, status: reqwest::StatusCode) -> Self {
        Self::Status {
            op,
            path: path.to_path_buf(),
            code: status.as_u16(),
            reason: status.canonical_reason().unwrap_or("").to_string(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DavEntry {
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub etag: Option<String>,
    pub content_type: Option<String>,
    pub has_preview: bool,
    pub is_shared: bool,
    pub permissions: Option<String>,
    pub fileid: Option<u64>,
    pub owner_id: Option<String>,
    pub owner_display_name: Option<String>,
}

pub fn propfind_list(
    client: &crate::http_clients::DavClient,
    webdav_url: &str,
    creds: &crate::auth::Credentials,
    path: &std::path::Path,
    timeout: Duration,
) -> Result<(Option<String>, Option<DavEntry>, Vec<DavEntry>), PropfindError> {
    let url = build_url(webdav_url, path);
    let t0 = Instant::now();
    log::info!("PROPFIND {} start", path.display());

    let resp = creds.apply(client
        .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), &url)
        .timeout(timeout)
        .header("Depth", "1")
        .header("Content-Type", "application/xml"))
        .body(PROPFIND_BODY)
        .send()
        .map_err(|e| PropfindError::transport("PROPFIND", path, e))?;

    log::info!("PROPFIND {} response {} in {:?}", path.display(), resp.status(), t0.elapsed());

    let status = resp.status();
    if status != reqwest::StatusCode::MULTI_STATUS && !status.is_success() {
        return Err(PropfindError::status("PROPFIND", path, status));
    }

    let reader = std::io::BufReader::new(resp);
    let result = parse_multistatus_stream(reader, webdav_url);
    log::info!("PROPFIND {} parsed in {:?}", path.display(), t0.elapsed());
    Ok(result?)
}

pub fn propfind_status(
    client: &crate::http_clients::DavClient,
    webdav_url: &str,
    creds: &crate::auth::Credentials,
    path: &std::path::Path,
    timeout: Duration,
) -> Result<(), u16> {
    let url = build_url(webdav_url, path);
    let resp = creds.apply(client
        .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), &url)
        .timeout(timeout)
        .header("Depth", "0")
        .header("Content-Type", "application/xml"))
        .body(PROPFIND_ETAG_BODY)
        .send()
        .map_err(|_| 0u16)?;

    let status = resp.status();
    if status == reqwest::StatusCode::MULTI_STATUS || status.is_success() {
        Ok(())
    } else {
        Err(status.as_u16())
    }
}

pub fn propfind_etag(
    client: &crate::http_clients::DavClient,
    webdav_url: &str,
    creds: &crate::auth::Credentials,
    path: &std::path::Path,
    timeout: Duration,
) -> Result<Option<String>, PropfindError> {
    let url = build_url(webdav_url, path);
    log::debug!("PROPFIND_ETAG {}", url);

    let resp = creds.apply(client
        .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), &url)
        .timeout(timeout)
        .header("Depth", "0")
        .header("Content-Type", "application/xml"))
        .body(PROPFIND_ETAG_BODY)
        .send()
        .map_err(|e| PropfindError::transport("PROPFIND_ETAG", path, e))?;

    let status = resp.status();
    if status != reqwest::StatusCode::MULTI_STATUS && !status.is_success() {
        return Err(PropfindError::status("PROPFIND_ETAG", path, status));
    }

    let reader = std::io::BufReader::new(resp);
    let (dir_etag, _, _) = parse_multistatus_stream(reader, webdav_url)?;
    Ok(dir_etag)
}

/// The paths of the files with these ids, in no particular order.
pub fn resolve_fileids(
    client: &crate::http_clients::DavClient,
    webdav_url: &str,
    creds: &crate::auth::Credentials,
    file_ids: &[u64],
    timeout: Duration,
) -> Result<Vec<PathBuf>, String> {
    Ok(search_fileids(client, webdav_url, creds, file_ids, timeout)?
        .into_iter()
        .map(|e| e.path)
        .collect())
}

/// The same lookup keyed by id, for callers that have to map a specific hit
/// back to the file it came from rather than just collect the set of paths.
///
/// Hits the server returns without an `oc:fileid` are dropped — there is no id
/// to key them by — which is why [`resolve_fileids`] does not go through here.
pub fn resolve_fileid_paths(
    client: &crate::http_clients::DavClient,
    webdav_url: &str,
    creds: &crate::auth::Credentials,
    file_ids: &[u64],
    timeout: Duration,
) -> Result<std::collections::HashMap<u64, PathBuf>, String> {
    Ok(search_fileids(client, webdav_url, creds, file_ids, timeout)?
        .into_iter()
        .filter_map(|e| e.fileid.map(|id| (id, e.path)))
        .collect())
}

fn search_fileids(
    client: &crate::http_clients::DavClient,
    webdav_url: &str,
    creds: &crate::auth::Credentials,
    file_ids: &[u64],
    timeout: Duration,
) -> Result<Vec<DavEntry>, String> {
    if file_ids.is_empty() {
        return Ok(Vec::new());
    }

    let parsed = url::Url::parse(webdav_url)
        .map_err(|e| format!("bad webdav_url: {}", e))?;
    let dav_root = {
        let path = parsed.path();
        let idx = path.find("/files/").unwrap_or(path.len());
        // Origin, not scheme + host_str: the latter drops the port, so a server
        // on https://cloud.example.com:8443 was sent this request — with its
        // credentials attached — to port 443 instead.
        format!("{}{}/", parsed.origin().ascii_serialization(), &path[..idx])
    };
    let scope = {
        let path = parsed.path().trim_end_matches('/');
        let idx = path.find("/files/").unwrap_or(0);
        path[idx..].to_string()
    };

    let where_clause = if file_ids.len() == 1 {
        format!(
            "<d:eq><d:prop><oc:fileid/></d:prop><d:literal>{}</d:literal></d:eq>",
            file_ids[0]
        )
    } else {
        let eqs: Vec<String> = file_ids.iter().map(|id| {
            format!("<d:eq><d:prop><oc:fileid/></d:prop><d:literal>{}</d:literal></d:eq>", id)
        }).collect();
        format!("<d:or>{}</d:or>", eqs.join(""))
    };

    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<d:searchrequest xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">
  <d:basicsearch>
    <d:select><d:prop><oc:fileid/></d:prop></d:select>
    <d:from><d:scope><d:href>{scope}</d:href><d:depth>infinity</d:depth></d:scope></d:from>
    <d:where>{where_clause}</d:where>
    <d:orderby/>
  </d:basicsearch>
</d:searchrequest>"#,
        scope = scope,
        where_clause = where_clause,
    );

    log::debug!("SEARCH resolve_fileids {:?} at {}", file_ids, dav_root);

    let resp = creds.apply(client
        .request(reqwest::Method::from_bytes(b"SEARCH").unwrap(), &dav_root)
        .timeout(timeout)
        .header("Content-Type", "text/xml"))
        .body(body)
        .send()
        .map_err(|e| format!("SEARCH resolve_fileids: {}", e))?;

    let status = resp.status();
    if status != reqwest::StatusCode::MULTI_STATUS && !status.is_success() {
        return Err(format!("SEARCH resolve_fileids returned {}", status));
    }

    let reader = std::io::BufReader::new(resp);
    let (_, self_entry, mut hits) = parse_multistatus_stream(reader, webdav_url)?;

    // The parser reserves the first response for the collection a PROPFIND was
    // issued against; in a SEARCH result it is an ordinary hit.
    if let Some(se) = self_entry {
        if se.path != PathBuf::from("/") {
            hits.push(se);
        }
    }

    log::info!(
        "SEARCH resolve_fileids {:?} → {:?}",
        file_ids,
        hits.iter().map(|e| &e.path).collect::<Vec<_>>()
    );
    Ok(hits)
}

fn build_url(webdav_url: &str, path: &std::path::Path) -> String {
    let base = webdav_url.trim_end_matches('/');
    let rel = path.strip_prefix("/").unwrap_or(path);
    if rel == std::path::Path::new("") {
        format!("{}/", base)
    } else {
        let encoded = percent_encoding::utf8_percent_encode(
            &rel.to_string_lossy(),
            super::PATH_ENCODE,
        )
        .to_string();
        format!("{}/{}", base, encoded)
    }
}

// ── XML parser ──────────────────────────────────────────────────────────────

fn parse_multistatus_stream<R: std::io::BufRead>(
    reader: R,
    webdav_url: &str,
) -> Result<(Option<String>, Option<DavEntry>, Vec<DavEntry>), String> {
    let mut entries = Vec::new();
    let mut dir_etag: Option<String> = None;
    let mut self_entry: Option<DavEntry> = None;
    let prefix = webdav_prefix(webdav_url);

    let xml_reader = quick_xml::Reader::from_reader(reader);
    let mut buf_reader = ResponseReader::new(xml_reader);

    let mut is_first = true;
    while let Some(resp) = buf_reader.next_response()? {
        let remote_path = match href_to_remote_path(&resp.href, &prefix) {
            Some(p) => p,
            None => {
                // Keep the self-entry slot aligned: the first response is the
                // directory itself, so dropping it must not promote a child
                // into its place.
                if is_first {
                    dir_etag = resp.etag;
                    is_first = false;
                }
                continue;
            }
        };

        let entry = DavEntry {
            path: remote_path,
            is_dir: resp.is_collection,
            size: if resp.is_collection { resp.oc_size } else { resp.content_length },
            modified: resp.last_modified,
            etag: resp.etag.clone(),
            content_type: resp.content_type,
            has_preview: resp.has_preview,
            is_shared: resp.is_shared,
            permissions: resp.permissions,
            fileid: resp.fileid,
            owner_id: resp.owner_id,
            owner_display_name: resp.owner_display_name,
        };

        if is_first {
            dir_etag = resp.etag;
            self_entry = Some(entry);
            is_first = false;
        } else {
            entries.push(entry);
        }
    }

    Ok((dir_etag, self_entry, entries))
}

pub fn propfind_list_streaming<E: From<DavEntry> + Send>(
    client: &crate::http_clients::DavClient,
    webdav_url: &str,
    creds: &crate::auth::Credentials,
    path: &std::path::Path,
    timeout: Duration,
    tx: std::sync::mpsc::Sender<E>,
    self_tx: std::sync::mpsc::Sender<E>,
) -> Result<Option<String>, PropfindError> {
    let url = build_url(webdav_url, path);
    let t0 = Instant::now();
    log::info!("PROPFIND_STREAM {} start", path.display());

    let resp = creds.apply(client
        .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), &url)
        .timeout(timeout)
        .header("Depth", "1")
        .header("Content-Type", "application/xml"))
        .body(PROPFIND_BODY)
        .send()
        .map_err(|e| PropfindError::transport("PROPFIND", path, e))?;

    log::info!("PROPFIND_STREAM {} response {} in {:?}", path.display(), resp.status(), t0.elapsed());

    let status = resp.status();
    if status != reqwest::StatusCode::MULTI_STATUS && !status.is_success() {
        return Err(PropfindError::status("PROPFIND", path, status));
    }

    let reader = std::io::BufReader::new(resp);
    let prefix = webdav_prefix(webdav_url);
    let xml_reader = quick_xml::Reader::from_reader(reader);
    let mut buf_reader = ResponseReader::new(xml_reader);

    let mut dir_etag: Option<String> = None;
    let mut count: usize = 0;
    let mut is_first = true;
    while let Some(resp) = buf_reader.next_response()? {
        let remote_path = match href_to_remote_path(&resp.href, &prefix) {
            Some(p) => p,
            None => {
                // Keep the self-entry slot aligned: the first response is the
                // directory itself, so dropping it must not promote a child
                // into its place.
                if is_first {
                    dir_etag = resp.etag;
                    is_first = false;
                }
                continue;
            }
        };
        let entry = DavEntry {
            path: remote_path,
            is_dir: resp.is_collection,
            size: if resp.is_collection { resp.oc_size } else { resp.content_length },
            modified: resp.last_modified,
            etag: resp.etag.clone(),
            content_type: resp.content_type,
            has_preview: resp.has_preview,
            is_shared: resp.is_shared,
            permissions: resp.permissions,
            fileid: resp.fileid,
            owner_id: resp.owner_id,
            owner_display_name: resp.owner_display_name,
        };
        if is_first {
            dir_etag = resp.etag;
            let _ = self_tx.send(E::from(entry));
            is_first = false;
        } else {
            count += 1;
            if tx.send(E::from(entry)).is_err() {
                break;
            }
        }
    }
    log::info!("PROPFIND_STREAM {} done: {} entries in {:?}", path.display(), count, t0.elapsed());
    Ok(dir_etag)
}

#[cfg(test)]
fn parse_multistatus_str(
    xml: &str,
    webdav_url: &str,
) -> Result<(Option<String>, Option<DavEntry>, Vec<DavEntry>), String> {
    parse_multistatus_stream(xml.as_bytes(), webdav_url)
}

fn webdav_prefix(webdav_url: &str) -> String {
    match url::Url::parse(webdav_url) {
        Ok(u) => u.path().trim_end_matches('/').to_string(),
        Err(_) => String::new(),
    }
}

/// Converts a `<d:href>` from a PROPFIND response into a remote path.
///
/// Returns `None` for an href carrying `..`. Those segments are stripped here
/// rather than relied upon to be harmless downstream: nothing about a path in a
/// server response is trustworthy, and the reason a malicious `..` cannot
/// currently reach a local write is an invariant elsewhere (the inode namespace
/// is rebuilt from `file_name()` alone) that no code enforces. Rejecting at the
/// parse boundary means a future caller that does join a cached entry path onto
/// a local directory cannot be made to escape it.
///
/// Also returns `None` for an href whose decoded path contains a control
/// character (e.g. a raw `\n`). Nothing downstream expects one — in
/// particular, filenames reach shell-integration helpers (Nautilus, the
/// thumbnailer) over a newline-delimited local IPC protocol, where an
/// embedded `\n` would be parsed as a second, attacker-chosen command. Local
/// writes already go through `filename_validation`; server-supplied names
/// need the same floor.
fn href_to_remote_path(href: &str, prefix: &str) -> Option<PathBuf> {
    let decoded = percent_encoding::percent_decode_str(href)
        .decode_utf8_lossy()
        .into_owned();
    if decoded.split(['/', '\\']).any(|seg| seg == "..") {
        log::warn!("PROPFIND: ignoring entry with parent-directory segment in href {:?}", href);
        return None;
    }
    if decoded.chars().any(|c| c.is_control()) {
        log::warn!("PROPFIND: ignoring entry with control character in href {:?}", href);
        return None;
    }
    let trimmed = decoded.trim_end_matches('/');
    let remote = if !prefix.is_empty() && trimmed.starts_with(prefix) {
        &trimmed[prefix.len()..]
    } else {
        trimmed
    };
    Some(if remote.is_empty() {
        PathBuf::from("/")
    } else if remote.starts_with('/') {
        PathBuf::from(remote)
    } else {
        PathBuf::from(format!("/{}", remote))
    })
}

// ── Streaming XML response reader ───────────────────────────────────────────

#[derive(Default)]
struct RawResponse {
    href: String,
    is_collection: bool,
    content_length: u64,
    oc_size: u64,
    last_modified: Option<SystemTime>,
    etag: Option<String>,
    content_type: Option<String>,
    has_preview: bool,
    is_shared: bool,
    permissions: Option<String>,
    fileid: Option<u64>,
    owner_id: Option<String>,
    owner_display_name: Option<String>,
}

struct ResponseReader<R: std::io::BufRead> {
    reader: quick_xml::Reader<R>,
    buf: Vec<u8>,
}

impl<R: std::io::BufRead> ResponseReader<R> {
    fn new(reader: quick_xml::Reader<R>) -> Self {
        Self { reader, buf: Vec::with_capacity(256) }
    }

    fn next_response(&mut self) -> Result<Option<RawResponse>, String> {
        use quick_xml::events::Event;

        loop {
            self.buf.clear();
            match self.reader.read_event_into(&mut self.buf) {
                Ok(Event::Start(ref e)) if tag_local_name(e.name().as_ref()) == "response" => {
                    return self.read_response().map(Some);
                }
                Ok(Event::Eof) => return Ok(None),
                Err(e) => return Err(format!("XML parse: {}", e)),
                _ => {}
            }
        }
    }

    fn read_response(&mut self) -> Result<RawResponse, String> {
        use quick_xml::events::Event;

        let mut resp = RawResponse::default();
        let mut depth = 1u32;

        loop {
            self.buf.clear();
            match self.reader.read_event_into(&mut self.buf) {
                Ok(Event::Start(ref e)) => {
                    let name = e.name();
                    let tag = tag_local_name(name.as_ref());
                    match tag {
                        "response" => depth += 1,
                        "href" => resp.href = self.read_text()?,
                        "propstat" => self.read_propstat(&mut resp)?,
                        _ => {}
                    }
                }
                Ok(Event::End(ref e)) => {
                    let name = e.name();
                    if tag_local_name(name.as_ref()) != "response" { continue; }
                    depth -= 1;
                    if depth == 0 {
                        return Ok(resp);
                    }
                }
                Ok(Event::Eof) => return Err("unexpected EOF in response".into()),
                Err(e) => return Err(format!("XML parse: {}", e)),
                _ => {}
            }
        }
    }

    fn read_propstat(&mut self, resp: &mut RawResponse) -> Result<(), String> {
        use quick_xml::events::Event;

        loop {
            self.buf.clear();
            match self.reader.read_event_into(&mut self.buf) {
                Ok(Event::Start(ref e)) => {
                    let name = e.name();
                    let tag = tag_local_name(name.as_ref());
                    match tag {
                        "resourcetype" => resp.is_collection = self.read_has_collection()?,
                        "getcontentlength" => {
                            resp.content_length = self.read_text()?.parse().unwrap_or(0);
                        }
                        "size" => {
                            resp.oc_size = self.read_text()?.parse().unwrap_or(0);
                        }
                        "getetag" => {
                            let raw = self.read_text()?;
                            let etag = raw.trim().trim_matches('"').trim();
                            // A server with no ETag for this resource can still answer the
                            // named property with an empty value — rclone returns
                            // <getetag></getetag> under a 404 propstat for collections.
                            // Storing "" as a token is worse than storing nothing: every
                            // later comparison matches, so the resource looks unchanged
                            // forever and is never re-listed or re-downloaded. Report the
                            // absence honestly and let the caller fetch.
                            resp.etag = if etag.is_empty() {
                                None
                            } else {
                                Some(etag.to_string())
                            };
                        }
                        "getcontenttype" => {
                            resp.content_type = Some(self.read_text()?);
                        }
                        "getlastmodified" => {
                            let raw = self.read_text()?;
                            resp.last_modified = parse_http_date(&raw);
                        }
                        "has-preview" => {
                            resp.has_preview = self.read_text()? == "true";
                        }
                        "permissions" => {
                            resp.permissions = Some(self.read_text()?);
                        }
                        "fileid" => {
                            // Only overwrite with a value that parsed. A server
                            // may echo the requested property back empty under a
                            // trailing 404 propstat (see `getetag` above), and
                            // this loop does not separate propstats by status —
                            // so a bare `<oc:fileid/>` must not erase the id the
                            // 200 propstat already gave us.
                            if let Ok(id) = self.read_text()?.parse() {
                                resp.fileid = Some(id);
                            }
                        }
                        "owner-id" => {
                            resp.owner_id = Some(self.read_text()?);
                        }
                        "owner-display-name" => {
                            resp.owner_display_name = Some(self.read_text()?);
                        }
                        "share-types" => {
                            resp.is_shared = self.read_has_children("share-types")?;
                        }
                        _ => {}
                    }
                }
                Ok(Event::End(ref e)) => {
                    let name = e.name();
                    if tag_local_name(name.as_ref()) == "propstat" {
                        return Ok(());
                    }
                }
                Ok(Event::Eof) => return Err("unexpected EOF in propstat".into()),
                Err(e) => return Err(format!("XML parse: {}", e)),
                _ => {}
            }
        }
    }

    fn read_has_collection(&mut self) -> Result<bool, String> {
        use quick_xml::events::Event;
        let mut found = false;
        loop {
            self.buf.clear();
            match self.reader.read_event_into(&mut self.buf) {
                Ok(Event::Start(ref e) | Event::Empty(ref e))
                    if tag_local_name(e.name().as_ref()) == "collection" =>
                {
                    found = true;
                }
                Ok(Event::End(ref e)) if tag_local_name(e.name().as_ref()) == "resourcetype" => {
                    return Ok(found);
                }
                Ok(Event::Eof) => return Err("unexpected EOF in resourcetype".into()),
                Err(e) => return Err(format!("XML parse: {}", e)),
                _ => {}
            }
        }
    }

    fn read_has_children(&mut self, end_tag: &str) -> Result<bool, String> {
        use quick_xml::events::Event;
        let mut found = false;
        loop {
            self.buf.clear();
            match self.reader.read_event_into(&mut self.buf) {
                Ok(Event::Start(_) | Event::Empty(_)) => found = true,
                Ok(Event::End(ref e)) => {
                    let name = e.name();
                    if tag_local_name(name.as_ref()) == end_tag {
                        return Ok(found);
                    }
                }
                Ok(Event::Eof) => return Ok(found),
                Err(e) => return Err(format!("XML parse: {}", e)),
                _ => {}
            }
        }
    }

    fn read_text(&mut self) -> Result<String, String> {
        use quick_xml::events::Event;
        let mut text = String::new();
        loop {
            self.buf.clear();
            match self.reader.read_event_into(&mut self.buf) {
                Ok(Event::Text(ref e)) => {
                    text.push_str(&e.unescape().map_err(|e| e.to_string())?);
                }
                Ok(Event::End(_)) => return Ok(text),
                Ok(Event::Start(_)) => {
                    self.skip_element()?;
                }
                Ok(Event::Eof) => return Ok(text),
                Err(e) => return Err(format!("XML parse: {}", e)),
                _ => {}
            }
        }
    }

    fn skip_element(&mut self) -> Result<(), String> {
        use quick_xml::events::Event;
        let mut depth = 1u32;
        loop {
            self.buf.clear();
            match self.reader.read_event_into(&mut self.buf) {
                Ok(Event::Start(_)) => depth += 1,
                Ok(Event::End(_)) => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(());
                    }
                }
                Ok(Event::Eof) => return Ok(()),
                Err(e) => return Err(format!("XML parse: {}", e)),
                _ => {}
            }
        }
    }
}

const PROPFIND_QUOTA_BODY: &str = r#"<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:">
  <d:prop>
    <d:quota-used-bytes />
    <d:quota-available-bytes />
  </d:prop>
</d:propfind>"#;

pub fn propfind_quota(
    client: &crate::http_clients::DavClient,
    webdav_url: &str,
    creds: &crate::auth::Credentials,
    timeout: Duration,
) -> Result<(u64, u64), String> {
    let resp = creds.apply(client
        .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), webdav_url)
        .header("Depth", "0")
        .header("Content-Type", "application/xml"))
        .body(PROPFIND_QUOTA_BODY)
        .timeout(timeout)
        .send()
        .map_err(|e| format!("PROPFIND quota: {}", e))?;

    if !resp.status().is_success() && resp.status().as_u16() != 207 {
        return Err(format!("PROPFIND quota: HTTP {}", resp.status()));
    }

    let body = resp.text().map_err(|e| format!("PROPFIND quota body: {}", e))?;
    let mut used: Option<u64> = None;
    let mut available: Option<u64> = None;

    let mut reader = quick_xml::Reader::from_str(&body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut current_tag = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(ref e)) => {
                current_tag = tag_local_name(e.name().as_ref()).to_string();
            }
            Ok(quick_xml::events::Event::Text(ref e)) => {
                if let Ok(text) = e.unescape() {
                    match current_tag.as_str() {
                        "quota-used-bytes" => used = text.trim().parse().ok(),
                        "quota-available-bytes" => {
                            let v: i64 = text.trim().parse().unwrap_or(-1);
                            if v >= 0 { available = Some(v as u64); }
                        }
                        _ => {}
                    }
                }
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    match (used, available) {
        (Some(u), Some(a)) => Ok((u, u + a)),
        (Some(u), None) => Ok((u, 0)),
        _ => Err("PROPFIND quota: missing quota-used-bytes".into()),
    }
}

fn tag_local_name(full: &[u8]) -> &str {
    let s = std::str::from_utf8(full).unwrap_or("");
    s.rsplit_once(':').map(|(_, local)| local).unwrap_or(s)
}

fn parse_http_date(s: &str) -> Option<SystemTime> {
    // RFC 7231 date: "Mon, 12 May 2026 10:00:00 GMT"
    httpdate::parse_http_date(s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:nc="http://nextcloud.org/ns" xmlns:oc="http://owncloud.org/ns">
  <d:response>
    <d:href>/remote.php/dav/files/user/Photos/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/></d:resourcetype>
        <d:getlastmodified>Mon, 12 May 2025 10:00:00 GMT</d:getlastmodified>
        <d:getetag>"abc123"</d:getetag>
        <oc:size>5000000</oc:size>
        <oc:permissions>RGDNVCK</oc:permissions>
        <oc:fileid>100</oc:fileid>
        <nc:has-preview>false</nc:has-preview>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/files/user/Photos/sunset.jpg</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype/>
        <d:getcontentlength>1234567</d:getcontentlength>
        <d:getcontenttype>image/jpeg</d:getcontenttype>
        <d:getlastmodified>Sun, 11 May 2025 08:30:00 GMT</d:getlastmodified>
        <d:getetag>"def456"</d:getetag>
        <oc:permissions>RGDNVW</oc:permissions>
        <oc:fileid>101</oc:fileid>
        <oc:owner-id>alice</oc:owner-id>
        <oc:owner-display-name>Alice Smith</oc:owner-display-name>
        <nc:has-preview>true</nc:has-preview>
        <oc:share-types><oc:share-type>0</oc:share-type></oc:share-types>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/files/user/Photos/Vacation%202024/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/></d:resourcetype>
        <d:getlastmodified>Sat, 10 May 2025 12:00:00 GMT</d:getlastmodified>
        <d:getetag>"ghi789"</d:getetag>
        <oc:size>2000000</oc:size>
        <nc:has-preview>false</nc:has-preview>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;

    #[test]
    fn parse_multistatus_basic() {
        let prefix = "/remote.php/dav/files/user";
        let (dir_etag, _self_entry, entries) =
            parse_multistatus_str(SAMPLE, &format!("https://cloud.example.com{}", prefix)).unwrap();

        assert_eq!(dir_etag.as_deref(), Some("abc123"));
        assert_eq!(entries.len(), 2);

        let file = &entries[0];
        assert_eq!(file.path, PathBuf::from("/Photos/sunset.jpg"));
        assert!(!file.is_dir);
        assert_eq!(file.size, 1234567);
        assert_eq!(file.etag.as_deref(), Some("def456"));
        assert_eq!(file.content_type.as_deref(), Some("image/jpeg"));
        assert!(file.has_preview);
        assert!(file.is_shared);
        assert!(file.modified.is_some());
        assert_eq!(file.permissions.as_deref(), Some("RGDNVW"));
        assert_eq!(file.fileid, Some(101));
        assert_eq!(file.owner_id.as_deref(), Some("alice"));
        assert_eq!(file.owner_display_name.as_deref(), Some("Alice Smith"));

        let dir = &entries[1];
        assert_eq!(dir.path, PathBuf::from("/Photos/Vacation 2024"));
        assert!(dir.is_dir);
        assert_eq!(dir.size, 2000000);
        assert!(!dir.has_preview);
        assert!(!dir.is_shared);
    }

    #[test]
    fn empty_getetag_is_no_token_not_an_always_matching_one() {
        // rclone answers a named getetag request on a collection like this.
        const NO_ETAG: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/remote.php/dav/files/user/sub/</d:href>
    <d:propstat>
      <d:prop><d:getetag></d:getetag></d:prop>
      <d:status>HTTP/1.1 404 Not Found</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
        let (dir_etag, _self_entry, _entries) =
            parse_multistatus_str(NO_ETAG, "https://cloud.example.com/remote.php/dav/files/user").unwrap();
        assert_eq!(
            dir_etag, None,
            "an empty ETag must read as absent — Some(\"\") matches every later probe, so the \
             directory would be treated as unchanged forever and never re-listed"
        );
    }

    #[test]
    fn quoted_and_padded_etags_are_normalised() {
        const PADDED: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/remote.php/dav/files/user/sub/</d:href>
    <d:propstat>
      <d:prop><d:getetag>  "abc123"  </d:getetag></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
        let (dir_etag, _s, _e) =
            parse_multistatus_str(PADDED, "https://cloud.example.com/remote.php/dav/files/user").unwrap();
        assert_eq!(dir_etag.as_deref(), Some("abc123"));
    }

    #[test]
    fn href_decoding() {
        let prefix = "/remote.php/dav/files/user";
        let p = href_to_remote_path("/remote.php/dav/files/user/My%20Files/", prefix).unwrap();
        assert_eq!(p, PathBuf::from("/My Files"));
    }

    #[test]
    fn href_root() {
        let prefix = "/remote.php/dav/files/user";
        let p = href_to_remote_path("/remote.php/dav/files/user/", prefix).unwrap();
        assert_eq!(p, PathBuf::from("/"));
    }

    #[test]
    #[allow(clippy::disallowed_methods)] // test scaffolding, not daemon threads
    fn streaming_channel_delivery() {
        let webdav_url = "https://cloud.example.com/remote.php/dav/files/user";
        let prefix = webdav_prefix(webdav_url);
        let (tx, rx) = std::sync::mpsc::channel();

        let xml = SAMPLE.to_string();
        let handle = std::thread::spawn(move || {
            let reader = quick_xml::Reader::from_reader(xml.as_bytes());
            let mut buf_reader = ResponseReader::new(reader);
            let mut dir_etag = None;
            let mut is_first = true;
            while let Some(resp) = buf_reader.next_response().unwrap() {
                let remote_path = href_to_remote_path(&resp.href, &prefix).unwrap();
                let entry = DavEntry {
                    path: remote_path,
                    is_dir: resp.is_collection,
                    size: if resp.is_collection { resp.oc_size } else { resp.content_length },
                    modified: resp.last_modified,
                    etag: resp.etag.clone(),
                    content_type: resp.content_type,
                    has_preview: resp.has_preview,
                    is_shared: resp.is_shared,
                    permissions: resp.permissions,
                    fileid: resp.fileid,
                    owner_id: resp.owner_id,
                    owner_display_name: resp.owner_display_name,
                };
                if is_first {
                    dir_etag = resp.etag;
                    is_first = false;
                } else {
                    tx.send(entry).unwrap();
                }
            }
            dir_etag
        });

        let mut received = Vec::new();
        while let Ok(entry) = rx.recv() {
            received.push(entry);
        }
        let dir_etag = handle.join().unwrap();

        assert_eq!(dir_etag.as_deref(), Some("abc123"));
        assert_eq!(received.len(), 2);
        assert_eq!(received[0].path, PathBuf::from("/Photos/sunset.jpg"));
        assert!(received[0].is_shared);
        assert_eq!(received[1].path, PathBuf::from("/Photos/Vacation 2024"));
    }
}

#[cfg(test)]
mod href_traversal_tests {
    use super::{href_to_remote_path, webdav_prefix};
    use std::path::PathBuf;

    const URL: &str = "https://cloud.example.com/remote.php/dav/files/user";

    #[test]
    fn ordinary_hrefs_still_resolve() {
        let prefix = webdav_prefix(URL);
        assert_eq!(
            href_to_remote_path("/remote.php/dav/files/user/Photos/a.jpg", &prefix),
            Some(PathBuf::from("/Photos/a.jpg")),
        );
        // A name that merely contains dots is not a traversal.
        assert_eq!(
            href_to_remote_path("/remote.php/dav/files/user/..hidden", &prefix),
            Some(PathBuf::from("/..hidden")),
        );
        assert_eq!(
            href_to_remote_path("/remote.php/dav/files/user/a..b/c...d", &prefix),
            Some(PathBuf::from("/a..b/c...d")),
        );
    }

    #[test]
    fn rejects_parent_directory_segments() {
        let prefix = webdav_prefix(URL);
        for hostile in [
            "/remote.php/dav/files/user/../../../../etc/passwd",
            "/remote.php/dav/files/user/a/../../../../.bashrc",
            // Percent-encoded, which is how it would actually arrive.
            "/remote.php/dav/files/user/%2e%2e/%2e%2e/.config/autostart/x.desktop",
            "/remote.php/dav/files/user/..",
            "/../etc/passwd",
        ] {
            assert_eq!(
                href_to_remote_path(hostile, &prefix), None,
                "{} must be rejected", hostile,
            );
        }
    }

    #[test]
    fn rejects_control_characters() {
        let prefix = webdav_prefix(URL);
        for hostile in [
            // A raw newline, as it would actually arrive percent-encoded.
            "/remote.php/dav/files/user/evil.jpg%0APAUSE",
            "/remote.php/dav/files/user/evil%0D%0Aname",
            "/remote.php/dav/files/user/evil\x01name",
        ] {
            assert_eq!(
                href_to_remote_path(hostile, &prefix), None,
                "{} must be rejected", hostile,
            );
        }
    }
}
