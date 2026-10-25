//! Browser tools for the agent, built on the native browser's primitive ops.
//!
//! Follows Aside's agent design: the agent reads pages through an
//! accessibility-style snapshot with stable refs, every action reports what
//! changed on the page (so the model rarely needs a separate look), several
//! actions can be chained in one call, and research happens in invisible
//! scratch tabs so the user's tabs are never hijacked.

use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
    time::Duration,
};

use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use serde_json::{json, Value};

use crate::app::{Handle, Op};

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Content {
    Text { text: String },
    Image { data: String, #[serde(rename = "mimeType")] mime_type: String },
}

fn text(s: impl Into<String>) -> Vec<Content> {
    vec![Content::Text { text: s.into() }]
}

/// JSON schemas for the MCP `tools/list` response.
pub fn tool_specs() -> Value {
    let refp = json!({ "type": "string", "description": "Element ref from a snapshot, e.g. \"e12\"" });
    let step = json!({
        "type": "object",
        "properties": {
            "action": { "type": "string", "enum": ["click", "type", "select_option", "press_key", "scroll", "navigate", "wait"] },
            "ref": { "type": "string" }, "text": { "type": "string" }, "submit": { "type": "boolean" },
            "value": { "type": "string" }, "key": { "type": "string" }, "direction": { "type": "string" },
            "url": { "type": "string" }, "seconds": { "type": "number" }
        },
        "required": ["action"]
    });
    json!([
        { "name": "snapshot", "description": "Read the agent's current page as an accessibility tree: headings, text, lists, forms and every interactive element with a ref like e12. The primary way to see a page. Includes off-screen content, so scrolling is rarely needed. Use interactive=true when you only need things to act on; scope to a ref or CSS selector to read one section in full.",
          "inputSchema": { "type": "object", "properties": {
              "interactive": { "type": "boolean" }, "scope": { "type": "string", "description": "A ref (e.g. e40) or CSS selector" } } } },
        { "name": "click", "description": "Click an element by ref. Waits for the page to settle and returns what changed.",
          "inputSchema": { "type": "object", "properties": { "ref": refp }, "required": ["ref"] } },
        { "name": "type", "description": "Type into a field by ref (replaces its contents unless clear=false). submit=true presses Enter. Returns what changed.",
          "inputSchema": { "type": "object", "properties": {
              "ref": refp, "text": { "type": "string" }, "submit": { "type": "boolean" }, "clear": { "type": "boolean" } }, "required": ["ref", "text"] } },
        { "name": "select_option", "description": "Choose an option in a <select> by value or visible text.",
          "inputSchema": { "type": "object", "properties": { "ref": refp, "value": { "type": "string" } }, "required": ["ref", "value"] } },
        { "name": "press_key", "description": "Press a key on the focused element: Enter, Escape, Tab, Backspace, ArrowDown, PageDown, End.",
          "inputSchema": { "type": "object", "properties": { "key": { "type": "string" } }, "required": ["key"] } },
        { "name": "scroll", "description": "Scroll the page or the scrollable area containing ref. Mainly useful to trigger lazy loading.",
          "inputSchema": { "type": "object", "properties": {
              "direction": { "type": "string", "enum": ["up", "down"] }, "amount": { "type": "integer" }, "ref": refp }, "required": ["direction"] } },
        { "name": "batch", "description": "Run several actions in a row in one call (e.g. type into a field, select an option, click submit). Stops at the first failure and returns what changed. Use when the next step does not depend on seeing the page in between.",
          "inputSchema": { "type": "object", "properties": { "steps": { "type": "array", "items": step } }, "required": ["steps"] } },
        { "name": "navigate", "description": "Load a URL in the agent's page (\"back\", \"forward\" and \"reload\" also work; plain words search Google). Returns the new page's interactive tree.",
          "inputSchema": { "type": "object", "properties": { "url": { "type": "string" } }, "required": ["url"] } },
        { "name": "open_tab", "description": "Open a URL in a new tab, which becomes the agent's page.",
          "inputSchema": { "type": "object", "properties": { "url": { "type": "string" } }, "required": ["url"] } },
        { "name": "tabs", "description": "List open tabs; marks the user's tab and the agent's tab.",
          "inputSchema": { "type": "object", "properties": {} } },
        { "name": "switch_tab", "description": "Make another tab (index from `tabs`) the agent's page.",
          "inputSchema": { "type": "object", "properties": { "index": { "type": "integer" } }, "required": ["index"] } },
        { "name": "close_tab", "description": "Close a tab by index (defaults to the agent's tab).",
          "inputSchema": { "type": "object", "properties": { "index": { "type": "integer" } } } },
        { "name": "read_page", "description": "Read the main text of the agent's page as plain text (articles, long results, threads).",
          "inputSchema": { "type": "object", "properties": { "max_chars": { "type": "integer" } } } },
        { "name": "search", "description": "Search the web in the background and return the top results (title, URL, snippet) without touching any tab. Call several in parallel for different queries.",
          "inputSchema": { "type": "object", "properties": { "query": { "type": "string" }, "max": { "type": "integer" } }, "required": ["query"] } },
        { "name": "fetch", "description": "Load a URL in the background (with the user's logins) and return its readable text, without touching any tab. Call several in parallel to read many sources.",
          "inputSchema": { "type": "object", "properties": { "url": { "type": "string" }, "max_chars": { "type": "integer" } }, "required": ["url"] } },
        { "name": "screenshot", "description": "Capture the agent's page as an image, for visual checks or as proof of a finished task.",
          "inputSchema": { "type": "object", "properties": {} } },
        { "name": "evaluate", "description": "Run JavaScript in the agent's page and return the JSON result (expression, or statements ending in `return`). For extracting structured data in bulk. It shares the page's DOM but not its JavaScript globals.",
          "inputSchema": { "type": "object", "properties": { "code": { "type": "string" } }, "required": ["code"] } },
        { "name": "wait", "description": "Wait up to 15 seconds, only when a snapshot shows the page is still loading.",
          "inputSchema": { "type": "object", "properties": { "seconds": { "type": "number" } }, "required": ["seconds"] } }
    ])
}

// DuckDuckGo's HTML results give direct (decodable) URLs; Google now hides
// them behind encrypted redirect links.
const SEARCH_JS: &str = r#"
const max = __MAX__;
const out = [];
for (const r of document.querySelectorAll('.result')) {
  if (r.classList.contains('result--ad')) continue;
  const a = r.querySelector('a.result__a');
  if (!a) continue;
  let url = a.href;
  try { const u = new URL(url, location.href); url = u.searchParams.get('uddg') || u.href; } catch {}
  const snippet = (r.querySelector('.result__snippet')?.innerText || '').replace(/\s+/g, ' ').trim();
  out.push({ title: a.innerText.trim(), url, snippet: snippet.slice(0, 300) });
  if (out.length >= max) break;
}
return out;
"#;

pub struct Agent {
    pub browser: Handle,
    /// The tab the agent is working in.
    current: Mutex<Option<u64>>,
    /// Last interactive tree seen per tab: (url, tree), for change reports.
    seen: Mutex<HashMap<u64, (String, String)>>,
    scratch_slots: std::sync::Arc<tokio::sync::Semaphore>,

}

impl Agent {
    pub fn new(browser: Handle) -> Self {
        Self { browser, current: Mutex::new(None), seen: Mutex::new(HashMap::new()), scratch_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(3)) }
    }

    /// Point the agent at the tab the user is looking at (None: it opens its own).
    pub fn set_current(&self, tab: Option<u64>) {
        *self.current.lock().unwrap() = tab;
    }

    pub fn current(&self) -> Option<u64> {
        *self.current.lock().unwrap()
    }

    /// The agent's tab, if it still exists.
    async fn tab(&self) -> Result<Option<u64>> {
        let st = self.browser.op(Op::Tabs).await?;
        let cur = self.current();
        Ok(cur.filter(|id| st.tabs.iter().any(|t| t.id == *id && t.kind == "web")))
    }

    async fn need_tab(&self) -> Result<u64> {
        self.tab().await?.ok_or_else(|| anyhow!("The agent has no page open yet. Use navigate or open_tab first."))
    }

    async fn eval_in(&self, tab: u64, js: &str) -> Result<Value> {
        for attempt in 0..2 {
            let js = js.to_string();
            match self.browser.op(|reply| Op::Eval { tab: Some(tab), js, reply }).await? {
                Ok(v) => return Ok(v),
                // CEF creates discarded views asynchronously. This response means
                // the script has not run; wait for the new document before retrying.
                Err(e) if attempt == 0 && e == "The tab is still loading. Try again in a moment." => {
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(24);
                    loop {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        let state = self.browser.op(Op::Tabs).await?;
                        let page = state.tabs.iter().find(|t| t.id == tab)
                            .ok_or_else(|| anyhow!("The tab closed while it was waking up"))?;
                        if !page.loading { break; }
                        if tokio::time::Instant::now() >= deadline { bail!("The tab is still loading; retry when navigation finishes"); }
                    }
                }
                Err(e) => bail!("{e}"),
            }
        }
        unreachable!("both evaluation attempts returned")
    }

    /// Like `eval_in`, but a navigation triggered by the script counts as success.
    async fn eval_nav(&self, tab: u64, js: &str) -> Result<Option<Value>> {
        match self.eval_in(tab, js).await {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.to_string().contains("__navigated__") || e.to_string().contains("navigated before") => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn wait_loaded(&self, tab: u64) {
        tokio::time::sleep(Duration::from_millis(200)).await;
        for _ in 0..80 {
            match self.browser.op(Op::Tabs).await {
                Ok(st) if st.tabs.iter().find(|t| t.id == tab).map(|t| !t.loading).unwrap_or(true) => break,
                Err(_) => break,
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let _ = self.eval_in(tab, "return __atrium.settle(400, 4000)").await;
    }

    async fn page_line(&self, tab: u64) -> String {
        match self.browser.op(Op::Tabs).await {
            Ok(st) => match st.tabs.iter().find(|t| t.id == tab) {
                Some(t) => format!("{} — {}", t.title, t.url),
                None => "the tab was closed".into(),
            },
            Err(e) => e.to_string(),
        }
    }

    /// What changed on the page since the agent last looked: the new page's
    /// interactive tree after a navigation, otherwise a line diff.
    async fn changes(&self, tab: u64) -> String {
        let v = match self.eval_in(tab, "await __atrium.settle(350, 3000); return __atrium.snapshot({ interactive: true })").await {
            Ok(v) => v,
            Err(_) => {
                self.wait_loaded(tab).await;
                match self.eval_in(tab, "return __atrium.snapshot({ interactive: true })").await {
                    Ok(v) => v,
                    Err(e) => return format!("\n\n(Could not read the page afterwards: {e})"),
                }
            }
        };
        let url = v["url"].as_str().unwrap_or("").to_string();
        let tree = v["tree"].as_str().unwrap_or("").to_string();
        let modal = if v["modal"] == true { " A dialog is open." } else { "" };
        let prev = self.seen.lock().unwrap().insert(tab, (url.clone(), tree.clone()));
        match prev {
            Some((old_url, old_tree)) if old_url == url => {
                let diff = line_diff(&old_tree, &tree, 60);
                if diff.is_empty() {
                    format!("\n\nNo visible change on the page.{modal}")
                } else {
                    format!("\n\nPage changes:{modal}\n{diff}")
                }
            }
            _ => format!(
                "\n\nNow on: {} — {url}{modal}\nInteractive elements:\n{}",
                v["title"].as_str().unwrap_or(""),
                cap_lines(&tree, 220)
            ),
        }
    }

    pub async fn call(&self, name: &str, args: &Value) -> Result<Vec<Content>> {
        let s = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_string);
        let req = |k: &str| s(k).ok_or_else(|| anyhow!("missing argument `{k}`"));
        match name {
            "tabs" => {
                let st = self.browser.op(Op::Tabs).await?;
                let cur = self.current();
                let lines: Vec<String> = st
                    .tabs
                    .iter()
                    .filter(|t| t.kind != "scratch")
                    .enumerate()
                    .map(|(i, t)| {
                        let mut marks = Vec::new();
                        if Some(t.id) == st.active {
                            marks.push("user is viewing");
                        }
                        if Some(t.id) == cur {
                            marks.push("agent's tab");
                        }
                        let marks = if marks.is_empty() { String::new() } else { format!(" ({})", marks.join(", ")) };
                        if t.kind == "web" {
                            format!("[{i}] {} — {}{marks}", t.title, t.url)
                        } else {
                            format!("[{i}] New Tab{marks}")
                        }
                    })
                    .collect();
                Ok(text(if lines.is_empty() { "No tabs open".into() } else { lines.join("\n") }))
            }
            "open_tab" => {
                let url = req("url")?;
                let id = self.browser.op(|reply| Op::NewTab { url: Some(url), background: true, scratch: false, reply }).await?;
                self.set_current(Some(id));
                self.wait_loaded(id).await;
                Ok(text(format!("Opened a new tab.{}", self.changes(id).await)))
            }
            "switch_tab" | "close_tab" => {
                let st = self.browser.op(Op::Tabs).await?;
                let tabs: Vec<_> = st.tabs.iter().filter(|t| t.kind != "scratch").collect();
                let idx = match args.get("index").and_then(Value::as_u64) {
                    Some(i) => i as usize,
                    None if name == "close_tab" => {
                        let cur = self.current();
                        tabs.iter().position(|t| Some(t.id) == cur).ok_or_else(|| anyhow!("the agent has no tab to close"))?
                    }
                    None => bail!("missing argument `index`"),
                };
                let t = (*tabs.get(idx).ok_or_else(|| anyhow!("no tab with index {idx}"))?).clone();
                if name == "switch_tab" {
                    self.set_current(Some(t.id));
                    self.seen.lock().unwrap().remove(&t.id);
                    Ok(text(format!("The agent now works in [{idx}] {} — {}", t.title, t.url)))
                } else {
                    self.browser.op(|reply| Op::Close { id: t.id, reply }).await?;
                    if self.current() == Some(t.id) {
                        self.set_current(None);
                    }
                    Ok(text(format!("Closed [{idx}] {}", t.title)))
                }
            }
            "navigate" => {
                let url = req("url")?;
                let id = match (url.as_str(), self.tab().await?) {
                    ("back" | "forward" | "reload", Some(tab)) => {
                        let action: &'static str = match url.as_str() {
                            "back" => "back",
                            "forward" => "forward",
                            _ => "reload",
                        };
                        self.browser.op(|reply| Op::History { tab: Some(tab), action, reply }).await?.map_err(|e| anyhow!(e))?;
                        tab
                    }
                    ("back" | "forward" | "reload", None) => bail!("The agent has no page open yet."),
                    (_, Some(tab)) => self.browser.op(|reply| Op::Navigate { tab: Some(tab), url, reply }).await?,
                    (_, None) => {
                        let id = self.browser.op(|reply| Op::NewTab { url: Some(url), background: true, scratch: false, reply }).await?;
                        self.set_current(Some(id));
                        id
                    }
                };
                self.seen.lock().unwrap().remove(&id);
                self.wait_loaded(id).await;
                Ok(text(format!("Loaded.{}", self.changes(id).await)))
            }
            "snapshot" => {
                let tab = self.need_tab().await?;
                let opts = json!({ "interactive": args.get("interactive").and_then(Value::as_bool).unwrap_or(false), "scope": s("scope") });
                let v = self.eval_in(tab, &format!("return __atrium.snapshot({opts})")).await?;
                if opts["interactive"] == true && opts["scope"].is_null() {
                    let url = v["url"].as_str().unwrap_or("").to_string();
                    self.seen.lock().unwrap().insert(tab, (url, v["tree"].as_str().unwrap_or("").to_string()));
                }
                Ok(text(format!(
                    "Page: {} — {}\nScroll: {}{}\n\n{}",
                    v["title"].as_str().unwrap_or(""),
                    v["url"].as_str().unwrap_or(""),
                    v["scroll"].as_str().unwrap_or(""),
                    if v["modal"] == true { "\nA dialog is open." } else { "" },
                    v["tree"].as_str().filter(|t| !t.is_empty()).unwrap_or("(empty: the page may still be loading)")
                )))
            }
            "click" | "type" | "select_option" | "press_key" | "scroll" => {
                let tab = self.need_tab().await?;
                let what = self.act(tab, name, args).await?;
                Ok(text(format!("{what}{}", self.changes(tab).await)))
            }
            "batch" => {
                let steps = args.get("steps").and_then(Value::as_array).cloned().unwrap_or_default();
                if steps.is_empty() {
                    bail!("`steps` is empty");
                }
                let mut log = Vec::new();
                let mut tab = self.tab().await?;
                for (i, step) in steps.iter().enumerate() {
                    let action = step["action"].as_str().unwrap_or("");
                    let r = match action {
                        "navigate" => {
                            let url = step["url"].as_str().unwrap_or("").to_string();
                            let id = match tab {
                                Some(t) => self.browser.op(|reply| Op::Navigate { tab: Some(t), url: url.clone(), reply }).await?,
                                None => {
                                    let id = self.browser.op(|reply| Op::NewTab { url: Some(url.clone()), background: true, scratch: false, reply }).await?;
                                    self.set_current(Some(id));
                                    id
                                }
                            };
                            tab = Some(id);
                            self.wait_loaded(id).await;
                            Ok(format!("Loaded {url}."))
                        }
                        "wait" => {
                            let secs = step["seconds"].as_f64().unwrap_or(1.0).clamp(0.0, 15.0);
                            tokio::time::sleep(Duration::from_secs_f64(secs)).await;
                            Ok(format!("Waited {secs}s."))
                        }
                        _ => match tab {
                            Some(t) => {
                                let r = self.act(t, action, step).await;
                                if r.is_ok() {
                                    let _ = self.eval_in(t, "return __atrium.settle(250, 2000)").await;
                                }
                                r
                            }
                            None => Err(anyhow!("no page open")),
                        },
                    };
                    match r {
                        Ok(m) => log.push(format!("{}. {m}", i + 1)),
                        Err(e) => {
                            log.push(format!("{}. {action} failed: {e}. Stopped here.", i + 1));
                            break;
                        }
                    }
                }
                let changes = match tab {
                    Some(t) => self.changes(t).await,
                    None => String::new(),
                };
                Ok(text(format!("{}{changes}", log.join("\n"))))
            }
            "read_page" => {
                let tab = self.need_tab().await?;
                let max = args.get("max_chars").and_then(Value::as_u64).unwrap_or(20_000);
                let v = self.eval_in(tab, &format!("return __atrium.readPage({max})")).await?;
                Ok(text(format!(
                    "Page: {} — {}\n\n{}{}",
                    v["title"].as_str().unwrap_or(""),
                    v["url"].as_str().unwrap_or(""),
                    v["text"].as_str().unwrap_or(""),
                    if v["truncated"] == true { "\n\n… (truncated)" } else { "" }
                )))
            }
            "search" => {
                let query = req("query")?;
                let max = args.get("max").and_then(Value::as_u64).unwrap_or(8).min(15);
                let q: String = query
                    .bytes()
                    .map(|b| match b {
                        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
                        b' ' => "+".into(),
                        _ => format!("%{b:02X}"),
                    })
                    .collect();
                let url = format!("https://html.duckduckgo.com/html/?q={q}");
                let scratch = self.scratch(&url).await?;
                let id = scratch.id;
                let res = self.eval_in(id, &SEARCH_JS.replace("__MAX__", &max.to_string())).await;
                let fallback = if res.as_ref().map(|v| v.as_array().map_or(true, |a| a.is_empty())).unwrap_or(true) {
                    self.eval_in(id, "return __atrium.readPage(3000).text").await.ok()
                } else {
                    None
                };
                self.close_scratch(scratch).await;
                let results = res?;
                let list = results.as_array().cloned().unwrap_or_default();
                if list.is_empty() {
                    return Ok(text(format!(
                        "No results parsed for \"{query}\". The page said:\n{}",
                        fallback.and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
                    )));
                }
                let lines: Vec<String> = list
                    .iter()
                    .enumerate()
                    .map(|(i, r)| {
                        format!(
                            "{}. {}\n   {}\n   {}",
                            i + 1,
                            r["title"].as_str().unwrap_or(""),
                            r["url"].as_str().unwrap_or(""),
                            r["snippet"].as_str().unwrap_or("")
                        )
                    })
                    .collect();
                Ok(text(format!("Results for \"{query}\":\n\n{}", lines.join("\n"))))
            }
            "fetch" => {
                let url = req("url")?;
                let max = args.get("max_chars").and_then(Value::as_u64).unwrap_or(12_000);
                let scratch = self.scratch(&url).await?;
                let id = scratch.id;
                let v = self.eval_in(id, &format!("return __atrium.readPage({max})")).await;
                self.close_scratch(scratch).await;
                let v = v?;
                Ok(text(format!(
                    "Page: {} — {}\n\n{}{}",
                    v["title"].as_str().unwrap_or(""),
                    v["url"].as_str().unwrap_or(""),
                    v["text"].as_str().unwrap_or(""),
                    if v["truncated"] == true { "\n\n… (truncated)" } else { "" }
                )))
            }
            "screenshot" => {
                let tab = self.need_tab().await?;
                let data = self.browser.op(|reply| Op::Screenshot { tab: Some(tab), reply }).await?.map_err(|e| anyhow!(e))?;
                Ok(vec![
                    Content::Image { data, mime_type: "image/jpeg".into() },
                    Content::Text { text: format!("Screenshot of {}", self.page_line(tab).await) },
                ])
            }
            "evaluate" => {
                let tab = self.need_tab().await?;
                let code = req("code")?;
                let body = if code.contains("return") { code } else { format!("return ({code})") };
                let v = self.eval_in(tab, &body).await?;
                let mut out = serde_json::to_string_pretty(&v)?;
                out.truncate(20_000);
                Ok(text(out))
            }
            "wait" => {
                let secs = args.get("seconds").and_then(Value::as_f64).unwrap_or(1.0).clamp(0.0, 15.0);
                tokio::time::sleep(Duration::from_secs_f64(secs)).await;
                Ok(text(format!("Waited {secs}s")))
            }
            other => bail!("unknown tool `{other}`"),
        }
    }

    /// Performs one page action and describes it (no change report).
    async fn act(&self, tab: u64, name: &str, args: &Value) -> Result<String> {
        let s = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_string);
        let req = |k: &str| s(k).ok_or_else(|| anyhow!("missing argument `{k}`"));
        let js = |v: &str| json!(v).to_string();
        match name {
            "click" => {
                let r = req("ref")?;
                let res = self.eval_nav(tab, &format!("return __atrium.click({})", js(&r))).await?;
                if res.is_none() {
                    self.wait_loaded(tab).await;
                }
                let mut msg = format!("Clicked {r}.");
                if let Some(by) = res.as_ref().and_then(|v| v["coveredBy"].as_str()) {
                    msg.push_str(&format!(" It was partly covered by {by}."));
                }
                Ok(msg)
            }
            "type" => {
                let r = req("ref")?;
                let body = req("text")?;
                let clear = args.get("clear").and_then(Value::as_bool).unwrap_or(true);
                let submit = args.get("submit").and_then(Value::as_bool).unwrap_or(false);
                let res = self.eval_nav(tab, &format!("return __atrium.type({}, {}, {clear}, {submit})", js(&r), js(&body))).await?;
                if submit {
                    self.wait_loaded(tab).await;
                    let how = res.as_ref().and_then(Value::as_str).unwrap_or("submitted");
                    return Ok(format!("Typed into {r} and pressed Enter ({how})."));
                }
                Ok(format!("Typed into {r}."))
            }
            "select_option" => {
                let chosen = self.eval_in(tab, &format!("return __atrium.selectOption({}, {})", js(&req("ref")?), js(&req("value")?))).await?;
                Ok(format!("Selected {chosen}."))
            }
            "press_key" => {
                let key = req("key")?;
                let res = self.eval_nav(tab, &format!("return __atrium.key({})", js(&key))).await?;
                if res.is_none() {
                    self.wait_loaded(tab).await;
                }
                Ok(format!("Pressed {key} ({}).", res.as_ref().and_then(Value::as_str).unwrap_or("page navigated")))
            }
            "scroll" => {
                let dir = s("direction").unwrap_or_else(|| "down".into());
                let amount = args.get("amount").and_then(Value::as_f64).unwrap_or(800.0);
                let r = s("ref").map(|r| js(&r)).unwrap_or_else(|| "null".into());
                let pos = self.eval_in(tab, &format!("return __atrium.scrollBy({}, {amount}, {r})", js(&dir))).await?;
                Ok(format!("Scrolled {dir} (now at {}).", pos.as_str().unwrap_or("?")))
            }
            other => bail!("unknown action `{other}`"),
        }
    }

    /// An invisible tab for background reads.
    async fn scratch(&self, url: &str) -> Result<Scratch> {
        let permit = self.scratch_slots.clone().acquire_owned().await?;
        let url = url.to_string();
        let id = self.browser.op(|reply| Op::NewTab { url: Some(url), background: true, scratch: true, reply }).await?;
        let scratch = Scratch { id, browser: self.browser.clone(), permit: Some(permit) };
        self.wait_loaded(id).await;
        Ok(scratch)
    }

    async fn close_scratch(&self, mut scratch: Scratch) {
        let id = scratch.id;
        let _ = self.browser.op(|reply| Op::Close { id, reply }).await;
        scratch.permit.take();
    }
}

fn cap_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= n {
        return s.to_string();
    }
    format!("{}\n- … ({} more; use snapshot for the full tree)", lines[..n].join("\n"), lines.len() - n)
}

