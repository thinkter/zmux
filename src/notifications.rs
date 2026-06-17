//! Agent notification store.
//!
//! Zmux keeps a global list of attention requests emitted by terminal panes
//! (manually for now, later via OSC escape sequences). Each notification is
//! attached to the terminal item that produced it and to the Zmux workspace
//! that was active at the time. The sidebar uses this store to render unread
//! badges and to jump to the most recent unread request.

use std::time::Instant;

use gpui::{App, EntityId, Global};

/// Zmux workspace identifier. Re-exported here so the notification model and
/// the workspace sidebar share the same type without a circular module import.
pub type WorkspaceId = u64;

/// Where a notification came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationSource {
    Manual,
    Osc9,
    Osc99,
    Osc777,
    Cli,
}

/// A single attention request from a terminal/agent.
#[derive(Clone)]
pub struct Notification {
    pub id: usize,
    pub item_id: EntityId,
    pub workspace_id: Option<WorkspaceId>,
    pub title: String,
    pub body: String,
    pub source: NotificationSource,
    pub read: bool,
    pub created_at: Instant,
}

/// App-global store for notifications.
pub struct NotificationStore {
    notifications: Vec<Notification>,
    next_id: usize,
}

impl Global for NotificationStore {}

impl NotificationStore {
    pub fn new() -> Self {
        Self {
            notifications: Vec::new(),
            next_id: 1,
        }
    }

    pub fn global(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    pub fn global_mut(cx: &mut App) -> &mut Self {
        cx.global_mut::<Self>()
    }

    /// Add a new unread notification and return a reference to it.
    pub fn add(
        &mut self,
        item_id: EntityId,
        workspace_id: Option<WorkspaceId>,
        source: NotificationSource,
        title: String,
        body: String,
    ) -> &Notification {
        let id = self.next_id;
        self.next_id += 1;
        self.notifications.push(Notification {
            id,
            item_id,
            workspace_id,
            source,
            title,
            body,
            read: false,
            created_at: Instant::now(),
        });
        self.notifications.last().unwrap()
    }

    pub fn pane_has_unread(&self, item_id: EntityId) -> bool {
        self.notifications
            .iter()
            .any(|n| n.item_id == item_id && !n.read)
    }

    pub fn workspace_has_unread(&self, workspace_id: WorkspaceId) -> bool {
        self.notifications
            .iter()
            .any(|n| n.workspace_id == Some(workspace_id) && !n.read)
    }

    pub fn latest_unread(&self) -> Option<&Notification> {
        self.notifications.iter().rfind(|n| !n.read)
    }

    pub fn unread_count(&self) -> usize {
        self.notifications.iter().filter(|n| !n.read).count()
    }

    pub fn workspace_unread_count(&self, workspace_id: WorkspaceId) -> usize {
        self.notifications
            .iter()
            .filter(|n| n.workspace_id == Some(workspace_id) && !n.read)
            .count()
    }

    pub fn mark_pane_read(&mut self, item_id: EntityId) {
        for notification in self
            .notifications
            .iter_mut()
            .filter(|n| n.item_id == item_id && !n.read)
        {
            notification.read = true;
        }
    }

    pub fn mark_workspace_read(&mut self, workspace_id: WorkspaceId) {
        for notification in self
            .notifications
            .iter_mut()
            .filter(|n| n.workspace_id == Some(workspace_id) && !n.read)
        {
            notification.read = true;
        }
    }

    pub fn mark_all_read(&mut self) {
        for notification in self.notifications.iter_mut().filter(|n| !n.read) {
            notification.read = true;
        }
    }
}

impl Default for NotificationStore {
    fn default() -> Self {
        Self::new()
    }
}
