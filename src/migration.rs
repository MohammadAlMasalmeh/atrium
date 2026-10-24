//! Read-only, opt-in Chrome migration. Never reads cookies or credential stores.
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Deserialize)]
pub struct ImportRequest {
    pub profile: String,
    #[serde(default)]
    pub bookmarks: bool,
    #[serde(default)]
    pub history: bool,
    #[serde(default)]
    pub extensions: bool,
}

#[derive(Clone, Serialize)]
pub struct Bookmark {
    url: String,
    title: String,
    folder: String,
}

fn chrome_root() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("No home directory")?
        .join("Library/Application Support/Google/Chrome"))
}

fn valid_profile(id: &str) -> bool {
    id == "Default"
        || id
            .strip_prefix("Profile ")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
}

fn profile_at(root: &Path, id: &str) -> Result<PathBuf> {
    if !valid_profile(id) {
        bail!("Choose a detected Chrome profile.");
    }
    let root = root
        .canonicalize()
        .context("Chrome was not found on this Mac.")?;
    let path = root
        .join(id)
        .canonicalize()
        .context("This Chrome profile is no longer available.")?;
    if path.parent() != Some(root.as_path()) || !path.is_dir() {
        bail!("Choose a detected Chrome profile.");
    }
    Ok(path)
}

fn read_json(path: &Path) -> Result<Value> {
    let metadata = path.metadata()?;
    if metadata.len() > 32 * 1024 * 1024 {
        bail!("This Chrome file is too large to import.");
    }
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

pub fn profiles() -> Result<Value> {
    let root = chrome_root()?;
    if !root.exists() {
        return Ok(json!({ "profiles": [] }));
    }
    let state = read_json(&root.join("Local State")).unwrap_or(Value::Null);
    let mut profiles = Vec::new();
    for entry in std::fs::read_dir(&root)
        .context("Chrome profiles could not be read. Check file access and try again.")?
    {
        let entry = entry?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if !valid_profile(&id) || profile_at(&root, &id).is_err() {
            continue;
        }
        let info = &state["profile"]["info_cache"][&id];
        profiles.push(json!({ "id": id, "name": info["name"].as_str().unwrap_or(&id) }));
    }
    profiles.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    Ok(json!({ "profiles": profiles }))
}

fn web_url(url: &str) -> bool {
    reqwest::Url::parse(url)
        .is_ok_and(|u| matches!(u.scheme(), "https" | "http") && u.host_str().is_some())
}

fn walk_bookmarks(node: &Value, folder: &str, out: &mut Vec<Bookmark>, seen: &mut HashSet<String>) {
    if out.len() >= 10000 {
        return;
    }
    if node["type"] == "url" {
        let url = node["url"].as_str().unwrap_or_default();
        if web_url(url) && seen.insert(url.to_string()) {
            out.push(Bookmark {
                url: url.into(),
                title: node["name"].as_str().unwrap_or(url).into(),
                folder: folder.into(),
            });
        }
    } else if let Some(children) = node["children"].as_array() {
        let name = node["name"].as_str().unwrap_or_default();
        let folder = if folder.is_empty() {
            name.to_string()
        } else {
            format!("{folder} / {name}")
        };
        for child in children {
            walk_bookmarks(child, &folder, out, seen);
        }
    }
}

fn bookmarks(path: &Path) -> Result<Vec<Bookmark>> {
    let value = read_json(&path.join("Bookmarks"))?;
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (_, root) in value["roots"]
        .as_object()
        .context("Chrome bookmarks could not be read.")?
    {
        walk_bookmarks(root, "", &mut out, &mut seen);
    }
    Ok(out)
}

fn version_key(path: &Path) -> Vec<u64> {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .split(['.', '_'])
        .map(|p| p.parse().unwrap_or(0))
        .collect()
}

fn extensions(path: &Path) -> Result<Vec<Value>> {
    let root = path.join("Extensions");
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if id.len() != 32 || !id.bytes().all(|c| (b'a'..=b'p').contains(&c)) {
            continue;
        }
        let mut versions: Vec<PathBuf> = std::fs::read_dir(entry.path())?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        versions.sort_by_key(|p| version_key(p));
        let Some(dir) = versions.last() else { continue };
        let Ok(manifest) = read_json(&dir.join("manifest.json")) else {
            continue;
        };
        if manifest["theme"].is_object() {
            continue;
        }
        let raw = manifest["name"].as_str().unwrap_or(&id);
        let name = if let Some(key) = raw
            .strip_prefix("__MSG_")
            .and_then(|s| s.strip_suffix("__"))
        {
            let locale = manifest["default_locale"].as_str().unwrap_or("en");
            let messages = if locale
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_')
            {
                read_json(&dir.join("_locales").join(locale).join("messages.json"))
                    .unwrap_or(Value::Null)
            } else {
                Value::Null
            };
            messages
                .as_object()
                .and_then(|m| m.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)))
                .and_then(|(_, v)| v["message"].as_str())
                .unwrap_or(raw)
                .to_string()
        } else {
            raw.to_string()
        };
        out.push(json!({ "id": id, "name": name, "version": manifest["version"],
            "status": "requires_chrome", "url": format!("https://chromewebstore.google.com/detail/{id}") }));
    }
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(out)
}

