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
//! machine, and event-driven terminal/context refreshes. The other
//! concerns live in submodules: [`agent_chat`] (agent chat rail state),
//! [`git_context`] (repository discovery/reconciliation), [`panel`]
//! (rendering), and [`persistence`] (layout capture/restore, session writes).

mod agent_chat;
mod git_context;
mod panel;
mod persistence;

pub use self::git_context::{install_git_repository_scope, register_git_repository_scope};
pub(crate) use self::persistence::{RestoredTerminal, restore_startup_layout};

use std::collections::{BTreeSet, HashMap, HashSet};
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
use workspace::item::ItemHandle;

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
    AgentChat, AgentChatState, agent_chat_needs_follow_up, reconcile_agent_chat_for_terminal,
};
use self::git_context::{
    GitDiscoveryState, GitRootRecheck, PathContextCache, WorkspaceContext,
    workspace_context_for_active_workspace, workspace_context_for_stored_layout,
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
const AGENT_REFRESH_INTERVAL: Duration = Duration::from_millis(300);
const EVENT_COALESCE_INTERVAL: Duration = Duration::from_millis(25);
const SESSION_PERSIST_DEBOUNCE: Duration = Duration::from_millis(500);
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct TerminalContextFingerprint {
    working_directory: Option<PathBuf>,
    foreground_process: Option<String>,
}

struct RegisteredTerminal {
    workspace_id: WorkspaceId,
    view: WeakEntity<terminal_view::TerminalView>,
    context: TerminalContextFingerprint,
}

#[derive(Debug, Default)]
struct DeadlineQueue<K> {
    deadlines: HashMap<K, Instant>,
}

impl<K: Copy + Eq + std::hash::Hash> DeadlineQueue<K> {
    fn schedule(&mut self, key: K, deadline: Instant) {
        self.deadlines
            .entry(key)
            .and_modify(|current| *current = (*current).min(deadline))
            .or_insert(deadline);
    }

    fn earliest(&self) -> Option<Instant> {
        self.deadlines.values().copied().min()
    }

    fn take_due(&mut self, now: Instant) -> Vec<K> {
        let due = self
            .deadlines
            .iter()
            .filter_map(|(key, deadline)| (*deadline <= now).then_some(*key))
            .collect::<Vec<_>>();
        for key in &due {
            self.deadlines.remove(key);
        }
        due
    }

    fn remove(&mut self, key: &K) {
        self.deadlines.remove(key);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GitRootRecheckTimerUpdate {
    Keep,
    Cancel,
    Arm(Duration),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GitRootRecheckScheduleUpdate {
    refresh: bool,
    timer: GitRootRecheckTimerUpdate,
}

#[derive(Debug, Default)]
struct GitRootRecheckSchedule {
    pending: HashSet<PathBuf>,
    deadline: Option<Instant>,
}

impl GitRootRecheckSchedule {
    fn observe(
        &mut self,
        directory: PathBuf,
        recheck: GitRootRecheck,
        now: Instant,
    ) -> GitRootRecheckScheduleUpdate {
        let previous_deadline = self.deadline;
        let refresh = self.observe_without_update(directory, recheck, now);
        GitRootRecheckScheduleUpdate {
            refresh,
            timer: Self::timer_update(previous_deadline, self.deadline, now),
        }
    }

    fn observe_without_update(
        &mut self,
        directory: PathBuf,
        recheck: GitRootRecheck,
        now: Instant,
    ) -> bool {
        match recheck {
            GitRootRecheck::Missing | GitRootRecheck::Due => {
                self.remove(&directory);
                true
            }
            GitRootRecheck::Positive => {
                self.remove(&directory);
                false
            }
            GitRootRecheck::Pending(delay) => {
                self.pending.insert(directory);
                let deadline = now + delay;
                if self.deadline.is_none_or(|current| deadline < current) {
                    self.deadline = Some(deadline);
                }
                false
            }
        }
    }

    fn remove(&mut self, directory: &Path) {
        self.pending.remove(directory);
        if self.pending.is_empty() {
            self.deadline = None;
        }
    }

    fn take_firing_paths(&mut self) -> Vec<PathBuf> {
        self.deadline = None;
        self.pending.drain().collect()
    }

    fn reduce_fired(
        &mut self,
        states: impl IntoIterator<Item = (PathBuf, GitRootRecheck)>,
        now: Instant,
    ) -> GitRootRecheckScheduleUpdate {
        debug_assert!(self.pending.is_empty() && self.deadline.is_none());
        let mut refresh = false;
        for (directory, state) in states {
            refresh |= self.observe_without_update(directory, state, now);
        }
        GitRootRecheckScheduleUpdate {
            refresh,
            timer: self
                .deadline
                .map_or(GitRootRecheckTimerUpdate::Cancel, |deadline| {
                    GitRootRecheckTimerUpdate::Arm(
                        deadline.checked_duration_since(now).unwrap_or_default(),
                    )
                }),
        }
    }

    fn clear(&mut self) -> GitRootRecheckTimerUpdate {
        let had_timer = self.deadline.take().is_some();
        self.pending.clear();
        if had_timer {
            GitRootRecheckTimerUpdate::Cancel
        } else {
            GitRootRecheckTimerUpdate::Keep
        }
    }

    fn timer_update(
        previous: Option<Instant>,
        next: Option<Instant>,
        now: Instant,
    ) -> GitRootRecheckTimerUpdate {
        if previous == next {
            GitRootRecheckTimerUpdate::Keep
        } else if let Some(deadline) = next {
            GitRootRecheckTimerUpdate::Arm(deadline.checked_duration_since(now).unwrap_or_default())
        } else {
            GitRootRecheckTimerUpdate::Cancel
        }
    }
}

/// The left dock panel that owns every logical workspace and refreshes derived
/// state only when workspace or terminal events invalidate it.
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
    _workspace_subscription: Subscription,
    _project_subscription: Option<Subscription>,
    terminal_registry: HashMap<EntityId, RegisteredTerminal>,
    dirty_agent_terminals: HashSet<EntityId>,
    agent_refresh_queue: DeadlineQueue<EntityId>,
    agent_refresh_deadline: Option<Instant>,
    context_refresh_task: Option<Task<()>>,
    context_refresh_queue: DeadlineQueue<WorkspaceId>,
    context_refresh_deadline: Option<Instant>,
    git_root_recheck_task: Option<Task<()>>,
    git_root_recheck_schedule: GitRootRecheckSchedule,
    agent_refresh_task: Option<Task<()>>,
    session_persist_task: Option<Task<()>>,
    session_store: SessionStore,
    owns_session: bool,
    session_persistence: SessionPersistence,
    attached_worktrees: HashMap<PathBuf, Entity<project::Worktree>>,
    pending_worktrees: BTreeSet<PathBuf>,
    audited_blocked_roots: BTreeSet<PathBuf>,
    warned_scan_roots: BTreeSet<PathBuf>,
    path_context_cache: std::sync::Mutex<PathContextCache>,
    agent_chats: HashMap<(WorkspaceId, EntityId), AgentChat>,
    next_agent_activity_sequence: u64,
}

impl WorkspacesPanel {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        initial_directory: Option<PathBuf>,
        session_enabled: bool,
        window: &mut Window,
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
                        // Sessions saved before empty panes were pruned at
                        // capture time may still carry them; prune here so the
                        // retry snapshot and its failed-restore paths agree.
                        restore: Some(workspace.layout.without_empty_panes()),
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
                    default_directory: initial_directory,
                    selected_git_root: None,
                    git_discovery: GitDiscoveryState::Authoritative,
                    git: MetadataState::NotRequested,
                    metadata_root: None,
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
        let workspace_entity = workspace
            .upgrade()
            .expect("WorkspacesPanel must be created for a live workspace");
        let workspace_subscription = cx.subscribe_in(
            &workspace_entity,
            window,
            |this, _, event: &workspace::Event, window, cx| {
                this.handle_workspace_event(event, window, cx);
            },
        );
        // Panels can be constructed from inside a Workspace update. Subscribe
        // after that update completes so reading the Project or its Git store
        // does not re-enter the Workspace entity.
        let panel = cx.weak_entity();
        cx.defer(move |cx| {
            panel
                .update(cx, |this, cx| {
                    let project = workspace_entity.read(cx).project().clone();
                    this._project_subscription = Some(cx.subscribe(
                        &project,
                        |this, project, event: &project::Event, cx| {
                            if let project::Event::WorktreeAdded(id) = event {
                                this.schedule_worktree_admission_audit(project.clone(), *id, cx);
                            }
                        },
                    ));
                    Self::subscribe_to_git_metadata(&workspace_entity, cx);
                })
                .ok();
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
            _workspace_subscription: workspace_subscription,
            _project_subscription: None,
            terminal_registry: HashMap::new(),
            dirty_agent_terminals: HashSet::new(),
            agent_refresh_queue: DeadlineQueue::default(),
            agent_refresh_deadline: None,
            context_refresh_task: None,
            context_refresh_queue: DeadlineQueue::default(),
            context_refresh_deadline: None,
            git_root_recheck_task: None,
            git_root_recheck_schedule: GitRootRecheckSchedule::default(),
            agent_refresh_task: None,
            session_persist_task: None,
            session_store,
            owns_session,
            session_persistence: SessionPersistence::new(restored),
            attached_worktrees: HashMap::new(),
            pending_worktrees: BTreeSet::new(),
            audited_blocked_roots: BTreeSet::new(),
            warned_scan_roots: BTreeSet::new(),
            path_context_cache: std::sync::Mutex::new(PathContextCache::default()),
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

    fn handle_workspace_event(
        &mut self,
        event: &workspace::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            workspace::Event::ItemAdded { item } => {
                // The view can still be on GPUI's update stack while ItemAdded
                // is emitted. Register after the complete workspace mutation,
                // when a layout swap's logical owner has also been committed.
                let item = item.boxed_clone();
                cx.defer_in(window, move |this, window, cx| {
                    this.register_terminal(item.as_ref(), window, cx);
                });
            }
            workspace::Event::ItemRemoved { item_id } => {
                let item_id = *item_id;
                cx.defer_in(window, move |this, _window, cx| {
                    this.prune_unmounted_terminal(item_id, cx);
                });
            }
            workspace::Event::ActiveItemChanged => {
                self.invalidate_active_workspace_agents();
                self.schedule_active_agent_refresh(EVENT_COALESCE_INTERVAL, cx);
            }
            workspace::Event::CenterLayoutChanged => self.schedule_session_persistence(cx),
            _ => {}
        }
    }

    fn terminal_context_fingerprint(terminal: &terminal::Terminal) -> TerminalContextFingerprint {
        TerminalContextFingerprint {
            working_directory: terminal.working_directory(),
            foreground_process: terminal.foreground_process_command_name(),
        }
    }

    fn register_terminal(
        &mut self,
        item: &dyn ItemHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = item.act_as::<terminal_view::TerminalView>(cx) else {
            return;
        };
        let item_id = item.item_id();
        let workspace_id = self.workspace_id_for_item(item_id);
        let terminal = view.read(cx).terminal().clone();
        let context = Self::terminal_context_fingerprint(terminal.read(cx));

        if let Some(registered) = self.terminal_registry.get_mut(&item_id) {
            let context_changed = registered.context != context;
            registered.workspace_id = workspace_id;
            registered.view = view.downgrade();
            registered.context = context;
            self.dirty_agent_terminals.insert(item_id);
            self.schedule_agent_refresh_for(item_id, EVENT_COALESCE_INTERVAL, cx);
            if context_changed {
                self.schedule_context_refresh(cx);
            }
            return;
        };

        self.terminal_registry.insert(
            item_id,
            RegisteredTerminal {
                workspace_id,
                view: view.downgrade(),
                context,
            },
        );
        self.dirty_agent_terminals.insert(item_id);
        self.schedule_agent_refresh_for(item_id, EVENT_COALESCE_INTERVAL, cx);
        self.schedule_context_refresh(cx);

        cx.subscribe_in(
            &terminal,
            window,
            move |this, terminal, event: &terminal::Event, _window, cx| {
                this.handle_terminal_event(item_id, terminal, event, cx);
            },
        )
        .detach();

        let panel = cx.weak_entity();
        view.update(cx, |view, view_cx| {
            let focus = view.focus_handle(view_cx);
            view_cx
                .on_focus_in(&focus, window, move |_view, _window, cx| {
                    let _ = panel.update(cx, |panel, cx| {
                        panel.invalidate_active_workspace_agents();
                        panel.schedule_active_agent_refresh(EVENT_COALESCE_INTERVAL, cx);
                    });
                })
                .detach();
        });
    }

    fn handle_terminal_event(
        &mut self,
        item_id: EntityId,
        terminal: &Entity<terminal::Terminal>,
        event: &terminal::Event,
        cx: &mut Context<Self>,
    ) {
        let workspace_id = self
            .terminal_registry
            .get(&item_id)
            .map(|terminal| terminal.workspace_id)
            .unwrap_or(self.active);
        let event_delay = self.event_coalesce_interval(workspace_id, cx);
        if matches!(event, terminal::Event::Wakeup)
            && let Some(directory) = terminal.read(cx).working_directory()
        {
            self.schedule_negative_git_root_recheck(directory, cx);
        }

        if matches!(
            event,
            terminal::Event::Wakeup
                | terminal::Event::TitleChanged
                | terminal::Event::BreadcrumbsChanged
                | terminal::Event::CloseTerminal
        ) {
            self.dirty_agent_terminals.insert(item_id);
            self.schedule_agent_refresh_for(item_id, event_delay, cx);
        }

        let next = Self::terminal_context_fingerprint(terminal.read(cx));
        let context_changed = self
            .terminal_registry
            .get_mut(&item_id)
            .is_some_and(|registered| {
                if registered.context == next {
                    false
                } else {
                    registered.context = next;
                    true
                }
            });
        if context_changed {
            self.schedule_context_refresh_for(workspace_id, event_delay, cx);
        }
    }

    fn event_coalesce_interval(&self, workspace_id: WorkspaceId, cx: &App) -> Duration {
        if terminal_view::visual_power_state(cx).low_power && workspace_id != self.active {
            Duration::from_secs(1)
        } else {
            EVENT_COALESCE_INTERVAL
        }
    }

    /// Negative Git-root entries are cheap to retain while a shell is idle,
    /// but terminal output is a useful signal that a command such as
    /// `git init` may have changed the answer. Low-power support can widen
    /// this policy interval without changing the cache's event-driven shape.
    fn negative_git_root_recheck_interval(&self, cx: &App) -> Duration {
        // Low Power Mode deliberately trades repository-discovery latency for
        // fewer filesystem wakeups while terminal output is active.
        if terminal_view::visual_power_state(cx).low_power {
            Duration::from_secs(5)
        } else {
            Duration::from_secs(1)
        }
    }

    fn schedule_negative_git_root_recheck(&mut self, directory: PathBuf, cx: &mut Context<Self>) {
        let interval = self.negative_git_root_recheck_interval(cx);
        let recheck = self
            .path_context_cache
            .lock()
            .expect("path context cache poisoned")
            .git_root_recheck(&directory, interval);
        let update = self
            .git_root_recheck_schedule
            .observe(directory, recheck, Instant::now());
        self.apply_git_root_recheck_schedule(update, cx);
    }

    fn clear_pending_git_root_rechecks(&mut self) {
        self.git_root_recheck_schedule.clear();
        self.git_root_recheck_task.take();
    }

    fn apply_git_root_recheck_schedule(
        &mut self,
        update: GitRootRecheckScheduleUpdate,
        cx: &mut Context<Self>,
    ) {
        if update.refresh {
            self.schedule_context_refresh(cx);
        }
        let GitRootRecheckTimerUpdate::Arm(delay) = update.timer else {
            if matches!(update.timer, GitRootRecheckTimerUpdate::Cancel) {
                self.git_root_recheck_task.take();
            }
            return;
        };
        self.git_root_recheck_task.take();
        self.git_root_recheck_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |this, cx| {
                this.git_root_recheck_task.take();
                let pending = this.git_root_recheck_schedule.take_firing_paths();
                let interval = this.negative_git_root_recheck_interval(cx);
                let states = {
                    let mut cache = this
                        .path_context_cache
                        .lock()
                        .expect("path context cache poisoned");
                    pending
                        .into_iter()
                        .map(|directory| {
                            let state = cache.git_root_recheck(&directory, interval);
                            (directory, state)
                        })
                        .collect::<Vec<_>>()
                };
                let update = this
                    .git_root_recheck_schedule
                    .reduce_fired(states, Instant::now());
                this.apply_git_root_recheck_schedule(update, cx);
            })
            .ok();
        }));
    }

    fn invalidate_active_workspace_agents(&mut self) {
        self.dirty_agent_terminals
            .extend(
                self.terminal_registry
                    .iter()
                    .filter_map(|(item_id, terminal)| {
                        (terminal.workspace_id == self.active).then_some(*item_id)
                    }),
            );
    }

    fn schedule_agent_refresh(&mut self, delay: Duration, cx: &mut Context<Self>) {
        let deadline = Instant::now() + delay;
        for item_id in &self.dirty_agent_terminals {
            if !self.agent_refresh_queue.deadlines.contains_key(item_id) {
                self.agent_refresh_queue.schedule(*item_id, deadline);
            }
        }
        self.arm_agent_refresh(cx);
    }

    fn schedule_agent_refresh_for(
        &mut self,
        item_id: EntityId,
        delay: Duration,
        cx: &mut Context<Self>,
    ) {
        self.dirty_agent_terminals.insert(item_id);
        self.agent_refresh_queue
            .schedule(item_id, Instant::now() + delay);
        self.arm_agent_refresh(cx);
    }

    fn schedule_active_agent_refresh(&mut self, delay: Duration, cx: &mut Context<Self>) {
        let deadline = Instant::now() + delay;
        for (item_id, terminal) in &self.terminal_registry {
            if terminal.workspace_id == self.active {
                self.dirty_agent_terminals.insert(*item_id);
                self.agent_refresh_queue.schedule(*item_id, deadline);
            }
        }
        self.arm_agent_refresh(cx);
    }

    fn arm_agent_refresh(&mut self, cx: &mut Context<Self>) {
        let Some(next_deadline) = self.agent_refresh_queue.earliest() else {
            return;
        };
        if self
            .agent_refresh_deadline
            .is_some_and(|current| current <= next_deadline)
        {
            return;
        }
        self.agent_refresh_task.take();
        self.agent_refresh_deadline = Some(next_deadline);
        let delay = next_deadline.saturating_duration_since(Instant::now());
        self.agent_refresh_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |this, cx| {
                this.agent_refresh_task.take();
                this.agent_refresh_deadline.take();
                let due = this.agent_refresh_queue.take_due(Instant::now());
                this.refresh_dirty_agent_chats(due, cx);
                this.schedule_agent_refresh(Duration::ZERO, cx);
            })
            .ok();
        }));
    }

    fn refresh_dirty_agent_chats(&mut self, dirty: Vec<EntityId>, cx: &mut Context<Self>) {
        let active_item_id = self
            .workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).active_item(cx))
            .map(|item| item.item_id());
        let mut changed = false;
        let mut follow_up = Vec::new();
        let mut released = Vec::new();

        for item_id in dirty {
            self.dirty_agent_terminals.remove(&item_id);
            let Some(registered) = self.terminal_registry.get(&item_id) else {
                continue;
            };
            let workspace_id = registered.workspace_id;
            let Some(view) = registered.view.upgrade() else {
                released.push(item_id);
                continue;
            };
            changed |= reconcile_agent_chat_for_terminal(
                &mut self.agent_chats,
                &mut self.next_agent_activity_sequence,
                workspace_id,
                item_id,
                &view,
                workspace_id == self.active && active_item_id == Some(item_id),
                cx,
            );
            if self
                .agent_chats
                .get(&(workspace_id, item_id))
                .is_some_and(agent_chat_needs_follow_up)
            {
                follow_up.push(item_id);
            }
        }

        for item_id in released {
            self.remove_terminal_registration(item_id, cx);
        }
        if !follow_up.is_empty() {
            self.dirty_agent_terminals.extend(follow_up);
            self.schedule_agent_refresh(AGENT_REFRESH_INTERVAL, cx);
        }
        if changed {
            cx.notify();
        }
    }

    fn schedule_context_refresh(&mut self, cx: &mut Context<Self>) {
        self.schedule_context_refresh_after(EVENT_COALESCE_INTERVAL, cx);
    }

    fn schedule_context_refresh_after(&mut self, delay: Duration, cx: &mut Context<Self>) {
        let now = Instant::now();
        let low_power = terminal_view::visual_power_state(cx).low_power;
        for entry in &self.entries {
            let effective_delay = if low_power && entry.id != self.active {
                delay.max(Duration::from_secs(1))
            } else {
                delay
            };
            self.context_refresh_queue
                .schedule(entry.id, now + effective_delay);
        }
        self.arm_context_refresh(cx);
    }

    fn schedule_context_refresh_for(
        &mut self,
        workspace_id: WorkspaceId,
        delay: Duration,
        cx: &mut Context<Self>,
    ) {
        let delay =
            if terminal_view::visual_power_state(cx).low_power && workspace_id != self.active {
                delay.max(Duration::from_secs(1))
            } else {
                delay
            };
        self.context_refresh_queue
            .schedule(workspace_id, Instant::now() + delay);
        self.arm_context_refresh(cx);
    }

    fn arm_context_refresh(&mut self, cx: &mut Context<Self>) {
        let Some(next_deadline) = self.context_refresh_queue.earliest() else {
            return;
        };
        if self
            .context_refresh_deadline
            .is_some_and(|current| current <= next_deadline)
        {
            return;
        }
        self.context_refresh_task.take();
        self.context_refresh_deadline = Some(next_deadline);
        let delay = next_deadline.saturating_duration_since(Instant::now());
        self.context_refresh_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |this, cx| {
                this.context_refresh_task.take();
                this.context_refresh_deadline.take();
                let due = this.context_refresh_queue.take_due(Instant::now());
                this.refresh_workspace_contexts_for(&due, cx);
                this.request_metadata_refreshes(cx);
                this.arm_context_refresh(cx);
            })
            .ok();
        }));
    }

    fn prune_unmounted_terminal(&mut self, item_id: EntityId, cx: &mut Context<Self>) {
        let mounted_in_active = self.workspace.upgrade().is_some_and(|workspace| {
            workspace
                .read(cx)
                .panes()
                .iter()
                .any(|pane| pane.read(cx).items().any(|item| item.item_id() == item_id))
        });
        let parked = self.entries.iter().any(|entry| {
            entry
                .stored
                .as_ref()
                .is_some_and(|layout| stored_layout_contains_item(layout, item_id))
        });
        if mounted_in_active || parked {
            return;
        }
        if self.remove_terminal_registration(item_id, cx) {
            self.schedule_context_refresh(cx);
            cx.notify();
        }
    }

    fn remove_terminal_registration(&mut self, item_id: EntityId, cx: &mut Context<Self>) -> bool {
        let Some(registered) = self.terminal_registry.remove(&item_id) else {
            return false;
        };
        self.dirty_agent_terminals.remove(&item_id);
        self.agent_refresh_queue.remove(&item_id);
        self.agent_refresh_task.take();
        self.agent_refresh_deadline.take();
        self.arm_agent_refresh(cx);
        self.agent_chats.remove(&(registered.workspace_id, item_id));
        true
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

    /// Treat terminal directory links as logical workspace navigation. The
    /// directory remains a shell cwd; it is never promoted to a recursive Zed
    /// worktree merely because the user clicked it.
    pub(crate) fn open_directory_workspace(
        &mut self,
        directory: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self.workspace_id_for_directory(&directory) {
            self.activate_workspace(id, window, cx);
        } else {
            self.create_workspace_at(Some(directory), None, window, cx);
        }
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
        self.schedule_session_persistence(cx);
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
        self.context_refresh_queue.remove(&id);
        self.context_refresh_task.take();
        self.context_refresh_deadline.take();
        self.arm_context_refresh(cx);
        self.agent_chats
            .retain(|(workspace_id, _), _| *workspace_id != id);
        self.reconcile_git_context(cx);
        self.request_metadata_refreshes(cx);
        if self.notification_filter == Some(id) {
            self.notification_filter = None;
        }
        NotificationRuntime::clear_workspace(cx.entity_id(), id, cx);
        self.schedule_session_persistence(cx);
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
        self.schedule_session_persistence(cx);
        cx.notify();
    }

    fn refresh_workspace_contexts(&mut self, cx: &mut Context<Self>) {
        let workspace_ids = self
            .entries
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>();
        for id in &workspace_ids {
            self.context_refresh_queue.remove(id);
        }
        if self.context_refresh_queue.earliest().is_none() {
            self.context_refresh_task.take();
            self.context_refresh_deadline.take();
        }
        self.refresh_workspace_contexts_for(&workspace_ids, cx);
    }

    fn refresh_workspace_contexts_for(
        &mut self,
        workspace_ids: &[WorkspaceId],
        cx: &mut Context<Self>,
    ) {
        let workspace_ids = workspace_ids.iter().copied().collect::<HashSet<_>>();
        let active_context = workspace_ids
            .contains(&self.active)
            .then(|| {
                self.workspace.upgrade().map(|workspace| {
                    workspace_context_for_active_workspace(
                        workspace.read(cx),
                        &self.path_context_cache,
                        cx,
                    )
                })
            })
            .flatten();
        let mut changed = false;

        for entry in &mut self.entries {
            if !workspace_ids.contains(&entry.id) {
                continue;
            }
            let observed = if entry.id == self.active {
                active_context.clone().unwrap_or_default()
            } else {
                entry
                    .stored
                    .as_ref()
                    .map(|layout| {
                        workspace_context_for_stored_layout(layout, &self.path_context_cache, cx)
                    })
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
        if self
            .entries
            .iter()
            .any(|entry| workspace_ids.contains(&entry.id) && !entry.context_authoritative)
        {
            let incomplete = self
                .entries
                .iter()
                .filter_map(|entry| {
                    (workspace_ids.contains(&entry.id) && !entry.context_authoritative)
                        .then_some(entry.id)
                })
                .collect::<Vec<_>>();
            for id in incomplete {
                self.schedule_context_refresh_for(id, AGENT_REFRESH_INTERVAL, cx);
            }
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
        self.schedule_session_persistence(cx);
        cx.notify();
    }

    fn use_automatic_name(&mut self, id: WorkspaceId, cx: &mut Context<Self>) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) {
            entry.manual_name = None;
            self.schedule_session_persistence(cx);
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
            blocked_git_roots: Vec::new(),
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
    fn git_root_recheck_reducer_covers_scheduling_cancellation_and_mixed_fire() {
        let now = Instant::now();
        let first = PathBuf::from("/outside/first");
        let second = PathBuf::from("/outside/second");
        let mut schedule = GitRootRecheckSchedule::default();

        let first_update = schedule.observe(
            first.clone(),
            GitRootRecheck::Pending(Duration::from_secs(1)),
            now,
        );
        assert_eq!(
            first_update.timer,
            GitRootRecheckTimerUpdate::Arm(Duration::from_secs(1))
        );
        assert_eq!(
            schedule
                .observe(
                    first.clone(),
                    GitRootRecheck::Pending(Duration::from_secs(1)),
                    now,
                )
                .timer,
            GitRootRecheckTimerUpdate::Keep
        );
        assert_eq!(
            schedule
                .observe(
                    second.clone(),
                    GitRootRecheck::Pending(Duration::from_millis(500)),
                    now,
                )
                .timer,
            GitRootRecheckTimerUpdate::Arm(Duration::from_millis(500))
        );

        assert_eq!(
            schedule.observe(second.clone(), GitRootRecheck::Positive, now),
            GitRootRecheckScheduleUpdate {
                refresh: false,
                timer: GitRootRecheckTimerUpdate::Keep,
            }
        );
        assert_eq!(
            schedule.observe(first.clone(), GitRootRecheck::Due, now),
            GitRootRecheckScheduleUpdate {
                refresh: true,
                timer: GitRootRecheckTimerUpdate::Cancel,
            }
        );
        assert_eq!(schedule.clear(), GitRootRecheckTimerUpdate::Keep);

        schedule.observe(
            first.clone(),
            GitRootRecheck::Pending(Duration::from_secs(1)),
            now,
        );
        schedule.observe(
            second.clone(),
            GitRootRecheck::Pending(Duration::from_secs(2)),
            now,
        );
        assert_eq!(schedule.take_firing_paths().len(), 2);
        assert_eq!(
            schedule.reduce_fired(
                [
                    (first, GitRootRecheck::Due),
                    (
                        second.clone(),
                        GitRootRecheck::Pending(Duration::from_secs(1)),
                    ),
                ],
                now + Duration::from_secs(1),
            ),
            GitRootRecheckScheduleUpdate {
                refresh: true,
                timer: GitRootRecheckTimerUpdate::Arm(Duration::from_secs(1)),
            }
        );
        assert_eq!(schedule.pending, HashSet::from([second]));
        assert_eq!(schedule.clear(), GitRootRecheckTimerUpdate::Cancel);
        assert!(schedule.pending.is_empty());
    }

    #[test]
    fn deadline_queue_keeps_inactive_work_behind_active_preemption_and_settles() {
        let now = Instant::now();
        let mut queue = DeadlineQueue::default();
        queue.schedule(2_u64, now + Duration::from_secs(1));
        queue.schedule(1_u64, now + EVENT_COALESCE_INTERVAL);

        assert_eq!(queue.earliest(), Some(now + EVENT_COALESCE_INTERVAL));
        assert_eq!(queue.take_due(now + EVENT_COALESCE_INTERVAL), vec![1_u64]);
        assert_eq!(queue.earliest(), Some(now + Duration::from_secs(1)));
        assert!(queue.take_due(now + Duration::from_millis(999)).is_empty());
        assert_eq!(queue.take_due(now + Duration::from_secs(1)), vec![2_u64]);
        assert_eq!(queue.earliest(), None, "settled queues must stay quiescent");

        let mut promoted_same_agent = DeadlineQueue::default();
        promoted_same_agent.schedule(9_u64, now + Duration::from_secs(1));
        promoted_same_agent.schedule(9_u64, now + EVENT_COALESCE_INTERVAL);
        assert_eq!(
            promoted_same_agent.earliest(),
            Some(now + EVENT_COALESCE_INTERVAL),
            "the same terminal becoming active must promote its existing deadline"
        );

        let mut existing_contexts = DeadlineQueue::default();
        existing_contexts.schedule(1_u64, now + Duration::from_secs(1));
        existing_contexts.schedule(2_u64, now + Duration::from_secs(1));
        existing_contexts.schedule(1_u64, now + EVENT_COALESCE_INTERVAL);
        existing_contexts.schedule(2_u64, now + Duration::from_secs(1));
        assert_eq!(
            existing_contexts.take_due(now + EVENT_COALESCE_INTERVAL),
            vec![1_u64]
        );
        assert_eq!(
            existing_contexts.earliest(),
            Some(now + Duration::from_secs(1))
        );

        let mut removed_earliest = DeadlineQueue::default();
        removed_earliest.schedule(1_u64, now + EVENT_COALESCE_INTERVAL);
        removed_earliest.schedule(2_u64, now + Duration::from_secs(1));
        removed_earliest.remove(&1);
        assert_eq!(
            removed_earliest.earliest(),
            Some(now + Duration::from_secs(1)),
            "removing the armed key must expose the next real deadline"
        );
        removed_earliest.remove(&2);
        assert_eq!(removed_earliest.earliest(), None);
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
