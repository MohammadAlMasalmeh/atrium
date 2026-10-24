#!/usr/bin/env python3
"""Check the deny-by-default site permission policy and the per-origin decision API.

Only grants window management, which has no macOS system permission, so the
test never triggers a camera, microphone, location, or notification prompt.
"""
import argparse
import json
import time
import urllib.error
import atrium_api

parser = argparse.ArgumentParser(description=__doc__)
atrium_api.add_arguments(parser)
parser.add_argument('--fixture', default='http://127.0.0.1:7789')
args = parser.parse_args()
client = atrium_api.Client.from_args(args)
api = client.api
origin = args.fixture.rstrip('/')


def evaluate(code):
    return json.loads('\n'.join(c.get('text', '') for c in client.tool('evaluate', {'code': code})))


def decide(permission, decision):
    return api('/api/browser/permissions', {'origin': origin, 'permission': permission, 'decision': decision})


SCREENS = "try { return (await getScreenDetails()).screens.length > 0 ? 'granted' : 'empty'; } catch (e) { return e.name; }"

for attempt in range(100):
    try:
        if api('/api/browser/memory').get('shell_ready'):
            break
    except urllib.error.URLError:
        pass
    time.sleep(.2)
else:
    raise AssertionError('Browser did not become ready')

before = {t['id'] for t in api('/api/browser/tabs')['tabs']}
api('/api/browser/command', {'cmd': 'newTab', 'url': origin + '/page/permissions', 'background': False})
listed = [t for t in api('/api/browser/tabs')['tabs'] if t['kind'] != 'scratch']
client.tool('switch_tab', {'index': next(i for i, t in enumerate(listed) if t['id'] not in before)})
client.tool('navigate', {'url': 'reload'})
assert evaluate('return await Notification.requestPermission();') == 'denied'
assert evaluate(SCREENS) == 'NotAllowedError'
recent = api('/api/browser/permissions')['recent']
assert any(r['origin'] == origin and r['permissions'] == ['notifications'] and r['granted'] == [] for r in recent), recent
print('unknown origins are denied and the requests are listed', flush=True)

state = decide('window-management', 'allow')
assert state['decisions'][origin] == {'window-management': 'allow'}, state
client.tool('navigate', {'url': 'reload'})
assert evaluate(SCREENS) == 'granted'
assert evaluate('return await Notification.requestPermission();') == 'denied', 'Allowing one permission must not allow others'
print('an allowed permission is granted for that origin only', flush=True)

state = decide('window-management', None)
assert origin not in state['decisions'], state
client.tool('navigate', {'url': 'reload'})
assert evaluate(SCREENS) == 'NotAllowedError'
for bad in [{'origin': origin + '/path', 'permission': 'camera', 'decision': 'allow'},
            {'origin': 'file:///tmp', 'permission': 'camera', 'decision': 'allow'},
            {'origin': origin, 'permission': 'everything', 'decision': 'allow'}]:
    assert 'error' in client.request('/api/browser/permissions', bad), bad
print('PASS: deny by default, per-origin allow, forget, and input validation', flush=True)
