//! Client for the `opencode serve` HTTP API.

use std::{path::Path, process::Stdio, sync::{Arc, RwLock}, time::{Duration, Instant}};

use anyhow::{bail, Context, Result};
use futures_util::{Stream, StreamExt};
use serde_json::{json, Value};
use tokio::{io::{AsyncRead, AsyncReadExt}, process::{Child, Command}, task::JoinHandle};

/// Receives setup output (`atrium.setup.log` / `atrium.setup.progress` events) for the onboarding terminal.
pub type Log = Arc<dyn Fn(Value) + Send + Sync>;

pub fn log_line(log: &Log, kind: &str, text: impl Into<String>) {
    log(json!({ "type": "atrium.setup.log", "kind": kind, "text": text.into() }));
}

pub const INSTALL_CMD: &str = "curl -fsSL https://opencode.ai/install | bash";

#[derive(Clone)]
pub struct OpenCode {
    connection: Arc<RwLock<Connection>>,
    pub changed: Arc<tokio::sync::Notify>,
    http: reqwest::Client,
}

#[derive(Clone)]
struct Connection {
    base: String,
    username: String,
    password: Option<String>,
}

impl OpenCode {
    pub fn new(base: impl Into<String>) -> Self {
        Self::with_auth(base.into(), std::env::var("OPENCODE_SERVER_USERNAME").unwrap_or_else(|_| "opencode".into()),
            std::env::var("OPENCODE_SERVER_PASSWORD").ok())
    }

    pub fn with_auth(base: String, username: String, password: Option<String>) -> Self {
        Self {
            connection: Arc::new(RwLock::new(Connection { base: base.trim_end_matches('/').into(), username, password })),
            changed: Arc::new(tokio::sync::Notify::new()),
            http: reqwest::Client::builder().no_proxy().connect_timeout(Duration::from_secs(3))
                .redirect(reqwest::redirect::Policy::none()).build().expect("http client"),
        }
    }

    pub fn base(&self) -> String { self.connection.read().unwrap().base.clone() }

    pub fn use_connection(&self, other: &Self) {
        *self.connection.write().unwrap() = other.connection.read().unwrap().clone();
        self.changed.notify_one();
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let c = self.connection.read().unwrap();
        let req = self.http.request(method, format!("{}{path}", c.base));
        match &c.password { Some(password) => req.basic_auth(&c.username, Some(password)), None => req }
    }

    pub async fn healthy(&self) -> bool {
        let response = self.request(reqwest::Method::GET, "/global/health")
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .ok()
            .filter(|r| r.status().is_success())
            .map(|r| r.json::<Value>());
        match response {
            Some(body) => body.await.is_ok_and(|v| v["healthy"] == true),
            None => false,
        }
    }

