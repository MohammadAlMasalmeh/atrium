# Privacy

<!-- PRODUCT NAME: "Atrium" below is a placeholder; update it when the project is renamed. -->

This document describes what Atrium, an experimental developer browser, sends over
the network and stores on disk. It is based on reading the source code in this
repository (October 2026). It covers this project's code; Chromium, OpenCode, AI
model providers, and websites have their own behavior and policies.

**Summary:** this project's code has no telemetry, analytics, crash reporting,
accounts, or app update checks. It talks to the websites you open, to the OpenCode
server you connect, and to its own loopback API. When you use the assistant, page
content is sent to OpenCode, which forwards it to whatever AI model provider
OpenCode is configured to use. **Chromium itself contacts Google in the
background** (component updates and network time), and there is no setting to turn
this off yet; see below.

## Network connections made by this project's code

| What | Destination | When |
| --- | --- | --- |
| Websites | The sites you (or the assistant) open | Normal browsing |
| Favicons | The icon URL a page declares, usually the same site | After a page reports its favicon |
| Address bar search | `https://www.google.com/search?q=…` | When the typed text is not a URL |
| Assistant search tool | `https://html.duckduckgo.com/html/?q=…` | When the assistant calls `atrium_search` |
| Assistant fetch tool | Any URL the model asks for | When the assistant calls `atrium_fetch` |
| OpenCode | `http://127.0.0.1:4097` by default, or the endpoint you configure | At startup and while the assistant is used |
| Local API | `http://127.0.0.1:<port>` (default 7777) | Shell, `atrium mcp`, and `atrium ask` |
| Chromium background services | Google (component updater and network time servers) | Shortly after startup and periodically |

Nothing else in the Rust code or the browser shell opens a network connection.
The only HTTP clients in the Rust code talk to the OpenCode endpoint and to the
loopback API. There is no app update checker, no download of Chrome Web Store
extensions, and no remote configuration.

### Websites and Chromium

Pages load in Chromium (CEF 156) with a persistent profile, so websites receive
what any Chromium browser sends: your IP address, cookies for that site, the user
agent, and so on. This project does not configure Google API keys, sign-in, sync,
or CEF crash reporting (the bundle has no `crash_reporter.cfg`), and it opens no
remote debugging port.

### Chromium's background connections to Google

CEF 156 runs Chromium's browser services, and two of them contact Google on their
own. Within a minute of starting a fresh profile showing only local pages, the
browser opened TLS and QUIC connections to Google address ranges (one reverse-resolves
to `*.1e100.net`) and a plain HTTP connection to a Google Cloud download host
(`*.googleusercontent.com`), and
`chromium/Local State` then recorded:

- a **component updater** check (`updateclientdata`). The component updater asks
  Google which data components are current and downloads them: certificate
  revocation lists (CRLSet), Certificate Transparency and PKI metadata, the
  Widevine DRM module, origin trial keys, Safe Browsing and subresource-filter
  lists, optimization hints, an on-device search suggestion model, and similar.
  Update requests carry Chromium's version, OS and architecture, the component
  IDs and versions, and update-cohort identifiers, as in Chrome.
- a **network time** query (`network_time`), which fetches the current time from
  Google to validate certificates when the system clock looks wrong.

Chromium's usual `--disable-background-networking` and `--disable-component-update`
switches do not stop either service in CEF 156: with both switches present on the
browser's command line (confirmed in `chrome://version`), the same connections
and Local State records appeared. Turning these services off will need more work
on Chromium's feature flags or policies. Doing so has costs: Widevine would not be
installed (DRM video and music sites would not play), and certificate revocation
lists and other security data would not be updated between CEF releases.
`tools/test_about.py` prints the connections it sees (`lsof` across the browser's
process tree while only loopback pages are open) without failing on them. This
was observed on macOS 26 with a fresh profile; it is a point-in-time check, not a
complete audit of Chromium's network behavior.

### Favicons

