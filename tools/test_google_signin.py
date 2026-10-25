#!/usr/bin/env python3
"""Check that Google sign-in, Gmail, YouTube, and third-party "Sign in with Google"
treat this build as an ordinary Chromium browser.

Starts its own isolated background instance (mock keychain, invisible window,
fresh profile) with a watchdog that kills it if it ever becomes frontmost. No
credentials are used: the sign-in check enters a random, nonexistent address
and stops at Google's answer. Third-party checks stop once Google's sign-in page
is shown.

Checks:
  fingerprint   (local) request headers, User-Agent Client Hints, navigator
                state and window globals as the page's own scripts see them,
                Runtime.enable console detection, agent scripts isolated from
                the page, no remote-debugging listener.
  google_signin accounts.google.com answers the identifier step normally.
  gmail         Gmail's signed-out entry point loads without a browser block.
  youtube       YouTube loads and its Sign in link reaches Google's sign-in page.
  third_party   A third-party "Sign in with Google" button reaches Google's flow.
  popup_opener  (informational) window.open keeps window.opener and postMessage.

Exit status: 0 when every required check passed, 1 when any failed (including
Google's "This browser or app may not be secure" block), 2 when nothing failed
but a network check was inconclusive (offline, captcha, consent loop).
"""
import argparse
import base64
import json
import os
import re
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import atrium_api

ROOT = Path(__file__).resolve().parent.parent
HINTS = ('Sec-CH-UA-Full-Version-List, Sec-CH-UA-Full-Version, Sec-CH-UA-Platform-Version, '
         'Sec-CH-UA-Arch, Sec-CH-UA-Bitness, Sec-CH-UA-Model, Sec-CH-UA-WoW64')
BLOCKED = re.compile(r"browser or app may not be secure|try using a different browser|couldn.t sign you in|"
                     r"browser (is )?(no longer|not) supported|update your browser|unsupported browser", re.I)
NOT_FOUND = re.compile(r"couldn.t find (this|your) (google )?account|enter a valid email", re.I)
CHALLENGE = re.compile(r"type the text you hear or see|verify it.s you|confirm you.re not a robot|unusual traffic", re.I)
GOOGLE_BUTTON = re.compile(r"(sign in|log in|login|continue|sign up) (with|using|via) google", re.I)
# Sites whose own markup holds the button; tried in order until one works. Buttons drawn by Google
# Identity Services inside a cross-origin iframe (Pinterest, Dropbox) are out of reach of page scripts.
THIRD_PARTY = [
    ('Stack Overflow', 'https://stackoverflow.com/users/login'),  # same-tab redirect
    ('Spotify', 'https://accounts.spotify.com/en/login'),  # popup
    ('Zoom', 'https://zoom.us/signin'),  # popup
]

FINGERPRINT_PAGE = '''<!doctype html><meta charset="utf-8"><title>Fingerprint fixture</title>
<button id="measure">Measure</button><output id="count">0</output><pre id="report"></pre>
<script>
window.fixtureGlobal = 1;
let cdpConsole = false, high = null, platformAuthenticator = null;
const bait = new Error('bait');
Object.defineProperty(bait, 'stack', { get() { cdpConsole = true; return ''; } });
navigator.userAgentData?.getHighEntropyValues(['fullVersionList', 'uaFullVersion', 'platformVersion', 'architecture', 'bitness'])
  .then(v => { high = v; });
window.PublicKeyCredential?.isUserVerifyingPlatformAuthenticatorAvailable?.().then(v => { platformAuthenticator = v; });
function measure() {
  console.debug(bait);
  const frame = document.createElement('iframe');
  frame.style.display = 'none';
  document.body.appendChild(frame);
  const base = new Set(Object.getOwnPropertyNames(frame.contentWindow));
  frame.remove();
  const own = new Set(['fixtureGlobal', 'measure']);
  const extra = Object.getOwnPropertyNames(window).filter(k => !base.has(k) && !own.has(k) && !/^\\d+$/.test(k));
  document.getElementById('report').textContent = JSON.stringify({
    ua: navigator.userAgent, brands: navigator.userAgentData?.brands, high, webdriver: navigator.webdriver,
    chrome: typeof chrome === 'object' ? Object.keys(chrome).sort() : typeof chrome, extra, cdpConsole,
    marks: [...document.documentElement.attributes].map(a => a.name), plugins: navigator.plugins.length,
    pdfViewer: navigator.pdfViewerEnabled, fedcm: typeof IdentityCredential, platformAuthenticator,
  });
}
let count = 0;
document.getElementById('measure').onclick = () => { document.getElementById('count').value = ++count; measure(); };
measure();
setInterval(measure, 300);
</script>'''

