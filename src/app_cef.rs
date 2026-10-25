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
#[path = "chromium/mac.rs"]
mod mac;
#[path = "chromium/shell_ipc.rs"]
mod shell_ipc;
#[path = "chromium/url.rs"]
mod url;
pub use url::normalize_url;

const LIFECYCLE_JS: &str = include_str!("../assets/lifecycle.js");
const PAGE_JS: &str = include_str!("../assets/page.js");
pub const SHELL: u64 = u64::MAX - 1;
const GLANCE: u64 = u64::MAX - 2;
/// CEF identifier of the shell's browser; process messages from any other browser are ignored.
static SHELL_BROWSER: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
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
}
#[derive(Clone)]
pub struct Handle(Bridge);
impl Handle {
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
    Window(Window),
    Resize,
    Operation(Op),
    Created(u64, Browser),
    Detach(u64),
    Closed(u64),
    Title(u64, String),
    Address(u64, String),
    Loading(u64, bool),
    Loaded(u64),
    Capture(u64, bool),
    Download(u64, u32, bool),
    Popup(String),
    Command(Json),
    Quit,
    DevResult(i32, bool, Json),
    Maintenance,
}
struct Tab {
    id: u64,
    title: String,
    url: String,
    loading: bool,
    icon: Option<String>,
    scratch: bool,
    view: Option<BrowserView>,
    browser: Option<Browser>,
    registration: Option<Registration>,
    last_used: Instant,
    capture: bool,
    page_protected_until: Instant,
    agent_until: Instant,
    suspended: bool,
    suspending: bool,
    downloads: std::collections::HashSet<u32>,
}
impl Tab {
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
        }
    }
}
enum Pending {
    Eval(Reply<Result<Json, String>>),
    Screenshot(Reply<Result<String, String>>),
    Frozen(&'static str),
    Probe(u64),
}
struct Manager {
    rx: mpsc::Receiver<Event>,
    bridge: Bridge,
    base: String,
    window: Option<Window>,
    tabs: Vec<Tab>,
    active: Option<u64>,
    split: Option<u64>,
    main_rect: Rect,
    split_rect: Option<Rect>,
    glance_rect: Option<Rect>,
    covered: bool,
    policy: crate::memory::Policy,
    session_dirty: bool,
    next_id: u64,
    next_call: i32,
    pending: HashMap<i32, (Instant, u64, Pending)>,
    closing: Arc<AtomicBool>,
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
        let mut state = serde_json::to_value(self.state()).unwrap();
        state["split"] = json!(self.split);
        state["reveal"] = json!(reveal);
        state["glance"] = self
            .tab(GLANCE)
            .map(|t| json!({ "title": t.title, "url": t.url, "loading": t.loading }))
            .unwrap_or(Json::Null);
        self.shell(&format!("window.asideShell?.state({state})"));
    }
    fn shell(&self, js: &str) {
        if let Some(frame) = self
            .tab(SHELL)
            .and_then(|t| t.browser.as_ref())
            .and_then(|b| b.main_frame())
        {
            frame.execute_java_script(Some(&CefString::from(js)), None, 0);
        }
    }
    fn shell_url(&self) -> String {
        format!("{}/shell", self.base)
    }
    fn cmd(&self, command: &str) {
        self.shell(&format!("window.asideShell?.cmd({})", json!(command)));
    }
    fn create_view(&mut self, id: u64) {
        let Some(window) = &self.window else { return };
        let Some(tab) = self.tab(id) else { return };
        if tab.view.is_some() || tab.url == "about:blank" {
            return;
        }
        let mut client = HostClient::new(self.bridge.clone(), id, self.base.clone());
        let mut delegate = HostView::new();
        let mut context: Option<RequestContext> = None;
        // Renderers expose window.ipc only in a browser created with this marker.
        let mut extra = (id == SHELL).then(dictionary_value_create).flatten();
        if let Some(extra) = &extra {
            extra.set_string(
                Some(&CefString::from(shell_ipc::EXTRA_INFO_KEY)),
                Some(&CefString::from(self.shell_url().as_str())),
            );
        }
        let view = browser_view_create(
            Some(&mut client),
            Some(&CefString::from(tab.url.as_str())),
            Some(&BrowserSettings::default()),
            extra.as_mut(),
            context.as_mut(),
            Some(&mut delegate),
        );
        if let Some(view) = view {
            let mut child = View::from(&view);
            window.add_child_view(Some(&mut child));
            if let Some(tab) = self.tab_mut(id) {
                tab.view = Some(view);
                tab.suspended = false;
                tab.loading = true;
            }
        }
    }
    fn new_tab(&mut self, url: Option<String>, foreground: bool, scratch: bool) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        let now = Instant::now();
        self.tabs.push(Tab {
            id,
            title: String::new(),
            url: url
                .map(|u| normalize_url(&u))
                .unwrap_or_else(|| "about:blank".into()),
            loading: false,
            icon: None,
            scratch,
            view: None,
            browser: None,
            registration: None,
            last_used: now,
            capture: false,
            page_protected_until: now,
            agent_until: now,
            suspended: false,
            suspending: false,
            downloads: Default::default(),
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
        }
        self.create_view(id);
        self.layout();
        self.push(true);
        // Agent and API tab switches must not pull the window in front of other apps.
        if let Some(view) = self.tab(id).and_then(|t| t.view.as_ref()) {
            view.request_focus();
        }
    }
    fn close(&mut self, id: u64) {
        if id == SHELL {
            return;
        }
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
        self.tabs.retain(|t| t.id != id);
        if self.active == Some(id) {
            self.active = self
                .tabs
                .iter()
                .find(|t| t.id != SHELL && t.id != GLANCE && !t.scratch)
                .map(|t| t.id);
        }
        if self.split == Some(id) {
            self.split = None;
        }
        self.pending.retain(|_, (_, tab, _)| *tab != id);
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
            t.url = url.clone();
            t.loading = true;
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
        let size = self.window.as_ref().map(|w| w.size()).unwrap_or(Size {
            width: 1200,
            height: 800,
        });
        for t in &self.tabs {
            let Some(view) = &t.view else { continue };
            let (bounds, visible) = if t.id == SHELL {
                (
                    Rect {
                        x: 0,
                        y: 0,
                        width: size.width,
                        height: size.height,
                    },
                    true,
                )
            } else if t.id == GLANCE {
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
                        x: size.width + 64,
                        ..self.main_rect.clone()
                    },
                    true,
                )
            } else {
                (bounds, visible)
            };
            view.set_bounds(Some(&bounds));
            view.set_visible(visible.into());
        }
    }
    fn capturing(&self, tab: u64) -> bool {
        self.pending.values().any(|(_, id, pending)| {
            *id == tab && matches!(pending, Pending::Screenshot(_) | Pending::Frozen(_))
        })
    }
    fn cdp(&mut self, tab: u64, method: &str, params: Json, pending: Pending) {
        self.create_view(tab);
        let Some(host) = self
            .tab(tab)
            .and_then(|t| t.browser.as_ref())
            .and_then(|b| b.host())
        else {
            match pending {
                Pending::Eval(r) => {
                    let _ = r.send(Err(
                        "The tab is still loading. Try again in a moment.".into()
                    ));
                }
                Pending::Screenshot(r) => {
                    let _ = r.send(Err("The tab is not ready to capture".into()));
                }
                _ => {}
            }
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
        self.cdp(
            tab,
            "Runtime.evaluate",
            json!({ "expression": expression, "awaitPromise": true, "returnByValue": true, "userGesture": gesture }),
            Pending::Eval(reply),
        );
    }
    fn finish(&mut self, id: i32, success: bool, data: Json) {
        let Some((_, _tab_id, pending)) = self.pending.remove(&id) else {
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
                                "window.asideShell?.frozen({}, {})",
                                json!(slot),
                                json!(format!("/frozen/{slot}?v={}", uuid::Uuid::new_v4()))
                            ));
                        }
                    }
                }
            }
            Pending::Probe(id) => {
                if error.is_some() || data["result"]["value"] != true {
                    if let Some(tab) = self.tab_mut(id) {
                        tab.page_protected_until = Instant::now() + Duration::from_secs(180);
                    }
                }
                if error.is_none() && data["result"]["value"] == true && self.can_suspend(id) {
                    if let Some(tab) = self.tab_mut(id) {
                        tab.suspending = true;
                        if let Some(host) = tab.browser.as_ref().and_then(|b| b.host()) {
                            host.close_browser(0);
                        }
                    }
                }
            }
        }
    }
    fn operation(&mut self, op: Op) {
        match op {
            Op::Tabs(reply) => { let _ = reply.send(self.state()); }
            Op::NewTab { url, background, scratch, reply } => { let id = self.new_tab(url, !background, scratch); let _ = reply.send(id); }
            Op::Close { id, reply } => { self.close(id); let _ = reply.send(()); }
            Op::Navigate { tab, url, reply } => { let id = self.navigate(tab.or(self.active), url); let _ = reply.send(id); }
            Op::History { tab, action, reply } => {
                let result = match tab.or(self.active).and_then(|id| self.tab(id)).and_then(|t| t.browser.as_ref()) {
                    Some(b) => { match action { "back" => b.go_back(), "forward" => b.go_forward(), _ => b.reload() }; Ok(()) }, None => Err("No page is open".into())
                }; let _ = reply.send(result);
            }
            Op::Eval { tab, js, reply } => { if let Some(tab) = tab.or(self.active) { self.evaluate(tab, js, reply, false); } else { let _ = reply.send(Err("No page is open".into())); } }
            Op::Act { tab, js, reply } => { if let Some(tab) = tab.or(self.active) { self.evaluate(tab, js, reply, true); } else { let _ = reply.send(Err("No page is open".into())); } }
            Op::InspectGoogle { tab, reply } => self.evaluate(tab, "return location.hostname === 'mail.google.com' && location.pathname.startsWith('/mail/u/') && !!document.querySelector('[gh=cm],[role=navigation]')".into(), reply, false),
            Op::Screenshot { tab, reply } => { if let Some(tab) = tab.or(self.active).filter(|id| *id != SHELL) { self.cdp(tab, "Page.captureScreenshot", json!({ "format": "jpeg", "quality": 80 }), Pending::Screenshot(reply)); } else { let _ = reply.send(Err("No page is open".into())); } }
            Op::Shell { body, reply } => { HTTP_COMMANDS.fetch_add(1, Ordering::Relaxed); self.command(body); let _ = reply.send(()); }
            Op::Memory(reply) => {
                let r = &self.main_rect;
                let _ = reply.send(json!({ "engine": "Chromium / CEF", "pid": std::process::id(), "max_live_tabs": self.policy.max_live_tabs, "idle_after_seconds": self.policy.idle_after.as_secs(), "shell_ready": self.tab(SHELL).is_some_and(|t|t.browser.is_some()), "shell_view": self.tab(SHELL).is_some_and(|t|t.view.is_some()), "shell_record": self.tab(SHELL).is_some(), "live_renderers": self.tabs.iter().filter(|t|t.view.is_some()).count(), "tabs": self.tabs.iter().filter(|t| t.id != SHELL).map(|t| json!({"id":t.id,"live":t.view.is_some(),"suspended":t.suspended})).collect::<Vec<_>>(),
                    "layout": { "main": { "x": r.x, "y": r.y, "w": r.width, "h": r.height }, "covered": self.covered, "glance": self.glance_rect.is_some() },
                    "shell_ipc": { "process_messages": SHELL_MESSAGES.load(Ordering::Relaxed), "rejected_messages": REJECTED_MESSAGES.load(Ordering::Relaxed), "http_commands": HTTP_COMMANDS.load(Ordering::Relaxed) } }));
            }
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
                        }
                        "open-location" => self.cmd("editUrl"),
                        "command-bar" => self.cmd("commandBar"),
                        "back" | "forward" | "reload" => self.command(json!({"cmd":name})),
                        "devtools" | "devtools-shell" => {
                            let id = if name == "devtools-shell" {
                                Some(SHELL)
                            } else {
                                self.active
                            };
                            if let Some(host) = id
                                .and_then(|id| self.tab(id))
                                .and_then(|t| t.browser.as_ref())
                                .and_then(|b| b.host())
                            {
                                host.show_dev_tools(None, None, None, None);
                            }
                        }
                        other => self.cmd(other),
                    }
                }
            }
            "log" => eprintln!("[shell] {}", m["msg"].as_str().unwrap_or("")),
            "ready" => {
                if self.active.is_none() {
                    self.new_tab(None, true, false);
                }
                self.push(false);
            }
            "layout" => {
                if let Some(r) = rect(&m["main"]) {
                    self.main_rect = r;
                }
                self.split_rect = rect(&m["split"]);
                self.glance_rect = rect(&m["glance"]);
                self.layout();
            }
            "cover" => {
                if m["on"] == true && !self.covered {
                    if let Some(view) = self.tab(SHELL).and_then(|t| t.view.as_ref()) {
                        view.request_focus();
                    }
                }
                self.covered = m["on"] == true;
                self.layout();
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
            "back" | "forward" | "reload" => {
                if let Some(b) = self
                    .active
                    .and_then(|id| self.tab(id))
                    .and_then(|t| t.browser.as_ref())
                {
                    match m["cmd"].as_str().unwrap() {
                        "back" => b.go_back(),
                        "forward" => b.go_forward(),
                        _ => b.reload(),
                    }
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
            "zoom" => {
                if let Some(w) = &self.window {
                    if w.is_maximized() != 0 {
                        w.restore();
                    } else {
                        w.maximize();
                    }
                }
            }
            _ => {}
        }
    }
    fn event(&mut self, event: Event) {
        match event {
            Event::Init => {
                let mut delegate = HostWindow::new(self.bridge.clone(), self.closing.clone());
                window_create_top_level(Some(&mut delegate));
            }
            Event::Window(window) => {
                window.set_title(Some(&CefString::from("Aside")));
                self.window = Some(window.clone());
                let now = Instant::now();
                self.tabs.push(Tab {
                    id: SHELL,
                    title: "Aside".into(),
                    url: format!("{}/shell", self.base),
                    loading: true,
                    icon: None,
                    scratch: false,
                    view: None,
                    browser: None,
                    registration: None,
                    last_used: now,
                    capture: false,
                    page_protected_until: now,
                    agent_until: now,
                    suspended: false,
                    suspending: false,
                    downloads: Default::default(),
                });
                self.create_view(SHELL);
                self.layout();
                if mac::background() {
                    mac::keep_out_of_sight(window.window_handle());
                }
                window.show();
                if mac::background() {
                    mac::keep_out_of_sight(window.window_handle());
                }
                self.restore_session();
                let mut maintenance = Maintenance::new(self.bridge.clone());
                post_delayed_task(ThreadId::UI, Some(&mut maintenance), 10000);
            }
            Event::Created(id, browser) => {
                let mut observer = DevObserver::new(self.bridge.clone());
                let reg = browser
                    .host()
                    .and_then(|h| h.add_dev_tools_message_observer(Some(&mut observer)));
                if let Some(t) = self.tab_mut(id) {
                    t.browser = Some(browser);
                    t.registration = reg;
                }
                self.layout();
            }
            Event::Detach(id) => {
                let view = self.tab_mut(id).and_then(|t| t.view.take());
                if let (Some(window), Some(view)) = (&self.window, view) {
                    window.remove_child_view(Some(&mut View::from(&view)));
                }
            }
            Event::Closed(id) => {
                let was_suspending = self.tab(id).is_some_and(|t| t.suspending);
                if !self.closing.load(Ordering::Relaxed)
                    && self.tab(id).is_some_and(|t| t.suspending)
                {
                    let view = self.tab_mut(id).and_then(|t| {
                        t.browser = None;
                        t.registration = None;
                        t.suspended = true;
                        t.suspending = false;
                        t.view.take()
                    });
                    if let (Some(w), Some(v)) = (&self.window, view) {
                        w.remove_child_view(Some(&mut View::from(&v)));
                    }
                    if self.active == Some(id) || self.split == Some(id) {
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
                        t.view = None;
                        t.registration = None;
                    }
                    if self.tabs.iter().all(|t| t.browser.is_none()) {
                        if let Some(window) = &self.window {
                            window.close();
                        }
                        quit_message_loop();
                    }
                }
            }
            Event::Resize => self.layout(),
            Event::Operation(op) => self.operation(op),
            Event::Command(m) => self.command(m),
            Event::Title(id, title) => {
                if let Some(t) = self.tab_mut(id) {
                    t.title = title;
                }
                self.push(false);
            }
            Event::Address(id, url) => {
                if let Some(t) = self.tab_mut(id) {
                    t.url = url;
                }
                self.push(false);
            }
            Event::Loading(id, loading) => {
                if let Some(t) = self.tab_mut(id) {
                    t.loading = loading;
                }
                self.push(false);
            }
            Event::Loaded(id) => {
                if id != SHELL {
                    if let Some(frame) = self
                        .tab(id)
                        .and_then(|t| t.browser.as_ref())
                        .and_then(|b| b.main_frame())
                    {
                        frame.execute_java_script(
                            Some(&CefString::from(
                                format!("{LIFECYCLE_JS}\n{PAGE_JS}").as_str(),
                            )),
                            None,
                            0,
                        );
                    }
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
            Event::DevResult(id, success, data) => self.finish(id, success, data),
            Event::Quit => {
                self.save_session();
                self.closing.store(true, Ordering::Relaxed);
                for t in &self.tabs {
                    if let Some(host) = t.browser.as_ref().and_then(|b| b.host()) {
                        host.close_browser(0);
                    }
                }
            }
            Event::Maintenance => self.maintain(),
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
        if let (Some(command), true) = (command_line.as_deref(), std::env::var_os("ASIDE_MOCK_KEYCHAIN").is_some()) {
            command.append_switch(Some(&CefString::from("use-mock-keychain")));
        }
        // An off-screen window counts as occluded; keep its pages painting for screenshots.
        if let (Some(command), true) = (command_line.as_deref(), mac::background()) {
            command.append_switch_with_value(Some(&CefString::from("disable-features")), Some(&CefString::from("MacWebContentsOcclusion")));
            command.append_switch(Some(&CefString::from("disable-backgrounding-occluded-windows")));
        }
        if let (Some(paths),Some(command))=(crate::CHROME_EXTENSIONS.get(),command_line) {
            if !paths.is_empty(){let value=paths.iter().map(|p|p.to_string_lossy()).collect::<Vec<_>>().join(",");command.append_switch_with_value(Some(&CefString::from("load-extension")),Some(&CefString::from(value.as_str())));}
        }
    }
}
fn browser_process_handler(&self)->Option<BrowserProcessHandler>{Some(HostProcess::new(self.bridge.clone()))}} }
wrap_browser_process_handler! {struct HostProcess {bridge:Bridge} impl BrowserProcessHandler {fn on_context_initialized(&self){self.bridge.send(Event::Init);}}}
wrap_browser_view_delegate! {struct HostView {} impl ViewDelegate {} impl BrowserViewDelegate {fn browser_runtime_style(&self)->RuntimeStyle{RuntimeStyle::ALLOY} fn chrome_toolbar_type(&self, _unused0:Option<&mut BrowserView>)->ChromeToolbarType{ChromeToolbarType::NONE}}}
wrap_window_delegate! {
    struct HostWindow {bridge:Bridge,closing:Arc<AtomicBool>}
    impl ViewDelegate {fn preferred_size(&self, _unused1:Option<&mut View>)->Size{Size{width:1280,height:820}} fn minimum_size(&self, _unused2:Option<&mut View>)->Size{Size{width:760,height:480}}
        // The window's default fill layout stretches every tab to the full window on each
        // layout pass. Restore tab bounds in the same pass, before Chromium paints.
        fn on_layout_changed(&self, _unused31:Option<&mut View>, _unused32:Option<&Rect>){
            match self.bridge.manager.upgrade().as_deref().map(Mutex::try_lock) {
                Some(Ok(manager)) => manager.layout(),
                _ => self.bridge.send(Event::Resize),
            }
        }
    }
    impl PanelDelegate {}
    impl WindowDelegate {
        fn on_window_created(&self,window:Option<&mut Window>){if let Some(w)=window{self.bridge.send(Event::Window(w.clone()));}}
        fn on_window_bounds_changed(&self, _unused3:Option<&mut Window>, _unused4:Option<&Rect>){self.bridge.send(Event::Resize);}
        fn can_close(&self, _unused5:Option<&mut Window>)->i32{if self.closing.load(Ordering::Relaxed){1}else{self.bridge.send(Event::Quit);0}}
        fn window_runtime_style(&self)->RuntimeStyle{RuntimeStyle::CHROME}
    }
}
fn is_shell_frame(frame: &Frame, base: &str) -> bool {
    frame.is_main() != 0
        && shell_ipc::is_shell_url(&CefString::from(&frame.url()).to_string(), &format!("{base}/shell"))
}
// Only the shell document in the shell's own browser carries the local API token;
// website tabs never see it, and the shell browser cannot be navigated elsewhere.
wrap_request_handler! {struct ShellRequests {bridge:Bridge,base:String} impl RequestHandler {
    fn on_before_browse(&self, _browser:Option<&mut Browser>, _frame:Option<&mut Frame>, request:Option<&mut Request>, user_gesture:i32, _is_redirect:i32)->i32{
        let url = request.map(|r| CefString::from(&r.url()).to_string()).unwrap_or_default();
        if shell_ipc::is_shell_url(&url, &format!("{}/shell", self.base)) { return 0; }
        // A link dropped on or opened from the shell becomes a tab instead.
        if user_gesture != 0 && url::is_web_url(&url) { self.bridge.send(Event::Popup(url)); }
        1
    }
    fn resource_request_handler(&self, _unused40:Option<&mut Browser>, _unused41:Option<&mut Frame>, _unused42:Option<&mut Request>, _unused43:i32, _unused44:i32, _unused45:Option<&CefString>, _unused46:Option<&mut i32>)->Option<ResourceRequestHandler>{Some(ShellResources::new(self.base.clone()))}
}}
wrap_resource_request_handler! {struct ShellResources {base:String} impl ResourceRequestHandler {
    fn on_before_resource_load(&self, _unused47:Option<&mut Browser>, frame:Option<&mut Frame>, request:Option<&mut Request>, _unused49:Option<&mut Callback>)->ReturnValue{
        if let (Some(request), Some(token), Some(frame)) = (request, crate::API_TOKEN.get(), frame) {
            if is_shell_frame(frame, &self.base) && CefString::from(&request.url()).to_string().starts_with(&format!("{}/", self.base)) {
                request.set_header_by_name(Some(&CefString::from(crate::TOKEN_HEADER)), Some(&CefString::from(token.as_str())), 1);
            }
        }
        ReturnValue::CONTINUE
    }
}}
fn is_local_api(url: &str, base: &str) -> bool {
    url.strip_prefix(base).is_some_and(|rest| rest.is_empty() || rest.starts_with(['/', '?', '#']))
}
// Tabs never load the API origin, so no page shares the shell's origin and storage.
wrap_request_handler! {struct TabRequests {base:String} impl RequestHandler {
    fn on_before_browse(&self, _browser:Option<&mut Browser>, _frame:Option<&mut Frame>, request:Option<&mut Request>, _user_gesture:i32, _is_redirect:i32)->i32{
        request.is_some_and(|r| is_local_api(&CefString::from(&r.url()).to_string(), &self.base)).into()
    }
}}
wrap_client! {struct HostClient {bridge:Bridge,id:u64,base:String} impl Client {
    fn request_handler(&self)->Option<RequestHandler>{Some(if self.id==SHELL { ShellRequests::new(self.bridge.clone(), self.base.clone()) } else { TabRequests::new(self.base.clone()) })}
    fn on_process_message_received(&self, browser:Option<&mut Browser>, frame:Option<&mut Frame>, source_process:ProcessId, message:Option<&mut ProcessMessage>)->i32{
        let Some(message) = message else { return 0 };
        if CefString::from(&message.name()).to_string() != shell_ipc::MESSAGE { return 0; }
        let trusted = self.id == SHELL
            && source_process.get_raw() == ProcessId::RENDERER.get_raw()
            && browser.is_some_and(|b| b.identifier() == SHELL_BROWSER.load(Ordering::Acquire))
            && frame.is_some_and(|f| is_shell_frame(f, &self.base));
        let body = message.argument_list().map(|args| CefString::from(&args.string(0)).to_string()).unwrap_or_default();
        match serde_json::from_str::<Json>(&body) {
            Ok(command) if trusted && body.len() <= shell_ipc::MAX_MESSAGE_BYTES && command.is_object() => {
                SHELL_MESSAGES.fetch_add(1, Ordering::Relaxed);
                self.bridge.send(Event::Command(command));
            }
            _ => {
                REJECTED_MESSAGES.fetch_add(1, Ordering::Relaxed);
                eprintln!("ignored a shell command from an untrusted frame or with an invalid body");
            }
        }
        1
    }
    fn life_span_handler(&self)->Option<LifeSpanHandler>{Some(HostLife::new(self.bridge.clone(),self.id))}
    fn display_handler(&self)->Option<DisplayHandler>{Some(HostDisplay::new(self.bridge.clone(),self.id))}
    fn load_handler(&self)->Option<LoadHandler>{Some(HostLoad::new(self.bridge.clone(),self.id))}
    fn download_handler(&self)->Option<DownloadHandler>{Some(HostDownload::new(self.bridge.clone(),self.id))}
    fn keyboard_handler(&self)->Option<KeyboardHandler>{Some(HostKeyboard::new(self.bridge.clone()))}
}}
wrap_life_span_handler! {struct HostLife {bridge:Bridge,id:u64} impl LifeSpanHandler {
    fn on_after_created(&self,browser:Option<&mut Browser>){if let Some(b)=browser{if self.id==SHELL{SHELL_BROWSER.store(b.identifier(),Ordering::Release);}self.bridge.send(Event::Created(self.id,b.clone()));}}
    fn do_close(&self,_browser:Option<&mut Browser>)->i32{self.bridge.send(Event::Detach(self.id));1}
    fn on_before_close(&self, _unused6:Option<&mut Browser>){self.bridge.send(Event::Closed(self.id));}
    fn on_before_popup(&self, _unused7:Option<&mut Browser>, _unused8:Option<&mut Frame>, _unused9:i32,url:Option<&CefString>, _unused10:Option<&CefString>, _unused11:WindowOpenDisposition, user_gesture:i32, _unused13:Option<&PopupFeatures>, _unused14:Option<&mut WindowInfo>, _unused15:Option<&mut Option<Client>>, _unused16:Option<&mut BrowserSettings>, _unused17:Option<&mut Option<DictionaryValue>>, _unused18:Option<&mut i32>)->i32{
        // Like Chrome's popup blocker, and without browser-initiated loads of file: or chrome: URLs.
        match url.map(ToString::to_string) {
            Some(u) if user_gesture != 0 && url::is_web_url(&u) => self.bridge.send(Event::Popup(u)),
            Some(u) => eprintln!("blocked a popup to {}", u.chars().take(80).collect::<String>()),
            None => {}
        }
        1
    }
}}
wrap_display_handler! {struct HostDisplay {bridge:Bridge,id:u64} impl DisplayHandler {
    fn on_title_change(&self, _unused19:Option<&mut Browser>,title:Option<&CefString>){self.bridge.send(Event::Title(self.id,title.map(ToString::to_string).unwrap_or_default()));}
    fn on_address_change(&self, _unused20:Option<&mut Browser>,frame:Option<&mut Frame>,url:Option<&CefString>){if frame.is_some_and(|f|f.is_main()!=0){self.bridge.send(Event::Address(self.id,url.map(ToString::to_string).unwrap_or_default()));}}
    fn on_media_access_change(&self, _unused21:Option<&mut Browser>,video:i32,audio:i32){self.bridge.send(Event::Capture(self.id,video!=0||audio!=0));}
}}
wrap_load_handler! {struct HostLoad {bridge:Bridge,id:u64} impl LoadHandler {
    fn on_loading_state_change(&self, _unused22:Option<&mut Browser>,loading:i32, _unused23:i32, _unused24:i32){self.bridge.send(Event::Loading(self.id,loading!=0));}
    fn on_load_end(&self, _unused25:Option<&mut Browser>,frame:Option<&mut Frame>, _unused26:i32){if frame.is_some_and(|f|f.is_main()!=0){self.bridge.send(Event::Loaded(self.id));}}
}}
wrap_dev_tools_message_observer! {struct DevObserver {bridge:Bridge} impl DevToolsMessageObserver {
    fn on_dev_tools_method_result(&self, _unused27:Option<&mut Browser>,id:i32,success:i32,result:Option<&[u8]>){self.bridge.send(Event::DevResult(id,success!=0,result.and_then(|r|serde_json::from_slice(r).ok()).unwrap_or(Json::Null)));}
}}
wrap_keyboard_handler! {struct HostKeyboard {bridge:Bridge} impl KeyboardHandler {
    fn on_pre_key_event(&self, _unused28:Option<&mut Browser>,event:Option<&KeyEvent>, _unused29:*mut u8, _unused30:Option<&mut i32>)->i32{
        let Some(e)=event else{return 0}; if e.type_!=KeyEventType::RAWKEYDOWN || e.modifiers & (1<<7)==0 {return 0;}
        let cmd=match e.windows_key_code {84|75=>Some("commandBar"),76=>Some("editUrl"),69=>Some("toggleAssistant"),78=>Some("newChat"),83=>Some("toggleSidebar"),_=>None};
        if let Some(cmd)=cmd{self.bridge.send(Event::Command(json!({"cmd":"shellCommand","name":cmd})));1}else{0}
    }
}}

pub fn run(base: String, start: impl FnOnce(Handle), on_exit: impl FnOnce() + 'static) -> ! {
    let loader = library_loader::LibraryLoader::new(
        &std::env::current_exe().expect("application path"),
        false,
    );
    assert!(
        loader.load(),
        "Run the bundled Aside.app; the Chromium framework must be next to the application"
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
            window: None,
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
            policy: crate::memory::Policy::default(),
            session_dirty: false,
            next_id: 0,
            next_call: 0,
            pending: HashMap::new(),
            closing,
        })
    });
    let bridge = manager.lock().unwrap().bridge.clone();
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
    drop(manager);
    // Chromium's teardown watchdog can terminate the process inside shutdown().
    on_exit();
    shutdown();
    std::process::exit(0)
}

