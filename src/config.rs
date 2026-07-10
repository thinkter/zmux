//! Zmux-owned configuration.
//!
//! This module deliberately does not use Zed's settings paths.  The app can
//! inject a [`ConfigPaths`] value at startup, which keeps ownership explicit
//! and lets the shared paths provider supply the location when it lands.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::{Global, Keystroke};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// The only configuration schema accepted by this build.
pub const CONFIG_SCHEMA_VERSION: u32 = 1;

/// Actions users may override or disable from `keybindings`.
///
/// Keep these stable: they are the configuration-file API, rather than Rust
/// action type names which are implementation details.
pub const CONFIGURABLE_ACTIONS: &[&str] = &[
    "copy",
    "paste",
    "scroll_page_down",
    "scroll_page_up",
    "scroll_to_bottom",
    "new_terminal",
    "new_workspace",
    "toggle_workspaces_panel",
    "next_workspace",
    "previous_workspace",
    "notify_current_pane",
    "jump_to_latest_notification",
    "open_settings",
    "open_keymaps",
    "reload_config",
    "reset_config",
    "next_tab",
    "previous_tab",
    "close_tab",
    "close_all_tabs",
    "close_other_tabs",
    "next_pane",
    "previous_pane",
    "split_right",
    "split_down",
    "increase_font_size",
    "decrease_font_size",
    "reset_font_size",
    "quit",
];

/// An explicit config-file location.  Callers that know about a shared Zmux
/// paths provider should construct this directly instead of relying on the
/// platform fallback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigPaths {
    config_file: PathBuf,
}

/// Supplies the location of the Zmux configuration file.
///
/// This deliberately has a tiny surface so a future shared Zmux paths crate
/// can implement it without this module learning about that crate (or about
/// any Zed path conventions).  Production startup currently injects
/// [`ConfigPaths::platform_default`]; tests and embedders can inject an
/// isolated path instead.
pub trait ConfigPathProvider {
    fn zmux_config_file(&self) -> PathBuf;
}

impl ConfigPaths {
    pub fn new(config_file: impl Into<PathBuf>) -> Self {
        Self {
            config_file: config_file.into(),
        }
    }

    pub fn config_file(&self) -> &Path {
        &self.config_file
    }

    /// The platform fallback used until the shared paths provider is present.
    /// It is intentionally Zmux-owned on every platform.
    pub fn platform_default() -> Self {
        Self::new(platform_config_dir().join("zmux").join("config.json"))
    }

    pub fn from_provider(provider: &impl ConfigPathProvider) -> Self {
        Self::new(provider.zmux_config_file())
    }
}

impl ConfigPathProvider for ConfigPaths {
    fn zmux_config_file(&self) -> PathBuf {
        self.config_file.clone()
    }
}

impl Default for ConfigPaths {
    fn default() -> Self {
        Self::platform_default()
    }
}

fn platform_config_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(path) = std::env::var_os("APPDATA") {
            return PathBuf::from(path);
        }
        if let Some(home) = std::env::var_os("USERPROFILE") {
            return PathBuf::from(home).join("AppData").join("Roaming");
        }
        return std::env::temp_dir();
    }

    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support");
        }
        return std::env::temp_dir();
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                return path;
            }
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".config");
        }
        std::env::temp_dir()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ZmuxConfig {
    pub schema_version: u32,
    pub keybindings: KeybindingsConfig,
    pub terminal: TerminalAppearance,
    pub sidebar: SidebarConfig,
    pub notifications: NotificationPolicy,
    pub automation: AutomationPolicy,
}

impl Default for ZmuxConfig {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            keybindings: KeybindingsConfig::default(),
            terminal: TerminalAppearance::default(),
            sidebar: SidebarConfig::default(),
            notifications: NotificationPolicy::default(),
            automation: AutomationPolicy::default(),
        }
    }
}

