//! Limits for the shell's command channel. The shell runs in WebKit, where
//! `window.ipc.postMessage(string)` reaches native.rs in order. The renderer helper's
//! CEF process-message path stays for any Chromium-hosted shell, and the browser
//! rejects such messages from website renderers.

/// Process message name carrying one JSON command as its only string argument.
pub const MESSAGE: &str = "atrium.shell.command";
/// Browser extra-info key that marks the shell browser; its value is the shell URL.
#[allow(dead_code)] // Read by atrium_helper only.
pub const EXTRA_INFO_KEY: &str = "atrium_shell_url";
pub const MAX_MESSAGE_BYTES: usize = 256 * 1024;
/// Process message from a website frame: media there that this Chromium build can't play.
/// Its only argument is the media URL. See assets/media.js.
pub const MEDIA_MESSAGE: &str = "atrium.media.unsupported";

/// The shell document itself, not another path or origin that merely starts the same way.
pub fn is_shell_url(url: &str, shell: &str) -> bool {
    !shell.is_empty()
        && url
            .strip_prefix(shell)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('?') || rest.starts_with('#'))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_the_exact_shell_document_matches() {
        let shell = "http://127.0.0.1:7777/shell";
        assert!(is_shell_url(shell, shell));
        assert!(is_shell_url("http://127.0.0.1:7777/shell#x", shell));
        for url in [
            "http://127.0.0.1:7777/shellx",
            "http://127.0.0.1:7777/shell/evil",
            "http://127.0.0.1:77770/shell",
            "http://127.0.0.1:7777/api/state",
            "http://127.0.0.1:7778/shell",
            "",
        ] {
            assert!(!is_shell_url(url, shell), "{url}");
        }
        assert!(!is_shell_url("", ""));
    }
}
