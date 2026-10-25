#!/usr/bin/env python3
"""Memory and load times of ad-heavy sites with built-in ad blocking on and off.

Each condition starts this build as a background test instance with a fresh data
directory, opens the sites one at a time in foreground tabs, and samples the process
tree once every page has loaded and sat live for --settle seconds. The site count
of the ad-heavy set stays within the live-tab budget, so no page is evicted; a longer
--sites list measures the browser as eviction leaves it. Memory includes the WebKit
processes behind the shell that appear during the run.
"""
import argparse
import json
import os
import shutil
import statistics
import subprocess
import time
import urllib.error
from pathlib import Path

import atrium_api
from bench_common import MIB, footprint_bytes, process_table, snapshot, stop_process

ROOT = Path(__file__).resolve().parent
SITES = [
    'https://www.bbc.com/news', 'https://stackoverflow.com/questions/11227809', 'https://edition.cnn.com',
    'https://weather.com', 'https://www.dailymail.co.uk/home/index.html', 'https://www.forbes.com',
]
TIMING = """(() => { const n = performance.getEntriesByType('navigation')[0], f = performance.getEntriesByName('first-contentful-paint')[0];
  return { ready: document.readyState, fcp: f ? Math.round(f.startTime) : null, load: n && n.loadEventEnd ? Math.round(n.loadEventEnd) : null,
    frames: (function count(w) { let c = 1; for (let i = 0; i < w.frames.length; i++) { try { c += count(w.frames[i]); } catch (e) { c += 1; } } return c; })(window),
    resources: performance.getEntriesByType('resource').length }; })()"""

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--app', type=Path, default=ROOT.parent / 'target/bundle/Atrium.app/Contents/MacOS/atrium')
parser.add_argument('--port', type=int, default=7893)
parser.add_argument('--runs', type=int, default=1)
parser.add_argument('--settle', type=int, default=30)
parser.add_argument('--output', type=Path)
parser.add_argument('--sites', type=Path, help='a site list like real_sites.json instead of the ad-heavy set')
parser.add_argument('--conditions', default='off,on')
args = parser.parse_args()
if args.sites:
    SITES = [site['url'] for site in json.loads(args.sites.read_text())]
CONDITIONS = [c == 'on' for c in args.conditions.split(',')]


def webkit_pids():
    return {pid for pid, (_, _, cmd) in process_table().items() if 'com.apple.WebKit' in cmd}


def measure(blocking, run):
    data = Path(f'/private/tmp/atrium-blocking-{"on" if blocking else "off"}-{run}')
    shutil.rmtree(data, ignore_errors=True)
    data.mkdir(parents=True)
    env = {**os.environ, 'ATRIUM_MOCK_KEYCHAIN': '1', 'ATRIUM_BACKGROUND_WINDOW': '1', 'ATRIUM_DEBUG_API': '1',
           'ATRIUM_CONTENT_BLOCKING': '1' if blocking else '0'}
    webkit_before = webkit_pids()
    log = (data.parent / f'{data.name}.log').open('w')
    process = subprocess.Popen([str(args.app), '--data-dir', str(data), 'run', '--port', str(args.port),
                                '--opencode-url', 'http://127.0.0.1:1'], env=env, stdout=log, stderr=log)
    client = atrium_api.Client(f'http://127.0.0.1:{args.port}', data_dir=data)
    try:
        for _ in range(240):
            try:
                if client.request('/api/browser/memory', timeout=5).get('shell_ready'):
                    break
            except (urllib.error.URLError, TimeoutError, ConnectionError):
                pass
            time.sleep(.25)
        pages = []
        for url in SITES:
            before = {t['id'] for t in client.api('/api/browser/tabs')['tabs']}
            client.command(cmd='newTab', url=url, background=False)
            tab, info, started = None, None, time.monotonic()
            while time.monotonic() - started < 30:
                tab = tab or next((t['id'] for t in client.api('/api/browser/tabs')['tabs'] if t['id'] not in before), None)
                if tab:
                    try:
                        info = client.api('/api/debug/cdp', {'tab': tab, 'method': 'Runtime.evaluate', 'params': {
                            'expression': TIMING, 'returnByValue': True}}, timeout=10)['result']['result'].get('value')
                    except Exception:
                        info = None
                    if info and info['ready'] == 'complete' and info['load'] is not None:
                        break
                time.sleep(.5)
            pages.append({'url': url, **(info or {})})
            print(' ', 'on ' if blocking else 'off', url, info, flush=True)
        time.sleep(args.settle)
        memory = client.api('/api/browser/memory')
        sample = snapshot(process.pid)
        webkit = sum(footprint_bytes(pid) or 0 for pid in webkit_pids() - webkit_before) / MIB
        sample['footprint_mib'] = round(sample['footprint_mib'] + webkit, 1) if sample['footprint_mib'] is not None else None
        return {'blocking': blocking, 'pages': pages, 'footprint_mib': sample['footprint_mib'], 'rss_mib': sample['rss_mib'],
                'processes': sample['process_count'], 'renderers': sample['by_type'].get('renderer', {}).get('count'),
                'live_tabs': memory.get('live_renderers'), 'content_blocking': memory.get('content_blocking')}
    finally:
        try:
            client.request('/api/browser/command', {'cmd': 'quit'}, timeout=5)
            process.wait(timeout=20)
        except Exception:
            stop_process(process)
        log.close()


results = []
for run in range(args.runs):
    for blocking in CONDITIONS if run % 2 == 0 else CONDITIONS[::-1]:
        r = measure(blocking, run)
        print('on ' if blocking else 'off', {k: r[k] for k in ('footprint_mib', 'rss_mib', 'processes', 'renderers', 'content_blocking')}, flush=True)
        results.append(r)
summary = {}
for blocking in CONDITIONS:
    rs = [r for r in results if r['blocking'] == blocking]
    fcp = {p['url']: p.get('fcp') for r in rs for p in r['pages']}
    summary['on' if blocking else 'off'] = {
        'footprint_mib': statistics.median(r['footprint_mib'] for r in rs if r['footprint_mib'] is not None),
        'processes': statistics.median(r['processes'] for r in rs),
        'renderers': statistics.median(r['renderers'] or 0 for r in rs),
        'load_ms_median': statistics.median(p['load'] for r in rs for p in r['pages'] if p.get('load') is not None),
        'fcp_ms_median': statistics.median(v for v in fcp.values() if v is not None),
        'frames': sum(p.get('frames') or 0 for r in rs for p in r['pages']) / len(rs)}
print(json.dumps(summary, indent=1))
if args.output:
    args.output.write_text(json.dumps({'date': time.strftime('%Y-%m-%d'), 'method': __doc__, 'summary': summary, 'runs': results}, indent=1))
