//! Registration of zmux's workspace-level actions and the capture-phase
//! interceptors that keep every terminal spawn on the per-terminal CLI route
//! (command palette, pane menus, and keymap splits included).

use gpui::{AppContext, InteractiveElement, TaskExt, actions};
use workspace::pane::{
    SplitDown as PaneSplitDown, SplitHorizontal as PaneSplitHorizontal, SplitLeft as PaneSplitLeft,
    SplitRight as PaneSplitRight, SplitUp as PaneSplitUp, SplitVertical as PaneSplitVertical,
};
use workspace::pane_group::SplitDirection;
use workspace::{
    ActivatePane as WorkspaceActivatePane, MultiWorkspace,
    NewCenterTerminal as WorkspaceNewCenterTerminal, NewTerminal as WorkspaceNewTerminal,
    Workspace,
};

use crate::keymap::{NewTerminal, OpenSettings, SplitTerminalDown, SplitTerminalRight};
use crate::notification_runtime::NotificationRuntime;
use crate::notifications::NotificationTarget;
use crate::settings_page::SettingsPage;
use crate::workspace_switcher::{SwitchDirection, WorkspaceSwitcher};
use crate::workspaces::{
    ActivateNextWorkspace, ActivatePreviousWorkspace, NewWorkspace, ToggleNotificationCenter,
    ToggleWorkspacesPanel, WorkspacesPanel,
};

use super::open::open_zmux_workspace_at;
use super::terminal::{
    capture_missing_terminal_pane_activation, capture_terminal_clone_split,
    capture_workspace_terminal_creation, create_center_terminal, create_split_terminal,
};

actions!(zmux, [NotifyCurrentPane, JumpToLatestNotification]);

