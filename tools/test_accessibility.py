#!/usr/bin/env python3
"""Accessibility check in an isolated background instance.

Starts its own fixture server, browser (ATRIUM_MOCK_KEYCHAIN, ATRIUM_BACKGROUND_WINDOW,
ATRIUM_DEBUG_API), and watchdog. Uses DevTools through /api/debug/cdp to check the
shell's and a page's accessibility trees, keyboard navigation, focus movement between
the shell and the page, reduced motion, and text contrast, and /api/debug/accessibility
to check that the native macOS tree is exposed once an assistive client asks for it.
It never starts VoiceOver and never uses the system Accessibility API.
"""
import argparse
import json
import subprocess
import sys
import tempfile
import time
import traceback
import urllib.error
from pathlib import Path

import atrium_api

ROOT = Path(__file__).resolve().parent.parent
parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
parser.add_argument('--app', type=Path, default=ROOT / 'target/bundle/atrium.app/Contents/MacOS/atrium')
parser.add_argument('--port', type=int, default=7820)
parser.add_argument('--fixture-port', type=int, default=7821)
parser.add_argument('--watchdog', type=Path, default=ROOT.parent / 'watchdog.sh')
parser.add_argument('--verbose', action='store_true')
args = parser.parse_args()

failures = []


def check(ok, what, detail=None):
    print(f"{'PASS' if ok else 'FAIL'}: {what}" + (f' ({detail})' if detail is not None and (args.verbose or not ok) else ''), flush=True)
    if not ok:
        failures.append(what)
    return ok


def wait(predicate, timeout=15, step=.25):
    end = time.time() + timeout
    last = None
    while time.time() < end:
        try:
            last = predicate()
            if last:
                return last
        except (urllib.error.URLError, TimeoutError, AssertionError, KeyError):
            pass
        time.sleep(step)
    return last


class Browser:
    def __init__(self, client):
        self.client = client

    def cdp(self, tab, method, params=None):
        return self.client.api('/api/debug/cdp', {'tab': tab, 'method': method, 'params': params or {}})['result']

    def js(self, expression, tab='shell'):
        result = self.cdp(tab, 'Runtime.evaluate', {'expression': expression, 'returnByValue': True, 'awaitPromise': True})
        assert 'exceptionDetails' not in result, result
        return result['result'].get('value')

    def ax(self, tab='shell'):
        return [n for n in self.cdp(tab, 'Accessibility.getFullAXTree')['nodes'] if not n.get('ignored')]

    def key(self, tab, key, code=None, vk=0, modifiers=0):
        base = {'key': key, 'code': code or key, 'windowsVirtualKeyCode': vk, 'nativeVirtualKeyCode': vk, 'modifiers': modifiers}
        self.cdp(tab, 'Input.dispatchKeyEvent', {'type': 'rawKeyDown', **base})
        self.cdp(tab, 'Input.dispatchKeyEvent', {'type': 'keyUp', **base})
        time.sleep(.15)

    def state(self, **body):
        return self.client.api('/api/debug/accessibility', body)

    def command(self, **body):
        self.client.command(**body)
        time.sleep(.2)

    def tabs(self):
        return self.client.api('/api/browser/tabs')

    def web_tabs(self):
        return [t for t in self.tabs()['tabs'] if t['kind'] == 'web']

    def active_id(self):
        return self.js("(a => !a ? '' : a.id || `${a.tagName.toLowerCase()}.${a.className}`)(document.activeElement)")


KEYS = {'down': ('ArrowDown', 40), 'up': ('ArrowUp', 38), 'enter': ('Enter', 13), 'delete': ('Delete', 46),
        'tab': ('Tab', 9), 'escape': ('Escape', 27), 'f6': ('F6', 117), 'end': ('End', 35)}


def press(b, tab, name, shift=False):
    key, vk = KEYS[name]
    b.key(tab, key, vk=vk, modifiers=8 if shift else 0)


def role(node):
    return node.get('role', {}).get('value')


def name(node):
    return node.get('name', {}).get('value', '')


def prop(node, key):
    for p in node.get('properties', []):
        if p['name'] == key:
            return p['value'].get('value')
    return None


def find(nodes, r, n=None):
    return [x for x in nodes if role(x) == r and (n is None or name(x) == n)]


