//! The native browser: one window, a transparent shell webview that draws the
//! browser UI (sidebar, spaces, command bar, new tab page, assistant), native
//! Liquid Glass panes behind it, and one WKWebView per tab laid over the
//! shell's content area. Everything here runs on the main thread; other
//! threads talk to it through [`Handle`].

use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{anyhow, Result};
use muda::{
    accelerator::{Accelerator, Code, Modifiers},
    AboutMetadata, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu,
};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSGlassEffectView, NSWindowOrderingMode};
use serde::Serialize;
use serde_json::{json, Value};
use tao::{
    dpi::{LogicalPosition, LogicalSize},
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
    platform::macos::WindowBuilderExtMacOS,
    window::{Window, WindowBuilder},
};
use tokio::sync::oneshot;
use wry::{NewWindowResponse, PageLoadEvent, Rect, WebView, WebViewBuilder, WebViewExtMacOS};

const PAGE_JS: &str = include_str!("../assets/page.js");

/// Keeps the URL in sync with single-page-app navigations, reports the
/// favicon, and adds ⌥-click (Glance) and ⌘-click (background tab) on links.
const NAV_HOOK_JS: &str = r#"(() => {
  if (window !== top || window.__atriumNavHook) return;
  window.__atriumNavHook = true;
  const post = (m) => { try { window.ipc.postMessage(JSON.stringify(m)); } catch {} };
  const nav = () => post({ __atriumNav: location.href });
  for (const k of ['pushState', 'replaceState']) {
    const orig = history[k];
    history[k] = function (...a) { const r = orig.apply(this, a); setTimeout(nav, 0); return r; };
  }
  addEventListener('popstate', nav);
  addEventListener('hashchange', nav);
  const icon = () => { const l = document.querySelector('link[rel~="icon"], link[rel="apple-touch-icon"]'); if (l && l.href) post({ __atriumIcon: l.href }); };
  addEventListener('DOMContentLoaded', icon);
  addEventListener('load', icon);
  addEventListener('click', (e) => {
    if (!e.isTrusted || !(e.altKey || e.metaKey)) return;
    const a = e.target.closest && e.target.closest('a[href]');
    if (!a || a.href.startsWith('javascript:')) return;
    e.preventDefault(); e.stopImmediatePropagation();
    post(e.altKey ? { __atriumGlance: a.href } : { __atriumBackground: a.href });
  }, true);
})();"#;

const SAFARI_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";

/// Latest frozen page images (JPEG) by slot, served to the shell over HTTP;
/// pushing megabytes through evaluate_script crashes WebKit's content process.
pub static FROZEN: std::sync::LazyLock<Mutex<HashMap<String, Vec<u8>>>> = std::sync::LazyLock::new(Default::default);

/// Pseudo tab id used for events from the Glance preview webview.
const GLANCE: u64 = u64::MAX;
/// Pseudo tab id addressing the shell itself (debugging).
pub const SHELL: u64 = u64::MAX - 1;

pub type Reply<T> = oneshot::Sender<T>;

#[derive(Debug, Clone, Serialize)]
pub struct TabInfo {
    pub id: u64,
    pub kind: &'static str,
    pub title: String,
    pub url: String,
    pub loading: bool,
    pub icon: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TabsState {
    pub tabs: Vec<TabInfo>,
    pub active: Option<u64>,
}

/// Primitive browser operations the agent tools are built from.
pub enum Op {
    Tabs(Reply<TabsState>),
    Shell { body: Value, reply: Reply<()> },
    Memory(Reply<Value>),
    /// Chromium-only browser commands (find, zoom, printing, downloads).
    Action { body: Value, reply: Reply<Result<Value, String>> },
    /// Background tabs open without taking focus (the agent's own pages).
    /// Scratch tabs are invisible to the user (background reads).
    NewTab { url: Option<String>, background: bool, scratch: bool, reply: Reply<u64> },
    Close { id: u64, reply: Reply<()> },
    /// The agent now works in this tab; the chat view previews it. A tab the agent
    /// `opened` stays out of the sidebar until the user opens it.
    AgentTab { id: u64, opened: bool, reply: Reply<()> },
    /// Load a URL in `tab` (or the active tab); returns the tab used.
    Navigate { tab: Option<u64>, url: String, reply: Reply<u64> },
    History { tab: Option<u64>, action: &'static str, reply: Reply<Result<(), String>> },
    /// `js` is a function body; its (awaited) return value comes back as JSON.
    /// Runs in `tab`, or the active tab.
    Eval { tab: Option<u64>, js: String, reply: Reply<Result<Value, String>> },
    /// Like `Eval`, for an agent action that should count as user activation.
    Act { tab: Option<u64>, js: String, reply: Reply<Result<Value, String>> },
    /// Setup checks sign-in without marking a tab as agent-controlled.
    InspectGoogle { tab: u64, reply: Reply<Result<Value, String>> },
    Screenshot { tab: Option<u64>, reply: Reply<Result<String, String>> },
    Cdp { tab: Option<u64>, method: String, params: Value, reply: Reply<Result<Value, String>> },
    Accessibility { assistive: Option<bool>, tree: bool, reply: Reply<Value> },
    DevTools { tab: Option<u64>, method: String, params: Value, reply: Reply<Result<Value, String>> },
}

pub enum TabEv {
    Title(String),
    LoadStarted(String),
    LoadFinished(String),
    OpenNew(String),
    Ipc(String),
}

pub enum UserEvent {
    Shell(String),
    Menu(String),
    Tab { id: u64, ev: TabEv },
    Op(Op),
    Frozen { slot: &'static str, data: String },
}

/// Thread-safe way to drive the browser from tokio.
#[derive(Clone)]
pub struct Handle(Arc<Mutex<EventLoopProxy<UserEvent>>>);

impl Handle {
    pub fn open_urls(&self, urls: Vec<String>) {
        for url in urls {
            let _ = self.send(UserEvent::Op(Op::NewTab { url: Some(url), background: false, scratch: false, reply: oneshot::channel().0 }));
        }
    }
    fn send(&self, ev: UserEvent) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .send_event(ev)
            .map_err(|_| anyhow!("the browser window is closed"))
    }

