#!/usr/bin/env python3
"""Compare real public websites with live tabs and after idle eviction.

Requires a fresh release CEF browser on 7783 and Node with native WebSocket.
Starts only a separate temporary Aside profile. Does not use signed-in accounts.
Validation reads public DOM metadata; it intentionally gives CEF pages the normal
agent protection window, allowing a live phase before the idle eviction phase.
"""
import argparse
import json
import statistics
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path
from urllib.parse import urlparse
from memory_benchmark import process_table, tree, physical_footprint_mib
import atrium_api

ROOT = Path(__file__).resolve().parent
parser = argparse.ArgumentParser(description=__doc__)
atrium_api.add_arguments(parser, api='http://127.0.0.1:7783')
parser.add_argument('--aside', default='/Applications/Aside.app/Contents/MacOS/Aside')
parser.add_argument('--sites', type=Path, default=ROOT / 'real_sites.json')
parser.add_argument('--output', type=Path, required=True)
args = parser.parse_args()
sites = json.loads(args.sites.read_text())
EXPRESSION = """({url:location.href,title:document.title,ready:document.readyState,
visibility:document.visibilityState,viewport:[innerWidth,innerHeight,devicePixelRatio],
textLength:document.body?.innerText.length || 0, preview:document.body?.innerText.slice(0,350) || '',
nodes:document.querySelectorAll('*').length,resources:performance.getEntriesByType('resource').length,
iframes:document.querySelectorAll('iframe').length})"""

client = atrium_api.Client.from_args(args)

def api(path, data=None):
    return client.api(path, data, timeout=40)

def tool(name, arguments):
    result = api('/api/tool', {'name':name,'args':arguments})
    assert not result.get('isError'), result
    return json.loads('\n'.join(c.get('text','') for c in result['content']))

def cdp(url, method, params=None):
    result = subprocess.run(['node',str(ROOT/'cdp.mjs')], input=json.dumps({'url':url,'method':method,'params':params or {}}),
                            capture_output=True, text=True, timeout=25)
    assert result.returncode == 0, result.stderr
    return json.loads(result.stdout)

def inspect_cef(tab_id):
    api('/api/browser/command', {'cmd':'activate','id':tab_id})
    tabs = api('/api/browser/tabs')['tabs']
    index = next(i for i,t in enumerate(tabs) if t['id']==tab_id)
    # switch_tab returns prose; use the plain endpoint rather than JSON helper.
    result = api('/api/tool', {'name':'switch_tab','args':{'index':index}})
    assert not result.get('isError'), result
    last = None
    for _ in range(30):
        try:
            info = tool('evaluate', {'code':'return '+EXPRESSION+';'})
            last = info
            if info.get('ready') in ('interactive','complete') and info.get('textLength',0)>80:
                return info
        except (AssertionError, urllib.error.URLError) as error:
            last = str(error)
        time.sleep(1)
    raise AssertionError(f'Page did not become inspectable: {last}')

def valid(info, site):
    hosts = [urlparse(site['url']).hostname, *site.get('redirect_hosts',[])]
    if urlparse(info.get('url','')).hostname not in hosts:
        return 'Unexpected redirect or browser error'
    if urlparse(info.get('url','')).hostname == urlparse(site['url']).hostname and urlparse(info['url']).path.rstrip('/') != urlparse(site['url']).path.rstrip('/'):
        return 'Page navigated away from the requested URL'
    title = info.get('title','').lower()
    preview = info.get('preview','').lower()
    if any(phrase in title for phrase in ['just a moment','access denied','security check','too many requests','error','prove your humanity']):
        return 'Challenge or error page'
    if any(phrase in preview for phrase in ["you've been blocked",'verify you are human','unusual traffic from your computer']):
        return 'Challenge or network block'
    if info.get('ready') not in ('interactive','complete') or info.get('textLength',0)<80 or info.get('nodes',0)<20:
        return 'Page not ready or insufficient rendered content'
    return None

def public_metadata(info):
    # Validation uses a short body excerpt, but reports retain only DOM counts,
    # titles, and URL paths. Challenge pages can contain a network's IP address.
    result={key:value for key,value in info.items() if key!='preview'}
    result['url']=urlparse(info.get('url',''))._replace(query='',fragment='').geturl()
    return result

def sample(label, cef_pid, baseline_pid):
    values={'Aside':[],'CEF build':[]}; counts={key:[] for key in values}
    footprints={key:[] for key in values}
    for i in range(8):
        processes=process_table()
        for name,pid in [('Aside',baseline_pid),('CEF build',cef_pid)]:
            pids=tree(processes,f'pid:{pid}')
            assert pids,f'{name} exited'
            values[name].append(round(sum(processes[p][1] for p in pids)/1024,2))
            counts[name].append(len(pids))
            footprints[name].append(physical_footprint_mib(pids))
        if i<7:time.sleep(3)
    memory=api('/api/browser/memory')
    report={'phase':label,'cef_live_views':memory['live_renderers'],
            'browsers':{name:{'median_mib':round(statistics.median(v),2),'samples_mib':v,'process_counts':counts[name],
                              'footprint_samples_mib':footprints[name],
                              'footprint_median_mib':round(statistics.median([f for f in footprints[name] if f is not None]),2) if sum(f is not None for f in footprints[name])>=4 else None} for name,v in values.items()}}
    baseline=report['browsers']['Aside']['median_mib']; candidate=report['browsers']['CEF build']['median_mib']
    report['rss_reduction_percent']=round(100*(baseline-candidate)/baseline,1)
    baseline_footprint=report['browsers']['Aside']['footprint_median_mib'];candidate_footprint=report['browsers']['CEF build']['footprint_median_mib']
    if baseline_footprint and candidate_footprint:
        report['footprint_reduction_percent']=round(100*(baseline_footprint-candidate_footprint)/baseline_footprint,1)
    print(json.dumps(report),flush=True)
    return report

