#!/usr/bin/env python3
"""Verify a sleeping tab wakes behind a snapshot of how it looked until its page has loaded."""
import argparse
import json
import subprocess
import tempfile
import time
import urllib.error
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from threading import Event, Thread

import atrium_api

ROOT = Path(__file__).resolve().parent.parent
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--app', type=Path, default=ROOT / 'target/bundle/Atrium.app/Contents/MacOS/atrium')
parser.add_argument('--port', type=int, default=7841)
args = parser.parse_args()
gates = {}


class Fixture(BaseHTTPRequestHandler):
    def do_GET(self):
        gate = gates.get(self.path)
        if gate:
            gate.wait(20)
        body = f'''<!doctype html><title>Wake {self.path}</title>
          <style>body{{margin:0;background:coral}}section{{height:2000px}}
          section+section{{background:royalblue}}</style>
          <section>{self.path}</section><section>Saved scroll position</section>
          <script>if (!sessionStorage.seen) {{ sessionStorage.seen = 1; scrollTo(0, 2400); }}</script>'''.encode()
        self.send_response(200)
        self.send_header('Content-Type', 'text/html')
        self.send_header('Cache-Control', 'no-store')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def log_message(self, *_):
        pass


def wait(fn, label, timeout=15):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        try:
            result = fn()
            if result:
                return result
        except (urllib.error.URLError, TimeoutError, ConnectionError):
            pass
        time.sleep(.05)
    raise AssertionError('Timed out: ' + label)


def check(ok, label):
    assert ok, label
    print('PASS:', label, flush=True)


