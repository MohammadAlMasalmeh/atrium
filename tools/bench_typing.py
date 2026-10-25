#!/usr/bin/env python3
"""Typing latency in a page: Chrome, Aside and this build, with real OS keystrokes.

Each browser gets a fresh profile and a frontmost window. A local page fills the
window with a textarea; type_keys.swift clicks into it and types through the HID
event tap. The page reports, per keystroke, the OS event to the start of the next
frame (rAF), and Chrome's Event Timing entries (input delay, handler time, time to
the presented frame) for keystrokes slower than 16 ms.

--background-sites first opens the ten real sites, so typing competes with them
the way it does in daily use.
"""
import argparse
import functools
import http.server
import json
import statistics
import subprocess
import threading
import time
from pathlib import Path

import bench_three_way as b3

ROOT = Path(__file__).resolve().parent
TYPER_SOURCE = ROOT / 'type_keys.swift'
TYPER = Path('/private/tmp/atrium-type-keys')


def serve(port):
    class Quiet(http.server.SimpleHTTPRequestHandler):
        def log_message(self, *args):
            pass
    handler = functools.partial(Quiet, directory=str(ROOT / 'fixtures'))
    server = http.server.ThreadingHTTPServer(('127.0.0.1', port), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def stats(values):
    if not values:
        return None
    values = sorted(values)
    pick = lambda q: round(values[min(len(values) - 1, int(q * len(values)))], 1)
    return {'n': len(values), 'median': round(statistics.median(values), 1), 'p95': pick(.95), 'max': round(values[-1], 1)}


def run(which, background, keys, interval_ms, page):
    scratch = b3.SCRATCH / f'typing-{which}'
    if which == 'chrome':
        b = b3.Chromium('Google Chrome', b3.CHROME, scratch)
    elif which == 'aside':
        b = b3.Chromium('Aside', b3.ASIDE, scratch)
    else:
        b = b3.Atrium(b3.args.app, b3.args.port, scratch)
    try:
        time.sleep(2)
        first = True
        if background:
            for site in json.loads(b3.args.sites.read_text()):
                b3.load_site(b, site, timeout=20)
                if first:
                    b.close_initial()
                    first = False
        handle = b.open(page)
        if first:
            b.close_initial()
        time.sleep(3)
        window = b3.front(b.pid)['window']
        click = (window['x'] + window['w'] * 0.6, window['y'] + window['h'] * 0.6)
        if not TYPER.exists() or TYPER.stat().st_mtime < TYPER_SOURCE.stat().st_mtime:
            subprocess.run(['swiftc', '-O', str(TYPER_SOURCE), '-o', str(TYPER)], check=True)
        typed = subprocess.run([str(TYPER), str(b.pid), str(keys), str(interval_ms), str(click[0]), str(click[1])],
                               capture_output=True, text=True, timeout=keys * interval_ms / 1000 + 30).stdout
        time.sleep(1)
        page_data = b.evaluate(handle, "({frames: window.__frames, slow: window.__lat, "
                                       "chars: document.getElementById('t').value.length, keydowns: window.__keys, focus: window.__focus, visibility: document.visibilityState})")
        frames = page_data['frames'] or []
        return {'browser': b.name, 'background_sites': background, 'typer': typed.strip(),
                'chars_received': page_data['chars'], 'keydowns_in_page': page_data['keydowns'], 'focus_events': page_data['focus'], 'keys_sent': keys, 'visibility': page_data['visibility'],
                'to_frame_ms': stats([f['to_frame'] for f in frames]),
                'input_delay_ms': stats([f['delay'] for f in frames]),
                'slow_keystrokes_over_16ms': len(page_data['slow'] or []),
                'event_timing_duration_ms': stats([e['duration'] for e in page_data['slow'] or []])}
    finally:
        b.stop()
        time.sleep(4)


if __name__ == '__main__':
    extra = argparse.ArgumentParser(add_help=False)
    extra.add_argument('--background-sites', action='store_true')
    extra.add_argument('--keys', type=int, default=150)
    extra.add_argument('--interval-ms', type=int, default=80)
    extra.add_argument('--page-port', type=int, default=7899)
    options, _ = extra.parse_known_args()
    server = serve(options.page_port)
    results = []
    try:
        for which in b3.args.browsers.split(','):
            result = run(which, options.background_sites, options.keys, options.interval_ms,
                         f'http://127.0.0.1:{options.page_port}/typing.html')
            results.append(result)
            print(json.dumps(result), flush=True)
            b3.args.output.write_text(json.dumps(results, indent=2) + '\n')
    finally:
        server.shutdown()
