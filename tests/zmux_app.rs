mod support;

use std::{path::PathBuf, time::Duration};

use gpui::prelude::*;
use gpui::{Action, Bounds, Keystroke, TestAppContext, VisualTestContext, point, px, size};
use settings::Settings;
use support::deterministic_output_shell;
use terminal::terminal_settings::{AlternateScroll, CursorShape, TerminalSettings};
use terminal::{TerminalBounds, TerminalBuilder};
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use theme::ActiveTheme;
use util::paths::PathStyle;
use workspace::dock::Panel;
use workspace::item::Item as _;
use workspace::pane::{CloseActiveItem, CloseAllItems, CloseOtherItems};
use workspace::{ActivateNextPane, ActivatePreviousPane};
use zmux::{
    CliEndpoint, CliNotification, CliServer, JumpToLatestNotification, NOTIFY_ENDPOINT_ENV,
    NewTerminal as ZmuxNewTerminal, NotificationSource, NotificationStore, NotifyCurrentPane,
    OscNotificationEvent, OscNotificationParser, SplitTerminalDown, SplitTerminalRight,
    WorkspacesPanel, configure_keybindings, configure_terminal_fonts, configure_zoom_actions,
    init_zmux, open_zmux_workspace_at, terminal_env,
};

#[test]
fn terminal_bounds_round_down_to_complete_cells() {
    let bounds = TerminalBounds::new(
        px(10.0),
        px(5.0),
        Bounds::new(point(px(0.0), px(0.0)), size(px(24.9), px(29.9))),
    );

    assert_eq!(bounds.num_columns(), 4);
    assert_eq!(bounds.num_lines(), 2);
}

#[test]
fn terminal_bounds_tolerate_exact_cell_multiples() {
    let bounds = TerminalBounds::new(
        px(18.0),
        px(7.0),
        Bounds::new(point(px(0.0), px(0.0)), size(px(70.0), px(180.0))),
    );

    assert_eq!(bounds.num_columns(), 10);
    assert_eq!(bounds.num_lines(), 10);
}

#[gpui::test]
async fn display_only_terminal_output_is_available_to_zmux(cx: &mut TestAppContext) {
    let terminal = cx.new(|cx| {
        TerminalBuilder::new_display_only(
            CursorShape::Block,
            AlternateScroll::On,
            Some(100),
            0,
            cx.background_executor(),
            PathStyle::local(),
        )
        .subscribe(cx)
    });

    terminal.update(cx, |terminal, cx| {
        terminal.write_output(b"hello zmux\n", cx);
    });

    cx.run_until_parked();

    let output = terminal.read_with(cx, |terminal, _| terminal.get_content());
    assert!(output.contains("hello zmux"), "{output:?}");
}

#[gpui::test]
async fn batched_terminal_osc_notifications_survive_the_title_event_bridge(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let terminal = cx.new(|cx| {
        TerminalBuilder::new_display_only(
            CursorShape::Block,
            AlternateScroll::On,
            Some(100),
            0,
            cx.background_executor(),
            PathStyle::local(),
        )
        .subscribe(cx)
    });

    terminal.update(cx, |terminal, cx| {
        terminal.write_output(
            b"\x1b]9;first build finished\x07\x1b]777;notify;Deploy;production ready\x1b\\",
            cx,
        );
    });
    let mut title = String::new();
    for _ in 0..50 {
        cx.run_until_parked();
        title = terminal.read_with(cx, |terminal, _| terminal.breadcrumb_text.clone());
        if OscNotificationParser::new()
            .push_title(&title)
            .is_ok_and(|events| events.len() == 2)
        {
            break;
        }
        cx.background_executor.timer(Duration::from_millis(5)).await;
    }

    let events = OscNotificationParser::new()
        .push_title(&title)
        .expect("the terminal must expose a valid replay envelope");
    let notifications = events
        .into_iter()
        .map(|event| match event {
            OscNotificationEvent::Notification(notification) => *notification,
            other => panic!("expected a notification, got {other:?}"),
        })
        .collect::<Vec<_>>();

    assert_eq!(
        notifications.len(),
        2,
        "batched title bridge lost an OSC frame: {title:?}; events={notifications:?}"
    );
    assert_eq!(notifications[0].source, NotificationSource::Osc9);
    assert_eq!(notifications[0].body, "first build finished");
    assert_eq!(notifications[1].source, NotificationSource::Osc777);
    assert_eq!(notifications[1].title, "Deploy");
    assert_eq!(notifications[1].body, "production ready");
}

