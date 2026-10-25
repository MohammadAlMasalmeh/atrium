//! About page with versions and every third-party notice. It holds no user data,
//! so it is served without the API token (Host checks still apply).
use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};

use crate::brand;

const NOTICES: &str = include_str!("../THIRD-PARTY-NOTICES.txt");
const CEF_LICENSE: &str = include_str!("../docs/licenses/CEF-LICENSE.txt");
const CHROMIUM_LICENSE: &str = include_str!("../docs/licenses/Chromium-LICENSE.txt");
pub const PATH: &str = "/about";
pub const CREDITS_PATH: &str = "/about/chromium-credits";
const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; img-src data:; \
                   frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

pub fn is_public(path: &str) -> bool {
    matches!(path, "/shell" | PATH | CREDITS_PATH)
}

/// (CEF, Chromium) versions this binary was built against.
#[cfg(feature = "chromium")]
pub fn engine_versions() -> Option<(String, String)> {
    let raw = std::ffi::CStr::from_bytes_with_nul(cef::sys::CEF_VERSION).ok()?.to_str().ok()?;
    let (cef, chromium) = raw.split_once("+chromium-")?;
    Some((cef.to_string(), chromium.to_string()))
}

#[cfg(not(feature = "chromium"))]
pub fn engine_versions() -> Option<(String, String)> {
    None
}

/// The Chromium credits shipped in the app bundle's Resources directory.
fn credits_file() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let path = exe.parent()?.parent()?.join("Resources/Chromium-CREDITS.html");
    path.is_file().then_some(path)
}

fn esc(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn secured(content_type: &'static str, body: impl IntoResponse) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CONTENT_SECURITY_POLICY, CSP),
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::HeaderName::from_static("cross-origin-resource-policy"), "same-origin"),
        ],
        body,
    )
        .into_response()
}

pub fn page_html() -> String {
    let name = esc(brand::PRODUCT_NAME);
    let license = if brand::PROJECT_LICENSE.is_empty() {
        "Not yet chosen".to_string()
    } else {
        esc(brand::PROJECT_LICENSE)
    };
    let engine = match engine_versions() {
        Some((cef, chromium)) => format!(
            r#"<dt>Chromium</dt><dd id="chromium-version">{}</dd><dt>CEF</dt><dd id="cef-version">{}</dd>"#,
            esc(&chromium),
            esc(&cef)
        ),
        None => r#"<dt>Engine</dt><dd id="engine">System WebKit (WKWebView)</dd>"#.into(),
    };
    let credits = if credits_file().is_some() {
        format!(r#"<a id="chromium-credits" href="{CREDITS_PATH}">Chromium component credits</a> (Chromium-CREDITS.html, the notices of every open-source project in Chromium)"#)
    } else {
        r#"<span id="chromium-credits-missing">Chromium-CREDITS.html is not in this build. Run the bundled app to view it.</span>"#.into()
    };
    format!(
        r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="color-scheme" content="light dark">
<title>About {name}</title>
<style>
body {{ font: 14px/1.5 -apple-system, BlinkMacSystemFont, sans-serif; max-width: 860px; margin: 40px auto; padding: 0 24px; }}
h1 {{ font-size: 26px; margin: 0 0 4px; }} h2 {{ font-size: 17px; margin: 32px 0 8px; }}
.sub {{ opacity: .7; margin: 0 0 24px; }}
dl {{ display: grid; grid-template-columns: max-content 1fr; gap: 4px 20px; }} dt {{ opacity: .7; }} dd {{ margin: 0; font-variant-numeric: tabular-nums; }}
pre {{ white-space: pre-wrap; font: 12px/1.45 ui-monospace, Menlo, monospace; padding: 14px; border-radius: 10px; background: color-mix(in srgb, currentColor 6%, transparent); max-height: 480px; overflow: auto; }}
a {{ color: #0a84ff; }}
</style></head>
<body>
<h1 id="product-name">{name}</h1>
<p class="sub">An experimental developer browser. Not a supported or security-maintained release.</p>
<dl>
<dt>Version</dt><dd id="app-version">{version}</dd>
{engine}
<dt>Bundle ID</dt><dd id="bundle-id">{bundle_id}</dd>
<dt>License</dt><dd id="project-license">{license}</dd>
</dl>
<h2>Third-party software</h2>
<p>{name} includes Chromium, the Chromium Embedded Framework, and the Rust crates listed below.</p>
<ul>
<li>{credits}</li>
<li><a href="#cef-license">Chromium Embedded Framework license</a></li>
<li><a href="#chromium-license">Chromium license</a></li>
<li><a href="#notices">Rust crates and adapted code</a></li>
</ul>
<h2 id="cef-license">Chromium Embedded Framework license</h2>
<pre>{cef_license}</pre>
<h2 id="chromium-license">Chromium license</h2>
<pre>{chromium_license}</pre>
<h2 id="notices">Third-party notices</h2>
<pre id="notices-text">{notices}</pre>
</body></html>"##,
        version = env!("CARGO_PKG_VERSION"),
        bundle_id = esc(brand::BUNDLE_ID),
        cef_license = esc(CEF_LICENSE),
        chromium_license = esc(CHROMIUM_LICENSE),
        notices = esc(NOTICES),
    )
}

pub async fn page() -> Response {
    secured("text/html; charset=utf-8", page_html())
}

pub async fn chromium_credits() -> Response {
    match credits_file() {
        Some(path) => match tokio::fs::read(path).await {
            Ok(bytes) => secured("text/html; charset=utf-8", bytes),
            Err(_) => StatusCode::NOT_FOUND.into_response(),
        },
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_shell_and_about_skip_the_token() {
        for path in ["/shell", "/about", "/about/chromium-credits"] {
            assert!(is_public(path), "{path}");
        }
        for path in ["/", "/about/", "/about/../api/state", "/api/state", "/favicon/example.com", "/frozen/main"] {
            assert!(!is_public(path), "{path}");
        }
    }
    #[test]
    fn page_lists_versions_and_every_notice() {
        let html = page_html();
        assert!(html.contains(&format!(r#"<dd id="app-version">{}</dd>"#, env!("CARGO_PKG_VERSION"))));
        #[cfg(feature = "chromium")]
        {
            let (cef, chromium) = engine_versions().unwrap();
            assert!(cef.split('.').count() >= 3 && chromium.split('.').count() == 4, "{cef} {chromium}");
            assert!(html.contains(&chromium) && html.contains(&cef));
        }
        assert!(html.contains("option-ext") && html.contains("webpki-roots"));
        assert!(html.contains(&esc(CEF_LICENSE.lines().next().unwrap())));
        assert!(!html.contains("<script"));
    }
}
