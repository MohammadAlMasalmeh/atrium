//! Standard browser commands: find in page, page zoom, printing and saving, the page
//! context menu, keyboard shortcuts, reopening closed tabs, URLs from other apps, and downloads.
use super::{mac, Bridge, Event, Manager, Pending, Reply, GLANCE, SHELL};
use cef::*;
use serde::Serialize;
use serde_json::{json, Value as Json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

type Answer = Option<Reply<Result<Json, String>>>;
type Slot = Arc<Mutex<Answer>>;
const ZOOM_FILE: &str = "zoom-levels.json";
const ZOOM_STEPS: [f64; 17] = [
    25., 33., 50., 67., 75., 80., 90., 100., 110., 125., 150., 175., 200., 250., 300., 400., 500.,
];
const CLOSED_LIMIT: usize = 25;
const DOWNLOAD_LIMIT: usize = 100;

// Context menu commands, from CEF's MENU_ID_USER_FIRST.
const OPEN_LINK: i32 = 26500;
const COPY_LINK: i32 = 26501;
const SAVE_LINK: i32 = 26502;
const OPEN_IMAGE: i32 = 26503;
const SAVE_IMAGE: i32 = 26504;
const COPY_IMAGE: i32 = 26505;
const COPY_IMAGE_URL: i32 = 26506;
const SEARCH: i32 = 26507;
const SAVE_PAGE: i32 = 26508;
const INSPECT: i32 = 26509;

// cef_event_flags_t
const SHIFT: u32 = 1 << 1;
const CTRL: u32 = 1 << 2;
const ALT: u32 = 1 << 3;
const CMD: u32 = 1 << 7;
const CMD_SHIFT: u32 = CMD | SHIFT;
const CMD_ALT: u32 = CMD | ALT;
const CTRL_SHIFT: u32 = CTRL | SHIFT;

pub(super) enum Ev {
    OpenUrls(Vec<String>),
    Find { tab: u64, count: i32, active: i32, done: bool },
    BeforeDownload { item: Download, callback: BeforeDownloadCallback },
    DownloadUpdated { item: Download, control: Option<DownloadItemCallback> },
    Menu(Context),
    MenuCommand(Context, i32),
    ImageCopied(Option<Vec<u8>>),
    SavePath { tab: u64, path: Option<PathBuf> },
    Toast(String),
}

#[derive(Default)]
pub(super) struct State {
    closed: Vec<Closed>,
    zoom: Option<HashMap<String, f64>>,
    find: Find,
    downloads: Vec<Download>,
    controls: HashMap<u32, DownloadItemCallback>,
    save_as: Vec<String>,
    menu: Option<Context>,
    urls: Vec<String>,
    /// Last report from the shell: find-bar text, zoom indicator, download rows.
    pub(super) shell: Json,
}

#[derive(Clone, Serialize)]
struct Closed {
    url: String,
    title: String,
}

#[derive(Clone, Default, Serialize)]
struct Find {
    tab: Option<u64>,
    text: String,
    count: i32,
    active: i32,
    done: bool,
    bar: bool,
    #[serde(skip)]
    forward: bool,
    #[serde(skip)]
    selected: bool,
}

#[derive(Clone, Serialize)]
pub(super) struct Download {
    id: u32,
    tab: u64,
    url: String,
    original_url: String,
    name: String,
    path: String,
    mime: String,
    received: i64,
    total: i64,
    percent: i32,
    state: &'static str,
}

#[derive(Clone, Default, Serialize)]
pub(super) struct Context {
    tab: u64,
    x: i32,
    y: i32,
    link: String,
    src: String,
    page: String,
    selection: String,
    image: bool,
    editable: bool,
    items: Vec<Json>,
}

fn answer(reply: Answer, result: Result<Json, String>) {
    if let Some(reply) = reply {
        let _ = reply.send(result);
    }
}

fn text(value: &CefStringUserfree) -> String {
    CefString::from(value).to_string()
}

/// Maps a key press to a browser command. Reserved shortcuts run before the page sees the
/// key; the others go to the page first and run only if it doesn't handle them.
pub(super) fn shortcut(code: i32, modifiers: u32) -> Option<(&'static str, bool)> {
    const TABS: [&str; 8] = ["tab-1", "tab-2", "tab-3", "tab-4", "tab-5", "tab-6", "tab-7", "tab-8"];
    Some(match (modifiers & (CMD | SHIFT | CTRL | ALT), code) {
        (CMD, 84) => ("new-tab", true),
        (CMD_SHIFT, 84) => ("reopen-tab", true),
        (CMD, 87) => ("close-tab", true),
        (CMD, 78) => ("newChat", true),
        (CMD, 75) => ("command-bar", true),
        (CMD, 76) => ("open-location", true),
        (CMD, 69) => ("toggleAssistant", true),
        (CMD_SHIFT, 83) => ("toggleSidebar", true),
        (CMD, 49..=56) => (TABS[(code - 49) as usize], true),
        (CMD, 57) => ("tab-last", true),
        (CTRL, 9) | (CMD_SHIFT, 221) | (CMD_ALT, 40) => ("next-tab", true),
        (CTRL_SHIFT, 9) | (CMD_SHIFT, 219) | (CMD_ALT, 38) => ("prev-tab", true),
        (CMD_ALT, 73) => ("devtools", true),
        (CMD_SHIFT, 73) => ("devtools-shell", true),
        (CMD, 82) => ("reload", false),
        (CMD_SHIFT, 82) => ("reload-hard", false),
        (CMD, 219) => ("back", false),
        (CMD, 221) => ("forward", false),
        (CMD, 70) => ("find", false),
        (CMD, 71) => ("find-next", false),
        (CMD_SHIFT, 71) => ("find-prev", false),
        (0, 27) => ("find-close", false),
        (CMD, 187) => ("zoom-in", false),
        (CMD, 189) => ("zoom-out", false),
        (CMD, 48) => ("zoom-reset", false),
        (CMD_SHIFT, 187) => ("toggleSplit", false),
        (CMD, 80) => ("print", false),
        (CMD, 83) => ("save-page", false),
        (CMD_SHIFT, 67) => ("copyUrl", false),
        (CMD, 68) => ("bookmark", false),
        _ => return None,
    })
}

/// Windows virtual key code for an API key name, as CEF reports it in `KeyEvent`.
fn key_code(key: &str) -> Option<i32> {
    Some(match key {
        "=" | "+" => 187,
        "-" => 189,
        "[" => 219,
        "]" => 221,
        "Tab" => 9,
        "Escape" | "Esc" => 27,
        "ArrowUp" => 38,
        "ArrowDown" => 40,
        k if k.len() == 1 && k.as_bytes()[0].is_ascii_alphanumeric() => {
            k.as_bytes()[0].to_ascii_uppercase() as i32
        }
        _ => return None,
    })
}

pub(super) fn zoom_step(current: f64, direction: i32) -> f64 {
    if direction > 0 {
        ZOOM_STEPS.into_iter().find(|s| *s > current + 0.5).unwrap_or(500.)
    } else {
        ZOOM_STEPS.into_iter().rev().find(|s| *s < current - 0.5).unwrap_or(25.)
    }
}
/// Chromium zoom levels are logarithmic: each step of 1.0 scales by 1.2.
fn zoom_level(percent: f64) -> f64 {
    (percent / 100.).ln() / 1.2f64.ln()
}
fn zoom_percent(level: f64) -> f64 {
    (100. * 1.2f64.powf(level)).round()
}
/// Like Chrome, zoom is remembered per host name.
fn zoom_key(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.host_str().map(str::to_string)
}

fn external_url(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https" | "file"))
}

fn downloads_dir() -> PathBuf {
    if mac::background() {
        if let Ok(dir) = crate::data_dir() {
            return dir.join("downloads");
        }
    }
    dirs::download_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
        .unwrap_or_else(std::env::temp_dir)
}

pub(super) fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if matches!(c, '/' | ':' | '\\') || c.is_control() { '_' } else { c })
        .take(200)
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim();
    if cleaned.is_empty() { "download".into() } else { cleaned.into() }
}