#[gpui::test]
async fn kitty_runtime_responses_bypass_terminal_user_input(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let root = fresh_workspace_root();
    let open_task = cx.update(|cx| {
        init_zmux(cx);
        open_zmux_workspace_at(None, root.path().to_path_buf(), cx)
    });
    let opened = open_task.await.expect("workspace should open");

    for _ in 0..50 {
        cx.run_until_parked();
        let ready = opened.workspace.read_with(cx, |workspace, cx| {
            center_terminal_notification_endpoints(workspace, cx)
                .into_iter()
                .any(|endpoint| endpoint.is_some())
        });
        if ready {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }

    let terminal = opened.workspace.read_with(cx, |workspace, cx| {
        for pane in workspace.panes() {
            for item in pane.read(cx).items() {
                if let Some(view) = item.act_as::<TerminalView>(cx) {
                    return view.read(cx).terminal().clone();
                }
            }
        }
        panic!("workspace has no terminal")
    });
    let mut transcript = (0..200)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    transcript.push_str("selection sentinel\n");
    terminal.update(cx, |terminal, cx| {
        terminal.write_output(transcript.as_bytes(), cx);
        terminal.scroll_to_top();
        terminal.take_input_log();
    });
    cx.run_until_parked();
    assert!(terminal.read_with(cx, |terminal, _| terminal.scrolled_to_top()));

    // Kitty's capability query causes NotificationRuntime to send a protocol
    // response. It must travel through backend PtyWrite, not Terminal::input,
    // which scrolls to the bottom and marks keyboard input.
    terminal.update(cx, |terminal, cx| {
        terminal.write_output(b"\x1b]99;i=query:p=?;\x1b\\", cx);
    });
    for _ in 0..10 {
        cx.run_until_parked();
        cx.background_executor
            .timer(Duration::from_millis(10))
            .await;
    }

    terminal.update(cx, |terminal, _| {
        assert!(
            terminal.take_input_log().is_empty(),
            "protocol responses must not be classified as user input"
        );
        assert!(
            terminal.scrolled_to_top(),
            "protocol responses must not force the viewport to the bottom"
        );
    });
}

#[gpui::test]
async fn notification_shell_tab_indicator_tracks_unread_state(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let root = fresh_workspace_root();
    let open_task = cx.update(|cx| {
        init_zmux(cx);
        open_zmux_workspace_at(None, root.path().to_path_buf(), cx)
    });
    let opened = open_task.await.expect("workspace should open");

    let terminal_view = loop {
        cx.run_until_parked();
        let terminal_view = opened.workspace.read_with(cx, |workspace, cx| {
            workspace.panes().iter().find_map(|pane| {
                pane.read(cx)
                    .items()
                    .find_map(|item| item.act_as::<TerminalView>(cx))
            })
        });
        if let Some(terminal_view) = terminal_view {
            break terminal_view;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    };
    assert!(
        !terminal_view.read_with(cx, |view, cx| view.is_dirty(cx)),
        "a fresh notification shell must not inherit Zed's running-task dot",
    );

    opened
        .window
        .update(cx, |_, window, cx| {
            window.dispatch_action(NotifyCurrentPane.boxed_clone(), cx);
        })
        .expect("window remains open");
    cx.run_until_parked();

    let item_id = terminal_view.entity_id();
    let notification_id = cx.read(|cx| {
        let store = NotificationStore::global(cx).read(cx);
        assert!(store.item_has_unread(item_id));
        store
            .latest_unread()
            .expect("manual pane notification should be recorded")
            .id
    });
    assert!(
        terminal_view.read_with(cx, |view, cx| view.is_dirty(cx)),
        "an unread terminal notification should show the native tab dot",
    );

    cx.update(|cx| {
        NotificationStore::global(cx).update(cx, |store, store_cx| {
            assert!(store.mark_read(notification_id));
            store_cx.notify();
        });
    });
    cx.run_until_parked();
    assert!(
        !terminal_view.read_with(cx, |view, cx| view.is_dirty(cx)),
        "reading the notification should remove the tab dot",
    );
}

#[cfg(unix)]
#[gpui::test]
async fn notification_shell_tab_title_tracks_foreground_process(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let root = fresh_workspace_root();
    let open_task = cx.update(|cx| {
        init_zmux(cx);
        open_zmux_workspace_at(None, root.path().to_path_buf(), cx)
    });
    let opened = open_task.await.expect("workspace should open");

    let terminal_view = loop {
        cx.run_until_parked();
        let terminal_view = opened.workspace.read_with(cx, |workspace, cx| {
            workspace.panes().iter().find_map(|pane| {
                pane.read(cx)
                    .items()
                    .find_map(|item| item.act_as::<TerminalView>(cx))
            })
        });
        if let Some(terminal_view) = terminal_view {
            break terminal_view;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    };
    let terminal = terminal_view.read_with(cx, |view, _| view.terminal().clone());
    terminal.read_with(cx, |terminal, _| {
        let task_id = &terminal
            .task()
            .expect("notification shell should be task-backed")
            .spawned_task
            .id
            .0;
        assert!(
            task_id.starts_with("zmux-shell-"),
            "unexpected task id: {task_id}"
        );
    });
    assert!(
        !terminal_view.read_with(cx, |view, cx| view.is_dirty(cx)),
        "the internal notification wrapper must not show Zed's blue dirty-task indicator",
    );

    let mut cx = VisualTestContext::from_window(opened.window.into(), cx);
    terminal.update(&mut cx, |terminal, _| {
        terminal.input(b"printf shell-ready\r".to_vec());
    });
    cx.update(|window, cx| {
        terminal.update(cx, |terminal, cx| terminal.sync(window, cx));
    });
    cx.run_until_parked();
    let shell_title = root
        .path()
        .file_name()
        .expect("test workspace should have a directory name")
        .to_string_lossy()
        .into_owned();
    wait_for_terminal_tab_title(&terminal, &terminal_view, &shell_title, &mut cx).await;

    terminal.update(&mut cx, |terminal, _| {
        terminal.input(
            b"bash -c 'exec -a nvim bash -c \"while :; do printf .; sleep 0.1; done\"'\r".to_vec(),
        );
    });
    wait_for_terminal_tab_title(&terminal, &terminal_view, "nvim", &mut cx).await;

    terminal.update(&mut cx, |terminal, _| terminal.input(b"\x03".to_vec()));
    wait_for_terminal_tab_title(&terminal, &terminal_view, &shell_title, &mut cx).await;
}

#[gpui::test]
async fn workspace_metadata_click_activates_the_workspace(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let root = fresh_workspace_root();
    let open_task = cx.update(|cx| {
        init_zmux(cx);
        open_zmux_workspace_at(None, root.path().to_path_buf(), cx)
    });
    let opened = open_task.await.expect("workspace should open");
    let panel = opened.workspace.read_with(cx, |workspace, cx| {
        workspace
            .panel::<WorkspacesPanel>(cx)
            .expect("workspaces panel is installed")
    });
    let origin = panel.read_with(cx, |panel, _| panel.active_workspace_id());

    opened
        .window
        .update(cx, |_, window, cx| {
            panel.update(cx, |panel, cx| panel.create_workspace(window, cx));
        })
        .expect("window remains open");
    for _ in 0..50 {
        cx.run_until_parked();
        if panel.read_with(cx, |panel, _| panel.active_workspace_id()) != origin {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }
    assert_ne!(
        panel.read_with(cx, |panel, _| panel.active_workspace_id()),
        origin,
        "the new workspace should be active before the click",
    );

    let mut cx = VisualTestContext::from_window(opened.window.into(), cx);
    let selector = Box::leak(format!("WORKSPACE_METADATA-{origin}").into_boxed_str());
    let mut metadata_bounds = None;
    for _ in 0..50 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        metadata_bounds = cx.debug_bounds(selector);
        if metadata_bounds.is_some() {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }
    let metadata_bounds = metadata_bounds.expect("inactive workspace metadata should be rendered");
    cx.simulate_click(metadata_bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();

    assert_eq!(
        panel.read_with(&cx, |panel, _| panel.active_workspace_id()),
        origin,
        "clicking shell, directory, git, or process metadata should activate its workspace",
    );
}

#[gpui::test]
async fn terminal_builder_runs_deterministic_command(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    cx.update(|cx| {
        settings::init(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::EditorSettings::register(cx);
        terminal::terminal_settings::TerminalSettings::register(cx);
    });

    let (completion_tx, completion_rx) = async_channel::unbounded();
    let builder_task = cx.update(|cx| {
        TerminalBuilder::new(
            std::env::current_dir().ok(),
            None,
            deterministic_output_shell("zmux-pty-smoke"),
            terminal_env(),
            CursorShape::Block,
            AlternateScroll::On,
            Some(100),
            Vec::new(),
            0,
            false,
            1,
            Some(completion_tx),
            cx,
            Vec::new(),
            PathStyle::local(),
        )
    });
    let builder = builder_task
        .await
        .expect("terminal builder should create a local PTY");

    let terminal = cx.new(|cx| builder.subscribe(cx));
    let completion = completion_rx
        .recv()
        .await
        .expect("terminal command should report completion");
    assert!(completion.is_some());

    for _ in 0..50 {
        cx.run_until_parked();
        let output = terminal.read_with(cx, |terminal, _| terminal.get_content());
        if output.contains("zmux-pty-smoke") {
            return;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }

    let output = terminal.read_with(cx, |terminal, _| terminal.get_content());
    panic!("expected PTY output, got {output:?}");
}

#[gpui::test]
async fn zmux_terminal_font_settings_are_applied(cx: &mut TestAppContext) {
    cx.update(|cx| {
        settings::init(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::EditorSettings::register(cx);
        terminal::terminal_settings::TerminalSettings::register(cx);
        configure_terminal_fonts(cx);

        let settings = TerminalSettings::get_global(cx);
        /*
        assert_eq!(
            settings
                .font_family
                .as_ref()
                .map(|family| family.0.as_ref()),
            Some("JetBrains Mono")
        );
        */
        assert_eq!(settings.font_size, Some(px(14.0)));
    });
}

#[gpui::test]
async fn zoom_actions_adjust_terminal_effective_font_size(cx: &mut TestAppContext) {
    cx.update(|cx| {
        settings::init(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::EditorSettings::register(cx);
        terminal::terminal_settings::TerminalSettings::register(cx);
        configure_terminal_fonts(cx);
        configure_zoom_actions(cx);

        let terminal_font_size = TerminalSettings::get_global(cx)
            .font_size
            .expect("zmux sets a terminal font size");
        assert_eq!(
            theme_settings::adjusted_font_size(terminal_font_size, cx),
            px(14.0)
        );

        cx.dispatch_action(&zed_actions::IncreaseBufferFontSize { persist: false });
        assert_eq!(
            theme_settings::adjusted_font_size(terminal_font_size, cx),
            px(15.0)
        );

        cx.dispatch_action(&zed_actions::DecreaseBufferFontSize { persist: false });
        assert_eq!(
            theme_settings::adjusted_font_size(terminal_font_size, cx),
            px(14.0)
        );

        cx.dispatch_action(&zed_actions::ResetBufferFontSize { persist: false });
        assert_eq!(
            theme_settings::adjusted_font_size(terminal_font_size, cx),
            px(14.0)
        );
    });
}

#[gpui::test]
async fn workspace_shell_opens_first_terminal_as_center_tab(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let root = fresh_workspace_root();
    let open_task = cx.update(|cx| {
        init_zmux(cx);
        open_zmux_workspace_at(None, root.path().to_path_buf(), cx)
    });

    let opened = open_task
        .await
        .expect("workspace shell should open without panicking");

    for _ in 0..50 {
        cx.run_until_parked();
        let center_terminal_count = opened.workspace.read_with(cx, center_terminal_count);
        if center_terminal_count > 0 {
            let (center_item_count, welcome_event, bottom_terminal_count) =
                opened.workspace.update(cx, |workspace, cx| {
                    let (center_item_count, welcome_event) = {
                        let center_pane = workspace.active_pane();
                        let center_pane = center_pane.read(cx);
                        (
                            center_pane.items_len(),
                            center_pane
                                .items()
                                .find_map(|item| item.telemetry_event_text(cx)),
                        )
                    };
                    (
                        center_item_count,
                        welcome_event,
                        bottom_terminal_count(workspace, cx),
                    )
                });

            assert_eq!(center_terminal_count, 1);
            assert_eq!(center_item_count, 2);
            assert_eq!(welcome_event, Some("Zmux Welcome Page Opened"));
            assert_eq!(bottom_terminal_count, 0);
            assert!(
                opened.workspace.read_with(cx, |workspace, cx| workspace
                    .panel::<TerminalPanel>(cx)
                    .is_none()),
                "terminal panel should not be installed for startup terminal creation"
            );

            return;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }

    let center_terminal_count = opened.workspace.read_with(cx, center_terminal_count);
    let bottom_terminal_count = opened.workspace.read_with(cx, bottom_terminal_count);
    assert_eq!(bottom_terminal_count, 0);
    assert!(
        center_terminal_count > 0,
        "expected at least one center terminal item"
    );
}

#[gpui::test]
async fn every_custom_or_generic_new_terminal_is_centered_and_credentialed(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let root = fresh_workspace_root();
    let open_task = cx.update(|cx| {
        init_zmux(cx);
        open_zmux_workspace_at(None, root.path().to_path_buf(), cx)
    });

    let opened = open_task
        .await
        .expect("workspace shell should open without panicking");

    for _ in 0..50 {
        cx.run_until_parked();
        if opened
            .workspace
            .read_with(cx, center_terminal_notification_endpoints)
            .iter()
            .any(Option::is_some)
        {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }

    assert_eq!(opened.workspace.read_with(cx, center_terminal_count), 1);
    assert_eq!(opened.workspace.read_with(cx, bottom_terminal_count), 0);

    opened
        .window
        .update(cx, |_, window, cx| {
            window.dispatch_action(ZmuxNewTerminal.boxed_clone(), cx);
        })
        .expect("window should still be open");

    for _ in 0..50 {
        cx.run_until_parked();
        if opened.workspace.read_with(cx, center_terminal_count) == 2 {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }

    assert_eq!(opened.workspace.read_with(cx, center_terminal_count), 2);
    assert_eq!(opened.workspace.read_with(cx, bottom_terminal_count), 0);

    let mut existing_ids = opened
        .workspace
        .read_with(cx, center_terminal_notification_routes)
        .into_iter()
        .map(|(item_id, _)| item_id)
        .collect::<std::collections::HashSet<_>>();
    let mut generic_routes = Vec::new();

    // These are the upstream actions used by the command palette and terminal
    // context menu. They must not fall through to Zed's uncredentialed shell
    // creation paths.
    for action in [
        workspace::NewTerminal::default().boxed_clone(),
        workspace::NewCenterTerminal::default().boxed_clone(),
    ] {
        opened
            .window
            .update(cx, |_, window, cx| window.dispatch_action(action, cx))
            .expect("window should still be open");

        let expected_count = existing_ids.len() + 1;
        for _ in 0..50 {
            cx.run_until_parked();
            if opened.workspace.read_with(cx, center_terminal_count) == expected_count {
                break;
            }
            cx.background_executor
                .timer(Duration::from_millis(20))
                .await;
        }
        let created = opened
            .workspace
            .read_with(cx, center_terminal_notification_routes)
            .into_iter()
            .find(|(item_id, _)| !existing_ids.contains(item_id))
            .expect("generic action creates one center terminal");
        assert!(
            created.1.is_some(),
            "generic terminal creation must provision a route capability"
        );
        existing_ids.insert(created.0);
        generic_routes.push(created);
    }

    assert_eq!(opened.workspace.read_with(cx, center_terminal_count), 4);
    assert_eq!(opened.workspace.read_with(cx, bottom_terminal_count), 0);
    let all_endpoints = opened
        .workspace
        .read_with(cx, center_terminal_notification_endpoints);
    assert!(all_endpoints.iter().all(Option::is_some));
    assert_eq!(
        all_endpoints
            .iter()
            .filter_map(Option::as_deref)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        4,
        "every terminal creation path needs a distinct capability"
    );

    let mut expected_notifications = Vec::new();
    let mut clients = Vec::new();
    for (index, (item_id, endpoint)) in generic_routes.into_iter().enumerate() {
        let endpoint: CliEndpoint = endpoint
            .expect("generic terminal is credentialed")
            .parse()
            .expect("route endpoint parses");
        let title = format!("Generic terminal route {index}");
        expected_notifications.push((title.clone(), item_id));
        clients.push(std::thread::spawn(move || {
            CliServer::send_to(
                &endpoint,
                CliNotification::new(title, None, "generic route integration"),
            )
        }));
    }
    for _ in 0..100 {
        cx.run_until_parked();
        let recorded = cx.read(|cx| {
            NotificationStore::global(cx)
                .read(cx)
                .notifications()
                .filter(|notification| notification.title.starts_with("Generic terminal route "))
                .count()
        });
        if recorded == expected_notifications.len() {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }
    for client in clients {
        client
            .join()
            .expect("CLI client thread should not panic")
            .expect("credentialed generic route should acknowledge after recording");
    }
    cx.read(|cx| {
        let store = NotificationStore::global(cx).read(cx);
        for (title, item_id) in expected_notifications {
            let notification = store
                .notifications()
                .find(|notification| notification.title == title)
                .expect("generic route notification should be recorded");
            assert_eq!(notification.target.item_id, item_id);
        }
    });
}

#[gpui::test]
async fn missing_workspace_pane_action_creates_a_credentialed_split(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let root = fresh_workspace_root();
    let open_task = cx.update(|cx| {
        init_zmux(cx);
        open_zmux_workspace_at(None, root.path().to_path_buf(), cx)
    });
    let opened = open_task.await.expect("workspace should open");

    for _ in 0..50 {
        cx.run_until_parked();
        let endpoints = opened
            .workspace
            .read_with(cx, center_terminal_notification_endpoints);
        if endpoints.len() == 1 && endpoints.iter().all(Option::is_some) {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }

    // Dispatch the workspace action itself. On Linux, alt-2 correctly resolves
    // to the higher-priority pane::ActivateItem binding while a terminal pane
    // is focused; overriding that normal tab shortcut would be incorrect.
    opened
        .window
        .update(cx, |_, window, cx| {
            window.dispatch_action(workspace::ActivatePane(1).boxed_clone(), cx);
        })
        .expect("window remains open");
    for _ in 0..50 {
        cx.run_until_parked();
        if opened.workspace.read_with(cx, center_terminal_count) == 2 {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }

    let routes = opened
        .workspace
        .read_with(cx, center_terminal_notification_routes);
    assert_eq!(routes.len(), 2);
    let endpoints = routes
        .iter()
        .map(|(_, endpoint)| endpoint.as_deref().expect("split is credentialed"))
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(endpoints.len(), 2, "pane action needs a fresh capability");

    let (item_id, endpoint) = routes.into_iter().nth(1).expect("new split route exists");
    let endpoint: CliEndpoint = endpoint
        .expect("new split has an endpoint")
        .parse()
        .expect("route endpoint parses");
    let client = std::thread::spawn(move || {
        CliServer::send_to(
            &endpoint,
            CliNotification::new("Pane-number route", None, "shortcut integration"),
        )
    });
    for _ in 0..100 {
        cx.run_until_parked();
        if cx.read(|cx| {
            NotificationStore::global(cx)
                .read(cx)
                .notifications()
                .any(|notification| notification.title == "Pane-number route")
        }) {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }
    client
        .join()
        .expect("CLI client should not panic")
        .expect("shortcut-created terminal route acknowledges");
    cx.read(|cx| {
        let notification = NotificationStore::global(cx)
            .read(cx)
            .notifications()
            .find(|notification| notification.title == "Pane-number route")
            .cloned()
            .expect("shortcut route records notification");
        assert_eq!(notification.target.item_id, item_id);
    });
}

#[gpui::test]
async fn direct_terminal_clone_is_credentialed_before_it_is_mounted(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let root = fresh_workspace_root();
    let open_task = cx.update(|cx| {
        init_zmux(cx);
        open_zmux_workspace_at(None, root.path().to_path_buf(), cx)
    });
    let opened = open_task.await.expect("workspace should open");

    let initial_item_id = loop {
        cx.run_until_parked();
        let routes = opened
            .workspace
            .read_with(cx, center_terminal_notification_routes);
        if let [(item_id, Some(_))] = routes.as_slice() {
            break *item_id;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    };

    // Pane's modifier-drop path calls this trait method directly rather than
    // dispatching a split action. Add the resulting plain clone to a fresh
    // split exactly as that upstream drop handler does.
    let clone_task = opened
        .window
        .update(cx, |_, window, cx| {
            opened.workspace.update(cx, |workspace, cx| {
                workspace
                    .active_pane()
                    .read(cx)
                    .active_item()
                    .expect("startup terminal is active")
                    .clone_on_split(workspace.database_id(), window, cx)
            })
        })
        .expect("window remains open");
    let cloned_item = clone_task
        .await
        .expect("direct terminal clone should create a shell");
    let cloned_view = cx
        .read(|cx| cloned_item.act_as::<TerminalView>(cx))
        .expect("cloned item remains a terminal view");
    assert!(
        cloned_view.read_with(cx, |view, cx| view
            .terminal()
            .read(cx)
            .task()
            .and_then(|task| task.spawned_task.env.get(NOTIFY_ENDPOINT_ENV))
            .is_some()),
        "the clone factory must provision the child before pane mounting"
    );
    opened
        .window
        .update(cx, |_, window, cx| {
            opened.workspace.update(cx, |workspace, cx| {
                let destination = workspace.split_pane(
                    workspace.active_pane().clone(),
                    workspace::SplitDirection::Right,
                    window,
                    cx,
                );
                workspace.add_item(destination, cloned_item, None, true, true, window, cx);
            });
        })
        .expect("window remains open");

    let routes = loop {
        cx.run_until_parked();
        let routes = opened
            .workspace
            .read_with(cx, center_terminal_notification_routes);
        if routes.len() == 2 && routes.iter().all(|(_, endpoint)| endpoint.is_some()) {
            break routes;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    };
    assert_eq!(
        cx.read(|cx| cx.global::<CliServer>().registered_route_count()),
        2,
        "one live lease must exist for each mounted terminal"
    );
    let endpoint_set = routes
        .iter()
        .filter_map(|(_, endpoint)| endpoint.as_deref())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(endpoint_set.len(), 2);
    let (cloned_item_id, endpoint) = routes
        .into_iter()
        .find(|(item_id, _)| *item_id != initial_item_id)
        .expect("clone destination contains the provisioned clone");
    let endpoint: CliEndpoint = endpoint
        .expect("replacement has a capability")
        .parse()
        .expect("replacement endpoint parses");

    let client = std::thread::spawn(move || {
        CliServer::send_to(
            &endpoint,
            CliNotification::new("Direct clone route", None, "clone integration"),
        )
    });
    for _ in 0..100 {
        cx.run_until_parked();
        if cx.read(|cx| {
            NotificationStore::global(cx)
                .read(cx)
                .notifications()
                .any(|notification| notification.title == "Direct clone route")
        }) {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }
    client
        .join()
        .expect("CLI client should not panic")
        .expect("direct clone route acknowledges");
    cx.read(|cx| {
        let notification = NotificationStore::global(cx)
            .read(cx)
            .notifications()
            .find(|notification| notification.title == "Direct clone route")
            .expect("direct clone route records a notification");
        assert_eq!(notification.target.item_id, cloned_item_id);
    });
}

#[gpui::test]
async fn deserialized_terminal_is_credentialed_before_mount_and_routes_exactly(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let root = fresh_workspace_root();
    let open_task = cx.update(|cx| {
        init_zmux(cx);
        open_zmux_workspace_at(None, root.path().to_path_buf(), cx)
    });
    let opened = open_task.await.expect("workspace should open");

    for _ in 0..50 {
        cx.run_until_parked();
        if opened
            .workspace
            .read_with(cx, center_terminal_notification_endpoints)
            .as_slice()
            .iter()
            .all(|endpoint| endpoint.is_some())
            && opened.workspace.read_with(cx, center_terminal_count) == 1
        {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }

    let (project, workspace_database_id) = opened.workspace.read_with(cx, |workspace, _| {
        (
            workspace.project().clone(),
            workspace
                .database_id()
                .expect("the open workspace has a persistence ID"),
        )
    });
    let deserialize = opened
        .window
        .update(cx, |_, window, cx| {
            <TerminalView as workspace::item::SerializableItem>::deserialize(
                project,
                opened.workspace.downgrade(),
                workspace_database_id,
                u64::MAX - 17,
                window,
                cx,
            )
        })
        .expect("window remains open");
    let restored_view = deserialize
        .await
        .expect("the persisted terminal factory should create a shell");
    let restored_item_id = restored_view.entity_id();
    let endpoint = restored_view.read_with(cx, |view, cx| {
        view.terminal()
            .read(cx)
            .task()
            .and_then(|task| task.spawned_task.env.get(NOTIFY_ENDPOINT_ENV))
            .cloned()
    });
    let endpoint = endpoint.expect("deserialization provisions an endpoint before pane mounting");
    assert!(!endpoint.is_empty());
    assert_eq!(
        cx.read(|cx| cx.global::<CliServer>().registered_route_count()),
        2,
        "the unmounted restored terminal owns one pending route lease"
    );

    opened
        .window
        .update(cx, |_, window, cx| {
            opened.workspace.update(cx, |workspace, cx| {
                let destination = workspace.split_pane(
                    workspace.active_pane().clone(),
                    workspace::SplitDirection::Right,
                    window,
                    cx,
                );
                workspace.add_item(
                    destination,
                    Box::new(restored_view),
                    None,
                    true,
                    true,
                    window,
                    cx,
                );
            });
        })
        .expect("window remains open");

    for _ in 0..50 {
        cx.run_until_parked();
        let activated = opened
            .workspace
            .read_with(cx, center_terminal_notification_routes)
            .into_iter()
            .any(|(item_id, endpoint)| item_id == restored_item_id && endpoint.is_some());
        if activated {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }
    assert_eq!(
        cx.read(|cx| cx.global::<CliServer>().registered_route_count()),
        2,
        "mounting activates the existing lease without minting another"
    );

    let endpoint: CliEndpoint = endpoint.parse().expect("restored endpoint parses");
    let client = std::thread::spawn(move || {
        CliServer::send_to(
            &endpoint,
            CliNotification::new("Persisted terminal route", None, "deserialize integration"),
        )
    });
    for _ in 0..100 {
        cx.run_until_parked();
        if cx.read(|cx| {
            NotificationStore::global(cx)
                .read(cx)
                .notifications()
                .any(|notification| notification.title == "Persisted terminal route")
        }) {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }
    client
        .join()
        .expect("CLI client should not panic")
        .expect("the deserialized terminal route acknowledges");
    cx.read(|cx| {
        let notification = NotificationStore::global(cx)
            .read(cx)
            .notifications()
            .find(|notification| notification.title == "Persisted terminal route")
            .expect("the deserialized route records a notification");
        assert_eq!(notification.target.item_id, restored_item_id);
    });
}

#[gpui::test]
async fn cli_route_is_revoked_when_shell_exits_while_its_tab_stays_open(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let root = fresh_workspace_root();
    let open_task = cx.update(|cx| {
        init_zmux(cx);
        open_zmux_workspace_at(None, root.path().to_path_buf(), cx)
    });
    let opened = open_task.await.expect("workspace should open");

    let (terminal, endpoint) = loop {
        cx.run_until_parked();
        let route = opened.workspace.read_with(cx, |workspace, cx| {
            workspace.panes().iter().find_map(|pane| {
                pane.read(cx).items().find_map(|item| {
                    let view = item.act_as::<TerminalView>(cx)?;
                    let terminal = view.read(cx).terminal().clone();
                    let endpoint = terminal
                        .read(cx)
                        .task()
                        .and_then(|task| task.spawned_task.env.get(NOTIFY_ENDPOINT_ENV))
                        .cloned()?;
                    Some((terminal, endpoint))
                })
            })
        });
        if let Some(route) = route {
            break route;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    };
    let endpoint: CliEndpoint = endpoint.parse().expect("route endpoint parses");
    assert_eq!(
        cx.read(|cx| cx.global::<CliServer>().registered_route_count()),
        1
    );

    terminal.update(cx, |terminal, _| terminal.input(b"exit\r"));
    for _ in 0..150 {
        cx.run_until_parked();
        let stopped = terminal.read_with(cx, |terminal, _| {
            terminal
                .task()
                .is_none_or(|task| task.status != terminal::TaskStatus::Running)
        });
        let no_live_route = cx.read(|cx| cx.global::<CliServer>().registered_route_count()) == 0;
        if stopped && no_live_route {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }

    assert_eq!(opened.workspace.read_with(cx, center_terminal_count), 1);
    assert_ne!(
        terminal.read_with(cx, |terminal, _| terminal
            .task()
            .expect("zmux shell is task-backed")
            .status),
        terminal::TaskStatus::Running
    );
    assert_eq!(
        cx.read(|cx| cx.global::<CliServer>().registered_route_count()),
        0,
        "completion watcher revokes before any stale client request"
    );

    let client = std::thread::spawn(move || {
        CliServer::send_to(
            &endpoint,
            CliNotification::new("Must be rejected", None, "dead shell"),
        )
    });
    for _ in 0..50 {
        cx.run_until_parked();
        if client.is_finished() {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }
    assert!(
        client.join().expect("CLI client should not panic").is_err(),
        "a completed shell endpoint must already be unauthorized"
    );
    cx.read(|cx| {
        assert!(
            NotificationStore::global(cx)
                .read(cx)
                .notifications()
                .all(|notification| notification.title != "Must be rejected")
        );
    });
}

#[gpui::test]
async fn terminal_splits_never_share_notification_capabilities(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let root = fresh_workspace_root();
    let open_task = cx.update(|cx| {
        init_zmux(cx);
        open_zmux_workspace_at(None, root.path().to_path_buf(), cx)
    });
    let opened = open_task.await.expect("workspace should open");

    for _ in 0..50 {
        cx.run_until_parked();
        if opened
            .workspace
            .read_with(cx, center_terminal_notification_endpoints)
            .len()
            == 1
        {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }

    opened
        .window
        .update(cx, |_, window, cx| {
            window.dispatch_action(SplitTerminalRight.boxed_clone(), cx);
        })
        .expect("window remains open");

    for _ in 0..50 {
        cx.run_until_parked();
        if opened
            .workspace
            .read_with(cx, center_terminal_notification_endpoints)
            .len()
            == 2
        {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }

    let custom_split_endpoints = opened
        .workspace
        .read_with(cx, center_terminal_notification_endpoints);
    assert_eq!(custom_split_endpoints.len(), 2);
    let first = custom_split_endpoints[0]
        .as_deref()
        .expect("startup terminal gets a route capability");
    let second = custom_split_endpoints[1]
        .as_deref()
        .expect("custom split gets a route capability");
    assert_ne!(first, second, "each terminal must get a fresh capability");

    // Built-in pane-menu/user-keymap split actions are captured before Zed's
    // generic clone path, then sent through the same provisioned spawn path.
    opened
        .window
        .update(cx, |_, window, cx| {
            window.dispatch_action(workspace::pane::SplitRight::default().boxed_clone(), cx);
        })
        .expect("window remains open");

    for _ in 0..50 {
        cx.run_until_parked();
        if opened
            .workspace
            .read_with(cx, center_terminal_notification_endpoints)
            .len()
            == 3
        {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }

    let all_endpoints = opened
        .workspace
        .read_with(cx, center_terminal_notification_endpoints);
    assert_eq!(all_endpoints.len(), 3);
    let provisioned: Vec<_> = all_endpoints.iter().filter_map(Option::as_deref).collect();
    assert_eq!(provisioned.len(), 3);
    let unique_endpoints = provisioned
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        unique_endpoints.len(),
        3,
        "every custom or generic clone split needs its own capability"
    );

    // Exercise the complete TCP -> authenticated route -> GPUI store -> ACK
    // path for all three terminals, not only their task metadata.
    let routes = opened
        .workspace
        .read_with(cx, center_terminal_notification_routes);
    let mut expected = Vec::new();
    let mut clients = Vec::new();
    for (index, (item_id, endpoint)) in routes.into_iter().enumerate() {
        let endpoint: CliEndpoint = endpoint
            .expect("every terminal split is credentialed")
            .parse()
            .expect("route endpoint parses");
        let title = format!("CLI exact route {index}");
        expected.push((title.clone(), item_id));
        clients.push(std::thread::spawn(move || {
            CliServer::send_to(
                &endpoint,
                CliNotification::new(title, None, "route integration"),
            )
        }));
    }
    for _ in 0..100 {
        cx.run_until_parked();
        let recorded = cx.read(|cx| {
            NotificationStore::global(cx)
                .read(cx)
                .notifications()
                .filter(|notification| notification.source == NotificationSource::Cli)
                .count()
        });
        if recorded == expected.len() {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }
    for client in clients {
        client
            .join()
            .expect("CLI client thread should not panic")
            .expect("CLI success is acknowledged after GPUI records the row");
    }
    cx.read(|cx| {
        let store = NotificationStore::global(cx).read(cx);
        for (title, item_id) in expected {
            let notification = store
                .notifications()
                .find(|notification| notification.title == title)
                .expect("CLI notification should exist");
            assert_eq!(notification.target.item_id, item_id);
        }
    });
}

#[gpui::test]
async fn delayed_split_completion_never_leaks_into_a_different_logical_workspace(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let root = fresh_workspace_root();
    let open_task = cx.update(|cx| {
        init_zmux(cx);
        open_zmux_workspace_at(None, root.path().to_path_buf(), cx)
    });
    let opened = open_task.await.expect("workspace should open");

    for _ in 0..50 {
        cx.run_until_parked();
        if opened
            .workspace
            .read_with(cx, center_terminal_notification_endpoints)
            .iter()
            .any(Option::is_some)
        {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }

    let panel = opened.workspace.read_with(cx, |workspace, cx| {
        workspace
            .panel::<WorkspacesPanel>(cx)
            .expect("workspaces panel is installed")
    });
    let origin = panel.read_with(cx, |panel, _| panel.active_workspace_id());

    // Start an asynchronous shell creation, then park its destination pane in
    // the same foreground turn. This deterministically puts the workspace swap
    // ahead of split completion without relying on a timing sleep.
    opened
        .window
        .update(cx, |_, window, cx| {
            window.dispatch_action(SplitTerminalRight.boxed_clone(), cx);
            let window_handle = window.window_handle();
            let panel = panel.clone();
            // `Window::dispatch_action` is itself deferred. Queue the workspace
            // switch after it so the split captures workspace 1, begins its
            // asynchronous spawn, and only then loses its mounted destination.
            cx.defer(move |cx| {
                let _ = window_handle.update(cx, |_, window, cx| {
                    panel.update(cx, |panel, cx| panel.create_workspace(window, cx));
                });
            });
        })
        .expect("window remains open");

    for _ in 0..50 {
        cx.run_until_parked();
        if panel.read_with(cx, |panel, _| panel.active_workspace_id()) != origin
            && opened.workspace.read_with(cx, center_terminal_count) == 1
        {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }
    let other = panel.read_with(cx, |panel, _| panel.active_workspace_id());
    assert_ne!(other, origin);
    assert_eq!(
        opened.workspace.read_with(cx, center_terminal_count),
        1,
        "a stale completion must not split the newly active workspace"
    );

    opened
        .window
        .update(cx, |_, window, cx| {
            panel.update(cx, |panel, cx| {
                panel.activate_workspace(origin, window, cx);
            });
        })
        .expect("window remains open");
    for _ in 0..50 {
        cx.run_until_parked();
        if panel.read_with(cx, |panel, _| panel.active_workspace_id()) == origin {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }
    assert_eq!(
        opened.workspace.read_with(cx, center_terminal_count),
        1,
        "the canceled split must not mutate the parked originating layout"
    );
}

#[gpui::test]
async fn delayed_center_completion_never_leaks_or_retains_its_staged_route(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let root = fresh_workspace_root();
    let open_task = cx.update(|cx| {
        init_zmux(cx);
        open_zmux_workspace_at(None, root.path().to_path_buf(), cx)
    });
    let opened = open_task.await.expect("workspace should open");

    for _ in 0..50 {
        cx.run_until_parked();
        if opened
            .workspace
            .read_with(cx, center_terminal_notification_endpoints)
            .iter()
            .any(Option::is_some)
        {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }

    let panel = opened.workspace.read_with(cx, |workspace, cx| {
        workspace
            .panel::<WorkspacesPanel>(cx)
            .expect("workspaces panel is installed")
    });
    let origin = panel.read_with(cx, |panel, _| panel.active_workspace_id());
    let baseline_routes = cx.read(|cx| cx.global::<CliServer>().registered_route_count());
    assert_eq!(opened.workspace.read_with(cx, center_terminal_count), 1);
    assert_eq!(baseline_routes, 1);

    // Dispatching is deferred by GPUI. Queue the logical-workspace creation
    // immediately afterward so the center spawn captures workspace 1 and its
    // pane, then loses that mounted destination before its shell completes.
    opened
        .window
        .update(cx, |_, window, cx| {
            window.dispatch_action(ZmuxNewTerminal.boxed_clone(), cx);
            let window_handle = window.window_handle();
            let panel = panel.clone();
            cx.defer(move |cx| {
                let _ = window_handle.update(cx, |_, window, cx| {
                    panel.update(cx, |panel, cx| panel.create_workspace(window, cx));
                });
            });
        })
        .expect("window remains open");

    let expected_live_routes = baseline_routes + 1;
    for _ in 0..100 {
        cx.run_until_parked();
        let switched = panel.read_with(cx, |panel, _| panel.active_workspace_id()) != origin;
        let live_routes = cx.read(|cx| cx.global::<CliServer>().registered_route_count());
        if switched
            && opened.workspace.read_with(cx, center_terminal_count) == 1
            && live_routes == expected_live_routes
        {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }

    // Keep pumping after the replacement workspace is ready. This forces the
    // stale shell through terminal creation, route staging, guarded rejection,
    // entity release, and registration revocation before the final assertions.
    for _ in 0..20 {
        cx.run_until_parked();
        cx.background_executor
            .timer(Duration::from_millis(10))
            .await;
    }

    let other = panel.read_with(cx, |panel, _| panel.active_workspace_id());
    assert_ne!(other, origin);
    assert_eq!(
        opened.workspace.read_with(cx, center_terminal_count),
        1,
        "the stale center completion must not add a terminal to workspace 2"
    );
    assert_eq!(
        cx.read(|cx| cx.global::<CliServer>().registered_route_count()),
        expected_live_routes,
        "the canceled terminal's staged route lease must be revoked"
    );

    opened
        .window
        .update(cx, |_, window, cx| {
            panel.update(cx, |panel, cx| {
                panel.activate_workspace(origin, window, cx);
            });
        })
        .expect("window remains open");
    for _ in 0..50 {
        cx.run_until_parked();
        if panel.read_with(cx, |panel, _| panel.active_workspace_id()) == origin {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }
    assert_eq!(
        opened.workspace.read_with(cx, center_terminal_count),
        1,
        "the stale center completion must not mutate its parked origin either"
    );
    assert_eq!(
        cx.read(|cx| cx.global::<CliServer>().registered_route_count()),
        expected_live_routes,
        "only the two terminals retained by the two layouts may own routes"
    );
}

#[gpui::test]
async fn fast_round_trip_to_welcome_only_workspace_provisions_exactly_one_terminal(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let root = fresh_workspace_root();
    let open_task = cx.update(|cx| {
        init_zmux(cx);
        open_zmux_workspace_at(None, root.path().to_path_buf(), cx)
    });
    let opened = open_task.await.expect("workspace should open");

    for _ in 0..50 {
        cx.run_until_parked();
        let endpoints = opened
            .workspace
            .read_with(cx, center_terminal_notification_endpoints);
        if endpoints.len() == 1 && endpoints[0].is_some() {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }

    let panel = opened.workspace.read_with(cx, |workspace, cx| {
        workspace
            .panel::<WorkspacesPanel>(cx)
            .expect("workspaces panel is installed")
    });
    let origin = panel.read_with(cx, |panel, _| panel.active_workspace_id());
    let baseline_routes = cx.read(|cx| cx.global::<CliServer>().registered_route_count());
    assert_eq!(baseline_routes, 1);

    // Run B -> A -> B in one foreground callback. GPUI cannot poll B's first
    // shell task until this callback returns, so the original spawn is held
    // across both switches while restoration starts its retry. Workspace ID
    // and root pane alone are identical again; the activation generation is
    // what must reject that first spawn.
    let parked = opened
        .window
        .update(cx, |_, window, cx| {
            panel.update(cx, |panel, cx| {
                panel.create_workspace(window, cx);
                let parked = panel.active_workspace_id();
                panel.activate_workspace(origin, window, cx);
                panel.activate_workspace(parked, window, cx);
                parked
            })
        })
        .expect("window remains open");
    assert_ne!(parked, origin);
    assert_eq!(
        panel.read_with(cx, |panel, _| panel.active_workspace_id()),
        parked
    );
    for _ in 0..100 {
        cx.run_until_parked();
        let routes = opened
            .workspace
            .read_with(cx, center_terminal_notification_routes);
        if panel.read_with(cx, |panel, _| panel.active_workspace_id()) == parked
            && routes.len() == 1
            && routes[0].1.is_some()
        {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }

    // Pump once more after success so a duplicate late completion would also
    // become visible before the exact-count assertions.
    for _ in 0..10 {
        cx.run_until_parked();
        cx.background_executor
            .timer(Duration::from_millis(10))
            .await;
    }
    let routes = opened
        .workspace
        .read_with(cx, center_terminal_notification_routes);
    assert_eq!(
        routes.len(),
        1,
        "restoration should provision exactly one shell"
    );
    assert!(
        routes[0].1.is_some(),
        "the retry shell must be credentialed"
    );
    assert_eq!(
        cx.read(|cx| cx.global::<CliServer>().registered_route_count()),
        baseline_routes + 1,
        "the invalidated first spawn must not leak a second route lease"
    );
}

#[gpui::test]
async fn no_cli_server_fallback_survives_workspace_round_trips_without_duplicate_shells(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let root = fresh_workspace_root();
    let open_task = cx.update(|cx| {
        init_zmux(cx);
        // Exercise the legitimate fallback used when listener/thread startup
        // fails. The project-wide terminal environment still scrubs any outer
        // capability, but these ordinary shells intentionally have no task
        // metadata or per-terminal endpoint.
        drop(cx.remove_global::<CliServer>());
        open_zmux_workspace_at(None, root.path().to_path_buf(), cx)
    });
    let opened = open_task.await.expect("workspace should open");

    for _ in 0..50 {
        cx.run_until_parked();
        if opened.workspace.read_with(cx, center_terminal_count) == 1 {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }
    assert_eq!(opened.workspace.read_with(cx, center_terminal_count), 1);
    assert_eq!(
        opened
            .workspace
            .read_with(cx, center_terminal_notification_endpoints),
        [None],
        "the fallback shell must not inherit or invent a route capability"
    );

    let panel = opened.workspace.read_with(cx, |workspace, cx| {
        workspace
            .panel::<WorkspacesPanel>(cx)
            .expect("workspaces panel is installed")
    });
    let origin = panel.read_with(cx, |panel, _| panel.active_workspace_id());
    let other = opened
        .window
        .update(cx, |_, window, cx| {
            panel.update(cx, |panel, cx| {
                panel.create_workspace(window, cx);
                panel.active_workspace_id()
            })
        })
        .expect("window remains open");
    assert_ne!(other, origin);

    for _ in 0..50 {
        cx.run_until_parked();
        if opened.workspace.read_with(cx, center_terminal_count) == 1 {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }

    for target in [origin, other, origin, other] {
        opened
            .window
            .update(cx, |_, window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.activate_workspace(target, window, cx);
                });
            })
            .expect("window remains open");
        for _ in 0..50 {
            cx.run_until_parked();
            if panel.read_with(cx, |panel, _| panel.active_workspace_id()) == target
                && opened.workspace.read_with(cx, center_terminal_count) == 1
            {
                break;
            }
            cx.background_executor
                .timer(Duration::from_millis(20))
                .await;
        }
        assert_eq!(
            opened.workspace.read_with(cx, center_terminal_count),
            1,
            "restoring workspace {target} must reuse its fallback terminal"
        );
    }

    for _ in 0..10 {
        cx.run_until_parked();
        cx.background_executor
            .timer(Duration::from_millis(10))
            .await;
    }
    assert_eq!(
        opened.workspace.read_with(cx, center_terminal_count),
        1,
        "a late provisioning task must not add a duplicate fallback terminal"
    );
}

#[gpui::test]
async fn notification_action_records_and_navigates_the_exact_terminal(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let root = fresh_workspace_root();
    let open_task = cx.update(|cx| {
        init_zmux(cx);
        open_zmux_workspace_at(None, root.path().to_path_buf(), cx)
    });
    let opened = open_task.await.expect("workspace should open");

    for _ in 0..50 {
        cx.run_until_parked();
        if opened
            .workspace
            .read_with(cx, center_terminal_notification_endpoints)
            .iter()
            .any(Option::is_some)
        {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }

    let panel = opened.workspace.read_with(cx, |workspace, cx| {
        workspace
            .panel::<WorkspacesPanel>(cx)
            .expect("workspaces panel is installed")
    });
    let target_workspace_id = panel.read_with(cx, |panel, _| panel.active_workspace_id());

    // Build two panes in workspace 1. The split action focuses the new pane, so
    // the original terminal becomes a concrete nonfocused target rather than a
    // value which happens to equal the current fallback.
    opened
        .window
        .update(cx, |_, window, cx| {
            window.dispatch_action(SplitTerminalRight.boxed_clone(), cx);
        })
        .expect("window remains open");
    for _ in 0..50 {
        cx.run_until_parked();
        if opened.workspace.read_with(cx, center_terminal_count) == 2 {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }
    assert_eq!(opened.workspace.read_with(cx, center_terminal_count), 2);

    let (focused_item_id, target_item_id, target_endpoint) =
        opened.workspace.read_with(cx, |workspace, cx| {
            let focused_item_id = workspace
                .active_pane()
                .read(cx)
                .active_item()
                .expect("split has an active terminal")
                .item_id();
            let (target_item_id, endpoint) = center_terminal_notification_routes(workspace, cx)
                .into_iter()
                .find(|(item_id, _)| *item_id != focused_item_id)
                .expect("the other pane supplies a nonfocused terminal target");
            (focused_item_id, target_item_id, endpoint)
        });
    assert_ne!(target_item_id, focused_item_id);
    let target_endpoint: CliEndpoint = target_endpoint
        .expect("the target terminal has a route capability")
        .parse()
        .expect("target route endpoint parses");

    // Create and activate workspace 2, parking the two-pane target workspace.
    // The notification below therefore belongs to both a nonfocused pane and an
    // inactive logical workspace.
    opened
        .window
        .update(cx, |_, window, cx| {
            panel.update(cx, |panel, cx| panel.create_workspace(window, cx));
        })
        .expect("window remains open");
    for _ in 0..50 {
        cx.run_until_parked();
        if panel.read_with(cx, |panel, _| panel.active_workspace_id()) != target_workspace_id
            && opened.workspace.read_with(cx, center_terminal_count) == 1
        {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }
    let fallback_workspace_id = panel.read_with(cx, |panel, _| panel.active_workspace_id());
    assert_ne!(fallback_workspace_id, target_workspace_id);
    let fallback_item_id = opened.workspace.read_with(cx, |workspace, cx| {
        workspace
            .active_pane()
            .read(cx)
            .active_item()
            .expect("workspace 2 has an active terminal")
            .item_id()
    });
    assert_ne!(fallback_item_id, target_item_id);
    assert!(
        opened.workspace.read_with(cx, |workspace, _| workspace
            .pane_for_item_id(target_item_id)
            .is_none()),
        "the target workspace must be parked before publishing"
    );

    let title = "Exact parked-pane notification".to_owned();
    let client_title = title.clone();
    let client = std::thread::spawn(move || {
        CliServer::send_to(
            &target_endpoint,
            CliNotification::new(client_title, None, "route integration"),
        )
    });
    for _ in 0..100 {
        cx.run_until_parked();
        let recorded = cx.read(|cx| {
            NotificationStore::global(cx)
                .read(cx)
                .notifications()
                .any(|notification| notification.title == title)
        });
        if recorded {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }
    client
        .join()
        .expect("CLI client thread should not panic")
        .expect("CLI request is acknowledged after recording");

    let notification = cx.read(|cx| {
        NotificationStore::global(cx)
            .read(cx)
            .notifications()
            .find(|notification| notification.title == title)
            .cloned()
            .expect("parked target notification should be recorded")
    });
    assert_eq!(notification.source, NotificationSource::Cli);
    assert_eq!(notification.target.workspace_id, target_workspace_id);
    assert_eq!(notification.target.item_id, target_item_id);
    assert!(!notification.read);
    assert_eq!(
        panel.read_with(cx, |panel, _| panel.active_workspace_id()),
        fallback_workspace_id,
        "publishing must not activate the target implicitly"
    );

    opened
        .window
        .update(cx, |_, window, cx| {
            window.dispatch_action(JumpToLatestNotification.boxed_clone(), cx);
        })
        .expect("window remains open");
    for _ in 0..50 {
        cx.run_until_parked();
        let activated = cx.read(|cx| {
            panel.read(cx).active_workspace_id() == target_workspace_id
                && NotificationStore::global(cx)
                    .read(cx)
                    .get(notification.id)
                    .is_some_and(|notification| notification.read)
                && opened
                    .workspace
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .active_item()
                    .is_some_and(|item| item.item_id() == target_item_id)
        });
        if activated {
            break;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }

    assert!(cx.read(|cx| {
        NotificationStore::global(cx)
            .read(cx)
            .get(notification.id)
            .is_some_and(|notification| notification.read)
    }));
    assert_eq!(
        panel.read_with(cx, |panel, _| panel.active_workspace_id()),
        target_workspace_id
    );
    assert_eq!(opened.workspace.read_with(cx, center_terminal_count), 2);
    assert_eq!(
        opened.workspace.read_with(cx, |workspace, cx| workspace
            .active_pane()
            .read(cx)
            .active_item()
            .map(|item| item.item_id())),
        Some(target_item_id),
        "opening must restore and focus the recorded pane, not workspace 2's fallback"
    );
}

#[gpui::test]
async fn zmux_keybindings_cover_terminal_copy_paste_and_zoom(cx: &mut TestAppContext) {
    cx.update(|cx| {
        configure_keybindings(cx);

        assert_bound(cx, "cmd-c", &terminal::Copy);
        assert_bound(cx, "cmd-v", &terminal::Paste);
        assert_bound(cx, "ctrl-shift-c", &terminal::Copy);
        assert_bound(cx, "ctrl-insert", &terminal::Copy);
        assert_bound(cx, "ctrl-shift-v", &terminal::Paste);
        assert_bound(cx, "shift-insert", &terminal::Paste);
        #[cfg(target_os = "windows")]
        assert_bound(cx, "ctrl-c", &terminal::Copy);
        #[cfg(not(target_os = "windows"))]
        assert_not_bound(cx, "ctrl-c", &terminal::Copy);

        assert_bound(
            cx,
            "ctrl-=",
            &zed_actions::IncreaseBufferFontSize { persist: false },
        );
        assert_bound(
            cx,
            "ctrl-+",
            &zed_actions::IncreaseBufferFontSize { persist: false },
        );
        assert_bound(
            cx,
            "ctrl--",
            &zed_actions::DecreaseBufferFontSize { persist: false },
        );
        assert_bound(
            cx,
            "ctrl-0",
            &zed_actions::ResetBufferFontSize { persist: false },
        );
        assert_bound(
            cx,
            "cmd-=",
            &zed_actions::IncreaseBufferFontSize { persist: false },
        );
        assert_bound(
            cx,
            "cmd-+",
            &zed_actions::IncreaseBufferFontSize { persist: false },
        );
        assert_bound(
            cx,
            "cmd--",
            &zed_actions::DecreaseBufferFontSize { persist: false },
        );
        assert_bound(
            cx,
            "cmd-0",
            &zed_actions::ResetBufferFontSize { persist: false },
        );
        assert_bound(cx, "ctrl-shift-t", &ZmuxNewTerminal);
        assert_bound(cx, "ctrl-shift-n", &ZmuxNewTerminal);
        assert_bound(cx, "ctrl-tab", &tab_switcher::Toggle::default());
        assert_bound(
            cx,
            "ctrl-shift-tab",
            &tab_switcher::Toggle { select_last: true },
        );
        assert_bound(cx, "ctrl-shift-w", &CloseActiveItem::default());
        assert_bound(cx, "ctrl-shift-alt-w", &CloseAllItems::default());
        assert_bound(cx, "ctrl-shift-o", &CloseOtherItems::default());
        assert_bound(cx, "alt-right", &ActivateNextPane);
        assert_bound(cx, "alt-left", &ActivatePreviousPane);
        assert_bound(cx, "ctrl-shift-d", &SplitTerminalRight);
        assert_bound(cx, "ctrl-shift-alt-d", &SplitTerminalDown);
        assert_not_bound(cx, "ctrl-shift-d", &workspace::pane::SplitRight::default());
        assert_not_bound(
            cx,
            "ctrl-shift-alt-d",
            &workspace::pane::SplitDown::default(),
        );
    });
}

fn center_terminal_count(workspace: &workspace::Workspace, cx: &gpui::App) -> usize {
    workspace
        .panes()
        .iter()
        .map(|pane| {
            pane.read(cx)
                .items()
                .filter(|item| item.act_as::<TerminalView>(cx).is_some())
                .count()
        })
        .sum()
}

fn center_terminal_notification_endpoints(
    workspace: &workspace::Workspace,
    cx: &gpui::App,
) -> Vec<Option<String>> {
    center_terminal_notification_routes(workspace, cx)
        .into_iter()
        .map(|(_, endpoint)| endpoint)
        .collect()
}

fn center_terminal_notification_routes(
    workspace: &workspace::Workspace,
    cx: &gpui::App,
) -> Vec<(gpui::EntityId, Option<String>)> {
    let mut routes = Vec::new();
    for pane in workspace.panes() {
        for item in pane.read(cx).items() {
            let Some(view) = item.act_as::<TerminalView>(cx) else {
                continue;
            };
            let terminal = view.read(cx).terminal().clone();
            let endpoint = terminal
                .read(cx)
                .task()
                .and_then(|task| task.spawned_task.env.get(NOTIFY_ENDPOINT_ENV))
                .cloned();
            routes.push((item.item_id(), endpoint));
        }
    }
    routes
}

#[cfg(unix)]
async fn wait_for_terminal_tab_title(
    terminal: &gpui::Entity<terminal::Terminal>,
    terminal_view: &gpui::Entity<TerminalView>,
    expected: &str,
    cx: &mut VisualTestContext,
) {
    let mut actual = String::new();
    for _ in 0..200 {
        cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| terminal.sync(window, cx));
        });
        cx.run_until_parked();
        actual = terminal_view.read_with(cx, |view, cx| view.tab_content_text(0, cx).to_string());
        if actual == expected {
            return;
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    }

    panic!("expected terminal tab title {expected:?}, got {actual:?}");
}

struct FreshWorkspaceRoot(PathBuf);

impl FreshWorkspaceRoot {
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for FreshWorkspaceRoot {
    fn drop(&mut self) {
        // Tests can return early once the async assertion succeeds. Keeping
        // cleanup in an RAII guard covers that path as well as panics.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fresh_workspace_root() -> FreshWorkspaceRoot {
    let root = std::env::temp_dir().join(format!("zmux-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).expect("create isolated zmux test workspace");
    FreshWorkspaceRoot(root)
}

#[test]
fn fresh_workspace_root_is_removed_when_its_guard_drops() {
    let path = {
        let root = fresh_workspace_root();
        let path = root.path().to_path_buf();
        assert!(path.is_dir());
        path
    };

    assert!(
        !path.exists(),
        "temporary workspace was not removed: {path:?}"
    );
}

fn bottom_terminal_count(workspace: &workspace::Workspace, cx: &gpui::App) -> usize {
    workspace
        .panel::<TerminalPanel>(cx)
        .and_then(|panel| panel.read(cx).pane())
        .map(|pane| pane.read(cx).items_len())
        .unwrap_or(0)
}

fn assert_bound(cx: &gpui::App, keystroke: &str, action: &dyn Action) {
    assert!(
        binding_matches(cx, keystroke, action),
        "expected {keystroke} to bind {}",
        action.name()
    );
}

fn assert_not_bound(cx: &gpui::App, keystroke: &str, action: &dyn Action) {
    assert!(
        !binding_matches(cx, keystroke, action),
        "expected {keystroke} not to bind {}",
        action.name()
    );
}

fn binding_matches(cx: &gpui::App, keystroke: &str, action: &dyn Action) -> bool {
    let keystroke = Keystroke::parse(keystroke).expect("valid test keystroke");
    cx.key_bindings()
        .borrow()
        .all_bindings_for_input(&[keystroke])
        .into_iter()
        .any(|binding| binding.action().partial_eq(action))
}

#[test]
fn terminal_env_scrubs_outer_image_protocol_hints() {
    let env = terminal_env();

    assert_eq!(env.get("TERM_PROGRAM").map(String::as_str), Some("zmux"));
    assert_eq!(env.get("TERM").map(String::as_str), Some("xterm-256color"));
    assert_eq!(env.get(NOTIFY_ENDPOINT_ENV).map(String::as_str), Some(""));
    assert_eq!(env.get("KITTY_WINDOW_ID").map(String::as_str), Some(""));
    assert_eq!(env.get("KITTY_PUBLIC_KEY").map(String::as_str), Some(""));
}

#[test]
fn embedded_assets_include_bundled_fonts() {
    use gpui::AssetSource;

    let fonts = zmux::Assets
        .list("fonts")
        .expect("embedded assets are listable");
    let ttf_count = fonts.iter().filter(|path| path.ends_with(".ttf")).count();

    assert_eq!(ttf_count, 8, "{fonts:?}");
    assert!(fonts.iter().any(|path| path.ends_with("Lilex-Regular.ttf")));
    assert!(
        fonts
            .iter()
            .any(|path| path.ends_with("IBMPlexSans-Regular.ttf"))
    );

    let themes = zmux::Assets
        .list("themes")
        .expect("embedded themes are listable");
    assert!(
        themes
            .iter()
            .any(|path| path.ends_with("vercel-theme.json")),
        "{themes:?}"
    );

    let icons = zmux::Assets
        .list("icons")
        .expect("embedded icons are listable");
    for process_icon in [
        "icons/ai_open_ai.svg",
        "icons/ai_claude.svg",
        "icons/ai_open_code.svg",
        "icons/ai_pi.svg",
        "icons/neovim.svg",
    ] {
        assert!(
            icons.iter().any(|path| path.as_ref() == process_icon),
            "missing embedded process icon {process_icon}: {icons:?}",
        );
    }
}

#[gpui::test]
async fn default_settings_use_the_bundled_theme_and_mono_font(cx: &mut TestAppContext) {
    cx.update(|cx| {
        settings::init(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::EditorSettings::register(cx);
        terminal::terminal_settings::TerminalSettings::register(cx);
        configure_terminal_fonts(cx);

        let terminal_settings = TerminalSettings::get_global(cx);
        assert_eq!(
            terminal_settings
                .font_family
                .as_ref()
                .map(|family| family.0.as_ref()),
            Some(zmux::DEFAULT_MONO_FONT)
        );
        assert_eq!(terminal_settings.font_size, Some(px(14.0)));
        assert!(
            terminal_settings
                .font_fallbacks
                .as_ref()
                .is_none_or(|fallbacks| fallbacks.fallback_list().is_empty()),
            "terminal defaults should not prepend named font fallbacks"
        );

        let theme_settings = theme_settings::ThemeSettings::get_global(cx);
        assert_eq!(theme_settings.ui_font_size(cx), px(16.0));
        assert_eq!(
            theme_settings.buffer_font.family.as_ref(),
            zmux::DEFAULT_MONO_FONT
        );
        assert!(
            theme_settings
                .buffer_font
                .fallbacks
                .as_ref()
                .is_none_or(|fallbacks| fallbacks.fallback_list().is_empty()),
            "buffer defaults should not prepend named font fallbacks"
        );
    });

    cx.run_until_parked();
    cx.update(|cx| assert_eq!(cx.theme().name.as_ref(), zmux::DEFAULT_THEME));
}

#[gpui::test]
async fn ui_font_size_setting_scales_the_ui_rem_size(cx: &mut TestAppContext) {
    use gpui::UpdateGlobal;

    cx.update(|cx| {
        settings::init(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::EditorSettings::register(cx);
        terminal::terminal_settings::TerminalSettings::register(cx);
        configure_terminal_fonts(cx);

        // A user bumping the UI scale in the settings page writes
        // `ui_font_size` (16 px * scale); 150% scale = 24 px.
        let mut settings_json: serde_json::Value =
            serde_json::from_str(&zmux::default_settings_json()).unwrap();
        settings_json["ui_font_size"] = serde_json::json!(24.0);
        settings_json["buffer_font_size"] = serde_json::json!(21.0);
        settings_json["terminal"]["font_size"] = serde_json::json!(21.0);
        let settings_json = settings_json.to_string();
        let result = settings::SettingsStore::update_global(cx, |store, cx| {
            store.set_user_settings(&settings_json, cx)
        });
        assert!(
            !matches!(result.parse_status, settings::ParseStatus::Failed { .. }),
            "scaled settings JSON should parse"
        );

        let theme_settings = theme_settings::ThemeSettings::get_global(cx);
        assert_eq!(theme_settings.ui_font_size(cx), px(24.0));
        assert_eq!(TerminalSettings::get_global(cx).font_size, Some(px(21.0)));
    });
}

#[gpui::test]
async fn open_settings_opens_a_single_reused_settings_tab(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let root = fresh_workspace_root();
    let open_task = cx.update(|cx| {
        init_zmux(cx);
        open_zmux_workspace_at(None, root.path().to_path_buf(), cx)
    });

    let opened = open_task
        .await
        .expect("workspace shell should open without panicking");
    cx.run_until_parked();

    for _ in 0..2 {
        opened
            .window
            .update(cx, |_, window, cx| {
                window.dispatch_action(zmux::OpenSettings.boxed_clone(), cx);
            })
            .expect("window should still be open");
        cx.run_until_parked();
    }

    let (settings_tab_count, active_is_settings) =
        opened.workspace.read_with(cx, |workspace, cx| {
            let pane = workspace.active_pane().read(cx);
            (
                pane.items_of_type::<zmux::SettingsPage>().count(),
                pane.active_item()
                    .and_then(|item| item.downcast::<zmux::SettingsPage>())
                    .is_some(),
            )
        });
    assert_eq!(settings_tab_count, 1);
    assert!(active_is_settings, "settings tab should be the active item");
}
