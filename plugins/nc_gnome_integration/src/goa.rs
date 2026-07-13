use std::path::{Path, PathBuf};

use anyhow::Context;

pub const NCRS_MANAGED_KEY: &str = "NcrsManaged";
pub const NCRS_ACCOUNT_ID: &str = "account_ncrs_calendar_0";

pub struct AccountEntry {
    pub id: String,
    pub is_ncrs_managed: bool,
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

struct AccountsConf {
    sections: Vec<AccountSection>,
}

struct AccountSection {
    name: String,
    entries: Vec<(String, String)>,
}

impl AccountSection {
    fn get(&self, key: &str) -> Option<&str> {
        let key_lower = key.to_lowercase();
        self.entries
            .iter()
            .find(|(k, _)| k.to_lowercase() == key_lower)
            .map(|(_, v)| v.as_str())
    }
}

impl AccountsConf {
    fn parse(text: &str) -> Self {
        let mut sections: Vec<AccountSection> = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                let name = line[1..line.len() - 1].to_string();
                sections.push(AccountSection {
                    name,
                    entries: Vec::new(),
                });
            } else if let Some(pos) = line.find('=') {
                let key = line[..pos].to_string();
                let value = line[pos + 1..].to_string();
                if let Some(section) = sections.last_mut() {
                    section.entries.push((key, value));
                }
            }
        }
        AccountsConf { sections }
    }

    #[allow(clippy::inherent_to_string)]
    fn to_string(&self) -> String {
        let mut out = String::new();
        for section in &self.sections {
            out.push('[');
            out.push_str(&section.name);
            out.push_str("]\n");
            for (k, v) in &section.entries {
                out.push_str(k);
                out.push('=');
                out.push_str(v);
                out.push('\n');
            }
            out.push('\n');
        }
        out
    }

}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

fn host_from_url(url: &str) -> &str {
    let s = if let Some(pos) = url.find("://") {
        &url[pos + 3..]
    } else {
        url
    };
    match s.find('/') {
        Some(pos) => &s[..pos],
        None => s,
    }
}

fn read_conf(path: &Path) -> AccountsConf {
    match std::fs::read_to_string(path) {
        Ok(text) => AccountsConf::parse(&text),
        Err(_) => AccountsConf {
            sections: Vec::new(),
        },
    }
}

fn write_conf(path: &Path, conf: &AccountsConf) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, conf.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Public(crate) API — accept explicit path for testability
// ---------------------------------------------------------------------------

pub(crate) fn ensure_account_at(
    path: &Path,
    base_url: &str,
    username: &str,
) -> anyhow::Result<AccountEntry> {
    let mut conf = read_conf(path);

    // Only manage our own account — never reuse a manually-added one.
    let already_exists = conf
        .sections
        .iter()
        .any(|s| s.name == format!("Account {}", NCRS_ACCOUNT_ID));
    if already_exists {
        return Ok(AccountEntry {
            id: NCRS_ACCOUNT_ID.to_string(),
            is_ncrs_managed: true,
        });
    }

    let base_url_trimmed = base_url.trim_end_matches('/');
    let host = host_from_url(base_url_trimmed).to_string();

    let new_section = AccountSection {
        name: format!("Account {}", NCRS_ACCOUNT_ID),
        entries: vec![
            ("Provider".to_string(), "owncloud".to_string()),
            ("Identity".to_string(), username.to_string()),
            (
                "PresentationIdentity".to_string(),
                format!("{}@{}", username, host),
            ),
            (
                "Uri".to_string(),
                format!("{}/remote.php/webdav", base_url_trimmed),
            ),
            ("CalendarEnabled".to_string(), "true".to_string()),
            (
                "CalDavUri".to_string(),
                format!("{}/remote.php/dav", base_url_trimmed),
            ),
            ("ContactsEnabled".to_string(), "true".to_string()),
            (
                "CardDavUri".to_string(),
                format!("{}/remote.php/dav", base_url_trimmed),
            ),
            ("FilesEnabled".to_string(), "false".to_string()),
            ("AcceptSslErrors".to_string(), "false".to_string()),
            ("NcrsManaged".to_string(), "true".to_string()),
        ],
    };

    conf.sections.push(new_section);
    write_conf(path, &conf).context("write accounts.conf")?;

    Ok(AccountEntry {
        id: NCRS_ACCOUNT_ID.to_string(),
        is_ncrs_managed: true,
    })
}

