use std::path::PathBuf;

use crate::{configuration_parser, MountOptions};

const DEFAULT_CONFIG: &str = r#"# ncRS Desktop configuration
# Generated on first run — fill in your Nextcloud credentials.

# Full WebDAV URL, e.g. https://cloud.example.com/remote.php/dav/files/USERNAME/
url: ""

username: ""
password: ""

# Local directory where the WebDAV tree will be mounted.
mount_point: ""

# Label used in log lines (usually your local username).
user: ""
"#;

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("ncrs")
        .join("config.yaml")
}

/// Load config from the XDG config dir, creating a skeleton file on first run.
pub fn load_config() -> Result<MountOptions, String> {
    let path = config_path();

    if !path.exists() {
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("Cannot create config dir {}: {}", dir.display(), e))?;
        std::fs::write(&path, DEFAULT_CONFIG)
            .map_err(|e| format!("Cannot write default config: {}", e))?;
        return Err(format!(
            "Created default config at {}. Please fill it in and restart.",
            path.display()
        ));
    }

    let yaml = std::fs::read_to_string(&path)
        .map_err(|e| format!("Cannot read {}: {}", path.display(), e))?;

    let opts = configuration_parser(&yaml)?;

    if opts.url.is_empty() {
        return Err(format!(
            "Config at {} is incomplete (url is empty). Please fill it in.",
            path.display()
        ));
    }

    Ok(opts)
}
