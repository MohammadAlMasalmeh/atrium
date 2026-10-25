#!/usr/bin/env python3
"""End-to-end updater test against a local release feed.

Needs a bundle built with ASIDE_UPDATE_PUBLIC_KEY set to the public half of --secret
(see docs/chromium.md). Everything runs on copies under --root in /private/tmp:

1. a release signed with another key, and a tampered archive, are rejected;
2. a correctly signed 0.2.0 is staged, installed by "restart", confirms startup, and
   keeps 0.1.0 as the previous version;
3. a signed 0.4.0 whose executable exits at once is rolled back to 0.2.0.
"""
import argparse
import json
import shutil
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import background_app

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--app', type=Path, default=Path('target/bundle/aside.app'))
parser.add_argument('--secret', type=Path, required=True, help='Ed25519 secret key matching the build')
parser.add_argument('--sign-tool', type=Path, default=Path('target/release/aside_sign_update'))
parser.add_argument('--port', type=int, default=7802)
parser.add_argument('--feed-port', type=int, default=7803)
parser.add_argument('--root', type=Path, default=Path('/private/tmp/aside-release-updater'))
args = parser.parse_args()
ARCH = 'arm64' if subprocess.run(['uname', '-m'], capture_output=True, text=True).stdout.strip() == 'arm64' else 'x86_64'
FEED = f'http://127.0.0.1:{args.feed_port}'


def step(message):
    print(f'-- {message}', flush=True)


def tool(*command):
    return background_app.run(args.sign_tool.resolve(), *command).stdout.strip()


def release(version, secret, executable=None, tamper=False):
    """Builds a fake release directory with a ditto zip and its signature."""
    out = args.root / 'releases' / f'{version}-{secret.stem}{"-tampered" if tamper else ""}'
    out.mkdir(parents=True)
    app = background_app.prepare(args.app.resolve(), out / 'stage/Aside.app', version=version, executable=executable)
    name = f'Aside-{version}-macos-{ARCH}.zip'
    archive = out / name
    background_app.run('/usr/bin/ditto', '-c', '-k', '--keepParent', app, archive)
    shutil.rmtree(out / 'stage')
    tool('sign', '--secret', secret, '--version', version, archive)
    if tamper:
        with open(archive, 'ab') as f:
            f.write(b'\0')
    return {'tag_name': f'v{version}', 'name': f'Aside {version}', 'body': f'Test release {version}', 'draft': False, 'prerelease': False,
            'dir': out, 'assets': [{'name': n, 'browser_download_url': f'{FEED}/files/{out.name}/{n}'} for n in (name, name + '.sig')]}


class Feed(BaseHTTPRequestHandler):
    current = None

    def do_GET(self):
        if self.path == '/latest' and Feed.current:
            body = json.dumps({k: v for k, v in Feed.current.items() if k != 'dir'}).encode()
            kind = 'application/json'
        elif self.path.startswith('/files/'):
            _, _, folder, name = self.path.split('/', 3)
            path = args.root / 'releases' / folder / name
            if not path.is_file():
                return self.send_error(404)
            body, kind = path.read_bytes(), 'application/octet-stream'
        else:
            return self.send_error(404)
        self.send_response(200)
        self.send_header('Content-Type', kind)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


if args.root.exists():
    shutil.rmtree(args.root)
