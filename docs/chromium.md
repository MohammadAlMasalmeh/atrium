# Chromium infrastructure

## Engine and process ownership

The default Cargo feature is `chromium`, using the pinned `cef` Rust bindings
(156.1.0 / CEF 156.0.8). The window belongs to the app, not CEF: a transparent
NSWindow with one Liquid Glass surface (`NSGlassEffectView`, tinted with
`windowBackgroundColor` at 0.55) behind everything. The shell UI runs in a
transparent WKWebView over the glass, and each Chromium tab is a windowed
(Alloy) browser whose NSView is a child of the window's content view, placed
over the shell's content area. CEF can't draw see-through windows, so Chromium
only paints the opaque pages. Websites and helpers use Chromium; there is no
Node.js runtime exposed to page JavaScript. The renderer sandbox stays enabled
and no remote DevTools port is opened by the browser.

The Rust process owns the tab registry and communicates with CEF on its UI thread
through queued tasks. OpenCode startup runs independently of the window. Only an
OpenCode child started by this app is stopped on exit. An existing server is reused.
The shell calls a loopback Axum server. Requests must use the expected Host and,
when present, the shell's Origin. Every route except `/shell` also requires a
per-launch secret, sent as the `x-atrium-token` header or the `atrium_token`
cookie. The browser writes it to `<data dir>/api-token` (mode 0600) at startup
and removes it on exit. WebKit can't add headers to EventSource or image loads,
so the shell starts at `/session?t=<token>`, which sets the token as an HttpOnly,
SameSite=Strict cookie for the loopback origin and redirects to `/shell`. The
shell's WebKit storage is separate from Chromium's, so website tabs never carry
the cookie, and it is kept per data directory. `atrium
mcp` and `atrium ask` read the token from the data directory or `ATRIUM_API_TOKEN`.
Any process running as the same user can still read that file, so this stops
websites and other users rather than hostile local software.

### Native browser chrome (in progress, `ATRIUM_NATIVE_CHROME=1`)

The browser controls are moving from the web shell to real AppKit controls on
the window glass, one piece at a time behind this flag. The web shell remains
the default until the native version covers everything. Done so far:

- Toolbar: sidebar toggle, back, forward and reload are `NSButton`s with SF
  Symbols; back and forward follow Chromium's `can_go_back`/`can_go_forward`;
  a small `NSProgressIndicator` spins while the active tab loads.
