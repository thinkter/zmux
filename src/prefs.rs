//! Durable zmux-owned preferences, kept out of Zed's `SettingsContent`.
//!
//! Lives next to the session file at `state/prefs-v1.json`. The Settings page
//! writes through this module so the sidebar can observe a GPUI global.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use gpui::{App, Global};
use serde::{Deserialize, Serialize};

pub const PREFS_VERSION: u32 = 1;
const MAX_PREFS_BYTES: u64 = 16_384;

/// Whether the agent rail lists every workspace or only the active one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentChatScope {
    #[default]
    Global,
    Workspace,
}

impl AgentChatScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::Global => "All workspaces",
            Self::Workspace => "Current workspace",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PrefsSnapshot {
    version: u32,
    agent_chat_scope: AgentChatScope,
}

impl Default for PrefsSnapshot {
    fn default() -> Self {
        Self {
            version: PREFS_VERSION,
            agent_chat_scope: AgentChatScope::Global,
        }
    }
}

impl PrefsSnapshot {
    fn validate(&self) -> Result<()> {
        if self.version != PREFS_VERSION {
            bail!(
                "unsupported zmux prefs version {}; expected {PREFS_VERSION}",
                self.version
            );
        }
        Ok(())
    }
}

/// Live agent-rail and other zmux-owned UI preferences.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ZmuxPrefs {
    pub agent_chat_scope: AgentChatScope,
}

impl Global for ZmuxPrefs {}

impl ZmuxPrefs {
    pub fn init(cx: &mut App) {
        if cx.has_global::<Self>() {
            return;
        }
        let prefs = match load(&prefs_path()) {
            Ok(prefs) => prefs,
            Err(error) => {
                eprintln!("ignoring invalid zmux prefs: {error:#}");
                Self::default()
            }
        };
        cx.set_global(prefs);
    }

    pub fn get(cx: &App) -> Self {
        cx.try_global::<Self>().cloned().unwrap_or_default()
    }
}

pub fn set_agent_chat_scope(scope: AgentChatScope, cx: &mut App) {
    let mut prefs = ZmuxPrefs::get(cx);
    if prefs.agent_chat_scope == scope && cx.has_global::<ZmuxPrefs>() {
        return;
    }
    prefs.agent_chat_scope = scope;
    if let Err(error) = save(&prefs_path(), &prefs) {
        eprintln!("failed to save zmux prefs: {error:#}");
    }
    cx.set_global(prefs);
}

fn prefs_path() -> PathBuf {
    paths::data_dir().join("state/prefs-v1.json")
}

fn load(path: &Path) -> Result<ZmuxPrefs> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ZmuxPrefs::default());
        }
        Err(error) => return Err(error).context("reading zmux prefs metadata"),
    };
    if metadata.len() > MAX_PREFS_BYTES {
        bail!("zmux prefs exceed the {MAX_PREFS_BYTES}-byte limit");
    }
    let bytes = fs::read(path).context("reading zmux prefs")?;
    let snapshot: PrefsSnapshot = serde_json::from_slice(&bytes).context("parsing zmux prefs")?;
    snapshot.validate()?;
    Ok(ZmuxPrefs {
        agent_chat_scope: snapshot.agent_chat_scope,
    })
}

fn save(path: &Path, prefs: &ZmuxPrefs) -> Result<()> {
    let snapshot = PrefsSnapshot {
        version: PREFS_VERSION,
        agent_chat_scope: prefs.agent_chat_scope,
    };
    snapshot.validate()?;
    let bytes = serde_json::to_vec_pretty(&snapshot).context("serializing zmux prefs")?;
    if bytes.len() as u64 > MAX_PREFS_BYTES {
        bail!("serialized zmux prefs exceed the {MAX_PREFS_BYTES}-byte limit");
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("creating zmux prefs directory")?;
    }
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, &bytes).context("writing zmux prefs")?;
    #[cfg(windows)]
    {
        let _ = fs::remove_file(path);
    }
    fs::rename(&temporary, path).context("installing zmux prefs")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "zmux-prefs-test-{}-{}-{name}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        directory.join("prefs-v1.json")
    }

    #[test]
    fn missing_prefs_file_defaults_to_a_global_agent_rail() {
        let path = test_path("missing");
        let prefs = load(&path).unwrap();
        assert_eq!(prefs.agent_chat_scope, AgentChatScope::Global);
        assert_eq!(AgentChatScope::default(), AgentChatScope::Global);
    }

    #[test]
    fn prefs_round_trip_workspace_scope() {
        let path = test_path("round-trip");
        save(
            &path,
            &ZmuxPrefs {
                agent_chat_scope: AgentChatScope::Workspace,
            },
        )
        .unwrap();

        let prefs = load(&path).unwrap();
        assert_eq!(prefs.agent_chat_scope, AgentChatScope::Workspace);
    }

    #[test]
    fn unknown_prefs_fields_are_rejected() {
        let path = test_path("unknown");
        fs::write(
            &path,
            r#"{ "version": 1, "agent_chat_scope": "global", "extra": true }"#,
        )
        .unwrap();

        assert!(load(&path).is_err());
    }

    #[test]
    fn unsupported_prefs_version_is_rejected() {
        let path = test_path("version");
        fs::write(&path, r#"{ "version": 2, "agent_chat_scope": "global" }"#).unwrap();

        assert!(load(&path).is_err());
    }
}
