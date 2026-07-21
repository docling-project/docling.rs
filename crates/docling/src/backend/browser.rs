//! Optional headless-browser HTML pre-render (Cargo feature `web-browser`).
//!
//! The one thing a pure-Rust HTML parse cannot do is resolve the CSS cascade:
//! whether a `<nav>`/menu is actually painted depends on stylesheet- and
//! class-driven `display`, which needs a layout engine. This module drives the
//! system Chromium over the DevTools protocol — purely from Rust via
//! [`headless_chrome`], no Node/Playwright — loads the page, lets it apply CSS
//! (and any load-time scripts), removes every element the browser *computes* as
//! `display:none`, and returns the cleaned HTML. That HTML then flows through the
//! normal [`super::html`] backend, so all structure/table/KVP/formatting logic
//! stays in Rust; the browser only decides visibility.
//!
//! Kept deliberately small: it removes computed-`display:none` subtrees (the
//! cascade-driven case, e.g. Wikipedia's collapsed sidebar) and leaves
//! everything else — inline `visibility:hidden`/`hidden`, images, tables — to the
//! Rust backend, which already handles them.

use std::path::{Path, PathBuf};

use headless_chrome::{Browser, LaunchOptions};

/// Remove every element the browser renders as `display:none` (the computed
/// value, so class/stylesheet-driven collapses count, not just inline styles),
/// then return the document's serialized HTML. Runs in the page context.
const STRIP_HIDDEN_JS: &str = r#"
(() => {
  const drop = [];
  for (const el of document.querySelectorAll('html *')) {
    if (!el.isConnected) continue;
    if (getComputedStyle(el).display === 'none') drop.push(el);
  }
  for (const el of drop) if (el.isConnected) el.remove();
  return '<!DOCTYPE html>\n' + document.documentElement.outerHTML;
})()
"#;

/// Render `html` in headless Chromium and return the HTML with computed-hidden
/// elements stripped. Errors are surfaced (never silently falling back to the
/// unrendered HTML), so a caller opting into `--use-web-browser` knows if the
/// browser was unavailable.
pub fn render_visible_html(html: &str) -> Result<String, String> {
    // Load via a temporary `file://` document rather than a `data:` URL — a large
    // page (e.g. a full Wikipedia article) blows past practical data-URL limits.
    //
    // The file is created with an unpredictable name via `tempfile` (O_EXCL +
    // random suffix): a fixed `<pid>-<seq>` path in a shared /tmp could be
    // pre-planted as a symlink by a local attacker, and `fs::write` would
    // follow it into an arbitrary-file overwrite. The handle also removes the
    // file on drop, so an early return can't leak it.
    use std::io::Write;
    let mut file = tempfile::Builder::new()
        .prefix("docling.rs-render-")
        .suffix(".html")
        .tempfile()
        .map_err(|e| format!("browser: temp page: {e}"))?;
    file.write_all(html.as_bytes())
        .map_err(|e| format!("browser: writing temp page: {e}"))?;
    file.flush()
        .map_err(|e| format!("browser: writing temp page: {e}"))?;
    render_file(file.path())
}

fn render_file(path: &Path) -> Result<String, String> {
    let options = LaunchOptions::default_builder()
        .headless(true)
        // Containers rarely allow the Chromium sandbox; the input is already
        // untrusted document HTML we parse ourselves, so this matches how the
        // rest of the pipeline treats it.
        .sandbox(false)
        .path(locate_chrome())
        .build()
        .map_err(|e| format!("browser: launch options: {e}"))?;
    let browser = Browser::new(options)
        .map_err(|e| format!("browser: launch failed ({e}); is Chromium installed?"))?;
    let tab = browser
        .new_tab()
        .map_err(|e| format!("browser: new tab: {e}"))?;
    let url = format!("file://{}", path.display());
    tab.navigate_to(&url)
        .map_err(|e| format!("browser: navigate: {e}"))?;
    tab.wait_until_navigated()
        .map_err(|e| format!("browser: page load: {e}"))?;
    let result = tab
        .evaluate(STRIP_HIDDEN_JS, true)
        .map_err(|e| format!("browser: evaluate: {e}"))?;
    match result.value {
        Some(serde_json::Value::String(s)) => Ok(s),
        _ => Err("browser: page returned no HTML".into()),
    }
}

/// Locate the Chromium binary: an explicit override, then the Playwright layout
/// this environment ships (`$PLAYWRIGHT_BROWSERS_PATH/chromium`), else let
/// `headless_chrome` autodetect a system install.
fn locate_chrome() -> Option<PathBuf> {
    for var in ["DOCLING_RS_CHROME", "CHROME"] {
        if let Ok(p) = std::env::var(var) {
            if !p.is_empty() {
                return Some(PathBuf::from(p));
            }
        }
    }
    if let Ok(dir) = std::env::var("PLAYWRIGHT_BROWSERS_PATH") {
        let p = PathBuf::from(dir).join("chromium");
        if p.exists() {
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cascade case a pure-Rust parse can't see: a class hidden by an
    /// embedded stylesheet (not an inline style) is removed after render, while
    /// visible siblings stay. Skips if no Chromium is available (the render
    /// harness only runs where a browser is installed).
    #[test]
    fn strips_stylesheet_hidden_elements() {
        let html = "<html><head><style>.gone{display:none}</style></head><body>\
            <div>SHOWN</div><div class=\"gone\">HIDDEN</div></body></html>";
        let rendered = match render_visible_html(html) {
            Ok(r) => r,
            Err(_) => return, // no browser in this environment — nothing to assert
        };
        assert!(rendered.contains("SHOWN"));
        assert!(
            !rendered.contains("HIDDEN"),
            "stylesheet-hidden element should be stripped: {rendered}"
        );
    }
}
