#!/usr/bin/env python3
"""Integration test for renderer eviction, safe wake, and draft protection."""
import argparse
import time
import urllib.error
import aside_api
parser=argparse.ArgumentParser(description=__doc__)
aside_api.add_arguments(parser,api='http://127.0.0.1:7782')
parser.add_argument('--fixture',default='http://127.0.0.1:7789')
args=parser.parse_args()
client=aside_api.Client.from_args(args)
api,command=client.api,client.command
FIXTURE=args.fixture
for attempt in range(100):
    try:
        if api('/api/browser/memory').get('shell_ready'):break
    except urllib.error.URLError:pass
    time.sleep(.2)
else:raise AssertionError('Browser did not become ready')
before_ids={t['id'] for t in api('/api/browser/tabs')['tabs']}
command(cmd='newTab',url=f'{FIXTURE}/page/foreground-control',background=False)
command(cmd='newTab',url=f'{FIXTURE}/draft/protected',background=True)
for i in range(9):command(cmd='newTab',url=f'{FIXTURE}/page/budget-{i}',background=True)
print('opened:',api('/api/browser/memory'),flush=True)
for i in range(12):
    time.sleep(5)
    state=api('/api/browser/memory')
    sleeping=[t for t in state['tabs'] if t['suspended'] and t['id'] not in before_ids]
    print('sample',i+1,'live',state['live_renderers'],'sleeping',len(sleeping),flush=True)
    if sleeping and state['live_renderers']<=state['max_live_tabs']+1:break
assert sleeping,state
all_tabs=api('/api/browser/tabs')['tabs']
draft=next(t for t in reversed(all_tabs) if '/draft/' in t['url'])
assert api('/api/browser/tabs')['active'] != draft['id'], 'Draft must be protected while in the background'
assert not draft['suspended'],draft
agent_wake=sleeping[0]['id']
index=next(i for i,t in enumerate(all_tabs) if t['id']==agent_wake)
client.tool('switch_tab',{'index':index})
result=client.tool('evaluate',{'code':'return document.title;'})
assert 'Fixture' in str(result),result
wake=sleeping[-1]['id']
command(cmd='activate',id=wake)
for _ in range(30):
    tab=next(t for t in api('/api/browser/tabs')['tabs'] if t['id']==wake)
    if not tab['suspended'] and not tab['loading']:break
    time.sleep(.2)
assert not tab['suspended'] and not tab['loading'],tab
print('PASS: idle renderers release; drafts stay; agent and selected sleeping tabs wake',flush=True)
