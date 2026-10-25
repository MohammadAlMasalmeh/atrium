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
APP = ROOT / 'target/bundle/Atrium.app/Contents/MacOS/atrium'
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
        # Shell scripting needs its own test flag on top of the mock keychain.
        full_env = {**os.environ, 'ATRIUM_MOCK_KEYCHAIN': '1', 'ATRIUM_BACKGROUND_WINDOW': '1', 'ATRIUM_TEST_SHELL_EVAL': '1', **env}
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
        def attempt():
            try:
                return app.api('/api/debug/window.png', raw=True)
            except urllib.error.HTTPError as e:
                raise urllib.error.URLError(f'{e.code}: {e.read().decode(errors="replace")}')
        # The window server can return nothing for an off-screen window while it is busy.
        png = wait('window image', attempt, timeout=10)
        assert png[:8] == b'\x89PNG\r\n\x1a\n', png[:40]
        return f'{len(png)} bytes'
    check('window capture', capture)


def sidebar_suite(app):
    """The native sidebar list. Rows are addressed by index as currently drawn."""
    def sidebar():
        return app.api('/api/browser/memory')['native_sidebar']

    def row(title=None, kind=None):
        rows = sidebar()['rows']
        return next(r for r in rows if (title is None or r['title'] == title) and (kind is None or r['kind'] == kind))

    def act(action, r, **extra):
        app.command(cmd='nativeSidebar', action=action, row=r, **extra)

    def tab_ids(path):
        return [t['id'] for t in app.tabs()['tabs'] if path in t['url']]

    def open_page(name):
        app.command(cmd='newTab', url=f'{FIXTURE}/page/{name}')
        wait(f'{name} row', lambda: row(f'Fixture /page/{name}', 'Tab'))

    def replaces_list():
        # Shown once the shell's first layout report says where its list is.
        s = wait('list placed', lambda: (s := sidebar())['visible'] and s)
        kinds = [r['kind'] for r in s['rows']]
        assert kinds[:3] == ['Favs', 'SpaceHeader', 'NewTab'] and 'ChatsHeader' in kinds, kinds
        hidden = app.shell("return getComputedStyle(document.querySelector('.sb-scroll')).visibility")
        assert hidden == 'hidden', hidden
        return f"{len(kinds)} rows in a source-list table; shell list hidden"
    check('native sidebar list replaces the shell list', replaces_list)

    def tabs_listed():
        for name in ('alpha', 'beta', 'gamma'):
            open_page(name)
        gamma = wait('gamma selected', lambda: (r := row('Fixture /page/gamma', 'Tab'))['selected'] and r)
        assert not row('Fixture /page/alpha', 'Tab')['selected']
        return f"tab rows with titles; active tab row {gamma['row']} selected"
    check('tabs listed and the active one selected', tabs_listed)

    def click_activates():
        act('click', row('Fixture /page/alpha', 'Tab')['row'])
        wait('alpha active', lambda: '/page/alpha' in app.active()['url'])
        wait('alpha selected', lambda: row('Fixture /page/alpha', 'Tab')['selected'])
        return 'click switches tabs and moves the selection'
    check('clicking a row activates the tab', click_activates)

    def option_click_splits():
        act('click', row('Fixture /page/beta', 'Tab')['row'], split=True)
        wait('split view', lambda: app.shell("return document.body.classList.contains('split')"))
        wait('both selected', lambda: row('Fixture /page/beta', 'Tab')['selected'] and row('Fixture /page/alpha', 'Tab')['selected'])
        app.command(cmd='split', id=None)
        wait('split closed', lambda: not app.shell("return document.body.classList.contains('split')"))
        return 'Option-click opens split view; both panes selected'
    check('Option-click opens split view', option_click_splits)

    def loading_row():
        app.command(cmd='newTab', url=f'{FIXTURE}/slow/sidebar')
        wait('spinner', lambda: any(r['spinning'] for r in sidebar()['rows'] if r['kind'] == 'Tab'), timeout=5)
        wait('loaded', lambda: (r := row('Fixture /slow/sidebar', 'Tab')) and not r['spinning'], timeout=15)
        return 'spinner replaces the favicon while loading'
    check('loading indicator in the tab row', loading_row)

    def middle_click_closes():
        [gid] = tab_ids('/page/gamma')
        act('middle', row('Fixture /page/gamma', 'Tab')['row'])
        wait('gamma closed', lambda: gid not in [t['id'] for t in app.tabs()['tabs']])
        wait('row gone', lambda: not any(r['title'] == 'Fixture /page/gamma' for r in sidebar()['rows']))
        return 'middle click closes the tab'
    check('middle-click closes a tab', middle_click_closes)

    def drag_reorders():
        alpha, beta = row('Fixture /page/alpha', 'Tab')['row'], row('Fixture /page/beta', 'Tab')['row']
        assert alpha < beta, (alpha, beta)
        act('drag', alpha, to=beta + 1)
        wait('alpha below beta', lambda: row('Fixture /page/alpha', 'Tab')['row'] > row('Fixture /page/beta', 'Tab')['row'])
        order = [t['url'].rsplit('/', 1)[-1] for t in app.tabs()['tabs'] if '/page/' in t['url']]
        assert order.index('alpha') > order.index('beta'), order
        return f'tab order is now {order}'
    check('drag to reorder tabs', drag_reorders)

    def menu_duplicate_and_close():
        act('menu', row('Fixture /page/beta', 'Tab')['row'], item='Duplicate')
        wait('duplicate', lambda: len(tab_ids('/page/beta')) == 2)
        copy = max(tab_ids('/page/beta'))  # opened last, so it is the lower row
        rows = wait('two beta rows', lambda: len(rs := [r for r in sidebar()['rows']
                                                         if r['kind'] == 'Tab' and r['title'] == 'Fixture /page/beta']) == 2 and rs)
        act('menu', rows[1]['row'], item='Close Tab')
        wait('copy closed', lambda: copy not in tab_ids('/page/beta'))
        return 'Duplicate and Close Tab from the context menu'
    check('tab context menu', menu_duplicate_and_close)

    def pin_by_drag():
        url = next(t['url'] for t in app.tabs()['tabs'] if '/page/beta' in t['url'])
        pinned = f"return Object.values(JSON.parse(localStorage.getItem('atrium.pinned') || '{{}}')).flat().some((p) => p.url === {json.dumps(url)})"
        act('drag', row('Fixture /page/beta', 'Tab')['row'], to=row(kind='SpaceHeader')['row'] + 1)
        wait('pinned', lambda: app.shell(pinned) is True)
        pin = wait('pin row', lambda: row(kind='Pin'))
        act('menu', pin['row'], item='Unpin')
        wait('unpinned', lambda: app.shell(pinned) is False)
        wait('back in tabs', lambda: row('Fixture /page/beta', 'Tab'))
        return f"dragging a tab above New Tab pins it (row title {pin['title']!r}); Unpin from its menu"
    check('pin by dragging, unpin from the menu', pin_by_drag)

    def move_to_space():
        alpha = row('Fixture /page/alpha', 'Tab')
        assert not alpha['selected'], 'alpha should not be active here'
        act('menu', alpha['row'], item='Move to Space/Work')
        wait('alpha left this space', lambda: not any(r['title'] == 'Fixture /page/alpha' for r in sidebar()['rows']))
        assert row(kind='SpaceHeader')['title'] == 'Personal'
        return 'Move to Space ▸ Work removes it from this space’s list'
    check('move a tab to another space', move_to_space)

    def rename_space():
        act('rename', 0, name='Research')
        wait('header renamed', lambda: row(kind='SpaceHeader')['title'] == 'Research')
        saved = app.shell("return JSON.parse(localStorage.getItem('atrium.spaces3')).map((s) => s.name).join()")
        assert 'Research' in saved, saved
        return f'renamed in place; saved spaces: {saved}'
    check('rename the space in place', rename_space)

    def new_tab_row():
        act('click', row(kind='NewTab')['row'])
        wait('start page', lambda: not app.chrome()['web'])
        wait('New Tab selected', lambda: row(kind='NewTab')['selected'])
        return 'New Tab row opens the start page and stays selected'
    check('New Tab row', new_tab_row)

    def sleeping_dimmed():
        for i in range(9):
            app.command(cmd='newTab', url=f'{FIXTURE}/page/sleep-{i}', background=True)
        state = wait('a tab to sleep', lambda: (s := app.api('/api/browser/memory')) and any(t['suspended'] for t in s['tabs']) and s, timeout=60)
        asleep = next(t['id'] for t in state['tabs'] if t['suspended'])
        url = next(t['url'] for t in app.tabs()['tabs'] if t['id'] == asleep)
        title = 'Fixture ' + url.split(str(args.fixture_port), 1)[1]
        r = wait('dimmed row', lambda: (r := row(title, 'Tab')) and r['dim'] and r)
        return f'sleeping tab row {r["row"]} drawn in secondary colour'
    check('sleeping tabs are dimmed', sleeping_dimmed)


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
    for name, env, suite in [('native', {'ATRIUM_NATIVE_CHROME': '1'}, native_suite),
                             ('sidebar', {'ATRIUM_NATIVE_CHROME': '1'}, sidebar_suite),
                             ('default', {}, default_suite)]:
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
