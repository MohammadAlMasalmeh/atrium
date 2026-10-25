//! Installed extensions: Chrome Web Store and self-hosted CRX installs, permission approval,
//! enable/disable/remove, and update checks. Chromium only loads extensions at process start
//! (`--load-extension`), so every change applies on the next launch.
use crate::{crx, extensions};
use anyhow::{anyhow, bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
};

pub const WEB_STORE_UPDATE_URL: &str = "https://clients2.google.com/service/update2/crx";
const REGISTRY: &str = "registry.json";
const TRASH: &str = ".trash";
const MAX_CRX: usize = 256 << 20;

#[derive(Serialize, Deserialize, Clone)]
struct Pending {
    version: String,
    dir: PathBuf,
}

#[derive(Serialize, Deserialize, Clone)]
struct Installed {
    id: String,
    version: String,
    dir: PathBuf,
    enabled: bool,
    /// The grants fingerprint the user approved; the extension never loads without it.
    approved: Option<Value>,
    update_url: Option<String>,
    web_store: bool,
    installed_at: u64,
    /// A downloaded update that requests more access and waits for approval.
    pending: Option<Pending>,
}

#[derive(Serialize, Deserialize, Default)]
struct Registry {
    extensions: BTreeMap<String, Installed>,
}

struct Store {
    root: PathBuf,
    registry: Registry,
    /// What this process passed to Chromium: id -> directory.
    loaded: BTreeMap<String, PathBuf>,
}

static STORE: Mutex<Option<Store>> = Mutex::new(None);

fn manifest(dir: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).context("manifest.json is missing")?)?)
}

fn grants(dir: &Path) -> Result<Value> {
    Ok(extensions::fingerprint(&extensions::summarize(dir, &manifest(dir)?)))
}

/// Access in `current` that `approved` did not include. Removing access needs no approval.
pub fn added_grants(approved: &Value, current: &Value) -> Vec<String> {
    let Some(current) = current.as_object() else { return vec![] };
    let mut added = Vec::new();
    for (kind, items) in current {
        let before: Vec<&Value> = approved[kind].as_array().map(|a| a.iter().collect()).unwrap_or_default();
        for item in items.as_array().into_iter().flatten() {
            if !before.contains(&item) {
                added.push(item.as_str().unwrap_or_default().to_string());
            }
        }
    }
    added.sort();
    added.dedup();
    added
}

impl Installed {
    fn wanted(&self) -> bool {
        // --load-extension takes a comma-separated list.
        self.dir.to_str().is_some_and(|p| !p.contains(','))
            && self.enabled && self.approved.as_ref().is_some_and(|a| grants(&self.dir).is_ok_and(|g| added_grants(a, &g).is_empty()))
    }
}

