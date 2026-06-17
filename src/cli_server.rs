//! CLI server for external agent/tools to send notifications to the running
//! zmux instance.
//!
//! A Unix domain socket is created at `~/.local/share/zmux/zmux.sock`.
//! Other processes can connect and send a JSON `CliNotification`. The server
//! attaches the notification to the currently focused terminal pane/workspace.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use async_channel::{Receiver, Sender, unbounded};
use gpui::{App, AsyncApp, Global, Task, WeakEntity};
use serde::{Deserialize, Serialize};

use crate::notifications::{NotificationSource, NotificationStore};
use crate::workspaces::WorkspacesPanel;
use workspace::Workspace;

#[derive(Serialize, Deserialize)]
pub struct CliNotification {
    pub title: String,
    pub body: String,
}

pub struct CliServer {
    socket_path: PathBuf,
    _task: Task<()>,
}

impl Global for CliServer {}

impl Drop for CliServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

impl CliServer {
    /// Start listening for CLI notifications.
    pub fn start(
        workspace: WeakEntity<Workspace>,
        panel: WeakEntity<WorkspacesPanel>,
        cx: &mut App,
    ) -> Self {
        let socket_path = Self::socket_path();
        let parent = socket_path.parent().expect("socket path has a parent");
        std::fs::create_dir_all(parent).ok();
        let _ = std::fs::remove_file(&socket_path);

        let listener = UnixListener::bind(&socket_path).expect("failed to bind zmux CLI socket");
        let (tx, rx) = unbounded::<CliNotification>();

        thread::spawn(move || Self::accept_loop(listener, tx));

        let _task = cx.spawn(async move |cx| {
            Self::process_loop(rx, workspace, panel, cx).await;
        });

        Self {
            socket_path,
            _task,
        }
    }

    /// Path to the Unix socket used by the CLI.
    pub fn socket_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        PathBuf::from(home)
            .join(".local/share/zmux")
            .join("zmux.sock")
    }

    fn accept_loop(listener: UnixListener, tx: Sender<CliNotification>) {
        for stream in listener.incoming() {
            match stream {
                Ok(mut stream) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .ok();
                    let mut buf = Vec::new();
                    if stream.read_to_end(&mut buf).is_ok()
                        && let Ok(msg) = serde_json::from_slice::<CliNotification>(&buf)
                    {
                        let _ = tx.try_send(msg);
                    }
                }
                Err(_) => break,
            }
        }
    }

    async fn process_loop(
        rx: Receiver<CliNotification>,
        workspace: WeakEntity<Workspace>,
        panel: WeakEntity<WorkspacesPanel>,
        cx: &mut AsyncApp,
    ) {
        while let Ok(msg) = rx.recv().await {
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

    /// Send a notification to the running zmux instance and return immediately.
    pub fn notify(title: String, body: String) -> anyhow::Result<()> {
        let socket_path = Self::socket_path();
        let mut stream = UnixStream::connect(&socket_path)?;
        let msg = CliNotification { title, body };
        stream.write_all(&serde_json::to_vec(&msg)?)?;
        Ok(())
    }
}
