//! Address suggestions from imported Chrome history/bookmarks and search engines.
//!
//! The shell keeps only its own recent visits, so an imported profile's history was
//! unreachable from the command bar. The index is built once from the import files and
//! rebuilt only when they change; a lookup scans it with no allocation per entry.
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

#[derive(Clone, Serialize, Debug, PartialEq)]
pub struct Suggestion {
    pub url: String,
    pub title: String,
    pub kind: &'static str,
}

struct Entry {
    url: String,
    title: String,
    kind: &'static str,
    /// Lowercase host without `www.`, then the lowercase URL and title, for matching.
    host: String,
    haystack: String,
    /// Visits weighted by typing and recency; bookmarks get a fixed share.
    weight: f64,
}

#[derive(Default)]
struct Index {
    stamp: Vec<Option<SystemTime>>,
    entries: Vec<Entry>,
}

static INDEX: Mutex<Option<(PathBuf, Index)>> = Mutex::new(None);

const DAY_MS: f64 = 86_400_000.0;

fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let host = rest.split(['/', '?', '#', ':']).next().unwrap_or_default().to_lowercase();
    host.strip_prefix("www.").map(str::to_string).unwrap_or(host)
}

fn entry(url: &str, title: &str, kind: &'static str, weight: f64) -> Entry {
    Entry {
        host: host_of(url),
        haystack: format!("{} {}", url.to_lowercase(), title.to_lowercase()),
        url: url.into(),
        title: title.into(),
        kind,
        weight,
    }
}

fn build(data_dir: &Path, now_ms: i64) -> Vec<Entry> {
    let library = crate::migration::library(data_dir, usize::MAX);
    let mut entries = Vec::new();
    for h in library["history"].as_array().into_iter().flatten() {
        let (Some(url), title) = (h["url"].as_str(), h["title"].as_str().unwrap_or_default()) else { continue };
        let visits = h["visits"].as_array().map_or(0, Vec::len) as f64;
        let typed = h["typed"].as_f64().unwrap_or(0.0);
        let last = h["visits"][0].as_i64().unwrap_or(0);
        let age_days = ((now_ms - last) as f64 / DAY_MS).max(0.0);
        let weight = (1.0 + visits + 3.0 * typed).ln() / (1.0 + age_days / 14.0);
        entries.push(entry(url, title, "history", weight));
    }
    for b in library["bookmarks"].as_array().into_iter().flatten() {
        if let Some(url) = b["url"].as_str() {
            entries.push(entry(url, b["title"].as_str().unwrap_or_default(), "bookmark", 1.5));
        }
    }
    entries
}

/// How well `q` (lowercase, non-empty tokens) matches: the host's start beats the
/// start of a word, which beats anywhere. Every token must match somewhere.
fn quality(e: &Entry, tokens: &[&str]) -> f64 {
    let mut total = 0.0;
    for (i, t) in tokens.iter().enumerate() {
        let score = if i == 0 && e.host.starts_with(t) {
            100.0
        } else if let Some(at) = e.haystack.find(t) {
            let word_start = at == 0 || !e.haystack.as_bytes()[at - 1].is_ascii_alphanumeric();
            if word_start { 40.0 } else { 10.0 }
        } else {
            return 0.0;
        };
        total += score;
    }
    total
}

fn rank(entries: &[Entry], query: &str, limit: usize) -> Vec<Suggestion> {
    let lower = query.to_lowercase();
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    if tokens.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(f64, &Entry)> = entries
        .iter()
        .filter_map(|e| {
            let q = quality(e, &tokens);
            (q > 0.0).then_some((q, e))
        })
        .collect();
    // A better kind of match always wins; visits and recency order pages within one.
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.weight.total_cmp(&a.1.weight)));
    // The same page often appears under several URLs (anchors, tracking parameters).
    let (mut urls, mut pages) = (std::collections::HashSet::new(), std::collections::HashSet::new());
    scored
        .into_iter()
        .filter(|(_, e)| urls.insert(e.url.as_str()) && (e.title.is_empty() || pages.insert((e.host.as_str(), e.title.as_str()))))
        .take(limit)
        .map(|(_, e)| Suggestion { url: e.url.clone(), title: e.title.clone(), kind: e.kind })
        .collect()
}