    pub async fn op<T>(&self, make: impl FnOnce(Reply<T>) -> Op) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.send(UserEvent::Op(make(tx)))?;
        match tokio::time::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(_)) => Err(anyhow!("the page navigated before answering")),
            Err(_) => Err(anyhow!("timed out waiting for the browser")),
        }
    }
}

pub fn normalize_url(raw: &str) -> String {
    let u = raw.trim();
    if u.contains("://") || u.starts_with("about:") || u.starts_with("data:") {
        u.to_string()
    } else if !u.contains(' ') && (u.contains('.') || u.starts_with("localhost")) && !u.ends_with('.') {
        if u.starts_with("localhost") || u.starts_with("127.0.0.1") {
            format!("http://{u}")
        } else {
            format!("https://{u}")
        }
    } else {
        let q: String = u
            .bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
                b' ' => "+".into(),
                _ => format!("%{b:02X}"),
            })
            .collect();
        format!("https://www.google.com/search?q={q}")
    }
}

type R4 = (f64, f64, f64, f64);

fn rect_of(v: &Value) -> Option<R4> {
    Some((v["x"].as_f64()?, v["y"].as_f64()?, v["w"].as_f64()?, v["h"].as_f64()?))
}

fn wry_rect((x, y, w, h): R4) -> Rect {
    Rect {
        position: LogicalPosition::new(x, y).into(),
        size: LogicalSize::new(w.max(1.0), h.max(1.0)).into(),
    }
}

struct Tab {
    id: u64,
    title: String,
    url: String,
    loading: bool,
    icon: Option<String>,
    view: Option<WebView>,
    scratch: bool,
}

impl Tab {
    fn info(&self) -> TabInfo {
        TabInfo {
            id: self.id,
            kind: if self.scratch { "scratch" } else if self.view.is_some() { "web" } else { "newtab" },
            title: self.title.clone(),
            url: self.url.clone(),
            loading: self.loading,
            icon: self.icon.clone(),
        }
    }
}

struct Glance {
    view: WebView,
    url: String,
    title: String,
    loading: bool,
}

struct Browser {
    window: Window,
    shell: WebView,
    proxy: EventLoopProxy<UserEvent>,
    tabs: Vec<Tab>,
    active: Option<u64>,
    /// Tab shown in the right pane of a split view.
    split: Option<u64>,
    glance: Option<Glance>,
    next_id: u64,
    main_rect: R4,
    split_rect: Option<R4>,
    glance_rect: Option<R4>,
    /// The shell is showing something over the content area (chat page, new tab page, overlay).
    covered: bool,
    /// Outstanding Eval requests: nonce -> (tab, reply).
    pending: HashMap<String, (u64, Reply<Result<Value, String>>)>,
}

impl Browser {
    fn tab(&mut self, id: u64) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    fn view_of(&self, id: u64) -> Option<&WebView> {
        if id == SHELL {
            return Some(&self.shell);
        }
        if id == GLANCE {
            return self.glance.as_ref().map(|g| &g.view);
        }
        self.tabs.iter().find(|t| t.id == id).and_then(|t| t.view.as_ref())
    }

