#!/usr/bin/env python3
"""Find, zoom, print/save, PDF viewing, context menu, shortcuts, default-browser
routing, and downloads against one background Aside instance.

Exits non-zero on failure. Never calls LSSetDefaultHandlerForURLScheme, never
opens the system print or save panels, and never reveals a file in Finder.
"""
import json
import plistlib
import shutil
import struct
import subprocess
import threading
import time
import urllib.error
import zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlparse

import aside_api

ROOT = Path(__file__).resolve().parents[1]
APP = ROOT / 'target/bundle/aside.app/Contents/MacOS/aside'
PLIST = ROOT / 'target/bundle/aside.app/Contents/Info.plist'
WATCHDOG = Path('/Users/mohammadalmasalmeh/Desktop/aside-rs-wt/watchdog.sh')
PORT = 7810
PAGE_PORT = 7811
DATA = Path('/private/tmp/aside-commands-1')
OUT = Path('/private/tmp/aside-commands-out')
FILE_PAGE = Path('/private/tmp/aside-commands-file.html')
BASE = f'http://127.0.0.1:{PAGE_PORT}'
API = f'http://127.0.0.1:{PORT}'


def png_bytes():
    def chunk(tag, data):
        return struct.pack('>I', len(data)) + tag + data + struct.pack('>I', zlib.crc32(tag + data) & 0xFFFFFFFF)
    raw = b''.join(b'\x00' + bytes((180, 40, 40)) * 8 for _ in range(8))
    ihdr = struct.pack('>IIBBBBB', 8, 8, 8, 2, 0, 0, 0)
    return b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', ihdr) + chunk(b'IDAT', zlib.compress(raw)) + chunk(b'IEND', b'')


def pdf_bytes(text):
    stream = f'BT /F1 24 Tf 40 90 Td ({text}) Tj ET'.encode()
    objects = [
        b'1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n',
        b'2 0 obj<</Type/Pages/Count 1/Kids[3 0 R]>>endobj\n',
        b'3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 360 200]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>endobj\n',
        f'4 0 obj<</Length {len(stream)}>>stream\n'.encode() + stream + b'\nendstream\nendobj\n',
        b'5 0 obj<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>endobj\n',
    ]
    header = b'%PDF-1.4\n%\xe2\xe3\xcf\xd3\n'
    body = b''
    offsets = [0]
    pos = len(header)
    for obj in objects:
        offsets.append(pos)
        body += obj
        pos += len(obj)
    xref = f'xref\n0 {len(offsets)}\n0000000000 65535 f \n'.encode()
    for off in offsets[1:]:
        xref += f'{off:010d} 00000 n \n'.encode()
    trailer = f'trailer<</Size {len(offsets)}/Root 1 0 R>>\nstartxref\n{pos}\n%%EOF\n'.encode()
    return header + body + xref + trailer


PNG = png_bytes()
PDF = pdf_bytes('Hello PDF')
PAGES = {
    '/page/one': '<title>One</title><h1>Page one</h1><p>alpha one</p>',
    '/page/two': '<title>Two</title><h1>Page two</h1><div id="blank" style="height:90px"></div>',
    '/page/three': '<title>Three</title><h1>Page three</h1>',
    '/page/find': '<title>Find</title><p id="a">alpha one</p><p>alpha two</p><p>alpha three</p>',
    '/page/save': '<title>Save Target</title><h1>SAVE-MARKER</h1><p>Keep this sentence.</p>',
    '/page/hist': '<title>Hist</title><p id="mark">history</p><div id="blank" style="height:90px"></div>',
    '/page/menu': '''<title>Menu</title>
        <div id="blank" style="height:70px">blank area</div>
        <p><a id="link" href="/page/external">Open this link</a></p>
        <p><a id="save-link" href="/file.bin">Download this link</a></p>
        <p><img id="pic" src="/pixel.png" width="48" height="48" alt="pixel"></p>
        <p id="words">selected words here</p>''',
    '/page/external': '<title>External</title><p>opened from outside</p>',
    '/page/delegate': '<title>Delegate</title><p>opened by another app</p>',
}


