//! Native browser chrome drawn directly on the window's glass: AppKit controls that
//! replace the web shell's own, one piece at a time. Enabled with `ATRIUM_NATIVE_CHROME=1`;
//! the shell hides the controls it no longer draws (see `native-chrome` in shell.html).
//!
//! AppKit calls back into these handlers synchronously (focus changes, Enter), so state is
//! only borrowed briefly and never across a call that can re-enter.
use super::{native::FlippedView, Bridge, Event};
use objc2::{
    define_class, msg_send,
    rc::Retained,
    runtime::{Bool, NSObjectProtocol, ProtocolObject, Sel},
    sel, DefinedClass, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
    NSAccessibility, NSBezelStyle, NSButton, NSControl, NSControlSize, NSControlTextEditingDelegate, NSImage, NSLineBreakMode,
    NSProgressIndicator, NSProgressIndicatorStyle, NSResponder, NSTextField, NSTextFieldBezelStyle,
    NSTextFieldDelegate, NSTextView, NSView,
};
use objc2_foundation::{NSNotification, NSObject, NSPoint, NSRect, NSSize, NSString};
use serde_json::{json, Value as Json};
use std::cell::RefCell;

pub(super) fn enabled() -> bool {
    super::native::flag("NATIVE_CHROME")
}

/// What the toolbar shows for the active tab.
#[derive(Default, Clone, PartialEq)]
pub(super) struct Page {
    pub url: String,
    pub can_back: bool,
    pub can_forward: bool,
    pub loading: bool,
}

/// What the shell reports: sidebar shown, a full overlay over everything, a web page in view.
#[derive(Default)]
pub(super) struct ShellState {
    pub sidebar: Option<bool>,
    pub overlay: Option<bool>,
    pub web: Option<bool>,
}

struct Chrome {
    root: Retained<NSView>,
    sidebar_toggle: Retained<NSButton>,
    back: Retained<NSButton>,
    forward: Retained<NSButton>,
    reload: Retained<NSButton>,
    address: Retained<NSTextField>,
    copy: Retained<NSButton>,
    pin: Retained<NSButton>,
    spinner: Retained<NSProgressIndicator>,
    _target: Retained<Target>,
    page: Page,
    editing: bool,
    sidebar: bool,
    overlay: bool,
    web: bool,
}

thread_local! {
    static CHROME: RefCell<Option<Chrome>> = const { RefCell::new(None) };
}

fn with<T>(f: impl FnOnce(&mut Chrome) -> T) -> Option<T> {
    CHROME.with(|c| c.borrow_mut().as_mut().map(f))
}

define_class!(
    /// Shows the full address while focused and the host otherwise, like Safari.
    #[unsafe(super(NSTextField, NSControl, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    struct AddressField;

    impl AddressField {
        #[unsafe(method(becomeFirstResponder))]
        fn become_first_responder(&self) -> bool {
            let url = with(|c| {
                c.editing = true;
                show_address(c);
                c.page.url.clone()
            })
            .unwrap_or_default();
            self.setStringValue(&NSString::from_str(if url == "about:blank" { "" } else { &url }));
            let ok: bool = unsafe { msg_send![super(self), becomeFirstResponder] };
            if let Some(editor) = self.currentEditor() {
                unsafe { editor.selectAll(None) };
            }
            ok
        }
    }
);

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Bridge]
    struct Target;

    impl Target {
        #[unsafe(method(goBack:))]
        fn go_back(&self, _sender: &NSButton) {
            self.send(json!({"cmd": "back"}));
        }
        #[unsafe(method(goForward:))]
        fn go_forward(&self, _sender: &NSButton) {
            self.send(json!({"cmd": "forward"}));
        }
        #[unsafe(method(reloadPage:))]
        fn reload_page(&self, _sender: &NSButton) {
            self.send(json!({"cmd": "reload"}));
        }
        #[unsafe(method(toggleSidebar:))]
        fn toggle_sidebar(&self, _sender: &NSButton) {
            self.send(json!({"cmd": "shellCommand", "name": "toggleSidebar"}));
        }
        #[unsafe(method(copyAddress:))]
        fn copy_address(&self, _sender: &NSButton) {
            self.send(json!({"cmd": "shellCommand", "name": "copyUrl"}));
        }
        #[unsafe(method(pinPage:))]
        fn pin_page(&self, _sender: &NSButton) {
            self.send(json!({"cmd": "shellCommand", "name": "bookmark"}));
        }
        #[unsafe(method(submitAddress:))]
        fn submit_address(&self, field: &NSTextField) {
            let text = field.stringValue().to_string();
            if text.trim().is_empty() {
                return;
            }
            self.send(json!({"cmd": "addressBar", "text": text.trim()}));
            stop_editing(field);
        }
    }

    unsafe impl NSObjectProtocol for Target {}

    unsafe impl NSControlTextEditingDelegate for Target {
        #[unsafe(method(controlTextDidEndEditing:))]
        fn did_end(&self, _note: &NSNotification) {
            with(|c| {
                c.editing = false;
                show_address(c);
            });
        }
        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn do_command(&self, control: &NSControl, _view: &NSTextView, command: Sel) -> Bool {
            if command == sel!(cancelOperation:) {
                stop_editing(control);
                return Bool::YES;
            }
            Bool::NO
        }
    }

    unsafe impl NSTextFieldDelegate for Target {}
);

