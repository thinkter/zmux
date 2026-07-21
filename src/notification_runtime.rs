//! Notification orchestration and exact terminal navigation.
//!
//! Parsers and IPC transports submit canonical requests here. The runtime is
//! deliberately small and synchronous on the GPUI thread: it mutates the
//! observable store, resolves a target, and hands blocking native work to the
//! bounded desktop dispatcher.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use alacritty_terminal::term::{build_zmux_notification_replay_ack, build_zmux_pty_response};
use gpui::{AnyWindowHandle, App, Entity, EntityId, Focusable, Global, WeakEntity, Window};
use terminal::Terminal;
use terminal_view::TerminalView;
use workspace::{Workspace, item::ItemHandle};

use crate::{
    cli_server::{CliNotification, CliRouteId, CliRouteRegistration},
    desktop_notifications::{
        DesktopNotificationAction, DesktopNotificationPolicy, DesktopNotificationService,
    },
    notifications::{
        Notification, NotificationId, NotificationIdentity, NotificationRequest,
        NotificationSequence, NotificationSource, NotificationTarget, WorkspaceId,
    },
    osc::{
        KittyActivation, KittyDeliveryCondition, MAX_KITTY_PLAIN_CHUNK_BYTES, OscNotificationEvent,
        OscNotificationParser, decode_bridged_osc_title,
    },
    workspaces::WorkspacesPanel,
};

