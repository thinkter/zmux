//! Versioned, transport-independent control-plane protocol.
//!
//! The IPC transport is deliberately kept out of this module. A Unix socket,
//! Windows named pipe, or in-process test harness can all hand a bounded JSON
//! frame to [`dispatch_frame`] and get the same typed response back.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::notifications::{NotificationLevel, NotificationSource, WorkspaceId};

/// The only protocol version this build understands.
pub const CONTROL_PROTOCOL_VERSION: u16 = 1;
/// Keep a malformed client from making the UI process allocate an unbounded
/// buffer before it has had a chance to reject the request.
pub const MAX_CONTROL_FRAME_BYTES: usize = 64 * 1024;
/// A caller may ask for less time, but never more than this bounded deadline.
pub const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Screen snapshots are intentionally bounded even when a transport supports
/// reading terminal content.
pub const MAX_SCREEN_TEXT_BYTES: usize = 64 * 1024;
/// Browser URLs are accepted as opaque strings because the backend is the
/// authority on supported schemes, but an automation client cannot make the
/// UI retain an unbounded URL.
pub const MAX_BROWSER_URL_BYTES: usize = 8 * 1024;
/// JavaScript is deliberately a bounded payload even when the browser backend
/// offers an evaluator that accepts arbitrary source text.
pub const MAX_BROWSER_SCRIPT_BYTES: usize = 64 * 1024;
/// A snapshot can contain many nodes. This is the hard ceiling across all
/// browser backends; callers may request a lower value.
pub const MAX_BROWSER_SNAPSHOT_NODES: usize = 10_000;
/// The serialized textual content in a browser result must remain bounded.
pub const MAX_BROWSER_RESULT_BYTES: usize = 512 * 1024;
/// Browser screenshots have a stricter payload limit than arbitrary terminal
/// images so local control clients cannot exhaust the UI process.
pub const MAX_BROWSER_SCREENSHOT_BYTES: usize = 4 * 1024 * 1024;
/// Console output and cookies are both untrusted browser-originated data.
pub const MAX_BROWSER_CONSOLE_ENTRIES: usize = 1_000;
pub const MAX_BROWSER_COOKIES: usize = 1_000;
pub const MAX_BROWSER_ORIGINS: usize = 1_000;

/// Stable surface identity. It is intentionally not a GPUI entity id: those
/// are implementation details that can be recycled after a surface is closed.
pub type SurfaceId = u64;

/// A complete request frame. `command` is flattened so the wire format is
/// concise, for example `{"version":1,"id":7,"method":"discover"}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlRequest {
    pub version: u16,
    pub id: u64,
    /// Requested deadline in milliseconds. It is clamped by
    /// [`ControlRequest::timeout`], never trusted verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u32>,
    #[serde(flatten)]
    pub command: ControlCommand,
}

impl ControlRequest {
    pub fn timeout(&self) -> Duration {
        self.timeout_ms
            .map(|milliseconds| Duration::from_millis(u64::from(milliseconds)))
            .unwrap_or(MAX_REQUEST_TIMEOUT)
            .min(MAX_REQUEST_TIMEOUT)
    }
}

