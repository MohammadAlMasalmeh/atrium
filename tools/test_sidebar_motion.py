#!/usr/bin/env python3
"""Check sidebar layout stability and interrupted hover transitions in a background app."""
import argparse
import json
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
parser.add_argument('--port', type=int, default=7836)
args = parser.parse_args()


class Fixture(BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'''<!doctype html><title>Sidebar motion fixture</title>
        <style>body { margin: 0; font: 18px system-ui; }
        main { width: 70%; margin: 50px auto; } .grid { display: grid;
        grid-template-columns: repeat(auto-fit, minmax(180px, 1fr)); gap: 20px; }
        article { padding: 30px; background: #cde; border-radius: 12px; }</style>
        <main><h1>Responsive page stays in place during hover</h1><div class=grid>
        <article>One</article><article>Two</article><article>Three</article></div></main>'''
        self.send_response(200)
        self.send_header('Content-Type', 'text/html')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

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
        time.sleep(.025)
    raise AssertionError(f'Timed out: {label}: {last}')


def check(condition, label, detail=None):
    assert condition, (label, detail)
    print('PASS:', label, flush=True)


with tempfile.TemporaryDirectory(prefix='atrium-sidebar-') as directory:
    profile = Path(directory)
    server = ThreadingHTTPServer(('127.0.0.1', 0), Fixture)
    Thread(target=server.serve_forever, daemon=True).start()
    log = (profile / 'app.log').open('w')
    app = subprocess.Popen([str(args.app.resolve()), '--data-dir', directory, 'run', '--port', str(args.port),
                            '--opencode-url', 'http://127.0.0.1:1'], stdout=log, stderr=log,
                           env={**atrium_api.instance_env(), 'ATRIUM_TEST_SHELL_EVAL': '1', 'ATRIUM_DEBUG_API': '1'})
    client = atrium_api.Client(f'http://127.0.0.1:{args.port}', data_dir=profile)

    def shell(js):
        result = client.request('/api/debug/shell', js, raw=True)
        assert not (isinstance(result, dict) and 'error' in result), result
        return result

    def state():
        return shell('return { ...window.__shell.S, tabs: [], tabSpace: null, lastTab: null };')

    def event(selector, name):
        return shell(f'document.querySelector({json.dumps(selector)}).dispatchEvent(new MouseEvent({json.dumps(name)})); return true;')

    def cdp(tab, method, params):
        return client.api('/api/debug/cdp', {'tab': tab, 'method': method, 'params': params})['result']

    def page(tab, expression):
        result = cdp(tab, 'Runtime.evaluate', {'expression': expression, 'returnByValue': True})
        assert 'exceptionDetails' not in result, result
        return result['result'].get('value')

    def closed():
        s = state()
        return not s['peek'] and not s['peekFrozen'] and not s['peekCover']

    try:
        wait(lambda: client.api('/api/browser/memory').get('shell_ready'), 'shell ready', 60)
        shell('window.atriumOnboarding?.close(); return true;')
        client.command(cmd='newTab', url=f'http://127.0.0.1:{server.server_port}/page')
        wait(lambda: any(t['kind'] == 'web' and not t['loading'] for t in client.api('/api/browser/tabs')['tabs']), 'fixture loaded')
        active = client.api('/api/browser/tabs')['active']

        def click_sidebar():
            started = time.monotonic()
            result = shell('''const start = performance.now();
              document.querySelector(document.body.classList.contains('sb-hidden') ? '#tb-sidebar' : '#sb-toggle').click();
              const b = document.querySelector('#pane-main').getBoundingClientRect();
              const style = getComputedStyle(document.querySelector('#sidebar'));
              const ready = document.body.classList.contains('sb-hidden') || (style.opacity === '1' && style.visibility === 'visible');
              return {handlerMs: performance.now() - start, ready, rect: {x:Math.round(b.x), y:Math.round(b.y), w:Math.round(b.width), h:Math.round(b.height)}};''')
            wait(lambda: client.api('/api/browser/memory')['layout']['main'] == result['rect'], 'click layout', 3)
            return {'handlerMs': round(result['handlerMs'], 1), 'appliedMs': round((time.monotonic() - started) * 1000, 1), 'ready': result['ready']}

        timings = [click_sidebar() for _ in range(6)]
        page(active, 'setTimeout(() => { const end = performance.now() + 1500; while (performance.now() < end) {} }, 0); true')
        time.sleep(.05)
        timings += [click_sidebar(), click_sidebar()]
        print('Sidebar button timing (including a busy page):', json.dumps(timings), flush=True)
        check(all(t['ready'] and t['handlerMs'] < 50 and t['appliedMs'] < 250 for t in timings),
              'sidebar button reveals the list in the click event, including while the page is busy', timings)
        page(active, 'true')  # Wait for the deliberate busy task before the geometry tests.
        shell('''window.motion = { samples: [], timer: null };
          motion.rects = () => ['#pane-main', '#pane-split', '#topbar'].map(s => {
            const b = document.querySelector(s).getBoundingClientRect(); return [b.x,b.y,b.width,b.height]; });
          motion.start = () => { motion.samples = []; clearInterval(motion.timer);
            motion.timer = setInterval(() => motion.samples.push(motion.rects()), 10); };
          return true;''')
        shell("atriumShell.cmd('toggleSidebar'); motion.start(); return true;")
        final = shell('return motion.rects();')
        time.sleep(.65)
        samples = shell('clearInterval(motion.timer); return motion.samples;')
        check(len(samples) > 5 and all(s == final for s in samples), 'docked close settles once without a moving page', samples[:3])
        check(not client.api('/api/browser/memory')['layout']['covered'], 'docked close never parks the live page')

        shell("atriumShell.cmd('toggleSidebar'); motion.start(); return true;")
        final = shell('return motion.rects();')
        time.sleep(.4)
        samples = shell('clearInterval(motion.timer); return motion.samples;')
        check(len(samples) > 5 and all(s == final for s in samples), 'docked open keeps page geometry at its final size')
        shell("for(let i=0;i<4;i++) atriumShell.cmd('toggleSidebar'); return true;")
        check(shell("return !document.body.classList.contains('sb-hidden');"), 'rapid toggles are not dropped')
        shell("atriumShell.cmd('toggleSidebar'); return true;")

        # Exercise unequal split widths: parking must not resize the second renderer.
        client.command(cmd='newTab', url=f'http://127.0.0.1:{server.server_port}/split')
        tabs = client.api('/api/browser/tabs')
        web = [t['id'] for t in tabs['tabs'] if t['kind'] == 'web']
        client.command(cmd='split', id=web[0])
        wait(lambda: shell("return document.body.classList.contains('split');"), 'split view')
        shell("document.querySelector('#pane-split').style.flex = '0.7'; return true;")
        expected = shell("return ['#pane-split','#pane-main'].map(s => { const b = document.querySelector(s).getBoundingClientRect(); return [Math.round(b.width),Math.round(b.height)]; });")
        wait(lambda: all(page(tab, '[innerWidth,innerHeight]') == size for tab, size in zip(web, expected)), 'native split sizes settled')
        for tab in web:
            page(tab, '''window.motionViewport = [innerWidth, innerHeight]; window.motionResizes = [];
              addEventListener('resize', () => {
                if (innerWidth !== motionViewport[0] || innerHeight !== motionViewport[1]) motionResizes.push([innerWidth,innerHeight]);
              }); true''')
        baseline = shell('motion.start(); return motion.rects();')
        shell('''window.initialOpacity = null;
          const observer = new MutationObserver(() => {
            if (__shell.S.peekFrozen && !__shell.S.peek) {
              initialOpacity = +getComputedStyle(document.querySelector('#sidebar')).opacity;
              observer.disconnect();
            }
          }); observer.observe(document.body, {attributes:true, attributeFilter:['class']}); return true;''')
        event('#peek-zone', 'mouseenter')
        wait(lambda: state()['peek'], 'hover open')
        check(shell('return initialOpacity;') == 0, 'hover prepares invisibly before starting its entrance')
        def panel_state():
            return shell("const b = document.querySelector('#sidebar').getBoundingClientRect(); return {top:b.top, bar:document.querySelector('#topbar').getBoundingClientRect().bottom, opacity:getComputedStyle(document.querySelector('#sidebar')).opacity};")
        # WebKit can suspend transition time in an invisible background window.
        # Drive the panel's transitions to their endpoint before checking geometry.
        transition = shell('''const panel = document.querySelector('#sidebar');
          const opacity = getComputedStyle(panel).opacity;
          const animations = panel.getAnimations().map(a => ({kind:a.constructor.name, time:a.currentTime, state:a.playState}));
          for (const a of panel.getAnimations()) a.finish();
          return {opacity, classes:document.body.className, animations};''')
        print('Background hover transition:', json.dumps(transition), flush=True)
        panel = wait(lambda: (p := panel_state())['opacity'] == '1' and p, 'hover entrance')
        check(panel['top'] >= panel['bar'] and float(panel['opacity']) == 1, 'hover panel sits below the toolbar and finishes opening', panel)
        event('#sidebar', 'mouseleave')
        wait(lambda: shell("return document.body.classList.contains('peek-out');"), 'close started')
        reversal = shell('''const panel = document.querySelector('#sidebar');
          for (const a of panel.getAnimations()) { a.pause(); a.currentTime = 60; }
          const before = +getComputedStyle(panel).opacity;
          panel.dispatchEvent(new MouseEvent('mouseenter'));
          const after = +getComputedStyle(panel).opacity;
          for (const a of panel.getAnimations()) a.play();
          return {before, after};''')
        check(0 < reversal['before'] < 1 and abs(reversal['before'] - reversal['after']) < .02,
              'interrupted close reverses from its current opacity without a jump', reversal)
        time.sleep(.6)
        check(state()['peek'] and state()['peekCover'], 're-entering during close keeps the panel open')

        # Re-enter after the fade, while the native-page handoff is still pending.
        shell('''window.reentered = false;
          const observer = new MutationObserver(() => {
            const s = __shell.S;
            if (!s.peek && s.peekFrozen && !s.peekCover) {
              observer.disconnect(); reentered = true;
              document.querySelector('#peek-zone').dispatchEvent(new MouseEvent('mouseenter'));
            }
          }); observer.observe(document.body, {attributes:true, attributeFilter:['class']}); return true;''')
        event('#sidebar', 'mouseleave')
        wait(lambda: shell('return reentered;'), 're-enter during native handoff')
        time.sleep(.4)
        check(state()['peek'] and state()['peekCover'], 're-entry cancels pending snapshot teardown')
        event('#sidebar', 'mouseleave')
        wait(closed, 'hover fully closed')
        samples = shell('clearInterval(motion.timer); return motion.samples;')
        check(len(samples) > 10 and all(s == baseline for s in samples), 'page, split pane and toolbar never move during hover')
        resizes = [page(tab, 'motionResizes') for tab in web]
        check(all(not sizes for sizes in resizes), 'neither live renderer resizes while parked for hover', resizes)
        check(not client.api('/api/browser/memory')['layout']['covered'], 'closing hover restores the live page')

        # A decoded snapshot arriving after the pointer leaves cannot reopen the panel.
        shell('''window.decodeRelease = null;
          const img = document.querySelector('#frozen-main'); window.originalDecode = img.decode;
          img.decode = () => new Promise(resolve => { window.decodeRelease = resolve; }); return true;''')
        event('#peek-zone', 'mouseenter')
        wait(lambda: shell('return !!decodeRelease;'), 'snapshot decoding')
        event('#peek-zone', 'mouseleave')
        shell("document.querySelector('#frozen-main').decode = originalDecode; decodeRelease(); return true;")
        time.sleep(.3)
        check(closed(), 'cancelled snapshot decoding cannot resurrect the hover panel')

        event('#peek-zone', 'mouseenter')
        wait(lambda: state()['peek'], 'hover before docking')
        shell("atriumShell.cmd('toggleSidebar'); return true;")
        time.sleep(.5)
        check(closed() and shell("return !document.body.classList.contains('sb-hidden');"), 'docking clears hover timers and snapshots')
        check(not client.api('/api/browser/memory')['layout']['covered'], 'docking leaves the live page visible')

        client.command(cmd='newTab')
        wait(lambda: shell("return document.querySelector('#ntp').style.display === 'block';"), 'start page')
        shell("atriumShell.cmd('toggleSidebar'); return true;")
        baseline = shell('return motion.rects();')
        event('#peek-zone', 'mouseenter')
        wait(lambda: state()['peek'], 'hover on start page')
        event('#sidebar', 'mouseleave')
        wait(closed, 'start-page hover closed')
        check(shell('return motion.rects();') == baseline, 'start-page hover works without a native snapshot or layout shift')
    except Exception:
        print((profile / 'app.log').read_text()[-5000:], flush=True)
        raise
    finally:
        try:
            client.command(cmd='quit')
            app.wait(timeout=15)
        except Exception:
            app.terminate()
            app.wait(timeout=5)
        log.close()
        server.shutdown()
