//! Opening zmux windows: builds the Zed [`Workspace`], installs the
//! [`WorkspacesPanel`] and Git panel, registers zmux actions, and either
//! restores the persisted startup layout or spawns the first terminal.

use std::path::PathBuf;

use gpui::{App, AppContext, Task, TaskExt, WindowHandle};
use workspace::{AppState, MultiWorkspace, OpenMode, OpenResult, Workspace};

use crate::env::terminal_env;
use crate::notification_runtime::NotificationRuntime;
use crate::welcome::ZmuxWelcome;
use crate::workspaces::{WorkspacesPanel, register_git_repository_scope, restore_startup_layout};

use super::actions::register_zmux_actions;
use super::terminal::{create_center_terminal, create_restored_terminals_for_workspace};

pub fn open_zmux_workspace(
    requesting_window: Option<WindowHandle<MultiWorkspace>>,
    cx: &mut App,
) -> Task<anyhow::Result<OpenResult>> {
    let initial_dir = crate::env::current_working_directory()
        .map(|path| vec![path])
        .unwrap_or_default();
    open_zmux_workspace_for_paths(requesting_window, initial_dir, true, cx)
}

/// Open a zmux window rooted at an explicit directory.
///
/// Besides being useful for embedders, this keeps tests independent from a
/// persisted Zed workspace for the repository that happens to run them.
pub fn open_zmux_workspace_at(
    requesting_window: Option<WindowHandle<MultiWorkspace>>,
    initial_dir: PathBuf,
    cx: &mut App,
) -> Task<anyhow::Result<OpenResult>> {
    open_zmux_workspace_for_paths(requesting_window, vec![initial_dir], false, cx)
}

fn open_zmux_workspace_for_paths(
    requesting_window: Option<WindowHandle<MultiWorkspace>>,
    initial_dirs: Vec<PathBuf>,
    session_enabled: bool,
    cx: &mut App,
) -> Task<anyhow::Result<OpenResult>> {
    let app_state = AppState::global(cx);

    Workspace::new_local(
        initial_dirs,
        app_state,
        requesting_window,
        // Route capabilities are per terminal and must never enter the
        // project-wide environment inherited by every shell.
        Some(terminal_env()),
        Some(Box::new(move |workspace, window, cx| {
            let welcome = cx.new(ZmuxWelcome::new);
            let center_pane = workspace.active_pane().clone();
            center_pane.update(cx, |pane, cx| {
                pane.set_should_display_welcome_page(false);
                pane.add_item(Box::new(welcome), true, true, None, window, cx);
            });
            workspace
                .bottom_dock()
                .update(cx, |dock, cx| dock.set_open(false, window, cx));

            let panel = cx.new(|cx| {
                WorkspacesPanel::new(workspace.weak_handle(), session_enabled, window, cx)
            });
            workspace.add_panel(panel.clone(), window, cx);
            register_git_repository_scope(workspace.project(), &panel, cx);
            workspace.open_panel::<WorkspacesPanel>(window, cx);
            cx.spawn_in(window, async move |workspace_handle, cx| {
                let git_panel =
                    git_ui::git_panel::GitPanel::load(workspace_handle.clone(), cx.clone()).await?;
                workspace_handle.update_in(cx, |workspace, window, cx| {
                    workspace.add_panel(git_panel, window, cx);
                })?;
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
            NotificationRuntime::attach_workspace(cx.entity(), panel.clone(), window, cx);

            register_zmux_actions(workspace);

            let startup_restore = panel.read(cx).initial_restore();
            if let Some(layout) = startup_restore {
                let workspace_id = panel.read(cx).active_workspace_id();
                let generation = panel.read(cx).active_workspace_generation();
                let terminals = restore_startup_layout(workspace, &layout, window, cx);
                create_restored_terminals_for_workspace(
                    workspace,
                    workspace_id,
                    generation,
                    terminals,
                    window,
                    cx,
                )
                .detach_and_log_err(cx);
            } else {
                create_center_terminal(workspace, window, cx).detach_and_log_err(cx);
            }
        })),
        OpenMode::NewWindow,
        cx,
    )
}
