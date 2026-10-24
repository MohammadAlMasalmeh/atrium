//! `atrium mcp`: a stdio MCP server that opencode launches. It exposes the
//! browser tools and forwards each call to the running browser, which owns
//! the webviews. Calls run concurrently so parallel reads stay parallel.

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::Mutex,
};

use crate::agent::tool_specs;

pub async fn serve(api: String) -> Result<()> {
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(180))
        .build()?;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let stdout = Arc::new(Mutex::new(tokio::io::stdout()));

    while let Some(line) = lines.next_line().await? {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = msg.get("id").cloned() else { continue }; // notification
        let (http, api, stdout) = (http.clone(), api.clone(), stdout.clone());
        tokio::spawn(async move {
            let params = &msg["params"];
            let result = match msg["method"].as_str().unwrap_or("") {
                "initialize" => Ok(json!({
                    "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2024-11-05"),
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "atrium", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": "Browser control for the user's browser window."
                })),
                "ping" => Ok(json!({})),
                "tools/list" => Ok(json!({ "tools": tool_specs() })),
                "tools/call" => {
                    let body = json!({ "name": params["name"], "args": params.get("arguments").cloned().unwrap_or(json!({})) });
                    Ok(match http.post(format!("{api}/api/tool")).json(&body).send().await {
                        Ok(resp) => resp.json::<Value>().await.unwrap_or_else(|e| error_result(&e.to_string())),
                        Err(e) => error_result(&format!("The browser is not reachable at {api}: {e}")),
                    })
                }
                other => Err(json!({ "code": -32601, "message": format!("method not found: {other}") })),
            };
            let reply = match result {
                Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
                Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
            };
            let mut out = stdout.lock().await;
            let _ = out.write_all(format!("{reply}\n").as_bytes()).await;
            let _ = out.flush().await;
        });
    }
    Ok(())
}

fn error_result(msg: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": msg }], "isError": true })
}