/// Register every zmux workspace action on a freshly opened window's
/// [`Workspace`]. Called once from the open callback in [`super::open`].
pub(super) fn register_zmux_actions(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &OpenSettings, window, cx| {
        let pane = workspace.active_pane().clone();
        pane.update(cx, |pane, cx| {
            let existing = pane.items_of_type::<SettingsPage>().next();
            if let Some(existing) = existing
                && let Some(index) = pane.index_for_item(&existing)
            {
                pane.activate_item(index, true, true, window, cx);
                return;
            }
            let page = cx.new(SettingsPage::new);
            pane.add_item(Box::new(page), true, true, None, window, cx);
        });
    });
    workspace.register_action(|workspace, _: &NewTerminal, window, cx| {
        create_center_terminal(workspace, window, cx).detach_and_log_err(cx);
    });
    workspace.register_action(|workspace, _: &SplitTerminalRight, window, cx| {
        create_split_terminal(workspace, SplitDirection::Right, window, cx).detach_and_log_err(cx);
    });
    workspace.register_action(|workspace, _: &SplitTerminalDown, window, cx| {
        create_split_terminal(workspace, SplitDirection::Down, window, cx).detach_and_log_err(cx);
    });
    workspace.register_action(|workspace, _: &zed_actions::git::Worktree, window, cx| {
        let focused_dock = workspace.focused_dock_position(window, cx);
        let project = workspace.project().clone();
        let workspace_handle = workspace.weak_handle();
        workspace.toggle_modal(window, cx, |window, cx| {
            git_ui::worktree_picker::WorktreePicker::new_modal(
                project,
                workspace_handle,
                focused_dock,
                window,
                cx,
            )
        });
    });
    workspace.register_action(
        |workspace, action: &zed_actions::CreateWorktree, window, cx| {
            let task =
                git_ui::worktree_service::create_worktree_paths(workspace, action, window, cx);
            let panel = workspace
                .panel::<WorkspacesPanel>(cx)
                .map(|panel| panel.downgrade());
            let workspace_handle = workspace.weak_handle();
            cx.spawn_in(window, async move |_, cx| {
                match task.await {
                    Ok(created) => {
                        if let Some(panel) = panel.and_then(|panel| panel.upgrade()) {
                            panel.update_in(cx, |panel, window, cx| {
                                panel.open_created_worktrees(
                                    created.paths,
                                    created.name,
                                    window,
                                    cx,
                                );
                            })?;
                        }
                    }
                    Err(error) => {
                        if let Some(workspace) = workspace_handle.upgrade() {
                            cx.update(|_, cx| {
                                git_ui::git_panel::show_error_toast(
                                    workspace,
                                    "worktree create",
                                    error,
                                    cx,
                                );
                            })?;
                        }
                    }
                }
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
        },
    );
    workspace.register_action(
        |workspace, action: &zed_actions::SwitchWorktree, window, cx| {
            let Some(panel) = workspace.panel::<WorkspacesPanel>(cx) else {
                return;
            };
            let path = action.path.clone();
            let display_name = action.display_name.clone();
            let window_handle = window.window_handle();
            cx.defer(move |cx| {
                window_handle
                    .update(cx, |_, window, cx| {
                        panel.update(cx, |panel, cx| {
                            panel.open_worktree(path, display_name, window, cx)
                        });
                    })
                    .ok();
            });
        },
    );
    workspace.register_action(
        |_workspace, action: &zed_actions::OpenWorktreeInNewWindow, window, cx| {
            let path = action.path.clone();
            let requesting_window = window.window_handle().downcast::<MultiWorkspace>();
            cx.defer(move |cx| {
                open_zmux_workspace_at(requesting_window, path, cx).detach_and_log_err(cx);
            });
        },
    );
    // Zed's built-in pane menus and user keymaps dispatch the generic
    // pane split actions. Capture clone splits before Pane handles
    // them so every newly spawned terminal receives a fresh route
    // capability; empty/move splits keep their ordinary semantics.
    let split_workspace = workspace.weak_handle();
    workspace.register_action_renderer(move |div, _, _, _| {
        let new_terminal = split_workspace.clone();
        let new_center_terminal = split_workspace.clone();
        let activate_pane = split_workspace.clone();
        let split_right = split_workspace.clone();
        let split_left = split_workspace.clone();
        let split_up = split_workspace.clone();
        let split_down = split_workspace.clone();
        let split_horizontal = split_workspace.clone();
        let split_vertical = split_workspace.clone();
        div.capture_action(move |_: &WorkspaceNewTerminal, window, cx| {
            capture_workspace_terminal_creation(&new_terminal, true, window, cx);
        })
        .capture_action(move |_: &WorkspaceNewCenterTerminal, window, cx| {
            capture_workspace_terminal_creation(&new_center_terminal, false, window, cx);
        })
        .capture_action(move |action: &WorkspaceActivatePane, window, cx| {
            capture_missing_terminal_pane_activation(&activate_pane, action.0, window, cx);
        })
        .capture_action(move |action: &PaneSplitRight, window, cx| {
            capture_terminal_clone_split(
                &split_right,
                action.mode,
                SplitDirection::Right,
                window,
                cx,
            );
        })
        .capture_action(move |action: &PaneSplitLeft, window, cx| {
            capture_terminal_clone_split(
                &split_left,
                action.mode,
                SplitDirection::Left,
                window,
                cx,
            );
        })
        .capture_action(move |action: &PaneSplitUp, window, cx| {
            capture_terminal_clone_split(&split_up, action.mode, SplitDirection::Up, window, cx);
        })
        .capture_action(move |action: &PaneSplitDown, window, cx| {
            capture_terminal_clone_split(
                &split_down,
                action.mode,
                SplitDirection::Down,
                window,
                cx,
            );
        })
        .capture_action(move |action: &PaneSplitHorizontal, window, cx| {
            let direction = SplitDirection::horizontal(cx);
            capture_terminal_clone_split(&split_horizontal, action.mode, direction, window, cx);
        })
        .capture_action(move |action: &PaneSplitVertical, window, cx| {
            let direction = SplitDirection::vertical(cx);
            capture_terminal_clone_split(&split_vertical, action.mode, direction, window, cx);
        })
    });
    workspace.register_action(|workspace, _: &NewWorkspace, window, cx| {
        let Some(panel) = workspace.panel::<WorkspacesPanel>(cx) else {
            return;
        };
        // Defer to the *app* level (not `defer_in`, which re-enters this
        // `Workspace` update) so the panel can swap the center — which
        // itself updates the workspace — without a re-entrant update.
        let window_handle = window.window_handle();
        cx.defer(move |cx| {
            window_handle
                .update(cx, |_, window, cx| {
                    panel.update(cx, |panel, cx| panel.prompt_for_workspace(window, cx));
                })
                .ok();
        });
    });
    workspace.register_action(|workspace, _: &ToggleWorkspacesPanel, window, cx| {
        workspace.toggle_panel_focus::<WorkspacesPanel>(window, cx);
    });
    workspace.register_action(|workspace, _: &ToggleNotificationCenter, window, cx| {
        workspace.open_panel::<WorkspacesPanel>(window, cx);
        if let Some(panel) = workspace.panel::<WorkspacesPanel>(cx) {
            panel.update(cx, |panel, cx| panel.toggle_notification_center(cx));
        }
    });
    workspace.register_action(|workspace, _: &ActivateNextWorkspace, window, cx| {
        let Some(panel) = workspace.panel::<WorkspacesPanel>(cx) else {
            return;
        };
        WorkspaceSwitcher::toggle(workspace, panel, SwitchDirection::Next, window, cx);
    });
    workspace.register_action(|workspace, _: &ActivatePreviousWorkspace, window, cx| {
        let Some(panel) = workspace.panel::<WorkspacesPanel>(cx) else {
            return;
        };
        WorkspaceSwitcher::toggle(workspace, panel, SwitchDirection::Previous, window, cx);
    });
    workspace.register_action(|workspace, _: &NotifyCurrentPane, window, cx| {
        let Some(panel) = workspace.panel::<WorkspacesPanel>(cx) else {
            return;
        };
        let workspace_id = panel.read(cx).active_workspace_id();
        let active_pane = workspace.active_pane().clone();
        let Some(item) = active_pane.read(cx).active_item() else {
            return;
        };
        if item.act_as::<terminal_view::TerminalView>(cx).is_none() {
            return;
        }
        let item_id = item.item_id();
        NotificationRuntime::publish_manual_for_target(
            NotificationTarget {
                scope_id: panel.entity_id(),
                workspace_id,
                item_id,
            },
            "Manual notification",
            "Test notification from current pane",
            window,
            cx,
        );
    });
    workspace.register_action(|workspace, _: &JumpToLatestNotification, _window, cx| {
        let Some(panel) = workspace.panel::<WorkspacesPanel>(cx) else {
            return;
        };
        NotificationRuntime::jump_to_latest_unread(panel.entity_id(), cx);
    });
}
