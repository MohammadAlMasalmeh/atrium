//! Sandboxed CEF subprocess entry point. See cef-rs's MIT/Apache-2.0 cefsimple helper.
//! In renderer processes it exposes `window.ipc` to the Atrium shell's main frame only,
//! and watches website frames for media this Chromium build can't play.
use cef::{args::Args, *};
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};
#[path = "../chromium/shell_ipc.rs"]
mod shell_ipc;

const MEDIA_JS: &str = include_str!("../../assets/media.js");

/// Shell browsers hosted by this renderer process: browser id to shell URL. Only the
/// browser process can mark a browser as the shell, through its creation extra info.
static SHELLS: LazyLock<Mutex<HashMap<i32, String>>> = LazyLock::new(Default::default);

fn is_shell_frame(frame: &Frame) -> bool {
    if frame.is_main() == 0 {
        return false;
    }
    let Some(browser) = frame.browser() else { return false };
    let url = CefString::from(&frame.url()).to_string();
    SHELLS
        .lock()
        .unwrap()
        .get(&browser.identifier())
        .is_some_and(|shell| shell_ipc::is_shell_url(&url, shell))
}

wrap_app! {
    struct HelperApp;
    impl App {
        fn render_process_handler(&self) -> Option<RenderProcessHandler> {
            Some(ShellRenderer::new())
        }
    }
}

wrap_render_process_handler! {
    struct ShellRenderer;
    impl RenderProcessHandler {
        fn on_browser_created(&self, browser: Option<&mut Browser>, extra_info: Option<&mut DictionaryValue>) {
            let (Some(browser), Some(info)) = (browser, extra_info) else { return };
            let key = CefString::from(shell_ipc::EXTRA_INFO_KEY);
            if info.has_key(Some(&key)) != 0 {
                let url = CefString::from(&info.string(Some(&key))).to_string();
                SHELLS.lock().unwrap().insert(browser.identifier(), url);
            }
        }
        fn on_browser_destroyed(&self, browser: Option<&mut Browser>) {
            if let Some(browser) = browser {
                SHELLS.lock().unwrap().remove(&browser.identifier());
            }
        }
        fn on_context_created(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>, context: Option<&mut V8Context>) {
            let (Some(frame), Some(context)) = (frame, context) else { return };
            // The page's main world only; extension content scripts run in other worlds.
            let Some(mut main) = frame.v8_context() else { return };
            if context.is_same(Some(&mut main)) == 0 {
                return;
            }
            if !is_shell_frame(frame) {
                watch_media(context);
                return;
            }
            let Some(global) = context.global() else { return };
            let mut handler = IpcHandler::new();
            let (Some(mut post), Some(mut ipc)) = (
                v8_value_create_function(Some(&CefString::from("postMessage")), Some(&mut handler)),
                v8_value_create_object(None, None),
            ) else {
                return;
            };
            let fixed = V8Propertyattribute::from(sys::cef_v8_propertyattribute_t(
                sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_READONLY.0
                    | sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_DONTDELETE.0,
            ));
            ipc.set_value_bykey(Some(&CefString::from("postMessage")), Some(&mut post), fixed);
            global.set_value_bykey(Some(&CefString::from("ipc")), Some(&mut ipc), fixed);
        }
    }
}

/// Runs assets/media.js with a native `report`, so the page never sees the callback.
fn watch_media(context: &mut V8Context) {
    let mut handler = MediaHandler::new();
    let Some(report) = v8_value_create_function(Some(&CefString::from("report")), Some(&mut handler)) else { return };
    context.enter();
    let (mut watch, mut exception) = (None, None);
    if context.eval(Some(&CefString::from(MEDIA_JS)), None, 0, Some(&mut watch), Some(&mut exception)) != 0 {
        if let Some(watch) = watch.filter(|w| w.is_function() != 0) {
            watch.execute_function(None, Some(&[Some(report)]));
        }
    } else if let Some(e) = exception {
        eprintln!("[media] watcher failed: {}", CefString::from(&e.message()));
    }
    context.exit();
}

wrap_v8_handler! {
    struct MediaHandler;
    impl V8Handler {
        fn execute(
            &self,
            _name: Option<&CefString>,
            _object: Option<&mut V8Value>,
            arguments: Option<&[Option<V8Value>]>,
            _retval: Option<&mut Option<V8Value>>,
            _exception: Option<&mut CefString>,
        ) -> i32 {
            let Some(frame) = v8_context_get_current_context().and_then(|c| c.frame()) else { return 1 };
            let src = match arguments {
                Some([Some(value), ..]) if value.is_string() != 0 => CefString::from(&value.string_value()).to_string(),
                _ => String::new(),
            };
            let Some(mut message) = process_message_create(Some(&CefString::from(shell_ipc::MEDIA_MESSAGE))) else { return 1 };
            if let Some(args) = message.argument_list() {
                args.set_string(0, Some(&CefString::from(src.chars().take(300).collect::<String>().as_str())));
            }
            frame.send_process_message(ProcessId::BROWSER, Some(&mut message));
            1
        }
    }
}

wrap_v8_handler! {
    struct IpcHandler;
    impl V8Handler {
        fn execute(
            &self,
            _name: Option<&CefString>,
            _object: Option<&mut V8Value>,
            arguments: Option<&[Option<V8Value>]>,
            _retval: Option<&mut Option<V8Value>>,
            exception: Option<&mut CefString>,
        ) -> i32 {
            let fail = |exception: Option<&mut CefString>, message: &str| {
                if let Some(exception) = exception {
                    exception.try_set(message);
                }
                1
            };
            let Some(frame) = v8_context_get_current_context().and_then(|c| c.frame()) else {
                return fail(exception, "window.ipc has no frame");
            };
            if !is_shell_frame(&frame) {
                return fail(exception, "window.ipc is only available to the Atrium shell");
            }
            let body = match arguments {
                Some([Some(value), ..]) if value.is_string() != 0 => CefString::from(&value.string_value()).to_string(),
                _ => return fail(exception, "window.ipc.postMessage expects a string"),
            };
            if body.len() > shell_ipc::MAX_MESSAGE_BYTES {
                return fail(exception, "window.ipc message is too large");
            }
            let Some(mut message) = process_message_create(Some(&CefString::from(shell_ipc::MESSAGE))) else {
                return fail(exception, "window.ipc could not create a message");
            };
            if let Some(args) = message.argument_list() {
                args.set_string(0, Some(&CefString::from(body.as_str())));
            }
            frame.send_process_message(ProcessId::BROWSER, Some(&mut message));
            1
        }
    }
}

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
    let mut app = HelperApp::new();
    execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    );
}
