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
The shell sends commands to Rust as CEF process messages through a native
`window.ipc.postMessage` (see [Shell IPC and local API
security](#shell-ipc-and-local-api-security)). It loads its document and reads
state from a loopback Axum server. Requests must use the expected Host and,
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
- Sidebar list: a source-list `NSTableView` in a scroll view, with Favorites
  as push-button tiles, the space name, pinned pages, New Tab, the space's tabs
  and recent chats. Rows show favicons (fetched on a background thread), a
  spinner while loading, dimmed text for sleeping tabs and unopened pins, and
  a close button on hover and on the active row. The system selection shows
  the active tab (both tabs in split view). Click activates, Option-click opens
  split view, middle click closes, drag reorders tabs or pins, and dragging a
  tab above New Tab pins it. Context menus: Open in Split View, Duplicate, Copy
  Link, Pin Tab, Move to Space, Close Tab and Close Other Tabs on tabs; Open,
  Copy Link and Unpin on pins; Remove from Favorites on favorites; Rename
  Space… (edits the name in place) on the space header.
- The shell still owns spaces, pins and favorites. In native mode it sends
  a sidebar model after each render (`sidebar` IPC) and runs the actions the
  list sends back through `atriumShell.sidebar`. Tab order belongs to the
  manager (`moveTab`), and is saved with the session.
- The shell hides only what it no longer draws, keeping its layout, and
  reports where its sidebar list is, whether the sidebar is shown and whether
  a full overlay (command bar, onboarding) is open. The native views hide in
  both cases, because they sit above the shell.

Still web-drawn: the space switcher and buttons at the bottom of the sidebar,
the start page search box, and (by design) chat and onboarding. ⌘L still opens
the command bar. Not yet verified with real input: clicking, typing with an
input method, real drag sessions (the test calls the same drop logic AppKit
calls), hover, dragging the window from the toolbar's empty area, and
VoiceOver. Chat rows and the agent-working badge need an OpenCode server and
aren't covered by the test. `tools/test_native_chrome.py` drives the real
controls through test-only hooks (`nativeChrome` and `nativeSidebar`
commands, background mode only) and checks the results.

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

### Background test instances

`ATRIUM_BACKGROUND_WINDOW=1` keeps a test instance out of the user's way. The
app uses the `Prohibited` activation policy, so it has no Dock icon or menu bar
and macOS does not let it become the active application. Its window stays
ordered in, so Chromium keeps a live compositor, but it is fully transparent,
has no shadow, ignores mouse events, is excluded from window cycling, and is
moved toward the screen edge (macOS keeps part of the frame on a display) and
ordered behind other windows. Chromium's macOS occlusion tracking is disabled in
this mode so the window still paints. Screenshots and evaluation work, and the
selected tab reports `document.visibilityState === "visible"`, so permission
prompts reach the app. The first-run onboarding is closed as soon as the shell
loads, because pages under a shell overlay are hidden and Chromium defers
their permission prompts. Keyboard focus, real input, and native dialogs are not
testable this way. Never use it for a real profile.

Verified on macOS 26 with every integration check listed below: a watchdog
polling `lsappinfo front` never saw the test instance frontmost, the window
server listed its window at `alpha 0` with most of the frame off the display,
and screenshots contained the rendered fixture.

The protections cover every window the process creates, not only the main
one, because extensions can make Chromium open windows of its own. In this
mode `NSApplication` refuses `activateIgnoringOtherApps:`, `activate`, and
any activation-policy change, and modal sessions return immediately.
`NSRunningApplication activateWithOptions:` is refused for the app itself.
`NSWindow` ordering calls are swizzled so any window that is ordered in is
first made transparent and moved off screen, then ordered back rather than to
the front; `makeKeyWindow` and `makeMainWindow` do nothing. A 250 ms sweep also
re-asserts the policy and conceals any visible window it finds. Each refusal is
logged to stderr. Outside background mode none of this is installed.

`tools/build-chromium.sh --release` creates a complete development app with the
CEF framework, sandbox helper executables, Chromium credits, and verified
signatures (see [Releases, signing, and updates](#releases-signing-and-updates)).
CEF updates require changing the pin, rebuilding, and re-running the
integration checks.

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
Chromium's profile. In-memory application state is not restored; history and
scroll are partially restored (see below). Media protection is conservative
but does not guarantee detection inside every cross-origin frame. Protected tabs
can exceed the target; the app does not discard unsaved forms to meet a RAM claim.

### Renderer crashes

Every tab's request handler reports `on_render_process_terminated`. The app
stays running, pending evaluations and screenshots for that tab fail, and the
tab is marked `crashed: true` in `/api/browser/tabs` at its last committed URL.
A crashed background tab releases its view immediately and becomes a normal
sleeping tab. A crashed visible tab keeps its (blank) view until it is selected
again, reloaded, or navigated; it is not reloaded automatically, which avoids
crash loops. Agent evaluation on a crashed tab returns an error asking for a
reload instead of silently waking it. A crashed shell reloads immediately.
There is no "page crashed" UI in the shell yet. `tools/test_crash_recovery.py`
kills visible and background renderers with `chrome://kill` and checks recovery;
renderer crashes with other causes (out of memory, real faults) follow the same
callback but were not separately reproduced. One renderer can host several
same-site tabs, so a single crash can mark several tabs.

### Popups and openers

Website popups are created by Chromium with the opener's relationship intact
and shown as tabs in the same window. `window.open` popups (CEF's `NEW_POPUP`
disposition) record their opener: while both are live neither is evicted,
and when the popup closes itself the opener becomes the active tab again.
This is what OAuth and payment popups need: the popup can `postMessage` to
`window.opener` and call `window.close()`. `target=_blank` links and
`window.open` without popup features open ordinary foreground tabs (background
for background dispositions); Chromium's default `noopener` for `_blank` still
applies. Requested popup sizes are ignored. Popups requested by the shell or
Glance are opened as plain new tabs without an opener, so websites never get a
reference to the browser UI. Agent evaluation runs with a user gesture so agent
clicks can open popups like real clicks. If an opener is evicted after its
popup closes, or the app restarts, the relationship is gone. Checked with
`tools/test_popups.py` against a local fixture pair; real identity providers
(Google, GitHub, Apple) have not been tested.

### Site permissions

Camera, microphone, screen capture, geolocation, notifications, and every other
Chromium permission prompt are denied unless the user allowed that origin for
every permission in the request. There is no prompt UI yet; decisions are made
through the API and stored in `<data dir>/site-permissions.json`:

- `GET /api/browser/permissions` lists `decisions` (origin to permission to
  `allow`/`deny`), the 50 most recent requests with what was granted, and the
  permission names.
- `POST /api/browser/permissions` with `{"origin": "https://meet.example",
  "permission": "camera", "decision": "allow"}` records a decision;
  `"decision": null` forgets it. Only canonical HTTP(S) origins and known
  permission names are accepted. The shell is never granted anything.

Chromium remembers a denied prompt as a per-origin content setting and stops
asking, so decisions are also written to Chromium's content settings at startup
and on every change (allow, block, or back to default). Requests answered by
those settings are not listed as recent. Granting camera, microphone, or
location still requires the macOS permission for the app. Checked with
`tools/test_permissions.py`, which only grants window management (no macOS
permission involved) and checks that notifications stay denied; camera,
microphone, and geolocation grants have not been exercised.

Session metadata is written with a temporary file and rename. Restart restores
logical tabs and loads only the active page. Assistant scratch pages are omitted
and bounded to three simultaneous pages, with cancellation cleanup.

### History and scroll after eviction and restart

CEF cannot give a new view a navigation history, so each tab keeps its own back
and forward URL lists (up to 50 each, HTTP(S) only) for the history of views it
no longer has. They are captured from Chromium's navigation entries when a view
is released and when the session is saved (every ten seconds when changed, and
at quit), and stored with each tab in `chromium-tabs.json`. Back and forward use
the live view's history first; stepping past it replaces the view with a fresh
one at the saved URL. A navigation that adds a history entry drops the saved
forward list, as in a normal browser. Each step outside the live history is a
full load in a new view: there is no back/forward cache, form state, or
`history.state`, and an entry reached this way has no scroll offset.

The eviction probe also records the page's document scroll offset; it is applied
with `scrollTo` once the page loads again, including after a restart. Pages that
build their content after load, or scroll an inner element instead of the
document, may not land at the same place. Tabs still live at quit are saved
without a scroll offset. Checked with `tools/test_history.py`.

The `/api/browser/memory` endpoint reports logical tabs and live views.
`live_renderers` is the legacy JSON field name for **BrowserViews**, not a count
of operating-system renderer processes. Use a process-tree measurement for RAM.

## Verification

Every script that calls the API uses `tools/atrium_api.py`, which sends the
`x-atrium-token` header. Pass the instance's `--data-dir` (its `api-token` file is
read) or `--token-file PATH`, or set `ATRIUM_API_TOKEN`; that is the order of
precedence. The file is re-read per request, so restarts are handled. It is
removed before Chromium shuts down, because Chromium's teardown watchdog
terminates a slow shutdown (seen on a heavily loaded machine) before later
cleanup would run. Scripts
that start their own instance (`test_profile_persistence.py`) read the token
from the profile they create and launch with `ATRIUM_MOCK_KEYCHAIN=1` and
`ATRIUM_BACKGROUND_WINDOW=1`.

Run the fixture server and an isolated background browser in separate terminals:

```sh
python3 tools/fixture_server.py --port 7799
ATRIUM_MOCK_KEYCHAIN=1 ATRIUM_BACKGROUND_WINDOW=1 \
  ./target/bundle/Atrium.app/Contents/MacOS/atrium \
  --data-dir /private/tmp/atrium-smoke run --port 7791 \
  --opencode-url http://127.0.0.1:1
python3 tools/smoke_chromium.py --api http://127.0.0.1:7791 \
  --data-dir /private/tmp/atrium-smoke --fixture http://127.0.0.1:7799
python3 tools/test_tab_lifecycle.py --api http://127.0.0.1:7791 \
  --data-dir /private/tmp/atrium-smoke --fixture http://127.0.0.1:7799
python3 tools/test_crash_recovery.py --api http://127.0.0.1:7791 \
  --data-dir /private/tmp/atrium-smoke --fixture http://127.0.0.1:7799
python3 tools/test_popups.py --api http://127.0.0.1:7791 \
  --data-dir /private/tmp/atrium-smoke --fixture http://127.0.0.1:7799
python3 tools/test_permissions.py --api http://127.0.0.1:7791 \
  --data-dir /private/tmp/atrium-smoke --fixture http://127.0.0.1:7799
python3 tools/test_profile_persistence.py --port 7792 --fixture http://127.0.0.1:7799
python3 tools/test_history.py --port 7793 --fixture http://127.0.0.1:7799
python3 tools/investigate_renderers.py --port 7794 --fixture-port 7799 \
  [--real-sites tools/real_sites.json [--per-site] [--skip-fixtures]]
```

The extension check starts its own background instances and fixture server.
It kills and fails on any instance that becomes frontmost, and also runs
`--watchdog SCRIPT PID` per instance when that script exists. It needs network
access to the Chrome Web Store and the blocked ad hosts:

```sh
python3 tools/test_extensions.py --port 7852 --fixture-port 7853
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
python3 tools/bench_guarded.py --workload fixture-distinct --runs 3 \
  --output docs/benchmarks/2026-10-24-guarded-fixture-distinct.json
python3 tools/bench_guarded.py --workload real --runs 3 \
  --output docs/benchmarks/2026-10-24-guarded-real-sites.json
```

The fixture workloads open ten local pages (`fixture` on one origin,
`fixture-distinct` on ten `.localhost` sites), each holding a 12 MiB buffer.
The CEF build samples them while all are live (6 s after loading) and again
after its lifecycle policy has evicted background pages (90 s); Aside has no
such policy, so its second phase measures the same elapsed time. The real-site
workload uses `tools/real_sites.json` signed out; a page rejected by Aside's
validation (challenge, redirect, or unrendered page) is closed in both browsers.
Samples sum macOS physical footprint (`proc_pid_rusage`, `ri_phys_footprint`)
and RSS over each browser's process tree, with process counts by type (browser,
GPU, network, other utility, renderer, extension renderer). Zombie children
and Aside's periodic `aside-daemon` are reported separately and excluded.
Reports contain titles, URL paths, and DOM counts, never page text or
addresses. The harness stops if the Mac sleeps during a measurement.

Treat footprint as the primary metric. RSS counts shared pages repeatedly and
moves with memory compression: in one Aside fixture run RSS fell 21% between
phases while footprint fell 7%. Neither is total machine memory.

> **Preliminary — do not publish.** Every result in the fixture, real-site,
> and responsiveness sections below came from a heavily loaded machine: load
> averages of roughly 100–375 during the session (34–244 at the start of
> individual browser runs), on battery, with other agents' builds and
> browsers running. They validate the tools, not the claims. Rerun on a quiet
> machine before citing any number.

### Fixture results: October 24, 2026

Apple Silicon, 16 GB, macOS 26, on battery; release build (CEF 156.0.8,
Chromium 156.0.8078.48) against Aside 1.0.1022.1 (Chromium 154.0.8037.100).
[Report](benchmarks/2026-10-24-guarded-fixture-distinct.json): ten
separate `.localhost` sites, three repetitions, all three comparable; every
CEF live sample had all 11 views (ten pages and the shell) live and every
idle sample had 7. No visibility-guard violations. The machine was shared with
other agents' builds: the one-minute load average before a browser run ranged
from 34 to 244, so these are **not** idle-machine numbers.

| Phase | Metric | Aside median (min–max) | This build median (min–max) | Paired reduction median (min–max) |
| --- | --- | ---: | ---: | ---: |
| All pages live | Footprint | 1,036 MiB (988–1,076) | 729 MiB (712–812) | 29.7% (17.9–33.8) |
| All pages live | RSS | 2,777 MiB (1,998–2,855) | 1,644 MiB (1,554–1,737) | 40.8% (13.0–45.6) |
| All pages live | Processes | 20 (19–21) | 16 (16–16) | |
| After eviction | Footprint | 1,030 MiB (969–1,370) | 544 MiB (521–560) | 45.7% (43.8–62.0) |
| After eviction | RSS | 2,181 MiB (1,741–2,852) | 1,065 MiB (989–1,133) | 51.2% (43.2–60.3) |
| After eviction | Processes | 20 (19–21) | 12 (12–12) | |

Per-run footprint, live then after eviction: Aside 1,036/969, 1,076/1,370,
988/1,030 MiB; this build 729/544, 712/521, 812/560 MiB.

Median footprint by process type with all pages live:

| Process type | Aside | This build |
| --- | ---: | ---: |
| Browser | 1 × 90 MiB | 1 × 71 MiB |
| GPU | 1 × 120 MiB | 1 × 104 MiB |
| Network service | 1 × 32 MiB | 1 × 28 MiB |
| Other utility | 1 × 22 MiB | 1 × 21 MiB |
| Renderers | 13, 587 MiB total | 12, 507 MiB total |
| Extension renderers | 2, 139 MiB total | none |

After eviction this build ran 8 renderers (334 MiB): six pages, the shell,
and one spare. For these fixtures the renderers of evicted pages exited. With
all pages live, most of the footprint difference comes from Aside's two
built-in extension renderers, its browser and GPU processes, and its own UI
pages; the per-page renderer cost is similar (about 40–45 MiB each). After
eviction, most of the difference is the four released pages. Aside's numbers
vary more between runs than this build's.

### Real public websites

`tools/real_sites.json` defines Google Search, YouTube, Gmail's signed-out page,
GitHub, Wikipedia, BBC News, MDN, Stack Overflow, Reddit, and Hacker News.
Public websites vary by time, region, cookies, viewport, and browser version;
signed-in Gmail and user documents are outside this test.

[Partial report](benchmarks/2026-10-24-guarded-real-sites.json): two
completed repetitions; the third stopped when this build hung (below). In
both completed runs the browsers accepted the same eight sites; YouTube (not
rendered in time headless) and Reddit (challenge page) were excluded from
both. CEF had all 9 views live in the first phase and 7 after eviction. Load
average 8–57.

| Run | Phase | Aside footprint (processes) | This build footprint (processes) | Reduction |
| --- | --- | ---: | ---: | ---: |
| 1 | All pages live | 5,813 MiB (95) | 5,349 MiB (92) | 8.0% |
| 1 | After eviction | 3,612 MiB (53) | 2,889 MiB (43) | 20.0% |
| 2 | All pages live | 2,968 MiB (50) | 4,999 MiB (88) | −68.5% |
| 2 | After eviction | 2,908 MiB (50) | 4,638 MiB (85) | −59.5% |
| 3 | All pages live | 6,008 MiB (94) | hung | |
| 3 | After eviction | 3,262 MiB (47) | | |

**These results support no memory claim for real websites, in either
direction.** Renderers are 80–90% of both footprints, and their number follows
the ads and embeds a site serves at that moment: Aside alone ran 85, 40, and
84 renderers in three runs and in run 3 fell from 84 to 37 within two minutes
with no eviction; Stack Overflow reported 1 iframe in one Aside run and 22 in
the next. The browsers run minutes apart and never see the same ad auction.
Two differences were consistent: this build's GPU process used 219–278 MiB
against Aside's 132–138 MiB on these pages (the reverse of the fixture
result), and in run 2 this build kept 79 renderers with 7 live views. That
fits renderers outliving evicted pages; it is under separate investigation
and was only measured here. A fair comparison needs identical content, for
example a Web Page Replay archive served to both browsers, and more runs.

**Hang observed.** In the third real-site repetition the CEF browser process
stopped responding 44 seconds after launch while the sites loaded: the UI
thread, the IO thread, and several thread-pool workers were all blocked
waiting for the write side of the same reader-writer lock inside the CEF
framework, and the local API timed out. CEF had logged "Timeout of new browser
info response" just before. It was seen once; it has not been reproduced or
diagnosed (the framework is stripped).

### Earlier results (superseded)

Before the guarded harness, `compare_memory.py` and `compare_real_sites.py`
started `/Applications/Aside.app` as a normal visible window and measured a
single run. Those scripts have been removed. Their reports remain for
reference: [same-origin](benchmarks/2026-10-24-same-origin.json) and
[distinct sites](benchmarks/2026-10-24-distinct-sites.json) RSS reductions of
57.1% and 60.1%, and a [real-site run](benchmarks/2026-10-24-real-sites-strict.json)
([validation](benchmarks/2026-10-24-real-sites-strict.validation.json)) with
footprint reductions of 22.5% live and 15.9% after eviction. These are single
RSS-led runs against a visible, differently configured baseline and should
not be cited.

### Responsiveness

`tools/test_responsiveness.py` starts its own fixture server and background
instance and measures through the token-protected
`POST /api/browser/devtools` route. That route forwards only
`Performance.enable`, `Performance.disable`, `Performance.getMetrics`, and
`Input.synthesizeScrollGesture`, and only to a live, non-shell tab, so a
measurement never wakes a sleeping page. It exits non-zero if the median live
switch exceeds 250 ms or the median wake exceeds 3 s.

```sh
python3 tools/test_responsiveness.py --output docs/benchmarks/2026-10-24-responsiveness.json
```

[Report](benchmarks/2026-10-24-responsiveness.json), load average 14–29,
local fixtures, CEF build only (Aside cannot be measured comparably in
headless mode):

| Measurement | Median | p95 | Range |
| --- | ---: | ---: | ---: |
| Live tab switch: activate API round trip (30) | 2.2 ms | 15.2 ms | 1.7–19.7 ms |
| Live tab switch: page `visibilitychange` after request | 1.2 ms | 3.5 ms | 0.7–6.6 ms |
| Live tab switch: first animation frame after request | 1.6 ms | 5.6 ms | 1.0–7.0 ms |
| Scroll, 4,000 px synthesized wheel gesture (5 runs): frame interval | 16.7 ms | 17.8 ms | 14.8–18.9 ms |
| Wake an evicted tab: view recreated (5) | 16.1 ms | 18.5 ms | 12.9–18.5 ms |
| Wake an evicted tab: page reports loaded (5) | 45.1 ms | 68.2 ms | 39.5–68.2 ms |

No scroll frame took longer than 33 ms. The selected tab ran 61 animation
frames per second. Times are taken from the request with the page's clock,
not from photons on screen; the background window is composited but not
displayed, so presentation latency is not covered. Wake time is for a
small local page; a real site reloads from the network after eviction. Under
load averages of 140–170 in an earlier trial, the p95 live switch rose to
650 ms and one evaluation timed out.

### Soak test

`tools/soak_test.py` runs a 30-minute background session: fixture tabs (plain,
form, scroll, dirty draft) opened, closed, switched, navigated, typed into, and
evicted/woken around a 12-tab target, plus three real sites and a looping
local video. Every five minutes it closes the churn tabs and samples the same
fixed set. It fails on a browser exit, renderer terminations (now logged by
the app), Chromium child-process crashes, Rust panics, unexpected API errors,
new crash reports, the Mac sleeping, checkpoint footprint growth above both
25% and 250 MiB, or process-count growth above 4. **It has not been run yet.**

### Battery

With eight pages live, that run had roughly 45 renderer processes, most idle at
about 38 MiB RSS each, and its process count stayed at 56 after two pages were
evicted.

### Where renderer processes come from

`tools/investigate_renderers.py` starts a background instance, drives it
through the shell command API, and counts its child processes by Chromium type
after each step. Recorded runs:
[fixtures and real sites together](benchmarks/2026-10-24-renderer-attribution.json)
and [each real site alone](benchmarks/2026-10-24-renderer-attribution-per-site.json)
(query strings removed). With fixture sites on `site-N.localhost`:

- An idle instance has two renderers: the shell and one more, consistent with
  Chromium's spare renderer.
- Each tab has its own renderer, including several tabs on the same site.
- Closing tabs or evicting views released their renderers within four seconds:
  10 views over budget went from 12 renderers to 8 when 4 views were evicted.
  No renderer outlived its BrowserView.
- Cross-site iframes get one process per iframe site, shared across tabs.
  Three tabs embedding the same three iframe sites used 3 + 3 renderers, and
  closing one tab freed only its own page's process.
- Navigating one tab across six sites left one renderer for it; the
  back/forward cache did not keep earlier pages' processes.

On real sites, almost all of the extra processes are cross-site subframes on
live pages. With each page as the only open tab, the instance ran 17 renderers
for Stack Overflow, whose page had 24 iframes from 15 ad-auction origins (Google
ad frames, Criteo, OpenX, PubMatic, Index Exchange, Rubicon, TripleLift, and
others); 6 for BBC News (Optimizely and a consent manager); 5 for Google Search
(a YouTube embed) and Reddit; 4 for YouTube, Gmail's signed-out page, GitHub,
Wikipedia, and MDN; and 3 for Hacker News. Just before each page opened, 2 to 4
renderers were already running (see below). Ad frames load
and rotate after the page reports loaded, so counts keep changing while such a
page is live. After closing a page, one or two renderers sometimes stayed for
over 35 seconds (after YouTube, which registers a service worker, for example);
the next page reused them and the count returned to the idle two when the last
page closed. In the mixed run, closing the two oldest of 12 tabs changed
nothing because eviction had already released them.

So the 45 renderers came from live pages' subframes, not from closed views.
The unchanged count after eviction is consistent with this: evicting a page
frees its subframe processes only when no other live page uses those sites,
and ad pages keep adding frames. That run's exact sequence was not reproduced.
Site isolation is left on. Reducing these processes needs fewer subframes,
which built-in ad blocking provides (below).

### Ad blocking

`src/blocking.rs` refuses third-party subresource and subframe requests to a short
list of ad exchanges, ad servers, and their ID-sync endpoints, so header-bidding
pages never create most of their ad frames. Top-level navigations are never
blocked (following an ad link still works), a listed company's own pages load
normally, and analytics and tag managers are left alone because pages call into
them. `ATRIUM_CONTENT_BLOCKING=0` turns it off; `/api/browser/memory` reports
`content_blocking.blocked_requests`.

`tools/bench_content_blocking.py` opens six ad-heavy sites (BBC News, Stack
Overflow, CNN, weather.com, Daily Mail, Forbes) in a background instance with
blocking off and on and samples the process tree after 30 s. Two alternating runs
on 2026-10-25 (`docs/benchmarks/2026-10-25-content-blocking.json`), medians:

| | Off | On |
|---|---|---|
| Physical footprint | 4775 MiB | 1775 MiB |
| Processes | 74 | 18.5 |
| Page load event | 1406 ms | 832 ms |
| First contentful paint | 480 ms | 328 ms |
| Frames, all six pages | 185.5 | 30 |

Stack Overflow and the Daily Mail served a short interstitial in both conditions.
Run benchmark instances with `ATRIUM_MOCK_KEYCHAIN=1`.

## Migration and extensions

Backend setup routes remain available under `/api/setup`. They discover Chrome
profile names, import selected data on request, and inventory installed extension
names. Chrome's files are only read, never changed, and they are not copied. Passwords and passkeys are not imported
(password import is separate work). OpenCode connection checks validate the
endpoint and browser tool registration before changing the active connection.
Only the endpoint address is saved; server credentials remain in memory or come
from `OPENCODE_SERVER_USERNAME` / `OPENCODE_SERVER_PASSWORD`.

### Chrome import

`POST /api/setup/import` takes `{"profile": "Default", "bookmarks": true,
"history": true, "tabs": true, "search_engine": true, "extensions": true}`; every
kind is opt-in. Imported data is stored under `<data dir>/import/`:

- **Bookmarks** keep Chrome's folder tree (bookmarks bar, other, mobile; empty
  roots dropped, empty sub-folders kept), titles, and added dates, keyed by
  Chrome's bookmark GUID. A re-import replaces that profile's snapshot and
  reports `added`, `updated`, `removed`, and `unchanged`. Only HTTP(S) URLs are
  imported (up to 10,000). The response keeps the old flat `bookmarks` list with
  `folder` paths such as `Bookmarks bar / Work`.
