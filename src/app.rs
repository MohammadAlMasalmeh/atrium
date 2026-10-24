//! Browser engine selected at compile time. Chromium is built and bundled with CEF.
#[cfg(feature = "chromium")]
#[path = "app_cef.rs"]
mod engine;
#[cfg(not(feature = "chromium"))]
#[path = "app_wry.rs"]
mod engine;
pub use engine::*;
