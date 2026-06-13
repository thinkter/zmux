use std::time::Duration;

use gpui::prelude::*;
use gpui::{Action, Bounds, Keystroke, TestAppContext, point, px, size};
use settings::Settings;
use task::Shell;
use terminal::terminal_settings::{AlternateScroll, CursorShape, TerminalSettings};
use terminal::{TerminalBounds, TerminalBuilder};
use util::paths::PathStyle;
use zmux::{configure_keybindings, configure_terminal_fonts, configure_zoom_actions, terminal_env};

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
        assert_eq!(
            settings
                .font_family
                .as_ref()
                .map(|family| family.0.as_ref()),
            Some("JetBrains Mono")
        );
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
    });
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