OPENER_PAGE = '''<!doctype html><meta charset="utf-8"><title>Opener fixture</title>
<button id="open">Open popup</button><output id="status">waiting</output>
<script>
addEventListener('message', e => { if (e.data === 'popup-ready') document.getElementById('status').value = 'message from ' + e.origin; });
document.getElementById('open').onclick = () => {
  const w = window.open(location.origin.replace('127.0.0.1', 'localhost') + '/popup', 'atrium_popup', 'width=480,height=600');
  if (!w) document.getElementById('status').value = 'window.open returned null (user activation: ' + navigator.userActivation?.isActive + ')';
};
</script>'''

POPUP_PAGE = '''<!doctype html><meta charset="utf-8"><title>Popup fixture</title><p id="state"></p>
<script>
document.title = window.opener ? 'popup has opener' : 'popup without opener';
if (window.opener) window.opener.postMessage('popup-ready', '*');
</script>'''


class Fixture(BaseHTTPRequestHandler):
    seen = []

    def do_GET(self):
        path = self.path.split('?')[0]
        Fixture.seen.append((path, dict(self.headers)))
        body = {'/fingerprint': FINGERPRINT_PAGE, '/opener': OPENER_PAGE, '/popup': POPUP_PAGE}.get(path)
        self.send_response(200 if body else 404)
        self.send_header('Content-Type', 'text/html; charset=utf-8')
        self.send_header('Cache-Control', 'no-store')
        if path == '/fingerprint':
            self.send_header('Accept-CH', HINTS)
            self.send_header('Critical-CH', HINTS)
        data = (body or 'not found').encode()
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *_):
        pass


class Inconclusive(Exception):
    pass


class Browser:
    def __init__(self, client):
        self.client = client

    def ev(self, code, tries=12):
        """Evaluate in the agent's tab, retrying while a navigation replaces the document."""
        for attempt in range(tries):
            try:
                content = self.client.tool('evaluate', {'code': code}, timeout=40)
            except (AssertionError, urllib.error.URLError, TimeoutError):
                if attempt == tries - 1:
                    raise
                time.sleep(1)
                continue
            text = '\n'.join(c.get('text', '') for c in content)
            try:
                return json.loads(text)
            except json.JSONDecodeError:
                return text

    def navigate(self, url):
        try:
            self.client.tool('navigate', {'url': url}, timeout=60)
        except (AssertionError, TimeoutError, urllib.error.URLError) as error:
            print(f'  (navigate {url}: {str(error)[:160]})', flush=True)

    def open(self, url):
        """Open a tab for the agent and show it, as a user would: hidden tabs are throttled."""
        before = {t['id'] for t in self.tabs()}
        try:
            self.client.tool('open_tab', {'url': url}, timeout=60)
        except (AssertionError, TimeoutError, urllib.error.URLError) as error:
            print(f'  (open {url}: {str(error)[:160]})', flush=True)
        tab = next((t['id'] for t in self.tabs() if t['id'] not in before), None)
        if tab:
            self.client.command(cmd='activate', id=tab)

    def tabs(self):
        return [t for t in self.client.api('/api/browser/tabs')['tabs'] if t['kind'] != 'scratch']

    def switch(self, tab_id):
        index = next(i for i, t in enumerate(self.tabs()) if t['id'] == tab_id)
        self.client.tool('switch_tab', {'index': index})
        self.client.command(cmd='activate', id=tab_id)

    def screenshot(self, path):
        try:
            content = self.client.tool('screenshot', {})
            image = next(c for c in content if c.get('type') == 'image')
            path.write_bytes(base64.b64decode(image['data']))
            return str(path)
        except Exception:
            return None


PAGE_STATE = r'''
const visible = e => !!e && e.getClientRects().length > 0 && getComputedStyle(e).visibility !== 'hidden';
const text = document.body ? document.body.innerText : '';
return {
  url: location.href, host: location.hostname, title: document.title, text: text.slice(0, 2000),
  identifier: visible(document.querySelector('input[name=identifier], input[type=email]')),
  password: [...document.querySelectorAll('input[type=password]')].some(e => e.name !== 'hiddenPassword' && visible(e)),
  captcha: visible(document.querySelector('#captchaimg, img[src*="Captcha"], iframe[src*="recaptcha"]')),
};
'''