#[derive(Clone)]
struct TerminalRoute {
    target: NotificationTarget,
    window: AnyWindowHandle,
    workspace: WeakEntity<Workspace>,
    panel: WeakEntity<WorkspacesPanel>,
    view: WeakEntity<TerminalView>,
    terminal: WeakEntity<Terminal>,
    terminal_id: EntityId,
    cli_registration: Option<Arc<CliRouteRegistration>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FocusedRouteState {
    key: (EntityId, EntityId),
    has_unread: bool,
}

fn take_cached_unread_route(
    focused_route: Option<&mut FocusedRouteState>,
) -> Option<(EntityId, EntityId)> {
    let focused_route = focused_route?;
    if !focused_route.has_unread {
        return None;
    }
    focused_route.has_unread = false;
    Some(focused_route.key)
}

fn affected_registered_route_keys(
    registered: impl IntoIterator<Item = (EntityId, EntityId)>,
    targets: impl IntoIterator<Item = NotificationTarget>,
) -> HashSet<(EntityId, EntityId)> {
    let affected = targets
        .into_iter()
        .map(|target| (target.scope_id, target.item_id))
        .collect::<HashSet<_>>();
    registered
        .into_iter()
        .filter(|key| affected.contains(key))
        .collect()
}

fn notify_route_entities<T: 'static>(
    routes: impl IntoIterator<Item = ((EntityId, EntityId), WeakEntity<T>)>,
    affected: &HashSet<(EntityId, EntityId)>,
    cx: &mut App,
) -> usize {
    routes
        .into_iter()
        .filter(|(key, _)| affected.contains(key))
        .filter(|(_, entity)| {
            entity
                .update(cx, |_entity, entity_cx| entity_cx.notify())
                .is_ok()
        })
        .count()
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, collections::HashSet, rc::Rc};

    use gpui::{AppContext, EntityId, TestAppContext};

    use super::{
        FocusedRouteState, KittyRegistry, KittyRuntimeState, MAX_KITTY_PLAIN_CHUNK_BYTES,
        NotificationRuntime, NotificationSequence, NotificationTarget,
        affected_registered_route_keys, joined_identifier_bytes,
        native_delivery_absorbs_removed_retraction, notify_route_entities, sequence_is_after,
        should_deliver_native, take_cached_unread_route, unread_notification_ids_for_scope,
    };
    use crate::{
        notifications::{
            NotificationIdentity, NotificationRequest, NotificationSource, NotificationStore,
        },
        osc::KittyActivation,
    };

    fn target(scope: u64, workspace: u64, item: u64) -> NotificationTarget {
        NotificationTarget {
            scope_id: EntityId::from(scope),
            workspace_id: workspace,
            item_id: EntityId::from(item),
        }
    }

    #[test]
    fn keystroke_without_cached_unread_skips_store_callback() {
        let store_accesses = Cell::new(0);
        let mut focused_route = FocusedRouteState {
            key: (EntityId::from(1), EntityId::from(2)),
            has_unread: false,
        };
        if take_cached_unread_route(Some(&mut focused_route)).is_some() {
            store_accesses.set(store_accesses.get() + 1);
        }

        assert_eq!(store_accesses.get(), 0);
    }

    #[test]
    fn keystroke_with_cached_unread_dispatches_exact_route() {
        let expected = (EntityId::from(1), EntityId::from(2));
        let mut focused_route = FocusedRouteState {
            key: expected,
            has_unread: true,
        };
        let dispatched = Cell::new(take_cached_unread_route(Some(&mut focused_route)));

        assert_eq!(dispatched.get(), Some(expected));
        assert!(
            !focused_route.has_unread,
            "dispatch consumes the cached bit"
        );
        assert_eq!(
            take_cached_unread_route(Some(&mut focused_route)),
            None,
            "a second key before observer refresh cannot rescan the store",
        );
    }

    #[test]
    fn store_change_selects_only_registered_affected_routes() {
        let route_a = (EntityId::from(1), EntityId::from(10));
        let route_b = (EntityId::from(1), EntityId::from(20));
        let route_c = (EntityId::from(2), EntityId::from(30));
        let selected = affected_registered_route_keys(
            [route_a, route_b, route_c],
            [target(1, 7, 20), target(1, 9, 20), target(3, 7, 40)],
        );

        assert_eq!(selected, HashSet::from([route_b]));
    }

    #[test]
    fn repeated_producer_changes_never_select_unrelated_routes() {
        let producer = (EntityId::from(1), EntityId::from(10));
        let unrelated = (EntityId::from(1), EntityId::from(20));
        let mut producer_notifies = 0;
        let mut unrelated_notifies = 0;

        for _ in 0..10 {
            let selected =
                affected_registered_route_keys([producer, unrelated], [target(1, 7, 10)]);
            producer_notifies += usize::from(selected.contains(&producer));
            unrelated_notifies += usize::from(selected.contains(&unrelated));
        }

        assert_eq!(producer_notifies, 10);
        assert_eq!(unrelated_notifies, 0);
    }

    #[gpui::test]
    async fn repeated_store_changes_notify_only_the_affected_gpui_entity(cx: &mut TestAppContext) {
        struct NotifyProbe;

        let producer_notifies = Rc::new(Cell::new(0));
        let unrelated_notifies = Rc::new(Cell::new(0));
        let (producer, unrelated) = cx.update(|cx| {
            let producer = cx.new(|_| NotifyProbe);
            let unrelated = cx.new(|_| NotifyProbe);
            cx.observe(&producer, {
                let count = producer_notifies.clone();
                move |_, _| count.set(count.get() + 1)
            })
            .detach();
            cx.observe(&unrelated, {
                let count = unrelated_notifies.clone();
                move |_, _| count.set(count.get() + 1)
            })
            .detach();
            (producer, unrelated)
        });
        let producer_key = (EntityId::from(1), EntityId::from(10));
        let unrelated_key = (EntityId::from(1), EntityId::from(20));
        let owner = target(1, 7, 10);
        let mut store = NotificationStore::with_capacity(20);

        for sequence in 0..10 {
            cx.update(|cx| {
                let mut request = NotificationRequest::new(
                    owner,
                    NotificationSource::Manual,
                    format!("event-{sequence}"),
                    "body",
                );
                request.identity = NotificationIdentity::Unique;
                let outcome = store.record(request).unwrap();
                let affected = affected_registered_route_keys(
                    [producer_key, unrelated_key],
                    outcome.unread_count_changed_targets(),
                );
                assert_eq!(
                    notify_route_entities(
                        [
                            (producer_key, producer.downgrade()),
                            (unrelated_key, unrelated.downgrade()),
                        ],
                        &affected,
                        cx,
                    ),
                    1,
                );
            });
            cx.run_until_parked();
        }

        assert_eq!(producer_notifies.get(), 10);
        assert_eq!(unrelated_notifies.get(), 0);
    }

    fn state(
        target: NotificationTarget,
        client_id: Option<&str>,
        sequence: NotificationSequence,
    ) -> KittyRuntimeState {
        KittyRuntimeState {
            target,
            client_id: client_id.map(str::to_owned),
            sequence,
            native_alive: true,
            activation: KittyActivation {
                focus: false,
                report: true,
            },
            report_close: false,
        }
    }

    #[test]
    fn kitty_capabilities_are_truthful_and_use_zero_for_anonymous_queries() {
        let expected = "\x1b]99;i=0:p=?;a=focus,report:c=1:o=always,unfocused:p=title,body,?,close,alive\x1b\\";
        assert_eq!(
            NotificationRuntime::kitty_capability_response(None),
            expected
        );
        assert_eq!(
            NotificationRuntime::kitty_capability_response(Some("query")),
            expected.replacen("i=0", "i=query", 1),
        );
    }

    #[test]
    fn replay_sequence_order_handles_duplicates_stale_frames_and_wraparound() {
        assert!(!sequence_is_after(7, 7));
        assert!(!sequence_is_after(6, 7));
        assert!(sequence_is_after(8, 7));
        assert!(sequence_is_after(1, u64::MAX));
        assert!(!sequence_is_after(u64::MAX, 1));
    }

    #[test]
    fn kitty_always_can_override_exact_focus_while_unfocused_mode_cannot() {
        assert!(!should_deliver_native(true, false));
        assert!(should_deliver_native(true, true));
        assert!(should_deliver_native(false, false));
    }

    #[test]
    fn scoped_jump_candidates_exclude_newer_unread_from_another_window() {
        let mut store = NotificationStore::with_capacity(10);
        let scope_a = target(1, 1, 10);
        let scope_b = target(2, 1, 20);
        let in_a = store
            .record(NotificationRequest::new(
                scope_a,
                NotificationSource::Manual,
                "A",
                "older in this window",
            ))
            .unwrap()
            .notification;
        let in_b = store
            .record(NotificationRequest::new(
                scope_b,
                NotificationSource::Manual,
                "B",
                "newer globally",
            ))
            .unwrap()
            .notification;

        assert_eq!(store.latest_unread().unwrap().id, in_b.id);
        assert_eq!(
            unread_notification_ids_for_scope(&store, scope_a.scope_id),
            [in_a.id],
            "a jump invoked in scope A must never consider scope B"
        );
    }

    #[test]
    fn accepted_target_replacement_absorbs_only_same_target_native_retractions() {
        let mut store = NotificationStore::with_capacity(10);
        let owner = target(3, 1, 30);
        let old = store
            .record(NotificationRequest::new(
                owner,
                NotificationSource::Manual,
                "old",
                "old",
            ))
            .unwrap()
            .notification;
        let replacement = store
            .record(NotificationRequest::new(
                owner,
                NotificationSource::Manual,
                "new",
                "new",
            ))
            .unwrap()
            .notification;
        let unrelated = store
            .record(NotificationRequest::new(
                target(4, 1, 40),
                NotificationSource::Manual,
                "other",
                "other",
            ))
            .unwrap()
            .notification;

        assert!(native_delivery_absorbs_removed_retraction(
            &replacement,
            &old
        ));
        assert!(!native_delivery_absorbs_removed_retraction(
            &replacement,
            &unrelated
        ));
    }

    #[test]
    fn named_canonical_update_reuses_id_without_a_removed_cleanup_row() {
        let mut store = NotificationStore::with_capacity(10);
        let owner = target(5, 1, 50);
        let mut request =
            NotificationRequest::new(owner, NotificationSource::Osc99, "build", "first");
        request.identity = NotificationIdentity::KittyNamed("build".to_owned());
        let first = store.record(request.clone()).unwrap().notification;
        request.body = "updated canonical body".to_owned();
        let update = store.record(request).unwrap();

        assert_eq!(update.notification.id, first.id);
        assert!(update.removed.is_empty());
        assert_eq!(
            store.get(first.id).unwrap().body,
            "updated canonical body",
            "native enqueue rejection must not roll back the canonical update"
        );
    }

    #[test]
    fn kitty_registry_keeps_named_and_anonymous_notifications_distinct() {
        let mut registry = KittyRegistry::default();
        let owner = target(1, 7, 10);

        let _ = registry.track(11, state(owner, None, 11));
        let _ = registry.track(12, state(owner, None, 12));
        let _ = registry.track(13, state(owner, Some("0"), 13));
        let _ = registry.track(14, state(owner, Some("build"), 14));

        assert_eq!(
            registry.named_id(owner.scope_id, owner.item_id, "0"),
            Some(13)
        );
        assert_eq!(
            registry.named_id(owner.scope_id, owner.item_id, "build"),
            Some(14)
        );
        assert_eq!(
            registry.alive_identifiers(owner.scope_id, owner.item_id),
            ["0", "build"]
        );
        assert_eq!(
            registry.ids_for_target(owner.scope_id, owner.item_id),
            [11, 12, 13, 14]
        );

        assert!(registry.take(11).unwrap().client_id.is_none());
        assert_eq!(
            registry.named_id(owner.scope_id, owner.item_id, "0"),
            Some(13)
        );
    }

    #[test]
    fn kitty_registry_replacement_and_removal_cannot_leave_stale_names() {
        let mut registry = KittyRegistry::default();
        let owner = target(1, 7, 10);
        let other = target(1, 7, 20);

        let _ = registry.track(21, state(owner, Some("build"), 21));
        let _ = registry.track(21, state(owner, Some("deploy"), 22));
        let _ = registry.track(22, state(other, Some("build"), 23));

        assert_eq!(
            registry.named_id(owner.scope_id, owner.item_id, "build"),
            None
        );
        assert_eq!(
            registry.named_id(owner.scope_id, owner.item_id, "deploy"),
            Some(21)
        );
        assert_eq!(
            registry.alive_identifiers(owner.scope_id, owner.item_id),
            ["deploy"]
        );
        assert_eq!(
            registry.alive_identifiers(other.scope_id, other.item_id),
            ["build"]
        );

        assert_eq!(
            registry.take(21).unwrap().client_id.as_deref(),
            Some("deploy")
        );
        assert_eq!(
            registry.named_id(owner.scope_id, owner.item_id, "deploy"),
            None
        );
        assert_eq!(
            registry.named_id(other.scope_id, other.item_id, "build"),
            Some(22)
        );
    }

    #[test]
    fn kitty_liveness_is_sequence_aware_and_bounded_to_one_plain_payload() {
        let mut registry = KittyRegistry::default();
        let owner = target(1, 7, 10);
        let mut evicted = Vec::new();
        for index in 0_u64..20 {
            let identifier = format!("{index:03}{}", "x".repeat(125));
            evicted.extend(registry.track(index + 1, state(owner, Some(&identifier), index + 1)));
        }

        let alive = registry.alive_identifiers(owner.scope_id, owner.item_id);
        assert_eq!(alive.len(), 15);
        assert_eq!(evicted, [1, 2, 3, 4, 5]);
        assert!(joined_identifier_bytes(&alive) <= MAX_KITTY_PLAIN_CHUNK_BYTES);

        let newest = 20;
        assert!(!registry.mark_native_unavailable(newest, 19));
        assert!(
            registry
                .alive_identifiers(owner.scope_id, owner.item_id)
                .contains(&format!("{:03}{}", newest - 1, "x".repeat(125)))
        );
        assert!(registry.mark_native_unavailable(newest, 20));
        assert!(
            !registry
                .alive_identifiers(owner.scope_id, owner.item_id)
                .contains(&format!("{:03}{}", newest - 1, "x".repeat(125)))
        );
    }

    #[test]
    fn kitty_untracked_close_downgrade_is_sequence_aware_and_emitted_once() {
        let mut registry = KittyRegistry::default();
        let owner = target(1, 7, 10);
        let mut tracked = state(owner, Some("build"), 41);
        tracked.report_close = true;
        let _ = registry.track(9, tracked);

        assert_eq!(registry.take_untracked_close_report(9, 40), None);
        assert_eq!(
            registry.take_untracked_close_report(9, 41),
            Some((owner, "build".to_owned()))
        );
        assert_eq!(registry.take_untracked_close_report(9, 41), None);
        assert!(
            registry.get(9).is_some(),
            "downgrading keeps liveness state"
        );
    }

    #[gpui::test]
    async fn native_expiry_clears_liveness_but_preserves_canonical_unread(cx: &mut TestAppContext) {
        cx.update(|cx| {
            NotificationStore::init(cx);
            cx.set_global(NotificationRuntime::default());
            let owner = target(9, 3, 12);
            let notification = NotificationStore::global(cx).update(cx, |store, _| {
                store
                    .record(NotificationRequest::new(
                        owner,
                        NotificationSource::Osc99,
                        "Background build",
                        "Still needs attention",
                    ))
                    .unwrap()
                    .notification
            });
            let mut tracked = state(owner, Some("build"), notification.sequence);
            tracked.report_close = true;
            let _ = cx
                .global_mut::<NotificationRuntime>()
                .kitty
                .track(notification.id, tracked);

            NotificationRuntime::native_notification_expired(
                notification.id,
                notification.sequence.wrapping_add(1),
                cx,
            );
            assert!(
                cx.global::<NotificationRuntime>()
                    .kitty
                    .get(notification.id)
                    .is_some(),
                "a stale expiry must not end the current generation"
            );

            NotificationRuntime::native_notification_expired(
                notification.id,
                notification.sequence,
                cx,
            );
            assert!(
                cx.global::<NotificationRuntime>()
                    .kitty
                    .get(notification.id)
                    .is_none(),
                "expiry ends Kitty/native liveness and consumes close reporting"
            );
            assert!(
                !NotificationStore::global(cx)
                    .read(cx)
                    .get(notification.id)
                    .unwrap()
                    .read,
                "automatic expiry must leave the canonical row unread"
            );
        });
    }
}

