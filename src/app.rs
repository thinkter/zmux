use std::{path::PathBuf, sync::Arc};

use client::{Client, UserStore};
use db::kvp::KeyValueStore;
use fs::Fs;
use gpui::{
    App, AppContext, Bounds, Context, InteractiveElement, Task, TaskExt, UpdateGlobal, WeakEntity,
    Window, WindowBounds, WindowHandle, WindowOptions, actions, px, size,
};
use gpui_platform::application;
use http_client::{BlockedHttpClient, HttpClientWithUrl};
use project::Project;
use settings::Settings;
use terminal_view::{TerminalView, default_working_directory};
use workspace::pane::{
    SplitDown as PaneSplitDown, SplitHorizontal as PaneSplitHorizontal, SplitLeft as PaneSplitLeft,
    SplitMode, SplitRight as PaneSplitRight, SplitUp as PaneSplitUp,
    SplitVertical as PaneSplitVertical,
};
use workspace::pane_group::SplitDirection;
use workspace::{
    ActivatePane as WorkspaceActivatePane, AppState, MultiWorkspace,
    NewCenterTerminal as WorkspaceNewCenterTerminal, NewTerminal as WorkspaceNewTerminal, OpenMode,
    OpenResult, Workspace, WorkspaceStore,
};

use crate::cli_server::CliServer;
use crate::env::{terminal_env, terminal_env_with_notification_endpoint};
use crate::keymap::{
    NewTerminal, Quit, SplitTerminalDown, SplitTerminalRight, configure_keybindings,
    configure_zoom_actions,
};
use crate::notification_runtime::NotificationRuntime;
use crate::notifications::{NotificationTarget, WorkspaceId};
use crate::theme::configure_terminal_fonts;
use crate::welcome::ZmuxWelcome;
use crate::workspaces::{
    ActivateNextWorkspace, ActivatePreviousWorkspace, NewWorkspace, ToggleNotificationCenter,
    ToggleWorkspacesPanel, WorkspacesPanel,
};

actions!(zmux, [NotifyCurrentPane, JumpToLatestNotification]);

pub fn run() -> anyhow::Result<()> {
    application()
        .with_assets(crate::assets::Assets)
        .run(|cx: &mut App| {
            let app_state = init_zmux(cx);
            load_user_settings(app_state.fs.clone(), cx);

            cx.spawn(async move |cx| {
                let open_task = cx.update(|cx| open_zmux_workspace(None, cx));
                if let Err(error) = open_task.await {
                    cx.update(|cx| {
                        eprintln!("failed to open zmux workspace: {error}");
                        cx.quit();
                    });
                }
            })
            .detach();
        });

    Ok(())
}

pub fn init_zmux(cx: &mut App) -> Arc<AppState> {
    if !cx.has_global::<db::AppDatabase>() {
        cx.set_global(db::AppDatabase::new());
    }

    if let Err(error) = crate::fonts::load_embedded_fonts(cx) {
        eprintln!("failed to load embedded fonts: {error:#}");
    }

    settings::init(cx);
    theme_settings::init(theme::LoadThemes::JustBase, cx);
    editor::init(cx);
    terminal::terminal_settings::TerminalSettings::register(cx);
    configure_terminal_fonts(cx);

    let app_state = init_app_state(cx);
    Project::init(&app_state.client, cx);
    client::init(&app_state.client, cx);
    workspace::init(app_state.clone(), cx);
    terminal_view::init(cx);
    terminal_view::set_terminal_creation_handler(
        Arc::new(|project, working_directory, cx| {
            project
                .update(cx, |project, cx| {
                    create_terminal_with_cli_route(project, working_directory, cx)
                })
                .unwrap_or_else(|error| Task::ready(Err(error)))
        }),
        cx,
    );
    NotificationRuntime::init(cx);

    if !cx.has_global::<CliServer>() {
        match CliServer::start() {
            Ok(server) => {
                let receiver = server.receiver();
                cx.set_global(server);
                cx.spawn(async move |cx| {
                    while let Ok(received) = receiver.recv().await {
                        let crate::cli_server::ReceivedCliNotification {
                            route_id,
                            notification,
                            completion,
                            ..
                        } = received;
                        if !completion.begin_recording() {
                            // The server deadline won while this event waited
                            // on GPUI. A timed-out CLI request must never be
                            // published later and turn a retry into a duplicate.
                            continue;
                        }
                        if cx.update(|cx| {
                            NotificationRuntime::publish_cli(route_id, notification, cx).is_some()
                        }) {
                            completion.recorded();
                        } else {
                            completion.reject("unknown or stale terminal route");
                        }
                    }
                })
                .detach();
            }
            Err(error) => {
                eprintln!("failed to start the zmux notification endpoint: {error:#}");
            }
        }
    }

    configure_keybindings(cx);
    configure_zoom_actions(cx);

    cx.on_action(|_: &Quit, cx| cx.quit());

    app_state
}

