//! CEF requires NSApplication's event-dispatch protocols on macOS.
//! Protocol pattern adapted from cef-rs cefsimple (MIT / Apache-2.0).
use super::{Bridge, Event};
use cef::application_mac::{CefAppProtocol, CrAppControlProtocol, CrAppProtocol};
use muda::{
    accelerator::{Accelerator, Code, Modifiers},
    AboutMetadata, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu,
};
use objc2::{
    define_class, extern_methods, msg_send,
    rc::Retained,
    runtime::{AnyObject, Bool, NSObjectProtocol},
    DefinedClass,
};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSEvent};
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
pub fn setup() {
    let app = AsideApplication::shared_application();
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
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
