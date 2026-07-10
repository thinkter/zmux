//! Safe, vendor-neutral terminal-agent hooks.
//!
//! Hooks are deliberately a thin layer over the versioned control API. A hook
//! must name the workspace and terminal surface that emitted it, so it never
//! lands in whichever pane happened to be focused when an external program
//! delivered the event. The module contains no listener and does not write a
//! vendor configuration file; an IPC transport owns delivery and a caller must
//! explicitly opt an adapter in.

use std::{
    collections::VecDeque,
    error::Error,
    fmt::{self, Display, Formatter},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    control::{CONTROL_PROTOCOL_VERSION, ControlCommand, ControlRequest, SurfaceId},
    notifications::{NotificationLevel, NotificationSource, WorkspaceId},
};

/// The protocol version understood by the generic hook parser.
pub const AGENT_HOOK_PROTOCOL_VERSION: u16 = 1;
/// Hook JSON is intentionally much smaller than a control frame. Hooks can be
/// emitted by untrusted terminal children, so retaining large payloads is not
/// useful or safe.
pub const MAX_AGENT_HOOK_FRAME_BYTES: usize = 8 * 1024;
/// A human-readable notification heading should stay compact.
pub const MAX_AGENT_HOOK_TITLE_BYTES: usize = 512;
/// Keep a malicious or broken agent from creating oversized notification and
/// audit-log entries.
pub const MAX_AGENT_HOOK_BODY_BYTES: usize = 4 * 1024;
/// Agent names are presentation metadata, not executable program names.
pub const MAX_AGENT_NAME_BYTES: usize = 128;
/// A resume ID is passed as one argument, never interpolated into a shell.
pub const MAX_PUBLIC_SESSION_ID_BYTES: usize = 256;
/// The in-memory audit trail is bounded independently from normal
/// notifications. Filtered subagent events remain inspectable here.
pub const DEFAULT_AGENT_HOOK_AUDIT_CAPACITY: usize = 500;
/// OSC payload prefix for the generic hook contract.
///
/// The complete terminal sequence is `ESC ]` + this prefix + one-line JSON +
/// `BEL` (or `ESC \`). The terminal integration should pass the bytes after
/// `ESC ]` to [`parse_osc_hook_payload`].
pub const AGENT_HOOK_OSC_PREFIX: &str = "777;zmux;hook;";

/// A concrete, stable target for a hook event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookOrigin {
    pub workspace_id: WorkspaceId,
    pub surface_id: SurfaceId,
}

impl HookOrigin {
    fn validate(self) -> Result<(), HookParseError> {
        if self.workspace_id == 0 || self.surface_id == 0 {
            return Err(HookParseError::invalid_event(
                "hook origin must contain non-zero workspace_id and surface_id",
            ));
        }
        Ok(())
    }
}

/// A generic event requested by an agent-aware terminal program.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentHookKind {
    PermissionRequest,
    TaskComplete,
    Idle,
    Waiting,
    Error,
}

impl AgentHookKind {
    fn notification_level(self) -> NotificationLevel {
        match self {
            Self::PermissionRequest => NotificationLevel::Warning,
            Self::TaskComplete => NotificationLevel::Success,
            Self::Idle | Self::Waiting => NotificationLevel::Info,
            Self::Error => NotificationLevel::Error,
        }
    }

    fn fallback_title(self) -> &'static str {
        match self {
            Self::PermissionRequest => "permission requested",
            Self::TaskComplete => "task completed",
            Self::Idle => "idle",
            Self::Waiting => "waiting",
            Self::Error => "reported an error",
        }
    }
}

/// Whether the event came from the main agent or a delegated teammate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    #[default]
    Primary,
    Subagent,
    Teammate,
}

/// The documented generic hook/RPC payload.
///
/// `agent` is an opaque display label for generic integrations. Vendor
/// adapters set it themselves instead of trusting a caller-provided label.
/// `public_session_id`, when present, is deliberately the only session data
/// accepted by the resume path. Do not put transcripts, prompts, filesystem
/// paths, environment values, or command lines in this frame.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentHookEvent {
    pub version: u16,
    pub origin: HookOrigin,
    pub kind: AgentHookKind,
    pub agent: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub role: AgentRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_session_id: Option<String>,
}

