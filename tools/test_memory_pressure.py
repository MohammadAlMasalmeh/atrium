#!/usr/bin/env python3
"""Check that memory pressure shrinks the live-tab budget.

Start the instance with ATRIUM_TEST_MEMORY_PRESSURE=1 (and ATRIUM_MOCK_KEYCHAIN=1) to
simulate pressure; without it, pass --expect-normal. Five tabs fit the normal budget
of six, so only under pressure should background pages go to sleep, down to two.
"""
import argparse
import time
import urllib.error
import atrium_api

parser = argparse.ArgumentParser(description=__doc__)
atrium_api.add_arguments(parser)
parser.add_argument('--fixture', default='http://127.0.0.1:7789')
parser.add_argument('--expect-normal', action='store_true')
args = parser.parse_args()
client = atrium_api.Client.from_args(args)

for attempt in range(100):
    try:
        if client.api('/api/browser/memory').get('shell_ready'):
            break
    except urllib.error.URLError:
        if attempt == 99:
            raise
    time.sleep(.2)

for i in range(5):
    client.command(cmd='newTab', url=f'{args.fixture}/page/pressure-{i}', background=False)
    time.sleep(1)


def pages():
    return [t for t in client.api('/api/browser/tabs')['tabs'] if '/page/pressure-' in t['url']]


# Pages must sit idle for the policy's 30 s minimum; maintenance runs every 10 s.
deadline = time.monotonic() + 75
while time.monotonic() < deadline:
    live = [t for t in pages() if not t['suspended']]
    print('live pages', len(live), flush=True)
    if not args.expect_normal and len(live) <= 3:
        break
    time.sleep(5)

live = [t for t in pages() if not t['suspended']]
if args.expect_normal:
    assert len(live) == 5, f'pages slept without memory pressure: {pages()}'
    print('PASS: without memory pressure, five tabs stay live')
else:
    # The selected page plus at most two background pages.
    assert len(live) <= 3, f'memory pressure did not shrink the budget: {pages()}'
    print('PASS: under memory pressure, background tabs sleep down to two')
