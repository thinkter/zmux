//! Safe, opt-in building blocks for SSH-backed zmux workspaces.
//!
//! This module intentionally separates **an ordinary SSH terminal** from the
//! optional remote integrations around it. A normal remote workspace runs the
//! user's `ssh` binary with a small, audited argument list and inherits the
//! user's SSH configuration and local agent. It does not upload credentials,
//! parse a shell command line, or silently enable port, agent, or browser
//! forwarding.
//!
//! The module is UI-independent so reconnect policy, durable identity,
//! authenticated relay messages, and tmux control-mode projection can be
//! exercised without starting GPUI or a real SSH session. The workspace panel
//! owns the eventual process lifecycle and feeds connection observations back
//! into [`ReconnectController`].

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt, fs,
    io::{self, Read as _, Write as _},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tempfile::NamedTempFile;

use crate::{
    SurfaceId,
    notifications::{NotificationLevel, WorkspaceId},
};

/// Maximum persisted remote workspaces in one zmux profile. Keeping this
/// bounded prevents a malformed state file from becoming an unbounded startup
/// allocation.
pub const MAX_REMOTE_WORKSPACES: usize = 128;
/// State files larger than this are rejected before they are deserialized.
pub const MAX_REMOTE_WORKSPACE_STATE_BYTES: u64 = 1024 * 1024;
/// Reconnects are deliberately bounded; a dead host must not retry forever.
pub const MAX_RECONNECT_ATTEMPTS: u8 = 8;
/// Maximum routes allowed in one explicitly enabled SSH workspace.
pub const MAX_REMOTE_PORT_ROUTES: usize = 8;
/// Largest authenticated relay frame accepted by the local loopback listener.
pub const MAX_RELAY_FRAME_BYTES: usize = 64 * 1024;
/// The local relay remembers a bounded replay window per grant.
pub const MAX_RELAY_NONCES: usize = 256;
/// Protocol version for authenticated remote relay envelopes.
pub const REMOTE_RELAY_PROTOCOL_VERSION: u16 = 1;
/// Version for the small, zmux-owned remote workspace state file.
pub const REMOTE_WORKSPACE_STATE_VERSION: u16 = 1;

type HmacSha256 = Hmac<Sha256>;

/// A typed, user-facing validation or policy error from the remote subsystem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteError {
    message: String,
}

impl RemoteError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RemoteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RemoteError {}

impl From<io::Error> for RemoteError {
    fn from(error: io::Error) -> Self {
        Self::new(error.to_string())
    }
}

type RemoteResult<T> = Result<T, RemoteError>;

/// A concrete SSH host or `Host` alias. This deliberately accepts a compact
/// portable subset rather than arbitrary shell syntax: users with unusual
/// machine names can put the real name in `~/.ssh/config` and connect through
/// a conventional alias.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SshHost(String);

impl SshHost {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SshHost {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl TryFrom<&str> for SshHost {
    type Error = RemoteError;

    fn try_from(value: &str) -> RemoteResult<Self> {
        validate_ssh_host(value)?;
        Ok(Self(value.to_owned()))
    }
}

impl TryFrom<String> for SshHost {
    type Error = RemoteError;

    fn try_from(value: String) -> RemoteResult<Self> {
        Self::try_from(value.as_str())
    }
}

impl From<SshHost> for String {
    fn from(value: SshHost) -> Self {
        value.0
    }
}

/// An optional SSH login name. It is emitted through `ssh -l USER`, never
/// concatenated into a `user@host` shell string.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SshUsername(String);

impl SshUsername {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for SshUsername {
    type Error = RemoteError;

    fn try_from(value: &str) -> RemoteResult<Self> {
        if value.is_empty()
            || value.len() > 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(RemoteError::new(
                "SSH usernames may contain only ASCII letters, digits, '.', '_' and '-'",
            ));
        }
        Ok(Self(value.to_owned()))
    }
}

impl TryFrom<String> for SshUsername {
    type Error = RemoteError;

    fn try_from(value: String) -> RemoteResult<Self> {
        Self::try_from(value.as_str())
    }
}

impl From<SshUsername> for String {
    fn from(value: SshUsername) -> Self {
        value.0
    }
}

/// Only identity-bearing fields are included here. Passwords, private-key
/// paths, agent sockets, display labels, and runtime reconnect state are not
/// part of durable remote identity.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SshDestination {
    pub host: SshHost,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<SshUsername>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

impl SshDestination {
    pub fn new(host: SshHost) -> Self {
        Self {
            host,
            username: None,
            port: None,
        }
    }

    pub fn validate(&self) -> RemoteResult<()> {
        if self.port == Some(0) {
            return Err(RemoteError::new("SSH port must be between 1 and 65535"));
        }
        Ok(())
    }

    fn stable_key(&self) -> String {
        format!(
            "{}\0{}\0{}",
            self.host.as_str(),
            self.username.as_ref().map_or("", SshUsername::as_str),
            self.port.map_or_else(String::new, |port| port.to_string())
        )
    }
}

/// Which SSH configuration source should be consulted. The default leaves
/// `-F` absent, which is important: OpenSSH then uses the user's normal
/// `~/.ssh/config`, includes, known-hosts setup, and `SSH_AUTH_SOCK` agent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "path")]
pub enum SshConfigSource {
    #[default]
    UserDefault,
    ExplicitFile(PathBuf),
}

/// Host-key policy is explicit and deliberately has no insecure `off` mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostKeyPolicy {
    /// Add `StrictHostKeyChecking=yes`; the host key must already be trusted.
    #[default]
    RequireKnownHost,
    /// Permit OpenSSH to add a previously unseen key, but still reject a
    /// changed key. This is an intentional first-connect choice.
    AcceptNew,
    /// Do not add an override; the user's SSH configuration owns this policy.
    UseUserConfig,
}

/// Local agent use is inherited by OpenSSH. Agent *forwarding* is a separate,
/// higher-risk remote capability and remains disabled unless explicitly set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentForwarding {
    #[default]
    Disabled,
    ExplicitlyEnabled,
}

/// Remote TCP hosts accepted for a loopback-only SSH `-L` route.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RemoteTcpHost(String);

impl RemoteTcpHost {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn ssh_forward_component(&self) -> String {
        if self.0.contains(':') && !self.0.starts_with('[') {
            format!("[{}]", self.0)
        } else {
            self.0.clone()
        }
    }
}

impl TryFrom<&str> for RemoteTcpHost {
    type Error = RemoteError;

    fn try_from(value: &str) -> RemoteResult<Self> {
        if value.is_empty()
            || value.len() > 253
            || !value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(byte, b'.' | b'_' | b'-' | b':' | b'[' | b']')
            })
        {
            return Err(RemoteError::new(
                "remote TCP hosts may contain only ASCII host-name or IPv6 characters",
            ));
        }
        Ok(Self(value.to_owned()))
    }
}

impl TryFrom<String> for RemoteTcpHost {
    type Error = RemoteError;

    fn try_from(value: String) -> RemoteResult<Self> {
        Self::try_from(value.as_str())
    }
}

impl From<RemoteTcpHost> for String {
    fn from(value: RemoteTcpHost) -> Self {
        value.0
    }
}

/// One opt-in, local-loopback SSH port forward. No route may bind a LAN or
/// public address: browser/port routing must not accidentally expose a local
/// development service to the network.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemotePortRoute {
    pub local_port: u16,
    pub remote_host: RemoteTcpHost,
    pub remote_port: u16,
    /// Requesting browser handling is separate from making the TCP route. It
    /// is denied unless the caller advertises browser-routing capability.
    #[serde(default)]
    pub browser_surface: bool,
}

impl RemotePortRoute {
    fn validate(&self) -> RemoteResult<()> {
        if self.local_port == 0 || self.remote_port == 0 {
            return Err(RemoteError::new(
                "remote port routes require non-zero local and remote ports",
            ));
        }
        Ok(())
    }

