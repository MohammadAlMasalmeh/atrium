#!/usr/bin/env python3
"""Crash a renderer in a background tab and check that Crashpad wrote a local dump the API lists."""
import argparse
import shutil
import sys
import time
from pathlib import Path

import background_app

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--app', type=Path, default=Path('target/bundle/Atrium.app'))
parser.add_argument('--port', type=int, default=7804)
parser.add_argument('--fixture', default='http://127.0.0.1:7801')
parser.add_argument('--root', type=Path, default=Path('/private/tmp/atrium-release-crash'))
args = parser.parse_args()

if args.root.exists():
    shutil.rmtree(args.root)
args.root.mkdir(parents=True)
watcher = background_app.FrontWatcher(args.root)
app = background_app.prepare(args.app.resolve(), args.root / 'Apps/Atrium.app')
assert (app / 'Contents/Resources/crash_reporter.cfg').exists(), 'crash_reporter.cfg is missing from the bundle'
cfg_lines = (app / 'Contents/Resources/crash_reporter.cfg').read_text().splitlines()
assert not any(line.strip().startswith('ServerURL') for line in cfg_lines), 'crash uploads must stay disabled'
instance = background_app.Instance(app, args.root / 'profile', args.port, args.root / 'browser.log', {'ATRIUM_DEBUG_API': '1'})
failed = False
try:
    instance.start()
    pid = instance.wait_ready()
    watcher.check()
    client = instance.client
    before = client.api('/api/crash-reports')
    assert before['enabled'] is True and before['uploads'] is False, before
    assert before['reports'] == [], before
    # Page.crash through the test-only DevTools endpoint; the assistant cannot open chrome:// URLs.
    client.tool('open_tab', {'url': f'{args.fixture}/page/crash-target'})
    target = next(t['id'] for t in client.api('/api/browser/tabs')['tabs'] if t['url'].endswith('/page/crash-target'))
    client.request('/api/debug/cdp', {'tab': target, 'method': 'Page.crash', 'params': {}})
    deadline = time.time() + 45
    reports = []
    while time.time() < deadline and not reports:
        time.sleep(1)
        reports = client.api('/api/crash-reports')['reports']
    assert reports, 'no crash dump appeared within 45 seconds'
    dump = Path(reports[0]['path'])
    assert dump.is_file() and dump.stat().st_size > 0, reports
    assert str(dump.resolve()).startswith(str((args.root / 'profile').resolve())), dump
    data = dump.read_bytes()
    assert data[:4] == b'MDMP', 'the dump is not a minidump'
    assert b'ptype' in data and b'renderer' in data, 'the dump is not from a renderer process'
    tabs = client.api('/api/browser/tabs')
    assert pid == client.api('/api/browser/memory')['pid'], 'the browser process must survive a renderer crash'
    watcher.check()
    instance.quit(pid)
    print(f'PASS: renderer crash produced {dump.name} ({dump.stat().st_size} bytes) in the profile; '
          f'browser stayed up with {len(tabs["tabs"])} tabs; uploads disabled', flush=True)
except Exception:
    failed = True
    raise
finally:
    if instance.process and instance.process.poll() is None:
        instance.process.kill()
    instance.kill_all()
    watcher.stop()
    if watcher.violations:
        print(f'FRONTMOST VIOLATION: {watcher.violations}', file=sys.stderr)
        sys.exit(3)
    if failed:
        print(f'Logs: {args.root}/browser.log', file=sys.stderr)
