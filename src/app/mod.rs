//! Application bootstrap: the GPUI entry point ([`run`]), one-time global
//! initialization ([`init_zmux`]), user-settings loading, and app-state
//! construction. Window/workspace opening lives in [`open`], terminal
//! creation and CLI-route glue in [`terminal`], and zmux action registration
//! in [`actions`].

mod actions;
mod open;
mod terminal;

pub use self::actions::{JumpToLatestNotification, NotifyCurrentPane};
pub use self::open::{open_zmux_workspace, open_zmux_workspace_at};
pub(crate) use self::terminal::{
    create_center_terminal_at_for_workspace, create_center_terminal_for_workspace,
    create_restored_terminals_for_workspace,
};

use std::sync::Arc;

use client::{Client, UserStore};
use db::kvp::KeyValueStore;
use fs::Fs;
use gpui::{App, AppContext, Bounds, Task, UpdateGlobal, WindowBounds, WindowOptions, px, size};
use gpui_platform::application;
use http_client::HttpClientWithUrl;
use project::Project;
use settings::Settings;
use theme::ActiveTheme as _;
use workspace::{AppState, WorkspaceStore};

use crate::cli_server::CliServer;
use crate::keymap::{Quit, configure_keybindings, configure_zoom_actions};
use crate::notification_runtime::NotificationRuntime;
use crate::notifications::NotificationStore;
use crate::theme::{DEFAULT_THEME, configure_terminal_fonts};
use crate::workspaces::{WorkspacesPanel, install_git_repository_scope};

use self::terminal::create_terminal_with_cli_route;

