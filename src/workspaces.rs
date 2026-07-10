//! Workspaces: a left sidebar that lets you keep several independent terminal
//! layouts alive at once and switch between them instantly.
//!
//! Each workspace owns its open terminal tabs *and* their split layout. The
//! active workspace lives directly in the [`Workspace`] center; inactive
//! workspaces are detached and parked in [`StoredLayout`] values that keep the
//! terminal entities (and therefore their PTYs) alive. Switching is a pure
//! detach/reattach of live entities — no PTY restart, no serialization — so it
//! stays snappy regardless of how many terminals are open.

use std::{cmp::Ordering, collections::HashMap, mem, path::PathBuf, time::Duration};

use editor::{Editor, EditorEvent};
use gpui::{
    App, Axis, Bounds, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable, Global,
    IntoElement, KeyDownEvent, Pixels, Render, SharedString, Subscription, TaskExt, WeakEntity,
    Window, actions, div, point, px, size,
};
use ui::prelude::*;
use ui::{IconButtonShape, Indicator, Tooltip};
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::item::ItemHandle;
use workspace::{Pane, SplitDirection, Workspace};

use crate::app::create_center_terminal;
use crate::config::ConfigStore;
use crate::metadata::{
    AgentActivity, MetadataState, WorkspaceMetadata, WorkspaceMetadataStore,
    collect_system_metadata,
};
use crate::notifications::{NotificationStore, WorkspaceId};
use crate::session::{
    LayoutAxis, LayoutNodeSnapshot, Ratio, SessionSnapshot, SessionStore, SurfaceId,
    TerminalSnapshot, WorkspaceIdWatermarkStore, WorkspaceLayoutSnapshot, WorkspaceSnapshot,
};
use crate::welcome::ZmuxWelcome;
use terminal_view::default_working_directory;

actions!(
    zmux,
    [
        NewWorkspace,
        ToggleWorkspacesPanel,
        ActivateNextWorkspace,
        ActivatePreviousWorkspace
    ]
);

const PANEL_WIDTH: f32 = 240.0;

/// A detached snapshot of a workspace's center. It keeps terminal item handles
/// alive while the workspace is in the background, but also records stable
/// surface identifiers and split ratios so a later persisted snapshot can be
/// reconstructed faithfully.
enum StoredLayout {
    Leaf {
        surface_id: SurfaceId,
        items: Vec<Box<dyn ItemHandle>>,
        active: usize,
    },
    Split {
        axis: Axis,
        ratio: Ratio,
        first: Box<StoredLayout>,
        second: Box<StoredLayout>,
    },
}

struct StoredWorkspace {
    layout: StoredLayout,
    active_surface_id: SurfaceId,
}

struct WorkspaceEntry {
    id: WorkspaceId,
    name: String,
    /// `Some` while the workspace is parked in the background, `None` while it is
    /// the active workspace displayed in the center.
    stored: Option<StoredWorkspace>,
    /// A disk snapshot which has not been materialized into live terminals yet.
    /// It is consumed only when the user selects this workspace.
    restore: Option<WorkspaceLayoutSnapshot>,
}

/// The immutable destination captured at terminal-request time. A completed
/// shell is routed back here even if the user changes workspace or pane while
/// its PTY is being created.
#[derive(Clone, Debug)]
pub(crate) struct TerminalTarget {
    workspace_id: WorkspaceId,
    surface_id: SurfaceId,
    destination_pane: WeakEntity<Pane>,
    destination_index: Option<usize>,
    activate_tab: bool,
}

impl TerminalTarget {
    fn new(workspace_id: WorkspaceId, surface_id: SurfaceId, pane: &Entity<Pane>) -> Self {
        Self {
            workspace_id,
            surface_id,
            destination_pane: pane.downgrade(),
            destination_index: None,
            activate_tab: true,
        }
    }

    fn restored(
        workspace_id: WorkspaceId,
        surface_id: SurfaceId,
        destination_index: usize,
        activate_tab: bool,
        pane: &Entity<Pane>,
    ) -> Self {
        Self {
            workspace_id,
            surface_id,
            destination_pane: pane.downgrade(),
            destination_index: Some(destination_index),
            activate_tab,
        }
    }
}

struct RestoredTerminal {
    target: TerminalTarget,
    working_directory: Option<PathBuf>,
}

type StartupRestore = (
    HashMap<EntityId, SurfaceId>,
    Vec<(TerminalTarget, Option<PathBuf>)>,
);

struct PendingRatio {
    first: Entity<Pane>,
    axis: Axis,
    ratio: Ratio,
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
            .px_2()
            .py_1()
            .gap_2()
            .rounded_md()
            .shadow_md()
            .bg(cx.theme().colors().element_selected)
            .child(Icon::new(IconName::Terminal).size(IconSize::Small))
            .child(Label::new(self.name.clone()).size(LabelSize::Small))
    }
}

/// Process-wide allocator for workspace identities.
///
/// Workspace IDs are used by app-global notification and control-plane state,
/// so a fresh window cannot safely start again at one while another window is
/// still alive. The first window seeds this allocator from the persisted
/// session watermark; later windows only allocate new identities from it.
#[derive(Debug)]
struct WorkspaceProcessState {
    next_workspace_id: WorkspaceId,
    session_restore_claimed: bool,
}

impl Global for WorkspaceProcessState {}

impl Default for WorkspaceProcessState {
    fn default() -> Self {
        Self {
            next_workspace_id: 1,
            session_restore_claimed: false,
        }
    }
}

impl WorkspaceProcessState {
    /// Return true only for the panel that is allowed to restore the durable
    /// session. Every later window starts independently.
    fn claim_session_restore(&mut self) -> bool {
        if self.session_restore_claimed {
            false
        } else {
            self.session_restore_claimed = true;
            true
        }
    }

    fn seed_from_persisted_state(
        &mut self,
        snapshot: Option<&SessionSnapshot>,
        watermark: Option<WorkspaceId>,
    ) {
        let session_next_workspace_id = snapshot.map_or(1, |snapshot| snapshot.next_workspace_id);
        self.next_workspace_id = session_next_workspace_id.max(watermark.unwrap_or(1));
    }

    fn allocate_workspace_id(&mut self) -> WorkspaceId {
        let id = self.next_workspace_id;
        self.next_workspace_id = self
            .next_workspace_id
            .checked_add(1)
            .expect("zmux workspace ID space exhausted");
        id
    }

