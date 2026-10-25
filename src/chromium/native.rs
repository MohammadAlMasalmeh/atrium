//! The window is ours, not CEF's: a transparent NSWindow with one Liquid Glass
//! surface behind everything, the shell UI in a transparent WKWebView, and each
//! Chromium tab as a child NSView laid over the shell's content area. CEF can't
//! draw see-through windows, so Chromium only ever paints the (opaque) pages.
use super::{Bridge, Event};
use cef::Rect;
use objc2::{
    define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, NSObjectProtocol, ProtocolObject},
    DefinedClass, MainThreadMarker, MainThreadOnly, Message,
};
use objc2_app_kit::{
    NSApplication, NSAutoresizingMaskOptions, NSBackingStoreType, NSColor, NSGlassEffectView,
    NSView, NSWindow, NSWindowButton, NSWindowDelegate, NSWindowOrderingMode, NSWindowStyleMask,
    NSWindowTitleVisibility,
};
use objc2_foundation::{ns_string, NSNotification, NSObject, NSPoint, NSRect, NSSize};
use std::{cell::RefCell, ffi::c_void, ptr::NonNull};
use wry::{
    raw_window_handle::{AppKitWindowHandle, HandleError, HasWindowHandle, RawWindowHandle, WindowHandle},
    WebView, WebViewBuilder, WebViewBuilderExtDarwin, WebViewExtMacOS,
};

/// Traffic lights centred in the shell's 40px top bar.
const LIGHTS: (f64, f64) = (18.0, 13.0);

struct Native {
    window: Retained<NSWindow>,
    content: Retained<NSView>,
    shell: WebView,
    _delegate: Retained<WindowEvents>,
}

thread_local! {
    static NATIVE: RefCell<Option<Native>> = const { RefCell::new(None) };
}

fn with<T>(f: impl FnOnce(&Native) -> T) -> Option<T> {
    NATIVE.with(|n| n.borrow().as_ref().map(f))
}

define_class!(
    /// Top-left origin, matching the shell's layout rects and CEF's bounds.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    pub(super) struct FlippedView;
    impl FlippedView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Bridge]
    struct WindowEvents;
    unsafe impl NSObjectProtocol for WindowEvents {}
    unsafe impl NSWindowDelegate for WindowEvents {
        #[unsafe(method(windowShouldClose:))]
        fn should_close(&self, _sender: &NSWindow) -> bool {
            self.ivars().send(Event::Quit);
            false
        }
        #[unsafe(method(windowDidResize:))]
        fn did_resize(&self, _note: &NSNotification) {
            with(|n| place_lights(&n.window));
            self.ivars().send(Event::Resize);
        }
    }
);

struct Parent(NonNull<c_void>);
impl HasWindowHandle for Parent {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let raw = RawWindowHandle::AppKit(AppKitWindowHandle::new(self.0));
        Ok(unsafe { WindowHandle::borrow_raw(raw) })
    }
}

pub(super) fn create(bridge: Bridge, shell_url: &str) {
    let mtm = MainThreadMarker::new().expect("CEF's UI thread is the main thread on macOS");
    if background() {
        test_appearance(mtm);
    }
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1280.0, 820.0));
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable
        | NSWindowStyleMask::FullSizeContentView;
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(ns_string!("Atrium"));
    window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
    window.setTitlebarAppearsTransparent(true);
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.setMinSize(NSSize::new(760.0, 480.0));

    let content: Retained<FlippedView> = unsafe { msg_send![FlippedView::alloc(mtm), initWithFrame: frame] };
    let content = Retained::into_super(content);
    window.setContentView(Some(&content));

    // One glass surface for the whole window, tinted so text on it stays legible.
    let glass = NSGlassEffectView::new(mtm);
    glass.setFrame(content.bounds());
    glass.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
    glass.setCornerRadius(16.0);
    let tint = NSColor::windowBackgroundColor().colorWithAlphaComponent(0.55);
    glass.setTintColor(Some(&tint));
    content.addSubview_positioned_relativeTo(&glass, NSWindowOrderingMode::Below, None);

    let ipc = bridge.clone();
    let parent = Parent(NonNull::from(&*content).cast());
    let mut shell = WebViewBuilder::new();
    if super::chrome::enabled() {
        shell = shell.with_initialization_script(
            "(function mark() { const root = document.documentElement; \
             if (root) root.classList.add('native-chrome'); \
             else document.addEventListener('readystatechange', mark, { once: true }); })();",
        );
    }
    let shell = shell
        .with_data_store_identifier(store_id())
        .with_url(shell_url)
        .with_transparent(true)
        .with_devtools(true)
        .with_accept_first_mouse(true)
        .with_bounds(wry::Rect {
            position: wry::dpi::LogicalPosition::new(0.0, 0.0).into(),
            size: wry::dpi::LogicalSize::new(frame.size.width, frame.size.height).into(),
        })
        .with_ipc_handler(move |req| shell_message(&ipc, req.into_body()))
        .build_as_child(&parent)
        .expect("shell webview");
    shell.webview().setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);

    if super::chrome::enabled() {
        super::chrome::install(&content, bridge.clone());
        super::sidebar::install(&content, bridge.clone());
    }

    let delegate: Retained<WindowEvents> =
        unsafe { msg_send![super(WindowEvents::alloc(mtm).set_ivars(bridge)), init] };
    window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    place_lights(&window);
    if background() {
        keep_out_of_sight(&window);
    } else {
        window.center();
        window.makeKeyAndOrderFront(None);
    }
    NATIVE.with(|n| *n.borrow_mut() = Some(Native { window, content, shell, _delegate: delegate }));
}

