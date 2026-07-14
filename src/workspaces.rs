//! Workspaces: a left sidebar that lets you keep several independent terminal
//! layouts alive at once and switch between them instantly.
//!
//! Each workspace owns its open terminal tabs *and* their split layout. The
//! active workspace lives directly in the [`Workspace`] center; inactive
//! workspaces are detached and parked in [`StoredLayout`] values that keep the
//! terminal entities (and therefore their PTYs) alive. Switching is a pure
//! detach/reattach of live entities — no PTY restart, no serialization — so it
//! stays snappy regardless of how many terminals are open.

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use editor::{Editor, EditorEvent};
use gpui::{
    App, Axis, Bounds, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable, FontWeight,
    Global, IntoElement, KeyDownEvent, Pixels, Render, SharedString, Subscription, Task, TaskExt,
    WeakEntity, Window, actions, div, point, px, size,
};
use terminal_view::TerminalView;
use ui::prelude::*;
use ui::{IconButtonShape, Indicator, Tooltip};
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::item::ItemHandle;
use workspace::{Pane, SplitDirection, Workspace};

use crate::app::{create_center_terminal_for_workspace, create_restored_terminals_for_workspace};
use crate::metadata::{GitMetadata, MetadataState, collect_git_metadata};
use crate::notification_runtime::NotificationRuntime;
use crate::notifications::{Notification, NotificationStore, WorkspaceId};
use crate::session::{
    LayoutAxis, LayoutNodeSnapshot, LayoutSnapshot, SESSION_VERSION, SessionSnapshot, SessionStore,
    TerminalSnapshot, WorkspaceSnapshot,
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
    automatic_name: String,
    context: WorkspaceContext,
    default_directory: Option<PathBuf>,
    selected_git_root: Option<PathBuf>,
    git: MetadataState<GitMetadata>,
    metadata_root: Option<PathBuf>,
    metadata_refreshed_at: Option<Instant>,
    /// A persisted layout that has not been materialized into fresh terminals yet.
    restore: Option<LayoutSnapshot>,
    /// `Some` while the workspace is parked in the background, `None` while it is
    /// the active workspace displayed in the center.
    stored: Option<StoredLayout>,
}

#[derive(Default)]
struct SessionOwnerClaimed(bool);

impl Global for SessionOwnerClaimed {}

pub(crate) struct RestoredTerminal {
    pub(crate) pane: Entity<Pane>,
    pub(crate) working_directory: Option<PathBuf>,
    pub(crate) activate: bool,
}

struct PendingRatio {
    first: Entity<Pane>,
    axis: Axis,
    ratio: f32,
}

