#!/usr/bin/env python3
"""Spike check for the app-owned window: glass, a WebKit shell, and Chromium tabs as child views.

Starts its own background-mode instance and fixture server, drives them through the
local API, saves a capture of the window, and stops only the processes it started.
"""
import argparse
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
APP = ROOT / 'target/bundle/Atrium.app/Contents/MacOS/atrium'
WATCHDOG = Path.home() / 'Desktop/atrium-rs-wt/watchdog.sh'

parser = argparse.ArgumentParser()
parser.add_argument('--port', type=int, default=7900)
parser.add_argument('--fixture-port', type=int, default=7909)
parser.add_argument('--data-dir', type=Path, default=Path(f'/private/tmp/atrium-native-{int(time.time())}'))
parser.add_argument('--shot', type=Path, default=Path('/private/tmp/native-chrome/window.png'))
args = parser.parse_args()
BASE = f'http://127.0.0.1:{args.port}'
FIXTURE = f'http://127.0.0.1:{args.fixture_port}'
results = []


def api(path, data=None, raw=False, timeout=35):
    token = (args.data_dir / 'api-token').read_text().strip()
    body = None if data is None else data.encode() if isinstance(data, str) else json.dumps(data).encode()
    req = urllib.request.Request(BASE + path, data=body,
                                 headers={'Content-Type': 'application/json', 'x-atrium-token': token})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        body = r.read()
    return body if raw else json.loads(body)


def command(**data):
    return api('/api/browser/command', data)


def tool(name, **args_):
    r = api('/api/tool', {'name': name, 'args': args_})
    if r.get('isError'):
        raise AssertionError(f'{name}: {r}')
    return ' '.join(c.get('text', '') for c in r['content'])


def wait(what, check, timeout=20):
    end = time.time() + timeout
    last = None
    while time.time() < end:
        try:
            last = check()
            if last:
                return last
        except (urllib.error.URLError, FileNotFoundError, ConnectionError, KeyError, StopIteration) as e:
            last = e
        time.sleep(.25)
    raise AssertionError(f'timed out waiting for {what}: {last}')


def check(name, fn):
    try:
        detail = fn()
        results.append((name, True, detail))
        print(f'PASS {name}: {detail}', flush=True)
    except Exception as e:  # noqa: BLE001 - report every check, then fail at the end
        results.append((name, False, str(e)))
        print(f'FAIL {name}: {e}', flush=True)


def tab_view(tab_id):
    return next(t for t in api('/api/browser/memory')['tabs'] if t['id'] == tab_id)['view']


def agent_uses(tab_id):
    """Points the agent's tools (evaluate, read_page) at a tab."""
    index = next(i for i, t in enumerate(api('/api/browser/tabs')['tabs']) if t['id'] == tab_id)
    tool('switch_tab', index=index)


# Shell scripting needs its own test flag on top of the mock keychain.
env = {**os.environ, 'ATRIUM_MOCK_KEYCHAIN': '1', 'ATRIUM_BACKGROUND_WINDOW': '1', 'ATRIUM_TEST_SHELL_EVAL': '1'}
args.data_dir.mkdir(parents=True, exist_ok=True)
args.shot.parent.mkdir(parents=True, exist_ok=True)
fixture = subprocess.Popen([sys.executable, str(ROOT / 'tools/fixture_server.py'), '--port', str(args.fixture_port)],
                           stdout=subprocess.DEVNULL)
log = open(args.data_dir / 'app.log', 'w')
app = subprocess.Popen([str(APP), '--data-dir', str(args.data_dir), 'run', '--port', str(args.port),
                        '--opencode-url', 'http://127.0.0.1:1'], env=env, stdout=log, stderr=subprocess.STDOUT)
watchdog_log = open(args.data_dir / 'watchdog.log', 'w')
watchdog = subprocess.Popen(['/bin/sh', str(WATCHDOG), str(app.pid)], stdout=watchdog_log, stderr=subprocess.STDOUT) \
    if WATCHDOG.exists() else None