    fn next_workspace_id(&self) -> WorkspaceId {
        self.next_workspace_id
    }
}

fn allocate_process_workspace_id(
    watermark_store: &WorkspaceIdWatermarkStore,
    cx: &mut App,
) -> WorkspaceId {
    let (id, next_workspace_id) = {
        let identities = cx.global_mut::<WorkspaceProcessState>();
        let id = identities.allocate_workspace_id();
        (id, identities.next_workspace_id())
    };
    if let Err(error) = watermark_store.advance(next_workspace_id) {
        // Keep the in-process identity unique even if the disk is temporarily
        // unavailable. A later allocation or session-owner persist retries.
        eprintln!("failed to persist zmux workspace ID watermark: {error:#}");
    }
    id
}

pub struct WorkspacesPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    entries: Vec<WorkspaceEntry>,
    active: WorkspaceId,
    /// Stable identities for the panes in the currently displayed workspace.
    surface_ids: HashMap<EntityId, SurfaceId>,
    /// Live pane routes for the currently displayed surface identities. Weak
    /// handles let closed panes disappear instead of keeping them alive until
    /// an asynchronous shell finishes.
    surface_panes: HashMap<SurfaceId, WeakEntity<Pane>>,
    next_surface_id: SurfaceId,
    rename: Option<RenameState>,
    session_store: SessionStore,
    workspace_id_store: WorkspaceIdWatermarkStore,
    /// Exactly one panel per process owns the durable zmux session. Later
    /// windows start independently and must not overwrite or replay it.
    owns_session_persistence: bool,
    persistence_suspended: bool,
    persistence_scheduled: bool,
    _workspace_observer: Option<Subscription>,
    _metadata_refresh_task: gpui::Task<()>,
}

impl WorkspacesPanel {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let session_store = SessionStore::from_environment();
        let workspace_id_store = WorkspaceIdWatermarkStore::from_environment();
        if !cx.has_global::<WorkspaceProcessState>() {
            cx.set_global(WorkspaceProcessState::default());
        }
        let owns_session_persistence = cx
            .global_mut::<WorkspaceProcessState>()
            .claim_session_restore();
        let (restored, persisted_workspace_id) = if owns_session_persistence {
            let restored = match session_store.load() {
                Ok(restored) => restored,
                Err(error) => {
                    // A malformed, stale, or manually edited file must never make
                    // startup fail or cause a partial restore.
                    eprintln!("ignoring invalid zmux session: {error:#}");
                    None
                }
            };
            let persisted_workspace_id = match workspace_id_store.load() {
                Ok(watermark) => watermark,
                Err(error) => {
                    eprintln!("ignoring invalid zmux workspace ID watermark: {error:#}");
                    None
                }
            };
            (restored, persisted_workspace_id)
        } else {
            // The process already has a session-owning window. Replaying its
            // layout here would duplicate terminals and collide with the
            // workspace identities held by app-global metadata.
            (None, None)
        };
        if owns_session_persistence {
            cx.global_mut::<WorkspaceProcessState>()
                .seed_from_persisted_state(restored.as_ref(), persisted_workspace_id);
        }

        let (entries, active, next_surface_id) = match restored {
            Some(snapshot) => entries_from_snapshot(snapshot),
            None => {
                let id = allocate_process_workspace_id(&workspace_id_store, cx);
                (
                    vec![WorkspaceEntry {
                        id,
                        name: format!("Workspace {id}"),
                        stored: None,
                        restore: None,
                    }],
                    id,
                    1,
                )
            }
        };

        let initial_working_directory = crate::env::current_working_directory();
        for entry in &entries {
            WorkspaceMetadataStore::global_mut(cx)
                .register_workspace(entry.id, initial_working_directory.clone());
        }

