use anyhow::Context as _;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.get(1).is_some_and(|argument| argument == "notify") {
        let arguments = parse_notify_args(&args)?;
        let notification =
            zmux::CliNotification::new(arguments.title, arguments.subtitle, arguments.body);
        zmux::CliServer::notify(notification)?;
        return Ok(());
    }

    configure_zmux_paths()?;
    zmux::run()
}

/// Point Zed's `paths` crate at a zmux-owned base directory so the settings
/// file and database live under e.g. `~/.local/share/zmux` instead of Zed's
/// own directories. Must run before anything resolves a path;
/// `set_custom_data_dir` panics if called too late. A failed migration aborts
/// startup before the database layer can create `db`, preserving the retry on
/// the next launch.
fn configure_zmux_paths() -> anyhow::Result<()> {
    let base = zmux_data_dir();
    migrate_legacy_database(&base)?;
    paths::set_custom_data_dir(&base.to_string_lossy());
    Ok(())
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
/// directory. Copy it (never move — a real Zed install reads that directory
/// too) into the zmux base on the first run after the switch.
fn migrate_legacy_database(base: &Path) -> anyhow::Result<()> {
    let legacy_db_dir = legacy_zed_data_dir().join("db");
    migrate_legacy_database_from(&legacy_db_dir, base)
        .with_context(|| format!("migrating database from {}", legacy_db_dir.display()))
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

fn migrate_legacy_database_from(legacy_db_dir: &Path, base: &Path) -> anyhow::Result<()> {
    migrate_legacy_database_with(legacy_db_dir, base, &mut copy_database_entry)
}

fn migrate_legacy_database_with(
    legacy_db_dir: &Path,
    base: &Path,
    copy_entry: &mut impl FnMut(&Path, &Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let new_db_dir = base.join("db");
    if new_db_dir
        .try_exists()
        .context("checking for an existing zmux database")?
    {
        return Ok(());
    }

    let legacy_metadata = match fs::metadata(legacy_db_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("reading the legacy database directory"),
    };
    if !legacy_metadata.is_dir() {
        return Ok(());
    }

    fs::create_dir_all(base).context("creating the zmux data directory")?;
    let staging = StagingDirectory::create(base)?;
    copy_dir_recursive(legacy_db_dir, staging.path(), copy_entry)
        .context("copying the legacy database into staging")?;
    sync_directory_tree(staging.path()).context("syncing the staged database")?;
    staging.install(&new_db_dir)
}

fn copy_dir_recursive(
    source: &Path,
    destination: &Path,
    copy_entry: &mut impl FnMut(&Path, &Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut entries = fs::read_dir(source)
        .with_context(|| format!("reading {}", source.display()))?
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let target = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            fs::create_dir(&target)
                .with_context(|| format!("creating staged directory {}", target.display()))?;
            copy_dir_recursive(&entry.path(), &target, copy_entry)?;
        } else if file_type.is_file() {
            copy_entry(&entry.path(), &target)?;
        } else {
            anyhow::bail!(
                "legacy database contains unsupported entry {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn copy_database_entry(source: &Path, destination: &Path) -> anyhow::Result<()> {
    match source.file_name().and_then(OsStr::to_str) {
        // The Zed database uses SQLite in WAL mode. Raw copies of db.sqlite
        // and its sidecars can represent different transactions when Zed is
        // running. SQLite's online backup API instead produces one committed,
        // transactionally consistent snapshot without moving or deleting any
        // legacy data.
        Some("db.sqlite") => backup_sqlite_database(source, destination),
        Some("db.sqlite-wal" | "db.sqlite-shm" | "db.sqlite-journal") => Ok(()),
        _ => copy_regular_file(source, destination),
    }
}

fn backup_sqlite_database(source: &Path, destination: &Path) -> anyhow::Result<()> {
    use db::sqlez::connection::Connection;

    let source_connection = Connection::open_file(&source.to_string_lossy());
    anyhow::ensure!(
        source_connection.persistent(),
        "could not open the legacy SQLite database as a file"
    );
    let destination_connection = Connection::open_file(&destination.to_string_lossy());
    anyhow::ensure!(
        destination_connection.persistent(),
        "could not create the staged SQLite database"
    );
    source_connection
        .backup_main(&destination_connection)
        .context("creating a consistent SQLite backup")?;
    drop(destination_connection);
    drop(source_connection);

    OpenOptions::new()
        .write(true)
        .open(destination)
        .and_then(|file| file.sync_all())
        .context("syncing the staged SQLite database")
}

fn copy_regular_file(source: &Path, destination: &Path) -> anyhow::Result<()> {
    let metadata = fs::metadata(source)?;
    let mut input = File::open(source)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    io::copy(&mut input, &mut output)?;
    output.sync_all()?;
    drop(output);
    fs::set_permissions(destination, metadata.permissions())?;
    Ok(())
}

struct StagingDirectory {
    path: PathBuf,
    installed: bool,
}

impl StagingDirectory {
    fn create(parent: &Path) -> anyhow::Result<Self> {
        for _ in 0..16 {
            let path = parent.join(format!(".db-migration-{}", uuid::Uuid::new_v4()));
            match fs::create_dir(&path) {
                Ok(()) => {
                    return Ok(Self {
                        path,
                        installed: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("creating database migration staging"),
            }
        }
        anyhow::bail!("could not allocate a unique database migration staging directory")
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn install(mut self, destination: &Path) -> anyhow::Result<()> {
        if destination
            .try_exists()
            .context("checking for a concurrent database migration")?
        {
            return Ok(());
        }
        // Staging and destination share a parent, so a successful rename makes
        // the fully copied tree visible in one atomic filesystem operation.
        if let Err(error) = fs::rename(&self.path, destination) {
            if destination
                .try_exists()
                .context("checking the database migration destination")?
            {
                return Ok(());
            }
            return Err(error).context("installing the staged database");
        }
        self.installed = true;
        sync_directory(
            destination
                .parent()
                .context("database destination has no parent")?,
        )
        .context("syncing the installed database directory")
    }
}

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        if !self.installed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(unix)]
fn sync_directory_tree(directory: &Path) -> anyhow::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_directory_tree(&entry.path())?;
        }
    }
    sync_directory(directory)
}

#[cfg(not(unix))]
fn sync_directory_tree(_directory: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> anyhow::Result<()> {
    File::open(directory)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct NotifyArguments {
    title: String,
    subtitle: Option<String>,
    body: String,
}

fn parse_notify_args(args: &[String]) -> anyhow::Result<NotifyArguments> {
    const USAGE: &str =
        "Usage: zmux notify [--title TITLE] [--subtitle SUBTITLE] [--body BODY] [TITLE] [BODY ...]";

    let mut title = None;
    let mut subtitle = None;
    let mut body = None;
    let mut positional = Vec::new();
    let mut positional_only = false;
    let mut index = 2;

    while index < args.len() {
        let argument = &args[index];
        if positional_only {
            positional.push(argument.clone());
            index += 1;
            continue;
        }
        if argument == "--" {
            positional_only = true;
            index += 1;
            continue;
        }

        let (name, inline_value) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(name, value)| {
                (name, Some(value.to_owned()))
            });
        let target = match name {
            "--title" => Some((&mut title, "--title")),
            "--subtitle" => Some((&mut subtitle, "--subtitle")),
            "--body" => Some((&mut body, "--body")),
            _ => None,
        };

        if let Some((target, flag)) = target {
            if target.is_some() {
                anyhow::bail!("{flag} may only be provided once\n{USAGE}");
            }
            let value = if let Some(value) = inline_value {
                value
            } else {
                index += 1;
                args.get(index)
                    .filter(|value| !value.is_empty() && !value.starts_with("--"))
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("{flag} requires a value\n{USAGE}"))?
            };
            if value.is_empty() {
                anyhow::bail!("{flag} requires a non-empty value\n{USAGE}");
            }
            *target = Some(value);
            index += 1;
            continue;
        }

        if argument.starts_with('-') {
            anyhow::bail!("unknown notify option {argument:?}\n{USAGE}");
        }
        positional.push(argument.clone());
        index += 1;
    }

    if title.is_none() && !positional.is_empty() {
        title = Some(positional.remove(0));
    }
    if body.is_none() && !positional.is_empty() {
        body = Some(positional.join(" "));
        positional.clear();
    }
    if !positional.is_empty() {
        let suffix = positional.join(" ");
        let existing = body.get_or_insert_with(String::new);
        if !existing.is_empty() {
            existing.push(' ');
        }
        existing.push_str(&suffix);
    }
    if title.is_none() && body.is_none() {
        anyhow::bail!("{USAGE}");
    }

    Ok(NotifyArguments {
        title: title.unwrap_or_else(|| "Terminal notification".to_owned()),
        subtitle,
        body: body.unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let id = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "zmux-database-migration-test-{}-{id}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn args(arguments: &[&str]) -> Vec<String> {
        ["zmux", "notify"]
            .into_iter()
            .chain(arguments.iter().copied())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn failed_database_staging_is_cleaned_and_the_next_launch_retries() {
        let root = TestDirectory::new();
        let legacy = root.path().join("legacy-db");
        let base = root.path().join("zmux");
        fs::create_dir_all(legacy.join("nested")).unwrap();
        fs::write(legacy.join("a-data"), "one").unwrap();
        fs::write(legacy.join("b-data"), "two").unwrap();
        fs::write(legacy.join("nested/c-data"), "three").unwrap();

        let mut copies = 0;
        let result = migrate_legacy_database_with(&legacy, &base, &mut |source, destination| {
            assert!(
                !base.join("db").exists(),
                "final database became visible before copying completed"
            );
            copies += 1;
            if copies == 2 {
                anyhow::bail!("injected mid-copy failure");
            }
            copy_database_entry(source, destination)
        });
        assert!(result.is_err());
        assert_eq!(copies, 2);
        assert!(!base.join("db").exists());
        assert!(
            fs::read_dir(&base).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".db-migration-")),
            "failed staging directory should be cleaned"
        );

        migrate_legacy_database_from(&legacy, &base).unwrap();
        assert_eq!(fs::read_to_string(base.join("db/a-data")).unwrap(), "one");
        assert_eq!(fs::read_to_string(base.join("db/b-data")).unwrap(), "two");
        assert_eq!(
            fs::read_to_string(base.join("db/nested/c-data")).unwrap(),
            "three"
        );
        assert_eq!(fs::read_to_string(legacy.join("a-data")).unwrap(), "one");
        assert_eq!(fs::read_to_string(legacy.join("b-data")).unwrap(), "two");
    }

    #[test]
    fn existing_zmux_database_is_never_overwritten() {
        let root = TestDirectory::new();
        let legacy = root.path().join("legacy-db");
        let base = root.path().join("zmux");
        fs::create_dir_all(&legacy).unwrap();
        fs::create_dir_all(base.join("db")).unwrap();
        fs::write(legacy.join("state"), "legacy").unwrap();
        fs::write(base.join("db/state"), "current").unwrap();

        let mut copy_was_called = false;
        migrate_legacy_database_with(&legacy, &base, &mut |_, _| {
            copy_was_called = true;
            anyhow::bail!("an existing destination must skip copying")
        })
        .unwrap();

        assert!(!copy_was_called);
        assert_eq!(
            fs::read_to_string(base.join("db/state")).unwrap(),
            "current"
        );
        assert_eq!(fs::read_to_string(legacy.join("state")).unwrap(), "legacy");
    }

    #[test]
    fn live_wal_database_is_migrated_through_a_consistent_sqlite_backup() {
        use db::sqlez::connection::Connection;

        let root = TestDirectory::new();
        let legacy = root.path().join("legacy-db/0-stable");
        let base = root.path().join("zmux");
        fs::create_dir_all(&legacy).unwrap();
        let source_path = legacy.join("db.sqlite");
        let source = Connection::open_file(&source_path.to_string_lossy());
        assert!(source.persistent());
        let journal_mode = source
            .select_row::<String>("PRAGMA journal_mode=WAL;")
            .unwrap()()
        .unwrap()
        .unwrap();
        assert_eq!(journal_mode, "wal");
        source
            .exec("CREATE TABLE records (value TEXT NOT NULL);")
            .unwrap()()
        .unwrap();
        source
            .exec("INSERT INTO records (value) VALUES ('committed');")
            .unwrap()()
        .unwrap();
        assert!(legacy.join("db.sqlite-wal").exists());

        migrate_legacy_database_from(&root.path().join("legacy-db"), &base).unwrap();
        let migrated_scope = base.join("db/0-stable");
        assert!(migrated_scope.join("db.sqlite").exists());
        assert!(!migrated_scope.join("db.sqlite-wal").exists());
        assert!(!migrated_scope.join("db.sqlite-shm").exists());

        let migrated = Connection::open_file(&migrated_scope.join("db.sqlite").to_string_lossy());
        assert!(migrated.persistent());
        let value = migrated
            .select_row::<String>("SELECT value FROM records;")
            .unwrap()()
        .unwrap()
        .unwrap();
        assert_eq!(value, "committed");
        assert!(
            source_path.exists(),
            "the legacy database must remain in place"
        );
    }

    #[test]
    fn parses_positional_title_and_body() {
        assert_eq!(
            parse_notify_args(&args(&["Build finished", "All", "checks", "passed"])).unwrap(),
            NotifyArguments {
                title: "Build finished".to_owned(),
                subtitle: None,
                body: "All checks passed".to_owned(),
            }
        );
    }

    #[test]
    fn parses_named_options_in_any_order() {
        assert_eq!(
            parse_notify_args(&args(&[
                "--body",
                "All checks passed",
                "--title=Build finished",
                "--subtitle",
                "agent-2",
            ]))
            .unwrap(),
            NotifyArguments {
                title: "Build finished".to_owned(),
                subtitle: Some("agent-2".to_owned()),
                body: "All checks passed".to_owned(),
            }
        );
    }

    #[test]
    fn positional_values_fill_only_missing_named_fields() {
        assert_eq!(
            parse_notify_args(&args(&["Fallback title", "--body", "Explicit body"])).unwrap(),
            NotifyArguments {
                title: "Fallback title".to_owned(),
                subtitle: None,
                body: "Explicit body".to_owned(),
            }
        );
        assert_eq!(
            parse_notify_args(&args(&["--title", "Explicit title", "fallback", "body"])).unwrap(),
            NotifyArguments {
                title: "Explicit title".to_owned(),
                subtitle: None,
                body: "fallback body".to_owned(),
            }
        );
        assert_eq!(
            parse_notify_args(&args(&[
                "--title",
                "Explicit title",
                "--body",
                "body",
                "continuation",
            ]))
            .unwrap(),
            NotifyArguments {
                title: "Explicit title".to_owned(),
                subtitle: None,
                body: "body continuation".to_owned(),
            }
        );
    }

    #[test]
    fn body_only_uses_a_safe_default_title() {
        assert_eq!(
            parse_notify_args(&args(&["--body", "Finished"])).unwrap(),
            NotifyArguments {
                title: "Terminal notification".to_owned(),
                subtitle: None,
                body: "Finished".to_owned(),
            }
        );
    }

    #[test]
    fn double_dash_allows_dash_prefixed_positionals() {
        assert_eq!(
            parse_notify_args(&args(&["--", "-build", "done"])).unwrap(),
            NotifyArguments {
                title: "-build".to_owned(),
                subtitle: None,
                body: "done".to_owned(),
            }
        );
    }

    #[test]
    fn rejects_unknown_duplicate_and_incomplete_options() {
        assert!(parse_notify_args(&args(&["--wat", "value"])).is_err());
        assert!(parse_notify_args(&args(&["--title", "one", "--title", "two"])).is_err());
        assert!(parse_notify_args(&args(&["--subtitle"])).is_err());
        assert!(parse_notify_args(&args(&[])).is_err());
    }
}