with tempfile.TemporaryDirectory(prefix='atrium-wake-', dir='/private/tmp') as directory:
    profile = Path(directory)
    fixture = ThreadingHTTPServer(('127.0.0.1', 0), Fixture)
    Thread(target=fixture.serve_forever, daemon=True).start()
    log = (profile / 'app.log').open('w')
    app = subprocess.Popen([str(args.app.resolve()), '--data-dir', directory, 'run', '--port', str(args.port),
                            '--opencode-url', 'http://127.0.0.1:1'], stdout=log, stderr=log,
                           env={**atrium_api.instance_env(), 'ATRIUM_TEST_SHELL_EVAL': '1',
                                'ATRIUM_TEST_MEMORY_PRESSURE': '1', 'ATRIUM_DEBUG_API': '1', 'ATRIUM_NATIVE_CHROME': '1'})
    client = atrium_api.Client(f'http://127.0.0.1:{args.port}', data_dir=profile)
    base = f'http://127.0.0.1:{fixture.server_port}'

    def memory():
        return client.api('/api/browser/memory')

    def tab(id):
        return next(t for t in memory()['tabs'] if t['id'] == id)

    def shell(js):
        result = client.request('/api/debug/shell', js, raw=True)
        assert not (isinstance(result, dict) and 'error' in result), result
        return result

    def opened(path):
        client.command(cmd='newTab', url=base + path, background=False)
        id = wait(lambda: next((t['id'] for t in client.api('/api/browser/tabs')['tabs']
                               if t['url'] == base + path), None), 'tab created')
        wait(lambda: not next(t for t in client.api('/api/browser/tabs')['tabs'] if t['id'] == id)['loading'], 'loaded')
        return id

    def waking():
        return shell('return Object.keys(__shell.S.waking || {}).map(Number);')

    def covered(id):
        """The shell shows the tab's snapshot while its live view loads off screen."""
        state = memory()
        page = next(t for t in state['tabs'] if t['id'] == id)
        main = state['layout']['main']
        shown = shell(f"""const img = [...document.querySelectorAll('img.waking')]
            .find(i => i.style.display !== 'none' && i.src.includes('/frozen/'));
            return !!img && {id} in (__shell.S.waking || {{}});""")
        return shown and page['view'] and page['view']['x'] > main['x'] + main['w']

    def in_place(id):
        state = memory()
        view = tab(id)['view']
        return (id not in waking() and view and not view['hidden']
                and view['x'] == state['layout']['main']['x'])

    def spinner(id):
        return memory()['native_chrome']['loading'] or shell(f'return __shell.S.tabs.find(t => t.id === {id}).loading;')

    try:
        wait(lambda: memory().get('shell_ready'), 'shell ready', 60)
        shell('window.atriumOnboarding?.close(); return true;')
        first = opened('/first')
        second = opened('/second')
        third = opened('/third')
        fourth = opened('/fourth')
        control = opened('/control')
        opened('/filler')
        client.command(cmd='activate', id=control)
        print('Waiting for real policy eviction (minimum idle: 30 seconds)', flush=True)
        wait(lambda: all(tab(id)['suspended'] for id in (first, second, third, fourth)), 'policy eviction', 85)
        wait(lambda: memory()['live_renderers'] <= 2, 'renderer budget')
        check(True, 'memory pressure still releases renderers to the two-tab budget')

        # The first page's response stalls, so its wake stays in progress.
        gates['/first'] = Event()
        client.command(cmd='activate', id=first)
        wait(lambda: covered(first), 'first wake cover')
        check(shell("""const img = [...document.querySelectorAll('img.waking')].find(i => i.style.display !== 'none');
            return img.complete && img.naturalWidth > 100;"""), 'the snapshot taken before sleeping is shown')
        check(not spinner(first), 'wake shows no loading spinner in the toolbar or sidebar')
        client.command(cmd='activate', id=control)
        wait(lambda: not covered(first) and in_place(control), 'switch away')
        client.command(cmd='activate', id=first)
        wait(lambda: covered(first), 'return to waking tab')
        time.sleep(.3)
        check(covered(first), 'the snapshot survives rapid tab switches and a stalled response')
        gates['/first'].set()
        wait(lambda: in_place(first), 'loaded handoff', 5)
        check(not spinner(first), 'the live page replaces the snapshot in place')
        client.tool('switch_tab', {'index': next(i for i, t in enumerate(client.api('/api/browser/tabs')['tabs']) if t['id'] == first)})
        check(json.loads(client.tool('evaluate', {'code': 'return scrollY;'})[0]['text']) == 2400,
              'scroll position is restored before the live page is revealed')

        # A page that never finishes loading is handed over after the fallback delay.
        gates['/second'] = Event()
        client.command(cmd='activate', id=second)
        wait(lambda: covered(second), 'second wake cover')
        started = time.monotonic()
        wait(lambda: in_place(second), 'fallback handoff', 6)
        elapsed = time.monotonic() - started
        check(2 <= elapsed <= 4.5, f'a stalled page replaces its snapshot after the fallback delay ({elapsed:.1f} s)')
        gates['/second'].set()

        # Explicit navigation cancels a pending wake.
        gates['/third'] = Event()
        gates['/replacement'] = Event()
        client.command(cmd='activate', id=third)
        wait(lambda: covered(third), 'third wake cover')
        client.command(cmd='navigate', url=base + '/replacement')
        wait(lambda: third not in waking(), 'navigation cancellation')
        gates['/third'].set()
        gates['/replacement'].set()
        wait(lambda: not next(t for t in client.api('/api/browser/tabs')['tabs'] if t['id'] == third)['loading'], 'replacement load')
        check(third not in waking() and in_place(third), 'navigation during a wake drops the snapshot and shows the new page')

        gates['/fourth'] = Event()
        shell(f'''__shell.ipc({{cmd:'activate',id:{fourth}}});
                  __shell.ipc({{cmd:'navigate',url:{json.dumps(base + '/wake-search')}}}); return true;''')
        wait(lambda: any(t['id'] == fourth and t['url'] == base + '/wake-search' and not t['loading']
                         for t in client.api('/api/browser/tabs')['tabs']), 'search during wake creation', 5)
        check(fourth not in waking(), 'search during wake creation bypasses the stalled old page')
        gates['/fourth'].set()

        # Submit a new destination before CEF has delivered on_after_created.
        # The original request stays blocked, so reaching the final page proves
        # navigation does not wait for it and does not get lost during creation.
        for attempt in range(4):
            old, final = f'/creating-{attempt}', f'/search-{attempt}'
            gates[old] = Event()
            shell(f'''__shell.ipc({{cmd:'newTab',url:{json.dumps(base + old)}}});
                      __shell.ipc({{cmd:'navigate',url:{json.dumps(base + '/superseded')}}});
                      __shell.ipc({{cmd:'navigate',url:{json.dumps(base + final)}}}); return true;''')
            def reached():
                state = client.api('/api/browser/tabs')
                return next((t for t in state['tabs'] if t['id'] == state['active']
                             and t['url'] == base + final and not t['loading']), None)
            wait(reached, 'navigation during browser creation', 5)
            gates[old].set()
        check(True, 'rapid navigation during browser creation reaches the latest destination')
    except Exception:
        log.flush()
        print((profile / 'app.log').read_text()[-10000:])
        raise
    finally:
        for gate in gates.values():
            gate.set()
        try:
            client.command(cmd='quit')
            app.wait(timeout=20)
        except (urllib.error.URLError, subprocess.TimeoutExpired):
            app.terminate()
            app.wait(timeout=5)
        fixture.shutdown()
        log.close()
