use gpui::{Action as _, App, KeyBinding, Unbind, actions};
use terminal::{Copy, Paste, ScrollPageDown, ScrollPageUp, ScrollToBottom};
use workspace::pane::{
    ActivateNextItem, ActivatePreviousItem, CloseActiveItem, CloseAllItems, CloseOtherItems,
    SplitDown, SplitRight,
};
use workspace::{ActivateNextPane, ActivatePreviousPane};
use zed_actions::{DecreaseBufferFontSize, IncreaseBufferFontSize, ResetBufferFontSize};

use crate::app::{JumpToLatestNotification, NotifyCurrentPane};
use crate::config::{ConfigStore, ZmuxConfig};
use crate::workspaces::{
    ActivateNextWorkspace, ActivatePreviousWorkspace, NewWorkspace, ToggleWorkspacesPanel,
};

actions!(
    zmux,
    [
        NewTerminal,
        OpenSettings,
        OpenKeymaps,
        ReloadConfig,
        ResetConfig,
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

/// Install Zmux's effective shortcut set. Calling this again replaces the
/// previous set, which is what makes config reload deterministic rather than
/// stacking stale keybindings behind the new ones.
pub fn configure_keybindings(cx: &mut App) {
    let config = cx
        .try_global::<ConfigStore>()
        .map(|store| store.config().clone())
        .unwrap_or_default();
    configure_keybindings_with_config(&config, cx);
}

pub fn configure_keybindings_with_config(config: &ZmuxConfig, cx: &mut App) {
    cx.clear_key_bindings();
    if let Ok(bindings) = settings::KeymapFile::load_asset_allow_partial_failure(DEFAULT_KEYMAP, cx)
    {
        cx.bind_keys(bindings);
    }

    let keybindings = &config.keybindings;
    let mut bindings = Vec::new();
    macro_rules! configurable {
        ($name:literal, $action:expr, [$($default:literal),+ $(,)?]) => {
            let disabled = keybindings.is_disabled($name);
            let override_key = keybindings.override_for($name);
            // The base Zed keymap is retained for editor ergonomics. When a
            // Zmux action is overridden or disabled, unbind its old sequences
            // first (with a broad context) so an inherited binding cannot win
            // at a deeper context such as Terminal.
            if disabled || override_key.is_some() {
                let action_name: gpui::SharedString = $action.name().into();
                $(bindings.push(KeyBinding::new($default, Unbind(action_name.clone()), None));)+
            }
            if !disabled {
                if let Some(override_key) = override_key {
                    bindings.push(KeyBinding::new(override_key, $action, None));
                } else {
                    $(bindings.push(KeyBinding::new($default, $action, None));)+
                }
            }
        };
    }

    configurable!("copy", Copy, ["cmd-c", "ctrl-shift-c", "ctrl-insert"]);
    configurable!("paste", Paste, ["cmd-v", "ctrl-shift-v", "shift-insert"]);
    configurable!(
        "scroll_page_down",
        ScrollPageDown,
        ["pagedown", "shift-pagedown"]
    );
    configurable!("scroll_page_up", ScrollPageUp, ["pageup", "shift-pageup"]);
    configurable!("scroll_to_bottom", ScrollToBottom, ["end"]);
    configurable!(
        "new_terminal",
        NewTerminal,
        ["ctrl-shift-t", "ctrl-shift-n"]
    );
    configurable!("new_workspace", NewWorkspace, ["ctrl-shift-e"]);
    configurable!(
        "toggle_workspaces_panel",
        ToggleWorkspacesPanel,
        ["ctrl-shift-b"]
    );
    configurable!(
        "next_workspace",
        ActivateNextWorkspace,
        ["ctrl-}", "ctrl-shift-right"]
    );
    configurable!(
        "previous_workspace",
        ActivatePreviousWorkspace,
        ["ctrl-{", "ctrl-shift-left"]
    );
    configurable!("notify_current_pane", NotifyCurrentPane, ["ctrl-shift-m"]);
    configurable!(
        "jump_to_latest_notification",
        JumpToLatestNotification,
        ["ctrl-shift-u"]
    );
    configurable!("open_settings", OpenSettings, ["ctrl-,"]);
    configurable!("open_keymaps", OpenKeymaps, ["ctrl-shift-,"]);
    configurable!("reload_config", ReloadConfig, ["ctrl-alt-r"]);
    configurable!("reset_config", ResetConfig, ["ctrl-alt-shift-r"]);
    configurable!("next_tab", ActivateNextItem::default(), ["ctrl-tab"]);
    configurable!(
        "previous_tab",
        ActivatePreviousItem::default(),
        ["ctrl-shift-tab"]
    );
    configurable!("close_tab", CloseActiveItem::default(), ["ctrl-shift-w"]);
    configurable!(
        "close_all_tabs",
        CloseAllItems::default(),
        ["ctrl-shift-alt-w"]
    );
    configurable!(
        "close_other_tabs",
        CloseOtherItems::default(),
        ["ctrl-shift-o"]
    );
    configurable!("next_pane", ActivateNextPane, ["alt-right"]);
    configurable!("previous_pane", ActivatePreviousPane, ["alt-left"]);
    configurable!("split_right", SplitRight::default(), ["ctrl-shift-d"]);
    configurable!("split_down", SplitDown::default(), ["ctrl-shift-alt-d"]);
    configurable!(
        "increase_font_size",
        IncreaseBufferFontSize { persist: false },
        ["ctrl-=", "ctrl-+", "cmd-=", "cmd-+"]
    );
    configurable!(
        "decrease_font_size",
        DecreaseBufferFontSize { persist: false },
        ["ctrl--", "cmd--"]
    );
    configurable!(
        "reset_font_size",
        ResetBufferFontSize { persist: false },
        ["ctrl-0", "cmd-0"]
    );
    configurable!("quit", Quit, ["cmd-q"]);

    cx.bind_keys(bindings);
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
