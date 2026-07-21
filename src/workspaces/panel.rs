//! Rendering for the workspaces panel: sidebar chrome, workspace rows, the
//! worktree/branch pickers, the agent chat rail, and the notification drawer.
//!
//! Pure presentation over state owned by [`WorkspacesPanel`]; mutations run
//! through the panel methods invoked from event listeners.

use gpui::{
    Anchor, App, Context, FontWeight, IntoElement, KeyDownEvent, Pixels, Render, SharedString,
    Window, div, px,
};
use ui::prelude::*;
use ui::{Button, ButtonSize, ContextMenu, IconButtonShape, Indicator, PopoverMenu, Tooltip};
use workspace::dock::{DockPosition, Panel};

use crate::agent_detection::AgentKind;
use crate::metadata::{GitMetadata, MetadataState};
use crate::notification_runtime::NotificationRuntime;
use crate::notifications::{Notification, NotificationStore, WorkspaceId};

use super::agent_chat::{
    AgentChatState, agent_chat_detail, agent_chat_display_title, agent_chat_tooltip,
    agent_chats_for_workspace,
};
use super::git_context::WorkspaceContext;
use super::{ToggleWorkspacesPanel, WorkspacesPanel, path_display_name, workspace_cwd_label};

const PANEL_WIDTH_REMS: f32 = 15.0;
const PANEL_MIN_WIDTH_REMS: f32 = 12.0;
const WORKSPACES_FONT_FAMILY: &str = "Lilex";
const NOTIFICATION_DRAWER_HEIGHT_REMS: f32 = 17.5;

fn scaled_panel_size(rem_size: Pixels, rems: f32) -> Pixels {
    px(f32::from(rem_size) * rems)
}

