#!/usr/bin/env python3
"""Exercise chat startup, scrolling previews and tab switches in a background app."""
import argparse
import subprocess
import tempfile
import time
import urllib.error
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from threading import Thread

import atrium_api

ROOT = Path(__file__).resolve().parent.parent
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--app', type=Path, default=ROOT / 'target/bundle/Atrium.app/Contents/MacOS/atrium')
parser.add_argument('--port', type=int, default=7837)
args = parser.parse_args()
client = None
startup_errors = []


class Fixture(BaseHTTPRequestHandler):
    def reply(self, body, content_type='application/json'):
        data = body.encode()
        self.send_response(200)
        self.send_header('Content-Type', content_type)
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path.startswith('/page'):
            self.reply('''<!doctype html><title>Agent preview fixture</title>
              <style>body { margin:0 } section { height:2000px; background:coral }
              section + section { background:royalblue }</style>
              <section>Top of the page</section><section>Scrolled down</section>''', 'text/html')
        elif self.path == '/session/status':
            self.reply('{}')
        else:
            self.reply('[]')

    def do_POST(self):
        self.rfile.read(int(self.headers.get('Content-Length', 0)))
        if self.path == '/session':
            self.reply('{"id":"preview-session"}')
        elif self.path.endswith('/prompt_async'):
            # A tool can finish before prompt_async returns the new session ID to
            # the UI. Keep this ordering deterministic without a model/provider.
            try:
                client.tool('open_tab', {'url': f'http://127.0.0.1:{self.server.server_port}/page'})
            except Exception as error:
                startup_errors.append(str(error))
            self.reply('{}')
        else:
            self.reply('{}')

    def log_message(self, *_):
        pass


def wait(fn, label, timeout=15):
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
    raise AssertionError(f'Timed out: {label}: {last}; tool errors: {startup_errors}')


def check(condition, label):
    assert condition, label
    print('PASS:', label, flush=True)


with tempfile.TemporaryDirectory(prefix='atrium-preview-') as directory:
    profile = Path(directory)
    fixture = ThreadingHTTPServer(('127.0.0.1', 0), Fixture)
    Thread(target=fixture.serve_forever, daemon=True).start()
    log = (profile / 'app.log').open('w')
    app = subprocess.Popen([str(args.app.resolve()), '--data-dir', directory, 'run', '--port', str(args.port),
                            '--opencode-url', f'http://127.0.0.1:{fixture.server_port}'], stdout=log, stderr=log,
                           env={**atrium_api.instance_env(), 'ATRIUM_TEST_SHELL_EVAL': '1', 'ATRIUM_DEBUG_API': '1'})
    client = atrium_api.Client(f'http://127.0.0.1:{args.port}', data_dir=profile)

    def shell(js):
        result = client.request('/api/debug/shell', js, raw=True)
        assert not (isinstance(result, dict) and 'error' in result), result
        return result

    def preview():
        return shell('''const i = document.querySelector('#pip-img'), p = document.querySelector('#pip');
          const r = p.getBoundingClientRect(), pane = document.querySelector('#pane-main').getBoundingClientRect();
          const c = document.createElement('canvas'); c.width = c.height = 1;
          const x = c.getContext('2d'); if (i.complete && i.naturalWidth) x.drawImage(i, 0, 0, 1, 1);
          return { on:p.classList.contains('on'), width:i.naturalWidth, src:i.src, complete:i.complete,
            tab:__shell.S.agentTabs.get(__shell.chat.id), pixel:[...x.getImageData(0,0,1,1).data],
            topRight:r.width > 0 && r.right <= pane.right && r.top >= pane.top && r.top < pane.top + 100 };''')

    try:
        wait(lambda: client.api('/api/browser/memory').get('shell_ready'), 'shell ready', 60)
        shell('''window.atriumOnboarding?.close(); __shell.newChat(true);
          void __shell.chat.send('Open the fixture and scroll it'); return true;''')
        wait(lambda: shell("return __shell.chat.id === 'preview-session';"), 'chat created')
        first = wait(lambda: p if (p := preview())['on'] and p['complete'] and p['width'] > 50
                     and p['pixel'][0] > p['pixel'][2] else None,
                     'new chat preview after a tab opens before the chat response')
        check(first['topRight'], 'new chat displays its tab preview in the top right')

        # Frequent shell renders must not restart the periodic capture timer.
        shell('window.previewRenders = setInterval(__shell.render, 100); return true;')
        client.tool('scroll', {'direction': 'down', 'amount': 2400})
        changed = wait(lambda: p if (p := preview())['complete'] and p['width'] > 50
                       and p['pixel'][2] > p['pixel'][0] else None,
                       'preview updates after scrolling during frequent renders')
        check(changed['tab'] == first['tab'], 'preview follows scrolling without switching tabs')
        shell('clearInterval(window.previewRenders); return true;')

        # A tab reported for another session belongs to that chat, even when it
        # is not the one currently displayed.
        shell(f"atriumShell.agent({first['tab']}, false, 'other-session'); return true;")
        check(shell(f"return __shell.S.agentTabs.get('other-session') === {first['tab']};"),
              'tab notifications retain their owning session')
        shell("__shell.chat.setBusy(false); return true;")
        time.sleep(.5)
        before = preview()['src']
        shell(f"atriumShell.frozen('pip', '/frozen/pip?stale', {first['tab'] + 1000}); return true;")
        time.sleep(.2)
        check(preview()['src'] == before, 'late captures from another tab are ignored')
        shell("__shell.newChat(true); return true;")
        check(not preview()['on'], 'new empty chat clears the previous preview')
        shell("__shell.chat.id = 'preview-session'; __shell.render(); return true;")
        wait(lambda: preview()['on'], 'returning to the original chat restores its preview')
        shell("document.querySelector('#pip').click(); return true;")
        wait(lambda: client.api('/api/browser/tabs')['active'] == first['tab']
             and shell("return __shell.S.view === 'tab' && __shell.S.side;"), 'opening the preview')
        check(not preview()['on'], 'opening the preview docks the chat beside the live tab')
        check(not startup_errors, 'fixture tool completed successfully')
    except Exception:
        log.flush()
        print((profile / 'app.log').read_text()[-4000:], flush=True)
        raise
    finally:
        try:
            client.command(cmd='quit')
            app.wait(timeout=15)
        except Exception:
            app.terminate()
            app.wait(timeout=15)
        fixture.shutdown()
        log.close()
