//! Runtime greeter artwork includes visible development and storage warnings.
use aftermath_ctl::graphical::Settings;
use std::fs;

/// Keep warnings visible on the login screen even during quiet graphical boot.
pub(super) fn prepare(config: &Settings) -> Result<(), String> {
    let development = if config.development_accounts {
        "Development image: fresh login bob / bob · root / bob"
    } else {
        "Wayland · Aftermath engine"
    };
    if config.development_accounts {
        crate::warn("DEVELOPMENT ACCOUNT BOOTSTRAP ENABLED: fresh images use bob/bob and root/bob; disable desktop.development_accounts before distributing");
        crate::logs::desktop_event("development account bootstrap enabled");
    }
    let storage = fs::read_to_string("/aftermath/image/boot-warning").unwrap_or_default();
    let storage_title = if storage.is_empty() {
        ""
    } else {
        "SAVED CHANGES UNAVAILABLE — temporary session"
    };
    let storage_detail = if storage.is_empty() {
        ""
    } else {
        "Saved disk kept. See amctl status for details."
    };
    let artwork = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="1920" height="1080" viewBox="0 0 1920 1080">
<defs><linearGradient id="shade" x2="1" y2="1"><stop stop-color="#08070d"/><stop offset="1" stop-color="#2b1044"/></linearGradient></defs>
<rect width="1920" height="1080" fill="url(#shade)"/>
<path d="M1300 1080 1730 320 1920 650 1920 1080Z" fill="#6d28a8" opacity=".45"/>
<path d="M1420 1080 1850 320 1920 440 1540 1080Z" fill="#dc354d" opacity=".55"/>
<g font-family="sans-serif"><text x="192" y="115" fill="#f34b68" font-size="52" font-weight="bold">AFTERMATH</text>
<text x="192" y="280" fill="#e4cfef" font-size="25">{}</text>
<text x="192" y="325" fill="#ff9aa9" font-size="28">{}</text>
<text x="192" y="355" fill="#e4cfef" font-size="22">{}</text></g></svg>"##,
        xml(development),
        xml(storage_title),
        xml(storage_detail)
    );
    fs::create_dir_all("/run/aftermath/greeter").map_err(|error| error.to_string())?;
    fs::write("/run/aftermath/greeter/background.svg", artwork).map_err(|error| error.to_string())
}

/// Dynamic filesystem diagnostics are text, never SVG markup.
fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
