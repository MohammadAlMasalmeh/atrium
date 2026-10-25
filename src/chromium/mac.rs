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
    runtime::{AnyObject, Bool, NSObjectProtocol},
    sel, DefinedClass,
};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSEvent};
use objc2_foundation::NSString;
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
            &item("new-tab", "New Task", cmd, Code::KeyT),
            &item("command-bar", "Command Bar", cmd, Code::KeyK),
            &item("newChat", "New Task", cmd, Code::KeyN),
            &item("open-location", "Edit URL", cmd, Code::KeyL),
            &PredefinedMenuItem::separator(),
            &item("close-tab", "Close Tab", cmd, Code::KeyW),
            &item("copyUrl", "Copy URL", cmd_shift, Code::KeyC),
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
            &item("toggleSidebar", "Show/Hide Tasks", cmd, Code::KeyS),
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
