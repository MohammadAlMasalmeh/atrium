//! Chrome cookie import. Runs only on an explicit, consented request: reading the
//! "Chrome Safe Storage" key shows a macOS keychain prompt by design.
use std::{ffi::c_void, path::Path};

use anyhow::{bail, ensure, Context, Result};
use serde::Serialize;
use serde_json::Value;

pub const SECURITY_NOTICE: &str = "Imported cookies are live sign-in sessions. Anything that can read this Atrium profile, including software running as you and installed extensions, can use them to act as you on those sites. Atrium keeps them in its own Chromium profile, encrypted with Atrium's own keychain key. Chrome's copies are not changed. Some sites tie sessions to one browser and may sign you out or ask you to verify. To revoke a session, sign out of that site.";

/// Microseconds between 1601-01-01 (Chrome and CEF time) and the Unix epoch.
const WINDOWS_EPOCH_OFFSET_US: i64 = 11_644_473_600_000_000;
const MAX_COOKIES: usize = 20_000;

/// A decrypted cookie ready for CEF's cookie manager. Times are Chrome/CEF microseconds.
#[derive(Clone, Debug)]
pub struct Write {
    pub url: String,
    pub name: String,
    pub value: String,
    /// Empty for host-only cookies.
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub httponly: bool,
    pub creation: i64,
    pub last_access: i64,
    pub expires: Option<i64>,
    /// CEF `cef_cookie_same_site_t`.
    pub same_site: i32,
    /// CEF `cef_cookie_priority_t`.
    pub priority: i32,
}

/// A cookie read back from CEF for verification.
#[derive(Clone, Debug)]
pub struct Stored {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub httponly: bool,
    pub same_site: i32,
    pub expires: Option<i64>,
}

#[derive(Default, Serialize)]
pub struct Plan {
    #[serde(skip)]
    pub cookies: Vec<Write>,
    pub skipped_expired: usize,
    pub skipped_partitioned: usize,
    pub skipped_filtered: usize,
    pub errors: Vec<String>,
}

mod cc {
    use std::ffi::c_void;
    extern "C" {
        pub fn CCKeyDerivationPBKDF(algorithm: u32, password: *const u8, password_len: usize, salt: *const u8, salt_len: usize, prf: u32, rounds: u32, derived: *mut u8, derived_len: usize) -> i32;
        pub fn CCCrypt(op: u32, alg: u32, options: u32, key: *const c_void, key_len: usize, iv: *const c_void, input: *const c_void, input_len: usize, output: *mut c_void, output_len: usize, moved: *mut usize) -> i32;
        pub fn CC_SHA256(data: *const c_void, len: u32, md: *mut u8) -> *mut u8;
    }
}

pub fn derive_key(password: &[u8]) -> Result<[u8; 16]> {
    let mut key = [0u8; 16];
    let salt = b"saltysalt";
    // kCCPBKDF2 = 2, kCCPRFHmacAlgSHA1 = 1; Chrome on macOS uses 1003 rounds.
    let status = unsafe { cc::CCKeyDerivationPBKDF(2, password.as_ptr(), password.len(), salt.as_ptr(), salt.len(), 1, 1003, key.as_mut_ptr(), key.len()) };
    ensure!(status == 0, "The cookie key could not be derived.");
    Ok(key)
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    unsafe { cc::CC_SHA256(data.as_ptr().cast(), data.len() as u32, out.as_mut_ptr()) };
    out
}

