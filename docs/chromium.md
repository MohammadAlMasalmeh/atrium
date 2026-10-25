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
read-only, and inventory installed extension names. They do not copy cookies,
passkeys, or Google sessions; passwords are imported separately (see Passwords
and passkeys). OpenCode connection checks validate the
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

## Work needed before public distribution

- Signed/notarized releases, engine security updates, default-browser registration,
  and update/rollback infrastructure.
- Extension permission management and tests against real required extensions.
- Renderer-crash recovery, complete popup/opener and OAuth behavior, permission
  prompts, downloads, printing, accessibility, and broader website compatibility.
- A password management UI, platform passkeys (signing and entitlements), account
  migration, and local API isolation beyond the same-user token.
- Per-tab history/scroll restoration and better media-aware eviction coverage.
- Real-world memory, responsiveness, battery, and long-running stability tests.

## References

- [CEF Rust bindings](https://github.com/tauri-apps/cef-rs)
- [CEF usage and process architecture](https://github.com/chromiumembedded/cef/blob/master/docs/general_usage.md)
- [Chromium process model and site isolation](https://chromium.googlesource.com/chromium/src.git/+/HEAD/docs/process_model_and_site_isolation.md)
