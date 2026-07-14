//! Small, zmux-owned logical workspace sessions.
//!
//! Sessions intentionally contain layout and fresh-shell working directories
//! only. Commands, terminal output, environment variables, and process state
//! are never serialized or replayed.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

use crate::notifications::WorkspaceId;

pub const SESSION_VERSION: u32 = 1;
pub const MAX_SESSION_BYTES: u64 = 1_048_576;
pub const MAX_WORKSPACES: usize = 64;
pub const MAX_PANES_PER_WORKSPACE: usize = 128;
pub const MAX_TERMINALS_PER_WORKSPACE: usize = 256;
pub const MAX_NAME_BYTES: usize = 256;
pub const MAX_PATH_BYTES: usize = 4_096;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSnapshot {
    pub version: u32,
    pub next_workspace_id: WorkspaceId,
    pub active_workspace_id: WorkspaceId,
    pub workspaces: Vec<WorkspaceSnapshot>,
}

impl SessionSnapshot {
    pub fn validate(&self) -> Result<()> {
        if self.version != SESSION_VERSION {
            bail!(
                "unsupported zmux session version {}; expected {SESSION_VERSION}",
                self.version
            );
        }
        if self.workspaces.is_empty() || self.workspaces.len() > MAX_WORKSPACES {
            bail!(
                "session contains {} workspaces; expected 1..={MAX_WORKSPACES}",
                self.workspaces.len()
            );
        }

        let mut ids = HashSet::new();
        let mut max_id = 0;
        for workspace in &self.workspaces {
            if workspace.id == 0 || !ids.insert(workspace.id) {
                bail!("session contains duplicate or zero workspace IDs");
            }
            if workspace
                .manual_name
                .as_ref()
                .is_some_and(|name| name.len() > MAX_NAME_BYTES)
            {
                bail!("workspace name exceeds {MAX_NAME_BYTES} bytes");
            }
            for path in [
                workspace.default_directory.as_ref(),
                workspace.selected_git_root.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                if path.as_os_str().len() > MAX_PATH_BYTES {
                    bail!("workspace path exceeds {MAX_PATH_BYTES} bytes");
                }
            }
            workspace.layout.validate()?;
            max_id = max_id.max(workspace.id);
        }
        if !ids.contains(&self.active_workspace_id) {
            bail!("active workspace is absent from the session");
        }
        if self.next_workspace_id <= max_id {
            bail!("next workspace ID would reuse an existing workspace");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSnapshot {
    pub id: WorkspaceId,
    pub manual_name: Option<String>,
    #[serde(default)]
    pub default_directory: Option<PathBuf>,
    #[serde(default)]
    pub selected_git_root: Option<PathBuf>,
    pub layout: LayoutSnapshot,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutSnapshot {
    pub root: LayoutNodeSnapshot,
}

impl LayoutSnapshot {
    pub fn validate(&self) -> Result<()> {
        let mut panes = 0;
        let mut terminals = 0;
        let mut focused_panes = 0;
        self.root
            .validate(&mut panes, &mut terminals, &mut focused_panes)?;
        if focused_panes != 1 {
            bail!("workspace layout must contain exactly one active pane");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutAxis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LayoutNodeSnapshot {
    Leaf {
        tabs: Vec<TerminalSnapshot>,
        active_tab: usize,
        focused: bool,
    },
    Split {
        axis: LayoutAxis,
        ratio: f32,
        first: Box<LayoutNodeSnapshot>,
        second: Box<LayoutNodeSnapshot>,
    },
}

impl LayoutNodeSnapshot {
    fn validate(
        &self,
        panes: &mut usize,
        terminals: &mut usize,
        focused_panes: &mut usize,
    ) -> Result<()> {
        match self {
            Self::Leaf {
                tabs,
                active_tab,
                focused,
            } => {
                *panes += 1;
                if *panes > MAX_PANES_PER_WORKSPACE {
                    bail!("workspace has more than {MAX_PANES_PER_WORKSPACE} panes");
                }
                if tabs.len() > MAX_TERMINALS_PER_WORKSPACE {
                    bail!("pane has more than {MAX_TERMINALS_PER_WORKSPACE} terminals");
                }
                if !tabs.is_empty() && *active_tab >= tabs.len() {
                    bail!("active tab is outside the pane's tab list");
                }
                *terminals += tabs.len();
                if *terminals > MAX_TERMINALS_PER_WORKSPACE {
                    bail!("workspace has more than {MAX_TERMINALS_PER_WORKSPACE} terminals");
                }
                if *focused {
                    *focused_panes += 1;
                }
                for terminal in tabs {
                    terminal.validate()?;
                }
            }
            Self::Split {
                ratio,
                first,
                second,
                ..
            } => {
                if !ratio.is_finite() || !(0.0..1.0).contains(ratio) {
                    bail!("split ratio must be finite and between zero and one");
                }
                first.validate(panes, terminals, focused_panes)?;
                second.validate(panes, terminals, focused_panes)?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalSnapshot {
    pub working_directory: Option<PathBuf>,
    pub resume: ResumePolicy,
}

impl TerminalSnapshot {
    pub fn fresh_shell(working_directory: Option<PathBuf>) -> Self {
        Self {
            working_directory,
            resume: ResumePolicy::Disabled,
        }
    }

    fn validate(&self) -> Result<()> {
        if let Some(path) = &self.working_directory
            && path.as_os_str().len() > MAX_PATH_BYTES
        {
            bail!("terminal working directory exceeds {MAX_PATH_BYTES} bytes");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumePolicy {
    #[default]
    Disabled,
}

#[derive(Clone, Debug)]
pub struct SessionStore {
    path: PathBuf,
}

impl SessionStore {
    pub fn from_environment() -> Self {
        Self::at(paths::data_dir().join("state/session-v1.json"))
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    #[cfg(test)]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn load(&self) -> Result<Option<SessionSnapshot>> {
        let metadata = match fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("reading zmux session metadata"),
        };
        if metadata.len() > MAX_SESSION_BYTES {
            bail!("zmux session exceeds the {MAX_SESSION_BYTES}-byte limit");
        }
        let bytes = fs::read(&self.path).context("reading zmux session")?;
        let snapshot: SessionSnapshot =
            serde_json::from_slice(&bytes).context("parsing zmux session")?;
        snapshot.validate()?;
        Ok(Some(snapshot))
    }

    pub fn save(&self, snapshot: &SessionSnapshot) -> Result<()> {
        snapshot.validate()?;
        let bytes = serde_json::to_vec_pretty(snapshot).context("serializing zmux session")?;
        if bytes.len() as u64 > MAX_SESSION_BYTES {
            bail!("serialized zmux session exceeds the {MAX_SESSION_BYTES}-byte limit");
        }

        let parent = self
            .path
            .parent()
            .context("zmux session path has no parent")?;
        fs::create_dir_all(parent).context("creating zmux session directory")?;
        let temporary = self
            .path
            .with_extension(format!("tmp-{}", std::process::id()));
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .context("creating temporary zmux session")?;
        file.write_all(&bytes)
            .context("writing temporary zmux session")?;
        file.sync_all().context("syncing temporary zmux session")?;
        drop(file);

        #[cfg(windows)]
        if self.path.exists() {
            fs::remove_file(&self.path).context("replacing previous zmux session")?;
        }
        fs::rename(&temporary, &self.path).context("installing zmux session")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_store(name: &str) -> SessionStore {
        let unique = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("zmux-session-test-{}-{unique}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        SessionStore::at(directory.join(format!("{name}.json")))
    }

    fn snapshot() -> SessionSnapshot {
        SessionSnapshot {
            version: SESSION_VERSION,
            next_workspace_id: 3,
            active_workspace_id: 1,
            workspaces: vec![WorkspaceSnapshot {
                id: 1,
                manual_name: Some("API".into()),
                default_directory: Some("/tmp/api".into()),
                selected_git_root: Some("/tmp/api".into()),
                layout: LayoutSnapshot {
                    root: LayoutNodeSnapshot::Split {
                        axis: LayoutAxis::Horizontal,
                        ratio: 0.35,
                        first: Box::new(LayoutNodeSnapshot::Leaf {
                            tabs: vec![TerminalSnapshot::fresh_shell(Some("/tmp/api".into()))],
                            active_tab: 0,
                            focused: true,
                        }),
                        second: Box::new(LayoutNodeSnapshot::Leaf {
                            tabs: vec![TerminalSnapshot::fresh_shell(Some("/tmp/web".into()))],
                            active_tab: 0,
                            focused: false,
                        }),
                    },
                },
            }],
        }
    }

    #[test]
    fn round_trip_preserves_layout_names_ratios_and_directories() {
        let store = test_store("round-trip");
        let snapshot = snapshot();
        store.save(&snapshot).unwrap();
        assert_eq!(store.load().unwrap(), Some(snapshot));
    }

    #[test]
    fn malformed_or_unbounded_sessions_are_rejected() {
        let store = test_store("malformed");
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        fs::write(store.path(), b"{not json").unwrap();
        assert!(store.load().is_err());

        let mut invalid = snapshot();
        invalid.workspaces[0].manual_name = Some("x".repeat(MAX_NAME_BYTES + 1));
        assert!(store.save(&invalid).is_err());
    }

    #[test]
    fn commands_cannot_be_injected_into_terminal_snapshots() {
        let terminal = serde_json::json!({
            "working_directory": "/tmp",
            "resume": "disabled",
            "command": "dangerous"
        });
        assert!(serde_json::from_value::<TerminalSnapshot>(terminal).is_err());
    }
}
