#!/usr/bin/env python3
"""End-to-end password manager and passkey test against an isolated background instance.

Starts its own fixture server and browser (ATRIUM_MOCK_KEYCHAIN=1, ATRIUM_BACKGROUND_WINDOW=1,
fresh data directory) and covers: detection in an isolated world, save on submit, explicit
fill on revisit, no fill on another port or host, update, never, password generation,
import from a synthetic Chrome profile with an injected key, management API, and WebAuthn
registration and sign-in with a CDP virtual authenticator. Exits non-zero on failure.
"""
import argparse
import hashlib
import json
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.error
from pathlib import Path

import atrium_api

ROOT = Path(__file__).resolve().parent
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--app', type=Path, default=ROOT.parent / 'target/bundle/atrium.app/Contents/MacOS/atrium')
parser.add_argument('--port', type=int, default=7830)
parser.add_argument('--fixture-ports', type=int, nargs=2, default=[7831, 7832])
parser.add_argument('--watchdog', type=Path, default=ROOT.parent.parent / 'watchdog.sh')
args = parser.parse_args()

A = f'http://127.0.0.1:{args.fixture_ports[0]}'
B = f'http://127.0.0.1:{args.fixture_ports[1]}'
HOST = f'http://localhost:{args.fixture_ports[0]}'


def wait(what, check, timeout=15, interval=.25):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        try:
            last = check()
            if last:
                return last
        except (urllib.error.URLError, TimeoutError, AssertionError) as e:
            last = e
        time.sleep(interval)
    raise AssertionError(f'Timed out waiting for {what}: {last!r}')


def stays(what, check, seconds=2.0):
    end = time.time() + seconds
    while time.time() < end:
        assert check(), what
        time.sleep(.25)


def chrome_profile(directory, secret):
    """A synthetic Chrome profile: Login Data encrypted like Chrome on macOS (v10, AES-128-CBC)."""
    key = hashlib.pbkdf2_hmac('sha1', secret.encode(), b'saltysalt', 1003, 16)
    def encrypt(text):
        out = subprocess.run(['/usr/bin/openssl', 'enc', '-aes-128-cbc', '-K', key.hex(), '-iv', '20' * 16],
                             input=text.encode(), capture_output=True, check=True).stdout
        return b'v10' + out
    profile = directory / 'Synthetic Profile'
    profile.mkdir()
    db = sqlite3.connect(profile / 'Login Data')
    db.execute('CREATE TABLE logins (origin_url TEXT, action_url TEXT, username_element TEXT, username_value TEXT, '
               'password_element TEXT, password_value BLOB, signon_realm TEXT, blacklisted_by_user INTEGER)')
    rows = [
        (f'{A}/login', 'imported@example.test', encrypt('imported-pass-1'), 0),
        ('https://example.org/signin', 'org-user', encrypt('imported-pass-2'), 0),
        ('https://blocked.example/', '', b'', 1),
    ]
    for url, user, value, never in rows:
        db.execute('INSERT INTO logins VALUES (?, ?, ?, ?, ?, ?, ?, ?)', (url, url, 'u', user, 'p', value, url, never))
    db.commit()
    db.close()
    return profile


