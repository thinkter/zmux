use gpui::{App, AppContext, Bounds, Focusable, WindowBounds, WindowOptions, px, size};
use gpui_platform::application;
use settings::Settings;

use crate::keymap::{Quit, configure_keybindings, configure_zoom_actions};
use crate::terminal_host::ZmuxTerminal;
use crate::theme::configure_terminal_fonts;

pub fn run() -> anyhow::Result<()> {
    application().run(|cx: &mut App| {
        settings::init(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::EditorSettings::register(cx);
        terminal::terminal_settings::TerminalSettings::register(cx);
        configure_terminal_fonts(cx);

        configure_keybindings(cx);
        configure_zoom_actions(cx);

        cx.on_action(|_: &Quit, cx| cx.quit());

        let bounds = Bounds::centered(None, size(px(960.0), px(640.0)), cx);
        let window = match cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| {
                window.set_window_title("zmux");
                cx.new(|cx| ZmuxTerminal::new(window, cx))
            },
        ) {
            Ok(window) => window,
            Err(error) => {
                eprintln!("failed to open zmux window: {error}");
                cx.quit();
                return;
            }
        };

        if let Err(error) = window.update(cx, |view, window, cx| {
            window.focus(&view.focus_handle(cx), cx);
            cx.activate(true);
        }) {
            eprintln!("failed to focus zmux window: {error}");
            cx.quit();
        }
    });

    Ok(())
}
