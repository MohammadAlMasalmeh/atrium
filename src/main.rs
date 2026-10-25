//! atrium — a Rust re-creation of the Aside AI browser.
//!
//! `atrium` opens a native browser window with Chromium tabs. Its built-in
//! assistant is an `opencode serve` session that drives the
//! tabs through the browser tools exposed by `atrium mcp`.

mod agent;
mod app;
mod extensions;
mod mcp;
mod migration;
mod memory;
mod opencode;
mod permissions;
mod server;
mod setup;

use std::{collections::HashMap, io::Write, path::PathBuf, sync::Arc};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::{agent::Agent, opencode::OpenCode, server::App};

#[derive(Parser)]
#[command(name = "atrium", version, about = "An AI browser powered by opencode")]
struct Cli {
    /// Keep browser and assistant data in an isolated directory.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open the browser (default).
    Run(RunArgs),
    /// Stdio MCP server exposing the browser tools (started by opencode).
    Mcp {
        /// Base URL of the running browser's local API.
        #[arg(long, default_value = "http://127.0.0.1:7777")]
        api: String,
    },
    /// Send one prompt to the running browser's assistant and stream the answer here.
    Ask {
        /// What to ask.
        prompt: Vec<String>,
        #[arg(long, default_value_t = 7777)]
        port: u16,
        /// Continue an existing session.
        #[arg(long)]
        session: Option<String>,
        /// Model as provider/model, e.g. opencode/big-pickle.
        #[arg(long)]
        model: Option<String>,
    },
}

#[derive(Args)]
struct RunArgs {
    /// Port for the browser's local UI/API server.
    #[arg(long, default_value_t = 7777)]
    port: u16,
    /// Use an already running opencode server instead of starting one.
    #[arg(long)]
    opencode_url: Option<String>,
    /// Port for the opencode server we start.
    #[arg(long, default_value_t = 4097)]
    opencode_port: u16,
    /// Load an unpacked Chromium extension from a local directory.
    #[arg(long = "extension")]
    extensions: Vec<PathBuf>,
}

static CHROME_EXTENSIONS: std::sync::OnceLock<Vec<PathBuf>> = std::sync::OnceLock::new();
static DATA_ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
/// Per-launch secret for the local API, also written to `<data dir>/api-token` (mode 0600).
static API_TOKEN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
const TOKEN_HEADER: &str = "x-atrium-token";
const TOKEN_FILE: &str = "api-token";

fn write_api_token(dir: &std::path::Path) -> Result<String> {
    use std::os::unix::fs::OpenOptionsExt;
    let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    let path = dir.join(TOKEN_FILE);
    let temp = dir.join(format!("{TOKEN_FILE}.tmp"));
    let _ = std::fs::remove_file(&temp);
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
    file.write_all(token.as_bytes())?;
    drop(file);
    std::fs::rename(&temp, &path)?;
    Ok(token)
}

/// The running browser's token, for clients started separately (`atrium mcp`, `atrium ask`).
fn read_api_token() -> Option<String> {
    std::env::var("ATRIUM_API_TOKEN").ok().filter(|t| !t.is_empty()).or_else(|| {
        let token = std::fs::read_to_string(data_dir().ok()?.join(TOKEN_FILE)).ok()?;
        Some(token.trim().to_string()).filter(|t| !t.is_empty())
    })
}

/// Scripting the shell is for debug builds and test instances started with
/// `ATRIUM_TEST_SHELL_EVAL=1`; it still requires the API token.
fn shell_eval_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| cfg!(debug_assertions) || std::env::var_os("ATRIUM_TEST_SHELL_EVAL").is_some_and(|v| v == "1"))
}

