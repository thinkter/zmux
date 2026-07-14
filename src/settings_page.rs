//! The Settings tab: a workspace item exposing zmux's appearance settings.
//! Every control writes through Zed's `SettingsStore::update_settings_file`,
//! so changes land in the user's `settings.json` and apply live via the
//! settings-file watcher installed by `crate::app::load_user_settings`.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement, ParentElement, Render,
    SharedString, Styled, Window,
};
use settings::{Settings, SettingsContent, SettingsStore};
use terminal::terminal_settings::TerminalSettings;
use theme_settings::ThemeSettings;
use ui::{
    Color, ContextMenu, Divider, DividerColor, DropdownMenu, Headline, IconButton, IconName,
    IntoElement, Label, LabelSize, Switch, ToggleState, h_flex, prelude::*, v_flex,
};
use vim_mode_setting::VimModeSetting;
use workspace::AppState;
use workspace::item::{Item, ItemEvent};

use crate::theme::{DEFAULT_MONO_FONT, DEFAULT_TERMINAL_FONT_SIZE, DEFAULT_UI_FONT_SIZE};

/// The whole UI is laid out in rems derived from `ui_font_size`, so the
/// scale shown to the user is `ui_font_size / 16` (Zed's base rem size).
const UI_SCALE_STEP: f32 = 0.05;
const MIN_UI_SCALE: f32 = 0.5;
const MAX_UI_SCALE: f32 = 3.0;
const TERMINAL_FONT_SIZE_STEP: f32 = 1.0;
const MIN_TERMINAL_FONT_SIZE: f32 = 6.0;
const MAX_TERMINAL_FONT_SIZE: f32 = 32.0;

pub struct SettingsPage {
    focus_handle: FocusHandle,
}

impl SettingsPage {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
        }
    }
}

fn update_settings_file(
    cx: &mut App,
    update: impl FnOnce(&mut SettingsContent, &App) + Send + 'static,
) {
    let fs = AppState::global(cx).fs.clone();
    cx.global::<SettingsStore>()
        .update_settings_file(fs, update);
}

fn current_ui_scale(cx: &App) -> f32 {
    f32::from(ThemeSettings::get_global(cx).ui_font_size_settings()) / DEFAULT_UI_FONT_SIZE
}

fn set_ui_scale(scale: f32, cx: &mut App) {
    // Steps are multiples of 0.05; round so repeated clicks can't drift.
    let scale = (scale.clamp(MIN_UI_SCALE, MAX_UI_SCALE) * 20.0).round() / 20.0;
    update_settings_file(cx, move |content, _| {
        content.theme.ui_font_size = Some((DEFAULT_UI_FONT_SIZE * scale).into());
    });
}

fn current_terminal_font_size(cx: &App) -> f32 {
    let configured_size = TerminalSettings::get_global(cx)
        .font_size
        .unwrap_or_else(|| ThemeSettings::get_global(cx).buffer_font_size_settings());
    f32::from(theme_settings::adjusted_font_size(configured_size, cx))
}

fn set_terminal_font_size(size: f32, cx: &mut App) {
    let size = size
        .clamp(MIN_TERMINAL_FONT_SIZE, MAX_TERMINAL_FONT_SIZE)
        .round();
    update_settings_file(cx, move |content, _| {
        content.theme.buffer_font_size = Some(size.into());
        content.terminal.get_or_insert_default().font_size = Some(size.into());
    });
}

fn current_font_family(cx: &App) -> SharedString {
    let family = TerminalSettings::get_global(cx)
        .font_family
        .as_ref()
        .map(|family| family.0.to_string())
        .unwrap_or_else(|| ThemeSettings::get_global(cx).buffer_font.family.to_string());
    if family == DEFAULT_MONO_FONT {
        "Default (Lilex)".into()
    } else {
        family.into()
    }
}

fn set_font_family(family: String, cx: &mut App) {
    update_settings_file(cx, move |content, _| {
        content.theme.buffer_font_family = Some(family.clone().into());
        content.terminal.get_or_insert_default().font_family = Some(family.into());
    });
}

fn current_vim_mode(cx: &App) -> bool {
    VimModeSetting::get_global(cx).0
}

fn apply_vim_mode(content: &mut SettingsContent, enabled: bool) {
    content.vim_mode = Some(enabled);
    if enabled && content.helix_mode == Some(true) {
        content.helix_mode = Some(false);
    }
}

fn set_vim_mode(enabled: bool, cx: &mut App) {
    update_settings_file(cx, move |content, _| apply_vim_mode(content, enabled));
}

#[derive(IntoElement)]
struct SectionHeader {
    title: SharedString,
}

impl SectionHeader {
    fn new(title: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
        }
    }
}

impl RenderOnce for SectionHeader {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        h_flex()
            .px_1()
            .mb_2()
            .gap_2()
            .child(
                Label::new(self.title.to_ascii_uppercase())
                    .buffer_font(cx)
                    .color(Color::Muted)
                    .size(LabelSize::XSmall),
            )
            .child(Divider::horizontal().color(DividerColor::BorderVariant))
    }
}

fn setting_row(
    label: &'static str,
    description: &'static str,
    controls: impl IntoElement,
) -> impl IntoElement {
    h_flex()
        .w_full()
        .py_2()
        .gap_4()
        .justify_between()
        .child(
            v_flex().child(Label::new(label)).child(
                Label::new(description)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            ),
        )
        .child(controls)
}