    fn state(&self) -> TabsState {
        TabsState { tabs: self.tabs.iter().map(Tab::info).collect(), active: self.active }
    }

    fn push_state(&self, reveal: bool) {
        let mut v = serde_json::to_value(self.state()).unwrap_or_default();
        v["reveal"] = json!(reveal);
        v["split"] = json!(self.split);
        v["glance"] = match &self.glance {
            Some(g) => json!({ "url": g.url, "title": g.title, "loading": g.loading }),
            None => Value::Null,
        };
        self.shell_js(&format!("atriumShell.state({v})"));
    }

    fn shell_js(&self, call: &str) {
        let _ = self.shell.evaluate_script(&format!("window.atriumShell && {call}"));
    }

    fn shell_cmd(&self, name: &str) {
        self.shell_js(&format!("atriumShell.cmd({})", json!(name)));
    }

    fn apply_layout(&self) {
        let split = self.split.filter(|s| Some(*s) != self.active && self.split_rect.is_some());
        for t in &self.tabs {
            let Some(v) = &t.view else { continue };
            let (rect, show) = if Some(t.id) == self.active {
                (self.main_rect, !self.covered)
            } else if Some(t.id) == split {
                (self.split_rect.unwrap(), !self.covered)
            } else {
                (self.main_rect, false)
            };
            let _ = v.set_bounds(wry_rect(rect));
            let _ = v.set_visible(show && rect.2 > 1.0);
        }
        if let Some(g) = &self.glance {
            match self.glance_rect {
                Some(r) => {
                    let _ = g.view.set_bounds(wry_rect(r));
                    let _ = g.view.set_visible(true);
                }
                None => {
                    let _ = g.view.set_visible(false);
                }
            }
        }
    }

    fn build_view(&self, id: u64, url: &str, rect: R4) -> Result<WebView> {
        let p = self.proxy.clone();
        let send = move |ev: TabEv| {
            let _ = p.send_event(UserEvent::Tab { id, ev });
        };
        let (s1, s2, s3, s4) = (send.clone(), send.clone(), send.clone(), send);
        let view = WebViewBuilder::new()
            .with_url(url)
            .with_bounds(wry_rect(rect))
            .with_visible(false)
            .with_user_agent(SAFARI_UA)
            .with_devtools(true)
            .with_accept_first_mouse(true)
            .with_back_forward_navigation_gestures(true)
            .with_initialization_script_for_main_only(format!("{PAGE_JS}\n{NAV_HOOK_JS}"), true)
            .with_ipc_handler(move |req| s1(TabEv::Ipc(req.into_body())))
            .with_document_title_changed_handler(move |t| s2(TabEv::Title(t)))
            .with_on_page_load_handler(move |ev, url| {
                s3(match ev {
                    PageLoadEvent::Started => TabEv::LoadStarted(url),
                    PageLoadEvent::Finished => TabEv::LoadFinished(url),
                })
            })
            .with_new_window_req_handler(move |url, _| {
                s4(TabEv::OpenNew(url));
                NewWindowResponse::Deny
            })
            .build_as_child(&self.window)?;
        round_corners(&view, 10.0);
        Ok(view)
    }

    /// Opens a tab. Foreground tabs become active; background ones don't.
    fn new_tab(&mut self, url: Option<String>, foreground: bool) -> u64 {
        // Reuse an untouched new tab instead of piling them up.
        if url.is_none() {
            if let Some(t) = self.tabs.iter().find(|t| t.view.is_none() && !t.scratch) {
                let id = t.id;
                self.active = Some(id);
                self.after_change(true);
                return id;
            }
        }
        self.next_id += 1;
        let id = self.next_id;
        let mut tab = Tab { id, title: String::new(), url: "about:blank".into(), loading: false, icon: None, view: None, scratch: false };
        if let Some(u) = url {
            let u = normalize_url(&u);
            match self.build_view(id, &u, self.main_rect) {
                Ok(v) => {
                    tab.view = Some(v);
                    tab.url = u;
                    tab.loading = true;
                }
                Err(e) => eprintln!("could not create tab: {e}"),
            }
        }
        self.tabs.push(tab);
        if foreground || self.active.is_none() {
            self.active = Some(id);
        }
        self.after_change(foreground);
        id
    }

