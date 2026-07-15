//! Workspaces: a left sidebar that lets you keep several independent terminal
//! layouts alive at once and switch between them instantly.
//!
//! Each workspace owns its open terminal tabs *and* their split layout. The
//! active workspace lives directly in the [`Workspace`] center; inactive
//! workspaces are detached and parked in [`StoredLayout`] values that keep the
//! terminal entities (and therefore their PTYs) alive. Switching is a pure
//! detach/reattach of live entities — no PTY restart, no serialization — so it
//! stays snappy regardless of how many terminals are open.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use editor::{Editor, EditorEvent};
use gpui::{
    Anchor, App, Axis, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable, FontWeight,
    Global, IntoElement, KeyDownEvent, Pixels, Render, SharedString, Subscription, Task, TaskExt,
    WeakEntity, Window, actions, div, px,
};
use terminal_view::TerminalView;
use ui::prelude::*;
use ui::{Button, ButtonSize, IconButtonShape, Indicator, PopoverMenu, Tooltip};
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::item::ItemHandle;
use workspace::{Member, Pane, SplitDirection, Workspace};

use crate::app::{
    create_center_terminal_at_for_workspace, create_center_terminal_for_workspace,
    create_restored_terminals_for_workspace,
};
use crate::metadata::{GitMetadata, MetadataState, collect_git_metadata};
use crate::notification_runtime::NotificationRuntime;
use crate::notifications::{Notification, NotificationStore, WorkspaceId};
use crate::session::{
    LayoutAxis, LayoutNodeSnapshot, LayoutSnapshot, SESSION_VERSION, SessionSnapshot, SessionStore,
    SessionWriteOutcome, TerminalSnapshot, WorkspaceSnapshot,
};
use crate::welcome::ZmuxWelcome;

actions!(
    zmux,
    [
        NewWorkspace,
        ToggleWorkspacesPanel,
        ToggleNotificationCenter,
        ActivateNextWorkspace,
        ActivatePreviousWorkspace
    ]
);

const PANEL_WIDTH_REMS: f32 = 15.0;
const PANEL_MIN_WIDTH_REMS: f32 = 12.0;
const WORKSPACES_FONT_FAMILY: &str = "Lilex";
const NOTIFICATION_DRAWER_HEIGHT_REMS: f32 = 17.5;
const MAX_WORKSPACE_NAME_CHARS: usize = 64;
const CONTEXT_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const MAX_INCOMPLETE_CONTEXT_REFRESHES: u8 = 3;
const ACTIVE_METADATA_INTERVAL: Duration = Duration::from_secs(5);
const INACTIVE_METADATA_INTERVAL: Duration = Duration::from_secs(30);

fn scaled_panel_size(rem_size: Pixels, rems: f32) -> Pixels {
    px(f32::from(rem_size) * rems)
}

/// A detached snapshot of a workspace's center: the split tree plus the live
/// terminal item handles, which keep the underlying terminals running while the
/// workspace is in the background.
enum StoredLayout {
    Leaf {
        items: Vec<Box<dyn ItemHandle>>,
        active: usize,
        focused: bool,
    },
    Split {
        axis: Axis,
        ratio: f32,
        first: Box<StoredLayout>,
        second: Box<StoredLayout>,
    },
}

struct WorkspaceEntry {
    id: WorkspaceId,
    manual_name: Option<String>,
    worktree_name: Option<String>,
    worktree_paths: Vec<PathBuf>,
    automatic_name: String,
    context: WorkspaceContext,
    context_authoritative: bool,
    incomplete_context_refreshes: u8,
    default_directory: Option<PathBuf>,
    selected_git_root: Option<PathBuf>,
    git_discovery: GitDiscoveryState,
    git: MetadataState<GitMetadata>,
    metadata_root: Option<PathBuf>,
    metadata_refreshed_at: Option<Instant>,
    /// The complete persisted layout, retained until every fresh terminal has
    /// materialized so an interrupted restore can retry without losing tabs.
    restore: Option<LayoutSnapshot>,
    failed_restores: Vec<FailedRestoreSlot>,
    /// `Some` while the workspace is parked in the background, `None` while it is
    /// the active workspace displayed in the center.
    stored: Option<StoredLayout>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GitDiscoveryState {
    /// A persisted layout has not finished recreating all of its terminals, so
    /// an empty or partial context cannot disprove the persisted selection.
    Restoring,
    /// Every restored terminal view is mounted, but one or more shells have
    /// not reported the working directory needed for repository discovery.
    Discovering,
    /// Every currently owned terminal contributes to repository discovery.
    Authoritative,
}

#[derive(Default)]
struct SessionOwnerClaimed(bool);

impl Global for SessionOwnerClaimed {}

pub(crate) struct RestoredTerminal {
    pub(crate) pane: Entity<Pane>,
    pub(crate) working_directory: Option<PathBuf>,
    pub(crate) activate: bool,
    pub(crate) failed_slot: FailedRestoreSlot,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FailedRestoreSlot {
    path: Vec<bool>,
    tab_index: usize,
    terminal: TerminalSnapshot,
    activate: bool,
}

/// Axis-aligned rectangle in the unit square describing a pane's share of the
/// center while a layout is being rebuilt.
#[derive(Clone, Copy, Debug, PartialEq)]
struct UnitRect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl UnitRect {
    const FULL: Self = Self {
        x: 0.0,
        y: 0.0,
        w: 1.0,
        h: 1.0,
    };

    fn split(self, axis: Axis, ratio: f32) -> (Self, Self) {
        let ratio = ratio.clamp(0.001, 0.999);
        match axis {
            Axis::Horizontal => {
                let first_w = self.w * ratio;
                (
                    Self { w: first_w, ..self },
                    Self {
                        x: self.x + first_w,
                        w: self.w - first_w,
                        ..self
                    },
                )
            }
            Axis::Vertical => {
                let first_h = self.h * ratio;
                (
                    Self { h: first_h, ..self },
                    Self {
                        y: self.y + first_h,
                        h: self.h - first_h,
                        ..self
                    },
                )
            }
        }
    }

    fn span(&self, axis: Axis) -> f32 {
        match axis {
            Axis::Horizontal => self.w,
            Axis::Vertical => self.h,
        }
    }

    fn union(self, other: Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = (self.x + self.w).max(other.x + other.w);
        let bottom = (self.y + self.h).max(other.y + other.h);
        Self {
            x,
            y,
            w: right - x,
            h: bottom - y,
        }
    }
}

impl WorkspaceEntry {
    fn display_name(&self) -> &str {
        self.manual_name
            .as_deref()
            .unwrap_or(self.automatic_name.as_str())
    }

    fn observe_context(&mut self, observed: WorkspaceContext) {
        if observed.is_complete() {
            self.context = observed;
            self.context_authoritative = true;
            self.incomplete_context_refreshes = 0;
            return;
        }

        self.incomplete_context_refreshes = self
            .incomplete_context_refreshes
            .saturating_add(1)
            .min(MAX_INCOMPLETE_CONTEXT_REFRESHES);
        self.context_authoritative =
            self.incomplete_context_refreshes >= MAX_INCOMPLETE_CONTEXT_REFRESHES;
        if self.context_authoritative {
            // A cwd probe can remain None indefinitely. After a bounded grace
            // period, accept the observable subset so session persistence and
            // worktree cleanup cannot be frozen forever.
            self.context = observed;
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct WorkspaceContext {
    working_directories: Vec<PathBuf>,
    git_roots: Vec<PathBuf>,
    git_root: Option<PathBuf>,
    foreground_processes: Vec<String>,
    shell_count: usize,
    reported_directories: usize,
}

impl WorkspaceContext {
    /// Whether every live shell has reported a working directory. A shell's
    /// directory probe can transiently fail, and the Git roots derived from
    /// such a pass understate the workspace; they must not tear down state.
    fn is_complete(&self) -> bool {
        self.reported_directories == self.shell_count
    }
}

#[derive(Debug, PartialEq, Eq)]
struct GitRootReconciliation {
    added: BTreeSet<PathBuf>,
    removed: BTreeSet<PathBuf>,
}

fn git_root_reference_counts<'a>(
    roots_by_workspace: impl IntoIterator<Item = &'a [PathBuf]>,
) -> BTreeMap<PathBuf, usize> {
    let mut counts = BTreeMap::new();
    for roots in roots_by_workspace {
        for root in roots {
            *counts.entry(root.clone()).or_insert(0) += 1;
        }
    }
    counts
}

fn git_root_is_referenced<'a>(
    roots_by_workspace: impl IntoIterator<Item = &'a [PathBuf]>,
    root: &Path,
) -> bool {
    roots_by_workspace
        .into_iter()
        .any(|roots| roots.iter().any(|candidate| candidate == root))
}

fn plan_git_root_reconciliation(
    reference_counts: BTreeMap<PathBuf, usize>,
    attached: &BTreeSet<PathBuf>,
    pending: &BTreeSet<PathBuf>,
) -> GitRootReconciliation {
    let added = reference_counts
        .keys()
        .filter(|root| !attached.contains(*root) && !pending.contains(*root))
        .cloned()
        .collect();
    let removed = attached
        .iter()
        .filter(|root| !reference_counts.contains_key(*root))
        .cloned()
        .collect();
    GitRootReconciliation { added, removed }
}

fn reconcile_selected_git_root(
    selected: &mut Option<PathBuf>,
    discovered_roots: &[PathBuf],
    discovery: GitDiscoveryState,
) {
    if discovery != GitDiscoveryState::Authoritative {
        return;
    }
    if selected
        .as_ref()
        .is_none_or(|root| !discovered_roots.contains(root))
    {
        *selected = discovered_roots.first().cloned();
    }
}

fn git_contexts_are_authoritative<'a>(
    entries: impl IntoIterator<Item = &'a WorkspaceEntry>,
) -> bool {
    entries.into_iter().all(|entry| entry.context_authoritative)
}

fn retain_completed_worktree_scan(root_is_referenced: bool, contexts_authoritative: bool) -> bool {
    root_is_referenced || !contexts_authoritative
}

fn track_pending_worktree(pending: &mut BTreeSet<PathBuf>, root: PathBuf) -> bool {
    pending.insert(root)
}

fn overlay_failed_restores(layout: &mut LayoutSnapshot, failed: &[FailedRestoreSlot]) {
    fn insert(node: &mut LayoutNodeSnapshot, path: &[bool], slot: &FailedRestoreSlot) -> bool {
        if let Some((&second, remaining)) = path.split_first() {
            let LayoutNodeSnapshot::Split {
                first, second: rhs, ..
            } = node
            else {
                return false;
            };
            return insert(if second { rhs } else { first }, remaining, slot);
        }
        let LayoutNodeSnapshot::Leaf {
            tabs, active_tab, ..
        } = node
        else {
            return false;
        };
        let index = slot.tab_index.min(tabs.len());
        tabs.insert(index, slot.terminal.clone());
        if slot.activate {
            *active_tab = index;
        } else if index <= *active_tab && tabs.len() > 1 {
            *active_tab += 1;
        }
        true
    }

    fn insert_first(node: &mut LayoutNodeSnapshot, slot: &FailedRestoreSlot) {
        match node {
            LayoutNodeSnapshot::Leaf { tabs, .. } => tabs.push(slot.terminal.clone()),
            LayoutNodeSnapshot::Split { first, .. } => insert_first(first, slot),
        }
    }

    let mut failed = failed.to_vec();
    failed.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.tab_index.cmp(&right.tab_index))
    });
    for slot in &failed {
        if !insert(&mut layout.root, &slot.path, slot) {
            // User layout edits during bounded retries may invalidate the old
            // split path. Preserve the terminal in the first live pane rather
            // than silently dropping it from the next session.
            insert_first(&mut layout.root, slot);
        }
    }
}

#[derive(Clone)]
struct WorkspaceRow {
    id: WorkspaceId,
    name: String,
    uses_manual_name: bool,
    context: WorkspaceContext,
    git: MetadataState<GitMetadata>,
}

#[derive(Clone)]
pub(crate) struct WorkspaceSwitcherEntry {
    pub(crate) id: WorkspaceId,
    pub(crate) name: String,
    pub(crate) detail: String,
    pub(crate) unread_count: usize,
}

struct RenameState {
    id: WorkspaceId,
    editor: Entity<Editor>,
    _subscription: Subscription,
}

#[derive(Clone)]
struct DraggedWorkspace {
    id: WorkspaceId,
    name: String,
    ix: usize,
}

impl Render for DraggedWorkspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .font_family(WORKSPACES_FONT_FAMILY)
            .px_2()
            .py_1()
            .gap_2()
            .rounded_md()
            .shadow_md()
            .bg(cx.theme().colors().element_selected)
            .child(Icon::new(IconName::Terminal).size(IconSize::Small))
            .child(
                Label::new(self.name.clone())
                    .size(LabelSize::Default)
                    .weight(FontWeight::BOLD),
            )
    }
}

pub struct WorkspacesPanel {
    scope_id: EntityId,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    entries: Vec<WorkspaceEntry>,
    active: WorkspaceId,
    activation_generation: u64,
    next_id: WorkspaceId,
    rename: Option<RenameState>,
    notifications_expanded: bool,
    notification_filter: Option<WorkspaceId>,
    _notification_subscription: Subscription,
    _context_refresh_task: Task<()>,
    session_store: SessionStore,
    owns_session: bool,
    session_persistence: SessionPersistence,
    attached_worktrees: HashMap<PathBuf, Entity<project::Worktree>>,
    pending_worktrees: BTreeSet<PathBuf>,
}

#[derive(Debug)]
struct SessionPersistence {
    persisted: Option<SessionSnapshot>,
    desired: Option<SessionSnapshot>,
    in_flight: Option<SessionSnapshot>,
}

impl SessionPersistence {
    fn new(restored: Option<SessionSnapshot>) -> Self {
        Self {
            persisted: restored.clone(),
            desired: restored,
            in_flight: None,
        }
    }

    fn request(&mut self, snapshot: SessionSnapshot) {
        if self.desired.as_ref() != Some(&snapshot) {
            self.desired = Some(snapshot);
        }
    }

    fn start_next(&mut self) -> Option<SessionSnapshot> {
        if self.in_flight.is_some() {
            return None;
        }
        let desired = self.desired.clone()?;
        if self.persisted.as_ref() == Some(&desired) {
            return None;
        }
        self.in_flight = Some(desired.clone());
        Some(desired)
    }

    fn complete(&mut self, snapshot: &SessionSnapshot, installed: bool) -> bool {
        if self.in_flight.as_ref() != Some(snapshot) {
            return false;
        }
        self.in_flight = None;
        if installed {
            self.persisted = Some(snapshot.clone());
            self.desired != self.persisted
        } else {
            // Keep the failed snapshot desired and retry it only on the next
            // persistence trigger. If something newer was coalesced while it
            // was in flight, that newer state can be drained immediately.
            self.desired.as_ref() != Some(snapshot)
        }
    }
}

/// Bridges the vendored Zed Git panel to zmux's logical workspaces.
#[derive(Default)]
pub struct ZmuxRepositoryScope {
    panels: Mutex<HashMap<EntityId, WeakEntity<WorkspacesPanel>>>,
}