impl WorkspaceEntry {
    fn display_name(&self) -> &str {
        self.manual_name
            .as_deref()
            .unwrap_or(self.automatic_name.as_str())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct WorkspaceContext {
    working_directories: Vec<PathBuf>,
    git_roots: Vec<PathBuf>,
    git_root: Option<PathBuf>,
    foreground_processes: Vec<String>,
    shell_count: usize,
}

#[derive(Clone)]
struct WorkspaceRow {
    id: WorkspaceId,
    name: String,
    uses_manual_name: bool,
    context: WorkspaceContext,
    git: MetadataState<GitMetadata>,
    latest_unread: Option<String>,
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
    last_session_snapshot: Option<SessionSnapshot>,
    attached_worktrees: HashMap<PathBuf, Entity<project::Worktree>>,
    pending_worktrees: BTreeSet<PathBuf>,
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
                        automatic_name: "New workspace".to_string(),
                        context: WorkspaceContext::default(),
                        default_directory: workspace.default_directory.clone(),
                        selected_git_root: workspace.selected_git_root.clone(),
                        git: MetadataState::NotRequested,
                        metadata_root: None,
                        metadata_refreshed_at: None,
                        restore: Some(workspace.layout.clone()),
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
                    automatic_name: "New workspace".to_string(),
                    context: WorkspaceContext::default(),
                    default_directory: None,
                    selected_git_root: None,
                    git: MetadataState::NotRequested,
                    metadata_root: None,
                    metadata_refreshed_at: None,
                    restore: None,
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
            last_session_snapshot: restored,
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

    pub(crate) fn take_initial_restore(&mut self) -> Option<LayoutSnapshot> {
        self.entries
            .iter_mut()
            .find(|entry| entry.id == self.active)
            .and_then(|entry| entry.restore.take())
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
        self.create_workspace_at(None, window, cx);
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
                this.create_workspace_at(Some(directory), window, cx);
            })
            .ok();
            Some(())
        })
        .detach();
    }

    fn create_workspace_at(
        &mut self,
        default_directory: Option<PathBuf>,
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
            automatic_name: "New workspace".to_string(),
            context: WorkspaceContext::default(),
            default_directory,
            selected_git_root: None,
            git: MetadataState::NotRequested,
            metadata_root: None,
            metadata_refreshed_at: None,
            restore: None,
            stored: None,
        });

        self.activate_workspace(id, window, cx);
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
        let target_layout = self
            .entries
            .iter_mut()
            .find(|entry| entry.id == id)
            .and_then(|entry| entry.stored.take());
        let target_restore = self
            .entries
            .iter_mut()
            .find(|entry| entry.id == id)
            .and_then(|entry| entry.restore.take());
        let target_default_directory = self.default_directory_for(id);

        let (captured, restored_terminals) = workspace.update(cx, |workspace, cx| {
            let captured = capture_layout(workspace, cx);
            clear_center(workspace, window, cx);
            let mut restored_terminals = Vec::new();

            let target_pane = workspace.active_pane().clone();
            match (target_layout, target_restore) {
                (Some(layout), _) => {
                    let mut pending_ratios = Vec::new();
                    let focused = restore_layout(
                        workspace,
                        target_pane,
                        layout,
                        window,
                        cx,
                        &mut pending_ratios,
                    );
                    if let Some(focused) = focused {
                        window.focus(&focused.focus_handle(cx), cx);
                    }
                    schedule_ratio_restores(workspace, pending_ratios, window, cx);
                    // A new workspace can be parked while its asynchronous
                    // first shell is still spawning. That stale completion is
                    // correctly rejected by the explicit workspace/pane
                    // guard, but the parked snapshot then contains only the
                    // Welcome item. Retry when that snapshot is activated so
                    // `Some(layout)` cannot become permanently terminal-less.
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
                (None, Some(layout)) => {
                    let mut pending_ratios = Vec::new();
                    let focused = restore_snapshot_layout(
                        workspace,
                        target_pane,
                        &layout,
                        window,
                        cx,
                        &mut restored_terminals,
                        &mut pending_ratios,
                    );
                    if let Some(focused) = focused {
                        window.focus(&focused.focus_handle(cx), cx);
                    }
                    schedule_ratio_restores(workspace, pending_ratios, window, cx);
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
            (captured, restored_terminals)
        });

        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == previous) {
            entry.stored = Some(captured);
        }
        self.active = id;
        if !restored_terminals.is_empty() {
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
            let context = if entry.id == self.active {
                active_context.clone().unwrap_or_default()
            } else {
                entry
                    .stored
                    .as_ref()
                    .map(|layout| workspace_context_for_stored_layout(layout, cx))
                    .unwrap_or_default()
            };
            let automatic_name = automatic_workspace_name(&context);
            if entry.context != context || entry.automatic_name != automatic_name {
                entry.context = context;
                entry.automatic_name = automatic_name;
                changed = true;
            }
        }

        if changed {
            cx.notify();
        }
        self.reconcile_git_context(cx);
    }

    fn reconcile_git_context(&mut self, cx: &mut Context<Self>) {
        for entry in &mut self.entries {
            if entry
                .selected_git_root
                .as_ref()
                .is_none_or(|root| !entry.context.git_roots.contains(root))
            {
                entry.selected_git_root = entry.context.git_roots.first().cloned();
            }
        }

        let roots = self
            .entries
            .iter()
            .flat_map(|entry| entry.context.git_roots.iter().cloned())
            .collect::<BTreeSet<_>>();
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let project = workspace.read(cx).project().clone();
        for root in roots {
            if self.attached_worktrees.contains_key(&root)
                || !self.pending_worktrees.insert(root.clone())
            {
                continue;
            }
            let task = project.update(cx, |project, cx| {
                project.find_or_create_worktree(&root, false, cx)
            });
            cx.spawn(async move |this, cx| {
                let result = task.await;
                this.update(cx, |this, cx| {
                    this.pending_worktrees.remove(&root);
                    if let Ok((worktree, _)) = result {
                        this.attached_worktrees.insert(root.clone(), worktree);
                        this.activate_selected_repository(cx);
                    }
                })
                .ok();
            })
            .detach();
        }
        self.activate_selected_repository(cx);
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
            let layout = if entry.id == self.active {
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
            workspaces.push(WorkspaceSnapshot {
                id: entry.id,
                manual_name: entry.manual_name.clone(),
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
        if snapshot.validate().is_err() || self.last_session_snapshot.as_ref() == Some(&snapshot) {
            return;
        }
        self.last_session_snapshot = Some(snapshot.clone());
        let store = self.session_store.clone();
        cx.background_spawn(async move {
            if let Err(error) = store.save(&snapshot) {
                eprintln!("failed to persist zmux session: {error:#}");
            }
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
            .gap_2()
            .overflow_hidden()
            .child(
                Icon::new(IconName::Terminal)
                    .size(IconSize::Small)
                    .color(if is_active {
                        Color::Default
                    } else {
                        Color::Muted
                    }),
            )
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
            })
            .when(unread_count > 0, |this| {
                this.child(
                    div()
                        .id(("ws-unread", id as usize))
                        .px_1()
                        .rounded_md()
                        .bg(cx.theme().colors().element_selected)
                        .cursor_pointer()
                        .tooltip(Tooltip::text("Show this workspace's notifications"))
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            cx.stop_propagation();
                            this.show_workspace_notifications(id, cx);
                        }))
                        .child(
                            Label::new(unread_count.to_string())
                                .size(LabelSize::Small)
                                .color(Color::Accent),
                        ),
                )
            });

        let context = entry.context.clone();
        let shell_label = match context.shell_count {
            0 => "No shells".to_string(),
            1 => "1 shell".to_string(),
            count => format!("{count} shells"),
        };
        let cwd_label = workspace_cwd_label(&context);
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
        let name_area = v_flex()
            .id(("ws-name-area", id as usize))
            .flex_1()
            .gap_0p5()
            .overflow_hidden()
            .child(name_row)
            .child(
                h_flex()
                    .debug_selector(move || format!("WORKSPACE_METADATA-{id}"))
                    .gap_1()
                    .overflow_hidden()
                    .child(
                        Label::new(shell_label)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .single_line(),
                    )
                    .when_some(cwd_label, |this, cwd| {
                        this.child(
                            Label::new(cwd)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .single_line(),
                        )
                    })
                    .when_some(git_label, |this, git| {
                        this.child(
                            div()
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
                    })
                    .children(context.foreground_processes.iter().take(3).map(|process| {
                        div()
                            .px_1()
                            .rounded_sm()
                            .bg(cx.theme().colors().element_background)
                            .child(
                                Label::new(process.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .single_line(),
                            )
                    })),
            )
            .when_some(entry.latest_unread.clone(), |this, latest| {
                this.child(
                    Label::new(latest)
                        .size(LabelSize::Small)
                        .color(Color::Accent)
                        .single_line(),
                )
            })
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
                    latest_unread: store
                        .notifications()
                        .find(|notification| {
                            notification.target.scope_id == scope_id
                                && notification.target.workspace_id == entry.id
                                && !notification.read
                        })
                        .map(|notification| notification.title.clone()),
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

/// Snapshot the current center into a [`StoredLayout`], cloning each item handle
/// so the terminals stay alive after the originals are detached.
fn capture_layout(workspace: &Workspace, cx: &App) -> StoredLayout {
    let mut nodes: Vec<(Bounds<Pixels>, StoredLayout)> = Vec::new();
    for pane in workspace.panes() {
        let pane_ref = pane.read(cx);
        let items: Vec<Box<dyn ItemHandle>> =
            pane_ref.items().map(|item| item.boxed_clone()).collect();
        if items.is_empty() {
            continue;
        }
        let active = pane_ref.active_item_index();
        let bounds = workspace.bounding_box_for_pane(pane).unwrap_or(Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(0.0), px(0.0)),
        });
        nodes.push((
            bounds,
            StoredLayout::Leaf {
                items,
                active,
                focused: pane == workspace.active_pane(),
            },
        ));
    }
    build_tree(nodes)
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

/// Reconstruct a binary split tree from the laid-out pane rectangles using a
/// guillotine partition: repeatedly find a clean horizontal or vertical cut that
/// separates the panes into two groups.
fn build_tree(nodes: Vec<(Bounds<Pixels>, StoredLayout)>) -> StoredLayout {
    if nodes.len() <= 1 {
        return nodes
            .into_iter()
            .next()
            .map(|(_, layout)| layout)
            .unwrap_or(StoredLayout::Leaf {
                items: Vec::new(),
                active: 0,
                focused: true,
            });
    }

    // `horizontal == true` looks for a vertical cut line, producing side-by-side
    // panes (a horizontal axis); `false` looks for a stacked split.
    for horizontal in [true, false] {
        if let Some(left_indices) = try_cut(&nodes, horizontal) {
            let axis = if horizontal {
                Axis::Horizontal
            } else {
                Axis::Vertical
            };
            let ratio = ratio_for_cut(&nodes, &left_indices, horizontal);
            let mut first = Vec::new();
            let mut second = Vec::new();
            for (index, node) in nodes.into_iter().enumerate() {
                if left_indices.contains(&index) {
                    first.push(node);
                } else {
                    second.push(node);
                }
            }
            return StoredLayout::Split {
                axis,
                ratio,
                first: Box::new(build_tree(first)),
                second: Box::new(build_tree(second)),
            };
        }
    }

    // No clean cut (shouldn't happen for guillotine layouts) — flatten the
    // terminals into a single tab strip so nothing is lost.
    let mut items = Vec::new();
    for (_, layout) in nodes {
        collect_items(layout, &mut items);
    }
    StoredLayout::Leaf {
        items,
        active: 0,
        focused: true,
    }
}

fn coord_lo(bounds: &Bounds<Pixels>, horizontal: bool) -> f32 {
    if horizontal {
        f32::from(bounds.origin.x)
    } else {
        f32::from(bounds.origin.y)
    }
}

fn coord_hi(bounds: &Bounds<Pixels>, horizontal: bool) -> f32 {
    if horizontal {
        f32::from(bounds.origin.x + bounds.size.width)
    } else {
        f32::from(bounds.origin.y + bounds.size.height)
    }
}

/// Find the leftmost/topmost clean cut and return the indices of the panes that
/// fall before it. Returns `None` if no cut cleanly separates the panes.
fn try_cut<T>(nodes: &[(Bounds<Pixels>, T)], horizontal: bool) -> Option<Vec<usize>> {
    const EPS: f32 = 1.0;
    let mut cuts: Vec<f32> = nodes
        .iter()
        .map(|(bounds, _)| coord_hi(bounds, horizontal))
        .collect();
    cuts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));

    for cut in cuts {
        let mut before = Vec::new();
        let mut straddles = false;
        for (index, (bounds, _)) in nodes.iter().enumerate() {
            if coord_hi(bounds, horizontal) <= cut + EPS {
                before.push(index);
            } else if coord_lo(bounds, horizontal) < cut - EPS {
                straddles = true;
                break;
            }
        }
        if !straddles && !before.is_empty() && before.len() < nodes.len() {
            return Some(before);
        }
    }
    None
}

fn ratio_for_cut<T>(
    nodes: &[(Bounds<Pixels>, T)],
    first_indices: &[usize],
    horizontal: bool,
) -> f32 {
    let lo = nodes
        .iter()
        .map(|(bounds, _)| coord_lo(bounds, horizontal))
        .fold(f32::INFINITY, f32::min);
    let hi = nodes
        .iter()
        .map(|(bounds, _)| coord_hi(bounds, horizontal))
        .fold(f32::NEG_INFINITY, f32::max);
    let first_hi = first_indices
        .iter()
        .map(|index| coord_hi(&nodes[*index].0, horizontal))
        .fold(f32::NEG_INFINITY, f32::max);
    let span = hi - lo;
    if !span.is_finite() || span <= 0.0 {
        0.5
    } else {
        ((first_hi - lo) / span).clamp(0.05, 0.95)
    }
}

fn collect_items(layout: StoredLayout, out: &mut Vec<Box<dyn ItemHandle>>) {
    match layout {
        StoredLayout::Leaf { items, .. } => out.extend(items),
        StoredLayout::Split { first, second, .. } => {
            collect_items(*first, out);
            collect_items(*second, out);
        }
    }
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
fn restore_layout(
    workspace: &mut Workspace,
    target: Entity<Pane>,
    layout: StoredLayout,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    pending_ratios: &mut Vec<PendingRatio>,
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
            let focused_first = restore_layout(
                workspace,
                target.clone(),
                *first,
                window,
                cx,
                pending_ratios,
            );
            let focused_second =
                restore_layout(workspace, new_pane, *second, window, cx, pending_ratios);
            pending_ratios.push(PendingRatio {
                first: target,
                axis,
                ratio,
            });
            focused_first.or(focused_second)
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
    let mut nodes = Vec::new();
    for pane in workspace.panes() {
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
        let bounds = workspace.bounding_box_for_pane(pane).unwrap_or(Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(0.0), px(0.0)),
        });
        nodes.push((
            bounds,
            LayoutNodeSnapshot::Leaf {
                tabs,
                active_tab,
                focused: pane == workspace.active_pane(),
            },
        ));
    }
    LayoutSnapshot {
        root: build_snapshot_tree(nodes),
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

fn build_snapshot_tree(nodes: Vec<(Bounds<Pixels>, LayoutNodeSnapshot)>) -> LayoutNodeSnapshot {
    if nodes.len() <= 1 {
        return nodes.into_iter().next().map(|(_, node)| node).unwrap_or(
            LayoutNodeSnapshot::Leaf {
                tabs: Vec::new(),
                active_tab: 0,
                focused: true,
            },
        );
    }
    for horizontal in [true, false] {
        if let Some(first_indices) = try_cut(&nodes, horizontal) {
            let ratio = ratio_for_cut(&nodes, &first_indices, horizontal);
            let mut first = Vec::new();
            let mut second = Vec::new();
            for (index, node) in nodes.into_iter().enumerate() {
                if first_indices.contains(&index) {
                    first.push(node);
                } else {
                    second.push(node);
                }
            }
            return LayoutNodeSnapshot::Split {
                axis: if horizontal {
                    LayoutAxis::Horizontal
                } else {
                    LayoutAxis::Vertical
                },
                ratio,
                first: Box::new(build_snapshot_tree(first)),
                second: Box::new(build_snapshot_tree(second)),
            };
        }
    }

    let focused = nodes.iter().any(|(_, node)| snapshot_node_is_focused(node));
    let mut tabs = Vec::new();
    for (_, node) in nodes {
        collect_snapshot_terminals(node, &mut tabs);
    }
    LayoutNodeSnapshot::Leaf {
        tabs,
        active_tab: 0,
        focused,
    }
}

fn snapshot_node_is_focused(node: &LayoutNodeSnapshot) -> bool {
    match node {
        LayoutNodeSnapshot::Leaf { focused, .. } => *focused,
        LayoutNodeSnapshot::Split { first, second, .. } => {
            snapshot_node_is_focused(first) || snapshot_node_is_focused(second)
        }
    }
}

fn collect_snapshot_terminals(node: LayoutNodeSnapshot, output: &mut Vec<TerminalSnapshot>) {
    match node {
        LayoutNodeSnapshot::Leaf { tabs, .. } => output.extend(tabs),
        LayoutNodeSnapshot::Split { first, second, .. } => {
            collect_snapshot_terminals(*first, output);
            collect_snapshot_terminals(*second, output);
        }
    }
}

fn restore_snapshot_layout(
    workspace: &mut Workspace,
    target: Entity<Pane>,
    layout: &LayoutSnapshot,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    terminals: &mut Vec<RestoredTerminal>,
    pending_ratios: &mut Vec<PendingRatio>,
) -> Option<Entity<Pane>> {
    fn restore_node(
        workspace: &mut Workspace,
        target: Entity<Pane>,
        node: &LayoutNodeSnapshot,
        window: &mut Window,
        cx: &mut Context<Workspace>,
        terminals: &mut Vec<RestoredTerminal>,
        pending_ratios: &mut Vec<PendingRatio>,
    ) -> Option<Entity<Pane>> {
        match node {
            LayoutNodeSnapshot::Leaf {
                tabs,
                active_tab,
                focused,
            } => {
                terminals.extend(tabs.iter().enumerate().map(|(index, terminal)| {
                    RestoredTerminal {
                        pane: target.clone(),
                        working_directory: terminal.working_directory.clone(),
                        activate: index == *active_tab,
                    }
                }));
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
                let focused_first = restore_node(
                    workspace,
                    target.clone(),
                    first,
                    window,
                    cx,
                    terminals,
                    pending_ratios,
                );
                let focused_second = restore_node(
                    workspace,
                    new_pane,
                    second,
                    window,
                    cx,
                    terminals,
                    pending_ratios,
                );
                pending_ratios.push(PendingRatio {
                    first: target,
                    axis,
                    ratio: *ratio,
                });
                focused_first.or(focused_second)
            }
        }
    }

    restore_node(
        workspace,
        target,
        &layout.root,
        window,
        cx,
        terminals,
        pending_ratios,
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
    let mut pending_ratios = Vec::new();
    let focused = restore_snapshot_layout(
        workspace,
        target,
        layout,
        window,
        cx,
        &mut terminals,
        &mut pending_ratios,
    );
    if let Some(focused) = focused {
        window.focus(&focused.focus_handle(cx), cx);
    }
    schedule_ratio_restores(workspace, pending_ratios, window, cx);
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

fn schedule_ratio_restores(
    workspace: &mut Workspace,
    pending_ratios: Vec<PendingRatio>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let workspace = workspace.weak_handle();
    for pending in pending_ratios {
        let workspace = workspace.clone();
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    let Some(bounds) = workspace.bounding_box_for_pane(&pending.first) else {
                        return;
                    };
                    let current = match pending.axis {
                        Axis::Horizontal => f32::from(bounds.size.width),
                        Axis::Vertical => f32::from(bounds.size.height),
                    };
                    if current <= 0.0 {
                        return;
                    }
                    let amount = current * (pending.ratio * 2.0 - 1.0);
                    if amount.abs() < 1.0 {
                        return;
                    }
                    window.focus(&pending.first.focus_handle(cx), cx);
                    workspace.resize_pane(pending.axis, px(amount), window, cx);
                })
                .ok();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn leaf(x: f32, y: f32, w: f32, h: f32) -> (Bounds<Pixels>, StoredLayout) {
        (
            Bounds {
                origin: point(px(x), px(y)),
                size: size(px(w), px(h)),
            },
            StoredLayout::Leaf {
                items: Vec::new(),
                active: 0,
                focused: true,
            },
        )
    }

    /// Render the layout's shape, ignoring the (empty) item lists, so tests can
    /// assert the reconstructed split tree.
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
    fn single_pane_is_a_leaf() {
        assert_eq!(shape(&build_tree(vec![leaf(0.0, 0.0, 100.0, 100.0)])), "·");
    }

    #[test]
    fn empty_layout_is_an_empty_leaf() {
        assert_eq!(shape(&build_tree(Vec::new())), "·");
    }

    #[test]
    fn side_by_side_panes_become_a_horizontal_split() {
        let tree = build_tree(vec![
            leaf(0.0, 0.0, 50.0, 100.0),
            leaf(50.0, 0.0, 50.0, 100.0),
        ]);
        assert_eq!(shape(&tree), "H(·,·)");
    }

    #[test]
    fn stacked_panes_become_a_vertical_split() {
        let tree = build_tree(vec![
            leaf(0.0, 0.0, 100.0, 50.0),
            leaf(0.0, 50.0, 100.0, 50.0),
        ]);
        assert_eq!(shape(&tree), "V(·,·)");
    }

    #[test]
    fn nested_layout_is_reconstructed() {
        // Left column, with the right side split into top/bottom.
        let tree = build_tree(vec![
            leaf(0.0, 0.0, 50.0, 100.0),
            leaf(50.0, 0.0, 50.0, 50.0),
            leaf(50.0, 50.0, 50.0, 50.0),
        ]);
        assert_eq!(shape(&tree), "H(·,V(·,·))");
    }

    #[test]
    fn non_equal_split_ratio_is_captured() {
        let tree = build_tree(vec![
            leaf(0.0, 0.0, 30.0, 100.0),
            leaf(30.0, 0.0, 70.0, 100.0),
        ]);
        let StoredLayout::Split { ratio, .. } = tree else {
            panic!("expected a split");
        };
        assert!((ratio - 0.3).abs() < 0.01);
    }

    #[test]
    fn three_columns_nest_left_to_right() {
        let tree = build_tree(vec![
            leaf(0.0, 0.0, 33.0, 100.0),
            leaf(33.0, 0.0, 33.0, 100.0),
            leaf(66.0, 0.0, 34.0, 100.0),
        ]);
        assert_eq!(shape(&tree), "H(·,H(·,·))");
    }
}
