#!/usr/bin/env python3
"""End-to-end extension management against real Chrome Web Store extensions.

Starts its own background instances (ATRIUM_MOCK_KEYCHAIN=1, ATRIUM_BACKGROUND_WINDOW=1) with the
watchdog, plus a fixture server, and checks:
  * install uBlock Origin Lite by ID and JSON Formatter by Web Store link (CRX3 verified),
  * nothing loads before approval; approval + restart loads them,
  * uBO Lite blocks ad/tracker requests from a local page, JSON Formatter formats JSON,
  * extension pages (the uBO Lite popup) open as tabs, windows.create becomes an Atrium tab,
  * disable and remove take effect after restart,
  * an update that adds permissions waits for re-approval while the old version keeps running,
  * the update2 protocol answers for the real Web Store extensions.
Needs network access to the Chrome Web Store and to the ad/tracker hosts being blocked.

    python3 tools/test_extensions.py --port 7852 --fixture-port 7853
"""
import argparse
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlparse, parse_qs

sys.path.insert(0, str(Path(__file__).parent))
import atrium_api
import crx_pack

ROOT = Path(__file__).resolve().parent.parent
UBOL = 'ddkjiahejlhfcafbddmgiahcphecmpfh'
JSON_FORMATTER = 'bcjindcccaagfpapjjmafapmmgkkhgoa'
JSON_FORMATTER_URL = f'https://chromewebstore.google.com/detail/json-formatter/{JSON_FORMATTER}'
BLOCKED = ['https://pagead2.googlesyndication.com/pagead/js/adsbygoogle.js', 'https://www.google-analytics.com/analytics.js']
ALLOWED = 'https://www.gstatic.com/generate_204'

parser = argparse.ArgumentParser()
parser.add_argument('--port', type=int, default=7852)
parser.add_argument('--fixture-port', type=int, default=7853)
parser.add_argument('--app', type=Path, default=ROOT / 'target/bundle/atrium.app/Contents/MacOS/atrium')
parser.add_argument('--data-dir', type=Path, default=Path(f'/private/tmp/atrium-extensions-e2e-{os.getpid()}'))
parser.add_argument('--watchdog', type=Path, default=ROOT.parent / 'watchdog.sh',
                    help='script run with each instance PID to kill it if frontmost (skipped if missing; a built-in guard always runs)')
args = parser.parse_args()
fixture = f'http://127.0.0.1:{args.fixture_port}'
work = Path(tempfile.mkdtemp(prefix='atrium-ext-test-'))


def synthetic(version, permissions):
    src = work / f'synthetic-{version}'
    src.mkdir()
    (src / 'manifest.json').write_text(json.dumps({
        'name': 'Atrium synthetic', 'version': version, 'manifest_version': 3,
        'permissions': permissions, 'update_url': f'{fixture}/updates.xml',
        'background': {'service_worker': 'sw.js'},
        'content_scripts': [{'matches': ['http://127.0.0.1/*'], 'js': ['content.js'], 'run_at': 'document_start'}],
    }))
    (src / 'content.js').write_text(f'document.documentElement.dataset.atriumSynthetic = "{version}";')
    (src / 'sw.js').write_text(
        "chrome.runtime.onInstalled.addListener(() => chrome.windows.create({ url: '%s/opened-by-extension.html' }));" % fixture)
    return crx_pack.pack(src, work / 'key.pem')


CRX = {}
CRX['1.0'], SYNTHETIC = synthetic('1.0', ['storage'])
CRX['2.0'], _ = synthetic('2.0', ['storage', 'cookies'])