/// Picks `name`, or `name (1)`, `name (2)`… so a download never overwrites a file.
pub(super) fn unique_path(dir: &Path, name: &str, taken: &[String]) -> PathBuf {
    let name = safe_name(name);
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name.as_str(), ""),
    };
    (0..)
        .map(|i| if i == 0 { dir.join(&name) } else { dir.join(format!("{stem} ({i}){ext}")) })
        .find(|p| !p.exists() && !taken.iter().any(|t| Path::new(t) == p))
        .expect("an unused file name")
}

pub(super) fn download_info(tab: u64, item: &DownloadItem, suggested: Option<&CefString>) -> Download {
    let path = text(&item.full_path());
    let name = Path::new(&path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .or_else(|| suggested.map(ToString::to_string))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| text(&item.suggested_file_name()));
    let state = if item.is_complete() != 0 {
        "complete"
    } else if item.is_canceled() != 0 {
        "canceled"
    } else if item.is_interrupted() != 0 {
        "interrupted"
    } else {
        "in_progress"
    };
    Download {
        id: item.id(),
        tab,
        url: text(&item.url()),
        original_url: text(&item.original_url()),
        name,
        path,
        mime: text(&item.mime_type()),
        received: item.received_bytes(),
        total: item.total_bytes(),
        percent: item.percent_complete(),
        state,
    }
}

fn context(tab: u64, p: &ContextMenuParams) -> Context {
    let flags = sys::cef_context_menu_type_flags_t::from(p.type_flags()).0;
    let src = text(&p.source_url());
    Context {
        tab,
        x: p.xcoord(),
        y: p.ycoord(),
        link: text(&p.link_url()),
        image: p.media_type() == ContextMenuMediaType::IMAGE && !src.is_empty(),
        src,
        page: text(&p.page_url()),
        selection: text(&p.selection_text()),
        editable: (flags & sys::cef_context_menu_type_flags_t::CM_TYPEFLAG_EDITABLE.0) != 0,
        items: Vec::new(),
    }
}

fn builtin(id: MenuId) -> i32 {
    id.get_raw() as i32
}

fn build_menu(browser: Option<&Browser>, c: &Context, model: &mut MenuModel) {
    let add = |id: i32, label: &str| {
        model.add_item(id, Some(&CefString::from(label)));
    };
    if c.editable {
        model.add_separator();
        add(INSPECT, "Inspect");
        return;
    }
    model.clear();
    if !c.link.is_empty() {
        add(OPEN_LINK, "Open Link in New Tab");
        add(COPY_LINK, "Copy Link Address");
        add(SAVE_LINK, "Save Link As…");
        model.add_separator();
    }
    if c.image {
        add(OPEN_IMAGE, "Open Image in New Tab");
        add(SAVE_IMAGE, "Save Image As…");
        add(COPY_IMAGE, "Copy Image");
        add(COPY_IMAGE_URL, "Copy Image Address");
        model.add_separator();
    }
    let selection = c.selection.trim();
    if !selection.is_empty() {
        let short: String = selection.chars().take(24).collect();
        let more = if selection.chars().count() > 24 { "…" } else { "" };
        add(builtin(MenuId::COPY), "Copy");
        add(SEARCH, &format!("Search Google for “{short}{more}”"));
        model.add_separator();
    }
    if c.link.is_empty() && !c.image && selection.is_empty() {
        add(builtin(MenuId::BACK), "Back");
        add(builtin(MenuId::FORWARD), "Forward");
        add(builtin(MenuId::RELOAD), "Reload");
        if let Some(b) = browser {
            model.set_enabled(builtin(MenuId::BACK), b.can_go_back());
            model.set_enabled(builtin(MenuId::FORWARD), b.can_go_forward());
        }
        model.add_separator();
        add(SAVE_PAGE, "Save Page As…");
        add(builtin(MenuId::PRINT), "Print…");
        model.add_separator();
    }
    add(INSPECT, "Inspect");
}

