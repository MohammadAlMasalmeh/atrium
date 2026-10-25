# Security

<!-- PRODUCT NAME: "Aside" below is a placeholder; update it when the project is renamed. -->

Aside is an **experimental developer browser**. It is not a security-maintained
release: there are no signed or notarized builds, no automatic updates, and its
Chromium version only changes when the pinned CEF version in `Cargo.toml` is
updated and the app is rebuilt. Do not rely on it for sensitive browsing.

## Reporting a vulnerability

<!-- TODO(maintainer): replace with the real contact before publishing. -->
Please report security issues privately, not in public issues:

- GitHub: use **Security > Report a vulnerability** on this repository
  (private vulnerability reporting must be enabled by the maintainer).
- Email: `SECURITY-CONTACT@EXAMPLE.INVALID` *(placeholder)*

Include the affected commit, steps to reproduce, and the impact you expect.
Vulnerabilities in Chromium itself should go to the
[Chromium security team](https://www.chromium.org/Home/chromium-security/reporting-security-bugs/);
CEF issues to the [CEF project](https://github.com/chromiumembedded/cef/security).
There is no bug bounty.

## What the local API trusts

The browser runs an HTTP server for its own UI and for automation. Anything that
can call it can control the browser as you, including running JavaScript in
signed-in tabs, so its checks matter:

- **Loopback only.** It listens on `127.0.0.1:<port>` (default 7777).
- **Host check.** Every request must carry `Host: 127.0.0.1:<port>`. This stops
  DNS-rebinding pages.
- **Origin check.** A request with an `Origin` header other than the server's own
  is rejected (403). This stops cross-site requests from websites.
- **Per-launch token.** Every route except `/shell`, `/about`, and
  `/about/chromium-credits` requires the `x-aside-token` header. The token is
  random for each launch, written to `<data dir>/api-token` with mode 0600, and
  removed on exit. The native side adds it only to requests from the shell's own
  browser to the local API, so website tabs never carry it. `aside mcp` and
  `aside ask` read it from the data directory or `ASIDE_API_TOKEN`.
- **Public routes** serve the shell's static HTML and the About page (versions and
  license texts). They contain no user data and set `X-Frame-Options: DENY` and a
  restrictive Content-Security-Policy (the About pages run no script). The favicon
  cache (`/favicon/<host>`) is not public, because it would reveal visited sites.
- **Release builds** disable `/api/debug/shell` (JavaScript evaluation in the shell).

**Trusted:** any process running as your macOS user. It can read the token file,
and it could read the Chromium profile directly anyway. The token stops websites
and other users, not hostile local software.

**Not trusted:** websites, including pages shown in assistant tabs. Page content
reaches the AI model as tool output; the system prompt tells the model to treat it
as data, but prompt injection is not a solved problem. Review what the assistant
does on sensitive sites. The assistant has the same browser powers as the API.

## Other boundaries and known gaps

- Chromium's renderer sandbox is enabled; no remote debugging port is opened.
- The shell shares the persistent Chromium request context with website tabs; it
  is not a separate storage partition.
- Unpacked extensions (`--extension`) run with every permission they declare.
  Their permissions are recorded and logged, not enforced or reviewed.
- An OpenCode server you connect is trusted with everything the assistant sees.
  Remote OpenCode endpoints must use HTTPS; credentials are never stored on disk.
- Missing before any public release: signed and notarized builds, timely engine
  security updates, an updater with rollback, extension permission management,
  password and passkey handling, and isolation of the local API beyond the
  same-user token. See `docs/chromium.md`.