impl AgentHookEvent {
    /// Validate the portions that serde cannot express declaratively.
    pub fn validate(&self) -> Result<(), HookParseError> {
        if self.version != AGENT_HOOK_PROTOCOL_VERSION {
            return Err(HookParseError::new(
                HookParseErrorCode::UnsupportedVersion,
                format!(
                    "unsupported agent hook protocol version {}; expected {AGENT_HOOK_PROTOCOL_VERSION}",
                    self.version
                ),
            ));
        }
        self.origin.validate()?;
        validate_display_text("agent", &self.agent, MAX_AGENT_NAME_BYTES, true)?;
        validate_display_text("title", &self.title, MAX_AGENT_HOOK_TITLE_BYTES, false)?;
        validate_display_text("body", &self.body, MAX_AGENT_HOOK_BODY_BYTES, false)?;
        if let Some(session_id) = &self.public_session_id {
            validate_public_session_id(session_id)?;
        }
        Ok(())
    }

    fn notification_title(&self) -> String {
        let title = if self.title.is_empty() {
            self.kind.fallback_title()
        } else {
            &self.title
        };
        format!("{}: {title}", self.agent)
    }
}

/// Why an event could not be parsed or safely accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookParseErrorCode {
    TooLarge,
    InvalidFrame,
    InvalidJson,
    UnsupportedVersion,
    InvalidEvent,
}

/// A compact, typed parser error suitable for IPC implementations to map onto
/// their normal control-plane error response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookParseError {
    pub code: HookParseErrorCode,
    pub message: String,
}

