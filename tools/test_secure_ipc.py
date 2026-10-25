#!/usr/bin/env python3
"""End-to-end check of the shell's process-message IPC and the local attack surface.

Starts its own background instance (mock keychain, invisible window, fresh profile),
drives the real shell over window.ipc, loads a hostile fixture page, probes the loopback
API like a website or a DNS-rebinding attacker would, and checks every fixed finding in
docs/SECURITY-REVIEW.md. Needs tools/fixture_server.py running at --fixture.
Exits non-zero on the first failure.
"""
import argparse
import http.client
import json
import os
import re
import stat
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path
import atrium_api

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--app', type=Path, default=Path('target/bundle/atrium.app/Contents/MacOS/atrium'))
parser.add_argument('--port', type=int, default=7880)
parser.add_argument('--opencode-port', type=int, default=7885)
parser.add_argument('--fixture', default='http://127.0.0.1:7881')
parser.add_argument('--watchdog', type=Path, default=Path(__file__).resolve().parents[2] / 'watchdog.sh')
parser.add_argument('--skip-opencode', action='store_true', help='skip the check that a started OpenCode server requires a password')
args = parser.parse_args()
API = f'http://127.0.0.1:{args.port}'
HOST = f'127.0.0.1:{args.port}'
GLANCE = 2**64 - 3
passed = []


def check(name, condition, detail=''):
    if not condition:
        raise AssertionError(f'{name}: {detail}')
    passed.append(name)
    print('ok', name, flush=True)


def wait(predicate, timeout=20, step=.25):
    deadline = time.time() + timeout
    while True:
        value = predicate()
        if value or time.time() > deadline:
            return value
        time.sleep(step)


def raw(method, path, headers=None, body=None, host=HOST):
    """A request with full control over Host and Origin, like a browser or an attacker would send."""
    conn = http.client.HTTPConnection('127.0.0.1', args.port, timeout=10)
    conn.putrequest(method, path, skip_host=True, skip_accept_encoding=True)
    conn.putheader('Host', host)
    for key, value in (headers or {}).items():
        conn.putheader(key, value)
    data = body.encode() if isinstance(body, str) else body
    if data is not None:
        conn.putheader('Content-Length', str(len(data)))
    conn.endheaders(data)
    response = conn.getresponse()
    result = (response.status, {k.lower(): v for k, v in response.getheaders()}, response.read())
    conn.close()
    return result


class Instance:
    def __init__(self, profile, extra_env=None, opencode=False):
        self.profile = profile
        self.client = atrium_api.Client(API, data_dir=profile)
        command = [str(args.app.resolve()), '--data-dir', str(profile), 'run', '--port', str(args.port)]
        command += ['--opencode-port', str(args.opencode_port)] if opencode else ['--opencode-url', 'http://127.0.0.1:1']
        self.log = (profile.parent / f'{profile.name}.log').open('w')
        self.process = subprocess.Popen(command, stdout=self.log, stderr=self.log,
                                        env={**atrium_api.instance_env(), **(extra_env or {})})
        self.watchdog = None
        if args.watchdog.exists():
            self.watchdog_log = profile.parent / f'{profile.name}.watchdog.log'
            self.watchdog = subprocess.Popen(['/bin/sh', str(args.watchdog), str(self.process.pid)],
                                             stdout=self.watchdog_log.open('w'), stderr=subprocess.STDOUT)

    def ready(self):
        def probe():
            try:
                return self.client.api('/api/browser/memory').get('shell_ready')
            except (urllib.error.URLError, TimeoutError, ConnectionError):
                return False
        if not wait(probe, 60, .5):
            raise AssertionError('Browser did not become ready')
        wait(lambda: self.memory()['shell_ipc']['process_messages'] > 0, 20)

    def memory(self):
        return self.client.api('/api/browser/memory')

    def tabs(self):
        return self.client.api('/api/browser/tabs')

    def shell(self, js):
        req = urllib.request.Request(API + '/api/debug/shell', data=js.encode(), headers=self.client.headers({'Content-Type': 'text/plain'}))
        with urllib.request.urlopen(req, timeout=35) as response:
            return json.load(response)

    def tool(self, name, arguments, timeout=60):
        return self.client.request('/api/tool', {'name': name, 'args': arguments}, timeout)

    def evaluate(self, code):
        result = self.tool('evaluate', {'code': code})
        assert not result.get('isError'), result
        return json.loads(result['content'][0]['text'])

    def stop(self):
        try:
            self.client.api('/api/browser/command', {'cmd': 'quit'})
        except (urllib.error.URLError, TimeoutError, ConnectionError, AssertionError):
            pass
        try:
            self.process.wait(timeout=20)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            self.process.wait(timeout=5)
        if self.watchdog:
            self.watchdog.wait(timeout=5)
            text = self.watchdog_log.read_text()
            if 'FRONTMOST' in text:
                raise AssertionError('The test instance became the frontmost application: ' + text)
        self.log.close()


