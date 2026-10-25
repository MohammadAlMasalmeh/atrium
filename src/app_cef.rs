//! Native Chromium host. CEF owns renderers; Rust owns tab lifecycle and the agent.
use anyhow::{anyhow, Result};
use cef::*;
use serde::Serialize;
use serde_json::{json, Value as Json};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex, Weak,
    },
    time::{Duration, Instant},
};
use tokio::sync::oneshot;
#[path = "chromium/chrome.rs"]
mod chrome;
#[path = "chromium/mac.rs"]
mod mac;
#[path = "chromium/native.rs"]
mod native;
#[path = "chromium/sidebar.rs"]
mod sidebar;
#[path = "chromium/shell_ipc.rs"]
mod shell_ipc;
#[path = "chromium/url.rs"]
mod url;
#[path = "chromium/commands.rs"]
mod commands;
pub use url::normalize_url;

const LIFECYCLE_JS: &str = include_str!("../assets/lifecycle.js");
const PAGE_JS: &str = include_str!("../assets/page.js");
const PASSWORDS_JS: &str = include_str!("../assets/passwords.js");
/// Isolated world for password forms; page scripts cannot reach its globals or binding.
const PW_WORLD: &str = "atrium-passwords";
const PW_BINDING: &str = "__atriumPasswords";
pub const SHELL: u64 = u64::MAX - 1;
const GLANCE: u64 = u64::MAX - 2;
/// CEF identifier of the shell's browser; process messages from any other browser are ignored.
static SHELL_MESSAGES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static HTTP_COMMANDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static REJECTED_MESSAGES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static FROZEN: std::sync::LazyLock<Mutex<HashMap<String, Vec<u8>>>> =
    std::sync::LazyLock::new(Default::default);
