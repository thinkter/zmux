//! Non-blocking native desktop notification delivery and retraction.
//!
//! The in-app [`Notification`](crate::notifications::Notification) is the
//! canonical record. This adapter owns only the native lifecycle: it maps each
//! canonical replacement identity to the platform-issued token, filters
//! callbacks from superseded generations, and retracts the native banner when
//! the canonical record is dismissed.
//!
//! Platform support is deliberately honest:
//!
//! - Freedesktop/XDG supplies a real numeric token. Updates reuse that token,
//!   and retraction calls `CloseNotification`.
//! - macOS uses `UNUserNotificationCenter` through the modern backend shared by
//!   `notify-rust`. Stable string request identifiers provide replacement and
//!   physical removal. One async observer multiplexes every response, and a
//!   buttonless request lets the backend poll Notification Center for silent
//!   dismissals while ordinary banner clicks still activate the notification.
//!   Delivery is disabled for bare binaries because the API requires a signed
//!   application bundle.
//! - Windows uses WinRT directly. Stable tag/group pairs replace in place,
//!   toast history provides physical retraction, and WinRT event handlers
//!   report activation or dismissal without a waiter thread. The process and
//!   installer shortcut use the same AppUserModelID; an unpackaged build can
//!   therefore fail safely while the in-app row remains available.

use std::{
    collections::{HashMap, VecDeque},
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{
            self, Receiver as DispatcherReceiver, RecvTimeoutError, SyncSender, TrySendError,
            sync_channel,
        },
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use async_channel::Sender;
use gpui::{App, EntityId, Global, Task};
#[cfg(all(unix, not(target_os = "macos")))]
use notify_rust::Urgency;
#[cfg(all(unix, not(target_os = "macos")))]
use notify_rust::{Hint, Notification as NativeNotification};

use crate::notifications::{
    Notification, NotificationId, NotificationIdentity, NotificationLevel, NotificationSequence,
    NotificationTarget, WorkspaceId,
};

const DELIVERY_QUEUE_CAPACITY: usize = 64;
const CONTROL_QUEUE_CAPACITY: usize = 64;
const MAX_TRACKED_NATIVE_NOTIFICATIONS: usize = 64;
const DISPATCHER_POLL_INTERVAL: Duration = Duration::from_millis(100);
const DISPATCHER_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(250);
#[cfg(all(unix, not(target_os = "macos")))]
const XDG_EARLY_SIGNAL_CAPACITY: usize = MAX_TRACKED_NATIVE_NOTIFICATIONS * 2;
#[cfg(any(target_os = "windows", test))]
const WINDOWS_TOAST_GROUP: &str = "zmux";
#[cfg(any(all(unix, not(target_os = "macos")), test))]
const XDG_ACTIVATION_ACTIONS: [(&str, &str); 2] =
    [("default", "Open in zmux"), ("open", "Open in zmux")];

/// Identity shared by the XDG desktop entry, macOS bundle, and Windows
/// AppUserModelID. Changing it requires a coordinated packaging change.
pub const ZMUX_APPLICATION_ID: &str = "io.github.thinkter.zmux";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DesktopNotificationPolicy {
    /// Match cmux's delivery gate: retain the in-app unread row, but avoid a
    /// redundant external banner when its exact terminal is already focused.
    pub suppress_when_exact_target_focused: bool,
}

impl DesktopNotificationPolicy {
    pub fn should_deliver(self, exact_target_is_focused: bool) -> bool {
        !(self.suppress_when_exact_target_focused && exact_target_is_focused)
    }
}

impl Default for DesktopNotificationPolicy {
    fn default() -> Self {
        Self {
            suppress_when_exact_target_focused: true,
        }
    }
}

#[derive(Clone, Debug)]
struct DeliveryJob {
    id: NotificationId,
    sequence: NotificationSequence,
    key: NativeNotificationKey,
    title: String,
    subtitle: String,
    body: String,
    level: NotificationLevel,
}

impl From<&Notification> for DeliveryJob {
    fn from(notification: &Notification) -> Self {
        Self {
            id: notification.id,
            sequence: notification.sequence,
            key: NativeNotificationKey::for_notification(notification),
            title: notification.title.clone(),
            subtitle: notification.subtitle.clone(),
            body: notification.body.clone(),
            level: notification.level,
        }
    }
}

/// Replacement identity understood by the native layer. It mirrors the
/// canonical store: target-scoped events replace only the target projection,
/// named Kitty notifications replace the same client ID, and anonymous Kitty
/// notifications are never replacement candidates.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum NativeNotificationKey {
    Target(NotificationTarget),
    KittyNamed {
        target: NotificationTarget,
        identifier: String,
    },
    Unique {
        target: NotificationTarget,
        id: NotificationId,
    },
}

impl NativeNotificationKey {
    fn for_notification(notification: &Notification) -> Self {
        match &notification.identity {
            NotificationIdentity::Target => Self::Target(notification.target),
            NotificationIdentity::KittyNamed(identifier) => Self::KittyNamed {
                target: notification.target,
                identifier: identifier.clone(),
            },
            NotificationIdentity::Unique => Self::Unique {
                target: notification.target,
                id: notification.id,
            },
        }
    }

    fn target(&self) -> NotificationTarget {
        match self {
            Self::Target(target)
            | Self::KittyNamed { target, .. }
            | Self::Unique { target, .. } => *target,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeCallbackKind {
    Activated,
    Closed,
    Expired,
}

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MacPassiveCloseReason {
    Expired,
    Dismissed,
}

#[cfg(any(target_os = "macos", test))]
fn native_callback_kind_from_mac_response(
    close_reason: Option<MacPassiveCloseReason>,
) -> NativeCallbackKind {
    match close_reason {
        None => NativeCallbackKind::Activated,
        Some(MacPassiveCloseReason::Dismissed) => NativeCallbackKind::Closed,
        Some(MacPassiveCloseReason::Expired) => NativeCallbackKind::Expired,
    }
}

#[cfg(any(all(unix, not(target_os = "macos")), test))]
fn native_callback_kind_from_xdg_close_reason(reason: u32) -> NativeCallbackKind {
    // Freedesktop reason 2 is the only reason that proves an explicit user
    // dismissal. Expiry (1), CloseNotification (3), and undefined/unknown
    // reasons must preserve the canonical unread state.
    if reason == 2 {
        NativeCallbackKind::Closed
    } else {
        NativeCallbackKind::Expired
    }
}

#[cfg(any(target_os = "windows", test))]
fn native_callback_kind_from_windows_dismissal_reason(reason: i32) -> NativeCallbackKind {
    // WinRT: UserCanceled=0, ApplicationHidden=1, TimedOut=2. Only the first
    // is affirmative user dismissal; unknown future values stay conservative.
    if reason == 0 {
        NativeCallbackKind::Closed
    } else {
        NativeCallbackKind::Expired
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NativeCallback {
    key: NativeNotificationKey,
    id: NotificationId,
    sequence: NotificationSequence,
    generation: u64,
    kind: NativeCallbackKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetractionSelector {
    Notification(NotificationId),
    Target(NotificationTarget),
    Workspace {
        scope_id: EntityId,
        workspace_id: WorkspaceId,
    },
    Scope(EntityId),
    All,
}

impl RetractionSelector {
    fn matches(self, key: &NativeNotificationKey, id: NotificationId) -> bool {
        let target = key.target();
        match self {
            Self::Notification(selected) => id == selected,
            Self::Target(selected) => target == selected,
            Self::Workspace {
                scope_id,
                workspace_id,
            } => target.scope_id == scope_id && target.workspace_id == workspace_id,
            Self::Scope(scope_id) => target.scope_id == scope_id,
            Self::All => true,
        }
    }
}

struct DeliveryPermit {
    slots: Arc<AtomicUsize>,
}

impl Drop for DeliveryPermit {
    fn drop(&mut self) {
        let previous = self.slots.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "native delivery permit underflow");
    }
}

fn reserve_delivery_slot(slots: &Arc<AtomicUsize>) -> Option<DeliveryPermit> {
    slots
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
            (used < DELIVERY_QUEUE_CAPACITY).then_some(used + 1)
        })
        .ok()?;
    Some(DeliveryPermit {
        slots: slots.clone(),
    })
}

struct QueuedDelivery {
    order: u64,
    job: DeliveryJob,
    permit: DeliveryPermit,
}

enum DispatcherControl {
    Retract {
        order: u64,
        selector: RetractionSelector,
    },
    Shutdown,
}

fn enqueue_retraction_control(
    sender: &SyncSender<DispatcherControl>,
    coalesced_retract_all_order: &AtomicU64,
    order: u64,
    selector: RetractionSelector,
) -> bool {
    match sender.try_send(DispatcherControl::Retract { order, selector }) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            // Never drop cancellation under pressure, but never let a hostile
            // stream of terminal-generated closes allocate without bound. A
            // single ordered Retract(All) watermark conservatively subsumes
            // every overflowing selector. Submission ordering ensures that a
            // delivery accepted afterward has a greater order and survives.
            coalesced_retract_all_order.fetch_max(order, Ordering::Release);
            true
        }
        Err(TrySendError::Disconnected(_)) => false,
    }
}

/// App-global native notification adapter.
pub struct DesktopNotificationService {
    delivery_sender: SyncSender<QueuedDelivery>,
    control_sender: SyncSender<DispatcherControl>,
    coalesced_retract_all_order: Arc<AtomicU64>,
    delivery_slots: Arc<AtomicUsize>,
    submission_order: Mutex<u64>,
    running: Arc<AtomicBool>,
    dispatcher: Option<JoinHandle<()>>,
    dispatcher_done: DispatcherReceiver<()>,
    _action_task: Option<Task<()>>,
}

type DispatcherRunner = Box<dyn FnOnce() + Send + 'static>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesktopNotificationAction {
    Activated {
        id: NotificationId,
        sequence: NotificationSequence,
    },
    Closed {
        id: NotificationId,
        sequence: NotificationSequence,
    },
    Expired {
        id: NotificationId,
        sequence: NotificationSequence,
    },
    Unavailable {
        id: NotificationId,
        sequence: NotificationSequence,
    },
}

impl Global for DesktopNotificationService {}

impl DesktopNotificationService {
    /// Start the native adapter. `action_sender` must be an unbounded channel:
    /// the dispatcher never blocks the platform callback path, and a connected
    /// GPUI consumer must observe every sequence-qualified action.
    pub fn init(
        action_sender: Sender<DesktopNotificationAction>,
        action_task: Task<()>,
        cx: &mut App,
    ) {
        if cx.has_global::<Self>() {
            return;
        }

        // Windows requires the process AppUserModelID to be assigned during
        // initial startup, before the application presents UI. Do this
        // synchronously here; only the dispatcher's WinRT apartment belongs on
        // its background thread.
        #[cfg(target_os = "windows")]
        let windows_process_identity_error = set_windows_process_identity().err();

        cx.set_global(Self::new_with_backend_and_spawner(
            NotifyRustBackend::new(
                #[cfg(target_os = "windows")]
                windows_process_identity_error,
            ),
            action_sender,
            Some(action_task),
            |runner| {
                thread::Builder::new()
                    .name("zmux-desktop-notifications".to_owned())
                    .spawn(runner)
            },
        ));
    }

    fn new_with_backend_and_spawner<B, Spawn>(
        backend: B,
        action_sender: Sender<DesktopNotificationAction>,
        action_task: Option<Task<()>>,
        spawn: Spawn,
    ) -> Self
    where
        B: NativeBackend,
        Spawn: FnOnce(DispatcherRunner) -> io::Result<JoinHandle<()>>,
    {
        let (delivery_sender, delivery_receiver) = sync_channel(DELIVERY_QUEUE_CAPACITY);
        let (control_sender, control_receiver) = sync_channel(CONTROL_QUEUE_CAPACITY);
        let (callback_sender, callback_receiver) = mpsc::channel();
        let (dispatcher_done_sender, dispatcher_done) = sync_channel(1);
        let delivery_slots = Arc::new(AtomicUsize::new(0));
        let coalesced_retract_all_order = Arc::new(AtomicU64::new(0));
        let running = Arc::new(AtomicBool::new(true));
        let dispatcher_running = running.clone();
        let dispatcher_coalesced_retract_all_order = coalesced_retract_all_order.clone();
        let runner: DispatcherRunner = Box::new(move || {
            run_dispatcher(
                backend,
                DispatcherChannels {
                    delivery_receiver,
                    control_receiver,
                    coalesced_retract_all_order: dispatcher_coalesced_retract_all_order,
                    callback_receiver,
                    callback_sender,
                    action_sender,
                },
                dispatcher_running,
            );
            // A normal completion is distinguished from channel disconnect so
            // Drop can also recognize a panicked dispatcher as terminated.
            let _ = dispatcher_done_sender.send(());
        });
        let dispatcher = match spawn(runner) {
            Ok(dispatcher) => Some(dispatcher),
            Err(error) => {
                running.store(false, Ordering::Release);
                eprintln!(
                    "native notification dispatcher unavailable; rows remain canonical-only: \
                     {error}"
                );
                None
            }
        };

        Self {
            delivery_sender,
            control_sender,
            coalesced_retract_all_order,
            delivery_slots,
            submission_order: Mutex::new(0),
            running,
            dispatcher,
            dispatcher_done,
            _action_task: action_task,
        }
    }

