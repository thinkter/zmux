//! Agent notification store.
//!
//! Zmux keeps a global list of attention requests emitted by terminal panes
//! (manually for now, later via OSC escape sequences). Each notification is
//! attached to the terminal item that produced it and to the Zmux workspace
//! that was active at the time. The sidebar uses this store to render unread
//! badges and to jump to the most recent unread request.

use std::time::Instant;

use gpui::{App, EntityId, Global};
use serde::{Deserialize, Serialize};

/// Zmux workspace identifier. Re-exported here so the notification model and
/// the workspace sidebar share the same type without a circular module import.
pub type WorkspaceId = u64;

/// Stable identifier used by the control plane. IDs never repeat within a
/// process even after older notifications are trimmed from the bounded history.
pub type NotificationId = u64;

/// The in-memory history is deliberately bounded. The UI shows the most recent
/// notifications while clients can acknowledge or clear entries explicitly.
pub const DEFAULT_NOTIFICATION_CAPACITY: usize = 500;

/// Where a notification came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationSource {
    #[default]
    Manual,
    Osc9,
    Osc99,
    Osc777,
    Cli,
    AgentHook,
}

/// A semantic level keeps UI rendering, CLI output, and native delivery policy
/// separate from the original terminal protocol.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationLevel {
    #[default]
    Info,
    Success,
    Warning,
    Error,
}

/// A single attention request from a terminal/agent.
#[derive(Clone)]
pub struct Notification {
    pub id: NotificationId,
    pub item_id: EntityId,
    pub workspace_id: Option<WorkspaceId>,
    pub title: String,
    pub body: String,
    pub source: NotificationSource,
    pub level: NotificationLevel,
    pub read: bool,
    pub created_at: Instant,
}

/// App-global store for notifications.
pub struct NotificationStore {
    notifications: Vec<Notification>,
    next_id: NotificationId,
    capacity: usize,
}

impl Global for NotificationStore {}

impl NotificationStore {
    pub fn new() -> Self {
        Self {
            notifications: Vec::new(),
            next_id: 1,
            capacity: DEFAULT_NOTIFICATION_CAPACITY,
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
        self.add_with_level(
            item_id,
            workspace_id,
            source,
            NotificationLevel::Info,
            title,
            body,
        )
    }

    /// Add a notification with a semantic delivery level. This is the single
    /// retention path so all origins (OSC, CLI, hooks, and UI actions) are
    /// bounded consistently.
    pub fn add_with_level(
        &mut self,
        item_id: EntityId,
        workspace_id: Option<WorkspaceId>,
        source: NotificationSource,
        level: NotificationLevel,
        title: String,
        body: String,
    ) -> &Notification {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.notifications.push(Notification {
            id,
            item_id,
            workspace_id,
            source,
            level,
            title,
            body,
            read: false,
            created_at: Instant::now(),
        });
        self.trim_to_capacity();
        self.notifications.last().unwrap()
    }

    /// Override the bounded history size. A value of zero is treated as one so
    /// callers cannot create a store that panics after accepting an event.
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity.max(1);
        self.trim_to_capacity();
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn notifications(&self) -> impl DoubleEndedIterator<Item = &Notification> {
        self.notifications.iter()
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

    pub fn latest_unread_for_workspace(&self, workspace_id: WorkspaceId) -> Option<&Notification> {
        self.notifications.iter().rfind(|notification| {
            notification.workspace_id == Some(workspace_id) && !notification.read
        })
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

    /// Mark one event as acknowledged. Returns false when it has already been
    /// evicted or does not exist, allowing the control API to return a typed
    /// not-found error rather than silently succeeding.
    pub fn acknowledge(&mut self, id: NotificationId) -> bool {
        let Some(notification) = self.notifications.iter_mut().find(|n| n.id == id) else {
            return false;
        };
        notification.read = true;
        true
    }

    /// Remove every notification associated with a workspace that was closed.
    ///
    /// Workspace identifiers are deliberately never recycled, but clearing the
    /// entries is still important: it releases references to now-closed panes
    /// and makes the close operation an explicit acknowledgement boundary rather
    /// than letting stale unread state accumulate indefinitely. This also
    /// prevents a future workspace from inheriting stale unread state.
    pub fn clear_workspace(&mut self, workspace_id: WorkspaceId) -> usize {
        let before = self.notifications.len();
        self.notifications
            .retain(|notification| notification.workspace_id != Some(workspace_id));
        before - self.notifications.len()
    }
    pub fn clear_all(&mut self) -> usize {
        let count = self.notifications.len();
        self.notifications.clear();
        count
    }

    fn trim_to_capacity(&mut self) {
        let excess = self.notifications.len().saturating_sub(self.capacity);
        if excess > 0 {
            self.notifications.drain(..excess);
        }
    }
}

impl Default for NotificationStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item_id(value: usize) -> EntityId {
        // Entity IDs are opaque to application code. Tests only need distinct
        // values and this conversion is the representation used by GPUI.
        EntityId::from(value as u64)
    }

    #[test]
    fn history_is_bounded_but_notification_ids_are_not_reused() {
        let mut store = NotificationStore::new();
        store.set_capacity(2);
        for index in 0..3 {
            store.add(
                item_id(index),
                Some(1),
                NotificationSource::Cli,
                format!("title {index}"),
                String::new(),
            );
        }

        let ids: Vec<_> = store
            .notifications()
            .map(|notification| notification.id)
            .collect();
        assert_eq!(ids, vec![2, 3]);
        assert!(!store.acknowledge(1));
        assert!(store.acknowledge(3));
    }

    #[test]
    fn clearing_a_workspace_does_not_affect_another_workspace() {
        let mut store = NotificationStore::new();
        store.add(
            item_id(1),
            Some(1),
            NotificationSource::Osc9,
            "one".to_string(),
            String::new(),
        );
        store.add(
            item_id(2),
            Some(2),
            NotificationSource::Osc99,
            "two".to_string(),
            String::new(),
        );

        assert_eq!(store.clear_workspace(1), 1);
        assert!(!store.workspace_has_unread(1));
        assert!(store.workspace_has_unread(2));
        assert_eq!(store.unread_count(), 1);
    }
}
