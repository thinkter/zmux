//! Modifier-aware switcher for zmux's logical workspaces.

use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    KeyDownEvent, Modifiers, ModifiersChangedEvent, Render, Window, rems,
};
use ui::prelude::*;
use workspace::{ModalView, Workspace};

use crate::workspaces::{WorkspaceSwitcherEntry, WorkspacesPanel};

#[derive(Clone, Copy)]
pub(crate) enum SwitchDirection {
    Next,
    Previous,
}

pub(crate) struct WorkspaceSwitcher {
    panel: Entity<WorkspacesPanel>,
    entries: Vec<WorkspaceSwitcherEntry>,
    selected_index: usize,
    init_modifiers: Option<Modifiers>,
    focus_handle: FocusHandle,
}

impl WorkspaceSwitcher {
    pub(crate) fn toggle(
        workspace: &mut Workspace,
        panel: Entity<WorkspacesPanel>,
        direction: SwitchDirection,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if let Some(switcher) = workspace.active_modal::<Self>(cx) {
            switcher.update(cx, |switcher, cx| switcher.cycle(direction, cx));
            return;
        }

        let (entries, active_id) = {
            let panel = panel.read(cx);
            (panel.switcher_entries(cx), panel.active_workspace_id())
        };
        workspace.toggle_modal(window, cx, move |window, cx| {
            Self::new(panel, entries, active_id, direction, window, cx)
        });
    }

    fn new(
        panel: Entity<WorkspacesPanel>,
        entries: Vec<WorkspaceSwitcherEntry>,
        active_id: u64,
        direction: SwitchDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let active_index = entries
            .iter()
            .position(|entry| entry.id == active_id)
            .unwrap_or(0);
        let selected_index = advanced_index(active_index, entries.len(), direction);
        Self {
            panel,
            entries,
            selected_index,
            init_modifiers: window.modifiers().modified().then_some(window.modifiers()),
            focus_handle: cx.focus_handle(),
        }
    }

    fn cycle(&mut self, direction: SwitchDirection, cx: &mut Context<Self>) {
        self.selected_index = advanced_index(self.selected_index, self.entries.len(), direction);
        cx.notify();
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(self.selected_index) else {
            cx.emit(DismissEvent);
            return;
        };
        self.panel.update(cx, |panel, cx| {
            panel.activate_workspace(entry.id, window, cx)
        });
        cx.emit(DismissEvent);
    }

    fn handle_modifiers_changed(
        &mut self,
        event: &ModifiersChangedEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(init_modifiers) = self.init_modifiers else {
            return;
        };
        if !event.modified() || !init_modifiers.is_subset_of(event) {
            self.init_modifiers = None;
            self.confirm(window, cx);
        }
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.keystroke.key.as_str() {
            "enter" => self.confirm(window, cx),
            "escape" => cx.emit(DismissEvent),
            "down" => self.cycle(SwitchDirection::Next, cx),
            "up" => self.cycle(SwitchDirection::Previous, cx),
            _ => return,
        }
        cx.stop_propagation();
    }
}

impl EventEmitter<DismissEvent> for WorkspaceSwitcher {}

impl Focusable for WorkspaceSwitcher {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for WorkspaceSwitcher {}

impl Render for WorkspaceSwitcher {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.entries.clone();
        v_flex()
            .key_context("WorkspaceSwitcher")
            .track_focus(&self.focus_handle)
            .on_modifiers_changed(cx.listener(Self::handle_modifiers_changed))
            .on_key_down(cx.listener(Self::handle_key_down))
            .w(rems(30.0))
            .max_h(rems(28.0))
            .p_1()
            .gap_1()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().colors().border)
            .shadow_lg()
            .bg(cx.theme().colors().elevated_surface_background)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .justify_between()
                    .child(Label::new("Switch workspace").size(LabelSize::Small))
                    .child(
                        Label::new("release shortcut to select")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(
                v_flex()
                    .id("workspace-switcher-list")
                    .gap_0p5()
                    .overflow_y_scroll()
                    .children(rows.into_iter().enumerate().map(|(index, entry)| {
                        let selected = index == self.selected_index;
                        h_flex()
                            .id(("workspace-switcher-entry", entry.id as usize))
                            .w_full()
                            .px_2()
                            .py_1p5()
                            .gap_2()
                            .rounded_md()
                            .cursor_pointer()
                            .when(selected, |this| {
                                this.bg(cx.theme().colors().element_selected)
                            })
                            .hover(|this| this.bg(cx.theme().colors().element_hover))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.selected_index = index;
                                this.confirm(window, cx);
                            }))
                            .child(Icon::new(IconName::Terminal).size(IconSize::Small).color(
                                if selected {
                                    Color::Default
                                } else {
                                    Color::Muted
                                },
                            ))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .overflow_hidden()
                                    .child(
                                        Label::new(entry.name).size(LabelSize::Small).single_line(),
                                    )
                                    .child(
                                        Label::new(entry.detail)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted)
                                            .single_line(),
                                    ),
                            )
                            .when(entry.unread_count > 0, |this| {
                                this.child(
                                    div()
                                        .px_1()
                                        .rounded_md()
                                        .bg(cx.theme().colors().element_background)
                                        .child(
                                            Label::new(entry.unread_count.to_string())
                                                .size(LabelSize::XSmall)
                                                .color(Color::Accent),
                                        ),
                                )
                            })
                    })),
            )
    }
}

fn advanced_index(current: usize, count: usize, direction: SwitchDirection) -> usize {
    if count == 0 {
        return 0;
    }
    match direction {
        SwitchDirection::Next => (current + 1) % count,
        SwitchDirection::Previous => current.checked_sub(1).unwrap_or(count - 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycling_wraps_in_both_directions() {
        assert_eq!(advanced_index(2, 3, SwitchDirection::Next), 0);
        assert_eq!(advanced_index(0, 3, SwitchDirection::Previous), 2);
        assert_eq!(advanced_index(0, 1, SwitchDirection::Next), 0);
        assert_eq!(advanced_index(0, 0, SwitchDirection::Previous), 0);
    }
}
