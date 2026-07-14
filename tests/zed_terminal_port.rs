//! Tests ported from `../zed/crates/terminal/src/terminal.rs`.
//!
//! The original Zed tests often live inside the `terminal` crate and can access
//! private internals. These copies keep the same behavioral assertions, but
//! adapt setup/sync code to use only public APIs available to zmux.

mod support;

use std::{collections::HashMap, process::ExitStatus, time::Duration};

use alacritty_terminal::term::build_zmux_pty_response;
use async_channel::Receiver;
use gpui::prelude::*;
use gpui::{
    Bounds, ClipboardItem, Context, Entity, IntoElement, Render, TestAppContext, Window, point, px,
    size,
};
use settings::Settings;
use support::deterministic_output_shell;
use task::Shell;
use terminal::terminal_settings::{AlternateScroll, CursorShape};
use terminal::{
    Color, Content, NamedColor, Terminal, TerminalBounds, TerminalBuilder, parse_ansi_text,
    strip_ansi_text,
};
use util::paths::PathStyle;

#[test]
fn strip_ansi_text_removes_ansi_and_handles_carriage_returns() {
    let cases = [
        ("no escape codes here\n", "no escape codes here\n"),
        ("\x1b[31mhello\x1b[0m", "hello"),
        ("\x1b[1;32mfoo\x1b[0m bar", "foo bar"),
        ("progress 10%\rprogress 100%\n", "progress 100%\n"),
    ];

    for (input, expected) in cases {
        assert_eq!(strip_ansi_text(input.as_bytes()), expected);
    }
}

#[test]
fn parse_ansi_text_records_foreground_and_background_spans() {
    let parsed = parse_ansi_text(b"\x1b[31mred\x1b[44mblue-bg\x1b[0mplain");

    assert_eq!(parsed.text, "redblue-bgplain");
    assert_eq!(
        parsed.foreground_spans,
        vec![
            (0..0, None),
            (0..10, Some(Color::Named(NamedColor::Red))),
            (10..15, None),
        ]
    );
    assert_eq!(
        parsed.background_spans,
        vec![
            (0..3, None),
            (3..10, Some(Color::Named(NamedColor::Blue))),
            (10..15, None),
        ]
    );
}

#[gpui::test]
async fn test_basic_terminal(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    init_terminal_test_app(cx);

    let (terminal, completion_rx) =
        build_command_terminal(cx, deterministic_output_shell("hello")).await;
    assert_content_eventually(&terminal, "hello", cx).await;

    // Inject additional output directly into the emulator (display-only path)
    terminal.update(cx, |term, cx| {
        term.write_output(b"\nfrom_injection", cx);
    });

    let content_after = terminal.update(cx, |term, _| term.get_content());
    assert!(
        content_after.contains("from_injection"),
        "expected injected output to appear, got: {content_after}"
    );

    let exit_status = completion_rx.recv().await.unwrap();
    assert_eq!(exit_status, Some(ExitStatus::default()));
}

#[gpui::test]
async fn test_write_output_converts_lf_to_crlf(cx: &mut TestAppContext) {
    let content = synced_display_only_content(cx, b"line1\nline2\n");
    let cells = &content.cells;
    let mut line1_col0 = false;
    let mut line2_col0 = false;

    for cell in cells {
        if cell.character() == 'l' && cell.point.column == 0 {
            if cell.point.line == 0 && !line1_col0 {
                line1_col0 = true;
            } else if cell.point.line == 1 && !line2_col0 {
                line2_col0 = true;
            }
        }
    }

    assert!(line1_col0, "First line should start at column 0");
    assert!(line2_col0, "Second line should start at column 0");
}

#[gpui::test]
async fn test_write_output_preserves_existing_crlf(cx: &mut TestAppContext) {
    let content = synced_display_only_content(cx, b"line1\r\nline2\r\n");
    let cells = &content.cells;

    let mut found_lines_at_column_0 = 0;
    for cell in cells {
        if cell.character() == 'l' && cell.point.column == 0 {
            found_lines_at_column_0 += 1;
        }
    }

    assert!(
        found_lines_at_column_0 >= 2,
        "Both lines should start at column 0"
    );
}

