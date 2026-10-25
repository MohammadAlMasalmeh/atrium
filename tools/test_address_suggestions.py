#!/usr/bin/env python3
"""Exercise the top address dropdown in a background browser with a throwaway profile."""
import argparse
import json
import subprocess
import tempfile
import time
import urllib.error
from urllib.parse import parse_qs, urlparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from threading import Thread

import atrium_api

ROOT = Path(__file__).resolve().parent.parent
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--app', type=Path, default=ROOT / 'target/bundle/Atrium.app/Contents/MacOS/atrium')
parser.add_argument('--port', type=int, default=7837)
args = parser.parse_args()


class Fixture(BaseHTTPRequestHandler):
    predictions = []
    completed = []

    def do_GET(self):
        parsed = urlparse(self.path)
        if parsed.path == '/suggest':
            query = parse_qs(parsed.query).get('q', [''])[0]
            self.predictions.append((query, dict(self.headers)))
            if query == 'new yo':
                time.sleep(.6)
            if query == 'failing search':
                self.send_error(503)
                self.completed.append(query)
                return
            terms = {'new yo': ['Stale prediction'], 'new york': ['new york city', 'new york giants']}.get(query, [])
            body = json.dumps([query, terms]).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            try:
                self.wfile.write(body)
            except BrokenPipeError:
                pass
            self.completed.append(query)
            return
        body = b'<!doctype html><title>Address fixture</title><h1>Address fixture</h1>'
        self.send_response(200)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


def wait(fn, label, timeout=30):
    end = time.monotonic() + timeout
    last = None
    while time.monotonic() < end:
        try:
            last = fn()
            if last:
                return last
        except (urllib.error.URLError, TimeoutError, ConnectionError):
            pass
        time.sleep(.05)
    raise AssertionError(f'Timed out: {label}: {last}')