    /// Queue native delivery. A `true` result means only that the bounded
    /// dispatcher accepted the request; platform delivery errors are isolated
    /// from the canonical in-app notification.
    pub fn submit(notification: &Notification, cx: &App) -> bool {
        let Some(service) = cx.try_global::<Self>() else {
            return false;
        };
        service.enqueue_delivery(notification.into())
    }

    /// Retract the banner belonging to one canonical notification generation.
    pub fn retract(id: NotificationId, cx: &App) -> bool {
        Self::enqueue_retraction(RetractionSelector::Notification(id), cx)
    }

    /// Retract whichever canonical generation currently belongs to `target`.
    pub fn retract_target(target: NotificationTarget, cx: &App) -> bool {
        Self::enqueue_retraction(RetractionSelector::Target(target), cx)
    }

    /// Retract every current banner in one sidebar workspace.
    pub fn retract_workspace(scope_id: EntityId, workspace_id: WorkspaceId, cx: &App) -> bool {
        Self::enqueue_retraction(
            RetractionSelector::Workspace {
                scope_id,
                workspace_id,
            },
            cx,
        )
    }

    /// Retract every current banner owned by an application window.
    pub fn retract_scope(scope_id: EntityId, cx: &App) -> bool {
        Self::enqueue_retraction(RetractionSelector::Scope(scope_id), cx)
    }

    fn enqueue_retraction(selector: RetractionSelector, cx: &App) -> bool {
        let Some(service) = cx.try_global::<Self>() else {
            return false;
        };
        service.enqueue_control(selector)
    }

