//! atrium — a Rust re-creation of the Aside AI browser.
//!
//! `atrium` opens a native browser window (WKWebView tabs inside an Atrium-style
//! shell). Its built-in assistant is an `opencode serve` session that drives the
//! tabs through the browser tools exposed by `atrium mcp`.

mod agent;
mod app;
mod mcp;
mod opencode;
mod server;

use std::{collections::HashMap, io::Write, path::PathBuf, sync::Arc};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::{agent::Agent, opencode::OpenCode, server::App};

#[derive(Parser)]
#[command(name = "atrium", version, about = "An AI browser powered by opencode")]
struct Cli {
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
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
    let dir = dirs::home_dir().context("no home directory")?.join(".atrium-rs");
    std::fs::create_dir_all(dir.join("workspace"))?;
    Ok(dir)
}

fn run(rt: tokio::runtime::Runtime, args: RunArgs) -> Result<()> {
    let dir = data_dir()?;
    let ui = format!("http://127.0.0.1:{}", args.port);

    // The UI server must be listening before the window loads the shell.
    let listener = std::net::TcpListener::bind(("127.0.0.1", args.port))
        .with_context(|| format!("port {} is busy (is Atrium already running?)", args.port))?;
    listener.set_nonblocking(true)?;

    // opencode: reuse a running server or start our own.
    let (oc, oc_child) = rt.block_on(async {
        if let Some(url) = &args.opencode_url {
            return anyhow::Ok((OpenCode::new(url.clone()), None));
        }
        let existing = OpenCode::new(format!("http://127.0.0.1:{}", args.opencode_port));
        if existing.healthy().await {
            return Ok((existing, None));
        }
        eprintln!("starting opencode server on port {}…", args.opencode_port);
        let (oc, child) = OpenCode::spawn(args.opencode_port, &dir.join("workspace")).await?;
        Ok((oc, Some(child)))
    })?;
    let oc_child = std::sync::Mutex::new(oc_child);

    app::run(ui.clone(), move |browser| {
        let (tx, _) = tokio::sync::broadcast::channel(1024);
        let app = Arc::new(App { agent: Agent::new(browser), oc: oc.clone(), events: tx });
        rt.spawn(async move {
            let listener = match tokio::net::TcpListener::from_std(listener) {
                Ok(l) => l,
                Err(e) => return eprintln!("ui server: {e}"),
            };
            server::spawn_event_pump(app.clone());
            let router = server::router(app.clone());
            tokio::spawn(async move { axum::serve(listener, router).await });

            // Give the opencode agent the browser tools.
            let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "atrium".into());
            if let Err(e) = oc.add_mcp("atrium", vec![exe, "mcp".into(), "--api".into(), ui]).await {
                eprintln!("could not register browser tools with opencode: {e}");
            }
        });
        // Keep the runtime alive for the life of the process.
        std::mem::forget(rt);
    }, move || {
        if let Some(mut c) = oc_child.lock().unwrap().take() {
            let _ = c.start_kill();
        }
    })
}

/// Terminal client: subscribe to the event relay, send the prompt, print the reply.
async fn ask(port: u16, prompt: String, session: Option<String>, model: Option<String>) -> Result<()> {
    if prompt.trim().is_empty() {
        bail!("usage: atrium ask <prompt>");
    }
    let base = format!("http://127.0.0.1:{port}");
    let http = reqwest::Client::builder().no_proxy().build()?;
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
