use std::sync::Arc;

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
use terminal_view::{default_working_directory, terminal_panel::TerminalPanel};
use workspace::{AppState, MultiWorkspace, OpenMode, OpenResult, Workspace, WorkspaceStore};

use crate::cli_server::CliServer;
use crate::env::terminal_env;
use crate::keymap::{NewTerminal, Quit, configure_keybindings, configure_zoom_actions};
use crate::notifications::{NotificationSource, NotificationStore};
use crate::theme::configure_terminal_fonts;
use crate::welcome::ZmuxWelcome;
use crate::workspaces::{
    ActivateNextWorkspace, ActivatePreviousWorkspace, NewWorkspace, ToggleWorkspacesPanel,
    WorkspacesPanel,
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
    if !cx.has_global::<db::AppDatabase>() {
        cx.set_global(init_database());
    }

    if !cx.has_global::<NotificationStore>() {
        cx.set_global(NotificationStore::new());
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

    configure_keybindings(cx);
    configure_zoom_actions(cx);

    cx.on_action(|_: &Quit, cx| cx.quit());

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

pub fn open_zmux_workspace(
    requesting_window: Option<WindowHandle<MultiWorkspace>>,
    cx: &mut App,
) -> Task<anyhow::Result<OpenResult>> {
    let app_state = AppState::global(cx);

    let initial_dir = crate::env::current_working_directory()
        .map(|p| vec![p])
        .unwrap_or_default();

    Workspace::new_local(
        initial_dir,
        app_state,
        requesting_window,
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
            let cli_server = CliServer::start(workspace.weak_handle(), panel.downgrade(), cx);
            cx.set_global(cli_server);

            workspace.register_action(|workspace, _: &NewTerminal, window, cx| {
                create_center_terminal(workspace, window, cx).detach_and_log_err(cx);
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
                panel.update(cx, |_, cx| cx.notify());
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
    let working_directory = default_working_directory(workspace, cx);
    TerminalPanel::add_center_terminal(workspace, window, cx, move |project, cx| {
        project.create_terminal_shell(working_directory, cx)
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