impl Store {
    fn save(&self) -> Result<()> {
        let path = self.root.join(REGISTRY);
        let temp = self.root.join(format!("{REGISTRY}.tmp"));
        std::fs::write(&temp, serde_json::to_vec_pretty(&self.registry)?)?;
        std::fs::rename(&temp, &path)?;
        Ok(())
    }
    fn get(&mut self, id: &str) -> Result<&mut Installed> {
        self.registry.extensions.get_mut(id).ok_or_else(|| anyhow!("Extension {id} is not installed"))
    }
    /// Directories in use by this process are moved aside and deleted on the next launch.
    fn discard(&self, dir: &Path) {
        let trash = self.root.join(TRASH);
        let _ = std::fs::create_dir_all(&trash);
        if std::fs::rename(dir, trash.join(uuid::Uuid::new_v4().simple().to_string())).is_err() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
    fn restart_required(&self) -> bool {
        let wanted: BTreeMap<_, _> = self
            .registry
            .extensions
            .values()
            .filter(|e| e.wanted())
            .map(|e| (e.id.clone(), e.dir.clone()))
            .collect();
        wanted != self.loaded
    }
    fn describe(&self, e: &Installed) -> Value {
        let manifest = manifest(&e.dir).unwrap_or(Value::Null);
        let summary = extensions::summarize(&e.dir, &manifest);
        let current = extensions::fingerprint(&summary);
        let loaded = self.loaded.get(&e.id) == Some(&e.dir);
        let page = |path: Option<&str>| {
            path.filter(|_| loaded).map(|p| format!("chrome-extension://{}/{}", e.id, p.trim_start_matches('/')))
        };
        let action = if manifest["action"].is_object() { &manifest["action"] } else { &manifest["browser_action"] };
        let pending = e.pending.as_ref().map(|p| {
            let summary = self::manifest(&p.dir).map(|m| extensions::summarize(&p.dir, &m)).unwrap_or(Value::Null);
            let added = e.approved.as_ref().map(|a| added_grants(a, &extensions::fingerprint(&summary))).unwrap_or_default();
            json!({ "version": p.version, "summary": summary, "added": added })
        });
        let status = if e.approved.is_none() {
            "needs_approval"
        } else if pending.is_some() {
            "update_needs_approval"
        } else if e.enabled {
            "enabled"
        } else {
            "disabled"
        };
        json!({
            "id": e.id,
            "name": summary["name"],
            "version": e.version,
            "enabled": e.enabled,
            "status": status,
            "web_store": e.web_store,
            "update_url": e.update_url,
            "summary": summary,
            "added": e.approved.as_ref().map(|a| added_grants(a, &current)).unwrap_or_default(),
            "pending": pending,
            "loaded": loaded,
            "applies_on_restart": e.wanted() != loaded,
            "popup": page(action["default_popup"].as_str()),
            "options": page(manifest["options_page"].as_str().or(manifest["options_ui"]["page"].as_str())),
            "icon": best_icon(&manifest).is_some(),
        })
    }
}

fn best_icon(manifest: &Value) -> Option<String> {
    let icons = manifest["icons"].as_object()?;
    icons
        .iter()
        .filter_map(|(size, path)| Some((size.parse::<u32>().ok()?, path.as_str()?)))
        .min_by_key(|(size, _)| size.abs_diff(32))
        .map(|(_, path)| path.trim_start_matches('/').to_string())
}

fn with<T>(f: impl FnOnce(&mut Store) -> Result<T>) -> Result<T> {
    let mut guard = STORE.lock().unwrap();
    f(guard.as_mut().ok_or_else(|| anyhow!("Extensions are not available"))?)
}

/// Loads the registry, deletes discarded files, and returns the directories to load.
pub fn init(data_dir: &Path) -> Vec<PathBuf> {
    let root = data_dir.join("extensions");
    let _ = std::fs::create_dir_all(&root);
    let _ = std::fs::remove_dir_all(root.join(TRASH));
    let registry: Registry = std::fs::read(root.join(REGISTRY))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let loaded: BTreeMap<_, _> = registry
        .extensions
        .values()
        .filter(|e| e.wanted())
        .map(|e| (e.id.clone(), e.dir.clone()))
        .collect();
    let dirs = loaded.values().cloned().collect();
    *STORE.lock().unwrap() = Some(Store { root, registry, loaded });
    dirs
}

pub fn list() -> Value {
    with(|s| {
        let installed: Vec<Value> = s.registry.extensions.values().map(|e| s.describe(e)).collect();
        Ok(json!({ "installed": installed, "restart_required": s.restart_required() }))
    })
    .unwrap_or_else(|e| json!({ "installed": [], "restart_required": false, "error": e.to_string() }))
}

pub fn icon(id: &str) -> Option<(Vec<u8>, &'static str)> {
    with(|s| {
        let e = s.get(id)?;
        let path = best_icon(&manifest(&e.dir)?).context("no icon")?;
        let file = e.dir.join(&path).canonicalize()?;
        ensure!(file.starts_with(e.dir.canonicalize()?), "icon is outside the extension");
        let kind = if path.ends_with(".svg") { "image/svg+xml" } else { "image/png" };
        Ok((std::fs::read(file)?, kind))
    })
    .ok()
}

pub fn chromium_version() -> String {
    crate::app::chromium_version()
}

fn download_url(update_url: &str, id: &str) -> String {
    format!(
        "{update_url}?response=redirect&prodversion={}&acceptformat=crx3&x=id%3D{id}%26installsource%3Dondemand%26uc",
        chromium_version()
    )
}

async fn fetch(url: &str) -> Result<Vec<u8>> {
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?
        .get(url)
        .send()
        .await?;
    ensure!(response.status().is_success(), "Download failed with HTTP {}", response.status());
    let bytes = response.bytes().await?;
    ensure!(bytes.len() <= MAX_CRX, "The extension package is too large");
    Ok(bytes.to_vec())
}

/// Verifies and unpacks a CRX into `<root>/<id>/<version>-<n>` with the signing key in the
/// manifest, so Chromium derives the same extension ID when it loads the directory.
fn unpack(root: &Path, bytes: &[u8], expected_id: Option<&str>, require_store: bool) -> Result<(String, String, PathBuf, Value)> {
    let verified = crx::verify(bytes, expected_id)?;
    ensure!(!require_store || verified.from_web_store, "The package is not signed by the Chrome Web Store");
    let parent = root.join(&verified.id);
    std::fs::create_dir_all(&parent)?;
    let staging = parent.join(format!(".staging-{}", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        crx::unzip(verified.archive, &staging)?;
        // Chromium refuses unpacked directories with reserved names such as _metadata.
        let _ = std::fs::remove_dir_all(staging.join("_metadata"));
        let mut manifest = manifest(&staging)?;
        ensure!(matches!(manifest["manifest_version"].as_u64(), Some(2 | 3)), "Extension manifest must be version 2 or 3");
        let version = manifest["version"].as_str().filter(|v| !v.is_empty() && v.len() < 64).context("manifest has no version")?.to_string();
        ensure!(version.bytes().all(|b| b.is_ascii_digit() || b == b'.'), "manifest has an invalid version");
        use base64::Engine;
        manifest["key"] = json!(base64::engine::general_purpose::STANDARD.encode(&verified.public_key));
        std::fs::write(staging.join("manifest.json"), serde_json::to_vec_pretty(&manifest)?)?;
        let dir = parent.join(format!("{version}-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
        std::fs::rename(&staging, &dir)?;
        anyhow::Ok((verified.id.clone(), version, dir.canonicalize()?, manifest))
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}

fn root() -> Result<PathBuf> {
    with(|s| Ok(s.root.clone()))
}

/// Installs a Chrome Web Store extension by ID or store link. It stays off until approved.
pub async fn install_from_store(input: &str) -> Result<Value> {
    let id = crx::parse_store_input(input)?;
    let bytes = fetch(&download_url(WEB_STORE_UPDATE_URL, &id)).await?;
    install(bytes, Some(id), true).await
}

/// Installs a self-hosted CRX. Its ID comes from the package's verified signing key.
pub async fn install_from_crx_url(url: &str) -> Result<Value> {
    let parsed = reqwest::Url::parse(url)?;
    ensure!(matches!(parsed.scheme(), "https" | "http"), "Only http(s) CRX links are supported");
    let bytes = fetch(url).await?;
    install(bytes, None, false).await
}

async fn install(bytes: Vec<u8>, id: Option<String>, from_store: bool) -> Result<Value> {
    let root = root()?;
    if let Some(id) = &id {
        ensure!(with(|s| Ok(!s.registry.extensions.contains_key(id)))?, "This extension is already installed");
    }
    let (id, version, dir, manifest) =
        tokio::task::spawn_blocking(move || unpack(&root, &bytes, id.as_deref(), from_store)).await??;
    let update_url = manifest["update_url"].as_str().map(str::to_string).or(from_store.then(|| WEB_STORE_UPDATE_URL.into()));
    with(|s| {
        if s.registry.extensions.contains_key(&id) {
            s.discard(&dir);
            bail!("This extension is already installed");
        }
        let web_store = from_store || update_url.as_deref() == Some(WEB_STORE_UPDATE_URL);
        let installed_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let entry = Installed { id: id.clone(), version, dir, enabled: false, approved: None, update_url, web_store, installed_at, pending: None };
        let described = s.describe(&entry);
        s.registry.extensions.insert(id, entry);
        s.save()?;
        Ok(described)
    })
}

/// approve, enable, disable, remove, or reject (discard a pending install or update).
pub fn act(id: &str, action: &str) -> Result<Value> {
    with(|s| {
        let e = s.get(id)?;
        match action {
            "approve" => {
                if let Some(pending) = e.pending.take() {
                    let old = std::mem::replace(&mut e.dir, pending.dir);
                    e.version = pending.version;
                    let old = (old != e.dir).then_some(old);
                    e.approved = Some(grants(&e.dir)?);
                    e.enabled = true;
                    if let Some(old) = old {
                        s.discard(&old);
                    }
                } else {
                    e.approved = Some(grants(&e.dir)?);
                    e.enabled = true;
                }
            }
            "enable" => {
                let current = grants(&e.dir)?;
                ensure!(e.approved.as_ref().is_some_and(|a| added_grants(a, &current).is_empty()), "Review and approve this extension's permissions first");
                e.enabled = true;
            }
            "disable" => e.enabled = false,
            "reject" => {
                if let Some(pending) = e.pending.take() {
                    s.discard(&pending.dir);
                } else if e.approved.is_none() {
                    return act_remove(s, id);
                }
            }
            "remove" => return act_remove(s, id),
            _ => bail!("Unknown extension action {action}"),
        }
        s.save()?;
        let e = s.registry.extensions[id].clone();
        Ok(json!({ "extension": s.describe(&e), "restart_required": s.restart_required() }))
    })
}

fn act_remove(s: &mut Store, id: &str) -> Result<Value> {
    let e = s.registry.extensions.remove(id).context("not installed")?;
    s.discard(&e.dir);
    if let Some(p) = e.pending {
        s.discard(&p.dir);
    }
    let _ = std::fs::remove_dir(s.root.join(id));
    s.save()?;
    Ok(json!({ "removed": id, "restart_required": s.restart_required() }))
}

fn newer(a: &str, b: &str) -> bool {
    let parts = |v: &str| v.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    parts(a) > parts(b)
}

/// The `<updatecheck>` attributes of one app in an update2 XML response.
pub fn parse_update_check(xml: &str, id: &str) -> Option<BTreeMap<String, String>> {
    let app = xml.find(&format!("appid=\"{id}\""))?;
    let rest = &xml[app..];
    let start = rest.find("<updatecheck")?;
    let tag = &rest[start + "<updatecheck".len()..];
    let tag = &tag[..tag.find('>')?];
    let mut attrs = BTreeMap::new();
    let mut s = tag;
    while let Some(eq) = s.find("=\"") {
        let name = s[..eq].trim().trim_end_matches('/').rsplit(' ').next().unwrap_or("").to_string();
        let value_end = s[eq + 2..].find('"')?;
        let value = s[eq + 2..eq + 2 + value_end].replace("&amp;", "&");
        attrs.insert(name, value);
        s = &s[eq + 2 + value_end + 1..];
    }
    Some(attrs)
}

/// Queries each extension's update service (update2 protocol) and stages newer versions.
/// Updates that request more access wait for approval; others apply on the next launch.
pub async fn check_updates() -> Result<Value> {
    let targets = with(|s| {
        Ok(s.registry.extensions.values().filter_map(|e| Some((e.id.clone(), e.pending.as_ref().map_or(e.version.clone(), |p| p.version.clone()), e.update_url.clone()?, e.web_store))).collect::<Vec<_>>())
    })?;
    let mut results = Vec::new();
    for (id, version, update_url, web_store) in targets {
        let outcome: Result<Value> = async {
            let query = format!(
                "{update_url}?response=updatecheck&prodversion={}&acceptformat=crx3&x=id%3D{id}%26v%3D{version}%26uc",
                chromium_version()
            );
            let xml = String::from_utf8(fetch(&query).await?)?;
            let check = parse_update_check(&xml, &id).context("The update service did not answer for this extension")?;
            let available = check.get("version").cloned().unwrap_or_default();
            if check.get("status").map(String::as_str) != Some("ok") || !newer(&available, &version) {
                return Ok(json!({ "id": id, "status": "up_to_date", "version": version }));
            }
            let codebase = check.get("codebase").context("The update has no download location")?;
            let bytes = fetch(codebase).await?;
            if let Some(expected) = check.get("hash_sha256") {
                let actual: String = ring::digest::digest(&ring::digest::SHA256, &bytes).as_ref().iter().map(|b| format!("{b:02x}")).collect();
                ensure!(actual.eq_ignore_ascii_case(expected), "The downloaded update does not match its published hash");
            }
            let root = root()?;
            let expected = id.clone();
            let (_, new_version, dir, _) = tokio::task::spawn_blocking(move || unpack(&root, &bytes, Some(&expected), web_store)).await??;
            ensure!(new_version == available || newer(&new_version, &version), "The update package has an unexpected version");
            with(|s| {
                let e = s.get(&id)?;
                let added = e.approved.as_ref().map(|a| added_grants(a, &grants(&dir).unwrap_or(Value::Null))).unwrap_or_default();
                if let Some(old) = e.pending.take() {
                    s.discard(&old.dir);
                }
                let e = s.get(&id)?;
                let status = if e.approved.is_some() && added.is_empty() {
                    let old = std::mem::replace(&mut e.dir, dir);
                    e.version = new_version.clone();
                    s.discard(&old);
                    "updated"
                } else {
                    e.pending = Some(Pending { version: new_version.clone(), dir });
                    "needs_approval"
                };
                s.save()?;
                Ok(json!({ "id": id, "status": status, "version": new_version, "added": added }))
            })
        }
        .await;
        results.push(outcome.unwrap_or_else(|e| json!({ "id": id, "status": "error", "error": e.to_string() })));
    }
    let restart_required = with(|s| Ok(s.restart_required()))?;
    Ok(json!({ "results": results, "restart_required": restart_required }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn added_grants_ignore_removals() {
        let approved = json!({ "permissions": ["storage", "tabs"], "host_permissions": ["https://a.test/*"] });
        let current = json!({ "permissions": ["storage", "cookies"], "host_permissions": [] });
        assert_eq!(added_grants(&approved, &current), vec!["cookies"]);
        assert!(added_grants(&current, &json!({ "permissions": ["storage"] })).is_empty());
    }

    #[test]
    fn update_responses_are_parsed() {
        let xml = r#"<gupdate><app appid="bcjindcccaagfpapjjmafapmmgkkhgoa" status="ok"><updatecheck codebase="https://x.test/a.crx?a=1&amp;b=2" hash_sha256="ab" status="ok" version="0.10.2"/></app></gupdate>"#;
        let check = parse_update_check(xml, "bcjindcccaagfpapjjmafapmmgkkhgoa").unwrap();
        assert_eq!(check["version"], "0.10.2");
        assert_eq!(check["codebase"], "https://x.test/a.crx?a=1&b=2");
        assert_eq!(check["status"], "ok");
        let none = r#"<app appid="ddkjiahejlhfcafbddmgiahcphecmpfh" status="ok"><updatecheck _esbAllowlist="true" status="noupdate"/></app>"#;
        assert_eq!(parse_update_check(none, "ddkjiahejlhfcafbddmgiahcphecmpfh").unwrap()["status"], "noupdate");
        assert!(newer("0.10.2", "0.9.0") && !newer("1.0", "1.0") && newer("2026.1021.1354", "2026.1020.9"));
    }

    #[test]
    fn approval_gates_loading_and_updates() {
        let root = std::env::temp_dir().join(format!("atrium-store-test-{}", uuid::Uuid::new_v4()));
        let dir = root.join("extensions/x/1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest.json"), r#"{"name":"X","version":"1","manifest_version":3,"permissions":["storage"]}"#).unwrap();
        let entry = Installed { id: "x".into(), version: "1".into(), dir: dir.clone(), enabled: false, approved: None, update_url: None, web_store: false, installed_at: 0, pending: None };
        let mut registry = Registry::default();
        registry.extensions.insert("x".into(), entry);
        std::fs::write(root.join("extensions").join(REGISTRY), serde_json::to_vec(&registry).unwrap()).unwrap();
        assert!(init(&root).is_empty());
        assert!(act("x", "enable").is_err());
        act("x", "approve").unwrap();
        assert!(list()["restart_required"] == true);
        assert_eq!(init(&root), vec![dir.clone()]);
        // A manifest that gains access stops loading until it is approved again.
        std::fs::write(dir.join("manifest.json"), r#"{"name":"X","version":"1","manifest_version":3,"permissions":["storage","cookies"]}"#).unwrap();
        assert!(init(&root).is_empty());
        assert_eq!(list()["installed"][0]["added"], json!(["cookies"]));
        act("x", "approve").unwrap();
        act("x", "disable").unwrap();
        assert!(init(&root).is_empty());
        act("x", "remove").unwrap();
        assert!(list()["installed"].as_array().unwrap().is_empty());
        assert!(!root.join("extensions/x").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
