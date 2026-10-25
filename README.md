# atrium-rs

> **Experimental developer browser.** This is a research prototype for developers,
> not a supported browser. Builds are ad hoc signed, not notarized, and never
> update themselves; the Chromium engine is only as current as the pinned CEF
> version. Do not use it for sensitive browsing. See [SECURITY.md](SECURITY.md).

<!-- TODO(maintainer): choose the project name and license before publishing. -->
| | |
| --- | --- |
| **Name** | **Atrium**, set in `src/brand.rs` (see [CONTRIBUTING.md](CONTRIBUTING.md#renaming-the-product)). It is not affiliated with the commercial Aside browser. |
| **License** | *Not yet chosen.* Until a `LICENSE` file is added, no license is granted. Third-party components keep their own licenses; see [THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt). |

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
./target/bundle/atrium.app/Contents/MacOS/atrium
```

Run the **bundled executable**, which includes Chromium's framework and sandboxed
helper applications. Running `target/release/atrium` directly cannot find that
runtime. The script stages and verifies an ad hoc signed app before replacing
`target/bundle/atrium.app`. This is not a notarized public release.

For an isolated profile and an existing OpenCode server:

```sh
./target/bundle/atrium.app/Contents/MacOS/atrium \
  --data-dir /tmp/atrium-test run --port 7782 \
  --opencode-url http://127.0.0.1:4097
```

Without an explicit server, the app reuses the saved endpoint or tries to start
`opencode serve` locally. Browsing opens independently of the assistant. OpenCode
must have a provider configured to answer prompts, and it forwards what the
assistant reads to that provider; see [PRIVACY.md](PRIVACY.md). Default browser
data is stored in `~/.atrium-rs`; Chromium cookies and storage live under
`chromium/Default`. Do not run two instances against the same profile.

Load an unpacked Chromium extension by adding `--extension /absolute/path/to/extension`
to `run`. A synthetic Manifest V3 content script and `chrome.storage` have been
verified. Chrome Web Store installation, extension management, and compatibility
with arbitrary extensions are unfinished.

The app menu's **About** item opens a page with the app, Chromium, and CEF
versions and every third-party notice, including the Chromium credits shipped in
the bundle.

## Infrastructure

- Sandboxed CEF subprocesses and a persistent Chromium profile.
- Saved tab metadata with lazy restoration: only the selected page loads at startup.
- Background tab eviction, with a six-page target and a five-minute idle timeout.
  Active/split pages, drafts, media checks, downloads, capture, and agent work can
  prevent eviction. Selecting an evicted tab reloads it.
- Limits background assistant reads to three concurrent operations, with page cleanup on cancellation.
- Browser automation through CEF's DevTools protocol: navigation, page reading,
  element actions, JavaScript evaluation, and screenshots.
- Loopback API with Host/Origin checks and a per-launch token (`x-atrium-token`,
  written to `<data dir>/api-token`) on every route except the shell and About
  pages; shell evaluation is disabled in release builds.
- Site icons come from the pages themselves and are cached locally; no
  third-party favicon service is used.
- This project adds no telemetry, crash reporting, or update checks. Chromium's
  component updater and network time service still contact Google; see
  [PRIVACY.md](PRIVACY.md).
- Backend APIs for explicit Chrome import (bookmarks with folders, history visits,
  last-session tabs, default search engine, and consented cookie import), an import
  report, extension inventory, and OpenCode connection. These have no new onboarding UI.

See [Chromium implementation and limitations](docs/chromium.md) for details,
verification commands, and the RAM benchmark methodology. This remains a
prototype: engine replacement and lifecycle coverage do not complete a shipping
browser's security, compatibility, and update responsibilities.

While the app is running, the terminal client can use its assistant (it reads the
API token from the data directory, or `ATRIUM_API_TOKEN`):

```sh
./target/bundle/atrium.app/Contents/MacOS/atrium ask "summarize this page"
```

The previous WKWebView backend remains available for development with
`cargo run --no-default-features --features webkit --bin atrium`; it is not the
default build and does not provide Chromium extensions.

## Project documents

- [PRIVACY.md](PRIVACY.md): what the code sends where, and what it stores.
- [SECURITY.md](SECURITY.md): reporting issues and the local API's trust model.
- [CONTRIBUTING.md](CONTRIBUTING.md): building, testing in background mode, style,
  license notices, and renaming.
- [THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt) and [docs/licenses](docs/licenses).
