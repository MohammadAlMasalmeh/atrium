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

## Accessibility

### Engine

CEF does not turn on Chromium's platform accessibility by itself; the embedder
must detect an assistive client and call `SetAccessibilityState`. The
application class (`src/chromium/mac.rs`) handles the attributes assistive
clients set on `NSApp`: `AXEnhancedUserInterface` (VoiceOver) and
`AXManualAccessibility` (Voice Control, Switch Control, and others). The UI
thread also reads `NSWorkspace.isVoiceOverEnabled` at startup and every ten
seconds. While any of these is on, every BrowserHost, including the shell and
browsers created later, is set to `STATE_ENABLED` (Chromium's complete mode, with
native `NSAccessibility` objects). When all of them turn off, the hosts are set to
`STATE_DISABLED`, as cefclient does. `ASIDE_FORCE_ACCESSIBILITY=1` keeps it on.
Without an assistive client, the window's native tree has no web areas, so
idle users don't pay for the accessibility tree.

Native focus order: the shell BrowserView comes first, then page views. A
`CefFocusHandler` turns `OnTakeFocus` (tabbing past either end of a document)
into an explicit move. Tab past the end of a page goes to the assistant panel if
it is open, otherwise to the start of the sidebar. Shift+Tab before the start of
a page goes to the end of the sidebar. In the shell, Tab from the last sidebar
control enters the page, and Shift+Tab from the first assistant control returns
to it. F6 toggles focus between the page and the sidebar's current tab. Escape
in a Glance page closes Glance. Shell commands are now sent one at a time and in
order, so a "focus the page" request cannot overtake the command that opens that
page. If the page is not visible yet, the request waits up to two seconds for the
shell to uncover it.

### Shell

- Landmarks: the sidebar is a navigation landmark, the page area is `main`, and
  the docked assistant is a complementary landmark. Headings mark the space name,
  Chats, and the chat title.
- Pinned and open tabs are `tablist`/`tab` with `aria-selected` on the active tab.
  Tab names include loading, sleeping, split view, and assistant activity. The
  two lists share one roving Tab stop. ArrowUp and ArrowDown move between rows
  and wrap, Home and End jump to the first and last row, Enter or Space activates
  a tab, and Delete or Backspace closes it (or unpins a pinned page). After a
  close, focus moves to the neighboring row. When a row is re-rendered, focus
  follows its id.
- Icon-only buttons have names and `aria-keyshortcuts`. Toggle buttons
  (Assistant, Pin, Spaces, Search/Ask) expose `aria-pressed`. The address
  control is a button named with the current URL. Start page rows, chat rows,
  the agent preview, and step groups (with `aria-expanded`) respond to the
  keyboard. Decorative SVG icons are hidden from assistive technology.
- The command bar and Glance are modal dialogs. The rest of the shell is
  `inert` while either is open. The command bar input is a combobox whose
  results are a grouped listbox with `aria-activedescendant`. Tab stays inside
  the dialog; in Glance it cycles through the header buttons and the Glance page.
  Escape, or running an action, returns focus to where it came from: a shell
  control, or the page when the dialog was opened from the page.
- Toasts and a hidden announcer are polite `status` regions. They announce when
  the assistant starts working, its reply (the first 280 characters), permission
  requests, and closed or unpinned tabs. Streaming text in the conversation log
  is not live, so it is not read token by token. The start page field no longer
  traps Tab when it is empty (Tab still switches Search/Ask while typing).
- Focus rings use `:focus-visible` with the accent color, and the address bar
  uses the existing accent glow. A hidden sidebar is `visibility: hidden`, so its
  controls leave the Tab order. Hiding it while focus is inside moves focus out.
- `prefers-reduced-motion: reduce` stops looping animations (spinners, pulses,
  shimmer) and transitions (sidebar slide, loading bar, toasts).
- Contrast: `--fg-2` and `--fg-3` were raised (light .66/.56, dark .7/.56). Link
  and mention text uses `--accent-text`, and selected rows and primary buttons use
  `--accent-strong` (AccentColor mixed toward black or white). Error text uses
  `--danger`. All text colors reach 4.5:1 on the card, menu, and field surfaces
  in light and dark mode, checked with the accent color the test build resolves.
  A light system accent color such as yellow can fall below that for accent text,
  because the mix ratio is fixed.

