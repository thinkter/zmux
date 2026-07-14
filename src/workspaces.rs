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
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use editor::{Editor, EditorEvent};
use gpui::{
    App, Axis, Bounds, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    IntoElement, KeyDownEvent, Pixels, Render, SharedString, Subscription, Task, TaskExt,
    WeakEntity, Window, actions, div, point, px, size,
};
use terminal_view::TerminalView;
use ui::prelude::*;
use ui::{IconButtonShape, Indicator, Tooltip};
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::item::ItemHandle;
use workspace::{Pane, SplitDirection, Workspace};

use crate::app::create_center_terminal_for_workspace;
use crate::notification_runtime::NotificationRuntime;
use crate::notifications::{Notification, NotificationStore, WorkspaceId};
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
const NOTIFICATION_DRAWER_HEIGHT_REMS: f32 = 17.5;
const MAX_WORKSPACE_NAME_CHARS: usize = 64;
const CONTEXT_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

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
    },
    Split {
        axis: Axis,
        first: Box<StoredLayout>,
        second: Box<StoredLayout>,
    },
}

struct WorkspaceEntry {
    id: WorkspaceId,
    manual_name: Option<String>,
    automatic_name: String,
    context: WorkspaceContext,
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
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct WorkspaceContext {
    working_directories: Vec<PathBuf>,
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
    _notification_subscription: Subscription,
    _context_refresh_task: Task<()>,
}

impl WorkspacesPanel {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let entries = vec![WorkspaceEntry {
            id: 1,
            manual_name: None,
            automatic_name: "New workspace".to_string(),
            context: WorkspaceContext::default(),
            stored: None,
        }];
        let notification_store = NotificationStore::global(cx);
        let notification_subscription = cx.observe(&notification_store, |_, _, cx| cx.notify());
        let context_refresh_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(CONTEXT_REFRESH_INTERVAL)
                    .await;
                if this
                    .update(cx, |this, cx| this.refresh_workspace_contexts(cx))
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
            active: 1,
            activation_generation: 0,
            next_id: 2,
            rename: None,
            notifications_expanded: false,
            _notification_subscription: notification_subscription,
            _context_refresh_task: context_refresh_task,
        }
    }

    pub fn active_workspace_id(&self) -> WorkspaceId {
        self.active
    }

    pub(crate) fn active_workspace_generation(&self) -> u64 {
        self.activation_generation
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

        let captured = workspace.update(cx, |workspace, cx| {
            let captured = capture_layout(workspace, cx);
            clear_center(workspace, window, cx);

            let target_pane = workspace.active_pane().clone();
            match target_layout {
                Some(layout) => {
                    restore_layout(workspace, target_pane, layout, window, cx);
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
                            window,
                            cx,
                        )
                        .detach_and_log_err(cx);
                    }
                }
                None => {
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
                        window,
                        cx,
                    )
                    .detach_and_log_err(cx);
                }
            }
            workspace.focus_center_pane(window, cx);
            captured
        });

        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == previous) {
            entry.stored = Some(captured);
        }
        self.active = id;
        self.refresh_workspace_contexts(cx);
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
        NotificationRuntime::clear_workspace(cx.entity_id(), id, cx);
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
        cx.notify();
    }

    fn use_automatic_name(&mut self, id: WorkspaceId, cx: &mut Context<Self>) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) {
            entry.manual_name = None;
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
            .when(unread_count > 0, |this| {
                this.child(
                    div()
                        .px_1()
                        .rounded_md()
                        .bg(cx.theme().colors().element_selected)
                        .child(
                            Label::new(unread_count.to_string())
                                .size(LabelSize::XSmall)
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
        let name_area = v_flex()
            .flex_1()
            .gap_0p5()
            .overflow_hidden()
            .child(name_row)
            .child(
                h_flex()
                    .gap_1()
                    .overflow_hidden()
                    .child(
                        Label::new(shell_label)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .single_line(),
                    )
                    .children(context.foreground_processes.iter().take(3).map(|process| {
                        div()
                            .px_1()
                            .rounded_sm()
                            .bg(cx.theme().colors().element_background)
                            .child(
                                Label::new(process.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .single_line(),
                            )
                    })),
            );

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
        self.notifications_expanded = !self.notifications_expanded;
        cx.notify();
    }

    pub fn toggle_notification_center(&mut self, cx: &mut Context<Self>) {
        self.toggle_notifications(cx);
    }

    fn dismiss_notification(&mut self, id: u64, cx: &mut Context<Self>) {
        NotificationRuntime::dismiss_notification(id, cx);
    }

    fn mark_scope_read(&mut self, cx: &mut Context<Self>) {
        let scope_id = cx.entity_id();
        NotificationRuntime::mark_scope_read(scope_id, cx);
    }

    fn clear_scope_notifications(&mut self, cx: &mut Context<Self>) {
        let scope_id = cx.entity_id();
        NotificationRuntime::clear_scope_notifications(scope_id, cx);
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
        let rows: Vec<_> = self
            .entries
            .iter()
            // Clone the lightweight metadata so we no longer borrow `self.entries`
            // while `render_entry` borrows `self`.
            .map(|entry| WorkspaceRow {
                id: entry.id,
                name: entry.display_name().to_string(),
                uses_manual_name: entry.manual_name.is_some(),
                context: entry.context.clone(),
            })
            .collect();

        let scope_id = cx.entity_id();
        let (latest, unread_count, notifications) = {
            let store = NotificationStore::global(cx);
            let store = store.read(cx);
            let latest = store
                .notifications()
                .find(|notification| notification.target.scope_id == scope_id && !notification.read)
                .cloned();
            let unread_count = store.scope_unread_count(scope_id);
            let notifications = store
                .notifications()
                .filter(|notification| notification.target.scope_id == scope_id)
                .cloned()
                .collect::<Vec<_>>();
            (latest, unread_count, notifications)
        };

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
                                        this.create_workspace(window, cx);
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
                                .child(
                                    Label::new(format!("Notifications · {unread_count} unread"))
                                        .size(LabelSize::Small),
                                )
                                .child(
                                    h_flex()
                                        .gap_0p5()
                                        .child(
                                            IconButton::new("notifications-read", IconName::Check)
                                                .shape(IconButtonShape::Square)
                                                .icon_size(IconSize::XSmall)
                                                .tooltip(Tooltip::text("Mark all read"))
                                                .on_click(cx.listener(|this, _, _window, cx| {
                                                    this.mark_scope_read(cx)
                                                })),
                                        )
                                        .child(
                                            IconButton::new("notifications-clear", IconName::Trash)
                                                .shape(IconButtonShape::Square)
                                                .icon_size(IconSize::XSmall)
                                                .tooltip(Tooltip::text("Clear notifications"))
                                                .on_click(cx.listener(|this, _, _window, cx| {
                                                    this.clear_scope_notifications(cx)
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

    let git_roots = context
        .working_directories
        .iter()
        .map(|directory| nearest_git_root(directory))
        .collect::<Option<Vec<_>>>();
    if let Some(git_roots) = git_roots {
        let git_roots = git_roots.into_iter().collect::<BTreeSet<_>>();
        if git_roots.len() == 1 {
            context.git_root = git_roots.into_iter().next();
        }
    }
    context
}

fn nearest_git_root(directory: &Path) -> Option<PathBuf> {
    directory
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .map(Path::to_path_buf)
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
        nodes.push((bounds, StoredLayout::Leaf { items, active }));
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
    StoredLayout::Leaf { items, active: 0 }
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
fn try_cut(nodes: &[(Bounds<Pixels>, StoredLayout)], horizontal: bool) -> Option<Vec<usize>> {
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
) {
    match layout {
        StoredLayout::Leaf { items, active } => {
            if items.is_empty() {
                return;
            }
            target.update(cx, |pane, cx| {
                for item in items {
                    pane.add_item(item, false, false, None, window, cx);
                }
                let index = active.min(pane.items_len().saturating_sub(1));
                pane.activate_item(index, false, false, window, cx);
            });
        }
        StoredLayout::Split {
            axis,
            first,
            second,
        } => {
            let direction = if axis == Axis::Horizontal {
                SplitDirection::Right
            } else {
                SplitDirection::Down
            };
            let new_pane = workspace.split_pane(target.clone(), direction, window, cx);
            restore_layout(workspace, target, *first, window, cx);
            restore_layout(workspace, new_pane, *second, window, cx);
        }
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

    fn leaf(x: f32, y: f32, w: f32, h: f32) -> (Bounds<Pixels>, StoredLayout) {
        (
            Bounds {
                origin: point(px(x), px(y)),
                size: size(px(w), px(h)),
            },
            StoredLayout::Leaf {
                items: Vec::new(),
                active: 0,
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
    fn three_columns_nest_left_to_right() {
        let tree = build_tree(vec![
            leaf(0.0, 0.0, 33.0, 100.0),
            leaf(33.0, 0.0, 33.0, 100.0),
            leaf(66.0, 0.0, 34.0, 100.0),
        ]);
        assert_eq!(shape(&tree), "H(·,H(·,·))");
    }
}
