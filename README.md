# aside-rs

A macOS browser written in Rust, using Chromium Embedded Framework (CEF) for
websites and a local OpenCode server for the assistant. Chromium is the default
engine. The original shell is retained; this work focuses on browser infrastructure.

## Build and run

Requires macOS, Rust, Xcode command-line tools, CMake, and Ninja. The first build
downloads the pinned CEF runtime, so allow time and disk space. This development
build has been exercised on Apple Silicon.

```sh
# If Ninja is not already installed:
python3 -m pip install --target target/build-tools ninja

./tools/build-chromium.sh --release
./target/bundle/aside.app/Contents/MacOS/aside
```

Run the **bundled executable**, which includes Chromium's framework and sandboxed
helper applications. Running `target/release/aside` directly cannot find that
runtime. The script stages and verifies an ad hoc signed app before replacing
`target/bundle/aside.app`. This is not a notarized public release.

For an isolated profile and an existing OpenCode server:

```sh
./target/bundle/aside.app/Contents/MacOS/aside \
  --data-dir /tmp/aside-test run --port 7782 \
  --opencode-url http://127.0.0.1:4097
```

Without an explicit server, the app reuses the saved endpoint or tries to start
`opencode serve` locally. Browsing opens independently of the assistant. OpenCode
must have a provider configured to answer prompts. Default browser data is stored
in `~/.aside-rs`; Chromium cookies and storage live under `chromium/profile`.
Do not run two instances against the same profile.

Load an unpacked Chromium extension by adding `--extension /absolute/path/to/extension`
to `run`. A synthetic Manifest V3 content script and `chrome.storage` have been
verified. Chrome Web Store installation, extension management, and compatibility
with arbitrary extensions are unfinished.

## Infrastructure

- Sandboxed CEF subprocesses and a persistent Chromium profile.
- Saved tab metadata with lazy restoration: only the selected page loads at startup.
- Background tab eviction, with a six-page target and a five-minute idle timeout.
  Active/split pages, drafts, media checks, downloads, capture, and agent work can
  prevent eviction. Selecting an evicted tab reloads it.
- Limits background assistant reads to three concurrent operations, with page cleanup on cancellation.
- Browser automation through CEF's DevTools protocol: navigation, page reading,
  element actions, JavaScript evaluation, and screenshots.
- Loopback API with Host/Origin checks; shell evaluation is disabled in release builds.
- Backend APIs for explicit Chrome import (bookmarks with folders, history visits,
  last-session tabs, default search engine, and consented cookie import), an import
  report, extension inventory, and OpenCode connection. These have no new onboarding UI.

See [Chromium implementation and limitations](docs/chromium.md) for details,
verification commands, and the RAM benchmark methodology. This remains a
prototype: engine replacement and lifecycle coverage do not complete a shipping
browser's security, compatibility, and update responsibilities.

While the app is running, the terminal client can use its assistant:

```sh
./target/bundle/aside.app/Contents/MacOS/aside ask "summarize this page"
```

The previous WKWebView backend remains available for development with
`cargo run --no-default-features --features webkit --bin aside`; it is not the
default build and does not provide Chromium extensions.