class Handler(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def do_GET(self):
        path = urlparse(self.path).path
        if path == '/pixel.png':
            return self._send(PNG, 'image/png')
        if path == '/file.bin':
            return self._send(b'FILEBYTES', 'application/octet-stream', 'attachment; filename="file.bin"')
        if path == '/doc.pdf':
            return self._send(PDF, 'application/pdf', 'inline; filename="doc.pdf"')
        if path == '/slow.bin':
            self.send_response(200)
            self.send_header('Content-Type', 'application/octet-stream')
            self.send_header('Content-Disposition', 'attachment; filename="slow.bin"')
            self.send_header('Content-Length', '400000')
            self.end_headers()
            try:
                for _ in range(80):
                    time.sleep(0.25)
                    self.wfile.write(b'x' * 5000)
                    self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError, OSError):
                return
            return
        html = PAGES.get(path)
        if html is None:
            self.send_error(404)
            return
        body = f'<!doctype html><meta charset="utf-8">{html}'.encode()
        self._send(body, 'text/html; charset=utf-8')

    def _send(self, body, content_type, disposition=None):
        self.send_response(200)
        self.send_header('Content-Type', content_type)
        self.send_header('Content-Length', str(len(body)))
        self.send_header('Cache-Control', 'no-store')
        if disposition:
            self.send_header('Content-Disposition', disposition)
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


def check_plist():
    info = plistlib.loads(PLIST.read_bytes())
    schemes = [s for item in info.get('CFBundleURLTypes', []) for s in item.get('CFBundleURLSchemes', [])]
    assert 'http' in schemes and 'https' in schemes, schemes
    types, exts = [], []
    for item in info.get('CFBundleDocumentTypes', []):
        types += item.get('LSItemContentTypes', [])
        exts += item.get('CFBundleTypeExtensions', [])
    assert 'public.html' in types and 'public.xhtml' in types, types
    assert 'html' in exts and 'htm' in exts, exts
    print('plist: http/https and HTML document types', flush=True)