print(f'app pid {app.pid}, data dir {args.data_dir}', flush=True)
try:
    wait('native shell', lambda: api('/api/browser/memory').get('shell_ready'))
    # The shell sends `ready` once its page and authenticated API calls have loaded.
    wait('shell ready (first tab record)', lambda: api('/api/browser/tabs')['tabs'])

    def shell_runs():
        # A fresh profile opens onboarding over the page; finish it as its Skip button does.
        r = api('/api/debug/shell', "window.atriumOnboarding && window.atriumOnboarding.close(); return typeof window.atriumShell")
        assert r == 'object', r
        return 'shell page loaded in WebKit with its authenticated API'
    check('web shell runs in the transparent WebKit view', shell_runs)

    def opens_page():
        command(cmd='newTab', url=f'{FIXTURE}/page/one')
        tab = wait('page load', lambda: next((t for t in api('/api/browser/tabs')['tabs']
                                               if '/page/one' in t['url'] and not t['loading']), None))
        view = wait('view placed', lambda: (v := tab_view(tab['id'])) and not v['hidden'] and v['w'] > 100 and v)
        assert view['in_window'], view
        agent_uses(tab['id'])
        return f"tab {tab['id']} at x={view['x']} y={view['y']} {view['w']}x{view['h']}"
    check('Chromium page is a visible child view of the app window', opens_page)

    def runs_page_script():
        out = tool('evaluate', code="document.querySelector('#increment').click(); document.querySelector('#increment').click(); return document.title + ' / ' + document.querySelector('#count').value")
        assert 'Fixture /page/one / 2' in out, out
        return out.strip()
    check('page script and DOM events run', runs_page_script)

    def follows_layout():
        active = api('/api/browser/tabs')['active']
        command(cmd='layout', main={'x': 300, 'y': 50, 'w': 600, 'h': 400})
        v = wait('new frame', lambda: (v := tab_view(active)) and v['x'] == 300 and v['w'] == 600 and v)
        assert (v['y'], v['h']) == (50, 400), v
        return f"moved to {v['x']},{v['y']} {v['w']}x{v['h']} (top-left origin)"
    check('tab view follows the shell layout rects', follows_layout)

    def hides_when_covered():
        active = api('/api/browser/tabs')['active']
        command(cmd='cover', on=True)
        wait('hidden', lambda: tab_view(active)['hidden'])
        command(cmd='cover', on=False)
        wait('shown', lambda: not tab_view(active)['hidden'])
        return 'hidden under overlays, shown again'
    check('cover hides and restores the page view', hides_when_covered)

    def focus():
        active = api('/api/browser/tabs')['active']
        command(cmd='activate', id=active)
        v = wait('first responder', lambda: (v := tab_view(active))['first_responder'] and v)
        return 'activated tab view is in the window first-responder chain'
    check('keyboard focus moves into the Chromium view', focus)

    # Popups need a real user click (scripted window.open is blocked), and test windows
    # ignore the mouse; popups are not covered here.

    def closes_one_tab_only():
        command(cmd='newTab', url=f'{FIXTURE}/page/to-close')
        tab = wait('second tab', lambda: next((t for t in api('/api/browser/tabs')['tabs']
                                               if '/page/to-close' in t['url'] and not t['loading']), None))
        command(cmd='close', id=tab['id'])
        wait('tab removed', lambda: all(t['id'] != tab['id'] for t in api('/api/browser/tabs')['tabs']))
        time.sleep(1)
        assert app.poll() is None, 'closing a tab closed the app'
        assert api('/api/browser/memory')['shell_ready']
        return 'tab and its view removed; window and other tabs stay'
    check('closing a tab does not close the window', closes_one_tab_only)

    def suspension():
        for i in range(9):
            command(cmd='newTab', url=f'{FIXTURE}/page/budget-{i}', background=True)
        state = wait('a tab to sleep', lambda: (s := api('/api/browser/memory')) and any(t['suspended'] for t in s['tabs']) and s,
                     timeout=60)
        asleep = next(t for t in state['tabs'] if t['suspended'])
        command(cmd='activate', id=asleep['id'])
        wait('wake', lambda: (t := next(t for t in api('/api/browser/tabs')['tabs'] if t['id'] == asleep['id']))
             and not t['suspended'] and not t['loading'])
        v = wait('woken view placed', lambda: (v := tab_view(asleep['id'])) and not v['hidden'] and v)
        return f"live {state['live_renderers']} of max {state['max_live_tabs']}; slept tab woke into a placed view"
    check('tab sleeping releases renderers and wakes them in place', suspension)

    def capture():
        def attempt():
            try:
                return api('/api/debug/window.png', raw=True)
            except urllib.error.HTTPError as e:
                raise urllib.error.URLError(f'{e.code}: {e.read().decode(errors="replace")}')
        # The window server can return nothing for an off-screen window while it is busy.
        png = wait('window image', attempt, timeout=10)
        assert png[:8] == b'\x89PNG\r\n\x1a\n', png[:40]
        args.shot.write_bytes(png)
        return f'{len(png)} bytes -> {args.shot}'
    check('window capture (glass, shell, page)', capture)

    def quits():
        command(cmd='quit')
        app.wait(timeout=30)
        return f'exit code {app.returncode}'
    check('quit closes every browser and exits', quits)
finally:
    for p in (app, fixture):
        if p.poll() is None:
            p.terminate()
    if watchdog:
        watchdog.wait(timeout=5)
    log.close()
    watchdog_log.close()
    print('watchdog:', (args.data_dir / 'watchdog.log').read_text().strip(), flush=True)

failed = [r for r in results if not r[1]]
print(f'{len(results) - len(failed)}/{len(results)} passed', flush=True)
sys.exit(1 if failed else 0)
