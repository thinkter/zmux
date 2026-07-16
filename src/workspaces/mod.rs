//! Workspaces: a left sidebar that lets you keep several independent terminal
//! layouts alive at once and switch between them instantly.
//!
//! Each workspace owns its open terminal tabs *and* their split layout. The
//! active workspace lives directly in the [`Workspace`] center; inactive
//! workspaces are detached and parked in [`StoredLayout`] values that keep the
//! terminal entities (and therefore their PTYs) alive. Switching is a pure
//! detach/reattach of live entities — no PTY restart, no serialization — so it
//! stays snappy regardless of how many terminals are open.
//!
//! This file owns the [`WorkspacesPanel`] entry list, activation state
//! machine, and periodic refresh loops (2s context, 300ms agent). The other
//! concerns live in submodules: [`agent_chat`] (agent chat rail state),
//! [`git_context`] (repository discovery/reconciliation), [`panel`]
//! (rendering), and [`persistence`] (layout capture/restore, session writes).

mod agent_chat;
mod git_context;
mod panel;
mod persistence;

pub use self::git_context::{install_git_repository_scope, register_git_repository_scope};
pub(crate) use self::persistence::{RestoredTerminal, restore_startup_layout};

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use editor::{Editor, EditorEvent};
use gpui::{
    App, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable, Subscription, Task,
    TaskExt, WeakEntity, Window, actions,
};
use ui::prelude::*;
use workspace::Workspace;
use workspace::dock::PanelEvent;

use crate::app::{
    create_center_terminal_at_for_workspace, create_center_terminal_for_workspace,
    create_restored_terminals_for_workspace,
};
use crate::metadata::{GitMetadata, MetadataState};
use crate::notification_runtime::NotificationRuntime;
use crate::notifications::{NotificationStore, WorkspaceId};
use crate::session::{LayoutSnapshot, SessionStore};
use crate::welcome::ZmuxWelcome;

use self::agent_chat::{
    AgentChat, AgentChatState, agent_observation_for_active_workspace,
    agent_observation_for_stored_layout, reconcile_agent_chats_for_workspace,
};
use self::git_context::{
    GitDiscoveryState, WorkspaceContext, workspace_context_for_active_workspace,
    workspace_context_for_stored_layout,
};
use self::persistence::{
    FailedRestoreSlot, SessionOwnerClaimed, SessionPersistence, StoredLayout, UnitRect,
    apply_restored_flexes, capture_layout, center_has_provisioned_terminal, clear_center,
    restore_layout, restore_snapshot_layout, stored_layout_contains_item,
};

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

const MAX_WORKSPACE_NAME_CHARS: usize = 64;
const CONTEXT_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const AGENT_REFRESH_INTERVAL: Duration = Duration::from_millis(300);
const MAX_INCOMPLETE_CONTEXT_REFRESHES: u8 = 3;

/// One logical workspace: identity, naming, discovered context, and parked
/// layout. Exactly one entry is active at a time; every other entry keeps its
/// terminals alive through `stored`.
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

/// The left dock panel that owns every logical workspace and drives the
/// periodic context (2s) and agent (300ms) refresh loops.
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
    _agent_refresh_task: Task<()>,
    session_store: SessionStore,
    owns_session: bool,
    session_persistence: SessionPersistence,
    attached_worktrees: HashMap<PathBuf, Entity<project::Worktree>>,
    pending_worktrees: BTreeSet<PathBuf>,
    agent_chats: HashMap<(WorkspaceId, EntityId), AgentChat>,
    next_agent_activity_sequence: u64,
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
        let agent_refresh_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(AGENT_REFRESH_INTERVAL).await;
                if this
                    .update(cx, |this, cx| this.refresh_agent_chats(cx))
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
            _agent_refresh_task: agent_refresh_task,
            session_store,
            owns_session,
            session_persistence: SessionPersistence::new(restored),
            attached_worktrees: HashMap::new(),
            pending_worktrees: BTreeSet::new(),
            agent_chats: HashMap::new(),
            next_agent_activity_sequence: 0,
        }
    }

    pub fn active_workspace_id(&self) -> WorkspaceId {
        self.active
    }

    pub(crate) fn active_workspace_generation(&self) -> u64 {
        self.activation_generation
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
        self.agent_chats
            .retain(|(workspace_id, _), _| *workspace_id != id);
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

    fn refresh_agent_chats(&mut self, cx: &mut Context<Self>) {
        let active_observation = self
            .workspace
            .upgrade()
            .map(|workspace| agent_observation_for_active_workspace(workspace.read(cx), cx));
        let observations = self
            .entries
            .iter()
            .map(|entry| {
                let observed = if entry.id == self.active {
                    active_observation.clone().unwrap_or_default()
                } else {
                    entry
                        .stored
                        .as_ref()
                        .map(|layout| agent_observation_for_stored_layout(layout, cx))
                        .unwrap_or_default()
                };
                (entry.id, observed)
            })
            .collect::<Vec<_>>();

        let mut changed = false;
        for (workspace_id, observed) in observations {
            changed |= reconcile_agent_chats_for_workspace(
                &mut self.agent_chats,
                &mut self.next_agent_activity_sequence,
                workspace_id,
                &observed,
            );
        }
        if changed {
            cx.notify();
        }
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

    fn focus_terminal_item(
        &mut self,
        item_id: EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(chat) = self.agent_chats.get_mut(&(self.active, item_id)) {
            chat.focused = true;
            if chat.state == AgentChatState::Idle {
                chat.seen = true;
            }
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            let Some(pane) = workspace.pane_for_item_id(item_id) else {
                return;
            };
            pane.update(cx, |pane, cx| {
                let ix = pane.items().position(|item| item.item_id() == item_id);
                if let Some(ix) = ix {
                    pane.activate_item(ix, true, true, window, cx);
                }
            });
        });
    }
}

impl Focusable for WorkspacesPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for WorkspacesPanel {}

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
