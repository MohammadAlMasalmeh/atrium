//! Favicons provided by pages, cached as PNG under `<data dir>/favicons` and served to
//! the shell, so the sidebar never asks a third-party icon service about visited sites.
use axum::{
    extract::Path,
    http::header,
    response::{IntoResponse, Response},
};

pub const MAX_BYTES: usize = 256 * 1024;
const GENERIC: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#8e8e93" stroke-width="1.6"><circle cx="12" cy="12" r="9"/><path d="M3 12h18M12 3a14 14 0 0 1 0 18M12 3a14 14 0 0 0 0 18"/></svg>"##;

fn valid(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 253
        && !key.starts_with('.')
        && !key.contains("..")
        && key.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
}

/// Cache key for a page, matching the shell's `host()`: the lowercase host without `www.`.
pub fn key(page_url: &str) -> Option<String> {
    let url = reqwest::Url::parse(page_url).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    valid(host).then(|| host.to_string())
}

/// Icons are fetched only from the page's own declared URLs.
pub fn fetchable(icon_url: &str) -> bool {
    reqwest::Url::parse(icon_url).is_ok_and(|u| matches!(u.scheme(), "http" | "https" | "data"))
}

fn file(key: &str) -> Option<std::path::PathBuf> {
    if !valid(key) {
        return None;
    }
    Some(crate::data_dir().ok()?.join("favicons").join(format!("{key}.png")))
}

pub fn store(key: &str, png: &[u8]) -> std::io::Result<()> {
    let path = file(key).ok_or_else(|| std::io::Error::other("invalid favicon key"))?;
    if png.is_empty() || png.len() > MAX_BYTES {
        return Err(std::io::Error::other("favicon size out of range"));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temp = path.with_extension(format!("png.{}.tmp", uuid::Uuid::new_v4().simple()));
    std::fs::write(&temp, png)?;
    std::fs::rename(&temp, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

pub async fn serve(Path(key): Path<String>) -> Response {
    let cached = match file(&key) {
        Some(path) => tokio::fs::read(path).await.ok(),
        None => None,
    };
    let (content_type, body) = match cached {
        Some(png) => ("image/png", png),
        None => ("image/svg+xml", GENERIC.as_bytes().to_vec()),
    };
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keys_match_the_shell_and_cannot_escape_the_cache() {
        assert_eq!(key("https://www.Example.com/a?b").as_deref(), Some("example.com"));
        assert_eq!(key("http://127.0.0.1:7898/x").as_deref(), Some("127.0.0.1"));
        assert_eq!(key("https://xn--bcher-kva.example/").as_deref(), Some("xn--bcher-kva.example"));
        for url in ["about:blank", "file:///etc/passwd", "data:text/html,x", "http://[::1]/"] {
            assert_eq!(key(url), None, "{url}");
        }
        for bad in ["", "../x", ".hidden", "a/b", "a..b", "A.com", "a%2fb"] {
            assert!(!valid(bad), "{bad}");
        }
        assert!(fetchable("https://example.com/favicon.ico") && fetchable("data:image/png;base64,AA=="));
        assert!(!fetchable("file:///tmp/x.png") && !fetchable("chrome://favicon/x"));
    }
}