/// App-global routing table. Entity handles are weak so notification support
/// never extends the lifetime of a closed terminal or window.
#[derive(Default)]
pub struct NotificationRuntime {
    routes: HashMap<(EntityId, EntityId), TerminalRoute>,
    focused_routes: HashMap<AnyWindowHandle, FocusedRouteState>,
    cli_routes: HashMap<CliRouteId, (EntityId, EntityId)>,
    pending_cli_routes: HashMap<EntityId, Arc<CliRouteRegistration>>,
    listeners_installed: HashSet<(EntityId, EntityId)>,
    osc_parsers: HashMap<(EntityId, EntityId), OscNotificationParser>,
    osc_bridge_sequences: HashMap<(EntityId, EntityId), u64>,
    kitty: KittyRegistry,
}

#[derive(Clone)]
struct KittyRuntimeState {
    target: NotificationTarget,
    client_id: Option<String>,
    sequence: NotificationSequence,
    native_alive: bool,
    activation: KittyActivation,
    report_close: bool,
}

#[derive(Default)]
struct KittyRegistry {
    by_notification: HashMap<NotificationId, KittyRuntimeState>,
    named: HashMap<(EntityId, EntityId, String), NotificationId>,
}

impl KittyRegistry {
    fn track(&mut self, id: NotificationId, state: KittyRuntimeState) -> Vec<NotificationId> {
        let target = state.target;
        if let Some(previous) = self.by_notification.remove(&id) {
            self.remove_named_index(id, &previous);
        }
        if let Some(identifier) = state.client_id.as_ref() {
            self.named.insert(
                (
                    state.target.scope_id,
                    state.target.item_id,
                    identifier.clone(),
                ),
                id,
            );
        }
        self.by_notification.insert(id, state);
        self.enforce_alive_payload_bound(target.scope_id, target.item_id)
    }

    fn get(&self, id: NotificationId) -> Option<&KittyRuntimeState> {
        self.by_notification.get(&id)
    }

    fn take(&mut self, id: NotificationId) -> Option<KittyRuntimeState> {
        let state = self.by_notification.remove(&id)?;
        self.remove_named_index(id, &state);
        Some(state)
    }

    fn named_id(
        &self,
        scope_id: EntityId,
        item_id: EntityId,
        identifier: &str,
    ) -> Option<NotificationId> {
        self.named
            .get(&(scope_id, item_id, identifier.to_owned()))
            .copied()
    }

    fn alive_identifiers(&self, scope_id: EntityId, item_id: EntityId) -> Vec<String> {
        let mut identifiers = self
            .by_notification
            .values()
            .filter(|state| {
                state.target.scope_id == scope_id
                    && state.target.item_id == item_id
                    && state.native_alive
            })
            .filter_map(|state| state.client_id.clone())
            .collect::<Vec<_>>();
        identifiers.sort();
        debug_assert!(joined_identifier_bytes(&identifiers) <= MAX_KITTY_PLAIN_CHUNK_BYTES);
        identifiers
    }

    fn mark_native_unavailable(
        &mut self,
        id: NotificationId,
        sequence: NotificationSequence,
    ) -> bool {
        let Some(state) = self.by_notification.get_mut(&id) else {
            return false;
        };
        if state.sequence != sequence || !state.native_alive {
            return false;
        }
        state.native_alive = false;
        true
    }

    /// Downgrade a requested close callback to Kitty's explicit `untracked`
    /// response without removing the notification's identity/liveness state.
    /// The bit is consumed so overlapping native failure and capacity paths
    /// can never emit the response twice.
    fn take_untracked_close_report(
        &mut self,
        id: NotificationId,
        sequence: NotificationSequence,
    ) -> Option<(NotificationTarget, String)> {
        let state = self.by_notification.get_mut(&id)?;
        if state.sequence != sequence || !state.report_close {
            return None;
        }
        state.report_close = false;
        Some((
            state.target,
            state.client_id.clone().unwrap_or_else(|| "0".to_owned()),
        ))
    }

