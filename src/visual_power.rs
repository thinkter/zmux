//! Event-driven window visibility, activation, and Low Power Mode projection.

use gpui::{App, Global, Window};

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::HashMap;
    use std::ptr::NonNull;
    use std::sync::Once;

    use block2::RcBlock;
    use gpui::UpdateGlobal as _;
    use objc2::runtime::{AnyObject, ProtocolObject};
    use objc2_app_kit::{
        NSView, NSWindow, NSWindowDidChangeOcclusionStateNotification, NSWindowOcclusionState,
        NSWindowWillCloseNotification,
    };
    use objc2_foundation::{
        NSNotification, NSNotificationCenter, NSObjectProtocol, NSOperationQueue, NSProcessInfo,
        NSProcessInfoPowerStateDidChangeNotification,
    };
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    use super::*;

    static ATTACH_FAILURE_LOGGED: Once = Once::new();

    #[cfg(debug_assertions)]
    fn named_gpui_test_app(name: Option<&'static str>) -> bool {
        name.is_some()
    }

    fn replace_registry_entry<K: std::hash::Hash + Eq, V>(
        registry: &mut HashMap<K, V>,
        key: K,
        value: V,
    ) -> Option<V> {
        registry.insert(key, value)
    }

    enum NativeVisualPowerEvent {
        Window(u64),
        WindowClosed(u64),
        Power,
    }

    struct ObservedWindow {
        window: objc2::rc::Retained<NSWindow>,
        observers: Vec<objc2::rc::Retained<ProtocolObject<dyn NSObjectProtocol>>>,
        visible: bool,
    }

    pub(crate) struct VisualPowerMonitor {
        center: objc2::rc::Retained<NSNotificationCenter>,
        power_observer: objc2::rc::Retained<ProtocolObject<dyn NSObjectProtocol>>,
        windows: HashMap<u64, ObservedWindow>,
        low_power: bool,
        test_mode: bool,
        sender: async_channel::Sender<NativeVisualPowerEvent>,
    }

    impl Global for VisualPowerMonitor {}

    impl Drop for VisualPowerMonitor {
        fn drop(&mut self) {
            unsafe {
                self.center
                    .removeObserver(self.power_observer.as_ref() as &AnyObject);
                for window in self.windows.values() {
                    for observer in &window.observers {
                        self.center.removeObserver(observer.as_ref() as &AnyObject);
                    }
                }
            }
        }
    }

    impl VisualPowerMonitor {
        pub(crate) fn init(cx: &mut App) {
            if cx.has_global::<Self>() {
                return;
            }
            let (sender, receiver) = async_channel::unbounded();
            let center = NSNotificationCenter::defaultCenter();
            let process_info = NSProcessInfo::processInfo();
            let low_power = process_info.isLowPowerModeEnabled();
            #[cfg(debug_assertions)]
            let test_mode = named_gpui_test_app(cx.get_name());
            #[cfg(not(debug_assertions))]
            let test_mode = false;
            let power_sender = sender.clone();
            let power_block = RcBlock::new(move |_notification: NonNull<NSNotification>| {
                let _ = power_sender.try_send(NativeVisualPowerEvent::Power);
            });
            let power_observer = unsafe {
                center.addObserverForName_object_queue_usingBlock(
                    Some(NSProcessInfoPowerStateDidChangeNotification),
                    None,
                    Some(&NSOperationQueue::mainQueue()),
                    &power_block,
                )
            };
            cx.set_global(Self {
                center,
                power_observer,
                windows: HashMap::new(),
                low_power,
                test_mode,
                sender,
            });
            terminal_view::set_visual_power_state(
                terminal_view::TerminalVisualPowerState::new([], [], low_power),
                cx,
            );

            cx.spawn(async move |cx| {
                while let Ok(event) = receiver.recv().await {
                    cx.update(|cx| {
                        let state = Self::update_global(cx, |monitor, _cx| {
                            monitor.refresh(event);
                            monitor.state()
                        });
                        terminal_view::set_visual_power_state(state, cx);
                    });
                }
            })
            .detach();
        }

        pub(crate) fn attach(window: &Window, cx: &mut App) {
            if cx.global::<Self>().test_mode {
                return;
            }
            let id = window.window_handle().window_id().as_u64();
            let native_window =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| native_window(window)))
                    .ok()
                    .flatten();
            let Some(native_window) = native_window else {
                ATTACH_FAILURE_LOGGED.call_once(|| {
                    eprintln!(
                        "macOS visual power monitoring unavailable for this window; using visible fallback"
                    );
                });
                return;
            };
            let sender = cx.global::<Self>().sender.clone();
            let block = RcBlock::new(move |_notification: NonNull<NSNotification>| {
                let _ = sender.try_send(NativeVisualPowerEvent::Window(id));
            });
            let close_sender = cx.global::<Self>().sender.clone();
            let close_block = RcBlock::new(move |_notification: NonNull<NSNotification>| {
                let _ = close_sender.try_send(NativeVisualPowerEvent::WindowClosed(id));
            });
            let center = cx.global::<Self>().center.clone();
            let observer = unsafe {
                center.addObserverForName_object_queue_usingBlock(
                    Some(NSWindowDidChangeOcclusionStateNotification),
                    Some(native_window.as_ref() as &AnyObject),
                    Some(&NSOperationQueue::mainQueue()),
                    &block,
                )
            };
            let close_observer = unsafe {
                center.addObserverForName_object_queue_usingBlock(
                    Some(NSWindowWillCloseNotification),
                    Some(native_window.as_ref() as &AnyObject),
                    Some(&NSOperationQueue::mainQueue()),
                    &close_block,
                )
            };
            let visible = is_visible(&native_window);
            let state = Self::update_global(cx, |monitor, _cx| {
                let previous = replace_registry_entry(
                    &mut monitor.windows,
                    id,
                    ObservedWindow {
                        window: native_window,
                        observers: vec![observer, close_observer],
                        visible,
                    },
                );
                if let Some(previous) = previous {
                    unsafe {
                        for observer in &previous.observers {
                            monitor
                                .center
                                .removeObserver(observer.as_ref() as &AnyObject);
                        }
                    }
                }
                monitor.state()
            });
            terminal_view::set_visual_power_state(state, cx);
        }

        fn refresh(&mut self, event: NativeVisualPowerEvent) {
            match event {
                NativeVisualPowerEvent::Window(id) => {
                    if let Some(window) = self.windows.get_mut(&id) {
                        window.visible = is_visible(&window.window);
                    }
                }
                NativeVisualPowerEvent::WindowClosed(id) => {
                    if let Some(window) = self.windows.remove(&id) {
                        unsafe {
                            for observer in &window.observers {
                                self.center.removeObserver(observer.as_ref() as &AnyObject);
                            }
                        }
                    }
                }
                NativeVisualPowerEvent::Power => {
                    self.low_power = NSProcessInfo::processInfo().isLowPowerModeEnabled();
                }
            }
        }

        fn state(&self) -> terminal_view::TerminalVisualPowerState {
            terminal_view::TerminalVisualPowerState::new(
                self.windows
                    .iter()
                    .filter_map(|(id, window)| (!window.visible).then_some(*id)),
                [],
                self.low_power,
            )
        }
    }

    fn is_visible(window: &NSWindow) -> bool {
        window
            .occlusionState()
            .contains(NSWindowOcclusionState::Visible)
    }

    fn native_window(window: &Window) -> Option<objc2::rc::Retained<NSWindow>> {
        let handle = HasWindowHandle::window_handle(window).ok()?;
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return None;
        };
        let view = unsafe { handle.ns_view.as_ptr().cast::<NSView>().as_ref()? };
        view.window()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn registry_replacement_returns_entry_for_observer_cleanup() {
            let mut registry = HashMap::from([(7_u64, "old")]);
            assert_eq!(replace_registry_entry(&mut registry, 7, "new"), Some("old"));
            assert_eq!(registry, HashMap::from([(7, "new")]));
        }

        #[test]
        fn named_gpui_test_apps_skip_native_platform_handles() {
            assert!(named_gpui_test_app(Some("integration test")));
            assert!(!named_gpui_test_app(None));
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) use macos::VisualPowerMonitor;

