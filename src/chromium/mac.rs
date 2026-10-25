//! CEF requires NSApplication's event-dispatch protocols on macOS.
//! Protocol pattern adapted from cef-rs cefsimple (MIT / Apache-2.0).
use super::{Bridge, Event};
use cef::application_mac::{CefAppProtocol, CrAppControlProtocol, CrAppProtocol};
use muda::{
    accelerator::{Accelerator, Code, Modifiers},
    Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu,
};
use objc2::{
    class, define_class, extern_methods, msg_send,
    rc::Retained,
    runtime::{AnyClass, AnyObject, Bool, Imp, NSObject, NSObjectProtocol, Sel},
    sel, ClassType, DefinedClass,
};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSEvent};
use objc2_foundation::{NSData, NSString};
use serde_json::{json, Value as Json};
use std::{cell::Cell, sync::OnceLock};
static BRIDGE: OnceLock<Bridge> = OnceLock::new();
#[derive(Default)]
pub struct ApplicationState {
    sending: Cell<bool>,
}
define_class!(
    #[unsafe(super(NSApplication))]
    #[ivars = ApplicationState]
    pub struct AtriumApplication;
    impl AtriumApplication {
        #[unsafe(method(sendEvent:))]
        unsafe fn send_event(&self, event: &NSEvent) {
            let previous = self.ivars().sending.replace(true);
            let _: () = msg_send![super(self), sendEvent: event];
            self.ivars().sending.set(previous);
        }
        #[unsafe(method(terminate:))]
        unsafe fn terminate(&self, _: &AnyObject) { if let Some(bridge)=BRIDGE.get(){bridge.send(Event::Quit);} }
        // VoiceOver sets AXEnhancedUserInterface on the application; Voice Control and other
        // assistive clients set AXManualAccessibility. CEF leaves enabling Chromium's tree to us.
        #[unsafe(method(accessibilitySetValue:forAttribute:))]
        unsafe fn accessibility_set_value(&self, value: Option<&AnyObject>, attribute: Option<&NSString>) {
            if let (Some(attribute), Some(bridge)) = (attribute, BRIDGE.get()) {
                let name = attribute.to_string();
                if name == "AXEnhancedUserInterface" || name == "AXManualAccessibility" {
                    let on = value.is_some_and(|v| {
                        let numeric: Bool = msg_send![v, respondsToSelector: sel!(intValue)];
                        numeric.as_bool() && { let n: i32 = msg_send![v, intValue]; n != 0 }
                    });
                    bridge.send(Event::Assistive(name, on));
                }
            }
            let _: () = msg_send![super(self), accessibilitySetValue: value, forAttribute: attribute];
        }
        // Chromium and extensions can ask to activate the app or change its policy; a
        // background instance must never become the active application.
        #[unsafe(method(activateIgnoringOtherApps:))]
        unsafe fn activate_ignoring_other_apps(&self, flag: Bool) {
            if background() { return eprintln!("background window mode: refused activateIgnoringOtherApps"); }
            let _: () = msg_send![super(self), activateIgnoringOtherApps: flag];
        }
        #[unsafe(method(activate))]
        unsafe fn activate(&self) {
            if background() { return eprintln!("background window mode: refused activate"); }
            let _: () = msg_send![super(self), activate];
        }
        #[unsafe(method(setActivationPolicy:))]
        unsafe fn set_activation_policy(&self, policy: NSApplicationActivationPolicy) -> Bool {
            let policy = if background() && policy != NSApplicationActivationPolicy::Prohibited {
                eprintln!("background window mode: refused activation policy {}", policy.0);
                NSApplicationActivationPolicy::Prohibited
            } else {
                policy
            };
            msg_send![super(self), setActivationPolicy: policy]
        }
        // An invisible modal would block the UI thread forever.
        #[unsafe(method(runModalForWindow:))]
        unsafe fn run_modal_for_window(&self, window: &AnyObject) -> isize {
            if background() {
                eprintln!("background window mode: refused a modal window");
                return -1001; // NSModalResponseAbort
            }
            msg_send![super(self), runModalForWindow: window]
        }
    }
    unsafe impl NSObjectProtocol for AtriumApplication {}
    unsafe impl CrAppProtocol for AtriumApplication {
        #[unsafe(method(isHandlingSendEvent))]
        unsafe fn is_handling(&self)->Bool { self.ivars().sending.get().into() }
    }
    unsafe impl CrAppControlProtocol for AtriumApplication {
        #[unsafe(method(setHandlingSendEvent:))]
        unsafe fn set_handling(&self,value:Bool){self.ivars().sending.set(value.as_bool());}
    }
    unsafe impl CefAppProtocol for AtriumApplication {}
);
impl AtriumApplication {
    extern_methods! { #[unsafe(method(sharedApplication))] fn shared_application()->Retained<Self>; }
}
// AppKit delivers URLs from other apps (links, `open URL`, HTML files) to the delegate.
define_class!(
    #[unsafe(super(NSObject))]
    pub struct AtriumAppDelegate;
    impl AtriumAppDelegate {
        #[unsafe(method(application:openURLs:))]
        unsafe fn open_urls(&self, _app: &AnyObject, urls: &AnyObject) {
            let count: usize = msg_send![urls, count];
            let urls = (0..count)
                .filter_map(|i| {
                    let url: *mut AnyObject = msg_send![urls, objectAtIndex: i];
                    let text: *mut NSString = msg_send![url, absoluteString];
                    text.as_ref().map(|t| t.to_string())
                })
                .collect::<Vec<_>>();
            if let Some(bridge) = BRIDGE.get() {
                bridge.send(Event::Commands(super::commands::Ev::OpenUrls(urls)));
            }
        }
    }
    unsafe impl NSObjectProtocol for AtriumAppDelegate {}
);
/// Set before Chromium starts so URLs delivered at launch reach the tab manager.
pub(super) fn set_bridge(bridge: Bridge) {
    let _ = BRIDGE.set(bridge);
}
/// Test hook: sends URLs through the same delegate method AppKit uses.
pub(super) fn deliver_open_urls(urls: &[String]) {
    ensure_delegate();
    unsafe {
        let app = AtriumApplication::shared_application();
        let delegate: *mut AnyObject = msg_send![&*app, delegate];
        let Some(delegate) = delegate.as_ref() else { return };
        let list: Retained<AnyObject> = msg_send![class!(NSMutableArray), new];
        for url in urls {
            let url: *mut AnyObject = msg_send![class!(NSURL), URLWithString: &*NSString::from_str(url)];
            if let Some(url) = url.as_ref() {
                let _: () = msg_send![&*list, addObject: url];
            }
        }
        let _: () = msg_send![delegate, application: &*app, openURLs: &*list];
    }
}
pub(super) fn delegate_installed() -> bool {
    unsafe {
        let delegate: *mut AnyObject = msg_send![&*AtriumApplication::shared_application(), delegate];
        delegate.as_ref().is_some_and(|d| d.class() == AtriumAppDelegate::class())
    }
}
/// Chromium may replace the delegate while starting. Put ours back so links from
/// other apps still arrive. NSApplication holds the delegate weakly.
pub(super) fn ensure_delegate() {
    if delegate_installed() {
        return;
    }
    unsafe {
        let app = AtriumApplication::shared_application();
        let delegate: Retained<AnyObject> = msg_send![AtriumAppDelegate::class(), new];
        let _: () = msg_send![&*app, setDelegate: &*delegate];
        std::mem::forget(delegate);
    }
}

/// Background test instances use a private pasteboard so tests never touch the user's clipboard.
fn pasteboard() -> Retained<AnyObject> {
    unsafe {
        if background() {
            let name = NSString::from_str(&format!("dev.atrium-rs.test.{}", std::process::id()));
            msg_send![class!(NSPasteboard), pasteboardWithName: &*name]
        } else {
            msg_send![class!(NSPasteboard), generalPasteboard]
        }
    }
}
const TEXT_TYPE: &str = "public.utf8-plain-text";
const PNG_TYPE: &str = "public.png";
pub(super) fn copy_text(text: &str) {
    let board = pasteboard();
    unsafe {
        let _: isize = msg_send![&*board, clearContents];
        let _: Bool = msg_send![&*board, setString: &*NSString::from_str(text), forType: &*NSString::from_str(TEXT_TYPE)];
    }
}
pub(super) fn copy_png(png: &[u8]) {
    let board = pasteboard();
    unsafe {
        let _: isize = msg_send![&*board, clearContents];
        let _: Bool = msg_send![&*board, setData: &*NSData::with_bytes(png), forType: &*NSString::from_str(PNG_TYPE)];
    }
}
pub(super) fn test_pasteboard() -> Json {
    let board = pasteboard();
    unsafe {
        let text: Option<Retained<NSString>> = msg_send![&*board, stringForType: &*NSString::from_str(TEXT_TYPE)];
        let png: Option<Retained<NSData>> = msg_send![&*board, dataForType: &*NSString::from_str(PNG_TYPE)];
        let header = png.as_ref().map(|d| d.to_vec().into_iter().take(8).collect::<Vec<u8>>());
        json!({ "text": text.map(|t| t.to_string()), "png_bytes": png.map(|d| d.len()), "png_header": header })
    }
}
pub(super) fn release_test_pasteboard() {
    if background() {
        unsafe {
            let _: () = msg_send![&*pasteboard(), releaseGlobally];
        }
    }
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn LSCopyDefaultHandlerForURLScheme(scheme: &NSString) -> *mut NSString;
    fn LSSetDefaultHandlerForURLScheme(scheme: &NSString, bundle: &NSString) -> i32;
    fn LSSetDefaultRoleHandlerForContentType(content: &NSString, role: u32, bundle: &NSString) -> i32;
}
fn bundle_id() -> Option<String> {
    unsafe {
        let bundle: *mut AnyObject = msg_send![class!(NSBundle), mainBundle];
        let id: *mut NSString = msg_send![bundle, bundleIdentifier];
        id.as_ref().map(|i| i.to_string())
    }
}
pub(super) fn default_browser() -> Json {
    ensure_delegate();
    let handler = |scheme: &str| unsafe {
        Retained::from_raw(LSCopyDefaultHandlerForURLScheme(&NSString::from_str(scheme))).map(|h| h.to_string())
    };
    let (http, https, bundle) = (handler("http"), handler("https"), bundle_id());
    let is_default = bundle.as_ref().is_some_and(|b| {
        [&http, &https].iter().all(|h| h.as_ref().is_some_and(|h| h.eq_ignore_ascii_case(b)))
    });
    json!({ "bundle_id": bundle, "http": http, "https": https, "is_default": is_default, "delegate": delegate_installed() })
}
/// macOS asks the user to confirm with a system dialog.
pub(super) fn make_default_browser() -> Result<(), String> {
    let bundle = NSString::from_str(&bundle_id().ok_or("This build is not running from an app bundle")?);
    unsafe {
        for scheme in ["http", "https"] {
            let status = LSSetDefaultHandlerForURLScheme(&NSString::from_str(scheme), &bundle);
            if status != 0 {
                return Err(format!("macOS refused to change the default browser (error {status})"));
            }
        }
        // kLSRolesViewer
        for content in ["public.html", "public.xhtml"] {
            LSSetDefaultRoleHandlerForContentType(&NSString::from_str(content), 2, &bundle);
        }
    }
    Ok(())
}
/// `ATRIUM_BACKGROUND_WINDOW=1` is for test instances that must never activate, take focus, or be seen.
pub fn background() -> bool {
    std::env::var_os("ATRIUM_BACKGROUND_WINDOW").is_some_and(|v| v == "1")
}
pub fn setup() {
    let app = AtriumApplication::shared_application();
    app.setActivationPolicy(if background() {
        NSApplicationActivationPolicy::Prohibited
    } else {
        NSApplicationActivationPolicy::Regular
    });
    ensure_delegate();
    if background() {
        guard_all_windows();
    }
}
/// Transparent, shadowless, click-through, excluded from window cycling, and off-screen.
unsafe fn conceal(window: &AnyObject) {
    let _: () = msg_send![window, setAlphaValue: 0.0f64];
    let _: () = msg_send![window, setHasShadow: Bool::NO];
    let _: () = msg_send![window, setIgnoresMouseEvents: Bool::YES];
    let _: () = msg_send![window, setExcludedFromWindowsMenu: Bool::YES];
    // NSWindowCollectionBehaviorTransient | NSWindowCollectionBehaviorIgnoresCycle
    let _: () = msg_send![window, setCollectionBehavior: (1usize << 3) | (1usize << 6)];
    let origin = objc2_foundation::NSPoint::new(-30000.0, -30000.0);
    let _: () = msg_send![window, setFrameOrigin: origin];
}
unsafe fn window_of(handle: *mut std::ffi::c_void) -> Option<&'static AnyObject> {
    let view = handle as *mut AnyObject;
    if view.is_null() {
        return None;
    }
    let window: *mut AnyObject = msg_send![view, window];
    window.as_ref()
}
/// Keeps the window ordered in, so Chromium still paints, but transparent, click-through,
/// off-screen, and behind other windows.
pub fn keep_out_of_sight(handle: *mut std::ffi::c_void) {
    unsafe {
        let Some(window) = window_of(handle) else { return };
        conceal(window);
        let _: () = msg_send![window, orderBack: std::ptr::null::<AnyObject>()];
    }
}
/// Removes a browser's top-level window from the screen.
pub fn hide_window(handle: *mut std::ffi::c_void) {
    unsafe {
        let Some(window) = window_of(handle) else { return };
        if background() {
            conceal(window);
        }
        let _: () = msg_send![window, orderOut: std::ptr::null::<AnyObject>()];
    }
}

