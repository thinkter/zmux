//! Local IPC server for notifications sent by processes running inside zmux.
//!
//! Each zmux instance creates its own endpoint in a private temporary
//! directory. The endpoint is passed to terminal children through
//! [`NOTIFICATION_ENDPOINT_ENV`], so a child can route a notification back to
//! the exact zmux instance that created it. There is deliberately no shared
//! socket path to unlink or steal from another running instance.

use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

use anyhow::Context as _;
use async_channel::{Receiver, Sender, unbounded};
use gpui::{App, AsyncApp, Global, Task, WeakEntity};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

use crate::{
    ipc::{LocalIpcTransport, PlatformLocalIpc},
    notifications::{NotificationSource, NotificationStore},
    workspaces::WorkspacesPanel,
};
use workspace::Workspace;

/// Environment variable that identifies the notification endpoint owned by
/// the zmux instance that launched a terminal child process.
pub const NOTIFICATION_ENDPOINT_ENV: &str = "ZMUX_NOTIFY_ENDPOINT";

/// Reject unexpectedly large payloads before allocating their declared size.
const MAX_NOTIFICATION_BYTES: usize = 64 * 1024;

/// A notification supplied by `zmux notify`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct CliNotification {
    pub title: String,
    pub body: String,
}

/// A capability-style endpoint for one running zmux instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationEndpoint {
    path: PathBuf,
    value: String,
}

impl NotificationEndpoint {
    fn from_path(path: PathBuf) -> anyhow::Result<Self> {
        let value = path
            .to_str()
            .context("notification endpoint path is not valid Unicode")?
            .to_owned();
        Ok(Self { path, value })
    }

    fn from_environment() -> anyhow::Result<Self> {
        let value = std::env::var(NOTIFICATION_ENDPOINT_ENV).with_context(|| {
            format!(
                "{NOTIFICATION_ENDPOINT_ENV} is not set; run `zmux notify` from a terminal started by zmux"
            )
        })?;
        anyhow::ensure!(
            !value.is_empty(),
            "{NOTIFICATION_ENDPOINT_ENV} is empty; run `zmux notify` from a terminal started by zmux"
        );

        Ok(Self {
            path: PathBuf::from(&value),
            value,
        })
    }

    /// Value to give to child processes in [`NOTIFICATION_ENDPOINT_ENV`].
    pub fn as_str(&self) -> &str {
        &self.value
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

/// Owns the private directory that scopes a single IPC endpoint.
///
/// `TempDir` uses a unique directory for every call and removes it when the
/// lease is dropped. On Unix we additionally force owner-only directory and
/// socket permissions. On Windows, Zed's `net` transport uses Winsock AF_UNIX;
/// the endpoint is still local and protected by the current user's temporary
/// directory ACL, with its unguessable directory name acting as a capability
/// passed only to child processes.
struct EndpointLease {
    _directory: TempDir,
    endpoint: NotificationEndpoint,
}

impl EndpointLease {
    fn create() -> anyhow::Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("zmux-notify-")
            .tempdir()
            .context("creating private directory for the zmux notification endpoint")?;
        restrict_directory_permissions(directory.path())
            .context("restricting zmux notification directory permissions")?;

        let endpoint = NotificationEndpoint::from_path(directory.path().join("notify.sock"))?;
        Ok(Self {
            _directory: directory,
            endpoint,
        })
    }
}

#[cfg(unix)]
fn restrict_directory_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn restrict_directory_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn restrict_endpoint_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_endpoint_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// The lifecycle owner for zmux's notification endpoint and message bridge.
pub struct CliServer {
    endpoint_lease: EndpointLease,
    receiver: Option<Receiver<CliNotification>>,
    running: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
    _task: Option<Task<()>>,
}

impl Global for CliServer {}

impl CliServer {
    /// Bind a private endpoint before creating terminal children.
    ///
    /// The server is prepared separately from [`Self::start`] so its endpoint
    /// can be included in the environment passed to the workspace's first
    /// terminal. Failures are returned to the caller instead of panicking.
    pub fn prepare() -> anyhow::Result<Self> {
        let endpoint_lease = EndpointLease::create()?;
        let endpoint = endpoint_lease.endpoint.clone();
        let listener = PlatformLocalIpc::bind(endpoint.path()).with_context(|| {
            format!(
                "binding zmux notification endpoint at {}",
                endpoint.path().display()
            )
        })?;
        restrict_endpoint_permissions(endpoint.path()).with_context(|| {
            format!(
                "restricting zmux notification endpoint permissions at {}",
                endpoint.path().display()
            )
        })?;

        let (sender, receiver) = unbounded::<CliNotification>();
        let running = Arc::new(AtomicBool::new(true));
        let accept_running = running.clone();
        let accept_thread = thread::Builder::new()
            .name("zmux-cli-notify".to_owned())
            .spawn(move || Self::accept_loop(listener, sender, accept_running))
            .context("starting zmux notification listener thread")?;

        Ok(Self {
            endpoint_lease,
            receiver: Some(receiver),
            running,
            accept_thread: Some(accept_thread),
            _task: None,
        })
    }

