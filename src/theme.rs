use gpui::{App, UpdateGlobal};

pub fn configure_terminal_fonts(cx: &mut App) {
    const SETTINGS: &str = r#"
        {
          "buffer_font_family": "JetBrains Mono",
          "buffer_font_features": {},
          "buffer_font_size": 14,
          "buffer_font_fallbacks": ["DejaVu Sans Mono", "Noto Color Emoji"],
          "buffer_line_height": {
            "custom": 1.2
          },
          "terminal": {
            "font_family": "JetBrains Mono",
            "font_features": {},
            "font_size": 14,
            "font_fallbacks": ["DejaVu Sans Mono", "Noto Color Emoji"],
            "line_height": {
              "custom": 1.2
            }
          }
        }
    "#;

    settings::SettingsStore::update_global(cx, |store, cx| {
        let _ = store.set_user_settings(SETTINGS, cx);
    });
}
