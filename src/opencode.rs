//! Client for the `opencode serve` HTTP API.

use std::{path::Path, process::Stdio, sync::{Arc, RwLock}, time::Duration};

use anyhow::{bail, Context, Result};
use futures_util::{Stream, StreamExt};
use serde_json::{json, Value};
use tokio::process::{Child, Command};

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
    pub async fn spawn(port: u16, workdir: &Path) -> Result<(Self, Child)> {
        let bin = which_opencode();
        let child = Command::new(&bin)
            .args(["serve", "--hostname", "127.0.0.1", "--port", &port.to_string()])
            .current_dir(workdir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("could not start `{bin} serve` (is opencode installed?)"))?;
        let oc = Self::new(format!("http://127.0.0.1:{port}"));
        for _ in 0..80 {
            if oc.healthy().await {
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

/// Run OpenCode's official installer (installs to ~/.opencode/bin).
pub async fn install_opencode() -> Result<String> {
    let out = Command::new("/bin/bash")
        .args(["-c", "curl -fsSL https://opencode.ai/install | bash"])
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/usr/local/bin")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(300), out).await
        .context("The installer took too long. Check your connection and try again.")??;
    if !out.status.success() || find_opencode().is_none() {
        let err = String::from_utf8_lossy(&out.stderr);
        let last = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
        bail!("OpenCode could not be installed{}", if last.is_empty() { ".".into() } else { format!(": {last}") });
    }
    Ok(opencode_version().await.unwrap_or_default())
}