pub type Reply<T> = oneshot::Sender<T>;
#[derive(Debug, Clone, Serialize)]
pub struct TabInfo {
    pub id: u64,
    pub kind: &'static str,
    pub title: String,
    pub url: String,
    pub loading: bool,
    pub icon: Option<String>,
    pub suspended: bool,
    /// The renderer died; the tab reloads when selected, reloaded, or navigated.
    pub crashed: bool,
}
#[derive(Debug, Clone, Serialize)]
pub struct TabsState {
    pub tabs: Vec<TabInfo>,
    pub active: Option<u64>,
}
pub enum Op {
    Tabs(Reply<TabsState>),
    NewTab {
        url: Option<String>,
        background: bool,
        scratch: bool,
        reply: Reply<u64>,
    },
    Close {
        id: u64,
        reply: Reply<()>,
    },
    Navigate {
        tab: Option<u64>,
        url: String,
        reply: Reply<u64>,
    },
    History {
        tab: Option<u64>,
        action: &'static str,
        reply: Reply<Result<(), String>>,
    },
    Eval {
        tab: Option<u64>,
        js: String,
        reply: Reply<Result<Json, String>>,
    },
    /// Like `Eval`, for an agent action that should count as user activation.
    Act {
        tab: Option<u64>,
        js: String,
        reply: Reply<Result<Json, String>>,
    },
    InspectGoogle {
        tab: u64,
        reply: Reply<Result<Json, String>>,
    },
    Screenshot {
        tab: Option<u64>,
        reply: Reply<Result<String, String>>,
    },
    Shell {
        body: Json,
        reply: Reply<()>,
    },
    Memory(Reply<Json>),
    WindowShot(Reply<Result<Vec<u8>, String>>),
    /// Adds sleeping tabs for (url, title) pairs not already open; replies (added, already open).
    Restore {
        tabs: Vec<(String, String)>,
        reply: Reply<(usize, usize)>,
    },
    /// Replies with whether CEF accepted each cookie, in order.
    SetCookies {
        cookies: Vec<CookieWrite>,
        reply: Reply<Vec<bool>>,
    },
    Cookies(Reply<Vec<CookieStored>>),
    /// A DevTools method on a tab, or on the shell with `SHELL`; the raw result is returned.
    Cdp {
        tab: Option<u64>,
        method: String,
        params: Json,
        reply: Reply<Result<Json, String>>,
    },
    /// Accessibility state; optionally simulates an assistive client and reads the native tree.
    Accessibility {
        assistive: Option<bool>,
        tree: bool,
        reply: Reply<Json>,
    },
    PasswordOffers(Reply<Json>),
    /// Find, zoom, printing, downloads, and other browser commands (`/api/browser/action`).
    Action {
        body: Json,
        reply: Reply<Result<Json, String>>,
    },
    /// A DevTools method on a live web tab, for measurements; it never wakes a sleeping tab.
    DevTools {
        tab: Option<u64>,
        method: String,
        params: Json,
        reply: Reply<Result<Json, String>>,
    },
}
use crate::migration::cookies::{Stored as CookieStored, Write as CookieWrite};
#[derive(Clone)]
pub struct Handle(Bridge);
impl Handle {
    /// Opens pages from the command line or another launch once the window exists.
    pub fn open_urls(&self, urls: Vec<String>) {
        if !urls.is_empty() {
            self.0.send(Event::Commands(commands::Ev::OpenUrls(urls)));
        }
    }
    pub async fn op<T>(&self, make: impl FnOnce(Reply<T>) -> Op) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.0.send(Event::Operation(make(tx)));
        tokio::time::timeout(Duration::from_secs(30), rx)
            .await
            .map_err(|_| anyhow!("Chromium did not respond in time"))?
            .map_err(|_| anyhow!("The browser closed before answering"))
    }
}
#[derive(Clone)]
struct Bridge {
    tx: mpsc::Sender<Event>,
    manager: Weak<Mutex<Manager>>,
    queued: Arc<AtomicBool>,
}
impl Bridge {
    fn send(&self, event: Event) {
        if self.tx.send(event).is_ok() {
            self.wake();
        }
    }
    fn wake(&self) {
        if self.queued.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut task = Dispatch::new(self.clone());
        if post_task(ThreadId::UI, Some(&mut task)) == 0 {
            self.queued.store(false, Ordering::Release);
        }
    }
}
wrap_task! {
    struct Dispatch { bridge: Bridge }
    impl Task {
        fn execute(&self) {
            self.bridge.queued.store(false, Ordering::Release);
            if let Some(manager) = self.bridge.manager.upgrade() {
                let mut manager = manager.lock().unwrap();
                while let Ok(event) = manager.rx.try_recv() { manager.event(event); }
            }
        }
    }
}
enum Event {
    Init,
    Resize,
    Operation(Op),
    Created(u64, Browser),
    Detach(u64),
    Closed(u64),
    Crashed(u64),
    Title(u64, String),
    Address(u64, String),
    Loading(u64, bool),
    /// The main frame started loading a document (true) or failed to (false).
    Document(u64, bool),
    Loaded(u64),
    Capture(u64, bool),
    Download(u64, u32, bool),
    Popup(String),
    PopupTab {
        id: u64,
        opener: Option<u64>,
        url: String,
        foreground: bool,
    },
    Adopt(Browser),
    Command(Json),
    /// A favicon the native sidebar asked for: its URL and the image bytes.
    Icon(String, Vec<u8>),
    Quit,
    DevResult(i32, bool, Json),
    Binding(u64, Json),
    Password(PwEvent),
    Maintenance,
    Assistive(String, bool),
    TakeFocus(u64, bool),
    Favicon(u64, String),
    FaviconSaved(u64, String),
    Commands(commands::Ev),
}
/// Results of password store work done off the UI thread.
enum PwEvent {
    Found { tab: u64, ctx: i64, login: bool, signup: bool, accounts: Vec<crate::passwords::Login> },
    Submitted { tab: u64, origin: String, username: String, password: String, status: crate::passwords::Saved },
    Fill { tab: u64, ctx: i64, login: crate::passwords::Login },
    Done { message: String },
}
#[derive(Clone)]
struct PwForms {
    ctx: i64,
    origin: String,
    login: bool,
    signup: bool,
}
struct PwPending {
    origin: String,
    username: String,
    password: String,
}
enum PwStep {
    Tree,
    World,
    Done,
    Fill(&'static str),
}
struct Tab {
    id: u64,
    title: String,
    url: String,
    loading: bool,
    /// The main document is loading. `loading` also covers subframes, and ad-heavy
    /// pages keep adding iframes long after their own load, so it can stay true for good.
    document_loading: bool,
    icon: Option<String>,
    scratch: bool,
    /// A Chromium browser exists or is being created for this tab.
    live: bool,
    /// The browser's view has left the window and may already be freed; never touch it.
    detached: bool,
    browser: Option<Browser>,
    registration: Option<Registration>,
    last_used: Instant,
    capture: bool,
    page_protected_until: Instant,
    agent_until: Instant,
    suspended: bool,
    suspending: bool,
    crashed: bool,
    /// The tab that opened this one as a `window.open` popup, while both exist.
    opener: Option<u64>,
    /// A popup whose browser Chromium is still creating; no other browser must be made for it.
    opening: bool,
    /// History from earlier views of this tab, outside the live view's own history:
    /// oldest first, and nearest first.
    back: Vec<String>,
    forward: Vec<String>,
    /// Document scroll offset to apply when the released page loads again.
    scroll: Option<(f64, f64)>,
    /// The view is being replaced to step into `back` or `forward`.
    reopen: bool,
    downloads: std::collections::HashSet<u32>,
}
impl Tab {
    fn new(id: u64, url: String, title: String) -> Self {
        let now = Instant::now();
        Tab {
            id,
            title,
            url,
            loading: false,
            document_loading: false,
            icon: None,
            scratch: false,
            live: false,
            detached: false,
            browser: None,
            registration: None,
            last_used: now,
            capture: false,
            page_protected_until: now,
            agent_until: now,
            suspended: false,
            suspending: false,
            crashed: false,
            opener: None,
            opening: false,
            back: Vec::new(),
            forward: Vec::new(),
            scroll: None,
            reopen: false,
            downloads: Default::default(),
        }
    }
    fn info(&self) -> TabInfo {
        TabInfo {
            id: self.id,
            kind: if self.scratch {
                "scratch"
            } else if self.url == "about:blank" {
                "newtab"
            } else {
                "web"
            },
            title: self.title.clone(),
            url: self.url.clone(),
            loading: self.loading,
            icon: self.icon.clone(),
            suspended: self.suspended,
            crashed: self.crashed,
        }
    }
}
enum Pending {
    Eval(Reply<Result<Json, String>>),
    Screenshot(Reply<Result<String, String>>),
    Frozen(&'static str),
    Probe(u64),
    Raw(Reply<Result<Json, String>>),
    Unload(u64, (f64, f64)),
    Inject,
    /// Steps toward running `expression` in the tab's isolated world, then `then`.
    Frame { expression: String, gesture: bool, then: Box<Pending> },
    World { expression: String, gesture: bool, then: Box<Pending> },
    Pw(u64, PwStep),
}
/// DevTools isolated world for injected scripts, so pages never see their globals.
const WORLD: &str = "atrium";
struct Manager {
    rx: mpsc::Receiver<Event>,
    bridge: Bridge,
    base: String,
    tabs: Vec<Tab>,
    active: Option<u64>,
    split: Option<u64>,
    main_rect: Rect,
    split_rect: Option<Rect>,
    glance_rect: Option<Rect>,
    covered: bool,
    /// The shell document has loaded and reported `ready`.
    shell_loaded: bool,
    policy: crate::memory::Policy,
    session_dirty: bool,
    next_id: u64,
    next_call: i32,
    pending: HashMap<i32, (Instant, u64, Pending)>,
    adopted: std::collections::HashSet<i32>,
    closing: Arc<AtomicBool>,
    voiceover: bool,
    assistive: std::collections::BTreeSet<String>,
    accessibility: bool,
    focus_page_until: Option<Instant>,
    pw_forms: HashMap<u64, PwForms>,
    pw_pending: HashMap<u64, PwPending>,
    pw_offers: HashMap<u64, Json>,
    cmds: commands::State,
}
impl Manager {
    fn tab(&self, id: u64) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.id == id)
    }
    fn tab_mut(&mut self, id: u64) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }
    fn state(&self) -> TabsState {
        TabsState {
            tabs: self
                .tabs
                .iter()
                .filter(|t| t.id != SHELL && t.id != GLANCE)
                .map(Tab::info)
                .collect(),
            active: self.active,
        }
    }
    fn push(&self, reveal: bool) {
        self.sync_chrome();
        let mut state = serde_json::to_value(self.state()).unwrap();
        state["split"] = json!(self.split);
        state["reveal"] = json!(reveal);
        state["glance"] = self
            .tab(GLANCE)
            .map(|t| json!({ "title": t.title, "url": t.url, "loading": t.loading }))
            .unwrap_or(Json::Null);
        self.shell(&format!("window.atriumShell?.state({state})"));
    }
    fn sync_chrome(&self) {
        let Some(t) = self.active.and_then(|id| self.tab(id)) else {
            return chrome::update(chrome::Page::default());
        };
        let browser = t.browser.as_ref();
        chrome::update(chrome::Page {
            url: t.url.clone(),
            can_back: browser.is_some_and(|b| b.can_go_back() == 1),
            can_forward: browser.is_some_and(|b| b.can_go_forward() == 1),
            loading: t.loading,
        });
    }
    fn shell(&self, js: &str) {
        native::shell_eval(js);
    }
    fn cmd(&self, command: &str) {
        self.shell(&format!("window.atriumShell?.cmd({})", json!(command)));
    }
    fn create_view(&mut self, id: u64) {
        let Some(parent) = native::content() else { return };
        let Some(tab) = self.tab(id) else { return };
        if tab.live || tab.opening || tab.url == "about:blank" {
            return;
        }
        let mut client = HostClient::new(self.bridge.clone(), id, self.base.clone(), Default::default());
        // Hidden until layout() places it, so it never flashes at the wrong spot.
        let info = WindowInfo {
            parent_view: parent,
            bounds: self.main_rect.clone(),
            hidden: 1,
            runtime_style: RuntimeStyle::ALLOY,
            ..Default::default()
        };
        let created = browser_host_create_browser(
            Some(&info),
            Some(&mut client),
            Some(&CefString::from(tab.url.as_str())),
            Some(&BrowserSettings::default()),
            None,
            None,
        );
        if created != 0 {
            if let Some(tab) = self.tab_mut(id) {
                tab.live = true;
                tab.detached = false;
                tab.suspended = false;
                tab.crashed = false;
                tab.loading = true;
                tab.document_loading = true;
            }
        }
    }
    fn new_tab(&mut self, url: Option<String>, foreground: bool, scratch: bool) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        let url = url
            .map(|u| normalize_url(&u))
            .unwrap_or_else(|| "about:blank".into());
        self.tabs.push(Tab {
            scratch,
            ..Tab::new(id, url, String::new())
        });
        if foreground || self.active.is_none() {
            self.active = Some(id);
        }
        self.session_dirty = true;
        self.create_view(id);
        self.layout();
        self.push(foreground);
        id
    }
    fn activate(&mut self, id: u64) {
        if self.tab(id).is_none() {
            return;
        }
        self.session_dirty = true;
        self.active = Some(id);
        if let Some(t) = self.tab_mut(id) {
            t.last_used = Instant::now();
            if t.crashed {
                if let Some(browser) = &t.browser {
                    browser.reload();
                }
            }
        }
        self.create_view(id);
        self.layout();
        self.push(true);
        self.apply_zoom(id);
        // Agent and API tab switches must not pull the window in front of other apps.
        if let Some(host) = self.host(id) {
            focus_page(&host);
        }
    }
    fn open_about(&mut self) {
        let url = format!("{}{}", self.base, crate::about::PATH);
        match self.tabs.iter().find(|t| t.url == url).map(|t| t.id) {
            Some(id) => self.activate(id),
            None => {
                self.new_tab(Some(url), true, false);
            }
        }
    }
    fn close(&mut self, id: u64) {
        if id == SHELL {
            return;
        }
        self.remember_closed(id);
        if let Some(host) = self
            .tab(id)
            .and_then(|t| t.browser.as_ref())
            .and_then(|b| b.host())
        {
            host.close_browser(0);
        } else {
            self.remove_tab(id);
        }
    }
    fn remove_tab(&mut self, id: u64) {
        let opener = self.tab(id).and_then(|t| t.opener);
        self.tabs.retain(|t| t.id != id);
        for t in &mut self.tabs {
            if t.opener == Some(id) {
                t.opener = None;
            }
        }
        if self.active == Some(id) {
            self.active = opener.filter(|o| self.tab(*o).is_some()).or_else(|| {
                self.tabs
                    .iter()
                    .find(|t| t.id != SHELL && t.id != GLANCE && !t.scratch)
                    .map(|t| t.id)
            });
        }
        if self.split == Some(id) {
            self.split = None;
        }
        self.pending.retain(|_, (_, tab, _)| *tab != id);
        self.pw_forms.remove(&id);
        self.pw_pending.remove(&id);
        if self.pw_offers.remove(&id).is_some() {
            self.shell(&format!("window.atriumShell?.password?.({id}, null)"));
        }
        self.session_dirty = true;
        if let Some(id) = self.active {
            self.create_view(id);
        }
        self.layout();
        self.push(false);
    }
    fn navigate(&mut self, tab: Option<u64>, raw: String) -> u64 {
        let id = tab
            .filter(|id| self.tab(*id).is_some())
            .unwrap_or_else(|| self.new_tab(None, true, false));
        let url = normalize_url(&raw);
        if let Some(t) = self.tab_mut(id) {
            if t.browser.is_none() && is_web(&t.url) {
                t.back.push(std::mem::replace(&mut t.url, url.clone()));
                t.forward.clear();
            }
            t.url = url.clone();
            t.loading = true;
            t.document_loading = true;
            t.scroll = None;
            t.last_used = Instant::now();
            if let Some(frame) = t.browser.as_ref().and_then(|b| b.main_frame()) {
                frame.load_url(Some(&CefString::from(url.as_str())));
            }
        }
        self.session_dirty = true;
        self.create_view(id);
        self.layout();
        self.push(false);
        id
    }
    fn layout(&self) {
        let (width, _) = native::size().unwrap_or((1200, 800));
        for t in self.tabs.iter().filter(|t| !t.detached) {
            let Some(host) = t.browser.as_ref().and_then(|b| b.host()) else { continue };
            let (bounds, visible) = if t.id == GLANCE {
                (
                    self.glance_rect
                        .clone()
                        .unwrap_or_else(|| self.main_rect.clone()),
                    self.glance_rect.is_some(),
                )
            } else if Some(t.id) == self.active {
                (self.main_rect.clone(), !self.covered && !t.scratch)
            } else if Some(t.id) == self.split {
                (
                    self.split_rect
                        .clone()
                        .unwrap_or_else(|| self.main_rect.clone()),
                    !self.covered && self.split_rect.is_some(),
                )
            } else {
                (self.main_rect.clone(), false)
            };
            // Chromium only paints visible views, so a hidden tab being captured stays
            // visible but outside the window instead of covering the user's page.
            let (bounds, visible) = if !visible && self.capturing(t.id) {
                (
                    Rect {
                        x: width + 64,
                        ..self.main_rect.clone()
                    },
                    true,
                )
            } else {
                (bounds, visible)
            };
            native::place(host.window_handle(), &bounds, visible);
        }
    }
    fn update_accessibility(&mut self) {
        let on = std::env::var_os("ATRIUM_FORCE_ACCESSIBILITY").is_some_and(|v| v == "1")
            || self.voiceover
            || !self.assistive.is_empty();
        if on == self.accessibility {
            return;
        }
        self.accessibility = on;
        for host in self.tabs.iter().filter_map(|t| t.browser.as_ref()?.host()) {
            host.set_accessibility_state(if on { State::ENABLED } else { State::DISABLED });
        }
    }
    fn accessibility_state(&self) -> Json {
        let focused = self
            .tabs
            .iter()
            .filter(|t| !t.detached)
            .find(|t| t.browser.as_ref().and_then(|b| b.host()).is_some_and(|h| native::has_focus(h.window_handle())))
            .map(|t| match t.id {
                GLANCE => json!("glance"),
                id => json!(id),
            })
            .or_else(|| native::shell_focused().then(|| json!("shell")));
        json!({
            "enabled": self.accessibility,
            "voiceover": self.voiceover,
            "assistive_clients": self.assistive,
            "focused_view": focused,
        })
    }
    /// A tab's live browser host, unless its view has already left the window.
    fn host(&self, id: u64) -> Option<BrowserHost> {
        self.tab(id).filter(|t| !t.detached)?.browser.as_ref()?.host()
    }
    fn page_visible(&self) -> Option<BrowserHost> {
        let tab = self.tab(self.active?)?;
        if self.covered || tab.scratch {
            return None;
        }
        self.host(tab.id)
    }
    /// Native focus moves between the shell's WKWebView and Chromium's views; the shell
    /// then places DOM focus with `cmd`.
    fn focus_shell(&self, cmd: &str) {
        native::focus_shell();
        self.cmd(cmd);
    }
    fn take_focus(&self, id: u64, next: bool) {
        match id {
            SHELL => match self.page_visible() {
                Some(host) => focus_page(&host),
                None => self.focus_shell(if next { "focusFirst" } else { "focusLast" }),
            },
            GLANCE => self.focus_shell(if next { "focusGlance" } else { "focusGlanceEnd" }),
            _ => self.focus_shell(if next { "focusAfterPage" } else { "focusBeforePage" }),
        }
    }
    fn capturing(&self, tab: u64) -> bool {
        self.pending.values().any(|(_, id, pending)| {
            *id == tab && matches!(pending, Pending::Screenshot(_) | Pending::Frozen(_))
        })
    }
    fn cdp(&mut self, tab: u64, method: &str, params: Json, pending: Pending) {
        let crashed = self.tab(tab).is_some_and(|t| t.crashed);
        if !crashed {
            self.create_view(tab);
        }
        let Some(host) = self
            .tab(tab)
            .filter(|t| !t.crashed)
            .and_then(|t| t.browser.as_ref())
            .and_then(|b| b.host())
        else {
            const CRASHED: &str = "The page crashed. Reload it or select the tab to recover.";
            let message = if crashed {
                CRASHED
            } else if matches!(pending, Pending::Screenshot(_)) {
                "The tab is not ready to capture"
            } else {
                "The tab is still loading. Try again in a moment."
            };
            self.fail(pending, message.into());
            return;
        };
        let capture = matches!(&pending, Pending::Screenshot(_) | Pending::Frozen(_));
        self.next_call += 1;
        let call = self.next_call;
        let mut dict = parse_json(
            Some(&CefString::from(params.to_string().as_str())),
            JsonParserOptions::RFC,
        )
        .and_then(|v| v.dictionary());
        self.pending.insert(call, (Instant::now(), tab, pending));
        if capture {
            self.layout();
        }
        if host.execute_dev_tools_method(call, Some(&CefString::from(method)), dict.as_mut()) == 0 {
            self.finish(
                call,
                false,
                json!({ "message": "Chromium could not execute this browser operation" }),
            );
        }
    }
    fn evaluate(&mut self, tab: u64, js: String, reply: Reply<Result<Json, String>>, gesture: bool) {
        if tab == SHELL && !crate::shell_eval_enabled() {
            let _ = reply.send(Err("The browser shell cannot be scripted".into()));
            return;
        }
        if tab != SHELL {
            if let Some(t) = self.tab_mut(tab) {
                t.agent_until = Instant::now() + Duration::from_secs(180);
            }
        }
        let expression = format!("{PAGE_JS}\n(async () => {{ {js}\n }})()");
        // Agent clicks count as user activation, so links they open become tabs. Reads
        // don't, or a page could use the agent's background reads to open popups.
        if tab == SHELL {
            self.cdp(
                tab,
                "Runtime.evaluate",
                json!({ "expression": expression, "awaitPromise": true, "returnByValue": true, "userGesture": gesture }),
                Pending::Eval(reply),
            );
        } else {
            self.isolated(tab, expression, gesture, Pending::Eval(reply));
        }
    }
    /// Runs `expression` in the main frame's isolated world. Its globals persist for
    /// the document's lifetime and are invisible to the page's own scripts.
    fn isolated(&mut self, tab: u64, expression: String, gesture: bool, then: Pending) {
        self.cdp(
            tab,
            "Page.getFrameTree",
            json!({}),
            Pending::Frame { expression, gesture, then: Box::new(then) },
        );
    }
    fn fail(&mut self, pending: Pending, message: String) {
        match pending {
            Pending::Eval(reply) => {
                let _ = reply.send(Err(message));
            }
            Pending::Screenshot(reply) => {
                let _ = reply.send(Err(message));
            }
            Pending::Probe(id) | Pending::Unload(id, _) => {
                if let Some(tab) = self.tab_mut(id) {
                    tab.page_protected_until = Instant::now() + Duration::from_secs(180);
                }
            }
            Pending::Frame { then, .. } | Pending::World { then, .. } => self.fail(*then, message),
            Pending::Raw(reply) => {
                let _ = reply.send(Err(message));
            }
            Pending::Frozen(_) | Pending::Inject | Pending::Pw(..) => {}
        }
    }
    fn finish(&mut self, id: i32, success: bool, data: Json) {
        let Some((_, tab_id, pending)) = self.pending.remove(&id) else {
            return;
        };
        let error = if !success {
            Some(
                data["message"]
                    .as_str()
                    .unwrap_or("Chromium operation failed")
                    .to_string(),
            )
        } else if !data["exceptionDetails"].is_null() {
            Some(
                data["exceptionDetails"]["exception"]["description"]
                    .as_str()
                    .or(data["exceptionDetails"]["text"].as_str())
                    .unwrap_or("Page script failed")
                    .to_string(),
            )
        } else {
            None
        };
        if matches!(&pending, Pending::Screenshot(_) | Pending::Frozen(_)) {
            self.layout();
        }
        match pending {
            Pending::Raw(reply) => {
                let _ = reply.send(if success {
                    Ok(data)
                } else {
                    Err(error.unwrap_or_default())
                });
            }
            Pending::Eval(reply) => {
                let _ = reply.send(error.map_or_else(|| Ok(data["result"]["value"].clone()), Err));
            }
            Pending::Screenshot(reply) => {
                let _ = reply.send(error.map_or_else(
                    || {
                        data["data"]
                            .as_str()
                            .map(str::to_string)
                            .ok_or_else(|| "No screenshot data".into())
                    },
                    Err,
                ));
            }
            Pending::Frozen(slot) => {
                if error.is_none() {
                    if let Some(data) = data["data"].as_str() {
                        use base64::Engine;
                        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                            FROZEN.lock().unwrap().insert(slot.into(), bytes);
                            self.shell(&format!(
                                "window.atriumShell?.frozen({}, {})",
                                json!(slot),
                                json!(format!("/frozen/{slot}?v={}", uuid::Uuid::new_v4()))
                            ));
                        }
                    }
                }
            }
            Pending::Frame { expression, gesture, then } => {
                match (error, data["frameTree"]["frame"]["id"].as_str()) {
                    (None, Some(frame)) => self.cdp(
                        tab_id,
                        "Page.createIsolatedWorld",
                        json!({ "frameId": frame, "worldName": WORLD }),
                        Pending::World { expression, gesture, then },
                    ),
                    (error, _) => self.fail(*then, error.unwrap_or_else(|| "The page has no main frame".into())),
                }
            }
            Pending::World { expression, gesture, then } => {
                match (error, data["executionContextId"].as_i64()) {
                    (None, Some(context)) => {
                        self.cdp(
                            tab_id,
                            "Runtime.evaluate",
                            json!({ "expression": expression, "contextId": context, "awaitPromise": true, "returnByValue": true, "userGesture": gesture }),
                            *then,
                        )
                    }
                    (error, _) => self.fail(*then, error.unwrap_or_else(|| "The page is not ready for scripts".into())),
                }
            }
            Pending::Inject => {}
            Pending::Pw(tab, step) => self.pw_step(tab, step, error, data),
            Pending::Probe(id) => {
                let scroll = &data["result"]["value"];
                let scroll = error.is_none().then(|| Some((scroll[0].as_f64()?, scroll[1].as_f64()?))).flatten();
                // `onbeforeunload` is per world, so it is read statelessly in the page's own world.
                if let Some(scroll) = scroll {
                    self.cdp(
                        id,
                        "Runtime.evaluate",
                        json!({ "expression": "!window.onbeforeunload", "returnByValue": true }),
                        Pending::Unload(id, scroll),
                    );
                } else if let Some(tab) = self.tab_mut(id) {
                    tab.page_protected_until = Instant::now() + Duration::from_secs(180);
                }
            }
            Pending::Unload(id, scroll) => {
                if error.is_some() || data["result"]["value"] != true {
                    if let Some(tab) = self.tab_mut(id) {
                        tab.page_protected_until = Instant::now() + Duration::from_secs(180);
                    }
                } else if self.can_suspend(id) {
                    if let Some(tab) = self.tab_mut(id) {
                        tab.scroll = Some(scroll).filter(|s| *s != (0.0, 0.0));
                    }
                    self.suspend(id);
                }
            }
        }
    }
    /// Closes the page but keeps the tab; selecting it creates a fresh view at its URL.
    fn suspend(&mut self, id: u64) {
        let (back, forward) = self.history(id);
        if let Some(tab) = self.tab_mut(id).filter(|t| t.browser.is_some() && !t.suspending) {
            tab.back = back;
            tab.forward = forward;
        }
        self.release(id);
    }
    fn release(&mut self, id: u64) {
        if let Some(tab) = self.tab_mut(id).filter(|t| !t.suspending) {
            if let Some(host) = tab.browser.as_ref().and_then(|b| b.host()) {
                tab.suspending = true;
                host.close_browser(0);
            }
        }
    }
    /// The tab's back and forward lists, joining the live view's entries with
    /// history saved from its earlier views.
    fn history(&self, id: u64) -> (Vec<String>, Vec<String>) {
        let Some(tab) = self.tab(id) else { return Default::default() };
        let entries = Entries::default();
        if let Some(host) = tab.browser.as_ref().filter(|_| !tab.suspending).and_then(|b| b.host()) {
            host.navigation_entries(Some(&mut HistoryVisitor::new(entries.clone())), 0);
        }
        let (live, current) = std::mem::take(&mut *entries.lock().unwrap());
        let mut back = tab.back.clone();
        let mut forward = tab.forward.clone();
        if let Some(current) = (!live.is_empty()).then(|| current.min(live.len() - 1)) {
            back.extend(live[..current].iter().filter(|u| is_web(u)).cloned());
            // A view starts with one entry; more entries mean the user navigated
            // from it, which drops the saved forward list.
            if live.len() > 1 {
                forward = live[current + 1..].iter().filter(|u| is_web(u)).cloned().collect();
            }
        }
        back.drain(..back.len().saturating_sub(HISTORY));
        forward.truncate(HISTORY);
        (back, forward)
    }
    /// Goes back or forward, replacing the view when the step leaves the live view's history.
    fn go(&mut self, id: u64, back: bool) -> Result<(), String> {
        let browser = self.tab(id).ok_or("No page is open")?.browser.clone();
        if let Some(b) = browser.filter(|b| (if back { b.can_go_back() } else { b.can_go_forward() }) != 0) {
            if back { b.go_back() } else { b.go_forward() }
            return Ok(());
        }
        let (mut older, mut newer) = self.history(id);
        let target = if back { older.pop() } else { (!newer.is_empty()).then(|| newer.remove(0)) };
        let target = target.ok_or_else(|| format!("There is no page to go {}", if back { "back" } else { "forward" }))?;
        let tab = self.tab_mut(id).ok_or("No page is open")?;
        let current = std::mem::replace(&mut tab.url, target);
        if is_web(&current) {
            if back { newer.insert(0, current) } else { older.push(current) }
        }
        tab.back = older;
        tab.forward = newer;
        tab.scroll = None;
        tab.loading = true;
        tab.document_loading = true;
        tab.last_used = Instant::now();
        tab.reopen = tab.browser.is_some();
        let live = tab.reopen;
        self.session_dirty = true;
        if live {
            self.release(id);
        } else {
            self.create_view(id);
            self.layout();
        }
        self.push(false);
        Ok(())
    }
    fn crashed(&mut self, id: u64) {
        if self.closing.load(Ordering::Relaxed) {
            return;
        }
        let calls = self
            .pending
            .iter()
            .filter(|(_, (_, tab, _))| *tab == id)
            .map(|(call, _)| *call)
            .collect::<Vec<_>>();
        for call in calls {
            self.finish(call, false, json!({"message":"The page's renderer stopped"}));
        }
        let visible = Some(id) == self.active || Some(id) == self.split || id == GLANCE;
        let Some(tab) = self.tab_mut(id).filter(|t| !t.suspending) else {
            return;
        };
        // Debug URLs such as chrome://kill never commit; keep the last committed page.
        if let Some(url) = tab
            .browser
            .as_ref()
            .and_then(|b| b.main_frame())
            .map(|f| CefString::from(&f.url()).to_string())
            .filter(|u| !u.is_empty())
        {
            tab.url = url;
        }
        tab.loading = false;
        if id == SHELL {
            if let Some(browser) = &tab.browser {
                browser.reload();
            }
            return;
        }
        eprintln!("tab {id}: renderer process terminated");
        tab.crashed = true;
        if !visible {
            self.suspend(id);
        }
        self.session_dirty = true;
        self.push(false);
    }
    fn operation(&mut self, op: Op) {
        match op {
            Op::Tabs(reply) => { let _ = reply.send(self.state()); }
            Op::NewTab { url, background, scratch, reply } => { let id = self.new_tab(url, !background, scratch); let _ = reply.send(id); }
            Op::Close { id, reply } => { self.close(id); let _ = reply.send(()); }
            Op::Navigate { tab, url, reply } => { let id = self.navigate(tab.or(self.active), url); let _ = reply.send(id); }
            Op::History { tab, action, reply } => {
                let id = tab.or(self.active).filter(|id| self.tab(*id).is_some());
                let browser = id.and_then(|id| self.tab(id)?.browser.clone());
                let result = match (id, browser) {
                    (Some(id), _) if action != "reload" => self.go(id, action == "back"),
                    (_, Some(b)) => { b.reload(); Ok(()) },
                    // A released (sleeping or crashed) tab reloads with a fresh view.
                    (Some(id), None) => { self.create_view(id); self.layout(); Ok(()) },
                    _ => Err("No page is open".into())
                }; let _ = reply.send(result);
            }
            Op::Eval { tab: Some(SHELL), js, reply } => {
                if !crate::shell_eval_enabled() {
                    let _ = reply.send(Err("The browser shell cannot be scripted".into()));
                } else {
                    // WebKit can't return a Promise here, so shell evaluation is synchronous.
                    native::shell_eval_with(&format!("JSON.stringify((() => {{ {js}\n }})() ?? null)"), move |result| {
                        let value = serde_json::from_str::<Json>(&result)
                            .ok()
                            .and_then(|v| match v { Json::String(text) => serde_json::from_str(&text).ok(), other => Some(other) });
                        let _ = reply.send(value.ok_or_else(|| format!("The shell script did not return JSON: {result}")));
                    });
                }
            }
            Op::Eval { tab, js, reply } => { if let Some(tab) = tab.or(self.active) { self.evaluate(tab, js, reply, false); } else { let _ = reply.send(Err("No page is open".into())); } }
            Op::Act { tab, js, reply } => { if let Some(tab) = tab.or(self.active) { self.evaluate(tab, js, reply, true); } else { let _ = reply.send(Err("No page is open".into())); } }
            Op::InspectGoogle { tab, reply } => self.evaluate(tab, "return location.hostname === 'mail.google.com' && location.pathname.startsWith('/mail/u/') && !!document.querySelector('[gh=cm],[role=navigation]')".into(), reply, false),
            Op::Screenshot { tab, reply } => { if let Some(tab) = tab.or(self.active).filter(|id| *id != SHELL) { self.cdp(tab, "Page.captureScreenshot", json!({ "format": "jpeg", "quality": 80 }), Pending::Screenshot(reply)); } else { let _ = reply.send(Err("No page is open".into())); } }
            Op::Shell { body, reply } => { HTTP_COMMANDS.fetch_add(1, Ordering::Relaxed); self.command(body); let _ = reply.send(()); }
            Op::Restore { tabs, reply } => { let _ = reply.send(self.restore_tabs(tabs)); }
            Op::SetCookies { cookies, reply } => set_cookies(cookies, reply),
            Op::Cookies(reply) => {
                let sink = Arc::new(CookieSink { cookies: Mutex::new(Vec::new()), reply: Mutex::new(Some(reply)) });
                if let Some(manager) = cookie_manager_get_global_manager(None) {
                    manager.visit_all_cookies(Some(&mut CookieList::new(sink)));
                }
            }
            Op::Cdp { tab, method, params, reply } => match tab.or(self.active) {
                Some(tab) => self.cdp(tab, &method, params, Pending::Raw(reply)),
                None => { let _ = reply.send(Err("No page is open".into())); }
            },
            Op::Accessibility { assistive, tree, reply } => {
                if let Some(on) = assistive {
                    mac::set_application_attribute("AXManualAccessibility", on);
                }
                let mut state = self.accessibility_state();
                if tree {
                    if let Some(content) = native::content() {
                        state["native"] = mac::accessibility_tree(content, 6000);
                    }
                }
                let _ = reply.send(state);
            }
            Op::PasswordOffers(reply) => {
                let forms: serde_json::Map<String, Json> = self.pw_forms.iter().map(|(id, f)| (id.to_string(), json!({ "origin": f.origin, "login": f.login, "signup": f.signup }))).collect();
                let offers: serde_json::Map<String, Json> = self.pw_offers.iter().map(|(id, o)| (id.to_string(), o.clone())).collect();
                let _ = reply.send(json!({ "forms": forms, "offers": offers }));
            }
            Op::Action { body, reply } => self.action(body, Some(reply)),
            // Measurements must not wake a sleeping tab, so only live pages are reachable.
            Op::DevTools { tab, method, params, reply } => match tab.or(self.active).filter(|id| *id != SHELL && self.tab(*id).is_some_and(|t| t.browser.is_some())) {
                Some(tab) => self.cdp(tab, &method, params, Pending::Raw(reply)),
                None => { let _ = reply.send(Err("The tab has no live page".into())); }
            },
            Op::Memory(reply) => {
                let r = &self.main_rect;
                let _ = reply.send(json!({ "engine": "Chromium / CEF", "pid": std::process::id(), "max_live_tabs": self.policy.max_live_tabs, "idle_after_seconds": self.policy.idle_after.as_secs(), "shell_ready": native::has_shell() && self.shell_loaded, "native_chrome": chrome::describe(), "native_sidebar": sidebar::describe(), "live_renderers": self.tabs.iter().filter(|t|t.live).count(), "tabs": self.tabs.iter().map(|t| json!({"id":t.id,"live":t.live,"suspended":t.suspended,"view":t.browser.as_ref().filter(|_| !t.detached).and_then(|b| b.host()).and_then(|h| native::frame_of(h.window_handle()))})).collect::<Vec<_>>(),
                    "layout": { "main": { "x": r.x, "y": r.y, "w": r.width, "h": r.height }, "covered": self.covered, "glance": self.glance_rect.is_some() },
                    "shell_ipc": { "process_messages": SHELL_MESSAGES.load(Ordering::Relaxed), "rejected_messages": REJECTED_MESSAGES.load(Ordering::Relaxed), "http_commands": HTTP_COMMANDS.load(Ordering::Relaxed) } }));
            }
            Op::WindowShot(reply) => { let _ = reply.send(native::window_png()); }
        }
    }
    fn command(&mut self, m: Json) {
        match m["cmd"].as_str().unwrap_or("") {
            "quit" => self.event(Event::Quit),
            "shellCommand" => {
                if let Some(name) = m["name"].as_str() {
                    self.cmd(name);
                }
            }
            "menu" => {
                if let Some(name) = m["name"].as_str() {
                    match name {
                        "close-tab" => {
                            if let Some(id) = self.active {
                                self.close(id);
                            }
                        }
                        "new-tab" => {
                            self.new_tab(None, true, false);
                            native::focus_shell();
                            self.cmd("focusStart");
                        }
                        "about" => self.open_about(),
                        "open-location" => self.cmd("editUrl"),
                        "command-bar" => self.cmd("commandBar"),
                        "back" | "forward" | "reload" => self.command(json!({"cmd":name})),
                        // DevTools opens a window, which background test instances must never show.
                        "devtools" | "devtools-shell" if mac::background() => {}
                        "devtools-shell" => native::shell_devtools(),
                        "devtools" => {
                            if let Some(host) = self
                                .active
                                .and_then(|id| self.tab(id))
                                .and_then(|t| t.browser.as_ref())
                                .and_then(|b| b.host())
                            {
                                host.show_dev_tools(None, None, None, None);
                            }
                        }
                        other => {
                            if !self.named(other) {
                                self.cmd(other)
                            }
                        }
                    }
                }
            }
            "log" => eprintln!("[shell] {}", m["msg"].as_str().unwrap_or("")),
            "ready" => {
                self.shell_loaded = true;
                // Onboarding is a shell overlay, and pages under an overlay are hidden,
                // which defers permission prompts and changes page lifecycle in tests.
                // onboarding.js loads after the shell reports ready; it opens as it defines itself.
                if mac::background() {
                    self.shell("(function close() { if (window.atriumOnboarding) atriumOnboarding.close(); else setTimeout(close, 50); })()");
                }
                if self.active.is_none() {
                    self.new_tab(None, true, false);
                }
                self.push(false);
                if self.covered {
                    native::focus_shell();
                }
                for (id, offer) in &self.pw_offers {
                    self.shell(&format!("window.atriumShell?.password?.({id}, {offer})"));
                }
            }
            "passwordFill" | "passwordGenerate" | "passwordSave" | "passwordDismiss" => {
                if let Some(tab) = m["tab"].as_u64().or(self.active) {
                    self.pw_command(tab, &m);
                }
            }
            "focus" => match m["target"].as_str() {
                // A command that opens a page asks for focus before the shell uncovers it.
                Some("page") => match self.page_visible() {
                    Some(host) => {
                        focus_page(&host);
                        self.focus_page_until = None;
                    }
                    None => self.focus_page_until = Some(Instant::now() + Duration::from_secs(2)),
                },
                Some("glance") => {
                    if let Some(host) = self.host(GLANCE) {
                        focus_page(&host);
                    }
                }
                _ => self.focus_shell("focusSidebar"),
            },
            "layout" => {
                if let Some(r) = rect(&m["main"]) {
                    self.main_rect = r;
                }
                self.split_rect = rect(&m["split"]);
                self.glance_rect = rect(&m["glance"]);
                if let Some(r) = rect(&m["list"]) {
                    sidebar::place(r.x as f64, r.y as f64, r.width as f64, r.height as f64);
                }
                chrome::shell_state(chrome::ShellState { sidebar: m["sidebar"].as_bool(), ..Default::default() });
                self.layout();
            }
            "cover" => {
                if m["on"] == true && !self.covered {
                    native::focus_shell();
                }
                self.covered = m["on"] == true;
                chrome::shell_state(chrome::ShellState {
                    overlay: m["overlay"].as_bool(),
                    web: m["web"].as_bool(),
                    ..Default::default()
                });
                self.layout();
                if self.focus_page_until.take().is_some_and(|until| until > Instant::now()) {
                    if let Some(host) = self.page_visible() {
                        focus_page(&host);
                    }
                }
            }
            "addressBar" => {
                if let Some(text) = m["text"].as_str().filter(|t| !t.trim().is_empty()) {
                    if chrome::on_web() {
                        self.navigate(self.active, text.into());
                    } else {
                        self.new_tab(Some(text.into()), true, false);
                    }
                }
            }
            "sidebar" => match serde_json::from_value(m["model"].clone()) {
                Ok(model) => sidebar::set_model(model),
                Err(e) => eprintln!("[native-chrome] sidebar model: {e}"),
            },
            "sidebarAction" => self.shell(&format!("window.atriumShell?.sidebar({})", m["action"])),
            "moveTab" => {
                let id = m["id"].as_u64().unwrap_or(0);
                if let Some(pos) = self.tabs.iter().position(|t| t.id == id) {
                    let tab = self.tabs.remove(pos);
                    let at = m["before"]
                        .as_u64()
                        .and_then(|b| self.tabs.iter().position(|t| t.id == b))
                        .unwrap_or(self.tabs.len());
                    self.tabs.insert(at, tab);
                    self.session_dirty = true;
                    self.push(false);
                }
            }
            "nativeSidebar" if native::background() => {
                let result = sidebar::debug_action(m["action"].as_str().unwrap_or(""), m["row"].as_i64().unwrap_or(-1), &m);
                if let Err(e) = result {
                    eprintln!("[native-chrome] {e}");
                }
            }
            "nativeAppearance" if native::background() => {
                if let Err(e) = native::set_appearance(m["mode"].as_str().unwrap_or("")) {
                    eprintln!("[native-appearance] {e}");
                }
            }
            // Test mode: native focus to the shell view without moving its DOM focus.
            "nativeFocusShell" if native::background() => native::focus_shell(),
            "nativeKey" if native::background() => {
                let code = m["keyCode"].as_u64().unwrap_or(0) as u16;
                if let Err(e) = native::send_key(code, m["characters"].as_str().unwrap_or(""), m["shift"] == true, m["toShell"] == true) {
                    eprintln!("[native-key] {e}");
                }
            }
            "nativeChrome" if native::background() => {
                let result = chrome::debug_action(m["action"].as_str().unwrap_or(""), m["text"].as_str().unwrap_or(""));
                if let Err(e) = result {
                    eprintln!("[native-chrome] {e}");
                }
            }
            "newTab" => {
                self.new_tab(
                    m["url"].as_str().map(str::to_string),
                    m["background"] != true,
                    false,
                );
            }
            "navigate" => {
                if let Some(url) = m["url"].as_str() {
                    self.navigate(self.active, url.into());
                }
            }
            "activate" => self.activate(m["id"].as_u64().unwrap_or(0)),
            "close" => self.close(m["id"].as_u64().unwrap_or(0)),
            "split" => {
                self.split = m["id"].as_u64().filter(|id| Some(*id) != self.active);
                if let Some(id) = self.split {
                    self.create_view(id);
                }
                self.layout();
                self.push(false);
            }
            "back" | "forward" => {
                if let Some(id) = self.active {
                    let _ = self.go(id, m["cmd"] == "back");
                }
            }
            "reload" => {
                if let Some(b) = self
                    .active
                    .and_then(|id| self.tab(id))
                    .and_then(|t| t.browser.as_ref())
                {
                    b.reload();
                }
            }
            "freeze" => {
                if let Some(id) = self.active {
                    self.cdp(
                        id,
                        "Page.captureScreenshot",
                        json!({"format":"jpeg","quality":70}),
                        Pending::Frozen("main"),
                    );
                }
                if let Some(id) = self.split {
                    self.cdp(
                        id,
                        "Page.captureScreenshot",
                        json!({"format":"jpeg","quality":70}),
                        Pending::Frozen("split"),
                    );
                }
            }
            "thumb" => {
                if let Some(id) = m["id"].as_u64() {
                    self.cdp(
                        id,
                        "Page.captureScreenshot",
                        json!({"format":"jpeg","quality":55}),
                        Pending::Frozen("pip"),
                    );
                }
            }
            "glance" => {
                if let Some(url) = m["url"].as_str() {
                    if self.tab(GLANCE).is_none() {
                        let id = self.new_tab(None, false, false);
                        if let Some(t) = self.tab_mut(id) {
                            t.id = GLANCE;
                        }
                        self.navigate(Some(GLANCE), url.into());
                    } else {
                        self.navigate(Some(GLANCE), url.into());
                    }
                    self.cmd("glanceOpened");
                    self.layout();
                    self.push(false);
                }
            }
            "glanceClose" => self.close(GLANCE),
            "glancePromote" => {
                if let Some(url) = self.tab(GLANCE).map(|t| t.url.clone()) {
                    self.close(GLANCE);
                    let id = self.new_tab(Some(url), m["split"] != true, false);
                    if m["split"] == true {
                        self.split = Some(id);
                        self.layout();
                        self.push(true);
                    }
                }
            }
            "openChrome" => {
                if let Some(url) = m["url"].as_str().filter(|u| {
                    reqwest::Url::parse(u).is_ok_and(|u| matches!(u.scheme(), "http" | "https"))
                }) {
                    let _ = std::process::Command::new("/usr/bin/open")
                        .args(["-a", "Google Chrome", url])
                        .spawn();
                }
            }
            // native.rs handles both while AppKit still holds the mouse event.
            "zoom" | "drag" => {}
            "uiReport" => self.cmds.shell = m.clone(),
            _ => self.action(m.clone(), None),
        }
    }
    fn event(&mut self, event: Event) {
        match event {
            Event::Init => {
                crate::permissions::sync(|origin, permission, decision| {
                    let value = match decision {
                        Some(crate::permissions::Decision::Allow) => ContentSettingValues::ALLOW,
                        Some(crate::permissions::Decision::Deny) => ContentSettingValues::BLOCK,
                        None => ContentSettingValues::DEFAULT,
                    };
                    let mut task = SiteSetting::new(origin.into(), permission.into(), value);
                    post_task(ThreadId::UI, Some(&mut task));
                });
                // The shell trades the per-launch token for a cookie, since WebKit
                // can't add headers to its EventSource and image requests.
                let token = crate::API_TOKEN.get().map(String::as_str).unwrap_or_default();
                native::create(self.bridge.clone(), &format!("{}/session?t={token}", self.base));
                self.restore_session();
                self.voiceover = mac::voiceover_enabled();
                self.update_accessibility();
                self.flush_urls();
                mac::ensure_delegate();
                if !mac::delegate_installed() {
                    eprintln!("URLs opened by other apps will not reach Atrium: the app delegate was replaced");
                }
                let mut maintenance = Maintenance::new(self.bridge.clone());
                post_delayed_task(ThreadId::UI, Some(&mut maintenance), 10000);
                if mac::background() {
                    let mut sweep = Sweep::new(self.bridge.clone());
                    post_delayed_task(ThreadId::UI, Some(&mut sweep), 250);
                }
            }
            Event::Created(id, browser) => {
                if self.closing.load(Ordering::Relaxed) {
                    if let Some(host) = browser.host() {
                        host.close_browser(1);
                    }
                }
                let mut observer = DevObserver::new(self.bridge.clone(), id);
                let host = browser.host();
                let reg = host
                    .as_ref()
                    .and_then(|h| h.add_dev_tools_message_observer(Some(&mut observer)));
                if let (Some(host), true) = (&host, self.accessibility) {
                    host.set_accessibility_state(State::ENABLED);
                }
                if let Some(t) = self.tab_mut(id) {
                    t.browser = Some(browser);
                    t.registration = reg;
                    // A popup's browser was created by Chromium, already parented to our window.
                    if t.opening {
                        t.opening = false;
                        t.live = true;
                        t.detached = false;
                    }
                }
                if id != SHELL {
                    // Binding calls are only reported while the Runtime domain is enabled.
                    self.cdp(id, "Runtime.enable", json!({}), Pending::Pw(id, PwStep::Done));
                    self.cdp(id, "Runtime.addBinding", json!({ "name": PW_BINDING, "executionContextName": PW_WORLD }), Pending::Pw(id, PwStep::Done));
                }
                self.layout();
            }
            Event::Binding(id, params) => {
                if params["name"] == PW_BINDING {
                    if let (Some(ctx), Some(Ok(msg))) = (params["executionContextId"].as_i64(), params["payload"].as_str().map(serde_json::from_str::<Json>)) {
                        self.pw_message(id, ctx, msg);
                    }
                }
            }
            Event::Password(event) => self.pw_event(event),
            Event::Detach(id) => {
                if let Some(t) = self.tab_mut(id).filter(|t| !t.detached) {
                    t.detached = true;
                    if let Some(host) = t.browser.as_ref().and_then(|b| b.host()) {
                        native::detach(host.window_handle());
                    }
                }
            }
            Event::Closed(id) => {
                let was_suspending = self.tab(id).is_some_and(|t| t.suspending);
                if !self.closing.load(Ordering::Relaxed)
                    && self.tab(id).is_some_and(|t| t.suspending)
                {
                    let mut reopen = false;
                    if let Some(t) = self.tab_mut(id) {
                        t.browser = None;
                        t.registration = None;
                        t.suspended = true;
                        t.suspending = false;
                        t.live = false;
                        reopen = std::mem::take(&mut t.reopen);
                    }
                    if reopen || self.active == Some(id) || self.split == Some(id) {
                        self.create_view(id);
                    }
                    self.push(false);
                }
                if !self.closing.load(Ordering::Relaxed) && !was_suspending {
                    self.remove_tab(id);
                }
                if self.closing.load(Ordering::Relaxed) {
                    if let Some(t) = self.tab_mut(id) {
                        t.browser = None;
                        t.live = false;
                        t.registration = None;
                    }
                    self.finish_quit();
                }
            }
            Event::Resize => self.layout(),
            Event::Operation(op) => self.operation(op),
            Event::Command(m) => self.command(m),
            Event::Icon(url, bytes) => sidebar::icon_loaded(&url, &bytes),
            Event::Title(id, title) => {
                if let Some(t) = self.tab_mut(id) {
                    t.title = title;
                }
                self.push(false);
            }
            Event::Address(id, url) => {
                if let Some(t) = self.tab_mut(id) {
                    if crate::favicons::key(&t.url) != crate::favicons::key(&url) {
                        t.icon = None;
                    }
                    t.url = url;
                }
                self.session_dirty = true;
                self.apply_zoom(id);
                self.push(false);
            }
            Event::Loading(id, loading) => {
                if let Some(t) = self.tab_mut(id) {
                    t.loading = loading;
                    t.document_loading &= loading;
                    t.crashed &= !loading;
                }
                self.push(false);
            }
            Event::Document(id, loading) => {
                if let Some(t) = self.tab_mut(id) {
                    t.document_loading = loading;
                }
            }
            Event::Crashed(id) => self.crashed(id),
            Event::Loaded(id) => {
                if let Some(t) = self.tab_mut(id) {
                    t.document_loading = false;
                }
                if id != SHELL && self.tab(id).is_some_and(|t| t.browser.is_some()) {
                    let scroll = self
                        .tab_mut(id)
                        .and_then(|t| t.scroll.take())
                        .map(|(x, y)| format!("\nscrollTo({x}, {y});"))
                        .unwrap_or_default();
                    self.isolated(id, format!("{LIFECYCLE_JS}\n{PAGE_JS}{scroll}"), false, Pending::Inject);
                    self.pw_forms.remove(&id);
                    if self.pw_offers.get(&id).is_some_and(|o| matches!(o["kind"].as_str(), Some("fill" | "generate"))) {
                        self.pw_offer(id, None);
                    }
                    self.cdp(id, "Page.getFrameTree", json!({}), Pending::Pw(id, PwStep::Tree));
                    self.apply_zoom(id);
                }
                self.push(false);
            }
            Event::Download(id, download, active) => {
                if let Some(t) = self.tab_mut(id) {
                    if active {
                        t.downloads.insert(download);
                    } else {
                        t.downloads.remove(&download);
                    }
                }
            }
            Event::Capture(id, capture) => {
                if let Some(t) = self.tab_mut(id) {
                    t.capture = capture;
                }
            }
            Event::Popup(url) => {
                self.new_tab(Some(url), true, false);
            }
            Event::PopupTab {
                id,
                opener,
                url,
                foreground,
            } => {
                let url = if url.is_empty() { "about:blank".into() } else { url };
                self.tabs.push(Tab {
                    opener,
                    opening: true,
                    loading: true,
                    ..Tab::new(id, url, String::new())
                });
                if foreground {
                    self.active = Some(id);
                }
                self.session_dirty = true;
                self.layout();
                self.push(foreground);
            }
            Event::Adopt(browser) => {
                let url = foreign_url(&browser);
                let id = browser.identifier();
                if url.starts_with("devtools://") || self.adopted.contains(&id) {
                    return;
                }
                let Some(host) = browser.host() else { return };
                mac::hide_window(host.window_handle());
                // The first commit can be a blank placeholder before the requested page.
                if url.is_empty() || url == "about:blank" {
                    return;
                }
                self.adopted.insert(id);
                eprintln!("opened a Chromium-created window as a tab: {url}");
                self.new_tab(Some(url), true, false);
                host.close_browser(1);
            }
            Event::DevResult(id, success, data) => self.finish(id, success, data),
            Event::Assistive(attribute, on) => {
                if on {
                    self.assistive.insert(attribute);
                } else {
                    self.assistive.remove(&attribute);
                }
                self.update_accessibility();
            }
            Event::TakeFocus(id, next) => self.take_focus(id, next),
            Event::Favicon(id, icon_url) => {
                let Some(tab) = self.tab(id) else { return };
                let (Some(key), true) = (crate::favicons::key(&tab.url), crate::favicons::fetchable(&icon_url)) else { return };
                if let Some(host) = tab.browser.as_ref().and_then(|b| b.host()) {
                    // As a favicon request, Chromium sends and stores no cookies for it.
                    let mut done = FaviconDownload::new(self.bridge.clone(), id, key);
                    host.download_image(Some(&CefString::from(icon_url.as_str())), 1, 64, 0, Some(&mut done));
                }
            }
            Event::FaviconSaved(id, key) => {
                if let Some(t) = self.tab_mut(id).filter(|t| crate::favicons::key(&t.url).as_ref() == Some(&key)) {
                    t.icon = Some(format!("/favicon/{key}?v={}", uuid::Uuid::new_v4().simple()));
                    self.push(false);
                }
            }
            Event::Quit => {
                self.session_dirty = true;
                mac::release_test_pasteboard();
                self.save_session();
                self.closing.store(true, Ordering::Relaxed);
                for t in &self.tabs {
                    if let Some(host) = t.browser.as_ref().and_then(|b| b.host()) {
                        host.close_browser(0);
                    }
                }
                // With no live browsers, nothing will report closing, so quit now.
                self.finish_quit();
            }
            Event::Maintenance => self.maintain(),
            Event::Commands(event) => self.on_commands(event),
        }
    }
}
fn rect(v: &Json) -> Option<Rect> {
    Some(Rect {
        x: v["x"].as_f64()?.round() as i32,
        y: v["y"].as_f64()?.round() as i32,
        width: (v["w"].as_f64()?.round() as i32).max(1),
        height: (v["h"].as_f64()?.round() as i32).max(1),
    })
}

