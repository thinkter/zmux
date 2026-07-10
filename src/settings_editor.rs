//! A small, Zmux-owned in-app editor for `config.json`.
//!
//! It deliberately edits the documented JSON document instead of opening
//! Zed's settings UI or writing Zed keymaps. Validation happens before a save
//! reaches disk, so an invalid buffer is safe to correct in place.

use editor::Editor;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render, SharedString,
    Window,
};
use ui::{Button, ButtonSize, Color, Label, LabelSize, prelude::*};
use workspace::item::{Item, ItemEvent};

use crate::config::{CONFIGURABLE_ACTIONS, ConfigStore};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsEditorMode {
    Settings,
    Keymaps,
}

impl SettingsEditorMode {
    fn title(self) -> &'static str {
        match self {
            Self::Settings => "Settings",
            Self::Keymaps => "Keymaps",
        }
    }

    fn help(self) -> String {
        match self {
            Self::Settings => {
                "Edit Zmux's own versioned config document. Save validates it and atomically replaces only zmux's config file.".to_string()
            }
            Self::Keymaps => format!(
                "Edit keybindings.overrides or keybindings.disabled in this Zmux-owned config. Configurable actions: {}.",
                CONFIGURABLE_ACTIONS.join(", ")
            ),
        }
    }
}

#[derive(Clone, Debug)]
enum EditorMessage {
    Info(String),
    Error(String),
}

/// A real editable text buffer with explicit save/reload/reset controls.
pub struct ZmuxSettingsEditor {
    mode: SettingsEditorMode,
    editor: Entity<Editor>,
    focus_handle: FocusHandle,
    message: Option<EditorMessage>,
}

impl ZmuxSettingsEditor {
    pub fn new(mode: SettingsEditorMode, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (contents, message) = match ConfigStore::global_mut(cx).editable_contents() {
            Ok(contents) => (contents, None),
            Err(error) => (
                String::new(),
                Some(EditorMessage::Error(format!(
                    "Could not open zmux config: {error}"
                ))),
            ),
        };
        let editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_text(contents, window, cx);
            editor.set_show_git_diff_gutter(false, cx);
            editor.set_show_runnables(false, cx);
            editor.set_show_bookmarks(false, cx);
            editor.set_show_breakpoints(false, cx);
            editor.set_show_code_actions(false, cx);
            editor.set_show_edit_predictions(Some(false), window, cx);
            editor.set_use_autoclose(false);
            editor
        });
        window.focus(&editor.focus_handle(cx), cx);
        Self {
            mode,
            editor,
            focus_handle: cx.focus_handle(),
            message,
        }
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let contents = self.editor.read(cx).text(cx);
        match ConfigStore::global_mut(cx).save_from_text(&contents) {
            Ok(reload) => {
                crate::app::apply_zmux_config(cx);
                self.message = Some(EditorMessage::Info(match reload.migrated_from {
                    Some(version) => format!(
                        "Saved config and migrated schema version {version} to the current schema."
                    ),
                    None => "Saved validated Zmux config.".to_string(),
                }));
                self.sync_contents(window, cx);
            }
            Err(error) => {
                self.message = Some(EditorMessage::Error(error.to_string()));
            }
        }
        cx.notify();
    }

    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match ConfigStore::global_mut(cx).reload() {
            Ok(reload) => {
                crate::app::apply_zmux_config(cx);
                self.message = Some(EditorMessage::Info(match reload.migrated_from {
                    Some(version) => format!("Reloaded and migrated schema version {version}."),
                    None if reload.changed => "Reloaded Zmux config.".to_string(),
                    None => "Config is already current.".to_string(),
                }));
                self.sync_contents(window, cx);
            }
            Err(error) => {
                self.message = Some(EditorMessage::Error(format!(
                    "Reload kept the last known-good config: {error}"
                )));
            }
        }
        cx.notify();
    }

    fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match ConfigStore::global_mut(cx).reset() {
            Ok(()) => {
                crate::app::apply_zmux_config(cx);
                self.message = Some(EditorMessage::Info(
                    "Restored and saved the default Zmux config.".to_string(),
                ));
                self.sync_contents(window, cx);
            }
            Err(error) => {
                self.message = Some(EditorMessage::Error(error.to_string()));
            }
        }
        cx.notify();
    }

    fn sync_contents(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match ConfigStore::global_mut(cx).editable_contents() {
            Ok(contents) => self.editor.update(cx, |editor, cx| {
                editor.set_text(contents, window, cx);
            }),
            Err(error) => self.message = Some(EditorMessage::Error(error.to_string())),
        }
    }
}

impl Render for ZmuxSettingsEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let message = self.message.clone();
        v_flex()
            .key_context("ZmuxSettings")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(
                v_flex()
                    .w_full()
                    .px_4()
                    .py_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Label::new(self.mode.title())
                            .size(LabelSize::Large)
                            .color(Color::Default),
                    )
                    .child(
                        Label::new(self.mode.help())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("zmux-settings-save", "Save")
                                    .size(ButtonSize::Compact)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.save(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("zmux-settings-reload", "Reload")
                                    .size(ButtonSize::Compact)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.reload(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("zmux-settings-reset", "Reset Defaults")
                                    .size(ButtonSize::Compact)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.reset(window, cx);
                                    })),
                            ),
                    )
                    .when_some(message, |this, message| match message {
                        EditorMessage::Info(message) => this.child(
                            Label::new(message)
                                .size(LabelSize::Small)
                                .color(Color::Success),
                        ),
                        EditorMessage::Error(message) => this.child(
                            Label::new(message)
                                .size(LabelSize::Small)
                                .color(Color::Error),
                        ),
                    }),
            )
            .child(
                div()
                    .id("zmux-settings-editor")
                    .flex_1()
                    .child(self.editor.clone()),
            )
    }
}

impl EventEmitter<ItemEvent> for ZmuxSettingsEditor {}

impl Focusable for ZmuxSettingsEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for ZmuxSettingsEditor {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.mode.title().into()
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Zmux Settings Editor Opened")
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}
