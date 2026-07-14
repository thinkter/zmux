//! Default settings for zmux, applied as the user-settings layer when no
//! settings file exists (tests) and used to seed the settings file on first
//! run. Fonts reference the `.ZedMono` alias, which GPUI resolves to the
//! embedded Lilex family (see `crate::fonts`), so no font detection is needed.

use gpui::{App, UpdateGlobal};
use serde_json::json;

/// GPUI resolves this alias to the embedded Lilex family.
pub const DEFAULT_MONO_FONT: &str = ".ZedMono";
pub const DEFAULT_UI_FONT_SIZE: f32 = 16.0;
pub const DEFAULT_TERMINAL_FONT_SIZE: f32 = 14.0;

pub fn default_settings_json() -> String {
    let font_fallbacks = json!(["Lilex", "Noto Sans Mono", "Noto Color Emoji", "monospace"]);
    let settings = json!({
        "disable_ai": true,
        "ui_font_size": DEFAULT_UI_FONT_SIZE,
        "buffer_font_family": DEFAULT_MONO_FONT,
        "buffer_font_features": {},
        "buffer_font_size": DEFAULT_TERMINAL_FONT_SIZE,
        "buffer_font_fallbacks": font_fallbacks,
        "buffer_line_height": {
            "custom": 1.2
        },
        "terminal": {
            "font_family": DEFAULT_MONO_FONT,
            "font_features": {},
            "font_size": DEFAULT_TERMINAL_FONT_SIZE,
            "font_fallbacks": font_fallbacks,
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
}