struct ZmuxRepositoryScopeGlobal(Arc<ZmuxRepositoryScope>);
impl Global for ZmuxRepositoryScopeGlobal {}

pub fn install_git_repository_scope(cx: &mut App) {
    let scope = Arc::new(ZmuxRepositoryScope::default());
    git_ui::set_repository_scope(scope.clone(), cx);
    cx.set_global(ZmuxRepositoryScopeGlobal(scope));
}

pub fn register_git_repository_scope(
    project: &Entity<project::Project>,
    panel: &Entity<WorkspacesPanel>,
    cx: &App,
) {
    cx.global::<ZmuxRepositoryScopeGlobal>()
        .0
        .register(project, panel);
}

impl ZmuxRepositoryScope {
    pub fn register(&self, project: &Entity<project::Project>, panel: &Entity<WorkspacesPanel>) {
        self.panels
            .lock()
            .expect("repository scope registry poisoned")
            .insert(project.entity_id(), panel.downgrade());
    }

    fn panel_for(&self, project: &Entity<project::Project>) -> Option<WeakEntity<WorkspacesPanel>> {
        self.panels
            .lock()
            .expect("repository scope registry poisoned")
            .get(&project.entity_id())
            .cloned()
    }
}

impl git_ui::RepositoryScope for ZmuxRepositoryScope {
    fn repositories(
        &self,
        project: &Entity<project::Project>,
        cx: &App,
    ) -> Vec<Entity<project::git_store::Repository>> {
        let roots = self
            .panel_for(project)
            .and_then(|panel| panel.upgrade())
            .map(|panel| panel.read(cx).active_git_roots().to_vec())
            .unwrap_or_default();
        project
            .read(cx)
            .git_store()
            .read(cx)
            .repositories()
            .values()
            .filter(|repo| {
                roots
                    .iter()
                    .any(|root| repo.read(cx).snapshot().work_directory_abs_path.as_ref() == root)
            })
            .cloned()
            .collect()
    }

    fn display_name(
        &self,
        repository: &Entity<project::git_store::Repository>,
        cx: &App,
    ) -> SharedString {
        repository
            .read(cx)
            .snapshot()
            .work_directory_abs_path
            .display()
            .to_string()
            .into()
    }

    fn select(
        &self,
        project: &Entity<project::Project>,
        repository: Entity<project::git_store::Repository>,
        cx: &mut App,
    ) {
        let root = repository
            .read(cx)
            .snapshot()
            .work_directory_abs_path
            .to_path_buf();
        if let Some(panel) = self.panel_for(project).and_then(|panel| panel.upgrade()) {
            panel.update(cx, |panel, cx| panel.select_git_root(root.clone(), cx));
        }
        repository.update(cx, |repository, cx| repository.set_as_active_repository(cx));
    }

    fn active_worktree_paths(&self, project: &Entity<project::Project>, cx: &App) -> Vec<PathBuf> {
        self.panel_for(project)
            .and_then(|panel| panel.upgrade())
            .and_then(|panel| panel.read(cx).active_git_root())
            .into_iter()
            .collect()
    }

    fn open_worktree_paths(&self, project: &Entity<project::Project>, cx: &App) -> Vec<PathBuf> {
        self.panel_for(project)
            .and_then(|panel| panel.upgrade())
            .map(|panel| panel.read(cx).open_git_roots())
            .unwrap_or_default()
    }

    fn close_open_worktree(
        &self,
        project: &Entity<project::Project>,
        path: &Path,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        self.panel_for(project)
            .and_then(|panel| panel.upgrade())
            .is_some_and(|panel| {
                panel.update(cx, |panel, cx| {
                    panel.close_workspace_for_git_root(path, window, cx)
                })
            })
    }
}