- **History** reads one row per visit from Chrome's `visits` table (subframe
  navigations and hidden URLs excluded) with real visit times, falling back to
  one row per URL for databases without it. Visits merge by URL and timestamp,
  so re-imports add nothing already imported (`already_imported`). Limits: 20,000
  URLs, 200 visits each.
- **Tabs** parse the newest `Sessions/Session_*` SNSS file (or `Current
  Session`): tabs of open normal windows in window and tab order, at their selected
  navigation entry. Closed tabs and windows, popups and app windows, and non-web
  URLs are skipped. Tabs are added asleep (no renderer) like a restored session;
  URLs already open are not opened again. Encrypted session files are rejected.
- **Search engine**: Chrome's default from `Preferences`
  (`default_search_provider_data.template_url_data`, or the `synced_guid` looked
  up in `Web Data`'s `keywords` table). Chrome-only template parameters are
  removed, and only HTTPS templates (or loopback HTTP) are accepted. The saved
  `search-engine.json` drives searches typed in the address bar. With
  no explicit choice Chrome uses its built-in default, so Atrium keeps Google.
  Other keywords (site search shortcuts) are not imported.

`GET /api/setup/imported?history=N` returns the imported bookmarks (flat and per
profile tree) and the N most recent history entries with their visit times. The
shell does not show imported bookmarks or history yet; this API is the
integration point.

`POST /api/setup/import/cookies` is a separate request that only runs with
`{"profile": ..., "consent": true}` (optionally `"domains": ["example.com"]` to
limit it to those sites and their subdomains). Without consent it returns
`consent_required` and the security notice and does nothing. **Security:**
imported cookies are live sign-in sessions. Anything that can read the Atrium
profile (software running as the user, installed extensions) can use them to act
as the user on those sites. Chrome's copies remain valid. Revoking a session means
signing out on the site. The response always includes this `security_notice`; a
UI must show it and get agreement before sending `consent: true`.

The import reads `Network/Cookies` (or `Cookies`) read-only and obtains the
"Chrome Safe Storage" password with `security find-generic-password`. **In
real use macOS shows a keychain prompt at this point**; that is by design and
happens only on this explicit request. Values are decrypted (`v10`,
AES-128-CBC, PBKDF2-SHA1 key, the SHA-256 domain prefix of database version
24+ verified and removed) through macOS CommonCrypto, so no crates were added.
Each cookie is written to CEF's global cookie manager with its domain (host-only
vs. domain cookie), path, Secure, HttpOnly, SameSite, priority, creation and
expiry; session cookies stay session cookies (the profile persists them). Expired
and partitioned (CHIPS) cookies are skipped, since CEF cannot set a partition key.
Values that fail to decrypt or belong to another domain are reported by domain and name.
After writing, the import reads all cookies back from CEF and reports how many
match exactly (`verified`). Re-importing replaces cookies with the same
name/domain/path rather than duplicating them. Cookie values never appear in
responses or reports. Chrome holds exclusive locks on its open databases, so
history and cookie imports ask the user to close Chrome when they are busy.