def native_nodes(tree, out=None):
    out = [] if out is None else out
    if isinstance(tree, dict):
        out.append(tree)
        for child in tree.get('children', []):
            native_nodes(child, out)
    return out


CONTRAST_JS = r'''(() => {
  const probe = document.createElement('div'); document.body.append(probe);
  const parse = (v) => {
    probe.style.color = ''; probe.style.color = v;
    const c = getComputedStyle(probe).color, n = (c.match(/[\d.]+/g) || []).map(Number);
    if (c.startsWith('color(srgb')) return [n[0] * 255, n[1] * 255, n[2] * 255, n.length > 3 ? n[3] : 1];
    return n.length === 3 ? [...n, 1] : n;
  };
  const over = (fg, bg) => fg.slice(0, 3).map((c, i) => c * fg[3] + bg[i] * (1 - fg[3]));
  const lum = (c) => { const [r, g, b] = c.map((v) => { v /= 255; return v <= .03928 ? v / 12.92 : ((v + .055) / 1.055) ** 2.4; }); return .2126 * r + .7152 * g + .0722 * b; };
  const ratio = (a, b) => { const [x, y] = [lum(a), lum(b)].sort((p, q) => q - p); return (x + .05) / (y + .05); };
  const card = over(parse('var(--card)'), [255, 255, 255]);
  const surfaces = { card, menu: over(parse('var(--menu)'), card), field: over(parse('var(--field)'), card) };
  const out = [];
  for (const fg of ['--fg', '--fg-2', '--fg-3', '--accent-text', '--danger'])
    for (const [s, bg] of Object.entries(surfaces)) out.push({ fg, on: s, ratio: ratio(over(parse(`var(${fg})`), bg), bg) });
  const strong = over(parse('var(--accent-strong)'), card);
  for (const fg of ['#fff', 'rgb(255 255 255 / .9)']) out.push({ fg, on: '--accent-strong', ratio: ratio(over(parse(fg), strong), strong) });
  probe.remove();
  return { accent: getComputedStyle(document.documentElement).getPropertyValue('--accent').trim(), pairs: out };
})()'''