#[cfg(not(target_os = "macos"))]
mod non_macos {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use gpui::UpdateGlobal as _;

    use super::*;

    /// Keep the probe infrequent enough to be negligible while still restoring an
    /// uncovered window before a human can notice stale terminal output.
    const VISIBILITY_PROBE_INTERVAL: Duration = Duration::from_millis(250);
    const FRAME_STARVATION_TIMEOUT: Duration = Duration::from_millis(750);

    pub(crate) struct VisualPowerMonitor {
        windows: HashMap<u64, ObservedWindow>,
        test_mode: bool,
        log_transitions: bool,
    }

    struct ObservedWindow {
        active: bool,
        hidden: bool,
        frame_fallback: bool,
        frame_probe: FrameVisibilityProbe,
        last_reported: Option<(bool, bool)>,
        _activation: gpui::Subscription,
    }

    #[derive(Debug)]
    struct FrameVisibilityProbe {
        next_generation: u64,
        pending: Option<(u64, Instant)>,
        starved: bool,
    }

    impl Default for FrameVisibilityProbe {
        fn default() -> Self {
            Self {
                next_generation: 1,
                pending: None,
                starved: false,
            }
        }
    }

    impl FrameVisibilityProbe {
        fn poll(&mut self, now: Instant) -> Option<u64> {
            if let Some((_, requested_at)) = self.pending {
                if now.duration_since(requested_at) >= FRAME_STARVATION_TIMEOUT {
                    self.starved = true;
                }
                return None;
            }

            let generation = self.next_generation;
            self.next_generation = self.next_generation.wrapping_add(1).max(1);
            self.pending = Some((generation, now));
            Some(generation)
        }

