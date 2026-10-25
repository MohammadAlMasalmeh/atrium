//! Opt-in Chrome migration. Chrome's files are only read, never changed. Cookies and
//! Chrome's cookie key are read only by the separate, consented cookie import.
pub mod cookies;
pub mod search;
pub mod session;

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::app::{Handle, Op};

/// Microseconds from Chrome's 1601 epoch to Unix milliseconds.
const CHROME_EPOCH_MS: i64 = 11_644_473_600_000;
const MAX_BOOKMARKS: usize = 10_000;
const MAX_HISTORY_URLS: usize = 20_000;
const MAX_VISITS_PER_URL: usize = 200;
const MAX_RUNS: usize = 50;
static IMPORT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Deserialize)]
pub struct ImportRequest {
    pub profile: String,
    #[serde(default)]
    pub bookmarks: bool,
    #[serde(default)]
    pub history: bool,
    #[serde(default)]
    pub extensions: bool,
    /// Open tabs from Chrome's last session.
    #[serde(default)]
    pub tabs: bool,
    #[serde(default)]
    pub search_engine: bool,
    /// Cookies need the separate `/api/setup/import/cookies` request with consent.
    #[serde(default)]
    pub cookies: bool,
}

#[derive(Deserialize)]
pub struct CookieRequest {
    pub profile: String,
    /// Must be true: the caller has shown `cookies::SECURITY_NOTICE` and the user agreed.
    #[serde(default)]
    pub consent: bool,
    /// Only import cookies for these sites (and their subdomains). Empty imports all.
    #[serde(default)]
    pub domains: Vec<String>,
}

