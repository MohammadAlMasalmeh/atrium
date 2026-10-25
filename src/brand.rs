//! Product identity. Renaming the browser changes `PRODUCT_NAME` and `BUNDLE_ID`
//! here; CONTRIBUTING.md lists the identifiers that are deliberately kept stable.

/// Shown in the window title, app menu, About page, and Info.plist.
pub const PRODUCT_NAME: &str = "Atrium";
/// macOS bundle identifier (CFBundleIdentifier).
pub const BUNDLE_ID: &str = "dev.atrium-rs.browser";
/// Must match the `[[bin]]` name in Cargo.toml; the bundle is `<EXECUTABLE>.app`.
pub const EXECUTABLE: &str = "atrium";
/// Default profile directory under the home directory. Changing it orphans existing profiles.
pub const DATA_DIR: &str = ".atrium-rs";
/// The project's own license (SPDX expression), shown on the About page. Empty until chosen.
pub const PROJECT_LICENSE: &str = "";
