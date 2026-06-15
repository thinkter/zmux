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

use editor::{Editor, EditorEvent};
use gpui::{
    App, Axis, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    KeyDownEvent, Pixels, Render, SharedString, Subscription, TaskExt, WeakEntity, Window, actions,
    div, point, px, size,
};
use ui::prelude::*;
use ui::{IconButtonShape, Tooltip};
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::item::ItemHandle;
use workspace::{Pane, SplitDirection, Workspace};

use crate::app::create_center_terminal;

actions!(zmux, [NewWorkspace, ToggleWorkspacesPanel, ActivateNextWorkspace, ActivatePreviousWorkspace]);

const PANEL_WIDTH: f32 = 240.0;

type WorkspaceId = u64;

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
    name: String,
    /// `Some` while the workspace is parked in the background, `None` while it is
    /// the active workspace displayed in the center.
    stored: Option<StoredLayout>,
}

struct RenameState {
    id: WorkspaceId,
    editor: Entity<Editor>,
    _subscription: Subscription,
}

pub struct WorkspacesPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    entries: Vec<WorkspaceEntry>,
    active: WorkspaceId,
    next_id: WorkspaceId,
    rename: Option<RenameState>,
}

impl WorkspacesPanel {
    pub fn new(workspace: WeakEntity<Workspace>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let entries = vec![WorkspaceEntry {
            id: 1,
            name: "Workspace 1".to_string(),
            stored: None,
        }];
        Self {
            workspace,
            focus_handle,
            entries,
            active: 1,
            next_id: 2,
            rename: None,
        }
    }

    /// Create a fresh, empty workspace and switch to it.
    pub fn create_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.next_id;
        self.next_id += 1;
        let name = format!("Workspace {id}");
        self.entries.push(WorkspaceEntry {
            id,
            name,
            stored: None,
        });
        self.activate_workspace(id, window, cx);
    }

    /// Switch the center to display the given workspace, parking the currently
    /// active one. The whole swap happens in a single [`Workspace`] update so the
    /// user never sees an intermediate state.
    pub fn activate_workspace(&mut self, id: WorkspaceId, window: &mut Window, cx: &mut Context<Self>) {
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
                Some(layout) => restore_layout(workspace, target_pane, layout, window, cx),
                None => {
                    create_center_terminal(workspace, window, cx).detach_and_log_err(cx);
                }
            }
            workspace.focus_center_pane(window, cx);
            captured
        });

        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == previous) {
            entry.stored = Some(captured);
        }
        self.active = id;
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
        cx.notify();
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
    }

    fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        if self.rename.take().is_some() {
            cx.notify();
        }
    }

    fn render_entry(
        &self,
        entry: &WorkspaceEntry,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = entry.id;
        let is_active = id == self.active;
        let renaming = self
            .rename
            .as_ref()
            .filter(|rename| rename.id == id)
            .map(|rename| rename.editor.clone());
        let group = SharedString::from(format!("ws-row-{id}"));
        let can_close = self.entries.len() > 1;

        let editor = renaming.clone();
        let is_renaming = renaming.is_some();
        let name_area = h_flex()
            .id(("ws-name", id as usize))
            .flex_1()
            .gap_2()
            .overflow_hidden()
            .child(
                Icon::new(IconName::Terminal)
                    .size(IconSize::Small)
                    .color(if is_active { Color::Default } else { Color::Muted }),
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
                        .color(if is_active { Color::Default } else { Color::Muted })
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
            });

        h_flex()
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
        let rows: Vec<_> = self
            .entries
            .iter()
            // Clone the lightweight metadata so we no longer borrow `self.entries`
            // while `render_entry` borrows `self`.
            .map(|entry| WorkspaceEntry {
                id: entry.id,
                name: entry.name.clone(),
                stored: None,
            })
            .collect();

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
                    .children(rows.iter().map(|entry| self.render_entry(entry, cx))),
            )
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

    fn set_position(&mut self, _position: DockPosition, _window: &mut Window, _cx: &mut Context<Self>) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(PANEL_WIDTH)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::ListCollapse)
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
    pane.update(cx, |pane, cx| {
        while pane.take_active_item(window, cx).is_some() {}
    });
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
        let tree = build_tree(vec![leaf(0.0, 0.0, 50.0, 100.0), leaf(50.0, 0.0, 50.0, 100.0)]);
        assert_eq!(shape(&tree), "H(·,·)");
    }

    #[test]
    fn stacked_panes_become_a_vertical_split() {
        let tree = build_tree(vec![leaf(0.0, 0.0, 100.0, 50.0), leaf(0.0, 50.0, 100.0, 50.0)]);
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
