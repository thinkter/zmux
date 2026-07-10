mod agent_hooks;
mod app;
mod assets;
mod cli_server;
mod control;
mod desktop_notifications;
mod env;
mod ipc;
mod keymap;
mod notifications;
mod osc;
mod theme;
mod welcome;
mod workspaces;

pub use agent_hooks::{
    AGENT_HOOK_OSC_PREFIX, AGENT_HOOK_PROTOCOL_VERSION, AdapterError, AdapterHookEvent,
    AdapterPlan, AdapterSettings, AgentAdapter, AgentHookEvent, AgentHookKind, AgentHookRouter,
    AgentRole, DEFAULT_AGENT_HOOK_AUDIT_CAPACITY, HookAuditRecord, HookDelivery, HookFilter,
    HookOrigin, HookParseError, HookParseErrorCode, MAX_AGENT_HOOK_BODY_BYTES,
    MAX_AGENT_HOOK_FRAME_BYTES, MAX_AGENT_HOOK_TITLE_BYTES, MAX_AGENT_NAME_BYTES,
    MAX_PUBLIC_SESSION_ID_BYTES, NativeResumeCommand, ResumeError, RoutedHookEvent,
    TrustedResumeRecord, encode_osc_hook_payload, parse_hook_rpc_frame, parse_osc_hook_payload,
};
pub use app::{JumpToLatestNotification, NotifyCurrentPane, init_zmux, open_zmux_workspace, run};
pub use cli_server::{CliNotification, CliServer, NOTIFICATION_ENDPOINT_ENV, NotificationEndpoint};
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
pub use keymap::{NewTerminal, configure_keybindings, configure_zoom_actions};
pub use notifications::{
    DEFAULT_NOTIFICATION_CAPACITY, Notification, NotificationId, NotificationLevel,
    NotificationSource, NotificationStore, WorkspaceId,
};
pub use osc::{MAX_PENDING_OSC_BYTES, OscNotification, OscNotificationParser, parse_osc_payload};
pub use theme::configure_terminal_fonts;
pub use workspaces::{NewWorkspace, ToggleWorkspacesPanel, WorkspacesPanel};
