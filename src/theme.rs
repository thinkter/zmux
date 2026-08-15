//! Default appearance settings for zmux. Bundled themes are loaded from
//! `assets/themes`, and the `.ZedMono` alias resolves to the embedded Lilex
//! family (see `crate::fonts`).

use gpui::{App, UpdateGlobal};
use serde_json::json;

/// GPUI resolves this alias to the embedded Lilex family.
pub const DEFAULT_MONO_FONT: &str = ".ZedMono";
pub const DEFAULT_THEME: &str = "Vercel Dark";
pub const DEFAULT_UI_FONT_SIZE: f32 = 16.0;
pub const DEFAULT_TERMINAL_FONT_SIZE: f32 = 14.0;

pub fn default_settings_json() -> String {
    // Leave explicit fallbacks unset, as Zed does. GPUI merges configured
    // families ahead of the native CoreText, DirectWrite, or fontconfig
    // cascade, so a cross-platform default must not name OS-specific fonts.
    let settings = json!({
        "disable_ai": true,
        "theme": DEFAULT_THEME,
        "ui_font_size": DEFAULT_UI_FONT_SIZE,
        "buffer_font_family": DEFAULT_MONO_FONT,
        "buffer_font_features": {},
        "buffer_font_size": DEFAULT_TERMINAL_FONT_SIZE,
        "buffer_line_height": {
            "custom": 1.2
        },
        "terminal": {
            "font_family": DEFAULT_MONO_FONT,
            "font_features": {},
            "font_size": DEFAULT_TERMINAL_FONT_SIZE,
            "line_height": {
                "custom": 1.2
            }
        }
    });
    serde_json::to_string_pretty(&settings).expect("default settings serialize to JSON")
}

pub fn configure_terminal_fonts(cx: &mut App) {
    let settings_json = default_settings_json();
    settings::SettingsStore::update_global(cx, |store, cx| {
        let _ = store.set_user_settings(&settings_json, cx);
    });
    theme_settings::reload_theme(cx);
}
