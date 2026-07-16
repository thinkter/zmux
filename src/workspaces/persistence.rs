//! Layout capture/restore and session persistence for logical workspaces.
//!
//! [`StoredLayout`] parks an inactive workspace's live terminal items
//! (keeping their PTYs alive); the snapshot functions convert between live
//! pane trees, stored layouts, and the serialized [`LayoutSnapshot`]s written
//! to disk. [`SessionPersistence`] coalesces snapshot requests so at most one
//! session write is in flight and failed writes retry without tight loops.

use std::collections::HashMap;
use std::path::PathBuf;

use gpui::{App, Axis, Context, Entity, EntityId, Focusable, Global, Window};
use terminal_view::TerminalView;
use ui::prelude::*;
use workspace::item::ItemHandle;
use workspace::{Member, Pane, SplitDirection, Workspace};

use crate::session::{
    LayoutAxis, LayoutNodeSnapshot, LayoutSnapshot, SESSION_VERSION, SessionSnapshot,
    SessionWriteOutcome, TerminalSnapshot, WorkspaceSnapshot,
};

use super::WorkspacesPanel;
use super::git_context::GitDiscoveryState;

/// A detached snapshot of a workspace's center: the split tree plus the live
/// terminal item handles, which keep the underlying terminals running while the
/// workspace is in the background.
pub(super) enum StoredLayout {
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

#[derive(Default)]
pub(super) struct SessionOwnerClaimed(pub(super) bool);

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
pub(super) struct UnitRect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl UnitRect {
    pub(super) const FULL: Self = Self {
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

#[derive(Debug)]
pub(super) struct SessionPersistence {
    persisted: Option<SessionSnapshot>,
    desired: Option<SessionSnapshot>,
    in_flight: Option<SessionSnapshot>,
}

impl SessionPersistence {
    pub(super) fn new(restored: Option<SessionSnapshot>) -> Self {
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

impl WorkspacesPanel {
    pub(super) fn persist_session(&mut self, cx: &mut Context<Self>) {
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
}

/// Snapshot the current center into a [`StoredLayout`], cloning each item handle
/// so the terminals stay alive after the originals are detached.
///
/// The split structure and ratios are read directly from the live pane group's
/// flex values rather than from paint-time bounding boxes: the boxes are only
/// refreshed by a layout pass, so they are stale (or too short) whenever a
/// capture runs between a structural change and the next frame.
pub(super) fn capture_layout(workspace: &Workspace, cx: &App) -> StoredLayout {
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

pub(super) fn stored_layout_contains_item(layout: &StoredLayout, item_id: EntityId) -> bool {
    match layout {
        StoredLayout::Leaf { items, .. } => items.iter().any(|item| item.item_id() == item_id),
        StoredLayout::Split { first, second, .. } => {
            stored_layout_contains_item(first, item_id)
                || stored_layout_contains_item(second, item_id)
        }
    }
}

pub(super) fn center_has_provisioned_terminal(workspace: &Workspace, cx: &App) -> bool {
    workspace.panes().iter().any(|pane| {
        pane.read(cx)
            .items()
            .any(|item| item.act_as::<TerminalView>(cx).is_some())
    })
}

/// Detach every item from the center, keeping the terminals alive (the caller is
/// expected to already hold cloned handles). Leaves a single empty pane.
pub(super) fn clear_center(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
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
pub(super) fn restore_layout(
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
pub(super) fn apply_restored_flexes(
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

pub(super) fn restore_snapshot_layout(
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