    fn ssh_spec(&self) -> String {
        format!(
            "127.0.0.1:{}:{}:{}",
            self.local_port,
            self.remote_host.ssh_forward_component(),
            self.remote_port
        )
    }
}

/// Routing defaults to disabled. SSH config forwards are also cleared by the
/// launch plan so an ordinary remote terminal has no surprise tunnels.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode", content = "routes")]
pub enum RemotePortRouting {
    #[default]
    Disabled,
    Loopback(Vec<RemotePortRoute>),
}

impl RemotePortRouting {
    fn validate(&self, capabilities: RemoteCapabilities) -> RemoteResult<()> {
        let Self::Loopback(routes) = self else {
            return Ok(());
        };
        if !capabilities.port_routing {
            return Err(RemoteError::new(
                "remote port routing is not enabled by this zmux capability set",
            ));
        }
        if routes.is_empty() || routes.len() > MAX_REMOTE_PORT_ROUTES {
            return Err(RemoteError::new(format!(
                "remote port routing requires between 1 and {MAX_REMOTE_PORT_ROUTES} routes"
            )));
        }
        let mut local_ports = BTreeSet::new();
        for route in routes {
            route.validate()?;
            if !local_ports.insert(route.local_port) {
                return Err(RemoteError::new(
                    "each remote port route must use a distinct local port",
                ));
            }
            if route.browser_surface && !capabilities.browser_routing {
                return Err(RemoteError::new(
                    "browser routing was requested but is not supported by this build",
                ));
            }
        }
        Ok(())
    }
}

/// Authenticated relay is opt-in because it requires a reverse SSH tunnel and
/// an in-memory capability token on the remote host. The token is never
/// persisted; the integration that owns a listener must rotate or discard the
/// grant when the corresponding remote session ends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode", content = "remote_port")]
pub enum RemoteRelayMode {
    #[default]
    Disabled,
    ReverseTunnel(u16),
}

impl RemoteRelayMode {
    fn validate(&self, capabilities: RemoteCapabilities) -> RemoteResult<()> {
        match self {
            Self::Disabled => Ok(()),
            Self::ReverseTunnel(port) if *port == 0 => Err(RemoteError::new(
                "an authenticated relay reverse tunnel needs a non-zero remote port",
            )),
            Self::ReverseTunnel(_) if !capabilities.authenticated_relay => Err(RemoteError::new(
                "authenticated remote relay is not enabled by this zmux capability set",
            )),
            Self::ReverseTunnel(_) => Ok(()),
        }
    }
}

/// The optional tmux integration is deliberately an explicit experimental
/// setting. It is never included in an ordinary SSH terminal command.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode", content = "session")]
pub enum TmuxBridgeConfig {
    #[default]
    Disabled,
    Experimental(TmuxSessionName),
}

/// A tmux session name accepted by the fixed `tmux -CC new-session -A -s`
/// invocation. Restricting this removes remote-shell metacharacters from the
/// only remote command zmux itself can construct.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TmuxSessionName(String);

impl TmuxSessionName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for TmuxSessionName {
    type Error = RemoteError;

    fn try_from(value: &str) -> RemoteResult<Self> {
        if value.is_empty()
            || value.len() > 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(RemoteError::new(
                "tmux session names may contain only ASCII letters, digits, '.', '_' and '-'",
            ));
        }
        Ok(Self(value.to_owned()))
    }
}

impl TryFrom<String> for TmuxSessionName {
    type Error = RemoteError;

    fn try_from(value: String) -> RemoteResult<Self> {
        Self::try_from(value.as_str())
    }
}

impl From<TmuxSessionName> for String {
    fn from(value: TmuxSessionName) -> Self {
        value.0
    }
}

/// Feature gates supplied by the caller that owns UI/platform integrations.
/// The foundation advertises relay, safe TCP forwarding, and tmux metadata
/// parsing, but deliberately does not claim browser-surface support yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RemoteCapabilities {
    pub port_routing: bool,
    pub browser_routing: bool,
    pub authenticated_relay: bool,
    pub tmux_control: bool,
}

impl RemoteCapabilities {
    pub const fn foundation() -> Self {
        Self {
            port_routing: true,
            browser_routing: false,
            authenticated_relay: true,
            tmux_control: true,
        }
    }
}

/// A persisted remote workspace definition. It contains no private key,
/// password, agent socket, relay token, or raw shell command. `remote_root`
/// participates in identity only; zmux does not use it to compose a remote
/// shell command in this foundation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshWorkspaceConfig {
    pub display_name: String,
    pub destination: SshDestination,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_root: Option<String>,
    #[serde(default)]
    pub ssh_config: SshConfigSource,
    #[serde(default)]
    pub host_key_policy: HostKeyPolicy,
    #[serde(default)]
    pub agent_forwarding: AgentForwarding,
    #[serde(default)]
    pub port_routing: RemotePortRouting,
    #[serde(default)]
    pub relay: RemoteRelayMode,
    #[serde(default)]
    pub tmux: TmuxBridgeConfig,
}

impl SshWorkspaceConfig {
    pub fn new(destination: SshDestination) -> Self {
        Self {
            display_name: destination.host.to_string(),
            destination,
            remote_root: None,
            ssh_config: SshConfigSource::UserDefault,
            host_key_policy: HostKeyPolicy::RequireKnownHost,
            agent_forwarding: AgentForwarding::Disabled,
            port_routing: RemotePortRouting::Disabled,
            relay: RemoteRelayMode::Disabled,
            tmux: TmuxBridgeConfig::Disabled,
        }
    }

    pub fn validate(&self) -> RemoteResult<()> {
        self.destination.validate()?;
        if self.display_name.trim().is_empty() || self.display_name.chars().count() > 128 {
            return Err(RemoteError::new(
                "remote workspace display names must contain 1 to 128 characters",
            ));
        }
        if let Some(remote_root) = &self.remote_root
            && (remote_root.contains('\0') || remote_root.len() > 4096)
        {
            return Err(RemoteError::new(
                "remote workspace roots may not contain NUL and must be at most 4096 bytes",
            ));
        }
        Ok(())
    }

    pub fn identity(&self) -> RemoteWorkspaceIdentity {
        RemoteWorkspaceIdentity::new(self.destination.clone(), self.remote_root.clone())
    }