    fn enforce_alive_payload_bound(
        &mut self,
        scope_id: EntityId,
        item_id: EntityId,
    ) -> Vec<NotificationId> {
        let mut alive = self
            .by_notification
            .iter()
            .filter_map(|(id, state)| {
                if state.target.scope_id == scope_id
                    && state.target.item_id == item_id
                    && state.native_alive
                {
                    state
                        .client_id
                        .as_ref()
                        .map(|identifier| (*id, state.sequence, identifier.len()))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        alive.sort_by_key(|(_, sequence, _)| *sequence);

        let mut serialized_bytes = alive
            .iter()
            .map(|(_, _, identifier_bytes)| *identifier_bytes)
            .sum::<usize>()
            .saturating_add(alive.len().saturating_sub(1));
        let mut remaining = alive.len();
        let mut evicted = Vec::new();
        for (id, _, identifier_bytes) in alive {
            if serialized_bytes <= MAX_KITTY_PLAIN_CHUNK_BYTES {
                break;
            }
            remaining -= 1;
            serialized_bytes = serialized_bytes.saturating_sub(identifier_bytes);
            if remaining > 0 {
                serialized_bytes = serialized_bytes.saturating_sub(1);
            }
            if let Some(state) = self.by_notification.get_mut(&id) {
                state.native_alive = false;
                evicted.push(id);
            }
        }
        evicted
    }

    fn ids_for_target(&self, scope_id: EntityId, item_id: EntityId) -> Vec<NotificationId> {
        let mut ids = self
            .by_notification
            .iter()
            .filter_map(|(id, state)| {
                (state.target.scope_id == scope_id && state.target.item_id == item_id)
                    .then_some(*id)
            })
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids
    }

    fn remove_named_index(&mut self, id: NotificationId, state: &KittyRuntimeState) {
        let Some(identifier) = state.client_id.as_ref() else {
            return;
        };
        let key = (
            state.target.scope_id,
            state.target.item_id,
            identifier.clone(),
        );
        if self.named.get(&key) == Some(&id) {
            self.named.remove(&key);
        }
    }
}

fn joined_identifier_bytes(identifiers: &[String]) -> usize {
    identifiers
        .iter()
        .map(String::len)
        .sum::<usize>()
        .saturating_add(identifiers.len().saturating_sub(1))
}

struct PublishedNotification {
    notification: Notification,
    native_alive: bool,
}

fn sequence_is_after(candidate: u64, seen: u64) -> bool {
    let distance = candidate.wrapping_sub(seen);
    distance != 0 && distance < (1_u64 << 63)
}

fn should_deliver_native(exact_target_is_focused: bool, force_native_delivery: bool) -> bool {
    force_native_delivery
        || DesktopNotificationPolicy::default().should_deliver(exact_target_is_focused)
}

fn native_delivery_absorbs_removed_retraction(
    current: &Notification,
    removed: &Notification,
) -> bool {
    if current.target != removed.target {
        return false;
    }
    match &current.identity {
        // Target publication atomically clears every native identity on the
        // target while reusing an existing Target token when present.
        NotificationIdentity::Target => true,
        NotificationIdentity::KittyNamed(identifier) => matches!(
            &removed.identity,
            NotificationIdentity::KittyNamed(removed_identifier)
                if removed_identifier == identifier
        ),
        NotificationIdentity::Unique => false,
    }
}

fn unread_notification_ids_for_scope(
    store: &crate::notifications::NotificationStore,
    scope_id: EntityId,
) -> Vec<NotificationId> {
    store
        .notifications()
        .filter(|notification| notification.target.scope_id == scope_id && !notification.read)
        .map(|notification| notification.id)
        .collect()
}

impl Global for NotificationRuntime {}

impl NotificationRuntime {
    pub fn init(cx: &mut App) {
        crate::notifications::NotificationStore::init(cx);
        if !cx.has_global::<Self>() {
            cx.set_global(Self::default());
            cx.observe_keystrokes(|_, window, cx| {
                let target = take_cached_unread_route(
                    cx.global_mut::<Self>()
                        .focused_routes
                        .get_mut(&window.window_handle()),
                );
                if let Some((scope_id, item_id)) = target {
                    Self::mark_item_read_state(scope_id, item_id, cx);
                }
            })
            .detach();
        }

        if !cx.has_global::<DesktopNotificationService>() {
            // Native lifecycle events are produced only for the dispatcher's
            // bounded active set. Keep this channel lossless so activation,
            // closure, and delivery-failure state can never drift from Kitty
            // `p=alive` while GPUI is briefly busy.
            let (action_sender, action_receiver) = async_channel::unbounded();
            let action_task = cx.spawn(async move |cx| {
                while let Ok(action) = action_receiver.recv().await {
                    cx.update(|cx| match action {
                        DesktopNotificationAction::Activated { id, sequence } => {
                            Self::open_native_notification(id, sequence, cx);
                        }
                        DesktopNotificationAction::Closed { id, sequence } => {
                            Self::native_notification_closed(id, sequence, cx);
                        }
                        DesktopNotificationAction::Expired { id, sequence } => {
                            Self::native_notification_expired(id, sequence, cx);
                        }
                        DesktopNotificationAction::Unavailable { id, sequence } => {
                            Self::native_notification_unavailable(id, sequence, cx);
                        }
                    });
                }
            });
            DesktopNotificationService::init(action_sender, action_task, cx);
        }
    }

    fn notify_terminal_tabs(targets: impl IntoIterator<Item = NotificationTarget>, cx: &mut App) {
        let route_keys =
            affected_registered_route_keys(cx.global::<Self>().routes.keys().copied(), targets);
        Self::notify_terminal_tab_keys(route_keys, cx);
    }

    fn notify_terminal_tab_keys(
        route_keys: impl IntoIterator<Item = (EntityId, EntityId)>,
        cx: &mut App,
    ) {
        let route_keys = route_keys.into_iter().collect::<HashSet<_>>();
        if route_keys.is_empty() {
            return;
        }
        Self::refresh_focused_route_unread(cx);
        let routes = cx
            .global::<Self>()
            .routes
            .iter()
            .map(|(key, route)| (*key, route.view.clone()))
            .collect::<Vec<_>>();
        notify_route_entities(routes, &route_keys, cx);
    }

    fn refresh_focused_route_unread(cx: &mut App) {
        let focused_routes = cx
            .global::<Self>()
            .focused_routes
            .iter()
            .map(|(window, state)| (*window, state.key))
            .collect::<Vec<_>>();
        let unread = {
            let store = crate::notifications::NotificationStore::global(cx);
            let store = store.read(cx);
            focused_routes
                .into_iter()
                .map(|(window, key)| (window, key, store.pane_has_unread(key.0, key.1)))
                .collect::<Vec<_>>()
        };
        let runtime = cx.global_mut::<Self>();
        for (window, key, has_unread) in unread {
            if let Some(state) = runtime.focused_routes.get_mut(&window)
                && state.key == key
            {
                state.has_unread = has_unread;
            }
        }
    }

    fn set_focused_route(window: AnyWindowHandle, key: (EntityId, EntityId), cx: &mut App) {
        let has_unread = crate::notifications::NotificationStore::global(cx)
            .read(cx)
            .pane_has_unread(key.0, key.1);
        cx.global_mut::<Self>()
            .focused_routes
            .insert(window, FocusedRouteState { key, has_unread });
    }

    fn clear_focused_route_if_matches(
        window: AnyWindowHandle,
        key: (EntityId, EntityId),
        cx: &mut App,
    ) {
        let runtime = cx.global_mut::<Self>();
        if runtime
            .focused_routes
            .get(&window)
            .is_some_and(|state| state.key == key)
        {
            runtime.focused_routes.remove(&window);
        }
    }

    /// Observe terminal additions/removals for one zmux window. This is called
    /// once after the sidebar panel has been installed.
    pub fn attach_workspace(
        workspace: gpui::Entity<Workspace>,
        panel: gpui::Entity<WorkspacesPanel>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let scope_id = panel.entity_id();
        let weak_workspace = workspace.downgrade();
        let weak_panel = panel.downgrade();
        let window_handle = window.window_handle();

        window
            .subscribe(&workspace, cx, {
                let weak_workspace = weak_workspace.clone();
                let weak_panel = weak_panel.clone();
                move |_workspace, event: &workspace::Event, window, cx| match event {
                    workspace::Event::ItemAdded { item } => {
                        // ItemAdded is emitted while the new TerminalView can
                        // still be on GPUI's update stack. Defer registration
                        // to avoid re-entrant view updates and to observe the
                        // logical workspace ID after a layout swap commits.
                        let item = item.boxed_clone();
                        let weak_workspace = weak_workspace.clone();
                        let weak_panel = weak_panel.clone();
                        let window_handle = window.window_handle();
                        cx.defer(move |cx| {
                            let _ = window_handle.update(cx, |_, window, cx| {
                                let Some(panel) = weak_panel.upgrade() else {
                                    return;
                                };
                                let workspace_id =
                                    panel.read(cx).workspace_id_for_item(item.item_id());
                                Self::register_terminal(
                                    item.as_ref(),
                                    scope_id,
                                    workspace_id,
                                    weak_workspace,
                                    weak_panel,
                                    window,
                                    cx,
                                );
                            });
                        });
                    }
                    workspace::Event::ItemRemoved { item_id } => {
                        let item_id = *item_id;
                        cx.defer(move |cx| Self::prune_released_item(scope_id, item_id, cx));
                    }
                    _ => {}
                }
            })
            .detach();

        window
            .observe_release(&panel, cx, move |_, _window, cx| {
                Self::clear_scope(scope_id, cx);
            })
            .detach();

        // zmux installs this observer before creating its first terminal. Every
        // terminal thereafter, including restored parked layouts and splits,
        // passes through `Workspace::Event::ItemAdded`. Avoiding an eager read
        // here is important because this hook itself runs during Workspace
        // initialization and GPUI forbids re-entrant entity reads.

        // Retain the handle only in the route; this local is intentionally used
        // to make the association explicit and catch accidental window swaps.
        debug_assert_eq!(window_handle, window.window_handle());
    }

    /// Stage a fresh per-terminal capability before TerminalView emits
    /// `ItemAdded`. The capability remains inactive until `register_terminal`
    /// binds it to the exact runtime target.
    pub fn stage_cli_route(
        terminal: &Entity<Terminal>,
        registration: Arc<CliRouteRegistration>,
        cx: &mut App,
    ) {
        let terminal_id = terminal.entity_id();
        let route_id = registration.route_id();
        let completion = terminal.read(cx).wait_for_completed_task(cx);
        let existing_key = cx
            .global::<Self>()
            .routes
            .iter()
            .find_map(|(key, route)| (route.terminal_id == terminal_id).then_some(*key));
        if let Some(key) = existing_key {
            Self::bind_cli_registration(key, registration, cx);
        } else {
            cx.global_mut::<Self>()
                .pending_cli_routes
                .insert(terminal_id, registration);
        }

        cx.observe_release(terminal, move |_terminal, cx| {
            Self::revoke_cli_registration_for_terminal(terminal_id, cx);
        })
        .detach();

        // Task-backed terminal entities intentionally remain mounted after the
        // child process exits. Tie the bearer lease to the process lifetime,
        // not the tab lifetime, and guard by route ID so a future restage on
        // the same entity cannot be revoked by an older completion watcher.
        cx.spawn(async move |cx| {
            let _ = completion.await;
            cx.update(|cx| {
                Self::revoke_cli_registration_if_matches(terminal_id, route_id, cx);
            });
        })
        .detach();
    }

    /// Bind first, activate second. If activation fails, remove the binding and
    /// drop the lease so a half-installed capability can never remain usable.
    fn bind_cli_registration(
        key: (EntityId, EntityId),
        registration: Arc<CliRouteRegistration>,
        cx: &mut App,
    ) -> bool {
        let route_id = registration.route_id();
        {
            let runtime = cx.global_mut::<Self>();
            let Some(route) = runtime.routes.get_mut(&key) else {
                return false;
            };
            let previous = route
                .cli_registration
                .replace(Arc::clone(&registration))
                .map(|registration| registration.route_id());
            if let Some(previous) = previous {
                runtime.cli_routes.remove(&previous);
            }
            runtime.cli_routes.insert(route_id, key);
        }

        if registration.activate() {
            return true;
        }

        let runtime = cx.global_mut::<Self>();
        runtime.cli_routes.remove(&route_id);
        if let Some(route) = runtime.routes.get_mut(&key)
            && route
                .cli_registration
                .as_ref()
                .is_some_and(|registration| registration.route_id() == route_id)
        {
            route.cli_registration = None;
        }
        false
    }

    fn revoke_cli_registration_for_terminal(terminal_id: EntityId, cx: &mut App) {
        let runtime = cx.global_mut::<Self>();
        runtime.pending_cli_routes.remove(&terminal_id);
        let route_ids = runtime
            .routes
            .values_mut()
            .filter(|route| route.terminal_id == terminal_id)
            .filter_map(|route| route.cli_registration.take())
            .map(|registration| registration.route_id())
            .collect::<Vec<_>>();
        for route_id in route_ids {
            runtime.cli_routes.remove(&route_id);
        }
    }

    fn revoke_cli_registration_if_matches(
        terminal_id: EntityId,
        route_id: CliRouteId,
        cx: &mut App,
    ) {
        let runtime = cx.global_mut::<Self>();
        let pending_matches = runtime
            .pending_cli_routes
            .get(&terminal_id)
            .is_some_and(|registration| registration.route_id() == route_id);
        if pending_matches {
            runtime.pending_cli_routes.remove(&terminal_id);
        }
        for route in runtime
            .routes
            .values_mut()
            .filter(|route| route.terminal_id == terminal_id)
        {
            let bound_matches = route
                .cli_registration
                .as_ref()
                .is_some_and(|registration| registration.route_id() == route_id);
            if bound_matches {
                route.cli_registration = None;
            }
        }
        runtime.cli_routes.remove(&route_id);
    }

    fn remove_route_state(key: (EntityId, EntityId), cx: &mut App) {
        let runtime = cx.global_mut::<Self>();
        if let Some(route) = runtime.routes.remove(&key) {
            if runtime
                .focused_routes
                .get(&route.window)
                .is_some_and(|state| state.key == key)
            {
                runtime.focused_routes.remove(&route.window);
            }
            if let Some(registration) = route.cli_registration {
                runtime.cli_routes.remove(&registration.route_id());
            }
        }
        runtime.listeners_installed.remove(&key);
        runtime.osc_parsers.remove(&key);
        runtime.osc_bridge_sequences.remove(&key);
    }

    pub fn register_terminal(
        item: &dyn ItemHandle,
        scope_id: EntityId,
        workspace_id: WorkspaceId,
        workspace: WeakEntity<Workspace>,
        panel: WeakEntity<WorkspacesPanel>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(view) = item.act_as::<TerminalView>(cx) else {
            return;
        };
        let item_id = item.item_id();
        let terminal = view.read(cx).terminal().clone();
        let terminal_id = terminal.entity_id();
        let target = NotificationTarget {
            scope_id,
            workspace_id,
            item_id,
        };
        let key = (scope_id, item_id);

        let cli_registration = {
            let runtime = cx.global_mut::<Self>();
            runtime.pending_cli_routes.remove(&terminal_id).or_else(|| {
                runtime
                    .routes
                    .get(&key)
                    .and_then(|route| route.cli_registration.clone())
            })
        };
        let current_window = window.window_handle();
        let previous_window = cx
            .global_mut::<Self>()
            .routes
            .insert(
                key,
                TerminalRoute {
                    target,
                    window: current_window,
                    workspace,
                    panel,
                    view: view.downgrade(),
                    terminal: terminal.downgrade(),
                    terminal_id,
                    cli_registration: None,
                },
            )
            .map(|route| route.window);
        if let Some(previous_window) = previous_window
            && previous_window != current_window
        {
            Self::clear_focused_route_if_matches(previous_window, key, cx);
        }
        if let Some(registration) = cli_registration {
            Self::bind_cli_registration(key, registration, cx);
        }

        if Self::target_is_focused_in_window(target, window, cx) {
            Self::set_focused_route(window.window_handle(), key, cx);
        }

        if !cx.global_mut::<Self>().listeners_installed.insert(key) {
            return;
        }

        view.update(cx, |view, view_cx| {
            let focus_handle = view.focus_handle(view_cx);
            view_cx
                .on_focus_in(&focus_handle, window, move |_view, window, cx| {
                    Self::set_focused_route(window.window_handle(), key, cx);
                    let store = crate::notifications::NotificationStore::global(cx);
                    let watermark = store.read(cx).newest_recorded_sequence();
                    cx.defer(move |cx| {
                        Self::mark_item_read_through_if_focused(scope_id, item_id, watermark, cx);
                    });
                })
                .detach();
            view_cx
                .on_focus_out(&focus_handle, window, move |_view, _event, window, cx| {
                    Self::clear_focused_route_if_matches(window.window_handle(), key, cx);
                })
                .detach();
        });

        // TerminalView intentionally forwards only a subset of terminal
        // events. Subscribe to Terminal directly so bridge frames cannot be
        // discarded before notification decoding sees them.
        window
            .subscribe(
                &terminal,
                cx,
                move |terminal, event: &terminal::Event, window, cx| {
                    if matches!(event, terminal::Event::BreadcrumbsChanged) {
                        let title = terminal.read(cx).breadcrumb_text.clone();
                        Self::handle_osc_title(scope_id, item_id, &title, window, cx);
                    }
                },
            )
            .detach();

        // A shell can emit output before ItemAdded's deferred registration.
        // V3 deliberately leaves its replay envelope in breadcrumbs until an
        // ACK, so consume that initial value immediately after subscribing.
        let initial_title = terminal.read(cx).breadcrumb_text.clone();
        Self::handle_osc_title(scope_id, item_id, &initial_title, window, cx);
    }

    /// Submit a notification from any ingress path.
    pub fn publish(request: NotificationRequest, cx: &mut App) -> Option<Notification> {
        let exact_target_is_focused = Self::target_is_focused(request.target, cx);
        Self::publish_with_focus(request, exact_target_is_focused, cx)
            .map(|published| published.notification)
    }

    /// Submit while already updating the notification's owning window.
    ///
    /// Calling `AnyWindowHandle::update` for that same window would be
    /// re-entrant and fail, so window-originating ingress computes focus from
    /// the live `Window` reference instead.
    fn publish_from_window(
        request: NotificationRequest,
        window: &Window,
        cx: &mut App,
    ) -> Option<Notification> {
        let exact_target_is_focused = Self::target_is_focused_in_window(request.target, window, cx);
        Self::publish_with_focus(request, exact_target_is_focused, cx)
            .map(|published| published.notification)
    }

    fn publish_with_focus(
        request: NotificationRequest,
        exact_target_is_focused: bool,
        cx: &mut App,
    ) -> Option<PublishedNotification> {
        Self::publish_with_native_override(request, exact_target_is_focused, false, cx)
    }

    fn publish_with_native_override(
        request: NotificationRequest,
        exact_target_is_focused: bool,
        force_native_delivery: bool,
        cx: &mut App,
    ) -> Option<PublishedNotification> {
        let target = request.target;
        if request.source != NotificationSource::Osc99 {
            Self::report_target_closed(target.scope_id, target.item_id, cx);
        }
        let outcome =
            crate::notifications::NotificationStore::global(cx).update(cx, |store, store_cx| {
                let outcome = store.record(request);
                if outcome.is_some() {
                    store_cx.notify();
                }
                outcome
            });
        let outcome = outcome?;
        let unread_count_changed_targets = outcome.unread_count_changed_targets();
        let notification = outcome.notification;
        Self::notify_terminal_tabs(unread_count_changed_targets, cx);
        let should_deliver = should_deliver_native(exact_target_is_focused, force_native_delivery);

        let native_alive = if should_deliver {
            // Queue the replacement before retracting superseded canonical
            // rows. On XDG this lets a target-scoped update reuse the server's
            // native token instead of briefly closing and recreating it.
            DesktopNotificationService::submit(&notification, cx)
        } else {
            false
        };

        for removed in outcome.removed {
            if !(native_alive
                && native_delivery_absorbs_removed_retraction(&notification, &removed))
            {
                DesktopNotificationService::retract(removed.id, cx);
            }
            Self::report_kitty_closed(removed.id, removed.sequence, cx);
        }

        // A rejected/full submission never reaches the backend's replacement
        // failure cleanup. This is especially important for named Kitty
        // updates, which reuse the canonical ID and therefore have no removed
        // row to reconcile: explicitly retract that same-ID stale banner.
        if !native_alive {
            DesktopNotificationService::retract(notification.id, cx);
        }

        Some(PublishedNotification {
            notification,
            native_alive,
        })
    }

    pub fn publish_manual_for_target(
        target: NotificationTarget,
        title: impl Into<String>,
        body: impl Into<String>,
        window: &Window,
        cx: &mut App,
    ) -> Option<Notification> {
        Self::publish_from_window(
            NotificationRequest::new(target, NotificationSource::Manual, title, body),
            window,
            cx,
        )
    }

    /// Publish only through the exact server-derived route capability. Client
    /// payloads contain no target claim, and stale/unknown IDs never fall back
    /// to the focused pane.
    pub fn publish_cli(
        route_id: CliRouteId,
        notification: CliNotification,
        cx: &mut App,
    ) -> Option<Notification> {
        let key = cx.global::<Self>().cli_routes.get(&route_id).copied()?;
        let route = cx
            .global::<Self>()
            .routes
            .get(&key)
            .filter(|route| {
                route.view.upgrade().is_some()
                    && route.terminal.upgrade().is_some()
                    && route
                        .cli_registration
                        .as_ref()
                        .is_some_and(|registration| registration.route_id() == route_id)
            })
            .cloned()?;
        let terminal = route.terminal.upgrade()?;
        let task_is_running = terminal
            .read(cx)
            .task()
            .is_some_and(|task| task.status == terminal::TaskStatus::Running);
        if !task_is_running {
            // Task-backed zmux tabs remain visible after their child exits.
            // Revoke on the first post-exit request so descendants that kept a
            // copied endpoint can neither target a dead pane nor retain a live
            // bearer capability for the rest of the tab's lifetime.
            Self::revoke_cli_registration_for_terminal(route.terminal_id, cx);
            return None;
        }
        let mut request = NotificationRequest::new(
            route.target,
            NotificationSource::Cli,
            notification.title,
            notification.body,
        );
        request.subtitle = notification.subtitle.unwrap_or_default();
        Self::publish(request, cx)
    }

    pub fn target_is_focused(target: NotificationTarget, cx: &mut App) -> bool {
        let route = cx
            .global::<Self>()
            .routes
            .get(&(target.scope_id, target.item_id))
            .filter(|route| route.target == target)
            .cloned();
        route.is_some_and(|route| Self::route_is_focused(route, cx))
    }

    fn target_is_focused_in_window(target: NotificationTarget, window: &Window, cx: &App) -> bool {
        cx.global::<Self>()
            .routes
            .get(&(target.scope_id, target.item_id))
            .filter(|route| route.target == target)
            .is_some_and(|route| Self::route_is_focused_in_window(route, window, cx))
    }

    fn route_is_focused(route: TerminalRoute, cx: &mut App) -> bool {
        route
            .window
            .update(cx, |_, window, cx| {
                Self::route_is_focused_in_window(&route, window, cx)
            })
            .unwrap_or(false)
    }

    fn route_is_focused_in_window(route: &TerminalRoute, window: &Window, cx: &App) -> bool {
        if route.window != window.window_handle() || !window.is_window_active() {
            return false;
        }
        let Some(panel) = route.panel.upgrade() else {
            return false;
        };
        if panel.read(cx).active_workspace_id() != route.target.workspace_id {
            return false;
        }
        let Some(view) = route.view.upgrade() else {
            return false;
        };
        view.read(cx).focus_handle(cx).contains_focused(window, cx)
    }

    pub fn open_notification(id: NotificationId, cx: &mut App) -> bool {
        Self::open_notification_inner(id, true, cx)
    }

    fn open_notification_inner(
        id: NotificationId,
        defer_if_window_busy: bool,
        cx: &mut App,
    ) -> bool {
        let Some(notification) = crate::notifications::NotificationStore::global(cx)
            .read(cx)
            .get(id)
            .cloned()
        else {
            return false;
        };
        let target = notification.target;
        let Some(route) = cx
            .global::<Self>()
            .routes
            .get(&(target.scope_id, target.item_id))
            .filter(|route| route.target == target)
            .cloned()
        else {
            Self::dismiss_stale(id, cx);
            return false;
        };

        let activation = route.window.update(cx, |_, window, cx| {
            let Some(panel) = route.panel.upgrade() else {
                return false;
            };
            let Some(workspace) = route.workspace.upgrade() else {
                return false;
            };

            if panel.read(cx).active_workspace_id() != target.workspace_id {
                panel.update(cx, |panel, cx| {
                    panel.activate_workspace(target.workspace_id, window, cx)
                });
            }
            if panel.read(cx).active_workspace_id() != target.workspace_id {
                return false;
            }

            // Restoring a parked layout inserts its item handles immediately,
            // while Workspace's `panes_by_item` index is updated by the pane
            // events queued from those insertions. Activation runs in that
            // same transaction, so fall back to the live panes until the index
            // catches up.
            let pane = workspace.read_with(cx, |workspace, cx| {
                workspace.pane_for_item_id(target.item_id).or_else(|| {
                    workspace
                        .panes()
                        .iter()
                        .find(|pane| {
                            pane.read(cx)
                                .items()
                                .any(|item| item.item_id() == target.item_id)
                        })
                        .cloned()
                })
            });
            let Some(pane) = pane else {
                return false;
            };
            let Some(index) = pane
                .read(cx)
                .items()
                .position(|item| item.item_id() == target.item_id)
            else {
                return false;
            };
            pane.update(cx, |pane, cx| {
                pane.activate_item(index, true, true, window, cx)
            });
            window.activate_window();
            true
        });

        let activated = match activation {
            Ok(activated) => activated,
            Err(_) if defer_if_window_busy => {
                // Row clicks and keyboard actions run inside a window update.
                // Retry once after that update commits; a second failure means
                // the window is genuinely stale and is handled below.
                cx.defer(move |cx| {
                    Self::open_notification_inner(id, false, cx);
                });
                return true;
            }
            Err(_) => {
                Self::dismiss_stale(id, cx);
                return false;
            }
        };

        if !activated {
            Self::dismiss_stale(id, cx);
            return false;
        }

        Self::report_kitty_activation(id, notification.sequence, cx);
        DesktopNotificationService::retract(id, cx);
        Self::mark_notification_read(id, cx);
        true
    }

    fn open_native_notification(id: NotificationId, sequence: NotificationSequence, cx: &mut App) {
        if !Self::native_action_is_current(id, sequence, cx) {
            return;
        }
        let focus_requested = cx
            .global::<Self>()
            .kitty
            .get(id)
            .filter(|state| state.sequence == sequence)
            .is_none_or(|state| state.activation.focus);
        if focus_requested {
            Self::open_notification(id, cx);
            return;
        }

        Self::report_kitty_activation(id, sequence, cx);
        DesktopNotificationService::retract(id, cx);
        Self::mark_notification_read(id, cx);
    }

    fn native_notification_closed(
        id: NotificationId,
        sequence: NotificationSequence,
        cx: &mut App,
    ) {
        if !Self::native_action_is_current(id, sequence, cx) {
            return;
        }
        Self::report_kitty_closed(id, sequence, cx);
        Self::mark_notification_read(id, cx);
    }

    fn native_notification_expired(
        id: NotificationId,
        sequence: NotificationSequence,
        cx: &mut App,
    ) {
        if !Self::native_action_is_current(id, sequence, cx) {
            return;
        }
        // Expiry ends native and Kitty liveness just like an explicit close,
        // including a requested Kitty close report, but it is not evidence the
        // user saw the message. Keep the canonical row unread.
        Self::report_kitty_closed(id, sequence, cx);
    }

    fn native_notification_unavailable(
        id: NotificationId,
        sequence: NotificationSequence,
        cx: &mut App,
    ) {
        if !Self::native_action_is_current(id, sequence, cx) {
            return;
        }
        let untracked = {
            let kitty = &mut cx.global_mut::<Self>().kitty;
            kitty
                .mark_native_unavailable(id, sequence)
                .then(|| kitty.take_untracked_close_report(id, sequence))
                .flatten()
        };
        if let Some((target, identifier)) = untracked {
            Self::write_kitty_untracked_close(target, &identifier, cx);
        }
    }

    fn native_action_is_current(
        id: NotificationId,
        sequence: NotificationSequence,
        cx: &App,
    ) -> bool {
        crate::notifications::NotificationStore::global(cx)
            .read(cx)
            .get(id)
            .is_some_and(|notification| notification.sequence == sequence)
    }

    fn reconcile_removed_notification(notification: Notification, cx: &mut App) {
        DesktopNotificationService::retract(notification.id, cx);
        Self::report_kitty_closed(notification.id, notification.sequence, cx);
    }

    fn mark_notification_read(id: NotificationId, cx: &mut App) -> bool {
        let target = crate::notifications::NotificationStore::global(cx)
            .read(cx)
            .get(id)
            .filter(|notification| !notification.read)
            .map(|notification| notification.target);
        let changed =
            crate::notifications::NotificationStore::global(cx).update(cx, |store, store_cx| {
                let changed = store.mark_read(id);
                if changed {
                    store_cx.notify();
                }
                changed
            });
        if changed {
            Self::notify_terminal_tabs(target, cx);
        }
        changed
    }

    pub fn jump_to_latest_unread(scope_id: EntityId, cx: &mut App) -> bool {
        let store = crate::notifications::NotificationStore::global(cx);
        let ids = unread_notification_ids_for_scope(store.read(cx), scope_id);
        ids.into_iter().any(|id| Self::open_notification(id, cx))
    }

    fn mark_item_read_state(scope_id: EntityId, item_id: EntityId, cx: &mut App) -> usize {
        let ids = crate::notifications::NotificationStore::global(cx)
            .read(cx)
            .notifications()
            .filter(|notification| {
                notification.target.scope_id == scope_id
                    && notification.target.item_id == item_id
                    && !notification.read
            })
            .map(|notification| (notification.id, notification.sequence))
            .collect::<Vec<_>>();
        let changed =
            crate::notifications::NotificationStore::global(cx).update(cx, |store, store_cx| {
                let changed = store.mark_pane_read(scope_id, item_id);
                if changed > 0 {
                    store_cx.notify();
                }
                changed
            });
        for (id, sequence) in ids {
            DesktopNotificationService::retract(id, cx);
            Self::report_kitty_closed(id, sequence, cx);
        }
        if changed > 0 {
            Self::notify_terminal_tab_keys([(scope_id, item_id)], cx);
        }
        changed
    }

    fn mark_item_read_through(
        scope_id: EntityId,
        item_id: EntityId,
        max_sequence: NotificationSequence,
        cx: &mut App,
    ) -> usize {
        let ids = crate::notifications::NotificationStore::global(cx)
            .read(cx)
            .notifications()
            .filter(|notification| {
                notification.target.scope_id == scope_id
                    && notification.target.item_id == item_id
                    && notification.sequence <= max_sequence
                    && !notification.read
            })
            .map(|notification| (notification.id, notification.sequence))
            .collect::<Vec<_>>();
        let changed =
            crate::notifications::NotificationStore::global(cx).update(cx, |store, store_cx| {
                let changed = store.mark_pane_read_through(scope_id, item_id, max_sequence);
                if changed > 0 {
                    store_cx.notify();
                }
                changed
            });
        for (id, sequence) in ids {
            DesktopNotificationService::retract(id, cx);
            Self::report_kitty_closed(id, sequence, cx);
        }
        if changed > 0 {
            Self::notify_terminal_tab_keys([(scope_id, item_id)], cx);
        }
        changed
    }

    fn mark_item_read_through_if_focused(
        scope_id: EntityId,
        item_id: EntityId,
        max_sequence: NotificationSequence,
        cx: &mut App,
    ) {
        let route = cx
            .global::<Self>()
            .routes
            .get(&(scope_id, item_id))
            .cloned();
        let Some(route) = route else {
            return;
        };
        let focused = route
            .window
            .update(cx, |_, window, cx| {
                Self::route_is_focused_in_window(&route, window, cx)
            })
            .unwrap_or(false);
        if focused {
            Self::mark_item_read_through(scope_id, item_id, max_sequence, cx);
        }
    }

    pub fn dismiss_notification(id: NotificationId, cx: &mut App) -> bool {
        let notification = crate::notifications::NotificationStore::global(cx)
            .read(cx)
            .get(id)
            .cloned();
        let changed =
            crate::notifications::NotificationStore::global(cx).update(cx, |store, store_cx| {
                let changed = store.dismiss(id);
                if changed {
                    store_cx.notify();
                }
                changed
            });
        if changed {
            DesktopNotificationService::retract(id, cx);
            if let Some(notification) = notification {
                Self::report_kitty_closed(id, notification.sequence, cx);
                if !notification.read {
                    Self::notify_terminal_tabs([notification.target], cx);
                }
            }
        }
        changed
    }

    pub fn mark_scope_read(scope_id: EntityId, cx: &mut App) -> usize {
        let ids = crate::notifications::NotificationStore::global(cx)
            .read(cx)
            .notifications()
            .filter(|notification| notification.target.scope_id == scope_id && !notification.read)
            .map(|notification| (notification.id, notification.sequence, notification.target))
            .collect::<Vec<_>>();
        let changed =
            crate::notifications::NotificationStore::global(cx).update(cx, |store, store_cx| {
                let changed = ids.iter().filter(|(id, _, _)| store.mark_read(*id)).count();
                if changed > 0 {
                    store_cx.notify();
                }
                changed
            });
        if changed > 0 {
            Self::notify_terminal_tabs(ids.iter().map(|(_, _, target)| *target), cx);
        }
        for (id, sequence, _) in ids {
            DesktopNotificationService::retract(id, cx);
            Self::report_kitty_closed(id, sequence, cx);
        }
        changed
    }

    pub fn mark_workspace_read(
        scope_id: EntityId,
        workspace_id: WorkspaceId,
        cx: &mut App,
    ) -> usize {
        let ids = crate::notifications::NotificationStore::global(cx)
            .read(cx)
            .notifications()
            .filter(|notification| {
                notification.target.scope_id == scope_id
                    && notification.target.workspace_id == workspace_id
                    && !notification.read
            })
            .map(|notification| (notification.id, notification.sequence, notification.target))
            .collect::<Vec<_>>();
        let changed =
            crate::notifications::NotificationStore::global(cx).update(cx, |store, store_cx| {
                let changed = ids.iter().filter(|(id, _, _)| store.mark_read(*id)).count();
                if changed > 0 {
                    store_cx.notify();
                }
                changed
            });
        if changed > 0 {
            Self::notify_terminal_tabs(ids.iter().map(|(_, _, target)| *target), cx);
        }
        for (id, sequence, _) in ids {
            DesktopNotificationService::retract(id, cx);
            Self::report_kitty_closed(id, sequence, cx);
        }
        changed
    }

    pub fn clear_scope_notifications(scope_id: EntityId, cx: &mut App) -> usize {
        let removed = crate::notifications::NotificationStore::global(cx)
            .read(cx)
            .notifications()
            .filter(|notification| notification.target.scope_id == scope_id)
            .cloned()
            .collect::<Vec<_>>();
        let changed =
            crate::notifications::NotificationStore::global(cx).update(cx, |store, store_cx| {
                let changed = store.clear_scope(scope_id);
                if changed > 0 {
                    store_cx.notify();
                }
                changed
            });
        if changed > 0 {
            Self::notify_terminal_tabs(
                removed
                    .iter()
                    .filter(|notification| !notification.read)
                    .map(|notification| notification.target),
                cx,
            );
        }
        for notification in removed {
            Self::reconcile_removed_notification(notification, cx);
        }
        changed
    }

    pub fn clear_workspace(scope_id: EntityId, workspace_id: WorkspaceId, cx: &mut App) -> usize {
        let removed = crate::notifications::NotificationStore::global(cx)
            .read(cx)
            .notifications()
            .filter(|notification| {
                notification.target.scope_id == scope_id
                    && notification.target.workspace_id == workspace_id
            })
            .cloned()
            .collect::<Vec<_>>();
        DesktopNotificationService::retract_workspace(scope_id, workspace_id, cx);
        let removed_keys: Vec<_> = cx
            .global::<Self>()
            .routes
            .iter()
            .filter_map(|(key, route)| {
                (route.target.scope_id == scope_id && route.target.workspace_id == workspace_id)
                    .then_some(*key)
            })
            .collect();
        for key in removed_keys {
            Self::report_target_closed(key.0, key.1, cx);
            Self::remove_route_state(key, cx);
        }
        let changed =
            crate::notifications::NotificationStore::global(cx).update(cx, |store, store_cx| {
                let changed = store.clear_workspace(scope_id, workspace_id);
                if changed > 0 {
                    store_cx.notify();
                }
                changed
            });
        for notification in removed {
            Self::reconcile_removed_notification(notification, cx);
        }
        changed
    }

    fn prune_released_item(scope_id: EntityId, item_id: EntityId, cx: &mut App) {
        let key = (scope_id, item_id);
        let released = cx
            .global::<Self>()
            .routes
            .get(&key)
            .is_some_and(|route| route.view.upgrade().is_none());
        if !released {
            return;
        }
        Self::report_target_closed(scope_id, item_id, cx);
        if let Some(target) = cx
            .global::<Self>()
            .routes
            .get(&key)
            .map(|route| route.target)
        {
            DesktopNotificationService::retract_target(target, cx);
        }
        let removed = crate::notifications::NotificationStore::global(cx)
            .read(cx)
            .notifications()
            .filter(|notification| {
                notification.target.scope_id == scope_id && notification.target.item_id == item_id
            })
            .cloned()
            .collect::<Vec<_>>();
        Self::remove_route_state(key, cx);
        crate::notifications::NotificationStore::global(cx).update(cx, |store, store_cx| {
            if store.clear_pane(scope_id, item_id) > 0 {
                store_cx.notify();
            }
        });
        for notification in removed {
            Self::reconcile_removed_notification(notification, cx);
        }
    }

    fn handle_osc_title(
        scope_id: EntityId,
        item_id: EntityId,
        title: &str,
        window: &Window,
        cx: &mut App,
    ) {
        let key = (scope_id, item_id);
        let envelope = match decode_bridged_osc_title(title) {
            Ok(Some(envelope)) => envelope,
            Ok(None) | Err(_) => return,
        };
        let Some(route) = cx.global::<Self>().routes.get(&key).cloned() else {
            // Do not ACK before a route exists. Alacritty retains the replay so
            // `register_terminal` can consume an early shell notification.
            return;
        };

        let ack_sequence = envelope.ack_sequence;
        if ack_sequence.is_none()
            && let Some(restore_title) = envelope.restore_title
        {
            Self::restore_terminal_title(route.terminal.clone(), restore_title, cx);
        }

        let mut newest_sequence = cx.global::<Self>().osc_bridge_sequences.get(&key).copied();
        let mut events = Vec::new();
        for frame in envelope.frames {
            if let Some(sequence) = frame.sequence {
                if newest_sequence.is_some_and(|seen| !sequence_is_after(sequence, seen)) {
                    continue;
                }
                newest_sequence = Some(sequence);
                cx.global_mut::<Self>()
                    .osc_bridge_sequences
                    .insert(key, sequence);
            }
            let event = {
                let parser = cx.global_mut::<Self>().osc_parsers.entry(key).or_default();
                parser.push_payload(frame.payload)
            };
            if let Ok(Some(event)) = event {
                events.push(event);
            }
        }

        let target = route.target;
        let exact_target_is_focused = Self::route_is_focused_in_window(&route, window, cx);
        for event in events {
            match event {
                OscNotificationEvent::Notification(notification) => {
                    if let Some(kitty) = notification.kitty.as_ref() {
                        let should_publish = match kitty.delivery_condition {
                            KittyDeliveryCondition::Always => true,
                            KittyDeliveryCondition::Unfocused => !exact_target_is_focused,
                            // GPUI exposes keyboard focus but no trustworthy
                            // cross-platform minimized/occluded signal. We do
                            // not advertise `invisible`; if a client sends it
                            // anyway, delivering unconditionally is safer than
                            // conflating a visible unfocused window with a
                            // hidden one and suppressing its notification.
                            KittyDeliveryCondition::Invisible => true,
                        };
                        if !should_publish {
                            continue;
                        }
                    }

                    let mut request = NotificationRequest::new(
                        target,
                        notification.source,
                        notification.title,
                        notification.body,
                    );
                    request.level = notification.level;
                    if let Some(kitty) = notification.kitty.as_ref() {
                        request.subtitle = kitty.application_name.clone().unwrap_or_default();
                        request.identity = kitty
                            .identifier
                            .as_ref()
                            .map_or(NotificationIdentity::Unique, |identifier| {
                                NotificationIdentity::KittyNamed(identifier.clone())
                            });
                    }
                    // Kitty's default `o=always` is stronger than zmux's
                    // ordinary exact-focus suppression. `unfocused` was
                    // already filtered above, while the unadvertised
                    // `invisible` fallback is deliberately treated as always.
                    let force_native_delivery = notification.kitty.as_ref().is_some_and(|kitty| {
                        matches!(
                            kitty.delivery_condition,
                            KittyDeliveryCondition::Always | KittyDeliveryCondition::Invisible
                        )
                    });
                    let Some(published) = Self::publish_with_native_override(
                        request,
                        exact_target_is_focused,
                        force_native_delivery,
                        cx,
                    ) else {
                        continue;
                    };

                    if let Some(kitty) = notification.kitty {
                        let client_id = kitty.identifier;
                        Self::track_kitty_notification(
                            published.notification.id,
                            KittyRuntimeState {
                                target,
                                client_id: client_id.clone(),
                                sequence: published.notification.sequence,
                                native_alive: published.native_alive,
                                activation: kitty.activation,
                                report_close: kitty.request_close_report && published.native_alive,
                            },
                            cx,
                        );
                        if kitty.request_close_report && !published.native_alive {
                            Self::write_kitty_untracked_close(
                                target,
                                client_id.as_deref().unwrap_or("0"),
                                cx,
                            );
                        }
                    }
                }
                OscNotificationEvent::Close { identifier } => {
                    let id = cx
                        .global::<Self>()
                        .kitty
                        .named_id(scope_id, item_id, &identifier);
                    if let Some(id) = id {
                        Self::dismiss_notification(id, cx);
                    }
                }
                OscNotificationEvent::AliveQuery { identifier } => {
                    let Some(query_id) = identifier else {
                        continue;
                    };
                    let alive = cx
                        .global::<Self>()
                        .kitty
                        .alive_identifiers(scope_id, item_id);
                    Self::write_pty(
                        target,
                        format!("\x1b]99;i={query_id}:p=alive;{}\x1b\\", alive.join(","))
                            .into_bytes(),
                        cx,
                    );
                }
                OscNotificationEvent::CapabilityQuery { identifier } => {
                    Self::write_pty(
                        target,
                        Self::kitty_capability_response(identifier.as_deref()).into_bytes(),
                        cx,
                    );
                }
            }
        }

        if let Some(sequence) = ack_sequence {
            Self::ack_terminal_replay(route.terminal, sequence, cx);
        }
    }

    fn restore_terminal_title(terminal: WeakEntity<Terminal>, title: Option<String>, cx: &mut App) {
        let mut sequence = b"\x1b]2;".to_vec();
        if let Some(title) = title {
            for character in title.chars() {
                // BEL and ESC terminate an OSC sequence. They cannot occur in
                // a legitimate parsed title, but filtering them keeps the
                // restoration path safe if a future backend becomes looser.
                if character != '\x1b' && character != '\x07' {
                    let mut encoded = [0; 4];
                    sequence.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
                }
            }
        }
        sequence.extend_from_slice(b"\x1b\\");
        cx.defer(move |cx| {
            if let Some(terminal) = terminal.upgrade() {
                terminal.update(cx, |terminal, cx| terminal.write_output(&sequence, cx));
            }
        });
    }

    fn ack_terminal_replay(terminal: WeakEntity<Terminal>, watermark: u64, cx: &mut App) {
        let Some(sequence) = build_zmux_notification_replay_ack(watermark) else {
            return;
        };
        cx.defer(move |cx| {
            if let Some(terminal) = terminal.upgrade() {
                terminal.update(cx, |terminal, cx| terminal.write_output(&sequence, cx));
            }
        });
    }

    fn track_kitty_notification(id: NotificationId, state: KittyRuntimeState, cx: &mut App) {
        let evicted = cx.global_mut::<Self>().kitty.track(id, state);
        for evicted_id in evicted {
            let sequence = cx
                .global::<Self>()
                .kitty
                .get(evicted_id)
                .map(|state| state.sequence);
            let untracked = sequence.and_then(|sequence| {
                cx.global_mut::<Self>()
                    .kitty
                    .take_untracked_close_report(evicted_id, sequence)
            });
            if let Some((target, identifier)) = untracked {
                Self::write_kitty_untracked_close(target, &identifier, cx);
            }
            DesktopNotificationService::retract(evicted_id, cx);
        }
    }

    fn take_kitty_notification(
        id: NotificationId,
        sequence: NotificationSequence,
        cx: &mut App,
    ) -> Option<KittyRuntimeState> {
        let is_current = cx
            .global::<Self>()
            .kitty
            .get(id)
            .is_some_and(|state| state.sequence == sequence);
        if is_current {
            cx.global_mut::<Self>().kitty.take(id)
        } else {
            None
        }
    }

    fn report_kitty_activation(id: NotificationId, sequence: NotificationSequence, cx: &mut App) {
        let Some(state) = Self::take_kitty_notification(id, sequence, cx) else {
            return;
        };
        if state.activation.report {
            Self::write_pty(
                state.target,
                format!(
                    "\x1b]99;i={};\x1b\\",
                    state.client_id.as_deref().unwrap_or("0")
                )
                .into_bytes(),
                cx,
            );
        }
        if state.report_close {
            Self::write_kitty_close(state.target, state.client_id.as_deref().unwrap_or("0"), cx);
        }
    }

    fn report_kitty_closed(id: NotificationId, sequence: NotificationSequence, cx: &mut App) {
        let Some(state) = Self::take_kitty_notification(id, sequence, cx) else {
            return;
        };
        if state.report_close {
            Self::write_kitty_close(state.target, state.client_id.as_deref().unwrap_or("0"), cx);
        }
    }

    fn report_target_closed(scope_id: EntityId, item_id: EntityId, cx: &mut App) {
        let notifications = cx
            .global::<Self>()
            .kitty
            .ids_for_target(scope_id, item_id)
            .into_iter()
            .filter_map(|id| {
                cx.global::<Self>()
                    .kitty
                    .get(id)
                    .map(|state| (id, state.sequence))
            })
            .collect::<Vec<_>>();
        for (id, sequence) in notifications {
            Self::report_kitty_closed(id, sequence, cx);
        }
    }

    fn write_kitty_close(target: NotificationTarget, identifier: &str, cx: &mut App) {
        Self::write_pty(
            target,
            format!("\x1b]99;i={identifier}:p=close;\x1b\\").into_bytes(),
            cx,
        );
    }

    fn write_kitty_untracked_close(target: NotificationTarget, identifier: &str, cx: &mut App) {
        Self::write_pty(
            target,
            format!("\x1b]99;i={identifier}:p=close;untracked\x1b\\").into_bytes(),
            cx,
        );
    }

    fn kitty_capability_response(identifier: Option<&str>) -> String {
        format!(
            "\x1b]99;i={}:p=?;a=focus,report:c=1:o=always,unfocused:p=title,body,?,close,alive\x1b\\",
            identifier.unwrap_or("0"),
        )
    }

    fn write_pty(target: NotificationTarget, bytes: Vec<u8>, cx: &mut App) {
        let terminal = cx
            .global::<Self>()
            .routes
            .get(&(target.scope_id, target.item_id))
            .filter(|route| route.target == target)
            .and_then(|route| route.terminal.upgrade());
        if let Some(terminal) = terminal {
            let Some(response) = build_zmux_pty_response(&bytes) else {
                return;
            };
            terminal.update(cx, |terminal, cx| terminal.write_output(&response, cx));
        }
    }

    fn dismiss_stale(id: NotificationId, cx: &mut App) {
        let notification = crate::notifications::NotificationStore::global(cx)
            .read(cx)
            .get(id)
            .cloned();
        let changed =
            crate::notifications::NotificationStore::global(cx).update(cx, |store, store_cx| {
                if store.dismiss(id) {
                    store_cx.notify();
                    true
                } else {
                    false
                }
            });
        if let Some(notification) = notification {
            if changed && !notification.read {
                Self::notify_terminal_tabs([notification.target], cx);
            }
            Self::reconcile_removed_notification(notification, cx);
        }
    }

    fn clear_scope(scope_id: EntityId, cx: &mut App) {
        let removed = crate::notifications::NotificationStore::global(cx)
            .read(cx)
            .notifications()
            .filter(|notification| notification.target.scope_id == scope_id)
            .cloned()
            .collect::<Vec<_>>();
        DesktopNotificationService::retract_scope(scope_id, cx);
        let keys: Vec<_> = cx
            .global::<Self>()
            .routes
            .keys()
            .filter(|(scope, _)| *scope == scope_id)
            .copied()
            .collect();
        for key in keys {
            Self::report_target_closed(key.0, key.1, cx);
            Self::remove_route_state(key, cx);
        }
        crate::notifications::NotificationStore::global(cx).update(cx, |store, store_cx| {
            if store.clear_scope(scope_id) > 0 {
                store_cx.notify();
            }
        });
        for notification in removed {
            Self::reconcile_removed_notification(notification, cx);
        }
    }
}