impl WorkspacesPanel {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        session_enabled: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        if !cx.has_global::<SessionOwnerClaimed>() {
            cx.set_global(SessionOwnerClaimed::default());
        }
        let owns_session = session_enabled && !cx.global::<SessionOwnerClaimed>().0;
        if owns_session {
            cx.global_mut::<SessionOwnerClaimed>().0 = true;
        }
        let session_store = SessionStore::from_environment();
        let restored = if owns_session {
            match session_store.load() {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    eprintln!("ignoring invalid zmux session: {error:#}");
                    None
                }
            }
        } else {
            None
        };
        let (entries, active, next_id) = if let Some(snapshot) = &restored {
            (
                snapshot
                    .workspaces
                    .iter()
                    .map(|workspace| WorkspaceEntry {
                        id: workspace.id,
                        manual_name: workspace.manual_name.clone(),
                        worktree_name: workspace.worktree_name.clone(),
                        worktree_paths: workspace.worktree_paths.clone(),
                        automatic_name: workspace
                            .worktree_name
                            .clone()
                            .unwrap_or_else(|| "New workspace".to_string()),
                        context: WorkspaceContext::default(),
                        context_authoritative: false,
                        incomplete_context_refreshes: 0,
                        default_directory: workspace.default_directory.clone(),
                        selected_git_root: workspace.selected_git_root.clone(),
                        git_discovery: GitDiscoveryState::Restoring,
                        git: MetadataState::NotRequested,
                        metadata_root: None,
                        metadata_refreshed_at: None,
                        restore: Some(workspace.layout.clone()),
                        failed_restores: Vec::new(),
                        stored: None,
                    })
                    .collect(),
                snapshot.active_workspace_id,
                snapshot.next_workspace_id,
            )
        } else {
            (
                vec![WorkspaceEntry {
                    id: 1,
                    manual_name: None,
                    worktree_name: None,
                    worktree_paths: Vec::new(),
                    automatic_name: "New workspace".to_string(),
                    context: WorkspaceContext::default(),
                    context_authoritative: true,
                    incomplete_context_refreshes: 0,
                    default_directory: None,
                    selected_git_root: None,
                    git_discovery: GitDiscoveryState::Authoritative,
                    git: MetadataState::NotRequested,
                    metadata_root: None,
                    metadata_refreshed_at: None,
                    restore: None,
                    failed_restores: Vec::new(),
                    stored: None,
                }],
                1,
                2,
            )
        };
        let notification_store = NotificationStore::global(cx);
        let notification_subscription = cx.observe(&notification_store, |_, _, cx| cx.notify());
        let context_refresh_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(CONTEXT_REFRESH_INTERVAL)
                    .await;
                if this
                    .update(cx, |this, cx| {
                        this.refresh_workspace_contexts(cx);
                        this.request_metadata_refreshes(cx);
                        this.persist_session(cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            scope_id: cx.entity_id(),
            workspace,
            focus_handle,
            entries,
            active,
            activation_generation: 0,
            next_id,
            rename: None,
            notifications_expanded: false,
            notification_filter: None,
            _notification_subscription: notification_subscription,
            _context_refresh_task: context_refresh_task,
            session_store,
            owns_session,
            session_persistence: SessionPersistence::new(restored),
            attached_worktrees: HashMap::new(),
            pending_worktrees: BTreeSet::new(),
        }
    }

    pub fn active_workspace_id(&self) -> WorkspaceId {
        self.active
    }

    pub(crate) fn active_workspace_generation(&self) -> u64 {
        self.activation_generation
    }

    fn active_git_roots(&self) -> &[PathBuf] {
        self.entries
            .iter()
            .find(|entry| entry.id == self.active)
            .map(|entry| entry.context.git_roots.as_slice())
            .unwrap_or_default()
    }

    fn active_git_root(&self) -> Option<PathBuf> {
        self.entries
            .iter()
            .find(|entry| entry.id == self.active)
            .and_then(|entry| {
                entry
                    .selected_git_root
                    .clone()
                    .or_else(|| entry.context.git_root.clone())
                    .or_else(|| {
                        entry
                            .default_directory
                            .as_deref()
                            .and_then(nearest_git_root)
                    })
            })
    }

    fn open_git_roots(&self) -> Vec<PathBuf> {
        let mut roots = BTreeSet::new();
        for entry in &self.entries {
            roots.extend(entry.context.git_roots.iter().cloned());
            roots.extend(entry.selected_git_root.iter().cloned());
            roots.extend(entry.worktree_paths.iter().cloned());
            roots.extend(
                entry
                    .default_directory
                    .as_deref()
                    .and_then(nearest_git_root),
            );
        }
        roots.into_iter().collect()
    }

    fn select_git_root(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.id == self.active)
            && entry.context.git_roots.contains(&root)
        {
            entry.selected_git_root = Some(root);
            self.request_metadata_refreshes(cx);
            self.persist_session(cx);
            cx.notify();
        }
    }

    pub(crate) fn active_default_directory(&self) -> Option<PathBuf> {
        self.default_directory_for(self.active)
    }

    pub(crate) fn default_directory_for(&self, id: WorkspaceId) -> Option<PathBuf> {
        self.entries
            .iter()
            .find(|entry| entry.id == id)
            .and_then(|entry| entry.default_directory.clone())
    }

    pub(crate) fn initial_restore(&self) -> Option<LayoutSnapshot> {
        self.entries
            .iter()
            .find(|entry| entry.id == self.active)
            .and_then(|entry| entry.restore.clone())
    }

    pub(crate) fn switcher_entries(&self, cx: &App) -> Vec<WorkspaceSwitcherEntry> {
        let notifications = NotificationStore::global(cx).read(cx);
        self.entries
            .iter()
            .map(|entry| {
                let mut detail = Vec::new();
                detail.push(match entry.context.shell_count {
                    0 => "No shells".to_string(),
                    1 => "1 shell".to_string(),
                    count => format!("{count} shells"),
                });
                if let Some(cwd) = workspace_cwd_label(&entry.context) {
                    detail.push(cwd);
                }
                if let MetadataState::Ready(git) = &entry.git {
                    detail.push(git.compact_label());
                }
                if let Some(process) = entry.context.foreground_processes.first() {
                    detail.push(process.clone());
                }
                WorkspaceSwitcherEntry {
                    id: entry.id,
                    name: entry.display_name().to_string(),
                    detail: detail.join(" · "),
                    unread_count: notifications.workspace_unread_count(self.scope_id, entry.id),
                }
            })
            .collect()
    }

    /// Resolve an item's logical workspace even if an `ItemAdded` callback is
    /// deferred until after the user switches away from it.
    pub(crate) fn workspace_id_for_item(&self, item_id: EntityId) -> WorkspaceId {
        self.entries
            .iter()
            .find_map(|entry| {
                entry
                    .stored
                    .as_ref()
                    .is_some_and(|layout| stored_layout_contains_item(layout, item_id))
                    .then_some(entry.id)
            })
            .unwrap_or(self.active)
    }

    /// Create a fresh, empty workspace and switch to it.
    pub fn create_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.create_workspace_at(None, None, window, cx);
    }

    pub fn prompt_for_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose a folder for the new workspace".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let directory = paths.await.ok().and_then(Result::ok).flatten()?.pop()?;
            this.update_in(cx, |this, window, cx| {
                this.create_workspace_at(Some(directory), None, window, cx);
            })
            .ok();
            Some(())
        })
        .detach();
    }

    fn create_workspace_at(
        &mut self,
        default_directory: Option<PathBuf>,
        worktree_name: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("workspace ID space exhausted");
        self.entries.push(WorkspaceEntry {
            id,
            manual_name: None,
            worktree_name: worktree_name.clone(),
            worktree_paths: default_directory
                .iter()
                .filter(|_| worktree_name.is_some())
                .cloned()
                .collect(),
            automatic_name: worktree_name.unwrap_or_else(|| "New workspace".to_string()),
            context: WorkspaceContext::default(),
            context_authoritative: true,
            incomplete_context_refreshes: 0,
            default_directory,
            selected_git_root: None,
            git_discovery: GitDiscoveryState::Authoritative,
            git: MetadataState::NotRequested,
            metadata_root: None,
            metadata_refreshed_at: None,
            restore: None,
            failed_restores: Vec::new(),
            stored: None,
        });

        self.activate_workspace(id, window, cx);
    }

    /// Activate a logical workspace for an existing linked worktree, creating
    /// one with a fresh terminal when the path is not already open.
    pub fn open_worktree(
        &mut self,
        path: PathBuf,
        display_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        if let Some(id) = self.workspace_id_for_git_root(&path) {
            self.activate_workspace(id, window, cx);
            return;
        }
        self.create_workspace_at(Some(path), Some(display_name), window, cx);
    }

    /// Open all worktrees created by one Zed multi-repository operation in a
    /// single zmux logical workspace. The first path becomes the initial shell;
    /// additional repositories receive their own terminal tabs.
    pub fn open_created_worktrees(
        &mut self,
        mut paths: Vec<PathBuf>,
        display_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        paths.sort();
        paths.dedup();
        let all_paths = paths.clone();
        let first = paths.remove(0);
        self.create_workspace_at(Some(first), Some(display_name), window, cx);
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.id == self.active)
        {
            entry.worktree_paths = all_paths;
        }

        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let id = self.active;
        let generation = self.activation_generation;
        for path in paths {
            workspace.update(cx, |workspace, cx| {
                create_center_terminal_at_for_workspace(
                    workspace,
                    id,
                    generation,
                    Some(path),
                    window,
                    cx,
                )
                .detach_and_log_err(cx);
            });
        }
    }

    fn workspace_id_for_git_root(&self, path: &Path) -> Option<WorkspaceId> {
        self.entries.iter().find_map(|entry| {
            (entry.selected_git_root.as_deref() == Some(path)
                || entry.default_directory.as_deref() == Some(path)
                || entry.worktree_paths.iter().any(|root| root == path)
                || entry.context.git_roots.iter().any(|root| root == path))
            .then_some(entry.id)
        })
    }

    fn close_workspace_for_git_root(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(id) = self.workspace_id_for_git_root(path) else {
            return false;
        };
        if id == self.active || self.entries.len() <= 1 {
            return false;
        }
        self.close_workspace(id, window, cx);
        true
    }

    /// Switch the center to display the given workspace, parking the currently
    /// active one. The whole swap happens in a single [`Workspace`] update so the
    /// user never sees an intermediate state.
    pub fn activate_workspace(
        &mut self,
        id: WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if id == self.active {
            if let Some(workspace) = self.workspace.upgrade() {
                workspace.update(cx, |workspace, cx| workspace.focus_center_pane(window, cx));
            }
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        if !self.entries.iter().any(|entry| entry.id == id) {
            return;
        }

        self.cancel_rename(cx);
        let previous = self.active;
        self.activation_generation = self
            .activation_generation
            .checked_add(1)
            .expect("workspace activation generation exhausted");
        let target_generation = self.activation_generation;
        // Take the target's parked layout out before we borrow the workspace so we
        // don't have to touch `self` inside the update closure.
        let (target_layout, target_restore) = self
            .entries
            .iter_mut()
            .find(|entry| entry.id == id)
            .map(|entry| {
                let restore = (entry.git_discovery == GitDiscoveryState::Restoring)
                    .then(|| entry.restore.clone())
                    .flatten();
                (entry.stored.take(), restore)
            })
            .unwrap_or_default();
        let target_default_directory = self.default_directory_for(id);

        let (captured, restored_terminals, restored_snapshot) =
            workspace.update(cx, |workspace, cx| {
                let captured = capture_layout(workspace, cx);
                clear_center(workspace, window, cx);
                let mut restored_terminals = Vec::new();
                let mut restored_snapshot = false;

                let target_pane = workspace.active_pane().clone();
                match (target_restore, target_layout) {
                    (Some(layout), _) => {
                        // A restore snapshot remains owned by the entry until
                        // all shells attach. If a previous attempt was parked
                        // midway through, discard that partial live layout and
                        // retry the complete persisted snapshot.
                        restored_snapshot = true;
                        let mut rects = Vec::new();
                        let focused = restore_snapshot_layout(
                            workspace,
                            target_pane,
                            &layout,
                            window,
                            cx,
                            &mut restored_terminals,
                            &mut rects,
                        );
                        apply_restored_flexes(workspace, &rects, cx);
                        if let Some(focused) = focused {
                            window.focus(&focused.focus_handle(cx), cx);
                        }
                    }
                    (None, Some(layout)) => {
                        let mut rects = Vec::new();
                        let focused = restore_layout(
                            workspace,
                            target_pane,
                            layout,
                            UnitRect::FULL,
                            window,
                            cx,
                            &mut rects,
                        );
                        apply_restored_flexes(workspace, &rects, cx);
                        if let Some(focused) = focused {
                            window.focus(&focused.focus_handle(cx), cx);
                        }
                        // A new workspace can be parked while its asynchronous
                        // first shell is still spawning. Non-restored workspaces
                        // do not own a retry snapshot, so provision a replacement
                        // if their parked layout is still terminal-less.
                        if !center_has_provisioned_terminal(workspace, cx) {
                            create_center_terminal_for_workspace(
                                workspace,
                                id,
                                target_generation,
                                target_default_directory.clone(),
                                window,
                                cx,
                            )
                            .detach_and_log_err(cx);
                        }
                    }
                    (None, None) => {
                        //spawning a new terminal sometimes fails...
                        // this a good workarround for now. gotta add some sorta retry logic
                        let welcome = cx.new(ZmuxWelcome::new);
                        let target_pane = workspace.active_pane().clone();
                        target_pane.update(cx, |pane, cx| {
                            pane.add_item(Box::new(welcome), true, true, None, window, cx);
                        });
                        create_center_terminal_for_workspace(
                            workspace,
                            id,
                            target_generation,
                            target_default_directory.clone(),
                            window,
                            cx,
                        )
                        .detach_and_log_err(cx);
                    }
                }
                workspace.focus_center_pane(window, cx);
                (captured, restored_terminals, restored_snapshot)
            });

        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == previous) {
            entry.stored = Some(captured);
        }
        self.active = id;
        if restored_snapshot && restored_terminals.is_empty() {
            // Empty snapshots have no asynchronous terminal task that could
            // otherwise announce completion.
            self.finish_restored_git_discovery(id, cx);
        } else if !restored_terminals.is_empty() {
            workspace.update(cx, |workspace, cx| {
                create_restored_terminals_for_workspace(
                    workspace,
                    id,
                    target_generation,
                    restored_terminals,
                    window,
                    cx,
                )
                .detach_and_log_err(cx);
            });
        }
        self.refresh_workspace_contexts(cx);
        self.request_metadata_refreshes(cx);
        self.persist_session(cx);
        cx.notify();
    }

    pub fn activate_next_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current_index = self
            .entries
            .iter()
            .position(|entry| entry.id == self.active)
            .unwrap_or(0);
        let next_index = (current_index + 1) % self.entries.len();
        let next_id = self.entries[next_index].id;
        self.activate_workspace(next_id, window, cx);
    }

    pub fn activate_previous_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current_index = self
            .entries
            .iter()
            .position(|entry| entry.id == self.active)
            .unwrap_or(0);
        let prev_index = if current_index == 0 {
            self.entries.len() - 1
        } else {
            current_index - 1
        };
        let prev_id = self.entries[prev_index].id;
        self.activate_workspace(prev_id, window, cx);
    }

    /// Close a workspace. Its terminals are dropped along with the entry. The
    /// last remaining workspace can't be closed.
    fn close_workspace(&mut self, id: WorkspaceId, window: &mut Window, cx: &mut Context<Self>) {
        if self.entries.len() <= 1 {
            return;
        }
        if id == self.active {
            // Move to a neighbour first so the center always shows something.
            let fallback = self
                .entries
                .iter()
                .find(|entry| entry.id != id)
                .map(|entry| entry.id);
            if let Some(fallback) = fallback {
                self.activate_workspace(fallback, window, cx);
            }
        }

        // Dropping the entry drops its `StoredLayout`, releasing the terminals.
        self.entries.retain(|entry| entry.id != id);
        self.reconcile_git_context(cx);
        self.request_metadata_refreshes(cx);
        if self.notification_filter == Some(id) {
            self.notification_filter = None;
        }
        NotificationRuntime::clear_workspace(cx.entity_id(), id, cx);
        self.persist_session(cx);
        cx.notify();
    }

    /// Move the dragged workspace to the position indicated by the drop target.
    /// The target's top border means "insert before" and the bottom border means
    /// "insert after", matching the direction of the drag.
    fn reorder_workspace(
        &mut self,
        dragged_id: WorkspaceId,
        target_id: WorkspaceId,
        cx: &mut Context<Self>,
    ) {
        if dragged_id == target_id {
            return;
        }
        let Some(drag_ix) = self.entries.iter().position(|entry| entry.id == dragged_id) else {
            return;
        };
        let Some(target_ix) = self.entries.iter().position(|entry| entry.id == target_id) else {
            return;
        };
        let entry = self.entries.remove(drag_ix);
        self.entries.insert(target_ix, entry);
        self.persist_session(cx);
        cx.notify();
    }

    fn refresh_workspace_contexts(&mut self, cx: &mut Context<Self>) {
        let active_context = self
            .workspace
            .upgrade()
            .map(|workspace| workspace_context_for_active_workspace(workspace.read(cx), cx));
        let mut changed = false;

        for entry in &mut self.entries {
            let observed = if entry.id == self.active {
                active_context.clone().unwrap_or_default()
            } else {
                entry
                    .stored
                    .as_ref()
                    .map(|layout| workspace_context_for_stored_layout(layout, cx))
                    .unwrap_or_default()
            };
            let previous_context = entry.context.clone();
            let previous_authoritative = entry.context_authoritative;
            entry.observe_context(observed);
            let automatic_name = entry
                .worktree_name
                .clone()
                .unwrap_or_else(|| automatic_workspace_name(&entry.context));
            if entry.context != previous_context
                || entry.context_authoritative != previous_authoritative
                || entry.automatic_name != automatic_name
            {
                entry.automatic_name = automatic_name;
                changed = true;
            }
        }

        let discovery_changed = self.promote_restored_git_discovery();
        if changed || discovery_changed {
            cx.notify();
        }
        self.reconcile_git_context(cx);
    }

    fn promote_restored_git_discovery(&mut self) -> bool {
        let mut changed = false;
        for entry in &mut self.entries {
            if entry.git_discovery != GitDiscoveryState::Discovering {
                continue;
            }
            // The live shell directories are authoritative once every mounted
            // terminal has reported one. They need not match the snapshot: a
            // shell rc file may intentionally cd elsewhere, or a repository
            // may have been deleted since the previous launch.
            if entry.context_authoritative {
                entry.restore = None;
                entry.git_discovery = GitDiscoveryState::Authoritative;
                changed = true;
            }
        }
        changed
    }

    fn reconcile_git_context(&mut self, cx: &mut Context<Self>) {
        for entry in &mut self.entries {
            // A timed-out incomplete context is authoritative enough to stop
            // blocking persistence and cleanup, but never authoritative enough
            // to erase a user's selected repository. Reconcile selection only
            // from a fully observed shell set.
            if !entry.context.is_complete() {
                continue;
            }
            reconcile_selected_git_root(
                &mut entry.selected_git_root,
                &entry.context.git_roots,
                entry.git_discovery,
            );
        }

        let reference_counts = git_root_reference_counts(
            self.entries
                .iter()
                .map(|entry| entry.context.git_roots.as_slice()),
        );
        let attached = self.attached_worktrees.keys().cloned().collect();
        let reconciliation =
            plan_git_root_reconciliation(reference_counts, &attached, &self.pending_worktrees);
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let project = workspace.read(cx).project().clone();

        // A pass where any shell's directory probe failed understates the
        // referenced roots; removing worktrees from it would detach and
        // rescan repositories that are still in use. Wait for a stable pass.
        let contexts_authoritative = git_contexts_are_authoritative(&self.entries);
        if contexts_authoritative {
            for root in reconciliation.removed {
                if let Some(worktree) = self.attached_worktrees.remove(&root) {
                    let id = worktree.read(cx).id();
                    project.update(cx, |project, cx| project.remove_worktree(id, cx));
                }
            }
        }

        for root in reconciliation.added {
            // Keep the mutation outside the debug assertion: assertions are
            // compiled out of release builds, but pending scans are required
            // there to prevent duplicate worktree creation every refresh.
            let inserted = track_pending_worktree(&mut self.pending_worktrees, root.clone());
            debug_assert!(
                inserted,
                "planned additions are disjoint from pending scans"
            );
            // `find_or_create_worktree` accepts any existing ancestor worktree,
            // and Zed deliberately disables Git discovery for invisible
            // worktrees. Both behaviors are wrong for terminal-driven discovery:
            // attach an exact, Git-tracked worktree root. zmux has no project
            // panel, so making it visible affects only Zed's internal scanning.
            let existing = project
                .read(cx)
                .worktrees(cx)
                .find(|worktree| worktree.read(cx).abs_path().as_ref() == root.as_path());
            let task = if let Some(worktree) = existing {
                Task::ready(Ok(worktree))
            } else {
                project.update(cx, |project, cx| project.create_worktree(&root, true, cx))
            };
            let project = project.clone();
            cx.spawn(async move |this, cx| {
                let result = task.await;
                this.update(cx, |this, cx| {
                    this.pending_worktrees.remove(&root);
                    match result {
                        Ok(worktree)
                            if retain_completed_worktree_scan(
                                git_root_is_referenced(
                                    this.entries
                                        .iter()
                                        .map(|entry| entry.context.git_roots.as_slice()),
                                    &root,
                                ),
                                git_contexts_are_authoritative(&this.entries),
                            ) =>
                        {
                            this.attached_worktrees.insert(root.clone(), worktree);
                            this.activate_selected_repository(cx);
                        }
                        Ok(worktree) => {
                            // Repository discovery moved on while this worktree
                            // was scanning. Remove the Project's ownership as
                            // well as dropping this late result.
                            let id = worktree.read(cx).id();
                            project.update(cx, |project, cx| project.remove_worktree(id, cx));
                        }
                        Err(error) => {
                            eprintln!(
                                "failed to attach Git repository {}: {error:#}",
                                root.display()
                            );
                        }
                    }
                })
                .ok();
            })
            .detach();
        }
        self.activate_selected_repository(cx);
    }

    pub(crate) fn finish_restored_git_discovery(
        &mut self,
        id: WorkspaceId,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) else {
            return;
        };
        if entry.git_discovery == GitDiscoveryState::Restoring {
            entry.git_discovery = GitDiscoveryState::Discovering;
        }
        entry.failed_restores.clear();
        self.refresh_workspace_contexts(cx);
        self.request_metadata_refreshes(cx);
        self.persist_session(cx);
    }

    pub(crate) fn finish_restored_git_discovery_with_failures(
        &mut self,
        id: WorkspaceId,
        failed: Vec<FailedRestoreSlot>,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) else {
            return;
        };
        entry.failed_restores = failed;
        // The live layout can now persist immediately. Failed tabs are overlaid
        // into that snapshot at their original pane path/index and will retry
        // on the next launch; the complete original snapshot is no longer the
        // only persistence source.
        entry.restore = None;
        entry.git_discovery = GitDiscoveryState::Authoritative;
        self.refresh_workspace_contexts(cx);
        self.request_metadata_refreshes(cx);
        self.persist_session(cx);
    }

    fn activate_selected_repository(&self, cx: &mut Context<Self>) {
        let Some(root) = self
            .entries
            .iter()
            .find(|entry| entry.id == self.active)
            .and_then(|entry| entry.selected_git_root.as_ref())
        else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let git_store = workspace.read(cx).project().read(cx).git_store().clone();
        let repository = git_store
            .read(cx)
            .repositories()
            .values()
            .find(|repo| repo.read(cx).snapshot().work_directory_abs_path.as_ref() == root)
            .cloned();
        if let Some(repository) = repository {
            repository.update(cx, |repository, cx| repository.set_as_active_repository(cx));
        }
    }

    fn request_metadata_refreshes(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        let mut requests = Vec::new();
        for entry in &mut self.entries {
            let root = entry.selected_git_root.clone();
            if entry.metadata_root != root {
                entry.metadata_root = root.clone();
                entry.metadata_refreshed_at = None;
                entry.git = MetadataState::NotRequested;
            }
            let Some(root) = root else {
                continue;
            };
            let interval = if entry.id == self.active {
                ACTIVE_METADATA_INTERVAL
            } else {
                INACTIVE_METADATA_INTERVAL
            };
            let is_due = entry
                .metadata_refreshed_at
                .is_none_or(|refreshed| now.duration_since(refreshed) >= interval);
            if is_due && !matches!(entry.git, MetadataState::Pending) {
                entry.git = MetadataState::Pending;
                entry.metadata_refreshed_at = Some(now);
                requests.push((entry.id, root));
            }
        }

        for (id, root) in requests {
            let requested_root = root.clone();
            let collection = cx.background_spawn(async move { collect_git_metadata(&root) });
            cx.spawn(async move |this, cx| {
                let git = collection.await;
                this.update(cx, |this, cx| {
                    if let Some(entry) = this.entries.iter_mut().find(|entry| entry.id == id)
                        && entry.metadata_root.as_ref() == Some(&requested_root)
                    {
                        entry.git = git;
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    fn persist_session(&mut self, cx: &mut Context<Self>) {
        if !self.owns_session {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let active_layout = snapshot_active_layout(workspace.read(cx), cx);
        let mut workspaces = Vec::with_capacity(self.entries.len());
        for entry in &self.entries {
            let retained_restore = (entry.git_discovery != GitDiscoveryState::Authoritative)
                .then(|| entry.restore.clone())
                .flatten();
            let using_retained_restore = retained_restore.is_some();
            let mut layout = if let Some(restore) = retained_restore {
                restore
            } else if entry.id == self.active {
                active_layout.clone()
            } else if let Some(stored) = &entry.stored {
                snapshot_stored_layout(stored, cx)
            } else if let Some(restore) = &entry.restore {
                restore.clone()
            } else {
                LayoutSnapshot {
                    root: LayoutNodeSnapshot::Leaf {
                        tabs: Vec::new(),
                        active_tab: 0,
                        focused: true,
                    },
                }
            };
            if !using_retained_restore {
                overlay_failed_restores(&mut layout, &entry.failed_restores);
            }
            workspaces.push(WorkspaceSnapshot {
                id: entry.id,
                manual_name: entry.manual_name.clone(),
                worktree_name: entry.worktree_name.clone(),
                worktree_paths: entry.worktree_paths.clone(),
                default_directory: entry.default_directory.clone(),
                selected_git_root: entry.selected_git_root.clone(),
                layout,
            });
        }

        let snapshot = SessionSnapshot {
            version: SESSION_VERSION,
            next_workspace_id: self.next_id,
            active_workspace_id: self.active,
            workspaces,
        };
        self.session_persistence.request(snapshot);
        self.start_session_write(cx);
    }

    fn start_session_write(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.session_persistence.start_next() else {
            return;
        };

        let store = self.session_store.clone();
        let write = match store.prepare_save(&snapshot) {
            Ok(write) => write,
            Err(error) => {
                eprintln!("failed to prepare zmux session persistence: {error:#}");
                // A newer snapshot may have been coalesced while this one was
                // current; drain it rather than waiting for the next trigger.
                if self.session_persistence.complete(&snapshot, false) {
                    self.start_session_write(cx);
                }
                return;
            }
        };
        let save = cx.background_spawn(async move { store.commit(&write) });
        cx.spawn(async move |this, cx| {
            let result = save.await;
            this.update(cx, |this, cx| {
                let installed = match result {
                    Ok(SessionWriteOutcome::Installed) => true,
                    Ok(SessionWriteOutcome::Superseded) => false,
                    Err(error) => {
                        eprintln!("failed to persist zmux session: {error:#}");
                        false
                    }
                };

                // Successful writes immediately drain a coalesced newer
                // snapshot. Failed current writes remain desired and are
                // retried by the next persistence trigger, avoiding a tight
                // failure loop while keeping the persisted watermark honest.
                if this.session_persistence.complete(&snapshot, installed) {
                    this.start_session_write(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn start_rename(&mut self, id: WorkspaceId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.iter().find(|entry| entry.id == id) else {
            return;
        };
        let name = entry.display_name().to_string();
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(name, window, cx);
            editor
        });
        let subscription = cx.subscribe(&editor, |this, _editor, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::Blurred) {
                this.commit_rename(cx);
            }
        });
        window.focus(&editor.focus_handle(cx), cx);
        self.rename = Some(RenameState {
            id,
            editor,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(rename) = self.rename.take() else {
            return;
        };
        let text = rename.editor.read(cx).text(cx);
        if let Some(name) = sanitize_workspace_name(&text)
            && let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == rename.id)
        {
            entry.manual_name = Some(name);
        }
        self.persist_session(cx);
        cx.notify();
    }

    fn use_automatic_name(&mut self, id: WorkspaceId, cx: &mut Context<Self>) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) {
            entry.manual_name = None;
            self.persist_session(cx);
            cx.notify();
        }
    }

    fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        if self.rename.take().is_some() {
            cx.notify();
        }
    }

    fn render_entry(
        &self,
        entry: &WorkspaceRow,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = entry.id;
        let is_active = id == self.active;
        let unread_count = NotificationStore::global(cx)
            .read(cx)
            .workspace_unread_count(cx.entity_id(), id);
        let renaming = self
            .rename
            .as_ref()
            .filter(|rename| rename.id == id)
            .map(|rename| rename.editor.clone());
        let group = SharedString::from(format!("ws-row-{id}"));
        let can_close = self.entries.len() > 1;

        let editor = renaming.clone();
        let is_renaming = renaming.is_some();
        let name_row = h_flex()
            .id(("ws-name", id as usize))
            .flex_1()
            .gap_1()
            .overflow_hidden()
            .when(unread_count > 0, |this| {
                this.child(
                    div()
                        .id(("ws-unread", id as usize))
                        .flex_none()
                        .cursor_pointer()
                        .tooltip(Tooltip::text("Show this workspace's notifications"))
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            cx.stop_propagation();
                            this.show_workspace_notifications(id, cx);
                        }))
                        .child(Indicator::dot().color(Color::Accent)),
                )
            })
            .map(|this| match &editor {
                Some(editor) => this.child(
                    div()
                        .flex_1()
                        .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                            match event.keystroke.key.as_str() {
                                "enter" => this.commit_rename(cx),
                                "escape" => this.cancel_rename(cx),
                                _ => {}
                            }
                        }))
                        .child(editor.clone()),
                ),
                None => this.child(
                    Label::new(entry.name.clone())
                        .size(LabelSize::Default)
                        .weight(FontWeight::BOLD)
                        .color(Color::Default)
                        .single_line(),
                ),
            });

        let context = entry.context.clone();
        let shell_tooltip = match context.shell_count {
            0 => "No shells".to_string(),
            1 => "1 shell".to_string(),
            count => format!("{count} shells"),
        };
        let cwd_label = workspace_cwd_label(&context).filter(|cwd| cwd != &entry.name);
        let git_label = match &entry.git {
            MetadataState::Ready(git) => Some(git.compact_label()),
            MetadataState::Pending => Some("git refreshing".to_string()),
            MetadataState::Unavailable(_) | MetadataState::Error(_) => {
                Some("git unavailable".to_string())
            }
            MetadataState::NotRequested => None,
        };
        let diff_stats = match &entry.git {
            MetadataState::Ready(git) if git.added_lines > 0 || git.deleted_lines > 0 => {
                Some((git.added_lines, git.deleted_lines))
            }
            _ => None,
        };
        let has_git_metadata = git_label.is_some() || diff_stats.is_some();
        let foreground_processes = context.foreground_processes.clone();
        let has_foreground_processes = !foreground_processes.is_empty();
        let metadata = v_flex()
            .debug_selector(move || format!("WORKSPACE_METADATA-{id}"))
            .w_full()
            .gap_0p5()
            .child(
                h_flex()
                    .min_w_0()
                    .gap_1()
                    .child(
                        h_flex()
                            .id(("ws-shell-count", id as usize))
                            .flex_none()
                            .gap_0p5()
                            .tooltip(Tooltip::text(shell_tooltip))
                            .child(
                                Icon::new(IconName::Terminal)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(context.shell_count.to_string())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .when_some(cwd_label, |this, cwd| {
                        this.child(
                            div()
                                .id(("ws-cwd", id as usize))
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .tooltip(Tooltip::text(format!("Working directory: {cwd}")))
                                .child(
                                    Label::new(cwd)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .truncate(),
                                ),
                        )
                    }),
            )
            .when(has_git_metadata, |this| {
                this.child(
                    h_flex()
                        .gap_1()
                        .flex_wrap()
                        .child(
                            Icon::new(IconName::GitBranch)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .when_some(git_label, |this, git| {
                            this.child(
                                div()
                                    .flex_none()
                                    .px_1()
                                    .rounded_sm()
                                    .bg(cx.theme().colors().element_background)
                                    .child(
                                        Label::new(git)
                                            .size(LabelSize::Small)
                                            .color(Color::Muted)
                                            .single_line(),
                                    ),
                            )
                        })
                        .when_some(diff_stats, |this, (added, deleted)| {
                            this.child(
                                h_flex()
                                    .flex_none()
                                    .gap_0p5()
                                    .child(
                                        Label::new(format!("+{added}"))
                                            .size(LabelSize::Small)
                                            .color(Color::Success),
                                    )
                                    .child(
                                        Label::new(format!("-{deleted}"))
                                            .size(LabelSize::Small)
                                            .color(Color::Error),
                                    ),
                            )
                        }),
                )
            })
            .when(has_foreground_processes, |this| {
                this.child(
                    h_flex()
                        .gap_0p5()
                        .flex_wrap()
                        .child(
                            Icon::new(IconName::PlayFilled)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .children(foreground_processes.into_iter().map(|process| {
                            let process_id =
                                SharedString::from(format!("ws-process-{id}-{process}"));
                            div()
                                .id(process_id)
                                .flex_none()
                                .px_1()
                                .rounded_sm()
                                .bg(cx.theme().colors().element_background)
                                .tooltip(Tooltip::text(format!("Running: {process}")))
                                .map(|this| match foreground_process_icon(&process) {
                                    Some(ForegroundProcessIcon::Named(icon)) => this.child(
                                        Icon::new(icon).size(IconSize::XSmall).color(Color::Muted),
                                    ),
                                    Some(ForegroundProcessIcon::Embedded(path)) => this.child(
                                        Icon::from_path(path)
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                                    None => this.child(
                                        Label::new(process)
                                            .size(LabelSize::Small)
                                            .color(Color::Muted)
                                            .single_line(),
                                    ),
                                })
                        })),
                )
            });
        let name_area = v_flex()
            .id(("ws-name-area", id as usize))
            .flex_1()
            .gap_0p5()
            .overflow_hidden()
            .child(name_row)
            .child(metadata)
            .when(!is_renaming, |this| {
                this.cursor_pointer().on_click(cx.listener(
                    move |this, event: &gpui::ClickEvent, window, cx| {
                        if event.click_count() >= 2 {
                            this.start_rename(id, window, cx);
                        } else {
                            this.activate_workspace(id, window, cx);
                        }
                    },
                ))
            });

        let drag_ix = self
            .entries
            .iter()
            .position(|entry| entry.id == id)
            .unwrap_or(0);
        let target_ix = drag_ix;

        h_flex()
            .id(("ws-row", id as usize))
            .group(group.clone())
            .w_full()
            .px_2()
            .py_1()
            .gap_1()
            .rounded_md()
            .when(is_active, |this| {
                this.bg(cx.theme().colors().element_selected)
            })
            .hover(|this| this.bg(cx.theme().colors().element_hover))
            .on_drag(
                DraggedWorkspace {
                    id,
                    name: entry.name.clone(),
                    ix: drag_ix,
                },
                |drag, _, _, cx| cx.new(|_| drag.clone()),
            )
            .drag_over::<DraggedWorkspace>(move |style, dragged, _window, cx| {
                if dragged.ix < target_ix {
                    style
                        .border_b_2()
                        .border_color(cx.theme().colors().drop_target_border)
                } else if dragged.ix > target_ix {
                    style
                        .border_t_2()
                        .border_color(cx.theme().colors().drop_target_border)
                } else {
                    style
                }
            })
            .on_drop(
                cx.listener(move |this, dragged: &DraggedWorkspace, _window, cx| {
                    this.reorder_workspace(dragged.id, id, cx);
                }),
            )
            .child(name_area)
            .child(
                h_flex()
                    .gap_0p5()
                    .visible_on_hover(group)
                    .when(entry.uses_manual_name, |this| {
                        this.child(
                            IconButton::new(("ws-auto-name", id as usize), IconName::RotateCcw)
                                .shape(IconButtonShape::Square)
                                .icon_size(IconSize::XSmall)
                                .tooltip(Tooltip::text("Use automatic name"))
                                .on_click(cx.listener(move |this, _, _window, cx| {
                                    cx.stop_propagation();
                                    this.use_automatic_name(id, cx);
                                })),
                        )
                    })
                    .child(
                        IconButton::new(("ws-rename", id as usize), IconName::Pencil)
                            .shape(IconButtonShape::Square)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Rename Workspace"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.start_rename(id, window, cx);
                            })),
                    )
                    .when(can_close, |this| {
                        this.child(
                            IconButton::new(("ws-close", id as usize), IconName::Close)
                                .shape(IconButtonShape::Square)
                                .icon_size(IconSize::XSmall)
                                .tooltip(Tooltip::text("Close Workspace"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.close_workspace(id, window, cx);
                                })),
                        )
                    }),
            )
    }

    fn toggle_notifications(&mut self, cx: &mut Context<Self>) {
        if self.notifications_expanded && self.notification_filter.is_none() {
            self.notifications_expanded = false;
        } else {
            self.notifications_expanded = true;
            self.notification_filter = None;
        }
        cx.notify();
    }

    fn show_workspace_notifications(&mut self, id: WorkspaceId, cx: &mut Context<Self>) {
        self.notification_filter = Some(id);
        self.notifications_expanded = true;
        cx.notify();
    }

    pub fn toggle_notification_center(&mut self, cx: &mut Context<Self>) {
        self.toggle_notifications(cx);
    }

    fn dismiss_notification(&mut self, id: u64, cx: &mut Context<Self>) {
        NotificationRuntime::dismiss_notification(id, cx);
    }

    fn mark_visible_read(&mut self, cx: &mut Context<Self>) {
        let scope_id = cx.entity_id();
        if let Some(workspace_id) = self.notification_filter {
            NotificationRuntime::mark_workspace_read(scope_id, workspace_id, cx);
        } else {
            NotificationRuntime::mark_scope_read(scope_id, cx);
        }
    }

    fn clear_visible_notifications(&mut self, cx: &mut Context<Self>) {
        let scope_id = cx.entity_id();
        if let Some(workspace_id) = self.notification_filter {
            NotificationRuntime::clear_workspace(scope_id, workspace_id, cx);
        } else {
            NotificationRuntime::clear_scope_notifications(scope_id, cx);
        }
    }

    fn render_notification(
        &self,
        notification: &Notification,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = notification.id;
        let is_unread = !notification.read;
        let title = if notification.title.is_empty() {
            "Terminal notification".to_owned()
        } else {
            notification.title.clone()
        };
        let subtitle = notification.subtitle.clone();
        let body = notification.body.clone();
        let source = notification.source.label();

        v_flex()
            .id(("notification", id as usize))
            .w_full()
            .p_2()
            .gap_1()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .hover(|this| this.bg(cx.theme().colors().element_hover))
            .cursor_pointer()
            .on_click(cx.listener(move |_, _, _window, cx| {
                NotificationRuntime::open_notification(id, cx);
            }))
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(Indicator::dot().color(if is_unread {
                        Color::Accent
                    } else {
                        Color::Muted
                    }))
                    .child(
                        Label::new(title)
                            .size(LabelSize::Small)
                            .single_line()
                            .flex_1(),
                    )
                    .child(
                        IconButton::new(("notification-dismiss", id as usize), IconName::Close)
                            .shape(IconButtonShape::Square)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Dismiss"))
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                cx.stop_propagation();
                                this.dismiss_notification(id, cx);
                            })),
                    ),
            )
            .when(!subtitle.is_empty(), |this| {
                this.child(
                    Label::new(subtitle)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .single_line(),
                )
            })
            .when(!body.is_empty(), |this| {
                this.child(
                    Label::new(body)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .line_clamp(3),
                )
            })
            .child(
                Label::new(source)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
    }
}

