//! Read-only review of unpacked extension manifests. Chromium grants what the manifest
//! declares; this records it so changes between launches are visible.
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const RECORD_FILE: &str = "extension-permissions.json";
static REVIEW: std::sync::OnceLock<Value> = std::sync::OnceLock::new();

const HIGH_RISK: &[&str] = &[
    "<all_urls>", "debugger", "nativeMessaging", "proxy", "webRequest", "webRequestBlocking",
    "declarativeNetRequestWithHostAccess", "cookies", "history", "management", "privacy",
    "clipboardRead", "desktopCapture", "tabCapture", "downloads", "scripting", "tabs",
    "webNavigation", "contentSettings", "browsingData", "pageCapture", "userScripts",
];

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

fn is_host_pattern(p: &str) -> bool {
    p == "<all_urls>" || p.contains("://")
}

fn broad_host(p: &str) -> bool {
    p == "<all_urls>" || p.starts_with("*://*/") || p.ends_with("://*/*") || p == "http://*/*" || p == "https://*/*"
}

/// Resolves a `__MSG_name__` manifest string from the default locale.
fn localized(path: &Path, manifest: &Value, value: &str) -> String {
    let Some(key) = value.strip_prefix("__MSG_").and_then(|v| v.strip_suffix("__")) else { return value.to_string() };
    let locale = manifest["default_locale"].as_str().unwrap_or("en");
    std::fs::read(path.join("_locales").join(locale).join("messages.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|messages| {
            let messages = messages.as_object()?;
            let (_, entry) = messages.iter().find(|(k, _)| k.eq_ignore_ascii_case(key))?;
            entry["message"].as_str().map(str::to_string)
        })
        .unwrap_or_else(|| value.to_string())
}

pub fn summarize(path: &Path, manifest: &Value) -> Value {
    let declared = strings(&manifest["permissions"]);
    let (mut hosts, permissions): (Vec<_>, Vec<_>) = declared.into_iter().partition(|p| is_host_pattern(p));
    hosts.extend(strings(&manifest["host_permissions"]));
    let optional = strings(&manifest["optional_permissions"]);
    let mut optional_hosts = strings(&manifest["optional_host_permissions"]);
    optional_hosts.extend(optional.iter().filter(|p| is_host_pattern(p)).cloned());
    let optional: Vec<_> = optional.into_iter().filter(|p| !is_host_pattern(p)).collect();
    let mut content_matches: Vec<String> = manifest["content_scripts"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|script| strings(&script["matches"]))
        .collect();
    content_matches.sort();
    content_matches.dedup();
    let mut high_risk: Vec<String> = permissions
        .iter()
        .filter(|p| HIGH_RISK.contains(&p.as_str()))
        .cloned()
        .chain(hosts.iter().chain(&content_matches).filter(|p| broad_host(p)).cloned())
        .collect();
    high_risk.sort();
    high_risk.dedup();
    json!({
        "path": path.to_string_lossy(),
        "name": localized(path, manifest, manifest["name"].as_str().unwrap_or("")),
        "version": manifest["version"].as_str().unwrap_or(""),
        "manifest_version": manifest["manifest_version"],
        "permissions": permissions,
        "host_permissions": hosts,
        "optional_permissions": optional,
        "optional_host_permissions": optional_hosts,
        "content_script_matches": content_matches,
        "background": !manifest["background"].is_null(),
        "externally_connectable": manifest["externally_connectable"],
        "high_risk": high_risk,
    })
}

pub fn fingerprint(summary: &Value) -> Value {
    json!({
        "permissions": summary["permissions"],
        "host_permissions": summary["host_permissions"],
        "optional_permissions": summary["optional_permissions"],
        "optional_host_permissions": summary["optional_host_permissions"],
        "content_script_matches": summary["content_script_matches"],
    })
}

fn load(paths: &[PathBuf]) -> Vec<Value> {
    paths
        .iter()
        .map(|path| match std::fs::read(path.join("manifest.json")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
            Some(manifest) => summarize(path, &manifest),
            None => json!({ "path": path.to_string_lossy(), "error": "manifest.json could not be read" }),
        })
        .collect()
}

