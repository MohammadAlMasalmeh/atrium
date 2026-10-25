#!/usr/bin/env python3
"""Start an isolated background browser and check the About page, the product
identity in the bundle, and that favicons come from pages rather than Google.

Uses its own fixture server (pages with declared favicons), launches the bundle
with ATRIUM_MOCK_KEYCHAIN=1 ATRIUM_BACKGROUND_WINDOW=1 and a fresh data directory,
and runs the watchdog next to it. Exits non-zero on any failure.
"""
import argparse
import hashlib
import json
import plistlib
import re
import struct
import subprocess
import threading
import time
import urllib.error
import urllib.request
import zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import atrium_api

ROOT = Path(__file__).resolve().parent.parent
WATCHDOG = Path('/Users/mohammadalmasalmeh/Desktop/atrium-rs-wt/watchdog.sh')

parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
parser.add_argument('--app', type=Path, default=ROOT / 'target/bundle/Atrium.app')
parser.add_argument('--port', type=int, default=7891)
parser.add_argument('--fixture-port', type=int, default=7892)
parser.add_argument('--data-dir', type=Path, default=Path(f'/private/tmp/atrium-legal-{int(time.time())}'))
args = parser.parse_args()


def brand(name):
    return re.search(rf'pub const {name}: &str = "([^"]*)";', (ROOT / 'src/brand.rs').read_text()).group(1)


def digest(relative):
    return hashlib.sha256((ROOT / relative).read_text().strip().encode()).hexdigest()


def png(width, height, rgb):
    rows = b''.join(b'\0' + bytes(rgb) * width for _ in range(height))
    chunk = lambda kind, data: struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))
    return (b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', width, height, 8, 2, 0, 0, 0))
            + chunk(b'IDAT', zlib.compress(rows)) + chunk(b'IEND', b''))


ICON = png(32, 32, (220, 40, 120))
REQUESTS = []


class Fixture(BaseHTTPRequestHandler):
    def do_GET(self):
        REQUESTS.append(self.path)
        if self.path == '/brand-icon.png':
            body, kind = ICON, 'image/png'
        else:
            body = (b'<!doctype html><meta charset="utf-8"><title>Favicon fixture</title>'
                    b'<link rel="icon" href="/brand-icon.png"><h1>Favicon fixture</h1>')
            kind = 'text/html; charset=utf-8'
        self.send_response(200)
        self.send_header('Content-Type', kind)
        self.send_header('Cache-Control', 'no-store')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


def raw(path, token=True, headers=None):
    """GET without failing on HTTP errors: (status, headers, body)."""
    sent = dict(headers or {})
    if token:
        sent[atrium_api.TOKEN_HEADER] = atrium_api.token(data_dir=args.data_dir)
    request = urllib.request.Request(base + path, headers=sent)
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            return response.status, response.headers, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.headers, error.read()


def evaluate(code):
    result = tool('evaluate', {'code': code})
    return json.loads('\n'.join(item.get('text', '') for item in result))