impl HookParseError {
    fn new(code: HookParseErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn invalid_event(message: impl Into<String>) -> Self {
        Self::new(HookParseErrorCode::InvalidEvent, message)
    }
}

impl Display for HookParseError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for HookParseError {}

/// Parse one bounded hook/RPC JSON frame.
///
/// The parser refuses literal terminal control bytes. Newlines and other
/// formatting must therefore be represented with JSON escapes; this prevents
/// an OSC frame from escaping its terminal envelope or smuggling another
/// terminal control sequence into the receiver.
pub fn parse_hook_rpc_frame(frame: &[u8]) -> Result<AgentHookEvent, HookParseError> {
    let event: AgentHookEvent = parse_bounded_json(frame)?;
    event.validate()?;
    Ok(event)
}

/// Parse a hook payload after the terminal's `ESC ]` introducer.
///
/// Non-zmux OSC payloads return `Ok(None)` so an OSC dispatcher can hand the
/// same payload to other supported parsers. A payload that claims the zmux
/// prefix but is malformed returns a typed error and is never routed.
pub fn parse_osc_hook_payload(payload: &[u8]) -> Result<Option<AgentHookEvent>, HookParseError> {
    let prefix = AGENT_HOOK_OSC_PREFIX.as_bytes();
    if !payload.starts_with(prefix) {
        return Ok(None);
    }
    if payload.len() > prefix.len() + MAX_AGENT_HOOK_FRAME_BYTES {
        return Err(HookParseError::new(
            HookParseErrorCode::TooLarge,
            format!(
                "agent hook OSC payload exceeds {} bytes",
                prefix.len() + MAX_AGENT_HOOK_FRAME_BYTES
            ),
        ));
    }
    parse_hook_rpc_frame(&payload[prefix.len()..]).map(Some)
}

/// Serialize a validated generic event into the payload portion of an OSC
/// notification. The caller chooses the terminal terminator (BEL or ST).
pub fn encode_osc_hook_payload(event: &AgentHookEvent) -> Result<Vec<u8>, HookParseError> {
    event.validate()?;
    let frame = serde_json::to_vec(event).map_err(|error| {
        HookParseError::new(
            HookParseErrorCode::InvalidJson,
            format!("failed to encode agent hook: {error}"),
        )
    })?;
    if frame.len() > MAX_AGENT_HOOK_FRAME_BYTES {
        return Err(HookParseError::new(
            HookParseErrorCode::TooLarge,
            format!("agent hook frame exceeds {MAX_AGENT_HOOK_FRAME_BYTES} bytes"),
        ));
    }

    let mut payload = Vec::with_capacity(AGENT_HOOK_OSC_PREFIX.len() + frame.len());
    payload.extend_from_slice(AGENT_HOOK_OSC_PREFIX.as_bytes());
    payload.extend_from_slice(&frame);
    Ok(payload)
}

fn parse_bounded_json<T: DeserializeOwned>(frame: &[u8]) -> Result<T, HookParseError> {
    if frame.len() > MAX_AGENT_HOOK_FRAME_BYTES {
        return Err(HookParseError::new(
            HookParseErrorCode::TooLarge,
            format!("agent hook frame exceeds {MAX_AGENT_HOOK_FRAME_BYTES} bytes"),
        ));
    }
    if frame.iter().any(|byte| byte.is_ascii_control()) {
        return Err(HookParseError::new(
            HookParseErrorCode::InvalidFrame,
            "agent hook frame contains a literal control byte",
        ));
    }

    serde_json::from_slice(frame).map_err(|error| {
        HookParseError::new(
            HookParseErrorCode::InvalidJson,
            format!("invalid agent hook JSON: {error}"),
        )
    })
}

fn validate_display_text(
    field: &str,
    value: &str,
    limit: usize,
    required: bool,
) -> Result<(), HookParseError> {
    if required && value.is_empty() {
        return Err(HookParseError::invalid_event(format!(
            "agent hook {field} must not be empty"
        )));
    }
    if value.len() > limit {
        return Err(HookParseError::invalid_event(format!(
            "agent hook {field} exceeds {limit} bytes"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(HookParseError::invalid_event(format!(
            "agent hook {field} contains a control character"
        )));
    }
    Ok(())
}

fn validate_public_session_id(session_id: &str) -> Result<(), HookParseError> {
    if session_id.is_empty() || session_id.len() > MAX_PUBLIC_SESSION_ID_BYTES {
        return Err(HookParseError::invalid_event(format!(
            "public_session_id must be between 1 and {MAX_PUBLIC_SESSION_ID_BYTES} bytes"
        )));
    }
    if session_id.starts_with('-')
        || !session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
    {
        return Err(HookParseError::invalid_event(
            "public_session_id contains unsafe characters",
        ));
    }
    Ok(())
}

/// The notification policy for delegated agents. A filtered event still gets
/// an audit entry; only visual delivery is suppressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HookFilter {
    pub show_subagents: bool,
    pub show_teammates: bool,
}

impl Default for HookFilter {
    fn default() -> Self {
        Self {
            show_subagents: true,
            show_teammates: true,
        }
    }
}

impl HookFilter {
    fn allows(self, role: AgentRole) -> bool {
        match role {
            AgentRole::Primary => true,
            AgentRole::Subagent => self.show_subagents,
            AgentRole::Teammate => self.show_teammates,
        }
    }
}

/// How the router handled a parsed event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookDelivery {
    Delivered,
    FilteredByRole,
}

/// A bounded in-memory record. The original public session ID is intentionally
/// excluded: resume records own that small, explicitly persisted value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookAuditRecord {
    pub id: u64,
    pub origin: HookOrigin,
    pub kind: AgentHookKind,
    pub agent: String,
    pub role: AgentRole,
    pub title: String,
    pub body: String,
    pub delivery: HookDelivery,
}

/// The result of routing one hook event. `command` is intentionally optional:
/// an intentionally filtered event must remain visible in the audit trail but
/// must not create a user notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutedHookEvent {
    pub audit_id: u64,
    pub delivery: HookDelivery,
    pub command: Option<ControlCommand>,
}

impl RoutedHookEvent {
    /// Convert a delivered hook to the existing control API request shape.
    /// The caller owns the request ID and the transport; the hook layer never
    /// guesses a target from focus state.
    pub fn into_control_request(self, id: u64) -> Option<ControlRequest> {
        self.command.map(|command| ControlRequest {
            version: CONTROL_PROTOCOL_VERSION,
            id,
            timeout_ms: None,
            command,
        })
    }
}

/// Routes validated events to control commands while preserving a bounded
/// audit log for all events, including those filtered from notifications.
#[derive(Debug)]
pub struct AgentHookRouter {
    filter: HookFilter,
    audit: VecDeque<HookAuditRecord>,
    audit_capacity: usize,
    next_audit_id: u64,
}

impl Default for AgentHookRouter {
    fn default() -> Self {
        Self::new(HookFilter::default())
    }
}

