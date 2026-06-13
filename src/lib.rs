use collections::HashMap;
use std::path::PathBuf;

use gpui::prelude::*;
use gpui::{
    App, Bounds, Context, Entity, FocusHandle, Focusable, IntoElement, KeyBinding, Render,
    SharedString, Task, UpdateGlobal, WeakEntity, Window, WindowBounds, WindowOptions, actions,
    div, px, size,
};
use gpui_platform::application;
use settings::Settings;
use task::Shell;
use terminal::terminal_settings::{AlternateScroll, CursorShape};
use terminal::{Copy, Paste, ScrollPageDown, ScrollPageUp, ScrollToBottom, TerminalBuilder};
use terminal_view::TerminalView;
use util::paths::PathStyle;
use zed_actions::{DecreaseBufferFontSize, IncreaseBufferFontSize, ResetBufferFontSize};
actions!(zmux, [Quit]);

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

pub struct ZmuxTerminal {
    terminal_view: Option<Entity<TerminalView>>,
    load_error: Option<SharedString>,
    focus_handle: FocusHandle,
    _load_task: Task<()>,
}

impl ZmuxTerminal {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let builder_task = TerminalBuilder::new(
            current_working_directory(),
            None,
            Shell::System,
            terminal_env(),
            CursorShape::Block,
            AlternateScroll::On,
            Some(10_000),
            Vec::new(),
            0,
            false,
            cx.entity_id().as_u64(),
            None,
            cx,
            Vec::new(),
            PathStyle::local(),
        );

        let load_task = cx.spawn_in(window, async move |this, cx| match builder_task.await {
            Ok(builder) => {
                this.update_in(cx, |this, window, cx| {
                    let terminal = cx.new(|cx| builder.subscribe(cx));
                    let terminal_view = cx.new(|cx| {
                        TerminalView::new(
                            terminal,
                            WeakEntity::new_invalid(),
                            None,
                            WeakEntity::new_invalid(),
                            window,
                            cx,
                        )
                    });
                    window.focus(&terminal_view.focus_handle(cx), cx);
                    this.terminal_view = Some(terminal_view);
                    this.load_error = None;
                    cx.notify();
                })
                .ok();
            }
            Err(error) => {
                this.update(cx, |this, cx| {
                    this.load_error = Some(error.to_string().into());
                    cx.notify();
                })
                .ok();
            }
        });

        Self {
            terminal_view: None,
            load_error: None,
            focus_handle,
            _load_task: load_task,
        }
    }
}

impl Focusable for ZmuxTerminal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if let Some(terminal_view) = &self.terminal_view {
            terminal_view.focus_handle(cx)
        } else {
            self.focus_handle.clone()
        }
    }
}

impl Render for ZmuxTerminal {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("zmux-terminal-host")
            .size_full()
            .track_focus(&self.focus_handle)
            .child(match self.terminal_view.clone() {
                Some(terminal_view) => terminal_view.into_any_element(),
                None => div()
                    .size_full()
                    .bg(gpui::rgb(0x101010))
                    .text_color(gpui::rgb(0xd8d8d8))
                    .font_family("JetBrains Mono")
                    .text_size(px(14.0))
                    .line_height(px(18.0))
                    .child(
                        self.load_error
                            .clone()
                            .unwrap_or_else(|| "starting shell...".into()),
                    )
                    .into_any_element(),
            })
    }
}

pub fn configure_keybindings(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-c", Copy, None),
        KeyBinding::new("cmd-v", Paste, None),
        KeyBinding::new("ctrl-shift-c", Copy, None),
        KeyBinding::new("ctrl-insert", Copy, None),
        KeyBinding::new("ctrl-shift-v", Paste, None),
        KeyBinding::new("shift-insert", Paste, None),
        KeyBinding::new("pageup", ScrollPageUp, None),
        KeyBinding::new("shift-pageup", ScrollPageUp, None),
        KeyBinding::new("pagedown", ScrollPageDown, None),
        KeyBinding::new("shift-pagedown", ScrollPageDown, None),
        KeyBinding::new("end", ScrollToBottom, None),
        KeyBinding::new("ctrl-=", IncreaseBufferFontSize { persist: false }, None),
        KeyBinding::new("ctrl-+", IncreaseBufferFontSize { persist: false }, None),
        KeyBinding::new("ctrl--", DecreaseBufferFontSize { persist: false }, None),
        KeyBinding::new("ctrl-0", ResetBufferFontSize { persist: false }, None),
        KeyBinding::new("cmd-=", IncreaseBufferFontSize { persist: false }, None),
        KeyBinding::new("cmd-+", IncreaseBufferFontSize { persist: false }, None),
        KeyBinding::new("cmd--", DecreaseBufferFontSize { persist: false }, None),
        KeyBinding::new("cmd-0", ResetBufferFontSize { persist: false }, None),
        KeyBinding::new("cmd-q", Quit, None),
    ]);
}

pub fn configure_zoom_actions(cx: &mut App) {
    cx.on_action(|action: &IncreaseBufferFontSize, cx| {
        if !action.persist {
            theme_settings::increase_buffer_font_size(cx);
        }
    });
    cx.on_action(|action: &DecreaseBufferFontSize, cx| {
        if !action.persist {
            theme_settings::decrease_buffer_font_size(cx);
        }
    });
    cx.on_action(|action: &ResetBufferFontSize, cx| {
        if !action.persist {
            theme_settings::reset_buffer_font_size(cx);
        }
    });
}

fn current_working_directory() -> Option<PathBuf> {
    std::env::current_dir().ok()
}

pub fn configure_terminal_fonts(cx: &mut App) {
    const SETTINGS: &str = r#"
        {
          "buffer_font_family": "JetBrains Mono",
          "buffer_font_features": {},
          "buffer_font_size": 14,
          "buffer_font_fallbacks": ["DejaVu Sans Mono", "Noto Color Emoji"],
          "buffer_line_height": {
            "custom": 1.2
          },
          "terminal": {
            "font_family": "JetBrains Mono",
            "font_features": {},
            "font_size": 14,
            "font_fallbacks": ["DejaVu Sans Mono", "Noto Color Emoji"],
            "line_height": {
              "custom": 1.2
            }
          }
        }
    "#;

    settings::SettingsStore::update_global(cx, |store, cx| {
        let _ = store.set_user_settings(SETTINGS, cx);
    });
}

pub fn terminal_env() -> HashMap<String, String> {
    HashMap::from_iter([
        ("TERM_PROGRAM".to_string(), "zmux".to_string()),
        (
            "TERM_PROGRAM_VERSION".to_string(),
            env!("CARGO_PKG_VERSION").to_string(),
        ),
        ("ZED_TERM".to_string(), "true".to_string()),
        ("TERM".to_string(), "xterm-256color".to_string()),
        ("COLORTERM".to_string(), "truecolor".to_string()),
        // Do not advertise outer terminal image protocols unless zmux renders them.
        ("KITTY_WINDOW_ID".to_string(), String::new()),
        ("KITTY_PID".to_string(), String::new()),
        ("KITTY_PUBLIC_KEY".to_string(), String::new()),
        ("KITTY_INSTALLATION_DIR".to_string(), String::new()),
        ("WEZTERM_PANE".to_string(), String::new()),
        ("GHOSTTY_RESOURCES_DIR".to_string(), String::new()),
    ])
}
