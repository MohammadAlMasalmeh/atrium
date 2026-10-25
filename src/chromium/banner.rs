//! The "Play in WebKit" offer, drawn like Chrome's permission bubbles: a native card over
//! the top-left of the page. The page is a native view above the shell, so the shell
//! can't draw over it. Its buttons send the same commands as the shell would.
use super::{native::FlippedView, Bridge, Event, Rect};
use objc2::{define_class, msg_send, rc::Retained, runtime::AnyObject, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBezelStyle, NSButton, NSColor, NSFont, NSImage, NSImageView, NSTextField, NSView, NSVisualEffectBlendingMode,
    NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView, NSWindowOrderingMode,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use serde_json::json;
use std::cell::RefCell;

const WIDTH: f64 = 340.0;
const HEIGHT: f64 = 124.0;

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = (Bridge, u64)]
    struct Target;
    impl Target {
        #[unsafe(method(play:))]
        fn play(&self, _sender: &NSButton) {
            let (bridge, tab) = self.ivars();
            bridge.send(Event::Command(json!({ "cmd": "webkitOpen", "tab": tab })));
        }
        #[unsafe(method(dismiss:))]
        fn dismiss(&self, _sender: &NSButton) {
            let (bridge, tab) = self.ivars();
            bridge.send(Event::Command(json!({ "cmd": "passwordDismiss", "tab": tab })));
        }
    }
    unsafe impl NSObjectProtocol for Target {}
);

struct Banner {
    tab: u64,
    host: String,
    view: Retained<NSView>,
    _target: Retained<Target>,
}

thread_local! {
    static BANNER: RefCell<Option<Banner>> = const { RefCell::new(None) };
}

fn frame(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

fn label(text: &str, font: &NSFont, color: &NSColor, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let label = NSTextField::wrappingLabelWithString(&NSString::from_str(text), mtm);
    label.setFont(Some(font));
    label.setTextColor(Some(color));
    label
}

fn build(bridge: &Bridge, tab: u64, host: &str, mtm: MainThreadMarker) -> Banner {
    let target: Retained<Target> = unsafe { msg_send![super(Target::alloc(mtm).set_ivars((bridge.clone(), tab))), init] };
    // The outer view casts the shadow; the glass inside is clipped to the rounded corners.
    let view = NSView::initWithFrame(NSView::alloc(mtm), frame(0.0, 0.0, WIDTH, HEIGHT));
    view.setWantsLayer(true);
    unsafe {
        let layer: *mut AnyObject = msg_send![&view, layer];
        if !layer.is_null() {
            let _: () = msg_send![layer, setShadowOpacity: 0.28f32];
            let _: () = msg_send![layer, setShadowRadius: 14.0f64];
            let _: () = msg_send![layer, setShadowOffset: NSSize::new(0.0, -4.0)];
        }
    }
    let glass = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), view.bounds());
    glass.setMaterial(NSVisualEffectMaterial::Popover);
    glass.setBlendingMode(NSVisualEffectBlendingMode::WithinWindow);
    glass.setState(NSVisualEffectState::Active);
    glass.setWantsLayer(true);
    unsafe {
        let layer: *mut AnyObject = msg_send![&glass, layer];
        if !layer.is_null() {
            let _: () = msg_send![layer, setCornerRadius: 12.0f64];
            let _: () = msg_send![layer, setMasksToBounds: true];
            let _: () = msg_send![layer, setBorderWidth: 0.5f64];
            let border = NSColor::separatorColor().CGColor();
            let _: () = msg_send![layer, setBorderColor: &*border];
        }
    }
    view.addSubview(&glass);
    let content: Retained<FlippedView> = unsafe { msg_send![FlippedView::alloc(mtm), initWithFrame: glass.bounds()] };
    glass.addSubview(&content);

    let symbol = |name: &str, label: &str| {
        NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(name), Some(&NSString::from_str(label)))
    };
    if let Some(image) = symbol("play.rectangle.fill", "Video") {
        let icon = NSImageView::imageViewWithImage(&image, mtm);
        icon.setContentTintColor(Some(&NSColor::controlAccentColor()));
        icon.setFrame(frame(16.0, 16.0, 22.0, 18.0));
        content.addSubview(&icon);
    }
    let title = label(&format!("{host} has video Atrium can't play"), &NSFont::boldSystemFontOfSize(13.0), &NSColor::labelColor(), mtm);
    title.setFrame(frame(48.0, 14.0, WIDTH - 48.0 - 40.0, 34.0));
    content.addSubview(&title);
    let body = label(
        "Its format needs Apple's media engine. Open this page in WebKit to watch it.",
        &NSFont::systemFontOfSize(12.0),
        &NSColor::secondaryLabelColor(),
        mtm,
    );
    body.setFrame(frame(48.0, 46.0, WIDTH - 48.0 - 16.0, 32.0));
    content.addSubview(&body);
    if let Some(image) = symbol("xmark", "Close") {
        let close = unsafe { NSButton::buttonWithImage_target_action(&image, Some(&target), Some(sel!(dismiss:)), mtm) };
        close.setBordered(false);
        close.setContentTintColor(Some(&NSColor::secondaryLabelColor()));
        close.setFrame(frame(WIDTH - 32.0, 12.0, 20.0, 20.0));
        content.addSubview(&close);
    }
    let button = |title: &str, action, primary: bool| {
        let b = unsafe { NSButton::buttonWithTitle_target_action(&NSString::from_str(title), Some(&target), Some(action), mtm) };
        b.setBezelStyle(NSBezelStyle::Push);
        if primary {
            // The default button: accent-filled, and Return presses it.
            b.setKeyEquivalent(&NSString::from_str("\r"));
        }
        b.sizeToFit();
        b
    };
    let play = button("Play in WebKit", sel!(play:), true);
    let later = button("Not now", sel!(dismiss:), false);
    let (pw, lw) = (play.frame().size.width.max(110.0), later.frame().size.width.max(80.0));
    play.setFrame(frame(WIDTH - 14.0 - pw, HEIGHT - 42.0, pw, 30.0));
    later.setFrame(frame(WIDTH - 14.0 - pw - 8.0 - lw, HEIGHT - 42.0, lw, 30.0));
    content.addSubview(&later);
    content.addSubview(&play);
    Banner { tab, host: host.to_string(), view, _target: target }
}

/// Shows the offer for `tab` at the top-left of `page`, or removes it.
pub(super) fn show(bridge: &Bridge, offer: Option<(u64, String)>, page: &Rect) {
    let Some(parent) = super::native::content().and_then(|p| unsafe { (p as *const NSView).as_ref() }) else { return };
    BANNER.with(|b| {
        let mut b = b.borrow_mut();
        let Some((tab, host)) = offer else {
            if let Some(old) = b.take() {
                old.view.removeFromSuperview();
            }
            return;
        };
        if b.as_ref().is_some_and(|old| old.tab != tab || old.host != host) {
            b.take().unwrap().view.removeFromSuperview();
        }
        let banner = b.get_or_insert_with(|| build(bridge, tab, &host, MainThreadMarker::from(parent)));
        banner.view.setFrameOrigin(NSPoint::new(page.x as f64 + 12.0, page.y as f64 + 10.0));
        // Pages, previews and WebKit views are added later; stay above all of them.
        let on_top = parent.subviews().lastObject().is_some_and(|v| std::ptr::eq(&*v, &*banner.view));
        if !on_top {
            banner.view.removeFromSuperview();
            parent.addSubview_positioned_relativeTo(&banner.view, NSWindowOrderingMode::Above, None);
        }
    });
}