wrap_app! { struct HostApp { bridge: Bridge } impl App {
fn on_before_command_line_processing(&self, process_type:Option<&CefString>,command_line:Option<&mut CommandLine>) {
    if process_type.is_none_or(|p|p.to_string().is_empty()) {
        // Throwaway test profiles must not prompt for the login keychain on every ad hoc build.
        if let (Some(command), true) = (command_line.as_deref(), std::env::var_os("ATRIUM_MOCK_KEYCHAIN").is_some()) {
            command.append_switch(Some(&CefString::from("use-mock-keychain")));
        }
        // An off-screen window counts as occluded; keep its pages painting for screenshots.
        if let (Some(command), true) = (command_line.as_deref(), mac::background()) {
            command.append_switch_with_value(Some(&CefString::from("disable-features")), Some(&CefString::from("MacWebContentsOcclusion")));
            command.append_switch(Some(&CefString::from("disable-backgrounding-occluded-windows")));
        }
        let paths: Vec<_> = crate::CHROME_EXTENSIONS.get().into_iter().chain(crate::INSTALLED_EXTENSIONS.get()).flatten().collect();
        if let (false,Some(command))=(paths.is_empty(),command_line) {
            let value=paths.iter().map(|p|p.to_string_lossy()).collect::<Vec<_>>().join(",");command.append_switch_with_value(Some(&CefString::from("load-extension")),Some(&CefString::from(value.as_str())));
        }
    }
}
fn browser_process_handler(&self)->Option<BrowserProcessHandler>{Some(HostProcess::new(self.bridge.clone()))}} }
wrap_browser_process_handler! {struct HostProcess {bridge:Bridge} impl BrowserProcessHandler {
    fn on_context_initialized(&self){self.bridge.send(Event::Init);}
    // Chromium itself creates top-level browser windows for extension tabs.create, windows.create,
    // and openOptionsPage. They are hidden at once and their pages reopened as Atrium tabs.
    fn default_client(&self)->Option<Client>{Some(ForeignClient::new(self.bridge.clone()))}
}}
wrap_client! {struct ForeignClient {bridge:Bridge} impl Client {
    fn life_span_handler(&self)->Option<LifeSpanHandler>{Some(ForeignLife::new(self.bridge.clone()))}
    fn display_handler(&self)->Option<DisplayHandler>{Some(ForeignDisplay::new(self.bridge.clone()))}
}}
wrap_life_span_handler! {struct ForeignLife {bridge:Bridge} impl LifeSpanHandler {
    fn on_after_created(&self,browser:Option<&mut Browser>){
        let Some(b)=browser else{return};
        if !foreign_url(b).starts_with("devtools://") { if let Some(host)=b.host() { mac::hide_window(host.window_handle()); } }
        self.bridge.send(Event::Adopt(b.clone()));
    }
    fn on_before_popup(&self, _unused7:Option<&mut Browser>, _unused8:Option<&mut Frame>, _unused9:i32,url:Option<&CefString>, _unused10:Option<&CefString>, _unused11:WindowOpenDisposition, _unused12:i32, _unused13:Option<&PopupFeatures>, _unused14:Option<&mut WindowInfo>, _unused15:Option<&mut Option<Client>>, _unused16:Option<&mut BrowserSettings>, _unused17:Option<&mut Option<DictionaryValue>>, _unused18:Option<&mut i32>)->i32{if let Some(u)=url{self.bridge.send(Event::Popup(u.to_string()));}1}
}}
wrap_display_handler! {struct ForeignDisplay {bridge:Bridge} impl DisplayHandler {
    fn on_address_change(&self,browser:Option<&mut Browser>,frame:Option<&mut Frame>, _unused20:Option<&CefString>){if let (Some(b),true)=(browser,frame.is_some_and(|f|f.is_main()!=0)){self.bridge.send(Event::Adopt(b.clone()));}}
}}
fn foreign_url(browser: &Browser) -> String {
    browser.main_frame().map(|f| CefString::from(&f.url()).to_string()).unwrap_or_default()
}
wrap_task! {struct Sweep {bridge:Bridge} impl Task {fn execute(&self){mac::sweep();if self.bridge.manager.upgrade().is_some(){let mut next=Self::new(self.bridge.clone());post_delayed_task(ThreadId::UI,Some(&mut next),250);}}}}
/// Keyboard focus to a page: AppKit's first responder and Chromium's own focus.
fn focus_page(host: &BrowserHost) {
    native::focus_view(host.window_handle());
    host.set_focus(1);
}
fn is_local_api(url: &str, base: &str) -> bool {
    url.strip_prefix(base).is_some_and(|rest| rest.is_empty() || rest.starts_with(['/', '?', '#']))
}
/// The About pages are static and sandboxed, so they may open in a tab.
fn is_about(url: &str, base: &str) -> bool {
    url.strip_prefix(base).is_some_and(|rest| {
        let path = rest.split(['?', '#']).next().unwrap_or_default();
        path == crate::about::PATH || path == crate::about::CREDITS_PATH
    })
}
// Tabs never load the API origin, so no page shares the shell's origin and storage.
wrap_request_handler! {struct TabRequests {bridge:Bridge,id:u64,base:String} impl RequestHandler {
    fn on_render_process_terminated(&self, _unused50:Option<&mut Browser>, _unused51:TerminationStatus, _unused52:i32, _unused53:Option<&CefString>){self.bridge.send(Event::Crashed(self.id));}
    fn on_before_browse(&self, _browser:Option<&mut Browser>, _frame:Option<&mut Frame>, request:Option<&mut Request>, _user_gesture:i32, _is_redirect:i32)->i32{
        request.is_some_and(|r| {
            let url = CefString::from(&r.url()).to_string();
            is_local_api(&url, &self.base) && !is_about(&url, &self.base)
        }).into()
    }
}}
type Popups = Arc<Mutex<HashMap<i32, u64>>>;
static NEXT_POPUP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1 << 32);
wrap_client! {struct HostClient {bridge:Bridge,id:u64,base:String,popups:Popups} impl Client {
    fn request_handler(&self)->Option<RequestHandler>{Some(TabRequests::new(self.bridge.clone(), self.id, self.base.clone()))}
    // The shell is a WKWebView, so a Chromium renderer claiming to send shell commands is a website.
    fn on_process_message_received(&self, _browser:Option<&mut Browser>, _frame:Option<&mut Frame>, _source_process:ProcessId, message:Option<&mut ProcessMessage>)->i32{
        let Some(message) = message else { return 0 };
        if CefString::from(&message.name()).to_string() != shell_ipc::MESSAGE { return 0; }
        REJECTED_MESSAGES.fetch_add(1, Ordering::Relaxed);
        eprintln!("ignored a shell command from a Chromium renderer");
        1
    }
    fn life_span_handler(&self)->Option<LifeSpanHandler>{Some(HostLife::new(self.bridge.clone(),self.id,self.base.clone(),self.popups.clone()))}
    fn permission_handler(&self)->Option<PermissionHandler>{Some(HostPermissions::new(self.id))}
    fn display_handler(&self)->Option<DisplayHandler>{Some(HostDisplay::new(self.bridge.clone(),self.id))}
    fn load_handler(&self)->Option<LoadHandler>{Some(HostLoad::new(self.bridge.clone(),self.id))}
    fn download_handler(&self)->Option<DownloadHandler>{Some(HostDownload::new(self.bridge.clone(),self.id))}
    fn keyboard_handler(&self)->Option<KeyboardHandler>{Some(HostKeyboard::new(self.bridge.clone(),self.id))}
    fn focus_handler(&self)->Option<FocusHandler>{Some(HostFocus::new(self.bridge.clone(),self.id))}
    fn find_handler(&self)->Option<FindHandler>{Some(commands::HostFind::new(self.bridge.clone(),self.id))}
    fn context_menu_handler(&self)->Option<ContextMenuHandler>{(self.id!=SHELL).then(|| commands::HostMenu::new(self.bridge.clone(),self.id))}
}}
// Tabbing past either end of a page continues in the shell or the next page.
wrap_focus_handler! {struct HostFocus {bridge:Bridge,id:u64} impl FocusHandler {
    fn on_take_focus(&self, _browser:Option<&mut Browser>, next:i32){self.bridge.send(Event::TakeFocus(self.id,next!=0));}
}}
wrap_life_span_handler! {struct HostLife {bridge:Bridge,id:u64,base:String,popups:Popups} impl LifeSpanHandler {
    fn on_after_created(&self,browser:Option<&mut Browser>){if let Some(b)=browser{self.bridge.send(Event::Created(self.id,b.clone()));}}
    fn do_close(&self,_browser:Option<&mut Browser>)->i32{self.bridge.send(Event::Detach(self.id));1}
    fn on_before_close(&self, _unused6:Option<&mut Browser>){self.bridge.send(Event::Closed(self.id));}
    // Website popups keep their opener so OAuth-style flows can message it and close.
    // The shell and Glance must never become a website's window.opener.
    fn on_before_popup(&self, _unused7:Option<&mut Browser>, _unused8:Option<&mut Frame>, popup_id:i32,url:Option<&CefString>, _unused10:Option<&CefString>, disposition:WindowOpenDisposition, user_gesture:i32, _unused13:Option<&PopupFeatures>, window_info:Option<&mut WindowInfo>, client:Option<&mut Option<Client>>, _unused16:Option<&mut BrowserSettings>, _unused17:Option<&mut Option<DictionaryValue>>, _unused18:Option<&mut i32>)->i32{
        let url=url.map(ToString::to_string).unwrap_or_default();
        // Like Chrome's popup blocker, and without browser-initiated loads of file: or chrome: URLs.
        if user_gesture==0||!url::is_web_url(&url){
            if !url.is_empty(){eprintln!("blocked a popup to {}", url.chars().take(80).collect::<String>());}
            return 1;
        }
        let Some(client)=client.filter(|_|self.id!=SHELL&&self.id!=GLANCE) else {
            if !url.is_empty(){self.bridge.send(Event::Popup(url));}
            return 1;
        };
        // Popups are tabs in our window: a hidden Alloy child view until layout() places it.
        let (Some(info),Some(parent))=(window_info,native::content()) else {
            self.bridge.send(Event::Popup(url));
            return 1;
        };
        info.parent_view=parent;
        info.hidden=1;
        info.runtime_style=RuntimeStyle::ALLOY;
        let id=NEXT_POPUP.fetch_add(1,Ordering::Relaxed);
        self.popups.lock().unwrap().insert(popup_id,id);
        self.bridge.send(Event::PopupTab{id,opener:(disposition==WindowOpenDisposition::NEW_POPUP).then_some(self.id),url,foreground:disposition!=WindowOpenDisposition::NEW_BACKGROUND_TAB});
        *client=Some(HostClient::new(self.bridge.clone(),id,self.base.clone(),Default::default()));
        0
    }
    fn on_before_popup_aborted(&self, _unused19:Option<&mut Browser>, popup_id:i32){if let Some(id)=self.popups.lock().unwrap().remove(&popup_id){self.bridge.send(Event::Closed(id));}}
}}
// Websites get a permission only after the user allows it for their origin; the shell never does.
wrap_permission_handler! {struct HostPermissions {id:u64} impl PermissionHandler {
    fn on_request_media_access_permission(&self, _unused70:Option<&mut Browser>, _unused71:Option<&mut Frame>, origin:Option<&CefString>, requested:u32, callback:Option<&mut MediaAccessCallback>)->i32{
        let Some(callback)=callback else{return 0};
        let granted=if self.id==SHELL{0}else{crate::permissions::decide(&origin.map(ToString::to_string).unwrap_or_default(),requested,crate::permissions::MEDIA)};
        if granted==0{callback.cancel();}else{callback.cont(granted);}
        1
    }
    fn on_show_permission_prompt(&self, _unused72:Option<&mut Browser>, _unused73:u64, origin:Option<&CefString>, requested:u32, callback:Option<&mut PermissionPromptCallback>)->i32{
        let Some(callback)=callback else{return 0};
        let granted=if self.id==SHELL{0}else{crate::permissions::decide(&origin.map(ToString::to_string).unwrap_or_default(),requested,crate::permissions::PROMPT)};
        callback.cont(if granted==requested&&requested!=0{PermissionRequestResult::ACCEPT}else{PermissionRequestResult::DENY});
        1
    }
}}
// Chromium persists a denied prompt per origin and stops asking, so user decisions
// are written to its content settings too.
wrap_task! {struct SiteSetting {origin:String,permission:String,value:ContentSettingValues} impl Task {fn execute(&self){
    let Some(context)=request_context_get_global_context() else{return};
    let origin=CefString::from(self.origin.as_str());
    for kind in content_settings(&self.permission){context.set_content_setting(Some(&origin),Some(&origin),kind.clone(),self.value.clone());}
}}}
fn content_settings(permission:&str)->&'static [ContentSettingTypes]{
    use ContentSettingTypes as T;
    match permission {
        "ar"=>&[T::AR],"camera-pan-tilt-zoom"=>&[T::CAMERA_PAN_TILT_ZOOM],"camera"=>&[T::MEDIASTREAM_CAMERA],
        "captured-surface-control"=>&[T::CAPTURED_SURFACE_CONTROL],"clipboard"=>&[T::CLIPBOARD_READ_WRITE],
        "storage-access"=>&[T::STORAGE_ACCESS],"local-fonts"=>&[T::LOCAL_FONTS],"geolocation"=>&[T::GEOLOCATION],
        "hand-tracking"=>&[T::HAND_TRACKING],"idle-detection"=>&[T::IDLE_DETECTION],"microphone"=>&[T::MEDIASTREAM_MIC],
        "midi"=>&[T::MIDI_SYSEX],"multiple-downloads"=>&[T::AUTOMATIC_DOWNLOADS],"notifications"=>&[T::NOTIFICATIONS],
        "keyboard-lock"=>&[T::KEYBOARD_LOCK],"pointer-lock"=>&[T::POINTER_LOCK],"protected-media"=>&[T::PROTECTED_MEDIA_IDENTIFIER],
        "vr"=>&[T::VR],"web-app-installation"=>&[T::WEB_APP_INSTALLATION],"window-management"=>&[T::WINDOW_MANAGEMENT],
        "local-network"=>&[T::LOCAL_NETWORK_ACCESS,T::LOCAL_NETWORK],"loopback-network"=>&[T::LOOPBACK_NETWORK],
        "sensors"=>&[T::SENSORS],"screen"=>&[T::DISPLAY_CAPTURE],
        _=>&[],
    }
}
wrap_display_handler! {struct HostDisplay {bridge:Bridge,id:u64} impl DisplayHandler {
    fn on_title_change(&self, _unused19:Option<&mut Browser>,title:Option<&CefString>){self.bridge.send(Event::Title(self.id,title.map(ToString::to_string).unwrap_or_default()));}
    fn on_address_change(&self, _unused20:Option<&mut Browser>,frame:Option<&mut Frame>,url:Option<&CefString>){if frame.is_some_and(|f|f.is_main()!=0){self.bridge.send(Event::Address(self.id,url.map(ToString::to_string).unwrap_or_default()));}}
    fn on_favicon_urlchange(&self, _unused50:Option<&mut Browser>,icon_urls:Option<&mut CefStringList>){
        let Some(list)=icon_urls else{return};
        let list:*mut sys::_cef_string_list_t=list.into();
        // The output string must be owned storage; a default CefString wraps a null pointer.
        let first=unsafe{
            let mut value:sys::cef_string_t=std::mem::zeroed();
            let found=sys::cef_string_list_size(list)>0&&sys::cef_string_list_value(list,0,&mut value)!=0;
            let url=found.then(||CefString::from(std::ptr::from_ref(&value)).to_string());
            sys::cef_string_utf16_clear(&mut value);
            url
        };
        if let Some(url)=first{self.bridge.send(Event::Favicon(self.id,url));}
    }
    fn on_media_access_change(&self, _unused21:Option<&mut Browser>,video:i32,audio:i32){self.bridge.send(Event::Capture(self.id,video!=0||audio!=0));}
}}
wrap_load_handler! {struct HostLoad {bridge:Bridge,id:u64} impl LoadHandler {
    fn on_loading_state_change(&self, _unused22:Option<&mut Browser>,loading:i32, _unused23:i32, _unused24:i32){self.bridge.send(Event::Loading(self.id,loading!=0));}
    fn on_load_start(&self, _unused51:Option<&mut Browser>,frame:Option<&mut Frame>, _unused52:TransitionType){if frame.is_some_and(|f|f.is_main()!=0){self.bridge.send(Event::Document(self.id,true));}}
    fn on_load_end(&self, _unused25:Option<&mut Browser>,frame:Option<&mut Frame>, _unused26:i32){if frame.is_some_and(|f|f.is_main()!=0){self.bridge.send(Event::Loaded(self.id));}}
    fn on_load_error(&self, _unused53:Option<&mut Browser>,frame:Option<&mut Frame>, _unused54:Errorcode, _unused55:Option<&CefString>, _unused56:Option<&CefString>){if frame.is_some_and(|f|f.is_main()!=0){self.bridge.send(Event::Document(self.id,false));}}
}}
wrap_dev_tools_message_observer! {struct DevObserver {bridge:Bridge,id:u64} impl DevToolsMessageObserver {
    fn on_dev_tools_method_result(&self, _unused27:Option<&mut Browser>,id:i32,success:i32,result:Option<&[u8]>){self.bridge.send(Event::DevResult(id,success!=0,result.and_then(|r|serde_json::from_slice(r).ok()).unwrap_or(Json::Null)));}
    fn on_dev_tools_event(&self, _unused50:Option<&mut Browser>,method:Option<&CefString>,params:Option<&[u8]>){
        if method.is_some_and(|m|m.to_string()=="Runtime.bindingCalled") {
            if let Some(params)=params.and_then(|p|serde_json::from_slice(p).ok()) {self.bridge.send(Event::Binding(self.id,params));}
        }
    }
}}
wrap_keyboard_handler! {struct HostKeyboard {bridge:Bridge,id:u64} impl KeyboardHandler {
    fn on_pre_key_event(&self, _unused28:Option<&mut Browser>,event:Option<&KeyEvent>, _unused29:*mut u8, is_shortcut:Option<&mut i32>)->i32{
        let Some(e)=event else{return 0}; if e.type_!=KeyEventType::RAWKEYDOWN {return 0;}
        // F6 moves focus between the browser UI and the page; Escape leaves Glance.
        let plain = e.modifiers & ((1<<2)|(1<<3)|(1<<7)) == 0;
        if plain && e.windows_key_code==117 {
            self.bridge.send(Event::Command(if self.id==SHELL {json!({"cmd":"focus","target":"page"})} else {json!({"cmd":"focus","target":"shell"})}));
            return 1;
        }
        if plain && e.windows_key_code==27 && self.id==GLANCE {
            self.bridge.send(Event::Command(json!({"cmd":"shellCommand","name":"closeOverlay"})));
            return 1;
        }
        match commands::shortcut(e.windows_key_code, e.modifiers) {
            Some((name, true)) => {self.bridge.send(Event::Command(json!({"cmd":"menu","name":name})));1}
            // The page may handle it first; on_key_event runs the command if it doesn't.
            Some((_, false)) => {if let Some(s)=is_shortcut{*s=1;}0}
            None => 0,
        }
    }
    fn on_key_event(&self, _unused50:Option<&mut Browser>,event:Option<&KeyEvent>, _unused51:*mut u8)->i32{
        let Some(e)=event else{return 0}; if e.type_!=KeyEventType::RAWKEYDOWN {return 0;}
        match commands::shortcut(e.windows_key_code, e.modifiers) {
            Some((name, false)) => {self.bridge.send(Event::Command(json!({"cmd":"menu","name":name})));1}
            _ => 0,
        }
    }
}}