/// Lines added/removed between two trees (refs are stable, so this is meaningful).
fn line_diff(old: &str, new: &str, cap: usize) -> String {
    let old_set: HashSet<&str> = old.lines().collect();
    let new_set: HashSet<&str> = new.lines().collect();
    let added: Vec<String> = new.lines().filter(|l| !old_set.contains(l)).map(|l| format!("+ {}", l.trim_start())).collect();
    let removed: Vec<String> = old.lines().filter(|l| !new_set.contains(l)).map(|l| format!("- {}", l.trim_start())).collect();
    let mut out: Vec<String> = Vec::new();
    let total = added.len() + removed.len();
    out.extend(removed.into_iter().take(cap / 3));
    out.extend(added.into_iter().take(cap - out.len().min(cap)));
    if total > out.len() {
        out.push(format!("… ({} more changed lines; snapshot for the full tree)", total - out.len()));
    }
    out.join("\n")
}

// Cancellation still closes temporary renderers before freeing their budget slot.
struct Scratch { id: u64, browser: Handle, permit: Option<tokio::sync::OwnedSemaphorePermit> }
impl Drop for Scratch {
    fn drop(&mut self) {
        let Some(permit) = self.permit.take() else { return };
        let id = self.id; let browser = self.browser.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move { let _ = browser.op(|reply| Op::Close { id, reply }).await; drop(permit); });
        }
    }
}
