//! Local Crashpad reports. `crash_reporter.cfg` in the bundle enables Crashpad without an upload
//! URL; CEF uses the root cache path, `<data dir>/chromium`, as the Crashpad database, so
//! reports are in its `new`, `pending`, and `completed` folders.
use std::{path::Path, sync::Arc, time::UNIX_EPOCH};

use axum::{routing::get, Json, Router};
use serde_json::{json, Value};

use crate::server::App;

fn database(dir: &Path) -> std::path::PathBuf {
    dir.join("chromium")
}

pub fn list(dir: &Path) -> Value {
    let root = database(dir);
    let mut reports = Vec::new();
    for state in ["new", "pending", "completed"] {
        let Ok(entries) = std::fs::read_dir(root.join(state)) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "dmp") {
                continue;
            }
            let meta = entry.metadata().ok();
            reports.push(json!({
                "id": path.file_stem().map(|s| s.to_string_lossy().into_owned()),
                "state": state,
                "path": path,
                "size": meta.as_ref().map(|m| m.len()),
                "modified": meta.and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()),
            }));
        }
    }
    reports.sort_by_key(|r| std::cmp::Reverse(r["modified"].as_u64()));
    let configured = std::env::current_exe().ok()
        .and_then(|exe| Some(exe.parent()?.parent()?.join("Resources/crash_reporter.cfg")))
        .is_some_and(|cfg| cfg.exists());
    json!({ "enabled": configured, "uploads": false, "directory": root, "reports": reports })
}

pub fn routes() -> Router<Arc<App>> {
    Router::new().route("/api/crash-reports", get(|| async {
        Json(match crate::data_dir() {
            Ok(dir) => list(&dir),
            Err(error) => json!({ "error": error.to_string() }),
        })
    }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn lists_dumps_newest_first() {
        let dir = std::env::temp_dir().join(format!("atrium-crash-test-{}", uuid::Uuid::new_v4()));
        let completed = super::database(&dir).join("completed");
        std::fs::create_dir_all(&completed).unwrap();
        std::fs::write(completed.join("a.dmp"), b"MDMP").unwrap();
        std::fs::write(completed.join("a.meta"), b"").unwrap();
        let listed = super::list(&dir);
        let reports = listed["reports"].as_array().unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0]["id"], "a");
        assert_eq!(reports[0]["state"], "completed");
        assert_eq!(listed["uploads"], false);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