/// Chrome's macOS format: "v10" + AES-128-CBC (IV of 16 spaces, PKCS#7). Cookie
/// database version 24 and later prefixes the plaintext with SHA-256(host_key).
pub fn decrypt(key: &[u8; 16], blob: &[u8], host_key: &str, db_version: i64) -> Result<Vec<u8>> {
    let Some(data) = blob.strip_prefix(b"v10") else { bail!("unsupported encryption format") };
    ensure!(!data.is_empty() && data.len() % 16 == 0, "corrupt encrypted value");
    let iv = [b' '; 16];
    let mut out = vec![0u8; data.len() + 16];
    let mut moved = 0usize;
    // kCCDecrypt = 1, kCCAlgorithmAES128 = 0, kCCOptionPKCS7Padding = 1.
    let status = unsafe {
        cc::CCCrypt(1, 0, 1, key.as_ptr() as *const c_void, key.len(), iv.as_ptr() as *const c_void,
            data.as_ptr() as *const c_void, data.len(), out.as_mut_ptr() as *mut c_void, out.len(), &mut moved)
    };
    ensure!(status == 0, "decryption failed (wrong key or corrupt value)");
    out.truncate(moved);
    if db_version >= 24 {
        ensure!(out.len() >= 32 && out[..32] == sha256(host_key.as_bytes()), "value does not belong to this domain");
        out.drain(..32);
    }
    Ok(out)
}

fn hex(s: &str) -> Result<Vec<u8>> {
    ensure!(s.len() % 2 == 0, "bad hex");
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(Into::into)).collect()
}

pub fn now_us() -> i64 {
    let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    unix.as_micros() as i64 + WINDOWS_EPOCH_OFFSET_US
}

fn valid_host(host: &str) -> bool {
    !host.is_empty() && host.len() <= 253
        && host.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_'))
        && !host.trim_start_matches('.').is_empty()
}

fn wanted(host: &str, domains: &[String]) -> bool {
    let host = host.trim_start_matches('.').to_ascii_lowercase();
    domains.is_empty() || domains.iter().any(|d| {
        let d = d.trim().trim_start_matches('.').to_ascii_lowercase();
        host == d || host.ends_with(&format!(".{d}"))
    })
}

/// Maps one Chrome `cookies` row. `Ok(None)` means skipped (expired or partitioned).
fn row(key: &[u8; 16], r: &Value, now: i64, db_version: i64, plan: &mut Plan) -> Result<Option<Write>> {
    let host = r["host_key"].as_str().unwrap_or_default();
    let name = r["name"].as_str().unwrap_or_default();
    ensure!(valid_host(host), "invalid domain");
    if r["top_frame_site_key"].as_str().is_some_and(|k| !k.is_empty()) {
        plan.skipped_partitioned += 1;
        return Ok(None);
    }
    let expires = r["expires_utc"].as_i64().unwrap_or(0);
    let persistent = r["has_expires"].as_i64().or(r["is_persistent"].as_i64()).unwrap_or(0) != 0 && expires > 0;
    if persistent && expires <= now {
        plan.skipped_expired += 1;
        return Ok(None);
    }
    let encrypted = hex(r["ev"].as_str().unwrap_or_default())?;
    let raw = if encrypted.is_empty() { r["value"].as_str().unwrap_or_default().as_bytes().to_vec() } else { decrypt(key, &encrypted, host, db_version)? };
    let value = String::from_utf8(raw).context("value is not text")?;
    let path = r["path"].as_str().filter(|p| p.starts_with('/')).unwrap_or("/");
    let secure = r["is_secure"].as_i64() == Some(1);
    let bare = host.trim_start_matches('.');
    let scheme = if secure { "https" } else { "http" };
    Ok(Some(Write {
        url: format!("{scheme}://{bare}{path}"),
        name: name.into(),
        value,
        domain: if host.starts_with('.') { host.into() } else { String::new() },
        path: path.into(),
        secure,
        httponly: r["is_httponly"].as_i64() == Some(1),
        creation: r["creation_utc"].as_i64().unwrap_or(now),
        last_access: r["last_access_utc"].as_i64().unwrap_or(now),
        expires: persistent.then_some(expires),
        // Chrome: -1 unspecified, 0 none, 1 lax, 2 strict. CEF: 0 unspecified, 1 none, 2 lax, 3 strict.
        same_site: match r["samesite"].as_i64() { Some(0) => 1, Some(1) => 2, Some(2) => 3, _ => 0 },
        // Chrome: 0 low, 1 medium, 2 high. CEF: -1 low, 0 medium, 1 high.
        priority: (r["priority"].as_i64().unwrap_or(1).clamp(0, 2) - 1) as i32,
    }))
}

