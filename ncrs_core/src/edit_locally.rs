use std::time::Duration;

const API_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, serde::Deserialize)]
struct OcsResponse {
    ocs: OcsEnvelope,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct OcsEnvelope {
    data: OpenLocalEditorData,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct OpenLocalEditorData {
    #[serde(rename = "userId")]
    pub user_id: String,
    #[serde(rename = "pathForUser")]
    pub path_for_user: String,
}

pub fn resolve_token(
    base_url: &str,
    creds: &crate::auth::Credentials,
    token: &str,
) -> Result<OpenLocalEditorData, String> {
    let url = format!(
        "{}/ocs/v2.php/apps/files/api/v1/openlocaleditor?format=json",
        base_url
    );
    let client = reqwest::blocking::Client::new();
    let resp = creds.apply(client
        .post(&url)
        .timeout(API_TIMEOUT))
        .header("OCS-APIREQUEST", "true")
        .json(&serde_json::json!({ "token": token }))
        .send()
        .map_err(|e| format!("openlocaleditor request failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("openlocaleditor returned {}", resp.status()));
    }

    let body: OcsResponse = resp
        .json()
        .map_err(|e| format!("openlocaleditor parse: {}", e))?;
    Ok(body.ocs.data)
}

pub struct NcUri {
    pub user: String,
    pub server: String,
    pub token: String,
}

pub fn parse_nc_uri(uri: &str) -> Result<NcUri, String> {
    let rest = uri
        .strip_prefix("nc://open/")
        .ok_or_else(|| format!("not a nc://open/ URI: {}", uri))?;

    let (user_at_server, token) = rest
        .rsplit_once('/')
        .ok_or_else(|| "missing token in nc:// URI".to_string())?;

    let (user, server) = user_at_server
        .split_once('@')
        .ok_or_else(|| "missing @ in nc:// URI".to_string())?;

    Ok(NcUri {
        user: user.to_string(),
        server: server.to_string(),
        token: token.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_nc_uri() {
        let uri = "nc://open/admin@cloud.example.com/abc123token";
        let parsed = parse_nc_uri(uri).unwrap();
        assert_eq!(parsed.user, "admin");
        assert_eq!(parsed.server, "cloud.example.com");
        assert_eq!(parsed.token, "abc123token");
    }

    #[test]
    fn parse_nc_uri_with_port() {
        let uri = "nc://open/user@cloud.example.com:8443/mytoken";
        let parsed = parse_nc_uri(uri).unwrap();
        assert_eq!(parsed.user, "user");
        assert_eq!(parsed.server, "cloud.example.com:8443");
        assert_eq!(parsed.token, "mytoken");
    }

    #[test]
    fn parse_nc_uri_rejects_bad_scheme() {
        assert!(parse_nc_uri("https://example.com").is_err());
    }

    #[test]
    fn parse_nc_uri_rejects_missing_token() {
        assert!(parse_nc_uri("nc://open/user@server").is_err());
    }

    #[test]
    fn parse_nc_uri_rejects_missing_at() {
        assert!(parse_nc_uri("nc://open/userserver/token").is_err());
    }
}