- Address field: an `NSTextField` that shows the host and switches to the full
  address when focused. Enter loads in the current tab on a web page or opens
  a new tab from the start page or chat (as the shell's command bar does), and
  Escape cancels. Copy Link and Pin buttons sit inside it on web pages and call
  the same shell commands as ⇧⌘C and ⌘D.
- The shell hides only the controls it no longer draws, keeping its layout,
  and reports whether its sidebar is shown and whether a full overlay (command
  bar, onboarding) is open. The native controls hide in both cases, because
  they sit above the shell.

Still web-drawn: the tab list, spaces, pinned tabs and favorites, the start
page search box, and (by design) chat and onboarding. ⌘L still opens the
command bar. Not yet verified with real input: clicking, typing with an input
method, dragging the window from the toolbar's empty area, and VoiceOver.
`tools/test_native_chrome.py` drives the real controls through a test-only hook
(`nativeChrome` command, background mode only) and checks the results.

### Layout stability

Tab views are child NSViews of the window's content view, and only `layout()`
moves or shows them. New views start hidden until their first placement, and a
closed tab's view is removed from the window before CEF frees it, so pages are
never drawn at the wrong place or after close. The shell's IPC is one `fetch`
per message, so layout messages are kept to one in flight (the newest rect
wins) to prevent stale rects being applied out of order. Sidebar lists replace
only rows whose markup changed, so state pushes from any tab don't re-create
favicons or restart spinners. Screenshots of hidden tabs, including the
assistant preview, keep the tab painted but positioned outside the window
rather than drawn over the visible page. Tab activation no longer calls
`window.activate()`, so agent or API tab switches don't pull the app in front
of other applications.

### Keychain prompts

Chromium stores its cookie encryption key as "Chromium Safe Storage" in the
login keychain. Ad hoc signatures change on every rebuild, so macOS asks again
after each build. Set `ATRIUM_MOCK_KEYCHAIN=1` for throwaway test and benchmark
profiles to use Chromium's mock keychain instead; never use it for a real
profile. A stable signing identity is required to stop the prompt for
development builds of a real profile.

`tools/build-chromium.sh --release` creates a complete development app with the
CEF framework, sandbox helper executables, Chromium credits, and verified ad hoc
signatures. CEF updates require changing the pin, rebuilding, and re-running the
integration checks. There is no automatic updater or notarized distribution yet.

## Memory behavior

`src/memory.rs` chooses the oldest eligible background pages. The default target
is six live content views, excluding the shell and Glance. It is a soft limit:
loading, recent activity (30 seconds), visible split panes, media capture,
downloads, and active agent work protect a page. A page-side probe protects dirty
forms, `beforeunload` handlers, and detected playing media. Rejected probes are
backed off for three minutes. Eligible pages idle for five minutes are released
even below the six-page target. A ten-second maintenance task applies the policy.

Eviction closes the page's BrowserView while retaining its tab metadata. Selecting
it creates a fresh view at the saved URL. Cookies and origin storage persist in
Chromium's profile. In-memory application state, full back/forward history, and
scroll position are not restored after eviction. Media protection is conservative
but does not guarantee detection inside every cross-origin frame. Protected tabs
can exceed the target; the app does not discard unsaved forms to meet a RAM claim.

Session metadata is written with a temporary file and rename. Restart restores
logical tabs and loads only the active page. Assistant scratch pages are omitted
and bounded to three simultaneous pages, with cancellation cleanup.

The `/api/browser/memory` endpoint reports logical tabs and live views.
`live_renderers` is the legacy JSON field name for **BrowserViews**, not a count
of operating-system renderer processes. Use a process-tree measurement for RAM.

## Verification

Run the fixture server and an isolated bundled browser in separate terminals:

```sh
python3 tools/fixture_server.py
./target/bundle/atrium.app/Contents/MacOS/atrium \
  --data-dir /tmp/atrium-smoke run --port 7782 \
  --opencode-url http://127.0.0.1:4097
python3 tools/smoke_chromium.py
python3 tools/test_tab_lifecycle.py
python3 tools/test_profile_persistence.py
```

The smoke check covers navigation, page interaction, JavaScript isolation,
screenshots, and rejection of a foreign Origin. With an appropriate synthetic
extension loaded, `--expect-extension` also checks its content script. The lifecycle
check opens clean and dirty pages, waits for eviction, checks draft retention,
and wakes a sleeping page. Rust unit tests cover eviction selection, migration
parsing, path validation, and endpoint validation:

```sh
CEF_PATH="$PWD/target/cef" PATH="$PWD/target/build-tools/bin:$PATH" cargo test --locked
```

The app-owned window and the native chrome have their own end-to-end checks.
Each starts its own instances (ports 7900 and 7909, fresh data directories under
`/private/tmp/atrium-native-*`) in background mode, which never activates or
shows the window, and stops only what it started:

```sh
python3 tools/spike_native_window.py
python3 tools/test_native_chrome.py --screenshots /private/tmp/native-chrome/shots
```

`--screenshots` captures the window in light and dark mode (`ATRIUM_APPEARANCE`).
For those two runs the window sits on screen behind every other window, because
glass needs something behind it to render; it still can't activate or take clicks.

For a repeatable RAM comparison, start a **fresh, empty**, extension-free release
profile on port 7783 and keep the fixture server on 7789. Then run:

```sh
python3 tools/compare_memory.py --output /tmp/atrium-memory.json
```

The script starts the installed `/Applications/Aside.app` with a separate temporary
profile, verifies both browsers opened the same ten local pages, waits for the
lifecycle policy, and samples both process trees six times. `--distinct-sites`
uses ten separate `.localhost` hosts. Each page touches a 12 MiB buffer and has
identical simple controls. The metric is median aggregate descendant-process RSS;
shared memory may be counted multiple times. It is not physical footprint, a
real-world browsing benchmark, or proof that every workload uses less memory.
The shared external OpenCode server is outside both process trees. Compare the
same accounts, extensions, page set, build profile, and warm-up interval.

### Recorded comparison: October 24, 2026

Apple Silicon macOS; release build; fresh profiles; Aside 1.0.1022.1
(Chromium 154.0.8037.100), this build CEF 156.0.8 (Chromium 156.0.8078.48).
Six samples after a 50-second warm-up, with ten fixture tabs in each browser.

| Workload | Aside RSS | This build RSS | Reduction |
| --- | ---: | ---: | ---: |
| [Ten same-origin pages](benchmarks/2026-10-24-same-origin.json) | 2,785.51 MiB | 1,195.99 MiB | 57.1% |
| [Ten separate localhost sites](benchmarks/2026-10-24-distinct-sites.json) | 2,843.91 MiB | 1,135.04 MiB | 60.1% |

This build retained six live content views plus its shell and released four
background pages. They remain in the tab list and reload when selected. These
numbers include that behavior and the different browser shells; they do not
isolate a per-renderer efficiency improvement. Baseline RSS varied during
sampling, so the raw samples and process counts are included in each report.
No claim is made about arbitrary websites, active workloads, battery use, or
physical memory footprint.

### Real public websites

`tools/real_sites.json` defines Google Search, YouTube, Gmail's signed-out page,
GitHub, Wikipedia, BBC News, MDN, Stack Overflow, Reddit, and Hacker News. Start
a fresh release profile on port 7783, then run:

```sh
python3 tools/compare_real_sites.py --output /tmp/atrium-real-sites.json
```

This requires Node with its built-in WebSocket API. The script opens each site in
both browsers, brings tabs into view, and verifies their rendered documents.
Challenge pages, errors, and incomplete loads are excluded from both sides and
recorded in the report. It samples once while normal agent protection keeps all
accepted pages live, and again after that protection expires and normal eviction
can run. Public websites may vary by time, region, cookies, viewport, and browser
version. Signed-in Gmail and user documents are outside this test.

Alongside RSS it records the sum of macOS per-process physical-footprint counters
(`proc_pid_rusage`, `rusage_info_v2.ri_phys_footprint`) over each browser's process
tree. Samples with incomplete process coverage are null. At least four complete
samples are required to publish a footprint median. These counters are a more
useful RAM signal than RSS alone, but are still process accounting rather than
an isolated measurement of total machine memory consumption.

### Recorded real-site run: October 24, 2026 (single run, preliminary)

[Report](benchmarks/2026-10-24-real-sites-strict.json) and
[validation](benchmarks/2026-10-24-real-sites-strict.validation.json). Same
builds and machine as above, fresh signed-out profiles, assistant disconnected.
Eight sites matched in both browsers; Google Search (baseline redirected) and
Reddit (baseline challenge page) were excluded from both.

| Phase | Aside footprint | This build footprint | Reduction | RSS reduction |
| --- | ---: | ---: | ---: | ---: |
| All accepted pages live (9 CEF views incl. shell) | 4,297 MiB | 3,329 MiB | 22.5% | 41.6% |
| After eviction (7 CEF views) | 3,339 MiB | 2,809 MiB | 15.9% | 41.7% |

This is **one** run. An earlier, unrecorded real-site run showed this build
using *more* footprint with all pages live, and a replicate was interrupted,
so treat these figures as unconfirmed and do not publish them. The browsers
also differ (Aside 1.0.1022.1 on Chromium 154 with its own built-in features
against a minimal CEF 156 shell), so the result is not a per-engine
comparison. Process counts were 89 against 56 when live.

Profiling lead: with eight pages live, this build ran roughly 45 renderer
processes, most idle at about 38 MiB RSS each. That pattern matches
cross-site subframes (ads and embeds) receiving their own process under site
isolation. Its process count also stayed at 56 after two pages were evicted,
which should be checked for renderers outliving their closed BrowserViews.
Run benchmark instances with `ATRIUM_MOCK_KEYCHAIN=1`; the scripts in `tools/`
must send the API token before they work against current builds.

## Migration and extensions

Backend setup routes remain available under `/api/setup`. They discover Chrome
profile names, explicitly import HTTP(S) bookmarks and optional recent history
read-only, and inventory installed extension names. They do not copy passwords,
cookies, passkeys, or Google sessions. OpenCode connection checks validate the
endpoint and browser tool registration before changing the active connection.
Only the endpoint address is saved; server credentials remain in memory or come
from `OPENCODE_SERVER_USERNAME` / `OPENCODE_SERVER_PASSWORD`.

`GET /api/browser/extensions` summarizes each loaded extension's declared API
permissions, host access, optional permissions, and content-script matches.
Startup records these in `extension-permissions.json`, marks each extension as
new, changed, or unchanged since the last launch, and logs new or changed
broad-access grants. There is no approval step before an extension runs.

Unpacked extensions can be loaded with repeated `run --extension` arguments. A
synthetic Manifest V3 extension with content scripts and `chrome.storage.local`
has been tested. There is no permission review UI, Chrome Web Store installer,
extension updater, or promise of arbitrary extension compatibility. Gmail is a
normal website in the Chromium profile; actual Google account sign-in and
Chrome Sync have not been validated or implemented here.

## Work needed before public distribution

- Signed/notarized releases, engine security updates, default-browser registration,
  and update/rollback infrastructure.
- Extension permission management and tests against real required extensions.
- Renderer-crash recovery, complete popup/opener and OAuth behavior, permission
  prompts, downloads, printing, accessibility, and broader website compatibility.
- Password management, passkeys, account migration, and local API isolation
  beyond the same-user token.
- Token support in the `tools/` integration and benchmark scripts.
- Per-tab history/scroll restoration and better media-aware eviction coverage.
- Real-world memory, responsiveness, battery, and long-running stability tests.

## References

- [CEF Rust bindings](https://github.com/tauri-apps/cef-rs)
- [CEF usage and process architecture](https://github.com/chromiumembedded/cef/blob/master/docs/general_usage.md)
- [Chromium process model and site isolation](https://chromium.googlesource.com/chromium/src.git/+/HEAD/docs/process_model_and_site_isolation.md)
