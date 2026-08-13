mod agent_detection;
mod app;
mod app_icon;
mod assets;
mod bootstrap;
mod cli_server;
mod desktop_notifications;
mod env;
mod fonts;
mod keymap;
mod launcher;
mod metadata;
mod notification_runtime;
mod notifications;
mod osc;
mod session;
mod settings_page;
mod shell_settings;
mod syntax;
mod theme;
mod theme_selector;
mod visual_power;
mod welcome;
mod workspace_switcher;
mod workspaces;

pub use app::{
    JumpToLatestNotification, NotifyCurrentPane, init_zmux, load_user_settings,
    open_zmux_workspace, open_zmux_workspace_at, run,
};
pub use assets::Assets;
pub use cli_server::{
    CliEndpoint, CliNotification, CliRequestCompletion, CliRouteId, CliRouteRegistration,
    CliServer, NOTIFY_ENDPOINT_ENV, ReceivedCliNotification,
};
pub use env::{terminal_env, terminal_env_with_notification_endpoint};
pub use fonts::load_embedded_fonts;
pub use keymap::{
    NewTerminal, OpenSettings, SplitTerminalDown, SplitTerminalRight, configure_keybindings,
    configure_zoom_actions,
};
pub use launcher::run_gui;
pub use notifications::{
    Notification, NotificationId, NotificationLevel, NotificationRequest, NotificationSequence,
    NotificationSource, NotificationStore, NotificationTarget, WorkspaceId,
};
pub use osc::{
    KittyActivation, KittyDeliveryCondition, KittyNotificationMetadata, KittyUrgency,
    OscNotification, OscNotificationEvent, OscNotificationParser, OscParseError,
    bridged_osc_payload, parse_osc_payload, try_parse_osc_payload,
};
pub use settings_page::SettingsPage;
pub use syntax::register_builtin_languages;
pub use theme::{
    DEFAULT_MONO_FONT, DEFAULT_THEME, configure_terminal_fonts, default_settings_json,
};
pub use workspaces::{NewWorkspace, ToggleWorkspacesPanel, WorkspacesPanel};
