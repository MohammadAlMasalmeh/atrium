//! This CEF build leaves out H.264 and AAC, so some players fail. A page whose media
//! Chromium rejects can be reopened in a WKWebView over the tab, which plays it through
//! macOS's own decoders. WebKit keeps its own cookies, apart from Chromium's and the shell's.
use super::{Bridge, Event, Manager, Pending, Rect};
use serde_json::json;
use std::{cell::RefCell, collections::HashMap, ffi::c_void, ptr::NonNull};
use wry::{
    raw_window_handle::{AppKitWindowHandle, HandleError, HasWindowHandle, RawWindowHandle, WindowHandle},
    WebView, WebViewBuilder, WebViewExtMacOS,
};

/// Sites tailor their players to the browser, so WebKit says it is Safari.
const SAFARI: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";

struct View {
    web: WebView,
    origin: String,
}

thread_local! {
    static VIEWS: RefCell<HashMap<u64, View>> = RefCell::new(HashMap::new());
}

struct Parent(NonNull<c_void>);
impl HasWindowHandle for Parent {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let raw = RawWindowHandle::AppKit(AppKitWindowHandle::new(self.0));
        Ok(unsafe { WindowHandle::borrow_raw(raw) })
    }
}

fn create(bridge: Bridge, url: &str) -> Option<WebView> {
    let parent = Parent(NonNull::new(super::native::content()?)?);
    WebViewBuilder::new()
        .with_url(url)
        .with_user_agent(SAFARI)
        // The user asked for this page to play its video.
        .with_autoplay(true)
        .with_accept_first_mouse(true)
        .with_navigation_handler(|url| super::url::is_web_url(&url) || url == "about:blank")
        .with_new_window_req_handler(move |url, _features| {
            if super::url::is_web_url(&url) {
                bridge.send(Event::Popup(url));
            }
            wry::NewWindowResponse::Deny
        })
        .build_as_child(&parent)
        .map_err(|e| eprintln!("[media] couldn't create a WebKit view: {e}"))
        .ok()
}

/// Places the tab's WebKit view, if it has one, and says whether it did.
pub(super) fn place(tab: u64, bounds: &Rect, visible: bool) -> bool {
    VIEWS.with(|v| {
        let views = v.borrow();
        let Some(view) = views.get(&tab) else { return false };
        super::native::place(objc2::rc::Retained::as_ptr(&view.web.webview()) as *mut c_void, bounds, visible);
        true
    })
}

pub(super) fn is_open(tab: u64) -> bool {
    VIEWS.with(|v| v.borrow().contains_key(&tab))
}

/// The tab's origin left the one WebKit was opened for, or the tab closed.
pub(super) fn remove(tab: u64) -> bool {
    let view = VIEWS.with(|v| v.borrow_mut().remove(&tab));
    if let Some(view) = &view {
        super::native::detach(objc2::rc::Retained::as_ptr(&view.web.webview()) as *mut c_void);
    }
    view.is_some()
}

impl Manager {
    pub(super) fn media_unsupported(&mut self, tab: u64, origin: String, src: &str) {
        eprintln!("[media] tab {tab}: Chromium can't play {}", src.chars().take(120).collect::<String>());
        // A password offer is more urgent; the page can still be reopened from the next failure.
        if is_open(tab) || self.pw_offers.contains_key(&tab) {
            return;
        }
        self.pw_offer(tab, Some(json!({ "kind": "media", "origin": origin })));
    }
    pub(super) fn media_command(&mut self, tab: u64, open: bool) {
        if !open {
            if remove(tab) {
                self.pw_offer(tab, None);
                self.layout();
            }
            return;
        }
        let Some(url) = self.tab(tab).map(|t| t.url.clone()) else { return };
        let Some(origin) = crate::passwords::origin_of(&url) else { return };
        if is_open(tab) {
            return;
        }
        let Some(web) = create(self.bridge.clone(), &url) else { return self.toast("Couldn't open this page in WebKit") };
        VIEWS.with(|v| v.borrow_mut().insert(tab, View { web, origin: origin.clone() }));
        // The Chromium page stays behind it, quiet, for when the user switches back.
        self.isolated(tab, "document.querySelectorAll('video, audio').forEach((m) => m.pause())".into(), false, Pending::Inject);
        self.pw_offer(tab, Some(json!({ "kind": "webkit", "origin": origin })));
        self.layout();
    }
    /// The offer is a native banner over the active page, like Chrome's permission bubbles.
    pub(super) fn media_banner(&self) {
        let offer = self
            .active
            .filter(|id| !self.covered && self.pw_offers.get(id).is_some_and(|o| o["kind"] == "media"))
            .and_then(|id| {
                let url = reqwest::Url::parse(&self.tab(id)?.url).ok()?;
                Some((id, url.host_str()?.trim_start_matches("www.").to_string()))
            });
        super::banner::show(&self.bridge, offer, &self.main_rect);
    }
    /// Chromium navigated somewhere else, so the WebKit copy of the old page goes away.
    pub(super) fn media_address(&mut self, tab: u64, url: &str) {
        let stale = VIEWS.with(|v| v.borrow().get(&tab).is_some_and(|view| crate::passwords::origin_of(url).as_deref() != Some(view.origin.as_str())));
        if stale {
            self.media_command(tab, false);
        }
    }
}