    /// Build an argv-only SSH launch plan. The plan has no arbitrary "extra
    /// argument" escape hatch on purpose. Users configure jumps, identities,
    /// multiplexing, and local-agent use through their SSH config.
    pub fn launch_plan(
        &self,
        capabilities: RemoteCapabilities,
        relay_listener: Option<&RemoteRelayListener>,
    ) -> RemoteResult<SshLaunchPlan> {
        self.validate()?;
        self.port_routing.validate(capabilities)?;
        self.relay.validate(capabilities)?;
        if matches!(self.tmux, TmuxBridgeConfig::Experimental(_)) && !capabilities.tmux_control {
            return Err(RemoteError::new(
                "experimental tmux control mode is not enabled by this zmux capability set",
            ));
        }

        let mut args = vec![
            "-o".to_owned(),
            // Do not inherit unreviewed -L/-R entries from SSH config.
            "ClearAllForwardings=yes".to_owned(),
            "-o".to_owned(),
            match self.agent_forwarding {
                AgentForwarding::Disabled => "ForwardAgent=no",
                AgentForwarding::ExplicitlyEnabled => "ForwardAgent=yes",
            }
            .to_owned(),
        ];

        match &self.ssh_config {
            SshConfigSource::UserDefault => {}
            SshConfigSource::ExplicitFile(path) => {
                let path = path.to_str().ok_or_else(|| {
                    RemoteError::new("the explicit SSH config path is not valid Unicode")
                })?;
                args.extend(["-F".to_owned(), path.to_owned()]);
            }
        }

        match self.host_key_policy {
            HostKeyPolicy::RequireKnownHost => {
                args.extend(["-o".to_owned(), "StrictHostKeyChecking=yes".to_owned()]);
            }
            HostKeyPolicy::AcceptNew => {
                args.extend([
                    "-o".to_owned(),
                    "StrictHostKeyChecking=accept-new".to_owned(),
                ]);
            }
            HostKeyPolicy::UseUserConfig => {}
        }

        if let Some(port) = self.destination.port {
            args.extend(["-p".to_owned(), port.to_string()]);
        }
        if let Some(username) = &self.destination.username {
            args.extend(["-l".to_owned(), username.as_str().to_owned()]);
        }

        if let RemotePortRouting::Loopback(routes) = &self.port_routing {
            args.extend(["-o".to_owned(), "ExitOnForwardFailure=yes".to_owned()]);
            for route in routes {
                args.extend(["-L".to_owned(), route.ssh_spec()]);
            }
        }

        if let RemoteRelayMode::ReverseTunnel(remote_port) = self.relay {
            let listener = relay_listener.ok_or_else(|| {
                RemoteError::new(
                    "an authenticated relay reverse tunnel needs a prepared local relay listener",
                )
            })?;
            args.extend([
                "-o".to_owned(),
                // Keep the remote listener on loopback even if the SSH daemon
                // has a permissive GatewayPorts default.
                "GatewayPorts=no".to_owned(),
                "-o".to_owned(),
                "ExitOnForwardFailure=yes".to_owned(),
                "-R".to_owned(),
                format!(
                    "127.0.0.1:{remote_port}:127.0.0.1:{}",
                    listener.local_port()
                ),
            ]);
        }

        let tmux_session = match &self.tmux {
            TmuxBridgeConfig::Disabled => None,
            TmuxBridgeConfig::Experimental(session) => {
                // This is an SSH option, so it must precede the destination;
                // arguments after the destination are the fixed remote tmux
                // command below.
                args.push("-tt".to_owned());
                Some(session)
            }
        };
        args.push(self.destination.host.as_str().to_owned());
        let mode = match tmux_session {
            None => SshSessionMode::Terminal,
            Some(session) => {
                // OpenSSH ultimately asks the remote SSH server to execute a
                // command. Every token below is fixed or validated by
                // `TmuxSessionName`; no user-provided shell fragment exists.
                args.extend([
                    "tmux".to_owned(),
                    "-CC".to_owned(),
                    "new-session".to_owned(),
                    "-A".to_owned(),
                    "-s".to_owned(),
                    session.as_str().to_owned(),
                ]);
                SshSessionMode::ExperimentalTmuxControl
            }
        };

        Ok(SshLaunchPlan {
            program: "ssh".to_owned(),
            args,
            label: format!("SSH {}", self.display_name),
            mode,
        })
    }
}

/// An argv-only process description. The caller must pass `program` and
/// `args` directly to the terminal/process API; it must not turn this back into
/// a shell string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshLaunchPlan {
    pub program: String,
    pub args: Vec<String>,
    pub label: String,
    pub mode: SshSessionMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SshSessionMode {
    Terminal,
    ExperimentalTmuxControl,
}

/// Stable, secrets-free identity used for persistence and reconnect matching.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RemoteWorkspaceIdentity {
    pub id: RemoteWorkspaceId,
    pub destination: SshDestination,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_root: Option<String>,
}

impl RemoteWorkspaceIdentity {
    pub fn new(destination: SshDestination, remote_root: Option<String>) -> Self {
        let mut key = destination.stable_key();
        key.push('\0');
        key.push_str(remote_root.as_deref().unwrap_or_default());
        let digest = Sha256::digest(key.as_bytes());
        let mut id = String::from("ssh-");
        for byte in &digest[..16] {
            use std::fmt::Write as _;
            write!(&mut id, "{byte:02x}").expect("writing to String cannot fail");
        }
        Self {
            id: RemoteWorkspaceId(id),
            destination,
            remote_root,
        }
    }
}

/// A deterministic identity key, not an authentication secret.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RemoteWorkspaceId(String);

impl RemoteWorkspaceId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RemoteWorkspaceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

fn validate_ssh_host(value: &str) -> RemoteResult<()> {
    if value.is_empty()
        || value.len() > 253
        || value.starts_with('-')
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':' | b'[' | b']')
        })
    {
        return Err(RemoteError::new(
            "SSH hosts must be a conventional alias, hostname, or IP address and may not start with '-'",
        ));
    }
    Ok(())
}

/// File name under zmux's own `paths::state_dir()`. It deliberately does not
/// reuse Zed recent-project or remote-server state.
pub const REMOTE_WORKSPACES_STATE_FILE: &str = "remote-workspaces-v1.json";

/// Return the durable store location for this zmux profile.
pub fn remote_workspace_store_path() -> PathBuf {
    paths::state_dir().join(REMOTE_WORKSPACES_STATE_FILE)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteWorkspaceStore {
    workspaces: BTreeMap<RemoteWorkspaceId, SshWorkspaceConfig>,
}

#[derive(Serialize, Deserialize)]
struct PersistedRemoteWorkspaceStore {
    version: u16,
    workspaces: Vec<SshWorkspaceConfig>,
}

impl RemoteWorkspaceStore {
    pub fn load_default() -> RemoteResult<Self> {
        Self::load_from_path(&remote_workspace_store_path())
    }

    /// Missing state is a normal first-run case. Existing state is size-limited
    /// and fully validated after deserialization before it can affect a launch.
    pub fn load_from_path(path: &Path) -> RemoteResult<Self> {
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error.into()),
        };
        if metadata.len() > MAX_REMOTE_WORKSPACE_STATE_BYTES {
            return Err(RemoteError::new(format!(
                "remote workspace state exceeds {MAX_REMOTE_WORKSPACE_STATE_BYTES} bytes"
            )));
        }

        let bytes = fs::read(path)?;
        let persisted: PersistedRemoteWorkspaceStore =
            serde_json::from_slice(&bytes).map_err(|error| {
                RemoteError::new(format!("invalid remote workspace state: {error}"))
            })?;
        if persisted.version != REMOTE_WORKSPACE_STATE_VERSION {
            return Err(RemoteError::new(format!(
                "unsupported remote workspace state version {}; expected {REMOTE_WORKSPACE_STATE_VERSION}",
                persisted.version
            )));
        }
        if persisted.workspaces.len() > MAX_REMOTE_WORKSPACES {
            return Err(RemoteError::new(format!(
                "remote workspace state contains more than {MAX_REMOTE_WORKSPACES} entries"
            )));
        }

        let mut store = Self::default();
        for workspace in persisted.workspaces {
            let identity = workspace.identity();
            if store.workspaces.contains_key(&identity.id) {
                return Err(RemoteError::new(
                    "remote workspace state contains duplicate destination identities",
                ));
            }
            store.upsert(workspace)?;
        }
        Ok(store)
    }

    pub fn save_default(&self) -> RemoteResult<()> {
        self.save_to_path(&remote_workspace_store_path())
    }

    /// Atomically write a private JSON file. `NamedTempFile::persist` keeps an
    /// old, valid store in place if serializing or syncing the replacement
    /// fails before the final rename.
    pub fn save_to_path(&self, path: &Path) -> RemoteResult<()> {
        let parent = path.parent().ok_or_else(|| {
            RemoteError::new("remote workspace state path must have a parent directory")
        })?;
        fs::create_dir_all(parent)?;
        restrict_directory_permissions(parent)?;

        let persisted = PersistedRemoteWorkspaceStore {
            version: REMOTE_WORKSPACE_STATE_VERSION,
            workspaces: self.workspaces.values().cloned().collect(),
        };
        let mut bytes = serde_json::to_vec_pretty(&persisted).map_err(|error| {
            RemoteError::new(format!("serializing remote workspace state: {error}"))
        })?;
        bytes.push(b'\n');

        let mut temporary = NamedTempFile::new_in(parent).map_err(|error| {
            RemoteError::new(format!("creating remote workspace state file: {error}"))
        })?;
        temporary.write_all(&bytes).map_err(|error| {
            RemoteError::new(format!("writing remote workspace state: {error}"))
        })?;
        temporary.as_file().sync_all().map_err(|error| {
            RemoteError::new(format!("syncing remote workspace state: {error}"))
        })?;
        temporary.persist(path).map_err(|error| {
            RemoteError::new(format!("replacing remote workspace state: {}", error.error))
        })?;
        restrict_file_permissions(path)?;
        sync_directory(parent)?;
        Ok(())
    }

    pub fn upsert(&mut self, workspace: SshWorkspaceConfig) -> RemoteResult<RemoteWorkspaceId> {
        workspace.validate()?;
        if self.workspaces.len() >= MAX_REMOTE_WORKSPACES
            && !self.workspaces.contains_key(&workspace.identity().id)
        {
            return Err(RemoteError::new(format!(
                "at most {MAX_REMOTE_WORKSPACES} remote workspaces may be persisted"
            )));
        }
        let id = workspace.identity().id;
        self.workspaces.insert(id.clone(), workspace);
        Ok(id)
    }

    pub fn remove(&mut self, id: &RemoteWorkspaceId) -> Option<SshWorkspaceConfig> {
        self.workspaces.remove(id)
    }

    pub fn get(&self, id: &RemoteWorkspaceId) -> Option<&SshWorkspaceConfig> {
        self.workspaces.get(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&RemoteWorkspaceId, &SshWorkspaceConfig)> {
        self.workspaces.iter()
    }

    pub fn len(&self) -> usize {
        self.workspaces.len()
    }

    pub fn is_empty(&self) -> bool {
        self.workspaces.is_empty()
    }
}