fn stepper(
    id_prefix: &'static str,
    value_label: SharedString,
    on_decrease: impl Fn(&mut Window, &mut App) + 'static,
    on_increase: impl Fn(&mut Window, &mut App) + 'static,
    on_reset: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    h_flex()
        .gap_1()
        .child(
            IconButton::new(
                SharedString::from(format!("{id_prefix}-decrease")),
                IconName::Dash,
            )
            .on_click(move |_, window, cx| on_decrease(window, cx)),
        )
        .child(
            h_flex()
                .min_w_16()
                .justify_center()
                .child(Label::new(value_label)),
        )
        .child(
            IconButton::new(
                SharedString::from(format!("{id_prefix}-increase")),
                IconName::Plus,
            )
            .on_click(move |_, window, cx| on_increase(window, cx)),
        )
        .child(
            IconButton::new(
                SharedString::from(format!("{id_prefix}-reset")),
                IconName::RotateCw,
            )
            .on_click(move |_, window, cx| on_reset(window, cx)),
        )
}

impl Render for SettingsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui_scale = current_ui_scale(cx);
        let terminal_font_size = current_terminal_font_size(cx);
        let vim_mode = current_vim_mode(cx);

        let font_menu = ContextMenu::build(window, cx, |mut menu, _window, cx| {
            menu = menu.entry("Default (Lilex)", None, |_window, cx| {
                set_font_family(DEFAULT_MONO_FONT.to_string(), cx);
            });
            for family in ::theme::FontFamilyCache::global(cx).list_font_families(cx) {
                // The text system's list includes internal alias names such
                // as ".ZedMono"; the default entry above already covers them.
                if family.starts_with('.') {
                    continue;
                }
                let name = family.to_string();
                menu = menu.entry(family, None, move |_window, cx| {
                    set_font_family(name.clone(), cx);
                });
            }
            menu
        });

        v_flex()
            .key_context("SettingsPage")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .justify_center()
            .items_center()
            .child(
                v_flex()
                    .id("zmux-settings-content")
                    .p_8()
                    .max_w_128()
                    .size_full()
                    .gap_6()
                    .justify_center()
                    .overflow_y_scroll()
                    .child(
                        v_flex().child(Headline::new("Settings")).child(
                            Label::new("Changes apply immediately and are saved to settings.json")
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .italic(),
                        ),
                    )
                    .child(
                        v_flex()
                            .min_w_full()
                            .child(SectionHeader::new("Appearance"))
                            .child(setting_row(
                                "UI scale",
                                "Scales the whole interface, including terminal text",
                                stepper(
                                    "ui-scale",
                                    format!("{:.0}%", ui_scale * 100.0).into(),
                                    move |_, cx| set_ui_scale(ui_scale - UI_SCALE_STEP, cx),
                                    move |_, cx| set_ui_scale(ui_scale + UI_SCALE_STEP, cx),
                                    |_, cx| set_ui_scale(1.0, cx),
                                ),
                            ))
                            .child(setting_row(
                                "Terminal font size",
                                "Current size of terminal and buffer text",
                                stepper(
                                    "terminal-font-size",
                                    format!("{terminal_font_size:.0} px").into(),
                                    move |_, cx| {
                                        set_terminal_font_size(
                                            terminal_font_size - TERMINAL_FONT_SIZE_STEP,
                                            cx,
                                        );
                                    },
                                    move |_, cx| {
                                        set_terminal_font_size(
                                            terminal_font_size + TERMINAL_FONT_SIZE_STEP,
                                            cx,
                                        );
                                    },
                                    |_, cx| set_terminal_font_size(DEFAULT_TERMINAL_FONT_SIZE, cx),
                                ),
                            ))
                            .child(setting_row(
                                "Font family",
                                "Terminal and buffer font",
                                DropdownMenu::new(
                                    "font-family",
                                    current_font_family(cx),
                                    font_menu,
                                ),
                            )),
                    )
                    .child(
                        v_flex()
                            .min_w_full()
                            .child(SectionHeader::new("Editing"))
                            .child(setting_row(
                                "Vim motions",
                                "Use Vim modes and keybindings in diff and text editors",
                                Switch::new(
                                    "vim-mode",
                                    if vim_mode {
                                        ToggleState::Selected
                                    } else {
                                        ToggleState::Unselected
                                    },
                                )
                                .on_click(|state, _, cx| {
                                    set_vim_mode(*state == ToggleState::Selected, cx);
                                }),
                            )),
                    ),
            )
    }
}

impl EventEmitter<ItemEvent> for SettingsPage {}

impl Focusable for SettingsPage {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for SettingsPage {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Settings".into()
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn terminal_size_tracks_transient_zoom_shortcuts(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(::theme::LoadThemes::JustBase, cx);
            editor::EditorSettings::register(cx);
            TerminalSettings::register(cx);
            crate::theme::configure_terminal_fonts(cx);
            crate::keymap::configure_zoom_actions(cx);

            assert_eq!(current_terminal_font_size(cx), 14.0);

            cx.dispatch_action(&zed_actions::IncreaseBufferFontSize { persist: false });
            assert_eq!(current_terminal_font_size(cx), 15.0);

            cx.dispatch_action(&zed_actions::DecreaseBufferFontSize { persist: false });
            assert_eq!(current_terminal_font_size(cx), 14.0);
        });
    }

    #[test]
    fn enabling_vim_mode_disables_helix_mode() {
        let mut content = SettingsContent::default();
        content.helix_mode = Some(true);

        apply_vim_mode(&mut content, true);

        assert_eq!(content.vim_mode, Some(true));
        assert_eq!(content.helix_mode, Some(false));
    }
}
