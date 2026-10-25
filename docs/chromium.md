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
The shell calls a loopback Axum server. Requests must use the expected Host and,
when present, the shell's Origin. Every route except `/shell` also requires a
per-launch secret in the `x-aside-token` header. The browser writes it to
`<data dir>/api-token` (mode 0600) at startup and removes it on exit. The native
side attaches the header only to requests from the shell's own browser for the
local API origin, so website tabs and external requests never carry it. `aside
mcp` and `aside ask` read the token from the data directory or `ASIDE_API_TOKEN`.
Any process running as the same user can still read that file, so this stops
websites and other users rather than hostile local software. The shell currently
shares the persistent request context with website views; it is not a separate
Chromium storage partition.

### Layout stability

Tab BrowserViews are children of the CEF window, whose default fill layout sizes
every child to the full window on each layout pass. The window delegate's
`on_layout_changed` re-applies tab bounds synchronously in that same pass, so
pages are never painted at full-window size between a resize, view creation,
or eviction and the next shell layout message. The shell's IPC is one `fetch`
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
this mode so the window still paints. Screenshots and evaluation work. The
selected tab reports `document.visibilityState === "visible"` and runs
animation frames (60–73 per second measured); background tabs report
`"hidden"`, as in a normal window. Keyboard focus, real input, and native
dialogs are not testable this way. Never use it for a real profile.

Verified on macOS 26 with the smoke, lifecycle, and persistence checks: the
frontmost application never changed to the test instance, the window reported
`alpha 0` and not on screen, and screenshots contained the rendered fixture.
`tools/window_audit.swift` (polls every 50 ms) also shows that, like any bundled
app started from its executable, the process checks in with LaunchServices as
a Regular app for about 50–250 ms before the code switches it to Prohibited.
A Dock icon can therefore flicker for a fraction of a second at launch. Setting
`LSUIElement` in the bundle's `Info.plist` (and `Regular` at runtime for normal
launches) would remove that; it has not been done.

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

Every script that calls the API uses `tools/aside_api.py`, which sends the
`x-aside-token` header. Pass the instance's `--data-dir` (its `api-token` file is
read) or `--token-file PATH`, or set `ASIDE_API_TOKEN`; that is the order of
precedence. The file is re-read per request, so restarts are handled. Scripts
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
python3 tools/test_profile_persistence.py --port 7792 --fixture http://127.0.0.1:7799
```

The smoke check covers navigation, page interaction, JavaScript isolation,
screenshots, rejection of a foreign Origin, and rejection of a request without
the token. With an appropriate synthetic
extension loaded, `--expect-extension` also checks its content script. The lifecycle
check opens clean and dirty pages, waits for eviction, checks draft retention,
and wakes a sleeping page. Rust unit tests cover eviction selection, migration
parsing, path validation, and endpoint validation:

```sh
CEF_PATH="$PWD/target/cef" PATH="$PWD/target/build-tools/bin:$PATH" cargo test --locked
```

## Performance measurements

### Method

`tools/bench_guarded.py` compares this build with the installed Aside without
showing anything on screen. Each repetition starts a fresh background CEF
instance and a fresh throwaway Aside profile, **one browser at a time**, and
waits (up to a limit) for the one-minute load average to drop first. Aside is
started with Chromium's `--headless` mode: it creates no windows, uses the
`Prohibited` activation policy, and still runs its own sidebar and toolbar
WebUI. Headless Chromium advertises `HeadlessChrome` and `navigator.webdriver`,
which websites treat as a bot (one rejected run got fewer ads and about a third
of the processes), so the baseline sends the normal windowed Chrome 154 user
agent and disables the automation flag. Its User-Agent Client Hints brands are
still empty. Every browser is watched by `tools/window_audit.swift` (polls every
50 ms; kills the process if it becomes frontmost, shows an on-screen window, or
stays a `Regular` app for more than one second) and by the shared watchdog.
Like any bundled app, both browsers check in as `Regular` for 50–250 ms at
launch before switching to `Prohibited`; the reports record this.

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

Not measured. `powermetrics` requires `sudo`, which the automated runs do not
use. Manual step: with the Mac on battery and other apps quiet, run
`sudo powermetrics --samplers tasks,cpu_power,gpu_power -i 5000 -n 60` while
each browser holds the same tabs, and compare the per-task energy impact of
each process tree.

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
- Quiet-machine runs of the guarded memory, responsiveness, and soak tests;
  battery measurement; diagnosis of the one-time CEF lock hang; and an answer
  on whether renderers outlive evicted pages on real sites.

## References

- [CEF Rust bindings](https://github.com/tauri-apps/cef-rs)
- [CEF usage and process architecture](https://github.com/chromiumembedded/cef/blob/master/docs/general_usage.md)
- [Chromium process model and site isolation](https://chromium.googlesource.com/chromium/src.git/+/HEAD/docs/process_model_and_site_isolation.md)