    /// A hidden tab the agent reads in the background; never shown.
    fn scratch_tab(&mut self, url: String) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        let url = normalize_url(&url);
        let view = self.build_view(id, &url, self.main_rect).ok();
        self.tabs.push(Tab { id, title: String::new(), url, loading: true, icon: None, view, scratch: true });
        id
    }

    fn navigate(&mut self, raw: &str) -> u64 {
        self.navigate_in(self.active, raw, true)
    }

    fn navigate_in(&mut self, tab: Option<u64>, raw: &str, reveal: bool) -> u64 {
        let url = normalize_url(raw);
        let Some(id) = tab.filter(|id| self.tabs.iter().any(|t| t.id == *id)) else {
            return self.new_tab(Some(url), reveal);
        };
        if let Some(v) = self.view_of(id) {
            let _ = v.load_url(&url);
            let t = self.tab(id).unwrap();
            t.loading = true;
            t.url = url;
        } else {
            match self.build_view(id, &url, self.main_rect) {
                Ok(v) => {
                    let t = self.tab(id).unwrap();
                    t.view = Some(v);
                    t.url = url;
                    t.loading = true;
                }
                Err(e) => eprintln!("could not load {url}: {e}"),
            }
        }
        self.after_change(reveal);
        id
    }

    fn close(&mut self, id: u64) {
        let Some(pos) = self.tabs.iter().position(|t| t.id == id) else { return };
        self.tabs.remove(pos); // dropping the WebView removes it from the window
        self.pending.retain(|_, (tab, _)| *tab != id);
        if self.split == Some(id) {
            self.split = None;
        }
        if self.active == Some(id) {
            let visible: Vec<u64> = self.tabs.iter().filter(|t| !t.scratch).map(|t| t.id).collect();
            self.active = self.split.take().or_else(|| visible.get(pos.min(visible.len().saturating_sub(1))).copied());
        }
        if self.tabs.iter().all(|t| t.scratch) {
            self.new_tab(None, true);
        } else {
            self.after_change(true);
        }
    }

    fn activate(&mut self, id: u64) {
        if !self.tabs.iter().any(|t| t.id == id) {
            return;
        }
        if self.split == Some(id) {
            // Clicking the split partner just moves focus to it.
            self.split = self.active;
        }
        self.active = Some(id);
        self.after_change(true);
        if let Some(v) = self.view_of(id) {
            let _ = v.focus();
        }
    }

    fn cycle(&mut self, step: isize) {
        let ids: Vec<u64> = self.tabs.iter().filter(|t| !t.scratch).map(|t| t.id).collect();
        let Some(pos) = ids.iter().position(|id| Some(*id) == self.active) else { return };
        let n = ids.len() as isize;
        let next = ((pos as isize + step) % n + n) % n;
        let id = ids[next as usize];
        self.activate(id);
    }

    fn after_change(&self, reveal: bool) {
        self.apply_layout();
        self.push_state(reveal);
    }

    fn history(&mut self, action: &str) -> Result<(), String> {
        self.history_in(self.active, action)
    }

    fn history_in(&mut self, tab: Option<u64>, action: &str) -> Result<(), String> {
        let v = tab.and_then(|id| self.view_of(id)).ok_or("this tab has no page yet")?;
        let r = match action {
            "back" => v.evaluate_script("history.back()"),
            "forward" => v.evaluate_script("history.forward()"),
            _ => v.reload(),
        };
        r.map_err(|e| e.to_string())
    }

    fn eval(&mut self, tab: Option<u64>, js: String, reply: Reply<Result<Value, String>>) {
        let Some(id) = tab.or(self.active) else {
            let _ = reply.send(Err("No tab is open".into()));
            return;
        };
        let Some(view) = self.view_of(id) else {
            let _ = reply.send(Err("That tab is an empty new tab. Navigate somewhere first.".into()));
            return;
        };
        let nonce = uuid::Uuid::new_v4().to_string();
        let script = format!(
            "(async () => {{ const __n = {n}; const __post = (m) => window.ipc.postMessage(JSON.stringify(m)); \
             try {{ const __v = await (async () => {{ {js}\n }})(); __post({{ __atriumReply: __n, ok: true, value: __v === undefined ? null : __v }}); }} \
             catch (e) {{ __post({{ __atriumReply: __n, ok: false, error: String(e && e.message || e) }}); }} }})()",
            n = json!(nonce)
        );
        match view.evaluate_script(&script) {
            Ok(()) => {
                self.pending.insert(nonce, (id, reply));
            }
            Err(e) => {
                let _ = reply.send(Err(e.to_string()));
            }
        }
    }

    /// Snapshot the visible page(s) so the shell can draw glass overlays
    /// (command bar, Glance) over a still image of them.
    fn freeze(&self) {
        let mut slots = vec![("main", self.active)];
        if self.split_rect.is_some() {
            slots.push(("split", self.split));
        }
        for (slot, id) in slots {
            let Some(v) = id.and_then(|id| self.view_of(id)) else { continue };
            let p = self.proxy.clone();
            snapshot(v, move |r| {
                if let Ok(data) = r {
                    let _ = p.send_event(UserEvent::Frozen { slot, data });
                }
            });
        }
    }

    fn open_glance(&mut self, url: String) {
        let url = normalize_url(&url);
        if let Some(g) = &mut self.glance {
            let _ = g.view.load_url(&url);
            g.url = url;
            g.loading = true;
        } else {
            let rect = self.glance_rect.unwrap_or(self.main_rect);
            match self.build_view(GLANCE, &url, rect) {
                Ok(view) => self.glance = Some(Glance { view, url, title: String::new(), loading: true }),
                Err(e) => return eprintln!("glance: {e}"),
            }
        }
        self.shell_cmd("glanceOpened");
        self.after_change(false);
    }

    /// Turn the Glance preview into a real tab (optionally as a split).
    fn promote_glance(&mut self, as_split: bool) {
        let Some(g) = self.glance.take() else { return };
        self.next_id += 1;
        let id = self.next_id;
        // The webview keeps reporting events under the GLANCE id, so reload
        // the page in a fresh view owned by the new tab.
        let url = g.view.url().unwrap_or(g.url);
        drop(g.view);
        let view = self.build_view(id, &url, self.main_rect).ok();
        self.tabs.push(Tab { id, title: g.title, url, loading: true, icon: None, view, scratch: false });
        if as_split {
            self.split = Some(id);
        } else {
            self.active = Some(id);
        }
        self.glance_rect = None;
        self.after_change(true);
    }

    fn handle_op(&mut self, op: Op) {
        // Tell the shell which tab the agent is touching so it can show it.
        let touched = match &op {
            Op::Tabs(_) | Op::InspectGoogle { .. } | Op::Shell { .. } | Op::Memory(_) | Op::Cdp { .. } | Op::Accessibility { .. } | Op::Action { .. } | Op::DevTools { .. } => None,
            Op::Eval { tab, .. } | Op::Act { tab, .. } | Op::Navigate { tab, .. } | Op::History { tab, .. } | Op::Screenshot { tab, .. } => Some(tab.or(self.active)),
            _ => Some(None),
        };
        match op {
            Op::Shell { body, reply } => { self.handle_shell(&body.to_string()); let _ = reply.send(()); }
            Op::Memory(reply) => { let _ = reply.send(json!({"engine":"WebKit","tabs":self.tabs.len()})); }
            Op::Cdp { reply, .. } => { let _ = reply.send(Err("DevTools methods need the Chromium engine".into())); }
            Op::Accessibility { reply, .. } => { let _ = reply.send(json!({"error":"Accessibility state needs the Chromium engine"})); }
            Op::Action { body: _, reply } => { let _ = reply.send(Err("This command needs the Chromium engine".into())); }
            Op::Tabs(r) => {
                let _ = r.send(self.state());
            }
            Op::NewTab { url, background, scratch, reply } => {
                let id = if scratch { self.scratch_tab(url.unwrap_or_default()) } else { self.new_tab(url, !background) };
                if !scratch {
                    self.shell_js(&format!("atriumShell.agent({id})"));
                }
                let _ = reply.send(id);
            }
            Op::Close { id, reply } => {
                self.close(id);
                let _ = reply.send(());
            }
            Op::AgentTab { id, opened, reply } => {
                self.shell_js(&format!("atriumShell.agent({id}, {opened})"));
                let _ = reply.send(());
            }
            Op::Navigate { tab, url, reply } => {
                let id = self.navigate_in(tab.or(self.active), &url, tab.is_none());
                let _ = reply.send(id);
            }
            Op::History { tab, action, reply } => {
                let r = self.history_in(tab.or(self.active), action);
                let _ = reply.send(r);
            }
            Op::Eval { tab, js, reply } | Op::Act { tab, js, reply } => self.eval(tab, js, reply),
            Op::InspectGoogle { tab, reply } => self.eval(Some(tab),
                "return location.hostname === 'mail.google.com' && location.pathname.startsWith('/mail/u/') && !!document.querySelector('[gh=cm], [role=navigation], a[href*=\"#inbox\"]')".into(), reply),
            Op::Screenshot { tab, reply } => match tab.or(self.active).and_then(|id| self.view_of(id)) {
                Some(v) => {
                    let reply = RefCell::new(Some(reply));
                    snapshot(v, move |r| {
                        if let Some(reply) = reply.borrow_mut().take() {
                            let _ = reply.send(r);
                        }
                    });
                }
                None => {
                    let _ = reply.send(Err("Nothing to capture: that tab has no page".into()));
                }
            },
            Op::DevTools { reply, .. } => { let _ = reply.send(Err("DevTools methods require the Chromium engine".into())); }
        }
        if let Some(Some(id)) = touched {
            let scratch = self.tabs.iter().any(|t| t.id == id && t.scratch);
            if id != SHELL && !scratch {
                self.shell_js(&format!("atriumShell.agent({id})"));
            }
        }
    }

    fn handle_tab(&mut self, id: u64, ev: TabEv) {
        if id == GLANCE {
            return self.handle_glance(ev);
        }
        match ev {
            TabEv::Title(title) => {
                if let Some(t) = self.tab(id) {
                    t.title = title;
                }
                self.push_state(false);
            }
            TabEv::LoadStarted(url) => {
                if let Some(t) = self.tab(id) {
                    t.loading = true;
                    t.url = url;
                    t.icon = None;
                }
                // Scripts waiting on the old document will never answer.
                let stale: Vec<String> = self.pending.iter().filter(|(_, (t, _))| *t == id).map(|(k, _)| k.clone()).collect();
                for k in stale {
                    if let Some((_, r)) = self.pending.remove(&k) {
                        let _ = r.send(Err("__navigated__".into()));
                    }
                }
                self.push_state(false);
            }
            TabEv::LoadFinished(url) => {
                let current = self.view_of(id).and_then(|v| v.url().ok());
                if let Some(t) = self.tab(id) {
                    t.loading = false;
                    t.url = current.unwrap_or(url);
                }
                self.push_state(false);
            }
            TabEv::OpenNew(url) => {
                self.new_tab(Some(url), true);
            }
            TabEv::Ipc(body) => {
                let Ok(v) = serde_json::from_str::<Value>(&body) else { return };
                if let Some(n) = v["__atriumReply"].as_str() {
                    if let Some((_, r)) = self.pending.remove(n) {
                        let _ = r.send(if v["ok"] == true {
                            Ok(v["value"].clone())
                        } else {
                            Err(v["error"].as_str().unwrap_or("script error").to_string())
                        });
                    }
                } else if let Some(u) = v["__atriumNav"].as_str() {
                    if let Some(t) = self.tab(id) {
                        t.url = u.to_string();
                    }
                    self.push_state(false);
                } else if let Some(u) = v["__atriumIcon"].as_str() {
                    if let Some(t) = self.tab(id) {
                        t.icon = Some(u.to_string());
                    }
                    self.push_state(false);
                } else if let Some(u) = v["__atriumGlance"].as_str() {
                    self.open_glance(u.to_string());
                } else if let Some(u) = v["__atriumBackground"].as_str() {
                    self.new_tab(Some(u.to_string()), false);
                    self.shell_js(&format!("atriumShell.toast({})", json!("Opened in background")));
                }
            }
        }
    }

    fn handle_glance(&mut self, ev: TabEv) {
        let Some(g) = &mut self.glance else { return };
        match ev {
            TabEv::Title(t) => g.title = t,
            TabEv::LoadStarted(u) => {
                g.loading = true;
                g.url = u;
            }
            TabEv::LoadFinished(u) => {
                g.loading = false;
                g.url = g.view.url().unwrap_or(u);
            }
            TabEv::OpenNew(u) => {
                self.new_tab(Some(u), true);
                return;
            }
            TabEv::Ipc(body) => {
                if let Ok(v) = serde_json::from_str::<Value>(&body) {
                    if let Some(n) = v["__atriumReply"].as_str() {
                        if let Some((_, r)) = self.pending.remove(n) {
                            let _ = r.send(Ok(v["value"].clone()));
                        }
                    }
                }
                return;
            }
        }
        self.push_state(false);
    }

    fn handle_shell(&mut self, body: &str) {
        let Ok(m) = serde_json::from_str::<Value>(body) else { return };
        if let Some(n) = m["__atriumReply"].as_str() {
            if let Some((_, r)) = self.pending.remove(n) {
                let _ = r.send(if m["ok"] == true { Ok(m["value"].clone()) } else { Err(m["error"].as_str().unwrap_or("error").to_string()) });
            }
            return;
        }
        match m["cmd"].as_str().unwrap_or("") {
            "log" => eprintln!("[shell] {}", m["msg"].as_str().unwrap_or("")),
            "ready" => {
                if self.tabs.is_empty() {
                    self.new_tab(None, true);
                } else {
                    self.push_state(false);
                }
            }
            "layout" => {
                if let Some(r) = rect_of(&m["main"]) {
                    self.main_rect = r;
                }
                self.split_rect = rect_of(&m["split"]);
                self.glance_rect = rect_of(&m["glance"]);
                self.apply_layout();
            }
            "cover" => {
                let on = m["on"] == true;
                if on && !self.covered {
                    // Move keyboard focus off the page before hiding it.
                    let _ = self.shell.focus();
                }
                self.covered = on;
                self.apply_layout();
            }
            "freeze" => self.freeze(),
            // Thumbnail of any tab (the live preview of the agent's page).
            "thumb" => {
                if let Some(v) = m["id"].as_u64().and_then(|id| self.view_of(id)) {
                    let p = self.proxy.clone();
                    snapshot(v, move |r| {
                        if let Ok(data) = r {
                            let _ = p.send_event(UserEvent::Frozen { slot: "pip", data });
                        }
                    });
                }
            }
            "drag" => {
                let _ = self.window.drag_window();
            }
            "zoom" => self.window.set_maximized(!self.window.is_maximized()),
            "newTab" => {
                let bg = m["background"] == true;
                self.new_tab(m["url"].as_str().map(str::to_string), !bg);
            }
            "openChrome" => {
                if let Some(url) = m["url"].as_str().filter(|u| reqwest::Url::parse(u)
                    .is_ok_and(|u| matches!(u.scheme(), "http" | "https"))) {
                    if std::process::Command::new("/usr/bin/open").args(["-a", "Google Chrome", url]).spawn().is_err() {
                        self.shell_js("atriumShell.toast('Chrome could not be opened')");
                    }
                }
            }
            "navigate" => {
                if let Some(u) = m["url"].as_str().filter(|u| !u.trim().is_empty()) {
                    self.navigate(u);
                }
            }
            "activate" => self.activate(m["id"].as_u64().unwrap_or(0)),
            "close" => self.close(m["id"].as_u64().unwrap_or(0)),
            "split" => {
                self.split = m["id"].as_u64().filter(|id| Some(*id) != self.active);
                self.after_change(false);
            }
            "glance" => {
                if let Some(u) = m["url"].as_str() {
                    self.open_glance(u.to_string());
                }
            }
            "glanceClose" => {
                self.glance = None;
                self.glance_rect = None;
                self.after_change(false);
            }
            "glancePromote" => self.promote_glance(m["split"] == true),
            "back" | "forward" | "reload" => {
                let _ = self.history(m["cmd"].as_str().unwrap());
            }
            _ => {}
        }
    }

    fn handle_menu(&mut self, id: &str) {
        match id {
            "new-tab" => self.shell_cmd("newChat"),
            "command-bar" => self.shell_cmd("commandBar"),
            "close-tab" => {
                if let Some(id) = self.active {
                    self.close(id);
                }
            }
            "open-location" => self.shell_cmd("editUrl"),
            "reload" | "back" | "forward" => {
                let _ = self.history(id);
            }
            "next-tab" => self.cycle(1),
            "prev-tab" => self.cycle(-1),
            "devtools" => {
                if let Some(v) = self.active.and_then(|id| self.view_of(id)) {
                    v.open_devtools();
                }
            }
            "devtools-shell" => self.shell.open_devtools(),
            // Everything else is UI the shell owns.
            other => self.shell_cmd(other),
        }
    }
}

