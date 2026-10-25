//! Built-in ad blocking, independent of the browser engine.
//!
//! Header-bidding pages load dozens of cross-site ad frames, and with site isolation each
//! one costs a renderer process. Refusing third-party subresources from ad exchanges keeps
//! those frames from being created. Top-level navigations are never blocked, so following
//! an ad link still works, and a site is never blocked from loading its own resources.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Ad exchanges, ad servers, and the ID-sync endpoints they chain through. Analytics and
/// tag managers are left out: pages call into them and some break when they are missing.
const DOMAINS: &[&str] = &[
    "33across.com", "3lift.com", "adform.net", "adnxs.com", "adroll.com", "adsafeprotected.com",
    "adservice.google.com", "adsrvr.org", "advertising.com", "agkn.com", "amazon-adsystem.com",
    "bidswitch.net", "bluekai.com", "casalemedia.com", "contextweb.com", "criteo.com", "criteo.net",
    "crwdcntrl.net", "demdex.net", "districtm.io", "doubleclick.net", "doubleverify.com",
    "everesttech.net", "exelator.com", "eyeota.net", "googleadservices.com",
    "googlesyndication.com", "gumgum.com", "id5-sync.com", "indexww.com", "krxd.net", "liadm.com",
    "lijit.com", "mathtag.com", "media.net", "mgid.com", "moatads.com", "onetag-sys.com",
    "openx.net", "outbrain.com", "pubmatic.com", "quantserve.com", "revcontent.com", "rlcdn.com",
    "rubiconproject.com", "scorecardresearch.com", "serving-sys.com", "sharethrough.com",
    "smartadserver.com", "sovrn.com", "spotxchange.com", "springserve.com", "taboola.com",
    "tapad.com", "teads.tv", "turn.com", "yieldmo.com", "zemanta.com",
];

static ENABLED: AtomicBool = AtomicBool::new(true);
static BLOCKED: AtomicU64 = AtomicU64::new(0);

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Requests refused since launch.
pub fn blocked_count() -> u64 {
    BLOCKED.load(Ordering::Relaxed)
}

/// Whether a subresource or subframe request for `url`, made by a document from
/// `initiator`, should be refused. Counts each refusal.
pub fn should_block(url: &str, initiator: Option<&str>) -> bool {
    if !enabled() {
        return false;
    }
    let Some(listed) = host(url).and_then(listed_domain) else { return false };
    // First-party: the ad company's own pages load their own resources.
    if initiator.and_then(host).is_some_and(|h| within(h, listed)) {
        return false;
    }
    BLOCKED.fetch_add(1, Ordering::Relaxed);
    true
}

/// The listed domain `host` falls under, checking each parent domain in turn.
fn listed_domain(host: &str) -> Option<&'static str> {
    let mut rest = host;
    loop {
        if let Ok(i) = DOMAINS.binary_search(&rest) {
            return Some(DOMAINS[i]);
        }
        rest = &rest[rest.find('.')? + 1..];
    }
}

fn within(host: &str, domain: &str) -> bool {
    host == domain || host.strip_suffix(domain).is_some_and(|p| p.ends_with('.'))
}

/// The lowercase host of an http(s) or ws(s) URL; other schemes are never blocked.
fn host(url: &str) -> Option<&str> {
    let (scheme, rest) = url.split_once("://")?;
    if !matches!(scheme, "http" | "https" | "ws" | "wss") {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = host.split(':').next()?.trim_end_matches('.');
    // Chromium hands over canonical (lowercase) URLs; anything else is left alone.
    (!host.is_empty() && !host.bytes().any(|b| b.is_ascii_uppercase())).then_some(host)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn domain_list_is_sorted_for_binary_search() {
        assert!(DOMAINS.windows(2).all(|w| w[0] < w[1]));
    }
    #[test]
    fn blocks_third_party_ad_hosts_and_their_subdomains() {
        assert!(should_block("https://securepubads.g.doubleclick.net/tag/js/gpt.js", Some("https://www.bbc.com")));
        assert!(should_block("https://pagead2.googlesyndication.com/x", Some("https://stackoverflow.com")));
        assert!(should_block("https://adservice.google.com/ddm/fls", Some("https://www.youtube.com")));
        assert!(should_block("https://ib.adnxs.com:443/ut/v3", None));
    }
    #[test]
    fn leaves_sites_and_lookalikes_alone() {
        assert!(!should_block("https://www.google.com/search?q=x", Some("https://www.google.com")));
        assert!(!should_block("https://notdoubleclick.net/a.js", Some("https://example.com")));
        assert!(!should_block("https://doubleclick.net.example.com/a.js", Some("https://example.com")));
        assert!(!should_block("https://www.googletagmanager.com/gtm.js", Some("https://example.com")));
        assert!(!should_block("data:text/html,doubleclick.net", Some("https://example.com")));
    }
    #[test]
    fn first_party_requests_are_allowed() {
        assert!(!should_block("https://static.criteo.net/a.js", Some("https://www.criteo.net")));
        assert!(!should_block("https://cdn.taboola.com/x.js", Some("https://taboola.com")));
    }
}
