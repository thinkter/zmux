//! Workspace metadata primitives.
//!
//! The sidebar is only a consumer of this module. Collectors and control-plane
//! callers write bounded, workspace-addressed values here; renderers receive a
//! cloneable snapshot and never run commands, inspect a terminal, or make a
//! platform assumption while painting UI.

use std::collections::{BTreeMap, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use gpui::{App, Global};
use serde::{Deserialize, Serialize};

use crate::notifications::WorkspaceId;

/// Upper bounds deliberately live at the storage boundary, not in the UI.
pub const DEFAULT_MAX_LOG_ENTRIES: usize = 100;
pub const MAX_STATUS_PILLS: usize = 16;
pub const MAX_PROGRESS_ENTRIES: usize = 16;
pub const MAX_STATUS_TEXT_BYTES: usize = 256;
pub const MAX_LOG_TEXT_BYTES: usize = 4 * 1024;
const MAX_METADATA_COMMAND_OUTPUT_BYTES: usize = 64 * 1024;

/// A value that can be unavailable on a platform without making a workspace
/// unusable. The states are suitable for a text-only renderer as well as a
/// visual badge.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum MetadataState<T> {
    #[default]
    NotRequested,
    Pending,
    Ready(T),
    Unavailable(String),
    Error(String),
}

impl<T> MetadataState<T> {
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Pending)
    }
}

/// Best-effort Git state, intentionally independent of Zed's project model.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitMetadata {
    pub branch: String,
    pub dirty_files: usize,
    pub ahead: usize,
    pub behind: usize,
}

impl GitMetadata {
    pub fn is_clean(&self) -> bool {
        self.dirty_files == 0
    }

    pub fn accessible_text(&self) -> String {
        let cleanliness = if self.is_clean() { "clean" } else { "modified" };
        let mut text = format!(
            "Git branch {}, {}, {} changed file{}",
            self.branch,
            cleanliness,
            self.dirty_files,
            if self.dirty_files == 1 { "" } else { "s" }
        );
        if self.ahead > 0 || self.behind > 0 {
            text.push_str(&format!(
                ", {} ahead and {} behind",
                self.ahead, self.behind
            ));
        }
        text
    }
}

/// A listener visible to the host. A collector must never claim that a port is
/// owned by a workspace unless it can prove the process attribution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListeningPort {
    pub protocol: String,
    pub address: String,
    pub port: u16,
}

impl ListeningPort {
    pub fn accessible_text(&self) -> String {
        format!(
            "{} listening on {}:{}",
            self.protocol, self.address, self.port
        )
    }
}

/// Scriptable coarse agent state. Vendor-specific adapters can map their own
/// lifecycle events to this without the terminal core knowing a vendor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentActivity {
    #[default]
    Unknown,
    Idle,
    Working,
    Waiting,
    Error,
}