def run(b, fixture):
    client = b.client
    ready = wait(lambda: client.api('/api/browser/memory').get('shell_ready') and b.js('!!window.atriumShell'), 40)
    assert ready, 'browser did not become ready'

    before = b.state()
    if before.get('voiceover'):
        print('NOTE: VoiceOver is running; the "disabled before an assistive client" check is skipped')
    else:
        check(before['enabled'] is False, 'Chromium accessibility stays off until an assistive client asks', before)

    for n in ('one', 'two', 'three'):
        client.tool('open_tab', {'url': f'{fixture}/page/a11y-{n}'}, timeout=120)
    titles = [f'Fixture /page/a11y-{n}' for n in ('one', 'two', 'three')]
    wait(lambda: [t['title'] for t in b.web_tabs()] == titles and not any(t['loading'] for t in b.web_tabs()), 30)
    tabs = b.web_tabs()
    b.command(cmd='activate', id=tabs[0]['id'])
    wait(lambda: b.js(f"!!document.querySelector('#tab-{tabs[0]['id']}[aria-selected=true]')"))

    # Shell semantics.
    nodes = b.ax()
    check(bool(find(nodes, 'navigation', 'Sidebar')), 'sidebar is a navigation landmark named Sidebar')
    check(bool(find(nodes, 'main', 'Page')), 'page area is the main landmark')
    tablist = find(nodes, 'tablist', 'Tabs')
    tab_nodes = find(nodes, 'tab')
    check(len(tablist) == 1 and [name(t) for t in tab_nodes] == titles, 'tab list exposes each tab with its title', [name(t) for t in tab_nodes])
    selected = [name(t) for t in tab_nodes if prop(t, 'selected')]
    check(selected == [titles[0]], 'only the active tab is aria-selected', selected)
    for label in ('Back', 'Forward', 'Reload', 'Hide Sidebar', 'Command Bar', 'Assistant', 'New Space', 'New Chat', 'Copy Link', 'Pin to This Space'):
        check(bool(find(nodes, 'button', label)), f'button "{label}" is named')
    unnamed = [x.get('backendDOMNodeId') for x in nodes if role(x) in ('button', 'tab', 'link', 'textbox', 'combobox') and not name(x).strip()]
    check(not unnamed, 'every button, tab, link, and field has an accessible name', unnamed)
    check(len(find(nodes, 'status')) >= 2, 'toast and announcer are status live regions')
    check(any(name(x).startswith('Address: ') for x in find(nodes, 'button')), 'address control names the current URL')
    check(prop(find(nodes, 'button', 'Assistant')[0], 'pressed') == 'false', 'assistant toggle reports aria-pressed')
    b.command(cmd='shellCommand', name='toggleAssistant')
    nodes = b.ax()
    check(bool(find(nodes, 'complementary', 'Assistant')) and prop(find(nodes, 'button', 'Assistant')[0], 'pressed') == 'true',
          'open assistant panel is a complementary landmark and its toggle is pressed')
    check(bool(find(nodes, 'textbox', 'Message the assistant')) and bool(find(nodes, 'log', 'Conversation')), 'assistant composer and conversation log are named')
    b.command(cmd='shellCommand', name='toggleAssistant')

    # Keyboard navigation of the sidebar, through real key events into the shell.
    b.cdp('shell', 'Emulation.setFocusEmulationEnabled', {'enabled': True})
    b.command(cmd='shellCommand', name='focusSidebar')
    check(b.active_id() == f"tab-{tabs[0]['id']}", 'focusSidebar focuses the active tab row', b.active_id())
    press(b, 'shell', 'down')
    check(b.active_id() == f"tab-{tabs[1]['id']}", 'ArrowDown moves to the next tab', b.active_id())
    stops = b.js("[...document.querySelectorAll('[role=tab]')].filter(t => t.tabIndex === 0).map(t => t.id)")
    check(stops == [f"tab-{tabs[1]['id']}"], 'the tab list has a single roving tab stop on the focused row', stops)
    press(b, 'shell', 'up')
    press(b, 'shell', 'up')
    check(b.active_id() == f"tab-{tabs[2]['id']}", 'ArrowUp wraps from the first tab to the last', b.active_id())
    press(b, 'shell', 'enter')
    ok = wait(lambda: b.tabs()['active'] == tabs[2]['id'] and b.js(f"document.querySelector('#tab-{tabs[2]['id']}').getAttribute('aria-selected')") == 'true')
    check(bool(ok), 'Enter activates the focused tab and marks it selected')
    focus = wait(lambda: b.state()['focused_view'] == tabs[2]['id'])
    check(bool(focus), 'activating a tab moves native focus to its page view', b.state()['focused_view'])
    b.js("""window.__focusLog = []; const d = (n) => n ? n.id || n.tagName : null;
      document.addEventListener('focusout', (e) => __focusLog.push(['out', d(e.target), d(e.relatedTarget), new Error().stack.split('\\n').slice(2, 6).join(' | ')]), true);
      new MutationObserver((m) => { for (const r of m) for (const n of r.removedNodes) if (n.id) __focusLog.push(['removed', n.id]); }).observe(document.body, { subtree: true, childList: true }); 1""")
    press(b, 'shell', 'delete')
    ok = wait(lambda: [t['id'] for t in b.web_tabs()] == [tabs[0]['id'], tabs[1]['id']])
    check(bool(ok), 'Delete closes the focused tab', [t['id'] for t in b.web_tabs()])
    if not check(b.active_id() == f"tab-{tabs[1]['id']}", 'focus moves to the neighboring tab after closing', b.active_id()):
        print(json.dumps(b.js('__focusLog'), indent=1))

    # Command bar dialog: focus moves in, stays in, and returns.
    b.command(cmd='shellCommand', name='commandBar')
    nodes = b.ax()
    dialog = find(nodes, 'dialog', 'Command Bar')
    check(bool(dialog) and prop(dialog[0], 'modal') is True, 'command bar is a modal dialog named Command Bar', [role(x) for x in find(nodes, 'dialog')])
    check(b.active_id() == 'cmdk-input' and bool(find(nodes, 'combobox')), 'focus moves into the command bar combobox', b.active_id())
    check(b.js("document.getElementById('app').inert") is True and not find(nodes, 'navigation', 'Sidebar'), 'content behind the dialog is inert')
    press(b, 'shell', 'tab')
    check(b.active_id() == 'cmdk-input', 'Tab keeps focus inside the dialog', b.active_id())
    first = b.js("document.getElementById('cmdk-input').getAttribute('aria-activedescendant')")
    press(b, 'shell', 'down')
    second = b.js("document.getElementById('cmdk-input').getAttribute('aria-activedescendant')")
    options = [x for x in b.ax() if role(x) == 'option' and prop(x, 'selected')]
    check(first != second and len(options) == 1, 'ArrowDown moves the active option', (first, second))
    press(b, 'shell', 'escape')
    check(b.js("document.body.classList.contains('cmdk')") is False and b.active_id() == f"tab-{tabs[1]['id']}",
          'Escape closes the dialog and returns focus to where it was', b.active_id())

    # Native focus between the shell and the page.
    page = tabs[1]['id']
    b.command(cmd='activate', id=page)
    wait(lambda: b.js(f"document.querySelector('#tab-{page}').getAttribute('aria-selected')") == 'true')
    b.command(cmd='focus', target='page')
    check(bool(wait(lambda: b.state()['focused_view'] == page)), 'focus command moves native focus into the page', b.state()['focused_view'])
    press(b, page, 'f6')
    ok = wait(lambda: b.state()['focused_view'] == 'shell')
    check(bool(ok) and b.active_id().startswith('tab-'), 'F6 in the page moves focus to the sidebar', (b.state()['focused_view'], b.active_id()))
    press(b, 'shell', 'f6')
    check(bool(wait(lambda: b.state()['focused_view'] == page)), 'F6 in the shell moves focus back to the page', b.state()['focused_view'])
    b.js("document.querySelector('a').focus()", tab=page)
    press(b, page, 'tab')
    ok = wait(lambda: b.state()['focused_view'] == 'shell' and b.active_id() == 'sb-toggle')
    check(bool(ok), 'Tab past the end of the page continues at the start of the sidebar', (b.state()['focused_view'], b.active_id()))
    b.js("document.getElementById('assist').focus()")
    press(b, 'shell', 'tab')
    check(bool(wait(lambda: b.state()['focused_view'] == page)), 'Tab from the end of the sidebar enters the page', b.state()['focused_view'])
    b.js("document.querySelector('button').focus()", tab=page)
    press(b, page, 'tab', shift=True)
    ok = wait(lambda: b.state()['focused_view'] == 'shell' and b.active_id() == 'assist')
    check(bool(ok), 'Shift+Tab before the start of the page continues at the end of the sidebar', (b.state()['focused_view'], b.active_id()))

    # Web content accessibility tree.
    nodes = b.ax(page)
    check(bool(find(nodes, 'heading', 'Chromium fixture')), 'page heading is in the web accessibility tree')
    check(bool(find(nodes, 'button', 'Increment')), 'page button is in the web accessibility tree')
    check(bool(find(nodes, 'textbox', 'Draft')), 'labelled page textarea is in the web accessibility tree')
    check(bool(find(nodes, 'link', 'Next page')), 'page link is in the web accessibility tree')

    # Native macOS tree: enabled through the same application attribute VoiceOver sets.
    if not before.get('voiceover'):
        tree = native_nodes(b.state(tree=True).get('native'))
        areas = [n for n in tree if n.get('role') == 'AXWebArea']
        print(f'INFO: before an assistive client, the native tree has {len(tree)} nodes and {len(areas)} web areas')
    b.state(assistive=True)
    check(bool(wait(lambda: b.state()['enabled'])), 'an assistive client enables Chromium accessibility')

    def native_ready():
        tree = native_nodes(b.state(tree=True).get('native'))
        areas = {n.get('title') or n.get('label') for n in tree if n.get('role') == 'AXWebArea'}
        return tree if {'Atrium', titles[1]} <= areas else None

    tree = wait(native_ready, 20) or native_nodes(b.state(tree=True).get('native'))
    areas = sorted({n.get('title') or n.get('label') or '' for n in tree if n.get('role') == 'AXWebArea'})
    check({'Atrium', titles[1]} <= set(areas), 'the native tree contains web areas for the shell and the page', areas)
    texts = {n.get('title') or n.get('label') or n.get('value') for n in tree}
    check('Increment' in texts and titles[0] in texts, 'native tree exposes page controls and shell tab names', len(tree))
    check(any(n.get('role') == 'AXTabGroup' for n in tree), 'native tree exposes the tab list as AXTabGroup')
    b.state(assistive=False)
    check(bool(wait(lambda: b.state()['enabled'] is False or b.state()['voiceover'])), 'turning the assistive client off disables accessibility again')

    # Reduced motion.
    b.cdp('shell', 'Emulation.setEmulatedMedia', {'features': [{'name': 'prefers-reduced-motion', 'value': 'reduce'}]})
    motion = b.js("""(() => { const s = document.createElement('span'); s.className = 'spin'; document.body.append(s);
      const cs = getComputedStyle(s), r = { reduce: matchMedia('(prefers-reduced-motion: reduce)').matches, spin: cs.animationIterationCount,
      spinDuration: parseFloat(cs.animationDuration), sidebar: Math.max(...getComputedStyle(document.getElementById('sidebar')).transitionDuration.split(',').map(parseFloat)) };
      s.remove(); return r; })()""")
    check(motion['reduce'] and motion['spin'] == '1' and motion['spinDuration'] < .001 and motion['sidebar'] < .001,
          'prefers-reduced-motion stops looping animations and transitions', motion)
    b.cdp('shell', 'Emulation.setEmulatedMedia', {'features': []})
    normal = b.js("(() => { const s = document.createElement('span'); s.className = 'spin'; document.body.append(s); const v = getComputedStyle(s).animationIterationCount; s.remove(); return v; })()")
    check(normal == 'infinite', 'animations run normally without the preference', normal)

    # Contrast of text colors on the palette's opaque surfaces, light and dark.
    for scheme in ('light', 'dark'):
        b.cdp('shell', 'Emulation.setEmulatedMedia', {'features': [{'name': 'prefers-color-scheme', 'value': scheme}]})
        report = b.js(CONTRAST_JS)
        low = [f"{p['fg']} on {p['on']} {p['ratio']:.2f}" for p in report['pairs'] if p['ratio'] < 4.5]
        worst = min(p['ratio'] for p in report['pairs'])
        check(not low, f'{scheme} text colors reach 4.5:1 (AccentColor {report["accent"]}, lowest {worst:.2f})', low)
    b.cdp('shell', 'Emulation.setEmulatedMedia', {'features': []})