pub fn suggest(data_dir: &Path, query: &str, limit: usize) -> Vec<Suggestion> {
    let files = [data_dir.join("import/history.json"), data_dir.join("import/bookmarks.json")];
    let stamp: Vec<_> = files.iter().map(|f| std::fs::metadata(f).and_then(|m| m.modified()).ok()).collect();
    let mut index = INDEX.lock().unwrap();
    if !index.as_ref().is_some_and(|(dir, i)| dir == data_dir && i.stamp == stamp) {
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
        *index = Some((data_dir.to_path_buf(), Index { entries: build(data_dir, now), stamp }));
    }
    rank(&index.as_ref().unwrap().1.entries, query, limit)
}

/// Don't send addresses, paths or email addresses to a remote search service.
fn search_query(query: &str) -> bool {
    let len = query.chars().count();
    let first = query.split_whitespace().next().unwrap_or_default();
    (2..=256).contains(&len)
        && !query.chars().any(char::is_control)
        && !first.contains([':', '/', '\\', '@'])
        && !(query.split_whitespace().count() == 1 && (first.contains('.') || first.starts_with("localhost")))
}

fn search_endpoint(engine: Option<&crate::migration::search::Engine>) -> Option<reqwest::Url> {
    let host = match engine {
        Some(e) => reqwest::Url::parse(&e.template).ok()?.host_str()?.to_string(),
        None => "www.google.com".to_string(),
    };
    // Keep predictions with the chosen provider; an unknown engine stays local.
    let endpoint = match host.strip_prefix("www.").unwrap_or(&host) {
        "google.com" => "https://www.google.com/complete/search?client=firefox",
        "bing.com" => "https://www.bing.com/osjson.aspx",
        "duckduckgo.com" => "https://ac.duckduckgo.com/ac/?type=list",
        _ => return None,
    };
    reqwest::Url::parse(endpoint).ok()
}

fn search_results(doc: &serde_json::Value, query: &str, limit: usize) -> Vec<String> {
    let mut seen = std::collections::HashSet::from([query.to_lowercase()]);
    doc[1].as_array().into_iter().flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.chars().count() <= 256 && !s.chars().any(char::is_control) && seen.insert(s.to_lowercase()))
        .take(limit.min(8))
        .map(str::to_string)
        .collect()
}

