#!/usr/bin/env python3
"""A page whose H.264 video Chromium rejects is offered in WebKit, and plays there.

Serves a local page with an H.264 <video> whose <source> names its codec, which the stock
CEF runtime rejects. (A background test window defers media loading, so Chromium never
reaches the decode error an untyped source gets in a visible window.) The shell should offer "Play in WebKit"; accepting it opens a WKWebView over the tab, where the
same page reports to this server that WebKit decoded the H.264 track. No external sites.
"""
import argparse
import json
import re
import urllib.parse
import tempfile
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from threading import Thread

import background_app

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / 'tools/fixtures/media'
PAGE = b'''<!doctype html><title>H.264 fallback</title>
<video id="v" muted playsinline autoplay><source src="/h264.mp4" type='video/mp4; codecs="avc1.42C01E"'></video>
<script>
const v = document.getElementById('v');
const note = (e) => fetch('/event?' + new URLSearchParams({ e, w: v.videoWidth, t: v.currentTime.toFixed(2),
  can: v.canPlayType('video/mp4; codecs="avc1.42C01E"'), ua: navigator.userAgent }), { cache: 'no-store' });
for (const e of ['loadedmetadata', 'canplay', 'playing', 'error']) v.addEventListener(e, () => note(e));
note('start');
</script>'''
played = []
requests = []


class Fixture(BaseHTTPRequestHandler):
    def do_GET(self):
        requests.append((self.path, self.headers.get('User-Agent', '')))
        if self.path == '/':
            body, mime = PAGE, 'text/html'
        elif self.path == '/h264.mp4':
            body, mime = (FIXTURES / 'h264.mp4').read_bytes(), 'video/mp4'
        elif self.path.startswith('/event'):
            played.append(dict(urllib.parse.parse_qsl(urllib.parse.urlsplit(self.path).query)))
            body, mime = b'ok', 'text/plain'
        else:
            self.send_error(404)
            return
        # WebKit's media stack only plays MP4 from servers that answer byte ranges.
        size, status, extra = len(body), 200, {'Accept-Ranges': 'bytes'}
        ranged = re.fullmatch(r'bytes=(\d+)-(\d*)', self.headers.get('Range', ''))
        if ranged:
            start = int(ranged[1])
            end = min(int(ranged[2]) if ranged[2] else size - 1, size - 1)
            body, status = body[start:end + 1], 206
            extra['Content-Range'] = f'bytes {start}-{end}/{size}'
        self.send_response(status)
        for key, value in extra.items():
            self.send_header(key, value)
        self.send_header('Content-Type', mime)
        self.send_header('Content-Length', str(len(body)))
        self.send_header('Cache-Control', 'no-store')
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


def wait(fn, label, timeout=30):
    end, last = time.monotonic() + timeout, None
    while time.monotonic() < end:
        last = fn()
        if last:
            return last
        time.sleep(.1)
    raise AssertionError(f'Timed out: {label}: {last}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--app', type=Path, default=ROOT / 'target/bundle/Atrium.app')
    parser.add_argument('--port', type=int, default=7854)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='atrium-webkit-', dir='/private/tmp') as folder:
        work = Path(folder)
        server = ThreadingHTTPServer(('127.0.0.1', 0), Fixture)
        Thread(target=server.serve_forever, daemon=True).start()
        instance = pid = None
        try:
            app = background_app.prepare(args.app.resolve(), work / 'Atrium.app')
            instance = background_app.Instance(app, work / 'profile', args.port, work / 'app.log',
                                               {'ATRIUM_TEST_SHELL_EVAL': '1', 'ATRIUM_NATIVE_CHROME': '0'})
            instance.start()
            pid = instance.wait_ready()
            client = instance.client

            def shell(js):
                result = client.request('/api/debug/shell', js, raw=True)
                assert not (isinstance(result, dict) and 'error' in result), result
                return result

            card = "const c = document.querySelector('#pw-card'); return c.hidden ? '' : c.textContent;"
            shell('window.atriumOnboarding && window.atriumOnboarding.close(); return 1')
            url = f'http://127.0.0.1:{server.server_port}/'
            client.tool('open_tab', {'url': url})
            # The agent opens tabs in the background; the offer shows for the active tab.
            tab = wait(lambda: next((t['id'] for t in client.api('/api/browser/tabs')['tabs'] if t['url'] == url), None), 'the tab')
            client.api('/api/browser/command', {'cmd': 'activate', 'id': tab})
            offer = wait(lambda: 'Play in WebKit' in shell(card) and shell(card), 'the WebKit offer')
            print('Offer:', offer, flush=True)
            chromium = [e for e in played if 'Chrome/' in e['ua']]
            assert all(e['e'] in ('start', 'error') for e in chromium), f'Chromium decoded H.264: {chromium}'

            shell("document.querySelector('#pw-card [data-a=webkit]').click(); return 1")
            webkit = lambda: [e for e in played if 'Chrome/' not in e['ua']]
            try:
                # A never-shown test window may hold playback back, but WebKit still demuxes
                # and sizes the H.264 track, which this Chromium build can't.
                meta = wait(lambda: next((e for e in webkit() if e['e'] in ('loadedmetadata', 'playing')), None), 'WebKit loading the video')
            except AssertionError:
                print('Events:', *played, 'Requests:', *requests, sep='\n  ')
                raise
            assert 'Safari' in meta['ua'] and meta['can'] == 'probably' and meta['w'] == '128', meta
            assert 'Back to Atrium' in shell(card), shell(card)
            print('WebKit events:', [e['e'] for e in webkit()], flush=True)

            shell("document.querySelector('#pw-card [data-a=chromium]').click(); return 1")
            wait(lambda: not shell(card), 'the card closing after switching back')
            print('PASS', flush=True)
        finally:
            if instance and pid:
                instance.quit(pid)
            server.shutdown()
            log = work / 'app.log'
            if log.exists():
                print(''.join(l for l in log.read_text().splitlines(True) if '[media]' in l), end='')


if __name__ == '__main__':
    main()
