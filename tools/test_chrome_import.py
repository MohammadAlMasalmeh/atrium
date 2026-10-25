#!/usr/bin/env python3
"""Import a synthetic Chrome profile into an isolated background browser and verify it.

Builds a fake Chrome root in /private/tmp (Bookmarks, History, Cookies encrypted with
a test key, a session file, Preferences and Web Data), points a background instance at
it with ATRIUM_TEST_CHROME_ROOT / ATRIUM_TEST_CHROME_SAFE_STORAGE (honored only with
ATRIUM_MOCK_KEYCHAIN=1), and checks every import, its idempotence, and the report.
The user's real Chrome data and login keychain are never touched.
"""
import argparse
import hashlib
import json
import sqlite3
import struct
import subprocess
import sys
import tempfile
import time
import urllib.error
from pathlib import Path

import atrium_api

WATCHDOG = '/Users/mohammadalmasalmeh/Desktop/atrium-rs-wt/watchdog.sh'
PASSWORD = 'atrium-test-safe-storage'
CHROME_EPOCH_US = 11_644_473_600_000_000

parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
parser.add_argument('--app', type=Path, default=Path('target/bundle/atrium.app/Contents/MacOS/atrium'))
parser.add_argument('--port', type=int, default=7860)
parser.add_argument('--fixture-port', type=int, default=7861)
args = parser.parse_args()
FIX = f'http://127.0.0.1:{args.fixture_port}'
COOKIE_HOST = f'app.import.localhost:{args.fixture_port}'


def chrome_time(seconds_from_now):
    return int((time.time() + seconds_from_now) * 1_000_000) + CHROME_EPOCH_US


# ── Synthetic Chrome profile ───────────────────────────────────────────────
def encrypt(host, value):
    """Chrome macOS cookie format (database version 24): v10 + AES-128-CBC(SHA256(host) + value)."""
    key = hashlib.pbkdf2_hmac('sha1', PASSWORD.encode(), b'saltysalt', 1003, 16)
    plain = hashlib.sha256(host.encode()).digest() + value.encode()
    out = subprocess.run(['openssl', 'enc', '-aes-128-cbc', '-K', key.hex(), '-iv', '20' * 16],
                         input=plain, capture_output=True, check=True).stdout
    return b'v10' + out


def write_bookmarks(profile):
    def url(name, href, guid):
        return {'type': 'url', 'name': name, 'url': href, 'guid': guid, 'date_added': str(chrome_time(-86400))}
    roots = {
        'bookmark_bar': {'type': 'folder', 'name': 'Bookmarks bar', 'guid': 'root-bar', 'children': [
            url('Fixture A', f'{FIX}/page/bookmark-a', 'bm-a'),
            {'type': 'folder', 'name': 'Work', 'guid': 'folder-work', 'children': [
                {'type': 'folder', 'name': 'Nested', 'guid': 'folder-nested', 'children': [
                    url('Deep link', f'{FIX}/page/bookmark-deep', 'bm-deep')]},
                url('Script', 'javascript:alert(1)', 'bm-js'),
            ]},
        ]},
        'other': {'type': 'folder', 'name': 'Other bookmarks', 'guid': 'root-other', 'children': [
            url('Other link', f'{FIX}/page/bookmark-other', 'bm-other')]},
        'synced': {'type': 'folder', 'name': 'Mobile bookmarks', 'guid': 'root-synced', 'children': []},
    }
    (profile / 'Bookmarks').write_text(json.dumps({'version': 1, 'roots': roots}))


