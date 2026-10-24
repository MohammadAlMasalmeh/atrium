//! Sandboxed CEF subprocess entry point. See cef-rs's MIT/Apache-2.0 cefsimple helper.
use cef::{args::Args, *};
fn main() {
    let args = Args::new();
    #[cfg(target_os = "macos")]
    let _sandbox = {
        let mut sandbox = cef::sandbox::Sandbox::new();
        sandbox.initialize(args.as_main_args());
        sandbox
    };
    #[cfg(target_os = "macos")]
    let _loader = {
        let loader = library_loader::LibraryLoader::new(
            &std::env::current_exe().expect("helper path"),
            true,
        );
        assert!(
            loader.load(),
            "Could not load the bundled Chromium framework"
        );
        loader
    };
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    execute_process(
        Some(args.as_main_args()),
        None::<&mut App>,
        std::ptr::null_mut(),
    );
}
