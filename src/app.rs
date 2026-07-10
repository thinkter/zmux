use std::{path::PathBuf, sync::Arc, time::Duration};

use client::{Client, UserStore};
use db::kvp::KeyValueStore;
use fs::Fs;
use gpui::{
    App, AppContext, Bounds, Context, Task, TaskExt, WeakEntity, Window, WindowBounds,
    WindowHandle, WindowOptions, actions, px, size,
};
use gpui_platform::application;
use http_client::{BlockedHttpClient, HttpClientWithUrl};
use project::Project;
use settings::Settings;
use terminal_view::default_working_directory;
use workspace::{AppState, MultiWorkspace, OpenMode, OpenResult, Workspace, WorkspaceStore};

use crate::cli_server::CliServer;
use crate::config::{ConfigError, ConfigPathProvider, ConfigPaths, ConfigReload, ConfigStore};
use crate::env::terminal_env_with_notification_endpoint;
use crate::keymap::{
    NewTerminal, OpenKeymaps, OpenSettings, Quit, ReloadConfig, ResetConfig,
    configure_keybindings_with_config, configure_zoom_actions,
};
use crate::metadata::{NotificationSummary, WorkspaceMetadataStore};
use crate::notifications::{NotificationSource, NotificationStore};
use crate::settings_editor::{SettingsEditorMode, ZmuxSettingsEditor};
use crate::theme::configure_terminal_fonts_with_config;
use crate::welcome::ZmuxWelcome;
use crate::workspaces::{
    ActivateNextWorkspace, ActivatePreviousWorkspace, NewWorkspace, TerminalTarget,
    ToggleWorkspacesPanel, WorkspacesPanel, restore_startup_layout,
};

actions!(zmux, [NotifyCurrentPane, JumpToLatestNotification]);

pub fn run() -> anyhow::Result<()> {
    application()
        .with_assets(crate::assets::Assets)
        .run(|cx: &mut App| {
            init_zmux(cx);

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
    init_zmux_with_config_paths(
        ConfigPaths::new(paths::config_dir().join("config.json")),
        cx,
    )
}

/// Initialize using a caller-owned location. A future shared `ZmuxPaths`
/// implementation can satisfy [`ConfigPathProvider`] without this module
/// knowing about Zed's path layer.
pub fn init_zmux_with_config_path_provider(
    provider: &impl ConfigPathProvider,
    cx: &mut App,
) -> Arc<AppState> {
    init_zmux_with_config_paths(ConfigPaths::from_provider(provider), cx)
}

pub fn init_zmux_with_config_paths(paths: ConfigPaths, cx: &mut App) -> Arc<AppState> {
    if !cx.has_global::<db::AppDatabase>() {
        cx.set_global(init_database());
    }

    if !cx.has_global::<ConfigStore>() {
        cx.set_global(ConfigStore::load_or_default(paths));
    }

    if !cx.has_global::<WorkspaceMetadataStore>() {
        let config = ConfigStore::global(cx).config().clone();
        cx.set_global(WorkspaceMetadataStore::new(
            config.sidebar.max_log_entries,
            Duration::from_secs(config.sidebar.metadata_refresh_seconds),
        ));
    }

    if !cx.has_global::<NotificationStore>() {
        cx.set_global(NotificationStore::new());
    }

    settings::init(cx);
    theme_settings::init(theme::LoadThemes::JustBase, cx);
    editor::init(cx);
    terminal::terminal_settings::TerminalSettings::register(cx);
    let config = ConfigStore::global(cx).config().clone();
    configure_terminal_fonts_with_config(&config.terminal, cx);

    let app_state = init_app_state(cx);
    Project::init(&app_state.client, cx);
    client::init(&app_state.client, cx);
    workspace::init(app_state.clone(), cx);
    terminal_view::init(cx);

    configure_keybindings_with_config(&config, cx);
    configure_zoom_actions(cx);

    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &ReloadConfig, cx| {
        let _ = reload_zmux_config(cx);
    });
    cx.on_action(|_: &ResetConfig, cx| {
        if ConfigStore::global_mut(cx).reset().is_ok() {
            apply_zmux_config(cx);
        }
    });
    start_config_watcher(cx);

    app_state
}

/// Open the application database under zmux's namespaced data directory.
///
/// `paths` is a small, pinned fork of Zed's path crate with `APP_NAME =
/// "Zmux"`, so this also keeps the shared key-value store used by sessions
/// separate from any Zed installation.
fn init_database() -> db::AppDatabase {
    let connection = gpui::block_on(db::open_db::<db::AppMigrator>(
        paths::database_dir(),
        *db::RELEASE_CHANNEL,
    ));
    db::AppDatabase(connection)
}

/// Reapply every runtime-owned setting from the already validated config.
/// Invalid disk edits never get here because [`ConfigStore`] retains the last
/// known-good document until a later reload parses successfully.
pub(crate) fn apply_zmux_config(cx: &mut App) {
    let config = ConfigStore::global(cx).config().clone();
    configure_keybindings_with_config(&config, cx);
    configure_terminal_fonts_with_config(&config.terminal, cx);
    WorkspaceMetadataStore::global_mut(cx).configure(
        config.sidebar.max_log_entries,
        Duration::from_secs(config.sidebar.metadata_refresh_seconds),
    );
    cx.refresh_windows();
}

pub(crate) fn reload_zmux_config(cx: &mut App) -> Result<ConfigReload, ConfigError> {
    let reload = ConfigStore::global_mut(cx).reload()?;
    apply_zmux_config(cx);
    Ok(reload)
}

/// A small polling watcher avoids coupling this feature to a platform-specific
/// filesystem watcher. It reads only Zmux's tiny config file and uses
/// content-based change detection, so saves with coarse mtimes still reload.
struct ConfigWatcher {
    _task: Task<()>,
}

impl gpui::Global for ConfigWatcher {}

fn start_config_watcher(cx: &mut App) {
    if cx.has_global::<ConfigWatcher>() {
        return;
    }
    let task = cx.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(750))
                .await;
            cx.update(|cx| match ConfigStore::global_mut(cx).reload_if_changed() {
                Ok(Some(_)) => apply_zmux_config(cx),
                Ok(None) => {}
                Err(_) => {
                    // The error is retained by ConfigStore for the settings
                    // editor; redraw so an open editor can show current state.
                    cx.refresh_windows();
                }
            });
        }
    });
    cx.set_global(ConfigWatcher { _task: task });
}

