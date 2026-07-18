#[cfg(target_os = "macos")]
pub(crate) fn configure_native_app_icon() {
    use objc2::{AnyThread as _, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    const ICON_BYTES: &[u8] = include_bytes!("../packaging/icons/macos/zmux-1024.png");
    let Some(main_thread) = MainThreadMarker::new() else {
        eprintln!("failed to set the macOS application icon off the main thread");
        return;
    };
    let data =
        unsafe { NSData::dataWithBytes_length(ICON_BYTES.as_ptr().cast(), ICON_BYTES.len()) };
    let Some(icon) = NSImage::initWithData(NSImage::alloc(), &data) else {
        eprintln!("failed to decode the bundled macOS application icon");
        return;
    };

    unsafe {
        NSApplication::sharedApplication(main_thread).setApplicationIconImage(Some(&icon));
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn configure_native_app_icon() {}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub(crate) fn linux_window_icon() -> Option<std::sync::Arc<image::RgbaImage>> {
    use std::io::Cursor;
    use std::sync::{Arc, LazyLock};

    static APP_ICON: LazyLock<Option<Arc<image::RgbaImage>>> = LazyLock::new(|| {
        const ICON_BYTES: &[u8] = include_bytes!(
            "../packaging/icons/linux/hicolor/256x256/apps/io.github.thinkter.zmux.png"
        );
        let icon = image::ImageReader::new(Cursor::new(ICON_BYTES))
            .with_guessed_format()
            .ok()?
            .decode()
            .ok()?
            .into_rgba8();
        Some(Arc::new(icon))
    });

    APP_ICON.clone()
}