impl AgentHookRouter {
    pub fn new(filter: HookFilter) -> Self {
        Self::with_audit_capacity(filter, DEFAULT_AGENT_HOOK_AUDIT_CAPACITY)
    }

    pub fn with_audit_capacity(filter: HookFilter, audit_capacity: usize) -> Self {
        Self {
            filter,
            audit: VecDeque::new(),
            audit_capacity: audit_capacity.max(1),
            next_audit_id: 1,
        }
    }

    pub fn filter(&self) -> HookFilter {
        self.filter
    }

    pub fn set_filter(&mut self, filter: HookFilter) {
        self.filter = filter;
    }

    pub fn audit_records(&self) -> impl DoubleEndedIterator<Item = &HookAuditRecord> {
        self.audit.iter()
    }

    pub fn route(&mut self, event: AgentHookEvent) -> Result<RoutedHookEvent, HookParseError> {
        event.validate()?;
        let delivery = if self.filter.allows(event.role) {
            HookDelivery::Delivered
        } else {
            HookDelivery::FilteredByRole
        };
        let audit_id = self.next_audit_id;
        self.next_audit_id = self.next_audit_id.saturating_add(1);

        let command =
            (delivery == HookDelivery::Delivered).then(|| ControlCommand::NotificationCreate {
                workspace_id: event.origin.workspace_id,
                surface_id: event.origin.surface_id,
                source: NotificationSource::AgentHook,
                level: event.kind.notification_level(),
                title: event.notification_title(),
                body: event.body.clone(),
            });

        self.audit.push_back(HookAuditRecord {
            id: audit_id,
            origin: event.origin,
            kind: event.kind,
            agent: event.agent,
            role: event.role,
            title: event.title,
            body: event.body,
            delivery,
        });
        while self.audit.len() > self.audit_capacity {
            self.audit.pop_front();
        }

        Ok(RoutedHookEvent {
            audit_id,
            delivery,
            command,
        })
    }
}

/// Known opt-in adapter identifiers. Adapters only normalize an incoming hook
/// frame and build a safe resume command; they do not install themselves or
/// modify any user-owned vendor configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentAdapter {
    Codex,
    #[serde(rename = "claude-code", alias = "claude_code")]
    ClaudeCode,
    #[serde(rename = "opencode", alias = "open_code")]
    OpenCode,
    Gemini,
}

impl AgentAdapter {
    pub const ALL: [Self; 4] = [Self::Codex, Self::ClaudeCode, Self::OpenCode, Self::Gemini];

    pub fn id(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude-code",
            Self::OpenCode => "opencode",
            Self::Gemini => "gemini",
        }
    }

    /// A human-readable plan that a UI can show before the user edits their
    /// own agent configuration. Calling this method has no side effects.
    pub fn opt_in_plan(self) -> AdapterPlan {
        AdapterPlan {
            adapter: self,
            enable_message: "Configure this CLI to emit the documented zmux hook JSON only after reviewing the change.",
            disable_message: "Remove the user-added hook from that CLI's configuration; zmux does not retain or rewrite it.",
        }
    }

    /// Parse the vendor adapter's shared wire shape. The adapter supplies the
    /// agent label itself, preventing a hook producer from impersonating a
    /// different known adapter in the notification UI.
    pub fn parse_opt_in_hook(
        self,
        settings: AdapterSettings,
        frame: &[u8],
    ) -> Result<AdapterHookEvent, AdapterError> {
        if !settings.enabled {
            return Err(AdapterError::Disabled(self));
        }

        let frame: AdapterHookFrame = parse_bounded_json(frame).map_err(AdapterError::Parse)?;
        let event = AgentHookEvent {
            version: frame.version,
            origin: frame.origin,
            kind: frame.kind,
            agent: self.id().to_string(),
            title: frame.title,
            body: frame.body,
            role: frame.role,
            public_session_id: frame.public_session_id,
        };
        event.validate().map_err(AdapterError::Parse)?;
        Ok(AdapterHookEvent {
            adapter: self,
            event,
        })
    }

    fn native_resume_command(self, public_session_id: String) -> Option<NativeResumeCommand> {
        let (program, args) = match self {
            // `codex resume [SESSION_ID]` is intentionally the first tested
            // adapter. The ID is a separate argv item, never a shell fragment.
            Self::Codex => ("codex", vec!["resume".to_string(), public_session_id]),
            // These forms are kept as explicit argv values as well. Gemini is
            // hook-capable but does not receive a native resume command until
            // its CLI resume contract is pinned by an integration test.
            Self::ClaudeCode => ("claude", vec!["--resume".to_string(), public_session_id]),
            Self::OpenCode => ("opencode", vec!["--session".to_string(), public_session_id]),
            Self::Gemini => return None,
        };
        Some(NativeResumeCommand { program, args })
    }
}