/// Seed `paths::settings_file()` with the zmux defaults on first run, then
/// load and watch it through Zed's settings machinery: hand edits and GUI
/// edits both re-apply live (the watcher refreshes every window).
///
/// Only the real application calls this; tests configure settings directly
/// via [`configure_terminal_fonts`] and must never touch on-disk config.
pub fn load_user_settings(fs: Arc<dyn Fs>, cx: &mut App) {
    let settings_path = paths::settings_file();
    if !settings_path.exists() {
        let seeded = settings_path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| std::fs::write(settings_path, crate::theme::default_settings_json()));
        if let Err(error) = seeded {
            eprintln!(
                "failed to create the settings file at {}: {error:#}",
                settings_path.display()
            );
        }
    }

    settings::SettingsStore::update_global(cx, |store, cx| {
        store.watch_settings_files(fs, cx, |file, result, _cx| {
            if let settings::ParseStatus::Failed { error } = &result.parse_status {
                eprintln!("failed to parse {file:?} settings: {error}");
            }
        });
    });
}

pub fn open_zmux_workspace(
    requesting_window: Option<WindowHandle<MultiWorkspace>>,
    cx: &mut App,
) -> Task<anyhow::Result<OpenResult>> {
    let initial_dir = crate::env::current_working_directory()
        .map(|path| vec![path])
        .unwrap_or_default();
    open_zmux_workspace_for_paths(requesting_window, initial_dir, cx)
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
    open_zmux_workspace_for_paths(requesting_window, vec![initial_dir], cx)
}