pub fn plan(rows: &[Value], key: &[u8; 16], domains: &[String]) -> Plan {
    let now = now_us();
    let mut plan = Plan::default();
    for r in rows {
        let host = r["host_key"].as_str().unwrap_or_default();
        if !wanted(host, domains) {
            plan.skipped_filtered += 1;
            continue;
        }
        if plan.cookies.len() >= MAX_COOKIES {
            plan.errors.push(format!("Only the first {MAX_COOKIES} cookies were imported."));
            break;
        }
        let version = r["db_version"].as_str().and_then(|v| v.parse().ok()).or(r["db_version"].as_i64()).unwrap_or(0);
        match row(key, r, now, version, &mut plan) {
            Ok(Some(c)) => plan.cookies.push(c),
            Ok(None) => {}
            // Names only; cookie values never appear in reports.
            Err(e) => plan.errors.push(format!("{host} {}: {e}", r["name"].as_str().unwrap_or_default())),
        }
    }
    plan
}

/// The "Chrome Safe Storage" password. Test instances (`ATRIUM_MOCK_KEYCHAIN=1`) never
/// touch the login keychain and use `ATRIUM_TEST_CHROME_SAFE_STORAGE` instead.
pub async fn safe_storage_password() -> Result<Vec<u8>> {
    if super::mock_keychain() {
        let test = std::env::var("ATRIUM_TEST_CHROME_SAFE_STORAGE").ok().filter(|_| std::env::var("ATRIUM_MOCK_KEYCHAIN").as_deref() == Ok("1"));
        return test.map(String::into_bytes).context("Test instances cannot read the login keychain. Set ATRIUM_TEST_CHROME_SAFE_STORAGE with ATRIUM_MOCK_KEYCHAIN=1.");
    }
    let output = tokio::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-w", "-s", "Chrome Safe Storage", "-a", "Chrome"])
        .kill_on_drop(true)
        .output()
        .await?;
    ensure!(output.status.success(), "Access to Chrome's cookie key was denied or it was not found.");
    let mut password = output.stdout;
    while password.last().is_some_and(|c| *c == b'\n') {
        password.pop();
    }
    ensure!(!password.is_empty(), "Chrome's cookie key is empty.");
    Ok(password)
}

pub fn database(profile: &Path) -> Option<std::path::PathBuf> {
    [profile.join("Network/Cookies"), profile.join("Cookies")].into_iter().find(|p| p.is_file())
}

pub const QUERY: &str = "SELECT host_key, name, value, hex(encrypted_value) AS ev, path, expires_utc, is_secure, is_httponly, has_expires, is_persistent, samesite, priority, creation_utc, last_access_utc, top_frame_site_key, (SELECT value FROM meta WHERE key = 'version') AS db_version FROM cookies";

