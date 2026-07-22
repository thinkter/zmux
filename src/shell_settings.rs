use std::path::{Path, PathBuf};

use task::Shell;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ShellCandidate {
    pub(crate) label: &'static str,
    pub(crate) program: PathBuf,
}

#[cfg(target_os = "windows")]
const SHELL_CANDIDATES: &[(&str, &str)] = &[
    ("PowerShell 7", "pwsh.exe"),
    ("Windows PowerShell", "powershell.exe"),
    ("Command Prompt", "cmd.exe"),
    ("Git Bash", "bash.exe"),
    ("Nushell", "nu.exe"),
    ("WSL (default distribution)", "wsl.exe"),
];

#[cfg(not(target_os = "windows"))]
const SHELL_CANDIDATES: &[(&str, &str)] = &[
    ("Bash", "bash"),
    ("Zsh", "zsh"),
    ("Fish", "fish"),
    ("Nushell", "nu"),
    ("Xonsh", "xonsh"),
    ("Elvish", "elvish"),
    ("C shell", "csh"),
    ("TENEX C shell", "tcsh"),
];

pub(crate) fn detect_shell_candidates() -> Vec<ShellCandidate> {
    detect_shell_candidates_with(SHELL_CANDIDATES, cfg!(target_os = "windows"), |program| {
        which::which(program).ok()
    })
}

fn detect_shell_candidates_with(
    candidates: &[(&'static str, &'static str)],
    paths_are_case_insensitive: bool,
    mut resolve: impl FnMut(&str) -> Option<PathBuf>,
) -> Vec<ShellCandidate> {
    let mut seen = Vec::<String>::new();
    candidates
        .iter()
        .filter_map(|(label, command)| {
            let program = resolve(command)?;
            let key = path_key(&program, paths_are_case_insensitive);
            if seen.iter().any(|seen| seen == &key) {
                return None;
            }
            seen.push(key);
            Some(ShellCandidate { label, program })
        })
        .collect()
}

fn path_key(path: &Path, case_insensitive: bool) -> String {
    let path = path.to_string_lossy();
    if case_insensitive {
        path.to_lowercase()
    } else {
        path.into_owned()
    }
}

fn looks_like_path(program: &str) -> bool {
    let path = Path::new(program);
    path.is_absolute() || program.contains('/') || program.contains('\\')
}

pub(crate) fn resolve_custom_shell(program: &str) -> Result<PathBuf, String> {
    let program = program.trim();
    if program.is_empty() {
        return Err("Enter a shell executable or command".to_string());
    }

    let path = if looks_like_path(program) {
        let path = PathBuf::from(program);
        if path.is_absolute() {
            path
        } else {
            std::env::current_dir()
                .map_err(|error| format!("Could not resolve the current directory: {error}"))?
                .join(path)
        }
    } else {
        which::which(program).map_err(|_| format!("Could not find `{program}` in PATH"))?
    };

    let metadata = path
        .metadata()
        .map_err(|_| format!("Shell executable does not exist: {}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "Shell executable is not a file: {}",
            path.display()
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(format!("Shell is not executable: {}", path.display()));
        }
    }

    path.canonicalize()
        .map_err(|error| format!("Could not resolve {}: {error}", path.display()))
}

pub(crate) fn shell_spawn_command(
    shell: &Shell,
    is_remote: bool,
) -> (Option<String>, Vec<String>, Shell) {
    if is_remote {
        return (None, Vec::new(), Shell::System);
    }

    match shell {
        Shell::System => (None, Vec::new(), Shell::System),
        Shell::Program(program) => (
            Some(program.clone()),
            Vec::new(),
            Shell::Program(program.clone()),
        ),
        Shell::WithArguments {
            program,
            args,
            title_override,
        } => (
            Some(program.clone()),
            args.clone(),
            Shell::WithArguments {
                program: program.clone(),
                args: args.clone(),
                title_override: title_override.clone(),
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_detection_preserves_order_and_deduplicates_paths() {
        let candidates = &[
            ("First", "first"),
            ("Duplicate", "duplicate"),
            ("Last", "last"),
        ];
        let detected = detect_shell_candidates_with(candidates, false, |command| match command {
            "first" | "duplicate" => Some(PathBuf::from("/bin/shared")),
            "last" => Some(PathBuf::from("/bin/last")),
            _ => None,
        });

        assert_eq!(
            detected,
            vec![
                ShellCandidate {
                    label: "First",
                    program: PathBuf::from("/bin/shared"),
                },
                ShellCandidate {
                    label: "Last",
                    program: PathBuf::from("/bin/last"),
                },
            ]
        );
    }

    #[test]
    fn candidate_detection_deduplicates_windows_paths_case_insensitively() {
        let candidates = &[("First", "first"), ("Duplicate", "duplicate")];
        let detected = detect_shell_candidates_with(candidates, true, |command| match command {
            "first" => Some(PathBuf::from(r"C:\Program Files\PowerShell\pwsh.exe")),
            "duplicate" => Some(PathBuf::from(r"c:\program files\powershell\PWSH.EXE")),
            _ => None,
        });

        assert_eq!(detected.len(), 1);
    }

    #[test]
    fn custom_shell_validation_rejects_empty_and_missing_commands() {
        assert_eq!(
            resolve_custom_shell("").unwrap_err(),
            "Enter a shell executable or command"
        );
        assert!(
            resolve_custom_shell("__zmux_shell_that_does_not_exist__")
                .unwrap_err()
                .contains("Could not find")
        );
    }

    #[test]
    fn spawn_mapping_preserves_program_arguments_and_remote_defaults() {
        let shell = Shell::WithArguments {
            program: "zsh".to_string(),
            args: vec!["-l".to_string()],
            title_override: Some("login".to_string()),
        };

        assert_eq!(
            shell_spawn_command(&shell, false),
            (Some("zsh".to_string()), vec!["-l".to_string()], shell)
        );
        assert_eq!(
            shell_spawn_command(&Shell::Program("fish".to_string()), true),
            (None, Vec::new(), Shell::System)
        );
    }
}