/// Session URLs, the server address, and the Chromium profile stay private to this user.
fn restrict_data_dir(dir: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if dirs::home_dir().and_then(|h| h.canonicalize().ok()).as_deref() == Some(dir) {
        return Ok(());
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// The MCP bridge reads the token from the data directory so it never appears in process arguments.
fn mcp_command(dir: &std::path::Path, api: &str) -> Vec<String> {
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "atrium".into());
    vec![exe, "--data-dir".into(), dir.to_string_lossy().into_owned(), "mcp".into(), "--api".into(), api.into()]
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(root) = cli.data_dir { let _ = DATA_ROOT.set(root); }
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    match cli.cmd {
        Some(Cmd::Mcp { api }) => rt.block_on(mcp::serve(api)),
        Some(Cmd::Ask { prompt, port, session, model }) => rt.block_on(ask(port, prompt.join(" "), session, model)),
        Some(Cmd::Run(args)) => run(rt, args),
        None => match Cli::parse_from(["atrium", "run"]).cmd {
            Some(Cmd::Run(args)) => run(rt, args),
            _ => unreachable!(),
        },
    }
}

fn data_dir() -> Result<PathBuf> {
    let dir = match DATA_ROOT.get() { Some(root) => root.clone(), None => dirs::home_dir().context("no home directory")?.join(".atrium-rs") };
    std::fs::create_dir_all(dir.join("workspace"))?;
    Ok(dir.canonicalize()?)
}

fn run(rt: tokio::runtime::Runtime, args: RunArgs) -> Result<()> {
    let dir = data_dir()?;
    restrict_data_dir(&dir).context("could not restrict access to the data directory")?;
    let mut extensions = Vec::new();
    for extension in &args.extensions {
        let path = extension.canonicalize().context("Extension directory does not exist")?;
        anyhow::ensure!(path.to_str().is_some_and(|p| !p.contains(',')), "Extension paths must be UTF-8 and cannot contain commas");
        let manifest: Value = serde_json::from_slice(&std::fs::read(path.join("manifest.json"))?)?;
        anyhow::ensure!(matches!(manifest["manifest_version"].as_u64(), Some(2 | 3)), "Extension manifest must be version 2 or 3");
        extensions.push(path);
    }
    extensions::record_review(&dir, &extensions);
    migration::search::load(&dir.join("import"));
    let _ = CHROME_EXTENSIONS.set(extensions);
    let ui = format!("http://127.0.0.1:{}", args.port);

    // The UI server must be listening before the window loads the shell.
    let listener = std::net::TcpListener::bind(("127.0.0.1", args.port))
        .with_context(|| format!("port {} is busy (is Atrium already running?)", args.port))?;
    listener.set_nonblocking(true)?;
    let _ = API_TOKEN.set(write_api_token(&dir).context("could not write the local API token")?);
    let token_dir = dir.clone();

    // Opening the browser must not wait for an optional AI server to start.
    let local_url = format!("http://127.0.0.1:{}", args.opencode_port);
    let saved_url = std::fs::read(dir.join("opencode-server.json")).ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|saved| saved["url"].as_str().map(str::to_string));
    let endpoint = args.opencode_url.clone().or(saved_url).unwrap_or_else(|| local_url.clone());
    let auto_start = args.opencode_url.is_none() && endpoint == local_url;
    let oc = if endpoint == local_url { OpenCode::local(args.opencode_port) } else { OpenCode::new(endpoint) };
    let oc_child = Arc::new(std::sync::Mutex::new(None));
    let app_child = oc_child.clone();

    app::run(ui.clone(), move |browser| {
        let (tx, _) = tokio::sync::broadcast::channel(1024);
        let app = Arc::new(App { agent: Agent::new(browser), oc: oc.clone(), events: tx,
            ui_origin: ui.clone(), oc_child: app_child, setup_lock: tokio::sync::Mutex::new(()),
            workspace: dir.join("workspace"), opencode_port: args.opencode_port });
        rt.spawn(async move {
            let listener = match tokio::net::TcpListener::from_std(listener) {
                Ok(l) => l,
                Err(e) => return eprintln!("ui server: {e}"),
            };
            server::spawn_event_pump(app.clone());
            let router = server::router(app.clone());
            tokio::spawn(async move { axum::serve(listener, router).await });

            // Serialize automatic startup with the onboarding's retry/connect actions.
            let _setup = app.setup_lock.lock().await;
            if auto_start && !oc.healthy().await {
                match OpenCode::spawn(args.opencode_port, &app.workspace).await {
                    Ok((local, child)) => {
                        *app.oc_child.lock().unwrap() = Some(child);
                        oc.use_connection(&local);
                    }
                    Err(e) => eprintln!("OpenCode is unavailable; browsing remains available: {e}"),
                }
            }

            // Give the opencode agent the browser tools.
            let command = mcp_command(app.workspace.parent().unwrap_or(&app.workspace), &ui);
            if let Err(e) = oc.add_mcp("atrium", command).await {
                eprintln!("could not register browser tools with opencode: {e}");
            }
            let _ = app.events.send(json!({ "type": "atrium.connection.ready" }));
        });
        // Keep the runtime alive for the life of the process.
        std::mem::forget(rt);
    }, move || {
        if let Some(mut c) = oc_child.lock().unwrap().take() {
            let _ = c.start_kill();
        }
        let _ = std::fs::remove_file(token_dir.join(TOKEN_FILE));
    })
}