impl Focusable for WorkspacesPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for WorkspacesPanel {}

impl Render for WorkspacesPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let scope_id = cx.entity_id();
        let notification_filter = self.notification_filter;
        let (rows, latest, unread_count, notifications) = {
            let store = NotificationStore::global(cx);
            let store = store.read(cx);
            let rows = self
                .entries
                .iter()
                .map(|entry| WorkspaceRow {
                    id: entry.id,
                    name: entry.display_name().to_string(),
                    uses_manual_name: entry.manual_name.is_some(),
                    context: entry.context.clone(),
                    git: entry.git.clone(),
                })
                .collect::<Vec<_>>();
            let latest = store
                .notifications()
                .find(|notification| {
                    notification.target.scope_id == scope_id
                        && notification_filter.is_none_or(|workspace_id| {
                            notification.target.workspace_id == workspace_id
                        })
                        && !notification.read
                })
                .cloned();
            let unread_count = notification_filter.map_or_else(
                || store.scope_unread_count(scope_id),
                |workspace_id| store.workspace_unread_count(scope_id, workspace_id),
            );
            let notifications = store
                .notifications()
                .filter(|notification| {
                    notification.target.scope_id == scope_id
                        && notification_filter.is_none_or(|workspace_id| {
                            notification.target.workspace_id == workspace_id
                        })
                })
                .cloned()
                .collect::<Vec<_>>();
            (rows, latest, unread_count, notifications)
        };
        let notification_heading = notification_filter
            .and_then(|workspace_id| rows.iter().find(|row| row.id == workspace_id))
            .map_or_else(
                || format!("Notifications · {unread_count} unread"),
                |row| format!("{} · {unread_count} unread", row.name),
            );

        let workspace_handle = self.workspace.clone();
        let project = workspace_handle
            .upgrade()
            .map(|workspace| workspace.read(cx).project().clone());
        let active_worktree_operation = workspace_handle
            .upgrade()
            .and_then(|workspace| workspace.read(cx).active_worktree_creation().label.clone());
        let active_root = self.active_git_root();
        let active_repository = project.as_ref().and_then(|project| {
            project
                .read(cx)
                .git_store()
                .read(cx)
                .repositories()
                .values()
                .find(|repository| {
                    active_root.as_ref().is_some_and(|root| {
                        repository
                            .read(cx)
                            .snapshot()
                            .work_directory_abs_path
                            .as_ref()
                            == root
                    })
                })
                .cloned()
        });
        let active_entry = self.entries.iter().find(|entry| entry.id == self.active);
        let active_worktree_name = active_entry
            .and_then(|entry| entry.worktree_name.clone())
            .or_else(|| active_root.as_deref().and_then(path_display_name))
            .unwrap_or_else(|| "Worktree".to_string());
        let active_workspace_name = active_worktree_operation
            .as_ref()
            .map(|label| format!("Creating {label}…"))
            .unwrap_or(active_worktree_name);
        let active_branch_name = active_repository
            .as_ref()
            .and_then(|repository| {
                repository
                    .read(cx)
                    .branch
                    .as_ref()
                    .map(|branch| branch.name().to_string())
            })
            .or_else(|| {
                active_entry.and_then(|entry| match &entry.git {
                    MetadataState::Ready(metadata) => Some(metadata.branch.clone()),
                    _ => None,
                })
            })
            .unwrap_or_else(|| "HEAD".to_string());

        let worktree_selector = active_repository
            .as_ref()
            .and(project.clone())
            .map(|project| {
                let workspace = workspace_handle.clone();
                PopoverMenu::new("worktree-selector")
                    .menu(move |window, cx| {
                        Some(cx.new(|cx| {
                            git_ui::worktree_picker::WorktreePicker::new(
                                project.clone(),
                                workspace.clone(),
                                window,
                                cx,
                            )
                        }))
                    })
                    .trigger_with_tooltip(
                        Button::new("worktree-selector-button", active_workspace_name)
                            .size(ButtonSize::None)
                            .start_icon(Icon::new(IconName::GitWorktree).size(IconSize::Small))
                            .disabled(active_worktree_operation.is_some())
                            .truncate(true),
                        |_, cx| Tooltip::simple("Switch or create a Git worktree", cx),
                    )
                    .anchor(Anchor::BottomLeft)
            });

        let branch_selector = active_repository.map(|repository| {
            let workspace = workspace_handle.clone();
            PopoverMenu::new("workspace-branch-selector")
                .menu(move |window, cx| {
                    Some(git_ui::branch_picker::popover(
                        workspace.clone(),
                        false,
                        Some(repository.clone()),
                        window,
                        cx,
                    ))
                })
                .trigger_with_tooltip(
                    Button::new("workspace-branch-selector-button", active_branch_name)
                        .size(ButtonSize::None)
                        .start_icon(Icon::new(IconName::GitBranch).size(IconSize::Small))
                        .truncate(true),
                    |_, cx| Tooltip::simple("Switch or create a Git branch", cx),
                )
                .anchor(Anchor::BottomLeft)
        });

        v_flex()
            .key_context("WorkspacesPanel")
            .font_family(WORKSPACES_FONT_FAMILY)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .px_2()
                    .py_1p5()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Label::new("Workspaces")
                            .size(LabelSize::Default)
                            .weight(FontWeight::SEMIBOLD)
                            .color(Color::Default),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                IconButton::new("notifications", IconName::Info)
                                    .shape(IconButtonShape::Square)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Notifications (Cmd+I)"))
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.toggle_notifications(cx);
                                    })),
                            )
                            .child(
                                IconButton::new("new-workspace", IconName::Plus)
                                    .shape(IconButtonShape::Square)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("New Workspace"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.prompt_for_workspace(window, cx);
                                    })),
                            ),
                    ),
            )
            .when_some(worktree_selector, |this, worktree_selector| {
                this.child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .px_2()
                        .py_1()
                        .gap_1()
                        .border_b_1()
                        .border_color(cx.theme().colors().border)
                        .child(div().flex_1().min_w_0().child(worktree_selector))
                        .when_some(branch_selector, |this, branch_selector| {
                            this.child(div().flex_1().min_w_0().child(branch_selector))
                        }),
                )
            })
            .child(
                v_flex()
                    .id("workspaces-list")
                    .p_1()
                    .gap_0p5()
                    .overflow_y_scroll()
                    .flex_1()
                    .children(rows.iter().map(|entry| self.render_entry(entry, cx))),
            )
            .when(self.notifications_expanded, |this| {
                this.child(
                    v_flex()
                        .max_h(scaled_panel_size(
                            window.rem_size(),
                            NOTIFICATION_DRAWER_HEIGHT_REMS,
                        ))
                        .border_t_1()
                        .border_color(cx.theme().colors().border)
                        .child(
                            h_flex()
                                .px_2()
                                .py_1()
                                .justify_between()
                                .child(Label::new(notification_heading).size(LabelSize::Small))
                                .child(
                                    h_flex()
                                        .gap_0p5()
                                        .child(
                                            IconButton::new("notifications-read", IconName::Check)
                                                .shape(IconButtonShape::Square)
                                                .icon_size(IconSize::XSmall)
                                                .tooltip(Tooltip::text("Mark all read"))
                                                .on_click(cx.listener(|this, _, _window, cx| {
                                                    this.mark_visible_read(cx)
                                                })),
                                        )
                                        .child(
                                            IconButton::new("notifications-clear", IconName::Trash)
                                                .shape(IconButtonShape::Square)
                                                .icon_size(IconSize::XSmall)
                                                .tooltip(Tooltip::text("Clear notifications"))
                                                .on_click(cx.listener(|this, _, _window, cx| {
                                                    this.clear_visible_notifications(cx)
                                                })),
                                        ),
                                ),
                        )
                        .child(
                            v_flex()
                                .id("notifications-list")
                                .p_1()
                                .gap_1()
                                .overflow_y_scroll()
                                .children(
                                    notifications.iter().map(|notification| {
                                        self.render_notification(notification, cx)
                                    }),
                                )
                                .when(notifications.is_empty(), |this| {
                                    this.child(
                                        Label::new("No notifications")
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                }),
                        ),
                )
            })
            .when(!self.notifications_expanded, |this| {
                this.when_some(latest, |this, notification| {
                    let id = notification.id;
                    this.child(
                        v_flex()
                            .id("latest-notification")
                            .p_2()
                            .gap_1()
                            .border_t_1()
                            .border_color(cx.theme().colors().border)
                            .cursor_pointer()
                            .hover(|this| this.bg(cx.theme().colors().element_hover))
                            .on_click(cx.listener(move |_, _, _window, cx| {
                                NotificationRuntime::open_notification(id, cx);
                            }))
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(Indicator::dot().color(Color::Accent))
                                    .child(
                                        Label::new(format!("{} unread", unread_count))
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    ),
                            )
                            .child(
                                Label::new(notification.title.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Default)
                                    .single_line(),
                            )
                            .child(
                                Label::new(notification.body.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .line_clamp(2),
                            ),
                    )
                })
            })
    }
}