/// Compares what CEF stored with what was written; returns (verified, mismatched names).
pub fn verify(written: &[Write], stored: &[Stored]) -> (usize, Vec<String>) {
    let mut ok = 0;
    let mut bad = Vec::new();
    for w in written {
        let domain = if w.domain.is_empty() { w.url.split("://").nth(1).and_then(|r| r.split('/').next()).unwrap_or_default().to_string() } else { w.domain.clone() };
        let found = stored.iter().find(|s| s.name == w.name && s.path == w.path && s.domain.eq_ignore_ascii_case(&domain));
        match found {
            Some(s) if s.value == w.value && s.secure == w.secure && s.httponly == w.httponly && s.same_site == w.same_site && s.expires.is_some() == w.expires.is_some() => ok += 1,
            _ => bad.push(format!("{domain} {}", w.name)),
        }
    }
    (ok, bad)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn encrypt(key: &[u8; 16], plain: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; plain.len() + 32];
        let mut moved = 0;
        let iv = [b' '; 16];
        let status = unsafe { cc::CCCrypt(0, 0, 1, key.as_ptr().cast(), 16, iv.as_ptr().cast(), plain.as_ptr().cast(), plain.len(), out.as_mut_ptr().cast(), out.len(), &mut moved) };
        assert_eq!(status, 0);
        out.truncate(moved);
        [b"v10".to_vec(), out].concat()
    }

    fn hexed(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02X}")).collect()
    }

    #[test]
    fn key_derivation_matches_chrome() {
        // PBKDF2-HMAC-SHA1("peanuts", "saltysalt", 1003, 16), computed independently with Python's hashlib.
        assert_eq!(hexed(&derive_key(b"peanuts").unwrap()), "D9A09D499B4E1B7461F28E67972C6DBD");
    }

    #[test]
    fn rows_map_attributes_and_skip_expired_partitioned_and_tampered_values() {
        let key = derive_key(b"test-key").unwrap();
        let now = now_us();
        let host_bound = |host: &str, v: &str| [sha256(host.as_bytes()).to_vec(), v.as_bytes().to_vec()].concat();
        let base = json!({ "path": "/", "is_secure": 1, "is_httponly": 1, "has_expires": 1, "is_persistent": 1,
            "samesite": 2, "priority": 2, "creation_utc": now - 10, "last_access_utc": now - 5, "top_frame_site_key": "",
            "db_version": "24", "value": "" });
        let make = |host: &str, name: &str, ev: Vec<u8>, extra: Value| {
            let mut r = base.clone();
            r["host_key"] = json!(host);
            r["name"] = json!(name);
            r["ev"] = json!(hexed(&ev));
            r["expires_utc"] = json!(now + 3_600_000_000);
            for (k, v) in extra.as_object().unwrap() { r[k] = v.clone(); }
            r
        };
        let rows = vec![
            make(".example.com", "sid", encrypt(&key, &host_bound(".example.com", "abc")), json!({})),
            make("example.com", "old", encrypt(&key, &host_bound("example.com", "x")), json!({ "expires_utc": now - 1 })),
            make("example.com", "part", encrypt(&key, &host_bound("example.com", "x")), json!({ "top_frame_site_key": "https://a.test" })),
            make("evil.com", "moved", encrypt(&key, &host_bound("example.com", "x")), json!({})),
            make("example.com", "session", vec![], json!({ "value": "plain", "has_expires": 0, "is_persistent": 0, "expires_utc": 0, "samesite": -1, "is_secure": 0 })),
            make("other.org", "filtered", vec![], json!({ "value": "v" })),
        ];
        let plan = plan(&rows, &key, &["example.com".into(), "evil.com".into()]);
        assert_eq!(plan.skipped_expired, 1);
        assert_eq!(plan.skipped_partitioned, 1);
        assert_eq!(plan.skipped_filtered, 1);
        assert_eq!(plan.errors.len(), 1, "{:?}", plan.errors);
        assert_eq!(plan.errors[0], "evil.com moved: value does not belong to this domain");
        let sid = &plan.cookies[0];
        assert_eq!((sid.value.as_str(), sid.domain.as_str(), sid.url.as_str()), ("abc", ".example.com", "https://example.com/"));
        assert!(sid.secure && sid.httponly && sid.expires.is_some());
        assert_eq!((sid.same_site, sid.priority), (3, 1));
        let session = &plan.cookies[1];
        assert_eq!((session.value.as_str(), session.domain.as_str(), session.url.as_str()), ("plain", "", "http://example.com/"));
        assert_eq!((session.expires, session.same_site), (None, 0));
        let wrong = derive_key(b"wrong").unwrap();
        assert!(decrypt(&wrong, &encrypt(&key, b"0123456789abcdef0123456789abcdefvalue"), "a", 24).is_err());
    }
}
