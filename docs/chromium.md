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
CEF framework, sandbox helper executables, Chromium credits, and verified
signatures (see [Releases, signing, and updates](#releases-signing-and-updates)).
CEF updates require changing the pin, rebuilding, and re-running the
integration checks.

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

## Releases, signing, and updates

The release procedure is in [RELEASING.md](../RELEASING.md).

### Signing

`tools/sign-app.sh APP` signs inside-out without `--deep`: the CEF libraries,
the framework, each helper with its own entitlements, then the app, and finally
runs `codesign --verify --deep --strict`. The bundler calls it, so every build is
signed this way. Helpers now get their own bundle identifiers
(`dev.aside-rs.browser.helper`, `.helper.renderer`, `.helper.gpu`,
`.helper.plugin`, `.helper.alerts`); the CEF bundler had copied the app's.

| Bundle | Entitlements (`tools/entitlements/`) |
| --- | --- |
| Aside.app | camera, audio input |
| Helper (Renderer) | allow-jit, allow-unsigned-executable-memory |
| Helper (GPU) | allow-jit |
| Helper (Plugin) | allow-unsigned-executable-memory, disable-library-validation |
| Helper (utility processes) | allow-jit, camera, audio input |
| Helper (Alerts) | none |

These follow Chromium's macOS helper entitlement split. Utility processes run
the audio and video capture services and V8 for proxy scripts, hence their set.

- `ASIDE_SIGN_IDENTITY` set (a Developer ID Application identity): hardened
  runtime and a secure timestamp, as notarization requires.
  `ASIDE_SIGN_KEYCHAIN` selects a keychain. No certificate or keychain is created
  by these scripts. **Not tested here**: no Developer ID identity is available.
- Unset: ad hoc, without the hardened runtime. Ad hoc code has no Team ID, so
  library validation under the hardened runtime rejects the ad hoc CEF framework
  (verified: `dlopen` fails with "different Team IDs"). `ASIDE_HARDENED_RUNTIME=1`
  signs ad hoc with the runtime plus `disable-library-validation` for local
  experiments only; it is never applied with a real identity.

Verified on the ad hoc build: `codesign --verify --deep --strict` passes, and
`codesign -d --entitlements -` shows the table above for each bundle.

### Packaging and notarization

`tools/release.sh` builds (skip with `ASIDE_SKIP_BUILD=1`), signs a copy as
`dist/stage/Aside.app`, writes `Aside-<version>-macos-<arch>.zip` (`ditto`) and,
when `ASIDE_UPDATE_SECRET_KEY`/`_B64` is set, its `.sig`, builds a UDZO DMG with
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
`ASIDE_UPDATE_FEED`, `"feed"` in `<data dir>/update.json`, or
`https://api.github.com/repos/<repo>/releases/latest`, where `<repo>` is
`"repo"` in `update.json` or `ASIDE_UPDATE_REPO` at build time (set by the
release workflow). Feeds and downloads must be HTTPS except on loopback. The
release needs `Aside-*-macos-<arch>.zip` and `<zip>.sig`.

The `.sig` is an Ed25519 signature over
`aside-update-v1\nversion=…\nsha256=…\nsize=…\n`, so a signature cannot be
replayed for another version or archive (`src/update_sig.rs`). The public key is
compiled in from `assets/update-public-key.txt` or `ASIDE_UPDATE_PUBLIC_KEY` at
build time. That file has **no key yet**, so normal builds report the updater as
disabled and never contact a feed. `aside_sign_update keygen|public|sign|verify`
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
waits up to 120 seconds (`ASIDE_UPDATE_CONFIRM_SECONDS`) for it to report a
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
`aside Helper (Renderer)` appeared in `<data dir>/chromium/pending`, and the
browser stayed up. That run listed the wrong folder (`Crashpad/`); the endpoint
was corrected afterwards and `tools/test_crash_reports.py` has not been re-run.

### Test instances

`tools/background_app.py` prepares bundle copies with `LSBackgroundOnly` and
re-signs them. Setting `ASIDE_BACKGROUND_WINDOW=1` alone was not enough: an ad
hoc copy at a new path became frontmost once, because its policy is applied at
runtime after LaunchServices registered it as a regular app. The harness also
kills any test process that becomes frontmost, attaches the shared watchdog, and
refuses to start quarantined bundles.

## Work needed before public distribution

- An update signing key in `assets/update-public-key.txt`, a Developer ID
  identity, and notary credentials; a full notarized release and update cycle.
- Engine security updates and default-browser registration.
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