def shell_suite(b):
    ipc = b.shell("const d = Object.getOwnPropertyDescriptor(window, 'ipc'); return { type: typeof window.ipc, native: String(window.ipc.postMessage).includes('[native code]'), writable: d.writable, configurable: d.configurable, url: location.href };")
    check('shell has a native, non-replaceable window.ipc', ipc == {'type': 'object', 'native': True, 'writable': False, 'configurable': False, 'url': API + '/shell'}, ipc)
    bad = b.shell("try { window.ipc.postMessage(42); return 'accepted'; } catch (e) { return String(e); }")
    check('window.ipc rejects non-string messages', 'expects a string' in str(bad), bad)
    before = b.memory()['shell_ipc']

    post = lambda message: b.shell(f'window.ipc.postMessage({json.dumps(json.dumps(message))}); return true;')
    existing = {t['id'] for t in b.tabs()['tabs']}
    post({'cmd': 'newTab', 'url': args.fixture + '/page/ipc-a'})
    post({'cmd': 'newTab', 'url': args.fixture + '/page/ipc-b'})
    created = wait(lambda: [t for t in b.tabs()['tabs'] if t['id'] not in existing and '/page/ipc-' in t['url']] if len([t for t in b.tabs()['tabs'] if '/page/ipc-' in t['url']]) == 2 else None)
    check('shell creates tabs over process messages', created and len(created) == 2, b.tabs())
    a = next(t for t in created if t['url'].endswith('ipc-a'))
    tb = next(t for t in created if t['url'].endswith('ipc-b'))
    check('a newly created tab is active', b.tabs()['active'] == tb['id'], b.tabs())
    wait(lambda: b.shell(f"return !!document.querySelector('#tabs .row[data-id=\"{a['id']}\"]')"))
    b.shell(f"document.querySelector('#tabs .row[data-id=\"{a['id']}\"]').click(); return true;")
    check('clicking a sidebar row switches tabs', wait(lambda: b.tabs()['active'] == a['id']), b.tabs())
    b.shell(f"document.querySelector('#tabs .row[data-id=\"{tb['id']}\"] .x').click(); return true;")
    check('the close button closes a tab', wait(lambda: all(t['id'] != tb['id'] for t in b.tabs()['tabs'])), b.tabs())

    rect = lambda: b.shell("const r = document.querySelector('#pane-main').getBoundingClientRect(); return { x: Math.round(r.left), y: Math.round(r.top), w: Math.round(r.width), h: Math.round(r.height) };")
    expected = rect()
    check('the native page bounds follow the shell layout', wait(lambda: b.memory()['layout']['main'] == expected), (b.memory()['layout'], expected))
    last = {'x': 399, 'y': 7, 'w': 699, 'h': 599}
    # The shell may send its own layout after the burst; only a stale test rect means reordering.
    for _ in range(3):
        time.sleep(2)
        b.shell("for (let i = 0; i < 400; i++) window.ipc.postMessage(JSON.stringify({ cmd: 'layout', main: { x: i, y: 7, w: 300 + i, h: 200 + i } })); return true;")
        applied = wait(lambda: b.memory()['layout']['main'] in (last, rect()) and b.memory()['layout']['main'], 60)
        expected = rect()
        check('rapid layout messages are never applied out of order', applied in (last, expected), (b.memory()['layout'], expected))
        if applied == last:
            break
    check('400 rapid layout messages apply in order (the last one wins)', applied == last, b.memory()['layout'])
    b.shell("window.dispatchEvent(new Event('resize')); return true;")
    check('the real layout is restored', wait(lambda: b.memory()['layout']['main'] == expected), b.memory()['layout'])

    b.shell("__shell.openCmdk(''); return true;")
    check('the command bar covers the page', wait(lambda: b.memory()['layout']['covered']), b.memory()['layout'])
    frozen = wait(lambda: b.shell("const i = document.querySelector('#frozen-main'); return i.src.includes('/frozen/main') && i.complete && i.naturalWidth > 0 ? i.naturalWidth : 0;"), 20)
    check('the overlay backdrop screenshot loads through the token-protected route', frozen, frozen)
    b.shell("document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' })); return true;")
    check('closing the overlay uncovers the page', wait(lambda: not b.memory()['layout']['covered']), b.memory()['layout'])
    post({'cmd': 'glance', 'url': args.fixture + '/page/ipc-glance'})
    check('Glance opens over process messages', wait(lambda: b.memory()['layout']['glance'] and any(t['id'] == GLANCE for t in b.memory()['tabs'])), b.memory())
    b.shell("document.querySelector('#glance-close').click(); return true;")
    check('Glance closes from its button', wait(lambda: not any(t['id'] == GLANCE for t in b.memory()['tabs']) and not b.memory()['layout']['glance']), b.memory())

    after = b.memory()['shell_ipc']
    check('the shell used process messages, never the HTTP command route',
          after['http_commands'] == before['http_commands'] and after['process_messages'] >= before['process_messages'] + 410, (before, after))


