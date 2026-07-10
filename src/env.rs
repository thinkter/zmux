use collections::HashMap;
use std::path::PathBuf;

use crate::cli_server::NOTIFICATION_ENDPOINT_ENV;

pub(crate) fn current_working_directory() -> Option<PathBuf> {
    std::env::current_dir().ok()
}

pub fn terminal_env() -> HashMap<String, String> {
    terminal_env_with_notification_endpoint(None)
}

/// Build the environment for a terminal spawned by zmux.
///
/// The notification endpoint is supplied by the running app instance rather
/// than inherited from the parent shell, so nested terminals always route to
/// their owning zmux process.
pub(crate) fn terminal_env_with_notification_endpoint(
    notification_endpoint: Option<&str>,
) -> HashMap<String, String> {
    let mut env = HashMap::from_iter([
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
    ]);

    if let Some(notification_endpoint) = notification_endpoint {
        env.insert(
            NOTIFICATION_ENDPOINT_ENV.to_string(),
            notification_endpoint.to_string(),
        );
    }

    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_environment_advertises_the_instance_notification_endpoint() {
        let env = terminal_env_with_notification_endpoint(Some("/private/zmux/notify.sock"));

        assert_eq!(
            env.get(NOTIFICATION_ENDPOINT_ENV).map(String::as_str),
            Some("/private/zmux/notify.sock")
        );
    }
}
