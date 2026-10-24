#!/usr/bin/env python3
"""Check that a window.open popup keeps its opener, can message it, and closes back to it."""
import argparse
import time
import urllib.error
import atrium_api

parser = argparse.ArgumentParser(description=__doc__)
atrium_api.add_arguments(parser)
parser.add_argument('--fixture', default='http://127.0.0.1:7789')
args = parser.parse_args()
client = atrium_api.Client.from_args(args)
api, command = client.api, client.command


def tabs():
    return api('/api/browser/tabs')


def until(predicate, what, seconds=20):
    for _ in range(seconds * 5):
        state = tabs()
        if predicate(state):
            return state
        time.sleep(.2)
    raise AssertionError(f'{what}: {state}')


for attempt in range(100):
    try:
        if api('/api/browser/memory').get('shell_ready'):
            break
    except urllib.error.URLError:
        pass
    time.sleep(.2)
else:
    raise AssertionError('Browser did not become ready')

before = {t['id'] for t in tabs()['tabs']}
command(cmd='newTab', url=args.fixture + '/opener/oauth', background=False)
opener = until(lambda s: any(t['id'] not in before and not t['loading'] for t in s['tabs']), 'Opener did not load')
opener = next(t['id'] for t in opener['tabs'] if t['id'] not in before)
listed = [t for t in tabs()['tabs'] if t['kind'] != 'scratch']
client.tool('switch_tab', {'index': next(i for i, t in enumerate(listed) if t['id'] == opener)})
client.tool('evaluate', {'code': "window.open(location.origin + '/popup/oauth', 'signin', 'popup,width=420,height=560'); return true;"})
state = until(lambda s: any('/popup/' in t['url'] for t in s['tabs']), 'Popup did not open as a tab')
popup = next(t for t in state['tabs'] if '/popup/' in t['url'])
assert state['active'] == popup['id'], state
print('popup opened as the active tab:', popup['id'], flush=True)
state = until(lambda s: all('/popup/' not in t['url'] for t in s['tabs']), 'Popup did not close itself')
assert state['active'] == opener, f'Closing a popup must return to its opener: {state}'
title = next(t['title'] for t in state['tabs'] if t['id'] == opener)
assert title == 'Opener got signed-in', title
print('popup kept window.opener, messaged it, closed itself, and returned to the opener', flush=True)
client.tool('evaluate', {'code': "const a = Object.assign(document.createElement('a'), {href: '/page/blank-target', target: '_blank'}); document.body.append(a); a.click(); return true;"})
state = until(lambda s: any(t['url'].endswith('/page/blank-target') and not t['loading'] for t in s['tabs']), 'target=_blank link did not open a tab')
assert next(t for t in state['tabs'] if t['url'].endswith('/page/blank-target'))['id'] == state['active'], state
print('PASS: window.open popups keep their opener; target=_blank links open foreground tabs', flush=True)
