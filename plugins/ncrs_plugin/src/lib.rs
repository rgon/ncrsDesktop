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

pub fn open_main_window(app: &AppHandle) {
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

/// Size and position the transparent overlay so it exactly covers its monitor.
///
/// The overlay is a transparent full-screen window; the visible card is
/// CSS-anchored to the right edge of the viewport. So the window must cover
/// the target monitor exactly, or the card falls off-screen.
///
/// On Wayland an unmapped window has no current monitor (the GdkWindow does
/// not exist until realize) and GDK reports no primary monitor at all, so
/// before the first show both lookups fail and we must fall back to the
/// monitor list. The pre-show guess can still be wrong (multi-monitor, or a
/// scale the compositor only reveals on map), so this is also re-run from the
/// ScaleFactorChanged window event once the surface is actually mapped.
pub fn fit_overlay_to_monitor(w: &tauri::WebviewWindow) {
    let monitor = w
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| w.primary_monitor().ok().flatten())
        .or_else(|| {
            w.available_monitors()
                .ok()
                .and_then(|ms| ms.into_iter().next())
        });
    if let Some(m) = monitor {
        // Size in *logical* units derived from the monitor's own scale factor.
        // Passing the monitor's physical size to set_size lets it be re-scaled
        // by the window's (still-default 1.0) scale factor, which overshoots
        // the screen on fractional-scaled displays (e.g. 125%/150%) and crops
        // the right-anchored card off-screen. Position is left to the
        // compositor — the overlay is a full-screen transparent window and the
        // compositor places it at the monitor origin by default.
        let scale = m.scale_factor();
        let _ = w.set_size(m.size().to_logical::<f64>(scale));
    } else {
        let _ = w.set_size(tauri::LogicalSize::new(1860f64, 1000f64));
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
