//! Site permission decisions per origin, persisted in the data directory.
//! A request is granted only if the user allowed that origin for every kind it asks for.
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{LazyLock, Mutex, OnceLock};

const FILE: &str = "site-permissions.json";
const RECENT: usize = 50;

/// `cef_permission_request_types_t` bits.
pub const PROMPT: &[(u32, &str)] = &[
    (1 << 0, "ar"),
    (1 << 1, "camera-pan-tilt-zoom"),
    (1 << 2, "camera"),
    (1 << 3, "captured-surface-control"),
    (1 << 4, "clipboard"),
    (1 << 5, "storage-access"),
    (1 << 6, "disk-quota"),
    (1 << 7, "local-fonts"),
    (1 << 8, "geolocation"),
    (1 << 9, "hand-tracking"),
    (1 << 10, "identity-provider"),
    (1 << 11, "idle-detection"),
    (1 << 12, "microphone"),
    (1 << 13, "midi"),
    (1 << 14, "multiple-downloads"),
    (1 << 15, "notifications"),
    (1 << 16, "keyboard-lock"),
    (1 << 17, "pointer-lock"),
    (1 << 18, "protected-media"),
    (1 << 19, "protocol-handler"),
    (1 << 20, "storage-access"),
    (1 << 21, "vr"),
    (1 << 22, "web-app-installation"),
    (1 << 23, "window-management"),
    (1 << 24, "file-system"),
    (1 << 25, "local-network"),
    (1 << 26, "local-network"),
    (1 << 27, "loopback-network"),
    (1 << 28, "sensors"),
    (1 << 29, "ambient-login"),
];
/// `cef_media_access_permission_types_t` bits.
pub const MEDIA: &[(u32, &str)] = &[(1, "microphone"), (2, "camera"), (4, "screen"), (8, "screen")];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Allow,
    Deny,
}

type Decisions = BTreeMap<String, BTreeMap<String, Decision>>;

#[derive(Default)]
struct Store {
    loaded: bool,
    decisions: Decisions,
    recent: VecDeque<Value>,
}

static STORE: LazyLock<Mutex<Store>> = LazyLock::new(Default::default);

type Hook = Box<dyn Fn(&str, &str, Option<Decision>) + Send + Sync>;
static CHANGED: OnceLock<Hook> = OnceLock::new();

/// Calls `hook` for every stored decision now and for every later change. The engine
/// remembers its own answers per origin and stops asking, so it must be told.
pub fn sync(hook: impl Fn(&str, &str, Option<Decision>) + Send + Sync + 'static) {
    for (origin, site) in &store().decisions {
        for (permission, decision) in site {
            hook(origin, permission, Some(*decision));
        }
    }
    let _ = CHANGED.set(Box::new(hook));
}

fn store() -> std::sync::MutexGuard<'static, Store> {
    let mut store = STORE.lock().unwrap();
    if !store.loaded {
        store.loaded = true;
        store.decisions = crate::data_dir()
            .ok()
            .and_then(|dir| std::fs::read(dir.join(FILE)).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
    }
    store
}

/// The serialized origin of an HTTP(S) URL; other schemes have no grantable origin.
pub fn origin(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.origin().ascii_serialization())
}

fn names(mask: u32, table: &[(u32, &'static str)]) -> Vec<&'static str> {
    let mut names = table.iter().filter(|(bit, _)| mask & bit != 0).map(|(_, name)| *name).collect::<Vec<_>>();
    names.sort_unstable();
    names.dedup();
    if table.iter().fold(mask, |rest, (bit, _)| rest & !bit) != 0 {
        names.push("unknown");
    }
    names
}

/// The subset of `mask` that `origin` may use. Unknown bits are never granted.
fn allowed(decisions: &Decisions, origin: Option<&str>, mask: u32, table: &[(u32, &str)]) -> u32 {
    let Some(site) = origin.and_then(|o| decisions.get(o)) else { return 0 };
    table
        .iter()
        .filter(|(bit, name)| mask & bit != 0 && site.get(*name) == Some(&Decision::Allow))
        .fold(0, |granted, (bit, _)| granted | bit)
}

/// Decides a request and records it for `GET /api/browser/permissions`.
pub fn decide(requesting: &str, mask: u32, table: &[(u32, &'static str)]) -> u32 {
    let origin = origin(requesting);
    let mut store = store();
    let granted = allowed(&store.decisions, origin.as_deref(), mask, table);
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    store.recent.push_front(json!({
        "origin": origin.as_deref().unwrap_or(requesting),
        "permissions": names(mask, table),
        "granted": names(granted, table),
        "at": at,
    }));
    store.recent.truncate(RECENT);
    granted
}

pub fn list() -> Value {
    let store = store();
    let mut kinds = PROMPT.iter().chain(MEDIA).map(|(_, name)| *name).collect::<Vec<_>>();
    kinds.sort_unstable();
    kinds.dedup();
    json!({ "decisions": store.decisions, "recent": store.recent, "permissions": kinds })
}

/// Records `decision` for `origin`, or forgets it (the default is to deny) when `None`.
pub fn set(raw_origin: &str, permission: &str, decision: Option<Decision>) -> Result<()> {
    let origin = origin(raw_origin).filter(|o| o == raw_origin.trim_end_matches('/'));
    let Some(origin) = origin else {
        anyhow::bail!("Use an origin such as https://example.com");
    };
    ensure!(PROMPT.iter().chain(MEDIA).any(|(_, name)| *name == permission), "Unknown permission `{permission}`");
    let mut store = store();
    let site = store.decisions.entry(origin.clone()).or_default();
    match decision {
        Some(decision) => {
            site.insert(permission.into(), decision);
        }
        None => {
            site.remove(permission);
        }
    }
    if site.is_empty() {
        store.decisions.remove(&origin);
    }
    let dir = crate::data_dir()?;
    let temp = dir.join(format!("{FILE}.tmp"));
    std::fs::write(&temp, serde_json::to_vec_pretty(&store.decisions)?)?;
    std::fs::rename(&temp, dir.join(FILE))?;
    drop(store);
    if let Some(hook) = CHANGED.get() {
        hook(&origin, permission, decision);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requests_are_denied_unless_every_kind_was_allowed() {
        let mut decisions = Decisions::new();
        decisions.entry("https://meet.example".into()).or_default().insert("camera".into(), Decision::Allow);
        decisions.entry("https://meet.example".into()).or_default().insert("microphone".into(), Decision::Deny);
        let meet = Some("https://meet.example");
        assert_eq!(allowed(&decisions, meet, 1 | 2, MEDIA), 2);
        assert_eq!(allowed(&decisions, Some("https://other.example"), 2, MEDIA), 0);
        assert_eq!(allowed(&decisions, None, 2, MEDIA), 0);
        assert_eq!(allowed(&decisions, meet, 1 << 8, PROMPT), 0);
        assert_eq!(allowed(&decisions, meet, 1 << 2, PROMPT), 1 << 2);
        assert_eq!(allowed(&decisions, meet, 1 << 31, PROMPT), 0);
    }
    #[test]
    fn only_web_origins_are_grantable() {
        assert_eq!(origin("https://Example.com:443/path?q").as_deref(), Some("https://example.com"));
        assert_eq!(origin("http://127.0.0.1:7799/").as_deref(), Some("http://127.0.0.1:7799"));
        for url in ["file:///tmp/x", "chrome://settings", "data:text/html,x", "about:blank"] {
            assert_eq!(origin(url), None, "{url}");
        }
        assert_eq!(names((1 << 8) | (1 << 31), PROMPT), ["geolocation", "unknown"]);
    }
}