/// All state-changing commands require an explicit target. That avoids a CLI
/// client accidentally targeting whichever pane happens to be focused when the
/// request reaches the application.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum ControlCommand {
    Discover,
    WorkspaceList,
    WorkspaceCreate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    WorkspaceSelect {
        workspace_id: WorkspaceId,
    },
    WorkspaceRename {
        workspace_id: WorkspaceId,
        name: String,
    },
    WorkspaceClose {
        workspace_id: WorkspaceId,
    },
    SurfaceList {
        workspace_id: WorkspaceId,
    },
    SurfaceCreateTerminal {
        workspace_id: WorkspaceId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        working_directory: Option<String>,
    },
    /// Browser creation is routed through an optional browser backend. A
    /// terminal-only build retains this wire type so clients can discover the
    /// absence of the capability instead of having to special-case protocol
    /// versions.
    SurfaceCreateBrowser {
        workspace_id: WorkspaceId,
        #[serde(default)]
        options: BrowserSurfaceOptions,
    },
    SurfaceFocus {
        workspace_id: WorkspaceId,
        surface_id: SurfaceId,
    },
    SurfaceSplit {
        workspace_id: WorkspaceId,
        surface_id: SurfaceId,
        direction: SplitDirection,
    },
    SurfaceClose {
        workspace_id: WorkspaceId,
        surface_id: SurfaceId,
    },
    SurfaceReorder {
        workspace_id: WorkspaceId,
        surface_id: SurfaceId,
        index: usize,
    },
    SurfaceSendInput {
        workspace_id: WorkspaceId,
        surface_id: SurfaceId,
        input: String,
    },
    SurfaceReadScreen {
        workspace_id: WorkspaceId,
        surface_id: SurfaceId,
        #[serde(default = "default_screen_text_limit")]
        max_bytes: usize,
    },
    SurfaceScreenshot {
        workspace_id: WorkspaceId,
        surface_id: SurfaceId,
    },
    /// Return the browser's policy and current route. Policies are surfaced so
    /// a control client never has to infer whether cookies, storage, grants, or
    /// downloads were enabled implicitly.
    BrowserGetInfo {
        workspace_id: WorkspaceId,
        surface_id: SurfaceId,
    },
    BrowserNavigate {
        workspace_id: WorkspaceId,
        surface_id: SurfaceId,
        url: String,
    },
    BrowserAccessibilitySnapshot {
        workspace_id: WorkspaceId,
        surface_id: SurfaceId,
        #[serde(default = "default_browser_snapshot_node_limit")]
        max_nodes: usize,
    },
    BrowserInteract {
        workspace_id: WorkspaceId,
        surface_id: SurfaceId,
        target: BrowserTarget,
        action: BrowserDomAction,
    },
    BrowserEvaluateJavaScript {
        workspace_id: WorkspaceId,
        surface_id: SurfaceId,
        script: String,
    },
    BrowserConsoleList {
        workspace_id: WorkspaceId,
        surface_id: SurfaceId,
        #[serde(default = "default_browser_console_limit")]
        limit: usize,
    },
    BrowserCookieList {
        workspace_id: WorkspaceId,
        surface_id: SurfaceId,
        #[serde(default = "default_browser_cookie_limit")]
        limit: usize,
    },
    BrowserStorageState {
        workspace_id: WorkspaceId,
        surface_id: SurfaceId,
        #[serde(default = "default_browser_origin_limit")]
        max_origins: usize,
    },
    BrowserDownload {
        workspace_id: WorkspaceId,
        surface_id: SurfaceId,
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        suggested_filename: Option<String>,
    },
    NotificationList {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_id: Option<WorkspaceId>,
        #[serde(default = "default_notification_limit")]
        limit: usize,
    },
    NotificationCreate {
        workspace_id: WorkspaceId,
        surface_id: SurfaceId,
        source: NotificationSource,
        #[serde(default)]
        level: NotificationLevel,
        title: String,
        body: String,
    },
    NotificationAcknowledge {
        notification_id: u64,
    },
    NotificationClear {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_id: Option<WorkspaceId>,
    },
}

fn default_screen_text_limit() -> usize {
    16 * 1024
}

fn default_notification_limit() -> usize {
    100
}

fn default_browser_snapshot_node_limit() -> usize {
    1_000
}

fn default_browser_console_limit() -> usize {
    100
}

fn default_browser_cookie_limit() -> usize {
    100
}

fn default_browser_origin_limit() -> usize {
    100
}

impl ControlCommand {
    /// Perform protocol-level bounds checks before a command reaches an
    /// optional browser backend. Native adapters must retain their own origin
    /// and sandbox checks because those are host-specific.
    pub fn validate(&self) -> Result<(), ControlError> {
        let result = match self {
            Self::SurfaceCreateBrowser { options, .. } => options.validate(),
            Self::BrowserNavigate { url, .. } => validate_browser_url(url),
            Self::BrowserAccessibilitySnapshot { max_nodes, .. } => validate_limit(
                "browser snapshot node limit",
                *max_nodes,
                MAX_BROWSER_SNAPSHOT_NODES,
            ),
            Self::BrowserInteract { target, action, .. } => {
                target.validate()?;
                action.validate()
            }
            Self::BrowserEvaluateJavaScript { script, .. } => {
                validate_bounded("JavaScript source", script, MAX_BROWSER_SCRIPT_BYTES)
            }
            Self::BrowserConsoleList { limit, .. } => {
                validate_limit("browser console limit", *limit, MAX_BROWSER_CONSOLE_ENTRIES)
            }
            Self::BrowserCookieList { limit, .. } => {
                validate_limit("browser cookie limit", *limit, MAX_BROWSER_COOKIES)
            }
            Self::BrowserStorageState { max_origins, .. } => {
                validate_limit("browser origin limit", *max_origins, MAX_BROWSER_ORIGINS)
            }
            Self::BrowserDownload {
                url,
                suggested_filename,
                ..
            } => {
                validate_browser_url(url)?;
                match suggested_filename {
                    Some(filename) => validate_bounded("suggested filename", filename, 255),
                    None => Ok(()),
                }
            }
            _ => Ok(()),
        };
        result.map_err(Into::into)
    }