/// `ATRIUM_BACKGROUND_WINDOW=1` is for test instances that must never activate, take focus, or be seen.
pub(super) fn background() -> bool {
    std::env::var_os("ATRIUM_BACKGROUND_WINDOW").is_some_and(|v| v == "1")
}

/// Ordered in, so Chromium and the glass still render, but transparent, click-through,
/// off-screen, and behind other windows.
fn keep_out_of_sight(window: &NSWindow) {
    use objc2_app_kit::NSWindowCollectionBehavior;
    // ATRIUM_SHOW_BEHIND=1 (screenshots only): on screen behind every other window, so the
    // glass has a backdrop, but still unable to activate or take clicks.
    let behind = std::env::var_os("ATRIUM_SHOW_BEHIND").is_some_and(|v| v == "1");
    #[allow(unused_unsafe)]
    unsafe {
        window.setIgnoresMouseEvents(true);
        window.setExcludedFromWindowsMenu(true);
        window.setCollectionBehavior(NSWindowCollectionBehavior::Transient | NSWindowCollectionBehavior::IgnoresCycle);
        if behind {
            window.center();
        } else {
            window.setAlphaValue(0.0);
            window.setHasShadow(false);
            window.setFrameOrigin(NSPoint::new(-30000.0, -30000.0));
        }
        window.orderBack(None);
    }
}

/// `ATRIUM_APPEARANCE=light|dark` pins a test instance's appearance (shell, glass and
/// pages alike) so both modes can be checked without touching the system setting.
fn test_appearance(mtm: MainThreadMarker) {
    use objc2_app_kit::{NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua};
    let name = match std::env::var("ATRIUM_APPEARANCE").as_deref() {
        Ok("dark") => unsafe { NSAppearanceNameDarkAqua },
        Ok("light") => unsafe { NSAppearanceNameAqua },
        _ => return,
    };
    NSApplication::sharedApplication(mtm).setAppearance(NSAppearance::appearanceNamed(name).as_deref());
}

pub(super) fn shown_behind() -> bool {
    background() && std::env::var_os("ATRIUM_SHOW_BEHIND").is_some_and(|v| v == "1")
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGWindowListCreateImage(bounds: NSRect, options: u32, window: u32, image: u32) -> *mut c_void;
    fn CGImageRelease(image: *mut c_void);
}

/// PNG of the whole window, glass included. Apps may read back their own windows
/// without Screen Recording access. Test mode only: the window is off-screen, so it
/// is made opaque just for the capture.
pub(super) fn window_png() -> Result<Vec<u8>, String> {
    use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep};
    if !background() {
        return Err("Window capture is only available with ATRIUM_BACKGROUND_WINDOW=1".into());
    }
    with(|n| {
        let number = n.window.windowNumber() as u32;
        n.window.setAlphaValue(1.0);
        n.window.displayIfNeeded();
        // CGRectNull: the window's own bounds. Options: just this window; ignore framing, best resolution.
        let null = NSRect::new(NSPoint::new(f64::INFINITY, f64::INFINITY), NSSize::new(0.0, 0.0));
        let image = unsafe { CGWindowListCreateImage(null, 1 << 3, number, (1 << 0) | (1 << 3)) };
        if !shown_behind() {
            n.window.setAlphaValue(0.0);
        }
        if image.is_null() {
            return Err("The window server returned no image".to_string());
        }
        let rep: Option<Retained<NSBitmapImageRep>> =
            unsafe { msg_send![<NSBitmapImageRep as objc2::AnyThread>::alloc(), initWithCGImage: image] };
        unsafe { CGImageRelease(image) };
        let rep = rep.ok_or("Could not read the window image")?;
        let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &objc2_foundation::NSDictionary::new()) }
            .ok_or("Could not encode PNG")?;
        Ok(png.to_vec())
    })
    .unwrap_or_else(|| Err("The window is not open".into()))
}

/// Where a tab's Chromium view actually is, for layout checks.
pub(super) fn frame_of(handle: *mut c_void) -> Option<serde_json::Value> {
    let v = view(handle)?;
    let f = v.frame();
    #[allow(unused_unsafe)]
    let (in_window, focused) = unsafe {
        let focused = with(|n| {
            let mut cur = n.window.firstResponder().and_then(|r| {
                let r: &AnyObject = &r;
                r.downcast_ref::<NSView>().map(|v| v.retain())
            });
            while let Some(c) = cur {
                if std::ptr::eq(&*c, v) {
                    return true;
                }
                cur = c.superview();
            }
            false
        });
        (v.superview().is_some(), focused.unwrap_or(false))
    };
    Some(serde_json::json!({
        "x": f.origin.x, "y": f.origin.y, "w": f.size.width, "h": f.size.height,
        "hidden": v.isHidden(), "in_window": in_window, "first_responder": focused,
    }))
}