CONSENT = r'''
const visible = e => !!e && e.getClientRects().length > 0;
if (!/^consent\./.test(location.hostname)) return false;
const buttons = [...document.querySelectorAll('button, input[type=submit]')].filter(visible);
const label = b => (b.innerText || b.value || b.getAttribute('aria-label') || '').trim();
const pick = buttons.find(b => /reject all/i.test(label(b))) || buttons.find(b => /accept all|i agree/i.test(label(b))) || buttons[0];
if (!pick) return false;
pick.click();
return label(pick) || true;
'''


def poll(browser, classify, timeout):
    """Re-read the page until classify(state) returns a verdict or time runs out."""
    deadline = time.time() + timeout
    state = None
    while time.time() < deadline:
        try:
            if browser.ev(CONSENT, tries=2):
                time.sleep(2)
                continue
            state = browser.ev(PAGE_STATE, tries=2)
        except Exception as error:
            state = {'error': str(error)[:200]}
            time.sleep(1)
            continue
        verdict = classify(state)
        if verdict:
            return verdict, state
        time.sleep(1)
    return None, state


def google_verdict(state, need_password=False):
    if 'text' not in state:
        return None
    text = state['text']
    if BLOCKED.search(text):
        return 'fail'
    if state['captcha'] or CHALLENGE.search(text):
        return 'inconclusive'
    if state['host'] == 'accounts.google.com':
        if state['password'] or re.search(r'enter your password|show password', text, re.I):
            return 'pass'
        if NOT_FOUND.search(text) and not need_password:
            return 'pass'
    return None


def check_fingerprint(browser, args, fixture, expected):
    fixture.seen.clear()
    browser.open(f'{args.fixture_url}/fingerprint')
    time.sleep(1.5)
    isolated = browser.ev("return { pageGlobal: typeof window.fixtureGlobal, agent: typeof __atrium, report: document.querySelector('#report').textContent }")
    tree = browser.ev("return __atrium.snapshot({ interactive: true }).tree")
    ref = re.search(r'button "Measure" \[(\w+)\]', tree).group(1)
    browser.ev(f"return __atrium.click({json.dumps(ref)})")
    time.sleep(.8)
    after = browser.ev("return { count: document.querySelector('#count').value, report: document.querySelector('#report').textContent }")
    report = json.loads(after['report'])
    headers = [h for p, h in fixture.seen if p == '/fingerprint']
    first, hinted = headers[0], headers[-1]
    lower = {k.lower(): v for k, v in hinted.items()}
    ua = lower.get('user-agent', '')
    full = (report.get('high') or {}).get('uaFullVersion')
    major = ua_major = re.search(r'Chrome/(\d+)\.0\.0\.0 Safari/537\.36$', ua)
    major = major and major.group(1)
    full_list = lower.get('sec-ch-ua-full-version-list', '')
    listening = subprocess.run(['lsof', '-nP', '-a', '-p', str(args.pid), '-iTCP', '-sTCP:LISTEN', '-Fn'],
                               capture_output=True, text=True).stdout
    ports = sorted({int(m) for m in re.findall(r':(\d+)$', listening, re.M)})
    problems = []
    expect = [
        (ua_major is not None, f'User-Agent is not the standard reduced Chrome string: {ua}'),
        (not re.search(r'headless|electron|cef|atrium', ua, re.I), f'User-Agent names a non-browser product: {ua}'),
        (report['ua'] == ua, 'navigator.userAgent differs from the User-Agent header'),
        (f'"Chromium";v="{major}"' in lower.get('sec-ch-ua', ''), f"sec-ch-ua lacks Chromium {major}: {lower.get('sec-ch-ua')}"),
        ('headless' not in lower.get('sec-ch-ua', '').lower(), 'sec-ch-ua contains HeadlessChrome'),
        (f'"Chromium";v="{full}"' in full_list and lower.get('sec-ch-ua-full-version') == f'"{full}"',
         f'full version list {full_list!r} disagrees with uaFullVersion {full}'),
        (not expected or full == expected, f'Chromium version {full} differs from the bundled engine {expected}'),
        (any(b['brand'] == 'Chromium' and b['version'] == major for b in report.get('brands') or []), 'navigator.userAgentData.brands lacks Chromium'),
        (report['webdriver'] is False, f"navigator.webdriver is {report['webdriver']}"),
        (report['extra'] == [], f"pages can see injected globals: {report['extra']}"),
        (not report['cdpConsole'], 'console messages are serialized to a DevTools client (Runtime.enable is detectable)'),
        (report['marks'] == [], f"injected attributes on <html>: {report['marks']}"),
        (isolated['pageGlobal'] == 'undefined' and isolated['agent'] == 'object', f'agent scripts are not isolated: {isolated}'),
        (after['count'] == '1', 'clicking by snapshot ref did not reach the page (isolated world state lost?)'),
        (ports == [args.port], f'browser listens on unexpected ports {ports} (remote debugging?)'),
        ('sec-ch-ua' in {k.lower() for k in first}, 'low-entropy client hints missing on first request'),
    ]
    problems = [message for ok, message in expect if not ok]
    detail = {
        'user_agent': ua, 'sec_ch_ua': lower.get('sec-ch-ua'), 'full_version_list': full_list,
        'platform': lower.get('sec-ch-ua-platform'), 'platform_version': lower.get('sec-ch-ua-platform-version'),
        'x_client_data': 'x-client-data' in lower, 'webdriver': report['webdriver'], 'window.chrome': report['chrome'],
        'plugins': report['plugins'], 'pdf_viewer': report['pdfViewer'], 'IdentityCredential (FedCM)': report['fedcm'],
        'platform_authenticator': report['platformAuthenticator'], 'listening_ports': ports,
    }
    return ('fail' if problems else 'pass'), {'problems': problems, **detail}