type OrderWindow = unsafe extern "C-unwind" fn(*mut AnyObject, Sel, isize, isize);
type WithSender = unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject);
type NoArgs = unsafe extern "C-unwind" fn(*mut AnyObject, Sel);
type Activate = unsafe extern "C-unwind" fn(*mut AnyObject, Sel, usize) -> Bool;
static ORDER_WINDOW: OnceLock<Imp> = OnceLock::new();
static ACTIVATE_WITH_OPTIONS: OnceLock<Imp> = OnceLock::new();

/// Applies the background treatment to every window the process orders in, including windows
/// Chromium or an extension creates on its own, and keeps them from becoming key.
fn guard_all_windows() {
    unsafe extern "C-unwind" fn order_window(this: *mut AnyObject, sel: Sel, place: isize, relative: isize) {
        // NSWindowOut = 0, NSWindowAbove = 1, NSWindowBelow = -1
        if let (Some(window), true) = (this.as_ref(), place != 0) {
            conceal(window);
        }
        let Some(original) = ORDER_WINDOW.get() else { return };
        let original: OrderWindow = std::mem::transmute(*original);
        if place == 1 {
            original(this, sel, -1, 0);
        } else {
            original(this, sel, place, relative);
        }
    }
    unsafe extern "C-unwind" fn order_back(this: *mut AnyObject, _: Sel, _: *mut AnyObject) {
        let _: () = msg_send![this, orderWindow: -1isize, relativeTo: 0isize];
    }
    unsafe extern "C-unwind" fn order_back_regardless(this: *mut AnyObject, _: Sel) {
        let _: () = msg_send![this, orderWindow: -1isize, relativeTo: 0isize];
    }
    unsafe extern "C-unwind" fn refuse(_: *mut AnyObject, _: Sel) {}
    unsafe extern "C-unwind" fn activate_with_options(this: *mut AnyObject, sel: Sel, options: usize) -> Bool {
        let current: *mut AnyObject = msg_send![AnyClass::get(c"NSRunningApplication").unwrap(), currentApplication];
        if this == current {
            eprintln!("background window mode: refused activateWithOptions");
            return Bool::NO;
        }
        match ACTIVATE_WITH_OPTIONS.get() {
            Some(original) => std::mem::transmute::<Imp, Activate>(*original)(this, sel, options),
            None => Bool::NO,
        }
    }
    unsafe fn replace(class: &AnyClass, sel: Sel, imp: Imp) -> Option<Imp> {
        class.instance_method(sel).map(|method| method.set_implementation(imp))
    }
    unsafe {
        let Some(window) = AnyClass::get(c"NSWindow") else { return };
        if let Some(original) = replace(window, sel!(orderWindow:relativeTo:), std::mem::transmute::<OrderWindow, Imp>(order_window)) {
            let _ = ORDER_WINDOW.set(original);
        }
        for sel in [sel!(orderFront:), sel!(makeKeyAndOrderFront:)] {
            replace(window, sel, std::mem::transmute::<WithSender, Imp>(order_back));
        }
        replace(window, sel!(orderFrontRegardless), std::mem::transmute::<NoArgs, Imp>(order_back_regardless));
        for sel in [sel!(makeKeyWindow), sel!(makeMainWindow)] {
            replace(window, sel, std::mem::transmute::<NoArgs, Imp>(refuse));
        }
        if let Some(running) = AnyClass::get(c"NSRunningApplication") {
            if let Some(original) = replace(running, sel!(activateWithOptions:), std::mem::transmute::<Activate, Imp>(activate_with_options)) {
                let _ = ACTIVATE_WITH_OPTIONS.set(original);
            }
        }
    }
}

