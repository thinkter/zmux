#[test]
fn test_support_uses_private_zmux_roots() {
    let root = paths::test_root();
    let roots = [
        paths::data_dir(),
        paths::config_dir(),
        paths::cache_dir(),
        paths::state_dir(),
        paths::temp_dir(),
        paths::logs_dir(),
    ];

    assert_eq!(paths::APP_NAME, "Zmux");
    for path in roots {
        assert!(
            path.starts_with(root),
            "{path:?} must be contained by the isolated test root {root:?}"
        );
        assert!(path.is_dir(), "{path:?} must exist for tests");
    }

    assert!(paths::database_dir().starts_with(paths::data_dir()));
    assert_ne!(paths::data_dir(), paths::config_dir());
    assert_ne!(paths::cache_dir(), paths::temp_dir());
}
