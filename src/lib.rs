mod app;
mod assets;
mod cli_server;
mod config;
mod control;
mod desktop_notifications;
mod env;
mod ipc;
mod keymap;
mod metadata;
mod notifications;
mod osc;
mod session;
mod settings_editor;
mod theme;
mod welcome;
mod workspaces;

pub use app::{
    JumpToLatestNotification, NotifyCurrentPane, init_zmux, init_zmux_with_config_path_provider,
    init_zmux_with_config_paths, open_zmux_workspace, run,
};
pub use cli_server::{CliNotification, CliServer, NOTIFICATION_ENDPOINT_ENV, NotificationEndpoint};
pub use config::{
    CONFIG_SCHEMA_VERSION, ConfigError, ConfigPathProvider, ConfigPaths, ConfigReload,
    ConfigSource, ConfigStore, ZmuxConfig,
};
pub use control::{
    Acknowledgement, CONTROL_PROTOCOL_VERSION, Capabilities, ControlCommand, ControlError,
    ControlErrorCode, ControlHandler, ControlRequest, ControlResponse, ControlResult,
    MAX_CONTROL_FRAME_BYTES, MAX_REQUEST_TIMEOUT, MAX_SCREEN_TEXT_BYTES, SplitDirection, SurfaceId,
    SurfaceKind, SurfaceSummary, WorkspaceSummary, decode_request, dispatch_frame, encode_response,
};
pub use desktop_notifications::{
    DesktopDelivery, DesktopNotification, DesktopNotificationPolicy, deliver_desktop_notification,
};
pub use env::terminal_env;
pub use keymap::{
    NewTerminal, OpenKeymaps, OpenSettings, ReloadConfig, ResetConfig, configure_keybindings,
    configure_keybindings_with_config, configure_zoom_actions,
};
pub use metadata::{
    AgentActivity, CollectedWorkspaceMetadata, GitMetadata, ListeningPort, LogLevel, MetadataError,
    MetadataRefreshRequest, MetadataState, MetadataUpdate, NotificationSummary, ProgressValue,
    RefreshCancellation, StatusPill, StatusTone, WorkspaceLogEntry, WorkspaceMetadata,
    WorkspaceMetadataStore, collect_system_metadata,
};
pub use notifications::{
    DEFAULT_NOTIFICATION_CAPACITY, Notification, NotificationId, NotificationLevel,
    NotificationSource, NotificationStore, WorkspaceId,
};
pub use osc::{MAX_PENDING_OSC_BYTES, OscNotification, OscNotificationParser, parse_osc_payload};
pub use theme::{configure_terminal_fonts, configure_terminal_fonts_with_config};
pub use workspaces::{NewWorkspace, ToggleWorkspacesPanel, WorkspacesPanel};
