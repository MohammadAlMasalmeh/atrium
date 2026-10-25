#!/usr/bin/env python3
"""End-to-end checks for the native AppKit browser chrome (ATRIUM_NATIVE_CHROME=1).

Starts its own background-mode instances (one at a time) and a local fixture server,
drives the real AppKit controls through a test-only hook, and stops only what it started.
Instances never activate or take focus. With --screenshots it also captures the window in
light and dark mode; those runs place the window on screen behind every other window,
still unable to activate or take clicks.
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
APP = ROOT / 'target/bundle/atrium.app/Contents/MacOS/atrium'
WATCHDOG = Path.home() / 'Desktop/atrium-rs-wt/watchdog.sh'

parser = argparse.ArgumentParser()
parser.add_argument('--port', type=int, default=7900)
parser.add_argument('--fixture-port', type=int, default=7909)
parser.add_argument('--data-root', type=Path, default=Path(f'/private/tmp/atrium-native-{int(time.time())}'))
parser.add_argument('--screenshots', type=Path, help='directory for light and dark window captures')
args = parser.parse_args()
BASE = f'http://127.0.0.1:{args.port}'
FIXTURE = f'http://127.0.0.1:{args.fixture_port}'
results = []


class Instance:
    """One background app instance with a fresh data dir and the watchdog beside it."""

    def __init__(self, name, **env):
        self.data = args.data_root / name
        self.data.mkdir(parents=True, exist_ok=True)
        full_env = {**os.environ, 'ATRIUM_MOCK_KEYCHAIN': '1', 'ATRIUM_BACKGROUND_WINDOW': '1', **env}
        self.log = open(self.data / 'app.log', 'w')
        self.proc = subprocess.Popen([str(APP), '--data-dir', str(self.data), 'run', '--port', str(args.port),
                                      '--opencode-url', 'http://127.0.0.1:1'],
                                     env=full_env, stdout=self.log, stderr=subprocess.STDOUT)
        self.dog_log = open(self.data / 'watchdog.log', 'w')
        self.dog = subprocess.Popen(['/bin/sh', str(WATCHDOG), str(self.proc.pid)], stdout=self.dog_log,
                                    stderr=subprocess.STDOUT) if WATCHDOG.exists() else None
        print(f'[{name}] pid {self.proc.pid}, data dir {self.data}', flush=True)
        wait('native shell', lambda: self.api('/api/browser/memory').get('shell_ready'))
        wait('shell ready', lambda: self.api('/api/browser/tabs')['tabs'])
        # A fresh profile opens onboarding over everything; finish it as its Skip button does.
        self.shell('window.atriumOnboarding && window.atriumOnboarding.close(); return true')

    def api(self, path, data=None, raw=False, timeout=35):
        token = (self.data / 'api-token').read_text().strip()
        body = None if data is None else data.encode() if isinstance(data, str) else json.dumps(data).encode()
        req = urllib.request.Request(BASE + path, data=body,
                                     headers={'Content-Type': 'application/json', 'x-atrium-token': token})
        with urllib.request.urlopen(req, timeout=timeout) as r:
            body = r.read()
        return body if raw else json.loads(body)

    def command(self, **data):
        return self.api('/api/browser/command', data)

    def shell(self, js):
        """Runs in the shell page. Its own code is block-scoped, so only `window` globals are reachable."""
        r = self.api('/api/debug/shell', f'try {{ {js}\n}} catch (e) {{ return {{ thrown: String(e) }} }}')
        if isinstance(r, dict) and ('error' in r or 'thrown' in r):
            raise AssertionError(f'shell eval failed: {r}')
        return r

    def chrome(self):
        return self.api('/api/browser/memory')['native_chrome']

    def click(self, action, text=''):
        self.command(cmd='nativeChrome', action=action, text=text)

    def tabs(self):
        return self.api('/api/browser/tabs')

    def active(self):
        state = self.tabs()
        return next(t for t in state['tabs'] if t['id'] == state['active'])

    def tool(self, name, **args_):
        r = self.api('/api/tool', {'name': name, 'args': args_})
        if r.get('isError'):
            raise AssertionError(f'{name}: {r}')
        return ' '.join(c.get('text', '') for c in r['content'])

    def evaluate_active(self, code):
        state = self.tabs()
        index = next(i for i, t in enumerate(state['tabs']) if t['id'] == state['active'])
        self.tool('switch_tab', index=index)
        return self.tool('evaluate', code=code).strip()

    def stop(self):
        if self.proc.poll() is None:
            try:
                self.command(cmd='quit')
                self.proc.wait(timeout=30)
            except Exception:  # noqa: BLE001 - fall back to terminating our own process
                self.proc.terminate()
                self.proc.wait(timeout=10)
        if self.dog:
            self.dog.wait(timeout=5)
        self.log.close()
        self.dog_log.close()
        print('watchdog:', (self.data / 'watchdog.log').read_text().strip(), flush=True)


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
        time.sleep(.2)
    raise AssertionError(f'timed out waiting for {what}: {last}')


def check(name, fn):
    try:
        detail = fn()
        results.append((name, True, detail))
        print(f'PASS {name}: {detail}', flush=True)
    except Exception as e:  # noqa: BLE001 - report every check, then fail at the end
        results.append((name, False, str(e)))
        print(f'FAIL {name}: {e}', flush=True)


def native_suite(app):
    def installed():
        c = app.chrome()
        assert c and c['visible'], c
        hidden = app.shell("return [getComputedStyle(document.querySelector('#url')).visibility, "
                           "getComputedStyle(document.querySelector('#back')).visibility].join()")
        assert hidden == 'hidden,hidden', hidden
        return 'AppKit toolbar and address field on the glass; shell hides its own'
    check('native toolbar replaces the shell toolbar', installed)

    def start_page():
        c = app.chrome()
        assert not c['web'] and c['address'] == '' and not c['back_enabled'] and not c['actions_visible'], c
        return 'empty field with placeholder, back disabled, no page actions'
    check('start page state', start_page)

    def address_opens_tab():
        before = len(app.tabs()['tabs'])
        app.click('address', f'{FIXTURE}/page/one')
        wait('page one active and loaded', lambda: (t := app.active()) and '/page/one' in t['url'] and not t['loading'])
        c = wait('field shows host', lambda: (c := app.chrome()) and c['web'] and c['address'] == '127.0.0.1' and c)
        assert c['reload_enabled'] and c['actions_visible'] and not c['editing'], c
        return f"{before} -> {len(app.tabs()['tabs'])} tabs; field shows {c['address']!r}"
    check('Enter in the address field on the start page opens a tab', address_opens_tab)

    def address_navigates():
        count = len(app.tabs()['tabs'])
        app.click('address', f'{FIXTURE}/page/two')
        wait('page two loaded', lambda: (t := app.active()) and '/page/two' in t['url'] and not t['loading'])
        assert len(app.tabs()['tabs']) == count, 'opened a new tab instead of navigating'
        c = wait('back enabled', lambda: (c := app.chrome())['back_enabled'] and c)
        assert not c['forward_enabled'], c
        return 'same tab navigated; back enabled'
    check('Enter on a web page navigates the current tab', address_navigates)

    def back_forward():
        app.click('back')
        wait('back to page one', lambda: '/page/one' in app.active()['url'])
        wait('forward enabled', lambda: app.chrome()['forward_enabled'])
        app.click('forward')
        wait('forward to page two', lambda: '/page/two' in app.active()['url'])
        wait('forward disabled again', lambda: not app.chrome()['forward_enabled'])
        return 'buttons drive history and track can-go-back/forward'
    check('back and forward buttons', back_forward)

    def reload():
        app.evaluate_active('window.__beforeReload = 1; return "marked"')
        app.click('reload')
        wait('page reloaded', lambda: 'gone' in app.evaluate_active('return window.__beforeReload ? "still" : "gone"'))
        return 'page state reset after reload'
    check('reload button', reload)

    def loading():
        app.click('address', f'{FIXTURE}/slow/page')
        wait('spinner on', lambda: app.chrome()['loading'], timeout=5)
        wait('spinner off', lambda: not app.chrome()['loading'], timeout=15)
        return 'spinner runs while the page loads'
    check('loading indicator', loading)

    def pin():
        url = app.active()['url']
        pinned = f"return Object.values(JSON.parse(localStorage.getItem('atrium.pinned') || '{{}}')).flat().some((p) => p.url === {json.dumps(url)})"
        assert app.shell(pinned) is False
        app.click('pin')
        wait('pinned', lambda: app.shell(pinned) is True)
        app.click('pin')
        wait('unpinned', lambda: app.shell(pinned) is False)
        return 'pin button pins and unpins the page in the current space'
    check('pin button', pin)

    def sidebar_toggle():
        app.click('sidebar')
        wait('sidebar hidden', lambda: app.shell("return document.body.classList.contains('sb-hidden')"))
        wait('chrome hidden with it', lambda: not app.chrome()['visible'])
        app.command(cmd='menu', name='toggleSidebar')
        wait('chrome back', lambda: app.chrome()['visible'])
        return 'toolbar hides with the sidebar and returns with ⌘S'
    check('sidebar toggle', sidebar_toggle)

    def overlays():
        app.command(cmd='menu', name='command-bar')
        wait('hidden under command bar', lambda: not app.chrome()['visible'])
        time.sleep(1)
        assert not app.chrome()['visible'], 'command bar closed by itself'
        app.shell("document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true })); return true")
        wait('back after Escape', lambda: app.chrome()['visible'])
        return 'hidden while a full shell overlay is open'
    check('shell overlays cover the native chrome', overlays)

    def capture():
        png = app.api('/api/debug/window.png', raw=True)
        assert png[:8] == b'\x89PNG\r\n\x1a\n', png[:40]
        return f'{len(png)} bytes'
    check('window capture', capture)


def default_suite(app):
    def web_toolbar():
        assert app.chrome() is None, app.chrome()
        shown = app.shell("return getComputedStyle(document.querySelector('#url')).visibility")
        assert shown == 'visible', shown
        return 'flag off: no native controls, shell draws its own'
    check('web toolbar stays the default', web_toolbar)


def screenshot(appearance):
    app = Instance(f'shot-{appearance}', ATRIUM_NATIVE_CHROME='1', ATRIUM_SHOW_BEHIND='1', ATRIUM_APPEARANCE=appearance)
    try:
        def shot():
            app.click('address', f'{FIXTURE}/page/screenshot')
            wait('page', lambda: (t := app.active()) and '/page/screenshot' in t['url'] and not t['loading'])
            time.sleep(1.5)
            out = args.screenshots / f'native-chrome-{appearance}.png'
            out.write_bytes(app.api('/api/debug/window.png', raw=True))
            return str(out)
        check(f'{appearance} screenshot', shot)
    finally:
        app.stop()


fixture = subprocess.Popen([sys.executable, str(ROOT / 'tools/fixture_server.py'), '--port', str(args.fixture_port)],
                           stdout=subprocess.DEVNULL)
try:
    for name, env, suite in [('native', {'ATRIUM_NATIVE_CHROME': '1'}, native_suite), ('default', {}, default_suite)]:
        app = Instance(name, **env)
        try:
            suite(app)
        finally:
            app.stop()
    if args.screenshots:
        args.screenshots.mkdir(parents=True, exist_ok=True)
        for appearance in ('light', 'dark'):
            screenshot(appearance)
finally:
    fixture.terminate()

failed = [r for r in results if not r[1]]
print(f'{len(results) - len(failed)}/{len(results)} passed', flush=True)
sys.exit(1 if failed else 0)