async fn history(path: &Path) -> Result<Vec<Value>> {
    // SQLite takes a consistent read-only snapshot, including Chrome's WAL.
    // Never copy or open Chrome's Login Data / Cookies databases.
    let output = tokio::time::timeout(Duration::from_secs(6), tokio::process::Command::new("/usr/bin/sqlite3")
        .args(["-readonly", "-json", "-cmd", ".timeout 1500"])
        .arg(path.join("History"))
        .arg("SELECT url, title, CAST(last_visit_time / 1000 - 11644473600000 AS INTEGER) AS t FROM urls WHERE url LIKE 'https://%' OR url LIKE 'http://%' ORDER BY last_visit_time DESC LIMIT 400")
        .kill_on_drop(true).output()).await.context("Chrome history is busy. Close Chrome and try again.")??;
    if !output.status.success() {
        bail!("Chrome history could not be read. Close Chrome, check file access, and try again.");
    }
    let rows: Vec<Value> = if output.stdout.is_empty() {
        Vec::new()
    } else {
        serde_json::from_slice(&output.stdout)?
    };
    Ok(rows
        .into_iter()
        .filter(|h| h["url"].as_str().is_some_and(web_url))
        .collect())
}

pub async fn import(req: ImportRequest) -> Result<Value> {
    let path = profile_at(&chrome_root()?, &req.profile)?;
    let mut result =
        json!({ "bookmarks": [], "history": [], "extensions": [], "warnings": [], "imported": [] });
    if req.bookmarks {
        match bookmarks(&path) {
            Ok(v) => {
                result["bookmarks"] = json!(v);
                result["imported"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!("bookmarks"));
            }
            Err(_) => result["warnings"].as_array_mut().unwrap().push(json!(
                "Bookmarks could not be read. Check Chrome profile access and try again."
            )),
        }
    }
    if req.history {
        match history(&path).await {
            Ok(v) => {
                result["history"] = json!(v);
                result["imported"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!("history"));
            }
            Err(e) => result["warnings"]
                .as_array_mut()
                .unwrap()
                .push(json!(e.to_string())),
        }
    }
    if req.extensions {
        match extensions(&path) {
            Ok(v) => {
                result["extensions"] = json!(v);
                result["imported"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!("extensions"));
            }
            Err(_) => result["warnings"].as_array_mut().unwrap().push(json!(
                "The extension list could not be read. Check file access and try again."
            )),
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bookmark_import_preserves_folders_and_rejects_executable_urls() {
        let node = json!({ "name": "Work", "children": [
            { "type": "url", "name": "Docs", "url": "https://example.com/docs" },
            { "type": "url", "url": "javascript:alert(1)" },
            { "type": "url", "url": "file:///etc/passwd" },
            { "name": "Team", "children": [{ "type": "url", "url": "https://example.com/team" }] },
            { "type": "url", "url": "https://example.com/docs" }
        ] });
        let mut out = Vec::new();
        walk_bookmarks(&node, "", &mut out, &mut HashSet::new());
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].folder, "Work / Team");
    }

    #[test]
    fn profile_selection_cannot_escape_chrome_directory() {
        for id in [
            "../Default",
            "Profile ../1",
            "/tmp",
            "Profile ",
            "Profile 1/../../",
        ] {
            assert!(!valid_profile(id));
        }
        assert!(valid_profile("Default"));
        assert!(valid_profile("Profile 12"));
        let root = std::env::temp_dir().join(format!("atrium-migration-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(root.parent().unwrap(), root.join("Default")).unwrap();
        assert!(profile_at(&root, "Default").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn extension_versions_are_sorted_numerically() {
        assert!(version_key(Path::new("10.0_0")) > version_key(Path::new("9.9_0")));
    }

    #[test]
    fn extension_inventory_uses_latest_version_and_resolves_localized_names() {
        let root = std::env::temp_dir().join(format!("atrium-extensions-{}", uuid::Uuid::new_v4()));
        let id = "a".repeat(32);
        for version in ["9.0_0", "10.0_0"] {
            let dir = root.join("Extensions").join(&id).join(version);
            std::fs::create_dir_all(dir.join("_locales/en")).unwrap();
            std::fs::write(dir.join("manifest.json"), json!({ "name": "__MSG_extensionName__", "version": version, "default_locale": "en" }).to_string()).unwrap();
            std::fs::write(
                dir.join("_locales/en/messages.json"),
                json!({ "extensionname": { "message": "Example extension" } }).to_string(),
            )
            .unwrap();
        }
        let rows = extensions(&root).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["name"], "Example extension");
        assert_eq!(rows[0]["version"], "10.0_0");
        assert_eq!(rows[0]["status"], "requires_chrome");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn history_import_converts_chrome_time_without_changing_the_database() {
        let root = std::env::temp_dir().join(format!("atrium-history-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let db = root.join("History");
        let result = std::process::Command::new("/usr/bin/sqlite3").arg(&db).arg(
            "CREATE TABLE urls(url TEXT,title TEXT,last_visit_time INTEGER); INSERT INTO urls VALUES ('https://example.com/','Example',11644473601000000),('javascript:alert(1)','Excluded',11644473602000000);"
        ).output().unwrap();
        assert!(result.status.success());
        let before = std::fs::read(&db).unwrap();
        let rows = history(&root).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["t"], 1000);
        assert_eq!(std::fs::read(db).unwrap(), before);
        std::fs::remove_dir_all(root).unwrap();
    }
}