impl Target {
    fn send(&self, command: Json) {
        self.ivars().send(Event::Command(command));
    }
}

/// Gives focus back and leaves the field showing the page's host.
fn stop_editing(field: &NSControl) {
    if let Some(window) = field.window() {
        window.makeFirstResponder(None);
    }
    with(|c| {
        c.editing = false;
        show_address(c);
    });
}

fn host(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_string()))
        .unwrap_or_default()
}

fn show_address(c: &Chrome) {
    let shown = if c.web { host(&c.page.url) } else { String::new() };
    if !c.editing {
        c.address.setStringValue(&NSString::from_str(&shown));
    }
    let actions = c.web && !shown.is_empty() && !c.editing;
    c.copy.setHidden(!actions);
    c.pin.setHidden(!actions);
}

fn symbol_button(symbol: &str, label: &str, action: Sel, target: &Target, mtm: MainThreadMarker) -> Retained<NSButton> {
    let label = NSString::from_str(label);
    let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(symbol), Some(&label))
        .expect("SF Symbol");
    let button = unsafe { NSButton::buttonWithImage_target_action(&image, Some(target), Some(action), mtm) };
    button.setBezelStyle(NSBezelStyle::Toolbar);
    button.setShowsBorderOnlyWhileMouseInside(true);
    button.setToolTip(Some(&label));
    button
}

