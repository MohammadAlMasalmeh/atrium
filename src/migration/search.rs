//! Default search engine, imported from Chrome's preferences and keyword table.
use std::{path::Path, sync::RwLock};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const FILE: &str = "search-engine.json";
static ENGINE: RwLock<Option<Engine>> = RwLock::new(None);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Engine {
    pub name: String,
    pub keyword: String,
    /// URL with a `{searchTerms}` placeholder.
    pub template: String,
}

/// Search URL for an already percent-encoded query.
pub fn query_url(encoded: &str) -> String {
    match ENGINE.read().unwrap().as_ref() {
        Some(e) => e.template.replace("{searchTerms}", encoded),
        None => format!("https://www.google.com/search?q={encoded}"),
    }
}

pub fn current() -> Option<Engine> {
    ENGINE.read().unwrap().clone()
}

pub fn load(dir: &Path) {
    let engine = std::fs::read(dir.join(FILE)).ok().and_then(|b| serde_json::from_slice::<Engine>(&b).ok()).filter(|e| template(&e.template).is_some());
    *ENGINE.write().unwrap() = engine;
}

pub fn save(dir: &Path, engine: &Engine) -> Result<()> {
    super::write_atomic(&dir.join(FILE), &serde_json::to_vec_pretty(engine)?)?;
    *ENGINE.write().unwrap() = Some(engine.clone());
    Ok(())
}

/// Converts a Chrome template to one Aside can fill, or None if it isn't a web search URL.
pub fn template(raw: &str) -> Option<String> {
    let mut t = raw.trim().replace("{google:baseURL}", "https://www.google.com/");
    if !t.contains("{searchTerms}") {
        return None;
    }
    t = t.replace("{searchTerms}", "\u{1}");
    // Drop Chrome-internal and optional parameters such as {google:RLZ} or {startIndex?}.
    while let Some(start) = t.find('{') {
        let end = t[start..].find('}').map(|e| start + e + 1)?;
        let token = &t[start..end];
        let value = match token {
            "{inputEncoding}" | "{outputEncoding}" => "UTF-8",
            _ => "",
        };
        t.replace_range(start..end, value);
    }
    let (base, query) = t.split_once('?').map_or((t.as_str(), None), |(b, q)| (b, Some(q)));
    let query = query.map(|q| q.split('&').filter(|p| !p.is_empty() && !p.ends_with('=') || p.contains('\u{1}')).collect::<Vec<_>>().join("&"));
    let t = match query {
        Some(q) if !q.is_empty() => format!("{base}?{q}"),
        _ => base.to_string(),
    };
    let url = reqwest::Url::parse(&t.replace('\u{1}', "q")).ok()?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1"));
    if !(url.scheme() == "https" || url.scheme() == "http" && local) || !url.username().is_empty() {
        return None;
    }
    Some(t.replace('\u{1}', "{searchTerms}"))
}

fn engine(data: &Value) -> Option<Engine> {
    Some(Engine {
        name: data["short_name"].as_str().unwrap_or_default().to_string(),
        keyword: data["keyword"].as_str().unwrap_or_default().to_string(),
        template: template(data["url"].as_str()?)?,
    })
}

/// Chrome keeps a user-chosen default in Preferences, either inline or as a keyword's sync GUID.
pub async fn read(profile: &Path) -> Result<Option<Engine>> {
    let prefs = super::read_json(&profile.join("Preferences")).context("Chrome preferences could not be read.")?;
    if let Some(e) = engine(&prefs["default_search_provider_data"]["template_url_data"]) {
        return Ok(Some(e));
    }
    let guid = prefs["default_search_provider"]["synced_guid"].as_str().or(prefs["default_search_provider"]["guid"].as_str());
    let Some(guid) = guid.filter(|g| !g.is_empty() && g.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-')) else {
        return Ok(None);
    };
    let rows = super::sqlite(&profile.join("Web Data"), &format!("SELECT short_name, keyword, url FROM keywords WHERE sync_guid = '{guid}' LIMIT 1")).await?;
    Ok(rows.first().and_then(engine))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_templates_become_plain_search_urls() {
        assert_eq!(
            template("{google:baseURL}search?q={searchTerms}&{google:RLZ}{google:originalQueryForSuggestion}sourceid=chrome&ie={inputEncoding}").unwrap(),
            "https://www.google.com/search?q={searchTerms}&sourceid=chrome&ie=UTF-8"
        );
        assert_eq!(template("https://duckduckgo.com/?q={searchTerms}").unwrap(), "https://duckduckgo.com/?q={searchTerms}");
        assert_eq!(template("http://127.0.0.1:7861/search?q={searchTerms}").unwrap(), "http://127.0.0.1:7861/search?q={searchTerms}");
        for bad in ["javascript:alert({searchTerms})", "http://example.com/?q={searchTerms}", "https://example.com/", "file:///{searchTerms}"] {
            assert!(template(bad).is_none(), "{bad}");
        }
    }
}
