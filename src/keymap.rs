use gpui::{App, KeyBinding, actions};
use terminal::{Copy, Paste, ScrollPageDown, ScrollPageUp, ScrollToBottom};
use zed_actions::{DecreaseBufferFontSize, IncreaseBufferFontSize, ResetBufferFontSize};

actions!(zmux, [Quit]);

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
