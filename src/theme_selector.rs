//! A theme-only adaptation of Zed's theme selector at revision
//! `abbe85a3321bf6cb7f5b241e623d9c2e16c29187`.
//!
//! Zmux does not initialize Zed's extension or icon-theme systems, so keeping
//! the selector local prevents it from exposing controls that cannot work.

use std::sync::Arc;

use fs::Fs;
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, Focusable, Render, UpdateGlobal, WeakEntity,
    Window,
};
use picker::{Picker, PickerDelegate};
use settings::{Settings, SettingsStore, update_settings_file};
use theme::{Appearance, SystemAppearance, Theme, ThemeMeta, ThemeRegistry};
use theme_settings::{
    ThemeAppearanceMode, ThemeName, ThemeSelection, ThemeSettings, appearance_to_mode,
};
use ui::{ListItem, ListItemSpacing, prelude::*, v_flex};
use util::ResultExt;
use workspace::{ModalView, Workspace, ui::HighlightedLabel, with_active_or_new_workspace};

pub fn init(cx: &mut App) {
    cx.on_action(|action: &zed_actions::theme_selector::Toggle, cx| {
        let action = action.clone();
        with_active_or_new_workspace(cx, move |workspace, window, cx| {
            toggle_theme_selector(workspace, &action, window, cx);
        });
    });
}

fn toggle_theme_selector(
    workspace: &mut Workspace,
    toggle: &zed_actions::theme_selector::Toggle,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let fs = workspace.app_state().fs.clone();
    workspace.toggle_modal(window, cx, |window, cx| {
        let delegate = ThemeSelectorDelegate::new(
            cx.entity().downgrade(),
            fs,
            toggle.themes_filter.as_ref(),
            cx,
        );
        ThemeSelector::new(delegate, window, cx)
    });
}

struct ThemeSelector {
    picker: Entity<Picker<ThemeSelectorDelegate>>,
}

impl ModalView for ThemeSelector {
    fn on_before_dismiss(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> workspace::DismissDecision {
        self.picker.update(cx, |picker, cx| {
            picker.delegate.revert_theme(cx);
        });
        workspace::DismissDecision::Dismiss(true)
    }
}

impl EventEmitter<DismissEvent> for ThemeSelector {}

impl Focusable for ThemeSelector {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for ThemeSelector {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("ThemeSelector")
            .w(rems(34.))
            .child(self.picker.clone())
    }
}

impl ThemeSelector {
    fn new(delegate: ThemeSelectorDelegate, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        Self { picker }
    }
}

struct ThemeSelectorDelegate {
    fs: Arc<dyn Fs>,
    themes: Vec<ThemeMeta>,
    matches: Vec<StringMatch>,
    original_theme_settings: ThemeSettings,
    original_system_appearance: Appearance,
    original_theme_id: Option<usize>,
    new_theme: Arc<Theme>,
    selection_completed: bool,
    selected_theme: Option<Arc<Theme>>,
    selected_index: usize,
    selector: WeakEntity<ThemeSelector>,
}

impl ThemeSelectorDelegate {
    fn new(
        selector: WeakEntity<ThemeSelector>,
        fs: Arc<dyn Fs>,
        themes_filter: Option<&Vec<String>>,
        cx: &mut Context<ThemeSelector>,
    ) -> Self {
        let original_theme = cx.theme().clone();
        let original_theme_settings = ThemeSettings::get_global(cx).clone();
        let original_system_appearance = SystemAppearance::global(cx).0;

        let registry = ThemeRegistry::global(cx);
        let mut themes = registry
            .list()
            .into_iter()
            .filter(|meta| {
                if let Some(theme_filter) = themes_filter {
                    theme_filter.contains(&meta.name.to_string())
                } else {
                    true
                }
            })
            .collect::<Vec<_>>();

        themes.sort_unstable_by(|a, b| {
            a.appearance
                .is_light()
                .cmp(&b.appearance.is_light())
                .then(a.name.cmp(&b.name))
        });

        let original_theme_id = themes
            .iter()
            .position(|meta| meta.name == original_theme.name);

        let matches = themes
            .iter()
            .enumerate()
            .map(|(id, meta)| StringMatch {
                candidate_id: id,
                score: 0.0,
                positions: Default::default(),
                string: meta.name.to_string(),
            })
            .collect::<Vec<_>>();

        let selected_index = matches
            .iter()
            .position(|theme_match| theme_match.string == original_theme.name)
            .unwrap_or(0);

        Self {
            fs,
            themes,
            matches,
            original_theme_settings,
            original_system_appearance,
            original_theme_id,
            new_theme: original_theme,
            selection_completed: false,
            selected_theme: None,
            selected_index,
            selector,
        }
    }

    fn is_original_theme(&self, index: usize) -> bool {
        self.matches
            .get(index)
            .zip(self.original_theme_id)
            .is_some_and(|(theme_match, original_theme_id)| {
                theme_match.candidate_id == original_theme_id
            })
    }

    fn show_selected_theme(
        &mut self,
        cx: &mut Context<Picker<ThemeSelectorDelegate>>,
    ) -> Option<Arc<Theme>> {
        if let Some(theme_match) = self.matches.get(self.selected_index) {
            let registry = ThemeRegistry::global(cx);

            match registry.get(&theme_match.string) {
                Ok(theme) => {
                    self.set_theme(theme.clone(), cx);
                    Some(theme)
                }
                Err(error) => {
                    log::error!("error loading theme {}: {}", theme_match.string, error);
                    None
                }
            }
        } else {
            None
        }
    }

    fn revert_theme(&mut self, cx: &mut App) {
        if !self.selection_completed {
            SettingsStore::update_global(cx, |store, _| {
                store.override_global(self.original_theme_settings.clone());
            });
            self.selection_completed = true;
        }
    }

