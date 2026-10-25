# Chromium infrastructure

## Engine and process ownership

The default Cargo feature is `chromium`, using the pinned `cef` Rust bindings
(156.1.0 / CEF 156.0.8). The native application embeds CEF BrowserViews in a
CEF window. Websites, the shell, and helpers use Chromium; there is no Node.js
runtime exposed to page JavaScript. The renderer sandbox stays enabled and no
remote DevTools port is opened by the browser.

The Rust process owns the tab registry and communicates with CEF on its UI thread
through queued tasks. OpenCode startup runs independently of the window. Only an
OpenCode child started by this app is stopped on exit. An existing server is reused.
The shell sends commands to Rust as CEF process messages through a native
`window.ipc.postMessage` (see [Shell IPC and local API
security](#shell-ipc-and-local-api-security)). It loads its document and reads
state from a loopback Axum server. Requests must use the expected Host and,
when present, the shell's Origin. Every route except `/shell` also requires a
per-launch secret in the `x-atrium-token` header. The browser writes it to
`<data dir>/api-token` (mode 0600) at startup and removes it on exit. The native
side attaches the header only to API requests from the shell's main frame while it
shows the shell document, so website tabs and external requests never carry it.
`atrium mcp` and `atrium ask` read the token from the data directory or
`ATRIUM_API_TOKEN`. Any process running as the same user can still read that file,
so this stops websites and other users rather than hostile local software. The
shell currently shares the persistent request context with website views; it is
not a separate Chromium storage partition.

### Layout stability

Tab BrowserViews are children of the CEF window, whose default fill layout sizes
every child to the full window on each layout pass. The window delegate's
`on_layout_changed` re-applies tab bounds synchronously in that same pass, so
pages are never painted at full-window size between a resize, view creation,
or eviction and the next shell layout message. Shell IPC messages arrive in the
order they were sent, so the newest layout rect is always applied last. Sidebar lists replace
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

### Background test instances

`ATRIUM_BACKGROUND_WINDOW=1` keeps a test instance out of the user's way. The
app uses the `Prohibited` activation policy, so it has no Dock icon or menu bar
and macOS does not let it become the active application. Its window stays
ordered in, so Chromium keeps a live compositor, but it is fully transparent,
has no shadow, ignores mouse events, is excluded from window cycling, and is
moved toward the screen edge (macOS keeps part of the frame on a display) and
ordered behind other windows. Chromium's macOS occlusion tracking is disabled in
this mode so the window still paints. Screenshots and evaluation work; pages
report `document.visibilityState === "hidden"`, so visibility-dependent page
behavior differs from a normal window. Keyboard focus, real input, and native
dialogs are not testable this way. Never use it for a real profile.

Verified on macOS 26 with the smoke, lifecycle, and persistence checks: the
frontmost application never changed to the test instance, the window reported
`alpha 0` and not on screen, and screenshots contained the rendered fixture.

`tools/build-chromium.sh --release` creates a complete development app with the
CEF framework, sandbox helper executables, Chromium credits, and verified ad hoc
signatures. CEF updates require changing the pin, rebuilding, and re-running the
integration checks. There is no automatic updater or notarized distribution yet.

## Shell IPC and local API security

The shell's commands travel as CEF process messages, not HTTP:

- **Marking the shell.** The browser process creates the shell's BrowserView with
  `extra_info` naming the shell URL.
- **Exposing `window.ipc`.** The renderer helper (`src/bin/atrium_helper.rs`) defines
  a read-only, native `window.ipc.postMessage(string)` only when the browser carries
  that marker, the frame is the main frame at exactly `<api>/shell`, and the context
  is the main world. Website tabs, frames, and extension worlds never see it.
- **Sending.** Each call sends one `atrium.shell.command` message, at most 256 KiB.
- **Receiving.** `Client::on_process_message_received` accepts it only from a
  renderer, for the shell's recorded browser identifier, from the shell's main frame,
  with a JSON object body. Rejected messages are dropped and counted.
- **Ordering.** Accepted commands go through the same FIFO queue as every other UI
  event. Messages from one frame arrive in order, so the old one-layout-in-flight
  workaround and the `fetch` fallback are gone. The WebKit build keeps its own
  native `window.ipc`.

The shell still loads its document and reads state, history, and frozen overlay images
from the loopback API. The token is attached only to those requests, and only from the
shell's main frame while it shows the shell document. `/api/browser/command` remains
for token holders such as scripts and tests.

Navigation and popup rules:

- The shell browser cannot navigate away from `/shell`. A user-gesture navigation to
  an http(s) URL, such as a dropped link, opens as a tab instead.
- Website tabs and frames cannot load the API origin.
- Website popups open as tabs only with a user gesture and an http(s) URL.
- Agent actions (click, type, key, select, scroll) run with `userGesture: true`, so
  links the assistant clicks still open. Its reads and `evaluate` don't.
- Agent tools only open http(s) URLs or `about:blank`.

Other protections:

- Every local response forbids framing, sniffing, caching, and referrers.
- `atrium run` makes the data directory 0700.
- When Atrium starts `opencode serve`, it passes a random `OPENCODE_SERVER_PASSWORD`,
  or the user's own if set, and authenticates with it.
- Exit cleanup (token file, OpenCode child) runs before CEF shutdown, because
  Chromium's teardown watchdog sometimes terminates the process inside `shutdown()`.

`/api/browser/memory` also reports:

- `layout`: the applied main-pane rect, plus whether an overlay covers it or Glance
  is open;
- `shell_ipc`: counters for accepted process messages, rejected process messages,
  and HTTP commands.

Shell scripting (`/api/debug/shell`, and evaluation or screenshots of the shell) is
available in debug builds, or in release builds started with
`ATRIUM_TEST_SHELL_EVAL=1`, and it still needs the token. Use that variable only for
test instances.

Findings, severities, and accepted risks are in [SECURITY-REVIEW.md](SECURITY-REVIEW.md).

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

Every script that calls the API uses `tools/atrium_api.py`, which sends the
`x-atrium-token` header. Pass the instance's `--data-dir` (its `api-token` file is
read) or `--token-file PATH`, or set `ATRIUM_API_TOKEN`; that is the order of
precedence. The file is re-read per request, so restarts are handled. Scripts
that start their own instance (`test_profile_persistence.py`) read the token
from the profile they create and launch with `ATRIUM_MOCK_KEYCHAIN=1` and
`ATRIUM_BACKGROUND_WINDOW=1`.

Run the fixture server and an isolated background browser in separate terminals:

```sh
python3 tools/fixture_server.py --port 7799
ATRIUM_MOCK_KEYCHAIN=1 ATRIUM_BACKGROUND_WINDOW=1 \
  ./target/bundle/atrium.app/Contents/MacOS/atrium \
  --data-dir /private/tmp/atrium-smoke run --port 7791 \
  --opencode-url http://127.0.0.1:1
python3 tools/smoke_chromium.py --api http://127.0.0.1:7791 \
  --data-dir /private/tmp/atrium-smoke --fixture http://127.0.0.1:7799
python3 tools/test_tab_lifecycle.py --api http://127.0.0.1:7791 \
  --data-dir /private/tmp/atrium-smoke --fixture http://127.0.0.1:7799
python3 tools/test_profile_persistence.py --port 7792 --fixture http://127.0.0.1:7799
```

The smoke check covers navigation, page interaction, JavaScript isolation,
screenshots, rejection of a foreign Origin, and rejection of a request without
the token. With an appropriate synthetic
extension loaded, `--expect-extension` also checks its content script. The lifecycle
check opens clean and dirty pages, waits for eviction, checks draft retention,
and wakes a sleeping page. Rust unit tests cover eviction selection, migration
parsing, path validation, endpoint validation, and the security fixes:

```sh
CEF_PATH="$PWD/target/cef" PATH="$PWD/target/build-tools/bin:$PATH" cargo test --locked
```

`tools/test_secure_ipc.py` starts its own background instances, with a mock
keychain, an invisible window, and a fresh profile, plus `ATRIUM_TEST_SHELL_EVAL=1` for
the first one. It runs the watchdog beside each instance (`--watchdog`) and fails if
an instance becomes the frontmost application. The fixture server must be running:

```sh
python3 tools/fixture_server.py --port 7881
python3 tools/test_secure_ipc.py --port 7880 --opencode-port 7885 \
  --fixture http://127.0.0.1:7881   # add --skip-opencode without an opencode binary
```

It drives the real shell UI. It creates tabs, switches them by clicking a sidebar
row, and closes them with the close button. It checks that the native pane follows
the shell layout and that 400 back-to-back layout messages apply in order. It opens
the command bar, which covers the page and loads the frozen backdrop through the
token route, then closes it with Escape, and opens and closes Glance. It confirms
that all of this went over process messages, with no HTTP command.

A hostile fixture page (`/evil`) then probes `window.ipc`, CORS and no-cors requests
to the API, popups (`file:` and http, without a gesture), framing the shell, and
navigating to it. The HTTP checks cover:

- missing and wrong tokens;
- bad Origins: other sites, `null`, the fixture port, and `localhost`;
- DNS-rebinding Host headers;
- preflights;
- hardening headers;
- file modes;
- `atrium mcp --api` to a non-loopback host.

A second instance checks that shell scripting is off without the variable and that an
existing 0755 data directory is tightened. Unless `--skip-opencode` is given, it also
checks that the OpenCode server it starts on `--opencode-port` refuses
unauthenticated requests and stops on quit.

### Recorded run: October 24, 2026 (`feat/ipc`)

| Command | Result |
| --- | --- |
| `cargo test --release --locked --offline` | 20 + 1 tests passed |
| `test_secure_ipc.py --port 7880 --fixture http://127.0.0.1:7881` | `PASS: 70 secure IPC and local-surface checks`, using opencode 1.18.10 on port 7885 |
| `smoke_chromium.py` against a background instance on 7882 | passed |
| `test_tab_lifecycle.py` against the same instance | `PASS: idle renderers release; drafts stay; agent and selected sleeping tabs wake` |

All instances ran with the watchdog, and none became frontmost.

Not covered:

- Keyboard input to the shell. The background window gets no real key events, so
  overlays are opened through the shell's own functions, and Escape is dispatched
  as a DOM event.
- Downloads, because they open a save dialog.
- Extension content scripts in the shell.

In background mode, every page reports `hidden` and its timers are throttled, so the
agent's settle wait after a click can take about 15 seconds.

For a repeatable RAM comparison, start a **fresh, empty**, extension-free release
profile on port 7783 and keep the fixture server on 7789. Then run:

```sh
python3 tools/compare_memory.py --data-dir /private/tmp/atrium-bench \
  --output /tmp/atrium-memory.json
```

The script starts the installed `/Applications/Aside.app` as a normal, visible
window (it has no background mode) with a separate temporary
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
python3 tools/compare_real_sites.py --data-dir /private/tmp/atrium-bench \
  --output /tmp/atrium-real-sites.json
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
Run benchmark instances with `ATRIUM_MOCK_KEYCHAIN=1`.

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
- Per-tab history/scroll restoration and better media-aware eviction coverage.
- Real-world memory, responsiveness, battery, and long-running stability tests.

## References

- [CEF Rust bindings](https://github.com/tauri-apps/cef-rs)
- [CEF usage and process architecture](https://github.com/chromiumembedded/cef/blob/master/docs/general_usage.md)
- [Chromium process model and site isolation](https://chromium.googlesource.com/chromium/src.git/+/HEAD/docs/process_model_and_site_isolation.md)
