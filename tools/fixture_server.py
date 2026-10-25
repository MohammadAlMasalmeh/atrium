#!/usr/bin/env python3
"""Local, deterministic browser workloads. No external services or personal data."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse
import argparse
import json

# A hostile website: it probes for the shell's command channel and the local API.
# Results are left in window.attack for the test to read; nothing leaves the machine.
EVIL = '''<!doctype html><meta charset="utf-8"><title>Fixture attacker</title>
<h1>Attacker fixture</h1><a id="shell-link" href="__API__/shell">Shell</a>
<iframe id="shell-frame" src="__API__/shell" width="200" height="100"></iframe>
<script>
const API = __API_JSON__;
window.attack = { ipc: typeof window.ipc, atriumShell: typeof window.atriumShell, results: {}, done: false };
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
})();
</script>'''


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        url = urlparse(self.path)
        path = url.path
        if path.startswith('/evil'):
            api = parse_qs(url.query).get('api', ['http://127.0.0.1:7777'])[0]
            body = EVIL.replace('__API_JSON__', json.dumps(api)).replace('__API__', api.replace('"', '')).encode()
        else:
            draft = 'draft' in path
            body = f'''<!doctype html><meta charset="utf-8"><title>Fixture {path}</title>
        <h1>Chromium fixture</h1><button id="increment">Increment</button><output id="count">0</output>
        <label>Draft<textarea id="draft"></textarea></label><a href="/next">Next page</a>
        <a id="new-window" href="/page/opened-by-link" target="_blank">Open in new tab</a>
        <script>
        window.fixtureMemory = new Uint8Array(12 * 1024 * 1024).fill(42);
        let count = 0; document.querySelector('#increment').onclick = () => document.querySelector('#count').value = ++count;
        {'document.querySelector("#draft").value = "Unsaved test draft";' if draft else ''}
        </script>'''.encode()
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