/// The window is one Liquid Glass surface: a single NSGlassEffectView behind
/// everything in a transparent window, so it refracts what is actually behind
/// the window. (Glass layered on a vibrancy material looks muddy.)
fn window_glass(window: &Window) {
    use objc2_app_kit::{NSAutoresizingMaskOptions, NSView};
    use tao::platform::macos::WindowExtMacOS;
    let Some(mtm) = MainThreadMarker::new() else { return };
    let content = unsafe { &*(window.ns_view() as *const NSView) };
    let glass = NSGlassEffectView::new(mtm);
    glass.setFrame(content.bounds());
    glass.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
    glass.setCornerRadius(16.0);
    // Tint with the (appearance-aware) window colour so text on the glass stays
    // legible over busy content, as the system sidebars do.
    let tint = objc2_app_kit::NSColor::windowBackgroundColor().colorWithAlphaComponent(0.55);
    glass.setTintColor(Some(&tint));
    content.addSubview_positioned_relativeTo(&glass, NSWindowOrderingMode::Below, None);
}

/// Round the web content's corners so it sits inside the shell's cards.
fn round_corners(view: &WebView, radius: f64) {
    let wk = view.webview();
    wk.setWantsLayer(true);
    if let Some(layer) = wk.layer() {
        layer.setCornerRadius(radius);
        layer.setMasksToBounds(true);
    }
}