fn open_zmux_workspace_for_paths(
    requesting_window: Option<WindowHandle<MultiWorkspace>>,
    initial_dirs: Vec<PathBuf>,
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
        Some(Box::new(|workspace, window, cx| {
            let welcome = cx.new(ZmuxWelcome::new);
            let center_pane = workspace.active_pane().clone();
            center_pane.update(cx, |pane, cx| {
                pane.set_should_display_welcome_page(false);
                pane.add_item(Box::new(welcome), true, true, None, window, cx);
            });
            workspace
                .bottom_dock()
                .update(cx, |dock, cx| dock.set_open(false, window, cx));

            let panel = cx.new(|cx| WorkspacesPanel::new(workspace.weak_handle(), window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            workspace.open_panel::<WorkspacesPanel>(window, cx);
            NotificationRuntime::attach_workspace(cx.entity(), panel.clone(), window, cx);

            workspace.register_action(|workspace, _: &NewTerminal, window, cx| {
                create_center_terminal(workspace, window, cx).detach_and_log_err(cx);
            });
            workspace.register_action(|workspace, _: &SplitTerminalRight, window, cx| {
                create_split_terminal(workspace, SplitDirection::Right, window, cx)
                    .detach_and_log_err(cx);
            });
            workspace.register_action(|workspace, _: &SplitTerminalDown, window, cx| {
                create_split_terminal(workspace, SplitDirection::Down, window, cx)
                    .detach_and_log_err(cx);
            });
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
                    capture_terminal_clone_split(
                        &split_up,
                        action.mode,
                        SplitDirection::Up,
                        window,
                        cx,
                    );
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
                    capture_terminal_clone_split(
                        &split_horizontal,
                        action.mode,
                        direction,
                        window,
                        cx,
                    );
                })
                .capture_action(move |action: &PaneSplitVertical, window, cx| {
                    let direction = SplitDirection::vertical(cx);
                    capture_terminal_clone_split(
                        &split_vertical,
                        action.mode,
                        direction,
                        window,
                        cx,
                    );
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
                            panel.update(cx, |panel, cx| panel.create_workspace(window, cx));
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
                let window_handle = window.window_handle();
                cx.defer(move |cx| {
                    window_handle
                        .update(cx, |_, window, cx| {
                            panel.update(cx, |panel, cx| panel.activate_next_workspace(window, cx));
                        })
                        .ok();
                });
            });
            workspace.register_action(|workspace, _: &ActivatePreviousWorkspace, window, cx| {
                let Some(panel) = workspace.panel::<WorkspacesPanel>(cx) else {
                    return;
                };
                let window_handle = window.window_handle();
                cx.defer(move |cx| {
                    window_handle
                        .update(cx, |_, window, cx| {
                            panel.update(cx, |panel, cx| {
                                panel.activate_previous_workspace(window, cx)
                            });
                        })
                        .ok();
                });
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
            create_center_terminal(workspace, window, cx).detach_and_log_err(cx);
        })),
        OpenMode::NewWindow,
        cx,
    )
}

pub(crate) fn create_center_terminal(
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
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<anyhow::Result<WeakEntity<terminal::Terminal>>> {
    let working_directory = default_working_directory(workspace, cx);
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

fn create_split_terminal(
    workspace: &mut Workspace,
    direction: SplitDirection,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<anyhow::Result<()>> {
    let working_directory = default_working_directory(workspace, cx);
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

/// Zed's pane-number action creates a clone when the requested index does not
/// exist. Task-backed terminal cloning falls back to a plain shell and loses
/// the per-terminal endpoint, so provision that implicit split ourselves.
fn capture_missing_terminal_pane_activation(
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
fn capture_workspace_terminal_creation(
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

fn capture_terminal_clone_split(
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
fn create_terminal_with_cli_route(
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

fn init_app_state(cx: &mut App) -> Arc<AppState> {
    let fs: Arc<dyn Fs> = Arc::new(fs::RealFs::new(None, cx.background_executor().clone()));
    <dyn Fs>::set_global(fs.clone(), cx);

    let languages = Arc::new(language::LanguageRegistry::new(
        cx.background_executor().clone(),
    ));
    let http = Arc::new(HttpClientWithUrl::new(
        Arc::new(BlockedHttpClient::new()),
        "http://localhost",
        None,
    ));
    let client = Client::new(Arc::new(clock::RealSystemClock), http, cx);
    Client::set_global(client.clone(), cx);

    let session = cx.foreground_executor().block_on(session::Session::new(
        format!("zmux-{}", std::process::id()),
        KeyValueStore::global(cx),
    ));
    let session = cx.new(|cx| session::AppSession::new(session, cx));
    let user_store = cx.new(|cx| UserStore::new(client.clone(), cx));
    let workspace_store = cx.new(|cx| WorkspaceStore::new(client.clone(), cx));

    let app_state = Arc::new(AppState {
        languages,
        client,
        user_store,
        workspace_store,
        fs,
        build_window_options,
        node_runtime: node_runtime::NodeRuntime::unavailable(),
        session,
    });
    AppState::set_global(app_state.clone(), cx);
    app_state
}

fn build_window_options(_display: Option<uuid::Uuid>, cx: &mut App) -> WindowOptions {
    let bounds = Bounds::centered(None, size(px(960.0), px(640.0)), cx);
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        ..Default::default()
    }
}