def write_history(profile):
    db = sqlite3.connect(profile / 'History')
    db.executescript('''
        CREATE TABLE urls(id INTEGER PRIMARY KEY, url LONGVARCHAR, title LONGVARCHAR, visit_count INTEGER DEFAULT 0 NOT NULL,
            typed_count INTEGER DEFAULT 0 NOT NULL, last_visit_time INTEGER NOT NULL, hidden INTEGER DEFAULT 0 NOT NULL);
        CREATE TABLE visits(id INTEGER PRIMARY KEY, url INTEGER NOT NULL, visit_time INTEGER NOT NULL, from_visit INTEGER,
            transition INTEGER DEFAULT 0 NOT NULL, segment_id INTEGER, visit_duration INTEGER DEFAULT 0 NOT NULL);''')
    urls = [(1, f'{FIX}/page/history-a', 'History A', 3, 1, 0), (2, f'{FIX}/page/history-b', 'History B', 1, 0, 0),
            (3, f'{FIX}/page/frame', 'Subframe', 1, 0, 0), (4, f'{FIX}/page/hidden', 'Hidden', 1, 0, 1),
            (5, 'javascript:alert(1)', 'Script', 1, 0, 0)]
    for row in urls:
        db.execute('INSERT INTO urls VALUES (?,?,?,?,?,?,?)', row[:5] + (chrome_time(-60), row[5]))
    visits = [(1, chrome_time(-3600), 1), (1, chrome_time(-7200), 0x30000000), (1, chrome_time(-10800), 0),
              (2, chrome_time(-60), 0), (3, chrome_time(-60), 3), (4, chrome_time(-60), 0), (5, chrome_time(-60), 0)]
    db.executemany('INSERT INTO visits(url, visit_time, transition) VALUES (?,?,?)', visits)
    db.commit()
    db.close()


COOKIES = [
    # name, host_key, path, value, secure, httponly, samesite, expires (seconds from now or None), extra
    ('host_only', 'app.import.localhost', '/', 'v-host', 0, 0, 1, 86400, {}),
    ('domain_wide', '.import.localhost', '/', 'v-domain', 0, 0, -1, None, {}),
    ('http_only', 'app.import.localhost', '/', 'v-http', 0, 1, 2, 86400, {}),
    ('secure_one', '.import.localhost', '/', 'v-secure', 1, 0, 0, 86400, {}),
    ('other_path', 'app.import.localhost', '/elsewhere', 'v-path', 0, 0, 1, 86400, {}),
    ('expired', 'app.import.localhost', '/', 'v-expired', 0, 0, 1, -60, {}),
    ('partitioned', 'app.import.localhost', '/', 'v-part', 1, 0, 0, 86400, {'top_frame_site_key': 'http://localhost'}),
    ('tampered', 'app.import.localhost', '/', 'v-tampered', 0, 0, 1, 86400, {'encrypt_for': 'evil.localhost'}),
    ('unrelated', 'unrelated.invalid', '/', 'v-unrelated', 0, 0, 1, 86400, {}),
    ('legacy_plain', 'app.import.localhost', '/', 'v-plain', 0, 0, 1, 86400, {'plain': True}),
]
IMPORTABLE = {'host_only', 'domain_wide', 'http_only', 'secure_one', 'other_path', 'unrelated', 'legacy_plain'}


def write_cookies(profile):
    db = sqlite3.connect(profile / 'Cookies')
    db.executescript('''
        CREATE TABLE meta(key LONGVARCHAR NOT NULL UNIQUE PRIMARY KEY, value LONGVARCHAR);
        INSERT INTO meta VALUES ('version', '24'), ('last_compatible_version', '24');
        CREATE TABLE cookies(creation_utc INTEGER NOT NULL, host_key TEXT NOT NULL, top_frame_site_key TEXT NOT NULL,
            name TEXT NOT NULL, value TEXT NOT NULL, encrypted_value BLOB NOT NULL, path TEXT NOT NULL,
            expires_utc INTEGER NOT NULL, is_secure INTEGER NOT NULL, is_httponly INTEGER NOT NULL,
            last_access_utc INTEGER NOT NULL, has_expires INTEGER NOT NULL, is_persistent INTEGER NOT NULL,
            priority INTEGER NOT NULL, samesite INTEGER NOT NULL, source_scheme INTEGER NOT NULL,
            source_port INTEGER NOT NULL, last_update_utc INTEGER NOT NULL, source_type INTEGER NOT NULL,
            has_cross_site_ancestor INTEGER NOT NULL);''')
    for name, host, path, value, secure, httponly, samesite, expires, extra in COOKIES:
        plain = extra.get('plain')
        encrypted = b'' if plain else encrypt(extra.get('encrypt_for', host), value)
        persistent = expires is not None
        db.execute('INSERT INTO cookies VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)', (
            chrome_time(-100), host, extra.get('top_frame_site_key', ''), name, value if plain else '', encrypted, path,
            chrome_time(expires) if persistent else 0, secure, httponly, chrome_time(-50), int(persistent), int(persistent),
            1, samesite, 2 if secure else 1, 443 if secure else 80, chrome_time(-50), 0, 0))
    db.commit()
    db.close()


def snss_command(command_id, payload):
    return struct.pack('<HB', len(payload) + 1, command_id) + payload


