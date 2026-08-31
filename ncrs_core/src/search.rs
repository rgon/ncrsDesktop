use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use std::time::Duration;

const API_TIMEOUT: Duration = Duration::from_secs(10);
const RESULTS_PER_PROVIDER: u32 = 5;

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchProvider {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub order: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchEntry {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub subline: String,
    #[serde(default, rename = "resourceUrl")]
    pub resource_url: String,
    #[serde(default, rename = "thumbnailUrl")]
    pub thumbnail_url: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub rounded: bool,
    #[serde(default, skip_deserializing)]
    pub local_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResultGroup {
    pub provider_id: String,
    pub provider_name: String,
    pub entries: Vec<SearchEntry>,
}

// ── OCS response wrappers ─────────────────────────────────────────────────────

#[derive(Deserialize)]
struct OcsProvidersBody {
    data: Vec<SearchProvider>,
}
#[derive(Deserialize)]
struct OcsProvidersResponse {
    ocs: OcsProvidersBody,
}

#[derive(Deserialize)]
struct ProviderResults {
    #[serde(default)]
    entries: Vec<SearchEntry>,
}
#[derive(Deserialize)]
struct OcsSearchBody {
    data: ProviderResults,
}
#[derive(Deserialize)]
struct OcsSearchResponse {
    ocs: OcsSearchBody,
}

// ── HTTP helpers ──────────────────────────────────────────────────────────────

/// Cached per HTTP/3 variant: rebuilding spawns a tokio runtime thread, a fresh
/// rustls root store and — with http3 — a new QUIC endpoint, which `search_all`
/// would otherwise pay once per provider on every keystroke-driven search.
fn client(http3: bool) -> &'static reqwest::blocking::Client {
    static H3: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    static H2: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    let cell = if http3 { &H3 } else { &H2 };
    cell.get_or_init(|| {
        let mut b = reqwest::blocking::Client::builder().timeout(API_TIMEOUT);
        if http3 {
            b = b.http3_prior_knowledge();
        }
        b.build().expect("reqwest client")
    })
}

fn decode_pct(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().into_owned()
}

/// Resolves a search-result URL against the server base.
///
/// Values here come from the server's unified-search providers and end up both
/// in `<img src>` in the webview and, for `resourceUrl`, at `open_link`. A value
/// that is neither absolute http(s) nor server-relative is therefore dropped
/// rather than passed through: letting an arbitrary scheme survive would hand a
/// compromised server a URI-handler invocation on the user's desktop.
fn absolutize(base: &str, url: &str) -> String {
    if url.is_empty() {
        String::new()
    } else if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else if url.starts_with('/') {
        format!("{}{}", base, url)
    } else {
        String::new()
    }
}

// ── API functions ─────────────────────────────────────────────────────────────

pub fn fetch_providers(
    base: &str,
    creds: &crate::auth::Credentials,
    http3: bool,
) -> Result<Vec<SearchProvider>, String> {
    let url = format!("{}/ocs/v2.php/search/providers", base);
    let resp = creds.apply(client(http3)
        .get(&url)
        .query(&[("format", "json")]))
        .header("OCS-APIREQUEST", "true")
        .send()
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("providers API returned {}", resp.status()));
    }
    let ocs: OcsProvidersResponse = resp.json().map_err(|e| e.to_string())?;
    // Provider icons are rendered by the GUI too, and were reaching it as raw
    // server strings.
    let providers = ocs.ocs.data.into_iter()
        .map(|mut p| {
            p.icon = crate::asset_url::same_origin_asset(base, &p.icon);
            p
        })
        .collect();
    Ok(providers)
}