    fn set_theme(&mut self, new_theme: Arc<Theme>, cx: &mut App) {
        SettingsStore::update_global(cx, |store, _| {
            override_global_theme(
                store,
                &new_theme,
                &self.original_theme_settings.theme,
                self.original_system_appearance,
            )
        });

        self.new_theme = new_theme;
    }
}

fn override_global_theme(
    store: &mut SettingsStore,
    new_theme: &Theme,
    original_theme: &ThemeSelection,
    system_appearance: Appearance,
) {
    let theme_name = ThemeName(new_theme.name.clone().into());
    let new_appearance = new_theme.appearance();
    let new_theme_is_light = new_appearance.is_light();

    let mut current_theme_settings = store.get::<ThemeSettings>(None).clone();

    match (original_theme, &current_theme_settings.theme) {
        (ThemeSelection::Static(_), ThemeSelection::Static(_)) => {
            current_theme_settings.theme = ThemeSelection::Static(theme_name);
        }
        (
            ThemeSelection::Dynamic {
                mode: original_mode,
                light: original_light,
                dark: original_dark,
            },
            ThemeSelection::Dynamic { .. },
        ) => {
            let new_mode = update_mode_if_new_appearance_is_different_from_system(
                original_mode,
                system_appearance,
                new_appearance,
            );

            current_theme_settings.theme = retain_original_opposing_theme(
                new_theme_is_light,
                new_mode,
                theme_name,
                original_light,
                original_dark,
            );
        }
        _ => return,
    }

    store.override_global(current_theme_settings);
}

fn update_mode_if_new_appearance_is_different_from_system(
    original_mode: &ThemeAppearanceMode,
    system_appearance: Appearance,
    new_appearance: Appearance,
) -> ThemeAppearanceMode {
    if original_mode == &ThemeAppearanceMode::System && system_appearance == new_appearance {
        ThemeAppearanceMode::System
    } else {
        appearance_to_mode(new_appearance)
    }
}

fn retain_original_opposing_theme(
    new_theme_is_light: bool,
    new_mode: ThemeAppearanceMode,
    theme_name: ThemeName,
    original_light: &ThemeName,
    original_dark: &ThemeName,
) -> ThemeSelection {
    if new_theme_is_light {
        ThemeSelection::Dynamic {
            mode: new_mode,
            light: theme_name,
            dark: original_dark.clone(),
        }
    } else {
        ThemeSelection::Dynamic {
            mode: new_mode,
            light: original_light.clone(),
            dark: theme_name,
        }
    }
}

impl PickerDelegate for ThemeSelectorDelegate {
    type ListItem = ListItem;

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Select Theme...".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn confirm(
        &mut self,
        _secondary: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<ThemeSelectorDelegate>>,
    ) {
        self.selection_completed = true;

        let theme_name: Arc<str> = self.new_theme.name.as_str().into();
        let theme_appearance = self.new_theme.appearance;
        let system_appearance = SystemAppearance::global(cx).0;

        update_settings_file(self.fs.clone(), cx, move |settings, _| {
            theme_settings::set_theme(settings, theme_name, theme_appearance, system_appearance);
        });

        self.selector.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<ThemeSelectorDelegate>>) {
        self.revert_theme(cx);
        self.selector
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        index: usize,
        _: &mut Window,
        cx: &mut Context<Picker<ThemeSelectorDelegate>>,
    ) {
        self.selected_index = index;
        self.selected_theme = self.show_selected_theme(cx);
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<ThemeSelectorDelegate>>,
    ) -> gpui::Task<()> {
        let background = cx.background_executor().clone();
        let candidates = self
            .themes
            .iter()
            .enumerate()
            .map(|(id, meta)| StringMatchCandidate::new(id, &meta.name))
            .collect::<Vec<_>>();

        cx.spawn_in(window, async move |this, cx| {
            let matches = if query.is_empty() {
                candidates
                    .into_iter()
                    .enumerate()
                    .map(|(index, candidate)| StringMatch {
                        candidate_id: index,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    100,
                    &Default::default(),
                    background,
                )
                .await
            };

            this.update(cx, |this, cx| {
                this.delegate.matches = matches;
                if query.is_empty() && this.delegate.selected_theme.is_none() {
                    this.delegate.selected_index = this
                        .delegate
                        .selected_index
                        .min(this.delegate.matches.len().saturating_sub(1));
                } else if let Some(selected) = this.delegate.selected_theme.as_ref() {
                    this.delegate.selected_index = this
                        .delegate
                        .matches
                        .iter()
                        .enumerate()
                        .find(|(_, theme_match)| theme_match.string == selected.name)
                        .map(|(index, _)| index)
                        .unwrap_or_default();
                } else {
                    this.delegate.selected_index = 0;
                }

                if let Some(theme) = this.delegate.show_selected_theme(cx) {
                    this.delegate.selected_theme = Some(theme);
                }
            })
            .log_err();
        })
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let theme_match = self.matches.get(index)?;
        let is_original_theme = self.is_original_theme(index);

        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(HighlightedLabel::new(
                    theme_match.string.clone(),
                    theme_match.positions.clone(),
                ))
                .when(is_original_theme, |this| {
                    this.end_slot(Icon::new(IconName::Check).color(Color::Muted))
                }),
        )
    }

    fn render_footer(
        &self,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<gpui::AnyElement> {
        Some(
            h_flex()
                .p_2()
                .w_full()
                .justify_between()
                .gap_2()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    Button::new("docs", "View Theme Docs")
                        .end_icon(
                            Icon::new(IconName::ArrowUpRight)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .on_click(cx.listener(|_, _, _, cx| {
                            cx.open_url("https://zed.dev/docs/themes");
                        })),
                )
                .into_any_element(),
        )
    }
}