#[gpui::test]
async fn test_write_output_preserves_bare_cr(cx: &mut TestAppContext) {
    let content = synced_display_only_content(cx, b"hello\rworld");
    let cells = &content.cells;

    let mut text = String::new();
    for cell in cells.iter().take(5) {
        if cell.point.line == 0 {
            text.push(cell.character());
        }
    }

    assert!(
        text.starts_with("world"),
        "Bare CR should allow overwriting: got '{}'",
        text
    );
}

#[gpui::test]
async fn test_display_only_write_output_ignores_osc52(cx: &mut TestAppContext) {
    cx.update(|cx| {
        cx.write_to_clipboard(ClipboardItem::new_string("original".to_string()));
    });

    let terminal = cx.new(|cx| {
        TerminalBuilder::new_display_only(
            CursorShape::default(),
            AlternateScroll::On,
            None,
            0,
            cx.background_executor(),
            PathStyle::local(),
        )
        .subscribe(cx)
    });

    terminal.update(cx, |terminal, cx| {
        terminal.write_output(b"\x1b]52;c;b3ZlcndyaXR0ZW4=\x07", cx);
    });
    cx.run_until_parked();

    let clipboard_text = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
    assert_eq!(clipboard_text.as_deref(), Some("original"));
}

#[gpui::test]
async fn authenticated_pty_response_preserves_scroll_selection_and_input_state(
    cx: &mut TestAppContext,
) {
    let bounds = TerminalBounds::new(
        px(18.0),
        px(9.0),
        Bounds::new(point(px(0.0), px(0.0)), size(px(720.0), px(180.0))),
    );
    let terminal = cx.new(|cx| {
        TerminalBuilder::new_display_only_with_bounds(
            CursorShape::default(),
            AlternateScroll::On,
            None,
            0,
            cx.background_executor(),
            PathStyle::local(),
            bounds,
        )
        .subscribe(cx)
    });
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|_| TestView))
            .unwrap()
    });
    let mut transcript = (0..100)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    transcript.push_str("selection sentinel\n");
    terminal.update(cx, |terminal, cx| {
        terminal.write_output(transcript.as_bytes(), cx);
        terminal.scroll_to_top();
        terminal.select_all();
        terminal.take_input_log();
    });
    window
        .update(cx, |_, window, cx| {
            terminal.update(cx, |terminal, cx| terminal.sync(window, cx));
        })
        .unwrap();
    assert!(terminal.read_with(cx, |terminal, _| terminal.scrolled_to_top()));

    let response = build_zmux_pty_response(b"\x1b]99;i=query:p=?;capabilities\x1b\\")
        .expect("bounded protocol response is valid");
    terminal.update(cx, |terminal, cx| terminal.write_output(&response, cx));
    cx.run_until_parked();
    window
        .update(cx, |_, window, cx| {
            terminal.update(cx, |terminal, cx| terminal.sync(window, cx));
        })
        .unwrap();

    terminal.update(cx, |terminal, _| {
        assert!(terminal.take_input_log().is_empty());
        assert!(terminal.scrolled_to_top());
        terminal.copy(Some(true));
    });
    window
        .update(cx, |_, window, cx| {
            terminal.update(cx, |terminal, cx| terminal.sync(window, cx));
        })
        .unwrap();
    let copied = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
    assert!(
        copied.is_some_and(|text| text.contains("selection sentinel")),
        "trusted backend responses must preserve the active selection"
    );
}

// Windows ConPTY can deliver neither completion nor output to GPUI's parked
// test scheduler for this immediate-exit no-op case. `test_basic_terminal`
// still covers Windows process completion; the platform-independent no-op
// behavior remains covered on Unix.
#[cfg(not(target_os = "windows"))]
#[gpui::test]
async fn test_kill_active_task_on_completed_task_is_noop(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    init_terminal_test_app(cx);

    let (terminal, completion_rx) =
        build_command_terminal(cx, deterministic_output_shell("done")).await;

    // Drive terminal output before awaiting the completion channel. On
    // Windows the ConPTY completion callback can otherwise race the GPUI test
    // scheduler into parking without a registered waker.
    assert_content_eventually(&terminal, "done", cx).await;

    let exit_status = completion_rx.recv().await.unwrap();
    assert_eq!(exit_status, Some(ExitStatus::default()));

    terminal.update(cx, |term, _cx| {
        term.kill_active_task();
    });

    let content = terminal.update(cx, |term, _| term.get_content());
    assert!(
        content.contains("done"),
        "Output should still be present after no-op kill, got: {content}"
    );
}