        fn presented(&mut self, generation: u64) {
            if self
                .pending
                .is_some_and(|(pending_generation, _)| pending_generation == generation)
            {
                self.pending = None;
                self.starved = false;
            }
        }

        fn restore_visible(&mut self) {
            self.pending = None;
            self.starved = false;
        }
    }

    #[derive(Clone, Copy)]
    enum NativeVisibilityConfig {
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        Xcb {
            window: u32,
        },
        #[cfg(target_os = "windows")]
        Win32 {
            hwnd: isize,
        },
        FrameCallbacks,
    }

    impl NativeVisibilityConfig {
        fn for_window(window: &Window) -> Self {
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            if let Ok(window) = raw_window_handle::HasWindowHandle::window_handle(window)
                && let raw_window_handle::RawWindowHandle::Xcb(window) = window.as_raw()
            {
                return Self::Xcb {
                    window: window.window.get(),
                };
            }

            #[cfg(target_os = "windows")]
            if let Ok(window) = raw_window_handle::HasWindowHandle::window_handle(window)
                && let raw_window_handle::RawWindowHandle::Win32(window) = window.as_raw()
            {
                return Self::Win32 {
                    hwnd: window.hwnd.get(),
                };
            }

            Self::FrameCallbacks
        }

        fn needs_frame_fallback(&self) -> bool {
            matches!(self, Self::FrameCallbacks)
        }
    }

    struct NativeVisibilityWorker {
        config: NativeVisibilityConfig,
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        xcb: Option<xcb::Connection>,
    }

    impl NativeVisibilityWorker {
        fn new(config: NativeVisibilityConfig) -> Self {
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            let xcb = matches!(config, NativeVisibilityConfig::Xcb { .. })
                .then(|| {
                    xcb::Connection::connect(None)
                        .ok()
                        .map(|(connection, _)| connection)
                })
                .flatten();
            Self {
                config,
                #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                xcb,
            }
        }

