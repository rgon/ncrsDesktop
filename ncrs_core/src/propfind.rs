use std::path::PathBuf;
use std::time::{Duration, SystemTime};

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

#[derive(Debug, Clone)]
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
    client: &reqwest::blocking::Client,
    webdav_url: &str,
    username: &str,
    password: &str,
    path: &std::path::Path,
    timeout: Duration,
) -> Result<(Option<String>, Option<DavEntry>, Vec<DavEntry>), String> {
    let url = build_url(webdav_url, path);
    log::debug!("PROPFIND {}", url);

    let resp = client
        .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), &url)
        .timeout(timeout)
        .header("Depth", "1")
        .header("Content-Type", "application/xml")
        .basic_auth(username, Some(password))
        .body(PROPFIND_BODY)
        .send()
        .map_err(|e| format!("PROPFIND {}: {}", path.display(), e))?;

    let status = resp.status();
    if status != reqwest::StatusCode::MULTI_STATUS && !status.is_success() {
        return Err(format!("PROPFIND {} returned {}", path.display(), status));
    }

    let reader = std::io::BufReader::new(resp);
    parse_multistatus_stream(reader, webdav_url)
}

pub fn propfind_etag(
    client: &reqwest::blocking::Client,
    webdav_url: &str,
    username: &str,
    password: &str,
    path: &std::path::Path,
    timeout: Duration,
) -> Result<Option<String>, String> {
    let url = build_url(webdav_url, path);
    log::debug!("PROPFIND_ETAG {}", url);

    let resp = client
        .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), &url)
        .timeout(timeout)
        .header("Depth", "0")
        .header("Content-Type", "application/xml")
        .basic_auth(username, Some(password))
        .body(PROPFIND_ETAG_BODY)
        .send()
        .map_err(|e| format!("PROPFIND_ETAG {}: {}", path.display(), e))?;

    let status = resp.status();
    if status != reqwest::StatusCode::MULTI_STATUS && !status.is_success() {
        return Err(format!("PROPFIND_ETAG {} returned {}", path.display(), status));
    }

    let reader = std::io::BufReader::new(resp);
    let (dir_etag, _, _) = parse_multistatus_stream(reader, webdav_url)?;
    Ok(dir_etag)
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
        let remote_path = href_to_remote_path(&resp.href, &prefix);

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

pub fn propfind_list_streaming(
    client: &reqwest::blocking::Client,
    webdav_url: &str,
    username: &str,
    password: &str,
    path: &std::path::Path,
    timeout: Duration,
    tx: std::sync::mpsc::Sender<DavEntry>,
    self_tx: std::sync::mpsc::Sender<DavEntry>,
) -> Result<Option<String>, String> {
    let url = build_url(webdav_url, path);
    log::debug!("PROPFIND_STREAM {}", url);

    let resp = client
        .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), &url)
        .timeout(timeout)
        .header("Depth", "1")
        .header("Content-Type", "application/xml")
        .basic_auth(username, Some(password))
        .body(PROPFIND_BODY)
        .send()
        .map_err(|e| format!("PROPFIND {}: {}", path.display(), e))?;

    let status = resp.status();
    if status != reqwest::StatusCode::MULTI_STATUS && !status.is_success() {
        return Err(format!("PROPFIND {} returned {}", path.display(), status));
    }

    let reader = std::io::BufReader::new(resp);
    let prefix = webdav_prefix(webdav_url);
    let xml_reader = quick_xml::Reader::from_reader(reader);
    let mut buf_reader = ResponseReader::new(xml_reader);

    let mut dir_etag: Option<String> = None;
    let mut is_first = true;
    while let Some(resp) = buf_reader.next_response()? {
        let remote_path = href_to_remote_path(&resp.href, &prefix);
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
            let _ = self_tx.send(entry);
            is_first = false;
        } else if tx.send(entry).is_err() {
            break;
        }
    }
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

fn href_to_remote_path(href: &str, prefix: &str) -> PathBuf {
    let decoded = percent_encoding::percent_decode_str(href)
        .decode_utf8_lossy()
        .into_owned();
    let trimmed = decoded.trim_end_matches('/');
    let remote = if !prefix.is_empty() && trimmed.starts_with(prefix) {
        &trimmed[prefix.len()..]
    } else {
        trimmed
    };
    if remote.is_empty() {
        PathBuf::from("/")
    } else if remote.starts_with('/') {
        PathBuf::from(remote)
    } else {
        PathBuf::from(format!("/{}", remote))
    }
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
                            resp.etag = Some(raw.trim_matches('"').to_string());
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
                            resp.fileid = self.read_text()?.parse().ok();
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
    fn href_decoding() {
        let prefix = "/remote.php/dav/files/user";
        let p = href_to_remote_path("/remote.php/dav/files/user/My%20Files/", prefix);
        assert_eq!(p, PathBuf::from("/My Files"));
    }

    #[test]
    fn href_root() {
        let prefix = "/remote.php/dav/files/user";
        let p = href_to_remote_path("/remote.php/dav/files/user/", prefix);
        assert_eq!(p, PathBuf::from("/"));
    }

    #[test]
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
                let remote_path = href_to_remote_path(&resp.href, &prefix);
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
