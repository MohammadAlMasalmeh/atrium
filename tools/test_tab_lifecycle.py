#!/usr/bin/env python3
"""Integration test for renderer eviction, safe wake, and draft protection."""
import json
import time
import urllib.request
BASE='http://127.0.0.1:7782'
def api(path,data=None):
    req=urllib.request.Request(BASE+path,data=None if data is None else json.dumps(data).encode(),headers={'Content-Type':'application/json'})
    with urllib.request.urlopen(req,timeout=10) as r:return json.load(r)
def command(**data):return api('/api/browser/command',data)
for attempt in range(100):
    try:
        if api('/api/browser/memory').get('shell_ready'):break
    except urllib.error.URLError:pass
    time.sleep(.2)
else:raise AssertionError('Browser did not become ready')
before_ids={t['id'] for t in api('/api/browser/tabs')['tabs']}
command(cmd='newTab',url='http://127.0.0.1:7789/page/foreground-control',background=False)
command(cmd='newTab',url='http://127.0.0.1:7789/draft/protected',background=True)
for i in range(9):command(cmd='newTab',url=f'http://127.0.0.1:7789/page/budget-{i}',background=True)
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
result=api('/api/tool',{'name':'switch_tab','args':{'index':index}})
assert not result.get('isError'),result
result=api('/api/tool',{'name':'evaluate','args':{'code':'return document.title;'}})
assert not result.get('isError'),result
assert 'Fixture' in str(result),result
wake=sleeping[-1]['id']
command(cmd='activate',id=wake)
for _ in range(30):
    tab=next(t for t in api('/api/browser/tabs')['tabs'] if t['id']==wake)
    if not tab['suspended'] and not tab['loading']:break
    time.sleep(.2)
assert not tab['suspended'] and not tab['loading'],tab
print('PASS: idle renderers release; drafts stay; agent and selected sleeping tabs wake',flush=True)