        fn hidden(&self) -> Option<bool> {
            match self.config {
                #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                NativeVisibilityConfig::Xcb { window } => {
                    use xcb::XidNew as _;

                    let connection = self.xcb.as_ref()?;
                    let cookie = connection.send_request(&xcb::x::GetWindowAttributes {
                        window: xcb::x::Window::new(window),
                    });
                    connection
                        .wait_for_reply(cookie)
                        .ok()
                        .map(|attributes| attributes.map_state() != xcb::x::MapState::Viewable)
                }
                #[cfg(target_os = "windows")]
                NativeVisibilityConfig::Win32 { hwnd } => {
                    use windows::Win32::Foundation::HWND;
                    use windows::Win32::UI::WindowsAndMessaging::IsIconic;

                    Some(unsafe { IsIconic(HWND(hwnd as *mut _)).as_bool() })
                }
                NativeVisibilityConfig::FrameCallbacks => None,
            }
        }
    }

    impl Global for VisualPowerMonitor {}

    impl VisualPowerMonitor {
        pub(crate) fn init(cx: &mut App) {
            if !cx.has_global::<Self>() {
                #[cfg(debug_assertions)]
                let test_mode = cx.get_name().is_some();
                #[cfg(not(debug_assertions))]
                let test_mode = false;
                cx.set_global(Self {
                    windows: HashMap::new(),
                    test_mode,
                    log_transitions: std::env::var_os("ZMUX_LOG_VISUAL_POWER").is_some(),
                });
            }
            terminal_view::set_visual_power_state(Default::default(), cx);
        }

        pub(crate) fn attach<T: 'static>(window: &mut Window, cx: &mut gpui::Context<T>) {
            if cx.global::<Self>().test_mode {
                return;
            }

            let id = window.window_handle().window_id().as_u64();
            let handle = window.window_handle();
            let active = window.is_window_active();
            // GPUI's TestWindow deliberately has no raw platform handle and
            // panics when one is requested. Production backends can also lose
            // a handle during teardown, so keep the same visible/frame-callback
            // fallback boundary as the macOS native attachment path.
            let native_config = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                NativeVisibilityConfig::for_window(window)
            }))
            .unwrap_or(NativeVisibilityConfig::FrameCallbacks);
            let activation = cx.observe_window_activation(window, move |_owner, window, cx| {
                Self::update_window(id, window.is_window_active(), None, cx);
                // Activation changes are also the fastest restoration signal on
                // compositors that do not expose an explicit mapped state.
                Self::poll_window(id, None, window, cx);
            });

            let state = Self::update_global(cx, |monitor, _cx| {
                monitor.windows.insert(
                    id,
                    ObservedWindow {
                        active,
                        hidden: false,
                        frame_fallback: native_config.needs_frame_fallback(),
                        frame_probe: FrameVisibilityProbe::default(),
                        last_reported: None,
                        _activation: activation,
                    },
                );
                monitor.state()
            });
            terminal_view::set_visual_power_state(state, cx);

            let (sample_sender, sample_receiver) = async_channel::bounded(1);
            let background_executor = cx.background_executor().clone();
            background_executor
                .clone()
                .spawn(async move {
                    let worker = NativeVisibilityWorker::new(native_config);
                    loop {
                        background_executor.timer(VISIBILITY_PROBE_INTERVAL).await;
                        if sample_sender.send(worker.hidden()).await.is_err() {
                            break;
                        }
                    }
                })
                .detach();

            cx.spawn(async move |_owner, cx| {
                while let Ok(native_hidden) = sample_receiver.recv().await {
                    if handle
                        .update(cx, |_, window, cx| {
                            Self::poll_window(id, native_hidden, window, cx)
                        })
                        .is_err()
                    {
                        cx.update(|cx| Self::remove_window(id, cx));
                        break;
                    }
                }
            })
            .detach();
        }

        fn poll_window(id: u64, native_hidden: Option<bool>, window: &mut Window, cx: &mut App) {
            let now = Instant::now();
            let (frame_generation, state) = Self::update_global(cx, |monitor, _cx| {
                let Some(observed) = monitor.windows.get_mut(&id) else {
                    return (None, monitor.state());
                };
                if let Some(hidden) = native_hidden {
                    observed.hidden = hidden;
                    observed.frame_fallback = false;
                } else {
                    observed.frame_fallback = true;
                }
                // Wayland has no portable occlusion API. Probe compositor
                // callbacks only after deactivation: continuously forcing
                // frames for the active window would itself waste power.
                let frame_generation = (observed.frame_fallback && !observed.active)
                    .then(|| observed.frame_probe.poll(now))
                    .flatten();
                if observed.frame_fallback {
                    observed.hidden = observed.frame_probe.starved;
                }
                let state = monitor.state();
                monitor.log_window_state(id);
                (frame_generation, state)
            });
            terminal_view::set_visual_power_state(state, cx);

            if let Some(generation) = frame_generation {
                window.on_next_frame(move |_window, cx| {
                    Self::frame_presented(id, generation, cx);
                });
                window.refresh();
            }
        }

        fn frame_presented(id: u64, generation: u64, cx: &mut App) {
            let state = Self::update_global(cx, |monitor, _cx| {
                if let Some(observed) = monitor.windows.get_mut(&id) {
                    observed.frame_probe.presented(generation);
                    if observed.frame_fallback {
                        observed.hidden = observed.frame_probe.starved;
                    }
                }
                monitor.log_window_state(id);
                monitor.state()
            });
            terminal_view::set_visual_power_state(state, cx);
        }

        fn update_window(id: u64, active: bool, hidden: Option<bool>, cx: &mut App) {
            let state = Self::update_global(cx, |monitor, _cx| {
                if let Some(observed) = monitor.windows.get_mut(&id) {
                    observed.active = active;
                    if active && observed.frame_fallback {
                        // Activation is the fastest reliable restore signal on
                        // Wayland, and also cancels a pending starvation probe.
                        observed.frame_probe.restore_visible();
                        observed.hidden = false;
                    }
                    if let Some(hidden) = hidden {
                        observed.hidden = hidden;
                    }
                }
                monitor.log_window_state(id);
                monitor.state()
            });
            terminal_view::set_visual_power_state(state, cx);
        }

        fn remove_window(id: u64, cx: &mut App) {
            let state = Self::update_global(cx, |monitor, _cx| {
                monitor.windows.remove(&id);
                monitor.state()
            });
            terminal_view::set_visual_power_state(state, cx);
        }

        fn state(&self) -> terminal_view::TerminalVisualPowerState {
            terminal_view::TerminalVisualPowerState::new(
                self.windows
                    .iter()
                    .filter_map(|(id, window)| window.hidden.then_some(*id)),
                self.windows
                    .iter()
                    .filter_map(|(id, window)| (!window.active && !window.hidden).then_some(*id)),
                false,
            )
        }

        fn log_window_state(&mut self, id: u64) {
            if self.log_transitions
                && let Some(window) = self.windows.get_mut(&id)
            {
                let state = (window.hidden, !window.active && !window.hidden);
                if window.last_reported == Some(state) {
                    return;
                }
                window.last_reported = Some(state);
                eprintln!(
                    "visual-power window={id} hidden={} throttled={}",
                    state.0, state.1
                );
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn frame_starvation_tracks_hidden_visible_hidden_transitions() {
            let start = Instant::now();
            let mut probe = FrameVisibilityProbe::default();

            let first = probe.poll(start).expect("first frame probe");
            assert!(!probe.starved);
            assert_eq!(
                probe.poll(start + FRAME_STARVATION_TIMEOUT),
                None,
                "a pending callback is never duplicated"
            );
            assert!(probe.starved, "callback starvation marks the window hidden");

            probe.presented(first);
            assert!(!probe.starved, "a compositor callback restores visibility");

            let second = probe
                .poll(start + FRAME_STARVATION_TIMEOUT + VISIBILITY_PROBE_INTERVAL)
                .expect("second frame probe");
            assert_ne!(second, first);
            probe.poll(start + FRAME_STARVATION_TIMEOUT * 2 + VISIBILITY_PROBE_INTERVAL);
            assert!(probe.starved, "later starvation hides the window again");

            probe.restore_visible();
            assert!(!probe.starved, "activation restores visibility immediately");
            assert!(
                probe.pending.is_none(),
                "activation cancels the old callback"
            );
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) use non_macos::VisualPowerMonitor;

#[cfg(test)]
mod tests {
    #[test]
    fn terminal_power_state_tracks_each_window_independently() {
        let state = terminal_view::TerminalVisualPowerState::new([2, 4], [3], false);
        assert!(!state.window_hidden(1));
        assert!(state.window_hidden(2));
        assert!(!state.window_hidden(3));
        assert!(state.window_hidden(4));
        assert!(!state.window_throttled(2));
        assert!(state.window_throttled(3));
        assert!(!state.low_power);

        let low_power = terminal_view::TerminalVisualPowerState::new([], [], true);
        assert!(low_power.window_throttled(1));
    }
}