`GET /api/setup/import/report` returns every import run (newest first, last 50)
with per-kind `status`, `imported`, `skipped`, and `errors` plus details, a
`latest` result per kind, and the active search engine.

Test instances (`ATRIUM_MOCK_KEYCHAIN` set) never read the user's Chrome
directory or login keychain. They see only `ATRIUM_TEST_CHROME_ROOT` (otherwise no
profiles). `ATRIUM_TEST_CHROME_SAFE_STORAGE` supplies the cookie password and is
honored only when `ATRIUM_MOCK_KEYCHAIN=1`. Without it, the cookie import fails
instead of prompting.

`tools/test_chrome_import.py` builds a synthetic Chrome root in `/private/tmp`
(Bookmarks, History and Cookies SQLite with values encrypted with a test key
using Python's `hashlib` and the `openssl` CLI, an SNSS session, Preferences, and
Web Data). It then starts its own background instance with the watchdog and
checks the following:
folder paths and counts; visit counts and times; that the second run adds no
bookmarks, visits, or tabs; restored tabs (closed window, old entries, and
`chrome://` skipped); a typed search using the imported engine; that cookies are
refused without consent; and that the fixture server's `/cookies` page receives
exactly the expected `Cookie` header. That header includes host-only, domain,
HttpOnly, Secure (on `*.localhost`), and plaintext legacy cookies. It excludes
expired, partitioned, wrong-path, unrelated-domain, and tampered cookies. The test
also checks that `document.cookie` hides the HttpOnly cookie, that there are no
duplicates after re-import, that the domain filter works, and the report:

