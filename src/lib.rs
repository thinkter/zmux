mod app;
mod assets;
#[cfg(feature = "browser")]
pub mod browser;
mod cli_server;
pub mod control;
mod desktop_notifications;
mod env;
mod ipc;
mod keymap;
mod notifications;
mod osc;
mod theme;
mod welcome;
mod workspaces;

pub use app::{JumpToLatestNotification, NotifyCurrentPane, init_zmux, open_zmux_workspace, run};
#[cfg(feature = "browser")]
pub use browser::{
    BrowserBackend, BrowserBackendFactory, BrowserSurface, BrowserSurfaceRegistry,
    BrowserSurfaceRoute, MockAccessibilityNode, MockBrowserBackend, MockBrowserBackendFactory,
    MockBrowserFixture, WebKitGtkBackendFactory, WebView2BackendFactory, WkWebViewBackendFactory,
    platform_backend_capability,
};
pub use cli_server::{CliNotification, CliServer, NOTIFICATION_ENDPOINT_ENV, NotificationEndpoint};
pub use control::{
    Acknowledgement, BrowserAccessibilityNode, BrowserAccessibilitySnapshot,
    BrowserAutomationError, BrowserAutomationErrorCode, BrowserBackendCapability,
    BrowserBackendKind, BrowserBackendPreference, BrowserBackendStatus, BrowserCapabilities,
    BrowserConsoleEntry, BrowserConsoleLevel, BrowserConsoleResult, BrowserCookie,
    BrowserCookiesResult, BrowserDomAction, BrowserDownloadPolicy, BrowserDownloadResult,
    BrowserDownloadState, BrowserInteractionResult, BrowserJavaScriptResult,
    BrowserNavigationResult, BrowserNodeId, BrowserOriginStorage, BrowserPermission,
    BrowserPermissionDecision, BrowserPermissionGrant, BrowserPermissionPolicy, BrowserScreenshot,
    BrowserSessionPolicy, BrowserStorageEntry, BrowserStorageState, BrowserSurfaceInfo,
    BrowserSurfaceOptions, BrowserTarget, CONTROL_PROTOCOL_VERSION, Capabilities, ControlCommand,
    ControlError, ControlErrorCode, ControlHandler, ControlRequest, ControlResponse, ControlResult,
    MAX_BROWSER_CONSOLE_ENTRIES, MAX_BROWSER_COOKIES, MAX_BROWSER_ORIGINS,
    MAX_BROWSER_RESULT_BYTES, MAX_BROWSER_SCREENSHOT_BYTES, MAX_BROWSER_SCRIPT_BYTES,
    MAX_BROWSER_SNAPSHOT_NODES, MAX_BROWSER_URL_BYTES, MAX_CONTROL_FRAME_BYTES,
    MAX_REQUEST_TIMEOUT, MAX_SCREEN_TEXT_BYTES, SplitDirection, SurfaceId, SurfaceKind,
    SurfaceSummary, WorkspaceSummary, decode_request, dispatch_frame, encode_response,
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