pub fn chromium_version() -> String {
    format!("{}.{}.{}.{}", sys::CHROME_VERSION_MAJOR, sys::CHROME_VERSION_MINOR, sys::CHROME_VERSION_BUILD, sys::CHROME_VERSION_PATCH)
}

pub fn run(base: String, start: impl FnOnce(Handle), on_exit: impl FnOnce() + 'static) -> ! {
    let loader = library_loader::LibraryLoader::new(
        &std::env::current_exe().expect("application path"),
        false,
    );
    assert!(
        loader.load(),
        "Run the bundled {}.app; the Chromium framework must be next to the application",
        crate::brand::EXECUTABLE
    );
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    mac::setup();
    let (tx, rx) = mpsc::channel();
    let queued = Arc::new(AtomicBool::new(false));
    let closing = Arc::new(AtomicBool::new(false));
    let manager = Arc::new_cyclic(|weak| {
        Mutex::new(Manager {
            rx,
            bridge: Bridge {
                tx,
                manager: weak.clone(),
                queued,
            },
            base,
            tabs: Vec::new(),
            active: None,
            split: None,
            main_rect: Rect {
                x: 250,
                y: 6,
                width: 1024,
                height: 808,
            },
            split_rect: None,
            glance_rect: None,
            covered: true,
            shell_loaded: false,
            policy: crate::memory::Policy::default(),
            session_dirty: false,
            next_id: 0,
            next_call: 0,
            pending: HashMap::new(),
            adopted: Default::default(),
            closing,
            voiceover: false,
            assistive: Default::default(),
            accessibility: false,
            focus_page_until: None,
            pw_forms: HashMap::new(),
            pw_pending: HashMap::new(),
            pw_offers: HashMap::new(),
            cmds: Default::default(),
        })
    });
    let bridge = manager.lock().unwrap().bridge.clone();
    mac::set_bridge(bridge.clone());
    start(Handle(bridge.clone()));
    let args = cef::args::Args::new();
    let root = crate::data_dir().expect("data directory").join("chromium");
    std::fs::create_dir_all(&root).expect("Chromium profile directory");
    let settings = Settings {
        root_cache_path: CefString::from(root.to_string_lossy().as_ref()),
        cache_path: CefString::from(root.join("profile").to_string_lossy().as_ref()),
        persist_session_cookies: 1,
        no_sandbox: 0,
        remote_debugging_port: 0,
        log_severity: LogSeverity::WARNING,
        ..Default::default()
    };
    let mut app = HostApp::new(bridge.clone());
    assert_eq!(
        initialize(
            Some(args.as_main_args()),
            Some(&settings),
            Some(&mut app),
            std::ptr::null_mut()
        ),
        1,
        "Chromium initialization failed"
    );
    let _menu = mac::menu(bridge);
    run_message_loop();
    // Chromium's teardown watchdog may terminate the process inside shutdown().
    on_exit();
    drop(manager);
    shutdown();
    std::process::exit(0)
}