class Fixture(BaseHTTPRequestHandler):
    def do_GET(self):
        url = urlparse(self.path)
        kind, body = 'text/html', b''
        if url.path == '/data.json':
            kind, body = 'application/json', b'{"hello": "world", "list": [1, 2, 3]}'
        elif url.path.endswith('.crx') and url.path[1:-4] in CRX:
            kind, body = 'application/x-chrome-extension', CRX[url.path[1:-4]]
        elif url.path == '/updates.xml':
            query = parse_qs(parse_qs(url.query).get('x', [''])[0])
            current = query.get('v', ['0'])[0]
            check = (f'<updatecheck codebase="{fixture}/2.0.crx" status="ok" version="2.0"/>' if current == '1.0'
                     else '<updatecheck status="noupdate"/>')
            kind = 'application/xml'
            body = f'<?xml version="1.0"?><gupdate protocol="2.0"><app appid="{SYNTHETIC}" status="ok">{check}</app></gupdate>'.encode()
        else:
            body = f'<!doctype html><title>Fixture {url.path}</title><div class="adsbygoogle ad-banner">ad</div><p>fixture</p>'.encode()
        self.send_response(200)
        self.send_header('Content-Type', kind)
        self.send_header('Content-Length', str(len(body)))
        self.send_header('Cache-Control', 'no-store')
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


server = ThreadingHTTPServer(('127.0.0.1', args.fixture_port), Fixture)
threading.Thread(target=server.serve_forever, daemon=True).start()

failure = []


def guard():
    """Kill any instance of this build that becomes frontmost, including relaunched ones."""
    while True:
        try:
            front = subprocess.run(['lsappinfo', 'front'], capture_output=True, text=True).stdout.strip()
            info = subprocess.run(['lsappinfo', 'info', '-only', 'pid', front], capture_output=True, text=True).stdout
            pid = ''.join(c for c in info if c.isdigit())
            if pid:
                command = subprocess.run(['ps', '-o', 'command=', '-p', pid], capture_output=True, text=True).stdout
                if str(args.app) in command and str(args.data_dir) in command:
                    os.kill(int(pid), signal.SIGKILL)
                    failure.append(f'instance {pid} became frontmost and was killed')
        except Exception:
            pass
        time.sleep(0.3)


threading.Thread(target=guard, daemon=True).start()
client = atrium_api.Client(f'http://127.0.0.1:{args.port}', data_dir=args.data_dir)
process = None
watchdogs = []


def check(condition, message):
    if failure:
        raise AssertionError(failure[0])
    if not condition:
        raise AssertionError(message)
    print('  ok:', message, flush=True)


def watch(pid):
    if not args.watchdog.exists():
        return
    log = open(f'{args.data_dir}.watchdog.log', 'a')
    watchdogs.append(subprocess.Popen(['/bin/sh', str(args.watchdog), str(pid)], stdout=log, stderr=log, start_new_session=True))


def instance_pids():
    """Main processes of this build using this test's data directory (helpers have other paths)."""
    out = subprocess.run(['pgrep', '-f', f'{args.app} --data-dir {args.data_dir} run'], capture_output=True, text=True).stdout
    return {int(p) for p in out.split()}


def wait_ready():
    for _ in range(300):
        try:
            memory = client.request('/api/browser/memory', timeout=5)
            if memory.get('shell_ready'):
                return memory['pid']
        except (urllib.error.URLError, ConnectionError, TimeoutError):
            pass
        time.sleep(0.2)
    raise AssertionError('the browser did not start')


def start():
    global process
    log = open(f'{args.data_dir}.log', 'a')
    process = subprocess.Popen([str(args.app), '--data-dir', str(args.data_dir), 'run', '--port', str(args.port),
                                '--opencode-url', 'http://127.0.0.1:1'],
                               env=atrium_api.instance_env(), stdout=log, stderr=log, start_new_session=True)
    watch(process.pid)
    wait_ready()


def stop():
    pid = client.request('/api/browser/memory', timeout=5)['pid']
    os.kill(pid, signal.SIGTERM)
    for _ in range(100):
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            break
        time.sleep(0.1)
    else:
        os.kill(pid, signal.SIGKILL)
    for _ in range(50):
        try:
            client.request('/api/browser/memory', timeout=1)
        except Exception:
            return
        time.sleep(0.1)


