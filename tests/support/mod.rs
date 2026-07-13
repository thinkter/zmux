use task::Shell;

pub fn deterministic_output_shell(output: &str) -> Shell {
    assert!(
        output
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "test output must be safe to embed in a cmd.exe command"
    );

    #[cfg(windows)]
    let (program, args) = (
        "cmd.exe",
        vec![
            "/D".to_string(),
            "/S".to_string(),
            "/C".to_string(),
            format!("echo({output}"),
        ],
    );
    #[cfg(not(windows))]
    let (program, args) = (
        "sh",
        vec![
            "-c".to_string(),
            "printf %s \"$1\"".to_string(),
            "zmux-test-output".to_string(),
            output.to_string(),
        ],
    );

    Shell::WithArguments {
        program: program.to_string(),
        args,
        title_override: None,
    }
}