impl ZmuxConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version != CONFIG_SCHEMA_VERSION {
            return Err(ConfigError::validation(format!(
                "schema_version must be {CONFIG_SCHEMA_VERSION}, got {}",
                self.schema_version
            )));
        }

        self.keybindings.validate()?;
        self.terminal.validate()?;
        self.sidebar.validate()?;
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KeybindingsConfig {
    /// Action-name to key-sequence overrides. An override replaces every
    /// built-in shortcut for that action.
    pub overrides: BTreeMap<String, String>,
    /// Action names whose built-in shortcuts should not be installed.
    pub disabled: Vec<String>,
}

impl KeybindingsConfig {
    pub fn is_disabled(&self, action: &str) -> bool {
        self.disabled.iter().any(|disabled| disabled == action)
    }

    pub fn override_for(&self, action: &str) -> Option<&str> {
        self.overrides.get(action).map(String::as_str)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let mut disabled = BTreeSet::new();
        for action in &self.disabled {
            validate_action_name(action)?;
            if !disabled.insert(action) {
                return Err(ConfigError::validation(format!(
                    "keybindings.disabled contains {action:?} more than once"
                )));
            }
        }

        for (action, sequence) in &self.overrides {
            validate_action_name(action)?;
            if disabled.contains(action) {
                return Err(ConfigError::validation(format!(
                    "keybinding {action:?} cannot be both overridden and disabled"
                )));
            }
            validate_key_sequence(sequence)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TerminalAppearance {
    /// `null` uses Zmux's platform-appropriate monospace fallback selection.
    pub font_family: Option<String>,
    pub font_size: f32,
    pub line_height: f32,
}

impl Default for TerminalAppearance {
    fn default() -> Self {
        Self {
            font_family: None,
            font_size: 14.0,
            line_height: 1.2,
        }
    }
}

impl TerminalAppearance {
    fn validate(&self) -> Result<(), ConfigError> {
        if !self.font_size.is_finite() || !(6.0..=72.0).contains(&self.font_size) {
            return Err(ConfigError::validation(
                "terminal.font_size must be a finite value from 6 through 72",
            ));
        }
        if !self.line_height.is_finite() || !(0.8..=3.0).contains(&self.line_height) {
            return Err(ConfigError::validation(
                "terminal.line_height must be a finite value from 0.8 through 3",
            ));
        }
        if let Some(font) = &self.font_family
            && (font.trim().is_empty() || font.len() > 256)
        {
            return Err(ConfigError::validation(
                "terminal.font_family must be a non-empty string up to 256 bytes",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SidebarConfig {
    pub starts_open: bool,
    pub show_metadata: bool,
    pub show_working_directory: bool,
    pub show_git_status: bool,
    /// Minimum interval between refreshes of the active workspace.
    pub metadata_refresh_seconds: u64,
    /// Maximum retained scriptable log entries per workspace.
    pub max_log_entries: usize,
}

impl Default for SidebarConfig {
    fn default() -> Self {
        Self {
            starts_open: true,
            show_metadata: true,
            show_working_directory: true,
            show_git_status: true,
            metadata_refresh_seconds: 5,
            max_log_entries: 100,
        }
    }
}

impl SidebarConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if !(1..=3600).contains(&self.metadata_refresh_seconds) {
            return Err(ConfigError::validation(
                "sidebar.metadata_refresh_seconds must be from 1 through 3600",
            ));
        }
        if !(1..=1000).contains(&self.max_log_entries) {
            return Err(ConfigError::validation(
                "sidebar.max_log_entries must be from 1 through 1000",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationPolicy {
    pub enabled: bool,
    pub show_unread_badges: bool,
    pub show_latest_summary: bool,
}

impl Default for NotificationPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            show_unread_badges: true,
            show_latest_summary: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AutomationPolicy {
    /// Enables messages submitted through Zmux's local control surface.
    pub allow_cli_notifications: bool,
    /// Reserved for a future trust prompt. Zmux currently never runs
    /// project-local commands merely because this is true.
    pub allow_trusted_project_commands: bool,
}

impl Default for AutomationPolicy {
    fn default() -> Self {
        Self {
            allow_cli_notifications: true,
            allow_trusted_project_commands: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigSource {
    File,
    MissingFile,
    Migrated { from_version: u32 },
    SafeDefaultsAfterError,
}

/// App-global, in-memory configuration state. Invalid on-disk changes never
/// replace the last known-good configuration.
pub struct ConfigStore {
    paths: ConfigPaths,
    config: ZmuxConfig,
    source: ConfigSource,
    last_error: Option<String>,
    /// The last complete document observed by a forced reload or the watcher.
    /// Storing the tiny config text avoids repeated parse attempts for one bad
    /// save and catches changes even on filesystems with coarse mtimes.
    last_disk_contents: Option<String>,
}

impl Global for ConfigStore {}

impl ConfigStore {
    pub fn load_or_default(paths: ConfigPaths) -> Self {
        match fs::read_to_string(paths.config_file()) {
            Ok(contents) => match parse_config(&contents) {
                Ok(parsed) => Self {
                    paths,
                    config: parsed.config,
                    source: parsed
                        .migrated_from
                        .map(|from_version| ConfigSource::Migrated { from_version })
                        .unwrap_or(ConfigSource::File),
                    last_error: None,
                    last_disk_contents: Some(contents),
                },
                Err(error) => Self {
                    paths,
                    config: ZmuxConfig::default(),
                    source: ConfigSource::SafeDefaultsAfterError,
                    last_error: Some(error.to_string()),
                    last_disk_contents: Some(contents),
                },
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self {
                paths,
                config: ZmuxConfig::default(),
                source: ConfigSource::MissingFile,
                last_error: None,
                last_disk_contents: Some(String::new()),
            },
            Err(error) => Self {
                paths,
                config: ZmuxConfig::default(),
                source: ConfigSource::SafeDefaultsAfterError,
                last_error: Some(format!("could not read config: {error}")),
                last_disk_contents: None,
            },
        }
    }

    pub fn global(cx: &gpui::App) -> &Self {
        cx.global::<Self>()
    }

    pub fn global_mut(cx: &mut gpui::App) -> &mut Self {
        cx.global_mut::<Self>()
    }

    pub fn paths(&self) -> &ConfigPaths {
        &self.paths
    }

    pub fn config(&self) -> &ZmuxConfig {
        &self.config
    }

    pub fn source(&self) -> &ConfigSource {
        &self.source
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Reads the file synchronously. Callers normally use this from an action;
    /// the file is deliberately tiny and validation is deterministic.
    pub fn reload(&mut self) -> Result<ConfigReload, ConfigError> {
        let contents = self.read_file()?;
        self.last_disk_contents = Some(contents.clone());
        self.reload_from_text(&contents)
    }

    /// Poll-friendly reload that only parses a document when its contents have
    /// changed.  A malformed write is remembered too, so the live watcher does
    /// not repeatedly parse/log the same bad file while retaining the last
    /// known-good configuration in memory.
    pub fn reload_if_changed(&mut self) -> Result<Option<ConfigReload>, ConfigError> {
        let contents = self.read_file()?;
        if self.last_disk_contents.as_deref() == Some(contents.as_str()) {
            return Ok(None);
        }
        self.last_disk_contents = Some(contents.clone());
        self.reload_from_text(&contents).map(Some)
    }

    /// Replace the in-memory configuration only after parsing and validation
    /// succeeds. This is used by the live file watcher as well as tests.
    pub fn reload_from_text(&mut self, contents: &str) -> Result<ConfigReload, ConfigError> {
        let parsed = match parse_config(contents) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.last_error = Some(error.to_string());
                return Err(error);
            }
        };
        let changed = self.config != parsed.config;
        self.config = parsed.config;
        self.source = parsed
            .migrated_from
            .map(|from_version| ConfigSource::Migrated { from_version })
            .unwrap_or_else(|| {
                if contents.trim().is_empty() {
                    ConfigSource::MissingFile
                } else {
                    ConfigSource::File
                }
            });
        self.last_error = None;
        Ok(ConfigReload {
            changed,
            migrated_from: parsed.migrated_from,
        })
    }

    /// Writes a complete, validated v1 document to the Zmux-owned path using a
    /// same-directory temporary file followed by rename.
    pub fn save(&mut self, config: ZmuxConfig) -> Result<(), ConfigError> {
        config.validate()?;
        let serialized = serde_json::to_string_pretty(&config)
            .map_err(|error| ConfigError::serialization(error.to_string()))?;
        let contents = format!("{serialized}\n");
        atomic_write(self.paths.config_file(), contents.clone())?;
        self.config = config;
        self.source = ConfigSource::File;
        self.last_error = None;
        self.last_disk_contents = Some(contents);
        Ok(())
    }

    /// Parse, validate, and atomically persist an editor buffer.  This is the
    /// write counterpart to [`reload_from_text`], and never replaces the
    /// on-disk file with malformed text.
    pub fn save_from_text(&mut self, contents: &str) -> Result<ConfigReload, ConfigError> {
        let parsed = parse_config(contents)?;
        let changed = self.config != parsed.config;
        let migrated_from = parsed.migrated_from;
        self.save(parsed.config)?;
        Ok(ConfigReload {
            changed,
            migrated_from,
        })
    }

    /// Creates the editable file on first use without overwriting an invalid
    /// existing file. The caller receives the exact file to open in-app.
    pub fn ensure_file(&mut self) -> Result<PathBuf, ConfigError> {
        if !self.paths.config_file().exists() {
            self.save(self.config.clone())?;
        }
        Ok(self.paths.config_file().to_path_buf())
    }

    /// Return the text shown by the in-app editor, creating a canonical
    /// default document on first use. Existing invalid files are deliberately
    /// not overwritten: the user must be able to inspect and repair them.
    pub fn editable_contents(&mut self) -> Result<String, ConfigError> {
        let path = self.ensure_file()?;
        fs::read_to_string(path).map_err(|error| ConfigError::io("read config", error))
    }

    pub fn reset(&mut self) -> Result<(), ConfigError> {
        self.save(ZmuxConfig::default())
    }

    fn read_file(&mut self) -> Result<String, ConfigError> {
        match fs::read_to_string(self.paths.config_file()) {
            Ok(contents) => Ok(contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => {
                let error = ConfigError::io("read config", error);
                self.last_error = Some(error.to_string());
                Err(error)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigReload {
    pub changed: bool,
    pub migrated_from: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    message: String,
}

impl ConfigError {
    fn validation(message: impl Into<String>) -> Self {
        Self {
            message: format!("invalid zmux config: {}", message.into()),
        }
    }

    fn io(operation: &str, error: std::io::Error) -> Self {
        Self {
            message: format!("could not {operation}: {error}"),
        }
    }

    fn serialization(message: String) -> Self {
        Self {
            message: format!("could not serialize zmux config: {message}"),
        }
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

struct ParsedConfig {
    config: ZmuxConfig,
    migrated_from: Option<u32>,
}

fn parse_config(contents: &str) -> Result<ParsedConfig, ConfigError> {
    if contents.trim().is_empty() {
        return Ok(ParsedConfig {
            config: ZmuxConfig::default(),
            migrated_from: None,
        });
    }

    let mut document: Value = serde_json::from_str(contents)
        .map_err(|error| ConfigError::validation(format!("JSON parse error: {error}")))?;
    let object = document
        .as_object_mut()
        .ok_or_else(|| ConfigError::validation("top level must be a JSON object"))?;

    let migrated_from = migrate_document(object)?;
    let config: ZmuxConfig = serde_json::from_value(document)
        .map_err(|error| ConfigError::validation(error.to_string()))?;
    config.validate()?;

    Ok(ParsedConfig {
        config,
        migrated_from,
    })
}

/// A small, explicit migration path for pre-versioned prototypes. It exists so
/// a future schema bump has one well-tested place to add a migration rather
/// than silently accepting unknown fields.
fn migrate_document(document: &mut Map<String, Value>) -> Result<Option<u32>, ConfigError> {
    let version = match document.get("schema_version") {
        Some(Value::Number(number)) => {
            let version = number.as_u64().ok_or_else(|| {
                ConfigError::validation("schema_version must be an unsigned integer")
            })?;
            u32::try_from(version).map_err(|_| {
                ConfigError::validation("schema_version is too large to be supported")
            })?
        }
        Some(_) => {
            return Err(ConfigError::validation(
                "schema_version must be an unsigned integer",
            ));
        }
        None => 0,
    };

    match version {
        CONFIG_SCHEMA_VERSION => Ok(None),
        0 => {
            // `shortcuts` and `terminal_font_size` were never a released
            // schema, but accepting them here makes pre-versioned local builds
            // migrate deterministically instead of losing customization.
            if let Some(shortcuts) = document.remove("shortcuts") {
                if document.contains_key("keybindings") {
                    return Err(ConfigError::validation(
                        "v0 config cannot contain both shortcuts and keybindings",
                    ));
                }
                document.insert("keybindings".to_string(), json!({ "overrides": shortcuts }));
            }
            if let Some(font_size) = document.remove("terminal_font_size") {
                let terminal = document
                    .entry("terminal".to_string())
                    .or_insert_with(|| Value::Object(Map::new()));
                let terminal = terminal.as_object_mut().ok_or_else(|| {
                    ConfigError::validation("terminal must be an object during v0 migration")
                })?;
                terminal.insert("font_size".to_string(), font_size);
            }
            document.insert(
                "schema_version".to_string(),
                Value::Number(CONFIG_SCHEMA_VERSION.into()),
            );
            Ok(Some(0))
        }
        future => Err(ConfigError::validation(format!(
            "schema_version {future} is newer than this Zmux build supports"
        ))),
    }
}

fn validate_action_name(action: &str) -> Result<(), ConfigError> {
    if CONFIGURABLE_ACTIONS.contains(&action) {
        Ok(())
    } else {
        Err(ConfigError::validation(format!(
            "unknown configurable action {action:?}"
        )))
    }
}

fn validate_key_sequence(sequence: &str) -> Result<(), ConfigError> {
    if sequence.trim().is_empty() {
        return Err(ConfigError::validation(
            "a keybinding override cannot be empty; use disabled to remove a binding",
        ));
    }
    for keystroke in sequence.split_whitespace() {
        Keystroke::parse(keystroke).map_err(|error| {
            ConfigError::validation(format!("invalid keybinding {sequence:?}: {error}"))
        })?;
    }
    Ok(())
}

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn atomic_write(path: &Path, contents: String) -> Result<(), ConfigError> {
    let parent = path
        .parent()
        .ok_or_else(|| ConfigError::validation("config path has no parent directory"))?;
    fs::create_dir_all(parent)
        .map_err(|error| ConfigError::io("create config directory", error))?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.json");
    let mut temporary_path = None;
    let mut temporary_file = None;
    for _ in 0..16 {
        let nonce = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".{file_name}.{}.{}.tmp", std::process::id(), nonce));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                temporary_path = Some(candidate);
                temporary_file = Some(file);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(ConfigError::io("create temporary config", error)),
        }
    }

    let temporary_path = temporary_path.ok_or_else(|| {
        ConfigError::validation("could not allocate a unique temporary config file")
    })?;
    let mut temporary_file = temporary_file.expect("temporary path always has a file handle");
    let write_result = (|| {
        temporary_file
            .write_all(contents.as_bytes())
            .map_err(|error| ConfigError::io("write temporary config", error))?;
        temporary_file
            .sync_all()
            .map_err(|error| ConfigError::io("sync temporary config", error))?;
        drop(temporary_file);
        fs::rename(&temporary_path, path)
            .map_err(|error| ConfigError::io("replace config", error))?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    write_result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "zmux-config-test-{}-{}-{name}",
            std::process::id(),
            TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn defaults_are_deterministic_and_valid() {
        let config = ZmuxConfig::default();
        config.validate().unwrap();
        assert_eq!(config.schema_version, CONFIG_SCHEMA_VERSION);
        assert_eq!(config.sidebar.max_log_entries, 100);
    }

    #[test]
    fn partial_user_config_receives_safe_defaults() {
        let parsed =
            parse_config(r#"{ "schema_version": 1, "terminal": { "font_size": 16 } }"#).unwrap();
        assert_eq!(parsed.config.terminal.font_size, 16.0);
        assert_eq!(parsed.config.terminal.line_height, 1.2);
        assert!(parsed.config.notifications.enabled);
    }

    #[test]
    fn unknown_fields_and_invalid_keys_are_rejected() {
        let unknown = parse_config(r#"{ "schema_version": 1, "surprise": true }"#);
        assert!(unknown.is_err());

        let invalid_key = parse_config(
            r#"{ "schema_version": 1, "keybindings": { "overrides": { "new_terminal": "ctrl-a-b" } } }"#,
        );
        assert!(invalid_key.is_err());
    }

    #[test]
    fn v0_shortcuts_migrate_without_losing_the_override() {
        let parsed = parse_config(r#"{ "shortcuts": { "new_terminal": "ctrl-alt-t" } }"#).unwrap();
        assert_eq!(parsed.migrated_from, Some(0));
        assert_eq!(
            parsed.config.keybindings.override_for("new_terminal"),
            Some("ctrl-alt-t")
        );
    }

    #[test]
    fn save_is_atomic_from_the_callers_point_of_view() {
        let path = test_path("config.json");
        let mut store = ConfigStore::load_or_default(ConfigPaths::new(path.clone()));
        store.save(ZmuxConfig::default()).unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("\"schema_version\": 1"));
        assert!(!path.with_extension("json.tmp").exists());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn bad_reload_keeps_the_last_good_config() {
        let path = test_path("config.json");
        let mut store = ConfigStore::load_or_default(ConfigPaths::new(path));
        let before = store.config().clone();
        assert!(store.reload_from_text("not json").is_err());
        assert_eq!(store.config(), &before);
        assert!(store.last_error().is_some());
    }

    #[test]
    fn content_watcher_reloads_once_and_keeps_last_good_state() {
        let path = test_path("config.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut store = ConfigStore::load_or_default(ConfigPaths::new(path.clone()));
        assert_eq!(store.reload_if_changed().unwrap(), None);

        fs::write(
            &path,
            r#"{ "schema_version": 1, "terminal": { "font_size": 18 } }"#,
        )
        .unwrap();
        assert!(store.reload_if_changed().unwrap().unwrap().changed);
        assert_eq!(store.config().terminal.font_size, 18.0);
        assert_eq!(store.reload_if_changed().unwrap(), None);

        fs::write(&path, "{").unwrap();
        assert!(store.reload_if_changed().is_err());
        assert_eq!(store.config().terminal.font_size, 18.0);
        // The unchanged bad document is not repeatedly reparsed by a watcher.
        assert_eq!(store.reload_if_changed().unwrap(), None);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn config_path_provider_is_explicit_and_rebasable() {
        struct TestProvider(PathBuf);
        impl ConfigPathProvider for TestProvider {
            fn zmux_config_file(&self) -> PathBuf {
                self.0.clone()
            }
        }

        let provider = TestProvider(PathBuf::from("/isolated/zmux/config.json"));
        assert_eq!(
            ConfigPaths::from_provider(&provider).config_file(),
            Path::new("/isolated/zmux/config.json")
        );
    }
}
