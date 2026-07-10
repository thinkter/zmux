use std::{fs, path::PathBuf, time::Duration};

use gpui::prelude::*;
use gpui::{Action, Bounds, Keystroke, TestAppContext, point, px, size};
use settings::Settings;
use task::Shell;
use terminal::terminal_settings::{AlternateScroll, CursorShape, TerminalSettings};
use terminal::{TerminalBounds, TerminalBuilder};
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use util::paths::PathStyle;
use workspace::dock::Panel;
use workspace::pane::{
    ActivateNextItem, ActivatePreviousItem, CloseActiveItem, CloseAllItems, CloseOtherItems,
    SplitDown, SplitRight,
};
use workspace::{ActivateNextPane, ActivatePreviousPane};
use zmux::{
    NewTerminal as ZmuxNewTerminal, configure_keybindings, configure_terminal_fonts,
    configure_zoom_actions, init_zmux, open_zmux_workspace, terminal_env,
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
            Shell::WithArguments {
                program: "sh".to_string(),
                args: vec!["-lc".to_string(), "printf 'zmux-pty-smoke\n'".to_string()],
                title_override: None,
            },
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
async fn zmux_starts_clean_when_zed_user_state_exists(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let zed_state = zed_state_fixture();
    assert!(!paths::data_dir().starts_with(&zed_state));
    assert!(!paths::config_dir().starts_with(&zed_state));
    assert!(!paths::cache_dir().starts_with(&zed_state));
    assert!(!paths::state_dir().starts_with(&zed_state));

    let open_task = cx.update(|cx| {
        init_zmux(cx);
        open_zmux_workspace(None, cx)
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
            assert_eq!(
                fs::read_to_string(zed_state.join("db/0-dev/db.sqlite")).unwrap(),
                "zed-session-fixture"
            );
            assert_eq!(
                fs::read_to_string(zed_state.join("config/settings.json")).unwrap(),
                "{\"terminal\": {\"restore_session\": true}}"
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
async fn zmux_new_terminal_action_adds_center_tab_not_bottom_panel(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let open_task = cx.update(|cx| {
        init_zmux(cx);
        open_zmux_workspace(None, cx)
    });

    let opened = open_task
        .await
        .expect("workspace shell should open without panicking");

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
        assert_bound(cx, "ctrl-tab", &ActivateNextItem::default());
        assert_bound(cx, "ctrl-shift-tab", &ActivatePreviousItem::default());
        assert_bound(cx, "ctrl-shift-w", &CloseActiveItem::default());
        assert_bound(cx, "ctrl-shift-alt-w", &CloseAllItems::default());
        assert_bound(cx, "ctrl-shift-o", &CloseOtherItems::default());
        assert_bound(cx, "alt-right", &ActivateNextPane);
        assert_bound(cx, "alt-left", &ActivatePreviousPane);
        assert_bound(cx, "ctrl-shift-d", &SplitRight::default());
        assert_bound(cx, "ctrl-shift-alt-d", &SplitDown::default());
    });
}

fn center_terminal_count(workspace: &workspace::Workspace, cx: &gpui::App) -> usize {
    workspace
        .active_pane()
        .read(cx)
        .items()
        .filter(|item| item.act_as::<TerminalView>(cx).is_some())
        .count()
}

fn bottom_terminal_count(workspace: &workspace::Workspace, cx: &gpui::App) -> usize {
    workspace
        .panel::<TerminalPanel>(cx)
        .and_then(|panel| panel.read(cx).pane())
        .map(|pane| pane.read(cx).items_len())
        .unwrap_or(0)
}

fn zed_state_fixture() -> PathBuf {
    let root = paths::test_root().join("zed-user-state");
    let database = root.join("db/0-dev/db.sqlite");
    let settings = root.join("config/settings.json");
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    fs::write(&database, "zed-session-fixture").unwrap();
    fs::write(&settings, "{\"terminal\": {\"restore_session\": true}}").unwrap();
    root
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
    assert_eq!(env.get("KITTY_WINDOW_ID").map(String::as_str), Some(""));
    assert_eq!(env.get("KITTY_PUBLIC_KEY").map(String::as_str), Some(""));
}