pub fn open_zmux_workspace(
    requesting_window: Option<WindowHandle<MultiWorkspace>>,
    cx: &mut App,
) -> Task<anyhow::Result<OpenResult>> {
    let app_state = AppState::global(cx);

    // The first workspace owns a per-process notification endpoint. Later
    // windows reuse that endpoint so all terminal children route to the same
    // running zmux instance instead of replacing or unlinking one another.
    let (cli_server, notification_endpoint) = if cx.has_global::<CliServer>() {
        (
            None,
            Some(cx.global::<CliServer>().endpoint().as_str().to_owned()),
        )
    } else {
        match CliServer::prepare() {
            Ok(server) => {
                let endpoint = server.endpoint().as_str().to_owned();
                (Some(server), Some(endpoint))
            }
            Err(error) => {
                eprintln!("failed to start zmux notification endpoint: {error:#}");
                (None, None)
            }
        }
    };

    let initial_dir = crate::env::current_working_directory()
        .map(|p| vec![p])
        .unwrap_or_default();

    Workspace::new_local(
        initial_dir,
        app_state,
        requesting_window,
        Some(terminal_env_with_notification_endpoint(
            notification_endpoint.as_deref(),
        )),
        Some(Box::new(move |workspace, window, cx| {
            workspace
                .bottom_dock()
                .update(cx, |dock, cx| dock.set_open(false, window, cx));

            let panel = cx.new(|cx| WorkspacesPanel::new(workspace.weak_handle(), window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            workspace.open_panel::<WorkspacesPanel>(window, cx);
            if let Some(cli_server) = cli_server {
                let cli_server = cli_server.start(workspace.weak_handle(), panel.downgrade(), cx);
                cx.set_global(cli_server);
            }

            let active_workspace_id = panel.read(cx).active_workspace_id();
            let startup_restore = panel.update(cx, |panel, _| panel.take_initial_restore());
            if let Some(layout) = startup_restore {
                panel.update(cx, |panel, _| panel.begin_session_restore());
                let (surface_ids, terminals) =
                    restore_startup_layout(workspace, layout, active_workspace_id, window, cx);
                let panes = workspace.panes().to_vec();
                panel.update(cx, |panel, cx| {
                    panel.install_restored_surfaces(surface_ids, panes);
                    panel.finish_session_restore(cx);
                });
                for (target, working_directory) in terminals {
                    create_center_terminal(
                        workspace,
                        panel.downgrade(),
                        target,
                        working_directory,
                        window,
                        cx,
                    )
                    .detach_and_log_err(cx);
                }
            } else {
                let welcome = cx.new(ZmuxWelcome::new);
                let center_pane = workspace.active_pane().clone();
                center_pane.update(cx, |pane, cx| {
                    pane.set_should_display_welcome_page(false);
                    pane.add_item(Box::new(welcome), true, true, None, window, cx);
                });
                let target =
                    panel.update(cx, |panel, _| panel.register_initial_surface(center_pane));
                create_center_terminal(
                    workspace,
                    panel.downgrade(),
                    target,
                    default_working_directory(workspace, cx),
                    window,
                    cx,
                )
                .detach_and_log_err(cx);
            }

            workspace.register_action(|workspace, _: &NewTerminal, window, cx| {
                let Some(panel) = workspace.panel::<WorkspacesPanel>(cx) else {
                    return;
                };
                let pane = workspace.active_pane().clone();
                let target = panel.update(cx, |panel, _| panel.active_terminal_target(pane));
                create_center_terminal(
                    workspace,
                    panel.downgrade(),
                    target,
                    default_working_directory(workspace, cx),
                    window,
                    cx,
                )
                .detach_and_log_err(cx);
            });
            workspace.register_action(|workspace, _: &OpenSettings, window, cx| {
                open_settings_editor(workspace, SettingsEditorMode::Settings, window, cx);
            });
            workspace.register_action(|workspace, _: &OpenKeymaps, window, cx| {
                open_settings_editor(workspace, SettingsEditorMode::Keymaps, window, cx);
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
            workspace.register_action(|workspace, _: &NotifyCurrentPane, _window, cx| {
                if !ConfigStore::global(cx).config().notifications.enabled {
                    return;
                }
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
                NotificationStore::global_mut(cx).add(
                    item_id,
                    Some(workspace_id),
                    NotificationSource::Manual,
                    "Manual notification".to_string(),
                    "Test notification from current pane".to_string(),
                );
                update_workspace_notification_metadata(workspace_id, cx);
                panel.update(cx, |_, cx| cx.notify());
            });
            workspace.register_action(|workspace, _: &JumpToLatestNotification, window, cx| {
                let Some(notification) = NotificationStore::global(cx).latest_unread().cloned()
                else {
                    return;
                };
                let Some(panel) = workspace.panel::<WorkspacesPanel>(cx) else {
                    return;
                };

                if let Some(workspace_id) = notification.workspace_id {
                    let is_active = panel.read(cx).active_workspace_id() == workspace_id;
                    if !is_active {
                        panel.update(cx, |panel, cx| {
                            panel.activate_workspace(workspace_id, window, cx);
                        });
                    }
                }

                if let Some(pane) = workspace.pane_for_item_id(notification.item_id) {
                    let index = pane
                        .read(cx)
                        .items()
                        .position(|item| item.item_id() == notification.item_id);
                    if let Some(index) = index {
                        pane.update(cx, |pane, cx| {
                            pane.activate_item(index, true, true, window, cx);
                        });
                    }
                }

                NotificationStore::global_mut(cx).mark_pane_read(notification.item_id);
                if let Some(workspace_id) = notification.workspace_id {
                    update_workspace_notification_metadata(workspace_id, cx);
                }
                panel.update(cx, |_, cx| cx.notify());
            });
        })),
        OpenMode::NewWindow,
        cx,
    )
}

pub(crate) fn create_center_terminal(
    workspace: &mut Workspace,
    panel: WeakEntity<WorkspacesPanel>,
    target: TerminalTarget,
    working_directory: Option<PathBuf>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<anyhow::Result<WeakEntity<terminal::Terminal>>> {
    let project = workspace.project().downgrade();
    let workspace_handle = workspace.weak_handle();
    let workspace_id = workspace.database_id();
    cx.spawn_in(window, async move |_workspace, cx| {
        let terminal = project
            .update(cx, |project, cx| {
                project.create_terminal_shell(working_directory, cx)
            })?
            .await?;
        panel.update_in(cx, |panel, window, cx| {
            let terminal_view = cx.new(|cx| {
                terminal_view::TerminalView::new(
                    terminal.clone(),
                    workspace_handle,
                    workspace_id,
                    project,
                    window,
                    cx,
                )
            });
            panel.attach_terminal(target, Box::new(terminal_view), window, cx);
        })?;
        Ok(terminal.downgrade())
    })
}

fn open_settings_editor(
    workspace: &mut Workspace,
    mode: SettingsEditorMode,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let editor = cx.new(|cx| ZmuxSettingsEditor::new(mode, window, cx));
    let pane = workspace.active_pane().clone();
    pane.update(cx, |pane, cx| {
        pane.add_item(Box::new(editor), true, true, None, window, cx);
    });
}

pub(crate) fn update_workspace_notification_metadata(
    workspace_id: crate::notifications::WorkspaceId,
    cx: &mut App,
) {
    let unread_count = NotificationStore::global(cx).workspace_unread_count(workspace_id);
    let latest = NotificationStore::global(cx)
        .latest_unread_for_workspace(workspace_id)
        .map(|notification| NotificationSummary {
            title: notification.title.clone(),
            body: notification.body.clone(),
        });
    let _ = WorkspaceMetadataStore::global_mut(cx).set_notification_summary(
        workspace_id,
        unread_count,
        latest,
    );
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