def attacker_suite(b):
    before = b.memory()['shell_ipc']
    b.tool('open_tab', {'url': f'{args.fixture}/evil?api={API}'})
    attack = wait(lambda: (lambda v: v if v and v.get('done') else None)(b.evaluate("return JSON.parse(document.documentElement.dataset.attack || 'null')")), 20)
    check('hostile page ran its probes', attack, attack)
    check('a website has no window.ipc', attack['ipc'] == 'undefined' and attack['atriumShell'] == 'undefined', attack)
    results = attack['results']
    check('a website cannot read the API (CORS)', results['tabs_cors'].startswith('blocked') and results['shell_cors'].startswith('blocked'), results)
    time.sleep(2)
    urls = [t['url'] for t in b.tabs()['tabs']]
    check('blind no-cors POSTs to /api/browser/command and /api/tool do nothing', not any('evil-command' in u or 'evil-tool' in u for u in urls), urls)
    check('popups without a user gesture are blocked', not any('evil-popup' in u for u in urls), urls)
    check('a website cannot open a file: URL', not any(u.startswith('file:') for u in urls), urls)
    after = b.memory()['shell_ipc']
    check('no forged process messages and no HTTP commands from the page',
          after['rejected_messages'] == before['rejected_messages'] and after['http_commands'] == before['http_commands'], (before, after))

    b.evaluate(f"setTimeout(() => {{ location.href = {json.dumps(API + '/shell')}; }}, 0); return true;")
    time.sleep(2)
    where = b.evaluate("return { url: location.href, ipc: typeof window.ipc }")
    check('a website cannot navigate itself to the shell origin', where['url'].startswith(args.fixture) and where['ipc'] == 'undefined', where)
    frame = b.evaluate("const f = document.querySelector('#shell-frame'); let same = false; try { same = !!f.contentDocument && f.contentDocument.location.href.startsWith(%s); } catch (e) {} return same;" % json.dumps(API))
    check('the shell cannot be framed by a website', frame is False, frame)
    clone = b.tool('open_tab', {'url': API + '/shell'})
    clone_state = b.evaluate("return { url: location.href, ipc: typeof window.ipc }")
    check('tabs never load the shell origin', not clone_state['url'].startswith(API) and clone_state['ipc'] == 'undefined', (clone_state, clone))
    b.tool('close_tab', {})

    b.tool('open_tab', {'url': args.fixture + '/page/gesture'})
    b.evaluate("document.querySelector('#new-window').click(); return true;")
    time.sleep(2)
    check('a page script (or an agent read) cannot open a popup', not any(t['url'].endswith('/page/opened-by-link') for t in b.tabs()['tabs']), b.tabs())
    tree = b.tool('snapshot', {'interactive': True})['content'][0]['text']
    ref = re.search(r'link "Open in new tab" \[(\w+)\]', tree)
    check('the gesture fixture link is in the snapshot', ref, tree)
    # Background instances report every page hidden, so the tool's settle wait is throttled.
    b.tool('click', {'ref': ref.group(1)}, timeout=180)
    check('an agent click on a target=_blank link still opens a tab', wait(lambda: any(t['url'].endswith('/page/opened-by-link') for t in b.tabs()['tabs'])), b.tabs())

    for name, arguments in [('navigate', {'url': 'file:///etc/hosts'}), ('open_tab', {'url': 'chrome://version'}),
                            ('fetch', {'url': 'file:///etc/passwd'}), ('batch', {'steps': [{'action': 'navigate', 'url': 'file:///etc/hosts'}]})]:
        result = b.tool(name, arguments)
        text = json.dumps(result)
        check(f'the agent {name} tool refuses non-web URLs', result.get('isError') or 'can only open http' in text, result)
    urls = [t['url'] for t in b.tabs()['tabs']]
    check('no local file was opened by the agent', not any(u.startswith(('file:', 'chrome:')) for u in urls), urls)

    b.shell(f"setTimeout(() => {{ location.href = {json.dumps(args.fixture + '/page/shell-escape')}; }}, 0); return true;")
    time.sleep(2)
    escape = b.shell("return { url: location.href, ipc: typeof window.ipc }")
    check('the shell cannot be navigated away from its document', escape == {'url': API + '/shell', 'ipc': 'object'}, escape)
    check('a script-initiated shell navigation does not open a tab', not any('shell-escape' in t['url'] for t in b.tabs()['tabs']), b.tabs())