```sh
python3 tools/test_chrome_import.py --port 7860 --fixture-port 7861
```

Not verified: a real Chrome profile and the real keychain prompt (by rule, tests
never touch them). Before relying on it, run one manual import against a real
profile with Chrome closed. SameSite enforcement was checked only by reading the
attributes back, not by cross-site requests. If a restored tab later redirects,
its URL changes, so a re-import can open the original URL again.

`GET /api/browser/extensions` summarizes each loaded extension's declared API
permissions, host access, optional permissions, and content-script matches.
Startup records these in `extension-permissions.json`, marks each extension as
new, changed, or unchanged since the last launch, and logs new or changed
broad-access grants. There is no approval step before an extension runs.

Unpacked extensions can be loaded with repeated `run --extension` arguments. A
synthetic Manifest V3 extension with content scripts and `chrome.storage.local`
has been tested. There is no permission review UI, Chrome Web Store installer,
extension updater, or promise of arbitrary extension compatibility. Gmail is a
normal website in the Chromium profile (see the Google section below). Chrome
Sync is not implemented.

## Google accounts, Gmail, and "Sign in with Google"

Google refuses sign-in from browsers it considers embedded or automated ("This
browser or app may not be secure"). The browser presents itself as what it is:
a standard Chromium build at its real engine version, with no extra product
token and nothing a page can use to tell it is driven by an agent.

What pages see (measured by `tools/test_google_signin.py`, CEF 156.0.8):

| Signal | Value |
| --- | --- |
| `User-Agent` | `Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/156.0.0.0 Safari/537.36` |
| `Sec-CH-UA` / `navigator.userAgentData.brands` | `"Not:A-Brand";v="8", "Chromium";v="156"` |
| Full version list (on `Accept-CH`) | `"Not:A-Brand";v="8.0.0.0", "Chromium";v="156.0.8078.48"` |
| `navigator.webdriver` | `false` |
| `window.chrome` | `app`, `csi`, `loadTimes` |
| Remote debugging | no listener; the process listens only on the local API port |

These are Chromium's own defaults; the app overrides none of them. Choices:

- **Brand.** The brand list stays `Chromium` plus Chromium's GREASE entry, as
  other Chromium builds send. An `Atrium` brand was not added: CEF has no setting
  for it, and per-tab DevTools emulation would not cover service and shared
  workers, giving pages two inconsistent answers, which is itself a bot signal.
  There is no `Google Chrome` brand and no `X-Client-Data` header, because this
  is not Google Chrome. The user agent keeps Chromium's standard reduced string
  (macOS 10_15_7 and a frozen minor version are Chromium's own reduction).
- **Injected scripts.** `assets/page.js` and `assets/lifecycle.js` used to define
  `window.__atrium` and `window.__atriumLifecycle` in the page's main world, where
  any script could detect them. They now run in a DevTools isolated world
  (`Page.createIsolatedWorld`, one per main-frame document, reused by name). It
  shares the DOM but not JavaScript globals, like an extension content script.
  The `evaluate` tool runs there too, so agent code cannot read page globals.
  `onbeforeunload` is per world, so the eviction probe reads it with a
  stateless main-world expression. The shell page is unchanged.
- **DevTools use.** The host drives pages through `Runtime.evaluate` without
  `Runtime.enable`. Console messages are not sent to a DevTools client, so the
  known getter-on-`Error.stack` trick does not trigger (the test checks this).
  Agent evaluations pass `userGesture: true`, as Puppeteer does by default, so
  an agent click on a button that opens a sign-in popup is not stopped by the
  popup blocker. The agent's highlight outline is briefly added to the DOM
  during an agent action; that is visible to pages but never present when the
  user browses alone.

### Results: October 24, 2026

US network, fresh signed-out background profiles, release build, no
credentials. Two consecutive complete runs of `tools/test_google_signin.py`
passed every required check, as did single-site third-party runs. An earlier
version of the script left the agent's tab hidden, where Chromium throttles
pages; YouTube then never rendered its Sign in button within the time limit.
The script now shows each tab it tests, as a user would.

| Check | Result |
| --- | --- |
| Fingerprint (above), isolation, snapshot refs across evaluations, no debug port | pass |
| accounts.google.com: random nonexistent `@gmail.com` address, then Next | pass: "Couldn’t find this account", no insecure-browser page |
| Gmail signed out (`mail.google.com`) | pass: redirected to `workspace.google.com/intl/en/gmail/` |
| YouTube home, then its Sign in link | pass: Google's identifier page for `service=youtube` |
| Stack Overflow "Log in with Google" (same tab) | pass: Google OAuth identifier page |
| Spotify "Continue with Google", Zoom "Sign in with Google" (popups) | pass: Google OAuth identifier page in a new tab |
| `window.open` opener (informational) | **gap**: see below |

The fixes in this section were not needed for these results; the build before
them also reached "Couldn’t find this account". They remove the injected-global
and console-detection signals before Google, or any site, starts checking them.

### Remaining risks and manual steps

- **The decisive check needs a real account.** Google answers a nonexistent
  address before it judges the browser; for an existing account the
  insecure-browser block appears after Next. Do this once by hand in a normal
  (not background) window: sign in to accounts.google.com with your own
  account, including your usual 2-Step Verification, then open Gmail. To check
  just the identifier step without typing a password, run
  `python3 tools/test_google_signin.py --only google_signin --identifier you@gmail.com`;
  it requires Google to show the password step and stops there.
- **Popups lose `window.opener`.** `on_before_popup` cancels every popup and
  opens its URL as a tab, so `window.open` returns `null` and the new page has
  no opener. Popup-based "Sign in with Google" (Spotify, Zoom, Google Identity
  Services `ux_mode: popup`) reaches Google, but a flow that returns its result
  to the opener with `postMessage` cannot complete. Proper popup/opener support
  is being implemented separately; `popup_opener` in the test reports it.
- **Passkeys and security keys.** `isUserVerifyingPlatformAuthenticatorAvailable()`
  is `false`: Touch ID passkeys need a signed app with keychain entitlements,
  which ad hoc development builds lack. Accounts that sign in with a passkey by
  default need "Try another way". Security keys and phone prompts are untested.
- **FedCM.** `IdentityCredential` exists, but no FedCM account chooser has been
  tested in Alloy-style views. Sites using Google Identity Services with FedCM
  (One Tap, some buttons) may not show the chooser.
- **Google can change its policy** or its signals at any time, per region,
  account, or risk score. A captcha or "Verify it's you" page is reported as
  inconclusive (exit status 2), not as a pass. Re-run the test after engine
  updates; a lagging Chromium version is a likely future trigger for blocks.
- Background instances report `document.hasFocus() === false` and have no real
  input events. Agent clicks are untrusted DOM events, which Google currently
  accepts on its identifier page. Buttons drawn inside a cross-origin Google
  Identity Services iframe (Pinterest, Dropbox) cannot be clicked by page
  scripts at all.

Run it (it starts and stops its own instance on ports 7840 and 7841, with the
watchdog next to it):

```sh
python3 tools/test_google_signin.py            # all checks, needs the internet
python3 tools/test_google_signin.py --offline  # fingerprint and popup checks only
```

Exit status is 0 when all required checks pass, 1 on any failure (including the
insecure-browser block), and 2 when nothing failed but a network check was
inconclusive. `--output FILE` writes JSON; screenshots of non-passing checks are
kept in the printed profile directory. Google pages are requested in English
with `hl=en`; consent interstitials on `consent.*` hosts are dismissed with "Reject
all". Third-party sites are tried in order until one reaches Google
(`--sites` picks some).

## Product identity, About page, and notices

`src/brand.rs` holds the product name (`PRODUCT_NAME`, currently the placeholder
"Atrium") and bundle identifier (`BUNDLE_ID`). `atrium_bundle` writes them to
`Info.plist` (`CFBundleName`, `CFBundleDisplayName`, `CFBundleIdentifier`); the
window title, app menu, shell `<title>`, About page, and user-facing errors read
them too. The executable name, data directory (`~/.atrium-rs`), `ATRIUM_*`
variables, token header, and MCP tool names are separate constants that stay
fixed on rename; CONTRIBUTING.md lists them. Existing profile paths are
unchanged.

App menu > **About** opens `/about` from the local server in a tab (or selects
the existing one). It shows the app version, the Chromium and CEF versions the
binary was built against (`cef::sys::CEF_VERSION`), the project license
placeholder (`brand::PROJECT_LICENSE`, empty until chosen), the CEF and Chromium
licenses, the full `THIRD-PARTY-NOTICES.txt` (embedded at compile time), and a
link to `/about/chromium-credits`, which serves the `Chromium-CREDITS.html`
shipped in the bundle (without its `chrome://` stylesheets, so every license is
expanded). These pages are the only routes besides `/shell` that skip the API
token. They contain no user data, still require the expected Host and Origin,
run no script, and forbid framing.

`tools/gen_third_party_notices.py` regenerates `THIRD-PARTY-NOTICES.txt` from
`cargo metadata` for both macOS targets with default features: 168 crates are
compiled in and 50 are build-time only. Each crate's license and notice files are
copied verbatim, identical texts printed once. `--check` fails if the file is
stale. Review flags:

- `option-ext` 0.2.0 (MPL-2.0, file-level copyleft), compiled in through
  `dirs` → `dirs-sys`. Allowed unmodified in a differently licensed app; its
  source is on crates.io. Replacing `dirs::home_dir()` with `$HOME` would remove
  it.
- `webpki-roots` (CDLA-Permissive-2.0, Mozilla's root certificate data) and 19
  Unicode-3.0 crates (ICU4X and `unicode-ident`; 4 of them build time only):
  permissive data licenses whose texts are included.
- `libbz2-rs-sys` (bzip2-1.0.6), build time only (CEF download).
- 18 crates (`objc2` family, `block2`, `dispatch2`, `cef`, `cef-dll-sys`,
  `download-cef`) publish no license file; their MIT text is reconstructed from
  the license field and authors and should be checked against upstream.

No GPL, LGPL, or AGPL crate is present.

### Favicons

The shell no longer requests `google.com/s2/favicons` or Gmail's icon from
`ssl.gstatic.com`. CEF reports each page's declared favicon
(`on_favicon_urlchange`); the tab's browser downloads it as a favicon request
(no cookies), stores a PNG of at most 64×64 at `<data dir>/favicons/<host>.png`,
and sets the tab's `icon` to `/favicon/<host>?v=…`. The shell loads every icon from
`/favicon/<host>` (token-protected); hosts without a cached icon get a generic
SVG. Hosts are keyed like the shell's `host()` (lowercase, without `www.`), so
history, favorites, and pinned rows reuse icons of visited sites. Sites never
visited, and the WebKit backend, show the generic icon. Icons are not expired.

### Chromium background networking

A fresh profile showing only local pages still connects to Google: within a
minute, `lsof` showed TLS, QUIC, and HTTP connections from the network service
to Google address ranges, and `Local State` recorded a component updater check
(`updateclientdata`) and a network time fetch (`network_time`). Empty component
directories (CRLSet, PKI metadata, Widevine, Safe Browsing lists, optimization
hints, and others) appear under `chromium/`. This is Chromium behavior, not this
project's code. Appending `--disable-background-networking` and
`--disable-component-update` in `on_before_command_line_processing` did not
change it: both switches showed in `chrome://version`, yet the same connections
and Local State records appeared, so the switch was not kept. Stopping these
services needs further work (feature flags such as network time querying, or
policies), and doing so also stops Widevine and certificate revocation list
updates, which is a product decision.

In background mode, the `--disable-features=MacWebContentsOcclusion` switch
appears after CEF's own `--disable-features=…` list in `chrome://version`. Which
of the two lists Chromium honors was not checked.

Profile data (cookies, history, local storage) is written to `chromium/Default`;
the configured `chromium/profile` cache path stays empty under CEF 156's Chrome
bootstrap. Persistence works either way (`test_profile_persistence.py`).

### Verification

`tools/test_about.py` starts the built bundle in background mode
(`ATRIUM_MOCK_KEYCHAIN=1 ATRIUM_BACKGROUND_WINDOW=1`, fresh `--data-dir`, assistant
disconnected) with the watchdog, plus its own fixture server whose page declares
a PNG favicon:

```sh
python3 tools/test_about.py --port 7891 --fixture-port 7892 \
  --data-dir /private/tmp/atrium-about-1
```

It checks `Info.plist` (name, display name, identifier, usage strings) against
`src/brand.rs`; that `/about` is public but Host/Origin-checked, framing-denied,
and script-free, while `/favicon/*` needs the token; that the shell's title is
the product name and it has no Google favicon URLs; that App menu > About (sent
as the `menu` command) opens one reused tab showing the Cargo version, the CEF
version, and a Chromium version equal to the running engine's
`navigator.userAgentData` full version; that the page's notices and both
licenses match the repository files by SHA-256; that the credits link returns
the bundle's 5.3 MB `Chromium-CREDITS.html`; that a page's declared favicon is
fetched from that page's server, cached under `favicons/`, exposed as the tab's
`icon`, and served as a ≤64 px PNG; that unknown hosts get the local generic
icon. It also prints, without failing, the instance's non-loopback connections
and whether Local State recorded a component update or network time fetch.

Result (2026-10-24, `/private/tmp/atrium-legal-10`): all 21 checks passed; the
watchdog reported no frontmost or visible window. Five connections to Google
addresses were open, all from the browser process, and Local State recorded
both background services.

Not covered: the real menu click (background instances have no menu bar), the
sidebar's rendered icons (shell evaluation is disabled in release builds), icon
refresh when a site changes its favicon, and the WebKit backend.

PRIVACY.md lists every network destination found in the code; SECURITY.md
describes the local API's trust model.

## Passwords and passkeys

Alloy-style browsers do not get Chrome's password manager, so Atrium has its own
(`src/passwords.rs`, `assets/passwords.js`).

### Detection and filling

For every top-level page load the engine creates a CDP isolated world named
`atrium-passwords` and runs `assets/passwords.js` in it. The script finds
password fields, classifies each form as login, signup (`new-password` or a
confirmation field), or password change, and reports through a
`Runtime.addBinding` binding that exists only in that world. It reports
submissions from `submit` events, from clicks on submit-like buttons in
formless pages, and from Enter in a password field. Page scripts share the DOM
but not the isolated world's globals, so they cannot call the binding or the
fill function; the E2E test checks that both are `undefined` in the main world.
`Runtime.enable` is sent to every page tab, because binding calls are only
reported while the Runtime domain is enabled.

Stored passwords stay in the browser process until the user fills. On a page
with a login form, the shell shows a card that lists only the origin and the
saved usernames. A password is sent into the page only when the user clicks
"Fill" on that card (or through the token-protected `passwordFill` shell
command). Before filling, the origin is checked three times: the reported origin
must equal the origin of the tab's committed URL, the store returns only
entries whose origin is exactly equal, and the script checks `location.origin`
again before it writes the fields. Origins are compared as scheme, host, and
port (`url::Origin`, with punycode for international hosts), so
`http://localhost:7831` matches neither `http://127.0.0.1:7831` nor
`http://localhost:7832`. Subdomains, the public-suffix list, and lookalike
domains are never matched. Only `http` and `https` are accepted. Plain
`http` origins are allowed, so a saved password can be filled into an
unencrypted page if the user saved it there.

After a submit, the card offers Save, Update (same username with a new
password), or Never for the origin. Submitting an unchanged password shows
nothing. On signup forms the card offers a generated 20-character password that
mixes upper case, lower case, digits, and symbols without look-alike
characters. "Use password" fills every new-password field, and the normal save
offer follows on submit.

Limits:

- Only the top frame is handled. Login forms inside iframes, including
  cross-origin identity provider frames, are not detected.
- Nothing fills on page load. Every fill needs a click on the card.
- Once filled, the password is in the page's DOM, so page JavaScript can read it,
  just as it would after typing. The isolation only protects stored passwords
  that were not filled.
- Detection is heuristic (autocomplete attributes, field order, and button
  text), so unusual multi-step sign-in pages may not be detected.
- There is no management UI yet. Saved logins are managed through the API.

### Storage

In normal use each login is a macOS Keychain generic password under the service
`Atrium Passwords`. The account is a random id, the secret is the password, and
non-secret metadata (origin, username, timestamps) is kept in `kSecAttrGeneric`,
so listing logins never reads a secret. These items are in the login keychain,
whose access list is tied to the app's code signature. Ad hoc builds change
signature on every rebuild, so macOS asks again before reading saved passwords
after a rebuild, just like the "Chromium Safe Storage" prompt above. A stable
signing identity avoids this.

With `ATRIUM_MOCK_KEYCHAIN=1` the Keychain is never touched. Logins go to
`passwords-test.vault` in the data directory, encrypted with AES-256-CBC and
authenticated with HMAC-SHA256 (CommonCrypto). The key is in
`passwords-test.key`, next to the vault (mode 0600), so the vault only keeps
the file unreadable to casual inspection. It is for tests only. The
"Never" list is `password-never.json` in the data directory in both modes.

The management API is behind the same per-launch token and Host/Origin checks
as the rest of the local API:

- `GET /api/passwords` lists the store kind, logins without passwords, and the
  Never list.
- `POST /api/passwords/reveal {id}` returns one password.
- `POST /api/passwords/edit {id, username?, password?}` and
  `POST /api/passwords/delete {id}` change one login.
- `POST /api/passwords/never {origin, never}` edits the Never list.
- `POST /api/passwords/export` returns a Chrome-compatible CSV. Cells that
  start with `=`, `+`, `-`, or `@` get a leading `'` so spreadsheet apps do not
  run them as formulas.

Any process of the same user that can read the token can reveal or export
every password. That is the same trust boundary as the rest of the local API.
It is not separate protection.

### Chrome import

`POST /api/passwords/import-chrome {profile}` imports one Chrome profile
(`Default`, `Profile 1`, ...). It copies `Login Data` into a private temporary
directory and reads the copy with `/usr/bin/sqlite3 -readonly`, so Chrome's
file is never opened for writing. It then reads "Chrome Safe Storage" from the
Keychain and decrypts `v10` values (AES-128-CBC; PBKDF2-HMAC-SHA1 key with
salt `saltysalt` and 1003 rounds). Reading that key always shows a macOS
prompt, so the import only runs when the user asks for it. Blacklisted
("never saved") sites go to the Never list. Rows that cannot be decrypted or
that have a non-web origin are counted as `skipped`. Existing logins with the
same origin and username are updated.

Tests use `{profile_path, safe_storage}` with a generated profile and key
instead. The API accepts these only when `ATRIUM_MOCK_KEYCHAIN=1`.
Cookies and bookmarks are imported elsewhere.

### Passkeys

WebAuthn works in CEF's Alloy runtime. On a local fixture page,
`navigator.credentials.create` and `get` complete against the CDP virtual
authenticator (`WebAuthn.enable` with `enableUI: false`, then a CTAP2 internal
authenticator with resident keys and user verification). The fixture checks
the ES256 assertion signature, challenge, user handle, and UV flag with
WebCrypto, and the sign count increases. Tests reach CDP through
`POST /api/debug/cdp {tab, method, params}`, which needs the API token and only
allows `WebAuthn.*` methods and `Page.captureScreenshot`.

In the current ad hoc signed build,
`PublicKeyCredential.isUserVerifyingPlatformAuthenticatorAvailable()` returns
`false` and `isConditionalMediationAvailable()` returns `true`. Without the
virtual authenticator, real sites cannot use a platform passkey. What platform
passkeys would need, from the strings in the CEF 156 framework and Apple's
documentation (none of it tested here):

- Chromium's own Touch ID passkeys (stored in the Keychain by Chromium) need the
  app signed with a Developer ID team, a provisioning profile embedded in the
  bundle, and the keychain access group `<TEAMID>.org.chromium.Chromium.webauthn`
  in `keychain-access-groups`. The framework looks up this group name
  (with `.org.chromium.Chromium.webauthn-uvk` for its key). Ad hoc signing
  cannot carry restricted entitlements, so this is unavailable today.
- iCloud Keychain passkeys need Apple's managed entitlement
  `com.apple.developer.web-browser.public-key-credential`. Apple grants it to
  browsers on request to the Account Holder of an organization developer account. With it,
  Chromium uses `ASAuthorizationWebBrowserPublicKeyCredentialManager`, and macOS
  shows its own prompt asking the user to allow the browser to use their
  passkeys.
- Hybrid sign-in with a phone and USB/NFC security keys were not tested,
  because they may open Bluetooth or other system dialogs. Google Password
  Manager passkeys depend on Chrome Sync and are not available.

### Verification

`tools/test_passwords.py` starts the fixture (`tools/password_fixture.py`) on
two ports and a background instance with `ATRIUM_MOCK_KEYCHAIN=1`. It checks:

- the isolation globals are absent in the main world;
- save on submit, and explicit fill on revisit with fields empty before the fill;
- no offer and no fill on another port or on `127.0.0.1` vs `localhost`;
- Never, silent unchanged submits, and Update;
- capture on a formless page;
- generation on a signup form;
- synthetic Chrome import (wrong key rejected, Chrome profile unchanged,
  imported login fills);
- export, edit, delete, and the debug CDP allowlist;
- virtual-authenticator passkey registration and login;
- that the vault file has no plaintext password, and that the instance never
  became the frontmost app.

Recorded on October 24, 2026 (macOS 26, CEF 156): `python3
tools/test_passwords.py` passed. `cargo test --release --locked --offline` passed
19 tests. `ATRIUM_TEST_REAL_KEYCHAIN=1 cargo test --release --locked --offline
keychain_round_trip` also passed. That test round-trips the real Keychain under
a unique test service, with user interaction disabled so it fails instead of
prompting, and deletes its item. The smoke, tab lifecycle, and persistence
checks still pass. One persistence run hit the existing teardown watchdog on
exit and the next three passed.

## Releases, signing, and updates

The release procedure is in [RELEASING.md](../RELEASING.md).

### Signing

`tools/sign-app.sh APP` signs inside-out without `--deep`: the CEF libraries,
the framework, each helper with its own entitlements, then the app, and finally
runs `codesign --verify --deep --strict`. The bundler calls it, so every build is
signed this way. Helpers now get their own bundle identifiers
(`dev.atrium-rs.browser.helper`, `.helper.renderer`, `.helper.gpu`,
`.helper.plugin`, `.helper.alerts`); the CEF bundler had copied the app's.

| Bundle | Entitlements (`tools/entitlements/`) |
| --- | --- |
| Atrium.app | camera, audio input |
| Helper (Renderer) | allow-jit, allow-unsigned-executable-memory |
| Helper (GPU) | allow-jit |
| Helper (Plugin) | allow-unsigned-executable-memory, disable-library-validation |
| Helper (utility processes) | allow-jit, camera, audio input |
| Helper (Alerts) | none |

These follow Chromium's macOS helper entitlement split. Utility processes run
the audio and video capture services and V8 for proxy scripts, hence their set.

- `ATRIUM_SIGN_IDENTITY` set (a Developer ID Application identity): hardened
  runtime and a secure timestamp, as notarization requires.
  `ATRIUM_SIGN_KEYCHAIN` selects a keychain. No certificate or keychain is created
  by these scripts. **Not tested here**: no Developer ID identity is available.
- Unset: ad hoc, without the hardened runtime. Ad hoc code has no Team ID, so
  library validation under the hardened runtime rejects the ad hoc CEF framework
  (verified: `dlopen` fails with "different Team IDs"). `ATRIUM_HARDENED_RUNTIME=1`
  signs ad hoc with the runtime plus `disable-library-validation` for local
  experiments only; it is never applied with a real identity.

Verified on the ad hoc build: `codesign --verify --deep --strict` passes, and
`codesign -d --entitlements -` shows the table above for each bundle.

### Packaging and notarization

`tools/release.sh` builds (skip with `ATRIUM_SKIP_BUILD=1`), signs a copy as
`dist/stage/Atrium.app`, writes `Atrium-<version>-macos-<arch>.zip` (`ditto`) and,
when `ATRIUM_UPDATE_SECRET_KEY`/`_B64` is set, its `.sig`, builds a UDZO DMG with
an Applications link, signs the DMG with a real identity, then mounts it with
`hdiutil attach -nobrowse -noverify -noautoopen -readonly` at a private
`/private/tmp` mount point, verifies the app inside, detaches, and writes
`SHA256SUMS`. Notarization (`xcrun notarytool submit --wait`, then
`stapler staple` for the app and later the DMG) runs only with a real identity
and one complete credential set (keychain profile, API key, or Apple ID); it is
skipped with a message otherwise. Verified: an ad hoc run produced the zip and
DMG, mounted it invisibly, verified the signature, and left no mounts.
Notarization itself is untested (no credentials).

`.github/workflows/ci.yml` runs script checks, `cargo test`, the bundle build,
and signature/entitlement checks on macOS for every push and pull request.
`release.yml` runs on `v*` tags, imports the certificate into a temporary
keychain when the secrets exist, runs `tools/release.sh`, and publishes a GitHub
release. `actionlint` is not installed here; both files were checked with a
YAML parse and a structural check (every job has `runs-on` and steps, each step
exactly one of `run`/`uses`). Neither workflow has run on GitHub yet.

### Auto-update

`src/updater.rs` reads a GitHub "latest release" document from
`ATRIUM_UPDATE_FEED`, `"feed"` in `<data dir>/update.json`, or
`https://api.github.com/repos/<repo>/releases/latest`, where `<repo>` is
`"repo"` in `update.json` or `ATRIUM_UPDATE_REPO` at build time (set by the
release workflow). Feeds and downloads must be HTTPS except on loopback. The
release needs `Atrium-*-macos-<arch>.zip` and `<zip>.sig`.

The `.sig` is an Ed25519 signature over
`atrium-update-v1\nversion=…\nsha256=…\nsize=…\n`, so a signature cannot be
replayed for another version or archive (`src/update_sig.rs`). The public key is
compiled in from `assets/update-public-key.txt` or `ATRIUM_UPDATE_PUBLIC_KEY` at
build time. That file has **no key yet**, so normal builds report the updater as
disabled and never contact a feed. `atrium_sign_update keygen|public|sign|verify`
creates keys and signs archives.

`POST /api/update/apply` checks the feed and, for a newer semantic version,
downloads the signature and archive, verifies the signature, extracts with
`ditto`, and requires the same bundle identifier, matching `CFBundleVersion`, a
valid `codesign --verify --deep --strict`, and the same Team ID when the running
app has one. It then removes `com.apple.quarantine` (everything this app writes
is quarantined because of `LSFileQuarantineEnabled`; running quarantined code
makes Gatekeeper show a dialog) and moves the app to `<data dir>/updates/staged`.
`GET /api/update/status` and `POST /api/update/check` report state;
`POST /api/update/restart` quits and relaunches. Unless `"auto": false`, the app
checks a minute after start and every six hours, stages updates itself, and the
sidebar shows an "Update ready · Restart" row (also shown at launch when one is
staged).

At the next launch, before the browser starts, the old binary moves the current
bundle to `updates/previous`, renames the staged one into place, starts it, and
waits up to 120 seconds (`ATRIUM_UPDATE_CONFIRM_SECONDS`) for it to report a
loaded shell. If it exits or times out, the previous bundle is put back and the
old version continues. A version that keeps failing to confirm on unsupervised
launches is rolled back on its third. The previous version stays in
`updates/previous` after a successful update. Moves fall back to a `ditto` copy
across volumes. The app must be able to write its own location; installing into
an admin-owned `/Applications` as a standard user is not handled.

Verified with `tools/test_updater.py` against a local feed, on background-only
copies in `/private/tmp`, with a build carrying a test key:
- a release signed with another key and an archive changed after signing are
  both rejected with nothing staged;
- 0.2.0 is found, downloaded, verified, and staged;
- a separate run of the launch-time swap (staged 0.2.0, launch 0.1.0) installed
  0.2.0, which confirmed its start, and the supervisor exited; the same via
  `POST /api/update/restart` also ended with `"result": "updated"`;
- when the new version was killed during startup the previous version was
  restored and kept running (`"result": "rolled_back"`).

In the full run the downloaded 0.2.0 was still quarantined, Gatekeeper showed a
dialog on the user's screen and rejected it, and the updater rolled back. That
led to the quarantine removal above, covered by a unit test. The full test has
**not** been re-run since, and the "update ready" row has not been checked
visually. Re-run `tools/test_updater.py` (see RELEASING.md) before relying on
it.

### Crash reports

The bundle's `Contents/Resources/crash_reporter.cfg` enables Crashpad with no
`ServerURL`, so nothing is uploaded. CEF uses the root cache path,
`<data dir>/chromium`, as the Crashpad database (`new`, `pending`, `completed`).
`GET /api/crash-reports` lists the dumps there. Verified by navigating a
background tab to `chrome://crash`: a 660 KB minidump with `ptype=renderer` from
`atrium Helper (Renderer)` appeared in `<data dir>/chromium/pending`, and the
browser stayed up. That run listed the wrong folder (`Crashpad/`); the endpoint
was corrected afterwards and `tools/test_crash_reports.py` has not been re-run.

### Test instances

`tools/background_app.py` prepares bundle copies with `LSBackgroundOnly` and
re-signs them. Setting `ATRIUM_BACKGROUND_WINDOW=1` alone was not enough: an ad
hoc copy at a new path became frontmost once, because its policy is applied at
runtime after LaunchServices registered it as a regular app. The harness also
kills any test process that becomes frontmost, attaches the shared watchdog, and
refuses to start quarantined bundles.

## Browser commands

Find, zoom, saving, the page menu, shortcuts, downloads, and links from other
apps are implemented on the Chromium engine. The shell shows a find bar (next,
previous, match count, Escape), a zoom indicator, and a short downloads list
with progress, cancel, and reveal. Zoom is stored per host in
`<data dir>/zoom-levels.json`. One process owns each data directory
(`instance.lock`); a second launch posts its URLs to the running one. The app
bundle declares `http`/`https` and HTML document types. "Make Atrium the default
browser" calls `LSSetDefaultHandlerForURLScheme`, and only from a normal window.

`tools/test_browser_commands.py` drives a background instance
(`ATRIUM_MOCK_KEYCHAIN=1`, `ATRIUM_BACKGROUND_WINDOW=1`, a fresh data directory,
and the watchdog). On this build it passed: the shortcut map and commands
(new/close/reopen tab, location bar, reload, back, forward, tab numbers,
Ctrl+Tab, find, zoom, save), find match counts, per-host zoom restored after
restart, HTML/MHTML save, print-to-PDF, a PDF URL displayed as
`application/pdf` instead of a download, the context menu (open/copy/save link,
copy/save/open image, back/forward/reload, Inspect refused), download progress,
cancel, and reveal without launching Finder, an unchanged default-browser
handler, and URLs forwarded by a second launch and by `application:openURLs:`.

The same run did not open the system print dialog, the default-browser dialog,
or Finder, and it did not deliver real keystrokes. Those need a normal,
focused window. Context-menu Copy and Search were present and were not invoked.
PDF tabs use the same Alloy browser views as other pages. CEF rejects a
Chrome-style browser view once this Chrome-style window already has a browser,
so the PDF viewer is the Alloy document (`application/pdf` in the test, and the
file was not downloaded).

## Work needed before public distribution

- An update signing key in `assets/update-public-key.txt`, a Developer ID
  identity, and notary credentials; a full notarized release and update cycle.
- Engine security updates and default-browser registration.
- Extension permission management and tests against real required extensions.
- A visible crashed-page state in the shell, OAuth tests against real identity providers, a site
  permission prompt UI, downloads, printing, accessibility, and broader website compatibility.
- Password management, passkeys, account migration, and local API isolation
  beyond the same-user token.
- Full session history restoration (form state, `history.state`, per-entry scroll)
  and better media-aware eviction coverage.
- Real-world memory, responsiveness, battery, and long-running stability tests.

## References

- [CEF Rust bindings](https://github.com/tauri-apps/cef-rs)
- [CEF usage and process architecture](https://github.com/chromiumembedded/cef/blob/master/docs/general_usage.md)
- [Chromium process model and site isolation](https://chromium.googlesource.com/chromium/src.git/+/HEAD/docs/process_model_and_site_isolation.md)