    /// Spawn `opencode serve` on `port` with `workdir` as its project directory.
    /// Startup output goes to `log` for a minute; after that it is drained silently.
    pub async fn spawn(port: u16, workdir: &Path, log: Log) -> Result<(Self, Child)> {
        let bin = which_opencode();
        log_line(&log, "cmd", format!("opencode serve --hostname 127.0.0.1 --port {port}"));
        let started = Instant::now();
        let mut child = Command::new(&bin)
            .args(["serve", "--hostname", "127.0.0.1", "--port", &port.to_string()])
            .current_dir(workdir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("could not start `{bin} serve` (is opencode installed?)"))?;
        let until = Some(started + Duration::from_secs(60));
        if let Some(out) = child.stdout.take() { pump(out, log.clone(), until); }
        if let Some(err) = child.stderr.take() { pump(err, log.clone(), until); }
        let oc = Self::new(format!("http://127.0.0.1:{port}"));
        for _ in 0..80 {
            if oc.healthy().await {
                log_line(&log, "ok", format!("Server is up ({:.1}s)", started.elapsed().as_secs_f32()));
                return Ok((oc, child));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        bail!("opencode server did not come up on port {port}")
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Value> {
        let resp = req.timeout(Duration::from_secs(12)).send().await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            bail!("opencode API error {status}: {body}");
        }
        Ok(if body.is_empty() { Value::Null } else { serde_json::from_str(&body).unwrap_or(Value::String(body)) })
    }

    async fn get(&self, path: &str) -> Result<Value> {
        self.send(self.request(reqwest::Method::GET, path)).await
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.send(self.request(reqwest::Method::POST, path).json(&body)).await
    }

    pub async fn add_mcp(&self, name: &str, command: Vec<String>) -> Result<Value> {
        self.post(
            "/mcp",
            json!({ "name": name, "config": { "type": "local", "command": command, "enabled": true, "timeout": 15000 } }),
        )
        .await
    }

    pub async fn browser_tools_ready(&self) -> bool {
        self.get("/mcp").await.is_ok_and(|v| v["atrium"]["status"] == "connected")
    }

    /// Returns (models as "provider/model" with display names, default "provider/model").
    pub async fn models(&self) -> Result<(Vec<(String, String)>, Option<String>)> {
        let v = self.get("/config/providers").await?;
        let config = self.get("/config").await.unwrap_or(Value::Null);
        let mut out = Vec::new();
        for p in v["providers"].as_array().into_iter().flatten() {
            let pid = p["id"].as_str().unwrap_or_default();
            let pname = p["name"].as_str().unwrap_or(pid);
            for (mid, m) in p["models"].as_object().into_iter().flatten() {
                let mname = m["name"].as_str().unwrap_or(mid);
                out.push((format!("{pid}/{mid}"), format!("{pname} · {mname}")));
            }
        }
        // Prefer the user's configured model, then a configured (non-free) provider's default.
        let default = config["model"].as_str().map(str::to_string).or_else(|| {
            let defaults = v["default"].as_object()?;
            let pick = defaults.iter().find(|(p, _)| p.as_str() != "opencode").or_else(|| defaults.iter().next())?;
            Some(format!("{}/{}", pick.0, pick.1.as_str()?))
        });
        Ok((out, default))
    }

    pub async fn create_session(&self, title: &str) -> Result<String> {
        let v = self.post("/session", json!({ "title": title })).await?;
        Ok(v["id"].as_str().context("session id missing")?.to_string())
    }

    pub async fn prompt(&self, session: &str, text: &str, model: Option<&str>, system: &str) -> Result<()> {
        let mut body = json!({
            "parts": [{ "type": "text", "text": text }],
            "system": system,
            // The agent asks in chat rather than through opencode's blocking question UI.
            "tools": { "question": false },
        });
        if let Some((provider, model)) = model.and_then(|m| m.split_once('/')) {
            body["model"] = json!({ "providerID": provider, "modelID": model });
            // A browsing agent doesn't need opencode's coding tools. OpenCode's own
            // free tier rejects requests without them, so only strip them elsewhere.
            if provider != "opencode" {
                for t in ["bash", "edit", "write", "patch", "multiedit", "read", "glob", "grep", "list", "lsp", "task"] {
                    body["tools"][t] = json!(false);
                }
            }
        }
        self.post(&format!("/session/{session}/prompt_async"), body).await?;
        Ok(())
    }

    pub async fn abort(&self, session: &str) -> Result<()> {
        self.post(&format!("/session/{session}/abort"), json!({})).await?;
        Ok(())
    }

    /// Recent top-level sessions, newest first, with busy state and a short
    /// summary (the start of the last assistant reply) for the first few.
    pub async fn session_summaries(&self, limit: usize) -> Result<Value> {
        let list = self.get("/session").await?;
        let status = self.get("/session/status").await.unwrap_or(Value::Null);
        let mut sessions: Vec<&Value> = list
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| s.get("parentID").map_or(true, Value::is_null))
            .collect();
        sessions.sort_by_key(|s| std::cmp::Reverse(s["time"]["updated"].as_i64().unwrap_or(0)));
        sessions.truncate(limit);
        let mut out = Vec::new();
        for (i, s) in sessions.into_iter().enumerate() {
            let id = s["id"].as_str().unwrap_or_default();
            let busy = status[id]["type"].as_str().map_or(false, |t| t != "idle");
            let summary = if i < 6 { self.last_reply(id).await.unwrap_or_default() } else { String::new() };
            out.push(json!({
                "id": id,
                "title": s["title"].as_str().unwrap_or("Untitled"),
                "updated": s["time"]["updated"],
                "busy": busy,
                "summary": summary,
            }));
        }
        Ok(Value::Array(out))
    }

    async fn last_reply(&self, session: &str) -> Result<String> {
        let msgs = self.messages(session).await?;
        let text = msgs
            .as_array()
            .into_iter()
            .flatten()
            .rev()
            .filter(|m| m["info"]["role"] == "assistant")
            .flat_map(|m| m["parts"].as_array().cloned().unwrap_or_default().into_iter().rev())
            .find_map(|p| (p["type"] == "text").then(|| p["text"].as_str().unwrap_or("").to_string()))
            .unwrap_or_default();
        Ok(text.chars().filter(|c| *c != '*' && *c != '#').take(180).collect())
    }

    pub async fn messages(&self, session: &str) -> Result<Value> {
        self.get(&format!("/session/{session}/message")).await
    }

    pub async fn reply_permission(&self, id: &str, reply: &str) -> Result<()> {
        self.post(&format!("/permission/{id}/reply"), json!({ "reply": reply })).await?;
        Ok(())
    }

    /// The server's event bus (SSE), as parsed JSON events.
    pub async fn events(&self) -> Result<impl Stream<Item = Value>> {
        let resp = self.request(reqwest::Method::GET, "/event")
            .header("accept", "text/event-stream")
            .send()
            .await?
            .error_for_status()?;
        let mut bytes = resp.bytes_stream();
        Ok(async_stream(move |tx| async move {
            let mut buf = String::new();
            while let Some(Ok(chunk)) = bytes.next().await {
                buf.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(end) = buf.find("\n\n") {
                    let frame: String = buf.drain(..end + 2).collect();
                    let data: String = frame
                        .lines()
                        .filter_map(|l| l.strip_prefix("data:"))
                        .map(str::trim_start)
                        .collect::<Vec<_>>()
                        .join("\n");
                    if let Ok(v) = serde_json::from_str::<Value>(&data) {
                        if tx.send(v).await.is_err() {
                            return;
                        }
                    }
                }
            }
        }))
    }
}

/// Tiny channel-backed stream builder (avoids pulling in async-stream).
fn async_stream<F, Fut>(f: F) -> impl Stream<Item = Value>
where
    F: FnOnce(tokio::sync::mpsc::Sender<Value>) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel(256);
    tokio::spawn(f(tx));
    tokio_stream::wrappers::ReceiverStream::new(rx)
}