def check_signin(browser, args):
    browser.open('https://accounts.google.com/ServiceLogin?hl=en')
    verdict, state = poll(browser, lambda s: 'text' in s and (s['identifier'] or google_verdict(s)) and 'ready', 30)
    if not verdict:
        raise Inconclusive(f'sign-in page did not load: {state}')
    if google_verdict(state) == 'fail':
        return 'fail', {'stage': 'before identifier', 'url': state['url'], 'text': state['text'][:300]}
    address = args.identifier or f'atrium{uuid.uuid4().hex}{uuid.uuid4().hex[:10]}@gmail.com'
    tree = browser.ev("return __atrium.snapshot({ interactive: true }).tree")
    field = re.search(r'textbox "Email or phone" \[(\w+)\]', tree) or re.search(r'textbox[^\n]*\[(\w+)\]', tree)
    browser.ev(f"return __atrium.type({json.dumps(field.group(1))}, {json.dumps(address)}, true, false)")
    tree = browser.ev("return __atrium.snapshot({ interactive: true }).tree")
    button = re.search(r'button "Next" \[(\w+)\]', tree)
    if not button:
        raise Inconclusive('no Next button on the identifier page')
    browser.ev(f"return __atrium.click({json.dumps(button.group(1))})")
    verdict, state = poll(browser, lambda s: google_verdict(s, need_password=bool(args.identifier)), 30)
    detail = {'identifier': 'supplied' if args.identifier else 'random nonexistent', 'url': (state or {}).get('url'),
              'answer': re.sub(r'\s+', ' ', (state or {}).get('text', ''))[:240]}
    if verdict is None:
        raise Inconclusive(f'no recognizable answer after Next: {detail}')
    return verdict, detail


def check_gmail(browser, args):
    browser.navigate('https://mail.google.com/mail/?hl=en')
    def classify(s):
        if 'text' not in s:
            return None
        if BLOCKED.search(s['text']):
            return 'fail'
        if s['host'] == 'accounts.google.com' and s['identifier']:
            return 'pass'
        if s['host'] in ('workspace.google.com', 'www.google.com') and re.search(r'gmail', s['url'] + s['title'], re.I):
            return 'pass'
        return None
    verdict, state = poll(browser, classify, 40)
    if verdict is None:
        raise Inconclusive(f"Gmail did not reach a signed-out page: {(state or {}).get('url')}")
    return verdict, {'url': state['url'], 'title': state['title']}