/// A side-effect-free description of how a user can enable and later remove an
/// adapter from their own configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdapterPlan {
    pub adapter: AgentAdapter,
    pub enable_message: &'static str,
    pub disable_message: &'static str,
}

/// In-memory opt-in state. It defaults to disabled and intentionally has no
/// file I/O API; settings persistence is owned by the zmux configuration layer
/// and must only be performed after explicit user consent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AdapterSettings {
    pub enabled: bool,
    pub resume_enabled: bool,
}

/// An event normalized by one of the known adapters. Its fields stay private
/// so a generic hook producer cannot forge the provenance required to create a
/// trusted resume record. Call [`Self::into_event`] to route it like any other
/// hook after the adapter opt-in check has passed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterHookEvent {
    adapter: AgentAdapter,
    event: AgentHookEvent,
}

impl AdapterHookEvent {
    pub fn adapter(&self) -> AgentAdapter {
        self.adapter
    }

    pub fn event(&self) -> &AgentHookEvent {
        &self.event
    }

    pub fn into_event(self) -> AgentHookEvent {
        self.event
    }
}

/// The common adapter JSON shape. It is the generic event minus `agent`, which
/// each adapter owns rather than accepts from its child process.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AdapterHookFrame {
    version: u16,
    origin: HookOrigin,
    kind: AgentHookKind,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    role: AgentRole,
    #[serde(default)]
    public_session_id: Option<String>,
}

/// A validated session reference. Its serialized form contains exactly a
/// schema version, adapter identifier, and public session ID—no transcript,
/// cwd, command, environment, workspace, or surface information.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedResumeRecord {
    pub version: u16,
    pub adapter: AgentAdapter,
    pub public_session_id: String,
}

impl TrustedResumeRecord {
    /// Build a record only after the adapter and resume behavior have both
    /// been explicitly enabled. The event must have been normalized by that
    /// adapter, not merely claim its display name.
    pub fn from_adapter_event(
        settings: AdapterSettings,
        event: &AdapterHookEvent,
    ) -> Result<Self, ResumeError> {
        let adapter = event.adapter;
        if !settings.enabled {
            return Err(ResumeError::AdapterDisabled(adapter));
        }
        if !settings.resume_enabled {
            return Err(ResumeError::ResumeDisabled(adapter));
        }
        let public_session_id = event
            .event
            .public_session_id
            .clone()
            .ok_or(ResumeError::MissingSessionId)?;
        validate_public_session_id(&public_session_id).map_err(ResumeError::InvalidSessionId)?;
        if adapter
            .native_resume_command(public_session_id.clone())
            .is_none()
        {
            return Err(ResumeError::UnsupportedAdapter(adapter));
        }

        Ok(Self {
            version: AGENT_HOOK_PROTOCOL_VERSION,
            adapter,
            public_session_id,
        })
    }

    pub fn validate(&self) -> Result<(), ResumeError> {
        if self.version != AGENT_HOOK_PROTOCOL_VERSION {
            return Err(ResumeError::UnsupportedVersion(self.version));
        }
        validate_public_session_id(&self.public_session_id)
            .map_err(ResumeError::InvalidSessionId)?;
        if self
            .adapter
            .native_resume_command(self.public_session_id.clone())
            .is_none()
        {
            return Err(ResumeError::UnsupportedAdapter(self.adapter));
        }
        Ok(())
    }

    /// Encode the deliberately minimal on-disk representation. The caller
    /// decides whether and where to persist it after user consent.
    pub fn encode(&self) -> Result<Vec<u8>, ResumeError> {
        self.validate()?;
        serde_json::to_vec(self).map_err(|error| ResumeError::Encode(error.to_string()))
    }