wrap_download_handler! {struct HostDownload {bridge:Bridge,id:u64} impl DownloadHandler {
    fn can_download(&self,_browser:Option<&mut Browser>,_url:Option<&CefString>,_method:Option<&CefString>)->i32{1}
    // The download list picks the path (no dialog unless the user chose "Save … As").
    fn on_before_download(&self,_browser:Option<&mut Browser>,item:Option<&mut DownloadItem>,name:Option<&CefString>,callback:Option<&mut BeforeDownloadCallback>)->i32{
        let (Some(item),Some(callback))=(item,callback) else {return 0};
        self.bridge.send(Event::Download(self.id,item.id(),true));
        self.bridge.send(Event::Commands(commands::Ev::BeforeDownload{item:commands::download_info(self.id,item,name),callback:callback.clone()}));
        1
    }
    fn on_download_updated(&self,_browser:Option<&mut Browser>,item:Option<&mut DownloadItem>,callback:Option<&mut DownloadItemCallback>){if let Some(item)=item{self.bridge.send(Event::Download(self.id,item.id(),item.is_in_progress()!=0));self.bridge.send(Event::Commands(commands::Ev::DownloadUpdated{item:commands::download_info(self.id,item,None),control:callback.map(|c|c.clone())}));}}
}}
type Entries = Arc<Mutex<(Vec<String>, usize)>>;
wrap_navigation_entry_visitor! {struct HistoryVisitor {entries:Entries} impl NavigationEntryVisitor {
    fn visit(&self,entry:Option<&mut NavigationEntry>,current:i32,_index:i32,_total:i32)->i32{
        let mut entries=self.entries.lock().unwrap();
        if current!=0{entries.1=entries.0.len();}
        entries.0.push(entry.map(|e|CefString::from(&e.url()).to_string()).unwrap_or_default());
        1
    }
}}
const HISTORY: usize = 50;
fn is_web(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https"))
}
wrap_download_image_callback! {struct FaviconDownload {bridge:Bridge,id:u64,key:String} impl DownloadImageCallback {
    fn on_download_image_finished(&self,_url:Option<&CefString>,_status:i32,image:Option<&mut Image>){
        let (mut width,mut height)=(0,0);
        let Some(png)=image.filter(|i|i.is_empty()==0).and_then(|i|i.as_png(1.0,1,Some(&mut width),Some(&mut height))) else{return};
        let mut bytes=vec![0;png.size()];
        if bytes.is_empty()||bytes.len()>crate::favicons::MAX_BYTES||png.data(Some(&mut bytes),0)!=bytes.len(){return}
        if crate::favicons::store(&self.key,&bytes).is_ok(){self.bridge.send(Event::FaviconSaved(self.id,self.key.clone()));}
    }
}}
wrap_task! {struct Maintenance {bridge:Bridge} impl Task {fn execute(&self){self.bridge.send(Event::Maintenance);if self.bridge.manager.upgrade().is_some(){let mut next=Self::new(self.bridge.clone());post_delayed_task(ThreadId::UI,Some(&mut next),10000);}}}}
impl Manager {
    /// Chromium must release every browser before the window and message loop go.
    fn finish_quit(&self) {
        if self.tabs.iter().all(|t| t.browser.is_none() && !t.live) {
            native::close();
            quit_message_loop();
        }
    }
    /// A live popup and its live opener protect each other, so `window.opener` keeps working.
    fn linked(&self, id: u64) -> bool {
        let live = |id: u64| self.tab(id).is_some_and(|t| t.browser.is_some());
        self.tab(id).and_then(|t| t.opener).is_some_and(live)
            || self.tabs.iter().any(|t| t.opener == Some(id) && live(t.id))
    }
    fn can_suspend(&self, id: u64) -> bool {
        !self.linked(id) && self.tab(id).is_some_and(|t| {
            t.live
                && !t.suspending
                && !t.document_loading
                && !t.capture
                && t.downloads.is_empty()
                && t.agent_until <= Instant::now()
                && Some(id) != self.active
                && Some(id) != self.split
                && id != SHELL
                && id != GLANCE
                && !t.scratch
        })
    }
    fn maintain(&mut self) {
        self.save_session();
        self.voiceover = mac::voiceover_enabled();
        self.update_accessibility();
        let now = Instant::now();
        let crashed = self
            .tabs
            .iter()
            .filter(|t| t.crashed && t.live && !t.suspending)
            .filter(|t| Some(t.id) != self.active && Some(t.id) != self.split && t.id != GLANCE)
            .map(|t| t.id)
            .collect::<Vec<_>>();
        for id in crashed {
            self.suspend(id);
        }
        let candidates = self
            .tabs
            .iter()
            .filter(|t| t.id != GLANCE && t.live && !t.scratch && !t.crashed)
            .map(|t| crate::memory::Candidate {
                id: t.id,
                idle: now.saturating_duration_since(t.last_used),
                visible: Some(t.id) == self.active || Some(t.id) == self.split,
                loading: t.document_loading,
                frames: t.browser.as_ref().map_or(0, |b| b.frame_count()),
                protected: t.capture
                    || self.linked(t.id)
                    || !t.downloads.is_empty()
                    || t.agent_until > now
                    || t.page_protected_until > now
                    || t.suspending,
            })
            .collect::<Vec<_>>();
        for id in self.policy.candidates(&candidates) {
            if self.pending.values().any(|(_, tab, _)| *tab == id) {
                continue;
            }
            self.isolated(id, format!("{LIFECYCLE_JS}; window.__atriumLifecycle.canSuspend() && [scrollX, scrollY]"), false, Pending::Probe(id));
        }
        let expired = self
            .pending
            .iter()
            .filter(|(_, (at, _, _))| at.elapsed() > Duration::from_secs(25))
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in expired {
            self.finish(
                id,
                false,
                json!({"message":"The browser operation timed out"}),
            );
        }
    }
    fn save_session(&mut self) {
        if !self.session_dirty {
            return;
        }
        let tabs = self
            .tabs
            .iter()
            .filter(|t| {
                t.id != SHELL
                    && t.id != GLANCE
                    && !t.scratch
                    && is_web(&t.url)
                    && !t.url.starts_with(&format!("{}/", self.base))
            })
            .take(1000)
            .map(|t| {
                let (back, forward) = self.history(t.id);
                json!({"url":t.url,"title":t.title,"active":Some(t.id)==self.active,"back":back,"forward":forward,"scroll":t.scroll.map(|(x, y)| [x, y])})
            })
            .collect::<Vec<_>>();
        if let Ok(dir) = crate::data_dir() {
            let path = dir.join("chromium-tabs.json");
            let temp = dir.join("chromium-tabs.json.tmp");
            if let Ok(bytes) = serde_json::to_vec(&tabs) {
                if std::fs::write(&temp, bytes)
                    .and_then(|_| std::fs::rename(&temp, &path))
                    .is_ok()
                {
                    self.session_dirty = false;
                }
            }
        }
    }
    fn restore_session(&mut self) {
        let Some(tabs) = crate::data_dir()
            .ok()
            .and_then(|dir| std::fs::read(dir.join("chromium-tabs.json")).ok())
            .and_then(|bytes| serde_json::from_slice::<Vec<Json>>(&bytes).ok())
        else {
            return;
        };
        for saved in tabs.into_iter().take(1000) {
            let Some(url) = saved["url"].as_str().filter(|url| is_web(url)) else {
                continue;
            };
            let urls = |key: &str| {
                let list = saved[key].as_array().map(Vec::as_slice).unwrap_or_default();
                list.iter().filter_map(Json::as_str).filter(|u| is_web(u)).map(String::from).collect::<Vec<_>>()
            };
            let (mut back, mut forward) = (urls("back"), urls("forward"));
            back.drain(..back.len().saturating_sub(HISTORY));
            forward.truncate(HISTORY);
            let scroll = &saved["scroll"];
            self.next_id += 1;
            let id = self.next_id;
            self.tabs.push(Tab {
                suspended: true,
                back,
                forward,
                scroll: scroll[0].as_f64().zip(scroll[1].as_f64()),
                ..Tab::new(id, url.into(), saved["title"].as_str().unwrap_or("").into())
            });
            if saved["active"] == true {
                self.active = Some(id);
            }
        }
        if self.active.is_none() {
            self.active = self.tabs.iter().find(|t| t.id != SHELL).map(|t| t.id);
        }
        if let Some(id) = self.active {
            self.create_view(id);
        }
    }
    /// A tab that keeps its URL and title but has no view until it is selected.
    fn push_sleeping(&mut self, url: &str, title: &str) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.tabs.push(Tab { suspended: true, ..Tab::new(id, url.into(), title.into()) });
        id
    }
    fn restore_tabs(&mut self, tabs: Vec<(String, String)>) -> (usize, usize) {
        let mut open: std::collections::HashSet<String> = self.tabs.iter().map(|t| t.url.clone()).collect();
        let (mut added, mut already) = (0, 0);
        for (url, title) in tabs.into_iter().take(1000) {
            if !is_web(&url) {
                continue;
            }
            if !open.insert(url.clone()) {
                already += 1;
                continue;
            }
            self.push_sleeping(&url, &title);
            added += 1;
        }
        if added > 0 {
            self.session_dirty = true;
            if self.active.is_none() {
                self.active = self.tabs.iter().find(|t| t.id != SHELL && t.id != GLANCE).map(|t| t.id);
                if let Some(id) = self.active {
                    self.create_view(id);
                }
            }
            self.layout();
            self.push(false);
        }
        (added, already)
    }
}
struct CookieBatch {
    manager: CookieManager,
    results: Mutex<Vec<bool>>,
    reply: Mutex<Option<Reply<Vec<bool>>>>,
}
// CEF releases each callback after it runs, so the last release reports the batch.
impl Drop for CookieBatch {
    fn drop(&mut self) {
        self.manager.flush_store(None);
        if let Some(reply) = self.reply.get_mut().unwrap().take() {
            let _ = reply.send(std::mem::take(self.results.get_mut().unwrap()));
        }
    }
}
wrap_set_cookie_callback! {struct CookieSet {batch:Arc<CookieBatch>,index:usize} impl SetCookieCallback {
    fn on_complete(&self,success:i32){self.batch.results.lock().unwrap()[self.index]=success!=0;}
}}
fn set_cookies(cookies: Vec<CookieWrite>, reply: Reply<Vec<bool>>) {
    let Some(manager) = cookie_manager_get_global_manager(None) else {
        let _ = reply.send(vec![false; cookies.len()]);
        return;
    };
    let batch = Arc::new(CookieBatch { manager: manager.clone(), results: Mutex::new(vec![false; cookies.len()]), reply: Mutex::new(Some(reply)) });
    for (index, c) in cookies.iter().enumerate() {
        let cookie = Cookie {
            name: CefString::from(c.name.as_str()),
            value: CefString::from(c.value.as_str()),
            domain: CefString::from(c.domain.as_str()),
            path: CefString::from(c.path.as_str()),
            secure: c.secure.into(),
            httponly: c.httponly.into(),
            creation: Basetime { val: c.creation },
            last_access: Basetime { val: c.last_access },
            has_expires: c.expires.is_some().into(),
            expires: Basetime { val: c.expires.unwrap_or(0) },
            same_site: CookieSameSite::from(match c.same_site {
                1 => sys::cef_cookie_same_site_t::CEF_COOKIE_SAME_SITE_NO_RESTRICTION,
                2 => sys::cef_cookie_same_site_t::CEF_COOKIE_SAME_SITE_LAX_MODE,
                3 => sys::cef_cookie_same_site_t::CEF_COOKIE_SAME_SITE_STRICT_MODE,
                _ => sys::cef_cookie_same_site_t::CEF_COOKIE_SAME_SITE_UNSPECIFIED,
            }),
            priority: match c.priority {
                -1 => CookiePriority::LOW,
                1 => CookiePriority::HIGH,
                _ => CookiePriority::MEDIUM,
            },
            ..Default::default()
        };
        let mut callback = CookieSet::new(batch.clone(), index);
        manager.set_cookie(Some(&CefString::from(c.url.as_str())), Some(&cookie), Some(&mut callback));
    }
}
struct CookieSink {
    cookies: Mutex<Vec<CookieStored>>,
    reply: Mutex<Option<Reply<Vec<CookieStored>>>>,
}
impl Drop for CookieSink {
    fn drop(&mut self) {
        if let Some(reply) = self.reply.get_mut().unwrap().take() {
            let _ = reply.send(std::mem::take(self.cookies.get_mut().unwrap()));
        }
    }
}
wrap_cookie_visitor! {struct CookieList {sink:Arc<CookieSink>} impl CookieVisitor {
    fn visit(&self,cookie:Option<&Cookie>,_count:i32,_total:i32,_delete:Option<&mut i32>)->i32{
        if let Some(c)=cookie{self.sink.cookies.lock().unwrap().push(CookieStored{name:c.name.to_string(),value:c.value.to_string(),domain:c.domain.to_string(),path:c.path.to_string(),secure:c.secure!=0,httponly:c.httponly!=0,same_site:c.same_site.get_raw() as i32,expires:(c.has_expires!=0).then_some(c.expires.val)});}
        1
    }
}}
impl Manager {
    /// What the shell shows for a tab. Offers carry origins and usernames, never passwords.
    fn pw_offer(&mut self, tab: u64, offer: Option<Json>) {
        match &offer {
            Some(o) => self.pw_offers.insert(tab, o.clone()),
            None => self.pw_offers.remove(&tab),
        };
        self.shell(&format!("window.atriumShell?.password?.({tab}, {})", offer.unwrap_or(Json::Null)));
    }
    fn pw_store(&self, work: impl FnOnce() -> anyhow::Result<PwEvent> + Send + 'static) {
        let bridge = self.bridge.clone();
        std::thread::spawn(move || {
            let event = work().unwrap_or_else(|e| PwEvent::Done { message: e.to_string() });
            bridge.send(Event::Password(event));
        });
    }
    fn pw_step(&mut self, tab: u64, step: PwStep, error: Option<String>, data: Json) {
        if let Some(e) = error.clone().or_else(|| error_of(&data)) {
            eprintln!("[passwords] tab {tab}: {e}");
        }
        match step {
            PwStep::Tree => {
                if let Some(frame) = data["frameTree"]["frame"]["id"].as_str() {
                    self.cdp(tab, "Page.createIsolatedWorld", json!({ "frameId": frame, "worldName": PW_WORLD }), Pending::Pw(tab, PwStep::World));
                }
            }
            PwStep::World => {
                if let Some(ctx) = data["executionContextId"].as_i64() {
                    self.cdp(tab, "Runtime.evaluate", json!({ "expression": PASSWORDS_JS, "contextId": ctx }), Pending::Pw(tab, PwStep::Done));
                }
            }
            PwStep::Done => {}
            PwStep::Fill(what) => {
                let value = &data["result"]["value"];
                let message = match (error, value["filled"].as_u64()) {
                    (None, Some(n)) if n > 0 => if what == "fill" { "Filled saved password" } else { "Filled a generated password" },
                    _ if value["error"] == "origin" => "This page changed sites; nothing was filled",
                    _ => "No password field to fill on this page",
                };
                self.toast(message);
            }
        }
    }
    /// Messages from the isolated world. Its `location.origin` must match the tab's committed URL.
    fn pw_message(&mut self, tab: u64, ctx: i64, msg: Json) {
        let Some(origin) = self.tab(tab).and_then(|t| crate::passwords::origin_of(&t.url)) else { return };
        if msg["origin"].as_str() != Some(origin.as_str()) {
            return;
        }
        match msg["type"].as_str() {
            Some("forms") => {
                let (login, signup) = (msg["login"] == true, msg["signup"] == true);
                self.pw_forms.insert(tab, PwForms { ctx, origin: origin.clone(), login, signup });
                if !login && !signup {
                    if self.pw_offers.get(&tab).is_some_and(|o| matches!(o["kind"].as_str(), Some("fill" | "generate"))) {
                        self.pw_offer(tab, None);
                    }
                    return;
                }
                self.pw_store(move || Ok(PwEvent::Found { tab, ctx, login, signup, accounts: crate::passwords::with(|s| s.for_origin(&origin))? }));
            }
            Some("submit") => {
                let username = msg["username"].as_str().unwrap_or("").chars().take(1024).collect::<String>();
                let Some(password) = msg["password"].as_str().filter(|p| !p.is_empty() && p.len() <= 4096).map(str::to_string) else { return };
                self.pw_store(
                    move || {
                        let status = crate::passwords::with(|s| {
                            Ok(if s.never().contains(&origin) { crate::passwords::Saved::Unchanged } else { s.check(&origin, &username, &password)? })
                        })?;
                        Ok(PwEvent::Submitted { tab, origin, username, password, status })
                    }
                );
            }
            _ => {}
        }
    }
    fn pw_event(&mut self, event: PwEvent) {
        use crate::passwords::Saved;
        match event {
            PwEvent::Found { tab, ctx, login, signup, accounts } => {
                let Some(forms) = self.pw_forms.get(&tab).filter(|f| f.ctx == ctx) else { return };
                if self.pw_offers.get(&tab).is_some_and(|o| matches!(o["kind"].as_str(), Some("save" | "update"))) {
                    return;
                }
                let offer = if login && !accounts.is_empty() {
                    Some(json!({ "kind": "fill", "origin": forms.origin, "accounts": accounts.iter().map(|l| json!({ "id": l.id, "username": l.username })).collect::<Vec<_>>() }))
                } else if signup {
                    Some(json!({ "kind": "generate", "origin": forms.origin }))
                } else {
                    None
                };
                self.pw_offer(tab, offer);
            }
            PwEvent::Submitted { tab, origin, username, password, status } => {
                if status == Saved::Unchanged {
                    return;
                }
                let kind = if status == Saved::New { "save" } else { "update" };
                self.pw_offer(tab, Some(json!({ "kind": kind, "origin": origin, "username": username })));
                self.pw_pending.insert(tab, PwPending { origin, username, password });
            }
            PwEvent::Fill { tab, ctx, login } => {
                let Some(forms) = self.pw_forms.get(&tab).filter(|f| f.ctx == ctx && f.origin == login.origin) else {
                    return self.toast("This page changed; nothing was filled");
                };
                let args = json!({ "origin": forms.origin, "username": login.username, "password": login.password });
                self.cdp(tab, "Runtime.evaluate", json!({ "expression": format!("window.__atriumPw.fill({args})"), "contextId": ctx, "returnByValue": true }), Pending::Pw(tab, PwStep::Fill("fill")));
            }
            PwEvent::Done { message } => self.toast(&message),
        }
    }
    fn pw_command(&mut self, tab: u64, m: &Json) {
        let forms = self.pw_forms.get(&tab).cloned().filter(|f| self.tab(tab).and_then(|t| crate::passwords::origin_of(&t.url)).as_ref() == Some(&f.origin));
        match m["cmd"].as_str().unwrap_or("") {
            "passwordFill" => {
                let Some(forms) = forms.filter(|f| f.login) else { return self.toast("No login form on this page") };
                let id = m["id"].as_str().map(str::to_string);
                self.pw_store(
                    move || {
                        let login = crate::passwords::with(|s| {
                            let id = match id {
                                Some(id) => id,
                                None => s.for_origin(&forms.origin)?.first().map(|l| l.id.clone()).ok_or_else(|| anyhow!("No password is saved for this site"))?,
                            };
                            let login = s.get(&id)?;
                            anyhow::ensure!(login.origin == forms.origin, "That password is saved for a different site");
                            Ok(login)
                        })?;
                        Ok(PwEvent::Fill { tab, ctx: forms.ctx, login })
                    }
                );
            }
            "passwordGenerate" => {
                let Some(forms) = forms.filter(|f| f.signup) else { return self.toast("No new-password field on this page") };
                let password = match crate::passwords::generate() {
                    Ok(p) => p,
                    Err(e) => return self.toast(&e.to_string()),
                };
                self.pw_offer(tab, None);
                let args = json!({ "origin": forms.origin, "password": password });
                self.cdp(tab, "Runtime.evaluate", json!({ "expression": format!("window.__atriumPw.generate({args})"), "contextId": forms.ctx, "returnByValue": true }), Pending::Pw(tab, PwStep::Fill("generate")));
            }
            "passwordSave" => {
                let pending = self.pw_pending.remove(&tab);
                if self.pw_offers.get(&tab).is_some_and(|o| matches!(o["kind"].as_str(), Some("save" | "update"))) {
                    self.pw_offer(tab, None);
                }
                let Some(p) = pending else { return };
                match m["action"].as_str() {
                    Some("save") => self.pw_store(
                        move || {
                            let (_, how) = crate::passwords::with(|s| s.save(&p.origin, &p.username, &p.password))?;
                            Ok(PwEvent::Done { message: if how == crate::passwords::Saved::Updated { "Password updated" } else { "Password saved" }.into() })
                        }
                    ),
                    Some("never") => self.pw_store(
                        move || {
                            crate::passwords::with(|s| s.set_never(&p.origin, true))?;
                            Ok(PwEvent::Done { message: "Passwords for this site won't be saved".into() })
                        }
                    ),
                    _ => {}
                }
            }
            _ => {
                if self.pw_offers.get(&tab).is_some_and(|o| matches!(o["kind"].as_str(), Some("fill" | "generate"))) {
                    self.pw_offer(tab, None);
                }
            }
        }
    }
}
fn error_of(data: &Json) -> Option<String> {
    data["exceptionDetails"]["exception"]["description"].as_str().or(data["exceptionDetails"]["text"].as_str()).map(str::to_string)
}
