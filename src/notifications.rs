//! Source-aware, bounded notification history.
//!
//! The store is intentionally independent from OSC, IPC, desktop delivery,
//! and navigation. Every ingress path submits the same [`NotificationRequest`]
//! and the runtime decides which side effects to perform. Keeping the history
//! as a GPUI entity gives every window a single observable source of truth.

use std::{collections::VecDeque, time::SystemTime};

use gpui::{App, AppContext, Entity, EntityId, Global};
use serde::{Deserialize, Serialize};

/// Logical workspace identifier used by zmux's workspace sidebar.
pub type WorkspaceId = u64;

/// Stable notification identifier. IDs are never recycled within a process.
pub type NotificationId = u64;

/// Monotonic order assigned on every record operation, including an update
/// that deliberately reuses a named Kitty notification's stable ID.
pub type NotificationSequence = u64;

/// Maximum number of current notification rows retained in memory.
pub const DEFAULT_NOTIFICATION_CAPACITY: usize = 500;

/// Text accepted from terminals and local IPC is bounded independently from
/// the wire frame so a single row cannot dominate rendering or native APIs.
pub const MAX_NOTIFICATION_TEXT_CHARS: usize = 4_096;

/// Identifies the exact window/sidebar workspace and terminal item that owns a
/// notification. `scope_id` disambiguates logical workspace IDs across windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NotificationTarget {
    pub scope_id: EntityId,
    pub workspace_id: WorkspaceId,
    pub item_id: EntityId,
}

/// Where a notification originated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationSource {
    #[default]
    Manual,
    Osc9,
    Osc99,
    Osc777,
    Cli,
}

impl NotificationSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Manual => "Manual",
            Self::Osc9 => "OSC 9",
            Self::Osc99 => "OSC 99",
            Self::Osc777 => "OSC 777",
            Self::Cli => "CLI",
        }
    }
}

/// Semantic severity shared by in-app and native presentation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationLevel {
    #[default]
    Info,
    Success,
    Warning,
    Error,
}

/// Replacement identity for a canonical notification row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum NotificationIdentity {
    /// cmux-style projection: the newest non-Kitty event replaces every row
    /// currently associated with the exact terminal target.
    #[default]
    Target,
    /// Kitty's explicit `i` value updates only the matching named row.
    KittyNamed(String),
    /// Kitty notifications without `i` are always distinct.
    Unique,
}

/// Canonical input accepted from all notification transports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationRequest {
    pub target: NotificationTarget,
    pub source: NotificationSource,
    pub level: NotificationLevel,
    pub identity: NotificationIdentity,
    pub title: String,
    pub subtitle: String,
    pub body: String,
}

impl NotificationRequest {
    pub fn new(
        target: NotificationTarget,
        source: NotificationSource,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            target,
            source,
            level: NotificationLevel::Info,
            identity: NotificationIdentity::Target,
            title: title.into(),
            subtitle: String::new(),
            body: body.into(),
        }
    }
}

/// A single current notification row.
#[derive(Clone, Debug)]
pub struct Notification {
    pub id: NotificationId,
    pub sequence: NotificationSequence,
    pub target: NotificationTarget,
    pub title: String,
    pub subtitle: String,
    pub body: String,
    pub source: NotificationSource,
    pub level: NotificationLevel,
    pub identity: NotificationIdentity,
    pub read: bool,
    pub created_at: SystemTime,
}

/// Result of recording a row, including every row that became unreachable due
/// to target supersession or capacity eviction.
#[derive(Clone, Debug)]
pub struct RecordOutcome {
    pub notification: Notification,
    pub removed: Vec<Notification>,
}

/// Observable app-global notification history.
pub struct NotificationStore {
    notifications: VecDeque<Notification>,
    next_id: NotificationId,
    next_sequence: NotificationSequence,
    capacity: usize,
}

/// GPUI globals are values, while observation requires an entity. This small
/// wrapper keeps the entity itself app-global.
#[derive(Clone)]
struct GlobalNotificationStore(Entity<NotificationStore>);

impl Global for GlobalNotificationStore {}