impl Panel for WorkspacesPanel {
    fn persistent_name() -> &'static str {
        "WorkspacesPanel"
    }

    fn panel_key() -> &'static str {
        "WorkspacesPanel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Left
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Left
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, window: &Window, _cx: &App) -> Pixels {
        scaled_panel_size(window.rem_size(), PANEL_WIDTH_REMS)
    }

    fn min_size(&self, window: &Window, _cx: &App) -> Option<Pixels> {
        Some(scaled_panel_size(window.rem_size(), PANEL_MIN_WIDTH_REMS))
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::ListCollapse)
    }

    fn icon_label(&self, _window: &Window, cx: &App) -> Option<String> {
        let count = NotificationStore::global(cx)
            .read(cx)
            .scope_unread_count(self.scope_id);
        (count > 0).then(|| count.to_string())
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Workspaces")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleWorkspacesPanel)
    }

    fn activation_priority(&self) -> u32 {
        0
    }

    fn starts_open(&self, _window: &Window, _cx: &App) -> bool {
        true
    }
}

fn workspace_context_for_active_workspace(workspace: &Workspace, cx: &App) -> WorkspaceContext {
    let mut context = WorkspaceContext::default();
    for pane in workspace.panes() {
        for item in pane.read(cx).items() {
            add_item_to_workspace_context(item.as_ref(), &mut context, cx);
        }
    }
    finalize_workspace_context(context)
}

fn workspace_context_for_stored_layout(layout: &StoredLayout, cx: &App) -> WorkspaceContext {
    fn visit(layout: &StoredLayout, context: &mut WorkspaceContext, cx: &App) {
        match layout {
            StoredLayout::Leaf { items, .. } => {
                for item in items {
                    add_item_to_workspace_context(item.as_ref(), context, cx);
                }
            }
            StoredLayout::Split { first, second, .. } => {
                visit(first, context, cx);
                visit(second, context, cx);
            }
        }
    }

    let mut context = WorkspaceContext::default();
    visit(layout, &mut context, cx);
    finalize_workspace_context(context)
}

fn add_item_to_workspace_context(item: &dyn ItemHandle, context: &mut WorkspaceContext, cx: &App) {
    let Some(terminal_view) = item.act_as::<TerminalView>(cx) else {
        return;
    };
    let terminal = terminal_view.read(cx).terminal().clone();
    let terminal = terminal.read(cx);
    context.shell_count += 1;
    if let Some(directory) = terminal.working_directory() {
        context.reported_directories += 1;
        context.working_directories.push(directory);
    }
    if let Some(process) = terminal.foreground_process_command_name()
        && !is_shell_process(&process)
        && let Some(process) = sanitize_process_label(&process)
    {
        context.foreground_processes.push(process);
    }
}

fn finalize_workspace_context(mut context: WorkspaceContext) -> WorkspaceContext {
    context.working_directories.sort();
    context.working_directories.dedup();

    let processes = context
        .foreground_processes
        .drain(..)
        .collect::<BTreeSet<_>>();
    context.foreground_processes = processes.into_iter().take(8).collect();

    context.git_roots = context
        .working_directories
        .iter()
        .filter_map(|directory| nearest_git_root(directory))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if context.git_roots.len() == 1 {
        context.git_root = context.git_roots.first().cloned();
    }
    context
}

fn nearest_git_root(directory: &Path) -> Option<PathBuf> {
    directory
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .map(|root| std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()))
}

fn common_ancestor(paths: &[PathBuf]) -> Option<PathBuf> {
    let first = paths.first()?;
    let mut common = first.clone();
    for path in &paths[1..] {
        while !path.starts_with(&common) {
            if !common.pop() {
                return None;
            }
        }
    }
    Some(common)
}

fn automatic_workspace_name(context: &WorkspaceContext) -> String {
    if let Some(git_root) = &context.git_root
        && let Some(name) = path_display_name(git_root)
    {
        return name;
    }

    if let Some(common) = common_ancestor(&context.working_directories) {
        if common == paths::home_dir().as_path() {
            if context.working_directories.len() == 1 {
                return "Home".to_string();
            }
        } else if common.starts_with(paths::home_dir().as_path())
            && let Some(name) = path_display_name(&common)
        {
            return name;
        }
    }

    match context.shell_count {
        0 => "New workspace".to_string(),
        1 => "Shell".to_string(),
        _ => "Mixed shells".to_string(),
    }
}

fn workspace_cwd_label(context: &WorkspaceContext) -> Option<String> {
    let directory = common_ancestor(&context.working_directories)?;
    let mut label = if let Some(repository) = &context.git_root
        && let Ok(relative) = directory.strip_prefix(repository)
    {
        let repository = repository.file_name()?.to_string_lossy();
        if relative.as_os_str().is_empty() {
            repository.to_string()
        } else {
            format!("{repository}/{}", relative.display())
        }
    } else if let Ok(relative) = directory.strip_prefix(paths::home_dir().as_path()) {
        if relative.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", relative.display())
        }
    } else {
        directory.display().to_string()
    };
    label.retain(|character| !character.is_control());
    if label.chars().count() > 40 {
        let tail = label
            .chars()
            .rev()
            .take(37)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        label = format!("…{tail}");
    }
    (!label.is_empty()).then_some(label)
}

fn path_display_name(path: &Path) -> Option<String> {
    path.file_name()
        .map(|name| name.to_string_lossy())
        .and_then(|name| sanitize_workspace_name(&name))
}

fn sanitize_workspace_name(name: &str) -> Option<String> {
    let normalized = name
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>();
    let normalized = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    let normalized = normalized
        .chars()
        .take(MAX_WORKSPACE_NAME_CHARS)
        .collect::<String>();
    (!normalized.is_empty()).then_some(normalized)
}

fn sanitize_process_label(process: &str) -> Option<String> {
    sanitize_workspace_name(process).map(|process| process.chars().take(24).collect())
}