#[cfg(unix)]
fn restrict_directory_permissions(path: &Path) -> RemoteResult<()> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_directory_permissions(_path: &Path) -> RemoteResult<()> {
    Ok(())
}

#[cfg(unix)]
fn restrict_file_permissions(path: &Path) -> RemoteResult<()> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_file_permissions(_path: &Path) -> RemoteResult<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> RemoteResult<()> {
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> RemoteResult<()> {
    Ok(())
}

/// Bounded exponential reconnect policy. Values are milliseconds to keep its
/// persisted/configurable form portable and easy to display in the sidebar.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconnectPolicy {
    pub max_attempts: u8,
    pub initial_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            initial_delay_ms: 250,
            max_delay_ms: 5_000,
        }
    }
}

impl ReconnectPolicy {
    pub fn validate(&self) -> RemoteResult<()> {
        if self.max_attempts == 0 || self.max_attempts > MAX_RECONNECT_ATTEMPTS {
            return Err(RemoteError::new(format!(
                "reconnect attempts must be between 1 and {MAX_RECONNECT_ATTEMPTS}"
            )));
        }
        if self.initial_delay_ms == 0 || self.max_delay_ms < self.initial_delay_ms {
            return Err(RemoteError::new(
                "reconnect delays must be non-zero and max_delay_ms must not be less than initial_delay_ms",
            ));
        }
        Ok(())
    }

    pub fn delay_for_attempt(&self, attempt: u8) -> RemoteResult<Duration> {
        self.validate()?;
        if attempt == 0 || attempt > self.max_attempts {
            return Err(RemoteError::new(
                "reconnect attempt is outside the configured bound",
            ));
        }
        let shift = u32::from(attempt.saturating_sub(1)).min(63);
        let multiplier = 1_u64.checked_shl(shift).unwrap_or(u64::MAX);
        let milliseconds = self
            .initial_delay_ms
            .saturating_mul(multiplier)
            .min(self.max_delay_ms);
        Ok(Duration::from_millis(milliseconds))
    }
}

/// Human-readable state for the workspace sidebar. A `Disconnected` state
/// never starts retries itself; the UI/process owner decides when to invoke
/// [`ReconnectController::connection_lost`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting { attempt: u8, delay: Duration },
    Exhausted { attempts: u8 },
}

impl RemoteConnectionState {
    pub fn sidebar_label(&self) -> String {
        match self {
            Self::Disconnected => "remote: disconnected".to_owned(),
            Self::Connecting => "remote: connecting".to_owned(),
            Self::Connected => "remote: connected".to_owned(),
            Self::Reconnecting { attempt, delay } => {
                format!("remote: retry {attempt} in {} ms", delay.as_millis())
            }
            Self::Exhausted { attempts } => format!("remote: reconnect stopped after {attempts}"),
        }
    }
}

/// Runtime-only reconnect bookkeeping. It is intentionally not persisted: a
/// fresh zmux launch starts disconnected rather than resurrecting stale retry
/// timers from a previous process.
#[derive(Clone, Debug)]
pub struct ReconnectController {
    policy: ReconnectPolicy,
    attempts: u8,
    state: RemoteConnectionState,
}

impl ReconnectController {
    pub fn new(policy: ReconnectPolicy) -> RemoteResult<Self> {
        policy.validate()?;
        Ok(Self {
            policy,
            attempts: 0,
            state: RemoteConnectionState::Disconnected,
        })
    }

    pub fn state(&self) -> &RemoteConnectionState {
        &self.state
    }

    pub fn attempts(&self) -> u8 {
        self.attempts
    }

    pub fn begin_connect(&mut self) {
        self.state = RemoteConnectionState::Connecting;
    }

    pub fn connected(&mut self) {
        self.attempts = 0;
        self.state = RemoteConnectionState::Connected;
    }

    /// Record one observed connection loss and return the next visible state.
    /// Once the configured budget is exhausted no more retry schedule is
    /// produced until the user/reconnector explicitly starts again.
    pub fn connection_lost(&mut self) -> RemoteConnectionState {
        if self.attempts >= self.policy.max_attempts {
            self.state = RemoteConnectionState::Exhausted {
                attempts: self.attempts,
            };
            return self.state.clone();
        }

        self.attempts = self.attempts.saturating_add(1);
        let delay = self
            .policy
            .delay_for_attempt(self.attempts)
            .expect("validated reconnect policy must accept a bounded attempt");
        self.state = RemoteConnectionState::Reconnecting {
            attempt: self.attempts,
            delay,
        };
        self.state.clone()
    }

    pub fn reset(&mut self) {
        self.attempts = 0;
        self.state = RemoteConnectionState::Disconnected;
    }
}

/// An exact, fixed UI target authorized for one remote relay token. Remote
/// clients cannot name a workspace or surface of their choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRelayTarget {
    pub workspace_id: WorkspaceId,
    pub surface_id: SurfaceId,
}

/// The only remote-originated event accepted in this foundation. It cannot
/// execute input, select a workspace, create a browser, or otherwise command
/// an arbitrary local UI surface.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "data")]
pub enum RemoteRelayEvent {
    Notification {
        #[serde(default)]
        level: NotificationLevel,
        title: String,
        body: String,
    },
}

impl RemoteRelayEvent {
    fn validate(&self) -> RemoteResult<()> {
        match self {
            Self::Notification { title, body, .. } => {
                if title.is_empty() || title.chars().count() > 256 {
                    return Err(RemoteError::new(
                        "remote notification titles must contain 1 to 256 characters",
                    ));
                }
                if body.len() > 16 * 1024 {
                    return Err(RemoteError::new(
                        "remote notification bodies may not exceed 16 KiB",
                    ));
                }
                Ok(())
            }
        }
    }
}

/// An in-memory, 256-bit capability token. It intentionally has no `Debug`,
/// `Display`, or serde implementation, so logging/persisting a workspace
/// cannot leak it. Listener owners are responsible for discarding a grant when
/// its associated remote session ends.
#[derive(Clone, PartialEq, Eq)]
pub struct RemoteRelayToken([u8; 32]);

impl RemoteRelayToken {
    pub fn generate() -> RemoteResult<Self> {
        let mut token = [0_u8; 32];
        getrandom::fill(&mut token)
            .map_err(|error| RemoteError::new(format!("generating relay token: {error}")))?;
        Ok(Self(token))
    }

