use collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::cli_server::NOTIFY_ENDPOINT_ENV;

pub(crate) fn current_working_directory() -> Option<PathBuf> {
    std::env::current_dir().ok()
}

pub fn terminal_env() -> HashMap<String, String> {
    terminal_env_with_notification_endpoint(None)
}

/// Construct a scrubbed terminal environment and optionally advertise the
/// per-terminal notification capability registered by the current zmux
/// process. Never pass one terminal's endpoint when spawning another terminal.
pub fn terminal_env_with_notification_endpoint(
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
        // PTY children inherit process variables that are not explicitly
        // overridden. Scrub an endpoint inherited from an outer zmux process;
        // registered terminals replace this empty value below.
        (NOTIFY_ENDPOINT_ENV.to_string(), String::new()),
    ]);

    // Packaged macOS/Windows launchers are commonly outside the user's PATH.
    // Put this running zmux binary first so `zmux notify` resolves to the same
    // build (and therefore the matching IPC protocol) inside every child PTY.
    if let Some(path) = path_with_running_executable_first() {
        env.insert("PATH".to_owned(), path);
    }

    if let Some(endpoint) = notification_endpoint {
        env.insert(NOTIFY_ENDPOINT_ENV.to_owned(), endpoint.to_owned());
    }

    env
}

fn path_with_running_executable_first() -> Option<String> {
    let executable = std::env::current_exe().ok()?;
    let directory = executable.parent()?;
    prepend_directory_to_path(directory, std::env::var_os("PATH").as_deref())
}

fn prepend_directory_to_path(directory: &Path, inherited: Option<&OsStr>) -> Option<String> {
    let mut entries = inherited
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
        .filter(|entry| !paths_equal(entry, directory))
        .collect::<Vec<_>>();
    entries.insert(0, directory.to_path_buf());
    Some(
        std::env::join_paths(entries)
            .ok()?
            .to_string_lossy()
            .into_owned(),
    )
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    if cfg!(windows) {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        left == right
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubs_an_inherited_endpoint_without_a_registered_route() {
        let env = terminal_env();

        assert_eq!(env.get("TERM_PROGRAM").map(String::as_str), Some("zmux"));
        assert_eq!(env.get(NOTIFY_ENDPOINT_ENV).map(String::as_str), Some(""));
    }

    #[test]
    fn inserts_the_explicit_notification_endpoint() {
        let endpoint = "v3;127.0.0.1:1234;route-selector;route-proof-key;per-terminal-capability";
        let env = terminal_env_with_notification_endpoint(Some(endpoint));

        assert_eq!(
            env.get(NOTIFY_ENDPOINT_ENV).map(String::as_str),
            Some(endpoint)
        );
    }

    #[test]
    fn prepends_the_executable_directory_and_deduplicates_it() {
        let directory = Path::new(if cfg!(windows) {
            r"C:\Program Files\zmux"
        } else {
            "/opt/zmux"
        });
        let other = Path::new(if cfg!(windows) {
            r"C:\Windows"
        } else {
            "/usr/bin"
        });
        let inherited = std::env::join_paths([other, directory, other]).unwrap();

        let path = prepend_directory_to_path(directory, Some(&inherited)).unwrap();
        let entries = std::env::split_paths(OsStr::new(&path)).collect::<Vec<_>>();

        assert_eq!(entries.first().map(PathBuf::as_path), Some(directory));
        assert_eq!(
            entries
                .iter()
                .filter(|entry| paths_equal(entry, directory))
                .count(),
            1
        );
        assert_eq!(entries.iter().filter(|entry| *entry == other).count(), 2);
    }

    #[test]
    fn terminal_path_starts_with_the_running_binary_directory() {
        let env = terminal_env();
        let executable = std::env::current_exe().unwrap();
        let directory = executable.parent().unwrap();
        let entries =
            std::env::split_paths(OsStr::new(env.get("PATH").unwrap())).collect::<Vec<_>>();

        assert!(
            entries
                .first()
                .is_some_and(|entry| paths_equal(entry, directory))
        );
    }
}
