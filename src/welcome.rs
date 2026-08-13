use crate::keymap::OpenSettings;
use crate::NewTerminal;
use gpui::{
    Action, App, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement, ParentElement,
    Render, SharedString, Styled, Window,
};
use ui::{
    h_flex, prelude::*, v_flex, ButtonLike, Color, Divider, DividerColor, Headline, Icon, IconName,
    IconSize, IntoElement, Label, LabelSize, RenderOnce,
};
use workspace::item::{Item, ItemEvent};
use zed_actions::command_palette;

pub struct ZmuxWelcome {
    focus_handle: FocusHandle,
}

impl ZmuxWelcome {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
        }
    }
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

#[derive(IntoElement)]
struct ActionButton {
    id: &'static str,
    label: &'static str,
    icon: IconName,
    action: Box<dyn Action>,
    tab_index: isize,
    focus_handle: FocusHandle,
}

impl ActionButton {
    fn new(
        id: &'static str,
        label: &'static str,
        icon: IconName,
        action: &dyn Action,
        tab_index: isize,
        focus_handle: FocusHandle,
    ) -> Self {
        Self {
            id,
            label,
            icon,
            action: action.boxed_clone(),
            tab_index,
            focus_handle,
        }
    }
}

impl RenderOnce for ActionButton {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        ButtonLike::new(self.id)
            .tab_index(self.tab_index)
            .full_width()
            .size(ButtonSize::Medium)
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        Icon::new(self.icon)
                            .color(Color::Muted)
                            .size(IconSize::Small),
                    )
                    .child(Label::new(self.label)),
            )
            .on_click(move |_, window, cx| {
                self.focus_handle.dispatch_action(&*self.action, window, cx)
            })
    }
}

#[derive(IntoElement)]
struct InertButton {
    id: &'static str,
    label: &'static str,
    icon: IconName,
    tab_index: isize,
}

impl InertButton {
    fn new(id: &'static str, label: &'static str, icon: IconName, tab_index: isize) -> Self {
        Self {
            id,
            label,
            icon,
            tab_index,
        }
    }
}

impl RenderOnce for InertButton {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        ButtonLike::new(self.id)
            .tab_index(self.tab_index)
            .full_width()
            .size(ButtonSize::Medium)
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        Icon::new(self.icon)
                            .color(Color::Muted)
                            .size(IconSize::Small),
                    )
                    .child(Label::new(self.label)),
            )
    }
}

impl Render for ZmuxWelcome {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("Welcome")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .justify_center()
            .items_center()
            .child(
                v_flex()
                    .id("zmux-welcome-content")
                    .p_8()
                    .max_w_128()
                    .size_full()
                    .gap_6()
                    .justify_center()
                    .overflow_y_scroll()
                    .child(
                        h_flex()
                            .w_full()
                            .justify_center()
                            .mb_4()
                            .gap_4()
                            // .child(Vector::square(VectorName::ZedLogo, rems_from_px(45.)))
                            .child(
                                v_flex().child(Headline::new("Welcome to Zmux")).child(
                                    Label::new("Crossplatform terminal workspace")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .italic(),
                                ),
                            ),
                    )
                    .child(
                        v_flex()
                            .min_w_full()
                            .child(SectionHeader::new("Get Started"))
                            .child(ActionButton::new(
                                "create-terminal",
                                "Create Terminal",
                                IconName::Terminal,
                                &NewTerminal,
                                0,
                                self.focus_handle.clone(),
                            ))
                            .child(ActionButton::new(
                                "open-command-palette",
                                "Open Command Palette",
                                IconName::ListCollapse,
                                &command_palette::Toggle,
                                1,
                                self.focus_handle.clone(),
                            )),
                    )
                    .child(
                        v_flex()
                            .min_w_full()
                            .child(SectionHeader::new("Configure"))
                            .child(ActionButton::new(
                                "open-settings",
                                "Open Settings",
                                IconName::Settings,
                                &OpenSettings,
                                2,
                                self.focus_handle.clone(),
                            ))
                            .child(InertButton::new(
                                "customize-keymaps",
                                "Customize Keymaps",
                                IconName::Keyboard,
                                3,
                            )),
                        // .child(InertButton::new(
                        //     "explore-extensions",
                        //     "Explore Extensions",
                        //     IconName::Blocks,
                        //     4,
                        // )),
                    ),
            )
    }
}

impl EventEmitter<ItemEvent> for ZmuxWelcome {}

impl Focusable for ZmuxWelcome {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for ZmuxWelcome {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Welcome".into()
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Zmux Welcome Page Opened")
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}