    /// Lets a composite UI control handler delegate only browser commands to
    /// the feature-gated browser registry, leaving terminal routing untouched.
    pub fn is_browser_command(&self) -> bool {
        matches!(
            self,
            Self::SurfaceCreateBrowser { .. }
                | Self::BrowserGetInfo { .. }
                | Self::BrowserNavigate { .. }
                | Self::BrowserAccessibilitySnapshot { .. }
                | Self::BrowserInteract { .. }
                | Self::BrowserEvaluateJavaScript { .. }
                | Self::BrowserConsoleList { .. }
                | Self::BrowserCookieList { .. }
                | Self::BrowserStorageState { .. }
                | Self::BrowserDownload { .. }
        )
    }
}

fn validate_limit(name: &str, value: usize, maximum: usize) -> Result<(), BrowserAutomationError> {
    if value > maximum {
        return Err(BrowserAutomationError::new(
            BrowserAutomationErrorCode::LimitExceeded,
            format!("{name} exceeds the {maximum}-item limit"),
            false,
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    Right,
    Down,
}

/// Policy and backend selection supplied when a browser surface is created.
/// Nothing is persisted or granted by default: the default is an ephemeral
/// session, denied permissions, and denied downloads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserSurfaceOptions {
    #[serde(default)]
    pub backend: BrowserBackendPreference,
    #[serde(default = "default_browser_url")]
    pub initial_url: String,
    #[serde(default)]
    pub session: BrowserSessionPolicy,
    #[serde(default)]
    pub permissions: BrowserPermissionPolicy,
    #[serde(default)]
    pub downloads: BrowserDownloadPolicy,
}

impl Default for BrowserSurfaceOptions {
    fn default() -> Self {
        Self {
            backend: BrowserBackendPreference::Auto,
            initial_url: default_browser_url(),
            session: BrowserSessionPolicy::Ephemeral,
            permissions: BrowserPermissionPolicy::default(),
            downloads: BrowserDownloadPolicy::Deny,
        }
    }
}

impl BrowserSurfaceOptions {
    /// Validate data that must be bounded before a backend gets to inspect it.
    /// Scheme, certificate, and origin validation remain backend-specific.
    pub fn validate(&self) -> Result<(), BrowserAutomationError> {
        validate_browser_url(&self.initial_url)?;
        if let BrowserSessionPolicy::Persistent { storage_path } = &self.session {
            validate_non_empty_bounded(
                "persistent storage path",
                storage_path,
                MAX_BROWSER_URL_BYTES,
            )?;
        }
        if let BrowserDownloadPolicy::AllowTo { directory } = &self.downloads {
            validate_non_empty_bounded("download directory", directory, MAX_BROWSER_URL_BYTES)?;
        }
        if self.permissions.grants.len() > 64 {
            return Err(BrowserAutomationError::new(
                BrowserAutomationErrorCode::LimitExceeded,
                "browser permission policy has more than 64 explicit grants",
                false,
            ));
        }
        for grant in &self.permissions.grants {
            if let Some(origin) = &grant.origin {
                validate_non_empty_bounded("permission origin", origin, MAX_BROWSER_URL_BYTES)?;
            }
        }
        Ok(())
    }
}

fn default_browser_url() -> String {
    "about:blank".to_string()
}

/// Selects the implementation seam. `Auto` only selects a backend advertised
/// as available; it never silently falls back to an unsupported host adapter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserBackendPreference {
    #[default]
    Auto,
    Mock,
    WkWebView,
    WebView2,
    WebKitGtk,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrowserSessionPolicy {
    #[default]
    Ephemeral,
    /// Persistent data must name an explicit host-approved location. Browser
    /// backends must not substitute an implicit profile directory.
    Persistent { storage_path: String },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserPermissionPolicy {
    #[serde(default)]
    pub default: BrowserPermissionDecision,
    #[serde(default)]
    pub grants: Vec<BrowserPermissionGrant>,
}

impl BrowserPermissionPolicy {
    /// Resolve an explicit decision without granting anything implicitly.
    /// An origin-specific rule takes precedence over a surface-wide rule, and
    /// the policy default is deny when neither applies.
    pub fn decision_for(
        &self,
        permission: BrowserPermission,
        origin: Option<&str>,
    ) -> BrowserPermissionDecision {
        self.grants
            .iter()
            .rev()
            .find(|grant| grant.permission == permission && grant.origin.as_deref() == origin)
            .or_else(|| {
                self.grants
                    .iter()
                    .rev()
                    .find(|grant| grant.permission == permission && grant.origin.is_none())
            })
            .map(|grant| grant.decision)
            .unwrap_or(self.default)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserPermissionGrant {
    pub permission: BrowserPermission,
    pub decision: BrowserPermissionDecision,
    /// Restrict a grant to one origin when supplied. An omitted origin applies
    /// to the surface, but should still be avoided for sensitive permissions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserPermission {
    ClipboardRead,
    ClipboardWrite,
    Camera,
    Microphone,
    Geolocation,
    Notifications,
    Popups,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserPermissionDecision {
    #[default]
    Deny,
    Allow,
    Prompt,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrowserDownloadPolicy {
    #[default]
    Deny,
    /// The host is responsible for sandboxing and canonicalizing this path
    /// before a native webview is allowed to create a file there.
    AllowTo { directory: String },
}

/// An opaque identity emitted by an accessibility snapshot. It is stable only
/// for `document_id`; a navigation invalidates it and must produce a typed
/// stale-target error rather than clicking a newly-matched element.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserNodeId {
    pub document_id: String,
    pub node_id: String,
}

/// Browser interaction targeting. Every option pins the expected document so
/// a delayed automation command cannot accidentally target a different page.
/// Snapshot nodes are the preferred route; accessibility and CSS targeting are
/// bounded fallbacks for callers that need to discover a node first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrowserTarget {
    SnapshotNode {
        node: BrowserNodeId,
    },
    Accessibility {
        document_id: String,
        role: String,
        name: String,
        #[serde(default)]
        index: usize,
    },
    Css {
        document_id: String,
        selector: String,
    },
}

impl BrowserTarget {
    pub fn validate(&self) -> Result<(), BrowserAutomationError> {
        match self {
            Self::SnapshotNode { node } => {
                validate_non_empty_bounded("browser document id", &node.document_id, 512)?;
                validate_non_empty_bounded("browser node id", &node.node_id, 512)
            }
            Self::Accessibility {
                document_id,
                role,
                name,
                ..
            } => {
                validate_non_empty_bounded("browser document id", document_id, 512)?;
                validate_non_empty_bounded("accessibility role", role, 256)?;
                validate_non_empty_bounded("accessibility name", name, 4 * 1024)
            }
            Self::Css {
                document_id,
                selector,
            } => {
                validate_non_empty_bounded("browser document id", document_id, 512)?;
                validate_non_empty_bounded("CSS selector", selector, 8 * 1024)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrowserDomAction {
    Click,
    Focus,
    Fill { text: String },
    SelectOption { value: String },
    PressKey { key: String },
}

impl BrowserDomAction {
    pub fn validate(&self) -> Result<(), BrowserAutomationError> {
        match self {
            Self::Fill { text } => validate_bounded("form value", text, MAX_BROWSER_RESULT_BYTES),
            Self::SelectOption { value } => validate_bounded("select value", value, 8 * 1024),
            Self::PressKey { key } => validate_non_empty_bounded("key", key, 128),
            Self::Click | Self::Focus => Ok(()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserAccessibilitySnapshot {
    pub document_id: String,
    pub url: String,
    pub title: String,
    pub nodes: Vec<BrowserAccessibilityNode>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserAccessibilityNode {
    pub id: BrowserNodeId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<BrowserNodeId>,
    pub role: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserNavigationResult {
    pub url: String,
    pub document_id: String,
    pub title: String,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserInteractionResult {
    pub target: BrowserNodeId,
    pub action: BrowserDomAction,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserJavaScriptResult {
    /// JSON-encoded primitive/object data supplied by the backend. Large or
    /// unserializable values must be truncated or rejected by that backend.
    pub value_json: String,
    pub truncated: bool,
}

/// Internal browser-backend representation of a bounded image. The control
/// result keeps its historical inline shape, while the backend seam can use a
/// named type without depending on terminal rendering code.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserScreenshot {
    pub mime_type: String,
    pub data_base64: String,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserConsoleLevel {
    Debug,
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserConsoleEntry {
    pub sequence: u64,
    pub level: BrowserConsoleLevel,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserConsoleResult {
    pub entries: Vec<BrowserConsoleEntry>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub http_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_site: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_unix_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserCookiesResult {
    pub cookies: Vec<BrowserCookie>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserStorageEntry {
    pub key: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserOriginStorage {
    pub origin: String,
    #[serde(default)]
    pub local_storage: Vec<BrowserStorageEntry>,
    #[serde(default)]
    pub session_storage: Vec<BrowserStorageEntry>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserStorageState {
    #[serde(default)]
    pub cookies: Vec<BrowserCookie>,
    #[serde(default)]
    pub origins: Vec<BrowserOriginStorage>,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserDownloadState {
    Accepted,
    Blocked,
    Completed,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserDownloadResult {
    pub url: String,
    pub filename: String,
    pub state: BrowserDownloadState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
}

/// Detailed browser discovery result. `available` is false unless a host has
/// registered an operational backend; feature compilation alone is never a
/// claim that WKWebView, WebView2, or WebKitGTK is usable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserCapabilities {
    pub available: bool,
    pub backends: Vec<BrowserBackendCapability>,
    pub navigation: bool,
    pub accessibility_snapshot: bool,
    pub dom_interaction: bool,
    pub javascript: bool,
    pub screenshots: bool,
    pub console: bool,
    pub cookies: bool,
    pub storage: bool,
    pub downloads: bool,
    pub max_timeout_ms: u32,
    pub max_snapshot_nodes: usize,
    pub max_result_bytes: usize,
}

impl Default for BrowserCapabilities {
    fn default() -> Self {
        Self {
            available: false,
            backends: Vec::new(),
            navigation: false,
            accessibility_snapshot: false,
            dom_interaction: false,
            javascript: false,
            screenshots: false,
            console: false,
            cookies: false,
            storage: false,
            downloads: false,
            max_timeout_ms: MAX_REQUEST_TIMEOUT.as_millis() as u32,
            max_snapshot_nodes: MAX_BROWSER_SNAPSHOT_NODES,
            max_result_bytes: MAX_BROWSER_RESULT_BYTES,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserBackendCapability {
    pub backend: BrowserBackendKind,
    pub status: BrowserBackendStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserBackendKind {
    Mock,
    WkWebView,
    WebView2,
    WebKitGtk,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserBackendStatus {
    Available,
    UnsupportedHost,
    NotCompiled,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserAutomationErrorCode {
    BackendUnavailable,
    Timeout,
    InvalidTarget,
    TargetNotFound,
    StaleTarget,
    PermissionDenied,
    DownloadDenied,
    NavigationFailed,
    JavaScriptFailed,
    LimitExceeded,
}

/// Errors returned by a browser backend before they are mapped to the generic
/// control-plane error envelope. Keeping this typed lets embedding hosts report
/// timeouts and stale targets precisely without parsing text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserAutomationError {
    pub code: BrowserAutomationErrorCode,
    pub message: String,
    pub retryable: bool,
}

impl BrowserAutomationError {
    pub fn new(
        code: BrowserAutomationErrorCode,
        message: impl Into<String>,
        retryable: bool,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
        }
    }
}

fn validate_browser_url(value: &str) -> Result<(), BrowserAutomationError> {
    validate_non_empty_bounded("browser URL", value, MAX_BROWSER_URL_BYTES)
}

fn validate_non_empty_bounded(
    name: &str,
    value: &str,
    max_bytes: usize,
) -> Result<(), BrowserAutomationError> {
    if value.is_empty() {
        return Err(BrowserAutomationError::new(
            BrowserAutomationErrorCode::InvalidTarget,
            format!("{name} must not be empty"),
            false,
        ));
    }
    validate_bounded(name, value, max_bytes)
}

fn validate_bounded(
    name: &str,
    value: &str,
    max_bytes: usize,
) -> Result<(), BrowserAutomationError> {
    if value.len() > max_bytes {
        return Err(BrowserAutomationError::new(
            BrowserAutomationErrorCode::LimitExceeded,
            format!("{name} exceeds the {max_bytes}-byte limit"),
            false,
        ));
    }
    Ok(())
}

/// Successful or failed response. All errors are encoded as data so clients do
/// not need to scrape stderr or infer failure from a disconnected transport.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ControlResponse {
    Ok {
        version: u16,
        id: u64,
        result: ControlResult,
    },
    Error {
        version: u16,
        id: u64,
        error: ControlError,
    },
}

impl ControlResponse {
    pub fn ok(id: u64, result: ControlResult) -> Self {
        Self::Ok {
            version: CONTROL_PROTOCOL_VERSION,
            id,
            result,
        }
    }

    pub fn error(id: u64, error: ControlError) -> Self {
        Self::Error {
            version: CONTROL_PROTOCOL_VERSION,
            id,
            error,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ControlResult {
    Capabilities(Capabilities),
    Workspaces(Vec<WorkspaceSummary>),
    Surfaces(Vec<SurfaceSummary>),
    BrowserSurface(BrowserSurfaceInfo),
    BrowserNavigation(BrowserNavigationResult),
    BrowserAccessibilitySnapshot(BrowserAccessibilitySnapshot),
    BrowserInteraction(BrowserInteractionResult),
    BrowserJavaScript(BrowserJavaScriptResult),
    BrowserConsole(BrowserConsoleResult),
    BrowserCookies(BrowserCookiesResult),
    BrowserStorage(BrowserStorageState),
    BrowserDownload(BrowserDownloadResult),
    Notifications(Vec<NotificationSummary>),
    ScreenText {
        text: String,
        truncated: bool,
    },
    Screenshot {
        mime_type: String,
        /// Base64-encoded image data. A capability advertises whether this is
        /// supported; a transport may also return a location instead later.
        data_base64: String,
        /// `true` when the backend clipped the image to the advertised control
        /// payload limit. A backend may instead reject an oversized image.
        #[serde(default)]
        truncated: bool,
    },
    Ack(Acknowledgement),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub protocol_version: u16,
    pub workspaces: bool,
    pub terminals: bool,
    pub notifications: bool,
    pub screen_text: bool,
    pub screenshots: bool,
    /// Kept as a quick boolean for older clients. New clients should inspect
    /// [`Capabilities::browser`] to learn which backend, if any, is actually
    /// usable on this host.
    pub browser_surfaces: bool,
    #[serde(default)]
    pub browser: BrowserCapabilities,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            workspaces: true,
            terminals: true,
            notifications: true,
            screen_text: false,
            screenshots: false,
            browser_surfaces: false,
            browser: BrowserCapabilities::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSummary {
    pub id: WorkspaceId,
    pub name: String,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceSummary {
    pub id: SurfaceId,
    pub workspace_id: WorkspaceId,
    pub kind: SurfaceKind,
    pub active: bool,
    pub title: String,
}

/// Browser-specific surface information. The contained [`SurfaceSummary`]
/// carries the same workspace/surface identity used by split, focus, and
/// routing commands, while this structure makes the explicit browser policy
/// observable to a control client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserSurfaceInfo {
    pub surface: SurfaceSummary,
    pub backend: BrowserBackendKind,
    pub url: String,
    pub policy: BrowserSurfaceOptions,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceKind {
    Terminal,
    Browser,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationSummary {
    pub id: u64,
    pub workspace_id: Option<WorkspaceId>,
    pub surface_id: Option<SurfaceId>,
    pub source: NotificationSource,
    pub level: NotificationLevel,
    pub title: String,
    pub body: String,
    pub read: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Acknowledgement {
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface_id: Option<SurfaceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_id: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlError {
    pub code: ControlErrorCode,
    pub message: String,
    pub retryable: bool,
}

impl ControlError {
    pub fn new(code: ControlErrorCode, message: impl Into<String>) -> Self {
        let retryable = matches!(
            code,
            ControlErrorCode::Timeout | ControlErrorCode::Overloaded
        );
        Self {
            code,
            message: message.into(),
            retryable,
        }
    }
}

impl From<BrowserAutomationError> for ControlError {
    fn from(error: BrowserAutomationError) -> Self {
        let code = match error.code {
            BrowserAutomationErrorCode::BackendUnavailable => ControlErrorCode::NotSupported,
            BrowserAutomationErrorCode::Timeout => ControlErrorCode::Timeout,
            BrowserAutomationErrorCode::InvalidTarget => ControlErrorCode::InvalidRequest,
            BrowserAutomationErrorCode::TargetNotFound => ControlErrorCode::NotFound,
            BrowserAutomationErrorCode::StaleTarget => ControlErrorCode::StaleTarget,
            BrowserAutomationErrorCode::PermissionDenied => ControlErrorCode::PermissionDenied,
            BrowserAutomationErrorCode::DownloadDenied => ControlErrorCode::DownloadDenied,
            BrowserAutomationErrorCode::NavigationFailed => ControlErrorCode::NavigationFailed,
            BrowserAutomationErrorCode::JavaScriptFailed => ControlErrorCode::JavaScriptFailed,
            BrowserAutomationErrorCode::LimitExceeded => ControlErrorCode::Overloaded,
        };
        Self {
            code,
            message: error.message,
            retryable: error.retryable,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlErrorCode {
    UnsupportedVersion,
    InvalidRequest,
    Unauthorized,
    NotFound,
    NotSupported,
    Timeout,
    Overloaded,
    StaleTarget,
    PermissionDenied,
    DownloadDenied,
    NavigationFailed,
    JavaScriptFailed,
    Internal,
}

/// Implemented by the UI-side control plane. A transport never receives a
/// `Workspace` or GPUI entity directly, keeping it portable and testable.
pub trait ControlHandler {
    fn handle(
        &mut self,
        command: ControlCommand,
        timeout: Duration,
    ) -> Result<ControlResult, ControlError>;
}

/// Parse one UTF-8 JSON request. Frames are bounded before deserialization so
/// callers can safely expose this to child processes on a local IPC endpoint.
pub fn decode_request(frame: &[u8]) -> Result<ControlRequest, ControlError> {
    if frame.len() > MAX_CONTROL_FRAME_BYTES {
        return Err(ControlError::new(
            ControlErrorCode::Overloaded,
            format!("control frame exceeds {MAX_CONTROL_FRAME_BYTES} bytes"),
        ));
    }

    let request: ControlRequest = serde_json::from_slice(frame).map_err(|error| {
        ControlError::new(
            ControlErrorCode::InvalidRequest,
            format!("invalid control request: {error}"),
        )
    })?;

    if request.version != CONTROL_PROTOCOL_VERSION {
        return Err(ControlError::new(
            ControlErrorCode::UnsupportedVersion,
            format!(
                "unsupported control protocol version {}; expected {CONTROL_PROTOCOL_VERSION}",
                request.version
            ),
        ));
    }

    request.command.validate()?;

    Ok(request)
}

/// Serialize a response as one JSON frame followed by a newline. The newline
/// makes the protocol convenient to inspect with shell tools while transports
/// remain free to use connection-per-frame or newline-delimited streams.
pub fn encode_response(response: &ControlResponse) -> Result<Vec<u8>, ControlError> {
    let mut frame = serde_json::to_vec(response).map_err(|error| {
        ControlError::new(
            ControlErrorCode::Internal,
            format!("failed to encode control response: {error}"),
        )
    })?;
    frame.push(b'\n');
    Ok(frame)
}

/// Decode and dispatch one frame. Decode errors use request id zero because no
/// trustworthy request id exists until parsing succeeds.
pub fn dispatch_frame(handler: &mut impl ControlHandler, frame: &[u8]) -> ControlResponse {
    match decode_request(frame) {
        Ok(request) => {
            let id = request.id;
            let timeout = request.timeout();
            match handler.handle(request.command, timeout) {
                Ok(result) => ControlResponse::ok(id, result),
                Err(error) => ControlResponse::error(id, error),
            }
        }
        Err(error) => ControlResponse::error(0, error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DiscoverOnly;

    impl ControlHandler for DiscoverOnly {
        fn handle(
            &mut self,
            command: ControlCommand,
            _timeout: Duration,
        ) -> Result<ControlResult, ControlError> {
            match command {
                ControlCommand::Discover => {
                    Ok(ControlResult::Capabilities(Capabilities::default()))
                }
                _ => Err(ControlError::new(
                    ControlErrorCode::NotSupported,
                    "test handler supports only discover",
                )),
            }
        }
    }

    #[test]
    fn discover_round_trips_with_a_typed_acknowledgement() {
        let request = ControlRequest {
            version: CONTROL_PROTOCOL_VERSION,
            id: 42,
            timeout_ms: Some(5_000),
            command: ControlCommand::Discover,
        };
        let bytes = serde_json::to_vec(&request).unwrap();
        let response = dispatch_frame(&mut DiscoverOnly, &bytes);

        assert_eq!(
            response,
            ControlResponse::ok(42, ControlResult::Capabilities(Capabilities::default()))
        );
        let frame = encode_response(&response).unwrap();
        assert!(frame.ends_with(b"\n"));
        assert_eq!(
            serde_json::from_slice::<ControlResponse>(&frame).unwrap(),
            response
        );
    }

    #[test]
    fn mutations_carry_an_explicit_workspace_and_surface_target() {
        let request = ControlRequest {
            version: CONTROL_PROTOCOL_VERSION,
            id: 7,
            timeout_ms: None,
            command: ControlCommand::SurfaceSendInput {
                workspace_id: 12,
                surface_id: 99,
                input: "echo hello\n".to_string(),
            },
        };

        let encoded = serde_json::to_string(&request).unwrap();
        assert!(encoded.contains("\"workspace_id\":12"));
        assert!(encoded.contains("\"surface_id\":99"));
        assert_eq!(decode_request(encoded.as_bytes()).unwrap(), request);
    }

    #[test]
    fn unsupported_versions_are_typed_errors() {
        let frame = br#"{"version":99,"id":8,"method":"discover"}"#;
        let response = dispatch_frame(&mut DiscoverOnly, frame);
        assert_eq!(
            response,
            ControlResponse::error(
                0,
                ControlError::new(
                    ControlErrorCode::UnsupportedVersion,
                    "unsupported control protocol version 99; expected 1"
                )
            )
        );
    }

    #[test]
    fn oversized_frames_are_rejected_before_deserialization() {
        let frame = vec![b'x'; MAX_CONTROL_FRAME_BYTES + 1];
        let error = decode_request(&frame).unwrap_err();
        assert_eq!(error.code, ControlErrorCode::Overloaded);
        assert!(error.retryable);
    }

    #[test]
    fn request_deadline_is_clamped() {
        let request = ControlRequest {
            version: CONTROL_PROTOCOL_VERSION,
            id: 1,
            timeout_ms: Some(u32::MAX),
            command: ControlCommand::Discover,
        };
        assert_eq!(request.timeout(), MAX_REQUEST_TIMEOUT);
    }

    #[test]
    fn browser_payloads_are_bounded_before_a_backend_is_called() {
        let error = ControlCommand::BrowserEvaluateJavaScript {
            workspace_id: 1,
            surface_id: 2,
            script: "x".repeat(MAX_BROWSER_SCRIPT_BYTES + 1),
        }
        .validate()
        .unwrap_err();

        assert_eq!(error.code, ControlErrorCode::Overloaded);
        assert!(!error.retryable);
    }

    #[test]
    fn browser_permission_policy_defaults_to_deny_and_honors_origin_rules() {
        let policy = BrowserPermissionPolicy {
            default: BrowserPermissionDecision::Deny,
            grants: vec![
                BrowserPermissionGrant {
                    permission: BrowserPermission::ClipboardRead,
                    decision: BrowserPermissionDecision::Allow,
                    origin: None,
                },
                BrowserPermissionGrant {
                    permission: BrowserPermission::ClipboardRead,
                    decision: BrowserPermissionDecision::Deny,
                    origin: Some("https://untrusted.example".to_string()),
                },
            ],
        };

        assert_eq!(
            policy.decision_for(
                BrowserPermission::ClipboardRead,
                Some("https://trusted.example")
            ),
            BrowserPermissionDecision::Allow
        );
        assert_eq!(
            policy.decision_for(
                BrowserPermission::ClipboardRead,
                Some("https://untrusted.example")
            ),
            BrowserPermissionDecision::Deny
        );
        assert_eq!(
            policy.decision_for(BrowserPermission::Camera, Some("https://trusted.example")),
            BrowserPermissionDecision::Deny
        );
    }
}
