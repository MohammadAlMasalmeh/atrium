#!/usr/bin/env python3
"""Restart an isolated bundled browser and check storage and lazy tab restoration."""
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
parser.add_argument('--port', type=int, default=7784)
parser.add_argument('--fixture', default='http://127.0.0.1:7789')
args = parser.parse_args()

def wait_ready():
    for _ in range(120):
        try:
            memory = api('/api/browser/memory')
            tabs = api('/api/browser/tabs')
            if memory.get('shell_ready') and not any(t['loading'] for t in tabs['tabs']):
                return memory, tabs
        except (urllib.error.URLError, TimeoutError):
            pass
        time.sleep(.5)
    raise AssertionError('Browser did not become ready')

def stop(process):
    try:
        api('/api/browser/command', {'cmd': 'quit'})
    except (urllib.error.URLError, TimeoutError):
        pass
    try:
        process.wait(timeout=15)
    except subprocess.TimeoutExpired:
        process.terminate()
        process.wait(timeout=5)
        raise AssertionError('Browser did not shut down cleanly')

with tempfile.TemporaryDirectory(prefix='atrium-persistence-', dir='/private/tmp') as directory:
    profile = Path(directory)
    client = atrium_api.Client(f'http://127.0.0.1:{args.port}', data_dir=profile)
    api, tool = client.api, client.tool
    with (profile / 'launch.log').open('w') as log:
        command = [str(args.app.resolve()), 'run', '--data-dir', str(profile), '--port', str(args.port),
                   '--opencode-url', 'http://127.0.0.1:1']
        launch = lambda: subprocess.Popen(command, stdout=log, stderr=log, env=atrium_api.instance_env())
        process = launch()
        try:
            wait_ready()
            tool('open_tab', {'url': f'{args.fixture}/page/persistence-one'})
            tool('open_tab', {'url': f'{args.fixture}/page/persistence-two'})
            tabs = api('/api/browser/tabs')
            api('/api/browser/command', {'cmd': 'activate', 'id': tabs['tabs'][-1]['id']})
            tool('evaluate', {'code': "document.cookie = 'atrium_fixture=persisted; Max-Age=86400; Path=/; SameSite=Lax'; localStorage.setItem('atrium_fixture', 'persisted'); return document.cookie;"})
            stop(process)
            assert not (profile / 'api-token').exists(), 'The API token must be removed on exit'
            process = launch()
            memory, tabs = wait_ready()
            assert len(tabs['tabs']) == 2, tabs
            assert sum(t['suspended'] for t in tabs['tabs']) == 1, tabs
            assert memory['live_renderers'] == 1, memory  # only the active page; the shell is WebKit
            index = next(i for i, tab in enumerate(tabs['tabs']) if tab['id'] == tabs['active'])
            tool('switch_tab', {'index': index})
            result = tool('evaluate', {'code': "return { cookie: document.cookie.includes('atrium_fixture=persisted'), storage: localStorage.getItem('atrium_fixture') };"})
            text = '\n'.join(item.get('text', '') for item in result)
            decoded = json.loads(text)
            assert decoded == {'cookie': True, 'storage': 'persisted'}, result
            print('PASS: cookies, localStorage, saved tabs, lazy restart, and clean shutdown', flush=True)
        finally:
            if process.poll() is None:
                stop(process)
