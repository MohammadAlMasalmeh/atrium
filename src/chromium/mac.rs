//! CEF requires NSApplication's event-dispatch protocols on macOS.
//! Protocol pattern adapted from cef-rs cefsimple (MIT / Apache-2.0).
use super::{Bridge, Event};
use cef::application_mac::{CefAppProtocol, CrAppControlProtocol, CrAppProtocol};
use muda::{
    accelerator::{Accelerator, Code, Modifiers},
    AboutMetadata, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu,
};
use objc2::{
    class, define_class, extern_methods, msg_send,
    rc::Retained,
    runtime::{AnyObject, Bool, NSObject, NSObjectProtocol},
    ClassType, DefinedClass,
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
    pub struct AsideApplication;
    impl AsideApplication {
        #[unsafe(method(sendEvent:))]
        unsafe fn send_event(&self, event: &NSEvent) {
            let previous = self.ivars().sending.replace(true);
            let _: () = msg_send![super(self), sendEvent: event];
            self.ivars().sending.set(previous);
        }
        #[unsafe(method(terminate:))]
        unsafe fn terminate(&self, _: &AnyObject) { if let Some(bridge)=BRIDGE.get(){bridge.send(Event::Quit);} }
    }
    unsafe impl NSObjectProtocol for AsideApplication {}
    unsafe impl CrAppProtocol for AsideApplication {
        #[unsafe(method(isHandlingSendEvent))]
        unsafe fn is_handling(&self)->Bool { self.ivars().sending.get().into() }
    }
    unsafe impl CrAppControlProtocol for AsideApplication {
        #[unsafe(method(setHandlingSendEvent:))]
        unsafe fn set_handling(&self,value:Bool){self.ivars().sending.set(value.as_bool());}
    }
    unsafe impl CefAppProtocol for AsideApplication {}
);
impl AsideApplication {
    extern_methods! { #[unsafe(method(sharedApplication))] fn shared_application()->Retained<Self>; }
}
// AppKit delivers URLs from other apps (links, `open URL`, HTML files) to the delegate.
define_class!(
    #[unsafe(super(NSObject))]
    pub struct AsideAppDelegate;
    impl AsideAppDelegate {
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
    unsafe impl NSObjectProtocol for AsideAppDelegate {}
);
/// Set before Chromium starts so URLs delivered at launch reach the tab manager.
pub(super) fn set_bridge(bridge: Bridge) {
    let _ = BRIDGE.set(bridge);
}
/// Test hook: sends URLs through the same delegate method AppKit uses.
pub(super) fn deliver_open_urls(urls: &[String]) {
    ensure_delegate();
    unsafe {
        let app = AsideApplication::shared_application();
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
        let delegate: *mut AnyObject = msg_send![&*AsideApplication::shared_application(), delegate];
        delegate.as_ref().is_some_and(|d| d.class() == AsideAppDelegate::class())
    }
}
/// Chromium may replace the delegate while starting. Put ours back so links from
/// other apps still arrive. NSApplication holds the delegate weakly.
pub(super) fn ensure_delegate() {
    if delegate_installed() {
        return;
    }
    unsafe {
        let app = AsideApplication::shared_application();
        let delegate: Retained<AnyObject> = msg_send![AsideAppDelegate::class(), new];
        let _: () = msg_send![&*app, setDelegate: &*delegate];
        std::mem::forget(delegate);
    }
}

/// Background test instances use a private pasteboard so tests never touch the user's clipboard.
fn pasteboard() -> Retained<AnyObject> {
    unsafe {
        if background() {
            let name = NSString::from_str(&format!("dev.aside-rs.test.{}", std::process::id()));
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
/// `ASIDE_BACKGROUND_WINDOW=1` is for test instances that must never activate, take focus, or be seen.
pub fn background() -> bool {
    std::env::var_os("ASIDE_BACKGROUND_WINDOW").is_some_and(|v| v == "1")
}
pub fn setup() {
    let app = AsideApplication::shared_application();
    app.setActivationPolicy(if background() {
        NSApplicationActivationPolicy::Prohibited
    } else {
        NSApplicationActivationPolicy::Regular
    });
    ensure_delegate();
}
/// Keeps the window ordered in, so Chromium still paints, but transparent, click-through,
/// off-screen, and behind other windows.
pub fn keep_out_of_sight(handle: *mut std::ffi::c_void) {
    let view = handle as *mut AnyObject;
    if view.is_null() {
        return;
    }
    unsafe {
        let window: *mut AnyObject = msg_send![view, window];
        let Some(window) = window.as_ref() else { return };
        let _: () = msg_send![window, setAlphaValue: 0.0f64];
        let _: () = msg_send![window, setHasShadow: Bool::NO];
        let _: () = msg_send![window, setIgnoresMouseEvents: Bool::YES];
        let _: () = msg_send![window, setExcludedFromWindowsMenu: Bool::YES];
        // NSWindowCollectionBehaviorTransient | NSWindowCollectionBehaviorIgnoresCycle
        let _: () = msg_send![window, setCollectionBehavior: (1usize << 3) | (1usize << 6)];
        let origin = objc2_foundation::NSPoint::new(-30000.0, -30000.0);
        let _: () = msg_send![window, setFrameOrigin: origin];
        let _: () = msg_send![window, orderBack: std::ptr::null::<AnyObject>()];
    }
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
    let app = Submenu::with_items(
        "Aside",
        true,
        &[
            &PredefinedMenuItem::about(
                Some("About Aside"),
                Some(AboutMetadata {
                    name: Some("Aside".into()),
                    ..Default::default()
                }),
            ),
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id("make-default-browser", "Make Aside the Default Browser…", true, None),
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
