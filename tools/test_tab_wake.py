#!/usr/bin/env python3
"""Verify sleeping tabs retain their viewport until the restored page has painted."""
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
image_requested = Event()


class Fixture(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == '/wake-image':
            image_requested.set()
        gate = gates.get(self.path)
        if gate:
            gate.wait(20)
        image = '<img src="/wake-image" width="1" height="1">' if self.path == '/first' and '/wake-image' in gates else ''
        body = f'''<!doctype html><title>Wake {self.path}</title>
          <style>body{{margin:0;background:coral}}section{{height:2000px}}
          section+section{{background:royalblue}}</style>
          <section>{self.path}</section><section>Saved scroll position</section>{image}
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

    def covered(id):
        state = memory()
        page = next(t for t in state['tabs'] if t['id'] == id)
        return (page['waking'] and id in state['wake_previews'] and page['view']
                and page['view']['x'] > state['layout']['main']['x'] + state['layout']['main']['w'])

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
        check(all(tab(id)['preview_bytes'] > 100 for id in (first, second, third)),
              'eviction saves compressed viewports while releasing renderers')

        gates['/first'] = Event()
        gates['/wake-image'] = Event()
        client.command(cmd='activate', id=first)
        wait(lambda: covered(first), 'first wake cover')
        check(not memory()['native_chrome']['loading'], 'wake does not show a toolbar loading spinner')
        check(shell(f'return __shell.S.tabs.find(t => t.id === {first}).loading === false;'),
              'wake does not show a sidebar loading spinner')
        # Switching away removes the native image, even while its page is still waking.
        client.command(cmd='activate', id=control)
        wait(lambda: not memory()['wake_previews'], 'switch away')
        client.command(cmd='activate', id=first)
        wait(lambda: covered(first), 'return to waking tab')
        time.sleep(.3)
        check(covered(first), 'saved viewport survives rapid tab switches and a stalled response')
        gates['/first'].set()
        wait(image_requested.is_set, 'image request')
        handoff_started = time.monotonic()
        wait(lambda: not tab(first)['waking'] and first not in memory()['wake_previews'], 'painted handoff', 5)
        elapsed = (time.monotonic() - handoff_started) * 1000
        check(next(t for t in client.api('/api/browser/tabs')['tabs'] if t['id'] == first)['loading'],
              f'usable page is revealed with its image still stalled ({elapsed:.0f} ms)')
        client.tool('switch_tab', {'index': next(i for i, t in enumerate(client.api('/api/browser/tabs')['tabs']) if t['id'] == first)})
        state = json.loads(client.tool('evaluate', {'code': 'return {ready:document.readyState,y:scrollY};'})[0]['text'])
        check(state == {'ready': 'interactive', 'y': 2400},
              'DOM and restored scroll are ready before the full load')
        check(not memory()['native_chrome']['loading'] and shell(f'return !__shell.S.tabs.find(t => t.id === {first}).loading;'),
              'remaining wake resources do not start a second loading indicator')
        gates['/wake-image'].set()
        live = tab(first)['view']
        check(live['x'] == memory()['layout']['main']['x'] and not live['hidden'],
              'painted page replaces the saved viewport in place')
        client.tool('switch_tab', {'index': next(i for i, t in enumerate(client.api('/api/browser/tabs')['tabs']) if t['id'] == first)})
        check(json.loads(client.tool('evaluate', {'code': 'return scrollY;'})[0]['text']) == 2400,
              'scroll position is restored before the live page is revealed')

        # Prewarming begins the same hidden restore before selection.
        gates['/second'] = Event()
        client.command(cmd='prewarm', id=second)
        wait(lambda: tab(second)['waking'], 'prewarm started')
        check(second not in memory()['wake_previews'], 'background prewarm does not cover another tab')
        client.command(cmd='split', id=second)
        wait(lambda: second in memory()['wake_previews'], 'split wake cover')
        gates['/second'].set()
        wait(lambda: not tab(second)['waking'] and second not in memory()['wake_previews'], 'split handoff')
        check(not tab(second)['view']['hidden'], 'split pane wakes behind its own viewport')
        client.command(cmd='split')

        # Explicit navigation cancels a pending restore and exposes normal navigation state.
        gates['/third'] = Event()
        gates['/replacement'] = Event()
        client.command(cmd='activate', id=third)
        wait(lambda: covered(third), 'third wake cover')
        client.command(cmd='navigate', url=base + '/replacement')
        wait(lambda: not tab(third)['waking'] and third not in memory()['wake_previews'], 'navigation cancellation')
        check(tab(third)['preview_bytes'] == 0, 'explicit navigation drops the old viewport')
        gates['/third'].set()
        gates['/replacement'].set()
        wait(lambda: not next(t for t in client.api('/api/browser/tabs')['tabs'] if t['id'] == third)['loading'], 'replacement load')
        check(third not in memory()['wake_previews'], 'late wake callbacks cannot restore a cancelled cover')

        gates['/fourth'] = Event()
        shell(f'''__shell.ipc({{cmd:'activate',id:{fourth}}});
                  __shell.ipc({{cmd:'navigate',url:{json.dumps(base + '/wake-search')}}}); return true;''')
        wait(lambda: any(t['id'] == fourth and t['url'] == base + '/wake-search' and not t['loading']
                         for t in client.api('/api/browser/tabs')['tabs']), 'search during wake creation', 5)
        check(not tab(fourth)['waking'] and fourth not in memory()['wake_previews'],
              'search during wake creation bypasses the stalled old page')
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