/// Capture the visible page as a base64 JPEG.
fn snapshot(view: &WebView, done: impl FnOnce(Result<String, String>) + 'static) {
    use base64::Engine;
    use block2::RcBlock;
    use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSImage};
    use objc2_foundation::{NSDictionary, NSError};

    let done = RefCell::new(Some(done));
    let block = RcBlock::new(move |img: *mut NSImage, _err: *mut NSError| {
        let Some(done) = done.borrow_mut().take() else { return };
        let result = (|| {
            let img = unsafe { img.as_ref() }.ok_or("snapshot failed")?;
            let tiff = img.TIFFRepresentation().ok_or("no image data")?;
            let rep = NSBitmapImageRep::imageRepWithData(&tiff).ok_or("bad image data")?;
            let props = NSDictionary::new();
            let jpeg = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::JPEG, &props) }
                .ok_or("could not encode JPEG")?;
            Ok::<_, &str>(base64::engine::general_purpose::STANDARD.encode(jpeg.to_vec()))
        })();
        done(result.map_err(str::to_string));
    });
    let wk = view.webview();
    unsafe { wk.takeSnapshotWithConfiguration_completionHandler(None, &block) };
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
        "Atrium",
        true,
        &[
            &PredefinedMenuItem::about(Some("About Atrium"), Some(AboutMetadata { name: Some("Atrium".into()), ..Default::default() })),
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
            &item("devtools-shell", "Inspect Browser UI", cmd_shift, Code::KeyI),
            &PredefinedMenuItem::fullscreen(None),
        ],
    )
    .unwrap();
    let window = Submenu::with_items(
        "Window",
        true,
        &[&PredefinedMenuItem::minimize(None), &PredefinedMenuItem::close_window(None)],
    )
    .unwrap();
    menu.append_items(&[&app, &file, &edit, &view, &window]).unwrap();
    menu
}