fn is_shell_process(process: &str) -> bool {
    matches!(
        process
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(process)
            .to_ascii_lowercase()
            .as_str(),
        "bash"
            | "zsh"
            | "fish"
            | "sh"
            | "dash"
            | "nu"
            | "xonsh"
            | "pwsh"
            | "powershell"
            | "cmd"
            | "cmd.exe"
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ForegroundProcessIcon {
    Named(IconName),
    Embedded(&'static str),
}

fn foreground_process_icon(process: &str) -> Option<ForegroundProcessIcon> {
    let executable = process
        .trim()
        .rsplit(['/', '\\'])
        .next()?
        .to_ascii_lowercase();
    let executable = executable.strip_suffix(".exe").unwrap_or(&executable);

    match executable {
        "codex" => Some(ForegroundProcessIcon::Named(IconName::AiOpenAi)),
        "claude" | "claude-code" => Some(ForegroundProcessIcon::Named(IconName::AiClaude)),
        "git" => Some(ForegroundProcessIcon::Named(IconName::GitBranch)),
        "nvim" | "neovim" => Some(ForegroundProcessIcon::Embedded("icons/neovim.svg")),
        _ => None,
    }
}

/// Snapshot the current center into a [`StoredLayout`], cloning each item handle
/// so the terminals stay alive after the originals are detached.
///
/// The split structure and ratios are read directly from the live pane group's
/// flex values rather than from paint-time bounding boxes: the boxes are only
/// refreshed by a layout pass, so they are stale (or too short) whenever a
/// capture runs between a structural change and the next frame.
fn capture_layout(workspace: &Workspace, cx: &App) -> StoredLayout {
    capture_member(&workspace.center_group().root, workspace, cx).unwrap_or(StoredLayout::Leaf {
        items: Vec::new(),
        active: 0,
        focused: true,
    })
}

fn capture_member(member: &Member, workspace: &Workspace, cx: &App) -> Option<StoredLayout> {
    match member {
        Member::Pane(pane) => {
            let pane_ref = pane.read(cx);
            let items: Vec<Box<dyn ItemHandle>> =
                pane_ref.items().map(|item| item.boxed_clone()).collect();
            if items.is_empty() {
                return None;
            }
            Some(StoredLayout::Leaf {
                items,
                active: pane_ref.active_item_index(),
                focused: pane == workspace.active_pane(),
            })
        }
        Member::Axis(axis) => {
            let weights = axis.flexes.lock().clone();
            let children: Vec<(f32, StoredLayout)> = axis
                .members
                .iter()
                .enumerate()
                .filter_map(|(index, child)| {
                    let child = capture_member(child, workspace, cx)?;
                    Some((weights.get(index).copied().unwrap_or(1.0).max(0.0), child))
                })
                .collect();
            fold_axis_run(axis.axis, children, &mut |axis, ratio, first, second| {
                StoredLayout::Split {
                    axis,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }
            })
        }
    }
}

/// Fold an n-member axis run into right-nested binary splits whose ratios are
/// derived from the run's flex weights, so weights `[1, 2, 1]` become
/// `Split(0.25, a, Split(2/3, b, c))`.
fn fold_axis_run<T>(
    axis: Axis,
    children: Vec<(f32, T)>,
    split: &mut impl FnMut(Axis, f32, T, T) -> T,
) -> Option<T> {
    let mut result: Option<(f32, T)> = None;
    for (weight, node) in children.into_iter().rev() {
        result = Some(match result {
            None => (weight, node),
            Some((rest_weight, rest)) => {
                let total = weight + rest_weight;
                let ratio = if total > f32::EPSILON {
                    weight / total
                } else {
                    0.5
                };
                (total, split(axis, ratio, node, rest))
            }
        });
    }
    result.map(|(_, node)| node)
}

fn stored_layout_contains_item(layout: &StoredLayout, item_id: EntityId) -> bool {
    match layout {
        StoredLayout::Leaf { items, .. } => items.iter().any(|item| item.item_id() == item_id),
        StoredLayout::Split { first, second, .. } => {
            stored_layout_contains_item(first, item_id)
                || stored_layout_contains_item(second, item_id)
        }
    }
}

fn center_has_provisioned_terminal(workspace: &Workspace, cx: &App) -> bool {
    workspace.panes().iter().any(|pane| {
        pane.read(cx)
            .items()
            .any(|item| item.act_as::<TerminalView>(cx).is_some())
    })
}

/// Detach every item from the center, keeping the terminals alive (the caller is
/// expected to already hold cloned handles). Leaves a single empty pane.
fn clear_center(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    workspace.join_all_panes(window, cx);
    let pane = workspace.active_pane().clone();
    pane.update(
        cx,
        |pane, cx| {
            while pane.take_active_item(window, cx).is_some() {}
        },
    );
}

/// Rebuild a [`StoredLayout`] into the center, starting from a single empty pane.
///
/// Each leaf's share of the unit square is recorded in `rects` so the caller
/// can write the stored split ratios into the live pane group afterwards via
/// [`apply_restored_flexes`].
fn restore_layout(
    workspace: &mut Workspace,
    target: Entity<Pane>,
    layout: StoredLayout,
    rect: UnitRect,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    rects: &mut Vec<(Entity<Pane>, UnitRect)>,
) -> Option<Entity<Pane>> {
    match layout {
        StoredLayout::Leaf {
            items,
            active,
            focused,
        } => {
            if !items.is_empty() {
                target.update(cx, |pane, cx| {
                    for item in items {
                        pane.add_item(item, false, false, None, window, cx);
                    }
                    let index = active.min(pane.items_len().saturating_sub(1));
                    pane.activate_item(index, false, false, window, cx);
                });
            }
            rects.push((target.clone(), rect));
            focused.then_some(target)
        }
        StoredLayout::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let direction = if axis == Axis::Horizontal {
                SplitDirection::Right
            } else {
                SplitDirection::Down
            };
            let new_pane = workspace.split_pane(target.clone(), direction, window, cx);
            let (first_rect, second_rect) = rect.split(axis, ratio);
            let focused_first =
                restore_layout(workspace, target, *first, first_rect, window, cx, rects);
            let focused_second =
                restore_layout(workspace, new_pane, *second, second_rect, window, cx, rects);
            focused_first.or(focused_second)
        }
    }
}

/// Write the stored split ratios into the freshly rebuilt center tree as flex
/// values. This runs synchronously after the splits are created: it does not
/// touch paint-time bounding boxes, which stay stale until the next frame (the
/// old resize-based approach either silently skipped the restore or indexed
/// out of bounds on axes with three or more panes).
fn apply_restored_flexes(
    workspace: &Workspace,
    rects: &[(Entity<Pane>, UnitRect)],
    cx: &mut Context<Workspace>,
) {
    let by_pane: HashMap<EntityId, UnitRect> = rects
        .iter()
        .map(|(pane, rect)| (pane.entity_id(), *rect))
        .collect();
    apply_member_flexes(&workspace.center_group().root, &by_pane);
    cx.notify();
}

fn apply_member_flexes(member: &Member, rects: &HashMap<EntityId, UnitRect>) -> Option<UnitRect> {
    match member {
        Member::Pane(pane) => rects.get(&pane.entity_id()).copied(),
        Member::Axis(axis) => {
            let child_rects = axis
                .members
                .iter()
                .map(|child| apply_member_flexes(child, rects))
                .collect::<Option<Vec<_>>>()?;
            let spans: Vec<f32> = child_rects
                .iter()
                .map(|child| child.span(axis.axis))
                .collect();
            let total: f32 = spans.iter().sum();
            if total > f32::EPSILON && spans.iter().all(|span| *span > f32::EPSILON) {
                let count = spans.len() as f32;
                *axis.flexes.lock() = spans.iter().map(|span| span / total * count).collect();
            }
            child_rects.into_iter().reduce(UnitRect::union)
        }
    }
}

fn terminal_snapshot(item: &dyn ItemHandle, cx: &App) -> Option<TerminalSnapshot> {
    let terminal_view = item.act_as::<TerminalView>(cx)?;
    let terminal = terminal_view.read(cx).terminal().clone();
    let working_directory = terminal.read(cx).working_directory();
    Some(TerminalSnapshot::fresh_shell(working_directory))
}

fn snapshot_active_layout(workspace: &Workspace, cx: &App) -> LayoutSnapshot {
    let root = snapshot_member(&workspace.center_group().root, workspace, cx).unwrap_or(
        LayoutNodeSnapshot::Leaf {
            tabs: Vec::new(),
            active_tab: 0,
            focused: true,
        },
    );
    LayoutSnapshot { root }
}

fn snapshot_member(member: &Member, workspace: &Workspace, cx: &App) -> Option<LayoutNodeSnapshot> {
    match member {
        Member::Pane(pane) => {
            let pane_ref = pane.read(cx);
            let mut tabs = Vec::new();
            let mut active_tab = 0;
            for (index, item) in pane_ref.items().enumerate() {
                if let Some(terminal) = terminal_snapshot(item.as_ref(), cx) {
                    if index == pane_ref.active_item_index() {
                        active_tab = tabs.len();
                    }
                    tabs.push(terminal);
                }
            }
            Some(LayoutNodeSnapshot::Leaf {
                tabs,
                active_tab,
                focused: pane == workspace.active_pane(),
            })
        }
        Member::Axis(axis) => {
            let weights = axis.flexes.lock().clone();
            let children: Vec<(f32, LayoutNodeSnapshot)> = axis
                .members
                .iter()
                .enumerate()
                .filter_map(|(index, child)| {
                    let child = snapshot_member(child, workspace, cx)?;
                    Some((weights.get(index).copied().unwrap_or(1.0).max(0.0), child))
                })
                .collect();
            fold_axis_run(axis.axis, children, &mut |axis, ratio, first, second| {
                LayoutNodeSnapshot::Split {
                    axis: axis_to_snapshot(axis),
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }
            })
        }
    }
}

fn snapshot_stored_layout(layout: &StoredLayout, cx: &App) -> LayoutSnapshot {
    fn snapshot_node(layout: &StoredLayout, cx: &App) -> LayoutNodeSnapshot {
        match layout {
            StoredLayout::Leaf {
                items,
                active,
                focused,
            } => {
                let mut tabs = Vec::new();
                let mut active_tab = 0;
                for (index, item) in items.iter().enumerate() {
                    if let Some(terminal) = terminal_snapshot(item.as_ref(), cx) {
                        if index == *active {
                            active_tab = tabs.len();
                        }
                        tabs.push(terminal);
                    }
                }
                LayoutNodeSnapshot::Leaf {
                    tabs,
                    active_tab,
                    focused: *focused,
                }
            }
            StoredLayout::Split {
                axis,
                ratio,
                first,
                second,
            } => LayoutNodeSnapshot::Split {
                axis: axis_to_snapshot(*axis),
                ratio: *ratio,
                first: Box::new(snapshot_node(first, cx)),
                second: Box::new(snapshot_node(second, cx)),
            },
        }
    }

    LayoutSnapshot {
        root: snapshot_node(layout, cx),
    }
}

fn restore_snapshot_layout(
    workspace: &mut Workspace,
    target: Entity<Pane>,
    layout: &LayoutSnapshot,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    terminals: &mut Vec<RestoredTerminal>,
    rects: &mut Vec<(Entity<Pane>, UnitRect)>,
) -> Option<Entity<Pane>> {
    struct RestoreState<'a> {
        terminals: &'a mut Vec<RestoredTerminal>,
        rects: &'a mut Vec<(Entity<Pane>, UnitRect)>,
        path: Vec<bool>,
    }

    fn restore_node(
        workspace: &mut Workspace,
        target: Entity<Pane>,
        node: &LayoutNodeSnapshot,
        rect: UnitRect,
        window: &mut Window,
        cx: &mut Context<Workspace>,
        state: &mut RestoreState<'_>,
    ) -> Option<Entity<Pane>> {
        match node {
            LayoutNodeSnapshot::Leaf {
                tabs,
                active_tab,
                focused,
            } => {
                state
                    .terminals
                    .extend(
                        tabs.iter()
                            .enumerate()
                            .map(|(index, terminal)| RestoredTerminal {
                                pane: target.clone(),
                                working_directory: terminal.working_directory.clone(),
                                activate: index == *active_tab,
                                failed_slot: FailedRestoreSlot {
                                    path: state.path.clone(),
                                    tab_index: index,
                                    terminal: terminal.clone(),
                                    activate: index == *active_tab,
                                },
                            }),
                    );
                state.rects.push((target.clone(), rect));
                focused.then_some(target)
            }
            LayoutNodeSnapshot::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let axis = axis_from_snapshot(*axis);
                let direction = if axis == Axis::Horizontal {
                    SplitDirection::Right
                } else {
                    SplitDirection::Down
                };
                let new_pane = workspace.split_pane(target.clone(), direction, window, cx);
                let (first_rect, second_rect) = rect.split(axis, *ratio);
                state.path.push(false);
                let focused_first =
                    restore_node(workspace, target, first, first_rect, window, cx, state);
                state.path.pop();
                state.path.push(true);
                let focused_second =
                    restore_node(workspace, new_pane, second, second_rect, window, cx, state);
                state.path.pop();
                focused_first.or(focused_second)
            }
        }
    }

    let mut state = RestoreState {
        terminals,
        rects,
        path: Vec::new(),
    };
    restore_node(
        workspace,
        target,
        &layout.root,
        UnitRect::FULL,
        window,
        cx,
        &mut state,
    )
}