def pickle_string(data, unit=1):
    count = len(data) // unit
    return struct.pack('<i', count) + data + b'\0' * (-len(data) % 4)


def navigation(tab, index, url, title):
    body = struct.pack('<ii', tab, index) + pickle_string(url.encode()) + pickle_string(title.encode('utf-16-le'), 2)
    body += pickle_string(b'') + struct.pack('<i', 0)
    return snss_command(6, struct.pack('<I', len(body)) + body)


def write_session(profile):
    """Chrome's SNSS command log: window 1 is open, window 2 was closed."""
    ints = lambda *v: struct.pack(f'<{len(v)}i', *v)
    commands = [
        snss_command(9, ints(1, 0)),                      # window 1 is a normal window
        snss_command(0, ints(1, 10)), snss_command(2, ints(10, 0)),
        navigation(10, 0, f'{FIX}/page/session-old', 'Old'),
        navigation(10, 1, f'{FIX}/page/session-one', 'Session one'),
        snss_command(7, ints(10, 1)),                     # tab 10 is on its second entry
        snss_command(0, ints(1, 11)), snss_command(2, ints(11, 1)),
        navigation(11, 0, f'{FIX}/page/session-two', 'Session two'),
        snss_command(0, ints(1, 12)), snss_command(2, ints(12, 2)),
        navigation(12, 0, 'chrome://settings/', 'Settings'),
        snss_command(8, ints(1, 1)),
        snss_command(0, ints(2, 20)), navigation(20, 0, f'{FIX}/page/session-closed', 'Closed'),
        snss_command(17, ints(2, 0) + b'\0' * 8),
    ]
    sessions = profile / 'Sessions'
    sessions.mkdir()
    (sessions / 'Session_13370000000000000').write_bytes(b'SNSS' + struct.pack('<i', 3) + b''.join(commands))


def write_search(profile):
    guid = '0f4c3a1e-1111-4222-8333-944455556666'
    (profile / 'Preferences').write_text(json.dumps({'default_search_provider': {'synced_guid': guid}}))
    db = sqlite3.connect(profile / 'Web Data')
    db.execute('CREATE TABLE keywords (id INTEGER PRIMARY KEY, short_name VARCHAR NOT NULL, keyword VARCHAR NOT NULL, '
               'favicon_url VARCHAR NOT NULL, url VARCHAR NOT NULL, sync_guid VARCHAR)')
    db.execute('INSERT INTO keywords VALUES (1, ?, ?, ?, ?, ?)', ('Google', 'google.com', '', '{google:baseURL}search?q={searchTerms}', 'other'))
    db.execute('INSERT INTO keywords VALUES (2, ?, ?, ?, ?, ?)', ('Fixture Search', 'fixture', '', f'{FIX}/search?q={{searchTerms}}&{{google:RLZ}}src=atrium', guid))
    db.commit()
    db.close()


def make_chrome_root(root):
    profile = root / 'Default'
    profile.mkdir(parents=True)
    (root / 'Local State').write_text(json.dumps({'profile': {'info_cache': {'Default': {'name': 'Synthetic'}}}}))
    write_bookmarks(profile)
    write_history(profile)
    write_cookies(profile)
    write_session(profile)
    write_search(profile)


# ── Browser driving ────────────────────────────────────────────────────────
def check(condition, message, detail=None):
    if not condition:
        raise AssertionError(f'{message}: {json.dumps(detail, indent=1) if detail is not None else ""}')