impl AgentActivity {
    pub fn accessible_text(self) -> &'static str {
        match self {
            Self::Unknown => "Agent activity is unknown",
            Self::Idle => "Agent is idle",
            Self::Working => "Agent is working",
            Self::Waiting => "Agent is waiting for input",
            Self::Error => "Agent reported an error",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusTone {
    #[default]
    Neutral,
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusPill {
    pub label: String,
    pub detail: Option<String>,
    pub tone: StatusTone,
}

impl StatusPill {
    pub fn accessible_text(&self) -> String {
        match &self.detail {
            Some(detail) => format!("Status {}: {}", self.label, detail),
            None => format!("Status {}", self.label),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressValue {
    pub label: String,
    pub completed: u64,
    pub total: u64,
}

impl ProgressValue {
    pub fn percent(&self) -> u8 {
        ((self.completed.saturating_mul(100) / self.total).min(100)) as u8
    }

    pub fn accessible_text(&self) -> String {
        format!(
            "{}: {} of {} complete ({} percent)",
            self.label,
            self.completed,
            self.total,
            self.percent()
        )
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    #[default]
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceLogEntry {
    pub created_at: SystemTime,
    pub level: LogLevel,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationSummary {
    pub title: String,
    pub body: String,
}

/// The rendering-independent state for one workspace.
#[derive(Clone, Debug)]
pub struct WorkspaceMetadata {
    pub working_directory: Option<PathBuf>,
    pub git: MetadataState<GitMetadata>,
    pub listening_ports: MetadataState<Vec<ListeningPort>>,
    pub agent_activity: AgentActivity,
    pub unread_count: usize,
    pub latest_notification: Option<NotificationSummary>,
    pub status_pills: BTreeMap<String, StatusPill>,
    pub progress: BTreeMap<String, ProgressValue>,
    pub logs: VecDeque<WorkspaceLogEntry>,
    pub is_refreshing: bool,
    pub refreshed_at: Option<SystemTime>,
    last_refresh_started_at: Option<Instant>,
}

impl WorkspaceMetadata {
    pub fn new(working_directory: Option<PathBuf>) -> Self {
        Self {
            working_directory,
            git: MetadataState::NotRequested,
            listening_ports: MetadataState::NotRequested,
            agent_activity: AgentActivity::Unknown,
            unread_count: 0,
            latest_notification: None,
            status_pills: BTreeMap::new(),
            progress: BTreeMap::new(),
            logs: VecDeque::new(),
            is_refreshing: false,
            refreshed_at: None,
            last_refresh_started_at: None,
        }
    }

    /// Plain-language text for assistive technology and unsupported visual
    /// environments. It includes the same state rendered as sidebar badges.
    pub fn accessible_summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(directory) = &self.working_directory {
            parts.push(format!("Working directory {}", directory.display()));
        }
        match &self.git {
            MetadataState::Ready(git) => parts.push(git.accessible_text()),
            MetadataState::Pending => parts.push("Git status is refreshing".to_string()),
            MetadataState::Unavailable(reason) => {
                parts.push(format!("Git status unavailable: {reason}"))
            }
            MetadataState::Error(error) => parts.push(format!("Git status error: {error}")),
            MetadataState::NotRequested => {}
        }
        match &self.listening_ports {
            MetadataState::Ready(ports) if ports.is_empty() => {
                parts.push("No listening ports detected".to_string())
            }
            MetadataState::Ready(ports) => {
                parts.extend(ports.iter().map(ListeningPort::accessible_text))
            }
            MetadataState::Pending => parts.push("Listening ports are refreshing".to_string()),
            MetadataState::Unavailable(reason) => {
                parts.push(format!("Listening ports unavailable: {reason}"))
            }
            MetadataState::Error(error) => parts.push(format!("Listening ports error: {error}")),
            MetadataState::NotRequested => {}
        }
        parts.push(self.agent_activity.accessible_text().to_string());
        if self.unread_count > 0 {
            parts.push(format!(
                "{} unread notification{}",
                self.unread_count,
                if self.unread_count == 1 { "" } else { "s" }
            ));
        }
        parts.extend(self.status_pills.values().map(StatusPill::accessible_text));
        parts.extend(self.progress.values().map(ProgressValue::accessible_text));
        if !self.logs.is_empty() {
            parts.push(format!(
                "{} retained workspace log entr{}",
                self.logs.len(),
                if self.logs.len() == 1 { "y" } else { "ies" }
            ));
        }
        parts.join(". ")
    }
}

/// A cancellation token held by the refresh store and checked by collectors.
/// Cancellation is cooperative and bounded by the collector command timeout.
#[derive(Clone, Debug)]
pub struct RefreshCancellation(Arc<AtomicBool>);

impl RefreshCancellation {
    fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Input to a background collector. It contains no GPUI entities and can move
/// to a worker thread safely.
#[derive(Clone, Debug)]
pub struct MetadataRefreshRequest {
    pub workspace_id: WorkspaceId,
    pub working_directory: PathBuf,
    pub generation: u64,
    pub cancellation: RefreshCancellation,
}

#[derive(Clone, Debug)]
pub struct CollectedWorkspaceMetadata {
    pub git: MetadataState<GitMetadata>,
    pub listening_ports: MetadataState<Vec<ListeningPort>>,
}

impl CollectedWorkspaceMetadata {
    pub fn cancelled() -> Self {
        Self {
            git: MetadataState::Unavailable("refresh cancelled".to_string()),
            listening_ports: MetadataState::Unavailable("refresh cancelled".to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataError(String);

impl MetadataError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for MetadataError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for MetadataError {}

struct ActiveRefresh {
    generation: u64,
    cancellation: RefreshCancellation,
}

/// App-global, bounded metadata store. This is the bridge a future
/// transport-independent control API can call without depending on the
/// sidebar implementation.
pub struct WorkspaceMetadataStore {
    workspaces: BTreeMap<WorkspaceId, WorkspaceMetadata>,
    active_refreshes: BTreeMap<WorkspaceId, ActiveRefresh>,
    max_log_entries: usize,
    refresh_interval: Duration,
    next_generation: u64,
}

impl Global for WorkspaceMetadataStore {}

impl Default for WorkspaceMetadataStore {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_LOG_ENTRIES, Duration::from_secs(5))
    }
}

impl WorkspaceMetadataStore {
    pub fn new(max_log_entries: usize, refresh_interval: Duration) -> Self {
        Self {
            workspaces: BTreeMap::new(),
            active_refreshes: BTreeMap::new(),
            max_log_entries: max_log_entries.clamp(1, 1_000),
            refresh_interval: refresh_interval.max(Duration::from_secs(1)),
            next_generation: 1,
        }
    }

    pub fn global(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    pub fn global_mut(cx: &mut App) -> &mut Self {
        cx.global_mut::<Self>()
    }

    pub fn register_workspace(&mut self, id: WorkspaceId, working_directory: Option<PathBuf>) {
        self.workspaces
            .entry(id)
            .or_insert_with(|| WorkspaceMetadata::new(working_directory));
    }

    pub fn remove_workspace(&mut self, id: WorkspaceId) {
        if let Some(refresh) = self.active_refreshes.remove(&id) {
            refresh.cancellation.cancel();
        }
        self.workspaces.remove(&id);
    }

    pub fn metadata(&self, id: WorkspaceId) -> Option<&WorkspaceMetadata> {
        self.workspaces.get(&id)
    }

    pub fn snapshot(&self, id: WorkspaceId) -> Option<WorkspaceMetadata> {
        self.workspaces.get(&id).cloned()
    }

    pub fn configure(&mut self, max_log_entries: usize, refresh_interval: Duration) {
        self.max_log_entries = max_log_entries.clamp(1, 1_000);
        self.refresh_interval = refresh_interval.max(Duration::from_secs(1));
        for metadata in self.workspaces.values_mut() {
            while metadata.logs.len() > self.max_log_entries {
                metadata.logs.pop_front();
            }
        }
    }

    pub fn set_working_directory(
        &mut self,
        id: WorkspaceId,
        working_directory: Option<PathBuf>,
    ) -> Result<(), MetadataError> {
        self.require_workspace_mut(id)?.working_directory = working_directory;
        Ok(())
    }

    /// Starts a bounded refresh if due. An existing refresh for this workspace
    /// is cancelled when `force` is true; stale completions are ignored by
    /// [`finish_refresh`](Self::finish_refresh).
    pub fn begin_refresh(
        &mut self,
        id: WorkspaceId,
        force: bool,
    ) -> Result<Option<MetadataRefreshRequest>, MetadataError> {
        let refresh_interval = self.refresh_interval;
        let working_directory = {
            let workspace = self.require_workspace_mut(id)?;
            let due = force
                || workspace
                    .last_refresh_started_at
                    .is_none_or(|last| last.elapsed() >= refresh_interval);
            if !due || (workspace.is_refreshing && !force) {
                return Ok(None);
            }
            let Some(working_directory) = workspace.working_directory.clone() else {
                workspace.git = MetadataState::Unavailable("no working directory".to_string());
                workspace.listening_ports =
                    MetadataState::Unavailable("no working directory".to_string());
                return Ok(None);
            };
            working_directory
        };

        if let Some(previous) = self.active_refreshes.remove(&id) {
            previous.cancellation.cancel();
        }
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        let cancellation = RefreshCancellation::new();
        self.active_refreshes.insert(
            id,
            ActiveRefresh {
                generation,
                cancellation: cancellation.clone(),
            },
        );
        let workspace = self.require_workspace_mut(id)?;
        workspace.git = MetadataState::Pending;
        workspace.listening_ports = MetadataState::Pending;
        workspace.is_refreshing = true;
        workspace.last_refresh_started_at = Some(Instant::now());
        Ok(Some(MetadataRefreshRequest {
            workspace_id: id,
            working_directory,
            generation,
            cancellation,
        }))
    }

    /// Applies only the latest non-cancelled collection result, making rapid
    /// workspace switches safe even when a slow Git command completes late.
    pub fn finish_refresh(
        &mut self,
        request: &MetadataRefreshRequest,
        result: CollectedWorkspaceMetadata,
    ) -> bool {
        let Some(active) = self.active_refreshes.get(&request.workspace_id) else {
            return false;
        };
        if active.generation != request.generation || active.cancellation.is_cancelled() {
            return false;
        }
        self.active_refreshes.remove(&request.workspace_id);
        let Some(workspace) = self.workspaces.get_mut(&request.workspace_id) else {
            return false;
        };
        workspace.git = result.git;
        workspace.listening_ports = result.listening_ports;
        workspace.is_refreshing = false;
        workspace.refreshed_at = Some(SystemTime::now());
        true
    }

    pub fn set_agent_activity(
        &mut self,
        id: WorkspaceId,
        activity: AgentActivity,
    ) -> Result<(), MetadataError> {
        self.require_workspace_mut(id)?.agent_activity = activity;
        Ok(())
    }

    pub fn set_notification_summary(
        &mut self,
        id: WorkspaceId,
        unread_count: usize,
        latest: Option<NotificationSummary>,
    ) -> Result<(), MetadataError> {
        let workspace = self.require_workspace_mut(id)?;
        workspace.unread_count = unread_count;
        workspace.latest_notification = latest.map(|summary| NotificationSummary {
            title: truncate_text(summary.title, MAX_STATUS_TEXT_BYTES),
            body: truncate_text(summary.body, MAX_STATUS_TEXT_BYTES),
        });
        Ok(())
    }

    pub fn set_status_pill(
        &mut self,
        id: WorkspaceId,
        key: impl Into<String>,
        pill: StatusPill,
    ) -> Result<(), MetadataError> {
        let key = validate_key(key.into())?;
        let workspace = self.require_workspace_mut(id)?;
        if !workspace.status_pills.contains_key(&key)
            && workspace.status_pills.len() >= MAX_STATUS_PILLS
        {
            return Err(MetadataError::new(format!(
                "workspace {id} already has the maximum of {MAX_STATUS_PILLS} status pills"
            )));
        }
        workspace.status_pills.insert(
            key,
            StatusPill {
                label: truncate_text(pill.label, MAX_STATUS_TEXT_BYTES),
                detail: pill
                    .detail
                    .map(|detail| truncate_text(detail, MAX_STATUS_TEXT_BYTES)),
                tone: pill.tone,
            },
        );
        Ok(())
    }

    pub fn clear_status_pill(&mut self, id: WorkspaceId, key: &str) -> Result<(), MetadataError> {
        self.require_workspace_mut(id)?.status_pills.remove(key);
        Ok(())
    }

    pub fn set_progress(
        &mut self,
        id: WorkspaceId,
        key: impl Into<String>,
        progress: ProgressValue,
    ) -> Result<(), MetadataError> {
        let key = validate_key(key.into())?;
        if progress.total == 0 || progress.completed > progress.total {
            return Err(MetadataError::new(
                "progress total must be non-zero and completed cannot exceed total",
            ));
        }
        let workspace = self.require_workspace_mut(id)?;
        if !workspace.progress.contains_key(&key)
            && workspace.progress.len() >= MAX_PROGRESS_ENTRIES
        {
            return Err(MetadataError::new(format!(
                "workspace {id} already has the maximum of {MAX_PROGRESS_ENTRIES} progress values"
            )));
        }
        workspace.progress.insert(
            key,
            ProgressValue {
                label: truncate_text(progress.label, MAX_STATUS_TEXT_BYTES),
                completed: progress.completed,
                total: progress.total,
            },
        );
        Ok(())
    }

    pub fn clear_progress(&mut self, id: WorkspaceId, key: &str) -> Result<(), MetadataError> {
        self.require_workspace_mut(id)?.progress.remove(key);
        Ok(())
    }

    pub fn append_log(
        &mut self,
        id: WorkspaceId,
        level: LogLevel,
        message: impl Into<String>,
    ) -> Result<(), MetadataError> {
        let max_log_entries = self.max_log_entries;
        let workspace = self.require_workspace_mut(id)?;
        workspace.logs.push_back(WorkspaceLogEntry {
            created_at: SystemTime::now(),
            level,
            message: truncate_text(message.into(), MAX_LOG_TEXT_BYTES),
        });
        while workspace.logs.len() > max_log_entries {
            workspace.logs.pop_front();
        }
        Ok(())
    }

    fn require_workspace_mut(
        &mut self,
        id: WorkspaceId,
    ) -> Result<&mut WorkspaceMetadata, MetadataError> {
        self.workspaces
            .get_mut(&id)
            .ok_or_else(|| MetadataError::new(format!("unknown workspace {id}")))
    }
}

/// The transport-independent update vocabulary intended for the future control
/// API. It carries immutable workspace IDs and contains no UI references.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum MetadataUpdate {
    SetAgentActivity(AgentActivity),
    SetStatusPill {
        key: String,
        pill: StatusPill,
    },
    ClearStatusPill {
        key: String,
    },
    SetProgress {
        key: String,
        progress: ProgressValue,
    },
    ClearProgress {
        key: String,
    },
    AppendLog {
        level: LogLevel,
        message: String,
    },
}

impl MetadataUpdate {
    /// Validate the fields that are meaningful independently of a particular
    /// store. Store-specific limits (such as the number of active pills) are
    /// still returned by the target store as typed failures.
    pub fn validate(&self) -> Result<(), MetadataError> {
        match self {
            Self::SetAgentActivity(_) | Self::AppendLog { .. } => Ok(()),
            Self::SetStatusPill { key, .. }
            | Self::ClearStatusPill { key }
            | Self::ClearProgress { key } => {
                validate_key(key.clone())?;
                Ok(())
            }
            Self::SetProgress { key, progress } => {
                validate_key(key.clone())?;
                if progress.total == 0 || progress.completed > progress.total {
                    return Err(MetadataError::new(
                        "progress total must be non-zero and completed cannot exceed total",
                    ));
                }
                Ok(())
            }
        }
    }
}

impl WorkspaceMetadataStore {
    pub fn apply_update(
        &mut self,
        workspace_id: WorkspaceId,
        update: MetadataUpdate,
    ) -> Result<(), MetadataError> {
        update.validate()?;
        match update {
            MetadataUpdate::SetAgentActivity(activity) => {
                self.set_agent_activity(workspace_id, activity)
            }
            MetadataUpdate::SetStatusPill { key, pill } => {
                self.set_status_pill(workspace_id, key, pill)
            }
            MetadataUpdate::ClearStatusPill { key } => self.clear_status_pill(workspace_id, &key),
            MetadataUpdate::SetProgress { key, progress } => {
                self.set_progress(workspace_id, key, progress)
            }
            MetadataUpdate::ClearProgress { key } => self.clear_progress(workspace_id, &key),
            MetadataUpdate::AppendLog { level, message } => {
                self.append_log(workspace_id, level, message)
            }
        }
    }
}

/// Collect a workspace snapshot using only portable command execution. It has
/// an explicit cancellation token and short timeouts; platform-specific
/// collectors can be layered in later without changing sidebar/store code.
pub fn collect_system_metadata(request: MetadataRefreshRequest) -> CollectedWorkspaceMetadata {
    if request.cancellation.is_cancelled() {
        return CollectedWorkspaceMetadata::cancelled();
    }

    let git = collect_git_metadata(&request.working_directory, &request.cancellation);
    let listening_ports =
        collect_listening_ports(&request.working_directory, &request.cancellation);
    CollectedWorkspaceMetadata {
        git,
        listening_ports,
    }
}

fn collect_git_metadata(
    working_directory: &Path,
    cancellation: &RefreshCancellation,
) -> MetadataState<GitMetadata> {
    match run_bounded_command(
        "git",
        &[
            "status",
            "--porcelain=v1",
            "--branch",
            "--untracked-files=normal",
        ],
        working_directory,
        cancellation,
        Duration::from_millis(750),
    ) {
        Ok(output) => parse_git_porcelain(&output)
            .map(MetadataState::Ready)
            .unwrap_or_else(MetadataState::Error),
        Err(CommandFailure::Cancelled) => {
            MetadataState::Unavailable("refresh cancelled".to_string())
        }
        Err(CommandFailure::TimedOut) => MetadataState::Error("git status timed out".to_string()),
        Err(CommandFailure::Spawn(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            MetadataState::Unavailable("git is not installed".to_string())
        }
        Err(CommandFailure::Spawn(error)) => {
            MetadataState::Error(format!("could not start git: {error}"))
        }
        Err(CommandFailure::Exit(status)) => {
            MetadataState::Unavailable(format!("not a Git repository ({status})"))
        }
        Err(CommandFailure::Read(error)) => {
            MetadataState::Error(format!("could not read git status: {error}"))
        }
    }
}

fn collect_listening_ports(
    _working_directory: &Path,
    cancellation: &RefreshCancellation,
) -> MetadataState<Vec<ListeningPort>> {
    if cancellation.is_cancelled() {
        return MetadataState::Unavailable("refresh cancelled".to_string());
    }

    // `ss -ltn` describes the entire host, not the terminal process tree for
    // this workspace. Reporting those listeners in every row would falsely
    // attribute unrelated services to a workspace. Keep the capability
    // explicit-but-unavailable until a collector can prove ownership.
    MetadataState::Unavailable(
        "workspace-owned listener discovery requires process attribution".to_string(),
    )
}

#[derive(Debug)]
enum CommandFailure {
    Cancelled,
    TimedOut,
    Spawn(std::io::Error),
    Exit(std::process::ExitStatus),
    Read(std::io::Error),
}

fn run_bounded_command(
    program: &str,
    arguments: &[&str],
    working_directory: &Path,
    cancellation: &RefreshCancellation,
    timeout: Duration,
) -> Result<String, CommandFailure> {
    let mut child = Command::new(program)
        .args(arguments)
        .current_dir(working_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(CommandFailure::Spawn)?;
    let stdout = child
        .stdout
        .take()
        .expect("stdout is piped immediately before spawning the reader");
    let stdout_reader = thread::spawn(move || drain_bounded_stdout(stdout));
    let started_at = Instant::now();

    loop {
        if cancellation.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            return Err(CommandFailure::Cancelled);
        }
        if started_at.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            return Err(CommandFailure::TimedOut);
        }
        let status = match child.try_wait() {
            Ok(status) => status,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                return Err(CommandFailure::Read(error));
            }
        };
        match status {
            Some(status) if status.success() => {
                return stdout_reader
                    .join()
                    .map_err(|_| {
                        CommandFailure::Read(std::io::Error::other(
                            "metadata stdout reader panicked",
                        ))
                    })?
                    .map_err(CommandFailure::Read);
            }
            Some(status) => {
                let _ = stdout_reader.join();
                return Err(CommandFailure::Exit(status));
            }
            None => thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// Drain child stdout concurrently with process polling. Keeping only a
/// bounded result while continuing to read avoids a child blocking forever on
/// a full OS pipe in a large repository or on a busy host.
fn drain_bounded_stdout(mut pipe: impl Read) -> Result<String, std::io::Error> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 8 * 1024];
    let mut exceeded_limit = false;

    loop {
        let read = pipe.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let retained = read.min(MAX_METADATA_COMMAND_OUTPUT_BYTES.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&buffer[..retained]);
        exceeded_limit |= retained < read;
    }

    if exceeded_limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("metadata command output exceeded {MAX_METADATA_COMMAND_OUTPUT_BYTES} bytes"),
        ));
    }

    String::from_utf8(bytes).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("metadata command output was not UTF-8: {error}"),
        )
    })
}

fn parse_git_porcelain(output: &str) -> Result<GitMetadata, String> {
    let mut metadata = GitMetadata {
        branch: "(detached)".to_string(),
        ..Default::default()
    };
    for line in output.lines() {
        if let Some(branch) = line.strip_prefix("# branch.head ") {
            metadata.branch = branch.to_string();
        } else if let Some(divergence) = line.strip_prefix("# branch.ab ") {
            for part in divergence.split_whitespace() {
                if let Some(value) = part.strip_prefix('+') {
                    metadata.ahead = value
                        .parse()
                        .map_err(|_| format!("invalid ahead count in git status: {part}"))?;
                } else if let Some(value) = part.strip_prefix('-') {
                    metadata.behind = value
                        .parse()
                        .map_err(|_| format!("invalid behind count in git status: {part}"))?;
                }
            }
        } else if !line.starts_with("# ") && !line.is_empty() {
            metadata.dirty_files += 1;
        }
    }
    Ok(metadata)
}

fn validate_key(key: String) -> Result<String, MetadataError> {
    if key.is_empty()
        || key.len() > 64
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(MetadataError::new(
            "metadata keys must be 1-64 ASCII letters, numbers, '.', '_' or '-'",
        ));
    }
    Ok(key)
}

fn truncate_text(mut value: String, maximum_bytes: usize) -> String {
    if value.len() <= maximum_bytes {
        return value;
    }
    let mut index = maximum_bytes;
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    value.truncate(index);
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_progress_and_logs_are_workspace_addressed_and_bounded() {
        let mut store = WorkspaceMetadataStore::new(2, Duration::from_secs(1));
        store.register_workspace(7, Some(PathBuf::from("/tmp/project")));
        store
            .apply_update(
                7,
                MetadataUpdate::SetStatusPill {
                    key: "build".to_string(),
                    pill: StatusPill {
                        label: "Build".to_string(),
                        detail: Some("running".to_string()),
                        tone: StatusTone::Info,
                    },
                },
            )
            .unwrap();
        store
            .apply_update(
                7,
                MetadataUpdate::SetProgress {
                    key: "build".to_string(),
                    progress: ProgressValue {
                        label: "Build".to_string(),
                        completed: 4,
                        total: 10,
                    },
                },
            )
            .unwrap();
        for message in ["first", "second", "third"] {
            store
                .apply_update(
                    7,
                    MetadataUpdate::AppendLog {
                        level: LogLevel::Info,
                        message: message.to_string(),
                    },
                )
                .unwrap();
        }

        let snapshot = store.snapshot(7).unwrap();
        assert_eq!(snapshot.status_pills["build"].label, "Build");
        assert_eq!(snapshot.progress["build"].percent(), 40);
        assert_eq!(snapshot.logs.len(), 2);
        assert_eq!(snapshot.logs.front().unwrap().message, "second");
        assert!(
            snapshot
                .accessible_summary()
                .contains("Build: 4 of 10 complete")
        );
    }

    #[test]
    fn stale_or_cancelled_refreshes_cannot_overwrite_newer_state() {
        let mut store = WorkspaceMetadataStore::default();
        store.register_workspace(1, Some(PathBuf::from("/tmp/project")));
        let first = store.begin_refresh(1, true).unwrap().unwrap();
        let second = store.begin_refresh(1, true).unwrap().unwrap();
        assert!(first.cancellation.is_cancelled());

        let result = CollectedWorkspaceMetadata {
            git: MetadataState::Ready(GitMetadata {
                branch: "main".to_string(),
                ..Default::default()
            }),
            listening_ports: MetadataState::Ready(Vec::new()),
        };
        assert!(!store.finish_refresh(&first, result.clone()));
        assert!(store.finish_refresh(&second, result));
        assert_eq!(
            store.snapshot(1).unwrap().git,
            MetadataState::Ready(GitMetadata {
                branch: "main".to_string(),
                ..Default::default()
            })
        );
    }

    #[test]
    fn git_porcelain_parser_is_deterministic() {
        let parsed = parse_git_porcelain(
            "# branch.oid abc\n# branch.head feature/meta\n# branch.upstream origin/feature/meta\n# branch.ab +2 -1\n M src/lib.rs\n?? tests/new.rs\n",
        )
        .unwrap();
        assert_eq!(parsed.branch, "feature/meta");
        assert_eq!(parsed.ahead, 2);
        assert_eq!(parsed.behind, 1);
        assert_eq!(parsed.dirty_files, 2);
    }

    #[test]
    fn listener_discovery_does_not_misattribute_host_ports_to_a_workspace() {
        let state = collect_listening_ports(Path::new("/tmp/project"), &RefreshCancellation::new());
        assert!(matches!(
            state,
            MetadataState::Unavailable(reason) if reason.contains("process attribution")
        ));
    }

    #[test]
    fn command_stdout_is_drained_while_retention_stays_bounded() {
        assert_eq!(
            drain_bounded_stdout(std::io::Cursor::new(b"ok".to_vec())).unwrap(),
            "ok"
        );

        let error = drain_bounded_stdout(std::io::Cursor::new(vec![
            b'x';
            MAX_METADATA_COMMAND_OUTPUT_BYTES
                + 1
        ]))
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn invalid_scriptable_keys_and_progress_are_rejected() {
        let mut store = WorkspaceMetadataStore::default();
        store.register_workspace(1, None);
        assert!(
            store
                .set_status_pill(
                    1,
                    "not valid",
                    StatusPill {
                        label: "x".to_string(),
                        detail: None,
                        tone: StatusTone::Neutral,
                    },
                )
                .is_err()
        );
        assert!(
            store
                .set_progress(
                    1,
                    "job",
                    ProgressValue {
                        label: "Job".to_string(),
                        completed: 1,
                        total: 0,
                    },
                )
                .is_err()
        );
    }
}
