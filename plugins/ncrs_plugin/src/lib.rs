use serde::{Deserialize, Serialize};
use tauri::menu::MenuItem;
use tauri::{AppHandle, EventLoopMessage, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_runtime_wry::Wry;

type WryRuntime = Wry<EventLoopMessage>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginMeta {
    pub id: String,
    pub name: String,
    pub description: String,
    pub icon: String,
    pub version: String,
}

pub trait NcrsPlugin: Send + Sync {
    fn meta(&self) -> PluginMeta;

    fn tray_items(&self, app: &AppHandle) -> Result<Vec<MenuItem<WryRuntime>>, tauri::Error> {
        let _ = app;
        Ok(vec![])
    }

    fn handle_tray_event(&self, app: &AppHandle, id: &str) -> bool {
        let _ = (app, id);
        false
    }
}


// ── Frontend load failures ────────────────────────────────────────────────────
//
// The main window is deliberately `transparent(true)` + `decorations(false)` so
// the overlay can paint its own chrome. That is fine while the frontend loads,
// and awful when it does not: WebKitGTK's built-in failure page ("Could not
// connect to localhost") renders as bare text on a fully transparent, borderless
// surface with no title bar, no close button and nothing to click — the window is
// effectively impossible to dismiss.
//
// So a load failure gets its own window instead: decorated, opaque, resizable,
// and explicit about which URL failed and why. The transparent overlay is never
// shown in that state.
//
// The page is served over a private URI scheme rather than a file:// or data:
// URL because `WebviewUrl::External` only accepts http/https, and an
// `App`-relative URL would resolve back to the very dev server that is down.

/// Private URI scheme serving the built-in load-failure page.
pub const ERROR_SCHEME: &str = "ncrsload";

/// Window label for the load-failure window.
const ERROR_WINDOW: &str = "load-error";

/// What failed, in terms a human can act on.
#[derive(Clone, Debug)]
pub struct FrontendLoadError {
    /// The URL the webview was going to load.
    pub target: String,
    /// Why it could not be reached.
    pub cause: String,
    /// What the user can do about it.
    pub hint: String,
}

fn load_error_slot() -> &'static std::sync::Mutex<Option<FrontendLoadError>> {
    static SLOT: std::sync::OnceLock<std::sync::Mutex<Option<FrontendLoadError>>> =
        std::sync::OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(None))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Renders the load-failure page. Self-contained: no scripts, no remote assets,