### Testing

`tools/test_accessibility.py` starts its own fixture server (port 7821),
background instance (port 7820, with `ASIDE_MOCK_KEYCHAIN=1`,
`ASIDE_BACKGROUND_WINDOW=1`, and `ASIDE_DEBUG_API=1`), and watchdog, then exits
non-zero on any failure:

```sh
python3 tools/test_accessibility.py --verbose
```

It uses two token-protected endpoints. They are enabled only in debug builds or
when `ASIDE_DEBUG_API=1` is set at launch:

- `POST /api/debug/cdp` `{tab: id | "shell" | null, method, params}` runs an
  allowlisted DevTools method (`Accessibility.*` tree reads,
  `Input.dispatchKeyEvent`, `Emulation.setEmulatedMedia`,
  `Emulation.setFocusEmulationEnabled`, `Runtime.evaluate`) through
  `Manager::cdp`.
- `POST /api/debug/accessibility` `{assistive?: bool, tree?: bool}` reports
  whether accessibility is enabled, VoiceOver state, and which view has native
  focus. `assistive` sends `AXManualAccessibility` to `NSApp` in process, the
  same path an assistive client uses. `tree` walks the window's
  `NSAccessibility` tree in process, which needs no TCC permission.

The test checks the shell's tree (landmarks, tab roles and selection, button
names and pressed states, live regions, dialog semantics, and that every button,
tab, link, and field is named). It drives the sidebar and command bar with real
key events from `Input.dispatchKeyEvent`. It checks native focus moving with the
focus command, F6, Tab, and Shift+Tab across view boundaries. It reads a fixture
page's tree (heading, button, labelled textarea, link) and checks that the native
tree has no web areas before an assistive client and, after one, has `AXWebArea`
nodes for both the shell and the page with `AXTabGroup`/`AXRadioButton` tabs. It
also checks reduced motion and contrast in light and dark mode under emulated
media, and that the instance never became frontmost. Background windows can't be
key, so the shell's document focus is emulated, and keys are injected through
DevTools rather than the window server.

### Needs a manual VoiceOver pass

This was not run with VoiceOver, because test instances must not start it or
request Accessibility permission. Before release, a person should check, in a
normal window with VoiceOver on (⌘F5):

- Chromium turns on within a moment of VoiceOver starting, and turns off after.
- VO+arrow navigation across the sidebar, page, and assistant, and the rotor's
  landmark, heading, and form lists.
- How tab rows are announced (selected state, position, the Delete hint), and the
  address, toggle, and dialog announcements.
- Toasts and assistant replies are spoken once, not repeatedly while text streams.
- F6, Tab, and Shift+Tab with real keyboard focus. The test used emulated
  document focus. In a real window, check that focus lands visibly after closing
  the focused tab and when an overlay closes.
- Glance, popups, native `<select>` model pickers, and the `prompt()` used to
  rename spaces.
- Increase Contrast, Reduce Motion, and Reduce Transparency system settings, and
  non-blue accent colors.

Not covered: Favorites can only be removed with a right-click, spaces can only be
renamed with a double-click, and Voice Control labels for page content are not
tested.

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

Profiling lead: with eight pages live, this build ran roughly 45 renderer
processes, most idle at about 38 MiB RSS each. That pattern matches
cross-site subframes (ads and embeds) receiving their own process under site
isolation. Its process count also stayed at 56 after two pages were evicted,
which should be checked for renderers outliving their closed BrowserViews.
Run benchmark instances with `ASIDE_MOCK_KEYCHAIN=1`.

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
  prompts, downloads, printing, a manual VoiceOver pass (see Accessibility), and
  broader website compatibility.
- Password management, passkeys, account migration, and local API isolation
  beyond the same-user token.
- Per-tab history/scroll restoration and better media-aware eviction coverage.
- Real-world memory, responsiveness, battery, and long-running stability tests.

## References

- [CEF Rust bindings](https://github.com/tauri-apps/cef-rs)
- [CEF usage and process architecture](https://github.com/chromiumembedded/cef/blob/master/docs/general_usage.md)
- [Chromium process model and site isolation](https://chromium.googlesource.com/chromium/src.git/+/HEAD/docs/process_model_and_site_isolation.md)