def http_suite(b):
    token = atrium_api.token(data_dir=b.profile)
    auth = {atrium_api.TOKEN_HEADER: token}
    for path, method in [('/api/browser/tabs', 'GET'), ('/api/browser/command', 'POST'), ('/api/tool', 'POST'), ('/frozen/main', 'GET'),
                         ('/api/debug/shell', 'POST'), ('/api/setup/profiles', 'GET'), ('/api/events', 'GET')]:
        status, _, _ = raw(method, path, {'Content-Type': 'application/json'}, '{}' if method == 'POST' else None)
        check(f'{path} requires the token', status == 401, status)
    status, _, _ = raw('GET', '/api/browser/tabs', {atrium_api.TOKEN_HEADER: 'x' * len(token)})
    check('a wrong token of the right length is refused', status == 401, status)
    for origin in ['https://untrusted.example', 'null', args.fixture, f'http://localhost:{args.port}']:
        status, _, _ = raw('POST', '/api/browser/command', {**auth, 'Origin': origin, 'Content-Type': 'application/json'}, '{"cmd":"newTab"}')
        check(f'Origin {origin} is refused even with the token', status == 403, status)
    for host in [f'rebind.attacker.test:{args.port}', f'localhost:{args.port}', '127.0.0.1', f'127.0.0.1:{args.port}.attacker.test']:
        for path in ['/api/browser/tabs', '/shell']:
            status, _, _ = raw('GET', path, auth, host=host)
            check(f'DNS-rebinding Host {host} is refused for {path}', status == 403, status)
    status, headers, _ = raw('OPTIONS', '/api/browser/command', {'Origin': 'https://untrusted.example', 'Access-Control-Request-Method': 'POST',
                                                                 'Access-Control-Request-Headers': 'x-atrium-token,content-type'})
    check('CORS preflights get no allow headers', status >= 400 and not any(k.startswith('access-control-') for k in headers), (status, headers))
    status, headers, _ = raw('GET', '/shell')
    check('/shell forbids framing and sniffing',
          status == 200 and headers.get('x-frame-options') == 'DENY' and "frame-ancestors 'none'" in headers.get('content-security-policy', '')
          and headers.get('x-content-type-options') == 'nosniff' and headers.get('referrer-policy') == 'no-referrer', headers)
    status, headers, _ = raw('GET', '/api/browser/tabs', auth)
    check('API responses are not cacheable', status == 200 and headers.get('cache-control') == 'no-store' and headers.get('x-content-type-options') == 'nosniff', headers)
    status, headers, _ = raw('GET', '/api/browser/tabs')
    check('refusals carry the same hardening headers', status == 401 and headers.get('x-frame-options') == 'DENY', headers)

    mode = stat.S_IMODE(os.stat(b.profile).st_mode)
    check('the data directory is private to the user', mode == 0o700, oct(mode))
    mode = stat.S_IMODE(os.stat(b.profile / 'api-token').st_mode)
    check('the token file is private to the user', mode == 0o600, oct(mode))
    mcp = subprocess.run([str(args.app.resolve()), '--data-dir', str(b.profile), 'mcp', '--api', 'http://attacker.example:7777'],
                         input=b'', capture_output=True, timeout=20)
    check('atrium mcp refuses to send the token to a non-loopback API', mcp.returncode != 0 and b'loopback' in mcp.stderr, mcp.stderr[-300:])