pub fn run() -> anyhow::Result<()> {
    application()
        .with_assets(crate::assets::Assets)
        .run(|cx: &mut App| {
            crate::app_icon::configure_native_app_icon();
            let initialize = init_zmux(cx);
            cx.spawn(async move |cx| {
                let app_state = initialize.await;
                cx.update(|cx| load_user_settings(app_state.fs.clone(), cx));
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

/// Start Zmux's one-time global initialization.
///
/// The returned task must complete before opening a workspace. Session state
/// is loaded on the background executor; all registrations that depend on the
/// completed [`AppState`] are then installed in their original order on GPUI's
/// update path.
pub fn init_zmux(cx: &mut App) -> Task<Arc<AppState>> {
    if !cx.has_global::<db::AppDatabase>() {
        cx.set_global(db::AppDatabase::new());
    }

    if let Err(error) = crate::fonts::load_embedded_fonts(cx) {
        eprintln!("failed to load embedded fonts: {error:#}");
    }
    crate::visual_power::VisualPowerMonitor::init(cx);
    // zmux tracks Zed's extension API without shipping a Zed release channel.
    // Use the development API range so current gallery packages (including HTML's
    // WASM API v0.7) are admitted by the pinned host.
    release_channel::init_test(
        env!("CARGO_PKG_VERSION")
            .parse()
            .expect("Cargo package versions are valid semver"),
        release_channel::ReleaseChannel::Dev,
        cx,
    );
    gpui_tokio::init(cx);

    settings::init(cx);
    theme_settings::init(theme::LoadThemes::JustBase, cx);
    editor::init(cx);
    ::terminal::terminal_settings::TerminalSettings::register(cx);
    configure_terminal_fonts(cx);

    let app_state = init_app_state(cx);
    cx.spawn(async move |cx| {
        let app_state = app_state.await;
        cx.update(|cx| finish_zmux_init(app_state, cx))
    })
}

fn finish_zmux_init(app_state: Arc<AppState>, cx: &mut App) -> Arc<AppState> {
    crate::extensions::init(&app_state, cx);
    crate::syntax::register_builtin_languages(&app_state.languages);
    // Grammars alone don't produce colors: the registry needs the active theme
    // to build its highlight maps, and must rebuild them on theme changes.
    app_state.languages.set_theme(cx.theme().clone());
    cx.observe_global::<theme::GlobalTheme>({
        let languages = app_state.languages.clone();
        move |cx| languages.set_theme(cx.theme().clone())
    })
    .detach();
    extensions_ui::init(cx);
    Project::init(&app_state.client, cx);
    client::init(&app_state.client, cx);
    workspace::init(app_state.clone(), cx);
    extensions_ui::init(cx);
    command_palette::init(cx);
    vim::init(cx);
    git_ui::init(cx);
    project_panel::init(cx);
    install_git_repository_scope(cx);
    tab_switcher::init(cx);
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
    terminal_view::set_terminal_directory_open_handler(
        Arc::new(|workspace, directory, window, cx| {
            let Some(workspace) = workspace.upgrade() else {
                return false;
            };
            let Some(panel) = workspace.read(cx).panel::<WorkspacesPanel>(cx) else {
                return false;
            };
            panel.update(cx, |panel, cx| {
                panel.open_directory_workspace(directory, window, cx);
            });
            true
        }),
        cx,
    );
    NotificationRuntime::init(cx);
    terminal_view::set_terminal_tab_indicator_handler(
        Arc::new(|item_id, cx| {
            NotificationStore::global(cx)
                .read(cx)
                .item_has_unread(item_id)
        }),
        cx,
    );

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
        store.watch_settings_files(fs.clone(), cx, |file, result, _cx| {
            if let settings::ParseStatus::Failed { error } = &result.parse_status {
                eprintln!("failed to parse {file:?} settings: {error}");
            }
        });
    });

    // Existing zmux settings predate the bundled theme. Give those installs the
    // new default too, while preserving any theme the user explicitly selected.
    settings::update_settings_file(fs, cx, |content, _cx| {
        if content.theme.theme.is_none() {
            content.theme.theme = Some(settings::ThemeSelection::Static(settings::ThemeName(
                DEFAULT_THEME.into(),
            )));
        }
        clear_legacy_font_fallbacks(content);
    });
}

/// Earlier builds seeded every settings file with a Linux-oriented font
/// fallback list. Zed leaves this unset so GPUI can use the native platform
/// cascade, so remove the exact seeded list while preserving hand-edited ones.
fn clear_legacy_font_fallbacks(content: &mut settings::SettingsContent) {
    let legacy_fallbacks = ["Lilex", "Noto Sans Mono", "Noto Color Emoji", "monospace"];
    let is_legacy = |fallbacks: &Option<Vec<settings::FontFamilyName>>| {
        fallbacks
            .as_ref()
            .is_some_and(|list| list.iter().map(AsRef::as_ref).eq(legacy_fallbacks))
    };
    if is_legacy(&content.theme.buffer_font_fallbacks) {
        content.theme.buffer_font_fallbacks = None;
    }
    if let Some(terminal) = content.terminal.as_mut()
        && is_legacy(&terminal.font_fallbacks)
    {
        terminal.font_fallbacks = None;
    }
}

fn init_app_state(cx: &mut App) -> Task<Arc<AppState>> {
    let fs: Arc<dyn Fs> = Arc::new(fs::RealFs::new(None, cx.background_executor().clone()));
    <dyn Fs>::set_global(fs.clone(), cx);

    let languages = Arc::new(language::LanguageRegistry::new(
        cx.background_executor().clone(),
    ));
    let http = Arc::new(HttpClientWithUrl::new(
        Arc::new(reqwest_client::ReqwestClient::new()),
        "https://zed.dev",
        None,
    ));
    let client = Client::new(Arc::new(clock::RealSystemClock), http, cx);
    Client::set_global(client.clone(), cx);

    let session = cx.background_spawn(session::Session::new(
        format!("zmux-{}", std::process::id()),
        KeyValueStore::global(cx),
    ));

    cx.spawn(async move |cx| {
        let session = session.await;
        cx.update(|cx| {
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
        })
    })
}

fn build_window_options(_display: Option<uuid::Uuid>, cx: &mut App) -> WindowOptions {
    let bounds = Bounds::centered(None, size(px(960.0), px(640.0)), cx);
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        app_id: Some(crate::desktop_notifications::ZMUX_APPLICATION_ID.to_owned()),
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        icon: crate::app_icon::linux_window_icon(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use workspace::AppState;

    use super::{clear_legacy_font_fallbacks, init_zmux};

    fn fallbacks(names: &[&str]) -> Option<Vec<settings::FontFamilyName>> {
        Some(
            names
                .iter()
                .map(|name| settings::FontFamilyName((*name).into()))
                .collect(),
        )
    }

    const LEGACY: &[&str] = &["Lilex", "Noto Sans Mono", "Noto Color Emoji", "monospace"];

    #[gpui::test]
    async fn initialization_task_installs_the_completed_app_state(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let initialization = cx.update(init_zmux);
        let app_state = initialization.await;

        assert!(cx.read(|cx| Arc::ptr_eq(&app_state, &AppState::global(cx))));
    }

    #[test]
    fn seeded_legacy_font_fallbacks_are_cleared() {
        let mut content = settings::SettingsContent::default();
        content.theme.buffer_font_fallbacks = fallbacks(LEGACY);
        content.terminal.get_or_insert_default().font_fallbacks = fallbacks(LEGACY);

        clear_legacy_font_fallbacks(&mut content);

        assert_eq!(content.theme.buffer_font_fallbacks, None);
        assert_eq!(content.terminal.unwrap().font_fallbacks, None);
    }

    #[test]
    fn hand_edited_font_fallbacks_are_kept() {
        let custom = &["JetBrainsMono Nerd Font"][..];
        let mut content = settings::SettingsContent::default();
        content.theme.buffer_font_fallbacks = fallbacks(custom);
        content.terminal.get_or_insert_default().font_fallbacks = fallbacks(custom);

        clear_legacy_font_fallbacks(&mut content);

        assert_eq!(content.theme.buffer_font_fallbacks, fallbacks(custom));
        assert_eq!(content.terminal.unwrap().font_fallbacks, fallbacks(custom));
    }
}
