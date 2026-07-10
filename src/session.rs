//! Zmux-owned, intentionally small session persistence.
//!
//! This module does not use Zed's database or session machinery.  It stores
//! only enough information to rebuild zmux's *layout*: workspace identity and
//! order, pane topology, terminal working directories, and selection state.
//! In particular, it never stores a command line or attempts to revive a
//! process.  Restoring a terminal creates a fresh shell in its recorded
//! directory.

use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
};

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

pub type SurfaceId = u64;

/// The complete on-disk document.  The `next_workspace_id` watermark is
/// persisted so deleting a workspace can never cause an identity to be reused
/// after a restart.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

        let mut workspace_ids = HashSet::new();
        let mut max_workspace_id = 0;
        for workspace in &self.workspaces {
            if workspace.id == 0 || !workspace_ids.insert(workspace.id) {
                bail!("session contains duplicate or zero workspace IDs");
            }
            if workspace.name.len() > MAX_NAME_BYTES {
                bail!("workspace name exceeds {MAX_NAME_BYTES} bytes");
            }
            workspace.layout.validate()?;
            max_workspace_id = max_workspace_id.max(workspace.id);
        }

        if !workspace_ids.contains(&self.active_workspace_id) {
            bail!("active workspace is absent from the session");
        }
        if self.next_workspace_id <= max_workspace_id {
            bail!("next workspace ID watermark would reuse an existing ID");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSnapshot {
    pub id: WorkspaceId,
    pub name: String,
    pub layout: WorkspaceLayoutSnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceLayoutSnapshot {
    pub active_surface_id: SurfaceId,
    pub root: LayoutNodeSnapshot,
}

impl WorkspaceLayoutSnapshot {
    pub fn single_empty(surface_id: SurfaceId) -> Self {
        Self {
            active_surface_id: surface_id,
            root: LayoutNodeSnapshot::Leaf {
                surface_id,
                tabs: Vec::new(),
                active_tab: 0,
            },
        }
    }

    pub fn validate(&self) -> Result<()> {
        let mut surface_ids = HashSet::new();
        let mut pane_count = 0;
        let mut terminal_count = 0;
        self.root
            .validate(&mut surface_ids, &mut pane_count, &mut terminal_count)?;
        if !surface_ids.contains(&self.active_surface_id) {
            bail!("active surface is absent from the workspace layout");
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LayoutNodeSnapshot {
    Leaf {
        surface_id: SurfaceId,
        tabs: Vec<TerminalSnapshot>,
        active_tab: usize,
    },
    Split {
        axis: LayoutAxis,
        /// Fraction of this split assigned to `first`, in the open interval
        /// `(0, 1)`.  It is derived from actual pane bounds and reapplied on
        /// restore where the host window has enough space.
        ratio: Ratio,
        first: Box<LayoutNodeSnapshot>,
        second: Box<LayoutNodeSnapshot>,
    },
}

impl LayoutNodeSnapshot {
    fn validate(
        &self,
        surface_ids: &mut HashSet<SurfaceId>,
        pane_count: &mut usize,
        terminal_count: &mut usize,
    ) -> Result<()> {
        match self {
            Self::Leaf {
                surface_id,
                tabs,
                active_tab,
            } => {
                *pane_count += 1;
                if *pane_count > MAX_PANES_PER_WORKSPACE {
                    bail!("workspace has more than {MAX_PANES_PER_WORKSPACE} panes");
                }
                if *surface_id == 0 || !surface_ids.insert(*surface_id) {
                    bail!("layout contains duplicate or zero surface IDs");
                }
                if tabs.len() > MAX_TERMINALS_PER_WORKSPACE {
                    bail!("pane has more than {MAX_TERMINALS_PER_WORKSPACE} terminal tabs");
                }
                if !tabs.is_empty() && *active_tab >= tabs.len() {
                    bail!("active tab is outside of the pane's tab list");
                }
                for terminal in tabs {
                    *terminal_count += 1;
                    if *terminal_count > MAX_TERMINALS_PER_WORKSPACE {
                        bail!(
                            "workspace has more than {MAX_TERMINALS_PER_WORKSPACE} terminal tabs"
                        );
                    }
                    terminal.validate()?;
                }
            }
            Self::Split {
                ratio,
                first,
                second,
                ..
            } => {
                ratio.validate()?;
                first.validate(surface_ids, pane_count, terminal_count)?;
                second.validate(surface_ids, pane_count, terminal_count)?;
            }
        }
        Ok(())
    }
}

/// A JSON-safe fractional split ratio.  Keeping it as a newtype lets malformed
/// or non-finite values be rejected before they touch the UI layout engine.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Ratio(f32);

impl Eq for Ratio {}

impl Ratio {
    pub const DEFAULT: Self = Self(0.5);

    pub fn new(value: f32) -> Self {
        // Pane minimum sizes mean exact 0 or 1 is never meaningful.  Keeping a
        // small margin also prevents malformed geometry from producing an
        // unrenderable restore tree.
        Self(value.clamp(0.05, 0.95))
    }

    pub fn get(self) -> f32 {
        self.0
    }

    fn validate(self) -> Result<()> {
        if !self.0.is_finite() || !(0.0..1.0).contains(&self.0) {
            bail!("split ratio must be finite and between zero and one");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalSnapshot {
    pub working_directory: Option<PathBuf>,
    /// This field deliberately has one possible value.  It documents that
    /// restore opens a fresh shell, never replays a command or resurrects an
    /// arbitrary process.
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
        if self.resume != ResumePolicy::Disabled {
            bail!("session resume must be explicitly disabled");
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

/// The only reader/writer for zmux's own session file.
#[derive(Clone, Debug)]
pub struct SessionStore {
    path: PathBuf,
}

impl SessionStore {
    pub fn from_environment() -> Self {
        Self::at(default_session_path())
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    #[cfg(test)]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// `Ok(None)` means there is no usable persisted state.  Callers can safely
    /// fall back to a new workspace; malformed or newer-version state is never
    /// partially applied.
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
            .context("zmux session path has no parent directory")?;
        fs::create_dir_all(parent).context("creating zmux session directory")?;

        let temporary = self.path.with_extension(format!(
            "tmp-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("main")
        ));
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

        // On Unix, rename is an atomic replace.  Windows does not replace an
        // existing target with `rename`, so remove only our own old session
        // before the final move; no Zed-owned path is ever touched.
        #[cfg(windows)]
        if self.path.exists() {
            fs::remove_file(&self.path).context("replacing previous zmux session")?;
        }
        fs::rename(&temporary, &self.path).context("installing zmux session")?;
        Ok(())
    }
}

fn default_session_path() -> PathBuf {
    paths::state_dir().join("session-v1.json")
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_store(name: &str) -> SessionStore {
        let unique = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("zmux-session-test-{}-{unique}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        SessionStore::at(dir.join(format!("{name}.json")))
    }

    fn nested_snapshot() -> SessionSnapshot {
        SessionSnapshot {
            version: SESSION_VERSION,
            next_workspace_id: 9,
            active_workspace_id: 7,
            workspaces: vec![WorkspaceSnapshot {
                id: 7,
                name: "keep this drag order".into(),
                layout: WorkspaceLayoutSnapshot {
                    active_surface_id: 3,
                    root: LayoutNodeSnapshot::Split {
                        axis: LayoutAxis::Horizontal,
                        ratio: Ratio::new(0.3),
                        first: Box::new(LayoutNodeSnapshot::Leaf {
                            surface_id: 2,
                            tabs: vec![TerminalSnapshot::fresh_shell(Some("/tmp/left".into()))],
                            active_tab: 0,
                        }),
                        second: Box::new(LayoutNodeSnapshot::Split {
                            axis: LayoutAxis::Vertical,
                            ratio: Ratio::new(0.7),
                            first: Box::new(LayoutNodeSnapshot::Leaf {
                                surface_id: 3,
                                tabs: vec![
                                    TerminalSnapshot::fresh_shell(Some("/tmp/top-a".into())),
                                    TerminalSnapshot::fresh_shell(Some("/tmp/top-b".into())),
                                ],
                                active_tab: 1,
                            }),
                            second: Box::new(LayoutNodeSnapshot::Leaf {
                                surface_id: 4,
                                tabs: Vec::new(),
                                active_tab: 0,
                            }),
                        }),
                    },
                },
            }],
        }
    }

    #[test]
    fn round_trip_preserves_nested_layout_order_focus_ratios_and_cwds() {
        let store = test_store("round-trip");
        let snapshot = nested_snapshot();
        store.save(&snapshot).unwrap();
        assert_eq!(store.load().unwrap(), Some(snapshot));
    }

    #[test]
    fn malformed_state_is_rejected_without_partial_restore() {
        let store = test_store("malformed");
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        fs::write(store.path(), b"{definitely not json").unwrap();
        assert!(store.load().is_err());
    }

    #[test]
    fn unknown_version_is_rejected() {
        let store = test_store("version");
        let mut snapshot = nested_snapshot();
        snapshot.version = SESSION_VERSION + 1;
        assert!(store.save(&snapshot).is_err());
    }

    #[test]
    fn resume_is_explicitly_and_only_disabled() {
        let snapshot = nested_snapshot();
        let json = serde_json::to_value(snapshot).unwrap();
        assert!(json.to_string().contains("\"disabled\""));

        let mut terminal = serde_json::json!({
            "working_directory": "/tmp",
            "resume": "disabled",
            "command": "rm -rf /"
        });
        assert!(serde_json::from_value::<TerminalSnapshot>(terminal.take()).is_err());
    }

    #[test]
    fn active_workspace_and_next_id_are_checked() {
        let mut snapshot = nested_snapshot();
        snapshot.active_workspace_id = 99;
        assert!(snapshot.validate().is_err());

        let mut snapshot = nested_snapshot();
        snapshot.next_workspace_id = 7;
        assert!(snapshot.validate().is_err());
    }
}