#[derive(Clone, Serialize)]
pub struct Bookmark {
    url: String,
    title: String,
    folder: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Node {
    /// Chrome's bookmark GUID, so re-imports update instead of duplicating.
    id: String,
    title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    /// Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    added: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    children: Option<Vec<Node>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct HistoryEntry {
    url: String,
    title: String,
    /// Unix milliseconds, newest first.
    visits: Vec<i64>,
    #[serde(default)]
    typed: i64,
}

pub(crate) fn mock_keychain() -> bool {
    std::env::var_os("ASIDE_MOCK_KEYCHAIN").is_some()
}

/// Test instances never read the user's Chrome data; they may point at a synthetic profile.
fn chrome_root() -> Result<PathBuf> {
    if mock_keychain() {
        return Ok(std::env::var_os("ASIDE_TEST_CHROME_ROOT").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/nonexistent/aside-test-chrome")));
    }
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

pub(crate) fn read_json(path: &Path) -> Result<Value> {
    let metadata = path.metadata()?;
    if metadata.len() > 32 * 1024 * 1024 {
        bail!("This Chrome file is too large to import.");
    }
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, bytes)?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

fn load<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// SQLite takes a consistent read-only snapshot, including Chrome's WAL.
pub(crate) async fn sqlite(db: &Path, query: &str) -> Result<Vec<Value>> {
    anyhow::ensure!(db.is_file(), "The Chrome database was not found.");
    let output = tokio::time::timeout(Duration::from_secs(15), tokio::process::Command::new("/usr/bin/sqlite3")
        .args(["-readonly", "-json", "-cmd", ".timeout 1500"])
        .arg(db)
        .arg(query)
        .kill_on_drop(true).output()).await.context("Chrome's database is busy. Close Chrome and try again.")??;
    if !output.status.success() {
        bail!("Chrome's database could not be read. Close Chrome, check file access, and try again.");
    }
    Ok(if output.stdout.is_empty() { Vec::new() } else { serde_json::from_slice(&output.stdout)? })
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

fn chrome_ms(us: i64) -> Option<i64> {
    (us > 0).then(|| us / 1000 - CHROME_EPOCH_MS)
}

#[derive(Default)]
struct Walk {
    urls: usize,
    folders: usize,
    skipped: usize,
    seen: HashSet<String>,
}

fn convert(node: &Value, path: &str, walk: &mut Walk) -> Option<Node> {
    let title = node["name"].as_str().unwrap_or_default();
    let added = node["date_added"].as_str().and_then(|s| s.parse().ok()).and_then(chrome_ms);
    if node["type"] == "url" {
        let url = node["url"].as_str().unwrap_or_default();
        let key = format!("{path}\n{url}");
        if !web_url(url) || walk.urls >= MAX_BOOKMARKS || !walk.seen.insert(key.clone()) {
            walk.skipped += 1;
            return None;
        }
        walk.urls += 1;
        let id = node["guid"].as_str().map_or(key, str::to_string);
        return Some(Node { id, title: if title.is_empty() { url } else { title }.into(), url: Some(url.into()), added, children: None });
    }
    let children = node["children"].as_array()?;
    if !path.is_empty() {
        walk.folders += 1;
    }
    let path = if path.is_empty() { title.to_string() } else { format!("{path} / {title}") };
    let children = children.iter().filter_map(|c| convert(c, &path, walk)).collect();
    Some(Node { id: node["guid"].as_str().map_or_else(|| format!("{path}\n"), str::to_string), title: title.into(), url: None, added, children: Some(children) })
}

/// Chrome's roots in display order: bookmarks bar, other, then mobile.
fn bookmark_tree(value: &Value) -> Result<(Vec<Node>, Walk)> {
    let roots = value["roots"].as_object().context("Chrome bookmarks could not be read.")?;
    let mut keys: Vec<&String> = roots.keys().collect();
    let order = |k: &str| ["bookmark_bar", "other", "synced"].iter().position(|o| *o == k).unwrap_or(3);
    keys.sort_by_key(|k| (order(k), (*k).clone()));
    let mut walk = Walk::default();
    let tree = keys.into_iter()
        .filter_map(|k| convert(&roots[k], "", &mut walk))
        .filter(|n| n.children.as_ref().is_some_and(|c| !c.is_empty()))
        .collect();
    Ok((tree, walk))
}

fn flatten(nodes: &[Node], folder: &str, out: &mut Vec<Bookmark>) {
    for n in nodes {
        match (&n.url, &n.children) {
            (Some(url), _) => out.push(Bookmark { url: url.clone(), title: n.title.clone(), folder: folder.into() }),
            (None, Some(children)) => flatten(children, &if folder.is_empty() { n.title.clone() } else { format!("{folder} / {}", n.title) }, out),
            _ => {}
        }
    }
}

fn url_nodes(nodes: &[Node], out: &mut HashMap<String, (String, String)>) {
    for n in nodes {
        if let Some(url) = &n.url {
            out.insert(n.id.clone(), (url.clone(), n.title.clone()));
        }
        url_nodes(n.children.as_deref().unwrap_or_default(), out);
    }
}

#[derive(Default, Serialize, Deserialize)]
struct BookmarkStore {
    /// Chrome profile id -> that profile's imported tree. A re-import replaces it.
    sources: HashMap<String, Value>,
}

fn import_bookmarks(dir: &Path, profile_id: &str, profile: &Path) -> Result<(Value, Vec<Bookmark>)> {
    let (tree, walk) = bookmark_tree(&read_json(&profile.join("Bookmarks")).context("Bookmarks could not be read. Check Chrome profile access and try again.")?)?;
    let path = dir.join("bookmarks.json");
    let mut store: BookmarkStore = load(&path);
    let old: Vec<Node> = store.sources.get(profile_id).and_then(|s| serde_json::from_value(s["roots"].clone()).ok()).unwrap_or_default();
    let (mut before, mut after) = (HashMap::new(), HashMap::new());
    url_nodes(&old, &mut before);
    url_nodes(&tree, &mut after);
    let added = after.keys().filter(|k| !before.contains_key(*k)).count();
    let updated = after.iter().filter(|(k, v)| before.get(*k).is_some_and(|b| b != *v)).count();
    let removed = before.keys().filter(|k| !after.contains_key(*k)).count();
    store.sources.insert(profile_id.into(), json!({ "imported_at": now_ms(), "roots": tree }));
    write_atomic(&path, &serde_json::to_vec(&store)?)?;
    let mut flat = Vec::new();
    flatten(&tree, "", &mut flat);
    Ok((json!({ "status": "imported", "imported": walk.urls, "folders": walk.folders,
        "added": added, "updated": updated, "removed": removed, "unchanged": after.len().saturating_sub(added + updated), "skipped": walk.skipped, "errors": [] }), flat))
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

/// One row per visit (Chrome's `visits` table); older or partial databases fall back to
/// one row per URL. Subframe navigations are not user history.
async fn history_rows(path: &Path) -> Result<Vec<Value>> {
    let db = path.join("History");
    let visits = format!("SELECT u.url AS url, u.title AS title, u.typed_count AS typed, CAST(v.visit_time / 1000 - {CHROME_EPOCH_MS} AS INTEGER) AS t FROM visits v JOIN urls u ON u.id = v.url WHERE (u.url LIKE 'https://%' OR u.url LIKE 'http://%') AND u.hidden = 0 AND (v.transition & 255) NOT IN (3, 4) ORDER BY v.visit_time DESC LIMIT 100000");
    let rows = match sqlite(&db, &visits).await {
        Ok(rows) => rows,
        Err(_) => sqlite(&db, &format!("SELECT url, title, CAST(last_visit_time / 1000 - {CHROME_EPOCH_MS} AS INTEGER) AS t FROM urls WHERE url LIKE 'https://%' OR url LIKE 'http://%' ORDER BY last_visit_time DESC LIMIT 400")).await
            .map_err(|_| anyhow::anyhow!("Chrome history could not be read. Close Chrome, check file access, and try again."))?,
    };
    Ok(rows.into_iter().filter(|h| h["url"].as_str().is_some_and(web_url) && h["t"].as_i64().is_some_and(|t| t > 0)).collect())
}

fn merge_history(store: &mut Vec<HistoryEntry>, rows: &[Value]) -> (usize, usize, usize) {
    let mut index: HashMap<String, usize> = store.iter().enumerate().map(|(i, e)| (e.url.clone(), i)).collect();
    let (mut new_urls, mut new_visits, mut known) = (0, 0, 0);
    for r in rows {
        let (Some(url), Some(t)) = (r["url"].as_str(), r["t"].as_i64()) else { continue };
        let i = match index.get(url) {
            Some(i) => *i,
            None => {
                if store.len() >= MAX_HISTORY_URLS {
                    continue;
                }
                store.push(HistoryEntry { url: url.into(), title: String::new(), visits: Vec::new(), typed: 0 });
                new_urls += 1;
                index.insert(url.into(), store.len() - 1);
                store.len() - 1
            }
        };
        let entry = &mut store[i];
        if entry.visits.contains(&t) {
            known += 1;
            continue;
        }
        if entry.visits.first().is_none_or(|last| t >= *last) {
            if let Some(title) = r["title"].as_str().filter(|s| !s.is_empty()) {
                entry.title = title.into();
            }
        }
        entry.typed = entry.typed.max(r["typed"].as_i64().unwrap_or(0));
        entry.visits.push(t);
        entry.visits.sort_unstable_by(|a, b| b.cmp(a));
        entry.visits.truncate(MAX_VISITS_PER_URL);
        new_visits += 1;
    }
    store.sort_by(|a, b| b.visits.first().cmp(&a.visits.first()));
    (new_urls, new_visits, known)
}

async fn import_history(dir: &Path, profile: &Path) -> Result<(Value, Vec<Value>)> {
    let rows = history_rows(profile).await?;
    let path = dir.join("history.json");
    let mut store: Vec<HistoryEntry> = load(&path);
    let (urls, visits, known) = merge_history(&mut store, &rows);
    write_atomic(&path, &serde_json::to_vec(&store)?)?;
    let recent = store.iter().take(400).map(|e| json!({ "url": e.url, "title": e.title, "t": e.visits.first(), "visits": e.visits.len() })).collect();
    Ok((json!({ "status": "imported", "imported": visits, "urls_added": urls, "already_imported": known, "skipped": 0, "errors": [] }), recent))
}

async fn import_tabs(profile: &Path, browser: &Handle) -> Result<Value> {
    let Some(file) = session::latest(profile) else {
        return Ok(json!({ "status": "nothing_found", "imported": 0, "skipped": 0, "errors": ["No Chrome session was found in this profile."] }));
    };
    anyhow::ensure!(file.metadata()?.len() <= 64 * 1024 * 1024, "This Chrome session is too large to import.");
    let tabs = session::parse(&std::fs::read(&file).context("Chrome's last session could not be read.")?)?;
    let (web, other): (Vec<_>, Vec<_>) = tabs.into_iter().partition(|t| web_url(&t.url));
    let pairs = web.iter().map(|t| (t.url.clone(), t.title.clone())).collect();
    let (opened, already) = restore(browser, pairs).await?;
    Ok(json!({ "status": "imported", "imported": opened, "already_open": already, "skipped": other.len(), "found": web.len() + other.len(), "errors": [] }))
}

#[cfg(feature = "chromium")]
async fn restore(browser: &Handle, tabs: Vec<(String, String)>) -> Result<(usize, usize)> {
    browser.op(|reply| Op::Restore { tabs, reply }).await
}
#[cfg(not(feature = "chromium"))]
async fn restore(_: &Handle, _: Vec<(String, String)>) -> Result<(usize, usize)> {
    bail!("Restoring tabs requires the Chromium build.")
}

async fn import_search(dir: &Path, profile: &Path) -> Result<Value> {
    Ok(match search::read(profile).await? {
        Some(engine) => {
            search::save(dir, &engine)?;
            json!({ "status": "imported", "imported": 1, "skipped": 0, "errors": [], "engine": engine })
        }
        None => json!({ "status": "nothing_found", "imported": 0, "skipped": 0, "engine": Value::Null,
            "errors": ["Chrome uses its built-in default search engine; Aside keeps Google."] }),
    })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as i64
}

fn failed(e: &anyhow::Error) -> Value {
    json!({ "status": "failed", "imported": 0, "skipped": 0, "errors": [e.to_string()] })
}

fn record(dir: &Path, profile: &str, results: &serde_json::Map<String, Value>) -> Result<Value> {
    let path = dir.join("report.json");
    let mut runs: Vec<Value> = load(&path);
    let run = json!({ "id": uuid::Uuid::new_v4().to_string(), "at": now_ms(), "profile": profile, "results": results });
    runs.insert(0, run.clone());
    runs.truncate(MAX_RUNS);
    write_atomic(&path, &serde_json::to_vec_pretty(&runs)?)?;
    Ok(run)
}

pub fn store_dir(data_dir: &Path) -> Result<PathBuf> {
    let dir = data_dir.join("import");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub async fn import(req: ImportRequest, data_dir: &Path, browser: &Handle) -> Result<Value> {
    let _lock = IMPORT_LOCK.lock().await;
    let path = profile_at(&chrome_root()?, &req.profile)?;
    let dir = store_dir(data_dir)?;
    let mut result =
        json!({ "bookmarks": [], "history": [], "extensions": [], "warnings": [], "imported": [] });
    let mut results = serde_json::Map::new();
    let warn = |result: &mut Value, e: &anyhow::Error| result["warnings"].as_array_mut().unwrap().push(json!(e.to_string()));
    let done = |result: &mut Value, kind: &str| result["imported"].as_array_mut().unwrap().push(json!(kind));
    if req.bookmarks {
        match import_bookmarks(&dir, &req.profile, &path) {
            Ok((report, flat)) => { result["bookmarks"] = json!(flat); results.insert("bookmarks".into(), report); done(&mut result, "bookmarks"); }
            Err(e) => { warn(&mut result, &e); results.insert("bookmarks".into(), failed(&e)); }
        }
    }
    if req.history {
        match import_history(&dir, &path).await {
            Ok((report, recent)) => { result["history"] = json!(recent); results.insert("history".into(), report); done(&mut result, "history"); }
            Err(e) => { warn(&mut result, &e); results.insert("history".into(), failed(&e)); }
        }
    }
    if req.tabs {
        match import_tabs(&path, browser).await {
            Ok(report) => { if report["status"] == "imported" { done(&mut result, "tabs"); } results.insert("tabs".into(), report); }
            Err(e) => { warn(&mut result, &e); results.insert("tabs".into(), failed(&e)); }
        }
    }
    if req.search_engine {
        match import_search(&dir, &path).await {
            Ok(report) => { if report["status"] == "imported" { done(&mut result, "search_engine"); } results.insert("search_engine".into(), report); }
            Err(e) => { warn(&mut result, &e); results.insert("search_engine".into(), failed(&e)); }
        }
    }
    if req.extensions {
        match extensions(&path) {
            Ok(v) => {
                results.insert("extensions".into(), json!({ "status": "listed", "imported": 0, "listed": v.len(), "skipped": 0, "errors": [] }));
                result["extensions"] = json!(v);
                done(&mut result, "extensions");
            }
            Err(_) => {
                let e = anyhow::anyhow!("The extension list could not be read. Check file access and try again.");
                warn(&mut result, &e);
                results.insert("extensions".into(), failed(&e));
            }
        }
    }
    if req.cookies {
        warn(&mut result, &anyhow::anyhow!("Cookies are imported separately, only after the user agrees to the security notice."));
    }
    result["report"] = record(&dir, &req.profile, &results)?;
    Ok(result)
}

pub async fn import_cookies(req: CookieRequest, data_dir: &Path, browser: &Handle) -> Result<Value> {
    if !req.consent {
        return Ok(json!({ "error": "Importing cookies needs the user's explicit consent.", "consent_required": true, "security_notice": cookies::SECURITY_NOTICE }));
    }
    let _lock = IMPORT_LOCK.lock().await;
    let path = profile_at(&chrome_root()?, &req.profile)?;
    let dir = store_dir(data_dir)?;
    let outcome = async {
        let db = cookies::database(&path).context("No Chrome cookie database was found in this profile.")?;
        let rows = sqlite(&db, cookies::QUERY).await.context("Chrome cookies could not be read. Close Chrome and try again.")?;
        let key = cookies::derive_key(&cookies::safe_storage_password().await?)?;
        let plan = cookies::plan(&rows, &key, &req.domains);
        let (written, verified, mismatched) = write_cookies(browser, &plan.cookies).await?;
        let mut errors = plan.errors.clone();
        if !mismatched.is_empty() {
            errors.push(format!("{} cookies did not read back as written: {}", mismatched.len(), mismatched.join(", ")));
        }
        anyhow::Ok(json!({
            "status": "imported", "imported": written, "verified": verified, "found": rows.len(),
            "rejected": plan.cookies.len() - written, "skipped": plan.skipped_expired + plan.skipped_partitioned + plan.skipped_filtered,
            "skipped_expired": plan.skipped_expired, "skipped_partitioned": plan.skipped_partitioned, "skipped_filtered": plan.skipped_filtered,
            "domains": plan.cookies.iter().map(|c| c.domain.trim_start_matches('.').to_string()).filter(|d| !d.is_empty())
                .chain(plan.cookies.iter().filter(|c| c.domain.is_empty()).map(|c| c.url.split('/').nth(2).unwrap_or_default().to_string()))
                .collect::<std::collections::BTreeSet<_>>(),
            "errors": errors,
        }))
    }.await;
    let report = outcome.unwrap_or_else(|e| failed(&e));
    let mut results = serde_json::Map::new();
    results.insert("cookies".into(), report.clone());
    let run = record(&dir, &req.profile, &results)?;
    Ok(json!({ "cookies": report, "security_notice": cookies::SECURITY_NOTICE, "report": run }))
}

/// Returns (accepted by CEF, read back identically, names that differ).
#[cfg(feature = "chromium")]
async fn write_cookies(browser: &Handle, list: &[cookies::Write]) -> Result<(usize, usize, Vec<String>)> {
    let results = browser.op(|reply| Op::SetCookies { cookies: list.to_vec(), reply }).await?;
    let accepted: Vec<cookies::Write> = list.iter().zip(&results).filter(|(_, ok)| **ok).map(|(c, _)| c.clone()).collect();
    let stored = browser.op(Op::Cookies).await?;
    let (verified, mismatched) = cookies::verify(&accepted, &stored);
    let mut rejected: Vec<String> = list.iter().zip(&results).filter(|(_, ok)| !**ok).map(|(c, _)| format!("{} {} (rejected)", c.url, c.name)).collect();
    rejected.extend(mismatched);
    Ok((accepted.len(), verified, rejected))
}
#[cfg(not(feature = "chromium"))]
async fn write_cookies(_: &Handle, _: &[cookies::Write]) -> Result<(usize, usize, Vec<String>)> {
    bail!("Cookie import requires the Chromium build.")
}

/// Every import run (newest first) and the latest result for each kind.
pub fn report(data_dir: &Path) -> Value {
    let runs: Vec<Value> = load(&data_dir.join("import/report.json"));
    let mut latest = serde_json::Map::new();
    for run in runs.iter().rev() {
        for (kind, result) in run["results"].as_object().into_iter().flatten() {
            let mut r = result.clone();
            r["at"] = run["at"].clone();
            r["profile"] = run["profile"].clone();
            latest.insert(kind.clone(), r);
        }
    }
    json!({ "runs": runs, "latest": latest, "search_engine": search::current() })
}

/// Imported bookmarks (tree per Chrome profile) and history, for the shell and API clients.
pub fn library(data_dir: &Path, history_limit: usize) -> Value {
    let bookmarks: BookmarkStore = load(&data_dir.join("import/bookmarks.json"));
    let history: Vec<HistoryEntry> = load(&data_dir.join("import/history.json"));
    let mut flat = Vec::new();
    let mut sources: Vec<(&String, &Value)> = bookmarks.sources.iter().collect();
    sources.sort_by_key(|(k, _)| (*k).clone());
    for (_, source) in &sources {
        let tree: Vec<Node> = serde_json::from_value(source["roots"].clone()).unwrap_or_default();
        flatten(&tree, "", &mut flat);
    }
    json!({
        "bookmarks": flat,
        "bookmark_tree": bookmarks.sources,
        "history": history.iter().take(history_limit).collect::<Vec<_>>(),
        "history_total": history.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bookmark_import_preserves_folders_and_rejects_executable_urls() {
        let node = json!({ "roots": { "other": { "name": "Other", "children": [], "type": "folder" }, "bookmark_bar": { "name": "Bar", "type": "folder", "children": [
            { "name": "Work", "type": "folder", "guid": "f1", "children": [
                { "type": "url", "name": "Docs", "url": "https://example.com/docs", "guid": "b1", "date_added": "13300000000000000" },
                { "type": "url", "url": "javascript:alert(1)" },
                { "type": "url", "url": "file:///etc/passwd" },
                { "name": "Team", "type": "folder", "children": [{ "type": "url", "url": "https://example.com/team" }] },
                { "type": "url", "url": "https://example.com/docs" }
            ] },
            { "name": "Empty", "type": "folder", "children": [] }
        ] } } });
        let (tree, walk) = bookmark_tree(&node).unwrap();
        assert_eq!(tree.len(), 1, "empty roots are omitted");
        assert_eq!((walk.urls, walk.skipped), (2, 3));
        let mut out = Vec::new();
        flatten(&tree, "", &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].folder, "Bar / Work");
        assert_eq!(out[1].folder, "Bar / Work / Team");
        let work = &tree[0].children.as_ref().unwrap()[0];
        assert_eq!(work.id, "f1");
        let docs = &work.children.as_ref().unwrap()[0];
        assert_eq!((docs.id.as_str(), docs.added), ("b1", Some(13300000000000000 / 1000 - CHROME_EPOCH_MS)));
        assert_eq!(tree[0].children.as_ref().unwrap()[1].children, Some(vec![]), "empty folders are kept");
    }

    #[test]
    fn bookmark_reimport_replaces_instead_of_duplicating() {
        let profile = std::env::temp_dir().join(format!("aside-bookmarks-{}", uuid::Uuid::new_v4()));
        let dir = profile.join("store");
        std::fs::create_dir_all(&dir).unwrap();
        let write = |title: &str| std::fs::write(profile.join("Bookmarks"), json!({ "roots": { "bookmark_bar": { "name": "Bar", "children": [
            { "type": "url", "name": title, "url": "https://a.test/", "guid": "a" }, { "type": "url", "name": "B", "url": "https://b.test/", "guid": "b" }] } } }).to_string()).unwrap();
        write("A");
        let (first, _) = import_bookmarks(&dir, "Default", &profile).unwrap();
        assert_eq!((first["added"].as_u64(), first["unchanged"].as_u64()), (Some(2), Some(0)));
        let (again, flat) = import_bookmarks(&dir, "Default", &profile).unwrap();
        assert_eq!((again["added"].as_u64(), again["unchanged"].as_u64(), flat.len()), (Some(0), Some(2), 2));
        write("Renamed");
        let (renamed, _) = import_bookmarks(&dir, "Default", &profile).unwrap();
        assert_eq!((renamed["updated"].as_u64(), renamed["added"].as_u64()), (Some(1), Some(0)));
        std::fs::remove_dir_all(profile).unwrap();
    }

    #[test]
    fn history_merge_keeps_each_visit_once() {
        let mut store = Vec::new();
        let rows = vec![json!({ "url": "https://a.test/", "title": "A new", "t": 3000, "typed": 2 }), json!({ "url": "https://a.test/", "title": "A old", "t": 1000 }), json!({ "url": "https://b.test/", "title": "B", "t": 2000 })];
        assert_eq!(merge_history(&mut store, &rows), (2, 3, 0));
        assert_eq!(merge_history(&mut store, &rows), (0, 0, 3));
        assert_eq!(store[0].visits, [3000, 1000]);
        assert_eq!((store[0].title.as_str(), store[0].typed), ("A new", 2));
        assert_eq!(merge_history(&mut store, &[json!({ "url": "https://b.test/", "title": "B2", "t": 4000 })]), (0, 1, 0));
        assert_eq!(store[0].url, "https://b.test/");
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
        let root = std::env::temp_dir().join(format!("aside-migration-{}", uuid::Uuid::new_v4()));
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
        let root = std::env::temp_dir().join(format!("aside-extensions-{}", uuid::Uuid::new_v4()));
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
    async fn history_import_reads_visits_without_changing_the_database() {
        let root = std::env::temp_dir().join(format!("aside-history-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let db = root.join("History");
        let result = std::process::Command::new("/usr/bin/sqlite3").arg(&db).arg(
            "CREATE TABLE urls(id INTEGER PRIMARY KEY,url TEXT,title TEXT,visit_count INTEGER,typed_count INTEGER,last_visit_time INTEGER,hidden INTEGER);
             CREATE TABLE visits(id INTEGER PRIMARY KEY,url INTEGER,visit_time INTEGER,transition INTEGER);
             INSERT INTO urls VALUES (1,'https://example.com/','Example',2,1,11644473602000000,0),(2,'javascript:alert(1)','Excluded',1,0,11644473602000000,0),(3,'https://frame.test/','Frame',1,0,11644473602000000,0);
             INSERT INTO visits VALUES (1,1,11644473601000000,1),(2,1,11644473602000000,805306368),(3,2,11644473602000000,0),(4,3,11644473602000000,3);"
        ).output().unwrap();
        assert!(result.status.success());
        let before = std::fs::read(&db).unwrap();
        let rows = history_rows(&root).await.unwrap();
        assert_eq!(rows.iter().map(|r| r["t"].as_i64().unwrap()).collect::<Vec<_>>(), [2000, 1000]);
        assert_eq!(std::fs::read(&db).unwrap(), before);
        // Databases without a visits table still import one row per URL.
        let legacy = root.join("legacy");
        std::fs::create_dir_all(&legacy).unwrap();
        std::process::Command::new("/usr/bin/sqlite3").arg(legacy.join("History")).arg(
            "CREATE TABLE urls(url TEXT,title TEXT,last_visit_time INTEGER); INSERT INTO urls VALUES ('https://example.com/','Example',11644473601000000);"
        ).output().unwrap();
        assert_eq!(history_rows(&legacy).await.unwrap()[0]["t"], 1000);
        std::fs::remove_dir_all(root).unwrap();
    }
}