def check_youtube(browser, args):
    browser.navigate('https://www.youtube.com/?hl=en&persist_hl=1')
    find = r'''
const a = [...document.querySelectorAll('a[href*="accounts.google.com"]')].find(a => /ServiceLogin|signin/i.test(a.href));
return { app: !!document.querySelector('ytd-app'), signin: a ? a.href : null, url: location.href,
  text: (document.body?.innerText || '').slice(0, 1500) };'''
    state = None
    for _ in range(30):
        try:
            if browser.ev(CONSENT, tries=2):
                time.sleep(2)
                continue
            state = browser.ev(find, tries=2)
        except Exception as error:
            state = {'error': str(error)[:200]}
        if state.get('text') and BLOCKED.search(state['text']):
            return 'fail', {'url': state['url'], 'text': state['text'][:300]}
        if state.get('app') and state.get('signin'):
            break
        time.sleep(1)
    else:
        raise Inconclusive(f"YouTube did not show a Sign in link: {(state or {}).get('url')}")
    browser.navigate(state['signin'])
    verdict, page = poll(browser, lambda s: 'text' in s and (google_verdict(s) or (s['host'] == 'accounts.google.com' and s['identifier'] and 'pass')), 30)
    if verdict is None:
        raise Inconclusive(f"YouTube's Sign in link did not reach Google sign-in: {(page or {}).get('url')}")
    return verdict, {'home': state['url'], 'signin_page': page['url']}


def check_third_party(browser, args):
    tried = {}
    sites = [s for s in THIRD_PARTY if not args.sites or s[0] in args.sites.split(',')]
    for name, url in sites:
        before = {t['id'] for t in browser.tabs()}
        browser.navigate(url)
        clicked = None
        for _ in range(15):
            try:
                tree = browser.ev("await __atrium.settle(300, 2000); return __atrium.snapshot({ interactive: true }).tree", tries=2)
            except Exception as error:
                tree = ''
            match = next((m for m in re.finditer(r'- (?:button|link|clickable) "([^"]*)" \[(\w+)\]', tree) if GOOGLE_BUTTON.search(m.group(1))), None)
            if match:
                clicked = match.group(1)
                try:
                    browser.ev(f"return __atrium.click({json.dumps(match.group(2))})", tries=1)
                except Exception:
                    pass
                break
            time.sleep(1)
        if not clicked:
            tried[name] = 'no Google button found (layout, region or bot check)'
            continue
        time.sleep(2)
        popup = next((t for t in browser.tabs() if t['id'] not in before), None)
        if popup:
            browser.switch(popup['id'])
        verdict, state = poll(browser, lambda s: 'text' in s and (google_verdict(s) or (
            s['host'] == 'accounts.google.com' and (s['identifier'] or re.search(r'choose an account|sign in', s['text'], re.I)) and 'pass')), 30)
        if verdict:
            return verdict, {'site': name, 'button': clicked, 'opened_as': 'new tab (popup)' if popup else 'same tab',
                             'google_url': re.sub(r'([?&](client_id|state|nonce|redirect_uri)=)[^&]*', r'\1…', state['url'])[:300],
                             'also_tried': tried}
        tried[name] = f"clicked {clicked!r} but reached {(state or {}).get('url')}"
    raise Inconclusive(f'no third-party site reached Google sign-in: {tried}')


def check_popup_opener(browser, args):
    before = {t['id'] for t in browser.tabs()}
    browser.navigate(f'{args.fixture_url}/opener')
    tree = browser.ev("return __atrium.snapshot({ interactive: true }).tree")
    ref = re.search(r'button "Open popup" \[(\w+)\]', tree).group(1)
    browser.ev(f"return __atrium.click({json.dumps(ref)})")
    time.sleep(3)
    status = browser.ev("return document.querySelector('#status').value")
    popup = next((t for t in browser.tabs() if t['id'] not in before), None)
    detail = {'opener_status': status, 'popup': popup and {'title': popup['title'], 'url': popup['url']}}
    ok = bool(popup) and popup['title'] == 'popup has opener' and status.startswith('message from')
    return ('pass' if ok else 'gap'), detail