def restart():
    stop()
    start()


def extensions():
    return client.api('/api/browser/extensions')


def entry(ext_id):
    return next((e for e in extensions()['installed'] if e['id'] == ext_id), None)


def act(ext_id, action):
    return client.api(f'/api/browser/extensions/{ext_id}/{action}', {})


def evaluate(code):
    text = '\n'.join(c.get('text', '') for c in client.tool('evaluate', {'code': code}, timeout=60))
    return json.loads(text)


def network_results():
    client.tool('open_tab', {'url': f'{fixture}/ads.html'}, timeout=60)
    return evaluate("""
        const out = {};
        for (const url of %s) {
            try { await fetch(url, { mode: 'no-cors', cache: 'no-store' }); out[url] = 'loaded'; }
            catch (e) { out[url] = 'blocked'; }
        }
        out.marker = document.documentElement.dataset.atriumSynthetic || null;
        return out;""" % json.dumps(BLOCKED + [ALLOWED]))


def json_formatted():
    client.tool('open_tab', {'url': f'{fixture}/data.json'}, timeout=60)
    time.sleep(1)
    return evaluate("return !!document.querySelector('#jsonFormatterParsed')")


try:
    shutil.rmtree(args.data_dir, ignore_errors=True)
    print('install', flush=True)
    start()
    ubol = client.api('/api/browser/extensions/install', {'source': UBOL}, )
    check(ubol['id'] == UBOL and ubol['web_store'] and ubol['status'] == 'needs_approval', 'uBO Lite installed by ID, waiting for approval')
    check('declarativeNetRequest' in ubol['summary']['permissions'], 'uBO Lite permissions are shown for review')
    check(ubol['name'] == 'uBlock Origin Lite', f"localized name resolved: {ubol['name']}")
    formatter = client.api('/api/browser/extensions/install', {'source': JSON_FORMATTER_URL})
    check(formatter['id'] == JSON_FORMATTER and formatter['status'] == 'needs_approval', 'JSON Formatter installed by store link')
    synth = client.api('/api/browser/extensions/install', {'crx_url': f'{fixture}/1.0.crx'})
    check(synth['id'] == SYNTHETIC and not synth['web_store'], 'self-hosted CRX installed with an ID from its key')
    duplicate = client.request('/api/browser/extensions/install', {'source': UBOL})
    check('already installed' in duplicate.get('error', ''), 'duplicate install refused')
    bad = client.request('/api/browser/extensions/install', {'source': 'https://example.com/detail/x/' + UBOL})
    check('error' in bad, 'non-store links refused')
    check(extensions()['restart_required'] is False, 'unapproved extensions need no restart')
    enable = client.request(f'/api/browser/extensions/{UBOL}/enable', {})
    check('approve' in enable.get('error', ''), 'enable without approval refused')

    print('unapproved extensions do not load', flush=True)
    restart()
    net = network_results()
    check(net[BLOCKED[0]] == 'loaded' and net[ALLOWED] == 'loaded', f'no blocking before approval: {net}')
    check(not json_formatted(), 'JSON is not formatted before approval')

    print('approve and restart through the API', flush=True)
    for ext_id in (UBOL, JSON_FORMATTER, SYNTHETIC):
        check(act(ext_id, 'approve')['extension']['status'] == 'enabled', f'{ext_id} approved')
    check(extensions()['restart_required'] is True, 'restart required after approval')
    old_pid = client.request('/api/browser/memory')['pid']
    client.api('/api/browser/extensions/restart', {})
    relaunched = set()
    for _ in range(300):
        relaunched = instance_pids() - {old_pid}
        if relaunched:
            break
        time.sleep(0.1)
    check(len(relaunched) == 1, f'relaunched process found: {relaunched}')
    pid = relaunched.pop()
    watch(pid)
    check(wait_ready() == pid, 'relaunched instance is ready')
    check(pid != old_pid, f'relaunched as {pid}')
    state = extensions()
    check(state['restart_required'] is False and all(e['loaded'] for e in state['installed']), 'all approved extensions loaded')
    time.sleep(3)
    net = network_results()
    check(all(net[u] == 'blocked' for u in BLOCKED), f'uBO Lite blocks ad and tracker requests: {net}')
    check(net[ALLOWED] == 'loaded', 'uBO Lite allows other requests')
    check(net['marker'] == '1.0', 'synthetic content script v1 runs')
    check(json_formatted(), 'JSON Formatter formats a JSON response')
    tabs = client.api('/api/browser/tabs')['tabs']
    check(any(t['url'].endswith('/opened-by-extension.html') for t in tabs), 'windows.create opened an Atrium tab')
    popup = entry(UBOL)['popup']
    check(popup == f'chrome-extension://{UBOL}/popup.html', 'popup URL is offered')
    client.tool('open_tab', {'url': popup}, timeout=60)
    time.sleep(1)
    page = evaluate("return { id: chrome.runtime.id, text: document.body.innerText.length, storage: typeof chrome.storage.local }")
    check(page['id'] == UBOL and page['text'] > 0 and page['storage'] == 'object', f'uBO Lite popup page renders with extension APIs: {page}')

    print('updates', flush=True)
    updates = client.api('/api/browser/extensions/updates', {}, timeout=180)
    by_id = {r['id']: r for r in updates['results']}
    check(by_id[UBOL]['status'] in ('up_to_date', 'updated'), f"Web Store update2 check for uBO Lite: {by_id[UBOL]['status']}")
    check(by_id[JSON_FORMATTER]['status'] in ('up_to_date', 'updated'), 'Web Store update2 check for JSON Formatter')
    check(by_id[SYNTHETIC]['status'] == 'needs_approval' and 'cookies' in by_id[SYNTHETIC]['added'], f'update adding permissions waits: {by_id[SYNTHETIC]}')
    synth = entry(SYNTHETIC)
    check(synth['status'] == 'update_needs_approval' and synth['version'] == '1.0' and synth['pending']['version'] == '2.0', 'old version stays installed')

    print('disable and pending update across restart', flush=True)
    act(JSON_FORMATTER, 'disable')
    check(extensions()['restart_required'] is True, 'disable requires restart')
    restart()
    check(not json_formatted(), 'disabled JSON Formatter no longer runs')
    net = network_results()
    check(net['marker'] == '1.0' and net[BLOCKED[0]] == 'blocked', 'unapproved update did not load; v1 and uBO Lite still run')

    print('approve update, remove uBO Lite', flush=True)
    check(act(SYNTHETIC, 'approve')['extension']['version'] == '2.0', 'update approved')
    ubol_dir = Path(next(e for e in extensions()['installed'] if e['id'] == UBOL)['summary']['path'])
    act(UBOL, 'remove')
    check(entry(UBOL) is None, 'uBO Lite removed from the list')
    restart()
    net = network_results()
    check(net['marker'] == '2.0', 'approved update v2 runs after restart')
    check(net[BLOCKED[0]] == 'loaded', 'removed uBO Lite no longer blocks')
    check(not ubol_dir.exists(), 'removed extension files deleted')
    act(JSON_FORMATTER, 'enable')
    restart()
    check(json_formatted(), 're-enabled JSON Formatter runs again')
    print('extensions: passed', flush=True)
finally:
    try:
        stop()
    except Exception:
        pass
    for pid in instance_pids():
        os.kill(pid, signal.SIGKILL)
    for w in watchdogs:
        try:
            w.wait(timeout=10)
        except subprocess.TimeoutExpired:
            w.terminate()
    server.shutdown()
    shutil.rmtree(work, ignore_errors=True)
    if failure:
        print('FAILED:', failure[0], file=sys.stderr)
        sys.exit(1)