        let metadata_refresh_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this
                    .update(cx, |this, cx| {
                        this.request_metadata_refresh(this.active, false, cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        let mut panel = Self {
            workspace,
            focus_handle,
            entries,
            active,
            surface_ids: HashMap::new(),
            next_surface_id,
            rename: None,
            session_store,
            workspace_id_store,
            owns_session_persistence,
            persistence_suspended: false,
            persistence_scheduled: false,
            surface_panes: HashMap::new(),
            _workspace_observer: None,
            _metadata_refresh_task: metadata_refresh_task,
        };
        if let Some(workspace) = panel.workspace.upgrade() {
            panel._workspace_observer = Some(cx.observe(&workspace, |this, _, cx| {
                this.schedule_persist(cx);
            }));
        }
        panel.request_metadata_refresh(active, true, cx);
        panel
    }

    pub fn active_workspace_id(&self) -> WorkspaceId {
        self.active
    }

    /// Consume the active workspace's on-disk layout, if one was loaded at
    /// startup. The app uses this plan to build panes before it starts any fresh
    /// shells, keeping persisted state isolated from Zed's own session state.
    pub(crate) fn take_initial_restore(&mut self) -> Option<WorkspaceLayoutSnapshot> {
        self.entries
            .iter_mut()
            .find(|entry| entry.id == self.active)
            .and_then(|entry| entry.restore.take())
    }

    pub(crate) fn register_initial_surface(&mut self, pane: Entity<Pane>) -> TerminalTarget {
        let surface_id = self.surface_for_pane(pane.entity_id());
        self.surface_panes.insert(surface_id, pane.downgrade());
        TerminalTarget::new(self.active, surface_id, &pane)
    }

    pub(crate) fn install_restored_surfaces(
        &mut self,
        surface_ids: HashMap<EntityId, SurfaceId>,
        panes: Vec<Entity<Pane>>,
    ) {
        self.surface_ids = surface_ids;
        self.refresh_surface_panes(panes);
    }

    pub(crate) fn begin_session_restore(&mut self) {
        self.persistence_suspended = true;
    }

    pub(crate) fn finish_session_restore(&mut self, cx: &mut Context<Self>) {
        self.persistence_suspended = false;
        self.schedule_persist(cx);
    }

    pub(crate) fn active_terminal_target(&mut self, pane: Entity<Pane>) -> TerminalTarget {
        let surface_id = self.surface_for_pane(pane.entity_id());
        self.surface_panes.insert(surface_id, pane.downgrade());
        TerminalTarget::new(self.active, surface_id, &pane)
    }

    /// Attach a completed asynchronous terminal to the logical destination
    /// captured when creation began. It intentionally never consults the
    /// currently focused pane.
    pub(crate) fn attach_terminal(
        &mut self,
        target: TerminalTarget,
        item: Box<dyn ItemHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if target.workspace_id == self.active {
            if let Some(pane) = self.pane_for_target(&target) {
                pane.update(cx, |pane, cx| {
                    if target.activate_tab {
                        pane.add_item(item, false, false, target.destination_index, window, cx);
                    } else {
                        // Restore requests can finish out of order. Insert a
                        // non-active tab without stealing its pane's selection,
                        // and shift the current active index when necessary.
                        let active_before = pane.active_item_index();
                        let insertion_index = target
                            .destination_index
                            .unwrap_or_else(|| pane.items_len())
                            .min(pane.items_len());
                        pane.add_item_inner(
                            item,
                            false,
                            false,
                            false,
                            Some(insertion_index),
                            window,
                            cx,
                        );
                        if insertion_index <= active_before && pane.items_len() > 1 {
                            pane.activate_item(
                                (active_before + 1).min(pane.items_len() - 1),
                                false,
                                false,
                                window,
                                cx,
                            );
                        }
                    }
                });
                self.schedule_persist(cx);
            }
        } else if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.id == target.workspace_id)
            && let Some(stored) = entry.stored.as_mut()
            && stored.layout.insert_item(target, item)
        {
            self.schedule_persist(cx);
        }

        // The workspace was closed while its shell was being created. Dropping
        // the view also drops the new terminal; it must not leak into whatever
        // surface happens to be active now.
    }

    /// Create a fresh, empty workspace and switch to it.
    pub fn create_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = allocate_process_workspace_id(&self.workspace_id_store, cx);
        let name = format!("Workspace {id}");
        self.entries.push(WorkspaceEntry {
            id,
            name,
            stored: None,
            restore: None,
        });
        // Appending is intentional: visible order belongs to the user and must
        // never be rewritten based on opaque IDs.
        WorkspaceMetadataStore::global_mut(cx)
            .register_workspace(id, crate::env::current_working_directory());
        self.activate_workspace(id, window, cx);
        self.request_metadata_refresh(id, true, cx);
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
        let (target_layout, target_restore) = self
            .entries
            .iter_mut()
            .find(|entry| entry.id == id)
            .map(|entry| (entry.stored.take(), entry.restore.take()))
            .unwrap_or((None, None));
        let panel = cx.weak_entity();
        self.persistence_suspended = true;
        let mut surface_ids = mem::take(&mut self.surface_ids);
        let mut pending_terminals = Vec::new();

        let (captured, panes) = workspace.update(cx, |workspace, cx| {
            let captured =
                capture_layout(workspace, cx, &mut surface_ids, &mut self.next_surface_id);
            clear_center(workspace, window, cx);
            surface_ids.clear();

            let target_pane = workspace.active_pane().clone();
            let active_surface_id = match target_layout {
                Some(stored) => {
                    let active_surface_id = stored.active_surface_id;
                    let mut pending_ratios = Vec::new();
                    restore_layout(
                        workspace,
                        target_pane,
                        stored.layout,
                        window,
                        cx,
                        &mut surface_ids,
                        &mut pending_ratios,
                    );
                    focus_surface(workspace, active_surface_id, &surface_ids, window, cx);
                    schedule_ratio_restores(
                        workspace,
                        pending_ratios,
                        active_surface_id,
                        window,
                        cx,
                    );
                    active_surface_id
                }
                None if let Some(layout) = target_restore => {
                    let active_surface_id = layout.active_surface_id;
                    let mut pending_ratios = Vec::new();
                    restore_snapshot_layout(
                        workspace,
                        target_pane,
                        &layout,
                        id,
                        window,
                        cx,
                        &mut surface_ids,
                        &mut pending_terminals,
                        &mut pending_ratios,
                    );
                    focus_surface(workspace, active_surface_id, &surface_ids, window, cx);
                    schedule_ratio_restores(
                        workspace,
                        pending_ratios,
                        active_surface_id,
                        window,
                        cx,
                    );
                    active_surface_id
                }
                None => {
                    let surface_id = allocate_surface_id(&mut self.next_surface_id);
                    surface_ids.insert(target_pane.entity_id(), surface_id);
                    let welcome = cx.new(ZmuxWelcome::new);
                    target_pane.update(cx, |pane, cx| {
                        pane.add_item(Box::new(welcome), true, true, None, window, cx);
                    });
                    pending_terminals.push(RestoredTerminal {
                        target: TerminalTarget::new(id, surface_id, &target_pane),
                        working_directory: default_working_directory(workspace, cx),
                    });
                    surface_id
                }
            };
            focus_surface(workspace, active_surface_id, &surface_ids, window, cx);
            (captured, workspace.panes().to_vec())
        });

        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == previous) {
            entry.stored = Some(captured);
        }
        self.surface_ids = surface_ids;
        self.refresh_surface_panes(panes);
        self.active = id;
        self.persistence_suspended = false;
        for terminal in pending_terminals {
            workspace.update(cx, |workspace, cx| {
                create_center_terminal(
                    workspace,
                    panel.clone(),
                    terminal.target,
                    terminal.working_directory,
                    window,
                    cx,
                )
                .detach_and_log_err(cx);
            });
        }
        self.request_metadata_refresh(id, false, cx);
        cx.notify();
        self.persist_session(cx);
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
            // Move to a visible neighbour first so the center always shows
            // something without perturbing the user-owned order.
            let index = self.entries.iter().position(|entry| entry.id == id);
            let fallback = index.and_then(|index| {
                self.entries
                    .get(index + 1)
                    .or_else(|| {
                        index
                            .checked_sub(1)
                            .and_then(|index| self.entries.get(index))
                    })
                    .map(|entry| entry.id)
            });
            if let Some(fallback) = fallback {
                self.activate_workspace(fallback, window, cx);
            }
        }

        // Dropping the entry drops its `StoredLayout`, releasing the terminals.
        self.entries.retain(|entry| entry.id != id);
        NotificationStore::global_mut(cx).clear_workspace(id);
        WorkspaceMetadataStore::global_mut(cx).remove_workspace(id);
        cx.notify();
        self.persist_session(cx);
    }

    /// Metadata command execution never runs from `render`: it moves to a
    /// background worker, has a short timeout/cancellation token, and only the
    /// current generation may update the app-global snapshot.
    fn request_metadata_refresh(&mut self, id: WorkspaceId, force: bool, cx: &mut Context<Self>) {
        let request = match WorkspaceMetadataStore::global_mut(cx).begin_refresh(id, force) {
            Ok(Some(request)) => request,
            Ok(None) | Err(_) => return,
        };
        let collection_request = request.clone();
        let task = cx.background_spawn(async move { collect_system_metadata(collection_request) });
        cx.spawn(async move |this, cx| {
            let collected = task.await;
            this.update(cx, |_, cx| {
                if WorkspaceMetadataStore::global_mut(cx).finish_refresh(&request, collected) {
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
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
        if !reorder_entries(&mut self.entries, dragged_id, target_id) {
            return;
        }
        cx.notify();
        self.persist_session(cx);
    }

    fn start_rename(&mut self, id: WorkspaceId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.iter().find(|entry| entry.id == id) else {
            return;
        };
        let name = entry.name.clone();
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
        let trimmed = text.trim();
        if !trimmed.is_empty()
            && let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == rename.id)
        {
            entry.name = trimmed.to_string();
        }
        cx.notify();
        self.persist_session(cx);
    }

    fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        if self.rename.take().is_some() {
            cx.notify();
        }
    }

    fn surface_for_pane(&mut self, pane_id: EntityId) -> SurfaceId {
        *self
            .surface_ids
            .entry(pane_id)
            .or_insert_with(|| allocate_surface_id(&mut self.next_surface_id))
    }

    fn refresh_surface_panes(&mut self, panes: Vec<Entity<Pane>>) {
        self.surface_panes = panes
            .into_iter()
            .filter_map(|pane| {
                self.surface_ids
                    .get(&pane.entity_id())
                    .copied()
                    .map(|surface_id| (surface_id, pane.downgrade()))
            })
            .collect();
    }

    fn pane_for_target(&self, target: &TerminalTarget) -> Option<Entity<Pane>> {
        target
            .destination_pane
            .upgrade()
            .filter(|pane| self.surface_ids.get(&pane.entity_id()) == Some(&target.surface_id))
            .or_else(|| {
                self.surface_panes
                    .get(&target.surface_id)
                    .and_then(WeakEntity::upgrade)
                    .filter(|pane| {
                        self.surface_ids.get(&pane.entity_id()) == Some(&target.surface_id)
                    })
            })
    }

    fn persist_session(&mut self, cx: &mut Context<Self>) {
        if !self.owns_session_persistence || self.persistence_suspended {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let workspace = workspace.read(cx);
        let panes = workspace.panes().to_vec();
        let active_layout = capture_layout_snapshot(
            workspace,
            cx,
            &mut self.surface_ids,
            &mut self.next_surface_id,
        );
        self.refresh_surface_panes(panes);
        let workspaces = self
            .entries
            .iter()
            .map(|entry| WorkspaceSnapshot {
                id: entry.id,
                name: entry.name.clone(),
                layout: if entry.id == self.active {
                    active_layout.clone()
                } else if let Some(stored) = &entry.stored {
                    stored.snapshot(cx)
                } else if let Some(restored) = &entry.restore {
                    restored.clone()
                } else {
                    WorkspaceLayoutSnapshot::single_empty(allocate_surface_id(
                        &mut self.next_surface_id,
                    ))
                },
            })
            .collect();
        let next_workspace_id = cx.global::<WorkspaceProcessState>().next_workspace_id();
        let snapshot = SessionSnapshot {
            version: crate::session::SESSION_VERSION,
            // Persist the process-wide watermark, including IDs allocated by
            // independent later windows, so a future process cannot reuse
            // any identity that overlapped with app-global state.
            next_workspace_id,
            active_workspace_id: self.active,
            workspaces,
        };
        if let Err(error) = self.session_store.save(&snapshot) {
            eprintln!("failed to persist zmux session: {error:#}");
        } else if let Err(error) = self.workspace_id_store.advance(next_workspace_id) {
            eprintln!("failed to persist zmux workspace ID watermark: {error:#}");
        }
    }

    fn schedule_persist(&mut self, cx: &mut Context<Self>) {
        if self.persistence_scheduled {
            return;
        }
        self.persistence_scheduled = true;
        let timer = cx.background_executor().timer(Duration::from_millis(1));
        cx.spawn(async move |panel, cx| {
            timer.await;
            panel
                .update(cx, |panel, cx| {
                    panel.persistence_scheduled = false;
                    panel.persist_session(cx);
                })
                .ok();
        })
        .detach();
    }

    fn render_entry(
        &self,
        id: WorkspaceId,
        name: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let is_active = id == self.active;
        let (show_metadata, show_working_directory, show_git_status, show_unread_badges) = {
            let config = ConfigStore::global(cx).config();
            (
                config.sidebar.show_metadata,
                config.sidebar.show_working_directory,
                config.sidebar.show_git_status,
                config.notifications.show_unread_badges,
            )
        };
        let has_unread =
            show_unread_badges && NotificationStore::global(cx).workspace_has_unread(id);
        let metadata_text = show_metadata
            .then(|| WorkspaceMetadataStore::global(cx).snapshot(id))
            .flatten()
            .and_then(|metadata| {
                workspace_row_metadata_text(&metadata, show_working_directory, show_git_status)
            });
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
            .id(("ws-name-row", id as usize))
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
                    Label::new(name.to_string())
                        .size(LabelSize::Small)
                        .color(if is_active {
                            Color::Default
                        } else {
                            Color::Muted
                        })
                        .single_line(),
                ),
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
            })
            .when(has_unread, |this| {
                this.child(Indicator::dot().color(Color::Accent))
            });
        let name_area = v_flex()
            .id(("ws-name", id as usize))
            .flex_1()
            .gap_0p5()
            .overflow_hidden()
            .child(name_row)
            .when_some(metadata_text, |this, text| {
                this.child(
                    Label::new(text)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .single_line(),
                )
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
                    name: name.to_string(),
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
}

impl Focusable for WorkspacesPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for WorkspacesPanel {}

impl Render for WorkspacesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Clone only render metadata so `render_entry` can freely borrow the
        // panel without holding a borrow of the live workspace state.
        let rows: Vec<_> = self
            .entries
            .iter()
            .map(|entry| (entry.id, entry.name.clone()))
            .collect();

        let (show_metadata, show_latest_summary) = {
            let config = ConfigStore::global(cx).config();
            (
                config.sidebar.show_metadata,
                config.notifications.show_latest_summary,
            )
        };
        let active_metadata = show_metadata
            .then(|| WorkspaceMetadataStore::global(cx).snapshot(self.active))
            .flatten();
        let latest = show_latest_summary
            .then(|| NotificationStore::global(cx).latest_unread().cloned())
            .flatten();
        let unread_count = NotificationStore::global(cx).unread_count();

        v_flex()
            .key_context("WorkspacesPanel")
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
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        IconButton::new("new-workspace", IconName::Plus)
                            .shape(IconButtonShape::Square)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("New Workspace"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.create_workspace(window, cx);
                            })),
                    ),
            )
            .child(
                v_flex()
                    .id("workspaces-list")
                    .p_1()
                    .gap_0p5()
                    .overflow_y_scroll()
                    .flex_1()
                    .children(
                        rows.iter()
                            .map(|(id, name)| self.render_entry(*id, name, cx)),
                    ),
            )
            .when_some(active_metadata, |this, metadata| {
                this.child(
                    v_flex()
                        .p_2()
                        .gap_1()
                        .border_t_1()
                        .border_color(cx.theme().colors().border)
                        .child(
                            Label::new("Active workspace context")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        // This is deliberately real text rather than icon-only
                        // metadata, making the same state available to screen
                        // readers and minimal/unsupported backends.
                        .child(
                            Label::new(metadata.accessible_summary())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .line_clamp(4),
                        ),
                )
            })
            .when_some(latest, |this, notification| {
                this.child(
                    v_flex()
                        .p_2()
                        .gap_1()
                        .border_t_1()
                        .border_color(cx.theme().colors().border)
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

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(PANEL_WIDTH)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::ListCollapse)
    }

    fn icon_label(&self, _window: &Window, cx: &App) -> Option<String> {
        if !ConfigStore::global(cx)
            .config()
            .notifications
            .show_unread_badges
        {
            return None;
        }
        let count = NotificationStore::global(cx).unread_count();
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

    fn starts_open(&self, _window: &Window, cx: &App) -> bool {
        ConfigStore::global(cx).config().sidebar.starts_open
    }
}

fn entries_from_snapshot(
    snapshot: SessionSnapshot,
) -> (Vec<WorkspaceEntry>, WorkspaceId, SurfaceId) {
    let next_surface_id = max_surface_id_in_snapshot(&snapshot)
        .checked_add(1)
        .expect("zmux surface ID space exhausted");
    let active = snapshot.active_workspace_id;
    let entries = snapshot
        .workspaces
        .into_iter()
        .map(|workspace| WorkspaceEntry {
            id: workspace.id,
            name: workspace.name,
            stored: None,
            restore: Some(workspace.layout),
        })
        .collect();
    (entries, active, next_surface_id)
}

fn workspace_row_metadata_text(
    metadata: &WorkspaceMetadata,
    show_working_directory: bool,
    show_git_status: bool,
) -> Option<String> {
    let mut details = Vec::new();
    if show_working_directory && let Some(directory) = &metadata.working_directory {
        details.push(directory.display().to_string());
    }
    if show_git_status {
        match &metadata.git {
            MetadataState::Ready(git) => {
                let state = if git.is_clean() {
                    "clean".to_string()
                } else {
                    format!("{} changed", git.dirty_files)
                };
                details.push(format!("{} {state}", git.branch));
            }
            MetadataState::Pending => details.push("git refreshing".to_string()),
            MetadataState::Unavailable(_) | MetadataState::Error(_) => {
                details.push("git unavailable".to_string())
            }
            MetadataState::NotRequested => {}
        }
    }
    if let MetadataState::Ready(ports) = &metadata.listening_ports
        && !ports.is_empty()
    {
        details.push(format!(
            "{} listening port{}",
            ports.len(),
            if ports.len() == 1 { "" } else { "s" }
        ));
    }
    if metadata.agent_activity != AgentActivity::Unknown {
        details.push(metadata.agent_activity.accessible_text().to_string());
    }
    if let Some(status) = metadata.status_pills.values().next() {
        details.push(status.accessible_text());
    }
    if let Some(progress) = metadata.progress.values().next() {
        details.push(format!("{} {}%", progress.label, progress.percent()));
    }
    (!details.is_empty()).then(|| details.join(" · "))
}

fn max_surface_id_in_snapshot(snapshot: &SessionSnapshot) -> SurfaceId {
    snapshot
        .workspaces
        .iter()
        .map(|workspace| max_surface_id_in_node(&workspace.layout.root))
        .max()
        .unwrap_or(0)
}

fn max_surface_id_in_node(node: &LayoutNodeSnapshot) -> SurfaceId {
    match node {
        LayoutNodeSnapshot::Leaf { surface_id, .. } => *surface_id,
        LayoutNodeSnapshot::Split { first, second, .. } => {
            max_surface_id_in_node(first).max(max_surface_id_in_node(second))
        }
    }
}

fn allocate_surface_id(next_surface_id: &mut SurfaceId) -> SurfaceId {
    let id = *next_surface_id;
    *next_surface_id = next_surface_id
        .checked_add(1)
        .expect("zmux surface ID space exhausted");
    id
}

impl StoredWorkspace {
    fn snapshot(&self, cx: &App) -> WorkspaceLayoutSnapshot {
        WorkspaceLayoutSnapshot {
            active_surface_id: self.active_surface_id,
            root: self.layout.snapshot(cx),
        }
    }
}

impl StoredLayout {
    fn insert_item(&mut self, target: TerminalTarget, item: Box<dyn ItemHandle>) -> bool {
        match self {
            Self::Leaf {
                surface_id,
                items,
                active,
            } if *surface_id == target.surface_id => {
                let index = target
                    .destination_index
                    .unwrap_or_else(|| active.saturating_add(1));
                let index = index.min(items.len());
                items.insert(index, item);
                if target.activate_tab {
                    *active = index;
                } else if index <= *active {
                    *active = active.saturating_add(1);
                }
                true
            }
            Self::Split { first, second, .. } => {
                first.insert_item(target.clone(), item.boxed_clone())
                    || second.insert_item(target, item)
            }
            _ => false,
        }
    }

    fn snapshot(&self, cx: &App) -> LayoutNodeSnapshot {
        match self {
            Self::Leaf {
                surface_id,
                items,
                active,
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
                    surface_id: *surface_id,
                    tabs,
                    active_tab,
                }
            }
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => LayoutNodeSnapshot::Split {
                axis: axis_to_snapshot(*axis),
                ratio: *ratio,
                first: Box::new(first.snapshot(cx)),
                second: Box::new(second.snapshot(cx)),
            },
        }
    }
}

fn terminal_snapshot(item: &dyn ItemHandle, cx: &App) -> Option<TerminalSnapshot> {
    let terminal_view = item.act_as::<terminal_view::TerminalView>(cx)?;
    let working_directory = terminal_view
        .read(cx)
        .terminal()
        .read(cx)
        .working_directory();
    Some(TerminalSnapshot::fresh_shell(working_directory))
}

/// Snapshot the current center into a [`StoredWorkspace`], cloning item handles
/// so terminal PTYs survive while the workspace is parked. Empty panes are kept:
/// their geometry is part of a user's layout too.
fn capture_layout(
    workspace: &Workspace,
    cx: &App,
    surface_ids: &mut HashMap<EntityId, SurfaceId>,
    next_surface_id: &mut SurfaceId,
) -> StoredWorkspace {
    let mut nodes = Vec::new();
    for pane in workspace.panes() {
        let surface_id = *surface_ids
            .entry(pane.entity_id())
            .or_insert_with(|| allocate_surface_id(next_surface_id));
        let pane_ref = pane.read(cx);
        let items = pane_ref.items().map(|item| item.boxed_clone()).collect();
        let active = pane_ref.active_item_index();
        let bounds = pane_bounds(workspace, pane);
        nodes.push((
            bounds,
            StoredLayout::Leaf {
                surface_id,
                items,
                active,
            },
        ));
    }
    let fallback = *surface_ids
        .entry(workspace.active_pane().entity_id())
        .or_insert_with(|| allocate_surface_id(next_surface_id));
    let active_surface_id = fallback;
    StoredWorkspace {
        layout: build_tree(nodes, fallback),
        active_surface_id,
    }
}

fn capture_layout_snapshot(
    workspace: &Workspace,
    cx: &App,
    surface_ids: &mut HashMap<EntityId, SurfaceId>,
    next_surface_id: &mut SurfaceId,
) -> WorkspaceLayoutSnapshot {
    let mut nodes = Vec::new();
    for pane in workspace.panes() {
        let surface_id = *surface_ids
            .entry(pane.entity_id())
            .or_insert_with(|| allocate_surface_id(next_surface_id));
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
        nodes.push((
            pane_bounds(workspace, pane),
            LayoutNodeSnapshot::Leaf {
                surface_id,
                tabs,
                active_tab,
            },
        ));
    }
    let active_surface_id = *surface_ids
        .entry(workspace.active_pane().entity_id())
        .or_insert_with(|| allocate_surface_id(next_surface_id));
    WorkspaceLayoutSnapshot {
        active_surface_id,
        root: build_snapshot_tree(nodes, active_surface_id),
    }
}

fn pane_bounds(workspace: &Workspace, pane: &Entity<Pane>) -> Bounds<Pixels> {
    workspace.bounding_box_for_pane(pane).unwrap_or(Bounds {
        origin: point(px(0.0), px(0.0)),
        size: size(px(0.0), px(0.0)),
    })
}

/// Reconstruct a binary split tree from laid-out pane rectangles. The ratios
/// are captured from the cut itself rather than assuming equal halves.
fn build_tree(
    nodes: Vec<(Bounds<Pixels>, StoredLayout)>,
    fallback_surface_id: SurfaceId,
) -> StoredLayout {
    if nodes.len() <= 1 {
        return nodes
            .into_iter()
            .next()
            .map(|(_, layout)| layout)
            .unwrap_or(StoredLayout::Leaf {
                surface_id: fallback_surface_id,
                items: Vec::new(),
                active: 0,
            });
    }

    for horizontal in [true, false] {
        if let Some(first_indices) = try_cut(&nodes, horizontal) {
            let axis = if horizontal {
                Axis::Horizontal
            } else {
                Axis::Vertical
            };
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
            return StoredLayout::Split {
                axis,
                ratio,
                first: Box::new(build_tree(first, fallback_surface_id)),
                second: Box::new(build_tree(second, fallback_surface_id)),
            };
        }
    }

    // A non-guillotine layout is not expected from the pane engine. Preserve
    // every terminal rather than losing tabs if an invalid geometry slips in.
    let mut nodes = nodes.into_iter();
    let (_, first) = nodes.next().expect("more than one layout node");
    let surface_id = first.first_surface_id();
    let mut items = Vec::new();
    collect_items(first, &mut items);
    for (_, layout) in nodes {
        collect_items(layout, &mut items);
    }
    StoredLayout::Leaf {
        surface_id,
        items,
        active: 0,
    }
}

fn build_snapshot_tree(
    nodes: Vec<(Bounds<Pixels>, LayoutNodeSnapshot)>,
    fallback_surface_id: SurfaceId,
) -> LayoutNodeSnapshot {
    if nodes.len() <= 1 {
        return nodes
            .into_iter()
            .next()
            .map(|(_, layout)| layout)
            .unwrap_or_else(|| WorkspaceLayoutSnapshot::single_empty(fallback_surface_id).root);
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
                first: Box::new(build_snapshot_tree(first, fallback_surface_id)),
                second: Box::new(build_snapshot_tree(second, fallback_surface_id)),
            };
        }
    }

    let mut nodes = nodes.into_iter();
    let (_, first) = nodes.next().expect("more than one layout node");
    let surface_id = first.first_surface_id();
    let mut tabs = Vec::new();
    collect_snapshot_tabs(first, &mut tabs);
    for (_, node) in nodes {
        collect_snapshot_tabs(node, &mut tabs);
    }
    LayoutNodeSnapshot::Leaf {
        surface_id,
        tabs,
        active_tab: 0,
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
) -> Ratio {
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
        Ratio::DEFAULT
    } else {
        Ratio::new((first_hi - lo) / span)
    }
}