impl AgentKind {
    fn icon(self) -> Option<ForegroundProcessIcon> {
        match self {
            Self::Claude => Some(ForegroundProcessIcon::Named(IconName::AiClaude)),
            Self::Codex => Some(ForegroundProcessIcon::Named(IconName::AiOpenAi)),
            Self::OpenCode => Some(ForegroundProcessIcon::Named(IconName::AiOpenCode)),
            Self::Gemini => Some(ForegroundProcessIcon::Named(IconName::AiGemini)),
            Self::Pi => Some(ForegroundProcessIcon::Embedded("icons/ai_pi.svg")),
            Self::Amp | Self::Aider | Self::Goose => None,
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

impl WorkspacesPanel {
    fn render_agent_chats(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let chats = agent_chats_for_workspace(&self.agent_chats, self.active);
        if chats.is_empty() {
            return None;
        }

        let total = chats.len();
        let active = chats
            .iter()
            .filter(|chat| chat.state != AgentChatState::Idle)
            .count();
        let done = chats
            .iter()
            .filter(|chat| chat.state == AgentChatState::Idle && !chat.seen)
            .count();

        Some(
            v_flex()
                .border_t_1()
                .border_color(cx.theme().colors().border)
                .child(
                    h_flex()
                        .px_2()
                        .py_1()
                        .justify_between()
                        .child(
                            Label::new("Chats")
                                .size(LabelSize::Small)
                                .weight(FontWeight::SEMIBOLD),
                        )
                        .child(
                            Label::new(format!("{active} active · {done} done · {total}"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
                .child(
                    v_flex()
                        .id("agent-chats-list")
                        .px_1()
                        .pb_1()
                        .gap_0p5()
                        .max_h(rems(18.0))
                        .overflow_y_scroll()
                        .children(chats.into_iter().map(|chat| {
                            let item_id = chat.item_id;
                            let title = agent_chat_display_title(&chat);
                            let detail = agent_chat_detail(&chat);
                            let tooltip = agent_chat_tooltip(&chat, &title);
                            let focused = chat.focused;

                            h_flex()
                                .id(("agent-chat-row", item_id))
                                .w_full()
                                .min_w_0()
                                .px_1()
                                .py_1()
                                .gap_1()
                                .rounded_sm()
                                .when(focused, |this| {
                                    this.bg(cx.theme().colors().element_selected)
                                })
                                .hover(|this| this.bg(cx.theme().colors().element_hover))
                                .cursor_pointer()
                                .tooltip(Tooltip::text(tooltip))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.focus_terminal_item(item_id, window, cx);
                                }))
                                .child(Indicator::dot().color(chat.state.color(chat.seen)))
                                .child(match chat.kind.icon() {
                                    Some(ForegroundProcessIcon::Named(icon)) => Icon::new(icon)
                                        .size(IconSize::Small)
                                        .color(Color::Muted)
                                        .into_any_element(),
                                    Some(ForegroundProcessIcon::Embedded(path)) => {
                                        Icon::from_path(path)
                                            .size(IconSize::Small)
                                            .color(Color::Muted)
                                            .into_any_element()
                                    }
                                    None => Label::new(chat.kind.label())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .into_any_element(),
                                })
                                .child(
                                    v_flex()
                                        .min_w_0()
                                        .flex_1()
                                        .child(
                                            Label::new(title)
                                                .size(LabelSize::Small)
                                                .weight(FontWeight::SEMIBOLD)
                                                .single_line(),
                                        )
                                        .child(
                                            Label::new(detail)
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted)
                                                .single_line(),
                                        ),
                                )
                        })),
                ),
        )
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
            MetadataState::Unavailable(_) => Some("git unavailable".to_string()),
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
        let active_root_choices = self.active_git_root_choices();
        let active_root_is_pinned = self.active_git_root_is_pinned();
        let active_root_attachment_pending = self.active_git_root_attachment_pending();
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

        let git_root_selector = active_root.clone().map(|active_root| {
            let panel = cx.weak_entity();
            let active_label = path_display_name(&active_root)
                .unwrap_or_else(|| active_root.display().to_string());
            let trigger_label = if active_root_attachment_pending {
                format!("{active_label} · loading…")
            } else {
                active_label
            };
            PopoverMenu::new("git-root-selector")
                .menu(move |window, cx| {
                    let roots = active_root_choices.clone();
                    let panel = panel.clone();
                    Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                        for root in roots {
                            let label = root.display().to_string();
                            let panel = panel.clone();
                            menu = menu.entry(label, None, move |_window, cx| {
                                let _ = panel.update(cx, |panel, cx| {
                                    panel.pin_active_git_root(root.clone(), cx);
                                });
                            });
                        }
                        if active_root_is_pinned {
                            let panel = panel.clone();
                            menu = menu.separator().entry(
                                "Follow terminal directory",
                                None,
                                move |_window, cx| {
                                    let _ = panel.update(cx, |panel, cx| {
                                        panel.follow_terminal_git_root(cx);
                                    });
                                },
                            );
                        }
                        menu
                    }))
                })
                .trigger_with_tooltip(
                    Button::new("git-root-selector-button", trigger_label)
                        .size(ButtonSize::None)
                        .start_icon(Icon::new(IconName::GitBranch).size(IconSize::Small))
                        .truncate(true),
                    |_, cx| {
                        Tooltip::simple("Choose a repository; selection enables full Git tools", cx)
                    },
                )
                .anchor(Anchor::BottomLeft)
        });

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

        let agent_chats = self.render_agent_chats(cx);

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
            .when_some(git_root_selector, |this, git_root_selector| {
                this.child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .px_2()
                        .py_1()
                        .gap_1()
                        .border_b_1()
                        .border_color(cx.theme().colors().border)
                        .child(div().flex_1().min_w_0().child(git_root_selector))
                        .when_some(worktree_selector, |this, worktree_selector| {
                            this.child(div().flex_1().min_w_0().child(worktree_selector))
                        })
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
            .when_some(agent_chats, |this, chats| this.child(chats))
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
    fn agent_kinds_map_to_their_product_icons() {
        assert_eq!(
            AgentKind::Claude.icon(),
            Some(ForegroundProcessIcon::Named(IconName::AiClaude))
        );
        assert_eq!(
            AgentKind::Codex.icon(),
            Some(ForegroundProcessIcon::Named(IconName::AiOpenAi))
        );
        assert_eq!(
            AgentKind::OpenCode.icon(),
            Some(ForegroundProcessIcon::Named(IconName::AiOpenCode))
        );
        assert_eq!(
            AgentKind::Gemini.icon(),
            Some(ForegroundProcessIcon::Named(IconName::AiGemini))
        );
        assert_eq!(
            AgentKind::Pi.icon(),
            Some(ForegroundProcessIcon::Embedded("icons/ai_pi.svg"))
        );
        assert_eq!(AgentKind::Amp.icon(), None);
        assert_eq!(AgentKind::Aider.icon(), None);
        assert_eq!(AgentKind::Goose.icon(), None);
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
}
