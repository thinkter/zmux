//! Event-driven macOS window visibility and Low Power Mode projection.

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

    pub(crate) struct MacVisualPowerMonitor {
        center: objc2::rc::Retained<NSNotificationCenter>,
        power_observer: objc2::rc::Retained<ProtocolObject<dyn NSObjectProtocol>>,
        windows: HashMap<u64, ObservedWindow>,
        low_power: bool,
        test_mode: bool,
        sender: async_channel::Sender<NativeVisualPowerEvent>,
    }

    impl Global for MacVisualPowerMonitor {}

    impl Drop for MacVisualPowerMonitor {
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

    impl MacVisualPowerMonitor {
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
                terminal_view::TerminalVisualPowerState::new([], low_power),
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
pub(crate) use macos::MacVisualPowerMonitor;

#[cfg(not(target_os = "macos"))]
pub(crate) struct MacVisualPowerMonitor;

#[cfg(not(target_os = "macos"))]
impl Global for MacVisualPowerMonitor {}

#[cfg(not(target_os = "macos"))]
impl MacVisualPowerMonitor {
    pub(crate) fn init(cx: &mut App) {
        if !cx.has_global::<Self>() {
            cx.set_global(Self);
        }
        terminal_view::set_visual_power_state(Default::default(), cx);
    }

    pub(crate) fn attach(_window: &Window, _cx: &mut App) {}
}

#[cfg(test)]
mod tests {
    #[test]
    fn terminal_power_state_tracks_each_window_independently() {
        let state = terminal_view::TerminalVisualPowerState::new([2, 4], true);
        assert!(!state.window_hidden(1));
        assert!(state.window_hidden(2));
        assert!(!state.window_hidden(3));
        assert!(state.window_hidden(4));
        assert!(state.low_power);
    }
}
