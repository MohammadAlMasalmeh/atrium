# Atrium

> **Experimental developer browser.** This is a research prototype for developers,
> not a supported browser. Builds are ad hoc signed, not notarized, and never
> update themselves; the Chromium engine is only as current as the pinned CEF
> version. Do not use it for sensitive browsing. See [SECURITY.md](SECURITY.md).

| | |
| --- | --- |
| **Name** | **Atrium**, set in `src/brand.rs` (see [CONTRIBUTING.md](CONTRIBUTING.md#renaming-the-product)). It is not affiliated with the commercial Aside browser. |
| **License** | [Apache License 2.0](LICENSE). Contributions and forks are welcome. Third-party components keep their own licenses; see [THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt). |

## License

Atrium's original code is licensed under the [Apache License, Version 2.0](LICENSE).
You may use, modify, distribute, and sell it under those terms. Redistributions
must include the license and preserve the applicable notices, including the
attribution in [NOTICE](NOTICE); modified files must identify changes as required
by Section 4. See [CONTRIBUTING.md](CONTRIBUTING.md#contribution-license) for
contribution terms. Third-party components retain their own licenses.

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
| Startup (median of 3 launches) | 0.9 s | **0.6 s** | 10.8 s |
| First contentful paint (median) | 526 ms | **316 ms** | 400 ms |
| Load event (median) | 1,084 ms | **787 ms** | 1,108 ms |
| Switching back to an idle tab | 0.6–1.8 s (reloads) | **0.1 s** | **0.1 s** |

Where Atrium is behind:
- **First paint:** on real sites Chrome paints about 120–200 ms sooner (a
  repeated visible-window check: YouTube 476 vs 280 ms, Gmail 496 vs 328 ms,
  Wikipedia 296 vs 176 ms). A plain local page paints equally fast in both, and
  turning ad blocking off does not close the gap. Single loads vary widely (YouTube
  312–1,164 ms across three loads), and a Chromium trace of a cold YouTube load
  showed the time going to network waits and the site's own scripts, not to Atrium
  holding back the paint.
- **Startup:** Chrome starts about 0.3 s sooner. Atrium's figure runs until the
  shell and first tab are ready; Chrome's ends when its DevTools port opens.
- **Idle tabs:** Atrium keeps memory low by evicting idle tabs, so switching back
  reloads them. Resting the pointer on a sleeping tab starts that reload early,
  which cut the switch from 0.39 s to 0.12 s (median, background-window test).

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
- The command bar searches imported Chrome history and bookmarks as well as recent
  visits, ranked like an address bar: a site-name prefix first, then word and
  substring matches, ordered by visits and recency. A lookup over 7,500 imported
  pages takes about 2.5 ms.
- The address field also shows live search predictions from Google, or an imported
  Bing/DuckDuckGo default. Requests wait for a typing pause and use no profile
  cookies. `ATRIUM_SEARCH_SUGGESTIONS=0` keeps suggestions local.
- Built-in ad blocking: third-party requests to about 60 ad exchanges, ad servers,
  and ID-sync endpoints are refused, so ad frames never get their own renderer
  processes. Invisible full-page layers that popunder ads use to catch clicks are
  removed. Top-level navigations and analytics scripts are left alone.
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
