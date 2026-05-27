use std::process::Command;

const KNOWN_APPS: &[&str] = &[
    "mozilla firefox",
    "firefox",
    "google chrome",
    "chrome",
    "chromium",
    "microsoft edge",
    "edge",
    "brave",
    "opera",
    "safari",
    "vivaldi",
    "librewolf",
    "waterfox",
    "zen browser",
    "zen",
    "epiphany",
    "gnome web",
    "konqueror",
    "falkon",
    "midori",
];

pub fn get_focused_window_title() -> Option<String> {
    xdotool_active_title()
        .or_else(gnome_shell_focused_title)
}

fn xdotool_active_title() -> Option<String> {
    let output = Command::new("xdotool")
        .args(["getactivewindow", "getwindowname"])
        .output()
        .ok()?;
    if output.status.success() {
        let title = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if title.is_empty() {
            None
        } else {
            Some(title)
        }
    } else {
        None
    }
}

fn gnome_shell_focused_title() -> Option<String> {
    let output = Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.gnome.Shell",
            "--object-path",
            "/org/gnome/Shell",
            "--method",
            "org.gnome.Shell.Eval",
            "global.display.focus_window ? global.display.focus_window.get_title() : ''",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // Output format: (true, 'Window Title Here')
    let start = text.find("'")? + 1;
    let end = text.rfind("'")?;
    if start >= end {
        return None;
    }
    let title = text[start..end].to_string();
    if title.is_empty() { None } else { Some(title) }
}

pub fn extract_search_term(title: &str) -> Option<String> {
    let separators = [" — ", " – ", " -- ", " - "];

    for sep in separators {
        if let Some(pos) = title.rfind(sep) {
            let suffix = title[pos + sep.len()..].trim().to_lowercase();
            if KNOWN_APPS.iter().any(|app| suffix == *app || suffix.starts_with(app)) {
                let page = title[..pos].trim();
                if !page.is_empty() {
                    return Some(page.to_string());
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_from_firefox_title() {
        assert_eq!(
            extract_search_term("LOGO PM -- Mozilla Firefox"),
            Some("LOGO PM".into())
        );
    }

    #[test]
    fn extracts_from_chrome_title() {
        assert_eq!(
            extract_search_term("GitHub - My Repo - Google Chrome"),
            Some("GitHub - My Repo".into())
        );
    }

    #[test]
    fn extracts_with_em_dash() {
        assert_eq!(
            extract_search_term("Login Page — Firefox"),
            Some("Login Page".into())
        );
    }

    #[test]
    fn extracts_with_en_dash() {
        assert_eq!(
            extract_search_term("My App – Chromium"),
            Some("My App".into())
        );
    }

    #[test]
    fn returns_none_for_non_browser() {
        assert_eq!(extract_search_term("Terminal"), None);
    }

    #[test]
    fn returns_none_for_unknown_app() {
        assert_eq!(extract_search_term("Document - LibreOffice Writer"), None);
    }

    #[test]
    fn returns_none_for_empty_page_title() {
        assert_eq!(extract_search_term(" - Mozilla Firefox"), None);
    }

    #[test]
    fn handles_brave_browser() {
        assert_eq!(
            extract_search_term("Netflix - Brave"),
            Some("Netflix".into())
        );
    }

    #[test]
    fn handles_zen_browser() {
        assert_eq!(
            extract_search_term("Mail - Zen Browser"),
            Some("Mail".into())
        );
    }
}