/// Terminal client: subscribe to the event relay, send the prompt, print the reply.
async fn ask(port: u16, prompt: String, session: Option<String>, model: Option<String>) -> Result<()> {
    if prompt.trim().is_empty() {
        bail!("usage: atrium ask <prompt>");
    }
    let base = format!("http://127.0.0.1:{port}");
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(token) = read_api_token().and_then(|t| t.parse().ok()) {
        headers.insert(TOKEN_HEADER, token);
    }
    let http = reqwest::Client::builder().no_proxy().default_headers(headers).build()?;
    let events = http
        .get(format!("{base}/api/events"))
        .send()
        .await
        .with_context(|| format!("atrium is not running on {base} (start it with `atrium run`)"))?;
    let mut bytes = events.bytes_stream();

    let r: Value = http
        .post(format!("{base}/api/chat"))
        .json(&json!({ "sessionID": session, "text": prompt, "model": model }))
        .send()
        .await?
        .json()
        .await?;
    if let Some(e) = r["error"].as_str() {
        bail!("{e}");
    }
    let sid = r["sessionID"].as_str().unwrap_or_default().to_string();
    eprintln!("session {sid}");

    let mut roles: HashMap<String, String> = HashMap::new();
    let mut part_kind: HashMap<String, String> = HashMap::new();
    let mut tool_status: HashMap<String, String> = HashMap::new();
    let mut buf = String::new();
    let mut out = std::io::stdout();
    while let Some(chunk) = bytes.next().await {
        buf.push_str(&String::from_utf8_lossy(&chunk?));
        while let Some(end) = buf.find("\n\n") {
            let frame: String = buf.drain(..end + 2).collect();
            let Some(data) = frame.lines().find_map(|l| l.strip_prefix("data:")) else { continue };
            let Ok(ev) = serde_json::from_str::<Value>(data.trim()) else { continue };
            let p = &ev["properties"];
            if p["sessionID"].as_str() != Some(&sid) && p["part"]["sessionID"].as_str() != Some(&sid) {
                continue;
            }
            match ev["type"].as_str().unwrap_or("") {
                "message.updated" => {
                    let info = &p["info"];
                    roles.insert(info["id"].as_str().unwrap_or("").into(), info["role"].as_str().unwrap_or("").into());
                    if let Some(err) = info.get("error") {
                        let msg = err["data"]["message"].as_str().unwrap_or("model error");
                        println!("\n✗ {msg}");
                    }
                }
                "message.part.updated" => {
                    let part = &p["part"];
                    let id = part["id"].as_str().unwrap_or("").to_string();
                    let kind = part["type"].as_str().unwrap_or("").to_string();
                    part_kind.insert(id.clone(), kind.clone());
                    if kind == "tool" {
                        let status = part["state"]["status"].as_str().unwrap_or("").to_string();
                        if tool_status.get(&id) != Some(&status) && status != "pending" {
                            let name = part["tool"].as_str().unwrap_or("tool").trim_start_matches("atrium_");
                            let input = part["state"]["input"].to_string();
                            let input: String = input.chars().take(100).collect();
                            match status.as_str() {
                                "running" => println!("\n  ⏺ {name} {input}"),
                                "error" => println!("  ✗ {name}: {}", part["state"]["error"].as_str().unwrap_or("")),
                                _ => {}
                            }
                            tool_status.insert(id, status);
                        }
                    }
                }
                "message.part.delta" => {
                    let msg = p["messageID"].as_str().unwrap_or("");
                    let part = p["partID"].as_str().unwrap_or("");
                    let is_text = part_kind.get(part).map(|k| k == "text").unwrap_or(true);
                    if roles.get(msg).map(String::as_str) != Some("user") && is_text && p["field"] == "text" {
                        print!("{}", p["delta"].as_str().unwrap_or(""));
                        out.flush().ok();
                    }
                }
                "session.error" => {
                    let msg = p["error"]["data"]["message"].as_str().unwrap_or("session error");
                    println!("\n✗ {msg}");
                }
                "session.idle" => {
                    println!();
                    return Ok(());
                }
                _ => {}
            }
        }
    }
    Ok(())
}