/// Safety net for background instances: conceals any visible window and resigns activation.
pub fn sweep() {
    if !background() {
        return;
    }
    unsafe {
        let app = AtriumApplication::shared_application();
        if app.activationPolicy() != NSApplicationActivationPolicy::Prohibited {
            eprintln!("background window mode: restored the Prohibited activation policy");
            app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
        }
        let active: Bool = msg_send![&*app, isActive];
        if active.as_bool() {
            eprintln!("background window mode: the app became active; deactivating");
            let _: () = msg_send![&*app, deactivate];
        }
        let windows: *mut AnyObject = msg_send![&*app, windows];
        let Some(windows) = windows.as_ref() else { return };
        let count: usize = msg_send![windows, count];
        for i in 0..count {
            let window: *mut AnyObject = msg_send![windows, objectAtIndex: i];
            let Some(window) = window.as_ref() else { continue };
            let alpha: f64 = msg_send![window, alphaValue];
            let visible: Bool = msg_send![window, isVisible];
            if alpha > 0.0 && visible.as_bool() {
                eprintln!("background window mode: concealed a visible window");
                conceal(window);
                let _: () = msg_send![window, orderBack: std::ptr::null::<AnyObject>()];
            }
        }
    }
}
pub fn voiceover_enabled() -> bool {
    unsafe {
        let workspace: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
        let Some(workspace) = workspace.as_ref() else { return false };
        let on: Bool = msg_send![workspace, isVoiceOverEnabled];
        on.as_bool()
    }
}
/// Sends an attribute to the application exactly as an assistive client would, in process.
pub fn set_application_attribute(name: &str, on: bool) {
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        let value: *mut AnyObject = msg_send![class!(NSNumber), numberWithBool: Bool::new(on)];
        let _: () = msg_send![app, accessibilitySetValue: value, forAttribute: &*NSString::from_str(name)];
    }
}
/// Walks the window's NSAccessibility tree in process; this needs no TCC permission.
pub fn accessibility_tree(handle: *mut std::ffi::c_void, max_nodes: usize) -> serde_json::Value {
    unsafe fn text(object: *mut AnyObject) -> Option<String> {
        let object = object.as_ref()?;
        let string: Bool = msg_send![object, isKindOfClass: class!(NSString)];
        string
            .as_bool()
            .then(|| (*(object as *const AnyObject as *const NSString)).to_string())
            .filter(|s| !s.is_empty())
    }
    macro_rules! attribute {
        ($object:expr, $selector:ident) => {{
            let responds: Bool = msg_send![$object, respondsToSelector: sel!($selector)];
            if responds.as_bool() {
                let value: *mut AnyObject = msg_send![$object, $selector];
                text(value)
            } else {
                None
            }
        }};
    }
    unsafe fn walk(object: &AnyObject, depth: usize, budget: &mut usize) -> serde_json::Value {
        *budget = budget.saturating_sub(1);
        let mut node = serde_json::json!({
            "role": attribute!(object, accessibilityRole),
            "title": attribute!(object, accessibilityTitle),
            "label": attribute!(object, accessibilityLabel),
            "value": attribute!(object, accessibilityValue),
        });
        let responds: Bool = msg_send![object, respondsToSelector: sel!(accessibilityChildren)];
        if responds.as_bool() && depth < 60 {
            let children: *mut AnyObject = msg_send![object, accessibilityChildren];
            if let Some(children) = children.as_ref() {
                let count: usize = msg_send![children, count];
                let mut out = Vec::new();
                for i in 0..count {
                    if *budget == 0 {
                        node["truncated"] = true.into();
                        break;
                    }
                    let child: *mut AnyObject = msg_send![children, objectAtIndex: i];
                    if let Some(child) = child.as_ref() {
                        out.push(walk(child, depth + 1, budget));
                    }
                }
                node["children"] = out.into();
            }
        }
        node
    }
    let view = handle as *mut AnyObject;
    objc2::rc::autoreleasepool(|_| unsafe {
        let Some(view) = view.as_ref() else { return serde_json::Value::Null };
        let window: *mut AnyObject = msg_send![view, window];
        let Some(window) = window.as_ref() else { return serde_json::Value::Null };
        let content: *mut AnyObject = msg_send![window, contentView];
        let Some(content) = content.as_ref() else { return serde_json::Value::Null };
        let mut budget = max_nodes;
        walk(content, 0, &mut budget)
    })
}
pub(super) fn menu(bridge: Bridge) -> Menu {
    let _ = BRIDGE.set(bridge.clone());
    let menu = build_menu();
    menu.init_for_nsapp();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        bridge.send(Event::Command(
            serde_json::json!({"cmd":"menu","name":event.id.0}),
        ))
    }));
    menu
}