    /// Intentionally explicit because whoever receives this value can send
    /// authenticated notifications to the one target in the associated grant.
    pub fn encode_for_remote(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }
}

/// A relay capability scoped to one workspace and one terminal surface.
#[derive(Clone)]
pub struct RemoteRelayGrant {
    target: RemoteRelayTarget,
    token: RemoteRelayToken,
}

impl RemoteRelayGrant {
    pub fn generate(target: RemoteRelayTarget) -> RemoteResult<Self> {
        Ok(Self {
            target,
            token: RemoteRelayToken::generate()?,
        })
    }

    pub fn target(&self) -> RemoteRelayTarget {
        self.target
    }

    pub fn token_for_remote(&self) -> String {
        self.token.encode_for_remote()
    }

    pub fn sign(&self, event: RemoteRelayEvent) -> RemoteResult<RemoteRelayEnvelope> {
        event.validate()?;
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce)
            .map_err(|error| RemoteError::new(format!("generating relay nonce: {error}")))?;
        self.sign_with_nonce(event, nonce)
    }

    /// The remote relay client can use this when it generates its own 128-bit
    /// random nonce. The nonce is authenticated and replay-protected locally.
    pub fn sign_with_nonce(
        &self,
        event: RemoteRelayEvent,
        nonce: [u8; 16],
    ) -> RemoteResult<RemoteRelayEnvelope> {
        event.validate()?;
        let mut envelope = RemoteRelayEnvelope {
            version: REMOTE_RELAY_PROTOCOL_VERSION,
            target: self.target,
            nonce: URL_SAFE_NO_PAD.encode(nonce),
            event,
            auth_tag: String::new(),
        };
        envelope.auth_tag = sign_relay_envelope(&self.token, &envelope)?;
        Ok(envelope)
    }

    pub fn verifier(&self) -> RemoteRelayVerifier {
        RemoteRelayVerifier {
            target: self.target,
            token: self.token.clone(),
            seen_nonces: VecDeque::new(),
        }
    }
}

/// Wire envelope for a remote relay message. Its target is verified against
/// the local grant, not trusted from the remote client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRelayEnvelope {
    pub version: u16,
    pub target: RemoteRelayTarget,
    pub nonce: String,
    pub event: RemoteRelayEvent,
    pub auth_tag: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthenticatedRemoteRelayEvent {
    pub target: RemoteRelayTarget,
    pub event: RemoteRelayEvent,
}

/// Verifies HMAC authentication, exact target binding, event size, and a
/// bounded replay window. It accepts no mutation other than a notification.
pub struct RemoteRelayVerifier {
    target: RemoteRelayTarget,
    token: RemoteRelayToken,
    seen_nonces: VecDeque<Vec<u8>>,
}

impl RemoteRelayVerifier {
    pub fn verify(
        &mut self,
        envelope: RemoteRelayEnvelope,
    ) -> RemoteResult<AuthenticatedRemoteRelayEvent> {
        if envelope.version != REMOTE_RELAY_PROTOCOL_VERSION {
            return Err(RemoteError::new(format!(
                "unsupported remote relay protocol version {}; expected {REMOTE_RELAY_PROTOCOL_VERSION}",
                envelope.version
            )));
        }
        if envelope.target != self.target {
            return Err(RemoteError::new(
                "remote relay grant may not target a different workspace or surface",
            ));
        }
        envelope.event.validate()?;
        let nonce = URL_SAFE_NO_PAD
            .decode(&envelope.nonce)
            .map_err(|_| RemoteError::new("remote relay nonce is not valid base64url"))?;
        if nonce.len() != 16 {
            return Err(RemoteError::new(
                "remote relay nonces must contain exactly 128 bits",
            ));
        }

        verify_relay_envelope(&self.token, &envelope)?;
        if self.seen_nonces.iter().any(|seen| seen == &nonce) {
            return Err(RemoteError::new("remote relay message was replayed"));
        }
        self.seen_nonces.push_back(nonce);
        if self.seen_nonces.len() > MAX_RELAY_NONCES {
            self.seen_nonces.pop_front();
        }

        Ok(AuthenticatedRemoteRelayEvent {
            target: envelope.target,
            event: envelope.event,
        })
    }
}

#[derive(Serialize)]
struct UnsignedRelayEnvelope<'a> {
    version: u16,
    target: RemoteRelayTarget,
    nonce: &'a str,
    event: &'a RemoteRelayEvent,
}

fn relay_signing_bytes(envelope: &RemoteRelayEnvelope) -> RemoteResult<Vec<u8>> {
    serde_json::to_vec(&UnsignedRelayEnvelope {
        version: envelope.version,
        target: envelope.target,
        nonce: &envelope.nonce,
        event: &envelope.event,
    })
    .map_err(|error| RemoteError::new(format!("serializing remote relay envelope: {error}")))
}

fn sign_relay_envelope(
    token: &RemoteRelayToken,
    envelope: &RemoteRelayEnvelope,
) -> RemoteResult<String> {
    let mut mac = HmacSha256::new_from_slice(&token.0)
        .map_err(|_| RemoteError::new("invalid remote relay token length"))?;
    mac.update(&relay_signing_bytes(envelope)?);
    Ok(URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

fn verify_relay_envelope(
    token: &RemoteRelayToken,
    envelope: &RemoteRelayEnvelope,
) -> RemoteResult<()> {
    let tag = URL_SAFE_NO_PAD
        .decode(&envelope.auth_tag)
        .map_err(|_| RemoteError::new("remote relay authentication tag is not valid base64url"))?;
    let mut mac = HmacSha256::new_from_slice(&token.0)
        .map_err(|_| RemoteError::new("invalid remote relay token length"))?;
    mac.update(&relay_signing_bytes(envelope)?);
    mac.verify_slice(&tag)
        .map_err(|_| RemoteError::new("remote relay authentication failed"))
}

/// A local TCP listener suitable for SSH reverse forwarding. It binds only to
/// 127.0.0.1 and still requires the HMAC grant, so a forwarded remote client
/// cannot use it to address arbitrary local surfaces.
pub struct RemoteRelayListener {
    listener: TcpListener,
    verifier: RemoteRelayVerifier,
}

impl RemoteRelayListener {
    pub fn bind(grant: &RemoteRelayGrant) -> RemoteResult<Self> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
        Ok(Self {
            listener,
            verifier: grant.verifier(),
        })
    }

    pub fn local_port(&self) -> u16 {
        self.listener
            .local_addr()
            .expect("a bound relay listener always has a local address")
            .port()
    }

    /// Accept exactly one bounded relay frame. The caller owns the thread/task
    /// that drives this blocking operation and can apply its own lifecycle and
    /// reconnect deadlines.
    pub fn receive_one(&mut self) -> RemoteResult<AuthenticatedRemoteRelayEvent> {
        let (mut stream, peer) = self.listener.accept()?;
        if !peer.ip().is_loopback() {
            return Err(RemoteError::new(
                "refusing a remote relay connection that did not arrive from loopback",
            ));
        }
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        self.verifier.verify(read_relay_envelope(&mut stream)?)
    }
}

/// Write one length-prefixed relay envelope. This is intentionally public for
/// a future remote CLI, but callers still need the in-memory grant token to
/// create an envelope the listener accepts.
pub fn write_relay_envelope(
    stream: &mut TcpStream,
    envelope: &RemoteRelayEnvelope,
) -> RemoteResult<()> {
    let payload = serde_json::to_vec(envelope)
        .map_err(|error| RemoteError::new(format!("serializing remote relay frame: {error}")))?;
    if payload.len() > MAX_RELAY_FRAME_BYTES {
        return Err(RemoteError::new(format!(
            "remote relay frame exceeds {MAX_RELAY_FRAME_BYTES} bytes"
        )));
    }
    let length = u32::try_from(payload.len())
        .map_err(|_| RemoteError::new("remote relay frame length does not fit u32"))?;
    stream.write_all(&length.to_be_bytes())?;
    stream.write_all(&payload)?;
    stream.flush()?;
    Ok(())
}

fn read_relay_envelope(stream: &mut TcpStream) -> RemoteResult<RemoteRelayEnvelope> {
    let mut length = [0_u8; std::mem::size_of::<u32>()];
    stream.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_RELAY_FRAME_BYTES {
        return Err(RemoteError::new(format!(
            "remote relay frame exceeds {MAX_RELAY_FRAME_BYTES} bytes"
        )));
    }
    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload)?;
    serde_json::from_slice(&payload)
        .map_err(|error| RemoteError::new(format!("invalid remote relay frame: {error}")))
}