with tempfile.TemporaryDirectory(prefix='atrium-passwords-', dir='/private/tmp') as directory:
    data = Path(directory)
    client = atrium_api.Client(f'http://127.0.0.1:{args.port}', data_dir=data / 'profile')
    api, tool = client.api, client.tool
    log = (data / 'browser.log').open('w')
    fixture = subprocess.Popen([sys.executable, str(ROOT / 'password_fixture.py'), '--port', str(args.fixture_ports[0]),
                                '--port', str(args.fixture_ports[1])], stdout=subprocess.DEVNULL)
    browser = subprocess.Popen([str(args.app.resolve()), '--data-dir', str(data / 'profile'), 'run', '--port', str(args.port),
                                '--opencode-url', 'http://127.0.0.1:1'], stdout=log, stderr=log, env=atrium_api.instance_env())
    watchdog = subprocess.Popen(['/bin/sh', str(args.watchdog), str(browser.pid)], stdout=(data / 'watchdog.log').open('w'))
    step = 'startup'

    def offers():
        return api('/api/passwords/offers')

    def forms_of(tab):
        return offers()['forms'].get(str(tab))

    def offer_of(tab):
        return offers()['offers'].get(str(tab))

    def page(code):
        result = tool('evaluate', {'code': code})
        return json.loads('\n'.join(item.get('text', '') for item in result))

    def go(url):
        tool('navigate', {'url': url})
        wait(f'{url} to load', lambda: page('return location.href') == url and page('return document.readyState') == 'complete')

    def logins():
        return api('/api/passwords')['logins']

    def submit(form, username, password, extra=''):
        page(f"document.querySelector('#username').value = {json.dumps(username)}; "
             f"document.querySelector('#password, #new').value = {json.dumps(password)}; {extra} "
             f"setTimeout(() => document.querySelector('#{form}').requestSubmit(), 50); return true")

    try:
        wait('the browser', lambda: api('/api/browser/memory').get('shell_ready'), timeout=60)
        assert api('/api/passwords')['store'] == 'test-vault'

        step = 'detect a login form in an isolated world'
        tool('open_tab', {'url': f'{A}/login'})
        tab = wait('the tab', lambda: next((t['id'] for t in api('/api/browser/tabs')['tabs'] if t['url'].startswith(A)), None))
        client.command(cmd='activate', id=tab)
        assert wait('login form', lambda: forms_of(tab)) == {'origin': A, 'login': True, 'signup': False}
        assert offer_of(tab) is None, 'Nothing is saved yet, so nothing should be offered'
        assert page("return [typeof window.__atriumPw, typeof window.__atriumPasswords]") == ['undefined', 'undefined'], \
            'The password script and binding must not be visible to page JavaScript'

        step = 'offer to save on submit'
        submit('login', 'ada@example.test', 'correct horse 1')
        offer = wait('save offer', lambda: offer_of(tab))
        assert offer == {'kind': 'save', 'origin': A, 'username': 'ada@example.test'}, offer
        client.command(cmd='passwordSave', tab=tab, action='save')
        saved = wait('saved login', lambda: [l for l in logins() if l['origin'] == A])
        assert len(saved) == 1 and saved[0]['username'] == 'ada@example.test' and 'password' not in saved[0], saved
        ada = saved[0]['id']
        assert api('/api/passwords/reveal', {'id': ada}) == {'password': 'correct horse 1'}
        assert offer_of(tab) is None

        step = 'fill only on explicit request when revisiting the same origin'
        go(f'{A}/login')
        offer = wait('fill offer', lambda: offer_of(tab))
        assert offer == {'kind': 'fill', 'origin': A, 'accounts': [{'id': ada, 'username': 'ada@example.test'}]}, offer
        assert page("return document.querySelector('#password').value") == '', 'Nothing may be filled before the user asks'
        client.command(cmd='passwordFill', tab=tab, id=ada)
        wait('filled fields', lambda: page("return [document.querySelector('#username').value, document.querySelector('#password').value]")
             == ['ada@example.test', 'correct horse 1'])

        step = 'no fill on another port or host'
        for other in (B, HOST):
            go(f'{other}/login')
            wait(f'login form on {other}', lambda: (forms_of(tab) or {}).get('origin') == other)
            stays(f'no fill offer on {other}', lambda: offer_of(tab) is None, 1.5)
            client.command(cmd='passwordFill', tab=tab, id=ada)
            client.command(cmd='passwordFill', tab=tab)
            stays(f'nothing filled on {other}', lambda: page("return document.querySelector('#password').value") == '', 2)

        step = 'never save for a site'
        go(f'{B}/login')
        wait('login form', lambda: (forms_of(tab) or {}).get('origin') == B)
        submit('login', 'bob', 'bob-pass')
        assert wait('save offer', lambda: offer_of(tab))['kind'] == 'save'
        client.command(cmd='passwordSave', tab=tab, action='never')
        wait('never list', lambda: B in api('/api/passwords')['never'])
        go(f'{B}/login')
        wait('login form', lambda: (forms_of(tab) or {}).get('origin') == B)
        submit('login', 'bob', 'bob-pass')
        stays('no offer for a never site', lambda: (offer_of(tab) or {}).get('kind') != 'save', 2.5)
        assert not [l for l in logins() if l['origin'] == B]

        step = 'unchanged submit is silent, changed password offers update'
        go(f'{A}/login')
        wait('fill offer', lambda: (offer_of(tab) or {}).get('kind') == 'fill')
        submit('login', 'ada@example.test', 'correct horse 1')
        stays('no offer for an unchanged password', lambda: (offer_of(tab) or {}).get('kind') not in ('save', 'update'), 2.5)
        go(f'{A}/login')
        wait('fill offer', lambda: (offer_of(tab) or {}).get('kind') == 'fill')
        submit('login', 'ada@example.test', 'correct horse 2')
        assert wait('update offer', lambda: (offer_of(tab) or {}).get('kind') == 'update' and offer_of(tab))['username'] == 'ada@example.test'
        client.command(cmd='passwordSave', tab=tab, action='save')
        wait('updated password', lambda: api('/api/passwords/reveal', {'id': ada}) == {'password': 'correct horse 2'})

        step = 'formless single-page login is captured'
        go(f'{A}/spa-login')
        wait('spa form', lambda: (forms_of(tab) or {}).get('login'))
        page("document.querySelector('#username').value = 'spa-user'; document.querySelector('#password').value = 'spa-pass'; "
             "document.querySelector('#go').click(); return true")
        offer = wait('save offer', lambda: (offer_of(tab) or {}).get('kind') == 'save' and offer_of(tab))
        assert offer['username'] == 'spa-user', offer
        client.command(cmd='passwordSave', tab=tab, action='dismiss')
        wait('dismissed', lambda: offer_of(tab) is None)
        assert not [l for l in logins() if l['username'] == 'spa-user']

        step = 'generate a password for a signup form'
        go(f'{A}/signup')
        assert wait('signup form', lambda: forms_of(tab)) == {'origin': A, 'login': False, 'signup': True}
        assert wait('generate offer', lambda: offer_of(tab)) == {'kind': 'generate', 'origin': A}
        client.command(cmd='passwordGenerate', tab=tab)
        generated = wait('generated password', lambda: (lambda v: v[0] if v[0] and v[0] == v[1] else None)(
            page("return [document.querySelector('#new').value, document.querySelector('#confirm').value]")))
        assert len(generated) == 20, generated
        page("document.querySelector('#username').value = 'grace@example.test'; "
             "setTimeout(() => document.querySelector('#signup').requestSubmit(), 50); return true")
        offer = wait('save offer', lambda: offer_of(tab))
        assert offer == {'kind': 'save', 'origin': A, 'username': 'grace@example.test'}, offer
        client.command(cmd='passwordSave', tab=tab, action='save')
        grace = wait('generated login saved', lambda: next((l['id'] for l in logins() if l['username'] == 'grace@example.test'), None))
        assert api('/api/passwords/reveal', {'id': grace}) == {'password': generated}

        step = 'import from a synthetic Chrome profile'
        profile = chrome_profile(data, 'synthetic-safe-storage-secret')
        before = (profile / 'Login Data').read_bytes()
        rejected = client.request('/api/passwords/import-chrome', {'profile_path': str(profile), 'safe_storage': 'wrong'})
        assert rejected.get('added') == 0 and rejected.get('skipped') == 2, rejected
        summary = api('/api/passwords/import-chrome', {'profile_path': str(profile), 'safe_storage': 'synthetic-safe-storage-secret'})
        assert summary == {'added': 2, 'updated': 0, 'unchanged': 0, 'never': 1, 'skipped': 0}, summary
        assert (profile / 'Login Data').read_bytes() == before, 'Import must not modify the Chrome profile'
        imported = {l['username']: l for l in logins()}
        assert imported['imported@example.test']['origin'] == A and imported['org-user']['origin'] == 'https://example.org'
        assert api('/api/passwords/reveal', {'id': imported['org-user']['id']}) == {'password': 'imported-pass-2'}
        assert 'https://blocked.example' in api('/api/passwords')['never']
        go(f'{A}/login')
        offer = wait('fill offer with both accounts', lambda: (lambda o: o if o and len(o.get('accounts', [])) == 3 else None)(offer_of(tab)))
        assert {a['username'] for a in offer['accounts']} == {'ada@example.test', 'grace@example.test', 'imported@example.test'}
        client.command(cmd='passwordFill', tab=tab, id=imported['imported@example.test']['id'])
        wait('imported login filled', lambda: page("return document.querySelector('#password').value") == 'imported-pass-1')

        step = 'management API'
        csv = api('/api/passwords/export', {})['csv']
        assert csv.splitlines()[0] == 'name,url,username,password,note' and '"correct horse 2"' in csv and generated in csv
        edited = api('/api/passwords/edit', {'id': imported['org-user']['id'], 'username': 'org-renamed', 'password': 'new-org-pass'})['login']
        assert edited['username'] == 'org-renamed' and 'password' not in edited
        assert api('/api/passwords/reveal', {'id': edited['id']}) == {'password': 'new-org-pass'}
        api('/api/passwords/delete', {'id': edited['id']})
        assert 'error' in client.request('/api/passwords/reveal', {'id': edited['id']})
        api('/api/passwords/never', {'origin': B, 'never': False})
        assert B not in api('/api/passwords')['never']
        assert 'error' in client.request('/api/debug/cdp', {'method': 'Runtime.evaluate', 'params': {'expression': '1'}})

        step = 'passkey registration and sign-in with a virtual authenticator'
        go(f'{HOST}/passkey')
        caps = page('return await passkey.capabilities()')
        assert caps['publicKeyCredential'] and caps['secureContext'], caps
        cdp = lambda method, params=None: api('/api/debug/cdp', {'tab': tab, 'method': method, 'params': params or {}})['result']
        cdp('WebAuthn.enable', {'enableUI': False})
        authenticator = cdp('WebAuthn.addVirtualAuthenticator', {'options': {
            'protocol': 'ctap2', 'transport': 'internal', 'hasResidentKey': True, 'hasUserVerification': True,
            'isUserVerified': True, 'automaticPresenceSimulation': True}})['authenticatorId']
        registered = page('return await passkey.register()')
        assert registered['type'] == 'webauthn.create' and registered['origin'] == HOST and registered['challengeOk'] and registered['alg'] == -7, registered
        credentials = cdp('WebAuthn.getCredentials', {'authenticatorId': authenticator})['credentials']
        assert len(credentials) == 1 and credentials[0]['rpId'] == 'localhost' and credentials[0]['isResidentCredential'], credentials
        signed_in = page('return await passkey.login()')
        assert signed_in == {'id': registered['id'], 'sameCredential': True, 'type': 'webauthn.get', 'origin': HOST, 'challengeOk': True,
                             'signatureValid': True, 'userHandle': 'user-ada', 'userVerified': True}, signed_in
        assert cdp('WebAuthn.getCredentials', {'authenticatorId': authenticator})['credentials'][0]['signCount'] >= 1
        cdp('WebAuthn.removeVirtualAuthenticator', {'authenticatorId': authenticator})
        cdp('WebAuthn.disable')
        print(f'passkey capabilities without signing: {json.dumps(caps)}', flush=True)

        step = 'shutdown'
        client.command(cmd='quit')
        browser.wait(timeout=20)
        vault = (data / 'profile' / 'passwords-test.vault').read_bytes()
        for secret in (b'correct horse 2', generated.encode(), b'imported-pass-1'):
            assert secret not in vault, 'The test vault must be encrypted'
        assert 'FRONTMOST' not in (data / 'watchdog.log').read_text()
        print('PASS: isolated detection, save, explicit fill, origin and port isolation, never, update, SPA capture, '
              'generation, Chrome import, management API, and virtual-authenticator passkeys', flush=True)
    except BaseException:
        print(f'FAIL during: {step}', file=sys.stderr, flush=True)
        log.flush()
        print((data / 'browser.log').read_text()[-4000:], file=sys.stderr)
        raise
    finally:
        if browser.poll() is None:
            try:
                client.command(cmd='quit')
                browser.wait(timeout=15)
            except Exception:
                browser.terminate()
                browser.wait(timeout=5)
        watchdog.wait(timeout=5)
        fixture.terminate()
        fixture.wait(timeout=5)