#[test]
fn test_num_lines_float_precision() {
    let line_heights = [
        20.1f32, 16.7, 18.3, 22.9, 14.1, 15.6, 17.8, 19.4, 21.3, 23.7,
    ];
    for &line_height in &line_heights {
        for n in 1..=100 {
            let height = n as f32 * line_height;
            let bounds = TerminalBounds::new(
                px(line_height),
                px(8.0),
                Bounds {
                    origin: point(px(0.0), px(0.0)),
                    size: size(px(800.0), px(height)),
                },
            );
            assert_eq!(
                bounds.num_lines(),
                n,
                "num_lines() should be {n} for height={height}, line_height={line_height}"
            );
        }
    }
}

#[test]
fn test_num_columns_float_precision() {
    let cell_widths = [8.1f32, 7.3, 9.7, 6.9, 10.1];
    for &cell_width in &cell_widths {
        for n in 1..=200 {
            let width = n as f32 * cell_width;
            let bounds = TerminalBounds::new(
                px(20.0),
                px(cell_width),
                Bounds {
                    origin: point(px(0.0), px(0.0)),
                    size: size(px(width), px(400.0)),
                },
            );
            assert_eq!(
                bounds.num_columns(),
                n,
                "num_columns() should be {n} for width={width}, cell_width={cell_width}"
            );
        }
    }
}

fn init_terminal_test_app(cx: &mut TestAppContext) {
    cx.update(|cx| {
        settings::init(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        terminal::terminal_settings::TerminalSettings::register(cx);
    });
}

async fn build_command_terminal(
    cx: &mut TestAppContext,
    shell: Shell,
) -> (Entity<Terminal>, Receiver<Option<ExitStatus>>) {
    let (completion_tx, completion_rx) = async_channel::unbounded();
    let builder_task = cx.update(|cx| {
        TerminalBuilder::new(
            None,
            None,
            shell,
            HashMap::default(),
            CursorShape::default(),
            AlternateScroll::On,
            None,
            Vec::new(),
            0,
            false,
            0,
            Some(completion_tx),
            cx,
            Vec::new(),
            PathStyle::local(),
        )
    });
    let builder = builder_task.await.unwrap();
    (cx.new(|cx| builder.subscribe(cx)), completion_rx)
}

async fn assert_content_eventually(
    terminal: &Entity<Terminal>,
    expected: &str,
    cx: &mut TestAppContext,
) {
    let mut content = String::new();
    for _ in 0..100 {
        content = terminal.update(cx, |term, _| term.get_content());
        if content.contains(expected) {
            return;
        }
        cx.background_executor
            .timer(Duration::from_millis(10))
            .await;
    }
    panic!("Expected terminal content to contain {expected:?}, got: {content}");
}

struct TestView;

impl Render for TestView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::div()
    }
}

fn synced_display_only_content(cx: &mut TestAppContext, output: &[u8]) -> Content {
    let bounds = TerminalBounds::new(
        px(18.0),
        px(9.0),
        Bounds::new(point(px(0.0), px(0.0)), size(px(720.0), px(180.0))),
    );
    let terminal = cx.new(|cx| {
        TerminalBuilder::new_display_only_with_bounds(
            CursorShape::default(),
            AlternateScroll::On,
            None,
            0,
            cx.background_executor(),
            PathStyle::local(),
            bounds,
        )
        .subscribe(cx)
    });
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|_| TestView))
            .unwrap()
    });

    terminal.update(cx, |terminal, cx| {
        terminal.write_output(output, cx);
    });
    window
        .update(cx, |_, window, cx| {
            terminal.update(cx, |terminal, cx| terminal.sync(window, cx));
        })
        .unwrap();

    terminal.read_with(cx, |terminal, _| terminal.last_content().clone())
}