/// Compares against the previous launch, logs new or changed grants, and saves the current record.
pub fn record_review(dir: &Path, paths: &[PathBuf]) {
    let record_path = dir.join(RECORD_FILE);
    let previous: Value = std::fs::read(&record_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| json!({}));
    let mut record = serde_json::Map::new();
    let mut extensions = load(paths);
    for summary in &mut extensions {
        let key = summary["path"].as_str().unwrap_or("").to_string();
        if summary.get("error").is_some() {
            continue;
        }
        let current = fingerprint(summary);
        let before = &previous[&key];
        let status = if before.is_null() {
            "new"
        } else if before["grants"] != current {
            "changed"
        } else {
            "unchanged"
        };
        summary["review_status"] = json!(status);
        if status != "unchanged" && summary["high_risk"].as_array().is_some_and(|r| !r.is_empty()) {
            eprintln!("extension {} ({status}) requests broad access: {}", summary["name"], summary["high_risk"]);
        }
        record.insert(key, json!({ "name": summary["name"], "version": summary["version"], "grants": current }));
    }
    let _ = std::fs::write(&record_path, serde_json::to_vec_pretty(&Value::Object(record)).unwrap_or_default());
    let _ = REVIEW.set(json!({
        "extensions": extensions,
        "enforced": false,
        "note": format!("Directories passed with `run --extension` are developer loads and receive every permission their manifest declares; {} records them for review. Installed extensions load only after their permissions are approved.", crate::brand::PRODUCT_NAME),
    }));
}

pub fn review(paths: &[PathBuf]) -> Value {
    REVIEW.get().cloned().unwrap_or_else(|| json!({ "extensions": load(paths), "enforced": false }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manifest_permissions_are_split_and_flagged() {
        let manifest = json!({
            "name": "Fixture", "version": "1.0", "manifest_version": 3,
            "permissions": ["storage", "tabs", "https://example.com/*"],
            "host_permissions": ["<all_urls>"],
            "optional_permissions": ["downloads", "*://*/*"],
            "content_scripts": [{ "matches": ["https://*/*", "https://a.test/*"] }, { "matches": ["https://a.test/*"] }],
            "background": { "service_worker": "sw.js" }
        });
        let s = summarize(Path::new("/tmp/ext"), &manifest);
        assert_eq!(s["permissions"], json!(["storage", "tabs"]));
        assert_eq!(s["host_permissions"], json!(["https://example.com/*", "<all_urls>"]));
        assert_eq!(s["optional_permissions"], json!(["downloads"]));
        assert_eq!(s["optional_host_permissions"], json!(["*://*/*"]));
        assert_eq!(s["content_script_matches"], json!(["https://*/*", "https://a.test/*"]));
        assert_eq!(s["high_risk"], json!(["<all_urls>", "https://*/*", "tabs"]));
        assert_eq!(s["background"], true);
    }
    #[test]
    fn review_record_detects_changed_grants() {
        let dir = std::env::temp_dir().join(format!("aside-ext-test-{}", uuid::Uuid::new_v4()));
        let ext = dir.join("ext");
        std::fs::create_dir_all(&ext).unwrap();
        std::fs::write(ext.join("manifest.json"), r#"{"name":"A","version":"1","manifest_version":3,"permissions":["storage"]}"#).unwrap();
        let summary = |dir: &Path| {
            let record: Value = serde_json::from_slice(&std::fs::read(dir.join(RECORD_FILE)).unwrap()).unwrap();
            record[ext.to_string_lossy().as_ref()]["grants"]["permissions"].clone()
        };
        record_review(&dir, &[ext.clone()]);
        assert_eq!(summary(&dir), json!(["storage"]));
        std::fs::write(ext.join("manifest.json"), r#"{"name":"A","version":"2","manifest_version":3,"permissions":["storage","cookies"]}"#).unwrap();
        let previous: Value = serde_json::from_slice(&std::fs::read(dir.join(RECORD_FILE)).unwrap()).unwrap();
        let current = fingerprint(&summarize(&ext, &serde_json::from_slice(&std::fs::read(ext.join("manifest.json")).unwrap()).unwrap()));
        assert_ne!(previous[ext.to_string_lossy().as_ref()]["grants"], current);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
