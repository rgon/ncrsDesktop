use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
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

fn client(http3: bool) -> reqwest::blocking::Client {
    let mut b = reqwest::blocking::Client::builder().timeout(API_TIMEOUT);
    if http3 {
        b = b.http3_prior_knowledge();
    }
    b.build().expect("reqwest client")
}

fn decode_pct(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().into_owned()
}

fn absolutize(base: &str, url: &str) -> String {
    if url.is_empty() || url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else if url.starts_with('/') {
        format!("{}{}", base, url)
    } else {
        url.to_string()
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
    Ok(ocs.ocs.data)
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
                                    e.resource_url = absolutize(base, &e.resource_url);
                                    e.icon = absolutize(base, &e.icon);
                                    e.thumbnail_url = absolutize(base, &e.thumbnail_url);
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