/// so it renders identically under the app's CSP.
pub fn load_error_page() -> String {
    let err = load_error_slot()
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_else(|| FrontendLoadError {
            target: "unknown".into(),
            cause: "The user interface could not be loaded.".into(),
            hint: "Restart ncRS. If this persists, reinstall the package.".into(),
        });

    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<title>ncRS - interface failed to load</title>
<style>
  :root {{ color-scheme: light dark; }}
  html, body {{ height: 100%; }}
  body {{
    margin: 0; padding: 2.5rem 2.75rem;
    background: #1e1f22; color: #e6e6e6;
    font: 14px/1.55 system-ui, -apple-system, "Segoe UI", Cantarell, sans-serif;
  }}
  @media (prefers-color-scheme: light) {{
    body {{ background: #f6f6f7; color: #1b1b1d; }}
    .box {{ background: #fff !important; border-color: #d8d8dc !important; }}
    .url {{ background: #f0f0f2 !important; }}
  }}
  h1 {{ font-size: 1.2rem; margin: 0 0 .35rem; }}
  p  {{ margin: .55rem 0; }}
  .sub {{ opacity: .7; margin-bottom: 1.4rem; }}
  .box {{
    background: #26282c; border: 1px solid #3a3d42; border-radius: 8px;
    padding: 1rem 1.15rem; margin: 1rem 0;
  }}
  .label {{ text-transform: uppercase; letter-spacing: .06em; font-size: .7rem; opacity: .6; }}
  .url {{
    font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    font-size: .9rem; background: #1a1b1e; border-radius: 5px;
    padding: .45rem .6rem; margin-top: .3rem; word-break: break-all;
  }}
  .hint {{ margin-top: 1.2rem; }}
</style></head>
<body>
  <h1>ncRS could not load its interface</h1>
  <p class="sub">The background sync daemon is unaffected - your files stay mounted.</p>

  <div class="box">
    <div class="label">Tried to load</div>
    <div class="url">{target}</div>
    <p style="margin-bottom:0"><span class="label">Reason</span><br>{cause}</p>
  </div>

  <p class="hint">{hint}</p>
</body></html>"#,
        target = escape(&err.target),
        cause = escape(&err.cause),
        hint = escape(&err.hint),
    )
}

/// The dev server the webview will use, when this build points at one.
///
/// `tauri::is_dev()` is `!cfg!(feature = "custom-protocol")`: without that
/// feature the webview loads `devUrl` rather than the bundled frontend, so a
/// plain `cargo build` (or a release build that forgot `--features
/// custom-protocol`) needs the vite server running.
fn dev_server_url(app: &AppHandle) -> Option<tauri::Url> {
    if !tauri::is_dev() {
        return None;
    }
    app.config().build.dev_url.clone()
}

/// Checks the dev server is actually accepting connections.
fn probe_dev_server(url: &tauri::Url) -> Result<(), String> {
    use std::net::{TcpStream, ToSocketAddrs};
    let host = url.host_str().ok_or_else(|| "no host in devUrl".to_string())?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "no port in devUrl".to_string())?;
    let addrs: Vec<_> = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("{host}:{port} did not resolve ({e})"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("{host}:{port} did not resolve to any address"));
    }
    let timeout = std::time::Duration::from_millis(750);
    let mut last = String::new();
    for addr in &addrs {
        match TcpStream::connect_timeout(addr, timeout) {
            Ok(_) => return Ok(()),
            Err(e) => last = e.to_string(),
        }
    }
    Err(format!("connection to {host}:{port} was refused ({last})"))
}

/// Shows the decorated, opaque failure window (or focuses an existing one).
fn show_load_error_window(app: &AppHandle, err: FrontendLoadError) {
    eprintln!("frontend load failed: {} - {}", err.target, err.cause);
    if let Ok(mut slot) = load_error_slot().lock() {
        *slot = Some(err);
    }

    if let Some(w) = app.get_webview_window(ERROR_WINDOW) {
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }

    let url = match format!("{ERROR_SCHEME}://localhost/").parse::<tauri::Url>() {
        Ok(u) => u,
        Err(e) => {
            eprintln!("could not build load-error URL: {e}");
            return;
        }
    };

    match WebviewWindowBuilder::new(app, ERROR_WINDOW, WebviewUrl::CustomProtocol(url))
        .title("ncRS - interface failed to load")
        .inner_size(720.0, 470.0)
        // The whole point: unlike the overlay, this one can be moved and closed.
        .decorations(true)
        .transparent(false)
        .resizable(true)
        .minimizable(true)
        .closable(true)
        .center()
        .visible(true)
        .build()
    {
        Ok(w) => {
            let _ = w.set_focus();
        }
        Err(e) => eprintln!("failed to build load-error window: {e}"),
    }
}

pub fn open_main_window(app: &AppHandle) {
    // Preflight: if the webview is pointed at a dev server that is not running,
    // showing the transparent overlay would render WebKitGTK's failure text on
    // an unclickable surface. Surface a real window instead.
    if let Some(dev) = dev_server_url(app) {
        if let Err(cause) = probe_dev_server(&dev) {
            show_load_error_window(app, FrontendLoadError {
                target: dev.to_string(),
                cause,
                hint: "This build loads its interface from the vite dev server. \
                       Start it with `pnpm dev` in ncrs-gui/, or build with \
                       `--features custom-protocol` to embed the interface instead."
                    .into(),
            });
            return;
        }
    }

    // The window is destroyed (not hidden) when closed to the tray, so the
    // WebKitGTK webview stops eating idle CPU. Rebuild it from the same config
    // the manifest declares before showing. Keep these attributes in sync with
    // the `main` window entry in tauri.conf.json.
    let w = match app.get_webview_window("main") {
        Some(w) => w,
        None => match WebviewWindowBuilder::new(app, "main", WebviewUrl::default())
            .title("ncrs-gui")
            .inner_size(1860.0, 1000.0)
            .resizable(false)
            .minimizable(false)
            .maximizable(false)
            .closable(true)
            .fullscreen(false)
            .decorations(false)
            .transparent(true)
            .visible(false)
            .build()
        {
            Ok(w) => w,
            Err(e) => {
                eprintln!("failed to rebuild main window: {e}");
                return;
            }
        },
    };
    fit_overlay_to_monitor(&w);
    let _ = w.show();
    let _ = w.set_focus();
}

/// Maximize the overlay to the compositor's work area (excludes taskbar/dock).
pub fn fit_overlay_to_monitor(w: &tauri::WebviewWindow) {
    // maximize() sizes the window to the compositor's work area (excludes taskbar/dock)
    // and handles scale factor automatically. The maximizable(false) builder flag only
    // suppresses the UI gesture; programmatic maximize always works.
    let _ = w.maximize();
}

#[cfg(test)]
mod load_error_tests {
    use super::{escape, load_error_page, load_error_slot, probe_dev_server, FrontendLoadError};

    #[test]
    fn probe_reports_a_refused_port() {
        // Bind then drop, so the port is almost certainly closed.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let url: tauri::Url = format!("http://127.0.0.1:{port}/").parse().unwrap();
        let err = probe_dev_server(&url).expect_err("a closed port must not probe OK");
        assert!(err.contains("refused") || err.contains("resolve"), "got: {err}");
        assert!(err.contains(&port.to_string()), "error should name the port: {err}");
    }

    #[test]
    fn probe_accepts_a_listening_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let url: tauri::Url = format!("http://127.0.0.1:{port}/").parse().unwrap();
        assert!(probe_dev_server(&url).is_ok(), "a listening port must probe OK");
    }

    #[test]
    fn the_page_names_the_url_the_reason_and_the_fix() {
        *load_error_slot().lock().unwrap() = Some(FrontendLoadError {
            target: "http://localhost:1420/".into(),
            cause: "connection refused".into(),
            hint: "start the dev server".into(),
        });
        let html = load_error_page();
        // The whole point of the window: say where the error comes from.
        assert!(html.contains("http://localhost:1420/"), "must name the URL");
        assert!(html.contains("connection refused"), "must give the reason");
        assert!(html.contains("start the dev server"), "must say what to do");
        // Reassures the user their files are still there.
        assert!(html.contains("daemon is unaffected"));
        // Self-contained: no remote assets or scripts, so it renders under the CSP.
        assert!(!html.contains("<script"), "page must not carry script");
        assert!(!html.contains("http://") || !html.contains("src="), "no remote assets");
    }

    #[test]
    fn the_page_still_renders_with_no_error_recorded() {
        *load_error_slot().lock().unwrap() = None;
        let html = load_error_page();
        assert!(html.contains("<html"), "must always produce a page");
        assert!(html.contains("could not load"));
    }

    #[test]
    fn details_are_html_escaped() {
        // The cause embeds OS text; never let it break out into markup.
        assert_eq!(escape("<script>&\"x\""), "&lt;script&gt;&amp;&quot;x&quot;");
        *load_error_slot().lock().unwrap() = Some(FrontendLoadError {
            target: "http://x/<img onerror=alert(1)>".into(),
            cause: "a & b".into(),
            hint: "\"quoted\"".into(),
        });
        let html = load_error_page();
        assert!(!html.contains("<img onerror"), "target must be escaped");
        assert!(html.contains("&lt;img onerror"), "escaped form should be present");
        assert!(html.contains("a &amp; b"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_meta_serializes_roundtrip() {
        let meta = PluginMeta {
            id: "test_plugin".into(),
            name: "Test Plugin".into(),
            description: "A test plugin".into(),
            icon: "mdiPuzzle".into(),
            version: "0.1.0".into(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: PluginMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "test_plugin");
        assert_eq!(back.name, "Test Plugin");
        assert_eq!(back.version, "0.1.0");
    }
}