    /// Load a small record before constructing a native argv command. Unknown
    /// fields are rejected rather than silently becoming persistence baggage.
    pub fn decode(frame: &[u8]) -> Result<Self, ResumeError> {
        if frame.len() > MAX_AGENT_HOOK_FRAME_BYTES {
            return Err(ResumeError::RecordTooLarge);
        }
        let record = serde_json::from_slice::<Self>(frame)
            .map_err(|error| ResumeError::Decode(error.to_string()))?;
        record.validate()?;
        Ok(record)
    }

    /// Return a native executable and argv vector. This does not spawn a
    /// process, invoke a shell, or mutate a vendor configuration.
    pub fn native_command(&self) -> Result<NativeResumeCommand, ResumeError> {
        self.validate()?;
        self.adapter
            .native_resume_command(self.public_session_id.clone())
            .ok_or(ResumeError::UnsupportedAdapter(self.adapter))
    }
}

/// A process invocation represented without shell interpolation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeResumeCommand {
    pub program: &'static str,
    pub args: Vec<String>,
}

/// Errors while deciding whether a saved public session may be resumed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResumeError {
    AdapterDisabled(AgentAdapter),
    ResumeDisabled(AgentAdapter),
    MissingSessionId,
    InvalidSessionId(HookParseError),
    UnsupportedAdapter(AgentAdapter),
    UnsupportedVersion(u16),
    RecordTooLarge,
    Decode(String),
    Encode(String),
}

impl Display for ResumeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::AdapterDisabled(adapter) => {
                write!(formatter, "{} adapter is not enabled", adapter.id())
            }
            Self::ResumeDisabled(adapter) => {
                write!(formatter, "{} resume is not enabled", adapter.id())
            }
            Self::MissingSessionId => formatter.write_str("hook event has no public_session_id"),
            Self::InvalidSessionId(error) => {
                write!(formatter, "invalid public_session_id: {error}")
            }
            Self::UnsupportedAdapter(adapter) => {
                write!(
                    formatter,
                    "{} has no pinned native resume command",
                    adapter.id()
                )
            }
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported resume record version {version}")
            }
            Self::RecordTooLarge => formatter.write_str("resume record is too large"),
            Self::Decode(error) => write!(formatter, "invalid resume record: {error}"),
            Self::Encode(error) => write!(formatter, "failed to encode resume record: {error}"),
        }
    }
}

impl Error for ResumeError {}

/// An error specific to adapter opt-in and parsing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdapterError {
    Disabled(AgentAdapter),
    Parse(HookParseError),
}

impl Display for AdapterError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled(adapter) => write!(formatter, "{} adapter is not enabled", adapter.id()),
            Self::Parse(error) => Display::fmt(error, formatter),
        }
    }
}