    /// Start forwarding received notifications into the currently focused
    /// terminal pane.
    pub fn start(
        mut self,
        workspace: WeakEntity<Workspace>,
        panel: WeakEntity<WorkspacesPanel>,
        cx: &mut App,
    ) -> Self {
        let Some(receiver) = self.receiver.take() else {
            eprintln!("zmux notification server was started more than once");
            return self;
        };

        self._task = Some(cx.spawn(async move |cx| {
            Self::process_loop(receiver, workspace, panel, cx).await;
        }));
        self
    }

    /// Endpoint advertised to terminal children.
    pub fn endpoint(&self) -> &NotificationEndpoint {
        &self.endpoint_lease.endpoint
    }

    /// Send a notification to the zmux instance identified by the terminal's
    /// [`NOTIFICATION_ENDPOINT_ENV`] value.
    pub fn notify(title: String, body: String) -> anyhow::Result<()> {
        let endpoint = NotificationEndpoint::from_environment()?;
        Self::send_to_endpoint(&endpoint, &CliNotification { title, body })
    }

    fn send_to_endpoint(
        endpoint: &NotificationEndpoint,
        notification: &CliNotification,
    ) -> anyhow::Result<()> {
        let mut stream = PlatformLocalIpc::connect(endpoint.path()).with_context(|| {
            format!(
                "connecting to zmux notification endpoint at {}",
                endpoint.path().display()
            )
        })?;
        write_notification(&mut stream, notification)?;
        stream
            .flush()
            .context("flushing notification to the zmux endpoint")?;
        Ok(())
    }

    fn accept_loop(
        listener: <PlatformLocalIpc as LocalIpcTransport>::Listener,
        sender: Sender<CliNotification>,
        running: Arc<AtomicBool>,
    ) {
        while running.load(Ordering::Acquire) {
            match PlatformLocalIpc::accept(&listener) {
                Ok(stream) => {
                    if !running.load(Ordering::Acquire) {
                        break;
                    }

                    let client_sender = sender.clone();
                    let client_running = running.clone();
                    if let Err(error) = thread::Builder::new()
                        .name("zmux-cli-notify-client".to_owned())
                        .spawn(move || Self::read_client(stream, client_sender, client_running))
                    {
                        eprintln!("failed to start zmux notification client handler: {error}");
                    }
                }
                Err(error) => {
                    if running.load(Ordering::Acquire) {
                        eprintln!("zmux notification listener stopped: {error}");
                    }
                    break;
                }
            }
        }
    }

    fn read_client(
        mut stream: <PlatformLocalIpc as LocalIpcTransport>::Stream,
        sender: Sender<CliNotification>,
        running: Arc<AtomicBool>,
    ) {
        match read_notification(&mut stream) {
            Ok(notification) if running.load(Ordering::Acquire) => {
                if sender.try_send(notification).is_err() {
                    eprintln!("zmux notification receiver is no longer available");
                }
            }
            Ok(_) => {}
            Err(error) => eprintln!("discarding invalid zmux notification: {error:#}"),
        }
    }

    async fn process_loop(
        receiver: Receiver<CliNotification>,
        workspace: WeakEntity<Workspace>,
        panel: WeakEntity<WorkspacesPanel>,
        cx: &mut AsyncApp,
    ) {
        while let Ok(msg) = receiver.recv().await {
            let Some(workspace) = workspace.upgrade() else {
                continue;
            };
            let Some(panel) = panel.upgrade() else {
                continue;
            };

            cx.update(|cx| {
                workspace.update(cx, |workspace, cx| {
                    let active_pane = workspace.active_pane().clone();
                    let Some(item) = active_pane.read(cx).active_item() else {
                        return;
                    };
                    if item.act_as::<terminal_view::TerminalView>(cx).is_none() {
                        return;
                    }
                    let item_id = item.item_id();
                    let workspace_id = panel.read(cx).active_workspace_id();

                    NotificationStore::global_mut(cx).add(
                        item_id,
                        Some(workspace_id),
                        NotificationSource::Cli,
                        msg.title,
                        msg.body,
                    );
                    panel.update(cx, |_, cx| cx.notify());
                });
            });
        }
    }
}

impl Drop for CliServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);

        // Wake a blocking accept so the listener can drop before its private
        // directory is removed. This only ever targets this instance's unique
        // endpoint, so it cannot affect another running zmux instance.
        let _ = PlatformLocalIpc::connect(self.endpoint().path());
        if let Some(accept_thread) = self.accept_thread.take()
            && accept_thread.join().is_err()
        {
            eprintln!("zmux notification listener thread panicked during shutdown");
        }

        if let Err(error) = fs::remove_file(self.endpoint().path())
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!("failed to remove zmux notification endpoint: {error}");
        }
    }
}