/// Where `opencode` lives, if installed. Apps launched from Finder don't get the
/// login shell's PATH, so check the installer's and package managers' locations too.
pub fn find_opencode() -> Option<std::path::PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    let mut dirs = vec![home.join(".opencode/bin"), "/opt/homebrew/bin".into(), "/usr/local/bin".into(),
        home.join(".bun/bin"), home.join(".npm-global/bin"), home.join(".local/bin")];
    dirs.extend(std::env::var_os("PATH").iter().flat_map(std::env::split_paths));
    dirs.into_iter().map(|d| d.join("opencode")).find(|p| p.is_file())
}

fn which_opencode() -> String {
    find_opencode().map_or_else(|| "opencode".into(), |p| p.to_string_lossy().into_owned())
}

pub async fn opencode_version() -> Option<String> {
    let bin = find_opencode()?;
    let out = Command::new(bin).arg("--version").output();
    let out = tokio::time::timeout(Duration::from_secs(5), out).await.ok()?.ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|v| !v.is_empty())
}

/// Run OpenCode's official installer (installs to ~/.opencode/bin), streaming its output to `log`.
pub async fn install_opencode(log: Log) -> Result<String> {
    log_line(&log, "cmd", INSTALL_CMD);
    let mut child = Command::new("/bin/bash")
        .args(["-c", INSTALL_CMD])
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/usr/local/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("Could not run the OpenCode installer.")?;
    let out = pump(child.stdout.take().context("installer stdout")?, log.clone(), None);
    let err = pump(child.stderr.take().context("installer stderr")?, log.clone(), None);
    let status = tokio::time::timeout(Duration::from_secs(300), child.wait()).await
        .context("The installer took too long. Check your connection and try again.")??;
    let _ = out.await;
    let last = err.await.unwrap_or_default();
    if !status.success() || find_opencode().is_none() {
        bail!("OpenCode could not be installed{}", if last.is_empty() { ".".into() } else { format!(": {last}") });
    }
    let version = opencode_version().await.unwrap_or_default();
    log_line(&log, "ok", format!("Installed OpenCode {version}").trim_end().to_string());
    Ok(version)
}

