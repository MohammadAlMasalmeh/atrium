#!/usr/bin/env python3
"""Local, deterministic browser workloads. No external services or personal data."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse
import argparse
import html
import json

# An OAuth-style pair: the popup reports to its opener, then closes itself.
OPENER = "addEventListener('message', e => { if (e.origin === location.origin) document.title = 'Opener got ' + e.data; });"
POPUP = "if (window.opener) { window.opener.postMessage('signed-in', location.origin); setTimeout(() => window.close(), 500); } else { document.title = 'Popup without opener'; }"
# Scrolls itself only on the first visit, so a later offset must have been restored by the browser.
TALL = "const seen = 'seen' + location.pathname.replace(/\\W/g, '_'); if (!document.cookie.includes(seen)) { document.cookie = seen + '=1; Path=/'; scrollTo(0, 1500); }"


# A hostile website: it probes for the shell's command channel and the local API.
# Results are left in window.attack for the test to read; nothing leaves the machine.
EVIL = '''<!doctype html><meta charset="utf-8"><title>Fixture attacker</title>
<h1>Attacker fixture</h1><a id="shell-link" href="__API__/shell">Shell</a>
<iframe id="shell-frame" src="__API__/shell" width="200" height="100"></iframe>
<script>
const API = __API_JSON__;
window.attack = { ipc: typeof window.ipc, asideShell: typeof window.asideShell, results: {}, done: false };
const probe = async (name, run) => {
  try { attack.results[name] = await run(); } catch (e) { attack.results[name] = 'blocked: ' + e.name; }
};
(async () => {
  await probe('tabs_cors', () => fetch(API + '/api/browser/tabs').then((r) => 'status ' + r.status));
  await probe('tabs_nocors', () => fetch(API + '/api/browser/tabs', { mode: 'no-cors' }).then((r) => r.type));
  await probe('command_nocors', () => fetch(API + '/api/browser/command', { method: 'POST', mode: 'no-cors',
    headers: { 'content-type': 'text/plain' }, body: JSON.stringify({ cmd: 'newTab', url: location.origin + '/page/evil-command' }) }).then((r) => r.type));
  await probe('tool_nocors', () => fetch(API + '/api/tool', { method: 'POST', mode: 'no-cors',
    body: JSON.stringify({ name: 'open_tab', args: { url: location.origin + '/page/evil-tool' } }) }).then((r) => r.type));
  await probe('shell_cors', () => fetch(API + '/shell').then((r) => 'status ' + r.status));
  attack.popup_file = window.open('file:///etc/hosts') !== null;
  attack.popup_web = window.open(location.origin + '/page/evil-popup') !== null;
  attack.done = true;
  // Agent scripts run in an isolated world, so results are also left on the DOM.
  document.documentElement.dataset.attack = JSON.stringify(attack);
})();
</script>'''


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        url = urlparse(self.path)
        path = url.path
        if path.startswith('/evil'):
            api = parse_qs(url.query).get('api', ['http://127.0.0.1:7777'])[0]
            return self.send(EVIL.replace('__API_JSON__', json.dumps(api)).replace('__API__', api.replace('"', '')).encode())
        if path.startswith('/cookies'):
            # Echoes the Cookie request header so tests can check what the browser sent.
            received = json.dumps({'host': self.headers.get('Host', ''), 'cookie': self.headers.get('Cookie', '')})
            return self.send(f'<!doctype html><meta charset="utf-8"><title>Cookies</title><pre id="received">{html.escape(received)}</pre>'.encode())
        draft = 'draft' in path
        # /frames/a.localhost,b.localhost embeds a page from each host, for cross-site subframes.
        hosts = path.split('/')[2].split(',') if path.startswith('/frames/') else []
        frames = ''.join(f'<iframe src="http://{host}:{self.server.server_port}/page/frame"></iframe>' for host in hosts)
        body = f'''<!doctype html><meta charset="utf-8"><title>Fixture {path}</title>
        <h1>Chromium fixture</h1><button id="increment">Increment</button><output id="count">0</output>
        <label>Draft<textarea id="draft"></textarea></label><a href="/next">Next page</a>
        <a id="new-window" href="/page/opened-by-link" target="_blank">Open in new tab</a>
        {'<div style="height:6000px"></div>' if path.startswith('/tall') else ''}{frames}
        <script>
        window.fixtureMemory = new Uint8Array(12 * 1024 * 1024).fill(42);
        let count = 0; document.querySelector('#increment').onclick = () => document.querySelector('#count').value = ++count;
        {'document.querySelector("#draft").value = "Unsaved test draft";' if draft else ''}
        {OPENER if path.startswith('/opener') else POPUP if path.startswith('/popup') else TALL if path.startswith('/tall') else ''}
        </script>'''.encode()
        self.send(body)
    def send(self, body):
        self.send_response(200)
        self.send_header('Content-Type', 'text/html; charset=utf-8')
        self.send_header('Cache-Control', 'no-store')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *_):
        pass

if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--port', type=int, default=7789)
    args = parser.parse_args()
    print(f'http://127.0.0.1:{args.port}', flush=True)
    ThreadingHTTPServer(('127.0.0.1', args.port), Handler).serve_forever()