wrap_download_handler! {struct HostDownload {bridge:Bridge,id:u64} impl DownloadHandler {
    fn can_download(&self,_browser:Option<&mut Browser>,_url:Option<&CefString>,_method:Option<&CefString>)->i32{1}
    fn on_before_download(&self,_browser:Option<&mut Browser>,item:Option<&mut DownloadItem>,_name:Option<&CefString>,callback:Option<&mut BeforeDownloadCallback>)->i32{if let Some(item)=item{self.bridge.send(Event::Download(self.id,item.id(),true));}if let Some(callback)=callback{callback.cont(None,1);1}else{0}}
    fn on_download_updated(&self,_browser:Option<&mut Browser>,item:Option<&mut DownloadItem>,_callback:Option<&mut DownloadItemCallback>){if let Some(item)=item{self.bridge.send(Event::Download(self.id,item.id(),item.is_in_progress()!=0));}}
}}
wrap_task! {struct Maintenance {bridge:Bridge} impl Task {fn execute(&self){self.bridge.send(Event::Maintenance);if self.bridge.manager.upgrade().is_some(){let mut next=Self::new(self.bridge.clone());post_delayed_task(ThreadId::UI,Some(&mut next),10000);}}}}
impl Manager {
    fn can_suspend(&self, id: u64) -> bool {
        self.tab(id).is_some_and(|t| {
            t.view.is_some()
                && !t.suspending
                && !t.loading
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
        let now = Instant::now();
        let candidates = self
            .tabs
            .iter()
            .filter(|t| t.id != SHELL && t.id != GLANCE && t.view.is_some() && !t.scratch)
            .map(|t| crate::memory::Candidate {
                id: t.id,
                idle: now.saturating_duration_since(t.last_used),
                visible: Some(t.id) == self.active || Some(t.id) == self.split,
                loading: t.loading,
                protected: t.capture
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
            self.cdp(id,"Runtime.evaluate",json!({"expression":format!("{LIFECYCLE_JS}; window.__asideLifecycle.canSuspend()"),"returnByValue":true}),Pending::Probe(id));
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
                    && reqwest::Url::parse(&t.url)
                        .is_ok_and(|u| matches!(u.scheme(), "http" | "https"))
            })
            .take(1000)
            .map(|t| json!({"url":t.url,"title":t.title,"active":Some(t.id)==self.active}))
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
            let Some(url) = saved["url"].as_str().filter(|url| {
                reqwest::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https"))
            }) else {
                continue;
            };
            self.next_id += 1;
            let id = self.next_id;
            let now = Instant::now();
            self.tabs.push(Tab {
                id,
                title: saved["title"].as_str().unwrap_or("").into(),
                url: url.into(),
                loading: false,
                icon: None,
                scratch: false,
                view: None,
                browser: None,
                registration: None,
                last_used: now,
                capture: false,
                page_protected_until: now,
                agent_until: now,
                suspended: true,
                suspending: false,
                downloads: Default::default(),
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
}
