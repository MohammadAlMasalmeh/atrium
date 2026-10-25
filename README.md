# Atrium

> **Experimental developer browser.** This is a research prototype for developers,
> not a supported browser. Builds are ad hoc signed, not notarized, and never
> update themselves; the Chromium engine is only as current as the pinned CEF
> version. Do not use it for sensitive browsing. See [SECURITY.md](SECURITY.md).

<!-- TODO(maintainer): choose the license before publishing. -->
| | |
| --- | --- |
| **Name** | **Atrium**, set in `src/brand.rs` (see [CONTRIBUTING.md](CONTRIBUTING.md#renaming-the-product)). It is not affiliated with the commercial Aside browser. |
| **License** | *Not yet chosen.* Until a `LICENSE` file is added, no license is granted. Third-party components keep their own licenses; see [THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt). |

Atrium is a macOS browser written in Rust, using Chromium Embedded Framework (CEF) for
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
./target/bundle/Atrium.app/Contents/MacOS/atrium
```

Run the **bundled executable**, which includes Chromium's framework and sandboxed
helper applications. Running `target/release/atrium` directly cannot find that
runtime. The script stages and verifies an ad hoc signed app before replacing
`target/bundle/Atrium.app`. This is not a notarized public release.

For an isolated profile and an existing OpenCode server:

```sh
./target/bundle/Atrium.app/Contents/MacOS/atrium \
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

## Performance

Measured on an Apple Silicon Mac with fresh profiles. Memory is the physical
footprint of the browser's whole process tree (what Activity Monitor calls Memory),
including the WebKit processes behind Atrium's shell.

**Built-in ad blocking** on six ad-heavy sites (BBC News, CNN, Forbes, weather.com,
Stack Overflow, Daily Mail), all open at once, median of two runs
(`tools/bench_content_blocking.py`):

| | Blocking off | Blocking on | Change |
| --- | --- | --- | --- |
| Memory | 4,775 MiB | 1,775 MiB | **−63%** |
| Processes | 74 | 18.5 | **−75%** |
| Page load (median) | 1,406 ms | 832 ms | **−41%** |
| First contentful paint (median) | 480 ms | 328 ms | **−32%** |

**Against Google Chrome**, ten popular sites opened one at a time in a visible
window (`tools/bench_three_way.py`, Chrome 155; Atrium two runs, Chrome one):

| | Atrium | Chrome |
| --- | --- | --- |
| Memory, no tabs | **181 MiB** | 628 MiB |
| Memory, 10 tabs after 6 min idle | **2,418 MiB** | 2,904 MiB |
| Memory, 10 tabs live | **1,222 MiB** | 3,272 MiB |
| Startup | 3.3 s | **1.2 s** |
| Switching back to an idle tab | reloads (evicted) | **instant** |

The 10-live-tab Atrium figure is from the current build with ad blocking, in a
background window 60 s after the last page loaded (one run, 16 processes,
`docs/benchmarks/2026-10-25-ten-sites-current.json`). Before ad blocking it was
4,990 MiB. Chrome's figures are from the earlier windowed run.

Where Atrium is behind:
- **Startup:** most of the time goes to showing the visible window. The shell is
  ready 0.8 s after launch in a background window.
- **Idle tabs:** Atrium frees memory by evicting idle tabs, so they reload when
  selected again.

Page timings against Chrome are not compared here, because on some sites Chrome
received a lighter page (Reddit: 44 DOM nodes in Chrome against 3,481 in Atrium).
Methodology and raw results are in [docs/chromium.md](docs/chromium.md) and
[docs/benchmarks](docs/benchmarks).

## Infrastructure

- Sandboxed CEF subprocesses and a persistent Chromium profile.
- Saved tab metadata with lazy restoration: only the selected page loads at startup.
- Background tab eviction, with a six-page target and a five-minute idle timeout.
  Active/split pages, drafts, media checks, downloads, capture, and agent work can
  prevent eviction. Selecting an evicted tab reloads it.
- Built-in ad blocking: third-party requests to about 60 ad exchanges, ad servers,
  and ID-sync endpoints are refused, so ad frames never get their own renderer
  processes. Top-level navigations and analytics scripts are left alone.
  `ATRIUM_CONTENT_BLOCKING=0` turns it off.
- Limits background assistant reads to three concurrent operations, with page cleanup on cancellation.
- Browser automation through CEF's DevTools protocol: navigation, page reading,
  element actions, JavaScript evaluation, and screenshots.
- Loopback API with Host/Origin checks and a per-launch token (`x-atrium-token`,
  written to `<data dir>/api-token`) on every route except the shell and About
  pages; shell evaluation is disabled in release builds.
- Site icons come from the pages themselves and are cached locally; no
  third-party favicon service is used.
- This project adds no telemetry or crash report uploads (crash dumps stay local).
  Only release builds with an embedded update key check GitHub for updates. Chromium's
  component updater and network time service still contact Google; see
  [PRIVACY.md](PRIVACY.md).
- Backend APIs for explicit Chrome import (bookmarks with folders, history visits,
  last-session tabs, default search engine, and consented cookie import), an import
  report, extension inventory, and OpenCode connection.
- First-run setup (assets/onboarding.js): welcome, Chrome bookmark/history
  import, and assistant setup that installs OpenCode if needed, starts it, and
  connects the browser tools. Reopen it from the command bar with "Set Up Atrium".

See [Chromium implementation and limitations](docs/chromium.md) for details,
verification commands, and the RAM benchmark methodology. This remains a
prototype: engine replacement and lifecycle coverage do not complete a shipping
browser's security, compatibility, and update responsibilities.

While the app is running, the terminal client can use its assistant (it reads the
API token from the data directory, or `ATRIUM_API_TOKEN`):

```sh
./target/bundle/Atrium.app/Contents/MacOS/atrium ask "summarize this page"
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