impl NotificationStore {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_NOTIFICATION_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            notifications: VecDeque::new(),
            next_id: 1,
            next_sequence: 1,
            capacity: capacity.max(1),
        }
    }

    pub fn init(cx: &mut App) -> Entity<Self> {
        if let Some(store) = cx.try_global::<GlobalNotificationStore>() {
            return store.0.clone();
        }

        let store = cx.new(|_| Self::new());
        cx.set_global(GlobalNotificationStore(store.clone()));
        store
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalNotificationStore>().0.clone()
    }

    /// Record the newest notification using its declared replacement identity.
    ///
    /// cmux treats the history as one current row per surface. Replacing the
    /// older row prevents agent notification storms from growing unread counts
    /// while preserving explicit read rows for every other pane.
    pub fn record(&mut self, request: NotificationRequest) -> RecordOutcome {
        let mut removed = Vec::new();
        let reused_id = match &request.identity {
            NotificationIdentity::Target => {
                let mut retained = VecDeque::with_capacity(self.notifications.len());
                while let Some(notification) = self.notifications.pop_front() {
                    if notification.target == request.target {
                        removed.push(notification);
                    } else {
                        retained.push_back(notification);
                    }
                }
                self.notifications = retained;
                None
            }
            NotificationIdentity::KittyNamed(identifier) => self
                .notifications
                .iter()
                .position(|notification| {
                    notification.target == request.target
                        && matches!(
                            &notification.identity,
                            NotificationIdentity::KittyNamed(candidate) if candidate == identifier
                        )
                })
                .and_then(|index| self.notifications.remove(index))
                .map(|notification| notification.id),
            NotificationIdentity::Unique => None,
        };

        let id = reused_id.unwrap_or_else(|| {
            let id = self.next_id;
            self.next_id = self
                .next_id
                .checked_add(1)
                .expect("notification ID space exhausted");
            id
        });
        let sequence = self.next_sequence;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .expect("notification sequence space exhausted");

        let notification = Notification {
            id,
            sequence,
            target: request.target,
            source: request.source,
            level: request.level,
            identity: request.identity,
            title: bounded_text(&request.title),
            subtitle: bounded_text(&request.subtitle),
            body: bounded_text(&request.body),
            read: false,
            created_at: SystemTime::now(),
        };
        self.notifications.push_front(notification.clone());
        removed.extend(self.trim_to_capacity());
        RecordOutcome {
            notification,
            removed,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn set_capacity(&mut self, capacity: usize) -> Vec<Notification> {
        self.capacity = capacity.max(1);
        self.trim_to_capacity()
    }

    pub fn notifications(&self) -> impl DoubleEndedIterator<Item = &Notification> {
        self.notifications.iter()
    }

    pub fn get(&self, id: NotificationId) -> Option<&Notification> {
        self.notifications
            .iter()
            .find(|notification| notification.id == id)
    }

    /// Highest record sequence assigned so far, used as an ordering watermark
    /// when GPUI focus events must defer their store mutation.
    pub fn newest_recorded_sequence(&self) -> NotificationSequence {
        self.next_sequence.saturating_sub(1)
    }

    pub fn latest_unread(&self) -> Option<&Notification> {
        self.notifications
            .iter()
            .find(|notification| !notification.read)
    }

    pub fn unread_count(&self) -> usize {
        self.notifications
            .iter()
            .filter(|notification| !notification.read)
            .count()
    }

    pub fn scope_unread_count(&self, scope_id: EntityId) -> usize {
        self.notifications
            .iter()
            .filter(|notification| notification.target.scope_id == scope_id && !notification.read)
            .count()
    }

    pub fn pane_has_unread(&self, scope_id: EntityId, item_id: EntityId) -> bool {
        self.notifications.iter().any(|notification| {
            notification.target.scope_id == scope_id
                && notification.target.item_id == item_id
                && !notification.read
        })
    }

    pub fn workspace_has_unread(&self, scope_id: EntityId, workspace_id: WorkspaceId) -> bool {
        self.notifications.iter().any(|notification| {
            notification.target.scope_id == scope_id
                && notification.target.workspace_id == workspace_id
                && !notification.read
        })
    }

    pub fn workspace_unread_count(&self, scope_id: EntityId, workspace_id: WorkspaceId) -> usize {
        self.notifications
            .iter()
            .filter(|notification| {
                notification.target.scope_id == scope_id
                    && notification.target.workspace_id == workspace_id
                    && !notification.read
            })
            .count()
    }

    pub fn mark_read(&mut self, id: NotificationId) -> bool {
        let Some(notification) = self
            .notifications
            .iter_mut()
            .find(|notification| notification.id == id)
        else {
            return false;
        };
        let changed = !notification.read;
        notification.read = true;
        changed
    }

    pub fn mark_unread(&mut self, id: NotificationId) -> bool {
        let Some(notification) = self
            .notifications
            .iter_mut()
            .find(|notification| notification.id == id)
        else {
            return false;
        };
        let changed = notification.read;
        notification.read = false;
        changed
    }

    pub fn mark_pane_read(&mut self, scope_id: EntityId, item_id: EntityId) -> usize {
        self.mark_pane_read_through(scope_id, item_id, NotificationId::MAX)
    }

    pub fn mark_pane_read_through(
        &mut self,
        scope_id: EntityId,
        item_id: EntityId,
        max_sequence: NotificationSequence,
    ) -> usize {
        let mut changed = 0;
        for notification in self.notifications.iter_mut().filter(|notification| {
            notification.target.scope_id == scope_id
                && notification.target.item_id == item_id
                && notification.sequence <= max_sequence
                && !notification.read
        }) {
            notification.read = true;
            changed += 1;
        }
        changed
    }

    pub fn mark_workspace_read(&mut self, scope_id: EntityId, workspace_id: WorkspaceId) -> usize {
        let mut changed = 0;
        for notification in self.notifications.iter_mut().filter(|notification| {
            notification.target.scope_id == scope_id
                && notification.target.workspace_id == workspace_id
                && !notification.read
        }) {
            notification.read = true;
            changed += 1;
        }
        changed
    }

    pub fn mark_all_read(&mut self) -> usize {
        let mut changed = 0;
        for notification in self
            .notifications
            .iter_mut()
            .filter(|notification| !notification.read)
        {
            notification.read = true;
            changed += 1;
        }
        changed
    }

    pub fn dismiss(&mut self, id: NotificationId) -> bool {
        let before = self.notifications.len();
        self.notifications
            .retain(|notification| notification.id != id);
        self.notifications.len() != before
    }

    pub fn dismiss_all_read(&mut self) -> usize {
        let before = self.notifications.len();
        self.notifications.retain(|notification| !notification.read);
        before - self.notifications.len()
    }

    pub fn clear_pane(&mut self, scope_id: EntityId, item_id: EntityId) -> usize {
        let before = self.notifications.len();
        self.notifications.retain(|notification| {
            notification.target.scope_id != scope_id || notification.target.item_id != item_id
        });
        before - self.notifications.len()
    }

    pub fn clear_workspace(&mut self, scope_id: EntityId, workspace_id: WorkspaceId) -> usize {
        let before = self.notifications.len();
        self.notifications.retain(|notification| {
            notification.target.scope_id != scope_id
                || notification.target.workspace_id != workspace_id
        });
        before - self.notifications.len()
    }

    pub fn clear_scope(&mut self, scope_id: EntityId) -> usize {
        let before = self.notifications.len();
        self.notifications
            .retain(|notification| notification.target.scope_id != scope_id);
        before - self.notifications.len()
    }

    pub fn clear_all(&mut self) -> usize {
        let count = self.notifications.len();
        self.notifications.clear();
        count
    }

    fn trim_to_capacity(&mut self) -> Vec<Notification> {
        let mut removed = Vec::new();
        while self.notifications.len() > self.capacity {
            if let Some(notification) = self.notifications.pop_back() {
                removed.push(notification);
            }
        }
        removed
    }
}

impl Default for NotificationStore {
    fn default() -> Self {
        Self::new()
    }
}

fn bounded_text(text: &str) -> String {
    let mut output = String::with_capacity(text.len().min(MAX_NOTIFICATION_TEXT_CHARS));
    let mut chars = text
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'));
    output.extend(chars.by_ref().take(MAX_NOTIFICATION_TEXT_CHARS));
    if chars.next().is_some() {
        output.push('…');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity_id(value: u64) -> EntityId {
        EntityId::from(value)
    }

    fn target(scope: u64, workspace: WorkspaceId, item: u64) -> NotificationTarget {
        NotificationTarget {
            scope_id: entity_id(scope),
            workspace_id: workspace,
            item_id: entity_id(item),
        }
    }

    fn request(target: NotificationTarget, title: &str) -> NotificationRequest {
        NotificationRequest::new(target, NotificationSource::Cli, title, "body")
    }

    fn record(store: &mut NotificationStore, request: NotificationRequest) -> Notification {
        store.record(request).notification
    }

    #[test]
    fn newest_notification_replaces_the_same_target() {
        let mut store = NotificationStore::new();
        let target = target(1, 1, 1);
        let first = record(&mut store, request(target, "first"));
        let outcome = store.record(request(target, "second"));
        let second = outcome.notification;

        assert_ne!(first.id, second.id);
        assert_eq!(
            outcome.removed.iter().map(|row| row.id).collect::<Vec<_>>(),
            [first.id]
        );
        assert_eq!(store.notifications().count(), 1);
        assert_eq!(store.notifications().next().unwrap().title, "second");
        assert!(store.get(first.id).is_none());
    }

    #[test]
    fn history_is_bounded_without_reusing_ids() {
        let mut store = NotificationStore::with_capacity(2);
        let first = record(&mut store, request(target(1, 1, 1), "one"));
        record(&mut store, request(target(1, 1, 2), "two"));
        let outcome = store.record(request(target(1, 2, 3), "three"));
        let third = outcome.notification;

        assert_eq!(store.notifications().count(), 2);
        assert!(store.get(first.id).is_none());
        assert!(third.id > first.id);
        assert_eq!(
            outcome.removed.iter().map(|row| row.id).collect::<Vec<_>>(),
            [first.id]
        );
    }

    #[test]
    fn kitty_identity_updates_only_the_matching_name() {
        let mut store = NotificationStore::new();
        let target = target(1, 1, 1);
        let mut named_a = request(target, "a1");
        named_a.identity = NotificationIdentity::KittyNamed("a".to_owned());
        let a = record(&mut store, named_a);

        let mut named_b = request(target, "b");
        named_b.identity = NotificationIdentity::KittyNamed("b".to_owned());
        let b = record(&mut store, named_b);

        let mut update_a = request(target, "a2");
        update_a.identity = NotificationIdentity::KittyNamed("a".to_owned());
        let outcome = store.record(update_a);

        assert_eq!(outcome.notification.id, a.id);
        assert!(outcome.removed.is_empty());
        assert_eq!(store.notifications().count(), 2);
        assert_eq!(store.get(a.id).unwrap().title, "a2");
        assert_eq!(store.get(b.id).unwrap().title, "b");
    }

    #[test]
    fn anonymous_kitty_notifications_are_unique() {
        let mut store = NotificationStore::new();
        let target = target(1, 1, 1);
        let mut first = request(target, "first");
        first.identity = NotificationIdentity::Unique;
        let first = record(&mut store, first);
        let mut second = request(target, "second");
        second.identity = NotificationIdentity::Unique;
        let second = record(&mut store, second);

        assert_ne!(first.id, second.id);
        assert_eq!(store.notifications().count(), 2);
    }

    #[test]
    fn workspace_queries_are_scoped_per_window() {
        let mut store = NotificationStore::new();
        record(&mut store, request(target(10, 1, 1), "first window"));
        record(&mut store, request(target(20, 1, 2), "second window"));

        assert_eq!(store.workspace_unread_count(entity_id(10), 1), 1);
        assert_eq!(store.workspace_unread_count(entity_id(20), 1), 1);
        assert_eq!(store.mark_workspace_read(entity_id(10), 1), 1);
        assert!(!store.workspace_has_unread(entity_id(10), 1));
        assert!(store.workspace_has_unread(entity_id(20), 1));
    }

    #[test]
    fn clear_workspace_preserves_other_targets() {
        let mut store = NotificationStore::new();
        record(&mut store, request(target(1, 1, 1), "one"));
        record(&mut store, request(target(1, 2, 2), "two"));

        assert_eq!(store.clear_workspace(entity_id(1), 1), 1);
        assert_eq!(store.notifications().count(), 1);
        assert_eq!(store.notifications().next().unwrap().title, "two");
    }

    #[test]
    fn focus_watermark_does_not_read_a_newer_named_update_that_reuses_its_id() {
        let mut store = NotificationStore::new();
        let target = target(1, 1, 1);
        let mut before_focus = request(target, "before focus");
        before_focus.identity = NotificationIdentity::KittyNamed("build".to_owned());
        let before_focus = record(&mut store, before_focus);
        let focus_watermark = store.newest_recorded_sequence();
        let mut after_focus = request(target, "after focus");
        after_focus.identity = NotificationIdentity::KittyNamed("build".to_owned());
        let after_focus = record(&mut store, after_focus);

        assert_eq!(after_focus.id, before_focus.id);
        assert!(after_focus.sequence > focus_watermark);
        assert_eq!(
            store.mark_pane_read_through(target.scope_id, target.item_id, focus_watermark,),
            0,
            "a named update recorded after the focus event must remain unread",
        );
        assert!(!store.get(after_focus.id).unwrap().read);
    }

    #[test]
    fn external_text_is_sanitized_and_bounded() {
        let mut store = NotificationStore::new();
        let text = format!("bad\0{}", "🙂".repeat(MAX_NOTIFICATION_TEXT_CHARS + 1));
        let notification = record(&mut store, request(target(1, 1, 1), &text));

        assert!(!notification.title.contains('\0'));
        assert_eq!(
            notification.title.chars().count(),
            MAX_NOTIFICATION_TEXT_CHARS + 1
        );
        assert!(notification.title.ends_with('…'));
    }
}
