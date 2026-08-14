//! The Settings tab: a workspace item exposing zmux's appearance settings.
//! Every control writes through Zed's `SettingsStore::update_settings_file`,
//! so changes land in the user's `settings.json` and apply live via the
//! settings-file watcher installed by `crate::app::load_user_settings`.

use std::path::Path;

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement, ParentElement,
    Render, SharedString, Styled, Window,
};
use settings::{Settings, SettingsContent, SettingsStore, Shell as SettingsShell};
use task::Shell as TaskShell;
use terminal::terminal_settings::TerminalSettings;
use theme_settings::ThemeSettings;
use ui::{
    Button, ButtonStyle, Color, ContextMenu, Divider, DividerColor, DropdownMenu, Headline,
    IconButton, IconName, IntoElement, Label, LabelSize, Switch, ToggleState, h_flex, prelude::*,
    v_flex,
};
use ui_input::InputField;
use vim_mode_setting::VimModeSetting;
use workspace::AppState;
use workspace::item::{Item, ItemEvent};

use crate::shell_settings::{ShellCandidate, detect_shell_candidates, resolve_custom_shell};
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
    shell_candidates: Vec<ShellCandidate>,
    custom_program: Entity<InputField>,
    custom_arguments: Vec<Entity<InputField>>,
    next_argument_id: usize,
    show_custom_shell: bool,
}

impl SettingsPage {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let shell_candidates = detect_shell_candidates();
        let configured_shell = TerminalSettings::get_global(cx).shell.clone();
        let (program, arguments) = custom_shell_parts(&configured_shell, &shell_candidates);
        let show_custom_shell = shell_is_custom(&configured_shell, &shell_candidates);
        let custom_program = new_input("Shell executable or command", program, window, cx);
        let custom_arguments = arguments
            .into_iter()
            .enumerate()
            .map(|(index, argument)| {
                new_input(
                    &format!("Shell argument {}", index + 1),
                    argument,
                    window,
                    cx,
                )
            })
            .collect::<Vec<_>>();
        let next_argument_id = custom_arguments.len() + 1;

        Self {
            focus_handle: cx.focus_handle(),
            shell_candidates,
            custom_program,
            custom_arguments,
            next_argument_id,
            show_custom_shell,
        }
    }

    fn begin_custom_shell(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let configured_shell = TerminalSettings::get_global(cx).shell.clone();
        let (program, arguments) = custom_shell_parts(&configured_shell, &self.shell_candidates);
        self.custom_program.update(cx, |input, cx| {
            input.set_text(&program, window, cx);
            input.set_error(None::<SharedString>, cx);
        });
        self.custom_arguments = arguments
            .into_iter()
            .map(|argument| {
                let id = self.next_argument_id;
                self.next_argument_id += 1;
                new_input(&format!("Shell argument {id}"), argument, window, cx)
            })
            .collect();
        self.show_custom_shell = true;
        cx.notify();
    }

    fn add_custom_argument(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.next_argument_id;
        self.next_argument_id += 1;
        self.custom_arguments.push(new_input(
            &format!("Shell argument {id}"),
            String::new(),
            window,
            cx,
        ));
        cx.notify();
    }

    fn save_custom_shell(&mut self, cx: &mut Context<Self>) {
        let program = self.custom_program.read(cx).text(cx);
        let resolved_program = match resolve_custom_shell(&program) {
            Ok(program) => program.to_string_lossy().into_owned(),
            Err(error) => {
                self.custom_program.update(cx, |input, cx| {
                    input.set_error(Some(error), cx);
                });
                return;
            }
        };
        self.custom_program.update(cx, |input, cx| {
            input.set_error(None::<SharedString>, cx);
        });

        let arguments = self
            .custom_arguments
            .iter()
            .map(|argument| argument.read(cx).text(cx))
            .filter(|argument| !argument.is_empty())
            .collect::<Vec<_>>();
        let title_override = preserved_title_override(&program, &resolved_program, cx);
        let shell = if arguments.is_empty() && title_override.is_none() {
            SettingsShell::Program(resolved_program)
        } else {
            SettingsShell::WithArguments {
                program: resolved_program,
                args: arguments,
                title_override,
            }
        };
        set_default_shell(shell, cx);
        cx.notify();
    }
}

fn new_input(
    placeholder: &str,
    text: String,
    window: &mut Window,
    cx: &mut Context<SettingsPage>,
) -> Entity<InputField> {
    cx.new(|cx| {
        let input = InputField::new(window, cx, placeholder);
        input.set_text(&text, window, cx);
        input
    })
}

fn same_program(left: &str, right: &Path) -> bool {
    let left = Path::new(left);
    if cfg!(target_os = "windows") {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        left == right
    }
}

fn shell_is_custom(shell: &TaskShell, candidates: &[ShellCandidate]) -> bool {
    match shell {
        TaskShell::System => false,
        TaskShell::Program(program) => !candidates
            .iter()
            .any(|candidate| same_program(program, &candidate.program)),
        TaskShell::WithArguments { .. } => true,
    }
}

fn custom_shell_parts(shell: &TaskShell, candidates: &[ShellCandidate]) -> (String, Vec<String>) {
    match shell {
        TaskShell::System => (String::new(), Vec::new()),
        TaskShell::Program(program) => {
            let program = candidates
                .iter()
                .find(|candidate| same_program(program, &candidate.program))
                .map(|candidate| candidate.program.to_string_lossy().into_owned())
                .unwrap_or_else(|| program.clone());
            (program, Vec::new())
        }
        TaskShell::WithArguments { program, args, .. } => (program.clone(), args.clone()),
    }
}