The sidebar used to load every site's icon from Google's favicon service
(`https://www.google.com/s2/favicons?domain=…`) and Gmail's icon from
`ssl.gstatic.com`, which told Google every site in your tabs, history, and
favorites. That is removed. Now, when a page declares a favicon, the browser
downloads that URL itself as a favicon request (Chromium sends and stores no
cookies for it), converts it to a PNG of at most 64×64 pixels, and saves it as
`<data dir>/favicons/<host>.png`. The shell loads icons only from the local API;
sites without a cached icon get a generic globe. The cache is never uploaded. To
clear it, delete the `favicons` folder.

### Search

Text typed in the address bar that is not a URL is searched with Google. The
assistant's search tool uses DuckDuckGo's HTML endpoint. Both load in Chromium with
your profile's cookies for those sites, like a normal visit. Neither is configurable
yet.

### The assistant (OpenCode and AI providers)

The assistant is an [OpenCode](https://opencode.ai) server. By default the app
reuses a server at `http://127.0.0.1:4097` or starts `opencode serve` there, with
`<data dir>/workspace` as its project directory. You can connect another server
through the setup API; remote servers must use HTTPS. Only the endpoint address is
saved (`opencode-server.json`); a server password comes from
`OPENCODE_SERVER_PASSWORD` or is kept in memory.

When you send a message, the app sends OpenCode:

- your message and the model you selected;
- a system prompt with the title and URL of the tab you are viewing and your
  local date, time, and time zone;
- the text (up to 12,000 characters each) of tabs you @-mention.

While working, the assistant uses browser tools whose results go back to OpenCode:
page snapshots and text, JavaScript results, screenshots (JPEG), the list of open
tab titles and URLs, and pages it fetches or searches. Hidden assistant tabs use
your browser profile, so they are signed in wherever you are.

**OpenCode forwards all of this to the AI model provider it is configured to
use** (for example OpenCode's hosted models or a provider whose API key you gave
OpenCode). This app does not choose the provider. OpenCode also stores sessions
itself and may make its own network requests; see OpenCode's documentation.
Browsing works without OpenCode. Until you use the assistant, the app only asks
OpenCode for its health, models, and session list, and registers the browser
tools (the `atrium mcp` command line, which includes the data directory path).

### Chrome import and extensions

Setup routes can read, on request and read-only, a local Google Chrome profile's
bookmarks, recent history (up to 400 entries), and installed extension names. They
never read cookies, passwords, or other credential stores, and nothing is sent over
the network. Extension entries include a `chromewebstore.google.com` link that is
built locally and not fetched. Unpacked extensions you load with `--extension` run
with every permission their manifest declares and can send data wherever those
permissions allow.

## Data stored on disk

Everything is under the data directory: `~/.atrium-rs` by default, or `--data-dir`.

| Path | Contents |
| --- | --- |
| `chromium/Default` | Chromium profile: cookies, cache, site storage, Chromium's own history and favicon databases, and the shell's local storage (browsing history list, favorites, pinned tabs, spaces, chosen model) |
| `chromium/` (other) | Chromium's `Local State`, GPU caches, and downloaded components |
| `chromium-tabs.json` | URLs and titles of open tabs, for session restore |
| `favicons/` | Cached site icons, named by host |
| `opencode-server.json` | The OpenCode endpoint address |
| `extension-permissions.json` | Permissions declared by loaded unpacked extensions |
| `api-token` | Per-launch local API secret (mode 0600), deleted on exit |
| `workspace/` | OpenCode's working directory |

Chromium encrypts cookies with a key stored in the macOS login keychain as
"Chromium Safe Storage" (test instances use `ATRIUM_MOCK_KEYCHAIN=1` instead). To
remove all browser data, quit the app and delete the data directory. OpenCode's
own session storage is outside it.

## Changes

This document must be updated with any change that adds a network destination or
stores new data. If you find a connection that is not listed here, please report it
(see [SECURITY.md](SECURITY.md)).
