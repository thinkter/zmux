use gpui::{App, KeyBinding, Unbind, actions};
use terminal::{Copy, Paste, ScrollPageDown, ScrollPageUp, ScrollToBottom};
use workspace::pane::{CloseActiveItem, CloseAllItems, CloseOtherItems};
use workspace::{ActivateNextPane, ActivatePreviousPane};
use zed_actions::{DecreaseBufferFontSize, IncreaseBufferFontSize, ResetBufferFontSize};

use crate::app::{JumpToLatestNotification, NotifyCurrentPane};
use crate::workspaces::{
    ActivateNextWorkspace, ActivatePreviousWorkspace, NewWorkspace, ToggleNotificationCenter,
    ToggleWorkspacesPanel,
};

actions!(
    zmux,
    [
        NewTerminal,
        SplitTerminalRight,
        SplitTerminalDown,
        OpenSettings,
        Quit
    ]
);

#[cfg(target_os = "macos")]
const DEFAULT_KEYMAP: &str = "keymaps/default-macos.json";
#[cfg(target_os = "linux")]
const DEFAULT_KEYMAP: &str = "keymaps/default-linux.json";
#[cfg(target_os = "windows")]
const DEFAULT_KEYMAP: &str = "keymaps/default-windows.json";
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const DEFAULT_KEYMAP: &str = "keymaps/default-linux.json";

pub fn configure_keybindings(cx: &mut App) {
    if let Ok(bindings) = settings::KeymapFile::load_asset_allow_partial_failure(DEFAULT_KEYMAP, cx)
    {
        cx.bind_keys(bindings);
    }
    if let Ok(bindings) =
        settings::KeymapFile::load_asset_allow_partial_failure(settings::VIM_KEYMAP_PATH, cx)
    {
        cx.bind_keys(bindings);
    }

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
        KeyBinding::new("ctrl-shift-t", NewTerminal, None),
        KeyBinding::new("ctrl-shift-n", NewTerminal, None),
        KeyBinding::new("ctrl-shift-e", NewWorkspace, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-t", NewTerminal, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-n", NewWorkspace, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-ctrl-w", zed_actions::git::Worktree, None),
        #[cfg(target_os = "linux")]
        KeyBinding::new("alt-ctrl-shift-w", zed_actions::git::Worktree, None),
        #[cfg(target_os = "windows")]
        KeyBinding::new("shift-alt-w", zed_actions::git::Worktree, None),
        KeyBinding::new("ctrl-shift-b", ToggleWorkspacesPanel, None),
        KeyBinding::new("ctrl-}", ActivateNextWorkspace, None),
        KeyBinding::new("ctrl-{", ActivatePreviousWorkspace, None),
        KeyBinding::new("ctrl-shift-right", ActivateNextWorkspace, None),
        KeyBinding::new("ctrl-shift-left", ActivatePreviousWorkspace, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-b", ToggleWorkspacesPanel, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-}", ActivateNextWorkspace, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-{", ActivatePreviousWorkspace, None),
        KeyBinding::new("ctrl-shift-m", NotifyCurrentPane, None),
        KeyBinding::new("ctrl-shift-u", JumpToLatestNotification, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-m", NotifyCurrentPane, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-u", JumpToLatestNotification, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-i", ToggleNotificationCenter, None),
        KeyBinding::new("ctrl-shift-i", ToggleNotificationCenter, None),
        KeyBinding::new("ctrl-tab", tab_switcher::Toggle::default(), None),
        KeyBinding::new(
            "ctrl-shift-tab",
            tab_switcher::Toggle { select_last: true },
            None,
        ),
        KeyBinding::new("ctrl-shift-w", CloseActiveItem::default(), None),
        KeyBinding::new("ctrl-shift-alt-w", CloseAllItems::default(), None),
        KeyBinding::new("ctrl-shift-o", CloseOtherItems::default(), None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-w", CloseActiveItem::default(), None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-alt-w", CloseAllItems::default(), None),
        KeyBinding::new("alt-right", ActivateNextPane, None),
        KeyBinding::new("alt-left", ActivatePreviousPane, None),
        // These actions allocate a fresh CLI route before spawning the split.
        // Built-in pane-menu clone actions are captured at the workspace root
        // and routed through the same provisioned spawn path.
        KeyBinding::new("ctrl-shift-d", SplitTerminalRight, None),
        KeyBinding::new("ctrl-shift-alt-d", SplitTerminalDown, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-d", SplitTerminalRight, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-d", SplitTerminalDown, None),
        KeyBinding::new("ctrl-=", IncreaseBufferFontSize { persist: false }, None),
        KeyBinding::new("ctrl-+", IncreaseBufferFontSize { persist: false }, None),
        KeyBinding::new("ctrl--", DecreaseBufferFontSize { persist: false }, None),
        KeyBinding::new("ctrl-0", ResetBufferFontSize { persist: false }, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-=", IncreaseBufferFontSize { persist: false }, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-+", IncreaseBufferFontSize { persist: false }, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd--", DecreaseBufferFontSize { persist: false }, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-0", ResetBufferFontSize { persist: false }, None),
        KeyBinding::new("ctrl-,", OpenSettings, None),
        KeyBinding::new("cmd-,", OpenSettings, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new(
            "cmd-k cmd-t",
            Unbind("theme_selector::Toggle".into()),
            Some("Workspace"),
        ),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new(
            "ctrl-k ctrl-t",
            Unbind("theme_selector::Toggle".into()),
            Some("Workspace"),
        ),
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