with tempfile.TemporaryDirectory(prefix='atrium-ipc-', dir='/private/tmp') as directory:
    root = Path(directory)
    browser = Instance(root / 'profile', {'ATRIUM_TEST_SHELL_EVAL': '1'})
    try:
        browser.ready()
        shell_suite(browser)
        attacker_suite(browser)
        http_suite(browser)
    finally:
        browser.stop()
    check('the token file is removed on exit', not (root / 'profile' / 'api-token').exists())

    # A release instance without the test hook: shell scripting is off, and an existing
    # world-readable data directory is tightened. Optionally it starts its own OpenCode.
    second = root / 'existing'
    second.mkdir(mode=0o755)
    os.chmod(second, 0o755)
    browser = Instance(second, opencode=not args.skip_opencode)
    try:
        browser.ready()
        denied = browser.shell('return 1')
        check('shell scripting is disabled without ATRIUM_TEST_SHELL_EVAL', 'disabled' in json.dumps(denied), denied)
        mode = stat.S_IMODE(os.stat(second).st_mode)
        check('an existing data directory is made private', mode == 0o700, oct(mode))
        if not args.skip_opencode:
            healthy = wait(lambda: browser.client.api('/api/state').get('healthy'), 90, 1)
            check('Atrium reaches the OpenCode server it started', healthy, browser.client.api('/api/state'))
            try:
                urllib.request.urlopen(f'http://127.0.0.1:{args.opencode_port}/global/health', timeout=5)
                status = 200
            except urllib.error.HTTPError as error:
                status = error.code
            check('the OpenCode server Atrium started requires a password', status == 401, status)
            check('/api/state does not reveal OpenCode credentials', '@' not in browser.client.api('/api/state')['opencode'])
    finally:
        browser.stop()
    if not args.skip_opencode:
        def closed():
            try:
                urllib.request.urlopen(f'http://127.0.0.1:{args.opencode_port}/global/health', timeout=2)
            except urllib.error.HTTPError:
                return False
            except (urllib.error.URLError, ConnectionError, TimeoutError):
                return True
            return False
        check('the started OpenCode server stops with the browser', wait(closed, 15, .5))

print(f'PASS: {len(passed)} secure IPC and local-surface checks', flush=True)