def main():
    check_plist()
    if DATA.exists():
        shutil.rmtree(DATA)
    if OUT.exists():
        shutil.rmtree(OUT)
    OUT.mkdir()
    FILE_PAGE.write_text('<!doctype html><title>From file</title><p>FILE-MARKER</p>')
    class Server(ThreadingHTTPServer):
        def handle_error(self, _request, _client_address):
            return

    httpd = Server(('127.0.0.1', PAGE_PORT), Handler)
    httpd.daemon_threads = True
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    print(f'pages: {BASE}', flush=True)

    client = aside_api.Client(API, data_dir=DATA)
    log_path = OUT / 'launch.log'
    watch_log = OUT / 'watchdog.log'
    command = [str(APP), '--data-dir', str(DATA), 'run', '--port', str(PORT), '--opencode-url', 'http://127.0.0.1:1']
    log = log_path.open('w')
    process = subprocess.Popen(command, stdout=log, stderr=log, env=aside_api.instance_env())
    watch = subprocess.Popen([str(WATCHDOG), str(process.pid)], stdout=watch_log.open('w'), stderr=subprocess.STDOUT)
    print(f'browser pid {process.pid}', flush=True)

    def alive():
        if process.poll() is not None:
            note = watch_log.read_text() if watch_log.exists() else ''
            tail = log_path.read_text()[-2000:]
            raise AssertionError(f'browser exited ({process.returncode})\n{note}\n{tail}')

    def until(desc, pred, seconds=15):
        last = None
        deadline = time.time() + seconds
        while time.time() < deadline:
            alive()
            try:
                last = pred()
            except Exception as exc:
                last = exc
            if last and not isinstance(last, Exception):
                return last
            time.sleep(0.15)
        raise AssertionError(f'{desc}\nlast={last!r}')

    def action(cmd, timeout=40, **kw):
        alive()
        return client.request('/api/browser/action', {'cmd': cmd, **kw}, timeout=timeout)

    def ok(cmd, timeout=40, **kw):
        result = action(cmd, timeout=timeout, **kw)
        assert 'error' not in result, result
        return result.get('result')

    def press(key, **mods):
        result = ok('key', key=key, **mods)
        assert result['command']
        return result

    def tabs():
        data = client.api('/api/browser/tabs')
        return data['tabs'], data.get('active')

    def activate(tab_id):
        client.api('/api/browser/command', {'cmd': 'activate', 'id': tab_id})

    def open_page(path):
        before = {t['id'] for t in tabs()[0]}
        # open_tab waits for the page to settle, which is slower than one API call.
        client.tool('open_tab', {'url': BASE + path}, timeout=90)
        created = until(path, lambda: next((t for t in tabs()[0] if t['id'] not in before), None))
        activate(created['id'])
        until(f'active {path}', lambda: tabs()[1] == created['id'])
        print(f'opened {path}', flush=True)
        return created['id']

    def evaluate(code):
        result = client.tool('evaluate', {'code': code}, timeout=60)
        text = '\n'.join(item.get('text', '') for item in result)
        return json.loads(text)

    def shell():
        data = ok('ui').get('shell')
        return data if isinstance(data, dict) else {}

    def href():
        return evaluate('return location.href')

    try:
        for _ in range(180):
            try:
                memory = client.api('/api/browser/memory')
                if memory.get('shell_ready'):
                    break
            except (urllib.error.URLError, TimeoutError, aside_api.MissingToken):
                pass
            alive()
            time.sleep(0.5)
        else:
            raise AssertionError('browser did not become ready')
        assert memory['engine'] == 'Chromium / CEF', memory
        print('ready', flush=True)

        # Shortcuts: mapping, then the real commands.
        mapping = [
            ('t', {'meta': True}, 'new-tab', True),
            ('w', {'meta': True}, 'close-tab', True),
            ('t', {'meta': True, 'shift': True}, 'reopen-tab', True),
            ('l', {'meta': True}, 'open-location', True),
            ('r', {'meta': True}, 'reload', False),
            ('[', {'meta': True}, 'back', False),
            (']', {'meta': True}, 'forward', False),
            ('1', {'meta': True}, 'tab-1', True),
            ('8', {'meta': True}, 'tab-8', True),
            ('9', {'meta': True}, 'tab-last', True),
            ('i', {'meta': True, 'alt': True}, 'devtools', True),
            ('Tab', {'ctrl': True}, 'next-tab', True),
            ('Tab', {'ctrl': True, 'shift': True}, 'prev-tab', True),
            ('=', {'meta': True}, 'zoom-in', False),
            ('-', {'meta': True}, 'zoom-out', False),
            ('0', {'meta': True}, 'zoom-reset', False),
            ('f', {'meta': True}, 'find', False),
            ('g', {'meta': True}, 'find-next', False),
            ('g', {'meta': True, 'shift': True}, 'find-prev', False),
            ('p', {'meta': True}, 'print', False),
            ('s', {'meta': True}, 'save-page', False),
            ('Escape', {}, 'find-close', False),
        ]
        for key, mods, command_name, reserved in mapping:
            got = action('key', key=key, dry=True, **mods)
            assert got.get('result', {}).get('command') == command_name, (key, mods, got)
            assert got['result']['reserved'] is reserved, (key, got)
        print('shortcut map: passed', flush=True)

        one = open_page('/page/one')
        two = open_page('/page/two')
        three = open_page('/page/three')
        web = [t for t in tabs()[0] if t['kind'] == 'web']
        assert [t['id'] for t in web][:3] == [one, two, three], web
        press('1', meta=True)
        until('tab 1', lambda: tabs()[1] == one)
        press('2', meta=True)
        until('tab 2', lambda: tabs()[1] == two)
        press('9', meta=True)
        until('last tab', lambda: tabs()[1] == three)
        press('Tab', ctrl=True)
        until('next tab', lambda: tabs()[1] == one)
        press('Tab', ctrl=True, shift=True)
        until('prev tab', lambda: tabs()[1] == three)
        before_tabs = len(tabs()[0])
        press('t', meta=True)
        until('new tab', lambda: len(tabs()[0]) == before_tabs + 1 and tabs()[1] not in (one, two, three))
        blank = tabs()[1]
        press('w', meta=True)
        until('blank closed', lambda: all(t['id'] != blank for t in tabs()[0]))
        activate(three)
        until('three active', lambda: tabs()[1] == three)
        press('w', meta=True)
        until('three closed', lambda: all(t['id'] != three for t in tabs()[0]))
        reopened = press('t', meta=True, shift=True)
        until('reopened', lambda: any(t['url'].endswith('/page/three') for t in tabs()[0]))
        assert reopened['command'] == 'reopen-tab'
        activate(two)
        press('l', meta=True)
        until('location bar', lambda: shell().get('overlay') == 'cmdk')
        press('Escape')
        until('location closed', lambda: not shell().get('overlay'))
        nav = open_page('/page/two')
        evaluate("window.__reload_marker = 'live'; return 1")
        press('r', meta=True)
        until('reloaded', lambda: evaluate('return window.__reload_marker || null') is None)
        client.tool('navigate', {'url': BASE + '/page/hist'}, timeout=90)
        until('hist', lambda: href().endswith('/page/hist') and tabs()[1] == nav)
        press('[', meta=True)
        until('back', lambda: href().endswith('/page/two'))
        press(']', meta=True)
        until('forward', lambda: href().endswith('/page/hist'))
        dev = press('i', meta=True, alt=True)
        assert dev['command'] == 'devtools'
        client.api('/api/browser/memory')
        print('shortcuts: passed', flush=True)

        # Find in page.
        find_id = open_page('/page/find')
        press('f', meta=True)
        until('find bar', lambda: shell().get('findOpen') is True and ok('findStatus').get('bar') is True)
        ok('find', text='alpha', viaShell=True)
        until('matches', lambda: (s := ok('findStatus')).get('done') and s.get('count') == 3 and s.get('active') == 1)
        until('count label', lambda: shell().get('findCount') == '1 of 3')
        press('g', meta=True)
        until('next match', lambda: ok('findStatus').get('active') == 2 and shell().get('findCount') == '2 of 3')
        press('g', meta=True, shift=True)
        until('prev match', lambda: ok('findStatus').get('active') == 1)
        ok('find', text='zzzz-not-here', viaShell=True)
        until('no matches', lambda: shell().get('findCount') == 'No results')
        press('Escape')
        until('find closed', lambda: shell().get('findOpen') is False and not ok('findStatus').get('bar'))
        print('find: passed', flush=True)

        # Zoom, including the indicator and per-host memory.
        activate(find_id)
        press('=', meta=True)
        zoomed = until('zoom in', lambda: (z := ok('zoomStatus', tab=find_id))['percent'] == 110 and z)
        assert zoomed['host'] == '127.0.0.1', zoomed
        until('zoom indicator', lambda: shell().get('zoom') == '110%' and shell().get('zoomOn') is True and 'Zoom 110%' in (shell().get('toast') or ''))
        press('-', meta=True)
        until('zoom out', lambda: ok('zoomStatus', tab=find_id)['percent'] == 100)
        ok('zoom', tab=find_id, how='set', percent=150)
        until('zoom 150', lambda: shell().get('zoom') == '150%' and shell().get('zoomOn') is True)
        stored = json.loads((DATA / 'zoom-levels.json').read_text())
        assert stored.get('127.0.0.1') == 150, stored
        press('0', meta=True)
        until('zoom reset', lambda: ok('zoomStatus', tab=find_id)['percent'] == 100 and shell().get('zoomOn') is False)
        assert '127.0.0.1' not in json.loads((DATA / 'zoom-levels.json').read_text())
        ok('zoom', tab=find_id, how='set', percent=125)
        until('saved 125', lambda: json.loads((DATA / 'zoom-levels.json').read_text()).get('127.0.0.1') == 125)
        press('0', meta=True)
        until('zoom back to 100', lambda: ok('zoomStatus', tab=find_id)['percent'] == 100)
        print('zoom: passed', flush=True)

        # Print is refused in the background. Save and PDF go to explicit paths.
        save_id = open_page('/page/save')
        refused = action('print', tab=save_id)
        assert 'error' in refused and 'dialog' in refused['error'].lower(), refused
        press('p', meta=True)
        html_path = OUT / 'page.html'
        mhtml_path = OUT / 'page.mhtml'
        pdf_path = OUT / 'page.pdf'
        saved = ok('savePage', tab=save_id, path=str(html_path), timeout=60)
        assert Path(saved['path']).read_text().find('SAVE-MARKER') >= 0
        archived = ok('savePage', tab=save_id, path=str(mhtml_path), timeout=60)
        archive = Path(archived['path']).read_bytes()
        assert b'SAVE-MARKER' in archive or b'Snapshot-Content-Location' in archive, archive[:200]
        printed = ok('printToPdf', tab=save_id, path=str(pdf_path), timeout=60)
        pdf = Path(printed['path']).read_bytes()
        assert pdf.startswith(b'%PDF') and len(pdf) > 800, pdf[:20]
        downloads_dir = DATA / 'downloads'
        before_names = {p.name for p in downloads_dir.glob('*')} if downloads_dir.exists() else set()
        press('s', meta=True)
        quick = until('save page shortcut', lambda: next((p for p in downloads_dir.glob('*.mhtml') if p.name not in before_names and p.stat().st_size > 0), None), 20)
        assert b'SAVE-MARKER' in quick.read_bytes() or b'multipart/related' in quick.read_bytes().lower()
        print('print and save: passed', flush=True)

        # Chromium's PDF viewer, not a download.
        pdf_id = open_page('/doc.pdf')
        probe = evaluate('''return {
          href: location.href,
          type: document.contentType,
          embed: [...document.querySelectorAll('embed,object,iframe')].map(n => n.type || n.src || n.localName),
          text: (document.body && document.body.innerText || '').slice(0, 400),
          html: document.documentElement.outerHTML.slice(0, 1200)
        }''')
        viewed = (
            any('pdf' in (item or '').lower() for item in probe['embed'])
            or 'pdf' in probe['type']
            or 'pdf' in probe['html'].lower()
            or 'Hello PDF' in probe['text']
        )
        downloads_now = ok('downloads')
        stole = [d for d in downloads_now if str(d.get('url', '')).endswith('/doc.pdf')]
        assert viewed and not stole, {'probe': probe, 'downloads': stole}
        print('pdf viewer: passed', probe['type'], probe['embed'], flush=True)

        # Context menu. Selection Copy and Search are present; Copy is not invoked
        # because Chromium would write the user's clipboard. Inspect must not open a window.
        menu_id = open_page('/page/menu')

        def center(selector):
            def measure():
                box = evaluate(
                    "const r = document.querySelector(" + json.dumps(selector) + ").getBoundingClientRect();"
                    "return {x: r.x + r.width / 2, y: r.y + r.height / 2, w: r.width, h: r.height};"
                )
                return box if box['w'] >= 2 and box['h'] >= 2 else None
            return until(selector, measure)

        def open_menu(selector, tab=menu_id):
            box = center(selector)
            action('contextMenu', tab=tab, x=box['x'], y=box['y'])
            def ready():
                menu = action('contextMenuStatus').get('result')
                return menu if isinstance(menu, dict) and menu.get('items') else None
            return until(f'menu {selector}', ready, 8)

        def labels(menu):
            return [item.get('label') for item in menu['items'] if item.get('label')]

        def run(menu, label):
            entry = next(item for item in menu['items'] if item.get('label') == label)
            assert entry.get('enabled', True), entry
            return action('contextCommand', id=entry['id'])

        link_menu = open_menu('#link')
        assert {'Open Link in New Tab', 'Copy Link Address', 'Save Link As…'} <= set(labels(link_menu)), labels(link_menu)
        opened = run(link_menu, 'Open Link in New Tab')
        assert 'error' not in opened, opened
        until('link tab', lambda: any(t['url'].endswith('/page/external') for t in tabs()[0]))
        run(link_menu, 'Copy Link Address')
        copied = until('copied link', lambda: (p := ok('pasteboard')).get('text') == BASE + '/page/external' and p)
        assert copied['text'].endswith('/page/external')
        save_menu = open_menu('#save-link')
        run(save_menu, 'Save Link As…')
        saved_link = until('saved link', lambda: next((d for d in ok('downloads') if d.get('name') == 'file.bin' and d.get('state') == 'complete'), None), 20)
        assert Path(saved_link['path']).read_bytes() == b'FILEBYTES'

        image_menu = open_menu('#pic')
        assert {'Open Image in New Tab', 'Save Image As…', 'Copy Image', 'Copy Image Address'} <= set(labels(image_menu)), labels(image_menu)
        run(image_menu, 'Copy Image')
        until('copied image', lambda: (ok('pasteboard').get('png_header') or [])[:8] == [137, 80, 78, 71, 13, 10, 26, 10])
        run(image_menu, 'Save Image As…')
        saved_image = until('saved image', lambda: next((d for d in ok('downloads') if str(d.get('name', '')).endswith('.png') and d.get('state') == 'complete'), None), 20)
        assert Path(saved_image['path']).read_bytes().startswith(b'\x89PNG')
        run(image_menu, 'Open Image in New Tab')
        until('image tab', lambda: any(t['url'].endswith('/pixel.png') for t in tabs()[0]))

        evaluate("const n = document.querySelector('#words'); const s = getSelection(); s.removeAllRanges(); const r = document.createRange(); r.selectNodeContents(n); s.addRange(r); return s.toString()")
        word_menu = open_menu('#words')
        assert 'Copy' in labels(word_menu) and any(label.startswith('Search Google for') for label in labels(word_menu)), labels(word_menu)

        hist_id = open_page('/page/two')
        client.tool('navigate', {'url': BASE + '/page/hist'}, timeout=90)
        until('hist again', lambda: href().endswith('/page/hist') and tabs()[1] == hist_id)
        page_menu = open_menu('#blank', hist_id)
        assert {'Back', 'Forward', 'Reload', 'Inspect'} <= set(labels(page_menu)), labels(page_menu)
        run(page_menu, 'Back')
        until('menu back', lambda: href().endswith('/page/two'))
        page_menu = open_menu('#blank', hist_id)
        forward = next(item for item in page_menu['items'] if item.get('label') == 'Forward')
        assert forward.get('enabled') is True, page_menu
        run(page_menu, 'Forward')
        until('menu forward', lambda: href().endswith('/page/hist'))
        evaluate("window.__menu_reload = 1; return 1")
        page_menu = open_menu('#blank', hist_id)
        run(page_menu, 'Reload')
        until('menu reload', lambda: evaluate('return window.__menu_reload || null') is None)
        page_menu = open_menu('#blank', hist_id)
        inspected = run(page_menu, 'Inspect')
        assert 'error' in inspected and 'window' in inspected['error'].lower(), inspected
        client.api('/api/browser/memory')
        print('context menu: passed', flush=True)

        # Downloads: progress row, cancel, and reveal without Finder.
        activate(menu_id)
        ok('download', tab=menu_id, url=BASE + '/slow.bin')
        slow = until('slow download', lambda: next((d for d in ok('downloads') if d.get('name') == 'slow.bin' and d.get('state') == 'in_progress'), None))
        until('download row', lambda: any(row.get('name') == 'slow.bin' and row.get('cancel') for row in shell().get('downloads') or []))
        ok('downloadCancel', id=slow['id'])
        until('canceled', lambda: next((d for d in ok('downloads') if d.get('id') == slow['id'] and d.get('state') == 'canceled'), None), 12)
        until('canceled row', lambda: any(row.get('name') == 'slow.bin' and row.get('status') == 'Canceled' for row in shell().get('downloads') or []))
        revealed = ok('downloadReveal', id=saved_link['id'])
        assert revealed['revealed'] is False, revealed
        assert Path(revealed['path']).is_file()
        assert str(revealed['path']).startswith(str(DATA)), revealed
        children = subprocess.run(['pgrep', '-P', str(process.pid), '-l'], capture_output=True, text=True).stdout
        assert 'open' not in children, children
        print('downloads: passed', flush=True)

        # Default browser: report status, refuse the system prompt, keep the handler.
        before = ok('defaultBrowser')
        assert before.get('delegate') is True, before
        assert before.get('bundle_id'), before
        info = plistlib.loads(PLIST.read_bytes())
        assert before['bundle_id'] == info.get('CFBundleIdentifier'), before
        refused = action('makeDefaultBrowser')
        assert 'error' in refused and 'dialog' in refused['error'].lower(), refused
        after = ok('defaultBrowser')
        assert after['http'] == before['http'] and after['https'] == before['https'], (before, after)
        assert after['is_default'] == before['is_default']
        print('default browser: status only, handler unchanged', flush=True)

        # URLs from a second launch and from the open-URLs delegate.
        external_before = sum(1 for t in tabs()[0] if t['url'].endswith('/page/external'))
        forward = subprocess.Popen(
            command + [BASE + '/page/external', str(FILE_PAGE)],
            stdout=log, stderr=log, env=aside_api.instance_env(),
        )
        forward_watch = subprocess.Popen([str(WATCHDOG), str(forward.pid)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            code = forward.wait(timeout=25)
        except subprocess.TimeoutExpired:
            forward.kill()
            raise AssertionError('second launch did not hand off to the running instance')
        finally:
            if forward_watch.poll() is None:
                forward_watch.kill()
        assert code == 0, code
        until('external url', lambda: sum(1 for t in tabs()[0] if t['url'].endswith('/page/external')) == external_before + 1)
        until('file url', lambda: any(t['url'].startswith('file:') and t['url'].endswith('aside-commands-file.html') for t in tabs()[0]))
        ok('simulateOpenUrls', urls=[BASE + '/page/delegate'])
        until('delegate url', lambda: any(t['url'].endswith('/page/delegate') for t in tabs()[0]))
        print('single instance and open URLs: passed', flush=True)

        # Zoom survives a restart of this data directory.
        activate(find_id)
        ok('zoom', tab=find_id, how='set', percent=150)
        until('zoom stored', lambda: json.loads((DATA / 'zoom-levels.json').read_text()).get('127.0.0.1') == 150)
        try:
            client.api('/api/browser/command', {'cmd': 'quit'})
        except (urllib.error.URLError, TimeoutError):
            pass
        try:
            process.wait(timeout=20)
        except subprocess.TimeoutExpired:
            process.terminate()
            process.wait(timeout=10)
        if watch.poll() is None:
            watch.terminate()
        assert '127.0.0.1' in json.loads((DATA / 'zoom-levels.json').read_text())
        process = subprocess.Popen(command, stdout=log, stderr=log, env=aside_api.instance_env())
        watch = subprocess.Popen([str(WATCHDOG), str(process.pid)], stdout=watch_log.open('a'), stderr=subprocess.STDOUT)
        for _ in range(180):
            try:
                if client.api('/api/browser/memory').get('shell_ready'):
                    break
            except (urllib.error.URLError, TimeoutError, aside_api.MissingToken):
                pass
            alive()
            time.sleep(0.5)
        else:
            raise AssertionError('browser did not restart')
        again = open_page('/page/find')
        restored = until('restored zoom', lambda: (z := ok('zoomStatus', tab=again))['percent'] == 150 and z)
        assert restored['saved'] == 150, restored
        until('restored indicator', lambda: shell().get('zoom') == '150%' and shell().get('zoomOn') is True)
        print('zoom persistence: passed', flush=True)
        print('PASS', flush=True)
    finally:
        httpd.shutdown()
        for proc in (locals().get('forward'), process, watch, locals().get('forward_watch')):
            if proc is not None and proc.poll() is None:
                proc.terminate()
                try:
                    proc.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    proc.kill()
        log.close()


if __name__ == '__main__':
    main()
