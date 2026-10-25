//! One browser per data directory. A second launch hands its URLs to the running one.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{fs::File, path::Path, time::Duration};

const LOCK_FILE: &str = "instance.lock";
const INFO_FILE: &str = "instance.json";

/// Takes the data directory's lock. `None` means another instance owns it.
/// The lock is released when the returned file is dropped or the process exits.
pub fn claim(dir: &Path) -> Result<Option<File>> {
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(LOCK_FILE))
        .context("could not open the instance lock")?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e).context("could not lock the data directory"),
    }
}

pub fn publish(dir: &Path, port: u16) -> Result<()> {
    let temp = dir.join(format!("{INFO_FILE}.tmp"));
    std::fs::write(&temp, json!({ "pid": std::process::id(), "port": port }).to_string())?;
    std::fs::rename(&temp, dir.join(INFO_FILE))?;
    Ok(())
}

pub fn clear(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(INFO_FILE));
}

/// Command-line arguments as URLs; existing files become `file://` URLs.
pub fn urls(args: &[String]) -> Vec<String> {
    args.iter()
        .filter(|a| !a.trim().is_empty())
        .map(|arg| match std::fs::canonicalize(arg) {
            Ok(path) if !arg.contains("://") => reqwest::Url::from_file_path(&path)
                .map(String::from)
                .unwrap_or_else(|_| arg.clone()),
            _ => arg.clone(),
        })
        .collect()
}

/// Sends URLs to the instance that owns `dir`, waiting briefly if it is still starting.
pub async fn forward(dir: &Path, urls: &[String]) -> Result<()> {
    let http = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(10)).build()?;
    let mut last = anyhow::anyhow!("the running instance has not published its address");
    for _ in 0..60 {
        match send(&http, dir, urls).await {
            Ok(()) => {
                eprintln!("Aside is already running for this data directory; opened {} page(s) there", urls.len());
                return Ok(());
            }
            Err(e) => last = e,
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err(last).context("Aside is already running for this data directory but did not accept the request")
}

async fn send(http: &reqwest::Client, dir: &Path, urls: &[String]) -> Result<()> {
    let info: Value = serde_json::from_slice(&std::fs::read(dir.join(INFO_FILE))?)?;
    let port = info["port"].as_u64().context("instance.json has no port")?;
    let token = std::fs::read_to_string(dir.join(crate::TOKEN_FILE))?;
    if urls.is_empty() {
        return Ok(());
    }
    let reply: Value = http
        .post(format!("http://127.0.0.1:{port}/api/browser/action"))
        .header(crate::TOKEN_HEADER, token.trim())
        .json(&json!({ "cmd": "openUrls", "urls": urls }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if let Some(e) = reply["error"].as_str() {
        bail!("{e}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_claim_is_refused_until_the_first_is_released() {
        let dir = std::env::temp_dir().join(format!("aside-instance-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = claim(&dir).unwrap();
        assert!(first.is_some());
        assert!(claim(&dir).unwrap().is_none());
        drop(first);
        assert!(claim(&dir).unwrap().is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn files_become_file_urls() {
        let dir = std::env::temp_dir().join(format!("aside-urls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let page = dir.join("page.html");
        std::fs::write(&page, "<p>hi</p>").unwrap();
        let out = urls(&[page.to_string_lossy().into(), "https://example.com".into(), " ".into()]);
        assert_eq!(out.len(), 2);
        assert!(out[0].starts_with("file:///") && out[0].ends_with("/page.html"), "{out:?}");
        assert_eq!(out[1], "https://example.com");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
