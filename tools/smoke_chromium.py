#!/usr/bin/env python3
"""Exercise a running isolated Chromium build against fixture_server.py."""
import argparse
import json
import time
import urllib.request

parser = argparse.ArgumentParser()
parser.add_argument('--api', default='http://127.0.0.1:7782')
parser.add_argument('--fixture', default='http://127.0.0.1:7789')
parser.add_argument('--expect-extension', action='store_true')
args = parser.parse_args()


def request(path, data=None):
    body = None if data is None else json.dumps(data).encode()
    req = urllib.request.Request(args.api + path, data=body, headers={'Content-Type': 'application/json'})
    with urllib.request.urlopen(req, timeout=35) as response:
        result = json.load(response)
    assert 'error' not in result, result
    return result


def tool(name, arguments):
    result = request('/api/tool', {'name': name, 'args': arguments})
    assert not result.get('isError'), result
    return result['content']


for attempt in range(50):
    try:
        memory = request('/api/browser/memory')
        break
    except urllib.error.URLError:
        if attempt == 49: raise
        time.sleep(.2)
assert memory['engine'] == 'Chromium / CEF', memory
assert memory.get('shell_ready'), memory
print('engine:', memory['engine'], flush=True)
opened = tool('open_tab', {'url': args.fixture + '/page/smoke'})
print('navigation: passed', flush=True)
result = tool('evaluate', {'code': "document.querySelector('#increment').click(); return { count: document.querySelector('#count').value, node: typeof process, require: typeof require, extension: document.documentElement.dataset.asideExtension || null };"})
text = '\n'.join(c.get('text', '') for c in result)
assert '"count": "1"' in text or '"count":"1"' in text, text
assert 'undefined' in text, text
if args.expect_extension:
    assert 'loaded' in text, text
print('page evaluation, input isolation, extension:', text, flush=True)
screenshot = tool('screenshot', {})
assert any(c.get('type') == 'image' and c.get('data') for c in screenshot), screenshot
print('screenshot: passed', flush=True)
# An untrusted website cannot invoke privileged browser commands.
req = urllib.request.Request(args.api + '/api/browser/command', data=b'{"cmd":"newTab"}',
                             headers={'Content-Type': 'application/json', 'Origin': 'https://untrusted.example'})
try:
    urllib.request.urlopen(req, timeout=5)
    raise AssertionError('Cross-origin browser command was accepted')
except urllib.error.HTTPError as error:
    assert error.code == 403, error
print('origin isolation: passed', flush=True)
print('tabs:', len(request('/api/browser/tabs')['tabs']), flush=True)
