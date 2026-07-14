use std::path::{Path, PathBuf};

/// Configure zmux-owned storage and launch the graphical application.
///
/// Windows packages expose separate console and GUI entry points. Keeping the
/// shared startup here ensures both entry points resolve settings and database
/// state identically.
pub fn run_gui() -> anyhow::Result<()> {
    configure_zmux_paths();
    crate::run()
}

/// Point Zed's `paths` crate at a zmux-owned base directory so the settings
/// file and database live under e.g. `~/.local/share/zmux` instead of Zed's
/// own directories. Must run before anything resolves a path;
/// `set_custom_data_dir` panics if called too late.
fn configure_zmux_paths() {
    let base = zmux_data_dir();
    migrate_legacy_database(&base);
    paths::set_custom_data_dir(&base.to_string_lossy());
}

fn zmux_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return dir.join("zmux");
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/zmux")
    } else {
        home.join(".local/share/zmux")
    }
}

/// Earlier zmux builds stored their database inside Zed's shared data
/// directory. Copy it (never move -- a real Zed install reads that directory
/// too) into the zmux base on the first run after the switch.
fn migrate_legacy_database(base: &Path) {
    let new_db_dir = base.join("db");
    if new_db_dir.exists() {
        return;
    }
    let legacy_db_dir = legacy_zed_data_dir().join("db");
    if !legacy_db_dir.is_dir() {
        return;
    }
    if let Err(error) = copy_dir_recursive(&legacy_db_dir, &new_db_dir) {
        eprintln!(
            "failed to migrate database from {}: {error:#}",
            legacy_db_dir.display()
        );
    }
}

fn legacy_zed_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return dir.join("zed");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Zed")
    } else {
        home.join(".local/share/zed")
    }
}

fn copy_dir_recursive(source: &Path, destination: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}
