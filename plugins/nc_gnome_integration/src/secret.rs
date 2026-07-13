use anyhow::Context;
use secret_service::{EncryptionType, SecretService};
use std::collections::HashMap;

/// Escape `password` and wrap it in the GVariant text format expected by
/// GOA's owncloud provider: `{'password': <'ESCAPED_PASS'>}`.
pub fn gvariant_password_string(password: &str) -> String {
    // Backslashes must be escaped before single quotes to avoid double-escaping.
    let escaped = password.replace('\\', "\\\\").replace('\'', "\\'");
    format!("{{'password': <'{}'>}}", escaped)
}

/// Returns the GOA identity string for a given account id.
/// Format: `owncloud:gen0:{account_id}`
pub fn goa_identity(account_id: &str) -> String {
    format!("owncloud:gen0:{}", account_id)
}

pub async fn store_credentials(account_id: &str, password: &str) -> anyhow::Result<()> {
    let ss = SecretService::connect(EncryptionType::Dh)
        .await
        .context("connect to Secret Service")?;
    let collection = ss
        .get_default_collection()
        .await
        .context("get default collection")?;
    if collection.is_locked().await.unwrap_or(false) {
        collection.unlock().await.context("unlock collection")?;
    }
    let identity = goa_identity(account_id);
    let secret = gvariant_password_string(password);
    let mut attrs = HashMap::new();
    attrs.insert("goa-identity", identity.as_str());

    // Delete stale entries first
    if let Ok(items) = collection.search_items(attrs.clone()).await {
        for item in items {
            let _ = item.delete().await;
        }
    }

    collection
        .create_item(
            &format!("GOA owncloud credentials for {}", account_id),
            attrs,
            secret.as_bytes(),
            false,
            "text/plain",
        )
        .await
        .context("create Secret Service item")?;
    Ok(())
}

pub async fn delete_credentials(account_id: &str) -> anyhow::Result<()> {
    let ss = SecretService::connect(EncryptionType::Dh)
        .await
        .context("connect to Secret Service")?;
    let collection = ss
        .get_default_collection()
        .await
        .context("get default collection")?;
    if collection.is_locked().await.unwrap_or(false) {
        collection.unlock().await.context("unlock collection")?;
    }
    let identity = goa_identity(account_id);
    let mut attrs = HashMap::new();
    attrs.insert("goa-identity", identity.as_str());
    if let Ok(items) = collection.search_items(attrs).await {
        for item in items {
            let _ = item.delete().await;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests — only pure string functions; no keyring daemon required
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gvariant_simple_password() {
        assert_eq!(
            gvariant_password_string("mypassword"),
            "{'password': <'mypassword'>}"
        );
    }

    #[test]
    fn gvariant_escapes_single_quote() {
        assert_eq!(
            gvariant_password_string("pass'word"),
            "{'password': <'pass\\'word'>}"
        );
    }

    #[test]
    fn gvariant_escapes_backslash() {
        assert_eq!(
            gvariant_password_string("pass\\word"),
            "{'password': <'pass\\\\word'>}"
        );
    }

    #[test]
    fn gvariant_empty_password() {
        assert_eq!(
            gvariant_password_string(""),
            "{'password': <''>}"
        );
    }

    #[test]
    fn gvariant_both_escapes() {
        // Input: it\'s  (backslash then single-quote)
        // After escaping backslash: it\\'s
        // After escaping single quote: it\\\'s
        // Wrapped:  {'password': <'it\\\'s'>}
        assert_eq!(
            gvariant_password_string("it\\'s"),
            "{'password': <'it\\\\\\'s'>}"
        );
    }

    #[test]
    fn goa_identity_format() {
        assert_eq!(
            goa_identity("account_12345_0"),
            "owncloud:gen0:account_12345_0"
        );
    }
}
