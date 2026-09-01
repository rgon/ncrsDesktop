use std::path::Path;
use std::time::Duration;

const WIPE_TIMEOUT: Duration = Duration::from_secs(10);

pub fn check_wipe(
    http: &crate::http_clients::DavClient,
    base_url: &str,
    token: &str,
) -> Result<bool, String> {
    let url = format!("{}/index.php/core/wipe/check", base_url);
    let resp = http
        .post(&url)
        .timeout(WIPE_TIMEOUT)
        .form(&[("token", token)])
        .send()
        .map_err(|e| format!("wipe check request failed: {}", e))?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(false);
    }
    if !resp.status().is_success() {
        return Err(format!("wipe check returned {}", resp.status()));
    }

    #[derive(serde::Deserialize)]
    struct WipeResponse {
        wipe: bool,
    }

    let body: WipeResponse = resp
        .json()
        .map_err(|e| format!("wipe check parse: {}", e))?;
    Ok(body.wipe)
}

pub fn confirm_wipe(
    http: &crate::http_clients::DavClient,
    base_url: &str,
    token: &str,
) -> Result<(), String> {
    let url = format!("{}/index.php/core/wipe/success", base_url);
    let resp = http
        .post(&url)
        .timeout(WIPE_TIMEOUT)
        .form(&[("token", token)])
        .send()
        .map_err(|e| format!("wipe success request failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("wipe success returned {}", resp.status()));
    }
    Ok(())
}

pub fn execute_wipe(cache_dir: &Path, config_path: &Path) -> Result<(), String> {
    if cache_dir.exists() {
        std::fs::remove_dir_all(cache_dir)
            .map_err(|e| format!("failed to delete cache dir {}: {}", cache_dir.display(), e))?;
        log::info!("REMOTE_WIPE: deleted cache dir {}", cache_dir.display());
    }

    if config_path.exists() {
        match std::fs::read_to_string(config_path) {
            Ok(content) => {
                let cleared: String = content
                    .lines()
                    .map(|line| {
                        let trimmed = line.trim_start();
                        if trimmed.starts_with("password:") {
                            "password: \"\""
                        } else if trimmed.starts_with("bearer_token:") {
                            "bearer_token: \"\""
                        } else if trimmed.starts_with("auth_command:") {
                            "auth_command: \"\""
                        } else {
                            line
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                // Shared helper: rewriting the config must not widen its mode.
                crate::config::write_private(config_path, cleared.as_bytes())
                    .map_err(|e| format!("failed to clear config credentials: {}", e))?;
                log::info!(
                    "REMOTE_WIPE: cleared credentials in {}",
                    config_path.display()
                );
            }
            Err(e) => {
                log::warn!("REMOTE_WIPE: could not read config to clear: {}", e);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn execute_wipe_deletes_cache_and_clears_password() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("cache");
        fs::create_dir_all(cache_dir.join("kept")).unwrap();
        fs::create_dir_all(cache_dir.join("cache")).unwrap();
        fs::write(cache_dir.join("dir_cache.json"), "{}").unwrap();
        fs::write(cache_dir.join("file_cache.json"), "{}").unwrap();
        fs::write(cache_dir.join("kept/myfile.txt"), "data").unwrap();

        let config_path = tmp.path().join("config.yaml");
        fs::write(
            &config_path,
            "url: \"https://cloud.example.com\"\nusername: \"user\"\npassword: \"secret-token\"\nbearer_token: \"ey.jwt.token\"\nauth_command: \"secret-tool lookup label authd\"\nmount_point: \"/mnt/nc\"\n",
        )
        .unwrap();

        execute_wipe(&cache_dir, &config_path).unwrap();

        assert!(!cache_dir.exists());
        let config = fs::read_to_string(&config_path).unwrap();
        assert!(config.contains("password: \"\""));
        assert!(config.contains("bearer_token: \"\""));
        assert!(config.contains("auth_command: \"\""));
        assert!(!config.contains("secret-token"));
        assert!(!config.contains("ey.jwt.token"));
        assert!(!config.contains("secret-tool"));
        assert!(config.contains("url: \"https://cloud.example.com\""));
    }
}