impl StoredLayout {
    fn first_surface_id(&self) -> SurfaceId {
        match self {
            Self::Leaf { surface_id, .. } => *surface_id,
            Self::Split { first, .. } => first.first_surface_id(),
        }
    }
}

impl LayoutNodeSnapshot {
    fn first_surface_id(&self) -> SurfaceId {
        match self {
            Self::Leaf { surface_id, .. } => *surface_id,
            Self::Split { first, .. } => first.first_surface_id(),
        }
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

fn collect_snapshot_tabs(node: LayoutNodeSnapshot, out: &mut Vec<TerminalSnapshot>) {
    match node {
        LayoutNodeSnapshot::Leaf { tabs, .. } => out.extend(tabs),
        LayoutNodeSnapshot::Split { first, second, .. } => {
            collect_snapshot_tabs(*first, out);
            collect_snapshot_tabs(*second, out);
        }
    }
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

fn restore_layout(
    workspace: &mut Workspace,
    target: Entity<Pane>,
    layout: StoredLayout,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    surface_ids: &mut HashMap<EntityId, SurfaceId>,
    pending_ratios: &mut Vec<PendingRatio>,
) {
    match layout {
        StoredLayout::Leaf {
            surface_id,
            items,
            active,
        } => {
            surface_ids.insert(target.entity_id(), surface_id);
            target.update(cx, |pane, cx| {
                for item in items {
                    pane.add_item(item, false, false, None, window, cx);
                }
                if pane.items_len() > 0 {
                    pane.activate_item(
                        active.min(pane.items_len().saturating_sub(1)),
                        false,
                        false,
                        window,
                        cx,
                    );
                }
            });
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
            restore_layout(
                workspace,
                target.clone(),
                *first,
                window,
                cx,
                surface_ids,
                pending_ratios,
            );
            restore_layout(
                workspace,
                new_pane,
                *second,
                window,
                cx,
                surface_ids,
                pending_ratios,
            );
            pending_ratios.push(PendingRatio {
                first: target,
                axis,
                ratio,
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn restore_snapshot_layout(
    workspace: &mut Workspace,
    target: Entity<Pane>,
    layout: &WorkspaceLayoutSnapshot,
    workspace_id: WorkspaceId,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    surface_ids: &mut HashMap<EntityId, SurfaceId>,
    pending_terminals: &mut Vec<RestoredTerminal>,
    pending_ratios: &mut Vec<PendingRatio>,
) {
    restore_snapshot_node(
        workspace,
        target,
        &layout.root,
        workspace_id,
        window,
        cx,
        surface_ids,
        pending_terminals,
        pending_ratios,
    );
}

/// Materialize the initially selected persisted layout into Zed's otherwise
/// empty center. This is intentionally separate from Zed's own restoration so
/// zmux never imports, migrates, or mutates Zed session state.
pub(crate) fn restore_startup_layout(
    workspace: &mut Workspace,
    layout: WorkspaceLayoutSnapshot,
    workspace_id: WorkspaceId,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> StartupRestore {
    clear_center(workspace, window, cx);
    let target = workspace.active_pane().clone();
    let mut surface_ids = HashMap::new();
    let mut terminals = Vec::new();
    let mut pending_ratios = Vec::new();
    let active_surface_id = layout.active_surface_id;
    restore_snapshot_layout(
        workspace,
        target,
        &layout,
        workspace_id,
        window,
        cx,
        &mut surface_ids,
        &mut terminals,
        &mut pending_ratios,
    );
    focus_surface(workspace, active_surface_id, &surface_ids, window, cx);
    schedule_ratio_restores(workspace, pending_ratios, active_surface_id, window, cx);
    (
        surface_ids,
        terminals
            .into_iter()
            .map(|terminal| (terminal.target, terminal.working_directory))
            .collect(),
    )
}

#[allow(clippy::too_many_arguments)]
fn restore_snapshot_node(
    workspace: &mut Workspace,
    target: Entity<Pane>,
    node: &LayoutNodeSnapshot,
    workspace_id: WorkspaceId,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    surface_ids: &mut HashMap<EntityId, SurfaceId>,
    pending_terminals: &mut Vec<RestoredTerminal>,
    pending_ratios: &mut Vec<PendingRatio>,
) {
    match node {
        LayoutNodeSnapshot::Leaf {
            surface_id,
            tabs,
            active_tab,
        } => {
            surface_ids.insert(target.entity_id(), *surface_id);
            for (index, terminal) in tabs.iter().enumerate() {
                pending_terminals.push(RestoredTerminal {
                    target: TerminalTarget::restored(
                        workspace_id,
                        *surface_id,
                        index,
                        index == *active_tab,
                        &target,
                    ),
                    working_directory: terminal.working_directory.clone(),
                });
            }
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
            restore_snapshot_node(
                workspace,
                target.clone(),
                first,
                workspace_id,
                window,
                cx,
                surface_ids,
                pending_terminals,
                pending_ratios,
            );
            restore_snapshot_node(
                workspace,
                new_pane,
                second,
                workspace_id,
                window,
                cx,
                surface_ids,
                pending_terminals,
                pending_ratios,
            );
            pending_ratios.push(PendingRatio {
                first: target,
                axis,
                ratio: *ratio,
            });
        }
    }
}

fn focus_surface(
    workspace: &mut Workspace,
    surface_id: SurfaceId,
    surface_ids: &HashMap<EntityId, SurfaceId>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let pane_id = surface_ids
        .iter()
        .find_map(|(pane_id, candidate)| (*candidate == surface_id).then_some(*pane_id));
    let pane = pane_id.and_then(|pane_id| {
        workspace
            .panes()
            .iter()
            .find(|pane| pane.entity_id() == pane_id)
            .cloned()
    });
    if let Some(pane) = pane {
        window.focus(&pane.focus_handle(cx), cx);
    } else {
        workspace.focus_center_pane(window, cx);
    }
}

fn schedule_ratio_restores(
    workspace: &mut Workspace,
    pending_ratios: Vec<PendingRatio>,
    _active_surface_id: SurfaceId,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let workspace = workspace.weak_handle();
    let focus = workspace
        .upgrade()
        .map(|workspace| workspace.read(cx).active_pane().clone());
    for pending in pending_ratios {
        let workspace = workspace.clone();
        let first = pending.first;
        let focus = focus.clone();
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    let Some(bounds) = workspace.bounding_box_for_pane(&first) else {
                        return;
                    };
                    let current = match pending.axis {
                        Axis::Horizontal => f32::from(bounds.size.width),
                        Axis::Vertical => f32::from(bounds.size.height),
                    };
                    if current <= 0.0 {
                        return;
                    }
                    // A newly-created split starts at 50/50. The first pane's
                    // current extent therefore gives us the full extent without
                    // reaching into Zed's private PaneGroup internals.
                    let amount = current * (pending.ratio.get() * 2.0 - 1.0);
                    if amount.abs() < 1.0 {
                        return;
                    }
                    window.focus(&first.focus_handle(cx), cx);
                    workspace.resize_pane(pending.axis, px(amount), window, cx);
                    if let Some(focus) = &focus {
                        window.focus(&focus.focus_handle(cx), cx);
                    }
                })
                .ok();
        });
    }
}

fn reorder_entries(
    entries: &mut Vec<WorkspaceEntry>,
    dragged_id: WorkspaceId,
    target_id: WorkspaceId,
) -> bool {
    if dragged_id == target_id {
        return false;
    }
    let Some(drag_ix) = entries.iter().position(|entry| entry.id == dragged_id) else {
        return false;
    };
    let Some(target_ix) = entries.iter().position(|entry| entry.id == target_id) else {
        return false;
    };
    let entry = entries.remove(drag_ix);
    entries.insert(target_ix, entry);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(
        surface_id: SurfaceId,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) -> (Bounds<Pixels>, StoredLayout) {
        (
            Bounds {
                origin: point(px(x), px(y)),
                size: size(px(w), px(h)),
            },
            StoredLayout::Leaf {
                surface_id,
                items: Vec::new(),
                active: 0,
            },
        )
    }

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

    fn ratio(layout: &StoredLayout) -> Ratio {
        match layout {
            StoredLayout::Split { ratio, .. } => *ratio,
            StoredLayout::Leaf { .. } => panic!("expected split"),
        }
    }

    #[test]
    fn single_pane_is_a_leaf() {
        assert_eq!(
            shape(&build_tree(vec![leaf(1, 0.0, 0.0, 100.0, 100.0)], 1)),
            "·"
        );
    }

    #[test]
    fn empty_layout_retains_the_allocated_surface_identity() {
        match build_tree(Vec::new(), 42) {
            StoredLayout::Leaf { surface_id, .. } => assert_eq!(surface_id, 42),
            StoredLayout::Split { .. } => panic!("expected empty leaf"),
        }
    }

    #[test]
    fn nested_layout_preserves_shape_and_non_equal_split_ratios() {
        // A 30/70 left/right split, whose right side is a 75/25 top/bottom split.
        let tree = build_tree(
            vec![
                leaf(1, 0.0, 0.0, 30.0, 100.0),
                leaf(2, 30.0, 0.0, 70.0, 75.0),
                leaf(3, 30.0, 75.0, 70.0, 25.0),
            ],
            1,
        );
        assert_eq!(shape(&tree), "H(·,V(·,·))");
        assert!((ratio(&tree).get() - 0.30).abs() < 0.01);
        let StoredLayout::Split { second, .. } = tree else {
            panic!("expected root split");
        };
        assert!((ratio(&second).get() - 0.75).abs() < 0.01);
    }

    #[test]
    fn drag_reorder_is_not_overwritten_by_id_ordering() {
        let mut entries = vec![
            WorkspaceEntry {
                id: 1,
                name: "one".into(),
                stored: None,
                restore: None,
            },
            WorkspaceEntry {
                id: 2,
                name: "two".into(),
                stored: None,
                restore: None,
            },
            WorkspaceEntry {
                id: 3,
                name: "three".into(),
                stored: None,
                restore: None,
            },
        ];
        assert!(reorder_entries(&mut entries, 3, 1));
        let new_id = 4;
        entries.push(WorkspaceEntry {
            id: new_id,
            name: "four".into(),
            stored: None,
            restore: None,
        });
        entries.retain(|entry| entry.id != 2);
        assert_eq!(
            entries.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            vec![3, 1, 4]
        );
    }

    #[test]
    fn process_workspace_allocator_never_reuses_a_closed_id() {
        let mut identities = WorkspaceProcessState::default();
        assert!(identities.claim_session_restore());
        identities.seed_from_persisted_state(None, None);
        let first_window_workspace = identities.allocate_workspace_id();
        let closed_workspace = identities.allocate_workspace_id();

        // Closing a workspace does not return its identity to the process
        // allocator, so app-global notifications cannot attach to a later,
        // unrelated workspace.
        let replacement_workspace = identities.allocate_workspace_id();
        assert_eq!(first_window_workspace, 1);
        assert_eq!(closed_workspace, 2);
        assert_eq!(replacement_workspace, 3);
        assert_eq!(identities.next_workspace_id(), 4);
    }

    #[test]
    fn process_workspace_allocator_uses_the_highest_persisted_watermark() {
        let snapshot = SessionSnapshot {
            version: crate::session::SESSION_VERSION,
            next_workspace_id: 41,
            active_workspace_id: 7,
            workspaces: vec![WorkspaceSnapshot {
                id: 7,
                name: "restored workspace".to_owned(),
                layout: WorkspaceLayoutSnapshot::single_empty(1),
            }],
        };
        snapshot.validate().unwrap();

        let mut session_watermark_wins = WorkspaceProcessState::default();
        assert!(session_watermark_wins.claim_session_restore());
        session_watermark_wins.seed_from_persisted_state(Some(&snapshot), Some(12));
        assert_eq!(session_watermark_wins.next_workspace_id(), 41);

        let mut identities = WorkspaceProcessState::default();
        assert!(identities.claim_session_restore());
        // The independent window advanced the sidecar watermark after the
        // session owner last wrote its layout. On a later process start, that
        // watermark wins over the older session watermark.
        identities.seed_from_persisted_state(Some(&snapshot), Some(61));
        // The restored window owns ID 7. A later process window and a new
        // workspace in the restored window both receive IDs beyond the
        // persisted watermark, so neither can replay/collide with it.
        assert!(!identities.claim_session_restore());
        let later_window_workspace = identities.allocate_workspace_id();
        let restored_window_new_workspace = identities.allocate_workspace_id();
        assert_eq!(later_window_workspace, 61);
        assert_eq!(restored_window_new_workspace, 62);
        assert_eq!(identities.next_workspace_id(), 63);
    }
}