with tempfile.TemporaryDirectory(prefix='atrium-a11y-', dir='/private/tmp', ignore_cleanup_errors=True) as directory:
    profile = Path(directory)
    fixture = f'http://127.0.0.1:{args.fixture_port}'
    client = atrium_api.Client(f'http://127.0.0.1:{args.port}', data_dir=profile)
    log = (profile / 'launch.log').open('w')
    server = subprocess.Popen([sys.executable, str(ROOT / 'tools/fixture_server.py'), '--port', str(args.fixture_port)], stdout=subprocess.DEVNULL)
    env = {**atrium_api.instance_env(), 'ATRIUM_DEBUG_API': '1'}
    browser = subprocess.Popen([str(args.app.resolve()), '--data-dir', str(profile), 'run', '--port', str(args.port),
                                '--opencode-url', 'http://127.0.0.1:1'], stdout=log, stderr=log, env=env)
    watchdog = subprocess.Popen(['/bin/sh', str(args.watchdog), str(browser.pid)], stdout=subprocess.PIPE, text=True)
    try:
        run(Browser(client), fixture)
    except Exception as error:  # report, then still shut down cleanly
        traceback.print_exc()
        check(False, f'test aborted: {error!r}')
    finally:
        try:
            client.command(cmd='quit')
        except (urllib.error.URLError, TimeoutError, atrium_api.MissingToken):
            pass
        try:
            browser.wait(timeout=15)
        except subprocess.TimeoutExpired:
            browser.terminate()
            browser.wait(timeout=5)
        server.terminate()
        guard = watchdog.communicate(timeout=5)[0]
        check('FRONTMOST' not in guard, 'the test instance never became the frontmost app', guard.strip())
        log.close()
        if failures:
            print((profile / 'launch.log').read_text()[-3000:])

print(f"{'FAILED' if failures else 'PASSED'}: {len(failures)} failure(s)")
sys.exit(1 if failures else 0)