fn menu_items(model: &MenuModel) -> Vec<Json> {
    (0..model.count())
        .map(|i| {
            if model.type_at(i) == MenuItemType::SEPARATOR {
                json!({ "separator": true })
            } else {
                json!({
                    "id": model.command_id_at(i),
                    "label": text(&model.label_at(i)),
                    "enabled": model.is_enabled_at(i) != 0,
                })
            }
        })
        .collect()
}

fn first_string(list: &mut CefStringList) -> Option<String> {
    let raw = <*mut sys::_cef_string_list_t>::from(&mut *list);
    if raw.is_null() {
        return None;
    }
    unsafe {
        if sys::cef_string_list_size(raw) == 0 {
            return None;
        }
        let mut value: sys::cef_string_utf16_t = std::mem::zeroed();
        if sys::cef_string_list_value(raw, 0, &mut value) <= 0 {
            return None;
        }
        let text = CefString::from(std::ptr::from_ref(&value)).to_string();
        sys::cef_string_utf16_clear(&mut value);
        Some(text)
    }
}

wrap_find_handler! {
    pub(super) struct HostFind { bridge: Bridge, id: u64 }
    impl FindHandler {
        fn on_find_result(&self, _browser: Option<&mut Browser>, _identifier: i32, count: i32, _rect: Option<&Rect>, active: i32, done: i32) {
            self.bridge.send(Event::Commands(Ev::Find { tab: self.id, count, active, done: done != 0 }));
        }
    }
}

wrap_context_menu_handler! {
    pub(super) struct HostMenu { bridge: Bridge, id: u64 }
    impl ContextMenuHandler {
        fn on_before_context_menu(&self, browser: Option<&mut Browser>, _frame: Option<&mut Frame>, params: Option<&mut ContextMenuParams>, model: Option<&mut MenuModel>) {
            if let (Some(params), Some(model)) = (params, model) {
                build_menu(browser.as_deref(), &context(self.id, params), model);
            }
        }
        // A background test instance must not show a native menu; it records the menu instead.
        fn run_context_menu(&self, _browser: Option<&mut Browser>, _frame: Option<&mut Frame>, params: Option<&mut ContextMenuParams>, model: Option<&mut MenuModel>, callback: Option<&mut RunContextMenuCallback>) -> i32 {
            if !mac::background() {
                return 0;
            }
            if let (Some(params), Some(model)) = (params, model) {
                let mut menu = context(self.id, params);
                menu.items = menu_items(model);
                self.bridge.send(Event::Commands(Ev::Menu(menu)));
            }
            if let Some(callback) = callback {
                callback.cancel();
            }
            1
        }
        fn on_context_menu_command(&self, _browser: Option<&mut Browser>, _frame: Option<&mut Frame>, params: Option<&mut ContextMenuParams>, command_id: i32, _flags: EventFlags) -> i32 {
            match params {
                Some(params) if (OPEN_LINK..=INSPECT).contains(&command_id) => {
                    self.bridge.send(Event::Commands(Ev::MenuCommand(context(self.id, params), command_id)));
                    1
                }
                _ => 0,
            }
        }
    }
}

