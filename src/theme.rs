use gpui::{App, UpdateGlobal};
use std::process::Command;

fn find_first_installed_font(preferred: &[&str]) -> Option<String> {
    if cfg!(target_os = "linux") {
        if let Ok(output) = Command::new("fc-list")
            .arg("-f")
            .arg("%{family}\n")
            .output()
        {
            if output.status.success() {
                let out = String::from_utf8_lossy(&output.stdout).to_lowercase();
                for &name in preferred {
                    if out.contains(&name.to_lowercase()) {
                        return Some(name.to_string());
                    }
                }
            }
        }
    }
    None
}

pub fn configure_terminal_fonts(cx: &mut App) {
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

    let primary_family =
        find_first_installed_font(&preferred_linux).unwrap_or_else(|| "monospace".to_string());

    let fallbacks: Vec<&str> = vec![
        "DejaVu Sans Mono",
        "Noto Sans Mono",
        "Noto Color Emoji",
        "monospace",
    ];

    let fallbacks_json = {
        let mut s = String::new();
        s.push('[');
        for (i, f) in fallbacks.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            s.push('"');
            s.push_str(f);
            s.push('"');
        }
        s.push(']');
        s
    };

    let settings_json = format!(
        r#"{{
  "buffer_font_family": "{primary}",
  "buffer_font_features": {{ }},
  "buffer_font_size": 14,
  "buffer_font_fallbacks": {fallbacks},
  "buffer_line_height": {{
    "custom": 1.2
  }},
  "terminal": {{
    "font_family": "{primary}",
    "font_features": {{ }},
    "font_size": 14,
    "font_fallbacks": {fallbacks},
    "line_height": {{
      "custom": 1.2
    }}
  }}
}}"#,
        primary = primary_family,
        fallbacks = fallbacks_json
    );

    settings::SettingsStore::update_global(cx, |store, cx| {
        let _ = store.set_user_settings(&settings_json, cx);
    });
}