/// Stable tmux object IDs, represented without their wire prefixes (`@` for a
/// window and `%` for a pane).
pub type TmuxWindowId = u64;
pub type TmuxPaneId = u64;

/// A native-layout-friendly projection of tmux's layout grammar. Horizontal
/// means panes are side by side; vertical means panes are stacked. These map
/// directly to the split directions used by zmux's workspace center without
/// importing GPUI/workspace types into the protocol parser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TmuxLayoutNode {
    Pane(TmuxPaneId),
    Split {
        axis: TmuxSplitAxis,
        children: Vec<TmuxLayoutNode>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TmuxSplitAxis {
    Horizontal,
    Vertical,
}

/// One parsed tmux `%...` control-mode metadata event. Output, command
/// responses, and unknown future events are intentionally ignored rather than
/// interpreted as executable local commands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TmuxControlEvent {
    WindowAdded(TmuxWindowId),
    WindowClosed(TmuxWindowId),
    WindowRenamed {
        window_id: TmuxWindowId,
        name: String,
    },
    PaneAdded {
        pane_id: TmuxPaneId,
        window_id: TmuxWindowId,
    },
    PaneClosed(TmuxPaneId),
    ActiveWindowChanged(TmuxWindowId),
    ActivePaneChanged {
        window_id: TmuxWindowId,
        pane_id: TmuxPaneId,
    },
    LayoutChanged {
        window_id: TmuxWindowId,
        layout: TmuxLayoutNode,
    },
    Exited,
}

impl TmuxControlEvent {
    /// Parse metadata from one tmux control-mode line. Unknown events and
    /// terminal output return `Ok(None)` so a newer remote tmux does not break
    /// an existing normal SSH session or force an unsafe fallback.
    pub fn parse(line: &str) -> RemoteResult<Option<Self>> {
        if line.len() > 16 * 1024 {
            return Err(RemoteError::new(
                "tmux control-mode line exceeds 16 KiB and was rejected",
            ));
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line == "%exit" {
            return Ok(Some(Self::Exited));
        }
        if let Some(value) = line.strip_prefix("%window-add ") {
            return parse_tmux_id(value.trim(), '@')
                .map(Self::WindowAdded)
                .map(Some);
        }
        if let Some(value) = line.strip_prefix("%window-close ") {
            return parse_tmux_id(value.trim(), '@')
                .map(Self::WindowClosed)
                .map(Some);
        }
        if let Some(value) = line.strip_prefix("%window-renamed ") {
            let (window, name) = split_tmux_fields(value, 2)?;
            if name.chars().count() > 256 {
                return Err(RemoteError::new(
                    "tmux window names may not exceed 256 characters",
                ));
            }
            return Ok(Some(Self::WindowRenamed {
                window_id: parse_tmux_id(window, '@')?,
                name: name.to_owned(),
            }));
        }
        if let Some(value) = line.strip_prefix("%pane-add ") {
            let (pane, window) = split_tmux_fields(value, 2)?;
            return Ok(Some(Self::PaneAdded {
                pane_id: parse_tmux_id(pane, '%')?,
                window_id: parse_tmux_id(window, '@')?,
            }));
        }
        if let Some(value) = line.strip_prefix("%pane-died ") {
            let pane = value.split_whitespace().next().ok_or_else(|| {
                RemoteError::new("tmux pane-died event did not include a pane id")
            })?;
            return parse_tmux_id(pane, '%').map(Self::PaneClosed).map(Some);
        }
        if let Some(value) = line.strip_prefix("%session-window-changed ") {
            let window = value.split_whitespace().last().ok_or_else(|| {
                RemoteError::new("tmux session-window-changed event did not include a window id")
            })?;
            return parse_tmux_id(window, '@')
                .map(Self::ActiveWindowChanged)
                .map(Some);
        }
        if let Some(value) = line.strip_prefix("%window-pane-changed ") {
            let (window, pane) = split_tmux_fields(value, 2)?;
            return Ok(Some(Self::ActivePaneChanged {
                window_id: parse_tmux_id(window, '@')?,
                pane_id: parse_tmux_id(pane, '%')?,
            }));
        }
        if let Some(value) = line.strip_prefix("%layout-change ") {
            let mut fields = value.split_whitespace();
            let window = fields.next().ok_or_else(|| {
                RemoteError::new("tmux layout-change event did not include a window id")
            })?;
            let layout = fields.next().ok_or_else(|| {
                RemoteError::new("tmux layout-change event did not include a layout")
            })?;
            return Ok(Some(Self::LayoutChanged {
                window_id: parse_tmux_id(window, '@')?,
                layout: parse_tmux_layout(layout)?,
            }));
        }
        Ok(None)
    }
}

/// Parser for tmux's compact `#{window_layout}` grammar. It strips the
/// optional layout checksum (`abcd,`) and retains only pane topology; cell
/// dimensions and offsets are not meaningful to zmux's independently sized
/// native layout.
pub fn parse_tmux_layout(input: &str) -> RemoteResult<TmuxLayoutNode> {
    let mut input = input.trim();
    if let Some((checksum, remainder)) = input.split_once(',')
        && !checksum.is_empty()
        && checksum.bytes().all(|byte| byte.is_ascii_hexdigit())
        && remainder
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_digit())
    {
        input = remainder;
    }
    let mut parser = TmuxLayoutParser::new(input);
    let layout = parser.parse_node()?;
    if !parser.is_finished() {
        return Err(RemoteError::new(
            "unexpected suffix in tmux layout description",
        ));
    }
    Ok(layout)
}

struct TmuxLayoutParser<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> TmuxLayoutParser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            bytes: input.as_bytes(),
            cursor: 0,
        }
    }

    fn parse_node(&mut self) -> RemoteResult<TmuxLayoutNode> {
        let _width = self.parse_number()?;
        self.expect(b'x')?;
        let _height = self.parse_number()?;
        self.expect(b',')?;
        let _x = self.parse_number()?;
        self.expect(b',')?;
        let _y = self.parse_number()?;

        match self.peek() {
            Some(b',') => {
                self.cursor += 1;
                Ok(TmuxLayoutNode::Pane(self.parse_number()?))
            }
            Some(b'{') => self.parse_split(b'{', b'}', TmuxSplitAxis::Horizontal),
            Some(b'[') => self.parse_split(b'[', b']', TmuxSplitAxis::Vertical),
            _ => Err(RemoteError::new(
                "tmux layout node must end in a pane id, '{', or '['",
            )),
        }
    }

    fn parse_split(
        &mut self,
        opening: u8,
        closing: u8,
        axis: TmuxSplitAxis,
    ) -> RemoteResult<TmuxLayoutNode> {
        self.expect(opening)?;
        let mut children = Vec::new();
        loop {
            children.push(self.parse_node()?);
            match self.peek() {
                Some(byte) if byte == closing => {
                    self.cursor += 1;
                    break;
                }
                Some(b',') => self.cursor += 1,
                _ => {
                    return Err(RemoteError::new(
                        "tmux split layout is missing a separator or closing delimiter",
                    ));
                }
            }
        }
        if children.len() < 2 {
            return Err(RemoteError::new(
                "tmux split layout must contain at least two children",
            ));
        }
        Ok(TmuxLayoutNode::Split { axis, children })
    }

    fn parse_number(&mut self) -> RemoteResult<u64> {
        let start = self.cursor;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.cursor += 1;
        }
        if start == self.cursor {
            return Err(RemoteError::new("expected a number in tmux layout"));
        }
        std::str::from_utf8(&self.bytes[start..self.cursor])
            .expect("tmux layout digits are ASCII")
            .parse()
            .map_err(|_| RemoteError::new("tmux layout number is out of range"))
    }

    fn expect(&mut self, byte: u8) -> RemoteResult<()> {
        if self.peek() == Some(byte) {
            self.cursor += 1;
            Ok(())
        } else {
            Err(RemoteError::new(format!(
                "expected '{}' in tmux layout",
                char::from(byte)
            )))
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.cursor).copied()
    }

    fn is_finished(&self) -> bool {
        self.cursor == self.bytes.len()
    }
}

