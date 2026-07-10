use std::process::Command;

use gpui::{App, UpdateGlobal};
use serde_json::json;

use crate::config::TerminalAppearance;

fn find_first_installed_font(preferred: &[&str]) -> Option<String> {
    if cfg!(target_os = "linux")
        && let Ok(output) = Command::new("fc-list")
            .arg("-f")
            .arg("%{family}\n")
            .output()
        && output.status.success()
    {
        let out = String::from_utf8_lossy(&output.stdout).to_lowercase();
        for &name in preferred {
            if out.contains(&name.to_lowercase()) {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Backwards-compatible default appearance setup used by focused tests.
pub fn configure_terminal_fonts(cx: &mut App) {
    configure_terminal_fonts_with_config(&TerminalAppearance::default(), cx);
}

/// Apply Zmux's terminal appearance policy without reading any Zed-owned
/// settings. The caller supplies the validated Zmux configuration.
pub fn configure_terminal_fonts_with_config(config: &TerminalAppearance, cx: &mut App) {
    let preferred_linux = [
        "JetBrains Mono",
        "SF Mono",
        "Fira Code",
        "Hack",
        "DejaVu Sans Mono",
        "Noto Sans Mono",
        "Courier New",
        "Consolas",
        "monospace",
    ];

    let primary_family = config.font_family.clone().unwrap_or_else(|| {
        find_first_installed_font(&preferred_linux).unwrap_or_else(|| "monospace".to_string())
    });

    let fallbacks = [
        "DejaVu Sans Mono",
        "Noto Sans Mono",
        "Noto Color Emoji",
        "monospace",
    ];

    // Serialize rather than interpolate: a valid user font family can still
    // contain a quote or other JSON-significant character.
    let settings_json = serde_json::to_string(&json!({
        "disable_ai": true,
        "buffer_font_family": primary_family,
        "buffer_font_features": {},
        "buffer_font_size": config.font_size,
        "buffer_font_fallbacks": fallbacks,
        "buffer_line_height": { "custom": config.line_height },
        "terminal": {
            "font_family": primary_family,
            "font_features": {},
            "font_size": config.font_size,
            "font_fallbacks": fallbacks,
            "line_height": { "custom": config.line_height }
        }
    }))
    .expect("terminal settings document is serializable");

    settings::SettingsStore::update_global(cx, |store, cx| {
        let _ = store.set_user_settings(&settings_json, cx);
    });
}