/// Write a single framed JSON notification to a local IPC stream.
fn write_notification<W: Write>(
    stream: &mut W,
    notification: &CliNotification,
) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(notification).context("serializing zmux notification")?;
    anyhow::ensure!(
        bytes.len() <= MAX_NOTIFICATION_BYTES,
        "notification is too large (maximum is {MAX_NOTIFICATION_BYTES} bytes)"
    );

    let length =
        u32::try_from(bytes.len()).context("notification length exceeds protocol limit")?;
    stream
        .write_all(&length.to_be_bytes())
        .context("writing zmux notification length")?;
    stream
        .write_all(&bytes)
        .context("writing zmux notification payload")?;
    Ok(())
}

/// Read one framed JSON notification from a local IPC stream.
fn read_notification<R: Read>(stream: &mut R) -> anyhow::Result<CliNotification> {
    let mut length = [0_u8; std::mem::size_of::<u32>()];
    stream
        .read_exact(&mut length)
        .context("reading zmux notification length")?;
    let payload_len = u32::from_be_bytes(length) as usize;
    anyhow::ensure!(
        payload_len <= MAX_NOTIFICATION_BYTES,
        "notification payload exceeds {MAX_NOTIFICATION_BYTES} bytes"
    );

    let mut payload = vec![0; payload_len];
    stream
        .read_exact(&mut payload)
        .context("reading zmux notification payload")?;
    serde_json::from_slice(&payload).context("decoding zmux notification JSON")
}

#[cfg(test)]
mod tests {
    use std::{
        io::Cursor,
        thread,
        time::{Duration, Instant},
    };

    use super::*;

    #[test]
    fn notification_protocol_round_trips() {
        let expected = CliNotification {
            title: "Build complete".to_owned(),
            body: "All checks passed".to_owned(),
        };
        let mut encoded = Vec::new();
        write_notification(&mut encoded, &expected).unwrap();

        let actual = read_notification(&mut Cursor::new(encoded)).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn notification_protocol_rejects_oversized_payload_before_allocating() {
        let encoded = (MAX_NOTIFICATION_BYTES as u32 + 1).to_be_bytes();
        let error = read_notification(&mut Cursor::new(encoded)).unwrap_err();

        assert!(error.to_string().contains("notification payload exceeds"));
    }

    #[test]
    fn endpoints_are_unique_and_drop_only_their_own_endpoint() {
        let first = CliServer::prepare().unwrap();
        let second = CliServer::prepare().unwrap();
        let first_path = first.endpoint().path().to_owned();
        let second_path = second.endpoint().path().to_owned();

        assert_ne!(first.endpoint(), second.endpoint());
        assert!(first_path.exists());
        assert!(second_path.exists());

        drop(first);

        assert!(!first_path.exists());
        assert!(second_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn endpoint_and_parent_directory_are_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let server = CliServer::prepare().unwrap();
        let endpoint = server.endpoint().path();
        let directory = endpoint.parent().unwrap();

        assert_eq!(
            fs::metadata(directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(endpoint).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn server_accepts_framed_notification_at_its_advertised_endpoint() {
        let server = CliServer::prepare().unwrap();
        let expected = CliNotification {
            title: "Task ready".to_owned(),
            body: "Review the result".to_owned(),
        };
        CliServer::send_to_endpoint(server.endpoint(), &expected).unwrap();

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match server
                .receiver
                .as_ref()
                .expect("prepared server keeps its receiver")
                .try_recv()
            {
                Ok(actual) => {
                    assert_eq!(actual, expected);
                    break;
                }
                Err(async_channel::TryRecvError::Empty) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("notification was not delivered: {error}"),
            }
        }
    }
}