args.root.mkdir(parents=True)
watcher = background_app.FrontWatcher(args.root)
server = ThreadingHTTPServer(('127.0.0.1', args.feed_port), Feed)
threading.Thread(target=server.serve_forever, daemon=True).start()
failed = False
instance = None
try:
    step('preparing the installed copy and fake releases')
    other_key = args.root / 'other.pk8'
    tool('keygen', '--secret', other_key)
    installed = background_app.prepare(args.app.resolve(), args.root / 'Apps/Aside.app')
    base_version = background_app.info(installed, 'CFBundleVersion')
    assert base_version == '0.1.0', base_version
    wrong_key = release('0.3.0', other_key)
    tampered = release('0.3.1', args.secret, tamper=True)
    good = release('0.2.0', args.secret)
    broken = release('0.4.0', args.secret, executable='/usr/bin/false')

    profile = args.root / 'profile'
    profile.mkdir()
    (profile / 'update.json').write_text(json.dumps({'feed': f'{FEED}/latest', 'auto': False}))
    instance = background_app.Instance(installed, profile, args.port, args.root / 'browser.log')
    instance.start()
    pid = instance.wait_ready()
    client = instance.client

    def update(path, data=None):
        result = client.request(path, data)
        assert not result.get('error'), result
        return result

    status = update('/api/update/status')
    assert status['public_key'] == tool('public', '--secret', args.secret), 'the build does not embed the test public key'
    assert status['enabled'] and status['current_version'] == '0.1.0' and status['phase'] == 'idle', status

    for bad, why in ((wrong_key, 'signed with another key'), (tampered, 'tampered after signing')):
        step(f'rejecting a release {why}')
        Feed.current = bad
        result = client.request('/api/update/apply', {})
        assert 'signature is not valid' in (result.get('error') or ''), result
        assert result['staged'] is None and result['phase'] == 'error', result
        assert not (profile / 'updates/staged').exists() or not any((profile / 'updates/staged').iterdir()), 'a rejected update was staged'

    step('checking, staging, and installing 0.2.0')
    Feed.current = good
    checked = update('/api/update/check', {})
    assert checked['available']['version'] == '0.2.0', checked
    applied = update('/api/update/apply', {})
    assert applied['staged'] == {'version': '0.2.0'} and applied['phase'] == 'staged', applied
    # Running quarantined ad hoc code would make Gatekeeper show a dialog.
    xattrs = subprocess.run(['/usr/bin/xattr', '-r', '-l', profile / 'updates/staged/Aside.app'], capture_output=True, text=True).stdout
    assert 'com.apple.quarantine' not in xattrs, 'the staged update is still quarantined'
    assert client.request('/api/update/restart', {}) == {'ok': True}
    instance.process.wait(30)
    pid = instance.wait_ready(120)
    status = update('/api/update/status')
    assert status['current_version'] == '0.2.0', status
    assert status['last']['result'] == 'updated' and status['last']['from'] == '0.1.0', status
    assert status['staged'] is None and status['rollback_available'] is True, status
    assert background_app.info(installed, 'CFBundleVersion') == '0.2.0'
    assert background_app.info(profile / 'updates/previous/Aside.app', 'CFBundleVersion') == '0.1.0'
    background_app.run('/usr/bin/codesign', '--verify', '--deep', '--strict', installed)
    watcher.check()

    step('staging 0.4.0, which cannot start, and rolling back')
    Feed.current = broken
    applied = update('/api/update/apply', {})
    assert applied['staged'] == {'version': '0.4.0'}, applied
    instance.quit(pid)
    instance.start()
    pid = instance.wait_ready(120)
    assert pid == instance.process.pid, 'after rollback the original process keeps running'
    status = update('/api/update/status')
    assert status['current_version'] == '0.2.0', status
    assert status['last']['result'] == 'rolled_back' and status['last']['version'] == '0.4.0', status
    assert 'exited during startup' in status['last']['reason'], status
    assert background_app.info(installed, 'CFBundleVersion') == '0.2.0'
    background_app.run('/usr/bin/codesign', '--verify', '--deep', '--strict', installed)
    watcher.check()
    instance.quit(pid)
    print('PASS: bad signature and tampered archive rejected; 0.1.0 -> 0.2.0 installed on restart with '
          '0.1.0 kept; failing 0.4.0 rolled back to 0.2.0', flush=True)
except Exception:
    failed = True
    raise
finally:
    if instance:
        if instance.process and instance.process.poll() is None:
            instance.process.kill()
        instance.kill_all()
    server.shutdown()
    watcher.stop()
    if watcher.violations:
        print(f'FRONTMOST VIOLATION: {watcher.violations}', file=sys.stderr)
        sys.exit(3)
    if failed:
        print(f'Logs: {args.root}/browser.log', file=sys.stderr)