/// Extension installs ask the Chrome Web Store for packages compatible with this version.
pub fn chromium_version() -> String {
    "156.0.8078.48".into()
}

/// Creates the event loop and hands a [`Handle`] to `start` (which should spin
/// up the servers), then runs the window until it is closed.
pub fn run(ui_base: String, start: impl FnOnce(Handle), on_exit: impl FnOnce() + 'static) -> ! {
    let mut on_exit = Some(on_exit);
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    start(Handle(Arc::new(Mutex::new(proxy.clone()))));

    let menu = build_menu();
    menu.init_for_nsapp();
    let mp = Mutex::new(proxy.clone());
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let _ = mp.lock().unwrap().send_event(UserEvent::Menu(e.id.0));
    }));

    let window = WindowBuilder::new()
        .with_title("Atrium")
        .with_inner_size(LogicalSize::new(1440.0, 920.0))
        .with_min_inner_size(LogicalSize::new(760.0, 480.0))
        .with_titlebar_transparent(true)
        .with_fullsize_content_view(true)
        .with_title_hidden(true)
        .with_traffic_light_inset(LogicalPosition::new(18.0, 20.0))
        .with_transparent(true)
        .build(&event_loop)
        .expect("window");
    window_glass(&window);

    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
    let sp = proxy.clone();
    let shell = WebViewBuilder::new()
        .with_url(format!("{ui_base}/shell"))
        .with_transparent(true)
        .with_devtools(true)
        .with_accept_first_mouse(true)
        .with_bounds(wry_rect((0.0, 0.0, size.width, size.height)))
        .with_ipc_handler(move |req| {
            let _ = sp.send_event(UserEvent::Shell(req.into_body()));
        })
        .build_as_child(&window)
        .expect("shell webview");

    let mut b = Browser {
        window,
        shell,
        proxy,
        tabs: Vec::new(),
        active: None,
        split: None,
        glance: None,
        next_id: 0,
        main_rect: (0.0, 0.0, 0.0, 0.0),
        split_rect: None,
        glance_rect: None,
        covered: true,
        pending: HashMap::new(),
    };

    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        match event {
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => *flow = ControlFlow::Exit,
            Event::LoopDestroyed => {
                if let Some(f) = on_exit.take() {
                    f();
                }
            }
            Event::WindowEvent { event: WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. }, .. } => {
                let size = b.window.inner_size().to_logical::<f64>(b.window.scale_factor());
                let _ = b.shell.set_bounds(wry_rect((0.0, 0.0, size.width, size.height)));
            }
            Event::UserEvent(UserEvent::Shell(body)) => b.handle_shell(&body),
            Event::UserEvent(UserEvent::Menu(id)) => b.handle_menu(&id),
            Event::UserEvent(UserEvent::Tab { id, ev }) => b.handle_tab(id, ev),
            Event::UserEvent(UserEvent::Op(op)) => b.handle_op(op),
            Event::UserEvent(UserEvent::Frozen { slot, data }) => {
                use base64::Engine;
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                    FROZEN.lock().unwrap().insert(slot.to_string(), bytes);
                    let url = format!("/frozen/{slot}?v={}", uuid::Uuid::new_v4().simple());
                    b.shell_js(&format!("atriumShell.frozen({}, {})", json!(slot), json!(url)));
                }
            }
            _ => {}
        }
    })
}
