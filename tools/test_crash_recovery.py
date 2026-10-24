#!/usr/bin/env python3
"""Kill tab renderers with chrome://kill and check that the browser survives and tabs recover."""
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


def tab(id):
    return next(t for t in tabs()['tabs'] if t['id'] == id)


def wait(id, predicate, what, seconds=20):
    for _ in range(seconds * 5):
        current = tab(id)
        if predicate(current):
            return current
        time.sleep(.2)
    raise AssertionError(f'{what}: {current}')


def agent_tab(id):
    listed = [t for t in tabs()['tabs'] if t['kind'] != 'scratch']
    client.tool('switch_tab', {'index': next(i for i, t in enumerate(listed) if t['id'] == id)})


def kill_renderer(id):
    agent_tab(id)
    # The tool reports the crash as an error; the browser itself must keep running.
    client.request('/api/tool', {'name': 'navigate', 'args': {'url': 'chrome://kill'}})
    return wait(id, lambda t: t['crashed'], 'Tab was not marked crashed')


def open_tab(path, background):
    before = {t['id'] for t in tabs()['tabs']}
    command(cmd='newTab', url=args.fixture + path, background=background)
    id = next(t['id'] for t in tabs()['tabs'] if t['id'] not in before)
    return wait(id, lambda t: not t['loading'] and t['title'].startswith('Fixture'), 'Tab did not load')['id']


for attempt in range(100):
    try:
        if api('/api/browser/memory').get('shell_ready'):
            break
    except urllib.error.URLError:
        pass
    time.sleep(.2)
else:
    raise AssertionError('Browser did not become ready')

visible = open_tab('/page/crash-visible', background=False)
crashed = kill_renderer(visible)
assert crashed['url'].endswith('/page/crash-visible'), crashed
assert api('/api/browser/memory')['shell_ready'], 'Browser must stay alive after a renderer crash'
result = client.request('/api/tool', {'name': 'evaluate', 'args': {'code': 'return 1'}})
assert result['isError'] and 'crashed' in str(result), result
print('visible crash: marked crashed, browser alive, agent told to recover', flush=True)
open_tab('/page/crash-other', background=False)
assert tab(visible)['crashed'], 'A crashed tab stays crashed until it is selected'
command(cmd='activate', id=visible)
wait(visible, lambda t: not t['crashed'] and not t['loading'] and t['title'].startswith('Fixture'), 'Selected tab did not reload')
print('visible crash: reloaded when selected', flush=True)

background = open_tab('/page/crash-background', background=True)
crashed = kill_renderer(background)
wait(background, lambda t: t['suspended'], 'Crashed background tab kept its view')
command(cmd='activate', id=background)
wait(background, lambda t: not t['crashed'] and not t['suspended'] and not t['loading'], 'Background tab did not recover')
agent_tab(background)
assert 'Fixture' in str(client.tool('evaluate', {'code': 'return document.title'}))
print('PASS: crashed tabs are reported, released in the background, and reload when selected', flush=True)
