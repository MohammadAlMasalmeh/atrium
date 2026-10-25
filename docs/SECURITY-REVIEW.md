# Local surface security review

Scope: the CEF build's local attack surface as of branch `feat/ipc` (from `c615f90`).
That covers the shell command channel, the loopback HTTP API and its auth, Host,
Origin, and token checks, CORS, DNS rebinding, file and path handling (frozen images,
setup and import), the agent's evaluate and screenshot tools, the MCP bridge,
OpenCode credentials, the injected page scripts, and extension loading.

Threats considered: hostile websites in any tab or frame, a page that steers the
assistant through prompt injection, other local user accounts, and DNS-rebinding
attackers. Hostile software running as the same user is out of scope. It can read
the token file and the Chromium profile.

Every fix has a regression check in `tools/test_secure_ipc.py` (end to end) or in a
Rust unit test (named below). See [chromium.md](chromium.md#shell-ipc-and-local-api-security)
for how to run them.

## Findings and fixes

| # | Finding | Severity | Status |
| --- | --- | --- | --- |
| 1 | Shell IPC rode on the token-bearing HTTP API; token attached to any request from the shell browser | High | Fixed |
| 2 | The shell browser could be navigated away while still carrying the token | High | Fixed |
| 3 | Website tabs and frames could load the shell's own origin | Medium | Fixed |
| 4 | Website popups were opened as browser-initiated tabs: any scheme, no user gesture | Medium | Fixed |
| 5 | The agent could be steered to `file:`, `chrome:`, `data:` and `javascript:` URLs | Medium | Fixed |
| 6 | Page titles were pasted unescaped into the assistant's system prompt | Medium | Fixed |
| 7 | A started OpenCode server had no password, so any local account could drive the browser through it | Medium | Fixed |
| 8 | Chat mentions and evaluation could target the shell itself | Low | Fixed |
| 9 | The data directory was world-readable (0755): session URLs, server address, profile | Low | Fixed |
| 10 | `aside mcp` sent the API token to whatever `--api` URL it was given | Low | Fixed |
| 11 | No framing, sniffing, caching, or referrer protections on local responses | Low | Fixed |
| 12 | OpenCode credentials embedded in a configured URL were shown in `/api/state` | Low | Fixed |
| 13 | Quitting left the token file behind and orphaned the OpenCode server Aside started | Low | Fixed |

### 1. Shell IPC over the HTTP API (High, fixed)

**Before:** `window.ipc.postMessage` was a `fetch` to `/api/browser/command`. A
`ResourceRequestHandler` added `x-aside-token` to every request from the shell's
browser to the API origin, whatever frame or document sent it. Ordering relied on
the shell keeping one layout request in flight.

**Fix:** the renderer helper (`src/bin/aside_helper.rs`) now has a
`RenderProcessHandler`. The browser process creates the shell's BrowserView with
extra info naming the shell URL, which a page cannot forge. The renderer defines
`window.ipc` (read-only, non-deletable, native) in `on_context_created` only when all
of these hold:

- the browser carries that marker;
- the frame is the main frame;
- the frame URL is exactly `<api>/shell`;
- the context is the frame's main world, so extension isolated worlds don't get it.

Each `postMessage(string)` call checks the calling frame again, then sends one
`CefProcessMessage` (`aside.shell.command`, at most 256 KiB). The browser-side
`Client::on_process_message_received` accepts it only when all of these hold:

- the client belongs to the shell;
- the source is a renderer;
- the browser identifier is the shell's (recorded in `on_after_created`);
- the frame is the main frame at the shell URL;
- the body is a JSON object.

Otherwise it drops and counts the message. Accepted commands enter the same FIFO
queue as every other UI event. Messages from one frame are delivered in order, so the
one-in-flight layout workaround and the shell's `fetch` fallback are removed. The
WebKit build has its own native `window.ipc`. The token is now attached only to
requests from the shell's main frame while it shows the shell document.

The HTTP route `/api/browser/command` remains for token holders (scripts, tests) and
is counted separately.

**Regression:** `test_secure_ipc.py` drives tab creation, switching, closing, layout,
the command bar overlay with its frozen backdrop, and Glance through the real shell UI.
It checks that `window.ipc` is native and non-writable, and that the `shell_ipc`
counters in `/api/browser/memory` show process messages only, with no HTTP command.
It also checks that 400 back-to-back layout messages apply in order (the last one
wins), and that a hostile page has no `window.ipc`. Unit test
`only_the_exact_shell_document_matches` covers the URL check.

### 2. Shell browser could be navigated away (High, fixed)

**Before:** a script in the shell, or a link or file dropped on the window, could
navigate the shell's browser to another page. That page would then get the native
token header on requests to the API, for example through no-cors `<img>` GETs.

**Fix:** a shell `RequestHandler::on_before_browse` cancels every navigation except
to the shell document. User-gesture navigations to http(s) URLs, such as a dropped
link, open as a normal tab instead.

**Regression:** `test_secure_ipc.py` sets `location.href` in the shell. The shell
stays on `/shell` with `window.ipc` intact, and no tab opens.

### 3. Tabs could load the shell origin (Medium, fixed)

**Before:** any tab, frame, or popup could load `http://127.0.0.1:<port>/shell`. That
page ran shell code without the token, but it shared the shell's origin and therefore
its `localStorage` (history, pins, spaces). The assistant's `evaluate` tool could then
read that storage.

**Fix:** non-shell browsers get a `RequestHandler` that cancels any main or subframe
navigation to the API origin. `/shell` also sends `X-Frame-Options: DENY` and
`frame-ancestors 'none'` (finding 11).

**Regression:** the hostile fixture tries a top-level navigation and an iframe to the
shell, and the agent tries `open_tab` with the shell URL. None of them loads the shell
document or gets `window.ipc`.

### 4. Website popups (Medium, fixed)

**Before:** `on_before_popup` cancelled every popup and reopened its URL with
`new_tab`. That made it a browser-initiated load, which skips Chromium's
renderer-side checks. A website could open `file:///…`, `chrome://…` or `devtools://`
URLs, and open tabs without any user gesture.

**Fix:** popups now become tabs only when Chromium reports a user gesture and the URL
is http(s). Agent actions (click, type, key, select, scroll) run as
`Runtime.evaluate` with `userGesture: true`, so links the assistant clicks still open.
Agent reads, probes, and the `evaluate` tool don't. Otherwise a page could use the
assistant's background reads to gain user activation.

**Regression:** the hostile fixture calls `window.open('file:///etc/hosts')` and
opens an http popup without a gesture; neither creates a tab. A page script that
clicks a `target=_blank` link is blocked, while the agent's `click` tool on the same
link opens it.

### 5. Agent navigation to non-web URLs (Medium, fixed)

**Before:** `navigate`, `open_tab`, `fetch`, and batch `navigate` passed any URL with
`://` through. A page could prompt-inject the assistant to open
`file:///Users/<me>/…`, read it with `read_page`, and send the text elsewhere.

**Fix:** agent tools only open http(s) URLs and `about:blank`. Plain words still
become a search. The shell's own address bar is unchanged, since it is the user's
action.

**Regression:** unit test `the_agent_only_opens_websites`. In `test_secure_ipc.py`,
the `navigate`, `open_tab`, `fetch`, and `batch` tools refuse `file:` and `chrome:`
URLs.

### 6. Prompt injection through the system prompt (Medium, fixed)

**Before:** the active tab's title and URL were inserted raw into the *system* prompt
as `"{title}" ({url})`. A page title could close the quotes and add "rules" with
system-prompt authority. Mentioned-tab text could also close its `<tab>` wrapper.

**Fix:** page-controlled strings are JSON-quoted, stripped of control characters, and
bounded (160 characters for titles, 300 for URLs). The prompt says the title is page
content. `</tab` inside page text is neutralized.

**Regression:** unit test `page_titles_cannot_break_out_of_the_system_prompt`. Page
text remains untrusted data, and the existing prompt instruction to ignore page
instructions still applies.

### 7. Unauthenticated OpenCode server (Medium, fixed)

**Before:** when Aside started `opencode serve`, it listened on loopback without a
password. Any local account could send prompts, and through the registered `aside` MCP
tools it could drive the user's logged-in tabs. That bypassed the Aside API token.

**Fix:** when Aside starts the server, it passes a random per-launch
`OPENCODE_SERVER_PASSWORD` (or the user's own, if set) and authenticates with it. A
pre-existing server without a password ignores the credentials, which was verified
against opencode 1.18.10.

**Regression:** `test_secure_ipc.py` (unless `--skip-opencode`) starts an instance that
launches its own OpenCode on port 7885. Aside reaches it, an unauthenticated request
gets 401, and the server stops with the browser. Unit test
`a_started_server_gets_a_password`.

### 8. Shell reachable from mentions and evaluation (Low, fixed)

**Before:** `/api/chat` evaluated `__aside.readPage` in any tab id it was given,
including the shell's. The manager also accepted evaluation and screenshots of the
shell.

**Fix:** mentions are filtered to existing web tabs (deduplicated, at most eight).
The manager refuses to evaluate or capture the shell unless shell scripting is enabled
(debug builds, or `ASIDE_TEST_SHELL_EVAL=1` for test instances).

**Regression:** unit test `mentions_cannot_reach_the_shell_or_scratch_pages`. In
`test_secure_ipc.py`, `/api/debug/shell` is refused on an instance without the test
variable.

### 9. World-readable data directory (Low, fixed)

**Before:** `~/.aside-rs` and the Chromium profile were created 0755. Saved tab URLs
(which can contain session tokens), the OpenCode address, and extension records were
readable by other accounts.

**Fix:** `aside run` sets the data directory to 0700, including one that already
exists. It leaves the home directory alone if that is passed as `--data-dir`. The
token file stays 0600.

**Regression:** `test_secure_ipc.py` checks a fresh profile and a pre-existing 0755
directory.

### 10. MCP bridge token destination (Low, fixed)

**Before:** `aside mcp --api URL` attached the token to requests to any URL.

**Fix:** `--api` must be `http://127.0.0.1`, `localhost`, or `[::1]`, with no
credentials.

**Regression:** unit test `the_token_is_only_sent_to_a_loopback_browser`. In
`test_secure_ipc.py`, a non-loopback `--api` exits with an error.

### 11. Response hardening (Low, fixed)

**Fix:** every local response, including 401 and 403 refusals, carries these headers:

- `X-Frame-Options: DENY`
- `Content-Security-Policy: frame-ancestors 'none'; object-src 'none'; base-uri 'none'; form-action 'none'`
- `X-Content-Type-Options: nosniff`
- `Referrer-Policy: no-referrer`, so favicon requests don't reveal the shell address
- `Cache-Control: no-store`

**Regression:** header checks in `test_secure_ipc.py`.

### 12. Credentials in OpenCode URLs (Low, fixed)

**Before:** a `--opencode-url http://user:pass@host` kept the credentials in the base
URL, which `/api/state` returns to the shell and error messages print.

**Fix:** credentials embedded in a URL are moved into the basic-auth settings and
removed from the base.

**Regression:** unit test `embedded_credentials_never_stay_in_the_base_url`. In
`test_secure_ipc.py`, `/api/state` doesn't contain `@`.

### 13. Cleanup skipped on quit (Low, fixed)

**Before:** exit cleanup removes `api-token` and stops the OpenCode child. It ran
after CEF's `shutdown()`, but Chromium's teardown watchdog often terminates the
process inside `shutdown()` ("Teardown watchdog expired" in the log). Every quit then
left a valid-looking token file behind, along with a running OpenCode server that had
the browser tools registered. The shutdown hang itself is older than this branch and
was not investigated.

**Fix:** cleanup runs after the message loop ends, when every browser has closed, and
before `shutdown()`.

**Regression:** `test_secure_ipc.py` quits through the API and checks that the token
file is gone. When it starts OpenCode, it checks that the server stops listening.

## Reviewed without changes

- **Host, Origin, token, CORS, DNS rebinding.** The server binds `127.0.0.1` only.
  Every route requires `Host` to be exactly `127.0.0.1:<port>`, so rebinding names,
  `localhost`, and missing ports get 403. When an `Origin` is present, it must be the
  shell's, so `null`, other ports, and other sites get 403. Every route except
  `/shell` needs the token, which is compared in constant time. There is no CORS
  layer, so preflights fail and browsers never expose responses. Blind no-cors POSTs
  from a page carry the page's Origin and no token, and are refused. All of this is
  re-checked end to end.
- **Frozen images.** These are in-memory JPEGs keyed by the fixed slots `main`,
  `split`, and `pip`. The path segment is only a map key, with no filesystem access.
  The route needs the token.
- **Setup and import paths.** Profile ids must be `Default` or `Profile N`, and are
  canonicalized and required to sit directly under Chrome's directory (symlink escape
  is covered by unit test). Files are size-capped at 32 MiB. History is read with
  `sqlite3 -readonly` and no shell. Locale names are restricted to alphanumerics.
  Only http(s) URLs are imported. Cookies, passwords, and Login Data are never opened.
- **OpenCode connect.** Remote servers need HTTPS, and URLs with credentials, a path,
  or a query are refused. Passwords stay in memory, redirects are disabled so basic
  auth is never forwarded, and only the address is saved.

## Accepted risks

- **Same-user local software.** It can read `<data dir>/api-token`, the Chromium
  profile, and the process environment, including the generated OpenCode password.
  The token stops websites and other accounts, not malware running as the user.
- **The assistant has full power over the pages it works in.** `evaluate` runs
  arbitrary JavaScript in the page's main world with the user's cookies, and
  `screenshot` captures any web tab, including hidden ones. This is the product.
  Mitigations are the system prompt rules (ask before irreversible actions, treat page
  content as data), findings 5 and 6, and OpenCode's own permission prompts.
  Prompt injection by page content can't be fully prevented.
- **Pages can see and tamper with the injected scripts.** `page.js` and
  `lifecycle.js` run in the page's main world. A page can detect Aside through
  `window.__aside`, pre-define or patch it (or the DOM APIs it uses) to show the
  assistant false content, and observe the assistant's actions on that page. It can
  also make `canSuspend()` return false to stay resident. None of this reaches other
  tabs, the shell, or the API. Moving these scripts to an isolated world through
  `Page.createIsolatedWorld` would stop the tampering, but not lying through the DOM.
- **Extensions are fully trusted.** Unpacked extensions given with `--extension`
  receive every permission they declare, and nothing enforces those permissions. An
  extension with host access to `http://127.0.0.1/*` can run content scripts in the
  shell page. The content scripts can't reach `window.ipc`, which exists in the main
  world only, but they can drive the shell's DOM. Their `fetch` calls from the shell
  page go through the shell's request handler and therefore carry the token.
  Startup logs broad grants and `/api/browser/extensions` lists them. Content scripts
  in the shell were not tested.
- **Remote OpenCode servers.** Connecting to a remote HTTPS server registers an MCP
  command line containing the local executable and data-directory paths on that
  server. The remote server can't reach the loopback API, so browser tools don't work
  there.
- **Downloads.** Any page can start a download, and Chromium then shows its save
  dialog (`show_dialog` = true). Nothing is written without the user choosing a
  location. This was not exercised in tests because it opens a system dialog.
- **Favicon privacy.** The shell loads favicons from `google.com/s2/favicons` for
  tabs, history, and links in chat. This tells Google the host names, but not full
  URLs or the shell address.
- **`openChrome` command.** Token holders and the shell can open an http(s) URL in
  Google Chrome. It is limited to http(s) and was not exercised by tests.
- **Shell test hook.** `ASIDE_TEST_SHELL_EVAL=1` re-enables `/api/debug/shell` in
  release builds for test instances. It still requires the token. Never set it for a
  real profile.