fn build_menu() -> Menu {
    let item = |id: &str, text: &str, mods: Modifiers, code: Code| {
        MenuItem::with_id(id, text, true, Some(Accelerator::new(mods, code)))
    };
    let cmd = Modifiers::META;
    let cmd_shift = Modifiers::META | Modifiers::SHIFT;
    let cmd_alt = Modifiers::META | Modifiers::ALT;
    let menu = Menu::new();
    let name = crate::brand::PRODUCT_NAME;
    let app = Submenu::with_items(
        name,
        true,
        &[
            &MenuItem::with_id("about", format!("About {name}"), true, None),
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id("make-default-browser", "Make Atrium the Default Browser…", true, None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::services(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::hide(None),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::quit(None),
        ],
    )
    .unwrap();
    let file = Submenu::with_items(
        "File",
        true,
        &[
            &item("new-tab", "New Tab", cmd, Code::KeyT),
            &item("command-bar", "Command Bar", cmd, Code::KeyK),
            &item("newChat", "New Task", cmd, Code::KeyN),
            &item("open-location", "Edit URL", cmd, Code::KeyL),
            &PredefinedMenuItem::separator(),
            &item("close-tab", "Close Tab", cmd, Code::KeyW),
            &item("reopen-tab", "Reopen Closed Tab", cmd_shift, Code::KeyT),
            &item("copyUrl", "Copy URL", cmd_shift, Code::KeyC),
            &PredefinedMenuItem::separator(),
            &item("save-page", "Save Page As…", cmd, Code::KeyS),
            &MenuItem::with_id("export-pdf", "Export as PDF…", true, None),
            &item("print", "Print…", cmd, Code::KeyP),
        ],
    )
    .unwrap();
    let edit = Submenu::with_items(
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(None),
            &PredefinedMenuItem::redo(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::cut(None),
            &PredefinedMenuItem::copy(None),
            &PredefinedMenuItem::paste(None),
            &PredefinedMenuItem::select_all(None),
            &PredefinedMenuItem::separator(),
            &item("find", "Find…", cmd, Code::KeyF),
            &item("find-next", "Find Next", cmd, Code::KeyG),
            &item("find-prev", "Find Previous", cmd_shift, Code::KeyG),
        ],
    )
    .unwrap();
    let view = Submenu::with_items(
        "View",
        true,
        &[
            &item("reload", "Reload", cmd, Code::KeyR),
            &item("back", "Back", cmd, Code::BracketLeft),
            &item("forward", "Forward", cmd, Code::BracketRight),
            &PredefinedMenuItem::separator(),
            &item("zoom-in", "Zoom In", cmd, Code::Equal),
            &item("zoom-out", "Zoom Out", cmd, Code::Minus),
            &item("zoom-reset", "Actual Size", cmd, Code::Digit0),
            &PredefinedMenuItem::separator(),
            &item("toggleSidebar", "Show/Hide Tasks", cmd_shift, Code::KeyS),
            &item("toggleAssistant", "Show/Hide Thread", cmd, Code::KeyE),
            &item("toggleSplit", "Split View", cmd_shift, Code::Equal),
            &PredefinedMenuItem::separator(),
            &item("next-tab", "Next Tab", cmd_alt, Code::ArrowDown),
            &item("prev-tab", "Previous Tab", cmd_alt, Code::ArrowUp),
            &PredefinedMenuItem::separator(),
            &item("devtools", "Developer Tools", cmd_alt, Code::KeyI),
            &item(
                "devtools-shell",
                "Inspect Browser UI",
                cmd_shift,
                Code::KeyI,
            ),
            &PredefinedMenuItem::fullscreen(None),
        ],
    )
    .unwrap();
    let window = Submenu::with_items(
        "Window",
        true,
        &[
            &PredefinedMenuItem::minimize(None),
            &PredefinedMenuItem::close_window(None),
        ],
    )
    .unwrap();
    menu.append_items(&[&app, &file, &edit, &view, &window])
        .unwrap();
    menu
}