impl Error for AdapterError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: AgentHookKind) -> AgentHookEvent {
        AgentHookEvent {
            version: AGENT_HOOK_PROTOCOL_VERSION,
            origin: HookOrigin {
                workspace_id: 12,
                surface_id: 99,
            },
            kind,
            agent: "build-agent".to_string(),
            title: "needs attention".to_string(),
            body: "please review the generated patch".to_string(),
            role: AgentRole::Primary,
            public_session_id: None,
        }
    }

    #[test]
    fn rpc_and_osc_parsers_round_trip_a_bounded_explicit_origin() {
        let original = event(AgentHookKind::PermissionRequest);
        let rpc = serde_json::to_vec(&original).unwrap();
        assert_eq!(parse_hook_rpc_frame(&rpc).unwrap(), original);

        let payload = encode_osc_hook_payload(&original).unwrap();
        assert_eq!(parse_osc_hook_payload(&payload).unwrap(), Some(original));
        assert_eq!(
            parse_osc_hook_payload(b"9;ordinary notification").unwrap(),
            None
        );
    }

    #[test]
    fn parser_rejects_unknown_fields_and_terminal_control_bytes() {
        let unknown = br#"{"version":1,"origin":{"workspace_id":12,"surface_id":99},"kind":"waiting","agent":"codex","unexpected":true}"#;
        assert_eq!(
            parse_hook_rpc_frame(unknown).unwrap_err().code,
            HookParseErrorCode::InvalidJson
        );

        let control = b"{\"version\":1,\"origin\":{\"workspace_id\":12,\"surface_id\":99},\"kind\":\"waiting\",\"agent\":\"codex\"}\x1b";
        assert_eq!(
            parse_hook_rpc_frame(control).unwrap_err().code,
            HookParseErrorCode::InvalidFrame
        );
    }

    #[test]
    fn router_preserves_origin_and_maps_semantic_levels_to_control_api() {
        let mut router = AgentHookRouter::default();
        let routed = router
            .route(event(AgentHookKind::PermissionRequest))
            .unwrap();
        assert_eq!(routed.delivery, HookDelivery::Delivered);

        assert_eq!(
            routed.into_control_request(73).unwrap(),
            ControlRequest {
                version: CONTROL_PROTOCOL_VERSION,
                id: 73,
                timeout_ms: None,
                command: ControlCommand::NotificationCreate {
                    workspace_id: 12,
                    surface_id: 99,
                    source: NotificationSource::AgentHook,
                    level: NotificationLevel::Warning,
                    title: "build-agent: needs attention".to_string(),
                    body: "please review the generated patch".to_string(),
                },
            }
        );
    }

    #[test]
    fn filtered_subagents_remain_in_the_audit_log() {
        let mut router = AgentHookRouter::new(HookFilter {
            show_subagents: false,
            show_teammates: true,
        });
        let mut subagent = event(AgentHookKind::TaskComplete);
        subagent.role = AgentRole::Subagent;

        let routed = router.route(subagent).unwrap();
        assert_eq!(routed.delivery, HookDelivery::FilteredByRole);
        assert!(routed.command.is_none());

        let audit: Vec<_> = router.audit_records().collect();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].role, AgentRole::Subagent);
        assert_eq!(audit[0].delivery, HookDelivery::FilteredByRole);
        assert_eq!(audit[0].origin.surface_id, 99);
    }

    #[test]
    fn audit_log_is_bounded_without_reusing_ids() {
        let mut router = AgentHookRouter::with_audit_capacity(HookFilter::default(), 2);
        for _ in 0..3 {
            router.route(event(AgentHookKind::Waiting)).unwrap();
        }
        let ids: Vec<_> = router.audit_records().map(|record| record.id).collect();
        assert_eq!(ids, vec![2, 3]);
    }

    #[test]
    fn adapters_are_disabled_by_default_and_own_the_agent_label() {
        let frame = br#"{"version":1,"origin":{"workspace_id":12,"surface_id":99},"kind":"waiting","role":"primary"}"#;
        assert_eq!(
            AgentAdapter::Codex
                .parse_opt_in_hook(AdapterSettings::default(), frame)
                .unwrap_err(),
            AdapterError::Disabled(AgentAdapter::Codex)
        );

        let event = AgentAdapter::Codex
            .parse_opt_in_hook(
                AdapterSettings {
                    enabled: true,
                    resume_enabled: false,
                },
                frame,
            )
            .unwrap();
        assert_eq!(event.adapter(), AgentAdapter::Codex);
        assert_eq!(event.event().agent, "codex");
    }

    #[test]
    fn adapter_persistence_ids_match_the_documented_opt_in_names() {
        assert_eq!(
            serde_json::to_string(&AgentAdapter::Codex).unwrap(),
            "\"codex\""
        );
        assert_eq!(
            serde_json::to_string(&AgentAdapter::ClaudeCode).unwrap(),
            "\"claude-code\""
        );
        assert_eq!(
            serde_json::to_string(&AgentAdapter::OpenCode).unwrap(),
            "\"opencode\""
        );
        assert_eq!(
            serde_json::to_string(&AgentAdapter::Gemini).unwrap(),
            "\"gemini\""
        );
    }

    #[test]
    fn unsafe_session_ids_cannot_become_command_arguments() {
        let unsafe_frame = br#"{"version":1,"origin":{"workspace_id":12,"surface_id":99},"kind":"waiting","public_session_id":"--dangerously-bypass-approvals"}"#;
        assert!(matches!(
            AgentAdapter::Codex.parse_opt_in_hook(
                AdapterSettings {
                    enabled: true,
                    resume_enabled: true,
                },
                unsafe_frame,
            ),
            Err(AdapterError::Parse(HookParseError {
                code: HookParseErrorCode::InvalidEvent,
                ..
            }))
        ));
    }
}
