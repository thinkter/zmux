//! Small, zmux-owned logical workspace sessions.
//!
//! Sessions intentionally contain layout and fresh-shell working directories
//! only. Commands, terminal output, environment variables, and process state
//! are never serialized or replayed.

use std::collections::HashSet;
use std::ffi::OsString;
#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

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
const STALE_TEMPORARY_AGE: Duration = Duration::from_secs(60 * 60);

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
            if [
                workspace.manual_name.as_ref(),
                workspace.worktree_name.as_ref(),
            ]
            .into_iter()
            .flatten()
            .any(|name| name.len() > MAX_NAME_BYTES)
            {
                bail!("workspace or worktree name exceeds {MAX_NAME_BYTES} bytes");
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
            for path in &workspace.worktree_paths {
                if path.as_os_str().len() > MAX_PATH_BYTES {
                    bail!("workspace worktree path exceeds {MAX_PATH_BYTES} bytes");
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
    /// Friendly linked-worktree name shown independently of the repository's
    /// directory name. Absent for ordinary folder-backed workspaces.
    #[serde(default)]
    pub worktree_name: Option<String>,
    #[serde(default)]
    pub worktree_paths: Vec<PathBuf>,
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

    /// Drop panes that carry no terminal tabs — e.g. a pane that only held a
    /// diff view when the session was captured. A split whose side disappears
    /// gives its space back to the surviving side; if the focused pane was
    /// dropped, focus falls back to the first remaining pane. An entirely
    /// empty layout collapses to a single empty focused pane.
    pub fn without_empty_panes(&self) -> Self {
        let mut root = self
            .root
            .without_empty_panes()
            .unwrap_or(LayoutNodeSnapshot::Leaf {
                tabs: Vec::new(),
                active_tab: 0,
                focused: true,
            });
        if !root.contains_focused_pane() {
            root.focus_first_pane();
        }
        Self { root }
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

    fn without_empty_panes(&self) -> Option<Self> {
        match self {
            Self::Leaf { tabs, .. } => (!tabs.is_empty()).then(|| self.clone()),
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => match (first.without_empty_panes(), second.without_empty_panes()) {
                (Some(first), Some(second)) => Some(Self::Split {
                    axis: *axis,
                    ratio: *ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (child, None) | (None, child) => child,
            },
        }
    }

    fn contains_focused_pane(&self) -> bool {
        match self {
            Self::Leaf { focused, .. } => *focused,
            Self::Split { first, second, .. } => {
                first.contains_focused_pane() || second.contains_focused_pane()
            }
        }
    }

    fn focus_first_pane(&mut self) {
        match self {
            Self::Leaf { focused, .. } => *focused = true,
            Self::Split { first, .. } => first.focus_first_pane(),
        }
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

#[derive(Debug, Default)]
struct SessionWriter {
    next_sequence: AtomicU64,
    newest_sequence: AtomicU64,
    owner_generation: AtomicU64,
    write_lock: Mutex<()>,
}

/// Durable session-file boundary.
///
/// `WorkspacesPanel` coalesces ordinary UI requests, while this store also
/// orders direct/concurrent callers and performs the platform-specific durable
/// replacement. Keeping both layers is intentional: scheduler correctness must
/// not be required for an older write to avoid replacing a newer file.
#[derive(Clone, Debug)]
pub struct SessionStore {
    path: PathBuf,
    writer: Arc<SessionWriter>,
}

#[derive(Debug)]
pub struct SessionWrite {
    owner_generation: SessionOwnerGeneration,
    sequence: u64,
    snapshot: SessionSnapshot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionOwnerGeneration(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionWriteOutcome {
    Installed,
    Superseded,
}

impl SessionStore {
    pub fn from_environment() -> Self {
        Self::at(paths::data_dir().join("state/session-v1.json"))
    }

    pub fn at(path: PathBuf) -> Self {
        Self {
            path,
            writer: Arc::new(SessionWriter::default()),
        }
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

    /// Start a new persistence-owner epoch. Writes prepared by every previous
    /// owner become stale immediately, even if the new owner has not prepared
    /// its first snapshot yet.
    pub fn begin_owner_generation(&self) -> Result<SessionOwnerGeneration> {
        let generation = self
            .writer
            .owner_generation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .ok()
            .and_then(|previous| previous.checked_add(1))
            .context("zmux session owner generation overflowed")?;
        Ok(SessionOwnerGeneration(generation))
    }

    /// Reserve this write's place in the newest-wins order. Validation and
    /// serialization are deferred to `commit` so callers on the UI thread
    /// only pay for atomic increments here.
    pub fn prepare_save(
        &self,
        snapshot: &SessionSnapshot,
        owner_generation: SessionOwnerGeneration,
    ) -> Result<SessionWrite> {
        let sequence = self
            .writer
            .next_sequence
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .ok()
            .and_then(|previous| previous.checked_add(1))
            .context("zmux session write sequence overflowed")?;
        self.writer
            .newest_sequence
            .fetch_max(sequence, Ordering::Release);
        Ok(SessionWrite {
            owner_generation,
            sequence,
            snapshot: snapshot.clone(),
        })
    }

    pub fn commit(&self, write: &SessionWrite) -> Result<SessionWriteOutcome> {
        write.snapshot.validate()?;
        let bytes =
            serde_json::to_vec_pretty(&write.snapshot).context("serializing zmux session")?;
        if bytes.len() as u64 > MAX_SESSION_BYTES {
            bail!("serialized zmux session exceeds the {MAX_SESSION_BYTES}-byte limit");
        }

        let _write_guard = self
            .writer
            .write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.is_superseded(write) {
            return Ok(SessionWriteOutcome::Superseded);
        }

        let parent = self
            .path
            .parent()
            .context("zmux session path has no parent")?;
        fs::create_dir_all(parent).context("creating zmux session directory")?;
        // Reclaim crash-orphaned siblings without deleting a young temporary
        // that another process may still be preparing for the same path.
        remove_stale_temporaries(&self.path);
        let temporary = temporary_path(&self.path)?;
        let mut temporary_created = false;
        let result = (|| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .context("creating temporary zmux session")?;
            temporary_created = true;
            file.write_all(&bytes)
                .context("writing temporary zmux session")?;
            file.sync_all().context("syncing temporary zmux session")?;
            drop(file);

            // A newer request may have arrived while the temporary file was
            // being written. Do not let this older request reach the durable
            // session path after that point.
            if self.is_superseded(write) {
                return Ok(SessionWriteOutcome::Superseded);
            }

            install_session_file(&temporary, &self.path)?;
            Ok(SessionWriteOutcome::Installed)
        })();

        // This is a no-op after a successful rename and removes partial files
        // after every other exit, leaving the write retryable.
        if temporary_created {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    #[cfg(test)]
    pub fn save(&self, snapshot: &SessionSnapshot) -> Result<SessionWriteOutcome> {
        let generation =
            SessionOwnerGeneration(self.writer.owner_generation.load(Ordering::Acquire));
        let write = self.prepare_save(snapshot, generation)?;
        self.commit(&write)
    }

    fn is_superseded(&self, write: &SessionWrite) -> bool {
        write.owner_generation
            != SessionOwnerGeneration(self.writer.owner_generation.load(Ordering::Acquire))
            || write.sequence < self.writer.newest_sequence.load(Ordering::Acquire)
    }
}

fn remove_stale_temporaries(path: &Path) {
    let (Some(parent), Some(file_name)) = (path.parent(), path.file_name()) else {
        return;
    };
    let mut prefix = file_name.to_os_string();
    prefix.push(".tmp-");
    let prefix = prefix.to_string_lossy().into_owned();
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok();
        if entry.file_name().to_string_lossy().starts_with(&prefix)
            && modified.is_some_and(|modified| temporary_is_stale(modified, now))
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn temporary_is_stale(modified: SystemTime, now: SystemTime) -> bool {
    now.duration_since(modified)
        .is_ok_and(|age| age >= STALE_TEMPORARY_AGE)
}

fn temporary_path(path: &Path) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .context("zmux session path has no file name")?;
    let mut temporary_name = OsString::from(file_name);
    temporary_name.push(format!(
        ".tmp-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    Ok(path.with_file_name(temporary_name))
}

#[cfg(unix)]
fn install_session_file(temporary: &Path, destination: &Path) -> Result<()> {
    fs::rename(temporary, destination).context("installing zmux session")?;
    let parent = destination
        .parent()
        .context("zmux session path has no parent")?;
    File::open(parent)
        .context("opening zmux session directory for sync")?
        .sync_all()
        .context("syncing zmux session directory")
}

#[cfg(windows)]
fn install_session_file(temporary: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt as _;

    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows::core::PCWSTR;

    let temporary = temporary
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let temporary = PCWSTR::from_raw(temporary.as_ptr());
    let destination = PCWSTR::from_raw(destination.as_ptr());

    unsafe {
        MoveFileExW(
            temporary,
            destination,
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .context("atomically installing zmux session")
}

#[cfg(not(any(unix, windows)))]
fn install_session_file(temporary: &Path, destination: &Path) -> Result<()> {
    fs::rename(temporary, destination).context("installing zmux session")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;

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
                worktree_name: Some("feature-api".into()),
                worktree_paths: vec!["/tmp/api".into()],
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
    fn pruning_collapses_panes_that_lost_all_tabs() {
        let layout = LayoutSnapshot {
            root: LayoutNodeSnapshot::Split {
                axis: LayoutAxis::Horizontal,
                ratio: 0.7,
                first: Box::new(LayoutNodeSnapshot::Leaf {
                    tabs: vec![TerminalSnapshot::fresh_shell(Some("/tmp/api".into()))],
                    active_tab: 0,
                    focused: false,
                }),
                second: Box::new(LayoutNodeSnapshot::Leaf {
                    tabs: Vec::new(),
                    active_tab: 0,
                    focused: true,
                }),
            },
        };

        let pruned = layout.without_empty_panes();
        assert_eq!(
            pruned.root,
            LayoutNodeSnapshot::Leaf {
                tabs: vec![TerminalSnapshot::fresh_shell(Some("/tmp/api".into()))],
                active_tab: 0,
                focused: true,
            }
        );
        pruned.validate().unwrap();
    }

    #[test]
    fn pruning_an_entirely_empty_layout_keeps_one_focused_pane() {
        let layout = LayoutSnapshot {
            root: LayoutNodeSnapshot::Split {
                axis: LayoutAxis::Vertical,
                ratio: 0.5,
                first: Box::new(LayoutNodeSnapshot::Leaf {
                    tabs: Vec::new(),
                    active_tab: 0,
                    focused: true,
                }),
                second: Box::new(LayoutNodeSnapshot::Leaf {
                    tabs: Vec::new(),
                    active_tab: 0,
                    focused: false,
                }),
            },
        };

        let pruned = layout.without_empty_panes();
        assert_eq!(
            pruned.root,
            LayoutNodeSnapshot::Leaf {
                tabs: Vec::new(),
                active_tab: 0,
                focused: true,
            }
        );
        pruned.validate().unwrap();
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
    fn only_old_temporary_session_files_are_reclaimed() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
        assert!(!temporary_is_stale(
            now - STALE_TEMPORARY_AGE + Duration::from_secs(1),
            now
        ));
        assert!(temporary_is_stale(now - STALE_TEMPORARY_AGE, now));
        assert!(!temporary_is_stale(now + Duration::from_secs(1), now));
    }

    #[test]
    fn stale_temporary_cleanup_is_scoped_to_the_session_file() {
        let store = test_store("stale-temporary-cleanup");
        let parent = store.path().parent().unwrap();
        fs::create_dir_all(parent).unwrap();
        let session_name = store.path().file_name().unwrap().to_string_lossy();
        let stale = parent.join(format!("{session_name}.tmp-dead-process"));
        let recent = parent.join(format!("{session_name}.tmp-live-process"));
        let unrelated = parent.join("other-session.json.tmp-dead-process");
        fs::write(&stale, "stale").unwrap();
        fs::write(&recent, "recent").unwrap();
        fs::write(&unrelated, "unrelated").unwrap();
        OpenOptions::new()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new().set_modified(SystemTime::now() - STALE_TEMPORARY_AGE),
            )
            .unwrap();

        remove_stale_temporaries(store.path());

        assert!(!stale.exists());
        assert!(recent.exists());
        assert!(unrelated.exists());
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

    #[test]
    fn newest_valid_snapshot_wins_concurrent_save_stress() {
        let store = test_store("concurrent-newest-wins");
        let generation = store.begin_owner_generation().unwrap();
        let mut newest = snapshot();
        let writes = (0..64)
            .map(|index| {
                let mut candidate = snapshot();
                candidate.workspaces[0].manual_name = Some(format!("snapshot-{index}"));
                newest = candidate.clone();
                store.prepare_save(&candidate, generation).unwrap()
            })
            .collect::<Vec<_>>();

        // Schedule requests in the opposite order from their logical creation
        // order, matching the detached-task race that used to corrupt state.
        let handles = writes
            .into_iter()
            .rev()
            .map(|write| {
                let store = store.clone();
                thread::spawn(move || store.commit(&write).unwrap())
            })
            .collect::<Vec<_>>();
        let outcomes = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == SessionWriteOutcome::Installed)
                .count(),
            1
        );
        assert_eq!(store.load().unwrap(), Some(newest));
    }

    #[test]
    fn failed_write_can_retry_without_a_new_request() {
        let store = test_store("retry");
        let generation = store.begin_owner_generation().unwrap();
        let parent = store.path().parent().unwrap();
        fs::write(parent, b"blocks directory creation").unwrap();
        let expected = snapshot();
        let write = store.prepare_save(&expected, generation).unwrap();

        assert!(store.commit(&write).is_err());
        fs::remove_file(parent).unwrap();
        assert_eq!(
            store.commit(&write).unwrap(),
            SessionWriteOutcome::Installed
        );
        assert_eq!(store.load().unwrap(), Some(expected));
    }

    #[test]
    fn write_sequences_start_nonzero_and_fail_closed_at_overflow() {
        let store = test_store("sequence-bounds");
        let generation = store.begin_owner_generation().unwrap();
        let first = store.prepare_save(&snapshot(), generation).unwrap();
        assert_eq!(first.sequence, 1);

        store
            .writer
            .next_sequence
            .store(u64::MAX, Ordering::Relaxed);
        assert!(store.prepare_save(&snapshot(), generation).is_err());
        assert_eq!(store.writer.next_sequence.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn stale_owner_generation_is_superseded_before_the_new_owner_writes() {
        let store = test_store("stale-owner-generation");
        let first_generation = store.begin_owner_generation().unwrap();
        let stale_snapshot = snapshot();
        let stale_write = store
            .prepare_save(&stale_snapshot, first_generation)
            .unwrap();

        let second_generation = store.begin_owner_generation().unwrap();
        assert_eq!(
            store.commit(&stale_write).unwrap(),
            SessionWriteOutcome::Superseded
        );
        assert_eq!(store.load().unwrap(), None);

        let mut current_snapshot = snapshot();
        current_snapshot.workspaces[0].manual_name = Some("new owner".into());
        let current_write = store
            .prepare_save(&current_snapshot, second_generation)
            .unwrap();
        assert_eq!(
            store.commit(&current_write).unwrap(),
            SessionWriteOutcome::Installed
        );
        assert_eq!(store.load().unwrap(), Some(current_snapshot));
    }

    #[test]
    fn poisoned_writer_lock_recovers_without_losing_snapshot() {
        let store = test_store("poisoned-writer");
        let writer = store.writer.clone();
        assert!(
            thread::spawn(move || {
                let _guard = writer.write_lock.lock().unwrap();
                panic!("poison session writer lock");
            })
            .join()
            .is_err()
        );
        assert!(store.writer.write_lock.is_poisoned());

        let expected = snapshot();
        assert_eq!(
            store.save(&expected).unwrap(),
            SessionWriteOutcome::Installed
        );
        assert_eq!(store.load().unwrap(), Some(expected));
    }
}