pub(crate) fn restore_startup_layout(
    workspace: &mut Workspace,
    layout: &LayoutSnapshot,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Vec<RestoredTerminal> {
    clear_center(workspace, window, cx);
    let target = workspace.active_pane().clone();
    let mut terminals = Vec::new();
    let mut rects = Vec::new();
    let focused = restore_snapshot_layout(
        workspace,
        target,
        layout,
        window,
        cx,
        &mut terminals,
        &mut rects,
    );
    apply_restored_flexes(workspace, &rects, cx);
    if let Some(focused) = focused {
        window.focus(&focused.focus_handle(cx), cx);
    }
    terminals
}

fn axis_to_snapshot(axis: Axis) -> LayoutAxis {
    if axis == Axis::Horizontal {
        LayoutAxis::Horizontal
    } else {
        LayoutAxis::Vertical
    }
}

fn axis_from_snapshot(axis: LayoutAxis) -> Axis {
    match axis {
        LayoutAxis::Horizontal => Axis::Horizontal,
        LayoutAxis::Vertical => Axis::Vertical,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn persistence_snapshot(name: &str) -> SessionSnapshot {
        SessionSnapshot {
            version: SESSION_VERSION,
            next_workspace_id: 2,
            active_workspace_id: 1,
            workspaces: vec![WorkspaceSnapshot {
                id: 1,
                manual_name: Some(name.into()),
                worktree_name: None,
                worktree_paths: Vec::new(),
                default_directory: None,
                selected_git_root: None,
                layout: LayoutSnapshot {
                    root: LayoutNodeSnapshot::Leaf {
                        tabs: Vec::new(),
                        active_tab: 0,
                        focused: true,
                    },
                },
            }],
        }
    }

    #[test]
    fn session_persistence_serializes_coalesces_and_retries() {
        let restored = persistence_snapshot("restored");
        let first = persistence_snapshot("first");
        let newest = persistence_snapshot("newest");
        let mut persistence = SessionPersistence::new(Some(restored.clone()));

        persistence.request(first.clone());
        assert_eq!(persistence.start_next(), Some(first.clone()));
        persistence.request(persistence_snapshot("intermediate"));
        persistence.request(newest.clone());
        assert_eq!(persistence.start_next(), None);

        assert!(persistence.complete(&first, true));
        assert_eq!(persistence.persisted, Some(first.clone()));
        assert_eq!(persistence.start_next(), Some(newest.clone()));
        assert!(!persistence.complete(&newest, true));
        assert_eq!(persistence.persisted, Some(newest.clone()));
        assert_eq!(persistence.start_next(), None);

        // Even an unexpected stale completion is ignored once a newer
        // snapshot has been installed.
        assert!(!persistence.complete(&first, true));
        assert_eq!(persistence.persisted, Some(newest.clone()));

        let retry = persistence_snapshot("retry");
        persistence.request(retry.clone());
        assert_eq!(persistence.start_next(), Some(retry.clone()));
        assert!(!persistence.complete(&retry, false));
        assert_eq!(persistence.persisted, Some(newest));
        assert_eq!(persistence.desired, Some(retry.clone()));
        assert_eq!(persistence.start_next(), Some(retry.clone()));
        assert!(!persistence.complete(&retry, true));
        assert_eq!(persistence.persisted, Some(retry));
    }

    #[test]
    fn failed_restore_overlay_preserves_the_tab_without_freezing_live_layout() {
        let live = TerminalSnapshot::fresh_shell(Some("/repos/live".into()));
        let failed = TerminalSnapshot::fresh_shell(Some("/repos/failed".into()));
        let newly_opened = TerminalSnapshot::fresh_shell(Some("/repos/new".into()));
        let mut layout = LayoutSnapshot {
            root: LayoutNodeSnapshot::Leaf {
                tabs: vec![live.clone(), newly_opened.clone()],
                active_tab: 1,
                focused: true,
            },
        };

        overlay_failed_restores(
            &mut layout,
            &[FailedRestoreSlot {
                path: Vec::new(),
                tab_index: 1,
                terminal: failed.clone(),
                activate: false,
            }],
        );

        let LayoutNodeSnapshot::Leaf {
            tabs, active_tab, ..
        } = layout.root
        else {
            panic!("expected leaf")
        };
        assert_eq!(tabs, vec![live, failed, newly_opened]);
        assert_eq!(active_tab, 2, "the live active tab remains active");
    }

    #[test]
    fn workspace_panel_dimensions_follow_ui_scale() {
        assert_eq!(scaled_panel_size(px(16.0), PANEL_WIDTH_REMS), px(240.0));
        assert_eq!(scaled_panel_size(px(22.4), PANEL_WIDTH_REMS), px(336.0));
        assert_eq!(scaled_panel_size(px(22.4), PANEL_MIN_WIDTH_REMS), px(268.8));
    }

    #[test]
    fn automatic_names_prioritize_a_shared_git_project() {
        let context = WorkspaceContext {
            working_directories: vec![
                PathBuf::from("/tmp/zmux/src"),
                PathBuf::from("/tmp/zmux/tests"),
            ],
            git_roots: vec![PathBuf::from("/tmp/zmux")],
            git_root: Some(PathBuf::from("/tmp/zmux")),
            foreground_processes: vec!["cargo".into()],
            shell_count: 2,
            reported_directories: 2,
        };

        assert_eq!(automatic_workspace_name(&context), "zmux");
    }

    #[test]
    fn automatic_names_use_the_common_project_directory() {
        let context = WorkspaceContext {
            working_directories: vec![
                paths::home_dir().join("Documents/project/api"),
                paths::home_dir().join("Documents/project/web"),
            ],
            shell_count: 2,
            ..WorkspaceContext::default()
        };

        assert_eq!(automatic_workspace_name(&context), "project");
    }

    #[test]
    fn manual_workspace_names_are_sanitized_and_bounded() {
        let long = format!("  hello\nworld {}  ", "x".repeat(100));
        let sanitized = sanitize_workspace_name(&long).unwrap();

        assert_eq!(sanitized.chars().count(), MAX_WORKSPACE_NAME_CHARS);
        assert!(sanitized.starts_with("helloworld "));
        assert!(!sanitized.chars().any(char::is_control));
        assert_eq!(sanitize_workspace_name("\n\t"), None);
    }

    #[test]
    fn shell_processes_are_not_rendered_as_activity_pills() {
        assert!(is_shell_process("/usr/bin/bash"));
        assert!(is_shell_process("pwsh"));
        assert!(!is_shell_process("cargo"));
    }

    #[test]
    fn known_workspace_processes_use_only_their_product_icons() {
        assert_eq!(
            foreground_process_icon("codex"),
            Some(ForegroundProcessIcon::Named(IconName::AiOpenAi))
        );
        assert_eq!(
            foreground_process_icon("/usr/local/bin/claude"),
            Some(ForegroundProcessIcon::Named(IconName::AiClaude))
        );
        assert_eq!(
            foreground_process_icon(r"C:\\tools\\claude-code.exe"),
            Some(ForegroundProcessIcon::Named(IconName::AiClaude))
        );
        assert_eq!(
            foreground_process_icon("/usr/bin/git"),
            Some(ForegroundProcessIcon::Named(IconName::GitBranch))
        );
        assert_eq!(
            foreground_process_icon("nvim"),
            Some(ForegroundProcessIcon::Embedded("icons/neovim.svg"))
        );
        assert_eq!(
            foreground_process_icon(r"C:\\tools\\Neovim\\bin\\nvim.exe"),
            Some(ForegroundProcessIcon::Embedded("icons/neovim.svg"))
        );
        assert_eq!(foreground_process_icon("cargo"), None);
    }

    #[test]
    fn git_context_keeps_independent_roots_and_ignores_non_repo_terminals() {
        let base = std::env::temp_dir().join(format!(
            "zmux-git-context-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let repo_a = base.join("a");
        let repo_b = base.join("b");
        let outside = base.join("outside");
        std::fs::create_dir_all(repo_a.join(".git")).unwrap();
        std::fs::create_dir_all(repo_a.join("src")).unwrap();
        std::fs::create_dir_all(repo_b.join(".git")).unwrap();
        std::fs::create_dir_all(outside.clone()).unwrap();

        let context = finalize_workspace_context(WorkspaceContext {
            working_directories: vec![repo_a.join("src"), outside, repo_b.clone()],
            shell_count: 3,
            ..WorkspaceContext::default()
        });

        assert_eq!(context.git_roots, vec![repo_a, repo_b]);
        assert_eq!(context.git_root, None);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn git_context_collapses_multiple_terminals_in_one_repository() {
        let base =
            std::env::temp_dir().join(format!("zmux-single-git-context-{}", std::process::id()));
        std::fs::create_dir_all(base.join(".git")).unwrap();
        std::fs::create_dir_all(base.join("api")).unwrap();
        std::fs::create_dir_all(base.join("web")).unwrap();

        let context = finalize_workspace_context(WorkspaceContext {
            working_directories: vec![base.join("api"), base.join("web")],
            shell_count: 2,
            ..WorkspaceContext::default()
        });

        assert_eq!(context.git_roots, vec![base.clone()]);
        assert_eq!(context.git_root, Some(base.clone()));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn inactive_restored_workspace_keeps_selection_before_discovery() {
        let selected_root = PathBuf::from("/repos/selected");
        let mut selected = Some(selected_root.clone());

        reconcile_selected_git_root(&mut selected, &[], GitDiscoveryState::Restoring);

        assert_eq!(selected, Some(selected_root));
    }

    #[test]
    fn active_restore_waits_for_complete_multi_repo_discovery() {
        let first_root = PathBuf::from("/repos/first");
        let selected_root = PathBuf::from("/repos/selected");
        let mut selected = Some(selected_root.clone());

        // The first restored terminal must not win merely because its shell
        // was created before the selected repository's terminal.
        reconcile_selected_git_root(
            &mut selected,
            std::slice::from_ref(&first_root),
            GitDiscoveryState::Restoring,
        );
        assert_eq!(selected, Some(selected_root.clone()));
        reconcile_selected_git_root(
            &mut selected,
            std::slice::from_ref(&first_root),
            GitDiscoveryState::Discovering,
        );
        assert_eq!(selected, Some(selected_root.clone()));

        reconcile_selected_git_root(
            &mut selected,
            &[first_root, selected_root.clone()],
            GitDiscoveryState::Authoritative,
        );
        assert_eq!(selected, Some(selected_root));
    }

    #[test]
    fn authoritative_discovery_replaces_a_missing_selection() {
        let discovered_root = PathBuf::from("/repos/current");
        let mut selected = Some(PathBuf::from("/repos/gone"));

        reconcile_selected_git_root(
            &mut selected,
            std::slice::from_ref(&discovered_root),
            GitDiscoveryState::Authoritative,
        );

        assert_eq!(selected, Some(discovered_root));
    }

    #[test]
    fn complete_live_context_is_authoritative_even_when_snapshot_root_moved() {
        let context = WorkspaceContext {
            working_directories: vec![PathBuf::from("/repos/current")],
            git_roots: vec![PathBuf::from("/repos/current")],
            shell_count: 1,
            reported_directories: 1,
            ..WorkspaceContext::default()
        };
        assert!(context.is_complete());

        let incomplete = WorkspaceContext {
            shell_count: 2,
            reported_directories: 1,
            ..context
        };
        assert!(!incomplete.is_complete());
    }

    #[test]
    fn permanently_missing_cwd_becomes_authoritative_after_a_bounded_grace_period() {
        let retained = PathBuf::from("/repos/retained");
        let mut entry = WorkspaceEntry {
            id: 1,
            manual_name: None,
            worktree_name: None,
            worktree_paths: Vec::new(),
            automatic_name: "retained".into(),
            context: WorkspaceContext {
                working_directories: vec![retained.clone()],
                git_roots: vec![retained.clone()],
                shell_count: 1,
                reported_directories: 1,
                ..WorkspaceContext::default()
            },
            context_authoritative: true,
            incomplete_context_refreshes: 0,
            default_directory: None,
            selected_git_root: Some(retained.clone()),
            git_discovery: GitDiscoveryState::Authoritative,
            git: MetadataState::NotRequested,
            metadata_root: None,
            metadata_refreshed_at: None,
            restore: None,
            failed_restores: Vec::new(),
            stored: None,
        };
        let missing = WorkspaceContext {
            shell_count: 1,
            reported_directories: 0,
            ..WorkspaceContext::default()
        };

        for _ in 1..MAX_INCOMPLETE_CONTEXT_REFRESHES {
            entry.observe_context(missing.clone());
            assert!(!entry.context_authoritative);
            assert_eq!(entry.context.git_roots, vec![retained.clone()]);
        }
        entry.observe_context(missing.clone());
        assert!(entry.context_authoritative);
        assert!(entry.context.git_roots.is_empty());

        entry.observe_context(WorkspaceContext {
            working_directories: vec![retained],
            shell_count: 1,
            reported_directories: 1,
            ..WorkspaceContext::default()
        });
        assert!(entry.context_authoritative);
        assert_eq!(entry.incomplete_context_refreshes, 0);
    }

    #[test]
    fn incomplete_context_retains_a_completed_worktree_scan_until_a_stable_pass() {
        assert!(retain_completed_worktree_scan(false, false));
        assert!(!retain_completed_worktree_scan(false, true));
        assert!(retain_completed_worktree_scan(true, true));
    }

    #[test]
    fn pending_scan_tracking_mutates_in_release_profiles() {
        let root = PathBuf::from("/repos/pending");
        let mut pending = BTreeSet::new();

        assert!(track_pending_worktree(&mut pending, root.clone()));
        assert!(pending.contains(&root));
        assert!(!track_pending_worktree(&mut pending, root));
    }

    #[test]
    fn git_root_reconciliation_counts_shared_workspace_ownership() {
        let shared = PathBuf::from("/repos/shared");
        let first = PathBuf::from("/repos/first");
        let second = PathBuf::from("/repos/second");
        let stale = PathBuf::from("/repos/stale");
        let workspace_a = [shared.clone(), first.clone()];
        let workspace_b = [shared.clone(), second.clone()];
        let counts = git_root_reference_counts([workspace_a.as_slice(), workspace_b.as_slice()]);
        assert_eq!(counts[&shared], 2);
        assert_eq!(counts[&first], 1);
        assert_eq!(counts[&second], 1);
        let attached = BTreeSet::from([shared.clone(), stale.clone()]);
        let pending = BTreeSet::from([second.clone()]);

        let reconciliation = plan_git_root_reconciliation(counts, &attached, &pending);

        assert_eq!(reconciliation.added, BTreeSet::from([first]));
        assert_eq!(reconciliation.removed, BTreeSet::from([stale]));
    }

    #[test]
    fn late_attachment_is_unreferenced_after_its_workspace_moves_on() {
        let late = PathBuf::from("/repos/late");
        let current = PathBuf::from("/repos/current");
        let roots = [current];

        assert!(!git_root_is_referenced([roots.as_slice()], &late));
    }

    #[test]
    fn git_root_reconciliation_stress_returns_to_active_roots() {
        let visited = (0..256)
            .map(|index| PathBuf::from(format!("/repos/visited-{index}")))
            .collect::<BTreeSet<_>>();
        let active = visited.iter().rev().take(3).cloned().collect::<Vec<_>>();
        let reference_counts = git_root_reference_counts([active.as_slice()]);

        let reconciliation =
            plan_git_root_reconciliation(reference_counts, &visited, &BTreeSet::new());
        let mut retained = visited;
        for root in reconciliation.removed {
            retained.remove(&root);
        }
        retained.extend(reconciliation.added);

        assert_eq!(retained, active.into_iter().collect());
        assert_eq!(retained.len(), 3);
    }

    #[gpui::test]
    async fn panel_restores_exact_selected_root_after_multi_repo_discovery(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.executor().allow_parking();
        let base =
            std::env::temp_dir().join(format!("zmux-multi-repo-restore-{}", uuid::Uuid::new_v4()));
        let first = base.join("first");
        let selected = base.join("selected");
        std::fs::create_dir_all(first.join(".git")).unwrap();
        std::fs::create_dir_all(selected.join(".git")).unwrap();

        let open = cx.update(|cx| {
            crate::app::init_zmux(cx);
            crate::app::open_zmux_workspace_at(None, base.clone(), cx)
        });
        let opened = open.await.expect("workspace should open");
        let panel = opened.workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<WorkspacesPanel>(cx)
                .expect("workspaces panel should be installed")
        });
        panel.update(cx, |panel, cx| {
            panel._context_refresh_task = Task::ready(());
            panel.entries.push(WorkspaceEntry {
                id: 2,
                manual_name: None,
                worktree_name: None,
                worktree_paths: Vec::new(),
                automatic_name: "Restored".into(),
                context: WorkspaceContext::default(),
                context_authoritative: false,
                incomplete_context_refreshes: 0,
                default_directory: None,
                selected_git_root: Some(selected.clone()),
                git_discovery: GitDiscoveryState::Restoring,
                git: MetadataState::NotRequested,
                metadata_root: None,
                metadata_refreshed_at: None,
                restore: Some(LayoutSnapshot {
                    root: LayoutNodeSnapshot::Leaf {
                        tabs: vec![
                            TerminalSnapshot::fresh_shell(Some(first.clone())),
                            TerminalSnapshot::fresh_shell(Some(selected.clone())),
                        ],
                        active_tab: 1,
                        focused: true,
                    },
                }),
                failed_restores: Vec::new(),
                stored: None,
            });
            panel.next_id = 3;
            // This is the periodic pass that used to erase an inactive
            // restored workspace's selection before it was ever activated.
            panel.reconcile_git_context(cx);
        });
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.entries[1].selected_git_root.clone()),
            Some(selected.clone())
        );

        opened
            .window
            .update(cx, |_, window, cx| {
                panel.update(cx, |panel, cx| panel.activate_workspace(2, window, cx));
            })
            .expect("window should remain open");
        assert_eq!(
            panel.read_with(cx, |panel, _| {
                let entry = panel.entries.iter().find(|entry| entry.id == 2).unwrap();
                (entry.git_discovery, entry.selected_git_root.clone())
            }),
            (GitDiscoveryState::Restoring, Some(selected.clone()))
        );

        for _ in 0..200 {
            cx.run_until_parked();
            panel.update(cx, |panel, cx| panel.refresh_workspace_contexts(cx));
            let restored = panel.read_with(cx, |panel, _| {
                let entry = panel.entries.iter().find(|entry| entry.id == 2).unwrap();
                entry.git_discovery == GitDiscoveryState::Authoritative
                    && entry.context.git_roots == vec![first.clone(), selected.clone()]
            });
            if restored {
                break;
            }
            cx.background_executor
                .timer(Duration::from_millis(10))
                .await;
        }
        let live_directories = opened.workspace.read_with(cx, |workspace, cx| {
            workspace_context_for_active_workspace(workspace, cx).working_directories
        });
        panel.read_with(cx, |panel, _| {
            let entry = panel.entries.iter().find(|entry| entry.id == 2).unwrap();
            assert_eq!(entry.git_discovery, GitDiscoveryState::Authoritative);
            assert_eq!(
                entry.context.git_roots,
                vec![first.clone(), selected.clone()],
                "shell_count={}, working_directories={:?}, live_directories={live_directories:?}",
                entry.context.shell_count,
                entry.context.working_directories,
            );
            assert_eq!(entry.selected_git_root, Some(selected.clone()));
        });

        let _ = std::fs::remove_dir_all(base);
    }

    #[gpui::test]
    async fn empty_restored_snapshot_becomes_authoritative_on_activation(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.executor().allow_parking();
        let base =
            std::env::temp_dir().join(format!("zmux-empty-restore-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let stale_selection = base.join("stale");

        let open = cx.update(|cx| {
            crate::app::init_zmux(cx);
            crate::app::open_zmux_workspace_at(None, base.clone(), cx)
        });
        let opened = open.await.expect("workspace should open");
        let panel = opened.workspace.read_with(cx, |workspace, cx| {
            workspace.panel::<WorkspacesPanel>(cx).unwrap()
        });
        panel.update(cx, |panel, _| {
            panel._context_refresh_task = Task::ready(());
            panel.entries.push(WorkspaceEntry {
                id: 2,
                manual_name: None,
                worktree_name: None,
                worktree_paths: Vec::new(),
                automatic_name: "Empty restore".into(),
                context: WorkspaceContext::default(),
                context_authoritative: false,
                incomplete_context_refreshes: 0,
                default_directory: None,
                selected_git_root: Some(stale_selection),
                git_discovery: GitDiscoveryState::Restoring,
                git: MetadataState::NotRequested,
                metadata_root: None,
                metadata_refreshed_at: None,
                restore: Some(LayoutSnapshot {
                    root: LayoutNodeSnapshot::Leaf {
                        tabs: Vec::new(),
                        active_tab: 0,
                        focused: true,
                    },
                }),
                failed_restores: Vec::new(),
                stored: None,
            });
            panel.next_id = 3;
        });

        opened
            .window
            .update(cx, |_, window, cx| {
                panel.update(cx, |panel, cx| panel.activate_workspace(2, window, cx));
            })
            .expect("window should remain open");

        panel.read_with(cx, |panel, _| {
            let entry = panel.entries.iter().find(|entry| entry.id == 2).unwrap();
            assert_eq!(entry.git_discovery, GitDiscoveryState::Authoritative);
            assert_eq!(entry.selected_git_root, None);
        });
        let _ = std::fs::remove_dir_all(base);
    }

    #[gpui::test]
    async fn interrupted_restore_retries_full_snapshot_before_becoming_authoritative(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.executor().allow_parking();
        let base =
            std::env::temp_dir().join(format!("zmux-interrupted-restore-{}", uuid::Uuid::new_v4()));
        let selected = base.join("selected");
        std::fs::create_dir_all(selected.join(".git")).unwrap();

        let open = cx.update(|cx| {
            crate::app::init_zmux(cx);
            crate::app::open_zmux_workspace_at(None, base.clone(), cx)
        });
        let opened = open.await.expect("workspace should open");
        let panel = opened.workspace.read_with(cx, |workspace, cx| {
            workspace.panel::<WorkspacesPanel>(cx).unwrap()
        });
        panel.update(cx, |panel, _| {
            panel._context_refresh_task = Task::ready(());
            panel.entries.push(WorkspaceEntry {
                id: 2,
                manual_name: None,
                worktree_name: None,
                worktree_paths: Vec::new(),
                automatic_name: "Interrupted restore".into(),
                context: WorkspaceContext::default(),
                context_authoritative: false,
                incomplete_context_refreshes: 0,
                default_directory: Some(selected.clone()),
                selected_git_root: Some(selected.clone()),
                git_discovery: GitDiscoveryState::Restoring,
                git: MetadataState::NotRequested,
                metadata_root: None,
                metadata_refreshed_at: None,
                restore: Some(LayoutSnapshot {
                    root: LayoutNodeSnapshot::Leaf {
                        tabs: vec![TerminalSnapshot::fresh_shell(Some(selected.clone()))],
                        active_tab: 0,
                        focused: true,
                    },
                }),
                failed_restores: Vec::new(),
                stored: None,
            });
            panel.next_id = 3;
        });

        // Switch away in the same foreground turn, before the restored shell
        // can attach. The parked empty layout must not replace the full snapshot.
        opened
            .window
            .update(cx, |_, window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.activate_workspace(2, window, cx);
                    panel.activate_workspace(1, window, cx);
                });
            })
            .expect("window should remain open");
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let entry = panel.entries.iter().find(|entry| entry.id == 2).unwrap();
            assert_eq!(entry.git_discovery, GitDiscoveryState::Restoring);
            assert_eq!(entry.selected_git_root, Some(selected.clone()));
        });

        opened
            .window
            .update(cx, |_, window, cx| {
                panel.update(cx, |panel, cx| panel.activate_workspace(2, window, cx));
            })
            .expect("window should remain open");
        for _ in 0..200 {
            cx.run_until_parked();
            panel.update(cx, |panel, cx| panel.refresh_workspace_contexts(cx));
            if panel.read_with(cx, |panel, _| {
                panel
                    .entries
                    .iter()
                    .find(|entry| entry.id == 2)
                    .is_some_and(|entry| entry.git_discovery == GitDiscoveryState::Authoritative)
            }) {
                break;
            }
            cx.background_executor
                .timer(Duration::from_millis(10))
                .await;
        }
        panel.read_with(cx, |panel, _| {
            let entry = panel.entries.iter().find(|entry| entry.id == 2).unwrap();
            assert_eq!(entry.git_discovery, GitDiscoveryState::Authoritative);
            assert_eq!(entry.selected_git_root, Some(selected.clone()));
            assert_eq!(entry.context.git_roots, vec![selected.clone()]);
        });

        let _ = std::fs::remove_dir_all(base);
    }

    #[gpui::test]
    async fn worktree_paths_open_once_and_keep_their_logical_identity(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.executor().allow_parking();
        let base =
            std::env::temp_dir().join(format!("zmux-logical-worktree-{}", uuid::Uuid::new_v4()));
        let first = base.join("feature").join("repo");
        let second = base.join("feature").join("docs");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();

        let open = cx.update(|cx| {
            crate::app::init_zmux(cx);
            crate::app::open_zmux_workspace_at(None, base.clone(), cx)
        });
        let opened = open.await.expect("workspace should open");
        let panel = opened.workspace.read_with(cx, |workspace, cx| {
            workspace.panel::<WorkspacesPanel>(cx).unwrap()
        });
        panel.update(cx, |panel, _| {
            panel._context_refresh_task = Task::ready(());
        });

        opened
            .window
            .update(cx, |_, window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.open_created_worktrees(
                        vec![second.clone(), first.clone(), first.clone()],
                        "feature".into(),
                        window,
                        cx,
                    );
                });
            })
            .unwrap();

        let worktree_id = panel.read_with(cx, |panel, _| {
            assert_eq!(panel.entries.len(), 2);
            let entry = panel
                .entries
                .iter()
                .find(|entry| entry.id == panel.active)
                .unwrap();
            assert_eq!(entry.worktree_name.as_deref(), Some("feature"));
            assert_eq!(entry.display_name(), "feature");
            assert_eq!(entry.worktree_paths, vec![second.clone(), first.clone()]);
            entry.id
        });

        opened
            .window
            .update(cx, |_, window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.activate_workspace(1, window, cx);
                    panel.open_worktree(first.clone(), "feature".into(), window, cx);
                });
            })
            .unwrap();

        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.entries.len(), 2, "an open worktree was duplicated");
            assert_eq!(panel.active, worktree_id);
            assert_eq!(panel.open_git_roots(), vec![second.clone(), first.clone()]);
        });

        let _ = std::fs::remove_dir_all(base);
    }

    #[gpui::test]
    async fn closing_workspace_removes_only_its_project_worktree(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let base =
            std::env::temp_dir().join(format!("zmux-worktree-ownership-{}", uuid::Uuid::new_v4()));
        let shared = base.join("shared");
        let unique = base.join("unique");
        std::fs::create_dir_all(shared.join(".git")).unwrap();
        std::fs::create_dir_all(unique.join(".git")).unwrap();

        let open = cx.update(|cx| {
            crate::app::init_zmux(cx);
            crate::app::open_zmux_workspace_at(None, base.clone(), cx)
        });
        let opened = open.await.expect("workspace should open");
        let panel = opened.workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<WorkspacesPanel>(cx)
                .expect("workspaces panel should be installed")
        });
        panel.update(cx, |panel, cx| {
            // Keep the test's synthetic terminal contexts stable instead of
            // letting the periodic live-terminal refresh replace them.
            panel._context_refresh_task = Task::ready(());
            let active = panel
                .entries
                .iter_mut()
                .find(|entry| entry.id == panel.active)
                .unwrap();
            active.context.git_roots = vec![shared.clone()];
            active.selected_git_root = Some(shared.clone());
            panel.entries.push(WorkspaceEntry {
                id: 2,
                manual_name: None,
                worktree_name: None,
                worktree_paths: Vec::new(),
                automatic_name: "Second".into(),
                context: WorkspaceContext {
                    git_roots: vec![shared.clone(), unique.clone()],
                    ..WorkspaceContext::default()
                },
                context_authoritative: true,
                incomplete_context_refreshes: 0,
                default_directory: None,
                selected_git_root: Some(unique.clone()),
                git_discovery: GitDiscoveryState::Authoritative,
                git: MetadataState::NotRequested,
                metadata_root: None,
                metadata_refreshed_at: None,
                restore: None,
                failed_restores: Vec::new(),
                stored: Some(StoredLayout::Leaf {
                    items: Vec::new(),
                    active: 0,
                    focused: true,
                }),
            });
            panel.next_id = 3;
            panel.reconcile_git_context(cx);
        });

        for _ in 0..100 {
            cx.run_until_parked();
            if panel.read_with(cx, |panel, _| panel.attached_worktrees.len()) == 2 {
                break;
            }
            cx.background_executor
                .timer(Duration::from_millis(10))
                .await;
        }
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.attached_worktrees.len()),
            2
        );

        opened
            .window
            .update(cx, |_, window, cx| {
                panel.update(cx, |panel, cx| panel.close_workspace(2, window, cx));
            })
            .expect("window should remain open");

        assert_eq!(
            panel.read_with(cx, |panel, _| {
                panel
                    .attached_worktrees
                    .keys()
                    .cloned()
                    .collect::<BTreeSet<_>>()
            }),
            BTreeSet::from([shared.clone()])
        );
        let project_roots = opened.workspace.read_with(cx, |workspace, cx| {
            workspace
                .project()
                .read(cx)
                .worktrees(cx)
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                .collect::<BTreeSet<_>>()
        });
        assert!(project_roots.contains(&shared));
        assert!(!project_roots.contains(&unique));

        let _ = std::fs::remove_dir_all(base);
    }

    fn stored_leaf() -> StoredLayout {
        StoredLayout::Leaf {
            items: Vec::new(),
            active: 0,
            focused: false,
        }
    }

    fn stored_split(
        axis: Axis,
        ratio: f32,
        first: StoredLayout,
        second: StoredLayout,
    ) -> StoredLayout {
        StoredLayout::Split {
            axis,
            ratio,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    /// Render the layout's shape, ignoring the (empty) item lists, so tests can
    /// assert the folded split tree.
    fn shape(layout: &StoredLayout) -> String {
        match layout {
            StoredLayout::Leaf { .. } => "·".to_string(),
            StoredLayout::Split {
                axis,
                first,
                second,
                ..
            } => {
                let axis = if *axis == Axis::Horizontal { "H" } else { "V" };
                format!("{axis}({},{})", shape(first), shape(second))
            }
        }
    }

    #[test]
    fn fold_axis_run_returns_single_child_directly() {
        let folded = fold_axis_run(
            Axis::Horizontal,
            vec![(1.0, stored_leaf())],
            &mut stored_split,
        )
        .expect("one child should fold to itself");
        assert_eq!(shape(&folded), "·");
    }

    #[test]
    fn fold_axis_run_of_nothing_is_none() {
        assert!(fold_axis_run(Axis::Horizontal, Vec::new(), &mut stored_split).is_none());
    }

    #[test]
    fn fold_axis_run_nests_left_to_right_with_weighted_ratios() {
        let folded = fold_axis_run(
            Axis::Horizontal,
            vec![
                (1.0, stored_leaf()),
                (2.0, stored_leaf()),
                (1.0, stored_leaf()),
            ],
            &mut stored_split,
        )
        .expect("three children should fold into nested splits");
        assert_eq!(shape(&folded), "H(·,H(·,·))");
        let StoredLayout::Split { ratio, second, .. } = folded else {
            panic!("expected a split");
        };
        assert!((ratio - 0.25).abs() < 1e-4);
        let StoredLayout::Split { ratio, .. } = *second else {
            panic!("expected a nested split");
        };
        assert!((ratio - 2.0 / 3.0).abs() < 1e-4);
    }

    #[test]
    fn unit_rect_split_partitions_the_axis_span() {
        let (first, second) = UnitRect::FULL.split(Axis::Horizontal, 0.3);
        assert!((first.w - 0.3).abs() < 1e-5);
        assert!((second.x - 0.3).abs() < 1e-5);
        assert!((second.w - 0.7).abs() < 1e-5);
        assert!((first.h - 1.0).abs() < 1e-5 && (second.h - 1.0).abs() < 1e-5);

        let (top, bottom) = UnitRect::FULL.split(Axis::Vertical, 0.25);
        assert!((top.h - 0.25).abs() < 1e-5);
        assert!((bottom.y - 0.25).abs() < 1e-5);
        assert!((bottom.h - 0.75).abs() < 1e-5);

        let reunited = first.union(second);
        assert!((reunited.x - 0.0).abs() < 1e-5 && (reunited.w - 1.0).abs() < 1e-5);
    }

    #[test]
    fn unit_rect_split_spans_follow_the_split_axis() {
        let (first, second) = UnitRect::FULL.split(Axis::Horizontal, 0.4);
        assert!((first.span(Axis::Horizontal) - 0.4).abs() < 1e-5);
        assert!((second.span(Axis::Horizontal) - 0.6).abs() < 1e-5);
        assert!((first.span(Axis::Vertical) - 1.0).abs() < 1e-5);
    }
}