fn parse_tmux_id(value: &str, prefix: char) -> RemoteResult<u64> {
    let value = value
        .strip_prefix(prefix)
        .ok_or_else(|| RemoteError::new(format!("expected tmux id prefixed with '{prefix}'")))?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(RemoteError::new("tmux ids must contain decimal digits"));
    }
    value
        .parse()
        .map_err(|_| RemoteError::new("tmux id is out of range"))
}

fn split_tmux_fields(value: &str, fields: usize) -> RemoteResult<(&str, &str)> {
    let mut parts = value.splitn(fields, char::is_whitespace);
    let first = parts
        .next()
        .filter(|part| !part.is_empty())
        .ok_or_else(|| RemoteError::new("tmux control event is missing its first field"))?;
    let second = parts
        .next()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .ok_or_else(|| RemoteError::new("tmux control event is missing its second field"))?;
    Ok((first, second))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeTmuxWindow {
    pub remote_window_id: TmuxWindowId,
    pub name: String,
    pub layout: Option<TmuxLayoutNode>,
    pub active_pane: Option<TmuxPaneId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeTmuxLayoutModel {
    pub windows: Vec<NativeTmuxWindow>,
    pub active_window: Option<TmuxWindowId>,
    pub exited: bool,
}

#[derive(Clone, Debug, Default)]
struct TmuxWindowState {
    name: String,
    layout: Option<TmuxLayoutNode>,
    active_pane: Option<TmuxPaneId>,
}

/// State reducer for the experimental tmux bridge. It projects remote metadata
/// into a native layout model only; it does not replace, parse, or modify an
/// ordinary SSH terminal when tmux mode is disabled.
#[derive(Clone, Debug, Default)]
pub struct TmuxControlBridge {
    windows: BTreeMap<TmuxWindowId, TmuxWindowState>,
    panes: BTreeMap<TmuxPaneId, TmuxWindowId>,
    active_window: Option<TmuxWindowId>,
    exited: bool,
}

impl TmuxControlBridge {
    pub fn apply_line(&mut self, line: &str) -> RemoteResult<()> {
        if let Some(event) = TmuxControlEvent::parse(line)? {
            self.apply(event);
        }
        Ok(())
    }

    pub fn apply(&mut self, event: TmuxControlEvent) {
        match event {
            TmuxControlEvent::WindowAdded(window_id) => {
                self.windows.entry(window_id).or_default();
            }
            TmuxControlEvent::WindowClosed(window_id) => {
                self.windows.remove(&window_id);
                self.panes
                    .retain(|_, pane_window| *pane_window != window_id);
                if self.active_window == Some(window_id) {
                    self.active_window = self.windows.keys().next().copied();
                }
            }
            TmuxControlEvent::WindowRenamed { window_id, name } => {
                self.windows.entry(window_id).or_default().name = name;
            }
            TmuxControlEvent::PaneAdded { pane_id, window_id } => {
                self.windows.entry(window_id).or_default();
                self.panes.insert(pane_id, window_id);
            }
            TmuxControlEvent::PaneClosed(pane_id) => {
                if let Some(window_id) = self.panes.remove(&pane_id)
                    && let Some(window) = self.windows.get_mut(&window_id)
                    && window.active_pane == Some(pane_id)
                {
                    window.active_pane = None;
                }
            }
            TmuxControlEvent::ActiveWindowChanged(window_id) => {
                self.windows.entry(window_id).or_default();
                self.active_window = Some(window_id);
            }
            TmuxControlEvent::ActivePaneChanged { window_id, pane_id } => {
                self.windows.entry(window_id).or_default().active_pane = Some(pane_id);
                self.panes.insert(pane_id, window_id);
            }
            TmuxControlEvent::LayoutChanged { window_id, layout } => {
                self.windows.entry(window_id).or_default().layout = Some(layout);
            }
            TmuxControlEvent::Exited => self.exited = true,
        }
    }

    pub fn projection(&self) -> NativeTmuxLayoutModel {
        NativeTmuxLayoutModel {
            windows: self
                .windows
                .iter()
                .map(|(&remote_window_id, state)| NativeTmuxWindow {
                    remote_window_id,
                    name: state.name.clone(),
                    layout: state.layout.clone(),
                    active_pane: state.active_pane,
                })
                .collect(),
            active_window: self.active_window,
            exited: self.exited,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, net::TcpStream, thread, time::Duration};

    use super::*;

    fn workspace(host: &str) -> SshWorkspaceConfig {
        SshWorkspaceConfig::new(SshDestination::new(SshHost::try_from(host).unwrap()))
    }

    #[test]
    fn normal_ssh_plan_uses_user_config_agent_and_safe_defaults() {
        let config = workspace("work-prod");
        let plan = config
            .launch_plan(RemoteCapabilities::foundation(), None)
            .unwrap();

        assert_eq!(plan.program, "ssh");
        assert_eq!(plan.mode, SshSessionMode::Terminal);
        assert_eq!(plan.args.last().map(String::as_str), Some("work-prod"));
        assert!(
            plan.args
                .windows(2)
                .any(|args| args == ["-o", "ClearAllForwardings=yes"])
        );
        assert!(
            plan.args
                .windows(2)
                .any(|args| args == ["-o", "ForwardAgent=no"])
        );
        assert!(
            plan.args
                .windows(2)
                .any(|args| args == ["-o", "StrictHostKeyChecking=yes"])
        );
        assert!(
            !plan
                .args
                .iter()
                .any(|arg| arg.contains("StrictHostKeyChecking=no"))
        );
        assert!(!plan.args.iter().any(|arg| arg == "sh" || arg == "-c"));
    }

    #[test]
    fn unsafe_ssh_host_and_session_values_are_rejected_before_argv_building() {
        assert!(SshHost::try_from("-oProxyCommand=whoami").is_err());
        assert!(SshHost::try_from("host;touch/tmp/pwned").is_err());
        assert!(SshUsername::try_from("alice bob").is_err());
        assert!(TmuxSessionName::try_from("dev;rm -rf /").is_err());
    }

    #[test]
    fn forwarding_is_opt_in_and_loopback_only() {
        let mut config = workspace("devbox");
        config.port_routing = RemotePortRouting::Loopback(vec![RemotePortRoute {
            local_port: 3000,
            remote_host: RemoteTcpHost::try_from("127.0.0.1").unwrap(),
            remote_port: 3000,
            browser_surface: false,
        }]);

        assert!(
            config
                .launch_plan(RemoteCapabilities::default(), None)
                .is_err()
        );
        let plan = config
            .launch_plan(RemoteCapabilities::foundation(), None)
            .unwrap();
        assert!(
            plan.args
                .windows(2)
                .any(|args| args == ["-L", "127.0.0.1:3000:127.0.0.1:3000"])
        );
    }

    #[test]
    fn browser_route_is_gated_even_when_the_tcp_route_is_supported() {
        let mut config = workspace("devbox");
        config.port_routing = RemotePortRouting::Loopback(vec![RemotePortRoute {
            local_port: 3000,
            remote_host: RemoteTcpHost::try_from("localhost").unwrap(),
            remote_port: 3000,
            browser_surface: true,
        }]);
        assert!(
            config
                .launch_plan(RemoteCapabilities::foundation(), None)
                .unwrap_err()
                .to_string()
                .contains("browser routing")
        );
    }

    #[test]
    fn identity_is_stable_and_changes_for_another_remote_root() {
        let mut first = workspace("buildbox");
        first.destination.username = Some(SshUsername::try_from("ash").unwrap());
        first.remote_root = Some("/srv/project-a".to_owned());
        let mut second = first.clone();
        second.display_name = "different label".to_owned();
        assert_eq!(first.identity(), second.identity());

        second.remote_root = Some("/srv/project-b".to_owned());
        assert_ne!(first.identity().id, second.identity().id);
    }

    #[test]
    fn state_round_trip_is_private_and_uses_only_zmux_data() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("remote-workspaces.json");
        let mut store = RemoteWorkspaceStore::default();
        let id = store.upsert(workspace("workbox")).unwrap();
        store.save_to_path(&path).unwrap();

        let loaded = RemoteWorkspaceStore::load_from_path(&path).unwrap();
        assert_eq!(
            loaded.get(&id).unwrap().destination.host.as_str(),
            "workbox"
        );
        let contents = fs::read_to_string(path).unwrap();
        assert!(!contents.contains("Zed"));
        assert!(!contents.contains("token"));
    }

    #[cfg(unix)]
    #[test]
    fn persisted_state_and_parent_directory_are_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("nested").join("remote-workspaces.json");
        let mut store = RemoteWorkspaceStore::default();
        store.upsert(workspace("workbox")).unwrap();
        store.save_to_path(&path).unwrap();
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn reconnect_is_exponential_but_bounded_and_resets_on_success() {
        let mut reconnect = ReconnectController::new(ReconnectPolicy {
            max_attempts: 3,
            initial_delay_ms: 10,
            max_delay_ms: 25,
        })
        .unwrap();
        assert_eq!(
            reconnect.connection_lost(),
            RemoteConnectionState::Reconnecting {
                attempt: 1,
                delay: Duration::from_millis(10)
            }
        );
        assert_eq!(
            reconnect.connection_lost(),
            RemoteConnectionState::Reconnecting {
                attempt: 2,
                delay: Duration::from_millis(20)
            }
        );
        assert_eq!(
            reconnect.connection_lost(),
            RemoteConnectionState::Reconnecting {
                attempt: 3,
                delay: Duration::from_millis(25)
            }
        );
        assert_eq!(
            reconnect.connection_lost(),
            RemoteConnectionState::Exhausted { attempts: 3 }
        );
        reconnect.connected();
        assert_eq!(reconnect.attempts(), 0);
        assert_eq!(reconnect.state(), &RemoteConnectionState::Connected);
    }

    #[test]
    fn relay_authentication_binds_events_to_one_target_and_rejects_replays() {
        let grant = RemoteRelayGrant::generate(RemoteRelayTarget {
            workspace_id: 7,
            surface_id: 11,
        })
        .unwrap();
        let event = RemoteRelayEvent::Notification {
            level: NotificationLevel::Success,
            title: "build complete".to_owned(),
            body: "all checks passed".to_owned(),
        };
        let envelope = grant.sign_with_nonce(event.clone(), [4; 16]).unwrap();
        let mut verifier = grant.verifier();
        assert_eq!(verifier.verify(envelope.clone()).unwrap().event, event);
        assert!(
            verifier
                .verify(envelope)
                .unwrap_err()
                .to_string()
                .contains("replayed")
        );

        let mut wrong_target = grant
            .sign_with_nonce(
                RemoteRelayEvent::Notification {
                    level: NotificationLevel::Info,
                    title: "hello".to_owned(),
                    body: String::new(),
                },
                [5; 16],
            )
            .unwrap();
        wrong_target.target.surface_id = 12;
        assert!(
            verifier
                .verify(wrong_target)
                .unwrap_err()
                .to_string()
                .contains("different workspace or surface")
        );
    }

    #[test]
    fn relay_listener_accepts_only_authenticated_loopback_events() {
        let grant = RemoteRelayGrant::generate(RemoteRelayTarget {
            workspace_id: 2,
            surface_id: 3,
        })
        .unwrap();
        let envelope = grant
            .sign_with_nonce(
                RemoteRelayEvent::Notification {
                    level: NotificationLevel::Info,
                    title: "remote status".to_owned(),
                    body: "ready".to_owned(),
                },
                [9; 16],
            )
            .unwrap();
        let mut listener = RemoteRelayListener::bind(&grant).unwrap();
        let port = listener.local_port();
        let sender = thread::spawn(move || {
            let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            write_relay_envelope(&mut stream, &envelope).unwrap();
        });
        let received = listener.receive_one().unwrap();
        sender.join().unwrap();
        assert_eq!(received.target.surface_id, 3);
    }

    #[test]
    fn experimental_tmux_is_opt_in_and_uses_only_validated_remote_tokens() {
        let mut config = workspace("devbox");
        config.tmux =
            TmuxBridgeConfig::Experimental(TmuxSessionName::try_from("project_1").unwrap());
        let plan = config
            .launch_plan(RemoteCapabilities::foundation(), None)
            .unwrap();
        assert_eq!(plan.mode, SshSessionMode::ExperimentalTmuxControl);
        assert!(plan.args.windows(2).any(|args| args == ["tmux", "-CC"]));
        assert!(plan.args.windows(2).any(|args| args == ["-s", "project_1"]));
        let pty = plan.args.iter().position(|arg| arg == "-tt").unwrap();
        let host = plan.args.iter().position(|arg| arg == "devbox").unwrap();
        assert!(pty < host, "SSH options must precede the destination");
    }

    #[test]
    fn tmux_layout_parser_projects_horizontal_and_vertical_splits() {
        let horizontal = parse_tmux_layout("8205,80x24,0,0{40x24,0,0,0,39x24,41,0,1}").unwrap();
        assert_eq!(
            horizontal,
            TmuxLayoutNode::Split {
                axis: TmuxSplitAxis::Horizontal,
                children: vec![TmuxLayoutNode::Pane(0), TmuxLayoutNode::Pane(1)]
            }
        );
        let vertical = parse_tmux_layout("80x24,0,0[80x12,0,0,0,80x11,0,13,1]").unwrap();
        assert_eq!(
            vertical,
            TmuxLayoutNode::Split {
                axis: TmuxSplitAxis::Vertical,
                children: vec![TmuxLayoutNode::Pane(0), TmuxLayoutNode::Pane(1)]
            }
        );
    }

    #[test]
    fn tmux_bridge_projects_remote_metadata_without_interpreting_output() {
        let mut bridge = TmuxControlBridge::default();
        bridge.apply_line("%window-add @4").unwrap();
        bridge.apply_line("%window-renamed @4 build").unwrap();
        bridge.apply_line("%pane-add %8 @4").unwrap();
        bridge.apply_line("%pane-add %9 @4").unwrap();
        bridge
            .apply_line("%layout-change @4 80x24,0,0{40x24,0,0,8,39x24,41,0,9} 80x24,0,0{40x24,0,0,8,39x24,41,0,9}")
            .unwrap();
        bridge.apply_line("%session-window-changed $0 @4").unwrap();
        bridge
            .apply_line("%output %8 arbitrary terminal output")
            .unwrap();

        let projection = bridge.projection();
        assert_eq!(projection.active_window, Some(4));
        assert_eq!(projection.windows.len(), 1);
        assert_eq!(projection.windows[0].name, "build");
        assert_eq!(
            projection.windows[0].layout,
            Some(TmuxLayoutNode::Split {
                axis: TmuxSplitAxis::Horizontal,
                children: vec![TmuxLayoutNode::Pane(8), TmuxLayoutNode::Pane(9)]
            })
        );
    }

    #[test]
    fn malformed_tmux_layout_and_unknown_control_events_are_safe() {
        assert!(parse_tmux_layout("80x24,0,0{bad}").is_err());
        assert_eq!(
            TmuxControlEvent::parse("%subscription-changed x").unwrap(),
            None
        );
    }
}
