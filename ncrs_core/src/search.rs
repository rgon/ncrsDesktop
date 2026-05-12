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

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(API_TIMEOUT)
        .build()
        .expect("reqwest client")
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
    username: &str,
    password: &str,
) -> Result<Vec<SearchProvider>, String> {
    let url = format!("{}/ocs/v2.php/search/providers", base);
    let resp = client()
        .get(&url)
        .query(&[("format", "json")])
        .basic_auth(username, Some(password))
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
    username: &str,
    password: &str,
    provider_id: &str,
    term: &str,
) -> Result<Vec<SearchEntry>, String> {
    let url = format!(
        "{}/ocs/v2.php/search/providers/{}/search",
        base, provider_id,
    );
    let limit = RESULTS_PER_PROVIDER.to_string();
    let resp = client()
        .get(&url)
        .query(&[("term", term), ("limit", &limit), ("format", "json")])
        .basic_auth(username, Some(password))
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
    username: &str,
    password: &str,
    term: &str,
) -> Result<Vec<SearchResultGroup>, String> {
    let mut providers = fetch_providers(base, username, password)?;
    providers.sort_by_key(|p| p.order);

    let mut results: Vec<SearchResultGroup> = Vec::new();

    std::thread::scope(|s| {
        let handles: Vec<_> = providers
            .iter()
            .map(|p| {
                let pid = p.id.clone();
                let pname = p.name.clone();
                s.spawn(move || {
                    match search_provider(base, username, password, &pid, term) {
                        Ok(entries) if !entries.is_empty() => {
                            let entries = entries
                                .into_iter()
                                .map(|mut e| {
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