def wait(predicate, what, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            value = predicate()
            if value:
                return value
        except (urllib.error.URLError, TimeoutError, AssertionError):
            pass
        time.sleep(.4)
    raise AssertionError(f'Timed out waiting for {what}')


def process_tree(root):
    pids, frontier = {root}, [root]
    while frontier:
        out = subprocess.run(['pgrep', '-P', str(frontier.pop())], capture_output=True, text=True).stdout
        for pid in map(int, out.split()):
            if pid not in pids:
                pids.add(pid)
                frontier.append(pid)
    return pids


def external_connections(root):
    pids = ','.join(map(str, process_tree(root)))
    out = subprocess.run(['lsof', '-a', '-nP', '-i', '-p', pids], capture_output=True, text=True).stdout
    found = []
    for line in out.splitlines()[1:]:
        name = line.split(None, 8)[-1]
        remote = name.split('->', 1)[1] if '->' in name else ''
        if remote and not re.match(r'(127\.0\.0\.1|\[::1\]|localhost):', remote):
            found.append(line)
    return found


def check(condition, message):
    if not condition:
        raise AssertionError(message)
    print('PASS:', message, flush=True)


product, bundle_id = brand('PRODUCT_NAME'), brand('BUNDLE_ID')
executable = args.app / 'Contents/MacOS' / brand('EXECUTABLE')
base = f'http://127.0.0.1:{args.port}'
assert not args.data_dir.exists(), f'{args.data_dir} must be a fresh directory'
args.data_dir.mkdir(parents=True)

info = plistlib.loads((args.app / 'Contents/Info.plist').read_bytes())
check(info['CFBundleIdentifier'] == bundle_id and info['CFBundleDisplayName'] == product
      and info['CFBundleName'] == product and product in info['NSCameraUsageDescription'],
      f'Info.plist uses the configured identity ({product}, {bundle_id})')
cargo_version = re.search(r'^version = "([^"]+)"', (ROOT / 'Cargo.toml').read_text(), re.M).group(1)

fixture = ThreadingHTTPServer(('127.0.0.1', args.fixture_port), Fixture)
threading.Thread(target=fixture.serve_forever, daemon=True).start()
fixture_url = f'http://127.0.0.1:{args.fixture_port}'

log = (args.data_dir.parent / f'{args.data_dir.name}.log').open('w')
process = subprocess.Popen([str(executable), '--data-dir', str(args.data_dir), 'run', '--port', str(args.port),
                            '--opencode-url', 'http://127.0.0.1:1'],
                           stdout=log, stderr=log,
                           env={**atrium_api.instance_env(), 'ATRIUM_DEBUG_API': '1'})
watchdog = subprocess.Popen(['/bin/sh', str(WATCHDOG), str(process.pid)], stdout=log, stderr=log)
client = atrium_api.Client(base, data_dir=args.data_dir)
api, tool = client.api, client.tool
failed = False
try:
    wait(lambda: api('/api/browser/memory').get('shell_ready'), 'the shell')

    status, headers, body = raw('/about', token=False)
    check(status == 200 and b'About ' + product.encode() in body, 'About page is served without the API token')
    check("frame-ancestors 'none'" in headers.get('Content-Security-Policy', '') and headers.get('X-Frame-Options') == 'DENY',
          'About page forbids framing and scripts')
    status, _, _ = raw('/about', token=False, headers={'Origin': 'https://untrusted.example'})
    check(status == 403, 'About page rejects a foreign Origin')
    status, _, _ = raw('/favicon/example.com', token=False)
    check(status == 401, 'Favicon cache still requires the API token')

    status, _, shell = raw('/shell', token=False)
    check(f'<title>{product}</title>'.encode() in shell and b'{{PRODUCT_NAME}}' not in shell,
          'Shell title comes from the product name')
    check(b's2/favicons' not in shell and b'gstatic.com' not in shell and b'google.com/s2' not in shell,
          'Shell no longer references Google favicon services')

    api('/api/browser/command', {'cmd': 'menu', 'name': 'about'})
    def about_loaded():
        tabs = api('/api/browser/tabs')
        return tabs if any(t['url'] == base + '/about' and not t['loading'] for t in tabs['tabs']) else None
    tabs = wait(about_loaded, 'the About tab')
    about = next(t for t in tabs['tabs'] if t['url'] == base + '/about')
    check(tabs['active'] == about['id'], 'App menu > About opens the About page in a tab')
    tool('switch_tab', {'index': [t['id'] for t in tabs['tabs'] if t['kind'] != 'scratch'].index(about['id'])})
    # Runs in the About page's own world: crypto.subtle and navigator.userAgentData are not
    # exposed to the agent's isolated world on this page.
    page = api('/api/debug/cdp', {'tab': about['id'], 'method': 'Runtime.evaluate', 'params': {
        'expression': '(async () => {' + """
        const text = (id) => document.getElementById(id)?.textContent ?? null;
        const notices = text('notices-text');
        return { title: document.title, product: text('product-name'), version: text('app-version'),
                 chromium: text('chromium-version'), cef: text('cef-version'), bundle: text('bundle-id'),
                 license: text('project-license'), notices,
                 crates: ['option-ext 0.2.0', 'webpki-roots', 'tokio', 'cef 156'].filter((c) => notices.includes(c)).length,
                 credits: document.getElementById('chromium-credits')?.getAttribute('href') ?? null,
                 cefLicense: document.querySelector('#cef-license + pre').textContent,
                 chromiumLicense: document.querySelector('#chromium-license + pre').textContent };
    """ + '})()', 'awaitPromise': True, 'returnByValue': True}})['result']
    assert 'value' in page.get('result', {}) and not page.get('exceptionDetails'), page
    page = page['result']['value']
    # The About page's opaque origin has no crypto.subtle, so texts are hashed here.
    for key in ('notices', 'cefLicense', 'chromiumLicense'):
        page[key] = hashlib.sha256(page[key].strip().encode()).hexdigest()

    # The About page's sandboxed origin has no navigator.userAgentData and the shell is WebKit,
    # so the running engine's version is read from an ordinary page.
    probe = tool('open_tab', {'url': f'http://127.0.0.1:{args.fixture_port}/page/version'})
    probe_tab = wait(lambda: next((t['id'] for t in api('/api/browser/tabs')['tabs'] if t['url'].endswith('/page/version') and not t['loading']), None), 'the version probe tab')
    page['runtime'] = api('/api/debug/cdp', {'tab': probe_tab, 'method': 'Runtime.evaluate', 'params': {
        'expression': "navigator.userAgentData.getHighEntropyValues(['fullVersionList']).then((b) => b.fullVersionList.find((x) => x.brand === 'Chromium')?.version ?? null)",
        'awaitPromise': True, 'returnByValue': True}})['result']['result']['value']
    check(page['title'] == f'About {product}' and page['product'] == product, 'About page shows the product name')
    check(page['version'] == cargo_version, f'About page shows the app version {cargo_version}')
    check(page['chromium'] == page['runtime'],
          f'Chromium version {page["chromium"]} matches the running engine ({page["runtime"]})')
    check(bool(re.fullmatch(r'\d+\.\d+\.\d+\+g[0-9a-f]+', page['cef'] or '')), f'About page shows the CEF version {page["cef"]}')
    check(page['bundle'] == bundle_id and page['license'] == brand('PROJECT_LICENSE'),
          'About page shows the bundle ID and project license')
    check(page['notices'] == digest('THIRD-PARTY-NOTICES.txt') and page['crates'] == 4,
          'About page shows the complete THIRD-PARTY-NOTICES.txt')
    check(page['cefLicense'] == digest('docs/licenses/CEF-LICENSE.txt')
          and page['chromiumLicense'] == digest('docs/licenses/Chromium-LICENSE.txt'),
          'About page shows the CEF and Chromium licenses')
    status, headers, credits = raw(page['credits'], token=False)
    shipped = (args.app / 'Contents/Resources/Chromium-CREDITS.html').read_bytes()
    check(status == 200 and credits == shipped and len(credits) > 1_000_000,
          f'Chromium credits link serves the bundled Chromium-CREDITS.html ({len(credits):,} bytes)')

    api('/api/browser/command', {'cmd': 'menu', 'name': 'about'})
    time.sleep(1)
    tabs = api('/api/browser/tabs')
    check(sum(t['url'] == base + '/about' for t in tabs['tabs']) == 1, 'Opening About again reuses its tab')

    tool('open_tab', {'url': fixture_url + '/favicon-page'})
    tab = wait(lambda: next((t for t in api('/api/browser/tabs')['tabs']
                             if t['url'].startswith(fixture_url) and (t['icon'] or '').startswith('/favicon/127.0.0.1?v=')), None),
               'the page favicon to be cached')
    check('/brand-icon.png' in REQUESTS, 'The declared favicon was fetched from the page’s own server')
    status, headers, icon = raw('/favicon/127.0.0.1')
    width, height = struct.unpack('>II', icon[16:24]) if icon.startswith(b'\x89PNG') else (0, 0)
    check(status == 200 and headers['Content-Type'] == 'image/png' and 0 < width <= 64 and 0 < height <= 64,
          f'Shell icon is served from the local cache ({width}x{height} PNG)')
    check((args.data_dir / 'favicons/127.0.0.1.png').is_file(), 'Favicon is stored in <data dir>/favicons')
    status, headers, generic = raw('/favicon/never-visited.example')
    check(status == 200 and headers['Content-Type'] == 'image/svg+xml', 'Unknown sites get a local generic icon')

    # Chromium's own background services still reach Google (see PRIVACY.md); report them, don't fail.
    connections = external_connections(process.pid)
    print(f'NOTE: {len(connections)} non-loopback connection(s) open; the test loads only loopback pages'
          + ''.join('\n  ' + line for line in connections), flush=True)
    api('/api/browser/command', {'cmd': 'quit'})
    process.wait(timeout=30)
    local_state = json.loads((args.data_dir / 'chromium/Local State').read_text())
    print('NOTE: Local State records' + ''.join(f' {key}' for key in ('updateclientdata', 'network_time') if key in local_state),
          flush=True)
except BaseException:
    failed = True
    raise
finally:
    try:
        if process.poll() is None:
            api('/api/browser/command', {'cmd': 'quit'})
            process.wait(timeout=20)
    except Exception:
        process.terminate()
        process.wait(timeout=10)
    watchdog.wait(timeout=5)
    fixture.shutdown()
    log.close()
    frontmost = 'FRONTMOST' in Path(log.name).read_text()
    if frontmost:
        print('FAIL: the instance became frontmost and was killed by the watchdog', flush=True)
    if not failed and not frontmost:
        print('PASS: About page, product identity, and local favicons', flush=True)
    if frontmost:
        raise SystemExit(1)
