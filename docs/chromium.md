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
per-launch secret in the `x-atrium-token` header. The browser writes it to
`<data dir>/api-token` (mode 0600) at startup and removes it on exit. The native
side attaches the header only to requests from the shell's own browser for the
local API origin, so website tabs and external requests never carry it. `atrium
mcp` and `atrium ask` read the token from the data directory or `ATRIUM_API_TOKEN`.
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
parsing, path validation, and endpoint validation:

```sh
CEF_PATH="$PWD/target/cef" PATH="$PWD/target/build-tools/bin:$PATH" cargo test --locked
```

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

Gmail is a normal website in the Chromium profile; actual Google account
sign-in and Chrome Sync have not been validated or implemented here.

### Installing extensions

The Extensions panel (puzzle button at the bottom of the sidebar, or
"Extensions" in the command bar) installs from the Chrome Web Store by
extension ID or store link (`chromewebstore.google.com` or
`chrome.google.com/webstore`). "Add to Chrome" on the store page is not
intercepted. The CRX is downloaded from
`clients2.google.com/service/update2/crx` with `prodversion` set to this
build's Chromium version (156.0.8078.48).

`src/crx.rs` verifies the package before anything is written to the profile:

- The file must be CRX3 (`Cr24`, version 3) with a well-formed header.
- Every RSA (PKCS#1 v1.5 SHA-256) and ECDSA (P-256 SHA-256) proof must verify
  over `"CRX3 SignedData\0"`, the signed header data, and the ZIP archive.
  Chromium's 1024-bit minimum is used for RSA because older store extensions
  (such as JSON Formatter) still have 1024-bit developer keys.
- The signed header's CRX ID must equal the first 16 bytes of SHA-256 of a
  proof's public key (mapped to `a`–`p`), and that ID must be the one requested.
- Store installs also need a valid proof from the Chrome Web Store publisher
  key.

The archive is unpacked (stored and deflate entries; absolute paths, `..`, and
more than 512 MiB are refused) into
`<data dir>/extensions/<id>/<version>-<random>`. The `_metadata` folder is
dropped because Chromium rejects reserved names in unpacked extensions. The
manifest gets the developer public key as `key`, so the unpacked load keeps the
store ID and its storage. `extensions/registry.json` records the version,
directory, enabled state, approved permissions, and update URL.

A new extension is off until its permissions are approved. The review card
lists API permissions, host access, and content-script sites in plain
language and highlights broad access. Optional permissions and the localized
name (`__MSG_…__`) come from the manifest. Approval stores a fingerprint of
the permissions, hosts, optional permissions, and matches. An extension whose
files on disk declare anything not in that fingerprint does not load.

### Restart to apply

CEF loads extensions only through `--load-extension` at process start, so
approve, enable, disable, remove, and updates take effect on the next launch.
The panel shows "Restart to Apply" when the set to load differs from the set
running. Restarting quits normally and relaunches the same executable with the
original arguments, environment, and working directory once the old process
has exited. Removed and replaced versions are moved to `extensions/.trash` and
deleted at the next startup. Chromium keeps a removed extension's storage
(`Local Extension Settings`, `DNR Extension Rules`, and similar folders in the
Chromium profile); reinstalling the same ID finds it again.

### Updates

"Check for Updates" queries each extension's update URL with the update2 GET
protocol (`response=updatecheck`, `acceptformat=crx3`, `x=id=…&v=…&uc`). A
newer version is downloaded, checked against `hash_sha256` when the response
has one, and verified like an install. If it adds no permissions, hosts, or matches, it replaces the current
version on the next restart. Otherwise it waits as a pending update, the old
version keeps running, and the panel asks for approval ("Approve Update" or
"Keep Current Version"). Self-hosted extensions can declare their own
`update_url`. There is no periodic background check.

### API

| Route | Purpose |
| --- | --- |
| `GET /api/browser/extensions` | Installed extensions, `restart_required`, and the developer-load permission summary |
| `POST /api/browser/extensions/install` | `{"source": id or store link}`; `{"crx_url": …}` for a self-hosted HTTP(S) CRX (no publisher proof) |
| `POST /api/browser/extensions/updates` | Run an update check |
| `POST /api/browser/extensions/<id>/<action>` | `approve`, `enable`, `disable`, `reject` (drops a pending update, or removes a never-approved install), `remove` |
| `GET /api/browser/extensions/<id>/icon` | Extension icon |
| `POST /api/browser/extensions/restart` | Quit and relaunch |

Developer loads with repeated `run --extension DIR` arguments still work. They
get every declared permission without approval and are listed read-only.
Startup records their summaries in `extension-permissions.json`, marks each
as new, changed, or unchanged, and logs new or changed broad-access grants.

### What extensions can do

Tabs are CEF Alloy-style BrowserViews inside a Chrome-style window, so
Chromium's extension system does not treat them as browser tabs. Verified with
CEF 156:

- **Works:** content scripts; MV3 background service workers;
  `declarativeNetRequest` (uBlock Origin Lite blocks ad and tracker requests);
  `chrome.storage`; `chrome.runtime`; `chrome.i18n`; extension pages at
  `chrome-extension://<id>/…`.
- **Routed:** windows or tabs that Chromium creates for an extension (for
  example `windows.create` from a service worker) open as normal Atrium tabs.
  The native window is hidden and closed and never shown.
- **Popups:** there is no toolbar action button. The panel's "Popup" and
  "Options" buttons open those pages in Glance. They run with extension APIs,
  but popups that act on "the current tab" find none. uBlock Origin Lite's
  popup loads, but its per-site controls stay empty.
- **Does not work:** `chrome.tabs.query` returns no tabs,
  `chrome.windows.getAll` returns none, and `chrome.tabs.create` fails with
  "No current window". Features built on those APIs (per-tab badges, popup
  state, `tabs.sendMessage` from the worker) are unavailable.
- **Untested:** `contextMenus` (Atrium has no extension context menu), action
  badges, and `chrome.identity`.
- `runtime.onInstalled` fires on every launch because Chromium treats
  command-line loads as fresh installs. Welcome tabs therefore reopen as Atrium
  tabs on each start for extensions that open them.

`tools/test_extensions.py` verifies this end to end with uBlock Origin Lite
(`ddkjiahejlhfcafbddmgiahcphecmpfh`), JSON Formatter
(`bcjindcccaagfpapjjmafapmmgkkhgoa`), and a synthetic self-hosted extension
whose second version adds `cookies` and whose service worker calls
`windows.create`.

## Work needed before public distribution

- Signed/notarized releases, engine security updates, default-browser registration,
  and update/rollback infrastructure.
- Extension tab and window APIs (Chrome-style tabs or a `tabs` shim), a toolbar
  action button with badges, "Add to Chrome" interception, periodic update
  checks, loading without a restart, and clearing storage on removal.
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
