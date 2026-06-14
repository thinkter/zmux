use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, SharedString, Task,
    WeakEntity, Window, div, px,
};
use task::Shell;
use terminal::TerminalBuilder;
use terminal::terminal_settings::{AlternateScroll, CursorShape};
use terminal_view::TerminalView;
use util::paths::PathStyle;

use crate::env::{current_working_directory, terminal_env};

pub struct ZmuxTerminal {
    terminal_view: Option<Entity<TerminalView>>,
    load_error: Option<SharedString>,
    focus_handle: FocusHandle,
    _load_task: Task<()>,
}

impl ZmuxTerminal {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
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
                    .font_family("monospace")
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