fn frame(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

/// Builds the controls in the shell's sidebar header: 232pt wide, starting 10pt in.
pub(super) fn install(parent: &NSView, bridge: Bridge) {
    let mtm = MainThreadMarker::from(parent);
    let target: Retained<Target> = unsafe { msg_send![super(Target::alloc(mtm).set_ivars(bridge)), init] };
    let root: Retained<FlippedView> =
        unsafe { msg_send![FlippedView::alloc(mtm), initWithFrame: frame(10.0, 0.0, 232.0, 80.0)] };
    let root = Retained::into_super(root);

    let buttons = [
        ("sidebar.left", "Hide Sidebar (⌘S)", sel!(toggleSidebar:)),
        ("chevron.left", "Back (⌘[)", sel!(goBack:)),
        ("chevron.right", "Forward (⌘])", sel!(goForward:)),
        ("arrow.clockwise", "Reload (⌘R)", sel!(reloadPage:)),
    ]
    .map(|(symbol, label, action)| symbol_button(symbol, label, action, &target, mtm));
    for (i, b) in buttons.iter().enumerate() {
        b.setFrame(frame(120.0 + i as f64 * 28.0, 7.0, 28.0, 26.0));
        root.addSubview(b);
    }
    let [sidebar_toggle, back, forward, reload] = buttons;

    let spinner = NSProgressIndicator::new(mtm);
    spinner.setStyle(NSProgressIndicatorStyle::Spinning);
    spinner.setControlSize(NSControlSize::Small);
    spinner.setDisplayedWhenStopped(false);
    spinner.setFrame(frame(98.0, 12.0, 16.0, 16.0));
    root.addSubview(&spinner);

    let address: Retained<AddressField> =
        unsafe { msg_send![AddressField::alloc(mtm), initWithFrame: frame(0.0, 42.0, 232.0, 30.0)] };
    let address = Retained::into_super(address);
    address.setBezelStyle(NSTextFieldBezelStyle::RoundedBezel);
    address.setControlSize(NSControlSize::Large);
    address.setPlaceholderString(Some(&NSString::from_str("Search or enter address")));
    address.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    address.setUsesSingleLineMode(true);
    address.setAccessibilityLabel(Some(&NSString::from_str("Address and search")));
    unsafe {
        address.setTarget(Some(&target));
        address.setAction(Some(sel!(submitAddress:)));
        address.setDelegate(Some(ProtocolObject::from_ref(&*target)));
    }
    // A bezeled field draws its text at the top, so size it to its natural height and
    // centre that in the shell's 34pt address row (y 42-76).
    let height = address.intrinsicContentSize().height.max(22.0);
    address.setFrame(frame(0.0, 59.0 - height / 2.0, 232.0, height));
    root.addSubview(&address);

    let [copy, pin] = [
        ("link", "Copy Link (⇧⌘C)", sel!(copyAddress:)),
        ("pin", "Pin to This Space (⌘D)", sel!(pinPage:)),
    ]
    .map(|(symbol, label, action)| {
        let b = symbol_button(symbol, label, action, &target, mtm);
        b.setBezelStyle(NSBezelStyle::AccessoryBarAction);
        b.setControlSize(NSControlSize::Small);
        b
    });
    copy.setFrame(frame(178.0, 48.0, 24.0, 22.0));
    pin.setFrame(frame(204.0, 48.0, 24.0, 22.0));
    root.addSubview(&copy);
    root.addSubview(&pin);

    parent.addSubview(&root);
    CHROME.with(|c| {
        *c.borrow_mut() = Some(Chrome {
            root,
            sidebar_toggle,
            back,
            forward,
            reload,
            address,
            copy,
            pin,
            spinner,
            _target: target,
            page: Page::default(),
            editing: false,
            sidebar: true,
            overlay: false,
            web: false,
        })
    });
    update(Page::default());
}

pub(super) fn update(page: Page) {
    with(|c| {
        c.back.setEnabled(page.can_back);
        c.forward.setEnabled(page.can_forward);
        c.reload.setEnabled(!host(&page.url).is_empty());
        if page.loading {
            unsafe { c.spinner.startAnimation(None) };
        } else {
            unsafe { c.spinner.stopAnimation(None) };
        }
        c.page = page;
        show_address(c);
    });
}

pub(super) fn shell_state(s: ShellState) {
    let visible = with(|c| {
        c.sidebar = s.sidebar.unwrap_or(c.sidebar);
        c.overlay = s.overlay.unwrap_or(c.overlay);
        c.web = s.web.unwrap_or(c.web);
        c.root.setHidden(!c.sidebar || c.overlay);
        show_address(c);
        !c.root.isHidden()
    });
    if let Some(visible) = visible {
        super::sidebar::set_visible(visible);
    }
}

/// Whether Enter in the address field should load in the current tab rather than a new one.
pub(super) fn on_web() -> bool {
    with(|c| c.web).unwrap_or(false)
}

/// Test hook: focuses the real address field, types `text` and sends its Enter action,
/// or clicks one of the toolbar buttons by name.
pub(super) fn debug_action(action: &str, text: &str) -> Result<(), String> {
    let Some((address, button)) = with(|c| {
        let button = match action {
            "sidebar" => Some(c.sidebar_toggle.clone()),
            "back" => Some(c.back.clone()),
            "forward" => Some(c.forward.clone()),
            "reload" => Some(c.reload.clone()),
            "copy" => Some(c.copy.clone()),
            "pin" => Some(c.pin.clone()),
            _ => None,
        };
        (c.address.clone(), button)
    }) else {
        return Err("native chrome is off".into());
    };
    if let Some(button) = button {
        if !button.isEnabled() || button.isHidden() {
            return Err(format!("{action} is not clickable"));
        }
        unsafe { button.performClick(None) };
        return Ok(());
    }
    if action != "address" {
        return Err(format!("unknown chrome action {action}"));
    }
    let window = address.window().ok_or("address field has no window")?;
    if !window.makeFirstResponder(Some(&address)) {
        return Err("address field refused focus".into());
    }
    address.setStringValue(&NSString::from_str(text));
    let sent = unsafe { address.sendAction_to(address.action(), address.target().as_deref()) };
    if sent {
        Ok(())
    } else {
        Err("address field action was not sent".into())
    }
}

/// Reported in the memory/debug state so tests can check the controls without seeing them.
pub(super) fn describe() -> Option<Json> {
    with(|c| {
        json!({
            "visible": !c.root.isHidden(),
            "address": c.address.stringValue().to_string(),
            "editing": c.editing,
            "web": c.web,
            "back_enabled": c.back.isEnabled(),
            "forward_enabled": c.forward.isEnabled(),
            "reload_enabled": c.reload.isEnabled(),
            "actions_visible": !c.copy.isHidden(),
            "loading": c.page.loading,
        })
    })
}