/// WebKit storage is per app, not per profile; each data directory keeps its own store
/// so the shell's settings and onboarding state stay with the profile.
fn store_id() -> [u8; 16] {
    let path = crate::data_dir().map(|d| d.join("webkit-store-id"));
    let saved = path.as_ref().ok().and_then(|p| std::fs::read(p).ok()).and_then(|b| <[u8; 16]>::try_from(b).ok());
    saved.unwrap_or_else(|| {
        let id = *uuid::Uuid::new_v4().as_bytes();
        if let Ok(p) = &path {
            let _ = std::fs::write(p, id);
        }
        id
    })
}

/// Window moves must start while AppKit is still handling the mouse-down,
/// so they don't wait for the browser's task queue.
fn shell_message(bridge: &Bridge, body: String) {
    let Ok(m) = serde_json::from_str::<serde_json::Value>(&body) else { return };
    match m["cmd"].as_str() {
        Some("drag") => {
            with(|n| {
                let app = NSApplication::sharedApplication(MainThreadMarker::from(&*n.window));
                if let Some(event) = app.currentEvent() {
                    n.window.performWindowDragWithEvent(&event);
                }
            });
        }
        Some("zoom") => {
            with(|n| n.window.zoom(None));
        }
        _ => bridge.send(Event::Command(m)),
    }
}

fn place_lights(window: &NSWindow) {
    let buttons = [NSWindowButton::CloseButton, NSWindowButton::MiniaturizeButton, NSWindowButton::ZoomButton]
        .map(|b| window.standardWindowButton(b));
    let [Some(close), Some(mini), Some(zoom)] = buttons else { return };
    let Some(bar) = (unsafe { close.superview() }).and_then(|v| unsafe { v.superview() }) else { return };
    let height = close.frame().size.height + LIGHTS.1;
    let mut rect = bar.frame();
    rect.size.height = height;
    rect.origin.y = window.frame().size.height - height;
    bar.setFrame(rect);
    let gap = mini.frame().origin.x - close.frame().origin.x;
    for (i, b) in [close, mini, zoom].iter().enumerate() {
        // The container is `height` tall and not flipped: y = 0 puts the buttons LIGHTS.1 from the top.
        b.setFrameOrigin(NSPoint::new(LIGHTS.0 + i as f64 * gap, 0.0));
    }
}

/// The parent NSView for new Chromium tabs.
pub(super) fn content() -> Option<*mut c_void> {
    with(|n| Retained::as_ptr(&n.content) as *mut c_void)
}

pub(super) fn size() -> Option<(i32, i32)> {
    with(|n| {
        let b = n.content.bounds();
        (b.size.width.round() as i32, b.size.height.round() as i32)
    })
}

fn view<'a>(handle: *mut c_void) -> Option<&'a NSView> {
    unsafe { (handle as *const NSView).as_ref() }
}

/// Position a tab's Chromium view; rounded so pages sit inside the shell's cards.
pub(super) fn place(handle: *mut c_void, bounds: &Rect, visible: bool) {
    let Some(v) = view(handle) else { return };
    let frame = NSRect::new(
        NSPoint::new(bounds.x as f64, bounds.y as f64),
        NSSize::new(bounds.width as f64, bounds.height as f64),
    );
    if v.frame() != frame {
        v.setFrame(frame);
    }
    // Between the window resizing and the shell reporting new rects, stretch with it.
    v.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
    v.setHidden(!visible);
    v.setWantsLayer(true);
    unsafe {
        let layer: *mut AnyObject = msg_send![v, layer];
        if !layer.is_null() {
            let _: () = msg_send![layer, setCornerRadius: 10.0f64];
            let _: () = msg_send![layer, setMasksToBounds: true];
        }
    }
}

/// Completes a tab close: CEF destroys the browser once its view leaves the window.
pub(super) fn detach(handle: *mut c_void) {
    if let Some(v) = view(handle) {
        v.removeFromSuperview();
    }
}

pub(super) fn shell_eval(js: &str) {
    with(|n| {
        let _ = n.shell.evaluate_script(js);
    });
}

pub(super) fn shell_eval_with(js: &str, done: impl FnOnce(String) + Send + 'static) {
    let done = std::sync::Mutex::new(Some(done));
    with(|n| {
        let _ = n.shell.evaluate_script_with_callback(js, move |result| {
            if let Some(done) = done.lock().unwrap().take() {
                done(result);
            }
        });
    });
}

pub(super) fn focus_shell() {
    with(|n| {
        let _ = n.shell.focus();
    });
}

pub(super) fn shell_devtools() {
    with(|n| n.shell.open_devtools());
}

pub(super) fn has_shell() -> bool {
    with(|_| ()).is_some()
}

pub(super) fn close() {
    if let Some(n) = NATIVE.with(|n| n.borrow_mut().take()) {
        n.window.setDelegate(None);
        n.window.close();
    }
}
