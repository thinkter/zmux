//! Versioned, transport-independent control-plane protocol.
//!
//! The IPC transport is deliberately kept out of this module. A Unix socket,
//! Windows named pipe, or in-process test harness can all hand a bounded JSON
//! frame to [`dispatch_frame`] and get the same typed response back.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{
    metadata::MetadataUpdate,
    notifications::{NotificationLevel, NotificationSource, WorkspaceId},
};

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
    /// A bounded, workspace-addressed status/progress/log update. The protocol
    /// defines the request shape; a UI-side [`ControlHandler`] decides whether
    /// the local runtime currently supports applying it.
    WorkspaceMetadataUpdate {
        workspace_id: WorkspaceId,
        update: MetadataUpdate,
    },
    SurfaceList {
        workspace_id: WorkspaceId,
    },
    SurfaceCreateTerminal {
        workspace_id: WorkspaceId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        working_directory: Option<String>,
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

impl ControlCommand {
    fn validate(&self) -> Result<(), ControlError> {
        if let Self::WorkspaceMetadataUpdate { update, .. } = self {
            update.validate().map_err(|error| {
                ControlError::new(
                    ControlErrorCode::InvalidRequest,
                    format!("invalid workspace metadata update: {error}"),
                )
            })?;
        }
        Ok(())
    }
}

fn default_screen_text_limit() -> usize {
    16 * 1024
}

fn default_notification_limit() -> usize {
    100
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    Right,
    Down,
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
    pub browser_surfaces: bool,
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
    use crate::metadata::{LogLevel, ProgressValue};

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
    fn metadata_updates_are_workspace_addressed_and_round_trip() {
        let request = ControlRequest {
            version: CONTROL_PROTOCOL_VERSION,
            id: 13,
            timeout_ms: None,
            command: ControlCommand::WorkspaceMetadataUpdate {
                workspace_id: 7,
                update: MetadataUpdate::AppendLog {
                    level: LogLevel::Info,
                    message: "build started".to_string(),
                },
            },
        };

        let encoded = serde_json::to_vec(&request).unwrap();
        assert_eq!(decode_request(&encoded).unwrap(), request);
        assert!(
            String::from_utf8(encoded)
                .unwrap()
                .contains("workspace_metadata_update")
        );
    }

    #[test]
    fn malformed_metadata_updates_fail_before_a_handler_receives_them() {
        let request = ControlRequest {
            version: CONTROL_PROTOCOL_VERSION,
            id: 14,
            timeout_ms: None,
            command: ControlCommand::WorkspaceMetadataUpdate {
                workspace_id: 7,
                update: MetadataUpdate::SetProgress {
                    key: "build".to_string(),
                    progress: ProgressValue {
                        label: "Build".to_string(),
                        completed: 1,
                        total: 0,
                    },
                },
            },
        };

        let encoded = serde_json::to_vec(&request).unwrap();
        let error = decode_request(&encoded).unwrap_err();
        assert_eq!(error.code, ControlErrorCode::InvalidRequest);
        assert!(!error.retryable);
    }
}
