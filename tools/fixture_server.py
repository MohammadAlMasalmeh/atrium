#!/usr/bin/env python3
"""Local, deterministic browser workloads. No external services or personal data."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse
import argparse

# An OAuth-style pair: the popup reports to its opener, then closes itself.
OPENER = "addEventListener('message', e => { if (e.origin === location.origin) document.title = 'Opener got ' + e.data; });"
POPUP = "if (window.opener) { window.opener.postMessage('signed-in', location.origin); setTimeout(() => window.close(), 500); } else { document.title = 'Popup without opener'; }"
# Scrolls itself only on the first visit, so a later offset must have been restored by the browser.
TALL = "const seen = 'seen' + location.pathname.replace(/\\W/g, '_'); if (!document.cookie.includes(seen)) { document.cookie = seen + '=1; Path=/'; scrollTo(0, 1500); }"


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        path = urlparse(self.path).path
        draft = 'draft' in path
        # /frames/a.localhost,b.localhost embeds a page from each host, for cross-site subframes.
        hosts = path.split('/')[2].split(',') if path.startswith('/frames/') else []
        frames = ''.join(f'<iframe src="http://{host}:{self.server.server_port}/page/frame"></iframe>' for host in hosts)
        body = f'''<!doctype html><meta charset="utf-8"><title>Fixture {path}</title>
        <h1>Chromium fixture</h1><button id="increment">Increment</button><output id="count">0</output>
        <label>Draft<textarea id="draft"></textarea></label><a href="/next">Next page</a>
        {'<div style="height:6000px"></div>' if path.startswith('/tall') else ''}{frames}
        <script>
        window.fixtureMemory = new Uint8Array(12 * 1024 * 1024).fill(42);
        let count = 0; document.querySelector('#increment').onclick = () => document.querySelector('#count').value = ++count;
        {'document.querySelector("#draft").value = "Unsaved test draft";' if draft else ''}
        {OPENER if path.startswith('/opener') else POPUP if path.startswith('/popup') else TALL if path.startswith('/tall') else ''}
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