def main(work):
    chrome, data = work / 'chrome', work / 'profile'
    make_chrome_root(chrome)
    client = atrium_api.Client(f'http://127.0.0.1:{args.port}', data_dir=data)
    api = lambda path, data=None: client.api(path, data, timeout=120)
    tool = client.tool
    env = {**atrium_api.instance_env(), 'ATRIUM_TEST_CHROME_ROOT': str(chrome), 'ATRIUM_TEST_CHROME_SAFE_STORAGE': PASSWORD}
    fixture = subprocess.Popen([sys.executable, str(Path(__file__).with_name('fixture_server.py')), '--port', str(args.fixture_port)],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    log = (work / 'launch.log').open('w')
    browser = subprocess.Popen([str(args.app.resolve()), '--data-dir', str(data), 'run', '--port', str(args.port),
                                '--opencode-url', 'http://127.0.0.1:1'], stdout=log, stderr=log, env=env)
    watchdog_log = (work / 'watchdog.log').open('w')
    watchdog = subprocess.Popen(['/bin/sh', WATCHDOG, str(browser.pid)], stdout=watchdog_log, stderr=watchdog_log)
    try:
        for _ in range(120):
            try:
                if api('/api/browser/memory').get('shell_ready'):
                    break
            except (urllib.error.URLError, TimeoutError, ConnectionError):
                pass
            time.sleep(.5)
        else:
            raise AssertionError('Browser did not become ready')

        profiles = api('/api/setup/profiles')['profiles']
        check(profiles == [{'id': 'Default', 'name': 'Synthetic'}], 'only the synthetic profile is visible', profiles)

        kinds = {'profile': 'Default', 'bookmarks': True, 'history': True, 'tabs': True, 'search_engine': True}
        first = api('/api/setup/import', kinds)
        r = first['report']['results']
        check(not first['warnings'], 'first import has no warnings', first)
        check(sorted(first['imported']) == ['bookmarks', 'history', 'search_engine', 'tabs'], 'all kinds imported', first['imported'])
        check((r['bookmarks']['imported'], r['bookmarks']['added'], r['bookmarks']['folders'], r['bookmarks']['skipped']) == (3, 3, 2, 1), 'bookmark counts', r['bookmarks'])
        folders = {b['title']: b['folder'] for b in first['bookmarks']}
        check(folders == {'Fixture A': 'Bookmarks bar', 'Deep link': 'Bookmarks bar / Work / Nested', 'Other link': 'Other bookmarks'}, 'folder structure', first['bookmarks'])
        check((r['history']['imported'], r['history']['urls_added']) == (4, 2), 'history visits (subframe, hidden and script excluded)', r['history'])
        check((r['tabs']['imported'], r['tabs']['skipped'], r['tabs']['found']) == (2, 1, 3), 'session tabs', r['tabs'])
        check(r['search_engine']['engine']['template'] == f'{FIX}/search?q={{searchTerms}}&src=atrium', 'search template', r['search_engine'])

        second = api('/api/setup/import', kinds)['report']['results']
        check((second['bookmarks']['added'], second['bookmarks']['unchanged'], second['bookmarks']['removed']) == (0, 3, 0), 'bookmark re-import is a no-op', second['bookmarks'])
        check((second['history']['imported'], second['history']['already_imported']) == (0, 4), 'history re-import adds nothing', second['history'])
        check((second['tabs']['imported'], second['tabs']['already_open']) == (0, 2), 'tab re-import opens nothing', second['tabs'])

        library = api('/api/setup/imported')
        check(len(library['bookmarks']) == 3, 'no duplicate bookmarks', library['bookmarks'])
        tree = library['bookmark_tree']['Default']['roots']
        check([n['title'] for n in tree] == ['Bookmarks bar', 'Other bookmarks'], 'bookmark roots keep order; empty root dropped', tree)
        history = {h['url']: h for h in library['history']}
        check(len(history) == 2 and len(history[f'{FIX}/page/history-a']['visits']) == 3, 'visit times kept once each', library['history'])
        visits = history[f'{FIX}/page/history-a']['visits']
        check(visits == sorted(visits, reverse=True) and abs(visits[0] / 1000 - (time.time() - 3600)) < 120, 'visit times converted to Unix ms', visits)

        state = api('/api/browser/tabs')
        tabs = state['tabs']
        urls = [t['url'] for t in tabs]
        for expected in (f'{FIX}/page/session-one', f'{FIX}/page/session-two'):
            check(urls.count(expected) == 1, f'{expected} restored exactly once', urls)
        check(not any('session-closed' in u or 'session-old' in u or u.startswith('chrome:') for u in urls), 'closed windows, old entries, internal pages skipped', urls)
        restored = [t for t in tabs if 'session-' in t['url']]
        check(all(t['suspended'] for t in restored if t['id'] != state['active']) and restored[1]['title'] == 'Session two',
              'background restored tabs sleep until selected', state)

        api('/api/browser/command', {'cmd': 'navigate', 'url': 'atrium import test'})
        for _ in range(40):
            active = api('/api/browser/tabs')
            url = next((t['url'] for t in active['tabs'] if t['id'] == active['active']), '')
            if url.startswith(f'{FIX}/search'):
                break
            time.sleep(.25)
        check(url == f'{FIX}/search?q=atrium+import+test&src=atrium', 'typed searches use the imported engine', url)

        refused = client.request('/api/setup/import/cookies', {'profile': 'Default'})
        check(refused.get('consent_required') and 'live sign-in sessions' in refused.get('security_notice', ''), 'cookies need consent', refused)
        check(not api('/api/setup/import/report')['latest'].get('cookies'), 'refused cookie import records nothing')

        imported = api('/api/setup/import/cookies', {'profile': 'Default', 'consent': True})
        c = imported['cookies']
        check(c['status'] == 'imported' and 'live sign-in sessions' in imported['security_notice'], 'cookie import succeeded with notice', imported)
        check((c['imported'], c['verified'], c['found']) == (len(IMPORTABLE), len(IMPORTABLE), len(COOKIES)), 'cookies written and read back with the same attributes', c)
        check((c['skipped_expired'], c['skipped_partitioned']) == (1, 1), 'expired and partitioned cookies skipped', c)
        check(len(c['errors']) == 1 and c['errors'][0].startswith('app.import.localhost tampered:'), 'tampered value rejected', c['errors'])
        check(not any(cookie[3] in json.dumps(imported) for cookie in COOKIES), 'no cookie values in the response', imported)

        def received():
            tool('open_tab', {'url': f'http://{COOKIE_HOST}/cookies'}, timeout=120)
            # Under heavy machine load the page script can miss Chromium's 30-second reply window.
            for attempt in range(4):
                result = client.request('/api/tool', {'name': 'evaluate', 'args': {'code':
                    "return { received: JSON.parse(document.querySelector('#received').textContent), page: document.cookie }"}}, timeout=120)
                if not result.get('isError'):
                    return json.loads('\n'.join(item.get('text', '') for item in result['content']))
                time.sleep(2)
            raise AssertionError(f'Could not read the cookie fixture: {result}')

        seen = received()
        header = dict(p.split('=', 1) for p in seen['received']['cookie'].split('; ') if p)
        sent = {'host_only': 'v-host', 'domain_wide': 'v-domain', 'http_only': 'v-http', 'secure_one': 'v-secure', 'legacy_plain': 'v-plain'}
        check(header == sent, 'fixture received exactly the matching cookies', seen)
        check('http_only' not in seen['page'] and 'host_only=v-host' in seen['page'], 'HttpOnly cookie hidden from page script', seen)

        again = api('/api/setup/import/cookies', {'profile': 'Default', 'consent': True})['cookies']
        check(again['imported'] == len(IMPORTABLE) and again['verified'] == len(IMPORTABLE), 'cookie re-import replaces', again)
        header_again = received()['received']['cookie'].split('; ')
        check(sorted(header_again) == sorted(f'{k}={v}' for k, v in sent.items()), 'no duplicate cookies after re-import', header_again)

        only = api('/api/setup/import/cookies', {'profile': 'Default', 'consent': True, 'domains': ['unrelated.invalid']})['cookies']
        check((only['imported'], only['domains']) == (1, ['unrelated.invalid']), 'domain filter', only)

        report = api('/api/setup/import/report')
        check(len(report['runs']) == 5, 'every run recorded', [run['results'].keys() for run in report['runs']])
        check(set(report['latest']) == {'bookmarks', 'history', 'tabs', 'search_engine', 'cookies'}, 'latest result per kind', report['latest'])
        check(report['latest']['cookies']['imported'] == 1 and report['search_engine']['name'] == 'Fixture Search', 'report reflects newest runs', report['latest'])
        print('PASS: bookmarks (folders), history (visit times), session tabs, search engine, cookies (attributes, '
              'expiry, HttpOnly, Cookie header), idempotent re-runs, and the import report', flush=True)
    finally:
        try:
            client.request('/api/browser/command', {'cmd': 'quit'}, timeout=5)
        except Exception:
            pass
        try:
            browser.wait(timeout=15)
        except subprocess.TimeoutExpired:
            browser.kill()
        # Chromium helpers can outlive the app; only processes using this test's profile match.
        subprocess.run(['pkill', '-f', str(data)], check=False)
        watchdog.wait(timeout=5)
        fixture.terminate()
        fixture.wait(timeout=5)
        watchdog_log.close()
        frontmost = 'FRONTMOST' in (work / 'watchdog.log').read_text()
        if frontmost:
            raise AssertionError('The test instance became frontmost and was killed')


with tempfile.TemporaryDirectory(prefix='atrium-import-', dir='/private/tmp') as directory:
    main(Path(directory))
