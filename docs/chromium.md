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
per-launch secret in the `x-aside-token` header. The browser writes it to
`<data dir>/api-token` (mode 0600) at startup and removes it on exit. The native
side attaches the header only to API requests from the shell's main frame while it
shows the shell document, so website tabs and external requests never carry it.
`aside mcp` and `aside ask` read the token from the data directory or
`ASIDE_API_TOKEN`. Any process running as the same user can still read that file,
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
after each build. Set `ASIDE_MOCK_KEYCHAIN=1` for throwaway test and benchmark
profiles to use Chromium's mock keychain instead; never use it for a real
profile. A stable signing identity is required to stop the prompt for
development builds of a real profile.

### Background test instances

`ASIDE_BACKGROUND_WINDOW=1` keeps a test instance out of the user's way. The
app uses the `Prohibited` activation policy, so it has no Dock icon or menu bar
and macOS does not let it become the active application. Its window stays
ordered in, so Chromium keeps a live compositor, but it is fully transparent,
has no shadow, ignores mouse events, is excluded from window cycling, and is
moved toward the screen edge (macOS keeps part of the frame on a display) and
ordered behind other windows. Chromium's macOS occlusion tracking is disabled in
this mode so the window still paints. Screenshots and evaluation work, and the
selected tab reports `document.visibilityState === "visible"`, so permission
prompts reach the app. Keyboard focus, real input, and native dialogs are not
testable this way. Never use it for a real profile.

Verified on macOS 26 with every integration check listed below: a watchdog
polling `lsappinfo front` never saw the test instance frontmost, the window
server listed its window at `alpha 0` with most of the frame off the display,
and screenshots contained the rendered fixture.

`tools/build-chromium.sh --release` creates a complete development app with the
CEF framework, sandbox helper executables, Chromium credits, and verified ad hoc
signatures. CEF updates require changing the pin, rebuilding, and re-running the
integration checks. There is no automatic updater or notarized distribution yet.

## Shell IPC and local API security

The shell's commands travel as CEF process messages, not HTTP:

- **Marking the shell.** The browser process creates the shell's BrowserView with
  `extra_info` naming the shell URL.
- **Exposing `window.ipc`.** The renderer helper (`src/bin/aside_helper.rs`) defines
  a read-only, native `window.ipc.postMessage(string)` only when the browser carries
  that marker, the frame is the main frame at exactly `<api>/shell`, and the context
  is the main world. Website tabs, frames, and extension worlds never see it.
- **Sending.** Each call sends one `aside.shell.command` message, at most 256 KiB.
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
- `aside run` makes the data directory 0700.
- When Aside starts `opencode serve`, it passes a random `OPENCODE_SERVER_PASSWORD`,
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
`ASIDE_TEST_SHELL_EVAL=1`, and it still needs the token. Use that variable only for
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

Every script that calls the API uses `tools/aside_api.py`, which sends the
`x-aside-token` header. Pass the instance's `--data-dir` (its `api-token` file is
read) or `--token-file PATH`, or set `ASIDE_API_TOKEN`; that is the order of
precedence. The file is re-read per request, so restarts are handled. It is
removed before Chromium shuts down, because Chromium's teardown watchdog
terminates a slow shutdown (seen on a heavily loaded machine) before later
cleanup would run. Scripts
that start their own instance (`test_profile_persistence.py`) read the token
from the profile they create and launch with `ASIDE_MOCK_KEYCHAIN=1` and
`ASIDE_BACKGROUND_WINDOW=1`.

Run the fixture server and an isolated background browser in separate terminals:

```sh
python3 tools/fixture_server.py --port 7799
ASIDE_MOCK_KEYCHAIN=1 ASIDE_BACKGROUND_WINDOW=1 \
  ./target/bundle/aside.app/Contents/MacOS/aside \
  --data-dir /private/tmp/aside-smoke run --port 7791 \
  --opencode-url http://127.0.0.1:1
python3 tools/smoke_chromium.py --api http://127.0.0.1:7791 \
  --data-dir /private/tmp/aside-smoke --fixture http://127.0.0.1:7799
python3 tools/test_tab_lifecycle.py --api http://127.0.0.1:7791 \
  --data-dir /private/tmp/aside-smoke --fixture http://127.0.0.1:7799
python3 tools/test_crash_recovery.py --api http://127.0.0.1:7791 \
  --data-dir /private/tmp/aside-smoke --fixture http://127.0.0.1:7799
python3 tools/test_popups.py --api http://127.0.0.1:7791 \
  --data-dir /private/tmp/aside-smoke --fixture http://127.0.0.1:7799
python3 tools/test_permissions.py --api http://127.0.0.1:7791 \
  --data-dir /private/tmp/aside-smoke --fixture http://127.0.0.1:7799
python3 tools/test_profile_persistence.py --port 7792 --fixture http://127.0.0.1:7799
python3 tools/test_history.py --port 7793 --fixture http://127.0.0.1:7799
python3 tools/investigate_renderers.py --port 7794 --fixture-port 7799 \
  [--real-sites tools/real_sites.json [--per-site] [--skip-fixtures]]
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
keychain, an invisible window, and a fresh profile, plus `ASIDE_TEST_SHELL_EVAL=1` for
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
- `aside mcp --api` to a non-loopback host.

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
python3 tools/compare_memory.py --data-dir /private/tmp/aside-bench \
  --output /tmp/aside-memory.json
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
python3 tools/compare_real_sites.py --data-dir /private/tmp/aside-bench \
  --output /tmp/aside-real-sites.json
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
Site isolation is left on. Reducing these processes needs fewer subframes
(for example content blocking) or evicting ad-heavy background pages sooner;
neither is implemented.
Run benchmark instances with `ASIDE_MOCK_KEYCHAIN=1`.

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
  no explicit choice Chrome uses its built-in default, so Aside keeps Google.
  Other keywords (site search shortcuts) are not imported.

`GET /api/setup/imported?history=N` returns the imported bookmarks (flat and per
profile tree) and the N most recent history entries with their visit times. The
shell does not show imported bookmarks or history yet; this API is the
integration point.

`POST /api/setup/import/cookies` is a separate request that only runs with
`{"profile": ..., "consent": true}` (optionally `"domains": ["example.com"]` to
limit it to those sites and their subdomains). Without consent it returns
`consent_required` and the security notice and does nothing. **Security:**
imported cookies are live sign-in sessions. Anything that can read the Aside
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

Test instances (`ASIDE_MOCK_KEYCHAIN` set) never read the user's Chrome
directory or login keychain. They see only `ASIDE_TEST_CHROME_ROOT` (otherwise no
profiles). `ASIDE_TEST_CHROME_SAFE_STORAGE` supplies the cookie password and is
honored only when `ASIDE_MOCK_KEYCHAIN=1`. Without it, the cookie import fails
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
  other Chromium builds send. An `Aside` brand was not added: CEF has no setting
  for it, and per-tab DevTools emulation would not cover service and shared
  workers, giving pages two inconsistent answers, which is itself a bot signal.
  There is no `Google Chrome` brand and no `X-Client-Data` header, because this
  is not Google Chrome. The user agent keeps Chromium's standard reduced string
  (macOS 10_15_7 and a frozen minor version are Chromium's own reduction).
- **Injected scripts.** `assets/page.js` and `assets/lifecycle.js` used to define
  `window.__aside` and `window.__asideLifecycle` in the page's main world, where
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

## Work needed before public distribution

- Signed/notarized releases, engine security updates, default-browser registration,
  and update/rollback infrastructure.
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