/// Search-engine predictions use a separate cookie-free client, never the browsing profile.
pub async fn search(query: &str, limit: usize) -> Vec<String> {
    if !search_query(query) || limit == 0 || std::env::var_os("ATRIUM_SEARCH_SUGGESTIONS").is_some_and(|v| v == "0") {
        return Vec::new();
    }
    let engine = crate::migration::search::current();
    let mut endpoint = search_endpoint(engine.as_ref());
    // Tests may replace the provider with a loopback fixture only under the mock keychain.
    if crate::migration::mock_keychain() {
        endpoint = std::env::var("ATRIUM_TEST_SUGGEST_URL").ok()
            .and_then(|s| reqwest::Url::parse(&s).ok())
            .filter(|u| u.scheme() == "http" && u.host_str() == Some("127.0.0.1") && u.username().is_empty() && u.password().is_none());
    }
    let Some(mut endpoint) = endpoint else { return Vec::new() };
    let param = if endpoint.host_str().is_some_and(|h| h == "www.bing.com") { "query" } else { "q" };
    endpoint.query_pairs_mut().append_pair(param, query);
    static CLIENT: std::sync::LazyLock<Option<reqwest::Client>> = std::sync::LazyLock::new(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(2))
            .user_agent("Atrium")
            .build().ok()
    });
    let Some(client) = CLIENT.as_ref() else { return Vec::new() };
    let result: anyhow::Result<Vec<String>> = async {
        let mut response = client.get(endpoint).send().await?.error_for_status()?;
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            anyhow::ensure!(body.len() + chunk.len() <= 64 * 1024, "suggestion response too large");
            body.extend_from_slice(&chunk);
        }
        Ok(search_results(&serde_json::from_slice(&body)?, query, limit))
    }.await;
    result.unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn predictions_only_send_search_terms() {
        for q in ["new york", "weather", "東京", "node.js tutorial"] {
            assert!(search_query(q), "{q}");
        }
        for q in ["", "n", "https://example.com/private", "example.com", "localhost:7777", "/private/file", "user@example.com", "about:blank", "new\nyork"] {
            assert!(!search_query(q), "{q}");
        }
        assert!(!search_query(&"x".repeat(257)));
    }
    #[test]
    fn prediction_provider_follows_the_search_engine() {
        let engine = |host: &str| crate::migration::search::Engine {
            name: "Test".into(), keyword: host.into(), template: format!("https://{host}/search?q={{searchTerms}}"),
        };
        assert_eq!(search_endpoint(None).unwrap().host_str(), Some("www.google.com"));
        for (host, expected) in [("www.google.com", "www.google.com"), ("bing.com", "www.bing.com"), ("duckduckgo.com", "ac.duckduckgo.com")] {
            assert_eq!(search_endpoint(Some(&engine(host))).unwrap().host_str(), Some(expected));
        }
        assert!(search_endpoint(Some(&engine("custom.example"))).is_none());
        assert!(search_endpoint(Some(&engine("google.com.evil.example"))).is_none());
    }
    #[test]
    fn prediction_responses_are_bounded_and_deduplicated() {
        let doc = serde_json::json!(["new york", ["new york", "new york city", "New York City", 42, "", "new york giants", "bad\nresult"]]);
        assert_eq!(search_results(&doc, "new york", 8), ["new york city", "new york giants"]);
        assert_eq!(search_results(&doc, "new york", 1), ["new york city"]);
        assert!(search_results(&serde_json::json!({"error":"unavailable"}), "new york", 8).is_empty());
    }
    fn e(url: &str, title: &str, weight: f64) -> Entry {
        entry(url, title, "history", weight)
    }
    #[test]
    fn host_prefix_beats_word_and_substring_matches() {
        let entries = [
            e("https://example.com/github-notes", "Notes", 3.0),
            e("https://docs.rs/x", "Using GitHub actions", 3.0),
            e("https://www.github.com/", "GitHub", 0.1),
        ];
        let urls: Vec<_> = rank(&entries, "git", 3).into_iter().map(|s| s.url).collect();
        assert_eq!(urls[0], "https://www.github.com/");
        assert_eq!(urls.len(), 3);
    }
    #[test]
    fn every_token_must_match_and_frequent_pages_rank_higher() {
        let entries = [e("https://a.test/rust-book", "The Rust Book", 0.2), e("https://b.test/rust-book", "The Rust Book", 2.0), e("https://c.test/", "Rust", 5.0)];
        let urls: Vec<_> = rank(&entries, "rust book", 5).into_iter().map(|s| s.url).collect();
        assert_eq!(urls, vec!["https://b.test/rust-book", "https://a.test/rust-book"]);
    }
    #[test]
    fn duplicates_and_empty_queries_return_nothing_extra() {
        let entries = [e("https://a.test/", "A", 1.0), entry("https://a.test/", "A", "bookmark", 1.5)];
        assert_eq!(rank(&entries, "a.test", 5).len(), 1);
        assert!(rank(&entries, "   ", 5).is_empty());
        let same_page = [e("https://g.test/pr/1", "Fix bug", 2.0), e("https://g.test/pr/1#c3", "Fix bug", 1.0), e("https://h.test/pr/1", "Fix bug", 1.0)];
        assert_eq!(rank(&same_page, "fix", 5).len(), 2);
    }
}
