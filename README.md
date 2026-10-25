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

**Against Google Chrome and Aside**, ten popular sites opened one at a time, each
in a new foreground tab of a visible window, one run each on 2026-10-25
(`tools/bench_three_way.py`, Chrome 155, Aside on Chromium 154):

| | Atrium | Chrome | Aside |
| --- | --- | --- | --- |
| Memory, no tabs | **261 MiB** | 582 MiB | 1,425 MiB |
| Memory, 10 tabs live (60 s after the last load) | **1,351 MiB** | 4,656 MiB | 6,315 MiB |
| Memory, 10 tabs after 6 min idle | **1,046 MiB** | 3,189 MiB | 4,094 MiB |
| Processes, 10 tabs live | **20** | 92 | 93 |
| Startup | 2.0 s | **1.0 s** | 2.8 s |
| First contentful paint (median) | 526 ms | **316 ms** | 400 ms |
| Load event (median) | 1,084 ms | **787 ms** | 1,108 ms |
| Switching back to an idle tab | 0.6–1.8 s (reloads) | **0.1 s** | **0.1 s** |

Where Atrium is behind:
- **First paint and load:** on the same pages Chrome paints first. Atrium also
  loads fuller pages on some sites (see below).
- **Startup:** Chrome is up in half the time. Atrium's figure runs until the shell
  and first tab are ready; Chrome's ends when its DevTools port opens.
- **Idle tabs:** Atrium keeps memory low by evicting idle tabs, so switching back
  reloads them.

Page timings are medians over the eight sites where all three browsers received
the same page. On Google Search and Reddit, Chrome and Aside were served a
lightweight page (49 and 44 DOM nodes) while Atrium got the full one (4,624 and
3,253), so those two sites are left out. Memory is the physical footprint of the
browser's process tree, including Atrium's shell WebKit processes. Raw results are
in [docs/benchmarks/2026-10-25-three-way-current.json](docs/benchmarks/2026-10-25-three-way-current.json);
methodology is in [docs/chromium.md](docs/chromium.md).

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