wrap_pdf_print_callback! {
    struct PdfDone { bridge: Bridge, reply: Slot }
    impl PdfPrintCallback {
        fn on_pdf_print_finished(&self, path: Option<&CefString>, ok: i32) {
            let path = path.map(ToString::to_string).unwrap_or_default();
            let reply = self.reply.lock().unwrap().take();
            if reply.is_none() {
                let name = Path::new(&path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.bridge.send(Event::Commands(Ev::Toast(if ok != 0 { format!("Saved {name}") } else { "Could not save the PDF".into() })));
            }
            answer(reply, if ok != 0 { Ok(json!({ "path": path })) } else { Err("Chromium could not create the PDF".into()) });
        }
    }
}

wrap_run_file_dialog_callback! {
    struct SaveDialog { bridge: Bridge, tab: u64 }
    impl RunFileDialogCallback {
        fn on_file_dialog_dismissed(&self, paths: Option<&mut CefStringList>) {
            let path = paths.and_then(first_string).map(PathBuf::from);
            self.bridge.send(Event::Commands(Ev::SavePath { tab: self.tab, path }));
        }
    }
}

impl Manager {
    fn browser_host(&self, id: u64) -> Option<BrowserHost> {
        self.tab(id)?.browser.as_ref()?.host()
    }
    fn active_page(&self) -> Option<u64> {
        self.active.filter(|id| self.tab(*id).is_some_and(|t| !t.scratch && t.url != "about:blank"))
    }
    pub(super) fn toast(&self, message: &str) {
        self.shell(&format!("window.atriumShell?.toast({})", json!(message)));
    }
    fn focus(&self, id: u64) {
        if let Some(host) = self.host(id) {
            host.set_focus(1);
        }
    }

    pub(super) fn on_commands(&mut self, event: Ev) {
        match event {
            Ev::OpenUrls(urls) => {
                self.open_urls(urls);
            }
            Ev::Find { tab, count, active, done } => {
                let find = &mut self.cmds.find;
                if find.tab == Some(tab) {
                    if count >= 0 {
                        find.count = count;
                    }
                    if active >= 0 {
                        find.active = active;
                    }
                    find.done = done;
                    // A new CEF find session counts matches without selecting one.
                    if done && find.count > 0 && find.active == 0 && !find.selected {
                        find.selected = true;
                        find.done = false;
                        let (text, forward) = (find.text.clone(), find.forward);
                        if let Some(host) = self.browser_host(tab) {
                            host.find(Some(&CefString::from(text.as_str())), forward.into(), 0, 1);
                            return;
                        }
                        self.cmds.find.done = true;
                    }
                    self.push_find();
                }
            }
            Ev::BeforeDownload { mut item, callback } => {
                let dir = downloads_dir();
                let _ = std::fs::create_dir_all(&dir);
                let taken: Vec<String> = self
                    .cmds
                    .downloads
                    .iter()
                    .filter(|d| d.state == "in_progress")
                    .map(|d| d.path.clone())
                    .collect();
                let path = unique_path(&dir, &item.name, &taken);
                let before = self.cmds.save_as.len();
                self.cmds.save_as.retain(|u| *u != item.url && *u != item.original_url);
                let ask = before != self.cmds.save_as.len() && !mac::background();
                item.name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                item.path = path.to_string_lossy().into_owned();
                callback.cont(Some(&CefString::from(item.path.as_str())), ask as i32);
                self.update_download(item, None);
            }
            Ev::DownloadUpdated { item, control } => self.update_download(item, control),
            Ev::Menu(menu) => self.cmds.menu = Some(menu),
            Ev::MenuCommand(menu, id) => {
                if let Err(e) = self.context_command(menu, id) {
                    self.toast(&e);
                }
            }
            Ev::ImageCopied(png) => match png {
                Some(png) if !png.is_empty() => {
                    mac::copy_png(&png);
                    self.toast("Copied image");
                }
                _ => self.toast("Could not copy the image"),
            },
            Ev::SavePath { tab, path } => {
                if let Some(path) = path {
                    self.save_page(tab, path, None);
                }
            }
            Ev::Toast(message) => self.toast(&message),
        }
    }

    /// Menu, shortcut, and command-bar commands. Returns false for names it doesn't own.
    pub(super) fn named(&mut self, name: &str) -> bool {
        let result = match name {
            "find" | "find-next" | "find-prev" => {
                if self.active_page().is_some() {
                    self.cmd(match name {
                        "find" => "find",
                        "find-next" => "findNext",
                        _ => "findPrev",
                    });
                }
                Ok(Json::Null)
            }
            "find-close" => {
                // The shell closes the command bar first, then the find bar.
                self.cmd("findClose");
                Ok(Json::Null)
            }
            "zoom-in" => self.zoom(None, "in", None),
            "zoom-out" => self.zoom(None, "out", None),
            "zoom-reset" => self.zoom(None, "reset", None),
            "print" => self.print(None),
            "save-page" => self.save_prompt(false),
            "export-pdf" => self.save_prompt(true),
            "reopen-tab" => self.reopen_closed(),
            "reload-hard" => {
                if let Some(b) = self.active.and_then(|id| self.tab(id)).and_then(|t| t.browser.as_ref()) {
                    b.reload_ignore_cache();
                }
                Ok(Json::Null)
            }
            "make-default-browser" => self.make_default_browser(),
            _ => return false,
        };
        if let Err(e) = result {
            self.toast(&e);
        }
        true
    }

    /// The `/api/browser/action` commands. The shell posts the same commands with no reply.
    pub(super) fn action(&mut self, body: Json, reply: Answer) {
        let cmd = body["cmd"].as_str().unwrap_or("");
        let tab = body["tab"].as_u64().or(self.active);
        if matches!(cmd, "printToPdf" | "savePage") {
            let path = body["path"].as_str().map(PathBuf::from).filter(|p| p.is_absolute());
            let (Some(tab), Some(path)) = (tab, path) else {
                return answer(reply, Err("An open tab and an absolute path are required".into()));
            };
            return if cmd == "printToPdf" {
                self.print_pdf(tab, path, reply)
            } else {
                self.save_page(tab, path, reply)
            };
        }
        let result = match cmd {
            "find" => self.find(
                body["text"].as_str().unwrap_or(""),
                body["forward"] != false,
                body["next"] == true,
                body["viaShell"] == true,
            ),
            "findBar" => {
                self.cmds.find.bar = body["open"] == true;
                self.focus(SHELL);
                Ok(Json::Null)
            }
            "findStop" => {
                self.stop_find();
                Ok(Json::Null)
            }
            "findStatus" => Ok(json!(self.cmds.find)),
            "zoom" => self.zoom(tab, body["how"].as_str().unwrap_or("reset"), body["percent"].as_f64()),
            "zoomStatus" => self.zoom_status(tab),
            "print" => self.print(tab),
            "closedTabs" => Ok(json!(self.cmds.closed)),
            "reopenClosedTab" => self.reopen_closed(),
            "key" => self.key(&body),
            "contextMenu" => self.right_click(tab, body["x"].as_f64(), body["y"].as_f64()),
            "contextMenuStatus" => Ok(json!(self.cmds.menu)),
            "contextCommand" => match (self.cmds.menu.clone(), body["id"].as_i64()) {
                (Some(menu), Some(id)) => self.context_command(menu, id as i32),
                _ => Err("Open a context menu first".into()),
            },
            "downloads" => Ok(json!(self.cmds.downloads)),
            "download" => match (tab.and_then(|id| self.browser_host(id)), body["url"].as_str()) {
                (Some(host), Some(url)) => {
                    host.start_download(Some(&CefString::from(url)));
                    Ok(Json::Null)
                }
                _ => Err("An open tab and a URL are required".into()),
            },
            "downloadCancel" => self.cancel_download(body["id"].as_u64()),
            "downloadReveal" => self.reveal_download(body["id"].as_u64()),
            "downloadsClear" => {
                self.cmds.downloads.retain(|d| d.state == "in_progress");
                self.push_downloads();
                Ok(Json::Null)
            }
            "openUrls" => {
                let urls = body["urls"].as_array().into_iter().flatten().filter_map(Json::as_str).map(str::to_string).collect();
                Ok(json!({ "tabs": self.open_urls(urls) }))
            }
            "simulateOpenUrls" if mac::background() => {
                let urls: Vec<String> = body["urls"].as_array().into_iter().flatten().filter_map(Json::as_str).map(str::to_string).collect();
                mac::deliver_open_urls(&urls);
                Ok(Json::Null)
            }
            "pasteboard" if mac::background() => Ok(mac::test_pasteboard()),
            "defaultBrowser" => Ok(mac::default_browser()),
            "makeDefaultBrowser" => self.make_default_browser(),
            "ui" => Ok(json!({ "covered": self.covered, "active": self.active, "find": self.cmds.find, "shell": self.cmds.shell })),
            other => Err(format!("Unknown browser action {other:?}")),
        };
        answer(reply, result)
    }

    pub(super) fn remember_closed(&mut self, id: u64) {
        if id == SHELL || id == GLANCE {
            return;
        }
        let Some(t) = self.tab(id).filter(|t| !t.scratch && t.url != "about:blank") else { return };
        let closed = Closed { url: t.url.clone(), title: t.title.clone() };
        self.cmds.closed.push(closed);
        if self.cmds.closed.len() > CLOSED_LIMIT {
            self.cmds.closed.remove(0);
        }
    }
    fn reopen_closed(&mut self) -> Result<Json, String> {
        let closed = self.cmds.closed.pop().ok_or("No recently closed tabs")?;
        let id = self.new_tab(Some(closed.url.clone()), true, false);
        Ok(json!({ "tab": id, "url": closed.url }))
    }

    fn open_urls(&mut self, urls: Vec<String>) -> Vec<u64> {
        let urls = urls.into_iter().filter(|u| external_url(u));
        if !super::native::has_shell() {
            self.cmds.urls.extend(urls);
            return Vec::new();
        }
        urls.map(|url| self.new_tab(Some(url), true, false)).collect()
    }
    /// URLs that arrived before the window existed.
    pub(super) fn flush_urls(&mut self) {
        let urls = std::mem::take(&mut self.cmds.urls);
        self.open_urls(urls);
    }

    fn key(&mut self, body: &Json) -> Result<Json, String> {
        let code = key_code(body["key"].as_str().unwrap_or("")).ok_or("Unknown key")?;
        let modifiers = [("meta", CMD), ("shift", SHIFT), ("ctrl", CTRL), ("alt", ALT)]
            .into_iter()
            .filter(|(name, _)| body[*name] == true)
            .fold(0, |all, (_, bit)| all | bit);
        let (name, reserved) = shortcut(code, modifiers).ok_or("No browser shortcut uses this key")?;
        if body["dry"] != true {
            self.command(json!({ "cmd": "menu", "name": name }));
        }
        Ok(json!({ "command": name, "reserved": reserved }))
    }

    fn find(&mut self, query: &str, forward: bool, next: bool, via_shell: bool) -> Result<Json, String> {
        if via_shell {
            self.shell(&format!("window.atriumShell?.findSet({})", json!(query)));
            return Ok(Json::Null);
        }
        let id = self.active_page().ok_or("No page is open")?;
        let host = self.browser_host(id).ok_or("The page is not loaded")?;
        let bar = self.cmds.find.bar;
        if query.is_empty() {
            host.stop_finding(1);
            self.cmds.find = Find { bar, done: true, ..Default::default() };
            self.push_find();
            return Ok(Json::Null);
        }
        let next = next && self.cmds.find.tab == Some(id) && self.cmds.find.text == query;
        if !next {
            self.cmds.find = Find { tab: Some(id), text: query.into(), bar, forward, ..Default::default() };
        } else {
            self.cmds.find.selected = true;
        }
        self.cmds.find.done = false;
        host.find(Some(&CefString::from(query)), forward as i32, 0, next as i32);
        Ok(json!({ "tab": id }))
    }
    fn stop_find(&mut self) {
        if let Some(host) = self.cmds.find.tab.and_then(|id| self.browser_host(id)) {
            host.stop_finding(0);
        }
        self.cmds.find = Find::default();
        if let Some(id) = self.active {
            self.focus(id);
        }
    }
    fn push_find(&self) {
        let f = &self.cmds.find;
        self.shell(&format!(
            "window.atriumShell?.findResult({})",
            json!({ "count": f.count, "active": f.active, "done": f.done, "text": f.text })
        ));
    }

    fn zoom_store(&mut self) -> &mut HashMap<String, f64> {
        self.cmds.zoom.get_or_insert_with(|| {
            crate::data_dir()
                .ok()
                .and_then(|dir| std::fs::read(dir.join(ZOOM_FILE)).ok())
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default()
        })
    }
    fn save_zoom(&mut self) {
        let Ok(dir) = crate::data_dir() else { return };
        let Ok(bytes) = serde_json::to_vec(self.zoom_store()) else { return };
        let temp = dir.join(format!("{ZOOM_FILE}.tmp"));
        if std::fs::write(&temp, bytes).is_ok() {
            let _ = std::fs::rename(&temp, dir.join(ZOOM_FILE));
        }
    }
    /// Re-applies the saved zoom for a page's host after it commits a navigation.
    pub(super) fn apply_zoom(&mut self, id: u64) {
        if id == SHELL {
            return;
        }
        let Some(key) = self.tab(id).and_then(|t| zoom_key(&t.url)) else { return };
        let percent = self.zoom_store().get(&key).copied().unwrap_or(100.);
        if let Some(host) = self.browser_host(id) {
            if (zoom_percent(host.zoom_level()) - percent).abs() > 0.5 {
                host.set_zoom_level(zoom_level(percent));
            }
        }
        if Some(id) == self.active {
            self.show_zoom(percent, false);
        }
    }
    fn zoom(&mut self, tab: Option<u64>, how: &str, value: Option<f64>) -> Result<Json, String> {
        let id = tab.or(self.active_page()).ok_or("No page is open")?;
        let host = self.browser_host(id).ok_or("The page is not loaded")?;
        let key = self.tab(id).and_then(|t| zoom_key(&t.url));
        let current = zoom_percent(host.zoom_level());
        let percent = match (how, value) {
            ("in", _) => zoom_step(current, 1),
            ("out", _) => zoom_step(current, -1),
            ("set", Some(v)) => v.clamp(25., 500.).round(),
            _ => 100.,
        };
        let level = zoom_level(percent);
        host.set_zoom_level(level);
        if let Some(key) = &key {
            for other in self.tabs.iter().filter(|t| t.id != id && zoom_key(&t.url).as_ref() == Some(key)) {
                if let Some(h) = other.browser.as_ref().and_then(|b| b.host()) {
                    h.set_zoom_level(level);
                }
            }
            let store = self.zoom_store();
            if percent == 100. {
                store.remove(key);
            } else {
                store.insert(key.clone(), percent);
            }
            self.save_zoom();
        }
        if Some(id) == self.active {
            self.show_zoom(percent, true);
        }
        Ok(json!({ "tab": id, "percent": percent, "host": key }))
    }
    fn zoom_status(&mut self, tab: Option<u64>) -> Result<Json, String> {
        let id = tab.ok_or("No page is open")?;
        let host = self.browser_host(id).ok_or("The page is not loaded")?;
        let key = self.tab(id).and_then(|t| zoom_key(&t.url));
        let saved = key.as_ref().and_then(|k| self.zoom_store().get(k).copied());
        Ok(json!({ "tab": id, "percent": zoom_percent(host.zoom_level()), "saved": saved, "host": key }))
    }
    fn show_zoom(&self, percent: f64, flash: bool) {
        self.shell(&format!("window.atriumShell?.zoom({})", json!({ "percent": percent, "flash": flash })));
    }

    fn print(&mut self, tab: Option<u64>) -> Result<Json, String> {
        if mac::background() {
            return Err("Printing opens the system print dialog, so it is off in background test instances".into());
        }
        let id = tab.or(self.active_page()).ok_or("No page is open")?;
        self.browser_host(id).ok_or("The page is not loaded")?.print();
        Ok(Json::Null)
    }
    fn print_pdf(&mut self, tab: u64, path: PathBuf, reply: Answer) {
        let Some(host) = self.browser_host(tab) else {
            return answer(reply, Err("The page is not loaded".into()));
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let settings = PdfPrintSettings { print_background: 1, ..Default::default() };
        let mut done = PdfDone::new(self.bridge.clone(), Arc::new(Mutex::new(reply)));
        host.print_to_pdf(Some(&CefString::from(path.to_string_lossy().as_ref())), Some(&settings), Some(&mut done));
    }
    /// `.pdf` prints to PDF, `.html` saves the current DOM, anything else a single-file MHTML archive.
    fn save_page(&mut self, tab: u64, path: PathBuf, reply: Answer) {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        if ext == "pdf" {
            return self.print_pdf(tab, path, reply);
        }
        if self.browser_host(tab).is_none() {
            return answer(reply, Err("The page is not loaded".into()));
        }
        let html = matches!(ext.as_str(), "html" | "htm");
        let (tx, rx) = tokio::sync::oneshot::channel();
        if html {
            self.evaluate(tab, "return (document.doctype ? new XMLSerializer().serializeToString(document.doctype) + '\\n' : '') + document.documentElement.outerHTML".into(), tx, false);
        } else {
            self.cdp(tab, "Page.captureSnapshot", json!({ "format": "mhtml" }), Pending::Raw(tx));
        }
        let bridge = self.bridge.clone();
        std::thread::spawn(move || {
            let result = match rx.blocking_recv() {
                Ok(Ok(value)) => match if html { value.as_str() } else { value["data"].as_str() } {
                    Some(contents) => path
                        .parent()
                        .map_or(Ok(()), std::fs::create_dir_all)
                        .and_then(|_| std::fs::write(&path, contents))
                        .map(|_| json!({ "path": path, "bytes": contents.len() }))
                        .map_err(|e| format!("Could not save the page: {e}")),
                    None => Err("Chromium returned no page contents".into()),
                },
                Ok(Err(e)) => Err(e),
                Err(_) => Err("The page closed before it was saved".into()),
            };
            if reply.is_none() {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                bridge.send(Event::Commands(Ev::Toast(match &result {
                    Ok(_) => format!("Saved {name}"),
                    Err(e) => e.clone(),
                })));
            }
            answer(reply, result);
        });
    }
    /// Save Page As / Export as PDF. Background test instances skip the save panel.
    fn save_prompt(&mut self, pdf: bool) -> Result<Json, String> {
        let id = self.active_page().ok_or("No page is open")?;
        let host = self.browser_host(id).ok_or("The page is not loaded")?;
        let t = self.tab(id).ok_or("No page is open")?;
        let title = if t.title.trim().is_empty() { zoom_key(&t.url).unwrap_or_else(|| "Page".into()) } else { t.title.clone() };
        let name = format!("{}.{}", safe_name(&title), if pdf { "pdf" } else { "mhtml" });
        let dir = downloads_dir();
        if mac::background() {
            let path = unique_path(&dir, &name, &[]);
            self.save_page(id, path.clone(), None);
            return Ok(json!({ "path": path }));
        }
        let mut dialog = SaveDialog::new(self.bridge.clone(), id);
        host.run_file_dialog(
            FileDialogMode::SAVE,
            Some(&CefString::from(if pdf { "Export as PDF" } else { "Save Page As" })),
            Some(&CefString::from(dir.join(name).to_string_lossy().as_ref())),
            None,
            Some(&mut dialog),
        );
        Ok(Json::Null)
    }

    fn right_click(&mut self, tab: Option<u64>, x: Option<f64>, y: Option<f64>) -> Result<Json, String> {
        let (Some(id), Some(x), Some(y)) = (tab, x, y) else {
            return Err("A tab and x/y page coordinates are required".into());
        };
        let host = self.browser_host(id).ok_or("The page is not loaded")?;
        let event = MouseEvent { x: x.round() as i32, y: y.round() as i32, modifiers: 0 };
        self.cmds.menu = None;
        host.send_mouse_move_event(Some(&event), 0);
        host.send_mouse_click_event(Some(&event), MouseButtonType::RIGHT, 0, 1);
        host.send_mouse_click_event(Some(&event), MouseButtonType::RIGHT, 1, 1);
        Ok(Json::Null)
    }
    fn context_command(&mut self, menu: Context, id: i32) -> Result<Json, String> {
        let browser = self.tab(menu.tab).and_then(|t| t.browser.clone());
        let host = browser.as_ref().and_then(|b| b.host());
        let done = Ok(json!({ "command": id }));
        match id {
            OPEN_LINK | OPEN_IMAGE => {
                let url = if id == OPEN_LINK { menu.link } else { menu.src };
                let tab = self.new_tab(Some(url), false, false);
                Ok(json!({ "command": id, "tab": tab }))
            }
            COPY_LINK | COPY_IMAGE_URL => {
                mac::copy_text(if id == COPY_LINK { &menu.link } else { &menu.src });
                done
            }
            SAVE_LINK | SAVE_IMAGE => {
                let url = if id == SAVE_LINK { menu.link } else { menu.src };
                let host = host.ok_or("The page is not loaded")?;
                self.cmds.save_as.push(url.clone());
                host.start_download(Some(&CefString::from(url.as_str())));
                done
            }
            COPY_IMAGE => {
                // Copy the image the page is showing. CEF's image download misses pictures
                // that are already on screen, especially in a background window.
                let src = json!(menu.src);
                let (tx, rx) = tokio::sync::oneshot::channel();
                self.evaluate(menu.tab, format!(
                    "const src = {src}; const img = [...document.images].find((i) => i.currentSrc === src || i.src === src); if (!img || !img.naturalWidth) return null; const c = document.createElement('canvas'); c.width = img.naturalWidth; c.height = img.naturalHeight; c.getContext('2d').drawImage(img, 0, 0); return c.toDataURL('image/png');"
                ), tx, false);
                let bridge = self.bridge.clone();
                std::thread::spawn(move || {
                    let png = (|| {
                        let value = rx.blocking_recv().ok()?.ok()?;
                        let encoded = value.as_str()?.strip_prefix("data:image/png;base64,")?;
                        use base64::Engine;
                        base64::engine::general_purpose::STANDARD.decode(encoded).ok()
                    })();
                    bridge.send(Event::Commands(Ev::ImageCopied(png)));
                });
                done
            }
            SEARCH => {
                let url = reqwest::Url::parse_with_params("https://www.google.com/search", &[("q", menu.selection.trim())])
                    .map_err(|e| e.to_string())?;
                let tab = self.new_tab(Some(url.into()), true, false);
                Ok(json!({ "command": id, "tab": tab }))
            }
            SAVE_PAGE => self.save_prompt(false),
            INSPECT => {
                if mac::background() {
                    return Err("Developer tools open a window, so they are off in background test instances".into());
                }
                let point = Point { x: menu.x, y: menu.y };
                host.ok_or("The page is not loaded")?.show_dev_tools(None, None, None, Some(&point));
                done
            }
            id if id == builtin(MenuId::BACK) => browser.map(|b| b.go_back()).ok_or("The page is not loaded".into()).and(done),
            id if id == builtin(MenuId::FORWARD) => browser.map(|b| b.go_forward()).ok_or("The page is not loaded".into()).and(done),
            id if id == builtin(MenuId::RELOAD) => browser.map(|b| b.reload()).ok_or("The page is not loaded".into()).and(done),
            id if id == builtin(MenuId::PRINT) => self.print(Some(menu.tab)),
            id if id == builtin(MenuId::COPY) => {
                browser.and_then(|b| b.focused_frame().or_else(|| b.main_frame())).ok_or("The page is not loaded")?.copy();
                done
            }
            _ => Err("This menu item is not available".into()),
        }
    }

    fn update_download(&mut self, item: Download, control: Option<DownloadItemCallback>) {
        let finished = item.state != "in_progress";
        match self.cmds.downloads.iter_mut().find(|d| d.id == item.id) {
            Some(d) => {
                let old = std::mem::replace(d, item.clone());
                d.tab = old.tab;
                if d.path.is_empty() {
                    d.path = old.path;
                }
                if d.name.is_empty() {
                    d.name = old.name;
                }
            }
            None => {
                self.cmds.downloads.insert(0, item.clone());
                self.cmds.downloads.truncate(DOWNLOAD_LIMIT);
            }
        }
        if finished {
            self.cmds.controls.remove(&item.id);
        } else if let Some(control) = control {
            self.cmds.controls.insert(item.id, control);
        }
        self.push_downloads();
    }
    fn push_downloads(&self) {
        self.shell(&format!("window.atriumShell?.downloads({})", json!(self.cmds.downloads)));
    }
    fn cancel_download(&mut self, id: Option<u64>) -> Result<Json, String> {
        let id = id.ok_or("A download id is required")? as u32;
        let control = self.cmds.controls.get(&id).ok_or("This download is not in progress")?;
        control.cancel();
        Ok(Json::Null)
    }
    fn reveal_download(&mut self, id: Option<u64>) -> Result<Json, String> {
        let id = id.ok_or("A download id is required")? as u32;
        let d = self.cmds.downloads.iter().find(|d| d.id == id).ok_or("Unknown download")?;
        let path = PathBuf::from(&d.path);
        if !path.exists() {
            return Err("The file was moved or deleted".into());
        }
        // Finder would come to the front, which test instances must never cause.
        let revealed = !mac::background()
            && std::process::Command::new("/usr/bin/open").arg("-R").arg(&path).spawn().is_ok();
        Ok(json!({ "path": path, "revealed": revealed }))
    }

    fn make_default_browser(&mut self) -> Result<Json, String> {
        if mac::background() {
            return Err("Changing the default browser shows a system dialog, so it is off in background test instances".into());
        }
        mac::make_default_browser()?;
        Ok(mac::default_browser())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cef::MenuId;

    #[test]
    fn shortcuts_cover_standard_commands() {
        assert_eq!(shortcut(84, CMD), Some(("new-tab", true)));
        assert_eq!(shortcut(84, CMD | SHIFT), Some(("reopen-tab", true)));
        assert_eq!(shortcut(87, CMD), Some(("close-tab", true)));
        assert_eq!(shortcut(76, CMD), Some(("open-location", true)));
        assert_eq!(shortcut(82, CMD), Some(("reload", false)));
        assert_eq!(shortcut(219, CMD), Some(("back", false)));
        assert_eq!(shortcut(221, CMD), Some(("forward", false)));
        assert_eq!(shortcut(49, CMD), Some(("tab-1", true)));
        assert_eq!(shortcut(56, CMD), Some(("tab-8", true)));
        assert_eq!(shortcut(57, CMD), Some(("tab-last", true)));
        assert_eq!(shortcut(73, CMD | ALT), Some(("devtools", true)));
        assert_eq!(shortcut(9, CTRL), Some(("next-tab", true)));
        assert_eq!(shortcut(9, CTRL | SHIFT), Some(("prev-tab", true)));
        assert_eq!(shortcut(70, CMD), Some(("find", false)));
        assert_eq!(shortcut(187, CMD), Some(("zoom-in", false)));
        assert_eq!(shortcut(189, CMD), Some(("zoom-out", false)));
        assert_eq!(shortcut(48, CMD), Some(("zoom-reset", false)));
        assert_eq!(shortcut(80, CMD), Some(("print", false)));
        assert_eq!(shortcut(83, CMD), Some(("save-page", false)));
        // Caps Lock and other flags don't change the shortcut.
        assert_eq!(shortcut(84, CMD | 1), Some(("new-tab", true)));
        assert_eq!(shortcut(67, CMD), None);
        assert_eq!(shortcut(86, CMD), None);
        assert_eq!(shortcut(84, 0), None);
    }

    #[test]
    fn key_names_map_to_virtual_key_codes() {
        assert_eq!(key_code("t"), Some(84));
        assert_eq!(key_code("1"), Some(49));
        assert_eq!(key_code("="), Some(187));
        assert_eq!(key_code("Tab"), Some(9));
        assert_eq!(key_code("Escape"), Some(27));
        assert_eq!(key_code("é"), None);
    }

    #[test]
    fn zoom_steps_follow_chrome_presets() {
        assert_eq!(zoom_step(100., 1), 110.);
        assert_eq!(zoom_step(100., -1), 90.);
        assert_eq!(zoom_step(500., 1), 500.);
        assert_eq!(zoom_step(25., -1), 25.);
        assert_eq!(zoom_step(104., 1), 110.);
        assert_eq!(zoom_percent(zoom_level(125.)), 125.);
        assert_eq!(zoom_percent(0.), 100.);
        assert!((zoom_level(120.) - 1.).abs() < 1e-9);
        assert_eq!(zoom_key("https://www.example.com:8443/a"), Some("www.example.com".into()));
        assert_eq!(zoom_key("file:///tmp/a.html"), None);
    }

    #[test]
    fn download_names_never_overwrite() {
        let dir = std::env::temp_dir().join(format!("atrium-unique-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("report.pdf"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "report.pdf", &[]), dir.join("report (1).pdf"));
        let taken = [dir.join("report (1).pdf").to_string_lossy().into_owned()];
        assert_eq!(unique_path(&dir, "report.pdf", &taken), dir.join("report (2).pdf"));
        assert_eq!(unique_path(&dir, "new.txt", &[]), dir.join("new.txt"));
        assert_eq!(safe_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(safe_name(".hidden"), "hidden");
        assert_eq!(safe_name("  "), "download");
        assert_eq!(unique_path(&dir, ".profile", &[]), dir.join("profile"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn menu_ids_start_at_cef_user_range() {
        assert_eq!(OPEN_LINK as u32, MenuId::USER_FIRST.get_raw());
        assert!((INSPECT as u32) < MenuId::USER_LAST.get_raw());
    }

    #[test]
    fn only_web_and_file_urls_come_from_other_apps() {
        assert!(external_url("https://example.com"));
        assert!(external_url("file:///tmp/a.html"));
        assert!(!external_url("javascript:alert(1)"));
        assert!(!external_url("atrium://settings"));
    }
}
