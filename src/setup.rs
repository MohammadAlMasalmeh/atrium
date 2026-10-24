//! Setup APIs are only available to the local browser shell.
use crate::{app::Op, migration, opencode::OpenCode, server::App};
use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub fn routes(_app: Arc<App>) -> Router<Arc<App>> {
    Router::new()
        .route("/api/setup/profiles", get(profiles))
        .route("/api/setup/import", post(import))
        .route("/api/setup/opencode/connect", post(connect))
        .route("/api/setup/opencode/start", post(start))
        .route("/api/setup/google", get(google))
}

pub(crate) async fn shell_only(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let host = req.headers().get("host").and_then(|h| h.to_str().ok());
    let origin = req.headers().get("origin").and_then(|h| h.to_str().ok());
    if host != app.ui_origin.strip_prefix("http://") || origin.is_some_and(|o| o != app.ui_origin) {
        return (
            StatusCode::FORBIDDEN,
            "Setup is only available inside Atrium.",
        )
            .into_response();
    }
    next.run(req).await
}

/// Host/Origin checks stop websites; the per-launch token also stops other local processes.
pub(crate) async fn require_token(req: Request, next: Next) -> Response {
    let provided = req.headers().get(crate::TOKEN_HEADER).map(|h| h.as_bytes());
    if req.uri().path() != "/shell" && !token_matches(crate::API_TOKEN.get().map(String::as_bytes), provided) {
        return (StatusCode::UNAUTHORIZED, "A valid Atrium API token is required.").into_response();
    }
    next.run(req).await
}

fn token_matches(expected: Option<&[u8]>, provided: Option<&[u8]>) -> bool {
    let (Some(expected), Some(provided)) = (expected, provided) else { return false };
    expected.len() == provided.len()
        && !expected.is_empty()
        && expected.iter().zip(provided).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

fn response(result: anyhow::Result<Value>) -> Json<Value> {
    Json(result.unwrap_or_else(|e| json!({ "error": e.to_string() })))
}

async fn profiles() -> Json<Value> {
    response(
        tokio::task::spawn_blocking(migration::profiles)
            .await
            .unwrap_or_else(|e| Err(e.into())),
    )
}

async fn import(Json(req): Json<migration::ImportRequest>) -> Json<Value> {
    response(migration::import(req).await)
}

#[derive(Deserialize)]
struct ConnectRequest {
    url: String,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

fn validate_endpoint(raw: &str) -> anyhow::Result<String> {
    let url = reqwest::Url::parse(raw.trim())?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    anyhow::ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && local),
        "Use a local HTTP server or an HTTPS server."
    );
    anyhow::ensure!(
        url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == "/",
        "Enter the server origin, for example http://127.0.0.1:4097."
    );
    Ok(url.as_str().trim_end_matches('/').to_string())
}

async fn attach(app: &App, oc: &OpenCode) -> anyhow::Result<()> {
    let dir = app.workspace.parent().unwrap_or(&app.workspace);
    oc.add_mcp("atrium", crate::mcp_command(dir, &app.ui_origin))
        .await?;
    anyhow::ensure!(
        oc.browser_tools_ready().await,
        "OpenCode could not start the browser tools. Check the server and retry."
    );
    Ok(())
}

async fn connect(State(app): State<Arc<App>>, Json(req): Json<ConnectRequest>) -> Json<Value> {
    response(async {
        let _lock = app.setup_lock.lock().await;
        let url = validate_endpoint(&req.url)?;
        let candidate = OpenCode::with_auth(url.clone(), req.username.unwrap_or_else(|| "opencode".into()), req.password.filter(|p| !p.is_empty()));
        anyhow::ensure!(candidate.healthy().await, "Could not reach OpenCode. Check the address, server password, and that the server is running.");
        let (models, _) = candidate.models().await
            .map_err(|_| anyhow::anyhow!("The server responded, but its models could not be read. Check OpenCode and try again."))?;
        attach(&app, &candidate).await.map_err(|_| anyhow::anyhow!("OpenCode responded, but browser tools could not be connected. Try again."))?;
        // Remember the address only. Server passwords stay in memory.
        std::fs::write(app.workspace.parent().unwrap().join("opencode-server.json"), serde_json::to_vec(&json!({ "url": url }))?)?;
        app.oc.use_connection(&candidate);
        let _ = app.events.send(json!({ "type": "atrium.connection.ready" }));
        Ok(json!({ "ok": true, "models": models.len() }))
    }.await)
}

async fn start(State(app): State<Arc<App>>) -> Json<Value> {
    response(
        async {
            let _lock = app.setup_lock.lock().await;
            let local = OpenCode::new(format!("http://127.0.0.1:{}", app.opencode_port));
            if !local.healthy().await {
                // Release an exited child before trying again; never kill a user's server.
                {
                    let mut child = app.oc_child.lock().unwrap();
                    if let Some(c) = child.as_mut() {
                        if c.try_wait()?.is_none() {
                            anyhow::bail!("OpenCode is still starting. Wait a moment and retry.");
                        }
                    }
                    child.take();
                }
                let (_, child) = OpenCode::spawn(app.opencode_port, &app.workspace)
                    .await
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "OpenCode could not start. Install it, then choose Try again."
                        )
                    })?;
                *app.oc_child.lock().unwrap() = Some(child);
            }
            attach(&app, &local).await?;
            app.oc.use_connection(&local);
            let _ = app.events.send(json!({ "type": "atrium.connection.ready" }));
            std::fs::write(
                app.workspace.parent().unwrap().join("opencode-server.json"),
                serde_json::to_vec(&json!({ "url": local.base() }))?,
            )?;
            Ok(json!({ "ok": true }))
        }
        .await,
    )
}

async fn google(State(app): State<Arc<App>>) -> Json<Value> {
    response(
        async {
            let tabs = app.agent.browser.op(Op::Tabs).await?;
            for tab in tabs.tabs {
                if !reqwest::Url::parse(&tab.url)
                    .is_ok_and(|u| u.host_str() == Some("mail.google.com"))
                {
                    continue;
                }
                let result = app
                    .agent
                    .browser
                    .op(|reply| Op::InspectGoogle { tab: tab.id, reply })
                    .await;
                if matches!(result, Ok(Ok(Value::Bool(true)))) {
                    return Ok(json!({ "signed_in": true, "tab": tab.id }));
                }
            }
            Ok(json!({ "signed_in": false }))
        }
        .await,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn endpoints_keep_credentials_out_of_urls_and_require_remote_tls() {
        for url in [
            "http://example.com",
            "file:///tmp",
            "http://user:secret@localhost:4097",
            "http://localhost:4097/other",
            "https://example.com?secret=1",
        ] {
            assert!(validate_endpoint(url).is_err(), "{url}");
        }
        assert_eq!(
            validate_endpoint("http://127.0.0.1:4097/").unwrap(),
            "http://127.0.0.1:4097"
        );
        assert!(validate_endpoint("https://opencode.example.com").is_ok());
    }
    #[test]
    fn api_token_must_match_exactly() {
        assert!(token_matches(Some(b"abc123"), Some(b"abc123")));
        for provided in [None, Some(&b""[..]), Some(b"abc12"), Some(b"abc124"), Some(b"abc1234")] {
            assert!(!token_matches(Some(b"abc123"), provided));
        }
        assert!(!token_matches(None, Some(b"abc123")));
        assert!(!token_matches(Some(b""), Some(b"")));
    }
}
