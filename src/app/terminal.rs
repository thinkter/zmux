//! Terminal creation and CLI-route glue. Every shell zmux spawns gets a
//! pending per-terminal CLI route capability, and every asynchronous spawn is
//! validated against the owning logical workspace's `(id, activation
//! generation)` plus its exact destination pane, so a fast workspace switch
//! can never leak a terminal into whichever layout became active.

use std::{path::PathBuf, sync::Arc, time::Duration};

use gpui::{App, AppContext, Context, Task, TaskExt, WeakEntity, Window};
use project::Project;
use terminal_view::{TerminalView, default_working_directory};
use workspace::Workspace;
use workspace::pane::SplitMode;
use workspace::pane_group::SplitDirection;

use crate::cli_server::CliServer;
use crate::env::terminal_env_with_notification_endpoint;
use crate::notification_runtime::NotificationRuntime;
use crate::notifications::WorkspaceId;
use crate::workspaces::{RestoredTerminal, WorkspacesPanel};

const MAX_RESTORED_TERMINAL_ATTEMPTS: u32 = 3;

pub(super) fn create_center_terminal(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<anyhow::Result<WeakEntity<terminal::Terminal>>> {
    let Some(panel) = workspace.panel::<WorkspacesPanel>(cx) else {
        return Task::ready(Err(anyhow::anyhow!(
            "the zmux workspace panel is unavailable"
        )));
    };
    let (owning_workspace_id, activation_generation) = {
        let panel = panel.read(cx);
        (
            panel.active_workspace_id(),
            panel.active_workspace_generation(),
        )
    };
    create_center_terminal_for_workspace(
        workspace,
        owning_workspace_id,
        activation_generation,
        panel.read(cx).active_default_directory(),
        window,
        cx,
    )
}

/// Start a center-terminal spawn owned by one logical workspace and its exact
/// destination pane. `WorkspacesPanel` uses this explicit-ID entry point while
/// it is changing `active`, when re-reading the panel would be re-entrant.
pub(crate) fn create_center_terminal_for_workspace(
    workspace: &mut Workspace,
    owning_workspace_id: WorkspaceId,
    activation_generation: u64,
    default_directory: Option<PathBuf>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<anyhow::Result<WeakEntity<terminal::Terminal>>> {
    let working_directory = source_terminal_working_directory(workspace, cx)
        .or(default_directory)
        .or_else(|| default_working_directory(workspace, cx));
    create_center_terminal_at_for_workspace(
        workspace,
        owning_workspace_id,
        activation_generation,
        working_directory,
        window,
        cx,
    )
}

/// Spawn a terminal in an exact directory for a logical workspace. Worktree
/// creation uses this instead of inheriting the current terminal's cwd.
pub(crate) fn create_center_terminal_at_for_workspace(
    workspace: &mut Workspace,
    owning_workspace_id: WorkspaceId,
    activation_generation: u64,
    working_directory: Option<PathBuf>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<anyhow::Result<WeakEntity<terminal::Terminal>>> {
    let destination_pane = workspace.active_pane().clone();
    let Some(panel) = workspace.panel::<WorkspacesPanel>(cx) else {
        return Task::ready(Err(anyhow::anyhow!(
            "the zmux workspace panel is unavailable"
        )));
    };
    let panel = panel.downgrade();
    let project = workspace.project().downgrade();

    cx.spawn_in(window, async move |workspace, cx| {
        let terminal = project
            .update(cx, |project, cx| {
                create_terminal_with_cli_route(project, working_directory, cx)
            })?
            .await?;
        let terminal_weak = terminal.downgrade();

        workspace.update_in(cx, move |workspace, window, cx| {
            // Shell creation and route staging are asynchronous. If a logical
            // workspace switch parks/removes this exact pane first, inserting
            // into `active_pane` would leak the terminal into the new layout.
            // Refuse that stale completion; dropping the unmounted Terminal
            // releases its staged route registration immediately.
            let destination_is_current = panel.upgrade().is_some_and(|panel| {
                let panel = panel.read(cx);
                panel.active_workspace_id() == owning_workspace_id
                    && panel.active_workspace_generation() == activation_generation
            }) && workspace
                .panes()
                .iter()
                .any(|pane| pane == &destination_pane);
            if !destination_is_current {
                return;
            }

            let terminal_view = cx.new(|cx| {
                TerminalView::new(
                    terminal,
                    workspace.weak_handle(),
                    workspace.database_id(),
                    workspace.project().downgrade(),
                    window,
                    cx,
                )
            });
            workspace.add_item(
                destination_pane,
                Box::new(terminal_view),
                None,
                false,
                true,
                window,
                cx,
            );
        })?;
        Ok(terminal_weak)
    })
}

/// Recreate persisted terminal tabs as fresh shells, serially, so tab order is
/// deterministic. The destination panes are exact entities from the restored
/// split tree; a workspace switch invalidates the remaining work instead of
/// leaking shells into whichever workspace became active.
pub(crate) fn create_restored_terminals_for_workspace(
    workspace: &mut Workspace,
    owning_workspace_id: WorkspaceId,
    activation_generation: u64,
    terminals: Vec<RestoredTerminal>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<anyhow::Result<()>> {
    let Some(panel) = workspace.panel::<WorkspacesPanel>(cx) else {
        return Task::ready(Err(anyhow::anyhow!(
            "the zmux workspace panel is unavailable"
        )));
    };
    let panel = panel.downgrade();
    let project = workspace.project().downgrade();
    let workspace_handle = workspace.weak_handle();
    let database_id = workspace.database_id();

    cx.spawn_in(window, async move |workspace, cx| {
        let mut pending = terminals;
        let mut attempt: u32 = 0;
        loop {
            let destination_is_current = panel
                .update(cx, |panel, _| {
                    panel.active_workspace_id() == owning_workspace_id
                        && panel.active_workspace_generation() == activation_generation
                })
                .unwrap_or(false);
            if !destination_is_current {
                // Switching away preserves the complete restore snapshot; the
                // next activation retries it from scratch.
                return Ok(());
            }

            let mut failed = Vec::new();
            let mut last_error = None;
            for restored in pending {
                let terminal = match project
                    .update(cx, |project, cx| {
                        create_terminal_with_cli_route(
                            project,
                            restored.working_directory.clone(),
                            cx,
                        )
                    })?
                    .await
                {
                    Ok(terminal) => terminal,
                    Err(error) => {
                        last_error = Some(error);
                        failed.push(restored);
                        continue;
                    }
                };
                let project_for_view = project.clone();
                let workspace_for_view = workspace_handle.clone();
                let panel = panel.clone();
                let attached = workspace.update_in(cx, move |workspace, window, cx| {
                    let destination_is_current =
                        panel.upgrade().is_some_and(|panel| {
                            let panel = panel.read(cx);
                            panel.active_workspace_id() == owning_workspace_id
                                && panel.active_workspace_generation() == activation_generation
                        }) && workspace.panes().iter().any(|pane| pane == &restored.pane);
                    if !destination_is_current {
                        return false;
                    }

                    let terminal_view = cx.new(|cx| {
                        TerminalView::new(
                            terminal,
                            workspace_for_view,
                            database_id,
                            project_for_view,
                            window,
                            cx,
                        )
                    });
                    workspace.add_item(
                        restored.pane,
                        Box::new(terminal_view),
                        None,
                        false,
                        restored.activate,
                        window,
                        cx,
                    );
                    true
                })?;
                if !attached {
                    return Ok(());
                }
            }

            if failed.is_empty() {
                panel
                    .update(cx, |panel, cx| {
                        panel.finish_restored_git_discovery(owning_workspace_id, cx);
                    })
                    .ok();
                return Ok(());
            }

            attempt = attempt.saturating_add(1);
            if attempt >= MAX_RESTORED_TERMINAL_ATTEMPTS {
                let failed_slots = failed
                    .iter()
                    .map(|terminal| terminal.failed_slot.clone())
                    .collect();
                panel
                    .update(cx, |panel, cx| {
                        panel.finish_restored_git_discovery_with_failures(
                            owning_workspace_id,
                            failed_slots,
                            cx,
                        );
                    })
                    .ok();
                eprintln!(
                    "failed to restore {} terminal(s) after {attempt} attempts; preserving them for the next session: {}",
                    failed.len(),
                    last_error
                        .as_ref()
                        .map(|error| format!("{error:#}"))
                        .unwrap_or_else(|| "terminal creation failed without an error".to_owned())
                );
                return Ok(());
            }
            let delay = restored_terminal_retry_delay(attempt);
            eprintln!(
                "failed to restore {} terminal(s); retrying in {}s: {}",
                failed.len(),
                delay.as_secs(),
                last_error
                    .as_ref()
                    .map(|error| format!("{error:#}"))
                    .unwrap_or_else(|| "terminal creation failed without an error".to_owned())
            );
            pending = failed;
            cx.background_executor().timer(delay).await;
        }
    })
}

fn restored_terminal_retry_delay(attempt: u32) -> Duration {
    Duration::from_secs(1_u64 << attempt.saturating_sub(1).min(5))
}

pub(super) fn create_split_terminal(
    workspace: &mut Workspace,
    direction: SplitDirection,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<anyhow::Result<()>> {
    let pane_to_split = workspace.active_pane().clone();
    let Some(panel) = workspace.panel::<WorkspacesPanel>(cx) else {
        return Task::ready(Err(anyhow::anyhow!(
            "the zmux workspace panel is unavailable"
        )));
    };
    let (originating_workspace_id, activation_generation) = {
        let panel = panel.read(cx);
        (
            panel.active_workspace_id(),
            panel.active_workspace_generation(),
        )
    };
    let working_directory = source_terminal_working_directory(workspace, cx)
        .or_else(|| panel.read(cx).active_default_directory())
        .or_else(|| default_working_directory(workspace, cx));
    let panel = panel.downgrade();
    let project = workspace.project().downgrade();

    cx.spawn_in(window, async move |workspace, cx| {
        let terminal = project
            .update(cx, |project, cx| {
                create_terminal_with_cli_route(project, working_directory, cx)
            })?
            .await?;

        workspace.update_in(cx, move |workspace, window, cx| {
            // Shell creation is asynchronous. A fast logical-workspace switch
            // parks the originating pane; PaneGroup would otherwise fall back
            // to splitting the first pane in whichever workspace is current.
            // Cancel that stale completion and let dropping the terminal revoke
            // its still-pending CLI registration.
            let destination_is_current =
                panel.upgrade().is_some_and(|panel| {
                    let panel = panel.read(cx);
                    panel.active_workspace_id() == originating_workspace_id
                        && panel.active_workspace_generation() == activation_generation
                }) && workspace.panes().iter().any(|pane| pane == &pane_to_split);
            if !destination_is_current {
                return;
            }

            let terminal_view = cx.new(|cx| {
                TerminalView::new(
                    terminal,
                    workspace.weak_handle(),
                    workspace.database_id(),
                    workspace.project().downgrade(),
                    window,
                    cx,
                )
            });
            let new_pane = workspace.split_pane(pane_to_split, direction, window, cx);
            workspace.add_item(
                new_pane,
                Box::new(terminal_view),
                None,
                true,
                true,
                window,
                cx,
            );
        })?;
        Ok(())
    })
}

/// New tabs and splits follow the terminal the user is acting on. This avoids
/// surprising jumps back to the window's original project root after `cd`.
fn source_terminal_working_directory(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    let item = workspace.active_pane().read(cx).active_item()?;
    let terminal_view = item.act_as::<TerminalView>(cx)?;
    let terminal = terminal_view.read(cx).terminal().clone();
    terminal.read(cx).working_directory()
}

/// Zed's pane-number action creates a clone when the requested index does not
/// exist. Task-backed terminal cloning falls back to a plain shell and loses
/// the per-terminal endpoint, so provision that implicit split ourselves.
pub(super) fn capture_missing_terminal_pane_activation(
    workspace: &WeakEntity<Workspace>,
    requested_index: usize,
    window: &mut Window,
    cx: &mut App,
) {
    let intercepted = workspace
        .update(cx, |workspace, cx| {
            if requested_index < workspace.panes().len()
                || workspace
                    .active_pane()
                    .read(cx)
                    .active_item()
                    .is_none_or(|item| item.act_as::<TerminalView>(cx).is_none())
            {
                return false;
            }
            create_split_terminal(workspace, SplitDirection::Right, window, cx)
                .detach_and_log_err(cx);
            true
        })
        .unwrap_or(false);
    if intercepted {
        cx.stop_propagation();
    } else {
        cx.propagate();
    }
}

/// Zed registers generic terminal actions before zmux attaches its route
/// provisioning. Capture the live center-terminal cases at the workspace root
/// so command-palette and terminal context-menu creation cannot bypass the
/// per-terminal CLI capability.
pub(super) fn capture_workspace_terminal_creation(
    workspace: &WeakEntity<Workspace>,
    require_active_terminal: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let intercepted = workspace
        .update(cx, |workspace, cx| {
            if require_active_terminal
                && workspace
                    .active_pane()
                    .read(cx)
                    .active_item()
                    .is_none_or(|item| item.act_as::<TerminalView>(cx).is_none())
            {
                return false;
            }
            create_center_terminal(workspace, window, cx).detach_and_log_err(cx);
            true
        })
        .unwrap_or(false);
    if intercepted {
        cx.stop_propagation();
    } else {
        cx.propagate();
    }
}

pub(super) fn capture_terminal_clone_split(
    workspace: &WeakEntity<Workspace>,
    mode: SplitMode,
    direction: SplitDirection,
    window: &mut Window,
    cx: &mut App,
) {
    if mode != SplitMode::ClonePane {
        cx.propagate();
        return;
    }

    let intercepted = workspace
        .update(cx, |workspace, cx| {
            let active_item_is_terminal = workspace
                .active_pane()
                .read(cx)
                .active_item()
                .is_some_and(|item| item.act_as::<TerminalView>(cx).is_some());
            if !active_item_is_terminal {
                return false;
            }
            create_split_terminal(workspace, direction, window, cx).detach_and_log_err(cx);
            true
        })
        .unwrap_or(false);
    if intercepted {
        cx.stop_propagation();
    } else {
        cx.propagate();
    }
}

/// Spawn one shell with one pending route capability. The registration is
/// staged against the resulting Terminal entity before TerminalView can emit
/// ItemAdded; NotificationRuntime binds and activates it only after the exact
/// `(window, logical workspace, item)` target exists.
pub(super) fn create_terminal_with_cli_route(
    project: &mut Project,
    working_directory: Option<PathBuf>,
    cx: &mut Context<Project>,
) -> Task<anyhow::Result<gpui::Entity<terminal::Terminal>>> {
    let Some(server) = cx.try_global::<CliServer>() else {
        return project.create_terminal_shell(working_directory, cx);
    };
    let registration = match server.register_route() {
        Ok(registration) => Arc::new(registration),
        Err(error) => return Task::ready(Err(error)),
    };
    let endpoint = registration.endpoint_env();
    let route_id = registration.route_id();
    let spawn = task::SpawnInTerminal {
        id: task::TaskId(format!("zmux-shell-{route_id}")),
        full_label: "Terminal".to_owned(),
        label: "Terminal".to_owned(),
        command_label: "Terminal".to_owned(),
        cwd: working_directory,
        env: terminal_env_with_notification_endpoint(Some(&endpoint)),
        use_new_terminal: true,
        allow_concurrent_runs: true,
        shell: task::Shell::System,
        ..Default::default()
    };
    let terminal_task = project.create_terminal_task(spawn, cx);

    cx.spawn(async move |_project, cx| {
        let terminal = terminal_task.await?;
        cx.update(|cx| {
            NotificationRuntime::stage_cli_route(&terminal, registration, cx);
        });
        Ok(terminal)
    })
}

#[cfg(test)]
mod tests {
    use super::{MAX_RESTORED_TERMINAL_ATTEMPTS, restored_terminal_retry_delay};
    use std::time::Duration;

    #[test]
    fn restored_terminal_retries_back_off_with_a_strict_cap() {
        assert_eq!(MAX_RESTORED_TERMINAL_ATTEMPTS, 3);
        assert_eq!(restored_terminal_retry_delay(1), Duration::from_secs(1));
        assert_eq!(restored_terminal_retry_delay(2), Duration::from_secs(2));
        assert_eq!(restored_terminal_retry_delay(6), Duration::from_secs(32));
        assert_eq!(restored_terminal_retry_delay(100), Duration::from_secs(32));
    }
}
