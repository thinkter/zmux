use collections::HashMap;
use std::path::PathBuf;

pub(crate) fn current_working_directory() -> Option<PathBuf> {
    std::env::current_dir().ok()
}

pub fn terminal_env() -> HashMap<String, String> {
    HashMap::from_iter([
        ("TERM_PROGRAM".to_string(), "zmux".to_string()),
        (
            "TERM_PROGRAM_VERSION".to_string(),
            env!("CARGO_PKG_VERSION").to_string(),
        ),
        ("ZED_TERM".to_string(), "true".to_string()),
        ("TERM".to_string(), "xterm-256color".to_string()),
        ("COLORTERM".to_string(), "truecolor".to_string()),
        // Do not advertise outer terminal image protocols unless zmux renders them.
        ("KITTY_WINDOW_ID".to_string(), String::new()),
        ("KITTY_PID".to_string(), String::new()),
        ("KITTY_PUBLIC_KEY".to_string(), String::new()),
        ("KITTY_INSTALLATION_DIR".to_string(), String::new()),
        ("WEZTERM_PANE".to_string(), String::new()),
        ("GHOSTTY_RESOURCES_DIR".to_string(), String::new()),
    ])
}