with tempfile.TemporaryDirectory(prefix='atrium-address-') as directory:
    profile = Path(directory)
    server = ThreadingHTTPServer(('127.0.0.1', 0), Fixture)
    Thread(target=server.serve_forever, daemon=True).start()
    base = f'http://127.0.0.1:{server.server_port}'
    imported = profile / 'import'
    imported.mkdir()
    (imported / 'search-engine.json').write_text(json.dumps({
        'name': 'Fixture', 'keyword': 'fixture', 'template': base + '/search?q={searchTerms}'}))
    (imported / 'bookmarks.json').write_text(json.dumps({'sources': {'Default': {'roots': [
        {'id': '1', 'title': 'Autocomplete bookmark', 'url': base + '/bookmark'}]}}}))
    with (profile / 'app.log').open('w') as log:
        app = subprocess.Popen([str(args.app.resolve()), '--data-dir', directory, 'run', '--port', str(args.port),
                                '--opencode-url', 'http://127.0.0.1:1'], stdout=log, stderr=log,
                               env={**atrium_api.instance_env(), 'ATRIUM_NATIVE_CHROME': '0', 'ATRIUM_TEST_SHELL_EVAL': '1',
                                    'ATRIUM_SEARCH_SUGGESTIONS': '1', 'ATRIUM_TEST_SUGGEST_URL': base + '/suggest'})
        client = atrium_api.Client(f'http://127.0.0.1:{args.port}', data_dir=profile)

        def evaluate(js):
            result = client.request('/api/debug/shell', js, raw=True)
            assert not (isinstance(result, dict) and 'error' in result), result
            return result

        def query(text):
            return evaluate(f'''const i = document.querySelector('#top-input'); i.value = {json.dumps(text)};
                i.dispatchEvent(new Event('input', {{bubbles:true}})); return true;''')

        def key(name, **options):
            return evaluate(f'''document.querySelector('#top-input').dispatchEvent(new KeyboardEvent('keydown',
                {{key: {json.dumps(name)}, bubbles: true, cancelable: true, ...{json.dumps(options)}}})); return true;''')

        def rows():
            return evaluate("return [...document.querySelectorAll('#cmdk-list .ci')].map(e => e.textContent);")

        def opened():
            evaluate("document.querySelector('#top-url').click(); return true;")
            assert evaluate("return document.body.classList.contains('top-editing') && !document.querySelector('#app').inert;")

        def active():
            state = client.api('/api/browser/tabs')
            return next(t for t in state['tabs'] if t['id'] == state['active'])

        try:
            wait(lambda: client.api('/api/browser/memory').get('shell_ready'), 'shell ready', 60)
            evaluate('window.atriumOnboarding?.close(); return true;')
            client.command(cmd='newTab', url=base + '/start')
            wait(lambda: active()['url'] == base + '/start' and not active()['loading'], 'fixture')
            evaluate('''if (!document.body.classList.contains('sb-hidden')) document.querySelector('#sb-toggle').click();
                window.__shell.S.hist = [{title:'Autocomplete history', url:''' + json.dumps(base + '/history') + '''}];
                return true;''')
            opened()
            query('Autocomplete')
            wait(lambda: any('Autocomplete bookmark' in r for r in rows()), 'imported bookmark')
            assert any('Autocomplete history' in r for r in rows()), rows()
            assert evaluate('''const i = document.querySelector('#top-input'), b = i.getBoundingClientRect();
                const d = document.querySelector('#cmdk').getBoundingClientRect();
                return i.getAttribute('aria-expanded') === 'true' && document.activeElement === i
                  && Math.abs(d.left-b.left) < 1 && d.top >= b.bottom && Math.abs(d.width-b.width) < 1
                  && document.elementFromPoint(b.left+b.width/2,b.top+b.height/2) === i
                  && getComputedStyle(document.querySelector('.cmdk-in')).display === 'none';''')
            print('PASS: typing shows history and imported bookmarks under the focused address field', flush=True)

            before = len(client.api('/api/browser/tabs')['tabs'])
            for _ in range(10):
                if evaluate("return document.querySelector('#cmdk-list .sel')?.textContent.includes('Autocomplete history');"):
                    break
                key('ArrowDown')
            else:
                raise AssertionError('History was not selectable')
            key('Enter', isComposing=True)
            assert evaluate("return document.body.classList.contains('top-editing');")
            key('Enter')
            wait(lambda: active()['url'] == base + '/history' and not active()['loading'], 'keyboard navigation')
            assert len(client.api('/api/browser/tabs')['tabs']) == before
            print('PASS: arrows and Enter navigate the existing tab; composition does not submit', flush=True)

            opened()
            query('Autocomplete')
            wait(lambda: any('Autocomplete bookmark' in r for r in rows()), 'bookmark again')
            assert evaluate('''const row = [...document.querySelectorAll('#cmdk-list .ci')].find(e => e.textContent.includes('Autocomplete bookmark'));
                const b = row.getBoundingClientRect();
                return document.elementFromPoint(b.left+8,b.top+8)?.closest('.ci') === row;''')
            assert evaluate('''const row = [...document.querySelectorAll('#cmdk-list .ci')].find(e => e.textContent.includes('Autocomplete bookmark'));
                const prevented = !row.dispatchEvent(new MouseEvent('mousedown', {bubbles:true, cancelable:true}));
                row.click(); return prevented;''')
            wait(lambda: active()['url'] == base + '/bookmark' and not active()['loading'], 'mouse navigation')
            print('PASS: clicking a visible suggestion navigates without losing it on blur', flush=True)

            client.command(cmd='newTab', url=base + '/other')
            wait(lambda: active()['url'] == base + '/other' and not active()['loading'], 'second tab')
            before = len(client.api('/api/browser/tabs')['tabs'])
            opened()
            query('/bookmark')
            assert evaluate('''const group = [...document.querySelectorAll('#cmdk-list [role=group]')]
                .find(e => e.querySelector('.cg')?.textContent === 'Switch to Tab');
                if (!group) return false; group.querySelector('.ci').click(); return true;''')
            wait(lambda: active()['url'] == base + '/bookmark', 'switch to existing tab')
            assert len(client.api('/api/browser/tabs')['tabs']) == before
            opened()
            query('Autocomplete')
            evaluate("document.querySelector('#scrim').click(); return true;")
            assert evaluate('return !window.__shell.S.overlay;')
            print('PASS: open-tab results switch tabs and clicking outside dismisses the dropdown', flush=True)

            opened()
            query('Autocomplete')
            key('Escape')
            assert evaluate("return !document.body.classList.contains('top-editing') && !window.__shell.S.overlay && document.querySelector('#top-input').getAttribute('aria-expanded') === 'false';")
            opened()
            query('')
            key('ArrowDown')
            key('Enter')
            assert active()['url'] == base + '/bookmark'
            assert evaluate('return !window.__shell.S.overlay;')
            print('PASS: Escape and an empty submission dismiss safely', flush=True)

            opened()
            query('new yo')
            wait(lambda: any(q == 'new yo' for q, _ in Fixture.predictions), 'delayed prediction request')
            query('new york')
            key('ArrowDown')  # Keep the assistant selected while predictions arrive above it.
            wait(lambda: any('new york city' in r for r in rows()) and any('new york giants' in r for r in rows()), 'search predictions')
            assert evaluate("return document.querySelector('#cmdk-list .sel').textContent.includes('Ask the assistant');")
            wait(lambda: 'new yo' in Fixture.completed, 'old request completes')
            assert not any('Stale prediction' in r for r in rows()), rows()
            for _ in range(10):
                if evaluate("return document.querySelector('#cmdk-list .sel')?.querySelector('.t').textContent === 'new york city';"):
                    break
                key('ArrowUp')
            else:
                raise AssertionError('Search prediction was not selectable')
            before = len(client.api('/api/browser/tabs')['tabs'])
            key('Enter')
            wait(lambda: active()['url'] == base + '/search?q=new+york+city' and not active()['loading'], 'full prediction search')
            assert len(client.api('/api/browser/tabs')['tabs']) == before
            assert all(not any(k.lower() in ('cookie', 'authorization', 'x-atrium-token') for k in headers)
                       for _, headers in Fixture.predictions)
            print('PASS: live search predictions preserve selection, reject stale results, and search the full phrase without profile credentials', flush=True)

            opened()
            query('failing search')
            wait(lambda: 'failing search' in Fixture.completed, 'provider failure')
            assert evaluate("return document.querySelector('#cmdk-list .ci .t').textContent === 'failing search';")
            key('Escape')
            # A blocked address must not reach even the test provider.
            client.api('/api/setup/search-suggest?q=https%3A%2F%2Fprivate.example%2Fsecret')
            assert not any('private.example' in q for q, _ in Fixture.predictions)
            print('PASS: provider failures keep search usable and URL input is excluded', flush=True)

            opened()
            evaluate('''window.originalFetch = window.fetch;
                window.fetch = (url, ...args) => String(url).includes('/api/setup/suggest')
                  ? new Promise(resolve => { window.finishSuggest = resolve; }) : originalFetch(url, ...args);
                return true;''')
            query('stale query')
            query('')
            evaluate('''finishSuggest({json:async () => ({results:[{url:'https://stale.invalid/', title:'Stale result', kind:'bookmark'}]})});
                window.fetch = originalFetch; return true;''')
            assert not any('Stale result' in r for r in rows()), rows()
            key('Escape')
            evaluate("window.__shell.openCmdk('New Tab'); return true;")
            assert any('New Tab' in r for r in rows()), rows()
            assert evaluate("return !document.body.classList.contains('top-editing') && document.querySelector('#app').inert && getComputedStyle(document.querySelector('.cmdk-in')).display !== 'none';")
            print('PASS: late responses stay out of cleared results and the command palette still works', flush=True)
        finally:
            try:
                client.command(cmd='quit')
                app.wait(timeout=20)
            except (urllib.error.URLError, subprocess.TimeoutExpired):
                app.terminate()
                app.wait(timeout=10)
            server.shutdown()