pub fn search_provider(
    base: &str,
    creds: &crate::auth::Credentials,
    provider_id: &str,
    term: &str,
    http3: bool,
) -> Result<Vec<SearchEntry>, String> {
    let url = format!(
        "{}/ocs/v2.php/search/providers/{}/search",
        base, provider_id,
    );
    let limit = RESULTS_PER_PROVIDER.to_string();
    let resp = creds.apply(client(http3)
        .get(&url)
        .query(&[("term", term), ("limit", &limit), ("format", "json")]))
        .header("OCS-APIREQUEST", "true")
        .send()
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("search {} returned {}", provider_id, resp.status()));
    }
    let ocs: OcsSearchResponse = resp.json().map_err(|e| e.to_string())?;
    Ok(ocs.ocs.data.entries)
}

/// Search all providers in parallel, returning only groups with results.
pub fn search_all(
    base: &str,
    creds: &crate::auth::Credentials,
    term: &str,
    http3: bool,
) -> Result<Vec<SearchResultGroup>, String> {
    search_filtered(base, creds, term, http3, &[])
}

/// Search selected providers (or all if `provider_ids` is empty).
pub fn search_filtered(
    base: &str,
    creds: &crate::auth::Credentials,
    term: &str,
    http3: bool,
    provider_ids: &[String],
) -> Result<Vec<SearchResultGroup>, String> {
    let mut providers = fetch_providers(base, creds, http3)?;
    providers.sort_by_key(|p| p.order);
    if !provider_ids.is_empty() {
        providers.retain(|p| provider_ids.contains(&p.id));
    }

    let mut results: Vec<SearchResultGroup> = Vec::new();

    std::thread::scope(|s| {
        let handles: Vec<_> = providers
            .iter()
            .map(|p| {
                let pid = p.id.clone();
                let pname = p.name.clone();
                s.spawn(move || {
                    match search_provider(base, creds, &pid, term, http3) {
                        Ok(entries) if !entries.is_empty() => {
                            let entries = entries
                                .into_iter()
                                .map(|mut e| {
                                    e.title = decode_pct(&e.title);
                                    e.subline = decode_pct(&e.subline);
                                    // A link may legitimately point off-site
                                    // (the Bookmarks provider returns the
                                    // bookmarked address); a rendered asset may
                                    // not — see crate::asset_url.
                                    e.resource_url = absolutize(base, &e.resource_url);
                                    e.icon = crate::asset_url::same_origin_asset(base, &e.icon);
                                    e.thumbnail_url =
                                        crate::asset_url::same_origin_asset(base, &e.thumbnail_url);
                                    e
                                })
                                .collect();
                            Some(SearchResultGroup { provider_id: pid, provider_name: pname, entries })
                        }
                        Ok(_) => None,
                        Err(e) => {
                            log::warn!("search provider {}: {}", pid, e);
                            None
                        }
                    }
                })
            })
            .collect();

        for handle in handles {
            if let Ok(Some(group)) = handle.join() {
                results.push(group);
            }
        }
    });

    Ok(results)
}

#[cfg(test)]
mod absolutize_tests {
    use super::absolutize;

    const BASE: &str = "https://cloud.example.com";

    #[test]
    fn resolves_server_relative_and_keeps_absolute_web_urls() {
        assert_eq!(absolutize(BASE, "/apps/files/?dir=/x"), "https://cloud.example.com/apps/files/?dir=/x");
        assert_eq!(absolutize(BASE, "https://cloud.example.com/a"), "https://cloud.example.com/a");
        assert_eq!(absolutize(BASE, "http://cloud.example.com/a"), "http://cloud.example.com/a");
        assert_eq!(absolutize(BASE, ""), "");
    }

    #[test]
    fn drops_values_with_a_non_web_scheme() {
        // These used to be returned verbatim and could reach the desktop URL
        // handler via open_link.
        for hostile in [
            "file:///etc/passwd",
            "file:///home/victim/.config/autostart/x.desktop",
            "smb://attacker.example/share",
            "javascript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "nc://open/x",
        ] {
            assert_eq!(absolutize(BASE, hostile), "", "{} must be dropped", hostile);
        }
    }
}