fn shell_label(shell: &TaskShell, candidates: &[ShellCandidate]) -> SharedString {
    match shell {
        TaskShell::System => {
            let system_shell = util::get_system_shell();
            let name = Path::new(&system_shell)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&system_shell);
            format!("System Default ({name})").into()
        }
        TaskShell::Program(program) => candidates
            .iter()
            .find(|candidate| same_program(program, &candidate.program))
            .map(|candidate| candidate.label.into())
            .unwrap_or_else(|| format!("Custom ({program})").into()),
        TaskShell::WithArguments { program, .. } => format!("Custom ({program})").into(),
    }
}

fn set_default_shell(shell: SettingsShell, cx: &mut App) {
    update_settings_file(cx, move |content, _| {
        apply_default_shell(content, shell);
    });
}

fn apply_default_shell(content: &mut SettingsContent, shell: SettingsShell) {
    content.terminal.get_or_insert_default().project.shell = Some(shell);
}

fn preserved_title_override(
    entered_program: &str,
    resolved_program: &str,
    cx: &App,
) -> Option<String> {
    let TaskShell::WithArguments {
        program,
        title_override,
        ..
    } = &TerminalSettings::get_global(cx).shell
    else {
        return None;
    };
    if program == entered_program || same_program(program, Path::new(resolved_program)) {
        title_override.clone()
    } else {
        None
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
        let configured_shell = TerminalSettings::get_global(cx).shell.clone();

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

        let settings_page = cx.weak_entity();
        let shell_candidates = self.shell_candidates.clone();
        let shell_menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            let settings_page_for_system = settings_page.clone();
            menu = menu.entry("System Default", None, move |_window, cx| {
                let _ = settings_page_for_system.update(cx, |this, cx| {
                    this.show_custom_shell = false;
                    set_default_shell(SettingsShell::System, cx);
                    cx.notify();
                });
            });
            for candidate in &shell_candidates {
                let label = candidate.label;
                let program = candidate.program.to_string_lossy().into_owned();
                let settings_page = settings_page.clone();
                menu = menu.entry(label, None, move |_window, cx| {
                    let program = program.clone();
                    let _ = settings_page.update(cx, |this, cx| {
                        this.show_custom_shell = false;
                        set_default_shell(SettingsShell::Program(program), cx);
                        cx.notify();
                    });
                });
            }
            let settings_page_for_custom = settings_page.clone();
            menu.separator()
                .entry("Custom Shell…", None, move |window, cx| {
                    let _ = settings_page_for_custom.update(cx, |this, cx| {
                        this.begin_custom_shell(window, cx);
                    });
                })
        });

        let custom_argument_rows = self
            .custom_arguments
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, argument)| {
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(div().flex_1().child(argument))
                    .child(
                        IconButton::new(("remove-shell-argument", index), IconName::Close)
                            .tooltip(ui::Tooltip::text("Remove argument"))
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                if index < this.custom_arguments.len() {
                                    this.custom_arguments.remove(index);
                                    cx.notify();
                                }
                            })),
                    )
            })
            .collect::<Vec<_>>();

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
                            .child(SectionHeader::new("Terminal"))
                            .child(setting_row(
                                "Default shell",
                                "Used by new local terminals; running terminals are unchanged",
                                DropdownMenu::new(
                                    "default-shell",
                                    shell_label(&configured_shell, &self.shell_candidates),
                                    shell_menu,
                                ),
                            ))
                            .when(self.show_custom_shell, |this| {
                                this.child(
                                    v_flex()
                                        .mt_2()
                                        .p_3()
                                        .gap_3()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(cx.theme().colors().border_variant)
                                        .child(
                                            v_flex()
                                                .gap_1()
                                                .child(Label::new("Executable"))
                                                .child(self.custom_program.clone()),
                                        )
                                        .child(
                                            v_flex()
                                                .gap_1()
                                                .child(Label::new("Arguments"))
                                                .child(
                                                    Label::new(
                                                        "Each field is passed as one exact argument",
                                                    )
                                                    .size(LabelSize::Small)
                                                    .color(Color::Muted),
                                                )
                                                .children(custom_argument_rows)
                                                .child(
                                                    Button::new(
                                                        "add-shell-argument",
                                                        "Add argument",
                                                    )
                                                    .start_icon(ui::Icon::new(IconName::Plus))
                                                    .on_click(cx.listener(
                                                        |this, _, window, cx| {
                                                            this.add_custom_argument(window, cx);
                                                        },
                                                    )),
                                                ),
                                        )
                                        .child(
                                            h_flex().justify_end().child(
                                                Button::new(
                                                    "save-custom-shell",
                                                    "Use Custom Shell",
                                                )
                                                .style(ButtonStyle::Filled)
                                                .on_click(cx.listener(
                                                    |this, _, _window, cx| {
                                                        this.save_custom_shell(cx);
                                                    },
                                                )),
                                            ),
                                        ),
                                )
                            }),
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

    #[test]
    fn enabling_vim_mode_disables_helix_mode() {
        let mut content = SettingsContent {
            helix_mode: Some(true),
            ..Default::default()
        };

        apply_vim_mode(&mut content, true);

        assert_eq!(content.vim_mode, Some(true));
        assert_eq!(content.helix_mode, Some(false));
    }

    #[test]
    fn default_shell_is_written_to_the_terminal_project_settings() {
        let shell = SettingsShell::WithArguments {
            program: "/bin/zsh".to_string(),
            args: vec!["-l".to_string()],
            title_override: None,
        };
        let mut content = SettingsContent::default();

        apply_default_shell(&mut content, shell.clone());

        assert_eq!(
            content
                .terminal
                .expect("terminal settings should be created")
                .project
                .shell,
            Some(shell)
        );
    }
}