for _ in range(60):
    try:
        state=api('/api/browser/memory')
        if state.get('shell_ready'):break
    except urllib.error.URLError:pass
    time.sleep(.5)
else:raise RuntimeError('CEF did not become ready')
assert not any(t['live'] for t in state['tabs']),'Use a fresh, empty profile'
profile=Path(tempfile.mkdtemp(prefix='aside-real-sites-baseline-'))
log=(profile/'launch.log').open('w')
process=subprocess.Popen([args.aside,f'--user-data-dir={profile}','--remote-debugging-port=0','--no-first-run','--new-window',*[s['url'] for s in sites]],stdout=log,stderr=log)
try:
    for _ in range(80):
        port_file=profile/'DevToolsActivePort'
        if port_file.exists():break
        assert process.poll() is None,'Aside exited'
        time.sleep(.5)
    port=int(port_file.read_text().splitlines()[0])
    def baseline_json(path):
        with urllib.request.urlopen(f'http://127.0.0.1:{port}{path}',timeout=10) as response:return json.load(response)
    version=baseline_json('/json/version')
    for site in sites:
        api('/api/browser/command',{'cmd':'newTab','url':site['url']})
        site['cef_id']=api('/api/browser/tabs')['tabs'][-1]['id']
    print('Opened ten real websites in each browser; waiting for real network loads.',flush=True)
    time.sleep(20)
    accepted=[];excluded=[]
    for site in sites:
        hosts=[urlparse(site['url']).hostname,*site.get('redirect_hosts',[])]
        target=next((t for t in baseline_json('/json/list') if t.get('type')=='page' and urlparse(t.get('url','')).hostname in hosts),None)
        try:
            assert target,'No matching baseline page'
            cdp(version['webSocketDebuggerUrl'],'Target.activateTarget',{'targetId':target['id']})
            time.sleep(2)
            aside=cdp(target['webSocketDebuggerUrl'],'Runtime.evaluate',{'expression':EXPRESSION,'returnByValue':True})['result']['value']
            cef=inspect_cef(site['cef_id'])
            reasons={'Aside':valid(aside,site),'CEF build':valid(cef,site)}
            entry={**site,'validation':{'Aside':public_metadata(aside),'CEF build':public_metadata(cef)}}
            if any(reasons.values()):
                excluded.append({**entry,'reasons':reasons})
            else:
                accepted.append(entry)
                print('Verified:',site['name'],'|',cef['title'],'| nodes',cef['nodes'],flush=True)
                continue
        except Exception as error:
            excluded.append({**site,'reason':str(error)})
        print('Excluded from both:',json.dumps(excluded[-1]),flush=True)
        api('/api/browser/command',{'cmd':'close','id':site['cef_id']})
        if target:cdp(version['webSocketDebuggerUrl'],'Target.closeTarget',{'targetId':target['id']})
    args.output.with_suffix('.validation.json').write_text(json.dumps({'accepted':accepted,'excluded':excluded},indent=2)+'\n')
    assert len(accepted)>=6,f'Only {len(accepted)} matched working sites; comparison refused'
    # Renew normal agent protection after validation so every accepted page is live
    # for the first sample, even when a slow site or a discarded tab needed a retry.
    for site in accepted:
        inspect_cef(site['cef_id'])
    protected_at=time.monotonic()
    print(f'{len(accepted)} matched real sites verified. Sampling with live pages.',flush=True)
    print('Settling live website activity for 75 seconds before sampling.',flush=True)
    for elapsed in range(0,75,15):
        time.sleep(15)
        print('live warm-up',elapsed+15,'seconds',flush=True)
    live=sample('live pages: agent protection prevents eviction',state['pid'],process.pid)
    while time.monotonic()-protected_at<210:
        memory=api('/api/browser/memory')
        elapsed=round(time.monotonic()-protected_at)
        print('idle wait',elapsed,'seconds; CEF live views',memory['live_renderers'],flush=True)
        time.sleep(min(20,210-(time.monotonic()-protected_at)))
    idle=sample('idle pages: normal background eviction enabled',state['pid'],process.pid)
    report={'metric':'Median aggregate descendant-process RSS and sum of macOS per-process physical footprint in MiB. RSS may double-count shared pages. Footprint uses proc_pid_rusage v2; incomplete samples are null.',
            'workload':'Public live websites, fresh profiles, signed out, no extensions. Challenges/errors excluded from BOTH browsers.',
            'baseline_browser':version['Browser'],'cef_version':'156.0.8','accepted_sites':accepted,'excluded_sites':excluded,
            'phases':[live,idle],'scope':'Shared external OpenCode and other running applications are outside both sampled process trees.'}
    args.output.write_text(json.dumps(report,indent=2)+'\n')
    print('Saved:',args.output,flush=True)
finally:
    process.terminate()
    try:process.wait(timeout=10)
    except subprocess.TimeoutExpired:process.kill();process.wait(timeout=5)
    log.close()
