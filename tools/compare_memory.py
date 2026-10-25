#!/usr/bin/env python3
"""Compare equal local workloads in a running isolated CEF build and a fresh reference-browser profile.

Starts a separate reference-browser instance, verifies all fixture tabs through its loopback
DevTools endpoint, samples both process trees, and closes only that test instance.
No user profile or signed-in account is used.
"""
import argparse
import json
import os
import subprocess
import tempfile
import time
import urllib.request
from pathlib import Path
from memory_benchmark import process_table, tree
import statistics

parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--api',default='http://127.0.0.1:7783')
parser.add_argument('--fixture',default='http://127.0.0.1:7789')
parser.add_argument('--baseline',default=os.environ.get('BASELINE_BROWSER',''))
parser.add_argument('--tabs',type=int,default=10)
parser.add_argument('--distinct-sites',action='store_true')
parser.add_argument('--warmup',type=int,default=50)
parser.add_argument('--output',type=Path,required=True)
args=parser.parse_args()
if not args.baseline:
    raise SystemExit('pass --baseline or set BASELINE_BROWSER to the reference browser executable')
def api(path,data=None):
    req=urllib.request.Request(args.api+path,data=None if data is None else json.dumps(data).encode(),headers={'Content-Type':'application/json'})
    with urllib.request.urlopen(req,timeout=10) as r:return json.load(r)
from urllib.parse import urlparse
fixture_port=urlparse(args.fixture).port or 80
workload=[(f'http://site-{i}.localhost:{fixture_port}' if args.distinct_sites else args.fixture)+f'/page/benchmark-{i}' for i in range(args.tabs)]
for attempt in range(60):
    try:
        state=api('/api/browser/memory')
        if state.get('shell_ready'):break
    except urllib.error.URLError:
        pass
    time.sleep(.5)
else:
    raise RuntimeError('Chromium test instance did not become ready')
assert state['shell_ready'],state
assert not any(t['live'] for t in state['tabs']), 'Use a fresh, empty CEF profile for a fair comparison'
profile=Path(tempfile.mkdtemp(prefix='baseline-memory-'))
log=(profile/'launch.log').open('w')
process=subprocess.Popen([args.baseline,f'--user-data-dir={profile}','--remote-debugging-port=0','--no-first-run','--new-window',*workload],stdout=log,stderr=log)
try:
    port_file=profile/'DevToolsActivePort'
    for _ in range(60):
        if port_file.exists():break
        if process.poll() is not None:raise RuntimeError('Reference browser exited; '+str(profile/'launch.log'))
        time.sleep(.5)
    port=int(port_file.read_text().splitlines()[0])
    with urllib.request.urlopen(f'http://127.0.0.1:{port}/json/version',timeout=5) as r:baseline_version=json.load(r).get('Browser')
    for _ in range(40):
        with urllib.request.urlopen(f'http://127.0.0.1:{port}/json/list',timeout=5) as r:targets=json.load(r)
        loaded=sum(t.get('type')=='page' and t.get('url') in workload and t.get('title','').startswith('Fixture ') for t in targets)
        if loaded==args.tabs:break
        time.sleep(.5)
    assert loaded==args.tabs, f'Baseline loaded {loaded}/{args.tabs} requested pages; comparison refused'
    for url in workload:api('/api/browser/command',{'cmd':'newTab','url':url,'background':False})
    for _ in range(40):
        tabs=api('/api/browser/tabs')['tabs']
        if len([t for t in tabs if t['url'] in workload and not t['loading'] and t['title'].startswith('Fixture ')])==args.tabs:break
        time.sleep(.5)
    assert len([t for t in tabs if t['url'] in workload and not t['loading'] and t['title'].startswith('Fixture ')])==args.tabs,'CEF workload did not load'
    print(f'Both browsers loaded {args.tabs} identical local pages. Warming up {args.warmup}s.',flush=True)
    for elapsed in range(0,args.warmup,10):
        time.sleep(min(10,args.warmup-elapsed))
        current=api('/api/browser/memory')
        print('warmup',min(elapsed+10,args.warmup),'CEF live views',current['live_renderers'],flush=True)
    values={'Baseline':[],'CEF build':[]};counts={name:[] for name in values}
    for i in range(6):
        table=process_table()
        for name,pid in [('Baseline',process.pid),('CEF build',state['pid'])]:
            pids=tree(table,f'pid:{pid}')
            assert pids,f'{name} process exited'
            values[name].append(round(sum(table[p][1] for p in pids)/1024,2));counts[name].append(len(pids))
        if i<5:time.sleep(3)
    final=api('/api/browser/memory')
    report={'metric':'Aggregate descendant-process RSS in MiB. Shared memory can be counted more than once. This is not physical footprint.',
            'workload':{'pages':args.tabs,'distinct_sites':args.distinct_sites,'page_content':'Local deterministic page, 12 MiB touched buffer, button, empty textarea','extensions':0,'signed_in_accounts':0,'warmup_seconds':args.warmup,'samples':6,'sample_interval_seconds':3},
            'baseline_browser':baseline_version,'cef_version':'156.0.8','cef_live_views':final['live_renderers'],'cef_tab_budget':final['max_live_tabs'],
            'browsers':{name:{'median_mib':round(statistics.median(v),2),'samples_mib':v,'process_counts':counts[name]} for name,v in values.items()}}
    baseline=report['browsers']['Baseline']['median_mib'];candidate=report['browsers']['CEF build']['median_mib']
    report['rss_reduction_percent']=round(100*(baseline-candidate)/baseline,1)
    args.output.write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps(report,indent=2),flush=True)
finally:
    process.terminate()
    try:process.wait(timeout=8)
    except subprocess.TimeoutExpired:process.kill();process.wait(timeout=5)
    log.close()