pub(crate) fn remove_managed_account_at(
    path: &Path,
    base_url: &str,
) -> anyhow::Result<Option<String>> {
    let mut conf = read_conf(path);

    let pos = conf.sections.iter().position(|s| {
        s.name.starts_with("Account ")
            && s.get(NCRS_MANAGED_KEY) == Some("true")
            && (s.get("Uri").map_or(false, |v| v.starts_with(base_url))
                || s.get("CalDavUri").map_or(false, |v| v.starts_with(base_url)))
    });

    if let Some(idx) = pos {
        let account_id = conf.sections[idx]
            .name
            .strip_prefix("Account ")
            .unwrap_or(&conf.sections[idx].name)
            .to_string();
        conf.sections.remove(idx);
        write_conf(path, &conf).context("write accounts.conf")?;
        Ok(Some(account_id))
    } else {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Public API — use the real config dir
// ---------------------------------------------------------------------------

pub fn ensure_account(base_url: &str, username: &str) -> anyhow::Result<AccountEntry> {
    let path = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("goa-1.0")
        .join("accounts.conf");
    ensure_account_at(&path, base_url, username)
}

pub fn remove_managed_account(base_url: &str) -> anyhow::Result<Option<String>> {
    let path = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("goa-1.0")
        .join("accounts.conf");
    remove_managed_account_at(&path, base_url)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // --- parsing ---

    #[test]
    fn parse_empty_string() {
        let conf = AccountsConf::parse("");
        assert!(conf.sections.is_empty());
    }

    #[test]
    fn parse_single_section() {
        let text = "[Account foo]\nProvider=owncloud\nIdentity=alice\n";
        let conf = AccountsConf::parse(text);
        assert_eq!(conf.sections.len(), 1);
        assert_eq!(conf.sections[0].name, "Account foo");
        assert_eq!(conf.sections[0].get("Provider"), Some("owncloud"));
        assert_eq!(conf.sections[0].get("Identity"), Some("alice"));
    }

    #[test]
    fn parse_multiple_sections() {
        let text = "[Section A]\nKey=value1\n\n[Section B]\nKey=value2\n";
        let conf = AccountsConf::parse(text);
        assert_eq!(conf.sections.len(), 2);
        assert_eq!(conf.sections[0].name, "Section A");
        assert_eq!(conf.sections[0].get("Key"), Some("value1"));
        assert_eq!(conf.sections[1].name, "Section B");
        assert_eq!(conf.sections[1].get("Key"), Some("value2"));
    }

    #[test]
    fn parse_value_with_equals() {
        let text = "[Section]\nKey=val=ue\n";
        let conf = AccountsConf::parse(text);
        assert_eq!(conf.sections[0].get("Key"), Some("val=ue"));
    }

    // --- ensure_account_at ---

    #[test]
    fn ensure_account_creates_new_entry() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("accounts.conf");

        let entry = ensure_account_at(&path, "https://cloud.example.com", "alice").unwrap();

        assert!(path.exists());
        assert_eq!(entry.id, NCRS_ACCOUNT_ID);
        assert!(entry.is_ncrs_managed);

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains(&format!("[Account {}]", NCRS_ACCOUNT_ID)));
        assert!(contents.contains("Provider=owncloud"));
        assert!(contents.contains("NcrsManaged=true"));
    }

    #[test]
    fn ensure_account_returns_our_own_account_without_write() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("accounts.conf");

        // Pre-existing ncrs-managed account — should be returned as-is.
        let content = format!(
            "[Account {}]\nProvider=owncloud\nUri=https://cloud.example.com/remote.php/webdav\nNcrsManaged=true\n\n",
            NCRS_ACCOUNT_ID
        );
        std::fs::write(&path, &content).unwrap();

        let mtime_before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let entry = ensure_account_at(&path, "https://cloud.example.com", "alice").unwrap();
        let mtime_after = std::fs::metadata(&path).unwrap().modified().unwrap();

        assert_eq!(mtime_before, mtime_after, "should not rewrite when our account already exists");
        assert_eq!(entry.id, NCRS_ACCOUNT_ID);
        assert!(entry.is_ncrs_managed);
    }

    #[test]
    fn ensure_account_ignores_manual_account_with_same_server() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("accounts.conf");

        // Manual account for the same server — must NOT be reused.
        let content = "[Account account_manual_0]\nProvider=owncloud\nUri=https://cloud.example.com/remote.php/webdav\n\n";
        std::fs::write(&path, content).unwrap();

        let entry = ensure_account_at(&path, "https://cloud.example.com", "alice").unwrap();

        // Must have created our own managed account, not returned the manual one.
        assert_eq!(entry.id, NCRS_ACCOUNT_ID);
        assert!(entry.is_ncrs_managed);

        // Both accounts should now coexist in the file.
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("[Account account_manual_0]"), "manual account must be preserved");
        assert!(contents.contains(&format!("[Account {}]", NCRS_ACCOUNT_ID)), "our account must be created");
    }

    #[test]
    fn ensure_account_idempotent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("accounts.conf");

        let entry1 = ensure_account_at(&path, "https://cloud.example.com", "alice").unwrap();
        let entry2 = ensure_account_at(&path, "https://cloud.example.com", "alice").unwrap();

        assert_eq!(entry1.id, entry2.id);
    }

    // --- remove_managed_account_at ---

    #[test]
    fn remove_managed_account_removes_only_ncrs() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("accounts.conf");

        let content = "\
[Account manual_0]\n\
Provider=owncloud\n\
Uri=https://cloud.example.com/remote.php/webdav\n\
\n\
[Account account_ncrs_calendar_0]\n\
Provider=owncloud\n\
Uri=https://cloud.example.com/remote.php/webdav\n\
NcrsManaged=true\n\
\n";
        std::fs::write(&path, content).unwrap();

        let removed = remove_managed_account_at(&path, "https://cloud.example.com").unwrap();
        assert_eq!(removed, Some("account_ncrs_calendar_0".to_string()));

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("[Account manual_0]"));
        assert!(!contents.contains("[Account account_ncrs_calendar_0]"));
    }

    #[test]
    fn remove_managed_account_skips_manual_accounts() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("accounts.conf");

        let content = "[Account manual_0]\nProvider=owncloud\nUri=https://cloud.example.com/remote.php/webdav\n\n";
        std::fs::write(&path, content).unwrap();

        let removed = remove_managed_account_at(&path, "https://cloud.example.com").unwrap();
        assert_eq!(removed, None);
    }

    #[test]
    fn remove_managed_account_empty_conf() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("accounts.conf");

        let removed = remove_managed_account_at(&path, "https://cloud.example.com").unwrap();
        assert_eq!(removed, None);
    }

    // --- host_from_url ---

    #[test]
    fn host_from_url_extracts_hostname() {
        assert_eq!(host_from_url("https://cloud.example.com/path"), "cloud.example.com");
        assert_eq!(host_from_url("http://localhost:8080/dav"), "localhost:8080");
        assert_eq!(host_from_url("https://cloud.example.com"), "cloud.example.com");
    }

    // --- to_string roundtrip ---

    #[test]
    fn to_string_roundtrips() {
        let text = "[Account abc_0]\nProvider=owncloud\nIdentity=user\n\n";
        let conf = AccountsConf::parse(text);
        let serialized = conf.to_string();
        let conf2 = AccountsConf::parse(&serialized);
        assert_eq!(conf2.sections.len(), conf.sections.len());
        assert_eq!(conf2.sections[0].name, "Account abc_0");
        assert_eq!(conf2.sections[0].get("Provider"), Some("owncloud"));
        assert_eq!(conf2.sections[0].get("Identity"), Some("user"));
    }
}