def expected_chromium():
    header = ROOT / 'target/cef/include/cef_version.h'
    if not header.exists():
        return None
    text = header.read_text()
    parts = [re.search(rf'#define CHROME_VERSION_{p} (\d+)', text) for p in ('MAJOR', 'MINOR', 'BUILD', 'PATCH')]
    return '.'.join(m.group(1) for m in parts) if all(parts) else None


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--app', type=Path, default=ROOT / 'target/bundle/Atrium.app/Contents/MacOS/atrium')
    parser.add_argument('--port', type=int, default=7840, help='browser API port')
    parser.add_argument('--fixture-port', type=int, default=7841)
    parser.add_argument('--watchdog', type=Path, default=ROOT.parent / 'watchdog.sh',
                        help='script run as `watchdog PID` that kills the instance if it becomes frontmost')
    parser.add_argument('--offline', action='store_true', help='run only the local fingerprint and popup checks')
    parser.add_argument('--only', help='comma-separated check names to run')
    parser.add_argument('--sites', help='comma-separated third-party site names to try, e.g. "Pinterest,Zoom"')
    parser.add_argument('--identifier', help='your own Google address: require the password step (no password is typed)')
    parser.add_argument('--output', type=Path, help='write results as JSON')
    parser.add_argument('--keep-profile', action='store_true')
    args = parser.parse_args()
    args.fixture_url = f'http://127.0.0.1:{args.fixture_port}'

    server = ThreadingHTTPServer(('127.0.0.1', args.fixture_port), Fixture)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    profile = Path(tempfile.mkdtemp(prefix='atrium-google-', dir='/private/tmp'))
    client = atrium_api.Client(f'http://127.0.0.1:{args.port}', data_dir=profile)
    browser = Browser(client)
    log = (profile / 'launch.log').open('w')
    process = subprocess.Popen([str(args.app.resolve()), '--data-dir', str(profile), 'run', '--port', str(args.port),
                                '--opencode-url', 'http://127.0.0.1:1'], stdout=log, stderr=log, env=atrium_api.instance_env())
    args.pid = process.pid
    watchdog = None
    if args.watchdog.exists():
        watchdog = subprocess.Popen(['/bin/sh', str(args.watchdog), str(process.pid)], stdout=subprocess.PIPE, text=True)
    else:
        print(f'warning: no watchdog at {args.watchdog}', flush=True)

    checks = [('fingerprint', True, lambda: check_fingerprint(browser, args, Fixture, expected_chromium()))]
    if not args.offline:
        checks += [('google_signin', True, lambda: check_signin(browser, args)),
                   ('gmail', True, lambda: check_gmail(browser, args)),
                   ('youtube', True, lambda: check_youtube(browser, args)),
                   ('third_party', True, lambda: check_third_party(browser, args))]
    checks.append(('popup_opener', False, lambda: check_popup_opener(browser, args)))
    if args.only:
        wanted = set(args.only.split(','))
        checks = [c for c in checks if c[0] in wanted]

    results = {}
    try:
        for _ in range(120):
            try:
                if client.api('/api/browser/memory').get('shell_ready'):
                    break
            except (urllib.error.URLError, TimeoutError, ConnectionError):
                pass
            if process.poll() is not None:
                raise SystemExit(f'browser exited early; see {profile / "launch.log"}')
            time.sleep(.5)
        for name, required, run in checks:
            started = time.time()
            try:
                verdict, detail = run()
            except Inconclusive as error:
                verdict, detail = 'inconclusive', {'reason': str(error)[:600]}
            except Exception as error:
                verdict, detail = ('error' if required else 'gap'), {'reason': f'{type(error).__name__}: {str(error)[:600]}'}
            if verdict not in ('pass',):
                shot = browser.screenshot(profile / f'{name}.jpg')
                if shot:
                    detail['screenshot'] = shot
            results[name] = {'verdict': verdict, 'required': required, 'seconds': round(time.time() - started, 1), **detail}
            print(f"{name}: {verdict.upper()}{'' if required else ' (informational)'}", flush=True)
            for key, value in detail.items():
                print(f'  {key}: {value}', flush=True)
    finally:
        try:
            client.api('/api/browser/command', {'cmd': 'quit'})
        except Exception:
            pass
        try:
            process.wait(timeout=20)
        except subprocess.TimeoutExpired:
            process.kill()
        server.shutdown()
        if watchdog:
            out = watchdog.communicate(timeout=10)[0]
            if 'FRONTMOST' in out:
                results['background'] = {'verdict': 'fail', 'required': True, 'reason': out.strip()}
                print('background: FAIL (the instance became frontmost and was killed)', flush=True)
        log.close()
        if args.output:
            args.output.write_text(json.dumps(results, indent=2) + '\n')
        if not args.keep_profile and not any(r['verdict'] != 'pass' and 'screenshot' in r for r in results.values()):
            subprocess.run(['rm', '-rf', str(profile)])
        else:
            print(f'profile and screenshots kept in {profile}', flush=True)

    required = [r['verdict'] for r in results.values() if r['required']]
    if any(v in ('fail', 'error') for v in required) or len(results) == 0:
        sys.exit(1)
    if any(v != 'pass' for v in required):
        sys.exit(2)
    print('PASS', flush=True)


if __name__ == '__main__':
    main()