    fn enqueue_delivery(&self, job: DeliveryJob) -> bool {
        if !self.running.load(Ordering::Acquire) {
            return false;
        }
        let mut submission_order = self
            .submission_order
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(permit) = reserve_delivery_slot(&self.delivery_slots) else {
            eprintln!("native notification delivery queue is full; row remains canonical-only");
            return false;
        };
        let order = submission_order
            .checked_add(1)
            .expect("native notification submission order exhausted");
        match self
            .delivery_sender
            .try_send(QueuedDelivery { order, job, permit })
        {
            Ok(()) => {
                *submission_order = order;
                drop(submission_order);
                self.wake_dispatcher();
                true
            }
            Err(TrySendError::Full(_)) => {
                eprintln!("native notification delivery queue is full; row remains canonical-only");
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    fn enqueue_control(&self, selector: RetractionSelector) -> bool {
        if !self.running.load(Ordering::Acquire) {
            return false;
        }
        let mut submission_order = self
            .submission_order
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let order = submission_order
            .checked_add(1)
            .expect("native notification submission order exhausted");
        if !enqueue_retraction_control(
            &self.control_sender,
            &self.coalesced_retract_all_order,
            order,
            selector,
        ) {
            return false;
        }
        *submission_order = order;
        drop(submission_order);
        self.wake_dispatcher();
        true
    }

    fn wake_dispatcher(&self) {
        if let Some(dispatcher) = &self.dispatcher {
            dispatcher.thread().unpark();
        }
    }
}

impl Drop for DesktopNotificationService {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        // Shutdown remains reliable even if this bounded best-effort wake
        // message cannot fit: `running` is the authoritative stop flag and the
        // explicit unpark below wakes the dispatcher to observe it.
        let _ = self.control_sender.try_send(DispatcherControl::Shutdown);
        self.wake_dispatcher();
        if let Some(dispatcher) = self.dispatcher.take() {
            match self
                .dispatcher_done
                .recv_timeout(DISPATCHER_SHUTDOWN_TIMEOUT)
            {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                    if dispatcher.join().is_err() {
                        eprintln!("native notification dispatcher panicked during shutdown");
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    // Dropping a JoinHandle detaches the worker. `running` is
                    // still authoritative, so a platform call that eventually
                    // returns will retract its active banners and then exit.
                    eprintln!(
                        "native notification dispatcher did not stop within {} ms; detaching",
                        DISPATCHER_SHUTDOWN_TIMEOUT.as_millis()
                    );
                    drop(dispatcher);
                }
            }
        }
    }
}

#[derive(Debug)]
struct ActiveNotification<T> {
    id: NotificationId,
    sequence: NotificationSequence,
    generation: u64,
    token: T,
}

trait NativeBackend: Send + 'static {
    type Token: Send + 'static;

    fn deliver(
        &mut self,
        job: &DeliveryJob,
        generation: u64,
        replacement: Option<&Self::Token>,
        callback_sender: &mpsc::Sender<NativeCallback>,
    ) -> Result<Self::Token, String>;

    /// Remove a delivered notification if the platform API supports it. An
    /// unsupported physical retraction is still a successful logical removal:
    /// its eventual callback is discarded by the dispatcher.
    fn retract(&mut self, token: &Self::Token) -> Result<PhysicalRetraction, String>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PhysicalRetraction {
    Retracted,
    #[cfg(test)]
    Unsupported,
}

struct NotificationDispatcher<B: NativeBackend> {
    backend: B,
    active: HashMap<NativeNotificationKey, ActiveNotification<B::Token>>,
    order: VecDeque<NativeNotificationKey>,
    callback_sender: mpsc::Sender<NativeCallback>,
    action_sender: Sender<DesktopNotificationAction>,
    #[cfg(test)]
    reported_retraction_degradation: bool,
    next_generation: u64,
}

impl<B: NativeBackend> NotificationDispatcher<B> {
    fn new(
        backend: B,
        callback_sender: mpsc::Sender<NativeCallback>,
        action_sender: Sender<DesktopNotificationAction>,
    ) -> Self {
        Self {
            backend,
            active: HashMap::new(),
            order: VecDeque::new(),
            callback_sender,
            action_sender,
            #[cfg(test)]
            reported_retraction_degradation: false,
            next_generation: 1,
        }
    }

    fn deliver(&mut self, job: DeliveryJob) {
        // A target-scoped canonical record removes every other identity owned
        // by that target. Preserve its own platform token for an in-place
        // update, but retract named/unique banners before publishing it.
        if matches!(&job.key, NativeNotificationKey::Target(_)) {
            let target = job.key.target();
            let stale_keys = self
                .active
                .keys()
                .filter(|key| key.target() == target && *key != &job.key)
                .cloned()
                .collect::<Vec<_>>();
            for key in stale_keys {
                self.retract_key(&key);
            }
        }

        let generation = self.next_generation;
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("native notification generation space exhausted");
        let supersedes_different_canonical_record = self
            .active
            .get(&job.key)
            .is_some_and(|active| active.id != job.id || active.sequence != job.sequence);
        let replacement = self.active.get(&job.key).map(|active| &active.token);
        let token = match self
            .backend
            .deliver(&job, generation, replacement, &self.callback_sender)
        {
            Ok(token) => token,
            Err(error) => {
                eprintln!("native notification delivery failed: {error}");
                self.emit_action(DesktopNotificationAction::Unavailable {
                    id: job.id,
                    sequence: job.sequence,
                });
                if supersedes_different_canonical_record {
                    self.retract_key(&job.key);
                }
                return;
            }
        };

        self.active.insert(
            job.key.clone(),
            ActiveNotification {
                id: job.id,
                sequence: job.sequence,
                generation,
                token,
            },
        );
        self.touch(job.key);
        self.enforce_capacity();
    }

    fn handle_callback(&mut self, callback: NativeCallback) {
        let Some(active) = self.active.get(&callback.key) else {
            return;
        };
        if active.id != callback.id
            || active.sequence != callback.sequence
            || active.generation != callback.generation
        {
            return;
        }

        self.active.remove(&callback.key);
        self.order.retain(|key| key != &callback.key);
        let action = match callback.kind {
            NativeCallbackKind::Activated => DesktopNotificationAction::Activated {
                id: callback.id,
                sequence: callback.sequence,
            },
            NativeCallbackKind::Closed => DesktopNotificationAction::Closed {
                id: callback.id,
                sequence: callback.sequence,
            },
            NativeCallbackKind::Expired => DesktopNotificationAction::Expired {
                id: callback.id,
                sequence: callback.sequence,
            },
        };
        self.emit_action(action);
    }

    fn retract(&mut self, selector: RetractionSelector) {
        self.retract_preserving(selector, |_| false);
    }

    fn retract_preserving(
        &mut self,
        selector: RetractionSelector,
        preserve: impl Fn(&NativeNotificationKey) -> bool,
    ) {
        let targets: Vec<_> = self
            .active
            .iter()
            .filter_map(|(key, active)| {
                (selector.matches(key, active.id) && !preserve(key)).then_some(key.clone())
            })
            .collect();
        for key in targets {
            self.retract_key(&key);
        }
    }

    fn make_unavailable(&mut self, selector: RetractionSelector) {
        let targets: Vec<_> = self
            .active
            .iter()
            .filter_map(|(key, active)| selector.matches(key, active.id).then_some(key.clone()))
            .collect();
        for key in targets {
            self.evict_key(&key);
        }
    }

    fn retract_key(&mut self, key: &NativeNotificationKey) {
        self.remove_native(key, false);
    }

    fn evict_key(&mut self, key: &NativeNotificationKey) {
        self.remove_native(key, true);
    }

    fn remove_native(&mut self, key: &NativeNotificationKey, report_unavailable: bool) {
        let Some(active) = self.active.remove(key) else {
            return;
        };
        self.order.retain(|candidate| candidate != key);
        match self.backend.retract(&active.token) {
            Ok(PhysicalRetraction::Retracted) => {}
            #[cfg(test)]
            Ok(PhysicalRetraction::Unsupported) => {
                if !self.reported_retraction_degradation {
                    self.reported_retraction_degradation = true;
                    eprintln!(
                        "this native notification backend cannot physically retract banners; \
                         stale callbacks will still be ignored"
                    );
                }
            }
            Err(error) => eprintln!("native notification retraction failed: {error}"),
        }
        if report_unavailable {
            self.emit_action(DesktopNotificationAction::Unavailable {
                id: active.id,
                sequence: active.sequence,
            });
        }
    }

    fn emit_action(&self, action: DesktopNotificationAction) {
        // `DesktopNotificationService::init` requires an unbounded action
        // channel, so `try_send` can fail only after the GPUI consumer stops.
        if self.action_sender.try_send(action).is_err() {
            eprintln!("native notification action consumer is unavailable");
        }
    }

    fn touch(&mut self, key: NativeNotificationKey) {
        self.order.retain(|candidate| candidate != &key);
        self.order.push_back(key);
    }

    fn enforce_capacity(&mut self) {
        while self.active.len() > MAX_TRACKED_NATIVE_NOTIFICATIONS {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.evict_key(&oldest);
        }
    }

    fn shutdown(&mut self) {
        self.retract(RetractionSelector::All);
    }
}

struct DispatcherChannels {
    delivery_receiver: DispatcherReceiver<QueuedDelivery>,
    control_receiver: DispatcherReceiver<DispatcherControl>,
    coalesced_retract_all_order: Arc<AtomicU64>,
    callback_receiver: mpsc::Receiver<NativeCallback>,
    callback_sender: mpsc::Sender<NativeCallback>,
    action_sender: Sender<DesktopNotificationAction>,
}

fn run_dispatcher<B: NativeBackend>(
    backend: B,
    channels: DispatcherChannels,
    running: Arc<AtomicBool>,
) {
    let DispatcherChannels {
        delivery_receiver,
        control_receiver,
        coalesced_retract_all_order,
        callback_receiver,
        callback_sender,
        action_sender,
    } = channels;
    let mut dispatcher = NotificationDispatcher::new(backend, callback_sender, action_sender);
    let mut pending_deliveries = VecDeque::with_capacity(DELIVERY_QUEUE_CAPACITY);
    loop {
        // Permits bound both this local backlog and the public delivery queue
        // together. Draining here lets a priority retraction cancel accepted
        // older work without opening an unbounded second delivery buffer.
        while let Ok(delivery) = delivery_receiver.try_recv() {
            pending_deliveries.push_back(delivery);
        }

        let mut shutdown = false;
        let mut controls_drained = 0;
        for _ in 0..CONTROL_QUEUE_CAPACITY {
            let Ok(control) = control_receiver.try_recv() else {
                break;
            };
            controls_drained += 1;
            match control {
                DispatcherControl::Retract { order, selector } => {
                    // A canonical replacement is submitted before its removed
                    // predecessor is reconciled. Controls have priority, so
                    // preserve an active token when an earlier accepted job
                    // with the same native key survives this selector; that
                    // job can then replace in place instead of seeing None.
                    dispatcher.retract_preserving(selector, |key| {
                        pending_deliveries.iter().any(|delivery| {
                            delivery.order < order
                                && &delivery.job.key == key
                                && !selector.matches(&delivery.job.key, delivery.job.id)
                        })
                    });
                    pending_deliveries.retain(|delivery| {
                        delivery.order > order
                            || !selector.matches(&delivery.job.key, delivery.job.id)
                    });
                }
                DispatcherControl::Shutdown => {
                    shutdown = true;
                    break;
                }
            }
        }
        let coalesced_order = coalesced_retract_all_order.swap(0, Ordering::AcqRel);
        if coalesced_order != 0 {
            // The overload fallback may retract unrelated native banners, so
            // report every collateral generation as unavailable. Sequence
            // checks in GPUI discard rows that the original close already
            // removed while keeping Kitty's alive state accurate for others.
            dispatcher.make_unavailable(RetractionSelector::All);
            pending_deliveries.retain(|delivery| {
                if delivery.order > coalesced_order {
                    true
                } else {
                    dispatcher.emit_action(DesktopNotificationAction::Unavailable {
                        id: delivery.job.id,
                        sequence: delivery.job.sequence,
                    });
                    false
                }
            });
        }

        if shutdown || !running.load(Ordering::Acquire) {
            pending_deliveries.clear();
            dispatcher.shutdown();
            break;
        }

        // Platform callbacks use their own unbounded channel so a full public
        // delivery queue can never drop activation or dismissal. Controls run
        // first so a canonical retraction already waiting in both channels
        // invalidates its callback before the callback can escape to GPUI.
        // Bound each callback drain for fairness; any remainder stays queued.
        for _ in 0..DELIVERY_QUEUE_CAPACITY {
            let Ok(callback) = callback_receiver.try_recv() else {
                break;
            };
            dispatcher.handle_callback(callback);
        }

        // A producer can refill a bounded control slot as it is drained. Cap
        // each batch to guarantee loop progress, while continuing before any
        // delivery whenever a full batch suggests more priority work remains.
        if controls_drained == CONTROL_QUEUE_CAPACITY {
            continue;
        }

        if let Some(delivery) = pending_deliveries.pop_front() {
            dispatcher.deliver(delivery.job);
            drop(delivery.permit);
            continue;
        }

        // `unpark` carries a one-shot token, so a producer racing this call
        // cannot lose its wakeup. The timeout also services native callbacks,
        // whose platform-owned senders intentionally know nothing about this
        // service thread.
        thread::park_timeout(DISPATCHER_POLL_INTERVAL);
    }
}

fn send_native_callback(sender: &mpsc::Sender<NativeCallback>, callback: NativeCallback) {
    if sender.send(callback).is_err() {
        eprintln!("native notification dispatcher stopped before callback delivery");
    }
}

#[derive(Debug)]
enum NotifyRustToken {
    #[cfg(all(unix, not(target_os = "macos")))]
    Xdg(u32),
    #[cfg(target_os = "macos")]
    Mac(String),
    #[cfg(target_os = "windows")]
    Windows(WindowsToastToken),
}

#[cfg(all(unix, not(target_os = "macos")))]
trait XdgShownNotification {
    fn native_id(&self) -> u32;
    fn close(self);
}

#[cfg(all(unix, not(target_os = "macos")))]
impl XdgShownNotification for notify_rust::NotificationHandle {
    fn native_id(&self) -> u32 {
        self.id()
    }

    fn close(self) {
        notify_rust::NotificationHandle::close(self);
    }
}

struct NotifyRustBackend {
    #[cfg(all(unix, not(target_os = "macos")))]
    xdg_listener: Option<XdgSignalListener>,
    #[cfg(all(unix, not(target_os = "macos")))]
    xdg_listener_error: Option<String>,
    #[cfg(all(unix, not(target_os = "macos")))]
    xdg_connection: Option<zbus::blocking::Connection>,
    #[cfg(target_os = "macos")]
    mac_bundle_ready: bool,
    #[cfg(target_os = "macos")]
    mac_authorized: Option<bool>,
    #[cfg(target_os = "macos")]
    mac_observer: Option<MacResponseObserver>,
    #[cfg(target_os = "macos")]
    mac_observer_attempted: bool,
    #[cfg(target_os = "windows")]
    windows_runtime: Option<WindowsRuntime>,
    #[cfg(target_os = "windows")]
    windows_runtime_error: Option<String>,
}

impl NotifyRustBackend {
    fn new(#[cfg(target_os = "windows")] windows_process_identity_error: Option<String>) -> Self {
        #[cfg(target_os = "macos")]
        let mac_bundle_ready = match mac_usernotifications::check_bundle() {
            Ok(()) => true,
            Err(error) => {
                eprintln!(
                    "native notifications disabled: zmux must run from its signed macOS app bundle: \
                     {error}"
                );
                false
            }
        };
        #[cfg(target_os = "windows")]
        let (windows_runtime, windows_runtime_error) =
            if let Some(error) = windows_process_identity_error {
                eprintln!("native Windows notifications disabled: {error}");
                (None, Some(error))
            } else {
                match WindowsRuntime::new() {
                    Ok(runtime) => (Some(runtime), None),
                    Err(error) => {
                        eprintln!("native Windows notifications disabled: {error}");
                        (None, Some(error))
                    }
                }
            };

        Self {
            #[cfg(all(unix, not(target_os = "macos")))]
            xdg_listener: None,
            #[cfg(all(unix, not(target_os = "macos")))]
            xdg_listener_error: None,
            #[cfg(all(unix, not(target_os = "macos")))]
            xdg_connection: None,
            #[cfg(target_os = "macos")]
            mac_bundle_ready,
            #[cfg(target_os = "macos")]
            mac_authorized: None,
            #[cfg(target_os = "macos")]
            mac_observer: None,
            #[cfg(target_os = "macos")]
            mac_observer_attempted: false,
            #[cfg(target_os = "windows")]
            windows_runtime,
            #[cfg(target_os = "windows")]
            windows_runtime_error,
        }
    }

    #[cfg(target_os = "macos")]
    fn ensure_mac_ready(
        &mut self,
        callback_sender: &mpsc::Sender<NativeCallback>,
    ) -> Result<(), String> {
        if !self.mac_bundle_ready {
            return Err(format!(
                "zmux is not running from the {ZMUX_APPLICATION_ID} macOS application bundle"
            ));
        }

        let authorized = match self.mac_authorized {
            Some(authorized) => authorized,
            None => {
                let authorized =
                    mac_usernotifications::blocking::request_auth().map_err(|error| {
                        format!("requesting macOS notification permission: {error}")
                    })?;
                self.mac_authorized = Some(authorized);
                authorized
            }
        };
        if !authorized {
            return Err("macOS notification permission was denied".to_owned());
        }

        if self.mac_observer.is_none() && !self.mac_observer_attempted {
            self.mac_observer_attempted = true;
            self.mac_observer = Some(MacResponseObserver::new(callback_sender.clone())?);
        }
        if self.mac_observer.is_none() {
            return Err("the macOS notification response observer is unavailable".to_owned());
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn deliver_mac(
        &mut self,
        job: &DeliveryJob,
        generation: u64,
        replacement: Option<&NotifyRustToken>,
        callback_sender: &mpsc::Sender<NativeCallback>,
    ) -> Result<NotifyRustToken, String> {
        use mac_usernotifications::InterruptionLevel;

        self.ensure_mac_ready(callback_sender)?;
        let mut notification = mac_usernotifications::Notification::new()
            .title(if job.title.is_empty() {
                "zmux"
            } else {
                &job.title
            })
            .message(&job.body)
            .maybe_subtitle((!job.subtitle.is_empty()).then_some(&job.subtitle))
            // Keep this buttonless. `mac-usernotifications` then races normal
            // delegate responses (including a default banner click) against a
            // delivered-notification poll that detects swipe and Clear All.
            // TimeSensitive requires a signing entitlement that the current
            // zmux bundle does not declare. Use the honest authorized baseline
            // for every level instead of requesting a capability we lack.
            .interruption_level(InterruptionLevel::Active);
        if let Some(NotifyRustToken::Mac(native_id)) = replacement {
            notification = notification.id(native_id);
        }

        let handle = notification
            .send_blocking()
            .map_err(|error| format!("posting through UNUserNotificationCenter: {error}"))?;
        let native_id = handle.notification_id().to_owned();
        let callback = NativeCallback {
            key: job.key.clone(),
            id: job.id,
            sequence: job.sequence,
            generation,
            kind: NativeCallbackKind::Closed,
        };
        if let Err(error) = self
            .mac_observer
            .as_ref()
            .expect("macOS response observer was initialized")
            .observe(handle, callback)
        {
            mac_usernotifications::blocking::close_delivered(&native_id);
            mac_usernotifications::blocking::cancel_pending(&native_id);
            return Err(error);
        }
        Ok(NotifyRustToken::Mac(native_id))
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn ensure_xdg_listener(
        &mut self,
        callback_sender: &mpsc::Sender<NativeCallback>,
    ) -> Result<(), String> {
        self.ensure_xdg_listener_with(callback_sender, XdgSignalListener::new)
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn ensure_xdg_listener_with<Create>(
        &mut self,
        callback_sender: &mpsc::Sender<NativeCallback>,
        create: Create,
    ) -> Result<(), String>
    where
        Create: FnOnce(mpsc::Sender<NativeCallback>) -> Result<XdgSignalListener, String>,
    {
        if self.xdg_listener.is_some() {
            return Ok(());
        }
        if let Some(error) = &self.xdg_listener_error {
            return Err(error.clone());
        }
        match create(callback_sender.clone()) {
            Ok(listener) => {
                self.xdg_listener = Some(listener);
                Ok(())
            }
            Err(error) => {
                self.xdg_listener_error = Some(error.clone());
                Err(error)
            }
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn show_and_track_xdg<Handle, Show>(
        &mut self,
        listener_status: Result<(), String>,
        show: Show,
        job: &DeliveryJob,
        generation: u64,
        replacement: Option<&NotifyRustToken>,
    ) -> Result<NotifyRustToken, String>
    where
        Handle: XdgShownNotification,
        Show: FnOnce() -> Result<Handle, String>,
    {
        let listener_ready = listener_status.is_ok() && self.xdg_listener.is_some();
        if listener_ready {
            self.xdg_listener
                .as_ref()
                .expect("XDG listener readiness was checked")
                .begin_show(replacement.map(|NotifyRustToken::Xdg(id)| *id));
        }
        let handle = match show() {
            Ok(handle) => handle,
            Err(error) => {
                if listener_ready {
                    self.xdg_listener
                        .as_ref()
                        .expect("XDG listener readiness was checked")
                        .cancel_show();
                }
                return Err(error);
            }
        };
        let native_id = handle.native_id();
        let listener_error = listener_status.err().or_else(|| {
            self.xdg_listener
                .is_none()
                .then(|| "the XDG signal listener disappeared".to_owned())
        });
        if let Some(error) = listener_error {
            // A banner without ActionInvoked/NotificationClosed tracking must
            // never be advertised to the canonical runtime as alive. Close
            // the successful show immediately, then let the dispatcher emit
            // sequence-qualified Unavailable for the in-app record.
            handle.close();
            return Err(format!(
                "XDG notification {native_id} was shown then closed because callbacks are \
                 unavailable: {error}"
            ));
        }

        let previous_id = replacement.map(|NotifyRustToken::Xdg(id)| *id);
        self.xdg_listener
            .as_mut()
            .expect("XDG listener status was checked")
            .register(
                native_id,
                previous_id,
                NativeCallback {
                    key: job.key.clone(),
                    id: job.id,
                    sequence: job.sequence,
                    generation,
                    kind: NativeCallbackKind::Closed,
                },
            );
        drop(handle);
        Ok(NotifyRustToken::Xdg(native_id))
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn close_xdg(&mut self, id: u32) -> Result<(), String> {
        if let Some(listener) = &mut self.xdg_listener {
            listener.unregister(id);
            return listener.close_notification(id);
        }
        if self.xdg_connection.is_none() {
            self.xdg_connection = Some(
                zbus::blocking::Connection::session()
                    .map_err(|error| format!("opening the session bus: {error}"))?,
            );
        }
        self.xdg_connection
            .as_ref()
            .expect("XDG connection was initialized")
            .call_method(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "CloseNotification",
                &(id),
            )
            .map(|_| ())
            .map_err(|error| format!("closing XDG notification {id}: {error}"))
    }
}

impl NativeBackend for NotifyRustBackend {
    type Token = NotifyRustToken;

    fn deliver(
        &mut self,
        job: &DeliveryJob,
        generation: u64,
        replacement: Option<&Self::Token>,
        callback_sender: &mpsc::Sender<NativeCallback>,
    ) -> Result<Self::Token, String> {
        #[cfg(target_os = "macos")]
        {
            self.deliver_mac(job, generation, replacement, callback_sender)
        }

        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let mut builder = NativeNotification::new();
            builder
                .appname("zmux")
                .hint(Hint::DesktopEntry(ZMUX_APPLICATION_ID.to_owned()))
                .summary(if job.title.is_empty() {
                    "zmux"
                } else {
                    &job.title
                })
                .body(&native_body(&job.body));
            for (identifier, label) in XDG_ACTIVATION_ACTIONS {
                builder.action(identifier, label);
            }

            let listener_status = self.ensure_xdg_listener(callback_sender);
            builder.urgency(match job.level {
                NotificationLevel::Error => Urgency::Critical,
                NotificationLevel::Warning
                | NotificationLevel::Info
                | NotificationLevel::Success => Urgency::Normal,
            });
            if let Some(NotifyRustToken::Xdg(id)) = replacement {
                builder.id(*id);
            }

            if !job.subtitle.is_empty() {
                builder.subtitle(&job.subtitle);
            }

            self.show_and_track_xdg(
                listener_status,
                || builder.show().map_err(|error| error.to_string()),
                job,
                generation,
                replacement,
            )
        }

        #[cfg(target_os = "windows")]
        {
            if self.windows_runtime.is_none() {
                return Err(self
                    .windows_runtime_error
                    .clone()
                    .unwrap_or_else(|| "the Windows notification runtime is unavailable".into()));
            }
            let replacement = replacement.map(|token| {
                let NotifyRustToken::Windows(token) = token;
                &token.identity
            });
            deliver_windows_notification(job, generation, replacement, callback_sender)
                .map(NotifyRustToken::Windows)
        }
    }

    fn retract(&mut self, token: &Self::Token) -> Result<PhysicalRetraction, String> {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let NotifyRustToken::Xdg(id) = token;
            self.close_xdg(*id)?;
            Ok(PhysicalRetraction::Retracted)
        }
        #[cfg(target_os = "macos")]
        {
            let NotifyRustToken::Mac(native_id) = token;
            if let Some(observer) = &self.mac_observer {
                observer.cancel(native_id)?;
            }
            mac_usernotifications::blocking::close_delivered(native_id);
            mac_usernotifications::blocking::cancel_pending(native_id);
            Ok(PhysicalRetraction::Retracted)
        }
        #[cfg(target_os = "windows")]
        {
            let NotifyRustToken::Windows(token) = token;
            retract_windows_notification(&token.identity)?;
            Ok(PhysicalRetraction::Retracted)
        }
    }
}

#[cfg(target_os = "macos")]
enum MacObserverCommand {
    Observe {
        handle: mac_usernotifications::NotificationHandle,
        callback: NativeCallback,
    },
    Cancel {
        native_id: String,
    },
}

#[cfg(target_os = "macos")]
struct MacResponseObserver {
    sender: Sender<MacObserverCommand>,
    listener: Option<JoinHandle<()>>,
}

#[cfg(target_os = "macos")]
impl MacResponseObserver {
    fn new(callback_sender: mpsc::Sender<NativeCallback>) -> Result<Self, String> {
        let (sender, receiver) = async_channel::bounded(MAX_TRACKED_NATIVE_NOTIFICATIONS * 2);
        let listener = thread::Builder::new()
            .name("zmux-macos-notification-responses".to_owned())
            .spawn(move || {
                mac_usernotifications::block_on(run_mac_response_observer(
                    receiver,
                    callback_sender,
                ));
            })
            .map_err(|error| format!("starting the macOS response observer: {error}"))?;
        Ok(Self {
            sender,
            listener: Some(listener),
        })
    }

    fn observe(
        &self,
        handle: mac_usernotifications::NotificationHandle,
        callback: NativeCallback,
    ) -> Result<(), String> {
        self.sender
            .send_blocking(MacObserverCommand::Observe { handle, callback })
            .map_err(|_| "the macOS response observer stopped".to_owned())
    }

    fn cancel(&self, native_id: &str) -> Result<(), String> {
        self.sender
            .send_blocking(MacObserverCommand::Cancel {
                native_id: native_id.to_owned(),
            })
            .map_err(|_| "the macOS response observer stopped".to_owned())
    }
}

#[cfg(target_os = "macos")]
impl Drop for MacResponseObserver {
    fn drop(&mut self) {
        self.sender.close();
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
    }
}

#[cfg(target_os = "macos")]
struct MacResponseCompletion {
    native_id: String,
    generation: u64,
    callback: NativeCallback,
    response: Result<
        Result<mac_usernotifications::NotificationResponse, mac_usernotifications::Error>,
        futures_util::future::Aborted,
    >,
}

#[cfg(target_os = "macos")]
type MacResponseFuture = futures_util::future::LocalBoxFuture<'static, MacResponseCompletion>;

#[cfg(target_os = "macos")]
fn enqueue_mac_observation(
    command: MacObserverCommand,
    pending: &mut futures_util::stream::FuturesUnordered<MacResponseFuture>,
    aborts: &mut HashMap<String, (u64, futures_util::future::AbortHandle)>,
) {
    use futures_util::FutureExt;

    match command {
        MacObserverCommand::Observe { handle, callback } => {
            let native_id = handle.notification_id().to_owned();
            let generation = callback.generation;
            let (abort, registration) = futures_util::future::AbortHandle::new_pair();
            if let Some((_, previous)) = aborts.insert(native_id.clone(), (generation, abort)) {
                previous.abort();
            }
            pending.push(
                async move {
                    let response =
                        futures_util::future::Abortable::new(handle.response(), registration).await;
                    MacResponseCompletion {
                        native_id,
                        generation,
                        callback,
                        response,
                    }
                }
                .boxed_local(),
            );
        }
        MacObserverCommand::Cancel { native_id } => {
            if let Some((_, abort)) = aborts.remove(&native_id) {
                abort.abort();
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn finish_mac_response(
    mut completion: MacResponseCompletion,
    aborts: &mut HashMap<String, (u64, futures_util::future::AbortHandle)>,
    callback_sender: &mpsc::Sender<NativeCallback>,
) {
    let is_current = aborts
        .get(&completion.native_id)
        .is_some_and(|(generation, _)| *generation == completion.generation);
    if !is_current {
        return;
    }
    aborts.remove(&completion.native_id);

    let response = match completion.response {
        Err(_) => return,
        Ok(Err(error)) => {
            eprintln!("macOS notification response failed: {error}");
            return;
        }
        Ok(Ok(response)) => response,
    };
    let close_reason = response.close_reason.map(|reason| match reason {
        mac_usernotifications::response::CloseReason::Expired => MacPassiveCloseReason::Expired,
        mac_usernotifications::response::CloseReason::Dismissed => MacPassiveCloseReason::Dismissed,
    });
    completion.callback.kind = native_callback_kind_from_mac_response(close_reason);
    send_native_callback(callback_sender, completion.callback);
}

#[cfg(target_os = "macos")]
async fn run_mac_response_observer(
    receiver: async_channel::Receiver<MacObserverCommand>,
    callback_sender: mpsc::Sender<NativeCallback>,
) {
    use futures_util::{FutureExt, StreamExt, future::Either};

    enum Event {
        Command(Result<MacObserverCommand, async_channel::RecvError>),
        Response(Option<MacResponseCompletion>),
    }

    let mut pending = futures_util::stream::FuturesUnordered::<MacResponseFuture>::new();
    let mut aborts = HashMap::<String, (u64, futures_util::future::AbortHandle)>::new();

    loop {
        if pending.is_empty() {
            let Ok(command) = receiver.recv().await else {
                break;
            };
            enqueue_mac_observation(command, &mut pending, &mut aborts);
            continue;
        }

        // Convert the selected result into an owned event in a nested scope.
        // The losing future borrows `pending`, so it must be dropped before
        // either branch mutates that set.
        let event = {
            let command_wait = receiver.recv().boxed_local();
            let response_wait = pending.next().boxed_local();
            match futures_util::future::select(command_wait, response_wait).await {
                Either::Left((command, response_wait)) => {
                    drop(response_wait);
                    Event::Command(command)
                }
                Either::Right((completion, command_wait)) => {
                    drop(command_wait);
                    Event::Response(completion)
                }
            }
        };

        match event {
            Event::Command(command) => {
                let Ok(command) = command else {
                    break;
                };
                enqueue_mac_observation(command, &mut pending, &mut aborts);
            }
            Event::Response(completion) => {
                if let Some(completion) = completion {
                    finish_mac_response(completion, &mut aborts, &callback_sender);
                }
            }
        }
    }

    for (_, (_, abort)) in aborts {
        abort.abort();
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
struct XdgSignalListener {
    connection: Option<zbus::blocking::Connection>,
    state: Arc<Mutex<XdgSignalState>>,
    callback_sender: mpsc::Sender<NativeCallback>,
    listener: Option<JoinHandle<()>>,
}

#[cfg(all(unix, not(target_os = "macos")))]
#[derive(Default)]
struct XdgSignalState {
    callbacks: HashMap<u32, NativeCallback>,
    early_signals: HashMap<u32, NativeCallbackKind>,
    early_signal_order: VecDeque<u32>,
    suspended_callback: Option<(u32, NativeCallback)>,
    show_in_flight: bool,
}

#[cfg(all(unix, not(target_os = "macos")))]
fn dispatch_xdg_signal(
    state: &Arc<Mutex<XdgSignalState>>,
    callback_sender: &mpsc::Sender<NativeCallback>,
    native_id: u32,
    kind: NativeCallbackKind,
) {
    let callback = {
        let mut state = state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(mut callback) = state.callbacks.remove(&native_id) {
            callback.kind = kind;
            Some(callback)
        } else {
            // Unknown daemon IDs are ignored except during the narrow interval
            // between Notify returning an ID and dispatcher registration. The
            // fixed-size first-signal map closes that race without allowing a
            // hostile signal stream to allocate without bound.
            if state.show_in_flight && !state.early_signals.contains_key(&native_id) {
                while state.early_signals.len() >= XDG_EARLY_SIGNAL_CAPACITY {
                    let Some(oldest) = state.early_signal_order.pop_front() else {
                        break;
                    };
                    state.early_signals.remove(&oldest);
                }
                state.early_signals.insert(native_id, kind);
                state.early_signal_order.push_back(native_id);
            }
            None
        }
    };
    if let Some(callback) = callback {
        send_native_callback(callback_sender, callback);
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
impl XdgSignalListener {
    fn new(callback_sender: mpsc::Sender<NativeCallback>) -> Result<Self, String> {
        let connection = zbus::blocking::Connection::session()
            .map_err(|error| format!("opening the session bus: {error}"))?;
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender("org.freedesktop.Notifications")
            .map_err(|error| format!("constraining the XDG notification signal sender: {error}"))?
            .path("/org/freedesktop/Notifications")
            .map_err(|error| format!("constraining the XDG notification signal path: {error}"))?
            .interface("org.freedesktop.Notifications")
            .map_err(|error| format!("building the XDG notification signal rule: {error}"))?
            .build();
        let messages = zbus::blocking::MessageIterator::for_match_rule(
            rule,
            &connection,
            Some(MAX_TRACKED_NATIVE_NOTIFICATIONS * 2),
        )
        .map_err(|error| format!("subscribing to XDG notification signals: {error}"))?;
        let state = Arc::new(Mutex::new(XdgSignalState::default()));
        let listener_state = state.clone();
        let listener_callback_sender = callback_sender.clone();
        let listener = thread::Builder::new()
            .name("zmux-xdg-notification-signals".to_owned())
            .spawn(move || {
                for message in messages {
                    let Ok(message) = message else {
                        break;
                    };
                    let header = message.header();
                    let Some(member) = header.member() else {
                        continue;
                    };
                    let parsed = if member.as_str() == "ActionInvoked" {
                        message
                            .body()
                            .deserialize::<(u32, String)>()
                            .ok()
                            .map(|(id, _)| (id, NativeCallbackKind::Activated))
                    } else if member.as_str() == "NotificationClosed" {
                        message
                            .body()
                            .deserialize::<(u32, u32)>()
                            .ok()
                            .map(|(id, reason)| {
                                (id, native_callback_kind_from_xdg_close_reason(reason))
                            })
                    } else {
                        None
                    };
                    let Some((id, kind)) = parsed else {
                        continue;
                    };
                    dispatch_xdg_signal(&listener_state, &listener_callback_sender, id, kind);
                }
            })
            .map_err(|error| format!("starting the XDG signal listener: {error}"))?;

        Ok(Self {
            connection: Some(connection),
            state,
            callback_sender,
            listener: Some(listener),
        })
    }

    #[cfg(test)]
    fn for_test(callback_sender: mpsc::Sender<NativeCallback>) -> Self {
        Self {
            connection: None,
            state: Arc::new(Mutex::new(XdgSignalState::default())),
            callback_sender,
            listener: None,
        }
    }

    fn begin_show(&self, replacement_id: Option<u32>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        debug_assert!(!state.show_in_flight, "XDG shows are serialized");
        state.show_in_flight = true;
        state.early_signals.clear();
        state.early_signal_order.clear();
        state.suspended_callback = replacement_id.and_then(|native_id| {
            state
                .callbacks
                .remove(&native_id)
                .map(|callback| (native_id, callback))
        });
    }

    fn cancel_show(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.show_in_flight = false;
        state.early_signals.clear();
        state.early_signal_order.clear();
        if let Some((native_id, callback)) = state.suspended_callback.take() {
            state.callbacks.insert(native_id, callback);
        }
    }

    fn register(&mut self, native_id: u32, previous_id: Option<u32>, callback: NativeCallback) {
        let ready_callback = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.show_in_flight = false;
            state.suspended_callback = None;
            if let Some(previous_id) = previous_id.filter(|previous_id| *previous_id != native_id) {
                state.callbacks.remove(&previous_id);
                state.early_signals.remove(&previous_id);
                state
                    .early_signal_order
                    .retain(|candidate| *candidate != previous_id);
            }
            let early_kind = state.early_signals.remove(&native_id);
            state.callbacks.remove(&native_id);
            state.early_signal_order.clear();
            state.early_signals.clear();
            if let Some(kind) = early_kind {
                let mut callback = callback;
                callback.kind = kind;
                Some(callback)
            } else {
                state.callbacks.insert(native_id, callback);
                None
            }
        };
        if let Some(callback) = ready_callback {
            send_native_callback(&self.callback_sender, callback);
        }
    }

    fn unregister(&mut self, native_id: u32) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.callbacks.remove(&native_id);
        state.early_signals.remove(&native_id);
        state
            .early_signal_order
            .retain(|candidate| *candidate != native_id);
    }

    fn close_notification(&self, native_id: u32) -> Result<(), String> {
        self.connection
            .as_ref()
            .expect("XDG signal listener connection is available")
            .call_method(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "CloseNotification",
                &(native_id),
            )
            .map(|_| ())
            .map_err(|error| format!("closing XDG notification {native_id}: {error}"))
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
impl Drop for XdgSignalListener {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            let _ = connection.close();
        }
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
    }
}

#[cfg(any(target_os = "windows", test))]
#[derive(Clone, Debug, PartialEq, Eq)]
struct WindowsToastIdentity {
    tag: String,
    group: String,
}

#[cfg(target_os = "windows")]
#[derive(Debug)]
struct WindowsToastToken {
    identity: WindowsToastIdentity,
    toast: windows::UI::Notifications::ToastNotification,
    activated: i64,
    dismissed: i64,
}

#[cfg(target_os = "windows")]
impl Drop for WindowsToastToken {
    fn drop(&mut self) {
        let _ = self.toast.RemoveActivated(self.activated);
        let _ = self.toast.RemoveDismissed(self.dismissed);
    }
}

#[cfg(target_os = "windows")]
struct WindowsRuntime;

#[cfg(target_os = "windows")]
fn set_windows_process_identity() -> Result<(), String> {
    use windows::{Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID, core::PCWSTR};

    let mut application_id = ZMUX_APPLICATION_ID.encode_utf16().collect::<Vec<_>>();
    application_id.push(0);
    unsafe { SetCurrentProcessExplicitAppUserModelID(PCWSTR(application_id.as_ptr())) }.map_err(
        |error| format!("setting process AppUserModelID to {ZMUX_APPLICATION_ID}: {error}"),
    )
}

#[cfg(target_os = "windows")]
impl WindowsRuntime {
    fn new() -> Result<Self, String> {
        use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};

        // The dispatcher is a dedicated thread, so it owns one matching WinRT
        // apartment initialization for the backend's entire lifetime.
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }
            .map_err(|error| format!("initializing WinRT: {error}"))?;
        Ok(Self)
    }
}

#[cfg(target_os = "windows")]
impl Drop for WindowsRuntime {
    fn drop(&mut self) {
        unsafe { windows::Win32::System::WinRT::RoUninitialize() };
    }
}

#[cfg(any(target_os = "windows", test))]
fn windows_toast_identity(
    key: &NativeNotificationKey,
    replacement: Option<&WindowsToastIdentity>,
) -> WindowsToastIdentity {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;

    if let Some(replacement) = replacement {
        return replacement.clone();
    }

    let mut digest = Sha256::new();
    let target = key.target();
    digest.update(target.scope_id.as_u64().to_le_bytes());
    digest.update(target.workspace_id.to_le_bytes());
    digest.update(target.item_id.as_u64().to_le_bytes());
    match key {
        NativeNotificationKey::Target(_) => digest.update([0]),
        NativeNotificationKey::KittyNamed { identifier, .. } => {
            digest.update([1]);
            digest.update((identifier.len() as u64).to_le_bytes());
            digest.update(identifier.as_bytes());
        }
        NativeNotificationKey::Unique { id, .. } => {
            digest.update([2]);
            digest.update(id.to_le_bytes());
        }
    }

    // WinRT limits both tag and group to 16 characters. Eight digest bytes
    // provide a stable 64-bit replacement identity without exposing IPC text.
    let digest = digest.finalize();
    let mut tag = String::with_capacity(16);
    for byte in &digest[..8] {
        write!(&mut tag, "{byte:02x}").expect("writing to a String cannot fail");
    }
    WindowsToastIdentity {
        tag,
        group: WINDOWS_TOAST_GROUP.to_owned(),
    }
}

#[cfg(any(target_os = "windows", test))]
fn windows_toast_xml(job: &DeliveryJob) -> String {
    let title = escape_xml_text(if job.title.is_empty() {
        "zmux"
    } else {
        &job.title
    });
    let subtitle = if job.subtitle.is_empty() {
        String::new()
    } else {
        format!("<text>{}</text>", escape_xml_text(&job.subtitle))
    };
    let body = if job.body.is_empty() {
        String::new()
    } else {
        format!("<text>{}</text>", escape_xml_text(&job.body))
    };
    let attributes = if job.level == NotificationLevel::Error {
        r#"duration="long" scenario="reminder""#
    } else {
        r#"duration="short""#
    };
    format!(
        r#"<toast {attributes} launch="open"><visual><binding template="ToastGeneric"><text>{title}</text>{subtitle}{body}</binding></visual><actions><action content="Open in zmux" arguments="open" activationType="foreground"/></actions></toast>"#
    )
}

#[cfg(target_os = "windows")]
fn deliver_windows_notification(
    job: &DeliveryJob,
    generation: u64,
    replacement: Option<&WindowsToastIdentity>,
    callback_sender: &mpsc::Sender<NativeCallback>,
) -> Result<WindowsToastToken, String> {
    use windows::{
        Data::Xml::Dom::XmlDocument,
        Foundation::TypedEventHandler,
        UI::Notifications::{ToastDismissedEventArgs, ToastNotification, ToastNotificationManager},
        core::{HSTRING, IInspectable},
    };

    let identity = windows_toast_identity(&job.key, replacement);
    let document = XmlDocument::new().map_err(|error| error.to_string())?;
    document
        .LoadXml(&HSTRING::from(windows_toast_xml(job)))
        .map_err(|error| format!("parsing Windows toast XML: {error}"))?;
    let toast = ToastNotification::CreateToastNotification(&document)
        .map_err(|error| format!("creating Windows toast: {error}"))?;
    toast
        .SetTag(&HSTRING::from(&identity.tag))
        .map_err(|error| format!("setting Windows toast tag: {error}"))?;
    toast
        .SetGroup(&HSTRING::from(&identity.group))
        .map_err(|error| format!("setting Windows toast group: {error}"))?;

    let activated_sender = callback_sender.clone();
    let activated = NativeCallback {
        key: job.key.clone(),
        id: job.id,
        sequence: job.sequence,
        generation,
        kind: NativeCallbackKind::Activated,
    };
    let dismissed_sender = callback_sender.clone();
    let dismissed = NativeCallback {
        kind: NativeCallbackKind::Closed,
        ..activated.clone()
    };
    let activated_handler =
        TypedEventHandler::<ToastNotification, IInspectable>::new(move |_, _| {
            send_native_callback(&activated_sender, activated.clone());
            Ok(())
        });
    let dismissed_handler =
        TypedEventHandler::<ToastNotification, ToastDismissedEventArgs>::new(move |_, args| {
            let kind = args
                .ok()
                .and_then(|args| args.Reason())
                .map(|reason| native_callback_kind_from_windows_dismissal_reason(reason.0))
                // Failure to read a reason is not proof of user dismissal.
                .unwrap_or(NativeCallbackKind::Expired);
            let mut callback = dismissed.clone();
            callback.kind = kind;
            send_native_callback(&dismissed_sender, callback);
            Ok(())
        });
    let activated = toast
        .Activated(&activated_handler)
        .map_err(|error| format!("registering Windows activation callback: {error}"))?;
    let dismissed = match toast.Dismissed(&dismissed_handler) {
        Ok(token) => token,
        Err(error) => {
            let _ = toast.RemoveActivated(activated);
            return Err(format!("registering Windows dismissal callback: {error}"));
        }
    };

    let notifier =
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(ZMUX_APPLICATION_ID))
            .map_err(|error| format!("creating Windows toast notifier: {error}"))?;
    if let Err(error) = notifier.Show(&toast) {
        let _ = toast.RemoveActivated(activated);
        let _ = toast.RemoveDismissed(dismissed);
        return Err(format!("showing Windows toast: {error}"));
    }

    Ok(WindowsToastToken {
        identity,
        toast,
        activated,
        dismissed,
    })
}

#[cfg(target_os = "windows")]
fn retract_windows_notification(identity: &WindowsToastIdentity) -> Result<(), String> {
    use windows::{UI::Notifications::ToastNotificationManager, core::HSTRING};

    ToastNotificationManager::History()
        .and_then(|history| {
            history.RemoveGroupedTagWithId(
                &HSTRING::from(&identity.tag),
                &HSTRING::from(&identity.group),
                &HSTRING::from(ZMUX_APPLICATION_ID),
            )
        })
        .map_err(|error| format!("removing Windows toast from history: {error}"))
}

fn native_body(body: &str) -> String {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        escape_xdg_body_markup(body)
    }
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        body.to_owned()
    }
}

/// XDG notification bodies may interpret a small XML-like markup language.
/// Terminal and IPC text is plain text, so escape its metacharacters before it
/// reaches an implementation advertising `body-markup`.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
fn escape_xdg_body_markup(body: &str) -> String {
    escape_xml_text(body)
}

#[cfg(any(all(unix, not(target_os = "macos")), target_os = "windows", test))]
fn escape_xml_text(body: &str) -> String {
    let mut escaped = String::with_capacity(body.len());
    for character in body.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use std::{sync::Mutex, time::Instant};

    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum FakeOperation {
        Deliver {
            id: NotificationId,
            replacement: Option<u64>,
            token: u64,
        },
        Retract(u64),
    }

    #[derive(Default)]
    struct FakeBackend {
        next_token: u64,
        operations: Arc<Mutex<Vec<FakeOperation>>>,
        physical_retraction: bool,
        fail_delivery: bool,
    }

    impl NativeBackend for FakeBackend {
        type Token = u64;

        fn deliver(
            &mut self,
            job: &DeliveryJob,
            _generation: u64,
            replacement: Option<&Self::Token>,
            _callback_sender: &mpsc::Sender<NativeCallback>,
        ) -> Result<Self::Token, String> {
            self.next_token += 1;
            self.operations
                .lock()
                .unwrap()
                .push(FakeOperation::Deliver {
                    id: job.id,
                    replacement: replacement.copied(),
                    token: self.next_token,
                });
            if self.fail_delivery {
                return Err("injected native delivery failure".to_owned());
            }
            Ok(self.next_token)
        }

        fn retract(&mut self, token: &Self::Token) -> Result<PhysicalRetraction, String> {
            self.operations
                .lock()
                .unwrap()
                .push(FakeOperation::Retract(*token));
            Ok(if self.physical_retraction {
                PhysicalRetraction::Retracted
            } else {
                PhysicalRetraction::Unsupported
            })
        }
    }

    struct BlockingBackend {
        entered_delivery: mpsc::SyncSender<()>,
        release_delivery: mpsc::Receiver<()>,
        retracted: mpsc::SyncSender<()>,
        dropped: mpsc::SyncSender<()>,
    }

    impl NativeBackend for BlockingBackend {
        type Token = u64;

        fn deliver(
            &mut self,
            _job: &DeliveryJob,
            _generation: u64,
            _replacement: Option<&Self::Token>,
            _callback_sender: &mpsc::Sender<NativeCallback>,
        ) -> Result<Self::Token, String> {
            self.entered_delivery
                .send(())
                .map_err(|_| "blocking test observer disconnected".to_owned())?;
            self.release_delivery
                .recv()
                .map_err(|_| "blocking test release disconnected".to_owned())?;
            Ok(1)
        }

        fn retract(&mut self, _token: &Self::Token) -> Result<PhysicalRetraction, String> {
            let _ = self.retracted.try_send(());
            Ok(PhysicalRetraction::Retracted)
        }
    }

    impl Drop for BlockingBackend {
        fn drop(&mut self) {
            let _ = self.dropped.try_send(());
        }
    }

    struct GatedFirstDeliveryBackend {
        next_token: u64,
        operations: Arc<Mutex<Vec<FakeOperation>>>,
        first_delivery_started: mpsc::SyncSender<()>,
        release_first_delivery: mpsc::Receiver<()>,
        first_delivery_gated: bool,
    }

    impl NativeBackend for GatedFirstDeliveryBackend {
        type Token = u64;

        fn deliver(
            &mut self,
            job: &DeliveryJob,
            _generation: u64,
            replacement: Option<&Self::Token>,
            _callback_sender: &mpsc::Sender<NativeCallback>,
        ) -> Result<Self::Token, String> {
            self.next_token += 1;
            self.operations
                .lock()
                .unwrap()
                .push(FakeOperation::Deliver {
                    id: job.id,
                    replacement: replacement.copied(),
                    token: self.next_token,
                });
            if !self.first_delivery_gated {
                self.first_delivery_gated = true;
                self.first_delivery_started
                    .send(())
                    .map_err(|_| "replacement-race observer disconnected".to_owned())?;
                self.release_first_delivery
                    .recv()
                    .map_err(|_| "replacement-race release disconnected".to_owned())?;
            }
            Ok(self.next_token)
        }

        fn retract(&mut self, token: &Self::Token) -> Result<PhysicalRetraction, String> {
            self.operations
                .lock()
                .unwrap()
                .push(FakeOperation::Retract(*token));
            Ok(PhysicalRetraction::Retracted)
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    struct FakeXdgHandle {
        native_id: u32,
        closed: Arc<AtomicBool>,
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    impl XdgShownNotification for FakeXdgHandle {
        fn native_id(&self) -> u32 {
            self.native_id
        }

        fn close(self) {
            self.closed.store(true, Ordering::Release);
        }
    }

    fn target(value: u64) -> NotificationTarget {
        NotificationTarget {
            scope_id: EntityId::from(value),
            workspace_id: value,
            item_id: EntityId::from(value + 100),
        }
    }

    fn job(id: NotificationId, target: NotificationTarget) -> DeliveryJob {
        job_with_sequence(id, id, target)
    }

    fn job_with_sequence(
        id: NotificationId,
        sequence: NotificationSequence,
        target: NotificationTarget,
    ) -> DeliveryJob {
        DeliveryJob {
            id,
            sequence,
            key: NativeNotificationKey::Target(target),
            title: "Build".to_owned(),
            subtitle: "worker".to_owned(),
            body: "Done".to_owned(),
            level: NotificationLevel::Info,
        }
    }

    fn named_job(id: NotificationId, target: NotificationTarget, identifier: &str) -> DeliveryJob {
        DeliveryJob {
            key: NativeNotificationKey::KittyNamed {
                target,
                identifier: identifier.to_owned(),
            },
            ..job(id, target)
        }
    }

    fn unique_job(id: NotificationId, target: NotificationTarget) -> DeliveryJob {
        DeliveryJob {
            key: NativeNotificationKey::Unique { target, id },
            ..job(id, target)
        }
    }

    fn dispatcher(
        backend: FakeBackend,
    ) -> (
        NotificationDispatcher<FakeBackend>,
        async_channel::Receiver<DesktopNotificationAction>,
        mpsc::Receiver<NativeCallback>,
    ) {
        let (callback_sender, callback_receiver) = mpsc::channel();
        let (action_sender, action_receiver) = async_channel::unbounded();
        (
            NotificationDispatcher::new(backend, callback_sender, action_sender),
            action_receiver,
            callback_receiver,
        )
    }

    #[test]
    fn focused_target_suppresses_only_native_delivery() {
        let policy = DesktopNotificationPolicy::default();
        assert!(!policy.should_deliver(true));
        assert!(policy.should_deliver(false));
    }

    #[test]
    fn policy_can_explicitly_allow_focused_delivery() {
        let policy = DesktopNotificationPolicy {
            suppress_when_exact_target_focused: false,
        };
        assert!(policy.should_deliver(true));
    }

    #[test]
    fn platform_close_reason_mappers_preserve_passive_expiry() {
        assert_eq!(
            native_callback_kind_from_mac_response(None),
            NativeCallbackKind::Activated
        );
        assert_eq!(
            native_callback_kind_from_mac_response(Some(MacPassiveCloseReason::Dismissed)),
            NativeCallbackKind::Closed
        );
        assert_eq!(
            native_callback_kind_from_mac_response(Some(MacPassiveCloseReason::Expired)),
            NativeCallbackKind::Expired
        );

        assert_eq!(
            native_callback_kind_from_xdg_close_reason(1),
            NativeCallbackKind::Expired
        );
        assert_eq!(
            native_callback_kind_from_xdg_close_reason(2),
            NativeCallbackKind::Closed
        );
        assert_eq!(
            native_callback_kind_from_xdg_close_reason(3),
            NativeCallbackKind::Expired
        );
        assert_eq!(
            native_callback_kind_from_xdg_close_reason(4),
            NativeCallbackKind::Expired
        );

        assert_eq!(
            native_callback_kind_from_windows_dismissal_reason(0),
            NativeCallbackKind::Closed
        );
        assert_eq!(
            native_callback_kind_from_windows_dismissal_reason(1),
            NativeCallbackKind::Expired
        );
        assert_eq!(
            native_callback_kind_from_windows_dismissal_reason(2),
            NativeCallbackKind::Expired
        );
        assert_eq!(
            native_callback_kind_from_windows_dismissal_reason(i32::MAX),
            NativeCallbackKind::Expired
        );
    }

    #[test]
    fn dispatcher_spawn_failure_installs_a_disabled_disconnected_service() {
        let (action_sender, _action_receiver) = async_channel::unbounded();
        let service = DesktopNotificationService::new_with_backend_and_spawner(
            FakeBackend::default(),
            action_sender,
            None,
            |_runner| Err(io::Error::other("injected dispatcher spawn failure")),
        );

        assert!(!service.running.load(Ordering::Acquire));
        assert!(service.dispatcher.is_none());
        assert!(!service.enqueue_delivery(job(1, target(1))));
        assert!(!service.enqueue_control(RetractionSelector::All));

        let permit = reserve_delivery_slot(&service.delivery_slots).unwrap();
        assert!(matches!(
            service.delivery_sender.try_send(QueuedDelivery {
                order: 1,
                job: job(2, target(2)),
                permit,
            }),
            Err(TrySendError::Disconnected(_))
        ));
        assert!(matches!(
            service.control_sender.try_send(DispatcherControl::Shutdown),
            Err(TrySendError::Disconnected(_))
        ));
    }

    #[test]
    fn rejected_same_id_named_update_retracts_the_stale_native_token() {
        let backend = FakeBackend {
            physical_retraction: true,
            ..FakeBackend::default()
        };
        let operations = backend.operations.clone();
        let (action_sender, _action_receiver) = async_channel::unbounded();
        let service = DesktopNotificationService::new_with_backend_and_spawner(
            backend,
            action_sender,
            None,
            |runner| {
                thread::Builder::new()
                    .name("zmux-named-rejection-test".to_owned())
                    .spawn(runner)
            },
        );
        let target = target(25);
        let id = 250;
        assert!(service.enqueue_delivery(named_job(id, target, "build")));
        let deadline = Instant::now() + Duration::from_secs(1);
        while operations.lock().unwrap().is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }

        // Force the exact bounded-queue rejection returned to runtime. A named
        // canonical update reuses `id`, so runtime's `!native_alive` cleanup
        // must explicitly retract that ID even though the store removed no row.
        service
            .delivery_slots
            .store(DELIVERY_QUEUE_CAPACITY, Ordering::Release);
        let mut update = named_job(id, target, "build");
        update.sequence += 1;
        assert!(!service.enqueue_delivery(update));
        service.delivery_slots.store(0, Ordering::Release);
        assert!(service.enqueue_control(RetractionSelector::Notification(id)));

        let deadline = Instant::now() + Duration::from_secs(1);
        while operations.lock().unwrap().len() < 2 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            *operations.lock().unwrap(),
            [
                FakeOperation::Deliver {
                    id,
                    replacement: None,
                    token: 1,
                },
                FakeOperation::Retract(1),
            ]
        );
        drop(service);
    }

    #[test]
    fn service_drop_detaches_after_a_strict_bound_when_platform_delivery_blocks() {
        let (entered_sender, entered_receiver) = sync_channel(1);
        let (release_sender, release_receiver) = sync_channel(1);
        let (retracted_sender, retracted_receiver) = sync_channel(1);
        let (dropped_sender, dropped_receiver) = sync_channel(1);
        let backend = BlockingBackend {
            entered_delivery: entered_sender,
            release_delivery: release_receiver,
            retracted: retracted_sender,
            dropped: dropped_sender,
        };
        let (action_sender, _action_receiver) = async_channel::unbounded();
        let service = DesktopNotificationService::new_with_backend_and_spawner(
            backend,
            action_sender,
            None,
            |runner| {
                thread::Builder::new()
                    .name("zmux-blocking-notification-test".to_owned())
                    .spawn(runner)
            },
        );

        assert!(service.enqueue_delivery(job(3, target(3))));
        entered_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("the fake platform call should start");

        let started = Instant::now();
        drop(service);
        let elapsed = started.elapsed();
        assert!(
            elapsed <= DISPATCHER_SHUTDOWN_TIMEOUT + Duration::from_millis(250),
            "service Drop exceeded its shutdown bound: {elapsed:?}"
        );

        release_sender
            .send(())
            .expect("the detached dispatcher still owns the fake backend");
        retracted_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("the dispatcher should retract after the platform call returns");
        dropped_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("the detached dispatcher should then exit and drop its backend");
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn xdg_successful_show_is_closed_and_rejected_when_listener_setup_fails() {
        let mut backend = NotifyRustBackend::new();
        let (callback_sender, callback_receiver) = mpsc::channel();
        let listener_status = backend.ensure_xdg_listener_with(&callback_sender, |_| {
            Err("injected XDG listener setup failure".to_owned())
        });
        let shown = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let show_observer = shown.clone();
        let close_observer = closed.clone();
        let delivery = backend.show_and_track_xdg(
            listener_status,
            move || {
                show_observer.store(true, Ordering::Release);
                Ok(FakeXdgHandle {
                    native_id: 42,
                    closed: close_observer,
                })
            },
            &job(4, target(4)),
            1,
            None,
        );

        assert!(shown.load(Ordering::Acquire));
        assert!(closed.load(Ordering::Acquire));
        assert!(delivery.unwrap_err().contains("callbacks are unavailable"));
        assert!(callback_receiver.try_recv().is_err());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn xdg_close_emitted_before_show_returns_is_delivered_after_registration() {
        let mut backend = NotifyRustBackend::new();
        let (callback_sender, callback_receiver) = mpsc::channel();
        backend.xdg_listener = Some(XdgSignalListener::for_test(callback_sender.clone()));
        let listener_state = backend.xdg_listener.as_ref().unwrap().state.clone();
        let signal_sender = callback_sender.clone();
        let closed = Arc::new(AtomicBool::new(false));
        let close_observer = closed.clone();
        let target = target(5);
        let delivery = backend.show_and_track_xdg(
            Ok(()),
            move || {
                // The daemon races Notification::show(): its close signal is
                // observed before show returns the native ID to the backend.
                dispatch_xdg_signal(
                    &listener_state,
                    &signal_sender,
                    77,
                    NativeCallbackKind::Closed,
                );
                Ok(FakeXdgHandle {
                    native_id: 77,
                    closed: close_observer,
                })
            },
            &job_with_sequence(5, 50, target),
            9,
            None,
        );

        assert!(matches!(delivery, Ok(NotifyRustToken::Xdg(77))));
        assert!(!closed.load(Ordering::Acquire));
        assert_eq!(
            callback_receiver
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            NativeCallback {
                key: NativeNotificationKey::Target(target),
                id: 5,
                sequence: 50,
                generation: 9,
                kind: NativeCallbackKind::Closed,
            }
        );
        let state = backend.xdg_listener.as_ref().unwrap().state.lock().unwrap();
        assert!(!state.show_in_flight);
        assert!(state.early_signals.is_empty());
        assert!(state.callbacks.is_empty());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn xdg_early_signal_handshake_is_strictly_bounded() {
        let (callback_sender, callback_receiver) = mpsc::channel();
        let listener = XdgSignalListener::for_test(callback_sender.clone());
        listener.begin_show(None);
        for native_id in 0..(XDG_EARLY_SIGNAL_CAPACITY as u32 * 4) {
            dispatch_xdg_signal(
                &listener.state,
                &callback_sender,
                native_id,
                NativeCallbackKind::Closed,
            );
        }

        let state = listener.state.lock().unwrap();
        assert_eq!(state.early_signals.len(), XDG_EARLY_SIGNAL_CAPACITY);
        assert_eq!(state.early_signal_order.len(), XDG_EARLY_SIGNAL_CAPACITY);
        drop(state);
        listener.cancel_show();
        assert!(callback_receiver.try_recv().is_err());
    }

    #[test]
    fn superseding_a_target_reuses_the_platform_token_and_filters_stale_callbacks() {
        let backend = FakeBackend::default();
        let operations = backend.operations.clone();
        let (mut dispatcher, actions, _callbacks) = dispatcher(backend);
        let target = target(1);
        let key = NativeNotificationKey::Target(target);

        dispatcher.deliver(job(10, target));
        dispatcher.deliver(job(11, target));
        dispatcher.handle_callback(NativeCallback {
            key: key.clone(),
            id: 10,
            sequence: 10,
            generation: 1,
            kind: NativeCallbackKind::Activated,
        });

        assert!(actions.try_recv().is_err());
        assert_eq!(dispatcher.active.get(&key).unwrap().id, 11);
        assert_eq!(
            *operations.lock().unwrap(),
            [
                FakeOperation::Deliver {
                    id: 10,
                    replacement: None,
                    token: 1,
                },
                FakeOperation::Deliver {
                    id: 11,
                    replacement: Some(1),
                    token: 2,
                },
            ]
        );
    }

    #[test]
    fn current_os_dismissal_is_forwarded_once_and_stops_tracking() {
        let (mut dispatcher, actions, _callbacks) = dispatcher(FakeBackend::default());
        let target = target(2);
        let key = NativeNotificationKey::Target(target);
        dispatcher.deliver(job(20, target));

        dispatcher.handle_callback(NativeCallback {
            key: key.clone(),
            id: 20,
            sequence: 20,
            generation: 1,
            kind: NativeCallbackKind::Closed,
        });
        dispatcher.handle_callback(NativeCallback {
            key: key.clone(),
            id: 20,
            sequence: 20,
            generation: 1,
            kind: NativeCallbackKind::Closed,
        });

        assert_eq!(
            actions.try_recv().unwrap(),
            DesktopNotificationAction::Closed {
                id: 20,
                sequence: 20,
            }
        );
        assert!(actions.try_recv().is_err());
        assert!(!dispatcher.active.contains_key(&key));
    }

    #[test]
    fn current_os_expiry_is_forwarded_without_becoming_a_user_close() {
        let (mut dispatcher, actions, _callbacks) = dispatcher(FakeBackend::default());
        let target = target(20);
        let key = NativeNotificationKey::Target(target);
        dispatcher.deliver(job_with_sequence(200, 2_000, target));

        dispatcher.handle_callback(NativeCallback {
            key: key.clone(),
            id: 200,
            sequence: 2_000,
            generation: 1,
            kind: NativeCallbackKind::Expired,
        });

        assert_eq!(
            actions.try_recv().unwrap(),
            DesktopNotificationAction::Expired {
                id: 200,
                sequence: 2_000,
            }
        );
        assert!(actions.try_recv().is_err());
        assert!(!dispatcher.active.contains_key(&key));
    }

    #[test]
    fn same_id_old_callback_action_retains_its_original_sequence() {
        let (mut dispatcher, actions, _callbacks) = dispatcher(FakeBackend::default());
        let target = target(21);
        let key = NativeNotificationKey::KittyNamed {
            target,
            identifier: "build".to_owned(),
        };
        let mut original = named_job(210, target, "build");
        original.sequence = 2_100;
        dispatcher.deliver(original);
        dispatcher.handle_callback(NativeCallback {
            key: key.clone(),
            id: 210,
            sequence: 2_100,
            generation: 1,
            kind: NativeCallbackKind::Activated,
        });

        let mut update = named_job(210, target, "build");
        update.sequence = 2_101;
        dispatcher.deliver(update);

        assert_eq!(
            actions.try_recv().unwrap(),
            DesktopNotificationAction::Activated {
                id: 210,
                sequence: 2_100,
            }
        );
        assert_eq!(dispatcher.active.get(&key).unwrap().sequence, 2_101);
    }

    #[test]
    fn backend_delivery_failure_emits_unavailable_for_the_exact_sequence() {
        let backend = FakeBackend {
            fail_delivery: true,
            ..FakeBackend::default()
        };
        let (mut dispatcher, actions, _callbacks) = dispatcher(backend);
        dispatcher.deliver(job_with_sequence(220, 2_200, target(22)));

        assert_eq!(
            actions.try_recv().unwrap(),
            DesktopNotificationAction::Unavailable {
                id: 220,
                sequence: 2_200,
            }
        );
        assert!(actions.try_recv().is_err());
        assert!(dispatcher.active.is_empty());
    }

    #[test]
    fn explicit_retraction_invalidates_callback_before_calling_the_backend() {
        let backend = FakeBackend {
            physical_retraction: true,
            ..FakeBackend::default()
        };
        let operations = backend.operations.clone();
        let (mut dispatcher, actions, _callbacks) = dispatcher(backend);
        let target = target(3);
        let key = NativeNotificationKey::Target(target);
        dispatcher.deliver(job(30, target));

        dispatcher.retract(RetractionSelector::Notification(30));
        dispatcher.handle_callback(NativeCallback {
            key: key.clone(),
            id: 30,
            sequence: 30,
            generation: 1,
            kind: NativeCallbackKind::Closed,
        });

        assert!(actions.try_recv().is_err());
        assert!(!dispatcher.active.contains_key(&key));
        assert_eq!(
            *operations.lock().unwrap(),
            [
                FakeOperation::Deliver {
                    id: 30,
                    replacement: None,
                    token: 1,
                },
                FakeOperation::Retract(1),
            ]
        );
    }

    #[test]
    fn workspace_and_scope_retractions_are_exact() {
        let backend = FakeBackend {
            physical_retraction: true,
            ..FakeBackend::default()
        };
        let (mut dispatcher, _actions, _callbacks) = dispatcher(backend);
        let first = target(4);
        let mut second = target(5);
        second.scope_id = first.scope_id;
        second.workspace_id = first.workspace_id;
        let third = target(6);
        dispatcher.deliver(job(40, first));
        dispatcher.deliver(job(41, second));
        dispatcher.deliver(job(42, third));

        dispatcher.retract(RetractionSelector::Workspace {
            scope_id: first.scope_id,
            workspace_id: first.workspace_id,
        });
        assert!(
            !dispatcher
                .active
                .contains_key(&NativeNotificationKey::Target(first))
        );
        assert!(
            !dispatcher
                .active
                .contains_key(&NativeNotificationKey::Target(second))
        );
        assert!(
            dispatcher
                .active
                .contains_key(&NativeNotificationKey::Target(third))
        );

        dispatcher.retract(RetractionSelector::Scope(third.scope_id));
        assert!(dispatcher.active.is_empty());
    }

    #[test]
    fn kitty_named_identity_replaces_only_the_same_identifier() {
        let backend = FakeBackend::default();
        let operations = backend.operations.clone();
        let (mut dispatcher, actions, _callbacks) = dispatcher(backend);
        let target = target(7);
        let key_a = NativeNotificationKey::KittyNamed {
            target,
            identifier: "a".to_owned(),
        };
        let key_b = NativeNotificationKey::KittyNamed {
            target,
            identifier: "b".to_owned(),
        };

        dispatcher.deliver(named_job(70, target, "a"));
        dispatcher.deliver(named_job(71, target, "b"));
        dispatcher.deliver(named_job(70, target, "a"));
        dispatcher.handle_callback(NativeCallback {
            key: key_a.clone(),
            id: 70,
            sequence: 70,
            generation: 1,
            kind: NativeCallbackKind::Closed,
        });

        assert!(actions.try_recv().is_err());
        assert_eq!(dispatcher.active.len(), 2);
        assert_eq!(dispatcher.active.get(&key_a).unwrap().generation, 3);
        assert_eq!(dispatcher.active.get(&key_a).unwrap().token, 3);
        assert_eq!(dispatcher.active.get(&key_b).unwrap().token, 2);
        assert_eq!(
            *operations.lock().unwrap(),
            [
                FakeOperation::Deliver {
                    id: 70,
                    replacement: None,
                    token: 1,
                },
                FakeOperation::Deliver {
                    id: 71,
                    replacement: None,
                    token: 2,
                },
                FakeOperation::Deliver {
                    id: 70,
                    replacement: Some(1),
                    token: 3,
                },
            ]
        );
    }

    #[test]
    fn unique_identity_never_replaces_and_target_identity_clears_the_projection() {
        let backend = FakeBackend {
            physical_retraction: true,
            ..FakeBackend::default()
        };
        let operations = backend.operations.clone();
        let (mut dispatcher, _, _callbacks) = dispatcher(backend);
        let target = target(8);

        dispatcher.deliver(unique_job(80, target));
        dispatcher.deliver(unique_job(81, target));
        assert_eq!(dispatcher.active.len(), 2);

        dispatcher.deliver(job(82, target));
        assert_eq!(dispatcher.active.len(), 1);
        assert!(
            dispatcher
                .active
                .contains_key(&NativeNotificationKey::Target(target))
        );

        let operations = operations.lock().unwrap();
        assert!(operations.contains(&FakeOperation::Retract(1)));
        assert!(operations.contains(&FakeOperation::Retract(2)));
        assert!(operations.contains(&FakeOperation::Deliver {
            id: 82,
            replacement: None,
            token: 3,
        }));
    }

    #[test]
    fn active_tracking_is_bounded_without_skipping_new_delivery() {
        let backend = FakeBackend {
            physical_retraction: true,
            ..FakeBackend::default()
        };
        let operations = backend.operations.clone();
        let (mut dispatcher, actions, _callbacks) = dispatcher(backend);

        for index in 0..=MAX_TRACKED_NATIVE_NOTIFICATIONS {
            let id = 1_000 + index as u64;
            dispatcher.deliver(unique_job(id, target(100 + index as u64)));
        }

        assert_eq!(dispatcher.active.len(), MAX_TRACKED_NATIVE_NOTIFICATIONS);
        let operations = operations.lock().unwrap();
        assert_eq!(
            operations
                .iter()
                .filter(|operation| matches!(operation, FakeOperation::Deliver { .. }))
                .count(),
            MAX_TRACKED_NATIVE_NOTIFICATIONS + 1
        );
        assert!(operations.contains(&FakeOperation::Retract(1)));
        assert_eq!(
            actions.try_recv().unwrap(),
            DesktopNotificationAction::Unavailable {
                id: 1_000,
                sequence: 1_000,
            }
        );
        assert!(actions.try_recv().is_err());
    }

    #[test]
    fn platform_callbacks_are_not_dropped_when_the_public_queue_would_be_full() {
        let (callback_sender, callback_receiver) = mpsc::channel();
        let callback_count = DELIVERY_QUEUE_CAPACITY * 4;
        for index in 0..callback_count {
            send_native_callback(
                &callback_sender,
                NativeCallback {
                    key: NativeNotificationKey::Unique {
                        target: target(300),
                        id: index as u64,
                    },
                    id: index as u64,
                    sequence: 10_000 + index as u64,
                    generation: index as u64,
                    kind: NativeCallbackKind::Closed,
                },
            );
        }

        for index in 0..callback_count {
            let callback = callback_receiver.try_recv().unwrap();
            assert_eq!(callback.id, index as u64);
            assert_eq!(callback.sequence, 10_000 + index as u64);
        }
        assert!(callback_receiver.try_recv().is_err());
    }

    #[test]
    fn queued_replacement_preserves_old_token_across_later_old_id_retraction() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let (started_sender, started_receiver) = sync_channel(1);
        let (release_sender, release_receiver) = sync_channel(1);
        let backend = GatedFirstDeliveryBackend {
            next_token: 0,
            operations: operations.clone(),
            first_delivery_started: started_sender,
            release_first_delivery: release_receiver,
            first_delivery_gated: false,
        };
        let delivery_slots = Arc::new(AtomicUsize::new(0));
        let (delivery_sender, delivery_receiver) = sync_channel(DELIVERY_QUEUE_CAPACITY);
        let (control_sender, control_receiver) = sync_channel(CONTROL_QUEUE_CAPACITY);
        let coalesced_retract_all_order = Arc::new(AtomicU64::new(0));
        let target = target(350);
        let old_id = 3_500;
        let new_id = 3_501;

        assert!(
            delivery_sender
                .try_send(QueuedDelivery {
                    order: 1,
                    job: job(old_id, target),
                    permit: reserve_delivery_slot(&delivery_slots).unwrap(),
                })
                .is_ok()
        );
        let (callback_sender, callback_receiver) = mpsc::channel();
        let (action_sender, _action_receiver) = async_channel::unbounded();
        let running = Arc::new(AtomicBool::new(true));
        let dispatcher_running = running.clone();
        let dispatcher_coalesced_retract_all_order = coalesced_retract_all_order.clone();
        let dispatcher = thread::spawn(move || {
            run_dispatcher(
                backend,
                DispatcherChannels {
                    delivery_receiver,
                    control_receiver,
                    coalesced_retract_all_order: dispatcher_coalesced_retract_all_order,
                    callback_receiver,
                    callback_sender,
                    action_sender,
                },
                dispatcher_running,
            );
        });
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("old delivery should reach the backend");

        // Both messages are queued while the first backend call is held. The
        // higher-priority control is therefore guaranteed to run before the
        // replacement delivery after the old token becomes active.
        assert!(
            delivery_sender
                .try_send(QueuedDelivery {
                    order: 2,
                    job: job(new_id, target),
                    permit: reserve_delivery_slot(&delivery_slots).unwrap(),
                })
                .is_ok()
        );
        assert!(
            control_sender
                .try_send(DispatcherControl::Retract {
                    order: 3,
                    selector: RetractionSelector::Notification(old_id),
                })
                .is_ok()
        );
        release_sender.send(()).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while delivery_slots.load(Ordering::Acquire) != 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(delivery_slots.load(Ordering::Acquire), 0);
        assert_eq!(
            *operations.lock().unwrap(),
            [
                FakeOperation::Deliver {
                    id: old_id,
                    replacement: None,
                    token: 1,
                },
                FakeOperation::Deliver {
                    id: new_id,
                    replacement: Some(1),
                    token: 2,
                },
            ],
            "old-id reconciliation must not retract the reusable token first"
        );

        running.store(false, Ordering::Release);
        let _ = control_sender.try_send(DispatcherControl::Shutdown);
        dispatcher.thread().unpark();
        dispatcher.join().unwrap();
        assert_eq!(
            operations.lock().unwrap().last(),
            Some(&FakeOperation::Retract(2)),
            "shutdown retracts the replacement token"
        );
    }

    #[test]
    fn priority_retraction_bypasses_a_full_delivery_queue_and_preserves_newer_work() {
        let backend = FakeBackend {
            physical_retraction: true,
            ..FakeBackend::default()
        };
        let operations = backend.operations.clone();
        let delivery_slots = Arc::new(AtomicUsize::new(0));
        let (delivery_sender, delivery_receiver) = sync_channel(DELIVERY_QUEUE_CAPACITY);
        let (control_sender, control_receiver) = sync_channel(CONTROL_QUEUE_CAPACITY);
        let coalesced_retract_all_order = Arc::new(AtomicU64::new(0));

        // Fill every bounded delivery permit. The first 63 submissions precede
        // the retraction; the final job's order proves that work submitted
        // afterward remains eligible even when both channels are preloaded.
        for index in 0..(DELIVERY_QUEUE_CAPACITY - 1) {
            let permit = reserve_delivery_slot(&delivery_slots).unwrap();
            assert!(
                delivery_sender
                    .try_send(QueuedDelivery {
                        order: index as u64 + 1,
                        job: unique_job(3_000 + index as u64, target(400 + index as u64)),
                        permit,
                    })
                    .is_ok()
            );
        }
        let retraction_order = DELIVERY_QUEUE_CAPACITY as u64;
        let future_id = 9_999;
        let permit = reserve_delivery_slot(&delivery_slots).unwrap();
        assert!(
            delivery_sender
                .try_send(QueuedDelivery {
                    order: retraction_order + 1,
                    job: unique_job(future_id, target(999)),
                    permit,
                })
                .is_ok()
        );
        assert!(reserve_delivery_slot(&delivery_slots).is_none());
        assert!(
            enqueue_retraction_control(
                &control_sender,
                &coalesced_retract_all_order,
                retraction_order,
                RetractionSelector::All,
            ),
            "the reliable control path must accept retraction while delivery is saturated"
        );

        let (callback_sender, callback_receiver) = mpsc::channel();
        let (action_sender, action_receiver) = async_channel::unbounded();
        let running = Arc::new(AtomicBool::new(true));
        let dispatcher_running = running.clone();
        let dispatcher_coalesced_retract_all_order = coalesced_retract_all_order.clone();
        let dispatcher = thread::spawn(move || {
            run_dispatcher(
                backend,
                DispatcherChannels {
                    delivery_receiver,
                    control_receiver,
                    coalesced_retract_all_order: dispatcher_coalesced_retract_all_order,
                    callback_receiver,
                    callback_sender,
                    action_sender,
                },
                dispatcher_running,
            );
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        while delivery_slots.load(Ordering::Acquire) != 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(delivery_slots.load(Ordering::Acquire), 0);
        assert_eq!(
            *operations.lock().unwrap(),
            [FakeOperation::Deliver {
                id: future_id,
                replacement: None,
                token: 1,
            }],
            "older queued deliveries must be canceled before reaching the backend"
        );
        assert!(action_receiver.try_recv().is_err());

        running.store(false, Ordering::Release);
        let _ = control_sender.try_send(DispatcherControl::Shutdown);
        dispatcher.thread().unpark();
        dispatcher.join().unwrap();
        assert_eq!(
            *operations.lock().unwrap(),
            [
                FakeOperation::Deliver {
                    id: future_id,
                    replacement: None,
                    token: 1,
                },
                FakeOperation::Retract(1),
            ],
            "shutdown must still retract the one newer delivered banner"
        );
    }

    #[test]
    fn control_flood_coalesces_to_one_bounded_ordered_retraction() {
        let backend = FakeBackend {
            physical_retraction: true,
            ..FakeBackend::default()
        };
        let operations = backend.operations.clone();
        let delivery_slots = Arc::new(AtomicUsize::new(0));
        let (delivery_sender, delivery_receiver) = sync_channel(DELIVERY_QUEUE_CAPACITY);
        let (control_sender, control_receiver) = sync_channel(CONTROL_QUEUE_CAPACITY);
        let coalesced_retract_all_order = Arc::new(AtomicU64::new(0));

        let old_id = 44_444;
        let old_permit = reserve_delivery_slot(&delivery_slots).unwrap();
        assert!(
            delivery_sender
                .try_send(QueuedDelivery {
                    order: 1,
                    job: unique_job(old_id, target(700)),
                    permit: old_permit,
                })
                .is_ok()
        );

        // The first 64 selectors occupy the fixed control queue. Thousands of
        // distinct later selectors consume no additional queue nodes: they
        // atomically advance one conservative Retract(All) watermark.
        let flood_end = 10_000;
        for order in 2..=flood_end {
            assert!(enqueue_retraction_control(
                &control_sender,
                &coalesced_retract_all_order,
                order,
                RetractionSelector::Notification(u64::MAX - order),
            ));
        }
        assert!(
            control_sender
                .try_send(DispatcherControl::Shutdown)
                .is_err()
        );
        assert_eq!(
            coalesced_retract_all_order.load(Ordering::Acquire),
            flood_end
        );

        // Although physically preloaded for a deterministic test, this job's
        // submission order is after the entire flood and must not be canceled
        // by the overload watermark.
        let future_id = 55_555;
        let future_permit = reserve_delivery_slot(&delivery_slots).unwrap();
        assert!(
            delivery_sender
                .try_send(QueuedDelivery {
                    order: flood_end + 1,
                    job: unique_job(future_id, target(701)),
                    permit: future_permit,
                })
                .is_ok()
        );

        let (callback_sender, callback_receiver) = mpsc::channel();
        let (action_sender, action_receiver) = async_channel::unbounded();
        let running = Arc::new(AtomicBool::new(true));
        let dispatcher_running = running.clone();
        let dispatcher_coalesced_retract_all_order = coalesced_retract_all_order.clone();
        let dispatcher = thread::spawn(move || {
            run_dispatcher(
                backend,
                DispatcherChannels {
                    delivery_receiver,
                    control_receiver,
                    coalesced_retract_all_order: dispatcher_coalesced_retract_all_order,
                    callback_receiver,
                    callback_sender,
                    action_sender,
                },
                dispatcher_running,
            );
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        while delivery_slots.load(Ordering::Acquire) != 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(delivery_slots.load(Ordering::Acquire), 0);
        assert_eq!(
            *operations.lock().unwrap(),
            [FakeOperation::Deliver {
                id: future_id,
                replacement: None,
                token: 1,
            }],
            "the coalesced watermark cancels older work without killing newer delivery"
        );
        assert_eq!(
            action_receiver.try_recv().unwrap(),
            DesktopNotificationAction::Unavailable {
                id: old_id,
                sequence: old_id,
            },
            "collateral overload cancellation must reconcile native liveness"
        );
        assert!(action_receiver.try_recv().is_err());

        running.store(false, Ordering::Release);
        let _ = control_sender.try_send(DispatcherControl::Shutdown);
        dispatcher.thread().unpark();
        dispatcher.join().unwrap();
        assert_eq!(
            *operations.lock().unwrap(),
            [
                FakeOperation::Deliver {
                    id: future_id,
                    replacement: None,
                    token: 1,
                },
                FakeOperation::Retract(1),
            ]
        );
    }

    #[test]
    fn xdg_body_markup_is_escaped_as_plain_text() {
        assert_eq!(
            escape_xdg_body_markup("<b>unsafe & text</b> > done"),
            "&lt;b&gt;unsafe &amp; text&lt;/b&gt; &gt; done"
        );
        assert_eq!(escape_xdg_body_markup("normal\ntext"), "normal\ntext");
    }

    #[test]
    fn xdg_activation_actions_include_the_reserved_default_action_first() {
        assert_eq!(
            XDG_ACTIVATION_ACTIONS,
            [("default", "Open in zmux"), ("open", "Open in zmux")]
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn xdg_signal_listener_shutdown_joins_its_owned_thread() {
        let (callback_sender, _callback_receiver) = mpsc::channel();
        let Ok(listener) = XdgSignalListener::new(callback_sender) else {
            // Most headless test environments do not expose a session bus.
            return;
        };
        drop(listener);
    }

    #[test]
    fn windows_toast_identity_is_stable_and_reuses_replacement_tokens() {
        let target = target(200);
        let alpha = NativeNotificationKey::KittyNamed {
            target,
            identifier: "alpha".to_owned(),
        };
        let beta = NativeNotificationKey::KittyNamed {
            target,
            identifier: "beta".to_owned(),
        };
        let alpha_identity = windows_toast_identity(&alpha, None);

        assert_eq!(alpha_identity, windows_toast_identity(&alpha, None));
        assert_ne!(alpha_identity, windows_toast_identity(&beta, None));
        assert_eq!(alpha_identity.tag.len(), 16);
        assert!(alpha_identity.group.len() <= 16);

        // A platform-issued replacement pair is authoritative even if the
        // derivation algorithm changes in a future release.
        assert_eq!(
            windows_toast_identity(&beta, Some(&alpha_identity)),
            alpha_identity
        );
    }

    #[test]
    fn windows_unique_toast_tags_coexist_and_xml_text_is_escaped() {
        let target = target(201);
        let first = NativeNotificationKey::Unique { target, id: 1 };
        let second = NativeNotificationKey::Unique { target, id: 2 };
        assert_ne!(
            windows_toast_identity(&first, None),
            windows_toast_identity(&second, None)
        );

        let mut notification = job(2, target);
        notification.title = "<build & test>".to_owned();
        notification.subtitle = "'quoted'".to_owned();
        notification.body = "done > pending".to_owned();
        let xml = windows_toast_xml(&notification);
        assert!(xml.contains("&lt;build &amp; test&gt;"));
        assert!(xml.contains("done &gt; pending"));
        assert!(!xml.contains("<build & test>"));
    }
}