/// Forward a child's output to `log` line by line (until `until`, then just drain it so the
/// child never blocks on a full pipe). curl's `-#` progress lines become progress events.
/// Resolves to the last non-empty line, for error messages.
fn pump(mut r: impl AsyncRead + Unpin + Send + 'static, log: Log, until: Option<Instant>) -> JoinHandle<String> {
    tokio::spawn(async move {
        let (mut buf, mut chunk) = (Vec::new(), [0u8; 4096]);
        let (mut last, mut pct, mut after_cr) = (String::new(), -1i32, false);
        let mut line = |raw: &[u8], log: &Log| {
            let live = until.is_none_or(|t| Instant::now() < t);
            let text = String::from_utf8_lossy(raw).into_owned();
            let plain = strip_ansi(&text);
            if let Some(p) = curl_progress(&plain) {
                if live && p as i32 != pct { log(json!({ "type": "atrium.setup.progress", "pct": p })); }
                pct = p as i32;
                return;
            }
            if !plain.trim().is_empty() { last = plain.trim().to_string(); }
            if live { log_line(log, "out", text); }
        };
        loop {
            let n = r.read(&mut chunk).await.unwrap_or(0);
            if n == 0 { break; }
            buf.extend_from_slice(&chunk[..n]);
            while let Some(i) = buf.iter().position(|&b| b == b'\n' || b == b'\r') {
                let seg: Vec<u8> = buf.drain(..=i).collect();
                let cr = seg[i] == b'\r';
                // `\r\n` is one line break, and a bare `\r` (curl redrawing) is not a line.
                if !(i == 0 && (cr || after_cr)) { line(&seg[..i], &log); }
                after_cr = cr;
            }
        }
        if !buf.is_empty() { line(&buf, &log); }
        last
    })
}

fn strip_ansi(s: &str) -> String {
    let (mut out, mut chars) = (String::with_capacity(s.len()), s.chars());
    while let Some(c) = chars.next() {
        if c != '\x1b' { out.push(c); continue; }
        if chars.next() == Some('[') {
            for c in chars.by_ref() { if ('@'..='~').contains(&c) { break; } }
        }
    }
    out
}

/// `######              42.3%` → 42.3
fn curl_progress(line: &str) -> Option<f32> {
    let n = line.trim().strip_suffix('%')?.trim_start_matches(['#', ' ']);
    n.parse::<f32>().ok().filter(|p| (0.0..=100.0).contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_curl_progress_and_ignores_prose() {
        assert_eq!(curl_progress("######                    42.3%"), Some(42.3));
        assert_eq!(curl_progress("                           0.0%"), Some(0.0));
        assert_eq!(curl_progress("################## 100.0%"), Some(100.0));
        assert_eq!(curl_progress("Downloading 50%"), None);
        assert_eq!(curl_progress("Installing opencode"), None);
    }

    #[test]
    fn strips_installer_colours() {
        assert_eq!(strip_ansi("\x1b[0;2mInstalling \x1b[0mopencode\x1b[0m"), "Installing opencode");
        assert_eq!(strip_ansi("\x1b[38;5;214m###  12%\x1b[0m\x1b[?25l"), "###  12%");
    }

    #[tokio::test]
    async fn pump_splits_lines_and_reports_progress() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let log: Log = Arc::new(move |v| sink.lock().unwrap().push(v));
        let input: &[u8] = b"hello\r\n\n\r###  10.0%\r#####  55.0%\r\nfailed: no network\n";
        let last = pump(input, log, None).await.unwrap();
        assert_eq!(last, "failed: no network");
        let seen = seen.lock().unwrap();
        let texts: Vec<_> = seen.iter().filter_map(|v| v["text"].as_str()).collect();
        assert_eq!(texts, ["hello", "", "failed: no network"]);
        let pcts: Vec<_> = seen.iter().filter_map(|v| v["pct"].as_f64()).collect();
        assert_eq!(pcts, [10.0, 55.0]);
    }
}
