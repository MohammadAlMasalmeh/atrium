#!/usr/bin/env python3
"""Check that back/forward history and scroll survive renderer eviction and restart."""
import argparse
import json
import subprocess
import tempfile
import time
import urllib.error
from pathlib import Path
import atrium_api

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--app', type=Path, default=Path('target/bundle/Atrium.app/Contents/MacOS/atrium'))
parser.add_argument('--port', type=int, default=7793)
parser.add_argument('--fixture', default='http://127.0.0.1:7789')
args = parser.parse_args()
FIXTURE = args.fixture.rstrip('/')


def wait_ready():
    for _ in range(120):
        try:
            if api('/api/browser/memory').get('shell_ready'):
                return
        except (urllib.error.URLError, TimeoutError):
            pass
        time.sleep(.5)
    raise AssertionError('Browser did not become ready')


def stop(process):
    try:
        command(cmd='quit')
    except (urllib.error.URLError, TimeoutError):
        pass
    try:
        process.wait(timeout=30)
    except subprocess.TimeoutExpired:
        process.terminate()
        process.wait(timeout=5)
        raise AssertionError('Browser did not shut down cleanly')


def tab(id):
    return next(t for t in api('/api/browser/tabs')['tabs'] if t['id'] == id)


def wait(id, path, seconds=30, suspended=False):
    for _ in range(seconds * 5):
        current = tab(id)
        if current['url'] == FIXTURE + path and not current['loading'] and current['suspended'] == suspended:
            return current
        time.sleep(.2)
    raise AssertionError(f'Expected {path}: {current}')


def step(cmd, id, path):
    command(cmd=cmd)
    wait(id, path)


def scroll_y(id):
    listed = [t for t in api('/api/browser/tabs')['tabs'] if t['kind'] != 'scratch']
    tool('switch_tab', {'index': next(i for i, t in enumerate(listed) if t['id'] == id)})
    return json.loads(tool('evaluate', {'code': 'return scrollY'})[0]['text'])


with tempfile.TemporaryDirectory(prefix='atrium-history-', dir='/private/tmp') as directory:
    profile = Path(directory)
    client = atrium_api.Client(f'http://127.0.0.1:{args.port}', data_dir=profile)
    api, tool, command = client.api, client.tool, client.command
    with (profile / 'launch.log').open('w') as log:
        launch = lambda: subprocess.Popen(
            [str(args.app.resolve()), 'run', '--data-dir', str(profile), '--port', str(args.port),
             '--opencode-url', 'http://127.0.0.1:1'],
            stdout=log, stderr=log, env=atrium_api.instance_env())
        process = launch()
        try:
            wait_ready()
            command(cmd='newTab', url=FIXTURE + '/tall/one', background=False)
            id = next(t['id'] for t in api('/api/browser/tabs')['tabs'] if t['url'] == FIXTURE + '/tall/one')
            wait(id, '/tall/one')
            command(cmd='navigate', url=FIXTURE + '/tall/two')
            wait(id, '/tall/two')
            command(cmd='navigate', url=FIXTURE + '/tall/three')
            wait(id, '/tall/three')
            step('back', id, '/tall/two')

            # The agent is not used on this tab before eviction; it would protect it.
            command(cmd='newTab', url=FIXTURE + '/page/history-other', background=False)
            for i in range(9):
                command(cmd='newTab', url=f'{FIXTURE}/page/history-budget-{i}', background=True)
            wait(id, '/tall/two', seconds=90, suspended=True)
            command(cmd='activate', id=id)
            wait(id, '/tall/two')
            assert scroll_y(id) == 1500, 'Scroll offset was not restored after eviction'
            print('evicted tab woke at its scroll offset', flush=True)

            step('back', id, '/tall/one')
            step('forward', id, '/tall/two')
            step('forward', id, '/tall/three')
            step('back', id, '/tall/two')
            print('back and forward cross the evicted history in both directions', flush=True)

            stop(process)
            saved = next(t for t in json.loads((profile / 'chromium-tabs.json').read_text()) if t['active'])
            assert saved['url'] == FIXTURE + '/tall/two', saved
            assert saved['back'] == [FIXTURE + '/tall/one'] and saved['forward'] == [FIXTURE + '/tall/three'], saved

            process = launch()
            wait_ready()
            id = api('/api/browser/tabs')['active']
            wait(id, '/tall/two')
            step('forward', id, '/tall/three')
            step('back', id, '/tall/two')
            step('back', id, '/tall/one')
            print('history survived a restart', flush=True)

            command(cmd='navigate', url=FIXTURE + '/page/history-new')
            wait(id, '/page/history-new')
            listed = [t for t in api('/api/browser/tabs')['tabs'] if t['kind'] != 'scratch']
            tool('switch_tab', {'index': next(i for i, t in enumerate(listed) if t['id'] == id)})
            result = client.request('/api/tool', {'name': 'navigate', 'args': {'url': 'forward'}})
            assert result.get('isError') and 'no page to go forward' in str(result), result
            step('back', id, '/tall/one')
            print('PASS: history and scroll survive eviction and restart; a new navigation drops forward history', flush=True)
        finally:
            if process.poll() is None:
                stop(process)
